# Staging performance campaigns — re-runnable harness

Everything needed to repeat the `bore` performance and stability campaigns
against **any** deployment: the staging server they were first run on, a real
pre-production server, a different region, a different instance size.

Two campaigns live here and share this file's topology, provisioning,
samplers and traps:

* **`bore vhost`** — the original campaign; everything below that does not say
  otherwise is about it.
* **`bore local` (public tunnels)** — `pub/`, with its own
  [`README`](pub/README.md). Same three machines, same `env.sh`, same rules;
  different origins (raw TCP, not HTTP) and its own driver.

Nothing in this directory hardcodes a host, a domain, a key path or a
credential. Re-pointing the campaign is one file (`env.sh`) plus one
provisioning run.

> **Secrets are never committed.** `env.sh.example` is a template with every
> credential field left blank. The real `env.sh` lives outside the repository
> (`~/.config/bore-perf/env.sh`, mode `600`) and is provided separately.

---

## 1. Topology

Three machines, and the distinction between them is the single most important
methodological point in the campaign:

| role | what runs there | why it matters |
| --- | --- | --- |
| **server** | `bore server` (Docker), the public vhost frontend | the thing under test |
| **test VM** | the origin application (`dufs`), the `bore vhost` provider, and the *same-region* consumer | same-region ⇒ ~2 ms RTT, so transport differences are visible |
| **workstation** | the *domestic* consumer, over a real ISP link | ~19.5 ms RTT ⇒ this is the number a user actually experiences |

The workstation must be the **consumer**, never the provider. An earlier round
of measurements put the provider on the workstation, so every byte crossed the
domestic link twice and the result was a measurement of that link, not of bore.

---

## 2. One-time setup

```bash
mkdir -p ~/.config/bore-perf
cp scripts/perf/staging/env.sh.example ~/.config/bore-perf/env.sh
chmod 600 ~/.config/bore-perf/env.sh
$EDITOR ~/.config/bore-perf/env.sh      # fill in hosts + credentials

cargo build --release                    # the binary under test
scripts/perf/staging/provision.sh        # installs everything on the test VM
```

`provision.sh` is idempotent and has sub-steps (`packages`, `binary`, `tools`,
`scripts`, `image`, `corpus`, `check`). It refuses to guess: if the VM's
architecture differs from the workstation's it pushes **no** binary and says so,
because benchmarking a different build than the one under review is worse than
not benchmarking at all.

Local prerequisites on the workstation: `curl`, `jq`, `python3`, `ssh`, `scp`,
and for the netns gates `unshare` with unprivileged user namespaces enabled.

---

## 3. Running the campaign

Order matters. Later stages depend on state earlier ones establish, and the
instance's network allowance (§5) has to be respected throughout.

### 3.1 References first — never skip these

```bash
scripts/perf/staging/ws/ws_rtt.sh          # RTT via curl %{time_connect}
scripts/perf/staging/ws/ws_ref.sh          # radio link, ISP link, plain ssh to the VM
scripts/perf/staging/ws/ws_ref_public.sh   # several public endpoints
scripts/perf/staging/ws/ws_ref_vm.sh       # capacity of THIS path, bore removed
scripts/perf/staging/ws/ws_dl_parallel.sh  # parallel-stream download ceiling
```

Without these, a tunnel throughput number cannot be judged: a figure that looks
poor may be the link, and a figure that looks excellent may be a link nobody
measured. `ws_ref.sh`'s ssh row is a *lower bound* — if bore beats ssh, ssh was
the bound.

> ICMP is blocked to most cloud hosts, so RTT comes from `curl
> %{time_connect}`. Do **not** use `/usr/bin/time -f %e`: 10 ms resolution on a
> ~19 ms value produced a suspiciously round 17.00 ms in an early run.

### 3.1b The public-tunnel campaign

```bash
scripts/perf/staging/pub/run_campaign.sh   # one command: build proof, samplers,
                                           # every VM stage serially, collection
ssh <vm> '~/pub/pub_driver.sh'             # or drive the stages by hand
ssh <vm> '~/pub/pub_driver.sh p1 conc eff' # or a subset
scripts/perf/staging/pub/ws_pub.sh         # the workstation topology, afterwards
scripts/perf/staging/pub/summarize.sh out/pub-<timestamp>
```

It is driven separately from the vhost stages below and must not overlap with
them: they share one server and one allowance budget. See
[`pub/README.md`](pub/README.md).

### 3.2 Same-region measurements (driven on the test VM)

```bash
ssh <vm> '~/driver.sh'        # s3_ab, s4_stab, s5_netem, s6_ssh, s7_eff, serially
# or individually:
ssh <vm> '~/vm_cal.sh'        # calibration / control drift
ssh <vm> '~/vm_ab.sh'         # paired relay-vs-QUIC A/B + latency suite
ssh <vm> '~/vm_bulklat.sh'    # F-15: small requests under bulk (phase 03)
ssh <vm> '~/vm_netem.sh'      # impaired paths
ssh <vm> '~/vm_ssh.sh'        # the SSH gateway leg
ssh <vm> '~/vm_eff.sh'        # CPU cost per GiB
ssh <vm> '~/vm_stab.sh all'   # stability / soak
ssh <vm> '~/vm_f13.sh baseline'   # F-13 slow-reader ladder
ssh <vm> '~/vm_g6.sh'         # concurrency ladder
ssh <vm> '~/vm_interop.sh'    # old-client / new-server wire compatibility
```

**Strictly serial.** Every stage shares one server and one 2-vCPU VM;
overlapping two of them contaminates both.

### 3.3 The domestic-consumer leg (run on the workstation)

```bash
scripts/perf/staging/ws/ws_tunnel.sh        # relay/QUIC/carriers, absolute throughput
scripts/perf/staging/ws/ws_tunnel_paired.sh # the same, as budget-neutral pairs
```

### 3.4 The real-application case (dufs)

```bash
scripts/perf/staging/ws/ws_dufs.sh native big-native --carriers 8
scripts/perf/staging/ws/ws_dufs.sh docker big-docker --carriers 8
scripts/perf/staging/ws/ws_dufs.sh ssh    big-ssh
scripts/perf/staging/ws/ws_flavours_rotated.sh        # all three, budget-neutral
scripts/perf/staging/ws/ws_dufs_relay_vs_quic.sh      # both data planes
```

`ws_dufs.sh` takes a flavour (`native` | `docker` | `ssh`), a tag, and any extra
`bore vhost` flags. `PHASES=bigdl,bigup,smalldl,smallup,lat` selects a subset.

### 3.5 Server-parameter programme

```bash
scripts/perf/staging/srv/setenv.sh set BORE_UDP_MEMORY_BUDGET 512MiB "why"
scripts/perf/staging/srv/setenv.sh unset BORE_UDP_MEMORY_BUDGET
scripts/perf/staging/srv/server_seq.sh    # the whole ordered programme
scripts/perf/staging/srv/budget_ws.sh     # the budget, priced from the domestic consumer
scripts/perf/staging/res/cfgkeys.sh       # what the server says is in force
scripts/perf/staging/srv/verify.sh        # entries + resolved tunables + counters
scripts/perf/staging/srv/logcheck.sh      # end-of-campaign log audit
```

Two of these are worth singling out.

`budget_ws.sh` prices `BORE_UDP_MEMORY_BUDGET` **from the workstation, on the
QUIC direct path, A/B/A/B**. That is not an arbitrary choice of vantage point:
the budget derives the stream window as
`clamp(budget / max_carriers, 16 MiB, 256 MiB) / 16`, which on a deployment with
a large `--max-carriers` is 1 MiB — the same order as a domestic consumer's
bandwidth-delay product, and several times smaller than it needs to be at a
same-region 2 ms RTT. Measuring it from the VM would answer nothing.

`annotate_compose.py` rewrites the deployed compose so every value that
deviates from a bore default states the default, the reason and the measurement
on the line above it, and so any commented block that contradicts an active
setting is corrected. It encodes the 2026-09-11 decisions; read it before
running it against a different deployment.

`setenv.sh` backs the compose up, writes a line prefixed by a
`# perf-campaign <date>:` marker naming the reason, restarts the container and
waits for it to answer. Restarting the server kills every registered tunnel, so
this block runs **last**.

### 3.6 Resources (CPU/RAM of every actor, throughout)

```bash
scripts/perf/staging/res/start_samplers.sh    # server, VM and workstation
scripts/perf/staging/res/stop_samplers.sh     # stop all three, pull the files back
scripts/perf/staging/res/res_reduce.py <samples...>
```

> The kill and the start **must be separate ssh invocations**. With both in one
> remote command string, `pkill -f 'res_sample[r].sh'` matches the command
> string's own later plain occurrence and the remote shell kills itself before
> reaching `setsid` — twice observed as "exit 144, zero samples".

### 3.7 The sudo-free netns gates (no deployment needed)

```bash
scripts/perf/vhost_idle_window.sh           # F-14 loss window: ladder + safety
scripts/perf/vhost_udp_concurrency_repro.sh # the carriers=1 window cliff
scripts/perf/vhost_bulk_latency.sh          # phase 03 scheduling, real network
scripts/perf/vhost_netem_matrix.sh          # impaired-path matrix
```

`vhost_idle_window.sh` is the one to run first when re-opening this work: it
reproduces the F-14 fallback behaviour, counters included, in about a minute,
on any Linux box, with no server and no privileges.

---

## 4. Reading a result

Every throughput line carries the server's inbound-allowance delta across
exactly that measurement, e.g. `48.95 MB/s (+201)`. Interpretation:

| delta | meaning |
| --- | --- |
| 0 – ~1 000 | clean; quotable as absolute throughput |
| ~10 000 + | the instance's token bucket was firing; the figure is AWS shaping |

A shaped burst is **reported and set aside**, never averaged in.

---

## 5. The traps this harness already avoids

These cost real time in the first campaign. They are encoded in the scripts;
do not "simplify" them back out.

1. **The instance network allowance dominates everything.** One 4-stream 10 s
   download is ≈500 MB, which is roughly a whole inbound burst budget. A
   sequential A-then-B comparison therefore measures the bucket: the first arm
   ran at 49.82 MB/s and the second at 23.37. The fix is threefold — register
   every arm **at once**, **rotate the order** every round so each arm occupies
   each position once, and cool down **75 s** between bursts (30 s is not
   enough).
2. **`curl --max-time` still emits `--write-out`** (rc 28, with the partial
   `size_download`). That is what makes a wall-clock-bounded burst possible. An
   earlier version killed curl instead and got zero bytes back, because a killed
   curl never writes the line.
3. **`pgrep`/`pkill -f <pattern>` matches the ssh/bash command string that
   contains the pattern.** Bracketing one occurrence is not enough — the bracket
   must cover *every* occurrence, or the invocations must be split.
4. **`2>&1 > file` is the wrong order.** bore logs to stderr; `docker logs
   … 2>&1 > /tmp/x` sends 6 MB to the terminal and an empty file to disk.
5. **`asort` is gawk-only** and silently produces nothing under mawk. Medians
   come from `sort -n`.
6. **`tc` wants `dev <if>` after the object and verb.**
7. **Loopback false-passes two whole classes of bug** — throughput and the
   injected-flush/keep-alive class — but is a valid oracle for a *deadline*.
   That is why `vhost_idle_window.sh` may run locally while everything in §3
   may not.

---

## 6. Where the results live

| document | what it is |
| --- | --- |
| `docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10.md` | the "before" campaign |
| `docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10_DEV_RESULT.md` | the "after" campaign: paired before/after, bug verification, bottleneck per test |
| `docs/performance/final_vhost_perf_review.md` | the same evidence in Italian, written to be readable by a non-specialist |
| `docs/performance/CARRIER_TUNING.md` | the operator-facing tuning guide |
| `docs/performance/PUBLIC_STAGING_EVIDENCE_2026-09-11.md` | the public-tunnel campaign: defect register, paired transport A/B, flavours, concurrency, netem, CPU s/GiB, soak |
| `docs/performance/final_public_perf_review.md` | the public-tunnel evidence in Italian, written to be readable by a non-specialist |

Each of those names the exact script that produced each figure, so a number can
always be traced back to a command in this directory.
