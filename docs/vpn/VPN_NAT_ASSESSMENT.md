# VPN NAT (1:1 netmap) — Bug-Hunt Assessment

> Date: 2026-06-15. Trigger: `--advertise R@V` NAT shipped broken (host-bit scramble) and
> reached QA. This is a full, **empirically verified** hunt of the NAT feature + adjacent
> gateway data-plane. Every finding was reproduced in a `netns` harness with the *exact*
> rules `bore` emits (nft 1.0.9 / kernel 7.0 — the same stack as the affected hosts).

## Severity table

| # | Sev | Affects | Status | One-liner |
|---|-----|---------|--------|-----------|
| **F1** | CRITICAL | **nft hosts (prod)** | ✅ FIXED + e2e guard | nft netmap missing `prefix` keyword → host bits scrambled |
| **F2** | HIGH | all hosts, NAT'd subnets | ✅ FIXED (`--nat-masquerade`, opt-in) | NAT'd subnet reaches only the *gateway itself*; other LAN hosts unreachable unless gw is their router |
| **F3** | MED | iptables-only hosts | ✅ FIXED (custom-chain teardown) | `stale_reclaim` comment-only delete does not match full-spec rules → SIGKILL leaks NAT rules |
| **F4** | MED-HIGH | iptables-only hosts | ✅ FIXED (custom chains, no `-i`) | masquerade builders use `-i` in POSTROUTING → `apply` errors out (gateway mode dead on iptables) |
| **G**  | — | test suite | ✅ FIXED | NAT tests used lo-aliases + `ping` (non-deterministic), were never run, iptables path untested |

**Resolution (2026-06-15, all verified):**
- **F2** → `--nat-masquerade` opt-in flag (default OFF = unchanged I-NAT5; ON adds a scoped masquerade
  of each NAT'd *real* subnet toward the LAN). Wired through nft + iptables `apply` and the
  `--no-route-manage` print path. Kernel-verified: Case A (off) NO-RESP, Case B (on) reaches the
  separate LAN host.
- **F3+F4** → iptables NAT moved to per-link **custom chains** `bore_<id>_pre` / `bore_<id>_post`
  (jumped from PRE/POSTROUTING). No `-i` in POSTROUTING (F4); teardown + `stale_reclaim` do
  `-D jump; -F; -X` by id only — no comment-matching (F3). A subagent draft dropped `-o <tun>` from
  the SNAT rule (would have mangled the real LAN's normal traffic) — caught in review and restored.
  New `BORE_VPN_FORCE_IPTABLES=1` test hook forces this path on nft hosts.
- **e2e:** T-NAT3 now asserts exact host-bit identity (fail-on-bug proven); **T-NAT-IPT** runs the
  whole iptables path through the real binary (apply ✓, NETMAP identity ✓, chains fully reclaimed ✓).
  Full netns suite: **139 PASS / 0 FAIL**.
- **Remaining test gap (follow-up):** an e2e for `--nat-masquerade` forwarding to a *separate* host
  behind the gateway needs a 4th netns; the masquerade rule itself is kernel-verified (Case A/B).

---

## F1 — nft netmap scrambles host bits  (CRITICAL, FIXED)

**Root cause.** `cmd_nft_add_netmap_dnat/snat` emitted `dnat ip to <prefix>` / `snat ip to <prefix>`.
Without the **`prefix`** keyword, nft treats the target as a plain *range* and the kernel
picks an address by hash — host bits are **not** preserved.

**Evidence (netns, exact rule bore emitted):**
```
dnat ip to 10.10.16.0/24   :  100.100.16.138 → 10.10.16.8    100.100.16.50 → 10.10.16.112   ❌
snat ip to 100.100.16.0/24 :  10.10.16.138   → 100.100.16.152 10.10.16.50  → 100.100.16.178 ❌
dnat ip prefix to ...      :  .138 → .138   .50 → .50   .23 → .23                            ✅
snat ip prefix to ...      :  .138 → .138   .50 → .50                                        ✅
```
So a connect to `100.100.16.138:5000` was DNAT'd to `10.10.16.8` (nothing there) → silent
timeout. The `map { ... }` form was tested too — same scramble. The iptables `NETMAP` target
was always correct.

**Fix.** Insert `prefix` in both builders (`src/vpn.rs`). Snapshot tests updated.
**Regression guard.** `scripts/vpn_netns_test.sh` T-NAT3 now asserts **exact** host-bit identity
via a socat listener that echoes `$SOCAT_SOCKADDR` (the address actually hit). Verified: the
assertion FAILS on the broken rule and PASSES on the fixed rule.

---

## F2 — NAT'd subnet reaches only the gateway, not other LAN hosts  (HIGH, decision needed)

**Symptom.** With `--advertise 10.10.16.0/24@100.100.16.0/24`, a peer can reach the gateway's
*own* IP (`100.100.16.138 → 10.10.16.138`, local delivery) but **not** another host on that LAN
(`100.100.16.50 → 10.10.16.50`) unless the gateway is that host's default router.

**Why.** A **plain** advertised subnet gets a masquerade toward the LAN (`iif tun oif lan_if …
masquerade`), so the LAN host sees the gateway's IP and can always reply on-link. For a **NAT'd**
subnet the masquerade is deliberately dropped (invariant I-NAT5: "source already peer virtual").
That assumption is false for topology **B** (roaming client: source = overlay `10.99.x`, which the
LAN host has no route back to) and for any deployment where the gateway is not the LAN's router.

**Evidence (netns, separate LAN host behind gateway, gw NOT its router):**
```
CASE A  bore NAT ruleset as-is (no LAN masquerade)        : 100.100.16.50:5000 → NO-RESP   ❌
CASE B  + scoped masq  (iif tun oif lan_if daddr <real>)  : 100.100.16.50:5000 → I-AM-LAN50 ✅
```
(Contrast: a **plain** subnet with the existing scoped masquerade reached the same unrouted LAN
host fine — confirming NAT mode is strictly weaker.)

**Fix options (your call):**
- **(a) Add scoped masquerade for NAT'd subnets too** (`… ip daddr <real> masquerade`). Makes NAT
  mode reach all LAN hosts like plain mode. Cost: the LAN host sees the *gateway* IP, not the
  peer's virtual — loses per-peer source visibility (matters for site↔site identical-LAN ACLs).
- **(b) Keep current behavior; document** that NAT'd LANs require the bore gateway to be the LAN
  router (ip_forward + on-link), preserving per-peer source. Pure docs.
- **(c) Make it opt-in** (`--nat-masquerade`), default off (b), on = (a). Best of both; small CLI add.

For the reported use case (app runs *on* the gateway host) F1's fix is sufficient — F2 only bites
when reaching *other* hosts behind the gateway.

---

## F3 + F4 — iptables fallback path is broken  (iptables-only hosts; coupled)

Modern hosts use nft (incl. the affected prod hosts), so this path is rarely taken — but it is
**dead code that looks alive**, the same failure class as F1.

**F4 (apply crashes).** `cmd_iptables_masquerade_add` and `cmd_iptables_masquerade_scoped_add`
build `-A POSTROUTING -i <tun> …`. iptables rejects `-i` in POSTROUTING:
```
iptables v1.8.10 (nf_tables): Can't use -i with POSTROUTING
```
→ `NetConfig::apply` returns `Err` at "iptables masquerade add" → gateway mode never starts on an
iptables-only host (this affects the **plain** path too, not just NAT).

**F3 (cleanup leaks).** `stale_reclaim` (SIGKILL recovery) deletes with a bare
`-D POSTROUTING -m comment --comment bore_vpn_<id>`. Verified it does **not** match a full-spec
rule:
```
iptables -t nat -D POSTROUTING -m comment --comment bore_vpn_x  →  "Bad rule (does a matching rule exist?)"
(rule with -s/-o/-j NETMAP remains)
```
The in-code comment claiming "delete-by-comment flushes every leaked rule" is **false** on
iptables-nft. → leaked DNAT/SNAT/masquerade rules after SIGKILL.

**They are coupled:** fixing F4 alone makes the masquerade actually get added, but the normal
`Drop` delete (also comment-only for the blanket rule) then leaks it on *every* teardown.

**Recommended fix (dedicated, tested pass):** put all per-link iptables nat rules in a **custom
chain** `BORE_VPN_<id>` (jumped from PRE/POSTROUTING), mirroring nft-table semantics:
- adds: valid rules (no `-i` in POSTROUTING; scope masquerade by `-o lan_if [-d <subnet>]`);
- teardown + `stale_reclaim`: `-D … -j BORE_VPN_<id>; -F BORE_VPN_<id>; -X BORE_VPN_<id>` — needs
  only the `id`, deletes everything atomically, no comment-matching.
- Add an iptables-forced netns test (`nft`-hidden) so this path is exercised like nft is.

**Implemented** exactly as above (custom chains) + a `BORE_VPN_FORCE_IPTABLES=1` test hook so the
path is exercised through the real binary (T-NAT-IPT). It is no longer untestable: T-NAT-IPT proves
apply succeeds, NETMAP preserves host bits, and teardown leaves zero `bore_*` chains.

---

## G — Test coverage gap (FIXED for F1, outstanding for F3/F4/F2)

The pre-existing T-NAT1/3 used **lo aliases on the gateway** (local delivery, never true
forwarding) and **`ping` reachability** — non-deterministic (a scrambled bit landing on another
assigned alias answers anyway) — and **were never executed** ("nat plan 1:1, to test").

- ✅ **Added:** deterministic exact-host-bit identity assertion (T-NAT3, socat `$SOCAT_SOCKADDR`).
- ✅ **Added:** T-NAT-IPT — forced-iptables run (guards F3/F4) through the real binary.
- ✅ **Added:** T-NAT-MASQ — differential e2e through the real binary to a *separate* host behind
  the gateway (dedicated `ns_lanm`): WITHOUT the flag the host is unreachable (gap reproduced),
  WITH `--nat-masquerade` it is reachable with the exact host bit. Guards F2 end-to-end.

## What was fixed in this pass
- **F1**: `prefix` keyword (`src/vpn.rs` netmap builders + snapshots).
- **F2**: `--nat-masquerade` opt-in flag (CLI + nft/iptables apply + `--no-route-manage` print).
- **F3+F4**: iptables NAT refactored to per-link custom chains (valid rules, atomic teardown,
  `stale_reclaim` by id, force-iptables test hook).
- Logging: listener prints `real ⇄ exposed` per entry; connector prints per accepted route
  (real subnet stays hidden by design — I-NAT2).
- Tests: T-NAT3 hardened to a real regression guard (fail-on-bug proven) + T-NAT-IPT (forced
  iptables) + T-NAT-MASQ (differential `--nat-masquerade` forwarding to a separate host).
  Full netns suite **142 PASS / 0 FAIL**; `cargo test --lib` **247 PASS**; clippy clean.
- Docs: this assessment + `VPN_NAT_PLAN.md` note recording the resolved nft syntax.
