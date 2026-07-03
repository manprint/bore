# Phase 4 — Gateway core + public tunnels

> **Intent:** stand up the embedded russh server (auth, session lifecycle, spec parsing, keepalive/reaper) and wire the first tunnel type: PUBLIC. Testable on a dedicated `--ssh-port` listener; control-port demux comes in Phase 6 (D8).
> **Shippable alone?** yes — feature off by default; with the feature on, only opt-in flags change behavior.
> **Preconditions:** Phases 1, 2, 3 DONE. Read `docs/plans/plan_SshGateway/SPIKE_FINDINGS.md` FIRST — it names the exact russh handler methods; do not rediscover the API.

Context (self-contained): `bore server` (struct `Server`, `src/server.rs:86-240`) holds the
registries and config. The SSH gateway accepts SSH connections, authenticates against the
Phase-3 stores, parses forward requests per D1, and for a numeric-port request binds a
public listener exactly like a native `Hello` tunnel, splicing each inbound public
connection to a `forwarded-tcpip` SSH channel. Constants `SSH_KEEPALIVE_INTERVAL` (20 s) /
`SSH_CTRL_TIMEOUT` (60 s) exist from Phase 1.1. Invariants I-2 (warn, never silent) and
I-3 (no zombie entries) bind here.

---

## Sub-phases

### 4.1 Flags, config validation, russh server skeleton, auth wiring
- **Model:** Sonnet
- **Files:** `src/main.rs:435-650` (Server args struct — add flags; follow the `#[cfg(feature = "vpn")]` flag pattern used for vpn flags in the same struct), `src/server.rs:86-240` (Server fields) and `:365-630` region (builder setters — add `set_ssh_gateway(cfg)`), `src/sshgw.rs`
- **Change:**
  1. New server flags (all `#[cfg(feature = "ssh-gateway")]`):
     `--ssh-gateway` (bool) — master switch; `--ssh-port <u16>` (optional dedicated listener; also requires `--ssh-gateway`); `--ssh-host-key-file <PATH>` (default `bore_ssh_host_key.pem`); `--ssh-authorized-keys-dir <PATH>`; `--ssh-passwords-file <PATH>`; `--ssh-banner <STRING>` (optional).
  2. Fail-fast validation at startup (before any listener binds, pattern: the cert/key pairing check at `src/main.rs:1918-1927`): `--ssh-gateway` requires at least one of keys-dir/passwords-file, else bail with remediation text; `--ssh-port` without `--ssh-gateway` bails.
  3. `src/sshgw.rs`: `pub struct SshGatewayConfig { ... }` mirroring the flags; `pub struct SshGateway` holding `Arc`s of what tunnel serving needs (clones of the Server's registries and helpers — pass them in from `Server`, do not re-derive: `providers: secret::Registry`, `vhost_registry`, `admin: AdminRegistry`, `conn_permits`, port-range, `bind_tunnels`). Host key: load PEM at `--ssh-host-key-file`; if missing, generate ed25519, write the file mode 0600, and `info!` the SHA256 fingerprint (D9) — on every start log the fingerprint.
  4. russh `Handler` per connection: `auth_publickey` → `KeyStore::check`; `auth_password` → `PasswordStore::check`; store the resulting identity + grant in the connection state. Reject all other auth methods. Pre-auth grace: 30 s timer aborts unauthenticated connections; max 3 auth attempts per connection then disconnect. Reject channel types other than `session` (used only for exec/env/UX output) — no shell (reply with a one-line message then EOF), no pty, no sftp/subsystem.
  5. When `--ssh-port` set: bind `TcpListener` on `(bind_addr, ssh_port)` in `Server::listen` alongside the vhost listeners (region `src/server.rs:688-880`), `shared::tune_tcp` on each accepted socket (invariant), hand off to russh (`run_stream`-style API per SPIKE_FINDINGS).
- **Unit tests:** `sshgw_config_validation` table test (gateway without auth source ⇒ err; ssh-port without gateway ⇒ err; valid combos ok) — pure function over the config struct so it tests without cfg tricks; `host_key_generated_and_reloaded` — tempdir: first call creates file, second call loads same fingerprint.
- **e2e tests:** covered by 4.3's T-SSH-PUB1 (auth path exercised there).
- **Done:** gates green; `bore server --ssh-gateway` without auth source exits non-zero with clear message; with keys-dir it starts and logs the host-key fingerprint.

### 4.2 Forward-spec parsing (D1) + parameter grammar (I-2)
- **Model:** Sonnet
- **Files:** `src/sshgw.rs`
- **Change:**
  1. `pub enum ForwardSpec { Public { port: u16 }, Vhost { label: String }, SecretProvider { id: String } }` and `pub fn parse_forward_spec(addr: &str, port: u32) -> Result<ForwardSpec, SpecError>` implementing D1 exactly:
     - `addr` empty / `"localhost"` / `"127.0.0.1"` / `"0.0.0.0"` / `"*"` ⇒ `Public { port }` (any port value; 0 = server-assigned);
     - `addr` starting `vhost/` ⇒ Vhost with the remainder as label (any port);
     - `addr` starting `secret/` ⇒ SecretProvider (any port);
     - bare label + port 80 or 443 ⇒ Vhost; bare label + port 0 ⇒ SecretProvider; bare label + any other port ⇒ error "ambiguous; use vhost/ or secret/ prefix";
     - label charset: validate like `vhost::extract_subdomain` (`src/vhost.rs:160`): lowercase `[a-z0-9-]+`, single label, no dots — reject otherwise (secret ids allow the same charset; document that existing native ids with other charsets remain reachable natively but not via SSH v1).
  2. `pub fn parse_params(exec: Option<&str>, env: &[(String,String)], grant: &KeyGrant) -> Params` — grammar `key=value` space-separated with double-quote support; env vars `BORE_<UPPERKEY>` map to the same keys; precedence: grant option > exec > env (D from analysis §2.5). Recognized keys: `notes`, `max-conns`, `basic-auth` (`user:pass`), `webserver-log` (`on`), `id`. Keys that name client-transport features (`udp`, `carriers`, `stun-server`, `upnp`, `try-port-prediction`, `nat-udp-preferred-port`, `auto-reconnect`) ⇒ collected into `Params.warnings` with a fixed message ("not available via SSH ingress; use the native bore client"), I-2. Unknown key ⇒ warning too. Nothing silent.
  3. `direct-tcpip` destination parsing: host `"<id>"` or `"secret/<id>"` with port 0 ⇒ secret consumer target (used in Phase 5.3); anything else ⇒ channel open failure with message.
- **Unit tests:** `spec_matrix` — table: `("",9005)→Public`, `("localhost",9005)→Public`, `("mysub",80)→Vhost`, `("mysub",443)→Vhost`, `("tcp-id",0)→Secret`, `("vhost/x",0)→Vhost`, `("secret/x",80)→Secret`, `("My_Sub",80)→err`, `("a.b",80)→err`, `("mysub",8080)→err(ambiguous)`;
  `params_precedence` — grant.max_conns=3 + exec `max-conns=9` ⇒ 3; env-only value used when exec absent;
  `params_quoting` — `notes="two words" basic-auth=u:p` parsed exactly;
  `params_warnings_for_transport_keys` — `udp=on carriers=4` ⇒ two warnings, no error;
  `direct_tcpip_dest` — `("tcp-id",0)→ok`, `("secret/tcp-id",0)→ok`, `("example.com",80)→err`.
- **e2e tests:** T-SSH-WARN1 (in 4.3's e2e file): session with exec `udp=on` ⇒ ssh client stderr/stdout contains the warning line.
- **Done:** gates green; the matrix test encodes every D1 row verbatim.

### 4.3 Public tunnel wiring
- **Model:** Sonnet — **Opus review gate on the accept/splice task structure** (one stream = one task; no yamux-style waker hazard exists on russh streams, but verify `into_stream()` is used from a single task)
- **Files:** `src/sshgw.rs`; reuse anchors: `create_listener` `src/server.rs:1011-1025`, public accept-loop pattern `src/server.rs:1706-1843`, `admin::register` `src/admin.rs:289`, `CountingStream` `src/shared.rs:41-70`, `tune_tcp` `src/shared.rs:263`
- **Change:** on granted `tcpip-forward` ⇒ `Public { port }`:
  1. Enforce grant `permit` list if present (`port/<n>`/`port/<a>-<b>` entries) — violation ⇒ reject request with message on the session channel.
  2. Bind via the same port-range rules as native tunnels: port 0 ⇒ pick from `port_range` (mirror `create_listener` usage; call it if visibility allows — make it `pub(crate)` if needed), else validate requested port within `--min-port/--max-port`. Reply the bound port in the tcpip-forward response (OpenSSH prints "Allocated port N for remote forward" for port 0 requests) AND write a line `public tunnel ready: <host>:<port>` to the session channel.
  3. `admin.register(NewEntry { role: Role::Public, transport: ssh, identity, public_port, notes, max_conns, ... })` — RAII `Registration` (`src/admin.rs:413-457`) owned by the forward's state so teardown (cancel-tcpip-forward, session close, reap) drops it (I-3). `transport`/`identity` fields are added in 4.5.
  4. Accept loop (mirror `src/server.rs:1706-1843` minus the direct-UDP branch): per inbound public connection — acquire `conn_permits` semaphore AND the per-tunnel `max-conns` cap (Params/grant; default `DEFAULT_MAX_CONNS`), `tune_tcp`, open `forwarded-tcpip` channel toward the client with originator = public peer addr, `channel.into_stream()`, wrap public side in `CountingStream` (wire the same global+entry counters native tunnels use), `copy_bidirectional_with_sizes`, decrement `active` on exit. NO STREAM_READY anywhere on the SSH side (I-4 — nothing to write: `open_ready` is not used for SSH-originated public tunnels; the SSH channel IS the client leg).
  5. `cancel-tcpip-forward` tears down exactly that listener + registration; other forwards on the same session survive.
- **Unit tests:** none beyond parsing (network path covered by e2e).
- **e2e tests:** new `tests/ssh_gateway_test.rs` (`#![cfg(feature = "ssh-gateway")]`, same harness style as `tests/ssh_gateway_spike_test.rs`: tempdir keys, real `ssh` CLI, skip-guard, ephemeral ports (D12), spawn `Server` via `bore_cli::server::Server::new(...)` + `set_ssh_gateway` + `tokio::spawn(listen)` — pattern `tests/transfer_test.rs` + `wait_for_control_port()`):
  **T-SSH-PUB1** — local HTTP echo service; `ssh -N -R 19005:127.0.0.1:<svc>` with test key; poll `TcpStream::connect(server:19005)` → HTTP roundtrip body matches; admin API (`--admin-token`, GET the status JSON — pattern `tests/control_port_test.rs`) shows one Public entry with `transport == "ssh"`.
  **T-SSH-PUB2** — `-R 0:127.0.0.1:<svc>`; parse "Allocated port" from ssh stderr; connect through it; roundtrip ok.
  **T-SSH-PUB3** — exec params `notes=itest max-conns=1`; admin JSON has the note; open one long-lived connection through the tunnel, second connect must NOT complete an HTTP roundtrip while the first is held (max-conns=1 enforced), succeeds after release.
  **T-SSH-WARN1** — exec `udp=on`; captured ssh output contains "not available via SSH".
  **T-SSH-CANCEL1** — session with two `-R` forwards; `cancel-tcpip-forward` one (drop it client-side via ssh -O cancel or a scripted control-master; if impractical with CLI, close the whole session and assert BOTH listeners freed ≤ 2 s — rename test accordingly).
- **Done:** gates green; T-SSH-PUB1..3, T-SSH-WARN1 pass; admin row count == 1 per tunnel (no spurious rows).

### 4.4 Keepalive + reaper (I-3)
- **Model:** Sonnet — **Opus review gate** (lifecycle: this is the zombie-entry defense; mirror of the secret-tunnel reaper invariant)
- **Files:** `src/sshgw.rs` (constants from Phase 1.1)
- **Change:** per authenticated connection spawn one supervisor task:
  every `SSH_KEEPALIVE_INTERVAL` (20 s) send the server→client keepalive recorded in SPIKE_FINDINGS.md (global request `keepalive@openssh.com` want_reply, or the documented fallback); track `last_inbound: Arc<AtomicU64>` (millis) updated by every handler callback (data, replies, requests — enumerate the callbacks in a comment); if `now - last_inbound >= SSH_CTRL_TIMEOUT` (60 s) ⇒ `warn!` + hard-disconnect the connection. Disconnect must drop ALL forward states (listeners aborted, admin Registrations dropped, vhost/secret registry entries removed — Phase 5 entries included via the same owner struct). Pre-auth 30 s grace from 4.1 stays separate.
  Comment block: parity table with `CTRL_CLIENT_HEARTBEAT`/`SECRET_CTRL_TIMEOUT` (`src/secret.rs:59,54`) and the CLAUDE.md zombie-entry invariant; check on the keepalive tick, not with `timeout(read)` (same select-starvation trap as the secret reaper).
- **Unit tests:** `should_reap_logic` — pure function `(last_inbound, now) -> bool` table incl. boundary 59.9/60.1 s.
- **e2e tests:** (in `tests/ssh_gateway_test.rs`)
  **T-SSH-PREAUTH1** — raw `TcpStream` to the ssh port, send `SSH-2.0-probe\r\n`, then silence ⇒ socket closed by server within 35 s (assert read returns 0/err before deadline).
  **T-SSH-KEEP1** — idle authenticated `-N -R` session (no tunnel traffic) survives 90 s AND the tunnel still relays afterwards (keepalives counted as activity on the CLIENT side too: run ssh with `ServerAliveInterval=15` so both directions stay warm).
  Half-open post-auth reap needs packet drop ⇒ netns only: deferred to **T-SSH-N1** (Phase 7.2) — noted here so nobody "fixes" the gap early.
- **Done:** gates green; T-SSH-PREAUTH1, T-SSH-KEEP1 pass; code-review confirms every registration lives inside the per-connection owner dropped on disconnect.

### 4.5 Admin `Entry` additive fields
- **Model:** Haiku
- **Files:** `src/admin.rs:43-122` (Entry), `src/admin.rs:289` region (NewEntry/register), `src/admin_api.rs` (JSON serialization of entries — locate the Entry→JSON mapping and extend)
- **Change:** add `pub transport: Transport` (`pub enum Transport { Bore, Ssh }`, default `Bore`) and `pub identity: Option<String>` to `Entry` + `NewEntry`; serialize as additive JSON fields `"transport": "bore"|"ssh"`, `"identity"`. Every existing construction site sets `Transport::Bore` via `Default`/struct-update — do not touch call-site semantics. FE rendering is Phase 7.1.
- **Unit tests:** extend `tests/admin_test.rs` with `admin_entry_transport_serialized` — a registered entry's JSON contains `"transport":"bore"` by default; existing tests unmodified and green.
- **e2e tests:** asserted inside T-SSH-PUB1 (`transport == "ssh"`).
- **Done:** gates green; `tests/admin_test.rs` green with only the one additive test added.

---

## Phase gates

- **Fmt:** `cargo fmt`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --features ssh-gateway --test ssh_gateway_test --test ssh_gateway_spike_test` + full `cargo test --all-features`
- **Regression guard:** full default-features `cargo test`; netns not required this phase (no native-path file touched except admin.rs additive fields).

## Phase done criterion

Reference-scenario line 2 (public via `ssh -R 9005`) works end-to-end on `--ssh-port`: T-SSH-PUB1..3, T-SSH-WARN1, T-SSH-PREAUTH1, T-SSH-KEEP1 all green; a killed session frees its port and admin row.
