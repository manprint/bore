//! Embedded SSH ingress gateway (russh-backed) for `bore server`.
//!
//! Lets a stock OpenSSH client create public, vhost and secret tunnels with
//! `ssh -R`/`-L` and no `bore` binary on the client side. The gateway is
//! ingress-only: from the accepted SSH channel inward, the existing server
//! data path (registries, relay, admin, weblog, `--max-conns`) is reused
//! unmodified. See `docs/SSH_GATEWAY.md` for the design and
//! `docs/plans/plan_SshGateway/` for the implementation plan.

use std::time::Duration;

/// Interval between server-initiated SSH keepalive probes on an authenticated
/// gateway connection. Parity with `CTRL_CLIENT_HEARTBEAT` (`src/secret.rs`),
/// deliberately far below `SSH_CTRL_TIMEOUT` so a healthy idle tunnel never
/// trips the reaper.
pub const SSH_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// Silence duration after which an SSH gateway connection is treated as dead
/// and torn down (all its forwards, registry entries and admin rows released).
/// Parity with `SECRET_CTRL_TIMEOUT` (`src/secret.rs`) — the same zombie-entry
/// reaper invariant applies here (I-SSH3).
pub const SSH_CTRL_TIMEOUT: Duration = Duration::from_secs(60);
