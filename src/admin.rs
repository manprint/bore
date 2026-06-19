//! In-memory registry of active tunnels for the admin status page.
//!
//! This is deliberately **stateless across restarts**: it reflects only the
//! currently-connected clients and is populated and cleared as connections come
//! and go. There is no persistence. A [`Registration`] is an RAII handle — when a
//! control connection's handler returns, the registration drops and the entry
//! disappears, so the admin page stays in sync automatically.
//!
//! The HTTP server that renders this state lives in [`crate::admin_http`].

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use serde::Serialize;

/// The role a registered control connection plays.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// A public-port tunnel (`bore local`).
    Public,
    /// A secret-tunnel provider (`bore local --tcp-secret-id`).
    SecretProvider,
    /// A secret-tunnel consumer (`bore proxy`).
    SecretConsumer,
    /// A vhost subdomain reverse-proxy provider (`bore vhost`).
    Vhost,
    /// A VPN link listener (`bore vpn listen`).
    VpnListener,
    /// A VPN link connector (`bore vpn connect`).
    VpnConnector,
}

/// A live tunnel registration. One per accepted control connection.
///
/// The immutable descriptive fields are set once at registration; the two
/// mutable runtime signals ([`Entry::active`] connection count and the
/// [`Entry::udp`] flag) are atomics so the connection handler can update them
/// without locking the registry.
pub struct Entry {
    /// What kind of tunnel this is.
    pub role: Role,
    /// Remote address of the client that opened the control connection.
    pub peer: SocketAddr,
    /// Secret-tunnel id, for the secret roles.
    pub secret_id: Option<String>,
    /// Allocated public port, for [`Role::Public`].
    pub public_port: Option<u16>,
    /// Free-form operator note supplied with `--notes`.
    pub notes: Option<String>,
    /// Whether HTTP Basic auth is enforced for this tunnel.
    pub basic_auth: bool,
    /// Whether the public tunnel terminates TLS (`--https`).
    pub https: bool,
    /// Whether the public tunnel redirects plain HTTP to https (`--force-https`).
    pub force_https: bool,
    /// Number of parallel carrier connections. For VPN roles this is updated to
    /// the *effective* negotiated count once pairing completes (atomic so the
    /// listener task can refresh it after a connector arrives).
    pub carriers: AtomicU16,
    /// Whether the client runs with `--auto-reconnect` (client-side reconnect
    /// loop; informational, sent over the wire via `TunnelOptions`).
    pub auto_reconnect: bool,
    /// When the connection registered (for an uptime readout).
    pub since: Instant,
    /// Provider: registered as UDP-capable. Consumer: requested a direct path.
    pub udp: AtomicBool,
    /// Live count of connections currently proxied through this tunnel, where the
    /// server can observe them (public tunnels and relayed consumers; a direct
    /// UDP consumer's data bypasses the server, so its count stays 0).
    pub active: Arc<AtomicUsize>,
    /// VPN roles: overlay address (`addr/prefix`), set at/after pairing.
    pub overlay: std::sync::Mutex<Option<String>>,
    /// VPN roles: whether the link reported the direct QUIC path as active.
    pub vpn_direct: AtomicBool,
    /// VPN roles: relay ciphertext bytes sent toward this client.
    pub relay_tx_bytes: Arc<AtomicU64>,
    /// VPN roles: relay ciphertext bytes received from this client.
    pub relay_rx_bytes: Arc<AtomicU64>,
    /// VPN display-only: relay-only mode enabled (no direct QUIC).
    pub vpn_relay_only: bool,
    /// VPN display-only: MTU pinning enabled.
    pub vpn_pin_mtu: bool,
    /// VPN display-only: TUN interface MTU.
    pub vpn_mtu: Option<u16>,
    /// VPN display-only: forward-accept iptables rule inserted.
    pub vpn_forward_accept: bool,
    /// VPN display-only: NAT masquerade enabled.
    pub vpn_nat_masquerade: bool,
    /// VPN display-only: route accept/refuse policy summary.
    pub vpn_route_policy: Option<String>,
    /// VPN display-only: CIDRs this side advertises (exposed/virtual), as strings.
    pub vpn_advertised: Vec<String>,
    /// VPN display-only: client's `--nat-udp-preferred-port` (None when 0/unset).
    pub vpn_nat_udp_port: Option<u16>,
}

/// Descriptive fields used to create an [`Entry`]; the atomics are initialized by
/// [`AdminRegistry::register`].
pub struct NewEntry {
    /// See [`Entry::role`].
    pub role: Role,
    /// See [`Entry::peer`].
    pub peer: SocketAddr,
    /// See [`Entry::secret_id`].
    pub secret_id: Option<String>,
    /// See [`Entry::public_port`].
    pub public_port: Option<u16>,
    /// See [`Entry::notes`].
    pub notes: Option<String>,
    /// See [`Entry::basic_auth`].
    pub basic_auth: bool,
    /// See [`Entry::https`].
    pub https: bool,
    /// See [`Entry::force_https`].
    pub force_https: bool,
    /// See [`Entry::carriers`].
    pub carriers: u16,
    /// See [`Entry::auto_reconnect`].
    pub auto_reconnect: bool,
    /// Initial value of [`Entry::udp`].
    pub udp: bool,
    /// See [`Entry::vpn_relay_only`].
    pub vpn_relay_only: bool,
    /// See [`Entry::vpn_pin_mtu`].
    pub vpn_pin_mtu: bool,
    /// See [`Entry::vpn_mtu`].
    pub vpn_mtu: Option<u16>,
    /// See [`Entry::vpn_forward_accept`].
    pub vpn_forward_accept: bool,
    /// See [`Entry::vpn_nat_masquerade`].
    pub vpn_nat_masquerade: bool,
    /// See [`Entry::vpn_route_policy`].
    pub vpn_route_policy: Option<String>,
    /// See [`Entry::vpn_advertised`].
    pub vpn_advertised: Vec<String>,
    /// See [`Entry::vpn_nat_udp_port`].
    pub vpn_nat_udp_port: Option<u16>,
}

/// A serializable snapshot of one [`Entry`], produced by [`AdminRegistry::snapshot`].
#[derive(Serialize)]
pub struct EntryView {
    /// Stable per-connection id (useful as a table key on the client).
    pub id: u64,
    /// See [`Entry::role`].
    pub role: Role,
    /// Remote client address, rendered as a string.
    pub peer: String,
    /// See [`Entry::secret_id`].
    pub secret_id: Option<String>,
    /// See [`Entry::public_port`].
    pub public_port: Option<u16>,
    /// See [`Entry::notes`].
    pub notes: Option<String>,
    /// See [`Entry::basic_auth`].
    pub basic_auth: bool,
    /// See [`Entry::https`].
    pub https: bool,
    /// See [`Entry::force_https`].
    pub force_https: bool,
    /// See [`Entry::carriers`].
    pub carriers: u16,
    /// See [`Entry::auto_reconnect`].
    pub auto_reconnect: bool,
    /// See [`Entry::udp`].
    pub udp: bool,
    /// Seconds since the connection registered.
    pub uptime_secs: u64,
    /// See [`Entry::active`].
    pub active: usize,
    /// See [`Entry::overlay`].
    pub overlay: Option<String>,
    /// See [`Entry::vpn_direct`].
    pub vpn_direct: bool,
    /// See [`Entry::relay_tx_bytes`].
    pub relay_tx_bytes: u64,
    /// See [`Entry::relay_rx_bytes`].
    pub relay_rx_bytes: u64,
    /// See [`Entry::vpn_relay_only`].
    pub vpn_relay_only: bool,
    /// See [`Entry::vpn_pin_mtu`].
    pub vpn_pin_mtu: bool,
    /// See [`Entry::vpn_mtu`].
    pub vpn_mtu: Option<u16>,
    /// See [`Entry::vpn_forward_accept`].
    pub vpn_forward_accept: bool,
    /// See [`Entry::vpn_nat_masquerade`].
    pub vpn_nat_masquerade: bool,
    /// See [`Entry::vpn_route_policy`].
    pub vpn_route_policy: Option<String>,
    /// See [`Entry::vpn_advertised`].
    pub vpn_advertised: Vec<String>,
    /// See [`Entry::vpn_nat_udp_port`].
    pub vpn_nat_udp_port: Option<u16>,
}

/// Shared, cloneable handle to the live tunnel registry.
#[derive(Clone)]
pub struct AdminRegistry {
    inner: Arc<Inner>,
}

struct Inner {
    entries: DashMap<u64, Arc<Entry>>,
    next_id: AtomicU64,
}

impl Default for AdminRegistry {
    fn default() -> Self {
        AdminRegistry {
            inner: Arc::new(Inner {
                entries: DashMap::new(),
                next_id: AtomicU64::new(1),
            }),
        }
    }
}

impl AdminRegistry {
    /// Register a new live tunnel and return an RAII [`Registration`]. The entry is
    /// removed automatically when the registration is dropped.
    pub fn register(&self, new: NewEntry) -> Registration {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let entry = Arc::new(Entry {
            role: new.role,
            peer: new.peer,
            secret_id: new.secret_id,
            public_port: new.public_port,
            notes: new.notes,
            basic_auth: new.basic_auth,
            https: new.https,
            force_https: new.force_https,
            carriers: AtomicU16::new(new.carriers),
            auto_reconnect: new.auto_reconnect,
            since: Instant::now(),
            udp: AtomicBool::new(new.udp),
            active: Arc::new(AtomicUsize::new(0)),
            overlay: std::sync::Mutex::new(None),
            vpn_direct: AtomicBool::new(false),
            relay_tx_bytes: Arc::new(AtomicU64::new(0)),
            relay_rx_bytes: Arc::new(AtomicU64::new(0)),
            vpn_relay_only: new.vpn_relay_only,
            vpn_pin_mtu: new.vpn_pin_mtu,
            vpn_mtu: new.vpn_mtu,
            vpn_forward_accept: new.vpn_forward_accept,
            vpn_nat_masquerade: new.vpn_nat_masquerade,
            vpn_route_policy: new.vpn_route_policy,
            vpn_advertised: new.vpn_advertised,
            vpn_nat_udp_port: new.vpn_nat_udp_port,
        });
        self.inner.entries.insert(id, Arc::clone(&entry));
        Registration {
            inner: Arc::clone(&self.inner),
            id,
            entry,
        }
    }

    /// Snapshot all live entries into a serializable vector, ordered by id so the
    /// admin table is stable between refreshes.
    pub fn snapshot(&self) -> Vec<EntryView> {
        let mut views: Vec<EntryView> = self
            .inner
            .entries
            .iter()
            .map(|e| {
                let (id, entry) = (*e.key(), e.value());
                EntryView {
                    id,
                    role: entry.role,
                    peer: entry.peer.to_string(),
                    secret_id: entry.secret_id.clone(),
                    public_port: entry.public_port,
                    notes: entry.notes.clone(),
                    basic_auth: entry.basic_auth,
                    https: entry.https,
                    force_https: entry.force_https,
                    carriers: entry.carriers.load(Ordering::Relaxed),
                    auto_reconnect: entry.auto_reconnect,
                    udp: entry.udp.load(Ordering::Relaxed),
                    uptime_secs: entry.since.elapsed().as_secs(),
                    active: entry.active.load(Ordering::Relaxed),
                    overlay: entry
                        .overlay
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .clone(),
                    vpn_direct: entry.vpn_direct.load(Ordering::Relaxed),
                    relay_tx_bytes: entry.relay_tx_bytes.load(Ordering::Relaxed),
                    relay_rx_bytes: entry.relay_rx_bytes.load(Ordering::Relaxed),
                    vpn_relay_only: entry.vpn_relay_only,
                    vpn_pin_mtu: entry.vpn_pin_mtu,
                    vpn_mtu: entry.vpn_mtu,
                    vpn_forward_accept: entry.vpn_forward_accept,
                    vpn_nat_masquerade: entry.vpn_nat_masquerade,
                    vpn_route_policy: entry.vpn_route_policy.clone(),
                    vpn_advertised: entry.vpn_advertised.clone(),
                    vpn_nat_udp_port: entry.vpn_nat_udp_port,
                }
            })
            .collect();
        views.sort_by_key(|v| v.id);
        views
    }

    /// Number of live entries (used by tests and the server summary).
    pub fn len(&self) -> usize {
        self.inner.entries.len()
    }

    /// Whether the registry currently holds no entries.
    pub fn is_empty(&self) -> bool {
        self.inner.entries.is_empty()
    }
}

/// RAII handle for a registered tunnel: removes the entry from the registry when
/// dropped, and exposes the entry's mutable runtime signals.
pub struct Registration {
    inner: Arc<Inner>,
    id: u64,
    entry: Arc<Entry>,
}

impl Registration {
    /// A shared handle to this tunnel's live connection counter. Increment it when
    /// a connection starts being proxied and decrement it when it ends (an
    /// [`ActiveGuard`] does both).
    pub fn active(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.entry.active)
    }

    /// Mark this tunnel as UDP-capable / direct-requested after registration (e.g.
    /// when a consumer later offers UDP candidates).
    pub fn mark_udp(&self) {
        self.entry.udp.store(true, Ordering::Relaxed);
    }

    /// Set the VPN overlay address (`addr/prefix`) once pairing assigns it.
    pub fn set_overlay(&self, overlay: String) {
        *self.entry.overlay.lock().unwrap_or_else(|p| p.into_inner()) = Some(overlay);
    }

    /// Record the VPN data-plane path reported by the client.
    pub fn set_vpn_direct(&self, direct: bool) {
        self.entry.vpn_direct.store(direct, Ordering::Relaxed);
    }

    /// Update the carrier count once the effective (negotiated) value is known.
    /// VPN listeners register before a connector arrives, so the count is
    /// refreshed here after pairing.
    pub fn set_carriers(&self, carriers: u16) {
        self.entry.carriers.store(carriers, Ordering::Relaxed);
    }

    /// Shared handles to the relay byte counters `(tx_toward_client, rx_from_client)`.
    pub fn relay_bytes(&self) -> (Arc<AtomicU64>, Arc<AtomicU64>) {
        (
            Arc::clone(&self.entry.relay_tx_bytes),
            Arc::clone(&self.entry.relay_rx_bytes),
        )
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.inner.entries.remove(&self.id);
    }
}

/// RAII counter guard: increments a tunnel's active-connection count on creation
/// and decrements it on drop, so the count is correct even if the connection task
/// panics or is cancelled.
pub struct ActiveGuard(Arc<AtomicUsize>);

impl ActiveGuard {
    /// Increment `counter` and return a guard that decrements it when dropped.
    pub fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        ActiveGuard(counter)
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(role: Role) -> NewEntry {
        NewEntry {
            role,
            peer: "127.0.0.1:1234".parse().unwrap(),
            secret_id: None,
            public_port: Some(4000),
            notes: Some("note".into()),
            basic_auth: false,
            https: false,
            force_https: false,
            carriers: 0,
            auto_reconnect: false,
            udp: false,
            vpn_relay_only: false,
            vpn_pin_mtu: false,
            vpn_mtu: None,
            vpn_forward_accept: false,
            vpn_nat_masquerade: false,
            vpn_route_policy: None,
            vpn_advertised: vec![],
            vpn_nat_udp_port: None,
        }
    }

    #[test]
    fn register_and_drop_updates_registry() {
        let reg = AdminRegistry::default();
        assert!(reg.is_empty());
        let handle = reg.register(sample(Role::Public));
        assert_eq!(reg.len(), 1);
        let view = &reg.snapshot()[0];
        assert_eq!(view.role, Role::Public);
        assert_eq!(view.public_port, Some(4000));
        assert_eq!(view.active, 0);
        drop(handle);
        assert!(
            reg.is_empty(),
            "dropping the registration must remove the entry"
        );
    }

    #[test]
    fn active_guard_counts() {
        let reg = AdminRegistry::default();
        let handle = reg.register(sample(Role::SecretConsumer));
        let counter = handle.active();
        {
            let _g1 = ActiveGuard::new(Arc::clone(&counter));
            let _g2 = ActiveGuard::new(Arc::clone(&counter));
            assert_eq!(reg.snapshot()[0].active, 2);
        }
        assert_eq!(reg.snapshot()[0].active, 0, "guards must decrement on drop");
    }

    #[test]
    fn mark_udp_flips_flag() {
        let reg = AdminRegistry::default();
        let handle = reg.register(sample(Role::SecretProvider));
        assert!(!reg.snapshot()[0].udp);
        handle.mark_udp();
        assert!(reg.snapshot()[0].udp);
    }

    #[test]
    fn relay_counters_snapshot() {
        // BUG-1: the per-entry relay byte counters were never incremented. This
        // proves the read path (snapshot) reflects writes through `relay_bytes()`.
        let reg = AdminRegistry::default();
        let handle = reg.register(sample(Role::Public));
        assert_eq!(reg.snapshot()[0].relay_tx_bytes, 0);
        assert_eq!(reg.snapshot()[0].relay_rx_bytes, 0);
        let (tx, rx) = handle.relay_bytes();
        tx.fetch_add(4096, Ordering::Relaxed);
        rx.fetch_add(2048, Ordering::Relaxed);
        let view = &reg.snapshot()[0];
        assert_eq!(view.relay_tx_bytes, 4096);
        assert_eq!(view.relay_rx_bytes, 2048);
    }

    #[test]
    fn carriers_and_auto_reconnect_in_snapshot() {
        // BUG-3: carriers + auto_reconnect must survive into the admin snapshot.
        let reg = AdminRegistry::default();
        let mut new = sample(Role::Public);
        new.carriers = 4;
        new.auto_reconnect = true;
        let _handle = reg.register(new);
        let view = &reg.snapshot()[0];
        assert_eq!(view.carriers, 4);
        assert!(view.auto_reconnect);
    }

    #[test]
    fn set_carriers_updates_snapshot() {
        // VPN listeners register before the connector arrives, then refresh the
        // count to the effective negotiated value via `set_carriers`.
        let reg = AdminRegistry::default();
        let mut new = sample(Role::VpnListener);
        new.carriers = 1;
        let handle = reg.register(new);
        assert_eq!(reg.snapshot()[0].carriers, 1);
        handle.set_carriers(4);
        assert_eq!(reg.snapshot()[0].carriers, 4);
    }

    #[test]
    fn vpn_display_fields_in_snapshot() {
        // notes / advertised / nat-udp-port must survive into the admin snapshot
        // (the VPN panel reads them from here, not the ephemeral provider registry).
        let reg = AdminRegistry::default();
        let mut new = sample(Role::VpnConnector);
        new.notes = Some("site-a".into());
        new.vpn_advertised = vec!["10.10.0.0/24".into()];
        new.vpn_nat_udp_port = Some(443);
        let _handle = reg.register(new);
        let view = &reg.snapshot()[0];
        assert_eq!(view.notes.as_deref(), Some("site-a"));
        assert_eq!(view.vpn_advertised, vec!["10.10.0.0/24".to_string()]);
        assert_eq!(view.vpn_nat_udp_port, Some(443));
    }
}
