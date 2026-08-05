# Phase 1 — Contracts, protocol and jump-only classic authentication

> **Planning model:** GPT-5/Codex
> **Intent:** establish internal additive types, validation and username-bound
> credential metadata
> without exposing an incomplete command or server flag and without routing
> production traffic yet.
> **Precondition:** owner decisions in `overview.md` are locked.

## 1.1 Red tests for target, alias and hostname/port grammar

- Add pure, table-driven parser tests before the command is exposed:
  - `localhost:22` + `vm-test-01` validates;
  - `localhost:2222` preserves virtual/local port 2222;
  - IPv6 target syntax works through the existing target resolver;
  - uppercase, dotted, empty, over-63-byte and hyphen-edge aliases fail;
  - conflicting embedded host vs any reused local-host value fails.
- Add pure hostname routing tests:
  - exact `vm-test-01.ssh.bore.tld` → `vm-test-01`;
  - case normalization and one trailing dot accepted;
  - nested name, wrong suffix and suffix-confusion rejected;
  - port mismatch rejected;
  - unmatched host is explicitly marked `NotJump`, not a jump parse error, so
    legacy secret parsing can continue unchanged.

## 1.2 Protocol and registry scaffolding

- Add `HelloSshJump` and `SshJumpUdpRenew` before final
  `ClientMessage::Heartbeat`; add `SshJumpReady` and `SshJumpUdp` responses.
- Bound alias/notes and validate nonzero `ssh_port` during decode/registration;
  never preallocate from attacker-controlled lengths.
- Add `SshJumpEntry`, `SshJumpRegistry`, pending direct nonce registry and a
  token-safe deregistration guard skeleton. Keep all behavior disabled until
  Phase 2.
- Add serde round-trip and debug-redaction tests. Prove new messages fit
  `MAX_FRAME_LENGTH`; do not lower the current limit.
- New client against old server must receive a clear registration error/EOF; no
  fallback that accidentally creates a public or secret tunnel.

## 1.3 Internal configuration scaffolding

- Add internal server/config fields for base domain and jump registry,
  with no Clap flags or public enablement yet.
- Constructors default to `None`/disabled so existing server paths remain
  byte-identical and no new startup requirement exists.
- Keep `direct_quic_port` generalization out of this phase; it lands with the
  working QUIC behavior in Phase 4.
- Add sanitized config-view fields with disabled defaults, without secrets or
  credential contents. They become populated only when Phase 2 exposes configuration.

## 1.4 Add jump-only classic credential binding

- Extend the public-key auth result additively with the set/account binding
  derived from a matching username-named authorized-keys file (`<user>` or
  `<user>.pub`). Keep the existing grant identity/comment/fingerprint and its
  current first-match selection untouched even if the same key appears in more
  than one file; compute jump binding as independent metadata.
- Compare the presented username against safe directory-entry filenames already
  returned by `read_dir` (exact, case-sensitive match); never interpolate an
  untrusted username into a filesystem path.
- Add a password-store lookup that verifies the hash for one exact label/username
  without replacing today's scan-any-label lookup.
- During auth, keep today's Accept/Reject decision unchanged and separately set
  `jump_principal = Some(presented_username)` only when the exact key-file or
  password-label binding succeeds.
- Do not require the principal until a future jump operation is requested.
- Tests:
  - key in `authorized_keys.d/fabio` + `fabio@host` sets principal `fabio`;
  - same key + `wrong@host` still authenticates under legacy semantics but has no
    jump principal;
  - password line `fabio:<hash>` behaves identically;
  - comments do not override the username binding;
  - multiple keys in one username file map to the same principal;
  - generic legacy files and every current auth/takeover test stay unchanged;
  - hot reload of key/password stores updates jump binding through their existing
    per-attempt reload behavior, with no new watcher/file.

## Phase 1 gates

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features ssh_jump
cargo test --all-features
```

Done only when scaffolding is behavior-neutral for every existing command,
wire/auth-binding tests pass, no incomplete command/flag is visible in `--help`, and
`resume.md` points to Phase 2. README is unchanged because no behavior/API is
exposed yet.
