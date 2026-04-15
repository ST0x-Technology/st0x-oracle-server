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
use thiserror::Error;
use tower_http::cors::CorsLayer;

use crate::{
    cache::QuoteCache,
    oracle::{OracleErrResponse, OracleOkResponse, OracleResult},
};
use crate::{
    oracle::OracleResponse,
    registry::{ResolvedPair, TokenRegistry},
};

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
}

impl AppState {
    pub fn new(signer: Signer, registry: TokenRegistry, cache: Arc<QuoteCache>) -> Self {
        Self {
            signer,
            registry,
            cache,
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
        .route("/context/v1", post(post_signed_context_v1))
        .layer(CorsLayer::permissive())
        .with_state(shared_state)
}

async fn health() -> &'static str {
    "ok"
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
        return Ok(Json(OracleResponse::new()));
    }

    // Resolve every request's token pair first so we know which symbols
    // we need from the cache. This lets us take a single snapshot of
    // exactly those entries, so a poll loop update mid-iteration can't
    // mix quotes (or publish_time values) for the same symbol within
    // one HTTP response.
    let mut resolved: Vec<(OrderV4, Result<ResolvedPair, OracleErrResponse>)> =
        Vec::with_capacity(requests.len());
    for (order, input_io_index, output_io_index, _counterparty) in requests {
        let res = resolve_pair_for_order(&state, &order, input_io_index, output_io_index);
        resolved.push((order, res));
    }

    let needed_symbols: Vec<&str> = resolved
        .iter()
        .filter_map(|(_, res)| res.as_ref().ok().and_then(|p| Some(p.symbol.as_str())))
        .collect();
    let snapshot = state.cache.snapshot_many(&needed_symbols).await;

    let mut responses: OracleResponse = Vec::with_capacity(resolved.len());
    for (_, res) in resolved {
        let resp = match res {
            Ok(pair) => {
                match snapshot.get(&pair.symbol).cloned() {
                    Some(quote) => build_response_from_quote(&state, &pair, &quote).await,
                    None => OracleResult::Err(AppError::Unavailable(format!(
                        "No cached quote for {} yet. The poll loop has not succeeded since startup.",
                        pair.symbol
                    )).into())
                }
            }
            Err(e) => OracleResult::Err(e),
        };
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
) -> Result<ResolvedPair, OracleErrResponse> {
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
/// quote. All quotes for a single batch must come from one snapshot so
/// concurrent poller updates can't mix prices/publish_times across
/// elements of the same response.
async fn build_response_from_quote(
    state: &AppState,
    pair: &ResolvedPair,
    quote: &crate::alpaca::QuoteData,
) -> OracleResult {
    // Use a single price for both directions. The bid is the most
    // reliably populated side of the NBBO — the ask is often zero on
    // free-tier Alpaca data outside regular hours. build_context()
    // handles inversion in Rain Float precision when needed, so we
    // always pass the same underlying price regardless of direction.
    let raw_price = quote.bid_price;

    if raw_price <= 0.0 {
        return OracleResult::Err(
            AppError::BadRequest(format!(
            "Zero or negative price for {} (bid={}, ask={}). Market may be closed or data is bad.",
            pair.symbol, quote.bid_price, quote.ask_price
        ))
            .into(),
        );
    }

    let publish_time: u64 = match quote.t.timestamp().try_into() {
        Ok(v) => v,
        Err(_) => {
            return OracleResult::Err(
                AppError::Internal(anyhow::anyhow!("publish_time out of range")).into(),
            )
        }
    };

    tracing::info!(
        symbol = %pair.symbol,
        bid = quote.bid_price,
        ask = quote.ask_price,
        raw_price = raw_price,
        inverted = pair.inverted,
        publish_time = publish_time,
        "Building signed context from cache"
    );

    match oracle::build_context(raw_price, publish_time, pair.inverted) {
        Err(e) => OracleResult::Err(OracleErrResponse { msg: e.to_string() }),
        Ok(context) => state.signer.sign_context(&context).await.map_or_else(
            |err| {
                OracleResult::Err(OracleErrResponse {
                    msg: err.to_string(),
                })
            },
            |(signature, signer)| {
                OracleResult::Ok(OracleOkResponse {
                    signer,
                    context,
                    signature,
                })
            },
        ),
    }
}
#[derive(Error, Debug)]
pub enum AppError {
    #[error("Internal error: {0}")]
    Internal(anyhow::Error),
    #[error("Bad request: {0}")]
    BadRequest(String),
    /// The server is alive but the poll loop hasn't produced a quote yet
    /// for this symbol. Distinct from BadRequest because it's transient
    /// and retrying may succeed.
    #[error("Service unavailable: {0}")]
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
