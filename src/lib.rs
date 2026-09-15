pub mod alpaca;
pub mod config;
pub mod market_hours;
pub mod metrics;
pub mod oracle;
pub mod pricing_client;
pub mod registry;
pub mod reuse;
pub mod sign;

use alloy::primitives::{Address, B256};
use alloy::sol;
use alloy::sol_types::SolValue;
use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use rain_math_float::Float;
use serde::Serialize;
use sign::Signer;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

use crate::market_hours::MarketHoursCache;
use crate::metrics::MetricsHandle;
use crate::pricing_client::{LiveClient, QuoteSnapshot};
use crate::registry::{PriceDirection, ResolvedPair, TokenRegistry};
use chrono::Utc;
use st0x_pricing_types::Quote;

trait Clock: Send + Sync {
    fn now_unix_ms(&self) -> i64;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> i64 {
        Utc::now().timestamp_millis()
    }
}

sol! {
    struct IOV2 {
        address token;
        bytes32 vaultId;
    }

    struct EvaluableV4 {
        address interpreter;
        address store;
        bytes bytecode;
    }

    struct OrderV4 {
        address owner;
        EvaluableV4 evaluable;
        IOV2[] validInputs;
        IOV2[] validOutputs;
        bytes32 nonce;
    }
}

/// Upstream (`rain.orderbook/crates/quote/src/oracle.rs`) posts one of
/// two ABI-encoded shapes:
/// - single: `(OrderV4, uint256 inputIOIndex, uint256 outputIOIndex, address counterparty)`
/// - batch:  `(OrderV4, uint256, uint256, address)[]`
///
/// We decode either. The response is always a JSON array of
/// `OracleResponse` whose length matches the number of requests.
type OracleRequestTuple = (
    OrderV4,
    alloy::primitives::U256,
    alloy::primitives::U256,
    Address,
);

pub struct AppState {
    signer: Signer,
    registry: TokenRegistry,
    /// The chain this deployment signs for, from config. Scopes the
    /// pricing quote cache (only this chain's frames are served) and is
    /// carried at slot 9 of `/context/v7`, inside the signature, so a
    /// strategy can reject a frame signed for another chain — see
    /// `oracle::SCHEMA_VERSION_V7`.
    chain_id: u64,
    /// Live WS subscription to st0x.pricing. Background-tasked, holds
    /// the latest `Quote` per symbol in an RwLock<HashMap>. Replaces
    /// the Alpaca polling cache (pre-RAI-360).
    pricing: LiveClient,
    /// Every symbol declared in config.toml. /status compares this
    /// against the pricing cache to surface the partial-serving set.
    configured_symbols: Vec<String>,
    /// Market-hours source from Alpaca's calendar, used ONLY to classify
    /// the current session for the v4/v5 session slots (tag +
    /// start/end bounds). `publish_time` comes from the pricing quote's
    /// own `source_ts_unix_ms`, not from this cache.
    market_hours: Arc<MarketHoursCache>,
    /// Prometheus exposition format renderer for `/metrics`.
    metrics: MetricsHandle,
    /// Cross-frame signature reuse for v5/v6/v7 (see `reuse`).
    reuse: reuse::ReuseCache,
    clock: Arc<dyn Clock>,
}

impl AppState {
    pub fn new(
        signer: Signer,
        registry: TokenRegistry,
        chain_id: u64,
        pricing: LiveClient,
        configured_symbols: Vec<String>,
        market_hours: Arc<MarketHoursCache>,
        metrics: MetricsHandle,
    ) -> Self {
        Self {
            signer,
            registry,
            chain_id,
            pricing,
            configured_symbols,
            market_hours,
            metrics,
            // Off until `with_signature_reuse` is called: `main.rs` passes
            // the configured margin, tests opt in explicitly so nothing
            // silently serves a previous frame.
            reuse: reuse::ReuseCache::new(0),
            clock: Arc::new(SystemClock),
        }
    }

    /// Set how many seconds a previous v5/v6/v7 quote must still have before
    /// its expiry to be served again instead of signing an unchanged price
    /// under a new publish_time. Zero disables the reuse.
    pub fn with_signature_reuse(mut self, min_remaining_secs: u64) -> Self {
        self.reuse = reuse::ReuseCache::new(min_remaining_secs);
        self
    }

    pub fn signer_address(&self) -> Address {
        self.signer.address()
    }

    #[cfg(test)]
    fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
}

pub fn create_app(state: AppState) -> Router {
    let shared_state = Arc::new(state);
    Router::new()
        .route("/", get(health))
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/context/v1", post(post_signed_context_v1))
        .route("/context/v4", post(post_signed_context_v4))
        .route("/context/v5", post(post_signed_context_v5))
        .route("/context/v6", post(post_signed_context_v6))
        .route("/context/v7", post(post_signed_context_v7))
        .layer(CorsLayer::permissive())
        .with_state(shared_state)
}

async fn health() -> &'static str {
    "ok"
}

async fn metrics(State(state): State<Arc<AppState>>) -> String {
    state.metrics.render()
}

#[derive(Serialize)]
struct StatusResponse {
    signer: String,
    configured_symbols: Vec<String>,
    missing_symbols: Vec<String>,
}

/// Operational status of the server. `/health` is for Fly liveness and
/// stays lenient ("ok" whenever the process is running). `/status` is
/// for ops/monitoring and reports the configured-vs-cached set so a
/// missing broker position is visible without parsing logs. Always
/// returns 200; consumers gate on the contents of `missing_symbols`.
async fn status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let missing = state.pricing.missing(&state.configured_symbols).await;
    // Side-effect: refresh coverage + freshness gauges every /status hit
    // so dashboards don't need a dedicated background tick. /status is
    // already on the obs scrape path, so this is free.
    ::metrics::gauge!("oracle_configured_symbols").set(state.configured_symbols.len() as f64);
    ::metrics::gauge!("oracle_missing_symbols").set(missing.len() as f64);
    if let Some(newest_ms) = state.pricing.newest_source_ts_ms().await {
        let age_secs = (Utc::now().timestamp_millis() - newest_ms) as f64 / 1000.0;
        ::metrics::gauge!("oracle_cache_freshness_seconds").set(age_secs);
    }
    Json(StatusResponse {
        signer: format!("{:?}", state.signer.address()),
        configured_symbols: state.configured_symbols.clone(),
        missing_symbols: missing,
    })
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
    detail: String,
}

/// Decode the POST body as either a single tuple or a batch array.
/// Returns a `Vec` in either case so downstream logic is uniform.
///
/// We try the batch form first because the empty-batch case (`[]`) is
/// a valid input upstream — returning an empty response array preserves
/// the "response length matches request length" contract. A batch
/// containing one element will also decode correctly here. Only when
/// the batch decoder rejects the body do we fall back to the single
/// tuple form (which is what most current callers send).
fn decode_request_body(body: &[u8]) -> Result<Vec<OracleRequestTuple>, AppError> {
    if let Ok(batch) = <Vec<OracleRequestTuple>>::abi_decode(body) {
        return Ok(batch);
    }
    let single = <OracleRequestTuple>::abi_decode(body)
        .map_err(|e| AppError::BadRequest(format!("Invalid ABI-encoded body: {}", e)))?;
    Ok(vec![single])
}

async fn post_signed_context_v1(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let result = post_signed_context_v1_inner(state, body).await;
    record_request_outcome("v1", &result);
    result
}

async fn post_signed_context_v1_inner(
    state: Arc<AppState>,
    body: Bytes,
) -> Result<axum::Json<Vec<oracle::OracleResponse>>, AppError> {
    let requests = decode_request_body(&body)?;

    if requests.is_empty() {
        return Ok(Json(Vec::<oracle::OracleResponse>::new()));
    }

    // Resolve every request's token pair first so we know which symbols
    // we need from the cache. This lets us take a single snapshot of
    // exactly those entries, so a poll loop update mid-iteration can't
    // mix quotes (or publish_time values) for the same symbol within
    // one HTTP response.
    let mut resolved: Vec<(OrderV4, ResolvedPair)> = Vec::with_capacity(requests.len());
    for (order, input_io_index, output_io_index, _counterparty) in requests {
        let pair = resolve_pair_for_order(&state, &order, input_io_index, output_io_index)?;
        resolved.push((order, pair));
    }

    let needed_symbols: Vec<&str> = resolved.iter().map(|(_, p)| p.symbol.as_str()).collect();
    let snapshot = state.pricing.snapshot_many(&needed_symbols).await;

    let mut responses = Vec::with_capacity(resolved.len());
    for (_, pair) in &resolved {
        let quote = snapshot
            .get(&pair.symbol)
            .ok_or_else(|| no_live_quote("v1", &pair.symbol))?;
        let resp = build_response_from_quote(&state, pair, quote).await?;
        responses.push(resp);
    }

    let validated_at_unix_ms = state.clock.now_unix_ms();
    for ((_, pair), response) in resolved.iter().zip(&responses) {
        let quote = snapshot
            .get(&pair.symbol)
            .ok_or_else(|| no_live_quote("v1", &pair.symbol))?;
        validate_quote_liveness(quote, "v1", &pair.symbol, RefusalPhase::BatchFinal)?;
        validate_expiry_deadline(
            response.validity_expiry_unix_ms,
            validated_at_unix_ms,
            "v1",
            &pair.symbol,
            false,
            RefusalPhase::BatchFinal,
        )?;
    }

    Ok(Json(
        responses
            .into_iter()
            .map(|response| response.response)
            .collect(),
    ))
}

/// Record a `/context/v{N}` request's outcome on the `oracle_context_request_total`
/// counter. `outcome` labels split into `ok` (signed responses returned),
/// `empty` (no requests in the body — Raindex's quote crate posts an empty
/// batch when an order's IO list is empty), and `error` (any `AppError`).
/// Keep the labels stable — the obs dashboard joins on these.
fn record_request_outcome(
    endpoint: &'static str,
    result: &Result<axum::Json<Vec<oracle::OracleResponse>>, AppError>,
) {
    let outcome = match result {
        Ok(json) if json.0.is_empty() => "empty",
        Ok(_) => "ok",
        Err(_) => "error",
    };
    ::metrics::counter!(
        "oracle_context_request_total",
        "endpoint" => endpoint,
        "outcome" => outcome,
    )
    .increment(1);
}

/// v4 handler — `/context/v4` endpoint. Same request shape and
/// snapshot-once batching as v1, plus the caller's raw
/// `validInputs[input_io_index].token` /
/// `validOutputs[output_io_index].token` addresses are stamped into
/// signed-context slots 6 and 7 respectively.
///
/// The security property: a v4 strategy that asserts
/// `equal-to(signed-context<0 6> input-token()) &&
/// equal-to(signed-context<0 7> output-token())` can no longer be
/// tricked into applying a signed price for pair `(A,B)` against an
/// order whose IO pair is `(C,D)`. See `oracle::SCHEMA_VERSION_V4`
/// for the full context layout.
async fn post_signed_context_v4(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let result = post_signed_context_pair_bound(state, body, PairSchema::V4).await;
    record_request_outcome("v4", &result);
    result
}

/// v5 handler — `/context/v5` endpoint. Identical request shape,
/// resolution and batching to v4; the response additionally signs the
/// pricing model's own expiry at slot 8.
///
/// The property v5 adds: a strategy no longer has to guess how long a
/// signed price is good for. `max-staleness` is a constant baked in when
/// the strategy was written, while the producer's binding horizon moves
/// with the asset, the session and the calibrated model. v5 lets the
/// strategy assert against the producer's own answer. See
/// `oracle::SCHEMA_VERSION_V5` for the full layout.
async fn post_signed_context_v5(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let result = post_signed_context_pair_bound(state, body, PairSchema::V5).await;
    record_request_outcome("v5", &result);
    result
}

/// v6 handler — `/context/v6` endpoint. Identical request shape,
/// resolution and batching to v4/v5; the response additionally signs
/// the vault NAV ratio the pricing model priced this quote against, as
/// a Rain Float at slot 9.
///
/// The property v6 adds: the base token is a share in a wrapped-token
/// vault whose NAV can step (e.g. on a dividend deposit), and a price
/// signed against one NAV but settled against another is stale in a
/// way no timestamp check can see. Slot 9 carries the on-chain
/// `convertToAssets(1e18)` value the model priced against, losslessly
/// packed as a Rain Float, so a v6 strategy can assert numeric equality
/// against the `erc4626-convert-to-assets` word at settlement. See
/// `oracle::SCHEMA_VERSION_V6` for the full layout, the encoding
/// rationale and the zero sentinel.
async fn post_signed_context_v6(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let result = post_signed_context_pair_bound(state, body, PairSchema::V6).await;
    record_request_outcome("v6", &result);
    result
}

/// v7 handler — `/context/v7` endpoint. Identical request shape,
/// resolution and batching to v4/v5/v6; the signed price at slot 1 is the
/// vault's UNDERLYING asset rate rather than the vault-share rate, no NAV
/// ratio is signed, and the deployment's chain id is signed at slot 9.
///
/// The property v7 changes: v6 signs the NAV ratio and forces a strategy
/// to assert exact equality against the vault's live answer at settlement
/// — a DoS surface (audit H03), because the NAV can step between sign and
/// settle for reasons outside any attacker's control and every step bricks
/// otherwise-valid frames. v7 signs only the underlying price and leaves
/// the ratio UNSIGNED; the consuming strategy (RAI-2199) reads the vault's
/// live `erc4626-convert-to-assets` on-chain and DERIVES the vault price
/// (`underlying × convertToAssets(1 share)`) atomically — nothing to
/// straddle. The underlying is the one quantity the chain can re-derive
/// the vault price from, so it is what must be signed.
///
/// The property v7 adds (RAI-1991): EIP-191 is chain-agnostic and no
/// other slot names a chain, while ST0x token addresses are deterministic
/// clones across chains — so a v6-and-below payload for chain A verifies
/// unchanged inside an order on chain B. Slot 9 carries this deployment's
/// `chain_id` inside the signature; a per-deployment
/// `equal-to(signed-context<0 9>() expected-chain-id)` binding closes the
/// gap in the strategy rather than in the URL wiring. See
/// `oracle::SCHEMA_VERSION_V7` for the full layout and the fail-closed
/// handling of an absent underlying rate.
async fn post_signed_context_v7(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let result = post_signed_context_pair_bound(state, body, PairSchema::V7).await;
    record_request_outcome("v7", &result);
    result
}

/// Which pair-bound schema a `/context/v4`, `/context/v5`, `/context/v6`
/// or `/context/v7` request is being served under. All four share request
/// decoding, registry resolution, snapshot-once batching and the session
/// snapshot; they differ in whether the model's expiry is signed into slot
/// 8 (v5, v6, v7), what slot 9 carries (the vault NAV ratio for v6, the
/// deployment's chain id for v7, nothing for v4/v5), and WHICH price is
/// signed into slot 1 — the vault-share rate for v4/v5/v6, the vault's
/// UNDERLYING asset rate for v7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairSchema {
    V4,
    V5,
    V6,
    V7,
}

impl PairSchema {
    fn tag(self) -> &'static str {
        match self {
            Self::V4 => "v4",
            Self::V5 => "v5",
            Self::V6 => "v6",
            Self::V7 => "v7",
        }
    }

    /// v7 signs the vault's UNDERLYING asset rate at slot 1; every other
    /// pair-bound schema signs the vault-share rate. This selects which of
    /// the cached quote's directional rate pairs the response is built
    /// from.
    fn signs_underlying(self) -> bool {
        matches!(self, Self::V7)
    }

    /// v5, v6 and v7 sign the model's expiry at slot 8; v4 signs only
    /// publish_time. Only the expiry-bearing schemas take part in
    /// cross-frame signature reuse (see `reuse`).
    fn signs_expiry(self) -> bool {
        !matches!(self, Self::V4)
    }
}

/// Shared body for `/context/v4`, `/context/v5`, `/context/v6` and
/// `/context/v7`.
async fn post_signed_context_pair_bound(
    state: Arc<AppState>,
    body: Bytes,
    schema: PairSchema,
) -> Result<axum::Json<Vec<oracle::OracleResponse>>, AppError> {
    let requests = decode_request_body(&body)?;

    if requests.is_empty() {
        return Ok(Json(Vec::<oracle::OracleResponse>::new()));
    }

    // Same resolution + batching shape as v1, but also keep the raw
    // input_token/output_token per request so we can bind them into the
    // signed context — that binding is the whole point of v4.
    let mut resolved: Vec<(Address, Address, ResolvedPair)> = Vec::with_capacity(requests.len());
    for (order, input_io_index, output_io_index, _counterparty) in requests {
        let (input_token, output_token) = io_tokens_for(&order, input_io_index, output_io_index)?;
        let pair = state
            .registry
            .resolve(input_token, output_token)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        tracing::info!(
            symbol = %pair.symbol,
            direction = pair.direction.as_str(),
            input = %input_token,
            output = %output_token,
            schema = schema.tag(),
            "Oracle request"
        );
        resolved.push((input_token, output_token, pair));
    }

    let needed_symbols: Vec<&str> = resolved.iter().map(|(_, _, p)| p.symbol.as_str()).collect();
    let snapshot = state.pricing.snapshot_many(&needed_symbols).await;

    // Session classification is snapshot once per batch; publish_time is
    // per-quote (the pricing quote's own source_ts), read inside the builder.
    let session_info = state.market_hours.session_info_for(Utc::now()).await;

    let mut responses = Vec::with_capacity(resolved.len());
    for (input_token, output_token, pair) in &resolved {
        let quote = snapshot
            .get(&pair.symbol)
            .ok_or_else(|| no_live_quote(schema.tag(), &pair.symbol))?;
        let resp = build_response_from_quote_pair_bound(
            &state,
            pair,
            quote,
            *input_token,
            *output_token,
            &session_info,
            schema,
        )
        .await?;
        responses.push(resp);
    }

    let validated_at_unix_ms = state.clock.now_unix_ms();
    for ((_, _, pair), response) in resolved.iter().zip(&responses) {
        let quote = snapshot
            .get(&pair.symbol)
            .ok_or_else(|| no_live_quote(schema.tag(), &pair.symbol))?;
        validate_quote_liveness(quote, schema.tag(), &pair.symbol, RefusalPhase::BatchFinal)?;
        validate_expiry_deadline(
            response.validity_expiry_unix_ms,
            validated_at_unix_ms,
            schema.tag(),
            &pair.symbol,
            schema.signs_expiry(),
            RefusalPhase::BatchFinal,
        )?;
    }

    Ok(Json(
        responses
            .into_iter()
            .map(|response| response.response)
            .collect(),
    ))
}

/// Extract the raw `(input_token, output_token)` addresses that the
/// caller nominated in this request's `OrderV4`. Same bounds checks
/// as `resolve_pair_for_order`, minus the registry lookup — the two
/// helpers pull from the same source but v4 keeps the addresses even
/// after they've been resolved to a symbol.
fn io_tokens_for(
    order: &OrderV4,
    input_io_index: alloy::primitives::U256,
    output_io_index: alloy::primitives::U256,
) -> Result<(Address, Address), AppError> {
    let input_idx: usize = input_io_index.try_into().unwrap_or(usize::MAX);
    let output_idx: usize = output_io_index.try_into().unwrap_or(usize::MAX);

    let input_token = order
        .validInputs
        .get(input_idx)
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "Invalid input IO index: {} (order has {} inputs)",
                input_idx,
                order.validInputs.len()
            ))
        })?
        .token;

    let output_token = order
        .validOutputs
        .get(output_idx)
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "Invalid output IO index: {} (order has {} outputs)",
                output_idx,
                order.validOutputs.len()
            ))
        })?
        .token;

    Ok((input_token, output_token))
}

/// Decode a request's IO indices into the actual input/output addresses
/// and look them up in the token registry. Pure: never touches the cache.
fn resolve_pair_for_order(
    state: &AppState,
    order: &OrderV4,
    input_io_index: alloy::primitives::U256,
    output_io_index: alloy::primitives::U256,
) -> Result<ResolvedPair, AppError> {
    let input_idx: usize = input_io_index.try_into().unwrap_or(usize::MAX);
    let output_idx: usize = output_io_index.try_into().unwrap_or(usize::MAX);

    let input_token = order
        .validInputs
        .get(input_idx)
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "Invalid input IO index: {} (order has {} inputs)",
                input_idx,
                order.validInputs.len()
            ))
        })?
        .token;

    let output_token = order
        .validOutputs
        .get(output_idx)
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "Invalid output IO index: {} (order has {} outputs)",
                output_idx,
                order.validOutputs.len()
            ))
        })?
        .token;

    let pair: ResolvedPair = state
        .registry
        .resolve(input_token, output_token)
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    tracing::info!(
        symbol = %pair.symbol,
        direction = pair.direction.as_str(),
        input = %input_token,
        output = %output_token,
        "Oracle request"
    );

    Ok(pair)
}

/// Pick the maker-side rate for this request's swap direction from a
/// live pricing-service `Quote`.
///
/// Per the pricing-types contract, each rate is DIRECTIONAL: it is "the
/// price the model would honour for an input of the named token going
/// to an output of the other". A swap where the taker puts `quote` in
/// and takes `base` out is priced by `rate_quote_to_base`; base-in /
/// quote-out is priced by `rate_base_to_quote`. Each carries that
/// direction's own spread — consumers must never use the opposite
/// rate to dodge a unit conversion.
///
/// A Raindex order's `ratio` is `input_amount / output_amount` (units
/// of inputToken per outputToken). An order with input=quote /
/// output=base is the venue where takers swap quote→base, so its price
/// is `rate_quote_to_base` — but in `quote per base` units, i.e.
/// INVERTED. Same for the other direction. That `1/x` is exactly the
/// "unit conversion at a protocol-adapter boundary" the pricing-types
/// doc blesses; it does not touch the spread decision.
///
/// History, because this exact function shipped wrong: the original
/// implementation picked the UNIT-compatible rate instead of the
/// DIRECTION-compatible one (`QuoteToBase → rate_base_to_quote`, no
/// inversion). Units lined up, so everything parsed and every parity
/// check passed — but each rate carries the OTHER direction's spread,
/// so every sell order quoted the bid and every buy order the ask.
/// The first deployed 0trade order pair read as crossed by exactly
/// 2x the session spread (2026-08-07, caught in pre-migration review;
/// zero funded orders ever traded on it). The regression tests pin
/// maker orientation: a sell-side request must price ABOVE the
/// buy-side request for the same pair.
fn pick_rate_bytes(quote: &Quote, direction: PriceDirection) -> Result<[u8; 32], anyhow::Error> {
    let (directional_rate, name) = match direction {
        // Order input=quote, output=base: takers swap quote->base; the
        // honoured rate is base-per-quote, inverted into ratio units.
        PriceDirection::QuoteToBase => (quote.rate_quote_to_base.0, "rate_quote_to_base"),
        // Order input=base, output=quote: takers swap base->quote.
        PriceDirection::BaseToQuote => (quote.rate_base_to_quote.0, "rate_base_to_quote"),
    };
    let inverted = Float::from_raw(B256::new(directional_rate))
        .inv()
        .map_err(|e| anyhow::anyhow!("Failed to invert {name} into ratio units: {e:?}"))?;
    Ok(B256::from(inverted).0)
}

/// Pick the maker-side UNDERLYING rate for this request's swap direction
/// from a live pricing-service `Quote` — the v7 analogue of
/// `pick_rate_bytes`.
///
/// The underlying rate prices the vault's ERC4626 underlying asset (the
/// offchain stock) rather than the vault share. It is DIRECTIONAL and
/// spread-carrying exactly like the vault rate, and is picked by the same
/// input/output orientation and inverted into Raindex ratio units the same
/// way — so slot 1 of a v7 context is the underlying-price counterpart of
/// slot 1 of a v5/v6 context. The consuming strategy multiplies this by
/// the vault's LIVE NAV ratio on-chain to derive the vault price, so the
/// oracle never signs the ratio.
///
/// Fail-closed on the all-zero sentinel: a pricing producer predating the
/// `underlying_rate_*` fields decodes them to the zero Float via
/// `#[serde(default)]`, and a real stock rate is never zero. Signing (or
/// inverting) a zero underlying price would hand the strategy a garbage
/// mark, so the request is refused. This mirrors how the server treats any
/// other unusable rate — an internal error, fail-closed — rather than the
/// v6 NAV-ratio path, where zero is a legitimate "no assertion" sentinel
/// the strategy is free to accept; a zero underlying price is never usable.
fn pick_underlying_rate_bytes(
    quote: &Quote,
    direction: PriceDirection,
) -> Result<[u8; 32], anyhow::Error> {
    let (directional_rate, name) = match direction {
        PriceDirection::QuoteToBase => (
            quote.underlying_rate_quote_to_base.0,
            "underlying_rate_quote_to_base",
        ),
        PriceDirection::BaseToQuote => (
            quote.underlying_rate_base_to_quote.0,
            "underlying_rate_base_to_quote",
        ),
    };
    let rate = Float::from_raw(B256::new(directional_rate));
    if rate
        .is_zero()
        .map_err(|e| anyhow::anyhow!("Failed to test {name} for zero: {e:?}"))?
    {
        return Err(anyhow::anyhow!(
            "{name} is the zero sentinel: the pricing producer carried no underlying rate for \
             this quote (it predates underlying pricing, or the pair is misconfigured); refusing \
             to sign a zero underlying price"
        ));
    }
    let inverted = rate
        .inv()
        .map_err(|e| anyhow::anyhow!("Failed to invert {name} into ratio units: {e:?}"))?;
    Ok(B256::from(inverted).0)
}

/// Build a signed response from a pre-resolved pair and a snapshotted
/// `Quote`. All `Quote`s for one batch must come from a single
/// `LiveClient::snapshot_many` so a concurrent WS push can't mix prices
/// across elements of the same response.
///
/// The pricing service publishes both swap directions independently,
/// already including its spread; the oracle just picks the rate that
/// matches the request's direction and signs the 32-byte Rain Float
/// with the single Rain-Float inversion from `pick_rate_bytes` — no
/// f64 round-trip, no extra spread.
///
/// `publish_time` is the pricing quote's own `source_ts_unix_ms` — the
/// honest as-of instant st0x.pricing already stamped on the mark (the
/// fetch time inside a session, the last `session_close` out-of-session;
/// RAI-732). The oracle signs that straight through rather than
/// re-deriving a timestamp from its own clock: pricing owns the
/// market-hours truth, and trusting its `source_ts` means a stalled or
/// frozen pricing feed surfaces directly — `source_ts` stops advancing,
/// the signed timestamp goes stale, and the strategy's `max-staleness`
/// rejects. The oracle's own `MarketHoursCache` is used only for the
/// v4/v5 session slots, never for `publish_time`.
///
/// One deliberate exception: while a v5/v6/v7 price is unchanged, the
/// `reuse` layer serves the previous frame's signature, whose
/// `publish_time` can trail the live frame by up to the expiry horizon
/// minus the reuse margin (~20s today). That lag is bounded and visible
/// in the signed bytes; a stalled feed still shows as `source_ts`
/// stopping altogether.
/// Derive the signed `publish_time` (Unix seconds) from a pricing-service
/// `Quote.source_ts_unix_ms` (Unix milliseconds). st0x.pricing already
/// stamps `source_ts` with the mark's honest as-of instant (RAI-732), so
/// the oracle just converts ms → s and signs it.
fn publish_time_from_quote(quote: &Quote) -> Result<u64, AppError> {
    u64::try_from(quote.source_ts_unix_ms / 1000)
        .map_err(|_| AppError::Internal(anyhow::anyhow!("source_ts out of range")))
}

/// The model's binding horizon for this quote, in whole Unix seconds.
///
/// Integer division floors for the non-negative values that survive the
/// conversion (for negatives it truncates toward zero, but `try_from`
/// rejects those first) — the safe direction: the signed expiry can
/// only ever land at or before the model's real one, never
/// past it. A negative or out-of-range value fails the request rather
/// than clamping — a defaulted expiry is a defaulted licence to keep
/// trading on a price the model has disowned, and the whole point of v5
/// is that this number is trustworthy.
fn expiry_from_quote(quote: &Quote) -> Result<u64, AppError> {
    u64::try_from(quote.expiry_unix_ms / 1000)
        .map_err(|_| AppError::Internal(anyhow::anyhow!("quote expiry out of range")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefusalPhase {
    Admission,
    PreSign,
    PostSign,
    PreReuseReturn,
    BatchFinal,
}

impl RefusalPhase {
    const fn label(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::PreSign => "pre_sign",
            Self::PostSign => "post_sign",
            Self::PreReuseReturn => "pre_reuse_return",
            Self::BatchFinal => "batch_final",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableReason {
    NoLiveQuote,
    ExpiredQuote,
}

impl UnavailableReason {
    const fn code(self) -> &'static str {
        match self {
            Self::NoLiveQuote => "no_live_quote",
            Self::ExpiredQuote => "expired_quote",
        }
    }
}

fn unavailable(
    reason: UnavailableReason,
    endpoint: &'static str,
    symbol: &str,
    phase: RefusalPhase,
    detail: String,
) -> AppError {
    ::metrics::counter!(
        "oracle_quote_refusals_total",
        "reason" => reason.code(),
        "endpoint" => endpoint,
        "symbol" => symbol.to_owned(),
        "phase" => phase.label(),
    )
    .increment(1);
    AppError::Unavailable { reason, detail }
}

fn no_live_quote_at(endpoint: &'static str, symbol: &str, phase: RefusalPhase) -> AppError {
    let reason = UnavailableReason::NoLiveQuote;
    let detail = format!("No live quote for {symbol}.");
    tracing::warn!(
        reason = reason.code(),
        endpoint,
        symbol,
        phase = phase.label(),
        %detail,
        "Refusing oracle quote request"
    );
    unavailable(reason, endpoint, symbol, phase, detail)
}

fn no_live_quote(endpoint: &'static str, symbol: &str) -> AppError {
    no_live_quote_at(endpoint, symbol, RefusalPhase::Admission)
}

fn validate_quote_liveness(
    quote: &QuoteSnapshot,
    endpoint: &'static str,
    symbol: &str,
    phase: RefusalPhase,
) -> Result<(), AppError> {
    if quote.is_live() {
        Ok(())
    } else {
        Err(no_live_quote_at(endpoint, symbol, phase))
    }
}

fn validate_expiry_deadline(
    expiry_unix_ms: i64,
    checked_at_unix_ms: i64,
    endpoint: &'static str,
    symbol: &str,
    signs_expiry: bool,
    phase: RefusalPhase,
) -> Result<(), AppError> {
    if quote_is_live(expiry_unix_ms, checked_at_unix_ms, signs_expiry) {
        return Ok(());
    }

    tracing::warn!(
        reason = UnavailableReason::ExpiredQuote.code(),
        phase = phase.label(),
        endpoint,
        symbol,
        expiry_unix_ms,
        checked_at_unix_ms,
        "Refusing expired pricing quote"
    );
    Err(unavailable(
        UnavailableReason::ExpiredQuote,
        endpoint,
        symbol,
        phase,
        format!(
            "Quote for {symbol} expired at {expiry_unix_ms} (checked at {checked_at_unix_ms})."
        ),
    ))
}

fn validate_quote_expiry(
    quote: &Quote,
    checked_at_unix_ms: i64,
    endpoint: &'static str,
    symbol: &str,
    signs_expiry: bool,
    phase: RefusalPhase,
) -> Result<(), AppError> {
    validate_expiry_deadline(
        quote.expiry_unix_ms,
        checked_at_unix_ms,
        endpoint,
        symbol,
        signs_expiry,
        phase,
    )
}

fn quote_is_live(expiry_unix_ms: i64, checked_at_unix_ms: i64, signs_expiry: bool) -> bool {
    expiry_unix_ms > checked_at_unix_ms
        && (!signs_expiry || expiry_unix_ms.div_euclid(1000) > checked_at_unix_ms.div_euclid(1000))
}

#[derive(Debug)]
struct BuiltResponse {
    response: oracle::OracleResponse,
    /// The effective deadline of the response itself. For a freshly signed
    /// response this comes from the source quote; for reuse it is the older
    /// expiry encoded in the stored signed context.
    validity_expiry_unix_ms: i64,
}

async fn build_response_from_quote(
    state: &AppState,
    pair: &ResolvedPair,
    quote: &QuoteSnapshot,
) -> Result<BuiltResponse, AppError> {
    validate_quote_liveness(quote, "v1", &pair.symbol, RefusalPhase::Admission)?;
    validate_quote_expiry(
        quote,
        state.clock.now_unix_ms(),
        "v1",
        &pair.symbol,
        false,
        RefusalPhase::Admission,
    )?;
    let publish_time = publish_time_from_quote(quote)?;

    let price_bytes = pick_rate_bytes(quote, pair.direction).map_err(AppError::Internal)?;

    let context = oracle::build_context(price_bytes, publish_time)?;
    validate_quote_liveness(quote, "v1", &pair.symbol, RefusalPhase::PreSign)?;
    validate_quote_expiry(
        quote,
        state.clock.now_unix_ms(),
        "v1",
        &pair.symbol,
        false,
        RefusalPhase::PreSign,
    )?;
    let (signature, signer) = state.signer.sign_context(&context).await?;

    let validated_at_unix_ms = state.clock.now_unix_ms();
    validate_quote_liveness(quote, "v1", &pair.symbol, RefusalPhase::PostSign)?;
    validate_quote_expiry(
        quote,
        validated_at_unix_ms,
        "v1",
        &pair.symbol,
        false,
        RefusalPhase::PostSign,
    )?;

    tracing::info!(
        symbol = %pair.symbol,
        direction = pair.direction.as_str(),
        schema = "v1",
        publish_time,
        source_ts_unix_ms = quote.source_ts_unix_ms,
        expiry_unix_ms = quote.expiry_unix_ms,
        validated_at_unix_ms,
        "Building signed context from live pricing quote"
    );

    Ok(BuiltResponse {
        response: oracle::OracleResponse {
            signer,
            context,
            signature,
        },
        validity_expiry_unix_ms: quote.expiry_unix_ms,
    })
}

/// Pair-bound response builder (v4/v5/v6/v7). Same publish_time logic as
/// v1's `build_response_from_quote`, plus the session slots and the
/// caller's raw input/output token addresses stamped into signed-context
/// slots 6 and 7; v5/v6/v7 add the quote expiry at slot 8 and v6 the vault
/// NAV ratio at slot 9. Slot 1 is the vault-share rate for v4/v5/v6 and the
/// vault's underlying rate for v7 (see the `oracle::build_context_v*`
/// builders for the layouts).
#[allow(clippy::too_many_arguments)]
async fn build_response_from_quote_pair_bound(
    state: &AppState,
    pair: &ResolvedPair,
    quote: &QuoteSnapshot,
    input_token: Address,
    output_token: Address,
    session_info: &crate::market_hours::SessionInfo,
    schema: PairSchema,
) -> Result<BuiltResponse, AppError> {
    validate_quote_liveness(quote, schema.tag(), &pair.symbol, RefusalPhase::Admission)?;
    validate_quote_expiry(
        quote,
        state.clock.now_unix_ms(),
        schema.tag(),
        &pair.symbol,
        schema.signs_expiry(),
        RefusalPhase::Admission,
    )?;
    // publish_time is the pricing quote's source_ts (see
    // `build_response_from_quote`); session slots come from the oracle's
    // own market-hours classification.
    let publish_time = publish_time_from_quote(quote)?;
    let session_start: u64 = session_info
        .start
        .timestamp()
        .try_into()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("session_start out of range")))?;
    let session_end: u64 = session_info
        .end
        .timestamp()
        .try_into()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("session_end out of range")))?;

    // Pick the directional rate and invert it into Raindex ratio units
    // (Rain-Float precision). v4/v5/v6 sign the vault-share rate
    // (`pick_rate_bytes`); v7 signs the vault's UNDERLYING asset rate
    // (`pick_underlying_rate_bytes`), which fails closed on the absent-rate
    // sentinel. Both come off the SAME cached quote.
    let price_bytes = if schema.signs_underlying() {
        pick_underlying_rate_bytes(quote, pair.direction)
    } else {
        pick_rate_bytes(quote, pair.direction)
    }
    .map_err(AppError::Internal)?;

    // Build the context first: it is cheap (no KMS), and the reuse layer
    // compares the built slots rather than a hand-kept list of them.
    let session_bytes = session_info.session.to_bytes32_v3();
    let expiry = if schema.signs_expiry() {
        Some(expiry_from_quote(quote)?)
    } else {
        None
    };
    let context = match (schema, expiry) {
        (PairSchema::V4, _) => oracle::build_context_v4(
            price_bytes,
            publish_time,
            session_bytes,
            session_start,
            session_end,
            input_token,
            output_token,
        )?,
        (PairSchema::V5, Some(expiry)) => oracle::build_context_v5(
            price_bytes,
            publish_time,
            session_bytes,
            session_start,
            session_end,
            input_token,
            output_token,
            expiry,
        )?,
        // The NAV ratio is read off the SAME `quote` as the rate at
        // slot 1 — both came out of one `snapshot_many` entry, and the
        // pricing client only ever stores whole frames — so the signed
        // context can never pair a rate from one frame with a ratio
        // from another.
        (PairSchema::V6, Some(expiry)) => oracle::build_context_v6(
            price_bytes,
            publish_time,
            session_bytes,
            session_start,
            session_end,
            input_token,
            output_token,
            expiry,
            quote.nav_ratio.0,
        )?,
        // v7 signs the UNDERLYING price at slot 1 (already selected into
        // `price_bytes` above) and NO NAV ratio — the strategy derives the
        // vault price on-chain from the live ratio (RAI-2198) — plus this
        // deployment's chain id at slot 9 (RAI-1991). The chain id is the
        // same one the quote cache is scoped by, so the slot names the
        // chain whose frames produced the price.
        (PairSchema::V7, Some(expiry)) => oracle::build_context_v7(
            price_bytes,
            publish_time,
            session_bytes,
            session_start,
            session_end,
            input_token,
            output_token,
            expiry,
            state.chain_id,
        )?,
        (_, None) => {
            return Err(AppError::Internal(anyhow::anyhow!(
                "{} signs an expiry but none was derived from the quote",
                schema.tag()
            )))
        }
    };

    // v5/v6/v7: if the previous signature for this pair states the same
    // price under the same session for the same tokens, does not outlive
    // this frame's expiry, and is still good for the configured margin,
    // serve it instead of signing an unchanged price under a new
    // publish_time. See the `reuse` module.
    let reuse = expiry.map(|expiry| {
        (
            reuse::ReuseKey {
                schema: schema.tag(),
                symbol: pair.symbol.clone(),
                direction: pair.direction.as_str(),
                input_token,
                output_token,
            },
            expiry,
        )
    });
    if let Some((key, expiry)) = &reuse {
        validate_quote_liveness(quote, schema.tag(), &pair.symbol, RefusalPhase::PreSign)?;
        let now_unix_ms = state.clock.now_unix_ms();
        let now_secs = u64::try_from(now_unix_ms.div_euclid(1000))
            .map_err(|_| AppError::Internal(anyhow::anyhow!("system clock before 1970")))?;
        if let Some(previous) =
            state
                .reuse
                .lookup(key, &context, *expiry, now_secs, quote.generation())
        {
            let validated_at_unix_ms = state.clock.now_unix_ms();
            validate_quote_liveness(
                quote,
                schema.tag(),
                &pair.symbol,
                RefusalPhase::PreReuseReturn,
            )?;
            let stored_expiry_unix_ms = i64::try_from(previous.expiry_unix_secs)
                .ok()
                .and_then(|expiry| expiry.checked_mul(1000))
                .ok_or_else(|| {
                    AppError::Internal(anyhow::anyhow!("stored quote expiry out of range"))
                })?;
            validate_expiry_deadline(
                stored_expiry_unix_ms,
                validated_at_unix_ms,
                schema.tag(),
                &pair.symbol,
                true,
                RefusalPhase::PreReuseReturn,
            )?;
            ::metrics::counter!("oracle_signature_reuse_total").increment(1);
            tracing::info!(
                symbol = %pair.symbol,
                direction = pair.direction.as_str(),
                schema = schema.tag(),
                input = %input_token,
                output = %output_token,
                candidate_publish_time = publish_time,
                session = session_info.session.as_str(),
                session_start,
                session_end,
                source_ts_unix_ms = quote.source_ts_unix_ms,
                expiry_unix_ms = stored_expiry_unix_ms,
                validated_at_unix_ms,
                "Price unchanged and previous quote still valid; reusing its signature"
            );
            return Ok(BuiltResponse {
                response: previous.response,
                validity_expiry_unix_ms: stored_expiry_unix_ms,
            });
        }
    }

    validate_quote_liveness(quote, schema.tag(), &pair.symbol, RefusalPhase::PreSign)?;
    validate_quote_expiry(
        quote,
        state.clock.now_unix_ms(),
        schema.tag(),
        &pair.symbol,
        schema.signs_expiry(),
        RefusalPhase::PreSign,
    )?;

    let (signature, signer) = state.signer.sign_context(&context).await?;

    let validated_at_unix_ms = state.clock.now_unix_ms();
    validate_quote_liveness(quote, schema.tag(), &pair.symbol, RefusalPhase::PostSign)?;
    validate_quote_expiry(
        quote,
        validated_at_unix_ms,
        schema.tag(),
        &pair.symbol,
        schema.signs_expiry(),
        RefusalPhase::PostSign,
    )?;

    let response = oracle::OracleResponse {
        signer,
        context,
        signature,
    };
    if let Some((key, expiry)) = reuse {
        state
            .reuse
            .store(key, expiry, response.clone(), quote.generation());
    }
    tracing::info!(
        symbol = %pair.symbol,
        direction = pair.direction.as_str(),
        schema = schema.tag(),
        input = %input_token,
        output = %output_token,
        publish_time,
        session = session_info.session.as_str(),
        session_start,
        session_end,
        source_ts_unix_ms = quote.source_ts_unix_ms,
        expiry_unix_ms = quote.expiry_unix_ms,
        validated_at_unix_ms,
        "Building pair-bound signed context from live pricing quote"
    );
    Ok(BuiltResponse {
        response,
        validity_expiry_unix_ms: quote.expiry_unix_ms,
    })
}

#[derive(Debug)]
pub enum AppError {
    Internal(anyhow::Error),
    BadRequest(String),
    /// The server is alive but the poll loop hasn't produced a quote yet
    /// for the requested symbol. Distinct from BadRequest because it's transient
    /// and retrying may succeed.
    Unavailable {
        reason: UnavailableReason,
        detail: String,
    },
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        match self {
            AppError::Internal(err) => {
                tracing::error!("Internal error: {:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: "internal_error".to_string(),
                        detail: format!("{}", err),
                    }),
                )
                    .into_response()
            }
            AppError::BadRequest(detail) => {
                tracing::warn!("Bad request: {}", detail);
                (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "bad_request".to_string(),
                        detail,
                    }),
                )
                    .into_response()
            }
            AppError::Unavailable { reason, detail } => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: reason.code().to_string(),
                    detail,
                }),
            )
                .into_response(),
        }
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err)
    }
}

#[cfg(test)]
mod expiry_tests {
    use super::*;
    use crate::market_hours::{Session, SessionInfo};
    use crate::registry::PriceDirection;
    use st0x_pricing_types::{
        ErrorCode, ErrorFrame, ServerFrame, WireAddress, WireFloat, WireU256,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    struct SequenceClock {
        values: Mutex<(VecDeque<i64>, i64)>,
    }

    impl SequenceClock {
        fn new(values: impl IntoIterator<Item = i64>) -> Self {
            let values: VecDeque<_> = values.into_iter().collect();
            let fallback = *values.back().expect("clock needs at least one value");
            Self {
                values: Mutex::new((values, fallback)),
            }
        }
    }

    impl Clock for SequenceClock {
        fn now_unix_ms(&self) -> i64 {
            let mut guard = self.values.lock().unwrap();
            guard.0.pop_front().unwrap_or(guard.1)
        }
    }

    fn test_quote(expiry_unix_ms: i64) -> Quote {
        let rate: B256 = Float::parse("100".to_string()).unwrap().into();
        Quote {
            asset: "COIN".to_string(),
            chain_id: 1,
            base: WireAddress::from_bytes([0x11; 20]),
            quote: WireAddress::from_bytes([0x22; 20]),
            rate_base_to_quote: WireFloat::from_bytes(rate.into()),
            rate_quote_to_base: WireFloat::from_bytes(rate.into()),
            expiry_unix_ms,
            source_ts_unix_ms: 1_000,
            nav_ratio: WireU256::ZERO,
            underlying_rate_base_to_quote: WireFloat::from_bytes(rate.into()),
            underlying_rate_quote_to_base: WireFloat::from_bytes(rate.into()),
        }
    }

    fn test_snapshot(expiry_unix_ms: i64) -> QuoteSnapshot {
        QuoteSnapshot::always_live(test_quote(expiry_unix_ms))
    }

    async fn test_state(clock: Arc<dyn Clock>, reuse_secs: u64) -> AppState {
        AppState::new(
            Signer::new(TEST_KEY).unwrap(),
            TokenRegistry::new(
                vec![(
                    "0x1111111111111111111111111111111111111111".into(),
                    "COIN".into(),
                )],
                "0x2222222222222222222222222222222222222222",
            )
            .unwrap(),
            1,
            LiveClient::with_seeded(vec![], 1).await,
            vec!["COIN".into()],
            Arc::new(MarketHoursCache::new()),
            MetricsHandle::install().unwrap(),
        )
        .with_signature_reuse(reuse_secs)
        .with_clock(clock)
    }

    fn pair() -> ResolvedPair {
        ResolvedPair {
            symbol: "COIN".into(),
            direction: PriceDirection::BaseToQuote,
        }
    }

    fn request_tuple(input_token: Address, output_token: Address) -> OracleRequestTuple {
        (
            OrderV4 {
                owner: Address::ZERO,
                evaluable: EvaluableV4 {
                    interpreter: Address::ZERO,
                    store: Address::ZERO,
                    bytecode: alloy::primitives::Bytes::new(),
                },
                validInputs: vec![IOV2 {
                    token: input_token,
                    vaultId: B256::ZERO,
                }],
                validOutputs: vec![IOV2 {
                    token: output_token,
                    vaultId: B256::ZERO,
                }],
                nonce: B256::ZERO,
            },
            alloy::primitives::U256::ZERO,
            alloy::primitives::U256::ZERO,
            Address::ZERO,
        )
    }

    fn request_body(requests: Vec<OracleRequestTuple>) -> Bytes {
        Bytes::from(requests.abi_encode())
    }

    fn two_symbol_quotes(first_expiry: i64, second_expiry: i64) -> Vec<Quote> {
        let first = test_quote(first_expiry);
        let mut second = test_quote(second_expiry);
        second.asset = "DRAM".into();
        let second_rate: B256 = Float::parse("50".to_string()).unwrap().into();
        second.rate_base_to_quote = WireFloat::from_bytes(second_rate.into());
        second.rate_quote_to_base = WireFloat::from_bytes(second_rate.into());
        vec![first, second]
    }

    async fn two_symbol_state(clock: Arc<dyn Clock>, quotes: Vec<Quote>) -> Arc<AppState> {
        Arc::new(
            AppState::new(
                Signer::new(TEST_KEY).unwrap(),
                TokenRegistry::new(
                    vec![
                        (
                            "0x1111111111111111111111111111111111111111".into(),
                            "COIN".into(),
                        ),
                        (
                            "0x3333333333333333333333333333333333333333".into(),
                            "DRAM".into(),
                        ),
                    ],
                    "0x2222222222222222222222222222222222222222",
                )
                .unwrap(),
                1,
                LiveClient::with_seeded(quotes, 1).await,
                vec!["COIN".into(), "DRAM".into()],
                Arc::new(MarketHoursCache::new()),
                MetricsHandle::install().unwrap(),
            )
            .with_clock(clock),
        )
    }

    fn two_symbol_request_body() -> Bytes {
        request_body(vec![
            request_tuple(Address::from([0x22; 20]), Address::from([0x11; 20])),
            request_tuple(Address::from([0x22; 20]), Address::from([0x33; 20])),
        ])
    }

    #[test]
    fn raw_millisecond_deadline_is_exclusive_without_flooring() {
        let now = 1_700_000_000_500;
        assert!(!quote_is_live(now - 1, now, false));
        assert!(!quote_is_live(now, now, false));
        assert!(quote_is_live(now + 1, now, false));
        assert!(quote_is_live(i64::MAX, now, false));
    }

    #[test]
    fn expiry_bearing_schemas_require_a_future_whole_second() {
        let now = 1_700_000_000_500;
        assert!(quote_is_live(now + 1, now, false), "v1/v4 use raw ms");
        assert!(
            !quote_is_live(now + 499, now, true),
            "same encoded second is already ineffective on chain"
        );
        assert!(quote_is_live(1_700_000_001_000, now, true));
        assert!(quote_is_live(i64::MAX, now, true));
    }

    #[test]
    fn subsecond_expiry_boundary_is_mapped_to_every_schema() {
        let now = 1_700_000_000_500;
        let raw_live_same_second = now + 1;

        assert!(quote_is_live(raw_live_same_second, now, false), "v1");
        assert!(quote_is_live(
            raw_live_same_second,
            now,
            PairSchema::V4.signs_expiry()
        ));
        for schema in [PairSchema::V5, PairSchema::V6, PairSchema::V7] {
            assert!(
                !quote_is_live(raw_live_same_second, now, schema.signs_expiry()),
                "{} must require a future encoded expiry second",
                schema.tag()
            );
        }
    }

    #[test]
    fn unavailable_reason_codes_are_stable() {
        assert_eq!(UnavailableReason::NoLiveQuote.code(), "no_live_quote");
        assert_eq!(UnavailableReason::ExpiredQuote.code(), "expired_quote");
    }

    #[tokio::test]
    async fn expiry_crossing_during_signing_is_refused_post_sign() {
        let state = test_state(Arc::new(SequenceClock::new([1_000, 1_000, 2_000])), 0).await;
        let error = build_response_from_quote(&state, &pair(), &test_snapshot(2_000))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AppError::Unavailable {
                reason: UnavailableReason::ExpiredQuote,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn stale_source_revokes_snapshot_while_signature_is_in_flight() {
        let pricing = LiveClient::with_seeded(vec![test_quote(i64::MAX)], 1).await;
        let state = Arc::new(
            AppState::new(
                Signer::new(TEST_KEY)
                    .unwrap()
                    .with_test_delay(Duration::from_millis(100)),
                TokenRegistry::new(
                    vec![(
                        "0x1111111111111111111111111111111111111111".into(),
                        "COIN".into(),
                    )],
                    "0x2222222222222222222222222222222222222222",
                )
                .unwrap(),
                1,
                pricing.clone(),
                vec!["COIN".into()],
                Arc::new(MarketHoursCache::new()),
                MetricsHandle::install().unwrap(),
            )
            .with_clock(Arc::new(SequenceClock::new([1_000]))),
        );
        let request = request_body(vec![request_tuple(
            Address::from([0x22; 20]),
            Address::from([0x11; 20]),
        )]);

        let request_state = Arc::clone(&state);
        let in_flight =
            tokio::spawn(async move { post_signed_context_v1_inner(request_state, request).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.signer.cache_stats().misses == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request must reach the delayed signer");

        let frame = ServerFrame::Error(ErrorFrame {
            code: ErrorCode::StaleSource,
            asset: Some("COIN".into()),
            last_ok_unix_ms: None,
            detail: None,
        });
        let mut encoded = Vec::new();
        ciborium::into_writer(&frame, &mut encoded).unwrap();
        let decoded = crate::pricing_client::decode_server_frame(&encoded).unwrap();
        pricing.apply_test_frame(decoded).await;

        let error = in_flight.await.unwrap().unwrap_err();
        assert!(matches!(
            &error,
            AppError::Unavailable {
                reason: UnavailableReason::NoLiveQuote,
                ..
            }
        ));
        assert_eq!(
            error.into_response().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn batch_revalidates_all_responses_at_one_final_timestamp() {
        let clock = Arc::new(SequenceClock::new([
            1_000, 1_000, 1_000, // first response completes while live
            1_000, 1_000, 2_000, // second response crosses the first deadline
            2_000, // one common final validation timestamp
        ]));
        let state = two_symbol_state(clock, two_symbol_quotes(2_000, 3_000)).await;

        let error = post_signed_context_v1_inner(state, two_symbol_request_body())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AppError::Unavailable {
                reason: UnavailableReason::ExpiredQuote,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn pair_bound_batch_revalidates_all_responses_at_one_final_timestamp() {
        let clock = Arc::new(SequenceClock::new([
            1_000, 1_000, 1_000, 1_000, // first response completes while live
            1_000, 1_000, 1_000, 3_000, // second signature crosses the first deadline
            3_000, // one common final validation timestamp
        ]));
        let state = two_symbol_state(clock, two_symbol_quotes(3_000, 4_000)).await;

        let error =
            post_signed_context_pair_bound(state, two_symbol_request_body(), PairSchema::V5)
                .await
                .unwrap_err();
        assert!(matches!(
            error,
            AppError::Unavailable {
                reason: UnavailableReason::ExpiredQuote,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn post_sign_expiry_is_counted_once_and_not_stored_for_reuse() {
        let clock = Arc::new(SequenceClock::new([
            1_000, 1_000, 1_000, 10_000, // first response expires after signing
            1_000, 1_000, 1_000, 1_000, // replacement remains live throughout
        ]));
        let state = test_state(clock, 1).await;
        let session = SessionInfo {
            session: Session::Rth,
            start: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            end: chrono::DateTime::from_timestamp(30, 0).unwrap(),
        };
        let input = Address::from([0x11; 20]);
        let output = Address::from([0x22; 20]);

        let first_quote = test_snapshot(10_000);
        let error = build_response_from_quote_pair_bound(
            &state,
            &pair(),
            &first_quote,
            input,
            output,
            &session,
            PairSchema::V5,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            AppError::Unavailable {
                reason: UnavailableReason::ExpiredQuote,
                ..
            }
        ));
        let metrics = state.metrics.render();
        let refusal = metrics
            .lines()
            .find(|line| {
                line.starts_with("oracle_quote_refusals_total{")
                    && line.contains("endpoint=\"v5\"")
                    && line.contains("phase=\"post_sign\"")
                    && line.contains("reason=\"expired_quote\"")
                    && line.contains("symbol=\"COIN\"")
            })
            .expect("post-sign refusal metric");
        assert_eq!(refusal.split_whitespace().last(), Some("1"));

        let mut replacement = test_quote(20_000);
        replacement.source_ts_unix_ms = 2_000;
        let replacement = first_quote.in_same_generation(replacement);
        let response = build_response_from_quote_pair_bound(
            &state,
            &pair(),
            &replacement,
            input,
            output,
            &session,
            PairSchema::V5,
        )
        .await
        .unwrap();
        let publish_time = Float::from(B256::from(response.response.context[2]));
        assert_eq!(
            publish_time.format().unwrap(),
            "2",
            "a post-sign refusal must not populate cross-frame reuse"
        );
    }

    #[tokio::test]
    async fn quote_that_remains_live_after_signing_succeeds() {
        let state = test_state(Arc::new(SequenceClock::new([1_000, 1_000, 1_999])), 0).await;
        build_response_from_quote(&state, &pair(), &test_snapshot(2_000))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn expiry_crossing_before_reuse_return_is_refused() {
        let clock = Arc::new(SequenceClock::new([
            1_000, 1_000, 1_000, 1_000, // first response: admission through post-sign
            1_000, 1_000, 10_000, // second response: admission, lookup, reuse return
        ]));
        let state = test_state(clock, 1).await;
        let session = SessionInfo {
            session: Session::Rth,
            start: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            end: chrono::DateTime::from_timestamp(20, 0).unwrap(),
        };
        let input = Address::from([0x11; 20]);
        let output = Address::from([0x22; 20]);
        let first_quote = test_snapshot(10_000);
        build_response_from_quote_pair_bound(
            &state,
            &pair(),
            &first_quote,
            input,
            output,
            &session,
            PairSchema::V5,
        )
        .await
        .unwrap();

        let mut next_quote = test_quote(20_000);
        next_quote.source_ts_unix_ms += 1_000;
        let next_quote = first_quote.in_same_generation(next_quote);
        let error = build_response_from_quote_pair_bound(
            &state,
            &pair(),
            &next_quote,
            input,
            output,
            &session,
            PairSchema::V5,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            AppError::Unavailable {
                reason: UnavailableReason::ExpiredQuote,
                ..
            }
        ));
    }
}
