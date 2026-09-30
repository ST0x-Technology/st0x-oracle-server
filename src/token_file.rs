//! T0's per-token config, read from the bucket.
//!
//! One file, `t0/<env>.toml` in st0x.registry, holds every token's config
//! for every T0 service; its CI uploads it to
//! `gs://t0-artifacts-tokens/<env>/tokens.toml`. The oracle takes from it
//! the `[[tokens]]` rows of the chain it serves: every slot with
//! `pricing = "enabled"` whose `venues` list `raindex`. Pricing publishes
//! every enabled slot; the oracle signs Raindex contexts, so a slot priced
//! only for bebop or the hook is left out.
//!
//! The rows are merged into the parsed config table before it is
//! deserialized, so [`Config::validate`] runs unchanged on the result.
//!
//! Every instance reads the file at boot. With `generation` set, it reads
//! that exact object generation, so every instance of a revision signs for
//! the same tokens and a change ships only through a release that bumps
//! it. Without it, each new instance reads whatever the bucket holds. The
//! refresh loop re-reads the latest copy and reports when it differs from
//! what this instance runs, but never applies it. With a pin, it also
//! checks the pinned generation is still readable.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Deserialize;
use toml::{Table, Value};

use crate::config::{Config, USDC_BASE};

/// The `schema_version` this build understands.
pub const SCHEMA_VERSION: i64 = 1;

/// The largest token file the oracle reads, the same bound pricing uses.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

/// `[registry]` in the oracle config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrySource {
    /// `gs://bucket/object`, read with the runtime service account.
    pub url: String,
    /// The object generation boot reads. Absent = the latest.
    #[serde(default)]
    pub generation: Option<u64>,
    #[serde(default = "default_refresh_secs")]
    pub refresh_secs: u64,
}

fn default_refresh_secs() -> u64 {
    60
}

/// The oracle's slice of the token file for one chain.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub chain_id: u64,
    pub quote_token: String,
    /// `[[tokens]]` rows, sorted by symbol.
    pub tokens: Vec<Value>,
}

impl Projection {
    /// Same rows and quote token, ignoring address case.
    pub fn same_as(&self, other: &Projection) -> bool {
        self.rows() == other.rows() && self.quote_token.eq_ignore_ascii_case(&other.quote_token)
    }

    fn rows(&self) -> BTreeSet<(String, String)> {
        self.tokens
            .iter()
            .filter_map(|t| {
                Some((
                    t.get("symbol")?.as_str()?.to_string(),
                    t.get("address")?.as_str()?.to_lowercase(),
                ))
            })
            .collect()
    }
}

/// The `[registry]` section of a config table, if it has one.
pub fn source_of(config: &Table) -> anyhow::Result<Option<RegistrySource>> {
    let Some(v) = config.get("registry") else {
        return Ok(None);
    };
    let source: RegistrySource = v
        .clone()
        .try_into()
        .context("[registry] must have `url` and, optionally, `generation` and `refresh_secs`")?;
    split_gs_url(&source.url)?;
    Ok(Some(source))
}

/// `gs://bucket/object` into its bucket and object, both non-empty.
fn split_gs_url(gs_url: &str) -> anyhow::Result<(&str, &str)> {
    gs_url
        .strip_prefix("gs://")
        .and_then(|rest| rest.split_once('/'))
        .filter(|(bucket, object)| !bucket.is_empty() && !object.is_empty())
        .with_context(|| format!("[registry] url {gs_url:?} is not gs://<bucket>/<object>"))
}

/// The chain a config table serves, with the same default as [`Config`].
pub fn chain_id_of(config: &Table) -> anyhow::Result<u64> {
    match config.get("chain_id") {
        None => Ok(8453),
        Some(v) => v
            .as_integer()
            .and_then(|i| u64::try_from(i).ok())
            .context("chain_id must be a positive integer"),
    }
}

pub fn parse(bytes: &[u8]) -> anyhow::Result<Table> {
    let text = std::str::from_utf8(bytes).context("token file is not UTF-8")?;
    toml::from_str(text).context("token file is not valid TOML")
}

fn table<'a>(v: Option<&'a Value>, what: &str) -> anyhow::Result<&'a Table> {
    v.and_then(Value::as_table)
        .with_context(|| format!("token file: {what} is missing or not a table"))
}

/// Turn the token file into the `[[tokens]]` rows for `chain_id`.
pub fn project(file: &Table, chain_id: u64) -> anyhow::Result<Projection> {
    match file.get("schema_version") {
        Some(Value::Integer(v)) if *v == SCHEMA_VERSION => {}
        other => bail!(
            "token file: schema_version must be {SCHEMA_VERSION}, got {}",
            other.map_or("nothing".to_string(), Value::to_string)
        ),
    }

    let chains = table(file.get("chains"), "chains")?;
    let mut matched = chains.iter().filter(|(_, c)| {
        c.get("chain_id")
            .and_then(Value::as_integer)
            .is_some_and(|id| u64::try_from(id).ok() == Some(chain_id))
    });
    let Some((name, chain)) = matched.next() else {
        bail!("token file has no chain with chain_id {chain_id}");
    };
    if matched.next().is_some() {
        bail!("token file has two chains with chain_id {chain_id}");
    }
    let chain = table(Some(chain), &format!("chains.{name}"))?;
    let quote_token = chain
        .get("quote_token")
        .and_then(Value::as_str)
        .with_context(|| format!("token file: chains.{name}.quote_token missing"))?
        .to_string();

    let mut tokens = Vec::new();
    if let Some(slots) = chain.get("assets").and_then(|a| a.get("equities")) {
        let slots = table(Some(slots), &format!("chains.{name}.assets.equities"))?;
        for (sym, slot) in slots {
            let slot = table(Some(slot), &format!("chains.{name}.assets.equities.{sym}"))?;
            match slot.get("pricing") {
                Some(Value::String(v)) if v == "enabled" => {}
                Some(Value::String(v)) if v == "disabled" => continue,
                other => bail!(
                    "token file: {name} {sym} pricing must be \"enabled\" or \"disabled\", got {}",
                    other.map_or("nothing".to_string(), Value::to_string)
                ),
            }
            let venues = slot
                .get("venues")
                .and_then(Value::as_array)
                .with_context(|| {
                    format!("token file: {name} {sym} is priced but has no venues list")
                })?;
            if !venues.iter().any(|v| v.as_str() == Some("raindex")) {
                continue;
            }
            let address = slot
                .get("tokenized_equity_derivative")
                .and_then(Value::as_str)
                .with_context(|| {
                    format!(
                        "token file: {name} {sym} is priced but has no tokenized_equity_derivative"
                    )
                })?;
            let mut row = Table::new();
            row.insert("address".into(), Value::String(address.to_string()));
            row.insert("symbol".into(), Value::String(format!("wt{sym}")));
            tokens.push(Value::Table(row));
        }
    }
    if tokens.is_empty() {
        bail!(
            "token file: no slot on chain {name} is priced for raindex; refusing an empty universe"
        );
    }
    tokens.sort_by_key(|t| t.get("symbol").and_then(Value::as_str).map(str::to_owned));

    Ok(Projection {
        chain_id,
        quote_token,
        tokens,
    })
}

/// Parse a token file, project it for the config's chain, and merge it in.
pub fn merge_bytes(config: &mut Table, bytes: &[u8]) -> anyhow::Result<Projection> {
    let projection = project(&parse(bytes)?, chain_id_of(config)?)?;
    merge(config, projection.clone())?;
    Ok(projection)
}

/// Put the projected rows into a config table that reads `[registry]`.
///
/// The config must not carry `[[tokens]]` itself, and its quote token must
/// be the one the token file gives its chain.
pub fn merge(config: &mut Table, p: Projection) -> anyhow::Result<()> {
    if config.contains_key("tokens") {
        bail!(
            "config reads its tokens from [registry] but also carries [[tokens]]; \
             keep one source and delete the inline copy"
        );
    }
    let declared = config
        .get("quote_token")
        .and_then(Value::as_str)
        .unwrap_or(USDC_BASE);
    if declared.to_lowercase() != p.quote_token.to_lowercase() {
        bail!(
            "token file says chain {} settles in {} but this config's quote_token is {declared}",
            p.chain_id,
            p.quote_token
        );
    }
    config.insert("tokens".into(), Value::Array(p.tokens));
    Ok(())
}

/// The client for bucket and metadata reads. Bounded, so a black-holed
/// request fails instead of hanging boot or the refresh loop.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("static reqwest client config cannot fail")
}

#[derive(Deserialize)]
struct MetadataToken {
    access_token: String,
}

/// Read the object at a `gs://bucket/object` URL as the runtime service
/// account. One `objects.get`; the reader role grants nothing else.
pub async fn fetch(
    http: &reqwest::Client,
    gs_url: &str,
    generation: Option<u64>,
) -> anyhow::Result<Vec<u8>> {
    let (bucket, object) = split_gs_url(gs_url)?;

    let token = http
        .get(METADATA_TOKEN_URL)
        .header("Metadata-Flavor", "Google")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("getting an access token to read the token file")?
        .json::<MetadataToken>()
        .await
        .context("reading the metadata access token")?
        .access_token;
    let mut url = format!(
        "https://storage.googleapis.com/storage/v1/b/{}/o/{}?alt=media",
        percent(bucket),
        percent(object)
    );
    if let Some(generation) = generation {
        url.push_str(&format!("&generation={generation}"));
    }
    let response = http
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("fetching {gs_url}"))?;
    let status = response.status();
    let mut response = response;
    if !status.is_success() {
        // The first chunk is enough for the message; an error body is not
        // worth reading up to MAX_BODY_BYTES.
        let first = response.chunk().await.ok().flatten().unwrap_or_default();
        bail!(
            "fetching {gs_url} returned {status}: {}",
            String::from_utf8_lossy(&first)
                .chars()
                .take(300)
                .collect::<String>()
        );
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BODY_BYTES as u64)
    {
        bail!("{gs_url} is larger than {MAX_BODY_BYTES} bytes; refusing it");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("reading the token file body")?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            bail!("{gs_url} is larger than {MAX_BODY_BYTES} bytes; refusing it");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Percent-encode one path segment; object names contain `/`.
fn percent(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Load the token file, project it, and merge it into `config`. `local`
/// (`--registry-file`) reads a file instead of the bucket. Bucket reads are
/// retried, so one transient error does not fail boot.
pub async fn load_into(
    config: &mut Table,
    source: &RegistrySource,
    local: Option<&std::path::Path>,
) -> anyhow::Result<Projection> {
    let bytes = match local {
        Some(path) => std::fs::read(path)
            .with_context(|| format!("reading the token file at {}", path.display()))?,
        None => {
            let http = http_client();
            let mut attempt = 1;
            loop {
                match fetch(&http, &source.url, source.generation).await {
                    Ok(bytes) => break bytes,
                    Err(e) if attempt < 4 => {
                        tracing::warn!(attempt, error = %format!("{e:#}"), "token file: boot read failed; retrying");
                        tokio::time::sleep(Duration::from_secs(2 << attempt)).await;
                        attempt += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    };
    merge_bytes(config, &bytes)
}

/// What changed between the running projection and a fresh one, in one
/// line for the log.
pub fn describe_change(live: &Projection, fresh: &Projection) -> String {
    let (a, b) = (live.rows(), fresh.rows());
    let syms = |s: &BTreeSet<(String, String)>| -> BTreeSet<String> {
        s.iter().map(|(sym, _)| sym.clone()).collect()
    };
    let (sa, sb) = (syms(&a), syms(&b));
    let mut parts = Vec::new();
    let added: Vec<_> = sb.difference(&sa).cloned().collect();
    let removed: Vec<_> = sa.difference(&sb).cloned().collect();
    if !added.is_empty() {
        parts.push(format!("added [{}]", added.join(",")));
    }
    if !removed.is_empty() {
        parts.push(format!("removed [{}]", removed.join(",")));
    }
    let moved: Vec<_> = sa
        .intersection(&sb)
        .filter(|s| a.iter().find(|(x, _)| x == *s) != b.iter().find(|(x, _)| x == *s))
        .cloned()
        .collect();
    if !moved.is_empty() {
        parts.push(format!("address changed [{}]", moved.join(",")));
    }
    if live.quote_token.to_lowercase() != fresh.quote_token.to_lowercase() {
        parts.push("quote token changed".to_string());
    }
    if parts.is_empty() {
        "no difference".to_string()
    } else {
        parts.join("; ")
    }
}

/// Check a fresh copy of the token file exactly the way boot would and
/// compare it with what this instance runs. `Err`: boot would refuse it.
/// `Ok(None)`: the same rows. `Ok(Some(change))`: a release would change
/// them, described in one line.
///
/// `static_config` is the config table as parsed from disk, before the
/// token rows were merged in.
pub fn assess(
    static_config: &Table,
    live: &Projection,
    bytes: &[u8],
) -> anyhow::Result<Option<String>> {
    let fresh = project(&parse(bytes)?, live.chain_id)?;
    let mut candidate = static_config.clone();
    merge(&mut candidate, fresh.clone())?;
    Config::from_table(candidate)?;
    Ok((!fresh.same_as(live)).then(|| describe_change(live, &fresh)))
}

fn set_gauge(name: &'static str, on: bool) {
    metrics::gauge!(name).set(if on { 1.0 } else { 0.0 });
}

/// Re-read the token file on an interval and say whether this instance is
/// behind it, and whether boot would accept it. Reports only; never applies.
///
/// Each fresh copy goes through [`assess`] against `static_config`, the
/// config table as parsed from disk.
pub fn spawn_refresh(static_config: Table, live: Projection, source: RegistrySource) {
    tokio::spawn(async move {
        let http = http_client();
        let mut ticker = tokio::time::interval(Duration::from_secs(source.refresh_secs.max(5)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // first tick is immediate; boot just loaded it.
                             // Boot loaded what it runs, so nothing is pending or invalid until a
                             // refresh says otherwise; without this the gauges are absent, not 0.
        set_gauge("oracle_registry_pending_restart", false);
        set_gauge("oracle_registry_invalid", false);
        set_gauge("oracle_registry_pinned_unreadable", false);
        let mut pending: Option<String> = None;
        let mut pinned_failures = 0u32;
        loop {
            ticker.tick().await;

            // Boot reads only the pinned generation and has no fallback, so
            // check it is still there. Three misses in a row, so one
            // transient error does not raise the gauge.
            if let Some(generation) = source.generation {
                match fetch(&http, &source.url, Some(generation)).await {
                    Ok(_) => pinned_failures = 0,
                    Err(e) => {
                        pinned_failures += 1;
                        metrics::counter!("oracle_registry_fetch_errors_total").increment(1);
                        tracing::warn!(url = %source.url, generation, failures = pinned_failures, error = %format!("{e:#}"), "token file: the pinned generation could not be read; boot needs it");
                    }
                }
                set_gauge("oracle_registry_pinned_unreadable", pinned_failures >= 3);
            }

            let bytes = match fetch(&http, &source.url, None).await {
                Ok(b) => b,
                Err(e) => {
                    metrics::counter!("oracle_registry_fetch_errors_total").increment(1);
                    tracing::warn!(url = %source.url, error = %format!("{e:#}"), "token file: fetch failed; still running what boot loaded");
                    continue;
                }
            };

            let change = match assess(&static_config, &live, &bytes) {
                Ok(change) => {
                    set_gauge("oracle_registry_invalid", false);
                    change
                }
                Err(e) => {
                    set_gauge("oracle_registry_invalid", true);
                    tracing::error!(url = %source.url, error = %format!("{e:#}"), "token file: the bucket copy would be REFUSED at boot; fix it before the next restart");
                    continue;
                }
            };

            match change {
                None => {
                    if pending.take().is_some() {
                        tracing::info!("token file: bucket copy matches this instance again");
                    }
                    set_gauge("oracle_registry_pending_restart", false);
                }
                Some(change) => {
                    if pending.as_deref() != Some(&change) {
                        tracing::warn!(change = %change, "token file: bucket copy differs from what this instance runs; a release picks it up");
                        pending = Some(change);
                    }
                    set_gauge("oracle_registry_pending_restart", true);
                }
            }
        }
    });
}

/// Load a deployed config for a test, taking the token rows from the
/// snapshot of that env's bucket file in `tests/fixtures`.
#[cfg(test)]
pub(crate) fn load_deployed(path: &std::path::Path) -> anyhow::Result<Config> {
    let mut table = Config::parse_table(path)?;
    if let Some(source) = source_of(&table)? {
        let env = if source.url.contains("/production/") {
            "production"
        } else {
            "staging"
        };
        let fixture = format!(
            "{}/tests/fixtures/tokens-{env}.toml",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(&fixture).with_context(|| format!("reading {fixture}"))?;
        merge_bytes(&mut table, &bytes)?;
    }
    Config::from_table(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn deployed(name: &str) -> String {
        format!("{}/deploy/config/{name}.toml", env!("CARGO_MANIFEST_DIR"))
    }

    fn inline_rows(name: &str) -> BTreeSet<(String, String)> {
        let inline: Config = toml::from_str(&fixture(&format!("{name}-inline.toml"))).unwrap();
        inline
            .tokens
            .iter()
            .map(|t| (t.symbol.clone(), t.address.to_lowercase()))
            .collect()
    }

    /// The token file projects into exactly the rows each inline config
    /// carried the day it was replaced, plus the rows listed since then.
    /// Staging drops wtSGOV, which the staging token file (like staging
    /// pricing) has switched off.
    #[test]
    fn registry_projects_to_the_inline_rows_it_replaced() {
        let base_added: &[(&str, &str)] =
            &[("wtSPY", "0x9aa9c5a24e976096a6a7ab44986dae8230fa5b27")];
        let robinhood_added: &[(&str, &str)] =
            &[("wtSNES", "0x06096908dbc38fc54509024674e4fd1891b5f7ca")];
        for (plane, env, dropped, added) in [
            ("production", "production", None, base_added),
            ("robinhood", "production", None, robinhood_added),
            ("staging", "staging", Some("wtSGOV"), &[][..]),
        ] {
            let path = deployed(plane);
            let table = Config::parse_table(Path::new(&path)).unwrap();
            let chain_id = chain_id_of(&table).unwrap();
            let p = project(
                &parse(fixture(&format!("tokens-{env}.toml")).as_bytes()).unwrap(),
                chain_id,
            )
            .unwrap();
            let mut expected = inline_rows(plane);
            if let Some(sym) = dropped {
                expected.retain(|(s, _)| s != sym);
            }
            expected.extend(
                added
                    .iter()
                    .map(|(sym, addr)| ((*sym).to_string(), (*addr).to_string())),
            );
            assert_eq!(p.rows(), expected, "{plane}");
        }
    }

    #[test]
    fn the_deployed_configs_load_through_the_registry() {
        for (plane, env, chain_id, count) in [
            ("production", "production", 8453, 48),
            ("robinhood", "production", 4663, 6),
            ("staging", "staging", 8453, 46),
        ] {
            let path = deployed(plane);
            let table = Config::parse_table(Path::new(&path)).unwrap();
            let source = source_of(&table).unwrap().expect("reads [registry]");
            assert_eq!(
                source.url,
                format!("gs://t0-artifacts-tokens/{env}/tokens.toml")
            );
            assert_eq!(source.generation.is_some(), env == "production", "{plane}");
            let config = load_deployed(Path::new(&path)).expect("merged config must validate");
            assert_eq!(config.chain_id, chain_id, "{plane}");
            assert_eq!(config.tokens.len(), count, "{plane}");
        }
    }

    #[test]
    fn a_wrong_schema_version_is_refused() {
        let mut file = parse(fixture("tokens-staging.toml").as_bytes()).unwrap();
        file.insert("schema_version".into(), Value::Integer(2));
        let err = project(&file, 8453).unwrap_err().to_string();
        assert!(err.contains("schema_version must be 1"), "{err}");
    }

    #[test]
    fn an_unknown_chain_is_refused() {
        let file = parse(fixture("tokens-staging.toml").as_bytes()).unwrap();
        let err = project(&file, 10).unwrap_err().to_string();
        assert!(err.contains("no chain with chain_id 10"), "{err}");
    }

    #[test]
    fn an_inline_copy_next_to_registry_is_refused() {
        let mut table: Table = toml::from_str(
            r#"
            [registry]
            url = "gs://b/o"
            [[tokens]]
            address = "0x1111111111111111111111111111111111111111"
            symbol = "wtCOIN"
            "#,
        )
        .unwrap();
        let p = project(
            &parse(fixture("tokens-staging.toml").as_bytes()).unwrap(),
            8453,
        )
        .unwrap();
        let err = merge(&mut table, p).unwrap_err().to_string();
        assert!(err.contains("also carries [[tokens]]"), "{err}");
    }

    #[test]
    fn a_quote_token_the_file_disagrees_with_is_refused() {
        let mut table: Table = toml::from_str(
            r#"
            chain_id = 4663
            [registry]
            url = "gs://b/o"
            "#,
        )
        .unwrap();
        let p = project(
            &parse(fixture("tokens-production.toml").as_bytes()).unwrap(),
            4663,
        )
        .unwrap();
        let err = merge(&mut table, p).unwrap_err().to_string();
        assert!(err.contains("settles in"), "{err}");
    }

    #[test]
    fn a_pricing_value_other_than_enabled_or_disabled_is_refused() {
        let file = parse(fixture("tokens-production.toml").as_bytes()).unwrap();
        let (name, _) = file["chains"]
            .as_table()
            .unwrap()
            .iter()
            .find(|(_, c)| c.get("chain_id").and_then(Value::as_integer) == Some(8453))
            .unwrap();
        let name = name.clone();
        for bad in [
            Some(Value::String("Enabled".into())),
            Some(Value::Boolean(true)),
            None,
        ] {
            let mut file = file.clone();
            let slots = file["chains"][name.as_str()]["assets"]["equities"]
                .as_table_mut()
                .unwrap();
            let slot = slots.iter_mut().next().unwrap().1.as_table_mut().unwrap();
            match &bad {
                Some(v) => slot.insert("pricing".into(), v.clone()),
                None => slot.remove("pricing"),
            };
            let err = project(&file, 8453).unwrap_err().to_string();
            assert!(err.contains("pricing must be"), "{bad:?}: {err}");
        }
    }

    /// A slot priced for other venues only is not the oracle's to sign;
    /// a priced slot with no venues list is a file the registry would
    /// refuse, so boot refuses it too.
    #[test]
    fn a_slot_priced_without_raindex_is_left_out() {
        let file = parse(fixture("tokens-production.toml").as_bytes()).unwrap();
        let before = project(&file, 4663).unwrap();
        let mut only_bebop = file.clone();
        let slot = only_bebop["chains"]["robinhood"]["assets"]["equities"]["FGI"]
            .as_table_mut()
            .unwrap();
        slot.insert(
            "venues".into(),
            Value::Array(vec![Value::String("bebop".into())]),
        );
        let after = project(&only_bebop, 4663).unwrap();
        assert_eq!(describe_change(&before, &after), "removed [wtFGI]");

        let mut no_venues = file;
        no_venues["chains"]["robinhood"]["assets"]["equities"]["FGI"]
            .as_table_mut()
            .unwrap()
            .remove("venues");
        let err = project(&no_venues, 4663).unwrap_err().to_string();
        assert!(err.contains("has no venues list"), "{err}");
    }

    #[test]
    fn a_registry_url_that_is_not_a_gs_object_is_refused() {
        for url in ["https://b/o", "gs://b", "gs:///o", "gs://b/"] {
            let table: Table = toml::from_str(&format!("[registry]\nurl = {url:?}")).unwrap();
            let err = source_of(&table).unwrap_err().to_string();
            assert!(
                err.contains("is not gs://<bucket>/<object>"),
                "{url}: {err}"
            );
        }
    }

    #[test]
    fn address_case_alone_is_not_drift() {
        let p = project(
            &parse(fixture("tokens-production.toml").as_bytes()).unwrap(),
            4663,
        )
        .unwrap();
        let mut q = p.clone();
        q.quote_token = q.quote_token.to_uppercase().replace("0X", "0x");
        for t in &mut q.tokens {
            if let Value::Table(t) = t {
                let lower = t["address"].as_str().unwrap().to_lowercase();
                t.insert("address".into(), Value::String(lower));
            }
        }
        assert!(p.same_as(&q));
        q.tokens.pop();
        assert!(!p.same_as(&q));
    }

    #[test]
    fn change_description_names_the_rows() {
        let p = project(
            &parse(fixture("tokens-production.toml").as_bytes()).unwrap(),
            4663,
        )
        .unwrap();
        let mut q = p.clone();
        q.tokens
            .retain(|t| t.get("symbol").and_then(Value::as_str) != Some("wtFGI"));
        if let Value::Table(t) = &mut q.tokens[0] {
            t.insert(
                "address".into(),
                Value::String("0x0000000000000000000000000000000000000001".into()),
            );
        }
        assert_eq!(
            describe_change(&p, &q),
            "removed [wtFGI]; address changed [wtDNUT]"
        );
        assert_eq!(describe_change(&p, &p), "no difference");
    }

    /// The refresh verdict on the production Base plane: an unchanged
    /// copy, one slot switched off, and a placeholder address.
    #[test]
    fn assess_gives_the_refresh_verdict() {
        let static_config = Config::parse_table(Path::new(&deployed("production"))).unwrap();
        let bytes = fixture("tokens-production.toml");
        let live = project(&parse(bytes.as_bytes()).unwrap(), 8453).unwrap();

        assert_eq!(
            assess(&static_config, &live, bytes.as_bytes()).unwrap(),
            None
        );

        let mut file = parse(bytes.as_bytes()).unwrap();
        let coin = file["chains"]["base"]["assets"]["equities"]["COIN"]
            .as_table_mut()
            .unwrap();
        coin.insert("pricing".into(), Value::String("disabled".into()));
        let disabled = toml::to_string(&file).unwrap();
        assert_eq!(
            assess(&static_config, &live, disabled.as_bytes()).unwrap(),
            Some("removed [wtCOIN]".to_string())
        );

        let mut file = parse(bytes.as_bytes()).unwrap();
        let coin = file["chains"]["base"]["assets"]["equities"]["COIN"]
            .as_table_mut()
            .unwrap();
        coin.insert(
            "tokenized_equity_derivative".into(),
            Value::String("0x0000000000000000000000000000000000000001".into()),
        );
        let placeholder = toml::to_string(&file).unwrap();
        let err = format!(
            "{:#}",
            assess(&static_config, &live, placeholder.as_bytes()).unwrap_err()
        );
        assert!(err.contains("Placeholder address"), "{err}");
    }
}
