# VPN Overlapping-Subnet NAT (1:1 stateless netmap) — Design & Implementation Plan

> **Status:** planning (no code written). Implementation handoff doc.
> **Authoring model:** Opus 4.8 (architecture).
> **Implementation models:** annotated per sub-phase (Haiku 4.5 = mechanical/bulk, Sonnet 4.6 = features/refactor/tests, Opus 4.8 = architecture review gate only).
> **Target:** unblock VPN links between sites with **identical/overlapping private LANs** (TODO item **E3**) via per-subnet stateless 1:1 NAT (netmap). Correct on both UDP-direct and TCP-relay paths, all three gateway topologies (B site↔host, C site↔site, D hub-and-spoke). Minimize token usage during implementation (delegate mechanical sub-phases to Haiku).
> **Locked decisions (user-approved 2026-06-13):** CLI syntax `--advertise <real>@<virtual>`; NAT engine = **stateless 1:1 netmap** (not stateful masquerade+DNAT); scope = Topologies **B + C + D** (hub included).

---

## 1. Context & problem

Today the server **rejects** any link whose advertised subnets overlap. `check_overlap`
(`src/vpn_server.rs:436-453`) chains the overlay subnet + listener advertised + connector
advertised into one vector and tests every pair with `Ipv4Net::overlaps`
(`src/shared.rs:547-554`); the first collision returns
`Some("overlapping subnets: X and Y")`. The 1:1 pairing path raises it as a `VpnError`
(`src/vpn_server.rs:1332-1333`); the hub listener checks its own advertised vs the hub
subnet (`src/vpn_server.rs:602`). `docs/vpn/VPN.md:326-330` documents this as a **v1
limitation** and names the future fix: *"per-subnet 1:1 NAT (DNAT/SNAT remapping)."*

This blocks the common real-world case: two offices both numbered `192.168.1.0/24` cannot
be joined, because each side's advertised LAN collides with the other's (and routing a
destination that also exists on the local LAN is ambiguous).

### Goal

A gateway exposes its real LAN to peers under a **distinct virtual CIDR**. Peers route the
virtual; the advertising gateway translates virtual↔real with **stateless 1:1 netmap**
(host-bits preserved). Two sites with identical real LANs each pick a distinct virtual and
reach each other's hosts by virtual address. Works identically on relay and direct, in
site↔host, site↔site, and hub-and-spoke topologies.

### Reference scenario (final acceptance test)

```
# Both sites have the SAME real LAN 192.168.1.0/24.
site-A (listen)  --advertise 192.168.1.0/24@10.50.1.0/24   # expose A's LAN as 10.50.1.0/24
site-B (connect) --advertise 192.168.1.0/24@10.60.1.0/24 --accept-all-routes

# From A's gateway:  ping 10.60.1.10  ->  reaches B's real 192.168.1.10
# From B's gateway:  ping 10.50.1.5   ->  reaches A's real 192.168.1.5
# B's host 192.168.1.10 sees the caller as 10.50.1.5 (stable 1:1, no collision).
```

### Why netmap and not masquerade (the locked decision, for implementers)

Stateful "DNAT + keep masquerade" **fails the symmetric-overlap case**. Trace, both LANs
`192.168.1.0/24`, A-host `.5` → B-host `.10` (addressed as virtual `10.60.1.10`):

```
1. A-host 192.168.1.5 -> 10.60.1.10        (A does not rewrite source)   -> tunnel
2. B prerouting DNAT  dst 10.60.1.10 -> 192.168.1.10
3. B masquerade       src 192.168.1.5 -> 192.168.1.1 (B gw LAN IP)        -> B-host
4. B-host reply  src 192.168.1.10 -> dst 192.168.1.1
5. B conntrack reverse: un-masq dst -> 192.168.1.5 ; un-DNAT src -> 10.60.1.10
   reply now: src=10.60.1.10  dst=192.168.1.5
6. B must route dst=192.168.1.5 into the tunnel -- but 192.168.1.0/24 IS B's own LAN.
   -> delivered to B's local LAN. BLACKHOLE.   (X)
```

Stateless netmap (each gateway maps **only its own** real↔virtual, both directions) works:

```
A: ingress  daddr 10.50.1.0/24 dnat-> 192.168.1.0/24   egress  saddr 192.168.1.0/24 snat-> 10.50.1.0/24
B: ingress  daddr 10.60.1.0/24 dnat-> 192.168.1.0/24   egress  saddr 192.168.1.0/24 snat-> 10.60.1.0/24

1. A-host 192.168.1.5 -> 10.60.1.10   (oif bore0)
2. A egress SNAT  src 192.168.1.5 -> 10.50.1.5      [src 10.50.1.5  dst 10.60.1.10]  -> tunnel
3. B ingress DNAT dst 10.60.1.10 -> 192.168.1.10    [src 10.50.1.5  dst 192.168.1.10] -> B-host
4. B-host sees src 10.50.1.5 (NO collision). reply -> dst 10.50.1.5
5. B routes 10.50.1.0/24 into tunnel. egress SNAT src 192.168.1.10 -> 10.60.1.10 -> tunnel
6. A ingress DNAT dst 10.50.1.5 -> 192.168.1.5  -> A-host.   (OK)
```

Stateless, symmetric, per-host identity preserved, no conntrack. This is the standard
"1:1 NAT" of strongSwan/OPNsense.

---

## 2. Approved design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **N1** | **CLI: extend `--advertise` with `<real>@<virtual>`.** Plain `<cidr>` (no `@`) = no NAT, today's behavior. | Per-subnet, backward-compatible, one flag. Convention: `<local-real>@<exposed-virtual>`. |
| **N2** | **Engine = stateless 1:1 netmap** (nft prefix `dnat`/`snat`; iptables `NETMAP` fallback). | Host-bits preserved, no conntrack, symmetric. Masquerade alone is provably broken (§1). |
| **N3** | **Wire carries virtuals only.** Real subnets never leave the local host. | **Zero new protocol.** `advertised: Vec<Ipv4Net>` already carries the virtuals; server overlap check operates on virtuals unchanged. |
| **N4** | **Real & virtual must have the same prefix length** (1:1 netmap). | Validated at CLI parse; mismatch = hard error. Many-real→one-virtual (PAT/overload) is out of scope (stateful). |
| **N5** | **Each gateway netmaps only its own real↔virtual.** | No global/peer state; identical rules regardless of which/how many peers; link-path-agnostic (relay==direct). |
| **N6** | **NAT'd subnets are NOT masqueraded.** | The arriving source is already a peer-side virtual (non-colliding). Masquerade would destroy source identity + force conntrack. When NAT is present, masquerade is scoped per **plain** subnet only. |
| **N7** | **LAN-egress iface + ip_forward use the REAL subnet.** | The virtual has no local route; `ip route get` must target a real host. |
| **N8** | **No-NAT path is byte-for-byte unchanged.** | If no advertise entry contains `@`, `NetConfig::apply` takes exactly today's blanket-masquerade path; no prerouting chain, no netmap rule. Zero-regression invariant (mirrors I-MC1). |
| **N9** | **NAT works in B, C, D.** Site↔host, site↔site (both map), hub (hub maps its LANs; spokes route virtuals). | One mechanism covers all: netmap is dst/src-based, peer-agnostic. Hub spokes never `--advertise` (D4) so spoke source is always its unique overlay IP — no spoke-side NAT. |

---

## 3. Target architecture

### 3.1 The mapping object

A `--advertise` item parses into:

```rust
/// One advertised subnet, optionally NAT-mapped.
pub struct AdvertiseEntry {
    /// Real local subnet (the actual LAN behind this gateway).
    pub real: Ipv4Net,
    /// Subnet exposed to peers over the wire. Equals `real` when no `@` mapping is given.
    pub exposed: Ipv4Net,
}
impl AdvertiseEntry {
    pub fn is_nat(&self) -> bool { self.real != self.exposed }
}
```

- Parse `<real>@<exposed>`: split on the single `@`. No `@` ⇒ `real == exposed` (plain).
- **Validate** `real.prefix == exposed.prefix` (N4) and exactly one `@`; else error.
- **On the wire** (`HelloVpn.advertised` / `ConnectVpn.advertised`, `src/shared.rs:692,710`):
  send `entry.exposed` for every entry (N3).
- **Locally** (`NetConfig::apply`): pass `entries.iter().map(|e| e.real)` as the `advertised`
  argument (drives `is_gateway`, LAN-iface detection, masquerade scope — N7), plus a new
  `nat_maps: &[(Ipv4Net /*real*/, Ipv4Net /*exposed*/)]` for the entries where `is_nat()`.

### 3.2 Kernel rule plane (the only thing that changes mechanically)

bore treats IP packets as **opaque** between TUN and the encrypted link — it never rewrites
headers (confirmed: router uplink reads dst at offset `16..20` only, `src/vpn.rs:4682,4784`;
downlink writes whole packets, `src/vpn.rs:4088`; relay seals/opens opaque payloads,
`src/vpn.rs:3440-3444,3364-3374`). **All NAT is kernel-side**, exactly like the existing
masquerade/MSS rules. So the feature = new nft/iptables rules in `hostcfg::NetConfig`, nothing
in the Rust data plane.

For each NAT entry (real `R`, exposed `V`, equal prefix), on the advertising gateway:

```
# nft (preferred — same bore_vpn_<id> table as masquerade/MSS):
#   need a NEW prerouting nat chain (only postrouting pri 100 + forward pri -10 exist today)
nft add chain inet bore_vpn_<id> pre { type nat hook prerouting priority -100 ; }
nft add rule  inet bore_vpn_<id> pre  iif <tun> ip daddr <V> dnat ip prefix to <R>   # ingress  V->R
nft add rule  inet bore_vpn_<id> post oif <tun> ip saddr <R> snat ip prefix to <V>   # egress   R->V

# iptables fallback (rock-solid stateless NETMAP target):
iptables -t nat -A PREROUTING  -i <tun> -d <V> -j NETMAP --to <R>
iptables -t nat -A POSTROUTING -o <tun> -s <R> -j NETMAP --to <V>
```

> **RESOLVED (2026-06-15, was "uncertain detail"):** the prefix-preserving 1:1 nft netmap
> **requires the `prefix` keyword**: `dnat ip prefix to <R>` / `snat ip prefix to <V>`. The
> plain `dnat ip to <prefix>` form (and the `dnat ip to ... map {...}` form) do **NOT** preserve
> host bits — the kernel treats the target as a range and scrambles the host part (empirically on
> **nft 1.0.9 / kernel 7.0**: `100.100.16.138 → 10.10.16.8`, `.50 → .112`). This shipped broken in
> commit `8564ae0` ("nat plan 1:1, to test") and silently timed out every NAT'd connection on
> nft hosts. Fixed by inserting `prefix` in `cmd_nft_add_netmap_dnat`/`cmd_nft_add_netmap_snat`.
> Verified in a netns harness that `dnat ip prefix to` maps `.138 → .138`, `.50 → .50`, `.23 → .23`
> and `snat ip prefix to` mirrors it. The iptables `NETMAP` target was always correct (reference
> fallback). **Lesson: the Phase 1.0 spike validated only that the rule *renders* (`dstnat`), not
> that the translation *preserves host bits at runtime* — a render-check is not a behavior-check.**

**Masquerade coexistence (N6).** Today `apply` installs one blanket rule
`iif <tun> oif <lan_if> masquerade` (`src/vpn.rs:2653`). That rule would wrongly source-NAT
NAT'd tunnel→LAN traffic (whose source is already a peer virtual). Therefore:

- **No NAT entries present** ⇒ today's blanket masquerade, no prerouting chain (N8, byte-identical).
- **Any NAT entry present** ⇒
  - install netmap (DNAT+SNAT) for each NAT entry;
  - replace the blanket masquerade with **per-plain-subnet** masquerade scoped by destination:
    `iif <tun> oif <lan_if> ip daddr <plain_subnet> masquerade` for each non-NAT advertised
    subnet (so plain LANs keep today's semantics; NAT'd LANs are handled purely by netmap).

### 3.3 Per-topology behavior

- **B (site↔host).** Gateway advertises `R@V`; installs netmap. Roaming client routes `V`
  (via `filter_accepted`, `src/vpn.rs:183-189`) and addresses `V` hosts. Only the **ingress
  DNAT** fires (client source is its unique overlay `/30` addr, not in `R`, so the egress SNAT
  never matches it). No client-side NAT.
- **C (site↔site).** Both sides are gateways: each advertises its own `R@V` and routes the
  peer's `V`. Each installs netmap for **its own** subnet. Symmetric (the §1 trace).
- **D (hub-and-spoke).** Hub advertises `R@V`, `NetConfig::apply(.., hub=true)` installs
  netmap. Spokes route `V` per policy; spokes never `--advertise` (D4) so each spoke's source
  is its unique overlay `.N` — the hub's egress SNAT (`saddr R`) never matches spoke sources,
  the hub router keys replies on the spoke overlay (unchanged). Netmap is dst/src-based →
  one rule set serves all spokes. Spoke-isolation (forward hook) and netmap (nat hooks) are
  independent chains, no interaction.

### 3.4 Reuse map (do not reinvent)

| Need | Reuse / extend | Location |
|------|----------------|----------|
| nft table + chains + revert | `hostcfg_cmd::cmd_nft_*` builders | `src/vpn.rs:1800-1895` |
| nft masquerade rule (scope it) | `cmd_nft_add_masquerade_rule` | `src/vpn.rs:1835` |
| nft postrouting chain (reuse for SNAT) | `cmd_nft_add_postrouting_chain` | `src/vpn.rs:1822` |
| iptables fallback pattern | `cmd_iptables_masquerade_add/_del` | `src/vpn.rs:1898+` |
| Apply/revert/ip_forward/stale-reclaim | `hostcfg::NetConfig` (`apply`, `Drop`, `stale_reclaim`) | `src/vpn.rs:2369,2516,2546,2727` |
| Route install (peer virtuals) | route loop in `apply` (`cmd_route_add`) | `src/vpn.rs:2575` |
| Connector route filter | `routes::filter_accepted` | `src/vpn.rs:183-189` |
| Advertise CLI parse pattern | `--advertise` clap + `Ipv4Net::from_str` | `src/main.rs:891-895,1475-1480,1523-1528` |
| Overlap check (virtuals) | `check_overlap` (unchanged) | `src/vpn_server.rs:436-453` |
| `Ipv4Net` prefix/network helpers | `network/contains/overlaps/prefix` | `src/shared.rs:533-562` |

---

## 4. New CLI syntax

### 4.1 `bore vpn listen` / `bore vpn connect` — `--advertise`

```
--advertise <ITEM,...>     ITEM = <cidr>                 (no NAT; advertised == real)
                           ITEM = <real-cidr>@<virtual>  (NAT; expose real LAN as virtual)
                           value_delimiter=','  (unchanged); env BORE_VPN_ADVERTISE
```

Examples:

```
--advertise 192.168.50.0/24                              # plain (today)
--advertise 192.168.1.0/24@10.50.1.0/24                  # NAT: real -> exposed
--advertise 192.168.1.0/24@10.50.1.0/24,172.16.0.0/24    # mixed list (one NAT, one plain)
```

- Convention `<local-real>@<exposed-virtual>`. **Only the virtual is advertised** to peers.
- Validation (CLI parse, fail fast with a clear message):
  - exactly one `@` per NAT item;
  - both sides parse as `Ipv4Net`;
  - `real.prefix == exposed.prefix` (N4);
  - the exposed CIDR should not equal the overlay pool family in a way the server will reject
    (deferred to the server's existing overlap check — virtuals are what the server sees).
- No new flag on `bore server`. (NAT is purely a gateway-client concern; the server only ever
  sees virtuals.)

### 4.2 No change to `--accept-routes` / `--refuse-routes`

The connector filters the peer's **virtuals** (`filter_accepted`, unchanged). A spoke accepts
`10.50.1.0/24` (the exposed CIDR), never the hidden real `192.168.1.0/24`.

---

## 5. Protocol — **no change**

`HelloVpn.advertised` / `ConnectVpn.advertised` / `VpnReady.peer_advertised` /
`VpnPeerJoin.peer_advertised` already carry `Vec<Ipv4Net>`; they now carry **virtuals**. The
wire format and the server's `check_overlap` are untouched. Real subnets are gateway-local
and never serialized. This is the central simplification of the netmap approach and a strong
backward-compatibility property: a NAT-using client interoperates with an unmodified server.

> Optional, deferred (Phase 4.4): an admin-only `#[serde(default)]` mapping field for
> cross-host visibility of "this exposed CIDR is NAT'd to a real CIDR". Not required for
> function; the advertising gateway already knows and can log/show its own mapping locally.

---

## 5.1 Logging & observability (cross-cutting — every route/NAT phase MUST satisfy this)

A user reading a client's logs must understand, with **no guesswork**, (a) which routes are
advertised, (b) which routes are installed locally, and (c) which NAT rules are in force.
Implement these at `info` (the codebase already uses `tracing` with structured fields like
`%subnet`, `%tun_name`). This is a **hard done-criterion** for Phases 1–3 and 5, not optional
polish.

**Required log lines (use these exact event messages + fields so tests can grep them):**

1. **Advertise summary (gateway, once at link-up), one line per entry + one rollup:**
   ```
   info!(real=%e.real, exposed=%e.exposed, nat=e.is_nat(), "vpn advertise entry");
   info!(count=entries.len(), nat_mapped=n_nat, "vpn advertise summary");
   ```
   Plain entry logs `nat=false` with `real==exposed`; NAT entry logs both distinct.

2. **NAT rule application (gateway, as each rule is installed — extend the existing per-rule
   `info!` in `NetConfig::apply`):**
   ```
   info!(exposed=%v, real=%r, %tun, "nat netmap: dnat ingress  exposed -> real");
   info!(real=%r, exposed=%v, %tun, "nat netmap: snat egress   real -> exposed");
   info!(plain=%p, %lan_if, %tun, "masquerade (scoped to plain subnet)");   // only when NAT present
   ```
   The existing masquerade/MSS/isolation `info!` lines stay. On revert, the existing
   `NetConfig::Drop` already logs each removed rule with its label — extend the labels so the
   netmap rules read clearly (`"del nat netmap dnat 10.50.1.0/24"` etc.).

3. **Connector/spoke route resolution (once, after `filter_accepted`):**
   ```
   info!(accepted=?accepted, refused=?refused_set, host_only=accepted.is_empty(),
         "vpn routes resolved");
   info!(route=%v, %tun, "route added");   // per installed route (already emitted by apply)
   ```
   The connector only ever sees **exposed** CIDRs (it cannot tell plain from NAT-mapped — the
   real subnet is hidden by design, N3). Emit a one-time hint at `debug`:
   `debug!("advertised routes may be NAT-mapped on the peer; addresses shown are the exposed CIDRs")`.

4. **Canonical route-table summary (both roles, single `info!` line at steady state)** so an
   operator sees the whole picture without correlating lines:
   ```
   info!(advertised=?exposed_list, nat_maps=?map_list, peer_routes=?installed,
         path=%current_path, "vpn link route summary");
   ```

5. **Warnings (preflight, client-side, at `warn`):**
   - exposed CIDR equals/overlaps the overlay pool family → `warn!` (server will also reject).
   - `--accept-routes <CIDR>` not covered by any advertised CIDR → `warn!` + skip (mirrors the
     multi-client behavior).
   - prefix-length mismatch in `R@V` is a hard parse error (not a warning).

6. **`--no-route-manage`:** every netmap + scoped-masquerade command is printed verbatim
   (Phase 4.2), prefixed exactly like the existing skipped commands
   (`# (skipped, --no-route-manage): nft ...`), so the operator can paste them.

**Test hook:** where a netns e2e asserts behavior, also `grep` the client log for the matching
event (e.g. `vpn advertise entry ... nat=true`, `nat netmap: dnat`). This makes the logging a
tested artifact, not a hope.

---

## 6. Implementation phases

**Global rules (CLAUDE.md):** tests first or alongside; every sub-phase must pass
`cargo fmt --check`, `cargo clippy --features vpn -- -D warnings`, `cargo test --features vpn`;
zero regressions; update docs when behavior/APIs change; print the model used per sub-task;
satisfy the §5.1 logging requirements.

Each sub-phase lists: **agent (complexity)**, **files**, **change**, **unit tests**,
**e2e tests**, **done-criteria**.

### 6.0 Agent-assignment legend (by complexity — CLAUDE.md tiers)

| Tier | Agent | Use for | In this plan |
|------|-------|---------|--------------|
| **Mechanical** | **Haiku 4.5** | struct/enum edits, clap text, string-split parsing against a fixed spec, argv builders that mirror an existing one, doc edits, print-block extension | 0.1, 1.1, 4.2, 4.6, 5.5 |
| **Feature** | **Sonnet 4.6** | integration across files, refactors, the apply rule plane, all harness/e2e authoring, server changes, benches | 0.2, 1.0(run), 1.2, 2.1, 2.2, 3.1, 3.2, 4.1, 4.3, 4.4, 4.5, 5.2, 5.3, 5.4 |
| **Architecture / review gate** | **Opus 4.8** | design sign-off only — never bulk implementation | 1.0(sign-off), 1.2(design), 2.2(assertions), 5.1(scenario matrix), 5.6(acceptance), 4.6(final docs read) |

> Every Haiku sub-phase is followed by a **mandatory Sonnet review** before merge (the plan
> supplies exact test vectors so Haiku implements against a fixed target; Sonnet confirms edge
> cases + gates). Opus appears **only** at the marked review gates.

**Per-sub-phase quick map:**

| Sub-phase | Primary | Reviewer | Complexity |
|-----------|---------|----------|------------|
| 0.1 `AdvertiseEntry`+`R@V` parse | Haiku | Sonnet | Mechanical (spec-driven) |
| 0.2 thread entries + advertise virtuals | Sonnet | — | Feature (plumbing, regression) |
| 1.0 nft netmap syntax spike | Sonnet (run) | **Opus** | Architecture (uncertain detail) |
| 1.1 nft/iptables netmap builders | Haiku | Sonnet | Mechanical (mirror builders) |
| 1.2 integrate into `NetConfig::apply` | Sonnet | **Opus** | Architecture (rule coexistence) |
| 2.1 client wire-up (both roles) | Sonnet | — | Feature |
| 2.2 site↔host / site↔site e2e | Sonnet | **Opus** | Feature + acceptance review |
| 3.1 hub `apply(hub=true)` + maps | Sonnet | — | Feature |
| 3.2 hub NAT e2e | Sonnet | — | Feature |
| 4.1 server sanity (virtuals only) | Sonnet | — | Feature |
| 4.2 `--no-route-manage` print | Haiku | Sonnet | Mechanical |
| 4.3 SIGKILL stale reclaim | Sonnet | — | Feature |
| 4.4 admin visibility | Sonnet | — | Feature |
| 4.5 bench guardrail | Sonnet | — | Feature |
| 4.6 docs | Haiku | **Opus** | Mechanical + final read |
| 5.1 real-world scenario matrix | **Opus** | — | Architecture (test design) |
| 5.2 multi-protocol + bidirectional services | Sonnet | — | Feature (harness) |
| 5.3 mixed mesh (NAT+plain+clean) via hub | Sonnet | — | Feature (harness) |
| 5.4 resilience under NAT | Sonnet | — | Feature (harness) |
| 5.5 ALG limits doc + manual two-host matrix | Haiku | Sonnet | Mechanical + manual |
| 5.6 acceptance sign-off | **Opus** | — | Architecture (final gate) |

---

### Phase 0 — Parsing & types (no behavior change)

> Pure additive. After this phase the binary advertises virtuals and carries the real↔virtual
> map locally, but installs **no** netmap rule yet. Safe to land independently.

#### 0.1 `AdvertiseEntry` + `--advertise` `R@V` parsing
- **Agent:** Haiku (*mechanical, spec-driven* — struct + one string-split fn against the exact spec + test vectors below) → **Sonnet review** (validation edge cases + gates).
- **Files:** `src/shared.rs` (the `AdvertiseEntry` type next to `Ipv4Net`, or a small `mod advertise`); `src/main.rs:1475-1480,1523-1528` (parse path for listen + connect).
- **Change:** parse each comma item into an `AdvertiseEntry`; build `Vec<AdvertiseEntry>`.
  No `@` ⇒ `real==exposed`. Replace the current `Vec<Ipv4Net>` advertise parse on both sides
  with the entry list; derive the wire vector `exposed: Vec<Ipv4Net>` from it. Exact target:
  ```rust
  impl FromStr for AdvertiseEntry {
      type Err = anyhow::Error;
      fn from_str(s: &str) -> anyhow::Result<Self> {
          match s.split_once('@') {
              None => { let n: Ipv4Net = s.parse()?; Ok(Self { real: n, exposed: n }) }
              Some((r, v)) => {
                  ensure!(!v.contains('@'), "advertise '{s}': at most one '@'");
                  let real: Ipv4Net = r.parse().with_context(|| format!("advertise real '{r}'"))?;
                  let exposed: Ipv4Net = v.parse().with_context(|| format!("advertise virtual '{v}'"))?;
                  ensure!(real.prefix == exposed.prefix,
                      "advertise '{s}': real /{} and virtual /{} must have equal prefix length (1:1 netmap)",
                      real.prefix, exposed.prefix);
                  Ok(Self { real, exposed })
              }
          }
      }
  }
  ```
  Then `let entries: Vec<AdvertiseEntry> = args.advertise.iter().map(|s| s.parse()).collect::<Result<_>>()?;`
  and `let exposed: Vec<Ipv4Net> = entries.iter().map(|e| e.exposed).collect();` (the wire vector).
- **Unit tests** (`src/shared.rs` / `src/main.rs` `#[cfg(test)]`):
  - `advertise_parse_plain_no_at` → `real==exposed`, `is_nat()==false`.
  - `advertise_parse_nat_at` → real/exposed distinct, `is_nat()==true`.
  - `advertise_parse_rejects_prefix_mismatch` (`/24@/25` → error).
  - `advertise_parse_rejects_double_at` and `advertise_parse_rejects_bad_cidr`.
  - `advertise_parse_mixed_list` (one plain, one NAT).
- **e2e:** none.
- **Done:** gates green; `--help` text documents the `R@V` form; existing plain `--advertise`
  unit/integration tests unchanged.

#### 0.2 Thread the entry list to the client structs; advertise virtuals on the wire
- **Model:** Sonnet (small but spans listen + connect arg plumbing).
- **Files:** `src/main.rs` (build `Vec<AdvertiseEntry>`), `src/vpn.rs` `VpnListenArgs`/`VpnConnectArgs` (carry `advertise_entries: Vec<AdvertiseEntry>` instead of / alongside the current `Vec<Ipv4Net>`).
- **Change:** where `HelloVpn`/`ConnectVpn` are built, send `entries → exposed` (N3). Keep the
  entries available for the later `NetConfig::apply` call. **No netmap yet.** For plain-only
  configs the serialized vector is byte-identical to today (N8).
- **Unit tests:** `hello_vpn_advertises_exposed_not_real` (NAT entry ⇒ wire carries the
  virtual); `plain_advertise_wire_unchanged` (regression).
- **e2e:** none.
- **Done:** gates green; a NAT'd listener still pairs (server sees only virtuals; netmap not
  installed yet so LAN traffic to the virtual is not yet translated — interim, documented as
  end of Phase 0).

---

### Phase 1 — Netmap rule construction in `NetConfig` (unit-only)

> Add the kernel-rule builders and wire them into `apply`, gated on `nat_maps` being non-empty.
> No client call site passes maps yet (that is Phase 2), so still no end-to-end behavior change.

#### 1.0 Spike: lock the exact nft netmap syntax  ⟵ **Opus review gate**
- **Model:** Sonnet runs the spike; **Opus reviews** the chosen syntax + coexistence reasoning.
- **Files:** none committed except a short note appended to this doc / `VPN_TEST_MATRIX.md`.
- **Change:** on the netns harness kernel (and note the `nft --version`), determine the exact
  working form for a prefix-preserving 1:1 DNAT and SNAT in an `inet` table, verifying with a
  crafted packet that host-bits are preserved (`10.50.1.7 → 192.168.1.7`). Confirm the iptables
  `NETMAP` fallback. Decide: nft-netmap-when-available vs iptables-`NETMAP`-always-for-netmap.
- **Done:** the exact argv forms are written down; Opus sign-off on the ordering (prerouting
  DNAT pri -100, postrouting SNAT pri 100) and the masquerade-scoping plan (N6).

#### 1.1 nft + iptables netmap rule builders
- **Agent:** Haiku (*mechanical* — each builder mirrors an existing `cmd_nft_*`/`cmd_iptables_*` argv) → **Sonnet review** (exact-argv tests + the syntax from 1.0).
- **Files:** `src/vpn.rs` `mod hostcfg_cmd` (next to the existing `cmd_nft_*` / `cmd_iptables_*`).
- **Change:** add, mirroring the existing builders' style and doc comments:
  - `cmd_nft_add_prerouting_chain(id)` — `... pre { type nat hook prerouting priority -100 ; }`.
  - `cmd_nft_add_netmap_dnat(id, tun, exposed, real)` — ingress `iif <tun> ip daddr <exposed> dnat ip to <real>`.
  - `cmd_nft_add_netmap_snat(id, tun, real, exposed)` — egress `oif <tun> ip saddr <real> snat ip to <exposed>` (postrouting chain, reused).
  - `cmd_nft_add_masquerade_scoped(id, tun, lan_if, plain_subnet)` — `iif <tun> oif <lan_if> ip daddr <plain_subnet> masquerade`.
  - iptables: `cmd_iptables_netmap_dnat_add/_del`, `cmd_iptables_netmap_snat_add/_del` (`-j NETMAP --to`), and `cmd_iptables_masquerade_scoped_add/_del` (`-d <plain_subnet>`).
  - use the syntax locked in 1.0.
- **Unit tests:** exact-argv assertions for each builder (the codebase tests builders this way),
  including prefix rendering (`Ipv4Net::to_string`).
- **e2e:** none.
- **Done:** gates green.

#### 1.2 Integrate into `NetConfig::apply`  ⟵ **Opus review gate**
- **Model:** Opus design review (rule ordering + masquerade/netmap coexistence + revert/reclaim correctness), then Sonnet implements.
- **Files:** `src/vpn.rs` `hostcfg::NetConfig::apply` (`2546-2724`), `Drop` (`2727`), `stale_reclaim` (`2369`).
- **Change:** add parameter `nat_maps: &[(Ipv4Net /*real*/, Ipv4Net /*exposed*/)]`. Redefine the
  `advertised` argument as the **real** subnets (N7) — callers already pass "this side's
  advertised subnets"; they will now pass reals (Phase 2). Within the gateway-mode block:
  - LAN-iface detection (`ip route get advertised[0].network()+1`, `src/vpn.rs:2627-2638`)
    already uses `advertised` → now correctly targets a real host (N7). No code change beyond
    the semantic shift; **assert** in tests that a virtual is never used here.
  - **If `nat_maps` is empty:** unchanged — blanket masquerade, no prerouting chain (N8).
  - **If `nat_maps` is non-empty (nft path):** add the prerouting chain once; for each map add
    DNAT (pre) + SNAT (post); for each **plain** advertised subnet (a real subnet not in
    `nat_maps`) add a scoped masquerade instead of the blanket rule; MSS clamp + spoke isolation
    unchanged.
  - **iptables path:** mirror with `NETMAP` + scoped masquerade; push per-rule deletes to
    `revert_cmds`/`revert_labels` (the nft path is covered by the single `nft delete table`).
  - **`--no-route-manage` print block** (`src/vpn.rs:2709-2721`): print the netmap + scoped
    masquerade commands verbatim too.
  - **`stale_reclaim`** (`src/vpn.rs:2369-2417`): nft path already deletes the whole table
    (covers netmap). For the iptables path, add deletes for the `NETMAP` PRE/POST rules and the
    scoped masquerade (mirror the BUG-3 masquerade/MSS reclaim).
- **Shape of the nft gateway branch** (drop-in for the block at `src/vpn.rs:2643-2674`; the
  `else`/iptables branch mirrors it):
  ```text
  add table; add postrouting chain;                       // existing
  if nat_maps.is_empty() {
      add masquerade_rule(tun, lan_if);                    // existing blanket rule (N8: unchanged path)
  } else {
      add prerouting_chain;                                // NEW (pri -100)
      for (real, exposed) in nat_maps {
          add netmap_dnat(tun, exposed, real);             // pre:  iif tun ip daddr exposed dnat to real
          add netmap_snat(tun, real, exposed);             // post: oif tun ip saddr real snat to exposed
          info!(.., "nat netmap: ...");                    // §5.1(2)
      }
      for plain in advertised.iter().filter(|p| !nat_maps.iter().any(|(r,_)| r == *p)) {
          add masquerade_scoped(tun, lan_if, plain);       // post: iif tun oif lan_if ip daddr plain masquerade
      }
  }
  add forward_chain; add mss_clamp; if hub { add spoke_isolation }   // existing, unchanged
  ```
  Ordering rationale (Opus to confirm in review): DNAT in prerouting (pri -100) runs before the
  routing decision; SNAT/masquerade in postrouting (pri 100) after. Netmap and masquerade never
  both touch the same subnet (the `filter` excludes NAT'd reals).
- **Unit tests** (fake `CommandRunner`, the harness already used by `tests/vpn_server_test.rs`):
  - `apply_plain_only_unchanged` — no `nat_maps` ⇒ exact today's command list + revert (regression, N8).
  - `apply_nat_only_emits_prerouting_dnat_snat_no_masquerade` — one map, no plain subnet ⇒
    prerouting chain + DNAT + SNAT + MSS, **no** masquerade rule.
  - `apply_mixed_scopes_masquerade_to_plain_only` — one NAT + one plain ⇒ netmap for the NAT
    subnet + scoped masquerade for the plain subnet, no blanket masquerade.
  - `apply_lan_iface_detection_uses_real_subnet` — `route get` argv targets the real host.
  - `apply_nat_revert_mirrors_apply` (nft: single table delete; iptables: every rule deleted).
- **e2e:** in Phase 2.
- **Done:** gates green; the plain-only command list is asserted byte-identical to current.

---

### Phase 2 — Client wire-up + site↔host / site↔site e2e

> Pass reals + maps from both client roles into `apply`; prove the §1 identical-LAN scenario
> end to end on relay and direct.

#### 2.1 Listener & connector pass reals + `nat_maps` to `apply`
- **Model:** Sonnet.
- **Files:** `src/vpn.rs` listener setup (`apply` call near `src/vpn.rs:477`) and connector
  `run_connect_once` (`apply` call near `src/vpn.rs:1207-1219`).
- **Change:** from the carried `Vec<AdvertiseEntry>` build `advertised_real:
  Vec<Ipv4Net> = entries.map(real)` and `nat_maps: Vec<(real, exposed)> =
  entries.filter(is_nat).map(|e| (e.real, e.exposed))`; pass both into `apply`. The
  `peer_routes` argument is unchanged (still the filtered peer virtuals from
  `filter_accepted`). Log the resolved maps at `info`.
- **Unit tests:** `connector_passes_real_and_maps_to_netconfig` and
  `listener_passes_real_and_maps_to_netconfig` (mock `apply`/runner; assert reals + maps).
- **e2e:** in 2.2.
- **Done:** gates green.

#### 2.2 Site↔host + site↔site e2e (the killer test)  ⟵ **Opus review of assertions**
- **Model:** Sonnet (harness); **Opus** reviews the topology + assertions (this is the acceptance core).
- **Files:** `scripts/vpn_netns_test.sh`. Build first: `cargo build --release --features vpn`
  (as user, not root — the harness refuses a stale binary). Add a topology with **two LANs that
  share the same real CIDR** `192.168.1.0/24` (one behind each gateway ns), plus the existing
  server ns.
- **e2e tests (run under BOTH `--relay-only` and direct):**
  - **T-NAT1 (site↔host, B):** gateway `--advertise 192.168.1.0/24@10.50.1.0/24`; roaming
    client `--accept-all-routes`. Client `ping 10.50.1.10` reaches the real `192.168.1.10`;
    the LAN host sees the client as the client's overlay IP (no SNAT on client source). Route
    table on the client shows `10.50.1.0/24 dev bore0`, **not** `192.168.1.0/24`.
  - **T-NAT2 (site↔site identical LANs, C):** A `--advertise 192.168.1.0/24@10.50.1.0/24`,
    B `--advertise 192.168.1.0/24@10.60.1.0/24 --accept-all-routes`. From A: `ping 10.60.1.10`
    → reply from B's real `.10`; from B: `ping 10.50.1.5` → reply from A's real `.5`. Assert
    B's host sees the caller as `10.50.1.5` (tcpdump/`conntrack`-free; netmap preserves host bits).
  - **T-NAT3 (host-bit preservation):** `ping 10.60.1.23` ↔ real `.23` for a couple of hosts
    (confirms 1:1 netmap, not a single-address DNAT).
  - **T-NAT4 (mixed plain+NAT):** gateway advertises `192.168.1.0/24@10.50.1.0/24,172.16.9.0/24`;
    peer reaches the NAT'd LAN via `10.50.1.x` and the plain LAN via `172.16.9.x` (plain still
    masqueraded). Confirms N6 scoping.
  - **T-NAT5 (cleanup):** after graceful exit, `nft list tables` / `iptables -t nat -S` /
    `ip route` are identical to before (RAII revert covers netmap + prerouting chain).
- **Done:** all T-NAT* pass on relay and direct; existing site↔host / site↔site tests
  (plain, no `@`) still pass unchanged (N8).

---

### Phase 3 — Multi-client hub NAT (Topology D)

> The hub advertises `R@V`; spokes route `V` per policy. Netmap is peer-agnostic, so this is
> mostly an integration + e2e phase on top of the existing hub data plane.

#### 3.1 Hub `apply(hub=true)` carries reals + maps; coexistence with isolation
- **Model:** Sonnet.
- **Files:** `src/vpn.rs` `mod hub` (the hub `NetConfig::apply` call site).
- **Change:** build `advertised_real` + `nat_maps` from the hub's `AdvertiseEntry` list (Phase
  2.1 logic) and pass to `apply(.., hub=true)`. Verify the netmap (nat hooks) and spoke
  isolation (forward hook, `cmd_nft_add_spoke_isolation`, `src/vpn.rs:1980`) live in different
  chains and don't interfere. Confirm the hub router (`run_router_uplink`) still keys replies on
  the spoke overlay (the egress SNAT `saddr R` never matches a spoke overlay source — N9).
- **Unit tests:** `hub_apply_emits_netmap_and_isolation` (both present, independent);
  `hub_apply_plain_only_unchanged` (regression).
- **e2e:** in 3.2.
- **Done:** gates green.

#### 3.2 Hub NAT e2e
- **Model:** Sonnet (harness).
- **Files:** `scripts/vpn_netns_test.sh` (reuse the hub topology; give the hub a real LAN
  `192.168.1.0/24` exposed as `10.50.1.0/24`).
- **e2e tests (relay and direct):**
  - **T-HUBNAT1:** hub `--advertise 192.168.1.0/24@10.50.1.0/24 --max-clients 4`; two spokes
    `--accept-all-routes` reach the real LAN via `10.50.1.x`; host-bit preserved per spoke.
  - **T-HUBNAT2 (isolation intact):** spoke↔spoke still dropped; LAN↔spoke (the gateway path)
    works through netmap.
  - **T-HUBNAT3 (per-path):** one spoke direct, one relay — both reach the NAT'd LAN identically.
- **Done:** all T-HUBNAT* pass; existing hub tests (T-HUB*, T-HUBD*, T-SCEN-*) unchanged.

---

### Phase 4 — Hardening, server checks, admin, docs, bench

#### 4.1 Server-side sanity (virtuals only)
- **Model:** Sonnet.
- **Files:** `src/vpn_server.rs`.
- **Change:** confirm `check_overlap` operates on the advertised **virtuals** (no code change
  expected — it already does); add a defensive assertion/test that real subnets never reach the
  server (they are not in any message). If two sides advertise **overlapping virtuals**, the
  existing rejection fires with the existing message — add a test pinning that and a doc note
  that virtuals must be chosen distinct.
- **Unit tests:** `server_rejects_overlapping_virtuals`; `server_accepts_overlapping_reals_via_distinct_virtuals` (both sides real `192.168.1.0/24`, distinct virtuals ⇒ paired, no `VpnError`).
- **Done:** gates green.

#### 4.2 `--no-route-manage` prints netmap commands
- **Model:** Haiku (extend the print block from 1.2).
- **Files:** `src/vpn.rs:2709-2721`.
- **Unit tests:** `no_route_manage_prints_netmap_and_scoped_masquerade` (capture stdout / assert
  via the runner stub).
- **Done:** gates green; one manual matrix entry (apply printed commands by hand, verify reach).

#### 4.3 SIGKILL stale reclaim for netmap
- **Model:** Sonnet.
- **Files:** `src/vpn.rs` `stale_reclaim` (`2369-2417`).
- **Change:** ensure the iptables path reclaim deletes the `NETMAP` PRE/POST rules and scoped
  masquerade (nft path already covered by table delete).
- **e2e:** **T-NATKILL:** SIGKILL a NAT gateway, re-run same `--id` → clean reclaim, no leaked
  `nat` rules, `ip_forward` restored.
- **Done:** gates green.

#### 4.4 Admin visibility (optional, local)
- **Model:** Sonnet.
- **Files:** `src/admin*.rs`, `src/admin_status.html`.
- **Change:** on a gateway link, show its real→exposed maps (local config; no protocol change).
  Optionally add the deferred `#[serde(default)]` admin field (§5) if cross-host visibility is
  desired — decide during implementation; default to local-only.
- **Unit tests:** extend the admin entry test to include a NAT mapping field.
- **Done:** gates green.

#### 4.5 Benchmark (guardrail)
- **Model:** Sonnet.
- **Change:** NAT is kernel-side and stateless, so bore's per-packet cost is unchanged; assert
  throughput/latency parity vs a plain gateway link (relay and direct) — the netmap must not
  regress the data path (it shouldn't; bore never touches the packet). Record numbers.
- **Done:** bench recorded in `docs/vpn/VPN_TEST_MATRIX.md`.

#### 4.6 Docs
- **Model:** Haiku (mechanical edits) + Opus final read.
- **Files:** `docs/vpn/VPN.md` (replace the §"Overlapping Subnets" v1-limitation text with the
  NAT feature: syntax, mechanism, per-host preservation, prefix-equality constraint),
  `docs/vpn/VPN_USER_FULL_GUIDE.md` (a NAT section + the `R@V` flag table row),
  `docs/vpn/VPN_TEST_MATRIX.md` (T-NAT*/T-HUBNAT*/T-NATKILL coverage), `CLAUDE.md` (the I-NAT*
  invariants below), and mark **E3 done** in `docs/vpn/VPN_FULL_PLAN_TODO.md:254` /
  `docs/vpn/VPN.md:330`.
- **Done:** docs build/read clean; Opus sign-off.

---

### Phase 5 — Extended real-world scenario validation

> Beyond the per-feature e2e of Phases 2–3, this phase proves NAT under **realistic, layered**
> conditions: multiple protocols, bidirectional service traffic, mixed meshes, path churn,
> resilience, and the documented limits. Most run in the netns harness; a few are manual
> two-host matrix entries (clearly marked). Each automated scenario also **greps the client log**
> for the §5.1 lines (logging is a tested artifact here).

#### 5.1 Scenario matrix design  ⟵ **Opus**
- **Agent:** Opus (*architecture* — defines the scenario set, the assertions, and what is
  netns-automatable vs manual). Sonnet implements 5.2–5.4 from it.
- **Output:** the table below, frozen into `docs/vpn/VPN_TEST_MATRIX.md` as the NAT real-world
  block, each row given a stable ID (`S-NAT-*`). Opus signs off that the set covers: both data
  paths, all three topologies, multi-protocol, identity preservation, mixed plain+NAT, resilience,
  and the ALG limit.

| ID | Topology | Setup | Path | Assertions |
|----|----------|-------|------|-----------|
| **S-NAT-1** | C site↔site, identical LANs | A `192.168.1.0/24@10.50.1.0/24`, B `192.168.1.0/24@10.60.1.0/24` | relay + direct | TCP (`curl`/`iperf3`), UDP (`iperf3 -u`), ICMP all succeed both directions; B-host sees caller as `10.50.1.5` (tcpdump); host-bit preserved for ≥2 hosts |
| **S-NAT-2** | B site↔host | gateway `192.168.1.0/24@10.50.1.0/24`, roaming `--accept-all-routes` | relay + direct | SSH + HTTP + DNS to `10.50.1.x` reach real hosts; LAN host sees client overlay IP (no client SNAT) |
| **S-NAT-3** | D hub, mixed mesh | hub `--advertise 192.168.1.0/24@10.50.1.0/24,172.16.9.0/24 --max-clients 4`; spoke-X reaches NAT'd LAN, spoke-Y reaches plain LAN, spoke-Z host-only | mixed (1 direct, 1 relay, 1 either) | each spoke reaches exactly its policy set; NAT'd via `10.50.1.x`, plain via `172.16.9.x`; spoke↔spoke dropped; host-bit preserved |
| **S-NAT-4** | C, asymmetric | A overlaps + maps `192.168.1.0/24@10.50.1.0/24`; B non-overlapping plain `10.77.0.0/24` | relay + direct | A reaches B's plain LAN; B reaches A's real LAN via `10.50.1.x`; confirms one-sided NAT is valid |
| **S-NAT-5** | C, resilience: reconnect | S-NAT-1 + kill server mid-session, `--auto-reconnect` | relay→direct | after reconnect, netmap rules re-applied, traffic resumes, no leaked/duplicate nft rules |
| **S-NAT-6** | C, resilience: path churn | S-NAT-1 direct, drop UDP mid-session → fallback to warm relay → restore | direct↔relay | NAT unaffected by path switch (kernel-side); traffic continues; TUN/rules untouched |
| **S-NAT-7** | C, resilience: SIGKILL | S-NAT-1, `kill -9` a gateway, re-run same `--id` | — | stale reclaim removes all netmap rules + restores `ip_forward`; no `EEXIST` on restart |
| **S-NAT-8** | C, scale/MTU/carriers | S-NAT-1 with `--carriers 4` (relay) and full-MTU flows; many parallel `iperf3` streams | relay + direct | throughput parity vs plain gateway (NAT is conntrack-free → no table pressure); no large-packet blackhole (MSS clamp holds) |
| **S-NAT-9** *(manual)* | C, two real hosts | two real machines, both real LAN `192.168.1.0/24`, distinct virtuals | direct (real NAT traversal) | end-to-end reach across real internet; documents the real deployment recipe |
| **S-NAT-10** | C, ALG limit (negative/doc) | S-NAT-1 + an app that embeds its own IP (FTP active mode / SIP) | relay + direct | document that embedded-IP payloads are **not** translated (no ALG); ICMP/TCP/UDP/HTTP/SSH fine. Asserts the limitation is real + documented, not a regression |

#### 5.2 Multi-protocol + bidirectional services (S-NAT-1, S-NAT-2)
- **Agent:** Sonnet (*feature* — harness extension).
- **Files:** `scripts/vpn_netns_test.sh`. Add real-ish services in the LAN ns (a tiny HTTP
  responder via `python3 -m http.server` or `nc`, an SSH-style TCP echo, a UDP echo) and assert
  TCP/UDP/ICMP all cross both directions, on relay and direct. Capture with `tcpdump` in the LAN
  ns to assert the **observed source** equals the peer's virtual (identity preservation).
- **Done:** S-NAT-1/2 pass on both paths; log greps for `nat netmap: dnat`/`snat` + `vpn link
  route summary` succeed.

#### 5.3 Mixed mesh via hub (S-NAT-3, S-NAT-4)
- **Agent:** Sonnet (*feature* — harness).
- **Files:** `scripts/vpn_netns_test.sh` (extend the hub topology with a NAT'd LAN + a plain LAN +
  a host-only spoke; add the asymmetric C case).
- **Done:** S-NAT-3/4 pass; per-spoke route tables match `filter_accepted`; isolation intact.

#### 5.4 Resilience under NAT (S-NAT-5..8)
- **Agent:** Sonnet (*feature* — harness; reuse existing reconnect/SIGKILL/path-churn/iperf helpers).
- **Files:** `scripts/vpn_netns_test.sh`.
- **Done:** S-NAT-5..8 pass; after each, `nft list ruleset`/`iptables -t nat -S`/`ip route`/
  `ip_forward` return to baseline (clean revert + reclaim); throughput within tolerance of a plain
  gateway baseline.

#### 5.5 ALG limits doc + manual two-host matrix (S-NAT-9, S-NAT-10)
- **Agent:** Haiku (*mechanical* — doc + matrix rows) → **Sonnet review**.
- **Files:** `docs/vpn/VPN_USER_FULL_GUIDE.md` (a "Overlapping subnets / 1:1 NAT" section + the
  real two-host recipe), `docs/vpn/VPN_TEST_MATRIX.md` (S-NAT-9/10 as manual entries).
- **Change:** document the **no-ALG** limitation (1:1 netmap does not rewrite IPs embedded in
  application payloads — FTP active, SIP, some RPC); recommend passive/IP-agnostic protocols or
  app-layer config. Provide the manual two-host test recipe (S-NAT-9).
- **Done:** docs read clean; matrix updated.

#### 5.6 Acceptance sign-off  ⟵ **Opus**
- **Agent:** Opus (*architecture* — final gate).
- **Change:** confirm every S-NAT-* automated row is green on **both** relay and direct, the
  §1 identical-LAN scenario holds, logging lines are present and accurate, all invariants
  (§7) hold, and the no-NAT path is byte-identical (I-NAT1). Record the result + bench numbers in
  `docs/vpn/VPN_TEST_MATRIX.md`. This gate is the equivalent of the multi-client `107/0` netns run.
- **Done:** signed-off acceptance recorded; E3 marked done in `VPN_FULL_PLAN_TODO.md` + `VPN.md`.

---

## 7. Invariants to preserve / add

- **I-NAT1:** No `@` in any advertise entry ⇒ `NetConfig::apply` is **byte-for-byte** today's
  path (blanket masquerade, no prerouting chain, no netmap). Zero-regression (mirrors I-MC1).
- **I-NAT2:** Only the **exposed (virtual)** CIDR is ever serialized (`HelloVpn`/`ConnectVpn`/
  `VpnReady`/`VpnPeerJoin`). Real subnets are gateway-local and never leave the host. Wire stays
  compatible; a NAT client interoperates with an unmodified server.
- **I-NAT3:** Netmap is **stateless 1:1, host-bits preserved**; real & exposed have equal prefix
  length (validated at parse, N4). No conntrack dependency for NAT'd traffic.
- **I-NAT4:** Each gateway netmaps **only its own** real↔exposed; no per-peer or global NAT
  state. Identical on relay and direct (kernel-side, link-agnostic).
- **I-NAT5:** NAT'd subnets are **never masqueraded** (the arriving source is already a peer
  virtual). When NAT is present, masquerade is scoped to plain subnets by destination (N6).
- **I-NAT6:** Server overlap check operates on **virtuals**; real subnets may overlap freely
  across sites (the feature's purpose).
- **I-NAT7:** `NetConfig` RAII reverts netmap rules + the prerouting chain on SIGINT/SIGTERM/
  panic; SIGKILL recovered via `stale_reclaim` (nft: table delete; iptables: explicit NETMAP +
  scoped-masquerade deletes).
- **I-NAT8:** The bore data plane is **unchanged** — IP packets stay opaque, no header rewrite
  in Rust; all NAT is kernel nft/iptables (reaffirms the existing invariant).
- **I-NAT9:** LAN-egress iface detection + `ip_forward` enablement use the **real** subnet (the
  virtual has no local route).
- **I-NAT10 (observability):** every link logs, at `info`, its advertise entries (real→exposed),
  the installed NAT rules, the resolved/installed peer routes, and a single canonical route-table
  summary (§5.1). A user reading the log understands the full route + NAT picture without
  guesswork; e2e tests grep these lines.

## 8. Risk register

| Risk | Mitigation |
|------|-----------|
| Exact nft netmap syntax is version-fragile | **Spike 1.0** locks it on the target kernel before any builder; iptables `NETMAP` is the stable fallback. |
| nft (masquerade) + iptables (NETMAP) on one host both hooking `nat` could interact | Prefer **all-nft** when nft is available (netmap in the same `bore_vpn_<id>` table). Only fall back to iptables-`NETMAP` if 1.0 shows nft netmap unusable; if mixed, document priority ordering and add a coexistence e2e. |
| Blanket masquerade wrongly NATs NAT'd traffic | N6: replace with per-plain-subnet scoped masquerade whenever any NAT entry exists; unit `apply_nat_only_*` asserts no blanket rule. |
| LAN-iface detection uses a virtual (no route) | N7/I-NAT9: `route get` targets a **real** host; unit `apply_lan_iface_detection_uses_real_subnet`. |
| Spoke/client source collides with the remote LAN | Client/spoke source is its unique overlay address (never in any real `R`); the egress SNAT `saddr R` cannot match it. Documented; covered by T-NAT1/T-HUBNAT1. |
| User picks overlapping **virtuals** | Server's existing `check_overlap` rejects with the existing message; test `server_rejects_overlapping_virtuals`; doc note. |
| Prefix mismatch (`/24@/25`) | Hard parse error (N4); unit `advertise_parse_rejects_prefix_mismatch`. |
| Asymmetric config (one site maps, the other forgot) | Each gateway is independent; if only one side overlaps it still works; if both overlap and one didn't map, routing is ambiguous — documented as user error, surfaced by the server overlap rejection when the un-mapped real collides with the overlay/peer virtual. |
| Many-real→one-virtual expectation (PAT) | Out of scope (stateful); N4 forbids unequal prefixes; documented. |

## 9. Verification summary

- **Gates (every sub-phase):** `cargo fmt --check`, `cargo clippy --features vpn -- -D warnings`,
  `cargo test --features vpn`.
- **Unit tests:** `src/shared.rs`/`src/main.rs` (advertise parse, wire = virtuals), `hostcfg_cmd`
  builder argv tests, `NetConfig::apply` fake-runner tests (plain-unchanged / nat-only / mixed /
  revert / lan-iface), `tests/vpn_server_test.rs` (overlapping-reals-via-distinct-virtuals pairs).
- **e2e:** rebuild `cargo build --release --features vpn` (as user) then
  `sudo -n /abs/path/scripts/vpn_netns_test.sh`, extended with a two-identical-LAN topology
  (`192.168.1.0/24` behind each gateway) and a hub NAT case. The harness refuses a stale
  release binary — always rebuild first.
- **Extended real-world (Phase 5):** the `S-NAT-*` matrix — multi-protocol (TCP/UDP/ICMP/HTTP/
  SSH/DNS), bidirectional services, mixed plain+NAT mesh via hub, asymmetric NAT, reconnect,
  path churn, SIGKILL reclaim, carriers/MTU scale, and the documented no-ALG limit. Each
  automated row runs on **both** paths and **greps the client log** for the §5.1 lines, so
  logging is a tested artifact.
- **Acceptance:** the §1 identical-LAN site↔site scenario (T-NAT2/3, S-NAT-1) passes on **both
  relay and direct**, plus T-NAT1 (site↔host) and T-HUBNAT1-3 (hub), with full RAII cleanup
  (T-NAT5), SIGKILL reclaim (T-NATKILL / S-NAT-7), and the Opus 5.6 sign-off recorded in
  `VPN_TEST_MATRIX.md`.

## 10. Model-assignment summary

| Phase | Sub-phases (primary) | Opus review gate |
|-------|----------------------|------------------|
| 0 | 0.1 Haiku→Sonnet review · 0.2 Sonnet | — |
| 1 | 1.0 Sonnet(run) · 1.1 Haiku→Sonnet review · 1.2 Sonnet | **1.0** (nft netmap syntax) · **1.2** (rule ordering + masquerade coexistence + revert) |
| 2 | 2.1 Sonnet · 2.2 Sonnet (harness) | **2.2** (acceptance assertions) |
| 3 | 3.1, 3.2 Sonnet | — |
| 4 | 4.1, 4.3, 4.4, 4.5 Sonnet · 4.2 Haiku→Sonnet review · 4.6 Haiku | **4.6** (final docs read) |
| 5 | 5.1 **Opus** · 5.2–5.4 Sonnet · 5.5 Haiku→Sonnet review · 5.6 **Opus** | **5.1** (scenario matrix) · **5.6** (acceptance sign-off) |

> Rule of thumb (CLAUDE.md): start Sonnet, drop to Haiku for mechanical/boilerplate sub-phases
> (advertise string split, clap text, doc edits, `--no-route-manage` print), escalate to Opus
> only for the gates marked above (the nft-netmap spike, the `apply` rule-coexistence design,
> and acceptance assertions). Print the model used per sub-task during implementation.
