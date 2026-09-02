use alloy::primitives::{Address, Bytes, FixedBytes};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer as AlloySigner;
use alloy_signer_gcp::{GcpKeyRingRef, GcpSigner, KeySpecifier};
use gcloud_sdk::google::cloud::kms::v1::key_management_service_client::KeyManagementServiceClient;
use gcloud_sdk::GoogleApi;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
// EIP-191 signing for Rain signed context

/// Upper bound on a single sign attempt. Only meaningful for the KMS
/// backend, where signing is a network RPC.
const SIGN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a cached signature is kept. Cache validity is not the
/// question here — a signature over given bytes is valid forever, and the
/// key IS the bytes — the TTL only bounds memory: an entry that nobody has
/// asked for in this long belongs to a superseded price frame. Pricing
/// frames arrive every ~5s, so 2 minutes is ~24 frames of headroom.
const SIGNATURE_CACHE_TTL: Duration = Duration::from_secs(120);

/// Hard cap on cached entries; a safety net if the TTL sweep ever falls
/// behind (78 live inputs per frame today, ~1,900 entries in a 2-minute
/// window). When exceeded the whole cache is dropped — cheap, and the next
/// frame refills the live set in one round of misses.
const SIGNATURE_CACHE_MAX_ENTRIES: usize = 16_384;

/// One cache slot. `sig` is a tokio mutex so that concurrent requests for
/// the same not-yet-signed bytes coalesce: the first locker signs, the
/// rest wait on the lock and read its result instead of each paying for a
/// KMS call. A failed sign leaves `None`, so the next caller retries.
struct CacheEntry {
    inserted: Instant,
    sig: tokio::sync::Mutex<Option<Bytes>>,
}

/// Content-addressed signature cache: `keccak256(abi.encodePacked(context))`
/// → signature. Every signed slot (price, publish_time, session, tokens,
/// expiry) is derived from the pricing frame and the pair, never from the
/// wall clock or the caller, so two requests inside one frame sign
/// byte-identical data. Measured 2026-09-01: 58% of KMS signatures were
/// re-signing bytes already signed seconds earlier, at ~£0.11 per 10k HSM
/// operations. Consumers see no difference — same bytes, a valid signature.
struct SignatureCache {
    entries: tokio::sync::Mutex<HashMap<FixedBytes<32>, Arc<CacheEntry>>>,
    ttl: Duration,
    max_entries: usize,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl SignatureCache {
    fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: tokio::sync::Mutex::new(HashMap::new()),
            ttl,
            max_entries,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Return the live slot for `hash`, creating it (and sweeping expired
    /// entries) as needed. The map lock is held only for the lookup; the
    /// signing itself happens under the per-entry lock.
    async fn slot(&self, hash: FixedBytes<32>) -> Arc<CacheEntry> {
        let mut map = self.entries.lock().await;
        let now = Instant::now();
        if let Some(entry) = map.get(&hash) {
            if now.duration_since(entry.inserted) < self.ttl {
                return Arc::clone(entry);
            }
            map.remove(&hash);
        }
        if map.len() >= self.max_entries {
            let ttl = self.ttl;
            map.retain(|_, e| now.duration_since(e.inserted) < ttl);
            if map.len() >= self.max_entries {
                map.clear();
            }
        }
        let entry = Arc::new(CacheEntry {
            inserted: now,
            sig: tokio::sync::Mutex::new(None),
        });
        map.insert(hash, Arc::clone(&entry));
        ::metrics::gauge!("oracle_signature_cache_entries").set(map.len() as f64);
        entry
    }
}

/// Hit/miss counters, for tests and `/status`-style introspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureCacheStats {
    pub hits: u64,
    pub misses: u64,
}

/// EIP-191 signer for Rain signed context.
///
/// Two backends behind the same interface:
/// - a local hex private key (tests / local dev), or
/// - GCP Cloud KMS (production): the key is non-extractable and never enters
///   this process; each signature is a KMS `AsymmetricSign` call authenticated
///   via Application Default Credentials (on GCE, the VM's attached service
///   account — no credential material on the box).
pub struct Signer {
    inner: Box<dyn AlloySigner + Send + Sync>,
    cache: SignatureCache,
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

impl Signer {
    /// Create a new signer from a hex private key (with or without 0x prefix).
    pub fn new(private_key: &str) -> anyhow::Result<Self> {
        let key = private_key.strip_prefix("0x").unwrap_or(private_key);
        let signer: PrivateKeySigner = key.parse()?;
        Ok(Self {
            inner: Box::new(signer),
            cache: SignatureCache::new(SIGNATURE_CACHE_TTL, SIGNATURE_CACHE_MAX_ENTRIES),
        })
    }

    /// Create a signer backed by a GCP Cloud KMS key version.
    ///
    /// `resource_name` is the full key version resource name (the Terraform
    /// stack's `signer_kms_key_version` output). Fails fast if the key is
    /// unreachable, is not secp256k1, or ADC cannot authenticate — better a
    /// loud startup error than serving unsigned/failing requests.
    pub async fn from_gcp_kms(resource_name: &str) -> anyhow::Result<Self> {
        let name = KmsKeyName::parse(resource_name)?;

        // Install a process-level rustls CryptoProvider before gcloud-sdk
        // builds its TLS client: both `ring` and `aws-lc-rs` are in the
        // dependency graph (reqwest vs gcloud-sdk/tonic), so rustls cannot
        // auto-select one and panics at first TLS use. Idempotent — the
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

        Ok(Self {
            inner: Box::new(signer),
            cache: SignatureCache::new(SIGNATURE_CACHE_TTL, SIGNATURE_CACHE_MAX_ENTRIES),
        })
    }

    /// Override the signature cache bounds (tests; also handy for a local
    /// dev loop that wants to see every sign call).
    pub fn with_cache_bounds(mut self, ttl: Duration, max_entries: usize) -> Self {
        self.cache = SignatureCache::new(ttl, max_entries);
        self
    }

    /// Signature cache hit/miss counters since startup.
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
        // abi.encodePacked(bytes32[]) — just concatenate the raw bytes
        let packed: Vec<u8> = context.iter().flat_map(|b| b.as_slice().to_vec()).collect();

        // keccak256 of the packed data — also the cache key: same bytes,
        // same (still valid) signature, no second KMS call.
        let hash = alloy::primitives::keccak256(&packed);

        let entry = self.cache.slot(hash).await;
        let mut cached = entry.sig.lock().await;
        if let Some(sig) = cached.as_ref() {
            self.cache.hits.fetch_add(1, Ordering::Relaxed);
            ::metrics::counter!("oracle_signature_cache_hits_total").increment(1);
            return Ok((sig.clone(), self.address()));
        }
        self.cache.misses.fetch_add(1, Ordering::Relaxed);
        ::metrics::counter!("oracle_signature_cache_misses_total").increment(1);

        // Sign with EIP-191 prefix: the Rain orderbook contract applies
        // toEthSignedMessageHash(hash) before ecrecover, so we must sign
        // the raw hash using sign_message (which internally prefixes with
        // "\x19Ethereum Signed Message:\n32" before signing).
        //
        // On the KMS backend this is a remote AsymmetricSign RPC, so it is
        // bounded by a timeout (a blackholed KMS connection must surface as
        // an error, not hang the request handler while `/` stays green) and
        // retried once for transient failures. The local backend signs in
        // microseconds and never hits either path.
        let signature = match tokio::time::timeout(
            SIGN_TIMEOUT,
            self.inner.sign_message(hash.as_slice()),
        )
        .await
        {
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
                tokio::time::timeout(SIGN_TIMEOUT, self.inner.sign_message(hash.as_slice()))
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "signing timed out after {}s (retry included)",
                            SIGN_TIMEOUT.as_secs()
                        )
                    })??
            }
        };

        let sig = Bytes::from(signature.as_bytes().to_vec());
        *cached = Some(sig.clone());
        Ok((sig, self.address()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;

    // Test private key — DO NOT use in production
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
        let signer = Signer::new(TEST_KEY).unwrap();
        let context = vec![
            FixedBytes::<32>::from(U256::from(1000u64)),
            FixedBytes::<32>::from(U256::from(2000u64)),
        ];

        let (sig1, addr1) = signer.sign_context(&context).await.unwrap();
        let (sig2, addr2) = signer.sign_context(&context).await.unwrap();

        assert_eq!(sig1, sig2, "Same context should produce same signature");
        assert_eq!(addr1, addr2);
        assert_eq!(sig1.len(), 65, "EIP-191 signature should be 65 bytes");
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
    async fn test_sign_context_concurrent_identical_requests_sign_once() {
        let signer = Arc::new(Signer::new(TEST_KEY).unwrap());
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
        // Coalesced: exactly one miss did the signing, the rest waited on it.
        let stats = signer.cache_stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 31);
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
        // Key name without an explicit version must be rejected — a version
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
    ///    produce distinct signatures — i.e. the signer can't
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
