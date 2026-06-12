# VPN Multi-Client (1:N Hub) — Design & Implementation Plan

> **Status:** planning (no code written). Implementation handoff doc.
> **Authoring model:** Opus 4.8 (architecture).
> **Implementation models:** annotated per sub-phase (Haiku 4.5 = mechanical/bulk, Sonnet 4.6 = features/refactor/tests, Opus 4.8 = architecture review gate only).
> **Target:** correct 1:N functionality + max performance / min latency on both UDP-direct and TCP-relay data paths. Minimize token usage during implementation (delegate mechanical sub-phases to Haiku).

---

## 1. Context & problem

`bore vpn` links are currently **strictly 1:1**. The server holds a registry
`VpnRegistry = Arc<DashMap<String, VpnProviderEntry>>` (`src/vpn_server.rs:27`). A
listener registers via `HelloVpn` and waits on a one-shot channel
(`VpnProviderEntry.pair_tx: oneshot::Sender<VpnPairMsg>`, `src/vpn_server.rs:38-54`).
When a connector arrives, `serve_vpn_connector` **removes** the entry and fires the
one-shot (`src/vpn_server.rs:630-637`). A second connector with the same `id` gets
`"vpn listener '{id}' not found"` (`src/vpn_server.rs:485-496`).

Each pair is a point-to-point `/30` (listener `.1`, connector `.2`,
`src/vpn_server.rs:516-547,591-594`). Each client process runs exactly **one** TUN,
**one** bridge, **one** peer (`src/vpn.rs:188-376` listener, `src/vpn.rs:886+` connector).

### Goal

host-D (`vpn listen`) must serve an **arbitrary number** of connectors (host-A…E),
and **each connector independently chooses which of host-D's advertised routes to
accept/refuse**. Default: a connector accepts **nothing** unless it opts in. Both data
paths keep working with current performance characteristics.

### Reference scenario (final acceptance test)

```
host-D (listen)  --advertise 192.168.4.0/24,10.10.0.0/16   --max-clients 8
host-A (connect) --accept-all-routes --refuse-routes 10.10.0.0/16   → reaches 192.168.4.0/24 only
host-B (connect) --accept-all-routes                                → reaches both
host-C (connect) --accept-all-routes --refuse-routes 192.168.4.0/24 → reaches 10.10.0.0/16 only
host-E (connect) (no route flags / --refuse-all-routes)             → reaches host-D overlay only
```

---

## 2. Approved design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | **Single TUN + per-peer router** on the hub. One `bore0`, one overlay subnet, hub=`.1`, each spoke a unique `.N`. | Hub uplink routes by dst-IP → per-peer `LinkSender`; per-peer downlink writes shared TUN. |
| **D2** | **Spoke isolation.** Spokes reach host-D + advertised routes only. | nft/iptables `FORWARD -i bore0 -o bore0 -j DROP` on the hub. |
| **D3** | **Client-local route filter only.** No hub-side ACL. | Connector installs only accepted routes; refused subnets have no route. |
| **D4** | **Hub-and-spoke only (v1).** Only the listener advertises. | Connector `--advertise` rejected in multi-client mode. Reverse site-to-site deferred. |
| **D5** | **Opt-in via `--max-clients <N>` (default 1).** `1` = today's path byte-for-byte. `>1` = hub mode. | Preserves zero-regression invariant; bounds spokes. |
| **D6** | **Multi-client requires pool addressing.** Server carves a hub subnet from `--vpn-pool`. | Static `/30` addressing stays a 1:1-only feature. |
| **D7** | **Hub subnet `/24` default**, server flag `--vpn-hub-prefix`. | 253 spokes per hub by default. |
| **D8** | **`peer_id: u32`**, monotonic within a hub session, server-assigned. | Threads through `VpnPeerJoin`, relay substream header, UDP punch. |
| **D9** | **Route match = exact-or-subset.** | refuse/accept remove/keep advertised CIDRs equal-to-or-contained-in the flag CIDR. Cannot accept a CIDR not advertised. |

---

## 3. Target architecture

### 3.1 Addressing

- **Hub registration:** `HelloVpn{ max_clients>1, addr: Pool }` → server allocates a
  `/vpn-hub-prefix` (default `/24`) block from `--vpn-pool`. Hub overlay = first host
  (`.1`), `prefix = 24`. Hub TUN configured `10.99.x.1/24` so the whole subnet routes
  into `bore0`.
- **Connector pairing:** server allocates the next free host in the hub's subnet;
  `VpnReady{ assigned=.N, prefix=24, peer_overlay=hub .1, peer_advertised=hub.advertised }`.
  Connector TUN = `10.99.x.N/24`, gateway = hub `.1`.
- The **connector stays a single-peer link** structurally — it only ever talks to the hub.

### 3.2 Relay demux — the central mechanism

Connectors tag substreams `[STREAM_READY, tag, idx?]` (unchanged — see
`link::connect_relay_multi`, `src/vpn.rs:2903-2935`). The server's `vpn_relay`
(`src/vpn_server.rs:855-873`) today strips the connector's `STREAM_READY` and re-emits a
bare `STREAM_READY` to the listener, copying the rest verbatim.

**Change (hub mode only):** the server writes `[STREAM_READY, peer_id:u32 BE]` to the
listener, then `copy_bidirectional` forwards the connector's `[tag, idx, payload…]`
verbatim. The hub's accept loop reads `STREAM_READY + peer_id + tag + idx` and routes the
substream to the matching peer link.

```
connector → server  : [STREAM_READY][tag][idx?][payload…]        (UNCHANGED)
server    → hub      : [STREAM_READY][peer_id u32][tag][idx?][payload…]   (peer_id injected)
```

All spokes' substreams share the hub's single yamux control session, demuxed by `peer_id`.
**Connector byte-stream is byte-for-byte unchanged**; only the server↔hub framing gains
`peer_id`. The relay stays AEAD-opaque (server injects framing, never plaintext).

### 3.3 Hub data plane (new — D1)

```
                       ┌─────────────── hub (host-D) ───────────────┐
   spoke A ⇄ relay/direct ⇄ peerLink A ─┐                            │
   spoke B ⇄ relay/direct ⇄ peerLink B ─┤  downlinks (N tasks) ─────▶ TUN bore0 (10.99.x.1/24)
   spoke C ⇄ relay/direct ⇄ peerLink C ─┘                            │  │
                                                                     │  ▼ kernel ip_forward
   PeerTable: HashMap<Ipv4Addr, PeerHandle>                          │  ├─ local (hub host)
       .2 → A   .3 → B   .4 → C                                      │  ├─ LAN egress (advertised) + masquerade
                                                                     │  └─ bore0→bore0 ⇒ DROP (spoke isolation)
   router uplink (1/queue): TUN read → dst IP → PeerTable → peer.sender.send
                       └─────────────────────────────────────────────┘
```

- **One TUN** with the hub subnet. Gateway setup (`ip_forward` + nft masquerade for
  advertised subnets) applied **once** at hub start; spoke-isolation drop rule added once.
- **`PeerHandle`** holds a **swappable** `LinkSender` (use `arc_swap::ArcSwap<Arc<LinkSender>>`
  or `Arc<Mutex<LinkSender>>`) + a shutdown trigger + the peer's overlay IP + `peer_id`.
- **Shared router uplink** (one task per TUN queue): read packet → parse dst IPv4 (offset
  16..20 of the IPv4 header) → `PeerTable` lookup → `peer.sender.load().send_batch(&[pkt])`.
  No match → drop (count it). **The router never restarts on a path switch** — the swap
  happens inside `PeerHandle.sender`. This is the key departure from the current bridge,
  which aborts+respawns the uplink on every relay↔direct switch (`src/vpn.rs:3596-3604`).
- **Per-peer (on `VpnPeerJoin`):** gather the peer's `2×carriers` substreams from a
  `peer_id`-keyed pending buffer, `crypto::derive_keys_listener(secret, session_nonce)`,
  `link::make_relay_multi` → store the sender in `PeerTable`, spawn a downlink
  (`bridge::run_downlink` → shared TUN `devs[0]`), spawn a per-peer direct-upgrade task. On
  upgrade: swap sender → `Direct` + add a direct downlink; on fallback: swap back (per-peer
  mini state machine reusing `bridge::bridge_next_action`, `src/vpn.rs:3379-3396`).
- **On `VpnPeerLeave` / link death:** remove from `PeerTable`, abort the peer's tasks,
  preserve the rest of the hub.

### 3.4 Reuse map (do not reinvent)

| Need | Reuse | Location |
|------|-------|----------|
| Relay link build | `link::make_relay_multi` | `src/vpn.rs:2856` |
| Direct link build | `link::make_direct` | `src/vpn.rs:2835` |
| Downlink pump | `bridge::run_downlink` | `src/vpn.rs:~3714` |
| Uplink pump (adapt → router) | `bridge::run_uplink_single`/`_offload` | `src/vpn.rs:3645+` |
| Path-switch transition table | `bridge::bridge_next_action` | `src/vpn.rs:3379` |
| Direct punch/upgrade | `direct_upgrade_task` + `DirectUpgradeCtx` | `src/vpn.rs:345-352,560+` |
| Key derivation | `crypto::derive_keys_listener/_connector` | `src/vpn.rs:1296-1313` |
| Host config (extend) | `hostcfg::NetConfig` | `src/vpn.rs:2244-2498` |
| UDP broker registry | `secret::UdpRegistry`/`UdpReg` | `src/secret.rs:72-99` |
| Server pool | `VpnPool`/`VpnPoolHandle`/`VpnLeaseGuard` | `src/vpn_server.rs` |

---

## 4. New CLI flags

### 4.1 `bore vpn connect` (`src/main.rs` `VpnConnectArgs`, lines 972-1095)

```
--accept-routes <CIDR,...>   Accept exactly these advertised routes (exact-or-subset of
                             what the listener advertises). value_delimiter=','.
--accept-all-routes          Accept every route the listener advertises.
--refuse-routes <CIDR,...>   Subtract these from the accepted set. Use with
                             --accept-all-routes for "all except". value_delimiter=','.
--refuse-all-routes          Accept nothing (== default; for explicit, self-documenting scripts).
```

- **Default (no flag): accept nothing.** A connector with no route flag is host-only: it
  reaches the hub overlay IP and nothing else.
- Resolution: `final = (accept_all ? advertised : (accept_routes ∩ advertised)) − refuse_routes`,
  using exact-or-subset matching (D9). `--refuse-all-routes` forces `final = ∅`.
- Conflicts: `--accept-all-routes` + `--accept-routes` → accept-all wins (warn). `--refuse-all-routes`
  overrides everything (warn if combined with accept flags). A CIDR in `--accept-routes`
  not covered by any advertised CIDR → warn and skip (no route to nowhere).

### 4.2 `bore vpn listen` (`VpnListenArgs`, lines 848-970)

```
--max-clients <N>   Max concurrent connectors (default 1). 1 = legacy 1:1 path
                    (byte-for-byte unchanged). >1 = hub mode (single TUN, overlay subnet).
```

### 4.3 `bore server` (`src/main.rs` lines 541-559, `#[cfg(feature = "vpn")]`)

```
--vpn-hub-prefix <P>   Overlay subnet prefix allocated per hub from --vpn-pool (default 24).
```

---

## 5. New protocol (all additive, `#[serde(default)]`, wire-compatible)

In `src/shared.rs`:

```rust
// ClientMessage::HelloVpn (lines 684-696) — add:
#[serde(default)] max_clients: u16,   // 0/absent → treated as 1 (legacy 1:1)

// ConnectVpn (lines 699-712) — no new field required (connector has a single peer).

// ServerMessage — add two variants (server → hub):
VpnPeerJoin {
    peer_id: u32,
    peer_overlay: Ipv4Addr,
    peer_advertised: Vec<Ipv4Net>,     // empty in v1 (hub-and-spoke; connectors don't advertise)
    session_nonce: [u8; UDP_NONCE_LEN],
    carriers: u16,
},
VpnPeerLeave { peer_id: u32 },

// ServerMessage::UdpPunch (lines 760-772) — add:
#[serde(default)] peer_id: u32,        // hub: which peer this punch belongs to; 0 in 1:1

// ClientMessage::UdpCandidateOffer — carry peer_id in hub mode. Either add a field to the
// existing UdpCandidateOffer struct (src/shared.rs:368-374) behind #[serde(default)], or add
// a parallel VpnUdpCandidateOffer { peer_id, offer }. Prefer extending UdpCandidateOffer with
// #[serde(default)] peer_id: u32 to avoid a second message type.
```

**Wire-compat note:** every new field uses `#[serde(default)]`; new `ServerMessage`
variants are only ever sent to a hub (`max_clients>1`), which is a new client, so an old
client never receives an undeserializable variant. Mirror the `admin_v2` gating pattern
(`VpnReady.admin_v2`, `src/shared.rs:834-857`) if a capability gate is needed.

---

## 6. Implementation phases

**Global rules (CLAUDE.md):** tests first or alongside; every sub-phase must pass
`cargo fmt --check`, `cargo clippy --features vpn -- -D warnings`, `cargo test --features vpn`;
zero regressions; update docs when behavior/APIs change; print the model used per sub-task.

Each sub-phase lists: **model**, **files**, **change**, **unit tests**, **e2e tests**, **done-criteria**.

---

### Phase 0 — Protocol & flag scaffolding (no behavior change)

> Pure additive. After this phase the binary behaves exactly as today; new fields/flags are
> parsed and serialized but unused. Safe to land independently.

#### 0.1 Add protocol fields/messages
- **Model:** Haiku (mechanical struct/enum edits).
- **Files:** `src/shared.rs` (HelloVpn, ServerMessage variants, UdpPunch, UdpCandidateOffer).
- **Change:** add fields/variants from §5 with `#[serde(default)]`. Add doc comments.
- **Unit tests** (`src/shared.rs` `#[cfg(test)]` or `tests/`):
  - `helloVpn_serde_roundtrip_with_and_without_max_clients` (old payload without the field
    deserializes to `max_clients == 0`).
  - `vpn_peer_join_leave_serde_roundtrip`.
  - `udp_punch_peer_id_default_zero` (legacy payload → `peer_id == 0`).
- **e2e:** none (no behavior).
- **Done:** gates green; existing `tests/vpn_server_test.rs` unchanged and passing.

#### 0.2 Add connector route flags + listener `--max-clients` + server `--vpn-hub-prefix`
- **Model:** Haiku (clap boilerplate; mirror existing `--advertise` parsing, `src/main.rs:877-885`).
- **Files:** `src/main.rs` (`VpnConnectArgs`, `VpnListenArgs`, server flags), plumb into
  `vpn::VpnConnectArgs`/`VpnListenArgs` structs in `src/vpn.rs`.
- **Change:** add the flags from §4; carry them into the vpn arg structs (new fields). Reuse
  `Ipv4Net::from_str` (`src/shared.rs:506-521`) and `value_delimiter=','`.
- **Unit tests:**
  - clap parse tests: each flag parses to the expected `Vec<Ipv4Net>` / `u16`; bad CIDR errors.
  - `max_clients` default == 1; `vpn-hub-prefix` default == 24.
- **e2e:** none.
- **Done:** `bore vpn connect --help` / `bore vpn listen --help` show the new flags; gates green.

#### 0.3 Pure route-filter function + test vectors
- **Model:** Sonnet (small but correctness-critical logic).
- **Files:** `src/vpn.rs` (new pure fn, e.g. `mod routes { pub fn filter_accepted(...) }`).
- **Change:**
  ```rust
  /// Resolve which advertised CIDRs the connector installs.
  /// final = (accept_all ? advertised : accept ∩ advertised) − refuse  (exact-or-subset, D9)
  pub fn filter_accepted(
      advertised: &[Ipv4Net],
      accept_all: bool,
      refuse_all: bool,
      accept: &[Ipv4Net],
      refuse: &[Ipv4Net],
  ) -> Vec<Ipv4Net>
  ```
  Use `Ipv4Net::contains`/`overlaps` (`src/shared.rs:499-559`) for exact-or-subset.
- **Unit tests** (table-driven, cover the scenario):
  - default (no flags) → `[]`.
  - `refuse_all` → `[]` even with accept flags.
  - `accept_all` → all advertised.
  - `accept_all + refuse 10.10.0.0/16` → `[192.168.4.0/24]` (host-A).
  - `accept_all + refuse 192.168.4.0/24` → `[10.10.0.0/16]` (host-C).
  - `accept 192.168.4.0/24` (subset of advertised) → `[192.168.4.0/24]`.
  - `accept 8.8.8.0/24` (not advertised) → `[]` + warn.
  - subset semantics: advertised `10.10.0.0/16`, refuse `10.10.5.0/24` → still `[10.10.0.0/16]`
    (refuse must cover the advertised entry to remove it; document this exactly).
- **Done:** all vectors pass; gates green.

---

### Phase 1 — Connector route accept/refuse (also fixes the existing 1:1 path)

> Independently shippable and user-visible: works on **today's** 1:1 link. Lets a connector
> choose routes even before the hub exists.

#### 1.1 Apply the filter on the connector
- **Model:** Sonnet.
- **Files:** `src/vpn.rs` connector `run_connect_once` (route apply at line 987).
- **Change:** replace `let peer_routes = peer_advertised.to_vec();` with
  `let peer_routes = routes::filter_accepted(&peer_advertised, args.accept_all, args.refuse_all, &args.accept_routes, &args.refuse_routes);`
  Log the resolved accepted set at `info`. Everything downstream (`NetConfig::apply`,
  `src/vpn.rs:989-1000`) is unchanged — it already installs exactly `peer_routes`.
- **Unit tests:** covered by 0.3; add one asserting the connector passes the filtered set to a
  mocked `NetConfig` (the test harness in `tests/vpn_server_test.rs` uses a fake runner — reuse it).
- **e2e** (`scripts/vpn_netns_test.sh`, current 1:1 topology):
  - **T-RF1:** listener advertises `192.168.50.0/24`; connector with **no** route flag →
    `ip route` on the connector has **no** route to `192.168.50.0/24`; ping to the fake LAN
    host **fails**.
  - **T-RF2:** same listener; connector `--accept-all-routes` → route present, ping **succeeds**.
  - **T-RF3:** connector `--accept-all-routes --refuse-routes 192.168.50.0/24` → no route, ping fails.
- **Done:** new netns tests pass; existing tests (which used implicit accept-all) updated to pass
  `--accept-all-routes` where they expect LAN reachability — **note the behavior change in docs**
  (default is now deny). Gates green.

> **Behavior-change callout:** before this phase, a connector silently accepted all advertised
> routes. After it, default is **deny**. Update `docs/vpn/VPN_USER_FULL_GUIDE.md` and the netns
> harness's existing site-to-host tests (Test 2, 8, 16) to pass `--accept-all-routes`.

---

### Phase 2 — Server hub registry + addressing (no client data-plane yet)

> Server learns to keep a listener entry alive, allocate a hub subnet, assign per-connector
> addresses + `peer_id`, and stream `VpnPeerJoin/Leave`. Validated entirely with in-process
> server unit tests (no TUN).

#### 2.1 Hub subnet allocation in the pool
- **Model:** Sonnet.
- **Files:** `src/vpn_server.rs` (`VpnPool` and friends).
- **Change:** add `alloc_hub_subnet(prefix)` → returns a hub block + the `.1` address; add
  `alloc_host_in(block)` / `free_host_in(block, addr)` for per-connector addresses; keep the
  existing `/30` `alloc()` untouched for 1:1. RAII lease guards mirror `VpnLeaseGuard`.
- **Unit tests** (extend `tests/vpn_server_test.rs`):
  - `vpn_pool_alloc_hub_subnet_and_hosts` (distinct hosts, `.1` reserved for hub).
  - `vpn_pool_hub_host_exhaustion` (full `/24` → error).
  - `vpn_pool_free_host_reallocates`.
  - `vpn_pool_legacy_slash30_unchanged` (regression).
- **Done:** gates green.

#### 2.2 Multi-capable registry entry + peer-event channel
- **Model:** Opus design review of the data model, then Sonnet implements.
- **Files:** `src/vpn_server.rs` (`VpnProviderEntry`, `VpnRegistry`, `VpnDeregister`).
- **Change:** when `max_clients>1`, the entry is **not** consumed. Replace the single
  `pair_tx: oneshot` with an `mpsc::Sender<HubPeerEvent>` (peer join/leave + per-peer UDP punch
  forwarding) plus a shared `Arc<Mutex<HubState>>` holding the hub subnet, a `peer_id`
  allocator, and `HashMap<u32, ConnectorSlot>`. Keep the `session` generation guard (D5) and the
  legacy oneshot path for `max_clients<=1`.
- **Unit tests:**
  - `vpn_registry_hub_entry_survives_first_connector`.
  - `vpn_registry_peer_id_monotonic`.
  - `vpn_registry_session_guard_still_holds` (reconnect race regression).
- **Done:** gates green; 1:1 entry lifecycle unchanged.

#### 2.3 `serve_vpn_listener` hub mode
- **Model:** Sonnet.
- **Files:** `src/vpn_server.rs:264-445`.
- **Change:** if `max_clients>1`: allocate hub subnet, send hub `VpnReady{ assigned=.1, prefix }`,
  then loop forwarding `HubPeerEvent` → `VpnPeerJoin`/`VpnPeerLeave` down the listener control
  stream, **plus** per-peer UDP punch forwarding (the `to_provider` channel becomes per-peer,
  keyed by `peer_id`). Heartbeats unchanged. On listener disconnect, free the whole subnet and
  send `VpnPeerLeave` cleanup to nothing (entry dropped).
- **Unit tests:**
  - `vpn_listener_hub_emits_peer_join_on_connect`.
  - `vpn_listener_hub_emits_peer_leave_on_disconnect`.
  - `vpn_listener_hub_forwards_udp_punch_with_peer_id`.
- **Done:** gates green.

#### 2.4 `serve_vpn_connector` hub case + `peer_id` relay injection
- **Model:** Sonnet.
- **Files:** `src/vpn_server.rs:450-798` (`serve_vpn_connector`, `vpn_relay`).
- **Change:** if the looked-up listener is a hub: **do not** remove the entry; allocate a host
  address + `peer_id`; reject if the connector set `advertised` (D4) → `VpnError`; send the
  connector `VpnReady{ assigned=.N, prefix=hub_prefix, peer_overlay=hub.1, peer_advertised=hub.advertised }`;
  push `HubPeerEvent::Join{ peer_id, overlay, nonce, carriers }` to the hub; relay substreams via
  `vpn_relay` **with `peer_id` injected** (`[STREAM_READY, peer_id u32]` then copy). Broker UDP
  per peer (tag `UdpPunch.peer_id`, read connector offer's `peer_id`). On connector exit: push
  `HubPeerEvent::Leave{ peer_id }`, free the host address.
- **Unit tests:**
  - `vpn_server_hub_pairs_multiple_connectors` (3 connectors → 3 distinct overlays + peer_ids,
    none rejected).
  - `vpn_server_hub_rejects_connector_advertise`.
  - `vpn_relay_injects_peer_id_header` (assert the listener-side stream begins with
    `[STREAM_READY, peer_id BE]` then the connector's tag bytes — extend the opacity test at
    `tests/vpn_server_test.rs:677-769`).
  - `vpn_server_hub_connector_leave_frees_address` (4th connector reuses a freed address).
  - `vpn_server_legacy_1to1_still_consumes_entry` (regression: `max_clients<=1` unchanged).
- **e2e:** none yet (no hub data plane).
- **Done:** gates green; the existing `vpn_server_duplicate_id_rejected` semantics preserved for
  1:1 (`tests/vpn_server_test.rs:450-471`).

---

### Phase 3 — Hub client data plane (relay path only)

> The big one. host-D runs a single TUN with the overlay subnet and a per-peer router. Relay
> path only — direct deferred to Phase 4. Split into focused sub-phases so each is testable.

#### 3.1 `NetConfig` subnet + spoke-isolation support
- **Model:** Sonnet.
- **Files:** `src/vpn.rs` `hostcfg::NetConfig` (`2244-2498`).
- **Change:** add a hub mode that (a) configures the TUN with the subnet prefix (already
  generic — `prefix` flows from `VpnReady`), (b) installs the `FORWARD -i bore0 -o bore0 -j DROP`
  isolation rule when hub mode, (c) applies ip_forward + masquerade for advertised subnets
  **once**. All additions reverted by the existing RAII `Drop` (`src/vpn.rs:2431-2497`); extend
  `revert_cmds`. Extend `stale_reclaim` (`src/vpn.rs:267`) to clean the isolation rule after SIGKILL.
- **Unit tests:**
  - `netconfig_hub_emits_isolation_rule` (use the fake `Runner` already in the codebase; assert
    the command list contains the drop rule + masquerade + subnet route, and the revert list
    mirrors it).
  - `netconfig_hub_revert_removes_isolation_rule`.
- **e2e:** exercised in 3.4.
- **Done:** gates green.

#### 3.2 PeerTable + swappable sender + shared router uplink
- **Model:** Opus design review (this is the hot-path refactor), then Sonnet implements.
- **Files:** `src/vpn.rs` new `mod hub` (next to `mod bridge`).
- **Change:**
  - `struct PeerHandle { sender: ArcSwap<Arc<link::LinkSender>>, overlay: Ipv4Addr, peer_id: u32, shutdown: ... }`.
  - `type PeerTable = Arc<RwLock<HashMap<Ipv4Addr, Arc<PeerHandle>>>>` (read-mostly; or
    `dashmap::DashMap` to match the codebase's existing DashMap use).
  - `run_router_uplink(dev, table, counters, mtu, offload)`: adapt `run_uplink_single`/`_offload`
    (`src/vpn.rs:3645+`) to parse dst IPv4 and look up the sender per packet/batch instead of a
    fixed `sender`. Drop + count packets with no matching peer. **Preserve the offload/GSO batch
    path**: group a read batch by destination peer before sending (most batches are single-flow;
    a per-packet fallback is acceptable but keep the common single-dst case batched).
- **Unit tests** (`mod hub` `#[cfg(test)]`):
  - `router_parses_ipv4_dst` (well-known header offset; IPv4 only — drop non-IPv4).
  - `router_routes_to_correct_peer` (two fake peers + channels; assert delivery by dst).
  - `router_drops_unknown_dst` (counter increments, no panic).
  - `peerhandle_sender_swap_is_seen_by_router` (swap relay→direct sender; next send goes to the
    new sender).
- **Done:** gates green.

#### 3.3 Per-peer link manager (relay) + `peer_id` substream demux
- **Model:** Sonnet.
- **Files:** `src/vpn.rs` `mod hub` + listener `run_listen_once` hub branch + `link` accept path.
- **Change:**
  - New `link::accept_relay_multi_tagged(acceptor)` (or a hub accept loop) that reads
    `[STREAM_READY, peer_id u32, tag, idx?]` and returns `(peer_id, dir, idx, stream)`; route each
    into a `peer_id`-keyed pending buffer. Once a peer has its `2×carriers` streams, build the
    link via `make_relay_multi` and register it. Handle substreams arriving before the matching
    `VpnPeerJoin` (buffer by `peer_id`; bounded; time out stale buffers).
  - Hub-mode `run_listen_once`: create ONE subnet TUN; spawn the router uplink(s); ctrl actor
    forwards `VpnPeerJoin`/`VpnPeerLeave` to the peer manager; per peer spawn a relay downlink
    (`run_downlink` → `devs[0]`) and insert the sender into the `PeerTable`. On `VpnPeerLeave` or
    downlink death, remove + abort. Keep `--max-clients 1` on the **legacy** path untouched
    (branch at the top of `run_listen_once`).
- **Unit tests:**
  - `hub_demux_routes_substreams_by_peer_id`.
  - `hub_buffers_substreams_arriving_before_join`.
  - `hub_peer_leave_removes_from_table_and_aborts`.
- **e2e:** in 3.4.
- **Done:** gates green; `--max-clients 1` byte-identical to today (assert via the existing
  `tests/vpn_relay_link_test.rs` still passing unchanged).

#### 3.4 Hub relay e2e (multi-connector)
- **Model:** Sonnet (bash harness extension).
- **Files:** `scripts/vpn_netns_test.sh` (add a hub topology: 1 server ns + 1 hub ns + 3
  connector ns + a fake LAN behind the hub). Build first: `cargo build --release --features vpn`.
- **e2e tests:**
  - **T-HUB1 (relay):** hub `--max-clients 4 --relay-only`; 3 connectors `--relay-only`. Each
    connector pings the hub overlay `.1` (0% loss). Distinct overlay IPs assigned (`.2/.3/.4`).
  - **T-HUB2 (spoke isolation):** connector A pings connector B's overlay → **fails** (dropped at
    hub). Hub→each spoke works.
  - **T-HUB3 (join/leave churn):** start 3, kill one, start a new one → reuses the freed address;
    survivors keep pinging throughout (no hub restart).
  - **T-HUB4 (throughput):** `iperf3` from 2 connectors concurrently through relay ≥ baseline
    floor (reuse the harness's existing iperf helper).
- **Done:** all T-HUB* pass; existing 1:1 tests still pass.

---

### Phase 4 — Per-peer direct (UDP/QUIC) upgrade on the hub

> Each spoke independently upgrades to a direct QUIC path; the hub swaps that peer's sender in
> place and adds a direct downlink, with warm-relay fallback — per peer, reusing the existing
> single-link machinery.

#### 4.1 Per-peer punch brokering (`peer_id`-tagged)
- **Model:** Sonnet.
- **Files:** `src/vpn_server.rs` (hub UDP broker), `src/vpn.rs` (hub ctrl actor routes
  `UdpPunch.peer_id` → the right per-peer direct task; offers carry `peer_id`).
- **Change:** the hub runs N direct-upgrade tasks (one per peer). The ctrl actor demuxes
  `CtrlEvent::Punch{ peer_id, … }` to the matching task; offers from each task carry `peer_id`.
  Server brokers per peer using the per-`peer_id` `to_provider` channel from 2.3. Reuse the
  re-arm/timeout logic (`src/vpn_server.rs:690-793`) per peer.
- **Unit tests:**
  - `hub_ctrl_actor_routes_punch_by_peer_id`.
  - `server_hub_brokers_two_peers_independently` (extend the broker tests at
    `tests/vpn_server_test.rs:771-1252`).
- **Done:** gates green.

#### 4.2 Per-peer direct upgrade + in-place sender swap + fallback
- **Model:** Opus design review (concurrency/lifecycle), then Sonnet implements.
- **Files:** `src/vpn.rs` `mod hub` (per-peer mini state machine).
- **Change:** per peer, run `direct_upgrade_task` (reuse `DirectUpgradeCtx::from_link_args`,
  `src/vpn.rs:329-352`). On upgrade: `make_direct` → swap `PeerHandle.sender` to `Direct`, spawn a
  direct downlink; keep the relay downlink warm. On direct death: swap sender back to relay (DEC-2
  seamless, in place — the router never restarts). Reuse `bridge_next_action` for the decision.
  Per-peer nonce counter is the peer link's own shared `Arc<AtomicU64>` (I-5/DEC-6 — never shared
  across peers; each peer has its own egress key from its own `session_nonce`).
- **Unit tests:**
  - `hub_peer_swap_relay_to_direct_and_back` (mock senders; assert router follows the swap).
  - `hub_peer_direct_death_falls_back_in_place` (relay stays warm; no table removal).
- **e2e:** in 4.3.
- **Done:** gates green.

#### 4.3 Hub direct e2e
- **Model:** Sonnet (harness).
- **Files:** `scripts/vpn_netns_test.sh`.
- **e2e tests:**
  - **T-HUBD1:** hub + 2 connectors, `--stun-server` pinned to the server ns; both upgrade to
    direct independently (`"bridge switched to direct path"` per peer); ping 0% loss; iperf UDP
    ≥ direct floor.
  - **T-HUBD2 (mixed paths):** block one connector's UDP only → that spoke stays on relay while
    the other goes direct; both ping works.
  - **T-HUBD3 (fallback):** bring a direct spoke up, then drop its UDP mid-session → it falls back
    to warm relay in place (no reconnect, no TUN churn), ping continues; the other spoke unaffected.
  - **T-HUBD4 (background retry):** spoke starts UDP-blocked (relay), unblock mid-session → upgrades
    to direct on the next retry round (reuse the 30 s retry grid behavior).
- **Done:** all T-HUBD* pass.

---

### Phase 5 — Site-to-host multi-client + route filtering end-to-end

> The full user scenario: hub advertises LAN subnets; spokes consume per-policy through the hub
> gateway, on both relay and direct.

#### 5.1 Gateway forwarding under the hub router
- **Model:** Sonnet.
- **Files:** `src/vpn.rs` `mod hub` + `NetConfig`.
- **Change:** verify reply traffic from the advertised LAN to a spoke lands on `bore0` (kernel
  routes the hub subnet to `bore0`), the router uplink looks up the spoke by dst, and forwards via
  its sender. Ensure masquerade + ip_forward are hub-global (not per peer) and the spoke-isolation
  rule does not block LAN↔spoke (only spoke↔spoke). Confirm MSS-clamp still applies on the hub
  egress.
- **Unit tests:** `netconfig_hub_gateway_rules_present_and_revert` (masquerade + forward + isolation).
- **e2e:** in 5.3.
- **Done:** gates green.

#### 5.2 Reject connector `--advertise` in multi mode (client-side guard)
- **Model:** Haiku (small guard + error).
- **Files:** `src/vpn.rs` connector; `src/vpn_server.rs` already rejects (2.4) — add a clear
  client-side preflight error too.
- **Unit tests:** `connector_advertise_in_multi_mode_is_rejected`.
- **Done:** gates green.

#### 5.3 Full-scenario e2e (acceptance)
- **Model:** Sonnet (harness) + Opus review of assertions.
- **Files:** `scripts/vpn_netns_test.sh` (final scenario topology: server + hub-D + A/B/C/E +
  LAN `192.168.4.0/24` and a second fake subnet for `10.10.0.0/16`).
- **e2e tests (run under BOTH `--relay-only` and direct):**
  - **T-SCEN-A:** host-A `--accept-all-routes --refuse-routes 10.10.0.0/16` → reaches
    `192.168.4.x`, **not** `10.10.x.x`.
  - **T-SCEN-B:** host-B `--accept-all-routes` → reaches both.
  - **T-SCEN-C:** host-C `--accept-all-routes --refuse-routes 192.168.4.0/24` → reaches
    `10.10.x.x`, **not** `192.168.4.x`.
  - **T-SCEN-E:** host-E (no flags) → reaches hub overlay only; neither LAN.
  - **T-SCEN-ISO:** A↔C blocked; all reach D.
  - Verify the route tables on each connector match `filter_accepted` expectations.
- **Done:** all scenario tests pass on relay and direct; gates green.

---

### Phase 6 — Hardening, limits, admin, docs, bench

#### 6.1 Limits & resource safety
- **Model:** Sonnet.
- **Change:** enforce per-hub `--max-clients`; pool/host exhaustion → `VpnError` to the connector,
  hub unaffected; bound the pre-join substream buffer; cap total hub peers vs `--vpn-max-links`.
- **Unit tests:** `hub_rejects_connector_over_max_clients`; `hub_pool_exhaustion_rejects_connector_only`.
- **e2e:** **T-LIM:** `--max-clients 2`, 3rd connector rejected, first 2 keep working.

#### 6.2 SIGKILL stale reclaim for hub
- **Model:** Sonnet.
- **Change:** extend the `/run` state file + `stale_reclaim` to restore ip_forward and remove the
  isolation rule + subnet route after a hub SIGKILL (mirror BUG-2/BUG-3 handling).
- **e2e:** **T-HUBKILL:** SIGKILL the hub, re-run same id → clean reclaim, no leaked rules.

#### 6.3 Admin page
- **Model:** Sonnet.
- **Files:** `src/admin*.rs`, `src/admin_status.html`.
- **Change:** show a hub with its N connectors (per-peer overlay, path relay/direct, bytes).
  Reuse the existing per-link admin entry pattern (`Role::VpnListener`/`VpnConnector`).
- **Unit tests:** extend `vpn_admin_entries_and_path_report` (`tests/vpn_server_test.rs:1279-1344`)
  for multiple connectors under one hub.

#### 6.4 Benchmarks (perf/latency guardrail)
- **Model:** Sonnet.
- **Change:** extend the relay/direct throughput+latency bench to N spokes; assert per-spoke
  throughput and added latency vs the 1:1 baseline are within tolerance (the router lookup must not
  regress single-flow latency). Document numbers.
- **Done:** bench recorded in `docs/vpn/VPN_TEST_MATRIX.md`.

#### 6.5 Docs
- **Model:** Haiku (mechanical doc updates) with Opus final read.
- **Files:** `docs/vpn/VPN.md`, `VPN_USER_FULL_GUIDE.md`, `VPN_TEST_MATRIX.md`, and CLAUDE.md
  invariants section.
- **Change:** document hub mode, the new flags, default-deny route behavior, addressing, isolation,
  and the new invariants (below).

---

## 7. Invariants to preserve / add

- **I-MC1:** `--max-clients 1` ⇒ identical to current 1:1 (addressing `/30`, headers, byte
  stream, relay substream framing). The hub path is a separate branch; the legacy path is not
  edited beyond adding the branch point.
- **I-MC2:** Connector relay substream bytes unchanged. The server injects `peer_id` framing on the
  server→hub side only; the connector→server side is byte-for-byte as today.
- **I-MC3:** One yamux Stream = one task (existing rule). Per-peer relay still uses two
  unidirectional substreams per carrier.
- **I-MC4:** AEAD nonce counter is one shared `Arc<AtomicU64>` **per peer egress key** — never
  shared across peers, never reused. Each peer derives its own keys from its own `session_nonce`.
- **I-MC5:** Hub router never restarts on a path switch; the swap is inside `PeerHandle.sender`.
- **I-MC6:** `NetConfig` RAII reverts subnet route + ip_forward + masquerade + isolation rule on
  SIGINT/SIGTERM/panic; SIGKILL recovered via `/run` stale reclaim.
- **I-MC7:** Relay stays AEAD-opaque; the server only adds `peer_id` framing, never plaintext.
- **I-MC8:** Default connector route policy is **deny**; routes appear only via explicit flags.

## 8. Risk register

| Risk | Mitigation |
|------|-----------|
| Shared hub yamux session becomes a relay bottleneck (all spokes funnel through the listener's one TCP conn) | Direct path offloads per-spoke; carriers widen relay; note a v2 hub carrier-pool option. Bench in 6.4. |
| Substream arrives before its `VpnPeerJoin` (ordering race) | `peer_id`-keyed pending buffer with bound + timeout (3.3). |
| Router per-packet lookup adds latency | Read-mostly map + batch-by-dst on the offload path; bench vs baseline (6.4). |
| Concurrent TUN writers (N downlinks) corrupting frames | TUN writes are packet-atomic per fd; downlinks write whole packets; verified by T-HUB1/iperf. |
| Behavior change: default route deny breaks existing setups/tests | Explicit callout (Phase 1) + update harness tests + docs. |
| Static addressing + hub mode | Disallowed in v1 (D6); hub requires pool. Clear `VpnError`. |

## 9. Verification summary

- **Gates (every sub-phase):** `cargo fmt --check`, `cargo clippy --features vpn -- -D warnings`,
  `cargo test --features vpn`.
- **Unit tests:** `tests/vpn_server_test.rs` (server registry/broker/pool), `mod hub`/`mod link`
  `#[cfg(test)]` (router, demux, sender swap, filter), `src/shared.rs` (serde).
- **e2e:** rebuild `cargo build --release --features vpn` (as user, not root) then
  `sudo scripts/vpn_netns_test.sh` extended with the hub topology. The harness refuses to run
  against a stale release binary — always rebuild first.
- **Acceptance:** the §1 5-host scenario (T-SCEN-*) passes on both relay and direct.

## 10. Model-assignment summary

| Phase | Sub-phases | Primary model | Opus review gate |
|-------|-----------|---------------|------------------|
| 0 | 0.1, 0.2 Haiku · 0.3 Sonnet | Haiku/Sonnet | — |
| 1 | 1.1 | Sonnet | — |
| 2 | 2.1, 2.3, 2.4 Sonnet · 2.2 Sonnet | Sonnet | 2.2 (data model) |
| 3 | 3.1, 3.3, 3.4 Sonnet · 3.2 Sonnet | Sonnet | 3.2 (hot-path router) |
| 4 | 4.1, 4.3 Sonnet · 4.2 Sonnet | Sonnet | 4.2 (concurrency/lifecycle) |
| 5 | 5.1, 5.3 Sonnet · 5.2 Haiku | Sonnet/Haiku | 5.3 (acceptance assertions) |
| 6 | 6.1–6.4 Sonnet · 6.5 Haiku | Sonnet/Haiku | 6.5 (final docs read) |

> Rule of thumb (CLAUDE.md): start Sonnet, drop to Haiku for mechanical/boilerplate sub-phases
> (serde fields, clap flags, doc edits, small guards), escalate to Opus only for the architecture
> review gates marked above. Print the model used per sub-task during implementation.
