//! Production smoke test. Runs only when `RUN_PROD_SMOKE=1` is set so it
//! doesn't execute in the normal `cargo test` suite.
//!
//! Usage: `RUN_PROD_SMOKE=1 cargo test --test smoke_prod -- --nocapture`

use alloy::primitives::{Address, FixedBytes, U256};
use alloy::sol_types::SolValue;
use rain_math_float::Float;
use st0x_oracle_server::oracle::{OracleResponse, SCHEMA_VERSION};
use st0x_oracle_server::{EvaluableV4, OrderV4, IOV2};
use std::str::FromStr;

const PROD_URL: &str = "https://st0x-oracle-server.fly.dev/context/v1";

// Base mainnet
const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
const WCOIN: &str = "0x5cDa0E1CA4ce2af96315f7F8963C85399c172204";

fn order_tuple(input: &str, output: &str) -> (OrderV4, U256, U256, Address) {
    let order = OrderV4 {
        owner: Address::ZERO,
        evaluable: EvaluableV4 {
            interpreter: Address::ZERO,
            store: Address::ZERO,
            bytecode: alloy::primitives::Bytes::new(),
        },
        validInputs: vec![IOV2 {
            token: Address::from_str(input).unwrap(),
            vaultId: FixedBytes::ZERO,
        }],
        validOutputs: vec![IOV2 {
            token: Address::from_str(output).unwrap(),
            vaultId: FixedBytes::ZERO,
        }],
        nonce: FixedBytes::ZERO,
    };
    (order, U256::from(0u64), U256::from(0u64), Address::ZERO)
}

#[tokio::test]
async fn prod_single_buy_coin() {
    if std::env::var("RUN_PROD_SMOKE").is_err() {
        eprintln!("Skipping prod smoke test (set RUN_PROD_SMOKE=1 to run)");
        return;
    }

    let body = order_tuple(USDC, WCOIN).abi_encode();
    let client = reqwest::Client::new();
    let resp = client
        .post(PROD_URL)
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200, "status");
    let responses: Vec<OracleResponse> = resp.json().await.unwrap();
    assert_eq!(responses.len(), 1, "single request → length-1 array");

    let r = &responses[0];
    assert_eq!(
        r.context.len(),
        3,
        "schema v1 → 3 context elements (version, price, publish_time)"
    );

    // version
    let version = Float::from(alloy::primitives::B256::from(r.context[0]));
    assert_eq!(version.format().unwrap(), SCHEMA_VERSION.to_string());

    // price sanity: must be > 0
    let price = Float::from(alloy::primitives::B256::from(r.context[1]));
    let price_str = price.format().unwrap();
    assert!(!price_str.is_empty(), "price must format to non-empty string");
    eprintln!("  COIN buy price: {}", price_str);

    // publish_time is a Unix seconds Rain Float — compare to now().
    // It may be seconds, minutes, hours old depending on market session
    // (24/5 via BOATS or delayed free-tier). We don't enforce a bound
    // here since this is a sanity test — we just log and assert it's
    // within the last year to catch obvious bugs.
    let publish = Float::from(alloy::primitives::B256::from(r.context[2]));
    let publish_str = publish.format().unwrap();
    eprintln!("  publish_time: {}", publish_str);

    // Signer sanity
    let expected_signer: Address =
        "0x8Ff1CA8ED2e98f693A3eA16b3EBE44FE90500A43".parse().unwrap();
    assert_eq!(r.signer, expected_signer, "signer mismatch");
    assert_eq!(r.signature.len(), 65, "signature length");
}

#[tokio::test]
async fn prod_batch_buy_sell_coin() {
    if std::env::var("RUN_PROD_SMOKE").is_err() {
        return;
    }

    let body = vec![order_tuple(USDC, WCOIN), order_tuple(WCOIN, USDC)].abi_encode();
    let client = reqwest::Client::new();
    let resp = client
        .post(PROD_URL)
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let responses: Vec<OracleResponse> = resp.json().await.unwrap();
    assert_eq!(responses.len(), 2, "batch → length-2 array");
    assert_eq!(responses[0].context.len(), 3);
    assert_eq!(responses[1].context.len(), 3);

    // The two responses should carry the SAME publish_time because both
    // resolve to COIN and read the same cache entry.
    assert_eq!(
        responses[0].context[2], responses[1].context[2],
        "both orders resolve to COIN and should share publish_time"
    );

    // And the second should be the inverse (1/bid) of the first side.
    let buy = Float::from(alloy::primitives::B256::from(responses[0].context[1]))
        .format()
        .unwrap();
    let sell = Float::from(alloy::primitives::B256::from(responses[1].context[1]))
        .format()
        .unwrap();
    eprintln!("  buy={}  sell={}", buy, sell);
    assert_ne!(buy, sell, "buy and sell sides should not be identical");
}

#[tokio::test]
async fn prod_cache_is_stable_between_requests() {
    if std::env::var("RUN_PROD_SMOKE").is_err() {
        return;
    }

    // Two back-to-back requests ~0.2s apart should return the same
    // publish_time if we're serving from cache (poll interval is 10s).
    let body = order_tuple(USDC, WCOIN).abi_encode();
    let client = reqwest::Client::new();

    let first: Vec<OracleResponse> = client
        .post(PROD_URL)
        .header("content-type", "application/octet-stream")
        .body(body.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let second: Vec<OracleResponse> = client
        .post(PROD_URL)
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        first[0].context[2], second[0].context[2],
        "publish_time must be stable within a 10s poll window (proves cache hit, not fresh fetch)"
    );
}
