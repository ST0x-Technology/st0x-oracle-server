pub mod alpaca;
pub mod config;
pub mod market_hours;
pub mod metrics;
pub mod oracle;
pub mod pricing_client;
pub mod registry;
pub mod sign;

use alloy::primitives::{Address, B256};
use alloy::sol;
use alloy::sol_types::SolValue;
use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use rain_math_float::Float;
use serde::Serialize;
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
        }
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

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
    detail: String,
}

/// Decode the POST body as either a single tuple or a batch array.
/// Returns a `Vec` in either case so downstream logic is uniform.
///
/// We try the batch form first because the empty-batch case (`[]`) is
/// a valid input upstream — returning an empty response array preserves
/// the "response length matches request length" contract. A batch
/// containing one element will also decode correctly here. Only when
/// the batch decoder rejects the body do we fall back to the single
/// tuple form (which is what most current callers send).
fn decode_request_body(body: &[u8]) -> Result<Vec<OracleRequestTuple>, AppError> {
    if let Ok(batch) = <Vec<OracleRequestTuple>>::abi_decode(body) {
        return Ok(batch);
    }
    let single = <OracleRequestTuple>::abi_decode(body)
        .map_err(|e| AppError::BadRequest(format!("Invalid ABI-encoded body: {}", e)))?;
    Ok(vec![single])
}

async fn post_signed_context_v1(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let result = post_signed_context_v1_inner(state, body).await;
    record_request_outcome("v1", &result);
    result
}

async fn post_signed_context_v1_inner(
    state: Arc<AppState>,
    body: Bytes,
) -> Result<axum::Json<Vec<oracle::OracleResponse>>, AppError> {
    let requests = decode_request_body(&body)?;

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
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let result = post_signed_context_pair_bound(state, body, PairSchema::V4).await;
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
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let result = post_signed_context_pair_bound(state, body, PairSchema::V5).await;
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
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let result = post_signed_context_pair_bound(state, body, PairSchema::V6).await;
    record_request_outcome("v6", &result);
    result
}

/// Which pair-bound schema a `/context/v4`, `/context/v5` or
/// `/context/v6` request is being served under. All three share request
/// decoding, registry resolution, snapshot-once batching and the
/// session snapshot; they differ only in whether the model's expiry is
/// signed into slot 8 (v5, v6) and the vault NAV ratio into slot 9
/// (v6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairSchema {
    V4,
    V5,
    V6,
}

impl PairSchema {
    fn tag(self) -> &'static str {
        match self {
            Self::V4 => "v4",
            Self::V5 => "v5",
            Self::V6 => "v6",
        }
    }
}

/// Shared body for `/context/v4`, `/context/v5` and `/context/v6`.
async fn post_signed_context_pair_bound(
    state: Arc<AppState>,
    body: Bytes,
    schema: PairSchema,
) -> Result<axum::Json<Vec<oracle::OracleResponse>>, AppError> {
    let requests = decode_request_body(&body)?;

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

/// Pair-bound response builder (v4/v5/v6). Same price + publish_time
/// logic as v1's `build_response_from_quote`, plus the session slots
/// and the caller's raw input/output token addresses stamped into
/// signed-context slots 6 and 7; v5 adds the quote expiry at slot 8 and
/// v6 the vault NAV ratio at slot 9 (see the `oracle::build_context_v*`
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

    // Same as v1: pick the directional rate and invert it into
    // Raindex ratio units (Rain-Float precision) — see `pick_rate_bytes`.
    let price_bytes = pick_rate_bytes(quote, pair.direction).map_err(AppError::Internal)?;

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

    let context = match schema {
        PairSchema::V4 => oracle::build_context_v4(
            price_bytes,
            publish_time,
            session_info.session.to_bytes32_v3(),
            session_start,
            session_end,
            input_token,
            output_token,
        )?,
        PairSchema::V5 => oracle::build_context_v5(
            price_bytes,
            publish_time,
            session_info.session.to_bytes32_v3(),
            session_start,
            session_end,
            input_token,
            output_token,
            expiry_from_quote(quote)?,
        )?,
        // The NAV ratio is read off the SAME `quote` as the rate at
        // slot 1 — both came out of one `snapshot_many` entry, and the
        // pricing client only ever stores whole frames — so the signed
        // context can never pair a rate from one frame with a ratio
        // from another.
        PairSchema::V6 => oracle::build_context_v6(
            price_bytes,
            publish_time,
            session_info.session.to_bytes32_v3(),
            session_start,
            session_end,
            input_token,
            output_token,
            expiry_from_quote(quote)?,
            quote.nav_ratio.0,
        )?,
    };
    let (signature, signer) = state.signer.sign_context(&context).await?;

    Ok(oracle::OracleResponse {
        signer,
        context,
        signature,
    })
}

pub enum AppError {
    Internal(anyhow::Error),
    BadRequest(String),
    /// The server is alive but the poll loop hasn't produced a quote yet
    /// for this symbol. Distinct from BadRequest because it's transient
    /// and retrying may succeed.
    Unavailable(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        match self {
            AppError::Internal(err) => {
                tracing::error!("Internal error: {:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: "internal_error".to_string(),
                        detail: format!("{}", err),
                    }),
                )
                    .into_response()
            }
            AppError::BadRequest(detail) => {
                tracing::warn!("Bad request: {}", detail);
                (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "bad_request".to_string(),
                        detail,
                    }),
                )
                    .into_response()
            }
            AppError::Unavailable(detail) => {
                tracing::warn!("Service unavailable: {}", detail);
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ErrorResponse {
                        error: "service_unavailable".to_string(),
                        detail,
                    }),
                )
                    .into_response()
            }
        }
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err)
    }
}
