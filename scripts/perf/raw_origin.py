#!/usr/bin/env python3
"""Raw-TCP byte source/sink for public-tunnel benchmarking.

A public `bore local` tunnel forwards ARBITRARY TCP, not HTTP. Every other
origin in this harness speaks HTTP, so a measurement taken through one cannot
separate the tunnel from the HTTP parsing on either end. This origin speaks a
protocol with no framing beyond a single request line, so what it measures is
the tunnel and the kernel and nothing else.

Protocol (one request per connection, then close):
    GET <n>\\n   -> the server writes exactly <n> zero bytes, then closes
    PUT <n>\\n   -> the server reads exactly <n> bytes, then writes "OK\\n"
    PING\\n      -> the server writes "P\\n" immediately (round-trip probe)
    ECHO\\n      -> every byte received is written back until EOF
    HOLD <s>\\n  -> the server answers "H\\n" and then keeps the connection
                  open, idle, for <s> seconds

`HOLD` exists for the concurrency ladder. Measuring "what does a fresh
connection cost while N others are open" with N *busy* connections measures the
link, not the concurrency: the held arm must move no bytes at all.

Zeros are fine: nothing on the path compresses, and /dev/urandom at these
sizes is slower than the link.

    ./raw_origin.py [port]        default 5053
"""
import asyncio
import socket
import sys

CHUNK = b"\0" * (1 << 20)
MAX = 1 << 40


async def handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    sock = writer.get_extra_info("socket")
    if sock is not None:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    tr = writer.transport
    if tr is not None:
        # The default 64 KiB high-water mark makes drain() wait for a nearly
        # empty socket buffer, serialising the producer against the kernel and
        # capping a sustained write well below the link.
        tr.set_write_buffer_limits(high=4 << 20, low=1 << 20)
    try:
        line = await asyncio.wait_for(reader.readline(), timeout=30)
        if not line:
            return
        parts = line.strip().split()
        verb = parts[0].upper() if parts else b""

        if verb == b"PING":
            writer.write(b"P\n")
            await writer.drain()

        elif verb == b"ECHO":
            while True:
                buf = await reader.read(1 << 16)
                if not buf:
                    break
                writer.write(buf)
                await writer.drain()

        elif verb == b"HOLD" and len(parts) == 2:
            # Answer first so the client knows the connection is established
            # end to end (the tunnel opened its substream and the splice is
            # live), then go quiet. An unanswered HOLD would count connections
            # the server has not actually wired up yet.
            writer.write(b"H\n")
            await writer.drain()
            await asyncio.sleep(min(float(parts[1]), 3600.0))

        elif verb == b"GET" and len(parts) == 2:
            n = min(int(parts[1]), MAX)
            while n > 0:
                take = CHUNK if n >= len(CHUNK) else CHUNK[:n]
                writer.write(take)
                n -= len(take)
                await writer.drain()

        elif verb == b"PUT" and len(parts) == 2:
            n = int(parts[1])
            got = 0
            while got < n:
                buf = await reader.read(min(1 << 20, n - got))
                if not buf:
                    break
                got += len(buf)
            writer.write(b"OK %d\n" % got)
            await writer.drain()
    except (asyncio.IncompleteReadError, ConnectionResetError,
            BrokenPipeError, asyncio.TimeoutError, ValueError):
        pass
    finally:
        try:
            writer.close()
        except Exception:
            pass


async def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 5053
    server = await asyncio.start_server(handle, "127.0.0.1", port,
                                        backlog=4096, reuse_address=True)
    print(f"raw origin on 127.0.0.1:{port}", flush=True)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
