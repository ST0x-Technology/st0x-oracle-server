use crate::alpaca::{AlpacaClient, QuoteData};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// In-memory cache of the latest known quote for each symbol.
///
/// On poll failure we intentionally leave the previous entry untouched so
/// that `/context/v1` can serve the last-known-good quote (with its
/// original Alpaca `t` timestamp) rather than hard-failing. The strategy
/// is responsible for bounding staleness via `max-staleness`.
#[derive(Debug, Default)]
pub struct QuoteCache {
    entries: RwLock<HashMap<String, QuoteData>>,
}

impl QuoteCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn update(&self, symbol: &str, q: QuoteData) {
        self.entries.write().await.insert(symbol.to_string(), q);
    }

    pub async fn get(&self, symbol: &str) -> Option<QuoteData> {
        self.entries.read().await.get(symbol).cloned()
    }
}

/// Fetch every symbol once, updating the cache on success. Used both to
/// prime the cache at startup and as the per-tick body of the poll loop.
pub async fn poll_once(cache: &QuoteCache, alpaca: &AlpacaClient, symbols: &[String]) {
    for symbol in symbols {
        match alpaca.latest_quote(symbol).await {
            Ok(q) => {
                tracing::debug!(
                    symbol = %symbol,
                    bid = q.bid_price,
                    ask = q.ask_price,
                    t = %q.t,
                    "Polled Alpaca quote"
                );
                cache.update(symbol, q).await;
            }
            Err(e) => {
                tracing::warn!(
                    symbol = %symbol,
                    error = %e,
                    "Failed to poll Alpaca quote; keeping previous entry"
                );
            }
        }
    }
}

/// Spawn a background task that polls Alpaca every `interval` for every
/// configured symbol. Returns immediately; task runs until the process
/// exits.
pub fn spawn_poll_loop(
    cache: Arc<QuoteCache>,
    alpaca: AlpacaClient,
    symbols: Vec<String>,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // first tick is immediate; we already prime synchronously at
        // startup, so skip it to avoid a double-poll right at boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            poll_once(&cache, &alpaca, &symbols).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn quote(bid: f64, ask: f64, ts: i64) -> QuoteData {
        QuoteData {
            bid_price: bid,
            ask_price: ask,
            t: Utc.timestamp_opt(ts, 0).unwrap(),
        }
    }

    #[tokio::test]
    async fn update_and_get_roundtrip() {
        let cache = QuoteCache::new();
        cache
            .update("COIN", quote(100.0, 101.0, 1_700_000_000))
            .await;
        let got = cache.get("COIN").await.expect("expected cached quote");
        assert_eq!(got.bid_price, 100.0);
        assert_eq!(got.ask_price, 101.0);
        assert_eq!(got.t.timestamp(), 1_700_000_000);
    }

    #[tokio::test]
    async fn missing_symbol_returns_none() {
        let cache = QuoteCache::new();
        assert!(cache.get("UNKNOWN").await.is_none());
    }

    #[tokio::test]
    async fn update_overwrites_previous() {
        let cache = QuoteCache::new();
        cache.update("COIN", quote(100.0, 101.0, 1)).await;
        cache.update("COIN", quote(200.0, 201.0, 2)).await;
        let got = cache.get("COIN").await.unwrap();
        assert_eq!(got.bid_price, 200.0);
        assert_eq!(got.t.timestamp(), 2);
    }
}
