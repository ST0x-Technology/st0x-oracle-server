use crate::alpaca::{AlpacaClient, QuoteData};
use crate::market_hours::MarketHoursCache;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// In-memory cache of the latest known mark for each symbol.
///
/// On poll failure we intentionally leave the previous entry untouched so
/// that requests can serve the last-known-good mark (with its as-of
/// timestamp frozen at the last successful poll) rather than hard-failing.
/// Because that stale timestamp is signed as `publish_time`, the
/// strategy's `max-staleness` bounds how long a frozen mark stays usable.
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
    /// `get(...)` calls in the same batch, mixing prices for the same
    /// symbol within one response. Returned map only contains entries
    /// that are currently cached — missing symbols are simply absent.
    pub async fn snapshot_many(&self, symbols: &[&str]) -> HashMap<String, QuoteData> {
        let guard = self.entries.read().await;
        let mut out = HashMap::with_capacity(symbols.len());
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

/// Fetch every position once via the Broker API and update the cache
/// for any registered symbol the issuer holds. Used both to prime the
/// cache at startup and as the per-tick body of the poll loop.
///
/// Symbols not held by the broker (or dropped during parsing) are
/// logged but not removed from the cache — `/context/v1` will continue
/// serving the last-known-good mark until `max-staleness` rejects it.
///
/// The mark's `t` (its as-of timestamp, later signed as `publish_time`)
/// is computed here via `market_hours`: the fetch instant in-session, the
/// last `session_close` out-of-session. Deciding this at fetch time means
/// a stalled poll simply stops refreshing `t`, so the signed timestamp
/// ages out on its own.
pub async fn poll_once(
    cache: &QuoteCache,
    alpaca: &AlpacaClient,
    symbols: &[String],
    market_hours: &MarketHoursCache,
) {
    if market_hours.window_count().await == 0 {
        tracing::warn!("market-hours cache is empty; keeping previous quotes");
        return;
    }
    let now = Utc::now();
    let as_of = market_hours.publish_time_for(now).await;
    let active_session_start = market_hours.active_signed_session_start_for(now).await;
    let marks = match alpaca.fetch_marks(as_of).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "Broker positions fetch failed; keeping previous cache");
            return;
        }
    };

    for symbol in symbols {
        match marks.get(symbol) {
            Some(q) => {
                let previous = cache.get(symbol).await;
                if let (None, Some(session_start)) = (previous.as_ref(), active_session_start) {
                    tracing::warn!(
                        symbol = %symbol,
                        price = q.price,
                        session_start = %session_start,
                        "no previous quote during active signed session; seeding quote before session start"
                    );
                    cache
                        .update(symbol, untrusted_session_seed_quote(q, session_start))
                        .await;
                    continue;
                }
                if should_keep_previous_quote_at_session_open(
                    previous.as_ref(),
                    q.price,
                    active_session_start,
                ) {
                    let previous = previous
                        .as_ref()
                        .expect("previous is Some when the guard returns true");
                    let session_start = active_session_start
                        .expect("active_session_start is Some when the guard returns true");
                    tracing::warn!(
                        symbol = %symbol,
                        price = q.price,
                        session_start = %session_start,
                        previous_ts = %previous.t,
                        "broker mark unchanged since before session open; keeping previous timestamp"
                    );
                    continue;
                }
                tracing::debug!(symbol = %symbol, price = q.price, t = %q.t, "Polled broker mark");
                cache.update(symbol, q.clone()).await;
            }
            None => {
                tracing::warn!(
                    symbol = %symbol,
                    "Broker has no position for this symbol; keeping previous entry"
                );
            }
        }
    }
}

/// Spawn a background task that polls the broker every `interval` and
/// refreshes every configured symbol. Returns immediately; task runs
/// until the process exits.
pub fn spawn_poll_loop(
    cache: Arc<QuoteCache>,
    alpaca: AlpacaClient,
    symbols: Vec<String>,
    market_hours: Arc<MarketHoursCache>,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // tokio's default is `Burst`, which queues missed ticks and fires
        // them back-to-back when the task catches up. If a poll ever
        // overruns the interval (slow broker, network blip), that would
        // hammer the API with rapid catch-up calls and risk rate
        // limiting. Skip just rebases onto the current schedule.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // first tick is immediate; we already prime synchronously at
        // startup, so skip it to avoid a double-poll right at boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            poll_once(&cache, &alpaca, &symbols, &market_hours).await;
        }
    });
}

fn should_keep_previous_quote_at_session_open(
    previous: Option<&QuoteData>,
    current_price: f64,
    active_session_start: Option<DateTime<Utc>>,
) -> bool {
    let (Some(previous), Some(session_start)) = (previous, active_session_start) else {
        return false;
    };
    previous.t < session_start && previous.price.to_bits() == current_price.to_bits()
}

fn untrusted_session_seed_quote(q: &QuoteData, session_start: DateTime<Utc>) -> QuoteData {
    QuoteData {
        price: q.price,
        t: session_start - chrono::Duration::milliseconds(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn quote(price: f64, ts: i64) -> QuoteData {
        QuoteData {
            price,
            t: Utc.timestamp_opt(ts, 0).unwrap(),
        }
    }

    #[tokio::test]
    async fn update_and_get_roundtrip() {
        let cache = QuoteCache::new();
        cache.update("COIN", quote(100.0, 1_700_000_000)).await;
        let got = cache.get("COIN").await.expect("expected cached quote");
        assert_eq!(got.price, 100.0);
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
        cache.update("COIN", quote(100.0, 1)).await;
        cache.update("COIN", quote(200.0, 2)).await;
        let got = cache.get("COIN").await.unwrap();
        assert_eq!(got.price, 200.0);
        assert_eq!(got.t.timestamp(), 2);
    }

    #[test]
    fn session_open_gate_holds_unchanged_pre_session_quote() {
        let previous = quote(100.0, 1_000);
        let session_start = Utc.timestamp_opt(2_000, 0).unwrap();

        assert!(should_keep_previous_quote_at_session_open(
            Some(&previous),
            100.0,
            Some(session_start)
        ));
    }

    #[test]
    fn session_open_gate_accepts_changed_quote() {
        let previous = quote(100.0, 1_000);
        let session_start = Utc.timestamp_opt(2_000, 0).unwrap();

        assert!(!should_keep_previous_quote_at_session_open(
            Some(&previous),
            100.01,
            Some(session_start)
        ));
    }

    #[test]
    fn session_open_gate_accepts_quotes_already_seen_this_session() {
        let previous = quote(100.0, 2_000);
        let session_start = Utc.timestamp_opt(2_000, 0).unwrap();

        assert!(!should_keep_previous_quote_at_session_open(
            Some(&previous),
            100.0,
            Some(session_start)
        ));
    }

    #[test]
    fn untrusted_session_seed_keeps_equal_quotes_blocked() {
        let session_start = Utc.timestamp_opt(2_000, 0).unwrap();
        let current = quote(100.0, 2_100);
        let previous = untrusted_session_seed_quote(&current, session_start);

        assert!(should_keep_previous_quote_at_session_open(
            Some(&previous),
            current.price,
            Some(session_start)
        ));
    }
}
