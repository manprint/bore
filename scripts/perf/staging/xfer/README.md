# Transfer staging harness

Everything needed to re-take the `bore transfer` campaign against a real deployment. If you
are reading this a month later: you do not need to reconstruct anything — fill in `env.sh`,
deploy a binary to the VM, and run the stages below in order.

## Prerequisites

1. **`env.sh` outside the repository.** Copy `scripts/perf/staging/env.sh.example` to
   `~/.config/bore-perf/env.sh`, `chmod 600` it, and fill it in. Credentials are provided
   separately and are never committed. The harness looks at `$BORE_PERF_ENV`, then
   `~/.config/bore-perf/env.sh`, then `./env.sh`.
2. **A matching binary on the VM** at `~/bore`:
   ```bash
   cargo build --release
   scp -i "$BORE_SSH_KEY" target/release/bore "$BORE_VM_USER@$BORE_VM:~/bore"
   ```
   Both ends of a transfer are clients, so **both** must be the build under test. A stale
   `~/bore` silently measures the old code on one side of every run.
3. Nothing else. The server needs no redeploy for this campaign: `bore transfer` only uses
   it to broker the rendezvous and to relay, and neither changed.

## Topologies

`TOPO` names **where the two roles run**, and the bytes flow sender → listener:

| `TOPO` | sender (sends) | listener (receives) | what it measures |
| --- | --- | --- | --- |
| `ws-vm` | workstation | VM | the home **uplink** |
| `vm-ws` | VM | workstation | the home **downlink** |
| `vm-vm` | VM | VM | no home link at all — the server and the same-region path |

The direction is not cosmetic: on a consumer line the two halves are nothing alike, and the
campaign measured the relay winning one direction and the direct path winning the other.

## Stages

```bash
# X1 — bandwidth: one large file, --parallel sweep, both arms
TOPO=ws-vm MB=1024 PARS="1 2 4 8 16" ARMS="relay direct" bash scripts/perf/staging/xfer/xfer_bw.sh

# X2 — shape: same bytes in more files, with the durability policy as the A/B
TOPO=ws-vm MB=512 FILES="1 100 1000 5000 20000" PAR=8 FSYNC=on  bash scripts/perf/staging/xfer/xfer_shape.sh
TOPO=ws-vm MB=512 FILES="1 100 1000 5000 20000" PAR=8 FSYNC=off bash scripts/perf/staging/xfer/xfer_shape.sh

# X3 — state of the art: the same bytes, the same link, the tools people already use
DIR=up   MB=1024 FILES=1    PAR=8 bash scripts/perf/staging/xfer/xfer_sota.sh
DIR=up   MB=512  FILES=5000 PAR=8 bash scripts/perf/staging/xfer/xfer_sota.sh
DIR=down MB=1024 FILES=1    PAR=8 bash scripts/perf/staging/xfer/xfer_sota.sh
```

X3 does not use `TOPO`; it uses `DIR` (`up` = workstation → VM, `down` = VM → workstation),
because `scp` and `rsync` have no notion of two roles to place. Its `TOOLS` list defaults to
`link bore-direct bore-relay scp rsync tar-ssh`; drop names from it to shorten a run.

Read X3 for what it is. `scp`, `rsync` and `tar | ssh` need a routable host running `sshd`,
which is precisely the situation bore exists to handle the absence of — so the comparison is
not "who is faster at the same job", it is what bore's protocol costs against the best case
on the same link. The `link` row is what makes the table legible: it is one SSH channel to
`/dev/null`, no filesystem and no per-file protocol, so it is the ceiling every other row is
bounded by. Without it, six near-identical numbers cannot be told apart from six tools that
are all equally mediocre.

Knobs common to all: `GAP` (seconds between runs, default `COOL`=75 — a full burst budget
on a burstable instance), `REPS`, `ARM` (`relay`/`direct`), `XFER_LOG`.

## Reading the output

```
  arm     par  rep     wall_s        MB/s     xfer_s       MiB/s  path
  relay   4    1        12.72       80.50       12.0        85.5  relay
```

* `wall_s` / `MB/s` — end-to-end, **including the rendezvous**. On the direct arm that is
  the hole punch, which the secret campaign measured at 37–53 ms when the check round ends
  cleanly and ~1.1 s when it does not (S-5).
* `xfer_s` / `MiB/s` — what bore itself reports having moved, excluding setup.
* `path` — read from the **sender's** log, because the sender is the secret consumer and is
  the only party that knows how the bytes travelled (S-1). A `--udp` run that failed to
  punch reports `relay`: never assume the arm from the flag.

Quoting only the wall charges the transport for the handshake; quoting only bore's figure
hides a slow rendezvous from the operator. Both columns are there on purpose.

## Rules this harness follows (and you must too)

* **Never `pkill bore`.** The deployment carries the operator's own live tunnels. Local
  processes are killed by the PID they were started with; remote ones by their per-run
  `--transfer-id`, which is minted per run and exists nowhere else on the box.
* **One stage at a time.** Two stages share the VM, the server and the home link.
* **Do not rebuild `target/release/bore` while a stage is running** — the harness invokes it
  per run, so a rebuild swaps the binary between cells of the same table.
* Fixtures are incompressible (`mkfixture.py`). Nothing on bore's path compresses, but the
  filesystem underneath can, and a sparse or compressible fixture makes the sender's read
  cheaper than the transfer it is meant to measure. Fixtures are cached per (MiB, files) and
  reused; delete `$BORE_PERF_WORK/xfer` and `~/xferwork` on the VM to force a rebuild.

## Local counterpart

`scripts/perf/transfer_scale.sh` runs the same shape sweep on loopback, with CPU columns and
no deployment. Use it to attribute cost (there is no network in the way); use this directory
to find out what the network does to the answer. `LIS_EXTRA` / `SND_EXTRA` append flags to
either side, so the fsync A/B is the same experiment in both places.
