# Phase 3 — macOS TUN runtime (`create_tun` twin)

**Goal:** replace the Phase 2 macOS `create_tun` stub with a working utun
implementation: no GSO/GRO offload, single queue, kernel-assigned `utunN` name
read back from the device. After this phase a macOS host can create and tear down
the TUN that the (still-stubbed) host-config will later configure.

Invariants in play: I-M3 (data plane untouched), I-M4 (no offload, queues=1),
I-M8 (`--tun-name` advisory, read-back name). Decisions: D7 (kernel auto-assign +
read-back). Depends on: Phase 2 (gate flip + stub present).

> This is an Opus design-review gate: utun lifecycle + the read-back name flow
> feed every downstream consumer (routes, PF anchor, admin reporting). Get it
> right once.

---

## Sub-phase 3.1 — Implement macOS `create_tun` (Opus design review → Sonnet)

**Model:** Opus design review → Sonnet implements.

**Files:** `src/vpn.rs` — the `#[cfg(target_os = "macos")]` `create_tun` stub
added in Phase 2.2 (signature
`pub async fn create_tun(name: &str, addr: Ipv4Addr, prefix: u8, mtu: u16, queues: usize) -> anyhow::Result<(Vec<tun_rs::AsyncDevice>, bool, String)>`).
Reference the Linux body at `src/vpn.rs:3916` for the contract (return shape,
logging style) but do NOT share its sysfs/offload/multi-queue logic.

**Change:**
1. Implement the macOS body:
   - If `queues > 1`, `tracing::warn!` "macOS utun: multi-queue unsupported,
     using 1 queue" and proceed with a single device (I-M4). (This duplicates the
     2.3 warning defensively; keep both — the create-time one is authoritative.)
   - Build the device WITHOUT offload and WITHOUT multi-queue:
     `tun_rs::DeviceBuilder::new().ipv4(addr, prefix, None).mtu(mtu)` then
     `.build_async()`. Do NOT call `.offload(true)` or `.multi_queue(true)`
     (I-M4). Do NOT inspect `tcp_gso()`/`udp_gso()` (Linux-only).
   - Name handling (D7/I-M8): macOS does not accept arbitrary names and has no
     `/sys/class/net`. Determine the request:
       - if `name == "auto"` OR `name` matches the Linux pattern `boreN`
         (starts with `bore` followed by digits), request a kernel-assigned utun
         (build without forcing a name, or with the platform's auto sentinel that
         `tun-rs` accepts on macOS — confirmed by the Phase 1 spike findings);
       - else (caller passed an explicit `utunN`) pass it through to the builder.
   - After `build_async()`, read the kernel-resolved interface name back from the
     device handle (the `tun-rs` device exposes the OS name; the Phase 1 spike
     recorded the exact accessor). Use that as `resolved_name`.
   - `tracing::info!(%resolved_name, "macOS utun created (single queue, no offload)")`.
   - Return `(vec![dev], false, resolved_name)` — offload flag is always `false`
     on macOS so the bridge takes the single-packet path (I-M3 reuse of
     `bridge::run(..., offload=false, ...)`).
2. Do NOT call `pick_tun_name` (it probes Linux sysfs). If you need a macOS
   name-mapping helper, add a pure `#[cfg(target_os="macos")]` fn
   `macos_tun_request(name: &str) -> Option<&str>` returning `None` for
   auto/`boreN` (→ kernel assigns) and `Some(name)` for explicit `utunN`, and
   unit-test it.
3. Address assignment: on macOS, `DeviceBuilder.ipv4(...)` may or may not set the
   peer address for a point-to-point utun. If the spike showed the /30 needs an
   explicit `ifconfig <utun> inet <local> <peer> up`, perform it here using the
   already-landed `macos::cmd_addr_add(dev, local, peer)` (`src/vpn.rs:2962`) +
   `cmd_link_set_up` (`:2970`) via `RealRunner` — but ONLY if the builder did not
   already configure it (avoid double-config). The spike findings dictate which
   path; document the choice in a code comment citing the findings doc.

**Unit tests (run on `macos-14`):**
- `macos_tun_request_maps_auto_and_bore` — `macos_tun_request("auto") == None`,
  `macos_tun_request("bore0") == None`, `macos_tun_request("utun9") == Some("utun9")`.
  Pure, in the `#[cfg(test)] mod tests` gated `#[cfg(target_os="macos")]`.

**e2e tests:** covered by the Phase 5.1 smoke (creates a real utun under sudo);
no separate harness here. A `#[ignore]`d root test may be added mirroring the
Linux `create_tun` root test pattern at `src/vpn.rs:5290-5298` if convenient.

**Done-criteria:**
- macOS `create_tun("auto", 10.x/30, 1350, 1)` on a Mac returns
  `(vec of len 1, false, "utunN")` with `utunN` matching `ifconfig` output.
- `macos_tun_request_*` unit test green on `macos-14`.
- No offload / multi-queue code path compiled on macOS (grep the macOS body for
  `offload`/`multi_queue` → none). Linux `create_tun` byte-for-byte unchanged
  (I-M1).

---

## Sub-phase 3.2 — macOS name-resolution tests + smoke hook

**Model:** Sonnet

**Files:** `src/vpn.rs` (`#[cfg(test)] mod tests`, macOS-gated section);
`scripts/` (a small macOS smoke helper consumed by Phase 5.1 — see note).

**Change:**
1. Add the `macos_tun_request_*` test from 3.1 if not already added there.
2. Add a thin macOS smoke entry the Phase 5.1 CI step can call. Prefer
   reusing/extending `examples/macos_vpn_spike.rs` (from Phase 1.2) rather than a
   new file: have it accept an arg `create-teardown` that calls the REAL
   `bore_cli` `create_tun` macOS fn (now implemented) and asserts the returned
   name appears in `ifconfig`, then drops and asserts it is gone. Follow the
   existing `examples/` convention; do not create a new top-level directory.

**Unit tests:** the `macos_tun_request_*` test (green on `macos-14`).

**e2e tests:** the `examples/macos_vpn_spike.rs create-teardown` mode is the
device-level e2e wired into CI by Phase 5.1.

**Done-criteria:**
- `cargo test --features vpn` green on Linux and `macos-14`.
- `examples/macos_vpn_spike.rs create-teardown`, run as root on a Mac, creates a
  utun via the real `create_tun` and confirms teardown.

---

## Phase gates

- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --features vpn --all-targets -- -D warnings` (Linux + `macos-14`).
- **Test:** `cargo test --features vpn` (both targets green).
- **Linux regression:** `sudo -n scripts/vpn_netns_test.sh` green.

## Phase done criterion

macOS `create_tun` creates a single-queue, no-offload utun with a read-back
`utunN` name; unit tests green on both targets; Linux unchanged (I-M1).
