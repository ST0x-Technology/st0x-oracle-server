# Operator cheat sheet — st0x-oracle-server

Common commands. Run inside `nix develop` (the shell wires up `tf-*`,
`deploy-*`, `gh`, `cargo`, etc.).

For incident response, see `RUNBOOK.md`. For first-time provisioning, see
`DEPLOY.md`.

## Where things live

| Thing             | Path / URL                                                |
| ----------------- | --------------------------------------------------------- |
| HTTP API          | `http://st0x-oracle-server:3000` (tailnet, parity-window) |
| Metrics           | `http://st0x-oracle-server:3000/metrics`                  |
| Status            | `http://st0x-oracle-server:3000/status`                   |
| Diff observer     | `http://st0x-oracle-server:3001/metrics`                  |
| Grafana dashboard | `http://st0x-obs:3000` → `st0x — oracle + parity window`  |
| Service unit      | `systemctl status st0x-oracle-server`                     |
| Config            | `config/st0x-oracle-server.toml` (in-repo)                |
| Secrets           | `secrets/*.age` (ragenix)                                 |
| Logs (host)       | `journalctl -u st0x-oracle-server`                        |
| Logs (Loki)       | `{service="st0x-oracle-server"}` in Grafana Explore       |

## Quick HTTP probes

```bash
# Liveness (always 200 when the process is running)
curl -s http://st0x-oracle-server:3000/

# Operational status — signer + configured/missing symbols
curl -s http://st0x-oracle-server:3000/status | jq

# Prometheus metrics in exposition format
curl -s http://st0x-oracle-server:3000/metrics | head -30

# Sign a context for "buy COIN with USDC" via the local probe example
nix develop -c cargo run --release --example probe_local -- \
    --base-url http://st0x-oracle-server:3000
```

## Deploy

```bash
# System config change (firewall, tailscale, packages, alloy):
caffeinate -i nix develop -c deploy-nixos

# Service binary change (Rust code, config/, secrets):
caffeinate -i nix develop -c deploy-service st0x-oracle-server

# Both services in one go (rolls the system profile first, then
# each per-service profile in the order defined in services.nix):
caffeinate -i nix develop -c deploy-all
```

`deploy-nixos` is a system-profile-only roll. `deploy-service <name>` takes the
second positional arg as the systemd unit name.

## Secrets

```bash
# Edit the encrypted runtime env (.age) blob for the oracle service:
rage -d -i ~/.ssh/id_ed25519 secrets/st0x-oracle-server-env.age > /tmp/env
vi /tmp/env
nix eval --raw --file ./keys.nix roles.host-secrets \
  --apply 'builtins.concatStringsSep "\n"' \
  | rage -e -R /dev/stdin -o secrets/st0x-oracle-server-env.age /tmp/env
rm /tmp/env

# Same shape for the observer's env + tailscale auth key. Commit
# all changed `secrets/*.age` files; redeploy.
```

## Tail logs

```bash
# Live oracle logs:
ssh root@st0x-oracle-server journalctl -u st0x-oracle-server -f

# Live diff observer logs:
ssh root@st0x-oracle-server journalctl -u st0x-oracle-diff-observer -f

# Rotated file logs (logrotate, weekly, 14 kept):
ssh root@st0x-oracle-server ls /mnt/data/st0x-oracle-server/logs/
```

## Tests + lint locally

```bash
# All test suites (lib + integration + prod-smoke):
nix develop -c oracle-rs-test

# Format + clippy gate:
nix develop -c oracle-rs-static

# Property-based fuzz targets (signing, pricing-client wire decode);
# included in `oracle-rs-test` via cargo's default test discovery.
nix develop -c bash -c 'cargo test --test integration -- --nocapture'

# Full nix-flake-check (also runs each derivation eval):
nix flake check --impure
```

## Inspect the running process state

```bash
# Symbols seen + missing, signer address:
curl -s http://st0x-oracle-server:3000/status | jq

# Recent v1 / v2 request volume:
curl -s http://st0x-oracle-server:3000/metrics \
  | grep -E '^oracle_context_request_total'

# Diff observer's current drift per pair:
curl -s http://st0x-oracle-server:3001/metrics \
  | grep -E '^oracle_diff_basis_points'
```

## Manual rebuild via the binary directly (no redeploy)

If you need to repro a bug locally with the same binary that's on the droplet:

```bash
nix build .#st0x-oracle-server
./result/bin/st0x-oracle-server \
    --config config/st0x-oracle-server.toml \
    --signer-private-key 0x... \
    --pricing-api-key pricing_oracle_... \
    --alpaca-api-key-id ... \
    --alpaca-api-secret-key ...
```
