pub mod alpaca;
pub mod cache;
pub mod config;
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
use crate::registry::{ResolvedPair, TokenRegistry};

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
}

impl AppState {
    pub fn new(
        signer: Signer,
        registry: TokenRegistry,
        cache: Arc<QuoteCache>,
        configured_symbols: Vec<String>,
    ) -> Self {
        Self {
            signer,
            registry,
            cache,
            configured_symbols,
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
