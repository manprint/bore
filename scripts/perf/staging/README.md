# Staging performance campaigns — re-runnable harness

Everything needed to repeat the `bore` performance and stability campaigns
against **any** deployment: the staging server they were first run on, a real
pre-production server, a different region, a different instance size.

Six campaigns live here and share this file's topology, provisioning,
samplers and traps:

* **`bore vhost`** — the original campaign; everything below that does not say
  otherwise is about it.
* **`bore local` (public tunnels)** — `pub/`, with its own
  [`README`](pub/README.md). Same three machines, same `env.sh`, same rules;
  different origins (raw TCP, not HTTP) and its own driver.
* **`bore proxy` (secret tunnels)** — `sec/`. Peer-to-peer: the interesting
  transport is consumer↔provider and the server is not on the direct path.
* **`bore transfer`** — `xfer/`.
* **`bore vpn`** — `vpn/` (§3.8). Both ends need root and a TUN, and the path is
  a STATE that must be waited for and verified, not a flag.
* **`bore sshjhost` (jump host)** — `jump/` (§3.9). The deliverable is latency,
  not bandwidth, and it needs a binary built with an extra feature.

§3.10 is not a campaign but a **precondition for all six**: qualifying the
access link before any absolute figure is quoted.

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
scripts/perf/staging/srv/verify_fixes.sh  # P-12/P-13 read back from the kernel
scripts/perf/staging/srv/logcheck.sh      # end-of-campaign log audit
```

`verify_fixes.sh` is the one that answers "did the deploy actually change
anything". The public campaign's two startup-time fixes are invisible in normal
operation — a descriptor limit that is now high enough, and a QUIC socket whose
buffers were configured — which is precisely why neither was noticed for as long
as it existed. So it reads both back from the **kernel**, through the container's
PID on the host (`/proc/<pid>/limits`, and `nsenter -n ss -uapm` for the socket's
`rb`/`tb`), never from a log line: a log only proves the server talked about it.
`redeploy.sh` ends by calling it and fails the deploy if it fails.

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
scripts/vhost_udp_concurrency_repro.sh      # the carriers=1 window cliff
scripts/perf/vhost_bulk_latency.sh          # phase 03 scheduling, real network
scripts/perf/vhost_netem_matrix.sh          # impaired-path matrix
scripts/perf/public_idle_window.sh          # T-PUB-UDPBUF / T-PUB-FDBUDGET / T-PUB-RELAYPATH
scripts/perf/secret_leak_hunt.sh            # S-2 path report, S-8 upgrade cap
```

> `vhost_udp_concurrency_repro.sh` is in `scripts/`, **not** `scripts/perf/` —
> this file pointed at the wrong directory until 2026-09-12, so anyone following
> the runbook got "No such file".


`vhost_idle_window.sh` is the one to run first when re-opening this work: it
reproduces the F-14 fallback behaviour, counters included, in about a minute,
on any Linux box, with no server and no privileges.

The two harnesses that DO need root live in `scripts/` and are invoked by their
absolute path (`sudo -n /abs/.../scripts/<name>.sh` — the sudoers grant is per
exact path, so `sudo bash scripts/...` prompts and must not be used):

| harness | gates added 2026-09-12 |
|---|---|
| `scripts/vpn_netns_test.sh` | `T-RF4` (`--accept-routes` exact / supernet / unrelated), `T-RF5` (`--refuse-all-routes`, overlay still works), `T-RF6` (`--no-route-manage`), `T-PINMTU` (control arm first: the TUN **must** move when the path MTU falls, then the same stimulus with `--pin-mtu` and it **must not**; the gate reports `SKIP` — never `PASS` — if the control arm did not move) |
| `scripts/udp_nat_netns_test.sh` | `T-NAT-MANUAL-CAND` + `T-NAT-NOSTUN-BARE` (the operator's declared static forward reaches the wire and wins DIRECT, and its red-check: same router, same forward, no declaration → RELAY), `T-NAT-PLAN-KILL` (`bore server --no-udp-adaptive-plan`: no plan computed, none delivered, **and the pair still goes direct** — a kill switch, not a way to break traversal) |

A suite that reports `SKIP` is not green. `vpn_netns_test.sh` now prints
`PASS=… FAIL=… SKIP=…` and says so explicitly: a skipped gate is one whose
stimulus did not apply, which is a fact about the run, not a pass.

**Never run two netns harnesses at once** — they share ns0/ns1/ns2 and one run's
pre-cleanup wipes the other's namespaces mid-flight.

---

## 3.8 The VPN campaign (`vpn/`)

```bash
scripts/perf/staging/vpn/link_baseline.sh        # ALWAYS FIRST -- see §3.10
scripts/perf/staging/vpn/vpn_ab.sh               # bare / relay / direct, interleaved
scripts/perf/staging/vpn/vpn_direct_deficit.sh   # the direct arm, decomposed
scripts/perf/staging/vpn/vpn_wire_ceiling.sh     # fixed UDP offer ladder
scripts/perf/staging/vpn/vpn_txqueue.sh          # TUN device queue depth
scripts/perf/staging/vpn/vpn_sndbuf.sh           # QUIC datagram send buffer
scripts/perf/staging/vpn/vpn_udpbuf.sh           # UDP socket send buffer
scripts/perf/staging/vpn/vpn_cc_matrix.sh        # congestion controller
scripts/perf/staging/vpn/vpn_lat.sh              # latency
scripts/perf/staging/vpn/vpn_rtt_load.sh         # latency UNDER LOAD, attributed
scripts/perf/staging/vpn/vpn_profile.sh          # where the CPU goes
scripts/perf/staging/vpn/vpn_modes.sh            # topologies
scripts/perf/staging/vpn/vpn_hub.sh              # 1:N hub
scripts/perf/staging/vpn/vpn_stability.sh        # soak
scripts/perf/staging/vpn/vpn_relay_attrib.sh     # whose ceiling is the relay's -- see below
scripts/perf/staging/vpn/vpn_ctrl_leak.sh        # the open defect's gate -- expected RED today
scripts/perf/staging/vpn/vpn_overhead.sh         # wire bytes per delivered byte
```

**The deep-dive set (`scripts/perf/staging/rerun_vpn_deep.sh`)** exists because
the list above measures the VPN's PERFORMANCE and says nothing about the shipped
options it never passes. The coverage matrix is
`docs/performance/VPN_DEEP_DIVE_PLAN.md`; the five stages are:

```bash
scripts/perf/staging/vpn/vpn_wiring.sh          # D1 knobs read back from the KERNEL, no traffic
scripts/perf/staging/vpn/vpn_routes.sh          # D3 route policy read from `ip route`, no traffic
scripts/perf/staging/vpn/vpn_traversal_opts.sh  # D2 --stun-server/--nat-udp-preferred-port/
                                                #    --try-port-prediction/--upnp, priced in TIME TO DIRECT
scripts/perf/staging/vpn/vpn_quic_timers.sh     # D4 idle/keepalive/initial-rtt, priced in DEAD TIME
scripts/perf/staging/vpn/vpn_carriers.sh        # D5 carriers on BOTH paths x 1/4 inner flows
```

Two rules these four added to the harness, both worth knowing before writing
another stage:

* **A stage must prove its flag ACTED, not merely that it was passed.** Every
  arm of `vpn_traversal_opts` carries the log line that is its own evidence
  (`port prediction ENABLED`, `managed port mapping ENABLED`, the local address
  on the preferred port, the selected STUN server); an arm without it prints
  `NOT-APPLIED` and is kept out of every median. Without this, `--upnp` on a
  router with neither PCP nor IGD produces a perfectly credible null result
  about a feature that never ran — the `vpn_hub` mistake in a new costume.
* **Killing a live direct path is now possible, and it is a root operation.**
  `sudo -n /abs/.../scripts/vpn_tun_endpoint.sh blackhole on|off|status <ipv4>`
  installs a dedicated nft table that drops UDP to and from the far end only, so
  the direct path dies while the TCP relay — which goes to the *server*, a
  different host — keeps working. It is the only stimulus on the real path that
  exercises DEC-2 (fall back to the warm relay in place). `vpn_cleanup` removes
  it on every exit path and `vpn_assert_clean` declares the host dirty if one
  survives: a forgotten blackhole is invisible to `ip route` and would silently
  ruin every later measurement.

What makes this campaign different from the other five, and what the scripts
therefore do that the others do not:

1. **Both ends need root and a TUN.** The workstation half goes through
   `scripts/vpn_tun_endpoint.sh` — the single root entry point, invoked as
   `sudo -n /absolute/path/...` because NOPASSWD sudo is granted per EXACT path.
   The VM half runs under `sudo -n` over ssh. Note that `scripts/perf/*` scripts
   must run **without** sudo: the sudoers glob does not cross a `/`.
2. **The path is a state, not a flag.** A VPN link ALWAYS starts on the relay
   and upgrades to direct on a background 30 s grid. `wait_path direct` exists
   for that, and an arm that could not reach its path is printed as FAILED and
   never averaged in. A direct number that was silently a relay number is the
   single mistake this campaign is built to avoid.
3. **Wait for the MTU to settle before measuring.** The TUN climbs 1350 → 1288 →
   1414 over ~25 s. `wait_mtu_settle` holds until it stops moving; an 8 s window
   settles on the intermediate 1288 and measures an MSS 126 bytes short of the
   one the link actually runs at.
4. **Leave the host clean.** A VPN endpoint edits interfaces, routes, possibly
   `ip_forward` and nft/iptables. **`vpnlib.sh` installs the EXIT/INT/TERM trap
   at source time**, so a stage inherits cleanup by merely sourcing it and no
   stage needs to declare one. A stage that *does* declare its own trap
   (`vpn_relay_attrib`, `vpn_rtt_load`) is REPLACING the library's, so it must
   re-list `vpn_cleanup; vpn_assert_clean` alongside whatever it is adding —
   forgetting that is how a stage silently loses its teardown. Do not "fix" a
   stage by adding a duplicate trap: grep `vpnlib.sh` before concluding one is
   missing.
5. **Compare a stage's DURATION against its timeout before reading its
   result.** `vpn_hub` once closed in 19 s against a 2400 s budget with `rc=0`,
   because the guard around its throughput block skipped the measurement
   without saying so. A stage far below its budget has usually skipped
   something; a stage that reports success having measured nothing is the
   failure mode the `.out` files exist to expose.

**Never run two netns harnesses at once** — they share the names ns0/ns1/ns2 and
one run's pre-cleanup wipes the other's namespaces mid-flight.

`vpn_ctrl_leak.sh` is the gate for the one product defect still open: a VPN
connector with `--auto-reconnect` leaks one ESTABLISHED control connection per
reconnect (measured 1→2→3→4, none reaped in 120 s, counted from `ss`). **It is
expected to FAIL on today's code** — that is what makes it a gate rather than a
test that happens to be green. Mechanism, red-check and the three fix options
(with why the obvious one is wrong) are in `docs/vpn/VPN_CTRL_CONN_LEAK.md`.

The leak is corroborated by a **second instrument in a different stage**:
`vpn_stability.sh` reads `/proc/<pid>/fd` through the root helper and sees the
same one-per-reconnect growth (12 → 13 → 14) that `ss` sees as ESTABLISHED
connections. In the same run the tunnel's throughput fell from 712 Mbit/s to
95–160 by the third reconnect while the bare path measured 733 up moments
later — so the leak is a candidate throughput regression, not a cosmetic
descriptor count. See §15 of `docs/performance/ETH_RERUN_EVIDENCE_2026-09-12.md`.

### `vpn_relay_attrib.sh` — why no relay percentage is a sentence about bore until this runs

The three arms are **not** three transports over one path. `bare` and `direct`
are workstation↔VM; the `relay` arm is server-mediated by construction, so it is
a DOUBLE TRANSIT through a third host — and that host is a 2-vCPU `t4g.micro`
whose cumulative allowance counters are in the tens of millions. This stage
splits the deficit four ways:

* the allowance counters read as a **delta bracketing the arm** (a counter that
  is nonzero at the end cannot say which arm spent it);
* each **leg measured separately** (a relay cannot beat its slower leg);
* the same double transit through a **non-bore relay** — the deployment shape
  with the product removed;
* **CPU on the relay host**, host-wide and per-process.

The control relay is `attrib_net.py`, a small `os.splice` TCP relay: the staging
server has neither iperf3 nor socat, and installing packages on the host that
carries the operator's live tunnels is not a benchmark's decision. Splice keeps
Python out of the data path, and the stage **checks the instrument instead of
trusting it** — a `bare-py` arm over the same path iperf3 measures, plus a
single-hop leg — reporting the control as a FLOOR rather than a verdict when
either check fails. This applies to **every** campaign, not just the VPN one:
the relay always transits this server.

---

## 3.9 The jump-host campaign (`jump/`)

```bash
scripts/perf/staging/rerun_jump.sh        # all three stages, with markers
```

Three stages, because the plan asks the jump host **five** questions and for a
long time only one stage was wired — covering four of them and leaving the
fifth, the one whose failure a user experiences as *"my session dropped"*,
entirely unmeasured:

| question | stage |
| --- | --- |
| session-open decomposition | `jump_lat` (`tcp`/`wchan`/`open`) |
| application RTT on a live session | `jump_lat` (`chan`/`echo`) |
| relay against direct QUIC | `jump_lat` (arms, interleaved) |
| what carriers cost | `jump_lat` (`direct4` arm) |
| **stability: rekey, warm-relay fallback when UDP dies** | **`jump_stab`** |
| **channel isolation under bulk (the vendored russh HOL fix)** | **`jump_hol`** |

The last row is not in the plan and is here anyway: this repository **vendors
russh** to fix per-channel head-of-line blocking, and that fix has only unit
gates. An in-process test false-passes exactly this class — loopback drains
opportunistically and never builds the queue that makes the bug visible — so
until `jump_hol` there was no real-path evidence that the shipped fix works
where it matters. It runs a time-bounded bulk transfer on a SECOND channel of
the SAME session and samples keystroke latency strictly inside it; the ratio
`loaded/idle` is the isolation. Time-bounded, not byte-bounded, because a byte
count lands differently on arms with different throughput and the samples would
fall outside the load — measuring an idle session and reporting "no
interference" with perfect confidence.

`jump_stab` kills the direct path the way the field kills it: an nft table
(installed through `scripts/vpn_tun_endpoint.sh blackhole on <vm>`, which the
sudoers `scripts/*` glob covers) dropping UDP to and from **one** address, so
the warm TCP relay to that same server keeps working. It then checks the three
promises CLAUDE.md makes and nothing had ever measured on a real path: the outer
SSH session survives, a NEW channel still opens over the relay, the server's
admin API flips `current_path` to `relay` inside a declared budget, the alias
still owns **exactly one** admin row, the path returns to `direct` afterwards,
and a rekey crosses the held session. Each check prints PASS / FAIL / **SKIP** —
a check that could not be evaluated is never a pass, because "no failures
observed" and "no observations" look identical in a summary table. It runs
**last** in the driver: it is the only stage that edits this workstation's
network plane.

The only mode where **bandwidth is not the deliverable**: `ssh -J` carries an
interactive session, so what a user feels is the round trip. Five nested timers
per repetition, because quoting only the last would attribute the inner sshd's
cost to the gateway:

| timer | what it adds |
| --- | --- |
| `tcp` | TCP connect to the gateway — the network floor |
| `wchan` | + outer SSH handshake + `direct-tcpip` (`ssh -W`) |
| `open` | + inner SSH handshake (full `ssh -J`) |
| `chan` | a new channel on an established session |
| `echo` | a byte there and back, no setup at all |

`wchan − tcp` is the gateway's own cost and the only term this project can
change; `open − wchan` is the inner sshd's and it cannot.

**Prerequisite:** the ordinary campaign binary has `vpn` and client-side
`sshjhost` but **not** `--ssh-gateway`, which is server-side. `jump_build_and_deploy`
builds `--features vpn,ssh-gateway` into its **own** `CARGO_TARGET_DIR`
(`target/jump`) and asserts `--ssh-gateway` is really in the resulting binary.

Run `p5_build_gate.sh` **first**: it lints and builds that feature set inside
the build window, where a compile error is a failed gate. Without it the first
thing to compile the combination is `jump_build_and_deploy` — inside a
measurement stage, after a ten-minute build, on a CPU that is part of the
instrument. With a warm `target/jump` the P6 step only copies and checksums,
which is the part that cannot be moved (it is what proves both ends run the
same bytes).
It must not build into `target/release/bore`: that is the artefact the other
campaigns are being measured with and whose checksum their drivers record.

Deploy to the **test VM**, never to staging — a staging redeploy restarts the
server and drops the operator's live tunnels, and needs explicit approval.

---

## 3.10 Qualify the access link BEFORE quoting any absolute figure

This is the most expensive lesson in the whole harness, so it is a numbered step
rather than a footnote.

Every campaign here that used the workstation as a traffic endpoint was first run
over **WiFi**, and WiFi was the bottleneck. Wired, the same link is **930 Mbit/s
down / 737 up**; the campaigns had recorded 150–416 down. Download was
understated between **2.3× and 6×**, and the consequences were not merely smaller
numbers:

* a tunnel delivering 92 % of a crippled path looks healthy;
* three separate campaigns concluded the link was "asymmetric upward" when it is
  asymmetric *downward* by 21 %;
* **three tuning ladders produced optima that do not exist on a wired path** —
  TUN `txqueuelen`, the QUIC datagram send buffer and the UDP socket send buffer
  are all flat wired, and the curves that justified them were the radio.

```bash
scripts/perf/staging/vpn/link_baseline.sh   # the line from sources that never touch bore
scripts/perf/staging/asym_qualify.sh        # whose limit is it?
```

**P=1 against P=8 is the one-line diagnosis:** a per-flow limit (window, loss,
Mathis) opens with parallelism; a link rate or a policer does not.

**The destination is the discriminator for attribution.** A limit that follows
the SOURCE reads the same toward every destination. `asym_qualify.sh` therefore
offers the same load to the test VM, to an unrelated public iperf3 server and to
Cloudflare over HTTPS — three networks, two protocols — and brackets every VM arm
with the instance's ENA allowance delta. Three destinations agreeing is what
proves a ceiling is yours.

**What survives a bad reference and what does not:** every *ratio* against a bare
control sampled in the SAME repetition survives — that is precisely why the
scripts sample it that way. Every absolute Mbit/s does not.

### Re-running a whole campaign against a corrected link

```bash
nohup ./scripts/perf/staging/rerun_eth.sh    & echo "driver pid $!"   # the sweep
nohup ./scripts/perf/staging/rerun_eth_p7.sh & echo "driver pid $!"   # the attribution stages
```

Both drivers are **serial**, **resumable** (a finished stage leaves
`out/eth/_done.<name>`; re-running skips it, a failed stage leaves none and is
retried), **bounded** (`timeout` per stage), and write to an isolated
`BORE_PERF_OUT=out/eth/` so the earlier evidence stays readable beside the new.
Each logs provenance — commit, binary checksum, NIC speed — and re-reads a bare
control before every block so drift is a number rather than an impression.
`rerun_eth_p7.sh` refuses to start while the main sweep is alive.

**Record the PID when you start one.** Stopping a driver by
`pkill -f rerun_eth.sh` matches the shell whose command line contains that
string — which, in an editor or an agent session, is your own. That mistake
killed a driver and its controlling shell once in this campaign; see trap 12.

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
8. **The access link is a confounder until it is qualified.** See §3.10. A bare
   control that is itself the bottleneck does not shrink results, it *invents*
   them: three tuning ladders in this repository produced optima that vanish on
   a wired path.
9. **`sort -n` is locale-dependent.** Under `it_IT` (any comma-decimal locale)
   `{397.46, 264.01, 408}` sorts to `408, 264.01, 397.46` and the median reads
   264.01. `LC_ALL=C` is pinned in `lib.sh`'s `med` and exported in `vpnlib.sh`
   so a locally-defined helper cannot reintroduce it. **A harness that prints a
   statistic must print the raw samples beside it** — this bug is invisible in a
   file that reports only medians, and that is exactly how it was caught.
10. **An apostrophe inside a single-quoted `awk` program ends the program.**
    `awk 'BEGIN{print "bore'"'"'s"}'` — writing `bore's` in the awk text closes
    the shell quote and the rest is parsed as shell. **`bash -n` does not catch
    it**; `shellcheck` does (SC1011/SC1012). Two new scripts shipped with this
    bug before it was swept out. Run `shellcheck` on anything in this tree.
11. **Do not identify a remote process by comparing `comm` to a hardcoded
    binary name.** The campaign binary is *deployed* under its own name
    (`~/bore-vpn`), so `comm` reads `bore-vpn` and a filter for `bore` matched
    nothing — the VM CPU column read 0.00 for an entire campaign. Select by
    EXCLUSION (reject `sudo`/`env`/`sh`/`bash`/empty), which also rejects the
    ssh remote shell matching its own argv.
12. **A pattern that names a script matches any command line containing the
    name — including your own shell's.** This is trap 3 one level up, and it is
    the most expensive one here: `pkill -TERM -f 'rerun_eth.sh'` killed the
    agent shell that issued it *and* the driver. Kill by RECORDED PID. When a
    guard must detect another process, scan by identity and exclude self and
    every ancestor (`rerun_eth_p7.sh::other_driver`), never by text alone.
13. **A buffer ladder must verify its own independent variable.** `net.core.wmem_max`
    on the workstation is 4 MiB, so a 16 MiB rung is a distinct rung only if
    bore's `SO_*BUFFORCE` path bypassed the clamp. Read the granted size from
    the kernel (`effective_send` in the log, or `ss -uapm`) and print it beside
    the rung — **Linux reports `SO_SNDBUF` as twice the requested value**, so the
    expected reading is `2 × rung`. Without that column a ladder whose rungs were
    all secretly the same buffer produces a flat table indistinguishable from a
    real null.
14. **`$$` does not exclude your own command substitution — use `$BASHPID` too.**
    Trap 12's remedy is "scan by identity, exclude self and every ancestor", and
    that remedy has a hole. `found="$(other_driver)"` runs the function in a
    command-substitution SUBSHELL: a fork of bash carrying the **same argv**, so
    `ps` lists it under the script's own name with a pid that is neither `$$`
    (bash deliberately keeps that as the original shell's pid) nor an ancestor of
    it. The guard then matches itself and the driver refuses to start on every
    run. MEASURED while adding a third driver: with `$$` alone the check named
    `bash ./scripts/perf/staging/rerun_vpn_deep.sh` as the offender every time;
    adding `$BASHPID` to the exclusion list prints nothing. `rerun_eth_p7.sh`
    escaped this only because it also excluded its own *name* — which hid the
    defect instead of fixing it, and would have broken the moment its pattern
    widened. Both drivers now exclude `" $$ $BASHPID "` plus the ancestor chain,
    and match every driver name with no name-based self-exclusion.
15. **Two drivers must refuse each other symmetrically.** A guard that names only
    the main sweep lets a second special-purpose driver through. `rerun_eth.sh`,
    `rerun_eth_p7.sh` and `rerun_vpn_deep.sh` all scan for all three.
16. **A stage that dies on an undefined variable looks exactly like a stage that
    ran.** `ws_tunnel.sh`, `ws_flavours.sh` and `ws_dufs_relay_vs_quic.sh` all
    referenced `$B` and none of them defined it; under `set -u` each aborts on
    its first line of real work. The driver logs `FAIL ... elapsed=0s` and moves
    on, so the only signal is the elapsed time — the same signal that exposed
    `vpn_hub`'s silent skip (trap in §3.8 rule 5). Two of the three were still
    queued when the first one failed. Had `$B` resolved, the payload would have
    been written to `scripts/perf/staging/ws/work/`, which **git does not
    ignore**, at 256 MB and 1 GB. Both now use lib.sh's `WORK`, outside the tree.
    Sweep for this class with a scan that compares every `$VAR` against the
    assignments in the file plus `lib.sh`/`env.sh` — but expect false positives
    from `awk -v` names and from `lib.sh`'s multi-assignment lines
    (`V="$BORE_VM"; SRV="$BORE_SRV"; GW="$BORE_GW"`), where only the first name
    sits at the start of a line.

16. **A transfer shorter than the ramp measures the ramp** (V-19). The
    public-tunnel stages moved 96 MiB per arm, sized when this workstation was
    on WiFi and that took about two seconds. Wired, 96 MiB at 922 Mbit/s lasts
    **0.83 s**, most of which is TCP slow start — and the output says so without
    being asked: `pub_ws_dl1` produced paired ratios `0.745 0.984 0.991 1.323
    0.979 1.005` and `pub_ws_carr` produced `0.623 1.029 1.148` on arms that
    differ by exactly one variable. `pub_ws_conns` was worse still at 24 MiB per
    connection: 615 s of stage, of which 600 s was cooldown and ~15 s was
    transfer, across eight cells. The rule generalises past this campaign — when
    the LINE changes, every byte count calibrated against the old one becomes a
    different experiment, and nothing in the output announces it. So the size is
    a variable (`XFER_MB`, wired default 384 MiB; `XFER_MB=96` reproduces the
    old figures) and the first thing to re-derive after a link changes. Related
    but not the same as V-15's corollary, which is about a CONTROL being shorter
    than its arm; this one bites even when both arms are equally short.

    **And the re-run measured the size of the effect, on the same line the same
    night, with `XFER_MB` as the only variable.** `pub_ws_dl1`, six paired
    single-connection downloads:

    | | relay median | spread WITHIN the relay arm | ratios |
    | --- | --- | --- | --- |
    | 96 MiB | 90.91 MB/s | 67.14 -> 94.32 = **40.5 %** | 0.745 · 0.984 · 0.991 · **1.323** · 0.979 · 1.005 |
    | 384 MiB | 106.12 MB/s | 105.19 -> 107.11 = **1.8 %** | 0.805 · 1.002 · 1.002 · 1.012 · 0.997 · 0.994 |

    The relay arm is the SAME experiment in both rows. At 96 MiB it varies by
    40 % between repetitions; at 384 MiB by 1.8 %. The question the stage exists
    to answer -- relay or direct -- was not measurable at 96 MiB and reads
    **0.9995** at 384 MiB. The absolute rate is not a detail either: 90.91 vs
    106.12 MB/s, because the short transfer never leaves the ramp.

17. **A bind that succeeds on the remote host is not reachability from the host
    that measures.** `vpn_relay_attrib` deploys a python instrument to the VM and
    the staging server, starts it, and confirms it is up with `ss -lnt` ON THAT
    HOST -- which passes. From this workstation the three ports it chose
    (5311/5312/5313) are filtered: the AWS security groups admit **5299 only on
    the VM** and **80/443/7835 plus the public-tunnel range on the server**. The
    stage therefore printed `bare-py 0.00`, `leg1 0.00`, `leg2 0.00` beside an
    iperf3 `bare` reading 932/747 Mbit/s, and the per-leg attribution it exists
    to produce was never measured. It cost no bandwidth (the connections never
    opened) and it published no false number (`add()` drops zeros before the
    median) -- it failed by going SILENT, which is the expensive way, and it
    spent roughly two thirds of its wall clock opening connections that could
    not succeed. **And the mechanism is TWO mechanisms**, which is why "pick
    another port" is not the fix: 5311/5312/5313 are refused by the security
    groups (measured in every direction, including server -> VM, so the two AWS
    hosts cannot reach each other on an arbitrary port either), while the
    public-tunnel range 9000+ IS admitted and still unreachable, because the
    staging server runs bore in **Docker** and the nat `DOCKER` chain DNATs that
    whole range to the container (`tcp dport 9031 dnat to 172.18.0.2:9031`). A
    process listening on the HOST there receives nothing -- which is exactly why
    `bore local --port 9031` works: bore listens INSIDE the container. So on a
    host that runs the product in a container, **the host's network plane is not
    the product's network plane**: verify reachability from the measuring end AND
    where the packet is delivered. Unblocking this stage is an infrastructure
    change (open a port pair in the security groups, outside the DNAT range), not
    a script edit; until then the stage must DETECT and skip, not retry.

18. **AWS bills EGRESS only, so half the arms are free -- and a harness that
    ignores direction cuts the free half.** Download (AWS -> here) costs; upload
    (here -> AWS) does not, and this end is flat-rate. Measured on the stages
    this campaign had left: `vpn_overhead`, `vpn_direct_deficit`, `vpn_stability`
    and `vpn_hub` measure in upload ONLY and cost nothing, while `vpn_relay_mtu`,
    `vpn_profile`, `vpn_relay_attrib` and the whole `pub/` block carry `-R`.
    Bigger still is WHERE the bytes go: the VM and the staging server share a VPC
    but the harness addresses both by PUBLIC IPv4, so relay-shaped stages pay for
    every delivered byte TWICE -- measured at **48 %** of the billed total inside
    `vpn_relay_attrib`. That term can be removed without shortening a single
    measurement, and it changes the path under test, so it is decided BETWEEN
    campaigns, never inside one. `aws_cost.sh` prices a window; `cost_watch.sh`
    follows a live driver and writes `out/eth/_cost_stage.tsv` per stage, reading
    the counters only in the gaps between stages.

19. **A variable nothing assigns kills a stage in zero seconds, and NEITHER
    `bash -n` NOR shellcheck can see it here.** `ws_tunnel.sh` died at
    `line 19: B: unbound variable`; the driver logged `FAIL ... elapsed=0s` and
    moved on, and the missing stage was noticed hours later by counting markers.
    The same undefined `$B` had already done it to two sibling stages the same
    evening. `bash -n` passes — the script is syntactically perfect. And
    shellcheck's SC2154 ("referenced but not assigned") **deliberately ignores
    ALL-CAPS names**, on the assumption that they come from the environment;
    every name in this harness is upper case. MEASURED, not read in a manual: a
    file containing only `echo "$UNDEF_THING"` produces no SC2154 at all, while
    the same file with `$undef_thing` produces one.

    So the check is ours: `unbound_scan.sh`, run by `lint.sh`. A name counts as
    bound if it is assigned anywhere in the file, in a sourced `*lib.sh`, or in
    `env.sh.example` — which is the CONTRACT for the coordinates file that lives
    outside the repository and cannot be linted. Only a BARE `$VAR` is a finding:
    `${VAR:-default}` and its relatives are how this harness declares an optional
    knob and are safe under `set -u`. Quoted heredocs (`<<'PY'`), single-quoted
    jq/awk programs and `\$` inside double quotes are all blanked first — those
    dollars belong to another language, and counting them reported 128 files out
    of 132, which is a gate nobody can pass.

    It found three real latent aborts on its first clean run: `$K` (an ssh key
    name that never existed anywhere — `ena_watch.sh` and `server_seq.sh` would
    have died at their first ssh), `$PRETTY_NAME` read bare from
    `/etc/os-release`, and two library contracts (`VPN_LINK_ID`,
    `JUMP_SERVER_EXTRA`) that only worked because every caller happened to
    remember them. The gate covers `scripts/perf/staging/**`; the older one-off
    tools above it are reported as NOTE and do not fail it, because a mixed gate
    would be failed permanently and therefore never read.

20. **A stage array that shadows a library SCALAR prints a coordinate into the
    evidence.** `lib.sh` publishes `S="$BORE_SRV"`; eight stages declared their
    sample table as `declare -A S`. Bash does not clear a scalar when it becomes
    an array -- it keeps the old value as element **[0]**:

    ```
    $ S="1.2.3.4"; declare -A S; S[a]=9; for k in "${!S[@]}"; do ...
      key=[0] val=[1.2.3.4]
    ```

    So every one of those stages printed the staging server's real address into
    its own raw-samples block, under the key `0`, on every run. Found in
    `out/eth/pub_ws_conns_r2.out`, between the quic and relay samples. It never
    reached a median (`med()` refuses a non-numeric sample -- that guard earned
    its keep), but it reached an evidence file, which is exactly how coordinates
    escape here: through prose and output, never through code.

    Gate: `shadow_scan.sh`, run by `lint.sh`, red-checked. The remedy is a NAME,
    not a discipline -- a stage's own tables are `SAMP`, `R`, `D`, `U`, never a
    single letter the library already owns.

21. **`secret_scan.sh` could not see it, and the blind spot is `out/`.** That
    scanner looks at what git would carry -- the diff plus untracked files --
    and `out/` is gitignored, so it reported CLEAN throughout. `--out` scans the
    RESULT files (`.out`, `.tsv`, `.md`, `.txt`; bore's own `*.log` are skipped,
    they carry addresses by design and would bury the one line that matters).

    It is deliberately NOT part of the commit gate: those files are never
    committed, and a permanent warning about a historical file is a warning
    nobody reads. **Run it before quoting a result into a document** -- that is
    the single moment a coordinate crosses from an ignored file into a tracked
    one, and the way every leak in this campaign actually happened. First run:
    28 lines across 11 result files, in three classes -- the shadow bug above,
    stage headers that print the VM's address instead of `<VM>`, and ssh's own
    `Permanently added ...` warnings. None had reached a tracked file.

22. **A PAIRED design whose summary divides MEDIANS has thrown the pairing
    away.** Two arms run inside one repetition for exactly one reason: drift
    cancels only when it is common to both. `median(A) / median(B)` then
    discards that -- the two medians can come from DIFFERENT repetitions, so
    the published quantity belongs to no experiment that was ever run.
    MEASURED in `pub/ws_asym.sh`: it divided repetition 1's download by
    repetition 2's upload, two arms five minutes apart, and printed **1.191**
    where the repetitions taken whole read 1.060 / 1.250 / 1.116, median
    **1.116**.

    The rule: compute the ratio INSIDE the repetition, publish the median of
    those ratios, and print every one of them (V-11 -- a summary statistic with
    no samples beside it cannot be checked). A ratio of medians may still be
    printed for continuity with older single-sample runs, labelled as what it
    is.

    And pair by REPETITION, never by index into the sample arrays: a `keep`
    that drops a failed arm shifts every later element by one, which is the
    same defect reintroduced one level down. `ws_asym.sh` red-checks exactly
    that case (an arm declared FAILED in the middle repetition must not move
    the others).

23. **A legend that names one leg of a two-leg path makes its own counters
    unreadable.** `ws_asym.sh` printed "download = server OUTBOUND", but the
    tunnel is a RELAY: every byte crosses the server twice, in from the VM and
    out to the workstation. So `bw_in_allowance_exceeded` firing during a
    DOWNLOAD looks like an instrument error and is not -- it names the
    VM -> server ingest LEG. An allowance delta convicts a leg, never a
    direction.

25. **Identifying a process by TEXT gives a wrong answer, and this campaign has
    now paid for it three times.** A `pkill -f` killed the session that issued
    it. A driver guard matched its own command-substitution subshell. And on
    2026-09-13, with P7 finished and nothing running, `rerun_eth_p7.sh` refused
    to start: the process it named as "the main sweep still running" was a
    MONITOR whose `bash -c` body MENTIONED the driver while following its log.

    `ps -eo pid,args | awk '$0 ~ /rerun_eth\.sh/'` finds the name anywhere on
    the line, in any field. The sound rule is POSITIONAL: a process is running a
    driver only when the driver's filename is one of the first two arguments of
    its argv, read from `/proc/<pid>/cmdline` -- NUL-separated, so it is
    unambiguous where one argument ends, unlike `ps args` where a filename and a
    sentence quoting it look identical. A `bash -c '<text>'` puts the text at
    argv[2] and can never match.

    It lives in `driverlib.sh` and the four drivers source it. A guard copied
    four times is a defect to fix four times -- and all four copies carried this
    one.

24. **A TOP-LEVEL call to a function defined LATER in the same file is a
    SILENT no-op.** bash resolves a call when the line RUNS, so the call fails
    with `command not found`, exit 127 -- and a call to one's own function is
    the last thing anyone guards with `|| fail=1`, so nothing reports it.
    `bash -n` passes. shellcheck passes.

    MEASURED in this harness's own build gate: `p5_build_gate.sh` called
    `invalidate_binary_gates` at line 176 and defined it at line 203. That
    function is the ENTIRE mechanism by which a product-verdict marker is
    invalidated when the binary changes -- i.e. the thing written so the M-1
    fix would actually be re-verified on the real path instead of being
    reported `SKIP (marker present)`. It would have done nothing, quietly.

    Gate: `order_scan.sh`, run by `lint.sh`, red-checked against that exact
    shape. Scope is deliberately narrow and therefore sound: only TOP-LEVEL
    calls. A call inside another function's body resolves when THAT function is
    invoked, so helpers in any order and mutual recursion are correct and are
    not reported. Quoted strings are blanked first, because
    `trap 'cleanup; assert_clean' EXIT` is the idiom every stage here uses and
    its body is evaluated when the trap fires.

    Corollary applied to `vpn/vpnlib.sh`: install a trap AFTER the functions it
    names. The body is late-evaluated so it is usually harmless -- but a signal
    arriving between install and definition runs a handler in which the check
    is not a command yet, and the cleanup happens while the CHECK silently does
    not.

26. **A helper called in `$( )` CANNOT publish a variable, and the failure is
    silent in exactly the same way.** Command substitution runs the helper in a
    SUBSHELL: every assignment it makes is discarded when that subshell exits.
    The caller then reads the variable's pre-call value -- usually empty -- and
    prints whatever its default was, forever, with nothing in the output to say
    a value was lost.

    Caught in `pub/ws_conns_procs.sh` before its first real run: `cell()`
    computed a headline rate on stdout and set `CELL_WALL` for the cross-check
    column. Written that way the cross-check would have read `n/a` in every cell
    of every run -- and `n/a` in a column that is ALLOWED to be `n/a` is
    indistinguishable from a cell that legitimately had no second reading.

    The fix is the one that cannot rot: everything a helper produces travels on
    stdout, and the caller splits it (`read -r a b <<<"$(helper)"`). Red-checked
    on both shapes it must handle, the numeric one and the `FAILED n/a` one.
    The alternatives -- a temporary file, or `declare -g` from a helper called
    WITHOUT substitution -- both work and both leave the next author free to
    reintroduce this by adding a `$( )`.

27. **A remote path that reaches `scp` is NOT shell-expanded.** OpenSSH 9 made
    SFTP the default `scp` protocol, and SFTP runs no shell: the remote path is
    literal. So `\$HOME/x` -- correct and idiomatic for `ssh host "cmd \$HOME/x"`
    -- becomes a request for a DIRECTORY named `$HOME` the moment the same
    variable is used as an `scp` destination.

    MEASURED on the first P6 run: `scp: dest open "$HOME/bore-jump": No such
    file or directory`, and all three jump stages died in provisioning. The
    spelling had been correct for every OTHER use of that variable, which is
    what made it survive review.

    Rule: a path used on BOTH sides of that line must be ABSOLUTE, and the
    remote home is resolved once (`vm 'printf %s "$HOME"'`) rather than spelled.
    Resolve it loudly -- an unresolved home silently yields `/x`, which fails
    later as a permission error and reads as an unrelated problem.

28. **A PIPELINE IS NOT A CHECK: `cmd | head -c N` exits 0 on zero bytes.**

    `jump_wchan_ms` ended in
    `... | head -c 1 >/dev/null || { echo FAILED; return; }`, above a comment
    asserting that reading one byte proved the channel carried data. It proved
    nothing. `head` succeeds having read nothing at all, and with `pipefail` the
    status it reports is its own, so with NO SSH SERVER BEHIND THE PROVIDER the
    helper published **1270.3 ms** -- the time it took to fail -- as a latency,
    while the sibling helper on the same path honestly reported `FAILED`.

    This is the campaign's signature defect ("a zero that means the instrument
    failed") wearing a pipe as a disguise, and note what did NOT catch it: the
    `INSTRUMENT FAILURE` guard added the day before checks that at least one
    sample exists, and a sample existed. A guard that counts samples cannot
    protect against an invented one.

    Rule: if the CONTENT is what matters, read it into a variable and compare
    it. `jump_wchan_ms` now demands `SSH-` (RFC 4253 requires the
    identification string to start with it).

29. **VERIFY THE PREMISE OF A MEASUREMENT OBTAINED BY SUBTRACTION.**

    The jump campaign's two deliverables are differences:
    `wchan - tcp` is the gateway's cost, `open - wchan` the inner sshd's
    handshake. This workstation has **no sshd** (`openssh-server` is not
    installed, port 22 answers `Connection refused`), and the provider had been
    pointed at `127.0.0.1:22` for the campaign's whole life. An absent term in
    a subtraction does not merely lose a column -- it redistributes onto the
    other one. `jump_require_inner_target` now refuses to let a stage measure
    without it, and `jump_inner_target.sh` provides a real OpenSSH in a
    container on loopback (a container because a system sshd needs root and
    would OUTLIVE the benchmark; loopback because provider -> inner target must
    not cross the WAN, or the round trip lands in the gateway's column).

30. **A REDIRECTION INSIDE `ssh "..."` HAPPENS ON THE FAR SIDE.**

    `ssh host "wc -c > '$LOAD_COUNT_FILE'"` expands the path locally and then
    WRITES IT REMOTELY. `$LOAD_COUNT_FILE` is under this workstation's
    `~/.cache`, so against an inner target that is a different machine every
    rep printed `No such file or directory` and the stage's own floor check
    correctly reported `LOAD FAILED -- 0 bytes crossed the second channel`.

    Rule: capture the far side's **stdout** locally
    (`ssh host "wc -c" > local_file`) instead of writing a remote file. It also
    needs nothing writable on the far side, so the stage works against a host
    the operator does not administer.

31. **THREE USERNAMES LIVE IN THE JUMP CAMPAIGN AND NONE SUBSTITUTES FOR
    ANOTHER.** `JUMP_SSH_USER` is a PRINCIPAL inside bore's gateway (it is not
    a Unix account anywhere) and must match the `ak/<name>` file; used on the
    inner hop it fails with `no such user`. `JUMP_INNER_USER` is a real account
    on the inner sshd; used on the outer hop the gateway answers
    `classic_auth_required`. `BORE_VM_USER` is the VM's own login and belongs
    only to the deploy. `$USER` is none of them. Each wrong pairing fails with
    a message that describes a DIFFERENT problem, which is what makes this cost
    an afternoon rather than a minute.

32. **THE INNER PORT IS A CLIENT-SIDE PARAMETER.** `sshjhost 127.0.0.1:2222`
    registers the alias AT PORT 2222 -- the provider's banner says so -- and the
    gateway matches the port the client requests against the registered one. A
    client asking for `:22` gets `channel 0: open failed: connect failed`,
    which reads exactly like an unreachable origin and is not one. Every
    client-side spelling carries the port: `-W host:$JUMP_INNER_PORT`, and
    `-p $JUMP_INNER_PORT` on each `-J` form (where `-p` is the FINAL
    destination's port; the jump's own port lives inside the `-J` argument).

33. **DO NOT WAIT ON A COMPLETION MARKER INSIDE A FILE A PREVIOUS RUN WROTE.**

    Waiting for a stage with `for i in ...; do grep -q '^DONE' out/stage.out &&
    break; sleep 15; done` matched the PREVIOUS run's `DONE`, because
    `run_stage` truncates the file only when the stage actually starts and the
    driver spends ~20 s on the link baseline first. The loop exited on the
    first iteration and the numbers read back were the old run's -- identical
    to the last byte, which is the only reason it was caught.

    Same family as the stale resume marker (a marker that outlives the thing it
    attests). Rule: wait on the PROCESS (`while kill -0 $pid; do sleep 20;
    done`), or delete the output before starting, or compare mtime. A
    completion marker is only evidence when you know which run wrote it.

34. **ONE DEAD PRECONDITION FORGES A WHOLE COLUMN OF FAILURES.**

    `jump_stab` reported PASS 6 / FAIL 10. All ten were one cause: a
    ControlMaster that had (correctly) ended with its transport, after which
    every check reusing its socket failed -- including the check carrying the
    actual product promise, which then convicted the product of the harness's
    mistake.

    Two rules. First, a check whose precondition is another check's subject
    must RE-ESTABLISH it, not inherit it (`master_reopen`). Second, when a
    stage reports many failures at once, look for ONE shared precondition
    before believing any of them: a product does not usually break in ten
    places simultaneously, and an instrument does.

35. **A MEASURED PROPERTY OF THE DESIGN IS NEITHER PASS NOR FAIL.** An
    `ssh -J` session rides one `direct-tcpip` channel on one QUIC stream, so it
    ends when that stream dies -- there is nothing to fix and nothing to
    celebrate. `jump_stab` records it with a third verdict kind, `OBSV`, counted
    in neither column. Forcing such an observation into PASS hides a real
    limitation from the reader; forcing it into FAIL fabricates a defect and, as
    above, buries the checks that matter.

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
| `docs/performance/SECRET_STAGING_EVIDENCE_2026-09-11.md` | the secret/peer-to-peer campaign |
| `docs/performance/final_secret_perf_review.md` | the same, in Italian |
| `docs/performance/TRANSFER_EVIDENCE_2026-09-12.md` | the `bore transfer` campaign |
| `docs/performance/final_transfer_perf_review.md` | the same, in Italian |
| `docs/performance/VPN_EVIDENCE_2026-09-12.md` | the VPN campaign (WiFi; superseded on every absolute figure) |
| `docs/performance/final_vpn_perf_review.md` | the same, in Italian |
| `docs/performance/ETH_CAMPAIGN_PLAN.md` | the wired window: schedule, rules, what is NOT re-measured and why |
| `docs/performance/ETH_RERUN_EVIDENCE_2026-09-12.md` | **the wired re-run** — which earlier conclusions invert, and which survive |
| `docs/performance/VPN_CAMPAIGN_HANDOFF.md` | campaign state, for picking the work back up cold |
| `docs/performance/VPN_DEEP_DIVE_PLAN.md` | **the VPN coverage matrix** — every CLI option and `BORE_*` knob against the stage that exercises it, and what is still uncovered |
| `docs/vpn/VPN_CTRL_CONN_LEAK.md` | the one open product defect: mechanism, red-check, fix options |

Each of those names the exact script that produced each figure, so a number can
always be traced back to a command in this directory.
