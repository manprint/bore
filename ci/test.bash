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
#
# Test scope depends on whether the target runs natively or under emulation:
#   - x86_64-* : native arch on the x86_64 runner. Run the FULL suite, including
#     the tests/ CLI integration tests that spawn the compiled `bore` binary.
#   - everything else (aarch64, arm, armv7, i686, ...) : `cross` runs the test
#     harness under QEMU userspace emulation. Exec'ing the foreign-arch `bore`
#     binary as a child process fails with ENOEXEC ("Exec format error", os
#     error 8), so the subprocess-spawning CLI integration tests cannot run.
#     Restrict to the in-process library unit tests (`--lib`); the integration
#     tests run natively on x86_64 here and across the host OSes in ci.yml.
TEST_SCOPE=""
case "$TARGET_TRIPLE" in
    x86_64-*) ;;
    *) TEST_SCOPE="--lib" ;;
esac

max_attempts=3
count=0
status=1

# NOTE: the test invocation MUST sit in an `if` condition. On its own line it
# would trip `set -e` (line 3) on the first non-zero exit and abort the script
# before the retry/status logic ever ran — silently defeating the retry loop.
while [ $count -lt $max_attempts ]; do
    if $CROSS test --target $TARGET_TRIPLE --no-default-features $TEST_SCOPE; then
        status=0
        echo "Test passed"
        break
    fi
    status=$?
    echo "Test failed, attempt $(($count + 1))"
    count=$(($count + 1))
done

if [ $status -ne 0 ]; then
    echo "Test failed after $max_attempts attempts"
    exit $status
fi
