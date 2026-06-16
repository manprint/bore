# VPN macOS Port — Operational Plan

**Goal:** extend the FULL `bore vpn` feature set (listen/connect, host↔host, site↔host, site↔site,
1:1 NAT netmap, `--nat-masquerade`, `--forward-accept`, hub multi-client, carriers, relay/direct,
PMTU, auto-reconnect, stale-reclaim) to **macOS**, with **zero regression on Linux**.

**Author:** Opus 4.8 (planning). Implementation handoff: Sonnet 4.6 (code), Haiku 4.5 (mechanical /
snapshot tests). Model per phase noted inline.

**Status:** PLAN + Mac-independent groundwork LANDED (2026-06-16). Linux is shipping; the macOS
runtime is greenfield (pending a Mac for the Phase 0 spike).

**Progress (2026-06-16, on this Linux box, zero Linux regression — netns 150/0):**
- ✅ Phase 1.1 — `tun-rs` made available on the macOS target in `Cargo.toml` (Linux dep set
  unchanged; `procfs` stays Linux-only).
- ✅ Phase 3 (pure slice) + Phase 5.1 — full `hostcfg_cmd::macos` builder set + `pf_ruleset`
  composer + `parse_lan_iface`, with snapshot/unit tests (5 tests) that run on the Linux CI.
  See [VPN_MACOS.md](VPN_MACOS.md).
- ⏳ PENDING a Mac: Phase 0 spike (validate utun + PF grammar), Phase 1.2/1.3 (gate flip),
  Phase 2 (utun `create_tun`), Phase 3 runtime (`#[cfg(macos)]` `NetConfig`), Phase 4/5.2/5.3.
- **NOT flipped:** the module `cfg(target_os="linux")` gate is intact, so the Linux build/runtime
  are byte-identical and macOS still has no `vpn` subcommand until the runtime lands.

---

## 0. Executive feasibility

| Layer | Port effort | Notes |
|---|---|---|
| **Transport/data plane** (bridge, AEAD, nonce, carriers, relay, QUIC direct, PMTU, reconnect) | **None** | Pure Rust + tokio + quinn + ring. Platform-agnostic. Works once TUN + UDP exist. |
| **UDP hole-punch / socket buffers** | **Done** | `holepunch.rs:220` already has the `unix, not(linux)` path (no `SO_*BUFFORCE`; plain setsockopt). |
| **TUN device** | **Low** | `tun-rs` supports macOS `utunN`, same `AsyncDevice` API. Drop `offload()`/`multi_queue()` (Linux-only); force `queues=1`; adapt name resolution (`utunN`, not `boreN`). |
| **Host config** (routes, ip_forward, NAT, MSS, spoke-iso, forward-accept) | **Medium — the bulk** | Re-implement `NetConfig` for macOS via `route` + `sysctl` + **PF** (`pfctl` anchors). PF `binat` = the nft prefix-netmap; `nat`/`scrub max-mss`/`block` cover the rest. |
| **Build gating / CLI** | **Low** | Flip `cfg(target_os="linux")` → `cfg(unix)` (or `any(linux, macos)`) on the `vpn` module + `Vpn` subcommand; re-gate the Linux-only internals. |
| **Tests** | **Medium** | Unit/snapshot tests already cross-platform. The `vpn_netns_test.sh` e2e is Linux-only (no `ip netns` on mac) → need a separate macOS e2e (feth/loopback or 2-node) + CI macos job. |

**Verdict: feasible.** No architectural blocker. PF gives true feature parity (binat = stateless 1:1
netmap, host bits preserved). The work is concentrated in one new file (a macOS host-config backend)
plus careful `#[cfg]` gating.

### Parity & degradation matrix (macOS vs Linux)

| Feature | macOS | How |
|---|---|---|
| host↔host, site↔host, site↔site | ✅ full | utun + `route` + PF nat |
| 1:1 NAT netmap (`real@virtual`) | ✅ full | **PF `binat`** (bidirectional 1:1, host-bit preserving) |
| `--nat-masquerade` | ✅ full | PF `nat ... -> (<lan_if>)` scoped to dst |
| hub multi-client + spoke isolation | ✅ full | shared utun + PF `block in on <tun> from <ov> to <ov>` |
| carriers (relay N pairs / direct N QUIC) | ✅ full | data plane, agnostic |
| relay fallback / direct upgrade / retry | ✅ full | data plane, agnostic |
| MSS clamp | ✅ full | PF `scrub ... max-mss <n>` |
| `--forward-accept` | ⚠️ semantics differ | No Docker `FORWARD DROP` on macOS host. Becomes a PF `pass` in the anchor + a sysctl-forwarding check; detection warns if PF default-blocks. Low priority. |
| GSO/GRO offload | ❌ → fallback | single-packet I/O (same as Linux no-offload path); throughput lower |
| `--tun-queues N>1` | ❌ → clamp to 1 | warn + force single queue (no `IFF_MULTI_QUEUE`) |
| `SO_*BUFFORCE` 16 MB UDP buffers | ⚠️ best-effort | kernel-clamped; raise `kern.ipc.maxsockbuf` + plain setsockopt |
| netns-scoped ip_forward refcount (B3) | N/A | no netns on macOS → simpler global marker |
| Linux netns e2e harness | N/A | replaced by a macOS e2e (Phase 5) |

---

## 1. Architecture decision (zero-Linux-regression contract)

**DEC-M1 — freeze the Linux path.** The existing `NetConfig::apply`/`Drop`/`stale_reclaim` and every
`cmd_nft_*`/`cmd_iptables_*`/`cmd_*` builder stay **byte-for-byte** under `#[cfg(target_os="linux")]`.
No edit to a Linux argv. Linux regression surface = 0 by construction.

**DEC-M2 — host config behind a thin platform split, not a runtime trait.** Keep the public surface
(`NetConfig::apply(..)`, `Drop`, `stale_reclaim`, `create_tun`) identical; provide a parallel macOS
implementation selected at compile time (`#[cfg(target_os="macos")]`). Rationale: the Linux apply is
deeply nft/iptables-specific with refcount + stale state; a shared runtime trait would force risky
edits into the Linux body. Compile-time split keeps both honest and independently testable. Shared,
already-generic pieces are reused as-is: `CommandRunner`, `revert_cmds`/`revert_labels` argv stack,
the `NetConfig` struct fields, the data plane.

**DEC-M3 — PF via per-link anchor.** All macOS NAT/filter rules live in a per-link PF anchor
`bore_vpn/<id>`, loaded with `pfctl -a bore_vpn/<id> -f -` (rules on stdin) and torn down with
`pfctl -a bore_vpn/<id> -F all`. PF is enabled once (`pfctl -e`, idempotent; record prior state for
RAII). Mirrors the Linux per-link `nft` table / iptables custom-chain isolation → same teardown
guarantees, SIGKILL `stale_reclaim` by id alone.

**DEC-M4 — command-builder modules per OS.** Extend the existing `hostcfg_cmd::macos` module (E6
groundwork already has `route`/`ifconfig` builders) with the PF + sysctl builders. Pure functions →
snapshot-tested on every platform (incl. the Linux CI box), so the macOS argv is verified without a
Mac.

**DEC-M5 — CommandRunner already abstracts exec.** No change. `RealRunner` runs `pfctl`/`route`/
`sysctl` the same way it runs `nft`. `TestRunner` records them for unit assertions.

---

## 2. Phases

Each phase: deliverable, gates, model. Gates = `cargo fmt`, `cargo clippy -- -D warnings`,
`cargo test`, zero Linux regression (the Linux netns suite must stay green after every phase).

### Phase 0 — De-risk spike (PoC, throwaway) — **Opus + manual Mac**
**Goal:** prove the two unknowns on a real Mac before committing to the refactor.
- **0.1** Spike: open a `utun` with `tun-rs` `DeviceBuilder` (no offload, no multi_queue) on macOS;
  read/write raw IPv4 packets via `AsyncDevice`; confirm tun-rs strips/adds the 4-byte AF header so
  the bridge sees the same byte stream as Linux. **Exit:** ping over a hand-wired utun.
- **0.2** Spike: PF `binat` for a 1:1 netmap + `nat` for masquerade in an anchor; confirm host-bit
  preservation (`10.50.1.5 ↔ 192.168.1.5`) and teardown via `pfctl -a ... -F all`. **Exit:** a
  manual `pfctl` script reproduces T-NAT1/T-NAT-MASQ behavior on macOS.
- **0.3** Confirm `route -n get <ip>` output format → write/validate `macos::parse_lan_iface`.
- **Deliverable:** `docs/vpn/VPN_MACOS_SPIKE_NOTES.md` recording exact utun behavior, PF anchor
  syntax that works, and any surprise (e.g. utun naming constraints, PF `scrub` placement).
- **Risk gate:** if utun/PF behave unexpectedly, revise the plan before Phase 1.

### Phase 1 — Build gating & CLI exposure (no behavior change) — **Sonnet**
- **1.1** `Cargo.toml`: make `tun-rs` available on macOS — `[target.'cfg(any(target_os="linux",
  target_os="macos"))'.dependencies] tun-rs = {...}` (keep `async`; `offload`/`multi_queue` are
  build-time-available but only *called* on Linux).
- **1.2** Flip module gates: `lib.rs`/`main.rs` `cfg(all(feature="vpn", target_os="linux"))` →
  `cfg(all(feature="vpn", any(target_os="linux", target_os="macos")))`. Introduce a helper alias
  `#[cfg(vpn_supported)]` via `build.rs` `cargo:rustc-cfg` to avoid repeating the predicate (optional
  cleanliness).
- **1.3** Inside `vpn.rs`, gate the Linux-only internals that won't compile on macOS:
  nft/iptables builders, the whole Linux `apply`/`Drop`/`stale_reclaim`, offload, multi-queue,
  `/proc` writes, netns refcount. Wrap each in `#[cfg(target_os="linux")]`. Add `#[cfg(target_os=
  "macos")]` stubs (Phase 2/3 fill them).
- **Gate:** `cargo check --target aarch64-apple-darwin --features vpn` compiles the bore source
  (note: needs an osxcross/Mac toolchain for `ring`'s C — see Phase 5 CI). Linux build + netns
  suite unchanged.

### Phase 2 — TUN on macOS — **Sonnet**
- **2.1** `create_tun`: under macOS, build with `DeviceBuilder::new()` WITHOUT `.offload()`/
  `.multi_queue()`; return `offload=false`. Force `queues=1`; if `--tun-queues > 1`, `warn!` +
  clamp.
- **2.2** Name resolution: macOS assigns `utunN`. `--tun-name auto` → let the OS pick / scan
  `utun0..` for a free index; reject arbitrary names (or map to `utunN`) with a clear error.
  Return the resolved `utunN` for routes/PF.
- **2.3** Address/up/MTU: macOS uses `ifconfig utunN <addr> <peer> up` + `ifconfig utunN mtu <n>`
  (note: utun is point-to-point — set local+peer overlay). Add `macos::cmd_addr_add`,
  `cmd_link_set_up` (the E6 module already has `cmd_route_*`/`cmd_link_set_mtu`).
- **Gate:** Phase 0 spike behaviors reproduced via the real code path (manual Mac smoke: link pairs,
  host↔host ping). Linux unchanged.

### Phase 3 — macOS host-config backend (the core) — **Sonnet (Opus review)**
New file `src/vpn_hostcfg_macos.rs` (or a `#[cfg(target_os="macos")] mod` in `vpn.rs`) implementing
`NetConfig::apply`/`Drop`/`stale_reclaim` with the SAME signature/semantics as Linux:
- **3.1 ip_forward:** save/restore `net.inet.ip.forwarding` (+ `net.inet.ip.fw.enable` if needed) via
  `sysctl`. Reuse the existing state-file recovery pattern with a portable `state_dir()` (`/var/run`
  → fallback temp). No netns refcount (macOS has none) → single global marker.
- **3.2 routes:** `macos::cmd_route_add/del` (already present). LAN-iface detection via
  `route -n get <real-host>` + `macos::parse_lan_iface`.
- **3.3 PF bring-up:** enable PF (`pfctl -e`, record prior state), create per-link anchor; RAII
  `pfctl -a bore_vpn/<id> -F all` + (conditionally) `pfctl -d` if we enabled it.
- **3.4 NAT rule emission** into the anchor (one ruleset string on stdin, mirrors `gateway_nft_cmds`):
  - blanket masquerade (plain, no `@`): `nat on <lan_if> from <tun_subnet> to any -> (<lan_if>)`
  - scoped masquerade (plain subnet, dst-scoped): `nat on <lan_if> ... to <subnet> -> (<lan_if>)`
  - 1:1 netmap (`real@virtual`): `binat on <lan_if> from <real> to any -> <virtual>` (bidirectional,
    host-bit preserving — the PF analogue of nft `dnat/snat ip prefix`)
  - `--nat-masquerade` (NAT'd subnet toward LAN): `nat on <lan_if> from any to <real> -> (<lan_if>)`
  - MSS clamp: `scrub on <tun> all max-mss <mtu-40>` (or `match ... scrub (max-mss ..)` on modern PF)
  - hub spoke isolation: `block in on <tun> from <overlay> to <overlay>`
- **3.5 `--forward-accept`:** on macOS there is no Docker `FORWARD DROP`; emit a PF `pass on <tun>`
  / `pass on <lan_if>` in the anchor and a forwarding-enabled assertion. Detection warns only if PF
  has a global block policy. (Document the semantic difference.)
- **3.6 Builders:** add `macos::cmd_pf_enable/disable/load_anchor/flush_anchor`, `cmd_sysctl_ip_fwd`,
  and a `pf_ruleset(id, tun, lan_if, advertised, nat_maps, hub, nat_masquerade, forward_accept) ->
  String` (the macOS twin of `gateway_nft_cmds`) — all pure, snapshot-tested.
- **Gate:** macOS unit snapshots green on the Linux CI box; manual Mac e2e for site↔host, netmap,
  masquerade, hub. Linux netns suite still 150/0.

### Phase 4 — Feature-parity sweep & edge cases — **Sonnet**
- **4.1** Signals: confirm SIGINT/SIGTERM RAII revert on macOS (tokio signal is cross-platform);
  SIGKILL → `stale_reclaim` flushes the PF anchor + restores sysctl by id.
- **4.2** `check_root`/privilege messaging: macOS needs root (utun + pfctl + route) — update the
  error text (no `CAP_NET_ADMIN` on macOS).
- **4.3** Concurrent links on one Mac: per-link anchor name + per-link state marker; verify two
  simultaneous `bore vpn` links don't clobber each other's PF anchor or sysctl restore.
- **4.4** Carriers / relay / direct / PMTU / auto-reconnect: data-plane — add to the macOS e2e to
  *confirm* (no code expected), since they're agnostic.

### Phase 5 — Tests & CI — **Sonnet + Haiku (snapshots)**
- **5.1 (Haiku)** Cross-platform unit snapshots for every `macos::*` builder + `pf_ruleset` +
  `parse_lan_iface` (run on the existing Linux CI — no Mac needed).
- **5.2 (Sonnet)** macOS e2e harness `scripts/vpn_macos_test.sh`: two `bore vpn` processes on one Mac
  (or Mac+Linux), a `feth` pair (macOS functional-ethernet) as the "behind-gateway LAN host" — the
  macOS analogue of the netns `ns_lanm` veth. Cover: host↔host, site↔host, NAT netmap (binat host
  bit), `--nat-masquerade` to a separate feth host, hub 2-spoke, RAII teardown (anchor gone), SIGKILL
  reclaim. Gate on `socat` + root like the Linux harness.
- **5.3 (Sonnet)** GitHub Actions matrix: `{ubuntu, macos} × {default, --features vpn}` — `fmt`,
  `clippy -D warnings`, `cargo test`. macOS job runs the unit suite + (optionally, self-hosted/
  macos-runner) the `vpn_macos_test.sh` smoke. This is what truly proves the Mac build (solves the
  osxcross `ring` gap noted in the assessment).
- **5.4** Keep the Linux netns suite as the Linux gate (unchanged).

### Phase 6 — Docs — **Haiku (draft) + Opus (review)**
- **6.1** `VPN_USER_FULL_GUIDE.md`: add a "Platform support" matrix (Linux full / macOS full with
  noted degradations / Windows TBD) + macOS quick-start (root, PF auto-managed, utunN naming).
- **6.2** New `docs/vpn/VPN_MACOS.md`: PF anchor model, sysctl forwarding, degradations
  (offload/multi-queue/buffers), `--forward-accept` semantic difference, troubleshooting (`pfctl -a
  bore_vpn/<id> -sa`, `route -n get`, `sysctl net.inet.ip.forwarding`).
- **6.3** `CLAUDE.md`: add macOS invariants (PF anchor per link, binat = netmap, no offload/mq,
  Linux path frozen byte-identical = DEC-M1).

---

## 3. Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `tun-rs` utun edge cases (naming, header, async wakeup) | Med | High | Phase 0 spike on real Mac BEFORE refactor |
| PF `binat`/`scrub` syntax differs across macOS versions (pf has changed) | Med | Med | Spike on target macOS version(s); snapshot the exact ruleset; pin tested versions in docs |
| `cargo check`/CI for macOS blocked by `ring` C toolchain on Linux box | High (local) | Low | Use a real `macos` GitHub runner (Phase 5.3); osxcross only if a local check is wanted |
| Accidental Linux regression from `#[cfg]` churn | Med | High | DEC-M1 (freeze Linux argv); run netns suite after every phase; CI gate |
| macOS e2e hard to automate (no netns) | Med | Med | `feth` pairs + 2 processes on one host; fall back to documented manual matrix if CI runner unavailable |
| utun is strictly point-to-point (/30-style), affects hub one-TUN-many-peers model | Med | Med | Spike hub addressing on utun in Phase 0.1/4.3; utun supports a subnet route via `route add -interface`, peers reachable by routing not by link addr — validate early |
| Privilege/entitlements (utun may need root or a signed entitlement) | Low | Med | Document "run as root"; utun via `/dev/utun` needs root, no special entitlement for CLI |

---

## 4. Sequencing & effort (rough)

```
Phase 0 (spike, Mac)        ──► gates the rest. ~1–2 days, MUST be first.
Phase 1 (gating)            ──► ~0.5 day. Mechanical, low risk.
Phase 2 (TUN)               ──► ~1 day.   Depends on 0.1.
Phase 3 (host-config/PF)    ──► ~3–5 days. The bulk. Depends on 0.2/0.3, 2.
Phase 4 (edge cases)        ──► ~1–2 days. Depends on 3.
Phase 5 (tests/CI)          ──► ~2–3 days. 5.1 can start with Phase 3; 5.2/5.3 after 4.
Phase 6 (docs)              ──► ~1 day.   After 4.
```
Critical path: 0 → 2 → 3 → 4 → 5.2. Unit snapshots (5.1) + docs draft (6) parallelize.

## 5. Decisions — LOCKED 2026-06-16

1. **Targets:** **Apple Silicon (arm64), macOS 13 Ventura+.** Spike + CI on this surface only.
   Intel/older macOS out of scope (PF/utun syntax pinned to 13+).
2. **`--forward-accept` on macOS:** **PF `pass` in the per-link anchor** (tun↔LAN) + a forwarding
   assertion — flag stays meaningful cross-platform (Phase 3.5). Documented semantic difference
   (no Docker `FORWARD DROP` on a Mac host).
3. **macOS e2e:** **GitHub `macos` hosted runner** — unit suite always; `vpn_macos_test.sh` smoke
   (utun + `feth` + root) where the runner permits (Phase 5.2/5.3). This is the build-proof that
   closes the osxcross `ring` gap.
4. **Windows:** **deferred** to a separate later plan (the `hostcfg_cmd::windows` `netsh`/wintun
   groundwork stays as-is; not pursued now). macOS first.

---

## 6. Why zero Linux regression holds

- Linux `apply`/`Drop`/`stale_reclaim` + all `cmd_nft_*`/`cmd_iptables_*` are untouched (DEC-M1),
  guarded by `#[cfg(target_os="linux")]`.
- macOS code is additive, behind `#[cfg(target_os="macos")]`.
- The shared surface (`CommandRunner`, `NetConfig` fields, `revert_cmds`, data plane) is already
  platform-neutral.
- The Linux netns suite (`vpn_netns_test.sh`, currently 150/0) is the per-phase Linux gate and must
  stay green; CI enforces it on every change.
```
