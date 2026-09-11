#!/usr/bin/env python3
"""Raw-TCP load driver for `raw_origin.py`, reached through a public tunnel.

Prints ONE line of machine-readable results so a shell harness can parse it
without jq:

    bytes=<total> secs=<wall> MBs=<MiB/s> Mbit=<Mbit/s> conns=<n> errs=<n>

Usage:
    raw_client.py get  <host> <port> <bytes-per-conn> [conns] [timeout]
    raw_client.py put  <host> <port> <bytes-per-conn> [conns] [timeout]
    raw_client.py ping <host> <port> <count>            [conns] [timeout]

`get`/`put` report aggregate throughput across `conns` parallel connections,
which is the only honest way to read a tunnel: a single TCP flow is bounded by
the Mathis relation and measures the path's RTT, not the tunnel.

`ping` opens one connection per probe (so it measures connection setup THROUGH
the tunnel, which is what a real client pays) and prints the latency
percentiles instead:

    n=<count> p50=<ms> p95=<ms> p99=<ms> max=<ms> errs=<n>
"""
import asyncio
import socket
import sys
import time

CHUNK = b"\0" * (1 << 20)


async def one_get(host, port, n, timeout):
    r, w = await asyncio.wait_for(asyncio.open_connection(host, port), timeout)
    s = w.get_extra_info("socket")
    if s is not None:
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    w.write(b"GET %d\n" % n)
    await w.drain()
    got = 0
    while got < n:
        buf = await r.read(1 << 16)
        if not buf:
            break
        got += len(buf)
    w.close()
    return got


async def one_put(host, port, n, timeout):
    r, w = await asyncio.wait_for(asyncio.open_connection(host, port), timeout)
    s = w.get_extra_info("socket")
    if s is not None:
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    tr = w.transport
    if tr is not None:
        tr.set_write_buffer_limits(high=4 << 20, low=1 << 20)
    w.write(b"PUT %d\n" % n)
    left = n
    while left > 0:
        take = CHUNK if left >= len(CHUNK) else CHUNK[:left]
        w.write(take)
        left -= len(take)
        await w.drain()
    ack = await asyncio.wait_for(r.readline(), timeout)
    w.close()
    # The origin echoes how many bytes it actually received; trust IT, not the
    # local write count, which only proves bytes left this process.
    try:
        return int(ack.split()[1])
    except Exception:
        return 0


async def one_ping(host, port, timeout):
    t0 = time.monotonic()
    r, w = await asyncio.wait_for(asyncio.open_connection(host, port), timeout)
    s = w.get_extra_info("socket")
    if s is not None:
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    w.write(b"PING\n")
    await w.drain()
    await asyncio.wait_for(r.readline(), timeout)
    w.close()
    return (time.monotonic() - t0) * 1000.0


def pct(vals, p):
    if not vals:
        return float("nan")
    v = sorted(vals)
    i = min(len(v) - 1, max(0, int(round((p / 100.0) * len(v) + 0.5)) - 1))
    return v[i]


async def main():
    mode = sys.argv[1]
    host = sys.argv[2]
    port = int(sys.argv[3])
    n = int(sys.argv[4])
    conns = int(sys.argv[5]) if len(sys.argv) > 5 else 1
    timeout = float(sys.argv[6]) if len(sys.argv) > 6 else 120.0

    if mode == "ping":
        lat, errs = [], 0
        # Serial on purpose: a latency probe that runs concurrently with itself
        # measures queueing, not latency.
        for _ in range(n):
            try:
                lat.append(await one_ping(host, port, timeout))
            except Exception:
                errs += 1
        print("n=%d p50=%.3f p95=%.3f p99=%.3f max=%.3f errs=%d" % (
            len(lat), pct(lat, 50), pct(lat, 95), pct(lat, 99),
            max(lat) if lat else float("nan"), errs))
        return

    fn = one_get if mode == "get" else one_put
    t0 = time.monotonic()
    res = await asyncio.gather(*[fn(host, port, n, timeout) for _ in range(conns)],
                               return_exceptions=True)
    secs = time.monotonic() - t0
    total = sum(x for x in res if isinstance(x, int))
    errs = sum(1 for x in res if not isinstance(x, int))
    print("bytes=%d secs=%.3f MBs=%.2f Mbit=%.0f conns=%d errs=%d" % (
        total, secs, total / 1048576.0 / secs, total * 8 / 1e6 / secs, conns, errs))


if __name__ == "__main__":
    asyncio.run(main())
