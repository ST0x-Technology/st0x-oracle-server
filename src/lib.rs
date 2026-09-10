pub mod alpaca;
pub mod config;
pub mod market_hours;
pub mod metrics;
pub mod oracle;
pub mod pricing_client;
pub mod registry;
pub mod reuse;
pub mod sign;

use alloy::primitives::{Address, B256};
use alloy::sol;
use alloy::sol_types::SolValue;
use axum::{
    body::Bytes,
    extract::{RawQuery, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use rain_math_float::Float;
use serde::{Deserialize, Serialize};
use sign::Signer;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

use crate::market_hours::MarketHoursCache;
use crate::metrics::MetricsHandle;
use crate::pricing_client::LiveClient;
use crate::registry::{PriceDirection, ResolvedPair, TokenRegistry};
use chrono::Utc;
use st0x_pricing_types::Quote;

sol! {
    struct IOV2 {
        address token;
        bytes32 vaultId;
    }

    struct EvaluableV4 {
        address interpreter;
        address store;
        bytes bytecode;
    }

    struct OrderV4 {
        address owner;
        EvaluableV4 evaluable;
        IOV2[] validInputs;
        IOV2[] validOutputs;
        bytes32 nonce;
    }
}

/// Upstream (`rain.orderbook/crates/quote/src/oracle.rs`) posts one of
/// two ABI-encoded shapes:
/// - single: `(OrderV4, uint256 inputIOIndex, uint256 outputIOIndex, address counterparty)`
/// - batch:  `(OrderV4, uint256, uint256, address)[]`
///
/// We decode either. The response is a JSON array whose length matches
/// the number of requests. By default (and always for the single form)
/// it is a bare `OracleResponse` array and any failing item fails the
/// whole request; with `?allowFailure=true` on the batch form it is a
/// `BatchItemResponse` array, one `ok`/`error` slot per item. See
/// `ContextQuery` for the flag and `finish` for where the two shapes
/// diverge.
type OracleRequestTuple = (
    OrderV4,
    alloy::primitives::U256,
    alloy::primitives::U256,
    Address,
);

pub struct AppState {
    signer: Signer,
    registry: TokenRegistry,
    /// Live WS subscription to st0x.pricing. Background-tasked, holds
    /// the latest `Quote` per symbol in an RwLock<HashMap>. Replaces
    /// the Alpaca polling cache (pre-RAI-360).
    pricing: LiveClient,
    /// Every symbol declared in config.toml. /status compares this
    /// against the pricing cache to surface the partial-serving set.
    configured_symbols: Vec<String>,
    /// Market-hours source from Alpaca's calendar, used ONLY to classify
    /// the current session for the v4/v5 session slots (tag +
    /// start/end bounds). `publish_time` comes from the pricing quote's
    /// own `source_ts_unix_ms`, not from this cache.
    market_hours: Arc<MarketHoursCache>,
    /// Prometheus exposition format renderer for `/metrics`.
    metrics: MetricsHandle,
    /// Cross-frame signature reuse for v5/v6/v7 (see `reuse`).
    reuse: reuse::ReuseCache,
}

impl AppState {
    pub fn new(
        signer: Signer,
        registry: TokenRegistry,
        pricing: LiveClient,
        configured_symbols: Vec<String>,
        market_hours: Arc<MarketHoursCache>,
        metrics: MetricsHandle,
    ) -> Self {
        Self {
            signer,
            registry,
            pricing,
            configured_symbols,
            market_hours,
            metrics,
            // Off until `with_signature_reuse` is called: `main.rs` passes
            // the configured margin, tests opt in explicitly so nothing
            // silently serves a previous frame.
            reuse: reuse::ReuseCache::new(0),
        }
    }

    /// Set how many seconds a previous v5/v6/v7 quote must still have before
    /// its expiry to be served again instead of signing an unchanged price
    /// under a new publish_time. Zero disables the reuse.
    pub fn with_signature_reuse(mut self, min_remaining_secs: u64) -> Self {
        self.reuse = reuse::ReuseCache::new(min_remaining_secs);
        self
    }

    pub fn signer_address(&self) -> Address {
        self.signer.address()
    }
}

pub fn create_app(state: AppState) -> Router {
    let shared_state = Arc::new(state);
    Router::new()
        .route("/", get(health))
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/context/v1", post(post_signed_context_v1))
        .route("/context/v4", post(post_signed_context_v4))
        .route("/context/v5", post(post_signed_context_v5))
        .route("/context/v6", post(post_signed_context_v6))
        .route("/context/v7", post(post_signed_context_v7))
        .layer(CorsLayer::permissive())
        .with_state(shared_state)
}

async fn health() -> &'static str {
    "ok"
}

async fn metrics(State(state): State<Arc<AppState>>) -> String {
    state.metrics.render()
}

#[derive(Serialize)]
struct StatusResponse {
    signer: String,
    configured_symbols: Vec<String>,
    missing_symbols: Vec<String>,
}

/// Operational status of the server. `/health` is for Fly liveness and
/// stays lenient ("ok" whenever the process is running). `/status` is
/// for ops/monitoring and reports the configured-vs-cached set so a
/// missing broker position is visible without parsing logs. Always
/// returns 200; consumers gate on the contents of `missing_symbols`.
async fn status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let missing = state.pricing.missing(&state.configured_symbols).await;
    // Side-effect: refresh coverage + freshness gauges every /status hit
    // so dashboards don't need a dedicated background tick. /status is
    // already on the obs scrape path, so this is free.
    ::metrics::gauge!("oracle_configured_symbols").set(state.configured_symbols.len() as f64);
    ::metrics::gauge!("oracle_missing_symbols").set(missing.len() as f64);
    if let Some(newest_ms) = state.pricing.newest_source_ts_ms().await {
        let age_secs = (Utc::now().timestamp_millis() - newest_ms) as f64 / 1000.0;
        ::metrics::gauge!("oracle_cache_freshness_seconds").set(age_secs);
    }
    Json(StatusResponse {
        signer: format!("{:?}", state.signer.address()),
        configured_symbols: state.configured_symbols.clone(),
        missing_symbols: missing,
    })
}

/// JSON error body. Today it is the body of every non-2xx `/context/v*`
/// response; the batch envelope (`allowFailure=true`) reuses the same
/// shape per failed item, so it is public and round-trippable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Stable machine-readable code: `bad_request`, `service_unavailable`
    /// or `internal_error`. Mirrors the HTTP status `AppError` maps to.
    pub error: String,
    /// Human-readable detail for logs and debugging.
    pub detail: String,
}

/// A decoded `/context/v*` body plus the wire shape it arrived in.
///
/// The shape matters beyond decoding: only the array form can be served
/// as a per-item envelope (`allowFailure=true`). A single tuple is never
/// enveloped, flag or not, so the caller that sent it keeps getting the
/// one-element array or the whole-request error it always got.
struct DecodedRequest {
    items: Vec<OracleRequestTuple>,
    /// `true` when the body decoded as `(OrderV4, uint256, uint256,
    /// address)[]` — including the empty and one-element arrays.
    is_batch: bool,
}

/// Decode the POST body as either a single tuple or a batch array.
/// Returns a `Vec` in either case so downstream logic is uniform, and
/// remembers which form it was.
///
/// We try the batch form first because the empty-batch case (`[]`) is
/// a valid input upstream — returning an empty response array preserves
/// the "response length matches request length" contract. A batch
/// containing one element will also decode correctly here. Only when
/// the batch decoder rejects the body do we fall back to the single
/// tuple form (which is what most current callers send).
fn decode_request_body(body: &[u8]) -> Result<DecodedRequest, AppError> {
    if let Ok(items) = <Vec<OracleRequestTuple>>::abi_decode(body) {
        return Ok(DecodedRequest {
            items,
            is_batch: true,
        });
    }
    let single = <OracleRequestTuple>::abi_decode(body)
        .map_err(|e| AppError::BadRequest(format!("Invalid ABI-encoded body: {}", e)))?;
    Ok(DecodedRequest {
        items: vec![single],
        is_batch: false,
    })
}

/// Per-request options carried in the query string of a `/context/v*`
/// POST. The body is opaque ABI, so the query string is the only place a
/// caller can put a flag without changing the upstream body encoding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextQuery {
    /// `?allowFailure=true` (or `=1`, case-insensitive). When set on a
    /// BATCH body the response is always `200` with one
    /// `BatchItemResponse` per request item, so a failing item no longer
    /// takes the whole batch down. Absent, `false`, or any other value
    /// keeps the all-or-nothing behaviour every existing caller relies
    /// on. Ignored for single-tuple bodies.
    pub allow_failure: bool,
}

impl ContextQuery {
    /// Lenient parse of a raw query string. Unknown keys are ignored,
    /// garbage never errors, a missing value reads as `false`, and if the
    /// key repeats the last occurrence wins. This is deliberately not a
    /// typed `Query<T>` extractor: a malformed query must not turn an
    /// otherwise valid oracle request into a 400.
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::default();
        };
        let allow_failure = raw
            .split('&')
            .filter_map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (key == "allowFailure").then_some(value)
            })
            .next_back()
            .map(|value| value.eq_ignore_ascii_case("true") || value == "1")
            .unwrap_or(false);
        Self { allow_failure }
    }
}

async fn post_signed_context_v1(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Result<ContextResponse, AppError> {
    let query = ContextQuery::parse(query.as_deref());
    let result = post_signed_context_v1_inner(state, query, body).await;
    record_request_outcome("v1", &result);
    result
}

async fn post_signed_context_v1_inner(
    state: Arc<AppState>,
    query: ContextQuery,
    body: Bytes,
) -> Result<ContextResponse, AppError> {
    let endpoint = "v1";
    let decoded = decode_request_body(&body)?;
    // Envelope mode is only reachable for the array form; a single tuple
    // keeps the whole-request error path whatever the flag says.
    let envelope = query.allow_failure && decoded.is_batch;
    let requests = decoded.items;

    if requests.is_empty() {
        return finish(endpoint, Vec::new(), envelope);
    }

    // Resolve every request's token pair first so we know which symbols
    // we need from the cache. This lets us take a single snapshot of
    // exactly those entries, so a poll loop update mid-iteration can't
    // mix quotes (or publish_time values) for the same symbol within
    // one HTTP response.
    //
    // Per-item: a failed resolution is kept in its slot rather than
    // aborting, so envelope mode can report it alongside the items that
    // did resolve. Strict mode surfaces the FIRST resolution failure
    // before touching the cache — exactly the order the all-or-nothing
    // path has always used.
    let resolved: Vec<Result<ResolvedPair, AppError>> = requests
        .iter()
        .map(|(order, input_io_index, output_io_index, _counterparty)| {
            resolve_pair_for_order(&state, order, *input_io_index, *output_io_index)
        })
        .collect();
    if !envelope && resolved.iter().any(Result::is_err) {
        // Move the error out rather than rebuild it: `AppError` is not
        // `Clone`, and rebuilding an `Internal` would lose its anyhow
        // chain and change the wire `detail`.
        let err = resolved
            .into_iter()
            .find_map(Result::err)
            .expect("checked above");
        return Err(strict_abort(endpoint, err));
    }

    let needed_symbols: Vec<&str> = resolved
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .map(|p| p.symbol.as_str())
        .collect();
    let snapshot = state.pricing.snapshot_many(&needed_symbols).await;

    let mut items: Vec<Result<oracle::OracleResponse, AppError>> =
        Vec::with_capacity(resolved.len());
    for pair in resolved {
        let item = match pair {
            Err(err) => Err(err),
            Ok(pair) => match snapshot.get(&pair.symbol) {
                None => Err(no_live_quote(&pair.symbol)),
                Some(quote) => build_response_from_quote(&state, &pair, quote).await,
            },
        };
        // Strict mode stops at the first failure, as before: nothing
        // after it is signed.
        if !envelope {
            if let Err(err) = item {
                return Err(strict_abort(endpoint, err));
            }
        }
        items.push(item);
    }

    finish(endpoint, items, envelope)
}

/// The transient "cache has nothing for this symbol yet" error, shared by
/// every schema so the wording (which ops grep for) stays identical.
fn no_live_quote(symbol: &str) -> AppError {
    AppError::Unavailable(format!(
        "No live quote for {symbol} yet. The pricing WS has not delivered a frame since startup."
    ))
}

/// What a `/context/v*` handler returns on `200`. The two arms are the
/// two wire shapes described on `oracle::BatchItemResponse`; which one a
/// request gets is decided by `finish`, never by the caller of `finish`.
pub enum ContextResponse {
    /// The historical shape: a bare `OracleResponse` array whose length
    /// equals the request length. Served for every single-tuple request
    /// and for batches without `allowFailure`.
    Strict(Vec<oracle::OracleResponse>),
    /// One `BatchItemResponse` per request item, in request order. Served
    /// only for batches with `allowFailure=true`.
    Envelope(Vec<oracle::BatchItemResponse>),
}

impl ContextResponse {
    /// Label for the `oracle_context_request_total{outcome}` counter.
    /// `empty` / `ok` keep their historical meaning; `partial` and
    /// `failed` are envelope-only, because only an envelope can carry a
    /// failed item inside a `200`.
    fn outcome(&self) -> &'static str {
        match self {
            Self::Strict(items) if items.is_empty() => "empty",
            Self::Strict(_) => "ok",
            Self::Envelope(items) if items.is_empty() => "empty",
            Self::Envelope(items) => {
                let failed = items
                    .iter()
                    .filter(|i| matches!(i, oracle::BatchItemResponse::Error(_)))
                    .count();
                if failed == 0 {
                    "ok"
                } else if failed == items.len() {
                    "failed"
                } else {
                    "partial"
                }
            }
        }
    }
}

impl IntoResponse for ContextResponse {
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::Strict(items) => Json(items).into_response(),
            Self::Envelope(items) => Json(items).into_response(),
        }
    }
}

/// Turn the per-item results of one request into its response. This is
/// the single place the strict/envelope decision is applied, shared by
/// every schema so they cannot drift.
///
/// - Envelope: always `Ok`, every slot mapped to a `BatchItemResponse`;
///   failed slots are logged here with their index (the HTTP status is
///   `200`, so this is the only trace they leave).
/// - Strict: the first `Err` becomes the whole-request error. Callers
///   already short-circuit before reaching here in strict mode, so the
///   `Err` arm is a fallback that keeps the function total.
fn finish(
    endpoint: &'static str,
    items: Vec<Result<oracle::OracleResponse, AppError>>,
    envelope: bool,
) -> Result<ContextResponse, AppError> {
    if !envelope {
        // Every item here reached the wire as a signed response (callers
        // abort strict mode before `finish` on the first failure, via
        // `strict_abort`, which counts that one error). The `Err` arm is
        // a fallback that keeps the function total.
        let mut responses = Vec::with_capacity(items.len());
        for item in items {
            match item {
                Ok(response) => responses.push(response),
                Err(err) => return Err(strict_abort(endpoint, err)),
            }
        }
        record_item_outcome(endpoint, "ok", responses.len());
        return Ok(ContextResponse::Strict(responses));
    }
    let items = items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let outcome = match &item {
                Ok(_) => "ok",
                Err(err) => {
                    err.log_batch_item(endpoint, index);
                    err.code()
                }
            };
            record_item_outcome(endpoint, outcome, 1);
            oracle::BatchItemResponse::from(item)
        })
        .collect();
    Ok(ContextResponse::Envelope(items))
}

/// Strict-mode abort: the one item whose failure becomes the whole
/// request's error. Counted on the item counter so the per-item view
/// stays complete across both modes, then handed back unchanged.
fn strict_abort(endpoint: &'static str, err: AppError) -> AppError {
    record_item_outcome(endpoint, err.code(), 1);
    err
}

/// Increment `oracle_context_item_total` — the per-item counterpart of
/// `oracle_context_request_total`. It counts item VERDICTS THAT REACHED
/// THE WIRE: in an `allowFailure` batch every slot (ok or its error
/// code); in strict mode either N `ok` for a fully signed batch, or the
/// single aborting error. Items resolved but never signed because an
/// earlier strict abort stopped the batch are not counted — they were
/// never served. Keep the labels stable — the obs dashboard joins on
/// these.
fn record_item_outcome(endpoint: &'static str, outcome: &'static str, count: usize) {
    if count == 0 {
        return;
    }
    ::metrics::counter!(
        "oracle_context_item_total",
        "endpoint" => endpoint,
        "outcome" => outcome,
    )
    .increment(count as u64);
}

/// Record a `/context/v{N}` request's outcome on the `oracle_context_request_total`
/// counter. `outcome` labels: `ok` (every item signed), `empty` (no
/// requests in the body — Raindex's quote crate posts an empty batch when
/// an order's IO list is empty), `error` (the whole request failed: a
/// single tuple, or a batch without `allowFailure`), and the two
/// envelope-only labels `partial` (some slots failed) and `failed` (every
/// slot failed) — see `ContextResponse::outcome`. Per-item detail lives on
/// `oracle_context_item_total`. Keep the labels stable — the obs dashboard
/// joins on these.
fn record_request_outcome(endpoint: &'static str, result: &Result<ContextResponse, AppError>) {
    let outcome = match result {
        Ok(response) => response.outcome(),
        Err(_) => "error",
    };
    ::metrics::counter!(
        "oracle_context_request_total",
        "endpoint" => endpoint,
        "outcome" => outcome,
    )
    .increment(1);
}

/// v4 handler — `/context/v4` endpoint. Same request shape and
/// snapshot-once batching as v1, plus the caller's raw
/// `validInputs[input_io_index].token` /
/// `validOutputs[output_io_index].token` addresses are stamped into
/// signed-context slots 6 and 7 respectively.
///
/// The security property: a v4 strategy that asserts
/// `equal-to(signed-context<0 6> input-token()) &&
/// equal-to(signed-context<0 7> output-token())` can no longer be
/// tricked into applying a signed price for pair `(A,B)` against an
/// order whose IO pair is `(C,D)`. See `oracle::SCHEMA_VERSION_V4`
/// for the full context layout.
async fn post_signed_context_v4(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Result<ContextResponse, AppError> {
    let query = ContextQuery::parse(query.as_deref());
    let result = post_signed_context_pair_bound(state, query, body, PairSchema::V4).await;
    record_request_outcome("v4", &result);
    result
}

/// v5 handler — `/context/v5` endpoint. Identical request shape,
/// resolution and batching to v4; the response additionally signs the
/// pricing model's own expiry at slot 8.
///
/// The property v5 adds: a strategy no longer has to guess how long a
/// signed price is good for. `max-staleness` is a constant baked in when
/// the strategy was written, while the producer's binding horizon moves
/// with the asset, the session and the calibrated model. v5 lets the
/// strategy assert against the producer's own answer. See
/// `oracle::SCHEMA_VERSION_V5` for the full layout.
async fn post_signed_context_v5(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Result<ContextResponse, AppError> {
    let query = ContextQuery::parse(query.as_deref());
    let result = post_signed_context_pair_bound(state, query, body, PairSchema::V5).await;
    record_request_outcome("v5", &result);
    result
}

/// v6 handler — `/context/v6` endpoint. Identical request shape,
/// resolution and batching to v4/v5; the response additionally signs
/// the vault NAV ratio the pricing model priced this quote against, as
/// a Rain Float at slot 9.
///
/// The property v6 adds: the base token is a share in a wrapped-token
/// vault whose NAV can step (e.g. on a dividend deposit), and a price
/// signed against one NAV but settled against another is stale in a
/// way no timestamp check can see. Slot 9 carries the on-chain
/// `convertToAssets(1e18)` value the model priced against, losslessly
/// packed as a Rain Float, so a v6 strategy can assert numeric equality
/// against the `erc4626-convert-to-assets` word at settlement. See
/// `oracle::SCHEMA_VERSION_V6` for the full layout, the encoding
/// rationale and the zero sentinel.
async fn post_signed_context_v6(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Result<ContextResponse, AppError> {
    let query = ContextQuery::parse(query.as_deref());
    let result = post_signed_context_pair_bound(state, query, body, PairSchema::V6).await;
    record_request_outcome("v6", &result);
    result
}

/// v7 handler — `/context/v7` endpoint. Identical request shape,
/// resolution and batching to v4/v5/v6; the signed price at slot 1 is the
/// vault's UNDERLYING asset rate rather than the vault-share rate, and no
/// NAV ratio is signed (the v5 nine-slot shape, no slot 9).
///
/// The property v7 changes: v6 signs the NAV ratio and forces a strategy
/// to assert exact equality against the vault's live answer at settlement
/// — a DoS surface (audit H03), because the NAV can step between sign and
/// settle for reasons outside any attacker's control and every step bricks
/// otherwise-valid frames. v7 signs only the underlying price and leaves
/// the ratio UNSIGNED; the consuming strategy (RAI-2199) reads the vault's
/// live `erc4626-convert-to-assets` on-chain and DERIVES the vault price
/// (`underlying × convertToAssets(1 share)`) atomically — nothing to
/// straddle. The underlying is the one quantity the chain can re-derive
/// the vault price from, so it is what must be signed. See
/// `oracle::SCHEMA_VERSION_V7` for the full layout and the fail-closed
/// handling of an absent underlying rate.
async fn post_signed_context_v7(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Result<ContextResponse, AppError> {
    let query = ContextQuery::parse(query.as_deref());
    let result = post_signed_context_pair_bound(state, query, body, PairSchema::V7).await;
    record_request_outcome("v7", &result);
    result
}

/// Which pair-bound schema a `/context/v4`, `/context/v5`, `/context/v6`
/// or `/context/v7` request is being served under. All four share request
/// decoding, registry resolution, snapshot-once batching and the session
/// snapshot; they differ in whether the model's expiry is signed into slot
/// 8 (v5, v6, v7), whether the vault NAV ratio is signed into slot 9 (v6
/// only), and WHICH price is signed into slot 1 — the vault-share rate for
/// v4/v5/v6, the vault's UNDERLYING asset rate for v7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairSchema {
    V4,
    V5,
    V6,
    V7,
}

impl PairSchema {
    fn tag(self) -> &'static str {
        match self {
            Self::V4 => "v4",
            Self::V5 => "v5",
            Self::V6 => "v6",
            Self::V7 => "v7",
        }
    }

    /// v7 signs the vault's UNDERLYING asset rate at slot 1; every other
    /// pair-bound schema signs the vault-share rate. This selects which of
    /// the cached quote's directional rate pairs the response is built
    /// from.
    fn signs_underlying(self) -> bool {
        matches!(self, Self::V7)
    }

    /// v5, v6 and v7 sign the model's expiry at slot 8; v4 signs only
    /// publish_time. Only the expiry-bearing schemas take part in
    /// cross-frame signature reuse (see `reuse`).
    fn signs_expiry(self) -> bool {
        !matches!(self, Self::V4)
    }
}

/// Shared body for `/context/v4`, `/context/v5`, `/context/v6` and
/// `/context/v7`.
async fn post_signed_context_pair_bound(
    state: Arc<AppState>,
    query: ContextQuery,
    body: Bytes,
    schema: PairSchema,
) -> Result<ContextResponse, AppError> {
    let endpoint = schema.tag();
    let decoded = decode_request_body(&body)?;
    // Envelope mode is only reachable for the array form; a single tuple
    // keeps the whole-request error path whatever the flag says.
    let envelope = query.allow_failure && decoded.is_batch;
    let requests = decoded.items;

    if requests.is_empty() {
        return finish(endpoint, Vec::new(), envelope);
    }

    // Same resolution + batching shape as v1, but also keep the raw
    // input_token/output_token per request so we can bind them into the
    // signed context — that binding is the whole point of v4.
    //
    // Per-item, same as v1: a failed resolution stays in its slot for
    // envelope mode; strict mode surfaces the FIRST one before touching
    // the cache, preserving the historical resolve-before-build order.
    let resolved: Vec<Result<(Address, Address, ResolvedPair), AppError>> = requests
        .iter()
        .map(|(order, input_io_index, output_io_index, _counterparty)| {
            let (input_token, output_token) =
                io_tokens_for(order, *input_io_index, *output_io_index)?;
            let pair = state
                .registry
                .resolve(input_token, output_token)
                .map_err(|e| AppError::BadRequest(e.to_string()))?;
            tracing::info!(
                symbol = %pair.symbol,
                direction = pair.direction.as_str(),
                input = %input_token,
                output = %output_token,
                schema = endpoint,
                "Oracle request"
            );
            Ok((input_token, output_token, pair))
        })
        .collect();
    if !envelope && resolved.iter().any(Result::is_err) {
        // Move, don't rebuild: see the v1 path for why.
        let err = resolved
            .into_iter()
            .find_map(Result::err)
            .expect("checked above");
        return Err(strict_abort(endpoint, err));
    }

    let needed_symbols: Vec<&str> = resolved
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .map(|(_, _, p)| p.symbol.as_str())
        .collect();
    let snapshot = state.pricing.snapshot_many(&needed_symbols).await;

    // Session classification is snapshot once per batch; publish_time is
    // per-quote (the pricing quote's own source_ts), read inside the builder.
    let session_info = state.market_hours.session_info_for(Utc::now()).await;

    let mut items: Vec<Result<oracle::OracleResponse, AppError>> =
        Vec::with_capacity(resolved.len());
    for slot in resolved {
        let item = match slot {
            Err(err) => Err(err),
            Ok((input_token, output_token, pair)) => match snapshot.get(&pair.symbol) {
                None => Err(no_live_quote(&pair.symbol)),
                Some(quote) => {
                    build_response_from_quote_pair_bound(
                        &state,
                        &pair,
                        quote,
                        input_token,
                        output_token,
                        &session_info,
                        schema,
                    )
                    .await
                }
            },
        };
        // Strict mode stops at the first failure, as before: nothing
        // after it is signed.
        if !envelope {
            if let Err(err) = item {
                return Err(strict_abort(endpoint, err));
            }
        }
        items.push(item);
    }

    finish(endpoint, items, envelope)
}

/// Extract the raw `(input_token, output_token)` addresses that the
/// caller nominated in this request's `OrderV4`. Same bounds checks
/// as `resolve_pair_for_order`, minus the registry lookup — the two
/// helpers pull from the same source but v4 keeps the addresses even
/// after they've been resolved to a symbol.
fn io_tokens_for(
    order: &OrderV4,
    input_io_index: alloy::primitives::U256,
    output_io_index: alloy::primitives::U256,
) -> Result<(Address, Address), AppError> {
    let input_idx: usize = input_io_index.try_into().unwrap_or(usize::MAX);
    let output_idx: usize = output_io_index.try_into().unwrap_or(usize::MAX);

    let input_token = order
        .validInputs
        .get(input_idx)
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "Invalid input IO index: {} (order has {} inputs)",
                input_idx,
                order.validInputs.len()
            ))
        })?
        .token;

    let output_token = order
        .validOutputs
        .get(output_idx)
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "Invalid output IO index: {} (order has {} outputs)",
                output_idx,
                order.validOutputs.len()
            ))
        })?
        .token;

    Ok((input_token, output_token))
}

/// Decode a request's IO indices into the actual input/output addresses
/// and look them up in the token registry. Pure: never touches the cache.
fn resolve_pair_for_order(
    state: &AppState,
    order: &OrderV4,
    input_io_index: alloy::primitives::U256,
    output_io_index: alloy::primitives::U256,
) -> Result<ResolvedPair, AppError> {
    let input_idx: usize = input_io_index.try_into().unwrap_or(usize::MAX);
    let output_idx: usize = output_io_index.try_into().unwrap_or(usize::MAX);

    let input_token = order
        .validInputs
        .get(input_idx)
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "Invalid input IO index: {} (order has {} inputs)",
                input_idx,
                order.validInputs.len()
            ))
        })?
        .token;

    let output_token = order
        .validOutputs
        .get(output_idx)
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "Invalid output IO index: {} (order has {} outputs)",
                output_idx,
                order.validOutputs.len()
            ))
        })?
        .token;

    let pair: ResolvedPair = state
        .registry
        .resolve(input_token, output_token)
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    tracing::info!(
        symbol = %pair.symbol,
        direction = pair.direction.as_str(),
        input = %input_token,
        output = %output_token,
        "Oracle request"
    );

    Ok(pair)
}

/// Pick the maker-side rate for this request's swap direction from a
/// live pricing-service `Quote`.
///
/// Per the pricing-types contract, each rate is DIRECTIONAL: it is "the
/// price the model would honour for an input of the named token going
/// to an output of the other". A swap where the taker puts `quote` in
/// and takes `base` out is priced by `rate_quote_to_base`; base-in /
/// quote-out is priced by `rate_base_to_quote`. Each carries that
/// direction's own spread — consumers must never use the opposite
/// rate to dodge a unit conversion.
///
/// A Raindex order's `ratio` is `input_amount / output_amount` (units
/// of inputToken per outputToken). An order with input=quote /
/// output=base is the venue where takers swap quote→base, so its price
/// is `rate_quote_to_base` — but in `quote per base` units, i.e.
/// INVERTED. Same for the other direction. That `1/x` is exactly the
/// "unit conversion at a protocol-adapter boundary" the pricing-types
/// doc blesses; it does not touch the spread decision.
///
/// History, because this exact function shipped wrong: the original
/// implementation picked the UNIT-compatible rate instead of the
/// DIRECTION-compatible one (`QuoteToBase → rate_base_to_quote`, no
/// inversion). Units lined up, so everything parsed and every parity
/// check passed — but each rate carries the OTHER direction's spread,
/// so every sell order quoted the bid and every buy order the ask.
/// The first deployed 0trade order pair read as crossed by exactly
/// 2x the session spread (2026-08-07, caught in pre-migration review;
/// zero funded orders ever traded on it). The regression tests pin
/// maker orientation: a sell-side request must price ABOVE the
/// buy-side request for the same pair.
fn pick_rate_bytes(quote: &Quote, direction: PriceDirection) -> Result<[u8; 32], anyhow::Error> {
    let (directional_rate, name) = match direction {
        // Order input=quote, output=base: takers swap quote->base; the
        // honoured rate is base-per-quote, inverted into ratio units.
        PriceDirection::QuoteToBase => (quote.rate_quote_to_base.0, "rate_quote_to_base"),
        // Order input=base, output=quote: takers swap base->quote.
        PriceDirection::BaseToQuote => (quote.rate_base_to_quote.0, "rate_base_to_quote"),
    };
    let inverted = Float::from_raw(B256::new(directional_rate))
        .inv()
        .map_err(|e| anyhow::anyhow!("Failed to invert {name} into ratio units: {e:?}"))?;
    Ok(B256::from(inverted).0)
}

/// Pick the maker-side UNDERLYING rate for this request's swap direction
/// from a live pricing-service `Quote` — the v7 analogue of
/// `pick_rate_bytes`.
///
/// The underlying rate prices the vault's ERC4626 underlying asset (the
/// offchain stock) rather than the vault share. It is DIRECTIONAL and
/// spread-carrying exactly like the vault rate, and is picked by the same
/// input/output orientation and inverted into Raindex ratio units the same
/// way — so slot 1 of a v7 context is the underlying-price counterpart of
/// slot 1 of a v5/v6 context. The consuming strategy multiplies this by
/// the vault's LIVE NAV ratio on-chain to derive the vault price, so the
/// oracle never signs the ratio.
///
/// Fail-closed on the all-zero sentinel: a pricing producer predating the
/// `underlying_rate_*` fields decodes them to the zero Float via
/// `#[serde(default)]`, and a real stock rate is never zero. Signing (or
/// inverting) a zero underlying price would hand the strategy a garbage
/// mark, so the request is refused. This mirrors how the server treats any
/// other unusable rate — an internal error, fail-closed — rather than the
/// v6 NAV-ratio path, where zero is a legitimate "no assertion" sentinel
/// the strategy is free to accept; a zero underlying price is never usable.
fn pick_underlying_rate_bytes(
    quote: &Quote,
    direction: PriceDirection,
) -> Result<[u8; 32], anyhow::Error> {
    let (directional_rate, name) = match direction {
        PriceDirection::QuoteToBase => (
            quote.underlying_rate_quote_to_base.0,
            "underlying_rate_quote_to_base",
        ),
        PriceDirection::BaseToQuote => (
            quote.underlying_rate_base_to_quote.0,
            "underlying_rate_base_to_quote",
        ),
    };
    let rate = Float::from_raw(B256::new(directional_rate));
    if rate
        .is_zero()
        .map_err(|e| anyhow::anyhow!("Failed to test {name} for zero: {e:?}"))?
    {
        return Err(anyhow::anyhow!(
            "{name} is the zero sentinel: the pricing producer carried no underlying rate for \
             this quote (it predates underlying pricing, or the pair is misconfigured); refusing \
             to sign a zero underlying price"
        ));
    }
    let inverted = rate
        .inv()
        .map_err(|e| anyhow::anyhow!("Failed to invert {name} into ratio units: {e:?}"))?;
    Ok(B256::from(inverted).0)
}

/// Build a signed response from a pre-resolved pair and a snapshotted
/// `Quote`. All `Quote`s for one batch must come from a single
/// `LiveClient::snapshot_many` so a concurrent WS push can't mix prices
/// across elements of the same response.
///
/// The pricing service publishes both swap directions independently,
/// already including its spread; the oracle just picks the rate that
/// matches the request's direction and signs the 32-byte Rain Float
/// with the single Rain-Float inversion from `pick_rate_bytes` — no
/// f64 round-trip, no extra spread.
///
/// `publish_time` is the pricing quote's own `source_ts_unix_ms` — the
/// honest as-of instant st0x.pricing already stamped on the mark (the
/// fetch time inside a session, the last `session_close` out-of-session;
/// RAI-732). The oracle signs that straight through rather than
/// re-deriving a timestamp from its own clock: pricing owns the
/// market-hours truth, and trusting its `source_ts` means a stalled or
/// frozen pricing feed surfaces directly — `source_ts` stops advancing,
/// the signed timestamp goes stale, and the strategy's `max-staleness`
/// rejects. The oracle's own `MarketHoursCache` is used only for the
/// v4/v5 session slots, never for `publish_time`.
///
/// One deliberate exception: while a v5/v6/v7 price is unchanged, the
/// `reuse` layer serves the previous frame's signature, whose
/// `publish_time` can trail the live frame by up to the expiry horizon
/// minus the reuse margin (~20s today). That lag is bounded and visible
/// in the signed bytes; a stalled feed still shows as `source_ts`
/// stopping altogether.
/// Derive the signed `publish_time` (Unix seconds) from a pricing-service
/// `Quote.source_ts_unix_ms` (Unix milliseconds). st0x.pricing already
/// stamps `source_ts` with the mark's honest as-of instant (RAI-732), so
/// the oracle just converts ms → s and signs it.
fn publish_time_from_quote(quote: &Quote) -> Result<u64, AppError> {
    u64::try_from(quote.source_ts_unix_ms / 1000)
        .map_err(|_| AppError::Internal(anyhow::anyhow!("source_ts out of range")))
}

/// The model's binding horizon for this quote, in whole Unix seconds.
///
/// Integer division floors for the non-negative values that survive the
/// conversion (for negatives it truncates toward zero, but `try_from`
/// rejects those first) — the safe direction: the signed expiry can
/// only ever land at or before the model's real one, never
/// past it. A negative or out-of-range value fails the request rather
/// than clamping — a defaulted expiry is a defaulted licence to keep
/// trading on a price the model has disowned, and the whole point of v5
/// is that this number is trustworthy.
fn expiry_from_quote(quote: &Quote) -> Result<u64, AppError> {
    u64::try_from(quote.expiry_unix_ms / 1000)
        .map_err(|_| AppError::Internal(anyhow::anyhow!("quote expiry out of range")))
}

async fn build_response_from_quote(
    state: &AppState,
    pair: &ResolvedPair,
    quote: &Quote,
) -> Result<oracle::OracleResponse, AppError> {
    let publish_time = publish_time_from_quote(quote)?;

    let price_bytes = pick_rate_bytes(quote, pair.direction).map_err(AppError::Internal)?;

    tracing::info!(
        symbol = %pair.symbol,
        direction = pair.direction.as_str(),
        publish_time = publish_time,
        source_ts_unix_ms = quote.source_ts_unix_ms,
        "Building signed context from live pricing quote"
    );

    let context = oracle::build_context(price_bytes, publish_time)?;
    let (signature, signer) = state.signer.sign_context(&context).await?;

    Ok(oracle::OracleResponse {
        signer,
        context,
        signature,
    })
}

/// Pair-bound response builder (v4/v5/v6/v7). Same publish_time logic as
/// v1's `build_response_from_quote`, plus the session slots and the
/// caller's raw input/output token addresses stamped into signed-context
/// slots 6 and 7; v5/v6/v7 add the quote expiry at slot 8 and v6 the vault
/// NAV ratio at slot 9. Slot 1 is the vault-share rate for v4/v5/v6 and the
/// vault's underlying rate for v7 (see the `oracle::build_context_v*`
/// builders for the layouts).
#[allow(clippy::too_many_arguments)]
async fn build_response_from_quote_pair_bound(
    state: &AppState,
    pair: &ResolvedPair,
    quote: &Quote,
    input_token: Address,
    output_token: Address,
    session_info: &crate::market_hours::SessionInfo,
    schema: PairSchema,
) -> Result<oracle::OracleResponse, AppError> {
    // publish_time is the pricing quote's source_ts (see
    // `build_response_from_quote`); session slots come from the oracle's
    // own market-hours classification.
    let publish_time = publish_time_from_quote(quote)?;
    let session_start: u64 = session_info
        .start
        .timestamp()
        .try_into()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("session_start out of range")))?;
    let session_end: u64 = session_info
        .end
        .timestamp()
        .try_into()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("session_end out of range")))?;

    // Pick the directional rate and invert it into Raindex ratio units
    // (Rain-Float precision). v4/v5/v6 sign the vault-share rate
    // (`pick_rate_bytes`); v7 signs the vault's UNDERLYING asset rate
    // (`pick_underlying_rate_bytes`), which fails closed on the absent-rate
    // sentinel. Both come off the SAME cached quote.
    let price_bytes = if schema.signs_underlying() {
        pick_underlying_rate_bytes(quote, pair.direction)
    } else {
        pick_rate_bytes(quote, pair.direction)
    }
    .map_err(AppError::Internal)?;

    // Build the context first: it is cheap (no KMS), and the reuse layer
    // compares the built slots rather than a hand-kept list of them.
    let session_bytes = session_info.session.to_bytes32_v3();
    let expiry = if schema.signs_expiry() {
        Some(expiry_from_quote(quote)?)
    } else {
        None
    };
    let context = match (schema, expiry) {
        (PairSchema::V4, _) => oracle::build_context_v4(
            price_bytes,
            publish_time,
            session_bytes,
            session_start,
            session_end,
            input_token,
            output_token,
        )?,
        (PairSchema::V5, Some(expiry)) => oracle::build_context_v5(
            price_bytes,
            publish_time,
            session_bytes,
            session_start,
            session_end,
            input_token,
            output_token,
            expiry,
        )?,
        // The NAV ratio is read off the SAME `quote` as the rate at
        // slot 1 — both came out of one `snapshot_many` entry, and the
        // pricing client only ever stores whole frames — so the signed
        // context can never pair a rate from one frame with a ratio
        // from another.
        (PairSchema::V6, Some(expiry)) => oracle::build_context_v6(
            price_bytes,
            publish_time,
            session_bytes,
            session_start,
            session_end,
            input_token,
            output_token,
            expiry,
            quote.nav_ratio.0,
        )?,
        // v7 signs the UNDERLYING price at slot 1 (already selected into
        // `price_bytes` above) and NO NAV ratio — the strategy derives the
        // vault price on-chain from the live ratio (RAI-2198).
        (PairSchema::V7, Some(expiry)) => oracle::build_context_v7(
            price_bytes,
            publish_time,
            session_bytes,
            session_start,
            session_end,
            input_token,
            output_token,
            expiry,
        )?,
        (_, None) => {
            return Err(AppError::Internal(anyhow::anyhow!(
                "{} signs an expiry but none was derived from the quote",
                schema.tag()
            )))
        }
    };

    // v5/v6/v7: if the previous signature for this pair states the same
    // price under the same session for the same tokens, does not outlive
    // this frame's expiry, and is still good for the configured margin,
    // serve it instead of signing an unchanged price under a new
    // publish_time. See the `reuse` module.
    let reuse = expiry.map(|expiry| {
        (
            reuse::ReuseKey {
                schema: schema.tag(),
                symbol: pair.symbol.clone(),
                direction: pair.direction.as_str(),
                input_token,
                output_token,
            },
            expiry,
        )
    });
    if let Some((key, expiry)) = &reuse {
        let now_secs = u64::try_from(Utc::now().timestamp())
            .map_err(|_| AppError::Internal(anyhow::anyhow!("system clock before 1970")))?;
        if let Some(previous) = state.reuse.lookup(key, &context, *expiry, now_secs) {
            ::metrics::counter!("oracle_signature_reuse_total").increment(1);
            tracing::debug!(
                symbol = %pair.symbol,
                direction = pair.direction.as_str(),
                schema = schema.tag(),
                source_ts_unix_ms = quote.source_ts_unix_ms,
                "Price unchanged and previous quote still valid; reusing its signature"
            );
            return Ok(previous);
        }
    }

    tracing::info!(
        symbol = %pair.symbol,
        direction = pair.direction.as_str(),
        schema = schema.tag(),
        input = %input_token,
        output = %output_token,
        publish_time = publish_time,
        session = session_info.session.as_str(),
        session_start = session_start,
        session_end = session_end,
        source_ts_unix_ms = quote.source_ts_unix_ms,
        expiry_unix_ms = quote.expiry_unix_ms,
        "Building pair-bound signed context from live pricing quote"
    );

    let (signature, signer) = state.signer.sign_context(&context).await?;

    let response = oracle::OracleResponse {
        signer,
        context,
        signature,
    };
    if let Some((key, expiry)) = reuse {
        state.reuse.store(key, expiry, response.clone());
    }
    Ok(response)
}

#[derive(Debug)]
pub enum AppError {
    Internal(anyhow::Error),
    BadRequest(String),
    /// The server is alive but the poll loop hasn't produced a quote yet
    /// for this symbol. Distinct from BadRequest because it's transient
    /// and retrying may succeed.
    Unavailable(String),
}

impl AppError {
    /// HTTP status this error maps to when it is the outcome of a whole
    /// request (single-tuple requests, and batches without
    /// `allowFailure`).
    pub fn status_code(&self) -> StatusCode {
        match self {
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// Stable machine-readable code. This is the `error` field of the
    /// JSON body AND the `outcome` label of `oracle_context_item_total`,
    /// so a dashboard and a client see the same vocabulary.
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Internal(_) => "internal_error",
            AppError::BadRequest(_) => "bad_request",
            AppError::Unavailable(_) => "service_unavailable",
        }
    }

    /// The JSON body for this error. Used both as the body of a non-2xx
    /// response and as the per-item `body` of a batch envelope error, so
    /// the two paths can never disagree on the `error` code strings.
    pub fn to_error_response(&self) -> ErrorResponse {
        let detail = match self {
            AppError::Internal(err) => format!("{}", err),
            AppError::BadRequest(detail) | AppError::Unavailable(detail) => detail.clone(),
        };
        ErrorResponse {
            error: self.code().to_string(),
            detail,
        }
    }

    /// Log this error at the severity the whole-request path has always
    /// used: `error!` for internal failures (with the full anyhow chain),
    /// `warn!` for client and transient errors.
    pub fn log(&self) {
        match self {
            AppError::Internal(err) => tracing::error!("Internal error: {:?}", err),
            AppError::BadRequest(detail) => tracing::warn!("Bad request: {}", detail),
            AppError::Unavailable(detail) => tracing::warn!("Service unavailable: {}", detail),
        }
    }

    /// Same severities as `log`, for a failed slot inside a `200`
    /// envelope. Carries the endpoint and the item's position so an
    /// operator can tie the line to one request in a batch that
    /// otherwise left no non-2xx trace.
    pub fn log_batch_item(&self, endpoint: &'static str, index: usize) {
        match self {
            AppError::Internal(err) => tracing::error!(
                endpoint,
                index,
                "Batch item failed (internal error): {:?}",
                err
            ),
            AppError::BadRequest(detail) => {
                tracing::warn!(
                    endpoint,
                    index,
                    "Batch item failed (bad request): {}",
                    detail
                )
            }
            AppError::Unavailable(detail) => tracing::warn!(
                endpoint,
                index,
                "Batch item failed (service unavailable): {}",
                detail
            ),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        self.log();
        (self.status_code(), Json(self.to_error_response())).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err)
    }
}

#[cfg(test)]
mod app_error_tests {
    use super::*;

    /// Pins the status/code mapping that both the whole-request path and
    /// the per-item batch envelope depend on. A drift here would change
    /// what downstream clients key on.
    #[test]
    fn app_error_maps_to_stable_status_and_code() {
        let cases = [
            (
                AppError::Internal(anyhow::anyhow!("boom")),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "boom",
            ),
            (
                AppError::BadRequest("bad".into()),
                StatusCode::BAD_REQUEST,
                "bad_request",
                "bad",
            ),
            (
                AppError::Unavailable("later".into()),
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "later",
            ),
        ];
        for (err, status, code, detail) in cases {
            assert_eq!(err.status_code(), status);
            assert_eq!(
                err.to_error_response(),
                ErrorResponse {
                    error: code.to_string(),
                    detail: detail.to_string(),
                }
            );
        }
    }

    #[test]
    fn error_response_round_trips_through_json() {
        let original = ErrorResponse {
            error: "bad_request".into(),
            detail: "Invalid input IO index".into(),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(
            json,
            r#"{"error":"bad_request","detail":"Invalid input IO index"}"#
        );
        let back: ErrorResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }
}

#[cfg(test)]
mod context_response_tests {
    use super::*;
    use alloy::primitives::address;

    fn ok() -> oracle::OracleResponse {
        oracle::OracleResponse {
            signer: address!("1111111111111111111111111111111111111111"),
            context: vec![],
            signature: alloy::primitives::Bytes::new(),
        }
    }

    fn err() -> AppError {
        AppError::Unavailable("x".into())
    }

    /// The counter labels the dashboard joins on. `partial` and `failed`
    /// exist only for envelopes; strict can never carry a failed item.
    #[test]
    fn outcome_labels() {
        assert_eq!(ContextResponse::Strict(vec![]).outcome(), "empty");
        assert_eq!(ContextResponse::Strict(vec![ok()]).outcome(), "ok");
        assert_eq!(ContextResponse::Envelope(vec![]).outcome(), "empty");
        assert_eq!(finish("t", vec![Ok(ok())], true).unwrap().outcome(), "ok");
        assert_eq!(
            finish("t", vec![Ok(ok()), Err(err())], true)
                .unwrap()
                .outcome(),
            "partial"
        );
        assert_eq!(
            finish("t", vec![Err(err()), Err(err())], true)
                .unwrap()
                .outcome(),
            "failed"
        );
    }

    /// Both arms render as `200 application/json`; the strict arm is the
    /// historical bare array, the envelope arm the tagged items. This is
    /// the seam every route goes through, so pin it once here.
    #[tokio::test]
    async fn into_response_renders_both_shapes_as_200_json() {
        async fn render(resp: ContextResponse) -> (StatusCode, String, serde_json::Value) {
            let http = resp.into_response();
            let status = http.status();
            let content_type = http
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let bytes = axum::body::to_bytes(http.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                status,
                content_type,
                serde_json::from_slice(&bytes).unwrap(),
            )
        }

        let (status, ct, json) = render(ContextResponse::Strict(vec![ok()])).await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.starts_with("application/json"), "{ct}");
        assert_eq!(
            json,
            serde_json::json!([serde_json::to_value(ok()).unwrap()])
        );

        let (status, ct, json) =
            render(finish("t", vec![Ok(ok()), Err(err())], true).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.starts_with("application/json"), "{ct}");
        assert_eq!(
            json,
            serde_json::json!([
                { "status": "ok", "body": serde_json::to_value(ok()).unwrap() },
                { "status": "error", "body": { "error": "service_unavailable", "detail": "x" } },
            ])
        );

        // Both empty arms are the same `[]` on the wire — the upstream
        // client's length check passes for an empty batch either way.
        let (status, _, json) = render(ContextResponse::Envelope(vec![])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json, serde_json::json!([]));
        let (status, _, json) = render(ContextResponse::Strict(vec![])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json, serde_json::json!([]));
    }

    /// Exact per-item counting for `oracle_context_item_total`, under a
    /// thread-local recorder so the process-global Prometheus recorder
    /// (and tests running in parallel against it) cannot interfere.
    /// Pins the "verdicts that reached the wire" rule in both modes.
    #[test]
    fn item_counter_counts_wire_verdicts_exactly() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        use std::collections::HashMap;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        ::metrics::with_local_recorder(&recorder, || {
            // Envelope, mixed: every slot counted under its own code.
            let _ = finish(
                "t",
                vec![
                    Ok(ok()),
                    Err(err()),
                    Err(AppError::BadRequest("b".into())),
                    Ok(ok()),
                ],
                true,
            );
            // Strict, fully signed: N ok.
            let _ = finish("t", vec![Ok(ok()), Ok(ok()), Ok(ok())], false);
            // Strict abort from a handler: the single aborting error.
            let _ = strict_abort("t", AppError::Internal(anyhow::anyhow!("x")));
            // Strict fallback inside `finish`: the error counts once, the
            // ok items before it do NOT (they never reached the wire).
            let _ = finish("t", vec![Ok(ok()), Err(err())], false);
            // Empty batches record nothing in either mode.
            let _ = finish("t", vec![], false);
            let _ = finish("t", vec![], true);
        });

        let mut counts: HashMap<String, u64> = HashMap::new();
        for (key, _unit, _desc, value) in snapshotter.snapshot().into_vec() {
            let key = key.key();
            if key.name() != "oracle_context_item_total" {
                continue;
            }
            let label = |name: &str| {
                key.labels()
                    .find(|l| l.key() == name)
                    .map(|l| l.value().to_string())
                    .unwrap_or_else(|| panic!("missing label {name} on {key:?}"))
            };
            assert_eq!(label("endpoint"), "t");
            let DebugValue::Counter(n) = value else {
                panic!("item counter must be a counter, got {value:?}");
            };
            counts.insert(label("outcome"), n);
        }
        assert_eq!(counts.get("ok"), Some(&5), "{counts:?}");
        assert_eq!(counts.get("service_unavailable"), Some(&2), "{counts:?}");
        assert_eq!(counts.get("bad_request"), Some(&1), "{counts:?}");
        assert_eq!(counts.get("internal_error"), Some(&1), "{counts:?}");
        assert_eq!(counts.len(), 4, "no other outcome labels: {counts:?}");
    }

    /// `finish` is the only place the mode is applied: strict collapses
    /// to the first error, envelope keeps every slot in order.
    #[test]
    fn finish_applies_mode() {
        let strict = finish("t", vec![Ok(ok()), Err(err()), Ok(ok())], false);
        assert!(matches!(strict, Err(AppError::Unavailable(_))));

        let strict_ok = finish("t", vec![Ok(ok()), Ok(ok())], false).unwrap();
        assert!(matches!(strict_ok, ContextResponse::Strict(v) if v.len() == 2));

        let env = finish("t", vec![Ok(ok()), Err(err()), Ok(ok())], true).unwrap();
        let ContextResponse::Envelope(items) = env else {
            panic!("expected envelope");
        };
        assert_eq!(items.len(), 3);
        assert!(matches!(items[0], oracle::BatchItemResponse::Ok(_)));
        assert!(
            matches!(&items[1], oracle::BatchItemResponse::Error(e) if e.error == "service_unavailable")
        );
        assert!(matches!(items[2], oracle::BatchItemResponse::Ok(_)));
    }
}

#[cfg(test)]
mod request_shape_tests {
    use super::*;
    use alloy::primitives::{FixedBytes, U256};

    fn tuple() -> OracleRequestTuple {
        let io = |b: u8| IOV2 {
            token: Address::repeat_byte(b),
            vaultId: FixedBytes::ZERO,
        };
        let order = OrderV4 {
            owner: Address::ZERO,
            evaluable: EvaluableV4 {
                interpreter: Address::ZERO,
                store: Address::ZERO,
                bytecode: alloy::primitives::Bytes::new(),
            },
            validInputs: vec![io(0x11)],
            validOutputs: vec![io(0x22)],
            nonce: FixedBytes::ZERO,
        };
        (order, U256::ZERO, U256::ZERO, Address::ZERO)
    }

    #[test]
    fn single_tuple_decodes_as_non_batch_with_one_item() {
        let decoded = decode_request_body(&tuple().abi_encode()).unwrap();
        assert!(!decoded.is_batch);
        assert_eq!(decoded.items.len(), 1);
    }

    #[test]
    fn array_decodes_as_batch_preserving_order() {
        let mut second = tuple();
        second.0.validInputs[0].token = Address::repeat_byte(0x33);
        let decoded = decode_request_body(&vec![tuple(), second].abi_encode()).unwrap();
        assert!(decoded.is_batch);
        assert_eq!(decoded.items.len(), 2);
        assert_eq!(
            decoded.items[1].0.validInputs[0].token,
            Address::repeat_byte(0x33)
        );
    }

    /// The two edge sizes the envelope rule depends on: an empty array
    /// and a one-element array are BATCHES (envelope-eligible), not
    /// singles.
    #[test]
    fn empty_and_one_element_arrays_are_batches() {
        let empty = decode_request_body(&Vec::<OracleRequestTuple>::new().abi_encode()).unwrap();
        assert!(empty.is_batch);
        assert!(empty.items.is_empty());

        let one = decode_request_body(&vec![tuple()].abi_encode()).unwrap();
        assert!(one.is_batch);
        assert_eq!(one.items.len(), 1);
    }

    #[test]
    fn undecodable_body_is_bad_request() {
        // `.err()` rather than `.unwrap_err()`: `OrderV4` (from `sol!`)
        // has no `Debug`, so the `Ok` arm cannot be formatted.
        let err = decode_request_body(b"not abi")
            .err()
            .expect("garbage must not decode");
        assert_eq!(err.status_code(), StatusCode::BAD_REQUEST);
        assert!(err
            .to_error_response()
            .detail
            .contains("Invalid ABI-encoded body"));
    }
}

#[cfg(test)]
mod context_query_tests {
    use super::*;

    fn allow(raw: Option<&str>) -> bool {
        ContextQuery::parse(raw).allow_failure
    }

    #[test]
    fn absent_or_empty_query_is_false() {
        assert!(!allow(None));
        assert!(!allow(Some("")));
    }

    #[test]
    fn truthy_spellings() {
        assert!(allow(Some("allowFailure=true")));
        assert!(allow(Some("allowFailure=TRUE")));
        assert!(allow(Some("allowFailure=True")));
        assert!(allow(Some("allowFailure=1")));
    }

    #[test]
    fn falsy_and_unknown_values_are_false() {
        assert!(!allow(Some("allowFailure=false")));
        assert!(!allow(Some("allowFailure=0")));
        assert!(!allow(Some("allowFailure=yes")));
        assert!(!allow(Some("allowFailure=nonsense")));
        assert!(!allow(Some("allowFailure=")));
        assert!(!allow(Some("allowFailure")));
    }

    /// The key is case-sensitive and exact: `allowfailure` or
    /// `allow_failure` are unknown keys, not aliases, so a client cannot
    /// half-opt-in by accident.
    #[test]
    fn key_must_match_exactly() {
        assert!(!allow(Some("allowfailure=true")));
        assert!(!allow(Some("allow_failure=true")));
        assert!(!allow(Some("AllowFailure=true")));
        assert!(!allow(Some("xallowFailure=true")));
    }

    #[test]
    fn unknown_keys_are_ignored_around_the_flag() {
        assert!(allow(Some("foo=bar&allowFailure=true&baz=1")));
        assert!(!allow(Some("foo=bar&baz=1")));
        assert!(!allow(Some("foo=true")));
    }

    #[test]
    fn last_occurrence_wins_when_repeated() {
        assert!(!allow(Some("allowFailure=true&allowFailure=false")));
        assert!(allow(Some("allowFailure=false&allowFailure=true")));
    }

    /// The parser does no percent-decoding: values are matched
    /// literally. `true` and `1` need no encoding, so an encoded spelling
    /// is treated as "some other value" (false), and the contract is
    /// simply "send the literal characters".
    #[test]
    fn percent_encoded_values_are_not_decoded() {
        assert!(!allow(Some("allowFailure=%74rue")));
        assert!(!allow(Some("allowFailure=%31")));
        assert!(!allow(Some("allow%46ailure=true")));
    }

    /// Surrounding whitespace is not trimmed either — the client sends
    /// the bare token or gets the default.
    #[test]
    fn whitespace_is_not_trimmed() {
        assert!(!allow(Some("allowFailure= true")));
        assert!(!allow(Some("allowFailure=true ")));
        assert!(!allow(Some(" allowFailure=true")));
    }

    /// Garbage must parse to the default, never panic or reject — a
    /// malformed query is not a reason to refuse an oracle request.
    #[test]
    fn garbage_never_panics_and_reads_false() {
        for raw in ["%%%", "&&&", "===", "=&=&", "a=b=c", "&allowFailure=true&"] {
            let _ = ContextQuery::parse(Some(raw));
        }
        assert!(!allow(Some("%%%&==")));
        assert!(allow(Some("&allowFailure=true&")));
    }
}
