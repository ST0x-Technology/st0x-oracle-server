use serde::Deserialize;

const ALPACA_DATA_URL: &str = "https://data.alpaca.markets";

#[derive(Debug, Clone)]
pub struct AlpacaClient {
    api_key_id: String,
    api_secret_key: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct QuoteData {
    /// Best bid price
    pub bid_price: f64,
    /// Best ask price
    pub ask_price: f64,
}

#[derive(Deserialize)]
struct LatestQuoteResponse {
    quote: QuoteInfo,
}

#[derive(Deserialize)]
struct QuoteInfo {
    /// Ask price
    ap: f64,
    /// Bid price
    bp: f64,
}

impl AlpacaClient {
    pub fn new(api_key_id: &str, api_secret_key: &str) -> Self {
        Self {
            api_key_id: api_key_id.to_string(),
            api_secret_key: api_secret_key.to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Fetch the latest NBBO quote for a given stock symbol.
    pub async fn latest_quote(&self, symbol: &str) -> anyhow::Result<QuoteData> {
        let url = format!("{}/v2/stocks/{}/quotes/latest", ALPACA_DATA_URL, symbol);

        let resp: LatestQuoteResponse = self
            .http
            .get(&url)
            .header("APCA-API-KEY-ID", &self.api_key_id)
            .header("APCA-API-SECRET-KEY", &self.api_secret_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(QuoteData {
            bid_price: resp.quote.bp,
            ask_price: resp.quote.ap,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_quote() {
        let json = r#"{"quote":{"t":"2024-01-15T20:00:00Z","ax":"V","ap":185.42,"as":2,"bx":"Q","bp":185.40,"bs":1,"c":["R"],"z":"C"}}"#;
        let resp: LatestQuoteResponse = serde_json::from_str(json).unwrap();
        assert!((resp.quote.ap - 185.42).abs() < 0.001);
        assert!((resp.quote.bp - 185.40).abs() < 0.001);
    }
}
