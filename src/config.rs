use alloy::primitives::Address;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    pub tokens: Vec<TokenEntry>,
    pub pricing: PricingConfig,
    #[serde(default)]
    pub signing: SigningConfig,
}

/// Default margin for cross-frame signature reuse (see `reuse`): a
/// previous v5/v6 quote is served again only while it still has at least
/// this many seconds before its expiry. Pricing stamps expiry 20 to 30s
/// after the frame, so 10s leaves a taker a real settlement window.
pub const DEFAULT_REUSE_MIN_REMAINING_SECS: u64 = 10;

/// Signing economics. Optional `[signing]` table in the TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct SigningConfig {
    /// Seconds a previous v5/v6 quote must still have before its expiry to
    /// be reused instead of signing an unchanged price under a new
    /// publish_time. 0 disables reuse (every new frame is signed).
    #[serde(default = "default_reuse_min_remaining_secs")]
    pub reuse_min_remaining_secs: u64,
}

impl Default for SigningConfig {
    fn default() -> Self {
        Self {
            reuse_min_remaining_secs: DEFAULT_REUSE_MIN_REMAINING_SECS,
        }
    }
}

fn default_reuse_min_remaining_secs() -> u64 {
    DEFAULT_REUSE_MIN_REMAINING_SECS
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

fn default_port() -> u16 {
    3000
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
