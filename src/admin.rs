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
    /// An SSH jump-host provider (native bore or pure OpenSSH transport).
    SshJumpHost,
    /// A VPN link listener (`bore vpn listen`).
    VpnListener,
    /// A VPN link connector (`bore vpn connect`).
    VpnConnector,
}

/// Which client implementation established this tunnel.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// The native `bore` client/protocol.
    #[default]
    Bore,
    /// A stock OpenSSH client via the embedded SSH gateway (`--ssh-gateway`).
    Ssh,
}

/// [`Entry::secret_path`]: the consumer has not reported yet.
///
/// A REAL state for a secret tunnel, not a placeholder for a missing feature:
/// the direct path runs consumer↔provider, so between "the server brokered a
/// punch" and "the consumer said what happened" the server genuinely does not
/// know. P-10 established the rule this obeys — a tunnel with only ONE possible
/// path must never read "unknown", so a consumer that never asked for `--udp`
/// is derived as `relay` by the admin API instead of sitting here.
pub const SECRET_PATH_UNKNOWN: u8 = 0;
/// [`Entry::secret_path`]: the consumer reported the server relay.
pub const SECRET_PATH_RELAY: u8 = 1;
/// [`Entry::secret_path`]: the consumer reported the QUIC direct path.
pub const SECRET_PATH_DIRECT: u8 = 2;

/// Render [`Entry::secret_path`] for the admin API.
///
/// Deliberately a separate set from `vhost::VHOST_PATH_*` rather than a shared
/// one: those are wired into a shipped invariant (phase 05.3) and this change
/// has no business touching it. The two render the same three words.
pub fn secret_path_label(path: u8) -> &'static str {
    match path {
        SECRET_PATH_RELAY => "relay",
        SECRET_PATH_DIRECT => "direct",
        _ => "unknown",
    }
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
    /// Whether the client requested HTTP access logging (`--webserver-log`),
    /// sent over the wire via `TunnelOptions` (public) / `HelloVhost` (vhost).
    /// Informational; the server takes no action on it.
    pub webserver_log: bool,
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
    /// Secret consumer: which data path the consumer last REPORTED using
    /// ([`SECRET_PATH_UNKNOWN`] / [`SECRET_PATH_RELAY`] / [`SECRET_PATH_DIRECT`]).
    ///
    /// S-1. Unlike the vhost, public and ssh-jump registries, the server is not
    /// an endpoint of a secret tunnel's direct path — it runs
    /// consumer↔provider and never reaches the server — so this is set from
    /// [`crate::shared::ClientMessage::SecretPathReport`] and not from
    /// anything the server can observe. `UNKNOWN` is therefore a real state and
    /// not a bug: a `--udp` consumer that has been brokered a punch and has not
    /// reported yet genuinely has no answer.
    pub secret_path: std::sync::atomic::AtomicU8,
    /// Secret consumer: how many times it reported falling back to the relay
    /// after asking for a direct path. A count, not a flag, because a tunnel
    /// that flaps between the two is a different problem from one that never
    /// got direct at all.
    pub secret_direct_fallbacks: AtomicU64,
    /// Secret consumer: why the direct path was not used, as the consumer
    /// reported it. `None` while direct, or before any report.
    pub secret_path_reason: std::sync::Mutex<Option<String>>,
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
    /// Consumer (secret) display-only: local proxy listen port (`--local-proxy-port`).
    pub local_proxy_port: Option<u16>,
    /// Display-only: local service host the client forwards to (`-l/--local-host`).
    pub local_host: Option<String>,
    /// Display-only: local service port the client forwards to.
    pub local_port: Option<u16>,
    /// Display-only: client's `--nat-udp-preferred-port` for secret/public roles
    /// (None when 0/unset). VPN roles use [`Entry::vpn_nat_udp_port`].
    pub nat_udp_preferred_port: Option<u16>,
    /// Display-only: client's `--nat-udp-release-timeout` seconds (None when unset).
    pub nat_udp_release_timeout: Option<u64>,
    /// Display-only: client's selected `--stun-server`, if any.
    pub stun_server: Option<String>,
    /// Display-only: whether the client enabled `--upnp`.
    pub upnp: bool,
    /// Display-only: whether the client enabled `--try-port-prediction`.
    pub try_port_prediction: bool,
    /// Display-only: client's `--max-conns` cap (None when unset).
    pub max_conns: Option<usize>,
    /// Which client implementation established this tunnel.
    pub transport: Transport,
    /// Identity presented at authentication (SSH pubkey comment / fingerprint or
    /// password label). `None` for the native `bore` transport.
    pub identity: Option<String>,
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
    /// See [`Entry::webserver_log`].
    pub webserver_log: bool,
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
    /// See [`Entry::local_proxy_port`].
    pub local_proxy_port: Option<u16>,
    /// See [`Entry::local_host`].
    pub local_host: Option<String>,
    /// See [`Entry::local_port`].
    pub local_port: Option<u16>,
    /// See [`Entry::nat_udp_preferred_port`].
    pub nat_udp_preferred_port: Option<u16>,
    /// See [`Entry::nat_udp_release_timeout`].
    pub nat_udp_release_timeout: Option<u64>,
    /// See [`Entry::stun_server`].
    pub stun_server: Option<String>,
    /// See [`Entry::upnp`].
    pub upnp: bool,
    /// See [`Entry::try_port_prediction`].
    pub try_port_prediction: bool,
    /// See [`Entry::max_conns`].
    pub max_conns: Option<usize>,
    /// See [`Entry::transport`].
    pub transport: Transport,
    /// See [`Entry::identity`].
    pub identity: Option<String>,
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
    /// See [`Entry::webserver_log`].
    pub webserver_log: bool,
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
    /// See [`Entry::secret_path`]; already rendered by [`secret_path_label`].
    pub secret_path: &'static str,
    /// See [`Entry::secret_direct_fallbacks`].
    pub secret_direct_fallbacks: u64,
    /// See [`Entry::secret_path_reason`].
    pub secret_path_reason: Option<String>,
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
    /// See [`Entry::local_proxy_port`].
    pub local_proxy_port: Option<u16>,
    /// See [`Entry::local_host`].
    pub local_host: Option<String>,
    /// See [`Entry::local_port`].
    pub local_port: Option<u16>,
    /// See [`Entry::nat_udp_preferred_port`].
    pub nat_udp_preferred_port: Option<u16>,
    /// See [`Entry::nat_udp_release_timeout`].
    pub nat_udp_release_timeout: Option<u64>,
    /// See [`Entry::stun_server`].
    pub stun_server: Option<String>,
    /// See [`Entry::upnp`].
    pub upnp: bool,
    /// See [`Entry::try_port_prediction`].
    pub try_port_prediction: bool,
    /// See [`Entry::max_conns`].
    pub max_conns: Option<usize>,
    /// See [`Entry::transport`].
    pub transport: Transport,
    /// See [`Entry::identity`].
    pub identity: Option<String>,
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
        self.register_with_counters(
            new,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        )
    }

    /// Register a tunnel using caller-owned activity and relay-byte counters.
    ///
    /// SSH jump entries need the same atomics in both their routing registry and
    /// admin row: channels are opened from the former while the dashboard reads
    /// the latter. Keeping one shared set avoids lagging copies and preserves
    /// exact RAII accounting on every cancellation/error path.
    pub(crate) fn register_with_counters(
        &self,
        new: NewEntry,
        active: Arc<AtomicUsize>,
        relay_tx_bytes: Arc<AtomicU64>,
        relay_rx_bytes: Arc<AtomicU64>,
    ) -> Registration {
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
            webserver_log: new.webserver_log,
            since: Instant::now(),
            udp: AtomicBool::new(new.udp),
            active,
            overlay: std::sync::Mutex::new(None),
            vpn_direct: AtomicBool::new(false),
            secret_path: std::sync::atomic::AtomicU8::new(SECRET_PATH_UNKNOWN),
            secret_direct_fallbacks: AtomicU64::new(0),
            secret_path_reason: std::sync::Mutex::new(None),
            relay_tx_bytes,
            relay_rx_bytes,
            vpn_relay_only: new.vpn_relay_only,
            vpn_pin_mtu: new.vpn_pin_mtu,
            vpn_mtu: new.vpn_mtu,
            vpn_forward_accept: new.vpn_forward_accept,
            vpn_nat_masquerade: new.vpn_nat_masquerade,
            vpn_route_policy: new.vpn_route_policy,
            vpn_advertised: new.vpn_advertised,
            vpn_nat_udp_port: new.vpn_nat_udp_port,
            local_proxy_port: new.local_proxy_port,
            local_host: new.local_host,
            local_port: new.local_port,
            nat_udp_preferred_port: new.nat_udp_preferred_port,
            nat_udp_release_timeout: new.nat_udp_release_timeout,
            stun_server: new.stun_server,
            upnp: new.upnp,
            try_port_prediction: new.try_port_prediction,
            max_conns: new.max_conns,
            transport: new.transport,
            identity: new.identity,
        });
        self.inner.entries.insert(id, Arc::clone(&entry));
        tracing::info!(
            id,
            role = ?entry.role,
            peer = %entry.peer,
            secret_id = ?entry.secret_id,
            "admin entry registered"
        );
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
                    webserver_log: entry.webserver_log,
                    udp: entry.udp.load(Ordering::Relaxed),
                    uptime_secs: entry.since.elapsed().as_secs(),
                    active: entry.active.load(Ordering::Relaxed),
                    overlay: entry
                        .overlay
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .clone(),
                    vpn_direct: entry.vpn_direct.load(Ordering::Relaxed),
                    secret_path: secret_path_label(entry.secret_path.load(Ordering::Relaxed)),
                    secret_direct_fallbacks: entry.secret_direct_fallbacks.load(Ordering::Relaxed),
                    secret_path_reason: entry
                        .secret_path_reason
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .clone(),
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
                    local_proxy_port: entry.local_proxy_port,
                    local_host: entry.local_host.clone(),
                    local_port: entry.local_port,
                    nat_udp_preferred_port: entry.nat_udp_preferred_port,
                    nat_udp_release_timeout: entry.nat_udp_release_timeout,
                    stun_server: entry.stun_server.clone(),
                    upnp: entry.upnp,
                    try_port_prediction: entry.try_port_prediction,
                    max_conns: entry.max_conns,
                    transport: entry.transport,
                    identity: entry.identity.clone(),
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

    /// Record the data path a secret consumer reported (S-1).
    ///
    /// Counts a fallback only on the TRANSITION into relay, so a consumer that
    /// re-reports "relay" on every reconnect of a permanently un-punchable pair
    /// does not inflate the counter into meaninglessness. An unrecognised label
    /// is ignored rather than stored: the field is peer-controlled and the
    /// admin API renders it.
    pub fn set_secret_path(&self, path: &str, reason: Option<String>) {
        let code = match path {
            "direct" => SECRET_PATH_DIRECT,
            "relay" => SECRET_PATH_RELAY,
            _ => return,
        };
        let prev = self.entry.secret_path.swap(code, Ordering::Relaxed);
        if code == SECRET_PATH_RELAY && prev != SECRET_PATH_RELAY {
            self.entry
                .secret_direct_fallbacks
                .fetch_add(1, Ordering::Relaxed);
        }
        *self
            .entry
            .secret_path_reason
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = if code == SECRET_PATH_DIRECT {
            None
        } else {
            reason
        };
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
        tracing::info!(id = self.id, role = ?self.entry.role, "admin entry dropped");
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
            webserver_log: false,
            udp: false,
            vpn_relay_only: false,
            vpn_pin_mtu: false,
            vpn_mtu: None,
            vpn_forward_accept: false,
            vpn_nat_masquerade: false,
            vpn_route_policy: None,
            vpn_advertised: vec![],
            vpn_nat_udp_port: None,
            local_proxy_port: None,
            local_host: None,
            local_port: None,
            nat_udp_preferred_port: None,
            nat_udp_release_timeout: None,
            stun_server: None,
            upnp: false,
            try_port_prediction: false,
            max_conns: None,
            transport: Transport::Bore,
            identity: None,
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
    fn secret_path_report_is_recorded_and_rendered() {
        let reg = AdminRegistry::default();
        let handle = reg.register(sample(Role::SecretConsumer));
        // Before any report: genuinely unknown, and it says so.
        assert_eq!(reg.snapshot()[0].secret_path, "unknown");
        assert_eq!(reg.snapshot()[0].secret_direct_fallbacks, 0);

        handle.set_secret_path("direct", None);
        assert_eq!(reg.snapshot()[0].secret_path, "direct");
        assert_eq!(reg.snapshot()[0].secret_direct_fallbacks, 0);
        assert_eq!(reg.snapshot()[0].secret_path_reason, None);
    }

    #[test]
    fn a_fallback_is_counted_on_the_transition_and_not_on_every_report() {
        // The counter answers "did this tunnel LOSE the direct path?", so a
        // consumer that is permanently un-punchable and re-reports "relay" on
        // every reconnect must read 1, not one per reconnect — otherwise a
        // stable relay tunnel and a flapping one look the same, and flapping is
        // the one worth waking up for.
        let reg = AdminRegistry::default();
        let handle = reg.register(sample(Role::SecretConsumer));

        handle.set_secret_path("relay", Some("no udp-capable provider registered".into()));
        assert_eq!(reg.snapshot()[0].secret_direct_fallbacks, 1);
        handle.set_secret_path("relay", Some("no udp-capable provider registered".into()));
        handle.set_secret_path("relay", Some("no udp-capable provider registered".into()));
        assert_eq!(
            reg.snapshot()[0].secret_direct_fallbacks,
            1,
            "re-reporting the same relay state must not count a new fallback"
        );
        assert_eq!(
            reg.snapshot()[0].secret_path_reason.as_deref(),
            Some("no udp-capable provider registered")
        );

        // A real flap: direct, then relay again, is a SECOND fallback.
        handle.set_secret_path("direct", None);
        assert_eq!(
            reg.snapshot()[0].secret_path_reason,
            None,
            "a direct report must clear the stale reason"
        );
        handle.set_secret_path("relay", Some("checks failed".into()));
        assert_eq!(reg.snapshot()[0].secret_direct_fallbacks, 2);
        assert_eq!(
            reg.snapshot()[0].secret_path_reason.as_deref(),
            Some("checks failed")
        );
    }

    #[test]
    fn an_unknown_path_label_is_ignored_rather_than_stored() {
        // `path` is peer-controlled and the admin API renders it. Storing an
        // arbitrary string would put attacker-chosen text on the dashboard; the
        // label set is closed, so anything else is simply not a report.
        let reg = AdminRegistry::default();
        let handle = reg.register(sample(Role::SecretConsumer));
        handle.set_secret_path("direct", None);
        handle.set_secret_path("<img src=x onerror=alert(1)>", Some("nope".into()));
        assert_eq!(reg.snapshot()[0].secret_path, "direct");
        assert_eq!(reg.snapshot()[0].secret_path_reason, None);
        assert_eq!(reg.snapshot()[0].secret_direct_fallbacks, 0);
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
    fn webserver_log_in_snapshot() {
        // webserver_log (from TunnelOptions / HelloVhost) must survive into the
        // admin snapshot so the dashboard can show the access-logging flag.
        let reg = AdminRegistry::default();
        let mut new = sample(Role::Public);
        new.webserver_log = true;
        let _handle = reg.register(new);
        assert!(reg.snapshot()[0].webserver_log);
        // default stays false
        let reg2 = AdminRegistry::default();
        let _h2 = reg2.register(sample(Role::SecretProvider));
        assert!(!reg2.snapshot()[0].webserver_log);
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
