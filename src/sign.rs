use alloy::primitives::{Address, Bytes, FixedBytes};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer as AlloySigner;
// EIP-191 signing for Rain signed context

/// EIP-191 signer for Rain signed context.
pub struct Signer {
    inner: PrivateKeySigner,
}

impl Signer {
    /// Create a new signer from a hex private key (with or without 0x prefix).
    pub fn new(private_key: &str) -> anyhow::Result<Self> {
        let key = private_key.strip_prefix("0x").unwrap_or(private_key);
        let signer: PrivateKeySigner = key.parse()?;
        Ok(Self { inner: signer })
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
        let signature = self.inner.sign_message(hash.as_slice()).await?;

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
