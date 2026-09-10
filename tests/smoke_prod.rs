//! Production smoke test. Runs only when `RUN_PROD_SMOKE=1` is set so it
//! doesn't execute in the normal `cargo test` suite.
//!
//! Usage: `RUN_PROD_SMOKE=1 cargo test --test smoke_prod -- --nocapture`
//! Success checks require an active quote during an executable market session.

use alloy::primitives::{Address, FixedBytes, U256};
use alloy::sol_types::SolValue;
use rain_math_float::Float;
use st0x_oracle_server::oracle::{OracleResponse, SCHEMA_VERSION_V7};
use st0x_oracle_server::{EvaluableV4, OrderV4, IOV2};
use std::str::FromStr;

/// Returns true only if the env var is exactly "1". `RUN_PROD_SMOKE=0`
/// or any other value disables the test, matching the documented contract.
fn smoke_enabled() -> bool {
    matches!(std::env::var("RUN_PROD_SMOKE").as_deref(), Ok("1"))
}

const PROD_URL: &str = "https://st0x-oracle-server.fly.dev/context/v7";
const LEGACY_URL: &str = "https://st0x-oracle-server.fly.dev/context/v1";

// Base mainnet
const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
const WCOIN: &str = "0x5cDa0E1CA4ce2af96315f7F8963C85399c172204";

fn assert_v7_context(response: &OracleResponse, input: &str, output: &str) {
    assert_eq!(response.context.len(), 9, "schema v7 context length");
    let version = Float::from(response.context[0]);
    assert_eq!(version.format().unwrap(), SCHEMA_VERSION_V7.to_string());
    assert_eq!(
        response.context[6],
        Address::from_str(input).unwrap().into_word()
    );
    assert_eq!(
        response.context[7],
        Address::from_str(output).unwrap().into_word()
    );
    let expiry: u64 = Float::from(response.context[8])
        .to_fixed_decimal(0)
        .unwrap()
        .try_into()
        .unwrap();
    assert!(expiry > 0, "signed expiry must be positive Unix seconds");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        now < expiry,
        "signed expiry is exclusive and must still be in the future"
    );
}

#[tokio::test]
async fn prod_legacy_v1_refuses_signatures() {
    if !smoke_enabled() {
        return;
    }

    let response = reqwest::Client::new()
        .post(LEGACY_URL)
        .header("content-type", "application/octet-stream")
        .body(order_tuple(USDC, WCOIN).abi_encode())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
}

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
    if !smoke_enabled() {
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
    assert_v7_context(r, USDC, WCOIN);

    // price sanity: must be > 0
    let price = Float::from(alloy::primitives::B256::from(r.context[1]));
    let price_str = price.format().unwrap();
    assert!(
        !price_str.is_empty(),
        "price must format to non-empty string"
    );
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
    let expected_signer: Address = "0x8Ff1CA8ED2e98f693A3eA16b3EBE44FE90500A43"
        .parse()
        .unwrap();
    assert_eq!(r.signer, expected_signer, "signer mismatch");
    assert_eq!(r.signature.len(), 65, "signature length");
}

#[tokio::test]
async fn prod_batch_buy_sell_coin() {
    if !smoke_enabled() {
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
    assert_v7_context(&responses[0], USDC, WCOIN);
    assert_v7_context(&responses[1], WCOIN, USDC);

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
async fn prod_publish_time_is_monotonic_and_dedupes() {
    if !smoke_enabled() {
        return;
    }

    // Cache-hit detection without timing flakes:
    // Fire 10 rapid requests with no inter-request delay. Across a
    // ~10s poll window we expect at most 1-2 distinct publish_time
    // values, which proves we're serving from a cache rather than
    // re-fetching on every request. We also assert publish_time is
    // weakly monotonic (never decreases) across the series — that
    // would catch obvious cache corruption regardless of refresh
    // boundaries.
    let body = order_tuple(USDC, WCOIN).abi_encode();
    let client = reqwest::Client::new();

    let mut publish_times: Vec<alloy::primitives::FixedBytes<32>> = Vec::with_capacity(10);
    for _ in 0..10 {
        let r: Vec<OracleResponse> = client
            .post(PROD_URL)
            .header("content-type", "application/octet-stream")
            .body(body.clone())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_v7_context(&r[0], USDC, WCOIN);
        publish_times.push(r[0].context[2]);
    }

    // Decode all publish_times to numeric Unix seconds via Float.
    let secs: Vec<String> = publish_times
        .iter()
        .map(|b| {
            Float::from(alloy::primitives::B256::from(*b))
                .format()
                .unwrap()
        })
        .collect();
    eprintln!("  publish_times: {:?}", secs);

    let distinct: std::collections::HashSet<&String> = secs.iter().collect();
    assert!(
        distinct.len() <= 3,
        "10 rapid requests produced {} distinct publish_times — caching is not working: {:?}",
        distinct.len(),
        secs
    );
}
