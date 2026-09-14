//! Reuse a still-valid signature when only the timestamps moved.
//!
//! The content cache in [`crate::sign`] removes duplicate signatures
//! inside one price frame. This layer removes them ACROSS frames: when a
//! new frame arrives for a pair but the price (and everything else that
//! is signed) is unchanged, the previous signature is still a correct,
//! unexpired statement about the same price, so the oracle hands it back
//! instead of signing the same price under a fresh publish_time.
//!
//! Only the schemas that sign an expiry (v5, v6, v7) take part: a taker
//! reading those can see exactly how long the quote is good for. v1 and
//! v4 sign only publish_time, and the strategy's own staleness rule on
//! chain is not visible here, so they always get the newest frame.
//!
//! What a reused response looks like to the taker: the ORIGINAL
//! publish_time (slot 2) and expiry (slot 8), both signed. The strategy's
//! `max-staleness` guard runs against that publish_time, so it must be
//! comfortably above the reuse window (expiry horizon minus the margin
//! below, ~20s with today's pricing) or flat prices start reverting as
//! stale on chain. The oracle leg of the parity board's source-age panel
//! steps up by the same amount on flat symbols; that is this layer, not
//! a stalled feed.
//!
//! The reuse is bounded by the OLD quote's expiry with a margin
//! (`min_remaining_secs`): a signature that dies before a taker could
//! settle against it is not offered. It is also bounded by the NEW
//! frame's expiry: pricing owns the horizon, and if it shortens it while
//! the price stands (a recalibrated profile, a mark it has disowned) the
//! oracle must not keep vouching for the longer one. Pricing stamps
//! expiry 20 to 30 seconds after the frame today, so a stable price costs
//! one signature per ~20s instead of one per 5s frame; a moving price
//! still gets a fresh one each frame.

use crate::oracle::OracleResponse;
use crate::pricing_client::QuoteGeneration;
use alloy::primitives::{Address, FixedBytes};
use std::collections::HashMap;
use std::sync::Mutex;

/// Context slots that are allowed to differ between two frames that state
/// the same price: publish_time and the model's expiry. Every other slot
/// (version, price, session tag and bounds, tokens, v6's NAV ratio) must
/// match byte for byte. Derived from the built context rather than
/// re-enumerated by hand, so a schema that adds a signed slot is covered
/// without anyone remembering to mirror it here.
const PUBLISH_TIME_SLOT: usize = 2;
const EXPIRY_SLOT: usize = 8;

/// True when `a` and `b` are the same signed statement up to timestamps.
fn same_statement(a: &[FixedBytes<32>], b: &[FixedBytes<32>]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .enumerate()
            .all(|(i, (x, y))| i == PUBLISH_TIME_SLOT || i == EXPIRY_SLOT || x == y)
}

/// One slot per (schema, symbol, direction, tokens). The tokens are part
/// of the key, not just of the signed bytes, because the registry lets
/// several token addresses map to one symbol; keyed on the symbol alone,
/// two vaults of one stock would overwrite each other's slot on every
/// request and never reuse anything.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReuseKey {
    pub schema: &'static str,
    pub symbol: String,
    /// `PriceDirection::as_str()`.
    pub direction: &'static str,
    pub input_token: Address,
    pub output_token: Address,
}

#[derive(Clone)]
struct Stored {
    expiry_unix_secs: u64,
    response: OracleResponse,
    generation: QuoteGeneration,
}

/// A prior signed response together with the deadline that is actually
/// encoded in it. Callers must validate this stored deadline immediately
/// before returning the response; the current candidate frame can have a
/// later expiry and is not evidence that the older signature is still live.
pub struct ReusedResponse {
    pub expiry_unix_secs: u64,
    pub response: OracleResponse,
}

pub struct ReuseCache {
    entries: Mutex<HashMap<ReuseKey, Stored>>,
    /// Minimum seconds the previous quote must still have before its
    /// expiry to be offered again. Zero disables reuse entirely.
    min_remaining_secs: u64,
}

impl ReuseCache {
    pub fn new(min_remaining_secs: u64) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            min_remaining_secs,
        }
    }

    pub fn enabled(&self) -> bool {
        self.min_remaining_secs > 0
    }

    /// The previous response for `key`, if it states the same thing as
    /// `context` (up to timestamps), does not outlive the current frame's
    /// `expiry_unix_secs`, and is good for at least `min_remaining_secs`
    /// more.
    pub(crate) fn lookup(
        &self,
        key: &ReuseKey,
        context: &[FixedBytes<32>],
        expiry_unix_secs: u64,
        now_secs: u64,
        generation: &QuoteGeneration,
    ) -> Option<ReusedResponse> {
        if !self.enabled() {
            return None;
        }
        let stored = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)?
            .clone();
        if !stored.generation.is_same(generation) || !stored.generation.is_live() {
            return None;
        }
        if !same_statement(&stored.response.context, context) {
            return None;
        }
        if stored.expiry_unix_secs > expiry_unix_secs {
            return None;
        }
        if stored.expiry_unix_secs < now_secs.saturating_add(self.min_remaining_secs) {
            return None;
        }
        Some(ReusedResponse {
            expiry_unix_secs: stored.expiry_unix_secs,
            response: stored.response.clone(),
        })
    }

    /// Remember a freshly signed response so later frames stating the
    /// same thing in the same live generation can reuse it until
    /// `expiry_unix_secs` (minus margin). Returns false when reuse is disabled
    /// or the producing generation was revoked before the store could begin.
    pub(crate) fn store(
        &self,
        key: ReuseKey,
        expiry_unix_secs: u64,
        response: OracleResponse,
        generation: &QuoteGeneration,
    ) -> bool {
        if !self.enabled() {
            return false;
        }
        generation
            .while_live(|| {
                let mut guard = self
                    .entries
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                guard.insert(
                    key,
                    Stored {
                        expiry_unix_secs,
                        response,
                        generation: generation.clone(),
                    },
                );
            })
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Bytes;

    fn slot(n: u8) -> FixedBytes<32> {
        FixedBytes::from([n; 32])
    }

    fn key() -> ReuseKey {
        ReuseKey {
            schema: "v5",
            symbol: "COIN".into(),
            direction: "base_to_quote",
            input_token: Address::from([0x11; 20]),
            output_token: Address::from([0x22; 20]),
        }
    }

    fn response(publish_time: u8, expiry: u8, signature: u8) -> OracleResponse {
        let mut context: Vec<FixedBytes<32>> = (0..9).map(slot).collect();
        context[PUBLISH_TIME_SLOT] = slot(publish_time);
        context[EXPIRY_SLOT] = slot(expiry);
        OracleResponse {
            signer: Address::ZERO,
            context,
            signature: Bytes::from(vec![signature]),
        }
    }

    #[test]
    fn same_statement_ignores_only_the_timestamp_slots() {
        let base: Vec<FixedBytes<32>> = (0..9).map(slot).collect();
        let mut moved = base.clone();
        moved[PUBLISH_TIME_SLOT] = slot(200);
        moved[EXPIRY_SLOT] = slot(201);
        assert!(same_statement(&base, &moved));

        for i in (0..9).filter(|i| *i != PUBLISH_TIME_SLOT && *i != EXPIRY_SLOT) {
            let mut other = base.clone();
            other[i] = slot(250);
            assert!(!same_statement(&base, &other), "slot {i} must be compared");
        }

        let mut longer = base.clone();
        longer.push(slot(9));
        assert!(!same_statement(&base, &longer), "a v6 slot 9 is not a v5");
    }

    #[test]
    fn revoked_store_cannot_overwrite_repopulated_generation() {
        let cache = ReuseCache::new(1);
        let old_generation = QuoteGeneration::new_for_test();
        let new_generation = QuoteGeneration::new_for_test();
        let old_response = response(10, 100, 0xa1);
        let new_response = response(20, 100, 0xb2);

        old_generation.revoke_for_test();
        assert!(cache.store(key(), 100, new_response.clone(), &new_generation));
        assert!(
            !cache.store(key(), 100, old_response, &old_generation),
            "a response finishing after revocation must not enter the cache"
        );

        let reused = cache
            .lookup(&key(), &new_response.context, 100, 0, &new_generation)
            .expect("the replacement generation must retain its own entry");
        assert_eq!(reused.response.signature, new_response.signature);
    }

    #[test]
    fn pre_invalidation_entry_does_not_match_repopulated_generation() {
        let cache = ReuseCache::new(1);
        let old_generation = QuoteGeneration::new_for_test();
        let new_generation = QuoteGeneration::new_for_test();
        let old_response = response(10, 100, 0xa1);
        let new_response = response(20, 100, 0xb2);

        assert!(cache.store(key(), 100, old_response, &old_generation));
        old_generation.revoke_for_test();
        assert!(
            cache
                .lookup(&key(), &new_response.context, 100, 0, &new_generation,)
                .is_none(),
            "an entry from before invalidation must not cross generations"
        );

        assert!(cache.store(key(), 100, new_response.clone(), &new_generation));
        let reused = cache
            .lookup(&key(), &new_response.context, 100, 0, &new_generation)
            .expect("the replacement generation may reuse its own entry");
        assert_eq!(reused.response.signature, new_response.signature);
    }
}
