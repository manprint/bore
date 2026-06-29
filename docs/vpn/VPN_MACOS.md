# bore vpn on macOS — backend reference

Companion to the [operational plan](VPN_MACOS_PORT_PLAN.md). This documents the **macOS
host-config backend** implementation, now SHIPPED AND VALIDATED on CI (2026-06-29).

> **Status (2026-06-29):** RUNTIME SHIPPED. `bore vpn` runs on **Linux AND macOS** (Apple Silicon,
> macOS 13+, root/sudo). Module gate: `cfg(all(feature="vpn", any(target_os="linux",
> target_os="macos")))`. CI validation: macos-14 runner (pfctl + utun + sysctl + RAII all green,
> 2026-06-29). Linux byte-identical, zero regression (netns 161/0).

---

## Why a separate backend

The Linux VPN drives the kernel through `ip` + **nftables/iptables** + `/proc/.../ip_forward` +
network namespaces. None of those exist on macOS. macOS uses:

| Concern | Linux | macOS |
|---|---|---|
| TUN device | `/dev/net/tun`, `boreN`, GSO/GRO, multi-queue | `utunN` (point-to-point), no offload, single queue |
| Routes | `ip route` | `route -n add/delete -net … -interface utunN` |
| Address/MTU | `ip addr` / `ip link` | `ifconfig utunN inet <local> <peer> up` / `ifconfig utunN mtu N` |
| IP forwarding | `/proc/sys/net/ipv4/ip_forward` | `sysctl net.inet.ip.forwarding` |
| NAT / filter | nftables table / iptables chains | **PF** (`pfctl`) per-link anchor `bore_vpn/<id>` |
| LAN iface probe | `ip route get <ip>` (`dev …`) | `route -n get <ip>` (`interface: …`) |
| Privilege | root or `CAP_NET_ADMIN` | root |
| Isolation for tests | network namespaces | `feth` pairs |

The Linux path stays **byte-for-byte frozen** under `#[cfg(target_os="linux")]` (DEC-M1) — the macOS
backend is additive, selected at compile time.

## PF model

All NAT/filter rules for a link live in one PF anchor `bore_vpn/<id>`, loaded with
`pfctl -a bore_vpn/<id> -f <file>` and removed on teardown with `pfctl -a bore_vpn/<id> -F all`
(by id alone → SIGKILL `stale_reclaim` works without knowing the LAN iface). PF is enabled once
(`pfctl -e`, prior state recorded for RAII). Because `CommandRunner` has no stdin, the runtime
writes the ruleset to a temp file and loads from it.

### Rule mapping (Linux nft → macOS PF)

| Linux (nft) | macOS (PF) |
|---|---|
| `nft … iif tun oif lan masquerade` (blanket/scoped) | `nat on <lan> from any to <subnet> -> (<lan>)` |
| `nft … dnat ip prefix to <real>` + `snat ip prefix to <virtual>` (1:1 netmap) | **`binat on <lan> from <real> to any -> <virtual>`** (one bidirectional rule, host bits preserved) |
| `--nat-masquerade` scoped masquerade of real | `nat on <lan> from any to <real> -> (<lan>)` |
| MSS clamp (`tcp option maxseg … rt mtu`) | `scrub on <tun> all max-mss <mtu-40>` |
| hub spoke isolation (`iif tun oif tun drop`) | `block in on <tun> from (<tun>:network) to (<tun>:network)` |
| `--forward-accept` (iptables FORWARD ACCEPT) | `pass on <tun> all` + `pass on <lan> from (<tun>:network) to any` |

`binat` is the clean win: PF's binat is exactly the stateless 1:1 prefix NAT (host-bit preserving)
that the nft `… ip prefix …` netmap implements — a single rule covers both ingress DNAT and egress
SNAT.

> **`--forward-accept` semantics differ on macOS.** There is no Docker `-P FORWARD DROP` on a Mac
> host, so the flag does not "punch a deny" — it emits PF `pass` rules for tun↔LAN so that a PF
> default-block policy still forwards. Detection/warning is PF-policy-based, not iptables-based.

## What shipped (runtime LANDED 2026-06-29)

### Module & build gating
- `src/vpn.rs`: gate flipped to `cfg(all(feature="vpn", any(target_os="linux", target_os="macos")))`.
- Linux-only internals (`nft`/iptables builders, offload, multi-queue, `/proc`) gated `#[cfg(target_os="linux")]` (DEC-M1: byte-for-byte frozen).
- macOS implementations behind `#[cfg(target_os="macos")]`.
- `Cargo.toml`: `tun-rs` available on macOS target.

### TUN creation
- `create_tun`: macOS builds with `DeviceBuilder::new()` WITHOUT `.offload()` or `.multi_queue()`.
- Forces `queues=1`; warns + clamps if `--tun-queues > 1`.
- `--tun-name auto` → kernel assigns `utunN`, read back via `dev.name()` (D7/I-M8).
- Advisory names map to kernel-assigned (e.g., `boreN` → `utunN`).

### Host-config backend (`src/vpn.rs::hostcfg_cmd::macos`)
In-process implementation of `NetConfig::apply`/`Drop`/`stale_reclaim`:

**Builders (pure functions, snapshot-tested):**
- Interface: `cmd_route_add/del`, `cmd_route_get`, `parse_lan_iface`, `cmd_addr_add`,
  `cmd_link_set_up`, `cmd_link_set_mtu`.
- Forwarding: `cmd_sysctl_ip_forward`, `cmd_sysctl_get_ip_forward`.
- PF: `cmd_pf_enable/disable`, `cmd_pf_load_anchor`, `cmd_pf_flush_anchor`, `cmd_pf_show_anchor`.
- Ruleset composer: `pf_ruleset(tun, lan_if, advertised, nat_maps, hub, nat_masquerade,
  forward_accept, mss) -> String`.

**Runtime wiring:**
- `NetConfig::apply`: save ip_forward, add routes, enable PF, load per-link anchor (rules on stdin via temp file), set TUN address/MTU.
- `Drop`: revert routes, flush anchor, restore ip_forward, conditionally disable PF.
- `stale_reclaim`: by-id anchor cleanup + forwarding restoration (no netns → single `/var/run` state file, D5).

**State files (`/var/run/bore-vpn-*`):**
- No netns → single global scope. PF anchor per-link by id; `ip_forward` record global (restored only if no other bore links active).

### CI validation (2026-06-29)
- `.github/workflows/ci.yml` macos-14 job: `cargo build --features vpn` + `clippy -D warnings` + `cargo test --features vpn` GREEN.
- Real `pfctl` on hosted macOS runner accepts the `pf_ruleset` grammar (binat, nat, scrub, block).
- Smoke test `examples/macos_vpn_spike.rs`: create/teardown + apply/revert + leak-then-reclaim PASS.

### Unit tests (cross-platform)
Tests in `src/vpn.rs` (run on Linux + macOS CI):
- `cmd_macos_builders_snapshot` — all builders snapshot-verified.
- `macos_parse_lan_iface_from_route_get` — parses `route -n get` output.
- `macos_pf_ruleset_*` — PF rule generation (plain, netmap, masquerade, hub, forward-accept).

## Degradations on macOS (by platform, not regressions)

- No GSO/GRO offload → single-packet TUN I/O (same as the Linux no-offload fallback).
- No multi-queue → `--tun-queues` forced to 1 (warn if `>1`).
- No `SO_*BUFFORCE` → UDP buffers kernel-clamped; raise `kern.ipc.maxsockbuf`.
- TUN naming: `utunN` only (no arbitrary `boreN` names).

## Troubleshooting

### PF rules
```bash
sudo pfctl -a bore_vpn/<id> -sa          # show the link's PF anchor rules
sudo pfctl -s Anchors                     # list all anchors
sudo pfctl -e                              # enable PF (idempotent)
sudo pfctl -d                              # disable PF (only if no other rules)
```

### Network diagnostics
```bash
route -n get 192.168.1.1                  # LAN egress interface
sysctl net.inet.ip.forwarding             # is forwarding enabled?
ifconfig utun4                            # overlay addr + MTU + stats
```

### Common issues

**`utun0..N` device missing or permission denied:**
- Ensure running as root (`sudo`). utun creation requires root.
- Check SIP (System Integrity Protection) on Apple Silicon: `csrutil status`. If enabled on `/dev/utun*`, you may need to disable SIP or run from a privileged context (rare for normal VPN use).

**PF rules not applied (state mismatch after SIGKILL):**
- If a previous `bore vpn` was killed with `SIGKILL`, the PF anchor may persist.
- Manual cleanup: `sudo pfctl -a bore_vpn/<id> -F all` (replace `<id>` with the tunnel ID).
- Next `bore vpn` run will call `stale_reclaim` automatically to clean up leaked anchors and restore `ip_forward`.

**IP forwarding not restored after crash:**
- `bore vpn` saves the original `net.inet.ip.forwarding` state in `/var/run/bore-vpn-*.fwd-orig`.
- After an unclean exit, check: `cat /var/run/bore-vpn-*.fwd-orig | xargs -I {} sysctl net.inet.ip.forwarding={}`
- The next `bore vpn` run will auto-reclaim.

**Why no `--version` probe on BSD tools:**
- macOS `route`, `sysctl`, `pfctl` have stable, undocumented behavior across macOS 13+ (CLI is not semver-ed).
- Bore does NOT call `route --version` or `pfctl --version` — it assumes the tools exist and uses them directly.
- If tools are missing, the system is broken; error at first use is clearer than a version check.

## Testing

### Unit tests
```bash
cargo test --features vpn --lib vpn::
```
Runs on all platforms (Linux CI + macOS CI). Tests are cross-platform.

### Smoke test (single-host, quick)
```bash
cargo build --examples --features vpn
sudo ./target/debug/examples/macos_vpn_spike
```
Covers: utun creation, PF anchor load, sysctl forwarding, apply/revert RAII, SIGKILL stale-reclaim.

### Manual acceptance (two-host LAN gateway)
See `docs/vpn/VPN_MACOS_ACCEPTANCE.md` (T-MAC-MANUAL) for two-host site↔host + netmap scenarios on real macOS hardware.

## Known degradations (platform, not regression)

- **No GSO/GRO offload** → single-packet TUN I/O (same as Linux no-offload fallback).
- **No multi-queue** → `--tun-queues` forced to 1 (warn if `>1`).
- **No `SO_*BUFFORCE`** → UDP buffers kernel-clamped to `kern.ipc.maxsockbuf` (~208 KiB stock); direct-path throughput ~10 MB/s at 20 ms RTT. Raise sysctl or use `--carriers N` to parallelize relay.
- **TUN naming** → `utunN` only (kernel-assigned, not arbitrary `boreN`).
- **`--forward-accept` semantics** → no Docker `FORWARD DROP` on macOS host (different use case than Linux containers). Flag emits PF `pass` rules; semantic difference documented [above](#rule-mapping-linux-nft--macos-pf).
