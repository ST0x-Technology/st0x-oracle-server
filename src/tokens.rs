//! The token set this instance signs for, shared by every handler.
//!
//! The set can be replaced while the server runs. A handler takes one
//! snapshot with [`Tokens::current`] at its top and resolves every item of
//! the request against it, so one response never mixes two sets.

use std::sync::{Arc, RwLock};

use crate::registry::TokenRegistry;
use crate::token_file::Projection;

/// One coherent token set: the registry requests resolve against, the
/// symbols the pricing subscription asks for, and where the set came from.
#[derive(Debug, Clone)]
pub struct TokenSet {
    pub registry: TokenRegistry,
    /// Every configured symbol, in config order.
    pub symbols: Vec<String>,
    /// The token file slice the set was built from. `None` for a config
    /// that carries its `[[tokens]]` inline.
    pub projection: Option<Projection>,
    /// The bucket object generation the set was read from. `None` when it
    /// did not come from the bucket.
    pub generation: Option<i64>,
}

impl TokenSet {
    pub fn new(registry: TokenRegistry, symbols: Vec<String>) -> Self {
        Self {
            registry,
            symbols,
            projection: None,
            generation: None,
        }
    }
}

/// Shared handle to the running [`TokenSet`]. The lock is held only to
/// clone or swap the `Arc`, never across an `.await`.
#[derive(Debug, Clone)]
pub struct Tokens(Arc<RwLock<Arc<TokenSet>>>);

impl Tokens {
    pub fn new(set: TokenSet) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(set))))
    }

    /// The running set. Take it once per request and pass it down.
    pub fn current(&self) -> Arc<TokenSet> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Swap in `next` and return the set it replaced. Requests that already
    /// hold the old set finish on it.
    pub fn replace(&self, next: TokenSet) -> Arc<TokenSet> {
        let mut guard = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::replace(&mut *guard, Arc::new(next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(symbol: &str) -> TokenSet {
        TokenSet::new(
            TokenRegistry::new(
                vec![(
                    "0x1111111111111111111111111111111111111111".into(),
                    symbol.into(),
                )],
                "0x2222222222222222222222222222222222222222",
            )
            .unwrap(),
            vec![symbol.into()],
        )
    }

    #[test]
    fn a_snapshot_outlives_a_replace() {
        let tokens = Tokens::new(set("COIN"));
        let before = tokens.current();
        let replaced = tokens.replace(set("TSLA"));
        assert!(Arc::ptr_eq(&before, &replaced));
        assert_eq!(before.symbols, ["COIN"]);
        assert_eq!(tokens.current().symbols, ["TSLA"]);
    }

    #[test]
    fn a_batch_resolves_one_snapshot_across_an_item_boundary_replace() {
        use alloy::primitives::Address;

        let quote = Address::from([0x22; 20]);
        let coin = Address::from([0x11; 20]);
        let dram = Address::from([0x33; 20]);
        let old = TokenSet::new(
            TokenRegistry::new(
                vec![
                    (coin.to_string(), "COIN".into()),
                    (dram.to_string(), "DRAM".into()),
                ],
                &quote.to_string(),
            )
            .unwrap(),
            vec!["COIN".into(), "DRAM".into()],
        );
        let tokens = Tokens::new(old);
        let snapshot = tokens.current();
        let first = snapshot.registry.resolve(quote, coin).unwrap();
        tokens.replace(TokenSet::new(
            TokenRegistry::new(vec![], &quote.to_string()).unwrap(),
            vec![],
        ));
        let second = snapshot.registry.resolve(quote, dram).unwrap();
        assert_eq!(
            [first.symbol.as_str(), second.symbol.as_str()],
            ["COIN", "DRAM"]
        );
        assert!(tokens.current().registry.resolve(quote, coin).is_err());
        assert!(tokens.current().registry.resolve(quote, dram).is_err());
    }
}
