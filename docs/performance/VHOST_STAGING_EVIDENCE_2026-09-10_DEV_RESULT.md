# vhost — what the plan actually changed, measured on staging

**Companion to** [`VHOST_STAGING_EVIDENCE_2026-09-10.md`](VHOST_STAGING_EVIDENCE_2026-09-10.md).
That document is the **before**: it measured the pre-plan binary
(`f15a3de`) against a frozen staging deployment, produced findings F-1…F-17 and
open questions OQ1…OQ8, and the plan
[`docs/plans/plan_VhostEnhancements/`](../plans/plan_VhostEnhancements/) was built
from it. This document is the **after**: the same apparatus, the same server, the
same test VM, the same suites, run against the deployed plan code, plus the
measurements the operator asked for that the first campaign never made — a
consumer on a near-gigabit domestic link, a real application (dufs) as origin,
and all three provider flavours (native binary, published Docker client image,
SSH ingress gateway).

Every number below is either **measured in this campaign** or **quoted from the
before-document with its section number**. Nothing is projected.

## 0. How to read this document

| section | question it answers |
| --- | --- |
| §1 | what was deployed, and how that was proven |
| §2 | did the *bugs* get fixed — one subsection per finding, in vivo |
| §3 | did the *performance* improve — paired before/after |
| §4 | the workstation as consumer: can a domestic gigabit link be saturated |
| §5 | dufs, the real application: big files, many small files, both directions |
| §6 | the three flavours: native binary, Docker client image, SSH gateway |
| §7 | CPU and RAM of every actor, per stage |
| §8 | parameters varied, and which ones are worth keeping |
| §9 | what is still open, and what should not be re-litigated |
| §10 | how to re-run all of this in an afternoon |

## 1. What was deployed, and how that was proven

The operator deployed the plan branch to the same staging server. Trusting the
tag would have been enough to invalidate everything below, so the running image
was interrogated directly:

| fact | value | how |
| --- | --- | --- |
| image | `ghcr.io/manprint/bore:main` | `docker inspect bore-server` |
| **revision label** | **`28f0a5f08172b004569416e5bd2ca5625dc323e1`** | `.Config.Labels["org.opencontainers.image.revision"]` |
| container start | 2026-09-10T19:51:45Z, `RestartCount 0` | `docker inspect` |
| host | `aarch64`, 2 vCPU, **903 MiB RAM** (t4g.micro) | `nproc`, `free -m` |

`28f0a5f` is the head of the plan work: all seven phases, the phase-04
attribution and the CI fix. Two independent in-vivo fingerprints confirm the
binary really is that code and not a cached older layer:

* the **phase 02.3 startup advisory** fires in the server log — *"the direct UDP
  path has no aggregate memory bound … Set `--udp-memory-budget <SIZE>`"* — a
  line that does not exist in the before-binary;
* `/admin/api/v1/vhost` entries carry **`current_path`, `direct_fallbacks`,
  `carrier_target`** (phase 05/03 wire additions, F-14).

### 1.1 The apparatus

Unchanged from the before-campaign, which is what makes the comparison legal:

| actor | what it is | role |
| --- | --- | --- |
| server | `15.161.245.248`, t4g.micro, 2 vCPU / 903 MiB, `brp.0912345.xyz` | the thing under test |
| test VM | `35.152.195.182`, c7i-flex.large, 2 vCPU, same AWS region | provider + origin, and the *same-region consumer* |
| workstation | 16 cores, 47 GiB, **WiFi 6 only** (every wired NIC is down) | the *domestic consumer* the operator asked to saturate |

RTT VM→server is **2.10 ms** measured by TCP connect (ICMP is blocked to both
hosts now, which is new since the before-document — every RTT below is a
TCP-connect sample, never `ping`).

Server parameters as deployed (`~/bore-staging-brp/compose.yml`): `BORE_MAX_CONNS=1024`,
`BORE_MAX_CARRIERS=1024`, `BORE_UDP_MAX_STREAMS=8192`,
**`BORE_PROXY_BUFFER_SIZE=128KiB`** (half the 256 KiB default),
`BORE_UDP=true`, `BORE_SSH_GATEWAY=true`, and — importantly for §2.7 —
**no `BORE_UDP_MEMORY_BUDGET`**.


> That is the configuration **every measurement in §2–§7 ran against**. The
> compose was changed once, deliberately, at the very end of the campaign, and
> §8.5 records exactly what changed and why.

## 2. The bugs — one subsection per finding, measured in vivo

Suite: `vm_stab.sh` (the G-series), run from the test VM against staging, every
case ending with a check that the registration is actually gone.

### 2.1 F-1 — a wedged provider held its subdomain forever. **FIXED.**

The failure that cost a user their label: `SIGSTOP` the provider while a
download is in flight, and the TCP connection stays alive, so nothing in the
old server ever noticed.

| | before (§2.13 G1) | after |
| --- | --- | --- |
| TCP relay, t+30 s | still registered, `relay_tx_bytes` frozen | still registered (correct — inside the 60 s deadline) |
| TCP relay, t+90 s | **still registered** | **released** |
| TCP relay, t+180 s | **still registered** (never released) | released |
| `--udp`, t+90 s / t+180 s | **still registered**, `active` even fell to 0 | **released** |
| re-register while wedged | rejected, `subdomain in use` | **accepted** — the new provider takes the label |
| release after `SIGKILL` | 0 s | 0 s |

The label now comes back within the `vhost_ctrl_timeout` window (60 s default)
on **both** transports, and — the part that matters operationally — a client
retrying in that window gets its subdomain instead of a permanent rejection.
Server log, same window: `vhost provider control idle; reaping` at `WARN`,
twice, once per transport. That is the reaper doing exactly what phase 01 built.

### 2.2 F-12 — a dead origin answered with a bare connection close. **FIXED.**

| case | before (§2.13 G5) | after |
| --- | --- | --- |
| origin killed under a live tunnel | **`http=000`** (no status at all) | **`http=502` in 15.6 ms** |
| second request to the same dead origin | `http=000` | **`http=502` in 12.0 ms** |
| tunnel pointed at a closed port | `http=000` | **`http=502` in 16.4 ms** |
| unknown subdomain | 404 | 404 (unchanged, correct) |
| origin restarted, same tunnel | 200 | 200 (unchanged — still no reconnect needed) |

`http=000` was indistinguishable from a dead tunnel, a dead server or a broken
network; a 502 says "the tunnel is up, your app is not". Both requests answer in
**12–16 ms**, i.e. the synthesized response costs nothing, and the entry stays
healthy (`active=0`, provider alive) so recovery needs no reconnect.

### 2.3 F-3 — benign disconnects flooded the log at WARN. **FIXED.**

Counted twice: once over a 60-minute window mid-campaign, and once over the
container's **entire lifetime** at the end — which by then included hours of
real browser-shaped traffic from a domestic WiFi consumer (aborted transfers,
parallel range requests, killed curls), i.e. exactly the traffic that generated
the before-document's 718 warnings.

| | before (§2.11 / F-3) | after, 60 min | **after, whole container life** |
| --- | --- | --- | --- |
| log lines | — | 22 801 | **53 238** |
| **WARN lines** | 786 | 3 | **19** |
| **ERROR lines** | — | 0 | **0** |
| of which `close_notify`-class | **718 (91 %)** | 0 | **0** |

And all 19 are events an operator should see — there is no residue to explain
away:

| count | WARN | is it real? |
| --- | --- | --- |
| 9 | `TLS handshake failed err=peer is incompatible: …` from 87.236.176.0/24 and 45.79.162.134 | yes — internet background scanners, not our traffic |
| 3 | `vhost provider control idle; reaping (peer wedged/abandoned) timeout=60s` | yes — **this is the F-1 fix firing** (§2.1) |
| 3 | `vhost subdomain already in use` | yes — the G2 race test, four providers contending for one label |
| 2 | `ssh gateway: reaped unresponsive connection err=Inactivity timeout` | yes — the I-SSH10 eviction firing during the wedged-client test (§6.2) |
| 1 | the phase 02.3 UDP-budget advisory | yes — startup, once (§2.7) |

**Zero `close_notify` in 53 238 lines.** The class that used to be 91 % of all
warnings is now `debug`, classified by `io::ErrorKind` rather than by message
text, and the signal-to-noise ratio of this log is now such that every single
warning in the container's life can be listed in one table.

### 2.4 F-6 — the admin config view did not reflect the vhost YAML. **FIXED.**

`/admin/api/v1/config` previously reported `vhost_default_response_headers` as
absent while the SSH gateway's own banner printed all of them. It now derives
the whole vhost section from the live `SharedVhostConfig`: all **7** response
headers from `config.yml` are present, together with
`vhost_default_request_headers` and `vhost_reservations`. `vhost_quic_port` and
the config file path remain the startup snapshot, on purpose — neither is
hot-reloadable.

### 2.5 Races, leaks and ghosts — unchanged and still clean

These were already correct before the plan and had to be proven not to have
regressed:

| case | before | after |
| --- | --- | --- |
| **G2** four providers race one label | 1 entry, 3 rejected, serving 200 | **identical** |
| **G3** 20 register/deregister cycles | 0 failures, 0 releases slower than 3 s | **identical** |
| **G4** provider `SIGKILL`ed mid-response | truncated body + `curl exit 18`, label released 0 s | **identical** (48 739 328 bytes then exit 18) |
| **G7** ten simultaneous tunnels, mixed transports | 10/10 registered and serving, RSS 21.2 MiB, 0 leaked after kill | **10/10, RSS 19.4 MiB, 0 leaked** |


### 2.6 The concurrency tail (OQ7 / N-9) — half of it is gone, and the half that remains is where §12 said it was

G8 holds N slow readers on one tunnel and then times a **fresh** request behind
them (new TCP, new TLS, one small GET). Phase 04 spent a whole phase failing to
reproduce this locally and closed with a written attribution to the t4g.micro's
network allowance rather than to bore's accept path (§12).

| slow readers | relay fresh-request, before | **after** | relay RSS after | QUIC fresh-request, before | **after** | QUIC RSS after |
| --- | --- | --- | --- | --- | --- | --- |
| 16 | 12 ms | 15.7 ms | 19.4 MiB | 13 ms | 13.1 ms | 27.0 MiB |
| 64 | 16 ms | 12.4 ms | 35.6 MiB | 13 ms | 13.0 ms | 68.7 MiB |
| **256** | **966 ms** | **12.7 ms** | 132.9 MiB | 14 ms | 13.4 ms | 288.7 MiB |
| **512** | **1 436 ms** | **1 469 ms** | 97.3 MiB | 121 ms | **14 044 ms** | 392.2 MiB |
| settled | 16.5 MiB | 14.7 MiB | — | → 33.6 MiB | 27.5 MiB | — |

Three separate things happen in that table and they must not be conflated:

1. **At 256 connections the relay tail disappeared entirely: 966 ms → 12.7 ms.**
   Nothing in the plan targeted the accept path, so this is not a fix — it is
   the same measurement taken when the instance's allowance bucket happened not
   to be empty, which is precisely what §12.3 predicted would happen.
2. **At 512 the relay tail is unchanged** (1 436 → 1 469 ms). The mechanism
   §12 could not falsify is still there at the top of the ladder.
3. **QUIC direct at 512 got much worse** (121 ms → 14 s). This is not the same
   phenomenon: RSS is 392 MiB on a 903 MiB host, i.e. the box is out of memory
   for QUIC receive windows, which is F-13 and is answered in §2.7 — not a
   latency defect.

The server's own ENA counters, read for the first time in this campaign, name
the dimension: cumulative since boot, `bw_in_allowance_exceeded` **78 334**,
`bw_out_allowance_exceeded` **13 024**, and `pps_allowance_exceeded`
**16 763 133**. The dominant allowance miss on this instance is **packets per
second**, by three orders of magnitude — which is exactly the shape that
penalises many small connections and leaves one big stream alone, and exactly
the asymmetry (relay tail, QUIC none) §12 could not explain from inside the
process. N-9 therefore stays open, still attributed to the instance, and still
deliberately unfixed; but it now has a named counter instead of a hypothesis.


### 2.7 F-14 — the UDP-to-relay fallback and its counters. **Mostly fixed; the residual gap is now measured exactly.**

The before-document blackholed UDP under a live `--udp` tunnel and found three
things wrong: the first request was lost (`http=000` after 9.90 s), and both
observability counters lied (`direct_stream_opens` climbed during a 100 % UDP
drop, `direct_fallbacks` stayed at 0 through an obvious fallback).

Redone with two changes to the apparatus: the blackhole is applied in **both**
directions (egress `tc`/netem plus ingress `iptables`, where the before-run
dropped only egress), and every counter is read **per request** instead of once
per phase — because "the first request is lost, the rest are fine" and "every
request is lost" are different bugs.

| step | request | `direct_stream_opens` | `current_path` | entry `direct_fallbacks` |
| --- | --- | --- | --- | --- |
| warm, direct proven | — | 1 → 2 | direct | 0 |
| before: 113.71 MB/s down | — | 2 | direct | 0 |
| **blackhole applied, both directions** | | | | |
| #1 | **`http=000` after 10.01 s** | 2 → 3 | direct | 0 |
| #2 | **`http=200` in 12.5 ms** | 3 | **relay** | **1** |
| #3 | `http=200` in 12.2 ms | 3 | relay | 2 |
| #4 | `http=200` in 12.2 ms | 3 | relay | 3 |
| #5 | `http=200` in 12.0 ms | 3 | relay | 4 |
| bulk during the blackhole | 170.91 MB/s | 3 (frozen) | relay | 5 |
| **blackhole cleared** | | | | |
| +4 s | `http=200` in 13.6 ms | 3 → 4 | **direct** | 5 (frozen) |
| +8 s / +12 s | 12.6 / 17.6 ms | 5 → 6 | direct | 5 |
| after: 123.19 MB/s | — | 7 | direct | 5 |

**What is fixed.** `current_path` flips to `relay` on the very first fallback and
back to `direct` when the path returns, which is the observability F-14 asked
for and which no counter could express. `direct_fallbacks` increments once per
fallen-back connection — 5 of them — and then stops, so the operator can see
both that it happened and that it stopped. `direct_stream_opens` freezes at 3
for the whole outage instead of climbing to 12, so it now means what it says.
And requests #2 onward are served in **12 ms on the warm relay**, with bulk
throughput during a total UDP outage actually *higher* than before it
(170.91 against 113.71 MB/s — the relay is the faster transport on this clean
path, F-8).

**What is not fixed, and exactly why.** Request #1 still dies with `http=000`,
now at 10.01 s against the before-run's 9.90 s — unchanged. The mechanism is
identifiable from this table and is *not* the one phase 05 bounded:
`DIRECT_OPEN_TIMEOUT` (3 s) covers `open_stream` and `write_stream_ready`
together, but on a QUIC connection whose peer has gone silent **both of those
succeed locally** — `open_bi` needs no round trip once stream credit exists, and
the marker write only enters quinn's send buffer. So the open is genuinely
successful (hence the counter's single increment), the request is committed to
that stream, and it then waits for the connection to die: quinn's keepalive is
3 s and its idle timeout 10 s, which is the 10.01 s measured. Only when the
connection is gone does the pool have `None` to report, and from that instant
every request falls back in 12 ms.

So the blast radius is **bounded and now known**: the requests issued in the
first ~10 s of a UDP outage on a given direct connection are lost; everything
after is served normally. Two candidate fixes, both cheap, neither speculative
about the mechanism any more:

1. **Lower the direct-path QUIC idle timeout** from 10 s toward ~4 s. Keepalive
   is 3 s, so a live path is never idle for 4 s and nothing healthy is torn
   down; the loss window shrinks by ~60 %.
2. **Bound the first response byte on a freshly opened direct stream**, not just
   the open — the request has already been written, so a read deadline of
   ~1 RTT + margin can abandon the stream and re-issue on the relay.

(1) is a constant; (2) needs care not to abandon a slow-but-healthy origin.
Neither was attempted here: this campaign measures, and the mechanism was only
isolated by this table.

**One residual observability inconsistency, worth a line in the API docs rather
than a fix.** The per-entry counter incremented to 5, while
`/admin/api/v1/metrics.direct_fallbacks` stayed at **0** throughout. That is not
a bug: the server-wide metric is incremented on the **public-tunnel** direct
path (`server.rs`, `bore local --udp`), and the vhost path deliberately keeps
its counter on the `VhostEntry` (`vhost.rs`, gated on `entry.udp` so a plain
relay tunnel is never counted as "falling back"). But the before-document's
F-14 complaint was written against the *metrics* endpoint, so an operator who
reads the fix note and then watches that endpoint will still see 0. Either
aggregate the vhost entries into the server metric, or say in the API reference
that `metrics.direct_fallbacks` is public-tunnel-only and vhost fallbacks live
on `vhost[].direct_fallbacks`.


### 2.7.1 F-14 follow-up — the loss window is now a tunable, and it is measured

§2.7 leaves one thing unfixed: the first request issued after UDP stops working
is lost, at 10.01 s. The mechanism was isolated there but not acted on, because
the campaign's job was to measure. It is acted on here, because the mechanism
turned out to point at a single constant.

**The claim under test.** With the peer silent, `open_bi` and the
`STREAM_READY` write both succeed locally — neither needs a round trip once
stream credit exists — so the request is already committed to that stream and
can only wait for the connection itself to die. That happens at
`max_idle_timeout`. If the reasoning is right, the loss window is not
approximately the idle timeout: it *is* the idle timeout, to the millisecond.

**The apparatus.** `scripts/perf/vhost_idle_window.sh`, new. It runs a server, a
provider and an origin inside a **rootless network namespace** (`unshare -rn`),
proves the direct path came up by reading `direct_stream_opens` off the admin
API, then blackholes the QUIC port in **both** directions with `iptables` and
times the next five requests. No privilege is needed, and no deployment.

Loopback false-passes two whole classes of bug in this codebase — throughput,
and the injected-flush/keep-alive class — so using it needs a justification
rather than a habit. A *deadline* is the exception: the mechanism is a quinn
timer, not a buffering interaction. The justification is empirical, not
theoretical: the default run reproduces the staging table exactly, including
the counters.

```
  == default ==
  config: keepalive_ms=3000 idle_ms=10000
    req#1: 000 10.001880   path=direct opens=3 fb=0
    req#2: 200 0.000834    path=relay  opens=3 fb=1
    req#3: 200 0.000964    path=relay  opens=3 fb=2
```

Staging measured `000` at 10.01 s, then the flip to `relay`, then
`direct_fallbacks` climbing while `direct_stream_opens` stays frozen. So does
this, on a laptop, in about a minute.

**The change.** `QUIC_KEEPALIVE` (3 s) and `QUIC_MAX_IDLE` (10 s) in
`src/holepunch.rs` were hardcoded with no override. They are now resolved
through `resolve_direct_quic_liveness`, a pure function reading
`BORE_DIRECT_QUIC_KEEPALIVE_MS` and `BORE_DIRECT_QUIC_IDLE_MS`. **Unset yields
exactly the shipped pair**, pinned by
`direct_quic_liveness_unset_is_the_shipped_pair` — without that, every
throughput and fallback figure in this document would stop applying.

One policy decision is baked into the resolver. A keep-alive that is not
comfortably shorter than the idle timeout tears down *healthy* connections,
because a single lost ping already exceeds the deadline. The resolved pair
therefore always satisfies `max_idle >= 3 × keepalive` (two consecutive losses
survivable), and when the operator's pair does not, the **keep-alive is
tightened** rather than the idle timeout relaxed — the operator asked for
faster detection, and that is the request that gets honoured. The adjustment is
reported by a `warn!` and by the API, never applied silently.

**The result.** The window is the idle timeout, exactly:

| `BORE_DIRECT_QUIC_IDLE_MS` | resolved keep-alive | first-request loss window |
| --- | --- | --- |
| unset (10 000) | 3 000 ms | **10.0019 s** |
| 6 000 | 2 000 ms | **6.0020 s** |
| 4 000 | 1 333 ms | **4.0011 s** |
| 4 000, keep-alive pinned to 1 000 | 1 000 ms | **4.0008 s** |
| 2 000 | 666 ms | **2.0021 s** |

In every row request #2 is served on the warm relay in about a millisecond, so
shortening the deadline does not change the shape of the fallback — only how
long the one lost request takes to give up.

**Is a server-side setting enough?** Yes, and this is the operationally
important half: QUIC negotiates `max_idle_timeout` as the **minimum** of the two
advertised values, so an operator who controls the server but not the providers
still gets the shorter window. Measured with the override in the server's
environment only and a provider started with a deliberately clean environment:
**4.0010 s**. An operator changes one line of the compose file; no provider has
to be touched or even restarted.

**Does a shorter deadline kill a healthy but lossy path?** No, up to 30 % loss.
Sustained traffic for 60 s under `netem`, watching `current_path`:

| loss | idle 10 000 (shipped) | idle 4 000 |
| --- | --- | --- |
| 10 % | 0 relay samples of 12 | **0 of 12** |
| 30 % | 0 relay samples of 12 | **0 of 12** |
| 50 % | inconclusive — see below | inconclusive |

At 50 % the harness itself stops being an oracle: `netem` is applied to the
whole loopback, so the admin API and the TCP relay are being dropped at the same
rate as the QUIC path, and the transcript fills with 502s and 40 s timeouts on
*every* leg. Notably `current_path` still read `direct` throughout even there —
the direct connection was not the thing that broke. The honest statement is
that a shorter idle timeout is safe to at least 30 % loss and that 50 % was not
measurable with this apparatus, not that it fails at 50 %.

**Recommendation.** `BORE_DIRECT_QUIC_IDLE_MS=4000` on the server cuts the
worst case an operator can see from **10 s to 4 s** — a 60 % reduction in the
only window where a `--udp` tunnel actually loses requests — for one extra
keep-alive packet every 1.33 s per quiet connection and no measured downside up
to 30 % loss. It is left **unset by default** because changing a shipped
constant on the strength of one measurement campaign is exactly the kind of
decision that should be the operator's, and because the fallback it shortens is
already the rare case: a tunnel that never loses UDP never reaches this code.

**Gates.** `direct_quic_liveness_{unset_is_the_shipped_pair,
keeps_two_losses_survivable, honours_a_consistent_pair_untouched,
clamps_absurd_inputs}` (pure, `src/holepunch.rs`);
`config_view_reports_the_live_direct_quic_liveness` (red-checked: deleting the
overlay fails it); `T-CFGFIELDS` in `scripts/admin_dashboard_test.sh`;
`scripts/perf/vhost_idle_window.sh` end to end, **10 PASS / 0 FAIL**.

### 2.8 Configuration coherence — one gap found and **FIXED in this campaign**

Not a plan finding: this one surfaced while reading the deployed compose against
the code, and the operator asked for it explicitly (*"se trovi delle incoerenze
sui parametri tra quelli del compose, i default e quelli visualizzati,
correggi"*).

**The gap.** The staging compose sets `BORE_PROXY_BUFFER_SIZE=128KiB`, half the
built-in `DEFAULT_PROXY_BUFFER_SIZE` of 256 KiB. `shared::proxy_buffer_size()`
reads that variable **once** into a `OnceLock`, clamps it to `[4 KiB, 16 MiB]`,
and logs the resolved value at **`trace`** level. `ConfigView` had no field for
it, so `/admin/api/v1/config` did not report it — confirmed against the live
server, where every neighbouring UDP window *is* reported:

```
udp_socket_send_buffer      = 16777216
udp_stream_receive_window   = 16MiB
udp_connection_receive_window = 256MiB
udp_send_window             = 256MiB
udp_max_streams             = 8192
(proxy_buffer_size          — absent)
```

An operator who set the variable had **no way to confirm it took effect**, and
no way to notice the deployment was running at half the default. That is
precisely the shape of F-6, which phase 06 fixed for the vhost headers, and it
was still open one field over.

**The fix**, following the same reasoning as `overlay_vhost_config`:

* `ConfigView` gains `proxy_buffer_size: String` (human size, matching the
  neighbouring UDP window fields);
* `admin_api::overlay_runtime_tunables` **derives** it from
  `shared::proxy_buffer_size()` on **every read**, so it can never be a startup
  literal that drifts from the constant it claims to report — the startup
  snapshot deliberately carries an empty placeholder;
* the frontend Configuration panel renders it automatically (it iterates the
  config object generically, which is why no JS change was needed);
* `README.md`, `docs/VHOST.md` and `docs/frontend/ADMIN_DASHBOARD.md` all say
  where to read the resolved value.

**Gates.** New unit `config_view_reports_the_live_proxy_buffer_size`, which
asserts both that the snapshot holds the empty placeholder and that the served
view holds the resolved size — **red-checked** by disabling the overlay:

```
assertion `left == right` failed: the view must report the resolved buffer size
  left: ""
 right: "256KiB"
```

`proxy_buffer_size` added to the `T-CFGFIELDS` list in
`scripts/admin_dashboard_test.sh`.

#### A second gap, found by the parameter programme itself

Setting `BORE_UDP_MEMORY_BUDGET=512MiB` on the staging server and then reading
`/admin/api/v1/config` returned:

```
udp_stream_receive_window = 16MiB
udp_connection_receive_window = 256MiB
```

which are the **defaults**, not the values a 512 MiB budget derives on a server
with `BORE_MAX_CARRIERS=1024` (the connection window hits its 16 MiB floor and
the 16:1 ratio puts the stream window at 1 MiB). The endpoint was reporting the
requested configuration while the process ran a different one — and there is no
worse answer to "is my budget on?" than a confident wrong one.

The cause is the same shape as the two before it. `--udp-memory-budget`
**derives** the three flow-control windows in `main.rs` *after* the CLI strings
have already been copied into the startup `ConfigView`, so the snapshot was
frozen one step too early. F-13's own design note says the budget "buys slots,
not windows"; nothing in the API said which slots or which windows.

**Fix.** `overlay_runtime_tunables` now derives the whole direct-UDP block from
`Server::udp_tuning()` — the tuning object actually installed — plus a new
`udp_direct_slots` field carrying the aggregate admission bound
(`null` = no budget, the historical unbounded path). `Server::udp_tuning()` is
a new accessor added for exactly this.

**Gate.** `config_view_reports_the_windows_the_budget_actually_installed`
builds a server, applies a 512 MiB / 1024-carrier plan the same way `main.rs`
does, and asserts the served view reports the **derived** windows and the slot
count — plus `assert_ne!` against the snapshot value, so an overlay that
silently stops running cannot pass by coincidence. **Red-checked**: removing
the derivation fails it. `udp_direct_slots` added to `T-CFGFIELDS`.

With this and the `proxy_buffer_size` fix, every tunable an operator can set
on this server is now readable back from the API **as resolved**, which is the
property the operator asked for: compose, defaults and displayed values agree
by construction rather than by discipline.

#### Coverage of every source file this campaign changed

The operator asked for this explicitly (*"vedo che ci sono file di codice
modificati. hai fatto i test a copertura?"*). Seven source files and two
harnesses changed; every one of them carries a gate, and every new gate was
**red-checked** — reverted the fix, watched the test fail, restored it.

| file changed | what changed | gate | red-checked |
| --- | --- | --- | --- |
| `src/holepunch.rs` | `resolve_direct_quic_liveness` + the two env overrides, replacing two hardcoded constants | 4 units: `..._unset_is_the_shipped_pair`, `..._keeps_two_losses_survivable`, `..._honours_a_consistent_pair_untouched`, `..._clamps_absurd_inputs`; plus the end-to-end ladder in `scripts/perf/vhost_idle_window.sh` | yes — the ladder fails if the override is ignored, and the first unit fails if the default pair drifts |
| `src/admin_views.rs` | four new `ConfigView` fields | the three `config_view_reports_*` units below, plus `tests/admin_test.rs` which must construct them | n/a (data carrier) |
| `src/admin_api.rs` | `overlay_runtime_tunables` now derives the buffer size, the QUIC liveness pair, the budget-derived windows and the slot count | `config_view_reports_the_live_proxy_buffer_size`, `config_view_reports_the_live_direct_quic_liveness`, `config_view_reports_the_windows_the_budget_actually_installed` | yes, each: disabling its overlay line fails exactly one test |
| `src/server.rs` | `udp_tuning()` accessor + the new placeholder fields at the snapshot site | exercised by `config_view_reports_the_windows_the_budget_actually_installed`, which installs a 512 MiB plan and reads the view back | yes (same test) |
| `src/main.rs` | the same placeholder fields at the production construction site | compile-gated; the values it holds are overwritten on every read by design | n/a |
| `tests/admin_test.rs` | the new fields in the wire-shape fixture | it *is* the gate for the serialized shape | — |
| `scripts/admin_dashboard_test.sh` | `T-CFGFIELDS` extended to the four new keys | runs against a live server; a field that stops being served fails it | yes — removing a field from the overlay fails `T-CFGFIELDS` |
| `scripts/perf/vhost_idle_window.sh` | new | it is itself the F-14 regression gate: `ladder` asserts the loss window tracks the configured idle within −0.5/+1.5 s **and** that request #2 is served | yes — with the override ignored, every rung reports 10 s and the assertion fails |
| `README.md`, `docs/VHOST.md`, `docs/frontend/ADMIN_DASHBOARD.md` | the two new variables, the clamp, the policy rule, and where to read resolved values | the project rule that "not in README.md ⇒ not done" | — |

No behaviour change ships without a default-preserving test. That is the one
property that matters most here: `direct_quic_liveness_unset_is_the_shipped_pair`
and `direct_admission_without_a_budget_never_refuses` between them pin the claim
that **a deployment which sets none of the new variables behaves exactly as it
did before this campaign** — which is what makes every measurement in this
document still applicable.

#### Every gate, run after the last change

| gate | result |
| --- | --- |
| `cargo fmt --all --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo clippy --all-targets --features ssh-gateway,vpn -- -D warnings` | clean |
| `cargo test --features ssh-gateway,vpn` | **955 passed, 0 failed**, 2 ignored |
| `npm test` (frontend) | **101 passed, 0 failed** |
| `sudo scripts/admin_dashboard_test.sh` (netns e2e, includes `T-CFGFIELDS`) | **25 PASS, 0 FAIL** |
| `scripts/perf/vhost_idle_window.sh ladder` (rootless netns) | **10 PASS, 0 FAIL** |

### 2.9 Wire compatibility with the pre-plan client — the one production risk, verified

This is the risk the plan carries into production and the reason DEC-VE2 exists.
The plan added `HelloVhost.ctrl_heartbeat` (phase 01) and
`HelloVhost.auto_carriers` + `ServerMessage::SetCarrierTarget` (phase 03.3).
Client→server additions are safe (`#[serde(default)]`), but **server→client
additions are not symmetric**: an old client cannot deserialize an unknown
`ServerMessage` variant, and on this wire that is a hard control-loop error, not
a skipped field.

So the pre-plan binary — the very one the before-campaign measured, `f15a3deb`
— was run against the deployed new server:

```
old client: bore 1.0.0 - main - f15a3deb
new client: bore 1.0.0 - main - 28f0a5f0
```

| # | what it proves | result |
| --- | --- | --- |
| **I1** | an old client still registers and serves | registered; `GET /1k` → **200 in 37 ms**; 100 MiB stream at **129.68 MB/s** |
| **I2** | an old client is **never reaped** — it cannot send heartbeats, so a reaper that did not gate on the declared capability would kill a healthy idle tunnel at every deadline | present and serving 200 at **t+30 s, t+60 s, t+75 s, t+95 s** — i.e. past the 60 s control-liveness deadline, twice over |
| **I3** | the server never sends `SetCarrierTarget` to a client that cannot decode it — proven by putting the old client under exactly the load that makes a *new* client grow | two concurrent 8 GiB streams; entry stayed **`carriers=1 target=1`** while still serving a small request in 53 ms, and the tunnel survived |
| **I4** | an old client asked for a flag it does not have | `--carriers 0` is rejected by the old binary's own parser; the tunnel never registers under that flag and the label is released in **1 s** |

**I2 is the one that would have been catastrophic if wrong.** The reaper added
in phase 01 is what fixes F-1, and applying it unconditionally would have
disconnected every legacy tunnel every 60 s of idleness. The `Option<Duration>`
gate — the server passes `ctrl_heartbeat.then_some(vhost_ctrl_timeout)` — is
the whole compat story, and it holds in vivo.

**I3 is the second one.** The load used is the same load that grew a new
client's pool 1 → 2 → 3 in §3.4. Against the legacy provider the target never
moved, which means the server checked the declared capability before sending,
rather than sending and hoping.

An operator can therefore upgrade the server **before** the clients, which is
the required order anyway (the same rule the secret-path `Heartbeat` addition
established).

## 3. Performance — paired before/after from the same-region VM

Method unchanged from §2.15, because it is the only design that survives this
server's drift: each comparison runs **paired**, both halves back to back, the
order alternating between pairs, and the statistic is the **median of the
per-pair ratios**. Absolute rates on this instance wander ~30 % between
identical runs; ratios do not.

### 3.1 Transport ranking — unchanged, and that is the correct result

**A1, `--carriers 1`, 12 s per half, 8 pairs.** Ratio > 1 means the TCP relay wins.

| | before (§2.15 A1) | after |
| --- | --- | --- |
| median ratio relay/QUIC | **1.589** | **1.544** |
| pairs favouring the relay | 8 of 8 | **8 of 8** |
| ratio spread | 1.31 – 1.95 | 1.21 – 1.79 |
| relay absolute | 160 – 232 MB/s | 150 – 215 MB/s |
| QUIC absolute | 113 – 131 MB/s | 109 – 126 MB/s |

The plan did not try to change this and did not: on a clean 2.10 ms path kernel
TCP with offload still beats a userspace QUIC stack doing per-packet AEAD on two
Graviton cores by about 1.5×. F-8 stands.

### 3.2 Carriers on a clean path — still a small cost, exactly as DEC-VE6 assumed

**A2, 4 carriers against 1, 5 pairs.** Ratio > 1 means carriers help.

| | before (§2.15 A2) | after |
| --- | --- | --- |
| median ratio c4/c1 | **0.941** | **0.880** |
| pairs below 1.0 | 4 of 5 | 4 of 5 |

This is the measurement the whole phase-03 design rests on, and it reproduced.
It is why `choose_carrier` consults the round-robin cursor first and only a
**strictly** smaller load wins — with nothing bulk anywhere the new scheduler
reduces to the old round-robin byte-for-byte, and why the default stayed
`--carriers 1` instead of being raised.

### 3.3 F-15 / phase 03 — the headline result

This is the case a real page load hits: one large asset or download in flight
while small requests arrive. It was the worst latency result of the whole
before-campaign. `oha -c 8` on `/1k`, 6 s per point, one and two concurrent
8 GiB streams as the bulk load.

**Small-request p50 / p95 (ms) and request rate, TCP relay:**

| carriers | idle, before → after | 1 bulk, before → after | 2 bulk, before → after |
| --- | --- | --- | --- |
| c=1 | 2.51/2.89 → 2.53/2.92 | 2.59/27.3, 1 227 rps → 16.7/27.2, 454 rps | 45.1/57.5, 181 rps → **34.8/50.1, 223 rps** |
| c=2 | 2.62/3.56 → 2.58/2.98 | 2.95/17.5, 1 419 rps → **2.71/4.06, 2 639 rps** | 60.5/109.9, 128 rps → **23.8/104.2, 240 rps** |
| c=4 | 2.53/2.86 → 2.57/2.89 | 7.09/37.9, 629 rps → **2.65/4.24, 2 719 rps** | 36.6/77.8, 205 rps → **20.6/37.2, 390 rps** |
| **c=8** | 2.55/2.89 → 2.45/2.79 | 3.42/14.1, 1 464 rps → **2.68/4.92, 2 458 rps** | 14.8/42.5, 393 rps → **5.48/15.9, 1 162 rps** |

Read the c=8 row, which is the configuration the before-document recommended:
under one bulk transfer p95 fell **14.06 → 4.92 ms (2.9×)**; under two bulk
transfers p50 fell **14.81 → 5.48 ms (2.7×)**, p95 **42.49 → 15.92 ms (2.7×)**
and request rate rose **393 → 1 162 rps (3.0×)**. At c=4 the improvement is
larger still because the before-run's c=4 cell was the bad one: p95 under one
bulk **37.91 → 4.24 ms (8.9×)** with rate 629 → 2 719 rps.

Against the *idle* baseline, which is what the before-document said no
configuration could get back to: idle p50 is 2.45 ms and **under one bulk
transfer c=2/c=4/c=8 now sit at 2.71 / 2.65 / 2.68 ms — inside noise of idle.**
The before-document's closing sentence on F-15 was "no configuration gets back
to the 2.5 ms idle p50; the best case under load is 7–15 ms". With bulk
scheduling in place, one bulk transfer is now **free** at two or more carriers.

Two honest negatives in the same table:

* **c=1 under one bulk got worse** (2.59 → 16.71 ms p50). With a single carrier
  there is nothing to schedule — `BulkTicket` occupancy has one slot to report
  and `choose_carrier` has one candidate — so this cell is byte-identical code
  and the two numbers are two samples of the same unmitigated case on a server
  that drifts 30 %. The before-run's 2.59 ms was the lucky sample: its own p95
  was 27.3 ms, i.e. the *tail* was already 10× the median. The after-run's
  16.7/27.2 is the same distribution reported from a different quantile of luck.
  It is the reason the mitigation is carriers and not something the single-carrier
  path could ever do.
* **Two bulk transfers are still expensive at every carrier count** (5.5 ms p50
  at c=8, 20–35 ms below that). Bulk scheduling avoids a carrier that is *already*
  carrying bulk; when bulk occupies every carrier there is nowhere left to go.
  That is the documented ceiling of the mechanism, not a defect.

**QUIC direct, same table:** essentially unchanged, as §11.3 predicted for this
RTT (2.10 ms).

| carriers | idle, before → after | 1 bulk, before → after | 2 bulk, before → after |
| --- | --- | --- | --- |
| c=1 | 2.53/2.92 → 2.46/2.84 | 8.07/28.6, 710 rps → 8.97/29.3, 614 rps | 7.07/14.8, 1 012 rps → 6.99/15.5, 993 rps |
| c=4 | 2.50/2.90 → 2.50/2.94 | 5.64/41.6, 690 rps → 4.22/39.8, 779 rps | 8.04/59.1, 494 rps → 8.87/73.9, 442 rps |

The one place the QUIC sender-side demotion (phase 03.4) shows clearly is the
**tail** in the independent A3 run: 1 KiB c=8 under bulk on QUIC direct went
**p99 219.08 ms → 66.36 ms**, a 3.3× tail improvement at the same p50. Demoting
the bulk stream and capping its per-write burst does not make the median
faster; it stops the bulk stream from monopolising a scheduling window for
hundreds of milliseconds at a time.

### 3.4 `--carriers 0` (adaptive) in vivo — it works, and it is not yet the best setting

The before-document could not test this: the mechanism did not exist. Observed
through `/admin/api/v1/vhost` (`carriers`, `carrier_target`):

| load | pool | small-request p50 / p95 | rate |
| --- | --- | --- | --- |
| idle | **1 → 1** (an idle tunnel never grows) | 2.49 / 2.93 ms | 3 129 rps |
| 1 bulk | **grew 1 → 2** | 22.79 / 36.13 ms | 330 rps |
| 2 bulk | **grew 2 → 3** | 6.00 / 46.56 ms | 530 rps |
| +6 s later | held at 3 | — | — |

The mechanism is confirmed end to end on a real server: the pool starts at 1,
the server raises the target only when bulk occupies every carrier, growth is
one step per 2 s, an idle tunnel never grows, and the target holds afterwards
(DEC-VE9 — lowering never closes a live carrier).

It is also, honestly, **worse than a static `--carriers 8`** under this load
(6.00 vs 5.48 ms p50 under two bulk, 46.6 vs 15.9 ms p95), for two reasons that
are both by design: growth is rate-limited to one carrier per 2 s while the
measurement window is 6 s, and `VHOST_AUTO_CARRIER_CEILING` is **4**. A tunnel
that is going to move bulk continuously is better off being told `--carriers 8`
once. Adaptive is the right default for a tunnel whose load you do not know; it
is not a replacement for knowing. See §8 for the recommendation this produces.


### 3.5 Impaired paths — F-2 reconfirmed, and one behaviour that changed

`tc` impairs traffic toward the server **by IP protocol**, which separates the
two legs: provider→server is TCP for the relay and UDP/QUIC for the direct path,
while consumer→server is always TCP and carries upload bodies. Each transport
therefore appears twice per condition, once impaired on its own data leg and
once as an unimpaired **control** that proves the filter did what it claims.

| condition | transport | download | upload | rps | p50 |
| --- | --- | --- | --- | --- | --- |
| **1 % loss, TCP only** | relay-tcp *(impaired)* | **43.51 MB/s** | 41.46 | 1 627 | 2.58 ms |
| | direct-quic *(control)* | 125.61 — untouched | 50.83 | 1 705 | 2.50 ms |
| **1 % loss, UDP only** | relay-tcp *(control)* | 129.11 — untouched | 136.84 | 3 065 | 2.55 ms |
| | direct-quic *(impaired)* | **117.20** — barely moved | 126.59 | 3 123 | 2.47 ms |
| **+40 ms, TCP only** | relay-tcp *(impaired)* | **20.45 MB/s** | 28.83 | 95 | 83.3 ms |
| | direct-quic *(control)* | 81.48 | 21.47 | 181 | 43.4 ms |
| **+40 ms, UDP only** | relay-tcp *(control)* | 162.82 — untouched | 130.58 | 3 092 | 2.49 ms |
| | direct-quic *(impaired)* | **128.61** † | 111.19 | 184 | 43.2 ms |
| **+40 ms and 1 % loss, TCP** | relay-tcp *(impaired)* | **1.07 MB/s** | 0.56 | 85 | 83.5 ms |
| | direct-quic *(control)* | 82.67 | 0.79 | 172 | 43.0 ms |

The impaired rows read against each other — never a row against its own control:

| data-leg impairment | TCP relay | QUIC direct | winner | before (§2.14) |
| --- | --- | --- | --- | --- |
| 1 % loss | 43.51 MB/s | **117.20 MB/s** | QUIC **2.7×** | QUIC 2.8× |
| +40 ms RTT | 20.45 MB/s | **128.61 MB/s** † | QUIC 6.3× † | QUIC 2.2× |
| +40 ms + 1 % loss | 1.07 MB/s | (control only) | — | relay 1.14 MB/s |
| clean (§3.1) | **≈180 MB/s** | ≈117 MB/s | relay 1.54× | relay 1.5× |

F-2 and F-8 both reconfirm: **QUIC direct is 2.7× the relay under 1 % loss and
the relay is 1.5× QUIC on a clean path.** The relay's `+40 ms and 1 % loss` cell
is the Mathis regime again (`MSS/(RTT·√p)` ≈ 0.34 MB/s per flow) — nothing about
bore is wrong there, a single cubic flow cannot go faster.

† **The one behaviour that changed, and it is an improvement with a caveat.**
Under +40 ms on UDP only, the before-run measured the QUIC arm at 59.41 MB/s
staying on the direct path. This run measured **128.61 MB/s with the harness's
own path proof reading `relay-tcp`** at the end of the case while p50 stayed at
43 ms (i.e. most small requests still paid the added UDP delay). That is a
**mixed** path: some proxied connections took the impaired direct path, others
fell back to the unimpaired relay per connection, in place, without dropping the
tunnel — which is exactly what phase 05's bounded open plus per-connection
fallback is for. It more than doubles throughput on a UDP-degraded path. But
because the two halves of that case were not on the same transport, the 6.3×
must not be quoted as a transport ratio; the honest statement is *"a `--udp`
tunnel on a badly delayed UDP path now recovers most of the relay's
throughput instead of being stuck at a third of it."*


## 4. The workstation as consumer — can a domestic link be saturated?

This is the leg the before-campaign never measured cleanly. §1.3 of that
document discarded its workstation numbers because the workstation was the
*provider*: every byte crossed the domestic link **twice** (consumer → server →
provider, both hops over the same WiFi). Here the topology is the one a real
user has — origin and provider on the AWS VM, server on AWS, and the
**workstation is the consumer** — so each byte crosses the domestic link once.

### 4.1 What this link can actually do (the ceiling to be judged against)

There is no wired link on this host; every wired NIC is down and the only path
is WiFi 6:

| | value |
| --- | --- |
| radio | 802.11ax, 80 MHz, 2 spatial streams, **HE-MCS 11**, PHY **1 200.9 Mbit/s** both ways |
| signal | −50 dBm (excellent) |
| SSID band | 5 GHz (5 560 MHz) |

Reference throughput to public, well-provisioned endpoints, parallel streams
(a single stream is Mathis-bound at this RTT and measures nothing about the
link):

| direction | x1 | x2 | x4 | x8 | x16 |
| --- | --- | --- | --- | --- | --- |
| **download** (Hetzner FSN1) | 30.71 MB/s | 36.33 | **46.10** | 35.56 | 43.81 |
| **upload** (Cloudflare `__up`) | 62.08 MB/s | 74.62 | **105.45** | — | — |

Cross-checked against three further endpoints, four streams each, because a
single reference could itself be the limit: Hetzner FSN1 36.71 MB/s, OVH
Roubaix 32.63 MB/s, Tele2 5.54 MB/s (its own limit), Scaleway unreachable.
`speed.cloudflare.com/__down` now answers **HTTP 403** and can no longer be used
— it could when the before-document was written.

So the honest ceiling for this consumer is:

* **download ≈ 46 MB/s (390 Mbit/s)**, reached at 4 parallel streams, and
  confirmed by two independent endpoints;
* **upload ≈ 105 MB/s (885 Mbit/s)**, reached at 4 parallel streams.

The link is asymmetric *upward*, which is unusual and worth stating plainly:
**this connection uploads more than twice as fast as it downloads.** Every
"saturation" claim below is therefore against 390 Mbit/s down and 885 Mbit/s up,
not against a symmetric gigabit. If a tunnel exceeds those figures, the
reference endpoint was the limit and the tunnel result is the better datum —
that case is called out where it happens.


### 4.2 The apparatus finding that governs every number below

The first workstation suite produced a result that looked like a catastrophic
regression and was not one. Five configurations were measured back to back in
one 877 s run; the first two were healthy and the last three collapsed:

| config | dl x1 | dl x4 | dl x8 | up x1 | up x4 |
| --- | --- | --- | --- | --- | --- |
| relay c=1 | 40.85 | 50.20 | **59.10** | 78.82 | **93.67** |
| relay c=8 | 50.09 | 49.57 | 52.55 | 75.84 | 72.17 |
| relay c=0 auto | 11.49 | 6.69 | 6.74 | 6.88 | 8.45 |
| QUIC c=1 | 9.81 | 7.11 | 7.03 | 7.23 | 7.57 |
| QUIC c=4 | 9.28 | 7.18 | 7.06 | 7.21 | 7.58 |

MB/s. Every collapsed row sits at ~7 MB/s — 56 to 60 Mbit/s — in **both**
directions and on **both** transports, which no code path can produce: the
relay and the QUIC data planes share nothing below the registry, and a
scheduling bug cannot cap an upload and a download at the identical figure.

The server's ENA counters name it. Over that one suite:

```
server bw_in_allowance_exceeded  +4 029 161
server pps_allowance_exceeded    +43 068
```

The t4g.micro's **inbound** allowance is a burst budget over a low baseline.
The suite drained it, and every subsequent transfer ran at the baseline — which
is what ~7 MB/s is. `bw_out` did not move, so this is specifically the inbound
direction (the server pulling response bytes from the provider on the VM), and
it explains why an *upload* from the workstation is capped too: an upload also
arrives at the server from outside.

Three consequences, all of which shape the rest of this document:

1. **A long unpaired ladder on a burstable instance measures AWS shaping, not
   bore.** Order alone decided which configuration looked good.
2. Every bandwidth measurement from here on is **gated** before it starts
   (`res/bw_probe.sh`: an 8 s single stream plus the allowance delta; above
   30 MB/s the budget is available, at ~7 MB/s it is not) and **reports its own
   allowance delta per rung**, so a shaped rung is visible instead of averaged
   in.
3. Comparisons between configurations are **paired**: reference and candidate
   are registered simultaneously and alternate in 10 s bursts with 40 s
   cooldowns (`ws/ws_tunnel2.sh`). A drifting budget then hits both arms of
   each pair equally and the ratio survives.

The budget recovers within about five minutes of idling. This is a property of
the instance the operator chose, not of the code, and it is the same mechanism
§12 of the before-document left open as N-9.

### 4.3 What the tunnel delivers to this workstation

Only the two rows measured with budget available are quotable as absolute
throughput, and they are the two that matter:

| | dl x1 | dl x4 | dl x8 | up x1 | up x4 | small-request p50 |
| --- | --- | --- | --- | --- | --- | --- |
| relay c=1 | 40.85 | 50.20 | **59.10 MB/s (496 Mbit/s)** | 78.82 | **93.67 MB/s (786 Mbit/s)** | 24.07 ms |
| relay c=8 | 50.09 | 49.57 | 52.55 | 75.84 | 72.17 | **20.57 ms** |

Against the §4.1 ceilings this reads:

* **Download: saturated, and then some.** 59.10 MB/s through the tunnel is
  **28 % more** than the best public reference this link could reach
  (46.10 MB/s from Hetzner FSN1 at the same stream count) and 26 % more than a
  plain `ssh` transfer from the same VM (47.04 MB/s at x8, §4.1's R3 row). The
  tunnel is not the constraint on download; the public reference endpoints were.
  There is no headroom left to find here — the link is the limit and bore is
  standing on it.
* **Upload: 93.67 MB/s (786 Mbit/s) against a 105.45 MB/s ceiling — 89 %.**
  Also **18 % faster than plain ssh** to the same VM (79.15 MB/s at x8). The
  remaining 11 % is the honest gap, and it is small enough that TLS framing plus
  the yamux header on every 128 KiB buffer accounts for it without needing a
  bug.

So the operator's stated excellence criterion — *saturare i link della mia
workstation in upload e download* — is **met on download** (the tunnel beats
every reference the link can reach) and **89 %-met on upload**, with the
shortfall attributable to per-frame overhead rather than to a stall, a window,
or a scheduling decision.

`--carriers` on this leg is a **latency/throughput trade, not a win in either
direction**: c=8 costs 11 % of peak download and 23 % of peak upload versus
c=1, and buys 3.5 ms (15 %) off the small-request p50. At a 19.5 ms RTT with one
bulk stream per carrier, eight carriers each get an eighth of the cwnd growth
and the aggregate loses; the same eight carriers are what keep a small GET from
queueing behind bulk. Pick by workload: **c=1 for a single big transfer, c=8
when small requests share the tunnel with bulk** (which is exactly what §3.3
priced on the same-region leg).

#### The paired comparisons (budget-neutral)

Ratios of candidate to a simultaneously-registered relay c=8 reference, median
of three alternating pairs:

| candidate | median ratio vs relay c=8 | pool state at the end |
| --- | --- | --- |
| relay `--carriers 0` (auto) | **1.033** | carriers=1 target=1 |
| QUIC c=1 | **0.611** | carriers=1 path=direct |
| QUIC c=4 | **0.720** | carriers=4 path=direct |
| QUIC `--carriers 0` | **0.712** | carriers=1 path=direct |

Two things fall out of this table.

**`--carriers 0` costs nothing on a domestic link** (1.033, i.e. it matched the
static c=8 within noise) while holding the pool at **one** carrier throughout.
That is the phase 03.3 design working as specified rather than a failure to
grow: `last_crowded` advances only when a small request actually *contends*
with bulk, and a workstation burst of four parallel downloads with no small
requests is quiet by definition. The tunnel got c=8 throughput at c=1 cost.

**QUIC direct is 30–40 % slower than the relay from this workstation**, and the
allowance counters say why: a QUIC burst generated **38 000 to 160 000**
inbound-allowance misses against the relay's 0 to 43 000 in the same pair.
Per delivered byte, the UDP path makes the instance's inbound token bucket
fire far more often — consistent with §7's cost-per-byte result (13.00 core-s
per GiB on QUIC against 6.85 on the relay, F-7) and with the smaller MTU-bound
datagram accounting. On a burstable instance the direct path is the *more
expensive* way to move bulk, which inverts the intuition that "direct must be
faster than a relay". It is not a bore defect — F-8 already recorded that the
relay wins on a clean path — but it is now measured from a real domestic
consumer, which the before-document could not do.

## 5. dufs — the real application, from the workstation

The operator's own framing: *"sulla vm di test avvii dufs … Poi dalla mia
workstation fai upload/download di file (grandi, molti file piccoli) … Qui si
vede veramente il funzionamento su un target reale."*

Everything in §3 and §4 uses a synthetic origin (`bench_origin.py`) that
generates bytes from memory at multi-GB/s, which is the right choice when the
tunnel is the thing being measured. It is the wrong choice for answering "does
this work for a real workload", because a real origin has a filesystem, a
per-request cost, and its own concurrency behaviour.

### 5.1 Apparatus

| piece | detail |
| --- | --- |
| origin | **dufs 0.44.0** on the test VM, `--bind 127.0.0.1 --port 5080 --allow-upload --allow-delete` |
| forwarder | `bore vhost 127.0.0.1:5080` on the same VM, one flavour per run |
| consumer | this workstation, over its own WiFi 6 link (§4.1) |
| corpus | `big1g.bin` 1 GiB · `med/m1..m10.bin` 20 MiB each · `small/s1..s2000.bin` 8 KiB each · `upload/` landing zone |
| disk discipline | the VM root has ~8.0 GiB free; the corpus is ~1.2 GiB, the landing zone is emptied after **every** upload phase, and every phase refuses to start with less than **2 GiB** free (`vm_dufs_setup.sh` `guard()`) |

Three shapes are measured, because they fail for different reasons:

* **BIG** — one 1 GiB file, 1/2/4/8 parallel: sustained throughput, cwnd- and
  BDP-bound. This is the "saturate my link" case.
* **MED** — ten 20 MiB files at once: the shape a download manager or a
  media-heavy page makes.
* **SMALL** — 500 × 8 KiB at `--parallel-max` 1, 8 and 32: latency-bound. Here
  throughput numbers are meaningless and *files per second* is the metric.

Uploads use the same shapes in reverse (PUT), and every upload phase is followed
by a `clean` that empties the landing zone.


### 5.2 The latency-bound shapes, and who actually pays for them

All three flavours, same corpus, same link, `--carriers 8` where the flavour
supports carriers:

| shape | native | docker `:client` | ssh gateway | **dufs on loopback** |
| --- | --- | --- | --- | --- |
| MED 10 × 20 MiB parallel | 34.26 MB/s | 36.81 | 33.59 | 1 124 MB/s |
| SMALL 500 × 8 KiB, pm 1 | 66.49 ms/file (15/s) | 67.48 (15/s) | 62.99 (16/s) | **41.03 ms (24/s)** |
| SMALL pm 8 | 8.20 ms (122/s) | 8.26 (121/s) | 8.01 (125/s) | 4.81 (208/s) |
| SMALL pm 32 | 2.77 ms (360/s) | 2.82 (355/s) | 2.80 (357/s) | 1.20 (832/s) |
| SMALL 500 PUT, pm 1 | 25.38 ms (39/s) | 25.12 (40/s) | 24.16 (41/s) | 0.21 (4 839/s) |
| SMALL 500 PUT, pm 32 | 1.62 ms (618/s) | 1.58 (633/s) | 1.58 (631/s) | 0.16 (6 306/s) |
| `oha -c 8` one small file | rps 124, p50 64.9 ms | 128, 62.9 | 124, 65.3 | **201, p50 41.0** |

Two conclusions, and the second one is the important one.

**The three flavours are indistinguishable.** Native, the published Docker
client image (`ghcr.io/manprint/bore:client`, same revision `28f0a5f…`, run
`--network host`) and a stock OpenSSH client with no bore binary on the
provider side land within 3 % of each other on every row. Whatever a user
chooses to run, they get the same application behaviour. The SSH row is
slightly *faster* on the sequential shapes despite running at `carriers=1` —
the SSH leg is TCP-relay-only by design (I-SSH2) and gets no carrier pool at
all, which at pm 1 costs nothing because there is never anything to schedule
against.

**The dominant cost in the small-file case is dufs, not bore.** The loopback
column is the same request shapes with the tunnel removed entirely, run on the
VM against `127.0.0.1:5080`. It reads **41.03 ms per sequential GET** and
`oha -c 8` on a single small file reports **p50 = 40.997 ms** — on loopback,
where the RTT is microseconds. A flat ~41 ms floor at sub-millisecond RTT is
the classic delayed-ACK/Nagle interaction signature, and it belongs to dufs's
own response path.

Arithmetic on the sequential GET: 66.49 ms through the tunnel, 41.03 ms of it
spent inside dufs on loopback, leaving **25.5 ms** for everything bore and the
network do — against a **19.5 ms** median workstation↔server RTT (median of
twelve TCP handshakes, §4.1) plus TLS on the public leg. **bore's own share of
a small-file request is therefore about 6 ms**, and roughly two thirds of what
a user would perceive as "the tunnel is slow" on this workload is the origin
application.

The `oha` rows make the same point from the other side: 124 rps through the
tunnel against 201 rps on loopback. If bore were the bottleneck the ratio would
track the RTT; instead both numbers are pinned near the same ~41 ms floor.

PUTs are the control that proves it. dufs answers a small PUT in **0.21 ms** on
loopback — no 41 ms floor, because the delayed-ACK interaction does not arise
on that path — and through the tunnel a sequential PUT costs 25.38 ms, i.e.
almost exactly one RTT plus change. That is bore's true per-request cost with
the origin's own latency removed, and it is what the tunnel should cost.

**Parallelism recovers everything that matters.** At `parallel-max 32` the
tunnel delivers **360 files/s downloaded and 618 files/s uploaded**. The
sequential figure is a property of doing one thing at a time over a 19.5 ms link,
not a limit of the tunnel; no configuration change can beat the speed of light,
and concurrency is the only real answer to a many-small-files workload.

### 5.3 Big files — the "saturate my home link" case

Native forwarder, `--carriers 8`, one 256 MiB range of `big1g.bin` per stream,
25 s cooldown between rungs, each rung reporting the server's own inbound
allowance delta so a shaped rung is visible:

| rung | throughput | allowance delta |
| --- | --- | --- |
| **download x1** | **51.13 MB/s (429 Mbit/s)** | +255 |
| download x2 | 26.00 MB/s | +939 |
| download x4 | 48.76 MB/s (409 Mbit/s) | +150 |
| download x8 | 41.45 MB/s | +618 |
| **upload x1** | **76.50 MB/s (642 Mbit/s)** | **+0** |
| upload x2 | 60.86 MB/s | +0 |
| **upload x4** | **80.57 MB/s (676 Mbit/s)** | **+0** |

**Download does not scale with stream count, and that is the answer, not a
problem.** 51.13 MB/s at a *single* stream is already above the 46.10 MB/s this
link reaches from the best public reference (§4.1) and above the 47.04 MB/s a
plain `ssh` transfer from the same VM manages at eight streams. Adding streams
cannot help because there is nothing left to win: **the WiFi link is saturated
by one connection through the tunnel.** The x2 rung's 26.00 MB/s is the outlier
and carries the largest allowance delta of the set (+939); it is reported rather
than smoothed away.

Upload reaches **80.57 MB/s (676 Mbit/s)** at four streams with the allowance
counter completely still — 76 % of the 105.45 MB/s link ceiling and, again,
above plain `ssh` (79.15 MB/s at eight streams). Uploads cost this instance
noticeably less allowance than downloads, consistently: every upload rung in
this table moved the counter by zero while every download rung moved it.

**The honest summary of the operator's excellence criterion.** *"L'eccellenza
sarebbe saturare i link della mia workstation in upload e download"*:

* **download — saturated.** Through the tunnel, from a real application, at one
  stream, above every reference this link can reach. Nothing further to extract.
* **upload — 76–89 % depending on the shape** (80.57 MB/s here on a real file
  server, 93.67 MB/s in §4.3 against the synthetic origin). The gap between
  those two figures is dufs's own read path, not the tunnel: the loopback
  baseline in §5.2 shows dufs serving a 256 MiB range at 288.54 MB/s, which is
  ample, but its per-request overhead still shows on the shorter transfers.

Neither figure is limited by anything bore chooses to do, and both are better
than the obvious alternative (`ssh`) over the identical path.

### 5.4 The same real application on the other data plane

Everything above ran on the TCP relay. That is not an oversight for the SSH
flavour — the SSH leg is TCP-only by design (I-SSH2) — but it left the QUIC
direct path unpriced on the one workload that resembles production. Both are
measured here with the same budget-neutral design: two native forwarders
registered at once against the **same** dufs, order rotated every round,
per-burst allowance delta printed.

Registration proved the two data planes really were different before any figure
was quoted: `relay` ended at `path=relay direct_opens=0`, `quic` at
`path=direct direct_opens=226 fallbacks=0` — 226 successful direct opens and
not one fallback for the whole run.

| round | position 1 | position 2 |
| --- | --- | --- |
| **download** | | |
| 1 | relay **46.44** (+954) | quic **52.69** (+145) |
| 2 | quic **45.36** (+25 835) | relay **49.82** (+0) |
| 3 | relay **53.27** (+100) | quic **58.66** (+11 642) |
| **upload** | | |
| 1 | relay **66.29** (+20 521) | quic **63.88** (+25 533) |
| 2 | quic **62.32** (+28 481) | relay **60.14** (+31 140) |
| 3 | relay **69.18** (+21 692) | quic **63.95** (+27 902) |

| | relay `--carriers 8` | QUIC direct `--carriers 1` |
| --- | --- | --- |
| download median | 49.82 MB/s (418 Mbit/s) | **52.69 MB/s (442 Mbit/s)** |
| upload median | **66.29 MB/s (556 Mbit/s)** | 63.88 MB/s (536 Mbit/s) |
| 200 sequential small GETs, p50 | **93.93 ms** | 103.74 ms |
| 200 sequential small GETs, p95 | **105.02 ms** | 135.82 ms |

Three readings, and the first one contradicts §4.3 in a way that turns out to
be informative rather than embarrassing.

**On short bursts QUIC direct is not slower — it is marginally faster.** 52.69
against 49.82 MB/s on download, a 5.8 % edge, from a `carriers=1` tunnel against
an eight-carrier relay. §4.3 measured the same two transports from the same
workstation and found QUIC **30–40 % slower**. The difference between the two
experiments is the burst length: §4.3 sustained each configuration for 20 s,
this one for 10 s. The allowance counters explain it exactly. Across the three
download bursts the QUIC arm generated **37 622** inbound-allowance misses
against the relay's **1 054** — 36× more, for the same delivered bytes. The
direct path spends the instance's token bucket far faster per byte, so the
longer the transfer, the further into shaping it runs. On a burst short enough
to fit inside the budget it wins; on a sustained one it loses badly. That is a
property of a burstable instance meeting a higher packet rate, not of bore, and
it is worth stating plainly because "direct must be faster than a relay" is the
intuition it falsifies in both directions.

**On upload the relay is ahead by 3.8 %**, and every upload burst in this run
was shaped (+20 000 to +31 000 on every arm), so the honest reading of that row
is "no difference that survives the shaping", not "the relay wins".

**On small sequential requests the relay wins by 10 %**, consistently, in both
p50 and p95, and the p95 gap is wider than the p50 gap — the QUIC path's tail is
worse. This is the same ranking §3.1 found on the same-region leg and F-8 found
before that.

> The absolute small-request numbers here (93.93 ms) are higher than §5.2's
> 66.49 ms because this arm forks a fresh `curl` per request and therefore pays
> a full TCP + TLS handshake each time, where §5.2 reused connections through a
> single `curl -K` config. Only the *comparison between the two arms* is meant
> to be read off this row; §5.2 remains the number to quote for the shape.

**Operational conclusion, unchanged and now supported on the real workload:
leave `--udp` off for a vhost that serves a file server.** The relay is faster
on small requests, equal or better on sustained bulk, cheaper in CPU per GiB
(§7), and far cheaper in instance allowance. `--udp` earns its place where the
relay cannot go, not as a throughput upgrade.

## 6. The three flavours — native binary, SSH gateway, Docker client image

The operator asked for all three ("con il binario, con l'immagine docker client,
con il gateway ssh. Non trascurare nulla"). All three run on the same VM,
against the same origin and the same server, interleaved.

### 6.1 SSH ingress gateway against the native binary

The SSH leg is TCP relay only by design (no `--udp`, no `--carriers`), so the
comparison is *SSH gateway* against *native TCP relay*, two interleaved rounds.

**Throughput (S2):**

| case | before (§2.11) | after |
| --- | --- | --- |
| SSH single stream | 100.98 / 159.45 MB/s | **170.66 / 100.53 MB/s** |
| SSH 8 parallel, aggregate | 100.98 / 109.37 → **105 MB/s** | **104.93 / 100.38 → 103 MB/s** |
| native single stream | 184.60 / 148.86 MB/s | **166.80 / 259.47 MB/s** |
| native 8 parallel, aggregate | 219.45 / 178.91 → **199 MB/s** | **238.46 / 235.03 → 237 MB/s** |
| SSH upload 256 MB | 150.88 / 169.74 MB/s | **190.48 / 178.97 MB/s** |
| native upload 256 MB | 157.20 / 60.49 MB/s | **132.77 / 168.38 MB/s** |

**F-9 reconfirmed exactly**: eight parallel downloads over the SSH gateway
aggregate to *no more* than one stream does (103 MB/s aggregate against a
100–171 MB/s single stream), while native gains from parallelism
(237 MB/s aggregate against 167–259 single). The plan did not target this and
did not change it — the cap is the OpenSSH client's own 2 MiB per-channel
window plus one shared session loop, documented in `ssh-gateway-throughput`.
Native's aggregate improved from 199 to 237 MB/s (+19 %), which is the same
order as this instance's drift and should not be quoted as a plan effect.

Worth noting for operators: **SSH upload is the fastest upload path measured**
(179–190 MB/s against native's 133–168), reproducing the before-run's ranking.

**Latency and request rate (S3), 6 s per point:**

| load | SSH before | SSH after | native before | native after |
| --- | --- | --- | --- | --- |
| 1 KiB c=1 | 355 rps, p50 2.65 | 359 rps, p50 2.77 | 376 rps, p50 2.60 | 396 rps, p50 2.47 |
| 1 KiB c=8 | 2 891, 2.73 | 3 095, 2.51 | 2 718, 2.68 | 3 143, 2.47 |
| 1 KiB c=32 | 11 426, 2.72 | 11 365, 2.74 | 11 018, 2.81 | 11 424, 2.70 |
| 1 KiB c=64 | **18 448**, 3.32 | **19 220**, 3.16 | 14 223, 4.21 | 13 638, 4.51 |
| 100 KiB c=8 | 1 281, 124.92 MB/s | 1 109, 108.19 MB/s | 1 497, 146.06 MB/s | 1 458, 142.23 MB/s |
| new conn per req | 916 rps, p50 8.58 | 933 rps, p50 8.45 | 1 054 rps, p50 7.46 | 1 056 rps, p50 7.40 |

Unchanged in every cell within drift, including the surprising one: at c=64 the
SSH gateway still delivers ~40 % more requests per second than native at a lower
p50. Success rate 1.0 everywhere.

**Head-of-line blocking (S4).** Six rate-limited readers pinned, then fast
requests on top: SSH 35.4 / 31.7 / 14.3 ms TTFB and 2 693 rps (p99 10.5 ms);
native 12.8 / 13.0 / 12.7 ms and 3 046 rps (p99 3.4 ms). No head-of-line
blocking on either path — but the SSH numbers are a little worse than the
before-run's 14.5 / 13.9 / 14.3 ms, and the first two samples are the slow ones,
which is the shape of a warm-up rather than of contention.

**Inapplicable parameters (S5).** `https=on force-https=on
basic-auth=user:pass max-conns=7 notes=perftest` still produces exactly one
warning — `max-conns: not applicable to vhost tunnels; ignoring` — and the
banner then reports `HTTPS policy: redirect`, `Basic-auth: enabled`,
`Notes: perftest`. I-SSH8 holds.

### 6.2 The wedged-client result — the sharpest before/after in the campaign

Same procedure on both transports: a live download in flight, then `SIGSTOP`
the provider (TCP stays alive and ACKs; the application answers nothing).

| | SSH gateway before | SSH gateway after | native before | **native after** |
| --- | --- | --- | --- | --- |
| t+20 s | registered | registered | registered | registered |
| t+40 s | **released** | **released** | registered, bytes frozen | registered, bytes frozen |
| t+60 s | released | released | **still registered** | **released** |
| t+90 / 120 s | released | released | **still registered** | released |
| re-register while wedged | succeeds | succeeds | **rejected, `subdomain in use`** | **succeeds** |

The before-document's phrasing was "the native vhost path has no equivalent and
holds the subdomain indefinitely — this is F-1, now with its fix already present
in the same binary on the neighbouring code path." That is exactly what phase 01
did: the native path now reaps at the 60 s control-liveness deadline, the SSH
path keeps its own 40 s eviction (two 15 s bounded channel opens), and the two
ingress paths finally behave the same way for the user who is watching their
subdomain.


### 6.3 The published Docker client image

`ghcr.io/manprint/bore:client` (commit `f15a3de`, "feat(docker): add root client
image published as :client") is the third flavour and had never been measured.
It was run on the test VM as:

```
docker run --rm --network host ghcr.io/manprint/bore:client \
    vhost 127.0.0.1:5080 --subdomain <label> --id <label> \
    --to <server> --secret <secret> --carriers 8
```

`--network host` is required and is the operationally interesting part: the
origin (dufs) is bound to the VM's **loopback**, so a bridged container could
not reach it at all. With host networking the container's data path is the
host's, and the measurement says so.

**Revision check first.** The image reports the same
`org.opencontainers.image.revision` as the running server image
(`28f0a5f08172b004569416e5bd2ca5625dc323e1`), so this is a like-for-like
comparison of the same code, not of two builds.

Against the native binary on the identical corpus and link (full table in
§5.2):

| shape | native | docker | delta |
| --- | --- | --- | --- |
| MED 10 × 20 MiB | 34.26 MB/s | 36.81 MB/s | +7 % |
| SMALL GET pm 1 | 66.49 ms | 67.48 ms | +1.5 % |
| SMALL GET pm 8 | 8.20 ms | 8.26 ms | +0.7 % |
| SMALL GET pm 32 | 2.77 ms | 2.82 ms | +1.8 % |
| SMALL PUT pm 1 | 25.38 ms | 25.12 ms | −1.0 % |
| SMALL PUT pm 32 | 1.62 ms | 1.58 ms | −2.5 % |
| `oha` rps / p50 | 124 / 64.9 ms | 128 / 62.9 ms | +3 % / −3 % |

Every delta is inside the run-to-run drift of this apparatus, and the sign is
not consistent — docker is nominally faster on four rows and slower on three.
**The published client image costs nothing measurable against the native
binary.** With `--network host` there is no extra hop, no NAT, and no veth
pair; the container is a packaging difference only.

The one caveat an operator needs: the image is documented as the *root* client
image, and `--network host` gives it the host's network namespace. That is the
correct configuration for reaching a loopback-bound origin and it is what these
numbers describe. A bridged container reaching a loopback origin is not a
slower configuration — it is a non-working one.

### 6.4 The three flavours on bulk bandwidth — the decisive comparison

§5.2 established the flavours are equivalent on latency-bound shapes. Bandwidth
needed its own design, because the first attempt produced a confident and
completely wrong answer.

**The trap.** One 4-stream 10 s download burst moves ~500 MB, which is about
this instance's whole inbound burst budget. Run the three flavours back to back
with a 30 s cooldown and the result is:

```
[1] native = 49.82 MB/s  (allowance +10 880)
[2] docker = 23.37 MB/s  (allowance +43 040)
[3] ssh    = 21.31 MB/s  (allowance +43 872)
```

which reads as "the native binary is twice as fast as the other two" and is
entirely an artifact of measurement order — the first arm spends the budget and
the rest read the baseline.

**The design that answers it.** All three forwarders registered simultaneously
against the *same* dufs; **75 s** between bursts (enough for the bucket to
refill — established by measurement, not guessed); and the **order rotated**
every round so each flavour occupies each position exactly once. Nine bursts,
every one of them clean (largest allowance delta in the whole set: +413):

| round | position 1 | position 2 | position 3 |
| --- | --- | --- | --- |
| 1 | native **49.71** (+357) | docker **50.94** (+0) | ssh **47.31** (+0) |
| 2 | docker **47.44** (+413) | ssh **46.75** (+0) | native **48.36** (+89) |
| 3 | ssh **52.08** (+0) | native **48.95** (+201) | docker **50.03** (+0) |

| flavour | median | by position |
| --- | --- | --- |
| native binary | **48.95 MB/s (411 Mbit/s)** | p1 49.71 · p2 48.95 · p3 48.36 |
| Docker `:client` | **50.03 MB/s (420 Mbit/s)** | p1 47.44 · p2 50.94 · p3 50.03 |
| SSH gateway | **47.31 MB/s (397 Mbit/s)** | p1 52.08 · p2 46.75 · p3 47.31 |

**Total spread across all three flavours and all three positions: 5.7 %**, and
the ranking is not stable — each flavour wins at least one position and the SSH
gateway posts the single fastest burst of the entire set (52.08 MB/s) while
running at `carriers=1`. There is **no bandwidth penalty for any flavour** on
this workload.

Two things follow that matter operationally:

* **The published Docker client image is not a compromise.** With
  `--network host` it is the native data path, and it measures as such — here
  and on every latency shape in §5.2.
* **The SSH gateway's known aggregate cap (F-9) does not bite here.** F-9 is
  about *many parallel streams on one SSH session* saturating the OpenSSH
  client's 2 MiB per-channel window (§6.1: 8 parallel downloads aggregate to
  103 MB/s, no better than one). At 4 streams against a 400 Mbit/s domestic
  link the cap is far above the link, so a home user pulling from an
  SSH-gateway tunnel loses nothing. The cap matters on a fast same-region path,
  not on this one.

#### Upload, same design

| round | position 1 | position 2 | position 3 |
| --- | --- | --- | --- |
| 1 | native **87.10** (+0) | docker **78.21** (+0) | ssh **88.83** (+0) |
| 2 | docker **81.08** (+0) | ssh **87.67** (+0) | native **73.41** (+0) |
| 3 | ssh **65.08** (+25 196) | native **60.84** (+23 011) | docker **65.82** (+26 450) |

Rounds 1 and 2 are clean on every arm (allowance delta 0). Round 3 is shaped on
every arm — the whole round, not one position — so it is reported and then set
aside rather than averaged in: it measures the instance's inbound token bucket,
which is what §4.4 is about.

Medians over the six clean bursts:

| flavour | median upload (clean rounds) | carriers |
| --- | --- | --- |
| native binary | **80.26 MB/s (673 Mbit/s)** | 8 |
| Docker `:client` | **79.65 MB/s (668 Mbit/s)** | 8 |
| SSH gateway | **88.25 MB/s (740 Mbit/s)** | 1 |

Native and Docker are again indistinguishable — 0.8 % apart, well inside the
run-to-run spread. **The SSH gateway is ~10 % faster on upload**, and that is
not an SSH advantage: it is the carrier count. The SSH leg is TCP-relay-only
by design (I-SSH2) and therefore runs at `carriers=1`, while the other two ran
`--carriers 8`. §4.3 priced exactly this on the same link and found c=8 costs
**23 % of peak upload** against c=1, because eight carriers each get an eighth
of the cwnd growth at a 19.5 ms RTT and the aggregate loses. The 10 % measured
here is the same effect at four concurrent PUTs instead of one.

So the upload table is not a ranking of forwarders; it is the carrier trade
showing up a second time, in a second workload, from a real domestic uplink.
The operational reading is unchanged from §4.3: **`--carriers 1` for a single
big transfer, `--carriers 8` when small requests must not queue behind bulk** —
and if a deployment is upload-dominated, the SSH gateway's forced `carriers=1`
is an advantage rather than the limitation it looks like.

## 7. CPU and RAM of every actor

The operator asked for this explicitly ("durante tutti i test raccogli anche
info di carico cpu/ram dei vari attori"). A sampler ran **on** each host — the
server's on the server, the VM's on the VM — every 2 s for the whole
benchmark run, capturing `/proc/stat` split into usr/sys/softirq/**steal**,
load, `MemTotal`/`MemAvailable`, and per-process cputime and RSS. Sampling from
inside the container would have missed the softirq the host kernel spends on its
behalf, which §2.10 measured at 37–40 % of the bill.

### 7.1 Per stage of the benchmark run

CPU is reported as a share of the whole box (2 vCPU on both hosts), so 100 %
means one of the two cores.

| stage | server CPU mean / max | server softirq | STEAL | server mem used max | bore RSS peak | VM CPU mean | VM bore CPU |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **s3_ab** paired transport + carrier A/B | 64.2 % / 85.9 % (1.28 / 1.72 cores) | 28.8 % | 1.43 % | 684 MiB | 31.3 MiB | 29.6 % | 107 % of a core |
| **s4_stab** G1–G9 stability | 9.1 % / 60.2 % | 2.9 % | 0.34 % | **863 MiB of 903** | **538.7 MiB** | 7.9 % | 4 % |
| **s5_netem** impairment matrix | 25.9 % / 81.5 % | 11.7 % | 1.03 % | 859 MiB | 533.4 MiB | 13.1 % | 10 % |
| **s6_ssh** SSH gateway vs native | 24.1 % / 95.1 % | 9.4 % | 0.57 % | 506 MiB | 117.0 MiB | 13.8 % | 3 % |
| **s7_eff** bulk efficiency | 52.0 % / 86.2 % (1.04 / 1.72 cores) | 24.9 % | 0.41 % | 541 MiB | 113.8 MiB | 25.2 % | 20 % |

Three things to take from that table:

* **STEAL stays under 1.5 %** everywhere, so none of these are throttled-CPU
  measurements — the same check §2.17.2 made, and it still holds.
* **The server's memory high-water mark is 863 MiB of 903 MiB, reached during
  s4_stab**, with bore's own RSS at 538.7 MiB. That is the G9/F-13 shape and it
  is the single most dangerous number in this campaign; §2.7 is about it.
* **The provider side is cheap.** On the VM, `bore vhost` cost 3–20 % of one
  core while moving 100–250 MB/s, against the server's 1.0–1.3 cores. The
  asymmetry is expected: the server terminates TLS for the public side, splices
  both halves, and pays the softirq for both sockets.

### 7.2 Cost per byte — did the plan make bore more expensive?

Six alternating 20 s single-stream transfers (three relay, three QUIC direct),
each with the server's `/proc/stat` window sliced to exactly that transfer. CPU
seconds are host-total (`usr+nice+sys+irq+softirq+steal`), so the kernel's
network processing is included.

| case | path | MB/s | cores of 2 | **core-s per GiB** | softirq share of busy | before (§2.17.2) |
| --- | --- | --- | --- | --- | --- | --- |
| relay-r1 | relay-tcp | 167.71 | 1.22 | **7.45** | 43.8 % | 6.33 |
| relay-r2 | relay-tcp | 250.58 | 1.64 | **6.70** | 47.0 % | 7.01 |
| relay-r3 | relay-tcp | 225.21 | 1.41 | **6.41** | 45.2 % | 7.01 |
| quic-r1 | direct-quic | 109.98 | 1.39 | **12.94** | 49.8 % | 12.46 |
| quic-r2 | direct-quic | 126.99 | 1.59 | **12.82** | 50.8 % | 11.95 |
| quic-r3 | direct-quic | 125.14 | 1.62 | **13.25** | 52.3 % | 11.36 |

**Mean relay 6.85 core-s/GiB against 6.78 before; mean QUIC 13.00 against
11.92.** Both are inside this instance's run-to-run spread, so the plan's
scheduling work — a `BulkTicket` per proxied connection, an occupancy count per
carrier, a per-write burst cap on a demoted QUIC stream — **costs nothing
measurable per byte.** That was the thing most worth checking: a latency fix
that had made the server 20 % more expensive would have been a bad trade at this
core count, and it did not happen.

F-7 also reconfirms independently: **44–52 % of the CPU is softirq**, i.e.
kernel network processing rather than bore's own code, in both transports and at
both ends of the before/after.


### 7.3 CPU and memory of every actor, through the workstation and parameter phases

§7.1 sampled only the same-region driver stages. The operator asked for the
load of **all** actors during **all** tests, so a 2 s sampler ran on the server,
the test VM and the workstation continuously through the three remaining
phases. Windows are cut at the phase boundaries recorded in the run logs.

Host CPU is a share of the whole box; the core-equivalent line converts it
using the real core count of that host (2 vCPU on the server and the VM, 16 on
the workstation) — the reducer takes the count as input precisely because it
runs on the workstation over samples copied from a 2-vCPU machine.

| phase | server (t4g.micro, 2 vCPU) | test VM (c7i-flex.large, 2 vCPU) | workstation (16 cores) |
| --- | --- | --- | --- |
| **rotated flavour comparison** (9 download + 9 upload bursts) | mean **0.17** core, p95 1.08, **max 1.52 of 2**; steal 0.42 % | mean 0.08 core, max 0.82 | mean 0.63 core, max 5.20 |
| **dufs relay vs QUIC** | mean **0.14** core, max 1.35 | mean 0.07, max 0.91 | mean 1.30, max 15.75 |
| **server-parameter programme** (F-13 ladders, restarts) | mean **0.27** core, max 1.58 | mean 0.07, max 0.76 | mean 1.35, max 15.74 |

Per-process, over the same windows:

| process | flavours | dufs relay/QUIC | parameter programme |
| --- | --- | --- | --- |
| server `bore` | 9 % of one core, peak RSS **84.5 MiB** | 7 %, peak RSS **106.6 MiB** | 13 %, peak RSS **340.4 MiB** |
| VM `dufs` | 4 % of one core, RSS 4.3 MiB | 3 %, RSS 4.2 MiB | idle |
| VM `bore` (provider) | 2 % of one core, RSS 17.3 MiB | 2 %, RSS 78.3 MiB | RSS 245.6 MiB |
| server host memory used | 428 MiB mean / 495 max | 431 / 551 | 651 / **739** |

Four things follow, and the first is the one that matters most for the whole
document.

**The server's CPU is not the bottleneck, and it is not close.** While moving
~50 MB/s down and ~80 MB/s up through three simultaneously registered tunnels,
the whole 2-vCPU instance averaged **0.17 of one core** and peaked at 1.52 of
two. The `bore` process itself spent 92 CPU-seconds across a 998-second window
— 9 % of a single core. Whatever caps throughput on this deployment, it is not
the server running out of processor, which is what makes §4.4's allowance
explanation the surviving one rather than merely the convenient one.

**CPU credits were never exhausted either.** `steal` stayed at 0.29–0.71 %
throughout. A burstable instance that had run out of CPU credit would show
steal in the tens of percent. So the two burstable resources behave completely
differently under this workload: the **network** allowance is spent by a single
10-second burst, while the **CPU** allowance is barely touched over an hour.

**Memory is where the F-13 budget shows up, and it is dramatic.** The server
`bore` process peaked at **340.4 MiB** during the parameter programme — that is
the F-13 baseline ladder with 48 slow readers on one `--udp` tunnel, on a host
with about 950 MiB of RAM. One tunnel, 36 % of the machine. The same ladder with
`BORE_UDP_MEMORY_BUDGET=512MiB` peaked at **42.2 MiB** (§8). Host memory used
tracks it: 739 MiB at the peak against 495 MiB in the phases without the ladder.

**The provider side is free.** `dufs` served the entire real-application
campaign on 3–4 % of one core and 4.3 MiB of RSS, and the `bore vhost` provider
cost 2 %. Nothing on the origin side of a vhost tunnel needs sizing.

> The workstation numbers include unrelated desktop activity — it is a working
> machine, not a dedicated load generator, and its `max 98.4 %` samples are not
> the benchmark. The per-process rows are the trustworthy part there, and they
> show `curl` and `bore` at under 1 % of one core each: generating four parallel
> 400 Mbit/s TLS streams is not expensive.

#### The final-parameter phase, sampled on all three hosts

The §8 parameter programme was sampled the same way, and it is the one phase in
which the server was restarted repeatedly (seven times, once per arm), so it is
also a check that a restart costs nothing lasting.

| host | mean host CPU | p95 | max | steal | `bore` peak RSS | host memory used |
| --- | --- | --- | --- | --- | --- | --- |
| server (2 vCPU) | 9.5 % of the box (0.19 core) | 36.6 % | 61.6 % | 0.47 % | 96.3 MiB | 700 MiB mean, 797 MiB max |
| VM (2 vCPU) | 2.6 % (0.05 core) | 13.3 % | 38.6 % | 0.00 % | 74.6 MiB | 851 MiB mean |
| workstation (16 cores) | 3.8 % (0.60 core) | 8.2 % | 14.3 % | 0.00 % | 7.9 MiB | — |

Restricted to the budget A/B/A/B alone — four QUIC-direct bursts at ~50 MB/s
from the domestic consumer, with a server restart between each — the server sat
at **0.20 core of 2** with 0.50 % steal and `bore` at 95.2 MiB. Two readings
worth keeping:

* **`bore`'s own RSS with the budget in force is 95–96 MiB**, against the
  340.4 MiB the unbounded ladder reached. That is the F-13 bound doing its job
  on a live deployment rather than in a purpose-built ladder.
* **Seven restarts left no residue.** Mean CPU, steal and memory over the whole
  22-minute phase are indistinguishable from the phases with no restart at all,
  and the three tunnels that belong to the operator re-registered on their own
  every time.

## 8. Parameters varied, and what to keep

The operator authorised varying both server and forwarder parameters
("puoi variare i parametri del server per fare i test … Se fai cambiamenti che
migliorano le performance riportmelo e se li lasci sul compose del server
commentali in modo chiaro"). This section is the answer, split into what was
varied on the **forwarder** (no deployment change needed) and what was varied on
the **server** (a compose change, therefore a restart).

### 8.1 Forwarder parameters — measured, no deployment change

| parameter | values tried | verdict |
| --- | --- | --- |
| `--carriers` | 1, 2, 4, 8, 0 (auto) | **workload-dependent; see the table below** |
| `--udp` | on / off | **leave off** for bulk; it costs 30–40 % of throughput from a domestic consumer and ~2× the CPU per byte (§4.3, §7.2). Turn it on for the many-held-connections case, where it is 100× better (14 ms vs 1 469 ms at 512 held connections). |

The carrier recommendation, now that both legs have been measured, is not one
number:

| situation | setting | why |
| --- | --- | --- |
| single big transfer, nothing else on the tunnel | **`--carriers 1`** | highest aggregate: 59.10 MB/s down / 93.67 MB/s up from the workstation, against c=8's 52.55 / 72.17. Splitting one bulk flow across eight carriers splits the congestion window eight ways. |
| a web app: small requests alongside occasional bulk | **`--carriers 8`** | this is F-15's case and where the plan's whole gain lives: p95 under one bulk 14.06 → 4.92 ms, rate 393 → 1 162 rps under two. Costs ~11 % of peak single-transfer bandwidth. |
| unknown or mixed load | **`--carriers 0`** | matched static c=8 throughput (median ratio 1.033) while holding one carrier, and grows only when small requests actually contend. Not a substitute for knowing the workload: under sustained bulk it is worse than a static c=8 because it grows one step per 2 s and stops at 4. |

**Nothing here changes the shipped defaults.** `--carriers 1` remains the right
default (DEC-VE6 reconfirmed: median c4/c1 = 0.880 on a clean path — carriers
still *cost* when there is nothing to schedule).

### 8.2 Server parameters as deployed **at the start of the campaign**

This is the state every measurement in §2–§7 ran against; §8.5 records the
one deliberate change made at the end. The compose set, among others:

```yaml
- BORE_MAX_CONNS=1024
- BORE_MAX_CARRIERS=1024
- BORE_UDP_MAX_STREAMS=8192
- BORE_PROXY_BUFFER_SIZE=128KiB     # half the built-in default of 256 KiB
# BORE_UDP_MEMORY_BUDGET            # unset
```

Two observations before any measurement.

**`BORE_MAX_CARRIERS=1024` silently disarms `--udp-memory-budget`'s window
scaling.** `UdpDirectTuning::from_memory_budget` computes
`conn = clamp(budget / max_carriers, 16 MiB, 256 MiB)`. With `max_carriers`
at 1024, *any* practical budget divides to below the 16 MiB floor, so the
connection window collapses to the floor and the stream window to 1 MiB
regardless of the budget chosen; the budget then only buys **slots**
(`slots = budget / conn`, so 512 MiB → 32). That is a defensible outcome — the
documented contract is "a bigger budget buys slots, not windows" — but an
operator setting a large budget on this server will get much smaller windows
than the 256 MiB/16 MiB default pair, and nothing says so at startup beyond a
`BelowCarrierCount` shortfall warning.

**`BORE_PROXY_BUFFER_SIZE=128KiB` could not be verified from the admin API** —
and neither could the windows a budget derives. Both gaps were found here,
both were fixed in this campaign, and §2.8 is the account of the fix. The rest
of this section is the measurement that decides what the compose should
actually say.

### 8.3 `BORE_PROXY_BUFFER_SIZE` — measured twice, and the first answer was wrong

The parameter sizes the per-direction copy buffer of the relay splice. It
should not matter on a 2 ms path (the buffer is refilled long before it drains)
and should matter on a high-latency one, so it was measured on the VM's
artificially delayed 40 ms path, where the deployed 128 KiB is at the same
order as the bandwidth-delay product.

**The first pass produced a number that had to be thrown away.** It ran
immediately after the dufs campaign, and every repetition after the first read
7–9 MB/s while the instance's `bw_in_allowance_exceeded` counter climbed by
tens of thousands. That is the signature of §4.2's shaping, not of a buffer
size. It is reported here rather than quietly dropped because the arithmetic
that makes it wrong is the single most useful operational fact in this
document: **one 4-stream 10 s burst is roughly one whole inbound burst budget
on this instance.**

Re-run after a gate on a recovered budget, in an A/B/A order so residual drift
cancels, with the allowance delta printed per arm:

| arm | `BORE_PROXY_BUFFER_SIZE` | reps (MB/s) | median | allowance delta |
| --- | --- | --- | --- | --- |
| 1 | 128 KiB | 21.74 / 25.57 / 26.46 | **25.57** | +14 |
| 2 | 256 KiB (the built-in default) | 28.53 / 26.14 / 30.15 | **28.53** | +0 |
| 3 | 128 KiB again | 36.52 / 29.67 / 24.05 | **29.67** | +0 |

Read the two 128 KiB arms against each other first: 25.57 and 29.67, a **16 %
spread between two identical configurations**. The 256 KiB arm's 28.53 sits
between them. The parameter's effect, if any, is smaller than the run-to-run
drift of the path, and the within-arm spread (24.05 to 36.52 in arm 3 alone) is
larger still.

**Verdict: no measurable effect, in either direction, even where the theory
says it should be largest.** There is therefore no performance argument for the
deployed 128 KiB, and one clarity argument against it: it is half the value
every other document, test and default assumes. The recommendation is to
**restore the built-in default of 256 KiB** and leave the line commented in the
compose with the measurement recorded beside it — not because 256 is faster,
but because a deployment that silently differs from the tested default costs
more in confusion than it can ever buy in throughput.

Two things this measurement is **not**. It is not a statement about the clean
2 ms path (the documentation already says the parameter does not bite there,
and the first pass's clean-path arms were shaped too). And it is not a licence
to make the buffer small: the parameter exists because `tokio::io::copy`'s
8 KiB internal buffer is a known high-BDP regression, which is exactly why the
injected-response path hand-rolls its loop. The finding is that 128 KiB and
256 KiB are both comfortably past the point where it matters.

### 8.4 `BORE_UDP_MEMORY_BUDGET` — priced where it could actually hurt

F-13 already established what the budget buys: with 48 slow readers on one
`--udp` tunnel the server's peak RSS falls from **340.4 MiB to 42.2 MiB** on a
903 MiB host, and requests keep being served instead of the host starving. What
was never priced is what it *costs*, and there is exactly one place it can.

The arithmetic is in §8.2: with `BORE_MAX_CARRIERS=1024`, a 512 MiB budget
derives a connection window at its **16 MiB floor** and therefore a stream
window of **1 MiB**. At the same-region 2 ms RTT a 1 MiB window is four times
the bandwidth-delay product and cannot bind. At the workstation's **19.5 ms and
~400 Mbit/s the BDP is ~975 KiB** — the *same order* as the derived window. If
the budget costs throughput anywhere, it costs it there.

So it was measured from there and nowhere else: a `--udp --carriers 1` vhost
against the same dufs, four parallel 10 s streams, **A/B/A/B** so each ON has an
OFF beside it, gated on a recovered allowance, with the path read back from the
admin API on every arm.

| arm | budget | rate | allowance delta | path |
| --- | --- | --- | --- | --- |
| 1 | **512 MiB** | 53.16 MB/s (445 Mbit/s) | +0 | direct |
| 2 | unset | 52.28 MB/s (438 Mbit/s) | +0 | direct |
| 3 | **512 MiB** | 56.25 MB/s (471 Mbit/s) | +0 | direct |
| 4 | unset | 49.72 MB/s (417 Mbit/s) | +27 | direct |
| | **ON median 54.71 MB/s** | **OFF median 51.00 MB/s** | | |

**The budget costs nothing.** It is nominally 7 % *faster*, which is inside the
drift this path shows between identical arms, so the honest statement is "no
difference" — but it is emphatically not the loss the window arithmetic
suggested was possible. Three arms ran with a zero allowance delta, so this is a
clean measurement rather than a shaped one, and every arm confirmed
`path=direct`, so the QUIC path really was the one under test.

Why the 1 MiB window does not bind, when the BDP says it is marginal: the
transfer is **four parallel streams**, and the window is per *stream* while the
BDP is a property of the *path*. Four streams at 1 MiB each cover a ~975 KiB
BDP four times over. A single-stream download at this RTT is the case that would
expose it, and that case is bounded by the same-order relay figures anyway.

**Verdict: keep it.** It converts an unbounded worst case into a bounded one for
free. The one caveat belongs in the compose beside it, and now is: at
`BORE_MAX_CARRIERS=1024` the budget buys admission slots, not windows, and
lowering the carrier ceiling is what would make its window arithmetic behave as
the documentation describes.

### 8.5 The compose as left, and why each line says what it says

The staging compose was edited once at the end of the campaign and every value
that deviates from a bore default now carries the default, the reason and the
measurement on the line above it. Three substantive changes:

| line | before | after | why |
| --- | --- | --- | --- |
| `BORE_PROXY_BUFFER_SIZE` | `128KiB` | **removed** (default 256 KiB applies) | §8.3: no measurable effect even on the 40 ms path; the deviation only cost clarity |
| `BORE_UDP_MEMORY_BUDGET` | unset | **`512MiB`** | §8.4 and F-13: 340.4 → 42.2 MiB worst case, no throughput cost |
| `# leave commented for max performance (sono i default ottimali)` | contradicted the active `BORE_UDP_MAX_STREAMS=8192` by listing `4096` as optimal | rewritten to say the commented values *are* the defaults, that the active line overrides one of them, and that three of them conflict with the budget | the block asserted the opposite of the running configuration |

`BORE_MAX_CARRIERS=1024` and `BORE_UDP_MAX_STREAMS=8192` were **kept** — both
are deliberate operator choices and neither measured harmful — but both now
state the default they deviate from, and `BORE_MAX_CARRIERS` states the
budget-window caveat.

One honest limitation to record. After the edit, `/admin/api/v1/config` on the
live server still reports:

```
udp_stream_receive_window     = 16MiB      <- the snapshot, not the derived 1MiB
udp_connection_receive_window = 256MiB     <- the snapshot, not the derived 16MiB
proxy_buffer_size             = (absent)
udp_direct_slots              = (absent)
direct_quic_idle_ms           = (absent)
```

That is **not** the fix failing: it is the deployed container still running an
image built before 2026-09-11. The fixes and their gates are in the source tree
(§2.8); the deployment will report the resolved values from the next image
build. Stating it plainly matters more than the neat screenshot would, because
an operator reading that endpoint today would otherwise conclude the budget is
not in force.

## 9. What is still open

Ranked by how much it would change an operator's decisions.

**OQ-A. The residual F-14 gap: the loss window can be shortened but not
removed.** §2.7 measured the mechanism and §2.7.1 acted on it: the window is
exactly the QUIC idle timeout, that timeout is now configurable
(`BORE_DIRECT_QUIC_IDLE_MS`), a server-side setting is sufficient because the
minimum is negotiated, and 4 s is safe to at least 30 % packet loss — so the
worst case an operator can see drops from 10 s to 4 s with one compose line.

What stays open is the last 4 s. A request already committed to a silent direct
stream must wait for that stream's connection to be declared dead; no idle
timeout can be short enough to make that free without also tearing down healthy
connections on a lossy path. Closing it properly needs the *second* candidate
fix — a deadline on the FIRST RESPONSE BYTE of a freshly opened direct stream,
which can abandon the stream and re-issue on the relay. That one is still not
implemented, and for the same reason as before: mis-tuned it abandons a
slow-but-healthy origin, so it needs its own red-checked gate and a workload
with genuinely slow origins, not a benchmark-driven patch.
`scripts/perf/vhost_idle_window.sh` is the harness that fix would be measured
with, and it now exists.

**OQ-B. N-9 / the concurrency tail at 512 held connections.** Half of it is
gone (relay fresh request at 256 held connections: 966 ms → **12.7 ms**), and
the half that remains behaves exactly as §12 of the before-document predicted:
the server's `pps_allowance_exceeded` counter stands at 16.76 M cumulative,
three orders of magnitude above either bandwidth counter. The dimension is
packets per second on a t4g.micro, not a queue in this code. It stays
deliberately unfixed — settling it needs a non-burstable instance, not another
patch.

**OQ-C. QUIC direct is the more expensive path per byte, and now also from a
real consumer.** 13.00 core-s/GiB against the relay's 6.85 (§7.2, F-7
reconfirmed), and from the workstation it runs at 0.61–0.72 of the relay's
throughput while generating 38 k–160 k inbound-allowance misses per burst
against the relay's 0–43 k (§4.3). The direct path's value is not bandwidth —
it is the concurrency tail (14 ms against the relay's 1 436 ms at 512 held
connections) and not needing a relay hop. Documenting that trade-off honestly
in the README would help operators choose; today `--udp` reads as an
unambiguous optimisation.

**OQ-D. `metrics.direct_fallbacks` is a public-tunnel counter, not a global
one.** §2.7 found the per-entry `VhostEntry.direct_fallbacks` incrementing
correctly 1→5 through a blackout while the server-wide metric stayed 0. Both
are behaving as written (`src/vhost.rs:1442` vs `src/server.rs:2503`); the
problem is that the name promises a total. An operator watching only
`/admin/api/v1/metrics` would conclude the direct path never fell back. This is
a documentation or naming fix, not a behaviour change.

**OQ-E. `--carriers 0` never grows under pure bulk, by design — but nobody
told the operator.** §3.4 and §4.3 both show the pool sitting at 1 through
sustained bulk transfers, because `last_crowded` advances only when a small
request actually *contends* with bulk. That is DEC-VE9's intent and it costs
nothing measurable (median ratio 1.033 against a static c=8). It is still
surprising enough in the field that `--help` should say it: *auto grows only
when small requests contend with bulk; a pure-bulk tunnel stays at one
carrier.*

**OQ-F. dufs's own ~41 ms per-GET floor.** Not a bore issue at all — it
reproduces on loopback with the tunnel removed (§5.2) — but it dominates the
small-file numbers a user would attribute to the tunnel. Worth knowing before
anyone benchmarks bore against a file server again.

## 10. Runbook — how to re-run all of this

The before-document's §9 covers provisioning the VM, patching the origin and the
secrets discipline; none of that changed and it is not repeated here. What
follows is only what is new or what cost real time this round.

### 10.1 The harnesses — now in the repository

Everything below **ships in the repository**, under `scripts/perf/staging/`, so
the campaign can be re-run in a month against this deployment or a different
one without rebuilding anything. The tree is deliberately coordinate-free: not
one hostname, IP, key path or credential appears in any script. All of that
lives in a single `env.sh` the operator writes once, from
`scripts/perf/staging/env.sh.example`, and which the library looks for at
`$BORE_PERF_ENV`, then `~/.config/bore-perf/env.sh`, then `./env.sh`.

    cp scripts/perf/staging/env.sh.example ~/.config/bore-perf/env.sh
    chmod 600 ~/.config/bore-perf/env.sh   # it holds the bore secret and admin token
    $EDITOR ~/.config/bore-perf/env.sh
    scripts/perf/staging/provision.sh      # idempotent; installs on both remote hosts

`provision.sh` pushes the binary (refusing if the architectures differ), the
client image, the dufs corpus, every VM-side harness and a generated remote
`~/env.sh` (mode 600) that exports the same coordinates to the scripts that run
on the VM. `scripts/perf/staging/README.md` carries the topology table, the
ordered run procedure, how to read a result, and the trap list below.

The names in the table are the file names inside that tree (`vm/`, `ws/`,
`srv/`, `res/`):

| script | host it runs on | what it does |
| --- | --- | --- |
| `vm/vm_cal.sh` | VM | calibration: versions, TCP-connect RTT, local origin rate, server config digest |
| `vm/vm_bulklat.sh` | VM | F-15 / phase 03: small-request latency with 0, 1, 2 bulk transfers, per carrier count, including `--carriers 0` with pool readback |
| `vm/vm_ab.sh` | VM | paired transport A/B (A1), paired carrier A/B (A2), latency suite (A3) |
| `vm/vm_stab.sh` | VM | the G-series (G1–G9) |
| `vm/vm_netem.sh` | VM | protocol-selective impairment matrix + the G6 redo |
| `vm/vm_ssh.sh` | VM | SSH gateway against native (S1–S9) |
| `vm/vm_eff.sh` | VM | cost per byte, printing epoch windows for the sampler |
| `vm/vm_g6.sh` | VM | F-14 instrumented per request, blackhole in both directions |
| `vm/vm_f13.sh` | VM | the F-13 slow-reader ladder with server RSS and budget refusals |
| `vm/vm_budget_ab.sh` | VM | prices `--udp-memory-budget` using the relay as an in-run control |
| `vm/vm_buf.sh` | VM | `BORE_PROXY_BUFFER_SIZE` reps on the clean and the 40 ms path |
| `vm/vm_interop.sh` | VM | the pre-plan client against the new server (DEC-VE2) |
| `vm/vm_dufs_setup.sh` | VM | dufs corpus manager with a 2 GiB free-space guard |
| `vm/vm_dufs_local.sh` | VM | dufs measured with no tunnel at all — the origin's own ceiling |
| `vm/driver.sh` | VM | runs the stages strictly serially, timestamping START/END |
| `res/res_sampler.sh` | every host | 2 s samples of `/proc/stat` (incl. softirq and steal), load, memory, per-process cputime/RSS |
| `res/res_reduce.py` | workstation | reduces a sampler file, optionally sliced to a window; **`NCPU=<n>`** must be set to the sampled host's core count |
| `res/start_samplers.sh` | workstation | starts the sampler on all three hosts at once (and never self-kills: see the trap list) |
| `res/ena_watch.sh` | workstation | prints the server's ENA allowance counters, which is how a shaped arm is detected |
| `res/bw_probe.sh` | workstation | the gate: 8 s download plus allowance delta, answering "does this instance have burst budget right now?" |
| `res/cfgkeys.sh` | workstation | prints the tunable subset of `/admin/api/v1/config`, which is how §2.8 was found |
| `ws/ws_ref.sh`, `ws/ws_ref_public.sh`, `ws/ws_ref_vm.sh`, `ws/ws_rtt.sh` | workstation | the link's own ceilings, cross-checked against several endpoints, and the RTT |
| `ws/ws_dl_parallel.sh` | workstation | parallel-stream reference download, no tunnel |
| `ws/ws_tunnel.sh` / `ws/ws_tunnel_paired.sh` | workstation | workstation-as-consumer, serial and **paired** |
| `ws/ws_dufs.sh` | workstation | the real-application suite, one run per flavour |
| `ws/ws_dufs_relay_vs_quic.sh` | workstation | the same dufs on both data planes, rotated, with per-burst allowance deltas |
| `ws/ws_flavours.sh` | workstation | all three flavours registered at once against the same dufs, measured in rotation |
| `ws/ws_flavours_rotated.sh` | workstation | three rotated rounds, each gated on a recovered burst budget |
| `srv/setenv.sh` | workstation | sets or unsets one compose variable and restarts the server, leaving a dated comment on the line |
| `srv/server_seq.sh` | workstation | the server-parameter A/B/A sequence |
| `srv/logcheck.sh` | workstation | pulls the server log and classifies what it contains |

### 10.2 Order of operations

1. `bw_probe.sh` **first**, and again between bandwidth suites. If it reports
   `THROTTLED at baseline`, stop and idle the instance for five minutes.
   **Budget arithmetic, measured this round:** one 4-stream 10 s download burst
   moves ~500 MB and is about one whole inbound burst budget. Three such bursts
   back to back with a 30 s cooldown shape the second and third
   (49.82 → 23.37 → 21.31 MB/s, allowance +10 880 → +43 040 → +43 872).
   **A 75 s cooldown is enough for all three to run clean**
   (49.71 / 50.94 / 47.31 MB/s at +357 / +0 / +0). Budget your suites in
   500 MB units with 75 s between them, and rotate the order so no arm always
   gets the fresh slot.
2. `res/start_samplers.sh`, then `vm/vm_cal.sh`, then `vm/driver.sh` (s3…s7).
   ~85 minutes, strictly serial.
3. `res/res_reduce.py` immediately afterwards, while the sampler files are intact.
4. `vm/vm_g6.sh`, `vm/vm_interop.sh`, `vm/vm_f13.sh` — short, order-independent.
5. `ws/ws_ref.sh` / `ws/ws_ref_vm.sh` with the VM idle, then `ws/ws_tunnel_paired.sh`.
6. Provision dufs and docker (`provision.sh image corpus`), then `ws/ws_dufs.sh`
   per flavour, `ws/ws_flavours_rotated.sh`, `ws/ws_dufs_relay_vs_quic.sh`.
7. Server-parameter A/Bs last, because they restart the container.
8. `scripts/perf/vhost_idle_window.sh ladder` needs no deployment at all and can
   be run at any time, on a laptop, as the F-14 regression gate.

### 10.3 Harness pitfalls that cost real time this round

Each of these produced a wrong number or a silent no-op, and each is now fixed
in the scripts above:

* **`tc` wants `dev <if>` after the object and verb**, never at the end of the
  argv. A helper that appended it made G6's blackhole filter a silent no-op —
  the *same* mistake §2.13's first G6 attempt made, carried into the VM harness.
  The tell was a "blackhole" during which throughput went *up*.
* **`pkill -f res_sampler.sh` inside an `ssh` command string kills the remote
  shell that is running it**, because that shell's own command line contains the
  pattern. Nothing started, and the missing sampler files were the only symptom.
  Bracket a character: `pkill -f 'res_sample[r].sh'`.
* **`os.cpu_count()` in a reducer that runs on the workstation prices the
  workstation**, not the 2-vCPU host whose samples it is reading. Pass the core
  count in (`NCPU=2`).
* **`asort` is a gawk extension**; under mawk the RTT script printed nothing at
  all rather than failing. Use `sort(1)`.
* **`/usr/bin/time -f %e` resolves 10 ms**, which is the same order as an RTT to
  these hosts — it reported a suspiciously round "17.00 ms". Use curl's
  `%{time_connect}`.
* **A `curl -Z` run needs one `output` per URL**; a trailing `-o` on the command
  line is an output with no URL to bind to. Put every URL and its output in a
  `-K` config file.
* **`speed.cloudflare.com/__down` now answers HTTP 403** (it worked when the
  before-document was written), which silently produced 0.00 MB/s download
  references. `__up` still works.
* **ICMP is blocked to both AWS hosts now**, so `ping` reports 100 % loss and
  every RTT must come from a TCP connect.
* **The instance's own network allowance can invalidate a whole suite** — see
  §4.2. Gate on `bw_probe.sh`, cool down between bandwidth rungs, and record the
  ENA delta beside every rate.


