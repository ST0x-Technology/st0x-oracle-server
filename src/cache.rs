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

    /// Snapshot multiple symbols under a single read lock.
    ///
    /// Used by the batch handler so that all elements of one HTTP
    /// response are built from a coherent view of the cache. Without
    /// this, the poll loop could update one entry between two
    /// `get(...)` calls in the same batch, mixing quotes for the same
    /// symbol within one response. Returned map only contains entries
    /// that are currently cached — missing symbols are simply absent.
    pub async fn snapshot_many(
        &self,
        symbols: &[&str],
    ) -> std::collections::HashMap<String, QuoteData> {
        let guard = self.entries.read().await;
        let mut out = std::collections::HashMap::with_capacity(symbols.len());
        for sym in symbols {
            if let Some(q) = guard.get(*sym) {
                out.insert((*sym).to_string(), q.clone());
            }
        }
        out
    }

    /// Returns the set of symbols not currently cached. Used at startup
    /// to gate readiness on every configured symbol being warm.
    pub async fn missing(&self, symbols: &[String]) -> Vec<String> {
        let guard = self.entries.read().await;
        symbols
            .iter()
            .filter(|s| !guard.contains_key(s.as_str()))
            .cloned()
            .collect()
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
        // tokio's default is `Burst`, which queues missed ticks and fires
        // them back-to-back when the task catches up. If a poll ever
        // overruns the interval (slow Alpaca, network blip), that would
        // hammer Alpaca with rapid catch-up calls and risk rate limiting.
        // Skip just rebases onto the current schedule.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
