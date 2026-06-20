//! Server-side VPN registry, pairing, pool allocation, and relay brokering.
//! Feature-gated: only compiled when `--features vpn` is active.

#![cfg(feature = "vpn")]

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use dashmap::{mapref::entry::Entry, DashMap};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};

use crate::admin::{AdminRegistry, NewEntry, Role};
use crate::mux;
use crate::secret;
use crate::shared::{
    proxy_buffer_size, ClientMessage, CountingStream, Delimited, Ipv4Net, ServerMessage,
    UdpDirectTuning, VpnAddrRequest, UDP_NONCE_LEN,
};

/// VPN provider registry, keyed by VPN link ID.
pub type VpnRegistry = Arc<DashMap<String, VpnProviderEntry>>;

/// Shared handle to the server's /30 overlay pool.
///
/// A **std** mutex, not a tokio one: every critical section is a few map
/// operations with no await inside, and `VpnLeaseGuard::drop` (a sync context)
/// must be able to take the lock unconditionally — the old `try_lock` silently
/// leaked the /30 block whenever the lock happened to be contended.
pub type VpnPoolHandle = Arc<std::sync::Mutex<VpnPool>>;

/// Shared handle to a hub's [`HubState`] and its peer-event channel. Cloned into
/// every connector handler so they can allocate peers and notify the hub
/// listener. Present only when `max_clients > 1`.
#[derive(Clone)]
pub struct HubShared {
    /// Hub address/peer table (std mutex; no awaits held across the lock).
    pub state: Arc<std::sync::Mutex<HubState>>,
    /// Channel to the hub listener handler: connector handlers push
    /// join/leave/punch events here, which become `ServerMessage`s to the hub.
    pub event_tx: mpsc::Sender<HubPeerEvent>,
    /// Max concurrent spokes (`HelloVpn.max_clients`).
    pub max_clients: u16,
}

/// Hub address/peer tracking (protected by std::sync::Mutex, no awaits inside).
pub struct HubState {
    /// The hub's overlay subnet (e.g. `10.99.0.0/24`).
    pub subnet: Ipv4Net,
    /// The hub's own overlay address (the subnet's `.1`).
    pub hub_overlay: Ipv4Addr,
    /// CIDRs the hub advertises to spokes (delivered in each connector's
    /// `VpnReady.peer_advertised` and `VpnPeerJoin`).
    pub advertised: Vec<Ipv4Net>,
    /// Monotonic peer-id allocator (starts at 1, never reused within a session).
    next_peer_id: u32,
    /// Live peers keyed by `peer_id`.
    pub peers: HashMap<u32, PeerSlot>,
}

/// One spoke's allocation within a hub: its overlay address, server-assigned
/// `peer_id`, per-peer AEAD/direct-path nonce, and Phase-4 UDP rendezvous slot.
#[derive(Clone)]
pub struct PeerSlot {
    /// Server-assigned id, monotonic within the hub session.
    pub peer_id: u32,
    /// This spoke's overlay address (a host in the hub subnet).
    pub overlay: Ipv4Addr,
    /// Per-peer session nonce seeding key derivation + direct-path token.
    pub nonce: [u8; UDP_NONCE_LEN],
    /// Hub's UDP candidates for this peer (filled by the hub on offer; read by
    /// the connector's broker in Phase 4).
    pub hub_candidates: Vec<std::net::SocketAddr>,
    /// Hub's selected STUN server for this peer (advisory, Phase 4).
    pub hub_selected_stun: Option<String>,
}

/// Event a connector handler sends to the hub listener handler.
pub enum HubPeerEvent {
    /// A new spoke paired: the hub emits `VpnPeerJoin` to its client.
    Join {
        /// Server-assigned peer id.
        peer_id: u32,
        /// The spoke's overlay address.
        overlay: Ipv4Addr,
        /// The hub's advertised CIDRs (echoed for symmetry; spokes don't advertise in v1).
        advertised: Vec<Ipv4Net>,
        /// Per-peer session nonce.
        nonce: [u8; UDP_NONCE_LEN],
        /// Negotiated relay carrier count for this peer.
        carriers: u16,
    },
    /// A spoke disconnected: the hub emits `VpnPeerLeave` to its client.
    Leave {
        /// Peer id that left.
        peer_id: u32,
    },
    /// Phase 4: the spoke offered UDP candidates; the hub forwards a per-peer
    /// `UdpPunch`.
    Punch {
        /// Peer id the punch is for.
        peer_id: u32,
        /// The spoke's candidate offer.
        offer: secret::UdpOffer,
    },
}

impl HubState {
    /// Create hub state for `subnet`; the hub takes the subnet's `.1`.
    pub fn new(subnet: Ipv4Net, advertised: Vec<Ipv4Net>) -> Self {
        let hub_overlay = Ipv4Addr::from(u32::from(subnet.network()) + 1);
        Self {
            subnet,
            hub_overlay,
            advertised,
            next_peer_id: 1,
            peers: HashMap::new(),
        }
    }

    /// Allocate the lowest free host (skipping `.0`/`.1`/broadcast) and a fresh
    /// monotonic `peer_id`. Errors if the subnet has no free host.
    pub fn alloc_peer(&mut self) -> Result<PeerSlot> {
        let base = u32::from(self.subnet.network());
        let host_bits = 32 - self.subnet.prefix;
        let subnet_size = 1u32 << host_bits;
        let first_host = base + 2;
        let last_host = base + subnet_size - 2;

        for addr_u32 in first_host..=last_host {
            if !self
                .peers
                .values()
                .any(|p| u32::from(p.overlay) == addr_u32)
            {
                let overlay = Ipv4Addr::from(addr_u32);
                let peer_id = self.next_peer_id;
                self.next_peer_id += 1;
                let nonce = new_nonce();
                let slot = PeerSlot {
                    peer_id,
                    overlay,
                    nonce,
                    hub_candidates: vec![],
                    hub_selected_stun: None,
                };
                self.peers.insert(peer_id, slot.clone());
                return Ok(slot);
            }
        }
        bail!("hub subnet full")
    }

    /// Release the peer holding `overlay` (idempotent).
    pub fn free_peer(&mut self, overlay: Ipv4Addr) {
        self.peers.retain(|_, slot| slot.overlay != overlay);
    }

    /// Number of live spokes.
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Store the hub's UDP candidates for `peer_id` (Phase 4 rendezvous).
    pub fn set_hub_candidates(
        &mut self,
        peer_id: u32,
        cands: Vec<std::net::SocketAddr>,
        stun: Option<String>,
    ) {
        if let Some(slot) = self.peers.get_mut(&peer_id) {
            slot.hub_candidates = cands;
            slot.hub_selected_stun = stun;
        }
    }
}

/// What the VPN listener registers while waiting for a connector.
pub struct VpnProviderEntry {
    /// Networks this side advertises.
    pub advertised: Vec<Ipv4Net>,
    /// How this side wants its overlay address assigned.
    pub addr: VpnAddrRequest,
    /// The listener's mux opener (for relay substream).
    pub opener: mux::Opener,
    /// 1:1 pairing oneshot (Some in 1:1 mode, None in hub mode).
    pub pair_tx: Option<oneshot::Sender<VpnPairMsg>>,
    /// Hub shared state (Some only when max_clients > 1).
    pub hub: Option<HubShared>,
    /// Relay carrier pairs requested by the listener (`HelloVpn.carriers`).
    pub carriers: u16,
    /// Monotonic registration generation. Protects against a stale handler's
    /// deregistration removing a newer registration with the same id (a
    /// reconnect race: the old handler's Drop can run after the new listener
    /// already re-registered).
    pub session: u64,
}

/// Global generation counter for [`VpnProviderEntry::session`].
static VPN_SESSION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Allocate the next VPN registration generation token.
pub fn next_vpn_session() -> u64 {
    VPN_SESSION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Message the connector sends to the listener to complete pairing.
pub struct VpnPairMsg {
    /// VpnReady for the listener side.
    pub listener_ready: ServerMessage,
    /// Shared nonce (same for both sides; listener re-uses it for UDP punch).
    pub nonce: [u8; UDP_NONCE_LEN],
}

/// Pure /30 and hub subnet allocator, no I/O.
pub struct VpnPool {
    parent: Ipv4Net,
    allocated: HashSet<u32>, // network address of each /30 block (as u32)
    hub_blocks: HashMap<u32, u8>, // network addr -> prefix for each hub block
}

impl VpnPool {
    /// Create a new VPN pool from a parent CIDR.
    pub fn new(parent: Ipv4Net) -> Result<Self> {
        anyhow::ensure!(
            parent.prefix <= 30,
            "vpn pool CIDR prefix must be ≤30, got /{}",
            parent.prefix
        );
        Ok(Self {
            parent,
            allocated: HashSet::new(),
            hub_blocks: HashMap::new(),
        })
    }

    /// Allocate next free /30. Returns (listener_addr .1, connector_addr .2).
    pub fn alloc(&mut self) -> Result<(Ipv4Addr, Ipv4Addr)> {
        let base = u32::from(self.parent.network());
        let host_bits = 32 - self.parent.prefix;
        let total_blocks = 1u32 << host_bits.saturating_sub(2); // each /30 = 4 addrs
        for i in 0..total_blocks {
            let net_addr = base + i * 4;
            if !self.allocated.contains(&net_addr) && !self.in_any_hub_block(net_addr, 4) {
                self.allocated.insert(net_addr);
                return Ok((
                    Ipv4Addr::from(net_addr + 1), // .1 = listener
                    Ipv4Addr::from(net_addr + 2), // .2 = connector
                ));
            }
        }
        bail!(
            "vpn pool exhausted (all /30 blocks in {} in use)",
            self.parent
        )
    }

    /// Free a /30 block. `net_addr` is the block's network address (host bits = 0).
    pub fn free(&mut self, net_addr: u32) {
        self.allocated.remove(&net_addr);
    }

    /// Check whether an address collides with any live allocation (a /30 block
    /// or a reserved hub subnet).
    pub fn is_allocated(&self, addr: Ipv4Addr) -> bool {
        let net_addr = u32::from(addr) & 0xFFFF_FFFC;
        self.allocated.contains(&net_addr) || self.in_any_hub_block(u32::from(addr), 1)
    }

    /// Allocate a hub subnet of the given prefix. Returns the Ipv4Net.
    pub fn alloc_hub_subnet(&mut self, prefix: u8) -> Result<Ipv4Net> {
        anyhow::ensure!(
            self.parent.prefix <= prefix && prefix <= 30,
            "vpn hub prefix must be between {} and 30, got {}",
            self.parent.prefix,
            prefix
        );
        let base = u32::from(self.parent.network());
        let host_bits = 32 - prefix;
        let block_size = 1u32 << host_bits;
        let parent_host_bits = 32 - self.parent.prefix;
        let parent_size = 1u32 << parent_host_bits;

        for offset in (0..parent_size).step_by(block_size as usize) {
            let block_net = base + offset;
            let block_end = block_net + block_size; // exclusive upper bound

            // Skip blocks overlapping an existing hub block.
            if self.in_any_hub_block(block_net, block_size) {
                continue;
            }
            // Skip blocks overlapping any allocated /30 (each /30 spans 4 addrs).
            // Iterate the live allocation set (small) — never the whole address
            // space.
            let collides_slash30 = self
                .allocated
                .iter()
                .any(|&s| s < block_end && s + 4 > block_net);
            if collides_slash30 {
                continue;
            }

            self.hub_blocks.insert(block_net, prefix);
            return Ok(Ipv4Net {
                addr: Ipv4Addr::from(block_net),
                prefix,
            });
        }
        bail!("vpn hub pool exhausted")
    }

    /// Free a hub subnet. `net_addr` is the subnet's network address.
    pub fn free_hub_subnet(&mut self, net_addr: u32) {
        self.hub_blocks.remove(&net_addr);
    }

    fn in_any_hub_block(&self, net_addr: u32, span: u32) -> bool {
        for (&hub_base, &hub_prefix) in &self.hub_blocks {
            let hub_bits = 32 - hub_prefix;
            let hub_size = 1u32 << hub_bits;
            let hub_end = hub_base + hub_size;
            let span_end = net_addr + span;
            if !(span_end <= hub_base || net_addr >= hub_end) {
                return true;
            }
        }
        false
    }
}

/// RAII guard that frees a hub subnet lease on drop.
pub struct VpnHubLeaseGuard {
    pool: Option<VpnPoolHandle>,
    net_addr: u32,
}

impl VpnHubLeaseGuard {
    /// Arm a guard that frees hub subnet `net_addr` from `pool` on drop.
    pub fn new(pool: VpnPoolHandle, net_addr: u32) -> Self {
        Self {
            pool: Some(pool),
            net_addr,
        }
    }

    /// Disarm: the lease is handed off elsewhere and must not be freed on drop.
    #[allow(dead_code)]
    pub fn disarm(&mut self) {
        self.pool = None;
    }
}

impl Drop for VpnHubLeaseGuard {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            let mut p = pool.lock().unwrap_or_else(|poison| poison.into_inner());
            p.free_hub_subnet(self.net_addr);
        }
    }
}

/// RAII guard for a hub spoke: on drop it releases the peer's overlay address
/// back to the [`HubState`] and best-effort notifies the hub listener with a
/// `Leave` event. Armed right after `alloc_peer`, so the address is freed on
/// EVERY return path of the connector handler (including early `?` failures
/// before the relay/broker loop starts) — no address leak.
struct HubPeerGuard {
    hub: HubShared,
    peer_id: u32,
    overlay: Ipv4Addr,
}

impl Drop for HubPeerGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.hub.state.lock() {
            state.free_peer(self.overlay);
        }
        // Best-effort (sync Drop context): the hub listener also frees the whole
        // subnet when it exits, and Phase 3 tears peers down on relay death.
        let _ = self.hub.event_tx.try_send(HubPeerEvent::Leave {
            peer_id: self.peer_id,
        });
    }
}

/// RAII guard that frees a /30 pool lease on drop.
pub struct VpnLeaseGuard {
    pool: Option<VpnPoolHandle>,
    net_addr: u32, // network address of the /30 block
}

impl VpnLeaseGuard {
    /// Create a new lease guard for a VPN pool block.
    pub fn new(pool: VpnPoolHandle, net_addr: u32) -> Self {
        Self {
            pool: Some(pool),
            net_addr,
        }
    }

    /// Disarm: the lease is handed off elsewhere and should not be freed.
    pub fn disarm(&mut self) {
        self.pool = None;
    }
}

impl Drop for VpnLeaseGuard {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            // Blocking lock: critical sections never hold the lock across an
            // await, so this waits microseconds at most and the block is always
            // freed (the old try_lock leaked it under contention).
            let mut p = pool.lock().unwrap_or_else(|poison| poison.into_inner());
            p.free(self.net_addr);
        }
    }
}

/// Generate a fresh random session nonce from the system CSPRNG.
pub fn new_nonce() -> [u8; UDP_NONCE_LEN] {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut nonce = [0u8; UDP_NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .expect("system CSPRNG must not fail");
    nonce
}

/// Check that no two subnets in `nets` overlap (including the overlay /30).
/// Returns Some("overlapping subnets: X, Y") if overlap found.
pub fn check_overlap(
    listener_advertised: &[Ipv4Net],
    connector_advertised: &[Ipv4Net],
    overlay: Ipv4Net,
) -> Option<String> {
    let all: Vec<&Ipv4Net> = std::iter::once(&overlay)
        .chain(listener_advertised.iter())
        .chain(connector_advertised.iter())
        .collect();
    for i in 0..all.len() {
        for j in (i + 1)..all.len() {
            if all[i].overlaps(all[j]) {
                return Some(format!("overlapping subnets: {} and {}", all[i], all[j]));
            }
        }
    }
    None
}

/// Validate static addressing pair (mirror-consistency rules from §5).
pub fn validate_static(
    listener_addr: Ipv4Addr,
    listener_prefix: u8,
    listener_peer: Ipv4Addr,
    connector_addr: Ipv4Addr,
    connector_prefix: u8,
    connector_peer: Ipv4Addr,
) -> Result<()> {
    // Both addrs must match each other's peer field
    anyhow::ensure!(
        listener_addr == connector_peer,
        "static mirror mismatch: listener addr {} != connector peer {}",
        listener_addr,
        connector_peer,
    );
    anyhow::ensure!(
        connector_addr == listener_peer,
        "static mirror mismatch: connector addr {} != listener peer {}",
        connector_addr,
        listener_peer,
    );
    // Both prefixes must be equal
    anyhow::ensure!(
        listener_prefix == connector_prefix,
        "static prefix mismatch: listener /{} != connector /{}",
        listener_prefix,
        connector_prefix,
    );
    // Both addrs must be distinct
    anyhow::ensure!(
        listener_addr != connector_addr,
        "static addrs must be distinct: both are {}",
        listener_addr,
    );
    // Both addrs must be in the same network
    let overlay = Ipv4Net {
        addr: listener_addr,
        prefix: listener_prefix,
    };
    anyhow::ensure!(
        overlay.contains(connector_addr),
        "static connector addr {} is not in the listener's network {}",
        connector_addr,
        overlay,
    );
    Ok(())
}

/// Removes a VPN provider/UDP registration when the provider connection ends.
///
/// Both removals are generation-guarded (D5): if the same id was re-registered
/// by a newer session (listener reconnect) before this handler's Drop ran, the
/// newer entries must survive. The provider registry is matched on `session`;
/// the UDP registry is matched on the pairing `nonce` (unique per pairing).
struct VpnDeregister {
    registry: VpnRegistry,
    udp_registry: secret::UdpRegistry,
    id: String,
    /// Generation of the provider entry this handler registered.
    session: u64,
    /// Pairing nonce of the UDP entry this handler registered (post-pairing).
    nonce: Option<[u8; UDP_NONCE_LEN]>,
}

impl Drop for VpnDeregister {
    fn drop(&mut self) {
        self.registry
            .remove_if(&self.id, |_, entry| entry.session == self.session);
        let udp_id = format!("vpn:{}", self.id);
        self.udp_registry
            .remove_if(&udp_id, |_, reg| Some(reg.nonce) == self.nonce);
    }
}

/// Server-side handler for a VPN listener (`HelloVpn`).
/// Registers the listener, waits for a connector to pair, sends `VpnReady`,
/// then keeps the control connection alive with heartbeats.
#[allow(clippy::too_many_arguments)]
pub async fn serve_vpn_listener(
    mut control: Delimited<mux::Stream>,
    opener: mux::Opener,
    vpn_providers: VpnRegistry,
    id: String,
    advertised: Vec<Ipv4Net>,
    addr: VpnAddrRequest,
    notes: Option<String>,
    admin: AdminRegistry,
    peer: std::net::SocketAddr,
    udp_providers: secret::UdpRegistry,
    udp_tuning: UdpDirectTuning,
    link_permits: Arc<Semaphore>,
    carriers: u16,
    max_clients: u16,
    vpn_hub_prefix: u8,
    vpn_pool: Option<VpnPoolHandle>,
    relay_only: bool,
    pin_mtu: bool,
    mtu: Option<u16>,
    forward_accept: bool,
    nat_masquerade: bool,
    route_policy: Option<String>,
    nat_udp_preferred_port: u16,
) -> Result<()> {
    // Acquire link permit (bounds live VPN links).
    // Display-only derivations for the admin panel.
    let nat_udp_display = (nat_udp_preferred_port != 0).then_some(nat_udp_preferred_port);
    let advertised_display: Vec<String> = advertised.iter().map(|n| n.to_string()).collect();
    let _permit = match link_permits.try_acquire() {
        Ok(p) => p,
        Err(_) => {
            warn!(%id, "vpn max links reached");
            control
                .send(ServerMessage::VpnError(
                    "server vpn-max-links reached".into(),
                ))
                .await?;
            return Ok(());
        }
    };

    // Register atomically; reject duplicate id.
    let session = next_vpn_session();
    let effective_max_clients = if max_clients == 0 { 1 } else { max_clients };

    // Hub mode requires pool addressing.
    let mut _hub_lease: Option<VpnHubLeaseGuard> = None;
    let pair_rx: Option<oneshot::Receiver<VpnPairMsg>>;
    let mut event_rx_opt: Option<mpsc::Receiver<HubPeerEvent>> = None;
    let hub_opt: Option<HubShared>;

    if effective_max_clients > 1 {
        // Hub mode
        if addr != VpnAddrRequest::Pool {
            control
                .send(ServerMessage::VpnError(
                    "hub mode (--max-clients > 1) requires server pool addressing (D6)".into(),
                ))
                .await?;
            return Ok(());
        }
        let pool_arc = match vpn_pool {
            Some(p) => p,
            None => {
                control
                    .send(ServerMessage::VpnError(
                        "hub mode requires a server pool (--vpn-pool)".into(),
                    ))
                    .await?;
                return Ok(());
            }
        };
        let subnet = {
            let mut p = pool_arc.lock().unwrap_or_else(|poison| poison.into_inner());
            p.alloc_hub_subnet(vpn_hub_prefix)
                .context("vpn hub subnet allocation failed")?
        };
        if let Some(msg) = check_overlap(&advertised, &[], subnet) {
            control.send(ServerMessage::VpnError(msg)).await?;
            return Ok(());
        }
        let hub_state = HubState::new(subnet, advertised.clone());
        let (event_tx, event_rx) = mpsc::channel::<HubPeerEvent>(32);
        let hub_shared = HubShared {
            state: Arc::new(std::sync::Mutex::new(hub_state)),
            event_tx,
            max_clients: effective_max_clients,
        };
        _hub_lease = Some(VpnHubLeaseGuard::new(pool_arc, u32::from(subnet.network())));
        hub_opt = Some(hub_shared);
        event_rx_opt = Some(event_rx);
        pair_rx = None;
    } else {
        // 1:1 mode
        let (pair_tx, rx) = oneshot::channel::<VpnPairMsg>();
        pair_rx = Some(rx);
        hub_opt = None;
        // event_rx_opt stays None (initialized above) in 1:1 mode.

        match vpn_providers.entry(id.clone()) {
            Entry::Occupied(_) => {
                warn!(%id, "vpn id already in use");
                control
                    .send(ServerMessage::VpnError(format!(
                        "vpn id '{id}' already in use"
                    )))
                    .await?;
                return Ok(());
            }
            Entry::Vacant(slot) => {
                slot.insert(VpnProviderEntry {
                    advertised: advertised.clone(),
                    addr: addr.clone(),
                    opener: opener.clone(),
                    pair_tx: Some(pair_tx),
                    hub: None,
                    carriers,
                    session,
                });
            }
        }
    };

    if effective_max_clients > 1 {
        match vpn_providers.entry(id.clone()) {
            Entry::Occupied(_) => {
                warn!(%id, "vpn id already in use");
                control
                    .send(ServerMessage::VpnError(format!(
                        "vpn id '{id}' already in use"
                    )))
                    .await?;
                return Ok(());
            }
            Entry::Vacant(slot) => {
                slot.insert(VpnProviderEntry {
                    advertised: advertised.clone(),
                    addr: addr.clone(),
                    opener: opener.clone(),
                    pair_tx: None,
                    hub: hub_opt.clone(),
                    carriers,
                    session,
                });
            }
        }
    }
    // RAII deregister when this fn returns (generation-guarded, D5).
    let mut deregister = VpnDeregister {
        registry: vpn_providers.clone(),
        udp_registry: udp_providers.clone(),
        id: id.clone(),
        session,
        nonce: None,
    };

    // Admin entry. Registered before pairing so a waiting listener shows up on
    // the panel; the overlay is filled in via `set_overlay` once assigned.
    let admin_reg = admin.register(NewEntry {
        role: Role::VpnListener,
        peer,
        secret_id: Some(format!("vpn:{id}")),
        public_port: None,
        notes,
        basic_auth: false,
        https: false,
        force_https: false,
        // The listener's requested count; refreshed to the effective negotiated
        // value once a connector pairs (1:1) — see the pair-receive block below.
        carriers,
        auto_reconnect: false,
        webserver_log: false,
        udp: false,
        vpn_relay_only: relay_only,
        vpn_pin_mtu: pin_mtu,
        vpn_mtu: mtu,
        vpn_forward_accept: forward_accept,
        vpn_nat_masquerade: nat_masquerade,
        vpn_route_policy: route_policy,
        vpn_advertised: advertised_display,
        vpn_nat_udp_port: nat_udp_display,
        local_proxy_port: None,
        local_host: None,
        local_port: None,
        nat_udp_preferred_port: None,
        nat_udp_release_timeout: None,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        max_conns: None,
    });

    info!(%id, "vpn listener registered, waiting for connector");

    if effective_max_clients > 1 {
        // Hub mode
        let mut event_rx = event_rx_opt.take().unwrap();
        let hub = hub_opt.clone().unwrap();

        // The hub's own VpnReady carries no peer routes (peer_advertised: vec![]);
        // its advertised CIDRs reach spokes via each connector's VpnReady instead.
        let (hub_overlay, hub_prefix) = {
            let hub_state = hub.state.lock().unwrap();
            (hub_state.hub_overlay, hub_state.subnet.prefix)
        };

        control
            .send(ServerMessage::VpnReady {
                assigned: hub_overlay,
                prefix: hub_prefix,
                peer_overlay: hub_overlay,
                peer_advertised: vec![],
                session_nonce: new_nonce(),
                tuning: udp_tuning,
                admin_v2: true,
                carriers,
            })
            .await?;

        admin_reg.set_overlay(format!("{hub_overlay}/{hub_prefix}"));

        let heartbeat = Duration::from_millis(500);
        let mut hb = interval(heartbeat);
        hb.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = hb.tick() => {
                    if control.send(ServerMessage::Heartbeat).await.is_err() {
                        info!(%id, "vpn hub listener disconnected");
                        break;
                    }
                }
                Some(ev) = event_rx.recv() => {
                    match ev {
                        HubPeerEvent::Join {
                            peer_id,
                            overlay,
                            advertised,
                            nonce,
                            carriers,
                        } => {
                            if control
                                .send(ServerMessage::VpnPeerJoin {
                                    peer_id,
                                    peer_overlay: overlay,
                                    peer_advertised: advertised,
                                    session_nonce: nonce,
                                    carriers,
                                })
                                .await
                                .is_err()
                            {
                                info!(%id, "vpn hub listener disconnected");
                                break;
                            }
                        }
                        HubPeerEvent::Leave { peer_id } => {
                            if control
                                .send(ServerMessage::VpnPeerLeave { peer_id })
                                .await
                                .is_err()
                            {
                                info!(%id, "vpn hub listener disconnected");
                                break;
                            }
                        }
                        HubPeerEvent::Punch {
                            peer_id,
                            offer,
                        } => {
                            if control
                                .send(ServerMessage::UdpPunch {
                                    nonce: offer.nonce,
                                    peer: offer.peer_candidates,
                                    peer_selected_stun: offer.peer_selected_stun,
                                    tuning: udp_tuning,
                                    peer_id,
                                })
                                .await
                                .is_err()
                            {
                                info!(%id, "vpn hub listener disconnected");
                                break;
                            }
                        }
                    }
                }
                msg = control.recv::<ClientMessage>() => {
                    match msg {
                        Ok(Some(ClientMessage::UdpCandidateOffer(offer))) => {
                            {
                                let mut hub_state = hub.state.lock().unwrap();
                                hub_state.set_hub_candidates(
                                    offer.peer_id,
                                    offer.candidates,
                                    offer.selected_stun,
                                );
                            }
                        }
                        Ok(Some(ClientMessage::VpnPathReport { path })) => {
                            admin_reg.set_vpn_direct(path == "direct");
                        }
                        Ok(None) | Err(_) => {
                            info!(%id, "vpn hub listener disconnected");
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    } else {
        // 1:1 mode (unchanged)
        let mut pair_rx = pair_rx.unwrap(); // safe: 1:1 mode always has pair_rx

        // Wait for a connector to pair us (or control channel to close).
        let pair_msg = tokio::select! {
            result = &mut pair_rx => {
                match result {
                    Ok(msg) => msg,
                    Err(_) => {
                        warn!(%id, "vpn listener: pair channel dropped before connector arrived");
                        return Ok(());
                    }
                }
            }
            result = control.recv::<ClientMessage>() => {
                // Client disconnected before pairing.
                let _ = result;
                return Ok(());
            }
        };

        // Record the assigned overlay on the admin entry, then deliver VpnReady.
        if let ServerMessage::VpnReady {
            assigned,
            prefix,
            carriers,
            ..
        } = &pair_msg.listener_ready
        {
            admin_reg.set_overlay(format!("{assigned}/{prefix}"));
            // Refresh to the effective negotiated carrier count (the connector
            // handler computed min(listener, connector, server-max)).
            admin_reg.set_carriers(*carriers);
        }
        control.send(pair_msg.listener_ready).await?;

        // Register for UDP direct path (so connector can get our candidates).
        // The channel is REAL (unlike the Phase-4 stub): the connector handler
        // forwards its offer through `to_provider`, and the select arm below relays
        // the punch to the listener client.
        let udp_id = format!("vpn:{id}");
        let (to_provider_tx, mut to_provider_rx) =
            tokio::sync::mpsc::channel::<secret::UdpOffer>(4);
        udp_providers.insert(
            udp_id.clone(),
            secret::UdpReg {
                candidates: vec![],
                selected_stun: None,
                nonce: pair_msg.nonce,
                to_provider: to_provider_tx,
            },
        );
        // Arm the generation-guarded UDP removal for this pairing only.
        deregister.nonce = Some(pair_msg.nonce);

        // Heartbeat loop (same shape as serve_provider).
        let heartbeat = Duration::from_millis(500);
        let mut hb = interval(heartbeat);
        hb.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = hb.tick() => {
                    if let Err(_e) = control.send(ServerMessage::Heartbeat).await {
                        info!(%id, "vpn listener disconnected");
                        break;
                    }
                }
                Some(offer) = to_provider_rx.recv() => {
                    // The connector offered candidates: forward the punch so the
                    // listener can start its QUIC endpoint and punch back.
                    info!(%id, "forwarding vpn udp punch to listener");
                    if control.send(ServerMessage::UdpPunch {
                        nonce: offer.nonce,
                        peer: offer.peer_candidates,
                        peer_selected_stun: offer.peer_selected_stun,
                        tuning: udp_tuning,
                        peer_id: 0,
                    }).await.is_err() {
                        info!(%id, "vpn listener disconnected");
                        break;
                    }
                }
                msg = control.recv::<ClientMessage>() => {
                    match msg {
                        Ok(Some(ClientMessage::UdpCandidateOffer(offer))) => {
                            // Store candidates for the connector's broker to read.
                            if let Some(mut entry) = udp_providers.get_mut(&udp_id) {
                                entry.candidates = offer.candidates;
                                entry.selected_stun = offer.selected_stun;
                            }
                        }
                        Ok(Some(ClientMessage::UdpCandidates(addrs))) => {
                            // Legacy UDP candidates format.
                            if let Some(mut entry) = udp_providers.get_mut(&udp_id) {
                                entry.candidates = addrs;
                            }
                        }
                        Ok(Some(ClientMessage::VpnPathReport { path })) => {
                            admin_reg.set_vpn_direct(path == "direct");
                        }
                        Ok(None) | Err(_) => {
                            info!(%id, "vpn listener disconnected");
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

/// Server-side handler for a VPN connector (`ConnectVpn`).
/// Finds the listener entry, validates addressing, allocates overlay, pairs, sets up relay.
#[allow(clippy::too_many_arguments)]
pub async fn serve_vpn_connector(
    mut control: Delimited<mux::Stream>,
    mut acceptor: mux::Acceptor,
    vpn_providers: VpnRegistry,
    vpn_pool: Option<VpnPoolHandle>,
    conn_permits: Arc<Semaphore>,
    id: String,
    advertised: Vec<Ipv4Net>,
    addr: VpnAddrRequest,
    notes: Option<String>,
    admin: AdminRegistry,
    peer: std::net::SocketAddr,
    udp_providers: secret::UdpRegistry,
    udp_tuning: UdpDirectTuning,
    punch_timeout: Duration,
    carriers: u16,
    max_carriers: u16,
    grx: Arc<AtomicU64>,
    gtx: Arc<AtomicU64>,
    relay_only: bool,
    pin_mtu: bool,
    mtu: Option<u16>,
    forward_accept: bool,
    nat_masquerade: bool,
    route_policy: Option<String>,
    nat_udp_preferred_port: u16,
) -> Result<()> {
    info!(%id, "vpn connector connecting");
    // Display-only: client's preferred holepunch port (0 = ephemeral/unset).
    let nat_udp_display = (nat_udp_preferred_port != 0).then_some(nat_udp_preferred_port);

    // Acquire link permit.
    let _permit = match Arc::clone(&conn_permits).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            warn!(%id, "vpn max connections reached");
            control
                .send(ServerMessage::VpnError(
                    "server connection limit reached".into(),
                ))
                .await?;
            return Ok(());
        }
    };

    // Find the listener entry.
    let listener_entry = match vpn_providers.get(&id) {
        Some(entry) => entry,
        None => {
            warn!(%id, "vpn listener not found");
            control
                .send(ServerMessage::VpnError(format!(
                    "vpn listener '{id}' not found"
                )))
                .await?;
            return Ok(());
        }
    };
    let listener_advertised = listener_entry.advertised.clone();
    let listener_addr_req = listener_entry.addr.clone();
    let listener_opener = listener_entry.opener.clone();
    let listener_carriers = listener_entry.carriers;
    let hub = listener_entry.hub.clone();
    drop(listener_entry);

    // Negotiate the relay carrier count (DEC-7): both sides and the server
    // must agree; an old peer's missing field defaults to 1 (I-9).
    let effective_carriers = listener_carriers
        .max(1)
        .min(carriers.max(1))
        .min(max_carriers.max(1));

    // RAII guard for pool lease: freed on all error returns; lives for link duration.
    let mut _pool_lease: Option<VpnLeaseGuard> = None;

    // Hub mode branch
    if let Some(hub) = hub {
        // Hub mode: allocate a peer address from the hub subnet
        if !advertised.is_empty() {
            control
                .send(ServerMessage::VpnError(
                    "connector --advertise is not allowed in hub mode (hub-and-spoke only, D4)"
                        .into(),
                ))
                .await?;
            return Ok(());
        }
        if addr != VpnAddrRequest::Pool {
            control
                .send(ServerMessage::VpnError(
                    "hub mode requires pool addressing on the connector".into(),
                ))
                .await?;
            return Ok(());
        }

        // Capacity check + address allocation under ONE lock (no TOCTOU between
        // the check and the alloc: two concurrent connectors can't both pass a
        // stale capacity check and over-allocate).
        let alloc = {
            let mut state = hub.state.lock().unwrap();
            if state.peer_count() >= hub.max_clients as usize {
                Err(format!("hub '{id}' is at capacity (--max-clients reached)"))
            } else {
                state.alloc_peer().map_err(|e| e.to_string())
            }
        };
        let peer_slot = match alloc {
            Ok(slot) => slot,
            Err(msg) => {
                control.send(ServerMessage::VpnError(msg)).await?;
                return Ok(());
            }
        };

        // Free the address + notify the hub on ANY subsequent return path.
        let _peer_guard = HubPeerGuard {
            hub: hub.clone(),
            peer_id: peer_slot.peer_id,
            overlay: peer_slot.overlay,
        };

        let hub_overlay = {
            let state = hub.state.lock().unwrap();
            state.hub_overlay
        };
        let hub_prefix = {
            let state = hub.state.lock().unwrap();
            state.subnet.prefix
        };
        let hub_advertised = {
            let state = hub.state.lock().unwrap();
            state.advertised.clone()
        };

        // Send VpnReady to connector
        control
            .send(ServerMessage::VpnReady {
                assigned: peer_slot.overlay,
                prefix: hub_prefix,
                peer_overlay: hub_overlay,
                peer_advertised: hub_advertised.clone(),
                session_nonce: peer_slot.nonce,
                tuning: udp_tuning,
                admin_v2: true,
                carriers: effective_carriers,
            })
            .await?;

        // Notify hub of the new peer
        let _ = hub
            .event_tx
            .send(HubPeerEvent::Join {
                peer_id: peer_slot.peer_id,
                overlay: peer_slot.overlay,
                advertised: hub_advertised.clone(),
                nonce: peer_slot.nonce,
                carriers: effective_carriers,
            })
            .await;

        // Admin entry
        let admin_reg = admin.register(NewEntry {
            role: Role::VpnConnector,
            peer,
            secret_id: Some(format!("vpn:{id}")),
            public_port: None,
            notes,
            basic_auth: false,
            https: false,
            force_https: false,
            carriers: effective_carriers,
            auto_reconnect: false,
            webserver_log: false,
            udp: false,
            vpn_relay_only: relay_only,
            vpn_pin_mtu: pin_mtu,
            vpn_mtu: mtu,
            vpn_forward_accept: forward_accept,
            vpn_nat_masquerade: nat_masquerade,
            vpn_route_policy: route_policy,
            vpn_advertised: advertised.iter().map(|n| n.to_string()).collect(),
            vpn_nat_udp_port: nat_udp_display,
            local_proxy_port: None,
            local_host: None,
            local_port: None,
            nat_udp_preferred_port: None,
            nat_udp_release_timeout: None,
            stun_server: None,
            upnp: false,
            try_port_prediction: false,
            max_conns: None,
        });
        admin_reg.set_overlay(format!("{}/32", peer_slot.overlay));

        // Set up relay acceptor for this peer
        let id_clone = id.clone();
        let active_counter = admin_reg.active();
        let (relay_tx, relay_rx) = admin_reg.relay_bytes();
        let peer_id = peer_slot.peer_id;
        let acceptor_handle = tokio::spawn(async move {
            while let Some(connector_stream) = acceptor.accept().await {
                let permit = match Arc::clone(&conn_permits).try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let listener_opener = listener_opener.clone();
                let id = id_clone.clone();
                let guard = crate::admin::ActiveGuard::new(Arc::clone(&active_counter));
                let grx_clone = Arc::clone(&grx);
                let gtx_clone = Arc::clone(&gtx);
                let counted = CountingStream {
                    inner: connector_stream,
                    rx: Arc::clone(&relay_rx),
                    tx: Arc::clone(&relay_tx),
                    grx: grx_clone,
                    gtx: gtx_clone,
                };
                tokio::spawn(async move {
                    let _permit = permit;
                    let _guard = guard;
                    if let Err(e) = vpn_relay_hub(counted, listener_opener, peer_id).await {
                        tracing::trace!(%e, %id, "vpn hub relay closed");
                    }
                });
            }
        });

        // Broker loop (Phase 4.1: per-peer UDP broker, hub-side candidates polling)
        // Track per-spoke offer state: deadline-based rounds (re-arm on each fresh offer).
        // On each 500ms tick: check deadline timeout, poll hub.state for hub_candidates,
        // send UdpPunch when both offers are ready.
        let mut heartbeat = interval(Duration::from_millis(500));
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let hub_clone = hub.clone();
        let punch_timeout = Duration::from_secs(10);

        let mut spoke_offer: Option<crate::shared::UdpCandidateOffer> = None;
        let mut offer_deadline: Option<tokio::time::Instant> = None;
        let mut punched = false;

        loop {
            // Check if it's time to poll and punch (before select! to avoid deadlock on lock)
            if !punched {
                if let Some(offer) = spoke_offer.as_ref() {
                    // Clone out so no Mutex guard is held across an await point (DEC-6 pattern).
                    let hub_cands = {
                        let hub_state = hub_clone.state.lock().unwrap();
                        hub_state.peers.get(&peer_id).map(|slot| {
                            (slot.hub_candidates.clone(), slot.hub_selected_stun.clone())
                        })
                    };

                    match hub_cands {
                        Some((cands, stun)) if !cands.is_empty() => {
                            // Both offers ready: punch the spoke with hub's candidates.
                            if control
                                .send(ServerMessage::UdpPunch {
                                    nonce: peer_slot.nonce,
                                    peer: cands.clone(),
                                    peer_selected_stun: stun,
                                    tuning: udp_tuning,
                                    peer_id,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                            // Forward spoke's offer to the hub listener via event channel.
                            let fwd = secret::UdpOffer {
                                nonce: peer_slot.nonce,
                                peer_candidates: offer.candidates.clone(),
                                peer_selected_stun: offer.selected_stun.clone(),
                            };
                            let _ = hub_clone
                                .event_tx
                                .send(HubPeerEvent::Punch {
                                    peer_id,
                                    offer: fwd,
                                })
                                .await;
                            info!(%peer_id, "brokered vpn udp punch to hub and spoke");
                            punched = true;
                            // Clear hub_candidates so the next round waits for a fresh offer from hub.
                            {
                                let mut hub_state = hub_clone.state.lock().unwrap();
                                if let Some(slot) = hub_state.peers.get_mut(&peer_id) {
                                    slot.hub_candidates.clear();
                                    slot.hub_selected_stun = None;
                                }
                            }
                        }
                        Some(_) => {
                            // Hub registered but has not offered candidates yet: wait until deadline.
                            if offer_deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                                info!(%peer_id, "hub offered no udp candidates in time; spoke stays on relay");
                                let _ = control.send(ServerMessage::UdpUnavailable).await;
                                punched = true;
                            }
                        }
                        None => {
                            // Hub peer slot gone (hub disconnected or peer freed):
                            // direct path is impossible for this spoke.
                            info!(%peer_id, "no hub udp available; spoke will use relay");
                            let _ = control.send(ServerMessage::UdpUnavailable).await;
                            punched = true;
                        }
                    }
                }
            }

            tokio::select! {
                _ = heartbeat.tick() => {
                    if control.send(ServerMessage::Heartbeat).await.is_err() {
                        break;
                    }
                }
                msg = control.recv::<ClientMessage>() => {
                    match msg {
                        Ok(Some(ClientMessage::UdpCandidateOffer(offer))) => {
                            // Fresh offer: re-arm the broker round (client retries while on relay).
                            // Store offer and reset deadline for polling hub candidates.
                            spoke_offer = Some(offer);
                            offer_deadline = Some(tokio::time::Instant::now() + punch_timeout);
                            punched = false;
                            // Push the spoke's offer to the hub listener so it can punch back.
                            let _ = hub_clone
                                .event_tx
                                .send(HubPeerEvent::Punch {
                                    peer_id,
                                    offer: secret::UdpOffer {
                                        nonce: peer_slot.nonce,
                                        peer_candidates: spoke_offer.as_ref().unwrap().candidates.clone(),
                                        peer_selected_stun: spoke_offer.as_ref().unwrap().selected_stun.clone(),
                                    },
                                })
                                .await;
                        }
                        Ok(Some(ClientMessage::VpnPathReport { path })) => {
                            admin_reg.set_vpn_direct(path == "direct");
                        }
                        Ok(None) | Err(_) => break,
                        _ => {}
                    }
                }
            }
        }

        // Cleanup: stop accepting relay substreams. `_peer_guard` frees the
        // overlay address and notifies the hub (Leave) on drop — covering this
        // normal exit and every early `?` return above.
        acceptor_handle.abort();
        return Ok(());
    }

    // 1:1 mode (unchanged)
    // Determine overlay addressing based on listener and connector requests.
    let (listener_overlay, connector_overlay, nonce) = match (&listener_addr_req, &addr) {
        // Both use pool: allocate a /30
        (VpnAddrRequest::Pool, VpnAddrRequest::Pool) => {
            let pool_arc = match vpn_pool {
                Some(p) => p,
                None => {
                    control
                        .send(ServerMessage::VpnError(
                            "server has no vpn pool; use --vpn-addr/--vpn-peer-addr".into(),
                        ))
                        .await?;
                    return Ok(());
                }
            };
            // Sync lock, no await inside the critical section (see VpnPoolHandle).
            let alloc_result = {
                let mut pool_locked = pool_arc.lock().unwrap_or_else(|p| p.into_inner());
                pool_locked.alloc()
            };
            let (listener_ip, connector_ip) = match alloc_result {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(%id, %e, "vpn pool exhausted");
                    control.send(ServerMessage::VpnError(e.to_string())).await?;
                    return Ok(());
                }
            };
            // Arm the lease guard immediately so the block is freed on all
            // subsequent early returns (overlap rejection, send failure, etc.).
            // The guard lives until serve_vpn_connector returns.
            let net_addr = u32::from(listener_ip) - 1;
            _pool_lease = Some(VpnLeaseGuard::new(pool_arc, net_addr));
            let n = new_nonce();
            (listener_ip, connector_ip, n)
        }
        // Both use static: validate consistency
        (
            VpnAddrRequest::Static {
                addr: l_addr,
                prefix: l_prefix,
                peer: l_peer,
            },
            VpnAddrRequest::Static {
                addr: c_addr,
                prefix: c_prefix,
                peer: c_peer,
            },
        ) => {
            if let Err(e) =
                validate_static(*l_addr, *l_prefix, *l_peer, *c_addr, *c_prefix, *c_peer)
            {
                warn!(%id, %e, "vpn static addressing inconsistency");
                control.send(ServerMessage::VpnError(e.to_string())).await?;
                return Ok(());
            }
            let n = new_nonce();
            (*l_addr, *c_addr, n)
        }
        // Mixed mode: error (§5 rule 1)
        _ => {
            let listener_mode = match &listener_addr_req {
                VpnAddrRequest::Pool => "Pool",
                _ => "Static",
            };
            let connector_mode = match &addr {
                VpnAddrRequest::Pool => "Pool",
                _ => "Static",
            };
            let msg = format!(
                "addressing mode mismatch: listener={listener_mode} connector={connector_mode}"
            );
            warn!(%id, %msg, "vpn addr mode mismatch");
            control.send(ServerMessage::VpnError(msg)).await?;
            return Ok(());
        }
    };

    let overlay = Ipv4Net {
        addr: listener_overlay,
        prefix: 30,
    };

    // Check for overlap
    if let Some(msg) = check_overlap(&listener_advertised, &advertised, overlay) {
        control.send(ServerMessage::VpnError(msg)).await?;
        return Ok(());
    }

    // Create VpnReady messages
    let listener_ready = ServerMessage::VpnReady {
        assigned: listener_overlay,
        prefix: 30,
        peer_overlay: connector_overlay,
        peer_advertised: advertised.clone(),
        session_nonce: nonce,
        tuning: udp_tuning,
        admin_v2: true,
        carriers: effective_carriers,
    };

    let connector_ready = ServerMessage::VpnReady {
        assigned: connector_overlay,
        prefix: 30,
        peer_overlay: listener_overlay,
        peer_advertised: listener_advertised.clone(),
        session_nonce: nonce,
        tuning: udp_tuning,
        admin_v2: true,
        carriers: effective_carriers,
    };

    // Send VpnReady to connector
    control.send(connector_ready).await?;

    // Send listener's VpnReady via the pair channel
    // Extract the entry and send the pair_tx (consuming it)
    if let Some((_, entry)) = vpn_providers.remove(&id) {
        // This fails silently if the listener disconnected.
        if let Some(pair_tx) = entry.pair_tx {
            let _ = pair_tx.send(VpnPairMsg {
                listener_ready,
                nonce,
            });
        }
        // Note: entry is not re-inserted; it's consumed after pairing
    }

    // Admin entry
    let admin_reg = admin.register(NewEntry {
        role: Role::VpnConnector,
        peer,
        secret_id: Some(format!("vpn:{id}")),
        public_port: None,
        notes,
        basic_auth: false,
        https: false,
        force_https: false,
        carriers: effective_carriers,
        auto_reconnect: false,
        webserver_log: false,
        udp: false,
        vpn_relay_only: relay_only,
        vpn_pin_mtu: pin_mtu,
        vpn_mtu: mtu,
        vpn_forward_accept: forward_accept,
        vpn_nat_masquerade: nat_masquerade,
        vpn_route_policy: route_policy,
        vpn_advertised: advertised.iter().map(|n| n.to_string()).collect(),
        vpn_nat_udp_port: nat_udp_display,
        local_proxy_port: None,
        local_host: None,
        local_port: None,
        nat_udp_preferred_port: None,
        nat_udp_release_timeout: None,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        max_conns: None,
    });
    admin_reg.set_overlay(format!("{connector_overlay}/30"));

    // Set up relay: accept substreams from connector, forward to listener.
    // Each substream counts toward the entry's live connections and its
    // ciphertext byte totals (the server only ever sees AEAD ciphertext).
    let id_clone = id.clone();
    let active_counter = admin_reg.active();
    let (relay_tx, relay_rx) = admin_reg.relay_bytes();
    let acceptor_handle = tokio::spawn(async move {
        while let Some(connector_stream) = acceptor.accept().await {
            let permit = match Arc::clone(&conn_permits).try_acquire_owned() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let listener_opener = listener_opener.clone();
            let id = id_clone.clone();
            let guard = crate::admin::ActiveGuard::new(Arc::clone(&active_counter));
            let grx_clone = Arc::clone(&grx);
            let gtx_clone = Arc::clone(&gtx);
            let counted = CountingStream {
                inner: connector_stream,
                rx: Arc::clone(&relay_rx),
                tx: Arc::clone(&relay_tx),
                grx: grx_clone,
                gtx: gtx_clone,
            };
            tokio::spawn(async move {
                let _permit = permit;
                let _guard = guard;
                if let Err(e) = vpn_relay(counted, listener_opener, &id).await {
                    tracing::trace!(%e, %id, "vpn relay closed");
                }
            });
        }
    });

    // Broker UDP candidates (DEC-3): the punch is sent to BOTH sides only once
    // the server holds BOTH offers. The connector's offer is buffered here; the
    // listener's candidates are read from the UDP registry on every wake-up
    // (500 ms tick or message arrival). If the listener has produced nothing
    // within `punch_timeout` of the connector's offer, the connector gets
    // `UdpUnavailable` and stays on relay.
    let udp_id = format!("vpn:{id}");
    let mut connector_offer: Option<crate::shared::UdpCandidateOffer> = None;
    let mut offer_deadline: Option<tokio::time::Instant> = None;
    let mut punched = false;
    let mut heartbeat = interval(Duration::from_millis(500));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        if !punched {
            if let Some(offer) = connector_offer.as_ref() {
                // Clone out so no DashMap guard is held across an await point.
                let provider = udp_providers.get(&udp_id).map(|e| {
                    (
                        e.candidates.clone(),
                        e.selected_stun.clone(),
                        e.nonce,
                        e.to_provider.clone(),
                    )
                });
                match provider {
                    Some((cands, stun, nonce, to_provider)) if !cands.is_empty() => {
                        // Both offers known: punch the connector with the
                        // listener's candidates...
                        control
                            .send(ServerMessage::UdpPunch {
                                nonce,
                                peer: cands,
                                peer_selected_stun: stun,
                                tuning: udp_tuning,
                                peer_id: 0,
                            })
                            .await?;
                        // ...and forward the connector's offer to the listener.
                        let fwd = secret::UdpOffer {
                            nonce,
                            peer_candidates: offer.candidates.clone(),
                            peer_selected_stun: offer.selected_stun.clone(),
                        };
                        let _ = to_provider.send(fwd).await;
                        info!(%id, "brokered vpn udp punch to both peers");
                        punched = true;
                        // Consume the listener's candidates: on the next retry
                        // round (the client keeps retrying while on relay) its
                        // socket will have changed, so a stale candidate here
                        // would make the connector punch a DEAD port and time
                        // out. Clearing forces the next round to wait for a
                        // FRESH listener offer — mirroring the first round's
                        // empty-registry behaviour.
                        if let Some(mut e) = udp_providers.get_mut(&udp_id) {
                            e.candidates.clear();
                            e.selected_stun = None;
                        }
                    }
                    Some(_) => {
                        // Listener registered but has not offered candidates yet:
                        // wait until the deadline.
                        if offer_deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                            info!(%id, "vpn listener offered no udp candidates in time; connector stays on relay");
                            control.send(ServerMessage::UdpUnavailable).await?;
                            punched = true;
                        }
                    }
                    None => {
                        // Listener UDP entry gone (listener disconnected):
                        // direct path is impossible for this pairing.
                        info!(%id, "no vpn listener udp available; connector will use relay");
                        control.send(ServerMessage::UdpUnavailable).await?;
                        punched = true;
                    }
                }
            }
        }
        tokio::select! {
            _ = heartbeat.tick() => {
                // Heartbeat; if connector is gone, exit
                if control.send(ServerMessage::Heartbeat).await.is_err() {
                    break;
                }
            }
            msg = control.recv::<ClientMessage>() => {
                match msg {
                    Ok(Some(ClientMessage::UdpCandidateOffer(consumer_offer))) => {
                        // A fresh offer begins a new direct-upgrade round (the
                        // client retries while staying on relay): re-arm the
                        // broker so it punches again with the latest candidates,
                        // even after a prior round already punched or timed out.
                        connector_offer = Some(consumer_offer);
                        offer_deadline = Some(tokio::time::Instant::now() + punch_timeout);
                        punched = false;
                    }
                    Ok(Some(ClientMessage::UdpCandidates(consumer_cands))) => {
                        connector_offer = Some(crate::shared::UdpCandidateOffer {
                            candidates: consumer_cands,
                            selected_stun: None,
                            peer_id: 0,
                        });
                        offer_deadline = Some(tokio::time::Instant::now() + punch_timeout);
                        punched = false;
                    }
                    Ok(Some(ClientMessage::VpnPathReport { path })) => {
                        admin_reg.set_vpn_direct(path == "direct");
                    }
                    Ok(None) | Err(_) => break,
                    _ => {}
                }
            }
        }
    }

    // Connector disconnected; clean up
    acceptor_handle.abort();
    Ok(())
}

/// Relay a connector substream to the hub listener, injecting the peer_id header.
async fn vpn_relay_hub<S>(
    mut connector: S,
    listener_opener: mux::Opener,
    peer_id: u32,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Consume the connector's readiness marker
    let mut marker = [0u8; 1];
    connector.read_exact(&mut marker).await?;

    // Open a substream to the listener
    let mut listener = listener_opener.open().await.context("hub unavailable")?;

    // Write STREAM_READY + peer_id header
    let mut head = [0u8; 5];
    head[0] = mux::STREAM_READY;
    head[1..5].copy_from_slice(&peer_id.to_be_bytes());
    listener.write_all(&head).await?;

    let buf = proxy_buffer_size();
    tokio::io::copy_bidirectional_with_sizes(&mut connector, &mut listener, buf, buf).await?;
    Ok(())
}

/// Relay a connector substream to the listener.
async fn vpn_relay<S>(mut connector: S, listener_opener: mux::Opener, _id: &str) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Consume the connector's readiness marker
    let mut marker = [0u8; 1];
    connector.read_exact(&mut marker).await?;

    // Open a substream to the listener
    let mut listener = listener_opener
        .open()
        .await
        .context("listener unavailable")?;
    listener.write_all(&[mux::STREAM_READY]).await?;

    let buf = proxy_buffer_size();
    tokio::io::copy_bidirectional_with_sizes(&mut connector, &mut listener, buf, buf).await?;
    Ok(())
}

/// Default time the connector broker waits for the listener's UDP candidates
/// after the connector's own offer arrived, before giving up with
/// `UdpUnavailable` (DEC-3). Overridable via `Server::set_vpn_punch_timeout`
/// (used by tests).
pub const DEFAULT_VPN_PUNCH_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with_session(opener: mux::Opener, session: u64) -> VpnProviderEntry {
        let (pair_tx, _pair_rx) = oneshot::channel();
        VpnProviderEntry {
            advertised: vec![],
            addr: VpnAddrRequest::Pool,
            opener,
            pair_tx: Some(pair_tx),
            hub: None,
            carriers: 1,
            session,
        }
    }

    fn udp_reg(nonce: [u8; UDP_NONCE_LEN]) -> secret::UdpReg {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        secret::UdpReg {
            candidates: vec![],
            selected_stun: None,
            nonce,
            to_provider: tx,
        }
    }

    /// D5 — a stale handler's deregistration must not remove the entries of a
    /// newer session that re-registered the same id (listener reconnect race).
    #[tokio::test]
    async fn vpn_deregister_does_not_remove_newer_session() {
        let registry: VpnRegistry = Arc::new(DashMap::new());
        let udp_registry: secret::UdpRegistry = Default::default();
        let (a, _b) = tokio::io::duplex(1024);
        let (opener, _acceptor) = mux::client(a);

        // Session 1 registers and pairs (UDP nonce [1; 16]).
        let s1 = next_vpn_session();
        registry.insert("x".into(), entry_with_session(opener.clone(), s1));
        udp_registry.insert("vpn:x".into(), udp_reg([1u8; UDP_NONCE_LEN]));
        let dereg1 = VpnDeregister {
            registry: registry.clone(),
            udp_registry: udp_registry.clone(),
            id: "x".into(),
            session: s1,
            nonce: Some([1u8; UDP_NONCE_LEN]),
        };

        // Reconnect: session 2 re-registers the same id with a new nonce.
        let s2 = next_vpn_session();
        registry.insert("x".into(), entry_with_session(opener, s2));
        udp_registry.insert("vpn:x".into(), udp_reg([2u8; UDP_NONCE_LEN]));

        // The stale handler's Drop runs AFTER the new registration.
        drop(dereg1);

        let entry = registry
            .get("x")
            .expect("newer provider entry must survive");
        assert_eq!(entry.session, s2);
        drop(entry);
        let udp = udp_registry
            .get("vpn:x")
            .expect("newer udp entry must survive");
        assert_eq!(udp.nonce, [2u8; UDP_NONCE_LEN]);
    }

    /// D5 — the guard still removes its OWN session's entries.
    #[tokio::test]
    async fn vpn_deregister_removes_own_session() {
        let registry: VpnRegistry = Arc::new(DashMap::new());
        let udp_registry: secret::UdpRegistry = Default::default();
        let (a, _b) = tokio::io::duplex(1024);
        let (opener, _acceptor) = mux::client(a);

        let s1 = next_vpn_session();
        registry.insert("y".into(), entry_with_session(opener, s1));
        udp_registry.insert("vpn:y".into(), udp_reg([7u8; UDP_NONCE_LEN]));
        let dereg = VpnDeregister {
            registry: registry.clone(),
            udp_registry: udp_registry.clone(),
            id: "y".into(),
            session: s1,
            nonce: Some([7u8; UDP_NONCE_LEN]),
        };
        drop(dereg);

        assert!(registry.get("y").is_none(), "own entry must be removed");
        assert!(
            udp_registry.get("vpn:y").is_none(),
            "own udp entry must be removed"
        );
    }

    /// Verify that CountingStream bumps both per-entry and global byte counters
    /// identically when reading and writing.
    #[tokio::test]
    async fn t_counting_stream_bumps_global() {
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Create an in-memory duplex stream
        let (mut left, right) = tokio::io::duplex(1024);

        // Create per-entry and global atomics
        let entry_rx = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let entry_tx = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let global_rx = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let global_tx = Arc::new(std::sync::atomic::AtomicU64::new(0));

        // Wrap the right side with CountingStream
        let mut counted = CountingStream {
            inner: right,
            rx: Arc::clone(&entry_rx),
            tx: Arc::clone(&entry_tx),
            grx: Arc::clone(&global_rx),
            gtx: Arc::clone(&global_tx),
        };

        // Spawn a task to write and read through the counting stream
        let counted_task = tokio::spawn(async move {
            let mut buf = [0u8; 100];
            // Read 50 bytes from left
            let n = counted.read(&mut buf).await.expect("read");
            assert_eq!(n, 50);
            // Write 75 bytes back
            counted.write_all(&[42u8; 75]).await.expect("write");
        });

        // Main task: write 50 bytes, read 75 bytes
        left.write_all(&[1u8; 50]).await.expect("main write");
        let mut buf = [0u8; 100];
        let n = left.read(&mut buf).await.expect("main read");
        assert_eq!(n, 75);

        counted_task.await.expect("task completed");

        // Verify both per-entry and global counters advanced identically
        let entry_rx_val = entry_rx.load(Ordering::Relaxed);
        let entry_tx_val = entry_tx.load(Ordering::Relaxed);
        let global_rx_val = global_rx.load(Ordering::Relaxed);
        let global_tx_val = global_tx.load(Ordering::Relaxed);

        assert_eq!(entry_rx_val, 50, "per-entry rx must be 50");
        assert_eq!(entry_tx_val, 75, "per-entry tx must be 75");
        assert_eq!(global_rx_val, 50, "global rx must be 50");
        assert_eq!(global_tx_val, 75, "global tx must be 75");
    }
}
