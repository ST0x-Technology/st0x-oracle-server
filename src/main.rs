use clap::Parser;
use st0x_oracle_server::alpaca::AlpacaClient;
use st0x_oracle_server::cache::{poll_once, spawn_poll_loop, QuoteCache};
use st0x_oracle_server::config::Config;
use st0x_oracle_server::registry::TokenRegistry;
use st0x_oracle_server::sign::Signer;
use st0x_oracle_server::{create_app, AppState};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

/// USDC on Base. Chain invariant; doesn't move with the token registry.
const USDC_BASE: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";

#[derive(Parser)]
#[command(name = "st0x-oracle-server")]
#[command(about = "Signed context oracle server for st0x tokenized equities")]
struct Cli {
    /// Path to config.toml. Contains port, poll interval and token
    /// registry — i.e. everything except secrets.
    #[arg(long, default_value = "config.toml", env = "CONFIG_PATH")]
    config: PathBuf,

    /// Private key for EIP-191 signing (hex, with or without 0x prefix)
    #[arg(long, env = "SIGNER_PRIVATE_KEY")]
    signer_private_key: String,

    /// Alpaca API key ID (read-only)
    #[arg(long, env = "ALPACA_API_KEY_ID")]
    alpaca_api_key_id: String,

    /// Alpaca API secret key
    #[arg(long, env = "ALPACA_API_SECRET_KEY")]
    alpaca_api_secret_key: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();

    let config = Config::load(&cli.config)?;
    tracing::info!(
        config = %cli.config.display(),
        port = config.port,
        poll_interval_secs = config.poll_interval_secs,
        token_count = config.tokens.len(),
        "Loaded config"
    );

    let signer = Signer::new(&cli.signer_private_key)?;
    let alpaca = AlpacaClient::new(&cli.alpaca_api_key_id, &cli.alpaca_api_secret_key);

    let registry = TokenRegistry::from_config(&config.tokens, USDC_BASE)?;

    tracing::info!("Signer address: {}", signer.address());
    tracing::info!(
        "Registered {} token(s): {}",
        config.tokens.len(),
        config
            .tokens
            .iter()
            .map(|t| format!("{}={}", t.symbol, t.address))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Prime the cache synchronously before opening the socket so the
    // first /context/v1 request doesn't race the poll loop.
    let cache = Arc::new(QuoteCache::new());
    let symbols = config.symbols();
    tracing::info!("Priming quote cache with {} symbols...", symbols.len());
    poll_once(&cache, &alpaca, &symbols).await;

    // Start background poller.
    spawn_poll_loop(
        cache.clone(),
        alpaca.clone(),
        symbols,
        Duration::from_secs(config.poll_interval_secs),
    );

    let state = AppState::new(signer, registry, cache);
    let app = create_app(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("Listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
