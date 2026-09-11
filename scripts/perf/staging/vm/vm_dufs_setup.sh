#!/usr/bin/env bash
# Prepare the real-world target on the test VM: dufs serving a corpus that has
# both shapes that matter, one big file and many small ones.
#
# DISK DISCIPLINE (the operator asked for it explicitly): the VM root is ~11 GiB
# with ~7.9 GiB free. The served corpus is ~1.2 GiB, the upload landing zone is
# emptied after every phase, and every phase refuses to start with less than
# 2 GiB free. Nothing here writes outside ~/dufsroot.
set -uo pipefail
H=$VM_HOME
ROOT=$H/dufsroot
PORT=5080
MINFREE_MB=2048

free_mb(){ df -Pm / | awk 'NR==2{print $4}'; }
guard(){ local f; f=$(free_mb); if [ "$f" -lt "$MINFREE_MB" ]; then
    echo "REFUSING: only ${f} MiB free on / (need ${MINFREE_MB})"; exit 1; fi
  echo "  disk free: ${f} MiB"; }

case "${1:-setup}" in
setup)
  guard
  mkdir -p "$ROOT/small" "$ROOT/med" "$ROOT/upload"
  # one big file: the sustained-throughput case
  [ -f "$ROOT/big1g.bin" ] || { echo "  creating big1g.bin"; head -c 1073741824 /dev/urandom > "$ROOT/big1g.bin"; }
  # ten medium files: the "several assets" case
  for i in $(seq 10); do
    [ -f "$ROOT/med/m$i.bin" ] || head -c 20971520 /dev/urandom > "$ROOT/med/m$i.bin"
  done
  # two thousand small files: the latency-bound case a real page load looks like
  if [ "$(ls -1 "$ROOT/small" 2>/dev/null | wc -l)" -lt 2000 ]; then
    echo "  creating 2000 small files (8 KiB each)"
    head -c 8192 /dev/urandom > /tmp/_seed8k
    for i in $(seq 2000); do cp /tmp/_seed8k "$ROOT/small/s$i.bin"; done
    rm -f /tmp/_seed8k
  fi
  echo "  corpus: $(du -sh "$ROOT" | cut -f1)  files=$(find "$ROOT" -type f | wc -l)"
  guard
  ;;
start)
  pkill -f 'dufs .*dufsroot' 2>/dev/null; sleep 1
  setsid nohup "$H/dufs" "$ROOT" --bind 127.0.0.1 --port $PORT --allow-upload --allow-delete \
      > "$H/out/dufs.log" 2>&1 < /dev/null &
  for i in $(seq 40); do
    curl -fsS -m 2 -o /dev/null "http://127.0.0.1:$PORT/big1g.bin" -r 0-1 2>/dev/null && { echo "  dufs up on $PORT"; exit 0; }
    sleep 0.25
  done
  echo "  dufs FAILED to start: $(tail -3 "$H/out/dufs.log" 2>/dev/null)"; exit 1
  ;;
stop)
  pkill -f 'dufs .*dufsroot' 2>/dev/null; echo "  dufs stopped"
  ;;
clean)
  rm -rf "$ROOT/upload"; mkdir -p "$ROOT/upload"
  echo "  upload zone emptied; $(free_mb) MiB free"
  ;;
purge)
  pkill -f 'dufs .*dufsroot' 2>/dev/null
  rm -rf "$ROOT"
  echo "  corpus removed; $(free_mb) MiB free"
  ;;
status)
  echo "  dufs pids: $(pgrep -f 'dufs .*dufsroot' | tr '\n' ' ')"
  echo "  corpus: $(du -sh "$ROOT" 2>/dev/null | cut -f1)  free: $(free_mb) MiB"
  echo "  local read of the big file: $(curl -s -o /dev/null -m 30 -w '%{speed_download}' "http://127.0.0.1:$PORT/big1g.bin" | awk '{printf "%.0f MB/s", $1/1048576}')"
  ;;
esac
