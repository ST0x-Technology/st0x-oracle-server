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
//! so their `# HELP` text is attached in `/metrics` output. (The
//! exporter prints a family only once it has a sample; the description
//! is what makes that first sample self-explanatory.)

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
        metrics::describe_gauge!(
            "oracle_registry_tokens",
            "Token rows this instance runs, from the token file generation on oracle_registry_generation."
        );
        metrics::describe_gauge!(
            "oracle_registry_generation",
            "Bucket object generation of the token file this instance runs."
        );
        metrics::describe_gauge!(
            "oracle_registry_invalid",
            "1 while the latest token file in the bucket fails validation; this instance keeps \
             the previous token set, and a new instance cannot start."
        );
        metrics::describe_counter!(
            "oracle_registry_reload_total",
            "Token file checks by result: applied (a new token set is live), unchanged, \
             rejected (the latest copy fails validation, counted on every check while it stays \
             the latest), fetch_error."
        );
        metrics::describe_gauge!(
            "oracle_registry_last_applied_timestamp_seconds",
            "Unix time the running token set was applied (boot or reload)."
        );
        metrics::describe_gauge!(
            "oracle_registry_last_check_timestamp_seconds",
            "Unix time of the last successful read of the token file. Stops advancing while the \
             bucket cannot be read."
        );
        metrics::describe_counter!(
            "oracle_registry_fetch_errors_total",
            "Failed reads of the token file from the bucket."
        );
        metrics::describe_counter!(
            "oracle_context_request_total",
            "Signed-context requests received, labelled by endpoint (v1 / v4 / v5 / v6 / v7) and outcome: \
             ok (every item signed), empty (no items in the body), error (whole request failed — a single \
             tuple, or a batch without allowFailure), partial (allowFailure batch with some failed slots), \
             failed (allowFailure batch with every slot failed)"
        );
        metrics::describe_counter!(
            "oracle_context_item_total",
            "Per-item verdicts that reached the wire, labelled by endpoint and outcome \
             (ok / bad_request / no_live_quote / expired_quote / internal_error). In an allowFailure batch every \
             slot is counted; in strict mode a fully signed batch counts N ok and a failed one counts the \
             single aborting error. Join with oracle_context_request_total for a per-item failure rate."
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
            "Number of symbols in the running token set — joined with oracle_missing_symbols on the dashboard for a coverage view"
        );
        metrics::describe_gauge!(
            "oracle_missing_symbols",
            "Configured symbols that have never been cached (broker positions absent at startup, or wiped mid-run)"
        );
        metrics::describe_counter!(
            "oracle_signature_cache_hits_total",
            "Signed-context requests served from the signature cache (finished or in-flight signature for identical bytes); no KMS call"
        );
        metrics::describe_counter!(
            "oracle_signature_cache_misses_total",
            "Signed-context requests that started a KMS AsymmetricSign; the KMS bill scales with this, not with requests"
        );
        metrics::describe_counter!(
            "oracle_signature_reuse_total",
            "v5/v6/v7 responses answered with the previous frame's signature because the price was unchanged and that quote's expiry was still ahead by the configured margin; each one is a KMS call not made"
        );
        metrics::describe_counter!(
            "oracle_quote_address_mismatch_total",
            "Quotes refused because pricing priced another equity or settlement token address than the \
             request's registry (pricing and the oracle run different token sets)"
        );
        metrics::describe_counter!(
            "oracle_quote_refusals_total",
            "Quote requests refused because no live quote exists or the snapshotted quote expired"
        );
        metrics::describe_gauge!(
            "oracle_signature_cache_entries",
            "Signatures currently held in the content-addressed cache (idle TTL 120s, swept every 256 inserts)"
        );
    }

    pub fn render(&self) -> String {
        self.inner.render()
    }
}
