# vhost performance and stability campaign

The harness behind
`docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10.md`. Read section 9 of that
document first — it is the runbook, including how to provision the measurement
VM and why a same-region VM is mandatory rather than convenient.

## Credentials

Nothing here contains a secret. Every script that talks to a deployment reads an
env file:

```bash
export BORE_PERF_ENV=$HOME/env.sh    # chmod 600, outside the repo
```

and that file exports `BORE_HOST`, `BORE_TO`, `BORE_SECRET`, `ADMIN_URL`,
`ADMIN_TOKEN`, and for the SSH-gateway suite `SSHGW_USER` / `SSHGW_PASS`.
The scripts that reach the server host over ssh additionally need
`BORE_SERVER_IP`, `BORE_SERVER_KEY` and optionally `BORE_SERVER_USER`.

## Scripts

| script | runs on | invocation |
| --- | --- | --- |
| `vhost_remote_bench.sh` | measurement VM | `v0` calibrate, `v1` transport A/B, `v2` latency suite |
| `vhost_transport_ab.sh` | measurement VM | no args; `DUR=12` seconds per half. Paired A/B — A1 transport, A2 carriers, A3 latency |
| `vhost_bulk_latency.sh` | measurement VM | no args. F-15: small-request latency with 0 / 1 / 2 bulk transfers in flight, across carrier counts and both transports |
| `vhost_remote_stability.sh` | measurement VM | `all`, or one of `g1`…`g9` |
| `vhost_ssh_gateway_bench.sh` | measurement VM | `all`, or one of `s1`…`s8` |
| `vhost_ssh_takeover_probe.sh` | measurement VM | no args. I-SSH5 with a discriminating origin |
| `vhost_netem_matrix.sh` | measurement VM | no args. Needs `sudo -n tc`. Protocol-selective loss and RTT, plus the UDP-blackhole fallback case |
| `vhost_registration_leak_repro.sh` | anywhere with a tunnel | no args. The F-1 reproducer: a provider alive at TCP level, dead at application level |
| `vhost_header_injection_ab.sh` | any host with ≥ 8 cores | no args. **Needs no deployment access** — runs its own private server on loopback |
| `vhost_app_ceiling.sh` | any host with ≥ 4 cores | `[--cores 0,1] [--cores all]`. **Needs no deployment access.** Application ceiling with the network removed: private server over loopback, transport × carriers × parallel-stream sweep, CPU seconds per GiB |
| `vhost_remote_efficiency.sh` | measurement VM | `paired` (default) for alternating single-stream rounds, `saturate` for parallel streams. Prints the epoch window of each case so the server samplers can be matched to it |
| `server_cpu_sample.sh` | workstation | `N IV` — N samples every IV seconds, over ssh to the server host |
| `server_cpu_report.sh` | workstation | `<file from the sampler>` |
| `../vhost_bulk_isolation.sh` | workstation, **root** | `sudo -n /abs/path/scripts/vhost_bulk_isolation.sh [rtt-list] [secs]`. Phase 03.5: small-request p50/p95 with 0/1/2 bulk transfers in flight, across `--carriers 1/4/0` and both transports, at a controlled RTT. **Needs no deployment access.** Lives in `scripts/` for the same sudo-path reason |
| `../vhost_concurrency_ladder.sh` | workstation, **root** | `sudo -n /abs/path/scripts/vhost_concurrency_ladder.sh [rtt-ms] [a\|b\|c\|e\|ladder\|u\|all] [server-cpu-list] [client-cpu-list]`. Phase 04: fresh-request time behind 16…512 held connections, on both transports, with `--max-conns`, carrier-count, active-vs-idle, unified-control-port and forked-`curl`-client variants. `[server-cpu-list]` pins the server (use `0,1` to reproduce a 2-vCPU instance). **Needs no deployment access.** Lives in `scripts/` for the sudo-path reason |
| `../vhost_h2_page_load.sh` | workstation, **root** | `sudo -n /abs/path/scripts/vhost_h2_page_load.sh [rtt-list] [runs] [large-bytes] [small-count]`. Phase 07.1: full page load versus RTT, h1-through-tunnel / h1-direct / h2-direct. **Needs no deployment access.** Lives in `scripts/` (not here) because NOPASSWD sudo is per exact path and the glob does not cross `/` |
| `server_ena_sample.sh` | workstation | `N IV`. AWS ENA instance-allowance counters (`bw_*_allowance_exceeded`, `pps_allowance_exceeded`) plus `/proc/net/dev`. Needs `sudo -n ethtool` on the server |

Run `server_cpu_sample.sh` **concurrently** with whichever suite you care about;
it is the only way to see the usr/sys/softirq split and `steal`.

### Answering "is the application the limit?"

Three scripts, in this order, because no single one can separate the code from
the box from the link:

1. `vhost_app_ceiling.sh` — the application with no network in the way. Gives
   the s/GB cost on a fast CPU and shows whether bore uses more than one core
   (it does: 1.0 → 3.6 as `--carriers` and parallelism rise).
2. `vhost_remote_efficiency.sh` plus `server_cpu_sample.sh` — the same cost per
   byte on the real path, as **host-total** CPU. Container-only accounting
   misses the softirq the host kernel spends on the container's behalf, and that
   is 46–55 % of the bill.
3. `vhost_remote_efficiency.sh saturate` plus `server_ena_sample.sh` — the
   discriminator. Cores near the core count means the guest CPU is the wall;
   low cores with climbing `pps_allowance_exceeded` means the hypervisor is, and
   no application change will help.

Then divide the target link rate by the measured s/GB. On staging this gave
0.67 cores for 1 Gbit/s on the relay and a hard 0.96 Gbit/s ceiling for `--udp`
(F-16, §2.17).

## Reading the results

- Take bulk rates from the server's own `relay_tx_bytes` delta, never from
  `curl`'s reported speed.
- Prove the data path per case, and prefer an impairment control over a counter:
  `direct_stream_opens` counts *attempts*, not successes (F-14).
- The control drift on the staging server was **29 %**. Independent means below
  that are noise; use the paired design in `vhost_transport_ab.sh` and quote the
  median ratio.
- Check for leftover registrations after every case, and check the operator's own
  tunnels before and after — the concurrency cases can put a 1 GiB host under
  memory pressure (F-13).

## Harness traps

Section 9.6 of the evidence document lists the ones that produced wrong
conclusions. The three that cost the most time:

1. `pgrep -f <pattern>` matches your own shell. Use
   `ps -eo pid,args | grep -E '[p]attern'` or `pgrep -x`, then kill explicit PIDs.
   Never blanket-`pkill bore` — it kills unrelated tunnels on the same machine.
2. A bare `wait` blocks on every child, the origin and the tunnel provider
   included. Collect PIDs and `wait "$p"` on each.
3. `kill -9` on `( sleep | ssh … )` kills the subshell, not `ssh`. Background
   `ssh -T … < /dev/null` directly. Never pass `-N`: it opens no session channel,
   so the gateway's banner and warnings can never be delivered.
4. A sweep spec of `"<flags>|<carriers>|<parallel>"` uses `|` and not `:` because
   an empty flags field with `:` splits into four fields for three variables,
   shifting the values along silently.
5. A stale server still holding the control port serves every request while the
   process you believe you are measuring has already exited with "address in
   use" — real throughput, wrong pinning, zero CPU. `vhost_app_ceiling.sh`
   pre-checks the ports and then verifies its own PID bound them.
6. ENA and `/proc/stat` counters are cumulative since boot. Only deltas across a
   measured window mean anything; a raw `pps_allowance_exceeded` of 9.2 million
   on a long-lived instance says nothing about the run in progress.
