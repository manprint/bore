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
| `sec_eff.sh` | S3 | CPU seconds per delivered GiB on all three hosts, plus the server's relay counters (which must stay flat on the direct arm) |
| `sec_lat.sh` | S4 | new-connection latency and the concurrency ladder, per transport |

Each is standalone and re-runnable:

```shell
TOPO=vm-ws PAIRS=5 MB=128 scripts/perf/staging/sec/sec_ab.sh   | tee out/s1-vm-ws.txt
TOPO=vm-ws N=20        scripts/perf/staging/sec/sec_ttd.sh     | tee out/s2-vm-ws.txt
TOPO=vm-ws GIB=2       scripts/perf/staging/sec/sec_eff.sh     | tee out/s3-vm-ws.txt
TOPO=vm-ws PROBES=100  scripts/perf/staging/sec/sec_lat.sh     | tee out/s4-vm-ws.txt
```

Start the resource samplers first when running S3 (they are what `cpu_window.sh`
reduces afterwards):

```shell
scripts/perf/staging/res/start_samplers.sh 20000
# ... stages ...
scripts/perf/staging/res/stop_samplers.sh
```

## Reading the results

* **The path comes from the CONSUMER row.** A secret tunnel's direct path does
  not touch the server, so the server cannot observe it; the consumer reports
  it and the admin API republishes it as `current_path` / `direct_fallbacks` /
  `path_reason`. A `--udp` PROVIDER row reads `unknown` **by design** — that is
  not a bug and not a fallback.
* **A `--udp` arm that ran on the relay is labelled `relay(fb=N)`** and is
  never merged into a "direct" median. A direct number that was silently a
  relay number is the single mistake this campaign exists to avoid.
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
* A stage that cannot bring a tunnel up prints `PROVFAIL`/`CONSFAIL` and keeps
  going; those rows are counted separately from traversal results, because a
  registration failure is not a NAT result.
