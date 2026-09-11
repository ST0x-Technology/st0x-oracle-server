use alloy::primitives::Address;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,

    /// The settlement stable this deployment quotes against: the
    /// implicit quote side of every pair `TokenRegistry::resolve`
    /// accepts. It is a property of the CHAIN, not of the protocol, and
    /// not even always a USDC — Base settles in Circle's USDC
    /// (`0x8335…2913`), Robinhood Chain (4663) in USDG, Global Dollar
    /// (`0x5fc5…d168`) — so it belongs next to the token registry it is
    /// keyed with rather than in the binary. Nothing reads its symbol;
    /// it is matched by address. Defaults to Base, which is what every
    /// config that predates multichain means.
    #[serde(default = "default_quote_token")]
    pub quote_token: String,

    pub tokens: Vec<TokenEntry>,
    pub pricing: PricingConfig,
    #[serde(default)]
    pub signing: SigningConfig,
}

/// Signing economics. Optional `[signing]` table in the TOML; a missing
/// table or a missing key takes the `Default`. Unknown keys are rejected
/// so a misspelt knob fails config-check instead of silently keeping the
/// default (this is the one runtime tunable an operator reaches for
/// during an incident).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SigningConfig {
    /// Seconds a previous v5/v6/v7 quote must still have before its expiry to
    /// be reused instead of signing an unchanged price under a new
    /// publish_time. 0 disables reuse (every new frame is signed).
    /// Pricing stamps expiry 20 to 30s after the frame, so the default of
    /// 10s leaves a taker a real settlement window.
    pub reuse_min_remaining_secs: u64,
}

impl Default for SigningConfig {
    fn default() -> Self {
        Self {
            reuse_min_remaining_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenEntry {
    pub address: String,
    pub symbol: String,
}

/// Connection settings for the st0x.pricing service. Live `Quote`s are
/// pushed over the WebSocket; secrets (`api_key`) come from the env file
/// as `PRICING_API_KEY` and override the placeholder in the TOML so the
/// committed config can stay free of credentials.
#[derive(Debug, Clone, Deserialize)]
pub struct PricingConfig {
    pub ws_url: String,
    pub consumer: String,
}

/// USDC on Base — the quote token for the Base deployment, and the
/// default when a config names none. Unchanged from when this lived as a
/// constant in `main.rs`.
pub const USDC_BASE: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";

fn default_port() -> u16 {
    3000
}

fn default_quote_token() -> String {
    USDC_BASE.to_string()
}

/// Addresses in the bottom 2^16 of the address space — the zero address,
/// the precompiles, and the `0x…0001`-style sentinels a config carries
/// while the chain's tokens are still being deployed. No ERC20 lands
/// there, so treating them as unfillable placeholders costs nothing and
/// stops a half-filled config from booting a deployment that looks
/// healthy on `/status` (symbols subscribed, quotes warm) while
/// resolving no real order.
fn is_placeholder_address(addr: &Address) -> bool {
    addr.as_slice()[..18].iter().all(|b| *b == 0)
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read config at {}: {}", path.display(), e))?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Failed to parse config {}: {}", path.display(), e))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.tokens.is_empty() {
            anyhow::bail!("config.toml has no [[tokens]] entries");
        }
        let quote = Address::from_str(&self.quote_token)
            .map_err(|e| anyhow::anyhow!("Invalid quote_token {:?}: {}", self.quote_token, e))?;
        if is_placeholder_address(&quote) {
            anyhow::bail!(
                "Placeholder quote_token {} — fill in the chain's settlement stable before releasing this config",
                self.quote_token
            );
        }
        // Reject duplicate addresses up front. The TokenRegistry stores
        // entries in a HashMap so a repeated address would silently
        // overwrite the earlier symbol — better to fail loud at config
        // load than serve the wrong market for that token.
        let mut seen_addresses: HashSet<Address> = HashSet::with_capacity(self.tokens.len());
        for t in &self.tokens {
            let addr = Address::from_str(&t.address)
                .map_err(|e| anyhow::anyhow!("Invalid token address {:?}: {}", t.address, e))?;
            if t.symbol.trim().is_empty() {
                anyhow::bail!("Empty symbol for token {}", t.address);
            }
            if is_placeholder_address(&addr) {
                anyhow::bail!(
                    "Placeholder address {} for {} — replace it with the token's deployed address before releasing this config",
                    t.address,
                    t.symbol
                );
            }
            // The quote token is the quote side of every pair, so an
            // entry repeating it would claim it is also a tStock and make
            // `resolve` answer for a quote/quote order.
            if addr == quote {
                anyhow::bail!(
                    "Token {} ({}) is the quote token — the quote side is implicit and must not be listed as a tStock",
                    t.address,
                    t.symbol
                );
            }
            if !seen_addresses.insert(addr) {
                anyhow::bail!(
                    "Duplicate token address {} in config.toml — each address must appear at most once",
                    t.address
                );
            }
        }
        if self.pricing.ws_url.trim().is_empty() {
            anyhow::bail!("pricing.ws_url must be set");
        }
        if self.pricing.consumer.trim().is_empty() {
            anyhow::bail!("pricing.consumer must be set");
        }
        Ok(())
    }

    pub fn token_pairs(&self) -> Vec<(String, String)> {
        self.tokens
            .iter()
            .map(|t| (t.address.clone(), t.symbol.clone()))
            .collect()
    }

    pub fn symbols(&self) -> Vec<String> {
        self.tokens.iter().map(|t| t.symbol.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN_PRICING: &str = r#"
            [pricing]
            ws_url = "ws://st0x-pricing:8080/ws"
            consumer = "oracle"
        "#;

    #[test]
    fn parses_minimal_config() {
        let text = format!(
            r#"
            port = 4000
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "COIN"
            {MIN_PRICING}
        "#
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.port, 4000);
        assert_eq!(cfg.tokens.len(), 1);
        assert_eq!(cfg.pricing.ws_url, "ws://st0x-pricing:8080/ws");
        assert_eq!(cfg.pricing.consumer, "oracle");
    }

    #[test]
    fn defaults_applied() {
        let text = format!(
            r#"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "COIN"
            {MIN_PRICING}
        "#
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.port, 3000);
    }

    #[test]
    fn signing_defaults_and_overrides() {
        let base = format!(
            r#"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "COIN"
            {MIN_PRICING}
        "#
        );
        let cfg: Config = toml::from_str(&base).unwrap();
        assert_eq!(
            cfg.signing.reuse_min_remaining_secs, 10,
            "no table: default"
        );

        let cfg: Config = toml::from_str(&format!("{base}\n[signing]\n")).unwrap();
        assert_eq!(
            cfg.signing.reuse_min_remaining_secs, 10,
            "empty table: default"
        );

        let cfg: Config = toml::from_str(&format!(
            "{base}\n[signing]\nreuse_min_remaining_secs = 0\n"
        ))
        .unwrap();
        assert_eq!(cfg.signing.reuse_min_remaining_secs, 0);
    }

    #[test]
    fn rejects_unknown_signing_key() {
        // A typo in the one incident-time knob must fail loud, not keep
        // the default.
        let text = format!(
            r#"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "COIN"
            {MIN_PRICING}
            [signing]
            reuse_min_remaining_sec = 0
        "#
        );
        let err = toml::from_str::<Config>(&text).unwrap_err().to_string();
        assert!(err.contains("reuse_min_remaining_sec"), "got: {err}");
    }

    #[test]
    fn rejects_empty_tokens() {
        let text = format!(
            r#"tokens = []
{MIN_PRICING}"#
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_bad_address() {
        let text = format!(
            r#"
            [[tokens]]
            address = "not-an-address"
            symbol = "COIN"
            {MIN_PRICING}
        "#
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_addresses() {
        let text = format!(
            r#"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "COIN"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "OTHER"
            {MIN_PRICING}
        "#
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("Duplicate token address"),
            "expected duplicate-address error, got: {err}"
        );
    }

    /// Robinhood Chain settles in USDG, not a USDC at all, so a
    /// deployment there sets `quote_token` and everything else — the
    /// registry, the direction logic — is unchanged.
    #[test]
    fn quote_token_defaults_to_base_and_is_overridable() {
        let base = format!(
            r#"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "wtCOIN"
            {MIN_PRICING}
        "#
        );
        let cfg: Config = toml::from_str(&base).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.quote_token, USDC_BASE);

        let robinhood = format!(
            r#"
            quote_token = "0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "wtCOIN"
            {MIN_PRICING}
        "#
        );
        let cfg: Config = toml::from_str(&robinhood).unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.quote_token,
            "0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168"
        );
    }

    #[test]
    fn rejects_bad_quote_token() {
        let text = format!(
            r#"
            quote_token = "not-an-address"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "wtCOIN"
            {MIN_PRICING}
        "#
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("quote_token"));
    }

    /// The forcing function on a config whose chain has not finished
    /// deploying: sentinel token addresses must be filled in before the
    /// config can load, so a placeholder registry can never boot.
    #[test]
    fn rejects_placeholder_token_address() {
        let text = format!(
            r#"
            [[tokens]]
            address = "0x0000000000000000000000000000000000000001"
            symbol = "wtCOIN"
            {MIN_PRICING}
        "#
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("Placeholder address"),
            "expected placeholder error, got: {err}"
        );
    }

    #[test]
    fn rejects_placeholder_quote_token() {
        let text = format!(
            r#"
            quote_token = "0x0000000000000000000000000000000000000000"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "wtCOIN"
            {MIN_PRICING}
        "#
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("Placeholder quote_token"),
            "expected placeholder error, got: {err}"
        );
    }

    #[test]
    fn rejects_quote_token_listed_as_tstock() {
        let text = format!(
            r#"
            quote_token = "0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168"
            [[tokens]]
            address = "0x5FC5360d0400A0fD4F2Af552add042d716F1D168"
            symbol = "wtUSDG"
            {MIN_PRICING}
        "#
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("is the quote token"),
            "expected quote-token collision error, got: {err}"
        );
    }

    #[test]
    fn rejects_empty_pricing_ws_url() {
        let text = r#"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "COIN"

            [pricing]
            ws_url = ""
            consumer = "oracle"
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_empty_consumer() {
        let text = r#"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "COIN"

            [pricing]
            ws_url = "ws://st0x-pricing:8080/ws"
            consumer = ""
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(cfg.validate().is_err());
    }
}
