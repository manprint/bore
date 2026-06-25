# Phase 4 — macOS host-config runtime (NetConfig / Drop / stale_reclaim twin)

**Goal:** replace the Phase 2 macOS host-config stubs with a working
`NetConfig::apply` (routes + `sysctl` forwarding + PF anchor), a working `Drop`
ip_forward restore, and a working `stale_reclaim` — full RAII + SIGKILL-recovery
parity with Linux, expressed through the already-landed `hostcfg_cmd::macos`
builders + `pf_ruleset`. This is the bulk of the port.

Invariants in play: I-M1 (Linux unchanged), I-M5 (PF semantics mirror nft),
I-M6 (RAII + SIGKILL parity), I-M7 (no `--version` probe for BSD tools).
Decisions: D4 (sysctl + single PF anchor), D5 (`/var/run` state, no netns
scoping), D8 (no BSD `--version` probe). Depends on: Phase 1.2 (validated PF
grammar) and Phase 2/3.

> HARD DEPENDENCY: Phase 1.2 spike must be signed off (D6) before starting 4.1 —
> the PF grammar this phase loads must be the validated one.
>
> Two Opus design-review gates (4.1, 4.2): lifecycle + teardown correctness.

---

## Sub-phase 4.1 — macOS `NetConfig::apply` (Opus design review → Sonnet)

**Model:** Opus design review → Sonnet implements.

**Files:** `src/vpn.rs` — the `#[cfg(target_os = "macos")]` `apply` stub from
Phase 2.2. Use the Linux `apply` at `src/vpn.rs:4145` as the STRUCTURAL template
(same arg order, same `revert_cmds`/`revert_labels`/`applied_ops` bookkeeping,
same logging style) but drive macOS builders. Signature is identical:
```
pub async fn apply<R: CommandRunner>(
    runner: &R, id: &str, role: &str, tun_name: &str,
    _assigned: Ipv4Addr, _prefix: u8,
    peer_routes: &[Ipv4Net], advertised: &[Ipv4Net],
    nat_maps: &[(Ipv4Net, Ipv4Net)],
    no_route_manage: bool, hub: bool, nat_masquerade: bool, forward_accept: bool,
) -> anyhow::Result<Self>
```

**Change — implement the macOS body in this order:**
1. Construct `NetConfig { id, role, tun_name, no_route_manage, nft_available:
   false, revert_cmds: vec![], revert_labels: vec![], ip_forward_saved: None,
   applied_ops: vec![] }` (mirror `:4162`).
2. **Routes** (mirror `:4176-4192`): for each `net` in `peer_routes`, if
   `no_route_manage` print the `macos::cmd_route_add(&subnet, tun_name)` argv with
   a skip note; else `runner.run(&macos::cmd_route_add(..))`, log "added route",
   and push the matching `macos::cmd_route_del(..)` (add it to
   `hostcfg_cmd::macos` if it does not exist yet — there must be a delete twin of
   `cmd_route_add`; the Linux side has `cmd_route_del`. If absent, add
   `macos::cmd_route_del(subnet, dev)` building `route -n delete -net <subnet>
   -interface <dev>` and snapshot-test it) to `revert_cmds` + a label to
   `revert_labels`.
3. **Gateway mode** (`is_gateway = !advertised.is_empty()`) and
   `!no_route_manage` (mirror `:4194` onward), but macOS-native:
   a. **Save + enable forwarding via sysctl (D4, NOT procfs).** Read current with
      `runner.run(&macos::cmd_sysctl_get_ip_forward())` (`src/vpn.rs:3005`),
      parse the trailing `0`/`1`. If `0`, enable with
      `runner.run(&macos::cmd_sysctl_ip_forward(1))` (`:2995`). Save the value to
      the macOS state file (4.3 helper) so `stale_reclaim` can restore after
      SIGKILL. Record first-wins orig + per-link refcount marker (4.3 helpers).
      Set `cfg.ip_forward_saved = Some(saved)` and push
      `AppliedOp::IpForward { saved_value: saved }` (the enum is shared).
      Do NOT read/write `/proc` and do NOT use the sudo-tee fallback (Linux-only).
   b. **LAN egress iface** (mirror `:4250-4262`): compute a sample host
      (`advertised[0].network() + 1`), run `macos::cmd_route_get(&sample)`
      (`:2952`), parse with `macos::parse_lan_iface(&out)` (`:2957`); error if
      `None` with the same message shape as Linux.
   c. **PF anchor (D4).** Compose the ruleset with
      `macos::pf_ruleset(tun_name, &lan_if, advertised, nat_maps, hub,
      nat_masquerade, forward_accept, mtu_minus_40)` — `mss = mtu - 40`; obtain
      `mtu` the same way the Linux MSS clamp does (the apply caller passes mtu via
      the link args; if `apply` does not currently receive mtu, pass it through:
      check how the Linux MSS clamp gets its value at `src/vpn.rs:4634`
      `cmd_nft_add_mss_clamp(id)` — if the clamp value is fixed there, mirror that
      constant; otherwise thread `mtu` in. Prefer reading the actual value used by
      the Linux clamp to stay consistent; document the source in a comment).
      Write the ruleset string to a temp file (use the project scratch/`tempfile`
      pattern already in the repo if one exists; otherwise `std::env::temp_dir()`
      with a unique name `bore_vpn_<id>.pf`). Then:
        - `runner.run(&macos::cmd_pf_enable())` (`:2991`) — enable PF
          (idempotent; if already enabled `pfctl -e` warns to stderr — treat a
          non-zero solely-from-"already enabled" as success; the spike findings
          give the exact stderr to tolerate).
        - `runner.run(&macos::cmd_pf_load_anchor(id, &tmp_path))` (`:2996`) — load
          the anchor.
        - push revert `macos::cmd_pf_flush_anchor(id)` (`:3006`) +
          label "flush PF anchor bore_vpn/<id>" to `revert_cmds`/`revert_labels`.
        - keep the temp file until Drop, or delete it after load if `pfctl`
          copies it in (the spike confirms; default: delete after successful load
          — pfctl reads it synchronously).
4. **`no_route_manage` path:** when set, print each macOS argv with a
   "# (skipped, --no-route-manage)" note exactly like Linux (`:4180-4181`), set
   no forwarding, install no PF anchor, push no reverts. This keeps the smoke
   test (T-MAC-SMOKE) able to run create_tun + addr without touching PF.
5. **D8:** do NOT call `check_binary_exists` for `route`/`ifconfig`/`pfctl`/
   `sysctl`. The macOS body assumes they exist; surface real `pfctl`/`route`
   errors via `?` with `.with_context(...)`.
6. Return `Ok(cfg)`.

**Unit tests:** see 4.4 (rule-plane argv capture). No new test here beyond what
4.4 covers.

**e2e tests:** Phase 5.1 (single host) + 5.2 (manual two-host).

**Done-criteria:**
- macOS `apply` with a `nat_maps` pair produces, against a capturing
  `CommandRunner` (4.4), this argv sequence: route add(s) → sysctl get → sysctl
  set 1 → route get → pf enable → pf load anchor; and the loaded temp-file
  content equals `pf_ruleset(...)` for those inputs.
- `apply` with empty `advertised` (non-gateway) installs only routes, no sysctl,
  no PF.
- `no_route_manage=true` runs zero `runner.run` calls and pushes zero reverts.
- Linux `apply` byte-for-byte unchanged (I-M1).

---

## Sub-phase 4.2 — macOS Drop ip_forward restore + `stale_reclaim` (Opus design review → Sonnet)

**Model:** Opus design review → Sonnet implements.

**Files:** `src/vpn.rs` — the `restore_ip_forward_op` macOS twin stub (Phase 2.2
step 4) and the `#[cfg(target_os="macos")]` `stale_reclaim` stub (Phase 2.2
step 5). Reference Linux Drop (`:4641`) and Linux `stale_reclaim` (`:3823`).

**Change:**
1. **macOS `restore_ip_forward_op(&self, saved_value: u8)`** (called by the
   shared Drop loop): mirror the Linux refcount logic (`:4673-...`) but with
   macOS state paths (4.3) and `sysctl`:
   - remove this link's refcount marker (4.3 `fwd_refcount_path`);
   - if another marker remains (`other_fwdref_present` over the macOS state dir),
     leave forwarding enabled, clean our own state file, return;
   - else (last link out) read the first-wins orig value (4.3 `ipforward_orig_path`,
     fallback to `saved_value`) and restore via
     `std::process::Command` running `macos::cmd_sysctl_ip_forward(restore_to)`
     (blocking is fine — Drop is sync, same as Linux). On failure `warn!` with a
     manual-remediation hint (mirror Linux `:4715`), e.g.
     `sudo sysctl -w net.inet.ip.forwarding=<v>`.
   - The PF anchor flush is already handled by the shared `revert_cmds` loop
     (the `cmd_pf_flush_anchor` pushed in 4.1), so Drop needs no extra PF code.
2. **macOS `stale_reclaim(id, role)`** (mirror Linux `:3823-3864`): on start,
   - flush any leftover PF anchor by id: run `macos::cmd_pf_flush_anchor(id)`
     (best-effort, ignore errors — anchor may not exist);
   - read the macOS ip_forward state file (4.3); if present and another link's
     refcount is NOT active, restore forwarding via
     `macos::cmd_sysctl_ip_forward(saved_value)` and log
     "stale_reclaim: restoring net.inet.ip.forwarding from state file";
   - remove our own stale state + refcount files; if last out, remove the orig
     file.
   - Mirror the Linux refcount-awareness so a concurrent live link is never
     clobbered.

**Unit tests (run on `macos-14`):**
- `macos_stale_reclaim_restores_forwarding` — write a fake macOS state file with
  saved value `0` and no other refcount marker, call `stale_reclaim` with a
  mock/`#[cfg(test)]` runner (or point the state-dir helper at a temp dir via a
  test seam), assert it emits the `sysctl ... =0` argv. If `stale_reclaim` runs
  real commands, factor the decision into a pure helper
  `macos_reclaim_plan(state) -> Option<u8>` and test that instead (preferred —
  no root needed in CI).
- `macos_drop_refcount_keeps_forwarding_when_peer_active` — pure-helper test
  mirroring the Linux `other_fwdref_present` semantics on the macOS state dir.

**e2e tests:** Phase 5.1 proves the real SIGKILL→stale_reclaim cycle under sudo.

**Done-criteria:**
- macOS Drop restores `net.inet.ip.forwarding` only when it is the last gateway
  link out (refcount-aware), else leaves it; PF anchor flushed via revert loop.
- macOS `stale_reclaim` flushes a leftover `bore_vpn/<id>` anchor and restores
  forwarding from a state file, refcount-aware.
- Linux Drop + stale_reclaim byte-for-byte unchanged (I-M1).

---

## Sub-phase 4.3 — macOS state-file helpers

**Model:** Sonnet

**Files:** `src/vpn.rs` — the `#[cfg(target_os="macos")]` stubs of
`ipforward_state_path` (`:4020`), `fwd_refcount_path` (`:4055`),
`ipforward_orig_path` (`:4068`), `other_fwdref_present` (`:4098`) from Phase 2.2
step 6. Reference the Linux bodies for naming convention.

**Change (per D5):**
1. Base dir: `/var/run` (writable as root on macOS; falls back to
   `std::env::temp_dir()` if `/var/run` is not writable — log at `debug` on
   fallback). NO `/proc/self/ns/net` inode scoping (macOS has no netns); the
   filename is `bore_vpn_<id>_<role>.fwdstate` / `.fwdref` and the orig is
   `bore_vpn.ipfwd-orig`, mirroring the Linux *names* minus the netns-inode
   prefix.
2. `other_fwdref_present(dir, mine)`: scan `dir` for `bore_vpn_*_*.fwdref` files
   other than `mine`; return true if any exists (mirror Linux
   `other_fwdref_present_with_prefix` at `:4076`, but with the macOS prefix and no
   inode).
3. Keep signatures identical to the Linux twins so 4.1/4.2 call them uniformly.

**Unit tests (run on `macos-14`):**
- `macos_state_paths_under_var_run` — assert the three path builders produce
  `/var/run/bore_vpn_*` paths with the id/role embedded.
- `macos_other_fwdref_present_detects_peer` — create two `.fwdref` files in a temp
  dir, assert `other_fwdref_present(tmp, file_a)` sees `file_b` (use the
  temp-dir-accepting inner helper, mirroring the Linux test pattern).

**e2e tests:** none (covered transitively by 5.1).

**Done-criteria:**
- macOS state helpers return `/var/run/bore_vpn_*` paths; refcount detection
  works in a temp dir; unit tests green on `macos-14`.

---

## Sub-phase 4.4 — macOS rule-plane unit tests

**Model:** Sonnet

**Files:** `src/vpn.rs` `#[cfg(test)] mod tests`, a `#[cfg(target_os="macos")]`
section. Mirror the Linux `NetConfig::apply` rule-plane tests at `src/vpn.rs:4790`,
`:4826`, `:4875`, `:4912`, `:4964`, `:5013`, `:5049`, `:5091`, `:5131` (they use a
capturing/mock `CommandRunner`). Reuse that mock runner if it is already in the
test module; otherwise add a small `#[cfg(test)]` capturing runner that records
each argv.

**Change — add macOS-gated tests asserting the captured argv sequence + PF
temp-file content from `NetConfig::apply` (D4 ordering):**
1. `macos_apply_plain_advertise_uses_sysctl_and_pf_nat` — one plain advertised
   subnet, empty `nat_maps`: assert captured argv contains, in order, a
   `route ... add`, `sysctl -n net.inet.ip.forwarding`, `sysctl -w
   net.inet.ip.forwarding=1`, `route -n get`, `pfctl -e`,
   `pfctl -a bore_vpn/<id> -f`; and the loaded ruleset contains
   `nat on <lan> from any to <subnet> -> (<lan>)` and `scrub on <tun> all max-mss`.
2. `macos_apply_netmap_uses_binat` — one `real@virtual` pair: assert the ruleset
   contains `binat on <lan> from <real> to any -> <virtual>` and NOT a `nat`
   masquerade for that subnet (mirror the Linux netmap test intent at `:4875`).
3. `macos_apply_nat_masquerade_and_hub_and_forward_accept` — pair +
   `nat_masquerade=true` + `hub=true` + `forward_accept=true`: assert the ruleset
   contains the extra `nat ... to <real>`, a `block in on <tun> ...`, and the two
   `pass` lines (mirror `macos_pf_ruleset_nat_masquerade_and_hub_and_forward_accept`
   at `:3298`, but via `apply`, not the pure composer).
4. `macos_apply_no_route_manage_runs_nothing` — `no_route_manage=true`: assert the
   capturing runner recorded ZERO commands and `cfg.revert_cmds` is empty.
5. `macos_apply_non_gateway_only_routes` — empty `advertised`: assert routes added
   but no sysctl/pf argv captured.

**Unit tests:** the five tests above (green on `macos-14`). They do NOT require
root (the capturing runner does not execute).

**e2e tests:** none.

**Done-criteria:**
- All five macOS rule-plane tests green on `macos-14`.
- The captured argv ordering matches D4; the PF temp-file content matches
  `pf_ruleset` for the inputs.
- Linux rule-plane tests unchanged and green.

---

## Phase gates

- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --features vpn --all-targets -- -D warnings` (Linux + `macos-14`).
- **Test:** `cargo test --features vpn` (both targets green, incl. all new
  `macos_apply_*` / `macos_state_*` / `macos_stale_reclaim_*` tests).
- **Linux regression:** `sudo -n scripts/vpn_netns_test.sh` + `_hard` green.

## Phase done criterion

macOS `NetConfig::apply`/`Drop`/`stale_reclaim` are fully implemented and pass
the rule-plane + state-file + reclaim unit tests on `macos-14`; RAII + SIGKILL
parity holds (I-M6); Linux runtime is byte-for-byte unchanged (I-M1).
