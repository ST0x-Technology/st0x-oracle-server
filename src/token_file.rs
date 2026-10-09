//! T0's per-token config, read from the bucket.
//!
//! One file, `t0/<env>.toml` in T0Trade/t0.tokens, holds every token's
//! config for every T0 service; its CI uploads it to
//! `gs://t0-artifacts-tokens/<env>/tokens.toml`. The oracle takes from it
//! the `[[tokens]]` rows of the chain it serves: every slot with
//! `pricing = "enabled"` whose `venues` list `raindex`. Pricing publishes
//! every enabled slot; the oracle signs Raindex contexts, so a slot priced
//! only for bebop or the hook is left out.
//!
//! The rows are merged into the parsed config table before it is
//! deserialized, so [`Config::validate`] runs unchanged on the result.
//!
//! The oracle always runs the latest copy in the bucket. Boot reads it
//! before anything else and refuses to start if it cannot read it or the
//! copy fails validation; there is no pinned generation and no fallback.
//! While running, the reload loop (`crate::reload`) checks for a newer
//! generation every `refresh_secs` and applies a copy that passes the same
//! checks boot runs ([`candidate`]).

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde::Deserialize;
use toml::{Table, Value};

use crate::config::{Config, USDC_BASE};
use crate::registry::TokenRegistry;
use crate::tokens::TokenSet;

/// The `schema_version` this build understands.
pub const SCHEMA_VERSION: i64 = 1;

/// The largest token file the oracle reads, the same bound pricing uses.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

const STORAGE_URL: &str = "https://storage.googleapis.com";

/// Set to an OAuth access token (e.g. `gcloud auth print-access-token`) to
/// read the bucket from outside GCP. Unset on the deployed service, which
/// asks the metadata server.
pub const ACCESS_TOKEN_ENV: &str = "GCS_ACCESS_TOKEN";

/// `[registry]` in the oracle config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrySource {
    /// `gs://bucket/object`, read with the runtime service account.
    pub url: String,
    /// Seconds between checks for a newer copy. Floored at 5.
    #[serde(default = "default_refresh_secs")]
    pub refresh_secs: u64,
}

fn default_refresh_secs() -> u64 {
    10
}

/// The oracle's slice of the token file for one chain.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub chain_id: u64,
    pub quote_token: String,
    /// `[[tokens]]` rows, sorted by symbol.
    pub tokens: Vec<Value>,
}

/// What changed between two projections, by symbol.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// In both, at another address.
    pub readdressed: Vec<String>,
    pub quote_token_changed: bool,
}

impl Projection {
    /// Same rows and quote token, ignoring address case.
    pub fn same_as(&self, other: &Projection) -> bool {
        self.rows() == other.rows() && self.quote_token.eq_ignore_ascii_case(&other.quote_token)
    }

    pub(crate) fn rows(&self) -> BTreeSet<(String, String)> {
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
    if v.get("generation").is_some() {
        bail!(
            "[registry].generation is no longer supported: the oracle follows the latest \
             bucket copy; delete it"
        );
    }
    let source: RegistrySource = v
        .clone()
        .try_into()
        .context("[registry] must have `url` and, optionally, `refresh_secs`")?;
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
    expires_in: u64,
}

/// Where the bucket's access token comes from.
enum TokenSource {
    /// `GCS_ACCESS_TOKEN`, for runs outside GCP.
    Fixed(String),
    /// The metadata server, cached until a minute before it expires.
    Metadata {
        url: String,
        cached: tokio::sync::Mutex<Option<(String, Instant)>>,
    },
}

/// One read of the object.
#[derive(Debug, PartialEq, Eq)]
pub enum Read {
    /// The latest generation is the one the read was conditional on.
    Unchanged,
    /// The latest copy and its generation, from the same response.
    Copy { bytes: Vec<u8>, generation: i64 },
}

/// Reads the token file from the bucket as the runtime service account.
/// Every read is one `objects.get`, which is all the reader role grants.
pub struct Bucket {
    http: reqwest::Client,
    storage_url: String,
    token: TokenSource,
}

impl Bucket {
    /// Cloud Storage, authenticated by `GCS_ACCESS_TOKEN` when it is set
    /// and by the metadata server otherwise.
    pub fn from_env() -> Self {
        let token = match std::env::var(ACCESS_TOKEN_ENV) {
            Ok(t) if !t.trim().is_empty() => TokenSource::Fixed(t.trim().to_string()),
            _ => TokenSource::Metadata {
                url: METADATA_TOKEN_URL.to_string(),
                cached: tokio::sync::Mutex::new(None),
            },
        };
        Self {
            http: http_client(),
            storage_url: STORAGE_URL.to_string(),
            token,
        }
    }

    /// A bucket served at `storage_url`, for tests.
    #[cfg(test)]
    pub(crate) fn at(storage_url: &str, metadata_url: Option<&str>) -> Self {
        Self {
            http: http_client(),
            storage_url: storage_url.to_string(),
            token: match metadata_url {
                Some(url) => TokenSource::Metadata {
                    url: url.to_string(),
                    cached: tokio::sync::Mutex::new(None),
                },
                None => TokenSource::Fixed("test-token".to_string()),
            },
        }
    }

    async fn access_token(&self) -> anyhow::Result<String> {
        let (url, cached) = match &self.token {
            TokenSource::Fixed(token) => return Ok(token.clone()),
            TokenSource::Metadata { url, cached } => (url, cached),
        };
        let mut cached = cached.lock().await;
        if let Some((token, until)) = cached.as_ref() {
            if Instant::now() < *until {
                return Ok(token.clone());
            }
        }
        let fresh = self
            .http
            .get(url)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .context("getting an access token to read the token file")?
            .json::<MetadataToken>()
            .await
            .context("reading the metadata access token")?;
        let until = Instant::now() + Duration::from_secs(fresh.expires_in.saturating_sub(60));
        *cached = Some((fresh.access_token.clone(), until));
        Ok(fresh.access_token)
    }

    async fn forget_access_token(&self) {
        if let TokenSource::Metadata { cached, .. } = &self.token {
            *cached.lock().await = None;
        }
    }

    /// Read the latest copy of the object at a `gs://bucket/object` URL.
    /// With `unless_generation`, a latest copy of that generation is not
    /// sent again and the read is [`Read::Unchanged`].
    pub async fn read(&self, gs_url: &str, unless_generation: Option<i64>) -> anyhow::Result<Read> {
        let (bucket, object) = split_gs_url(gs_url)?;
        let token = self.access_token().await?;
        let mut url = format!(
            "{}/storage/v1/b/{}/o/{}?alt=media",
            self.storage_url,
            percent(bucket),
            percent(object)
        );
        if let Some(generation) = unless_generation {
            url.push_str(&format!("&ifGenerationNotMatch={generation}"));
        }
        let mut response = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .with_context(|| format!("fetching {gs_url}"))?;
        let status = response.status();
        // Only 304 proves the latest copy is still that generation. A 412 can
        // carry other failed preconditions, so it is a fetch error.
        if unless_generation.is_some() && status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(Read::Unchanged);
        }
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.forget_access_token().await;
            }
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
        let generation = generation_of(response.headers())
            .with_context(|| format!("fetching {gs_url}: no usable x-goog-generation header"))?;
        if response
            .content_length()
            .is_some_and(|n| n > MAX_BODY_BYTES as u64)
        {
            bail!("{gs_url} is larger than {MAX_BODY_BYTES} bytes; refusing it");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("reading the token file body")?
        {
            if bytes.len() + chunk.len() > MAX_BODY_BYTES {
                bail!("{gs_url} is larger than {MAX_BODY_BYTES} bytes; refusing it");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Read::Copy { bytes, generation })
    }
}

/// The object generation of a media response. It names the bytes in the
/// same response, so the two cannot come from different uploads.
fn generation_of(headers: &reqwest::header::HeaderMap) -> Option<i64> {
    headers
        .get("x-goog-generation")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|g: &i64| *g > 0)
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

/// Check a copy of the token file the way boot does and build what the
/// server would run from it. Boot and the reload loop both call this, so
/// they cannot disagree on what is valid.
///
/// `static_config` is the config table as parsed from disk, before the
/// token rows are merged in.
pub fn candidate(
    static_config: &Table,
    bytes: &[u8],
) -> anyhow::Result<(Projection, Config, TokenRegistry)> {
    let projection = project(&parse(bytes)?, chain_id_of(static_config)?)?;
    let mut table = static_config.clone();
    merge(&mut table, projection.clone())?;
    let config = Config::from_table(table)?;
    let registry = TokenRegistry::from_config(&config.tokens, &config.quote_token)?;
    Ok((projection, config, registry))
}

/// How boot retries a failed bucket read.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    pub attempts: u32,
    /// Doubles after each failure.
    pub first_backoff: Duration,
}

impl Retry {
    /// Four reads, 4s/8s/16s apart: about half a minute, well inside the
    /// Cloud Run startup timeout.
    pub const BOOT: Self = Self {
        attempts: 4,
        first_backoff: Duration::from_secs(4),
    };
}

/// Read the latest token file and build the token set the server starts
/// with. A read is retried per `retry`; a copy that fails validation is
/// not, and nothing falls back to an older copy.
pub async fn boot(
    static_config: &Table,
    source: &RegistrySource,
    bucket: &Bucket,
    retry: Retry,
) -> anyhow::Result<(Config, TokenSet)> {
    let mut backoff = retry.first_backoff;
    let mut attempt = 1;
    let (bytes, generation) = loop {
        match bucket.read(&source.url, None).await {
            Ok(Read::Copy { bytes, generation }) => break (bytes, generation),
            Ok(Read::Unchanged) => {
                bail!("an unconditional read of {} was not modified", source.url)
            }
            Err(e) if attempt < retry.attempts => {
                tracing::warn!(attempt, url = %source.url, error = %format!("{e:#}"), "token file: boot read failed; retrying");
                tokio::time::sleep(backoff).await;
                backoff *= 2;
                attempt += 1;
            }
            Err(e) => {
                tracing::error!(url = %source.url, error = %format!("{e:#}"), "no live token file reachable; refusing to start");
                return Err(e.context(format!(
                    "no live token file reachable at {} after {attempt} tries; refusing to start",
                    source.url
                )));
            }
        }
    };
    let (projection, config, registry) = candidate(static_config, &bytes).with_context(|| {
        format!(
            "the live token file {} (generation {generation}) fails validation; refusing to start",
            source.url
        )
    })?;
    let set = TokenSet {
        registry,
        symbols: config.symbols(),
        projection: Some(projection),
        generation: Some(generation),
    };
    Ok((config, set))
}

/// What changed between the running projection and a fresh one.
pub fn change(live: &Projection, fresh: &Projection) -> Change {
    let (a, b) = (live.rows(), fresh.rows());
    let syms = |s: &BTreeSet<(String, String)>| -> BTreeSet<String> {
        s.iter().map(|(sym, _)| sym.clone()).collect()
    };
    let (sa, sb) = (syms(&a), syms(&b));
    Change {
        added: sb.difference(&sa).cloned().collect(),
        removed: sa.difference(&sb).cloned().collect(),
        readdressed: sa
            .intersection(&sb)
            .filter(|s| a.iter().find(|(x, _)| x == *s) != b.iter().find(|(x, _)| x == *s))
            .cloned()
            .collect(),
        quote_token_changed: !live.quote_token.eq_ignore_ascii_case(&fresh.quote_token),
    }
}

impl std::fmt::Display for Change {
    /// One line for the log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if !self.added.is_empty() {
            parts.push(format!("added [{}]", self.added.join(",")));
        }
        if !self.removed.is_empty() {
            parts.push(format!("removed [{}]", self.removed.join(",")));
        }
        if !self.readdressed.is_empty() {
            parts.push(format!("address changed [{}]", self.readdressed.join(",")));
        }
        if self.quote_token_changed {
            parts.push("quote token changed".to_string());
        }
        if parts.is_empty() {
            f.write_str("no difference")
        } else {
            f.write_str(&parts.join("; "))
        }
    }
}

/// What changed between the running projection and a fresh one, in one
/// line for the log.
pub fn describe_change(live: &Projection, fresh: &Projection) -> String {
    change(live, fresh).to_string()
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
            assert!(
                table["registry"].get("generation").is_none()
                    && table["registry"].get("refresh_secs").is_none(),
                "{plane}: follows the latest copy at the default interval"
            );
            assert_eq!(source.refresh_secs, 10, "{plane}");
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

    #[test]
    fn a_pinned_generation_is_refused() {
        let table: Table =
            toml::from_str("[registry]\nurl = \"gs://b/o\"\ngeneration = 1790782803062872")
                .unwrap();
        let err = source_of(&table).unwrap_err().to_string();
        assert!(err.contains("generation is no longer supported"), "{err}");
    }

    /// One queued response from the stub bucket.
    struct Reply {
        status: u16,
        generation: Option<&'static str>,
        body: Vec<u8>,
    }

    fn copy(generation: &'static str, body: &[u8]) -> Reply {
        Reply {
            status: 200,
            generation: Some(generation),
            body: body.to_vec(),
        }
    }

    fn status(status: u16) -> Reply {
        Reply {
            status,
            generation: None,
            body: b"nope".to_vec(),
        }
    }

    #[derive(Default)]
    struct Stub {
        replies: std::sync::Mutex<std::collections::VecDeque<Reply>>,
        queries: std::sync::Mutex<Vec<String>>,
        token_reads: std::sync::atomic::AtomicUsize,
    }

    /// A local stand-in for Cloud Storage and the metadata server. Each
    /// object read takes the next queued reply; an empty queue is a 503.
    async fn stub(replies: Vec<Reply>) -> (String, std::sync::Arc<Stub>) {
        use axum::extract::{RawQuery, State};
        use axum::response::IntoResponse;
        let stub = std::sync::Arc::new(Stub {
            replies: std::sync::Mutex::new(replies.into()),
            ..Default::default()
        });
        let app = axum::Router::new()
            .route(
                "/token",
                axum::routing::get(|State(stub): State<std::sync::Arc<Stub>>| async move {
                    stub.token_reads
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    axum::Json(serde_json::json!({"access_token": "t", "expires_in": 3600}))
                }),
            )
            .fallback(
                |State(stub): State<std::sync::Arc<Stub>>, RawQuery(q): RawQuery| async move {
                    stub.queries.lock().unwrap().push(q.unwrap_or_default());
                    let reply = stub
                        .replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or(status(503));
                    let mut response = (
                        axum::http::StatusCode::from_u16(reply.status).unwrap(),
                        reply.body,
                    )
                        .into_response();
                    if let Some(g) = reply.generation {
                        response
                            .headers_mut()
                            .insert("x-goog-generation", g.parse().unwrap());
                    }
                    response
                },
            )
            .with_state(stub.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, stub)
    }

    const URL: &str = "gs://t0-artifacts-tokens/production/tokens.toml";

    #[tokio::test]
    async fn a_read_carries_its_generation_and_only_304_is_unchanged() {
        let (url, stub) = stub(vec![
            copy("1790782803062872", b"body"),
            status(304),
            status(412),
            Reply {
                status: 200,
                generation: None,
                body: b"body".to_vec(),
            },
            copy("not-a-number", b"body"),
        ])
        .await;
        let bucket = Bucket::at(&url, Some(&format!("{url}/token")));

        assert_eq!(
            bucket.read(URL, None).await.unwrap(),
            Read::Copy {
                bytes: b"body".to_vec(),
                generation: 1790782803062872
            }
        );
        assert_eq!(bucket.read(URL, Some(7)).await.unwrap(), Read::Unchanged);
        let err = format!("{:#}", bucket.read(URL, Some(7)).await.unwrap_err());
        assert!(err.contains("412"), "{err}");
        for _ in 0..2 {
            let err = format!("{:#}", bucket.read(URL, None).await.unwrap_err());
            assert!(err.contains("x-goog-generation"), "{err}");
        }
        let queries = stub.queries.lock().unwrap().clone();
        assert_eq!(queries[0], "alt=media");
        assert_eq!(queries[1], "alt=media&ifGenerationNotMatch=7");
        assert_eq!(
            stub.token_reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the access token is cached"
        );
    }

    const FAST: Retry = Retry {
        attempts: 4,
        first_backoff: Duration::from_millis(1),
    };

    fn production() -> (Table, RegistrySource) {
        let table = Config::parse_table(Path::new(&deployed("production"))).unwrap();
        let source = source_of(&table).unwrap().unwrap();
        (table, source)
    }

    #[tokio::test]
    async fn boot_refuses_to_start_without_a_live_copy() {
        let (table, source) = production();
        let (url, stub) = stub(vec![]).await;
        let err = format!(
            "{:#}",
            boot(&table, &source, &Bucket::at(&url, None), FAST)
                .await
                .unwrap_err()
        );
        assert!(err.contains("refusing to start"), "{err}");
        assert!(err.contains("503"), "{err}");
        assert_eq!(
            stub.queries.lock().unwrap().len(),
            4,
            "retried, then gave up"
        );

        // Nothing listening at all.
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let err = format!(
            "{:#}",
            boot(&table, &source, &Bucket::at(&closed, None), FAST)
                .await
                .unwrap_err()
        );
        assert!(err.contains("no live token file reachable"), "{err}");
    }

    #[tokio::test]
    async fn boot_refuses_an_invalid_live_copy() {
        let (table, source) = production();
        let mut file = parse(fixture("tokens-production.toml").as_bytes()).unwrap();
        file["chains"]["base"]["assets"]["equities"]["COIN"]
            .as_table_mut()
            .unwrap()
            .insert(
                "tokenized_equity_derivative".into(),
                Value::String("0x0000000000000000000000000000000000000001".into()),
            );
        let bad = toml::to_string(&file).unwrap();
        let (url, stub) = stub(vec![copy("9", bad.as_bytes())]).await;
        let err = format!(
            "{:#}",
            boot(&table, &source, &Bucket::at(&url, None), FAST)
                .await
                .unwrap_err()
        );
        assert!(err.contains("generation 9) fails validation"), "{err}");
        assert!(err.contains("Placeholder address"), "{err}");
        assert_eq!(stub.queries.lock().unwrap().len(), 1, "not retried");
    }

    #[tokio::test]
    async fn boot_applies_the_latest_generation() {
        let (table, source) = production();
        let (url, _stub) = stub(vec![
            status(500),
            copy("7", fixture("tokens-production.toml").as_bytes()),
        ])
        .await;
        let (config, set) = boot(&table, &source, &Bucket::at(&url, None), FAST)
            .await
            .unwrap();
        assert_eq!(set.generation, Some(7));
        assert_eq!(config.tokens.len(), 48);
        assert_eq!(set.symbols, config.symbols());
        assert!(set.projection.is_some());
    }
}
