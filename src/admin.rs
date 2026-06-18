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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    /// Number of parallel TCP carrier connections the client requested
    /// (`--carriers`; `0`/`1` = single-connection default).
    pub carriers: u16,
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
            carriers: new.carriers,
            auto_reconnect: new.auto_reconnect,
            since: Instant::now(),
            udp: AtomicBool::new(new.udp),
            active: Arc::new(AtomicUsize::new(0)),
            overlay: std::sync::Mutex::new(None),
            vpn_direct: AtomicBool::new(false),
            relay_tx_bytes: Arc::new(AtomicU64::new(0)),
            relay_rx_bytes: Arc::new(AtomicU64::new(0)),
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
                    carriers: entry.carriers,
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
}
