# Public-tunnel performance harness

Everything needed to re-run the public-tunnel campaign from scratch, on this
deployment or on a different one. Nothing here hardcodes a host, a port, a
domain or a credential: the whole harness is re-pointed by editing one
`env.sh`.

> **Credentials never live in this repository.** `env.sh` is created from
> `../env.sh.example`, stored at `~/.config/bore-perf/env.sh` with mode 600,
> and derived onto the test VM as `~/env.sh` (also 600) by `provision.sh`. The
> values themselves are provided separately.

## Layout

| file | runs on | what it answers |
| --- | --- | --- |
| `publib.sh` | VM | shared plumbing: admin API for `/tunnels`, tunnel lifecycle, raw-TCP measurement primitives, cooldown |
| `vm_pub_ab.sh` | VM | P1 paired relay-vs-QUIC, P2 carrier ladder, P3 latency, P4 HTTP-over-public |
| `vm_pub_flavours.sh relay` | VM | P5 native vs dockerized binary vs OpenSSH `-R`, all on the TCP relay, rotated and budget-neutral |
| `vm_pub_flavours.sh udp` | VM | P5b native vs dockerized binary, both on the QUIC direct path. Separate mode because Docker clears every capability for a non-root UID, so the flavour answer cannot be inherited from the relay result — and because an SSH forward has no QUIC arm to compare (I-SSH2) |
| `vm_pub_conc.sh` | VM | P6 concurrency ladder: what a fresh connection costs behind N held ones |
| `vm_pub_netem.sh` | VM | P8 loss / delay / reorder matrix, both transports under the SAME shaping |
| `vm_pub_eff.sh` | VM | P9 CPU seconds per GiB, the application-limit question |
| `vm_pub_stab.sh` | VM | P7 soak, direct-path recovery, port release, wedged-client reap, churn |
| `pub_driver.sh` | VM | runs the stages strictly serially with timestamps |
| `ws_pub.sh` | workstation | the real-world topology: consumer outside AWS |

Plus one reducer, on the workstation:

| file | what it answers |
| --- | --- |
| `../res/cpu_window.sh <prefix.stat> <t0> <t1> [gib] [prefix.proc]` | the CPU seconds a host spent inside one measurement window, and CPU s/GiB when told how much was moved. Pair it with the `window=<t0>-<t1> ... gib=` line `vm_pub_eff.sh` prints per case. |

Two origins, both started automatically:

* `raw_origin.py` (port 5053) — **raw TCP**, one request line then bytes. A
  public tunnel forwards arbitrary TCP, so this is what most arms measure: no
  HTTP parsing on either end, and the origin itself contributes ~0.1 ms and
  ~19 Gbit/s on loopback. Verbs: `GET n`, `PUT n`, `PING`, `ECHO`, and `HOLD s`
  — the last one answers once and then moves no bytes at all, which is what the
  concurrency ladder needs (held connections that *download* measure the link,
  not the concurrency). `HOLD` parks on a READ, never on a sleep: a rung ends
  by killing its driver, and an origin that slept through that kept its own
  half of every connection open for the rest of the hold, so the server stayed
  busy with the previous rung and the next rung measured the SUM (defect H-9 —
  `active_at_server` read 80 / 208 / 464 at the 64 / 128 / 256 rungs, each the
  running total). A read returns `b""` the instant the peer goes away.
* `bench_origin.py` (port 5052) — HTTP, used only by the P4 arm so the numbers
  can be compared with the vhost campaign.

An origin is deliberately REUSED across stages (restarting it between stages
re-warms the page cache and moves the numbers), but `start_origins` first calls
`reap_stale_origin`, which kills the running origin — that PID only, never a
pattern-wide `pkill` — when the **process is older than its own script file**.
This is defect H-7: a long-lived `raw_origin.py` started before the `HOLD` verb
was added answered every `HOLD` with an error, so the whole concurrency ladder
reported `held=N up=0 errs=N` while looking like it had run. `pgrep` cannot
tell "a process matching this name" from "a process running this program".
`ws_pub.sh` applies the same rule to the origin it starts remotely.

`ws_pub.sh` also has a trap of its own, defect H-12, and the shape of the fix
matters because the obvious one does not work. Its origin lives on the VM, so
the idempotency check runs REMOTELY through `vm "..."` — and the remote command
string contains the very name being searched for, so `pgrep -f raw_origin.py`
matches the `bash -c` that is running it. Bracketing the pattern
(`raw_origin.p[y]`), which is the fix everywhere else in this harness, is only
HALF a fix here and was measured to be: the start command in the same one-liner
still contains the plain `raw_origin.py`, so the `pgrep` keeps self-matching and
the origin is never started. The stage then tunnels a local port with nothing
behind it and reads `0.00 MB/s` on every arm — H-7's shape again, in the exact
format of a real measurement. Two changes close it for good:

* the existence check is a real connection to the port
  (`timeout 2 bash -c '</dev/tcp/127.0.0.1/$RP'`), which is the question the
  stage actually needs answered and is immune to the whole class;
* the warm-up is a **parsed preflight**: it reads `bytes=` back from both
  transports and exits 2 with the diagnosis rather than measuring. A registered
  tunnel proves the CONTROL path and says nothing about the DATA path, so
  `PREFLIGHT FAILED on port 9021: registered, but it moved no bytes` is the
  line to expect when the origin is down — never a table of zeros.

The reaper in the same block still needs a PID and still uses a bracketed
`pgrep`; that is safe because its own command text contains `raw_origin.py)`
from the `stat` and never `raw_origin.py $RP`. If you add a line there, check
what your own command string contains before trusting a remote `pgrep`.

The ladder also refuses to trust itself: `wait_quiet` waits for the SERVER's
own `active` count to fall back to zero between rungs and prints a warning
when it does not, so a rung can never quietly publish a cumulative number
again.

## Running it

```bash
# once, from the workstation
scripts/perf/staging/provision.sh              # pushes pub/, both origins, the binary

# the full VM-side campaign (hours; strictly serial by design)
ssh <vm> '~/pub/pub_driver.sh'                 # or: ~/pub/pub_driver.sh p1 conc eff
#   stages: p1 p2 p3 p4 flavours flavours_udp conc netem eff stab soak

# the real-world topology, from the workstation
scripts/perf/staging/pub/ws_pub.sh

# ONE stage again, afterwards, with everything it needs (see below)
scripts/perf/staging/pub/rerun_stage.sh eff
DEST=out/pub-20260911-055454 scripts/perf/staging/pub/rerun_stage.sh conc eff

# resource sampling on all three actors, around a stage
scripts/perf/staging/res/start_samplers.sh
scripts/perf/staging/res/ena_timeline.sh 3600 > out/ena.timeline &
#   ... run the stage ...
scripts/perf/staging/res/stop_samplers.sh
```

## The follow-up probes

Eight small scripts that exist because a headline number was not believed.
They are not part of the campaign sweep — each one answers ONE question, and
each is named in the evidence document where its answer is used, so a reader
can re-run the thing that produced a claim rather than trusting the claim.

| script | the question it answers | what it found |
| --- | --- | --- |
| `pktrate.sh` | the direct path costs 2.9× the kernel softirq per delivered GiB — does it really move 3× the packets? | No: 739 363 pkt/GiB against the relay's 764 075. But it takes IN 3.885 GiB to hand OUT 2.279 (1.78×) where the relay's agree to 0.2 % — retransmission, which is how P-13 was found (§13.1) |
| `ws_asym.sh` | the workstation pulls at ~27 MB/s and pushes at ~71 — is that the instance's outbound allowance shaping the download? | No: `bw_in_allowance_exceeded` and `bw_out_allowance_exceeded` both moved by 0 in both arms. The asymmetry is the radio link's own, and the campaign's dominant confounder is absent in this topology |
| `ws_carr.sh` | is the relay's download deficit to a distant consumer single-carrier head-of-line? | No: c4/c1 = 0.754 / 0.889 / 1.027, median 0.889. Carriers cost, same sign as in-region |
| `ws_conns.sh` | is there a per-connection window bound? Then the aggregate must scale with connections | No: the aggregate is flat (35.23 at one connection, 27.92 at four) and per-connection falls as 1/N. One connection already saturates the link |
| `ws_dl.sh` | four download pairs favouring the direct path is a sign test at p=0.0625 — does it hold over eight? | Ratios spanned 0.694-2.607. The direction held (7 of 8, p=0.035), the magnitude is unresolvable |
| `ws_dl1.sh` | if one connection saturates the link, and the consumer hop is TCP either way, the transport cannot matter — so does the reversal survive at one connection? | No: median 0.943 over six pairs, three above one and three below. The reversal was the four-connection regime, not the transport |
| `mtu.sh` | quinn caps its MTU search at 1452 — is the VM↔server path jumbo-capable, making that a lever? | No: `tracepath` reports `pmtu 1500`. Raising the ceiling would buy 1.4 %. Lever rejected before any code was written |
| `aead.sh` | the relay carries plain bytes and the direct path runs AEAD per packet — is the cipher the gap, and is AES-256 the wrong default? | AES-128 is 14 % faster than AES-256 at QUIC packet sizes on the server's aarch64, but AEAD is only ~0.88 CPU s/GiB of the direct path's 13.58. Worth 0.8 %. Lever rejected |

Two of the eight found something; six killed a hypothesis. That ratio is the
point — the two that found something were only reachable because the six ruled
out everything cheaper.

## Reading the results, and the traps that make them wrong

**The instance network allowance is the dominant confounder.** One 4-stream
10 s download is roughly a whole inbound burst budget on this instance class.
Consequences, all of them learned the hard way in the vhost campaign:

* every arm waits `COOL` (75 s by default) after its burst;
* comparisons are **paired** — both transports measured back to back, the
  ratio reported, the order alternating — because the control arm drifted 29 %
  over a few minutes;
* the flavour comparison registers all three forwarders **at once** and hits
  them in rotation, because a sequential ladder charges the whole budget to
  whichever arm runs first;
* `res/ena_timeline.sh` samples the counters for the whole run so a shaped
  burst can be identified and discarded rather than silently averaged in.

**MB/s cannot answer "is the server application-limited?"** on a burstable
instance, because the bucket caps the rate long before the CPU does.
`vm_pub_eff.sh` answers it in CPU seconds per GiB, which is invariant to the
cap — and it prints an epoch window per case so the HOST-side sampler can be
matched to it, because the container's own CPU% misses the softirq the host
kernel spends on its behalf (37–40 % of the bill).

**The direct path is confirmed, never assumed.** Every arm reads
`current_path`, `direct_stream_opens`, `direct_fallbacks` and `direct_pool`
from `/admin/api/v1/tunnels` around the measurement. An arm that asked for
`--udp` and ran on the relay is reported as a relay number and must not be
quoted as a QUIC one.

**The SSH leg is TCP-relay-only by design** (I-SSH2: no `--udp`, no
`--carriers>1`). It is compared against the native and docker RELAY arms,
never against their QUIC arms.

**Never `pkill bore`.** This deployment carries live tunnels belonging to the
operator. Every teardown in this harness matches an exact port or container
name.

**Hold the concurrency ladder from ONE process.** `raw_client.py hold` opens
all N connections with asyncio inside a single interpreter. One OS process per
connection puts 512 Python interpreters on a 2-vCPU / 3.8 GiB VM and measures
the driver's own memory pressure. The arm also prints `up=` (how many the
driver got answered) beside `active_at_server=` (how many the server reports),
because a ladder that silently held fewer connections than it claims is worse
than no ladder.

**The workstation and VM numbers are not comparable.** The workstation's radio
link caps the path well below what the in-region VM sees, so a lower number
there means the link. Use the workstation topology for latency, for transport
RATIOS, and for proving the path works from outside AWS.

## Re-running one stage afterwards

`run_campaign.sh` stops the resource samplers when it finishes, which is right
for a campaign and wrong for a re-run: `vm_pub_eff.sh` prints
`window=<t0>-<t1>` lines whose only meaning comes from a `/proc/stat` stream
covering the same window, so a stage re-run with the samplers stopped reduces
to `no samples in the window` for every case — a whole stage paid for and
thrown away (defect H-10).

`rerun_stage.sh <stage...>` is the entry point that does not have that hole:

1. pushes `pub/*.sh` and both origins to the VM and **proves the copies match
   by md5**, refusing to run otherwise (a re-run against a stale script is
   H-7 again);
2. checks the server sampler is alive *and* recent, and starts the samplers
   when it is not — before the stage runs, not after;
3. runs the stages serially on the VM, then collects the stage logs and the
   sample streams into `DEST` (default `out/rerun-<timestamp>`);
4. for the `eff` stage, joins each `CASE` window to the server's CPU samples
   itself and prints the CPU s/GiB table, so that reduction is not a
   copy-paste step.
