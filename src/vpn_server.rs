//! Server-side VPN registry, pairing, pool allocation, and relay brokering.
//! Feature-gated: only compiled when `--features vpn` is active.

#![cfg(feature = "vpn")]

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use dashmap::{mapref::entry::Entry, DashMap};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Semaphore};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};

use crate::admin::{AdminRegistry, NewEntry, Role};
use crate::mux;
use crate::secret;
use crate::shared::{
    proxy_buffer_size, ClientMessage, Delimited, Ipv4Net, ServerMessage, UdpDirectTuning,
    VpnAddrRequest, UDP_NONCE_LEN,
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

/// What the VPN listener registers while waiting for a connector.
pub struct VpnProviderEntry {
    /// Networks this side advertises.
    pub advertised: Vec<Ipv4Net>,
    /// How this side wants its overlay address assigned.
    pub addr: VpnAddrRequest,
    /// The listener's mux opener (for relay substream).
    pub opener: mux::Opener,
    /// Connector sends pairing info here; listener awaits this.
    pub pair_tx: oneshot::Sender<VpnPairMsg>,
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

/// Pure /30 allocator, no I/O.
pub struct VpnPool {
    parent: Ipv4Net,
    allocated: HashSet<u32>, // network address of each /30 block (as u32)
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
        })
    }

    /// Allocate next free /30. Returns (listener_addr .1, connector_addr .2).
    pub fn alloc(&mut self) -> Result<(Ipv4Addr, Ipv4Addr)> {
        let base = u32::from(self.parent.network());
        let host_bits = 32 - self.parent.prefix;
        let total_blocks = 1u32 << host_bits.saturating_sub(2); // each /30 = 4 addrs
        for i in 0..total_blocks {
            let net_addr = base + i * 4;
            if !self.allocated.contains(&net_addr) {
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

    /// Check whether an address collides with any live allocation.
    pub fn is_allocated(&self, addr: Ipv4Addr) -> bool {
        let net_addr = u32::from(addr) & 0xFFFF_FFFC;
        self.allocated.contains(&net_addr)
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
) -> Result<()> {
    // Acquire link permit (bounds live VPN links).
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
    let (pair_tx, pair_rx) = oneshot::channel::<VpnPairMsg>();
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
                opener,
                pair_tx,
                carriers,
                session,
            });
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
        udp: false,
    });

    info!(%id, "vpn listener registered, waiting for connector");

    // Wait for a connector to pair us (or control channel to close).
    let pair_msg = tokio::select! {
        result = pair_rx => {
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
        assigned, prefix, ..
    } = &pair_msg.listener_ready
    {
        admin_reg.set_overlay(format!("{assigned}/{prefix}"));
    }
    control.send(pair_msg.listener_ready).await?;

    // Register for UDP direct path (so connector can get our candidates).
    // The channel is REAL (unlike the Phase-4 stub): the connector handler
    // forwards its offer through `to_provider`, and the select arm below relays
    // the punch to the listener client.
    let udp_id = format!("vpn:{id}");
    let (to_provider_tx, mut to_provider_rx) = tokio::sync::mpsc::channel::<secret::UdpOffer>(4);
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
) -> Result<()> {
    info!(%id, "vpn connector connecting");

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
    drop(listener_entry);

    // Negotiate the relay carrier count (DEC-7): both sides and the server
    // must agree; an old peer's missing field defaults to 1 (I-9).
    let effective_carriers = listener_carriers
        .max(1)
        .min(carriers.max(1))
        .min(max_carriers.max(1));

    // RAII guard for pool lease: freed on all error returns; lives for link duration.
    let mut _pool_lease: Option<VpnLeaseGuard> = None;

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
        let _ = entry.pair_tx.send(VpnPairMsg {
            listener_ready,
            nonce,
        });
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
        udp: false,
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
            let counted = CountingStream {
                inner: connector_stream,
                rx: Arc::clone(&relay_rx),
                tx: Arc::clone(&relay_tx),
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

/// Byte-counting wrapper around a relay substream: reads from the inner stream
/// bump `rx`, writes bump `tx`. The counters power the admin page's relay
/// traffic columns (ciphertext totals — the server never sees plaintext).
struct CountingStream<S> {
    inner: S,
    rx: Arc<std::sync::atomic::AtomicU64>,
    tx: Arc<std::sync::atomic::AtomicU64>,
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for CountingStream<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let res = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &res {
            let n = (buf.filled().len() - before) as u64;
            self.rx.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        }
        res
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for CountingStream<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let res = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &res {
            self.tx
                .fetch_add(*n as u64, std::sync::atomic::Ordering::Relaxed);
        }
        res
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
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
            pair_tx,
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
}
