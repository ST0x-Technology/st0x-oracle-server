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
use crate::pricing_client::{LiveClient, QuoteSnapshot};
use crate::registry::{PriceDirection, ResolvedPair, TokenRegistry};
use chrono::Utc;
use st0x_pricing_types::Quote;

trait Clock: Send + Sync {
    fn now_unix_ms(&self) -> i64;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> i64 {
        Utc::now().timestamp_millis()
    }
}

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
    /// The chain this deployment signs for, from config. Scopes the
    /// pricing quote cache (only this chain's frames are served) and is
    /// carried at slot 9 of `/context/v7`, inside the signature, so a
    /// strategy can reject a frame signed for another chain — see
    /// `oracle::SCHEMA_VERSION_V7`.
    chain_id: u64,
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
    clock: Arc<dyn Clock>,
}

impl AppState {
    pub fn new(
        signer: Signer,
        registry: TokenRegistry,
        chain_id: u64,
        pricing: LiveClient,
        configured_symbols: Vec<String>,
        market_hours: Arc<MarketHoursCache>,
        metrics: MetricsHandle,
    ) -> Self {
        Self {
            signer,
            registry,
            chain_id,
            pricing,
            configured_symbols,
            market_hours,
            metrics,
            // Off until `with_signature_reuse` is called: `main.rs` passes
            // the configured margin, tests opt in explicitly so nothing
            // silently serves a previous frame.
            reuse: reuse::ReuseCache::new(0),
            clock: Arc::new(SystemClock),
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

    #[cfg(test)]
    fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
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

/// JSON error body of every non-2xx response the server itself produces
/// on `/context/v*`. Rejections that axum raises before a handler runs
/// are not JSON: 404 for an unknown route, 405 for a wrong method, 413
/// for a body over the default 2 MiB limit. The batch envelope
/// (`allowFailure=true`) reuses the same shape per failed item, so it is
/// public and round-trippable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Stable machine-readable code: `bad_request`, `internal_error`, or
    /// one of the `UnavailableReason` codes (`no_live_quote`,
    /// `expired_quote`). Mirrors the HTTP status `AppError` maps to.
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
    /// `?allowFailure=true` (or `=1`, case-insensitive). On a BATCH body
    /// the response is then always `200`, with one `BatchItemResponse`
    /// per request item. A failing item no longer takes the whole batch
    /// down. Absent, `false`, or any other value keeps the all-or-nothing
    /// behaviour every existing caller relies on. Ignored for
    /// single-tuple bodies.
    pub allow_failure: bool,
}

impl ContextQuery {
    /// Lenient parse of a raw query string. Unknown keys are ignored,
    /// garbage never errors, a missing value reads as `false`, and if the
    /// key repeats the last occurrence wins. This is deliberately not a
    /// typed `Query<T>` extractor: a malformed query must not turn an
    /// otherwise valid oracle request into a 400.
    ///
    /// Key and value are compared PRE-percent-decode, literally: the flag
    /// needs no encoding (`allowFailure`, `true`, `1` are plain ASCII), so
    /// an encoded spelling such as `allow%46ailure` or `%74rue` simply
    /// reads as "not the flag" — see `context_query_tests`.
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

    let items = resolved
        .into_iter()
        .map(|pair| pair.and_then(|pair| Err(legacy_signature_unavailable(endpoint, &pair.symbol))))
        .collect();
    finish(endpoint, items, envelope)
}

/// One request slot that built successfully, kept until the batch-final
/// revalidation pass. Holds the snapshot quote the response was built
/// from, so the final check sees the same quote the builder saw.
struct BuiltSlot<'a> {
    pair: ResolvedPair,
    quote: &'a QuoteSnapshot,
    response: BuiltResponse,
}

/// Final pass of a batch: every slot that built is re-checked against
/// ONE common timestamp taken after the last signature, so no response
/// in the reply is already expired when the reply leaves the server.
///
/// Per-item, like the build loop: a slot that fails here fails alone in
/// envelope mode and keeps its position; strict mode fails the whole
/// request at the first such slot, as it always has. Slots that already
/// failed earlier pass through untouched.
fn revalidate_batch(
    endpoint: &'static str,
    signs_expiry: bool,
    validated_at_unix_ms: i64,
    built: Vec<Result<BuiltSlot<'_>, AppError>>,
    current: &std::collections::HashMap<String, QuoteSnapshot>,
    envelope: bool,
) -> Result<Vec<Result<oracle::OracleResponse, AppError>>, AppError> {
    let mut items = Vec::with_capacity(built.len());
    for slot in built {
        let item = slot.and_then(|slot| {
            validate_quote_liveness(
                slot.quote,
                endpoint,
                &slot.pair.symbol,
                RefusalPhase::BatchFinal,
            )?;
            validate_expiry_deadline(
                slot.response.validity_expiry_unix_ms,
                validated_at_unix_ms,
                endpoint,
                &slot.pair.symbol,
                signs_expiry,
                RefusalPhase::BatchFinal,
            )?;
            let current_quote = current.get(&slot.pair.symbol).ok_or_else(|| {
                no_live_quote_at(endpoint, &slot.pair.symbol, RefusalPhase::BatchFinal)
            })?;
            validate_quote_liveness(
                current_quote,
                endpoint,
                &slot.pair.symbol,
                RefusalPhase::BatchFinal,
            )?;
            validate_quote_expiry(
                current_quote,
                validated_at_unix_ms,
                endpoint,
                &slot.pair.symbol,
                signs_expiry,
                RefusalPhase::BatchFinal,
            )?;
            if signs_expiry
                && slot.response.validity_expiry_unix_ms > effective_expiry_unix_ms(current_quote)?
            {
                return Err(expired_quote_at(
                    endpoint,
                    &slot.pair.symbol,
                    RefusalPhase::BatchFinal,
                    "Signed expiry exceeds the current execution bound.".into(),
                ));
            }
            Ok(slot.response.response)
        });
        if !envelope {
            if let Err(err) = item {
                return Err(err);
            }
        }
        items.push(item);
    }
    Ok(items)
}

/// What a `/context/v*` handler returns on `200`. The two arms are the
/// two wire shapes described on `oracle::BatchItemResponse`; which one a
/// request gets is decided by `finish`, never by the caller of `finish`.
#[derive(Debug)]
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
/// vault's UNDERLYING asset rate rather than the vault-share rate, no NAV
/// ratio is signed, and the deployment's chain id is signed at slot 9.
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
/// the vault price from, so it is what must be signed.
///
/// The property v7 adds (RAI-1991): EIP-191 is chain-agnostic and no
/// other slot names a chain, while ST0x token addresses are deterministic
/// clones across chains — so a v6-and-below payload for chain A verifies
/// unchanged inside an order on chain B. Slot 9 carries this deployment's
/// `chain_id` inside the signature; a per-deployment
/// `equal-to(signed-context<0 9>() expected-chain-id)` binding closes the
/// gap in the strategy rather than in the URL wiring. See
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
/// 8 (v5, v6, v7), what slot 9 carries (the vault NAV ratio for v6, the
/// deployment's chain id for v7, nothing for v4/v5), and WHICH price is
/// signed into slot 1 — the vault-share rate for v4/v5/v6, the vault's
/// UNDERLYING asset rate for v7.
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
            tracing::trace!(
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

    if schema == PairSchema::V4 {
        let items = resolved
            .into_iter()
            .map(|slot| {
                slot.and_then(|(_, _, pair)| {
                    Err(legacy_signature_unavailable(endpoint, &pair.symbol))
                })
            })
            .collect();
        return finish(endpoint, items, envelope);
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

    let mut built: Vec<Result<BuiltSlot<'_>, AppError>> = Vec::with_capacity(resolved.len());
    for slot in resolved {
        let item = match slot {
            Err(err) => Err(err),
            Ok((input_token, output_token, pair)) => match snapshot.get(&pair.symbol) {
                None => Err(no_live_quote(endpoint, &pair.symbol)),
                Some(quote) => build_response_from_quote_pair_bound(
                    &state,
                    &pair,
                    quote,
                    input_token,
                    output_token,
                    &session_info,
                    schema,
                )
                .await
                .map(|response| BuiltSlot {
                    pair,
                    quote,
                    response,
                }),
            },
        };
        // Strict mode stops at the first failure, as before: nothing
        // after it is signed.
        if !envelope {
            if let Err(err) = item {
                return Err(strict_abort(endpoint, err));
            }
        }
        built.push(item);
    }

    let current_symbols: Vec<&str> = built
        .iter()
        .filter_map(|slot| slot.as_ref().ok())
        .map(|slot| slot.pair.symbol.as_str())
        .collect();
    let current = state.pricing.snapshot_many(&current_symbols).await;
    let validated_at_unix_ms = state.clock.now_unix_ms();
    let items = revalidate_batch(
        endpoint,
        schema.signs_expiry(),
        validated_at_unix_ms,
        built,
        &current,
        envelope,
    )
    .map_err(|err| strict_abort(endpoint, err))?;

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

    tracing::trace!(
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
    u64::try_from(effective_expiry_unix_ms(quote)? / 1000)
        .map_err(|_| AppError::Internal(anyhow::anyhow!("quote expiry out of range")))
}

fn effective_expiry_unix_ms(quote: &Quote) -> Result<i64, AppError> {
    let execution_deadline = quote
        .execution_deadline_unix_ms
        .filter(|deadline| *deadline > 0)
        .ok_or_else(|| AppError::Unavailable {
            reason: UnavailableReason::ExpiredQuote,
            detail: "Execution deadline is missing or invalid.".into(),
        })?;
    Ok(quote.expiry_unix_ms.min(execution_deadline))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefusalPhase {
    Admission,
    PreSign,
    PostSign,
    PreReuseReturn,
    BatchFinal,
}

impl RefusalPhase {
    const fn label(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::PreSign => "pre_sign",
            Self::PostSign => "post_sign",
            Self::PreReuseReturn => "pre_reuse_return",
            Self::BatchFinal => "batch_final",
        }
    }
}

/// Log a refusal. At admission it is the normal answer while a market is
/// closed and repeats on every poll, so it goes to TRACE; a quote that dies
/// later in the request is a race worth seeing, so it stays at WARN.
macro_rules! log_refusal {
    ($phase:expr, $($event:tt)+) => {
        if $phase == RefusalPhase::Admission {
            tracing::trace!($($event)+)
        } else {
            tracing::warn!($($event)+)
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableReason {
    NoLiveQuote,
    ExpiredQuote,
    LegacySchema,
}

impl UnavailableReason {
    const fn code(self) -> &'static str {
        match self {
            Self::NoLiveQuote => "no_live_quote",
            Self::ExpiredQuote => "expired_quote",
            Self::LegacySchema => "legacy_schema",
        }
    }
}

fn unavailable(
    reason: UnavailableReason,
    endpoint: &'static str,
    symbol: &str,
    phase: RefusalPhase,
    detail: String,
) -> AppError {
    ::metrics::counter!(
        "oracle_quote_refusals_total",
        "reason" => reason.code(),
        "endpoint" => endpoint,
        "symbol" => symbol.to_owned(),
        "phase" => phase.label(),
    )
    .increment(1);
    AppError::Unavailable { reason, detail }
}

fn no_live_quote_at(endpoint: &'static str, symbol: &str, phase: RefusalPhase) -> AppError {
    let reason = UnavailableReason::NoLiveQuote;
    let detail = format!("No live quote for {symbol}.");
    log_refusal!(
        phase,
        reason = reason.code(),
        endpoint,
        symbol,
        phase = phase.label(),
        %detail,
        "Refusing oracle quote request"
    );
    unavailable(reason, endpoint, symbol, phase, detail)
}

fn no_live_quote(endpoint: &'static str, symbol: &str) -> AppError {
    no_live_quote_at(endpoint, symbol, RefusalPhase::Admission)
}

fn expired_quote_at(
    endpoint: &'static str,
    symbol: &str,
    phase: RefusalPhase,
    detail: String,
) -> AppError {
    let reason = UnavailableReason::ExpiredQuote;
    log_refusal!(
        phase,
        reason = reason.code(),
        endpoint,
        symbol,
        phase = phase.label(),
        %detail,
        "Refusing expired pricing quote"
    );
    unavailable(reason, endpoint, symbol, phase, detail)
}

fn legacy_signature_unavailable(endpoint: &'static str, symbol: &str) -> AppError {
    let reason = UnavailableReason::LegacySchema;
    let detail = "Legacy schema has no verified settlement expiry bound; migrate to v5, v6, or v7."
        .to_string();
    unavailable(reason, endpoint, symbol, RefusalPhase::Admission, detail)
}

fn validate_quote_liveness(
    quote: &QuoteSnapshot,
    endpoint: &'static str,
    symbol: &str,
    phase: RefusalPhase,
) -> Result<(), AppError> {
    if quote.is_live() {
        Ok(())
    } else {
        Err(no_live_quote_at(endpoint, symbol, phase))
    }
}

fn validate_expiry_deadline(
    expiry_unix_ms: i64,
    checked_at_unix_ms: i64,
    endpoint: &'static str,
    symbol: &str,
    signs_expiry: bool,
    phase: RefusalPhase,
) -> Result<(), AppError> {
    if quote_is_live(expiry_unix_ms, checked_at_unix_ms, signs_expiry) {
        return Ok(());
    }

    log_refusal!(
        phase,
        reason = UnavailableReason::ExpiredQuote.code(),
        phase = phase.label(),
        endpoint,
        symbol,
        expiry_unix_ms,
        checked_at_unix_ms,
        "Refusing expired pricing quote"
    );
    Err(unavailable(
        UnavailableReason::ExpiredQuote,
        endpoint,
        symbol,
        phase,
        format!(
            "Quote for {symbol} expired at {expiry_unix_ms} (checked at {checked_at_unix_ms})."
        ),
    ))
}

fn validate_quote_expiry(
    quote: &Quote,
    checked_at_unix_ms: i64,
    endpoint: &'static str,
    symbol: &str,
    signs_expiry: bool,
    phase: RefusalPhase,
) -> Result<(), AppError> {
    let effective_expiry = effective_expiry_unix_ms(quote).map_err(|_| {
        expired_quote_at(
            endpoint,
            symbol,
            phase,
            format!("Execution deadline for {symbol} is missing or invalid."),
        )
    })?;
    if quote.execution_deadline_unix_ms == Some(effective_expiry)
        && !quote_is_live(effective_expiry, checked_at_unix_ms, signs_expiry)
    {
        return Err(expired_quote_at(
            endpoint,
            symbol,
            phase,
            format!(
                "Execution deadline for {symbol} elapsed at {effective_expiry} (checked at {checked_at_unix_ms})."
            ),
        ));
    }
    validate_expiry_deadline(
        effective_expiry,
        checked_at_unix_ms,
        endpoint,
        symbol,
        signs_expiry,
        phase,
    )
}

fn quote_is_live(expiry_unix_ms: i64, checked_at_unix_ms: i64, signs_expiry: bool) -> bool {
    expiry_unix_ms > checked_at_unix_ms
        && (!signs_expiry || expiry_unix_ms.div_euclid(1000) > checked_at_unix_ms.div_euclid(1000))
}

#[derive(Debug)]
struct BuiltResponse {
    response: oracle::OracleResponse,
    /// The effective deadline of the response itself. For a freshly signed
    /// response this comes from the source quote; for reuse it is the older
    /// expiry encoded in the stored signed context.
    validity_expiry_unix_ms: i64,
}

#[cfg(test)]
async fn build_response_from_quote(
    state: &AppState,
    pair: &ResolvedPair,
    quote: &QuoteSnapshot,
) -> Result<BuiltResponse, AppError> {
    validate_quote_liveness(quote, "v1", &pair.symbol, RefusalPhase::Admission)?;
    validate_quote_expiry(
        quote,
        state.clock.now_unix_ms(),
        "v1",
        &pair.symbol,
        false,
        RefusalPhase::Admission,
    )?;
    let publish_time = publish_time_from_quote(quote)?;

    let price_bytes = pick_rate_bytes(quote, pair.direction).map_err(AppError::Internal)?;

    let context = oracle::build_context(price_bytes, publish_time)?;
    validate_quote_liveness(quote, "v1", &pair.symbol, RefusalPhase::PreSign)?;
    validate_quote_expiry(
        quote,
        state.clock.now_unix_ms(),
        "v1",
        &pair.symbol,
        false,
        RefusalPhase::PreSign,
    )?;
    let (signature, signer) = state.signer.sign_context(&context).await?;

    let validated_at_unix_ms = state.clock.now_unix_ms();
    validate_quote_liveness(quote, "v1", &pair.symbol, RefusalPhase::PostSign)?;
    validate_quote_expiry(
        quote,
        validated_at_unix_ms,
        "v1",
        &pair.symbol,
        false,
        RefusalPhase::PostSign,
    )?;

    tracing::trace!(
        symbol = %pair.symbol,
        direction = pair.direction.as_str(),
        schema = "v1",
        publish_time,
        source_ts_unix_ms = quote.source_ts_unix_ms,
        expiry_unix_ms = quote.expiry_unix_ms,
        validated_at_unix_ms,
        "Building signed context from live pricing quote"
    );

    Ok(BuiltResponse {
        response: oracle::OracleResponse {
            signer,
            context,
            signature,
        },
        validity_expiry_unix_ms: quote.expiry_unix_ms,
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
    quote: &QuoteSnapshot,
    input_token: Address,
    output_token: Address,
    session_info: &crate::market_hours::SessionInfo,
    schema: PairSchema,
) -> Result<BuiltResponse, AppError> {
    validate_quote_liveness(quote, schema.tag(), &pair.symbol, RefusalPhase::Admission)?;
    validate_quote_expiry(
        quote,
        state.clock.now_unix_ms(),
        schema.tag(),
        &pair.symbol,
        schema.signs_expiry(),
        RefusalPhase::Admission,
    )?;
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
        // vault price on-chain from the live ratio (RAI-2198) — plus this
        // deployment's chain id at slot 9 (RAI-1991). The chain id is the
        // same one the quote cache is scoped by, so the slot names the
        // chain whose frames produced the price.
        (PairSchema::V7, Some(expiry)) => oracle::build_context_v7(
            price_bytes,
            publish_time,
            session_bytes,
            session_start,
            session_end,
            input_token,
            output_token,
            expiry,
            state.chain_id,
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
        validate_quote_liveness(quote, schema.tag(), &pair.symbol, RefusalPhase::PreSign)?;
        let now_unix_ms = state.clock.now_unix_ms();
        let now_secs = u64::try_from(now_unix_ms.div_euclid(1000))
            .map_err(|_| AppError::Internal(anyhow::anyhow!("system clock before 1970")))?;
        if let Some(previous) =
            state
                .reuse
                .lookup(key, &context, *expiry, now_secs, quote.generation())
        {
            let validated_at_unix_ms = state.clock.now_unix_ms();
            validate_quote_liveness(
                quote,
                schema.tag(),
                &pair.symbol,
                RefusalPhase::PreReuseReturn,
            )?;
            let stored_expiry_unix_ms = i64::try_from(previous.expiry_unix_secs)
                .ok()
                .and_then(|expiry| expiry.checked_mul(1000))
                .ok_or_else(|| {
                    AppError::Internal(anyhow::anyhow!("stored quote expiry out of range"))
                })?;
            validate_expiry_deadline(
                stored_expiry_unix_ms,
                validated_at_unix_ms,
                schema.tag(),
                &pair.symbol,
                true,
                RefusalPhase::PreReuseReturn,
            )?;
            ::metrics::counter!("oracle_signature_reuse_total").increment(1);
            tracing::trace!(
                symbol = %pair.symbol,
                direction = pair.direction.as_str(),
                schema = schema.tag(),
                input = %input_token,
                output = %output_token,
                candidate_publish_time = publish_time,
                session = session_info.session.as_str(),
                session_start,
                session_end,
                source_ts_unix_ms = quote.source_ts_unix_ms,
                expiry_unix_ms = stored_expiry_unix_ms,
                validated_at_unix_ms,
                "Price unchanged and previous quote still valid; reusing its signature"
            );
            return Ok(BuiltResponse {
                response: previous.response,
                validity_expiry_unix_ms: stored_expiry_unix_ms,
            });
        }
    }

    validate_quote_liveness(quote, schema.tag(), &pair.symbol, RefusalPhase::PreSign)?;
    validate_quote_expiry(
        quote,
        state.clock.now_unix_ms(),
        schema.tag(),
        &pair.symbol,
        schema.signs_expiry(),
        RefusalPhase::PreSign,
    )?;

    let (signature, signer) = state.signer.sign_context(&context).await?;

    let validated_at_unix_ms = state.clock.now_unix_ms();
    validate_quote_liveness(quote, schema.tag(), &pair.symbol, RefusalPhase::PostSign)?;
    validate_quote_expiry(
        quote,
        validated_at_unix_ms,
        schema.tag(),
        &pair.symbol,
        schema.signs_expiry(),
        RefusalPhase::PostSign,
    )?;

    let response = oracle::OracleResponse {
        signer,
        context,
        signature,
    };
    if let Some((key, expiry)) = reuse {
        state
            .reuse
            .store(key, expiry, response.clone(), quote.generation());
    }
    tracing::trace!(
        symbol = %pair.symbol,
        direction = pair.direction.as_str(),
        schema = schema.tag(),
        input = %input_token,
        output = %output_token,
        publish_time,
        session = session_info.session.as_str(),
        session_start,
        session_end,
        source_ts_unix_ms = quote.source_ts_unix_ms,
        expiry_unix_ms = quote.expiry_unix_ms,
        validated_at_unix_ms,
        "Building pair-bound signed context from live pricing quote"
    );
    Ok(BuiltResponse {
        response,
        validity_expiry_unix_ms: effective_expiry_unix_ms(quote)?,
    })
}

#[derive(Debug)]
pub enum AppError {
    Internal(anyhow::Error),
    BadRequest(String),
    /// The server is alive but the poll loop hasn't produced a quote yet
    /// for the requested symbol. Distinct from BadRequest because it's transient
    /// and retrying may succeed.
    Unavailable {
        reason: UnavailableReason,
        detail: String,
    },
}

impl AppError {
    /// HTTP status this error maps to when it is the outcome of a whole
    /// request (single-tuple requests, and batches without
    /// `allowFailure`).
    pub fn status_code(&self) -> StatusCode {
        match self {
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// Stable machine-readable code. This is the `error` field of the
    /// JSON body AND the `outcome` label of `oracle_context_item_total`,
    /// so a dashboard and a client see the same vocabulary.
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Internal(_) => "internal_error",
            AppError::BadRequest(_) => "bad_request",
            AppError::Unavailable { reason, .. } => reason.code(),
        }
    }

    /// The JSON body for this error. Used both as the body of a non-2xx
    /// response and as the per-item `body` of a batch envelope error, so
    /// the two paths can never disagree on the `error` code strings.
    pub fn to_error_response(&self) -> ErrorResponse {
        let detail = match self {
            AppError::Internal(err) => format!("{}", err),
            AppError::BadRequest(detail) | AppError::Unavailable { detail, .. } => detail.clone(),
        };
        ErrorResponse {
            error: self.code().to_string(),
            detail,
        }
    }

    /// Log this error: `error!` for internal failures (with the full anyhow
    /// chain), `trace!` for client errors, since one broken caller repeats
    /// the same bad body several times a second and
    /// `oracle_context_request_total` already counts them. `Unavailable` is
    /// not logged here: refusals decided on a live quote log where they are
    /// decided, and legacy-schema refusals are counted by
    /// `oracle_quote_refusals_total` only.
    pub fn log(&self) {
        match self {
            AppError::Internal(err) => tracing::error!("Internal error: {:?}", err),
            AppError::BadRequest(detail) => tracing::trace!("Bad request: {}", detail),
            AppError::Unavailable { .. } => {}
        }
    }

    /// Same severities as `log`, for a failed slot inside a `200`
    /// envelope. Carries the endpoint and the item's position so an
    /// operator can tie the line to one request in a batch that
    /// otherwise left no non-2xx trace. `Unavailable` lines are TRACE for
    /// the reasons given on `log`.
    pub fn log_batch_item(&self, endpoint: &'static str, index: usize) {
        match self {
            AppError::Internal(err) => tracing::error!(
                endpoint,
                index,
                "Batch item failed (internal error): {:?}",
                err
            ),
            AppError::BadRequest(detail) => {
                tracing::trace!(
                    endpoint,
                    index,
                    "Batch item failed (bad request): {}",
                    detail
                )
            }
            AppError::Unavailable { reason, detail } => tracing::trace!(
                endpoint,
                index,
                reason = reason.code(),
                "Batch item failed (unavailable): {}",
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
mod expiry_tests {
    use super::*;
    use crate::market_hours::{Session, SessionInfo};
    use crate::registry::PriceDirection;
    use st0x_pricing_types::{
        ErrorCode, ErrorFrame, HaltFrame, ServerFrame, WireAddress, WireFloat, WireU256,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    struct SequenceClock {
        values: Mutex<(VecDeque<i64>, i64)>,
    }

    impl SequenceClock {
        fn new(values: impl IntoIterator<Item = i64>) -> Self {
            let values: VecDeque<_> = values.into_iter().collect();
            let fallback = *values.back().expect("clock needs at least one value");
            Self {
                values: Mutex::new((values, fallback)),
            }
        }
    }

    impl Clock for SequenceClock {
        fn now_unix_ms(&self) -> i64 {
            let mut guard = self.values.lock().unwrap();
            guard.0.pop_front().unwrap_or(guard.1)
        }
    }

    fn test_quote(expiry_unix_ms: i64) -> Quote {
        let rate: B256 = Float::parse("100".to_string()).unwrap().into();
        Quote {
            asset: "COIN".to_string(),
            chain_id: 1,
            base: WireAddress::from_bytes([0x11; 20]),
            quote: WireAddress::from_bytes([0x22; 20]),
            rate_base_to_quote: WireFloat::from_bytes(rate.into()),
            rate_quote_to_base: WireFloat::from_bytes(rate.into()),
            expiry_unix_ms,
            execution_deadline_unix_ms: Some(expiry_unix_ms),
            source_ts_unix_ms: 1_000,
            nav_ratio: WireU256::ZERO,
            underlying_rate_base_to_quote: WireFloat::from_bytes(rate.into()),
            underlying_rate_quote_to_base: WireFloat::from_bytes(rate.into()),
        }
    }

    fn test_snapshot(expiry_unix_ms: i64) -> QuoteSnapshot {
        QuoteSnapshot::always_live(test_quote(expiry_unix_ms))
    }

    async fn test_state(clock: Arc<dyn Clock>, reuse_secs: u64) -> AppState {
        AppState::new(
            Signer::new(TEST_KEY).unwrap(),
            TokenRegistry::new(
                vec![(
                    "0x1111111111111111111111111111111111111111".into(),
                    "COIN".into(),
                )],
                "0x2222222222222222222222222222222222222222",
            )
            .unwrap(),
            1,
            LiveClient::with_seeded(vec![], 1).await,
            vec!["COIN".into()],
            Arc::new(MarketHoursCache::new()),
            MetricsHandle::install().unwrap(),
        )
        .with_signature_reuse(reuse_secs)
        .with_clock(clock)
    }

    fn pair() -> ResolvedPair {
        ResolvedPair {
            symbol: "COIN".into(),
            direction: PriceDirection::BaseToQuote,
        }
    }

    fn request_tuple(input_token: Address, output_token: Address) -> OracleRequestTuple {
        (
            OrderV4 {
                owner: Address::ZERO,
                evaluable: EvaluableV4 {
                    interpreter: Address::ZERO,
                    store: Address::ZERO,
                    bytecode: alloy::primitives::Bytes::new(),
                },
                validInputs: vec![IOV2 {
                    token: input_token,
                    vaultId: B256::ZERO,
                }],
                validOutputs: vec![IOV2 {
                    token: output_token,
                    vaultId: B256::ZERO,
                }],
                nonce: B256::ZERO,
            },
            alloy::primitives::U256::ZERO,
            alloy::primitives::U256::ZERO,
            Address::ZERO,
        )
    }

    fn request_body(requests: Vec<OracleRequestTuple>) -> Bytes {
        Bytes::from(requests.abi_encode())
    }

    fn two_symbol_quotes(first_expiry: i64, second_expiry: i64) -> Vec<Quote> {
        let first = test_quote(first_expiry);
        let mut second = test_quote(second_expiry);
        second.asset = "DRAM".into();
        let second_rate: B256 = Float::parse("50".to_string()).unwrap().into();
        second.rate_base_to_quote = WireFloat::from_bytes(second_rate.into());
        second.rate_quote_to_base = WireFloat::from_bytes(second_rate.into());
        vec![first, second]
    }

    async fn two_symbol_state(clock: Arc<dyn Clock>, quotes: Vec<Quote>) -> Arc<AppState> {
        Arc::new(
            AppState::new(
                Signer::new(TEST_KEY).unwrap(),
                TokenRegistry::new(
                    vec![
                        (
                            "0x1111111111111111111111111111111111111111".into(),
                            "COIN".into(),
                        ),
                        (
                            "0x3333333333333333333333333333333333333333".into(),
                            "DRAM".into(),
                        ),
                    ],
                    "0x2222222222222222222222222222222222222222",
                )
                .unwrap(),
                1,
                LiveClient::with_seeded(quotes, 1).await,
                vec!["COIN".into(), "DRAM".into()],
                Arc::new(MarketHoursCache::new()),
                MetricsHandle::install().unwrap(),
            )
            .with_clock(clock),
        )
    }

    fn two_symbol_request_body() -> Bytes {
        request_body(vec![
            request_tuple(Address::from([0x22; 20]), Address::from([0x11; 20])),
            request_tuple(Address::from([0x22; 20]), Address::from([0x33; 20])),
        ])
    }

    #[test]
    fn raw_millisecond_deadline_is_exclusive_without_flooring() {
        let now = 1_700_000_000_500;
        assert!(!quote_is_live(now - 1, now, false));
        assert!(!quote_is_live(now, now, false));
        assert!(quote_is_live(now + 1, now, false));
        assert!(quote_is_live(i64::MAX, now, false));
    }

    #[test]
    fn expiry_bearing_schemas_require_a_future_whole_second() {
        let now = 1_700_000_000_500;
        assert!(quote_is_live(now + 1, now, false), "v1/v4 use raw ms");
        assert!(
            !quote_is_live(now + 499, now, true),
            "same encoded second is already ineffective on chain"
        );
        assert!(quote_is_live(1_700_000_001_000, now, true));
        assert!(quote_is_live(i64::MAX, now, true));
    }

    #[test]
    fn subsecond_expiry_boundary_is_mapped_to_every_schema() {
        let now = 1_700_000_000_500;
        let raw_live_same_second = now + 1;

        assert!(quote_is_live(raw_live_same_second, now, false), "v1");
        assert!(quote_is_live(
            raw_live_same_second,
            now,
            PairSchema::V4.signs_expiry()
        ));
        for schema in [PairSchema::V5, PairSchema::V6, PairSchema::V7] {
            assert!(
                !quote_is_live(raw_live_same_second, now, schema.signs_expiry()),
                "{} must require a future encoded expiry second",
                schema.tag()
            );
        }
    }

    #[test]
    fn unavailable_reason_codes_are_stable() {
        assert_eq!(UnavailableReason::NoLiveQuote.code(), "no_live_quote");
        assert_eq!(UnavailableReason::ExpiredQuote.code(), "expired_quote");
        assert_eq!(UnavailableReason::LegacySchema.code(), "legacy_schema");
    }

    #[test]
    fn execution_deadline_bounds_signed_expiry_and_fails_closed() {
        let mut quote = test_quote(20_000);
        quote.execution_deadline_unix_ms = Some(10_999);
        assert_eq!(effective_expiry_unix_ms(&quote).unwrap(), 10_999);
        assert_eq!(expiry_from_quote(&quote).unwrap(), 10);

        for deadline in [None, Some(0), Some(-1)] {
            quote.execution_deadline_unix_ms = deadline;
            assert!(matches!(
                effective_expiry_unix_ms(&quote),
                Err(AppError::Unavailable {
                    reason: UnavailableReason::ExpiredQuote,
                    ..
                })
            ));
        }
    }

    #[tokio::test]
    async fn expiry_crossing_during_signing_is_refused_post_sign() {
        let state = test_state(Arc::new(SequenceClock::new([1_000, 1_000, 2_000])), 0).await;
        let error = build_response_from_quote(&state, &pair(), &test_snapshot(2_000))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AppError::Unavailable {
                reason: UnavailableReason::ExpiredQuote,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn stale_source_revokes_snapshot_while_signature_is_in_flight() {
        let pricing = LiveClient::with_seeded(vec![test_quote(i64::MAX)], 1).await;
        let state = Arc::new(
            AppState::new(
                Signer::new(TEST_KEY)
                    .unwrap()
                    .with_test_delay(Duration::from_millis(100)),
                TokenRegistry::new(
                    vec![(
                        "0x1111111111111111111111111111111111111111".into(),
                        "COIN".into(),
                    )],
                    "0x2222222222222222222222222222222222222222",
                )
                .unwrap(),
                1,
                pricing.clone(),
                vec!["COIN".into()],
                Arc::new(MarketHoursCache::new()),
                MetricsHandle::install().unwrap(),
            )
            .with_clock(Arc::new(SequenceClock::new([1_000]))),
        );
        let request = request_body(vec![request_tuple(
            Address::from([0x22; 20]),
            Address::from([0x11; 20]),
        )]);

        let request_state = Arc::clone(&state);
        let in_flight = tokio::spawn(async move {
            post_signed_context_pair_bound(
                request_state,
                ContextQuery::default(),
                request,
                PairSchema::V5,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.signer.cache_stats().misses == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request must reach the delayed signer");

        let frame = ServerFrame::Error(ErrorFrame {
            code: ErrorCode::StaleSource,
            asset: Some("COIN".into()),
            last_ok_unix_ms: None,
            detail: None,
        });
        let mut encoded = Vec::new();
        ciborium::into_writer(&frame, &mut encoded).unwrap();
        let decoded = crate::pricing_client::decode_server_frame(&encoded).unwrap();
        pricing.apply_test_frame(decoded).await;

        let error = in_flight.await.unwrap().unwrap_err();
        assert!(matches!(
            &error,
            AppError::Unavailable {
                reason: UnavailableReason::NoLiveQuote,
                ..
            }
        ));
        assert_eq!(
            error.into_response().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn concurrent_deadline_changes_revalidate_in_flight_responses() {
        for change in ["shorten", "revoke", "halt", "extend"] {
            let entered = Arc::new(tokio::sync::Semaphore::new(0));
            let release = Arc::new(tokio::sync::Semaphore::new(0));
            let mut quote = test_quote(120_000);
            let pricing = LiveClient::with_seeded(vec![quote.clone()], 1).await;
            let state = Arc::new(
                AppState::new(
                    Signer::new(TEST_KEY)
                        .unwrap()
                        .with_gate(entered.clone(), release.clone()),
                    TokenRegistry::new(
                        vec![(
                            "0x1111111111111111111111111111111111111111".into(),
                            "COIN".into(),
                        )],
                        "0x2222222222222222222222222222222222222222",
                    )
                    .unwrap(),
                    1,
                    pricing.clone(),
                    vec!["COIN".into()],
                    Arc::new(MarketHoursCache::new()),
                    MetricsHandle::install().unwrap(),
                )
                .with_clock(Arc::new(SequenceClock::new([1_000]))),
            );
            let request = request_body(vec![request_tuple(
                Address::from([0x22; 20]),
                Address::from([0x11; 20]),
            )]);
            let request_state = Arc::clone(&state);
            let in_flight = tokio::spawn(async move {
                post_signed_context_pair_bound(
                    request_state,
                    ContextQuery::default(),
                    request,
                    PairSchema::V5,
                )
                .await
            });
            entered.acquire().await.unwrap().forget();

            match change {
                "halt" => {
                    pricing
                        .apply_test_frame(ServerFrame::Halt(HaltFrame {
                            asset: quote.asset.clone(),
                            chain_id: quote.chain_id,
                            base: quote.base,
                            quote: quote.quote,
                            halted: true,
                            reason: None,
                        }))
                        .await;
                }
                _ => {
                    quote.execution_deadline_unix_ms = match change {
                        "shorten" => Some(60_000),
                        "extend" => Some(180_000),
                        _ => None,
                    };
                    quote.expiry_unix_ms = 180_000;
                    pricing.seed(quote).await;
                }
            }
            release.add_permits(1);

            let result = in_flight.await.unwrap();
            if change == "extend" {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(AppError::Unavailable { .. })));
            }
        }
    }

    #[tokio::test]
    async fn batch_revalidates_all_responses_at_one_final_timestamp() {
        let clock = Arc::new(SequenceClock::new([
            1_000, 1_000, 1_000, 1_000, // first response completes while live
            1_000, 1_000, 1_000, 3_000, // second response crosses the first deadline
            3_000, // one common final validation timestamp
        ]));
        let state = two_symbol_state(clock, two_symbol_quotes(3_000, 4_000)).await;

        let error = post_signed_context_pair_bound(
            state,
            ContextQuery::default(),
            two_symbol_request_body(),
            PairSchema::V5,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            AppError::Unavailable {
                reason: UnavailableReason::ExpiredQuote,
                ..
            }
        ));
    }

    /// Same clock as above, but the caller opted into per-item results:
    /// the slot that expired during the batch fails alone, the live slot
    /// is still delivered, and the request is a `200` envelope.
    #[tokio::test]
    async fn envelope_batch_keeps_final_revalidation_per_slot() {
        let clock = Arc::new(SequenceClock::new([
            1_000, 1_000, 1_000, 1_000, // first response completes while live
            1_000, 1_000, 1_000, 3_000, // second response crosses the first deadline
            3_000, // one common final validation timestamp
        ]));
        let state = two_symbol_state(clock, two_symbol_quotes(3_000, 4_000)).await;

        let response = post_signed_context_pair_bound(
            state,
            ContextQuery {
                allow_failure: true,
            },
            two_symbol_request_body(),
            PairSchema::V5,
        )
        .await
        .unwrap();
        let ContextResponse::Envelope(items) = response else {
            panic!("expected envelope, got {response:?}");
        };
        assert_eq!(items.len(), 2);
        assert!(
            matches!(&items[0], oracle::BatchItemResponse::Error(e) if e.error == "expired_quote"),
            "{:?}",
            items[0]
        );
        assert!(
            matches!(items[1], oracle::BatchItemResponse::Ok(_)),
            "{:?}",
            items[1]
        );
    }

    #[tokio::test]
    async fn pair_bound_batch_revalidates_all_responses_at_one_final_timestamp() {
        let clock = Arc::new(SequenceClock::new([
            1_000, 1_000, 1_000, 1_000, // first response completes while live
            1_000, 1_000, 1_000, 3_000, // second signature crosses the first deadline
            3_000, // one common final validation timestamp
        ]));
        let state = two_symbol_state(clock, two_symbol_quotes(3_000, 4_000)).await;

        let error = post_signed_context_pair_bound(
            state,
            ContextQuery::default(),
            two_symbol_request_body(),
            PairSchema::V5,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            AppError::Unavailable {
                reason: UnavailableReason::ExpiredQuote,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn post_sign_expiry_is_counted_once_and_not_stored_for_reuse() {
        let clock = Arc::new(SequenceClock::new([
            1_000, 1_000, 1_000, 10_000, // first response expires after signing
            1_000, 1_000, 1_000, 1_000, // replacement remains live throughout
        ]));
        let state = test_state(clock, 1).await;
        let session = SessionInfo {
            session: Session::Rth,
            start: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            end: chrono::DateTime::from_timestamp(30, 0).unwrap(),
        };
        let input = Address::from([0x11; 20]);
        let output = Address::from([0x22; 20]);

        let first_quote = test_snapshot(10_000);
        let error = build_response_from_quote_pair_bound(
            &state,
            &pair(),
            &first_quote,
            input,
            output,
            &session,
            PairSchema::V5,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            AppError::Unavailable {
                reason: UnavailableReason::ExpiredQuote,
                ..
            }
        ));
        let metrics = state.metrics.render();
        let refusal = metrics
            .lines()
            .find(|line| {
                line.starts_with("oracle_quote_refusals_total{")
                    && line.contains("endpoint=\"v5\"")
                    && line.contains("phase=\"post_sign\"")
                    && line.contains("reason=\"expired_quote\"")
                    && line.contains("symbol=\"COIN\"")
            })
            .expect("post-sign refusal metric");
        assert_eq!(refusal.split_whitespace().last(), Some("1"));

        let mut replacement = test_quote(20_000);
        replacement.source_ts_unix_ms = 2_000;
        let replacement = first_quote.in_same_generation(replacement);
        let response = build_response_from_quote_pair_bound(
            &state,
            &pair(),
            &replacement,
            input,
            output,
            &session,
            PairSchema::V5,
        )
        .await
        .unwrap();
        let publish_time = Float::from(B256::from(response.response.context[2]));
        assert_eq!(
            publish_time.format().unwrap(),
            "2",
            "a post-sign refusal must not populate cross-frame reuse"
        );
    }

    #[tokio::test]
    async fn quote_that_remains_live_after_signing_succeeds() {
        let state = test_state(Arc::new(SequenceClock::new([1_000, 1_000, 1_999])), 0).await;
        build_response_from_quote(&state, &pair(), &test_snapshot(2_000))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn expiry_crossing_before_reuse_return_is_refused() {
        let clock = Arc::new(SequenceClock::new([
            1_000, 1_000, 1_000, 1_000, // first response: admission through post-sign
            1_000, 1_000, 10_000, // second response: admission, lookup, reuse return
        ]));
        let state = test_state(clock, 1).await;
        let session = SessionInfo {
            session: Session::Rth,
            start: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            end: chrono::DateTime::from_timestamp(20, 0).unwrap(),
        };
        let input = Address::from([0x11; 20]);
        let output = Address::from([0x22; 20]);
        let first_quote = test_snapshot(10_000);
        build_response_from_quote_pair_bound(
            &state,
            &pair(),
            &first_quote,
            input,
            output,
            &session,
            PairSchema::V5,
        )
        .await
        .unwrap();

        let mut next_quote = test_quote(20_000);
        next_quote.source_ts_unix_ms += 1_000;
        let next_quote = first_quote.in_same_generation(next_quote);
        let error = build_response_from_quote_pair_bound(
            &state,
            &pair(),
            &next_quote,
            input,
            output,
            &session,
            PairSchema::V5,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            AppError::Unavailable {
                reason: UnavailableReason::ExpiredQuote,
                ..
            }
        ));
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
                AppError::Unavailable {
                    reason: UnavailableReason::NoLiveQuote,
                    detail: "later".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
                "no_live_quote",
                "later",
            ),
            (
                AppError::Unavailable {
                    reason: UnavailableReason::ExpiredQuote,
                    detail: "stale".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
                "expired_quote",
                "stale",
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
        AppError::Unavailable {
            reason: UnavailableReason::NoLiveQuote,
            detail: "x".into(),
        }
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
                { "status": "error", "body": { "error": "no_live_quote", "detail": "x" } },
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
        assert_eq!(counts.get("no_live_quote"), Some(&2), "{counts:?}");
        assert_eq!(counts.get("bad_request"), Some(&1), "{counts:?}");
        assert_eq!(counts.get("internal_error"), Some(&1), "{counts:?}");
        assert_eq!(counts.len(), 4, "no other outcome labels: {counts:?}");
    }

    /// `finish` is the only place the mode is applied: strict collapses
    /// to the first error, envelope keeps every slot in order.
    #[test]
    fn finish_applies_mode() {
        let strict = finish("t", vec![Ok(ok()), Err(err()), Ok(ok())], false);
        assert!(matches!(strict, Err(AppError::Unavailable { .. })));

        let strict_ok = finish("t", vec![Ok(ok()), Ok(ok())], false).unwrap();
        assert!(matches!(strict_ok, ContextResponse::Strict(v) if v.len() == 2));

        let env = finish("t", vec![Ok(ok()), Err(err()), Ok(ok())], true).unwrap();
        let ContextResponse::Envelope(items) = env else {
            panic!("expected envelope");
        };
        assert_eq!(items.len(), 3);
        assert!(matches!(items[0], oracle::BatchItemResponse::Ok(_)));
        assert!(
            matches!(&items[1], oracle::BatchItemResponse::Error(e) if e.error == "no_live_quote")
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
        for raw in ["%%%", "&&&", "===", "=&=&", "a=b=c", "%%%&=="] {
            assert!(!allow(Some(raw)), "{raw:?} must read as false");
        }
        // Empty pairs around a well-formed flag are harmless.
        assert!(allow(Some("&allowFailure=true&")));
    }
}
