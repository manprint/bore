#!/usr/bin/env bash
# Print the server's live configuration, as the server itself names it.
#
# Exists because a harness that guesses a field name sees an unconfigured
# server and says nothing: `/admin/api/v1/config` publishes the public range as
# ONE `port_range` key, not `min_port`/`max_port`, and the vhost section is
# DERIVED from the live config on every read rather than being a startup
# snapshot. Read the names here before writing a `jq` filter against them.
#
#   cfgkeys.sh            # the tuning-relevant keys (the default filter)
#   cfgkeys.sh all        # every key
#   cfgkeys.sh udp        # any regex
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"

FILTER="${1:-port|buffer|proxy|window|carrier|conn|udp|version}"
[ "$FILTER" = all ] && FILTER='.'

adm config | jq -r --arg re "$FILTER" \
  'to_entries[] | select(.key|test($re)) | "\(.key) = \(.value|tostring)"'
