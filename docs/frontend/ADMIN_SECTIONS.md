# Admin Dashboard — Tunnel / Secret / Vhost Sections

## Purpose

The three tunnel sections (Tunnels, Secret, Vhost) now share a unified column layout and logic. Vhost was refactored to be structurally identical to Tunnels, with Subdomain replacing Port and vhost-only fields isolated in dedicated columns. This parity ensures consistent UX and simplifies maintenance.

## Column Layout

| Section  | Columns |
|----------|---------|
| **Tunnels** | Port \| Peer \| Flags \| Connections \| Uptime \| TX \| RX \| Notes (modal also: Local target, Max-conns) |
| **Secret** | **Grouped by `secret_id` into cards** (mirrors VPN). Each card has a Provider block and a Consumer block; rows: Peer \| Local \| Flags \| Connections \| Uptime \| TX \| RX \| Notes |
| **Vhost** | Subdomain \| Peer \| Flags \| Connections \| Uptime \| TX \| RX \| Notes \| Direct Opens \| Headers (modal also: Local target) |

Every row is clickable and opens a detail modal listing all fields—a catch-all for vhost-only data (direct_pool, request/response header pairs) and any field without a dedicated column.

### Secret grouping (flag-parity)

The Secret section groups entries by `--tcp-secret-id` into one card each
(`src/admin_ui/panels/secret.js`, mirroring `panels/vpn.js`). Provider rows show
the local target `local_host:local_port`; consumer rows show `--local-proxy-port`.
Both roles now surface every applicable flag because the consumer wire message
(`ConnectSecret`) and provider message (`HelloSecret`) carry them — see the
flag table below and `docs/frontend/ADMIN_FLAG_PARITY_PLAN.md`.

## Flags

All three sections use a single shared helper `flagBadges(entry)` in `src/admin_ui/ui.js` for consistency. Badges are shown when the flag is set:

| Flag | Tunnels | Secret | Vhost | Badge |
|------|---------|--------|-------|-------|
| https | ✓ | — | — | HTTPS |
| force_https | ✓ | — | — | Force-HTTPS |
| tls | — | — | ✓ | TLS |
| basic_auth | ✓ | ✓ | ✓ | Basic Auth |
| udp | ✓ | ✓ | ✓ | UDP |
| carriers > 1 | ✓ | ✓ | ✓ | x{N} carriers |
| auto_reconnect | ✓ | ✓ | ✓ | Auto-reconnect |
| webserver_log | ✓ | ✓ (provider) | ✓ | Weblog |
| upnp | — | ✓ | — | UPnP |
| try_port_prediction | — | ✓ | — | Port-Pred |
| nat_udp_preferred_port > 0 | — | ✓ | — | NAT:{port} |

Non-badge display fields surfaced in the modal: `local_host`/`local_port`
(provider local target), `local_proxy_port` (consumer), `max_conns`,
`nat_udp_release_timeout`, `stun_server`. Holepunch flags (`upnp`,
`try_port_prediction`, `nat_udp_*`, `stun_server`) are **secret-tunnel only** —
public tunnels warn-and-ignore them (D4), so they never appear in the Tunnels
section. Secret tunnels have no public port, so `https`/`force_https` do not apply.

## Data Sources

**Tunnels & Secret:** Serialized from the admin registry (`EntryView` → `TunnelView`/`SecretView` in `src/admin_views.rs`).

**Vhost:** Serialized from `VhostEntry` (in `src/vhost.rs`), which is now self-sufficient: carries peer, since, notes, basic_auth, udp, auto_reconnect, and webserver_log. Per-subdomain TX/RX counters are incremented in `relay_vhost` and require no admin-registry join.

## Known Gap

The VPN and Config sections have separate panels (`src/admin_ui/panels/vpn.js`, `config.js`) and are **not** part of this parity refactor.
