pub mod alpaca;
pub mod config;
pub mod market_hours;
pub mod metrics;
pub mod oracle;
pub mod pricing_client;
pub mod registry;
pub mod sign;

use alloy::primitives::Address;
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
    /// Authoritative market-hours source from Alpaca's calendar.
    /// Determines whether `publish_time` is `now` (inside an active
    /// session) or the most recent `session_close` (outside).
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
        .route("/context/v2", post(post_signed_context_v2))
        .route("/context/v3", post(post_signed_context_v3))
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

/// Pick which signed-context shape an endpoint emits. The two
/// shapes share everything except the schema-version constant in
/// slot 0 and the IntOrAString layout in slot 3:
///
/// - `V2` → `SCHEMA_VERSION_V2 = 2` + `Session::to_bytes32_v1`
///   (byte-0 length). What live v2 strategies are bound to.
/// - `V3` → `SCHEMA_VERSION_V3 = 3` + `Session::to_bytes32_v3`
///   (byte-31 length). Matches Rainlang `"…"` string literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionSchema {
    V2,
    V3,
}

impl SessionSchema {
    fn encode_session(self, session: crate::market_hours::Session) -> [u8; 32] {
        match self {
            Self::V2 => session.to_bytes32_v1(),
            Self::V3 => session.to_bytes32_v3(),
        }
    }

    fn build_context(
        self,
        price_bytes: [u8; 32],
        publish_time: u64,
        session_bytes: [u8; 32],
        session_start: u64,
        session_end: u64,
    ) -> Result<Vec<alloy::primitives::FixedBytes<32>>, anyhow::Error> {
        match self {
            Self::V2 => oracle::build_context_v2(
                price_bytes,
                publish_time,
                session_bytes,
                session_start,
                session_end,
            ),
            Self::V3 => oracle::build_context_v3(
                price_bytes,
                publish_time,
                session_bytes,
                session_start,
                session_end,
            ),
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::V2 => "v2",
            Self::V3 => "v3",
        }
    }
}

/// v2 handler — `/context/v2` endpoint. See [`SessionSchema`] for
/// what makes v2 different from v3.
async fn post_signed_context_v2(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let result = post_signed_context_session(state, body, SessionSchema::V2).await;
    record_request_outcome("v2", &result);
    result
}

/// v3 handler — `/context/v3` endpoint. Same request shape and
/// snapshot-once batching as v2; the only difference from the
/// caller's perspective is slot 3 carries V3 IntOrAString (matches
/// Rainlang `"…"` literals) and slot 0 carries schema version 3.
async fn post_signed_context_v3(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let result = post_signed_context_session(state, body, SessionSchema::V3).await;
    record_request_outcome("v3", &result);
    result
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

/// Shared body for `/context/v2` and `/context/v3`. Same orderbook
/// request decoding, same cache snapshot-once batching, same
/// market-hours snapshot-once-per-batch. The `schema` arg picks
/// which schema-version constant and IntOrAString layout we emit.
async fn post_signed_context_session(
    state: Arc<AppState>,
    body: Bytes,
    schema: SessionSchema,
) -> Result<axum::Json<Vec<oracle::OracleResponse>>, AppError> {
    let requests = decode_request_body(&body)?;

    if requests.is_empty() {
        return Ok(Json(Vec::<oracle::OracleResponse>::new()));
    }

    let mut resolved: Vec<(OrderV4, ResolvedPair)> = Vec::with_capacity(requests.len());
    for (order, input_io_index, output_io_index, _counterparty) in requests {
        let pair = resolve_pair_for_order(&state, &order, input_io_index, output_io_index)?;
        resolved.push((order, pair));
    }

    let needed_symbols: Vec<&str> = resolved.iter().map(|(_, p)| p.symbol.as_str()).collect();
    let snapshot = state.pricing.snapshot_many(&needed_symbols).await;

    // Snapshot the session classification once for the whole batch so
    // every signed context in this response agrees on which session
    // we're in, even across a phase boundary mid-iteration.
    let now = Utc::now();
    let publish_dt = state.market_hours.publish_time_for(now).await;
    let session_info = state.market_hours.session_info_for(now).await;

    let mut responses = Vec::with_capacity(resolved.len());
    for (_, pair) in &resolved {
        let quote = snapshot.get(&pair.symbol).cloned().ok_or_else(|| {
            AppError::Unavailable(format!(
                "No live quote for {} yet. The pricing WS has not delivered a frame since startup.",
                pair.symbol
            ))
        })?;
        let resp = build_response_from_quote_session(
            &state,
            pair,
            &quote,
            publish_dt,
            &session_info,
            schema,
        )
        .await?;
        responses.push(resp);
    }

    Ok(Json(responses))
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

/// Pick the rate slot from a live pricing-service `Quote` that matches
/// this request's swap direction. The pricing service emits both rates
/// independently (each already incorporating the model's per-direction
/// spread); the oracle never derives one from the other.
///
/// Raindex's `ratio` for an order is `input_amount / output_amount`
/// (units of inputToken received per outputToken paid). Pricing-service
/// rate naming is "Y per X" — `rate_base_to_quote` is *quote per base*
/// and `rate_quote_to_base` is *base per quote*. So a `QuoteToBase`
/// order (input=quote, output=base) needs `quote / base = rate_base_to_quote`,
/// and a `BaseToQuote` order (input=base, output=quote) needs
/// `base / quote = rate_quote_to_base`. Names look reversed at first
/// glance — they refer to which side of the *order* is which, not which
/// conversion direction.
///
/// Mismatching these silently flips the price by ~4 orders of magnitude;
/// the parity-window diff observer caught this against the legacy Fly
/// oracle on first probe (RAI-361).
fn pick_rate_bytes(quote: &Quote, direction: PriceDirection) -> [u8; 32] {
    match direction {
        PriceDirection::QuoteToBase => quote.rate_base_to_quote.0,
        PriceDirection::BaseToQuote => quote.rate_quote_to_base.0,
    }
}

/// Build a signed response from a pre-resolved pair and a snapshotted
/// `Quote`. All `Quote`s for one batch must come from a single
/// `LiveClient::snapshot_many` so a concurrent WS push can't mix prices
/// across elements of the same response.
///
/// The pricing service publishes both swap directions independently,
/// already including its spread; the oracle just picks the rate that
/// matches the request's direction and signs the 32-byte Rain Float
/// straight through — no inversion, no f64 round-trip, no extra spread.
///
/// `publish_time` is chosen by `MarketHoursCache`: inside an active
/// extended session window we sign `now`; outside we sign the most
/// recent `session_close`. This is RAI-693: pricing-service quotes are
/// pushed continuously even when the underlying market is closed, so a
/// naive `now` would label an off-hours quote with a fresh timestamp.
async fn build_response_from_quote(
    state: &AppState,
    pair: &ResolvedPair,
    quote: &Quote,
) -> Result<oracle::OracleResponse, AppError> {
    let now = Utc::now();
    let publish_dt = state.market_hours.publish_time_for(now).await;
    let publish_time: u64 = publish_dt
        .timestamp()
        .try_into()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("publish_time out of range")))?;

    let price_bytes = pick_rate_bytes(quote, pair.direction);

    tracing::info!(
        symbol = %pair.symbol,
        direction = pair.direction.as_str(),
        publish_time = publish_time,
        in_session = (publish_dt == now),
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

/// Shared response builder for `/context/v2` and `/context/v3`.
/// Same price + publish_time + session-bounds logic as the v2-only
/// helper this replaces; the `schema` arg picks which IntOrAString
/// layout slot 3 uses and which schema-version constant slot 0
/// carries.
async fn build_response_from_quote_session(
    state: &AppState,
    pair: &ResolvedPair,
    quote: &Quote,
    publish_dt: chrono::DateTime<Utc>,
    session_info: &crate::market_hours::SessionInfo,
    schema: SessionSchema,
) -> Result<oracle::OracleResponse, AppError> {
    let publish_time: u64 = publish_dt
        .timestamp()
        .try_into()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("publish_time out of range")))?;
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

    let price_bytes = pick_rate_bytes(quote, pair.direction);

    tracing::info!(
        symbol = %pair.symbol,
        direction = pair.direction.as_str(),
        schema = schema.tag(),
        publish_time = publish_time,
        session = session_info.session.as_str(),
        session_start = session_start,
        session_end = session_end,
        source_ts_unix_ms = quote.source_ts_unix_ms,
        "Building session signed context from live pricing quote"
    );

    let context = schema.build_context(
        price_bytes,
        publish_time,
        schema.encode_session(session_info.session),
        session_start,
        session_end,
    )?;
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
