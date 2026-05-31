//! Authoritative market-hours cache from Alpaca's `/v1/calendar` endpoint.
//!
//! Used at sign time to choose `publish_time` truthfully:
//!
//! - Inside the **extended session window** for a trading day
//!   (04:00 ET -> 20:00 ET) the broker mark legitimately tracks live
//!   extended-hours quotes, so we sign with the current instant.
//! - Outside any active window (overnight, weekends, holidays) the broker
//!   keeps returning a frozen mark with no per-symbol timestamp. Signing
//!   `now()` would lie. We sign the most recent past `session_close`
//!   instead, and the strategy's `max-staleness` correctly rejects.
//!
//! This is the fix for RAI-693. See the original stopgap-doesn't-work
//! discussion: the alternative ("only re-stamp when current_price changes")
//! breaks SGOV/AMZN RTH false-positives because the broker mark can freeze
//! for 15-45 min during real trading on thin-to-mid tickers.

use crate::alpaca::AlpacaClient;
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, TimeZone, Utc};
use chrono_tz::US::Eastern;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// One trading day's extended-session boundaries, materialised to UTC
/// from the HHMM strings the Alpaca calendar returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWindow {
    pub date: NaiveDate,
    pub session_open: DateTime<Utc>,
    pub session_close: DateTime<Utc>,
}

/// Rolling cache of session windows around today. Refreshed periodically
/// in the background; read by `/context/v1` on every request.
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

    /// Choose `publish_time` for a sign at `now`.
    ///
    /// In an active window -> `now` (broker mark is genuinely live).
    /// Otherwise -> most recent past `session_close`.
    /// If no windows are cached yet (cold-start failure) -> `now`, which
    /// preserves pre-fix behaviour rather than blocking sign attempts.
    pub async fn publish_time_for(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let windows = self.windows.read().await;
        for w in windows.iter() {
            if w.session_open <= now && now < w.session_close {
                return now;
            }
        }
        windows
            .iter()
            .filter(|w| w.session_close <= now)
            .map(|w| w.session_close)
            .max()
            .unwrap_or(now)
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
        // 2026-05-29 (Fri): session 04:00-20:00 ET = 08:00-24:00 UTC during EDT (UTC-4).
        SessionWindow {
            date: date(2026, 5, 29),
            session_open: utc(2026, 5, 29, 8, 0),
            session_close: utc(2026, 5, 30, 0, 0),
        }
    }

    fn mon_window() -> SessionWindow {
        // 2026-06-01 (Mon): session 04:00-20:00 ET = 08:00-24:00 UTC during EDT.
        SessionWindow {
            date: date(2026, 6, 1),
            session_open: utc(2026, 6, 1, 8, 0),
            session_close: utc(2026, 6, 2, 0, 0),
        }
    }

    #[tokio::test]
    async fn inside_session_returns_now() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window()]).await;
        let now = utc(2026, 5, 29, 14, 30); // 10:30 ET Friday — RTH
        assert_eq!(c.publish_time_for(now).await, now);
    }

    #[tokio::test]
    async fn premarket_inside_session_returns_now() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window()]).await;
        let now = utc(2026, 5, 29, 9, 0); // 05:00 ET Friday — pre-market, still in session
        assert_eq!(c.publish_time_for(now).await, now);
    }

    #[tokio::test]
    async fn weekend_returns_last_friday_session_close() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window()]).await;
        let now = utc(2026, 5, 31, 12, 0); // Sunday noon UTC
        assert_eq!(c.publish_time_for(now).await, fri_window().session_close);
    }

    #[tokio::test]
    async fn holiday_monday_returns_previous_friday_close() {
        let c = MarketHoursCache::new();
        // Mon 2026-06-01 has no entry (simulated holiday); Tue has one.
        let tue_window = SessionWindow {
            date: date(2026, 6, 2),
            session_open: utc(2026, 6, 2, 8, 0),
            session_close: utc(2026, 6, 3, 0, 0),
        };
        c.set(vec![fri_window(), tue_window]).await;
        let now = utc(2026, 6, 1, 14, 0); // Mon 10:00 ET — would be RTH on a non-holiday
        assert_eq!(c.publish_time_for(now).await, fri_window().session_close);
    }

    #[tokio::test]
    async fn overnight_weekday_returns_previous_session_close() {
        let c = MarketHoursCache::new();
        c.set(vec![fri_window(), mon_window()]).await;
        // Mon 02:00 ET = Mon 06:00 UTC — before Monday's 04:00 ET session_open
        let now = utc(2026, 6, 1, 6, 0);
        assert_eq!(c.publish_time_for(now).await, fri_window().session_close);
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
        // At session_close exactly, we're outside the window and should
        // emit the close timestamp itself (which equals `now` here, but
        // would diverge a moment later).
        assert_eq!(c.publish_time_for(exactly_close).await, exactly_close);
    }

    #[tokio::test]
    async fn empty_cache_falls_back_to_now() {
        let c = MarketHoursCache::new();
        let now = utc(2026, 5, 31, 12, 0);
        assert_eq!(c.publish_time_for(now).await, now);
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
}
