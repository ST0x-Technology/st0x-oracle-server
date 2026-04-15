use alloy::primitives::{Address, FixedBytes, U256};
use alloy::sol_types::SolValue;
use axum::body::Bytes;
use chrono::{TimeZone, Utc};
use http_body_util::BodyExt;
use rain_math_float::Float;
use st0x_oracle_server::alpaca::QuoteData;
use st0x_oracle_server::cache::QuoteCache;
use st0x_oracle_server::oracle::{OracleResponse, OracleResult, SCHEMA_VERSION};
use st0x_oracle_server::registry::TokenRegistry;
use st0x_oracle_server::sign::Signer;
use st0x_oracle_server::{create_app, AppState, EvaluableV4, OrderV4, IOV2};
use std::str::FromStr;
use std::sync::Arc;
use tower::ServiceExt;

const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

// Token addresses for testing
const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
const WCOIN: &str = "0x1111111111111111111111111111111111111111";
const UNKNOWN_TOKEN: &str = "0x2222222222222222222222222222222222222222";

const FIXED_PUBLISH_TIME: i64 = 1_800_000_000;

fn test_order_tuple(input_token: &str, output_token: &str) -> (OrderV4, U256, U256, Address) {
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

    (order, U256::from(0u64), U256::from(0u64), Address::ZERO)
}

fn encode_single(input_token: &str, output_token: &str) -> Bytes {
    let tuple = test_order_tuple(input_token, output_token);
    Bytes::from(tuple.abi_encode())
}

fn encode_batch(pairs: &[(&str, &str)]) -> Bytes {
    let tuples: Vec<(OrderV4, U256, U256, Address)> =
        pairs.iter().map(|(i, o)| test_order_tuple(i, o)).collect();
    Bytes::from(tuples.abi_encode())
}

/// Build a test app with a pre-populated cache so tests don't need to
/// hit Alpaca. The cache contains a fixed quote for COIN with a known
/// timestamp we can assert against.
async fn test_app() -> axum::Router {
    let signer = Signer::new(TEST_KEY).unwrap();
    let registry = TokenRegistry::new(vec![(WCOIN.to_string(), "COIN".to_string())], USDC).unwrap();

    let cache = Arc::new(QuoteCache::new());
    cache
        .update(
            "COIN",
            QuoteData {
                bid_price: 100.0,
                ask_price: 101.0,
                t: Utc.timestamp_opt(FIXED_PUBLISH_TIME, 0).unwrap(),
            },
        )
        .await;

    let state = AppState::new(signer, registry, cache);
    create_app(state)
}

#[tokio::test]
async fn test_health_endpoint() {
    let app = test_app().await;

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
async fn test_old_context_route_is_404() {
    let app = test_app().await;

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

    assert_eq!(
        response.status(),
        404,
        "old /context endpoint must be fully removed"
    );
}

#[tokio::test]
async fn test_v1_invalid_body_returns_400() {
    let app = test_app().await;

    // 5 bytes can't decode as either an ABI-encoded tuple or an
    // ABI-encoded array of tuples, so both paths in decode_request_body
    // must reject it.
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v1")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00]))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn test_v1_empty_batch_returns_empty_array() {
    let app = test_app().await;

    // ABI-encoded empty Vec<OracleRequestTuple>: a properly encoded
    // batch with zero elements. Per upstream contract the response
    // length must match the request length, so this should be a 200
    // with `[]`, not a 400.
    let empty: Vec<(OrderV4, U256, U256, Address)> = Vec::new();
    let body = Bytes::from(empty.abi_encode());

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v1")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: OracleResponse = serde_json::from_slice(&bytes).unwrap();
    assert!(responses.is_empty(), "empty batch must return empty array");
}

#[tokio::test]
async fn test_v1_unknown_token_returns_200_with_inner_err() {
    let app = test_app().await;

    let body = encode_single(
        "0x9999999999999999999999999999999999999999",
        "0x8888888888888888888888888888888888888888",
    );

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v1")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: OracleResponse = serde_json::from_slice(&bytes).unwrap();

    assert!(matches!(responses[0], OracleResult::Err(_)));
}

#[tokio::test]
async fn test_v1_single_returns_v1_schema_from_cache() {
    let app = test_app().await;
    let body = encode_single(USDC, WCOIN);

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v1")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: OracleResponse = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        responses.len(),
        1,
        "single-request must return length-1 array"
    );
    let resp = responses[0].as_result().unwrap();
    assert_eq!(
        resp.context.len(),
        3,
        "schema v1 must have 3 context elements"
    );

    // version
    let version = Float::from(alloy::primitives::B256::from(resp.context[0]));
    assert_eq!(version.format().unwrap(), SCHEMA_VERSION.to_string());

    // price (bid = 100.0 — we always use bid, inverted via Float when needed)
    let price = Float::from(alloy::primitives::B256::from(resp.context[1]));
    assert_eq!(price.format().unwrap(), "100");

    // publish_time = the cached Alpaca `t`, not server now().
    // Compare against a Float-round-tripped canonical form since Rain
    // Float formats large integers in scientific notation.
    let publish = Float::from(alloy::primitives::B256::from(resp.context[2]));
    let expected = Float::parse(FIXED_PUBLISH_TIME.to_string())
        .unwrap()
        .format()
        .unwrap();
    assert_eq!(publish.format().unwrap(), expected);
}

#[tokio::test]
async fn test_v1_batch_returns_length_matching_array() {
    let app = test_app().await;
    // Two orders: buy COIN, then sell COIN.
    let body = encode_batch(&[(USDC, WCOIN), (WCOIN, USDC)]);

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v1")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: OracleResponse = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(responses.len(), 2, "batch of 2 must return length-2 array");

    // First: buy → bid (100) — both directions use the same underlying price
    let buy_price = Float::from(alloy::primitives::B256::from(
        responses[0].as_result().unwrap().context[1],
    ));
    assert_eq!(buy_price.format().unwrap(), "100");

    // Second: sell → 1/bid, where bid = 100 → exactly 0.01
    let sell_price = Float::from(alloy::primitives::B256::from(
        responses[1].as_result().unwrap().context[1],
    ));
    assert_eq!(sell_price.format().unwrap(), "0.01");
}

#[tokio::test]
async fn test_v1_batch_returns_mixed_result() {
    let app = test_app().await;
    // Two orders: one i sknown and chade, other is unknown and not cached.
    let body = encode_batch(&[(USDC, WCOIN), (UNKNOWN_TOKEN, USDC)]);

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v1")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: OracleResponse = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(responses.len(), 2, "batch of 2 must return length-2 array");

    // First: known order pair
    let buy_price = Float::from(alloy::primitives::B256::from(
        responses[0].as_result().unwrap().context[1],
    ));
    assert_eq!(buy_price.format().unwrap(), "100");

    // Second: error as the UNKNOWN_TOKEN is not cached
    assert_eq!(
        responses[1].as_result().unwrap_err().msg,
        format!(
            "Bad request: Unknown tStock token: {} (not in registry)",
            UNKNOWN_TOKEN.to_string()
        )
    );
}
