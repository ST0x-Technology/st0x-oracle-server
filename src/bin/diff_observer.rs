//! `st0x-oracle-diff-observer` — parity-window side-by-side probe of
//! the legacy Fly oracle and the new DigitalOcean oracle (RAI-361).
//!
//! Runs as its own systemd unit alongside `st0x-oracle-server` on the
//! DO droplet. Probes every `(symbol, direction)` pair against both
//! URLs in lockstep, decodes the v1 signed contexts, computes
//! per-direction drift in basis points, and surfaces everything as
//! Prometheus metrics on its own `/metrics` endpoint (`port` in the
//! observer's TOML). The obs droplet's scrape job picks it up
//! alongside the oracle itself.

use alloy::primitives::Address;
use axum::{extract::State, response::IntoResponse, routing::get, Router};
use clap::Parser;
use reqwest::Client;
use st0x_oracle_server::diff_observer::{
    drift_basis_points, encode_probe_body, extract_v1_price_and_time, ObserverConfig,
    ProbeDirection, ProbeOutcome, SymbolEntry,
};
use st0x_oracle_server::metrics::MetricsHandle;
use st0x_oracle_server::oracle::OracleResponse;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "st0x-oracle-diff-observer")]
#[command(about = "Side-by-side parity observer for the Fly vs DO oracle migration")]
struct Cli {
    #[arg(long, default_value = "config.toml", env = "CONFIG_PATH")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();
    let cfg = ObserverConfig::load(&cli.config)?;
    let metrics = MetricsHandle::install()?;

    tracing::info!(
        config = %cli.config.display(),
        port = cfg.port,
        probe_interval_secs = cfg.probe_interval_secs,
        symbol_count = cfg.symbols.len(),
        fly = %cfg.fly_oracle_base_url,
        do_url = %cfg.do_oracle_base_url,
        "Loaded observer config"
    );

    let quote = Address::from_str(&cfg.quote_token)?;
    let symbols: Vec<(String, Address)> = cfg
        .symbols
        .iter()
        .map(|s: &SymbolEntry| -> anyhow::Result<(String, Address)> {
            Ok((s.symbol.clone(), Address::from_str(&s.base_token)?))
        })
        .collect::<Result<_, _>>()?;

    let http = Client::builder().timeout(Duration::from_secs(10)).build()?;
    let app_state = Arc::new(AppState {
        metrics: metrics.clone(),
    });

    let probe_state = ProbeState {
        http: http.clone(),
        fly: cfg.fly_oracle_base_url.clone(),
        do_url: cfg.do_oracle_base_url.clone(),
        quote,
        symbols: symbols.clone(),
    };

    tokio::spawn(async move {
        let interval = Duration::from_secs(cfg.probe_interval_secs);
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            run_round(&probe_state).await;
        }
    });

    let app = Router::new()
        .route("/metrics", get(serve_metrics))
        .route("/", get(|| async { "ok" }))
        .with_state(app_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], cfg.port));
    tracing::info!(%addr, "Diff observer listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

struct AppState {
    metrics: MetricsHandle,
}

async fn serve_metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.metrics.render()
}

struct ProbeState {
    http: Client,
    fly: String,
    do_url: String,
    quote: Address,
    symbols: Vec<(String, Address)>,
}

async fn run_round(state: &ProbeState) {
    for (symbol, base) in &state.symbols {
        probe_pair(state, symbol, *base, ProbeDirection::QuoteToBase).await;
        probe_pair(state, symbol, *base, ProbeDirection::BaseToQuote).await;
    }
}

async fn probe_pair(state: &ProbeState, symbol: &str, base: Address, direction: ProbeDirection) {
    let (input, output) = match direction {
        ProbeDirection::QuoteToBase => (state.quote, base),
        ProbeDirection::BaseToQuote => (base, state.quote),
    };
    let body = encode_probe_body(input, output);

    let fly_result = fetch_price(&state.http, &state.fly, &body).await;
    let do_result = fetch_price(&state.http, &state.do_url, &body).await;

    let outcome = build_outcome(symbol, direction, fly_result, do_result);
    emit_metrics(&outcome);
    log_outcome(&outcome);
}

fn build_outcome(
    symbol: &str,
    direction: ProbeDirection,
    fly: anyhow::Result<(f64, u64)>,
    do_side: anyhow::Result<(f64, u64)>,
) -> ProbeOutcome {
    let (fly_price, fly_publish) = match &fly {
        Ok((p, t)) => (Some(*p), Some(*t)),
        Err(_) => (None, None),
    };
    let (do_price, do_publish) = match &do_side {
        Ok((p, t)) => (Some(*p), Some(*t)),
        Err(_) => (None, None),
    };
    let basis_points = match (&fly_price, &do_price) {
        (Some(f), Some(d)) => drift_basis_points(*f, *d),
        _ => None,
    };
    let publish_time_diff_secs = match (&fly_publish, &do_publish) {
        (Some(f), Some(d)) => Some(*d as i64 - *f as i64),
        _ => None,
    };
    ProbeOutcome {
        symbol: symbol.to_string(),
        direction,
        fly_price,
        do_price,
        basis_points,
        fly_publish_time_secs: fly_publish,
        do_publish_time_secs: do_publish,
        publish_time_diff_secs,
    }
}

fn emit_metrics(outcome: &ProbeOutcome) {
    let labels = [
        ("symbol", outcome.symbol.clone()),
        ("direction", outcome.direction.as_str().to_string()),
    ];
    let outcome_label = match (outcome.fly_price, outcome.do_price) {
        (Some(_), Some(_)) => "ok",
        (None, Some(_)) => "fly_missing",
        (Some(_), None) => "do_missing",
        (None, None) => "both_missing",
    };
    metrics::counter!(
        "oracle_diff_probe_total",
        "symbol" => outcome.symbol.clone(),
        "direction" => outcome.direction.as_str().to_string(),
        "outcome" => outcome_label.to_string(),
    )
    .increment(1);
    if let Some(bps) = outcome.basis_points {
        metrics::gauge!("oracle_diff_basis_points", &labels).set(bps);
    }
    if let Some(diff) = outcome.publish_time_diff_secs {
        metrics::gauge!("oracle_diff_publish_time_seconds", &labels).set(diff as f64);
    }
}

fn log_outcome(outcome: &ProbeOutcome) {
    match (outcome.fly_price, outcome.do_price, outcome.basis_points) {
        (Some(f), Some(d), Some(bps)) => tracing::info!(
            symbol = %outcome.symbol,
            direction = outcome.direction.as_str(),
            fly_price = f,
            do_price = d,
            basis_points = bps,
            publish_time_diff_secs = outcome.publish_time_diff_secs.unwrap_or(0),
            "Probe ok"
        ),
        _ => tracing::warn!(
            symbol = %outcome.symbol,
            direction = outcome.direction.as_str(),
            fly_price = ?outcome.fly_price,
            do_price = ?outcome.do_price,
            "Probe missing a side; outcome counter only"
        ),
    }
}

async fn fetch_price(client: &Client, base_url: &str, body: &[u8]) -> anyhow::Result<(f64, u64)> {
    let url = format!("{}/context/v1", base_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .header("content-type", "application/octet-stream")
        .body(body.to_vec())
        .send()
        .await?
        .error_for_status()?;
    let parsed: Vec<OracleResponse> = resp.json().await?;
    // Some legacy Fly responses might serve a length-mismatched array
    // under failure; treat anything but length-1 as a probe miss.
    if parsed.len() != 1 {
        anyhow::bail!("expected 1-element response, got {}", parsed.len());
    }
    extract_v1_price_and_time(&parsed[0])
}
