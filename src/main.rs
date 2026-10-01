use clap::Parser;
use st0x_oracle_server::alpaca::AlpacaClient;
use st0x_oracle_server::config::Config;
use st0x_oracle_server::market_hours::{
    refresh_once, spawn_market_hours_refresh, MarketHoursCache,
};
use st0x_oracle_server::metrics::MetricsHandle;
use st0x_oracle_server::pricing_client::{LiveClient, LiveClientConfig};
use st0x_oracle_server::reload::{record_applied, Reloader};
use st0x_oracle_server::sign::Signer;
use st0x_oracle_server::token_file;
use st0x_oracle_server::tokens::Tokens;
use st0x_oracle_server::{create_app, AppState};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "st0x-oracle-server")]
#[command(about = "Signed context oracle server for st0x tokenized equities.\n\
    Run `st0x-oracle-server validate [path] [--registry-file PATH]` to check a config file and exit.")]
struct Cli {
    /// Path to config.toml. Contains port, pricing connection, and the
    /// `[registry]` bucket the tokens come from — everything except
    /// secrets. Outside GCP, set GCS_ACCESS_TOKEN (e.g. `gcloud auth
    /// print-access-token`) to read the bucket.
    #[arg(long, default_value = "config.toml", env = "CONFIG_PATH")]
    config: PathBuf,

    /// Private key for EIP-191 signing (hex, with or without 0x prefix).
    /// Local dev / tests only — production uses --signer-kms-key. Exactly
    /// one of the two signer sources must be set (validated after parsing
    /// so that empty env vars — e.g. from compose templating — count as
    /// unset instead of tripping clap-level conflicts).
    #[arg(long, env = "SIGNER_PRIVATE_KEY", hide_env_values = true)]
    signer_private_key: Option<String>,

    /// GCP Cloud KMS key VERSION resource name for EIP-191 signing
    /// (projects/…/locations/…/keyRings/…/cryptoKeys/…/cryptoKeyVersions/N).
    /// The key never leaves KMS; each signature-cache miss is one
    /// AsymmetricSign call authenticated via ADC (native on GCP runtimes such as Cloud Run;
    /// elsewhere provide GOOGLE_APPLICATION_CREDENTIALS).
    #[arg(long, env = "SIGNER_KMS_KEY")]
    signer_kms_key: Option<String>,

    /// API key for the st0x.pricing WebSocket. Format
    /// `pricing_<consumer>_<32 hex>`; consumer name must match the
    /// `[pricing].consumer` value in config.toml. Unused when
    /// `--pricing-iam-auth` is set (Cloud Run IAM replaces it).
    #[arg(long, env = "PRICING_API_KEY")]
    pricing_api_key: String,

    /// Override the pricing WS URL from config. Set per-env (the image is
    /// built once and promoted staging->prod, which point at different
    /// pricing services), e.g. `wss://st0x-pricing-….run.app/ws`.
    #[arg(long, env = "PRICING_WS_URL")]
    pricing_ws_url: Option<String>,

    /// Authenticate to pricing with a Google ID token (Cloud Run IAM) instead
    /// of the API key. Set true where pricing is a private Cloud Run service.
    #[arg(long, env = "PRICING_IAM_AUTH", action = clap::ArgAction::Set, default_value_t = false)]
    pricing_iam_auth: bool,

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
    // `st0x-oracle-server validate [path]` — parse + validate a config file
    // with the rules boot uses and exit. Dispatched before clap so
    // the serve-only required flags (pricing/Alpaca creds) are not needed:
    // CI validates candidate configs by running the shipped image with no
    // env at all. Path resolution mirrors --config: positional arg, then
    // CONFIG_PATH, then ./config.toml.
    let mut argv = std::env::args();
    if argv.nth(1).as_deref() == Some("validate") {
        let (path, registry_file) = validate_args(argv)?;
        let path = path
            .or_else(|| std::env::var("CONFIG_PATH").ok())
            .unwrap_or_else(|| "config.toml".to_string());
        return validate(&path, registry_file.as_deref());
    }

    // Per-request lines are TRACE, so the default keeps this crate's DEBUG
    // lines and drops TRACE before it leaves the process. Dependencies log
    // at WARN and above only.
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,st0x_oracle_server=debug".into()),
        )
        .with(tracing_stackdriver::layer().with_source_location(false))
        .init();

    let cli = Cli::parse();

    // Install the Prometheus recorder before anything else records a
    // metric — `metrics::counter!` / `gauge!` against the global facade
    // would otherwise no-op. Matches the bebop / pricing pattern.
    let metrics = MetricsHandle::install()?;

    // The first thing the server does is read the latest token file from
    // the bucket and check it. Nothing listens until that succeeds: an
    // instance that cannot read the live copy, or reads one that fails
    // validation, exits instead of serving an old or unchecked token set.
    let static_table = Config::parse_table(&cli.config)?;
    let source = serve_source(&static_table)?;
    let bucket = token_file::Bucket::from_env();
    let (config, token_set) =
        token_file::boot(&static_table, &source, &bucket, token_file::Retry::BOOT).await?;
    tracing::info!(
        url = %source.url,
        generation = ?token_set.generation,
        tokens = token_set.symbols.len(),
        "tokens loaded from the latest token file"
    );
    record_applied(&token_set);
    metrics::gauge!("oracle_registry_last_check_timestamp_seconds")
        .set(chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
    metrics::gauge!("oracle_registry_invalid").set(0.0);
    tracing::info!(
        config = %cli.config.display(),
        port = config.port,
        pricing_ws_url = %config.pricing.ws_url,
        pricing_consumer = %config.pricing.consumer,
        chain_id = config.chain_id,
        quote_token = %config.quote_token,
        token_count = config.tokens.len(),
        signature_reuse_min_remaining_secs = config.signing.reuse_min_remaining_secs,
        "Loaded config"
    );

    // Exactly one signer source, validated here rather than via clap
    // conflicts: empty/whitespace env values (compose/CI templating of unset
    // vars) are treated as absent, and ambiguous config fails loud with a
    // message naming both options — no silent precedence for a signer that
    // guards real funds.
    let kms_key = cli
        .signer_kms_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let private_key = cli
        .signer_private_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let signer = match (kms_key, private_key) {
        (Some(kms_key), None) => {
            tracing::info!(key = %kms_key, "Using GCP Cloud KMS signer");
            Signer::from_gcp_kms(kms_key).await?
        }
        (None, Some(private_key)) => {
            tracing::warn!("Using local private key signer — production must use SIGNER_KMS_KEY");
            Signer::new(private_key)?
        }
        (Some(_), Some(_)) => anyhow::bail!(
            "Both SIGNER_KMS_KEY and SIGNER_PRIVATE_KEY are set — set exactly one \
             (SIGNER_KMS_KEY for production, SIGNER_PRIVATE_KEY for local dev)"
        ),
        (None, None) => anyhow::bail!(
            "No signer configured — set exactly one of SIGNER_KMS_KEY (production, \
             GCP Cloud KMS) or SIGNER_PRIVATE_KEY (local dev)"
        ),
    };
    let alpaca = AlpacaClient::new(&cli.alpaca_api_key_id, &cli.alpaca_api_secret_key);

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
    let symbols = token_set.symbols.clone();
    let (assets_tx, assets_rx) = tokio::sync::watch::channel(symbols.clone());
    let pricing_ws_url = cli
        .pricing_ws_url
        .clone()
        .unwrap_or_else(|| config.pricing.ws_url.clone());
    let pricing = LiveClient::spawn(
        LiveClientConfig::new(
            pricing_ws_url,
            cli.pricing_api_key.clone(),
            config.pricing.consumer.clone(),
            symbols.clone(),
            config.chain_id,
        )
        .with_assets(assets_rx)
        .with_iam_auth(cli.pricing_iam_auth),
    );
    tracing::info!(
        symbol_count = symbols.len(),
        "Spawned pricing WS subscriber (live quotes warm asynchronously)"
    );

    // Prime market hours (Alpaca trading calendar). Used only to classify
    // the session for the v2/v3/v4 session slots — `publish_time` comes
    // from the pricing quote's `source_ts`, so a failure here just means
    // sessions classify as closed until the hourly refresh succeeds.
    let market_hours = Arc::new(MarketHoursCache::new());
    match refresh_once(&market_hours, &alpaca).await {
        Ok(()) => tracing::info!(
            window_count = market_hours.window_count().await,
            "Primed market hours from Alpaca calendar"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "Initial market hours fetch failed; session slots classify as closed until refresh succeeds"
        ),
    }
    spawn_market_hours_refresh(
        market_hours.clone(),
        alpaca.clone(),
        Duration::from_secs(3600),
    );

    let tokens = Tokens::new(token_set);
    Reloader::new(tokens.clone(), assets_tx, pricing.clone()).spawn(
        static_table,
        config.clone(),
        source,
        bucket,
    );
    let state = AppState::with_tokens(
        signer,
        tokens,
        config.chain_id,
        pricing,
        market_hours,
        metrics,
    )
    .with_signature_reuse(config.signing.reuse_min_remaining_secs);
    let app = create_app(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("Listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// The `[registry]` the server reads its tokens from. `serve` takes its
/// tokens only from the live token file, never from rows in the config.
fn serve_source(table: &toml::Table) -> anyhow::Result<token_file::RegistrySource> {
    token_file::source_of(table)?.ok_or_else(|| {
        anyhow::anyhow!(
            "the server reads its tokens only from the live token file; add [registry] \
             (inline [[tokens]] are for `validate` and tests)"
        )
    })
}

/// The arguments after `validate`: one optional config path and an
/// optional `--registry-file PATH`. An unknown flag or a second path is
/// refused, so a misspelled flag cannot pass as a static-only check.
fn validate_args(
    mut args: impl Iterator<Item = String>,
) -> anyhow::Result<(Option<String>, Option<PathBuf>)> {
    let mut path = None;
    let mut registry_file = None;
    while let Some(arg) = args.next() {
        if arg == "--registry-file" {
            let file = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("--registry-file needs a path"))?;
            if registry_file.replace(PathBuf::from(file)).is_some() {
                anyhow::bail!("validate takes one --registry-file");
            }
        } else if arg.starts_with("--") {
            anyhow::bail!("validate: unknown flag {arg}");
        } else if path.replace(arg).is_some() {
            anyhow::bail!("validate takes one config path");
        }
    }
    Ok((path, registry_file))
}

/// `validate [path] [--registry-file PATH]`. A config that reads its tokens
/// from the bucket is checked in full only with a copy of the token file;
/// without one, everything else is checked and the output says so.
fn validate(path: &str, registry_file: Option<&std::path::Path>) -> anyhow::Result<()> {
    let table = Config::parse_table(std::path::Path::new(path))?;
    let config = match (token_file::source_of(&table)?, registry_file) {
        (None, _) => Config::from_table(table)?,
        (Some(_), Some(file)) => {
            let bytes = std::fs::read(file).map_err(|e| {
                anyhow::anyhow!("reading the token file at {}: {e}", file.display())
            })?;
            let (_, config, _) = token_file::candidate(&table, &bytes)?;
            println!("registry: tokens taken from {}", file.display());
            config
        }
        (Some(source), None) => {
            let config = Config::from_table_static(table)?;
            println!(
                "registry: tokens NOT validated here. They come from {} at boot; \
                 pass --registry-file to check them.",
                source.url
            );
            config
        }
    };
    // The chain id is in the output because /context/v7 signs it: a config
    // that inherits the 8453 default on a non-Base plane is a signing fault
    // the reviewer of a config PR can now see.
    let tokens = if config.tokens.is_empty() {
        "static part".to_string()
    } else {
        format!("{} tokens", config.tokens.len())
    };
    println!(
        "{path}: OK ({tokens}, chain {}, quote token {})",
        config.chain_id, config.quote_token
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> anyhow::Result<(Option<String>, Option<PathBuf>)> {
        validate_args(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn serve_refuses_inline_tokens_and_registry_file() {
        let inline: toml::Table = toml::from_str(
            r#"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "wtCOIN"
            "#,
        )
        .unwrap();
        let err = serve_source(&inline).unwrap_err().to_string();
        assert!(err.contains("add [registry]"), "{err}");

        let registry: toml::Table = toml::from_str("[registry]\nurl = \"gs://b/o\"").unwrap();
        assert_eq!(serve_source(&registry).unwrap().url, "gs://b/o");

        let err = Cli::try_parse_from([
            "st0x-oracle-server",
            "--registry-file",
            "t.toml",
            "--pricing-api-key",
            "k",
            "--alpaca-api-key-id",
            "a",
            "--alpaca-api-secret-key",
            "s",
        ])
        .err()
        .expect("serve has no --registry-file");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn validate_args_take_one_path_and_one_registry_file() {
        assert_eq!(args(&[]).unwrap(), (None, None));
        assert_eq!(
            args(&["cfg.toml", "--registry-file", "t.toml"]).unwrap(),
            (Some("cfg.toml".into()), Some(PathBuf::from("t.toml")))
        );
        for (bad, want) in [
            (
                &["--registry-fil", "t.toml", "cfg.toml"][..],
                "unknown flag",
            ),
            (&["a.toml", "b.toml"][..], "one config path"),
            (&["--registry-file"][..], "needs a path"),
            (
                &["--registry-file", "a", "--registry-file", "b"][..],
                "one --registry-file",
            ),
        ] {
            let err = args(bad).unwrap_err().to_string();
            assert!(err.contains(want), "{bad:?}: {err}");
        }
    }
}
