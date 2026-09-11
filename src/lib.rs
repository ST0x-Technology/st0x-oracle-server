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
/// We decode either. The response is always a JSON array of
/// `OracleResponse` whose length matches the number of requests.
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
) -> Result<impl IntoResponse, AppError> {
    let query = ContextQuery::parse(query.as_deref());
    let result = post_signed_context_v1_inner(state, query, body).await;
    record_request_outcome("v1", &result);
    result
}

async fn post_signed_context_v1_inner(
    state: Arc<AppState>,
    query: ContextQuery,
    body: Bytes,
) -> Result<axum::Json<Vec<oracle::OracleResponse>>, AppError> {
    let decoded = decode_request_body(&body)?;
    // Envelope mode is only reachable for the array form. The per-item
    // pipeline (plan steps 4/6) consumes this; until then the flag is
    // parsed and decided here but does not change the response.
    let _envelope = query.allow_failure && decoded.is_batch;
    let requests = decoded.items;

    if requests.is_empty() {
        return Ok(Json(Vec::<oracle::OracleResponse>::new()));
    }

    // Resolve every request's token pair first so we know which symbols
    // we need from the cache. This lets us take a single snapshot of
    // exactly those entries, so a poll loop update mid-iteration can't
    // mix quotes (or publish_time values) for the same symbol within
    // one HTTP response.
    let mut resolved: Vec<(OrderV4, ResolvedPair)> = Vec::with_capacity(requests.len());
    for (order, input_io_index, output_io_index, _counterparty) in requests {
        let pair = resolve_pair_for_order(&state, &order, input_io_index, output_io_index)?;
        resolved.push((order, pair));
    }

    let needed_symbols: Vec<&str> = resolved.iter().map(|(_, p)| p.symbol.as_str()).collect();
    let snapshot = state.pricing.snapshot_many(&needed_symbols).await;

    let mut responses = Vec::with_capacity(resolved.len());
    for (_, pair) in &resolved {
        let quote = snapshot.get(&pair.symbol).cloned().ok_or_else(|| {
            AppError::Unavailable(format!(
                "No live quote for {} yet. The pricing WS has not delivered a frame since startup.",
                pair.symbol
            ))
        })?;
        let resp = build_response_from_quote(&state, pair, &quote).await?;
        responses.push(resp);
    }

    Ok(Json(responses))
}

/// Record a `/context/v{N}` request's outcome on the `oracle_context_request_total`
/// counter. `outcome` labels split into `ok` (signed responses returned),
/// `empty` (no requests in the body — Raindex's quote crate posts an empty
/// batch when an order's IO list is empty), and `error` (any `AppError`).
/// Keep the labels stable — the obs dashboard joins on these.
fn record_request_outcome(
    endpoint: &'static str,
    result: &Result<axum::Json<Vec<oracle::OracleResponse>>, AppError>,
) {
    let outcome = match result {
        Ok(json) if json.0.is_empty() => "empty",
        Ok(_) => "ok",
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
) -> Result<impl IntoResponse, AppError> {
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
) -> Result<impl IntoResponse, AppError> {
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
) -> Result<impl IntoResponse, AppError> {
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
) -> Result<impl IntoResponse, AppError> {
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
) -> Result<axum::Json<Vec<oracle::OracleResponse>>, AppError> {
    let decoded = decode_request_body(&body)?;
    // Envelope mode is only reachable for the array form. The per-item
    // pipeline (plan steps 5/6) consumes this; until then the flag is
    // parsed and decided here but does not change the response.
    let _envelope = query.allow_failure && decoded.is_batch;
    let requests = decoded.items;

    if requests.is_empty() {
        return Ok(Json(Vec::<oracle::OracleResponse>::new()));
    }

    // Same resolution + batching shape as v1, but also keep the raw
    // input_token/output_token per request so we can bind them into the
    // signed context — that binding is the whole point of v4.
    let mut resolved: Vec<(Address, Address, ResolvedPair)> = Vec::with_capacity(requests.len());
    for (order, input_io_index, output_io_index, _counterparty) in requests {
        let (input_token, output_token) = io_tokens_for(&order, input_io_index, output_io_index)?;
        let pair = state
            .registry
            .resolve(input_token, output_token)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        tracing::info!(
            symbol = %pair.symbol,
            direction = pair.direction.as_str(),
            input = %input_token,
            output = %output_token,
            schema = schema.tag(),
            "Oracle request"
        );
        resolved.push((input_token, output_token, pair));
    }

    let needed_symbols: Vec<&str> = resolved.iter().map(|(_, _, p)| p.symbol.as_str()).collect();
    let snapshot = state.pricing.snapshot_many(&needed_symbols).await;

    // Session classification is snapshot once per batch; publish_time is
    // per-quote (the pricing quote's own source_ts), read inside the builder.
    let session_info = state.market_hours.session_info_for(Utc::now()).await;

    let mut responses = Vec::with_capacity(resolved.len());
    for (input_token, output_token, pair) in &resolved {
        let quote = snapshot.get(&pair.symbol).cloned().ok_or_else(|| {
            AppError::Unavailable(format!(
                "No live quote for {} yet. The pricing WS has not delivered a frame since startup.",
                pair.symbol
            ))
        })?;
        let resp = build_response_from_quote_pair_bound(
            &state,
            pair,
            &quote,
            *input_token,
            *output_token,
            &session_info,
            schema,
        )
        .await?;
        responses.push(resp);
    }

    Ok(Json(responses))
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

    /// The JSON body for this error. Used both as the body of a non-2xx
    /// response and as the per-item `body` of a batch envelope error, so
    /// the two paths can never disagree on the `error` code strings.
    pub fn to_error_response(&self) -> ErrorResponse {
        match self {
            AppError::Internal(err) => ErrorResponse {
                error: "internal_error".to_string(),
                detail: format!("{}", err),
            },
            AppError::BadRequest(detail) => ErrorResponse {
                error: "bad_request".to_string(),
                detail: detail.clone(),
            },
            AppError::Unavailable(detail) => ErrorResponse {
                error: "service_unavailable".to_string(),
                detail: detail.clone(),
            },
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
