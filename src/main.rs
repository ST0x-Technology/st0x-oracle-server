use clap::Parser;
use st0x_oracle_server::alpaca::AlpacaClient;
use st0x_oracle_server::config::Config;
use st0x_oracle_server::market_hours::{
    refresh_once, spawn_market_hours_refresh, MarketHoursCache,
};
use st0x_oracle_server::metrics::MetricsHandle;
use st0x_oracle_server::pricing_client::{LiveClient, LiveClientConfig};
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
    /// Path to config.toml. Contains port, pricing connection, and the
    /// token registry — everything except secrets.
    #[arg(long, default_value = "config.toml", env = "CONFIG_PATH")]
    config: PathBuf,

    /// Private key for EIP-191 signing (hex, with or without 0x prefix).
    #[arg(long, env = "SIGNER_PRIVATE_KEY")]
    signer_private_key: String,

    /// API key for the st0x.pricing WebSocket. Format
    /// `pricing_<consumer>_<32 hex>`; consumer name must match the
    /// `[pricing].consumer` value in config.toml.
    #[arg(long, env = "PRICING_API_KEY")]
    pricing_api_key: String,

    /// Alpaca Broker API key id. Used only for the trading calendar
    /// endpoint — the oracle no longer polls Alpaca for reference
    /// prices (live quotes come from st0x.pricing).
    #[arg(long, env = "ALPACA_API_KEY_ID")]
    alpaca_api_key_id: String,

    /// Alpaca Broker API secret.
    #[arg(long, env = "ALPACA_API_SECRET_KEY")]
    alpaca_api_secret_key: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();

    // Install the Prometheus recorder before anything else records a
    // metric — `metrics::counter!` / `gauge!` against the global facade
    // would otherwise no-op. Matches the bebop / pricing pattern.
    let metrics = MetricsHandle::install()?;

    let config = Config::load(&cli.config)?;
    tracing::info!(
        config = %cli.config.display(),
        port = config.port,
        pricing_ws_url = %config.pricing.ws_url,
        pricing_consumer = %config.pricing.consumer,
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

    // Spawn the pricing WS subscriber. Connect / subscribe / cache is
    // entirely background; we open the HTTP socket immediately and let
    // the first /context/v1 request either find a warm quote or return
    // 503 with a clear "no live quote yet" detail. The reconnect loop
    // owns retry logic, so we don't gate startup on a successful
    // connect — that would block boot on a transient pricing-service
    // outage.
    let symbols = config.symbols();
    let pricing = LiveClient::spawn(LiveClientConfig::new(
        config.pricing.ws_url.clone(),
        cli.pricing_api_key.clone(),
        config.pricing.consumer.clone(),
        symbols.clone(),
    ));
    tracing::info!(
        symbol_count = symbols.len(),
        "Spawned pricing WS subscriber (live quotes warm asynchronously)"
    );

    // Prime market hours (Alpaca trading calendar). Failure here isn't
    // fatal — `MarketHoursCache::publish_time_for` falls back to `now`
    // when empty, which is the pre-RAI-693 behaviour. The hourly refresh
    // task will keep trying.
    let market_hours = Arc::new(MarketHoursCache::new());
    match refresh_once(&market_hours, &alpaca).await {
        Ok(()) => tracing::info!(
            window_count = market_hours.window_count().await,
            "Primed market hours from Alpaca calendar"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "Initial market hours fetch failed; publish_time will use `now` until refresh succeeds"
        ),
    }
    spawn_market_hours_refresh(
        market_hours.clone(),
        alpaca.clone(),
        Duration::from_secs(3600),
    );

    let state = AppState::new(signer, registry, pricing, symbols, market_hours, metrics);
    let app = create_app(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("Listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
