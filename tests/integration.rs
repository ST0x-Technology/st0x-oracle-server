use alloy::primitives::{Address, FixedBytes, U256};
use alloy::sol_types::SolValue;
use axum::body::Bytes;
use chrono::{Duration as ChronoDuration, NaiveDate, TimeZone, Utc};
use http_body_util::BodyExt;
use rain_math_float::Float;
use st0x_oracle_server::alpaca::QuoteData;
use st0x_oracle_server::cache::QuoteCache;
use st0x_oracle_server::market_hours::{MarketHoursCache, SessionWindow};
use st0x_oracle_server::oracle::{OracleResponse, SCHEMA_VERSION};
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
const WDRAM: &str = "0x2222222222222222222222222222222222222222";

// The cached `QuoteData.t` (mark fetch time) for every seeded quote.
// Since publish_time IS the fetch time, tests assert the signed
// publish_time equals this exact value. Chosen in the past relative to
// wall-clock now so a regression that accidentally signs `now` would
// diverge visibly. 1_700_000_000 = 2023-11-14T22:13:20Z.
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

/// Build a test app with a pre-populated cache. Seeded quotes carry
/// `QuoteData.t = FIXED_PUBLISH_TIME`, and since publish_time is the
/// mark's fetch time, tests assert the signed publish_time against it.
/// The default market-hours cache is pinned outside any session window;
/// that only affects the v2/v3/v4 session slots, not publish_time.
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

    let cache = Arc::new(QuoteCache::new());
    for (_, sym, price) in entries {
        if let Some(p) = price {
            cache
                .update(
                    sym,
                    QuoteData {
                        price: *p,
                        t: Utc.timestamp_opt(FIXED_PUBLISH_TIME, 0).unwrap(),
                    },
                )
                .await;
        }
    }

    let configured_symbols: Vec<String> = entries.iter().map(|(_, s, _)| s.to_string()).collect();
    let state = AppState::new(signer, registry, cache, configured_symbols, market_hours);
    create_app(state)
}

/// Cache configured with one prior session window in the past, so the
/// app classifies as "out of session" for the v2/v3/v4 session slots.
/// publish_time is unaffected (it's the fetch time); this just makes the
/// session classification deterministic.
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
/// the session slots classify as `rth`. publish_time is still the mark's
/// fetch time regardless; used by the session-slot tests.
async fn always_in_session_market_hours() -> Arc<MarketHoursCache> {
    let mh = Arc::new(MarketHoursCache::new());
    let now = Utc::now();
    let window = SessionWindow {
        date: now.date_naive(),
        session_open: now - ChronoDuration::hours(8),
        // Bracket `now` in the middle of the RTH sub-window too, so a v2
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

/// Decode a session tag from slot 3 of the v2 context. The on-the-wire
/// format is Rain's `IntOrAString` V1: byte 0 holds `(len & 0x1f) | 0x80`,
/// ASCII data lives in bytes `1..=len`, tail zero-padded.
fn decode_session_tag_v1(b: alloy::primitives::FixedBytes<32>) -> String {
    let bytes: [u8; 32] = b.into();
    let len = (bytes[0] & 0x1f) as usize;
    String::from_utf8(bytes[1..=len].to_vec()).unwrap()
}

/// Decode a session tag from slot 3 of the v3 context. The on-the-wire
/// format is Rain's `IntOrAString` V3: byte 31 holds `(len & 0x1f) | 0xe0`,
/// ASCII data lives in bytes `(31-len)..31`, head zero-padded.
fn decode_session_tag_v3(b: alloy::primitives::FixedBytes<32>) -> String {
    let bytes: [u8; 32] = b.into();
    let len = (bytes[31] & 0x1f) as usize;
    String::from_utf8(bytes[31 - len..31].to_vec()).unwrap()
}

/// Send a single buy through `/context/v2` and return the decoded
/// session tag from the response. Used by the phase-coverage tests.
async fn v2_session_tag_for(app: axum::Router) -> String {
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v2")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
    decode_session_tag_v1(responses[0].context[3])
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
async fn test_v1_publish_time_is_fetch_time_even_when_in_session() {
    // publish_time is ALWAYS the mark's own fetch time (`QuoteData.t`),
    // never the request-handling instant. Here the market-hours cache
    // says we're inside an active session — under the old sign-time
    // behaviour that would have stamped `now`. The signed timestamp must
    // instead be the cached fetch time (FIXED_PUBLISH_TIME), so a stalled
    // poll loop can't hide behind a fresh request clock.
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
        "in-session publish_time must be the mark's fetch time, not the request clock"
    );
}

#[tokio::test]
async fn test_v2_single_returns_v2_schema_with_session() {
    // In-session market_hours -> /context/v2 should return a 6-element
    // context with schema version 2, fresh publish_time, "rth" session
    // tag, and the session bounds we put in the cache.
    let mh = always_in_session_market_hours().await;
    let app = test_app_full(&[(WCOIN, "COIN", Some(100.0))], mh).await;

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v2")
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
    assert_eq!(resp.context.len(), 6, "v2 must emit 6 context elements");

    // schema_version
    let version = Float::from(alloy::primitives::B256::from(resp.context[0]));
    assert_eq!(version.format().unwrap(), "2");

    // price (broker mark)
    let price = Float::from(alloy::primitives::B256::from(resp.context[1]));
    assert_eq!(price.format().unwrap(), "100");

    // session tag — Rain IntOrAString V1 encoding of "rth" (what
    // `/context/v2` emits for live v2 strategies):
    // byte 0 = 0x83 (0x80 | 3), bytes 1..4 = "rth", tail zero-padded.
    let sess = resp.context[3].as_slice();
    assert_eq!(sess[0], 0x83, "byte 0 must be 0x80 | 3");
    assert_eq!(&sess[1..4], b"rth");
    assert!(
        sess[4..].iter().all(|&b| b == 0),
        "session tag must be zero-padded after the data: {:?}",
        sess
    );

    // session_start, session_end - non-zero and ordered
    let start = Float::from(alloy::primitives::B256::from(resp.context[4]))
        .format()
        .unwrap()
        .parse::<f64>()
        .unwrap() as i64;
    let end = Float::from(alloy::primitives::B256::from(resp.context[5]))
        .format()
        .unwrap()
        .parse::<f64>()
        .unwrap() as i64;
    assert!(
        start > 0 && end > start,
        "session bounds: start={start} end={end}"
    );
}

#[tokio::test]
async fn test_v3_single_returns_v3_schema_with_v3_session_encoding() {
    // /context/v3 mirrors /context/v2 except slot 0 = 3 and slot 3 is
    // V3 IntOrAString (matches what the Rainlang parser emits for a
    // `"…"` string literal in a v3 strategy).
    let mh = always_in_session_market_hours().await;
    let app = test_app_full(&[(WCOIN, "COIN", Some(100.0))], mh).await;

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v3")
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
    assert_eq!(resp.context.len(), 6, "v3 must emit 6 context elements");

    // schema_version
    let version = Float::from(alloy::primitives::B256::from(resp.context[0]));
    assert_eq!(version.format().unwrap(), "3");

    // session tag — V3 IntOrAString for "rth":
    // byte 31 = 0xe3 (0xe0 | 3), bytes 28..31 = "rth", head zero-padded.
    let sess = resp.context[3].as_slice();
    assert_eq!(sess[31], 0xe3, "byte 31 must be 0xe0 | 3");
    assert_eq!(&sess[28..31], b"rth");
    assert!(
        sess[..28].iter().all(|&b| b == 0),
        "session tag must be zero-padded before the data: {:?}",
        sess
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
async fn test_v2_and_v3_session_slot_differ_byte_for_byte() {
    // Same request hitting both endpoints with the same session must
    // produce different slot-3 layouts: V1 byte-0 vs V3 byte-31. This
    // is the whole point of having two endpoints.
    let mh = always_in_session_market_hours().await;
    let app_v2 = test_app_full(&[(WCOIN, "COIN", Some(100.0))], mh.clone()).await;
    let app_v3 = test_app_full(&[(WCOIN, "COIN", Some(100.0))], mh).await;

    let v2_resp = app_v2
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v2")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                .unwrap(),
        )
        .await
        .unwrap();
    let v3_resp = app_v3
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v3")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                .unwrap(),
        )
        .await
        .unwrap();

    let v2_bytes = v2_resp.into_body().collect().await.unwrap().to_bytes();
    let v3_bytes = v3_resp.into_body().collect().await.unwrap().to_bytes();
    let v2: Vec<OracleResponse> = serde_json::from_slice(&v2_bytes).unwrap();
    let v3: Vec<OracleResponse> = serde_json::from_slice(&v3_bytes).unwrap();

    let v2_sess = v2[0].context[3];
    let v3_sess = v3[0].context[3];
    assert_ne!(v2_sess, v3_sess, "v2 and v3 session encodings must differ");
    assert_eq!(decode_session_tag_v1(v2_sess), "rth");
    assert_eq!(decode_session_tag_v3(v3_sess), "rth");
}

#[tokio::test]
async fn test_v2_handler_signs_rth_when_now_inside_rth() {
    let app = test_app_full(
        &[(WCOIN, "COIN", Some(100.0))],
        always_in_session_market_hours().await,
    )
    .await;
    assert_eq!(v2_session_tag_for(app).await, "rth");
}

#[tokio::test]
async fn test_v2_handler_signs_premarket_when_now_before_rth_open() {
    let app = test_app_full(
        &[(WCOIN, "COIN", Some(100.0))],
        premarket_market_hours().await,
    )
    .await;
    assert_eq!(v2_session_tag_for(app).await, "premarket");
}

#[tokio::test]
async fn test_v2_handler_signs_afterhours_when_now_past_rth_close() {
    let app = test_app_full(
        &[(WCOIN, "COIN", Some(100.0))],
        afterhours_market_hours().await,
    )
    .await;
    assert_eq!(v2_session_tag_for(app).await, "afterhours");
}

#[tokio::test]
async fn test_v2_handler_signs_overnight_closed_for_short_gap() {
    let app = test_app_full(
        &[(WCOIN, "COIN", Some(100.0))],
        overnight_closed_market_hours().await,
    )
    .await;
    assert_eq!(v2_session_tag_for(app).await, "overnight_closed");
}

#[tokio::test]
async fn test_v2_handler_signs_weekend_closed_for_long_gap() {
    let app = test_app_full(
        &[(WCOIN, "COIN", Some(100.0))],
        weekend_closed_market_hours().await,
    )
    .await;
    assert_eq!(v2_session_tag_for(app).await, "weekend_closed");
}

#[tokio::test]
async fn test_v2_session_tag_reflects_market_phase() {
    // Out-of-session market_hours (fixed_close_market_hours pins us
    // outside any active window) -> session tag should be a closed
    // variant, not "rth".
    let app = test_app_full(
        &[(WCOIN, "COIN", Some(100.0))],
        fixed_close_market_hours().await,
    )
    .await;
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v2")
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(encode_single(USDC, WCOIN)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
    let sess = responses[0].context[3].as_slice();
    // Decode the IntOrAString V1 format (what `/context/v2` emits):
    // length = byte 0 & 0x1f, ASCII data starts at byte 1. With only
    // a single window in the cache and `now` after it, the cache
    // returns OvernightClosed (no `next_open` to widen the gap).
    let len = (sess[0] & 0x1f) as usize;
    let name = std::str::from_utf8(&sess[1..=len]).unwrap();
    assert_eq!(name, "overnight_closed");
}

#[tokio::test]
async fn test_v2_batch_returns_length_matching_array_with_session() {
    let mh = always_in_session_market_hours().await;
    let app = test_app_full(&[(WCOIN, "COIN", Some(100.0))], mh).await;
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v2")
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
    // Sell leg's price is inverted (1/100 = 0.01) per the build_context_v2
    // contract — exercises the same inversion path as v1.
    let sell_price = Float::from(alloy::primitives::B256::from(responses[1].context[1]));
    assert_eq!(sell_price.format().unwrap(), "0.01");
}

#[tokio::test]
async fn test_v2_empty_batch_returns_empty_array() {
    let empty: Vec<(OrderV4, U256, U256, Address)> = Vec::new();
    let body = Bytes::from(empty.abi_encode());
    let app = test_app().await;
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/context/v2")
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

    // First: buy → broker mark (100)
    let buy_price = Float::from(alloy::primitives::B256::from(responses[0].context[1]));
    assert_eq!(buy_price.format().unwrap(), "100");

    // Second: sell → 1/mark, where mark = 100 → exactly 0.01
    let sell_price = Float::from(alloy::primitives::B256::from(responses[1].context[1]));
    assert_eq!(sell_price.format().unwrap(), "0.01");
}
