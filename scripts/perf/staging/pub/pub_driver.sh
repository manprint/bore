#!/usr/bin/env bash
# Sequential driver for the VM-side public-tunnel stages.
#
# Strictly serial on purpose: every stage shares one server, one 2-vCPU VM and
# one instance allowance budget, so overlapping two of them contaminates both.
# Each stage's log carries its own start/end timestamps so it can be correlated
# afterwards with the workstation-side allowance timeline and the resource
# samplers.
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"
R="$H/res"; mkdir -p "$R"
HERE="$(cd "$(dirname "$0")" && pwd)"

run() {
    local name="$1"; shift
    echo "### $(date -Is) START $name" >> "$R/pub_driver.log"
    "$@" > "$R/$name.log" 2>&1
    echo "### $(date -Is) END   $name rc=$?" >> "$R/pub_driver.log"
    sleep 20
}

STAGES="${*:-p1 p2 flavours conc netem eff stab}"
for s in $STAGES; do
  case "$s" in
    p1)       run pub_ab_p1     "$HERE/vm_pub_ab.sh" p1 ;;
    p2)       run pub_ab_p2     "$HERE/vm_pub_ab.sh" p2 ;;
    p3)       run pub_ab_p3     "$HERE/vm_pub_ab.sh" p3 ;;
    p4)       run pub_ab_p4     "$HERE/vm_pub_ab.sh" p4 ;;
    flavours) run pub_flavours  "$HERE/vm_pub_flavours.sh" ;;
    conc)     run pub_conc      "$HERE/vm_pub_conc.sh" ;;
    netem)    run pub_netem     "$HERE/vm_pub_netem.sh" ;;
    eff)      run pub_eff       "$HERE/vm_pub_eff.sh" ;;
    stab)     run pub_stab      "$HERE/vm_pub_stab.sh" all ;;
    soak)     run pub_soak      "$HERE/vm_pub_stab.sh" soak ;;
    *) echo "unknown stage: $s" >&2 ;;
  esac
done
echo "ALL PUBLIC STAGES DONE $(date -Is)" >> "$R/pub_driver.log"
