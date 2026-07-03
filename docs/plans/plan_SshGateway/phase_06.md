# Phase 6 — Control-port demux + SSH-over-TLS

> **Intent:** serve SSH on the control port itself (deployed as 443) alongside TLS, HTTP and native bore, including SSH inside TLS (D4), with the gateway-off path byte-identical (I-1).
> **Shippable alone?** yes — new branch only taken when `--ssh-gateway` is on.
> **Preconditions:** Phase 4 DONE (5 recommended: full tunnel matrix testable through the demux).

Context (self-contained): the accept path today: `src/server.rs:983-1008` accept loop; if a
`TlsAcceptor` is configured (`Server.tls`, set via `set_tls` `src/server.rs:365`) the
stream is TLS-accepted at `src/server.rs:992`; then `route_connection`
(`src/server.rs:1055-1085`) peeks the first byte with `NETWORK_TIMEOUT` (3 s,
`src/shared.rs:204`) and dispatches HTTP (`admin_http::is_http_first_byte`,
`src/admin_http.rs:46`, matches `G/P/H/D/O/T/C`) vs bore protocol
(`handle_connection`, `src/server.rs:1184`). `Prefixed` (`src/prefixed.rs`) replays peeked
bytes. First-byte disambiguation: SSH banner starts `S` (0x53); TLS ClientHello 0x16;
yamux frame 0x00; HTTP the verb letters. `S` collides with no existing branch.

---

## Sub-phases

### 6.1 Pre-TLS peek dispatcher
- **Model:** Sonnet — **Opus review gate** (hot accept path + I-1)
- **Files:** `src/server.rs:983-1008` (accept loop), `src/server.rs:1055-1085` (route_connection), `src/prefixed.rs` (reuse), `src/sshgw.rs` (entry point `serve_ssh_connection(socket: impl mux::Transport, peer)`)
- **Change:** in the per-connection spawned task, branch FIRST on gateway state:
  - `ssh_gateway` disabled ⇒ EXACTLY the current code, no added reads, no timeout, no wrapper (I-1). Structure the edit so the legacy branch is the untouched original block — the diff must show insertion of a new `if`, not a rewrite.
  - enabled ⇒ peek 1 byte with a 2 s timeout (constant `SSH_PEEK_TIMEOUT: Duration = 2 s` in sshgw.rs, doc comment: sslh-style — SSH clients that wait for the server banner send nothing; everyone else (TLS/HTTP/yamux/native bore, which sends `Hello` immediately because yamux is lazy) talks within milliseconds):
    - timeout (no byte) ⇒ SSH (`serve_ssh_connection` on the raw socket);
    - `b'S'` ⇒ SSH (wrap in `Prefixed` to replay);
    - `0x16` ⇒ TLS accept (existing acceptor path) then 6.2's post-TLS routing;
    - HTTP verb byte (`is_http_first_byte`) or anything else ⇒ existing `route_connection` logic on the `Prefixed` stream (which re-peeks harmlessly — verify `route_connection` works on an already-peeked `Prefixed`; it does since `Prefixed` implements the transport trait; otherwise refactor route_connection to accept the peeked byte as an argument, keeping the disabled path calling it exactly as today).
  - When TLS is NOT configured but gateway is on: same peek, minus the 0x16 branch (0x16 falls to bore/HTTP as today — do not invent TLS handling).
- **Unit tests:** `demux_classify_first_byte` — pure function `(Option<u8>) -> Route` table: None⇒Ssh, b'S'⇒Ssh, 0x16⇒Tls, b'G'⇒Http, 0x00⇒Bore, 0xFF⇒Bore.
- **e2e tests:** see 6.3.
- **Done:** gates green; diff of the disabled path is empty (reviewer checks `git diff` hunk shape); Opus sign-off recorded.

### 6.2 Post-TLS second peek (SSH-over-TLS, D4)
- **Model:** Sonnet
- **Files:** `src/server.rs` (the TLS-accepted branch), `src/sshgw.rs`
- **Change:** gateway on: after `acceptor.accept(stream)` succeeds, peek again (same 2 s timeout semantics): bytes `SSH-`? — a single byte suffices (`b'S'`) since no TLS-wrapped bore/HTTP payload starts with `S`... it does: an HTTP request `SUBSCRIBE`? Not in `is_http_first_byte`'s verb set, but admin serves standard verbs only; still, to be exact peek FOUR bytes and match the literal prefix `b"SSH-"` (four-byte peek via `Prefixed`), else fall through to the existing post-TLS routing (`route_connection` body: HTTP vs bore). `serve_ssh_connection` must be generic over `mux::Transport` (`src/mux.rs:26`) so it runs on `TlsStream` and plain `TcpStream` alike (russh run-over-stream API is generic — SPIKE_FINDINGS.md).
  Gateway off: post-TLS path untouched (I-1).
- **Unit tests:** extend `demux_classify_first_byte` with a 4-byte variant: `b"SSH-"`⇒Ssh, `b"SUBS"`⇒NotSsh, `b"GET "`⇒NotSsh.
- **e2e tests:** T-SSH-TLS1 (6.3).
- **Done:** gates green.

### 6.3 Demux e2e + off-path regression
- **Model:** Sonnet
- **Files:** `tests/ssh_gateway_test.rs` (extend)
- **Change:** server started WITHOUT `--ssh-port` (demux only), with TLS certs (self-signed helper — pattern `tests/tls_test.rs`), vhost base domain, admin token. All tests hit the ONE control port:
  **T-SSH-DMX1** — concurrently: (a) native public tunnel via TLS control connection (lib client with `https://127.0.0.1:<port>` + insecure — pattern `tests/tls_test.rs`), (b) `ssh -N -R 19006:...` public tunnel, (c) plain-HTTP admin/vhost request, (d) plain bore client (no TLS scheme). All four succeed simultaneously; traffic through (a) and (b) round-trips.
  **T-SSH-DMX2** — raw TCP connect, send NOTHING: within ~2.5 s the server speaks first with `SSH-2.0-` banner (read and assert prefix) — proves the timeout fallback for banner-waiting clients.
  **T-SSH-TLS1** — `ssh -o ProxyCommand='openssl s_client -quiet -verify_quiet -connect 127.0.0.1:<port>' -N -R 19007:...` ⇒ tunnel works over TLS (D4). Skip-guard if `openssl` missing.
  **T-DMX-OFF** — gateway DISABLED, TLS configured: (a) native TLS tunnel + plain bore + HTTP admin all work (this is the existing behavior — reuse/extend an existing test only by RUNNING it, not editing); (b) a client sending `SSH-2.0-...` gets no SSH banner and the connection errors/closes (proves the branch is truly off).
- **Unit tests:** none.
- **e2e tests:** T-SSH-DMX1, T-SSH-DMX2, T-SSH-TLS1, T-DMX-OFF.
- **Done:** all four green; full `cargo test --all-features` green.

---

## Phase gates

- **Fmt:** `cargo fmt`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --features ssh-gateway --test ssh_gateway_test` + full `cargo test --all-features` AND full default-features `cargo test`
- **Regression guard (mandatory, sudo):** all four netns suites (`secret`, `vhost`, `local_proxy`, `vpn`) FAIL: 0 — the accept path is shared by everything; invoke by exact absolute path with `sudo -n` (see phase_02.md).

## Phase done criterion

One port serves SSH + TLS + HTTP + native bore concurrently (T-SSH-DMX1), silent clients get the SSH banner (T-SSH-DMX2), SSH works inside TLS (T-SSH-TLS1), and with the flag off the accept path is demonstrably unchanged (T-DMX-OFF + netns suites).
