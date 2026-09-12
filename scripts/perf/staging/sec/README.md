# Secret-tunnel staging campaign

Repeatable harness for the **secret tunnel** (`bore local --tcp-secret-id` +
`bore proxy`), the only bore transport whose fast path is peer-to-peer: the
relay arm is `consumer → server → provider`, the direct arm is a hole-punched
QUIC connection `consumer ↔ provider` that the server only brokered.

Everything here is driven **from the workstation**, because a secret tunnel
has two clients and only the workstation can reach both hosts.

## Coordinates and secrets

Same `env.sh` as every other staging harness — `$BORE_PERF_ENV`, then
`~/.config/bore-perf/env.sh`, then `./env.sh`; `chmod 600`, never committed.
See `../env.sh.example`. No script in this directory hardcodes a host, a port
or a credential.

## Topologies

`TOPO=` selects where the two clients run. It is the first thing to set and it
changes what the measurement means:

| `TOPO` | provider | consumer | what it measures |
|---|---|---|---|
| `vm-ws` | test VM | workstation | download from a same-region provider into a home NAT — the common user shape |
| `ws-vm` | workstation | test VM | the asymmetric twin: the home NAT is on the sending side |
| `vm-vm` | test VM | test VM | CONTROL: no WAN between the peers, so the direct arm isolates QUIC's CPU cost from every network effect |

## Stages

| script | id | question |
|---|---|---|
| `sec_ab.sh` | S1 | relay vs direct throughput, paired, median ratio |
| `sec_ttd.sh` | S2 | punch success RATE, time-to-direct distribution, and the `path_reason` census for every fallback |
| `sec_eff.sh` | S3 | CPU seconds per delivered GiB on all three hosts, plus the server's relay counters (which must stay flat on the direct arm) and kernel packet counters on both peers |
| `sec_lat.sh` | S4 | new-connection latency and the concurrency ladder, per transport |
| `sec_ack.sh` | S5 | is quinn's ACK policy worth thinning on the direct path? Both arms are `--udp`; the only difference is the ACK pair. **Answered 2026-09-12: not as a default** — see below |

Each is standalone and re-runnable:

```shell
TOPO=vm-ws PAIRS=5 MB=128 scripts/perf/staging/sec/sec_ab.sh   | tee out/s1-vm-ws.txt
TOPO=vm-ws N=20        scripts/perf/staging/sec/sec_ttd.sh     | tee out/s2-vm-ws.txt
TOPO=vm-ws GIB=8       scripts/perf/staging/sec/sec_eff.sh     | tee out/s3-vm-ws.txt
TOPO=vm-ws PROBES=100  scripts/perf/staging/sec/sec_lat.sh     | tee out/s4-vm-ws.txt
TOPO=vm-vm ACK=10 ACK_DELAY_MS=1 \
                       scripts/perf/staging/sec/sec_ack.sh     | tee out/s5-vm-vm.txt
```

S5 needs both topologies to mean anything, and the 2026-09-12 run showed why in
a way nobody designed: **the same 25 ms delay bound is a different multiple of
the RTT on each leg.** `vm-vm` is loopback-class RTT, so 25 ms is several
hundred RTTs; `vm-ws` measures 22.31 ms median, so 25 ms is ≈1.1 RTT.

```
vm-vm   get median 0.622, put 0.908, worst pair 0.020 (393.86 -> 7.91 MB/s)
vm-ws   get median 1.011, put 1.029, worst pair 0.909 — no collapse at all
```

The verdict is **do not default it**: a fixed constant that is benign at 22 ms
RTT and catastrophic at 0.05 ms is by definition not a default. The idea is not
refuted — what was refuted is asking for a threshold while letting the delay
bound fall back to the peer's transport parameter.

**Both env vars are now required.** `ACK_DELAY_MS` (default 1 ms) sets
`BORE_DIRECT_QUIC_ACK_MAX_DELAY_MS`; the binary refuses a bare threshold with a
warning, and this stage refuses to start when either peer's binary predates the
delay variable — read out of the binary, not from a version string, because a
peer that silently ignores it would make the stage print a full table of the
trap under the heading of an ACK experiment.

**Run S3 at `GIB=8`, not `GIB=2`.** `ps -eo cputimes` counts whole seconds, so a
2 GiB arm quantises the per-process deltas at ±1 s — in the 2026-09-11 run the
receiver read 3 s → 4 s and could not be claimed, and the 8 GiB re-run showed
the true figure was 13 s → 22 s (1.69×). Eight GiB costs about ten minutes per
topology and is the difference between a measurement and a shrug.

**S3 does not measure single-stream bandwidth.** Every stage here runs four
concurrent connections, i.e. four QUIC streams, each with its own receive
window — so a per-stream cap is invisible to it. The oracle for one stream is
the diagnostic, which prints the windows actually in force alongside quinn's own
rtt/cwnd/loss:

```shell
bore test-udp --tcp-secret-id <id> ...    # UDP direct path tuning : stream recv ...
```

That is what found the window-floor defect (§10 of the evidence doc) after S3
had run clean four times.

Start the resource samplers first when running S3 (they are what `cpu_window.sh`
reduces afterwards):

```shell
scripts/perf/staging/res/start_samplers.sh 20000
# ... stages ...
scripts/perf/staging/res/stop_samplers.sh
```

**On a shared machine, `busy` is not attributable (H-17).** `/proc/stat` counts
every neighbour on the box, and the sampler's process regex
(`bore|dufs|curl|oha|python3`) does not see most of them. On the workstation a
2 GiB window read `busy` 41.07 → 76.89 CPU s between two arms while every
matched process accounted for 1 s of the 35.8 s difference — read at face value
that says the transport doubled the CPU bill, which is a statement about a
browser. `cpu_window.sh` warns when the unattributed CPU seconds
exceed **10** *and* are more than **half** the host bill — a bare ratio misfires
in both directions, passing a `ws` row at 2.5× with 44.7 unattributed CPU s and
firing on a dedicated server whose `busy` is 3.06 over two minutes (0.02 cores). **Quote `busy` on a dedicated host (server, test VM) because it
includes the softirq the process never sees; quote the per-process delta on a
shared one, and say which you used.**

## Reading the results

* **The path comes from the CONSUMER row.** A secret tunnel's direct path does
  not touch the server, so the server cannot observe it; the consumer reports
  it and the admin API republishes it as `current_path` / `direct_fallbacks` /
  `path_reason`. A `--udp` PROVIDER row reads `unknown` **by design** — that is
  not a bug and not a fallback.
* **A `--udp` arm that ran on the relay is labelled `relay(fb=N)`** and is
  never merged into a "direct" median. A direct number that was silently a
  relay number is the single mistake this campaign exists to avoid.
* **A pair is EXCLUDED from the median unless both of its arms measured
  something**, and the excluded rows stay printed with a `-` ratio plus an
  explicit `EXCLUDED n of N pairs` line. Counting a failed arm as the ratio 0
  is not conservative, it is wrong in a specific direction: it drags the
  median down to the smallest arm that actually ran. Measured — `s1-vm-ws get`
  reported 0.965 with two `startfail` rows when the three real arms were
  0.965 / 1.247 / 1.218.
* **Cooldowns are not padding.** The staging server is a burstable instance
  whose inbound allowance is a token bucket; one 4-stream burst is roughly a
  whole budget. `COOL` defaults to 75 s for that reason, and the paired design
  exists because the budget drifts faster than an A-then-B comparison can
  tolerate.

## House rules this harness follows

* **Never `pkill bore`.** The deployment carries the operator's own live
  tunnels. Local processes are killed by the PID they were started with;
  remote ones by `--tcp-secret-id <id>`, where the id is minted per run and
  exists nowhere else on the box.
* **Never two netns harnesses at once** — unrelated to this directory, but the
  lab gates it cross-references (`scripts/udp_nat_netns_test.sh`,
  `scripts/secret_netns_test.sh`) share namespace names.
* **No local compiles while a throughput stage runs.** The workstation is one
  half of every `vm-ws` and `ws-vm` arm, and the direct arm encrypts in user
  space while the relay arm is TCP in the kernel — so `cargo` competing for
  the CPU depresses the direct arm more than the relay arm and biases the very
  ratio the stage reports. Done once in the 2026-09-11 campaign; it cost a
  re-run of the stage.
* `SEC_ENV` carries extra `NAME=value` pairs to every bore process the library
  starts, **on both hosts** — the only correct way to A/B a knob read through
  `std::env::var`, since exporting it in this shell would reach the
  workstation peer and silently miss the VM one.
* A stage that cannot bring a tunnel up prints `PROVFAIL`/`CONSFAIL` and keeps
  going; those rows are counted separately from traversal results, because a
  registration failure is not a NAT result.
