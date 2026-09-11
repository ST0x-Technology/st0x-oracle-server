# st0x Oracle Server

Signed context oracle server for st0x tokenized equities on [Raindex](https://rainlang.xyz).

Serves `SignedContextV1` data using real-time Alpaca NBBO quotes, enabling Raindex orders to price tokenized equities at executable hedging prices without on-chain oracle gas costs.

## How it works

1. A background loop polls Alpaca every `poll_interval_secs` (default 10s) for every configured symbol and caches the quote alongside its Alpaca-reported timestamp.
2. On each `POST /context/v1`, the server decodes the ABI-encoded request body, resolves the input/output tokens to an Alpaca ticker via the token registry, and serves the **cached** quote — it never hits Alpaca synchronously.
3. Selects the executable price (ask for buys, `1/bid` for sells), encoding the inversion in Rain DecimalFloat precision (not f64).
4. Encodes `[schema_version, price, publish_time]` as Rain DecimalFloats where `publish_time` is Alpaca's own quote timestamp (NOT our fetch time).
5. Signs via EIP-191 and returns a JSON array of `OracleResponse` whose length matches the request length.

If Alpaca is temporarily unreachable, the poll loop logs the error and leaves the previous cached quote in place. The Rainlang strategy bounds freshness via a `max-staleness` guard against `block.timestamp`.

### Signature cache

The price, publish time and quote expiry the oracle signs come from the
pricing frame; the token addresses from the requested pair; the session
window from the market-hours cache, which changes only at session
boundaries. Two requests for one pair inside one price frame therefore
sign byte-identical data, and the server keeps a content-addressed cache
(`keccak256` of the packed context to signature) so the second request
reuses the first signature instead of paying for another KMS operation.
Concurrent requests for bytes already being signed wait on that one sign;
it runs on its own task, so a client hanging up cannot abort it, and a
failure fails all waiters at once. Entries live while used (idle TTL 2
minutes, swept every 256 inserts, 16k hard cap). Consumers see nothing
different: same bytes, a valid signature, the same expiry.

One caveat: if the market-hours calendar failed to load (the server
starts anyway and retries hourly) the session window is `now`, the bytes
change every second and the cache stops helping until the calendar loads.

`oracle_signature_cache_hits_total` / `_misses_total` and
`oracle_signature_cache_entries` on `/metrics` show the effect; the KMS
bill follows the misses.

### Reusing a signature across frames (v5, v6, v7)

A new price frame every few seconds does not mean a new price. When the
frame for a pair carries the same price as the one already signed, under
the same session and for the same tokens, the oracle serves the previous
signature again as long as that quote still has at least
`signing.reuse_min_remaining_secs` (default 10) before its expiry. The
taker gets an older publish time and the original expiry, both in the
signed bytes, so it can judge freshness itself. A moving price still gets
a fresh signature on every frame, and so does an unchanged price whose
new frame carries an earlier expiry: pricing owns the horizon. v1 and v4
sign no expiry and are never reused.

The one thing a consumer does see: a reused quote's signed `publish_time`
trails the live frame by up to the expiry horizon minus this margin
(about 20s with today's pricing). The strategy's `max-staleness` must sit
comfortably above that or flat prices revert as stale on chain. Set the
value to 0 in `config.toml` to sign every frame:

```toml
[signing]
reuse_min_remaining_secs = 10
```

`oracle_signature_reuse_total` counts the KMS calls this avoided.

## Usage

```bash
# Enter nix dev shell
nix develop

# Run with secrets in env + config.toml on disk
SIGNER_PRIVATE_KEY=0x... \
ALPACA_API_KEY_ID=... \
ALPACA_API_SECRET_KEY=... \
cargo run -- --config config.toml
```

### Environment variables (secrets only)

| Variable | Description |
|----------|-------------|
| `SIGNER_PRIVATE_KEY` | Hex private key for EIP-191 signing |
| `ALPACA_API_KEY_ID` | Alpaca read-only API key |
| `ALPACA_API_SECRET_KEY` | Alpaca API secret |

### config.toml

Everything non-secret lives in `config.toml` at the repo root:

```toml
port = 3000
poll_interval_secs = 10

[[tokens]]
address = "0x5cDa0E1CA4ce2af96315f7F8963C85399c172204"
symbol  = "COIN"
```

The chain's settlement stable is the implicit quote token of every pair, and it's a property of the chain rather than of the protocol — it isn't even always a USDC — so it lives in the config as `quote_token` (defaulting to Base's USDC, `0x8335…2913`). It's matched by address; nothing reads its symbol. Token addresses in the bottom 2^16 of the address space are rejected as placeholders: a config for a chain whose tokens are still deploying fails to load rather than booting a registry that resolves nothing.

### Chains

One deployment serves one chain. The chain rides the oracle URL — a deploy-time binding on each order — so it isn't a dimension inside the server: it's which config the deployment was started with. Adding a chain means a config (its `quote_token` and its token registry), a service to run it, and nothing in the request path.

| Chain | Config | Cloud Run service | URL | Quote token |
|-------|--------|-------------------|-----|-------------|
| Base (8453) | `deploy/config/production.toml` | `t0-oracle` in `t0-oracle` | `https://oracle.t0trade.com` | USDC `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913` |
| Base (8453), staging | `deploy/config/staging.toml` | `t0-oracle` in `t0-oracle-staging` | — | same |
| Robinhood Chain (4663) | `deploy/config/robinhood.toml` | `t0-oracle-robinhood` in `t0-oracle` (**to be created**) | `https://oracle-robinhood.t0trade.com` (**proposed**) | USDG `0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168` |

`https://oracle.t0trade.com` is the host st0x.registry's deployed strategies already carry as `oracle-url`; the Robinhood host and the service name are this repo's proposal and nothing resolves them yet. Both are `<service>.t0trade.com` under the same convention, and the name has to be settled before the first Robinhood order is written, because an order binds the URL at deploy time and can't be re-pointed without rewriting the strategy.

Robinhood Chain settles in USDG (Global Dollar), not Circle USDC — Circle's USDC is deployed there too and is not what the chain settles in.

Robinhood Chain is staged, not live. Its registry is complete — 48 wt tokens, exactly st0x.pricing's published set, every address read back from the chain. The two properties that gated it now both hold in this binary: the pricing quote cache is keyed by `(chain id, symbol)`, so frames for one symbol on different chains no longer overwrite each other (RAI-2130), and `/context/v7` signs the deployment's `chain_id`, so a context signed here cannot be replayed into an order on another chain (RAI-1991, below).

#### The chain is in the signature (v7)

EIP-191 signing is chain-agnostic, and every schema up to v6 binds a signed frame only to its `(input_token, output_token)` pair. ST0x token contracts are deterministic clones across chains, so the same pair of addresses can exist on two chains and a frame signed for one verifies byte-for-byte inside an order on the other. Up to v6 the only thing stopping that is which oracle URL an order was deployed against — an operational binding, as strong as whoever wired it.

`/context/v7` signs the deployment's `chain_id` at slot 9, so the strategy can check it itself:

```rainlang
/* per-deployment binding, alongside oracle-signer */
expected-chain-id: 8453,

:ensure(equal-to(signed-context<0 9>() expected-chain-id) "wrong chain"),
```

Set `expected-chain-id` to the chain the order lives on — `8453` on Base, `4663` on Robinhood Chain. A wrong-chain payload then fails the strategy's own assert regardless of URL wiring. A v7 strategy that omits the assert is exactly as exposed as a v6 one.

`chain_id` in the config is what the slot carries. It defaults to Base (8453); `deploy/config/robinhood.toml` sets `4663`. A deployment that claims the wrong chain signs frames its own orders will refuse.

Each deployment rolls on its own: `deploy.yml` rolls Base staging on every main merge, `production-release.yml` rolls Base production, and `robinhood-release.yml` rolls Robinhood — one manual dispatch each, production and Robinhood both behind the `app-deploy` PAM grant in `t0-oracle`. They share a concurrency group because they share that entitlement. One image serves all of them; only the config and the service differ. Every deployed config is validated on PRs by `config-check.yml` and again inside the exact digest it will run on before any grant is requested.

#### Robinhood Chain: what is done and what is not

Done in this repo: the config (48 wt tokens, `chain_id = 4663`, USDG quote token), the PR-time validation, and the release workflow. The binary reads only the pricing frames stamped with its own `chain_id` (RAI-2130), so Base and Robinhood frames for one symbol no longer collide.

Done since RAI-1991 landed: `/context/v7` signs the deployment's `chain_id` at slot 9 (see "The chain is in the signature" above), so release this plane from a tag that carries it.

Not done, and it gates signing:

- **st0x.pricing does not publish chain 4663.** Its production config declares 8453, 1 and 999. A Robinhood oracle pointed at it connects, subscribes, and serves nothing: every symbol sits in the missing-symbols count on `/status`. Pricing needs its `[[chains]]` entry for 4663 (quote token USDG) before this deployment is useful.

#### GCP prerequisites

The shared `app-release` workflow deploys; it does **not** provision. It runs `gcloud run services update`, `gcloud secrets versions add` and a Binary Authorization attestation lookup, so every object below must already exist or the first dispatch fails on the first call. All of it lives in the same project and region as the Base production service.

1. **Cloud Run service `t0-oracle-robinhood`**, project `t0-oracle`, region `europe-west3`. Create it in terraform next to `t0-oracle`, first revision on the current released digest. Its runtime env must mirror the Base production service: `SIGNER_KMS_KEY` (see RAI-1991 above — same key or a Robinhood-specific one, a decision, not a default), `PRICING_WS_URL`, `PRICING_API_KEY` or `PRICING_IAM_AUTH=true`, `ALPACA_API_KEY_ID`, `ALPACA_API_SECRET_KEY`.
2. **`CONFIG_PATH=/config/st0x-oracle-server.toml` on that service.** The image bakes `CONFIG_PATH=/etc/st0x-oracle-server.toml`, a Base registry with no `chain_id`. A service that leaves the baked value serves Base prices on the Robinhood URL and looks healthy doing it. This is the one env var that is silently wrong rather than loudly missing.
3. **Secret `oracle-runtime-config-robinhood`** in `t0-oracle` (t0.devops `modules/runtime-config`), mounted at `/config/st0x-oracle-server.toml`. The release adds versions to it; it never creates it. The service account the service runs as needs `secretmanager.secretAccessor` on it.
4. **IAM for the two release identities**, both already existing in `t0-artifacts`, scoped to the new objects:
   - `oracle-releaser@t0-artifacts.iam.gserviceaccount.com` — `run.services.get`/`update` on `t0-oracle-robinhood` **through the `app-deploy` PAM entitlement only** (no standing deploy role), plus `secretmanager.viewer` + `secretVersionAdder` on `oracle-runtime-config-robinhood`, and `iam.serviceAccounts.actAs` on the service's runtime SA.
   - `oracle-labeler@t0-artifacts.iam.gserviceaccount.com` — nothing new; `config-check` only pulls the released image, which it can already do.
   - `oracle-deployer@…` — nothing new; it never touches this plane.
5. **`app-deploy` PAM entitlement in `t0-oracle`** — extend its role bindings to cover the new service. Approvers see one justification naming `t0-oracle-robinhood`; if the entitlement's role doesn't reach the service, the grant activates and the deploy still 403s.
6. **Binary Authorization.** The `t0-oracle` policy already demands the `projects/t0-artifacts/attestors/oracle-image` attestation, but Cloud Run evaluates it per service: create `t0-oracle-robinhood` with binary authorization enabled, as `t0-oracle` is. Nothing new to sign — it is the same image and the same attestation.
7. **DNS / ingress for `oracle-robinhood.t0trade.com`** (name to be confirmed): whatever fronts `oracle.t0trade.com` today — Cloud Run domain mapping or the external LB — gets the same treatment for the new service, with its own certificate. Public and unauthenticated, like Base: Raindex takers call it directly.
8. **st0x.pricing subscription for chain 4663** — the `[[chains]]` entry above, and, if pricing authenticates by API key rather than Cloud Run IAM, a `pricing_oracle_*` key this service can use. `[pricing].consumer` in the config is `oracle`, the same consumer name as Base.

### Endpoint

```http
POST /context/v1
Content-Type: application/octet-stream
```

Accepts either form (matching upstream `rain.orderbook/crates/quote/src/oracle.rs`):

- **Single**: ABI-encoded `(OrderV4, uint256 inputIOIndex, uint256 outputIOIndex, address counterparty)`
- **Batch**:  ABI-encoded `(OrderV4, uint256, uint256, address)[]`

The response is always a JSON array of `OracleResponse`, with length matching the request:

```json
[
  {
    "signer": "0x...",
    "context": ["0x...", "0x...", "0x..."],
    "signature": "0x..."
  }
]
```

Schema v1 context layout (all Rain DecimalFloats):
- `context[0]`: schema version (= 1)
- `context[1]`: price (ask for buys, `1/bid` for sells)
- `context[2]`: publish_time — Alpaca's own quote timestamp as Unix seconds UTC

Schema v7 context layout (`POST /context/v7`):

- `context[0]`: schema version (= 7)
- `context[1]`: price of the vault's **underlying** asset, for this request's direction
- `context[2]`: publish_time (Unix seconds; the pricing frame's own `source_ts`)
- `context[3]`: session tag
- `context[4]`: session start (Unix seconds)
- `context[5]`: session end (Unix seconds)
- `context[6]`: input token address
- `context[7]`: output token address
- `context[8]`: quote expiry (Unix seconds)
- `context[9]`: chain id this deployment signs for

No slot carries a NAV ratio: v7 signs the underlying price and the strategy derives the vault price on-chain from the live `erc4626-convert-to-assets` answer. v6 is unchanged and still served, NAV ratio at slot 9 and all.

The old `/context` endpoint has been removed; it now returns `404`.

### Price direction

The server automatically determines price direction from the order's IO tokens:

- **Buy tStock** (input=quote token, output=tStock): returns **ask price** (cost to buy)
- **Sell tStock** (input=tStock, output=quote token): returns **1/bid price** (inverted in Rain Float precision)

This ensures the on-chain price matches what can be immediately hedged on Alpaca.

## Development

```bash
nix develop
cargo test
cargo clippy
cargo fmt
```

## License

CAL-1.0-Combined-Work-Exception
