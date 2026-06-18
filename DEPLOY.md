# Deploying st0x-oracle-server

NixOS on a DigitalOcean droplet, Terraform-provisioned,
`nixos-anywhere`-bootstrapped, `deploy-rs`-rolled. Mirrors the
[`st0x.bebop`](https://github.com/ST0x-Technology/st0x.bebop/blob/master/DEPLOY.md)
and
[`st0x.pricing`](https://github.com/ST0x-Technology/st0x.pricing/blob/master/DEPLOY.md)
flows — same toolchain, same templates.

**Cutover note:** until the parity window proves the new droplet serves the same
prices as the Fly deployment, this droplet is **Tailnet-only** — no public
ingress. Raindex consumers continue to hit the Fly URL; the diff observer (PR 3)
reaches both over the tailnet. The public-ingress cutover is a separate later
PR.

---

## Prerequisites

Same toolchain as the other st0x services — see
[`st0x.bebop/DEPLOY.md` Prerequisites](https://github.com/ST0x-Technology/st0x.bebop/blob/master/DEPLOY.md#prerequisites)
for the canonical list. In short: Nix with flakes, an ed25519 SSH key, a DO
Personal Access Token, your SSH key uploaded to DO under the name `st0x-op`, a
Tailscale pre-auth key tagged `tag:st0x-oracle-server`, then `nix develop` to
enter the dev shell.

In addition the oracle needs:

- **Signer ETH private key.** The address that Raindex strategies declare as the
  oracle signer. Same key value as the current Fly deployment — copy from
  `fly secrets list` / your password vault.
- **st0x.pricing API key.** Whatever value sits in `PRICING_API_KEYS` on the
  pricing droplet for the `oracle` consumer (format `pricing_oracle_<32-hex>`).
  The Tailscale ACL must also grant `tag:st0x-oracle-server` reach into
  `tag:st0x-pricing:8080` — see the `st0x.pricing/DEPLOY.md` ACL section.
- **Alpaca Broker API credentials.** Key id + secret only; the brokerage account
  id is no longer needed. Used solely for the trading-calendar endpoint that
  powers `MarketHoursCache` (RAI-693).

---

## Step 1 — Add your SSH key to `keys.nix`

Identical to bebop / pricing flow — open `keys.nix` and add yourself to `keys` +
the relevant `roles`. The team SSH keys are shared across droplets so this list
mirrors what's in the other repos.

## Step 2 — Configure Terraform variables

```bash
nix develop -c tf-edit-vars
```

Fill in `do_token`. The `s-1vcpu-1gb` default is sufficient for oracle (one WS
connection + one HTTP server); the bebop OOM recommendation doesn't apply here
because we don't build closures on-droplet on first deploy unless
cross-compiling.

## Step 3 — Provision infrastructure

```bash
nix develop -c tf-init
nix develop -c tf-plan
nix develop -c tf-apply
```

Creates `st0x-oracle-server-nixos` droplet, `st0x-oracle-server-data` 5 GB
volume, reserved IP.

## Step 4 — Create runtime secrets (initial encryption)

`os.nix` references both `.age` files via `age.secrets`, so they must exist
before `bootstrap-nixos` can build the NixOS closure. Encrypt now to the team's
operator keys (`roles.ssh`); after bootstrap pins the host key we re-encrypt to
`roles.host-secrets` (Step 6).

### `secrets/tailscale-authkey.age`

Generate a pre-auth key in the Tailscale admin console. **Tick the "Tags" box**
and select `tag:st0x-oracle-server` when generating — devices that join untagged
don't match the ACL grants and stay unreachable on the tailnet until you tag
them manually.

```bash
echo "tskey-auth-..." > /tmp/tailscale-authkey

nix eval --raw --file ./keys.nix roles.ssh \
  --apply 'builtins.concatStringsSep "\n"' \
  | rage -e -R /dev/stdin -o secrets/tailscale-authkey.age /tmp/tailscale-authkey

rm /tmp/tailscale-authkey
```

### `secrets/st0x-oracle-server-env.age`

```bash
cat > /tmp/st0x-oracle-server-env <<'EOF'
SIGNER_PRIVATE_KEY=0x...
PRICING_API_KEY=pricing_oracle_...
ALPACA_API_KEY_ID=...
ALPACA_API_SECRET_KEY=...
RUST_LOG=st0x_oracle_server=info,warn
EOF

nix eval --raw --file ./keys.nix roles.ssh \
  --apply 'builtins.concatStringsSep "\n"' \
  | rage -e -R /dev/stdin -o secrets/st0x-oracle-server-env.age /tmp/st0x-oracle-server-env

rm /tmp/st0x-oracle-server-env

git add secrets/*.age
git commit -m "chore(deploy): initial runtime secrets"
```

⚠️ Host can't decrypt these yet — its key is still `PLACEHOLDER`. That's fine
for the build (only file presence matters); we re-encrypt in Step 6.

## Step 5 — Bootstrap NixOS

```bash
caffeinate -i nix develop -c bootstrap-nixos
```

Wipes Ubuntu → installs NixOS → reads new host key → rewrites `keys.nix`
in-place. Closure build runs on the droplet when cross-compiling from darwin;
takes 5–10 min plus another ~5 min for the reboot wait loop.

Commit the updated `keys.nix`:

```bash
git add keys.nix
git commit -m "chore(deploy): pin host SSH key after bootstrap"
git push
```

## Step 6 — Re-encrypt secrets with the host key

`roles.host-secrets` = `roles.ssh ++ [ host ]`. Now that `host` is real,
re-encrypt both runtime secrets so the droplet can decrypt them at boot.

```bash
for s in tailscale-authkey st0x-oracle-server-env; do
  rage -d -i ~/.ssh/id_ed25519 secrets/$s.age > /tmp/$s
  nix eval --raw --file ./keys.nix roles.host-secrets \
    --apply 'builtins.concatStringsSep "\n"' \
    | rage -e -R /dev/stdin -o secrets/$s.age /tmp/$s
  rm /tmp/$s
done

nix develop -c tf-rekey

git add secrets/*.age infra/terraform.tfvars.age infra/terraform.tfstate.age
git commit -m "chore(deploy): rekey secrets with new host key"
git push
```

## Step 7 — Deploy

`deploy-all` rolls both the system profile and the service profile. Oracle's
blast radius is **smaller than bebop's** — no inventory at risk, no auto-fill —
but a misconfigured oracle signs wrong prices, so still split the first deploy:

```bash
caffeinate -i nix develop -c deploy-nixos       # system config only
# verify tailnet join + /metrics scrape, then:
caffeinate -i nix develop -c deploy-service st0x-oracle-server
```

## Step 8 — Verify

```bash
# From any tailscale node:
curl -s http://st0x-oracle-server:3000/status | jq
curl -s http://st0x-oracle-server:3000/metrics | head -40
ssh root@st0x-oracle-server journalctl -u st0x-oracle-server -f
```

The obs droplet's Prometheus scrape config (st0x.observability, PR 4) reads
`/metrics` on the tailnet hostname; Loki picks up journald via Alloy with no
extra config.

---

## Ongoing operations

### Redeploy after code changes

```bash
nix develop -c deploy-service st0x-oracle-server
```

### Tail logs

```bash
ssh root@st0x-oracle-server journalctl -u st0x-oracle-server -f
```

### Rotate the signer key

1. Generate a new ETH key.
2. Update strategies that pin the old signer address in their trusted-signers
   list.
3. Update `SIGNER_PRIVATE_KEY` in `secrets/st0x-oracle-server-env.age`.
4. `deploy-service st0x-oracle-server`.

### Rotate the pricing API key

Replace `PRICING_API_KEY` in `secrets/st0x-oracle-server-env.age` and redeploy.
The matching value on `st0x-pricing` lives in its `PRICING_API_KEYS` env file
under the `oracle=` entry — rotate both together.

### Rotate Alpaca creds

Replace `ALPACA_API_KEY_ID` / `ALPACA_API_SECRET_KEY` in
`secrets/st0x-oracle-server-env.age` and redeploy. The Fly deployment is rotated
through `fly secrets set` separately until the cutover.

### Tear down

```bash
nix develop -c tf-destroy
```

---

## Architecture summary

```
Your machine
  └─ nix develop shell
       ├─ Terraform (infra/)   → DO API → Droplet + Volume + Reserved IP
       ├─ nixos-anywhere        → SSH → Install NixOS
       └─ deploy-rs             → SSH → Roll system + service

DigitalOcean droplet (NixOS) — Tailscale node `st0x-oracle-server`
  ├─ tailscaled (joined via /run/agenix/tailscale-authkey)
  ├─ systemd: st0x-oracle-server.service
  │    ├─ EnvironmentFile = /run/agenix/st0x-oracle-server-env
  │    ├─ ExecStart = .../st0x-oracle-server --config config/st0x-oracle-server.toml
  │    └─ Restart = always
  ├─ Outbound: st0x-pricing WS (Tailscale, MagicDNS) + Alpaca calendar
  │            endpoint (public broker-api.alpaca.markets, calendar only)
  ├─ Inbound: HTTP /context/v[12] + /status + /metrics on tailnet only
  │           (parity window); public ingress is a later cutover PR
  ├─ /mnt/data/st0x-oracle-server/logs/*.log (logrotate weekly, 14 retained)
  └─ Alloy → http://st0x-obs:3100/loki/api/v1/push (journald shipping)
```
