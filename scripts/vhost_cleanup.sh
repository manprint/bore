#!/usr/bin/env bash
# Emergency / idempotent cleanup for the vhost netns test harness.
# Removes EVERYTHING the vhost_* scripts can leave behind, in any order, even
# after a crash/segfault/stall that skipped a script's own cleanup trap.
#
# Safe to run repeatedly. Run as root:  sudo scripts/vhost_cleanup.sh
set +e

echo "=== vhost test cleanup ==="

# 1. Kill every bore process started by the harness (server AND clients). The old
#    trap used `pkill -f bore.*vhost`, which matched ONLY `bore vhost` clients and
#    left `bore server` running — that orphan held the namespace open.
pkill -f 'target/release/bore' 2>/dev/null
pkill -x bore 2>/dev/null
# Test-side helpers that may linger.
pkill -f 'http\.server' 2>/dev/null
pkill -f 'ThreadingHTTPServer' 2>/dev/null
pkill -f 'iperf3' 2>/dev/null
pkill -x hey 2>/dev/null
pkill -x wrk 2>/dev/null
sleep 0.5
# Force-kill any survivors.
pkill -9 -f 'target/release/bore' 2>/dev/null
pkill -9 -f 'http\.server' 2>/dev/null
sleep 0.3

# 2. Delete every namespace any vhost_* script can create.
for ns in ns0 nsp nsp1 nsp2 nsc dbg_ns0 dbg_ns1; do
    if ip netns list 2>/dev/null | grep -qw "$ns"; then
        # Kill anything still pinned inside the namespace before deleting it, so
        # the delete cannot fail with EBUSY and leak /run/netns/$ns.
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null && echo "  deleted netns $ns" \
            || echo "  WARN: could not delete netns $ns (still busy?)"
    fi
done
# Belt-and-suspenders: remove any stale /run/netns mount files left behind.
for ns in ns0 nsp nsp1 nsp2 nsc dbg_ns0 dbg_ns1; do
    if [ -e "/run/netns/$ns" ]; then
        umount "/run/netns/$ns" 2>/dev/null
        rm -f "/run/netns/$ns" 2>/dev/null && echo "  removed stale /run/netns/$ns"
    fi
done

# 3. Remove any veth left in the root namespace (peers are auto-removed with their
#    namespace; these are only the root-side stubs if a setup half-failed).
for v in vethsp vethps vethsc vethcs veth0s veth0p veth1s veth1p veth2s veth2p vdbg_s vdbg_p; do
    ip link show "$v" >/dev/null 2>&1 && ip link del "$v" 2>/dev/null && echo "  removed veth $v"
done

# 4. Temp files / origin dirs.
rm -rf /tmp/bore_vhost_* /tmp/vhost_config.yml /tmp/vhost_bench_* 2>/dev/null

# 5. Report what (if anything) survived.
echo "--- residual check ---"
LEFT_NS=$(ip netns list 2>/dev/null | grep -E '^(ns0|nsp|nsp1|nsp2|nsc|dbg_)' || true)
LEFT_PROC=$(pgrep -af 'target/release/bore|http\.server' 2>/dev/null || true)
[ -z "$LEFT_NS" ]   && echo "  namespaces: clean"   || echo "  LEAKED namespaces:\n$LEFT_NS"
[ -z "$LEFT_PROC" ] && echo "  processes:  clean"   || echo "  LEAKED processes:\n$LEFT_PROC"
echo "=== done ==="
