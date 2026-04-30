//! Alpaca Broker API client for reference prices.
//!
//! We read each registered symbol's `current_price` from the issuer's
//! brokerage positions, not from the Market Data API's `/quotes/latest`
//! endpoint. Background:
//!
//! - The Market Data API's free / basic feeds (`iex`, `delayed_sip`)
//!   either give one-venue quotes that drift well off NBBO during
//!   illiquid hours (IEX) or sit 15 min behind (delayed_sip).
//! - Real-time `feed=sip` requires a separate paid Market Data
//!   subscription tier.
//! - The Broker API's positions endpoint exposes Alpaca's internal
//!   real-time mark for every position the issuer holds, derived from
//!   the same NBBO the broker uses to value real money. No extra
//!   subscription needed beyond the broker account.
//!
//! Mirror of the approach in `st0x.liquidity-monitor`'s
//! `src/prices/alpaca_positions.rs`.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;

const BROKER_BASE_URL: &str = "https://broker-api.alpaca.markets";

#[derive(Debug, Clone)]
pub struct AlpacaClient {
    api_key: String,
    api_secret: String,
    account_id: String,
    broker_url: String,
    http: reqwest::Client,
}

/// One reference price for one symbol.
///
/// `price` is the broker-side mark at fetch time (Alpaca's internal
/// real-time NBBO-driven valuation of the position). `t` is when *we*
/// fetched the response — the broker doesn't expose a per-mark
/// timestamp, so the freshest "as-of" we can sign is our fetch time.
/// Polling cadence (config.poll_interval_secs) bounds how stale `t` can
/// be relative to the broker's actual mark.
#[derive(Debug, Clone)]
pub struct QuoteData {
    pub price: f64,
    pub t: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct PositionResponse {
    symbol: String,
    /// Stringified decimal — the broker returns numeric fields as strings.
    current_price: String,
}

impl AlpacaClient {
    pub fn new(api_key: &str, api_secret: &str, account_id: &str) -> Self {
        Self::with_url(api_key, api_secret, account_id, BROKER_BASE_URL)
    }

    pub fn with_url(api_key: &str, api_secret: &str, account_id: &str, broker_url: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
            account_id: account_id.to_string(),
            broker_url: broker_url.to_string(),
            http: reqwest::Client::new(),
        }
    }

    fn basic_auth(&self) -> String {
        let encoded = BASE64.encode(format!("{}:{}", self.api_key, self.api_secret));
        format!("Basic {encoded}")
    }

    /// Fetch every position for the configured account in one call and
    /// return a map of `symbol → QuoteData`. Symbols the issuer doesn't
    /// hold simply won't appear in the result; the caller decides how to
    /// react (we treat absence as "no fresh data this tick — keep the
    /// last-known mark").
    ///
    /// Position rows with a non-positive or non-numeric `current_price`
    /// are dropped so downstream signing never sees a bad mark.
    pub async fn fetch_marks(&self) -> anyhow::Result<HashMap<String, QuoteData>> {
        let url = format!(
            "{}/v1/trading/accounts/{}/positions",
            self.broker_url, self.account_id
        );

        let positions: Vec<PositionResponse> = self
            .http
            .get(&url)
            .header("Authorization", self.basic_auth())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(positions_to_marks(positions, Utc::now()))
    }
}

/// Pure transform from the broker's positions response to the cache
/// shape. Split out so it can be exercised without an HTTP round-trip.
fn positions_to_marks(
    positions: Vec<PositionResponse>,
    fetch_time: DateTime<Utc>,
) -> HashMap<String, QuoteData> {
    let mut out = HashMap::with_capacity(positions.len());
    for p in positions {
        match p.current_price.parse::<f64>() {
            Ok(price) if price > 0.0 => {
                out.insert(
                    p.symbol,
                    QuoteData {
                        price,
                        t: fetch_time,
                    },
                );
            }
            Ok(price) => {
                tracing::warn!(
                    symbol = %p.symbol,
                    price = price,
                    "Dropping non-positive broker mark"
                );
            }
            Err(e) => {
                tracing::warn!(
                    symbol = %p.symbol,
                    raw = %p.current_price,
                    error = %e,
                    "Failed to parse current_price; dropping"
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real positions response shape captured from the live Broker API
    /// for the issuer's account. Exercises the fields we care about
    /// plus extras we ignore.
    const SAMPLE_POSITIONS_JSON: &str = r#"[
        {
            "asset_id": "de29752c-29ea-479c-8abe-5fca106af9e6",
            "symbol": "SPYM",
            "exchange": "ARCA",
            "asset_class": "us_equity",
            "asset_marginable": true,
            "qty": "741.476711632",
            "qty_available": "741.476711632",
            "avg_entry_price": "80.878548",
            "side": "long",
            "market_value": "62180.014594",
            "cost_basis": "59969.559813",
            "unrealized_pl": "2210.454781",
            "unrealized_plpc": "0.03686",
            "unrealized_intraday_pl": "73.925228",
            "unrealized_intraday_plpc": "0.00119",
            "current_price": "83.8597",
            "lastday_price": "83.76",
            "change_today": "0.00119"
        },
        {
            "asset_id": "0000",
            "symbol": "COIN",
            "qty": "10",
            "current_price": "182.00"
        }
    ]"#;

    #[test]
    fn deserialize_positions_response_picks_symbol_and_current_price() {
        let positions: Vec<PositionResponse> = serde_json::from_str(SAMPLE_POSITIONS_JSON).unwrap();
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].symbol, "SPYM");
        assert_eq!(positions[0].current_price, "83.8597");
        assert_eq!(positions[1].symbol, "COIN");
        assert_eq!(positions[1].current_price, "182.00");
    }

    #[test]
    fn positions_to_marks_keeps_well_formed_rows() {
        let positions: Vec<PositionResponse> = serde_json::from_str(SAMPLE_POSITIONS_JSON).unwrap();
        let now = Utc::now();
        let marks = positions_to_marks(positions, now);
        assert_eq!(marks.len(), 2);
        assert_eq!(marks["SPYM"].price, 83.8597);
        assert_eq!(marks["COIN"].price, 182.00);
        assert_eq!(marks["SPYM"].t, now);
    }

    #[test]
    fn positions_to_marks_drops_zero_negative_and_unparseable() {
        let raw = r#"[
            {"symbol": "OK", "current_price": "10.5"},
            {"symbol": "ZERO", "current_price": "0"},
            {"symbol": "NEG", "current_price": "-1.2"},
            {"symbol": "JUNK", "current_price": "not-a-number"}
        ]"#;
        let positions: Vec<PositionResponse> = serde_json::from_str(raw).unwrap();
        let marks = positions_to_marks(positions, Utc::now());
        assert_eq!(marks.len(), 1, "only OK survives");
        assert_eq!(marks["OK"].price, 10.5);
    }

    #[test]
    fn basic_auth_encodes_key_secret() {
        let c = AlpacaClient::with_url("CK1234", "SECRET", "acc-id", "http://example");
        // base64("CK1234:SECRET") = Q0sxMjM0OlNFQ1JFVA==
        assert_eq!(c.basic_auth(), "Basic Q0sxMjM0OlNFQ1JFVA==");
    }
}
