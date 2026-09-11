#!/usr/bin/env bash
# The in-region result the campaign could not explain: the TCP relay beats the
# QUIC direct path by 1.5-1.7x on download, and the direct path has a hard
# ~0.96 Gbit ceiling with CPU to match. A public tunnel's RELAY path carries
# PLAIN bytes over yamux over TCP — zero crypto per byte. The DIRECT path is
# QUIC: AEAD on every packet, at both ends. So the gap may simply be the cost
# of the cipher, and if it is, the lever is WHICH cipher.
#
# `client_config`/`server_config` build rustls with the ring provider and TLS
# 1.3 only, taking the provider's DEFAULT suite order — which puts
# AES-256-GCM first. Nothing about this path needs 256-bit: the certificate is
# not even verified (the token handshake authenticates the peer), and TLS 1.3
# AES-128-GCM is what every browser negotiates. So measure the two on the ARM
# hardware that actually runs this, before writing any code.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
probe='nproc; uname -m;
for c in aes-128-gcm aes-256-gcm chacha20-poly1305; do
  printf "%-20s " "$c";
  openssl speed -elapsed -evp $c 2>/dev/null | tail -1;
done'
echo "===== test VM ====="; vm "$probe" 2>/dev/null
echo "===== server ====="; srv "$probe" 2>/dev/null || echo "  (no server shell configured)"
