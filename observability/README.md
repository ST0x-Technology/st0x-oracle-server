# Observability for the oracle

The Grafana alert rules about the oracle, and nothing else. There are no
dashboards here yet: the Price integrity board stays with pricing. A board
added under `dashboards/oracle/` ships the same way and reloads within about
30 seconds. The rules ship from here: a merge to `main` runs
`.github/workflows/observability.yml`, a short caller of the shared flow in
`ST0x-Technology/.github`, which copies them into the bucket the T0
observability box syncs every minute. A rules change restarts Grafana.
Open it at https://grafana.t0trade.com.

The box itself, its datasources, who gets paged and on which Zulip channel, and
the platform boards (Health, Deployments, Authentication) live in
`T0Trade/t0.grafana` and `T0Trade/t0.devops`, and devops owns those. Nobody
here needs to touch them to change an alert or a board.

`alerting/oracle.rules.yml` is the alert rules, in the `Alerts` folder. Each
rule's `uid` is permanent and must be unique across every repo's file.
Removing a rule from the file does not remove it from Grafana: add its `uid`
to `deleteRules:` as well, or it keeps firing. `deleteRules` retires one of this file's rules for good; never list a rule that merely moved to another file, because Grafana applies `deleteRules` by uid whatever file provisions the rule and would delete it after every restart. Routing is by label: `service: pricing` picks the Zulip channel, each rule
gets its own topic named `<service> / <rule>`, and `severity: critical` also
pages PagerDuty in US extended hours. Datasource uids are fixed by the box:
`victoriametrics` (PromQL over the probes' gauges) and `cloudmon` (Cloud
Monitoring: the service's own metrics and log-based metrics).

The check job provisions these files into a throwaway Grafana on every PR,
because a rules file Grafana refuses would take the real one down when it
restarts. It compares the counts of rules and boards it finds against the
files, so a board that fails to load fails the check. It also keeps the file
in scope: rule groups and `deleteRules` only (no contact points, policies or
mute timings, those are devops's), every group in the `Alerts` folder, every
rule labelled `service: pricing`, and no per-rule routing override.
