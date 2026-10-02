# Phase 4 — Outage gate, invariants, delivery

Intent: prove the reference scenarios on a real network stack for every mode with shipped defaults. Red-check against the baseline, wire the gate into CI, record the invariants, push to `dev` and get CI fully green.
Prerequisites: P2, P3.
Phase closure: P4 (final); review by agent-1.

## State and ownership contract
Same as phase_01.md.

## Local design context
Plan revision 1. Reference scenarios: overview.md table (T-OUT-FLICK / OUTAGE / IPCHANGE / IPCHANGE0).

**Topology.** Unique names (prefix `bout`) so the harness never collides with another harness's namespaces; never run two netns harnesses concurrently anyway.

| Namespace | Role | Addressing |
|-----------|------|------------|
| `bout_s` | server | `10.77.0.1/24`, gw `10.77.0.254` |
| `bout_r` | router | forwarding on |
| `bout_c` | client site: every bore client + local services | `10.78.0.2/24` AND `10.78.0.3/24`, default route `src 10.78.0.2` |
| `bout_v` | visitor / remote consumer / VPN peer site, never affected by events | `10.79.0.2/24` |

**Events,** all in `bout_r` via `iptables -I FORWARD`:

| Event | Method |
|-------|--------|
| blackhole | DROP `-s 10.78.0.0/24` and `-d 10.78.0.0/24` |
| IP change | remove the blackhole; add permanent DROP for `10.78.0.2` both directions; `ip -n bout_c route replace default via 10.78.0.254 src 10.78.0.3` |

Existing sockets keep source `.2`, so they are dead forever; new ones use `.3`. This is exactly an ISP IP change.

**Tunnels under test,** all `--auto-reconnect`, defaults, release binary `--features vpn,ssh-gateway`:

| Mode | Command |
|------|---------|
| public random | `bore local 8001 --to S` |
| public fixed | `bore local 8001 --port 40100` |
| public `--udp --carriers 2` | — |
| vhost | `--udp --carriers 4` (the field configuration) and a plain vhost |
| secret provider | in `bout_c` |
| secret consumer | `bore proxy` in `bout_v` (unaffected side) |
| secret consumer on the affected side | provider in `bout_v`, consumer in `bout_c` |
| ssh-jump native provider | — |
| VPN 1:1 | listener in `bout_c`, connector in `bout_v`, relay-only plus a direct variant |

**Probe.** Per mode, one `probe_<mode>` function returns 0 when a request through the tunnel returns the expected body. A python http.server serves files in `bout_c`.
- **Recovery measurement:** poll every 0.5 s from the restoration instant; record seconds-to-first-success; PASS if ≤ the window.
- **Stability measurement (FLICK):**
  - count `connected` lines per client log before and after; they must be equal;
  - plus a `curl --limit-rate 200k` download of a 2 MiB file through the vhost tunnel, started 2 s before the flick, that must complete with the correct sha256.

## Sub-phases

### 4.1 `scripts/outage_netns_test.sh`
- **Files:** NEW `scripts/outage_netns_test.sh` (executable). Conventions: copy the staleness guard, `pass`/`fail`/`die`, trap cleanup, and dependency SKIP from `scripts/local_proxy_netns_test.sh`. The `BORE` env override allows the red-check on another binary.
- **Change:**
  1. S1 — topology + services + server (`--vhost-domain`, `--udp`, `--vpn`, `--ssh-gateway` as needed by the modes; read `bore server --help` for exact flags).
  2. S2 — tunnels + baseline probes (all PASS before any event).
  3. S3 — the FLICK, OUTAGE, IPCHANGE and IPCHANGE0 events with measurement.
  4. S4 — summary `PASS: n FAIL: m`; exit 1 on any FAIL.
  - Checkpoint after each S.
- **Tests:** the script itself; it must run green with the new binary.
  - **Red-check:** build the baseline `f7b0745` into a separate target dir (`git worktree add /tmp/…/base f7b0745 && cargo build --release --features vpn,ssh-gateway`), run with `BORE=…` and an `OUT_WINDOW` default. T-OUT-IPCHANGE must FAIL for at least the vhost/public modes.
  - Record both runs in STATE §7.
- **Done:** green on new; red on baseline; committed.

### 4.2 CI wiring + regression harnesses
- **Files:** `.github/workflows/e2e_netns.yml` matrix: `{ script: outage_netns_test, secs: 1200, gate: true }`.
- **Change:** run the regression harnesses serially with the new release binary: `local_proxy_netns_test`, `secret_netns_test`, `vhost_netns_test`, `vpn_netns_test`, `ssh_gateway_test`. Record the results.
- **Done:** all green (or a pre-existing failure proven on baseline and recorded); committed.

### 4.3 Invariants + README final pass
- **Files:**
  - `CLAUDE.md`: a new "Client/server liveness (O-1)" invariant bullet summarizing D1–D9 + gates; update the I-SSH3 numbers.
  - `README.md`: verify every section from phases 0–3 against the shipped defaults; the troubleshooting entry "tunnel down for minutes after a network drop".
- **Done:** committed.

## Phase gates and closure (final)
- G-FINAL:
  - fmt;
  - clippy (default, `--features vpn,ssh-gateway`, `--no-default-features`);
  - `cargo test` (default, `--features vpn`, `--features ssh-gateway`);
  - npm tests if the admin UI changed (it should not);
  - T-OUT green;
  - the regression harnesses green.
- Push `dev` (user-authorized 2026-10-02: "committa su Dev e segui la ci"). Watch the GitHub CI (`gh run list --branch dev`, `gh run watch`) until every workflow is green. Fix anything red with new commits, and re-watch.
- P4: final self-review against the reference scenarios; the closure commit.
