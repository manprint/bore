#!/usr/bin/env bash
# Measure the bore server's CPU cost per GiB moved, and separate a CPU ceiling
# from a link ceiling. Runs ON the measurement VM (see the runbook, section 9.2).
#
# CPU seconds per GiB is the only throughput metric that transfers between
# machines: absolute MB/s is a property of the box and the link, s/GiB is a
# property of the code and the CPU. Divide any target link rate by it to get the
# core count that rate needs -- which is how "does the application limit the
# link?" gets answered without guessing the provider's provisioning.
#
# Each case prints the epoch window it occupied, so a server-side sampler
# (scripts/perf/server_cpu_sample.sh and scripts/perf/server_ena_sample.sh, both
# started from the workstation BEFORE this script) can be matched to it. Run
# those two concurrently; this script does not collect server CPU itself,
# deliberately -- the container's own accounting misses the softirq the host
# kernel spends on its behalf, and that is nearly half the bill.
#
# There is a 4 s idle gap before each measurement window and 5 s after each
# case so consecutive windows are separable in the sampler's 2 s series.
#
# Usage:
#   ./vhost_remote_efficiency.sh            # 3 paired single-stream rounds
#   ./vhost_remote_efficiency.sh saturate   # parallel streams: CPU vs link
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"; . "${BORE_PERF_ENV:-$H/env.sh}"
BORE=$H/bore; OP=5052; OUT=$H/out; GW="${BORE_VHOST_DOMAIN:?set BORE_VHOST_DOMAIN}"
KIDS=(); cleanup(){ for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
txb(){ adm vhost | jq -r --arg l "$1" '.[]|select(.subdomain==$l)|.relay_tx_bytes'; }
dso(){ adm vhost | jq -r --arg l "$1" '.[]|select(.subdomain==$l)|.direct_stream_opens'; }
curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping \
  || { setsid nohup python3 $H/bench_origin.py $OP > $OUT/origin.log 2>&1 < /dev/null & sleep 2; }

case_run(){ # <tag> <parallel> <flags...>
  local tag=$1 par=$2; shift 2
  local L=sa$(date +%s%N | cut -c6-13)
  # shellcheck disable=SC2086
  $BORE vhost 127.0.0.1:$OP --subdomain "$L" --id "$L" --to "$BORE_TO" --secret "$BORE_SECRET" "$@" > $OUT/$L.log 2>&1 &
  local P=$!; KIDS+=("$P")
  for i in $(seq 60); do present "$L" && break; kill -0 $P 2>/dev/null || break; sleep 0.5; done
  present "$L" || { echo "$tag REGISTRATION FAILED"; return 1; }
  curl -fsS -o /dev/null -m 30 "https://$L.$GW/100k" 2>/dev/null
  local d0 a b t0 t1 d1; local ps=()
  d0=$(dso "$L"); sleep 4
  a=$(txb "$L"); t0=$(date +%s)
  for i in $(seq "$par"); do
    curl -fsS -o /dev/null --max-time 20 "https://$L.$GW/stream/$((16*1073741824))" 2>/dev/null & ps+=("$!")
  done
  for p in "${ps[@]}"; do wait "$p" 2>/dev/null; done
  t1=$(date +%s); b=$(txb "$L"); d1=$(dso "$L")
  local path=relay-tcp; [ "${d1:-0}" -gt "${d0:-0}" ] && path=direct-quic
  LC_ALL=C awk -v t="$tag" -v p="$path" -v a="$a" -v b="$b" -v s="$t0" -v e="$t1" 'BEGIN{
    printf "CASE %s path=%s window=%d-%d dur=%ds bytes=%d rate=%.2f MB/s gb=%.3f\n", t, p, s, e, e-s, b-a, (b-a)/1048576/(e-s), (b-a)/1073741824}'
  kill -9 $P 2>/dev/null
  for i in $(seq 20); do present "$L" || break; sleep 1; done
  sleep 5
}

echo "START $(date +%s)"
case "${1:-paired}" in
  saturate)
    # Does throughput stall on the guest CPU or on the instance's own allowance?
    # Read alongside server_cpu_sample.sh: cores near the core count means the
    # guest is the wall; low cores with climbing pps/bw_exceeded means it is not.
    case_run relay-p4   4 --carriers 1
    case_run relay-p8   8 --carriers 4
    case_run quic-p4    4 --carriers 1 --udp
    case_run quic-c4p4  4 --carriers 4 --udp
    ;;
  *)
    # Alternating single-stream rounds. The order alternates so a systematic
    # first-versus-second effect cancels, and three rounds bound the drift the
    # rest of this campaign measured at 29 %.
    for r in 1 2 3; do
      case_run "relay-r$r" 1 --carriers 1
      case_run "quic-r$r"  1 --carriers 1 --udp
    done
    ;;
esac
echo "END $(date +%s)"
