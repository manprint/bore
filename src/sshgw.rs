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

/// Where a granted `tcpip-forward` request routes to (D1's address grammar):
/// a native `bore local` public tunnel, a vhost subdomain, or a secret-tunnel
/// provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardSpec {
    /// Public tunnel. `port` is the requested port; `0` means "assign one
    /// from the server's port range".
    Public {
        /// Requested port, or 0 to auto-assign.
        port: u16,
    },
    /// Vhost subdomain forward.
    Vhost {
        /// Subdomain label: lowercase `[a-z0-9-]+`, single label, same
        /// charset as [`crate::vhost::extract_subdomain`].
        label: String,
    },
    /// Secret-tunnel provider forward.
    SecretProvider {
        /// Secret tunnel id, same charset as a vhost label.
        id: String,
    },
}

/// Error parsing a `tcpip-forward`/`direct-tcpip` address into a
/// [`ForwardSpec`] or secret-consumer target id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecError(pub String);

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SpecError {}

/// Validates a vhost/secret label: lowercase `[a-z0-9-]+`, single label (no
/// dots), not starting or ending with `-` — the exact charset
/// `vhost::extract_subdomain` (`src/vhost.rs`) accepts.
fn validate_label(label: &str) -> Result<String, SpecError> {
    let valid = !label.is_empty()
        && !label.contains('.')
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !label.starts_with('-')
        && !label.ends_with('-');
    if valid {
        Ok(label.to_string())
    } else {
        Err(SpecError(format!(
            "invalid label {label:?}: must be lowercase [a-z0-9-]+, a single label, no leading/trailing hyphen"
        )))
    }
}

/// Parses a `tcpip-forward` bind address/port into a [`ForwardSpec`] (D1):
/// - empty / `localhost` / `127.0.0.1` / `0.0.0.0` / `*` → [`ForwardSpec::Public`];
/// - `vhost/<label>` → [`ForwardSpec::Vhost`], any port;
/// - `secret/<id>` → [`ForwardSpec::SecretProvider`], any port;
/// - a bare label on port 80/443 → [`ForwardSpec::Vhost`];
/// - a bare label on port 0 → [`ForwardSpec::SecretProvider`];
/// - a bare label on any other port is ambiguous and rejected — use a
///   `vhost/` or `secret/` prefix to disambiguate.
pub fn parse_forward_spec(addr: &str, port: u32) -> Result<ForwardSpec, SpecError> {
    let port16 = u16::try_from(port).map_err(|_| SpecError(format!("port {port} out of range")))?;

    if addr.is_empty() || matches!(addr, "localhost" | "127.0.0.1" | "0.0.0.0" | "*") {
        return Ok(ForwardSpec::Public { port: port16 });
    }
    if let Some(label) = addr.strip_prefix("vhost/") {
        return validate_label(label).map(|label| ForwardSpec::Vhost { label });
    }
    if let Some(id) = addr.strip_prefix("secret/") {
        return validate_label(id).map(|id| ForwardSpec::SecretProvider { id });
    }
    match port16 {
        80 | 443 => validate_label(addr).map(|label| ForwardSpec::Vhost { label }),
        0 => validate_label(addr).map(|id| ForwardSpec::SecretProvider { id }),
        _ => Err(SpecError(format!(
            "ambiguous forward address {addr:?} on port {port16}; use a vhost/ or secret/ prefix"
        ))),
    }
}

/// Parses a `direct-tcpip` destination host/port into a secret-consumer
/// target id (Phase 5.3 routes `ssh -L` through this). Only `<id>` or
/// `secret/<id>` on port 0 are accepted; anything else is rejected with a
/// message suitable for the channel-open failure reason.
pub fn parse_direct_tcpip_dest(host: &str, port: u32) -> Result<String, SpecError> {
    if port != 0 {
        return Err(SpecError(format!(
            "direct-tcpip to {host}:{port} not supported; use port 0 with a secret tunnel id"
        )));
    }
    let id = host.strip_prefix("secret/").unwrap_or(host);
    validate_label(id).map_err(|_| SpecError(format!("invalid secret tunnel id {host:?}")))
}

/// Client-transport-only keys: features the native `bore` client implements
/// that have no equivalent over SSH ingress. Recognized so they produce a
/// clear warning instead of a silent no-op or an "unknown parameter" one.
const TRANSPORT_ONLY_KEYS: &[&str] = &[
    "udp",
    "carriers",
    "stun-server",
    "upnp",
    "try-port-prediction",
    "nat-udp-preferred-port",
    "auto-reconnect",
];

/// Per-forward parameters parsed from an `exec` request string and/or
/// `env` requests, merged with a [`crate::sshgw_auth::KeyGrant`]'s own
/// options (precedence: grant > exec > env, per I-2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params {
    /// Free-text notes for the admin dashboard.
    pub notes: Option<String>,
    /// Per-tunnel connection cap.
    pub max_conns: Option<usize>,
    /// HTTP basic-auth credentials (`user:pass`) for a vhost forward.
    pub basic_auth: Option<String>,
    /// Enable per-tunnel access logging.
    pub webserver_log: bool,
    /// Explicit tunnel id override.
    pub id: Option<String>,
    /// One warning per unsupported or unrecognized key, in encounter order —
    /// nothing is silently dropped (I-2).
    pub warnings: Vec<String>,
}

/// Splits a `key=value ...` string into tokens, honoring double-quoted
/// values that may contain spaces (e.g. `notes="two words"`). Quote
/// characters themselves are stripped, not part of the token.
fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Splits each whitespace-delimited (quote-aware) token on its first `=`
/// into a `(key, value)` pair. Tokens without an `=` are dropped.
fn parse_kv_tokens(s: &str) -> Vec<(String, String)> {
    tokenize(s)
        .into_iter()
        .filter_map(|tok| {
            tok.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// Maps `BORE_<KEY>` environment entries to the same `key=value` grammar as
/// `exec` params (e.g. `BORE_MAX_CONNS` → `max-conns`). Entries without the
/// `BORE_` prefix are ignored — they are not bore parameters.
fn env_params(env: &[(String, String)]) -> Vec<(String, String)> {
    env.iter()
        .filter_map(|(k, v)| {
            k.strip_prefix("BORE_")
                .map(|rest| (rest.to_ascii_lowercase().replace('_', "-"), v.clone()))
        })
        .collect()
}

/// Parses `exec`/`env` request data into [`Params`], applying the SSH
/// gateway's key=value grammar (I-2). Precedence is grant > exec > env: a
/// [`crate::sshgw_auth::KeyGrant`]'s own `max-conns`/`notes` always win, an
/// `exec` value wins over the same key set via `env`, and any key naming a
/// client-transport-only feature or that isn't recognized at all produces a
/// warning rather than being silently accepted or dropped.
pub fn parse_params(
    exec: Option<&str>,
    env: &[(String, String)],
    grant: &crate::sshgw_auth::KeyGrant,
) -> Params {
    let mut merged: Vec<(String, String)> = env_params(env);
    if let Some(exec) = exec {
        merged.extend(parse_kv_tokens(exec));
    }

    let mut params = Params::default();
    for (key, value) in &merged {
        match key.as_str() {
            "notes" => params.notes = Some(value.clone()),
            "max-conns" => match value.parse() {
                Ok(n) => params.max_conns = Some(n),
                Err(_) => params
                    .warnings
                    .push(format!("max-conns: invalid value {value:?}")),
            },
            "basic-auth" => params.basic_auth = Some(value.clone()),
            "webserver-log" => params.webserver_log = value == "on",
            "id" => params.id = Some(value.clone()),
            k if TRANSPORT_ONLY_KEYS.contains(&k) => params.warnings.push(format!(
                "{k}: not available via SSH ingress; use the native bore client"
            )),
            k => params.warnings.push(format!("{k}: unknown parameter")),
        }
    }

    if let Some(max_conns) = grant.max_conns {
        params.max_conns = Some(max_conns);
    }
    if let Some(notes) = &grant.notes {
        params.notes = Some(notes.clone());
    }

    params
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

    fn grant(identity: &str) -> crate::sshgw_auth::KeyGrant {
        crate::sshgw_auth::KeyGrant {
            identity: identity.to_string(),
            permit: None,
            max_conns: None,
            notes: None,
        }
    }

    #[test]
    fn spec_matrix() {
        let ok_cases = [
            ("", 9005, ForwardSpec::Public { port: 9005 }),
            ("localhost", 9005, ForwardSpec::Public { port: 9005 }),
            ("127.0.0.1", 9005, ForwardSpec::Public { port: 9005 }),
            ("0.0.0.0", 9005, ForwardSpec::Public { port: 9005 }),
            ("*", 9005, ForwardSpec::Public { port: 9005 }),
            (
                "vhost/foo",
                9005,
                ForwardSpec::Vhost {
                    label: "foo".to_string(),
                },
            ),
            (
                "vhost/foo",
                0,
                ForwardSpec::Vhost {
                    label: "foo".to_string(),
                },
            ),
            (
                "secret/bar",
                9005,
                ForwardSpec::SecretProvider {
                    id: "bar".to_string(),
                },
            ),
            (
                "secret/bar",
                0,
                ForwardSpec::SecretProvider {
                    id: "bar".to_string(),
                },
            ),
            (
                "mysub",
                80,
                ForwardSpec::Vhost {
                    label: "mysub".to_string(),
                },
            ),
            (
                "mysub",
                443,
                ForwardSpec::Vhost {
                    label: "mysub".to_string(),
                },
            ),
            (
                "tcp-id",
                0,
                ForwardSpec::SecretProvider {
                    id: "tcp-id".to_string(),
                },
            ),
        ];
        for (addr, port, expected) in ok_cases {
            assert_eq!(
                parse_forward_spec(addr, port).unwrap(),
                expected,
                "addr={addr:?} port={port}"
            );
        }

        let err_cases = [
            ("mysub", 8080),  // ambiguous: bare label, non-80/443/0 port
            ("My_Sub", 80),   // uppercase/underscore not allowed
            ("a.b", 80),      // dot not allowed in a single label
            ("-bad", 80),     // leading hyphen not allowed
            ("bad-", 443),    // trailing hyphen not allowed
            ("vhost/", 9005), // empty label after prefix
        ];
        for (addr, port) in err_cases {
            assert!(
                parse_forward_spec(addr, port).is_err(),
                "addr={addr:?} port={port} should be rejected"
            );
        }
    }

    #[test]
    fn params_precedence() {
        let mut g = grant("id1");
        g.max_conns = Some(3);
        let params = parse_params(Some("max-conns=9"), &[], &g);
        assert_eq!(params.max_conns, Some(3), "grant value must win over exec");

        let env = [("BORE_MAX_CONNS".to_string(), "7".to_string())];
        let params = parse_params(None, &env, &grant("id2"));
        assert_eq!(
            params.max_conns,
            Some(7),
            "env value must be used when exec is absent"
        );
    }

    #[test]
    fn params_quoting() {
        let params = parse_params(
            Some(r#"notes="two words" basic-auth=u:p"#),
            &[],
            &grant("id"),
        );
        assert_eq!(params.notes.as_deref(), Some("two words"));
        assert_eq!(params.basic_auth.as_deref(), Some("u:p"));
        assert!(params.warnings.is_empty());
    }

    #[test]
    fn params_warnings_for_transport_keys() {
        let params = parse_params(Some("udp=on carriers=4"), &[], &grant("id"));
        assert_eq!(params.warnings.len(), 2);
        assert!(params.warnings[0].contains("udp"));
        assert!(params.warnings[1].contains("carriers"));
        assert!(params
            .warnings
            .iter()
            .all(|w| w.contains("not available via SSH ingress")));
    }

    #[test]
    fn direct_tcpip_dest() {
        assert_eq!(
            parse_direct_tcpip_dest("tcp-id", 0).unwrap(),
            "tcp-id".to_string()
        );
        assert_eq!(
            parse_direct_tcpip_dest("secret/tcp-id", 0).unwrap(),
            "tcp-id".to_string()
        );
        assert!(parse_direct_tcpip_dest("example.com", 80).is_err());
    }
}
