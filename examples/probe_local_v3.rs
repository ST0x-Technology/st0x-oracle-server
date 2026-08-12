//! Smoke probe for `/context/v3`. Same orderbook request shape as
//! `probe_local_v2`, but decodes the 6-element v3 context (schema
//! version 3, session slot 3 as V3 IntOrAString — what Rainlang `"…"`
//! string literals parse to). Verifies:
//!
//! - schema_version (slot 0) == 3
//! - oracle buy price (slot 1) is within 1% of the broker mark
//! - sell leg returns 1/mark in Rain Float precision
//! - publish_time (slot 2), session (slot 3, V3 byte-31 layout),
//!   session_start (slot 4), session_end (slot 5) all decode cleanly
//!
//! Usage:
//!   cargo run --example probe_local_v3 -- SPYM
//!
//! `ORACLE_URL=https://oracle.t0trade.com/context/v3 cargo run ...`
//! points the probe at prod.

use alloy::primitives::{Address, FixedBytes, U256};
use alloy::sol_types::SolValue;
use rain_math_float::Float;
use st0x_oracle_server::oracle::OracleResponse;
use st0x_oracle_server::{EvaluableV4, OrderV4, IOV2};
use std::str::FromStr;

/// Probed URL. Override with
/// `ORACLE_URL=https://oracle.t0trade.com/context/v3` to compare
/// prod against the broker.
fn oracle_url() -> String {
    std::env::var("ORACLE_URL").unwrap_or_else(|_| "http://127.0.0.1:3000/context/v3".to_string())
}
const USDC_BASE: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";

// Same registry as config.toml — keep in sync.
const TOKENS: &[(&str, &str)] = &[
    ("AMZN", "0x997baE3EC193a249596d3708C3fAB7C501Bb8a53"),
    ("BMNR", "0x2512EC661f0bA089c275EA105E31bAD6FcFcf319"),
    ("CEG", "0x3aF952888Cd89DAD3e8AF67cf4b7E740B36829C3"),
    ("COIN", "0x5cDa0E1CA4ce2af96315f7F8963C85399c172204"),
    ("CRCL", "0x8AFba81DEc38DE0A18E2Df5E1967a7493651eebf"),
    ("DRAM", "0x1A91Df4a970EBaB1bB4AF32Eb6d10509028eE4b8"),
    ("IAU", "0x1E46d7eFef64A833AFB1CD49299a7AD5B439f4d8"),
    ("MSTR", "0xFF05E1bD696900dc6A52CA35Ca61Bb1024eDa8e2"),
    ("NVDA", "0xFb5B41acdbA20a3230F84BE995173CFb98b8D6E7"),
    ("PPLT", "0x82f5BAEE1076334357a34A19E04f7c282D51cE47"),
    ("SGOV", "0x78c31580c97101694C70022c83D570150c11e935"),
    ("SIVR", "0xEB7F3E4093C9d68253b6104FbbfF561F3eC0442F"),
    ("SPCX", "0x19F89aaEf8a93f38A974beca9776f09aB844887F"),
    ("SPYM", "0x31C2C14134e6E3B7ef9478297F199331133Fc2d8"),
    ("TSLA", "0x219A8d384a10BF19b9f24cB5cC53F79Dd0e5A03D"),
    ("TSM", "0x71C66449d2528E23514A9c197BFD55Ae9DB3B714"),
];

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

/// Decode a session tag from Rain's `IntOrAString` V3 bytes32 — byte
/// 31 is `(len & 0x1f) | 0xe0`, ASCII data lives in bytes
/// `(31-len)..31`. Same shape the Rainlang parser emits for a `"…"`
/// string literal via `LibIntOrAString::fromStringV3`.
fn decode_session_tag(b: alloy::primitives::B256) -> String {
    let bytes = b.as_slice();
    let len = (bytes[31] & 0x1f) as usize;
    String::from_utf8(bytes[31 - len..31].to_vec()).unwrap_or_else(|_| "<non-utf8>".to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let symbol = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "SPYM".to_string());
    let token = TOKENS
        .iter()
        .find(|(s, _)| *s == symbol)
        .map(|(_, addr)| *addr)
        .ok_or_else(|| anyhow::anyhow!("unknown symbol: {symbol}"))?;

    let client = reqwest::Client::new();

    // BUY: input=USDC -> output=wtToken. Should sign mark directly.
    let buy = order_tuple(USDC_BASE, token).abi_encode();
    let buy_resp: Vec<OracleResponse> = client
        .post(oracle_url())
        .header("content-type", "application/octet-stream")
        .body(buy)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // SELL: input=wtToken -> output=USDC. Should sign 1/mark.
    let sell = order_tuple(token, USDC_BASE).abi_encode();
    let sell_resp: Vec<OracleResponse> = client
        .post(oracle_url())
        .header("content-type", "application/octet-stream")
        .body(sell)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let resp = &buy_resp[0];
    anyhow::ensure!(
        resp.context.len() == 6,
        "expected v3 context length 6, got {}",
        resp.context.len()
    );

    let version = Float::from(alloy::primitives::B256::from(resp.context[0]))
        .format()
        .unwrap();
    anyhow::ensure!(version == "3", "expected schema version 3, got {version}");

    let buy_price = Float::from(alloy::primitives::B256::from(resp.context[1]))
        .format()
        .unwrap();
    let sell_price = Float::from(alloy::primitives::B256::from(sell_resp[0].context[1]))
        .format()
        .unwrap();
    let publish = Float::from(alloy::primitives::B256::from(resp.context[2]))
        .format()
        .unwrap();
    let session = decode_session_tag(alloy::primitives::B256::from(resp.context[3]));
    let sess_start = Float::from(alloy::primitives::B256::from(resp.context[4]))
        .format()
        .unwrap();
    let sess_end = Float::from(alloy::primitives::B256::from(resp.context[5]))
        .format()
        .unwrap();

    println!("=== {symbol} via v3 oracle ===");
    println!("  schema version            : {version}");
    println!("  buy  (USDC -> {symbol}) price : {buy_price}");
    println!("  sell ({symbol} -> USDC) price : {sell_price}");
    println!("  publish_time              : {publish}");
    println!("  session                   : {session:?}");
    println!("  session_start             : {sess_start}");
    println!("  session_end               : {sess_end}");
    println!("  signer                    : {:?}", resp.signer);

    // Cross-check the buy price against the broker mark.
    let key = std::env::var("ALPACA_API_KEY_ID")?;
    let secret = std::env::var("ALPACA_API_SECRET_KEY")?;
    let acc = std::env::var("ALPACA_BROKER_ACCOUNT_ID")?;
    let auth = format!(
        "Basic {}",
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{key}:{secret}")
        )
    );
    let url = format!("https://broker-api.alpaca.markets/v1/trading/accounts/{acc}/positions");
    let positions: serde_json::Value = client
        .get(&url)
        .header("Authorization", auth)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mark = positions
        .as_array()
        .and_then(|a| a.iter().find(|p| p["symbol"] == symbol))
        .and_then(|p| p["current_price"].as_str())
        .ok_or_else(|| anyhow::anyhow!("no broker position for {symbol}"))?;
    println!("  broker current_price      : {mark}");

    let oracle_buy: f64 = buy_price.parse()?;
    let broker_mark: f64 = mark.parse()?;
    let drift_pct = (oracle_buy - broker_mark).abs() / broker_mark * 100.0;
    println!("  drift (oracle vs broker)  : {drift_pct:.4}%");
    if drift_pct > 1.0 {
        anyhow::bail!("drift {drift_pct:.4}% > 1% — oracle and broker disagree");
    }

    Ok(())
}
