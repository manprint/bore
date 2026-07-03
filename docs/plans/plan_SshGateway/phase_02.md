# Phase 2 — LinkOpener abstraction and STREAM_READY confinement

> **Intent:** make the server's "open a substream toward the tunnel client" operation transport-agnostic, and confine the `STREAM_READY` marker to that operation, so Phase 4-5 can plug SSH channels in without touching relay logic. Zero behavior change.
> **Shippable alone?** yes — pure refactor, wire bytes identical.
> **Preconditions:** none (independent of Phase 1; feature flag not involved).

Context (self-contained): today the server opens yamux substreams toward tunnel clients via
`mux::Opener::open()` (`src/mux.rs:74-84`) — pooled in `CarrierPool`
(`src/secret.rs:80`: `pub type Registry = Arc<DashMap<String, Arc<CarrierPool>>>`) — and
then the CALL SITES write the readiness marker `mux::STREAM_READY` (`src/mux.rs:35`,
`pub const STREAM_READY: u8 = 0`, one byte) into the substream before splicing. An SSH
channel must never receive that byte (I-4). Therefore the write moves INSIDE the open
operation, which becomes variant-aware.

In-scope STREAM_READY write sites (client-substream direction only):
- `src/secret.rs:735` and `src/secret.rs:1923` (secret relay paths)
- `src/vhost.rs:824` (`relay_vhost`, via `mux::write_stream_ready`)
- the public per-connection task inside `serve_tunnel`'s accept loop, `src/server.rs:1706-1843`
**OUT of scope (do not touch): `src/vpn.rs`, `src/vpn_server.rs:1678`, `src/udp_diagnostic.rs:232`, and the mux driver's own legacy writes at `src/mux.rs:202-209`** — these are different protocols with their own framing (D7).

---

## Sub-phases

### 2.1 Introduce `LinkOpener` + `open_ready()` in mux.rs
- **Model:** Sonnet
- **Files:** `src/mux.rs:22-96`, `src/secret.rs:80` (CarrierPool definition and its pick/prune internals, same file region)
- **Change:**
  1. In `src/mux.rs` add (not feature-gated — the enum is always available; the SSH variant arrives in Phase 4 behind `#[cfg(feature = "ssh-gateway")]`):
     - `pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}` with a blanket impl (check first: `src/vhost.rs` already has an `AsyncReadWrite` boxed-stream alias used by `relay_vhost` (`src/vhost.rs:772`) — if it is reusable as the common boxed type, reuse it and do NOT invent a second alias; move/re-export it from `mux.rs` if needed to avoid a dependency cycle).
     - `pub type LinkStream = Box<dyn Duplex>;` (or the reused alias).
     - `pub enum LinkOpener { Mux(Opener) }` with:
       `pub async fn open_ready(&self) -> io::Result<LinkStream>` — for `Mux`: `let mut s = opener.open().await?; s.write_all(&[STREAM_READY]).await?; Ok(Box::new(s))`. Preserve EXACT current semantics: today call sites open, then write the marker, then splice; `open_ready` must produce the same byte order on the wire (marker first, before any payload). No flush-behavior change: if a current call site flushes after the marker, keep that flush inside `open_ready`.
  2. Generalize `CarrierPool` (in `src/secret.rs`) to store `LinkOpener` instead of `mux::Opener`: constructor takes `LinkOpener::Mux(opener)` at every existing construction site; `pick()` and the dead-carrier pruning logic keep identical semantics (pruning is driven by open failures — unchanged).
  3. Keep `mux::write_stream_ready` (if it remains used by out-of-scope sites) or inline it; do not change out-of-scope sites.
- **Unit tests:** in `src/mux.rs` (or `tests/` following where mux tests live today):
  `link_open_ready_writes_single_zero_byte` — build an in-process yamux client/server pair (pattern exists in mux/secret unit tests), call `open_ready`, assert the acceptor side reads exactly one byte `0` followed by a test payload;
  `carrier_pool_generalized_pick_roundrobin` — existing `tests/secret_pool_test.rs` assertions still pass unmodified (that file is the regression, not a new test).
- **e2e tests:** none new (no behavior change).
- **Done:** gates green (`cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`); `tests/secret_pool_test.rs` untouched and green.

### 2.2 Migrate the four in-scope call sites
- **Model:** Sonnet — **Opus review gate on the diff before merge** (hot path; checks: marker/payload ordering, no double-write, no lost flush, tune/counting wrappers applied in the same order as before)
- **Files:** `src/secret.rs:735`, `src/secret.rs:1923`, `src/vhost.rs:772-824`, `src/server.rs:1706-1843`
- **Change:** at each site, replace the pair "open substream + write STREAM_READY" with a single `pool.open_ready()` / `LinkOpener::Mux(opener).open_ready()` call returning `LinkStream`; the subsequent splice code (`copy_bidirectional_with_sizes`, `CountingStream` wrapping — `src/shared.rs:41-70`) operates on the boxed stream. The consumer failover loop in `serve_consumer` (`src/secret.rs:~600-743`, retry pick→open across pool on failure — this is the BUG-S4 guarantee) must keep IDENTICAL retry semantics: an `open_ready` failure counts exactly like an `open` failure did. Do not restructure the loops; minimal substitution only.
- **Unit tests:** none new beyond 2.1.
- **e2e tests:** none new. Regression instead (see gates).
- **Done:** `git grep -n "STREAM_READY"` shows, for src/secret.rs, src/vhost.rs, src/server.rs: zero write sites (only `open_ready` in mux.rs writes it); full regression green.

---

## Phase gates

- **Fmt:** `cargo fmt`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --all-features` (includes tests/secret_test.rs, secret_pool_test.rs, vhost_test.rs, e2e_test.rs, admin_test.rs — all must pass unmodified)
- **Regression guard (mandatory, sudo):** `cargo build --release --features vpn` then
  `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/secret_netns_test.sh` and
  `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/vhost_netns_test.sh` and
  `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/local_proxy_netns_test.sh`
  — all with FAIL: 0 (invoke by exact absolute path; `sudo bash scripts/...` prompts and must not be used).

## Phase done criterion

STREAM_READY toward tunnel clients is written in exactly one place (`mux.rs` `open_ready`), proven by grep; every existing cargo test and the three netns suites pass with zero modifications to their files.
