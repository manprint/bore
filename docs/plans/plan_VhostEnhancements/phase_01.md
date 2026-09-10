# Phase 01 — vhost control liveness: heartbeat + reaper

> **Motivating findings:** F-1 (reproduced deterministically), F-10 (the fix
> already exists in this binary, on the SSH path)
> **Kind:** stability defect | **Effort:** small — mechanical mirror
> **Prerequisite:** none

## The defect, precisely

A vhost provider that is **alive at TCP level but dead at application level** —
a frozen process, a suspended laptop, a wedged runtime — keeps its subdomain
registered indefinitely. Measured at t+180 s with `relay_tx_bytes` frozen since
t+20 s, on **both** transports, and re-registration of the same label is
rejected for as long as the ghost holds it (F-1, §2.13 G1).

The mechanism is the one `CLAUDE.md` already documents for secret tunnels: the
control channel is a yamux substream, so a half-open peer is invisible to both
`send` (buffers into yamux) and `recv` (blocks forever). The RAII `Deregister`
guard therefore never drops.

The identical scenario on the **SSH gateway** releases the label between t+20 s
and t+40 s and accepts re-registration, because I-SSH10 gave it a bounded
channel open plus 2-strike session eviction (F-10). So the native vhost path is
the only one of the three without a liveness mechanism.

## Current code, exactly as it stands

`serve_vhost_provider` (`src/vhost.rs:603`) ends in this loop (line 816):

```rust
let mut hb = interval(HEARTBEAT_INTERVAL);          // 500 ms, src/server.rs:75
hb.set_missed_tick_behavior(MissedTickBehavior::Delay);
loop {
    tokio::select! {
        _ = hb.tick() => {
            if control.send(ServerMessage::Heartbeat).await.is_err() {
                return Ok(());
            }
        }
        message = control.recv() => {
            match message? {
                Some(ClientMessage::HelloVhost { .. }) | /* … */ => {
                    warn!(%subdomain, "unexpected message from vhost provider");
                }
                Some(ClientMessage::VhostUdpRenew { subdomain: renew_subdomain }) => { /* … */ }
                Some(_) => warn!(%subdomain, "unexpected message from vhost provider"),
                None => return Ok(()),
            }
        }
        joined = crate::pool::recv_carrier(carrier_rx.as_mut()) => { /* … */ }
    }
}
```

There is no `last_recv`, no deadline and no reap. Compare `serve_provider`
(`src/secret.rs:423-437`), which is the same loop **with** the mechanism:

```rust
// Checked on every heartbeat tick (≤ HEARTBEAT_INTERVAL granularity) rather
// than via `timeout(recv)` — the latter would reset every time the heartbeat
// branch wins the `select!`, so it could never reach `ctrl_timeout`.
let mut last_recv = TokioInstant::now();
loop {
    tokio::select! {
        _ = heartbeat.tick() => {
            if control.send(ServerMessage::Heartbeat).await.is_err() {
                return Ok(());
            }
            if last_recv.elapsed() >= ctrl_timeout {
                warn!(%id, timeout = ?ctrl_timeout,
                    "secret provider control idle; reaping (peer wedged/abandoned)");
                return Ok(());
            }
        }
        message = control.recv() => {
            last_recv = TokioInstant::now();
            match message? {
                // Liveness ping; the deadline reset above is its only effect.
                Some(ClientMessage::Heartbeat) => {}
                /* … */
            }
        }
    }
}
```

The client side already exists too (`src/client.rs:1009-1019`), gated by
`Client::sends_ctrl_heartbeat` (`src/client.rs:82`), whose doc comment currently
reads *"public and vhost tunnels keep the legacy heartbeat-free path"*. Vhost
providers go through the same shared `client::listen`, so the branch is present
and simply disabled for them.

## The compatibility problem this phase must solve

Turning the reaper on unconditionally would be a **regression, not a fix**. A
vhost provider running an older binary never sends `ClientMessage::Heartbeat`,
so a healthy idle tunnel would be reaped every 60 s — including the operator's
own long-lived tunnels (`tennis`, `tennis1`, `dufspcloud` were live throughout
the campaign). The secret path accepted this because clients were upgraded with
the server; vhost providers are long-lived and frequently run older binaries.

**DEC-VE2:** reap **only** a provider that declared it sends heartbeats, via an
additive `#[serde(default)]` capability flag on `HelloVhost`. A provider that
does not declare it keeps today's exact behaviour and is never reaped. F-1 then
persists only for un-upgraded clients — strictly better than killing healthy
ones. This matches how every other wire change in this codebase shipped
(`carriers`, `udp`, `webserver_log`, `auto_reconnect`, `backend_tls` are all
additive `#[serde(default)]` fields on this same message).

---

## 1.1 — Wire: additive capability flag on `HelloVhost`

**Files:** `src/shared.rs` (the `HelloVhost` variant, line 1173)

Add, at the **end** of the variant's fields, following the existing pattern:

```rust
/// Whether this provider sends periodic `ClientMessage::Heartbeat` frames on
/// the control substream. When `false` the server keeps the legacy
/// heartbeat-free path and NEVER applies its recv-deadline reaper to this
/// tunnel (DEC-VE2): an old client cannot send heartbeats, and reaping it
/// would kill a healthy idle tunnel every `vhost_ctrl_timeout`.
/// `#[serde(default)]` keeps the wire format backward-compatible (an old
/// client omits it ⇒ reads as `false`).
#[serde(default)]
ctrl_heartbeat: bool,
```

**Invariants**
- Append at the end. `ClientMessage` is serde_json on the wire, but the
  project's convention is append-last and the `MAX_FRAME_LENGTH` worst-case
  reasoning in `CLAUDE.md` assumes it.
- `ctrl_heartbeat == false` ⇒ every downstream path byte-identical to today.

**Gate:** a wire round-trip unit test in `src/shared.rs` `mod tests`
(alongside `t_wire_heartbeat_roundtrip`) proving (a) a `HelloVhost` serialized
**without** the field deserializes with `ctrl_heartbeat == false`, and (b) a
round-trip with it `true` preserves it.

---

## 1.2 — Server: a `vhost_ctrl_timeout` builder, plumbed through

**Files:** `src/server.rs` (fields ≈ 351-355, defaults ≈ 476-477, builders
≈ 486-493, call site of `serve_vhost_provider`)

Mirror the two that exist:

```rust
// field, beside secret_ctrl_timeout / ssh_jump_ctrl_timeout
vhost_ctrl_timeout: std::time::Duration,
// default, beside the other two
vhost_ctrl_timeout: crate::secret::SECRET_CTRL_TIMEOUT,     // 60 s
// builder, for tests to lower it
pub fn vhost_ctrl_timeout(mut self, timeout: std::time::Duration) -> Self { … }
```

Pass it into `serve_vhost_provider` as a new `ctrl_timeout: Duration`
parameter, next to the existing `max_carriers` / `carriers` arguments.

**Decisions**
- **60 s / 20 s**, identical to `SECRET_CTRL_TIMEOUT` and
  `CTRL_CLIENT_HEARTBEAT`, and in parity with the SSH gateway's I-SSH3
  (keepalive 20 s / reaper 60 s). Three mechanisms in one binary with three
  different deadlines would be an operability trap.
- A builder rather than a CLI flag: the secret and SSH-jump equivalents are
  builder-only, used to lower the deadline in tests. No operator has asked to
  tune it and an untunable constant is one less thing to misconfigure.

**Gate:** compiles; `cargo test` unchanged (no behaviour yet).

---

## 1.3 — Server: track `last_recv`, reap on the tick, accept `Heartbeat`

**Files:** `src/vhost.rs`, the loop at the tail of `serve_vhost_provider`

Three edits, mirroring `src/secret.rs:423-437`:

1. `let mut last_recv = TokioInstant::now();` before the loop, carrying the
   same explanatory comment about why the check is on the tick.
2. In the `hb.tick()` arm, after the send:
   ```rust
   if ctrl_heartbeat && last_recv.elapsed() >= ctrl_timeout {
       warn!(%subdomain, timeout = ?ctrl_timeout,
           "vhost provider control idle; reaping (peer wedged/abandoned)");
       return Ok(());
   }
   ```
3. In the `control.recv()` arm: `last_recv = TokioInstant::now();` as the first
   statement, and a new match arm
   `Some(ClientMessage::Heartbeat) => {}` with the comment
   *"Liveness ping; the deadline reset above is its only effect."*

Returning `Ok(())` drops `_guard` (`Deregister`), which is what releases the
subdomain, the pending-UDP slot and the admin row. No new teardown code.

**Invariants**
- **DEC-VE3:** the check lives in the `hb.tick()` arm. Never wrap
  `control.recv()` in a `timeout`.
- `ctrl_heartbeat == false` ⇒ the condition short-circuits and the loop is
  byte-identical to today.
- `ClientMessage::Heartbeat` currently falls into `Some(_) => warn!(…)`, so
  without edit 3 an upgraded client would log a warning 3 times a minute per
  tunnel. Both halves ship together.

**Gates (must be red-checked)**
- `vhost_wedged_provider_is_reaped_and_label_freed`: a provider that declares
  `ctrl_heartbeat` and then stops reading/writing is reaped within
  `vhost_ctrl_timeout` (lowered via the builder), the subdomain becomes
  re-registrable, and the admin row disappears.
- `vhost_provider_with_heartbeats_is_never_reaped`: heartbeats keep a healthy
  idle provider alive well past the deadline. Mirrors
  `secret_consumer_survives_with_heartbeats` (`tests/secret_test.rs:793`).
- `vhost_legacy_provider_without_capability_is_never_reaped`: the zero-regression
  gate for DEC-VE2 — an idle provider with `ctrl_heartbeat == false` survives
  past several deadlines.
- **Red-check:** with edit 2 reverted, the first test must fail. Note
  `[[feedback-inprocess-test-false-pass]]`: an in-process test can false-pass a
  liveness bug because dropping a future *looks* like a dead peer. The wedge
  must be produced by a peer that stops answering while its transport stays
  alive — a mock control stream whose `poll_read` returns `Pending` forever
  while `poll_write` keeps accepting, which is the shape the secret tests use.
  The end-to-end proof is `scripts/perf/vhost_registration_leak_repro.sh`.

---

## 1.4 — Client: vhost providers send heartbeats and declare it

**Files:** `src/client.rs` — `new_vhost_provider` (≈ 541),
`new_vhost_provider_with_udp` (≈ 572), the `sends_ctrl_heartbeat` field doc
(≈ 79-82), and the `HelloVhost` construction site

- Set `sends_ctrl_heartbeat: true` in both vhost provider constructors. The
  `select!` branch at `src/client.rs:1016` then fires every
  `CTRL_CLIENT_HEARTBEAT` (20 s) with no further change.
- Send `ctrl_heartbeat: true` in the `HelloVhost` message.
- Update the field's doc comment: it currently says vhost keeps the
  heartbeat-free path, which this subphase makes false.

**Invariant:** public tunnels (`bore local`) are untouched and keep
`sends_ctrl_heartbeat: false`. This phase is vhost-only; the public path has no
reaper to feed and F-1 was not reproduced there.

**Gate:** an end-to-end test in `tests/vhost_test.rs` where a real client
registers, sits idle across more than one lowered `vhost_ctrl_timeout`, and
still serves a request afterwards. This is the test that would catch a client
that declares the capability but fails to actually send — the worst possible
combination, since it converts a healthy tunnel into a reaped one.

---

## 1.5 — SSH-gateway path: confirm no double mechanism

**Files:** `src/sshgw.rs` (read-only expected)

An SSH-registered vhost label is served through the gateway's own
`ConnState`/I-SSH10 machinery, not through `serve_vhost_provider`, so it must
neither gain a second reaper nor lose its existing one.

- Confirm the SSH vhost registration path does not reach
  `serve_vhost_provider`'s loop; if it shares any of it, `ctrl_heartbeat` must
  be `false` for SSH providers so I-SSH10 stays the single mechanism.
- F-10 measured the SSH path releasing at 20–40 s. That must not change.

**Gate:** `cargo test --features ssh-gateway` unchanged, and
`sudo -n /abs/path/scripts/ssh_gateway_test.sh` still green (exact-path sudo
only — `sudo bash scripts/...` prompts and must not be used).

---

## 1.6 — Documentation

**Files:** `README.md`, `docs/VHOST_PLAN.md` or `docs/vhost/`, `CLAUDE.md`

- `README.md`: the vhost section states that a provider sends a control
  heartbeat every 20 s and that a server reaps a silent registration after
  60 s, and that an older client is never reaped (so a stuck label on an old
  client still needs a manual restart).
- `CLAUDE.md`: extend the *Secret control liveness (zombie-entry reaper)*
  invariant to say vhost now shares it, including DEC-VE2 and DEC-VE3, so the
  next reader does not "simplify" the tick check into a `timeout(recv)`.

## Phase acceptance

1. All four unit/e2e gates green, each red-checked.
2. `scripts/perf/vhost_registration_leak_repro.sh` shows the label released and
   re-registrable within 60 s on the native transport — against the campaign's
   measured baseline of *still held at t+180 s*.
3. An old client (previous release binary) registers, idles 5 minutes, and is
   still serving — the DEC-VE2 zero-regression proof, and the one result that
   cannot be obtained from a unit test.
4. Full `cargo test` + `--features vpn` + `--features ssh-gateway` regression,
   zero failures.
