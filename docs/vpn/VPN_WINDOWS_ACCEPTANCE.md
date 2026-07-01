# Windows — Manual Two-Host Acceptance Checklist (T-WIN-MANUAL)

> Plan: `docs/plans/plan_WindowsSupport/phase_05.md`/`phase_06.md`. Per the
> 2026-07-01 decision recorded in `resume.md`, cross-OS two-machine tests are
> **manual acceptance only** — no CI rig (self-hosted runner or public-tunnel
> infra) exists for them today. GitHub-hosted `windows-latest` proved out
> every SINGLE-host operation instead (`windows-vpn-e2e` CI job,
> `examples/windows_vpn_spike.rs`) — what's left here needs a REAL second
> machine (Linux or macOS) that a real Windows binary talks to over an actual
> network. Record results in the tables at the bottom.

Each step has an **exact command** and an **exact expected observation**.
Windows is one side unless noted; the other side is a Linux or macOS peer
running the same `bore` build. `<srv>` = the bore server address. Build
Windows with `cargo build --release --features vpn` and grab the release zip
(bundles `wintun.dll`, see `VPN_WINDOWS.md`) or build locally.

---

## Part A — VPN (T-WIN-VPN-*, T-WIN-HUB*, T-WIN-GW*, T-WIN-ADMIN1)

### A1 — Relay bring-up, Windows connector → Linux/macOS listener (T-WIN-VPN-RELAY1)

```powershell
# Windows (connector) — run as Administrator
bore vpn connect --to <srv> --secret S3cret --id w1 --relay-only
```
```sh
# Linux/macOS (listener)
sudo bore vpn listen --to <srv> --secret S3cret --id w1 --relay-only
```

Expect:
- Windows log: `Windows WinTun adapter created (single queue, no offload)` with a
  `boreN` name; `Get-NetAdapter -Name boreN` shows it up.
- Both sides get a `/30` overlay address from the server pool; `ping` from
  each side to the other's overlay address succeeds.

### A2 — Relay bring-up, Linux/macOS connector → Windows listener (T-WIN-VPN-RELAY2)

Swap roles from A1 (Windows runs `bore vpn listen`, Linux/macOS runs `connect`).
Same expectations.

### A3 — Windows ↔ Windows via server relay (T-WIN-VPN-RELAY3)

Both peers on Windows machines, same command shapes as A1 with `--relay-only`
on both sides. Expect identical behavior — this validates the WinTun/host-
config path has no asymmetry between the two roles.

### A4 — Direct upgrade, relay → direct QUIC (T-WIN-VPN-DIRECT1)

Repeat A1 **without** `--relay-only` on either side.

Expect:
- Within the direct-retry grid (~30 s), logs on both sides show the switch to
  Direct (`direct path established`).
- `ping` continues across the switch with no sustained loss (seamless
  fallback, shared data plane).

### A5 — Direct death falls back to warm relay (T-WIN-VPN-DIRECT2)

With A4's direct link up, block UDP on the Windows side (e.g. temporarily
`New-NetFirewallRule -DisplayName tmp-block-udp -Direction Outbound -Protocol
UDP -Action Block`) and confirm traffic keeps flowing (falls back to relay,
TUN preserved, no reconnect). Remove the block rule afterward.

### A6 — Direct retry succeeds after UDP re-opens (T-WIN-VPN-DIRECT3)

Continuing from A5, remove the block rule and confirm the link re-upgrades to
Direct within the next ~30 s retry tick, without restarting either process.

### A7 — Relay carriers on Windows (T-WIN-VPN-CARR1)

```powershell
bore vpn connect --to <srv> --secret S3cret --id w1 --relay-only --carriers 4
```

Expect: 4 relay substream pairs open (visible in `bore server`'s admin
panel — see A11); traffic still flows correctly end to end.

### A8 — Direct carriers: full count establishes or stays relay (T-WIN-VPN-CARR2)

Repeat A7 without `--relay-only`. Expect either all 4 direct QUIC
connections establish (never a partial/mismatched count) or the link stays
on relay and keeps working.

### A9 — Direct carriers preserve single-flow order (T-WIN-VPN-CARR3)

With A8's direct carriers up, run a large file transfer (e.g. `bore transfer`
over the tunnel, or a big `scp`-equivalent through the overlay) and confirm
no reordering-induced throughput collapse (flow-pinning, not per-datagram
round-robin, on the direct path).

### A10 — Windows hub with two Linux/macOS spokes (T-WIN-HUB1)

```powershell
# Windows gateway (hub)
bore vpn listen --to <srv> --secret S3cret --id office --max-clients 8
```
```sh
# Linux/macOS spoke 1 and spoke 2 (separate machines)
sudo bore vpn connect --to <srv> --secret S3cret --id office --accept-all-routes
```

Expect: both spokes get distinct overlay addresses; each can reach the
Windows hub; **host-only hub isolation is a known gap** (D2, §2.6/§2.8) — do
NOT expect spoke↔spoke traffic to be blocked. Confirm the log warning
("Windows VPN hub mode does not yet enforce spoke isolation") appears.

### A11 — Linux/macOS hub with a Windows spoke (T-WIN-HUB2)

Swap roles from A10 (Linux/macOS runs the hub with `--max-clients`, Windows
is one of the spokes). Confirm the Windows spoke behaves identically to a
Linux/macOS spoke from the hub's perspective (admin panel row, routing).

### A12 — Windows gateway hub advertises LAN (T-WIN-HUB3)

```powershell
bore vpn listen --to <srv> --secret S3cret --id site --advertise 192.168.50.0/24 --max-clients 8
```

Expect: `Set-ItemProperty ... IPEnableRouter` → `1`; spokes with
`--accept-all-routes` can reach `192.168.50.0/24` through the Windows
gateway; `--forward-accept` firewall rules present if passed (see Part A,
T-WIN-FWD1/2 already CI-proven single-host — this confirms real LAN traffic
transits them, not just that the rules exist).

### A13 — Per-peer direct/relay mixed mode (T-WIN-HUBD1)

With A10/A12's hub up and 2+ spokes, force one spoke to `--relay-only` and
leave the other on default (attempts direct). Confirm the hub serves both
simultaneously without one path affecting the other.

### A14 — Windows gateway LAN route accepted and reachable (T-WIN-GW1)

Already partially exercised by A12; confirm from the peer side that
`ip route`/`route print` shows the advertised route only when
`--accept-all-routes`/`--accept-routes <CIDR>` was passed.

### A15 — Refused route installs nothing (T-WIN-GW2)

Connect without any accept flag (default deny, I-MC8) and confirm no route
to the advertised LAN appears on the peer.

### A16 — `--no-route-manage` installs no routes (T-WIN-GW3)

```powershell
bore vpn connect --to <srv> --secret S3cret --id site --no-route-manage
```

Expect: bore prints the routes/nft-equivalent commands it *would* have run,
but `Get-NetRoute` shows nothing added.

### A17 — Windows VPN admin status and live TX/RX (T-WIN-ADMIN1)

With any link from A1–A16 up, open `bore server`'s `/admin/status` VPN panel
and confirm: the Windows peer's row shows correct role, carriers, direct/
relay flag, and TX/RX counters incrementing live (not stuck at 0.00B).

### A18 — MTU through a Windows gateway (T-WIN-MTU1)

With A12's gateway up, transfer a large file (or `bore transfer`) through
the tunnel end to end at the default MTU 1350. Confirm no fragmentation
errors / stalls — this is the one MTU row that genuinely needs real
gateway traffic (T-WIN-MTU2's `--pin-mtu` observe-only behavior is already
proven structurally — see `resume.md` — no manual step needed for it).

### A19 — Non-admin fails before side effects (T-WIN-HOST0)

On the Windows machine, open a **non-elevated** PowerShell/CMD (right-click →
"Windows PowerShell", not "Run as administrator") and run:

```powershell
bore vpn connect --to <srv> --secret S3cret --id w1
```

Expect: fails immediately with "bore vpn requires an elevated process on
Windows" — no adapter created, no `Get-NetAdapter` row appears.

### A20 — Apply failure rolls back prior ops (T-WIN-HOST4)

Contrive a partial failure (e.g. point `--advertise` at a subnet that makes
the LAN-interface probe fail, or revoke a firewall permission mid-run) and
confirm any routes/rules already added before the failure are rolled back —
no leaked state after the process exits.

---

## Part B — Non-VPN cross-OS parity (T-WIN-LOCAL/SECRET/VHOST/SERVER/TRANSFER/UDPTEST)

These do **not** need admin/elevation — `windows-vpn-build`'s hosted CI
already proves the Windows binary's non-VPN logic in isolation; what's left
is confirming it actually talks to a **different OS's** binary over a real
network (loopback-in-one-process CI can't do that).

### B1 — Windows public TCP relay (T-WIN-LOCAL1)

```sh
# Linux/macOS server
bore server
```
```powershell
# Windows client
bore local 8080 --to <srv>
```

Expect: public port assigned by the server; traffic to it reaches the local
Windows service on port 8080.

### B2 — Windows public `--udp` direct (T-WIN-LOCAL2)

```powershell
bore local 8080 --to <srv> --udp
```

Expect: log shows the QUIC direct path established (server↔client, no
STUN/hole-punch needed for public tunnels); falls back to TCP relay if UDP
is blocked (see B3).

### B3 — Public UDP blocked → TCP fallback (T-WIN-LOCAL3)

Block UDP egress on Windows (temporary firewall rule, as in A5) and repeat
B2. Expect a clean fallback to the TCP relay — no dropped connections.

### B4 — Windows provider, Linux/macOS consumer, secret relay (T-WIN-SECRET1)

```powershell
# Windows provider
bore local 8080 --to <srv> --secret S3cret --tcp-secret-id svc
```
```sh
# Linux/macOS consumer
bore proxy --to <srv> --secret S3cret --tcp-secret-id svc --local-proxy-port :5555
```

Expect: consumer's local port 5555 reaches the Windows provider's port 8080.

### B5 — Linux/macOS provider, Windows consumer, secret relay (T-WIN-SECRET2)

Swap roles from B4. Same expectation.

### B6 — Secret `--udp --carriers 4` direct/fallback (T-WIN-SECRET3)

Add `--udp --carriers 4` to both sides of B4/B5. Expect direct QUIC path or
clean relay fallback, 4 carriers visible in the admin panel.

### B7 — Admin shows one logical secret row, no carrier rows (T-WIN-SECRET4)

With B6 running, check `/admin/status` Secret Tunnels section: exactly ONE
row per logical tunnel regardless of `--carriers`, no spurious "N/A" rows
(BUG-S1 regression — already unit-tested, this confirms it holds across a
real Windows↔Linux pair too).

### B8 — Provider carrier failover (T-WIN-SECRET5)

With B6 running, kill one relay carrier connection on the Windows provider
side (or induce a network blip) and confirm the consumer's traffic fails
over to a surviving carrier without dropping the forwarded connection.

### B9 — Windows vhost TCP relay (T-WIN-VHOST1)

```sh
bore server --vhost-config /etc/bore/vhost.yml
```
```powershell
bore vhost 127.0.0.1:8080 --subdomain winapp --id w1 --to <srv>
```

Expect: `http://winapp.<base_domain>` reaches the Windows-hosted service.

### B10 — Windows vhost `--udp` direct (T-WIN-VHOST2)

Add `--udp` to B9's vhost command (needs `bore server --udp`). Expect direct
QUIC path, same as B2.

### B11 — Vhost UDP blocked → fallback (T-WIN-VHOST3)

Same as B3, applied to B10.

### B12 — Vhost admin flags visible (T-WIN-VHOST4)

With B9/B10 running, confirm `/admin/status` Vhost section shows the Windows
provider's flags (carriers, UDP, notes) correctly.

### B13 — Windows server relays public/secret/vhost (T-WIN-SERVER1/2/3)

Run `bore server` **on Windows** and repeat B1 (public), B4 (secret), and B9
(vhost) with the Linux/macOS side as the non-server peer in each case.
Expect identical behavior to a Linux-hosted server.

### B14 — Windows server `--udp` accepts direct registrations (T-WIN-SERVER4)

Run `bore server --udp` on Windows and repeat B2/B10 against it.

### B15 — Windows server relays VPN between Linux/macOS peers (T-WIN-SERVER5)

Run `bore server --vpn --vpn-pool <CIDR>` on Windows; run A1's relay
bring-up with both VPN peers on Linux/macOS. Confirms the Windows server
role itself, independent of any Windows VPN peer.

### B16 — Windows sender → Linux/macOS listener transfer (T-WIN-TRANSFER1)

```sh
bore transfer listener --secret S3cret --transfer-id t1 --dest-path /tmp/inbox
```
```powershell
bore transfer sender --secret S3cret --transfer-id t1 --sources C:\data\file.bin --parallel 4
```

Expect: file arrives intact (BLAKE3-verified); kill and resume mid-transfer
to confirm resume works across the OS boundary.

### B17 — Linux/macOS sender → Windows listener transfer (T-WIN-TRANSFER2)

Swap roles from B16.

### B18 — Windows `test-udp` basic diagnostic (T-WIN-UDPTEST1)

```powershell
bore test-udp --to <srv>
```

Expect: NAT type classification and direct-path viability report, same shape
as the Linux/macOS output.

### B19 — Windows two-peer `test-udp --tcp-secret-id` (T-WIN-UDPTEST2)

```powershell
# Windows
bore test-udp --secret S3cret --tcp-secret-id diag
```
```sh
# Linux/macOS, same id
bore test-udp --secret S3cret --tcp-secret-id diag
```

Expect: paired direct-path + relay-fallback latency/bandwidth report on both
sides.

### B20 — Cross-OS matrix (T-WIN-INTEROP-*)

B4/B5, B9, B13, B16/B17 already cover the core local/secret/vhost/server/
transfer pairs. If time allows, repeat each with the Windows side and the
other side swapped between Linux and macOS separately (three-OS coverage),
recording any OS-pair-specific failure in the results table.

---

## Part C — Performance, soak, install, packaging, security

These rows exist in `resume.md`'s test table but are lower-priority smoke/
long-running checks, not correctness gates. Run opportunistically.

- **T-WIN-PERF\*** — Throughput smoke: repeat B1/B9/B16 with a large payload
  and note MB/s; no fixed pass/fail threshold, just a sanity number.
- **T-WIN-SOAK\*** — Leave A4 (direct) and A7 (relay carriers) running
  idle for several hours; confirm no memory growth, no unexpected
  disconnects, direct-path PMTU stays stable (no oscillation, see
  `docs/plans/UDP_DIRECT_PATH_FLAP_PLAN.md` for the mechanism this guards
  against).
- **T-WIN-INSTALL1** — Fresh Windows VM, no dev tools: download the release
  zip (Task in `resume.md` §Phase 6), unzip, run `bore.exe` directly. Confirm
  `wintun.dll` is found without setting `BORE_WINTUN_DLL` (same-directory DLL
  search).
- **T-WIN-PKG1** — Same as T-WIN-INSTALL1 but explicitly checking the raw
  `.exe` + separate `wintun-<target>.dll` release assets (for users who don't
  want the zip) — copy both into one directory, confirm it still loads.
- **T-WIN-SEC1** — Pass shell metacharacters (`; & | $() \` "`) inside
  `--id`/`--advertise`/adapter-name-adjacent flags and confirm they end up
  literally in rule/adapter names (or are rejected) — never interpreted by a
  shell, since every Windows command is built as an argv `Vec<String>`, never
  a concatenated string (T-WIN-SEC1 in `resume.md`).
- **T-WIN-SEC2** — Run two unrelated `bore vpn` links with different `--id`s
  on one Windows host, `kill -9`/Task-Manager-kill one, and confirm
  `stale_reclaim` on the next run deletes only that link's firewall rules/
  WinNAT instances (wildcard prefix match, `link_prefix`) — the other link's
  rules must survive untouched.

---

## Result log — Part A (VPN)

| Step | Pass/Fail | Date | Windows version | Peer OS | Notes |
|------|-----------|------|------------------|---------|-------|
| A1 — relay, Win→peer | | | | | |
| A2 — relay, peer→Win | | | | | |
| A3 — Win↔Win relay | | | | | |
| A4 — direct upgrade | | | | | |
| A5 — direct death→relay | | | | | |
| A6 — direct retry | | | | | |
| A7 — relay carriers | | | | | |
| A8 — direct carriers | | | | | |
| A9 — carrier flow order | | | | | |
| A10 — hub, Win gateway | | | | | |
| A11 — hub, Win spoke | | | | | |
| A12 — gateway LAN advertise | | | | | |
| A13 — mixed direct/relay hub | | | | | |
| A14 — route accepted | | | | | |
| A15 — route refused | | | | | |
| A16 — no-route-manage | | | | | |
| A17 — admin panel live | | | | | |
| A18 — MTU real traffic | | | | | |
| A19 — non-admin fails | | | | | |
| A20 — apply failure rollback | | | | | |

## Result log — Part B (non-VPN cross-OS)

| Step | Pass/Fail | Date | Windows version | Peer OS | Notes |
|------|-----------|------|------------------|---------|-------|
| B1 — public TCP relay | | | | | |
| B2 — public UDP direct | | | | | |
| B3 — public UDP fallback | | | | | |
| B4 — secret, Win provider | | | | | |
| B5 — secret, Win consumer | | | | | |
| B6 — secret carriers | | | | | |
| B7 — admin one row | | | | | |
| B8 — carrier failover | | | | | |
| B9 — vhost TCP | | | | | |
| B10 — vhost UDP direct | | | | | |
| B11 — vhost UDP fallback | | | | | |
| B12 — vhost admin flags | | | | | |
| B13 — Win server (all modes) | | | | | |
| B14 — Win server UDP | | | | | |
| B15 — Win server VPN relay | | | | | |
| B16 — transfer, Win sender | | | | | |
| B17 — transfer, Win listener | | | | | |
| B18 — test-udp basic | | | | | |
| B19 — test-udp paired | | | | | |
| B20 — 3-OS interop matrix | | | | | |

## Result log — Part C (perf/soak/install/security)

| Step | Pass/Fail | Date | Notes |
|------|-----------|------|-------|
| PERF | | | |
| SOAK | | | |
| INSTALL1 | | | |
| PKG1 | | | |
| SEC1 | | | |
| SEC2 | | | |
