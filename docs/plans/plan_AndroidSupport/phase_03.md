# Phase 3 — VPN Android compile port (cfg gates, twins, host-only guards)

> Precondition: phases 1-2 done.
> Postcondition: `cargo ndk -t x86_64 clippy --all-targets --features vpn -- -D warnings`
> compiles the REAL vpn module for android (no longer cfg'd out); host-only
> guards enforced; **zero Linux/macOS/Windows diff** (D-A5 / I-A1). No runtime
> claim yet — that is phase 4.

Context for the implementer (do not re-explore):

- Anchors (verify with a symbol search if lines drifted): `create_tun` twins
  `vpn.rs:768` (linux) / `:886` (macos) / `:954` (windows); `check_root`
  `:4521` (any(linux,macos)) / `:4539` (windows); `check_binary_exists`
  `:4557` (linux); `stale_reclaim` `:4571`/`:4661`/`:4715`; `run_dir`
  `:5112`/`:5116`/`:5120`; `NetConfig::apply` `:5255`/`:5419`/`:5872`;
  `impl Drop for NetConfig` `:6111`; `pick_tun_name` `:4421` (pure, un-gated);
  `CommandRunner`/`RealRunner` `:4435`; offload pumps: search
  `run_uplink_offload` / `run_downlink_offload` / `run_router_uplink_offload`
  (linux bodies + `unreachable!` twins gated `any(macos, windows)`).
- Gates to flip: `Cargo.toml:79` (`tun-rs` under `any(linux, macos)`),
  `src/lib.rs` (search `pub mod vpn` — gate is
  `all(feature="vpn", any(linux, macos, windows))`), `src/main.rs:410-412`
  (Vpn subcommand, same any-list).
- tun-rs 2.8.5 declares android support; kernel is Linux. Whether the android
  backend accepts the same `DeviceBuilder` calls as the macOS twin is
  UNVERIFIED until the phase 4 spike — write the android `create_tun` to
  compile against the documented tun-rs android API and mark assumptions with
  a `// verified by android_vpn_spike (phase 4)` comment.
- macOS twin pattern to copy for attribute order: doc comment, then
  `#[allow(...)]`, then `#[cfg(...)]` (else `missing_docs` misfires on
  cfg-twinned pub items).
- House rule: unsupported flags ERROR loudly (I-A3); D-A4 fixes the rejected
  set; D-A6 explains why today's vpn-feature check passes without this code.

---

### 3.1 — Gate flips + shared joins (no new bodies yet)

**Model:** Sonnet — **Opus review gate before merge**
**Files:** `Cargo.toml:79`, `src/lib.rs`, `src/main.rs:410-412`, `src/vpn.rs`
(`check_root:4521`, `check_binary_exists:4557`, offload `unreachable!` twin
cfg lists), `src/holepunch.rs` (NO change — D-A7; listed to make the no-change
explicit)
**Change:**
1. `Cargo.toml:79`: extend to `any(target_os="linux", target_os="macos", target_os="android")`.
2. `src/lib.rs` + `src/main.rs`: add `target_os = "android"` to the Vpn
   any-lists (module + subcommand + any sibling gate found by
   `grep -n 'target_os = "windows"' src/lib.rs src/main.rs` in vpn context).
3. `check_root` (`vpn.rs:4521`): extend cfg to
   `any(linux, macos, android)`; body unchanged (`nix::unistd::getuid()`).
   Error text gains an android-conditional hint: on android append
   "on Android run under root (tsu / Magisk su); non-root VPN is impossible —
   see docs/vpn/limits_win_mac/VPN_ANDROID_ACTUAL_LIMIT.md" (use a
   `cfg!(target_os = "android")` branch in the message only — body otherwise
   identical).
4. `check_binary_exists` (`vpn.rs:4557`): extend cfg to `any(linux, android)`
   (toybox provides `which`); body unchanged.
5. Offload pumps: add `target_os = "android"` to each `unreachable!` twin's
   `any(macos, windows)` list (android = single-queue, no offload — same as
   macOS; the Linux `cfg(target_os="linux")` bodies UNTOUCHED).
6. Compile checkpoint: `cargo ndk -t x86_64 check --features vpn` now FAILS
   with missing android twins (expected) — do not "fix" by re-gating; proceed
   to 3.2.
**Unit tests:** none new (no behavior).
**e2e tests:** none.
**Done-criteria:** Linux `cargo clippy --all-targets --features vpn -- -D warnings`
+ `cargo test --features vpn` green (Linux sees zero change);
`git diff` shows ONLY cfg-list edits + the message branch — Opus reviews
exactly this diff for I-A1.

---

### 3.2 — Android twins: `create_tun`, `run_dir`, `stale_reclaim`, `NetConfig::apply`, `Drop` glue

**Model:** Sonnet
**Files:** `src/vpn.rs` (new `#[cfg(target_os="android")]` items placed
directly after their macOS siblings, mirroring the existing twin layout)
**Change:**
1. `create_tun` (after `:886` macOS twin): same signature
   `async fn create_tun(name:&str, addr:Ipv4Addr, prefix:u8, mtu:u16, queues:usize) -> Result<(Vec<TunDevice>, bool, String)>`.
   Single-queue tun-rs device: builder with name request (android accepts a
   name, unlike macOS utun — request `pick_tun_name` result verbatim), address,
   prefix, mtu; offload always `false`; return `(vec![dev], false, actual_name)`
   where `actual_name` is read back via `dev.name()` like macOS. If
   `queues > 1` this fn is NEVER reached (guard in 3.3 errors first) — assert
   with `debug_assert!(queues <= 1)`.
2. `run_dir` (after `:5116`): `#[cfg(target_os="android")] fn run_dir() -> &'static str { "/data/local/tmp" }` (D-A8).
3. `stale_reclaim` (after `:4661`): android body = scan `run_dir()` for this
   platform's leaked state files by `(id, role)` naming (reuse the exact
   filename helpers the linux twin uses — they are shared, un-gated) and
   delete them with an `info!` per file. NO ip_forward handling, NO fwdref/
   netns-inode logic, NO firewall cleanup (host-only never created any).
4. `NetConfig::apply` (after macOS twin `:5419`): android host-only body:
   - Inputs where gateway options are non-empty → `bail!` (defense in depth;
     the CLI guard in 3.3 is the primary gate).
   - Commands via the existing `CommandRunner` (`:4435`): the tun-rs builder
     already configured addr/mtu; issue only `ip link set <tun> up` if tun-rs
     did not, and `ip route add <cidr> dev <tun>` per accepted peer route,
     pushing the exact inverse (`ip route del ...`) onto `revert_cmds`. Use
     only toybox-supported spellings: `ip addr add A/P dev T`,
     `ip link set T up`, `ip link set T mtu N`, `ip route add C dev T`,
     `ip route del C dev T`.
   - Route-table summary `info!` at the end (parity with I-NAT10 style).
   - No nft, no iptables, no sysctl, no PF (D-A4/D-A9).
5. `Drop for NetConfig` (`:6111`): confirm it executes `revert_cmds` via
   argv replay with no OS-specific branching; if it has per-OS match arms, add
   the android arm identical to linux's argv-replay arm. Linux arm untouched.
6. Follow the attribute-order rule (doc, allow, cfg) on every new pub item.
**Unit tests (in `src/vpn.rs` tests mod or `tests/vpn_server_test.rs`,
following where existing hostcfg unit tests live):**
- `android_apply_builds_expected_argv` — with a `MockRunner` (reuse the
  existing CommandRunner test double), apply with 2 peer routes produces the
  exact `ip` argv sequences above and the exact inverse revert stack (LIFO).
- `android_apply_rejects_gateway_inputs` — advertise non-empty → Err whose
  message contains "host-only".
- `android_stale_reclaim_removes_leaked_state` — create fake state file in a
  tempdir-overridden run dir (if run_dir is not injectable, factor the scan
  into a `stale_reclaim_in(dir, id, role)` helper — Linux twin refactor NOT
  allowed; add the helper android-side only), assert file removed.
  These tests are `#[cfg(target_os="android")]`-independent where possible:
  pure builders (argv construction) should be plain un-gated functions so the
  LINUX host test run executes them (pattern: `hostcfg_cmd` builders are
  un-gated). Structure the android argv builders as un-gated
  `pub(crate) fn android_apply_cmds(...) -> Vec<Vec<String>>` +
  `android_revert_cmds(...)` in `hostcfg_cmd` (new `pub mod android` beside
  `mod macos:3268`), and keep the cfg-gated `apply` a thin executor — this is
  what makes the logic testable on the Linux host.
**e2e tests:** none (phase 4).
**Done-criteria:** `cargo ndk -t x86_64 clippy --all-targets --features vpn -- -D warnings`
and same for `-t arm64-v8a` — GREEN (first real android vpn compile);
Linux gates green; new unit tests pass on Linux host.

---

### 3.3 — Host-only CLI guards (fail-fast matrix)

**Model:** Sonnet — **Opus review gate on the guard matrix**
**Files:** `src/main.rs` (vpn arg structs ~`:945-1115` and the vpn dispatch),
or `src/vpn.rs` entry fns if validation lives there (search where
`--tun-queues` is validated for macOS — put android guards in the SAME place)
**Change:** On `target_os = "android"` (use `cfg!` runtime branch in the
validation fn, not struct-level cfg — keeps CLI surface identical across
platforms) reject with `bail!`, exact messages:
| Flag/condition | Error must contain |
|---|---|
| `--advertise` non-empty (listen or connect) | "Android VPN is host-only: --advertise is not supported" |
| `--nat-masquerade` | "not supported on Android (host-only)" |
| `--forward-accept` | "not supported on Android (host-only)" |
| `--max-clients > 1` | "hub mode is not supported on Android" |
| `--tun-queues > 1` | "multi-queue TUN is not supported on Android" (hard error per limits doc — NOT the macOS warn-and-clamp) |
UDP hole-punch helper flags (`--upnp`, `--stun-server`, `--try-port-prediction`,
`--nat-udp-*`): keep whatever cross-platform behavior exists (they are
secret-tunnel/vpn generic) — android does NOT special-case them.
**Unit tests:** `android_guard_matrix` — table-driven test invoking the
validation fn with each rejected combination, asserting each message
substring; plus one accepted baseline (host-only connect with
`--accept-all-routes`) returns Ok. Make the validation fn take the parsed
args struct + a `target_is_android: bool` parameter so the LINUX host test
exercises both branches (same technique as other testable guards in repo);
the call site passes `cfg!(target_os = "android")`.
**e2e tests:** covered in phase 4 (T-AND-L4/L5 re-assert two of these on the
real binary).
**Done-criteria:** matrix test green on Linux host; Opus signs off that the
rejected set == D-A4 exactly (no silent acceptance of any gateway-flavored
flag on android).

---

### 3.4 — Regression sweep + CLAUDE.md note

**Model:** Haiku (mechanical execution + doc edit)
**Files:** none (runs) + `CLAUDE.md`
**Change:**
1. Run full Linux gates: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`
   (default AND `--features vpn`), `cargo test` (default AND `--features vpn`).
2. Run `sudo -n /abs/path/scripts/vpn_netns_test.sh` (exact-path sudo; rebuild
   `cargo build --release --features vpn` as user first — harness refuses
   stale binaries). Expect the full suite green, zero regressions.
3. Confirm `git diff` on pre-existing cfg(linux)/cfg(macos)/cfg(windows)
   bodies is EMPTY (semantic) — the D-A5 proof. Command:
   `git diff main..HEAD -- src/vpn.rs` reviewed against the twin-only rule.
4. Add a short "VPN Android" bullet block to CLAUDE.md mirroring the macOS
   block's format: host-only scope, guard matrix, run_dir, D-A5 contract,
   spike-pending status.
**Unit tests:** n/a. **e2e tests:** netns suite = the regression e2e.
**Done-criteria:** all green; CLAUDE.md updated; resume.md updated.

---

## Phase gates

- Linux: fmt + clippy (default, vpn) + test (default, vpn) green.
- Android: `cargo ndk` clippy `-D warnings` both targets, default + vpn, green.
- `vpn_netns_test.sh` full pass (zero regressions).
- macOS + Windows CI jobs green (cfg-list edits must not disturb them).

**Phase done when:** all gates green + Opus reviews (3.1 diff, 3.3 matrix) approved.
