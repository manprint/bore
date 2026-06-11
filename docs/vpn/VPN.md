# bore VPN — Linux Point-to-Point L3 Tunnel

## Concept

`bore vpn` establishes a **point-to-point Layer 3 virtual network interface** between two Linux machines, carrying real IP packets over bore's brokered, NAT-traversing transport. Two peers rendezvous through the server, establish a direct QUIC path when possible, and fall back to a server-relayed encryption layer for maximum availability. The tunnel works equally well for exposing a single host's services to a peer, or for routing entire subnets between two gateways.

**The load-bearing mental model:** a VPN link is structurally a secret tunnel that carries IP packets instead of a TCP byte-stream.

### Platform support

| Platform | Status | Notes |
|---|---|---|
| Linux | ✅ Full (host + gateway mode) | Reference platform; all features |
| Android (Termux, rooted) | 🔬 Build-checked in CI | Kernel is Linux but `target_os = "android"`; runtime support pending the portability refactor (plan §5.1/§5.4). Requires `tsu` + Termux `iproute2` |
| macOS (utun) | 📐 Groundwork | `hostcfg_cmd::macos` argv builders + CI cross-check in place; runtime TUN/host-config wiring pending (§5.2) |
| Windows (wintun) | 📐 Groundwork | `hostcfg_cmd::windows` argv builders + CI cross-check in place; runtime wiring + `wintun.dll` handling pending (§5.3) |

Non-Linux targets are planned as **host-only mode** (no `--advertise`, no
NAT/forwarding/MSS-clamp): gateway mode needs a per-OS NAT engine and stays
Linux-only (DEC-8).

### Requirements

- **Operating system:** Linux only (kernel TUN/TAP support required)
- **Privilege:** root or `CAP_NET_ADMIN` (to manage network interfaces and routes)
- **Build:** `cargo build --release --features vpn`
- **Server:** must be started with `--vpn` flag and have a pool configured (`--vpn-pool <CIDR>`)
- **Authentication:** `--secret` is mandatory on both client sides (required for E2E encryption on the relay fallback path)

#### Running with CAP_NET_ADMIN but without root (gateway mode)

Gateway mode toggles `/proc/sys/net/ipv4/ip_forward`. `CAP_NET_ADMIN` alone is
**not** enough to write that procfs file: the write fails with `EACCES` even
though interface/route management works. In that case bore falls back to a
non-interactive `sudo -n tee /proc/sys/net/ipv4/ip_forward` for both the enable
(at link setup) and the restore (at teardown). For the fallback to work, install
a sudoers rule for the user running bore:

```
# /etc/sudoers.d/bore-vpn
youruser ALL=(root) NOPASSWD: /usr/bin/tee /proc/sys/net/ipv4/ip_forward
```

If both the direct write and the `sudo -n` fallback fail at **setup**, the link
aborts with an actionable error. If they fail at **teardown**, bore logs a
`warn!` with the exact manual command to restore the saved value
(`echo <saved> | sudo tee /proc/sys/net/ipv4/ip_forward`). Host-only links
(no `--advertise`) never touch `ip_forward`.

Route management uses `ip route replace` (idempotent): a stale route left over
from a crashed run or an in-flight reconnect never aborts link setup with
`EEXIST`.

### Security Model

**Direct path (preferred):** unreliable QUIC datagrams, encrypted end-to-end via QUIC-TLS 1.3. The server is not involved in the data path; it only orchestrates the handshake. Both peers authenticate the QUIC connection with a token derived from `(secret, session_nonce)` — the same mechanism as secret tunnels.

**Relay path (fallback):** framed AEAD-encrypted IP packets over **two yamux substreams, one per direction**. The connector opens both and tags each with a direction byte (`0x01` connector→listener, `0x02` listener→connector) right after the readiness marker; each substream is then written by exactly one side and read only by the other. Each packet is sealed with ChaCha20-Poly1305 under a key derived from the shared secret and a server-issued nonce. The server splices ciphertext bytes opaque — **it never sees plaintext IP packets**, preserving E2E encryption even when a direct path is unavailable.

> Why two substreams: a `yamux::Stream` holds a single parked-task waker on its internal command channel, so its read and write halves must never be polled from two different tasks (e.g. via `tokio::io::split`) — the reader and writer overwrite each other's waker and the link freezes permanently after ~256 KB under load, with no error reported. One unidirectional substream per task removes the contention entirely. Both peers must run a build that speaks the dual-substream protocol; an old single-substream peer fails link setup with an explicit "peer built from an older version?" error instead of wedging. The relay queue applies backpressure (the TUN read loop waits) rather than dropping packets: the relay is an ordered, reliable stream, so dropping there only multiplies inner-TCP retransmissions.

**No network traversal is possible without the shared `--secret`.** Server cannot derive relay encryption keys; keys are bound to the secret supplied by the client.

---

## Three Topologies

### Topology A: Host ↔ Host

Neither peer advertises a subnet; each side forwards only its own traffic.

**Setup:**

```bash
# Machine A (listener) — root required
sudo bore vpn listen \
  --to bore.example.com \
  --secret S3cret \
  --id mylink

# Machine B (connector) — root required
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id mylink
```

**Expected behavior:**

- Both sides obtain a `/30` overlay address pair from the server's pool (e.g., A gets `10.99.0.1`, B gets `10.99.0.2`).
- A TUN interface (`bore0` by default) is created on each side, UP, with the assigned address.
- Logs show `path="direct"` if hole-punching succeeded, or `path="relay"` if it fell back to the server.
- `ping` between the overlay addresses works; small packets (56 bytes) usually succeed immediately.
- Large packets (e.g., `ping -s 1300`) may drop briefly during QUIC MTU discovery, then succeed. This is normal (§6.1 transient).
- No IP forwarding is enabled; no NAT rules are installed (each side routes only its own traffic).

**Throughput:** `iperf3 -s` on A, `iperf3 -c 10.99.0.1` on B shows sustained throughput. Direct path achieves roughly the same bandwidth as `bore test-udp` between the same hosts. Relay path uses the server's TCP relay, potentially more latency.

---

### Topology B: Site ↔ Host (Gateway + Roaming Client)

The listener advertises one or more subnets behind it (its LAN); the connector reaches hosts in those subnets.

**Setup:**

```bash
# Machine A: gateway of LAN 192.168.50.0/24
sudo bore vpn listen \
  --to bore.example.com \
  --secret S3cret \
  --id site \
  --advertise 192.168.50.0/24

# Machine B: roaming client, connects to the site
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id site
```

**Expected behavior:**

- A becomes the gateway; B becomes the client.
- A's system detects that it advertises routes; it enables IP forwarding (`/proc/sys/net/ipv4/ip_forward = 1`), saves the previous value for restoration.
- A installs an `nft` table `bore_vpn_site` with masquerade (NAT) and MSS-clamp rules. One rule marks packets inbound on the TUN as going out toward the LAN with source NAT (masquerade). Another rule clamps TCP MSS to the path MTU to avoid PMTU blackholes. Logs show each rule at `info!` on apply.
- B receives a route: `192.168.50.0/24 dev bore0` (the peer's advertised subnet via the TUN).
- From B, you can now `ping 192.168.50.10` (a real host on A's LAN) and see replies from that host's real IP.
- From B, `curl http://192.168.50.10` reaches the LAN host's service. The LAN host sees the source IP as A's LAN address (masquerade), not B's.
- TCP connections from B into A's LAN never get stuck with "PMTU blackhole" errors because the MSS is clamped at setup time.

**On exit (Ctrl-C or error):**

- The `nft` table is deleted (atomic, single operation).
- IP forwarding is restored to its previous value.
- B's route is deleted.
- Both TUN interfaces are removed.
- Logs show each undo at `info!`.

**Cleanup guarantee:** after a graceful exit, `ip route show`, `nft list tables`, and `cat /proc/sys/net/ipv4/ip_forward` are identical to before the link started. (A `SIGKILL` cannot clean up; the next `bore vpn` run with the same `--id` reclaims stale state.)

---

### Topology C: Site ↔ Site (Gateway ↔ Gateway)

Both peers advertise subnets; each side is both a gateway and a client.

**Setup:**

```bash
# Site A gateway (LAN 192.168.50.0/24)
sudo bore vpn listen \
  --to bore.example.com \
  --secret S3cret \
  --id s2s \
  --advertise 192.168.50.0/24

# Site B gateway (LAN 192.168.60.0/24)
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id s2s \
  --advertise 192.168.60.0/24
```

**Expected behavior:**

- A installs: routes to B's LAN (192.168.60.0/24), IP forwarding, NAT/MSS rules.
- B installs: routes to A's LAN (192.168.50.0/24), IP forwarding, NAT/MSS rules.
- A host on LAN A can reach a host on LAN B **if** LAN A's router knows to forward `192.168.60.0/24` via gateway A, or if you run the test from the gateway itself.
- Similarly for B.

**LAN router configuration:** Bore manages the gateway hosts; it does not manage the LAN's internal routing. For a full site-to-site mesh, ensure each LAN's router or default gateway is aware of the route via the bore gateway. Example:

```bash
# On LAN A's router: add a route to B's LAN via A's gateway
ip route add 192.168.60.0/24 via 192.168.50.10  # A's gateway IP

# On LAN B's router: add a route to A's LAN via B's gateway
ip route add 192.168.50.0/24 via 192.168.60.10  # B's gateway IP
```

---

## Addressing

### Pool Mode (Default)

If neither `--vpn-addr` nor `--vpn-peer-addr` is specified, the server allocates addresses from its `--vpn-pool` (e.g., `10.99.0.0/16`). Each /30 subnet is allocated once per link; on teardown, it is freed and becomes available for reuse.

- **Listener** gets the `.1` address of the allocated /30 (e.g., `10.99.0.1/30`).
- **Connector** gets the `.2` address (e.g., `10.99.0.2/30`).

Both sides must use pool mode (or both static); mixed mode is rejected with `VpnError("addressing mode mismatch")`.

### Static Mode

Provide explicit overlay addresses on both sides:

```bash
# Listener
sudo bore vpn listen \
  --to bore.example.com \
  --secret S3cret \
  --id st \
  --vpn-addr 172.31.0.1/30 \
  --vpn-peer-addr 172.31.0.2

# Connector
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id st \
  --vpn-addr 172.31.0.2/30 \
  --vpn-peer-addr 172.31.0.1
```

**Validation (server-side):**

1. Both sides must use the same mode (both pool or both static).
2. For static pairs, the mirror must be consistent: `listener.addr == connector.peer` and vice versa; same prefix; both addresses in the same network; addresses distinct.
3. Static addresses cannot collide with any live pool lease or another live static link.
4. Pool mode requires the server to have `--vpn-pool`; if absent, both sides get `VpnError("server has no vpn pool")`.

On validation failure, the connector receives a `VpnError` and exits non-zero; the listener remains registered, waiting for a valid connector.

---

## Network Configuration

### MTU and QUIC Datagrams

The TUN interface default MTU is **1350 bytes**, overridable with `--mtu`. This is a conservative value chosen to:

1. **Fit inside a QUIC datagram.** QUIC datagram payloads have a maximum size advertised by the peer (`max_datagram_size()`). By clamping the TUN MTU, we guarantee that a segmented packet always fits in one datagram on the direct path, avoiding retransmission meltdown.
2. **Survive path MTU discovery transients.** At the start of a direct QUIC connection, the peer's `max_datagram_size()` may report 1200 bytes (the QUIC conservative initial guess) before MTU discovery raises it. Small packets pass immediately; full-size packets (`ping -s 1300`) drop transiently, and TCP retransmits. After the first round-trips (usually within 1–2 seconds), MTU discovery settles and throughput normalizes.

**If you see persistent large-packet loss >10 seconds after link-up:**

The path MTU is likely below 1350. Try `--mtu 1280` or lower, or enable `--no-route-manage` and manually inspect the path with `tracepath` / `mtu-test`.

### On the Relay Fallback Path

When the direct QUIC path is unavailable, IP packets are framed with a 4-byte length + 8-byte counter + 16-byte AEAD tag, so the minimum relay frame is roughly 28 bytes overhead per packet. The server multiplexes this over a TCP connection with standard TCP framing, so the effective path MTU is slightly lower than the TUN MTU. If you find the relay path is losing large packets, reduce `--mtu` by 50 bytes and retry.

### Gateway MSS Clamping

When you advertise subnets (gateway mode), the setup installs an `nft` rule:

```
tcp flags syn tcp option maxseg size set rt mtu
```

This clamps TCP's **Maximum Segment Size** to the route MTU on outbound packets, preventing TCP implementations that ignore path MTU discovery from sending oversized segments that silently drop.

---

## Limitations (v1)

### Overlapping Subnets

If both sides (or either side and the overlay /30) advertise overlapping subnets, the server rejects the pair with `VpnError("overlapping subnets: ...")`. The listener remains registered; the connector exits non-zero.

This is a **v1 limitation**. Future versions may support overlapping subnets via per-subnet 1:1 NAT (DNAT/SNAT remapping).

### IPv4 Only

v1 supports IPv4 only (`Ipv4Addr`, `/30` overlay, advertised `Ipv4Net` subnets). IPv6 and dual-stack support is deferred to v2 (§V2).

---

## The `--no-route-manage` Flag

By default, `bore vpn` auto-manages all network configuration: interface creation, address assignment, routing, IP forwarding, and NAT rules. This requires root or `CAP_NET_ADMIN`.

With `--no-route-manage`, the TUN device itself is still created and configured (non-negotiable), but all **routing and NAT mutations are skipped**. Instead, every command is **printed verbatim** so you can review and run them manually:

```bash
sudo bore vpn connect --to srv --id site --no-route-manage 2>&1 | tee /tmp/vpn_cmds.txt

# Review, then apply manually:
cat /tmp/vpn_cmds.txt | bash
```

On exit, only the TUN interface is removed; the manually-applied routes and rules are left in place. This is useful for:

- Environments where you prefer to control NAT rules (Docker, network namespaces).
- Testing the exact rules before applying them system-wide.
- Constrained privilege models where you want to separate interface setup from routing.

---

## Automatic Reconnection

With `--auto-reconnect`, the client retries on link failure with exponential
backoff (1, 2, 4, 8, 16, 32 seconds, then every 32 seconds). Each attempt is a
**full teardown + rebuild** (DEC-5): the TUN is destroyed and re-created, and
`NetConfig` is reverted and re-applied — `ip route replace` keeps the re-apply
idempotent even if a previous teardown was incomplete. With pool addressing the
overlay /30 may change across reconnects (the server re-allocates); static
addressing keeps the same addresses. The direct-path upgrade is re-attempted on
every reconnect with a fresh nonce.

An attempt that stayed up for more than 60 seconds resets the backoff to 1 s,
so a long-lived link that drops reconnects promptly.

**Fatal errors stop the loop** — retrying a configuration mistake would fail
identically forever: missing root/`CAP_NET_ADMIN`, missing `ip` binary,
`VpnError` for overlapping subnets, addressing mode mismatch, static mirror
mismatch, exhausted pool, missing server pool, or `vpn-max-links`. The process
exits non-zero with the error.

**Deliberate exceptions (both reconnect-race transients, not config errors):**

- `vpn id already in use` IS retried (with a `warn!`). During a reconnect the
  server-side handler of the previous session can take a few seconds to notice
  the dead connection and release the id; one or two backoff rounds resolve it.
- `vpn listener '<id>' not found` IS retried. After a server restart the
  connector and listener race to re-register; if the connector wins it gets this
  error before the listener is back. Retrying lets the listener catch up. (Found
  by netns Test 10; without `--auto-reconnect` the connector still exits on the
  first error, so a genuinely-missing listener is not retried forever.)

---

## Path Selection & Fallback

**On link-up the link ALWAYS starts on the relay** (instant availability), and a
background task attempts the direct upgrade in parallel:

1. Both clients bind a UDP socket, gather hole-punch candidates (STUN reflexive
   + local address; optionally UPnP and port prediction), and send a
   `UdpCandidateOffer` to the server on the control stream.
2. The server's broker waits until it holds **both** offers, then sends
   `UdpPunch` to **both** sides, each carrying the other peer's candidates. If
   the listener produces no candidates within 10 s of the connector's offer,
   the connector receives `UdpUnavailable` and stays on relay.
3. The listener starts a QUIC endpoint (`DirectListener`) and the connector
   dials it (`connect_direct`); both punch UDP toward the peer's candidates.
   The connection is authenticated with the token derived from
   `(secret, session_nonce)`.
4. On success the bridge performs a **controlled restart** (DEC-1): both pumps
   are stopped, the relay link halves are dropped (closing the relay
   substreams), and the pumps respawn on the direct QUIC link. Logs show
   `info!(path="direct", "vpn path upgraded to direct QUIC")` and
   `"bridge switched to direct path"`. A few in-flight packets may be lost
   during the switch — IP is best-effort; TCP inside the tunnel retransmits.

**If the direct attempt fails** (no punch within 15 s, `UdpUnavailable`, QUIC
handshake timeout), the client logs
`info!(path="relay", "direct path unavailable; staying on relay")` and the
relay bridge keeps running untouched — there is no retry within the same
session (a reconnect re-attempts the upgrade with a fresh nonce).

**If the direct path dies at runtime** (DEC-2), the bridge ends with an error
and the process exits — or reconnects when `--auto-reconnect` is set, landing
back on relay first and re-attempting the upgrade.

**`--relay-only`** (both subcommands) disables the upgrade attempt entirely:
no UDP socket, no STUN, no offer. Useful for deterministic tests and for
environments where outbound UDP is undesirable.

**Relay is always available** (assuming the server is up) because it is the fallback transport; there is no scenario where the relay "succeeds or fails" — it is the baseline.

---

## Admin Page

With `bore server --admin-token <T>`, the status page at `/admin/status` shows a
dedicated **VPN links** section: role (`listener`/`connector`), link id
(`vpn:<id>`), client address, assigned overlay (`addr/30`), active path
(`relay`/`direct`), relay traffic counters, live relay substream count, and
uptime.

- The **path** column is fed by `VpnPathReport` messages: clients report
  `relay` right after pairing and `direct` after a successful upgrade. The
  server advertises support via the `admin_v2` flag in `VpnReady`; clients
  never send the report to an older server (whose JSON decoder would reject the
  unknown variant).
- The **relay TX/RX** counters measure AEAD **ciphertext** spliced by the
  server (it never sees plaintext, I-3). On the direct path the server carries
  no traffic, so the page shows `n/a (p2p)` — correct and honest.

---

## Diagnosing Issues

### Link pairs but no ping

Check which path is active in the logs:

```bash
# From the logs:
2026-06-10T10:30:42.123Z info vpn_link_paired link_id=mylink path=relay overlay=10.99.0.1/30
```

If `path="relay"`, run `bore test-udp` between the two hosts to diagnose NAT:

```bash
# Machine A
bore test-udp --to bore.example.com

# Machine B (same command)
bore test-udp --to bore.example.com
```

This prints NAT class (cone, symmetric, etc.), port preservation, CGNAT detection, and UPnP status. If both are "open" or "cone", direct should work; if one is "symmetric" and the other is not, only one direction will punch. If both are "symmetric", direct fails (relay only).

### `path=relay` persists (no direct upgrade)

The order of checks:

1. `--relay-only` set on either side? Then this is by design.
2. Grep both client logs for `direct path unavailable; staying on relay` —
   the attached error says why: `no usable UDP candidates` (STUN unreachable,
   UDP egress blocked), `no punch from server` (the **other** peer never
   offered — check its log), `server reported the direct path unavailable`
   (`UdpUnavailable`: the listener produced no candidates within the broker's
   10 s window), or a QUIC timeout (punch packets dropped between the peers).
3. Diagnose NAT with `bore test-udp` as above.

The link stays fully functional on relay in all these cases.

### Ping ok, TCP slow or stalls

Likely an MTU issue:

1. Try `--mtu 1280`.
2. On a gateway, verify the MSS-clamp rule exists: `nft list table inet bore_vpn_<id>`.
3. Check if the path MTU is actually lower than your `--mtu`: run `tracepath` between the peers from outside the tunnel and look for the "no route to host" point, which reveals the bottleneck.

### Works from gateway, not from LAN hosts

**For site↔host (Topology B):** the LAN behind the gateway is behind NAT (masquerade). This is correct. If a LAN host can't reach the other site's tunnel gateway, the LAN's router is missing the route. Example:

```bash
# On LAN A's router:
ip route add 192.168.60.0/24 via 192.168.50.10  # A's gateway
```

**For site↔site (Topology C):** same issue, but both LANs need routes. Each LAN must know to forward packets destined for the peer LAN via the local bore gateway.

### Interface disappears after exit

Normal. `Ctrl-C` (SIGINT), a link error, or panic all trigger cleanup: routes deleted, IP forwarding restored, nft table dropped, TUN interface removed. Stale state from a crash (e.g., `SIGKILL`) is reclaimed on the next `bore vpn` run with the same `--id`.

---

## Performance — Carriers, Multi-Queue, Dynamic PMTU (Phase 2 plan, §4)

**`--carriers <N>` (relay):** opens N relay substream pairs instead of one,
breaking the single-TCP-stream RTT×window throughput ceiling on high-latency
WANs. Frames are distributed round-robin **per datagram** (out-of-order
delivery is fine: IP is best-effort; TCP inside the tunnel reorders). The
effective count is `min(listener, connector, server --max-carriers)`; old
peers default to 1. The AEAD nonce counter is a single shared atomic across
all carriers — never two seals with the same `(key, counter)`. If any carrier
substream dies, the whole link dies cleanly (auto-reconnect picks it up); no
silent half-degraded state.

**`--tun-queues <N>` (Linux):** creates the TUN with `IFF_MULTI_QUEUE` and
runs one uplink pump per queue (the kernel hashes flows across queues), for
multi-Gbit links where a single pump is CPU-bound. The downlink remains a
single pump writing to the first queue (TUN writes are not the typical
bottleneck; revisit if benchmarks say otherwise). Default 1 = identical to the
single-queue path.

**Dynamic PMTU (direct path):** after the switch to direct, a monitor samples
`max_datagram_size()` every 5 s; once the QUIC MTU discovery settles (3 equal
samples, ≥16 bytes away from the current MTU, within [576, 9000]) it runs
`ip link set <tun> mtu <new>` and logs `tun MTU adjusted to QUIC path MTU`.
No revert needed — the TUN is destroyed at teardown, and the nft MSS clamp
uses `rt mtu`, adapting on its own.

**Benchmarks:** `sudo scripts/vpn_bench.sh` produces the comparison table
(relay 1c / relay 4c / direct / direct 4q × TCP / UDP / latency). Record the
numbers here after each tuning change. **Status: still PENDING** — this is the
last open execution item (the §4.4 tuning pass is gated on it; criterion: apply
a change only on a reproducible ≥5% gain, and verify relay-4c ≥ relay-1c and
direct > relay).

> Baseline (pre-Phase-4, docker 3-node): relay ≈ 200 MB/s bulk, ping 0% loss
> under load. The functional netns suite (Test 1–14) passed on 2026-06-11
> (`PASS=42 FAIL=0`) and reports sanity throughput (e.g. direct gateway iperf3
> TCP ≈ 4.8 Gbps, 4-carrier relay ≈ 1.4 Gbps, multi-queue ≈ 4.7 Gbps), but the
> structured comparison numbers are recorded on the first `vpn_bench.sh` run.

---

## Performance — GSO/GRO Offload (Phase 6.2, Implemented)

TUN I/O uses **batch read/write with GSO/GRO offload** when the kernel supports `IFF_VNET_HDR`. The implementation auto-detects support at startup and logs the result at `info!` level.

**How it works:**

1. `IFF_VNET_HDR` is enabled on the TUN device at creation time (`tun-rs` `offload(true)`).
2. On the uplink (TUN → network), `recv_multiple()` reads a batch of packets; if `tcp_gso()` or `udp_gso()` is active, the kernel coalesces multiple packets into super-buffers with a `virtio_net_hdr`. Each super-buffer is segmented to ≤`max_datagram_size()` before dispatch.
3. On the downlink (network → TUN), received datagrams are coalesced via `GROTable` and written back to the TUN in one `send_multiple()` call with zero-prefix `virtio_net_hdr`.

**Fallback:** if the kernel does not support `IFF_VNET_HDR`, the implementation transparently falls back to single-packet mode. No configuration change required.

**Measured baseline (iperf3 over loopback):**

| Mode | Throughput |
|------|-----------|
| Single-packet (Phase 6.1) | ~13,500 Mbps |
| GSO/GRO offload (Phase 6.2) | ~14,000 Mbps |

Large packets may drop transiently during the first 1–2 seconds of a direct QUIC connection (QUIC MTU discovery); after that, throughput stabilizes.

---

## Building and Running

### Build

```bash
# With VPN feature
cargo build --release --features vpn

# Verification
./target/release/bore vpn --help  # subcommand exists
```

### Server

```bash
bore server \
  --secret S3cret \
  --vpn \
  --vpn-pool 10.99.0.0/16 \
  --vpn-max-links 32 \
  --bind-addr 0.0.0.0
```

- `--vpn`: enable VPN brokering (server must be built with `--features vpn`; if not, clients get `VpnError("vpn not supported/enabled")`).
- `--vpn-pool <CIDR>`: allocate /30 blocks from this pool (required for pool-mode clients).
- `--vpn-max-links <N>`: limit concurrent VPN links (default unlimited; reuse pattern from `--max-conns`).

### Client (Listen)

```bash
sudo bore vpn listen \
  --to bore.example.com \
  --secret S3cret \
  --id mylink \
  --advertise 192.168.50.0/24  # (optional; omit for host-only)
```

### Client (Connect)

```bash
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id mylink
```

---

## Environment Variables

| Env var | Flag | Notes |
|---------|------|-------|
| `BORE_SERVER` | `--to` | |
| `BORE_SECRET` | `--secret` | Hidden in `ps` and logs |
| `BORE_VPN_ID` | `--id` | |
| `BORE_VPN_ADVERTISE` | `--advertise` | Comma-separated CIDR list |
| `BORE_VPN_ADDR` | `--vpn-addr` | |
| `BORE_VPN_PEER_ADDR` | `--vpn-peer-addr` | |
| `BORE_INSECURE` | `--insecure` | |
| `BORE_AUTO_RECONNECT` | `--auto-reconnect` | |
| `BORE_STUN_SERVER` | `--stun-server` | |
| `BORE_UPNP` | `--upnp` | |
| `BORE_TRY_PORT_PREDICTION` | `--try-port-prediction` | |
| `BORE_NAT_UDP_PREFERRED_PORT` | `--nat-udp-preferred-port` | |
| `BORE_NAT_UDP_RELEASE_TIMEOUT` | `--nat-udp-release-timeout` | |
| `BORE_NOTES` | `--notes` | |
| `RUST_LOG` | — | e.g. `RUST_LOG=bore_cli=debug` |

---

## Tested Scenarios

> Verified by the netns harness (`sudo scripts/vpn_netns_test.sh`, Test 1–14) —
> **all PASS on 2026-06-11** (`PASS=42 FAIL=0`), plus the automated unit/integration
> suite. The first netns run also exposed and fixed two bugs (direct-switch panic,
> reconnect-race fatal — see VPN_TEST_MATRIX.md note ‡).

- Host ↔ host (pool and static addressing)
- Site ↔ host (one gateway, one client)
- Site ↔ site (both gateways)
- Direct path hole-punch success and failure
- Relay fallback from direct path drop
- `--no-route-manage` (prints commands without applying)
- `--auto-reconnect` with server drop and recovery
- `Ctrl-C` exit cleanup (routes, IP forward, nft table, interface)
- Duplicate link id rejection
- Overlapping subnet rejection
- Pool exhaustion detection
- Address collision detection
- Gateway MSS-clamp rule validation
- Sustained throughput over overlay (iperf3 sanity check)
- `SIGKILL` full stale reclaim: TUN **+ nft table + routes** survive `kill -9`
  and are reclaimed on the next start with no `EEXIST` (netns Test 14)
- Multi-carrier relay (`--carriers 4`) and TUN multi-queue (`--tun-queues 4`)

See [`docs/vpn/VPN_USER_FULL_GUIDE.md`](VPN_USER_FULL_GUIDE.md) for the complete flag reference and use-case guide, and [`docs/vpn/VPN_TEST_MATRIX.md`](VPN_TEST_MATRIX.md) for the full test matrix and traceability to Phase 8 acceptance criteria.
