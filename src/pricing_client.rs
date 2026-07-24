//! Minimal WebSocket client for st0x.pricing.
//!
//! Wire types come from the public
//! [`st0x-pricing-types`](https://github.com/ST0x-Technology/st0x.pricing-types)
//! crate; this file holds only the consumer-side glue (auto-reconnecting
//! WS session that stashes the latest `Quote` per asset). Mirror of
//! st0x.bebop's `src/pricing_client.rs` — same shape, same retries.
//!
//! We can't depend on `st0x.pricing/crates/pricing-client` directly —
//! that crate lives in the private pricing repo and can't be resolved
//! across the GITHUB_TOKEN scope wall. Recreating the reconnect loop
//! here is cheaper than the cross-repo auth ceremony.

use futures_util::{SinkExt as _, StreamExt as _};
use http::HeaderValue;
use st0x_pricing_types::{ClientFrame, PongFrame, Quote, ServerFrame, SubscribeFrame, Symbol};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    #[error("CBOR encode/decode error: {0}")]
    Cbor(String),
    #[error("invalid header value: {0}")]
    Header(String),
}

#[derive(Debug, Clone)]
pub struct LiveClientConfig {
    pub ws_url: String,
    pub api_key: String,
    pub consumer: String,
    pub assets: Vec<Symbol>,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl LiveClientConfig {
    pub fn new(
        ws_url: impl Into<String>,
        api_key: impl Into<String>,
        consumer: impl Into<String>,
        assets: Vec<Symbol>,
    ) -> Self {
        Self {
            ws_url: ws_url.into(),
            api_key: api_key.into(),
            consumer: consumer.into(),
            assets,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Background subscriber. Spawns one task that connects, subscribes,
/// reads price frames, and stashes the latest per-asset `Quote` in a
/// shared `RwLock<HashMap>`. Auto-reconnects with exponential backoff.
#[derive(Clone)]
pub struct LiveClient {
    cache: Arc<RwLock<HashMap<Symbol, Quote>>>,
}

impl LiveClient {
    pub fn spawn(cfg: LiveClientConfig) -> Self {
        let cache = Arc::new(RwLock::new(HashMap::new()));
        let task_cache = cache.clone();
        tokio::spawn(async move { run_loop(cfg, task_cache).await });
        Self { cache }
    }

    /// Test-only constructor that builds a `LiveClient` with a
    /// pre-populated cache and no background task. The integration
    /// tests seed deterministic `Quote`s here instead of standing up
    /// a real pricing WS server.
    pub async fn with_seeded(quotes: Vec<Quote>) -> Self {
        let mut map = HashMap::with_capacity(quotes.len());
        for q in quotes {
            map.insert(q.asset.clone(), q);
        }
        Self {
            cache: Arc::new(RwLock::new(map)),
        }
    }

    pub async fn latest(&self, symbol: &str) -> Option<Quote> {
        self.cache.read().await.get(symbol).cloned()
    }

    /// Snapshot multiple symbols under a single read lock so every
    /// element of a batch HTTP response is built from a coherent view
    /// of the WS cache. Mirrors `cache::QuoteCache::snapshot_many` from
    /// the pre-pricing-client world. Symbols missing from the cache are
    /// simply absent in the returned map.
    pub async fn snapshot_many(&self, symbols: &[&str]) -> HashMap<String, Quote> {
        let guard = self.cache.read().await;
        let mut out = HashMap::with_capacity(symbols.len());
        for sym in symbols {
            if let Some(q) = guard.get(*sym) {
                out.insert((*sym).to_string(), q.clone());
            }
        }
        out
    }

    /// Newest `source_ts_unix_ms` across all cached quotes. `None` if
    /// the cache is empty (no `Price` frame received yet). Used by the
    /// `oracle_cache_freshness_seconds` gauge: dashboard wants seconds
    /// since the most-recently-refreshed quote, so the caller does
    /// `now_ms - newest_source_ts` and divides by 1000.
    pub async fn newest_source_ts_ms(&self) -> Option<i64> {
        self.cache
            .read()
            .await
            .values()
            .map(|q| q.source_ts_unix_ms)
            .max()
    }

    /// Returns the set of subscribed symbols not yet seen on the wire.
    /// Used by /status so an operator can spot a half-warm cache without
    /// parsing logs.
    pub async fn missing(&self, symbols: &[String]) -> Vec<String> {
        let guard = self.cache.read().await;
        symbols
            .iter()
            .filter(|s| !guard.contains_key(s.as_str()))
            .cloned()
            .collect()
    }
}

async fn run_loop(cfg: LiveClientConfig, cache: Arc<RwLock<HashMap<Symbol, Quote>>>) {
    let mut backoff = cfg.initial_backoff;
    loop {
        match connect_and_run(&cfg, &cache).await {
            Ok(()) => {
                tracing::info!("Pricing WS session ended cleanly; reconnecting");
                backoff = cfg.initial_backoff;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Pricing WS session error; backoff {:?}", backoff);
                ::metrics::counter!(
                    "oracle_upstream_failure_total",
                    "kind" => "pricing_ws",
                )
                .increment(1);
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, cfg.max_backoff);
            }
        }
    }
}

fn encode_cbor<T: serde::Serialize>(v: &T) -> Result<Vec<u8>, ClientError> {
    let mut buf = Vec::new();
    ciborium::into_writer(v, &mut buf).map_err(|e| ClientError::Cbor(e.to_string()))?;
    Ok(buf)
}

async fn connect_and_run(
    cfg: &LiveClientConfig,
    cache: &Arc<RwLock<HashMap<Symbol, Quote>>>,
) -> Result<(), ClientError> {
    let mut req = cfg
        .ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| ClientError::WebSocket(format!("{e}")))?;
    let bearer = format!("Bearer {}", cfg.api_key);
    req.headers_mut().insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&bearer).map_err(|e| ClientError::Header(format!("{e}")))?,
    );
    let (mut socket, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| ClientError::WebSocket(format!("{e}")))?;

    let sub = ClientFrame::Subscribe(SubscribeFrame {
        consumer: cfg.consumer.clone(),
        assets: cfg.assets.clone(),
    });
    socket
        .send(WsMessage::Binary(encode_cbor(&sub)?))
        .await
        .map_err(|e| ClientError::WebSocket(format!("{e}")))?;

    while let Some(msg) = socket.next().await {
        match msg {
            Ok(WsMessage::Binary(b)) => {
                let frame: ServerFrame = match ciborium::from_reader(&b[..]) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(error = %e, "Bad pricing WS frame; ignoring");
                        continue;
                    }
                };
                match frame {
                    ServerFrame::Price(p) => {
                        let q = Quote {
                            asset: p.asset.clone(),
                            chain_id: p.chain_id,
                            base: p.base,
                            quote: p.quote,
                            rate_base_to_quote: p.rate_base_to_quote,
                            rate_quote_to_base: p.rate_quote_to_base,
                            expiry_unix_ms: p.expiry_unix_ms,
                            source_ts_unix_ms: p.source_ts_unix_ms,
                        };
                        cache.write().await.insert(p.asset, q);
                    }
                    ServerFrame::Error(e) => {
                        tracing::warn!(?e.code, asset = ?e.asset, detail = ?e.detail, "Pricing server error frame");
                        ::metrics::counter!(
                            "oracle_upstream_failure_total",
                            "kind" => "pricing_error_frame",
                        )
                        .increment(1);
                    }
                    ServerFrame::Ping(p) => {
                        let pong = ClientFrame::Pong(PongFrame {
                            ts_unix_ms: p.ts_unix_ms,
                        });
                        if let Ok(buf) = encode_cbor(&pong) {
                            let _ = socket.send(WsMessage::Binary(buf)).await;
                        }
                    }
                }
            }
            Ok(WsMessage::Ping(payload)) => {
                let _ = socket.send(WsMessage::Pong(payload)).await;
            }
            Ok(_) => {}
            Err(e) => return Err(ClientError::WebSocket(format!("{e}"))),
        }
    }
    Ok(())
}
