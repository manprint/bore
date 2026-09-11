#!/usr/bin/env python3
"""Bring the staging compose to the state the 2026-09-11 measurements support,
and annotate every value that deviates from a bore default with the default,
the reason and the measurement that justifies it.

Decisions encoded here, each backed by a number in the evidence document:
  * BORE_PROXY_BUFFER_SIZE=128KiB  -> REMOVED (back to the 256KiB default).
    Measured A/B/A on the 40ms path: 25.57 / 28.53 / 29.67 MB/s, i.e. the two
    identical 128KiB arms differ by 16% while the 256KiB arm sits between them.
    No effect; the deviation only costs clarity.
  * BORE_UDP_MEMORY_BUDGET=512MiB  -> KEPT.  Cuts peak server RSS from 340.4 to
    42.2 MiB with 48 slow readers, and costs nothing on throughput even from the
    domestic consumer where the derived 1MiB stream window is the same order as
    the bandwidth-delay product (ON 53.16/56.25 vs OFF 52.28/49.72 MB/s).
"""
import sys

p = sys.argv[1]
s = open(p).read()


def sub(old, new, required=True):
    global s
    if old not in s:
        if required:
            print("WARN anchor missing:", repr(old[:60]))
        return
    s = s.replace(old, new, 1)


# --- 1. proxy buffer: drop the override, keep the finding on the record ----
sub("      # perf-campaign 2026-09-11: proxy-buffer A/B/A, 2026-09-11\n"
    "      - BORE_PROXY_BUFFER_SIZE=128KiB\n",
    "      # BORE_PROXY_BUFFER_SIZE: intentionally NOT set -> the built-in default of\n"
    "      # 256KiB applies (shared.rs DEFAULT_PROXY_BUFFER_SIZE, clamped to [4KiB,16MiB]).\n"
    "      # This deployment used to force 128KiB. Measured A/B/A on 2026-09-11 over a\n"
    "      # 40ms path, which is where a copy buffer should bite: 128KiB -> 25.57 MB/s,\n"
    "      # 256KiB -> 28.53, 128KiB again -> 29.67. The two identical arms differ by 16%,\n"
    "      # so the parameter has no effect larger than the drift of the path. Removed so\n"
    "      # the deployment matches the tested default. Do NOT set it small: the buffer\n"
    "      # exists because an 8KiB copy loop is a known high-latency regression.\n"
    "      # Resolved value is readable at GET /admin/api/v1/config -> proxy_buffer_size\n"
    "      # (needs a server image from 2026-09-11 or later).\n")

# --- 2. memory budget: keep it, say why ------------------------------------
sub("      - BORE_UDP_MAX_STREAMS=8192\n",
    "      # DEVIATION: default is 4096. Doubled so one direct QUIC connection can carry\n"
    "      # more concurrent proxied connections before the stream limit binds.\n"
    "      - BORE_UDP_MAX_STREAMS=8192\n"
    "      # KEPT after measurement 2026-09-11 (F-13, the direct-UDP aggregate bound).\n"
    "      # WHY: without it, tunnels x carriers x connection_receive_window has no\n"
    "      # server-wide ceiling. With 48 slow readers on one --udp tunnel, peak server\n"
    "      # RSS went 340.4 MiB -> 42.2 MiB on this 903 MiB host, and requests were still\n"
    "      # served instead of the host starving.\n"
    "      # COST: none measurable. Because BORE_MAX_CARRIERS is 1024 (see above), the\n"
    "      # derived per-connection window lands on its 16MiB floor and the stream\n"
    "      # window on 1MiB.\n"
    "      # That 1MiB is the same order as a domestic consumer's bandwidth-delay product\n"
    "      # (19.5ms x ~400Mbit/s = ~975KiB), which is the one place it could hurt, so it\n"
    "      # was measured there: budget ON 53.16/56.25 MB/s vs OFF 52.28/49.72 MB/s,\n"
    "      # QUIC direct, A/B/A/B, unshaped. Refusals (if any) appear as\n"
    "      # direct_budget_refusals on /admin/api/v1/metrics; a refused connection stays\n"
    "      # on the warm TCP relay, it is never a failed request.\n"
    "      - BORE_UDP_MEMORY_BUDGET=512MiB\n")

# --- 3. the two remaining deviating values ---------------------------------
sub("      - BORE_MAX_CONNS=1024\n",
    "      # = the built-in default (DEFAULT_MAX_CONNS); kept explicit for clarity.\n"
    "      - BORE_MAX_CONNS=1024\n")

sub("      - BORE_MAX_CARRIERS=1024\n",
    "      # DEVIATION: default is 16. 1024 lifts the server-side ceiling so a client may\n"
    "      # ask for any carrier count it likes.\n"
    "      # CAVEAT measured 2026-09-11: --udp-memory-budget derives its per-connection\n"
    "      # window as clamp(budget/max_carriers, 16MiB, 256MiB), so at 1024 ANY practical\n"
    "      # budget lands on the 16MiB floor and then buys only admission slots, never\n"
    "      # windows. Measured harmless here (see BORE_UDP_MEMORY_BUDGET below), but lower\n"
    "      # this if you ever want the budget's window arithmetic to behave as documented.\n"
    "      - BORE_MAX_CARRIERS=1024\n")

# --- 4. the commented block that contradicted an active setting ------------
sub("# leave commented for max performance (sono i default ottimali)\n",
    "# The five UDP window/buffer values below are EXACTLY the built-in defaults, verified\n"
    "# against /admin/api/v1/config on 2026-09-11 (16MiB / 256MiB / 256MiB / 16MiB / 16MiB).\n"
    "# Leaving them commented is therefore identical to setting them, and is preferred so a\n"
    "# future change to the tested defaults is picked up automatically.\n"
    "# NOTE: the first THREE (the two receive windows and the send window) also CONFLICT\n"
    "# with BORE_UDP_MEMORY_BUDGET above (clap conflicts_with_all) -- setting any of those\n"
    "# three while the budget is set makes the server refuse to start. The two socket\n"
    "# buffers and BORE_UDP_MAX_STREAMS do not conflict.\n")

sub("#      - BORE_UDP_MAX_STREAMS=4096\n",
    "#      - BORE_UDP_MAX_STREAMS=4096   # <- this IS the default, but the ACTIVE setting\n"
    "#                                    #    above overrides it with 8192. Kept here only\n"
    "#                                    #    to record what the default is.\n")

open(p, "w").write(s)
print("annotated")
