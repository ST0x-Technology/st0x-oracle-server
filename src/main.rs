use clap::Parser;
use st0x_oracle_server::alpaca::AlpacaClient;
use st0x_oracle_server::cache::{poll_once, spawn_poll_loop, QuoteCache};
use st0x_oracle_server::config::Config;
use st0x_oracle_server::market_hours::{
    refresh_once, spawn_market_hours_refresh, MarketHoursCache,
};
use st0x_oracle_server::metrics::MetricsHandle;
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

    /// Alpaca Broker API key (used as HTTP Basic auth username).
    /// We read reference prices from the issuer's brokerage positions,
    /// so these are Broker API creds, not Market Data creds.
    #[arg(long, env = "ALPACA_API_KEY_ID")]
    alpaca_api_key_id: String,

    /// Alpaca Broker API secret (HTTP Basic auth password).
    #[arg(long, env = "ALPACA_API_SECRET_KEY")]
    alpaca_api_secret_key: String,

    /// Alpaca brokerage account ID whose positions back the oracle.
    /// Must be the issuer's account that holds every symbol listed in
    /// config.toml — startup will fail loud if any registered symbol
    /// has no current position.
    #[arg(long, env = "ALPACA_BROKER_ACCOUNT_ID")]
    alpaca_broker_account_id: String,
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
        poll_interval_secs = config.poll_interval_secs,
        token_count = config.tokens.len(),
        "Loaded config"
    );

    let signer = Signer::new(&cli.signer_private_key)?;
    let alpaca = AlpacaClient::new(
        &cli.alpaca_api_key_id,
        &cli.alpaca_api_secret_key,
        &cli.alpaca_broker_account_id,
    );

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
    // first /context/v1 request doesn't race the poll loop. Missing
    // symbols are logged loudly but no longer fatal: the server starts
    // in a partial-serving state where healthy symbols quote normally
    // and missing symbols return 503 at request time. /status exposes
    // the missing set so monitoring can pick up the partial state. We
    // chose this over the old hard-bail because the bail took the whole
    // oracle down on the next Fly restart whenever any single position
    // went to 0 — and the "alert" was the outage itself.
    let cache = Arc::new(QuoteCache::new());
    let symbols = config.symbols();
    tracing::info!("Priming quote cache with {} symbols...", symbols.len());
    poll_once(&cache, &alpaca, &symbols).await;
    let missing = cache.missing(&symbols).await;
    if missing.is_empty() {
        tracing::info!("Cache primed for all {} symbols", symbols.len());
    } else {
        for sym in &missing {
            tracing::error!(
                symbol = %sym,
                "No broker position for configured symbol after initial poll. \
                 /context/v1 will return 503 for requests resolving to this symbol \
                 until the issuer acquires inventory or the symbol is removed from \
                 config.toml. See /status for the current missing-symbol set."
            );
        }
        tracing::warn!(
            missing_count = missing.len(),
            primed_count = symbols.len() - missing.len(),
            total = symbols.len(),
            "Starting in degraded mode. Healthy symbols quote normally; missing \
             symbols return 503 at request time and appear in /status."
        );
    }

    // Start background poller.
    spawn_poll_loop(
        cache.clone(),
        alpaca.clone(),
        symbols.clone(),
        Duration::from_secs(config.poll_interval_secs),
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

    let state = AppState::new(signer, registry, cache, symbols, market_hours, metrics);
    let app = create_app(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("Listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
