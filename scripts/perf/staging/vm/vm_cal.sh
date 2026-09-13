#!/usr/bin/env bash
# Stage 0 — apparatus calibration for the DEV re-run.
#
# ICMP is blocked toward the server now (it was not during the original
# campaign, whose §2.14 quotes a 1.84 ms ping baseline), so RTT is measured
# with a TCP connect to the vhost HTTPS port instead. That is the RTT the
# tunnel actually pays, and it is the number every latency table below must be
# read against.
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"; . "${BORE_PERF_ENV:-$H/env.sh}"
BORE=$H/bore; OP=5052; OUT=$H/out; mkdir -p "$OUT"
SRV=$SRV; GW=$GW
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }

echo "=== client (measurement VM) ==="
echo "  $(. /etc/os-release; echo "${PRETTY_NAME:-unknown}")  $(uname -m)  vcpu=$(nproc)  mem=$(free -m | awk '/Mem:/{print $2" MiB"}')"
echo "  client build: $($BORE --version)"
echo "  baseline build kept for interop: $($H/bore.f15a3de --version)"
echo "  iface toward server: $(ip route get $SRV | awk '{print $5;exit}')"

echo "=== RTT, TCP connect to the vhost HTTPS port (ICMP is blocked) ==="
T=()
for i in $(seq 10); do
  T+=("$(curl -s -o /dev/null -m 5 -w '%{time_connect}' "https://$GW:443/" 2>/dev/null || echo 9)")
done
printf '%s\n' "${T[@]}" | LC_ALL=C sort -g | awk '{a[NR]=$1} END{
  printf "  n=%d min=%.3f ms  median=%.3f ms  max=%.3f ms\n", NR, a[1]*1000, a[int((NR+1)/2)]*1000, a[NR]*1000}'

echo "=== origin ==="
curl -fsS -m 2 -o /dev/null "http://127.0.0.1:$OP/ping" 2>/dev/null || {
  setsid nohup python3 "$H/bench_origin.py" $OP > "$OUT/origin.log" 2>&1 < /dev/null &
  sleep 2; }
echo "  local origin: $(curl -s -o /dev/null -m 5 -w 'http=%{http_code} t=%{time_total}' http://127.0.0.1:$OP/ping)"
echo "  local origin 100 MiB: $(curl -s -o /dev/null -m 60 -w '%{speed_download}' http://127.0.0.1:$OP/stream/104857600 | awk '{printf "%.0f MB/s", $1/1048576}')"

echo "=== server, as deployed ==="
adm metrics | jq -r '"  uptime=\(.uptime_secs)s rss=\((.mem_rss_bytes/1048576*10|round)/10)MiB vhost_domains=\(.vhost_domains) active=\(.active_connections) rej=\(.conn_rejections) fallbacks=\(.direct_fallbacks) budget_refusals=\(.direct_budget_refusals) transport_bore=\(.transport_bore) transport_ssh=\(.transport_ssh)"'
adm config | jq -r '"  max_conns=\(.max_conns) max_carriers=\(.max_carriers) udp=\(.udp) vhost_mode=\(.vhost_mode) quic_port=\(.vhost_quic_port) stream_win=\(.udp_stream_receive_window) conn_win=\(.udp_connection_receive_window) max_streams=\(.udp_max_streams)"'
echo "  --- F-6 check: is the vhost section of /config derived from the live YAML? ---"
adm config | jq -r '"  vhost_default_response_headers: \(.vhost_default_response_headers|length) entries; reservations: \(.vhost_reservations|length)"'
adm config | jq -r '.vhost_default_response_headers | to_entries | .[] | "    \(.key): \(.value)"' | head -8
echo "  --- pre-existing tunnels on this server (not mine; must survive) ---"
adm vhost | jq -r '.[] | "    \(.subdomain) transport=\(.transport) udp=\(.udp) active=\(.active)"'
echo DONE
