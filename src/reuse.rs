//! Reuse a still-valid signature when only the timestamps moved.
//!
//! The content cache in [`crate::sign`] removes duplicate signatures
//! inside one price frame. This layer removes them ACROSS frames: when a
//! new frame arrives for a pair but the price (and everything else that
//! is signed) is unchanged, the previous signature is still a correct,
//! unexpired statement about the same price, so the oracle hands it back
//! instead of signing the same price under a fresh publish_time.
//!
//! Only the schemas that sign an expiry (v5, v6) take part: a taker reading
//! those can see exactly how long the quote is good for. v1 and v4 sign
//! only publish_time, and the strategy's own staleness rule on chain is
//! not visible here, so they always get the newest frame.
//!
//! The reuse is bounded by the OLD quote's expiry with a margin
//! (`min_remaining_secs`): a signature that dies before a taker could
//! settle against it is not offered. Pricing stamps expiry 20 to 30 seconds
//! after the frame today, so a stable price costs one signature per ~20s
//! instead of one per 5s frame; a moving price still gets a fresh one each
//! frame.

use crate::oracle::OracleResponse;
use alloy::primitives::{Address, FixedBytes};
use std::collections::HashMap;
use std::sync::Mutex;

/// Everything in a v5/v6 signed context EXCEPT publish_time and expiry.
/// If all of this matches the previous signature for the pair, the two
/// contexts state the same price under the same session for the same
/// tokens; only their timestamps differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub price_bytes: [u8; 32],
    pub session_tag: FixedBytes<32>,
    pub session_start: u64,
    pub session_end: u64,
    pub input_token: Address,
    pub output_token: Address,
    /// v6 NAV ratio; zero for v5 (its sentinel value, never signed there).
    pub nav_ratio: FixedBytes<32>,
}

/// One (schema, symbol, direction) slot.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReuseKey {
    pub schema: &'static str,
    pub symbol: String,
    /// `PriceDirection::as_str()`.
    pub direction: &'static str,
}

struct Stored {
    fingerprint: Fingerprint,
    expiry_unix_secs: u64,
    response: OracleResponse,
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
    /// `fingerprint` and is good for at least `min_remaining_secs` more.
    pub fn lookup(
        &self,
        key: &ReuseKey,
        fingerprint: &Fingerprint,
        now_secs: u64,
    ) -> Option<OracleResponse> {
        if !self.enabled() {
            return None;
        }
        let guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let stored = guard.get(key)?;
        if stored.fingerprint != *fingerprint {
            return None;
        }
        if stored.expiry_unix_secs < now_secs.saturating_add(self.min_remaining_secs) {
            return None;
        }
        Some(stored.response.clone())
    }

    /// Remember a freshly signed response so later frames with the same
    /// fingerprint can reuse it until `expiry_unix_secs` (minus margin).
    pub fn store(
        &self,
        key: ReuseKey,
        fingerprint: Fingerprint,
        expiry_unix_secs: u64,
        response: OracleResponse,
    ) {
        if !self.enabled() {
            return;
        }
        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            key,
            Stored {
                fingerprint,
                expiry_unix_secs,
                response,
            },
        );
    }
}
