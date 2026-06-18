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
///
/// `/context/v2` signs slot 3 as `Session::to_bytes32_v1` (byte-0
/// length, `0x80 | len`) to match the hex presets baked into live v2
/// strategies.
pub const SCHEMA_VERSION_V2: u64 = 2;

/// Schema version emitted by `/context/v3`. Same six-element layout
/// as v2; the only difference is slot 3 is signed with
/// `Session::to_bytes32_v3` (byte-31 length, `0xe0 | len`) so that v3
/// strategies can compare against Rainlang `"…"` string literals
/// directly (the parser produces the same V3 bytes via
/// `LibIntOrAString::fromStringV3`).
pub const SCHEMA_VERSION_V3: u64 = 3;

/// Oracle response matching Rain's SignedContextV1 format.
/// The JSON array shape of this struct is what upstream
/// `rain.orderbook/crates/quote/src/oracle.rs` expects to deserialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleResponse {
    pub signer: Address,
    pub context: Vec<FixedBytes<32>>,
    pub signature: Bytes,
}

/// Build the signed context array from a pre-computed Rain Float price
/// and a publish time.
///
/// `price_bytes` is the 32-byte packed Rain Float representation taken
/// directly from the pricing service's wire `Quote` (either
/// `rate_quote_to_base` or `rate_base_to_quote` depending on swap
/// direction; the caller has already picked the correct rate). The
/// pricing service emits both rates pre-spread, so the oracle does no
/// inversion and applies no spread of its own — its job is to sign and
/// publish whatever the pricing service quoted.
///
/// `publish_time` is the time at which the signed context is being
/// produced (Unix seconds, UTC). Inside an active session this is `now`;
/// outside, `MarketHoursCache` rounds it back to the most recent
/// `session_close` so consumers see a freshness signal that tracks the
/// market rather than the request clock.
///
/// Schema v1 context layout:
/// - `context[0]`: schema version (Rain Float, = 1)
/// - `context[1]`: price (Rain Float; pre-spread, direction-correct)
/// - `context[2]`: publish_time (Rain Float, Unix seconds)
pub fn build_context(
    price_bytes: [u8; 32],
    publish_time: u64,
) -> Result<Vec<FixedBytes<32>>, anyhow::Error> {
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
    let publish_bytes_b: B256 = publish_float.into();

    Ok(vec![
        version_bytes,
        B256::from(price_bytes),
        publish_bytes_b,
    ])
}

/// Build a session-aware signed-context array. Layout:
///
/// - `context[0]`: schema version (Rain Float)
/// - `context[1]`: price (Rain Float; pre-spread, direction-correct)
/// - `context[2]`: publish_time (Rain Float, Unix seconds — `now`
///   in-session, `last_session_close` out-of-session per RAI-693)
/// - `context[3]`: session tag (Rain IntOrAString bytes32 in whatever
///   layout the caller supplies — `to_bytes32_v1` for `/context/v2`,
///   `to_bytes32_v3` for `/context/v3`)
/// - `context[4]`: start of the CURRENT session (Rain Float, Unix sec)
/// - `context[5]`: end of the CURRENT session (Rain Float, Unix sec)
///
/// Both `/context/v2` and `/context/v3` use this shared body. They
/// differ only in:
/// - `schema_version` (slot 0): `SCHEMA_VERSION_V2` vs
///   `SCHEMA_VERSION_V3`
/// - `session_bytes` (slot 3): V1 byte-0 vs V3 byte-31 IntOrAString
///   layout — chosen by the caller via `Session::to_bytes32_v1` /
///   `to_bytes32_v3`
pub fn build_session_context(
    schema_version: u64,
    price_bytes: [u8; 32],
    publish_time: u64,
    session_bytes: [u8; 32],
    session_start: u64,
    session_end: u64,
) -> Result<Vec<FixedBytes<32>>, anyhow::Error> {
    let version_float = Float::parse(schema_version.to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse schema version: {:?}", e))?;
    let publish_float = Float::parse(publish_time.to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse publish_time as Rain float: {:?}", e))?;
    let start_float = Float::parse(session_start.to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse session_start as Rain float: {:?}", e))?;
    let end_float = Float::parse(session_end.to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse session_end as Rain float: {:?}", e))?;

    let version_b: B256 = version_float.into();
    let publish_b: B256 = publish_float.into();
    let session_b: B256 = B256::from(session_bytes);
    let start_b: B256 = start_float.into();
    let end_b: B256 = end_float.into();

    Ok(vec![
        version_b,
        B256::from(price_bytes),
        publish_b,
        session_b,
        start_b,
        end_b,
    ])
}

/// Thin wrapper that pins schema_version = 2 for `/context/v2`.
/// Caller supplies `session_bytes` from `Session::to_bytes32_v1`.
pub fn build_context_v2(
    price_bytes: [u8; 32],
    publish_time: u64,
    session_bytes: [u8; 32],
    session_start: u64,
    session_end: u64,
) -> Result<Vec<FixedBytes<32>>, anyhow::Error> {
    build_session_context(
        SCHEMA_VERSION_V2,
        price_bytes,
        publish_time,
        session_bytes,
        session_start,
        session_end,
    )
}

/// Thin wrapper that pins schema_version = 3 for `/context/v3`.
/// Caller supplies `session_bytes` from `Session::to_bytes32_v3`.
pub fn build_context_v3(
    price_bytes: [u8; 32],
    publish_time: u64,
    session_bytes: [u8; 32],
    session_start: u64,
    session_end: u64,
) -> Result<Vec<FixedBytes<32>>, anyhow::Error> {
    build_session_context(
        SCHEMA_VERSION_V3,
        price_bytes,
        publish_time,
        session_bytes,
        session_start,
        session_end,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a u64 through Float::parse+format so we can assert
    /// against whatever canonical string Rain chooses (which uses
    /// scientific notation for large values, e.g. "1.7e9").
    fn float_string_of(n: u64) -> String {
        Float::parse(n.to_string()).unwrap().format().unwrap()
    }

    /// Build a 32-byte Rain Float from a decimal string. Used as a
    /// stand-in for the WireFloat that arrives over the pricing WS.
    fn price_bytes_of(s: &str) -> [u8; 32] {
        let f = Float::parse(s.to_string()).unwrap();
        let b: B256 = f.into();
        b.into()
    }

    #[test]
    fn test_build_context_v1_layout() {
        let ctx = build_context(price_bytes_of("185.42"), 1_700_000_000).unwrap();
        assert_eq!(ctx.len(), 3, "schema v1 must emit 3 elements");

        let version = Float::from(alloy::primitives::B256::from(ctx[0]));
        assert_eq!(version.format().unwrap(), "1");

        let price = Float::from(alloy::primitives::B256::from(ctx[1]));
        assert_eq!(price.format().unwrap(), "185.42");

        let publish = Float::from(alloy::primitives::B256::from(ctx[2]));
        assert_eq!(publish.format().unwrap(), float_string_of(1_700_000_000));
    }

    #[test]
    fn test_build_context_v1_passes_price_bytes_through_unchanged() {
        // Anything in slot 1 must be the exact 32 bytes we passed in —
        // the oracle no longer touches the price (no inversion, no
        // spread). This is the central invariant of the
        // pricing-client integration.
        let bytes = price_bytes_of("0.005");
        let ctx = build_context(bytes, 1).unwrap();
        assert_eq!(ctx[1].as_slice(), &bytes[..]);
    }

    #[test]
    fn test_build_context_v2_layout_and_session_encoding() {
        // Synthetic bytes32 — `build_context_v2` is encoding-agnostic;
        // the V3 vs old-V1 layout choice happens in
        // `market_hours::Session::to_bytes32` and is exercised there.
        // Here we just confirm slot 3 passes through unchanged.
        let mut sess = [0u8; 32];
        sess[..3].copy_from_slice(b"rth");

        let ctx = build_context_v2(
            price_bytes_of("185.42"),
            1_700_000_000,
            sess,
            1_700_000_000,
            1_700_023_400,
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
    fn test_build_context_v2_passes_price_bytes_through_unchanged() {
        let mut sess = [0u8; 32];
        sess[..3].copy_from_slice(b"rth");
        let bytes = price_bytes_of("0.005");
        let ctx = build_context_v2(bytes, 1, sess, 1, 2).unwrap();
        assert_eq!(ctx[1].as_slice(), &bytes[..]);
    }

    #[test]
    fn test_build_context_v3_layout_and_session_encoding() {
        // /context/v3 differs from /context/v2 only in slot 0 (schema
        // version constant) and the IntOrAString encoding the caller
        // chose for slot 3. Here we just confirm slot 0 is 3 and slot
        // 3 passes through unchanged.
        let mut sess = [0u8; 32];
        // V3 IntOrAString puts length in byte 31, not byte 0.
        sess[31] = 0xe0 | 3;
        sess[..3].copy_from_slice(b"rth");

        let ctx = build_context_v3(
            price_bytes_of("185.42"),
            1_700_000_000,
            sess,
            1_700_000_000,
            1_700_023_400,
        )
        .unwrap();
        assert_eq!(ctx.len(), 6, "schema v3 must emit 6 elements");

        let version = Float::from(alloy::primitives::B256::from(ctx[0]));
        assert_eq!(version.format().unwrap(), "3");

        let price = Float::from(alloy::primitives::B256::from(ctx[1]));
        assert_eq!(price.format().unwrap(), "185.42");

        // Session lives raw in the bytes32, not as a Float.
        assert_eq!(ctx[3].as_slice(), sess.as_slice());
    }

    #[test]
    fn test_build_context_v3_passes_price_bytes_through_unchanged() {
        let mut sess = [0u8; 32];
        sess[31] = 0xe0 | 3;
        sess[..3].copy_from_slice(b"rth");
        let bytes = price_bytes_of("0.005");
        let ctx = build_context_v3(bytes, 1, sess, 1, 2).unwrap();
        assert_eq!(ctx[1].as_slice(), &bytes[..]);
    }
}
