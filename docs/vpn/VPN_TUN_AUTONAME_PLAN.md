# VPN TUN auto-naming + non-destructive reclaim

## Problem (reported bug)

Topology: ≥2 `bore vpn listen` nodes (N=`listen-pippo`, K=`listen-pluto`).
4 connectors: A→N, B→N, C→K, D→K. **A and C share one physical host.**

Both A and C default to TUN name `bore0`. On one host two interfaces cannot share a
name, so they collide. User expectation: `A→N` gets `bore0`, `C→K` gets `bore1`, and an
arbitrary number of `listen`/`connect` instances coexist on one host with correct routing.

## Root causes (verified in `src/vpn.rs`)

### RC1 — destructive `stale_reclaim` (severe)
`hostcfg::stale_reclaim` (vpn.rs:2369) ends with an **unconditional**
`ip link del <tun_name>` (vpn.rs:2417). The TUN is created **non-persistent**
(`create_tun`, no `.persist()`), so the kernel auto-removes it when the owning process
dies. Therefore this line never reclaims a real leftover — it only ever fires when a
*live* interface of that name exists, i.e. a co-located instance. Result: C's startup
`stale_reclaim("bore0")` **deletes A's live `bore0`**, killing A's link.

Co-located hub spokes share the link `--id` (connect `--id` = the listener/hub id), so an
id-keyed guard cannot fix this. The line must go.

### RC2 — hardcoded default name collision
`--tun-name` default = `"bore0"` for both `vpn listen` and `vpn connect`
(main.rs:911, main.rs:1045). The 2nd co-located `create_tun(.name("bore0"))` fails EEXIST,
and routes `... dev bore0` are ambiguous between the two.

## Frozen spec

1. **Default `--tun-name` `"bore0"` → `"auto"`** (both subcommands). Help text updated.
2. **`"auto"`** → pick the first free `boreN` (N=0,1,2,…) by checking `/sys/class/net/`,
   race-safe via a create-retry loop (on create error bump N, cap 0..=255).
   A single instance still gets `bore0` (backward-compatible common case).
3. **Explicit `--tun-name NAME`** → used verbatim, behavior unchanged (user owns collisions;
   EEXIST surfaces as a normal create error — no longer silently destroys a live iface).
4. **Resolved name threaded** into `NetConfig::apply` (routes/nft) + logs + the RAII revert,
   so routing references the real interface, not the literal `"auto"`.
5. **Remove the unconditional `ip link del <tun_name>`** in `stale_reclaim`. nft / iptables /
   ip_forward reclaim (id-keyed, for resources that DO persist past SIGKILL) stays.

### Invariants preserved
- I-9 / I-MC1: explicit `--tun-name bore0` + single instance = byte/path-identical. Auto
  resolves to `bore0` when free; explicit is verbatim.
- 1:1 vs hub branch structure untouched; fix lives in `hostcfg::create_tun` +
  `hostcfg::stale_reclaim` + the 3 call sites (listen 463, connect 1176, hub 5310).

### API change
`create_tun(name, addr, prefix, mtu, queues)` resolves `"auto"` and returns
`(Vec<AsyncDevice>, bool, String)` — the trailing `String` is the chosen name. The 3 call
sites capture it and use it for `NetConfig::apply`, logs, and downstream refs.

Name picker factored as a pure fn for unit testing:
`fn pick_tun_name(requested: &str, exists: impl Fn(&str) -> bool) -> Option<String>`.

## Tests
- Unit: `pick_tun_name` — explicit passthrough; auto skips occupied names; auto returns
  `bore0` when none exist; exhaustion → `None`.
- e2e (netns, `vpn_netns_test.sh`): two connectors on ONE host to two different listeners;
  assert two distinct ifaces (`bore0`,`bore1`) both up, traffic flows on both, killing one
  link leaves the other's iface + traffic intact (RC1 regression guard).

## Docs to update
README.md, USER_GUIDE.md, docs/vpn/VPN_USER_FULL_GUIDE.md — document `--tun-name auto`
default, the multi-instance-per-host story, and that co-located instances no longer collide.
