# On-call runbook — st0x-oracle-server

Symptoms you'll see in the obs dashboard (`st0x — oracle + parity window`) or as
a Telegram alert, paired with the first thing to check.

The deploy mechanics + secret rotation are in `DEPLOY.md`; this file is strictly
**incident response**.

---

## Alert: `OracleDown` (critical)

**Symptom.** `up{job="st0x-oracle-server"} == 0` for ≥ 2 min. Raindex consumers
calling `/context/v1` time out or get connection refused.

**Diagnose.**

```bash
# 1. Is the systemd unit running?
ssh root@st0x-oracle-server systemctl status st0x-oracle-server
# 2. If it's there, are there panics or upstream connect errors?
ssh root@st0x-oracle-server journalctl -u st0x-oracle-server --since '10m ago' -p err
# 3. If the unit is up but Prometheus still can't scrape, the
#    obs droplet's tailnet path to the oracle is broken.
ssh root@st0x-obs curl -sS http://st0x-oracle-server:3000/
```

**Fix.**

- Crash loop after a deploy → roll back:
  `nix develop -c deploy-service st0x-oracle-server` with the previous commit
  checked out.
- Tailnet path broken → check `tailscale status` on both ends; restart
  `tailscaled` on whichever side is missing the peer.
- Bind failure (port 3000 in use) → some other process on the droplet stole the
  port; identify with `ss -tlnp`, kill, restart unit.

---

## Alert: `OracleHasMissingSymbols` (warning)

**Symptom.** One or more configured symbols never appeared on the pricing WS.
Raindex consumers requesting those symbols get a 503 with a "No live quote"
detail.

**Diagnose.**

```bash
# 1. What's missing right now?
curl -s http://st0x-oracle-server:3000/status | jq .missing_symbols

# 2. Is the pricing WS connected at all?
ssh root@st0x-oracle-server journalctl -u st0x-oracle-server --since '5m ago' -g 'Pricing WS'

# 3. Is st0x-pricing actually publishing those symbols?
ssh root@st0x-obs curl -s http://st0x-pricing:8080/status
```

**Fix.**

- If the pricing WS is reconnecting in a loop → cross-check `PRICING_API_KEY`
  against the `oracle=` entry in st0x-pricing's `PRICING_API_KEYS`. Mismatched
  consumer = silent disconnect.
- If pricing-side has no data for the symbol → either the symbol was newly added
  to oracle's config.toml but not to pricing's, or Alpaca isn't returning a
  position for it. Trace via pricing's own dashboard.
- If pricing is healthy but the oracle can't reach it → tailnet ACL grant
  `tag:st0x-oracle-server → tag:st0x-pricing:8080` missing or revoked.

---

## Manual ops

### Restart the oracle (no code change)

```bash
ssh root@st0x-oracle-server systemctl restart st0x-oracle-server
```

### Re-prime the market-hours cache after Alpaca came back

The oracle hits Alpaca's `/v1/calendar` hourly. If it failed at startup the
first hourly tick will pick it back up — but if you need it immediately:

```bash
ssh root@st0x-oracle-server systemctl restart st0x-oracle-server
```

### Stop alerting during a planned cutover

Silence the oracle alerts via Alertmanager UI
(`http://st0x-obs:9093/#/silences`) before flipping the public DNS record. Set
TTL = the expected cutover window + a small buffer.
