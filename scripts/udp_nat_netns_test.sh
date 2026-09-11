#!/usr/bin/env bash
# UDP NAT-traversal netns smoke — plan Fase 0 (real kernel NAT, full binaries)
# Must be invoked directly with sudo (not via 'sudo bash ...') per sudoers setup.
#
# Complements the deterministic userspace NAT lab (tests/nat_traversal_test.rs):
# the lab covers the RFC 4787 profile matrix in-process; this script proves the
# SAME end-to-end flow (bore server + provider + consumer binaries) across two
# REAL netfilter NATs, with routing, conntrack and ICMP in the path.
#
# Topology (double NAT):
#   nsprov(10.1.0.2) ─ nsnat1{10.1.0.1 | 192.0.2.1 masq} ─┐
#                                                          ns0 "internet"
#   nscli (10.2.0.2) ─ nsnat2{10.2.0.1 | 192.0.3.1 masq} ─┘ (bore server --udp)
#
# What this proves that the in-process lab cannot: the RFC 4787 profile of a
# real Linux router is produced by conntrack, not by a policy enum, and the
# two axes are controlled by different mechanisms:
#
#   MAPPING    `masquerade`               -> endpoint-INDEPENDENT (EIM): the
#                                            same internal port is reused for
#                                            every destination when it is free
#              `masquerade fully-random`  -> endpoint-DEPENDENT (EDM, the
#                                            "symmetric" NAT): a fresh random
#                                            source port per flow
#
#   FILTERING  conntrack alone            -> address+port dependent (APDF):
#                                            only the exact reply tuple gets
#                                            back in. The typical home router.
#              + dnat from a punched-addr -> address dependent (ADF): any port
#                set                        of a peer we have sent to.
#              + unconditional dnat       -> endpoint independent (EIF), the
#                                            "full cone" / static port-forward.
#
# The filtering axis needs an explicit `dnat` because a masquerading router
# has no other way to know WHICH inside host an unsolicited datagram belongs
# to — which is also exactly why, in the field, "open the port" and "full
# cone" are the same sentence. The inside peer therefore runs with
# `--nat-udp-preferred-port` so the forward has a fixed target.
#
# Scenarios (provider x consumer; the DOC's §6 matrix is the oracle):
#   T-NAT-DIRECT           EIM+APDF x EIM+APDF  -> DIRECT (crossfire punch)
#   T-NAT-RANDOM-RELAY     EDM      x EDM       -> RELAY
#   T-NAT-BLOCKED-RELAY    UDP egress dropped   -> RELAY
#   T-NAT-APDF-VS-EDM      EIM+APDF x EDM       -> RELAY  (the classic
#                          "port-restricted home provider cannot serve a
#                           mobile/symmetric consumer" cell)
#   T-NAT-ADF-VS-EDM       EIM+ADF  x EDM       -> DIRECT (one filtering step
#                          looser flips the SAME cell: this is the rule the
#                          in-process lab extracted and the reason §13's
#                          "test-udp reports the mapping, not the filtering"
#                          is a real gap and not a cosmetic one)
#   T-NAT-EIF-VS-EDM       EIM+EIF  x EDM       -> DIRECT (a port-forwarded
#                          provider serves any consumer with UDP egress)
#
# Usage: sudo scripts/udp_nat_netns_test.sh [scenario ...]
#        BORE_NAT_CASES="T-NAT-ADF-VS-EDM" sudo -E scripts/udp_nat_netns_test.sh
# Exit code: 0 = all tests passed, nonzero = failures

set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"

# ── Guards ──────────────────────────────────────────────────────────────────
if [ ! -x "$BORE" ]; then
    echo "ERROR: $BORE not found. Build first (as your user, NOT root):" >&2
    echo "  cargo build --release" >&2
    exit 1
fi
if find "$(dirname "$0")/../src" "$(dirname "$0")/../Cargo.toml" \
        -newer "$BORE" -print -quit 2>/dev/null | grep -q .; then
    echo "ERROR: $BORE is OLDER than the sources — stale build." >&2
    echo "  Rebuild (as your user, NOT root):  cargo build --release" >&2
    exit 1
fi

# The Fase 7 escape opens a few hundred auxiliary UDP sockets on the symmetric
# side, and it declines rather than starve a live process of descriptors. Raise
# the SOFT limit only (`-Sn`): `ulimit -n N` sets BOTH, and a hard limit below
# the current soft one is refused — the exact trap `T-PUB-FDBUDGET` documents.
ulimit -Sn 8192 2>/dev/null || true

for cmd in ip nft nc socat; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "SKIP: $cmd not installed" >&2
        exit 0
    fi
done

# ── Configuration ───────────────────────────────────────────────────────────
SECRET="natsmoke$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
SERVER_IP="192.0.2.100"        # ns0 side of the nsnat1 link (server bind)
SERVER_IP2="192.0.3.100"       # ns0 side of the nsnat2 link
CTRL_PORT="7835"
ECHO_PORT="9111"
PROXY_PORT="9555"
# Fixed UDP ports for the sides whose router forwards a port (ADF/EIF). They
# must differ so the two routers' rules can never be confused in a log.
# Per-cell UDP source ports, NOT one fixed pair for the whole matrix.
#
# This is isolation, not cosmetics. A conntrack entry is keyed by the 5-tuple,
# and a nat chain is traversed only by the FIRST packet of a flow: with one
# fixed port, cell N+1's STUN flow (10.1.0.2:PORT -> server:7835) reuses cell
# N's entry, so the router's freshly installed rules never run — the
# `@punched` set stays empty and the `update` rule never fires. MEASURED: run
# alone, T-NAT-FILTER-ADF reads `adf-or-eif`; run straight after
# T-NAT-FILTER-APDF on the same port, it read `apdf`, i.e. the harness
# reported the PREVIOUS cell's router. `flush_conntrack` was supposed to
# prevent exactly this and could not: the `conntrack` tool is not installed on
# every box and the flush failed silently (see below).
#
# The sequence number is bumped for EVERY cell, before the case filter, so a
# cell's ports are a property of its position in this file and one cell
# re-run in isolation uses the same ports it uses in a full run.
PROV_UDP_PORT_BASE="41641"
CONS_UDP_PORT_BASE="41741"
CELL_SEQ=0
PROV_UDP_PORT="$PROV_UDP_PORT_BASE"
CONS_UDP_PORT="$CONS_UDP_PORT_BASE"
cell_ports() {
    CELL_SEQ=$((CELL_SEQ + 1))
    PROV_UDP_PORT=$((PROV_UDP_PORT_BASE + CELL_SEQ))
    CONS_UDP_PORT=$((CONS_UDP_PORT_BASE + CELL_SEQ))
}
# Optional case filter: positional arguments first, then BORE_NAT_CASES.
CASES="${*:-${BORE_NAT_CASES:-}}"
# Fixed dir, wiped at START (not exit) so a failed run leaves its logs behind.
TMPDIR="/tmp/bore_udpnat_last"
PASS=0
FAIL=0

pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
die()  { echo "ERROR: $*" >&2; exit 1; }

# ── Cleanup ─────────────────────────────────────────────────────────────────
cleanup() {
    set +e
    for ns in ns0 nsnat1 nsnat2 nsprov nscli; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    pkill -9 -f 'target/release/bore' 2>/dev/null
    for v in vn1w vn2w vp1l vc2l; do ip link del "$v" 2>/dev/null; done
    set -e
}
trap cleanup EXIT INT TERM

rm -rf "$TMPDIR"
mkdir -p "$TMPDIR"

# ── Topology ────────────────────────────────────────────────────────────────
build_topology() {
    for ns in ns0 nsnat1 nsnat2 nsprov nscli; do ip netns add "$ns"; done
    for ns in ns0 nsnat1 nsnat2 nsprov nscli; do
        ip -n "$ns" link set lo up
    done

    # nsnat1 WAN ↔ ns0
    ip link add vn1w type veth peer name vn1s
    ip link set vn1w netns nsnat1; ip link set vn1s netns ns0
    ip -n nsnat1 addr add 192.0.2.1/24 dev vn1w
    ip -n ns0    addr add "$SERVER_IP/24" dev vn1s
    ip -n nsnat1 link set vn1w up; ip -n ns0 link set vn1s up

    # nsnat2 WAN ↔ ns0
    ip link add vn2w type veth peer name vn2s
    ip link set vn2w netns nsnat2; ip link set vn2s netns ns0
    ip -n nsnat2 addr add 192.0.3.1/24 dev vn2w
    ip -n ns0    addr add "$SERVER_IP2/24" dev vn2s
    ip -n nsnat2 link set vn2w up; ip -n ns0 link set vn2s up

    # nsprov LAN ↔ nsnat1
    ip link add vp1l type veth peer name vp1n
    ip link set vp1l netns nsprov; ip link set vp1n netns nsnat1
    ip -n nsprov addr add 10.1.0.2/24 dev vp1l
    ip -n nsnat1 addr add 10.1.0.1/24 dev vp1n
    ip -n nsprov link set vp1l up; ip -n nsnat1 link set vp1n up

    # nscli LAN ↔ nsnat2
    ip link add vc2l type veth peer name vc2n
    ip link set vc2l netns nscli; ip link set vc2n netns nsnat2
    ip -n nscli  addr add 10.2.0.2/24 dev vc2l
    ip -n nsnat2 addr add 10.2.0.1/24 dev vc2n
    ip -n nscli link set vc2l up; ip -n nsnat2 link set vc2n up

    # Routing: peers default via their NAT; NATs default via ns0; ns0 forwards.
    ip -n nsprov route add default via 10.1.0.1
    ip -n nscli  route add default via 10.2.0.1
    ip -n nsnat1 route add default via "$SERVER_IP"
    ip -n nsnat2 route add default via "$SERVER_IP2"
    ip netns exec ns0    sysctl -qw net.ipv4.ip_forward=1
    ip netns exec nsnat1 sysctl -qw net.ipv4.ip_forward=1
    ip netns exec nsnat2 sysctl -qw net.ipv4.ip_forward=1
}

# nat_rules <ns> <wan-if> <mapping: eim|edm> [filtering: apdf|adf|eif] [lan-ip] [port]
#
# `mapping` and `filtering` are INDEPENDENT axes (RFC 4787 §4.1 / §5) and the
# whole point of this harness is that a real kernel implements them with two
# different mechanisms. Defaults reproduce the previous two-mode behaviour
# byte-for-byte: `eim`/`edm` with no filtering argument is `apdf`, which is
# plain conntrack and what every scenario before this matrix used.
nat_rules() {
    local ns="$1" wan="$2" mapping="$3" filtering="${4:-apdf}" lan_ip="${5:-}" port="${6:-}"
    ip netns exec "$ns" nft flush ruleset
    ip netns exec "$ns" nft add table ip nat
    # The set of addresses this router has SENT a UDP datagram to. It is what
    # makes address-dependent filtering expressible: "an unsolicited datagram
    # from a peer we already talked to". The timeout is generous relative to a
    # punch round; a real router's UDP conntrack timeout is 30-180 s.
    if [ "$filtering" = "adf" ]; then
        ip netns exec "$ns" nft add set ip nat punched \
            '{ type ipv4_addr ; flags dynamic,timeout ; timeout 3m ; }'
    fi
    # PREROUTING first: the filtering axis. A masquerading router cannot
    # deliver an unsolicited datagram without being told which inside host
    # owns the port, so "looser than APDF" IS a port forward, conditional on
    # the source for ADF and unconditional for EIF.
    if [ "$filtering" != "apdf" ]; then
        [ -n "$lan_ip" ] && [ -n "$port" ] || die "nat_rules $filtering needs a lan ip and a port"
        ip netns exec "$ns" nft add chain ip nat prerouting \
            '{ type nat hook prerouting priority -100 ; }'
        if [ "$filtering" = "adf" ]; then
            ip netns exec "$ns" nft add rule ip nat prerouting iifname "$wan" \
                udp dport "$port" ip saddr @punched dnat to "$lan_ip:$port"
        else
            ip netns exec "$ns" nft add rule ip nat prerouting iifname "$wan" \
                udp dport "$port" dnat to "$lan_ip:$port"
        fi
    fi
    ip netns exec "$ns" nft add chain ip nat postrouting '{ type nat hook postrouting priority 100 ; }'
    # Recording must come BEFORE the masquerade rule: `update` does not
    # terminate evaluation, but a `masquerade` verdict does. A nat chain is
    # only traversed by the FIRST packet of a flow, which is precisely the
    # punch that opens the hole — exactly the event ADF keys on.
    if [ "$filtering" = "adf" ]; then
        ip netns exec "$ns" nft add rule ip nat postrouting oifname "$wan" \
            ip protocol udp update @punched '{ ip daddr }'
    fi
    if [ "$mapping" = "edm" ] || [ "$mapping" = "random" ]; then
        # Fully-random per-flow port allocation ≈ endpoint-dependent mapping.
        ip netns exec "$ns" nft add rule ip nat postrouting oifname "$wan" masquerade fully-random
    else
        ip netns exec "$ns" nft add rule ip nat postrouting oifname "$wan" masquerade
    fi
    # Realistic router INPUT policy: DROP unsolicited WAN datagrams addressed
    # to the router itself. Without this, a peer's punch that wins the
    # crossfire race lands in the router's INPUT conntrack and CLAIMS the
    # reply tuple — the inside peer's own mapping then gets remapped to a
    # random port (observed live: advertised :59246, remapped :49254) and the
    # hole-punch deadlocks. A dropped packet's conntrack entry is never
    # confirmed, so port preservation survives. Home routers behave this way.
    #
    # NOTE this is the router's own INPUT, never FORWARD: a packet that the
    # prerouting dnat above redirected to a LAN host is forwarded, not input,
    # so the two rules do not contradict each other.
    ip netns exec "$ns" nft add table ip filter
    ip netns exec "$ns" nft add chain ip filter input '{ type filter hook input priority 0 ; }'
    ip netns exec "$ns" nft add rule ip filter input ct state established,related accept
    ip netns exec "$ns" nft add rule ip filter input iifname "$wan" ip protocol udp drop
}

block_udp() {
    local ns="$1" wan="$2"
    ip netns exec "$ns" nft add table ip filter
    ip netns exec "$ns" nft add chain ip filter forward '{ type filter hook forward priority 0 ; }'
    ip netns exec "$ns" nft add rule ip filter forward oifname "$wan" ip protocol udp drop
}

# Best-effort conntrack flush. It is BEST-EFFORT on purpose and must never be
# the only thing isolating two cells: `conntrack` is a separate package
# (conntrack-tools) that is absent on a stock box, and the old body hid that
# behind `2>/dev/null || true` — a harness that believes it flushed when it
# did not is worse than one that never tried, because it fabricates results
# (see the port comment above). The real isolation is the per-cell port pair;
# this stays because it also clears the INBOUND entries a punch leaves behind.
CONNTRACK_TOOL="$(command -v conntrack || true)"
flush_conntrack() {
    [ -n "$CONNTRACK_TOOL" ] || return 0
    ip netns exec "$1" "$CONNTRACK_TOOL" -F >/dev/null 2>&1 || true
}

wait_tcp() {
    local ns="$1" ip="$2" port="$3"
    for _ in $(seq 1 50); do
        if ip netns exec "$ns" nc -z -w1 "$ip" "$port" 2>/dev/null; then return 0; fi
        sleep 0.2
    done
    return 1
}

# run_scenario <label> <prov-profile> <cons-profile> <block-consumer-udp> \
#              <expect> [spray]
#
# `spray` is `off` (default) or `on`. OFF pins the ordinary check round — the
# mechanism the §6 matrix is about — by giving the Fase 7 escape a zero
# budget. ON sizes the escape so its collision probability is ~1 rather than
# the ~95% the shipped defaults aim for: a gate that fails one run in twenty is
# a gate that gets deleted, and the mechanism under test is the rendezvous, not
# the dice. See `BORE_UDP_SPRAY_*` in `src/holepunch.rs`.
#
# A profile is `<mapping>:<filtering>` — `eim:apdf` is the typical home
# router, `edm:apdf` the symmetric/mobile one, `eim:adf` a restricted cone and
# `eim:eif` a full cone (a static UDP port forward). The legacy two-argument
# form is still accepted so the three original scenarios read unchanged.
run_scenario() {
    local label="$1" prov_prof="$2" cons_prof="$3" block="$4" expect="$5"
    local spray="${6:-off}"
    local id="udpnat-${label}"
    local sdir="$TMPDIR/$label"

    # Before the case filter: a cell keeps its ports whether or not the rest
    # of the matrix runs.
    cell_ports

    # Honour a case filter so ONE cell can be re-run in isolation: a matrix
    # that takes minutes is a matrix nobody re-runs while investigating.
    if [ -n "${CASES:-}" ] && ! printf '%s\n' $CASES | grep -qx "$label"; then
        return
    fi
    mkdir -p "$sdir"

    # `<mapping>:<filtering>[:port]`. The optional third field forces
    # `--nat-udp-preferred-port` on a side whose filtering does NOT need it,
    # which exists for exactly one reason: it is the control that isolates the
    # dnat as the cause of a DIRECT result. Without it, `eim:adf` differs from
    # `eim:apdf` in TWO things — the forward AND the fixed, port-preserved
    # mapping — and a two-variable comparison proves nothing.
    local prov_map cons_map prov_filt cons_filt
    prov_map="$(printf '%s' "$prov_prof" | cut -d: -f1)"
    prov_filt="$(printf '%s' "$prov_prof" | cut -d: -f2)"
    cons_map="$(printf '%s' "$cons_prof" | cut -d: -f1)"
    cons_filt="$(printf '%s' "$cons_prof" | cut -d: -f2)"
    local prov_port_flag=() cons_port_flag=()
    # A side whose filtering is looser than APDF ALWAYS needs a fixed port:
    # that is what the router's dnat targets, and what a real operator
    # configures. The `:port` suffix asks for it anyway.
    if [ "$prov_filt" != "apdf" ] || [ "${prov_prof##*:}" = "port" ]; then
        prov_port_flag=(--nat-udp-preferred-port "$PROV_UDP_PORT")
    fi
    if [ "$cons_filt" != "apdf" ] || [ "${cons_prof##*:}" = "port" ]; then
        cons_port_flag=(--nat-udp-preferred-port "$CONS_UDP_PORT")
    fi

    # 512 sockets x 4096 sprayed ports over the 64512 allocatable ones is
    # p = 1 - (1 - 4096/64512)^512 ≈ 1 - 2.5e-15.
    local spray_env=(BORE_UDP_SPRAY_CAP_MS=0) settle=2
    if [ "$spray" = "on" ]; then
        spray_env=(
            BORE_UDP_SPRAY_SOCKETS=512
            BORE_UDP_SPRAY_PORTS=2048
            BORE_UDP_SPRAY_PASSES=2
            BORE_UDP_SPRAY_PACE_US=200
            BORE_UDP_SPRAY_CAP_MS=8000
        )
        settle=14
    fi

    echo "--- $label: provider $prov_prof  x  consumer $cons_prof  (expect $expect, spray $spray) ---"
    nat_rules nsnat1 vn1w "$prov_map" "$prov_filt" 10.1.0.2 "$PROV_UDP_PORT"
    nat_rules nsnat2 vn2w "$cons_map" "$cons_filt" 10.2.0.2 "$CONS_UDP_PORT"
    if [ "$block" = "yes" ]; then block_udp nsnat2 vn2w; fi
    flush_conntrack nsnat1
    flush_conntrack nsnat2

    # Echo service in nsprov.
    ip netns exec nsprov socat "TCP-LISTEN:$ECHO_PORT,reuseaddr,fork" PIPE &
    local echo_pid=$!
    sleep 0.3

    # Provider (bore local, secret tunnel, --udp). STUN target = the server IP
    # on the provider's own side: ns0 is multihomed and an unconnected UDP
    # reply picks its source by route, so a cross-side STUN target would answer
    # from the "wrong" IP and be discarded by the source check.
    ip netns exec nsprov env RUST_LOG=info "${spray_env[@]}" "$BORE" local "$ECHO_PORT" \
        --to "http://$SERVER_IP:$CTRL_PORT" --secret "$SECRET" \
        --tcp-secret-id "$id" --udp \
        --stun-server "$SERVER_IP:$CTRL_PORT" \
        "${prov_port_flag[@]}" \
        >"$sdir/provider.log" 2>&1 &
    local prov_pid=$!
    sleep 1.5

    # Consumer (bore proxy, --udp). STUN = server IP on the consumer's side
    # (see the provider note above).
    ip netns exec nscli env RUST_LOG=info "${spray_env[@]}" "$BORE" proxy \
        --to "http://$SERVER_IP:$CTRL_PORT" --secret "$SECRET" \
        --tcp-secret-id "$id" --udp \
        --stun-server "$SERVER_IP2:$CTRL_PORT" \
        --local-proxy-port "127.0.0.1:$PROXY_PORT" \
        "${cons_port_flag[@]}" \
        >"$sdir/consumer.log" 2>&1 &
    local cons_pid=$!

    if ! wait_tcp nscli 127.0.0.1 "$PROXY_PORT"; then
        fail "$label: consumer proxy port never came up"
        kill -9 "$echo_pid" "$prov_pid" "$cons_pid" 2>/dev/null || true
        return
    fi
    # Give the direct-path negotiation time to settle before probing. The
    # escape runs AFTER a dry check round, so a spray cell needs the round, the
    # spray budget and the QUIC handshake that follows it.
    sleep "$settle"

    local got
    got=$(ip netns exec nscli sh -c "printf 'hello-nat\n' | nc -w3 127.0.0.1 $PROXY_PORT" 2>/dev/null | head -1)
    if [ "$got" = "hello-nat" ]; then
        pass "$label: end-to-end data through double NAT"
    else
        fail "$label: no echo through tunnel (got '$got')"
    fi

    sleep 0.5
    local cons_direct prov_direct
    cons_direct=$(grep -c "direct udp connection established (consumer" "$sdir/consumer.log" || true)
    prov_direct=$(grep -c "accepted direct udp connection (provider" "$sdir/provider.log" || true)

    if [ "$expect" = "direct" ]; then
        if [ "$cons_direct" -ge 1 ] && [ "$prov_direct" -ge 1 ]; then
            pass "$label: direct path established (consumer+provider logs)"
        else
            fail "$label: expected DIRECT path (consumer=$cons_direct provider=$prov_direct)"
        fi
    else
        if [ "$cons_direct" -eq 0 ]; then
            pass "$label: stayed on relay as expected"
        else
            fail "$label: unexpected direct path (NAT scenario should force relay)"
        fi
        if grep -qE "falling back to relay|fallback_reason|direct path unavailable|UdpUnavailable|no STUN response|all STUN probes failed" \
            "$sdir/consumer.log"; then
            pass "$label: consumer logged an explicit relay-fallback reason"
        else
            fail "$label: no relay-fallback reason in consumer log"
        fi
    fi

    kill -9 "$echo_pid" "$prov_pid" "$cons_pid" 2>/dev/null || true
    wait "$echo_pid" "$prov_pid" "$cons_pid" 2>/dev/null || true
    sleep 0.3
}

# run_filter_probe <label> <provider-profile> <expect: apdf|adf-or-eif>
#
# Runs the STANDALONE `bore test-udp` diagnostic behind a router of a known
# filtering profile and asserts that the tool REPORTS that profile.
#
# This is the gate that makes the §13 gap closed rather than merely coded:
# every other assertion in this file measures what the punch DOES, while this
# one measures whether the diagnostic can PREDICT it. They are different
# claims and the second is the one an operator acts on — `bore test-udp` is
# what gets run before deciding whether a direct path is worth pursuing, and
# until now it could only report the mapping, which the matrix above shows is
# not the axis that decides.
run_filter_probe() {
    local label="$1" prof="$2" expect="$3"
    local sdir="$TMPDIR/$label"

    cell_ports
    if [ -n "${CASES:-}" ] && ! printf '%s\n' $CASES | grep -qx "$label"; then
        return
    fi
    mkdir -p "$sdir"
    local map filt
    map="$(printf '%s' "$prof" | cut -d: -f1)"
    filt="$(printf '%s' "$prof" | cut -d: -f2)"
    echo "--- $label: probe behind $prof (expect filtering=$expect) ---"
    nat_rules nsnat1 vn1w "$map" "$filt" 10.1.0.2 "$PROV_UDP_PORT"
    flush_conntrack nsnat1

    # The fixed port is what the router's forward targets; it is also what a
    # real operator configures, so the diagnostic is exercised in the shape it
    # is actually used.
    ip netns exec nsprov env RUST_LOG=info "$BORE" test-udp \
        --to "http://$SERVER_IP:$CTRL_PORT" \
        --stun-server "$SERVER_IP:$CTRL_PORT" \
        --nat-udp-preferred-port "$PROV_UDP_PORT" \
        >"$sdir/test-udp.log" 2>&1 || true

    # The router's own view, captured every run: when a cell disagrees with
    # the matrix the first question is always whether the rule matched, and
    # re-running to find out costs a full teardown.
    ip netns exec nsnat1 nft list ruleset >"$sdir/router.nft" 2>&1 || true

    local line
    line=$(grep -m1 '^NAT filtering' "$sdir/test-udp.log" 2>/dev/null || true)
    if [ -z "$line" ]; then
        fail "$label: the diagnostic printed no NAT filtering line (see $sdir/test-udp.log)"
        return
    fi
    case "$expect" in
        apdf)       want='address+port dependent' ;;
        adf-or-eif) want='address dependent or open' ;;
        *)          want="$expect" ;;
    esac
    if printf '%s' "$line" | grep -qF "$want"; then
        pass "$label: $line"
    else
        fail "$label: expected '$want', got '$line'"
    fi
}

# ── Run ─────────────────────────────────────────────────────────────────────
build_topology

ip netns exec ns0 env RUST_LOG=info "$BORE" server \
    --secret "$SECRET" --udp >"$TMPDIR/server.log" 2>&1 &
SERVER_PID=$!
if ! wait_tcp ns0 "$SERVER_IP" "$CTRL_PORT"; then
    die "bore server never came up (see $TMPDIR/server.log)"
fi

# The three original smoke cells, unchanged in meaning.
run_scenario "T-NAT-DIRECT"        eim:apdf eim:apdf no direct
run_scenario "T-NAT-RANDOM-RELAY"  edm:apdf edm:apdf no relay
run_scenario "T-NAT-BLOCKED-RELAY" eim:apdf eim:apdf yes relay

# The matrix cells the documentation asserts and nothing on a real kernel
# proved. All three share the SAME consumer — endpoint-dependent mapping, the
# mobile/CGNAT shape — and differ only in the provider's FILTERING, which is
# the axis `bore test-udp` does not currently report (§13). The outcome flips
# across them, which is what makes that gap matter.
# NOTE the explicit `off` on the two RELAY cells: they measure the ordinary
# check round, which is what the matrix is a statement about. With the Fase 7
# escape in its default sizing both would go DIRECT — that is the point of
# Fase 7 and it is gated separately below (`T-NAT-SPRAY-*`), but letting it
# leak in here would silently turn the matrix into a measurement of something
# else.
run_scenario "T-NAT-APDF-VS-EDM"   eim:apdf      edm:apdf no relay off
run_scenario "T-NAT-ADF-VS-EDM"    eim:adf       edm:apdf no direct
run_scenario "T-NAT-EIF-VS-EDM"    eim:eif       edm:apdf no direct

# The control for the two DIRECT cells above: the SAME fixed, port-preserved
# mapping, WITHOUT the router's forward. If this went direct, the forward
# would not be what the pair is measuring and the two cells above would be
# proving nothing — so its expected RELAY is a red-check, not a smoke test.
run_scenario "T-NAT-FIXEDPORT-VS-EDM" eim:apdf:port edm:apdf no relay off

# Does the DIAGNOSTIC see what the matrix just proved? Same two routers, read
# through `bore test-udp` instead of through a punch. A tool that cannot tell
# these two apart cannot advise on the cells above.
run_filter_probe "T-NAT-FILTER-APDF" eim:apdf apdf
run_filter_probe "T-NAT-FILTER-ADF"  eim:adf  adf-or-eif

# Fase 7 — the sprayed escape (docs/nat/NAT_SOTA_COMPARISON.md §4.1).
#
# The cells above establish that `eim:apdf x edm` cannot be won by an ordinary
# check round: port prediction has nothing to predict against a fully-random
# mapping, and every probe the round sends goes, by construction, to the wrong
# port. The escape wins it anyway, by rendezvous: the endpoint-independent side
# sprays destination ports (each one opening its OWN filter for that peer port,
# which is the direction that was blocked) while the symmetric side opens
# auxiliary sockets, and one collision opens both directions at once.
#
# The three cells below are a SINGLE experiment with one variable. The OFF cell
# re-measures the baseline every run rather than trusting a commit message, and
# the two ON cells flip it — with the roles the other way round in the second,
# because promoting an auxiliary socket on the LISTENER side (a QUIC server
# built on a socket that did not exist when the round started) is a different
# code path from promoting it on the dialer side, and only one of them is
# exercised by any given cell.
#
# If a future change breaks the escape, OFF still passes and ON fails, which
# says so precisely. If a future change breaks the CELL — say, by making the
# ordinary round win it — OFF fails, and that is the more interesting failure.
run_scenario "T-NAT-SPRAY-OFF"      eim:apdf edm:apdf no relay  off
run_scenario "T-NAT-SPRAY-DIALER"   eim:apdf edm:apdf no direct on
run_scenario "T-NAT-SPRAY-LISTENER" edm:apdf eim:apdf no direct on

kill -9 "$SERVER_PID" 2>/dev/null || true

echo
echo "── Results ──────────────────────────────────────────"
echo "PASS: $PASS  FAIL: $FAIL"
[ "$FAIL" -eq 0 ]
