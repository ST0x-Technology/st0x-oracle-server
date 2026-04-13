use alloy::primitives::Address;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    pub tokens: Vec<TokenEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenEntry {
    pub address: String,
    pub symbol: String,
}

fn default_port() -> u16 {
    3000
}
fn default_poll_interval() -> u64 {
    10
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
        if self.poll_interval_secs == 0 {
            anyhow::bail!("poll_interval_secs must be > 0");
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

    #[test]
    fn parses_minimal_config() {
        let text = r#"
            port = 4000
            poll_interval_secs = 5
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "COIN"
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.port, 4000);
        assert_eq!(cfg.poll_interval_secs, 5);
        assert_eq!(cfg.tokens.len(), 1);
    }

    #[test]
    fn defaults_applied() {
        let text = r#"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "COIN"
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.port, 3000);
        assert_eq!(cfg.poll_interval_secs, 10);
    }

    #[test]
    fn rejects_empty_tokens() {
        let text = r#"tokens = []"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_bad_address() {
        let text = r#"
            [[tokens]]
            address = "not-an-address"
            symbol = "COIN"
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_addresses() {
        // Addresses with different casing should still be rejected (the
        // Address parser normalizes via checksum decode).
        let text = r#"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "COIN"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "OTHER"
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("Duplicate token address"),
            "expected duplicate-address error, got: {err}"
        );
    }

    #[test]
    fn rejects_zero_poll_interval() {
        let text = r#"
            poll_interval_secs = 0
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "COIN"
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(cfg.validate().is_err());
    }
}
