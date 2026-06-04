#!/usr/bin/env python3
"""Check reflexive UDP port via STUN.

Reports the public (reflexive) IP:port the NAT assigns for a given local port.

When --watch is used and the port is REMAPPED (NAT changed the port), the
script switches to ephemeral probes so it does NOT refresh the NAT mapping
on the target port, and periodically re-checks the target port to report
exactly when the NAT releases it.

Usage examples:
  python3 check-reflexive-ports.py --port 3478              # single shot
  python3 check-reflexive-ports.py --port 0                 # ephemeral
  python3 check-reflexive-ports.py --port 3478 --watch 5    # monitor release
"""

import argparse
import os
import socket
import struct
import sys
import time

MAGIC = 0x2112_A442
STUN_SERVERS = [
    ("stun.cloudflare.com", 3478),
    ("stun.l.google.com", 19302),
    ("stun1.l.google.com", 19302),
]


def parse_xor_mapped(data: bytes) -> tuple[str, int] | None:
    i = 20
    while i + 4 <= len(data):
        t, l = struct.unpack_from("!HH", data, i)
        if t == 0x0020:  # XOR-MAPPED-ADDRESS
            port = struct.unpack_from("!H", data, i + 6)[0] ^ (MAGIC >> 16)
            b0 = data[i + 8] ^ ((MAGIC >> 24) & 0xFF)
            b1 = data[i + 9] ^ ((MAGIC >> 16) & 0xFF)
            b2 = data[i + 10] ^ ((MAGIC >> 8) & 0xFF)
            b3 = data[i + 11] ^ (MAGIC & 0xFF)
            return f"{b0}.{b1}.{b2}.{b3}", port
        i += 4 + (l + 3) // 4 * 4
    return None


def probe(
    port: int, stun_host: str, stun_port: int, timeout: float = 3
) -> tuple[str, int] | str | None:
    """Bind local PORT, send STUN binding request.

    Returns:
      (ip, port)           reflexive address on success
      "still held"         port cannot be bound (local kernel not released)
      None                 STUN server did not respond
    """
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        if port != 0:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.settimeout(timeout)
        try:
            sock.bind(("0.0.0.0", port))
        except OSError:
            return "still held"
        tid = os.urandom(12)
        msg = struct.pack("!HH", 0x0001, 0) + struct.pack("!I", MAGIC) + tid
        sock.sendto(msg, (stun_host, stun_port))
        data, _ = sock.recvfrom(1024)
        if data[8:20] != tid:
            return None
        return parse_xor_mapped(data)
    except socket.timeout:
        return None
    finally:
        sock.close()


def print_result(ts: str, stun_host: str, stun_port: int, rip: str, rport: int,
                 local_port: int, tag: str, elapsed: float | None = None,
                 was_remapped: float | None = None) -> None:
    if local_port == 0:
        print(f"[{ts}] {stun_host}:{stun_port:<6} -> {rip}:{rport}  "
              f"(ephemeral -> public {rport})")
    elif rport == local_port:
        extra = ""
        if was_remapped is not None:
            extra = f" — released after {was_remapped:.0f}s"
        print(f"[{ts}] {stun_host}:{stun_port:<6} -> {rip}:{rport}  "
              f"(local {local_port} <- reflex, PRESERVED{extra})")
    else:
        extra = ""
        if elapsed is not None:
            extra = f" — waiting {elapsed:.0f}s so far"
        print(f"[{ts}] {stun_host}:{stun_port:<6} -> {rip}:{rport}  "
              f"(local {local_port} != reflex, REMAPPED {local_port} -> {rport}{extra})")


def main() -> None:
    ap = argparse.ArgumentParser(
        description="Check reflexive UDP port via STUN.",
    )
    ap.add_argument("--port", "-p", type=int, default=3478,
                    help="Local UDP port (0 = ephemeral). Default 3478.")
    ap.add_argument("--server", "-s", default=None,
                    help="STUN server host:port (default: Cloudflare+Google).")
    ap.add_argument("--timeout", "-t", type=float, default=3.0,
                    help="Per-probe timeout in seconds. Default 3.")
    ap.add_argument("--watch", "-w", type=float, default=0,
                    help="Repeat every W seconds (0 = single shot).")
    ap.add_argument("--count", "-c", type=int, default=0,
                    help="Stop after COUNT iterations (0 = infinite).")
    args = ap.parse_args()

    servers: list[tuple[str, int]] = []
    if args.server:
        parts = args.server.split(":")
        servers.append((parts[0], int(parts[1]) if len(parts) > 1 else 3478))
    else:
        servers = STUN_SERVERS

    # Once remapped is detected, use ephemeral port for probes
    # so we don't keep the NAT mapping alive, and only re-check
    # the target port every RECHECK_INTERVAL probes.
    RECHECK_INTERVAL = 3
    remapped_since: float | None = None
    recheck_counter = 0
    probe_port = args.port
    iteration = 0

    while True:
        iteration += 1
        if args.count > 0 and iteration > args.count:
            break

        # If we're in REMAPPED state, most probes use ephemeral to
        # avoid refreshing NAT; every RECHECK_INTERVAL we try the
        # actual target port to see if it's been released.
        if remapped_since is not None and args.port != 0:
            recheck_counter += 1
            probe_port = args.port if recheck_counter >= RECHECK_INTERVAL else 0
            if recheck_counter >= RECHECK_INTERVAL:
                recheck_counter = 0
        else:
            probe_port = args.port

        found = False
        still_held = False
        for shost, sport in servers:
            result = probe(probe_port, shost, sport, args.timeout)
            if result == "still held":
                still_held = True
                continue
            if result is None:
                continue
            found = True
            rip, rport = result
            break

        ts = time.strftime("%H:%M:%S")

        if not found:
            if still_held:
                print(f"[{ts}] port={args.port}  STILL HELD")
            else:
                print(f"[{ts}] port={probe_port}  ALL STUN SERVERS TIMED OUT")
        else:
            # Now interpret the result.
            # If we probed on ephemeral (0), we can only see the
            # current reflexive; we can't know about the target port.
            if probe_port == 0:
                # We're in the quiet period.  Just log.
                print(f"[{ts}] {shost}:{sport:<6} -> {rip}:{rport}  "
                      f"(ephemeral probe — not touching :{args.port} NAT entry)")
            else:
                tag = "ephemeral" if args.port == 0 else str(args.port)
                if rport == args.port:
                    # PRESERVED!
                    if remapped_since is not None:
                        elapsed = time.time() - remapped_since
                        print_result(ts, shost, sport, rip, rport, args.port,
                                     "PRESERVED", was_remapped=elapsed)
                        remapped_since = None
                    else:
                        print_result(ts, shost, sport, rip, rport, args.port,
                                     "PRESERVED")
                    recheck_counter = 0
                else:
                    # REMAPPED
                    if remapped_since is None:
                        remapped_since = time.time()
                    ela = time.time() - remapped_since
                    print_result(ts, shost, sport, rip, rport, args.port,
                                 "REMAPPED", elapsed=ela)

        if args.watch <= 0:
            break

        # Print wait message only for quiet periods > 5s
        if args.watch >= 5:
            pass  # just sleep
        sys.stdout.flush()
        time.sleep(args.watch)


if __name__ == "__main__":
    main()
