//! SSH jump-host contracts shared by the native provider and SSH gateway.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Result};
use dashmap::DashMap;
use tokio::sync::Semaphore;

use crate::pool::CarrierPool;
use crate::shared::{ClientMessage, MAX_NOTES_LEN, UDP_NONCE_LEN};

/// Maximum length of the one-label jump alias carried in a DNS-style hostname.
pub const MAX_ALIAS_LEN: usize = 63;

/// Maximum encoded length of the provider's local target hostname.
pub const MAX_LOCAL_HOST_LEN: usize = 255;

/// A destination routed through the jump namespace, or an unrelated hostname
/// that must continue through the legacy SSH-gateway parser unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JumpRoute {
    /// The hostname does not belong to the configured jump base domain.
    NotJump,
    /// The hostname is one valid jump alias and the requested port matches.
    Match {
        /// Normalized lowercase alias.
        alias: String,
        /// Requested and registered SSH port.
        port: u16,
    },
}

/// Validate and return one lowercase DNS label suitable for a jump alias.
pub fn validate_alias(alias: &str) -> Result<String> {
    let valid = !alias.is_empty()
        && alias.len() <= MAX_ALIAS_LEN
        && alias
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !alias.starts_with('-')
        && !alias.ends_with('-');
    if !valid {
        bail!(
            "invalid SSH jump alias {alias:?}: expected one lowercase [a-z0-9-] DNS label (max {MAX_ALIAS_LEN} bytes, no leading/trailing hyphen)"
        );
    }
    Ok(alias.to_string())
}

fn normalize_base_domain(base_domain: &str) -> Result<String> {
    let normalized = base_domain
        .strip_suffix('.')
        .unwrap_or(base_domain)
        .to_ascii_lowercase();
    if normalized.is_empty()
        || normalized.split('.').any(|label| {
            label.is_empty()
                || label.len() > MAX_ALIAS_LEN
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        bail!("invalid SSH jump base domain {base_domain:?}");
    }
    Ok(normalized)
}

/// Classify an OpenSSH `direct-tcpip` destination against one jump base domain.
///
/// Hostnames outside the exact suffix return [`JumpRoute::NotJump`] so callers
/// can execute the existing secret-consumer parser unchanged. A hostname inside
/// the suffix is fail-closed when its alias or requested port is invalid.
pub fn classify_destination(
    hostname: &str,
    requested_port: u16,
    base_domain: &str,
    registered_port: u16,
) -> Result<JumpRoute> {
    if requested_port == 0 || registered_port == 0 {
        bail!("SSH jump ports must be nonzero");
    }
    let base = normalize_base_domain(base_domain)?;
    let host = hostname
        .strip_suffix('.')
        .unwrap_or(hostname)
        .to_ascii_lowercase();
    if host == base {
        bail!("SSH jump destination is missing its alias");
    }
    let suffix = format!(".{base}");
    let Some(alias) = host.strip_suffix(&suffix) else {
        return Ok(JumpRoute::NotJump);
    };
    let alias = validate_alias(alias)?;
    if requested_port != registered_port {
        bail!("SSH jump port mismatch: requested {requested_port}, registered {registered_port}");
    }
    Ok(JumpRoute::Match {
        alias,
        port: requested_port,
    })
}

/// Validated metadata sent by a native `bore sshjhost` provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshJumpRegistration {
    /// Registry alias (one lowercase DNS label).
    pub alias: String,
    /// Virtual SSH destination port exposed through ProxyJump.
    pub ssh_port: u16,
    /// Optional bounded operator note.
    pub notes: Option<String>,
    /// Requested number of TCP/direct carriers.
    pub carriers: u16,
    /// Whether the native provider requested direct QUIC.
    pub udp: bool,
    /// Whether the native provider requested automatic reconnect.
    pub auto_reconnect: bool,
    /// Provider-local target host.
    pub local_host: String,
    /// Provider-local target port; equal to `ssh_port` in v1.
    pub local_port: u16,
}

impl SshJumpRegistration {
    /// Validate provider metadata before it can enter the registry.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        alias: &str,
        ssh_port: u16,
        notes: Option<&str>,
        carriers: u16,
        udp: bool,
        auto_reconnect: bool,
        local_host: &str,
        local_port: u16,
    ) -> Result<Self> {
        let alias = validate_alias(alias)?;
        if ssh_port == 0 || local_port == 0 {
            bail!("SSH jump ports must be nonzero");
        }
        if ssh_port != local_port {
            bail!(
                "SSH jump virtual port {ssh_port} must equal local target port {local_port} in v1"
            );
        }
        if local_host.is_empty() || local_host.len() > MAX_LOCAL_HOST_LEN {
            bail!("SSH jump local host must contain 1..={MAX_LOCAL_HOST_LEN} encoded bytes");
        }
        if notes.is_some_and(|value| value.chars().count() > MAX_NOTES_LEN) {
            bail!("SSH jump notes exceed {MAX_NOTES_LEN} characters");
        }
        Ok(Self {
            alias,
            ssh_port,
            notes: notes.map(str::to_string),
            carriers,
            udp,
            auto_reconnect,
            local_host: local_host.to_string(),
            local_port,
        })
    }
}

impl TryFrom<&ClientMessage> for SshJumpRegistration {
    type Error = anyhow::Error;

    fn try_from(message: &ClientMessage) -> Result<Self> {
        let ClientMessage::HelloSshJump {
            alias,
            ssh_port,
            notes,
            carriers,
            udp,
            auto_reconnect,
            local_host,
            local_port,
        } = message
        else {
            bail!("client message is not an SSH jump registration");
        };
        Self::new(
            alias,
            *ssh_port,
            notes.as_deref(),
            *carriers,
            *udp,
            *auto_reconnect,
            local_host,
            *local_port,
        )
    }
}

/// One registered jump provider. Phase 1 only defines ownership and bounded
/// metadata; serving/opening behavior remains disabled until Phase 2.
pub struct SshJumpEntry {
    registration_id: u64,
    /// Warm TCP/SSH carrier pool used by the later gateway splice.
    pub pool: Arc<CarrierPool>,
    /// Validated registration metadata.
    pub registration: SshJumpRegistration,
    /// Real per-tunnel concurrent connection bound.
    pub permits: Arc<Semaphore>,
    /// Live direct QUIC connections for a native provider.
    #[cfg(feature = "udp")]
    pub direct: crate::vhost::DirectPool,
}

impl SshJumpEntry {
    /// Build an inert registry entry from already-validated metadata.
    pub fn new(
        pool: Arc<CarrierPool>,
        registration: SshJumpRegistration,
        permits: Arc<Semaphore>,
    ) -> Self {
        static NEXT_REGISTRATION_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            registration_id: NEXT_REGISTRATION_ID.fetch_add(1, Ordering::Relaxed),
            pool,
            registration,
            permits,
            #[cfg(feature = "udp")]
            direct: crate::vhost::DirectPool::default(),
        }
    }

    /// Process-local ownership token used to make RAII teardown replacement-safe.
    pub fn registration_id(&self) -> u64 {
        self.registration_id
    }
}

/// Registry of jump providers keyed by their single-label alias.
pub type SshJumpRegistry = Arc<DashMap<String, Arc<SshJumpEntry>>>;

/// One pending native direct-path nonce tied to its exact registration owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingSshJumpNonce {
    /// Nonce used with the bore secret to derive the direct authentication token.
    pub nonce: [u8; UDP_NONCE_LEN],
    owner_id: u64,
}

impl PendingSshJumpNonce {
    /// Bind a nonce to one exact registry entry.
    pub fn new(owner: &SshJumpEntry, nonce: [u8; UDP_NONCE_LEN]) -> Self {
        Self {
            nonce,
            owner_id: owner.registration_id(),
        }
    }
}

/// Pending direct-path nonce registry keyed by `jump:<alias>`.
pub type PendingSshJumpUdp = Arc<DashMap<String, PendingSshJumpNonce>>;

/// Return the collision-proof direct-path key for one validated alias.
pub fn direct_key(alias: &str) -> String {
    format!("jump:{alias}")
}

/// RAII deregistration guard that cannot remove a replacement entry or nonce.
#[allow(dead_code)]
pub(crate) struct SshJumpDeregister {
    registry: SshJumpRegistry,
    pending_udp: PendingSshJumpUdp,
    alias: String,
    entry: Arc<SshJumpEntry>,
}

#[allow(dead_code)]
impl SshJumpDeregister {
    /// Capture ownership of one installed entry and its current direct nonce.
    pub(crate) fn new(
        registry: SshJumpRegistry,
        pending_udp: PendingSshJumpUdp,
        alias: String,
        entry: Arc<SshJumpEntry>,
    ) -> Self {
        Self {
            registry,
            pending_udp,
            alias,
            entry,
        }
    }
}

impl Drop for SshJumpDeregister {
    fn drop(&mut self) {
        let removed = self
            .registry
            .remove_if(&self.alias, |_, current| Arc::ptr_eq(current, &self.entry))
            .is_some();
        if removed {
            let key = direct_key(&self.alias);
            let owner_id = self.entry.registration_id();
            self.pending_udp
                .remove_if(&key, |_, current| current.owner_id == owner_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Semaphore;

    use super::*;
    use crate::mux;
    use crate::pool::CarrierPool;

    fn registration(alias: &str, port: u16) -> SshJumpRegistration {
        SshJumpRegistration::new(
            alias,
            port,
            Some("vm test AWS su zona eu-south-1"),
            2,
            true,
            true,
            "localhost",
            port,
        )
        .unwrap()
    }

    #[test]
    fn ssh_jump_alias_contract() {
        assert_eq!(validate_alias("vm-test-01").unwrap(), "vm-test-01");
        for invalid in [
            "",
            "Vm-test-01",
            "vm.test",
            "-vm",
            "vm-",
            "vm_test",
            &"a".repeat(64),
        ] {
            assert!(validate_alias(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn ssh_jump_hostname_routing_contract() {
        assert_eq!(
            classify_destination("vm-test-01.ssh.bore.tld", 22, "ssh.bore.tld", 22,).unwrap(),
            JumpRoute::Match {
                alias: "vm-test-01".to_string(),
                port: 22,
            }
        );
        assert_eq!(
            classify_destination("VM-TEST-01.SSH.BORE.TLD.", 22, "ssh.bore.tld", 22,).unwrap(),
            JumpRoute::Match {
                alias: "vm-test-01".to_string(),
                port: 22,
            }
        );
        for not_jump in [
            "vm-test-01.bore.tld",
            "evilssh.bore.tld",
            "vm-test-01.ssh.bore.example",
        ] {
            assert_eq!(
                classify_destination(not_jump, 22, "ssh.bore.tld", 22).unwrap(),
                JumpRoute::NotJump,
            );
        }
        assert!(
            classify_destination("nested.vm-test-01.ssh.bore.tld", 22, "ssh.bore.tld", 22,)
                .is_err()
        );
        assert!(
            classify_destination("vm-test-01.ssh.bore.tld", 2222, "ssh.bore.tld", 22,).is_err()
        );
    }

    #[test]
    fn ssh_jump_registration_bounds_and_ports() {
        assert_eq!(registration("vm-test-01", 22).local_port, 22);
        assert_eq!(registration("vm-test-01", 2222).ssh_port, 2222);
        assert!(
            SshJumpRegistration::new("vm-test-01", 0, None, 1, false, false, "localhost", 0,)
                .is_err()
        );
        assert!(SshJumpRegistration::new(
            "vm-test-01",
            22,
            None,
            1,
            false,
            false,
            "localhost",
            2222,
        )
        .is_err());
        assert!(SshJumpRegistration::new(
            "vm-test-01",
            22,
            Some(&"n".repeat(crate::shared::MAX_NOTES_LEN + 1)),
            1,
            false,
            false,
            "localhost",
            22,
        )
        .is_err());
        assert!(SshJumpRegistration::new(
            "vm-test-01",
            22,
            None,
            1,
            false,
            false,
            &"h".repeat(MAX_LOCAL_HOST_LEN + 1),
            22,
        )
        .is_err());
    }

    #[tokio::test]
    async fn ssh_jump_stale_guard_preserves_new_registration_and_nonce() {
        let registry = SshJumpRegistry::default();
        let pending = PendingSshJumpUdp::default();

        let (old_io, _old_peer) = tokio::io::duplex(1024);
        let (old_opener, _old_acceptor) = mux::client(old_io);
        let old_entry = Arc::new(SshJumpEntry::new(
            Arc::new(CarrierPool::new(mux::LinkOpener::Mux(old_opener))),
            registration("vm-test-01", 22),
            Arc::new(Semaphore::new(4)),
        ));
        registry.insert("vm-test-01".to_string(), Arc::clone(&old_entry));
        pending.insert(
            "jump:vm-test-01".to_string(),
            PendingSshJumpNonce::new(&old_entry, [1; crate::shared::UDP_NONCE_LEN]),
        );
        let old_guard = SshJumpDeregister::new(
            registry.clone(),
            pending.clone(),
            "vm-test-01".to_string(),
            Arc::clone(&old_entry),
        );

        let (new_io, _new_peer) = tokio::io::duplex(1024);
        let (new_opener, _new_acceptor) = mux::client(new_io);
        let new_entry = Arc::new(SshJumpEntry::new(
            Arc::new(CarrierPool::new(mux::LinkOpener::Mux(new_opener))),
            registration("vm-test-01", 22),
            Arc::new(Semaphore::new(4)),
        ));
        registry.insert("vm-test-01".to_string(), Arc::clone(&new_entry));
        pending.insert(
            "jump:vm-test-01".to_string(),
            PendingSshJumpNonce::new(&new_entry, [2; crate::shared::UDP_NONCE_LEN]),
        );

        drop(old_guard);

        assert!(registry
            .get("vm-test-01")
            .is_some_and(|entry| Arc::ptr_eq(entry.value(), &new_entry)));
        assert_eq!(
            pending.get("jump:vm-test-01").map(|pending| pending.nonce),
            Some([2; crate::shared::UDP_NONCE_LEN]),
        );
    }

    #[tokio::test]
    async fn ssh_jump_current_guard_removes_its_registration_and_nonce() {
        let registry = SshJumpRegistry::default();
        let pending = PendingSshJumpUdp::default();
        let (io, _peer) = tokio::io::duplex(1024);
        let (opener, _acceptor) = mux::client(io);
        let entry = Arc::new(SshJumpEntry::new(
            Arc::new(CarrierPool::new(mux::LinkOpener::Mux(opener))),
            registration("vm-test-01", 22),
            Arc::new(Semaphore::new(4)),
        ));
        registry.insert("vm-test-01".to_string(), Arc::clone(&entry));
        pending.insert(
            "jump:vm-test-01".to_string(),
            PendingSshJumpNonce::new(&entry, [1; crate::shared::UDP_NONCE_LEN]),
        );
        let guard = SshJumpDeregister::new(
            registry.clone(),
            pending.clone(),
            "vm-test-01".to_string(),
            entry,
        );

        drop(guard);

        assert!(!registry.contains_key("vm-test-01"));
        assert!(!pending.contains_key("jump:vm-test-01"));
    }
}
