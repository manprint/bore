# SSH Jump Host — Resume

> **Last update:** 2026-08-05
> **Planning model:** GPT-5/Codex

## Status

- Owner requirements refined and locked.
- Preliminary repository/documentation/code-path audit complete.
- Overview and Phases 1–5 written.
- `examples_usage.md` written as the Compose/credential/command and E2E
  acceptance contract for native and pure-OpenSSH providers.
- Planning documents validated for balanced code fences, trailing whitespace,
  required files and port-topology consistency.
- Application implementation has not started.
- No tests were run because this turn changed planning documentation only.

## Locked configuration

- SSH gateway: TCP 443 through OpenSSH config alias.
- Jump namespace: `<label>.ssh.bore.tld` (separate from HTTP vhost).
- Access: no separate ACL. Only jump publish/connect requires username-bound
  public-key/password authentication; all existing gateway modes keep today's
  username-ignored behavior.
- Provider auth: existing bore shared secret over TLS control transport.
- Provider modes: native `bore sshjhost` uses that secret and may use QUIC;
  pure OpenSSH `-R jump/...` uses classic username-bound gateway auth and is
  TCP-only. Both share one registry.
- UDP scope: QUIC only on server→provider; warm TCP fallback retained.
- Ports in the owner's real Compose: public SSH/control 443/tcp maps to internal
  7835; STUN remains 7835/udp; sshjhost reuses the existing vhost/public direct
  endpoint on 443/udp. No 8443 mapping or new QUIC-port variable is needed.
- Compose delta: add only
  `BORE_SSH_JUMP_BASE_DOMAIN=ssh.bore.0912345.xyz`; every current port, vhost,
  VPN, SSH-gateway and volume setting remains unchanged.
- Ports: standard and nonstandard supported; virtual port equals TARGET port in v1.

## Next

Await explicit owner approval to begin `phase_01.md` §1.1 (red CLI and hostname
grammar tests). Do not begin implementation automatically.
