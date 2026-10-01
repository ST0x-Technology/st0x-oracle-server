//! Minimal WebSocket client for st0x.pricing.
//!
//! Wire types come from the public
//! [`st0x-pricing-types`](https://github.com/ST0x-Technology/st0x.pricing-types)
//! crate; this file holds only the consumer-side glue (auto-reconnecting
//! WS session that stashes the latest `Quote` per chain and asset). Mirror of
//! st0x.bebop's `src/pricing_client.rs` — same shape, same retries.
//!
//! We can't depend on `st0x.pricing/crates/pricing-client` directly —
//! that crate lives in the private pricing repo and can't be resolved
//! across the GITHUB_TOKEN scope wall. Recreating the reconnect loop
//! here is cheaper than the cross-repo auth ceremony.

use futures_util::{SinkExt as _, StreamExt as _};
use http::HeaderValue;
use st0x_pricing_types::{
    ClientFrame, ErrorCode, PongFrame, Quote, ServerFrame, SubscribeFrame, Symbol, UnsubscribeFrame,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{watch, RwLock};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

/// Longest we let the socket sit silent before declaring it dead. The
/// pricing server heartbeats every 15s, so this is four missed heartbeats —
/// generous against jitter, still under a minute to detect a frozen feed.
const READ_DEADLINE: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    #[error("CBOR encode/decode error: {0}")]
    Cbor(String),
    #[error("invalid header value: {0}")]
    Header(String),
    #[error("id-token error: {0}")]
    IdToken(String),
}

#[derive(Debug, Clone)]
pub struct LiveClientConfig {
    pub ws_url: String,
    pub api_key: String,
    pub consumer: String,
    /// The symbols to subscribe to. A session sends the difference as
    /// `Subscribe` / `Unsubscribe` when the value changes, and a price frame
    /// for a symbol not in the current value is dropped.
    pub assets: watch::Receiver<Vec<Symbol>>,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// When true, authenticate the WS handshake with a Google ID token minted
    /// from the runtime service account (Cloud Run IAM), instead of the
    /// app-level `api_key`. Set for the GCP deployment, where pricing is a
    /// private Cloud Run service gated by IAM; false on the tailnet/local,
    /// where pricing checks the API key itself. No secret either way — the ID
    /// token is fetched on the fly from the metadata server.
    pub iam_auth: bool,
    /// The chain this deployment serves, from `config.chain_id`. Frames for
    /// other chains stay in the cache — the subscription is per-symbol, not
    /// per-chain — but only this chain's quotes are ever handed out.
    pub chain_id: u64,
}

impl LiveClientConfig {
    pub fn new(
        ws_url: impl Into<String>,
        api_key: impl Into<String>,
        consumer: impl Into<String>,
        assets: Vec<Symbol>,
        chain_id: u64,
    ) -> Self {
        // A fixed list: the sender is dropped, so the session never sees a
        // change. `with_assets` takes a live one.
        let (_, assets) = watch::channel(assets);
        Self {
            ws_url: ws_url.into(),
            api_key: api_key.into(),
            consumer: consumer.into(),
            assets,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            iam_auth: false,
            chain_id,
        }
    }

    /// Follow a symbol list that can change while the client runs.
    #[must_use]
    pub fn with_assets(mut self, assets: watch::Receiver<Vec<Symbol>>) -> Self {
        self.assets = assets;
        self
    }

    /// Authenticate with a Cloud Run IAM ID token instead of the API key.
    #[must_use]
    pub fn with_iam_auth(mut self, on: bool) -> Self {
        self.iam_auth = on;
        self
    }
}

/// Latest quote per `(chain_id, asset)`. st0x.pricing publishes one frame
/// per chain and symbol, so a symbol-only key makes two chains' frames for
/// the same asset last-write-wins and lets this server sign a context
/// against another chain's rate. Nothing else catches that: both frames are
/// individually fresh, in-expiry and correctly signed, so the difference is
/// invisible to every staleness and expiry check downstream.
struct CachedQuote {
    quote: Quote,
    generation: QuoteGeneration,
}

impl CachedQuote {
    fn snapshot(&self) -> QuoteSnapshot {
        QuoteSnapshot {
            quote: self.quote.clone(),
            generation: self.generation.clone(),
        }
    }

    fn revoke(&self) {
        self.generation.revoke();
    }
}

struct GenerationState {
    live: AtomicBool,
    store_gate: Mutex<()>,
}

/// Identity and liveness for one uninterrupted run of price frames. Reuse
/// entries retain this token so an invalidated generation cannot cross into a
/// replacement generation even when both frames state the same price.
#[derive(Clone)]
pub(crate) struct QuoteGeneration {
    state: Arc<GenerationState>,
}

impl QuoteGeneration {
    fn new() -> Self {
        Self {
            state: Arc::new(GenerationState {
                live: AtomicBool::new(true),
                store_gate: Mutex::new(()),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self::new()
    }

    pub(crate) fn is_live(&self) -> bool {
        self.state.live.load(Ordering::Acquire)
    }

    pub(crate) fn is_same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// Run `operation` only while this generation is live. The gate makes the
    /// operation linearizable with revocation: an operation that starts after
    /// revocation is refused, while revocation waits for an already-started
    /// operation and makes its result immediately unusable.
    pub(crate) fn while_live<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
        let _guard = self
            .state
            .store_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.is_live().then(operation)
    }

    fn revoke(&self) {
        let _guard = self
            .state
            .store_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.state.live.store(false, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn revoke_for_test(&self) {
        self.revoke();
    }
}

/// One immutable quote version captured from the live cache. All ordinary
/// price updates in one uninterrupted live generation share the token;
/// `stale_source` or halt permanently revokes it. A later price starts a new
/// generation, so it cannot accidentally resurrect an in-flight old snapshot.
#[derive(Clone)]
pub struct QuoteSnapshot {
    quote: Quote,
    generation: QuoteGeneration,
}

impl QuoteSnapshot {
    pub fn is_live(&self) -> bool {
        self.generation.is_live()
    }

    pub(crate) fn generation(&self) -> &QuoteGeneration {
        &self.generation
    }

    #[cfg(test)]
    pub(crate) fn always_live(quote: Quote) -> Self {
        Self {
            quote,
            generation: QuoteGeneration::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn in_same_generation(&self, quote: Quote) -> Self {
        Self {
            quote,
            generation: self.generation.clone(),
        }
    }
}

impl Deref for QuoteSnapshot {
    type Target = Quote;

    fn deref(&self) -> &Self::Target {
        &self.quote
    }
}

type QuoteCache = Arc<RwLock<HashMap<(u64, Symbol), CachedQuote>>>;

/// Remove and revoke every cached quote for `symbols`, on every chain.
fn evict(cache: &mut HashMap<(u64, Symbol), CachedQuote>, symbols: &HashSet<&str>) -> usize {
    let before = cache.len();
    cache.retain(|(_, symbol), entry| {
        let keep = !symbols.contains(symbol.as_str());
        if !keep {
            entry.revoke();
        }
        keep
    });
    before - cache.len()
}

fn insert_quote(cache: &mut HashMap<(u64, Symbol), CachedQuote>, quote: Quote) {
    let key = (quote.chain_id, quote.asset.clone());
    let generation = cache
        .get(&key)
        .map(|previous| previous.generation.clone())
        .unwrap_or_else(QuoteGeneration::new);
    cache.insert(key, CachedQuote { quote, generation });
}

/// Background subscriber. Spawns one task that connects, subscribes,
/// reads price frames, and stashes the latest per-chain-and-asset `Quote`
/// in a shared `RwLock<HashMap>`. Auto-reconnects with exponential backoff.
#[derive(Clone)]
pub struct LiveClient {
    cache: QuoteCache,
    /// Chain this deployment serves; every read below is keyed by it.
    chain_id: u64,
}

impl LiveClient {
    pub fn spawn(cfg: LiveClientConfig) -> Self {
        let cache = Arc::new(RwLock::new(HashMap::new()));
        let chain_id = cfg.chain_id;
        let task_cache = cache.clone();
        tokio::spawn(async move { run_loop(cfg, task_cache).await });
        Self { cache, chain_id }
    }

    /// Drop and revoke every cached quote for `symbols`, on every chain.
    /// A request that already snapshotted one of them fails its final
    /// liveness check instead of signing it.
    ///
    /// Change the subscribed list first: a frame is checked against that
    /// list under the same write lock this takes, so once the list no
    /// longer has a symbol, no frame for it lands after this returns.
    pub async fn forget(&self, symbols: &[String]) -> usize {
        if symbols.is_empty() {
            return 0;
        }
        let symbols: HashSet<&str> = symbols.iter().map(String::as_str).collect();
        evict(&mut *self.cache.write().await, &symbols)
    }

    /// Test-only constructor that builds a `LiveClient` with a
    /// pre-populated cache and no background task. The integration
    /// tests seed deterministic `Quote`s here instead of standing up
    /// a real pricing WS server. Each quote is seeded under its own
    /// `chain_id`, exactly as the WS ingest path would.
    pub async fn with_seeded(quotes: Vec<Quote>, chain_id: u64) -> Self {
        let mut map = HashMap::with_capacity(quotes.len());
        for q in quotes {
            insert_quote(&mut map, q);
        }
        Self {
            cache: Arc::new(RwLock::new(map)),
            chain_id,
        }
    }

    /// Test helper: replace the cached quote for one asset, as a new WS
    /// frame would. Lets integration tests advance the price feed
    /// between requests without a live pricing server.
    pub async fn seed(&self, quote: Quote) {
        let mut guard = self.cache.write().await;
        insert_quote(&mut guard, quote);
    }

    pub async fn latest(&self, symbol: &str) -> Option<Quote> {
        self.cache
            .read()
            .await
            .get(&(self.chain_id, symbol.to_string()))
            .map(|entry| entry.quote.clone())
    }

    /// Snapshot multiple symbols under a single read lock so every
    /// element of a batch HTTP response is built from a coherent view
    /// of the WS cache. Mirrors `cache::QuoteCache::snapshot_many` from
    /// the pre-pricing-client world. Symbols missing from the cache — or
    /// cached only for another chain — are simply absent in the returned map.
    pub async fn snapshot_many(&self, symbols: &[&str]) -> HashMap<String, QuoteSnapshot> {
        let guard = self.cache.read().await;
        let mut out = HashMap::with_capacity(symbols.len());
        for sym in symbols {
            if let Some(entry) = guard.get(&(self.chain_id, (*sym).to_string())) {
                out.insert((*sym).to_string(), entry.snapshot());
            }
        }
        out
    }

    /// Newest `source_ts_unix_ms` across this chain's cached quotes.
    /// `None` if none have arrived yet. Used by the
    /// `oracle_cache_freshness_seconds` gauge: dashboard wants seconds
    /// since the most-recently-refreshed quote, so the caller does
    /// `now_ms - newest_source_ts` and divides by 1000. Other chains are
    /// excluded — a fresh frame we would never serve must not mask a
    /// frozen feed on the chain we do.
    pub async fn newest_source_ts_ms(&self) -> Option<i64> {
        self.cache
            .read()
            .await
            .iter()
            .filter(|((chain, _), _)| *chain == self.chain_id)
            .map(|(_, entry)| entry.quote.source_ts_unix_ms)
            .max()
    }

    /// Returns the set of subscribed symbols not yet seen on the wire for
    /// this chain. Used by /status so an operator can spot a half-warm
    /// cache without parsing logs. A symbol cached only for another chain
    /// counts as missing — we cannot serve it.
    pub async fn missing(&self, symbols: &[String]) -> Vec<String> {
        let guard = self.cache.read().await;
        symbols
            .iter()
            .filter(|s| !guard.contains_key(&(self.chain_id, (*s).clone())))
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub(crate) async fn apply_test_frame(&self, frame: ServerFrame) {
        apply_server_frame(&self.cache, None, frame).await;
    }
}

async fn run_loop(cfg: LiveClientConfig, cache: QuoteCache) {
    let mut backoff = cfg.initial_backoff;
    let mut assets = cfg.assets.clone();
    loop {
        // Pricing rejects empty Subscribe frames. Stay disconnected until
        // there is something to request, including after the last removal.
        while assets.borrow().is_empty() {
            if assets.changed().await.is_err() {
                return;
            }
        }
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

/// Pure decoder for an inbound `ServerFrame`. Exposed for fuzzing
/// (RAI-363): the on-wire loop uses this and discards `Err` results,
/// so any input that panics here is a real bug. Property tests at
/// the bottom of this file exercise it against arbitrary byte strings.
pub fn decode_server_frame(bytes: &[u8]) -> Result<ServerFrame, ClientError> {
    ciborium::from_reader(bytes).map_err(|e| ClientError::Cbor(e.to_string()))
}

/// Derive the Cloud Run audience (the service's base URL) from a WS URL:
/// `wss://host/ws` -> `https://host`. A Cloud Run ID token's audience must
/// exactly match the invoked service's URL (scheme + host, no path).
fn service_audience(ws_url: &str) -> String {
    let http = ws_url
        .strip_prefix("wss://")
        .map(|r| format!("https://{r}"))
        .or_else(|| ws_url.strip_prefix("ws://").map(|r| format!("http://{r}")))
        .unwrap_or_else(|| ws_url.to_string());
    match http.find("://") {
        Some(i) => {
            let rest = &http[i + 3..];
            let host_len = rest.find('/').unwrap_or(rest.len());
            format!("{}{}", &http[..i + 3], &rest[..host_len])
        }
        None => http,
    }
}

/// Mint a Google-signed ID token for the runtime service account from the
/// GCE/Cloud Run metadata server, scoped to `audience`. No stored secret: the
/// token is fetched on demand and lives ~1h — each reconnect gets a fresh one.
async fn fetch_id_token(audience: &str) -> Result<String, ClientError> {
    let resp = reqwest::Client::new()
        .get("http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/identity")
        .query(&[("audience", audience)])
        .header("Metadata-Flavor", "Google")
        .send()
        .await
        .map_err(|e| ClientError::IdToken(format!("metadata request: {e}")))?;
    if !resp.status().is_success() {
        return Err(ClientError::IdToken(format!(
            "metadata status {}",
            resp.status()
        )));
    }
    let token = resp
        .text()
        .await
        .map_err(|e| ClientError::IdToken(format!("metadata body: {e}")))?;
    Ok(token.trim().to_string())
}

/// A price frame without a valid execution deadline is a producer fault, not
/// a closed market, and every request for that quote is then refused at TRACE
/// on admission. Count every such frame, and WARN only when a (chain, asset)
/// goes bad or recovers, so a feed-wide regression cannot flood the log.
fn note_deadline_validity(chain_id: u64, asset: &str, valid: bool) {
    static INVALID: std::sync::OnceLock<Mutex<std::collections::HashSet<(u64, String)>>> =
        std::sync::OnceLock::new();
    if !valid {
        ::metrics::counter!(
            "oracle_upstream_failure_total",
            "kind" => "missing_execution_deadline",
        )
        .increment(1);
    }
    let mut invalid = INVALID
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (chain_id, asset.to_string());
    if valid {
        if invalid.remove(&key) {
            tracing::warn!(
                asset,
                chain_id,
                "Price frames carry a valid execution deadline again"
            );
        }
    } else if invalid.insert(key) {
        tracing::warn!(
            asset,
            chain_id,
            "Price frame has no valid execution deadline"
        );
    }
}

/// Apply one decoded inbound `ServerFrame` to the quote cache and
/// return the reply frame to send, if the frame demands one (Ping →
/// Pong). Split out of the socket loop so the cache semantics are unit
/// testable:
///
/// - `Price` stores the whole frame as one `Quote`, under the frame's own
///   `chain_id` — the rates, expiry, source_ts and NAV ratio of a cached
///   observation always come from the same frame, so a signed context can
///   never pair a rate from one frame with a NAV ratio from another, nor a
///   rate from one chain with a request on another. A frame for a symbol
///   not in `assets` (removed from the token set) is dropped.
/// - `Halt` fails closed: `halted = true` evicts the cached quote so
///   every subsequent request for the asset 503s instead of serving a
///   price the producer has disowned (the wrapped vault NAV can step on
///   a dividend deposit; the producer halts around the step). Resume
///   (`halted = false`) needs no action — the next price frame
///   repopulates the cache. Both are scoped to the frame's chain: a halt
///   elsewhere must not evict the quote this deployment serves.
async fn apply_server_frame(
    cache: &QuoteCache,
    assets: Option<&watch::Receiver<Vec<Symbol>>>,
    frame: ServerFrame,
) -> Option<ClientFrame> {
    match frame {
        ServerFrame::Price(p) => {
            note_deadline_validity(
                p.chain_id,
                &p.asset,
                p.execution_deadline_unix_ms
                    .is_some_and(|deadline| deadline > 0),
            );
            let q = Quote {
                asset: p.asset.clone(),
                chain_id: p.chain_id,
                base: p.base,
                quote: p.quote,
                rate_base_to_quote: p.rate_base_to_quote,
                rate_quote_to_base: p.rate_quote_to_base,
                expiry_unix_ms: p.expiry_unix_ms,
                execution_deadline_unix_ms: p.execution_deadline_unix_ms,
                source_ts_unix_ms: p.source_ts_unix_ms,
                nav_ratio: p.nav_ratio,
                underlying_rate_base_to_quote: p.underlying_rate_base_to_quote,
                underlying_rate_quote_to_base: p.underlying_rate_quote_to_base,
            };
            let mut guard = cache.write().await;
            // Checked under the write lock `LiveClient::forget` takes: a frame
            // that passed against the old list is in the cache before
            // `forget` runs, and `forget` removes it.
            if assets.is_some_and(|assets| !assets.borrow().contains(&q.asset)) {
                tracing::debug!(asset = %q.asset, "Price frame for a symbol no longer subscribed; dropped");
                return None;
            }
            insert_quote(&mut guard, q);
            None
        }
        ServerFrame::Error(e) => {
            tracing::warn!(?e.code, asset = ?e.asset, detail = ?e.detail, "Pricing server error frame");
            ::metrics::counter!(
                "oracle_upstream_failure_total",
                "kind" => "pricing_error_frame",
            )
            .increment(1);
            if e.code == ErrorCode::StaleSource {
                if let Some(asset) = e.asset {
                    // ErrorFrame v0.7 has no chain_id, so retaining any chain's
                    // quote could keep serving the observation pricing just
                    // disowned. Fail closed across chains for now. Once the wire
                    // and producer carry chain_id, narrow this to one cache key.
                    evict(&mut *cache.write().await, &HashSet::from([asset.as_str()]));
                    tracing::warn!(%asset, "Stale pricing source; quote evicted on every chain");
                } else {
                    tracing::warn!("Stale-source frame omitted asset; no quotes evicted");
                }
            }
            None
        }
        ServerFrame::Halt(h) => {
            if h.halted {
                if let Some(entry) = cache.write().await.remove(&(h.chain_id, h.asset.clone())) {
                    entry.revoke();
                }
                tracing::warn!(asset = %h.asset, reason = ?h.reason, "Asset halted by pricing server; quote evicted");
            } else {
                tracing::info!(asset = %h.asset, "Asset halt lifted; awaiting next price frame");
            }
            None
        }
        ServerFrame::Ping(p) => Some(ClientFrame::Pong(PongFrame {
            ts_unix_ms: p.ts_unix_ms,
        })),
    }
}

async fn connect_and_run(cfg: &LiveClientConfig, cache: &QuoteCache) -> Result<(), ClientError> {
    if cfg.assets.borrow().is_empty() {
        return Ok(());
    }
    let mut req = cfg
        .ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| ClientError::WebSocket(format!("{e}")))?;
    // Cloud Run IAM commandeers the Authorization header for a Google ID token,
    // so when iam_auth is set we mint one for the runtime SA (audience = the
    // pricing service's base URL) and send that; pricing's own API-key auth is
    // disabled behind IAM. Otherwise send the app-level API key (tailnet/local).
    let bearer = if cfg.iam_auth {
        format!(
            "Bearer {}",
            fetch_id_token(&service_audience(&cfg.ws_url)).await?
        )
    } else {
        format!("Bearer {}", cfg.api_key)
    };
    req.headers_mut().insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&bearer).map_err(|e| ClientError::Header(format!("{e}")))?,
    );
    let (socket, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| ClientError::WebSocket(format!("{e}")))?;
    run_session(cfg, cache, socket).await
}

async fn send_frame<S>(socket: &mut S, frame: &ClientFrame) -> Result<(), ClientError>
where
    S: futures_util::Sink<WsMessage> + Unpin,
    S::Error: std::fmt::Display,
{
    socket
        .send(WsMessage::Binary(encode_cbor(frame)?))
        .await
        .map_err(|e| ClientError::WebSocket(format!("{e}")))
}

/// One connected session: subscribe to the current list, then read frames
/// and follow changes to the list until the socket fails.
async fn run_session<S>(
    cfg: &LiveClientConfig,
    cache: &QuoteCache,
    mut socket: S,
) -> Result<(), ClientError>
where
    S: futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + futures_util::Sink<WsMessage>
        + Unpin,
    <S as futures_util::Sink<WsMessage>>::Error: std::fmt::Display,
{
    let mut assets = cfg.assets.clone();
    let mut sent: BTreeSet<Symbol> = assets.borrow_and_update().iter().cloned().collect();
    // The watch may become empty while authentication or the handshake
    // awaits. Return to run_loop's wait rather than sending an invalid frame.
    if sent.is_empty() {
        return Ok(());
    }
    send_frame(
        &mut socket,
        &ClientFrame::Subscribe(SubscribeFrame {
            consumer: cfg.consumer.clone(),
            assets: sent.iter().cloned().collect(),
        }),
    )
    .await?;
    // A fixed list (the sender is gone) never changes; stop polling it.
    let mut watching = true;

    // Bound the silence between frames. The pricing server heartbeats every
    // 15s (ServerFrame::Ping) and itself drops clients that stop ponging, so
    // a healthy wire always carries a frame at least every 15s. Without a
    // deadline, a half-open TCP path (LB idle drop, NAT timeout — the close
    // never reaches us) leaves the read blocked forever: no error, no
    // reconnect, and the price cache silently freezes. That is exactly how
    // production served 14-hour-old marks on 2026-07-20 (source_ts pinned at
    // 09:21 UTC with zero session-error log lines). Four missed heartbeats
    // means the session is dead — surface it as an error so `run_loop`
    // reconnects with backoff. Only an inbound frame moves the deadline; a
    // token-set change does not.
    let deadline = tokio::time::sleep(READ_DEADLINE);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            () = &mut deadline => {
                return Err(ClientError::WebSocket(format!(
                    "no frame for {READ_DEADLINE:?} (server heartbeats every 15s); \
                     presuming half-open connection"
                )));
            }
            changed = assets.changed(), if watching => {
                if changed.is_err() {
                    watching = false;
                    continue;
                }
                let next: BTreeSet<Symbol> = assets.borrow_and_update().iter().cloned().collect();
                let added: Vec<Symbol> = next.difference(&sent).cloned().collect();
                let removed: Vec<Symbol> = sent.difference(&next).cloned().collect();
                if !added.is_empty() {
                    send_frame(
                        &mut socket,
                        &ClientFrame::Subscribe(SubscribeFrame {
                            consumer: cfg.consumer.clone(),
                            assets: added.clone(),
                        }),
                    )
                    .await?;
                }
                if !removed.is_empty() {
                    send_frame(
                        &mut socket,
                        &ClientFrame::Unsubscribe(UnsubscribeFrame {
                            assets: removed.clone(),
                        }),
                    )
                    .await?;
                }
                if !added.is_empty() || !removed.is_empty() {
                    tracing::info!(?added, ?removed, "Pricing subscription follows the token set");
                }
                sent = next;
                if sent.is_empty() {
                    // run_loop waits disconnected until a token is added.
                    return Ok(());
                }
            }
            msg = socket.next() => {
                let Some(msg) = msg else { break };
                deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + READ_DEADLINE);
                match msg {
                    Ok(WsMessage::Binary(b)) => {
                        let frame = match decode_server_frame(&b[..]) {
                            Ok(f) => f,
                            Err(e) => {
                                tracing::warn!(error = %e, "Bad pricing WS frame; ignoring");
                                continue;
                            }
                        };
                        if let Some(reply) = apply_server_frame(cache, Some(&assets), frame).await {
                            let _ = send_frame(&mut socket, &reply).await;
                        }
                    }
                    Ok(WsMessage::Ping(payload)) => {
                        let _ = socket.send(WsMessage::Pong(payload)).await;
                    }
                    Ok(_) => {}
                    Err(e) => return Err(ClientError::WebSocket(format!("{e}"))),
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use st0x_pricing_types::{
        ErrorFrame, HaltFrame, PriceFrame, Venue, WireAddress, WireFloat, WireU256,
    };

    /// Base (8453) is what these tests treat as the configured chain.
    const CONFIGURED: u64 = 8453;

    fn price_frame(asset: &str, nav_ratio: WireU256) -> ServerFrame {
        price_frame_on(asset, CONFIGURED, nav_ratio)
    }

    fn price_frame_on(asset: &str, chain_id: u64, nav_ratio: WireU256) -> ServerFrame {
        ServerFrame::Price(PriceFrame {
            asset: asset.to_string(),
            venue: Venue::Raindex,
            chain_id,
            base: WireAddress::from_bytes([0x11; 20]),
            quote: WireAddress::from_bytes([0x22; 20]),
            rate_base_to_quote: WireFloat::from_bytes([0x42; 32]),
            rate_quote_to_base: WireFloat::from_bytes([0x43; 32]),
            expiry_unix_ms: 1_715_000_030_000,
            execution_deadline_unix_ms: Some(1_715_000_060_000),
            model_version: "0.1.0".into(),
            source_ts_unix_ms: 1_714_999_970_000,
            nav_ratio,
            underlying_rate_base_to_quote: WireFloat::from_bytes([0x44; 32]),
            underlying_rate_quote_to_base: WireFloat::from_bytes([0x45; 32]),
        })
    }

    fn halt_frame(asset: &str, halted: bool) -> ServerFrame {
        halt_frame_on(asset, CONFIGURED, halted)
    }

    fn halt_frame_on(asset: &str, chain_id: u64, halted: bool) -> ServerFrame {
        ServerFrame::Halt(HaltFrame {
            asset: asset.to_string(),
            chain_id,
            base: WireAddress::from_bytes([0x11; 20]),
            quote: WireAddress::from_bytes([0x22; 20]),
            halted,
            reason: None,
        })
    }

    fn error_frame(code: ErrorCode, asset: Option<&str>) -> ServerFrame {
        ServerFrame::Error(ErrorFrame {
            code,
            asset: asset.map(str::to_string),
            last_ok_unix_ms: None,
            detail: None,
        })
    }

    /// The cached `Quote` must carry the frame's NAV ratio and the two
    /// directional underlying rates bit-for-bit alongside its vault rates:
    /// one frame in, one coherent observation out. `/context/v7` signs the
    /// underlying rate off the same cached quote, so it has to survive the
    /// frame→quote copy unchanged.
    #[tokio::test]
    async fn price_frame_stores_nav_ratio_with_the_same_observation() {
        let cache = Arc::new(RwLock::new(HashMap::new()));
        let mut nav = [0u8; 32];
        let mut v: u8 = 5;
        for b in &mut nav {
            *b = v;
            v = v.wrapping_add(29);
        }

        let reply =
            apply_server_frame(&cache, None, price_frame("COIN", WireU256::from_bytes(nav))).await;
        assert!(reply.is_none());

        let q = cached(&cache, CONFIGURED, "COIN").await.unwrap();
        assert_eq!(q.nav_ratio.0, nav, "NAV ratio must be bit-for-bit");
        assert_eq!(q.rate_quote_to_base, WireFloat::from_bytes([0x43; 32]));
        assert_eq!(
            q.underlying_rate_base_to_quote,
            WireFloat::from_bytes([0x44; 32]),
            "underlying base->quote rate must carry through bit-for-bit"
        );
        assert_eq!(
            q.underlying_rate_quote_to_base,
            WireFloat::from_bytes([0x45; 32]),
            "underlying quote->base rate must carry through bit-for-bit"
        );
        assert_eq!(q.execution_deadline_unix_ms, Some(1_715_000_060_000));
    }

    /// A halt fails closed: the cached quote is evicted immediately, so
    /// requests for the asset 503 instead of serving a price the
    /// producer has disowned. A resume frame does NOT resurrect the old
    /// quote — only the next price frame repopulates the cache.
    #[tokio::test]
    async fn halt_evicts_cached_quote_and_resume_does_not_restore_it() {
        let cache = Arc::new(RwLock::new(HashMap::new()));
        apply_server_frame(&cache, None, price_frame("COIN", WireU256::ZERO)).await;
        apply_server_frame(&cache, None, price_frame("TSLA", WireU256::ZERO)).await;

        apply_server_frame(&cache, None, halt_frame("COIN", true)).await;
        assert!(
            cached(&cache, CONFIGURED, "COIN").await.is_none(),
            "halted asset must be evicted"
        );
        assert!(
            cached(&cache, CONFIGURED, "TSLA").await.is_some(),
            "halt must only evict the named asset"
        );

        apply_server_frame(&cache, None, halt_frame("COIN", false)).await;
        assert!(
            cached(&cache, CONFIGURED, "COIN").await.is_none(),
            "resume must not resurrect the pre-halt quote"
        );

        apply_server_frame(&cache, None, price_frame("COIN", WireU256::ZERO)).await;
        assert!(
            cached(&cache, CONFIGURED, "COIN").await.is_some(),
            "next price frame repopulates the cache"
        );
    }

    async fn cached(cache: &QuoteCache, chain_id: u64, asset: &str) -> Option<Quote> {
        cache
            .read()
            .await
            .get(&(chain_id, asset.to_string()))
            .map(|entry| entry.quote.clone())
    }

    /// st0x.pricing publishes one frame per (chain, symbol). Two frames for
    /// the same symbol on different chains are distinct observations and
    /// must both survive ingest — keyed by symbol alone they were
    /// last-write-wins, and the loser vanished without a trace.
    #[tokio::test]
    async fn frames_for_one_symbol_on_two_chains_do_not_clobber_each_other() {
        let cache: QuoteCache = Arc::new(RwLock::new(HashMap::new()));
        let (base_nav, other_nav) = ([0x01u8; 32], [0x02u8; 32]);

        apply_server_frame(
            &cache,
            None,
            price_frame_on("COIN", CONFIGURED, WireU256::from_bytes(base_nav)),
        )
        .await;
        apply_server_frame(
            &cache,
            None,
            price_frame_on("COIN", 1, WireU256::from_bytes(other_nav)),
        )
        .await;

        assert_eq!(cache.read().await.len(), 2, "one entry per (chain, symbol)");
        assert_eq!(
            cached(&cache, CONFIGURED, "COIN")
                .await
                .unwrap()
                .nav_ratio
                .0,
            base_nav
        );
        assert_eq!(
            cached(&cache, 1, "COIN").await.unwrap().nav_ratio.0,
            other_nav
        );
    }

    /// Reads serve the configured chain, never whichever frame landed last.
    /// A quote cached only for another chain reads as absent everywhere:
    /// signing a context off it would bind another chain's rate, and no
    /// staleness or expiry check downstream would notice.
    #[tokio::test]
    async fn reads_serve_the_configured_chain_not_the_latest_frame() {
        let cache: QuoteCache = Arc::new(RwLock::new(HashMap::new()));
        let (base_nav, other_nav) = ([0x01u8; 32], [0x02u8; 32]);

        apply_server_frame(
            &cache,
            None,
            price_frame_on("COIN", CONFIGURED, WireU256::from_bytes(base_nav)),
        )
        .await;
        // Arrives last and would win under a symbol-only key.
        apply_server_frame(
            &cache,
            None,
            price_frame_on("COIN", 1, WireU256::from_bytes(other_nav)),
        )
        .await;
        // Only ever seen on another chain.
        apply_server_frame(&cache, None, price_frame_on("TSLA", 1, WireU256::ZERO)).await;

        let client = LiveClient {
            cache,
            chain_id: CONFIGURED,
        };

        assert_eq!(client.latest("COIN").await.unwrap().nav_ratio.0, base_nav);
        let snapshot = client.snapshot_many(&["COIN", "TSLA"]).await;
        assert_eq!(snapshot.get("COIN").unwrap().nav_ratio.0, base_nav);
        assert!(
            !snapshot.contains_key("TSLA"),
            "another chain's quote is never served"
        );
        assert!(client.latest("TSLA").await.is_none());
        assert_eq!(
            client
                .missing(&["COIN".to_string(), "TSLA".to_string()])
                .await,
            vec!["TSLA".to_string()],
            "a symbol cached only for another chain is missing"
        );
    }

    /// Halts are chain-scoped too: the producer halts an asset on the chain
    /// it repriced, and that must not fail-closed a deployment serving the
    /// same symbol elsewhere.
    #[tokio::test]
    async fn halt_on_another_chain_does_not_evict_our_quote() {
        let cache: QuoteCache = Arc::new(RwLock::new(HashMap::new()));
        apply_server_frame(
            &cache,
            None,
            price_frame_on("COIN", CONFIGURED, WireU256::ZERO),
        )
        .await;
        apply_server_frame(&cache, None, price_frame_on("COIN", 1, WireU256::ZERO)).await;

        apply_server_frame(&cache, None, halt_frame_on("COIN", 1, true)).await;

        assert!(
            cached(&cache, 1, "COIN").await.is_none(),
            "halt evicts the quote on its own chain"
        );
        assert!(
            cached(&cache, CONFIGURED, "COIN").await.is_some(),
            "halt must not reach across chains"
        );
    }

    #[tokio::test]
    async fn stale_source_evicts_named_symbol_on_every_chain_and_price_repopulates() {
        let cache: QuoteCache = Arc::new(RwLock::new(HashMap::new()));
        apply_server_frame(
            &cache,
            None,
            price_frame_on("COIN", CONFIGURED, WireU256::ZERO),
        )
        .await;
        apply_server_frame(&cache, None, price_frame_on("COIN", 1, WireU256::ZERO)).await;
        apply_server_frame(
            &cache,
            None,
            price_frame_on("TSLA", CONFIGURED, WireU256::ZERO),
        )
        .await;
        let client = LiveClient {
            cache: Arc::clone(&cache),
            chain_id: CONFIGURED,
        };
        let in_flight = client
            .snapshot_many(&["COIN"])
            .await
            .remove("COIN")
            .unwrap();

        apply_server_frame(
            &cache,
            None,
            error_frame(ErrorCode::StaleSource, Some("COIN")),
        )
        .await;

        assert!(cached(&cache, CONFIGURED, "COIN").await.is_none());
        assert!(
            cached(&cache, 1, "COIN").await.is_none(),
            "fail-closed eviction includes a valid other-chain quote until ErrorFrame carries chain_id"
        );
        assert!(cached(&cache, CONFIGURED, "TSLA").await.is_some());
        assert!(!in_flight.is_live(), "eviction revokes owned snapshots");

        apply_server_frame(
            &cache,
            None,
            price_frame_on("COIN", CONFIGURED, WireU256::ZERO),
        )
        .await;
        assert!(cached(&cache, CONFIGURED, "COIN").await.is_some());
        assert!(
            !in_flight.is_live(),
            "a new live generation must not resurrect an older snapshot"
        );
    }

    #[tokio::test]
    async fn stale_source_without_asset_and_other_errors_do_not_evict() {
        for code in [
            ErrorCode::StaleSource,
            ErrorCode::UnknownAsset,
            ErrorCode::ModelError,
            ErrorCode::Internal,
        ] {
            let cache: QuoteCache = Arc::new(RwLock::new(HashMap::new()));
            apply_server_frame(&cache, None, price_frame("COIN", WireU256::ZERO)).await;
            let asset = if code == ErrorCode::StaleSource {
                None
            } else {
                Some("COIN")
            };
            apply_server_frame(&cache, None, error_frame(code, asset)).await;
            assert!(
                cached(&cache, CONFIGURED, "COIN").await.is_some(),
                "{code:?} must not evict this quote"
            );
        }
    }

    use tokio_tungstenite::tungstenite::protocol::Role;
    use tokio_tungstenite::WebSocketStream;

    type Duplex = WebSocketStream<tokio::io::DuplexStream>;

    /// A connected client and server socket over an in-memory pipe.
    async fn socket_pair() -> (Duplex, Duplex) {
        let (a, b) = tokio::io::duplex(1 << 16);
        (
            WebSocketStream::from_raw_socket(a, Role::Client, None).await,
            WebSocketStream::from_raw_socket(b, Role::Server, None).await,
        )
    }

    fn session_config(assets: watch::Receiver<Vec<Symbol>>) -> LiveClientConfig {
        LiveClientConfig::new("ws://unused", "k", "oracle", vec![], CONFIGURED).with_assets(assets)
    }

    async fn next_client_frame(server: &mut Duplex) -> ClientFrame {
        loop {
            match server.next().await.expect("socket open").expect("frame") {
                WsMessage::Binary(b) => return ciborium::from_reader(&b[..]).unwrap(),
                _ => continue,
            }
        }
    }

    fn symbols(list: &[&str]) -> Vec<Symbol> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[tokio::test]
    async fn empty_assets_wait_to_connect_and_reconnect_until_a_token_is_added() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (tx, rx) = watch::channel(Vec::new());
        let cfg = LiveClientConfig::new(
            format!("ws://{}", listener.local_addr().unwrap()),
            "k",
            "oracle",
            vec![],
            CONFIGURED,
        )
        .with_assets(rx);
        let cache: QuoteCache = Arc::new(RwLock::new(HashMap::new()));
        let client = tokio::spawn(run_loop(cfg, cache));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "empty startup must not connect"
        );
        tx.send(symbols(&["A"])).unwrap();
        let (tcp, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut server = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let message = server.next().await.unwrap().unwrap().into_data();
        let frame: ClientFrame = ciborium::from_reader(&message[..]).unwrap();
        assert!(matches!(frame, ClientFrame::Subscribe(f) if f.assets == symbols(&["A"])));
        tx.send(Vec::new()).unwrap();
        let message = server.next().await.unwrap().unwrap().into_data();
        let frame: ClientFrame = ciborium::from_reader(&message[..]).unwrap();
        assert!(matches!(frame, ClientFrame::Unsubscribe(f) if f.assets == symbols(&["A"])));
        assert!(
            !matches!(
                tokio::time::timeout(Duration::from_secs(1), server.next())
                    .await
                    .unwrap(),
                Some(Ok(_))
            ),
            "the client disconnects after its last Unsubscribe"
        );
        drop(server);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "removing the final token must suspend reconnection"
        );
        tx.send(symbols(&["B"])).unwrap();
        let (tcp, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut server = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let message = server.next().await.unwrap().unwrap().into_data();
        let frame: ClientFrame = ciborium::from_reader(&message[..]).unwrap();
        assert!(matches!(frame, ClientFrame::Subscribe(f) if f.assets == symbols(&["B"])));
        tx.send(Vec::new()).unwrap();
        let message = server.next().await.unwrap().unwrap().into_data();
        let frame: ClientFrame = ciborium::from_reader(&message[..]).unwrap();
        assert!(matches!(frame, ClientFrame::Unsubscribe(f) if f.assets == symbols(&["B"])));
        drop(tx);
        assert!(
            !matches!(
                tokio::time::timeout(Duration::from_secs(1), server.next())
                    .await
                    .unwrap(),
                Some(Ok(_))
            ),
            "the client disconnects after its last Unsubscribe"
        );
        drop(server);
        tokio::time::timeout(Duration::from_secs(1), client)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_set_emptied_during_handshake_sends_no_empty_subscribe() {
        let (tx, rx) = watch::channel(symbols(&["A"]));
        let cfg = session_config(rx);
        let (client, mut server) = socket_pair().await;
        tx.send(Vec::new()).unwrap();
        let cache: QuoteCache = Arc::new(RwLock::new(HashMap::new()));
        run_session(&cfg, &cache, client).await.unwrap();
        assert!(
            server.next().await.unwrap().is_err(),
            "the socket closes without a Subscribe"
        );
    }

    /// A token set change reaches pricing as the difference, on the same
    /// session: new symbols are subscribed, removed ones unsubscribed.
    #[tokio::test]
    async fn a_token_change_sends_subscribe_and_unsubscribe_deltas() {
        let (tx, rx) = watch::channel(symbols(&["A", "B"]));
        let (client, mut server) = socket_pair().await;
        let cache: QuoteCache = Arc::new(RwLock::new(HashMap::new()));
        let session = tokio::spawn({
            let cache = cache.clone();
            async move { run_session(&session_config(rx), &cache, client).await }
        });

        match next_client_frame(&mut server).await {
            ClientFrame::Subscribe(f) => assert_eq!(f.assets, symbols(&["A", "B"])),
            other => panic!("{other:?}"),
        }

        tx.send(symbols(&["B", "C"])).unwrap();
        match next_client_frame(&mut server).await {
            ClientFrame::Subscribe(f) => assert_eq!(f.assets, symbols(&["C"])),
            other => panic!("{other:?}"),
        }
        match next_client_frame(&mut server).await {
            ClientFrame::Unsubscribe(f) => assert_eq!(f.assets, symbols(&["A"])),
            other => panic!("{other:?}"),
        }
        assert!(!session.is_finished(), "the session stays up");
        session.abort();
    }

    /// Only an inbound frame moves the read deadline. Token changes keep
    /// the session busy sending, but a silent pricing link still fails at
    /// READ_DEADLINE and the client reconnects.
    #[tokio::test(start_paused = true)]
    async fn the_read_deadline_is_not_extended_by_token_changes() {
        let (tx, rx) = watch::channel(symbols(&["A"]));
        let (client, _server) = socket_pair().await;
        let cache: QuoteCache = Arc::new(RwLock::new(HashMap::new()));
        let started = tokio::time::Instant::now();
        let session = tokio::spawn({
            let cache = cache.clone();
            async move { run_session(&session_config(rx), &cache, client).await }
        });
        for (at, list) in [(20, ["A", "B"]), (50, ["B", "C"])] {
            tokio::time::sleep_until(started + Duration::from_secs(at)).await;
            tx.send(symbols(&list)).unwrap();
        }
        let err = session.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("half-open"), "{err}");
        assert_eq!(started.elapsed(), READ_DEADLINE);
    }

    /// Once a symbol leaves the subscribed list, a frame for it that is
    /// still in flight does not repopulate the cache.
    #[tokio::test]
    async fn a_frame_for_a_forgotten_symbol_is_dropped() {
        let (tx, rx) = watch::channel(symbols(&["COIN", "TSLA"]));
        let cache: QuoteCache = Arc::new(RwLock::new(HashMap::new()));
        apply_server_frame(&cache, Some(&rx), price_frame("COIN", WireU256::ZERO)).await;
        assert!(cached(&cache, CONFIGURED, "COIN").await.is_some());

        tx.send(symbols(&["TSLA"])).unwrap();
        let client = LiveClient {
            cache: Arc::clone(&cache),
            chain_id: CONFIGURED,
        };
        let in_flight = client
            .snapshot_many(&["COIN"])
            .await
            .remove("COIN")
            .unwrap();
        assert_eq!(client.forget(&["COIN".to_string()]).await, 1);
        assert!(!in_flight.is_live(), "forget revokes owned snapshots");

        apply_server_frame(&cache, Some(&rx), price_frame("COIN", WireU256::ZERO)).await;
        apply_server_frame(&cache, Some(&rx), price_frame("TSLA", WireU256::ZERO)).await;
        assert!(cached(&cache, CONFIGURED, "COIN").await.is_none());
        assert!(cached(&cache, CONFIGURED, "TSLA").await.is_some());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// The WS receive loop runs `decode_server_frame` on every
        /// inbound binary frame and silently drops the result on
        /// `Err`. Any panic here would crash the subscriber task and
        /// stall the pricing cache until the next reconnect — bad
        /// enough that we exercise it against arbitrary bytes.
        #[test]
        fn wire_decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = decode_server_frame(&bytes);
        }
    }
}
