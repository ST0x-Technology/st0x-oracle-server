use alloy::primitives::{Address, FixedBytes, U256};
use alloy::sol_types::SolValue;
use axum::body::Bytes;
use http_body_util::BodyExt;
use st0x_oracle_server::alpaca::AlpacaClient;
use st0x_oracle_server::registry::TokenRegistry;
use st0x_oracle_server::sign::Signer;
use st0x_oracle_server::{create_app, AppState, EvaluableV4, OrderV4, IOV2};
use std::str::FromStr;
use tower::ServiceExt;

const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

// Token addresses for testing
const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
const WCOIN: &str = "0x1111111111111111111111111111111111111111";

fn test_order(input_token: &str, output_token: &str) -> Bytes {
    let order = OrderV4 {
        owner: Address::ZERO,
        evaluable: EvaluableV4 {
            interpreter: Address::ZERO,
            store: Address::ZERO,
            bytecode: alloy::primitives::Bytes::new(),
        },
        validInputs: vec![IOV2 {
            token: Address::from_str(input_token).unwrap(),
            vaultId: FixedBytes::ZERO,
        }],
        validOutputs: vec![IOV2 {
            token: Address::from_str(output_token).unwrap(),
            vaultId: FixedBytes::ZERO,
        }],
        nonce: FixedBytes::ZERO,
    };

    let input_io_index = U256::from(0u64);
    let output_io_index = U256::from(0u64);
    let counterparty = Address::ZERO;

    let encoded = (order, input_io_index, output_io_index, counterparty).abi_encode();
    Bytes::from(encoded)
}

fn test_app() -> axum::Router {
    let signer = Signer::new(TEST_KEY).unwrap();
    // Use real Alpaca client — tests that hit this endpoint need
    // ALPACA_API_KEY_ID and ALPACA_API_SECRET_KEY env vars set.
    // For unit-level tests, we test components individually instead.
    let alpaca = AlpacaClient::new(
        &std::env::var("ALPACA_API_KEY_ID").unwrap_or_default(),
        &std::env::var("ALPACA_API_SECRET_KEY").unwrap_or_default(),
    );
    let registry = TokenRegistry::new(vec![(WCOIN.to_string(), "COIN".to_string())], USDC).unwrap();

    let state = AppState::new(signer, alpaca, registry, 30);
    create_app(state)
}

#[tokio::test]
async fn test_health_endpoint() {
    let app = test_app();

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn test_invalid_body_returns_400() {
    let app = test_app();

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(vec![0u8; 32]))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn test_unknown_token_returns_400() {
    let app = test_app();

    // Use an unknown token address
    let body = test_order(
        "0x9999999999999999999999999999999999999999",
        "0x8888888888888888888888888888888888888888",
    );

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
}

/// This test requires live Alpaca API credentials.
/// Set ALPACA_API_KEY_ID and ALPACA_API_SECRET_KEY env vars to run.
#[tokio::test]
async fn test_buy_tstock_returns_signed_context() {
    if std::env::var("ALPACA_API_KEY_ID").is_err() {
        eprintln!("Skipping live Alpaca test — ALPACA_API_KEY_ID not set");
        return;
    }

    let app = test_app();
    let body = test_order(USDC, WCOIN);

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // Should have signer, context (2 elements), and signature
    assert!(resp["signer"].is_string());
    assert_eq!(resp["context"].as_array().unwrap().len(), 2);
    assert!(resp["signature"].is_string());
}
