#!/usr/bin/env python3
# Probe the exact kernel UDP demux behavior behind the bore port-collision bug.
# Mirrors holepunch::bind_socket: wildcard 0.0.0.0:<port>, SO_REUSEADDR (no REUSEPORT).
import socket, sys, os, time, errno

PORT = 54545  # arbitrary high port; behavior is port-independent

def mk(reuseaddr=False, reuseport=False, connect_to=None):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    if reuseaddr:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    if reuseport:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
    s.bind(("0.0.0.0", PORT))
    if connect_to:
        s.connect(connect_to)
    s.setblocking(False)
    return s

def drain(s):
    got = []
    for _ in range(20):
        try:
            d, src = s.recvfrom(2048)
            got.append((d.decode(errors="replace"), src))
        except BlockingIOError:
            break
    return got

def case(name, **kw_second):
    print(f"\n=== {name} ===")
    try:
        a = mk(reuseaddr=True)  # first binder (like the live tunnel)
    except OSError as e:
        print(f"  socket A bind FAILED: {e}"); return
    try:
        b = mk(**kw_second)     # second binder (the colliding tunnel re-punch)
    except OSError as e:
        print(f"  socket B bind FAILED ({errno.errorcode.get(e.errno,e.errno)}: {e}) "
              f"=> second tunnel CANNOT steal; would fall back to ephemeral. GOOD.")
        a.close(); return
    print("  both A and B bound 0.0.0.0:%d simultaneously" % PORT)
    # sender → 127.0.0.1:PORT
    tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    for i in range(5):
        tx.sendto(f"pkt{i}".encode(), ("127.0.0.1", PORT))
    time.sleep(0.2)
    ga, gb = drain(a), drain(b)
    print(f"  A received: {[g[0] for g in ga]}")
    print(f"  B received: {[g[0] for g in gb]}")
    if ga and not gb:
        print("  => delivered to A (first). ")
    elif gb and not ga:
        print("  => delivered to B (LAST binder) => STEAL confirmed: a re-punch starves the live tunnel.")
    elif ga and gb:
        print("  => load-balanced across both (REUSEPORT-style).")
    else:
        print("  => neither got it (unexpected).")
    tx.close(); a.close(); b.close()

print("kernel:", os.uname().release)
case("A=REUSEADDR, B=REUSEADDR (current bore behavior)", reuseaddr=True)
case("A=REUSEADDR, B=plain (no opts) — does plain bind get refused?")
case("A=REUSEADDR, B=REUSEPORT", reuseport=True)
