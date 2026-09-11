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
| `vm_pub_flavours.sh` | VM | P5 native vs dockerized binary vs OpenSSH `-R`, rotated and budget-neutral |
| `vm_pub_conc.sh` | VM | P6 concurrency ladder: what a fresh connection costs behind N held ones |
| `vm_pub_netem.sh` | VM | P8 loss / delay / reorder matrix, both transports under the SAME shaping |
| `vm_pub_eff.sh` | VM | P9 CPU seconds per GiB, the application-limit question |
| `vm_pub_stab.sh` | VM | P7 soak, direct-path recovery, port release, wedged-client reap, churn |
| `pub_driver.sh` | VM | runs the stages strictly serially with timestamps |
| `ws_pub.sh` | workstation | the real-world topology: consumer outside AWS |

Two origins, both started automatically:

* `raw_origin.py` (port 5053) — **raw TCP**, one request line then bytes. A
  public tunnel forwards arbitrary TCP, so this is what most arms measure: no
  HTTP parsing on either end, and the origin itself contributes ~0.1 ms and
  ~19 Gbit/s on loopback. Verbs: `GET n`, `PUT n`, `PING`, `ECHO`, and `HOLD s`
  — the last one answers once and then moves no bytes at all, which is what the
  concurrency ladder needs (held connections that *download* measure the link,
  not the concurrency).
* `bench_origin.py` (port 5052) — HTTP, used only by the P4 arm so the numbers
  can be compared with the vhost campaign.

## Running it

```bash
# once, from the workstation
scripts/perf/staging/provision.sh              # pushes pub/, both origins, the binary

# the full VM-side campaign (hours; strictly serial by design)
ssh <vm> '~/pub/pub_driver.sh'                 # or: ~/pub/pub_driver.sh p1 conc eff

# the real-world topology, from the workstation
scripts/perf/staging/pub/ws_pub.sh

# resource sampling on all three actors, around a stage
scripts/perf/staging/res/start_samplers.sh
scripts/perf/staging/res/ena_timeline.sh 3600 > out/ena.timeline &
#   ... run the stage ...
scripts/perf/staging/res/stop_samplers.sh
```

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
