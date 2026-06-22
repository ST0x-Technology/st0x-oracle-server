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

// Used both as the cached QuoteData.t (just bookkeeping post-RAI-693)
// and as the `last_session_close` in the default out-of-session
// MarketHoursCache. Must be in the past relative to wall-clock now so the
// cache's `publish_time_for` returns it via the "most recent past close"
// branch. 1_700_000_000 = 2023-11-14T22:13:20Z.
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

/// Build a test app with a pre-populated cache. By default we use a
/// market-hours cache pinned **outside** any session window so signed
/// `publish_time` is deterministic — the `last_session_close` is set
/// to `FIXED_PUBLISH_TIME` and tests can assert against it.
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

/// Cache configured with one prior session window whose `session_close`
/// equals `FIXED_PUBLISH_TIME`. Since that close is far in the past (the
/// sampler runs in the present), every `publish_time_for(now)` returns
/// the close — i.e. the app behaves as "out of session" and signs
/// `FIXED_PUBLISH_TIME` deterministically.
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

/// Cache that places `now` strictly inside an active session window —
/// publish_time will be the request's wall-clock `Utc::now()`. Used for
/// the in-session test where we just need `publish_time` to be close to
/// the signing instant.
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
/// format is Rain's `IntOrAString` V3: byte 31 holds `(len & 0x1f) | 0xe0`,
/// the ASCII data lives in bytes `(31-len)..31`, head is zero-padded.
fn decode_session_tag(b: alloy::primitives::FixedBytes<32>) -> String {
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
    decode_session_tag(responses[0].context[3])
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

    // publish_time = the cached MarketHoursCache's last session_close.
    // Default test_app() is "out of session" with session_close pinned
    // to FIXED_PUBLISH_TIME, so we expect to see that exact value here.
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
async fn test_v1_publish_time_uses_now_when_in_session() {
    // RAI-693: when MarketHoursCache says we're inside an active session,
    // publish_time must be the request's wall-clock now, NOT the cached
    // fetch_time (which is FIXED_PUBLISH_TIME and would look hours old).
    let mh = always_in_session_market_hours().await;
    let app = test_app_full(&[(WCOIN, "COIN", Some(100.0))], mh).await;

    let before = Utc::now().timestamp();
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
    let after = Utc::now().timestamp();

    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let responses: Vec<OracleResponse> = serde_json::from_slice(&bytes).unwrap();
    let publish_raw = Float::from(alloy::primitives::B256::from(responses[0].context[2]))
        .format()
        .unwrap();
    // Rain Float formats large ints in scientific notation ("1.78e9");
    // parse via f64 then round to the nearest integer second.
    let publish_ts: i64 = publish_raw
        .parse::<f64>()
        .expect("publish_time decodes as numeric")
        .round() as i64;

    // publish_time should fall in the [before, after] interval the
    // request straddled — i.e. the request's `now`, not the stale
    // FIXED_PUBLISH_TIME from the fetch-time cache.
    assert!(
        publish_ts >= before && publish_ts <= after,
        "in-session publish_time {publish_ts} should be in [{before}, {after}], not the cached fetch_time {FIXED_PUBLISH_TIME}"
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

    // session tag — Rain IntOrAString V3 encoding of "rth":
    // byte 31 = 0xe3 (0xe0 | 3), bytes 28..31 = "rth", head zero-padded.
    let sess = resp.context[3].as_slice();
    assert_eq!(sess[31], 0xe3, "byte 31 must be 0xe0 | 3");
    assert_eq!(&sess[28..31], b"rth");
    assert!(
        sess[..28].iter().all(|&b| b == 0),
        "session tag must be zero-padded before the data: {:?}",
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
    // Decode the IntOrAString V3 format: length = byte 31 & 0x1f,
    // ASCII data ends at byte 31. With only a single window in the
    // cache and `now` after it, the cache returns OvernightClosed (no
    // `next_open` to widen the gap).
    let len = (sess[31] & 0x1f) as usize;
    let name = std::str::from_utf8(&sess[31 - len..31]).unwrap();
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
