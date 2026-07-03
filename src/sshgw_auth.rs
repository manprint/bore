//! Credential stores for the SSH gateway: an authorized-keys directory and an
//! argon2id password file. Both re-read the filesystem on every authentication
//! attempt (hot reload by construction, cached by mtime) so operators can add
//! or revoke credentials without restarting `bore server`. See
//! `docs/SSH_GATEWAY.md` §2.9/§2.10 and `docs/plans/plan_SshGateway/phase_03.md`.
