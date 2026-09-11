#!/usr/bin/env bash
# Host resource sampler. Runs ON the host being sampled (server, VM, or this
# workstation) and writes one line per interval. Two things are captured that a
# container's own accounting misses, and both mattered in the original campaign:
#   * the host CPU busy split including softirq and STEAL (§2.10 measured
#     softirq at 46-55 % of the bill, and steal is the only in-guest evidence of
#     burstable-credit throttling)
#   * per-process CPU and RSS for the processes that actually move the bytes,
#     so "the server used a core" can be attributed to bore rather than to the
#     kernel or to a neighbour.
#
# usage: res_sampler.sh <seconds> <interval> <outprefix> [process-regex]
set -u
N="${1:-120}"; IV="${2:-2}"; OUT="${3:-/tmp/res}"; RE="${4:-bore|dufs|curl|oha|python3}"
: > "$OUT.stat"; : > "$OUT.proc"
for i in $(seq "$N"); do
  ts=$(date +%s)
  read -r _ u ni sy id io irq sirq st _rest < /proc/stat
  ld=$(cut -d' ' -f1 /proc/loadavg)
  mt=$(awk '/^MemTotal:/{print $2}' /proc/meminfo)
  ma=$(awk '/^MemAvailable:/{print $2}' /proc/meminfo)
  printf '%s %s %s %s %s %s %s %s %s %s %s %s\n' "$ts" "$u" "$ni" "$sy" "$id" "$io" "$irq" "$sirq" "$st" "$ld" "$mt" "$ma" >> "$OUT.stat"
  # %cpu here is the process' lifetime average, so the reducer differentiates
  # cputime (TIME) instead; RSS is instantaneous and used as-is.
  ps -eo pid=,rss=,cputimes=,comm= 2>/dev/null | awk -v ts="$ts" -v re="$RE" '$4 ~ re {print ts, $1, $2, $3, $4}' >> "$OUT.proc"
  sleep "$IV"
done
