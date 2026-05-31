#!/usr/bin/env bash
# Script for building your rust projects.
set -e

source ci/common.bash

# $1 {path} = Path to cross/cargo executable
CROSS=$1
# $1 {string} = <Target Triple>
TARGET_TRIPLE=$2

required_arg $CROSS 'CROSS'
required_arg $TARGET_TRIPLE '<Target Triple>'

# The `udp` feature (QUIC) is built per target by ci/build.bash, but its e2e
# test does a real QUIC handshake on loopback — flaky under qemu emulation. Run
# the cross test suite relay-only (--no-default-features); the udp path is tested
# natively on the host in ci.yml (--all-features).
max_attempts=3
count=0

while [ $count -lt $max_attempts ]; do
    $CROSS test --target $TARGET_TRIPLE --no-default-features
    status=$?
    if [ $status -eq 0 ]; then
        echo "Test passed"
        break
    else
        echo "Test failed, attempt $(($count + 1))"
    fi
    count=$(($count + 1))
done

if [ $status -ne 0 ]; then
    echo "Test failed after $max_attempts attempts"
fi
