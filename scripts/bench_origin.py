#!/usr/bin/env python3
"""Minimal, artifact-free HTTP origin for tunnel benchmarking.

Why not a general static server: dufs/python-http.server split head and body
across writes without TCP_NODELAY, so small responses pay the 40 ms
Nagle/delayed-ACK penalty on loopback. That penalty would show up as tunnel
latency. Here every response is ONE write on a NODELAY socket, from a
preallocated buffer, so the origin contributes ~0.1 ms.

Routes:
  GET  /b/<bytes>     body of <bytes> zeros (cached per size, capped at 1 GiB)
  GET  /stream/<bytes>  same, but streamed from a reused 1 MiB buffer: no
                      preallocation, so it is safe for multi-GB sustained runs
  GET  /1k /100k /1m /10m /200m   shorthands
  GET  /ping          2-byte body
  PUT|POST /sink      read and discard the body, reply 200 (upload tests)
Keep-alive is supported; Content-Length always set.
"""
import asyncio
import re
import sys

SIZES = {"/ping": 2, "/1k": 1024, "/100k": 102400, "/1m": 1048576,
         "/10m": 10485760, "/200m": 209715200}
_cache: dict[int, bytes] = {}
RE_B = re.compile(rb"^/b/(\d+)$")
RE_S = re.compile(rb"^/stream/(\d+)$")
CHUNK = b"\0" * (1 << 20)


def response(n: int) -> bytes:
    r = _cache.get(n)
    if r is None:
        head = (f"HTTP/1.1 200 OK\r\nContent-Length: {n}\r\n"
                f"Content-Type: application/octet-stream\r\n"
                f"Cache-Control: no-store\r\n\r\n").encode()
        r = head + b"\0" * n
        _cache[n] = r
    return r


NOT_FOUND = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"
OK_EMPTY = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"


async def handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    sock = writer.get_extra_info("socket")
    if sock is not None:
        import socket as _s
        sock.setsockopt(_s.IPPROTO_TCP, _s.TCP_NODELAY, 1)
    # Default high-water mark is 64 KiB: drain() after every 1 MiB write would
    # then block until the socket buffer is nearly empty, serialising the
    # producer against the kernel and capping /stream well below the link.
    tr = writer.transport
    if tr is not None:
        tr.set_write_buffer_limits(high=4 << 20, low=1 << 20)
    try:
        while True:
            head = await reader.readuntil(b"\r\n\r\n")
            first = head.split(b"\r\n", 1)[0].split(b" ")
            if len(first) < 2:
                break
            method, path = first[0], first[1]
            clen = 0
            for line in head.split(b"\r\n")[1:]:
                if line[:15].lower() == b"content-length:":
                    clen = int(line.split(b":", 1)[1].strip())
            if method in (b"PUT", b"POST"):
                # Answer Expect: 100-continue. Without it curl waits out its own
                # 1 s timeout before sending the body, which would be charged to
                # the tunnel. It also exercises interim-1xx relaying in the proxy.
                if b"\r\nexpect: 100-continue" in head.lower():
                    writer.write(b"HTTP/1.1 100 Continue\r\n\r\n")
                    await writer.drain()
                left = clen
                while left > 0:
                    chunk = await reader.read(min(left, 1 << 20))
                    if not chunk:
                        return
                    left -= len(chunk)
                writer.write(OK_EMPTY)
                await writer.drain()
                continue
            m = RE_S.match(path)
            if m:
                n = int(m.group(1))
                writer.write((f"HTTP/1.1 200 OK\r\nContent-Length: {n}\r\n"
                              f"Content-Type: application/octet-stream\r\n"
                              f"Cache-Control: no-store\r\n\r\n").encode())
                left = n
                while left > 0:
                    take = min(left, len(CHUNK))
                    writer.write(CHUNK[:take] if take != len(CHUNK) else CHUNK)
                    left -= take
                    await writer.drain()
                continue
            m = RE_B.match(path)
            if m:
                writer.write(response(min(int(m.group(1)), 1 << 30)))
            elif path.decode(errors="replace") in SIZES:
                writer.write(response(SIZES[path.decode()]))
            else:
                writer.write(NOT_FOUND)
            await writer.drain()
    except (asyncio.IncompleteReadError, ConnectionResetError, BrokenPipeError):
        pass
    finally:
        try:
            writer.close()
        except Exception:
            pass


async def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 5052
    for n in SIZES.values():
        response(n)          # prealloc so the first request is not slow
    # limit= sizes the StreamReader buffer. At the 64 KiB default the transport
    # pauses and resumes reading on every buffer refill, which caps PUT/POST
    # ingest far below the loopback rate and would be misread as tunnel cost.
    server = await asyncio.start_server(handle, "127.0.0.1", port,
                                        backlog=1024, limit=4 << 20)
    print(f"origin listening on 127.0.0.1:{port}", flush=True)
    async with server:
        await server.serve_forever()


asyncio.run(main())
