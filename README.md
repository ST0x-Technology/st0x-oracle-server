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

### Reusing a signature across frames (v5, v6)

A new price frame every few seconds does not mean a new price. When the
frame for a pair carries the same price as the one already signed, under
the same session and for the same tokens, the oracle serves the previous
signature again as long as that quote still has at least
`signing.reuse_min_remaining_secs` (default 10) before its expiry. The
taker gets an older publish time and the original expiry, both in the
signed bytes, so it can judge freshness itself. A moving price still gets
a fresh signature on every frame. v1 and v4 sign no expiry and are never
reused. Set the value to 0 in `config.toml` to sign every frame:

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

USDC on Base is hardcoded as the quote token in `src/main.rs` — it's a chain invariant.

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

The old `/context` endpoint has been removed; it now returns `404`.

### Price direction

The server automatically determines price direction from the order's IO tokens:

- **Buy tStock** (input=USDC, output=tStock): returns **ask price** (cost to buy)
- **Sell tStock** (input=tStock, output=USDC): returns **1/bid price** (inverted in Rain Float precision)

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
