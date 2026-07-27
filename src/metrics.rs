//! Prometheus metrics surface.
//!
//! `metrics` (facade) + `metrics-exporter-prometheus` (recorder),
//! matching the bebop / pricing-service pattern. Installed once at
//! startup and surfaced as `MetricsHandle`. `/metrics` route lives
//! in `lib.rs`; the obs droplet scrapes it over the tailnet.
//!
//! Naming follows the `oracle_*` prefix so dashboards can join
//! metrics across services without collisions.
//!
//! Minimal initial set — PR 2 (pricing-client integration) will
//! add pricing-link gauges; the obs dashboard PR consumes whatever
//! is declared here. Keep new metric names registered in `declare`
//! so they show up in `/metrics` output even before the first
//! sample is recorded.

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::{Mutex, OnceLock};

// The `metrics` facade's recorder is process-global; calling
// `install_recorder` twice fails with "global recorder already set".
// Cache the handle so repeat calls (test suite builds many AppStates
// in parallel, multi-instance e2e binaries, etc.) return the existing
// recorder instead of erroring. The Mutex serialises the install
// attempt — without it, two threads can both pass the OnceLock check,
// race into install_recorder, and the second one fails.
static INSTALLED: OnceLock<PrometheusHandle> = OnceLock::new();
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone)]
pub struct MetricsHandle {
    inner: PrometheusHandle,
}

impl MetricsHandle {
    pub fn install() -> anyhow::Result<Self> {
        if let Some(existing) = INSTALLED.get() {
            return Ok(Self {
                inner: existing.clone(),
            });
        }
        let _guard = INSTALL_LOCK.lock().expect("metrics install lock poisoned");
        // Double-checked: another thread may have installed between
        // the `INSTALLED.get()` above and acquiring the lock.
        if let Some(existing) = INSTALLED.get() {
            return Ok(Self {
                inner: existing.clone(),
            });
        }
        let inner = PrometheusBuilder::new()
            .install_recorder()
            .map_err(|e| anyhow::anyhow!("Failed to install Prometheus recorder: {e}"))?;
        Self::declare();
        let _ = INSTALLED.set(inner.clone());
        Ok(Self { inner })
    }

    fn declare() {
        metrics::describe_counter!(
            "oracle_context_request_total",
            "Signed-context requests received, labelled by endpoint (v1 / v2) and outcome (signed / rejected)"
        );
        metrics::describe_counter!(
            "oracle_upstream_failure_total",
            "Upstream errors fetching reference prices (Alpaca polling today; pricing-service WS after PR 2)"
        );
        metrics::describe_gauge!(
            "oracle_cache_freshness_seconds",
            "Seconds since the newest quote in the cache was refreshed; alerts on this catch a wedged poller before stale prices reach the chain"
        );
        metrics::describe_gauge!(
            "oracle_configured_symbols",
            "Number of symbols declared in config.toml — joined with oracle_missing_symbols on the dashboard for a coverage view"
        );
        metrics::describe_gauge!(
            "oracle_missing_symbols",
            "Configured symbols that have never been cached (broker positions absent at startup, or wiped mid-run)"
        );
    }

    pub fn render(&self) -> String {
        self.inner.render()
    }
}
