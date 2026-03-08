pub mod alpaca;
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

use crate::alpaca::AlpacaClient;
use crate::registry::TokenRegistry;

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

type OracleRequestBody = (OrderV4, alloy::primitives::U256, alloy::primitives::U256, Address);

pub struct AppState {
    signer: Signer,
    alpaca: AlpacaClient,
    registry: TokenRegistry,
    expiry_seconds: u64,
}

impl AppState {
    pub fn new(
        signer: Signer,
        alpaca: AlpacaClient,
        registry: TokenRegistry,
        expiry_seconds: u64,
    ) -> Self {
        Self {
            signer,
            alpaca,
            registry,
            expiry_seconds,
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
        .route("/context", post(post_signed_context))
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

async fn post_signed_context(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    // Decode ABI-encoded request
    let (order, input_io_index, output_io_index, _counterparty) =
        <OracleRequestBody>::abi_decode(&body)
            .map_err(|e| AppError::BadRequest(format!("Invalid ABI-encoded body: {}", e)))?;

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

    // Resolve token pair to Alpaca symbol + direction
    let pair = state
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

    // Fetch NBBO quote from Alpaca
    let quote = state.alpaca.latest_quote(&pair.symbol).await?;

    // Use the executable price:
    // - Buy tStock (not inverted): taker pays ask price
    // - Sell tStock (inverted): taker receives bid price (then we invert)
    let price = if pair.inverted {
        quote.bid_price
    } else {
        quote.ask_price
    };

    tracing::info!(
        symbol = %pair.symbol,
        bid = quote.bid_price,
        ask = quote.ask_price,
        selected_price = price,
        inverted = pair.inverted,
        "Alpaca NBBO"
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expiry = now + state.expiry_seconds;

    let context = oracle::build_context(price, expiry, pair.inverted)?;
    let (signature, signer) = state.signer.sign_context(&context).await?;

    let response = oracle::OracleResponse {
        signer,
        context,
        signature,
    };

    Ok(Json(response))
}

pub enum AppError {
    Internal(anyhow::Error),
    BadRequest(String),
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
        }
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err)
    }
}
