# Ethernet re-run — evidence (2026-09-12 →)

Re-measurement of every workstation-side stage over a wired link, after the
discovery that the WiFi the campaigns ran on was itself the bottleneck.

Driver: `scripts/perf/staging/rerun_eth.sh`. Results: `out/eth/`. The WiFi
evidence the published documents cite is untouched, so the two read side by side.

---

## 1. The reference, and the proof that it is the right one

Wired egress `eno0`, 1000 Mbit/s full duplex, RTT ~19 ms to the test VM
(`c7i-flex.large`).

| | download | upload |
|---|---|---|
| P=1 | 926 / 865 / 926 | 735 / 735 / 735 |
| P=8 | 935 / 934 / 935 | 741 / 741 / 740 |
| **median** | **930** | **737** |

P=1 ≈ P=8 in both directions: no per-flow limit (V-9's one-line diagnosis — a
window/loss/Mathis limit opens with parallelism, a link rate does not).

### Whose limit is the upload ceiling? (`asym_qualify.sh`)

A reference that belongs to the far end would make every "percent of bare" in
every campaign a statement about the VM. The discriminator is **destination**: a
limit that follows the source reads the same toward every destination.

| endpoint | down P=8 | up P=1 | up P=8 |
|---|---|---|---|
| test VM (AWS eu-south-1) | 933 | 726 | **740** |
| ping.online.net (FR, unrelated provider) | 917 | 583 | **730** |
| Cloudflare (HTTPS POST, different protocol) | — | 210 | **779** |

The ceiling follows the **source**. Three destinations, three networks, two
protocols, one answer: ~730–780 Mbit/s up. It is this end's uplink.

The instance is independently exonerated: its ENA allowance counters were read
as a **delta bracketing every VM arm** — before and after, never once at the end,
because a counter that is nonzero at the end cannot say which arm spent it — and
read `bw_in_allowance_exceeded=0 bw_out_allowance_exceeded=0
pps_allowance_exceeded=0` every time.

**The line is 930 down / 737 up — asymmetric DOWNWARD by ~21 %, which is
ordinary for a domestic connection.**

### Two instrument faults found while qualifying it

1. **Cloudflare's `__down` endpoint returns nothing** in this harness (reads 0
   in every repetition). Download's second opinion therefore rests on
   `ping.online.net`'s 917, which agrees with the VM's 933. Stated rather than
   hidden.
2. **Aggregates computed by summing per-stream rates over non-identical
   intervals over-count.** `link_baseline.sh` reported `cachefly x8 =
   1089 Mbit/s` on a 1000 Mbit/s NIC, which is impossible. The qualification
   stage therefore reads Cloudflare through the **kernel's own interface
   counters** instead. This matters beyond cosmetics — see §2.

---

## 2. The asymmetry claim was wrong in three campaigns, and the attribution was right

Three campaigns independently concluded this link is "asymmetric upward". Each
correctly ruled out the instance's allowance bucket and attributed the asymmetry
to the workstation's own link. **The attribution holds. The characterisation was
the radio's.**

| campaign | recorded download | recorded upload | what it concluded |
|---|---|---|---|
| vhost 2026-09-10 | 390 Mbit/s | 885 | "this connection uploads more than twice as fast as it downloads" |
| public 2026-09-11 | 208–368 | 544–576 | "the cap is the radio's and it is asymmetric, in the opposite sense to what a domestic line usually is" |
| VPN 2026-09-12 | 150–416 | 414–705 | "the path is asymmetric and the download side is the narrow one" |
| **wired** | **930** | **737** | asymmetric downward, ~21 % |

Upload was compressed ~7 % by WiFi; **download was understated between 2.3× and
6×**. Every conclusion drawn about the download direction describes the radio.

**The vhost figure of 885 Mbit/s upload is not merely stale, it is impossible:**
it exceeds the wired uplink, over a radio sharing that uplink. It carries the
signature of the over-counting fault in §1.2. This matters in the direction that
flatters nothing: an inflated bare reference made that tunnel look **worse** than
it was, so vhost's "saturation" verdicts need re-reading against 930/737, not
against 390/885. Block C re-measures it.

**What survives untouched:** every *ratio* against a bare control sampled in the
**same repetition** — that is precisely why the campaigns measure them that way
(V-9) — and every stage that never crossed this access link (`vm/`, `pub/vm_*`,
`srv/`: those ran VM↔server inside one AWS region).

---

## 3. V-6 inverts: there is no 37 % direct-path deficit

`vpn_ab.sh`, 5 interleaved repetitions, bare control in every repetition, each
arm's path read back and verified.

| direction | bare | relay | direct |
|---|---|---|---|
| download (VM → workstation) | 929.9 | 573.6 (61.7 %) | **830.0 (89.3 %)** |
| upload (workstation → VM) | 734.0 | 554.2 (75.5 %) | **691.6 (94.2 %)** |

`direct/relay`: **1.447 download, 1.248 upload.** MTU settled at 1414 on direct,
1350 on relay.

The WiFi measurement this replaces is the premise of the entire V-6 stage, which
exists only to explain it:

```
upload    bare 414.5    relay 415.7 (100.3%)    direct 259.7 (62.7%)
download  bare 150.7    relay 132.1  (87.6%)    direct 138.7 (92.0%)
```

On WiFi the relay *reached* bare on upload and the direct path lost 37 %. Wired,
**the direct path is the fast one in both directions and the relay is the slow
arm.** The deficit V-6 was built to explain does not exist on this path.

Why the radio punished one transport and not the other is consistent with what
each is: the relay is TCP, which WiFi's link-layer retransmission and frame
aggregation are tuned to protect, while the direct path is QUIC datagrams whose
congestion controller reads the radio's jitter and reordering as loss. The
measurement says the deficit was the medium's; that mechanism is the plausible
account of it and is **not** itself measured here.

### V-6's own stage confirms it, and adds a second inversion

`vpn_direct_deficit.sh` re-run on the upload direction — the direction the
deficit was in — 2 repetitions, 20 s, bare control in each:

| rep | bare | relay | direct | QUIC on the direct arm |
|---|---|---|---|---|
| 1 | 740.9 | 494.9 (66.8 %) | **682.5 (92.1 %)** | rtt 38.4 ms, **lost 0.00 %**, cong_events 0 |
| 2 | 738.7 | 632.5 (85.6 %) | **707.3 (95.7 %)** | rtt 37.4 ms, **lost 0.00 %**, cong_events 0 |

The stage built to explain a 37 % deficit does not reproduce one. A stage that
comes back empty is the cleanest possible confirmation.

**The second inversion is CPU.** Earlier campaigns repeatedly found the direct
path billing the endpoints more per delivered GiB (P-13; the secret campaign).
Wired, it is the cheaper transport:

| arm | CPU per GiB | busiest thread |
|---|---|---|
| relay | 3.85 / 3.75 s/GiB | 4.0 % / 5.2 % of one core |
| direct | **3.34 / 3.42 s/GiB** | 7.0 % / 5.8 % of one core |

Direct uses more CPU per second of wall clock and less per **delivered gigabyte**,
which is the quantity that matters — it is moving far more data in the same
window. Neither arm is remotely CPU-bound (≤ 7 % of one core).

**Instrument gap, stated not hidden — now diagnosed and fixed.** The VM-side CPU
column read 0.00 s in every arm, announced by the stage as `VM bore pid not
found`. The workstation-side figures above are sound; the far end's cost was not
measured this run.

The cause was found by listing every VM process the selector matches, with its
`comm`:

```
pid=777798   comm=sudo         <- the wrapper
pid=777800   comm=bore-vpn     <- the binary
pid=777933   comm=bash         <- the ssh remote shell, matching its OWN argv
pid=777934   comm=
```

The selector kept the pid whose `comm` was the literal string `bore`. The binary
is **deployed to the VM under its own name** (`$VM_BORE`, today `~/bore-vpn`), so
`comm` reads `bore-vpn` and the comparison never matched — for the whole
campaign. It now selects by EXCLUSION (reject `sudo`/`env`/`sh`/`bash`/empty),
which also rejects the ssh shell's self-match and cannot be blinded again by
renaming the deployed binary. Red-checked against a live link: the old filter
selects nothing, the new one selects pid 777800 with a moving tick count.

`vpn_direct_deficit` is therefore queued for re-run in P7 to fill that column.
This is the reason the stage announces a missing pid instead of printing a zero:
a confident 0.00 would have been published as a finding.

### Consequences to carry into the rest of the sweep

- The relay's 61.7 % of bare on download is a **new** finding with no WiFi
  counterpart worth comparing to, and it is the larger gap now open. It was
  invisible before because bare download was itself 150–416.

- **Before that gap is called a relay defect, note that the three arms are not
  three transports over one path — they are two paths.** `bare` and `direct` run
  workstation ↔ test VM. The `relay` arm is server-mediated by construction, so
  it is DOUBLE TRANSIT through a third host: workstation ↔ staging server ↔ VM.
  That third host is a **t4g.micro with 2 vCPU**, and its allowance counters
  read:

  ```
  bw_in_allowance_exceeded:   11 808 249
  bw_out_allowance_exceeded:       78 317
  pps_allowance_exceeded:     26 595 715
  ```

  Those are cumulative since boot, so they prove the instance exhausts its
  network allowance *at some point* — **not** that it did during these arms. The
  distinction is the whole difference between evidence and a story, and this
  project has a standing rule about it: CLAUDE.md already records the t4g.micro's
  allowance bucket as the surviving hypothesis for the vhost concurrency tail
  (N-9), reached after every in-code mechanism was falsified.

  The honest reading today is therefore: **the relay figure is a property of this
  deployment, and how much of it belongs to the instance is not yet measured.**
  A dedicated stage must read `bw_*_allowance_exceeded` as a DELTA bracketing the
  relay arm, exactly as `asym_qualify.sh` does for the VM. Until then no relay
  percentage in this document should be quoted as a statement about bore's relay
  code — and that caveat applies to **every** campaign, since the relay always
  transits this server.

---

## 4. Running log of the sweep

| stage | status | headline |
|---|---|---|
| `link_baseline` | done | 930/936 down, 734/737 up; ENA allowance all zero |
| `asym_qualify` | done | upload ceiling follows the source: 737 is ours |
| `vpn_ab` | done | direct 89.3 % / 94.2 % of bare; V-6 inverted |
| `vpn_direct_deficit` | done | no deficit reproduces: direct 92.1 % / 95.7 % of bare, QUIC loss 0.00 %; direct also cheaper per GiB |
| `vpn_wire_ceiling` | done | tunnel/bare **1.000 up to 700 Mbit/s**, loss 0.0 %, no standing queue — the ceiling V-13 measured was the radio (§5) |
| `vpn_txqueue` | done | all five rungs **flat** (699.9–706.5 Mbit/s, 37.9–39.1 ms) — V-10's ladder does not reproduce wired (§6) |
| `vpn_sndbuf` | done | 512 KiB ≡ 2 MiB ≡ 8 MiB (678–685 Mbit/s, 39.2–39.8 ms) — V-13's inner-TCP ladder does not reproduce wired (§7) |
| `vpn_udpbuf` | done | 256 KiB → 16 MiB all 92–95 % of bare; between-rung spread = within-rung spread (§8) |
| `vpn_cc_matrix` | done, **re-run queued** | arms do not separate; the samples split into two disjoint groups *across* the arms — a harness bug, see §9 |
| `vpn_lat` | done | **direct idle RTT = bare idle RTT** (19.2 vs 19.6 ms); the tunnel adds nothing when idle (§11) |
| `vpn_profile` | done, **re-run queued** | the relay collapses with flow count, direct does not (§12); one rung contaminated |
| `vpn_modes` | done | overlay ≡ lanaddr ≡ netmap: the stateless 1:1 rewrite is free (§13) |
| `vpn_hub` | **re-run queued** | measured no bandwidth and its isolation verdict was an artefact — three harness defects (§14) |
| `vpn_stability` | done, **re-run queued** | **throughput fell 712 → 95–160 Mbit/s by the third reconnect**, with one descriptor leaked per reconnect (§15) |
| `sec_ab` | done | secret at 92 % / 87 % of bare; **direct's 22 % win over relay does not survive wiring** — it is now 2 % behind (§16) |
| `sec_eff` | done | 95 % / 94 % of bare; no retransmission signature; direct HALVES the receiver's wire packets (§17) |
| `sec_lat` | done | **direct p50 19.79 ms ≡ the bare RTT**, flat to 256 held connections; relay +4.4 ms (§17) |
| `sec_ack` | done | ACK thinning buys **nothing** on download and **costs 9 %** on upload — the shipped default is right (§18) |
| `sec_ttd` | done | **20/20 direct**, median 67 ms; S-5's ~1 s second mode is gone (§18) |
| `ws_rtt`, `ws_ref`, `ws_ref_vm`, `ws_ref_public` | done | references re-read wired; single stream 425 Mbit/s, x16 **840** to a third party |
| `ws_tunnel_paired` | done | vhost **110 MB/s (880 Mbit/s, 95 % of bare)** on every arm; carriers and QUIC direct neither help nor hurt goodput (§19) |
| `ws_tunnel` | **FAILED, re-run needed** | died in 0 s on an undefined `$B` — a committed defect, not a measurement (§19) |
| `ws_dl_parallel` | done | ISP ceiling x1 425 → x16 840 Mbit/s: one stream is Mathis-bound, the aggregate is not |
| `ws_dufs` | done | real file server: small-file throughput is 22× with concurrency (§20) |
| `ws_dufs_rq` | done | **vhost reaches the line: 925/924 Mbit/s down, 748/742 up** — relay ≡ QUIC on speed (§20) |

(Filled in as stages land.)

---

## 5. There is no wire ceiling, and no standing queue, below the line's own rate

`vpn_wire_ceiling.sh`, direct path, MTU settled at 1414, fixed UDP offer, bare
control at every rung, 2 repetitions:

| offered | bare delivered | tunnel delivered | tun/bare | tunnel rtt avg | rtt min |
|---|---|---|---|---|---|
| 300M | 300.0 (0.0 %) | 299.9 (0.0 %) | **1.000** | 16.97 | 16.04 |
| 450M | 449.8 (0.0 %) | 449.6 (0.1 %) | **1.000** | 16.67 | 16.06 |
| 600M | 599.7 (0.0 %) | 599.6 (0.0 %) | **1.000** | 16.87 | 16.05 |
| 700M | 699.9 (0.0 %) | 699.7 (0.0 %) | **1.000** | 17.05 | 16.75 |
| 800M | 762.6 (4.4 %) | 574.6 (17.3 %) | 0.753 | 36.30 | 18.28 |

Up to 700 Mbit/s the tunnel is **transparent**: it delivers what is offered, it
loses nothing, and `rtt avg − rtt min` is 0.3–0.9 ms, which is no queue at all.
The 800M rung is not a tunnel result — **bare itself loses 4.4 % there**, because
the offer is above this line's 737 Mbit/s uplink. Past the point where the path
cannot carry the offer, the tunnel degrades faster than bare; below it, it does
not degrade.

**What this replaces.** V-13 measured, at 540 Mbit/s offered, `8 MiB → 388
Mbit/s @ 227 ms` and a curve that turned over at 64 MiB. Wired, 700 Mbit/s
offered delivers 699.7 at 17.0 ms on the **shipped 8 MiB default**. V-13's own
headline — "there is no ~400 Mbit/s wire ceiling, the residual gap is a LATENCY
gap" — was right about there being no capacity ceiling; what it could not know is
that the latency gap was the radio's too. The buffer-depth optimum it derived
(32 MiB best, turn-over at 64 MiB) was measured on a path that could not carry
the offer, so **the ladder has to be re-run wired before any of those rungs is
quoted again.** The default was not changed on the strength of that ladder, which
is why this correction costs nothing shipped.

---

## 6. The TUN queue ladder is flat wired — V-10's evidence was the radio's

`vpn_txqueue.sh`, direct path, inner TCP upload, one link held across the whole
ladder, 2 repetitions, 12 s per rung:

| txqueuelen | upload | rtt avg | rtt min | qdisc drops |
|---|---|---|---|---|
| 500 (kernel default) | 699.92 | 38.12 | 30.61 | 0 |
| 256 | 705.88 | 37.93 | 30.67 | 0 |
| 192 | 704.74 | 38.30 | 30.87 | 0 |
| **128 (shipped)** | 705.82 | 38.15 | 31.11 | 0 |
| 64 | 705.58 | 38.55 | 30.65 | 0 |

Every one of the ten samples lies in 699.9–706.5 Mbit/s and 37.9–39.1 ms. The
ladder is **flat on both axes**, and the single lowest cell (500 → 699.92) is
matched by that same rung reading 706.05 in the other repetition, so it is noise.

**What this replaces.** V-10 measured `500 → 248.3 Mbit/s @ 147 ms` against
`128 → 265.7 @ 97`, and concluded the kernel default "is the worst rung on BOTH
axes". Wired, it is indistinguishable from every other rung.

**This does NOT make `VPN_TUN_TXQUEUELEN = 128` wrong, and the difference
matters.** V-10's measurement was real; what was misattributed is its *scope*. A
deep device queue only hurts when the queue actually fills, and wired at 95 % of
the uplink with zero qdisc drops it never does. On a radio — a link with jitter,
retransmission and a variable rate, which is what a large share of real users
have — it did, by 6× on latency. So 128 remains the better default for the
population of paths bore runs on; what must be corrected is the CLAUDE.md claim
that the kernel default is "the worst rung on BOTH axes", which is true of a
congested or lossy link and false of an uncongested wired one.

**The queue did not disappear, it moved.** Under a *matched UDP offer* (§5) the
tunnel shows 17.0 ms avg against 16.7 min — no queue. Under *inner TCP* at the
same rate it shows 38.1 ms avg against 30.6 min — a standing queue of ~19 ms
above the bare path's ~19 ms RTT, and `txqueuelen` does not move it. An inner TCP
flow fills whatever buffer is in front of it, and with the device queue ruled out
the remaining candidate is the QUIC datagram send buffer. That is precisely
V-10's own recorded rule — *latency is conserved across stages in series* — and
it is what `vpn_sndbuf` is measuring next.

---

## 7. The datagram send buffer ladder is flat too — and that closes out the tunables

`vpn_sndbuf.sh`, direct path, inner TCP upload, bare control in every
repetition, 3 repetitions:

| `BORE_DIRECT_DGRAM_SEND_BUF` | upload | % of bare | rtt avg under load | rtt min |
|---|---|---|---|---|
| 8 MiB (shipped default) | 684.00 | 92.9 % | 39.19 | 31.3–31.9 |
| 2 MiB | 678.00 | 92.1 % | 39.45 | 31.2–32.0 |
| 512 KiB | 684.55 | 93.0 % | 39.85 | 31.2–31.7 |

bare: 736.17 Mbit/s.

A **16× reduction** in the buffer moves neither throughput (0.9 % spread, inside
the repetition-to-repetition noise) nor latency (0.7 ms spread). The nine
individual samples span 661.7–686.5 Mbit/s with no ordering by rung.

This is decisive about the buffer's role rather than merely null. At 684 Mbit/s
the 512 KiB rung holds **6 ms** of traffic and the 8 MiB rung holds **98 ms**. If
a standing queue lived in that buffer, collapsing it by 16× could not leave the
latency unchanged. So on this path the buffer is **not full** — an inner TCP flow
paces itself to its congestion window and never offers more than the path takes,
which is exactly the mechanism V-13 itself identified when it explained why its
*first* ladder (driven by inner TCP) found nothing and its second (driven by a
fixed UDP offer) did.

**What this replaces.** V-13's fixed-offer ladder measured `8 MiB → 388 Mbit/s
@ 227 ms`, `32 MiB → 462 @ 708 ms`, `64 MiB → 410 @ 1092 ms` — a curve with an
optimum and a turn-over. Wired (§5) the same transport delivers 699.7 Mbit/s at
17.0 ms on the 8 MiB default. Those hundreds of milliseconds were a queue
standing in front of a radio that could not drain it.

### The running score on the VPN tunables

See §8 — a fourth ladder joins these two, with the same result.

### An instrument gap this creates, to close in P7

Both §6 and §7 report `rtt min` ≈ 31 ms under load against a bare path whose
idle RTT is ~19 ms — a 12 ms floor that no tunable moves — plus ~8 ms of variance
on top. **Neither stage samples the BARE path's RTT under load**, so that 12 ms
cannot currently be split between "the tunnel adds it" and "any flow saturating
this link adds it". Until it is, the floor is not attributable and must not be
quoted as a tunnel cost. P7 adds a bare-RTT-under-load column.

---

## 8. The UDP socket send buffer: a fourth flat ladder, and the check that makes it mean something

`vpn_udpbuf.sh`, `BORE_DIRECT_UDP_SEND_BUF`, direct path, inner TCP upload,
bare control in every repetition, 2 repetitions:

| rung | rep 1 | rep 2 | median | % of bare | rtt avg | rtt min |
|---|---|---|---|---|---|---|
| 16 MiB | 680.36 | 702.82 | 680.36 | 92.1 % | 38.10 | 18.79 |
| 4 MiB | 685.07 | 703.72 | 685.07 | 92.7 % | 38.53 | 22.15 |
| 1 MiB | 699.87 | 699.52 | 699.52 | 94.7 % | 38.83 | 30.74 |
| 256 KiB | 706.44 | 691.10 | 691.10 | 93.5 % | 37.51 | 20.47 |

bare: 738.95 Mbit/s.

**Three of these eight samples are contaminated, and §9 identifies why.** This
stage builds a fresh link per rung and discarded the result of
`wait_mtu_settle`, so a rung measured before the TUN finished climbing ran at a
short MSS. The signature is the same one §9 documents — low throughput paired
with a low `rtt min`:

| rung | sample | rtt min | |
|---|---|---|---|
| 16 MiB | 680.36 | **18.79** | contaminated |
| 4 MiB | 685.07 | **22.15** | contaminated |
| 256 KiB | 691.10 | **20.47** | contaminated |
| 16 MiB | 702.82 | 31.18 | settled |
| 4 MiB | 703.72 | 30.72 | settled |
| 1 MiB | 699.87 / 699.52 | 30.74 / 30.87 | settled |
| 256 KiB | 706.44 | 30.77 | settled |

Every contaminated sample has `rtt min` 18.8–22.1 ms; every settled one has
30.7–31.2. The groups do not overlap.

**Reading only the five settled samples, the ladder is flat and is now more
convincing, not less:** 16 MiB 702.82, 4 MiB 703.72, 1 MiB 699.87/699.52,
256 KiB 706.44 — a spread of **6.9 Mbit/s, under 1 %, across a 64× change in the
buffer.** Discarding the bad samples removed the apparent ordering along with
them, which is what should happen when the ordering was the artefact.

The `n` is small (five samples, one per rung plus a duplicate), so this is
"no effect large enough for this instrument to see", not a priced small one.
Re-queued as `vpn_udpbuf_r2` with the settle bug fixed.

### The check that keeps this from being a meaningless null

A buffer ladder whose rungs were all secretly the same buffer would produce
exactly this table. On this workstation `net.core.wmem_max` is **4 MiB**, so the
16 MiB rung is a distinct rung only if bore's `SO_SNDBUFFORCE` path — which needs
`CAP_NET_ADMIN`, which the VPN has — actually bypassed the clamp. The stage's own
`granted` column read `?` for the entire ladder, so it could not answer.

Read instead from the kernel, out of the live link's log (P-12's rule — the
kernel's view, never an assumption):

```
forced=true   effective_recv=33554432   effective_send=8388608     <- the 4 MiB rung
              ...                       effective_send=2097152     <- the 1 MiB rung
```

Linux reports `SO_SNDBUF` as **twice** the requested value, so `8388608` is
4 MiB and `2097152` is 1 MiB: each rung got exactly what it asked for. And
`effective_recv=33554432` is a **16 MiB receive buffer granted on a host whose
`rmem_max` is 4 MiB** — direct proof the forced setter works here. The rungs are
real, and so is the null.

**The stage is fixed** to grep `effective_send` (the emitted field) instead of
`actual_send` (a Debug-printed `Option` on a different statement), to print the
rung it expected beside it, and to say `CLAMPED` outright when the two disagree.
A ladder that cannot see its own independent variable is not a ladder.

---

## 9. The controller matrix did not measure controllers — it measured a harness bug

`vpn_cc_matrix.sh`, 7 arms × 2 repetitions, wired. The per-arm medians looked
like a result: `bbr` 95.2 % of bare, `cubic` 90.5 %, `newreno` 91.5 %. They are
not one.

Sorting the 14 raw samples by throughput instead of by arm:

| group | n | throughput | rtt min |
|---|---|---|---|
| high | 11 | 700.3 – 707.1 Mbit/s | 30.7 – 31.3 ms |
| low | 3 | 668.2 – 679.6 Mbit/s | 16.8 – 21.9 ms |

**The two groups are disjoint on both axes, and the split falls across the arms
rather than along them** — `cubic` has one sample in each group, and so do
`newreno` and `both1m`. Every arm that "lost" lost because it caught a low
sample; with two repetitions the median IS that sample. The medians are an
artefact of the draw.

### The cause, and it is ours

```bash
wait_mtu_settle cc >/dev/null 2>&1      # the old line
```

`wait_mtu_settle` returns **1** when it gives up without the MTU ever holding
still, and echoes the last value it saw. That line discarded the value *and* the
exit status. This stage builds a **fresh link per arm** — it has to, because the
controller is chosen by an environment variable read at process start — and a
fresh TUN climbs 1350 → 1288 → 1414 over roughly 25 s. So an arm whose MTU had
not finished climbing was measured anyway, at an MSS up to 9 % short of the one
every other arm used, and nothing in the output said so.

The signature fits: a smaller MTU means less goodput (Mathis: throughput is
proportional to MSS) *and* a lower `rtt min` (smaller packets drain the queue
faster) — which is exactly the joint direction the low group moves in. Predicted
MSS shortfall 1248 vs 1374 = 9.2 %; measured throughput shortfall ≈ 4.5 %,
consistent with a link that settled partway through the 15 s window.

### Fixed

* an arm whose MTU does not settle is now a **FAILED cell**, printed and not
  averaged in — the same rule this campaign already applies to an arm that did
  not reach its path;
* **the MTU is printed on every sample line.** It is an input to the number
  beside it, and a column that turns out constant is the cheapest possible proof
  that it was;
* `REPS` default 2 → **4**: two repetitions cannot rank seven arms, because a
  single bad sample anywhere becomes that arm's median.

Re-queued as `vpn_cc_matrix_r2` in `rerun_eth_p7.sh`.

### What this says about V-12 for now

V-12 measured, over WiFi, `newreno` 280.9 Mbit/s / `cubic` 278.6 / `bbr` 272.0
and recorded a clean separation. Wired, **there is no separation to report yet**
— not "the controllers are equal", but "this run could not tell", which is a
different and weaker statement. The decision V-12 records (keep `bbr`; re-open
only with a lossy path in the matrix) is untouched by that, because it was never
made on the strength of the +3.3 %.

### The general lesson, which is the reason this is written up at all

Three of the four ladders in §5–§8 came back flat and were reported as flat.
This one came back with *structure*, and the structure was the instrument. The
discriminator that separated them was not the medians — it was **sorting the raw
samples by the value instead of by the arm**, which is only possible because
every stage in this harness prints its raw samples beside its medians (V-11).
A stage that had printed medians alone would have published this.

---

## 10. Design rule this campaign earned: hold ONE link across a ladder

The three buffer/queue ladders differ in exactly one structural way, and it
predicts their data quality perfectly:

| stage | link | contaminated samples | spread of the settled ones |
|---|---|---|---|
| `vpn_txqueue` | **one link held across every rung** | **0 of 10** | 699.9–706.5 Mbit/s, `rtt min` 30.6–31.3 |
| `vpn_udpbuf` | fresh link per rung | 3 of 8 | 699.5–706.4 once cleaned |
| `vpn_cc_matrix` | fresh link per arm (forced: the controller is read at process start) | 3 of 14 | — |

A fresh link per rung buys independence and costs a ~25 s MTU climb that has to
be waited out correctly every single time. One link held across the ladder pays
that once. Where the independent variable can be changed without restarting the
process — `txqueuelen` is set with `ip link`, live — **hold the link.** Where it
cannot (an environment variable read at startup), the settle check is not
optional and its exit status is not optional either.

`vpn_txqueue` is the model: ten samples, one link, `rtt min` constant to 0.7 ms
across the whole ladder, and a flat result that nothing in §9 casts doubt on.

---

## 11. The tunnel adds no latency when idle — and §7's "12 ms floor" was a load effect

`vpn_lat.sh`. Reference is a TCP handshake to the VM (ICMP to it is dropped by
its security group), median of 15: **19.61 ms**.

| | idle min | idle avg | under download | under upload |
|---|---|---|---|---|
| bare (reference) | — | **19.61** | not sampled | not sampled |
| **direct** | 19.23 | **19.59** | avg 30.01 (+10.4, ×1.53) | avg 38.12 (+18.5, ×1.95) |
| relay | 23.61 | 24.08 | avg 45.65 (+21.3, ×1.88) | avg 56.94 (+32.6, ×2.34) |

**The direct tunnel's idle RTT is indistinguishable from the bare path's** —
19.59 against 19.61, with `mdev` 0.17 ms. Encapsulation, AEAD, the TUN hop and
QUIC together cost, on this path, nothing that this instrument can see.

**This closes the question §7 left open.** §6 and §7 both reported an `rtt min`
of ~31 ms under load against a bare *idle* RTT of ~19, and that 12 ms could not
be split between "the tunnel adds it" and "the load adds it". It is not a fixed
tunnel cost: the same tunnel idles at 19.2. It appears only under load — the
direct arm's own minimum goes 19.23 idle → 30.64 under upload — so it is
queueing, not encapsulation.

**What is still not attributed** is whether a *bare* flow saturating this link
would queue the same amount. `vpn_lat` does not sample the bare path under load
either, so the ×1.53/×1.95 bufferbloat figures are the tunnel's *absolute*
behaviour, not its behaviour *relative to the link*. That is exactly what
`vpn_rtt_load.sh` measures, and it is queued in P7. Until it runs, these
multipliers must not be quoted as a cost the tunnel imposes.

**The relay's +4.5 ms at idle is, by contrast, structural and expected:** the
relay arm is a double transit through the staging server, so it pays that hop
twice. It is the one relay figure here that needs no attribution stage.

---

## 12. The relay collapses with flow count; the direct path does not

`vpn_profile.sh`, flow ladder on one link per path.

| flows | relay down | relay up | direct down | direct up |
|---|---|---|---|---|
| 1 | 606.82 | 550.49 | 820.98 | 694.70 |
| 2 | 775.73 | 318.02 | *536.85* † | 692.05 |
| 4 | 511.24 | 249.71 | 872.33 | 694.52 |
| 8 | 439.23 | 205.95 | 874.08 | 695.68 |

† contaminated — see below.

**The direct path's upload is flat to four significant figures across an 8×
change in concurrency: 694.70 / 692.05 / 694.52 / 695.68 Mbit/s.** That is the
behaviour wanted from a tunnel: the aggregate is the link, and the flows share
it. Download rises to 874 at 8 flows, which is 94 % of the 930 bare reference.

**The relay's upload falls monotonically — 550 → 318 → 250 → 206 — and at eight
flows it is 3.4× below the direct path.** Download shows the same shape after
the flows=2 rung.

**No part of that is yet a statement about the bore relay code**, for the reason
§3 sets out and P7 exists to settle: the relay arm is a double transit through a
2-vCPU `t4g.micro`, and per-flow collapse under concurrency is precisely what a
CPU-bound or allowance-limited middlebox looks like. `vpn_relay_attrib.sh`
measures the same double transit through a **non-bore splice relay on the same
host**; if that collapses too, this is the deployment. It is the single most
consequential open question in the campaign, because every campaign here relays
through this one server.

### † The flows=2 direct rung is a harness artefact, and it names its own cause

820.98 → **536.85** → 872.33 → 874.08 is not a concurrency curve: the two
*highest* rungs are the two *most* concurrent. This stage holds one link per
path and walks the ladder on it, and before this evening it did not wait for the
MTU to settle first. A fresh TUN sits at 1288 from roughly t+5 s to t+25 s — and
the ladder's second rung is measured in exactly that window.

The shortfall (38 %) is larger than the MSS ratio alone predicts (1248/1374 =
9 %), which is consistent with the MTU also *changing* mid-transfer: a shrink
costs TooLarge-dropped packets on top of the smaller segments.

Fixed the same way as §9 — settle first, record the MTU in the section header,
skip the path if it never settles — and re-queued as `vpn_profile_r2`.

---

## 13. Gateway, LAN address and netmap cost the same — the 1:1 rewrite is free

`vpn_modes.sh`, 2 repetitions × 10 s, inner TCP upload, bare control in each
repetition.

| arm | median | % of bare | % of plain overlay | raw samples |
|---|---|---|---|---|
| bare | 734.72 | — | — | 734.72 · 739.77 |
| overlay (spoke's own address) | 685.26 | 93.3 % | 100.0 % | 694.54 · 685.26 |
| lanaddr (through the gateway to a LAN address) | 699.47 | 95.2 % | 102.1 % | 699.47 · 705.85 |
| netmap (`--advertise real@virtual`, nft PREROUTING rewrite) | 700.12 | 95.3 % | 102.2 % | 700.12 · 702.26 |

**The three tunnel arms are the same number.** The spread *within* the overlay
arm (694.54 → 685.26, 9.3 Mbit/s) is as large as the spread *between* the arms,
so with two repetitions the ordering is noise and the ">100 % of overlay"
readings are not a claim that adding a NAT rewrite makes a tunnel faster.

What the measurement does support is the negative, and it is the useful half:
**the stateless 1:1 netmap costs nothing measurable.** That is the result the
design predicts — the rewrite is host-bits-preserving, stateless, has no
conntrack and never touches bore's data plane (I-NAT3/I-NAT8: the packets stay
opaque and every rule is kernel nft) — and it is the first time the prediction
has been checked on a real path rather than in netns.

All three arms sit at **93–95 % of bare**, consistent with every other wired
tunnel figure in this document.

### What this stage did NOT measure

`LAN_HOST` was unset, so the FORWARD-chain probe was skipped: the `lanaddr` and
`netmap` arms priced the PREROUTING rewrite and the gateway hop, **not**
forwarding to a third host. The stage also reported `far end FORWARD policy:
DROP`, which is exactly the condition `--forward-accept` exists for — so on this
pair a real LAN host behind the far gateway would have been unreachable without
it. Setting `LAN_HOST` to a machine behind the far end is what turns that from a
note into a measurement.

---

## 14. The hub stage measured no bandwidth, and its one verdict was an artefact

`vpn_hub.sh` finished in **19 seconds with rc=0**, which is what prompted
reading it rather than filing it. Three defects, all in the harness:

**1. The spoke-isolation probe could not fail.** Both spokes run on this
workstation, so spoke B's overlay address is a *local* address here, and Linux
answers any packet to a local address out of the `local` routing table without
putting it on a wire:

```
$ ip route get 192.168.1.6
local 192.168.1.6 dev lo table local src 192.168.1.6
```

The ping therefore never reached the hub, and a hub isolating its spokes
perfectly would still have read `REACHABLE`. The stage printed
`REACHABLE -- isolation broken` — a security verdict the measurement could not
support in **either** direction. It now detects the local destination and
reports NOT MEASURABLE, naming the topology that would answer it (two spokes on
two hosts) and the gate that already does (`T-HUB*` in
`scripts/vpn_netns_test.sh`, which has real separate namespaces).

**2. The hub's overlay address never resolved, and that silently deleted the
whole bandwidth measurement.** `HUB` was parsed out of a spoke log line with a
guessed pattern; it matched nothing, and the `if [ -n "$HUB" ]` that followed
skipped the reachability probe *and* the throughput run without a word. The
stage reported success having measured no bandwidth at all. It now reads the
address from the **hub's own kernel** (`ip -o -4 addr show` on the VM) rather
than from any log — P-12's rule — and says so loudly when it cannot.

**3. The stage's own headline question was printed as a bare number.** It exists
to check whether hub mode runs the authenticated check round or still falls
through to the legacy blind punch, and the count of
`hub peer check round finished` came back **0** while two peers upgraded to
direct. Those are not the same question: `try_hub_peer_direct` builds a
`CheckConfig` only when the brokered punch carried a v2 payload and otherwise
takes the `None` arm — the legacy blind punch — which on an easy NAT reaches
direct anyway. That is precisely why this went unnoticed for a traversal
generation. The stage now prints both counts and a verdict.

**The 0 is not yet a code finding.** The working tree contains the fix (the
prod-versus-punch skip in the hub punch loop, `src/vpn.rs`), so the open
question is whether the **deployed** hub binary on the VM does — the campaign's
own provenance line records the workstation binary, not the far end's. Checking
that is a prerequisite to reading this as a defect, and the re-run says so in
its own output.


---

## 15. The connector loses most of its throughput by the third reconnect

`vpn_stability.sh`, one connector with `--auto-reconnect` held across three
cycles; each cycle reaches direct, runs three 20 s inner-TCP uploads, then the
VM listener is SIGTERMed and brought back.

| cycle | phase | rss_kib | threads | fds | up_mbit | path |
|---|---|---|---|---|---|---|
| 0 | start | 23 836 | 19 | **12** | — | relay |
| 1 | load1/2/3 | 41 920 → 42 032 | 18 | 12 | 695.66 · 711.83 · **712.58** | direct |
| 1 | back | 42 788 | 19 | **13** | — | direct |
| 2 | load1/2/3 | 44 320 → 46 216 | 18 | 13 | 711.08 · 712.11 · **712.12** | direct |
| 2 | back | 53 192 | 19 | **14** | — | direct |
| 3 | load1/2/3 | 53 588 → 53 848 | 18 | 14 | **95.35 · 159.87 · 118.51** | direct |

Three things move together, all monotonically:

- **descriptors: 12 → 13 → 14**, exactly one per reconnect;
- **RSS: 23.8 → 53.8 MiB**, with the step falling on the `back` sample (42 788 →
  53 192 across the second reconnect — ~10 MiB, far more than a socket);
- **throughput: 712 → 712 → ~120 Mbit/s**, a factor of six, sustained across all
  three rounds of the cycle rather than a single bad sample.

### It is not the link, and not the MTU

The driver's next bare baseline, taken **29 seconds** after this stage ended,
read **927 Mbit/s down / 733 up** — the highest of the evening (the two block-A
baselines were 923/730 and 924/730). So while the tunnel was delivering 95–160,
the bare path between the same two hosts was capable of 733.

`wait_mtu_settle` succeeded in all three cycles — the stage prints an explicit
`UNSETTLED mtu` line otherwise and printed none — so this is not §9's artefact.
The path column reads `direct` throughout, so it is not a silent fallback to
relay. The stage also holds ONE connector process for its whole life by design
(a process restarted between cycles could not leak across them), so the cycles
are comparable.

### The descriptor leak is the same defect, measured with a second instrument

The open control-connection leak was found with `ss`, counting `ESTABLISHED`
connections toward the server's control port, and it grew 1→2→3→4 across
reconnects. Here it appears independently as `/proc/<pid>/fd`, read from the
kernel through the root helper, growing one per reconnect on a different host
and a different day. Two instruments, same rate.

What is new is that it no longer looks free. A leaked descriptor costs a
descriptor; it does not obviously cost **six sevenths of the throughput**. The
mechanism connecting the two is not established, and a connection's
worth of buffers is a candidate rather than a conclusion.

### What this run cannot say, and what the re-run fixes

**This stage carried no bare control inside the cycle** — the one thing V-9
forbids — so its own output could not separate a degrading tunnel from a
degrading line; the 733 above came from the *next stage's header*, 29 s later,
which is strong but not the same measurement. It also read no allowance
counters, so a far-end shaping episode that had recovered by the time that
baseline ran is not fully excluded.

Both are now in the stage: a bare sample and an ENA delta bracketing every
cycle, printed as `tunnel/bare` beside the raw pair. The re-run is queued as
`vpn_stability_r2` with **five** cycles rather than three, because three cycles
cannot distinguish a cliff at the third reconnect from a slope that began at the
first.

**Until it runs, this is the campaign's most consequential open finding and the
strongest reason the control-connection leak is worth fixing in P5.**
*(Superseded the next day — read the subsection below before quoting this one:
the connector's own log says the collapse is path loss, and the leak is not its
cause. The leak was fixed regardless.)*

### RE-READ 2026-09-13 from the connector's own log: it is LOSS, and the leak is
### not its cause

The stage's summary could not separate the two, but the connector's log was
still on disk (`/run/bore-vpn-bench/sta.log`, the whole run — 18:44:31 to
18:51:04) and its 5 s QUIC carrier samples answer it outright. Per-sample, while
the inner TCP upload was running:

| cycle | rtt_ms | lost_pkts per 5 s | cong_events | cwnd | carrier tx |
|---|---|---|---|---|---|
| 1 | 32.0 – 42.2 | **0 in every sample** | 0 | 8.7 – 11.2 MB | 760 – 763 Mbit/s |
| 2 | 26.9 – 41.5 | **0 in every sample** | 0 | 8.7 – 11.2 MB | 761 – 764 |
| 3 | **18.6 – 19.2** | **2 – 8** | 2 – 8 | 2.8 → 11.7 MB | 41 – 260 |

And on the same lines, our own drop counters through all of cycle 3:
`buffer_drop_est=0`, `tx_drops_total=0`, bridge `tx_drops=0`. Nothing was
dropped inside this process.

**Three facts kill the leak-as-cause reading:**

1. **The loss is on the path.** Cycles 1 and 2 moved 5.52 GB and 5.55 GB with
   `lost_pkts_d` exactly zero in *every* sample at 762 Mbit/s. Cycle 3 moved
   0.97 GB and lost packets in eleven of thirteen samples. A leaked descriptor
   or a leaked allocation cannot make a WAN path drop packets.
2. **The RTT went DOWN, not up.** 32–42 ms in cycles 1–2, 18.6–19.2 ms in cycle
   3 — the bare path's own RTT. Whatever a leak does, it does not remove a
   standing queue. (The low RTT is a *consequence*: with the inner flow
   collapsed the sender no longer fills any buffer. It is evidence about the
   shape of the failure, not independent evidence of a policer.)
3. **A monotone cause cannot produce a cliff.** Descriptors and RSS rose at
   every reconnect, including the one between cycle 1 (712.58 Mbit/s) and cycle
   2 (712.12) — where throughput did not move at all. The §15 reading of "three
   things move together" was wrong: two moved monotonically, the third stepped
   once.

**The mechanism, and why 0.01 % is enough.** The direct path carries inner IP
packets as QUIC *datagrams*, which are unreliable by design (retransmitting
under a tunnelled TCP flow is the TCP-over-TCP meltdown this project must not
build). So a dropped datagram IS a dropped inner TCP segment, and the inner flow
pays Mathis: `MSS / (RTT · √p)`. At 1350 B, 19 ms and p = 1e-4 that is
**≈ 69 Mbit/s** — the same order as the 41–260 measured. The tunnel did not
degrade; a single TCP flow met a path that had started dropping.

**What the loss most likely is, stated as a hypothesis with a number.** Before
cycle 3 this stage had pushed **11.07 GB** into the test VM in about four and a
half minutes of wall clock — roughly 712 Mbit/s while active, against a
`c7i-flex.large` whose sustained allowance is a fraction of that and whose burst
credit is finite. The next bare baseline, 29 s later, read 733 Mbit/s up and is
NOT evidence against this: it is a ~10 s burst after an idle gap, which is
exactly what a refilled credit bucket serves and a 20 s sustained flow is not.
A local cause is separately excluded by `tx_drops_total=0` and
`buffer_drop_est=0` above.

**What this changes in the harness** (both now in `vpn_stability.sh`, so no
future run can reproduce this ambiguity):

- `ws_quic_since` prints, per cycle, the carrier's `lost`/`sent`/`cong_events`
  and its RTT range. A slow cycle with `lost=0` and a slow cycle with `lost>0`
  are opposite diagnoses and used to look identical in the output.
- `ws_nic_drops` brackets each cycle with this end's `tx_dropped`/`tx_errors`,
  so "we dropped it" and "the path dropped it" separate without reading a log.
- The bare control inside each cycle runs for `LOAD_S` — the SAME duration as
  the tunnel arm. On a credit-limited path a shorter control is not a control:
  it measures the burst bucket while the arm measures the sustained rate.

**The control-connection leak was fixed anyway** (`src/mux.rs`, connection
liveness; `docs/vpn/VPN_CTRL_CONN_LEAK.md`) — it is a real defect, measured with
two instruments, and one descriptor per reconnect at both ends is worth
removing. What it is no longer is the explanation for §15's collapse.


---

## 16. The secret path: 92 % of bare — and direct's 22 % win over relay was the radio

`sec_ab.sh`, topology `vm-ws`, 128 MiB per arm over 4 connections, 5 pairs,
75 s cooldown between arms.

| direction | relay | direct | vs bare |
|---|---|---|---|
| get (download to the workstation) | 106.29–108.52 MB/s (**~856 Mbit/s**) | 102.39–107.00 | **92 %** of the 927 bare |
| put (upload from the workstation) | 76.93–82.28 MB/s (**~640 Mbit/s**) | 77.39–80.26 | **87 %** of the 733 bare |

Time-to-direct was 58–86 ms on every pair, and all ten arms reached the path
their label claims — no exclusions, `n=5 of 5` in both directions.

### The inversion

| | WiFi (2026-09-11) | wired (2026-09-12) |
|---|---|---|
| valid pairs | 3 of 5 | **5 of 5** |
| samples | 0.965 · 1.247 · 1.218 | 0.972 · 0.977 · 1.000 · 0.963 · 0.998 |
| median direct/relay | **1.218** — direct wins by 22 % | **0.977** — direct 2 % behind |
| spread | 29 % | 3.7 % |

This is the same shape as V-6 and V-13 in the VPN campaign, and it has the same
explanation. The relay arm is a **double transit**; a lossy, variable radio
penalises two hops far more than one, so the radio manufactured an advantage for
the direct path. Remove it and the advantage goes with it — the two paths become
the same number, with the direct path a consistent two percent behind.

Note which measurement is the more trustworthy one on its own terms: the WiFi
median rested on **three** surviving samples spanning 29 %, the wired one on
**five of five** spanning 3.7 %. A 29 % spread is not a measurement of a
transport, it is a measurement of a link.

### What this does and does not change

It does **not** overturn the secret campaign's central result, which was never a
throughput claim: the direct path bills the **endpoints** instead of the server
(server CPU 49 s → under 1 s per transfer) and costs less in total. That is a
CPU accounting result, measured on both ends, and nothing here touches it.

What it does retire is any statement of the form "the direct path is faster".
On a clean path it is not — which is the same conclusion F-8 reached for vhost,
and the reason `--udp` direct is a **CPU and server-load** feature rather than a
bandwidth feature. The campaign's own rule (V-9) predicted exactly this: ratios
against a control sampled in the same repetition survive a change of link;
these did not, because the ratio was *between two paths of different length*
over a link whose cost per hop was the thing that changed.


---

## 17. The secret path costs ~0.2 ms and 94 % of the line, and holds it under concurrency

### Throughput and wire efficiency (`sec_eff`, 2 GiB per arm, 4 connections)

| arm | goodput | vs bare | path |
|---|---|---|---|
| relay | **110.11 MB/s** (880 Mbit/s) | **95 %** of 927 | relay, fb=0 |
| direct | **109.01 MB/s** (872 Mbit/s) | **94 %** | direct, fb=0, ttd 65 ms |

**No retransmission signature.** The far end's NIC transmit per delivered GiB is
759 876 (relay) against 763 193 (direct) — 0.4 % apart. P-13's defect showed up
as a 1.78× inflation of bytes-in over bytes-delivered; nothing of that shape is
present here, on either transport.

**The direct path halves the receiver's wire packets.** On a download the
workstation is the receiving end, and its NIC transmit count per GiB is
**108 716 on relay against 59 638 on direct**. TCP's delayed ACK puts a segment
on the wire for roughly every second data segment; QUIC's acknowledgement frames
cover far more packets each. This is the packet-level face of the result the
secret campaign already reached in CPU terms — the direct path bills the
endpoints less — measured here as frames rather than cycles.

### Latency, and what concurrency costs (`sec_lat`, 100 probes per rung)

| held connections | relay p50 | direct p50 |
|---|---|---|
| 0 | 24.106 | **19.796** |
| 16 | 24.220 | **19.814** |
| 64 | 24.307 | **19.794** |
| 256 | 24.146 | **19.795** |

Zero errors at every rung, p99 never above 29.7 ms.

Two things stand out.

**The direct path's p50 is the bare RTT.** The network round trip on this pair
measures 19.61 ms (§11, an independent stage using a different instrument on a
different day). A full secret-tunnel request and response completes in 19.79 ms
— about **0.2 ms** of tunnel on top of the wire. The direct secret path is, to
the precision available here, free.

**The relay costs +4.4 ms, and it is the same +4.5 ms the VPN relay costs.**
§11 measured the VPN's relay arm 4.5 ms above its direct arm at idle. Two
different products, two different stages, two different instruments, one shared
staging server: the figure is the cost of the extra hop, and it now has two
independent measurements.

**Concurrency costs nothing on either path.** 256 held connections move the
relay's p50 by 0.04 ms and the direct path's by 0.001 ms. That is worth
recording against N-9 — the vhost campaign's unexplained "concurrency tail",
where a fresh request behind 256 held connections took 966 ms and behind 512
took 1 436 ms. It is **not** a refutation: that was the vhost frontend with
HTTPS connections, and this is the secret path with tunnel connections, so the
load shape differs. But whatever produces N-9 is not simply "this server with
256 connections held open", because here that costs 40 microseconds.

**Correction to an earlier draft of this section: Block C does not re-measure
N-9.** No stage in `scripts/perf/staging/ws/` runs a held-connection ladder —
the ladder lives in `scripts/vhost_concurrency_ladder.sh`, which is the
*local private-server* experiment that already read 11 ms flat from 16 to 512
connections on both transports. Re-running that here would be a third local
experiment, and CLAUDE.md's standing note on N-9 says exactly why that settles
nothing: the surviving hypothesis is the t4g.micro's own network-allowance token
bucket, which is a property of the instance. **Settling it needs a same-region
client driving the real staging instance**, which this workstation cannot be.
N-9 therefore stays open, and this wired window cannot close it.


---

## 18. Two defaults confirmed against the temptation to change them

### ACK thinning is a trap, and now it is a measured one (`sec_ack`)

Both arms are `--udp` direct; the only difference is
`BORE_DIRECT_QUIC_ACK_THRESHOLD=10 BORE_DIRECT_QUIC_ACK_MAX_DELAY_MS=1`.
Topology `vm-vm`, so this one is intra-region and was never distorted by the
access link — it is here because the block runs it, and it is worth having on
current code.

| direction | ratio ack=10 / default | pairs |
|---|---|---|
| get | **1.001** | 0.981 · 0.988 · 1.001 · 1.021 · 1.034 (n=5 of 5) |
| put | **0.912** | 0.906 · 0.911 · 0.912 · 0.934 · 0.942 (n=5 of 5) |

Thinning acknowledgements **buys nothing on download and costs 9 % on upload**,
and the upload penalty is consistent — every one of the five pairs is between
0.906 and 0.942, with no overlap against the download arm. `BORE_DIRECT_QUIC_ACK_THRESHOLD`
stays at its shipped value, and the reason is now a measurement rather than an
argument about the peer's `max_ack_delay` transport parameter.

Absolute rates here are the intra-AWS path: 271–390 MB/s.

### Traversal is clean, and S-5's slow mode is gone (`sec_ttd`)

20 consecutive establishment attempts, topology `vm-ws`:

```
  direct : 20        relay : 0        failed : 0
  time-to-direct ms: median 67   min 57   max 401
```

Worth putting beside what S-5 was written to fix. That work started from a
**bimodal** distribution measured on staging: 18 runs in 37–53 ms and 9 runs in
1036–1162 ms with nothing between, the slow mode being quinn's initial PTO
(333 + 4 × 166 ms) burned because the listener kept probing an address the
dialer had already stopped answering.

Here there is **no second mode at all**: 20 of 20 attempts, worst case 401 ms,
median 67. That is the field confirmation the netns suite structurally cannot
give — netns has no real NAT — and it holds on the wired path.


---

## 19. vhost wired: 95 % of the line, and the transports separate on COST, not speed

`ws_tunnel_paired.sh`, workstation as consumer (the shape a real browser takes:
the bytes cross the access link once), 4 parallel streams, 10 s bursts, 75 s
cooldown, every arm paired against a `relay c=8` reference **in the same
repetition**.

| arm | median ratio vs reference | proven path | pool |
|---|---|---|---|
| `c0auto` | **1.004** | relay | carriers=2, target=1 |
| `quic-c1` | **0.997** | direct | carriers=1 |
| `quic-c4` | **0.988** | direct | carriers=4, target=4 |
| `quic-c0auto` | **0.999** | direct | carriers=1 |

Every arm, reference included, sits at **109–110 MB/s ≈ 880 Mbit/s**, which is
**95 % of the 929 Mbit/s bare** measured at the start of the block. Fallbacks
were 0 everywhere and every "direct" arm was verified direct from the admin API
rather than assumed.

**Goodput does not separate the transports.** Four carriers are not faster than
one; QUIC direct is not faster than the relay. On a clean line the differences
are inside the noise, and the two low outliers (0.803 and 0.916, one per arm)
are single bursts, not a trend — the other two pairs of each arm read 0.99–1.00.

**What does separate them is what the server pays.** The `allowance_misses`
column, read as a delta bracketing each burst:

| | reference (relay c=8) | configuration under test (direct) |
|---|---|---|
| per burst | +87, +89, +323, +324, +363, +428, +532, +567, +715, +801, +805, +1219 | +0, +0, +0, +0, +0, +0, +0, +14, +36, +98, +160 |

The relay arm spends the staging instance's network allowance on **every**
burst; the direct arms spend essentially none. That is the same conclusion the
secret campaign reached in CPU seconds and §17 reached in wire packets, arriving
here a third way: **`--udp` direct is a server-cost feature, not a bandwidth
feature.** An operator choosing it to go faster has the wrong reason; an
operator choosing it to stop paying for a relay hop has the right one.

The WiFi vhost campaign recorded 390 Mbit/s of download on this path. Wired it
is 880 — the 2.3× understatement the whole window was opened to correct.

### `ws_tunnel` did not run, and its failure is a committed defect

```
scripts/perf/staging/ws/ws_tunnel.sh: line 19: B: unbound variable
```

`$B` is referenced and never assigned; under `set -u` the stage aborts before
measuring anything, and the driver logged `FAIL ... elapsed=0s`. The same
defect sits in `ws_flavours.sh` and `ws_dufs_relay_vs_quic.sh`, both of which
were still queued behind it, and it is present in the committed tree (`fe8a9d6`)
rather than introduced tonight.

Had `$B` resolved, those two would have written a 256 MB and a 1 GB payload into
`scripts/perf/staging/ws/work/` — a path git does not ignore. All three now use
`lib.sh`'s `WORK`, which lives outside the repository. `ws_tunnel` left no
marker, so re-running the driver picks it up; the three arms it measures are not
covered by `ws_tunnel_paired`.


---

## 20. vhost through a real file server reaches the line rate in both directions

`ws_dufs_relay_vs_quic.sh`, 4 streams, 10 s bursts, 75 s cooldown, **3 rotated
rounds** so neither arm always goes first, both paths verified from the admin
API (`relay` never opened a direct stream; `quic` opened 226 and fell back 0).

| direction | relay | QUIC direct | bare reference | tunnel / bare |
|---|---|---|---|---|
| download | **110.21 MB/s (925 Mbit/s)** | 110.16 (924) | 929 | **99.6 %** |
| upload | **89.12 MB/s (748 Mbit/s)** | 88.42 (742) | 737 | **101 %** |

Upload reading slightly *above* the bare baseline is not a tunnel that beats the
wire: it means the tunnel measurement and the baseline landed on a line whose
own rate moved by about 1.5 % between them. The honest statement is that
**upload through the tunnel is indistinguishable from upload without one.**

Rotation matters here and is why the medians can be trusted: the `by position`
line shows each arm measured from both first and second slot
(`p1=109.97 p2=110.21 p1=110.22`), so a systematic advantage to whichever arm
runs first cannot hide in the median.

Request latency on small sequential GETs separates the transports by **0.35 ms**
(relay p50 65.85 / p95 76.99 against QUIC p50 65.50 / p95 76.95) — which is to
say, not at all.

**And again the cost, not the speed, is where they differ.** Allowance deltas
across the download bursts: relay +86, +324, +53; QUIC +0, +0, +0. Three
campaigns, three instruments — CPU seconds (secret), wire packets (§17),
allowance misses (§19 and here) — all say the same thing and none of them says
"faster".

### What `ws_dufs` adds: concurrency is the whole story for small files

Same tunnel, a real `dufs` origin:

| workload | at 1 | at 8 | at 32 |
|---|---|---|---|
| 500 × 8 KiB GET | 16 files/s (61.10 ms each) | 130 files/s (7.67) | **371 files/s (2.70)** |
| 500 × 8 KiB PUT | 50 files/s (19.82 ms each) | — | **715 files/s (1.40)** |

A 23× and 14× improvement from concurrency alone, on an unchanged tunnel. Bulk
numbers on the same run: 10 × 20 MiB in parallel 694 Mbit/s; 256 MiB PUT 692
(×1), 706 (×2), 626 (×4).

The operational reading is the one the Mathis bound predicts and `ws_dl_parallel`
independently confirmed on a third-party endpoint (425 Mbit/s at one stream,
840 at sixteen): **at ~19 ms RTT a single TCP flow cannot fill this line, through
a tunnel or without one.** Parallelism is not a tuning knob here, it is the
workload's own property.


---

## 21. The public-tunnel stages were sized for a radio, and wired they measure TCP slow start

This is a defect in the **instrument**, found the way the campaign plan says to
find one — by comparing a stage's elapsed time with its budget before reading
its result — and it invalidates the spread of every public-tunnel figure in
Block D.

### The arithmetic

Four stages move a fixed **96 MiB** per arm (`ws_asym`, `ws_dl`, `ws_carr`) or
per single connection (`ws_dl1`); `ws_conns` moves **24 MiB per connection**.
Those numbers were chosen when this workstation was on WiFi, where 96 MiB took
about two seconds. The wired line measured 922 Mbit/s in the same hour:

    96 MiB / 115 MB/s = 0.83 s
    24 MiB / 115 MB/s = 0.21 s

A single TCP flow at 19 ms RTT needs roughly 1.1 MB in flight to reach
465 Mbit/s; starting from a 10-segment initial window that is **seven round
trips, ~133 ms, before the rate is even approached**, and the flow is still
opening when the transfer ends. So what these cells report is the ramp.

### The output already said so

`pub_ws_dl1` — six pairs of the SAME two arms, one variable between them:

    0.745  0.984  0.991  1.323  0.979  1.005

`pub_ws_carr` — three rounds, carriers 1 against carriers 4:

    0.623  1.029  1.148

A 1.8× spread between repetitions of an experiment whose two arms differ by one
setting is not a property of the tunnel. `pub_ws_conns` is the clearest case of
all: **615 s of stage, of which 600 s was cooldown** (eight cells × 75 s) and
about fifteen seconds was transfer.

### What it does and does not invalidate

It does NOT touch the medians' central tendency — `pub_ws_dl1`'s median ratio
0.9875 and `pub_ws_carr`'s 1.029 are still the best available estimates, and
they agree with the in-region campaigns. What it invalidates is any reading of
the SPREAD, and with it any single-cell claim: in particular `pub_ws_conns`'s
apparent 36 % relay collapse at eight connections (92.41 → 58.96 MB/s), which
rests on one sample per cell and cannot be distinguished from the noise the
other stages display openly.

### The fix, and the rule it leaves behind

`XFER_MB` is now a variable in all four stages with a wired default of **384
MiB** (~3.3 s at line rate); `XFER_MB=96` reproduces the original figures
exactly, so the old runs remain comparable rather than orphaned. `ws_conns.sh`
moves 256 MiB per connection, takes three repetitions, alternates the direction
it walks the connection ladder, and prints its raw samples — none of which it
did before. `pub_ws_asym_r2`, `pub_ws_dl1_r2`, `pub_ws_carr_r2` and
`pub_ws_conns_r2` are queued in P7, deliberately LAST so that a driver which
runs out of window loses these rather than the VPN attribution.

The general rule is recorded as **V-19** in `CLAUDE.md` and as trap 16 in
`scripts/perf/staging/README.md`: *a transfer shorter than the ramp measures the
ramp.* Byte counts are calibrated against a line, so when the line changes every
one of them silently becomes a different experiment. It is a near relative of
V-15's corollary (a control shorter than its arm is not a control) but bites
even when both arms are equally short — which is why no A/B comparison protects
against it.

### One thing the same stage got right, and it matters for the relay question

`ws_conns`'s relay column rises 57.98 → 81.04 → 92.41 MB/s from one to four
connections. Even discounted as a ramp measurement, the direction is the one
`ws_dl_parallel` sees against a completely unrelated endpoint (425 Mbit/s at one
stream, 840 at sixteen) and the one §20's dufs numbers see: **at this RTT the
single flow is the bottleneck, tunnel or no tunnel.** Any relay percentage taken
from a single-connection download is therefore quoting Mathis, not bore — and
the reference it should be quoted against is the bare workstation↔**SERVER**
figure that `vpn_relay_attrib.sh` takes, because the relay arm's last hop is
server→workstation and the workstation↔VM baseline every other stage uses is a
different host.

---

## 22. `bore transfer` reaches 99 % of the line — except on one cell, and one hypothesis is already dead

`xfer_bw`, 1024 MiB single file, workstation → VM (so the reference is the
**upload** baseline, 740 Mbit/s):

| `--parallel` | relay MiB/s | direct MiB/s |
|---|---|---|
| 1 | 87.5 | **45.8** |
| 2 | 86.7 | 87.8 |
| 4 | 88.4 | 88.1 |
| 8 | 88.6 | 87.8 |
| 16 | 75.4 | 86.0 |

87.5 MiB/s is 734 Mbit/s — **99.2 % of bare upload**, on both transports, with
the path verified from the sender's own log on every row. That is the headline
and it is a good one: the file-transfer mode gives up essentially nothing.

### The one cell that does not fit

Direct at `--parallel 1` reads 45.8 MiB/s and `--parallel 2` recovers the line
exactly. A per-stream bound that halves with one stream and disappears with two
is the shape of `something / RTT`, and the arithmetic is inviting:
`CHUNK_SIZE / RTT` = 1 MiB / 19 ms = **52.6 MiB/s**, the right size.

**That hypothesis is falsified by the same table.** The relay arm runs the SAME
chunk protocol at the SAME RTT and reads 87.5 MiB/s at `--parallel 1`, so the
bound cannot live in the chunk loop. Flow control is out too, and by reading
rather than by measurement: `holepunch::transport_config` is the only place in
this crate that builds a `quinn::TransportConfig` (P-13's single-funnel rule,
re-checked here), and it installs a 16 MiB per-stream receive window — 842 MB/s
at this RTT.

What survives is specific to ONE QUIC stream on the direct path: congestion
control, quinn's pacer, or receive-side CPU on the 2-vCPU listener. Note the
secret campaign recorded the same shape from the other end — its own
single-stream finding — while `sec_eff`'s four-connection direct arm reaches
109 MB/s, i.e. the line.

### Why this is not yet a finding

One sample, one cell. It is recorded here with its falsified hypothesis so the
next campaign does not spend the same hour re-deriving `CHUNK_SIZE / RTT` and
believing it. Settling it needs what V-15 already requires of every direct-path
throughput stage and this one does not yet do — **print the carrier's loss and
RTT beside the rate** — plus a second repetition. The cheap form is
`REPS=2 PARS="1 2" ARMS=direct`, noted in the stage's own header.

### The other two cells worth a line

`relay --parallel 16` falls to 75.4 MiB/s while direct stays at 86.0: sixteen
yamux substreams on one TCP carrier against sixteen independent QUIC streams,
which is the one regime where the relay's single congestion window is visibly
shared. It is a 14 % effect at a parallelism nobody needs (the table saturates
at 2) and is not worth a knob.

`relay --parallel 1` at 87.5 MiB/s deserves noting for the opposite reason: it
is the cell V-19 predicts should be worst (one stream, longest ramp) and it is
not, because this stage moves 1024 MiB and therefore runs for 11.7 s. It is the
counter-example that shows the public stages' spread was the instrument and not
the tunnel.

---

## 23. Against the state of the art, `bore transfer` is the state of the art

`xfer_sota`, 1024 MiB in one file, workstation → VM, `--parallel 8`, every tool
on the same link in the same six minutes:

| tool | MB/s | what it is |
|---|---|---|
| **bore-direct** | **82.05** | hole-punched QUIC, server off the path (`path=direct`, verified) |
| **bore-relay** | **81.59** | through the server — TWO WAN legs |
| tar-ssh | 80.19 | best case for the classics: one stream, no per-file protocol |
| link | 80.06 | `ssh` → `/dev/null`: no filesystem, no transfer protocol at all |
| scp | 79.88 | direct TCP + SSH |
| rsync | 79.44 | direct TCP + SSH |

Two things to read carefully before quoting this.

**First, `link` is not a bare control.** It is `ssh` to `/dev/null`, so it
carries SSH's own crypto and its 2 MiB channel window (the ceiling this
repository already measured and documented in `docs/` for the SSH gateway). It
is the right reference for "what the classical tools can reach on this link",
which is what this table is about, and it is NOT the reference for "what the
line can carry" — that is the 740 Mbit/s upload baseline, i.e. 88.2 MB/s. Every
row here sits at 90-93 % of the line, and the spread between the six of them is
3.3 %.

**Second, bore-direct above `link` is real but small.** 82.05 against 80.06 is
+2.5 %, one repetition, on a spread of 3.3 % across the whole table. The honest
statement is not "bore is faster than ssh" — it is that **a tunnel with NAT
traversal, a relay fallback and AEAD framing costs nothing measurable against
tools that dial the host directly**, and that the relay arm, which crosses two
WAN legs and a third machine, is within half a percent of the direct one.

That second clause is the one worth keeping. It is also the counterpart to §21:
where the public-tunnel stages could not separate their own arms because they
measured 0.8 s of TCP slow start, this stage moves 1024 MiB over 12.5 s and
separates six tools by 3 %.

---

## 24. WHOSE ceiling is it? — the question every percentage in this repository rests on, answered

`asym_qualify.sh` exists to answer one question: when this campaign writes
"88 % of bare", is "bare" a property of **this** end (the home uplink, the NIC,
the kernel) or of the **test VM's** ingress? A ratio taken against the wrong
reference is precise and meaningless.

The discriminator is DESTINATION: a limit that follows the source reads the same
toward every destination; a limit belonging to one destination does not. The
same offered load therefore goes to three places, and P=1 is kept beside P=8
because that is V-9's one-line diagnosis — a per-flow limit (window, loss,
Mathis) opens with parallelism, a policer or a hard link rate does not.

| destination | down P=1 | down P=8 | up P=1 | up P=8 |
|---|---|---|---|---|
| test VM (iperf3) | 926.5 | 933 | 726.5 | **740** |
| `ping.online.net` (France, unrelated provider) | — | 917 | 583 | **730** |
| Cloudflare (HTTPS POST, kernel counters) | *instrument failed* | *instrument failed* | 210 | **779** |

**The answer: the ceiling is ours.** At P=8 three independent destinations —
two different providers, two different protocols — agree within 6 %: 730, 740,
779 Mbit/s up and 917/933 down. The VM's own ENA allowance counters
(`bw_in_allowance_exceeded`, `bw_out_allowance_exceeded`,
`pps_allowance_exceeded`) read **zero before and zero after every single VM
arm**, so the instance never noticed the load at all.

So **740 Mbit/s up and ~925 down is this workstation's line**, and it is the
correct reference for every "percent of bare" in every campaign in this
repository. The three earlier campaigns that attributed the asymmetry to this
end were right about the attribution; §0 of this document already corrected
their characterisation (WiFi, not the cable).

The P=1 column is worth its own line. Upload at one stream reads 726 to the VM,
583 to France and 210 to Cloudflare — a clean ordering by RTT, which is the
signature of `window / RTT` and not of a policer. It is the same statement
`ws_dl_parallel` makes on download (425 Mbit/s at one stream, 840 at sixteen)
and the same one §20's dufs table makes: **at this RTT a single TCP flow cannot
fill this line, with or without a tunnel.** Any single-connection percentage in
this repository is quoting Mathis before it quotes bore.

### Two defects in the stage itself, both now fixed

**It was wired to no driver.** It had been run by hand, left a `.out` with no
marker, and its answer — the one every other number depends on — sat unread
until this pass. That is the same defect class as `vpn_overhead.sh` and
`jump_lat.sh`: a stage nobody runs is a gate that does not exist. It is now the
FIRST stage in `rerun_eth.sh`, ahead of `link_baseline`.

**It published a failure as a measurement.** The Cloudflare download arm printed
`0` in all four cells — not "slow", but a NIC counter delta of essentially zero,
i.e. curl fetched nothing and the table reported it as a rate. A zero that means
"the instrument failed" is the most expensive way a harness can be wrong. `cf()`
now cross-checks curl's own `%{size_download}` against the kernel's counters and
prints `FAILED(http=...)` when nothing moved, and `add()` refuses any non-numeric
cell so a failure can never enter a median.

---

## 25. Il conto AWS: quale metà si può togliere, e quale no

La domanda posta è stata: *si può consumare meno banda verso AWS senza falsare i
test?* La risposta è sì, ma non dove verrebbe naturale cercarla — e la parte
grossa non è una questione di quanti byte si spostano, è una questione di **dove
vanno**.

### 25.1 Il fatto che decide tutto: AWS fattura una direzione sola

AWS fattura l'**uscita** (AWS → workstation) e non fattura l'**ingresso**. La
workstation è dietro TIM, flat, senza limite. Quindi:

- ogni arm di **download costa**;
- ogni arm di **upload è gratis**, da entrambe le parti.

Non è una sfumatura contabile: cambia quali fasi si possono toccare. Misurato
sulle fasi VPN che restavano da eseguire, **`vpn_overhead`, `vpn_direct_deficit`,
`vpn_stability` e `vpn_hub` misurano solo in upload** — non costano nulla e non
vanno accorciate di un secondo. Le fasi che costano davvero sono quelle con `-R`
(`vpn_relay_mtu`, `vpn_profile`, `vpn_relay_attrib`) e tutto il blocco `pub/`.

Una campagna che "riduce il traffico" senza guardare la direzione taglia per metà
proprio la metà gratuita.

### 25.2 Il termine più grosso non arriva mai qui

Misurato con `scripts/perf/staging/aws_cost.sh` su una finestra di 47 s dentro
`vpn_relay_attrib`:

| | GiB |
|---|---|
| uscita VM | 0.27 |
| uscita server di staging | 0.30 |
| **fatturato totale** | **0.56** |
| ricevuto da questa workstation | 0.29 |
| **differenza (hairpin VM↔server)** | **0.27 — il 48 %** |

VM e server di staging stanno **nella stessa VPC** (indirizzamento privato della VPC), ma l'harness
li indirizza per **IPv4 pubblico**: il traffico fra i due esce e rientra, e viene
fatturato una seconda volta. Nelle fasi a forma di relay — cioè quasi tutto il
blocco `pub/` e l'arm relay della VPN — **ogni byte consegnato qui si paga due
volte**.

È l'unico termine che si può togliere **senza accorciare una sola misura**. Ed è
anche l'unico che **cambia il percorso sotto misura**: la tratta VM→server
passerebbe da pubblica a privata, e tutti i numeri già registrati in questa
campagna sono stati presi sulla tratta pubblica. Cambiarlo adesso renderebbe le
ri-esecuzioni non confrontabili con le esecuzioni che devono confrontare, che è
esattamente il prodotto della campagna.

**Quindi: non è stato cambiato.** È una decisione dell'operatore, da prendere
*fra* due campagne e non dentro una, e vale circa la metà del conto.

### 25.3 Cosa è stato tagliato davvero, e perché non perde dati

1. **`pub/ws_conns.sh` — byte per RUNG invece che per CONNESSIONE.**
   Non è un risparmio travestito da correzione: è una correzione che risparmia.
   Il disegno originale teneva costanti i byte per connessione «così ogni cella
   paga la stessa rampa», ma **la rampa è un tempo, non un conteggio di byte**
   (V-19). A byte per connessione costanti la cella a n=1 durava 2,2 s e quella a
   n=8 diciassette — la cella che aveva bisogno di più durata ne riceveva meno.
   Ora ogni rung sposta `AGG_MB` (460 MiB) in totale: tutte le celle stanno fra
   ~5 s e ~8 s, cioè fra trenta e sessanta volte i ~133 ms di rampa. Il fatturato
   della fase passa da ~22,5 a ~11 GiB. `PER_FIXED=<byte>` riproduce la forma
   vecchia per chi deve rifare i numeri vecchi.

2. **`vpn_cc_matrix_r2` non rieseguita.** CLAUDE.md V-12 dice testualmente di non
   riaprire la scelta del controllo di congestione *«su un numero di throughput a
   percorso singolo; riaprirla con un percorso LOSSY nella matrice»*. Questa
   ri-esecuzione è esattamente un numero di throughput a percorso singolo: non
   può cambiare la decisione. Era la fase VPN più lunga (981 s, sette arm × quattro
   ripetizioni più un controllo nudo ciascuna).

3. **`vpn_udpbuf_r2` non rieseguita.** La prima esecuzione aveva già risposto
   PIATTO su cinque campioni puliti; otto campioni puliti su una curva piatta
   sostengono la stessa decisione. 577 s di scala a piena velocità risparmiati.
   Le due decisioni sono scritte per esteso in `out/eth/vpn_cc_matrix_r2.out` e
   `out/eth/vpn_udpbuf_r2.out`, con il modo di annullarle (`rm out/eth/_done.<fase>`).

Quello che **non** è stato toccato: nessun `SECS`, nessun `REPS`, nessun `CYCLES`,
nessun arm di upload, nessuna baseline. Accorciare una misura sotto la sua rampa
è il difetto V-19 — che questa campagna ha già pagato una volta.

### 25.4 D'ora in poi il conto si misura

`scripts/perf/staging/cost_watch.sh` segue il driver vivo, legge i contatori NIC
dei due host AWS **solo nei momenti di passaggio fra una fase e l'altra** (mai
dentro una misura) e scrive `out/eth/_cost_stage.tsv`: per ogni fase i secondi,
l'uscita della VM, l'uscita del server, quanto è arrivato davvero qui e quanto è
hairpin. La prossima campagna decide su una tabella, non su una stima.

`aws_cost.sh` conteneva a sua volta un difetto trovato usandolo: `$3` dentro il
programma awk è il **terzo campo del record**, non il terzo argomento della
shell, quindi l'helper leggeva il campo numero ~1,1·10¹² e tornava vuoto. È
morto forte (`syntax error: operand expected` dentro `$(( ))`) invece di
riportare 0 GiB — l'unica ragione per cui non è diventato una buona notizia
falsa.

### 25.5 Difetto trovato mentre si misurava: `vpn_relay_attrib` non può rispondere

La fase di punta di P7 pubblica `bare-py 0.00`, `leg1 0.00`, `leg2 0.00` accanto a
un `bare` iperf3 che legge 932/747 Mbit/s. Non è lentezza: i security group AWS
**aprono alla workstation soltanto la 5299 sulla VM** e **80/443/7835 più
l'intervallo delle porte pubbliche sul server**. Lo strumento python della fase
si mette su 5311/5312/5313, verificando il bind *sull'host remoto* (`ss -lnt`) —
che riesce — mentre da qui le tre porte sono filtrate. Verificato a mano:

    <VM>:5311  filtrata      <SERVER>:5312  filtrata
    <SERVER>:5313  filtrata      <VM>:5299  aperta

Conseguenza: l'arm relay di bore contro il bare resta valido, ma **l'attribuzione
per tratta — la domanda per cui la fase esiste — non viene misurata**. In banda
non costa nulla (le connessioni non si aprono), costa il risultato. `add()` scarta
già gli zeri dalle mediane, quindi nessun numero falso è stato pubblicato: la fase
sbaglia *tacendo*, non *mentendo*.

La correzione è di forma, non di infrastruttura: gli endpoint devono stare su
porte che il security group già ammette — `IPERF_PORT` (5299) sulla VM, a turno
con il server iperf3, e una porta dell'intervallo pubblico sul server. Da fare
quando P7 ha finito: **non si modifica uno script mentre una sua copia è in
esecuzione.**

**Regola che questa fase ha guadagnato:** *verificare la raggiungibilità dal lato
che misura, non il bind dal lato che serve.* `ss -lnt` sull'host remoto dice che
il processo è partito; non dice che qualcuno possa parlargli.

### 25.6 …e il meccanismo è DOPPIO, non uno solo

La prima diagnosi ("i security group") era giusta a metà. Verificato:

| tratta | esito | meccanismo |
|---|---|---|
| workstation → VM:5311 | bloccata | security group: la VM ammette **solo 5299** |
| workstation → server:5312/5313 | bloccata | security group |
| **server → VM:5311** | bloccata | security group (nemmeno fra i due host AWS) |
| **VM → server:5312** | bloccata | security group |
| workstation → server:9031/9041/9077 con un processo in ascolto **sull'host** | bloccata | **NON** il security group |

L'ultima riga è la scoperta. Il server di staging esegue bore **in Docker**, e la
catena nat `DOCKER` fa DNAT delle porte pubbliche verso il container:

    tcp dport 443  dnat to 172.18.0.2:7835
    tcp dport 7835 dnat to 172.18.0.2:7835
    tcp dport 9000 dnat to 172.18.0.2:9000
    ... (l'intero intervallo delle porte pubbliche)

Quindi un processo che ascolta sull'**host** su una porta di quell'intervallo non
riceve nulla: il pacchetto viene dirottato nel container prima di arrivargli. È
esattamente perché `bore local --port 9031` funziona — bore ascolta *dentro* il
container, e il DNAT gli consegna.

**Conseguenza pratica.** Un helper di misura piazzato sul server di staging ha
due sole strade: una porta che il security group ammette e che il DNAT **non**
dirotta (oggi: nessuna), oppure girare *dentro* il container — il che significa
toccare il servizio vivo dell'utente. Sbloccare l'attribuzione per tratta è
quindi una **modifica infrastrutturale**, non una correzione di script:

1. aprire nel security group una coppia di porte fra workstation↔VM,
   workstation↔server e server↔VM;
2. verificare che quelle porte non cadano nell'intervallo DNAT del server.

Fino ad allora la fase deve **rilevare** l'irraggiungibilità e saltare gli arm
che non può eseguire, invece di aprire per tre ripetizioni connessioni che non
possono riuscire — che è come ha speso circa due terzi del suo tempo di parete.

**Regola generale, valida oltre questa fase:** su un host che esegue il prodotto
in un container, *il piano di rete dell'host non è il piano di rete del
prodotto*. Verificare la raggiungibilità **e** dove il pacchetto viene consegnato.

---

## 26. Quanto costa quello che resta, fase per fase

§25 ha stabilito il principio; questa è la stima applicata a ogni fase ancora da
eseguire, perché "risparmiare" senza sapere dove sono i byte è tirare a indovinare.

Tariffe usate: uscita verso internet ≈ **$0,09/GB**; traffico fra due istanze per
IP pubblico ≈ **$0,01/GB per direzione**. Quindi un GiB *consegnato qui* attraverso
l'hairpin costa circa **$0,11**, e un GiB consegnato da un percorso diretto
workstation↔VM circa **$0,09**.

Velocità misurate in questa campagna e usate per la stima: nudo cablato
925 Mbit/s (115,6 MiB/s), P=1 verso la VM 583 Mbit/s, relay VPN 459 Mbit/s,
diretto VPN ~830, tunnel pubblico 782.

| fase | arm di download | GiB consegnati | hairpin | **GiB fatturati** |
|---|---|---|---|---|
| `vpn_relay_attrib` | 3×10 s nudo + 3×10 s relay | 5,2 | sul solo relay | **6,9** |
| `vpn_rtt_load` | 3 arm × 3 rip × 14 s | 11,6 | sul solo relay | **14** |
| `vpn_overhead` | — solo upload | 0 | — | **0** |
| `vpn_relay_mtu` | 4 rung × 3 rip × (nudo+tunnel) | 13,5 | sul solo tunnel | **18** |
| `pub_ws_conns_r2` | 4 rung × 2 arm × 3 rip × 460 MiB | 11,3 | sì | **22,5** |
| `vpn_direct_deficit_r2` | — solo upload | 0 | — | **0** |
| `vpn_ctrl_leak` | connessioni, non traffico | ~0 | — | **~0** |
| `vpn_profile_r2` | 2 percorsi × (4 rung + carico) × 10 s | 8 | sul solo relay | **11** |
| `vpn_hub_r2` | — solo upload | 0 | — | **0** |
| `vpn_stability_r2` | — solo upload | 0 | — | **0** |
| `pub_ws_asym_r2` | 3 rip × 384 MiB | 1,1 | sì | **2,3** |
| `pub_ws_dl1_r2` | 6 coppie × 2 arm × 384 MiB | 4,5 | sì | **9** |
| `pub_ws_carr_r2` | 3 rip × 2 arm × 384 MiB | 2,3 | sì | **4,5** |
| `asym_qualify` | 2 rip × 8 s (solo l'arm VM; CDN e Ookla non sono AWS) | 1,9 | no | **1,9** |
| `ws_tunnel` | 20 s per direzione | 1,8 | sì | **3,5** |
| `vpn_traversal`, `vpn_quic_timers`, `vpn_carriers` | negoziazione, ping, e upload | ~0,5 | — | **~1** |
| `jump_lat` | latenza, non banda | ~0,1 | — | **~0,2** |
| `jump_hol` | 2 arm × 3 rip × 15 s di bulk | 3,9 | no (ws→VM→ws, un solo transito fatturato) | **3,9** |
| `jump_stab` | latenza + un canale per verifica | ~0,1 | — | **~0,2** |
| | | | **totale** | **≈ 99 GiB ≈ €8,4** |

**Cosa dice la tabella.** Metà delle fasi VPN rimaste costano **zero** — misurano
in upload, che AWS non fattura. Il conto si concentra in quattro voci
(`pub_ws_conns_r2`, `vpn_relay_mtu`, `vpn_rtt_load`, `vpn_profile_r2`) e più di
metà del loro costo è l'hairpin di §25.2.

**`jump_hol` è l'unica voce nuova con un prezzo vero, ed è dichiarato prima di
spenderlo.** Il suo carico attraversa il link d'accesso due volte
(client ssh sulla workstation → gateway sulla VM → provider → sshd della
workstation), ma **una sola** di quelle tratte è uscita AWS, quindi non è un
hairpin nel senso di §25.2: ogni byte è fatturato una volta. A ~400 Mbit/s,
6 carichi da 15 s fanno ≈ 3,9 GiB ≈ **€0,35**. Non è tagliabile senza
cancellare la misura: il carico deve **saturare** il canale per formare la coda
che rende visibile il blocco di testa, quindi non può essere limitato in banda;
e deve essere limitato nel **tempo** e non nei byte, o i campioni di latenza
cadono fuori dal carico. €0,35 per la prima prova sul percorso reale che la
correzione russh vendorizzata funziona è il rapporto migliore di tutta la
tabella.

### 26.1 Quanto hanno già risparmiato i tagli

| taglio | GiB fatturati evitati |
|---|---|
| `ws_conns` a byte per rung (era 45 GiB) | ~22,5 |
| `vpn_cc_matrix_r2` non rieseguita | ~25 |
| `vpn_udpbuf_r2` non rieseguita | ~15 |
| | **≈ 62 GiB ≈ €5,5** |

Cioè circa il **40 %** di quello che P7 avrebbe speso, senza rinunciare a una
sola misura che potesse cambiare una decisione.

### 26.2 Tagli esaminati e RIFIUTATI, con il prezzo di ciascuno

Li elenco perché un taglio non fatto è una decisione quanto uno fatto.

- **Portare `XFER_MB` da 384 a 256 MiB nelle tre fasi pubbliche.** Risparmierebbe
  ~5 GiB (≈ €0,45). Rifiutato: la cella scenderebbe da 3,5 s a 2,3 s e la rampa
  passerebbe dal 2 % al 3 % del trasferimento. I rapporti appaiati non ne
  risentono, ma i valori assoluti sì, e 384 MiB è il default che V-19 ha appena
  stabilito misurando. Spostare un default appena fissato per mezzo euro è il
  modo in cui una campagna perde la propria linea di riferimento.
- **Un controllo nudo per RIPETIZIONE invece che per RUNG in `vpn_relay_mtu`.**
  Risparmierebbe ~6,5 GiB (≈ €0,6). Rifiutato: il controllo appaiato dentro la
  stessa cella è precisamente ciò che rende validi i rapporti su una linea che
  deriva (V-9). Si risparmierebbe rompendo il disegno che rende leggibile la fase.
- **Meno coppie in `pub_ws_dl1_r2` (6 → 4).** Risparmierebbe ~3 GiB. Rifiutato: la
  prima esecuzione ha prodotto rapporti da 0,745 a 1,323; la ri-esecuzione esiste
  per stringere quella dispersione, e la si stringe con più campioni, non con meno.
- **Accorciare `SECS`/`LOAD_SECS`.** Rifiutato ovunque: è esattamente il difetto
  V-19, che questa campagna ha già pagato una volta.
- **Indirizzare VM e server per IP privato.** Risparmierebbe circa **metà** del
  totale. Rifiutato *in questa campagna* per il motivo di §25.2 — cambia il
  percorso sotto misura — e raccomandato per la prossima, dove sarebbe la scelta
  coerente dall'inizio.

### 26.3 Dove ho invece deciso di SPENDERE di più

`pub/ws_asym.sh` decideva a quale dei due fattori — la linea o l'allowance
dell'istanza — appartiene l'asimmetria download/upload, e lo decideva con **un
solo campione per direzione**. Un campione non separa un effetto del 20 % dal
rumore, e la regola V-11 (una statistica va pubblicata con i campioni accanto)
non era nemmeno applicabile: non c'era statistica.

Ora fa `REPS` (3) ripetizioni appaiate e stampa mediana *e* campioni. L'ordine
degli arm **alterna**: con il download sempre per primo, il download misura
sempre un bucket di allowance appena ricaricato dal cooldown precedente e
l'upload sempre 75 s più tardi — cioè esattamente l'asimmetria che la fase deve
attribuire. Le ripetizioni dispari partono dal download, le pari dall'upload.

Costo: **+0,75 GiB** fatturati, circa **€0,08**. Un numero che non si può
difendere vale meno della banda che è servita a produrlo.

---

## 27. Il jump host: quattro domande su cinque avevano una fase, e la quinta no

La revisione del jump host doveva essere l'ultimo blocco di misura della
finestra. Prima di eseguirla ho confrontato quello che il piano chiede con
quello che l'harness contiene, e il confronto ha prodotto un difetto **prima**
di produrre un numero — che è l'ordine giusto.

### 27.1 La mappa domanda → fase, che non esisteva

`docs/performance/ETH_CAMPAIGN_PLAN.md` §P6 pone al jump host cinque domande.
`rerun_jump.sh` dichiarava **una sola fase**, `jump_lat`:

| # | domanda del piano | fase | stato prima |
|---|---|---|---|
| 1 | scomposizione dell'apertura di sessione | `jump_lat` (`tcp`/`wchan`/`open`) | coperta |
| 2 | RTT applicativo a sessione aperta | `jump_lat` (`chan`/`echo`) | coperta |
| 3 | relay contro diretto (QUIC) | `jump_lat` (bracci interlacciati) | coperta |
| 4 | costo dei carrier | `jump_lat` (braccio `direct4`) | coperta |
| 5 | **stabilità: rekey, ritorno su relay warm quando l'UDP muore** | — | **nessuna fase** |

La lacuna non era visibile da nessuna parte. Il driver chiudeva `rc=0`, la fase
produceva una tabella completa di mediane, e nulla nel repository diceva che la
metà che importa di più non veniva posta. È il difetto del punto 5 della
relazione in una forma peggiore: lì una fase esisteva e nessuno la eseguiva, qui
la fase non esisteva affatto **e il risultato sembrava completo**.

Correzione strutturale, non un'aggiunta: la tabella qui sopra è ora in testa a
`rerun_jump.sh`. Una domanda scoperta si vede nel file che dovrebbe risponderle.

### 27.2 E una sesta domanda che il piano non poneva

Questo repository **vendorizza russh** per correggere il blocco di testa (HOL)
fra canali della stessa sessione SSH: un client lento o in streaming bloccava
**tutti** gli altri sullo stesso tunnel (`crates/russh/HOL_FIX.md`). Quella
correzione ha solo gate unitari — e il registro di copertura di questo progetto
dice, per esperienza pagata due volte, che **un test in processo falsifica
esattamente questa classe**: su loopback il consumatore drena in modo
opportunistico e la coda che rende visibile il difetto non si forma mai.

Quindi la correzione spedita non aveva alcuna prova sul percorso reale. Ora c'è
`jump_hol.sh`: un trasferimento bulk su un **secondo canale della stessa
sessione**, con la latenza di tasto campionata **dentro** il carico, e il
rapporto `loaded/idle` come misura dell'isolamento.

Il carico è limitato **nel tempo e non nei byte**, per la stessa ragione per cui
`vpn_rtt_load` lo è: un conteggio di byte finisce in istanti diversi su bracci
con throughput diverso, i campioni cadono fuori dal carico, e la fase riporta
"nessuna interferenza" con perfetta sicurezza.

### 27.3 Come si uccide il percorso diretto senza uccidere il resto

La fase di stabilità ha bisogno che il percorso diretto **muoia mentre il relay
TCP verso lo stesso server resta vivo** — che è come muore sul campo, e l'unica
forma in cui la promessa del prodotto (*"un QUIC assente, morto o che non apre
ripiega per lo STESSO canale sul TCP warm, e non uccide mai la sessione SSH
esterna"*) è verificabile.

Il meccanismo esiste già: `scripts/vpn_tun_endpoint.sh blackhole on <ip>` crea
una tabella nft **propria** che scarta l'UDP da e verso **un solo** indirizzo, e
`off` la rimuove ed è idempotente, quindi può stare in un trap di pulizia.

Avevo concluso che quel percorso non fosse fra i comandi `sudo` NOPASSWD, e
stavo progettando la fase attorno a quel vincolo — cioè rinunciando
all'esperimento. **La conclusione era sbagliata**: l'elenco contiene anche una
riga glob `scripts/*`, che copre quel file (il glob non attraversa `/`, e lì non
ce n'è). `sudo -n -l <percorso assoluto>` risponde in una riga e chiude la
domanda. Regola: una capacità *creduta assente* costa quanto una creduta
presente, e nel primo caso si rinuncia all'unica misura che rispondeva.

### 27.4 Il difetto che sarebbe costato l'intera campagna P6

`jumplib.sh::jump_path` — l'unica funzione che dice su quale percorso sta il
provider, e quindi quella che **etichetta ogni braccio** — chiedeva:

```
.[] | select(.alias == $a) | .current_path // "unknown"
```

`SshJumpView` (`src/admin_views.rs`) **non ha né `alias` né `current_path`**.
Quei campi esistono per vhost e public, non per il jump host. Il selettore non
seleziona nulla, `jq` non stampa nulla, la funzione ritorna **vuoto**: ogni
braccio `--udp` sarebbe stato atteso per 90 s e poi dichiarato `FAILED`, e la
campagna avrebbe prodotto **zero dati sul percorso diretto** sembrando
semplicemente sfortunata.

È la classe più cara di questa campagna — *uno zero che significa "lo strumento
ha fallito"* — alla quarta occorrenza (`cf()`, `vpn_hub`, `vpn_relay_attrib`, e
ora questa), e l'unica finora **intercettata prima dell'esecuzione**, leggendo
la struttura che l'API serializza invece di fidarsi della simmetria fra
registri.

Il percorso è ora **derivato** dai campi che esistono davvero:

| campo | cosa dice |
|---|---|
| `hostname` | il nome ProxyJump completo, la cui prima etichetta è l'alias |
| `udp_active` | almeno un carrier QUIC diretto è **vivo** |
| `direct_stream_opens` | canali SSH aperti **con successo** sul diretto |
| `direct_fallbacks` | canali che avevano chiesto UDP e sono stati serviti dal relay |

con un terzo esito, `absent`, distinto da `relay`: un provider che non si è mai
registrato e uno registrato sul relay sono diagnosi opposte.

E la fase di stabilità non si accontenta dell'etichetta. **Un'etichetta che
cambia è prova più debole di un contatore che sale**: `udp_active` può cadere
perché il carrier è scaduto in silenzio senza che nessuno lo usasse, mentre
`direct_fallbacks` si incrementa solo quando un canale che aveva chiesto UDP è
stato davvero servito dal relay warm. Quella è la promessa, quindi quello è il
numero.

### 27.5 Cosa dichiara la fase quando non può misurare

Ogni verifica di `jump_stab` stampa `PASS`, `FAIL` o **`SKIP`**, e una verifica
non valutabile è `SKIP`, mai `PASS`. Se il blackhole non si installa la fase si
ferma in preflight invece di misurare un percorso indisturbato e chiamarlo
resilienza. La distinzione è la lezione più cara della finestra: *"nessun
fallimento osservato"* e *"nessuna osservazione"* sono indistinguibili in una
tabella riassuntiva, ed è precisamente come `vpn_hub` ha chiuso in 19 secondi
con `rc=0`.

### 27.6 Ordine di esecuzione, e perché

`jump_lat` → `jump_hol` → `jump_stab`. L'ultima è l'unica che **modifica il
piano di rete di questa workstation** (una tabella nft di root): rimossa da un
trap di `EXIT` e di nuovo dal proprio preflight, ma una fase che scrive sul
firewall dell'host viene dopo quelle che si limitano a misurarlo.

---

## 28. L'attribuzione del relay: metà del risultato è arrivata, e la fase dice quale metà manca

`vpn_relay_attrib` ha chiuso `rc=0` in 3582 s. Il numero che pubblica è questo:

| braccio | down | up | percorso |
|---|---|---|---|
| `bare` | **932,97** | **747,81** | ws↔VM, iperf3 — il riferimento della campagna |
| `bare-py` | — | — | ws↔VM, python (fedeltà dello strumento) |
| `leg1` | — | — | ws↔srv, un salto |
| `leg2` | — | — | srv↔VM, la workstation non è nel percorso |
| `ctrl` | — | — | ws→srv→VM, **senza** bore |
| `bore-relay` | **466,69** | **474,32** | ws→srv→VM, VPN bore sul relay |

Il relay sta al **50 %** del nudo in download e al **63 %** in upload.

### 28.1 Quello che la fase si è RIFIUTATA di dire

Tutti i bracci python sono vuoti — bloccati dal doppio meccanismo del §25.6 (i
security group per 5311/5312/5313, il DNAT Docker per la fascia 9000). Il
braccio mancante che conta è `ctrl`: un **doppio transito attraverso lo stesso
server senza bore**. Senza di lui la separazione fra *costo del deployment* e
*costo del codice* non è misurata, e la fase lo stampa a lettere intere:

> THE CONTROL DID NOT PRODUCE A NUMBER. Without a non-bore double transit
> through the same server, the split between deployment and code is NOT
> measured here and must not be asserted.

Quindi **"il relay di bore costa il 50 %"** non è una frase su bore: è una frase
su bore *più* un doppio transito che nessuno ha misurato separatamente. La fase
pubblica il rapporto e nega la conclusione, che è il comportamento giusto — e
vale la pena notare che ha chiuso `rc=0` con metà delle celle vuote. Il marker
c'è, la fase è "riuscita", e solo il blocco di lettura dice che non lo è del
tutto. È la ragione per cui la patch del cancello di raggiungibilità
(applicata subito dopo la fine di questa esecuzione) stampa un blocco
**PER-LEG ATTRIBUTION UNAVAILABLE** invece di sei celle a `0.00`.

**Non è stata rieseguita con il cancello.** Il cancello cambia il *messaggio*,
non produce i numeri mancanti: il blocco è infrastrutturale e si rimuove nella
console AWS, non in uno script. Una riesecuzione costerebbe banda reale per la
stessa informazione. Il costo del non-taglio, come ogni altro in §26.2, è
dichiarato invece che nascosto.

### 28.2 Il relay costa UN CORE, e questo sì che è misurato

| fase | CPU host | bore |
|---|---|---|
| bracci di controllo (idle) | 0,3–0,5 % | 0,26–0,27 core-s (**1 %** di un core) |
| `bore-relay` | 48,6–64,4 % | 19,0–28,0 core-s (**87–127 %** di un core) |

Un endpoint VPN bore sul relay consuma **circa un core intero** per muovere
~460 Mbit/s su questa workstation. È coerente con la campagna vhost (il relay
misurato in CPU s per GiB, non in MB/s) e sposta la domanda: il tetto del relay
qui non è la linea — la linea nuda dà 933 — ed è compatibile con un limite di
CPU per flusso. Il braccio `ctrl` mancante è anche quello che avrebbe detto
quanto di quel core è AEAD e quanto è doppio transito.

### 28.3 Un campione da non pubblicare da solo: l'upload del relay ha una rampa

I campioni grezzi dell'upload sul relay, in ordine di ripetizione:

```
relay|up    182.42   474.32   550.84
relay|down  459.25   466.69   470.27
```

Il download è stretto (±1,2 %). L'upload **cresce in modo monotono** e la prima
ripetizione vale **un terzo** dell'ultima. Non è rumore: è una rampa, e la
mediana (474,32) la attraversa senza mostrarla.

Regola che ne discende, e che costa poco applicare: **una fase a una sola
ripetizione avrebbe pubblicato 182**, cioè il 24 % del nudo invece del 63 %, e
lo avrebbe fatto con la stessa faccia. È V-19 in una forma nuova — lì era il
*trasferimento* più corto della rampa, qui è la *campagna* — e la difesa è la
stessa: stampare sempre i campioni grezzi accanto alla mediana. Senza quel
blocco questa rampa sarebbe invisibile.

Resta aperto **perché** l'upload abbia una rampa e il download no, su un
percorso che è simmetrico per costruzione. Candidati non ancora separati: il
riscaldamento della finestra QUIC sul primo transito, il bucket di allowance
dell'istanza (i contatori ENA sono però **identici prima e dopo ogni braccio**,
il che lo scagiona per questi bracci), e il ritardo con cui il relay raggiunge
il regime di CPU misurato in §28.2.

---

## 29. Tre fasi di attribuzione, tre risposte — e una è un tunable nuovo

### 29.1 `vpn_overhead`: il deficit residuo del diretto **sono le intestazioni**

| braccio | wire/consegnato | overhead per frame |
|---|---|---|
| nudo | 1,05135 | 73,72 B |
| diretto | 1,10391 | 127,58 B |

Il nudo mette sul filo il **5,14 %** in più di quello che consegna; il diretto il
**10,39 %**. Il solo incapsulamento predice quindi che il tunnel arrivi al
**95,2 %** del nudo — e il rapporto diretto/nudo misurato da `vpn_ab` e
`vpn_txqueue` è **~0,95**.

I due numeri coincidono. Il tunnel aggiunge **54 B per frame** (128 contro 74),
che è esattamente QUIC + AEAD + l'header UDP/IP esterno. **Non c'è alcun tunable
che possa toglierli**: il residuo non è una perdita da recuperare, è aritmetica.
Questa domanda si chiude qui e non va riaperta senza cambiare l'incapsulamento.

### 29.2 `vpn_rtt_load`: il diretto conserva la latenza sotto carico, il relay no

Mediane, ms:

| braccio | a riposo | sotto carico **up** | sotto carico **down** | up Mbit/s | down Mbit/s |
|---|---|---|---|---|---|
| nudo | n/d | n/d | n/d | 746,8 | 935,6 |
| relay | 20,47 | **58,26** | 30,26 | 413,7 | 495,1 |
| diretto | 19,84 | **19,93** | 38,71 | 489,1 | 832,6 |

**In upload il diretto paga 0,09 ms e il relay ne paga 37,8.** È la
contropressione dell'uplink 1:1 (`send_batch_wait`, BW-F3) che fa quello per cui
esiste: su buffer di datagrammi pieno *attende* invece di lasciare che quinn
scarti il più vecchio, e la pausa risale fino alla coda TUN. Il relay, con la
sua coda a canale limitato, accumula una coda stazionaria.

**In download il verso si rovescia, e non è una contraddizione**: il diretto
consegna il **68 % in più** (832,6 contro 495,1) e paga 8,4 ms in più. Muovere
più byte su un collo di bottiglia costa coda; la domanda giusta è quanta se ne
paga per byte, e lì il diretto vince comunque.

**Quello che questa fase NON può dire**, e lo dice: il braccio nudo legge `nan`
in tutte e tre le ripetizioni. Il suo strumento è un endpoint `attrib_net.py`
sulla porta 5311 della VM — la stessa porta che i security group bloccano
(§25.6). La fase è progettata attorno a **un solo strumento per entrambi i
bracci** proprio per non lasciare che sia lo strumento a produrre il risultato
(ICMP verso la VM è scartato, quindi il nudo userebbe un handshake TCP e il
tunnel un ping: due cose diverse). Il blocco toglie il riferimento a entrambi.
Quindi i 20,47 ms del relay a riposo non sono confrontabili con un nudo
misurato **nella stessa ripetizione**; il nudo di `ws_rtt` (~19 ms) è un
confronto fra fasi, più debole, ed è etichettato come tale.

### 29.3 `vpn_relay_mtu`: la scala **non è piatta** — e questo è un tunable nuovo

Quattro campagne di seguito avevano prodotto scale piatte, e il testo della fase
si aspettava la quinta. Non è andata così:

| MTU | down | up | down/nudo | up/nudo | byte/pacchetto |
|---|---|---|---|---|---|
| **1350** (default) | 574,35 | 495,54 | 0,618 | 0,666 | 2 353 |
| 1500 | 586,40 | 555,00 | 0,631 | 0,746 | 2 585 |
| 4000 | 701,21 | 620,70 | 0,755 | 0,835 | 6 387 |
| **8000** | 764,65 | 685,00 | **0,823** | **0,921** | 10 729 |
| nudo | 929,31 | 743,60 | — | — | — |

Da 1350 a 8000: **+33 % in download, +38 % in upload**, e la frazione del nudo
passa da 0,62 a 0,82 giù e da 0,67 a 0,92 su.

**Non è un artefatto di riscaldamento, ed è stato verificato invece che
assunto.** La fase **inverte l'ordine dei rung a ripetizioni alterne**, proprio
perché un riscaldamento o un'allowance spesa non cadano sempre sullo stesso
rung. E gli intervalli non si sovrappongono: 1350 non supera mai 598 in nessuna
delle tre ripetizioni, 8000 non scende mai sotto 731. La variabile è la MTU.

**Perché il relay può farlo e il diretto no.** Sul relay i pacchetti interni
viaggiano dentro una **sequenza di byte TCP** (substream yamux): una MTU della
TUN più grande del filo è legittima, perché è il TCP a segmentare, e il
guadagno è meno lavoro per pacchetto (una sigillatura AEAD, un frame, un
passaggio di driver ogni 8000 byte invece che ogni 1350). Sul percorso diretto
i pacchetti interni viaggiano in **datagrammi QUIC**, che non possono superare
la MTU del percorso: lì 1350 non è conservatorismo, è il vincolo.

**Quindi la proposta non è "alzare il default".** È: *tenere una MTU grande
finché il link è sul relay, e restringerla prima di passare al diretto* — che è
esattamente quello che il monitor PMTU già fa, misurato in **~1 ms** (passaggio
a .983622, MTU corretta a .984358). Resta da decidere e da gated; è registrato
fra i tunable aperti e **non** è stato cambiato in questa finestra, perché un
default si spedisce a ogni percorso e questa misura è su un percorso solo.

Due avvertenze sui dati grezzi, stampati com'è regola: a MTU 8000 un campione di
upload legge **227,26** contro 699 e 685 delle altre due ripetizioni, e a MTU
1350 il primo campione di upload (593,83) è più alto della mediana. La mediana
regge entrambi; una fase a una ripetizione sola no — di nuovo §28.3.

---

## 30. `vpn_direct_deficit` rieseguito: il diretto vince **anche sulla CPU**, ai due estremi

Questa è la fase per cui P7 doveva precedere la finestra di build: il confronto
vale solo **contro lo stesso binario** della prima esecuzione, e P5 lo
sostituisce. Ha chiuso in 318 s, esattamente come la prima volta.

### 30.1 Perché andava rieseguita: uno zero che *diceva* di essere un fallimento

La prima esecuzione stampava, per ogni braccio:

```
(note: VM bore pid not found -- VM cpu column is not measured this arm)
relay   goodput   632.48 Mbit/s   tun_mtu 1350
        cpu  ws  5.52 s ( 3.75 s/GiB)   vm  0.00 s ( 0.00 s/GiB)
```

La nota è onesta e la colonna resta a `0.00`. È la versione *buona* della
classe che questa campagna paga da mesi — dichiara il guasto — ma la tabella
resta leggibile come «sulla VM il tunnel costa zero», che è la conclusione
opposta a quella vera. **Una nota accanto a uno zero non è un rifiuto di
pubblicare**: il rimedio resta stampare `n/d` e non un numero.

### 30.2 Il risultato, ora che lo strumento misura

Upload (workstation → VM), la direzione dove sta il deficit, 2 ripetizioni:

| braccio | Mbit/s | % del nudo | CPU ws (s/GiB) | CPU VM (s/GiB) | MTU TUN |
|---|---|---|---|---|---|
| nudo | 751,2 / 750,2 | — | — | — | — |
| relay | 603,4 / 532,1 | 80 % / 71 % | 3,71 / 4,04 | 8,56 / 8,83 | 1350 |
| **diretto** | 678,9 / 712,6 | **90 % / 95 %** | **3,61 / 3,44** | **8,38 / 8,44** | 1414 |

**Il diretto consegna più byte e ne consegna ognuno con meno CPU, su entrambi
gli estremi.** Non è il compromesso che ci si aspetta da un percorso che
aggiunge QUIC e AEAD: il relay paga il doppio transito e la sigillatura del
server, e quel costo supera l'incapsulamento del diretto. Coerente con quanto
misurato nella campagna secret (il diretto fattura gli estremi e costa meno in
totale).

`lost 0 (0,00 %)` e `cong_events 0` in tutte le ripetizioni: la vittoria non è
dovuta a un percorso migliore, è a parità di percorso.

### 30.3 E una parte del deficit del relay è la MTU, adesso quantificata

Il braccio relay gira a **MTU 1350 fissa**; il diretto si auto-regola a **1414**.
§29.3 ha appena misurato che il relay a 8000 arriva allo **0,92 del nudo in
upload** contro lo 0,67 a 1350 — e qui il relay a 1350 legge 0,71–0,80.

I due numeri si tengono: **una parte del «deficit del relay» che questa fase
attribuisce al relay è in realtà la sua MTU**, ed è la stessa parte che §29.3
propone di recuperare tenendo una MTU grande finché il link è sul relay. Le due
fasi vanno lette insieme; nessuna delle due, da sola, lo direbbe.

### 30.4 Un'anomalia registrata e non conclusa

Nella prima esecuzione la ripetizione 2 riporta `cwnd 1162610453` — **1,16 GB**
di finestra di congestione. Nella riesecuzione le stesse celle leggono 17,4 MB e
9,3 MB. Su un percorso con `lost 0,00 %` e `cong_events 0` una finestra del
genere non ha un significato fisico plausibile a 750 Mbit/s e 37 ms
(≈ 3,5 MB di prodotto banda-ritardo). È registrata come **anomalia dello
strumento, non come misura**: o è un campione di quinn letto in un momento
degenere, o è un difetto di lettura del contatore. Non entra in nessuna
conclusione e va guardata se ricompare.

---

## 31. Il cancello sul campo di M-1 era **cieco**, e la prova era nel suo stesso output

`vpn_ctrl_leak.sh` è il gate sul percorso reale citato da `CLAUDE.md` e da
`docs/vpn/VPN_CTRL_CONN_LEAK.md` per M-1 — una connessione TCP di controllo
persa a **entrambi** gli estremi a ogni riconnessione del connector VPN. Ha
chiuso `rc=1` in 206 s, e il verdetto era questo:

```
FAIL -- 0 established control connections where 1 is correct.
        -1 leaked, unreaped after 120s.
```

**Un leak di meno uno.** È l'unica ragione per cui qualcuno ha guardato.

### 31.1 La causa: una porta scritta a mano che in questo deployment non esiste

La sonda contava le connessioni `ESTAB` verso `:7835`, il default di bore,
scritto come costante. Ma `BORE_TO` qui è `https://<host>` **senza porta**: il
client si collega sul **443**, attraverso il fronte TLS. Il filtro non ha mai
trovato nulla, `ctrl_conns` ha restituito `0` in ogni campione, e il blocco di
verdetto ha fatto l'aritmetica su quello zero.

**Un cancello che non vede la cosa che sorveglia non fallisce rumorosamente:
fallisce con un numero.** Ed era un numero della forma peggiore — uno zero che
significa "lo strumento ha mancato il bersaglio", la classe che questa campagna
paga dalla prima settimana, stavolta dentro il gate che deve dimostrare un
difetto di prodotto.

Correzione, in due metà:

1. **La porta si deriva** da `BORE_TO`, che è l'indirizzo che il client riceve
   davvero (`https` → 443, `http` → 80, `host:porta` → quella porta, altrimenti
   7835), con `CTRL_PORT` che resta un override. Auto-testata su sette forme,
   IPv6 fra parentesi quadre compreso.
2. **Zero non è un leak di −1.** Un tunnel che ha portato traffico attraverso
   quattro riconnessioni possiede una connessione di controllo per costruzione,
   quindi contarne nessuna significa che è **la sonda** ad averla mancata. La
   fase ora stampa `INSTRUMENT FAILURE`, dice quale porta ha cercato e da dove
   l'ha derivata, e **si rifiuta di produrre un verdetto sul prodotto**.

### 31.2 E intanto la prova del difetto era già lì, sotto il verdetto sbagliato

Il dump dei descrittori che la fase stampa quando fallisce elenca, per il pid
del connector, **cinque** connessioni TCP `ESTAB` verso la porta 443 del server,
tutte da porte sorgenti locali diverse. La fase era configurata con `CYCLES=4`.

**Cinque = una corretta + quattro perse, una per riconnessione.** È esattamente
la firma di M-1 descritta in `CLAUDE.md` (misurata 1→2→3→4, mai riassorbite in
120 s) e la sonda l'aveva sotto gli occhi mentre pubblicava `0`.

Quindi questa esecuzione **conferma il difetto** — con l'evidenza grezza, non
con il contatore — e non conferma nulla sulla correzione: il binario in uso è
del 12/09 18:05 e `src/mux.rs` con `Liveness`/`TrackedStream` è del 13/09 00:23.
Il gate ha girato contro codice che non contiene la correzione.

### 31.3 L'esperimento che manca, e quando si fa

La fase non ha lasciato marker (`rc=1`), quindi si ripete da sola. Ma il
confronto che serve è **prima/dopo sullo stesso strumento corretto**:

| quando | binario | cosa risponde |
|---|---|---|
| fine P7, **prima** della build | quello attuale (senza la correzione) | il gate corretto vede davvero 5? cioè: il contatore ora funziona |
| dopo P5 | ricostruito con `src/mux.rs` corretto | ne vede **1**? cioè: la correzione regge sul percorso reale |

Costa ~7 minuti in tutto e nessuna banda apprezzabile (conta connessioni, non
byte). Senza il primo passo la correzione verrebbe verificata con uno strumento
che nessuno ha mai visto funzionare — e un gate che passa alla prima esecuzione
valida non ha red-check.

Il meccanismo che impedisce al marker di confondere i due è in `p5_build_gate.sh`:
**un marker vale solo per il binario che lo ha prodotto**, e quando lo sha del
binario cambia i marker delle fasi il cui verdetto riguarda il *prodotto*
vengono invalidati (conservati come `_done.<fase>.pre-<sha>`), mentre quelli
delle fasi di *misura* restano, perché sono il termine di paragone.

---

## 32. Il relay **crolla** con la concorrenza, il diretto no — riprodotto due volte

`vpn_profile` è girata due volte, a ore di distanza (261 s e 322 s), e le due
esecuzioni si sovrappongono **anche nelle anomalie**. Non è rumore.

### 32.1 I numeri

Aggregato, Mbit/s, al variare del numero di flussi TCP interni:

| percorso | direzione | 1 flusso | 2 | 4 | 8 |
|---|---|---|---|---|---|
| relay (1ª) | **up** | 550,5 | 318,0 | 249,7 | **206,0** |
| relay (2ª) | **up** | 575,4 | 595,8 | 372,9 | **204,4** |
| relay (1ª) | down | 606,8 | 775,7 | 511,2 | 439,2 |
| relay (2ª) | down | 594,7 | 775,9 | 531,2 | 466,7 |
| **diretto** (1ª) | **up** | 694,7 | 692,1 | 694,5 | **695,7** |
| **diretto** (2ª) | **up** | 710,3 | 698,7 | 700,7 | **702,5** |
| diretto (1ª) | down | 821,0 | 536,9 | 872,3 | 874,1 |
| diretto (2ª) | down | 834,4 | 516,5 | 871,5 | 873,6 |

**In upload, a otto flussi il relay consegna il ~36 % di quello che consegna a
uno** (204 contro 575). Il diretto consegna **lo stesso numero a ogni conteggio
di flussi**, entro l'1 %: 695, 692, 695, 696 nella prima esecuzione; 710, 699,
701, 703 nella seconda. Quattro celle indipendenti che coincidono a due cifre su
due esecuzioni separate non sono un caso.

Il relay in download disegna la stessa forma con un picco: sale a **775,7 / 775,9**
a due flussi — le due esecuzioni concordano sulla **terza cifra** — e poi cade.

### 32.2 Il meccanismo, dichiarato come ipotesi e non come conclusione

Sta nel progetto di questo repository, non in una congettura esterna:

* Il **relay** multiplexa ogni pacchetto interno in **una sequenza di byte TCP
  ordinata e affidabile** (coppie di substream yamux su un carrier). Otto flussi
  TCP interni condividono quindi **una sola finestra di congestione** e si
  bloccano a vicenda in testa alla coda: la perdita o il riordino di un flusso
  ferma anche gli altri, che non ne sanno nulla.
* Il **diretto** li porta in **datagrammi QUIC**, inaffidabili e non ordinati per
  progetto: la sfortuna di un flusso interno è invisibile agli altri.

È la stessa proprietà che V-15 descrive dal lato opposto — sul diretto un
datagramma perso *è* un segmento interno perso, e questo costa quando il
percorso perde — qui, su un percorso a perdita zero, si legge come il suo
vantaggio.

Coerente anche il resto del profilo: il relay misura `udp @300M` con lo **0,23 %**
di perdita contro lo **0 %** del diretto, e paga più bufferbloat (×1,76–1,89,
+18 ms contro ×1,51–1,61, +10–11 ms).

### 32.3 Il rimedio esiste già nel prodotto, e non era mai stato misurato qui

Se il meccanismo è quello, i **`--carriers` sul relay** sono la risposta che bore
già spedisce: il relay distribuisce **per datagramma** su N coppie di substream
(DEC-7), quindi N finestre di congestione indipendenti invece di una.

Attenzione a non trasportare qui una conclusione che vale altrove: **V-13 ha
falsificato i carrier sul percorso DIRETTO** (4 carrier consegnano meno, a 1,7×
la CPU) — ma quel percorso è *flow-pinned* per costruzione (BW-F2: `flow_carrier`
lega un flusso interno a un carrier) e quella misura aveva un solo flusso
saturante. Nessuna delle due cose vale per il relay sotto otto flussi.

`vpn_carriers.sh` (in `rerun_vpn_deep.sh`, non ancora eseguita) ha già la scala
`relay | 1,2,4 carrier | 1,4 flussi`. **Si fermava però a quattro flussi, dove
l'effetto è visibile a metà** (373 di 575); a otto è inequivocabile (204). Sono
state aggiunte le celle a **otto flussi** su entrambi i percorsi — cinque celle,
3 ripetizioni da 10 s, **tutte in upload, quindi a costo AWS zero**.

**Predizione da falsificare, scritta prima della misura:** se il collo di
bottiglia è la finestra unica condivisa, il relay a 8 flussi deve migliorare
sensibilmente da 1 a 2 a 4 carrier; se invece è la CPU della sigillatura o il
doppio transito, i carrier non muoveranno nulla e l'ipotesi va buttata.

### 32.4 Un'anomalia riprodotta e non spiegata

Il diretto in **download** ha un avvallamento a **due flussi**: 536,9 nella prima
esecuzione e 516,5 nella seconda, contro 821/834 a un flusso e 872/874 a quattro
e otto. Si riproduce, quindi non è rumore, e **non ha una spiegazione**. È
registrata come domanda aperta e non entra in nessuna conclusione: una forma che
scende e poi risale sopra il punto di partenza non è compatibile con una
saturazione semplice.

---

## 33. M-1 misurato **dal vivo**, e la fase che doveva misurarlo è la sola che non ci riusciva

`vpn_stability` rieseguita con 5 cicli (802 s). Ogni ciclo: stabilisci → sali sul
diretto → tre round di carico → uccidi il listener → riconnetti.

### 33.1 Un descrittore per riconnessione, sotto gli occhi

| ciclo | rss_kib | threads | **fds** | path |
|---|---|---|---|---|
| 0 (start) | 24 040 | 19 | **12** | relay |
| 1 | 42 236 | 18 | **12** | direct |
| 2 | 46 620 | 18 | **13** | direct |
| 3 | 58 912 | 18 | **14** | direct |
| 4 | 55 204 | 18 | **15** | direct |
| 5 | 58 192 | 18 | **16** | direct |

`bridge switches: 5`, `relay fallbacks: 0`. I thread restano **18**. I
descrittori salgono di **uno per riconnessione** e non tornano mai indietro.

La fase dichiara il proprio criterio nella terza riga della sua intestazione,
scritta prima di questa esecuzione: *«a leak is RSS rising TOGETHER with threads
or fds; RSS alone that plateaus is the allocator»*. Qui la RSS sale **e** i
descrittori salgono. È una perdita secondo la regola che la fase stessa si era
data, non secondo una lettura fatta a posteriori.

### 33.2 Tre strumenti indipendenti, e il gate dedicato è quello che ha fallito

M-1 è ora sostenuto da tre misure che non condividono lo strumento:

| strumento | cosa ha visto |
|---|---|
| `ss`, conteggio manuale (campagna precedente) | 1 → 2 → 3 → 4 `ESTABLISHED`, mai riassorbite in 120 s |
| dump dei descrittori di `vpn_ctrl_leak` (§31.2) | **5** connessioni `ESTAB` con `CYCLES=4` |
| colonna `fds` di `vpn_stability_r2` | **+1 per riconnessione**, 12 → 16 su 5 cicli |

L'unico che **non** ha visto nulla è `vpn_ctrl_leak` — cioè il gate scritto
apposta per questo — perché contava su una porta sbagliata (§31.1). Vale come
regola: **un difetto confermato da tre strumenti diversi e negato dal proprio
gate dedicato è un difetto del gate**, e va trattato in quell'ordine.

### 33.3 V-15 **non** si è riprodotto, e questo ne definisce la portata

La campagna precedente aveva registrato, al ciclo 3, 95–160 Mbit/s contro 712
dei cicli 1–2, con `lost_pkts_d` 2–8 per 5 s dove gli altri cicli leggevano zero
— la base empirica di V-15 (sul percorso diretto un datagramma perso *è* un
segmento TCP interno perso, e Mathis vincola il flusso a `MSS / (RTT · √p)`).

In questa esecuzione, **cinque cicli su cinque**: `lost=0`, `cong=0`,
`tx_dropped=0`, `tx_errors=0`, contatori di allowance ENA a zero prima e dopo
ogni ciclo. Il tunnel legge 717,4 / 717,1 / 717,3 / 713,5 Mbit/s, stabile entro
lo 0,6 %.

Quindi V-15 resta valido come **aritmetica** e come regola di lettura — una fase
sul diretto deve stampare la perdita del carrier accanto alla velocità, perché
un braccio lento con `lost=0` e uno con `lost>0` sono diagnosi opposte — ma
l'episodio che lo ha fatto scoprire era un **evento del percorso**, non una
proprietà del prodotto. Cinque cicli puliti lo delimitano.

### 33.4 Un rapporto sopra 1 accusa il controllo, non il braccio

Il ciclo 3 stampa `tunnel/bare 1.073`: tunnel 717,12 contro un nudo di **668,55**.
Gli altri quattro cicli leggono il nudo a 750,77 / 751,64 / 750,81 — entro lo
0,2 % l'uno dall'altro.

Un tunnel non può battere il proprio controllo nudo sullo stesso percorso: ci
aggiunge incapsulamento e sigillatura, e §29.1 ha appena misurato quanto (54 B
per frame, 95,2 % predetto). **Un rapporto sopra 1 non è un risultato del
braccio: è un campione di controllo cattivo**, e va letto come tale invece che
celebrato. Il ciclo 3 va quindi escluso dalla media dei rapporti, non incluso
come il migliore.

Con i quattro cicli buoni il diretto legge **0,950–0,956 del nudo**, che è la
stessa cifra a cui §29.1 arriva per via aritmetica dal solo incapsulamento.

---

## 34. L'asimmetria download/upload era della **radio**, e la statistica che la fase stampava era quella sbagliata

**Fase:** `pub/ws_asym.sh` → `out/eth/pub_ws_asym_r2.out` (424 s, `rc=0`).
**Domanda originale della fase:** la workstation tirava a ~27 MB/s e spingeva a
~71 MB/s attraverso **lo stesso tunnel**, negli stessi minuti. È la linea a
essere asimmetrica in quel verso, o è l'istanza che modella la direzione di
**uscita** del server (il download è server-OUTBOUND, e l'allowance in uscita è
il confondente attorno a cui è costruita tutta questa campagna)?

### 34.1 I sei bracci

384 MiB per braccio, 4 connessioni, tunnel relay sulla porta 9031, tre
ripetizioni appaiate con l'ordine dei bracci alternato.

| rep | ordine | download | upload | Δ allowance (server) |
|---|---|---|---|---|
| 1 | down → up | 90,87 MB/s (762 Mbit/s) | 85,70 MB/s (719) | `in=0 out=0` su entrambi |
| 2 | up → down | 95,35 MB/s (800) | 76,27 MB/s (640) | `in=0 out=0` su entrambi |
| 3 | down → up | 73,74 MB/s (619) | 66,07 MB/s (554) | **`in=27`** sul download, `out=0` sull'upload |

### 34.2 La risposta: era la radio, e l'allowance in uscita è **assolta**

In WiFi il rapporto download/upload di questo stesso tunnel era **0,38**
(27 contro 71 MB/s). Cablato è **1,116**: non si è ridotto, si è **invertito**.
Un'istanza che modella l'uscita non cambia verso perché il client cambia
scheda di rete; una radio sì. L'asimmetria che la fase era stata scritta per
attribuire apparteneva quindi all'accesso, non al prodotto e non all'istanza.

L'assoluzione è più forte di un rapporto, perché c'è un contatore: su **sei
bracci**, 2,25 GiB di egress inclusi i tre download — che *sono* la direzione di
uscita del server — `bw_out_allowance_exceeded` è rimasto **zero ovunque**.
La regola della campagna («uno zero attraverso un braccio assolve il secchiello
per *quel* braccio») dice esattamente questo.

L'unico delta non nullo dell'intera campagna è `bw_in_exceeded=27`, sul download
della rep 3. **Non è una contraddizione**, ed è il punto in cui la legenda della
fase era scritta male: il tunnel è un **relay**, quindi ogni byte attraversa il
server **due volte** — in ingresso dalla VM e in uscita verso la workstation per
un download, al contrario per un upload. Un delta di allowance nomina la
**tratta**, non la direzione del braccio: quei 27 incriminano la tratta
VM → server di quel download. La legenda ora lo dice (§34.4).

### 34.3 Contro la linea, il download paga **più** dell'upload

Il rapporto grezzo non significa niente da solo: la linea è asimmetrica anche
lei. V-9 ha qualificato questo accesso a ~925 Mbit/s in download e ~740 in
upload, cioè **1,25**. Il tunnel misura **1,116**, sotto la linea:

| rep | download / linea | upload / linea |
|---|---|---|
| 1 | 82,4 % | 97,2 % |
| 2 | 86,5 % | 86,5 % |
| 3 | 66,9 % | 74,9 % |

In tutte e tre le ripetizioni l'upload rende quanto il download o **di più**.
Il tunnel non crea l'asimmetria della linea: la **comprime**, perché la
direzione di download ne perde una frazione maggiore. Questo è coerente con
§30 (le intestazioni sono il deficit residuo del percorso diretto) e con il
fatto che il download attraversa due tratte del relay con l'ingresso della VM
in testa.

**Vincolo su questa lettura:** le percentuali sopra dividono per una linea
qualificata *in un'altra ora*. `asym_qualify.sh` è nella lista di riesecuzione
di questa finestra proprio per questo, e finché non gira nella stessa finestra
il rapporto appaiato **1,116** è il numero difendibile, non le tre percentuali.

### 34.4 Tre difetti della fase, tutti miei

1. **Stampava il rapporto delle mediane, non la mediana dei rapporti appaiati.**
   L'appaiamento è l'unica ragione per cui i due bracci girano dentro la stessa
   ripetizione — la deriva si annulla solo se è comune a entrambi — e poi il
   riepilogo lo buttava via. Misurato: `mediana(download)/mediana(upload)` ha
   diviso il download della **rep 1** per l'upload della **rep 2**, due bracci a
   cinque minuti di distanza, e ha stampato **1,191** dove le ripetizioni prese
   intere danno 1,060 / 1,250 / 1,116, **mediana 1,116**. Il 7 % di differenza
   non cambia la conclusione qui; cambia il fatto che il numero pubblicato non
   proveniva da nessun esperimento eseguito.
   Corretto: si stampano i rapporti appaiati, la loro mediana come titolo, e il
   rapporto delle mediane resta *etichettato per quello che è* (continuità con
   le esecuzioni a campione singolo).
   L'appaiamento è tenuto **per ripetizione** e non per indice negli array dei
   campioni: `keep` scarta un braccio fallito, e un indice condiviso farebbe
   scivolare in silenzio ogni download successivo contro l'upload di un'altra
   ripetizione — cioè lo stesso difetto, un livello più in basso.
2. **La legenda nominava una sola tratta di un percorso a due tratte**, e così
   faceva leggere un delta `bw_in` su un braccio di download come un errore
   dello strumento (§34.2).
3. **Nessun controllo nudo dentro la ripetizione.** Resta così di proposito, per
   la stessa ragione per cui `ws_conns.sh` non duplica il proprio: una
   misura, un proprietario. Il riferimento di questa fase è `asym_qualify.sh`,
   e la fase ora lo **dice**, con il vincolo che vanno eseguiti nella stessa
   finestra.

**Red-check** (`/tmp/.../asym_stat_test.sh`, il codice reale con il solo `arm`
sostituito da una coda di valori): con i sei tassi realmente registrati
riproduce 1,060 / 1,250 / 1,116, mediana **1,116**, e il rapporto delle mediane
**1,191** — cioè i numeri pubblicati, più quello che mancava. Con l'upload della
rep 2 dichiarato FALLITO la rep 3 continua a leggere 1,116 (non 95,35/66,07 =
1,444, che è quello che farebbe l'appaiamento per indice). Con tutti e sei i
bracci falliti non inventa nessun rapporto.

### 34.5 Il decadimento fra ripetizioni, e cosa **non** lo spiega

In ordine di orologio i sei bracci leggono 90,87 → 85,70 → 76,27 → **95,35** →
73,74 → 66,07. Non è una discesa monotona: il quarto braccio è il più veloce di
tutti. Per direzione, però, **l'upload scende in modo monotono** (85,70 → 76,27
→ 66,07, −23 % in ~15 minuti) mentre il download no.

Il solo braccio con un meccanismo lato istanza è il **download** della rep 3
(`bw_in=27`) — e l'upload della stessa ripetizione, che è il più lento dei tre,
ha delta **zero**. Nessun meccanismo unico copre entrambi. Quindi:

- la rep 3 in download è una misura di **budget**, non di tunnel: si legge, non
  si media;
- il calo dell'upload resta **non attribuito**, ed è precisamente ciò che una
  riesecuzione di `asym_qualify` in questa finestra può separare (linea contro
  tunnel).

**Un risultato che l'alternanza ha prodotto per contrasto:** l'ordine dei bracci
è stato alternato per annullare il vantaggio del braccio che gira per primo su
un secchiello appena ricaricato. I primi bracci leggono 90,87 / 76,27 / 73,74 e
i secondi 85,70 / 95,35 / 66,07 — il più veloce e il più lento dell'intera fase
stanno **entrambi** in seconda posizione. A 384 MiB con 75 s di raffreddamento
quel confondente non è visibile: il raffreddamento sta facendo il suo lavoro.

---

## 35. A connessione singola relay e diretto sono **pari**, e a 96 MiB la domanda non era misurabile

**Fase:** `pub/ws_dl1.sh` → `out/eth/pub_ws_dl1_r2.out` (949 s, `rc=0`), contro
`out/eth/pub_ws_dl1.out` della prima esecuzione. Stesso codice, stessa linea,
stesso giorno: **cambia solo `XFER_MB`**, da 96 a 384 MiB. Sei coppie, ordine
dei bracci alternato, UNA connessione.

| | relay (mediana) | dispersione *dentro* il braccio relay | quic (mediana) | rapporti |
|---|---|---|---|---|
| 96 MiB | 90,91 MB/s | 67,14 → 94,32 = **40,5 %** | 88,94 | 0,745 · 0,984 · 0,991 · **1,323** · 0,979 · 1,005 |
| 384 MiB | **106,12 MB/s** | 105,19 → 107,11 = **1,8 %** | 105,90 | 0,805 · 1,002 · 1,002 · 1,012 · 0,997 · 0,994 |

### 35.1 V-19 non è una teoria: è questa tabella

Il braccio relay è **lo stesso esperimento** in entrambe le righe. A 96 MiB
varia del **40 %** fra una coppia e l'altra; a 384 MiB dell'**1,8 %**. Non è
rumore che si media via, è il **ramp**: 96 MiB su questa linea durano 1,1 s, e
gran parte di quel tempo è slow start, quindi ogni coppia misurava un punto
diverso della salita. Con 3,6 s per braccio la salita è ammortizzata e lo
strumento smette di rumoreggiare.

La conseguenza sulla **risposta**, non solo sulla dispersione: a 96 MiB i
rapporti andavano da 0,745 a 1,323 su bracci che differiscono per una sola
variabile — cioè la domanda «relay o diretto?» era **non misurabile**, e la
coppia 4 (1,323) sarebbe stata pubblicabile come «il diretto vince del 32 %».
A 384 MiB, escludendo la prima coppia (§35.3), i sei rapporti stanno fra
**0,994 e 1,012**.

### 35.2 La risposta: pari entro l'1 %

**Mediana quic/relay 0,9995.** Su una connessione sola, in download, il relay
TCP e il QUIC diretto del tunnel pubblico consegnano la stessa banda. E la
consegnano a **890 Mbit/s su UNA connessione**, cioè ~96 % della linea
qualificata da V-9 (~925 Mbit/s in download) — attraverso due tratte
(workstation ← server ← VM), non una.

Questo chiude, per questo percorso e questa forma di carico, la domanda «il
percorso diretto conviene in banda?»: **no, e non serve** — non perde. Dove i
due trasporti si separano davvero è il **costo** (CPU per GiB consegnato, §30) e
la **concorrenza** (§32), non la velocità di un flusso.

### 35.3 Un'osservazione riproducibile: il **primo** trasferimento QUIC paga ~19 %

La prima coppia è bassa sul braccio quic in **entrambe** le esecuzioni: 0,805 a
384 MiB e 0,745 a 96 MiB, cioè **18,6 %** e **21,1 %** sotto la mediana delle
coppie successive dello stesso braccio. Non è un effetto di posizione:
l'ordine si alterna, e il braccio quic gira per primo nelle coppie pari — dove
legge 105,7, pieno regime. È il **primo trasferimento QUIC della fase**, e
soltanto quello.

Ed è un effetto di **velocità**, non un costo fisso di setup: la penalità in
tempo cresce con la dimensione del trasferimento, 0,303 s a 96 MiB e **0,871 s**
a 384 MiB. Un handshake o un'apertura di stream costerebbero lo stesso in
entrambi (un RTT è 19 ms); un rate più basso mantenuto per tutto il
trasferimento costa in proporzione. Il tunnel viene registrato una volta sola
all'inizio della fase, quindi la prima connessione proxata è la prima a
esercitare davvero il percorso diretto.

**ATTRIBUITO NEL FRATTEMPO — vedi §40**, che registra i due tunnel in anticipo
e legge i contatori attorno a ogni trasferimento: il pool diretto vale già 1
prima che passi un byte e `direct_fallbacks` resta 0, quindi delle tre ipotesi
qui sotto sopravvive solo la prima. §40.3 discute perché là il costo misura
4,2 % e qui 18,6 %.

**Non era attribuito quando questa sezione è stata scritta, e la fase non poteva
attribuirlo.** Le ipotesi compatibili —
partenza a freddo del controllo di congestione lato server, la prima
connessione servita sul relay warm mentre il pool diretto si popola, un
`direct_fallbacks` che scatta una volta — si distinguono leggendo
`current_path`, `direct_stream_opens` e `direct_fallbacks` **attorno** al primo
trasferimento, che è precisamente ciò che l'admin API già espone e che nessuna
fase campiona in quel momento. Costa ~3 minuti e 768 MiB: è un buco di
copertura, registrato come tale.

Conta oltre la curiosità: un tunnel pubblico `--udp` di vita breve — quello che
serve una manciata di richieste e chiude — vive **interamente** dentro quella
prima connessione.

---

## 36. ~~Il relay pubblico crolla a 4 connessioni~~ — **RITIRATO, vedi §42**

> **Questa sezione è superata.** La sua conclusione centrale — che a quattro
> connessioni il relay pubblico crolla mentre il diretto no — poggiava su tre
> campioni per cella che non si sovrapponevano. §42 ha ripetuto quelle celle e
> ha misurato che l'**escursione della stessa cella ripetuta è del 38 %** a
> n≥4: con quella dispersione due terne separate sono un evento ordinario, non
> una prova, e alla replica le due celle si sovrappongono su quasi tutto
> l'intervallo. Resta valido quanto la sezione dice su **n=1 e n=2**, dove la
> cella si ripete entro il 6 %. Il testo è conservato integralmente perché il
> percorso che ha portato alla conclusione sbagliata è la cosa da non ripetere.


**Fasi:** `pub/ws_conns.sh` → `out/eth/pub_ws_conns_r2.out` (scala di
concorrenza, 460 MiB **per gradino** così che ogni gradino duri lo stesso, 3
ripetizioni con l'ordine dei gradini alternato) e `pub/ws_carr.sh` →
`out/eth/pub_ws_carr_r2.out` (`--carriers` 1 contro 4, 4 connessioni, 3 round
alternati). Linea qualificata nella stessa finestra: **922–927 Mbit/s** in
download.

| connessioni | relay | % linea | diretto (QUIC) | % linea | diretto/relay |
|---|---|---|---|---|---|
| 1 | 895 Mbit/s | 96,8 % | 878 | 94,9 % | 0,981 |
| 2 | **909** | **98,3 %** | 877 | 94,8 % | 0,964 |
| 4 | **532** | 57,5 % | **740** | 80,0 % | **1,393** |
| 8 | 620 | 67,0 % | 618 | 66,8 % | 0,997 |

### 36.1 La separazione a n=4 è netta, non statistica

I tre campioni di ogni cella non si sovrappongono: il relay a 4 connessioni
legge 67,17 / 63,38 / 58,29 MB/s e il diretto 85,03 / 88,27 / 92,45 — il
**massimo** del relay sta sotto il **minimo** del diretto. Rispetto al proprio
gradino n=2 il relay perde il **41,5 %**, il diretto il **15,6 %**.

Riprodotto indipendentemente dalla fase dei carrier, che a 4 connessioni su un
tunnel `--carriers 1` misura **68,52 MB/s** contro i 63,38 della scala: due
fasi diverse, tunnel diversi, stessa risposta.

### 36.1-bis Il meccanismo «ovvio» è FALSIFICATO dalla fase successiva — e anche questa correzione è poi caduta

> **Anche questa sottosezione è superata da §42.** Falsificava correttamente il
> meccanismo che §36 aveva dichiarato, e poi ne proponeva un altro («il limite
> sta sulla tratta server↔VM del percorso pubblico»). Era una spiegazione
> migliore per un fatto che alla replica non esiste. Conservata perché la
> falsificazione del meccanismo resta valida: un relay vhost a UN carrier regge
> davvero otto download a 933 Mbit/s, e quello è un dato, non un'ipotesi.

La prima stesura di questa sezione attribuiva il crollo all'invariante già
scritto nel progetto — *il relay è UN unico flusso TCP ordinato, quindi le
connessioni proxate condividono una finestra di congestione e si bloccano a
vicenda in testa alla coda*. È una spiegazione che si adatta ai numeri, ed è
**sbagliata come spiegazione generale**, perché `ws_tunnel` ha girato **la
stessa notte sulla stessa linea** e misura questo:

| tunnel vhost, `--carriers 1` | x1 | x4 | x8 |
|---|---|---|---|
| download aggregato | 930 Mbit/s | **933** | **933** |

Un relay a **un solo carrier** che regge otto download concorrenti a piena
linea. Se «un unico flusso TCP ordinato» bastasse a spiegare il crollo del
pubblico a n=4, questa riga non potrebbe esistere.

**Cosa resta stabilito.** A n=4 i due bracci del pubblico differiscono del 39 %
con tre campioni ciascuno che non si sovrappongono, e la differenza **non può**
essere dello strumento: dal lato workstation i due bracci sono identici — in
entrambi il client apre `n` socket TCP verso il server, e il QUIC vive sulla
tratta server↔VM, dove il client non arriva. Lo stesso client che consegna 740
Mbit/s nel braccio diretto ne consegna 532 nel relay, con lo stesso numero di
connessioni e gli stessi byte. Il limite sta quindi sulla tratta **server↔VM**
del percorso **pubblico**.

**Cosa NON è più stabilito, e diventa una domanda migliore di quella di
partenza:** perché il relay **pubblico** cade a n=4 e il relay **vhost** no.
Le due tratte sono, sulla carta, la stessa cosa — substream yamux su un'unica
connessione TCP verso il server. Le differenze restano tre: il percorso di
relay del server (pubblico contro vhost), il client sulla VM (`bore local`
contro `bore vhost`), e lo strumento ai due capi (`raw_*.py`, un processo
asyncio per tutte le connessioni, contro `curl`, un processo per connessione).

L'esperimento che le separa non è una nuova campagna: è la stessa scala di
concorrenza girata su un tunnel **vhost** con lo stesso strumento del pubblico,
oppure la scala pubblica girata con `curl`. Finché non è fatto, il numero da
citare è il **rapporto** fra i due bracci pubblici nella stessa ripetizione
(1,393 a n=4), non l'attribuzione a un meccanismo.

### 36.2 `--carriers 4` aiuta in tutti e tre i round — ma «il 46 %» non è misurato

> **Qualificato da §42.** Il disegno è appaiato per round, che è la forma giusta
> (trappola 22), e in tutti e tre i round c4 batte c1. Ma i rapporti sono 1,457,
> 1,463 e **1,085**, e l'escursione della stessa cella a questo gradino vale il
> 38 %: la **direzione** è consistente, la **misura** no. Citare «aiuta, fra
> l'8 e il 46 %»; mai «+46 %», che è solo la mediana di tre valori dispersi.


| | mediana | rapporto |
|---|---|---|
| `--carriers 1`, 4 connessioni | 68,52 MB/s | — |
| `--carriers 4`, 4 connessioni | 83,92 MB/s | **c4/c1 = 1,457** |

I tre rapporti sono 1,457 · 1,463 · 1,085. Due sono stretti, il terzo no: la
fase gira **tre** round e la sua dispersione lo dice, quindi il 46 % è la
mediana di tre campioni e non un numero raffinato. Va letto come "grande e
riproducibile", non come "1,457".

**Questa è la seconda metà di una regola di cui finora esisteva solo la prima.**
La campagna vhost aveva misurato i carrier che *danneggiano leggermente* un
percorso ozioso e pulito (mediana c4/c1 **0,941**), ed è il motivo per cui
`choose_carrier` si riduce byte per byte al round-robin quando non c'è nulla da
schedulare (DEC-VE6) e perché il default resta 1. Le due misure non sono in
conflitto: **i carrier non costano nulla quando non c'è concorrenza e comprano
il 46 % quando ce ne sono quattro.** È esattamente il presupposto su cui poggia
la crescita automatica della fase 03, e il suo tetto di 4 (`auto_carrier_ceiling`)
cade dove questa tabella smette di migliorare.

Nota che `--carriers 4` **non supera** il flusso singolo (83,92 contro 106,72):
recupera gran parte della perdita da concorrenza, non aggiunge banda.

### 36.3 Il gradino n=8 NON è una misura del tunnel, ed è onesto dirlo

A 8 connessioni i due trasporti convergono su **618–620 Mbit/s**, cioè lo
stesso numero entro lo 0,3 %. Due trasporti che non condividono quasi nulla e
che a 4 connessioni differiscono del 39 % non arrivano allo stesso valore per
caso: a n=8 sta legando **qualcosa che condividono**.

Non è la linea: `asym_qualify` misura 933 Mbit/s a P=8 verso la stessa VM, cioè
il 50 % in più. I due candidati che restano sono l'**origine**
(`scripts/perf/raw_origin.py`, un singolo processo asyncio sulla VM) e la VM
stessa (`c7i-flex.large`, 2 vCPU). Il dato interno che li rende plausibili è
che a n=4 l'origine è la **stessa** per entrambi i bracci e i bracci
differiscono del 39 % — quindi l'origine non può spiegare n=4, ma può
benissimo legare a n=8, dove il costo per socket del ciclo di eventi si moltiplica.

**Quindi la scala si legge fino a n=4 e il gradino n=8 resta non attribuito.**
Il test che lo decide è banale e non è stato fatto: campionare la CPU del
processo origine durante il gradino, o rifare il solo n=8 contro un'origine che
non sia Python. Registrato come buco, non risolto per intuizione.

### 36.4 Cosa cambia per chi usa il prodotto

- Un tunnel pubblico che serve **una o due** connessioni alla volta gira al
  96–98 % della linea sul relay, e il percorso diretto non serve a quello.
- Da **quattro** connessioni concorrenti in su il relay a carrier singolo perde
  oltre il 40 %: o `--udp`, o `--carriers 4`.
- `--carriers` resta a 1 di default perché su un percorso ozioso non aiuta e
  costa qualcosa; la crescita automatica (`--carriers 0`) esiste precisamente
  perché la soglia utile è un fatto del **traffico**, non della configurazione.

---

## 37. M-1 misurato con lo strumento riparato: **quattro connessioni perse su quattro riconnessioni**

**Fase:** `vpn/vpn_ctrl_leak.sh` → `out/eth/vpn_ctrl_leak.before-7fb1a6b6a35266d8.out`
(207 s, `rc=1` — il rifiuto è il verdetto, non un errore). Binario
`7fb1a6b6a35266d8` = `ed50a40`, cioè **prima** della correzione di `src/mux.rs`.

Questa è la ri-esecuzione promessa in §31, dove lo stesso cancello aveva
stampato «0 established … −1 leaked» mentre il suo stesso dump elencava cinque
socket: cercava `:7835` quando `BORE_TO` dice `https://…`, cioè `:443`. Con la
porta **derivata** da `BORE_TO` il cancello vede quello che c'è:

```
  fresh link             ctrl_conns=1
  after reconnect 1      ctrl_conns=2
  after reconnect 2      ctrl_conns=3
  after reconnect 3      ctrl_conns=3
  after reconnect 4      ctrl_conns=4
  --- settling 120 s ---
  t+30s .. t+120s        ctrl_conns=5   (piatto, mai riassorbite)
```

**Cinque connessioni TCP `ESTABLISHED` verso la porta di controllo dove la
corretta è UNA**, tutte e cinque con un descrittore aperto nello stesso
processo, nessuna in `FIN_WAIT` o `TIME_WAIT` — cioè non è un socket che il TCP
sta chiudendo, è un socket che nessuno chiude. Quattro perse su quattro
riconnessioni, più quella viva.

Il conteggio **non scende mai**, e sale anche durante il settle (4 → 5): la
serie è monotona, che è la firma di una perdita e non di un transitorio.

**Perché conta oltre i cinque descrittori.** Il meccanismo è quello annotato
negli invarianti: `yamux::Connection` vive in un task staccato e `drive()`
usciva dal proprio ciclo solo su `Step::Done`, che richiede la chiusura del
**peer** — e `src/server.rs` esegue lo stesso `drive()`, quindi nessuno dei due
capi la iniziava. Un socket resta quindi appeso a **entrambi** i capi per ogni
connessione finita: sul server, che ne ospita molti, il costo non è il client.

**Questo è il "prima".** Il file è conservato col checksum del binario nel nome
perché il "dopo" gira sullo **stesso cancello** contro il binario corretto in
P5, e l'invalidazione dei marker in `p5_build_gate.sh` — che si accorge del
cambio di checksum — esiste precisamente perché un `SKIP (marker present)`
avrebbe fatto passare la correzione senza verificarla (§33).

Nota di metodo, terza volta in questa campagna: il cancello che ha prodotto
questi numeri è lo stesso che due ore prima ne aveva prodotti di **opposti** con
lo stesso binario e la stessa rete. La differenza era una costante sbagliata
nello strumento. Un verdetto di prodotto vale quanto la prova che lo strumento
sa misurare — ed è il motivo per cui `vpn_ctrl_leak.sh` ora si rifiuta di
pubblicare un verdetto quando il conteggio finale è sotto 1: con zero
connessioni non c'è nemmeno il link, quindi non c'è niente su cui pronunciarsi.

---

## 38. Il download di questa linea è fermo allo 0,8 %, l'upload **oscilla** del 20 % in pochi minuti

Ogni blocco di ogni driver rilegge la linea nuda prima di partire — `iperf3`
verso la VM, P=1, 8 s per direzione, nessun bore nel percorso. Quattordici
letture in nove ore:

| ora | down | up | | ora | down | up |
|---|---|---|---|---|---|---|
| 12/09 19:24 | 923 | 730 | | 13/09 01:15 | 928 | 742 |
| 12/09 19:45 | 924 | 730 | | 13/09 04:17 | 922 | **714** |
| 12/09 20:51 | 927 | 733 | | 13/09 04:21 | 927 | 739 |
| 12/09 21:54 | 929 | 737 | | 13/09 04:25 | 926 | **622** |
| 12/09 23:58 | 926 | 742 | | 13/09 04:26 | 926 | **638** |
| 13/09 00:57 | 922 | 740 | | 13/09 04:30 | 928 | **673** |
| 13/09 01:13 | 924 | 740 | | 13/09 04:30 | 926 | **740** |

**Download: 922–929 Mbit/s, dispersione 0,8 % in nove ore.**
**Upload: 622–742, dispersione 19,3 %.**

### 38.1 Non è una degradazione: è un'oscillazione

La prima lettura di questa sezione era sbagliata. Le tre letture basse delle
04:25–04:30 sembravano un calo dell'uplink, e la lettura successiva — **quattro
minuti dopo** — è tornata a **740**. Non c'è una tendenza: c'è un'oscillazione
fra ~620 e ~742 su scala di **minuti**, mentre il download non si muove.

La prova indipendente è nella stessa finestra e con un altro strumento.
`asym_qualify.sh` offre lo stesso carico a tre destinazioni indipendenti, ed è
costruito precisamente per distinguere un limite che segue la **sorgente** da
uno che appartiene a una destinazione:

| | download P=1 | upload P=1 | download P=8 | upload P=8 |
|---|---|---|---|---|
| VM di test | 924 | **678** (617 e 739 nelle due ripetizioni) | 934 | 656 |
| provider francese | 913 | 597 | 910 | 611 |
| Cloudflare | n/d (403) | 210 | n/d (403) | 725 |

Le **due ripetizioni della stessa cella**, a due minuti di distanza, leggono
**617** e **739** Mbit/s in upload mentre il download della stessa cella legge
923 e 925. Un'oscillazione del 20 % dentro una singola fase, su un'unica
variabile: il tempo.

Ed è **della sorgente**: scende verso tutte e tre le destinazioni. Il 12/09 la
stessa fase leggeva 740 / 730 / 779; oggi 656 / 611 / 725. Quel 740 non era un
valore stabile, era un **campione alto**.

### 38.2 Conseguenze

1. **Nessuna percentuale di upload va citata contro una linea misurata in
   un'altra ora** — e ora si sa che «un'altra ora» può voler dire «quattro
   minuti prima». È la regola V-9 (i rapporti contro un controllo nudo
   campionato *nella stessa ripetizione* valgono ovunque, i valori assoluti no);
   queste righe sono la dimostrazione, non una regola nuova.
2. **§34.5 ha un candidato.** Il calo monotono dell'upload di `ws_asym` —
   85,70 → 76,27 → 66,07 MB/s, −23 % in quindici minuti, con delta di allowance
   **zero** su tutti e tre i bracci — è dello stesso ordine, dello stesso segno e
   sulla stessa scala temporale dell'oscillazione qui misurata sulla linea nuda.
   Non è dimostrato, perché quella fase non ha un controllo nudo dentro la
   ripetizione (§34.4, terzo difetto); ma non è più «senza meccanismo».
3. **Il download resta la direzione su cui questa campagna può parlare in
   assoluto.** «96,8 % della linea» (§36) e «890 Mbit/s su una connessione»
   (§35) poggiano su una linea che in nove ore si è mossa dello 0,8 %.
4. **Una fase di upload a una sola ripetizione su questa linea non misura
   niente.** Con l'uplink che si muove del 20 % fra un minuto e l'altro, un
   braccio singolo campiona la fase dell'oscillazione, non il prodotto.

### 38.3 Due strumenti che si sono comportati bene

Vale la pena registrarlo, perché entrambi i comportamenti sono stati aggiunti
dopo un errore: il braccio download di Cloudflare stampa
`FAILED(http=403,403,...)` invece di `0` — la correzione di `cf()`, che
confronta il conteggio di byte di curl con quello del kernel — e il braccio
`public` della prima ripetizione stampa `UNREACHABLE` e resta fuori dalla
mediana. Due celle fallite, due fatti nel file, nessuno zero spacciato per
misura.

---

## 39. `ws_tunnel` — sette configurazioni di tunnel vhost, e il primo risultato che NON ha bisogno di essere qualificato

Questa fase non aveva mai prodotto dati in questa finestra: girava con un `$B`
non inizializzato sotto `set -u` e moriva prima della prima misura (difetto 12).
Con la correzione ha girato per intero in **620 s** alle 04:30 del 13/09, subito
dopo la baseline che leggeva 922/714 Mbit/s.

Workstation come consumer, provider e origine sulla VM, 20 s per punto di
download, 256 MiB per stream in upload, `oha -z 8s -c 8` su risposte da 1 KiB per
la riga di latenza — **otto richieste concorrenti**, non una sequenza.

| arm | dl x1 | dl x4 | dl x8 | up x1 | up x4 | rps | p50 ms |
|---|---|---|---|---|---|---|---|
| relay-tcp c=1 | 930 | 932 | **934** | 706 | 903 | 408 | 19,19 |
| relay-tcp c=4 | 913 | 933 | 932 | 698 | 819 | 414 | 19,15 |
| relay-tcp c=8 | 929 | 932 | 932 | 688 | 827 | 410 | 19,23 |
| relay-tcp c=0auto (salito a 3) | 882 | 933 | 933 | 655 | 804 | 430 | 19,43 |
| direct-quic c=1 | 911 | 931 | 901 | 670 | 948 | 361 | 22,13 |
| direct-quic c=4 | 892 | 931 | 933 | 678 | 818 | 397 | 19,51 |
| direct-quic c=0auto (rimasto a 1) | 914 | 932 | 857 | 673 | 876 | 388 | 22,06 |

*(Mbit/s; ogni riga è un `path=` **verificato** dall'API di admin, mai un'etichetta.)*

### 39.1 Il tunnel non è più il collo di bottiglia, in nessuna configurazione

Venti dei ventuno punti di download stanno fra **857 e 934 Mbit/s** contro una
linea nuda di 922–930 misurata la stessa notte: **93–101 %**. L'upload a quattro
stream sta fra 804 e 948 contro 714–740: **fino al 100 %**. Non c'è deficit da
spiegare. Il lavoro di V-10/V-13/P-13 e la fine del WiFi hanno tolto di mezzo
tutto quello che le campagne precedenti stavano inseguendo, e quello che resta è
un tunnel che consegna la linea.

L'unico numero sotto il 93 % è `up x1`, 655–706 su 714–740, cioè **88–95 %** — ed
è il solito `window / RTT` su una connessione sola, non una proprietà del
prodotto (V-9: a P=1 la linea stessa dà 726 verso la VM e 210 verso Cloudflare,
ordinati per RTT).

### 39.2 I carrier non comprano banda qui, e non la costano

Da `c=1` a `c=8`, su entrambi i percorsi, il download a otto stream si muove fra
**932 e 934 Mbit/s**: 0,2 % di escursione su un fattore otto di carrier. Il
`c=0auto` del relay è salito a 3 e ha consegnato gli stessi numeri del `c=1`.
È esattamente ciò che DEC-VE6 prevede — lo scheduler si riduce al round-robin di
sempre quando non c'è nulla da schedulare — e vale come conferma sul campo che il
default `--carriers 1` non lascia niente sul tavolo su una linea pulita.

Da notare che `direct-quic c=0auto` è rimasto a **1** carrier mentre il relay è
salito a 3: il nudge del server nasce da `CarrierPick.bulk_load`, che esiste solo
per il pool TCP. Sul percorso diretto la contesa la gestisce
`BulkSendState`, non i carrier. Comportamento corretto, qui misurato.

### 39.3 Il costo del percorso diretto è la LATENZA sotto concorrenza, e i carrier lo pagano

L'unica differenza sistematica fra i due percorsi in tutta la tabella:

| | rps | p50 | p95 | p99 |
|---|---|---|---|---|
| relay, qualunque numero di carrier | 408–430 | 19,15–19,43 | 19,98–22,10 | 20,32–22,42 |
| direct **c=1** | 361 | 22,13 | 23,68 | 23,92 |
| direct **c=4** | 397 | 19,51 | 23,40 | 23,80 |
| direct c=0auto (1 carrier) | 388 | 22,06 | 22,90 | 23,18 |

Otto richieste concorrenti da 1 KiB su **una** connessione QUIC costano
**+2,9 ms di p50 e −12 % di rps** rispetto al relay; con quattro connessioni QUIC
il p50 torna a 19,5 e l'rps a 397. I due arm a un carrier (`c=1` e il `c=0auto`
che è rimasto a 1) leggono 22,13 e 22,06 — si confermano a vicenda.

**Questo è il rovescio esatto dell'intuizione che §36 aveva usato.** Il relay,
cioè "un unico flusso TCP ordinato", serve otto richieste concorrenti con lo
stesso p50 a uno o a otto carrier; il percorso diretto, cioè "uno stream QUIC per
connessione", è quello che ha bisogno di più connessioni per non perdere
latenza. Con risposte da 1 KiB non c'è nulla in coda da bloccare in testa, quindi
l'head-of-line del relay non ha modo di manifestarsi — mentre sul QUIC la coda
condivisa è il controllo di congestione e il pacer dell'unica connessione.

Il meccanismo preciso NON è stabilito da questa fase e non viene dichiarato:
serve una scala di concorrenza sulla riga di latenza (c=1 contro c=4 a 1, 2, 4, 8,
16 richieste in volo) per separare il pacing dalla coda applicativa. È l'unica
domanda che questa tabella lascia aperta, ed è una domanda sulla latenza, non
sulla banda.

### 39.4 Conseguenza operativa

Per un carico di **richieste piccole e concorrenti** — cioè un sito — il relay a
un carrier è la configurazione migliore misurata: massimo rps, minimo p50, zero
configurazione. Il percorso diretto va scelto per quello che compra altrove
(il costo CPU sul server, §13.1 della campagna vhost), e se lo si sceglie con un
carico interattivo va accompagnato da `--carriers 4`.

---

## 40. `ws_first_conn` — la penalità della prima connessione è il controllo di congestione, e vale il 4,2 %

§35 aveva registrato che il primo trasferimento di un braccio è più lento degli
altri, senza poter dire **di che cosa** fosse il costo. Le tre possibilità erano
la composizione del pool QUIC, un fallback silenzioso sul relay, e un controllore
di congestione freddo — indistinguibili da un solo numero.

Questa fase le separa per costruzione: registra **entrambi** i tunnel in anticipo
(relay su 9047, `--udp` su 9048), poi li guida a turno, e stampa i contatori del
server **prima e dopo ogni trasferimento**, non solo alla fine.

| trasferimento | relay MB/s | quic MB/s |
|---|---|---|
| 1 | 106,39 | **102,45** |
| 2 | 106,46 | 106,92 |
| 3 | 106,85 | 106,12 |

Contatori del braccio quic, letti dall'API di admin:

| momento | `current_path` | `direct_stream_opens` | `direct_fallbacks` | `direct_pool` |
|---|---|---|---|---|
| prima di ogni traffico | `unknown` | 0 | 0 | **1** |
| dopo il trasferimento 1 | `direct` | 1 | 0 | 1 |
| dopo il 2 | `direct` | 2 | 0 | 1 |
| dopo il 3 | `direct` | 3 | 0 | 1 |

### 40.1 Due delle tre ipotesi cadono per costruzione, non per interpretazione

**Il pool vale 1 prima che sia passato un byte.** La connessione QUIC viene
composta alla registrazione, non alla prima richiesta: quindi la penalità non è
il costo di comporla. **`direct_fallbacks` resta 0** e `current_path` legge
`direct` già dopo il primo trasferimento: quindi non è un passaggio silenzioso
sul relay travestito da misura del diretto — che è il modo in cui questa domanda
poteva ricevere la risposta sbagliata.

Resta la terza: un controllore di congestione che parte freddo su una
connessione che non ha ancora portato nulla. Costa **4,2 %** su 384 MiB
(102,45 contro una media di 106,52 sui due successivi), una volta sola, e si
richiude da sé.

Da notare anche che `current_path` legge `unknown` sul tunnel `--udp` che non ha
ancora proxato nulla, e `relay` sul tunnel che non ha chiesto `--udp`: è
esattamente la distinzione che P-10 impone, qui osservata sul campo.

### 40.2 Perché il braccio relay NON paga lo stesso costo

Il relay non ha un primo trasferimento freddo perché il suo carrier TCP porta la
connessione di controllo **dalla registrazione in poi**: quando arriva il primo
byte di dati quella connessione ha già una finestra aperta. È lo stesso
meccanismo che rende il relay «warm» nella documentazione del prodotto, qui
misurato invece che dichiarato — 106,39 al primo trasferimento contro 106,46 e
106,85, cioè 0,4 % di escursione.

**Conseguenza pratica:** su un tunnel `--udp` la prima richiesta pesante dopo la
registrazione è ~4 % più lenta, e nient'altro. Non c'è niente da correggere: la
correzione sarebbe scaldare la connessione con traffico finto, che costa banda
vera per un risparmio che si esaurisce in un trasferimento.

### 40.3 Il 4,2 % di qui e il 18,6 % di §35 non sono lo stesso numero, e la differenza è informativa

§35 misura la stessa penalità a **18,6 %** (384 MiB) e **21,1 %** (96 MiB);
questa fase la misura a **4,2 %** sullo stesso identico formato di
trasferimento. Un fattore quattro fra due misure della stessa cosa va
dichiarato, non mediato.

Le due fasi differiscono in un punto strutturale, ed è l'unico:

- §35 registra il tunnel e **trasferisce subito**. Il primo trasferimento quic
  è la prima cosa che tocca quel tunnel.
- questa fase registra **entrambi** i tunnel in anticipo e poi guida per primo
  il braccio **relay** — cioè fa passare ~40 s fra la registrazione del tunnel
  `--udp` e il suo primo byte.

L'ipotesi compatibile con entrambe è quindi che il costo non sia funzione del
**numero d'ordine** del trasferimento ma del **tempo trascorso dalla
registrazione**: a 40 s la connessione QUIC è composta, validata e ha già
scambiato keepalive, e resta solo la finestra di congestione da aprire (il
4,2 % di qui); a zero secondi si paga anche il resto (il 18,6 % di §35).

**È un'ipotesi, e va scritta come tale.** La fase che la decide è questa stessa,
con un ritardo variabile fra registrazione e primo trasferimento (0, 5, 20, 60 s)
come unico asse — un'ora di misura, nessun codice nuovo. Finché non gira, il
numero da citare per «quanto costa la prima richiesta su un tunnel appena
registrato» è quello **peggiore**, cioè il 18,6 % di §35, e questa sezione dice
quanto di quel costo sparisce da solo se il tunnel è in piedi da un minuto.

---

## 41. `origin_cpu` — la CPU NON è il tetto di n=8, su nessuno dei tre host

§36 aveva lasciato aperta una domanda con tre candidati: al gradino n=8 i due
bracci del ladder pubblico convergono e si fermano, e il limite poteva stare
nell'**origine**, nella **VM** o nel **client**. Questa fase li campiona tutti e
tre attorno agli stessi trasferimenti, con n=4 come **controllo**: a n=4 i due
trasporti provabilmente differiscono, quindi qualunque CPU spendano lì non è
ciò che lega.

Origine `raw_origin.py` (pid letto dalla VM, `CLK_TCK` 100, 2 core), 460 MiB per
gradino:

| gradino | braccio | MB/s | CPU origine (s) | CPU s/GiB | origine %core | VM occupata % | client %core |
|---|---|---|---|---|---|---|---|
| 4 | relay | 70,41 | 0,18 | 0,40 | **3** | 9 | 10 |
| 4 | quic | 91,31 | 0,19 | 0,42 | **4** | 20 | 17 |
| 8 | relay | 77,93 | 0,17 | 0,38 | **3** | 10 | 12 |
| 8 | quic | 87,27 | 0,18 | 0,40 | **3** | 19 | 14 |

**Nessuna colonna si avvicina alla saturazione, in nessuna riga.** L'origine
spende il 3–4 % di un core, l'intera istanza il 9–20 %, il client fra il 10 e il
17 % di un core. Il costo per GiB consegnato è costante entro il 10 % fra n=4 e
n=8 e fra i due trasporti: l'origine fa la stessa quantità di lavoro per byte a
qualunque gradino, cioè non sta degradando.

E la fase si rifiuta di girare se non riesce a leggere il pid dell'origine o
`CLK_TCK` (`INSTRUMENT FAILURE`, uscita 2) — perché una colonna CPU che legge
zero perché il campionamento non è partito è indistinguibile da un host scarico,
ed è la classe di difetto che questa campagna ha pagato otto volte.

### 41.1 Che cosa esclude davvero, e che cosa no

Esclude la CPU. Non esclude il **client**, e la distinzione è sostanziale: un
event loop che serializza le letture è legato dalla **latenza**, non dal
processore, e mentre lo fa sembra scarico. Il 12–17 % di un core è compatibile
sia con "il client ha molto margine" sia con "il client passa l'86 % del tempo
ad aspettare un `await` alla volta".

Non campiona nemmeno il **server di staging**, che rilaya ogni byte del percorso
pubblico e sta su un terzo host. Il braccio vhost di `ws_tunnel` attraversa lo
stesso server e muove 933 Mbit/s con otto stream (§39), quindi quel server
*può* muovere quella banda — ma su un percorso di codice diverso, e questo è un
argomento, non una misura.

### 41.2 L'esperimento che chiude la domanda, e perché è piccolo

Resta una sola variabile lato client, ed è la **topologia dei processi**:
`raw_client.py` guida n connessioni da UN processo asyncio, mentre `ws_tunnel`
— che a otto stream arriva a 933 Mbit/s sulla stessa linea — usa `curl`, cioè
n processi separati. `pub/ws_conns_procs.sh` varia esattamente quello e
nient'altro: stessa origine, stesso protocollo, stessi tunnel, stessi byte per
gradino, stessa notte.

Se `many/one` sta a 1,00 il client è scagionato e il tetto è a valle di lui; se
sta sopra 1,00 a n=8 e a 1,00 a n=4, il gradino alto del ladder misurava
**python** e ogni conclusione tratta da quel gradino su bore va ritirata.

---

## 42. `ws_conns_procs` — la topologia del client non spiega il ladder, e nel misurarlo il ladder si è rivelato NON RIPRODUCIBILE a n≥4

La fase doveva rispondere a una domanda sola: il gradino alto del ladder
pubblico misura bore o misura `raw_client.py`? Ha risposto a quella, e nel farlo
ne ha chiusa una più importante che non era stata posta.

24 celle: due gradini (4, 8), due bracci (relay, `--udp`), due topologie di
client, tre ripetizioni, 460 MiB per cella, 75 s fra una cella e l'altra,
ordine delle topologie alternato fra ripetizioni.

### 42.1 La risposta alla domanda posta: la topologia non è la spiegazione

| braccio | n | `one` (1 proc × n conn) | `many` (n proc × 1 conn) | many/one |
|---|---|---|---|---|
| relay | 4 | 86,81 | 91,85 | **1,058** |
| relay | 8 | 74,52 | 70,83 | **0,950** |
| quic | 4 | 81,50 | 92,28 | **1,132** |
| quic | 8 | 75,95 | 74,53 | **0,981** |

Tutti e quattro i rapporti stanno entro il 13 % da 1,00, e due dei quattro
stanno *sotto*. `raw_client.py` non è il tetto: n processi separati non fanno
meglio di uno, e il cronometraggio era perfino a loro sfavore (per `many` il
rate è calcolato sul processo **più lento**, come se tutti fossero durati
quanto lui).

### 42.2 La risposta alla domanda vera: quei rapporti non significano niente, perché la cella non è stabile

Prima di attribuire un 5 % a una topologia bisogna sapere quanto vale la stessa
cella ripetuta. Escursione `(max − min) / mediana` sulle tre ripetizioni:

| cella | min | mediana | max | escursione |
|---|---|---|---|---|
| relay n=4 `one` | 75,48 | 86,81 | 108,11 | **37,6 %** |
| relay n=4 `many` | 75,72 | 91,85 | 94,26 | 20,2 % |
| relay n=8 `one` | 57,98 | 74,52 | 74,60 | 22,3 % |
| relay n=8 `many` | 63,61 | 70,83 | 92,00 | **40,1 %** |
| quic n=4 `one` | 62,93 | 81,50 | 100,02 | **45,5 %** |
| quic n=4 `many` | 88,89 | 92,28 | 106,90 | 19,5 % |
| quic n=8 `one` | 64,03 | 75,95 | 93,31 | 38,6 % |
| quic n=8 `many` | 69,03 | 74,53 | 104,71 | **47,9 %** |

**Mediana delle escursioni: 38,6 %.** L'effetto che la fase cercava vale fra il
2 e il 13 %. È sotto il rumore di un fattore tre, quindi la tabella 42.1 non
distingue nulla — e lo dice per costruzione, perché i campioni grezzi stanno
accanto alle mediane (V-11 impone di stamparli; qui è ciò che salva la fase dal
pubblicare un 1,132 come se fosse un risultato).

### 42.3 E questa instabilità NON c'è ai gradini bassi: compare a n≥4

Dalla stessa colonna di `pub_ws_conns_r2.out`, tre ripetizioni per cella:

| gradino | escursione relay | escursione quic |
|---|---|---|
| n=1 | **2,4 %** | **3,5 %** |
| n=2 | 5,9 % | 5,4 % |
| n=4 | 14,0 % | 8,4 % |
| n=8 | 4,4 % | 39,6 % |

A una e due connessioni la stessa cella si ripete entro il 6 %. Da quattro in su
l'escursione esplode, in questa esecuzione fino al 48 %. **Il regime ad alta
varianza è esso stesso il fenomeno**, e non era mai stato guardato perché
nessuna fase confrontava la dispersione fra gradini invece dei valori.

### 42.4 Conseguenza: §36 va ritirato, non ri-attribuito

§36 concludeva dal fatto che a n=4 i tre campioni del relay (67,17 / 63,38 /
58,29) non si sovrappongono ai tre del `--udp` (85,03 / 88,27 / 92,45). Con
tre campioni per cella e un'escursione reale del 38 %, **due terne separate sono
un evento ordinario, non una prova**. E infatti oggi, sullo stesso gradino e
sulla stessa linea, le stesse due celle leggono 75,48–108,11 (relay) e
62,93–100,02 (`--udp`): **si sovrappongono su quasi tutto l'intervallo**.

Quindi:

- **Cade** «il relay pubblico crolla a quattro connessioni». Non ha retto alla
  replica.
- **Cade** con esso la mia stessa correzione di §36.1-bis, che riformulava il
  crollo come «il limite sta sulla tratta server↔VM». Era una spiegazione
  migliore per un fatto che non c'è.
- **Resta in piedi** tutto ciò che §36 dice su n=1 e n=2, dove la cella è
  riproducibile entro il 6 %: a una connessione i due trasporti sono pari entro
  l'1 ‰, a due stanno entrambi a ~900 Mbit/s.
- **Resta aperto** perché la varianza compaia a n≥4. Candidati non separati:
  il percorso di accept pubblico del server sotto concorrenza, lo scheduling
  dell'istanza, e il micro-bursting dell'allowance ENA (a n≥4 il tasso
  istantaneo per flusso cambia). Nessuno dei tre è stato misurato e non se ne
  dichiara nessuno.

**La fase che lo chiude è nota e piccola:** la stessa cella ripetuta 15 volte
invece di 3, su un solo gradino, con i contatori di allowance della VM e del
server letti come delta attorno a ogni ripetizione. Un'ora. Finché non gira, il
ladder pubblico va citato **fino a n=2** e non oltre.

### 42.5 La regola che questa fase aggiunge all'harness

Una fase che pubblica un confronto fra celle deve pubblicare anche
**l'escursione della cella ripetuta**, e il lettore deve poter vedere i due
numeri accanto. Tre campioni bastano per una mediana; non bastano per dire che
due mediane differiscono. Qui l'effetto cercato era tre volte più piccolo del
rumore, e senza la colonna dell'escursione la tabella 42.1 sarebbe diventata
«la topologia dei processi vale il 13 % sul percorso diretto».

---

## 43. M-1 chiuso — la stessa misura, prima e dopo, sullo stesso percorso

§37 aveva confermato la perdita sul binario **prima** della correzione, con lo
strumento appena riparato. La correzione è ora nel binario e la stessa fase ha
girato di nuovo sullo stesso percorso reale.

| | prima (`7fb1a6b6a35266d8`) | dopo (`a159cea090281847`) |
|---|---|---|
| link fresco | 1 | 1 |
| dopo riconnessione 1 | 2 | 1 |
| dopo riconnessione 2 | 3 | 1 |
| dopo riconnessione 3 | 4 | 1 |
| dopo riconnessione 4 | 5 | 0 · |
| dopo 120 s di assestamento | **5, nessuna riassorbita** | **1** |

· Lo zero non è un difetto e non va letto come tale: il campione è caduto fra
la caduta e il ristabilimento del link. È esattamente il motivo per cui la fase
campiona **anche** dopo un assestamento di 120 s, dove legge 1.

Il file di prima è conservato come
`out/eth/vpn_ctrl_leak.before-7fb1a6b6a35266d8.out`: un «dopo» senza il «prima»
accanto non dimostra una correzione, dimostra solo uno stato.

### 43.1 Tre livelli di prova, e nessuno dei tre basta da solo

1. **Quattro unit** nel gruppo «Connection liveness» di `src/mux.rs`. Il primo è
   il red-check della perdita misurata (senza il conteggio `Liveness` **va in
   timeout** — cioè riproduce il sintomo di produzione, non un fallimento
   qualsiasi); gli altri tre rifiutano le correzioni troppo entusiaste che
   passerebbero il primo distruggendo traffico vivo. Sono nello stesso gruppo di
   proposito: la correzione ovvia («chiudi quando cade l'`Opener`») uccide
   substream ancora in uso, e la simmetrica («chiudi quando cade l'`Acceptor`»)
   sbaglia dall'altra parte, perché `src/pool.rs` tiene solo un `Opener` per
   tutta la vita di un tunnel.
2. **Il gate netns `T-CTRLLEAK`**, che nella finestra P5 ha letto
   `exactly one control connection after 3 reconnects (fds 11 -> 11)` dentro una
   suite di **169 PASS / 0 FAIL / 1 SKIP**. Lo skip è `T-PINMTU`, e si è
   dichiarato tale per la ragione giusta: restringere il percorso non ha mosso
   un TUN **non** pinnato, quindi il braccio pinnato non avrebbe discriminato
   nulla. Un gate che non discrimina e tace è il difetto che questa campagna ha
   già pagato due volte.
3. **Questa fase**, sul percorso reale, contro il server vero, con i descrittori
   letti da `/proc/<pid>/fd` e le connessioni da `ss` — mai dal log del processo
   che dovrebbe averle chiuse (P-12).

Il primo livello prova il **meccanismo**, il secondo il **cablaggio**, il terzo
che la cosa accade **dove vive il prodotto**. La perdita era invisibile ai primi
due fino a che non è stata vista dal terzo, ed è stato il terzo a trovarla.

---

## 44. Jump host: **l'82 % dell'apertura di sessione era un `sleep`**, e per trovarlo è servito prima costruire l'estremo che non esisteva

**Fase:** `jump_lat` / `jump_hol` / `jump_stab` (P6) — 13/09, gateway sulla VM di
test, provider su questa workstation, linea `924/736 Mbit/s` riletta a inizio e
fine blocco. Binario `ab8aaaf0330b09fc` verificato **identico ai due estremi**.

### 44.1 Il difetto dello strumento veniva prima di quello del prodotto

Il jump host è l'unico modo d'uso in cui la banda non è il prodotto: `ssh -J`
porta una sessione interattiva, quindi la grandezza è il tempo di andata e
ritorno. La fase lo scompone in cinque termini, e i due che contano si ottengono
per **sottrazione**:

```
wchan_ms - tcp_ms   costo del gateway        <- l'unico termine che questo progetto scrive
open_ms  - wchan_ms handshake dell'sshd interno
```

Una sottrazione ha bisogno che entrambi i termini esistano. Non esistevano:
**su questa workstation non c'è nessun sshd**. `openssh-server` non è
installato, la porta 22 risponde `Connection refused`, e il provider è stato
puntato su `127.0.0.1:22` per tutta la vita della campagna.

Il modo in cui questo è rimasto invisibile è la firma di questa campagna. La
fase stampava:

```
=== tcp=22.2 ms
=== wchan=1270.3 ms      <- un numero plausibile
=== open=FAILED ms
```

`open` falliva onestamente. `wchan` no, perché l'helper finiva così:

```bash
... | head -c 1 >/dev/null || { echo FAILED; return; }
```

con sopra il commento «leggere un byte è ciò che dimostra che il canale
trasporta dati davvero». Non lo dimostrava: **una pipeline che produce zero
byte esce comunque 0**. 1270,3 ms era il tempo che ci metteva a fallire, e
veniva pubblicato come latenza. È la stessa classe di difetto — *uno zero che
significa che lo strumento si è rotto* — travestita stavolta da pipe.
La guardia `INSTRUMENT FAILURE` aggiunta il giorno prima non poteva prenderlo:
controlla che esista *almeno un campione*, e un campione c'era.

Correzioni, entrambe nell'harness:

- `jump_wchan_ms` **ispeziona i byte**: legge 4 caratteri e pretende `SSH-`
  (RFC 4253 impone che la stringa di identificazione cominci così; nient'altro
  che quel canale possa trasportare lo fa).
- `jump_require_inner_target` è una **verifica della premessa**, eseguita prima
  della misura: un estremo interno assente non fa perdere una colonna, ne
  **corrompe un'altra** per sottrazione.

L'estremo interno è ora reale: `jump_inner_target.sh` alza un OpenSSH vero in un
container su `127.0.0.1:2222`. Container e non `apt install`, per due ragioni
dichiarate nel file: installare un sshd di sistema richiede root e soprattutto
**lascerebbe un sshd in ascolto sulla workstation dopo un benchmark**, contro la
regola per cui una fase lascia la macchina come l'ha trovata. Loopback e non la
VM, perché provider → estremo interno non deve attraversare la WAN: se lo
facesse, `open_ms - wchan_ms` conterrebbe un round trip che appartiene alla
topologia e la sottrazione lo attribuirebbe in silenzio al gateway.

Tre inciampi, tutti misurati e tutti annotati nel sorgente perché costano un
giro di debug ciascuno:

1. `adduser -D` di alpine lascia l'account **bloccato** (`!` in `/etc/shadow`) e
   sshd rifiuta un account bloccato **anche per chiave pubblica**, rispondendo
   al client il generico `Permission denied (publickey)`. Il motivo sta solo nel
   log del server: `User bench not allowed because account is locked`.
2. quella build di OpenSSH non ha GSSAPI, quindi nominare l'opzione è un errore
   di configurazione, non un no-op.
3. **la porta interna è un parametro lato client.** `sshjhost 127.0.0.1:2222`
   registra l'alias *alla porta 2222* — lo dice il banner del provider
   (`SSH jump host ready hostname=wsjump.j.test port=2222`) — e il gateway
   confronta la porta richiesta dal client con quella registrata. Un client che
   chiede `:22` si sente rispondere `channel 0: open failed: connect failed`,
   che si legge esattamente come un'origine irraggiungibile e non lo è.

E una confusione di identità che vale la pena fissare, perché in questa campagna
convivono **tre** nomi utente non intercambiabili:

| nome | dove vive | cosa succede se lo si usa nell'altro posto |
|---|---|---|
| `JUMP_SSH_USER` | principal del **gateway**, esiste solo dentro bore | usato all'interno: `no such user` |
| `JUMP_INNER_USER` | account reale sull'**sshd interno** | usato all'esterno: `classic_auth_required` |
| `BORE_VM_USER` | login della VM, solo per il deploy | nessuno dei due |

### 44.2 Con lo strumento riparato, il difetto del prodotto era una sola riga

Traccia `ssh -v` con marcatura temporale per riga, sul percorso reale
(workstation → gateway sulla VM, RTT 24 ms), **prima**:

```
  0.133  debug1: SSH2_MSG_SERVICE_ACCEPT received
  1.155  debug1: Authentications that can continue: password,publickey,...
  1.201  Authenticated to ... using "publickey".
  1.202  debug1: channel_connect_stdio_fwd: wsjump.j.test:2222
  1.252  SSH-2.0-OpenSSH_9.7
```

Tutto il secondo sta in **un solo intervallo**: 0,133 → 1,155, cioè **1,022 s**
nella risposta del server alla prima richiesta di autenticazione. Il lavoro vero
del gateway — apertura del canale e banner dell'sshd interno — è 1,202 → 1,252,
**46 ms**. Di un'apertura di sessione da 1,25 s, l'**82 %** era un'attesa fissa.

La causa è `russh::server::Config`, e bore non impostava né l'uno né l'altro
campo:

| campo | default russh | effetto |
|---|---|---|
| `auth_rejection_time` | 1 s | ritardo costante su un tentativo **rifiutato** |
| `auth_rejection_time_initial` | `None` | ricade sul precedente → **1 s** |

La documentazione di russh dice già a cosa serve il secondo: «*OpenSSH clients
will send an initial "none" auth to probe for authentication methods*». Quel
`none` non è un tentativo di indovinare una credenziale: RFC 4252 §5.2 lo rende
la **sonda di enumerazione dei metodi**, e il rifiuto del server è ciò che porta
al client l'elenco `Authentications that can continue` di cui ha bisogno prima
di poter offrire qualunque cosa. Ritardarlo non rallenta nessun attacco —
`none` non contiene segreti da indovinare, e l'alternativa per chi attacca è una
nuova connessione TCP più uno scambio di chiavi completo, che costa già molto
più del secondo tolto. Gli esempi di russh spediscono esattamente questa coppia.

Il costo era pagato da **ogni sessione di ogni ingresso** che il gateway serve
(jump host, vhost `ssh -R`, public, secret), non solo dal jump host; ma è per il
jump host — dove il prodotto è il round trip — che era il termine dominante di
un ordine di grandezza.

Correzione: `auth_rejection_time_initial: Some(Duration::ZERO)`, con
`auth_rejection_time` lasciato a 1 s per le credenziali **sbagliate**, entrambi
regolabili (`BORE_SSH_AUTH_REJECT_MS`, `BORE_SSH_AUTH_REJECT_INITIAL_MS`).

Stessa traccia, **dopo**:

```
  0.119  debug1: SSH2_MSG_SERVICE_ACCEPT received
  0.139  debug1: Authentications that can continue: password,publickey,...
  0.180  Authenticated to ... using "publickey".
  0.227  SSH-2.0-OpenSSH_9.7
  0.242  debug1: stdio forwarding: done
```

L'intervallo è passato da **1,022 s a 20 ms**, cioè a un RTT.

### 44.3 Prima e dopo, cinque ripetizioni sullo stesso percorso

| termine | prima | dopo (mediana di 5) | |
|---|---|---|---|
| `tcp` (connessione al gateway) | 22,2 ms | 22,0 ms | invariato, com'era atteso |
| `wchan` (+ handshake esterno + `direct-tcpip`) | 1248,6 ms | **250,8 ms** | −80 % |
| `open` (+ handshake dell'sshd interno) | 1592,7 ms | **657,8 ms** | −59 % |
| `chan` (nuovo canale su sessione viva) | — | 86,7 ms | |
| `echo` (un byte andata e ritorno) | — | 125,1 ms | |

Campioni grezzi di `wchan`/`open` dopo la correzione: 250,8/597,0 · 282,4/658,8
· 248,0/609,7 · 242,2/664,0 · 279,9/657,8.

Il residuo è topologia, non prodotto: con **due soli host** il client sta sulla
stessa macchina del provider, quindi ogni round trip dell'handshake **interno**
attraversa la WAN due volte (client → VM → workstation → container e ritorno,
≈ 44 ms). I ~400 ms di `open - wchan` sono ~6 round trip di un handshake SSH
completo su quel percorso a forcina, e gli 86,7 ms di `chan` sono due
attraversate. È esattamente il limite che D6/D7 registrano da tempo: **manca un
terzo host**. Con un client altrove, `open` scenderebbe di quanto vale la
forcina, e il termine che questo progetto controlla — i 46 ms di gateway — resta
quello misurato qui.

### 44.4 Regole che questa fase lascia

- **Una guardia che conta i campioni non protegge da un campione inventato.**
  `INSTRUMENT FAILURE` verifica che esista almeno una misura; non che la misura
  sia una misura. Un helper che *deriva un numero da un fallimento* la
  attraversa indisturbato.
- **Una pipeline non è un controllo.** `cmd | head -c N` esce 0 con zero byte in
  ingresso. Se il contenuto conta, va **letto in una variabile e confrontato**.
- **Verificare la premessa di una misura prima della misura**, soprattutto
  quando il risultato si ottiene per differenza: un termine assente non si
  limita a mancare, si redistribuisce sugli altri.

### 44.5 Correzione: dopo la riparazione, «costo del gateway» **non è** costo di bore

La fase etichetta `wchan_ms - tcp_ms` come «il costo del gateway, l'unico
termine che questo progetto può cambiare». Con il secondo di attesa dentro,
l'etichetta era di fatto vera: 1 022 dei 1 226 ms erano bore. Tolto quello,
**non lo è più**, e lasciarla com'è significherebbe attribuire al prodotto un
costo di protocollo. La traccia marcata a posteriori lo scompone:

| intervallo | durata | RTT (22 ms) | di chi è |
|---|---|---|---|
| scambio versioni + key exchange | 86 ms | ~4 | OpenSSH ↔ russh, imposto da RFC 4253 |
| autenticazione (`none` + `publickey`) | 61 ms | ~3 | RFC 4252 |
| apertura canale + dial del provider + banner interno | **46 ms** | ~2 | **bore** |
| | **193 ms** | | contro i 208–221 ms misurati per sottrazione |

Quindi dei ~210 ms, **~147 sono l'handshake SSH esterno** — che nessuna riga di
questo repository può accorciare — e **46 ms sono il gateway**, per di più
composti quasi interamente da due attraversate della WAN (il provider sta
dall'altra parte del gateway, non dentro di esso). Il margine residuo lato
prodotto su questo percorso è dell'ordine della decina di millisecondi, non
della centinaia.

Il commento di `jump_lat.sh` va corretto di conseguenza: il termine misurato è
«handshake esterno + apertura del canale», e solo il secondo addendo è nostro.

### 44.6 Relay contro diretto, e il costo dei carrier: **nessuna differenza misurabile**

Punti 3 e 4 del piano P6, cinque ripetizioni interleaved, percorso verificato
dall'admin API braccio per braccio (nessun braccio «direct» era in realtà relay):

| braccio | tcp | wchan | open | chan | echo |
|---|---|---|---|---|---|
| relay | 20,6 | 230,9 | 629,1 | 88,6 | 129,8 |
| direct | 23,6 | 231,8 | 578,3 | 89,0 | 130,0 |
| direct4 | 22,0 | 243,0 | 613,5 | 87,9 | 128,7 |

**E la dispersione della cella ripetuta, che è ciò che rende leggibile la
tabella** (regola guadagnata in §42): `wchan` del braccio relay varia fra 205,6
e 270,3 ms sulle sue cinque ripetizioni — **±14 %** — mentre la differenza fra
relay e direct è dello **0,4 %**. Per `chan` ed `echo` i 35 campioni per cella
si raggruppano per ripetizione (relay `chan`: 88,x · 8x · 77,x · 91,x · 9x),
quindi la variazione **fra** sessioni è del ~20 % contro l'1,4 % fra i bracci.

Conclusione onesta: **su un jump host relay e diretto sono indistinguibili, e i
carrier non costano latenza.** Non è un risultato deludente, è quello atteso e
finora mai verificato: ogni canale SSH usa esattamente uno stream bidi, il
percorso è dominato dal round trip e i due trasporti hanno lo stesso RTT. I
carrier qui comprano isolamento (vedi `jump_hol`), non banda — e il punto 4 del
piano chiedeva esattamente se lo pagassero in latenza. Non lo pagano.

Ne segue una raccomandazione operativa: per un jump host **non c'è ragione di
abilitare `--udp`** per la latenza. Se lo si abilita, lo si fa per l'isolamento
dei canali sotto carico, che è la domanda di `jump_hol`.

### 44.7 `jump_hol` — **nessun head-of-line**: 690 MiB su un canale non si sentono sull'altro

Domanda: un trasferimento massivo su un canale SSH ritarda la sessione
interattiva che gli sta accanto? È la prima verifica **sul campo** della
correzione HOL di russh vendorizzato (`crates/russh/HOL_FIX.md`, backpressure
legata alla finestra, PR upstream #730 non mergiata): finora era garantita solo
da test in-process, e questa campagna ha già documentato che un test
in-process fa false-pass proprio su questa classe di difetti.

Tre ripetizioni, bracci interleaved, carico di 15 s su un **secondo** canale
della stessa sessione mentre il primo campiona la latenza di un tasto:

| braccio | idle | loaded | loaded/idle | peggior loaded |
|---|---|---|---|---|
| relay | 128,2 | 124,8 | **0,97×** | 141,5 |
| direct | 131,3 | 130,7 | **1,00×** | 140,3 |

Carico effettivamente passato: 644–734 MiB per ripetizione (soglia minima della
fase: 8 MiB; una ripetizione sotto soglia viene scartata ed è ciò che ha fatto
emergere il difetto della trappola 30). Il «peggior loaded» è la colonna che
conta davvero — una mediana buona con una coda pessima è esattamente ciò che un
operatore sente — ed è **pari alla mediana idle**, non sopra.

Il rapporto leggermente **sotto** 1,0 è rumore: la dispersione fra ripetizioni
dello stesso braccio è ±9 % (relay idle: 140,3 · 118,8 · 128,2) contro il 3 %
della differenza misurata. Ma la misura idle/loaded è **appaiata dentro la
ripetizione**, quindi la deriva si annulla ed è il confronto giusto.

Conclusione: **il canale massivo e quello interattivo convivono**, su entrambi i
trasporti. È il risultato che la correzione russh prometteva e che non era mai
stato visto su una rete vera.

### 44.8 `jump_stab` — **16 PASS, 0 FAIL**, dopo che l'harness ha smesso di pretendere una promessa inesistente

Prima esecuzione: **PASS 6, FAIL 10, SKIP 2**. Nessuno dei dieci era un difetto
del prodotto. Erano **uno solo**, moltiplicato per cascata.

La verifica incriminata era «la sessione esterna sopravvive»: si blackhola
l'UDP verso la VM e si pretende che la sessione `ssh -J` già stabilita resti
viva. **Non può, e non è una promessa che il prodotto faccia.** Una sessione
`ssh -J` viaggia dentro **un** canale `direct-tcpip`, i cui byte stanno su uno
stream QUIC da un capo all'altro; uno stream che muore a metà non si sposta sul
relay senza perdere i byte in volo, e SSH non ha resumption. L'invariante in
CLAUDE.md riguarda l'**apertura** del canale — *«missing/dead/open-failed QUIC
falls back for the SAME channel to warm TCP»* — non la migrazione di uno già
stabilito.

Misurato esplicitamente, sondando ogni 5 s durante un blackout:

```
  t=5s   path=direct  master=yes
  t=10s  path=direct  master=yes
  t=15s  path=relay   master=NO      <- finisce CON il percorso, non prima
```

Muore nello stesso campione in cui il percorso cambia: è questo che identifica
la causa e la distingue da un timeout.

Da lì la cascata: `jump_chan_ms` apre un canale **attraverso il socket del
ControlMaster**, e quel master era appena morto — quindi «nuovo canale sul
relay» FAIL; `direct_fallbacks` non si muoveva perché nessun canale veniva mai
aperto — FAIL; le fasi successive campionavano sullo stesso cadavere — FAIL,
FAIL; il rekey non poteva avvenire su una sessione inesistente — SKIP. **Dieci
verdetti, una causa, e la causa era l'harness.**

Verificato a mano che la promessa vera regge, prima di riscrivere la fase:

```
=== does a FRESH session open during the blackout? ===
  open=592.8 ms   counters(opens fb carriers)=1 1 0
  open=621.2 ms   counters(opens fb carriers)=1 2 0
```

Una sessione **nuova** si apre in 592,8 e 621,2 ms con l'UDP morto — cioè al
costo ordinario (578–658 ms dalla §44.3) — e `direct_fallbacks` sale 0 → 1 → 2
esattamente come documentato. Il prodotto era corretto dall'inizio.

Riscritte le fasi 2–4 attorno all'invariante che esiste:

- la fine della sessione stabilita è registrata come **osservazione** (`OBSV`),
  non come verdetto: è una proprietà misurata del disegno, e contarla come PASS
  o come FAIL sarebbe una bugia in una delle due direzioni;
- la verifica è ora «una sessione **nuova** si apre sul relay caldo», con
  `jump_open_ms` e non `jump_chan_ms`;
- ogni fase che campiona **riapre** il master (`master_reopen`), e lo riapre con
  l'apritore di *questa* fase — quello che imposta `RekeyLimit` e `-E $STAB_LOG`
  — perché il log è l'unico oracolo del rekey. Riaprire con quello generico
  lascia una sessione perfettamente funzionante e **non osservabile**: la
  verifica stampa SKIP per sempre, onesta e inutile.

Risultato, due ripetizioni:

| verifica | rep 1 | rep 2 |
|---|---|---|
| caduta sul relay | 11 s (budget 30) | 11 s |
| sessione nuova sul relay | 674,8 ms | 584,5 ms |
| `direct_fallbacks` si muove | 0 → 1, carrier 1 → 0 | idem |
| un alias, una riga admin | 1 | 1 |
| ritorno al diretto | 15 s (budget 90) | 16 s |
| sessione viva attraverso il ritorno | sì | sì |
| **rekey attraversato** | **5 rekey**, tasto a 248,4 ms dopo | **5 rekey**, 216,1 ms |
| sessione viva dopo 130 s | sì | sì |

**PASS 16, FAIL 0, SKIP 0.** Da notare la verifica «sessione viva attraverso il
ritorno al diretto»: è la direzione *opposta* dell'osservazione qui sopra e
**questa sì** è una promessa — un canale in volo è appuntato al suo trasporto, e
il percorso che torna diretto cambia dove andrà il canale *successivo*, mai
quelli già aperti. Regge.

### 44.9 Un limite di questa fase, dichiarato invece che nascosto

La tabella per fase dice `blackout 1,11×` e `recovered 1,21×` contro la
baseline. **Non va letta come «il relay costa il 21 % di latenza»**, per due
ragioni che i campioni grezzi rendono visibili:

```
  baseline    128.8 128.3 128.1 128.6 129.4 | 113.2 113.8 113.4 111.9 112.3
  blackout    148.0 146.5 145.7 146.7 148.1 | 121.0 119.3 121.6 120.1 119.8
  recovered   246.2 146.4 147.1 147.4 148.9 | 213.1 121.3 120.2 120.2 119.8
```

1. le due ripetizioni stanno su **livelli diversi** (128 contro 113 di
   baseline), quindi una mediana aggregata mescola due popolazioni;
2. il **primo** campione dopo un evento di sessione (riapertura, rekey) è un
   fuori scala — 246,2 e 213,1 ms contro i ~147 e ~120 a regime — e sposta la
   mediana di una cella da dieci campioni.

E soprattutto: in questa fase la baseline è **sempre** la prima e il relay
**sempre** dopo, quindi la fase non può separare «il relay costa di più» da «è
passato del tempo». `jump_lat`, che i bracci li **interleava** dentro la
ripetizione — il disegno che annulla la deriva — misura `echo` a 129,8 ms su
relay contro 130,0 su diretto: **identici**. Quella è la risposta; questa
tabella è una serie temporale, non un confronto.

Miglioramento naturale e non fatto in questa finestra: campionare una seconda
baseline **dopo** il ritorno al diretto, così la deriva diventa un numero anche
qui. Registrato fra le domande aperte.

### 44.10 Cosa risponde P6, punto per punto

| # del piano | domanda | risposta |
|---|---|---|
| 1 | tempo di apertura sessione, scomposto | tcp 22 · gateway 210 (di cui **46 nostri**) · sshd interno ~400 ms — §44.3, §44.5 |
| 2 | RTT applicativo a sessione aperta | `chan` 87–89 ms, `echo` 129–130 ms — §44.6 |
| 3 | relay contro diretto | **indistinguibili**, differenza 0,4 % contro ±14 % di dispersione — §44.6 |
| 4 | costo dei carrier | **nessuno**: `direct4` pari a `direct` — §44.6 |
| 5 | stabilità, fallback, rekey | **16 PASS / 0 FAIL**, fallback in 11 s, ritorno in 15–16 s, 5 rekey attraversati — §44.8 |

E una raccomandazione operativa che nasce dai punti 3 e 4: **per un jump host
non c'è ragione di abilitare `--udp` per la latenza.** Se lo si abilita è per
l'isolamento dei canali sotto carico (§44.7) — che però regge anche sul relay.

---

## 45. P7b — i buchi di topologia VPN, e due strumenti che misuravano sé stessi

Cinque fasi scritte per coprire quello che il Blocco A non aveva toccato. Tre
hanno risposto subito; due hanno prima dovuto essere riparate, e la riparazione
è il risultato più istruttivo del blocco.

### 45.1 `vpn_wiring` — ogni manopola arriva davvero al dispositivo

Verifica di **cablaggio**, non di valore: gli unit test coprono la risoluzione
del valore, questa fase alza un link e poi **rilegge il dispositivo dal kernel**
(P-12).

| caso | atteso | letto | |
|---|---|---|---|
| default | 128 | 128 | il default di V-10 arriva al device |
| `BORE_VPN_TUN_TXQUEUELEN=64` | 64 | 64 | override in range applicato alla lettera |
| `=0` | 500 | 500 | «0» lascia il valore del kernel |
| `=1` | 16 | 16 | clampato in basso, non rifiutato |
| `--mtu 1350` | 1350 | 1350 | |

`pass=5 fail=0`. Un caso — `=9999` — è marcato **NOT DISCRIMINATING** invece
che PASS, ed è la riga più onesta della tabella: il clamp verso l'alto è 500,
che è *anche* il default del kernel, quindi quel caso non distingue «clampato»
da «ignorato». Un test che non può fallire non è una prova, e dirlo vale più
che contarlo fra i passati.

### 45.2 `vpn_routes` — il default-deny regge in tutte e nove le combinazioni

Il connector installa rotte solo se autorizzato (I-MC8). Il listener annuncia
`192.168.77.0/24`, una subnet che non esiste da nessuna parte: la fase **legge
la tabella di routing**, non manda un pacchetto.

| politica | rotta installata | |
|---|---|---|
| nessun flag (default) | no | **default-deny**, com'è documentato |
| `--refuse-all-routes` | no | |
| `--accept-all-routes` | sì | |
| `--accept-routes` esatta | sì | |
| `--accept-routes` supernet `192.168.0.0/16` | sì | la regola «uguale o supernet» |
| `--accept-routes` estranea `10.0.0.0/8` | no | |
| `--accept-all` + `--refuse-routes` la rotta | no | il rifiuto vince |
| `--refuse-all-routes` + `--accept-all` | no | il rifiuto vince |
| `--no-route-manage` | no | |

`pass=9 fail=0`.

### 45.3 `vpn_traversal` — nessun flag di traversal sposta nulla su questo percorso, e `--upnp` non è misurato

Tre ripetizioni, il flag sempre sul **connector** (la workstation, dietro NAT),
listener identico in ogni braccio.

| braccio | ttd mediano | candidati | |
|---|---|---|---|
| baseline | 410 ms | 2 | |
| `--nat-udp-preferred-port` | 424 ms | 2 | |
| `--try-port-prediction` | 426 ms | 2 | |
| `--stun-server` alternativo | 415 ms | 2 | |
| `--upnp` | — | — | **NOT-APPLIED 3/3** |

Su questo percorso il **primo** tentativo vince sempre (`retries=0`, 2 candidati,
~410 ms), quindi non c'è niente da migliorare e le differenze fra 410 e 426 ms
sono rumore. La fase lo dice esplicitamente: *«il numero che conterebbe è un
braccio che trasforma un link al SECONDO tentativo (>30 s) in uno al primo»*, e
questo NAT non ne produce.

`--upnp` è marcato **NOT-APPLIED** e non è mai entrato in una mediana: nel log
non compare la riga `managed port mapping ENABLED`, cioè non c'è né PCP né IGD
da affittare su questo router. **È un'affermazione su questo router, non sulla
funzione** — e la distinzione fra «misurato e non serve» e «non misurabile qui»
è precisamente ciò che un braccio che va a mediana con 2383 ms avrebbe
cancellato.

### 45.4 `vpn_quic_timers` — il tempo di buio è **13,1 s** di default, ed è la coppia idle/keepalive a deciderlo

Con la sonda riparata (§45.6), tre ripetizioni, misura **a pacchetti** attraverso
il tunnel e non da una riga di log:

| braccio | `dead` mediano | campioni grezzi |
|---|---|---|
| **shipped** (keepalive 3 s / idle 10 s) | **13 104 ms** | 13110 · 13093 · 13104 |
| fast | **4 664 ms** | 4664 · 4674 · 3463 |
| slow | **40 800 ms** | 40803 · 40790 · 40800 |
| rtt-low (`BORE_DIRECT_QUIC_INITIAL_RTT_MS` basso) | 13 101 ms | 13101 · 13097 · 13102 |
| rtt-high | 13 100 ms | 13109 · 13100 · 10687 |

Da leggere così:

- **la coppia idle/keepalive È la manopola**, e lo è in modo netto: `fast` vale
  un terzo di `shipped`, `slow` più del triplo. La domanda che la fase si pone
  nel suo commento — «se il braccio veloce non batte quello spedito, allora la
  coppia non è ciò che chiude l'interruzione e la manopola è documentazione
  invece che controllo» — ha risposta: **è un controllo**;
- **l'RTT iniziale non c'entra nulla**: `rtt-low` e `rtt-high` leggono 13 101 e
  13 100 ms contro i 13 104 di `shipped`, cioè lo stesso numero. Coerente: quel
  parametro governa il primo Initial di una connessione nuova, non la scoperta
  che una viva è morta;
- la dispersione è **minuscola** (±0,1 % su `shipped`, ±0,03 % su `slow`), il
  che rende questi confronti fra i più solidi della campagna. Un `dead` è un
  timeout che scade, non una misura di rete;
- `ttd` (tempo per arrivare al diretto) è ~400 ms in **ogni** braccio: nessuno
  di questi parametri lo tocca.

**Ripetuto due volte a distanza di un'ora, con la stessa fase:** 13 104 e
13 095 ms per `shipped`, 4 664 e 4 670 per `fast`, 40 800 e 40 793 per `slow`.
Uno scarto dello **0,07 %** fra due esecuzioni separate — l'oggetto misurato è
un timeout che scade, non una rete.

**E il ritorno al diretto, che la prima esecuzione non poteva misurare** (§45.6:
`ws_path` non riconosceva la riga di fallback e rispondeva `direct` per sempre,
quindi `back` leggeva ~30 ms):

| braccio | `dead` | `back` | `dead + back` |
|---|---|---|---|
| shipped | 13 095 | 16 127 | 29,2 s |
| fast | 4 670 | 24 180 | 28,9 s |
| slow | 40 793 | 2 041 | 42,8 s |
| rtt-low | 13 102 | 16 127 | 29,2 s |
| rtt-high | 13 103 | 16 130 | 29,2 s |

La somma è la lettura giusta, ed è **quantizzata dalla griglia di ritentativo**
(`DIRECT_RETRY_INTERVAL`, 30 s, ancorata al pairing): tre bracci su cinque
sommano a 29,2 s. `back` da solo non è una proprietà del braccio — dice solo
dove, dentro la griglia, è caduta la rimozione della regola; ed è esattamente
quello che la nota della fase avvertiva. Ma **`back` non è tempo di
interruzione**: durante quei secondi il tunnel funziona, sul relay caldo. Il
costo visibile all'utente è `dead`, e solo `dead`.

**Il default non viene cambiato**, e la ragione è quella di sempre in questa
campagna: 13,1 s è il costo nel caso peggiore di un percorso diretto che muore,
ma abbassare `idle` significa dichiarare morto un percorso che ha solo avuto una
pausa — e su un percorso mobile o radio quella pausa è comune. La manopola
esiste già (`BORE_DIRECT_QUIC_IDLE_MS` / `_KEEPALIVE_MS`) ed è ora **misurata**:
un operatore che sa di avere un percorso stabile può comprare 8 secondi.

### 45.5 `vpn_carriers` — sul **relay** i carrier fanno male a un flusso singolo; sul **diretto** non fanno niente

Tredici celle, tre ripetizioni, ogni cella un rapporto contro un controllo nudo
campionato nella **stessa** ripetizione (V-9).

**Percorso diretto — stabile e vicino al tetto:**

| cella | mediana | campioni |
|---|---|---|
| direct/c1/f4 | **0,948** | 0,950 · 0,896 · 0,948 |
| direct/c4/f4 | **0,954** | 0,949 · 0,958 · 0,954 |
| direct/c1/f8 | **0,947** | 0,947 · 0,941 · 0,952 |
| direct/c4/f8 | **0,956** | 0,957 · 0,956 · 0,953 |

Il percorso diretto consegna **~95 % della linea nuda** e i carrier sono
**neutri** (0,948 contro 0,954, con escursioni dell'1 %). Non contraddice V-13
— che misurava 4 carrier consegnare *meno* — perché quella misura aveva **un
solo flusso interno**: con il pinning per flusso (BW-F2) un carrier in più non
può aiutare un flusso solo, e qui con 4 e 8 flussi non c'è comunque margine da
recuperare, essendo già al 95 %.

**Percorso relay — i carrier costano, e a flusso singolo la cosa è netta:**

| cella | mediana | campioni |
|---|---|---|
| relay/c1/f1 | **0,824** | 0,840 · 0,792 · 0,824 |
| relay/c2/f1 | 0,286 | 0,286 · 0,528 · **0,164** |
| relay/c4/f1 | 0,440 | 0,274 · 0,440 · 0,441 |

A **un flusso** i tre insiemi **non si sovrappongono**: il massimo di c2 (0,528)
e quello di c4 (0,441) stanno entrambi sotto il **minimo** di c1 (0,792). Questa
è una separazione vera, non una differenza fra mediane — ed è il criterio che
§42 ha insegnato a pretendere.

Il meccanismo è noto e documentato: il relay distribuisce **per datagramma** in
round-robin sulle N coppie di substream (DEC-7), quindi un singolo flusso TCP
interno arriva **fuori ordine** e legge il riordino come perdita. È esattamente
BW-F2, il difetto corretto sul percorso **diretto** con il pinning per flusso e
deliberatamente **non** corretto sul relay (il pinning richiederebbe di
ridimensionare la finestra di replay, DEC-10).

**A 4 e 8 flussi, invece, la misura non separa niente:**

| cella | campioni |
|---|---|
| relay/c1/f8 | 0,916 · 0,375 · 0,382 |
| relay/c2/f8 | 0,350 · 0,517 · 0,388 |
| relay/c4/f8 | 0,381 · 0,485 · 0,374 |

Gli intervalli si sovrappongono quasi per intero e la cella c1/f8 è addirittura
**bimodale** (0,916 contro due valori a 0,38). Qui la risposta onesta è **non
citabile**, come per il ladder pubblico a n≥4.

E il confronto fra le due metà della tabella è esso stesso un risultato: le
escursioni del **diretto** stanno all'1 %, quelle del **relay** arrivano a
**3,2×** sulla stessa cella. Il percorso relay non è solo più lento su questa
linea: è **molto più variabile**, e ogni sua percentuale va letta con quella
dispersione accanto.

**Conclusione operativa:** `--carriers` sul VPN resta a **1**, e la nota di
CLAUDE.md («raramente aiuta un VPN») si rafforza in «su relay con un flusso
singolo **danneggia**, di un fattore 2–5».

### 45.6 Le due fasi non misuravano il tunnel: misuravano sé stesse

Vale la pena isolare perché §45.4 e §45.5 hanno dovuto essere rifatte, perché è
il difetto più istruttivo dell'intera finestra.

Entrambe puntavano `$A_PEER` — `10.77.0.2` — che è l'indirizzo overlay **di
questa workstation**: il connector si alza con
`--vpn-addr $B_ADDR --vpn-peer-addr $B_PEER`, quindi `$A_PEER` è il nome che il
*listener* dà a **noi**. Tutte le altre fasi VPN usavano già `$B_PEER`.

Conseguenze, opposte e ugualmente invisibili:

- `vpn_quic_timers` verificava la vita del tunnel con `ping $A_PEER`. **Un ping
  al proprio indirizzo di TUN è risposto dallo stack locale senza che un
  pacchetto entri mai nel tunnel.** La sonda riusciva quindi sempre: con il
  tunnel vivo, morto, o mai costruito. Tutti e 15 i bracci hanno riportato
  `NEVER-SILENT(blackhole did not bite)` — e il blackhole mordeva benissimo.
- `vpn_carriers` faceva `iperf3` **contro sé stessa**, non trovava server e
  riceveva la stringa `FAILED`. Che la guardia della cella non intercettava
  (verificava `0|0.0|""`), così `FAILED` finiva nel calcolo del rapporto, dove
  `awk` produce **0.000** — e *quello* entrava nella mediana. `med()` rifiutava
  il `FAILED` nella colonna dei Mbit/s e non poteva rifiutare lo zero accanto.

Un'ipotesi smentita, che vale la pena registrare perché stava per essere
scritta: la prima lettura di `NEVER-SILENT` è stata «il fallback senza cuciture
di DEC-2 funziona così bene che nessun pacchetto si perde». Lusinghiera, e
falsa. La distinzione resta però nel codice — se il tunnel non tace, la fase ora
**chiede al server** se il percorso è passato a relay, e solo allora `dead=0` è
una misura invece che un guasto.

**La domanda da farsi davanti a un controllo che passa sempre è la stessa che si
fa davanti a uno zero: cosa lo farebbe fallire?**

---

## 46. `ws_conns_var` — perché il ladder pubblico non si ripete a n≥4: due dei tre candidati sono **esclusi**, e il terzo non è dove lo cercavamo

§42 aveva chiuso con un debito preciso: la stessa cella ripetuta **15 volte**
invece di 3, su un solo gradino, con i contatori di allowance letti come delta
attorno a ogni ripetizione. Finché non girava, il ladder pubblico andava citato
fino a n=2. Questa è quella fase.

Disegno: due bracci (relay, `--udp`), la cella sotto esame a **n=4**, il
controllo a **n=1** *dentro la stessa esecuzione* (una ripetizione su tre),
460 MiB per gradino, 75 s di raffreddamento, ordine dei bracci alternato fra
ripetizioni. 40 celle, 3387 s. Baseline nuda presa prima e dopo: **924 → 928
Mbit/s in download, 733 → 732 in upload** — la linea non si è mossa, quindi
nulla di quanto segue è deriva.

E una cosa in più che §42 non aveva chiesto: oltre ai contatori di allowance,
ogni cella legge **`steal` da `/proc/stat`** su entrambi gli host. «Scheduling
dell'istanza» non è una metafora: su un'istanza condivisa la decisione
dell'hypervisor di far girare qualcun altro il kernel ospite la *riporta*, nel
campo 9 della riga `cpu`. Una campagna che elenca lo scheduling fra i candidati
e poi non legge l'unico contatore che lo dichiara non ha guardato.

### 46.1 La premessa, misurata prima di spendere l'ora

Tutto ciò che segue è un ragionamento su **delta**, e vale solo se i contatori
che bracciano un trasferimento appartengono a quel trasferimento. Il server
porta anche i tunnel vivi dell'operatore, quindi è una domanda vera.

Controllo fuori banda, sei campioni da 8 s presi dentro i raffreddamenti della
fase, con nulla di nostro in volo (`pub_ws_conns_var_idle_control.out`):

```
  idle 8s: in+0 out+0 pps+0      (x5)
  idle 8s: in+0 out+0 pps+13     (x1)
```

**0,27 eventi/s di linea di fondo**, e zero su entrambi i contatori di banda.
Contro i ~4 800/s di una cella `--udp` carica: **quattro ordini di grandezza**.
L'attribuzione è misurata, non assunta.

### 46.2 La dispersione, con il controllo dentro la stessa esecuzione

| braccio | n | min | mediana | max | escursione | rip. |
|---|---|---|---|---|---|---|
| relay | 1 | 106,37 | 106,63 | 108,25 | **1,8 %** | 5 |
| quic | 1 | 71,87 | 106,70 | 107,38 | 33,3 % | 5 |
| quic | 4 | 71,76 | 88,67 | 107,08 | **39,8 %** | 15 |
| relay | 4 | 54,73 | 79,23 | 107,36 | **66,4 %** | 15 |

**§42.3 regge, e ora è misurato dentro una sola esecuzione invece che fra due
esecuzioni a ore di distanza.** Il relay a una connessione si ripete entro
l'1,8 %; a quattro arriva al 66,4 %. Il gradino *è* la variabile.

Il 33,3 % della riga `quic n=1` non è un'eccezione a quella conclusione: è
**una cella sola**, e ha una causa con un nome. Le altre quattro stanno fra
106,49 e 107,38 — **0,8 %**. Vedi §46.3.

### 46.3 `steal`: colto sul fatto, e non è dove serviva

Le tre celle più lente di ogni serie, con tutto ciò che era vero attorno a
*quel* trasferimento:

| braccio | n | MiB/s | srv_in | srv_pps | srv_busy | **srv_steal** | vm_busy |
|---|---|---|---|---|---|---|---|
| quic | 1 | **71,87** | 0 | 23 | 54,9 % | **21,55 %** | 9,6 % |
| quic | 1 | 106,49 | 0 | 3094 | 37,3 % | 0,00 % | 11,7 % |
| quic | 1 | 106,70 | 0 | 4115 | 38,4 % | 0,00 % | 12,8 % |
| relay | 4 | **54,73** | 135 | 5 | 11,7 % | 0,00 % | 4,7 % |
| relay | 4 | 58,55 | 216 | 51 | 10,2 % | 0,11 % | 5,3 % |
| relay | 4 | 65,18 | 206 | 65 | 10,9 % | 0,06 % | 6,3 % |

L'unica cella lenta del controllo `quic n=1` è **l'unica cella dell'intera
esecuzione con `steal` a 21,55 %**; tutte le altre stanno fra 0,00 e 0,26 %.
Quindi: lo scheduling dell'istanza **esiste**, è misurabile, e costa il 33 % del
throughput quando capita — ma è capitato **una volta su quaranta, e a n=1**.
Non è il meccanismo di n≥4.

### 46.4 Il bucket di allowance è ESCLUSO, e il colpo d'occhio diceva il contrario

Correlazione di rango (Spearman) fra il *rate* di una cella e ciascun candidato,
sulla stessa cella ripetuta:

| braccio | n | srv_in | srv_pps | srv_busy | srv_steal | vm_busy | rip. |
|---|---|---|---|---|---|---|---|
| quic | 4 | **+0,04** | +0,06 | +0,95 | −0,55 | +0,97 | 15 |
| relay | 4 | **−0,12** | +0,21 | +0,84 | +0,16 | +0,95 | 15 |

`srv_in` non ha **nessuna** associazione con il rate a n=4 (−0,12 e +0,04). E il
segno, dove si vede, è quello sbagliato per un'ipotesi di strozzatura: le due
celle `relay n=1` **più veloci** dell'esecuzione (108,24 e 108,25 MB/s, cioè
piena linea) portano `in+235` e `in+430`, mentre le celle `relay n=4` lente
portano `in+135`/`in+206`/`in+216`. **Il contatore segue il traffico, non lo
strozza.**

Vale la pena dire come è stato preso questo punto, perché è una regola e non un
aneddoto. Alla prima ripetizione avevo letto a occhio `65,18 → in+206`,
`54,73 → in+135`, `79,23 → in+13` e concluso «i lenti sono gli strozzati».
Su quindici ripetizioni il rho dice −0,12. **Leggere una correlazione dalla
pagina è il modo in cui un lettore trova il pattern che è andato a cercare**:
l'analisi è per questo uno script (`pub/analyze_conns_var.py`) che legge
l'output della fase e stampa rho accanto a `n`, e rifiuta di leggerlo sotto
n=6.

### 46.5 E la CPU segue il rate, non lo causa — il che restringe davvero

`srv_busy` e `vm_busy` correlano **+0,84…+0,97** col rate su entrambi i bracci.
È la direzione attesa se la CPU è *conseguenza* del throughput: più byte, più
lavoro. Il fatto decisivo è nella tabella di §46.3: le celle `relay n=4` più
lente girano con il server al **10–12 %** di un core e la VM al **4,7–6,3 %**.

Quindi a n=4, nella cella lenta, **nessuno dei due host misurati sta facendo
qualcosa**. Il trasferimento è lento mentre entrambe le estremità note sono
ferme.

### 46.6 Il verdetto su §42.4, e il buco che ha aperto

I tre candidati che §42.4 elencava senza misurarne nessuno:

| candidato | esito a n≥4 |
|---|---|
| 3. micro-bursting dell'allowance ENA | **ESCLUSO** — rho −0,12/+0,04; le celle più veloci sono quelle che lo toccano di più |
| 2. scheduling dell'istanza | **ESCLUSO a n≥4** — rho +0,16/−0,55 con le celle lente a `steal` 0,00–0,31 %. Confermato *esistente*, ma a n=1 e una volta sola |
| 1. percorso di accept pubblico del server sotto concorrenza | **non escluso, ma indebolito**: il server è al 10–12 % di un core proprio nelle celle lente |

**Il ladder pubblico si può ora citare oltre n=2 dichiarando l'escursione
accanto al valore**, che è la forma in cui §42.5 aveva già detto che andava
citato. Quello che NON si può ancora dire è *perché* la cella a n=4 vari del
66 %.

E misurando questo è emerso un buco che nessuna fase di questa campagna aveva:
**bracciamo il server e la VM, e nessuno dei due è il ricevente.** In un
download i byte finiscono su **questa workstation**, la cui NIC, il cui
softirq e le cui code di ricezione TCP possono limitare il rate quanto
qualunque cosa a monte — e nessuna fase le ha mai lette attorno a un
trasferimento. `origin_cpu` aveva misurato la CPU del client come percentuale,
che risponde a «era saturo» e non a «ha buttato via qualcosa».

`pub/udp_pktsize.sh` fa girare **lo stesso identico snapshot su tutti e tre gli
host**, allo stesso n=4: è la fase che chiude questo buco, ed è anche la fase
nata dalla scoperta di §46.7.

### 46.7 I due bracci inciampano in LIMITI DIVERSI dello stesso carico

Non è un dettaglio della cella lenta: è vero in ogni ripetizione.

```
  n=4 relay   in+206  out+0  pps+65        <- bucket di BANDA
  n=4 quic    in+0    out+0  pps+23957     <- bucket di PACCHETTI
```

Lo stesso payload, negli stessi secondi, e i due trasporti toccano contatori
d'istanza **diversi**: il relay fa doppio transito sul server come flusso di
byte e si vede sulla banda in ingresso; il percorso diretto si vede sui
**pacchetti** — gli stessi byte in datagrammi molto più numerosi e più piccoli.

Un limite di pacchetti al secondo è un'affermazione sulla **dimensione del
pacchetto**, e su un percorso QUIC la dimensione del pacchetto è una cosa che
il prodotto sceglie. È l'unica cosa emersa da questa fase che possa implicare
una modifica al codice, e per questo `udp_pktsize` gira **subito dopo** e non
alla fine.

## 47. `udp_pktsize` — il percorso diretto spende **l'11–19 % di pacchetti in più** per lo stesso payload; ma il criterio che la fase si era data **non è decidibile** con i contatori che ha usato

§46.7 aveva chiuso con una frase impegnativa: la dimensione del pacchetto, su un
percorso QUIC, è una cosa che **il prodotto sceglie**, quindi è l'unica cosa
emersa da §46 che possa implicare una modifica al codice. `pub/udp_pktsize.sh`
è nato per deciderla, e si era dato un criterio esplicito, scritto dentro la
fase stessa prima di vedere un solo numero:

> media in ingresso vicina a **1200 B** sul braccio quic ⇒ quinn non ha mai
> alzato la dimensione del datagramma oltre il valore iniziale conservativo, e
> un quinto del budget di pacchetti se ne va in niente — **si cambia il codice**.
> Vicina a **~1450** ⇒ la discovery ha funzionato e il resto è framing QUIC più
> acknowledgement, cioè il prezzo del trasporto e non un difetto.

La colonna ha stampato **1135 B**. Secondo quel criterio la risposta sarebbe
«si cambia il codice», e sarebbe **sbagliata**: quel numero non misura la
dimensione del datagramma. Questa sezione mostra che cosa misura davvero, che
cosa invece i dati dicono in modo esatto, e perché la domanda resta aperta.

### 47.1 Il controllo tiene, e va detto prima

Ogni ripetizione apre con una cella `idle` che bracketta sei secondi di niente:

```
  rep 1  idle  rx    106 pkts /    10370 B | tx  137 pkts /  16537 B
  rep 2  idle  rx    140 pkts /    12345 B | tx  144 pkts /  17121 B
  rep 3  idle  rx    134 pkts /    11935 B | tx  138 pkts /  16683 B
```

Da 106 a 140 pacchetti, contro le ~400 000 delle celle cariche: **tre parti su
diecimila**. Il traffico altrui del server non è dentro questi numeri. Il
controllo è la prima cosa da leggere, non l'ultima, perché se avesse letto
50 000 nessuna delle righe sotto sarebbe stata per-braccio.

### 47.2 Quello che è **esatto**: la differenza fra i bracci

La gamba server→workstation è TCP su **entrambi** i bracci, e si misura che è la
stessa cella per cella — non si assume:

```
  rep 1  ws rx  335222 (relay) vs 335170 (quic)   delta  -52 pkts  (-0,016 %)
  rep 2  ws rx  335068        vs 335173            delta +105 pkts  (+0,031 %)
  rep 3  ws rx  335091        vs 335269            delta +178 pkts  (+0,053 %)
```

Cinque centesimi di punto percentuale. Quindi **il flusso di ACK che la
workstation rimanda al server è comune ai due bracci**, e sparisce nella
sottrazione: la differenza fra i due `rx` del server è attribuibile *interamente*
alla gamba VM→server, che è l'unica cosa che cambia fra i bracci.

```
  rep   pacchetti in più   byte in più   %pkt     %byte   B per pacchetto in più
   1         +46 330       +4 454 116   +11,5 %   +0,88 %          96,1
   2         +71 642       +5 722 128   +19,5 %   +1,14 %          79,9
   3         +45 706       +4 356 303   +11,0 %   +0,86 %          95,3
```

Il segno è stabile 3 su 3: **il braccio diretto mette sulla gamba VM→server dall'11
al 19,5 % di pacchetti in più per lo stesso payload consegnato.**

E la forma di quel costo si legge nell'ultima colonna. Ogni pacchetto in più
porta con sé solo **80–96 byte** in più: non porta payload nuovo, porta
un'intestazione. Ottanta-novanta byte è esattamente un IP+UDP+intestazione
QUIC+tag AEAD, oppure un IP+TCP. Cioè: **gli stessi byte tagliati più fini**,
non byte aggiuntivi. In totale i byte crescono dello 0,86–1,14 %, i pacchetti
dell'11–19,5 %.

### 47.3 La dispersione della cella ripetuta (regola §42.5)

Un confronto fra celle va pubblicato con l'escursione della cella ripetuta,
altrimenti la differenza sopra non ha scala:

```
  relay  rx pkts   min 368 294   mediana 402 493   max 415 903   escursione 47 609  (11,8 % della mediana)
  quic   rx pkts   min 439 936   mediana 448 823   max 461 609   escursione 21 673  ( 4,8 % della mediana)
```

L'escursione del braccio relay (47 609) è **più grande della differenza mediana
fra i bracci** (46 330). Per lo stesso identico payload il relay si ripartisce
in un numero di pacchetti che varia del 12 % da una ripetizione all'altra —
segmentazione, pacing e finestra non si ripetono. Quindi:

- la **direzione** è solida (3 ripetizioni su 3, appaiate, ordine alternato);
- la **grandezza** non è fissata a n=3, e va letta come «fra l'11 e il 19,5 %»,
  mai come «46 330».

### 47.4 Quello che **non** è decidibile: la media assoluta in B/pkt

Il criterio della fase legge la colonna `in B/pkt` come se fosse la dimensione
del datagramma in arrivo dalla VM. Non lo è: `rx` del server somma **due**
flussi sulla stessa interfaccia — il payload dalla VM *e* gli ACK dalla
workstation — e il secondo non è misurato separatamente da nessuna parte.

Si può provare a risolverlo per sottrazione, e il tentativo **fallisce in modo
informativo**. Prendendo il payload che la workstation ha davvero ricevuto
(`ws rx bytes − 66·ws rx pkts`) e chiedendo che i pacchetti in ingresso al
server ci stiano sopra con almeno il frame ethernet minimo (60 B):

```
  cella        payload ws   budget di byte sopra il payload   pavimento minimo per i pkt rx   chiude?
  rep1 relay   483,71 MB              21,24 MB                        24,15 MB                 NO
  rep1 quic    483,71 MB              25,69 MB                        26,93 MB                 NO
  rep2 relay   483,69 MB              19,48 MB                        22,10 MB                 NO
  rep2 quic    483,76 MB              25,13 MB                        26,40 MB                 NO
  rep3 relay   483,69 MB              21,98 MB                        24,95 MB                 NO
  rep3 quic    483,73 MB              26,30 MB                        27,70 MB                 NO
```

Sei celle su sei: il server ha ricevuto **meno** byte di quanti ne servirebbero
per portare quel payload in quel numero di pacchetti, anche assegnando a ogni
pacchetto il minimo assoluto. Manca da 1,3 a 2,9 MB.

Non è un errore di misura: è la prova che una delle grandezze non è quello che
la legenda assume. Il payload che la workstation riceve **non** è il payload che
ha attraversato la gamba VM→server, perché la gamba server→workstation è TCP e
**ritrasmette** — byte che escono dal server due volte ed entrano una volta
sola. Con le ritrasmissioni incognite su entrambe le gambe e il rapporto di ACK
incognito (dipende dal coalescing GRO del ricevente, che nessuno qui misura),
il sistema ha più incognite che equazioni.

**Conclusione operativa: la colonna `in B/pkt` non decide il criterio di §47, e
la domanda «quinn ha alzato la dimensione del datagramma?» resta APERTA.** Il
difetto non è nel prodotto: è nello strumento, ed è la firma di questa campagna
al contrario — non uno zero che vuol dire che lo strumento ha fallito, ma **un
numero plausibile che non può fallire**, perché stampa sempre qualcosa di
credibile qualunque cosa stia succedendo.

E si noti che la differenza di §47.2 sopravvive comunque: è una sottrazione fra
due celle in cui tutto l'ignoto è comune. Quello che *non* sopravvive è la sua
scomposizione in «datagrammi più piccoli» contro «più ritrasmissioni» — due
diagnosi opposte che da questi contatori hanno lo stesso aspetto.

### 47.5 I due bracci toccano limiti diversi, e il diretto ne spende molto di più

```
  braccio   pps_allowance_exceeded (delta per ripetizione)
  relay       +130      +45      +20
  quic     +11 774  +30 139  +16 791
```

Da due a tre **ordini di grandezza**. È la conferma diretta di §46.7 su una fase
costruita apposta: lo stesso payload, negli stessi secondi, e il percorso
diretto consuma il bucket dei *pacchetti* mentre il relay non lo tocca.

E tuttavia il braccio quic è stato **più veloce** in ogni ripetizione
(99,34 / 100,59 / 106,84 contro 76,88 / 88,00 / 102,69 MiB/s). Lo shaping dei
pacchetti non gli è costato banda **in questa fase**. È un margine speso, non
una perdita incassata — e vale la pena distinguere le due cose, perché la
seconda si vede in un grafico e la prima no.

### 47.6 Ma la banda, qui, non dice niente — e va detto

La linea nuda di questa finestra è **924 Mbit/s in download**, cioè
**110,2 MiB/s**. Il braccio quic migliore ha letto 106,84 MiB/s: il **97 % della
linea di accesso della workstation**. Entrambi i bracci corrono contro il tetto
dell'accesso, non contro il trasporto.

Quindi il confronto di throughput di questa fase **non è un confronto fra
trasporti** e non va citato come tale (V-9: i rapporti valgono, gli assoluti
vanno qualificati con la linea — e qui la linea è il vincolo attivo). L'unica
colonna informativa di `udp_pktsize` è quella dei contatori di pacchetti, che è
poi la ragione per cui la fase esiste.

### 47.7 La correzione dello strumento: bracchettare il **mittente**

L'unica vista non confusa della dimensione del pacchetto è quella di chi lo
mette sul filo. I contatori `tx` della VM non hanno nessuno dei due problemi:
non sommano un secondo flusso, e contano prima che qualunque cosa a valle possa
fondere pacchetti insieme.

`pub/udp_pktsize.sh` ora installa lo **stesso** snapshot anche sulla VM e
bracchetta ogni trasferimento anche lì:

```
  vm tx  <pkts> pkts / <bytes> B = <B/pkt> | vm drops tx+<n>
```

con la colonna `VM tx B/pkt` in testa alla tabella delle mediane, prima di
`in B/pkt`, perché è quella che decide il criterio e l'altra no.

Resta una cosa che nemmeno questo bracket separa da solo — la segmentazione
hardware (TSO sul braccio relay, GSO UDP su quello quic) fa contare al mittente
un'unica voce per quello che sul filo diventano più pacchetti. Per questo la
ri-esecuzione registra anche `ethtool -k` dei due estremi: se l'offload è
attivo, il numero del mittente è un *limite inferiore* dichiarato come tale e
non una misura, e la fase deve dirlo invece di stampare una media.

**Stato: la fase è stata corretta e rimessa in coda.** I numeri di questa
sezione vengono tutti dalla **prima** esecuzione, conservata come
`out/eth/pub_udp_pktsize_r1.out`; la seconda riscrive `pub_udp_pktsize.out` e
chiuderà §47. La prima resta nell'albero di proposito: una campagna che
cancella l'esecuzione che l'ha ingannata non può più mostrare a nessuno perché
la regola esiste. Finché quel numero non c'è, la posizione di
questo documento è che **non si tocca il codice**: cambiare la dimensione del
datagramma sulla base della colonna `in B/pkt` significherebbe intervenire su una
misura che §47.4 ha appena dimostrato non essere quella grandezza.
