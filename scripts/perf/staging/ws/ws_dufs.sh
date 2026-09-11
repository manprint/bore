#!/usr/bin/env bash
# The real-world case the operator asked for: dufs on the AWS test VM, exposed
# through a bore forwarder, driven from this workstation over its own ISP link.
#
# Three shapes, because they fail for different reasons:
#   BIG    one 1 GiB file — sustained throughput, cwnd- and BDP-bound
#   MED    ten 20 MiB files in parallel — the shape a download manager makes
#   SMALL  two thousand 8 KiB files — latency-bound; this is where a tunnel's
#          per-request cost shows up and where throughput numbers are meaningless
#
# Both directions. Uploads land in dufsroot/upload and are deleted after every
# phase (the VM root has ~7.9 GiB free and must not be filled).
#
# usage: ws_dufs.sh <flavor> <tag> [flags...]
#   flavor: native | ssh | docker    (how the forwarder is run on the VM)
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"



DPORT=$DUFS_PORT
WORK="${WORK:-/tmp/claude-1000/-mnt-fabio-dati-Git-Github-manprint-bore-forked/b7760e88-ba02-4aaa-a589-04f734c10b51/scratchpad/bench/dufs/work}"
mkdir -p "$WORK"
BIGUP="$WORK/up256m.bin"
[ -f "$BIGUP" ] || head -c 268435456 /dev/zero > "$BIGUP"

adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
fld(){ adm vhost | jq -r --arg l "$1" --arg f "$2" '.[]|select(.subdomain==$l)|.[$f]'; }
mbs(){ LC_ALL=C awk -v b="$1" 'BEGIN{printf "%7.2f MB/s (%4.0f Mbit/s)", b/1048576, b*8/1000000}'; }
# The staging server is a t4g.micro: its INBOUND network allowance is a burst
# budget over a low baseline, and this campaign exhausted it once already (every
# transport collapsed to ~60 Mbit/s while `bw_in_allowance_exceeded` rose by 4 M).
# So every bandwidth rung reports the server's allowance delta across its own
# window, and rungs are separated by a cooldown. A rung with a large delta is a
# measurement of AWS shaping and is flagged rather than quietly averaged in.

COOL="${COOL:-25}"
ena(){ $SSH $SRVU@$SRV "sudo -n ethtool -S $IFACE | grep bw_in_allowance_exceeded | tr -dc '0-9'" 2>/dev/null; }
rate(){ LC_ALL=C awk -v by="$1" -v s="$2" -v e="$3" 'BEGIN{printf "%7.2f MB/s (%4.0f Mbit/s)", by/1048576/(e-s), by*8/1000000/(e-s)}'; }

FLAVOR="${1:-native}"; TAG="${2:-untagged}"; shift 2 || true
PHASES="${PHASES:-all}"
has(){ case ",$PHASES," in *,all,*|*,"$1",*) return 0;; *) return 1;; esac; }
FLAGS=("$@")
L="df$(date +%s%N | cut -c7-13)"

start_forwarder(){
  case "$FLAVOR" in
  native)
    $SSH $VMU@$V "setsid nohup $VM_HOME/bore vhost 127.0.0.1:$DPORT --subdomain $L --id $L \
        --to '$BORE_TO' --secret '$BORE_SECRET' ${FLAGS[*]} > $VM_HOME/out/$L.log 2>&1 < /dev/null & true" >/dev/null 2>&1 ;;
  docker)
    # the published client image, run with host networking so it can reach the
    # dufs bound on the VM's loopback exactly as the native binary does
    $SSH $VMU@$V "setsid nohup sudo -n docker run --rm --name bore-$L --network host \
        ghcr.io/manprint/bore:client vhost 127.0.0.1:$DPORT --subdomain $L --id $L \
        --to '$BORE_TO' --secret '$BORE_SECRET' ${FLAGS[*]} > $VM_HOME/out/$L.log 2>&1 < /dev/null & true" >/dev/null 2>&1 ;;
  ssh)
    # stock OpenSSH client, no bore binary on the provider side at all.
    # Never -N: it skips the session channel, so the gateway's banner and its
    # warnings could never be delivered (I-SSH7).
    $SSH $VMU@$V "setsid nohup sshpass -p '$SSHGW_PASS' ssh -T -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o PubkeyAuthentication=no \
        -o PreferredAuthentications=password -o ExitOnForwardFailure=yes \
        -o ServerAliveInterval=30 -o LogLevel=ERROR \
        -R vhost/$L:80:127.0.0.1:$DPORT -p 443 '$SSHGW_USER@$GW' \
        > $VM_HOME/out/$L.ssh 2>&1 < /dev/null & true" >/dev/null 2>&1 ;;
  esac
  for i in $(seq 70); do present "$L" && return 0; sleep 0.5; done
  return 1
}
stop_forwarder(){
  case "$FLAVOR" in
    native) $SSH $VMU@$V "pkill -9 -f 'subdomain $L' 2>/dev/null; true" >/dev/null 2>&1 ;;
    docker) $SSH $VMU@$V "sudo -n docker rm -f bore-$L 2>/dev/null; true" >/dev/null 2>&1 ;;
    ssh)    $SSH $VMU@$V "pkill -9 -f 'vhost/$L:80' 2>/dev/null; true" >/dev/null 2>&1 ;;
  esac
  for i in $(seq 30); do present "$L" || break; sleep 1; done
}
trap 'stop_forwarder' EXIT INT TERM

echo "##### flavor=$FLAVOR  tag=$TAG  flags='${FLAGS[*]:-none}'  label=$L"
$SSH $VMU@$V "$VM_HOME/vm_dufs_setup.sh clean" 2>/dev/null | sed 's/^/  /'
if ! start_forwarder; then
  echo "  REGISTRATION FAILED"; $SSH $VMU@$V "tail -3 $VM_HOME/out/$L.log $VM_HOME/out/$L.ssh 2>/dev/null" | sed 's/^/    /'
  exit 1
fi
curl -fsS -o /dev/null -m 40 -r 0-1023 "https://$L.$GW/big1g.bin" 2>/dev/null
d0=$(fld "$L" direct_stream_opens)
curl -fsS -o /dev/null -m 40 -r 0-1023 "https://$L.$GW/big1g.bin" 2>/dev/null
d1=$(fld "$L" direct_stream_opens)
path=relay; [ "${d1:-0}" -gt "${d0:-0}" ] && path=direct
echo "  transport=$(fld "$L" transport) proven_path=$path"

if has bigdl; then
echo "  --- BIG download: 256 MiB range of big1g.bin per stream ---"
for n in 1 2 4 8; do
  sleep "$COOL"
  tmp=$(mktemp -d); e0=$(ena); t0=$(date +%s.%N)
  for i in $(seq "$n"); do
    ( curl -s -o /dev/null -m 300 -r 0-268435455 -w '%{speed_download} %{size_download}\n' \
        "https://$L.$GW/big1g.bin" > "$tmp/$i" 2>/dev/null ) &
  done
  wait; t1=$(date +%s.%N); e1=$(ena)
  by=$(LC_ALL=C awk '{s+=$2} END{printf "%.0f", s}' "$tmp"/*)
  echo "      x$n: $(rate "$by" "$t0" "$t1")  bytes=$by allowance=+$((${e1:-0}-${e0:-0}))"
  rm -rf "$tmp"
done

fi

if has meddl; then
echo "  --- MED download: 10 x 20 MiB, all at once ---"
tmp=$(mktemp -d); : > "$WORK/med.cfg"
for i in $(seq 10); do printf 'url = "https://%s.%s/med/m%s.bin"\noutput = "/dev/null"\n' "$L" "$GW" "$i" >> "$WORK/med.cfg"; done
t0=$(date +%s.%N)
# every URL in a -Z run needs its OWN output, which the config file already
# gives it; a trailing -o here would be an output with no URL to bind to
curl -sZ --parallel-max 10 -K "$WORK/med.cfg" -w '%{size_download} %{num_connects}\n' > "$tmp/o" 2>/dev/null
t1=$(date +%s.%N)
by=$(LC_ALL=C awk '{s+=$1} END{printf "%.0f", s}' "$tmp/o"); cn=$(LC_ALL=C awk '{s+=$2} END{print s}' "$tmp/o")
echo "      10 parallel: $(rate "${by:-0}" "$t0" "$t1")  conns=$cn"
rm -rf "$tmp"

fi

if has smalldl; then
echo "  --- SMALL download: 2000 x 8 KiB (latency-bound) ---"
for pm in 1 8 32; do
  : > "$WORK/small.cfg"
  for i in $(seq 500); do printf 'url = "https://%s.%s/small/s%s.bin"\noutput = "/dev/null"\n' "$L" "$GW" "$i" >> "$WORK/small.cfg"; done
  t0=$(date +%s.%N)
  curl -sZ --parallel-max "$pm" -K "$WORK/small.cfg" >/dev/null 2>&1
  t1=$(date +%s.%N)
  LC_ALL=C awk -v s="$t0" -v e="$t1" -v n=500 'BEGIN{printf "      500 files, parallel-max %s: %.2fs total, %.2f ms per file, %.0f files/s\n", "'"$pm"'", e-s, (e-s)*1000/n, n/(e-s)}'
done

fi

if has bigup; then
echo "  --- BIG upload: 256 MiB PUT per stream ---"
for n in 1 2 4; do
  sleep "$COOL"
  tmp=$(mktemp -d); e0=$(ena); t0=$(date +%s.%N)
  for i in $(seq "$n"); do
    ( curl -s -o /dev/null -m 300 -X PUT -T "$BIGUP" -H 'Expect:' -w '%{size_upload}\n' \
        "https://$L.$GW/upload/u$i.bin" > "$tmp/$i" 2>/dev/null ) &
  done
  wait; t1=$(date +%s.%N); e1=$(ena)
  by=$(LC_ALL=C awk '{s+=$1} END{printf "%.0f", s}' "$tmp"/*)
  echo "      x$n: $(rate "${by:-0}" "$t0" "$t1")  bytes=$by allowance=+$((${e1:-0}-${e0:-0}))"
  rm -rf "$tmp"
  $SSH $VMU@$V "$VM_HOME/vm_dufs_setup.sh clean" >/dev/null 2>&1
done

fi

if has smallup; then
echo "  --- SMALL upload: 500 x 8 KiB PUT ---"
[ -f "$WORK/s8k.bin" ] || head -c 8192 /dev/zero > "$WORK/s8k.bin"
for pm in 1 32; do
  : > "$WORK/supload.cfg"
  for i in $(seq 500); do
    printf 'url = "https://%s.%s/upload/p%s_%s.bin"\nupload-file = "%s"\noutput = "/dev/null"\n' "$L" "$GW" "$pm" "$i" "$WORK/s8k.bin" >> "$WORK/supload.cfg"
  done
  t0=$(date +%s.%N)
  curl -sZ --parallel-max "$pm" -K "$WORK/supload.cfg" -H 'Expect:' >/dev/null 2>&1
  t1=$(date +%s.%N)
  LC_ALL=C awk -v s="$t0" -v e="$t1" -v n=500 'BEGIN{printf "      500 PUTs, parallel-max %s: %.2fs total, %.2f ms per file, %.0f files/s\n", "'"$pm"'", e-s, (e-s)*1000/n, n/(e-s)}'
  $SSH $VMU@$V "$VM_HOME/vm_dufs_setup.sh clean" >/dev/null 2>&1
done

fi

echo "  --- request latency through the same tunnel ---"
echo "      $(timeout 30 oha -z 8s -c 8 --no-tui --output-format json "https://$L.$GW/small/s1.bin" 2>/dev/null \
    | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) p99=\(.metrics.latency_ms.p99)"')"
echo "  --- entry at the end ---"
echo "      carriers=$(fld "$L" carriers) target=$(fld "$L" carrier_target) path=$(fld "$L" current_path) fallbacks=$(fld "$L" direct_fallbacks) active=$(fld "$L" active)"
stop_forwarder
trap - EXIT INT TERM
echo "##### END $TAG"
