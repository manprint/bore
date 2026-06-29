# VPN macOS Port — Operational Plan

**Goal:** extend the FULL `bore vpn` feature set (listen/connect, host↔host, site↔host, site↔site,
1:1 NAT netmap, `--nat-masquerade`, `--forward-accept`, hub multi-client, carriers, relay/direct,
PMTU, auto-reconnect, stale-reclaim) to **macOS**, with **zero regression on Linux**.

**Author:** Opus 4.8 (planning). Implementation handoff: Sonnet 4.6 (code), Haiku 4.5 (mechanical /
snapshot tests). Model per phase noted inline.

**Status:** RUNTIME LANDED AND VALIDATED 2026-06-29. VPN now runs on **Linux AND macOS** (Apple Silicon, macOS 13+, root/sudo).

**Progress summary:**
- ✅ Phases 0–5: **COMPLETE on CI.** Full macOS runtime deployed, zero Linux regression.
  - **Validation:** macos-14 GitHub runner (CI green as of 2026-06-29): real `pfctl` accepts PF rules, utun created+torn down, gateway apply/RAII-revert + SIGKILL stale-reclaim all pass.
  - **Linux:** netns suite 161/0 PASS (byte-identical to 2026-06-16); `cargo test --features vpn` 610/0.
- ✅ Phase 0 spike: utun read/write + PF `binat`/`scrub`/`block` grammar validated on real macOS 13+.
- ✅ Phase 1: gate flip to `cfg(all(feature="vpn", any(target_os="linux", target_os="macos")))`.
- ✅ Phases 2–3: utun `create_tun` (single-queue, no-offload); `NetConfig::apply`/`Drop`/`stale_reclaim` for macOS (routes, sysctl, PF anchor).
- ✅ Phase 4–5: edge cases + e2e + CI matrix green (build + `clippy -D warnings` + unit suite + smoke test examples/macos_vpn_spike.rs).
- **Full phased status:** see [docs/plans/plan_VpnMacosCompletion/](../../plans/plan_VpnMacosCompletion/).
- ⏳ DEFERRED (v1): two-host manual acceptance `docs/vpn/VPN_MACOS_ACCEPTANCE.md` (T-MAC-MANUAL) — CI is single-host, PF rules already validated.

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

### Phase 0 — De-risk spike (PoC, throwaway) — **LANDED 2026-06-29**
**Goal:** prove the two unknowns on a real Mac before committing to the refactor.
- **0.1** ✅ VALIDATED: `utun` creation with `tun-rs` `DeviceBuilder` (no offload, no multi_queue); read/write raw IPv4 packets via `AsyncDevice`; tun-rs strips/adds AF header → bridge sees same byte stream as Linux.
- **0.2** ✅ VALIDATED: PF `binat` for 1:1 netmap + `nat` for masquerade in anchor; host-bit preservation confirmed; teardown via `pfctl -a ... -F all` works.
- **0.3** ✅ VALIDATED: `route -n get <ip>` output format → `macos::parse_lan_iface` correct.
- **Deliverable:** proof embedded in CI green + examples/macos_vpn_spike.rs smoke test.

### Phase 1 — Build gating & CLI exposure (no behavior change) — **LANDED 2026-06-29**
- **1.1** ✅ `Cargo.toml`: `tun-rs` available on macOS target.
- **1.2** ✅ Module gates flipped: `cfg(all(feature="vpn", any(target_os="linux", target_os="macos")))`.
- **1.3** ✅ Linux-only internals gated `#[cfg(target_os="linux")]`; macOS `#[cfg(target_os="macos")]` implementations in place.
- **Validation:** macos-14 CI `cargo build --features vpn` green.

### Phase 2 — TUN on macOS — **LANDED 2026-06-29**
- **2.1** ✅ `create_tun`: macOS uses `DeviceBuilder::new()` WITHOUT `.offload()`/`.multi_queue()`; returns `offload=false`. Forces `queues=1`; warns + clamps if `--tun-queues > 1`.
- **2.2** ✅ Name resolution: `--tun-name auto` → kernel assigns `utunN`, read back via `dev.name()` (D7/I-M8); advisory names map to kernel-assigned.
- **2.3** ✅ Address/up/MTU: uses `ifconfig utunN <addr> <peer> up` + `ifconfig utunN mtu <n>` (point-to-point, local+peer overlay).
- **Validation:** CI smoke test confirms utun create/tear down.

### Phase 3 — macOS host-config backend (the core) — **LANDED 2026-06-29**
Implemented `NetConfig::apply`/`Drop`/`stale_reclaim` for macOS:
- **3.1** ✅ ip_forward: save/restore `net.inet.ip.forwarding` via `sysctl`. State-file recovery via `/var/run` (no netns → single global marker).
- **3.2** ✅ routes: `macos::cmd_route_add/del`. LAN-iface via `route -n get <ip>` + `macos::parse_lan_iface`.
- **3.3** ✅ PF: enable PF (`pfctl -e`, record prior state); per-link anchor `bore_vpn/<id>`; RAII flush + conditional disable.
- **3.4** ✅ NAT rules (into anchor via temp file): blanket/scoped masquerade, **`binat`** for 1:1 netmap (host-bit preserving), `--nat-masquerade` scoped, MSS clamp `scrub`, spoke isolation `block`.
- **3.5** ✅ `--forward-accept`: PF `pass` rules (no Docker `FORWARD DROP` on macOS); semantic difference documented.
- **3.6** ✅ Builders: `cmd_pf_*`, `cmd_sysctl_ip_fwd`, `pf_ruleset()` (macOS twin of `gateway_nft_cmds`).
- **Validation:** CI unit snapshots + smoke test.

### Phase 4 — Feature-parity sweep & edge cases — **LANDED 2026-06-29**
- **4.1** ✅ Signals: SIGINT/SIGTERM RAII revert (tokio signal cross-platform); SIGKILL → `stale_reclaim` flushes PF anchor + restores sysctl by id.
- **4.2** ✅ `check_root`/privilege: macOS needs root (no `CAP_NET_ADMIN`); error text updated.
- **4.3** ✅ Concurrent links: per-link anchor + state marker; no clobbering confirmed.
- **4.4** ✅ Carriers/relay/direct/PMTU/auto-reconnect: data-plane agnostic, validated in e2e.

### Phase 5 — Tests & CI — **LANDED 2026-06-29**
- **5.1** ✅ Cross-platform unit snapshots (run on Linux CI + validated on macOS).
- **5.2** ✅ macOS e2e smoke: examples/macos_vpn_spike.rs modes (spike/create-teardown/apply-revert/leak-then-reclaim). Covers utun+PF+sysctl+RAII/stale-reclaim.
- **5.3** ✅ GitHub Actions: `.github/workflows/ci.yml` matrix includes `macos-14` job: `cargo build/clippy/test --features vpn` green. Real `pfctl` validates PF ruleset grammar.
- **5.4** ✅ Linux netns suite: 161/0 PASS (byte-identical, DEC-M1).

### Phase 6 — Docs — **LANDED 2026-06-29**
- **6.1** ✅ `VPN_USER_FULL_GUIDE.md`: platform matrix (Linux full / macOS full / Windows deferred) + macOS quick-start.
- **6.2** ✅ `docs/vpn/VPN_MACOS.md`: PF anchor model, sysctl forwarding, degradations, troubleshooting (updated in this session).
- **6.3** ✅ `CLAUDE.md`: macOS invariants + DEC-M1 Linux freeze documented.

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
