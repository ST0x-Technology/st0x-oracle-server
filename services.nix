{
  st0x-oracle-server = {
    enabled = true;
    bin = "st0x-oracle-server";
  };
  # Parity-window observer (RAI-361). Runs on the same droplet as
  # st0x-oracle-server, probes both the legacy Fly URL and the new DO
  # URL in lockstep, and emits per-symbol drift metrics that the obs
  # droplet scrapes. Once the public-ingress cutover lands and Fly is
  # decommissioned, set `enabled = false` here and the deploy will
  # stop rolling out the unit.
  st0x-oracle-diff-observer = {
    enabled = true;
    bin = "st0x-oracle-diff-observer";
  };
}
