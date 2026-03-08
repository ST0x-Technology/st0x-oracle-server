# st0x Oracle Server

Signed context oracle server for st0x tokenized equities on [Raindex](https://rainlang.xyz).

Serves `SignedContextV1` data using real-time Alpaca NBBO quotes, enabling Raindex orders to price tokenized equities at executable hedging prices without on-chain oracle gas costs.

## How it works

1. Receives ABI-encoded order data from a Raindex solver/taker
2. Resolves the order's input/output tokens to an Alpaca ticker via the token registry
3. Fetches the latest NBBO quote from Alpaca Markets API
4. Selects the executable price (ask for buys, bid for sells)
5. Encodes price + expiry as Rain DecimalFloats
6. Signs via EIP-191 and returns `SignedContextV1`

## Usage

```bash
# Enter nix dev shell
nix develop

# Run with required config
SIGNER_PRIVATE_KEY=0x... \
ALPACA_API_KEY_ID=... \
ALPACA_API_SECRET_KEY=... \
TOKEN_REGISTRY=0xTOKEN_ADDR=COIN,0xTOKEN_ADDR=RKLB \
cargo run
```

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SIGNER_PRIVATE_KEY` | (required) | Hex private key for EIP-191 signing |
| `ALPACA_API_KEY_ID` | (required) | Alpaca read-only API key |
| `ALPACA_API_SECRET_KEY` | (required) | Alpaca API secret |
| `TOKEN_REGISTRY` | (required) | Comma-separated `ADDRESS=SYMBOL` pairs |
| `EXPIRY_SECONDS` | `5` | Signed context expiry in seconds |
| `PORT` | `3000` | Server port |

### Endpoint

```
POST /context
Content-Type: application/octet-stream
Body: ABI-encoded (OrderV4, uint256 inputIOIndex, uint256 outputIOIndex, address counterparty)
```

Response:
```json
{
  "signer": "0x...",
  "context": ["0x...", "0x..."],
  "signature": "0x..."
}
```

Context layout (Rain DecimalFloats):
- `context[0]`: price (ask for buys, 1/bid for sells)
- `context[1]`: expiry timestamp

### Price direction

The server automatically determines price direction from the order's IO tokens:

- **Buy tStock** (input=USDC, output=tStock): returns **ask price** (cost to buy)
- **Sell tStock** (input=tStock, output=USDC): returns **1/bid price** (inverted)

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
