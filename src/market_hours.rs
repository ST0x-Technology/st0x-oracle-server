//! Authoritative market-hours cache from Alpaca's `/v1/calendar` endpoint.
//! Serves two roles, both fed by the same session windows:
//!
//! 1. **Mark as-of stamping (`publish_time_for`).** Called by the poll
//!    loop at fetch time to compute each mark's "as-of" timestamp:
//!    `fetch_time` in-session, the last `session_close` out-of-session
//!    (the price hasn't been valid since then). That value is stored on
//!    `QuoteData.t` and signed straight through as `publish_time`, so the
//!    strategy's `max-staleness` sees a truthful age. Stamping at fetch
//!    time (not sign time) means a stalled poll loop can't hide: `t`
//!    stops advancing and the timestamp ages out.
//!
//! 2. **Session classification (`session_info_for`).** The v2/v3/v4
//!    signed context carries the session tag (slot 3) plus the UTC
//!    start/end bounds (slots 4/5); strategies gate on those.
//!
//! This is the fetch-time refinement of RAI-693: the frozen-out-of-session
//! mark still gets a stale timestamp, but now it's decided when the price
//! is obtained rather than when a consumer requests a signature.

use crate::alpaca::AlpacaClient;
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, TimeZone, Utc};
use chrono_tz::US::Eastern;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// One trading day's boundaries, materialised to UTC from the HHMM
/// strings the Alpaca calendar returns.
///
/// `session_open` / `session_close` are the extended-hours bounds
/// (typically 04:00 / 20:00 ET); `rth_open` / `rth_close` are the
/// regular-trading-hours bounds (typically 09:30 / 16:00 ET) — used by
/// v2's session classifier to differentiate RTH from pre-/after-hours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWindow {
    pub date: NaiveDate,
    pub session_open: DateTime<Utc>,
    pub rth_open: DateTime<Utc>,
    pub rth_close: DateTime<Utc>,
    pub session_close: DateTime<Utc>,
}

/// The market-session classification at a given instant. The bytes32
/// ASCII encoding is what `/context/v2` signs at context slot 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Rth,
    Premarket,
    Afterhours,
    OvernightClosed,
    WeekendClosed,
}

impl Session {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rth => "rth",
            Self::Premarket => "premarket",
            Self::Afterhours => "afterhours",
            Self::OvernightClosed => "overnight_closed",
            Self::WeekendClosed => "weekend_closed",
        }
    }

    /// Encode the session tag as the **V1** IntOrAString shape used
    /// by `/context/v2`: byte 0 = `(len & 0x1f) | 0x80`, ASCII data
    /// at bytes `1..1+len`, tail zero-padded.
    ///
    /// This is what live v2 orders bind `allowed-session` to via the
    /// hex presets in `st0x-fixed-spread-v2.rain` /
    /// `st0x-oracle-limit-v2.rain` (e.g.
    /// `0x8372746800…` for "rth"). The Rainlang parser does **not**
    /// emit this shape from a `"…"` string literal — see
    /// `to_bytes32_v3` for that — so v2 strategies have to compare
    /// against the hex preset bytes32 directly.
    pub fn to_bytes32_v1(self) -> [u8; 32] {
        let bytes = self.as_str().as_bytes();
        assert!(bytes.len() < 32, "session name must fit in 31 bytes");
        let mut out = [0u8; 32];
        out[0] = 0x80 | (bytes.len() as u8);
        out[1..=bytes.len()].copy_from_slice(bytes);
        out
    }

    /// Encode the session tag as Rain `IntOrAString` **V3** bytes32 —
    /// the exact byte layout the Rainlang parser produces for a `"…"`
    /// string literal via `LibIntOrAString::fromStringV3`. Byte 31 =
    /// `(len & 0x1f) | 0xe0`, ASCII data at bytes `(31-len)..31`, head
    /// zero-padded.
    ///
    /// Used by `/context/v3`. v3 strategies compare
    /// `equal-to(signed-context<0 3>() "rth")` directly — both sides
    /// resolve to the same V3 bytes32 and the equality holds
    /// byte-for-byte.
    pub fn to_bytes32_v3(self) -> [u8; 32] {
        let bytes = self.as_str().as_bytes();
        assert!(bytes.len() < 32, "session name must fit in 31 bytes");
        let mut out = [0u8; 32];
        let len = bytes.len();
        out[31 - len..31].copy_from_slice(bytes);
        out[31] = 0xe0 | (len as u8);
        out
    }
}

/// The session-level info `/context/v2` exposes at slots 3-5: which
/// session we're in, and the UTC bounds of *that current* session
/// (not the next).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub session: Session,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// Rolling cache of session windows around today. Refreshed periodically
/// in the background; read by the v2/v3/v4 handlers to classify the
/// current session for the signed context's session slots.
#[derive(Debug, Default)]
pub struct MarketHoursCache {
    windows: RwLock<Vec<SessionWindow>>,
}

impl MarketHoursCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the cached windows. Sorted ascending by date.
    pub async fn set(&self, mut windows: Vec<SessionWindow>) {
        windows.sort_by_key(|w| w.date);
        *self.windows.write().await = windows;
    }

    /// Compute the "as-of" timestamp to stamp on a mark fetched at
    /// `fetch_time`. This is called by the **poll loop** at fetch time,
    /// and the value is signed straight through as `publish_time`.
    ///
    /// - Inside an active extended-session window -> `fetch_time` (the
    ///   broker mark is genuinely live, so the fetch instant is truthful).
    /// - Outside any active window (overnight / weekend / holiday) the
    ///   broker keeps returning a frozen mark; the price hasn't been valid
    ///   since the last close, so we stamp the most recent past
    ///   `session_close`. The strategy's `max-staleness` then rejects it.
    /// - No cached windows yet (cold start) -> `fetch_time`, preserving
    ///   liveness rather than blocking until the calendar primes.
    ///
    /// Stamping at fetch time (rather than sign time) means a stalled poll
    /// loop is self-evident: `t` stops advancing, so the signed timestamp
    /// ages out and the strategy rejects — the failure can't hide behind
    /// a fresh request clock.
    pub async fn publish_time_for(&self, fetch_time: DateTime<Utc>) -> DateTime<Utc> {
        let windows = self.windows.read().await;
        for w in windows.iter() {
            if w.session_open <= fetch_time && fetch_time < w.session_close {
                return fetch_time;
            }
        }
        windows
            .iter()
            .filter(|w| w.session_close <= fetch_time)
            .map(|w| w.session_close)
            .max()
            .unwrap_or(fetch_time)
    }

    /// Classify the current market session and return its UTC bounds.
    ///
    /// - Inside an active extended-session window we look at the cached
    ///   `rth_open` / `rth_close` to split into `Premarket` / `Rth` /
    ///   `Afterhours`. `start` / `end` are the bounds of the *current*
    ///   sub-window — not the whole extended session and not the next
    ///   open.
    /// - Outside any active window we return either `OvernightClosed`
    ///   (gap < 12 h between prev close and next open) or
    ///   `WeekendClosed` (gap >= 12 h — folds Friday-night-through-Monday
    ///   and multi-day holiday closes into the same bucket). `start` is
    ///   the most recent past `session_close`; `end` is the next future
    ///   `session_open`.
    /// - With no cached windows we fall back to `OvernightClosed` with
    ///   degenerate bounds (`start = end = now`) so callers always get
    ///   *some* answer rather than blocking on calendar availability.
    pub async fn session_info_for(&self, now: DateTime<Utc>) -> SessionInfo {
        let windows = self.windows.read().await;

        for w in windows.iter() {
            if w.session_open <= now && now < w.session_close {
                return if now < w.rth_open {
                    SessionInfo {
                        session: Session::Premarket,
                        start: w.session_open,
                        end: w.rth_open,
                    }
                } else if now < w.rth_close {
                    SessionInfo {
                        session: Session::Rth,
                        start: w.rth_open,
                        end: w.rth_close,
                    }
                } else {
                    SessionInfo {
                        session: Session::Afterhours,
                        start: w.rth_close,
                        end: w.session_close,
                    }
                };
            }
        }

        let prev_close = windows
            .iter()
            .filter(|w| w.session_close <= now)
            .map(|w| w.session_close)
            .max();
        let next_open = windows
            .iter()
            .filter(|w| w.session_open > now)
            .map(|w| w.session_open)
            .min();

        let session = match (prev_close, next_open) {
            (Some(prev), Some(next)) if (next - prev) >= ChronoDuration::hours(12) => {
                Session::WeekendClosed
            }
            _ => Session::OvernightClosed,
        };

        SessionInfo {
            session,
            start: prev_close.unwrap_or(now),
            end: next_open.unwrap_or(now),
        }
    }

    /// Diagnostic for `/status`: how many windows we have cached.
    pub async fn window_count(&self) -> usize {
        self.windows.read().await.len()
    }
}

/// Anchor a HHMM Alpaca session boundary to UTC via America/New_York. Handles
/// the EST/EDT transitions by going through chrono-tz; ambiguous local times
/// (the fall-back hour) and gaps (the spring-forward hour) error rather than
/// silently picking a side.
pub fn anchor_session_to_utc(date: NaiveDate, hhmm: &str) -> anyhow::Result<DateTime<Utc>> {
    if hhmm.len() != 4 {
        anyhow::bail!("expected 4-char HHMM, got {hhmm:?}");
    }
    let hour: u32 = hhmm[..2]
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid HH in {hhmm:?}: {e}"))?;
    let minute: u32 = hhmm[2..]
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid MM in {hhmm:?}: {e}"))?;
    let naive = date
        .and_hms_opt(hour, minute, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid time on {date}: {hour:02}:{minute:02}"))?;
    let et = Eastern.from_local_datetime(&naive).single().ok_or_else(|| {
        anyhow::anyhow!("ambiguous or non-existent local time on {date} {hour:02}:{minute:02} (DST transition)")
    })?;
    Ok(et.with_timezone(&Utc))
}

/// Refresh the cache from Alpaca, covering a rolling ±7-day window so a
/// query on a holiday Monday still has the previous Friday's close
/// available as `last_session_close`.
pub async fn refresh_once(cache: &MarketHoursCache, alpaca: &AlpacaClient) -> anyhow::Result<()> {
    let today = Utc::now().date_naive();
    let start = today - ChronoDuration::days(7);
    let end = today + ChronoDuration::days(7);
    let windows = alpaca.fetch_calendar(start, end).await?;
    let n = windows.len();
    cache.set(windows).await;
    tracing::debug!(windows = n, "Market hours refreshed");
    Ok(())
}

/// Spawn a background task that refreshes the calendar every `interval`.
/// Refresh failures log a warning and keep the previous cache — matching
/// the quote-poll loop's behavior in `cache::poll_once`.
pub fn spawn_market_hours_refresh(
    cache: Arc<MarketHoursCache>,
    alpaca: AlpacaClient,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // first tick fires immediately; skip — we primed at startup
        loop {
            ticker.tick().await;
            if let Err(e) = refresh_once(&cache, &alpaca).await {
                tracing::warn!(error = %e, "Market hours refresh failed; keeping previous cache");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, mi, 0).unwrap()
    }

    fn fri_window() -> SessionWindow {
        // 2026-05-29 (Fri): session 04:00-20:00 ET = 08:00-24:00 UTC
        // during EDT (UTC-4). RTH 09:30-16:00 ET = 13:30-20:00 UTC.
        SessionWindow {
            date: date(2026, 5, 29),
            session_open: utc(2026, 5, 29, 8, 0),
            rth_open: Utc.with_ymd_and_hms(2026, 5, 29, 13, 30, 0).unwrap(),
            rth_close: utc(2026, 5, 29, 20, 0),
            session_close: utc(2026, 5, 30, 0, 0),
        }
    }

    fn mon_window() -> SessionWindow {
        // 2026-06-01 (Mon): session 04:00-20:00 ET = 08:00-24:00 UTC
        // during EDT. RTH 09:30-16:00 ET = 13:30-20:00 UTC.
        SessionWindow {
            date: date(2026, 6, 1),
            session_open: utc(2026, 6, 1, 8, 0),
            rth_open: Utc.with_ymd_and_hms(2026, 6, 1, 13, 30, 0).unwrap(),
            rth_close: utc(2026, 6, 1, 20, 0),
            session_close: utc(2026, 6, 2, 0, 0),
        }
    }

    #[tokio::test]
    async fn inside_session_returns_fetch_time() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window()]).await;
        let fetch = utc(2026, 5, 29, 14, 30); // 10:30 ET Friday — RTH
        assert_eq!(c.publish_time_for(fetch).await, fetch);
    }

    #[tokio::test]
    async fn premarket_inside_session_returns_fetch_time() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window()]).await;
        let fetch = utc(2026, 5, 29, 9, 0); // 05:00 ET Friday — pre-market, still in session
        assert_eq!(c.publish_time_for(fetch).await, fetch);
    }

    #[tokio::test]
    async fn weekend_returns_last_friday_session_close() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window()]).await;
        let fetch = utc(2026, 5, 31, 12, 0); // Sunday noon UTC
        assert_eq!(c.publish_time_for(fetch).await, fri_window().session_close);
    }

    #[tokio::test]
    async fn holiday_monday_returns_previous_friday_close() {
        let c = MarketHoursCache::new();
        // Mon 2026-06-01 has no entry (simulated holiday); Tue has one.
        let tue_window = SessionWindow {
            date: date(2026, 6, 2),
            session_open: utc(2026, 6, 2, 8, 0),
            rth_open: Utc.with_ymd_and_hms(2026, 6, 2, 13, 30, 0).unwrap(),
            rth_close: utc(2026, 6, 2, 20, 0),
            session_close: utc(2026, 6, 3, 0, 0),
        };
        c.set(vec![fri_window(), tue_window]).await;
        let fetch = utc(2026, 6, 1, 14, 0); // Mon 10:00 ET — would be RTH on a non-holiday
        assert_eq!(c.publish_time_for(fetch).await, fri_window().session_close);
    }

    #[tokio::test]
    async fn overnight_weekday_returns_previous_session_close() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window(), mon_window()]).await;
        // Mon 02:00 ET = Mon 06:00 UTC — before Monday's 04:00 ET session_open
        let fetch = utc(2026, 6, 1, 6, 0);
        assert_eq!(c.publish_time_for(fetch).await, fri_window().session_close);
    }

    #[tokio::test]
    async fn boundary_at_session_open_is_in_session() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window()]).await;
        let exactly_open = fri_window().session_open;
        assert_eq!(c.publish_time_for(exactly_open).await, exactly_open);
    }

    #[tokio::test]
    async fn boundary_at_session_close_is_out_of_session() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window()]).await;
        let exactly_close = fri_window().session_close;
        // At session_close exactly, we're outside the window and stamp the
        // close timestamp itself (which equals `fetch_time` here, but
        // would diverge a moment later).
        assert_eq!(c.publish_time_for(exactly_close).await, exactly_close);
    }

    #[tokio::test]
    async fn empty_cache_falls_back_to_fetch_time() {
        let c = MarketHoursCache::new();
        let fetch = utc(2026, 5, 31, 12, 0);
        assert_eq!(c.publish_time_for(fetch).await, fetch);
    }

    #[test]
    fn anchor_session_handles_edt() {
        // EDT = UTC-4. 04:00 ET on 2026-05-29 -> 08:00 UTC.
        let got = anchor_session_to_utc(date(2026, 5, 29), "0400").unwrap();
        assert_eq!(got, utc(2026, 5, 29, 8, 0));
    }

    #[test]
    fn anchor_session_handles_est() {
        // EST = UTC-5. 20:00 ET on 2026-01-15 -> 01:00 UTC next day.
        let got = anchor_session_to_utc(date(2026, 1, 15), "2000").unwrap();
        assert_eq!(got, utc(2026, 1, 16, 1, 0));
    }

    #[test]
    fn anchor_session_rejects_invalid_hhmm() {
        assert!(anchor_session_to_utc(date(2026, 5, 29), "abc").is_err());
        assert!(anchor_session_to_utc(date(2026, 5, 29), "2500").is_err());
        assert!(anchor_session_to_utc(date(2026, 5, 29), "04:00").is_err()); // wrong format
    }

    #[test]
    fn session_bytes32_v1_matches_byte0_layout() {
        // V1 IntOrAString: byte 0 = (len & 0x1f) | 0x80; bytes
        // 1..1+len = ASCII; bytes 1+len..32 = 0. Used by `/context/v2`
        // — matches the hex presets baked into deployed v2 strategies.
        for sess in [
            Session::Rth,
            Session::Premarket,
            Session::Afterhours,
            Session::OvernightClosed,
            Session::WeekendClosed,
        ] {
            let b = sess.to_bytes32_v1();
            let name = sess.as_str().as_bytes();
            let len = name.len();
            assert_eq!(
                b[0],
                0x80 | len as u8,
                "{}: byte 0 must be 0x80 | length",
                sess.as_str()
            );
            assert_eq!(
                &b[1..=len],
                name,
                "{}: ASCII data must immediately follow the length byte",
                sess.as_str()
            );
            assert!(
                b[1 + len..].iter().all(|&x| x == 0),
                "{}: tail must be zero-padded",
                sess.as_str()
            );
        }
    }

    #[test]
    fn session_bytes32_v3_matches_rain_intorastring_v3_format() {
        // V3 IntOrAString: byte 31 = (len & 0x1f) | 0xe0; bytes
        // (31-len)..31 = ASCII; bytes 0..(31-len) = 0. This is what
        // `LibIntOrAString::fromStringV3` produces and what the
        // Rainlang parser emits for a `"…"` string literal — used by
        // `/context/v3` so v3 strategies can compare against string
        // literals without inline hex.
        for sess in [
            Session::Rth,
            Session::Premarket,
            Session::Afterhours,
            Session::OvernightClosed,
            Session::WeekendClosed,
        ] {
            let b = sess.to_bytes32_v3();
            let name = sess.as_str().as_bytes();
            let len = name.len();
            assert_eq!(
                b[31],
                0xe0 | len as u8,
                "{}: byte 31 must be 0xe0 | length",
                sess.as_str()
            );
            assert_eq!(
                &b[31 - len..31],
                name,
                "{}: ASCII data must end immediately before the length byte",
                sess.as_str()
            );
            assert!(
                b[..31 - len].iter().all(|&x| x == 0),
                "{}: head must be zero-padded",
                sess.as_str()
            );
        }
    }

    #[test]
    fn session_bytes32_v1_known_rth_value() {
        // Spot-check the exact V1 bytes for "rth". This is the value
        // hex-baked into `st0x-fixed-spread-v2.rain`'s preset.
        let b = Session::Rth.to_bytes32_v1();
        let mut expected = [0u8; 32];
        expected[..4].copy_from_slice(&[0x83, b'r', b't', b'h']);
        assert_eq!(b, expected);
    }

    #[test]
    fn session_bytes32_v3_known_rth_value() {
        // Spot-check the exact V3 bytes for "rth". Matches what
        // `Float::from_str("\"rth\"")` produces inside the Rainlang
        // parser for the string literal `"rth"`.
        let b = Session::Rth.to_bytes32_v3();
        let mut expected = [0u8; 32];
        expected[28] = b'r';
        expected[29] = b't';
        expected[30] = b'h';
        expected[31] = 0xe3; // 0xe0 | 3
        assert_eq!(b, expected);
    }

    #[test]
    fn session_bytes32_v3_known_weekend_closed_value() {
        // Empirical anchor for the V3 encoder. This exact 32-byte
        // value was confirmed on-chain to equal Rainlang's
        // `"weekend_closed"` literal via a successful quote2 call
        // against a self-signed context (see #29). If this assertion
        // ever changes, somebody touched `to_bytes32_v3` — re-prove
        // the on-chain compare before shipping.
        let b = Session::WeekendClosed.to_bytes32_v3();
        let expected: [u8; 32] = [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'w', b'e', b'e', b'k', b'e', b'n',
            b'd', b'_', b'c', b'l', b'o', b's', b'e', b'd', 0xee,
        ];
        assert_eq!(b, expected);
    }

    #[test]
    fn session_names_unique_and_fit() {
        let names = [
            Session::Rth.as_str(),
            Session::Premarket.as_str(),
            Session::Afterhours.as_str(),
            Session::OvernightClosed.as_str(),
            Session::WeekendClosed.as_str(),
        ];
        for n in &names {
            assert!(n.len() <= 31, "{n} must fit in 31 data bytes");
        }
        let set: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(set.len(), 5);
    }

    #[tokio::test]
    async fn session_info_rth() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window()]).await;
        let now = utc(2026, 5, 29, 17, 0); // 13:00 ET
        let info = c.session_info_for(now).await;
        assert_eq!(info.session, Session::Rth);
        assert_eq!(info.start, fri_window().rth_open);
        assert_eq!(info.end, fri_window().rth_close);
    }

    #[tokio::test]
    async fn session_info_premarket() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window()]).await;
        let now = utc(2026, 5, 29, 10, 0); // 06:00 ET
        let info = c.session_info_for(now).await;
        assert_eq!(info.session, Session::Premarket);
        assert_eq!(info.start, fri_window().session_open);
        assert_eq!(info.end, fri_window().rth_open);
    }

    #[tokio::test]
    async fn session_info_afterhours() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window()]).await;
        let now = utc(2026, 5, 29, 22, 0); // 18:00 ET
        let info = c.session_info_for(now).await;
        assert_eq!(info.session, Session::Afterhours);
        assert_eq!(info.start, fri_window().rth_close);
        assert_eq!(info.end, fri_window().session_close);
    }

    #[tokio::test]
    async fn session_info_overnight_closed_between_weekdays() {
        let c = MarketHoursCache::new();
        // Mon and Tue trading days, both EDT.
        let tue_window = SessionWindow {
            date: date(2026, 6, 2),
            session_open: utc(2026, 6, 2, 8, 0),
            rth_open: Utc.with_ymd_and_hms(2026, 6, 2, 13, 30, 0).unwrap(),
            rth_close: utc(2026, 6, 2, 20, 0),
            session_close: utc(2026, 6, 3, 0, 0),
        };
        c.set(vec![mon_window(), tue_window.clone()]).await;
        // Tue 02:00 ET = Tue 06:00 UTC, between Mon's session_close
        // (Tue 00:00 UTC) and Tue's session_open (Tue 08:00 UTC). Gap
        // is 8 h - overnight, not weekend.
        let now = utc(2026, 6, 2, 6, 0);
        let info = c.session_info_for(now).await;
        assert_eq!(info.session, Session::OvernightClosed);
        assert_eq!(info.start, mon_window().session_close);
        assert_eq!(info.end, tue_window.session_open);
    }

    #[tokio::test]
    async fn session_info_weekend_closed() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window(), mon_window()]).await;
        // Sat noon UTC - between Fri's session_close (Sat 00:00 UTC)
        // and Mon's session_open (Mon 08:00 UTC). Gap is 56h, well
        // over the 12h threshold - classified as weekend.
        let now = utc(2026, 5, 30, 12, 0);
        let info = c.session_info_for(now).await;
        assert_eq!(info.session, Session::WeekendClosed);
        assert_eq!(info.start, fri_window().session_close);
        assert_eq!(info.end, mon_window().session_open);
    }

    #[tokio::test]
    async fn session_info_holiday_long_close_classifies_as_weekend() {
        // 3-day holiday weekend: Fri Mon trading days only (Tue or
        // Wed simulating the holiday Mon being absent), with Tue back
        // to normal. Gap from Fri close to Tue open is ~85h - bucket
        // as `weekend_closed` per the 12h threshold.
        let c = MarketHoursCache::new();
        let tue_window = SessionWindow {
            date: date(2026, 6, 2),
            session_open: utc(2026, 6, 2, 8, 0),
            rth_open: Utc.with_ymd_and_hms(2026, 6, 2, 13, 30, 0).unwrap(),
            rth_close: utc(2026, 6, 2, 20, 0),
            session_close: utc(2026, 6, 3, 0, 0),
        };
        c.set(vec![fri_window(), tue_window.clone()]).await;
        let now = utc(2026, 6, 1, 14, 0); // Mon 10:00 ET, holiday
        let info = c.session_info_for(now).await;
        assert_eq!(info.session, Session::WeekendClosed);
        assert_eq!(info.start, fri_window().session_close);
        assert_eq!(info.end, tue_window.session_open);
    }

    #[tokio::test]
    async fn session_info_empty_cache_returns_degenerate_overnight() {
        let c = MarketHoursCache::new();
        let now = utc(2026, 5, 29, 12, 0);
        let info = c.session_info_for(now).await;
        assert_eq!(info.session, Session::OvernightClosed);
        // No data - degenerate bounds (start == end == now).
        assert_eq!(info.start, now);
        assert_eq!(info.end, now);
    }
}
