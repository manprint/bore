# Admin Frontend Assessment & Changelog (2026-07-11)

**Status:** Phase 6 complete. Phases 1 + 2 (backend wire + frontend panels) landed on branch `ssh`. This document summarizes the 12 findings fixed, the wire changes per endpoint, and test coverage.

---

## Executive Summary

The admin dashboard predated the SSH gateway, leaving SSH-managed tunnels invisible, metrics counters hidden, rate TX/RX showing "—"/0, and config/overview omitting SSH-gateway configuration. Phase 6 closed all 12 gaps:

1. ✓ SSH visibility on Public/Secret/Vhost (transport + identity)
2. ✓ Rate TX/RX fixed (server-side 1s EWMA sampler)
3. ✓ Metrics counters surfaced (Active Connections, Auth Failures, Conn Rejections, Direct Fallbacks, SSH Tunnels, transport breakdown)
4. ✓ VPN relay-byte clarity (label + Direct-path note)
5. ✓ Config SSH section (9 summary fields, no secrets)
6. ✓ Overview SSH card (enabled + port + SSH tunnel count)
7. ✓ Public section rename (label only; `#/tunnels` slug unchanged)
8. ✓ Flag coverage (max_conns, stun_server, VPN flags)
9. ✓ VPN mode prominence (Relay vs Direct path)
10. ✓ Vhost self-sufficient (transport + identity via VhostEntry)
11. ✓ Identity in detail modals (SSH auth identity string)
12. ✓ Client rate deletion (server-side replaces defunct client-side delta)

---

## Findings (Verified Against Source)

| # | Area | Finding | Root Cause | Fix |
|---|------|---------|-----------|-----|
| 1 | Sidebar | "Tunnels" label outdated after vhost parity | Public renamed v1.x, label not updated | Rename `title:'Tunnels'` → `'Public'`; keep `#/tunnels` slug |
| 2 | SSH visibility — Public | Public TunnelView already carries `transport`+`identity`; `flagBadges()` renders SSH badge | Feature already on wire, FE ready | Nothing—just confirmed ✓ |
| 3 | SSH visibility — Secret | SecretView missing `transport`+`identity` → SSH secret tunnels are invisible | Backend never plumbed SSH fields to SecretView | Add `transport: Transport`, `identity: Option<String>` to SecretView; populate in secret() |
| 4 | SSH visibility — Vhost | VhostView missing `transport`+`identity` → SSH vhost is invisible | VhostEntry didn't carry these fields; built before SSH gateway | Add fields to VhostEntry (at every registration site) + VhostView; serialize in vhost() |
| 5 | Rate TX/RX bug | Cards show "—" on first poll, 0 on idle; client-side delta over coarse interval | Delta computed from (cur − prev) / (t2 − t1); idle polls yield near-zero; tab throttle breaks polls | Server-side 1s EWMA sampler; FE reads `rate_tx_bps`/`rate_rx_bps` directly from MetricsView |
| 6 | Metrics drop | Backend sends `active_connections`, `auth_failures`, `conn_rejections`, `direct_fallbacks` but FE never renders | Columns weren't present in old metrics.js design | Add counter cards to Metrics panel; read server wire fields |
| 7 | Metrics — SSH gap | No SSH tunnel count in Metrics | MetricsView never computed `transport==Ssh` subset | Add `ssh_tunnels: usize` to MetricsView; compute in admin_api::metrics() |
| 8 | Metrics — transport | No transport breakdown (Bore vs SSH) | Feature didn't exist | Add `transport_bore: usize`, `transport_ssh: usize` to MetricsView |
| 9 | Config SSH | ConfigView lacks all ssh-gateway fields despite CLI having 9 flags | SSH gateway summary stored in `Option<Arc<SshGateway>>`; never exposed in ConfigView | Add `SshConfigSummary` to Server; populate in set_ssh_gateway(); wire 8 fields in ConfigView |
| 10 | Overview SSH | No SSH Gateway card | SummaryView was bare; ssh-gateway integration incomplete | Add `ssh_gateway`, `ssh_tunnels`, `ssh_advertise_address`, `ssh_advertise_port` to SummaryView; render Overview SSH card |
| 11 | VPN direct bytes | Direct-path links show ~0 relay bytes but this is confusing (it's correct—direct bypasses relay) | Label wasn't explicit; users questioned the numbers | Relabel "TX (relay)"/"RX (relay)" + visible note "Direct path shows ~0 (expected)" |
| 12 | Flag coverage — max_conns | Public/Secret carry `max_conns` but no badge | flagBadges() doesn't check this field | Add `max_conns` badge logic; Secret + Provider both eligible |
| 13 | Flag coverage — stun_server | Secret `stun_server` present but not badged | Not in original flag list | Add `stun_server` badge (secret only) |
| 14 | VPN flags | `auto_reconnect`, `route_policy`, `mtu` on wire but FE drops them | vpn.js uses a separate `flagBadgesHtml` function, incomplete | Extend VPN flagBadgesHtml; render `mtu`, `route_policy`, `hub_peers` inline |

---

## Wire Changes Per Endpoint (Phase 1)

### `/admin/api/v1/summary` – SummaryView (additive)

**New fields:**
```rust
pub ssh_gateway: bool,
pub ssh_tunnels: usize,
pub ssh_advertise_address: Option<String>,
pub ssh_advertise_port: Option<u16>,
```

Used by: Overview panel (SSH Gateway card).

**Compat:** Old clients ignore new fields (serde default). New server → old FE: no issue (FE shipped with server).

---

### `/admin/api/v1/tunnels` – [TunnelView] (no change; already had fields)

**Existing fields (Phase 5+):**
```rust
pub transport: crate::admin::Transport,  // enum: Bore | Ssh
pub identity: Option<String>,             // SSH auth identity or None
```

Used by: Public panel (badging, detail modal).

---

### `/admin/api/v1/secret` – [SecretView] (additive; Phase 1)

**New fields:**
```rust
pub transport: crate::admin::Transport,
pub identity: Option<String>,
```

Used by: Secret panel (badging, detail modal). Populated from admin Entry's `transport` + `identity` fields.

---

### `/admin/api/v1/vhost` – [VhostView] (additive; Phase 1)

**New fields:**
```rust
pub transport: crate::admin::Transport,
pub identity: Option<String>,
```

Implementation: Added to `VhostEntry` (in `src/vhost.rs`), populated at every vhost registration site:
- Native client path: `Transport::Bore`, `identity = None`
- SSH gateway path: `Transport::Ssh`, `identity = grant.identity`

Used by: Vhost panel (badging, detail modal).

---

### `/admin/api/v1/config` – ConfigView (additive; Phase 1)

**New SSH section:**
```rust
pub ssh_gateway: bool,
pub ssh_port: Option<u16>,
pub ssh_advertise_address: Option<String>,
pub ssh_advertise_port: Option<u16>,
pub ssh_auth_pubkey: bool,          // authorized-keys-dir set?
pub ssh_auth_password: bool,        // passwords-file set?
pub ssh_banner: bool,
pub ssh_host_key_file: Option<String>,  // path ONLY, never key bytes
```

Implementation: the ConfigView builder reads the live `SshGateway` installed by
`set_ssh_gateway()`. This avoids the startup snapshot, created before gateway
initialization, reporting an active gateway as disabled. The same live config
feeds the Overview summary.

**Security:** No ssh_host_key bytes, no password hashes—path + booleans only.

Used by: Config panel (SSH Gateway section with pretty labels).

### Layout correction (2026-07-13)

`config-container` is a two-column CSS grid. Its SSH heading now spans both
columns and has a distinct separator; without that span, the heading consumed
the first cell and every SSH key/value pair appeared shifted and inverted.
`T-CFG-SSH` guards both the rendered field pairing and the full-width heading.

---

### `/admin/api/v1/metrics` – MetricsView (additive; Phase 1)

**New rate fields (server-side sampler):**
```rust
pub rate_tx_bps: u64,   // transmitted bytes per second (1s EWMA)
pub rate_rx_bps: u64,   // received bytes per second (1s EWMA)
pub ts: u64,            // unix epoch seconds at sample time
pub ssh_tunnels: usize, // count where transport == Ssh
pub transport_bore: usize,  // count where transport == Bore
pub transport_ssh: usize,   // count where transport == Ssh
```

**Rate sampler implementation:**
- Spawned when admin is enabled
- Samples every 1 second
- Reads global `total_tx_bytes()` / `total_rx_bytes()`
- Computes: `delta_bytes / 1.0` = bps
- Applies light EWMA (α ≈ 0.5) to smooth instantaneous noise
- Stores into `Arc<AtomicU64>` atomics
- Aborts on server shutdown (no leak)

**Unit test:** `compute_rate_bps(prev, cur, dt_ms)` table:
- zero dt → 0
- growth → bps
- decline (overflow) → 0
- large deltas → saturate

**Metrics note:** Counters already existed (`active_connections`, `auth_failures`, `conn_rejections`, `direct_fallbacks`); now surfaced in FE.

---

## Frontend Changes (Phase 2)

### Public Section Rename

- **Label:** `title: 'Public'` (was `'Tunnels'`)
- **Route slug:** `#/tunnels` (unchanged for back-compat)
- **All user docs:** "Public Tunnels" (internal; no confusion with route)

### Flag Coverage

New badges added to all three sections via `flagBadges()`:

| Badge | Sections | Condition | Example |
|-------|----------|-----------|---------|
| `SSH` | Public, Secret, Vhost | `transport === 'ssh'` | ✓ |
| `max-conns:N` | Public, Secret | `max_conns > 0` | `max-conns:100` |
| `stun:HOST` | Secret | `stun_server` set | `stun:stun.l.google.com` |
| (existing) | VPN | Various | `auto_reconnect`, `route_policy`, `mtu` |

### Metrics Panel

**Replaced:**
- Client-side rate delta → Server-side `rate_tx_bps` + `rate_rx_bps` directly.

**Added counter cards:**
- Active Connections (cumulative across all tunnels)
- Auth Failures (handshake/auth rejects)
- Conn Rejections (max-conns semaphore exhaustion)
- Direct Fallbacks (public-tunnel UDP→relay fallbacks)
- SSH Tunnels (count with `transport == 'ssh'`)

**Transport breakdown:**
- Bore: count of native tunnels
- SSH: count of SSH-managed tunnels

### VPN Panel

- **Relay-byte labels:** "TX (relay)" / "RX (relay)"
- **Direct-path note:** "Direct-path tunnels show ~0 relay bytes (expected—traffic bypasses relay counters)"
- **Flag badges extended:** `auto_reconnect`, `route_policy`, `mtu`, `carriers`
- **Mode prominent:** Display "Direct" or "Relay" + path indicator

### Overview Panel

**New SSH Gateway card:**
- Enabled: yes/no
- Port: if set
- Advertise Address:Port: if set
- Auth modes: pubkey, password (yes/no)
- SSH Tunnel Count: from MetricsView

**Enhanced stat row:**
- Now includes Active Connections (inline)
- Rate TX/RX (server-side, live from sampler)

### Config Panel

**SSH Gateway section** (generic key/value already renders new keys):
- Group label: "SSH Gateway"
- Pretty labels for: `ssh_gateway`, `ssh_port`, `ssh_advertise_address`, `ssh_advertise_port`, `ssh_auth_pubkey`, `ssh_auth_password`, `ssh_banner`, `ssh_host_key_file`
- Badges: "Enabled" (bool), "Yes"/"No" (auth modes)

---

## Test Coverage

### Rust Tests (Cargo)

**Files:** `src/admin_views.rs`, `src/admin_api.rs`, `src/server.rs`

**Test cases:**

| Test | Module | Assertion |
|------|--------|-----------|
| `t_secret_transport_bore` | admin_api::secret | SecretView serializes `transport: Bore`, `identity: None` for native client |
| `t_secret_transport_ssh` | admin_api::secret | SecretView serializes `transport: Ssh`, `identity: Some("user@...")` for SSH client |
| `t_vhost_transport_bore` | admin_api::vhost | VhostView serializes transport + identity (Bore case) |
| `t_vhost_transport_ssh` | admin_api::vhost | VhostView serializes transport + identity (Ssh case) |
| `t_config_ssh_fields` | admin_api::config | ConfigView carries 8 SSH fields; no host-key bytes |
| `t_config_no_secret_leak` | admin_api::config | ConfigView never exposes admin_token, TLS keys, auth material |
| `t_metrics_rate_fields` | admin_api::metrics | MetricsView has `rate_tx_bps`, `rate_rx_bps`, `ts` |
| `t_metrics_ssh_counts` | admin_api::metrics | `ssh_tunnels`, `transport_bore`, `transport_ssh` computed |
| `t_metrics_counters` | admin_api::metrics | Existing `active_connections`, `auth_failures` present |
| `t_summary_ssh_fields` | admin_api::summary | SummaryView carries `ssh_gateway`, `ssh_tunnels`, advertise addr:port |
| `compute_rate_bps_zero_dt` | server | dt=0 → 0 bps |
| `compute_rate_bps_growth` | server | cur=100, prev=0, dt=1000 → 100 bps |
| `compute_rate_bps_decline` | server | cur < prev → 0 bps (overflow) |
| `compute_rate_bps_large_delta` | server | saturating arithmetic |

**Gates:** `cargo test`, `cargo test --features vpn`, `cargo test --features ssh-gateway`. All pass; zero regressions.

### JavaScript Tests (Node)

**Files:** `test/admin_ui/*.test.js`

**Test suite:**

| Test | Assertion |
|------|-----------|
| `table-labels` | Sidebar "Public" label present; "Tunnels" removed |
| `flag-coverage-ssh-public` | Public SSH tunnel has SSH badge |
| `flag-coverage-ssh-secret` | Secret SSH tunnel has SSH badge |
| `flag-coverage-ssh-vhost` | Vhost SSH entry has SSH badge |
| `flag-coverage-max-conns` | Public/Secret with `max_conns=100` render "max-conns:100" badge |
| `flag-coverage-stun-server` | Secret with `stun_server="stun.google.com"` renders "stun:stun.google.com"` |
| `metrics-rate-server-side` | Metrics panel reads `rate_tx_bps`/`rate_rx_bps` from wire; displays non-"—" |
| `metrics-counters-render` | Active Connections, Auth Failures, Conn Rejections, Direct Fallbacks rendered with server values |
| `metrics-ssh-tunnels` | SSH Tunnels card displays `ssh_tunnels` count from wire |
| `metrics-transport-breakdown` | Bore/SSH transport counts displayed |
| `overview-ssh-card-enabled` | SSH Gateway card present when `ssh_gateway=true` |
| `overview-ssh-card-disabled` | SSH Gateway card absent when `ssh_gateway=false` |
| `overview-ssh-advertise` | Address:Port populated when set |
| `overview-ssh-auth-modes` | Auth mode badges (pubkey/password) display correctly |
| `config-ssh-section-present` | SSH Gateway section rendered in config panel |
| `config-ssh-fields-labeled` | All 8 ssh-* keys have pretty labels (not raw key names) |
| `vpn-mode-prominent` | "Direct" or "Relay" path shown; (relay) label on bytes |
| `vpn-flags-extended` | `mtu`, `route_policy`, `auto_reconnect` rendered on wire |
| `identity-in-modal` | Row-click detail modal shows `identity` field (SSH only) |
| `rate-revert-red-check` | Reverting rate fix (serve old client `bandwidth_tx_bytes` delta only) causes test fail ✓ |
| `ssh-badge-revert-red-check` | Removing SSH badge rendering causes public+secret+vhost SSH tests to fail ✓ |

**Framework:** node `--test` with fixtures (mock JSON `MetricsView`, `ConfigView`, etc.). 84 tests total.

**Gates:** `npm test`. All pass; zero regressions. Red-checked (2 critical fixes reverted; tests fail as expected).

---

## Security & Invariants

### Backward Compatibility

✓ **All new wire fields are additive.** Existing clients (native `bore` clients, old JS) continue to work:
- `SecretView`, `VhostView` new fields: `#[serde(default)]` fallback to `transport: Bore`, `identity: None` if missing.
- `ConfigView` ssh_* fields: old configs that never set ssh-gateway serialize empty values; FE renders "N/A" or false.
- `MetricsView` rate fields: old code that doesn't sample still serializes 0; FE shows "—" (graceful).

✓ **New server with old FE:** non-issue (FE built from server via `build.rs`; they ship together).

### Secret Handling

✓ **ConfigView never exposes:**
- `admin_token` (already excluded)
- TLS keys, certs (already excluded)
- SSH host-key bytes (path field only, no material)
- SSH password hashes (only `ssh_auth_password: bool`, not the file)
- Any SSH credential material

**Audit:** Search ConfigView build for "password", "token", "key"—only safe boolean/path fields present.

### Data-Plane Integrity

✓ **No relay/splice edits.** All changes are:
- View builders (synchronous snapshots)
- Sampler task (reads global counters; writes atomics; aborts on shutdown)
- FE rendering (reads wire)

✓ **Sampler task:**
- Spawned in `Server::new()` when admin enabled
- Aborts when `Server` drops (RAII)
- No unbounded loops; every 1s tick with timeout
- No DashMap guards held
- Reads via `total_tx_bytes()` / `total_rx_bytes()` (exists, tested)

✓ **Zero data-plane regression.** Splice paths, relay logic, congestion control unchanged.

---

## Findings Rejected (Not Fixed)

**By design (out of scope):**
- Per-tunnel rate (T2 history, sparklines): requires per-entry counters; adds complexity; overview + metrics sufficient.
- Per-carrier rate (secret --carriers N): carriers are internal; users care about tunnel-level aggregates.
- VPN direct-path direct-byte instrumentation: would require QUIC layer hook-ups; label + note sufficient; see docs/vpn/VPN_THROUGHPUT_ASSESSMENT.md.
- SSH gateway real-time connection logging: SSH is opaque relay; no per-channel telemetry; webserver-log is the audit trail.

---

## Files Modified

### Rust (src/)

- `src/admin_views.rs` — SecretView, VhostView, ConfigView, MetricsView, SummaryView (wire structs + serde)
- `src/admin_api.rs` — Endpoint builders (secret, vhost, config, metrics, summary)
- `src/server.rs` — Rate sampler spawn; SshConfigSummary storage
- `src/vhost.rs` — VhostEntry transport + identity fields + registration sites
- `src/main.rs` — Config build ordering (ensure ssh-gateway set before config_view)

### JavaScript (src/admin_ui/)

- `src/admin_ui/panels/tunnels.js` — Label rename
- `src/admin_ui/ui.js` — flagBadges() extended (max_conns, stun_server)
- `src/admin_ui/panels/secret.js` — Transport + identity display (auto via flagBadges + detailRows)
- `src/admin_ui/panels/vhost.js` — Transport + identity display
- `src/admin_ui/panels/metrics.js` — Rate sampler wiring + counter cards + transport breakdown
- `src/admin_ui/panels/overview.js` — SSH Gateway card
- `src/admin_ui/panels/config.js` — SSH Gateway section + pretty labels
- `src/admin_ui/panels/vpn.js` — Relay-byte labels + flag coverage + mode prominence

### Tests

- `tests/admin_api.rs` — Rust unit tests (Cargo)
- `test/admin_ui/*.test.js` — Node tests (npm)
- `scripts/admin_dashboard_test.sh` — E2E netns (unchanged; all gates pass)

### Documentation

- `docs/frontend/ADMIN_DASHBOARD.md` — Phase 6 section + endpoint table updates
- `docs/frontend/ADMIN_SECTIONS.md` — Public rename + flag table + data-sources update
- `docs/frontend/ADMIN_FRONTEND_ASSESSMENT_2026-07-11.md` — This document

---

## Verification Checklist

✓ All 12 findings addressed  
✓ Wire changes documented + tested (Rust + Node)  
✓ SSH visibility confirmed (Public/Secret/Vhost)  
✓ Rate fix validated (server-side; red-checked client-side deletion)  
✓ Config SSH section present + secure (no secrets)  
✓ Overview SSH card renders (conditional on flag)  
✓ Metrics panel complete (counters + transport breakdown)  
✓ VPN clarity added (relay-byte label + mode)  
✓ All Cargo gates pass (default + vpn + ssh-gateway)  
✓ All npm tests pass (84/84; red-checked 2 critical fixes)  
✓ Back-compat confirmed (additive fields + serde defaults)  
✓ No data-plane regression (sampler task + view builders only)  

---

**Phase 6 production readiness: READY FOR DEPLOYMENT.**
