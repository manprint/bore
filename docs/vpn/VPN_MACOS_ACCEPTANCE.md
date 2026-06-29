# VPN macOS — Manual Two-Host Acceptance Checklist (T-MAC-MANUAL)

> Plan: `docs/plans/plan_VpnMacosCompletion/phase_05.md` §5.2. CI cannot reach a
> two-host LAN, so this is run **by a human** with a Mac (Apple Silicon, macOS
> 13+, root/`sudo`) and a Linux peer running `bore server`/`bore vpn listen`.
> Record results in the table at the bottom.

Each step has an **exact command** and an **exact expected observation**. The
macOS side is the connector unless noted. `<srv>` = the Linux server address;
`utunN` = the kernel-assigned utun name printed at start (`--tun-name` is
advisory on macOS, D7/I-M8).

---

## 1 — Relay bring-up

```sh
sudo bore vpn connect --to <srv> --secret s --id m1 --tun-name auto --accept-all-routes
```

Expect:
- Log line `macOS utun created (single queue, no offload)` with a `utunN` name.
- `ifconfig utunN` shows the overlay `/30` (e.g. `inet 10.x.x.2 --> 10.x.x.1`).
- From the Linux peer, `ping <macOS overlay addr>` succeeds.

## 2 — Direct upgrade (relay → direct QUIC)

With the link from step 1 up (no `--relay-only`):

Expect:
- Within the direct-retry grid (~30 s), logs show the relay→direct switch
  (`direct path established` / link switches to Direct).
- `ping` from the Linux peer **continues across the switch** (seamless fallback;
  shared data plane, I-M3) — no sustained loss.

## 3 — Gateway netmap (PF `binat`)

Restart the macOS connector advertising a real LAN behind it, NAT-mapped:

```sh
sudo bore vpn connect --to <srv> --secret s --id m1 --tun-name auto \
  --advertise 192.168.7.0/24@10.77.0.0/24 --nat-masquerade --accept-all-routes
```

Expect:
- `sysctl -n net.inet.ip.forwarding` → `1`.
- `sudo pfctl -a bore_vpn/m1 -sa` shows a `binat` rule (192.168.7.0/24 ↔
  10.77.0.0/24, host-bit preserving) **and** a `nat` (masquerade) rule.
- From the Linux peer, reaching `10.77.0.x` lands on the real host `192.168.7.x`
  behind the Mac.

## 4 — RAII teardown (Ctrl-C)

`Ctrl-C` the macOS `bore vpn` process.

Expect:
- `ifconfig utunN` → `interface does not exist` (utun gone).
- `sudo pfctl -a bore_vpn/m1 -sa` → empty (anchor flushed).
- `sysctl -n net.inet.ip.forwarding` → back to its **pre-run** value.

## 5 — SIGKILL recovery (`stale_reclaim`)

Re-run step 3, then:

```sh
sudo kill -9 <bore pid>          # no RAII Drop runs
sudo bore vpn connect --to <srv> --secret s --id m1 --tun-name auto \
  --advertise 192.168.7.0/24@10.77.0.0/24 --nat-masquerade --accept-all-routes
```

Expect (on the restart, before re-applying):
- `stale_reclaim` flushed the stale anchor and restored forwarding — no leaked
  rules linger: `sudo pfctl -a bore_vpn/m1 -sa` reflects only the fresh run.
- `net.inet.ip.forwarding` was restored from the `/var/run` state file (then
  re-enabled by the new run).

## 6 — Flag warnings

```sh
sudo bore vpn connect --to <srv> --secret s --id m1 --tun-name auto \
  --accept-all-routes --tun-queues 4 --stun-server x
```

Expect:
- A warning that `--tun-queues` is ignored on macOS (utun has no multi-queue,
  using 1 queue).
- A warning that the UDP hole-punch helper flags (`--upnp`/`--stun-server`/
  `--try-port-prediction`/`--nat-udp-*`) are advisory/unsupported on macOS.
- The link still comes up normally (warnings are advisory; no control-flow
  change).

---

## Result log

| Step | Pass/Fail | Date | macOS version | Notes |
|------|-----------|------|---------------|-------|
| 1 — relay bring-up | | | | |
| 2 — direct upgrade | | | | |
| 3 — gateway netmap | | | | |
| 4 — RAII teardown | | | | |
| 5 — SIGKILL recovery | | | | |
| 6 — flag warnings | | | | |
