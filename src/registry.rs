use crate::config::TokenEntry;
use alloy::primitives::Address;
use std::collections::HashMap;
use std::str::FromStr;

/// Maps on-chain token addresses to st0x.pricing asset symbols.
///
/// The "base" token is always the tStock (e.g. wtCOIN), and the
/// "quote" token is always USDC.
#[derive(Debug, Clone)]
pub struct TokenRegistry {
    /// token address → pricing symbol
    tokens: HashMap<Address, String>,
    /// USDC address on Base
    pub quote_token: Address,
}

/// Which rate from a pricing-service `Quote` this request needs.
///
/// The pricing service publishes both rates independently (the model
/// applies its own per-direction spread); we don't synthesize one from
/// the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceDirection {
    /// Order input is USDC (quote), output is tStock (base) — the order
    /// is the venue where takers swap quote→base. Priced by the
    /// DIRECTIONAL rate `rate_quote_to_base`, inverted into Raindex
    /// `ratio = input/output = quote per base` units by
    /// `pick_rate_bytes`. (The unit-compatible `rate_base_to_quote`
    /// carries the OTHER direction's spread — picking it was the
    /// 2026-08-07 crossed-orders bug.)
    QuoteToBase,
    /// Order input is tStock (base), output is USDC (quote) — the
    /// base→quote venue. Priced by `rate_base_to_quote`, inverted into
    /// `base per quote` ratio units by `pick_rate_bytes`.
    BaseToQuote,
}

impl PriceDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QuoteToBase => "quote_to_base",
            Self::BaseToQuote => "base_to_quote",
        }
    }
}

/// A resolved token pair from the registry.
#[derive(Debug, Clone)]
pub struct ResolvedPair {
    /// Pricing-service asset symbol (e.g. "COIN").
    pub symbol: String,
    /// Which of the two rates in the live `Quote` to sign for this
    /// request. Determined by which side of the swap is USDC.
    pub direction: PriceDirection,
}

impl TokenRegistry {
    /// Build a registry from env-style config.
    ///
    /// `entries` is a list of (token_address, pricing_symbol) pairs.
    /// `quote_token` is the USDC address.
    pub fn new(entries: Vec<(String, String)>, quote_token: &str) -> anyhow::Result<Self> {
        let quote = Address::from_str(quote_token)
            .map_err(|e| anyhow::anyhow!("Invalid quote token address: {}", e))?;

        let mut tokens = HashMap::new();
        for (addr_str, symbol) in entries {
            let addr = Address::from_str(&addr_str)
                .map_err(|e| anyhow::anyhow!("Invalid token address '{}': {}", addr_str, e))?;
            tokens.insert(addr, symbol);
        }

        Ok(Self {
            tokens,
            quote_token: quote,
        })
    }

    /// Build a registry from parsed config.toml entries.
    pub fn from_config(entries: &[TokenEntry], quote_token: &str) -> anyhow::Result<Self> {
        let pairs: Vec<(String, String)> = entries
            .iter()
            .map(|t| (t.address.clone(), t.symbol.clone()))
            .collect();
        Self::new(pairs, quote_token)
    }

    /// Resolve an input/output token pair to a pricing symbol + direction.
    pub fn resolve(
        &self,
        input_token: Address,
        output_token: Address,
    ) -> anyhow::Result<ResolvedPair> {
        // Case 1: order input=USDC, output=tStock — the ORDER (maker) receives
        // quote and pays out base: it is the venue where takers swap
        // quote→base. Priced by pricing's `rate_quote_to_base`, inverted
        // into ratio units (see `pick_rate_bytes`).
        if input_token == self.quote_token {
            let symbol = self.tokens.get(&output_token).ok_or_else(|| {
                anyhow::anyhow!("Unknown tStock token: {} (not in registry)", output_token)
            })?;
            return Ok(ResolvedPair {
                symbol: symbol.clone(),
                direction: PriceDirection::QuoteToBase,
            });
        }

        // Case 2: order input=tStock, output=USDC — the maker receives base
        // and pays out quote: the base→quote venue. Priced by
        // `rate_base_to_quote`, inverted into ratio units.
        if output_token == self.quote_token {
            let symbol = self.tokens.get(&input_token).ok_or_else(|| {
                anyhow::anyhow!("Unknown tStock token: {} (not in registry)", input_token)
            })?;
            return Ok(ResolvedPair {
                symbol: symbol.clone(),
                direction: PriceDirection::BaseToQuote,
            });
        }

        anyhow::bail!(
            "Neither token is USDC ({}). Got input={}, output={}",
            self.quote_token,
            input_token,
            output_token
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_registry() -> TokenRegistry {
        TokenRegistry::new(
            vec![
                (
                    "0x1111111111111111111111111111111111111111".into(),
                    "COIN".into(),
                ),
                (
                    "0x2222222222222222222222222222222222222222".into(),
                    "RKLB".into(),
                ),
            ],
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
        )
        .unwrap()
    }

    #[test]
    fn test_resolve_buy_tstock() {
        let reg = test_registry();
        let usdc = reg.quote_token;
        let coin = Address::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let pair = reg.resolve(usdc, coin).unwrap();
        assert_eq!(pair.symbol, "COIN");
        assert_eq!(pair.direction, PriceDirection::QuoteToBase);
    }

    #[test]
    fn test_resolve_sell_tstock() {
        let reg = test_registry();
        let usdc = reg.quote_token;
        let rklb = Address::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let pair = reg.resolve(rklb, usdc).unwrap();
        assert_eq!(pair.symbol, "RKLB");
        assert_eq!(pair.direction, PriceDirection::BaseToQuote);
    }

    #[test]
    fn test_resolve_unknown_token() {
        let reg = test_registry();
        let usdc = reg.quote_token;
        let unknown = Address::from_str("0x3333333333333333333333333333333333333333").unwrap();
        assert!(reg.resolve(usdc, unknown).is_err());
    }

    #[test]
    fn test_resolve_no_usdc() {
        let reg = test_registry();
        let coin = Address::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let rklb = Address::from_str("0x2222222222222222222222222222222222222222").unwrap();
        assert!(reg.resolve(coin, rklb).is_err());
    }
}
