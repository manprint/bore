# bore vpn — Complete User Guide

`bore vpn` establishes a point-to-point Layer 3 virtual network interface between two Linux machines. IP packets travel over bore's brokered, NAT-traversing transport: a direct QUIC path when hole-punching succeeds, an AEAD-encrypted relay as fallback.

**Platform:** Linux only. Requires root or `CAP_NET_ADMIN`. Build: `cargo build --release --features vpn`.

---

## Table of Contents

1. [Quick Start](#1-quick-start)
2. [How It Works](#2-how-it-works)
3. [Addressing Modes](#3-addressing-modes)
4. [Topology A — Host to Host](#4-topology-a--host-to-host)
5. [Topology B — Site to Host (Gateway + Roaming Client)](#5-topology-b--site-to-host)
6. [Topology C — Site to Site (Gateway to Gateway)](#6-topology-c--site-to-site)
7. [Complete Flag Reference](#7-complete-flag-reference)
8. [Server Configuration](#8-server-configuration)
9. [Static Addressing](#9-static-addressing)
10. [NAT Traversal Options](#10-nat-traversal-options)
11. [No-Manage Mode](#11-no-manage-mode)
12. [Auto-Reconnect](#12-auto-reconnect)
13. [Security Model](#13-security-model)
14. [Network Configuration Details](#14-network-configuration-details)
15. [GSO/GRO Offload](#15-gsogro-offload)
16. [Cleanup Guarantee](#16-cleanup-guarantee)
17. [Environment Variables](#17-environment-variables)
18. [Diagnosing Problems](#18-diagnosing-problems)
19. [Known Limitations](#19-known-limitations)

---

## 1. Quick Start

```bash
# Build with VPN support
cargo build --release --features vpn

# Start the server (once, on a public host)
bore server --secret MYSECRET --vpn --vpn-pool 10.99.0.0/16

# Machine A — listen (waits for a connector with the same --id)
sudo bore vpn listen \
  --to bore.example.com \
  --secret MYSECRET \
  --id demo

# Machine B — connect (pairs with the listener)
sudo bore vpn connect \
  --to bore.example.com \
  --secret MYSECRET \
  --id demo
```

After pairing, both machines have a `bore0` interface with overlay addresses (e.g. `10.99.0.1` and `10.99.0.2`). Ping them:

```bash
# From Machine B
ping 10.99.0.1
```

Press `Ctrl-C` on either side to tear down the link cleanly.

---

## 2. How It Works

```
Machine A                     Server                    Machine B
  bore vpn listen ─────────── register id=demo ────────────────────
                                                 bore vpn connect ──
                               ← pair both sides →
  VpnReady (addr, nonce) ←───────────────────── VpnReady (addr, nonce)
  TUN up, routes installed                       TUN up, routes installed
  hole-punch attempt ─────────────────────────── hole-punch attempt
         │                                              │
         └──── direct QUIC path (preferred) ───────────┘
         OR
         └──── relay substream (fallback) ──── server splices ────┘
```

**Listener** registers with the server under `--id`. Blocks until a connector pairs.

**Connector** contacts the server with the same `--id`. The server allocates a `/30` overlay block, sends `VpnReady` to both sides simultaneously with assigned addresses and a session nonce. Both sides build their TUN device, apply network configuration, then begin the bridge loop (IP packets in ↔ out).

**Path selection:** both sides attempt QUIC hole-punching in parallel with the relay substream opening. If QUIC succeeds first, the bridge uses it; otherwise relay. The path is logged at `info!` level:

```
info vpn_link_paired link_id=demo path=direct overlay=10.99.0.1/30
```

---

## 3. Addressing Modes

### Pool Mode (default)

Server allocates a `/30` block from `--vpn-pool`. The listener gets `.1`, the connector `.2`. Example with pool `10.99.0.0/16`:

- First link: listener `10.99.0.1/30`, connector `10.99.0.2/30`
- Second link: listener `10.99.0.5/30`, connector `10.99.0.6/30`

On link teardown the block is freed and returned to the pool.

**Requirements:** server must have `--vpn-pool` set. Both sides must use pool mode (no `--vpn-addr`).

### Static Mode

Both sides specify their own overlay addresses explicitly. Values must be mirror-consistent:

| Side | Example value |
|------|--------------|
| Listener `--vpn-addr` | `172.31.0.1/30` |
| Listener `--vpn-peer-addr` | `172.31.0.2` |
| Connector `--vpn-addr` | `172.31.0.2/30` |
| Connector `--vpn-peer-addr` | `172.31.0.1` |

Server validates:
1. Both sides use the same mode (pool vs. static).
2. `listener.addr == connector.peer` and `connector.addr == listener.peer`.
3. Both addresses fall in the same network; same prefix length; distinct addresses.
4. No overlap with any live pool lease or other static link.

On validation failure, the connector receives `VpnError` and exits non-zero; the listener stays registered.

---

## 4. Topology A — Host to Host

Neither peer advertises subnets. Each side routes only its own traffic over the TUN.

```bash
# Machine A
sudo bore vpn listen \
  --to bore.example.com \
  --secret S3cret \
  --id mylink

# Machine B
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id mylink
```

**What bore installs on each side:**

| Resource | Installed? |
|----------|-----------|
| TUN `bore0` with overlay address | Yes |
| `ip route` to peer overlay | Yes (direct via TUN) |
| IP forwarding | No |
| nft masquerade | No |
| MSS-clamp rule | No |

**Test:**

```bash
# From B
ping 10.99.0.1   # pings A's overlay address
iperf3 -s        # run server on A first
iperf3 -c 10.99.0.1 -u -b 0  # UDP throughput test from B
```

**Custom TUN name:**

```bash
sudo bore vpn listen --id mylink --secret S --tun-name vpn0
```

The TUN device appears as `vpn0` instead of `bore0`.

---

## 5. Topology B — Site to Host

The listener (gateway) advertises one or more LAN subnets. The connector (roaming client) can reach those subnets through the tunnel.

```bash
# Gateway machine (LAN 192.168.50.0/24)
sudo bore vpn listen \
  --to bore.example.com \
  --secret S3cret \
  --id office \
  --advertise 192.168.50.0/24

# Roaming client
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id office
```

**What bore installs on the gateway (listener):**

| Resource | Value |
|----------|-------|
| TUN `bore0` with overlay address | `10.99.0.1/30` |
| `ip route` to peer overlay | `10.99.0.2/30 dev bore0` |
| IP forwarding | `/proc/sys/net/ipv4/ip_forward = 1` (previous value saved) |
| nft table | `inet bore_vpn_office` |
| nft masquerade rule | outbound from TUN toward LAN |
| nft MSS-clamp rule | `tcp option maxseg size set rt mtu` on SYN packets |

**What bore installs on the roaming client (connector):**

| Resource | Value |
|----------|-------|
| TUN `bore0` with overlay address | `10.99.0.2/30` |
| `ip route` to gateway overlay | `10.99.0.1/30 dev bore0` |
| `ip route` to advertised subnets | `192.168.50.0/24 via 10.99.0.1 dev bore0` |
| IP forwarding | No |
| nft rules | No |

**Test from roaming client:**

```bash
ping 192.168.50.1     # gateway's LAN IP
ping 192.168.50.100   # any host on the LAN (must be up)
curl http://192.168.50.100  # HTTP service on LAN host
```

LAN hosts see the roaming client's source IP as the gateway's LAN interface address (masquerade). They do not need to know about the VPN.

**Multiple subnets:**

```bash
sudo bore vpn listen \
  --id office \
  --secret S \
  --advertise 192.168.50.0/24,192.168.51.0/24
```

Both subnets are routed and masqueraded.

---

## 6. Topology C — Site to Site

Both sides advertise subnets; each is both a gateway and a client for the other's LAN.

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

**What bore installs on Site A:**

- TUN with overlay address
- Route to B's LAN (`192.168.60.0/24 via bore0`)
- IP forwarding enabled
- nft masquerade + MSS-clamp for traffic toward B's LAN

**What bore installs on Site B:**

- TUN with overlay address
- Route to A's LAN (`192.168.50.0/24 via bore0`)
- IP forwarding enabled
- nft masquerade + MSS-clamp for traffic toward A's LAN

**LAN-to-LAN routing (not managed by bore):**

For a host on LAN A to reach a host on LAN B, LAN A's router must know to forward `192.168.60.0/24` via the bore gateway host. Example (on LAN A's router):

```bash
ip route add 192.168.60.0/24 via 192.168.50.10  # where 192.168.50.10 is Site A's gateway
```

Without this, only the gateway hosts themselves can reach the remote LAN — not other LAN hosts.

**Test from the gateway itself (no router config needed):**

```bash
# From Site A gateway — reaching Site B's LAN
ping 192.168.60.1
```

---

## 7. Complete Flag Reference

### `bore vpn listen`

| Flag | Short | Env var | Type | Default | Description |
|------|-------|---------|------|---------|-------------|
| `--to` | `-t` | `BORE_SERVER` | `ADDR` | `bore.0912345.xyz` | Server address (`host`, `host:port`, or `https://host`) |
| `--secret` | `-s` | `BORE_SECRET` | `SECRET` | **required** | Shared secret for auth + relay encryption |
| `--id` | | `BORE_VPN_ID` | `ID` | **required** | Link identifier; connector must use the same value |
| `--advertise` | | `BORE_VPN_ADVERTISE` | `CIDR[,CIDR...]` | — | Subnets to expose; comma-separated; enables gateway mode when non-empty |
| `--vpn-addr` | | `BORE_VPN_ADDR` | `IP/PREFIX` | — | Static overlay address with prefix (e.g. `172.31.0.1/30`); omit for pool mode |
| `--vpn-peer-addr` | | `BORE_VPN_PEER_ADDR` | `IP` | — | Static peer overlay address (requires `--vpn-addr`) |
| `--tun-name` | | — | `NAME` | `bore0` | TUN interface name |
| `--mtu` | | — | `N` | `1350` | TUN interface MTU; reduce if large packets drop persistently |
| `--no-route-manage` | | — | flag | — | Print all route/NAT commands verbatim instead of running them; TUN is still created |
| `--auto-reconnect` | | `BORE_AUTO_RECONNECT` | flag | — | Reconnect on link failure with exponential backoff (full teardown+rebuild per attempt; fatal config errors exit) |
| `--relay-only` | | `BORE_VPN_RELAY_ONLY` | flag | — | Never attempt the direct UDP path; stay on the server relay |
| `--carriers` | | `BORE_VPN_CARRIERS` | N | 1 | Parallel relay carrier substream pairs (1-16); effective = min(both sides, server --max-carriers) |
| `--tun-queues` | | `BORE_VPN_TUN_QUEUES` | N | 1 | Linux TUN queues (IFF_MULTI_QUEUE, 1-8); one uplink pump per queue |
| `--insecure` | | `BORE_INSECURE` | flag | — | Skip TLS certificate verification (useful with self-signed certs) |
| `--stun-server` | | `BORE_STUN_SERVER` | `HOST:PORT` | — | Additional STUN server for UDP candidate discovery |
| `--upnp` | | `BORE_UPNP` | flag | — | Attempt UPnP-IGD to add a router-mapped UDP candidate |
| `--try-port-prediction` | | `BORE_TRY_PORT_PREDICTION` | flag | — | Predict symmetric-NAT port offsets as extra candidates |
| `--nat-udp-preferred-port` | | `BORE_NAT_UDP_PREFERRED_PORT` | `PORT` | `0` | Bind UDP hole-punch socket to this port; `0` lets the OS choose |
| `--nat-udp-release-timeout` | | `BORE_NAT_UDP_RELEASE_TIMEOUT` | `SECS` | `0` | Seconds to wait before retrying a preferred port that is in use |
| `--notes` | | `BORE_NOTES` | `TEXT` | — | Operator note, logged on link-up; purely informational |

### `bore vpn connect`

Identical flag set to `bore vpn listen`. The only semantic difference: the connector role determines which address (`listener.1` vs `connector.2`) the server assigns in pool mode, and the connector triggers the pairing on the server.

### `bore server` (VPN-related flags)

| Flag | Env var | Type | Default | Description |
|------|---------|------|---------|-------------|
| `--vpn` | `BORE_VPN` | flag | — | Enable VPN brokering (requires `--features vpn` at build time) |
| `--vpn-pool` | `BORE_VPN_POOL` | `CIDR` | — | Overlay address pool for `/30` allocation (required for pool-mode clients) |
| `--vpn-max-links` | `BORE_VPN_MAX_LINKS` | `N` | `32` | Maximum concurrent VPN links |

---

## 8. Server Configuration

The server must be built with `--features vpn` (same as the client).

**Minimal:**

```bash
bore server --secret S3cret --vpn
# Pool-mode clients will fail — no --vpn-pool provided.
# Static-mode clients work.
```

**With pool:**

```bash
bore server \
  --secret S3cret \
  --vpn \
  --vpn-pool 10.99.0.0/16 \
  --vpn-max-links 64 \
  --bind-addr 0.0.0.0
```

**With TLS (recommended for production):**

```bash
bore server \
  --secret S3cret \
  --vpn \
  --vpn-pool 10.99.0.0/16 \
  --tls-cert /etc/bore/cert.pem \
  --tls-key  /etc/bore/key.pem
```

Clients use `--to https://bore.example.com` to trigger TLS.

**Error cases:**

| Condition | Connector receives |
|-----------|-------------------|
| `--vpn` not set on server | `VpnError("vpn not supported/enabled")` |
| `--vpn-pool` absent, pool-mode client | `VpnError("server has no vpn pool")` |
| Unknown `--id` | `VpnError("unknown vpn id: <id>")` |
| Pool exhausted | `VpnError("vpn pool exhausted")` |
| Addressing mode mismatch | `VpnError("addressing mode mismatch")` |
| Overlapping subnets | `VpnError("overlapping subnets: ...")` |
| Duplicate `--id` | listener: `VpnError("duplicate vpn id: <id>")` |

---

## 9. Static Addressing

Use static addressing when you need stable, predictable overlay IPs regardless of server pool state — e.g., for firewall rules or systemd unit files.

```bash
# Listener (Machine A)
sudo bore vpn listen \
  --to bore.example.com \
  --secret S3cret \
  --id fixed \
  --vpn-addr 172.31.0.1/30 \
  --vpn-peer-addr 172.31.0.2

# Connector (Machine B)
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id fixed \
  --vpn-addr 172.31.0.2/30 \
  --vpn-peer-addr 172.31.0.1
```

**Rules enforced by the server:**

1. `listener.addr == connector.peer_addr` (mirror check)
2. `connector.addr == listener.peer_addr` (mirror check)
3. Both addresses in the same network; same prefix length
4. Addresses are distinct
5. No collision with other live links (pool or static)

**Error on mismatch:** the connector receives `VpnError("inconsistent static addressing")` and exits non-zero. The listener stays registered and can accept a corrected connector.

---

## 10. NAT Traversal Options

These flags tune how bore discovers and advertises UDP candidates for direct QUIC hole-punching. They mirror the flags available on `bore local`/`bore proxy`.

### `--stun-server <HOST:PORT>`

Add an extra STUN server to the discovery chain. Bore always queries Cloudflare and Google STUN first; `--stun-server` prepends your custom server.

```bash
sudo bore vpn listen \
  --id mylink --secret S \
  --stun-server stun.yourco.internal:3478
```

Use when the default STUN servers are blocked or you want a STUN server colocated with your bore server.

### `--upnp`

Attempt UPnP-IGD to add a port mapping on the router. If the router supports it, this allows hole-punching to succeed even when behind some symmetric NATs.

```bash
sudo bore vpn connect --id mylink --secret S --upnp
```

Non-fatal: if UPnP fails, bore logs a warning and continues with other candidates.

### `--try-port-prediction`

For symmetric NATs with sequential port allocations, predict the likely next port and include it as an extra candidate. Increases hole-punch success rate against some symmetric NATs at the cost of more UDP probes.

```bash
sudo bore vpn listen --id mylink --secret S --try-port-prediction
```

Run `bore test-udp` first to check if your NAT uses sequential ports (the test reports this explicitly).

### `--nat-udp-preferred-port <PORT>`

Bind the UDP hole-punch socket to a fixed port instead of a random OS-assigned port. Useful when:

- Your firewall only allows outbound UDP on specific ports.
- You want reproducible candidates for static firewall rules.

```bash
sudo bore vpn listen \
  --id mylink --secret S \
  --nat-udp-preferred-port 51820
```

If the port is already in use, bore falls back to a random port unless `--nat-udp-release-timeout` is set.

### `--nat-udp-release-timeout <SECS>`

When `--nat-udp-preferred-port` is in use, wait up to this many seconds for it to become available before giving up and using a random port. Default `0` (do not wait).

```bash
sudo bore vpn listen \
  --id mylink --secret S \
  --nat-udp-preferred-port 51820 \
  --nat-udp-release-timeout 10
```

### Combining NAT flags

For maximum hole-punch success rate on difficult networks:

```bash
sudo bore vpn listen \
  --id mylink --secret S \
  --upnp \
  --try-port-prediction \
  --nat-udp-preferred-port 51820 \
  --stun-server stun.yourco.internal:3478
```

---

## 11. No-Manage Mode

`--no-route-manage` creates and configures the TUN device but **skips all routing and NAT mutations**. Instead, every command that would have been run is printed to stderr.

```bash
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id mylink \
  --no-route-manage 2>&1 | tee /tmp/vpn_setup.sh
```

Output example (gateway mode):

```bash
ip addr add 10.99.0.2/30 dev bore0
ip link set bore0 up
ip route add 10.99.0.1/32 dev bore0
ip route add 192.168.50.0/24 via 10.99.0.1 dev bore0
```

Review, modify, and apply:

```bash
bash /tmp/vpn_setup.sh
```

**Cleanup:** in no-manage mode, only the TUN interface is removed on exit. Routes and rules applied manually are **not** cleaned up automatically.

**Use cases:**

- Environments with restricted privilege models (create TUN separately from routing).
- Auditing the exact rules before system-wide deployment.
- Docker or network-namespace environments where you want full control of the network config.

---

## 12. Auto-Reconnect

`--auto-reconnect` reconnects automatically when the link drops, using exponential backoff:

| Attempt | Wait (seconds) |
|---------|---------------|
| 1 | 1 |
| 2 | 2 |
| 3 | 4 |
| 4 | 8 |
| 5 | 16 |
| 6+ | 32 (fixed) |

```bash
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id mylink \
  --auto-reconnect
```

**Behavior on reconnect:**

- Each attempt is a full teardown + rebuild: the TUN is destroyed and
  re-created, routes/NAT are reverted and re-applied (`ip route replace` makes
  the re-apply idempotent).
- With pool addressing the overlay /30 **may change** across reconnects (the
  server re-allocates); use static addressing if you need stable addresses.
- The direct-path upgrade is re-attempted on every reconnect (fresh nonce).
- An attempt that stayed up >60 s resets the backoff to 1 second.
- **Fatal configuration errors exit instead of looping**: overlap, addressing
  mode mismatch, static mismatch, pool exhausted, no server pool, max-links,
  missing root or `ip` binary. Exception: `vpn id already in use` is retried
  (the previous server-side session may take a few seconds to die).

**Log output during reconnect loop:**

```
warn vpn link lost; reconnecting error="server closed the vpn control stream" delay=1s
info vpn listener starting link_id=mylink
info vpn link paired link_id=mylink path=relay overlay=10.99.0.2/30
```

**Ctrl-C during reconnect:** cancels the reconnect loop cleanly; all installed state is removed.

---

## 13. Security Model

### Authentication

Both sides authenticate with the server via `--secret`. The server verifies a HMAC before pairing peers. Without the correct secret, no pairing occurs and no overlay addresses are assigned.

### Relay path encryption

When the direct QUIC path is unavailable, IP packets travel over a yamux substream with per-packet AEAD encryption:

- **Algorithm:** ChaCha20-Poly1305
- **Key derivation:** `HKDF-SHA256(secret, session_nonce, label)` with distinct labels for each direction (`bore-vpn l2c v1` / `bore-vpn c2l v1`)
- **Session nonce:** server-issued, unique per link pairing
- **Frame format:** `[u32 BE total_len][u64 BE counter][ciphertext ‖ 16-byte AEAD tag]`

The server splices bytes between the two relay substreams without ever seeing plaintext. Even if the server is compromised, relay traffic is protected by the shared secret.

### Direct path encryption

QUIC-TLS 1.3 end-to-end. The server is not involved in the data path after pairing. The direct path token is derived from `--secret` and the server nonce; the server verifies the token before brokering the punch but does not retain it.

### What the server sees

| Information | Visible to server? |
|-------------|-------------------|
| Link `--id` | Yes |
| `--secret` value | No (HMAC-verified only) |
| Overlay addresses assigned | Yes (needed for routing) |
| Advertised subnets | Yes |
| IP packet headers / payload (relay) | No (AEAD-encrypted) |
| IP packet headers / payload (direct) | No (QUIC-TLS) |

---

## 14. Network Configuration Details

### nft table

When gateway mode is active, bore creates an nft table named `bore_vpn_<id>` (where `<id>` is your `--id` value). Inside it:

**Chain `bore_fw`** (forward, inet):

```
chain bore_fw {
    type filter hook forward priority 0; policy accept;
    tcp flags syn tcp option maxseg size set rt mtu  # MSS clamp
    oifname "<lan_iface>" iifname "bore0" masquerade  # masquerade toward LAN
}
```

Verify:

```bash
nft list table inet bore_vpn_mylink
```

**Cleanup:** `nft delete table inet bore_vpn_mylink` is called on graceful exit. If bore crashes (SIGKILL), the table persists until the next `bore vpn` run with the same `--id`, which detects and removes it (stale reclaim).

### IP forwarding

When gateway mode is active, bore reads `/proc/sys/net/ipv4/ip_forward`, saves the current value, and writes `1`. On exit, the saved value is restored.

If your system already has IP forwarding enabled (e.g., you run a router), bore's restore operation will re-write `1` — the value is not decreased, it is restored to exactly what it was.

### TUN MTU

Default MTU is **1350 bytes**. This is a conservative value that:

1. Fits inside a QUIC datagram (peer `max_datagram_size()` starts at 1200 during MTU discovery, rises to ~1450 as discovery settles).
2. Leaves room for QUIC + AEAD framing overhead on the relay path.

**If you see persistent large-packet loss** (more than 5 seconds after link-up):

```bash
# Lower MTU
sudo bore vpn connect --id mylink --secret S --mtu 1280

# Verify which MTU succeeds
ping -M do -s 1280 10.99.0.1  # check 1280-byte packets
ping -M do -s 1300 10.99.0.1  # likely fails if path MTU is 1350
```

### Routes installed

| Condition | Route installed | On which side |
|-----------|----------------|--------------|
| Always | `<peer_overlay>/32 dev <tun>` | Both |
| Peer advertises subnets | `<subnet> via <peer_overlay> dev <tun>` | Local side |

---

## 15. GSO/GRO Offload

Bore auto-detects kernel support for `IFF_VNET_HDR` (GSO/GRO) at TUN creation time.

**If supported:**

- **Uplink (TUN → network):** `recv_multiple()` reads a batch; GSO super-buffers are segmented to ≤`max_datagram_size()`.
- **Downlink (network → TUN):** received datagrams are coalesced via GRO table and written back in one `send_multiple()` call.

**If not supported:** falls back silently to single-packet mode. No user action required.

**Log output:**

```
info tun_created name=bore0 mtu=1350 offload=true   # GSO/GRO active
info tun_created name=bore0 mtu=1350 offload=false  # single-packet mode
```

**Measured baseline (iperf3 over loopback):**

| Mode | Throughput |
|------|-----------|
| Single-packet | ~13,500 Mbps |
| GSO/GRO offload | ~14,000 Mbps |

The offload has minimal visible impact on real-network paths where the bottleneck is the WAN link, not TUN syscalls.

---

## 16. Cleanup Guarantee

On graceful exit (`Ctrl-C`, `SIGTERM`, or link error), bore undoes all system mutations in reverse order:

1. TUN interface removed (kernel removes address, routes via that interface)
2. Explicit routes that survive interface removal are deleted (`ip route del`)
3. nft table deleted (`nft delete table inet bore_vpn_<id>`)
4. IP forwarding restored to pre-link value
5. Relay/QUIC connections closed

After cleanup, `ip route show`, `nft list tables`, and `/proc/sys/net/ipv4/ip_forward` are identical to their state before `bore vpn` started.

### SIGKILL / crash recovery

SIGKILL bypasses cleanup. On the next `bore vpn` run with the same `--id`, bore detects stale state by checking:

- `bore0` interface exists → delete and recreate
- `bore_vpn_<id>` nft table exists → delete before installing new rules
- `/proc/sys/net/ipv4/ip_forward` is `1` but was `0` before → note: bore cannot distinguish its own previous change from an independent one; IP forwarding is left as-is in ambiguous cases

---

## 17. Environment Variables

All environment variables accept the same values as the corresponding flags.

| Variable | Flag | Notes |
|----------|------|-------|
| `BORE_SERVER` | `--to` | |
| `BORE_SECRET` | `--secret` | Hidden in `ps` output and logs |
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
| `RUST_LOG` | — | e.g. `RUST_LOG=bore_cli=debug,bore_cli::vpn=trace` |

**Example: store credentials in environment, avoid secrets on command line:**

```bash
export BORE_SERVER=bore.example.com
export BORE_SECRET=mysecret
export BORE_VPN_ID=mylink

sudo -E bore vpn listen --advertise 192.168.50.0/24
```

(`sudo -E` preserves the environment.)

---

## 18. Diagnosing Problems

### Link paired but no ping

Check which path is active:

```bash
# Look for path= in bore logs
journalctl -u bore-vpn.service | grep vpn_link_paired
# or watch stderr directly
sudo bore vpn connect --id mylink --secret S 2>&1 | grep "path="
```

If `path=relay`, run `bore test-udp` on both hosts to understand the NAT situation:

```bash
bore test-udp --to bore.example.com
```

Possible outcomes:

| NAT class (listener) | NAT class (connector) | Direct path? |
|----------------------|-----------------------|-------------|
| open | any | Yes |
| cone | any | Yes |
| symmetric (sequential) | cone or open | Maybe (`--try-port-prediction`) |
| symmetric | symmetric | No (relay only) |

### Ping works but TCP stalls

Usually an MTU problem:

1. Try `--mtu 1280`.
2. Check the MSS-clamp rule is installed: `nft list table inet bore_vpn_<id>`.
3. Trace the path MTU: `tracepath 10.99.0.1`.

### Can ping gateway overlay but not hosts behind it

From the connector side:

1. Verify the route is installed: `ip route get 192.168.50.1` should show `dev bore0`.
2. Confirm the gateway has IP forwarding enabled: `cat /proc/sys/net/ipv4/ip_forward` (should be `1`).
3. Confirm the nft masquerade rule exists: `nft list table inet bore_vpn_<id>`.
4. Try `ping -I bore0 192.168.50.1` to force the TUN as the outbound interface.

### Works from gateway but not from other LAN hosts

Site-to-site topology: the LAN's default gateway/router needs a route. From Site A's router:

```bash
ip route add 192.168.60.0/24 via <site-a-gateway-lan-ip>
```

### Stale TUN interface after crash

```bash
# List TUN interfaces
ip link show type tun

# Remove manually if needed (bore will also do this on next run)
ip link delete bore0
nft delete table inet bore_vpn_mylink
```

### Enable debug logging

```bash
RUST_LOG=bore_cli=debug sudo -E bore vpn connect --id mylink --secret S
```

For VPN-specific trace:

```bash
RUST_LOG=bore_cli::vpn=trace,bore_cli::vpn_server=trace sudo -E bore vpn connect --id mylink --secret S
```

---

## 19. Known Limitations

### IPv4 only

v1 supports IPv4. IPv6 and dual-stack are not implemented. The overlay addresses, `--vpn-pool`, `--vpn-addr`, and `--advertise` all accept IPv4 only.

### Overlapping subnets rejected

If the listener's advertised subnets overlap with the connector's (or with the overlay `/30` block), the server rejects the pair with `VpnError("overlapping subnets: ...")`. Ensure subnet assignments are non-overlapping, or use `--vpn-addr` static mode with non-conflicting addresses.

### TCP over relay is reliable but has higher latency

The relay path wraps IP packets in a yamux substream over a TCP connection. For latency-sensitive protocols, use `bore test-udp` to investigate whether the direct path can be made to work (see §10). If both sides are behind symmetric NAT, direct is impossible; reduce relay latency by choosing a geographically close server.

### No multi-peer mesh in v1

Each `--id` accepts exactly one listener and one connector. For three or more peers, run multiple independent links with different `--id` values and manage routing manually.

### No privilege drop after setup

v1 holds root/`CAP_NET_ADMIN` for the entire duration of the link. Future versions may drop to a lower-privilege context after setup.
