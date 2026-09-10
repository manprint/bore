#!/usr/bin/env bash
# Render the JSONL produced by vhost_staging_bench.sh as markdown tables.
# Usage: scripts/vhost_staging_report.sh <results.jsonl> [more.jsonl...]
set -euo pipefail
[ $# -ge 1 ] || { echo "usage: $0 <results.jsonl>..." >&2; exit 1; }

cat "$@" | jq -s -r '
  def f(x): if x == null then "—" else (x|tostring) end;
  def ms(x): if x == null then "—" else ((x*10|round)/10|tostring) end;
  ( "### Latency (ms, p50 / p95 / p99)\n",
    "| case | path | carriers | keep-alive c=1 | keep-alive c=8 | keep-alive c=32 | new conn c=8 | asset 100k c=8 | under bulk c=8 |",
    "| --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ( .[] | "| \(.case) | \(.path) | \(.carriers) | "
        + "\(ms(.lat_ka_c1.p50)) / \(ms(.lat_ka_c1.p95)) / \(ms(.lat_ka_c1.p99)) | "
        + "\(ms(.lat_ka_c8.p50)) / \(ms(.lat_ka_c8.p95)) / \(ms(.lat_ka_c8.p99)) | "
        + "\(ms(.lat_ka_c32.p50)) / \(ms(.lat_ka_c32.p95)) / \(ms(.lat_ka_c32.p99)) | "
        + "\(ms(.lat_newconn_c8.p50)) / \(ms(.lat_newconn_c8.p95)) / \(ms(.lat_newconn_c8.p99)) | "
        + "\(ms(.asset_100k_c8.p50)) / \(ms(.asset_100k_c8.p95)) / \(ms(.asset_100k_c8.p99)) | "
        + "\(ms(.lat_under_bulk_c8.p50)) / \(ms(.lat_under_bulk_c8.p95)) / \(ms(.lat_under_bulk_c8.p99)) |" ),
    "",
    "### Throughput and load (MB/s, requests/s)\n",
    "| case | path | carriers | rps c=8 | rps c=32 | bulk 200MB | parallel 8x10MB | upload 32MB | server tx peak | server RSS peak | client warns |",
    "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ( .[] | "| \(.case) | \(.path) | \(.carriers) | \(f(.lat_ka_c8.rps)) | \(f(.lat_ka_c32.rps)) | "
        + "\(f(.bulk_single.mb_s)) | \(f(.bulk_par8.mb_s)) | \(f(.upload_32m.mb_s)) | "
        + "\(f(.server_rate.tx_mb_max)) | \(if .server_peak.rss_max == null then "—" else ((.server_peak.rss_max/1048576*10|round)/10|tostring) + " MiB" end) | \(f(.client_warns)) |" ),
    "",
    "### Sustained runs\n",
    "| case | path | seconds | server tx mean | server tx peak |",
    "| --- | --- | --- | --- | --- |",
    ( .[] | select(.sustained != null) |
      "| \(.case) | \(.path) | \(f(.wall_s)) | \(f(.sustained.tx_mb_mean)) | \(f(.sustained.tx_mb_max)) |" )
  )'
