//! Embedded SSH ingress gateway (russh-backed) for `bore server`.
//!
//! Lets a stock OpenSSH client create public, vhost and secret tunnels with
//! `ssh -R`/`-L` and no `bore` binary on the client side. The gateway is
//! ingress-only: from the accepted SSH channel inward, the existing server
//! data path (registries, relay, admin, weblog, `--max-conns`) is reused
//! unmodified. See `docs/SSH_GATEWAY.md` for the design and
//! `docs/plans/plan_SshGateway/` for the implementation plan.

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Handler, Msg, Session};
use russh::{Channel, ChannelId};
use tokio::sync::Semaphore;
use tracing::info;

use crate::admin::AdminRegistry;
use crate::secret;
use crate::sshgw_auth::{KeyStore, PasswordStore};
use crate::vhost::VhostRegistry;

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

/// Grace period given to a freshly-accepted connection to complete
/// authentication before it is disconnected.
pub const SSH_PREAUTH_GRACE: Duration = Duration::from_secs(30);

/// Maximum number of authentication attempts (any method) allowed on one
/// connection before russh disconnects it.
pub const SSH_MAX_AUTH_ATTEMPTS: usize = 3;

/// One-line message written to the channel (then EOF+close) when a client
/// requests an interactive shell — the gateway is ingress-only and never
/// grants one.
const SHELL_DENIED_MESSAGE: &str =
    "bore ssh-gateway: interactive shells are not supported; use -R/-L forwarding.\r\n";

/// Validated configuration for the embedded SSH gateway, built from
/// `bore server`'s `--ssh-*` flags.
#[derive(Debug, Clone)]
pub struct SshGatewayConfig {
    /// Dedicated TCP port the gateway listens on, if any. `None` means the
    /// gateway is enabled but reachable only once control-port demux lands
    /// (a later phase, D8) — never a startup error.
    pub port: Option<u16>,
    /// Path to the ed25519 host key (PEM, OpenSSH format). Generated on first
    /// use if it does not exist yet (D9).
    pub host_key_file: PathBuf,
    /// Directory of `authorized_keys`-format files granting public-key auth.
    pub authorized_keys_dir: Option<PathBuf>,
    /// Argon2id password file granting password auth.
    pub passwords_file: Option<PathBuf>,
    /// Banner text sent to clients before authentication.
    pub banner: Option<String>,
}

impl SshGatewayConfig {
    /// Fail fast on a configuration that could never authenticate anyone.
    pub fn validate(&self) -> Result<()> {
        if self.authorized_keys_dir.is_none() && self.passwords_file.is_none() {
            bail!(
                "--ssh-gateway requires --ssh-authorized-keys-dir and/or \
                 --ssh-passwords-file (no credential source configured)"
            );
        }
        Ok(())
    }
}

/// Load the ed25519 host key from `path`, generating and persisting a fresh
/// one (mode 0600, via [`PrivateKey::write_openssh_file`]) if it does not
/// exist yet. Logs the SHA256 fingerprint either way (D9).
fn load_or_generate_host_key(path: &Path) -> Result<PrivateKey> {
    let key = if path.exists() {
        PrivateKey::read_openssh_file(path)
            .with_context(|| format!("failed to read SSH host key {}", path.display()))?
    } else {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .context("failed to generate ed25519 SSH host key")?;
        key.write_openssh_file(path, LineEnding::LF)
            .with_context(|| format!("failed to write SSH host key {}", path.display()))?;
        info!(path = %path.display(), "ssh-gateway: generated new ed25519 host key");
        key
    };
    let fingerprint = key.fingerprint(HashAlg::Sha256);
    info!(path = %path.display(), %fingerprint, "ssh-gateway: host key ready");
    Ok(key)
}

/// The embedded SSH gateway: host key, credential stores, and the registries/
/// helpers tunnel serving needs. Constructed once from `Server::set_ssh_gateway`
/// and shared (via `Arc`) across every accepted connection; never re-derives
/// its registries — they are clones of the `Server`'s own.
pub struct SshGateway {
    config: SshGatewayConfig,
    host_key: PrivateKey,
    keys: Option<KeyStore>,
    passwords: Option<PasswordStore>,
    /// Wired for Phase 4.3 (`tcpip_forward` public-tunnel handling).
    #[allow(dead_code)]
    providers: secret::Registry,
    /// Wired for Phase 5 (vhost mapping).
    #[allow(dead_code)]
    vhost_registry: VhostRegistry,
    /// Wired for Phase 4.3 (admin registration, `transport: Ssh`).
    #[allow(dead_code)]
    admin: AdminRegistry,
    /// Wired for Phase 4.3 (per-connection inbound cap, shared with the rest
    /// of the server's `--max-conns`).
    #[allow(dead_code)]
    conn_permits: Arc<Semaphore>,
    /// Wired for Phase 4.2 (`permit="port/<n>"` range validation).
    #[allow(dead_code)]
    port_range: RangeInclusive<u16>,
    /// Wired for Phase 4.3 (public-tunnel listener bind address).
    #[allow(dead_code)]
    bind_tunnels: std::net::IpAddr,
}

impl SshGateway {
    /// Build the gateway: validates `config`, loads/generates the host key,
    /// and wires the credential stores. `providers`/`vhost_registry`/`admin`/
    /// `conn_permits`/`port_range`/`bind_tunnels` must be clones of the
    /// `Server`'s own — never re-derived.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: SshGatewayConfig,
        providers: secret::Registry,
        vhost_registry: VhostRegistry,
        admin: AdminRegistry,
        conn_permits: Arc<Semaphore>,
        port_range: RangeInclusive<u16>,
        bind_tunnels: std::net::IpAddr,
    ) -> Result<Self> {
        config.validate()?;
        let host_key = load_or_generate_host_key(&config.host_key_file)?;
        let keys = config.authorized_keys_dir.clone().map(KeyStore::new);
        let passwords = config.passwords_file.clone().map(PasswordStore::new);
        Ok(Self {
            config,
            host_key,
            keys,
            passwords,
            providers,
            vhost_registry,
            admin,
            conn_permits,
            port_range,
            bind_tunnels,
        })
    }

    /// Dedicated TCP port the gateway listens on, if any.
    pub fn port(&self) -> Option<u16> {
        self.config.port
    }

    /// A fresh `russh::server::Config` for one accepted connection: the
    /// loaded host key, the pre-auth grace period, and the auth-attempt cap.
    /// Keepalive tuning (`keepalive_interval`/`keepalive_max`) is set in
    /// Phase 4.4 (I-SSH3).
    pub fn russh_config(&self) -> Arc<russh::server::Config> {
        Arc::new(russh::server::Config {
            keys: vec![self.host_key.clone()],
            inactivity_timeout: Some(SSH_PREAUTH_GRACE),
            max_auth_attempts: SSH_MAX_AUTH_ATTEMPTS,
            ..Default::default()
        })
    }

    /// A fresh per-connection [`Handler`] bound to this gateway.
    pub fn handler(self: &Arc<Self>) -> GatewayHandler {
        GatewayHandler {
            gateway: Arc::clone(self),
            identity: None,
        }
    }
}

/// Per-connection `russh::server::Handler`. Holds only what one connection
/// needs; everything shared lives on [`SshGateway`].
pub struct GatewayHandler {
    gateway: Arc<SshGateway>,
    /// Identity granted by a successful auth (authorized-keys comment/
    /// fingerprint, or the matched password label). `None` until authenticated.
    identity: Option<String>,
}

impl GatewayHandler {
    /// Identity granted by a successful auth, if any. Wired for Phase 4.3
    /// (admin registration, per-forward logging).
    pub fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }
}

impl Handler for GatewayHandler {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        let Some(keys) = &self.gateway.keys else {
            return Ok(Auth::reject());
        };
        match keys.check(public_key) {
            Some(grant) => {
                self.identity = Some(grant.identity);
                Ok(Auth::Accept)
            }
            None => Ok(Auth::reject()),
        }
    }

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        let Some(passwords) = &self.gateway.passwords else {
            return Ok(Auth::reject());
        };
        match passwords.check(password).await {
            Some(label) => {
                self.identity = Some(label);
                Ok(Auth::Accept)
            }
            None => Ok(Auth::reject()),
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(channel, SHELL_DENIED_MESSAGE.as_bytes().to_vec())?;
        session.exit_status_request(channel, 1)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn base_config(dir: &Path) -> SshGatewayConfig {
        SshGatewayConfig {
            port: None,
            host_key_file: dir.join("host_key.pem"),
            authorized_keys_dir: None,
            passwords_file: None,
            banner: None,
        }
    }

    fn build(config: SshGatewayConfig) -> Result<SshGateway> {
        SshGateway::new(
            config,
            secret::Registry::default(),
            VhostRegistry::default(),
            AdminRegistry::default(),
            Arc::new(Semaphore::new(1)),
            1024..=65535,
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        )
    }

    #[test]
    fn sshgw_config_validation() {
        let dir = tempfile::tempdir().unwrap();

        let cfg = base_config(dir.path());
        assert!(cfg.validate().is_err(), "no auth source must be rejected");

        let mut cfg = base_config(dir.path());
        cfg.authorized_keys_dir = Some(dir.path().join("keys"));
        assert!(cfg.validate().is_ok(), "keys-dir alone is sufficient");

        let mut cfg = base_config(dir.path());
        cfg.passwords_file = Some(dir.path().join("passwords"));
        assert!(cfg.validate().is_ok(), "passwords-file alone is sufficient");

        let mut cfg = base_config(dir.path());
        cfg.authorized_keys_dir = Some(dir.path().join("keys"));
        cfg.passwords_file = Some(dir.path().join("passwords"));
        assert!(cfg.validate().is_ok(), "both sources together are fine");
    }

    #[test]
    fn host_key_generated_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = base_config(dir.path());
        cfg.authorized_keys_dir = Some(dir.path().join("keys"));

        assert!(!cfg.host_key_file.exists());
        let gateway = build(cfg.clone()).expect("first construction generates a host key");
        assert!(cfg.host_key_file.exists());
        let first = gateway.host_key.fingerprint(HashAlg::Sha256);

        let gateway2 = build(cfg).expect("second construction reloads the same host key");
        let second = gateway2.host_key.fingerprint(HashAlg::Sha256);
        assert_eq!(
            first, second,
            "reloaded host key must have the same fingerprint"
        );
    }
}
