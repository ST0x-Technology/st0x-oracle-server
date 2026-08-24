use alloy::primitives::{Address, Bytes, FixedBytes, B256, U256};
use rain_math_float::Float;
use serde::{Deserialize, Serialize};

/// Schema version for the signed context array. Bump this whenever the
/// layout changes — strategies assert on it to reject data they don't
/// understand.
pub const SCHEMA_VERSION: u64 = 1;

/// Schema version emitted by `/context/v4`. The retired v2/v3 schemas
/// extended v1 with three session slots (a bytes32 ASCII tag plus the
/// UTC `start` and `end` of the current session); v4 keeps that
/// six-element shape and additionally binds the signed price to the
/// specific `(input_token, output_token)` pair the caller requested it
/// for, appending two extra slots:
///
/// - `context[6]`: input token address (bytes32, Address left-padded)
/// - `context[7]`: output token address (bytes32, Address left-padded)
///
/// Without this binding the signer commits to a numeric price + time
/// only — an attacker holding a valid signed frame for `(A,B)` could
/// stuff it into an order whose IO is `(C,D)` and (if the strategy's
/// `min-price`/`max-price` bands overlap) execute at the wrong ratio.
/// v4 strategies MUST assert
/// `equal-to(signed-context<0 6>, input-token())` and
/// `equal-to(signed-context<0 7>, output-token())` to close the gap.
/// `input-token` / `output-token` are the rain.orderbook subparser
/// context words (see `LibRaindexSubParser.sol`), NOT `order-*` — the
/// canonical spelling matters because Rainlang doesn't fall back on
/// alias names.
pub const SCHEMA_VERSION_V4: u64 = 4;

/// Schema version emitted by `/context/v5`. Extends v4 by signing the
/// pricing model's own expiry alongside the price:
///
/// - `context[8]`: quote expiry (Rain Float, Unix seconds)
///
/// v1–v4 sign a price and a `publish_time` and leave the question of
/// how long that price is good for entirely to the consumer, via the
/// strategy's `max-staleness`. That number is a constant chosen when
/// the strategy was written; the producer's actual binding horizon is
/// not. `st0x.pricing` publishes `expiry_unix_ms` on every frame — the
/// point past which it has explicitly disowned the rate — and it moves
/// with the asset, the session and the calibrated model. A strategy
/// comparing against its own hardcoded window either trusts a price
/// past the point the model stopped standing behind it, or refuses one
/// the model still honours.
///
/// v5 signs that horizon so the producer's answer, not the strategy's
/// guess, is what binds. v5 strategies SHOULD assert
/// `less-than(block-timestamp(), signed-context<0 8>)` in addition to
/// whatever `max-staleness` bound they already apply — the two are
/// complementary: `max-staleness` bounds how old the mark may be,
/// `context[8]` bounds how long the model will honour it.
///
/// v4 stays unchanged and is still served on `/context/v4`.
pub const SCHEMA_VERSION_V5: u64 = 5;

/// Schema version emitted by `/context/v6`. Extends v5 by signing the
/// vault NAV ratio the pricing model priced this quote against
/// (RAI-1479 / RAI-1720):
///
/// - `context[9]`: NAV ratio (Rain Float — the lossless packing of the
///   raw 18-decimal fixed-point `uint256`)
///
/// The base token of a pair is a share in a wrapped-token vault whose
/// NAV can step (e.g. on a dividend deposit). A price signed against
/// one NAV and settled against another is stale in a way no timestamp
/// check can see. st0x.pricing publishes the exact on-chain
/// `convertToAssets(1e18)` value it priced each quote against, as a raw
/// `uint256`; the oracle packs that value as a Rain Float — losslessly,
/// never through any binary-float or decimal-string detour — so a v6
/// strategy can assert numeric equality between the signed ratio and
/// the vault's live answer at settlement, rejecting any fill that
/// straddles a NAV step.
///
/// A Rain Float (not the raw uint256) because of what the strategy
/// compares against: the `erc4626-convert-to-assets` word
/// (rainlanguage/rain.erc4626.words) returns the vault's answer as a
/// Rain Float via `fromFixedDecimalLosslessPacked`, and the
/// interpreter's standard word set has no raw↔Float conversion — the
/// signed slot must speak the word's value model for `equal-to` to be
/// meaningful. Both sides pack losslessly, so numeric equality holds
/// exactly when the underlying uint256 values are equal.
///
/// Zero is the sentinel for "no ratio": the pricing model published no
/// NAV ratio for this asset (a real vault NAV ratio is never zero),
/// and no settlement assertion applies. The oracle packs the sentinel
/// as Float zero rather than failing the request — whether zero is
/// acceptable is the strategy's decision, not the carrier's.
///
/// v5 stays unchanged and is still served on `/context/v5`.
pub const SCHEMA_VERSION_V6: u64 = 6;

/// Fixed-point scale of the NAV ratio signed into v6 slot 9.
///
/// 18 is correct for every ST0x wt vault by construction of the
/// issuance system: wt-vault shares are 18-decimal AND the underlying
/// asset is 18-decimal, so `convertToAssets(1e18)` is an 18-decimal
/// fixed-point number. The on-chain `erc4626-convert-to-assets` word
/// packs at the vault's LIVE asset decimals, so a non-18-decimal
/// underlying would break the strategy's equality against this slot —
/// the invariant lives in the issuance system, and this constant
/// depends on it.
const NAV_RATIO_DECIMALS: u8 = 18;

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
/// direction; the caller has already picked the DIRECTIONAL rate and
/// inverted it into Raindex ratio units — see `pick_rate_bytes`). The
/// pricing service's rates each carry their own direction's spread; the
/// oracle applies no spread of its own — its job is to sign and publish
/// the maker-side price the pricing service quoted for this direction.
///
/// `publish_time` is the time at which the signed context is being
/// produced (Unix seconds, UTC). Inside an active session this is `now`;
/// outside, `MarketHoursCache` rounds it back to the most recent
/// `session_close` so consumers see a freshness signal that tracks the
/// market rather than the request clock.
///
/// Schema v1 context layout:
/// - `context[0]`: schema version (Rain Float, = 1)
/// - `context[1]`: price (Rain Float; maker-side for the request's
///   direction, spread included — see `pick_rate_bytes`)
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

/// Build the six session-aware slots shared by the pair-bound schemas
/// (v4 and v5). Layout:
///
/// - `context[0]`: schema version (Rain Float)
/// - `context[1]`: price (Rain Float; maker-side for the request's
///   direction, spread included — see `pick_rate_bytes`)
/// - `context[2]`: publish_time (Rain Float, Unix seconds — `now`
///   in-session, `last_session_close` out-of-session per RAI-693)
/// - `context[3]`: session tag (Rain IntOrAString bytes32; callers
///   supply `Session::to_bytes32_v3`)
/// - `context[4]`: start of the CURRENT session (Rain Float, Unix sec)
/// - `context[5]`: end of the CURRENT session (Rain Float, Unix sec)
fn build_session_context(
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

/// Build the v4 signed-context array. The six session slots, plus
/// the caller-supplied `input_token` / `output_token` at slots
/// 6 and 7 respectively. Both addresses are the exact bytes the
/// caller sent in `validInputs[input_io_index].token` /
/// `validOutputs[output_io_index].token` — no server-side rewriting,
/// no normalisation, so the strategy's byte-for-byte equality check
/// against the running order's IO addresses is meaningful.
///
/// `price_bytes` is the 32-byte packed Rain Float the caller already
/// picked for this request's swap direction (via `pick_rate_bytes`),
/// exactly as for v1 — the pricing service emits both directional
/// rates directionally (each carrying its own spread); the caller
/// (`pick_rate_bytes`) inverts the directional rate into Raindex ratio
/// units, and this function signs that maker-side price.
///
/// Layout:
/// - `context[0]`: schema version (= 4)
/// - `context[1]`: price (Rain Float; maker-side, spread included)
/// - `context[2]`: publish_time (Rain Float, Unix seconds)
/// - `context[3]`: session tag (Rain IntOrAString V3)
/// - `context[4]`: session_start (Rain Float, Unix seconds)
/// - `context[5]`: session_end (Rain Float, Unix seconds)
/// - `context[6]`: input_token address (bytes32, Address left-padded)
/// - `context[7]`: output_token address (bytes32, Address left-padded)
///
/// `input_token` / `output_token` follow Ethereum's Address→bytes32
/// convention: 12 bytes of zero followed by the 20-byte address, so a
/// Rainlang `equal-to(signed-context<0 6> input-token())` comparison
/// against a `bytes32(uint160(address))`-widened order IO address
/// matches with no extra masking.
pub fn build_context_v4(
    price_bytes: [u8; 32],
    publish_time: u64,
    session_bytes: [u8; 32],
    session_start: u64,
    session_end: u64,
    input_token: Address,
    output_token: Address,
) -> Result<Vec<FixedBytes<32>>, anyhow::Error> {
    build_pair_bound_context(
        SCHEMA_VERSION_V4,
        price_bytes,
        publish_time,
        session_bytes,
        session_start,
        session_end,
        input_token,
        output_token,
    )
}

/// Shared body for the pair-bound schemas (v4 and v5): the six session
/// slots followed by the caller's raw input/output token addresses.
#[allow(clippy::too_many_arguments)]
fn build_pair_bound_context(
    schema_version: u64,
    price_bytes: [u8; 32],
    publish_time: u64,
    session_bytes: [u8; 32],
    session_start: u64,
    session_end: u64,
    input_token: Address,
    output_token: Address,
) -> Result<Vec<FixedBytes<32>>, anyhow::Error> {
    let mut ctx = build_session_context(
        schema_version,
        price_bytes,
        publish_time,
        session_bytes,
        session_start,
        session_end,
    )?;
    let input_b32 = FixedBytes::<32>::left_padding_from(input_token.as_slice());
    let output_b32 = FixedBytes::<32>::left_padding_from(output_token.as_slice());
    ctx.push(input_b32);
    ctx.push(output_b32);
    Ok(ctx)
}

/// Build the v5 signed-context array — the v4 shape plus the pricing
/// model's own expiry at slot 8.
///
/// `quote_expiry` is the frame's `expiry_unix_ms` reduced to whole Unix
/// seconds. It is the producer's binding horizon for this exact rate,
/// measured from the upstream mark's `source_ts`, so a price built off
/// an already-old mark carries a correspondingly near expiry. Truncating
/// to seconds floors it — the signed horizon can never round past the
/// model's real one.
///
/// Layout:
/// - `context[0]`: schema version (= 5)
/// - `context[1]`: price (Rain Float; the rate for this request's
///   direction, spread included — see `pick_rate_bytes`)
/// - `context[2]`: publish_time (Rain Float, Unix seconds)
/// - `context[3]`: session tag (Rain IntOrAString V3)
/// - `context[4]`: session_start (Rain Float, Unix seconds)
/// - `context[5]`: session_end (Rain Float, Unix seconds)
/// - `context[6]`: input_token address (bytes32, Address left-padded)
/// - `context[7]`: output_token address (bytes32, Address left-padded)
/// - `context[8]`: quote expiry (Rain Float, Unix seconds; the
///   less-than consumer assert makes the expiry second itself
///   EXCLUSIVE — already rejected)
#[allow(clippy::too_many_arguments)]
pub fn build_context_v5(
    price_bytes: [u8; 32],
    publish_time: u64,
    session_bytes: [u8; 32],
    session_start: u64,
    session_end: u64,
    input_token: Address,
    output_token: Address,
    quote_expiry: u64,
) -> Result<Vec<FixedBytes<32>>, anyhow::Error> {
    build_expiry_bound_context(
        SCHEMA_VERSION_V5,
        price_bytes,
        publish_time,
        session_bytes,
        session_start,
        session_end,
        input_token,
        output_token,
        quote_expiry,
    )
}

/// Shared body for the expiry-bound schemas (v5 and v6): the pair-bound
/// v4 shape plus the model's expiry at slot 8.
#[allow(clippy::too_many_arguments)]
fn build_expiry_bound_context(
    schema_version: u64,
    price_bytes: [u8; 32],
    publish_time: u64,
    session_bytes: [u8; 32],
    session_start: u64,
    session_end: u64,
    input_token: Address,
    output_token: Address,
    quote_expiry: u64,
) -> Result<Vec<FixedBytes<32>>, anyhow::Error> {
    let mut ctx = build_pair_bound_context(
        schema_version,
        price_bytes,
        publish_time,
        session_bytes,
        session_start,
        session_end,
        input_token,
        output_token,
    )?;
    let expiry_float = Float::parse(quote_expiry.to_string())
        .map_err(|e| anyhow::anyhow!("Failed to parse quote_expiry as Rain float: {:?}", e))?;
    let expiry_b: B256 = expiry_float.into();
    ctx.push(expiry_b);
    Ok(ctx)
}

/// Build the v6 signed-context array — the v5 shape plus the vault NAV
/// ratio at slot 9.
///
/// `nav_ratio` is the raw big-endian `uint256` bytes from the pricing
/// quote's `nav_ratio` — the exact on-chain `convertToAssets(1e18)`
/// value the model priced this rate against. It is packed as a Rain
/// Float at [`NAV_RATIO_DECIMALS`] via `Float::from_fixed_decimal`,
/// the Rust binding of the same Solidity `fromFixedDecimalLossless`
/// the on-chain `erc4626-convert-to-assets` word uses — exact for any
/// value the vault can return, never through an `f64` or a decimal
/// string — so a strategy's numeric `equal-to` against the word's
/// answer holds exactly when the underlying uint256 values are equal.
/// A value too large to pack losslessly fails the request rather than
/// rounding: a rounded ratio is a licence to settle against a NAV the
/// model never priced. A zero value is the "no ratio" sentinel and
/// packs to Float zero (see [`SCHEMA_VERSION_V6`]).
///
/// Layout:
/// - `context[0]`: schema version (= 6)
/// - `context[1]`: price (Rain Float; the rate for this request's
///   direction, spread included — see `pick_rate_bytes`)
/// - `context[2]`: publish_time (Rain Float, Unix seconds)
/// - `context[3]`: session tag (Rain IntOrAString V3)
/// - `context[4]`: session_start (Rain Float, Unix seconds)
/// - `context[5]`: session_end (Rain Float, Unix seconds)
/// - `context[6]`: input_token address (bytes32, Address left-padded)
/// - `context[7]`: output_token address (bytes32, Address left-padded)
/// - `context[8]`: quote expiry (Rain Float, Unix seconds; the
///   less-than consumer assert makes the expiry second itself
///   EXCLUSIVE — already rejected)
/// - `context[9]`: vault NAV ratio (Rain Float, lossless from the raw
///   18-decimal fixed-point uint256; Float zero = no ratio)
#[allow(clippy::too_many_arguments)]
pub fn build_context_v6(
    price_bytes: [u8; 32],
    publish_time: u64,
    session_bytes: [u8; 32],
    session_start: u64,
    session_end: u64,
    input_token: Address,
    output_token: Address,
    quote_expiry: u64,
    nav_ratio: [u8; 32],
) -> Result<Vec<FixedBytes<32>>, anyhow::Error> {
    let mut ctx = build_expiry_bound_context(
        SCHEMA_VERSION_V6,
        price_bytes,
        publish_time,
        session_bytes,
        session_start,
        session_end,
        input_token,
        output_token,
        quote_expiry,
    )?;
    let nav_float =
        Float::from_fixed_decimal(U256::from_be_bytes(nav_ratio), NAV_RATIO_DECIMALS)
            .map_err(|e| anyhow::anyhow!("Failed to pack nav_ratio as a Rain float: {e:?}"))?;
    let nav_b: B256 = nav_float.into();
    ctx.push(nav_b);
    Ok(ctx)
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
        // beyond the pick+invert in `pick_rate_bytes` the oracle does no
        // further price math (no
        // spread). This is the central invariant of the
        // pricing-client integration.
        let bytes = price_bytes_of("0.005");
        let ctx = build_context(bytes, 1).unwrap();
        assert_eq!(ctx[1].as_slice(), &bytes[..]);
    }

    fn v3_session_bytes() -> [u8; 32] {
        let mut sess = [0u8; 32];
        sess[31] = 0xe0 | 3;
        sess[..3].copy_from_slice(b"rth");
        sess
    }

    const IN_TOKEN: Address =
        alloy::primitives::address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    const OUT_TOKEN: Address =
        alloy::primitives::address!("5cDa0E1CA4ce2af96315f7F8963C85399c172204");

    #[test]
    fn test_build_context_v5_layout() {
        let ctx = build_context_v5(
            price_bytes_of("185.42"),
            1_700_000_000,
            v3_session_bytes(),
            1_700_000_000,
            1_700_023_400,
            IN_TOKEN,
            OUT_TOKEN,
            1_700_000_020,
        )
        .unwrap();
        assert_eq!(ctx.len(), 9, "schema v5 must emit 9 elements");

        let version = Float::from(alloy::primitives::B256::from(ctx[0]));
        assert_eq!(version.format().unwrap(), "5");

        let price = Float::from(alloy::primitives::B256::from(ctx[1]));
        assert_eq!(price.format().unwrap(), "185.42");

        // Slots 6/7 keep the v4 pair binding, left-padded per the
        // Address→bytes32 convention.
        assert_eq!(&ctx[6].as_slice()[12..], IN_TOKEN.as_slice());
        assert_eq!(&ctx[6].as_slice()[..12], [0u8; 12]);
        assert_eq!(&ctx[7].as_slice()[12..], OUT_TOKEN.as_slice());

        // Slot 8 is the model's expiry as a Rain Float.
        let expiry = Float::from(alloy::primitives::B256::from(ctx[8]));
        assert_eq!(expiry.format().unwrap(), float_string_of(1_700_000_020));
    }

    #[test]
    fn test_build_context_v5_is_v4_plus_expiry() {
        // v5 must be a strict extension: bytes 0..8 identical to v4 for
        // the same inputs. A v5 strategy that ignores slot 8 has to
        // behave exactly like a v4 one, and anyone diffing the two
        // schemas should see one appended slot and nothing else.
        let price = price_bytes_of("185.42");
        let sess = v3_session_bytes();
        let v4 = build_context_v4(
            price,
            1_700_000_000,
            sess,
            1_700_000_000,
            1_700_023_400,
            IN_TOKEN,
            OUT_TOKEN,
        )
        .unwrap();
        let v5 = build_context_v5(
            price,
            1_700_000_000,
            sess,
            1_700_000_000,
            1_700_023_400,
            IN_TOKEN,
            OUT_TOKEN,
            1_700_000_020,
        )
        .unwrap();

        assert_eq!(v5.len(), v4.len() + 1);
        // Slot 0 is the schema version and is expected to differ.
        assert_eq!(&v5[1..8], &v4[1..8]);
    }

    #[test]
    fn test_build_context_v5_passes_price_bytes_through_unchanged() {
        let bytes = price_bytes_of("0.005");
        let ctx =
            build_context_v5(bytes, 1, v3_session_bytes(), 1, 2, IN_TOKEN, OUT_TOKEN, 3).unwrap();
        assert_eq!(ctx[1].as_slice(), &bytes[..]);
    }

    /// A full-entropy 18-decimal fixed-point NAV ratio — every decimal
    /// digit populated, no trailing zeros — so a lossy (f64 / truncating)
    /// packing cannot round-trip by accident. Represents the numeric
    /// value 1.007813592910771427.
    const NAV_RATIO_RAW: u64 = 1_007_813_592_910_771_427;

    fn nav_ratio_bytes() -> [u8; 32] {
        U256::from(NAV_RATIO_RAW).to_be_bytes()
    }

    #[test]
    fn test_build_context_v6_layout() {
        let nav = nav_ratio_bytes();
        let ctx = build_context_v6(
            price_bytes_of("185.42"),
            1_700_000_000,
            v3_session_bytes(),
            1_700_000_000,
            1_700_023_400,
            IN_TOKEN,
            OUT_TOKEN,
            1_700_000_020,
            nav,
        )
        .unwrap();
        assert_eq!(ctx.len(), 10, "schema v6 must emit 10 elements");

        let version = Float::from(alloy::primitives::B256::from(ctx[0]));
        assert_eq!(version.format().unwrap(), "6");

        let price = Float::from(alloy::primitives::B256::from(ctx[1]));
        assert_eq!(price.format().unwrap(), "185.42");

        // Slots 6/7 keep the v4 pair binding.
        assert_eq!(&ctx[6].as_slice()[12..], IN_TOKEN.as_slice());
        assert_eq!(&ctx[7].as_slice()[12..], OUT_TOKEN.as_slice());

        // Slot 8 keeps the v5 expiry.
        let expiry = Float::from(alloy::primitives::B256::from(ctx[8]));
        assert_eq!(expiry.format().unwrap(), float_string_of(1_700_000_020));

        // Slot 9 is the NAV ratio as a Rain Float: numerically equal to
        // raw / 10^18 with all 18 fractional digits intact...
        let nav_float = Float::from(alloy::primitives::B256::from(ctx[9]));
        let expected = Float::parse("1.007813592910771427".to_string()).unwrap();
        assert!(
            nav_float.eq(expected).unwrap(),
            "slot 9 must equal 1.007813592910771427, got {}",
            nav_float.format().unwrap()
        );

        // ...and losslessly so: unpacking back to 18-decimal fixed point
        // recovers the exact uint256 the vault reported.
        assert_eq!(
            nav_float.to_fixed_decimal(18).unwrap(),
            U256::from(NAV_RATIO_RAW),
            "slot 9 must round-trip to the exact raw ratio"
        );
    }

    #[test]
    fn test_build_context_v6_is_v5_plus_nav_ratio() {
        // v6 must be a strict extension: slots 1..9 identical to v5 for
        // the same inputs. A v6 strategy that ignores slot 9 has to
        // behave exactly like a v5 one, and anyone diffing the two
        // schemas should see one appended slot and nothing else.
        let price = price_bytes_of("185.42");
        let sess = v3_session_bytes();
        let v5 = build_context_v5(
            price,
            1_700_000_000,
            sess,
            1_700_000_000,
            1_700_023_400,
            IN_TOKEN,
            OUT_TOKEN,
            1_700_000_020,
        )
        .unwrap();
        let v6 = build_context_v6(
            price,
            1_700_000_000,
            sess,
            1_700_000_000,
            1_700_023_400,
            IN_TOKEN,
            OUT_TOKEN,
            1_700_000_020,
            nav_ratio_bytes(),
        )
        .unwrap();

        assert_eq!(v6.len(), v5.len() + 1);
        // Slot 0 is the schema version and is expected to differ.
        assert_eq!(&v6[1..9], &v5[1..9]);
    }

    #[test]
    fn test_build_context_v6_zero_nav_ratio_packs_to_float_zero() {
        // Zero is the "no ratio" sentinel: the carrier packs it as Float
        // zero rather than erroring, and the downstream strategy decides
        // whether to accept a frame with no settlement assertion.
        let ctx = build_context_v6(
            price_bytes_of("185.42"),
            1,
            v3_session_bytes(),
            1,
            2,
            IN_TOKEN,
            OUT_TOKEN,
            3,
            [0u8; 32],
        )
        .unwrap();
        assert_eq!(ctx.len(), 10);
        let nav_float = Float::from(alloy::primitives::B256::from(ctx[9]));
        assert!(
            nav_float.is_zero().unwrap(),
            "zero sentinel must pack to Float zero, got {}",
            nav_float.format().unwrap()
        );
    }

    #[test]
    fn test_build_context_v6_unpackable_nav_ratio_fails_closed() {
        // A ratio too large for lossless Float packing must fail the
        // request, not round: a rounded ratio would sign a NAV the model
        // never priced. u256::MAX is far past the packable coefficient
        // range (and past anything a real vault can return).
        let result = build_context_v6(
            price_bytes_of("185.42"),
            1,
            v3_session_bytes(),
            1,
            2,
            IN_TOKEN,
            OUT_TOKEN,
            3,
            [0xffu8; 32],
        );
        assert!(result.is_err(), "unpackable nav_ratio must fail closed");
    }

    #[test]
    fn test_build_context_v6_passes_price_bytes_through_unchanged() {
        let bytes = price_bytes_of("0.005");
        let ctx = build_context_v6(
            bytes,
            1,
            v3_session_bytes(),
            1,
            2,
            IN_TOKEN,
            OUT_TOKEN,
            3,
            nav_ratio_bytes(),
        )
        .unwrap();
        assert_eq!(ctx[1].as_slice(), &bytes[..]);
    }
}
