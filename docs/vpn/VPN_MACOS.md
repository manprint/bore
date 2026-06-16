# bore vpn on macOS — backend reference

Companion to the [operational plan](VPN_MACOS_PORT_PLAN.md). This documents the **macOS
host-config backend** design and the **groundwork already landed**.

> **Status (2026-06-16):** groundwork only — the pure command/ruleset builders + their snapshot
> tests are in `src/vpn.rs::hostcfg_cmd::macos`, compiled and tested on the Linux CI. The runtime
> that wires them (utun creation, PF anchor load, sysctl save/restore, RAII teardown) and the
> module `cfg` gate flip are **pending** the Phase 0 spike on a real Mac. `bore vpn` does **not**
> run on macOS yet.

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

## What landed (groundwork)

In `src/vpn.rs::hostcfg_cmd::macos` (pure functions, snapshot-tested on Linux):

- Interface: `cmd_route_add/del`, `cmd_route_get`, `parse_lan_iface`, `cmd_addr_add`,
  `cmd_link_set_up`, `cmd_link_set_mtu`.
- Forwarding: `cmd_sysctl_ip_forward`, `cmd_sysctl_get_ip_forward`.
- PF: `pf_anchor`, `cmd_pf_enable/disable`, `cmd_pf_load_anchor`, `cmd_pf_flush_anchor`,
  `cmd_pf_show_anchor`.
- Ruleset composer: `pf_ruleset(tun, lan_if, advertised, nat_maps, hub, nat_masquerade,
  forward_accept, mss) -> String` — the macOS twin of `gateway_nft_cmds`.

`Cargo.toml`: `tun-rs` is now available on the macOS target (`cfg(any(linux, macos))`), so a future
gate flip can compile the utun path.

Tests (run on the Linux CI box, no Mac required): `cmd_macos_builders_snapshot`,
`macos_parse_lan_iface_from_route_get`, `macos_pf_ruleset_plain_only`,
`macos_pf_ruleset_netmap_uses_binat_not_masquerade`,
`macos_pf_ruleset_nat_masquerade_and_hub_and_forward_accept`.

## What is pending (needs a Mac)

1. **Phase 0 spike** — verify utun read/write + PF `binat`/`scrub`/`block` grammar on macOS 13+
   (Apple Silicon). The PF syntax above is **provisional** until then.
2. **Module gate flip** — `cfg(all(feature="vpn", target_os="linux"))` →
   `…, any(target_os="linux", target_os="macos")`, with the Linux-only internals re-gated.
3. **macOS runtime** — `create_tun` (utunN, no offload/multi-queue) + a `#[cfg(target_os="macos")]`
   `NetConfig::apply`/`Drop`/`stale_reclaim` using the builders above (temp-file PF load, sysctl
   save/restore, anchor teardown).
4. **macOS e2e** — `scripts/vpn_macos_test.sh` (utun + `feth` LAN host) on a GitHub `macos` runner.

## Degradations on macOS (by platform, not regressions)

- No GSO/GRO offload → single-packet TUN I/O (same as the Linux no-offload fallback).
- No multi-queue → `--tun-queues` forced to 1 (warn if `>1`).
- No `SO_*BUFFORCE` → UDP buffers kernel-clamped; raise `kern.ipc.maxsockbuf`.
- TUN naming: `utunN` only (no arbitrary `boreN` names).

## Diagnostics (once runtime lands)

```bash
sudo pfctl -a bore_vpn/<id> -sa          # show the link's PF anchor rules
route -n get 192.168.1.1                  # LAN egress interface
sysctl net.inet.ip.forwarding             # forwarding enabled?
ifconfig utun4                            # overlay addr + MTU
```
