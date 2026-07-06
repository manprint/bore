# Vendored `russh` — per-channel head-of-line-blocking fix

This is an **unmodified copy of `russh` 0.62.1** (crates.io, checksum
`20d2039c…`) plus a single functional patch that fixes **per-channel
head-of-line (HOL) blocking** in the SSH session read loop. It is wired into
`bore` via `[patch.crates-io]` in the workspace root `Cargo.toml`, so every
`russh` dependency (direct and transitive) resolves to this tree.

## Why we carry it

`bore --ssh-gateway` embeds a `russh` **server**. Every proxied connection on a
public / vhost / secret `ssh -R`/`-L` tunnel is one SSH channel multiplexed
over the single client↔gateway TCP connection.

Stock `russh` forwards each inbound `CHANNEL_DATA` packet to that channel's
bounded mpsc with a **blocking `chan.send(..).await` inside the one session read
loop** (`server/encrypted.rs`), and replenishes the SSH receive window **on
receipt** (`session.rs::adjust_window_size`), decoupled from whether the
application actually consumed the bytes.

Consequence: a single slow or paused consumer — e.g. a **browser buffering a
video**, which stops reading its TCP socket for seconds at a time — fills that
one channel's mpsc, and the read loop then **parks the entire connection**. Every
*other* channel (every other browser/visitor on the same tunnel), plus
keepalives, starves until the slow consumer drains. Symptom in the field: a
second browser hitting the same tunnel hangs on "loading" until the first one is
closed. Raising `channel_buffer_size` only delays it (a continuously streaming
consumer keeps the buffer full indefinitely, and unbounded buffering just moves
the failure to OOM). The window-vs-drain decoupling makes a pure-config fix
impossible — the fix has to live in `russh`.

The native (yamux TCP) and QUIC/UDP bore paths do **not** have this bug (proper
per-stream flow control), so only the SSH gateway needed the change.

## The patch — upstream `Eugeny/russh#730`

Source: <https://github.com/Eugeny/russh/pull/730> ("fix: gate WINDOW_ADJUST on
consumption to fix per-channel HOL blocking"). The PR was **closed unmerged**,
so no released `russh` contains it; we port it here.

Mechanism:

- The session read loop no longer blocks. Inbound `CHANNEL_DATA` is delivered
  with `try_send`; on `Full` the message is queued in a bounded per-channel
  `ChannelBacklog` (`channels/mod.rs`) and **`WINDOW_ADJUST` is withheld** for
  that channel. The peer's SSH window for the stalled channel therefore drains
  to zero and *it* stops sending — while the loop stays live for every other
  channel and for keepalives.
- Window replenishment is split out of `adjust_window_size` into
  `record_received_data` (debit on receipt, no wire effect) +
  `replenish_receive_window` (emit `WINDOW_ADJUST`), and the latter is called
  from the **drain path** (`ChannelRx` consumer signals a shared `drain_notify`;
  the session loop drains the backlog and re-opens windows for fully-drained,
  non-closing channels). Backpressure is thus tied to actual consumption.
- The queue is bounded by the SSH receive window for a compliant peer (window
  withheld while a backlog exists ⇒ in-flight ≤ `window_size`). No data is
  dropped and no connection is force-closed — a slow consumer is simply
  throttled, exactly like OpenSSH's own server.
- `ChannelReadHalf` latches EOF locally (instead of closing the receiver) so a
  post-EOF backlog (`ExitStatus`/`Close`) still drains; `Session::drop` does a
  best-effort final drain.

`bore` only uses the `russh` **server**; the PR's symmetric **client**-side
changes are carried too, solely so the shared `session.rs` split compiles.

## Files changed vs pristine 0.62.1

`src/channels/mod.rs`, `src/channels/io/rx.rs`, `src/session.rs`,
`src/server/session.rs`, `src/server/mod.rs`, `src/server/encrypted.rs`,
`src/client/mod.rs`, `src/client/encrypted.rs`. Everything else is byte-for-byte
upstream 0.62.1. `git diff` against a fresh 0.62.1 checkout isolates the change.

## Regression coverage

- `crates/russh` in-tree unit tests (`ChannelBacklog` ordering / close-defer /
  closed-receiver) — run with `cargo test -p russh`.
- `tests/ssh_gateway_test.rs::t_ssh_hol1_slow_consumer_does_not_block_peers`
  (real `ssh` client, loopback): reader A stalls, reader B must still be served,
  A must resume.
- `scripts/ssh_gateway_test.sh` — `T-SSH-HOL-PUB` / `-VHOST` / `-SECRET` (netns,
  real network): same shape over all three ssh tunnel types.

## Re-applying on a `russh` version bump

The patch is preserved as a diff at `scripts/patches/russh-hol-730.diff` (base:
this vendored 0.62.1 tree). To move to a newer `russh`:

1. Replace `crates/russh` with the new pristine crate source.
2. `cd crates/russh && patch -p1 --fuzz=3 < ../../scripts/patches/russh-hol-730.diff`
   (or port by hand if the flow-control internals moved).
3. Rebuild + run all three regression suites above.
4. If upstream has since merged an equivalent fix, drop the vendored crate and
   the `[patch.crates-io]` entry entirely and rely on the release.
