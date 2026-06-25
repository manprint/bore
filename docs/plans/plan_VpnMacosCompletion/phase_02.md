# Phase 2 — Flip cfg gates + macOS runtime stubs

**Goal:** make the `vpn` module and the `bore vpn` subcommand compile and appear
on macOS, with the platform-specific runtime entry points present as **stubs**
that bail cleanly. After this phase the `macos-14` CI runner compile-checks the
whole vpn module; Phases 3–4 replace the stubs with real implementations.

Invariants in play: I-M1 (Linux byte-for-byte), I-M2 (compile-time twin).
Decisions: D1 (cfg twin), D2 (early gate flip + stubs).

> BEHAVIOR CHANGE CALLOUT: this phase makes `bore vpn` **appear** on macOS for
> the first time. Until Phases 3–4 land, every macOS `bore vpn listen|connect`
> invocation must `bail!` with a clear "macOS VPN runtime pending Phase N"
> message. This is intentional (D2). Linux is untouched.

---

## Sub-phase 2.1 — Flip the cfg gates to `any(linux, macos)` (Opus design review → Sonnet)

**Model:** Opus design review → Sonnet implements. (Hinge change; review the diff.)

**Files (exact gates — change `target_os = "linux"` to
`any(target_os = "linux", target_os = "macos")` at each):**
- `src/vpn.rs:3` — module attribute `#![cfg(all(feature = "vpn", target_os = "linux"))]`.
- `src/lib.rs:40-42` — `#[cfg(...)] pub mod vpn;` and its `pub use`.
- `src/main.rs:5-8` — `#[cfg(...)] use bore_cli::vpn;`.
- `src/main.rs:937-938` — `#[cfg(...)] struct VpnListenArgs`.
- `src/main.rs:1092-1093` — `#[cfg(...)] struct VpnConnectArgs`.
- `src/main.rs:1643-1644` — `#[cfg(...)] Command::Vpn { command } => match command`.

**Change:**
1. At each anchor above, replace the gate predicate `target_os = "linux"` with
   `any(target_os = "linux", target_os = "macos")`, keeping the
   `all(feature = "vpn", ...)` wrapper intact. Example for `src/vpn.rs:3`:
   `#![cfg(all(feature = "vpn", any(target_os = "linux", target_os = "macos")))]`.
2. Grep the whole `src/` for any other `feature = "vpn"`/`target_os = "linux"`
   pair that gates VPN CLI wiring or the VPN enum and apply the same change
   (command: `grep -rnE 'feature = "vpn".*target_os = "linux"|target_os = "linux".*feature = "vpn"' src/`).
   Do NOT change gates that are Linux-only for non-VPN reasons (e.g. `procfs`
   usage in `admin_api.rs`/`udp_diagnostic.rs`).
3. This step alone will NOT compile on macOS yet (the module body still
   references Linux-only runtime). It compiles on macOS only after 2.2 lands;
   land 2.1 and 2.2 together in the same change set so the macOS CI job stays
   green. On Linux it must remain byte-for-byte (only the gate predicate widened,
   which is a no-op on Linux).

**Unit tests:** none new (gate change). Existing tests must still pass on Linux.

**e2e tests:** none.

**Done-criteria:**
- All six anchors (plus any grep hits) widened to `any(linux, macos)`.
- Linux `cargo build/test --features vpn` unchanged (no new/removed symbols on
  Linux).
- Compiles on macOS only in combination with 2.2 (verified by the `macos-14`
  job).

---

## Sub-phase 2.2 — cfg-split the runtime + add macOS stubs (Opus design review → Sonnet)

**Model:** Opus design review → Sonnet implements. (Concurrency/lifecycle + cfg
discipline; review carefully.)

**Files:** `src/vpn.rs` (the `hostcfg` module — `create_tun` `:3916`,
`NetConfig::apply` `:4145`, `impl Drop` `:4641`, `stale_reclaim` `:3823`, the
state-file helpers `:4020`/`:4055`/`:4068`/`:4098`, and the `cmd_nft_*`/
`cmd_iptables_*` Linux builders `:2340-2799`).

**Change (the goal is: Linux items keep their current bodies under
`#[cfg(target_os="linux")]`; macOS gets stub twins under
`#[cfg(target_os="macos")]`; shared items stay un-gated):**

1. **Freeze Linux builders (I-M1).** Wrap the top-level Linux argv builders in
   `hostcfg_cmd` (the `cmd_nft_*` and `cmd_iptables_*` functions, `src/vpn.rs`
   ~`:2340-2799`) in `#[cfg(target_os = "linux")]` if they are not already, so
   they are not compiled on macOS. The `pub mod macos` and `pub mod windows`
   submodules stay as-is (pure, always compiled). Do not move or edit the bodies.

2. **`create_tun` (`:3916`).** Rename the current Linux body to a
   `#[cfg(target_os = "linux")]` function `create_tun` (keep signature + body
   verbatim — I-M1). Add a `#[cfg(target_os = "macos")]` STUB with the SAME
   signature:
   `pub async fn create_tun(name: &str, addr: Ipv4Addr, prefix: u8, mtu: u16, queues: usize) -> anyhow::Result<(Vec<tun_rs::AsyncDevice>, bool, String)>`
   whose body is `anyhow::bail!("macOS VPN TUN runtime pending Phase 3")`.
   (Phase 3 replaces this stub.)

3. **`NetConfig::apply` (`:4145`).** Split `impl NetConfig` into a
   `#[cfg(target_os = "linux")]` block holding the current `apply` body verbatim
   and a `#[cfg(target_os = "macos")]` block holding a stub `apply` with the SAME
   signature returning `anyhow::bail!("macOS VPN host-config runtime pending Phase 4")`.
   The `struct NetConfig` field definitions (`:4105-4121`) stay un-gated (shared).

4. **`impl Drop for NetConfig` (`:4641`).** Keep the `revert_cmds` reversal loop
   un-gated (it is argv-based, works on both OSes). The `AppliedOp::IpForward`
   restore branch (`:4673-...`) writes `/proc` — extract its body into a private
   helper `restore_ip_forward_op(&self, saved_value: u8)` with two cfg twins:
   `#[cfg(target_os="linux")]` = the current procfs+refcount+sudo-tee logic
   verbatim; `#[cfg(target_os="macos")]` = a stub `{ /* Phase 4 */ }` (no-op for
   now). Drop calls the helper. This keeps Drop compiling on macOS without
   changing Linux behavior.

5. **`stale_reclaim` (`:3823`).** Same pattern: rename current body to
   `#[cfg(target_os="linux")]`; add `#[cfg(target_os="macos")]` stub
   `pub async fn stale_reclaim(_id: &str, _role: &str) { /* Phase 4 */ }`.

6. **State-file helpers (`:4020`/`:4055`/`:4068`/`:4098`).** These reference
   `/run` and `/proc/self/ns/net`. Gate the current bodies
   `#[cfg(target_os="linux")]`; add `#[cfg(target_os="macos")]` stub twins with
   the same signatures returning placeholder paths under `/var/run` and a trivial
   `other_fwdref_present` returning `false` for now (Phase 4 fills them per D5).
   If a helper is only referenced from Linux-gated code, gate the helper
   `#[cfg(target_os="linux")]` and omit the macOS twin until Phase 4 needs it —
   pick whichever keeps both builds warning-free.

7. **Confirm shared items are NOT gated:** `pick_tun_name` (`:3699`),
   `check_root` (`:3798`), `check_binary_exists` (`:3810`), the `CommandRunner`
   trait + `RealRunner`. They compile on both. (`check_root` uses unix uid which
   is fine on macOS; `check_binary_exists` is fine but the macOS path will not
   call it for BSD tools per D8.)

8. Build the macOS target via the `macos-14` CI job (or a Mac) until it is
   warning-clean. Resolve any `unused import`/`dead_code` by scoping imports with
   `#[cfg(...)]` as needed — never by editing a Linux body.

**Unit tests:**
- Existing Linux tests unchanged and green (`cargo test --features vpn` on Linux).
- New (macOS, runs on `macos-14`): `macos_runtime_stubs_bail` — assert that
  `create_tun("auto", ..)`, `NetConfig::apply(..)` return `Err` whose message
  contains "pending Phase". Place it in the existing `#[cfg(test)] mod tests`
  inside `src/vpn.rs`, gated `#[cfg(target_os = "macos")]`.

**e2e tests:** none (smoke is Phase 5).

**Done-criteria:**
- `macos-14` CI job: `cargo build/clippy/test --features vpn` all green; `bore
  vpn --help` lists `listen`/`connect`.
- On macOS, `bore vpn listen ...` / `connect ...` exits non-zero with a message
  containing "pending Phase".
- Linux: `git diff` shows NO semantic change inside any `#[cfg(target_os="linux")]`
  body (only the wrapping cfg attributes were added/renamed). `cargo test
  --features vpn` green; `scripts/vpn_netns_test.sh` still green (I-M1).

---

## Sub-phase 2.3 — Platform flag warnings on macOS

**Model:** Haiku

**Files:** `src/vpn.rs` — at the top of the macOS-reachable code path in
`run_listen` (near `:585`) and `run_connect` (near `:1651`), and the hub
`run_listen_hub` entry (near `:8061`).

**Change (mirror the existing Linux "secret-only flag warns" pattern — find an
existing `warn!` that fires when an inapplicable flag is set, and copy its
shape):**
1. Add `#[cfg(target_os = "macos")]` guarded `tracing::warn!` calls, executed
   once at link start, for flags that do not apply on macOS:
   - `args.tun_queues > 1` → warn "macOS utun has no multi-queue; --tun-queues
     ignored (using 1)".
   - any of `args.upnp`, `args.stun_server.is_some()`,
     `args.try_port_prediction`, `args.nat_udp_preferred_port != <default>`,
     `args.nat_udp_release_timeout != <default>` set → warn that these
     hole-punch-helper flags are advisory/unsupported on macOS (do NOT silently
     ignore — I-M7 spirit: warn).
2. Do NOT change Linux behavior — guard every new warn with
   `#[cfg(target_os = "macos")]`.
3. These warns are advisory only; they must not change control flow.

**Unit tests:** none (logging). Optionally a `#[cfg(target_os="macos")]` test
asserting a helper `macos_flag_warnings(args) -> Vec<&'static str>` returns the
expected warning keys for a given arg set, if you factor the messages into such a
pure helper (preferred for testability).

**e2e tests:** none.

**Done-criteria:**
- On macOS, starting a link with `--tun-queues 4` logs the multi-queue warning;
  with `--stun-server x` logs the hole-punch advisory.
- Linux output unchanged.

---

## Phase gates

- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --features vpn --all-targets -- -D warnings` (Linux AND
  `macos-14`).
- **Test:** `cargo test --features vpn` (Linux green; `macos-14` green incl.
  `macos_runtime_stubs_bail`).
- **Linux regression:** `sudo -n scripts/vpn_netns_test.sh` green.

## Phase done criterion

The vpn module compiles, lints, and tests clean on both Linux and `macos-14`;
`bore vpn` appears on macOS and bails cleanly at the stub boundary; Linux is
byte-for-byte unchanged (I-M1).
