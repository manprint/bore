# Difetto aperto: una connessione di controllo persa per ogni riconnessione

**Stato:** misurato sul campo, **meccanismo non ancora confermato**. Il red-check
qui sotto va eseguito *prima* di scegliere una correzione — se mostra che il
server chiude davvero e che a tenere il socket è altro, tutte e tre le opzioni di
fix rispondono alla domanda sbagliata.

**Perché è qui e non in uno scratchpad:** è l'unico difetto di prodotto ancora
aperto della campagna VPN, e la campagna deve poter essere ripresa a freddo fra
un mese. Contesto e stato generale in
`docs/performance/VPN_CAMPAIGN_HANDOFF.md`; misure in
`docs/performance/ETH_RERUN_EVIDENCE_2026-09-12.md`.

**Quando eseguirlo:** nella finestra di build (P5 in
`docs/performance/ETH_CAMPAIGN_PLAN.md`), mai mentre una fase sta misurando —
compilare occupa la CPU, e la CPU fa parte dello strumento.

---

## What is measured (field, not theory)

`fdprobe2.sh`, workstation ↔ staging, VPN connector with `--auto-reconnect`,
listener killed per cycle with the narrow per-run pattern:

```
fresh link          : ctrl_conns=1 fds=12
after reconnect 1   : ctrl_conns=2 fds=13
after reconnect 2   : ctrl_conns=3 fds=14
after reconnect 3   : ctrl_conns=4 fds=15
t+30/60/90/120 s    : ctrl_conns=4 fds=15    (never reaped)
```

Counted from `ss`, i.e. the kernel's view, never the log (P-12). Four
`ESTABLISHED` TCP connections to the server's control port where one is correct.

### Corroborated by a second instrument, and it is not free

`vpn_stability.sh` (Ethernet re-run, 2026-09-12) reads `/proc/<pid>/fd` through
the root helper — a different instrument, a different host, a different day —
and sees the same rate:

| cycle | fds | rss_kib | up_mbit |
|---|---|---|---|
| 0 start | **12** | 23 836 | — |
| 1 (after reconnect 1) | **13** | 42 788 | 695.66 · 711.83 · 712.58 |
| 2 (after reconnect 2) | **14** | 53 192 | 711.08 · 712.11 · 712.12 |
| 3 | 14 | 53 848 | **95.35 · 159.87 · 118.51** |

One descriptor per reconnect, as `ss` reported. What is new is the last column:
**throughput fell by a factor of six on the third cycle**, sustained across all
three rounds, on a link whose bare capacity measured 733 Mbit/s up twenty-nine
seconds later. The MTU had settled (the stage prints an explicit warning
otherwise and printed none) and the path column read `direct` throughout.

RSS also rises with the descriptor count — 23.8 → 53.8 MiB — with the step
landing on the reconnect. A leaked `drive()` task retains its stack, and the
`yamux::Connection` it owns retains its buffers, so the two are consistent.

**This is not proof that the leak causes the collapse** — an idle leaked
connection moves no bytes, and the connecting mechanism is exactly what is not
established. But it removes the reading under which this defect was merely
untidy: a descriptor leak that coincides with losing five sixths of the link's
throughput has to be fixed before it can be dismissed. Re-run queued as
`vpn_stability_r2`, five cycles, with a bare control and the far end's allowance
delta inside every cycle.

## The suspected mechanism

`src/mux.rs`:

- `spawn_driver` puts the `yamux::Connection` — and therefore the TCP socket —
  inside a **detached** `tokio::spawn`. Nothing the caller holds owns it.
- `drive()` breaks out of its loop **only** on `Step::Done`, which comes from
  `poll_next_inbound` returning `None`/`Err` — that is, only when the **peer**
  closes or the connection errors.
- Dropping the `Opener` sets `openers_gone`, which merely stops pulling new open
  requests: the comment says so explicitly ("keep driving the connection for
  streams that are still alive").
- Dropping the `Acceptor` makes `inbound_tx.send` fail, and the result is
  discarded with `let _ =`.

So a client that opened a connection, finished with it, and dropped both handles
parks on `poll_next_inbound` forever holding the socket. `run_connect_once`
(src/vpn.rs) is exactly that shape: it builds `mux::client(control_stream)` per
attempt, and `run_with_reconnect` calls it again on the next attempt.

**Why the server does not rescue it — ESTABLISHED BY READING, 2026-09-13.** It
was listed here as an open question for the red-check. It is not open: the
server end runs the SAME driver. `src/server.rs:2132` builds its side with
`mux::server(socket)`, which is the same `spawn_driver` and therefore the same
`drive()` loop that exits only on `Step::Done`. So BOTH peers are waiting for
the OTHER one to close, and neither ever initiates it — a mutual liveness
deadlock, not "the server fails to notice". TCP stays `ESTABLISHED` because
`shared::tune_tcp`'s `SO_KEEPALIVE` probes are answered by a socket that is, in
fact, still open at both ends; keepalive detects a dead peer, and this peer is
not dead.

The two call sites that produce the leak are confirmed:
`src/vpn.rs:2059` inside `run_connect_once` and `src/vpn.rs:803` inside
`run_listen_once` — both build `crate::mux::client(...)` per attempt, and
`run_with_reconnect` calls them again on the next attempt.

This favours **fix option 3** below: one side initiating `poll_close` is enough
to unwind BOTH, because the peer's `poll_next_inbound` then returns `None` and
its own `drive()` reaches `Step::Done` on the very next poll. A fix on the
client alone therefore also releases the server's descriptor, which is the half
this project cannot patch on someone else's deployment.

## Red-check (write into `src/mux.rs`'s `mod tests`)

Needs real sockets — `tokio::io::duplex` cannot express "the peer never closes".

```rust
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{timeout, Duration};

/// A mux connection whose handles are ALL gone has nothing left to do, and must
/// close rather than park on the socket forever.
///
/// The connection lives in a detached driver task, so nothing the caller holds
/// owns it and nothing the caller drops takes it away; `drive()` exits only on
/// `Step::Done`, which needs the PEER to close. Measured consequence: a VPN
/// connector with `--auto-reconnect` leaked one ESTABLISHED control connection
/// per reconnect (1→2→3→4, none reaped in 120 s).
#[tokio::test]
async fn a_connection_whose_handles_are_all_dropped_closes_itself() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // The PEER never closes first — it only ever reports what it observes.
    let peer = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let (_opener, mut acceptor) = server(sock);
        acceptor.accept().await.is_none() // None ⇔ the client closed
    });

    let (opener, acceptor) = client(TcpStream::connect(addr).await.unwrap());
    drop(opener);
    drop(acceptor);

    let observed_close = timeout(Duration::from_secs(5), peer)
        .await
        .expect("client never closed a connection it had finished with")
        .unwrap();
    assert!(observed_close);
}

/// The other half of the same invariant, and the reason the naive fix is wrong:
/// substreams routinely OUTLIVE the `Opener` (the relay hands a stream to a
/// task and drops the opener), so "exit when the Opener is gone" would tear
/// down live traffic.
#[tokio::test]
async fn a_connection_with_a_live_substream_keeps_driving() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let peer = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let (_o, mut acceptor) = server(sock);
        let mut s = acceptor.accept().await.expect("inbound substream");
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await.unwrap();
        s.write_all(b"pong").await.unwrap();
        s.flush().await.unwrap();
    });

    let (opener, acceptor) = client(TcpStream::connect(addr).await.unwrap());
    let mut stream = opener.open().await.unwrap();
    drop(opener);   // the ordinary relay shape
    drop(acceptor);

    stream.write_all(b"ping").await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = [0u8; 4];
    timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
        .await
        .expect("the connection was torn down under a live substream")
        .unwrap();
    assert_eq!(&buf, b"pong");
    peer.await.unwrap();
}
```

Expected on today's code: the first test FAILS by timing out (the production
symptom), the second PASSES. That pairing is what makes it a red-check rather
than a test that happens to be green.

## Fix options, and why the obvious one is wrong

`yamux::Connection` (0.13.10) exposes only `new`, `poll_new_outbound`,
`poll_next_inbound`, `poll_close` — **no live-stream count**. So the count has
to be ours.

1. **"Exit when the Opener drops" — WRONG.** Substreams outlive the opener by
   design; this kills live relay traffic. Second test above exists to refuse it.
2. **Refcounted streams.** Hand out streams wrapped in a type holding an
   `Arc<()>`; the driver exits when both handles are gone and the count is back
   to one. Structurally correct and fixes every caller, but `mux::Stream` is a
   *type alias* used across the codebase as `LinkStream`, so newtyping it
   ripples into every splice site — and those sites are governed by the yamux
   single-waker invariant ("never split a `mux::Stream` across two tasks"), so
   the refactor is not mechanical.
3. **Explicit connection handle.** `spawn_driver` also returns a guard whose
   `Drop` tells the driver to `poll_close`. Callers that own a whole connection
   for a bounded scope (the VPN connector's per-attempt control connection) hold
   it; every other caller is byte-identical to today. Smallest auditable change
   that fixes the measured defect, at the cost of not fixing callers that have
   the same shape and have not been measured.

Decide AFTER the red-check RUNS, not before. Reading has already removed the one
branch that would have made all three options answers to the wrong question (the
server runs the same driver and does not close either — see above), so what the
red-check adds now is the *executable* proof plus the second test's refusal of
option 1. A mechanism established by reading and never executed is a hypothesis
with good footnotes.

## The fix that landed (2026-09-13)

**Option 2, realised without the ripple that made it look expensive.** The note
above priced option 2 as "newtype `mux::Stream`, and that ripples into every
splice site". It does not: `Stream` is a *type alias*, and callers use it
opaquely — nothing outside `src/mux.rs` names `Compat<yamux::Stream>`, calls
`into_inner`, or otherwise depends on what is inside it (checked across `src/`
and `crates/`). So the alias changed its inner type and every call site compiled
unchanged:

```rust
pub type Stream = Compat<TrackedStream>;
```

The count is ONE counter for everything that can still make the connection
useful — every `Opener` clone, the `Acceptor`, and every substream handed out:

- `Liveness { handles: AtomicUsize, waker: AtomicWaker }`, owned by the driver
  and shared with each handle;
- `ConnRef`, a token whose `Drop` decrements and, on reaching zero, wakes the
  driver;
- `drive()`'s `poll_fn` registers the waker FIRST and then reads the count
  (the reverse order can miss the wake of a handle dropped between the two);
  at zero it returns `Step::Done`, which is what `poll_close` already hangs off.

Why this is safe where option 1 was not: a substream carries its own `ConnRef`,
so the relay shape (hand a stream to a task, drop the opener) keeps the
connection exactly as alive as it is today. Why it is better than option 3: it
fixes every caller rather than the two that were measured — `src/pool.rs:478`
builds a `LinkOpener::Mux(mux::client(a).0)` and drops the acceptor on the spot,
and eleven more call sites hold `(opener, _acceptor)` for a bounded scope.

And because both peers run this same driver, ONE side noticing is enough: the
close makes the peer's `poll_next_inbound` return `None`, its own `drive()`
reaches `Step::Done`, and the server's descriptor is released too — without
deploying anything on the server.

### Gates

`src/mux.rs`, "Connection liveness group" — four tests, real `TcpListener`
sockets throughout:

| test | what it refuses |
|---|---|
| `a_connection_whose_handles_are_all_dropped_closes_itself` | the leak itself — RED-CHECK: times out without the count, which is the production symptom |
| `a_connection_with_a_live_substream_keeps_driving` | option 1 (exit when the opener drops) |
| `a_connection_closes_when_its_last_substream_is_dropped` | "close when opener AND acceptor are gone" — passes the first two, fails this one |
| `an_opener_alone_keeps_the_connection` | any fix that keys on the acceptor — the server's carrier pool holds only an opener |

The last three exist because the first one alone is satisfied by three different
wrong fixes.

## Field gate

Whatever lands, the gate is the descriptor probe, because that is what found it:
N reconnects must leave exactly one ESTABLISHED control connection, counted from
`ss`, after a settle window.

**That probe now exists as `scripts/perf/staging/vpn/vpn_ctrl_leak.sh`** and runs
from `scripts/perf/staging/rerun_eth_p7.sh`. It is expected to **FAIL** on
today's code and exits non-zero when it does, so it leaves no completion marker
and is retried — which is exactly the behaviour wanted from a gate that is not
green yet.

```bash
scripts/perf/staging/vpn/vpn_ctrl_leak.sh          # CYCLES=4 SETTLE=120 by default
```

It counts through the root helper's `fdlist`/`res` verbs (/proc/<pid>/fd on a
root process is mode 0500, so a `sudo ls` at the call site would prompt and
report zero descriptors forever), and it kills the remote listener **by PID
located from the link id**, never by pattern.
