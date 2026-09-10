use alloy::primitives::{Address, FixedBytes, B256, U256};
use alloy::sol_types::SolValue;
use axum::body::Bytes;
use chrono::{Duration as ChronoDuration, NaiveDate, TimeZone, Utc};
use http_body_util::BodyExt;
use rain_math_float::Float;
use st0x_oracle_server::market_hours::{MarketHoursCache, SessionWindow};
use st0x_oracle_server::metrics::MetricsHandle;
use st0x_oracle_server::oracle::{BatchItemResponse, OracleResponse, SCHEMA_VERSION};
use st0x_oracle_server::pricing_client::LiveClient;
use st0x_oracle_server::registry::TokenRegistry;
use st0x_oracle_server::sign::Signer;
use st0x_oracle_server::{create_app, AppState, ErrorResponse, EvaluableV4, OrderV4, IOV2};
use st0x_pricing_types::{Quote, WireAddress, WireFloat, WireU256};
use std::str::FromStr;
use std::sync::Arc;
use tower::ServiceExt;

const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

// Token addresses for testing
const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
const WCOIN: &str = "0x1111111111111111111111111111111111111111";
const WDRAM: &str = "0x2222222222222222222222222222222222222222";

// The `source_ts` (Unix seconds) seeded on every test quote via
// `fake_quote` (which multiplies by 1000 for the ms field). Since the
// oracle signs the quote's source_ts as publish_time, tests assert the
// signed publish_time equals this. Chosen in the past so a regression
// that signed `now` would diverge visibly. 1_700_000_000 = 2023-11-14.
const FIXED_PUBLISH_TIME: i64 = 1_700_000_000;

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

/// Build a 32-byte Rain WireFloat from a decimal string. Used to seed
/// deterministic prices on test `Quote`s without going through the
/// pricing WS.
fn wire_float_of(s: &str) -> WireFloat {
    let f = Float::parse(s.to_string()).unwrap();
    let b: B256 = f.into();
    WireFloat::from_bytes(b.into())
}

/// Build a fake live `Quote` for a single symbol. Rates are DIRECTIONAL
/// per the pricing-types contract (`quote_to_base` prices quote-in/
/// base-out, `base_to_quote` the reverse); the oracle inverts the
/// directional rate into Raindex ratio units (`pick_rate_bytes`).
fn fake_quote(symbol: &str, base_token: &str, quote_to_base: &str, base_to_quote: &str) -> Quote {
    let base_bytes: [u8; 20] = Address::from_str(base_token).unwrap().into();
    let usdc_bytes: [u8; 20] = Address::from_str(USDC).unwrap().into();
    Quote {
        asset: symbol.to_string(),
        chain_id: 8453,
        base: WireAddress::from(base_bytes),
        quote: WireAddress::from(usdc_bytes),
        rate_base_to_quote: wire_float_of(base_to_quote),
        rate_quote_to_base: wire_float_of(quote_to_base),
        expiry_unix_ms: i64::MAX,
        source_ts_unix_ms: FIXED_PUBLISH_TIME * 1000,
        // Zero = the "no ratio" sentinel. Tests that exercise the v6
        // NAV-ratio slot overwrite this with a full-entropy pattern.
        nav_ratio: WireU256::ZERO,
        // By default the underlying rates mirror the vault rates — the
        // "base is not a vault token, underlying == vault rate" case — so
        // existing schema/orientation tests behave identically on /context/v7.
        // The v7 slot-1 tests below seed DISTINCT underlying rates to prove
        // v7 signs the underlying, not the vault rate.
        underlying_rate_base_to_quote: wire_float_of(base_to_quote),
        underlying_rate_quote_to_base: wire_float_of(quote_to_base),
    }
}

/// A full-entropy 18-decimal fixed-point NAV ratio — every decimal
/// digit populated, no trailing zeros — so a lossy (f64 / truncating)
/// packing cannot round-trip by accident. Represents the numeric value
/// 1.007813592910771427.
const NAV_RATIO_RAW: u64 = 1_007_813_592_910_771_427;

fn nav_ratio_pattern() -> WireU256 {
    WireU256::from_bytes(U256::from(NAV_RATIO_RAW).to_be_bytes())
}

/// Build a test app with a pre-populated pricing cache. Seeded quotes
/// carry `source_ts_unix_ms = FIXED_PUBLISH_TIME * 1000`, and since the
/// oracle signs the quote's source_ts as publish_time, tests assert the
/// signed publish_time against `FIXED_PUBLISH_TIME`. The market-hours
/// cache only affects the v4/v5 session slots, not publish_time.
async fn test_app() -> axum::Router {
    test_app_with(&[(WCOIN, "COIN", Some(100.0))]).await
}

async fn test_app_with(entries: &[(&str, &str, Option<f64>)]) -> axum::Router {
    test_app_full(entries, fixed_close_market_hours().await).await
}

/// Same as `test_app_with` but lets a caller plug in any
/// `MarketHoursCache` configuration — used by the publish_time tests.
async fn test_app_full(
    entries: &[(&str, &str, Option<f64>)],
    market_hours: Arc<MarketHoursCache>,
) -> axum::Router {
    let signer = Signer::new(TEST_KEY).unwrap();

    let registry_entries: Vec<(String, String)> = entries
        .iter()
        .map(|(addr, sym, _)| (addr.to_string(), sym.to_string()))
        .collect();
    let registry = TokenRegistry::new(registry_entries, USDC).unwrap();

    let mut quotes = Vec::new();
    for (addr, sym, price) in entries {
        if let Some(p) = price {
            // Seed a spread-free two-sided quote at `p` USDC/base: per the
            // pricing-types contract, `quote_to_base` is base-per-quote
            // (= 1/p) and `base_to_quote` is quote-per-base (= p). The
            // oracle inverts the DIRECTIONAL rate into Raindex ratio
            // units (see `pick_rate_bytes`), so a QuoteToBase request
            // serves inv(1/p) = p — schema tests keep asserting `p`.
            // Asymmetric (spread-carrying) rates are exercised in
            // `test_maker_orientation_*` below.
            let s = format!("{p}");
            // Rain-Float reciprocal, not f64: keeps inv(1/p) == p exact
            // for any seed price (an f64 reciprocal only round-trips for
            // lucky values like 100).
            let inv_f = Float::parse(s.clone()).unwrap().inv().unwrap();
            let inv = inv_f.format().unwrap();
            quotes.push(fake_quote(sym, addr, &inv, &s));
        }
    }
    let pricing = LiveClient::with_seeded(quotes).await;

    let configured_symbols: Vec<String> = entries.iter().map(|(_, s, _)| s.to_string()).collect();
    let metrics = MetricsHandle::install().expect("metrics install");
    let state = AppState::new(
        signer,
        registry,
        pricing,
        configured_symbols,
        market_hours,
        metrics,
    );
    create_app(state)
}

/// Build a test app whose pricing cache holds asymmetric per-direction
/// rates for one symbol. Used by the maker-orientation test proving the
/// oracle serves each order shape the inverse of its DIRECTIONAL rate
/// (ask to sell-side orders, bid to buy-side).
async fn test_app_asymmetric(quote_to_base: &str, base_to_quote: &str) -> axum::Router {
    let signer = Signer::new(TEST_KEY).unwrap();
    let registry = TokenRegistry::new(vec![(WCOIN.to_string(), "COIN".to_string())], USDC).unwrap();
    let pricing = LiveClient::with_seeded(vec![fake_quote(
        "COIN",
        WCOIN,
        quote_to_base,
        base_to_quote,
    )])
    .await;
    let metrics = MetricsHandle::install().expect("metrics install");
    let state = AppState::new(
        signer,
        registry,
        pricing,
        vec!["COIN".to_string()],
        fixed_close_market_hours().await,
        metrics,
    );
    create_app(state)
}

/// Cache with one prior session window in the past, so the app classifies
/// as "out of session" for the v4/v5 session slots. publish_time is
/// unaffected (it's the quote's source_ts); this just makes the session
/// classification deterministic.
async fn fixed_close_market_hours() -> Arc<MarketHoursCache> {
    let mh = Arc::new(MarketHoursCache::new());
    let close = Utc.timestamp_opt(FIXED_PUBLISH_TIME, 0).unwrap();
    let open = close - ChronoDuration::hours(16);
    let window = SessionWindow {
        date: NaiveDate::from_ymd_opt(2027, 1, 14).unwrap(),
        session_open: open,
        rth_open: open + ChronoDuration::hours(5) + ChronoDuration::minutes(30), // 09:30 ET
        rth_close: open + ChronoDuration::hours(12),                             // 16:00 ET
        session_close: close,
    };
    mh.set(vec![window]).await;
    mh
}

/// Cache that places `now` strictly inside an active session window, so
/// the session slots classify as `rth`. publish_time is still the quote's
/// source_ts regardless; used by the session-slot tests.
async fn always_in_session_market_hours() -> Arc<MarketHoursCache> {
    let mh = Arc::new(MarketHoursCache::new());
    let now = Utc::now();
    let window = SessionWindow {
        date: now.date_naive(),
        session_open: now - ChronoDuration::hours(8),
        // Bracket `now` in the middle of the RTH sub-window too, so a
        // session_info_for(now) classifies as Rth.
        rth_open: now - ChronoDuration::hours(2),
        rth_close: now + ChronoDuration::hours(2),
        session_close: now + ChronoDuration::hours(8),
    };
    mh.set(vec![window]).await;
    mh
}

/// Position the cached window so wall-clock `now` lands in pre-market:
/// inside the extended session, before the RTH sub-window.
async fn premarket_market_hours() -> Arc<MarketHoursCache> {
    let mh = Arc::new(MarketHoursCache::new());
    let now = Utc::now();
    let window = SessionWindow {
        date: now.date_naive(),
        session_open: now - ChronoDuration::hours(1),
        rth_open: now + ChronoDuration::hours(2),
        rth_close: now + ChronoDuration::hours(8),
        session_close: now + ChronoDuration::hours(12),
    };
    mh.set(vec![window]).await;
    mh
}

/// After RTH closes but before the extended-session bell rings — `now`
/// is inside the extended session, past `rth_close`.
async fn afterhours_market_hours() -> Arc<MarketHoursCache> {
    let mh = Arc::new(MarketHoursCache::new());
    let now = Utc::now();
    let window = SessionWindow {
        date: now.date_naive(),
        session_open: now - ChronoDuration::hours(12),
        rth_open: now - ChronoDuration::hours(8),
        rth_close: now - ChronoDuration::hours(1),
        session_close: now + ChronoDuration::hours(2),
    };
    mh.set(vec![window]).await;
    mh
}

/// Two adjacent weekday windows with `now` in the overnight gap between
/// them. The gap is ~8 h (typical weekday overnight), below the 12 h
/// threshold so the classifier returns `OvernightClosed`.
async fn overnight_closed_market_hours() -> Arc<MarketHoursCache> {
    let mh = Arc::new(MarketHoursCache::new());
    let now = Utc::now();
    let yesterday = SessionWindow {
        date: (now - ChronoDuration::days(1)).date_naive(),
        session_open: now - ChronoDuration::hours(20),
        rth_open: now - ChronoDuration::hours(16),
        rth_close: now - ChronoDuration::hours(10),
        session_close: now - ChronoDuration::hours(2), // 2 h ago
    };
    let tomorrow = SessionWindow {
        date: (now + ChronoDuration::days(1)).date_naive(),
        session_open: now + ChronoDuration::hours(6), // 6 h ahead — 8 h overall gap, < 12 h
        rth_open: now + ChronoDuration::hours(10),
        rth_close: now + ChronoDuration::hours(16),
        session_close: now + ChronoDuration::hours(20),
    };
    mh.set(vec![yesterday, tomorrow]).await;
    mh
}

/// Two non-adjacent windows separated by a >= 12 h gap straddling
/// `now`. Mimics Friday-night-through-Monday-morning. The classifier
/// returns `WeekendClosed`.
async fn weekend_closed_market_hours() -> Arc<MarketHoursCache> {
    let mh = Arc::new(MarketHoursCache::new());
    let now = Utc::now();
    let friday = SessionWindow {
        date: (now - ChronoDuration::days(2)).date_naive(),
        session_open: now - ChronoDuration::hours(60),
        rth_open: now - ChronoDuration::hours(56),
        rth_close: now - ChronoDuration::hours(50),
        session_close: now - ChronoDuration::hours(40), // 40 h ago
    };
    let monday = SessionWindow {
        date: (now + ChronoDuration::days(2)).date_naive(),
        session_open: now + ChronoDuration::hours(20), // 20 h ahead — 60 h overall gap
        rth_open: now + ChronoDuration::hours(24),
        rth_close: now + ChronoDuration::hours(30),
        session_close: now + ChronoDuration::hours(36),
    };
    mh.set(vec![friday, monday]).await;
    mh
}

/// Decode a session tag from slot 3 of the signed context. The
/// on-the-wire format is Rain's `IntOrAString` V3: byte 31 holds
/// `(len & 0x1f) | 0xe0`, ASCII data lives in bytes `(31-len)..31`,
/// head zero-padded.
fn decode_session_tag_v3(b: alloy::primitives::FixedBytes<32>) -> String {
    let bytes: [u8; 32] = b.into();
    let len = (bytes[31] & 0x1f) as usize;
    String::from_utf8(bytes[31 - len..31].to_vec()).unwrap()
}

/// Send a single buy through `/context/v5` and return the decoded
/// session tag from the response. Used by the phase-coverage tests.
async fn v5_session_tag_for(app: axum::Router) -> String {
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v5")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
    decode_session_tag_v3(responses[0].context[3])
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
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
    assert!(responses.is_empty(), "empty batch must return empty array");
}

#[tokio::test]
async fn test_v1_unknown_token_returns_400() {
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

    assert_eq!(response.status(), 400);
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
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        responses.len(),
        1,
        "single-request must return length-1 array"
    );
    let resp = &responses[0];
    assert_eq!(
        resp.context.len(),
        3,
        "schema v1 must have 3 context elements"
    );

    // version
    let version = Float::from(alloy::primitives::B256::from(resp.context[0]));
    assert_eq!(version.format().unwrap(), SCHEMA_VERSION.to_string());

    // price (broker mark = 100.0 — same number for both directions,
    // build_context inverts via Float when needed)
    let price = Float::from(alloy::primitives::B256::from(resp.context[1]));
    assert_eq!(price.format().unwrap(), "100");

    // publish_time = the mark's fetch time (QuoteData.t), seeded to
    // FIXED_PUBLISH_TIME, so we expect that exact value here. Compare
    // against a Float-round-tripped canonical form since Rain Float
    // formats large integers in scientific notation.
    let publish = Float::from(alloy::primitives::B256::from(resp.context[2]));
    let expected = Float::parse(FIXED_PUBLISH_TIME.to_string())
        .unwrap()
        .format()
        .unwrap();
    assert_eq!(publish.format().unwrap(), expected);
}

#[tokio::test]
async fn test_v1_publish_time_is_quote_source_ts_even_when_in_session() {
    // publish_time is ALWAYS the pricing quote's own `source_ts`, never
    // the oracle's request clock. Here the market-hours cache says we're
    // inside an active session — under the old behaviour that would have
    // stamped `now`. The signed timestamp must instead be the quote's
    // seeded source_ts (FIXED_PUBLISH_TIME), so the oracle trusts
    // st0x.pricing's honest as-of stamp rather than re-deriving one.
    let mh = always_in_session_market_hours().await;
    let app = test_app_full(&[(WCOIN, "COIN", Some(100.0))], mh).await;

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v1")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
    let publish = Float::from(alloy::primitives::B256::from(responses[0].context[2]));
    let expected = Float::parse(FIXED_PUBLISH_TIME.to_string())
        .unwrap()
        .format()
        .unwrap();
    assert_eq!(
        publish.format().unwrap(),
        expected,
        "in-session publish_time must be the quote's source_ts, not the request clock"
    );
}

#[tokio::test]
async fn test_v4_binds_input_and_output_tokens_at_slots_6_and_7() {
    // /context/v4's whole reason for existing: the signed context binds
    // the raw input/output token addresses so an attacker can't reuse a
    // frame across pairs. This test asserts that binding is byte-exact:
    // the caller's USDC + WCOIN come back at slot 6 and slot 7 with
    // Ethereum's Address→bytes32 left-padding.
    let mh = always_in_session_market_hours().await;
    let app = test_app_full(&[(WCOIN, "COIN", Some(100.0))], mh).await;

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v4")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(responses.len(), 1);
    let resp = &responses[0];
    assert_eq!(resp.context.len(), 8, "v4 must emit 8 context elements");

    // schema_version = 4
    let version = Float::from(alloy::primitives::B256::from(resp.context[0]));
    assert_eq!(version.format().unwrap(), "4");

    // session tag still uses V3 IntOrAString (same shape as v3's slot 3)
    // — v4 only adds tokens, it doesn't renegotiate session encoding.
    let sess = resp.context[3].as_slice();
    assert_eq!(sess[31], 0xe3, "byte 31 must be 0xe0 | 3");
    assert_eq!(&sess[28..31], b"rth");

    // Slot 6: input token = USDC, left-padded with 12 zero bytes.
    let usdc_addr: alloy::primitives::Address = std::str::FromStr::from_str(USDC).unwrap();
    let expected_input =
        alloy::primitives::FixedBytes::<32>::left_padding_from(usdc_addr.as_slice());
    assert_eq!(
        resp.context[6], expected_input,
        "slot 6 must equal left-padded input token (USDC)"
    );

    // Slot 7: output token = WCOIN, left-padded with 12 zero bytes.
    let wcoin_addr: alloy::primitives::Address = std::str::FromStr::from_str(WCOIN).unwrap();
    let expected_output =
        alloy::primitives::FixedBytes::<32>::left_padding_from(wcoin_addr.as_slice());
    assert_eq!(
        resp.context[7], expected_output,
        "slot 7 must equal left-padded output token (WCOIN)"
    );

    // The first 12 bytes of each token slot must be zero: a strategy
    // that compares against `bytes32(uint160(address))` expects that
    // convention, so anything nonzero in the padding would silently
    // break equality.
    for slot in [6, 7] {
        let bytes = resp.context[slot].as_slice();
        assert!(
            bytes[..12].iter().all(|&b| b == 0),
            "slot {slot} padding must be zero: {:?}",
            &bytes[..12]
        );
    }
}

#[tokio::test]
async fn test_v4_rejects_the_swapped_token_attack() {
    // The scenario v4 exists to prevent: an attacker submits a signed
    // context whose IO tokens don't match the running order. This test
    // proves the tokens the signer commits to *are* the ones the caller
    // sent — an on-chain byte-for-byte check against the order's IO
    // therefore cannot be satisfied by a frame signed for a different
    // pair. Signing itself is unconditional (an attacker can always
    // get *a* frame for the pair they submit); the strategy's equality
    // check on slots 6/7 is what closes the loophole.
    //
    // Scenario: victim has an order with IO = (USDC, WCOIN). An attacker
    // asks the oracle for a frame targeting the SWAPPED pair (WCOIN, USDC)
    // and tries to submit that as calldata against the victim order.
    // The returned frame's slots 6/7 bind to what the attacker asked
    // for, not to what the victim order will read on-chain — so the
    // v4 strategy's `equal-to(signed-context<0 6> input-token())`
    // check fails and the order reverts.
    let mh = always_in_session_market_hours().await;
    let app = test_app_full(&[(WCOIN, "COIN", Some(100.0))], mh).await;

    // Attacker requests: input = WCOIN, output = USDC (swapped from the
    // victim's (USDC, WCOIN) IO).
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v4")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(WCOIN, USDC)))
                .unwrap(),
        )
        .await
        .unwrap();

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
    let resp = &responses[0];

    let usdc_addr: alloy::primitives::Address = std::str::FromStr::from_str(USDC).unwrap();
    let wcoin_addr: alloy::primitives::Address = std::str::FromStr::from_str(WCOIN).unwrap();

    // Signed slots reflect the attacker's swapped request, verbatim.
    assert_eq!(
        resp.context[6],
        alloy::primitives::FixedBytes::<32>::left_padding_from(wcoin_addr.as_slice())
    );
    assert_eq!(
        resp.context[7],
        alloy::primitives::FixedBytes::<32>::left_padding_from(usdc_addr.as_slice())
    );

    // ...which means they do NOT match the victim order's `input-token()`
    // (USDC) / `output-token()` (WCOIN). Those two `assert_ne!`s are the
    // heart of the security property: the v4 strategy's on-chain
    // `equal-to(signed-context<0 6> input-token())` check must fail
    // against this frame, so the swapped-frame attack reverts.
    assert_ne!(
        resp.context[6],
        alloy::primitives::FixedBytes::<32>::left_padding_from(usdc_addr.as_slice()),
        "slot 6 must NOT match the victim order's input-token (USDC)"
    );
    assert_ne!(
        resp.context[7],
        alloy::primitives::FixedBytes::<32>::left_padding_from(wcoin_addr.as_slice()),
        "slot 7 must NOT match the victim order's output-token (WCOIN)"
    );
}

#[tokio::test]
async fn test_v5_handler_signs_rth_when_now_inside_rth() {
    let app = test_app_full(
        &[(WCOIN, "COIN", Some(100.0))],
        always_in_session_market_hours().await,
    )
    .await;
    assert_eq!(v5_session_tag_for(app).await, "rth");
}

#[tokio::test]
async fn test_v5_handler_signs_premarket_when_now_before_rth_open() {
    let app = test_app_full(
        &[(WCOIN, "COIN", Some(100.0))],
        premarket_market_hours().await,
    )
    .await;
    assert_eq!(v5_session_tag_for(app).await, "premarket");
}

#[tokio::test]
async fn test_v5_handler_signs_afterhours_when_now_past_rth_close() {
    let app = test_app_full(
        &[(WCOIN, "COIN", Some(100.0))],
        afterhours_market_hours().await,
    )
    .await;
    assert_eq!(v5_session_tag_for(app).await, "afterhours");
}

#[tokio::test]
async fn test_v5_handler_signs_overnight_closed_for_short_gap() {
    let app = test_app_full(
        &[(WCOIN, "COIN", Some(100.0))],
        overnight_closed_market_hours().await,
    )
    .await;
    assert_eq!(v5_session_tag_for(app).await, "overnight_closed");
}

#[tokio::test]
async fn test_v5_handler_signs_weekend_closed_for_long_gap() {
    let app = test_app_full(
        &[(WCOIN, "COIN", Some(100.0))],
        weekend_closed_market_hours().await,
    )
    .await;
    assert_eq!(v5_session_tag_for(app).await, "weekend_closed");
}

#[tokio::test]
async fn test_v5_session_tag_reflects_market_phase() {
    // Out-of-session market_hours (fixed_close_market_hours pins us
    // outside any active window) -> session tag should be a closed
    // variant, not "rth". With only a single window in the cache and
    // `now` after it, the cache returns OvernightClosed (no
    // `next_open` to widen the gap).
    let app = test_app_full(
        &[(WCOIN, "COIN", Some(100.0))],
        fixed_close_market_hours().await,
    )
    .await;
    assert_eq!(v5_session_tag_for(app).await, "overnight_closed");
}

#[tokio::test]
async fn test_v5_batch_returns_length_matching_array_with_session() {
    let mh = always_in_session_market_hours().await;
    let app = test_app_full(&[(WCOIN, "COIN", Some(100.0))], mh).await;
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v5")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_batch(&[
                    (USDC, WCOIN),
                    (WCOIN, USDC),
                ])))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(responses.len(), 2);
    // Both legs must agree on the session snapshot.
    assert_eq!(responses[0].context[3], responses[1].context[3]);
    assert_eq!(responses[0].context[4], responses[1].context[4]);
    assert_eq!(responses[0].context[5], responses[1].context[5]);
    // Second leg is a buy-side order (input=tStock, output=USDC): its
    // ratio is base-per-quote at the bid = inv(base_to_quote) = 1/100.
    // Maker orientation is pinned in
    // `test_maker_orientation_ask_above_bid_per_direction`.
    let sell_price = Float::from(alloy::primitives::B256::from(responses[1].context[1]));
    assert_eq!(sell_price.format().unwrap(), "0.01");
}

#[tokio::test]
async fn test_v5_empty_batch_returns_empty_array() {
    let empty: Vec<(OrderV4, U256, U256, Address)> = Vec::new();
    let body = Bytes::from(empty.abi_encode());
    let app = test_app().await;
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v5")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
    assert!(responses.is_empty());
}

#[tokio::test]
async fn test_status_reports_no_missing_when_all_cached() {
    let app = test_app().await;

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/status")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(body["configured_symbols"], serde_json::json!(["COIN"]));
    assert_eq!(body["missing_symbols"], serde_json::json!([]));
    assert!(
        body["signer"].as_str().unwrap().starts_with("0x"),
        "signer should be a 0x-prefixed address"
    );
}

#[tokio::test]
async fn test_status_reports_missing_when_symbol_uncached() {
    // Configure two symbols but only cache COIN. /status should list
    // DRAM as missing so operators / monitoring can pick up the partial
    // state without parsing logs.
    let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/status")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        body["configured_symbols"],
        serde_json::json!(["COIN", "DRAM"])
    );
    assert_eq!(body["missing_symbols"], serde_json::json!(["DRAM"]));
}

#[tokio::test]
async fn test_v1_returns_503_for_uncached_symbol() {
    // Configured-but-uncached symbol is the post-soft-start failure
    // mode: server is up, healthy symbols quote, an unfilled position
    // returns 503 per request instead of taking down the whole oracle.
    let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;

    // Healthy symbol still works.
    let ok_response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v1")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok_response.status(), 200);

    // Uncached symbol returns 503.
    let degraded_response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v1")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WDRAM)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        degraded_response.status(),
        503,
        "uncached symbol must 503 instead of taking down the whole server"
    );
    let bytes = degraded_response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        body["detail"].as_str().unwrap().contains("DRAM"),
        "503 body should name the missing symbol; got: {body}"
    );
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
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(responses.len(), 2, "batch of 2 must return length-2 array");

    // First leg (input=USDC, output=tStock → maker sells base):
    // quote-per-base at the ask = inv(quote_to_base) = inv(1/100) = 100.
    let buy_price = Float::from(alloy::primitives::B256::from(responses[0].context[1]));
    assert_eq!(buy_price.format().unwrap(), "100");

    // Second leg (input=tStock, output=USDC → maker buys base):
    // base-per-quote at the bid = inv(base_to_quote) = 1/100. Maker
    // orientation is pinned in
    // `test_maker_orientation_ask_above_bid_per_direction`; this test
    // exercises batching coherence + array length.
    let sell_price = Float::from(alloy::primitives::B256::from(responses[1].context[1]));
    assert_eq!(sell_price.format().unwrap(), "0.01");
}

#[tokio::test]
async fn test_maker_orientation_ask_above_bid_per_direction() {
    // MAKER-ORIENTATION INVARIANT (the 2026-08-07 crossed-order fix).
    //
    // Pricing rates are DIRECTIONAL: `quote_to_base` prices the
    // quote-in/base-out swap, `base_to_quote` the base-in/quote-out
    // swap, each carrying its own spread. A Raindex order with
    // input=quote/output=base IS the quote-in/base-out venue, so its
    // ratio (quote per base) must be inv(quote_to_base). The previous
    // implementation grabbed the unit-compatible OPPOSITE slot instead
    // ("no inversion math") — every sell order quoted the bid and every
    // buy order the ask, and the first deployed order pair read as
    // crossed by exactly 2x the session spread.
    //
    // Seed a realistic spread-carrying pair around mid 100:
    //   base_to_quote = 99   (taker sells base, receives 99: maker bid)
    //   quote_to_base = 0.01 (taker pays 1 quote, receives 0.01 base:
    //                         maker ask = inv(0.01) = 100)
    let app = test_app_asymmetric("0.01", "99").await;

    // v5 is what production signs; v1, v4 and v6 share pick_rate_bytes
    // but each handler carries its own code path and its own comments —
    // the exact divergence surface that produced this bug — so pin all
    // remaining endpoints. v7 uses `pick_underlying_rate_bytes` (a distinct
    // picker), and since this app's underlying rates mirror the vault rates
    // it must pick the SAME direction and orient identically.
    for endpoint in [
        "/context/v1",
        "/context/v4",
        "/context/v5",
        "/context/v6",
        "/context/v7",
    ] {
        // Sell-side order (input=USDC, output=tStock): must serve the ASK
        // in quote-per-base units = inv(quote_to_base) = 100.
        let sell_resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(endpoint)
                    .header("content-type", "application/octet-stream")
                    .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = sell_resp.into_body().collect().await.unwrap().to_bytes();
        let sell: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
        let sell_price = Float::from(alloy::primitives::B256::from(sell[0].context[1]));
        assert_eq!(sell_price.format().unwrap(), "100");

        // Buy-side order (input=tStock, output=USDC): must serve
        // base-per-quote at the BID = inv(base_to_quote) = 1/99.
        let buy_resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(endpoint)
                    .header("content-type", "application/octet-stream")
                    .body(axum::body::Body::from(encode_single(WCOIN, USDC)))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = buy_resp.into_body().collect().await.unwrap().to_bytes();
        let buy: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
        let buy_price = Float::from(alloy::primitives::B256::from(buy[0].context[1]));
        let buy_as_quote_per_base = buy_price.inv().unwrap();

        // The pair, viewed in the same units, must be a healthy market:
        // maker ask (100) strictly above maker bid (99). Under the old
        // mapping this exact assertion fails with ask=99 < bid=100 — the
        // crossed pair Raindex displayed.
        assert_eq!(buy_as_quote_per_base.format().unwrap(), "99");
        assert!(
            sell_price.gt(buy_as_quote_per_base).unwrap(),
            "maker ask must exceed maker bid ({endpoint}): ask={} bid={}",
            sell_price.format().unwrap(),
            buy_as_quote_per_base.format().unwrap()
        );
    }
}

#[tokio::test]
async fn test_zero_rate_fails_closed_with_500() {
    // A zero directional rate cannot be inverted; the request must fail
    // before signing (previously a zero would have been signed, leaving
    // the strategy's greater-than(price 0) guard as the only backstop).
    // Without `allowFailure` one bad symbol still 500s the whole batch;
    // the per-item behaviour behind the flag is pinned in
    // `test_v1_zero_rate_item_is_internal_error_slot_with_flag` and its
    // pair-bound twin.
    let app = test_app_asymmetric("0", "99").await;
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v1")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 500, "zero rate must fail closed, not sign");
}

#[tokio::test]
async fn test_v5_endpoint_signs_floored_quote_expiry() {
    // Route-level pin for /context/v5. Every other v5 test exercises the
    // context BUILDER — a typo routing the v5 handler through
    // PairSchema::V4 would pass all of them. This one hits the endpoint
    // and asserts the full nine-slot shape plus the expiry value derived
    // from the seeded quote, with a realistic expiry (the shared harness
    // seeds i64::MAX, which would hide a ms/s confusion).
    let signer = Signer::new(TEST_KEY).unwrap();
    let registry = TokenRegistry::new(vec![(WCOIN.to_string(), "COIN".to_string())], USDC).unwrap();
    let mut quote = fake_quote("COIN", WCOIN, "100", "100");
    // 1_700_000_020_500 ms floors to 1_700_000_020 s — the trailing
    // 500ms must be dropped, never rounded up past the model's horizon.
    quote.expiry_unix_ms = 1_700_000_020_500;
    let pricing = LiveClient::with_seeded(vec![quote]).await;
    let metrics = MetricsHandle::install().expect("metrics install");
    let state = AppState::new(
        signer,
        registry,
        pricing,
        vec!["COIN".to_string()],
        fixed_close_market_hours().await,
        metrics,
    );
    let app = create_app(state);

    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v5")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(responses.len(), 1);
    let ctx = &responses[0].context;
    assert_eq!(ctx.len(), 9, "v5 endpoint must emit 9 context elements");

    let version = Float::from(alloy::primitives::B256::from(ctx[0]));
    assert_eq!(version.format().unwrap(), "5", "slot 0 must be schema v5");

    // Slot 8: the seeded expiry, ms floored to whole seconds. Compare
    // Float-canonical forms (large ints format in scientific notation).
    let expiry = Float::from(alloy::primitives::B256::from(ctx[8]));
    let expected = Float::parse("1700000020".to_string())
        .unwrap()
        .format()
        .unwrap();
    assert_eq!(expiry.format().unwrap(), expected);
}

/// Build a test app whose single COIN quote carries the given NAV
/// ratio and a realistic expiry. Used by the /context/v6 route tests.
async fn test_app_with_nav_ratio(nav_ratio: WireU256) -> axum::Router {
    let signer = Signer::new(TEST_KEY).unwrap();
    let registry = TokenRegistry::new(vec![(WCOIN.to_string(), "COIN".to_string())], USDC).unwrap();
    let mut quote = fake_quote("COIN", WCOIN, "0.01", "100");
    quote.expiry_unix_ms = 1_700_000_020_500;
    quote.nav_ratio = nav_ratio;
    let pricing = LiveClient::with_seeded(vec![quote]).await;
    let metrics = MetricsHandle::install().expect("metrics install");
    let state = AppState::new(
        signer,
        registry,
        pricing,
        vec!["COIN".to_string()],
        fixed_close_market_hours().await,
        metrics,
    );
    create_app(state)
}

/// POST a single (USDC -> WCOIN) request to `endpoint` and return the
/// signed context of the one response.
async fn context_of(app: axum::Router, endpoint: &str) -> Vec<FixedBytes<32>> {
    response_of(app, endpoint).await.context
}

#[tokio::test]
async fn test_v6_endpoint_signs_nav_ratio_at_slot_9() {
    // Route-level pin for /context/v6: the full ten-slot shape, with
    // the quote's NAV ratio at slot 9 as a Rain Float — the lossless
    // packing of the raw 18-decimal fixed-point uint256 off the pricing
    // wire, never through an f64 or a decimal string.
    let nav = nav_ratio_pattern();
    let app = test_app_with_nav_ratio(nav).await;
    let ctx = context_of(app, "/context/v6").await;

    assert_eq!(ctx.len(), 10, "v6 endpoint must emit 10 context elements");

    let version = Float::from(alloy::primitives::B256::from(ctx[0]));
    assert_eq!(version.format().unwrap(), "6", "slot 0 must be schema v6");

    // Slot 8 keeps the v5 expiry semantics: ms floored to whole seconds.
    let expiry = Float::from(alloy::primitives::B256::from(ctx[8]));
    let expected = Float::parse("1700000020".to_string())
        .unwrap()
        .format()
        .unwrap();
    assert_eq!(expiry.format().unwrap(), expected);

    // Slot 9: numerically equal to raw / 10^18 with all 18 fractional
    // digits intact — the same value model as the on-chain
    // `erc4626-convert-to-assets` word the strategy compares against...
    let nav_float = Float::from(alloy::primitives::B256::from(ctx[9]));
    let expected_nav = Float::parse("1.007813592910771427".to_string()).unwrap();
    assert!(
        nav_float.eq(expected_nav).unwrap(),
        "slot 9 must equal 1.007813592910771427, got {}",
        nav_float.format().unwrap()
    );

    // ...and losslessly so: unpacking back to 18-decimal fixed point
    // recovers the exact uint256 the pricing quote carried.
    assert_eq!(
        nav_float.to_fixed_decimal(18).unwrap(),
        U256::from(NAV_RATIO_RAW),
        "slot 9 must round-trip to the exact raw ratio"
    );
}

#[tokio::test]
async fn test_v6_zero_nav_ratio_signs_float_zero() {
    // Zero is the "no ratio" sentinel (non-vault base token / producer
    // predates nav_ratio): the carrier packs it as Float zero instead
    // of erroring, and no downstream settlement assertion applies.
    let app = test_app_with_nav_ratio(WireU256::ZERO).await;
    let ctx = context_of(app, "/context/v6").await;
    assert_eq!(ctx.len(), 10);
    let nav_float = Float::from(alloy::primitives::B256::from(ctx[9]));
    assert!(
        nav_float.is_zero().unwrap(),
        "zero sentinel must sign as Float zero, got {}",
        nav_float.format().unwrap()
    );
}

#[tokio::test]
async fn test_v5_response_is_v6_minus_nav_ratio() {
    // v5 must be untouched by the v6 addition even when the underlying
    // quote carries a NAV ratio: same nine slots as before, and every
    // slot except the schema version identical to the v6 response built
    // from the same cached quote. Anyone diffing the two endpoints sees
    // one appended slot and nothing else.
    let app = test_app_with_nav_ratio(nav_ratio_pattern()).await;
    let v5 = context_of(app.clone(), "/context/v5").await;
    let v6 = context_of(app, "/context/v6").await;

    assert_eq!(v5.len(), 9, "v5 must still emit 9 context elements");
    assert_eq!(v6.len(), 10);
    let v5_version = Float::from(alloy::primitives::B256::from(v5[0]));
    assert_eq!(v5_version.format().unwrap(), "5");
    assert_eq!(&v5[1..9], &v6[1..9], "v6 must extend v5 without changes");
}

/// Build a test app whose single COIN quote prices the vault share at
/// `vault_px` USDC and the underlying stock at a DISTINCT `underlying_px`,
/// so v7's slot 1 (underlying) can be told apart from v5/v6's slot 1
/// (vault rate). Per the pricing-types contract each directional rate is
/// seeded so a QuoteToBase (sell-side, USDC->WCOIN) request serves the
/// price back after the oracle's `inv`: `quote_to_base = 1/px`,
/// `base_to_quote = px`.
async fn test_app_with_underlying(vault_px: &str, underlying_px: &str) -> axum::Router {
    let signer = Signer::new(TEST_KEY).unwrap();
    let registry = TokenRegistry::new(vec![(WCOIN.to_string(), "COIN".to_string())], USDC).unwrap();

    let vault_inv = Float::parse(vault_px.to_string())
        .unwrap()
        .inv()
        .unwrap()
        .format()
        .unwrap();
    let mut quote = fake_quote("COIN", WCOIN, &vault_inv, vault_px);
    quote.expiry_unix_ms = 1_700_000_020_500;

    let under_inv = Float::parse(underlying_px.to_string())
        .unwrap()
        .inv()
        .unwrap()
        .format()
        .unwrap();
    quote.underlying_rate_quote_to_base = wire_float_of(&under_inv);
    quote.underlying_rate_base_to_quote = wire_float_of(underlying_px);

    let pricing = LiveClient::with_seeded(vec![quote]).await;
    let metrics = MetricsHandle::install().expect("metrics install");
    let state = AppState::new(
        signer,
        registry,
        pricing,
        vec!["COIN".to_string()],
        fixed_close_market_hours().await,
        metrics,
    );
    create_app(state)
}

#[tokio::test]
async fn test_v7_endpoint_signs_underlying_price_at_slot_1() {
    // Route-level pin for /context/v7: the v5 nine-slot shape (no NAV
    // ratio at slot 9), with slot 1 carrying the UNDERLYING price (90),
    // NOT the vault-share rate (100). Sell-side request (USDC->WCOIN).
    let app = test_app_with_underlying("100", "90").await;
    let ctx = context_of(app, "/context/v7").await;

    assert_eq!(
        ctx.len(),
        9,
        "v7 endpoint must emit 9 context elements — no NAV-ratio slot"
    );

    let version = Float::from(alloy::primitives::B256::from(ctx[0]));
    assert_eq!(version.format().unwrap(), "7", "slot 0 must be schema v7");

    let price = Float::from(alloy::primitives::B256::from(ctx[1]));
    assert_eq!(
        price.format().unwrap(),
        "90",
        "slot 1 must be the underlying price, not the vault rate (100)"
    );

    // Slot 8 keeps the v5 expiry semantics: ms floored to whole seconds.
    let expiry = Float::from(alloy::primitives::B256::from(ctx[8]));
    let expected = Float::parse("1700000020".to_string())
        .unwrap()
        .format()
        .unwrap();
    assert_eq!(expiry.format().unwrap(), expected);
}

#[tokio::test]
async fn test_v5_v6_still_sign_vault_rate_when_underlying_differs() {
    // The v7 underlying path must not leak into v4/v5/v6: with the
    // underlying (90) distinct from the vault rate (100), the vault-rate
    // endpoints must still serve 100 at slot 1. Regression pin proving
    // slot 1's meaning is per-schema.
    let app = test_app_with_underlying("100", "90").await;
    let v5 = context_of(app.clone(), "/context/v5").await;
    let v6 = context_of(app, "/context/v6").await;

    let v5_price = Float::from(alloy::primitives::B256::from(v5[1]));
    assert_eq!(
        v5_price.format().unwrap(),
        "100",
        "v5 slot 1 is the vault rate"
    );
    let v6_price = Float::from(alloy::primitives::B256::from(v6[1]));
    assert_eq!(
        v6_price.format().unwrap(),
        "100",
        "v6 slot 1 is the vault rate"
    );
}

#[tokio::test]
async fn test_v7_fails_closed_on_absent_underlying_rate() {
    // A quote from a producer predating the underlying_rate_* fields
    // decodes them to the all-zero sentinel. v7 must refuse to sign a
    // zero underlying price — without `allowFailure` one bad symbol 500s
    // the request; with it, the item is an `internal_error` slot (see
    // `test_v7_absent_underlying_rate_is_per_item_with_flag`) — rather
    // than hand the strategy a garbage mark. The vault rate is present and
    // valid, so v5/v6 would happily serve; only v7 fails closed.
    let signer = Signer::new(TEST_KEY).unwrap();
    let registry = TokenRegistry::new(vec![(WCOIN.to_string(), "COIN".to_string())], USDC).unwrap();
    let mut quote = fake_quote("COIN", WCOIN, "0.01", "100");
    quote.underlying_rate_quote_to_base = WireFloat::from_bytes([0u8; 32]);
    quote.underlying_rate_base_to_quote = WireFloat::from_bytes([0u8; 32]);
    let pricing = LiveClient::with_seeded(vec![quote]).await;
    let metrics = MetricsHandle::install().expect("metrics install");
    let state = AppState::new(
        signer,
        registry,
        pricing,
        vec!["COIN".to_string()],
        fixed_close_market_hours().await,
        metrics,
    );
    let app = create_app(state);

    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v7")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        500,
        "absent underlying rate must fail closed, not sign a zero price"
    );
}

// ---------------------------------------------------------------------------
// Cross-frame signature reuse (v5/v6): an unchanged price under a new frame
// is answered with the previous, still-valid signature.

/// App plus a handle on its pricing cache so a test can push new frames.
async fn reuse_test_app(reuse_min_remaining_secs: u64) -> (axum::Router, LiveClient) {
    let signer = Signer::new(TEST_KEY).unwrap();
    let registry = TokenRegistry::new(vec![(WCOIN.to_string(), "COIN".to_string())], USDC).unwrap();
    let pricing = LiveClient::with_seeded(vec![]).await;
    let metrics = MetricsHandle::install().expect("metrics install");
    let state = AppState::new(
        signer,
        registry,
        pricing.clone(),
        vec!["COIN".to_string()],
        always_in_session_market_hours().await,
        metrics,
    )
    .with_signature_reuse(reuse_min_remaining_secs);
    (create_app(state), pricing)
}

/// A COIN frame at `price` USDC, stamped `source_ts_secs`, expiring
/// `expires_in_secs` after NOW (the reuse margin is measured against the
/// wall clock, so the expiry must be too).
fn frame(price: &str, source_ts_secs: i64, expires_in_secs: i64) -> Quote {
    let inv = Float::parse(price.to_string())
        .unwrap()
        .inv()
        .unwrap()
        .format()
        .unwrap();
    let mut q = fake_quote("COIN", WCOIN, &inv, price);
    q.source_ts_unix_ms = source_ts_secs * 1000;
    q.expiry_unix_ms = (Utc::now().timestamp() + expires_in_secs) * 1000;
    q
}

async fn response_of(app: axum::Router, endpoint: &str) -> OracleResponse {
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(endpoint)
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let mut responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(responses.len(), 1);
    responses.pop().unwrap()
}

/// Slot 2 (publish_time) rendered the same way a `Float::parse` of the
/// expected seconds would render, so tests compare strings, not floats.
fn publish_time_of(r: &OracleResponse) -> String {
    Float::from(B256::from(r.context[2])).format().unwrap()
}

fn secs(s: i64) -> String {
    Float::parse(s.to_string()).unwrap().format().unwrap()
}

#[tokio::test]
async fn test_v5_unchanged_price_reuses_previous_signature() {
    let (app, pricing) = reuse_test_app(10).await;
    pricing.seed(frame("100", FIXED_PUBLISH_TIME, 60)).await;
    let first = response_of(app.clone(), "/context/v5").await;
    assert_eq!(publish_time_of(&first), secs(FIXED_PUBLISH_TIME));

    // Next frame, five seconds later, same price: the previous signature
    // is still good for ~60s, so it is served again unchanged.
    pricing.seed(frame("100", FIXED_PUBLISH_TIME + 5, 60)).await;
    let second = response_of(app.clone(), "/context/v5").await;
    assert_eq!(
        second.context, first.context,
        "same bytes, publish_time not advanced"
    );
    assert_eq!(second.signature, first.signature);

    // The price moves: a fresh signature on the new frame.
    pricing
        .seed(frame("101", FIXED_PUBLISH_TIME + 10, 60))
        .await;
    let third = response_of(app, "/context/v5").await;
    assert_eq!(publish_time_of(&third), secs(FIXED_PUBLISH_TIME + 10));
    assert_ne!(third.context[1], first.context[1], "price slot moved");
    assert_ne!(third.signature, first.signature);
}

#[tokio::test]
async fn test_v5_near_expiry_quote_is_not_reused() {
    let (app, pricing) = reuse_test_app(10).await;
    // Only 6s of validity left: under the 10s margin, so never reused.
    pricing.seed(frame("100", FIXED_PUBLISH_TIME, 6)).await;
    let first = response_of(app.clone(), "/context/v5").await;

    pricing.seed(frame("100", FIXED_PUBLISH_TIME + 5, 6)).await;
    let second = response_of(app, "/context/v5").await;
    assert_eq!(publish_time_of(&second), secs(FIXED_PUBLISH_TIME + 5));
    assert_ne!(second.context, first.context);
}

#[tokio::test]
async fn test_v5_shorter_expiry_on_new_frame_is_not_reused() {
    // Same price, but pricing has pulled the horizon in (a recalibrated
    // profile, or a mark it no longer stands behind). The stored 60s
    // signature would still clear the margin, yet serving it would vouch
    // for the price past the point the producer disowned it.
    let (app, pricing) = reuse_test_app(10).await;
    pricing.seed(frame("100", FIXED_PUBLISH_TIME, 60)).await;
    let first = response_of(app.clone(), "/context/v5").await;

    pricing.seed(frame("100", FIXED_PUBLISH_TIME + 5, 15)).await;
    let second = response_of(app.clone(), "/context/v5").await;
    assert_eq!(publish_time_of(&second), secs(FIXED_PUBLISH_TIME + 5));
    assert_ne!(
        second.context[8], first.context[8],
        "the shorter expiry is signed"
    );

    // The horizon opens back up: the 15s signature is the one on file
    // and still fits inside the new frame, so it is reused.
    pricing
        .seed(frame("100", FIXED_PUBLISH_TIME + 10, 60))
        .await;
    let third = response_of(app, "/context/v5").await;
    assert_eq!(third.context, second.context);
}

#[tokio::test]
async fn test_v4_never_reuses_across_frames() {
    // v4 signs no expiry, so the taker cannot see how old a reused quote
    // would be; it always gets the newest frame.
    let (app, pricing) = reuse_test_app(10).await;
    pricing.seed(frame("100", FIXED_PUBLISH_TIME, 60)).await;
    let first = response_of(app.clone(), "/context/v4").await;
    pricing.seed(frame("100", FIXED_PUBLISH_TIME + 5, 60)).await;
    let second = response_of(app, "/context/v4").await;
    assert_eq!(publish_time_of(&second), secs(FIXED_PUBLISH_TIME + 5));
    assert_ne!(second.context, first.context);
}

#[tokio::test]
async fn test_reuse_disabled_signs_every_frame() {
    let (app, pricing) = reuse_test_app(0).await;
    pricing.seed(frame("100", FIXED_PUBLISH_TIME, 60)).await;
    let first = response_of(app.clone(), "/context/v5").await;
    pricing.seed(frame("100", FIXED_PUBLISH_TIME + 5, 60)).await;
    let second = response_of(app, "/context/v5").await;
    assert_eq!(publish_time_of(&second), secs(FIXED_PUBLISH_TIME + 5));
    assert_ne!(second.context, first.context);
}

#[tokio::test]
async fn test_v6_reuse_requires_same_nav_ratio() {
    let (app, pricing) = reuse_test_app(10).await;
    let mut q = frame("100", FIXED_PUBLISH_TIME, 60);
    q.nav_ratio = nav_ratio_pattern();
    pricing.seed(q).await;
    let first = response_of(app.clone(), "/context/v6").await;

    // Same price, new frame, same ratio: reused.
    let mut q = frame("100", FIXED_PUBLISH_TIME + 5, 60);
    q.nav_ratio = nav_ratio_pattern();
    pricing.seed(q).await;
    let second = response_of(app.clone(), "/context/v6").await;
    assert_eq!(second.context, first.context);

    // Same price, ratio changed: the ratio is signed at slot 9, so re-sign.
    let mut q = frame("100", FIXED_PUBLISH_TIME + 10, 60);
    q.nav_ratio = WireU256::from_bytes(U256::from(NAV_RATIO_RAW + 1).to_be_bytes());
    pricing.seed(q).await;
    let third = response_of(app, "/context/v6").await;
    assert_eq!(publish_time_of(&third), secs(FIXED_PUBLISH_TIME + 10));
    assert_ne!(third.context[9], first.context[9]);
}

/// Wire-level pin for the error body shape. `AppError::into_response`
/// is the single funnel for every non-2xx `/context/v*` reply, and the
/// batch envelope (`allowFailure=true`) reuses the same body per failed
/// item, so downstream clients will key on `error`. Hit all three
/// status paths through the real router and check the code strings and
/// the JSON content type, not just the status.
#[tokio::test]
async fn test_error_body_shape_is_stable_across_status_codes() {
    async fn post(app: axum::Router, body: Bytes) -> (u16, String, ErrorResponse) {
        let resp = app
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
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: ErrorResponse = serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("error body must be {{error, detail}}: {e}: {bytes:?}"));
        (status, content_type, body)
    }

    // 400: body is not ABI-decodable as either request shape.
    let (status, content_type, body) = post(test_app().await, Bytes::from_static(b"nope")).await;
    assert_eq!(status, 400);
    assert!(
        content_type.starts_with("application/json"),
        "{content_type}"
    );
    assert_eq!(body.error, "bad_request");
    assert!(
        body.detail.contains("Invalid ABI-encoded body"),
        "{}",
        body.detail
    );

    // 503: configured symbol with no live quote yet.
    let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;
    let (status, content_type, body) = post(app, encode_single(USDC, WDRAM)).await;
    assert_eq!(status, 503);
    assert!(
        content_type.starts_with("application/json"),
        "{content_type}"
    );
    assert_eq!(body.error, "service_unavailable");
    assert!(body.detail.contains("DRAM"), "{}", body.detail);

    // 500: zero directional rate cannot be inverted, fails closed.
    let (status, content_type, body) = post(
        test_app_asymmetric("0", "99").await,
        encode_single(USDC, WCOIN),
    )
    .await;
    assert_eq!(status, 500);
    assert!(
        content_type.starts_with("application/json"),
        "{content_type}"
    );
    assert_eq!(body.error, "internal_error");
    assert!(!body.detail.is_empty());
}

/// Query-string handling must never reject an otherwise valid oracle
/// request: unknown keys, garbage, and the `allowFailure` flag on a
/// SINGLE-tuple body all leave the response exactly as it is without a
/// query string. (Batch + flag is pinned separately once the envelope
/// path lands.)
#[tokio::test]
async fn test_query_string_never_changes_single_tuple_responses() {
    async fn post(app: axum::Router, uri: &str, body: Bytes) -> (u16, serde_json::Value) {
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/octet-stream")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    for endpoint in [
        "/context/v1",
        "/context/v4",
        "/context/v5",
        "/context/v6",
        "/context/v7",
    ] {
        // Happy path: the flag must NOT envelope a single tuple.
        let app = test_app().await;
        let (base_status, base_body) =
            post(app.clone(), endpoint, encode_single(USDC, WCOIN)).await;
        assert_eq!(base_status, 200, "{endpoint}");
        let responses: Vec<OracleResponse> = serde_json::from_value(base_body.clone()).unwrap();
        assert_eq!(responses.len(), 1, "{endpoint}");

        for query in [
            "?allowFailure=true",
            "?allowFailure=1",
            "?allowFailure=false",
            "?foo=bar",
            "?%%%&==",
            "?allowFailure=true&foo=bar",
        ] {
            let uri = format!("{endpoint}{query}");
            let (status, body) = post(app.clone(), &uri, encode_single(USDC, WCOIN)).await;
            assert_eq!(status, 200, "{uri}");
            // Same shape: a bare one-element array of OracleResponse, not
            // an envelope. Signatures are deterministic for a fixed key +
            // fixed context, so the body should be identical too.
            assert_eq!(body, base_body, "{uri} must equal the no-query response");
        }

        // Error path: a single tuple still gets the whole-request error
        // with the flag set — never a 200 with an error item.
        let uri = format!("{endpoint}?allowFailure=true");
        let (status, body) = post(
            test_app().await,
            &uri,
            encode_single(
                "0x9999999999999999999999999999999999999999",
                "0x8888888888888888888888888888888888888888",
            ),
        )
        .await;
        assert_eq!(status, 400, "{uri}");
        let err: ErrorResponse = serde_json::from_value(body).unwrap();
        assert_eq!(err.error, "bad_request", "{uri}");
    }
}

/// Batch bodies WITHOUT the flag (absent, `false`, unknown keys, garbage)
/// must keep the all-or-nothing behaviour byte-for-byte: a healthy batch
/// returns the bare array, and a batch with one bad item returns the
/// whole-request error. This is the compatibility promise to deployed
/// Raindex clients that batch in normal mode, and it must hold both
/// before and after the envelope path lands.
#[tokio::test]
async fn test_batch_without_flag_keeps_all_or_nothing_behaviour() {
    async fn post(app: axum::Router, uri: &str, body: Bytes) -> (u16, serde_json::Value) {
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/octet-stream")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    let non_flag_queries = [
        "",
        "?allowFailure=false",
        "?allowFailure=0",
        "?allowFailure=nonsense",
        "?foo=bar",
        "?%%%&==",
    ];

    for endpoint in ["/context/v1", "/context/v7"] {
        // Healthy batch: bare array, identical across non-flag queries.
        let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;
        let healthy = encode_batch(&[(USDC, WCOIN), (WCOIN, USDC)]);
        let (status, base) = post(app.clone(), endpoint, healthy.clone()).await;
        assert_eq!(status, 200, "{endpoint}");
        let responses: Vec<OracleResponse> = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(responses.len(), 2, "{endpoint}");
        for query in non_flag_queries {
            let uri = format!("{endpoint}{query}");
            let (status, body) = post(app.clone(), &uri, healthy.clone()).await;
            assert_eq!(status, 200, "{uri}");
            assert_eq!(body, base, "{uri} must equal the no-query response");
        }

        // Mixed batch (valid, no-quote, valid): whole request fails with
        // the first item's error, and no envelope appears.
        let mixed = encode_batch(&[(USDC, WCOIN), (USDC, WDRAM), (WCOIN, USDC)]);
        for query in non_flag_queries {
            let uri = format!("{endpoint}{query}");
            let (status, body) = post(app.clone(), &uri, mixed.clone()).await;
            assert_eq!(status, 503, "{uri}: one uncached item must 503 the batch");
            let err: ErrorResponse = serde_json::from_value(body).unwrap();
            assert_eq!(err.error, "service_unavailable", "{uri}");
            assert!(err.detail.contains("DRAM"), "{uri}: {}", err.detail);
        }
    }
}

/// Shared helper for the envelope tests: POST and return status + JSON.
async fn post_json(app: axum::Router, uri: &str, body: Bytes) -> (u16, serde_json::Value) {
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// The point of the whole change: a batch with `allowFailure=true` is
/// `200` with one slot per request item, in request order, and the
/// failing items do not take the healthy ones down. Failure kinds
/// covered: a resolution error (unknown token → bad_request) and a
/// cache miss (no live quote → service_unavailable). The ok slots must
/// be byte-identical to what a strict request for the same pair signs.
#[tokio::test]
async fn test_v1_batch_with_flag_returns_per_item_envelope() {
    let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;
    let unknown = "0x9999999999999999999999999999999999999999";

    // Reference: strict single responses for the two healthy pairs.
    let (_, buy_ref) = post_json(app.clone(), "/context/v1", encode_single(USDC, WCOIN)).await;
    let (_, sell_ref) = post_json(app.clone(), "/context/v1", encode_single(WCOIN, USDC)).await;

    let mixed = encode_batch(&[(USDC, WCOIN), (USDC, WDRAM), (USDC, unknown), (WCOIN, USDC)]);
    let (status, body) = post_json(app.clone(), "/context/v1?allowFailure=true", mixed).await;
    assert_eq!(
        status, 200,
        "envelope mode must be 200 even with failures: {body}"
    );
    let items: Vec<BatchItemResponse> = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(items.len(), 4, "one slot per request item: {body}");

    match &items[0] {
        BatchItemResponse::Ok(r) => {
            assert_eq!(
                serde_json::to_value(r).unwrap(),
                buy_ref[0],
                "slot 0 = strict buy"
            )
        }
        other => panic!("slot 0 should be ok, got {other:?}"),
    }
    match &items[1] {
        BatchItemResponse::Error(e) => {
            assert_eq!(e.error, "service_unavailable");
            assert!(e.detail.contains("DRAM"), "{}", e.detail);
        }
        other => panic!("slot 1 should be service_unavailable, got {other:?}"),
    }
    match &items[2] {
        BatchItemResponse::Error(e) => assert_eq!(e.error, "bad_request"),
        other => panic!("slot 2 should be bad_request, got {other:?}"),
    }
    match &items[3] {
        BatchItemResponse::Ok(r) => {
            assert_eq!(
                serde_json::to_value(r).unwrap(),
                sell_ref[0],
                "slot 3 = strict sell"
            )
        }
        other => panic!("slot 3 should be ok, got {other:?}"),
    }

    // Wire shape sanity, independent of the Rust type: every slot has
    // exactly `status` + `body`.
    for slot in body.as_array().unwrap() {
        let keys: Vec<_> = slot.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["status", "body"], "{slot}");
    }
}

/// A batch where nothing can be signed is still `200` in envelope mode,
/// with every slot an error — the client decides what to do, the server
/// does not escalate to a whole-request error.
#[tokio::test]
async fn test_v1_batch_with_flag_all_failed_is_200_with_error_slots() {
    let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;
    let unknown = "0x9999999999999999999999999999999999999999";
    let body = encode_batch(&[(USDC, WDRAM), (USDC, unknown)]);
    let (status, json) = post_json(app, "/context/v1?allowFailure=true", body).await;
    assert_eq!(status, 200);
    let items: Vec<BatchItemResponse> = serde_json::from_value(json).unwrap();
    assert_eq!(items.len(), 2);
    let codes: Vec<&str> = items
        .iter()
        .map(|i| match i {
            BatchItemResponse::Error(e) => e.error.as_str(),
            BatchItemResponse::Ok(_) => "ok",
        })
        .collect();
    assert_eq!(codes, vec!["service_unavailable", "bad_request"]);
}

/// An empty batch with the flag is an empty array — same as without.
#[tokio::test]
async fn test_v1_empty_batch_with_flag_returns_empty_array() {
    let (status, json) = post_json(
        test_app().await,
        "/context/v1?allowFailure=true",
        encode_batch(&[]),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json, serde_json::json!([]));
}

/// The build-phase failure (zero rate → fail closed) is per-item too: it
/// lands as `internal_error` in its slot and the other direction, which
/// has a healthy rate, still signs. Without the flag the same batch is
/// the historical whole-request 500.
#[tokio::test]
async fn test_v1_zero_rate_item_is_internal_error_slot_with_flag() {
    let app = test_app_asymmetric("0", "99").await;
    let body = encode_batch(&[(USDC, WCOIN), (WCOIN, USDC)]);

    let (status, json) =
        post_json(app.clone(), "/context/v1?allowFailure=true", body.clone()).await;
    assert_eq!(status, 200, "{json}");
    let items: Vec<BatchItemResponse> = serde_json::from_value(json).unwrap();
    assert!(
        matches!(&items[0], BatchItemResponse::Error(e) if e.error == "internal_error"),
        "{:?}",
        items[0]
    );
    assert!(
        matches!(&items[1], BatchItemResponse::Ok(_)),
        "{:?}",
        items[1]
    );

    let (status, json) = post_json(app, "/context/v1", body).await;
    assert_eq!(
        status, 500,
        "strict mode keeps the whole-request 500: {json}"
    );
}

/// A one-element ARRAY is a batch, so it is envelope-eligible — unlike a
/// bare single tuple for the same pair, which stays strict.
#[tokio::test]
async fn test_v1_one_element_array_with_flag_is_enveloped_but_single_tuple_is_not() {
    let app = test_app().await;
    let (status, json) = post_json(
        app.clone(),
        "/context/v1?allowFailure=true",
        encode_batch(&[(USDC, WCOIN)]),
    )
    .await;
    assert_eq!(status, 200);
    let items: Vec<BatchItemResponse> = serde_json::from_value(json).unwrap();
    assert_eq!(items.len(), 1);
    assert!(matches!(items[0], BatchItemResponse::Ok(_)));

    let (status, json) = post_json(
        app,
        "/context/v1?allowFailure=true",
        encode_single(USDC, WCOIN),
    )
    .await;
    assert_eq!(status, 200);
    let plain: Vec<OracleResponse> = serde_json::from_value(json).unwrap();
    assert_eq!(plain.len(), 1);
}

/// `/metrics` distinguishes a partially served envelope from a clean one.
#[tokio::test]
async fn test_v1_partial_envelope_is_counted_as_partial_outcome() {
    let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;
    let mixed = encode_batch(&[(USDC, WCOIN), (USDC, WDRAM)]);
    let (status, _) = post_json(app.clone(), "/context/v1?allowFailure=true", mixed).await;
    assert_eq!(status, 200);

    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let text = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    let line = text
        .lines()
        .find(|l| {
            l.starts_with("oracle_context_request_total{")
                && l.contains(r#"endpoint="v1""#)
                && l.contains(r#"outcome="partial""#)
        })
        .unwrap_or_else(|| panic!("no partial outcome sample for v1 in:\n{text}"));
    let count: f64 = line.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(count >= 1.0, "{line}");
}

/// Strict mode's historical error ordering: every item is RESOLVED
/// before any is BUILT, so a resolution failure (unknown token → 400)
/// in a later slot wins over a cache miss (→ 503) in an earlier slot.
/// The per-item rewrite must keep this, or a deployed client would see
/// a different status for the same batch. Envelope mode, by contrast,
/// reports both in their own slots.
#[tokio::test]
async fn test_v1_strict_batch_resolve_error_wins_over_earlier_build_error() {
    let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;
    let unknown = "0x9999999999999999999999999999999999999999";
    // slot 0: resolves, but no live quote (build-phase 503)
    // slot 1: unknown token (resolve-phase 400)
    let body = encode_batch(&[(USDC, WDRAM), (USDC, unknown)]);

    let (status, json) = post_json(app.clone(), "/context/v1", body.clone()).await;
    assert_eq!(status, 400, "resolve error must win in strict mode: {json}");
    let err: ErrorResponse = serde_json::from_value(json).unwrap();
    assert_eq!(err.error, "bad_request");

    let (status, json) = post_json(app, "/context/v1?allowFailure=true", body).await;
    assert_eq!(status, 200);
    let items: Vec<BatchItemResponse> = serde_json::from_value(json).unwrap();
    assert!(matches!(&items[0], BatchItemResponse::Error(e) if e.error == "service_unavailable"));
    assert!(matches!(&items[1], BatchItemResponse::Error(e) if e.error == "bad_request"));
}

const PAIR_BOUND_ENDPOINTS: [&str; 4] =
    ["/context/v4", "/context/v5", "/context/v6", "/context/v7"];

/// Pair-bound schemas share one pipeline, so one route-level sweep pins
/// all four: a batch with `allowFailure=true` is `200`, one slot per
/// item in request order, ok slots byte-identical to the strict single
/// response for the same pair (so the schema-specific slots 6/7/8/9 are
/// intact), failed slots carrying the per-item error code.
#[tokio::test]
async fn test_pair_bound_batch_with_flag_returns_per_item_envelope() {
    let unknown = "0x9999999999999999999999999999999999999999";
    for endpoint in PAIR_BOUND_ENDPOINTS {
        let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;
        let (_, buy_ref) = post_json(app.clone(), endpoint, encode_single(USDC, WCOIN)).await;
        let (_, sell_ref) = post_json(app.clone(), endpoint, encode_single(WCOIN, USDC)).await;

        let mixed = encode_batch(&[(USDC, WCOIN), (USDC, WDRAM), (USDC, unknown), (WCOIN, USDC)]);
        let uri = format!("{endpoint}?allowFailure=true");
        let (status, body) = post_json(app.clone(), &uri, mixed).await;
        assert_eq!(status, 200, "{uri}: {body}");
        let items: Vec<BatchItemResponse> = serde_json::from_value(body.clone()).unwrap();
        assert_eq!(items.len(), 4, "{uri}: {body}");

        match &items[0] {
            BatchItemResponse::Ok(r) => {
                assert_eq!(serde_json::to_value(r).unwrap(), buy_ref[0], "{uri} slot 0")
            }
            other => panic!("{uri} slot 0 should be ok, got {other:?}"),
        }
        match &items[1] {
            BatchItemResponse::Error(e) => {
                assert_eq!(e.error, "service_unavailable", "{uri}");
                assert!(e.detail.contains("DRAM"), "{uri}: {}", e.detail);
            }
            other => panic!("{uri} slot 1 should be service_unavailable, got {other:?}"),
        }
        match &items[2] {
            BatchItemResponse::Error(e) => assert_eq!(e.error, "bad_request", "{uri}"),
            other => panic!("{uri} slot 2 should be bad_request, got {other:?}"),
        }
        match &items[3] {
            BatchItemResponse::Ok(r) => {
                assert_eq!(
                    serde_json::to_value(r).unwrap(),
                    sell_ref[0],
                    "{uri} slot 3"
                )
            }
            other => panic!("{uri} slot 3 should be ok, got {other:?}"),
        }
        for slot in body.as_array().unwrap() {
            let keys: Vec<_> = slot.as_object().unwrap().keys().collect();
            assert_eq!(keys, vec!["status", "body"], "{uri}: {slot}");
        }
    }
}

#[tokio::test]
async fn test_pair_bound_all_failed_and_empty_batches_with_flag() {
    let unknown = "0x9999999999999999999999999999999999999999";
    for endpoint in PAIR_BOUND_ENDPOINTS {
        let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;
        let uri = format!("{endpoint}?allowFailure=true");

        let (status, json) = post_json(
            app.clone(),
            &uri,
            encode_batch(&[(USDC, WDRAM), (USDC, unknown)]),
        )
        .await;
        assert_eq!(status, 200, "{uri}");
        let items: Vec<BatchItemResponse> = serde_json::from_value(json).unwrap();
        let codes: Vec<&str> = items
            .iter()
            .map(|i| match i {
                BatchItemResponse::Error(e) => e.error.as_str(),
                BatchItemResponse::Ok(_) => "ok",
            })
            .collect();
        assert_eq!(codes, vec!["service_unavailable", "bad_request"], "{uri}");

        let (status, json) = post_json(app, &uri, encode_batch(&[])).await;
        assert_eq!(status, 200, "{uri}");
        assert_eq!(json, serde_json::json!([]), "{uri}");
    }
}

/// Build-phase failure per item on the pair-bound path. The fixture's
/// underlying rates mirror the vault rates, so a zero quote_to_base
/// fails the buy direction on v4/v5/v6 (vault rate) AND v7 (underlying
/// rate) alike, while the sell direction signs. Strict stays 500.
#[tokio::test]
async fn test_pair_bound_zero_rate_item_is_internal_error_slot_with_flag() {
    for endpoint in PAIR_BOUND_ENDPOINTS {
        let app = test_app_asymmetric("0", "99").await;
        let body = encode_batch(&[(USDC, WCOIN), (WCOIN, USDC)]);
        let uri = format!("{endpoint}?allowFailure=true");

        let (status, json) = post_json(app.clone(), &uri, body.clone()).await;
        assert_eq!(status, 200, "{uri}: {json}");
        let items: Vec<BatchItemResponse> = serde_json::from_value(json).unwrap();
        assert!(
            matches!(&items[0], BatchItemResponse::Error(e) if e.error == "internal_error"),
            "{uri}: {:?}",
            items[0]
        );
        assert!(
            matches!(&items[1], BatchItemResponse::Ok(_)),
            "{uri}: {:?}",
            items[1]
        );

        let (status, json) = post_json(app, endpoint, body).await;
        assert_eq!(
            status, 500,
            "{endpoint} strict keeps the whole-request 500: {json}"
        );
    }
}

/// Same historical ordering pin as v1, on the shared pair-bound path.
#[tokio::test]
async fn test_pair_bound_strict_resolve_error_wins_over_earlier_build_error() {
    let unknown = "0x9999999999999999999999999999999999999999";
    for endpoint in PAIR_BOUND_ENDPOINTS {
        let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;
        let body = encode_batch(&[(USDC, WDRAM), (USDC, unknown)]);

        let (status, json) = post_json(app.clone(), endpoint, body.clone()).await;
        assert_eq!(
            status, 400,
            "{endpoint}: resolve error must win in strict mode: {json}"
        );
        let err: ErrorResponse = serde_json::from_value(json).unwrap();
        assert_eq!(err.error, "bad_request", "{endpoint}");

        let uri = format!("{endpoint}?allowFailure=true");
        let (status, json) = post_json(app, &uri, body).await;
        assert_eq!(status, 200, "{uri}");
        let items: Vec<BatchItemResponse> = serde_json::from_value(json).unwrap();
        assert!(
            matches!(&items[0], BatchItemResponse::Error(e) if e.error == "service_unavailable"),
            "{uri}"
        );
        assert!(
            matches!(&items[1], BatchItemResponse::Error(e) if e.error == "bad_request"),
            "{uri}"
        );
    }
}

/// `/metrics` labels the pair-bound endpoints by their own tag.
#[tokio::test]
async fn test_v7_partial_envelope_is_counted_as_partial_outcome() {
    let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;
    let mixed = encode_batch(&[(USDC, WCOIN), (USDC, WDRAM)]);
    let (status, _) = post_json(app.clone(), "/context/v7?allowFailure=true", mixed).await;
    assert_eq!(status, 200);

    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let text = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(
        text.lines()
            .any(|l| l.starts_with("oracle_context_request_total{")
                && l.contains(r#"endpoint="v7""#)
                && l.contains(r#"outcome="partial""#)),
        "no partial outcome sample for v7 in:\n{text}"
    );
}

const ALL_ENDPOINTS: [&str; 5] = [
    "/context/v1",
    "/context/v4",
    "/context/v5",
    "/context/v6",
    "/context/v7",
];

/// The other resolve-phase failure: an IO index outside the order's
/// `validInputs` / `validOutputs`. Per-item in envelope mode (with the
/// index-specific detail), first-error-wins in strict mode. A `U256`
/// index too large for `usize` takes the same path. Covers both the v1
/// resolver and the pair-bound `io_tokens_for`.
#[tokio::test]
async fn test_invalid_io_index_is_per_item_bad_request_with_flag() {
    fn with_indices(input: U256, output: U256) -> (OrderV4, U256, U256, Address) {
        let mut t = test_order_tuple(USDC, WCOIN);
        t.1 = input;
        t.2 = output;
        t
    }
    let batch = Bytes::from(
        vec![
            test_order_tuple(USDC, WCOIN),
            with_indices(U256::from(5u64), U256::ZERO),
            with_indices(U256::ZERO, U256::from(7u64)),
            with_indices(U256::MAX, U256::ZERO),
            test_order_tuple(WCOIN, USDC),
        ]
        .abi_encode(),
    );

    for endpoint in ALL_ENDPOINTS {
        let app = test_app().await;
        let uri = format!("{endpoint}?allowFailure=true");
        let (status, json) = post_json(app.clone(), &uri, batch.clone()).await;
        assert_eq!(status, 200, "{uri}: {json}");
        let items: Vec<BatchItemResponse> = serde_json::from_value(json).unwrap();
        assert_eq!(items.len(), 5, "{uri}");
        assert!(matches!(items[0], BatchItemResponse::Ok(_)), "{uri}");
        match &items[1] {
            BatchItemResponse::Error(e) => {
                assert_eq!(e.error, "bad_request", "{uri}");
                assert!(
                    e.detail.contains("Invalid input IO index: 5"),
                    "{uri}: {}",
                    e.detail
                );
            }
            other => panic!("{uri} slot 1: {other:?}"),
        }
        match &items[2] {
            BatchItemResponse::Error(e) => {
                assert_eq!(e.error, "bad_request", "{uri}");
                assert!(
                    e.detail.contains("Invalid output IO index: 7"),
                    "{uri}: {}",
                    e.detail
                );
            }
            other => panic!("{uri} slot 2: {other:?}"),
        }
        match &items[3] {
            BatchItemResponse::Error(e) => {
                assert_eq!(e.error, "bad_request", "{uri}");
                assert!(
                    e.detail.contains("Invalid input IO index"),
                    "{uri}: {}",
                    e.detail
                );
            }
            other => panic!("{uri} slot 3: {other:?}"),
        }
        assert!(matches!(items[4], BatchItemResponse::Ok(_)), "{uri}");

        // Strict: the first bad slot's detail is the whole-request error.
        let (status, json) = post_json(app, endpoint, batch.clone()).await;
        assert_eq!(status, 400, "{endpoint}: {json}");
        let err: ErrorResponse = serde_json::from_value(json).unwrap();
        assert_eq!(err.error, "bad_request", "{endpoint}");
        assert!(
            err.detail.contains("Invalid input IO index: 5"),
            "{endpoint}: {}",
            err.detail
        );
    }
}

/// Snapshot-once coherence survives the per-item rewrite: repeated
/// symbols in one batch are served from the same snapshot, so two
/// identical requests in the same envelope sign identical contexts.
#[tokio::test]
async fn test_repeated_symbol_in_envelope_batch_signs_identically() {
    for endpoint in ALL_ENDPOINTS {
        let app = test_app().await;
        let uri = format!("{endpoint}?allowFailure=true");
        let body = encode_batch(&[(USDC, WCOIN), (WCOIN, USDC), (USDC, WCOIN)]);
        let (status, json) = post_json(app, &uri, body).await;
        assert_eq!(status, 200, "{uri}");
        let slots = json.as_array().unwrap();
        assert_eq!(slots.len(), 3, "{uri}");
        for slot in slots {
            assert_eq!(slot["status"], "ok", "{uri}: {slot}");
        }
        assert_eq!(
            slots[0], slots[2],
            "{uri}: same pair, same snapshot, same signature"
        );
        assert_ne!(slots[0], slots[1], "{uri}: opposite direction must differ");
    }
}

/// Read one counter sample (by name + label subset) out of a Prometheus
/// exposition dump. Missing → 0, which is what a never-incremented
/// counter reads as.
fn counter_value(text: &str, name: &str, labels: &[(&str, &str)]) -> f64 {
    text.lines()
        .find(|l| {
            l.starts_with(&format!("{name}{{"))
                && labels
                    .iter()
                    .all(|(k, v)| l.contains(&format!(r#"{k}="{v}""#)))
        })
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

async fn scrape_metrics(app: axum::Router) -> String {
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

/// `oracle_context_item_total` reaches `/metrics` with the endpoint and
/// outcome labels, and moves in the right direction for every path.
///
/// The Prometheus recorder is process-global and the integration tests
/// run in parallel against it, so this test only asserts that each
/// counter grew by AT LEAST the expected amount between two scrapes —
/// another test may have added to it in between. The exact counts
/// (including the "nothing counted" cases) are pinned in the lib unit
/// test `item_counter_counts_wire_verdicts_exactly` under a thread-local
/// recorder.
#[tokio::test]
async fn test_item_counter_reaches_metrics_for_every_path() {
    let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;
    let unknown = "0x9999999999999999999999999999999999999999";
    let item = |text: &str, outcome: &str| {
        counter_value(
            text,
            "oracle_context_item_total",
            &[("endpoint", "v1"), ("outcome", outcome)],
        )
    };
    let grew_by_at_least = |before: &str, after: &str, outcome: &str, n: f64| {
        let delta = item(after, outcome) - item(before, outcome);
        assert!(delta >= n, "{outcome}: grew by {delta}, expected >= {n}");
    };

    // Envelope, mixed: every slot counted under its own code.
    let before = scrape_metrics(app.clone()).await;
    let mixed = encode_batch(&[(USDC, WCOIN), (USDC, WDRAM), (USDC, unknown), (WCOIN, USDC)]);
    let (status, _) = post_json(app.clone(), "/context/v1?allowFailure=true", mixed.clone()).await;
    assert_eq!(status, 200);
    let after = scrape_metrics(app.clone()).await;
    grew_by_at_least(&before, &after, "ok", 2.0);
    grew_by_at_least(&before, &after, "service_unavailable", 1.0);
    grew_by_at_least(&before, &after, "bad_request", 1.0);

    // Strict, mixed: the aborting error (resolve wins → bad_request).
    let before = after;
    let (status, _) = post_json(app.clone(), "/context/v1", mixed).await;
    assert_eq!(status, 400);
    let after = scrape_metrics(app.clone()).await;
    grew_by_at_least(&before, &after, "bad_request", 1.0);

    // Strict, healthy: N ok.
    let before = after;
    let (status, _) = post_json(
        app.clone(),
        "/context/v1",
        encode_batch(&[(USDC, WCOIN), (WCOIN, USDC), (USDC, WCOIN)]),
    )
    .await;
    assert_eq!(status, 200);
    let after = scrape_metrics(app.clone()).await;
    grew_by_at_least(&before, &after, "ok", 3.0);

    // Strict, single tuple failing in the build phase: one error.
    let before = after;
    let (status, _) = post_json(app.clone(), "/context/v1", encode_single(USDC, WDRAM)).await;
    assert_eq!(status, 503);
    let after = scrape_metrics(app.clone()).await;
    grew_by_at_least(&before, &after, "service_unavailable", 1.0);

    // Request-level: an all-failed envelope lands on `failed`.
    let before = after;
    let (status, _) = post_json(
        app.clone(),
        "/context/v1?allowFailure=true",
        encode_batch(&[(USDC, WDRAM), (USDC, unknown)]),
    )
    .await;
    assert_eq!(status, 200);
    let after = scrape_metrics(app.clone()).await;
    let failed = |t: &str| {
        counter_value(
            t,
            "oracle_context_request_total",
            &[("endpoint", "v1"), ("outcome", "failed")],
        )
    };
    assert!(failed(&after) - failed(&before) >= 1.0);
}

/// The `describe_counter!` text registered in `metrics.rs` is what the
/// exporter prints as `# HELP`. The exporter only prints a family once
/// it has a sample, so drive one request first, then check both
/// counters carry the new outcome vocabulary in their HELP line.
#[tokio::test]
async fn test_metrics_help_text_describes_both_context_counters() {
    let app = test_app().await;
    let (status, _) = post_json(app.clone(), "/context/v1", encode_single(USDC, WCOIN)).await;
    assert_eq!(status, 200);
    let text = scrape_metrics(app).await;

    let help = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(&format!("# HELP {name} ")))
            .unwrap_or_else(|| panic!("no HELP line for {name} in:\n{text}"))
            .to_string()
    };
    let request_help = help("oracle_context_request_total");
    for word in ["partial", "failed", "allowFailure", "v7"] {
        assert!(request_help.contains(word), "{request_help}");
    }
    let item_help = help("oracle_context_item_total");
    for word in [
        "ok",
        "bad_request",
        "service_unavailable",
        "internal_error",
        "allowFailure",
    ] {
        assert!(item_help.contains(word), "{item_help}");
    }
}

/// The v7-only fail-closed path (`pick_underlying_rate_bytes` on the
/// all-zero sentinel) is per-item too, and per-schema: the same batch
/// that yields an `internal_error` slot on v7 signs cleanly on v5, and a
/// second symbol with a valid underlying rate signs on v7 alongside the
/// failed one. Strict v7 keeps the whole-request 500.
#[tokio::test]
async fn test_v7_absent_underlying_rate_is_per_item_with_flag() {
    let signer = Signer::new(TEST_KEY).unwrap();
    let registry = TokenRegistry::new(
        vec![
            (WCOIN.to_string(), "COIN".to_string()),
            (WDRAM.to_string(), "DRAM".to_string()),
        ],
        USDC,
    )
    .unwrap();
    let mut coin = fake_quote("COIN", WCOIN, "0.01", "100");
    coin.underlying_rate_quote_to_base = WireFloat::from_bytes([0u8; 32]);
    coin.underlying_rate_base_to_quote = WireFloat::from_bytes([0u8; 32]);
    let dram = fake_quote("DRAM", WDRAM, "0.02", "50");
    let pricing = LiveClient::with_seeded(vec![coin, dram]).await;
    let metrics = MetricsHandle::install().expect("metrics install");
    let app = create_app(AppState::new(
        signer,
        registry,
        pricing,
        vec!["COIN".to_string(), "DRAM".to_string()],
        fixed_close_market_hours().await,
        metrics,
    ));
    let body = encode_batch(&[(USDC, WCOIN), (USDC, WDRAM)]);

    let (status, json) =
        post_json(app.clone(), "/context/v7?allowFailure=true", body.clone()).await;
    assert_eq!(status, 200, "{json}");
    let items: Vec<BatchItemResponse> = serde_json::from_value(json).unwrap();
    assert!(
        matches!(&items[0], BatchItemResponse::Error(e) if e.error == "internal_error"),
        "{:?}",
        items[0]
    );
    assert!(
        matches!(&items[1], BatchItemResponse::Ok(_)),
        "{:?}",
        items[1]
    );

    let (status, json) =
        post_json(app.clone(), "/context/v5?allowFailure=true", body.clone()).await;
    assert_eq!(status, 200, "{json}");
    let items: Vec<BatchItemResponse> = serde_json::from_value(json).unwrap();
    assert!(
        items.iter().all(|i| matches!(i, BatchItemResponse::Ok(_))),
        "v5 ignores the underlying rate"
    );

    let (status, json) = post_json(app, "/context/v7", body).await;
    assert_eq!(status, 500, "strict v7 keeps the whole-request 500: {json}");
}

/// Cryptographic check, not a field comparison: recover the EIP-191
/// signer from `signature` over `keccak256(abi.encodePacked(context))`
/// — the exact scheme `LibContext.build` verifies on-chain — and demand
/// it equals both the claimed `signer` field and the test key's address.
fn assert_signed_by_test_key(resp: &OracleResponse, what: &str) {
    let expected = Signer::new(TEST_KEY).unwrap().address();
    assert_eq!(resp.signer, expected, "{what}: signer field");
    let packed: Vec<u8> = resp.context.iter().flat_map(|b| b.to_vec()).collect();
    let hash = alloy::primitives::keccak256(&packed);
    let sig = alloy::primitives::Signature::from_raw(&resp.signature)
        .unwrap_or_else(|e| panic!("{what}: signature bytes: {e}"));
    let recovered = sig
        .recover_address_from_msg(hash)
        .unwrap_or_else(|e| panic!("{what}: recover: {e}"));
    assert_eq!(recovered, expected, "{what}: recovered signer");
}

/// Plan case 1's last clause: the ok slots of an envelope are real
/// signatures from the configured key, recoverable exactly as the
/// orderbook contract recovers them — not merely equal to some other
/// response. Also covers the strict array so the two modes are held to
/// the same standard.
#[tokio::test]
async fn test_envelope_ok_slots_carry_recoverable_signatures() {
    let unknown = "0x9999999999999999999999999999999999999999";
    for endpoint in ALL_ENDPOINTS {
        let app = test_app_with(&[(WCOIN, "COIN", Some(100.0)), (WDRAM, "DRAM", None)]).await;
        let mixed = encode_batch(&[(USDC, WCOIN), (USDC, WDRAM), (USDC, unknown), (WCOIN, USDC)]);

        let uri = format!("{endpoint}?allowFailure=true");
        let (status, json) = post_json(app.clone(), &uri, mixed).await;
        assert_eq!(status, 200, "{uri}");
        let items: Vec<BatchItemResponse> = serde_json::from_value(json).unwrap();
        let mut ok_slots = 0;
        for (i, item) in items.iter().enumerate() {
            if let BatchItemResponse::Ok(resp) = item {
                assert_signed_by_test_key(resp, &format!("{uri} slot {i}"));
                ok_slots += 1;
            }
        }
        assert_eq!(ok_slots, 2, "{uri}");

        let (status, json) =
            post_json(app, endpoint, encode_batch(&[(USDC, WCOIN), (WCOIN, USDC)])).await;
        assert_eq!(status, 200, "{endpoint}");
        let strict: Vec<OracleResponse> = serde_json::from_value(json).unwrap();
        for (i, resp) in strict.iter().enumerate() {
            assert_signed_by_test_key(resp, &format!("{endpoint} strict {i}"));
        }
    }
}
