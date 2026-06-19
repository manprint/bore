# Admin Dashboard — Tunnel / Secret / Vhost Sections

## Purpose

The three tunnel sections (Tunnels, Secret, Vhost) now share a unified column layout and logic. Vhost was refactored to be structurally identical to Tunnels, with Subdomain replacing Port and vhost-only fields isolated in dedicated columns. This parity ensures consistent UX and simplifies maintenance.

## Column Layout

| Section  | Columns |
|----------|---------|
| **Tunnels** | Port \| Peer \| Flags \| Connections \| Uptime \| TX \| RX \| Notes |
| **Secret** | Role \| Secret ID \| Peer \| Flags \| Connections \| Uptime \| TX \| RX \| Notes |
| **Vhost** | Subdomain \| Peer \| Flags \| Connections \| Uptime \| TX \| RX \| Notes \| Direct Opens \| Headers |

Every row is clickable and opens a detail modal listing all fields—a catch-all for vhost-only data (direct_pool, request/response header pairs) and any field without a dedicated column.

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
| auto_reconnect | ✓ | — | ✓ | Auto-reconnect |
| webserver_log | ✓ | — | ✓ | Weblog |

## Data Sources

**Tunnels & Secret:** Serialized from the admin registry (`EntryView` → `TunnelView`/`SecretView` in `src/admin_views.rs`).

**Vhost:** Serialized from `VhostEntry` (in `src/vhost.rs`), which is now self-sufficient: carries peer, since, notes, basic_auth, udp, auto_reconnect, and webserver_log. Per-subdomain TX/RX counters are incremented in `relay_vhost` and require no admin-registry join.

## Known Gap

The VPN and Config sections have separate panels (`src/admin_ui/panels/vpn.js`, `config.js`) and are **not** part of this parity refactor.
