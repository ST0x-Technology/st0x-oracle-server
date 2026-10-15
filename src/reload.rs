//! Apply a newer token file while the server runs.
//!
//! Every `refresh_secs` the loop reads the bucket object, conditional on
//! the generation this instance runs. A newer copy goes through
//! [`candidate`], the same checks boot runs. If it passes, the new set is
//! swapped in at once and everything derived from it follows: requests
//! resolve against it, the pricing subscription follows its symbols, and
//! the cached quotes of removed or readdressed tokens are dropped and
//! revoked. If it fails, nothing changes; the instance keeps its current
//! set and raises `oracle_registry_invalid`.

use std::time::Duration;

use tokio::sync::watch;
use toml::Table;

use crate::config::Config;
use crate::pricing_client::LiveClient;
use crate::token_file::{self, candidate, Bucket, Read, RegistrySource};
use crate::tokens::{TokenSet, Tokens};

/// What to do with one copy of the token file.
#[derive(Debug)]
pub enum Decision {
    /// The generation this instance already runs.
    Unchanged { generation: i64 },
    /// A valid copy with the rows this instance runs.
    SameRows { generation: i64 },
    /// A valid copy with other rows: swap it in.
    Apply {
        set: Box<TokenSet>,
        /// Symbols to stop quoting.
        removed: Vec<String>,
        /// Symbols kept at another address. Their cached quotes are for the
        /// old token.
        readdressed: Vec<String>,
        /// One line for the log.
        change: String,
    },
    /// A copy boot would refuse.
    Reject { generation: i64, error: String },
}

/// Decide what a copy of the token file means for the running set. Pure:
/// no locks, no I/O, and everything boot checks.
///
/// `static_config` is the config table as parsed from disk and
/// `boot_config` the config boot built from it; a copy may change the
/// tokens and nothing else.
pub fn decide(
    static_config: &Table,
    boot_config: &Config,
    current: &TokenSet,
    generation: i64,
    bytes: &[u8],
) -> Decision {
    if current.generation == Some(generation) {
        return Decision::Unchanged { generation };
    }
    let reject = |error: String| Decision::Reject { generation, error };
    let (projection, config, registry) = match candidate(static_config, bytes) {
        Ok(built) => built,
        Err(e) => return reject(format!("{e:#}")),
    };
    // `candidate` merges only token rows, so the two checks below are
    // backstops against a future merge change, not the defense itself.
    if let Some(field) = static_field_changed(boot_config, &config) {
        return reject(format!(
            "the token file would change {field}, which only a restart may change"
        ));
    }
    let Some(running) = &current.projection else {
        return reject("this instance does not run a token file".to_string());
    };
    if projection.same_as(running) {
        return Decision::SameRows { generation };
    }
    let change = token_file::change(running, &projection);
    let described = change.to_string();
    Decision::Apply {
        set: Box::new(TokenSet {
            registry,
            symbols: config.symbols(),
            projection: Some(projection),
            generation: Some(generation),
        }),
        removed: change.removed,
        readdressed: change.readdressed,
        change: described,
    }
}

/// The first non-token field that differs between two configs.
fn static_field_changed(a: &Config, b: &Config) -> Option<&'static str> {
    if a.chain_id != b.chain_id {
        Some("chain_id")
    } else if !a.quote_token.eq_ignore_ascii_case(&b.quote_token) {
        Some("quote_token")
    } else if a.port != b.port {
        Some("port")
    } else if a.pricing.ws_url != b.pricing.ws_url || a.pricing.consumer != b.pricing.consumer {
        Some("pricing")
    } else if a.signing.reuse_min_remaining_secs != b.signing.reuse_min_remaining_secs {
        Some("signing")
    } else {
        None
    }
}

fn now_secs() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

fn count(result: &'static str) {
    metrics::counter!("oracle_registry_reload_total", "result" => result).increment(1);
}

/// Record the set this instance runs on the registry gauges.
pub fn record_applied(set: &TokenSet) {
    if let Some(generation) = set.generation {
        // GCS generations are about 1.8e15, below 2^53: exact in an f64.
        metrics::gauge!("oracle_registry_generation").set(generation as f64);
    }
    metrics::gauge!("oracle_registry_tokens").set(set.symbols.len() as f64);
    metrics::gauge!("oracle_configured_symbols").set(set.symbols.len() as f64);
    metrics::gauge!("oracle_registry_last_applied_timestamp_seconds").set(now_secs());
}

/// Applies decisions to the running server.
pub struct Reloader {
    tokens: Tokens,
    assets: watch::Sender<Vec<String>>,
    pricing: LiveClient,
    /// The generation last refused, so it is logged once and not checked
    /// again.
    rejected: Option<i64>,
}

impl Reloader {
    pub fn new(tokens: Tokens, assets: watch::Sender<Vec<String>>, pricing: LiveClient) -> Self {
        Self {
            tokens,
            assets,
            pricing,
            rejected: None,
        }
    }

    /// True while the latest copy is one this instance refused.
    pub fn invalid(&self) -> bool {
        self.rejected.is_some()
    }

    pub async fn apply(&mut self, decision: Decision) {
        match decision {
            Decision::Unchanged { generation } => {
                self.clear_rejected(generation);
                count("unchanged");
            }
            Decision::Reject { generation, error } => {
                if self.rejected != Some(generation) {
                    tracing::error!(
                        generation,
                        error = %error,
                        "token file: the latest copy is REFUSED; still running the current tokens"
                    );
                }
                self.rejected = Some(generation);
                count("rejected");
            }
            Decision::SameRows { generation } => {
                let mut set = (*self.tokens.current()).clone();
                set.generation = Some(generation);
                self.tokens.replace(set);
                self.clear_rejected(generation);
                metrics::gauge!("oracle_registry_generation").set(generation as f64);
                count("unchanged");
            }
            Decision::Apply {
                set,
                removed,
                readdressed,
                change,
            } => {
                let generation = set.generation;
                let symbols = set.symbols.clone();
                // Order matters. New requests resolve against the new set
                // first; then the subscription drops removed symbols, so no
                // new frame for them is cached; then their cached quotes go,
                // which also fails any request still holding one.
                let previous = self.tokens.replace(*set);
                self.assets.send_replace(symbols);
                let mut forget = removed;
                forget.extend(readdressed);
                self.pricing.forget(&forget).await;
                if let Some(generation) = generation {
                    self.clear_rejected(generation);
                }
                record_applied(&self.tokens.current());
                count("applied");
                tracing::info!(
                    from = ?previous.generation,
                    to = ?generation,
                    change = %change,
                    "token file: applied a new token set"
                );
            }
        }
        metrics::gauge!("oracle_registry_invalid").set(if self.invalid() { 1.0 } else { 0.0 });
    }

    fn clear_rejected(&mut self, generation: i64) {
        if self.rejected.take().is_some() {
            tracing::info!(generation, "token file: the latest copy is valid again");
        }
    }

    /// Check the bucket every `refresh_secs` and apply what it holds.
    pub fn spawn(
        mut self,
        static_config: Table,
        boot_config: Config,
        source: RegistrySource,
        bucket: Bucket,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(source.refresh_secs.max(5)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            metrics::gauge!("oracle_registry_invalid").set(0.0);
            let mut last_error: Option<String> = None;
            loop {
                ticker.tick().await;
                let current = self.tokens.current();
                // A refused copy stays the latest until a new upload, which
                // always gets a new generation; skip reading it again.
                let unless = self.rejected.or(current.generation);
                let read = bucket.read(&source.url, unless).await;
                let (bytes, generation) = match read {
                    Err(e) => {
                        let error = format!("{e:#}");
                        if last_error.as_deref() != Some(&error) {
                            tracing::warn!(url = %source.url, error = %error, "token file: check failed; still running the current tokens");
                            last_error = Some(error);
                        }
                        metrics::counter!("oracle_registry_fetch_errors_total").increment(1);
                        count("fetch_error");
                        continue;
                    }
                    Ok(read) => {
                        if last_error.take().is_some() {
                            tracing::info!(url = %source.url, "token file: checks succeed again");
                        }
                        metrics::gauge!("oracle_registry_last_check_timestamp_seconds")
                            .set(now_secs());
                        match read {
                            // A refused copy that is still the latest keeps
                            // counting as rejected on every check.
                            Read::Unchanged => {
                                count(if self.rejected.is_some() {
                                    "rejected"
                                } else {
                                    "unchanged"
                                });
                                continue;
                            }
                            Read::Copy { bytes, generation } => (bytes, generation),
                        }
                    }
                };
                let decision = decide(&static_config, &boot_config, &current, generation, &bytes);
                self.apply(decision).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;
    use toml::Value;

    use st0x_pricing_types::{Quote, WireAddress, WireFloat, WireU256};

    fn deployed() -> Table {
        let path = format!(
            "{}/deploy/config/production.toml",
            env!("CARGO_MANIFEST_DIR")
        );
        Config::parse_table(Path::new(&path)).unwrap()
    }

    fn fixture() -> Table {
        let path = format!(
            "{}/tests/fixtures/tokens-production.toml",
            env!("CARGO_MANIFEST_DIR")
        );
        token_file::parse(&std::fs::read(path).unwrap()).unwrap()
    }

    fn bytes(file: &Table) -> Vec<u8> {
        toml::to_string(file).unwrap().into_bytes()
    }

    fn slot<'a>(file: &'a mut Table, sym: &str) -> &'a mut Table {
        file["chains"]["base"]["assets"]["equities"][sym]
            .as_table_mut()
            .unwrap()
    }

    /// Boot state: generation 1 of the production fixture, and a reloader
    /// over a seeded pricing client.
    struct Running {
        static_config: Table,
        boot_config: Config,
        tokens: Tokens,
        assets: watch::Receiver<Vec<String>>,
        pricing: LiveClient,
        reloader: Reloader,
    }

    fn quote(symbol: &str, address: &str) -> Quote {
        let base: alloy::primitives::Address = address.parse().unwrap();
        Quote {
            asset: symbol.to_string(),
            chain_id: 8453,
            base: WireAddress::from(<[u8; 20]>::from(base)),
            quote: WireAddress::from_bytes([0x22; 20]),
            rate_base_to_quote: WireFloat::default(),
            rate_quote_to_base: WireFloat::default(),
            expiry_unix_ms: i64::MAX,
            execution_deadline_unix_ms: Some(i64::MAX),
            source_ts_unix_ms: 0,
            nav_ratio: WireU256::ZERO,
            underlying_rate_base_to_quote: WireFloat::default(),
            underlying_rate_quote_to_base: WireFloat::default(),
            session: Some(st0x_pricing_types::QuoteSession {
                tag: st0x_pricing_types::SessionTag::Rth,
                start_unix_ms: 0,
                end_unix_ms: i64::MAX,
            }),
        }
    }

    const COIN: &str = "wtCOIN";

    fn address_of(set: &TokenSet, symbol: &str) -> String {
        set.projection
            .as_ref()
            .unwrap()
            .rows()
            .into_iter()
            .find(|(s, _)| s == symbol)
            .map(|(_, a)| a)
            .unwrap()
    }

    async fn running() -> Running {
        let static_config = deployed();
        let (projection, boot_config, registry) =
            candidate(&static_config, &bytes(&fixture())).unwrap();
        let set = TokenSet {
            registry,
            symbols: boot_config.symbols(),
            projection: Some(projection),
            generation: Some(1),
        };
        let coin = address_of(&set, COIN);
        let (tx, assets) = watch::channel(set.symbols.clone());
        let tokens = Tokens::new(set);
        let pricing = LiveClient::with_seeded(vec![quote(COIN, &coin)], 8453).await;
        let reloader = Reloader::new(tokens.clone(), tx, pricing.clone());
        Running {
            static_config,
            boot_config,
            tokens,
            assets,
            pricing,
            reloader,
        }
    }

    impl Running {
        fn decide(&self, generation: i64, file: &Table) -> Decision {
            self.decide_bytes(generation, &bytes(file))
        }

        fn decide_bytes(&self, generation: i64, bytes: &[u8]) -> Decision {
            decide(
                &self.static_config,
                &self.boot_config,
                &self.tokens.current(),
                generation,
                bytes,
            )
        }
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_loop_applies_rejects_restores_and_survives_fetch_errors() {
        use axum::extract::{RawQuery, State};
        use axum::response::IntoResponse;

        struct Latest {
            generation: i64,
            bytes: Vec<u8>,
            unavailable: bool,
            queries: Vec<String>,
        }
        let mut file = fixture();
        slot(&mut file, "COIN").insert("pricing".into(), Value::String("disabled".into()));
        let latest = Arc::new(std::sync::Mutex::new(Latest {
            generation: 2,
            bytes: bytes(&file),
            unavailable: false,
            queries: Vec::new(),
        }));
        let app =
            axum::Router::new()
                .fallback(
                    |State(latest): State<Arc<std::sync::Mutex<Latest>>>,
                     RawQuery(query): RawQuery| async move {
                        let mut latest = latest.lock().unwrap();
                        let query = query.unwrap_or_default();
                        latest.queries.push(query.clone());
                        if latest.unavailable {
                            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        if query.split('&').any(|part| {
                            part == format!("ifGenerationNotMatch={}", latest.generation)
                        }) {
                            return axum::http::StatusCode::NOT_MODIFIED.into_response();
                        }
                        (
                            [("x-goog-generation", latest.generation.to_string())],
                            latest.bytes.clone(),
                        )
                            .into_response()
                    },
                )
                .with_state(latest.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let metrics = recorder.handle();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let r = running().await;
        let old_quote = r.pricing.snapshot_many(&[COIN]).await.remove(COIN).unwrap();
        let source = RegistrySource {
            url: "gs://test/tokens.toml".into(),
            refresh_secs: 5,
        };
        // Keep the runtime runnable while local HTTP I/O completes, so
        // paused time advances only when the test explicitly advances it.
        let keep_time_paused = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        let reload = r.reloader.spawn(
            r.static_config,
            r.boot_config,
            source,
            Bucket::at(&url, None),
        );
        tokio::task::yield_now().await;

        async fn wait_for_result(
            metrics: &metrics_exporter_prometheus::PrometheusHandle,
            result: &str,
            count: u64,
        ) {
            let expected = format!("oracle_registry_reload_total{{result=\"{result}\"}} {count}");
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !metrics.render().lines().any(|line| line == expected) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "expected {expected}; got {}",
                    metrics.render()
                );
                tokio::task::yield_now().await;
            }
        }

        async fn tick(
            metrics: &metrics_exporter_prometheus::PrometheusHandle,
            result: &str,
            count: u64,
        ) {
            tokio::time::advance(Duration::from_secs(5)).await;
            wait_for_result(metrics, result, count).await;
        }

        wait_for_result(&metrics, "applied", 1).await;
        let applied = r.tokens.current();
        assert_eq!(applied.generation, Some(2));
        assert!(!r.assets.borrow().iter().any(|symbol| symbol == COIN));
        assert!(r.pricing.latest(COIN).await.is_none());
        assert!(!old_quote.is_live());
        {
            let mut latest = latest.lock().unwrap();
            latest.generation = 3;
            latest.bytes = b"not a token file".to_vec();
        }
        tick(&metrics, "rejected", 1).await;
        assert!(Arc::ptr_eq(&applied, &r.tokens.current()));
        tick(&metrics, "rejected", 2).await;
        assert!(Arc::ptr_eq(&applied, &r.tokens.current()));
        assert!(metrics
            .render()
            .lines()
            .any(|line| line == "oracle_registry_invalid 1"));
        {
            let mut latest = latest.lock().unwrap();
            latest.generation = 4;
            latest.bytes = bytes(&file);
        }
        tick(&metrics, "unchanged", 1).await;
        let restored = r.tokens.current();
        assert_eq!(restored.generation, Some(4));
        assert!(metrics
            .render()
            .lines()
            .any(|line| line == "oracle_registry_invalid 0"));
        tick(&metrics, "unchanged", 2).await;
        assert!(Arc::ptr_eq(&restored, &r.tokens.current()));
        latest.lock().unwrap().unavailable = true;
        tick(&metrics, "fetch_error", 1).await;
        assert!(Arc::ptr_eq(&restored, &r.tokens.current()));
        assert!(metrics
            .render()
            .lines()
            .any(|line| line == "oracle_registry_invalid 0"));
        let queries = latest.lock().unwrap().queries.clone();
        assert_eq!(queries.len(), 6);
        for (query, generation) in queries.iter().zip([1, 2, 3, 3, 4, 4]) {
            assert!(
                query.contains(&format!("ifGenerationNotMatch={generation}")),
                "{query}"
            );
        }
        reload.abort();
        server.abort();
        keep_time_paused.abort();
        assert!(reload.await.unwrap_err().is_cancelled());
        assert!(server.await.unwrap_err().is_cancelled());
        assert!(keep_time_paused.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn reload_applies_a_valid_change() {
        let mut r = running().await;
        let snapshot = r.pricing.snapshot_many(&[COIN]).await.remove(COIN).unwrap();
        let mut file = fixture();
        slot(&mut file, "COIN").insert("pricing".into(), Value::String("disabled".into()));

        let decision = r.decide(2, &file);
        let Decision::Apply {
            removed, change, ..
        } = &decision
        else {
            panic!("{decision:?}");
        };
        assert_eq!(removed, &[COIN]);
        assert_eq!(change, "removed [wtCOIN]");
        r.reloader.apply(decision).await;

        let set = r.tokens.current();
        assert_eq!(set.generation, Some(2));
        assert!(!set.symbols.iter().any(|s| s == COIN));
        assert!(r.assets.has_changed().unwrap());
        assert!(!r.assets.borrow_and_update().iter().any(|s| s == COIN));
        assert!(r.pricing.latest(COIN).await.is_none());
        assert!(!snapshot.is_live(), "the old quote is revoked");
        assert!(!r.reloader.invalid());
    }

    #[tokio::test]
    async fn reload_adds_a_token() {
        let mut r = running().await;
        let mut file = fixture();
        let mut spy = slot(&mut file, "COIN").clone();
        spy.insert(
            "tokenized_equity_derivative".into(),
            Value::String("0x5555555555555555555555555555555555555555".into()),
        );
        spy.insert("venues".into(), Value::Array(vec!["raindex".into()]));
        file["chains"]["base"]["assets"]["equities"]
            .as_table_mut()
            .unwrap()
            .insert("ZZZ".into(), Value::Table(spy));

        let decision = r.decide(2, &file);
        assert!(
            matches!(&decision, Decision::Apply { change, .. } if change == "added [wtZZZ]"),
            "{decision:?}"
        );
        r.reloader.apply(decision).await;

        let usdc: alloy::primitives::Address = r.boot_config.quote_token.parse().unwrap();
        let zzz: alloy::primitives::Address = "0x5555555555555555555555555555555555555555"
            .parse()
            .unwrap();
        let pair = r.tokens.current().registry.resolve(usdc, zzz).unwrap();
        assert_eq!(pair.symbol, "wtZZZ");
        assert!(r.assets.borrow().iter().any(|s| s == "wtZZZ"));
        assert!(r.pricing.latest(COIN).await.is_some(), "other quotes stay");
    }

    #[tokio::test]
    async fn reload_rejects_an_invalid_file_and_keeps_running() {
        let placeholder = {
            let mut f = fixture();
            slot(&mut f, "COIN").insert(
                "tokenized_equity_derivative".into(),
                Value::String("0x0000000000000000000000000000000000000001".into()),
            );
            bytes(&f)
        };
        let empty = {
            let mut f = fixture();
            for (_, s) in f["chains"]["base"]["assets"]["equities"]
                .as_table_mut()
                .unwrap()
                .iter_mut()
            {
                s.as_table_mut()
                    .unwrap()
                    .insert("pricing".into(), Value::String("disabled".into()));
            }
            bytes(&f)
        };
        let schema = {
            let mut f = fixture();
            f.insert("schema_version".into(), Value::Integer(2));
            bytes(&f)
        };
        let quote_token = {
            let mut f = fixture();
            f["chains"]["base"].as_table_mut().unwrap().insert(
                "quote_token".into(),
                Value::String("0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168".into()),
            );
            bytes(&f)
        };
        let duplicate = {
            let mut f = fixture();
            let coin = slot(&mut f, "COIN")["tokenized_equity_derivative"].clone();
            slot(&mut f, "TSLA").insert("tokenized_equity_derivative".into(), coin);
            bytes(&f)
        };
        for (name, file, want) in [
            ("placeholder", placeholder, "Placeholder address"),
            ("empty", empty, "empty universe"),
            ("schema", schema, "schema_version"),
            ("quote token", quote_token, "settles in"),
            ("duplicate", duplicate, "uplicate"),
            ("not toml", b"[[[".to_vec(), "not valid TOML"),
        ] {
            let mut r = running().await;
            let before = r.tokens.current();
            let decision = r.decide_bytes(2, &file);
            let Decision::Reject { error, .. } = &decision else {
                panic!("{name}: {decision:?}");
            };
            assert!(error.contains(want), "{name}: {error}");
            r.reloader.apply(decision).await;
            assert!(Arc::ptr_eq(&before, &r.tokens.current()), "{name}");
            assert!(!r.assets.has_changed().unwrap(), "{name}");
            assert!(r.pricing.latest(COIN).await.is_some(), "{name}");
            assert_eq!(r.tokens.current().generation, Some(1), "{name}");
            assert!(r.reloader.invalid(), "{name}");
        }
    }

    #[tokio::test]
    async fn a_restore_clears_the_invalid_state() {
        let mut r = running().await;
        r.reloader
            .apply(r.decide_bytes(2, b"not a token file"))
            .await;
        assert!(r.reloader.invalid());

        let before = r.tokens.current();
        let decision = r.decide(1, &fixture());
        assert!(matches!(decision, Decision::Unchanged { generation: 1 }));
        r.reloader.apply(decision).await;
        assert!(!r.reloader.invalid());
        assert!(Arc::ptr_eq(&before, &r.tokens.current()));
        assert!(!r.assets.has_changed().unwrap());

        r.reloader
            .apply(r.decide_bytes(2, b"not a token file"))
            .await;
        assert!(r.reloader.invalid());

        let decision = r.decide(3, &fixture());
        assert!(
            matches!(decision, Decision::SameRows { generation: 3 }),
            "{decision:?}"
        );
        r.reloader.apply(decision).await;
        assert!(!r.reloader.invalid());
        assert_eq!(r.tokens.current().generation, Some(3));
        assert!(
            !r.assets.has_changed().unwrap(),
            "same rows, same subscription"
        );
    }

    #[tokio::test]
    async fn the_running_generation_is_unchanged() {
        let r = running().await;
        assert!(matches!(
            r.decide_bytes(1, b"anything"),
            Decision::Unchanged { generation: 1 }
        ));
    }

    #[tokio::test]
    async fn an_address_change_evicts_and_revokes() {
        let mut r = running().await;
        let snapshot = r.pricing.snapshot_many(&[COIN]).await.remove(COIN).unwrap();
        let mut file = fixture();
        slot(&mut file, "COIN").insert(
            "tokenized_equity_derivative".into(),
            Value::String("0x5555555555555555555555555555555555555555".into()),
        );
        let decision = r.decide(2, &file);
        let Decision::Apply {
            removed,
            readdressed,
            ..
        } = &decision
        else {
            panic!("{decision:?}");
        };
        assert!(removed.is_empty());
        assert_eq!(readdressed, &[COIN]);
        r.reloader.apply(decision).await;
        assert!(r.pricing.latest(COIN).await.is_none());
        assert!(!snapshot.is_live());
        assert!(
            r.assets.borrow().iter().any(|s| s == COIN),
            "still subscribed"
        );
    }

    /// Boot and the reload loop share `candidate`, so a copy one refuses
    /// the other refuses too.
    #[tokio::test]
    async fn boot_and_reload_validate_alike() {
        let r = running().await;
        let mut file = fixture();
        slot(&mut file, "COIN").insert("pricing".into(), Value::String("maybe".into()));
        let boot = candidate(&r.static_config, &bytes(&file)).unwrap_err();
        let Decision::Reject { error, .. } = r.decide(2, &file) else {
            panic!("reload accepted what boot refuses");
        };
        assert_eq!(error, format!("{boot:#}"));
    }
}
