# SSH gateway — russh 0.62.1 API spike findings

Phase 1.2 deliverable. Produced by writing and running a real integration test
(`tests/ssh_gateway_spike_test.rs`, T-SSH-SPIKE1..5) against the actual OpenSSH
CLI (`ssh`/`ssh-keygen`) driving an embedded `russh::server` on an ephemeral
127.0.0.1 port. Phases 4-6 should read this file instead of re-discovering the
API from scratch.

Pinned versions (resolved, `cargo tree --features ssh-gateway`):
- `russh v0.62.1`
- `russh::keys` = re-exported `ssh-key v0.7.0-rc.11` (NOT a separate `russh-keys`
  crate — that crate no longer exists for this russh version).
- Crypto backend: exactly one, `ring v0.17.14`, confirmed via
  `cargo tree --features ssh-gateway -i ring` / `-i aws-lc-rs` (the latter empty).
  Forcing `russh = { default-features = false, features = ["ring", "flate2", "rsa"] }`
  in `Cargo.toml` works as planned — no duplicate crypto backend links.
- Minor, benign duplication: `ssh-key`/russh's own encrypted-private-key support
  pulls `argon2 v0.6.0-rc.8` transitively, alongside our direct `argon2 = "0.5"`
  dependency (for Phase 3's `PasswordStore`). Two different major versions of the
  same hashing crate compile side by side — extra binary size only, no interop or
  security concern (we never call the transitive one).

## Per-primitive findings

**T-SSH-SPIKE1 (pubkey auth).** `Handler::auth_publickey(&mut self, user: &str,
public_key: &ssh_key::PublicKey) -> Result<Auth, Self::Error>` is called AFTER
russh has already verified the client's signature — the handler only decides
accept/reject by identity. **Trap:** `PublicKey` derives `PartialEq` over ALL
fields including `comment`, but a comment is a local file-format artifact never
sent over the wire — the offered key's comment is always empty, so comparing
`offered_key == stored_key` (loaded from an authorized_keys-style file via
`PrivateKey::read_openssh_file(..).public_key().clone()`) silently rejects every
legitimate key. **Fix:** compare `.key_data()` (a `KeyData`, `PartialEq`-derived,
comment-free), not the whole `PublicKey`. Phase 3's `KeyStore` lookup MUST key/
compare on `key_data()`, never the full struct.
`Auth::Accept` / `Auth::reject()` are the two constructors used.

**T-SSH-SPIKE2 (tcpip-forward + forwarded-tcpip, `-R`).** `Handler::tcpip_forward
(&mut self, address: &str, port: &mut u32, session: &mut Session) -> Result<bool,
_>` — write the allocated port back through `*port` when the client asked for
`0`; return `Ok(true)` to accept (russh sends `REQUEST_SUCCESS` with the port
only if the client asked for `0` AND wants a reply). The SERVER (not the client)
initiates the actual data channel back: `session.handle()` returns a clonable
`server::Handle`; `Handle::channel_open_forwarded_tcpip(connected_address,
connected_port, originator_address, originator_port) -> Result<Channel<Msg>,
Error>` opens it, and `channel.into_stream()` yields a plain `AsyncRead +
AsyncWrite` (`ChannelStream`). This must run from a task spawned OFF the handler
call (not blocking `tcpip_forward` itself) since the client only starts servicing
`-R` once it has actually seen the `REQUEST_SUCCESS` packet, which russh doesn't
flush until the handler future returns. A short delay (test used 300 ms) before
opening the forwarded channel is a workaround only needed because the test
opens it eagerly with no other signal; the real gateway (Phase 4) has no such
race because it opens the channel lazily, on each real inbound public/vhost/
secret connection, which is always causally after the client's forward is live.

**T-SSH-SPIKE3 (direct-tcpip, `-L`).** `Handler::channel_open_direct_tcpip(&mut
self, channel: Channel<Msg>, host_to_connect: &str, port_to_connect: u32,
originator_address: &str, originator_port: u32, reply: ChannelOpenHandle,
session: &mut Session)` — note the channel is handed to you ALREADY (unlike
`tcpip_forward`, there is no separate "open" call); `reply.accept().await` (or
`reply.reject(ChannelOpenFailure)`) is purely the accept/reject signal, and
`channel.into_stream()` is the data path, same `ChannelStream` type as SPIKE2.
**Deviation from `docs/SSH_GATEWAY.md`/plan assumption:** the plan's spec text
used `-L <lport>:testname:0` (target port `0`, mirroring `-R`'s `:0` meaning
"let the far end pick"). OpenSSH's CLIENT-SIDE parser rejects this outright
before ever contacting the server — `Bad local forwarding specification
'PORT:testname:0'` — because for `-L`/`-D` a `:0` remote target port has no
defined meaning (only `-R`'s LISTEN port may be `0`). Any real usage docs
(Phase 4/7 user-facing docs) must use a real, nonzero placeholder port in the
forward-spec examples; the gateway's own port-parsing logic (D1, Phase 4.2)
never needs to special-case a `0` target port for direct-tcpip.

**T-SSH-SPIKE4 (exec + env).** `Handler::env_request(&mut self, channel:
ChannelId, variable_name: &str, variable_value: &str, session: &mut Session)`
and `Handler::exec_request(&mut self, channel: ChannelId, data: &[u8], session:
&mut Session)` both require the implementor to explicitly call
`session.channel_success(channel)` / `channel_failure(channel)` — there is no
implicit success. `session.data(channel, bytes)` writes channel data toward the
client (used here to prove server→client output); `session.exit_status_request
(channel, code)`, `session.eof(channel)`, `session.close(channel)` end the
channel (must be called in that order for the client to read a clean exit code).
**Observed actual client behavior:** with `-o SetEnv=BORE_NOTES=spike`, the
handler received TWO env requests, not one: `LANG=<inherited-client-locale>`
(OpenSSH forwards locale vars by default via its built-in `SendEnv LANG LC_*`)
AND `BORE_NOTES=spike`. Phase 4/6 gateway code must not assume a fixed/known
set of env vars, or reject on an unexpected one — accept and ignore anything
it doesn't specifically use (e.g. `channel_success` unconditionally, record
only recognized names).

**T-SSH-SPIKE5 (keepalive / I-3).** Two independent, complementary mechanisms,
neither requiring hand-rolled timers:
1. **Server→client (russh built-in, `Config.keepalive_interval: Option<Duration>`
   + `Config.keepalive_max: usize`, default `None`/`3`).** "If nothing is
   received from the client for this amount of time, send a keepalive message
   [and] if this many keepalives have been sent without reply, close the
   connection." This is exactly I-3's zombie-entry reaper requirement — Phase
   4.4 should SET these two `Config` fields (parity with `SSH_KEEPALIVE_INTERVAL`
   / an equivalent max count for `SSH_CTRL_TIMEOUT`) instead of writing a custom
   `last_recv` timestamp + heartbeat tick like `secret.rs` does for the plain-TCP
   control loop. No handler code needed at all for this direction.
2. **Client→server (OpenSSH `ServerAliveInterval`/`ServerAliveCountMax`, tested
   here).** The client sends a `keepalive@openssh.com` GLOBAL_REQUEST with
   `want_reply=1`. russh's server loop already replies `REQUEST_FAILURE` to any
   *unrecognized* global request type by default (see `src/server/encrypted.rs`,
   the catch-all arm) — OpenSSH's client treats ANY reply (success or failure) as
   proof of liveness, so this direction needs **zero gateway code**: the spike
   test's session survived 5 s of `ServerAliveInterval=1` with no `global_request`
   override in the handler at all.
   Both directions were exercised together in the same test (server
   `keepalive_interval=500ms`/`keepalive_max=4` AND client
   `ServerAliveInterval=1`/`ServerAliveCountMax=2`) with no interference.

## Other API notes for Phases 4-6

- Single-connection entry point is `russh::server::run_stream(config: Arc<Config>,
  stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static, handler: H) ->
  Result<RunningSession<H>, H::Error>` — no `Server` factory trait needed when
  accepting off a plain `TcpListener` loop and constructing one fresh `Handler`
  per connection (the higher-level `server::Server` trait / `server::run()` is
  only needed for its listener-management convenience, which the gateway does
  not need since Phase 6 owns the accept loop for the control-port demux anyway).
  `run_stream` spawns the actual session-processing task internally and returns
  immediately after the SSH version exchange; the returned `RunningSession` is
  itself a `Future` (poll it, e.g. via `tokio::spawn`, to observe/propagate a
  terminal error — dropping it does NOT kill the connection, the internal spawn
  already owns it).
- Host key loading: generate/load via `russh::keys::PrivateKey::read_openssh_file
  (path)` (an `ssh-key` passthrough) — no need for `PrivateKey::random(rng, ..)`
  (which requires the `rand_core` cargo feature and a `CryptoRng` impl we don't
  otherwise depend on). Phase 4.1's host-key-file config flag should load this
  way, matching what the spike does for both host and client test keys.
- `ChannelOpenHandle` (`server::ChannelOpenHandle`) is consumed by `.accept()`/
  `.reject(ChannelOpenFailure)` (both `async`, `self`-consuming); dropping it
  without calling either auto-rejects with `AdministrativelyProhibited` — a
  useful safety net, not a footgun, for early-return error paths.
- `ssh_key::authorized_keys` module (re-exported at `russh::keys::authorized_keys`
  in 0.7.0-rc.11, same API as the 0.6.7 docs) provides `AuthorizedKeys::new(&str)`
  (an `Iterator<Item = Result<Entry>>`) and `Entry::config_opts() ->
  &ConfigOpts`/`ConfigOpts::iter() -> ConfigOptsIter` for quote-aware option
  tokenization. Phase 3.1's `KeyStore` should reuse this instead of hand-rolling
  an authorized_keys line parser as the phase_03.md draft text sketches — one
  less state machine to get wrong on escaped quotes/commas.

## Loud-note check

No spike assertion was impossible with russh. One assumption in the plan's own
spec text (a `:0` target port for `-L`, T-SSH-SPIKE3) was wrong and is corrected
above and in the test; it does not affect any design decision (D1-D12) or
invariant (I-1..I-5) in `overview.md` — it only affects example forward-spec
strings in user-facing docs (Phase 7.4).
