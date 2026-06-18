//! Parity-window diff observer (RAI-361).
//!
//! Probes the legacy Fly oracle and the new DigitalOcean oracle in
//! lockstep with the same ABI-encoded `/context/v1` request, decodes
//! both signed contexts, and emits per-symbol per-direction drift
//! metrics so we can see — before flipping the public-ingress switch
//! — whether the two URLs agree on price.
//!
//! Public surface here is pure logic: request encoding, response
//! decoding, drift computation, single-probe orchestration. The
//! binary wraps it in a tokio loop + axum `/metrics` server.

use crate::oracle::OracleResponse;
use crate::{EvaluableV4, OrderV4, IOV2};
use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolValue;
use rain_math_float::Float;
use serde::Deserialize;
use std::str::FromStr;

#[derive(Debug, Clone, Deserialize)]
pub struct ObserverConfig {
    /// HTTP port the observer binds for its own `/metrics` endpoint.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Seconds between probe rounds. Each round hits both oracles
    /// once per (symbol, direction) pair.
    #[serde(default = "default_interval")]
    pub probe_interval_secs: u64,

    /// Full URL of the legacy Fly oracle, including scheme + host +
    /// port. The `/context/v1` suffix is appended by the probe loop.
    pub fly_oracle_base_url: String,

    /// Full URL of the new DigitalOcean oracle, same format. During
    /// the parity window this resolves through the tailnet MagicDNS
    /// (e.g. `http://st0x-oracle-server:3000`).
    pub do_oracle_base_url: String,

    /// USDC (the implicit quote token) on the chain the oracles serve.
    pub quote_token: String,

    /// Symbols + base-token addresses to probe. Address is the
    /// on-chain tStock (e.g. wtCOIN) — USDC is the implicit quote.
    pub symbols: Vec<SymbolEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SymbolEntry {
    pub symbol: String,
    pub base_token: String,
}

fn default_port() -> u16 {
    3001
}
fn default_interval() -> u64 {
    30
}

impl ObserverConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!(
                "Failed to read observer config at {}: {}",
                path.display(),
                e
            )
        })?;
        let cfg: Self = toml::from_str(&text).map_err(|e| {
            anyhow::anyhow!("Failed to parse observer config {}: {}", path.display(), e)
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.symbols.is_empty() {
            anyhow::bail!("observer config has no [[symbols]] entries");
        }
        if self.fly_oracle_base_url.trim().is_empty() {
            anyhow::bail!("fly_oracle_base_url must be set");
        }
        if self.do_oracle_base_url.trim().is_empty() {
            anyhow::bail!("do_oracle_base_url must be set");
        }
        Address::from_str(&self.quote_token)
            .map_err(|e| anyhow::anyhow!("Invalid quote_token: {}", e))?;
        for s in &self.symbols {
            Address::from_str(&s.base_token)
                .map_err(|e| anyhow::anyhow!("Invalid base_token for {}: {}", s.symbol, e))?;
            if s.symbol.trim().is_empty() {
                anyhow::bail!("Empty symbol in observer config");
            }
        }
        if self.probe_interval_secs == 0 {
            anyhow::bail!("probe_interval_secs must be > 0");
        }
        Ok(())
    }
}

/// Which side of the swap to probe. Both oracles are exercised under
/// both directions per round so drift in either rate is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeDirection {
    /// input=USDC, output=tStock (buy).
    QuoteToBase,
    /// input=tStock, output=USDC (sell).
    BaseToQuote,
}

impl ProbeDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QuoteToBase => "quote_to_base",
            Self::BaseToQuote => "base_to_quote",
        }
    }
}

/// Build the ABI-encoded `/context/v1` body for a single-element
/// probe of `(input_token → output_token)`. Wire-compatible with
/// both the Fly oracle and the new DO oracle — the request shape
/// is unchanged across the migration.
pub fn encode_probe_body(input_token: Address, output_token: Address) -> Vec<u8> {
    let order = OrderV4 {
        owner: Address::ZERO,
        evaluable: EvaluableV4 {
            interpreter: Address::ZERO,
            store: Address::ZERO,
            bytecode: alloy::primitives::Bytes::new(),
        },
        validInputs: vec![IOV2 {
            token: input_token,
            vaultId: B256::ZERO,
        }],
        validOutputs: vec![IOV2 {
            token: output_token,
            vaultId: B256::ZERO,
        }],
        nonce: B256::ZERO,
    };
    let tuple: (OrderV4, U256, U256, Address) =
        (order, U256::from(0u64), U256::from(0u64), Address::ZERO);
    tuple.abi_encode()
}

/// Outcome of a single `(symbol, direction)` probe against both
/// oracles. `basis_points` is the relative diff between the two
/// signed prices expressed in basis points (`(do - fly) / fly *
/// 10_000`). `None` means the legacy side returned a non-positive
/// price and a ratio would be undefined.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub symbol: String,
    pub direction: ProbeDirection,
    pub fly_price: Option<f64>,
    pub do_price: Option<f64>,
    pub basis_points: Option<f64>,
    pub fly_publish_time_secs: Option<u64>,
    pub do_publish_time_secs: Option<u64>,
    pub publish_time_diff_secs: Option<i64>,
}

/// Extract `(price, publish_time)` from a length-3 v1 context. Returns
/// `Err` if the context is the wrong length or either field doesn't
/// decode as a Rain Float / Unix-seconds integer.
pub fn extract_v1_price_and_time(resp: &OracleResponse) -> anyhow::Result<(f64, u64)> {
    if resp.context.len() != 3 {
        anyhow::bail!("expected 3-element v1 context, got {}", resp.context.len());
    }
    let price_f = Float::from(B256::from(resp.context[1]));
    let price_str = price_f
        .format()
        .map_err(|e| anyhow::anyhow!("price format failed: {:?}", e))?;
    let price: f64 = price_str
        .parse()
        .map_err(|e| anyhow::anyhow!("price '{}' did not parse as f64: {}", price_str, e))?;

    let publish_f = Float::from(B256::from(resp.context[2]));
    let publish_str = publish_f
        .format()
        .map_err(|e| anyhow::anyhow!("publish format failed: {:?}", e))?;
    // Rain Float formats large ints in scientific notation ("1.7e9"),
    // so go via f64 then round to seconds — same trick the existing
    // integration test uses.
    let publish_f64: f64 = publish_str
        .parse()
        .map_err(|e| anyhow::anyhow!("publish '{}' did not parse as f64: {}", publish_str, e))?;
    let publish_time = publish_f64.round() as i64;
    if publish_time < 0 {
        anyhow::bail!("negative publish_time {}", publish_time);
    }
    Ok((price, publish_time as u64))
}

/// Compute the (do - fly) / fly drift in basis points. Returns `None`
/// when the legacy side is non-positive (ratio undefined).
pub fn drift_basis_points(fly: f64, do_side: f64) -> Option<f64> {
    if fly <= 0.0 || !fly.is_finite() {
        return None;
    }
    Some((do_side - fly) / fly * 10_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle;

    fn float_bytes(s: &str) -> [u8; 32] {
        let f = Float::parse(s.to_string()).unwrap();
        let b: B256 = f.into();
        b.into()
    }

    fn fake_response_v1(price_str: &str, publish_time: u64) -> OracleResponse {
        let ctx = oracle::build_context(float_bytes(price_str), publish_time).unwrap();
        OracleResponse {
            signer: Address::ZERO,
            context: ctx,
            signature: alloy::primitives::Bytes::new(),
        }
    }

    #[test]
    fn extract_round_trips_through_rain_float() {
        let r = fake_response_v1("185.42", 1_700_000_000);
        let (price, publish_time) = extract_v1_price_and_time(&r).unwrap();
        assert!((price - 185.42).abs() < 1e-9);
        // Rain Float scientific notation slightly rounds at 1.7e9 — we
        // accept a tight window around the input.
        let diff = (publish_time as i64) - 1_700_000_000_i64;
        assert!(
            diff.abs() <= 10,
            "publish_time {publish_time} too far from input"
        );
    }

    #[test]
    fn extract_rejects_wrong_context_length() {
        let mut r = fake_response_v1("100", 1);
        r.context.truncate(2);
        assert!(extract_v1_price_and_time(&r).is_err());
    }

    #[test]
    fn drift_is_zero_when_sides_match() {
        let bps = drift_basis_points(100.0, 100.0).unwrap();
        assert!(bps.abs() < 1e-9);
    }

    #[test]
    fn drift_one_percent_is_one_hundred_bps() {
        let bps = drift_basis_points(100.0, 101.0).unwrap();
        assert!((bps - 100.0).abs() < 1e-9);
    }

    #[test]
    fn drift_negative_when_do_below_fly() {
        let bps = drift_basis_points(100.0, 99.5).unwrap();
        assert!((bps - -50.0).abs() < 1e-9);
    }

    #[test]
    fn drift_undefined_when_fly_non_positive() {
        assert!(drift_basis_points(0.0, 100.0).is_none());
        assert!(drift_basis_points(-1.0, 100.0).is_none());
    }

    #[test]
    fn encode_probe_body_round_trips_through_oracle_decoder() {
        // The observer must produce a request the same decoder used
        // by the production oracle can parse. We round-trip via the
        // same `decode_request_body` path the handler uses.
        let usdc = Address::from_str("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913").unwrap();
        let wcoin = Address::from_str("0x5cDa0E1CA4ce2af96315f7F8963C85399c172204").unwrap();
        let body = encode_probe_body(usdc, wcoin);
        let decoded =
            <(OrderV4, U256, U256, Address)>::abi_decode(&body).expect("single tuple decodes");
        assert_eq!(decoded.0.validInputs.len(), 1);
        assert_eq!(decoded.0.validInputs[0].token, usdc);
        assert_eq!(decoded.0.validOutputs[0].token, wcoin);
    }
}
