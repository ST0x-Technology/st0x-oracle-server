use alloy::primitives::{Address, Bytes, FixedBytes, B256};
use rain_math_float::Float;
use serde::{Deserialize, Serialize};

/// Schema version for the signed context array. Bump this whenever the
/// layout changes — strategies assert on it to reject data they don't
/// understand.
pub const SCHEMA_VERSION: u64 = 1;

/// Schema version emitted by `/context/v2`. The v2 context extends v1
/// with three additional fields describing the current market session:
/// a bytes32 ASCII tag plus the UTC `start` and `end` of that session.
/// v1 stays unchanged and is still served on `/context/v1`.
pub const SCHEMA_VERSION_V2: u64 = 2;

/// Oracle response matching Rain's SignedContextV1 format.
/// The JSON array shape of this struct is what upstream
/// `rain.orderbook/crates/quote/src/oracle.rs` expects to deserialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleResponse {
    pub signer: Address,
    pub context: Vec<FixedBytes<32>>,
    pub signature: Bytes,
}

/// Build the signed context array from a broker mark + publish time.
///
/// `price` is the issuer's broker-side `current_price` for the symbol —
/// a single fair-value number, not a bid/ask pair. Buy orders sign the
/// mark directly (USDC per share); sell orders pass `inverted = true`
/// and this function computes `1 / price` in full Rain DecimalFloat
/// precision. Do NOT pre-compute `1.0 / price` at f64 precision before
/// calling — you'll lose digits that the Float could otherwise retain.
///
/// `publish_time` is the time at which the oracle fetched the mark
/// from the broker (Unix seconds, UTC). Unlike a Market Data API quote
/// (which carries an exchange-stamped `t`), the broker positions
/// endpoint exposes no per-mark timestamp, so the freshest defensible
/// "as-of" we can sign is fetch time. Polling cadence
/// (config.poll_interval_secs) bounds how stale this is relative to
/// the broker's actual mark; consumers gate further with
/// `max-staleness`.
///
/// Schema v1 context layout (all Rain DecimalFloats):
/// - `context[0]`: schema version (= 1)
/// - `context[1]`: price (mark for buys, 1/mark for sells)
/// - `context[2]`: publish_time (Unix seconds)
pub fn build_context(
    price: f64,
    publish_time: u64,
    inverted: bool,
) -> Result<Vec<FixedBytes<32>>, anyhow::Error> {
    let price_str = format_price(price);
    let price_float = Float::parse(price_str.clone()).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse price '{}' as Rain float: {:?}",
            price_str,
            e
        )
    })?;

    // Invert on the Float, not the f64, to preserve precision.
    let final_price = if inverted {
        let one = Float::parse("1".to_string())
            .map_err(|e| anyhow::anyhow!("Failed to parse '1' as Rain float: {:?}", e))?;
        (one / price_float).map_err(|e| anyhow::anyhow!("Failed to invert price: {:?}", e))?
    } else {
        price_float
    };

    let version_float = Float::parse(SCHEMA_VERSION.to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse schema version: {:?}", e))?;

    let publish_str = publish_time.to_string();
    let publish_float = Float::parse(publish_str.clone()).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse publish_time '{}' as Rain float: {:?}",
            publish_str,
            e
        )
    })?;

    let version_bytes: B256 = version_float.into();
    let price_bytes: B256 = final_price.into();
    let publish_bytes: B256 = publish_float.into();

    Ok(vec![version_bytes, price_bytes, publish_bytes])
}

/// Build the v2 signed-context array. Extends v1's `[version, price,
/// publish_time]` with three session fields. The full layout is:
///
/// - `context[0]`: schema version (= 2, Rain Float)
/// - `context[1]`: price (Rain Float; mark for buys, 1/mark for sells)
/// - `context[2]`: publish_time (Rain Float, Unix seconds — same rule
///   as v1: `now` in-session, `last_session_close` out-of-session)
/// - `context[3]`: session tag (right-padded ASCII bytes32; one of
///   `rth`, `premarket`, `afterhours`, `overnight_closed`,
///   `weekend_closed`)
/// - `context[4]`: start of the CURRENT session (Rain Float, Unix sec)
/// - `context[5]`: end of the CURRENT session (Rain Float, Unix sec)
///
/// Strategies compare `context[3]` against the same right-padded ASCII
/// literal via `equal-to`, e.g.:
/// `equal-to(signed-context<0 3>() <ASCII-padded-bytes32>)`.
pub fn build_context_v2(
    price: f64,
    publish_time: u64,
    session_bytes: [u8; 32],
    session_start: u64,
    session_end: u64,
    inverted: bool,
) -> Result<Vec<FixedBytes<32>>, anyhow::Error> {
    let price_str = format_price(price);
    let price_float = Float::parse(price_str.clone()).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse price '{}' as Rain float: {:?}",
            price_str,
            e
        )
    })?;

    let final_price = if inverted {
        let one = Float::parse("1".to_string())
            .map_err(|e| anyhow::anyhow!("Failed to parse '1' as Rain float: {:?}", e))?;
        (one / price_float).map_err(|e| anyhow::anyhow!("Failed to invert price: {:?}", e))?
    } else {
        price_float
    };

    let version_float = Float::parse(SCHEMA_VERSION_V2.to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse v2 schema version: {:?}", e))?;
    let publish_float = Float::parse(publish_time.to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse publish_time as Rain float: {:?}", e))?;
    let start_float = Float::parse(session_start.to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse session_start as Rain float: {:?}", e))?;
    let end_float = Float::parse(session_end.to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse session_end as Rain float: {:?}", e))?;

    let version_bytes: B256 = version_float.into();
    let price_bytes: B256 = final_price.into();
    let publish_bytes: B256 = publish_float.into();
    let session_b256: B256 = B256::from(session_bytes);
    let start_bytes: B256 = start_float.into();
    let end_bytes: B256 = end_float.into();

    Ok(vec![
        version_bytes,
        price_bytes,
        publish_bytes,
        session_b256,
        start_bytes,
        end_bytes,
    ])
}

/// Format an f64 price as a string suitable for Float::parse.
/// Avoids scientific notation which Float::parse may not handle.
fn format_price(price: f64) -> String {
    // Use enough decimal places to preserve precision
    let s = format!("{:.10}", price);
    // Trim trailing zeros after decimal point
    if s.contains('.') {
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        trimmed.to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_price() {
        assert_eq!(format_price(185.42), "185.42");
        assert_eq!(format_price(1000.0), "1000");
        assert_eq!(format_price(0.0005), "0.0005");
        assert_eq!(format_price(99999.99), "99999.99");
    }

    /// Round-trip a u64 through Float::parse+format so we can assert
    /// against whatever canonical string Rain chooses (which uses
    /// scientific notation for large values, e.g. "1.7e9").
    fn float_string_of(n: u64) -> String {
        Float::parse(n.to_string()).unwrap().format().unwrap()
    }

    #[test]
    fn test_build_context_v1_layout_as_is() {
        let ctx = build_context(185.42, 1_700_000_000, false).unwrap();
        assert_eq!(ctx.len(), 3, "schema v1 must emit 3 elements");

        let version = Float::from(alloy::primitives::B256::from(ctx[0]));
        assert_eq!(version.format().unwrap(), "1");

        let price = Float::from(alloy::primitives::B256::from(ctx[1]));
        assert_eq!(price.format().unwrap(), "185.42");

        let publish = Float::from(alloy::primitives::B256::from(ctx[2]));
        assert_eq!(publish.format().unwrap(), float_string_of(1_700_000_000));
    }

    #[test]
    fn test_build_context_v1_layout_inverted() {
        // 1/200 = exact 0.005 — good baseline.
        let ctx = build_context(200.0, 1_700_000_000, true).unwrap();
        assert_eq!(ctx.len(), 3);

        let version = Float::from(alloy::primitives::B256::from(ctx[0]));
        assert_eq!(version.format().unwrap(), "1");

        let price = Float::from(alloy::primitives::B256::from(ctx[1]));
        assert_eq!(price.format().unwrap(), "0.005");
    }

    #[test]
    fn test_build_context_v2_layout_and_session_encoding() {
        // "rth" right-padded into a bytes32 == session slot we'd
        // compare against in the strategy.
        let mut sess = [0u8; 32];
        sess[..3].copy_from_slice(b"rth");

        let ctx = build_context_v2(
            185.42,
            1_700_000_000,
            sess,
            1_700_000_000,
            1_700_023_400,
            false,
        )
        .unwrap();
        assert_eq!(ctx.len(), 6, "schema v2 must emit 6 elements");

        let version = Float::from(alloy::primitives::B256::from(ctx[0]));
        assert_eq!(version.format().unwrap(), "2");

        let price = Float::from(alloy::primitives::B256::from(ctx[1]));
        assert_eq!(price.format().unwrap(), "185.42");

        let publish = Float::from(alloy::primitives::B256::from(ctx[2]));
        assert_eq!(publish.format().unwrap(), float_string_of(1_700_000_000));

        // Session lives raw in the bytes32, not as a Float.
        assert_eq!(ctx[3].as_slice(), sess.as_slice());

        let start = Float::from(alloy::primitives::B256::from(ctx[4]));
        assert_eq!(start.format().unwrap(), float_string_of(1_700_000_000));
        let end = Float::from(alloy::primitives::B256::from(ctx[5]));
        assert_eq!(end.format().unwrap(), float_string_of(1_700_023_400));
    }

    #[test]
    fn test_build_context_v2_inverts_price_same_as_v1() {
        let mut sess = [0u8; 32];
        sess[..3].copy_from_slice(b"rth");
        let ctx = build_context_v2(200.0, 1, sess, 1, 2, true).unwrap();
        let price = Float::from(alloy::primitives::B256::from(ctx[1]));
        assert_eq!(price.format().unwrap(), "0.005");
    }

    #[test]
    fn test_inversion_preserves_precision_vs_f64() {
        // Naive f64 inversion of 3.0 gives 0.3333333333333333 (~16 digits).
        // Our Float-based inversion of the *parsed* 3 should give a cleaner
        // repeating decimal representation than the pre-rounded f64.
        let ctx_rain = build_context(3.0, 1, true).unwrap();
        let rain_str = Float::from(alloy::primitives::B256::from(ctx_rain[1]))
            .format()
            .unwrap();

        let naive = 1.0_f64 / 3.0_f64;
        let naive_ctx = build_context(naive, 1, false).unwrap();
        let naive_str = Float::from(alloy::primitives::B256::from(naive_ctx[1]))
            .format()
            .unwrap();

        // At minimum, the Rain-inverted representation must not be a
        // shorter/truncated form of the pre-rounded f64 string — i.e. they
        // must differ, proving we didn't silently accept the lossy path.
        assert_ne!(
            rain_str, naive_str,
            "Float inversion must not match naive f64 inversion; got rain={rain_str} naive={naive_str}"
        );
    }
}
