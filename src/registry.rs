use alloy::primitives::Address;
use std::collections::HashMap;
use std::str::FromStr;

/// Maps on-chain token addresses to Alpaca ticker symbols.
///
/// The "base" token is always the tStock (e.g. wtCOIN), and the
/// "quote" token is always USDC.
#[derive(Debug, Clone)]
pub struct TokenRegistry {
    /// token address (lowercased) → Alpaca symbol
    tokens: HashMap<Address, String>,
    /// USDC address on Base
    pub quote_token: Address,
}

/// A resolved token pair from the registry.
#[derive(Debug, Clone)]
pub struct ResolvedPair {
    /// Alpaca ticker symbol (e.g. "COIN")
    pub symbol: String,
    /// Whether the price should be inverted for this order direction.
    /// - false: input is quote (USDC), output is base (tStock) → price as-is (USDC per share)
    /// - true: input is base (tStock), output is quote (USDC) → inverted (shares per USDC)
    pub inverted: bool,
}

impl TokenRegistry {
    /// Build a registry from env-style config.
    ///
    /// `entries` is a list of (token_address, alpaca_symbol) pairs.
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

    /// Resolve an input/output token pair to an Alpaca symbol and direction.
    pub fn resolve(
        &self,
        input_token: Address,
        output_token: Address,
    ) -> anyhow::Result<ResolvedPair> {
        // Case 1: input=USDC, output=tStock → price as-is (how many USDC per share)
        if input_token == self.quote_token {
            let symbol = self.tokens.get(&output_token).ok_or_else(|| {
                anyhow::anyhow!("Unknown tStock token: {} (not in registry)", output_token)
            })?;
            return Ok(ResolvedPair {
                symbol: symbol.clone(),
                inverted: false,
            });
        }

        // Case 2: input=tStock, output=USDC → inverted (how many shares per USDC)
        if output_token == self.quote_token {
            let symbol = self.tokens.get(&input_token).ok_or_else(|| {
                anyhow::anyhow!("Unknown tStock token: {} (not in registry)", input_token)
            })?;
            return Ok(ResolvedPair {
                symbol: symbol.clone(),
                inverted: true,
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
        assert!(!pair.inverted);
    }

    #[test]
    fn test_resolve_sell_tstock() {
        let reg = test_registry();
        let usdc = reg.quote_token;
        let rklb = Address::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let pair = reg.resolve(rklb, usdc).unwrap();
        assert_eq!(pair.symbol, "RKLB");
        assert!(pair.inverted);
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
