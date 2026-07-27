//! GCP KMS signer smoke test. Runs only when `RUN_KMS_SMOKE=1` so it doesn't
//! execute in the normal `cargo test` suite (it needs real GCP credentials
//! with `cloudkms.signerVerifier` on the key).
//!
//! Usage:
//!   SIGNER_KMS_KEY=projects/…/cryptoKeyVersions/1 \
//!   EXPECTED_SIGNER=0x… \
//!   RUN_KMS_SMOKE=1 cargo test --test smoke_kms -- --nocapture
//!
//! This doubles as the signer-address verification step (RAI-1002): before
//! RAI-1077 bakes the address into redeployed orders, this test must pass
//! with EXPECTED_SIGNER set to the address derived from the KMS public key.

use alloy::primitives::{Address, FixedBytes, U256};
use st0x_oracle_server::sign::Signer;

fn smoke_enabled() -> bool {
    match std::env::var("RUN_KMS_SMOKE").as_deref() {
        Ok("1") => true,
        Err(_) | Ok("") | Ok("0") => false,
        // Anything else (true/yes/on/…) must fail loudly rather than skip
        // and report green: this test gates the trusted-signer address that
        // RAI-1077 hardcodes into live orders — a silent no-op pass is how a
        // wrong key version ships to production.
        Ok(other) => {
            panic!("RUN_KMS_SMOKE must be '1' to run (or unset/'0' to skip); got {other:?}")
        }
    }
}

#[tokio::test]
async fn kms_signer_address_and_roundtrip() {
    if !smoke_enabled() {
        eprintln!("RUN_KMS_SMOKE != 1 — skipping KMS smoke test");
        return;
    }

    let key = std::env::var("SIGNER_KMS_KEY")
        .expect("SIGNER_KMS_KEY must be set to the full key version resource name");
    let signer = Signer::from_gcp_kms(&key)
        .await
        .expect("failed to construct GCP KMS signer");

    let address = signer.address();
    eprintln!("KMS signer address: {address}");

    // Pin the on-chain trusted signer: RAI-1077 hardcodes this address in the
    // redeployed orders' Rainlang, so any drift here must fail loudly.
    if let Ok(expected) = std::env::var("EXPECTED_SIGNER") {
        let expected: Address = expected.parse().expect("EXPECTED_SIGNER is not an address");
        assert_eq!(
            address, expected,
            "KMS-derived signer address does not match EXPECTED_SIGNER"
        );
    }

    // Round-trip: sign a context and verify the signature recovers to the
    // signer address under EIP-191 (exactly what the orderbook's
    // SignatureChecker does at fill time).
    let context = vec![
        FixedBytes::<32>::from(U256::from(1u64)),
        FixedBytes::<32>::from(U256::from(123_456u64)),
        FixedBytes::<32>::from(U256::from(1_700_000_000u64)),
    ];
    let (sig_bytes, addr) = signer
        .sign_context(&context)
        .await
        .expect("KMS sign_context failed");
    assert_eq!(addr, address);
    assert_eq!(sig_bytes.len(), 65, "expected 65-byte (r,s,v) signature");

    let packed: Vec<u8> = context.iter().flat_map(|b| b.as_slice().to_vec()).collect();
    let hash = alloy::primitives::keccak256(&packed);
    let signature = alloy::signers::Signature::from_raw(&sig_bytes).expect("invalid signature");
    let recovered = signature
        .recover_address_from_msg(hash.as_slice())
        .expect("ecrecover failed");
    assert_eq!(
        recovered, address,
        "signature does not recover to the signer address"
    );

    eprintln!("KMS signing round-trip OK — recovered {recovered}");
}
