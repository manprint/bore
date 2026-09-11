#!/usr/bin/env bash
# End-of-campaign re-check of F-3: after hours of real traffic from a browser-like
# consumer (curl over WiFi, aborted transfers, parallel streams), does the log
# stay quiet? The before-campaign's complaint was 718 of 786 warnings being
# "peer closed connection without sending TLS close_notify".
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"


$SSH 'sudo -n docker logs bore-server > /tmp/bl.txt 2>&1; wc -l < /tmp/bl.txt' | sed 's/^/  total log lines: /'
$SSH 'grep -c " WARN " /tmp/bl.txt || true' | sed 's/^/  WARN lines: /'
$SSH 'grep -c " ERROR " /tmp/bl.txt || true' | sed 's/^/  ERROR lines: /'
$SSH 'grep -ci "close_notify" /tmp/bl.txt || true' | sed 's/^/  close_notify mentions: /'
echo "  the WARN lines themselves, deduplicated by message:"
$SSH 'grep " WARN " /tmp/bl.txt | sed "s/^[^ ]* *//" | sed "s/subdomain=[a-z0-9]*/subdomain=<label>/g" | sort | uniq -c | sort -rn | head -12' | sed 's/^/    /'
