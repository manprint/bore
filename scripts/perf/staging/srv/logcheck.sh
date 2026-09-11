#!/usr/bin/env bash
# End-of-campaign re-check of F-3: after hours of real traffic from a browser-like
# consumer (curl over WiFi, aborted transfers, parallel streams), does the log
# stay quiet? The before-campaign's complaint was 718 of 786 warnings being
# "peer closed connection without sending TLS close_notify".
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"


# NOTE `srv`, not `$SSH`: `$SSH` in lib.sh carries the ssh OPTIONS only, with no
# host, so `$SSH "cmd"` would try to connect to a host literally named by the
# command string. These commands all run on the SERVER host.
[ -n "$BORE_SRV" ] || { echo "BORE_SRV is not set in env.sh; nothing to audit" >&2; exit 2; }
C="$BORE_SRV_CONTAINER"
srv "sudo -n docker logs $C > /tmp/bl.txt 2>&1; wc -l < /tmp/bl.txt" | sed 's/^/  total log lines: /'
srv 'grep -c " WARN " /tmp/bl.txt || true' | sed 's/^/  WARN lines: /'
srv 'grep -c " ERROR " /tmp/bl.txt || true' | sed 's/^/  ERROR lines: /'
srv 'grep -ci "close_notify" /tmp/bl.txt || true' | sed 's/^/  close_notify mentions: /'
echo "  the WARN lines themselves, deduplicated by message:"
srv 'grep " WARN " /tmp/bl.txt | sed "s/^[^ ]* *//" | sed "s/subdomain=[a-z0-9]*/subdomain=<label>/g" | sed "s/port=[0-9]*/port=<port>/g" | sort | uniq -c | sort -rn | head -12' | sed 's/^/    /'
