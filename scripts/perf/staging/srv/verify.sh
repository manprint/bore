#!/usr/bin/env bash
# Read back what the server is actually running: the vhost entries and the
# tunables the admin API resolves. Run it after every compose change.
#
# The tunables block is the point: /admin/api/v1/config DERIVES the direct-UDP
# window values from the tuning installed at runtime, so a budget that changed
# them shows up here. A field reported as null means the running image predates
# the 2026-09-11 admin fix, NOT that the value is unset.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"

say "vhost entries"
adm vhost | jq -r '.[]|"  \(.subdomain) path=\(.current_path) carriers=\(.carriers) target=\(.carrier_target) fallbacks=\(.direct_fallbacks)"'

say "resolved tunables"
adm config | jq '{
  proxy_buffer_size,
  udp_stream_receive_window,
  udp_connection_receive_window,
  udp_send_window,
  udp_socket_recv_buffer,
  udp_socket_send_buffer,
  udp_max_streams,
  udp_direct_slots,
  direct_quic_keepalive_ms,
  direct_quic_idle_ms
}'

say "direct-path counters"
adm metrics | jq '{direct_budget_refusals, direct_fallbacks, auth_failures, conn_rejections}' 2>/dev/null
