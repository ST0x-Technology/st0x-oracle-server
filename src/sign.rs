use alloy::primitives::{Address, Bytes, FixedBytes, Keccak256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer as AlloySigner;
use alloy_signer_gcp::{GcpKeyRingRef, GcpSigner, KeySpecifier};
use gcloud_sdk::google::cloud::kms::v1::key_management_service_client::KeyManagementServiceClient;
use gcloud_sdk::GoogleApi;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
// EIP-191 signing for Rain signed context

/// Upper bound on a single sign attempt. Only meaningful for the KMS
/// backend, where signing is a network RPC.
const SIGN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a request waits on a signature another request is already
/// producing: the full attempt-plus-retry budget of that sign, with slack.
/// Waiters never sign themselves, so this is also the worst case a caller
/// can spend inside `sign_context`.
const WAIT_TIMEOUT: Duration = Duration::from_secs(2 * 5 + 1);

/// Idle time after which a cached signature is dropped. Validity is not the
/// question (a signature over given bytes is valid for as long as the quote
/// it carries, and the key IS the bytes); the TTL only bounds memory. An
/// entry nobody has asked for in two minutes belongs to a price frame that
/// was superseded ~24 frames ago. Measured from the last hit, so a frozen
/// price that takers keep polling is never re-signed.
const SIGNATURE_CACHE_IDLE_TTL: Duration = Duration::from_secs(120);

/// Hard cap on cached entries, a backstop for the sweep below (78 live
/// inputs per frame today, well under 2,000 live entries in a two-minute
/// window). Past the cap every finished entry is dropped; in-flight signs
/// are kept so no KMS call is ever orphaned.
const SIGNATURE_CACHE_MAX_ENTRIES: usize = 16_384;

/// Expired entries are swept on every N-th insert (a full scan of a
/// ~2,000-entry map every ~15 seconds at today's miss rate), so the map
/// tracks the live set instead of growing to the cap.
const SWEEP_EVERY_INSERTS: u64 = 256;

/// Outcome of one sign, broadcast to every request waiting on it.
#[derive(Clone, Debug)]
enum SignState {
    Pending,
    Done(Bytes),
    Failed(String),
}

/// One cache slot. The signature is produced by a detached task that owns
/// the `watch::Sender`; every request for these bytes, the one that
/// started the task included, just waits on the receiver. So a client that
/// disconnects mid-sign cannot abort the KMS call it started, and a failed
/// sign fails all current waiters at once instead of letting each retry
/// in turn.
struct CacheEntry {
    /// Millis since `SignatureCache::epoch` at the last hit or insert.
    last_used: AtomicU64,
    state: watch::Receiver<SignState>,
}

impl CacheEntry {
    fn is_pending(&self) -> bool {
        matches!(*self.state.borrow(), SignState::Pending)
    }
}

struct CacheMap {
    entries: HashMap<FixedBytes<32>, Arc<CacheEntry>>,
    inserts_since_sweep: u64,
}

/// Content-addressed signature cache: `keccak256(abi.encodePacked(context))`
/// to signature. The signed slots (price, publish_time, expiry) derive from
/// the pricing frame and the pair, and the session slots from the
/// market-hours cache, which changes only at session boundaries. Two
/// requests for one pair inside one price frame therefore sign
/// byte-identical data, and the second reuses the first signature.
/// Consumers see no difference: same bytes, a valid signature.
///
/// Caveat: with an EMPTY market-hours cache (initial calendar fetch failed;
/// retried hourly by `main.rs`) the session window degenerates to
/// `start = end = now`, slots 4 and 5 change every second, and this cache
/// stops helping until the calendar loads. It stays correct, just useless.
struct SignatureCache {
    map: Mutex<CacheMap>,
    epoch: Instant,
    idle_ttl: Duration,
    max_entries: usize,
    /// Signs started (one per distinct input actually sent to KMS).
    misses: AtomicU64,
    /// Requests served from an existing slot, finished or in flight.
    hits: AtomicU64,
}

enum Lookup {
    /// An entry exists (finished or in flight); wait on it.
    Existing(Arc<CacheEntry>),
    /// No usable entry; the caller must start the sign that fills this one.
    Fresh(Arc<CacheEntry>, watch::Sender<SignState>),
}

impl SignatureCache {
    fn new(idle_ttl: Duration, max_entries: usize) -> Self {
        Self {
            map: Mutex::new(CacheMap {
                entries: HashMap::new(),
                inserts_since_sweep: 0,
            }),
            epoch: Instant::now(),
            idle_ttl,
            max_entries,
            misses: AtomicU64::new(0),
            hits: AtomicU64::new(0),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn is_live(&self, entry: &CacheEntry, now_ms: u64) -> bool {
        entry.is_pending()
            || now_ms.saturating_sub(entry.last_used.load(Ordering::Relaxed))
                < u64::try_from(self.idle_ttl.as_millis()).unwrap_or(u64::MAX)
    }

    /// Find or create the slot for `hash`. Synchronous: the map lock never
    /// spans an await, signing happens outside it.
    fn lookup(&self, hash: FixedBytes<32>) -> Lookup {
        let mut guard = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let now_ms = self.now_ms();

        if let Some(entry) = guard.entries.get(&hash) {
            if self.is_live(entry, now_ms) {
                entry.last_used.store(now_ms, Ordering::Relaxed);
                return Lookup::Existing(Arc::clone(entry));
            }
            guard.entries.remove(&hash);
        }

        guard.inserts_since_sweep += 1;
        if guard.inserts_since_sweep >= SWEEP_EVERY_INSERTS
            || guard.entries.len() >= self.max_entries
        {
            guard.inserts_since_sweep = 0;
            guard.entries.retain(|_, e| self.is_live(e, now_ms));
            if guard.entries.len() >= self.max_entries {
                // Still over: drop everything finished, keep in-flight signs.
                guard.entries.retain(|_, e| e.is_pending());
            }
        }

        let (tx, rx) = watch::channel(SignState::Pending);
        let entry = Arc::new(CacheEntry {
            last_used: AtomicU64::new(now_ms),
            state: rx,
        });
        guard.entries.insert(hash, Arc::clone(&entry));
        ::metrics::gauge!("oracle_signature_cache_entries").set(guard.entries.len() as f64);
        Lookup::Fresh(entry, tx)
    }

    /// Drop `entry` from the map if it is still the one registered for
    /// `hash`, so the next request starts a fresh sign after a failure.
    fn evict(&self, hash: FixedBytes<32>, entry: &Arc<CacheEntry>) {
        let mut guard = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if guard
            .entries
            .get(&hash)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            guard.entries.remove(&hash);
            ::metrics::gauge!("oracle_signature_cache_entries").set(guard.entries.len() as f64);
        }
    }
}

/// Hit/miss counters, read by the unit tests.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureCacheStats {
    pub hits: u64,
    pub misses: u64,
}

/// Test-only knobs on the detached sign task: an artificial delay so
/// concurrent requests really overlap, and an injected failure.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
struct TestHook {
    delay: Duration,
    fail: bool,
}

/// EIP-191 signer for Rain signed context.
///
/// Two backends behind the same interface:
/// - a local hex private key (tests / local dev), or
/// - GCP Cloud KMS (production): the key is non-extractable and never enters
///   this process; a cache miss is one KMS `AsymmetricSign` call
///   authenticated via Application Default Credentials (on GCE, the VM's
///   attached service account, so no credential material on the box).
///
/// Signatures are cached by content (see [`SignatureCache`]), so repeated
/// requests for the same bytes cost one KMS operation.
pub struct Signer {
    inner: Arc<dyn AlloySigner + Send + Sync>,
    cache: Arc<SignatureCache>,
    #[cfg(test)]
    test_hook: Option<TestHook>,
}

/// Components of a KMS key version resource name:
/// `projects/{p}/locations/{l}/keyRings/{r}/cryptoKeys/{k}/cryptoKeyVersions/{v}`.
#[derive(Debug, PartialEq, Eq)]
struct KmsKeyName {
    project: String,
    location: String,
    keyring: String,
    key: String,
    version: u64,
}

impl KmsKeyName {
    fn parse(resource_name: &str) -> anyhow::Result<Self> {
        let parts: Vec<&str> = resource_name.split('/').collect();
        match parts.as_slice() {
            ["projects", p, "locations", l, "keyRings", r, "cryptoKeys", k, "cryptoKeyVersions", v] => {
                Ok(Self {
                    project: p.to_string(),
                    location: l.to_string(),
                    keyring: r.to_string(),
                    key: k.to_string(),
                    version: v.parse().map_err(|_| {
                        anyhow::anyhow!("cryptoKeyVersions segment '{v}' is not a number")
                    })?,
                })
            }
            _ => anyhow::bail!(
                "SIGNER_KMS_KEY must be a full key version resource name \
                 (projects/…/locations/…/keyRings/…/cryptoKeys/…/cryptoKeyVersions/N), got: {resource_name}"
            ),
        }
    }
}

/// One EIP-191 sign of `hash` with the KMS timeout and single retry.
///
/// The Rain orderbook contract applies toEthSignedMessageHash(hash) before
/// ecrecover, so we sign the raw hash with sign_message, which prefixes
/// "\x19Ethereum Signed Message:\n32" internally.
///
/// On the KMS backend this is a remote AsymmetricSign RPC, so it is bounded
/// by a timeout (a blackholed KMS connection must surface as an error, not
/// hang the request handler while `/` stays green) and retried once for
/// transient failures. The local backend signs in microseconds and never
/// hits either path.
async fn sign_hash_with_retry(
    inner: &(dyn AlloySigner + Send + Sync),
    hash: FixedBytes<32>,
) -> anyhow::Result<Bytes> {
    let signature =
        match tokio::time::timeout(SIGN_TIMEOUT, inner.sign_message(hash.as_slice())).await {
            Ok(Ok(sig)) => sig,
            first_failure => {
                match &first_failure {
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "sign_message failed; retrying once")
                    }
                    _ => tracing::warn!(
                        timeout_secs = SIGN_TIMEOUT.as_secs(),
                        "sign_message timed out; retrying once"
                    ),
                }
                tokio::time::timeout(SIGN_TIMEOUT, inner.sign_message(hash.as_slice()))
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "signing timed out after {}s (retry included)",
                            SIGN_TIMEOUT.as_secs()
                        )
                    })??
            }
        };
    Ok(Bytes::from(signature.as_bytes().to_vec()))
}

impl Signer {
    fn with_inner(inner: Arc<dyn AlloySigner + Send + Sync>) -> Self {
        Self {
            inner,
            cache: Arc::new(SignatureCache::new(
                SIGNATURE_CACHE_IDLE_TTL,
                SIGNATURE_CACHE_MAX_ENTRIES,
            )),
            #[cfg(test)]
            test_hook: None,
        }
    }

    /// Create a new signer from a hex private key (with or without 0x prefix).
    pub fn new(private_key: &str) -> anyhow::Result<Self> {
        let key = private_key.strip_prefix("0x").unwrap_or(private_key);
        let signer: PrivateKeySigner = key.parse()?;
        Ok(Self::with_inner(Arc::new(signer)))
    }

    /// Create a signer backed by a GCP Cloud KMS key version.
    ///
    /// `resource_name` is the full key version resource name (the Terraform
    /// stack's `signer_kms_key_version` output). Fails fast if the key is
    /// unreachable, is not secp256k1, or ADC cannot authenticate: better a
    /// loud startup error than serving unsigned/failing requests.
    pub async fn from_gcp_kms(resource_name: &str) -> anyhow::Result<Self> {
        let name = KmsKeyName::parse(resource_name)?;

        // Install a process-level rustls CryptoProvider before gcloud-sdk
        // builds its TLS client: both `ring` and `aws-lc-rs` are in the
        // dependency graph (reqwest vs gcloud-sdk/tonic), so rustls cannot
        // auto-select one and panics at first TLS use. Idempotent: the
        // result is ignored if a provider is already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let keyring = GcpKeyRingRef::new(&name.project, &name.location, &name.keyring);
        let client = GoogleApi::from_function(
            KeyManagementServiceClient::new,
            "https://cloudkms.googleapis.com",
            None,
        )
        .await?;
        let specifier = KeySpecifier::new(keyring, &name.key, name.version);
        // No chain id: EIP-191 message signing is chain-agnostic.
        let signer = GcpSigner::new(client, specifier, None).await?;

        Ok(Self::with_inner(Arc::new(signer)))
    }

    /// Override the cache bounds; a zero idle TTL disables caching.
    #[cfg(test)]
    pub fn with_cache_bounds(mut self, idle_ttl: Duration, max_entries: usize) -> Self {
        self.cache = Arc::new(SignatureCache::new(idle_ttl, max_entries));
        self
    }

    #[cfg(test)]
    fn with_test_hook(mut self, hook: TestHook) -> Self {
        self.test_hook = Some(hook);
        self
    }

    /// Signature cache hit/miss counters since startup.
    #[cfg(test)]
    pub fn cache_stats(&self) -> SignatureCacheStats {
        SignatureCacheStats {
            hits: self.cache.hits.load(Ordering::Relaxed),
            misses: self.cache.misses.load(Ordering::Relaxed),
        }
    }

    /// Get the signer's address.
    pub fn address(&self) -> Address {
        self.inner.address()
    }

    /// Sign a context array using EIP-191.
    ///
    /// The signature is over `keccak256(abi.encodePacked(context[]))`,
    /// matching `LibContext.build` in the Rain orderbook contract which uses
    /// OpenZeppelin's `SignatureChecker.isValidSignatureNow`.
    pub async fn sign_context(
        &self,
        context: &[FixedBytes<32>],
    ) -> anyhow::Result<(Bytes, Address)> {
        // abi.encodePacked(bytes32[]) is the raw concatenation, so hash the
        // slots straight through without building the packed buffer. The
        // hash is what gets signed and is therefore also the cache key:
        // same bytes, same (still valid) signature, no second KMS call.
        let mut hasher = Keccak256::new();
        for slot in context {
            hasher.update(slot);
        }
        let hash = hasher.finalize();

        let entry = match self.cache.lookup(hash) {
            Lookup::Existing(entry) => {
                self.cache.hits.fetch_add(1, Ordering::Relaxed);
                ::metrics::counter!("oracle_signature_cache_hits_total").increment(1);
                entry
            }
            Lookup::Fresh(entry, tx) => {
                self.cache.misses.fetch_add(1, Ordering::Relaxed);
                ::metrics::counter!("oracle_signature_cache_misses_total").increment(1);
                self.spawn_sign(hash, Arc::clone(&entry), tx);
                entry
            }
        };

        let mut rx = entry.state.clone();
        let settled = tokio::time::timeout(
            WAIT_TIMEOUT,
            rx.wait_for(|s| !matches!(s, SignState::Pending)),
        )
        .await;
        match settled {
            Ok(Ok(state)) => match &*state {
                SignState::Done(sig) => Ok((sig.clone(), self.address())),
                SignState::Failed(msg) => Err(anyhow::anyhow!("{msg}")),
                SignState::Pending => unreachable!("wait_for returned while pending"),
            },
            // Sender dropped without a result: the sign task panicked.
            Ok(Err(_)) => Err(anyhow::anyhow!(
                "signing task aborted before producing a result"
            )),
            Err(_) => Err(anyhow::anyhow!(
                "timed out after {}s waiting for an in-flight signature",
                WAIT_TIMEOUT.as_secs()
            )),
        }
    }

    /// Run the KMS sign for `hash` on its own task so it completes even if
    /// the request that triggered it goes away, then broadcast the result
    /// to every waiter. On failure the entry is evicted so the next request
    /// starts over instead of inheriting a dead slot.
    fn spawn_sign(
        &self,
        hash: FixedBytes<32>,
        entry: Arc<CacheEntry>,
        tx: watch::Sender<SignState>,
    ) {
        let inner = Arc::clone(&self.inner);
        let cache = Arc::clone(&self.cache);
        #[cfg(test)]
        let hook = self.test_hook;
        tokio::spawn(async move {
            #[cfg(test)]
            if let Some(h) = hook {
                tokio::time::sleep(h.delay).await;
                if h.fail {
                    cache.evict(hash, &entry);
                    let _ = tx.send(SignState::Failed("injected test failure".into()));
                    return;
                }
            }
            let outcome = sign_hash_with_retry(inner.as_ref(), hash).await;
            let state = match outcome {
                Ok(sig) => SignState::Done(sig),
                Err(e) => {
                    cache.evict(hash, &entry);
                    SignState::Failed(e.to_string())
                }
            };
            // Evict BEFORE broadcasting a failure so a waiter that retries on
            // error finds a fresh slot, never the dead one.
            let _ = tx.send(state);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;

    // Test private key. DO NOT use in production.
    const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[test]
    fn test_signer_from_key() {
        let signer = Signer::new(TEST_KEY).unwrap();
        // Hardhat account #0
        assert_eq!(
            signer.address(),
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
                .parse::<Address>()
                .unwrap()
        );
    }

    #[test]
    fn test_signer_with_0x_prefix() {
        let signer = Signer::new(&format!("0x{}", TEST_KEY)).unwrap();
        assert_eq!(
            signer.address(),
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
                .parse::<Address>()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_sign_context_deterministic() {
        // Zero idle TTL: every call signs, so this observes the local
        // signer's determinism rather than a cache hit.
        let signer = Signer::new(TEST_KEY)
            .unwrap()
            .with_cache_bounds(Duration::ZERO, 16);
        let context = vec![
            FixedBytes::<32>::from(U256::from(1000u64)),
            FixedBytes::<32>::from(U256::from(2000u64)),
        ];

        let (sig1, addr1) = signer.sign_context(&context).await.unwrap();
        let (sig2, addr2) = signer.sign_context(&context).await.unwrap();

        assert_eq!(sig1, sig2, "Same context should produce same signature");
        assert_eq!(addr1, addr2);
        assert_eq!(sig1.len(), 65, "EIP-191 signature should be 65 bytes");
        assert_eq!(
            signer.cache_stats().misses,
            2,
            "cache must be bypassed here"
        );
    }

    #[tokio::test]
    async fn test_sign_context_cache_hit_on_identical_bytes() {
        let signer = Signer::new(TEST_KEY).unwrap();
        let context = vec![
            FixedBytes::<32>::from(U256::from(1000u64)),
            FixedBytes::<32>::from(U256::from(2000u64)),
        ];

        let (sig1, _) = signer.sign_context(&context).await.unwrap();
        assert_eq!(
            signer.cache_stats(),
            SignatureCacheStats { hits: 0, misses: 1 }
        );
        let (sig2, _) = signer.sign_context(&context).await.unwrap();
        assert_eq!(
            signer.cache_stats(),
            SignatureCacheStats { hits: 1, misses: 1 }
        );
        assert_eq!(sig1, sig2);

        // Different bytes are a different key, never a false hit.
        let other = vec![FixedBytes::<32>::from(U256::from(3000u64))];
        let (sig3, _) = signer.sign_context(&other).await.unwrap();
        assert_ne!(sig1, sig3);
        assert_eq!(
            signer.cache_stats(),
            SignatureCacheStats { hits: 1, misses: 2 }
        );
    }

    #[tokio::test]
    async fn test_sign_context_cache_expires_and_resigns() {
        let signer = Signer::new(TEST_KEY)
            .unwrap()
            .with_cache_bounds(Duration::from_millis(20), 16);
        let context = vec![FixedBytes::<32>::from(U256::from(1000u64))];

        signer.sign_context(&context).await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let (sig, _) = signer.sign_context(&context).await.unwrap();
        assert_eq!(sig.len(), 65);
        assert_eq!(
            signer.cache_stats(),
            SignatureCacheStats { hits: 0, misses: 2 }
        );
    }

    #[tokio::test]
    async fn test_sign_context_cache_cap_drops_and_keeps_serving() {
        let signer = Signer::new(TEST_KEY)
            .unwrap()
            .with_cache_bounds(Duration::from_secs(60), 4);
        for i in 0..10u64 {
            let ctx = vec![FixedBytes::<32>::from(U256::from(i))];
            signer.sign_context(&ctx).await.unwrap();
        }
        // Ten distinct inputs through a 4-entry cap: every one signed once,
        // nothing wrongly served from cache, no panic on the sweeps.
        assert_eq!(
            signer.cache_stats(),
            SignatureCacheStats {
                hits: 0,
                misses: 10
            }
        );
        // The most recent input is still cached.
        let ctx = vec![FixedBytes::<32>::from(U256::from(9u64))];
        signer.sign_context(&ctx).await.unwrap();
        assert_eq!(signer.cache_stats().hits, 1);
    }

    #[tokio::test]
    async fn test_sign_context_idle_ttl_is_refreshed_by_hits() {
        // Idle TTL: a key that keeps being asked for is never re-signed,
        // however long ago it was first signed.
        let signer = Signer::new(TEST_KEY)
            .unwrap()
            .with_cache_bounds(Duration::from_millis(60), 16);
        let context = vec![FixedBytes::<32>::from(U256::from(7u64))];
        signer.sign_context(&context).await.unwrap();
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(30)).await;
            signer.sign_context(&context).await.unwrap();
        }
        assert_eq!(
            signer.cache_stats(),
            SignatureCacheStats { hits: 5, misses: 1 }
        );
    }

    #[tokio::test]
    async fn test_sign_context_concurrent_identical_requests_sign_once() {
        // The sign task sleeps, so all 32 requests genuinely overlap with
        // the in-flight sign and must coalesce onto it.
        let signer = Arc::new(Signer::new(TEST_KEY).unwrap().with_test_hook(TestHook {
            delay: Duration::from_millis(50),
            fail: false,
        }));
        let context = vec![FixedBytes::<32>::from(U256::from(42u64))];
        let tasks: Vec<_> = (0..32)
            .map(|_| {
                let signer = Arc::clone(&signer);
                let context = context.clone();
                tokio::spawn(async move { signer.sign_context(&context).await.unwrap().0 })
            })
            .collect();
        let mut sigs = Vec::new();
        for t in tasks {
            sigs.push(t.await.unwrap());
        }
        assert!(sigs.iter().all(|s| s == &sigs[0]));
        assert_eq!(
            signer.cache_stats(),
            SignatureCacheStats {
                hits: 31,
                misses: 1
            }
        );
    }

    #[tokio::test]
    async fn test_sign_context_survives_caller_disconnect() {
        // The request that started the sign is aborted mid-flight (what axum
        // does when the client hangs up). The KMS call must still complete
        // and the next request must reuse it rather than sign again.
        let signer = Arc::new(Signer::new(TEST_KEY).unwrap().with_test_hook(TestHook {
            delay: Duration::from_millis(80),
            fail: false,
        }));
        let context = vec![FixedBytes::<32>::from(U256::from(9u64))];
        let leader = {
            let signer = Arc::clone(&signer);
            let context = context.clone();
            tokio::spawn(async move { signer.sign_context(&context).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        leader.abort();
        let (sig, _) = signer.sign_context(&context).await.unwrap();
        assert_eq!(sig.len(), 65);
        assert_eq!(
            signer.cache_stats().misses,
            1,
            "the orphaned sign was reused"
        );
    }

    #[tokio::test]
    async fn test_sign_context_failure_fails_all_waiters_then_retries_fresh() {
        let signer = Arc::new(Signer::new(TEST_KEY).unwrap().with_test_hook(TestHook {
            delay: Duration::from_millis(30),
            fail: true,
        }));
        let context = vec![FixedBytes::<32>::from(U256::from(3u64))];
        let started = std::time::Instant::now();
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let signer = Arc::clone(&signer);
                let context = context.clone();
                tokio::spawn(async move { signer.sign_context(&context).await.is_err() })
            })
            .collect();
        for t in tasks {
            assert!(t.await.unwrap(), "every coalesced waiter sees the failure");
        }
        // One failed sign, broadcast; nobody retried in series.
        assert!(started.elapsed() < Duration::from_millis(300));
        assert_eq!(signer.cache_stats().misses, 1);
        // The dead slot was evicted: the next request starts a fresh sign.
        let _ = signer.sign_context(&context).await;
        assert_eq!(signer.cache_stats().misses, 2);
    }

    #[tokio::test]
    async fn test_sign_context_different_data() {
        let signer = Signer::new(TEST_KEY).unwrap();

        let ctx1 = vec![FixedBytes::<32>::from(U256::from(1000u64))];
        let ctx2 = vec![FixedBytes::<32>::from(U256::from(2000u64))];

        let (sig1, _) = signer.sign_context(&ctx1).await.unwrap();
        let (sig2, _) = signer.sign_context(&ctx2).await.unwrap();

        assert_ne!(
            sig1, sig2,
            "Different context should produce different signatures"
        );
    }

    #[test]
    fn test_kms_name_parse() {
        let name = KmsKeyName::parse(
            "projects/st0x-production/locations/europe-west3/keyRings/st0x-oracle/cryptoKeys/oracle-signer/cryptoKeyVersions/1",
        )
        .unwrap();
        assert_eq!(
            name,
            KmsKeyName {
                project: "st0x-production".into(),
                location: "europe-west3".into(),
                keyring: "st0x-oracle".into(),
                key: "oracle-signer".into(),
                version: 1,
            }
        );
    }

    #[test]
    fn test_kms_name_parse_rejects_garbage() {
        assert!(KmsKeyName::parse("not-a-resource-name").is_err());
        // Key name without an explicit version must be rejected: a version
        // bump changes the signer address, so it must be a deliberate,
        // reviewed config change, never an implicit "latest".
        assert!(KmsKeyName::parse("projects/p/locations/l/keyRings/r/cryptoKeys/k").is_err());
        assert!(KmsKeyName::parse(
            "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/latest"
        )
        .is_err());
    }

    /// Property fuzz: sign + recover roundtrips for any context array
    /// the production code might ever produce. Per RAI-363.
    ///
    /// Two invariants guarded here:
    ///
    /// 1. Signing any non-empty `Vec<FixedBytes<32>>` of up to 8
    ///    elements never panics and always emits a 65-byte EIP-191
    ///    signature plus the configured signer address.
    /// 2. Any two distinct contexts in the same proptest case
    ///    produce distinct signatures, i.e. the signer can't
    ///    accidentally collapse different inputs onto a single
    ///    signature (which would let a strategy replay the wrong
    ///    price under a fresh hash).
    use proptest::prelude::*;

    fn arb_context() -> impl Strategy<Value = Vec<FixedBytes<32>>> {
        proptest::collection::vec(any::<[u8; 32]>().prop_map(FixedBytes::<32>::from), 1..=8)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn sign_context_never_panics_and_emits_65_byte_signature(ctx in arb_context()) {
            let signer = Signer::new(TEST_KEY).unwrap();
            let rt = tokio::runtime::Runtime::new().unwrap();
            let (sig, addr) = rt.block_on(signer.sign_context(&ctx)).unwrap();
            prop_assert_eq!(sig.len(), 65);
            prop_assert_eq!(addr, signer.address());
        }

        #[test]
        fn distinct_contexts_produce_distinct_signatures(
            a in arb_context(),
            b in arb_context(),
        ) {
            prop_assume!(a != b);
            let signer = Signer::new(TEST_KEY).unwrap();
            let rt = tokio::runtime::Runtime::new().unwrap();
            let (sig_a, _) = rt.block_on(signer.sign_context(&a)).unwrap();
            let (sig_b, _) = rt.block_on(signer.sign_context(&b)).unwrap();
            prop_assert_ne!(sig_a, sig_b);
        }
    }
}
