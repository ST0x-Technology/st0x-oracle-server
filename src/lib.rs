pub mod alpaca;
pub mod cache;
pub mod config;
pub mod market_hours;
pub mod oracle;
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

use crate::cache::QuoteCache;
use crate::market_hours::MarketHoursCache;
use crate::registry::{ResolvedPair, TokenRegistry};
use chrono::Utc;

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
    cache: Arc<QuoteCache>,
    /// Every symbol declared in config.toml. /status compares this
    /// against the cache to surface the partial-serving set.
    configured_symbols: Vec<String>,
    /// Authoritative market-hours source from Alpaca's calendar. Feeds
    /// the v2/v3/v4 session slots (tag + start/end bounds) here, and — in
    /// the poll loop — each mark's as-of `publish_time` (fetch instant
    /// in-session, last `session_close` out-of-session). The sign path
    /// itself just signs `QuoteData.t` straight through.
    market_hours: Arc<MarketHoursCache>,
}

impl AppState {
    pub fn new(
        signer: Signer,
        registry: TokenRegistry,
        cache: Arc<QuoteCache>,
        configured_symbols: Vec<String>,
        market_hours: Arc<MarketHoursCache>,
    ) -> Self {
        Self {
            signer,
            registry,
            cache,
            configured_symbols,
            market_hours,
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
        .route("/context/v1", post(post_signed_context_v1))
        .route("/context/v2", post(post_signed_context_v2))
        .route("/context/v3", post(post_signed_context_v3))
        .route("/context/v4", post(post_signed_context_v4))
        .layer(CorsLayer::permissive())
        .with_state(shared_state)
}

async fn health() -> &'static str {
    "ok"
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
    let missing = state.cache.missing(&state.configured_symbols).await;
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
    let snapshot = state.cache.snapshot_many(&needed_symbols).await;

    let mut responses = Vec::with_capacity(resolved.len());
    for (_, pair) in &resolved {
        let quote = snapshot.get(&pair.symbol).cloned().ok_or_else(|| {
            AppError::Unavailable(format!(
                "No cached quote for {} yet. The poll loop has not succeeded since startup.",
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
        price: f64,
        publish_time: u64,
        session_bytes: [u8; 32],
        session_start: u64,
        session_end: u64,
        inverted: bool,
    ) -> Result<Vec<alloy::primitives::FixedBytes<32>>, anyhow::Error> {
        match self {
            Self::V2 => oracle::build_context_v2(
                price,
                publish_time,
                session_bytes,
                session_start,
                session_end,
                inverted,
            ),
            Self::V3 => oracle::build_context_v3(
                price,
                publish_time,
                session_bytes,
                session_start,
                session_end,
                inverted,
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
    post_signed_context_session(state, body, SessionSchema::V2).await
}

/// v3 handler — `/context/v3` endpoint. Same request shape and
/// snapshot-once batching as v2; the only difference from the
/// caller's perspective is slot 3 carries V3 IntOrAString (matches
/// Rainlang `"…"` literals) and slot 0 carries schema version 3.
async fn post_signed_context_v3(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    post_signed_context_session(state, body, SessionSchema::V3).await
}

/// v4 handler — `/context/v4` endpoint. Same request shape and
/// snapshot-once batching as v2/v3, plus the caller's raw
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
    let requests = decode_request_body(&body)?;

    if requests.is_empty() {
        return Ok(Json(Vec::<oracle::OracleResponse>::new()));
    }

    // Same resolution + batching shape as v2/v3, but also keep the raw
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
            inverted = pair.inverted,
            input = %input_token,
            output = %output_token,
            schema = "v4",
            "Oracle request"
        );
        resolved.push((input_token, output_token, pair));
    }

    let needed_symbols: Vec<&str> = resolved.iter().map(|(_, _, p)| p.symbol.as_str()).collect();
    let snapshot = state.cache.snapshot_many(&needed_symbols).await;

    // Session classification is snapshot once per batch so every signed
    // context agrees on the session even across a phase boundary
    // mid-iteration. `publish_time`, by contrast, is per-quote — it's
    // the mark's own fetch time (`quote.t`), read inside each builder.
    let session_info = state.market_hours.session_info_for(Utc::now()).await;

    let mut responses = Vec::with_capacity(resolved.len());
    for (input_token, output_token, pair) in &resolved {
        let quote = snapshot.get(&pair.symbol).cloned().ok_or_else(|| {
            AppError::Unavailable(format!(
                "No cached quote for {} yet. The poll loop has not succeeded since startup.",
                pair.symbol
            ))
        })?;
        let resp = build_response_from_quote_v4(
            &state,
            pair,
            &quote,
            *input_token,
            *output_token,
            &session_info,
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
    let snapshot = state.cache.snapshot_many(&needed_symbols).await;

    // Snapshot the session classification once for the whole batch so
    // every signed context in this response agrees on which session
    // we're in, even across a phase boundary mid-iteration. `publish_time`
    // is per-quote (the mark's own fetch time), read inside the builder.
    let session_info = state.market_hours.session_info_for(Utc::now()).await;

    let mut responses = Vec::with_capacity(resolved.len());
    for (_, pair) in &resolved {
        let quote = snapshot.get(&pair.symbol).cloned().ok_or_else(|| {
            AppError::Unavailable(format!(
                "No cached quote for {} yet. The poll loop has not succeeded since startup.",
                pair.symbol
            ))
        })?;
        let resp =
            build_response_from_quote_session(&state, pair, &quote, &session_info, schema).await?;
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
        inverted = pair.inverted,
        input = %input_token,
        output = %output_token,
        "Oracle request"
    );

    Ok(pair)
}

/// Build a signed response from a pre-resolved pair and a snapshotted
/// mark. All marks for a single batch must come from one snapshot so
/// concurrent poller updates can't mix prices/publish_times across
/// elements of the same response.
///
/// The broker mark is a single fair-value number per symbol, so buy and
/// sell directions both use it; `build_context` inverts via Rain Float
/// when `pair.inverted` is true.
///
/// `publish_time` is `QuoteData.t` — the mark's as-of timestamp, computed
/// by the poll loop when it fetched this price (fetch instant in-session,
/// last `session_close` out-of-session; see `MarketHoursCache::publish_time_for`).
/// The sign path signs it straight through, NOT the moment a consumer hit
/// `/context`. Signing the request instant would make a stalled poll loop
/// invisible — it would keep stamping a fresh `now` onto an increasingly
/// stale cached mark, and the strategy's `max-staleness` (which clocks off
/// this timestamp) could never catch it. Signing the fetch-time as-of
/// means a frozen poll surfaces directly: `t` stops advancing, the
/// timestamp ages out, the strategy rejects.
async fn build_response_from_quote(
    state: &AppState,
    pair: &ResolvedPair,
    quote: &crate::alpaca::QuoteData,
) -> Result<oracle::OracleResponse, AppError> {
    // The fetch path already drops non-positive marks, so this is a
    // belt-and-braces guard for any future code path that bypasses it.
    if quote.price <= 0.0 {
        return Err(AppError::BadRequest(format!(
            "Zero or negative broker mark for {} (price={}). Market may be closed or data is bad.",
            pair.symbol, quote.price
        )));
    }

    let publish_time: u64 = quote
        .t
        .timestamp()
        .try_into()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("publish_time out of range")))?;

    tracing::info!(
        symbol = %pair.symbol,
        price = quote.price,
        inverted = pair.inverted,
        publish_time = publish_time,
        "Building signed context from cache"
    );

    let context = oracle::build_context(quote.price, publish_time, pair.inverted)?;
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
    quote: &crate::alpaca::QuoteData,
    session_info: &crate::market_hours::SessionInfo,
    schema: SessionSchema,
) -> Result<oracle::OracleResponse, AppError> {
    if quote.price <= 0.0 {
        return Err(AppError::BadRequest(format!(
            "Zero or negative broker mark for {} (price={}). Market may be closed or data is bad.",
            pair.symbol, quote.price
        )));
    }

    // publish_time is the mark's own fetch time (see
    // `build_response_from_quote` for why we sign the fetch instant
    // rather than the request-handling instant).
    let publish_time: u64 = quote
        .t
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

    tracing::info!(
        symbol = %pair.symbol,
        price = quote.price,
        inverted = pair.inverted,
        schema = schema.tag(),
        publish_time = publish_time,
        session = session_info.session.as_str(),
        session_start = session_start,
        session_end = session_end,
        "Building session signed context from cache"
    );

    let context = schema.build_context(
        quote.price,
        publish_time,
        schema.encode_session(session_info.session),
        session_start,
        session_end,
        pair.inverted,
    )?;
    let (signature, signer) = state.signer.sign_context(&context).await?;

    Ok(oracle::OracleResponse {
        signer,
        context,
        signature,
    })
}

/// v4 response builder. Same price + publish_time + session-bounds logic
/// as v3's `build_response_from_quote_session`, plus the caller's raw
/// input/output token addresses stamped into signed-context slots 6 and
/// 7 (see `oracle::build_context_v4` for the layout).
async fn build_response_from_quote_v4(
    state: &AppState,
    pair: &ResolvedPair,
    quote: &crate::alpaca::QuoteData,
    input_token: Address,
    output_token: Address,
    session_info: &crate::market_hours::SessionInfo,
) -> Result<oracle::OracleResponse, AppError> {
    if quote.price <= 0.0 {
        return Err(AppError::BadRequest(format!(
            "Zero or negative broker mark for {} (price={}). Market may be closed or data is bad.",
            pair.symbol, quote.price
        )));
    }

    // publish_time is the mark's own fetch time (see
    // `build_response_from_quote` for why we sign the fetch instant
    // rather than the request-handling instant).
    let publish_time: u64 = quote
        .t
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

    tracing::info!(
        symbol = %pair.symbol,
        price = quote.price,
        inverted = pair.inverted,
        schema = "v4",
        input = %input_token,
        output = %output_token,
        publish_time = publish_time,
        session = session_info.session.as_str(),
        session_start = session_start,
        session_end = session_end,
        "Building v4 signed context from cache"
    );

    let context = oracle::build_context_v4(
        quote.price,
        publish_time,
        session_info.session.to_bytes32_v3(),
        session_start,
        session_end,
        input_token,
        output_token,
        pair.inverted,
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
