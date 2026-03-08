use clap::Parser;
use st0x_oracle_server::alpaca::AlpacaClient;
use st0x_oracle_server::registry::TokenRegistry;
use st0x_oracle_server::sign::Signer;
use st0x_oracle_server::{create_app, AppState};
use std::net::SocketAddr;
use tracing_subscriber::EnvFilter;

/// USDC on Base
const USDC_BASE: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";

#[derive(Parser)]
#[command(name = "st0x-oracle-server")]
#[command(about = "Signed context oracle server for st0x tokenized equities")]
struct Cli {
    /// Port to listen on
    #[arg(short, long, default_value = "3000", env = "PORT")]
    port: u16,

    /// Private key for EIP-191 signing (hex, with or without 0x prefix)
    #[arg(long, env = "SIGNER_PRIVATE_KEY")]
    signer_private_key: String,

    /// Alpaca API key ID (read-only)
    #[arg(long, env = "ALPACA_API_KEY_ID")]
    alpaca_api_key_id: String,

    /// Alpaca API secret key
    #[arg(long, env = "ALPACA_API_SECRET_KEY")]
    alpaca_api_secret_key: String,

    /// Signed context expiry in seconds
    #[arg(long, default_value = "5", env = "EXPIRY_SECONDS")]
    expiry_seconds: u64,

    /// Token registry entries: "TOKEN_ADDRESS=SYMBOL" (repeatable)
    /// e.g. --token 0xabc...=COIN --token 0xdef...=RKLB
    #[arg(long = "token", env = "TOKEN_REGISTRY", value_delimiter = ',')]
    tokens: Vec<String>,
}

fn parse_token_entries(entries: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    entries
        .iter()
        .map(|entry| {
            let parts: Vec<&str> = entry.splitn(2, '=').collect();
            if parts.len() != 2 {
                anyhow::bail!(
                    "Invalid token entry '{}'. Expected format: TOKEN_ADDRESS=SYMBOL",
                    entry
                );
            }
            Ok((parts[0].to_string(), parts[1].to_string()))
        })
        .collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();

    let signer = Signer::new(&cli.signer_private_key)?;
    let alpaca = AlpacaClient::new(&cli.alpaca_api_key_id, &cli.alpaca_api_secret_key);

    let token_entries = parse_token_entries(&cli.tokens)?;
    if token_entries.is_empty() {
        tracing::warn!("No tokens registered. Use --token ADDRESS=SYMBOL to add tokens.");
    }

    let registry = TokenRegistry::new(token_entries.clone(), USDC_BASE)?;

    tracing::info!("Signer address: {}", signer.address());
    tracing::info!(
        "Registered {} token(s): {}",
        token_entries.len(),
        token_entries
            .iter()
            .map(|(a, s)| format!("{}={}", s, a))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let state = AppState::new(signer, alpaca, registry, cli.expiry_seconds);
    let app = create_app(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    tracing::info!("Listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
