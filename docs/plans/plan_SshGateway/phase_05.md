# Phase 5 — Vhost + secret mapping + takeover

> **Intent:** map SSH forwards onto the vhost and secret registries (provider and consumer sides) and implement same-identity takeover (D2/I-5). After this phase all three tunnel types work over SSH.
> **Shippable alone?** yes — extends the opt-in gateway only.
> **Preconditions:** Phases 2 and 4 DONE. Read `SPIKE_FINDINGS.md` for channel APIs.

Context (self-contained): Phase 2 made `CarrierPool` hold `LinkOpener`s whose
`open_ready()` yields a client-leg stream (yamux: writes STREAM_READY; SSH: must not).
Phase 4 gives authenticated sessions with parsed `ForwardSpec`s, params, admin fields,
reaper. This phase adds the SSH variant of `LinkOpener` and registers SSH-backed entries
in the existing registries so the UNCHANGED server-side relay code serves them:
- vhost: `VhostRegistry` (`src/vhost.rs:471`), `VhostEntry` (`src/vhost.rs:339-385`), relay via `relay_vhost` (`src/vhost.rs:772-824`);
- secret: `Registry` (`src/secret.rs:80`), consumer relay inside `serve_consumer` (`src/secret.rs:441,~600-743`).

---

## Sub-phases

### 5.1 `LinkOpener::Ssh` variant
- **Model:** Sonnet
- **Files:** `src/mux.rs` (LinkOpener from Phase 2.1), `src/sshgw.rs`
- **Change:** add `#[cfg(feature = "ssh-gateway")] Ssh(SshOpener)` to `LinkOpener`. `SshOpener` (defined in `sshgw.rs`, re-exported or passed as a boxed trait if a module cycle appears — prefer: define a small `pub trait ChannelOpen: Send + Sync { async fn open(&self) -> io::Result<LinkStream>; }` in `mux.rs` and store `Ssh(Arc<dyn ChannelOpen>)`) opens a `forwarded-tcpip` channel on the owning session (russh session handle per SPIKE_FINDINGS; originator = the public/consumer peer when available, else 0.0.0.0:0) and returns `Box::new(channel.into_stream())`. `open_ready` for `Ssh`: NO marker byte (I-4). Open failure (session gone) returns `io::Error` so `CarrierPool` pruning/failover treats a dead SSH session exactly like a dead yamux carrier.
- **Unit tests:** `link_open_ready_ssh_writes_no_marker` — fake `ChannelOpen` impl backed by an in-memory duplex; assert first byte of what the "provider" reads is the payload, not 0.
- **e2e tests:** covered by T-SSH-VH1/T-SSH-SEC1.
- **Done:** gates green; default-features build unaffected (variant cfg-gated).

### 5.2 Vhost provider via SSH
- **Model:** Sonnet
- **Files:** `src/sshgw.rs`; anchors: registration/atomic insert `src/vhost.rs:536,576`, `VhostEntry` fields `src/vhost.rs:339-385`, basic auth check module `src/basicauth.rs`
- **Change:** on `ForwardSpec::Vhost { label }`:
  1. Require vhost enabled on the server (`vhost_config`/base-domain present — same condition `HelloVhost` handling checks); else reject with channel message "vhost not enabled on this server".
  2. Enforce grant `permit` globs (`vhost/<glob>`).
  3. Build a `VhostEntry` with `pool` = `CarrierPool` containing one `LinkOpener::Ssh` (carriers=1 by definition), `peer`, `notes`, `local_host/local_port` from the `-R` spec's target (host:port the client gave — display-only), `udp:false`, `auto_reconnect:false`; atomic-insert into `vhost_registry` keyed by label (collision → 5.4 takeover path). Register admin entry `Role::Vhost`, `transport: Ssh`, identity (RAII, owned by the forward state — Phase 4.4 teardown covers it; registry entry removal on drop too: wrap in a guard struct whose `Drop` does `vhost_registry.remove_if(label, |e| Arc::ptr_eq(...))`).
  4. Reply on the session channel with the ready URLs (compose exactly like `ServerMessage::VhostReady` does server-side — reuse the same URL builder if it is a plain function; else format from base domain + ports).
  5. `basic-auth=user:pass` param: store credentials on the SSH-owned entry as a NEW field `gateway_basic_auth: Option<(String, String)>` on `VhostEntry` (default `None`; native path never sets it). Enforcement: in the vhost request path where the parsed head is available before relay (inside `relay_vhost` `src/vhost.rs:772-824` or its caller `handle_http`/`handle_https` `src/vhost.rs:1168,1236` — put the check where the head bytes are already parsed, BEFORE opening the provider substream), if `gateway_basic_auth` is Some validate the `Authorization` header via `src/basicauth.rs`; failure ⇒ write a minimal `401` + `WWW-Authenticate: Basic` response and close. Native entries (`None`) take the existing path untouched.
  > Behavior-change callout: `VhostEntry` gains one `Option` field; every native construction site sets `None`. The existing display-only `basic_auth: bool` stays as-is for native providers.
- **Unit tests:** `gateway_basic_auth_none_is_noop` (unit around the check function: None ⇒ passthrough; Some+bad ⇒ 401 bytes; Some+good ⇒ pass).
- **e2e tests:** (in `tests/ssh_gateway_test.rs`; server started with `--vhost-base-domain test.local` and explicit vhost http port, pattern `tests/vhost_test.rs`)
  **T-SSH-VH1** — `ssh -N -R mysub:80:127.0.0.1:<svc>`; `curl -H "Host: mysub.test.local" http://<srv>:<http_port>/` returns backend body; ssh session output contains the http URL; admin shows Role::Vhost transport ssh.
  **T-SSH-VH2** — same with exec `basic-auth=u:p`: request without header ⇒ 401; with correct header ⇒ 200 backend body.
  **T-SSH-PFX1** — `-R vhost/pfx:0:...` registers vhost `pfx` (prefix overrides the port-0 heuristic); `-R secret/sid:80:...` registers secret provider `sid`.
- **Done:** gates green; `tests/vhost_test.rs` (native) unmodified and green; T-SSH-VH1/VH2/PFX1 pass; killing ssh frees the subdomain (assert re-register succeeds after kill).

### 5.3 Secret provider + consumer via SSH
- **Model:** Sonnet
- **Files:** `src/sshgw.rs`; anchors: `src/secret.rs:80` (Registry), provider registration in `serve_provider` `src/secret.rs:254` (mirror its registry-insert + admin-register sequence), consumer relay/failover `src/secret.rs:~600-743`
- **Change:**
  - **Provider** (`ForwardSpec::SecretProvider { id }`): enforce `permit` (`secret/<glob>`); insert `Arc<CarrierPool>` (one `LinkOpener::Ssh`) into the secret `Registry` under `id` (collision → 5.4); admin entry `Role::SecretProvider`, transport ssh, identity, `max_conns` from params/grant enforced by the same semaphore mechanism native providers use (see how `serve_provider`/relay acquires permits — reuse, do not fork). RAII guard removes registry entry + admin row on teardown (I-3). Native consumers (`bore proxy`) now reach it through the untouched `serve_consumer` relay: their `pool.open_ready()` opens an SSH channel transparently.
  - **Consumer** (`direct-tcpip` to `<id>`/`secret/<id>`, parsed in 4.2 — the port is an ignored placeholder: OpenSSH's `-L` CLI rejects a literal port 0 outright, unlike `-R`, so real clients send some nonzero value): per channel — look up `Registry.get(id)`; miss ⇒ open-failure with message "unknown secret id". Hit ⇒ acquire conn permit, `pool.open_ready()` toward the provider with the SAME failover-retry semantics as the native consumer loop (retry pick→open up to pool size — this is the BUG-S4 guarantee; if that loop is not already a callable helper after Phase 2.2, extract `open_with_failover(pool) -> io::Result<LinkStream>` in `src/secret.rs` NOW and switch the native loop to it — regression: `tests/secret_test.rs`, `tests/secret_pool_test.rs` unmodified green), then splice channel-stream ↔ provider-stream with `CountingStream`.
  - **Admin (D11):** ONE `Role::SecretConsumer` row per SSH session per id, created lazily at the first direct-tcpip for that id, `active` incremented per live channel — never one row per channel (BUG-S1 parity; assert in e2e).
- **Unit tests:** none new (failover helper covered by existing secret pool tests).
- **e2e tests:** (server + native client binaries driven like `tests/secret_test.rs` does)
  **T-SSH-SEC1** — ssh provider (`-R tcp-id:0:127.0.0.1:<svc>`) + NATIVE consumer (`bore proxy --tcp-secret-id tcp-id --local-proxy-port :0` via library API as secret_test does) ⇒ roundtrip through the consumer port.
  **T-SSH-SEC2** — NATIVE provider (`bore local <svc> --tcp-secret-id tcp-id` via lib) + ssh consumer (`-N -L <lp>:tcp-id:1`) ⇒ `curl 127.0.0.1:<lp>` roundtrip.
  **T-SSH-SEC3** — ssh on both sides ⇒ roundtrip; admin shows exactly 2 rows (one provider, one consumer) with `transport == "ssh"`; open 3 concurrent proxied connections ⇒ still 2 rows, consumer `active == 3`.
- **Done:** gates green; T-SSH-SEC1..3 pass; `tests/secret_test.rs` + `tests/secret_pool_test.rs` unmodified green.

### 5.4 Same-identity takeover (D2, I-5)
- **Model:** Sonnet — **Opus review gate** (registry race semantics)
- **Files:** `src/sshgw.rs`; touch points: vhost insert (5.2), secret insert (5.3)
- **Change:** on name collision at registration:
  1. Read the incumbent's identity (store identity on the SSH-owned guard AND — for native entries — treat identity as absent).
  2. Incumbent identity == new session identity (exact string match, both non-empty) ⇒ takeover: synchronously tear down the incumbent forward (invoke its owner-side teardown: close its channels/listener, drop its Registration and registry guard; if the incumbent session has no remaining forwards, disconnect it with a message "evicted by newer session with same identity"), THEN insert the new entry. Use the DashMap entry API so check-evict-insert cannot interleave with a concurrent third registration (hold the entry lock across the decision; the teardown that touches the old session must not run inside the map lock — remove-under-lock, then finish teardown outside; document the two-step in a comment).
  3. Different identity, or incumbent is a NATIVE entry ⇒ reject with channel message "name in use" (takeover never evicts native tunnels — SSH identities and the HMAC secret are different trust domains; document this in the code comment and user docs).
  4. Public ports: no takeover (D9-overview): collision ⇒ plain reject.
- **Unit tests:** `takeover_decision_table` — pure function `(incumbent: Option<(Identity, IsSsh)>, newcomer: Identity) -> Decision` covering: free ⇒ insert; same-id ssh ⇒ evict; diff-id ⇒ reject; native incumbent ⇒ reject.
- **e2e tests:**
  **T-SSH-TAKE1** — session A (key K) registers vhost `mysub` backed by service S1; session B (same key K) registers `mysub` backed by S2 ⇒ B succeeds, curl now returns S2's body, A's session receives the eviction message (or exits); no window where curl fails hard for more than the switchover (poll tolerance 2 s).
  **T-SSH-TAKE2** — session C (different key) tries `mysub` ⇒ rejected (`ExitOnForwardFailure=yes` makes ssh exit non-zero), B's tunnel still serves S2.
- **Done:** gates green; T-SSH-TAKE1/2 pass; Opus sign-off on the lock discipline recorded in the PR/commit description.

---

## Phase gates

- **Fmt:** `cargo fmt`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --features ssh-gateway --test ssh_gateway_test` + full `cargo test --all-features`
- **Regression guard:** `tests/secret_test.rs`, `tests/secret_pool_test.rs`, `tests/vhost_test.rs` unmodified and green; netns `secret_netns_test.sh` + `vhost_netns_test.sh` (sudo, exact path — see phase_02.md gates for invocation) FAIL: 0 if `src/secret.rs`/`src/vhost.rs` were touched (they are: failover helper + gateway_basic_auth field ⇒ REQUIRED).

## Phase done criterion

Reference-scenario lines 1, 3, 4 (vhost, secret provider, secret consumer via SSH) green: T-SSH-VH1/2, T-SSH-PFX1, T-SSH-SEC1..3, T-SSH-TAKE1/2 all pass; native suites untouched and green.
