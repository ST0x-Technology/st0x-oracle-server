//! Alpaca Broker API client — calendar-only.
//!
//! After the pricing-client integration (RAI-360) the oracle no longer
//! polls Alpaca for reference prices; live quotes arrive over the
//! st0x-pricing WebSocket. We still hit Alpaca's `/v1/calendar`
//! endpoint, though, because the pricing-service `Quote` doesn't carry
//! session boundaries — `MarketHoursCache` needs them to decide whether
//! `publish_time` should be `now` or the most recent `session_close`
//! (RAI-693).
//!
//! Only `ALPACA_API_KEY_ID` + `ALPACA_API_SECRET_KEY` are needed; the
//! pre-RAI-360 `ALPACA_BROKER_ACCOUNT_ID` is gone.

use crate::market_hours::{anchor_session_to_utc, SessionWindow};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::NaiveDate;
use serde::Deserialize;

const BROKER_BASE_URL: &str = "https://broker-api.alpaca.markets";

#[derive(Debug, Clone)]
pub struct AlpacaClient {
    api_key: String,
    api_secret: String,
    broker_url: String,
    http: reqwest::Client,
}

impl AlpacaClient {
    pub fn new(api_key: &str, api_secret: &str) -> Self {
        Self::with_url(api_key, api_secret, BROKER_BASE_URL)
    }

    pub fn with_url(api_key: &str, api_secret: &str, broker_url: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
            broker_url: broker_url.to_string(),
            http: reqwest::Client::new(),
        }
    }

    fn basic_auth(&self) -> String {
        let encoded = BASE64.encode(format!("{}:{}", self.api_key, self.api_secret));
        format!("Basic {encoded}")
    }

    /// Fetch the trading calendar over `[start, end]` (inclusive). Each
    /// returned `SessionWindow` carries the extended-hours boundaries
    /// (04:00 ET pre-market open and 20:00 ET after-hours close) as UTC
    /// instants. Non-trading dates simply aren't in the response. Used
    /// by `MarketHoursCache` to decide whether `publish_time` should be
    /// `now` or the last `session_close`.
    pub async fn fetch_calendar(
        &self,
        start: NaiveDate,
        end: NaiveDate,
    ) -> anyhow::Result<Vec<SessionWindow>> {
        let url = format!(
            "{}/v1/calendar?start={}&end={}",
            self.broker_url, start, end
        );
        let raw: Vec<CalendarResponse> = self
            .http
            .get(&url)
            .header("Authorization", self.basic_auth())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        raw.into_iter().map(calendar_row_to_window).collect()
    }
}

#[derive(Debug, Deserialize)]
struct CalendarResponse {
    date: String,
    /// "HH:MM" in ET — RTH open (usually "09:30").
    open: String,
    /// "HH:MM" in ET — RTH close (usually "16:00").
    close: String,
    /// "HHMM" in ET — extended-session open (pre-market start, usually "0400").
    session_open: String,
    /// "HHMM" in ET — extended-session close (after-hours end, usually "2000").
    session_close: String,
}

fn calendar_row_to_window(r: CalendarResponse) -> anyhow::Result<SessionWindow> {
    let date = NaiveDate::parse_from_str(&r.date, "%Y-%m-%d")
        .map_err(|e| anyhow::anyhow!("invalid calendar date {:?}: {}", r.date, e))?;
    // RTH `open` / `close` are emitted as "HH:MM"; the extended-session
    // boundaries as "HHMM". `anchor_session_to_utc` wants 4-char "HHMM",
    // so strip the colon for RTH before anchoring.
    let rth_open_hhmm = r.open.replace(':', "");
    let rth_close_hhmm = r.close.replace(':', "");
    Ok(SessionWindow {
        date,
        session_open: anchor_session_to_utc(date, &r.session_open)?,
        rth_open: anchor_session_to_utc(date, &rth_open_hhmm)?,
        rth_close: anchor_session_to_utc(date, &rth_close_hhmm)?,
        session_close: anchor_session_to_utc(date, &r.session_close)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_auth_encodes_key_secret() {
        let c = AlpacaClient::with_url("CK1234", "SECRET", "http://example");
        // base64("CK1234:SECRET") = Q0sxMjM0OlNFQ1JFVA==
        assert_eq!(c.basic_auth(), "Basic Q0sxMjM0OlNFQ1JFVA==");
    }
}
