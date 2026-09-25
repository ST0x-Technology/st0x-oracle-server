//! T0's per-token config, read from the bucket.
//!
//! One file, `t0/<env>.toml` in st0x.registry, holds every token's config
//! for every T0 service; its CI uploads it to
//! `gs://t0-artifacts-tokens/<env>/tokens.toml`. The oracle takes from it
//! the `[[tokens]]` rows of the chain it serves: every slot with
//! `pricing = "enabled"`, the same rule st0x.pricing uses, so the oracle
//! subscribes to exactly what pricing publishes on that chain.
//!
//! The rows are merged into the parsed config table before it is
//! deserialized, so [`Config::validate`] runs unchanged on the result.
//!
//! Every instance reads the file at boot. With `generation` set, it reads
//! that exact object generation, so every instance of a revision signs for
//! the same tokens and a change ships only through a release that bumps
//! it. Without it, each new instance reads whatever the bucket holds. The
//! refresh loop re-reads the latest copy and reports when it differs from
//! what this instance runs, but never applies it.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Deserialize;
use toml::{Table, Value};

use crate::config::{Config, USDC_BASE};

/// The `schema_version` this build understands.
pub const SCHEMA_VERSION: i64 = 1;

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
    match config.get("registry") {
        None => Ok(None),
        Some(v) => Ok(Some(v.clone().try_into().context(
            "[registry] must have `url` and, optionally, `generation` and `refresh_secs`",
        )?)),
    }
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
            if slot.get("pricing").and_then(Value::as_str) != Some("enabled") {
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
        bail!("token file: no slot on chain {name} has pricing = \"enabled\"; refusing an empty universe");
    }
    tokens.sort_by_key(|t| t.get("symbol").and_then(Value::as_str).map(str::to_owned));

    Ok(Projection {
        chain_id,
        quote_token,
        tokens,
    })
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
    let rest = gs_url
        .strip_prefix("gs://")
        .with_context(|| format!("[registry] url {gs_url:?} is not a gs:// url"))?;
    let (bucket, object) = rest
        .split_once('/')
        .with_context(|| format!("[registry] url {gs_url:?} names no object"))?;

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
    let body = response
        .bytes()
        .await
        .context("reading the token file body")?;
    if !status.is_success() {
        bail!(
            "fetching {gs_url} returned {status}: {}",
            String::from_utf8_lossy(&body)
                .chars()
                .take(300)
                .collect::<String>()
        );
    }
    Ok(body.to_vec())
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
    let projection = project(&parse(&bytes)?, chain_id_of(config)?)?;
    merge(config, projection.clone())?;
    Ok(projection)
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

fn set_gauge(name: &'static str, on: bool) {
    metrics::gauge!(name).set(if on { 1.0 } else { 0.0 });
}

/// Re-read the token file on an interval and say whether this instance is
/// behind it, and whether boot would accept it. Reports only; never applies.
///
/// `static_config` is the config table as parsed from disk, before the
/// token rows were merged in, so each fresh file is checked exactly the way
/// boot checks it.
pub fn spawn_refresh(static_config: Table, live: Projection, source: RegistrySource) {
    tokio::spawn(async move {
        let http = http_client();
        let mut ticker = tokio::time::interval(Duration::from_secs(source.refresh_secs.max(5)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // first tick is immediate; boot just loaded it.
        let mut pending: Option<String> = None;
        loop {
            ticker.tick().await;

            let bytes = match fetch(&http, &source.url, None).await {
                Ok(b) => b,
                Err(e) => {
                    metrics::counter!("oracle_registry_fetch_errors_total").increment(1);
                    tracing::warn!(url = %source.url, error = %format!("{e:#}"), "token file: fetch failed; still running what boot loaded");
                    continue;
                }
            };

            let verdict = parse(&bytes)
                .and_then(|t| project(&t, live.chain_id))
                .and_then(|fresh| {
                    let mut candidate = static_config.clone();
                    merge(&mut candidate, fresh.clone())?;
                    Config::from_table(candidate)?;
                    Ok(fresh)
                });
            let fresh = match verdict {
                Ok(fresh) => {
                    set_gauge("oracle_registry_invalid", false);
                    fresh
                }
                Err(e) => {
                    set_gauge("oracle_registry_invalid", true);
                    tracing::error!(url = %source.url, error = %format!("{e:#}"), "token file: the bucket copy would be REFUSED at boot; fix it before the next restart");
                    continue;
                }
            };

            if fresh == live {
                if pending.take().is_some() {
                    tracing::info!("token file: bucket copy matches this instance again");
                }
                set_gauge("oracle_registry_pending_restart", false);
            } else {
                let change = describe_change(&live, &fresh);
                if pending.as_deref() != Some(&change) {
                    tracing::warn!(change = %change, "token file: bucket copy differs from what this instance runs; a release picks it up");
                    pending = Some(change);
                }
                set_gauge("oracle_registry_pending_restart", true);
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
        let chain_id = chain_id_of(&table)?;
        merge(&mut table, project(&parse(&bytes)?, chain_id)?)?;
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
    /// carried the day it was replaced. Staging drops wtSGOV, which the
    /// staging token file (like staging pricing) has switched off.
    #[test]
    fn registry_projects_to_the_inline_rows_it_replaced() {
        for (plane, env, dropped) in [
            ("production", "production", None),
            ("robinhood", "production", None),
            ("staging", "staging", Some("wtSGOV")),
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
            assert_eq!(p.rows(), expected, "{plane}");
        }
    }

    #[test]
    fn the_deployed_configs_load_through_the_registry() {
        for (plane, env, chain_id, count) in [
            ("production", "production", 8453, 47),
            ("robinhood", "production", 4663, 5),
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
}
