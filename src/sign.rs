use alloy::primitives::{Address, Bytes, FixedBytes};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer as AlloySigner;
use alloy_signer_gcp::{GcpKeyRingRef, GcpSigner, KeySpecifier};
use gcloud_sdk::google::cloud::kms::v1::key_management_service_client::KeyManagementServiceClient;
use gcloud_sdk::GoogleApi;
use std::time::Duration;
// EIP-191 signing for Rain signed context

/// Upper bound on a single sign attempt. Only meaningful for the KMS
/// backend, where signing is a network RPC.
const SIGN_TIMEOUT: Duration = Duration::from_secs(5);

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
        })
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

        // keccak256 of the packed data
        let hash = alloy::primitives::keccak256(&packed);

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

        Ok((Bytes::from(signature.as_bytes().to_vec()), self.address()))
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
}
