#!/usr/bin/env python3
"""A minimal TCP load instrument, for the one measurement iperf3 cannot make here.

WHY THIS EXISTS
---------------
`vpn_relay_attrib.sh` has to answer whether the VPN relay arm's deficit belongs
to bore's relay code or to the deployment it runs in -- a double transit through
a 2-vCPU t4g.micro.  The decisive control is the SAME two legs chained by a
relay that contains no bore: same hosts, same paths, product removed.  That
control needs a TCP relay and a load endpoint ON THE STAGING SERVER, and that
server has neither iperf3 nor socat.  It has python3 3.14 with `os.splice`.

THE INSTRUMENT MUST NOT BECOME THE MEASUREMENT
----------------------------------------------
A control relay that is itself the bottleneck would read as "the deployment is
the ceiling" when the truth is "the control was slow", which is the most
expensive way this stage could be wrong.  Two things keep that from happening:

  * the relay never touches the bytes.  Each direction is spliced socket -> pipe
    -> socket entirely inside the kernel, so Python schedules the transfer and
    copies none of it.  The pipe is enlarged to 1 MiB so a splice moves a useful
    quantum per syscall.
  * the harness measures a SINGLE hop through this same instrument before
    trusting the double hop.  If one hop already reaches the link rate, the
    instrument is not the limit; if it does not, the control is reported as a
    FLOOR and no attribution is claimed from it.

RECEIVER-SIDE TRUTH
-------------------
Throughput is counted where the bytes ARRIVE, never where they were handed to a
kernel -- the same reason `tcp_mbps` in vpnlib.sh reads iperf3's
`sum_received`.  On download the client counts what it read.  On upload the
endpoint counts what it read and returns the total after the client half-closes,
so a few megabytes still in flight in socket buffers cannot be reported as
delivered.

MODES
-----
  duplex PORT                 endpoint: per connection, obeys a one-byte header
                              -- b'D' means "send to the client", b'U' means
                              "receive from the client, then report the count".
  relay PORT DHOST DPORT      transparent splice relay, half-close propagated.
  client HOST PORT DIR SECS PAR   DIR is up|down.  Prints Mbit/s, one number.
"""

import os
import socket
import struct
import sys
import threading
import time

BUF = 1 << 20
PIPE_SZ = 1 << 20
F_SETPIPE_SZ = 1031  # Linux, not exposed by the fcntl module


def _tune(sock: socket.socket) -> None:
    try:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    except OSError:
        pass


def _serve_forever(port: int, handler) -> None:
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("0.0.0.0", port))
    listener.listen(128)
    while True:
        conn, _ = listener.accept()
        _tune(conn)
        threading.Thread(target=handler, args=(conn,), daemon=True).start()


# --- endpoint ---------------------------------------------------------------


def _duplex_conn(conn: socket.socket) -> None:
    try:
        head = conn.recv(1)
        if not head:
            return
        if head == b"D":
            payload = b"\0" * BUF
            while True:
                conn.sendall(payload)
        elif head == b"U":
            total = 0
            view = memoryview(bytearray(BUF))
            while True:
                n = conn.recv_into(view, BUF)
                if n == 0:
                    break
                total += n
            # The client is waiting for this: it is the only receiver-side
            # figure, and without it an upload reports bytes that are still in
            # a socket buffer as bytes that arrived.
            conn.sendall(struct.pack("!Q", total))
    except OSError:
        pass
    finally:
        try:
            conn.close()
        except OSError:
            pass


# --- relay ------------------------------------------------------------------


def _pump_splice(src: socket.socket, dst: socket.socket) -> None:
    read_fd, write_fd = os.pipe()
    try:
        try:
            import fcntl

            fcntl.fcntl(write_fd, F_SETPIPE_SZ, PIPE_SZ)
        except (OSError, ImportError):
            pass
        while True:
            n = os.splice(src.fileno(), write_fd, PIPE_SZ)
            if n == 0:
                break
            while n:
                n -= os.splice(read_fd, dst.fileno(), n)
    except OSError:
        pass
    finally:
        os.close(read_fd)
        os.close(write_fd)
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def _relay_conn(conn: socket.socket, dst_host: str, dst_port: int) -> None:
    try:
        upstream = socket.create_connection((dst_host, dst_port))
    except OSError:
        conn.close()
        return
    _tune(upstream)
    back = threading.Thread(target=_pump_splice, args=(upstream, conn), daemon=True)
    back.start()
    _pump_splice(conn, upstream)
    back.join()
    for sock in (conn, upstream):
        try:
            sock.close()
        except OSError:
            pass


# --- client -----------------------------------------------------------------


def _client_stream(host: str, port: int, direction: str, deadline: float, out: list, idx: int) -> None:
    try:
        conn = socket.create_connection((host, port))
    except OSError:
        out[idx] = 0
        return
    _tune(conn)
    total = 0
    try:
        if direction == "down":
            conn.sendall(b"D")
            view = memoryview(bytearray(BUF))
            while time.monotonic() < deadline:
                n = conn.recv_into(view, BUF)
                if n == 0:
                    break
                total += n
        else:
            conn.sendall(b"U")
            payload = b"\0" * BUF
            while time.monotonic() < deadline:
                conn.sendall(payload)
            conn.shutdown(socket.SHUT_WR)
            # Receiver-side truth, as above.
            acc = b""
            while len(acc) < 8:
                chunk = conn.recv(8 - len(acc))
                if not chunk:
                    break
                acc += chunk
            total = struct.unpack("!Q", acc)[0] if len(acc) == 8 else 0
    except OSError:
        pass
    finally:
        try:
            conn.close()
        except OSError:
            pass
    out[idx] = total


def _client(host: str, port: int, direction: str, secs: float, par: int) -> None:
    out = [0] * par
    start = time.monotonic()
    deadline = start + secs
    threads = [
        threading.Thread(target=_client_stream, args=(host, port, direction, deadline, out, i))
        for i in range(par)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    elapsed = time.monotonic() - start
    if elapsed <= 0:
        print("0")
        return
    print("%.2f" % (sum(out) * 8 / 1e6 / elapsed))


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    mode = sys.argv[1]
    if mode == "duplex":
        _serve_forever(int(sys.argv[2]), _duplex_conn)
    elif mode == "relay":
        port, dst_host, dst_port = int(sys.argv[2]), sys.argv[3], int(sys.argv[4])
        _serve_forever(port, lambda c: _relay_conn(c, dst_host, dst_port))
    elif mode == "client":
        _client(sys.argv[2], int(sys.argv[3]), sys.argv[4], float(sys.argv[5]), int(sys.argv[6]))
    else:
        print("unknown mode %r" % mode, file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
