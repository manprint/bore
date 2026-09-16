#!/usr/bin/env bash
# T-WEB-PACKAGE (6.4): the CRATE ARTIFACT serves the browser shell, with no
# Node on the machine that builds it.
#
# `bore` embeds `web/transfer/dist` at compile time, and the bundle is
# committed exactly so that `cargo install bore-cli` needs no npm. That claim
# is about the PACKAGED tree, not about this working copy: a file that
# `cargo package` leaves out compiles here and 404s there. So the gate packs
# the crate, checks the file list, builds from the packed source with the
# node tree removed, and then asks the resulting binary for an asset over
# HTTP and compares the bytes with the committed one.
set -euo pipefail
export LC_ALL=C

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
crate="bore-cli-$version"
echo "packaging $crate"

# `--allow-dirty` because the plan's tree is uncommitted by design during
# execution; `--no-verify` here because the verify build is run explicitly
# below, from the extracted source, which is what we want to observe.
list="$(cargo package --allow-dirty --no-verify --list)"

fail=0
need() {
  if printf '%s\n' "$list" | grep -qx "$1"; then
    printf 'PASS packaged %s\n' "$1"
  else
    printf 'FAIL missing from the crate: %s\n' "$1"
    fail=1
  fi
}
refuse() {
  if printf '%s\n' "$list" | grep -q "$1"; then
    printf 'FAIL the crate carries %s\n' "$1"
    fail=1
  else
    printf 'PASS no %s in the crate\n' "$1"
  fi
}

for asset in index.html app.js app.css offer-worker.js stage-worker.js; do
  need "web/transfer/dist/$asset"
done
refuse "node_modules"
refuse "web/transfer/test-results"
refuse "playwright-report"

[ "$fail" -eq 0 ] || { echo "T-WEB-PACKAGE: file list is wrong"; exit 1; }

# Build from the PACKAGED FILE SET, in a tree that has no node tree at all.
#
# NOT from a real `.crate`: this workspace has PATH dependencies with no
# published version (`bore-android-tun`, `bore-wintun`), so `cargo package`
# refuses to produce one — a repository-level fact, unrelated to web transfer,
# and not something this gate should paper over or silently fix. What it can
# still prove, and what the claim actually rests on, is that the file set
# `cargo package` WOULD ship is enough to build a server that serves the
# shell: the listed files are copied into a clean tree (plus the workspace
# member crates the manifest names, which a real publish would resolve from
# the registry), and the build runs there with no `node_modules` anywhere.
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
tree="$work/tree"
mkdir -p "$tree"
while IFS= read -r file; do
  case "$file" in
    Cargo.toml.orig | .cargo_vcs_info.json | Cargo.lock) continue ;;
  esac
  [ -f "$file" ] || continue
  mkdir -p "$tree/$(dirname "$file")"
  cp "$file" "$tree/$file"
done <<<"$list"
cp Cargo.lock "$tree/Cargo.lock" 2>/dev/null || true
cp -r crates "$tree/crates"
find "$tree/crates" -name node_modules -prune -exec rm -rf {} + 2>/dev/null || true

if [ -d "$tree/web/transfer/node_modules" ]; then
  echo "FAIL the packaged tree carries node_modules"
  exit 1
fi
echo "PASS the packaged tree has no node_modules"

( cd "$tree" && cargo build --bin bore 2>&1 | tail -3 )
bin="$tree/target/debug/bore"
test -x "$bin"

port=""
for candidate in $(seq 41000 41099); do
  if ! (exec 3<>"/dev/tcp/127.0.0.1/$candidate") 2>/dev/null; then
    port="$candidate"
    break
  fi
done
[ -n "$port" ] || { echo "T-WEB-PACKAGE: no free port"; exit 1; }

"$bin" server --control-port "$port" --web-transfer-base-url "http://127.0.0.1:$port/" \
  >"$work/server.log" 2>&1 &
server=$!
trap 'kill "$server" 2>/dev/null || true; rm -rf "$work"' EXIT
for _ in $(seq 1 100); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then break; fi
  sleep 0.1
done

# The asset the packed binary serves must be the asset in the repository,
# byte for byte: a stale embed is exactly what this gate exists to catch.
served="$work/app.js"
code="$(curl -s -o "$served" -w '%{http_code}' "http://127.0.0.1:$port/transfer/assets/app.js")"
if [ "$code" != "200" ]; then
  echo "FAIL the packaged binary answered $code for /transfer/assets/app.js"
  exit 1
fi
if cmp -s "$served" "web/transfer/dist/app.js"; then
  echo "PASS the packaged binary serves the committed bundle"
else
  echo "FAIL the packaged binary serves a different bundle"
  exit 1
fi

# And a room shell, which is the route a user opens.
room="$(printf 'a%.0s' $(seq 1 32))"
code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/transfer/$room")"
if [ "$code" = "200" ]; then
  echo "PASS the packaged binary serves a room shell"
else
  echo "FAIL the room shell answered $code"
  exit 1
fi
echo "T-WEB-PACKAGE: ok"
