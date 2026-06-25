# Phase 6 — Docs

**Goal:** bring all VPN/macOS documentation in line with the now-real runtime:
mark the port phases landed, drop the PROVISIONAL/PENDING language, add a user
guide + platform matrix + troubleshooting. Docs are part of the deliverable.

Invariants in play: none (docs only). Decisions: D6 (PROVISIONAL wording removed
once validated), D7/D8 (document macOS quirks). Depends on: Phases 1–5 complete.

> Opus final read gate: Opus reads the full doc set before the feature is
> declared done.

---

## Sub-phase 6.1 — Update VPN macOS documentation

**Model:** Haiku writes; **Opus final read gate**.

**Files (all existing under `docs/vpn/` or repo root — follow current layout, no
new top-level dirs):**
- `docs/vpn/VPN_MACOS_PORT_PLAN.md` — mark Phases 0–6 status (LANDED / validated
  with dates); replace "runtime PENDING a Mac" with the shipped state.
- `docs/vpn/VPN_MACOS.md` — update the backend reference: PF anchor + `sysctl`
  runtime is real (not provisional); record the validated PF grammar from the
  Phase 1 spike; document `binat`=netmap, `nat`=masquerade, `scrub max-mss`=MSS
  clamp, `block`=spoke isolation, `pass`=`--forward-accept`.
- `README` (or the main user-facing doc that lists `bore vpn` usage) — add macOS
  to the supported-platform statement for VPN; note Apple Silicon, macOS 13+,
  requires `sudo`/root.
- `CLAUDE.md` — update the "VPN macOS port (groundwork only, runtime PENDING)"
  block to reflect that the runtime has landed (Phases 1–5), keeping the locked
  decisions and invariant references (I-M1..I-M8) accurate. Keep it terse and
  factual.

**Change — the docs must cover, concisely:**
1. **Platform matrix:** VPN is Linux + macOS (Apple Silicon, macOS 13+); Windows
   deferred. State that the data plane is shared and only the host edge differs.
2. **macOS usage:** exact `bore vpn listen|connect` examples for macOS, noting:
   - `--tun-name` is advisory; the kernel assigns `utunN` (D7/I-M8);
   - `--tun-queues > 1` and the UDP hole-punch helper flags warn and are
     ignored/advisory on macOS (D8/I-M4, Phase 2.3);
   - root/`sudo` required; PF must be enableable (`pfctl -e`).
3. **Mechanism:** forwarding via `sysctl net.inet.ip.forwarding`; NAT/filter via a
   single per-link PF anchor `bore_vpn/<id>`; RAII revert + SIGKILL
   `stale_reclaim` (parity with Linux, I-M6); state files under `/var/run` (D5).
4. **Troubleshooting:** PF rejected rules (check `pfctl -a bore_vpn/<id> -sa`);
   utun not appearing (root? SIP?); forwarding not restored after SIGKILL (next
   run reclaims); BSD tools have no `--version` (D8 — why bore does not probe
   them).
5. **Testing:** how to run the macOS unit/snapshot tests (`cargo test
   --features vpn` on a Mac), the smoke example (`examples/macos_vpn_spike.rs`),
   and the manual acceptance checklist (`docs/vpn/VPN_MACOS_ACCEPTANCE.md`).

**Unit tests:** none (docs). Run a link-check / `cargo fmt` is irrelevant; ensure
no broken intra-repo links.

**e2e tests:** none.

**Done-criteria:**
- All four doc targets updated; no remaining "PENDING a Mac" / "PROVISIONAL"
  language about the macOS runtime (only honestly-deferred items, e.g. Windows,
  remain marked deferred).
- The platform matrix, usage, mechanism, troubleshooting, and testing sections
  exist and are accurate.
- **Opus final read gate:** Opus reads `overview.md` + all phase files + the
  updated docs and confirms consistency (no contradiction between code, tests,
  and docs); signs off in `resume.md`.

---

## Phase gates

- **Fmt:** `cargo fmt --all -- --check` (unchanged code).
- **Lint:** `cargo clippy --features vpn --all-targets -- -D warnings` (both
  targets — should be untouched).
- No broken doc links.

## Phase done criterion

Documentation reflects the shipped macOS VPN runtime across all four targets;
Opus final read gate passed; the feature is declared done.
