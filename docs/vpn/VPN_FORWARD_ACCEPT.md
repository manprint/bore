# VPN FORWARD default-deny gap — `--forward-accept`

**Status:** ✅ IMPLEMENTED + e2e-validated (T-FWD, netns PASS=150/0). Found in the field
2026-06-16 (NAT site↔site, both LANs `192.168.1.0/24`; peer reached only the gateway host).

---

## TL;DR

On a host whose `FORWARD` chain is **default-deny** — the Docker daemon sets `-P FORWARD DROP`,
`ufw`/hardened hosts do the same — a bore VPN **gateway** (`--advertise`) reaches **only itself**.
Every other host behind it times out. bore's NAT rules cannot fix this; the deny is terminal and
lives in a chain bore does not own.

Fix: pass **`--forward-accept`** on the listen (gateway) side. Without it, bore now **detects** the
condition and **warns** with the exact manual remediation.

```bash
sudo bore vpn listen --id adv --secret S \
  --advertise 192.168.1.0/24@10.30.0.0/24 \
  --nat-masquerade \
  --forward-accept
```

---

## Symptom

- Link pairs fine (`relay` or `direct`).
- Peer reaches the **gateway host itself** — its LAN IP, or (with NAT) the virtual address mapping
  to the gateway's own LAN IP.
- Peer **cannot** reach any other host on the advertised LAN.

This is identical whether bore runs natively or inside a `--network host` container.

## Root cause

The gateway must **forward** packets destined for other LAN hosts out its LAN interface. That
traverses the kernel `FORWARD` netfilter hook. The Docker daemon installs `iptables -P FORWARD
DROP` plus allow rules for *its own* bridges only; `bore0 → <lan_if>` matches none → dropped. The
gateway host *itself* is reachable because traffic to it is delivered locally and never hits
`FORWARD`.

bore's NAT/masquerade rules are in a **separate** nftables table (`inet bore_vpn_<id>`). In
netfilter, at a given hook every base chain runs; **`accept` ends only the current chain, but
`drop` (or a base-chain `DROP` policy) is terminal**. So bore cannot override Docker's `FORWARD
DROP` from its own table — no matter the priority. The only reliable fix is to put an `ACCEPT` into
the **same** chain that holds the deny: the iptables `filter FORWARD` chain.

Key trap: the Docker **daemon's** rule persists on the host independent of how bore is launched.
Stopping bore's *own* container does nothing — `docker0`/`br-*` in `ip route` means dockerd is
running and its `FORWARD DROP` is live.

## Behavior

### Default (flag absent) — detect + warn

In gateway mode, bore probes `iptables -S FORWARD` and, if the policy is `DROP` or `REJECT`
(`forward_policy_is_deny`), logs:

```
WARN ... FORWARD chain is default-deny (e.g. Docker daemon / ufw): peers will reach ONLY this
gateway host, NOT other hosts behind it — bore's NAT rules cannot override a FORWARD DROP from
another firewall. Fix: re-run with `--forward-accept`, or add manually:
`iptables -I FORWARD -i bore0 -o wlp0s20f3 -j ACCEPT` and
`iptables -I FORWARD -i wlp0s20f3 -o bore0 -j ACCEPT`.
```

Nothing is changed; this is purely diagnostic.

### `--forward-accept` — punch the chain

bore installs a per-link custom chain and jumps to it from the **top** of `FORWARD` (mirrors the
F3/F4 NAT custom-chain pattern, so teardown is by id alone — `stale_reclaim` works after SIGKILL):

```
iptables -N bore_<id>_fwd
iptables -A bore_<id>_fwd -i bore0     -o <lan_if> -j ACCEPT
iptables -A bore_<id>_fwd -i <lan_if>  -o bore0    -j ACCEPT
iptables -I FORWARD -j bore_<id>_fwd          # inserted at TOP, before Docker's rules/policy
```

Reverted on graceful exit (RAII, reverse order: del jump → flush → del chain) and reclaimed on the
next run after SIGKILL. When punching, bore does **not** also probe (mutually exclusive).

`iptables` is used (not nft) on purpose: the real-world deny lives in `ip filter FORWARD` even on
nftables-backed systems (Docker uses iptables-nft). On a host where `iptables` is absent, bore
`warn!`s that it cannot punch.

## Usage examples

**NAT'd overlapping LAN behind Docker/ufw (the field case):**

```bash
sudo bore vpn listen --id adv --secret S \
  --advertise 192.168.1.0/24@10.30.0.0/24 \
  --nat-masquerade --forward-accept
```

**Plain LAN behind a default-deny FORWARD host:**

```bash
sudo bore vpn listen --id office --secret S \
  --advertise 192.168.50.0/24 --forward-accept
```

**Manual equivalent (no flag; replace `wlp0s20f3` with your LAN interface):**

```bash
sudo iptables -I FORWARD -i bore0 -o wlp0s20f3 -j ACCEPT
sudo iptables -I FORWARD -i wlp0s20f3 -o bore0 -j ACCEPT
```

## Interaction with `--nat-masquerade` (orthogonal)

Two independent host-side conditions gate "reach hosts *behind* the gateway":

| Condition | Symptom if missing | Fix |
|-----------|--------------------|-----|
| **Return path** (LAN host can reply) | host silent even with FORWARD open | plain subnets auto-masqueraded; NAT'd need `--nat-masquerade` (I-NAT5) |
| **Forwarding allowed** (`FORWARD` permits tun↔LAN) | only the gateway itself reachable | `--forward-accept` (or manual rules) |

The field repro needed **both** flags: NAT mapping + `--nat-masquerade` (return path) **and**
`--forward-accept` (Docker's `FORWARD DROP`).

## Limitations

- Only the **iptables** `filter FORWARD` chain is punched. A hand-rolled `nft inet filter forward`
  base chain with policy `drop` (rare; not what Docker/ufw do) is **not** overridden — add an `nft`
  accept rule yourself. (v1 scope.)
- The hub per-peer path inherits the same behavior; `--forward-accept` is set on the gateway/listen
  side and applies to that gateway's TUN↔LAN forwarding.

## Diagnostic gotchas (seen in the field)

- The nft table is **`inet`** family: `nft list table inet bore_vpn_<id>` (querying `ip` always
  says "No such file" — not a real problem).
- The DNAT rule is `iif bore0` (tunnel-only). Pinging the **virtual** CIDR (`10.30.0.x`) **from the
  gateway host itself** never works (no PREROUTING, no route). Test from the **connector**, or ping
  the **real** address on the gateway.

## Implementation

- Builders/parser: `src/vpn.rs` `hostcfg_cmd` — `ipt_fwd_chain`, `cmd_iptables_filter_{new,flush,del}_chain`,
  `cmd_iptables_forward_jump[_del]`, `cmd_iptables_fwd_accept`, `cmd_iptables_list_forward`,
  `forward_accept_cmds`, `forward_policy_is_deny`.
- Wiring: `NetConfig::apply(.., forward_accept)` (install + RAII revert, or probe+warn);
  `stale_reclaim` tears the chain down by id; CLI flag on `bore vpn listen`/`connect`.
- Tests: unit (`cmd_forward_accept_chain_snapshots`, `forward_accept_cmds_order_*`,
  `forward_policy_is_deny_*`, `apply_without_forward_accept_probes_forward_policy`,
  `apply_forward_accept_installs_and_reverts_chain`); e2e (`T-FWD` in `scripts/vpn_netns_test.sh` —
  reproduces the gap, the warning, the fix, and the RAII revert against a real `-P FORWARD DROP`).

See also: [VPN_NAT_ASSESSMENT.md](VPN_NAT_ASSESSMENT.md) (F2 `--nat-masquerade`),
[VPN_USER_FULL_GUIDE.md](VPN_USER_FULL_GUIDE.md) §5.1.
