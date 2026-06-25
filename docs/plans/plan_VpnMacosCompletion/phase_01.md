# Phase 1 — De-risk spike + macOS CI build job

**Goal:** stand up the macOS CI compile/test oracle that every later phase relies
on, and validate the PROVISIONAL PF grammar + utun behavior on a real Mac before
any runtime is wired. Pure-additive: no Linux behavior changes; the VPN module is
still macOS-gated out, so this phase only adds a CI job and a throwaway spike.

Invariants in play: I-M1 (no Linux change). Decisions: D3 (macOS CI = `macos-14`),
D6 (PF grammar provisional until validated here).

---

## Sub-phase 1.1 — Add macOS CI build job

**Model:** Haiku

**Files:**
- `.github/workflows/ci.yml` (existing build/cross-build workflow; add a job).

**Change:**
1. Read `.github/workflows/ci.yml` to learn the existing job style (checkout
   action version, Rust toolchain action, cache step). Mirror it exactly.
2. Add a new job `macos-vpn-build` keyed on `runs-on: macos-14` (Apple Silicon,
   per D3). Steps, in order, mirroring the existing Linux job's setup:
   - checkout
   - install stable Rust toolchain with `clippy` + `rustfmt` components
   - cache (same action/keys as the Linux job, keyed additionally by
     `macos-14`)
   - `cargo build --features vpn`
   - `cargo clippy --features vpn --all-targets -- -D warnings`
   - `cargo test --features vpn`
3. Do NOT add an e2e job here (that is Phase 5). Do NOT touch
   `.github/workflows/e2e_netns.yml`.

Note for the implementer: at this point the vpn module is still
`#[cfg(target_os="linux")]`, so on macOS `cargo build --features vpn` produces a
binary **without** the `bore vpn` subcommand and the macOS snapshot tests do
**not** run on macOS (they compile only on Linux). That is expected — the job
proves the rest of the crate builds clean on macOS and provides the runner that
Phases 2–5 will exercise once the gate is flipped.

**Unit tests:** none (CI config).

**e2e tests:** the job itself is the test — it must go green on `macos-14`.

**Done-criteria:**
- `.github/workflows/ci.yml` contains a `macos-14` job running build + clippy +
  test with `--features vpn`.
- The job passes on a PR (green check named `macos-vpn-build`).
- No existing job was modified except additively.

---

## Sub-phase 1.2 — De-risk spike on real macOS 13+ (Opus gate on findings)

**Model:** Sonnet writes the spike harness; a human runs it on a Mac; **Opus
reviews the findings and approves/patches `pf_ruleset` + builders**.

> This sub-phase is HARDWARE-GATED: it requires an Apple Silicon Mac (macOS 13+)
> with `sudo`. It validates the assumptions the Phase 3/4 runtime is built on. It
> is the gate for Phase 4 (D6). If no Mac is available yet, the rest of Phase 1
> (1.1) can still ship; Phase 4 must not start until 1.2 is signed off.

**Files (new — follow the repo's existing layout):**
- `examples/macos_vpn_spike.rs` — a small standalone binary (the repo already
  builds examples; do not invent a new top-level dir). Keep it behind
  `#![cfg(target_os = "macos")]` so it is inert elsewhere.
- `docs/vpn/VPN_MACOS_SPIKE_FINDINGS.md` — the recorded results (new doc under
  the existing `docs/vpn/` tree).

**Change:**
1. `examples/macos_vpn_spike.rs` must, when run as root on macOS:
   a. Create a utun via `tun_rs::DeviceBuilder::new().ipv4(10.255.255.1/30).mtu(1350).build_async()`
      WITHOUT `.offload(true)` and WITHOUT `.multi_queue(true)` (I-M4). Print the
      kernel-resolved interface name obtained from the device handle (this proves
      the read-back path D7 / Phase 3 depends on).
   b. Run the argv from `bore_cli::...::hostcfg_cmd::macos::cmd_addr_add`,
      `cmd_link_set_mtu`, `cmd_link_set_up` (import the builders; they are pure)
      and confirm `ifconfig <utun>` shows the address/MTU.
   c. Compose a sample ruleset with `macos::pf_ruleset("utunX", "en0",
      &[192.168.7.0/24@10.77.0.0/24 as nat_map], hub=false, nat_masquerade=true,
      forward_accept=true, mss=1310)`, write it to a temp file, and load it with
      the argv from `cmd_pf_enable` + `cmd_pf_load_anchor("spike0", <tmp>)`.
      Capture the exit status and any stderr from `pfctl`.
   d. Dump the loaded anchor with `cmd_pf_show_anchor("spike0")`.
   e. Flush with `cmd_pf_flush_anchor("spike0")`; toggle
      `cmd_sysctl_ip_forward(1)` then restore the original from
      `cmd_sysctl_get_ip_forward`.
   f. Tear down: drop the device; confirm the utun disappears
      (`ifconfig <utun>` fails).
2. Record in `docs/vpn/VPN_MACOS_SPIKE_FINDINGS.md`:
   - exact utun name format observed (e.g. `utun4`) and whether a requested name
     is honored or ignored;
   - every `pfctl` acceptance/rejection with the exact stderr for any rejected
     line;
   - the exact `route -n get <host>` output shape vs what `parse_lan_iface`
     expects (line `interface: <iface>`);
   - whether `sysctl net.inet.ip.forwarding` read/write works under sudo;
   - any builder/grammar correction required.
3. **Opus gate:** Opus reads the findings. If any `pfctl` line was rejected or
   `parse_lan_iface` mismatched, Opus directs the exact fix to
   `hostcfg_cmd::macos` (`pf_ruleset` / the relevant `cmd_*` / `parse_lan_iface`)
   and updates the affected snapshot test(s) in `src/vpn.rs` (the `macos_*` tests
   listed in the reuse map). The validated grammar becomes non-PROVISIONAL.

**Unit tests:**
- If 1.2 patches any builder, update the corresponding snapshot test so it
  encodes the **validated** argv/ruleset:
  `cmd_macos_builders_snapshot` (`src/vpn.rs:3179`), `macos_pf_ruleset_plain_only`
  (`:3260`), `macos_pf_ruleset_netmap_uses_binat_not_masquerade` (`:3280`),
  `macos_pf_ruleset_nat_masquerade_and_hub_and_forward_accept` (`:3298`),
  `macos_parse_lan_iface_from_route_get` (`:3245`). These run on Linux CI; keep
  them green.

**e2e tests:** the spike binary run on a Mac IS the e2e for this phase. No
automated harness yet (that is Phase 5.1).

**Done-criteria:**
- `examples/macos_vpn_spike.rs` exists, compiles on the `macos-14` runner, and a
  human has run it as root on macOS 13+.
- `docs/vpn/VPN_MACOS_SPIKE_FINDINGS.md` records every result listed above with
  exact tool output.
- Opus has signed off: `pf_ruleset` + `cmd_*` + `parse_lan_iface` are confirmed
  or patched, and the `macos_*` snapshot tests reflect the validated form and are
  green on Linux CI.
- The "PROVISIONAL" wording in the `pf_ruleset` doc comment (`src/vpn.rs:3069`,
  `:3083`, `:3105` region) and the `hostcfg_cmd::macos` header
  (`src/vpn.rs:2905`) is updated to "validated on macOS <version>" by Opus.

---

## Phase gates

- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --features vpn --all-targets -- -D warnings`
- **Test:** `cargo test --features vpn` (Linux CI; snapshots green)
- **macOS CI:** the new `macos-14` job green.

## Phase done criterion

macOS CI build job is green; the spike has run on a real Mac and Opus has signed
off the validated PF grammar + builders (or recorded that no change was needed).
No Linux behavior changed (I-M1).
