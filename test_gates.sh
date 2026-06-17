#!/bin/bash
set -e

cd /mnt/fabio/dati/Git/Github-manprint/bore-forked

echo "=== cargo fmt --all ==="
cargo fmt --all
echo "PASS"

echo "=== cargo fmt --all --check ==="
cargo fmt --all --check
echo "PASS"

echo "=== cargo clippy --all-features --all-targets ==="
cargo clippy --all-features --all-targets -- -D warnings
echo "PASS"

echo "=== cargo build --all-features ==="
cargo build --all-features
echo "PASS"

echo "=== cargo build --no-default-features ==="
cargo build --no-default-features
echo "PASS"

echo "=== cargo build --features vpn ==="
cargo build --features vpn
echo "PASS"

echo "=== cargo test --all-features ==="
cargo test --all-features
echo "PASS"

echo "ALL GATES PASSED"
