//! Named "secret" tunnels: a provider and a consumer rendezvous on the server by
//! a shared `tcp-secret-id`, with no public port allocated.
//!
//! - The **provider** (`bore local --tcp-secret-id <id>`) is an ordinary client
//!   ([`crate::client::Client::new_secret_provider`]) that registers under `id`
//!   instead of requesting a public port.
//! - The **consumer** ([`Proxy`], `bore proxy`) binds a local listener and opens
//!   one substream per accepted connection to the server.
//! - The **server** relays each consumer substream to the registered provider
//!   over a freshly opened substream, splicing the two together. No port is bound.

use std::future::pending;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::time::{interval, Instant as TokioInstant, MissedTickBehavior};
#[cfg(feature = "udp")]
use tracing::debug;
use tracing::{error, info, info_span, trace, warn, Instrument};

use crate::admin::{ActiveGuard, AdminRegistry, NewEntry, Registration, Role};
use crate::auth::Authenticator;
use crate::mux;
use crate::pool::{self, Carrier, CarrierPool, PendingCarriers, TokenGuard};
use crate::reconnect;
use crate::shared::{
    proxy_buffer_size, tune_tcp, ClientMessage, Delimited, ServerMessage, UdpCandidateOffer,
    UdpDirectTuning, UDP_NONCE_LEN,
};
use crate::transport::{self, Endpoint};
use uuid::Uuid;

/// Heartbeat interval on secret-tunnel control substreams.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// How long the server waits for *any* control frame from a secret
/// provider/consumer before reaping the connection (and its admin entry). The
/// control channel is a yamux substream, so a half-open/abandoned peer is
/// invisible to `send`/`recv`; without this deadline the loop blocks forever on
/// `recv` and the RAII admin `Registration` never drops — a zombie entry. The
/// client sends [`ClientMessage::Heartbeat`] every [`CTRL_CLIENT_HEARTBEAT`] so a
/// healthy idle tunnel always beats this. Parity with the VPN
/// `CTRL_HEARTBEAT_TIMEOUT` (60 s). Overridable per-server (see
/// [`crate::server::Server::secret_ctrl_timeout`]) so tests can reap fast.
pub(crate) const SECRET_CTRL_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a secret provider/consumer *client* sends [`ClientMessage::Heartbeat`]
/// up the control substream. Must stay well under [`SECRET_CTRL_TIMEOUT`] so a
/// few lost frames never trip the server's reaper.
pub(crate) const CTRL_CLIENT_HEARTBEAT: Duration = Duration::from_secs(20);

/// How long a consumer waits for the server to broker a UDP direct path before
/// falling back to the relay.
#[cfg(feature = "udp")]
const UDP_NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(5);

/// Payload of a successful relay→direct upgrade: the authenticated direct QUIC
/// connection. Without the `udp` feature there is no direct path, so it is the
/// uninhabited [`std::convert::Infallible`] — the upgrade channels are still
/// declared (so `tokio::select!`, which forbids `#[cfg]` on arms, type-checks) but
/// nothing ever sends on them.
#[cfg(feature = "udp")]
type DirectUpgrade = crate::holepunch::DirectConn;
#[cfg(not(feature = "udp"))]
type DirectUpgrade = std::convert::Infallible;

/// Registry mapping each `tcp-secret-id` to the provider's carrier pool. The pool
/// holds one substream opener per parallel provider connection (`--carriers`);
/// [`relay`] round-robins across the live ones. With the default single connection
/// the pool simply has one member.
pub type Registry = Arc<DashMap<String, Arc<CarrierPool>>>;

/// Registry of UDP-capable providers, keyed by `tcp-secret-id`, used to broker a
/// direct hole-punched path. Independent of [`Registry`] (which always carries
/// the relay path) and free of any QUIC dependency, so the server brokers UDP
/// regardless of whether it was built with the `udp` feature.
pub type UdpRegistry = Arc<DashMap<String, UdpReg>>;

/// A UDP-capable provider's registration: its candidate addresses, a stable
/// per-provider session nonce, and a channel to deliver a consumer's offer to
/// the provider's control task.
pub struct UdpReg {
    /// The provider's hole-punch candidate addresses.
    pub candidates: Vec<SocketAddr>,
    /// STUN server selected by the provider, if known.
    pub selected_stun: Option<String>,
    /// Stable session nonce for this provider; every consumer derives the same
    /// QUIC token from it, so the provider's persistent QUIC listener can
    /// authenticate any of them (and reconnecting consumers).
    pub nonce: [u8; UDP_NONCE_LEN],
    /// Delivers a consumer offer to the provider's control task.
    pub to_provider: mpsc::Sender<UdpOffer>,
}

/// A consumer's offer relayed to a provider so it can punch back and accept a
/// direct connection.
pub struct UdpOffer {
    /// The provider's stable session nonce (so the relayed `UdpPunch` carries it).
    pub nonce: [u8; UDP_NONCE_LEN],
    /// The consumer's candidate addresses to punch toward.
    pub peer_candidates: Vec<SocketAddr>,
    /// STUN server selected by the consumer, if known.
    pub peer_selected_stun: Option<String>,
}

fn udp_offer_from_legacy(candidates: Vec<SocketAddr>) -> UdpCandidateOffer {
    UdpCandidateOffer {
        candidates,
        selected_stun: None,
        peer_id: 0,
    }
}

fn register_provider_udp_offer(
    udp_registry: &UdpRegistry,
    id: &str,
    admin_reg: &Registration,
    offers: &mut Option<mpsc::Receiver<UdpOffer>>,
    offer: UdpCandidateOffer,
) {
    info!(
        %id,
        candidates = ?offer.candidates,
        selected_stun = ?offer.selected_stun,
        "provider offered udp candidates"
    );
    admin_reg.mark_udp();

    let (to_provider, rx) = match udp_registry.get(id) {
        Some(existing) => (existing.to_provider.clone(), None),
        None => {
            let (tx, rx) = mpsc::channel(4);
            (tx, Some(rx))
        }
    };
    let nonce = udp_registry
        .get(id)
        .map(|existing| existing.nonce)
        .unwrap_or_else(new_nonce);

    udp_registry.insert(
        id.to_string(),
        UdpReg {
            candidates: offer.candidates,
            selected_stun: offer.selected_stun,
            nonce,
            to_provider,
        },
    );
    if let Some(rx) = rx {
        *offers = Some(rx);
    }
}

fn provider_stun_hint(udp_registry: &UdpRegistry, id: &str) -> Option<String> {
    udp_registry
        .get(id)
        .and_then(|provider| provider.selected_stun.clone())
}

/// Generate a fresh random session nonce from the system CSPRNG. The nonce keys
/// the direct-path token; with no `--secret` it is the *only* entropy, so it must
/// be cryptographically unpredictable (not a fast PRNG).
fn new_nonce() -> [u8; UDP_NONCE_LEN] {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut nonce = [0u8; UDP_NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .expect("system CSPRNG must not fail");
    nonce
}

/// Removes a provider registration when the provider connection ends.
struct Deregister {
    registry: Registry,
    udp_registry: UdpRegistry,
    id: String,
}

impl Drop for Deregister {
    fn drop(&mut self) {
        self.registry.remove(&self.id);
        self.udp_registry.remove(&self.id);
    }
}

/// Display-only flag/parameter bundle carried by `HelloSecret`/`ConnectSecret`
/// for the admin status page. None of these affect the data path; they are
/// captured into the admin [`crate::admin::Entry`] so the dashboard can show
/// every flag a secret provider/consumer was launched with (flag-parity).
pub struct SecretDisplay {
    /// `--udp` (direct path requested).
    pub udp: bool,
    /// `--auto-reconnect`.
    pub auto_reconnect: bool,
    /// `--webserver-log` enabled (provider only).
    pub webserver_log: bool,
    /// `--nat-udp-preferred-port` (0 = unset).
    pub nat_udp_preferred_port: u16,
    /// `--nat-udp-release-timeout` seconds.
    pub nat_udp_release_timeout: u64,
    /// `--stun-server`, if any.
    pub stun_server: Option<String>,
    /// `--upnp`.
    pub upnp: bool,
    /// `--try-port-prediction`.
    pub try_port_prediction: bool,
    /// `--max-conns` (provider only; 0 = unset/default).
    pub max_conns: usize,
    /// `-l/--local-host` (provider local target).
    pub local_host: Option<String>,
    /// Provider local target port (0 = unknown).
    pub local_port: u16,
    /// Consumer `--local-proxy-port` (0 = unset).
    pub local_proxy_port: u16,
    /// `--carriers` requested.
    pub carriers: u16,
}

impl SecretDisplay {
    /// Apply the display-only fields onto a [`NewEntry`] under construction.
    fn fill(self, mut new: NewEntry) -> NewEntry {
        new.udp = self.udp;
        new.auto_reconnect = self.auto_reconnect;
        new.webserver_log = self.webserver_log;
        new.carriers = self.carriers;
        new.nat_udp_preferred_port =
            (self.nat_udp_preferred_port != 0).then_some(self.nat_udp_preferred_port);
        new.nat_udp_release_timeout =
            (self.nat_udp_release_timeout != 0).then_some(self.nat_udp_release_timeout);
        new.stun_server = self.stun_server;
        new.upnp = self.upnp;
        new.try_port_prediction = self.try_port_prediction;
        new.max_conns = (self.max_conns != 0).then_some(self.max_conns);
        new.local_host = self.local_host;
        new.local_port = (self.local_port != 0).then_some(self.local_port);
        new.local_proxy_port = (self.local_proxy_port != 0).then_some(self.local_proxy_port);
        new
    }
}

/// Server side: register this connection as the provider for `id`, then keep it
/// alive with heartbeats until it disconnects (which deregisters it).
#[allow(clippy::too_many_arguments)]
pub async fn serve_provider(
    mut control: Delimited<mux::Stream>,
    opener: mux::Opener,
    registry: Registry,
    udp_registry: UdpRegistry,
    id: String,
    admin: AdminRegistry,
    peer: SocketAddr,
    notes: Option<String>,
    basic_auth: bool,
    pending_carriers: PendingCarriers,
    max_carriers: u16,
    carriers: u16,
    udp_tuning: UdpDirectTuning,
    display: SecretDisplay,
    ctrl_timeout: Duration,
) -> Result<()> {
    // Register atomically, rejecting a duplicate id rather than hijacking it. The
    // registration is a carrier pool seeded with this connection's opener; extra
    // provider connections (`--carriers`) join the pool via `JoinCarrier`.
    let pool = match registry.entry(id.clone()) {
        Entry::Occupied(_) => {
            warn!(%id, "secret tunnel id already in use");
            let msg = format!("tcp-secret-id '{id}' already in use");
            control.send(ServerMessage::Error(msg)).await?;
            return Ok(());
        }
        Entry::Vacant(slot) => {
            let pool = Arc::new(CarrierPool::new(opener));
            slot.insert(Arc::clone(&pool));
            pool
        }
    };
    let _guard = Deregister {
        registry: registry.clone(),
        udp_registry: udp_registry.clone(),
        id: id.clone(),
    };
    // Live admin entry for this provider; dropped (removed) when it disconnects.
    let admin_reg = admin.register(display.fill(NewEntry {
        role: Role::SecretProvider,
        peer,
        secret_id: Some(id.clone()),
        public_port: None,
        notes,
        basic_auth,
        https: false,
        force_https: false,
        carriers,
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
    }));
    info!(%id, "secret provider registered");
    control.send(ServerMessage::Ok).await?;

    // Carrier pool: if the provider requested more than one carrier, issue a token
    // and tell it how many extra connections to open. They arrive as `JoinCarrier`
    // handshakes and their openers are delivered here (via `carrier_rx`) and added
    // to the pool, so `relay` round-robins relayed substreams across them.
    let effective = carriers.clamp(1, max_carriers.max(1));
    let mut carrier_rx = if carriers > 1 {
        let extra = effective - 1;
        let token = Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::unbounded_channel();
        pending_carriers.insert(token.clone(), tx);
        control
            .send(ServerMessage::CarrierToken {
                token: token.clone(),
                extra,
            })
            .await?;
        info!(%id, extra, "provider carrier pool offered");
        Some((rx, TokenGuard::new(pending_carriers.clone(), token)))
    } else {
        None
    };

    // After registering, the provider only sends a `UdpCandidates` message if it
    // opted into the direct-path mode; otherwise it sends nothing. We heartbeat
    // to detect a dead provider, watch for its candidates, forward any consumer
    // offer to it as a `UdpPunch`, and add joined carrier connections to the pool.
    let mut offers: Option<mpsc::Receiver<UdpOffer>> = None;
    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Liveness deadline: reaped if no client frame arrives within `ctrl_timeout`.
    // Checked on every heartbeat tick (≤ HEARTBEAT_INTERVAL granularity) rather
    // than via `timeout(recv)` — the latter would reset every time the heartbeat
    // branch wins the `select!`, so it could never reach `ctrl_timeout`.
    let mut last_recv = TokioInstant::now();
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if control.send(ServerMessage::Heartbeat).await.is_err() {
                    return Ok(());
                }
                if last_recv.elapsed() >= ctrl_timeout {
                    warn!(%id, timeout = ?ctrl_timeout,
                        "secret provider control idle; reaping (peer wedged/abandoned)");
                    return Ok(());
                }
            }
            message = control.recv() => {
                last_recv = TokioInstant::now();
                match message? {
                    // Liveness ping; the deadline reset above is its only effect.
                    Some(ClientMessage::Heartbeat) => {}
                    Some(ClientMessage::UdpCandidates(candidates)) => {
                        register_provider_udp_offer(
                            &udp_registry,
                            &id,
                            &admin_reg,
                            &mut offers,
                            udp_offer_from_legacy(candidates),
                        );
                    }
                    Some(ClientMessage::UdpCandidateOffer(offer)) => {
                        register_provider_udp_offer(
                            &udp_registry,
                            &id,
                            &admin_reg,
                            &mut offers,
                            offer,
                        );
                    }
                    Some(ClientMessage::UdpStunHintRequest) => {
                        warn!(%id, "unexpected udp stun hint request from provider")
                    }
                    Some(_) => warn!(%id, "unexpected message from provider"),
                    None => return Ok(()),
                }
            }
            offer = recv_offer(&mut offers) => {
                let msg = ServerMessage::UdpPunch {
                    nonce: offer.nonce,
                    peer: offer.peer_candidates,
                    peer_selected_stun: offer.peer_selected_stun,
                    tuning: udp_tuning,
                    peer_id: 0,
                };
                if control.send(msg).await.is_err() {
                    return Ok(());
                }
            }
            joined = pool::recv_carrier(carrier_rx.as_mut()) => {
                if let Some(carrier) = joined {
                    if pool.push(carrier, effective as usize) {
                        info!(%id, size = pool.len(), "provider carrier joined pool");
                    }
                }
            }
        }
    }
}

/// Await the next consumer offer, or stay pending when no UDP channel exists yet
/// (so it never resolves and the `select!` waits on the other branches).
async fn recv_offer(offers: &mut Option<mpsc::Receiver<UdpOffer>>) -> UdpOffer {
    match offers {
        Some(rx) => match rx.recv().await {
            Some(offer) => offer,
            None => pending().await,
        },
        None => pending().await,
    }
}

/// Server side: relay every substream the consumer opens to the provider
/// registered under `id`. No port is bound; the server is a pure substream relay.
#[allow(clippy::too_many_arguments)]
pub async fn serve_consumer(
    mut control: Delimited<mux::Stream>,
    mut acceptor: mux::Acceptor,
    registry: Registry,
    udp_registry: UdpRegistry,
    permits: Arc<Semaphore>,
    id: String,
    admin: AdminRegistry,
    peer: SocketAddr,
    notes: Option<String>,
    udp_tuning: UdpDirectTuning,
    grx: Arc<std::sync::atomic::AtomicU64>,
    gtx: Arc<std::sync::atomic::AtomicU64>,
    display: SecretDisplay,
    ctrl_timeout: Duration,
    carrier: bool,
) -> Result<()> {
    if carrier {
        debug!(%id, %peer, "secret consumer relay carrier connected (no admin entry, not reaped — liveness is owned by the consumer's main control connection)");
    } else {
        info!(%id, %peer, "secret consumer connected");
    }
    control.send(ServerMessage::Ok).await?;

    // A primary consumer registers a live admin entry (removed when it disconnects).
    // An extra relay *carrier* (`--carriers N` opens N-1 of these) registers NOTHING
    // and is never reaped (I-2/I-3): its data substreams are still accepted and
    // relayed below, but it owns no admin row and its liveness is the main control
    // connection's responsibility. The `carrier == false` path stays byte-identical
    // to the previous behaviour (I-1).
    let admin_reg = if carrier {
        None
    } else {
        Some(admin.register(display.fill(NewEntry {
            role: Role::SecretConsumer,
            peer,
            secret_id: Some(id.clone()),
            public_port: None,
            notes,
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
        })))
    };
    let active = admin_reg
        .as_ref()
        .map(|r| r.active())
        .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)));
    // Per-tunnel relay byte counters for this consumer (shown on the admin page),
    // summed off the hot path once per closed relayed substream. A carrier owns no
    // entry, so it gets throwaway counters here; its bytes still count toward the
    // global server totals inside `relay()`.
    let (relay_tx, relay_rx) = admin_reg
        .as_ref()
        .map(|r| r.relay_bytes())
        .unwrap_or_else(|| {
            (
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
            )
        });

    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Liveness deadline: reaped if no client frame arrives within `ctrl_timeout`.
    // Checked on the heartbeat tick, not via `timeout(recv)` (which the heartbeat
    // branch would reset every iteration so it never reaches `ctrl_timeout`).
    let mut last_recv = TokioInstant::now();
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if control.send(ServerMessage::Heartbeat).await.is_err() {
                    return Ok(());
                }
                // Carriers are never reaped (they send no heartbeats by design —
                // their liveness is the main control connection's job, I-2).
                if !carrier && last_recv.elapsed() >= ctrl_timeout {
                    warn!(%id, %peer, timeout = ?ctrl_timeout,
                        "secret consumer control idle; reaping (peer wedged/abandoned)");
                    return Ok(());
                }
            }
            // A direct-path consumer offers its candidates here; broker them to
            // the registered provider (if it is UDP-capable) and reply with the
            // provider's candidates + a shared nonce, else say it is unavailable.
            message = control.recv() => {
                last_recv = TokioInstant::now();
                match message? {
                    // Liveness ping; the deadline reset above is its only effect.
                    Some(ClientMessage::Heartbeat) => {}
                    Some(ClientMessage::UdpCandidates(consumer_cands)) => {
                        // Flag for the admin page that this consumer attempted a
                        // direct path (success is established peer-to-peer, off the
                        // server, so the relay can only record the attempt).
                        if let Some(reg) = &admin_reg {
                            reg.mark_udp();
                        }
                        broker_udp(
                            &mut control,
                            &udp_registry,
                            &id,
                            udp_offer_from_legacy(consumer_cands),
                            udp_tuning,
                        )
                        .await?;
                    }
                    Some(ClientMessage::UdpCandidateOffer(consumer_offer)) => {
                        if let Some(reg) = &admin_reg {
                            reg.mark_udp();
                        }
                        broker_udp(&mut control, &udp_registry, &id, consumer_offer, udp_tuning)
                            .await?;
                    }
                    Some(ClientMessage::UdpStunHintRequest) => {
                        let stun_server = provider_stun_hint(&udp_registry, &id);
                        info!(
                            %id,
                            provider_selected_stun = ?stun_server,
                            "consumer requested provider stun hint"
                        );
                        control.send(ServerMessage::UdpStunHint { stun_server }).await?;
                    }
                    Some(_) => warn!(%id, "unexpected message from consumer"),
                    None => return Ok(()),
                }
            }
            inbound = acceptor.accept() => {
                let Some(consumer_stream) = inbound else {
                    return Ok(());
                };
                // Bound concurrently relayed connections; drop excess under a flood.
                let permit = match Arc::clone(&permits).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        warn!(%id, "too many active connections, dropping");
                        continue;
                    }
                };
                let registry = registry.clone();
                let id = id.clone();
                let active = Arc::clone(&active);
                let grx = Arc::clone(&grx);
                let gtx = Arc::clone(&gtx);
                let erx = Arc::clone(&relay_rx);
                let etx = Arc::clone(&relay_tx);
                tokio::spawn(
                    async move {
                        let _permit = permit;
                        let _active = ActiveGuard::new(active);
                        if let Err(err) = relay(consumer_stream, registry, &id, grx, gtx, erx, etx).await {
                            trace!(%err, "secret relay closed");
                        }
                    }
                    .instrument(info_span!("relay")),
                );
            }
        }
    }
}

/// Broker a UDP direct path: look up the provider, mint a shared nonce, tell the
/// provider to punch toward the consumer, and reply to the consumer with the
/// provider's candidates. Replies `UdpUnavailable` if no UDP-capable provider is
/// registered, so the consumer falls back to the relay.
async fn broker_udp(
    control: &mut Delimited<mux::Stream>,
    udp_registry: &UdpRegistry,
    id: &str,
    consumer_offer: UdpCandidateOffer,
    tuning: UdpDirectTuning,
) -> Result<()> {
    // Clone out so no DashMap guard is held across an await point.
    let provider = udp_registry.get(id).map(|e| {
        (
            e.candidates.clone(),
            e.selected_stun.clone(),
            e.nonce,
            e.to_provider.clone(),
        )
    });
    let Some((provider_cands, provider_selected_stun, nonce, to_provider)) = provider else {
        info!(%id, "no udp-capable provider; consumer will use relay");
        control.send(ServerMessage::UdpUnavailable).await?;
        return Ok(());
    };

    let stun_aligned =
        provider_selected_stun.is_some() && provider_selected_stun == consumer_offer.selected_stun;
    info!(
        %id,
        provider_candidates = ?provider_cands,
        provider_selected_stun = ?provider_selected_stun,
        consumer_candidates = ?consumer_offer.candidates,
        consumer_selected_stun = ?consumer_offer.selected_stun,
        stun_aligned,
        "brokering udp direct path"
    );
    // Tell the provider first so its QUIC listener is up before the consumer dials.
    let offer = UdpOffer {
        nonce,
        peer_candidates: consumer_offer.candidates,
        peer_selected_stun: consumer_offer.selected_stun,
    };
    if to_provider.send(offer).await.is_err() {
        warn!(
            %id,
            "provider task vanished during UDP brokering — provider connection \
             closed between candidate offer and punch; consumer falls back to relay"
        );
        control.send(ServerMessage::UdpUnavailable).await?;
        return Ok(());
    }
    info!(%id, "brokered udp direct path (consumer told to punch)");
    control
        .send(ServerMessage::UdpPunch {
            nonce,
            peer: provider_cands,
            peer_selected_stun: provider_selected_stun,
            tuning,
            peer_id: 0,
        })
        .await?;
    Ok(())
}

/// Splice one consumer substream to a freshly opened provider substream.
async fn relay(
    mut consumer: mux::Stream,
    registry: Registry,
    id: &str,
    grx: Arc<std::sync::atomic::AtomicU64>,
    gtx: Arc<std::sync::atomic::AtomicU64>,
    erx: Arc<std::sync::atomic::AtomicU64>,
    etx: Arc<std::sync::atomic::AtomicU64>,
) -> Result<()> {
    // Consume the consumer's readiness marker (it announced the substream).
    let mut marker = [0u8; 1];
    consumer.read_exact(&mut marker).await?;

    // Clone the pool handle out so no DashMap guard is held across an await point,
    // then round-robin to a live provider carrier.
    let pool = match registry.get(id).map(|entry| Arc::clone(entry.value())) {
        Some(pool) => pool,
        None => bail!("no provider registered for '{id}'"),
    };
    // Fail over across live provider carriers (D5): a carrier can die between
    // `pick` (which prunes dead ones under the lock) and `open` (the connection
    // breaking is observed asynchronously), so try up to the pool size before
    // dropping this forwarded connection. `pick` round-robins, so successive
    // attempts hit different carriers.
    let attempts = pool.len().max(1);
    let mut provider = None;
    let mut last_err = None;
    for _ in 0..attempts {
        let Some(opener) = pool.pick() else { break };
        match opener.open().await {
            Ok(stream) => {
                provider = Some(stream);
                break;
            }
            Err(err) => {
                trace!(%id, %err, "provider carrier open failed; trying next live carrier");
                last_err = Some(err);
            }
        }
    }
    let mut provider = match provider {
        Some(stream) => stream,
        None => match last_err {
            Some(err) => return Err(err).context("all provider carriers unavailable"),
            None => bail!("no live provider carrier"),
        },
    };
    provider.write_all(&[mux::STREAM_READY]).await?;

    let buf = proxy_buffer_size();
    // Count bytes LIVE as they flow (not only on close) so the admin secret-tunnel
    // TX/RX columns update while the connection is open. `rx`/`tx` map to the
    // (consumer→provider, provider→consumer) directions `copy_bidirectional`
    // would have returned.
    let mut counted = crate::shared::CountingStream::new(consumer, erx, etx, grx, gtx);
    tokio::io::copy_bidirectional_with_sizes(&mut counted, &mut provider, buf, buf).await?;
    Ok(())
}

/// The consumer's data path for forwarding a proxied connection: either the server
/// relay (a pool of one or more connections, round-robined to avoid single-TCP
/// head-of-line blocking) or a direct UDP path (native QUIC streams, one per
/// proxied connection).
enum DataPath {
    /// Server relay: round-robin a substream opener from the carrier pool.
    Relay(Arc<CarrierPool>),
    /// Direct UDP path: open a native QUIC stream per proxied connection.
    #[cfg(feature = "udp")]
    Direct(crate::holepunch::DirectConn),
}

impl DataPath {
    /// A per-connection handle used to open one forwarded stream. Cloned out so the
    /// open + splice can run in a spawned task without borrowing the path.
    fn opener(&self) -> Result<StreamOpener> {
        match self {
            DataPath::Relay(pool) => Ok(StreamOpener::Mux(
                pool.pick().context("no live relay carrier")?,
            )),
            #[cfg(feature = "udp")]
            DataPath::Direct(conn) => Ok(StreamOpener::Direct(conn.clone())),
        }
    }
}

/// A per-connection handle to open one forwarded stream (relay substream or direct
/// QUIC stream).
#[derive(Clone)]
enum StreamOpener {
    Mux(mux::Opener),
    #[cfg(feature = "udp")]
    Direct(crate::holepunch::DirectConn),
}

impl StreamOpener {
    async fn open(self) -> Result<DataStream> {
        match self {
            StreamOpener::Mux(opener) => Ok(DataStream::Mux(
                opener
                    .open()
                    .await
                    .context("failed to open stream to server")?,
            )),
            #[cfg(feature = "udp")]
            StreamOpener::Direct(conn) => Ok(DataStream::Quic(conn.open_stream().await?)),
        }
    }
}

/// One forwarded stream — a yamux substream (relay) or a native QUIC bidi (direct).
/// Both are `Unpin`, so the trait impls delegate by matching on `&mut self`.
enum DataStream {
    Mux(mux::Stream),
    #[cfg(feature = "udp")]
    Quic(crate::holepunch::QuicTransport),
}

impl AsyncRead for DataStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            DataStream::Mux(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "udp")]
            DataStream::Quic(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for DataStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            DataStream::Mux(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "udp")]
            DataStream::Quic(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            DataStream::Mux(s) => Pin::new(s).poll_flush(cx),
            #[cfg(feature = "udp")]
            DataStream::Quic(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            DataStream::Mux(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "udp")]
            DataStream::Quic(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Client side of a secret tunnel consumer (`bore proxy`): binds a local listener
/// and forwards each accepted connection to the provider through the server.
pub struct Proxy {
    control: Delimited<mux::Stream>,
    /// Current data path: the server relay (a carrier pool of one or more
    /// connections) or, after a successful negotiation, the direct UDP path.
    data_path: DataPath,
    listener: TcpListener,
    /// Whether data flows over a direct UDP path rather than the server relay.
    direct: bool,
    /// Resolves once when the direct QUIC connection closes (provider restart /
    /// QUIC idle timeout), tearing the proxy down so it re-negotiates. `None` on
    /// the relay path; set when the direct path comes up (at startup or on upgrade).
    direct_closed_rx: Option<oneshot::Receiver<()>>,
    /// Whether the direct UDP path was requested (`--udp`). When set and we are
    /// currently on the relay, `listen` periodically retries the direct path and
    /// upgrades to it without dropping the session.
    #[cfg(feature = "udp")]
    udp: bool,
    /// Control endpoint, retained to re-negotiate a direct path for the upgrade.
    #[cfg(feature = "udp")]
    endpoint: Endpoint,
    /// Tunnel secret, retained to derive the direct-path token on upgrade.
    #[cfg(feature = "udp")]
    secret: Option<String>,
    /// Explicit STUN server, retained for the upgrade negotiation.
    #[cfg(feature = "udp")]
    stun_server: Option<String>,
    /// Whether to attempt UPnP-IGD router port mapping during (re)negotiation.
    #[cfg(feature = "udp")]
    port_map: bool,
    /// Whether to advertise predicted symmetric-NAT ports during (re)negotiation.
    #[cfg(feature = "udp")]
    port_prediction: bool,
    /// Fixed UDP source port for hole-punching (0 = ephemeral), retained so the
    /// upgrade re-negotiation binds the same port.
    #[cfg(feature = "udp")]
    udp_port: u16,
    /// Seconds before re-checking if the preferred UDP port was released by NAT.
    /// 0 = disable port-release detection.
    #[cfg(feature = "udp")]
    nat_udp_release_timeout: Duration,
    /// Capped exponential backoff for relay→direct upgrade retries.
    /// Sequence: 2, 4, 8, 16, … up to 256 s, then every ~4.3 min.
    #[cfg(feature = "udp")]
    upgrade_backoff: reconnect::Backoff,
    /// Monotonically increasing attempt counter for upgrade retry logs.
    #[cfg(feature = "udp")]
    upgrade_attempt: u64,
}

/// Initial delay (s) for the UDP upgrade exponential backoff: 2, 4, 8, 16, …
#[cfg(feature = "udp")]
const UDP_UPGRADE_INITIAL_SECS: u64 = 2;

/// Maximum delay (s) for the UDP upgrade backoff — retry at most every ~4.3 min.
#[cfg(feature = "udp")]
const UDP_UPGRADE_MAX_SECS: u64 = 256;

impl Proxy {
    /// Connect to the server, register as a consumer of `tcp_secret_id`, and bind
    /// the local proxy listener.
    #[allow(clippy::too_many_arguments)]
    // Without the `udp` feature the data path never upgrades, so these stay `Relay`.
    #[cfg_attr(not(feature = "udp"), allow(unused_mut))]
    pub async fn new(
        to: &str,
        bind_addr: SocketAddr,
        tcp_secret_id: &str,
        secret: Option<&str>,
        insecure: bool,
        udp: bool,
        stun_server: Option<&str>,
        port_map: bool,
        port_prediction: bool,
        udp_port: u16,
        nat_udp_release_timeout: u64,
        carriers: u16,
        notes: Option<String>,
        auto_reconnect: bool,
    ) -> Result<Self> {
        #[cfg(not(feature = "udp"))]
        let _ = nat_udp_release_timeout;
        let endpoint = Endpoint::parse(to);
        let socket = transport::connect(&endpoint, insecure).await?;
        let (opener, _acceptor) = mux::client(socket);
        let mut control = Delimited::with_label(
            opener
                .open()
                .await
                .context("failed to open control stream")?,
            "proxy/consumer",
        );

        // Send the registration first so the lazily-opened substream is announced
        // before the (server-initiated) auth handshake. The note rides along for
        // the admin status page.
        control
            .send(ClientMessage::ConnectSecret {
                id: tcp_secret_id.to_string(),
                notes,
                carriers,
                auto_reconnect,
                udp,
                local_proxy_port: bind_addr.port(),
                nat_udp_preferred_port: udp_port,
                nat_udp_release_timeout,
                stun_server: stun_server.map(|s| s.to_string()),
                upnp: port_map,
                try_port_prediction: port_prediction,
                carrier: false,
            })
            .await?;
        if let Some(secret) = secret {
            Authenticator::new(secret)
                .client_handshake(&mut control)
                .await?;
        }
        match control.recv_timeout().await? {
            Some(ServerMessage::Ok) => {}
            Some(ServerMessage::Error(message)) => bail!("server error: {message}"),
            Some(ServerMessage::Challenge(_)) => {
                bail!("server requires authentication, but no client secret was provided");
            }
            Some(_) => bail!("unexpected response to secret connect"),
            None => bail!("unexpected EOF"),
        }

        // The relay carrier pool seeded with this (main) connection's opener.
        let pool = Arc::new(CarrierPool::new(opener));
        let mut data_path = DataPath::Relay(Arc::clone(&pool));
        let mut direct = false;
        let mut direct_closed_rx = None;

        // Optionally negotiate a direct UDP path; on any failure keep the relay so
        // the tunnel still works through the server.
        if udp {
            #[cfg(feature = "udp")]
            if secret.is_none() {
                warn!(
                    "--udp without --secret: the direct-path token derives from an empty key, so \
                     its security rests only on the (random) server nonce and the control channel. \
                     Pass --secret for a strong token."
                );
            }
            match negotiate_direct_consumer(
                &mut control,
                &endpoint,
                secret,
                stun_server,
                port_map,
                port_prediction,
                udp_port,
            )
            .await
            {
                #[cfg(feature = "udp")]
                Ok(Some(conn)) => {
                    info!(%tcp_secret_id, "using direct udp path");
                    // The direct path uses a single QUIC connection (one stream per
                    // proxied connection); `--carriers` only governs the relay
                    // fallback. Warn rather than silently ignore the flag (D8).
                    if carriers > 1 {
                        warn!(
                            %tcp_secret_id, carriers,
                            "secret --udp direct path uses a single QUIC connection; \
                             --carriers applies only to the relay fallback \
                             (multi-connection direct is not supported)"
                        );
                    }
                    direct_closed_rx = Some(spawn_closed_monitor(conn.clone()));
                    data_path = DataPath::Direct(conn);
                    direct = true;
                }
                // Unreachable without the `udp` feature (no direct path exists).
                #[cfg(not(feature = "udp"))]
                Ok(Some(_)) => {}
                Ok(None) => info!(%tcp_secret_id, "udp unavailable, using relay"),
                Err(err) => warn!(%err, "udp negotiation failed, using relay"),
            }
        }

        // On the relay path with `--carriers > 1`, open extra connections and add
        // their openers to the pool so the consumer's forwarded substreams spread
        // across several TCP connections (avoiding single-connection HOL). Each
        // extra is drained of heartbeats and pruned when it drops.
        if matches!(data_path, DataPath::Relay(_)) && carriers > 1 {
            for _ in 1..carriers {
                if let Err(err) = open_consumer_carrier(
                    &endpoint,
                    insecure,
                    secret,
                    tcp_secret_id,
                    &pool,
                    carriers,
                )
                .await
                {
                    warn!(%err, "failed to open extra relay carrier");
                }
            }
            let opened = pool.len();
            if (opened as u16) < carriers {
                warn!(
                    %tcp_secret_id, requested = carriers, opened,
                    "consumer relay carrier pool degraded: fewer carriers connected than requested"
                );
            } else {
                info!(
                    %tcp_secret_id, requested = carriers, opened,
                    "consumer relay carrier pool established"
                );
            }
        }

        let listener = TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("failed to bind {bind_addr}"))?;
        info!(%tcp_secret_id, "proxying {bind_addr} to secret tunnel");

        Ok(Proxy {
            control,
            data_path,
            listener,
            direct,
            direct_closed_rx,
            #[cfg(feature = "udp")]
            udp,
            #[cfg(feature = "udp")]
            endpoint,
            #[cfg(feature = "udp")]
            secret: secret.map(str::to_string),
            #[cfg(feature = "udp")]
            stun_server: stun_server.map(str::to_string),
            #[cfg(feature = "udp")]
            port_map,
            #[cfg(feature = "udp")]
            port_prediction,
            #[cfg(feature = "udp")]
            udp_port,
            #[cfg(feature = "udp")]
            nat_udp_release_timeout: Duration::from_secs(nat_udp_release_timeout),
            #[cfg(feature = "udp")]
            upgrade_backoff: reconnect::Backoff::new_with(
                UDP_UPGRADE_INITIAL_SECS,
                UDP_UPGRADE_MAX_SECS,
            ),
            #[cfg(feature = "udp")]
            upgrade_attempt: 0,
        })
    }

    /// Returns the local address the proxy is listening on.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Whether the proxy negotiated a direct UDP path (vs. the server relay).
    pub fn is_direct(&self) -> bool {
        self.direct
    }

    /// Start forwarding: accept local connections, relay each to the provider.
    #[cfg_attr(not(feature = "udp"), allow(unused_mut))]
    pub async fn listen(self) -> Result<()> {
        let Proxy {
            mut control,
            mut data_path,
            listener,
            mut direct,
            mut direct_closed_rx,
            #[cfg(feature = "udp")]
            udp,
            #[cfg(feature = "udp")]
            endpoint,
            #[cfg(feature = "udp")]
            secret,
            #[cfg(feature = "udp")]
            stun_server,
            #[cfg(feature = "udp")]
            port_map,
            #[cfg(feature = "udp")]
            port_prediction,
            #[cfg(feature = "udp")]
            udp_port,
            #[cfg(feature = "udp")]
            mut upgrade_backoff,
            #[cfg(feature = "udp")]
            mut upgrade_attempt,
            #[cfg(feature = "udp")]
            nat_udp_release_timeout,
        } = self;
        let mut path = if direct { "direct-udp" } else { "relay" };
        #[cfg(feature = "udp")]
        let mut last_upgrade = tokio::time::Instant::now();
        #[cfg(not(feature = "udp"))]
        let _last_upgrade = tokio::time::Instant::now();
        // Port-release detection: when the preferred port was remapped by NAT,
        // switch to ephemeral probes and periodically re-check the preferred port.
        #[cfg(feature = "udp")]
        let mut preferred_port_remapped = false;
        #[cfg(not(feature = "udp"))]
        let mut preferred_port_remapped = false;
        // Pre-resolve one STUN target for the lightweight port-release check.
        #[cfg(feature = "udp")]
        let check_stun_addr = crate::holepunch::resolve_live_stun_targets(
            &endpoint.host,
            endpoint.port,
            stun_server.as_deref(),
        )
        .await
        .ok()
        .and_then(|t| t.into_iter().next().map(|t| t.addr));
        #[cfg(not(feature = "udp"))]
        let _check_stun_addr: Option<SocketAddr> = None;
        // Relay → direct upgrade state. The slow work (STUN gather, punch, QUIC
        // dial) runs in a spawned `upgrade_task` so the accept/forward loop never
        // stalls; this loop only does the quick control I/O. The receivers are
        // separate locals (not one struct) so the `select!` arms borrow disjoint
        // fields, and are declared unconditionally because `tokio::select!` does
        // not allow `#[cfg]` on its branches. An attempt is "in flight" exactly
        // while `nego_done_rx` is `Some`. Without the `udp` feature nothing ever
        // sets them, so the arms stay dormant.
        #[cfg(feature = "udp")]
        let mut waiting_for_stun_hint = false;
        #[cfg(not(feature = "udp"))]
        let _waiting_for_stun_hint = false;
        #[cfg(feature = "udp")]
        let mut effective_udp_port = udp_port;
        #[cfg(not(feature = "udp"))]
        let _effective_udp_port = 0u16;
        let mut nego_cand_rx: Option<oneshot::Receiver<UdpCandidateOffer>> = None;
        #[allow(clippy::type_complexity)]
        let mut nego_punch_tx: Option<oneshot::Sender<UdpPunchResult>> = None;
        let mut nego_done_rx: Option<oneshot::Receiver<DirectUpgrade>> = None;
        // Periodic timer that fires every nat_udp_release_timeout to re-check
        // whether the preferred port has been released by the NAT.
        #[cfg(feature = "udp")]
        let mut release_check = {
            let mut t = tokio::time::interval(nat_udp_release_timeout.max(Duration::from_secs(1)));
            t.set_missed_tick_behavior(MissedTickBehavior::Delay);
            t
        };
        #[cfg(not(feature = "udp"))]
        let mut release_check = {
            let mut t = tokio::time::interval(Duration::from_secs(3600));
            t.set_missed_tick_behavior(MissedTickBehavior::Delay);
            t
        };
        #[cfg(not(feature = "udp"))]
        let udp_port = 0u16;
        #[cfg(not(feature = "udp"))]
        let nat_udp_release_timeout = Duration::ZERO;
        #[cfg(not(feature = "udp"))]
        let mut upgrade_backoff = reconnect::Backoff::new_with(1, 1);
        #[cfg(not(feature = "udp"))]
        let mut upgrade_attempt = 0;
        // Periodic liveness ping so the server's recv-deadline reaper never trips
        // on a healthy idle consumer (a yamux substream hides a half-open peer).
        let mut ctrl_heartbeat = {
            let mut t = tokio::time::interval(CTRL_CLIENT_HEARTBEAT);
            t.set_missed_tick_behavior(MissedTickBehavior::Delay);
            t
        };
        loop {
            // Kick off an upgrade attempt on the timer. Non-blocking: the attempt
            // runs in `upgrade_task`; this loop keeps accepting and forwarding.
            #[cfg(feature = "udp")]
            if udp
                && !direct
                && nego_done_rx.is_none()
                && !waiting_for_stun_hint
                && last_upgrade.elapsed() >= upgrade_backoff.peek()
            {
                last_upgrade = tokio::time::Instant::now();
                upgrade_attempt += 1;
                let backoff_delay = upgrade_backoff.next_delay();
                // When the preferred port was remapped, use ephemeral so the
                // NAT entry for the preferred port expires naturally.
                effective_udp_port = if preferred_port_remapped { 0 } else { udp_port };
                if effective_udp_port != udp_port {
                    info!(
                        attempt = upgrade_attempt,
                        next_retry_s = backoff_delay.as_secs(),
                        "udp upgrade attempt #{upgrade_attempt}: preferred port :{udp_port} \
                         still REMAPPED on NAT, using ephemeral port to avoid refreshing \
                         the NAT entry; will re-check in {:?}",
                        nat_udp_release_timeout,
                    );
                } else {
                    info!(
                        attempt = upgrade_attempt,
                        next_retry_s = backoff_delay.as_secs(),
                        "starting udp upgrade attempt #{upgrade_attempt}; \
                         will retry in {}s on failure",
                        backoff_delay.as_secs(),
                    );
                }
                if control
                    .send(ClientMessage::UdpStunHintRequest)
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                waiting_for_stun_hint = true;
            }
            tokio::select! {
                // Drain control so server heartbeats are read; surfaces teardown.
                message = control.recv() => {
                    match message? {
                        Some(ServerMessage::Heartbeat) | Some(ServerMessage::Ok) => (),
                        Some(ServerMessage::Error(err)) => error!(%err, "server error"),
                        Some(ServerMessage::Hello(_)) => warn!("unexpected hello"),
                        Some(ServerMessage::CarrierToken { .. }) => warn!("unexpected carrier token"),
                        Some(ServerMessage::Challenge(_)) => warn!("unexpected challenge"),
                        Some(ServerMessage::VhostUdp { .. }) => warn!("unexpected vhost udp offer"),
                        // Deliver the brokered candidates to the in-flight upgrade
                        // task (which then punches + dials QUIC); else it is stray.
                        Some(ServerMessage::UdpPunch { nonce, peer, peer_selected_stun, tuning, peer_id: _ }) => match nego_punch_tx.take() {
                            Some(tx) => {
                                let _ = tx.send(Some((nonce, peer, peer_selected_stun, tuning)));
                            }
                            None => warn!("unexpected udp punch"),
                        },
                        Some(ServerMessage::UdpStunHint { stun_server: provider_stun_hint }) => {
                            #[cfg(feature = "udp")]
                            if waiting_for_stun_hint {
                                waiting_for_stun_hint = false;
                                info!(
                                    provider_selected_stun = ?provider_stun_hint,
                                    "consumer received provider stun hint for udp upgrade"
                                );
                                let (cand_rx, punch_tx, done_rx) = spawn_upgrade_attempt(
                                    endpoint.clone(),
                                    secret.clone(),
                                    stun_server.clone(),
                                    provider_stun_hint,
                                    port_map,
                                    port_prediction,
                                    effective_udp_port,
                                );
                                nego_cand_rx = Some(cand_rx);
                                nego_punch_tx = Some(punch_tx);
                                nego_done_rx = Some(done_rx);
                            } else {
                                warn!(?provider_stun_hint, "unexpected udp stun hint");
                            }
                            #[cfg(not(feature = "udp"))]
                            warn!(?provider_stun_hint, "unexpected udp stun hint");
                        }
                        Some(ServerMessage::UdpUnavailable) => match nego_punch_tx.take() {
                            // Provider not UDP-capable right now; tell the task to
                            // give up so it stops and we stay on the relay.
                            Some(tx) => {
                                let _ = tx.send(None);
                            }
                            None => warn!("unexpected udp unavailable"),
                        },
                        Some(ServerMessage::TestUdpWaiting) => warn!("unexpected udp diagnostic wait"),
                        Some(ServerMessage::TestUdpStart { .. }) => warn!("unexpected udp diagnostic start"),
                        Some(ServerMessage::PublicUdp { .. }) => warn!("unexpected public udp offer"),
                        Some(ServerMessage::VhostReady { .. }) => warn!("unexpected vhost ready"),
                        Some(ServerMessage::VpnReady { .. }) => warn!("unexpected vpn ready"),
                        Some(ServerMessage::VpnError(err)) => error!(%err, "vpn error"),
                        Some(ServerMessage::VpnPeerJoin { .. }) => warn!("unexpected vpn peer join in 1:1 mode"),
                        Some(ServerMessage::VpnPeerLeave { .. }) => warn!("unexpected vpn peer leave in 1:1 mode"),
                        None => return Ok(()),
                    }
                }
                // The upgrade task gathered its candidates: send them on control
                // (this loop owns `control`, so no shared-mutable conflict).
                offer = recv_opt(&mut nego_cand_rx) => {
                    match offer {
                        Some(offer) => {
                            // Detect NAT remap of the preferred port so we stop
                            // refreshing it and let the entry expire.
                            if !preferred_port_remapped
                                && udp_port != 0
                                && nat_udp_release_timeout.as_secs() > 0
                            {
                                if let Some(first) = offer.candidates.first() {
                                    if first.port() != udp_port {
                                        preferred_port_remapped = true;
                                        info!(
                                            port = udp_port,
                                            reflexive_port = first.port(),
                                            recheck_s = nat_udp_release_timeout.as_secs(),
                                            "port :{udp_port} was REMAPPED to :{reflexive_port} \
                                             by NAT; switching to ephemeral probes. \
                                             Will re-check in {:?}",
                                            nat_udp_release_timeout,
                                            reflexive_port = first.port(),
                                        );
                                    }
                                }
                            }
                            if control.send(ClientMessage::UdpCandidateOffer(offer)).await.is_err() {
                                return Ok(());
                            }
                        }
                        // Gather failed (task dropped the sender): abort this attempt.
                        None => {
                            let next_s = upgrade_backoff.peek().as_secs();
                            info!(
                                attempt = upgrade_attempt,
                                next_retry_s = next_s,
                                "udp upgrade candidate gathering failed; \
                                 will retry in {next_s}s"
                            );
                            nego_punch_tx = None;
                            nego_done_rx = None;
                        }
                    }
                }
                // The upgrade task established a direct path: swap to it in place.
                done = recv_opt(&mut nego_done_rx) => {
                    nego_cand_rx = None;
                    nego_punch_tx = None;
                    if done.is_none() {
                        let next_s = upgrade_backoff.peek().as_secs();
                        info!(
                            attempt = upgrade_attempt,
                            next_retry_s = next_s,
                            "udp upgrade attempt failed; will retry in {next_s}s"
                        );
                    }
                    if let Some(_conn) = done {
                        #[cfg(feature = "udp")]
                        {
                            info!("upgraded relay → direct udp path");
                            upgrade_backoff.reset();
                            upgrade_attempt = 0;
                            direct_closed_rx = Some(spawn_closed_monitor(_conn.clone()));
                            data_path = DataPath::Direct(_conn);
                            direct = true;
                            path = "direct-udp";
                        }
                    }
                }
                // Detect the direct UDP path dying (provider restart / QUIC close)
                // even while the server control channel stays up: tear down so
                // auto-reconnect re-negotiates a fresh path (direct or relay).
                _ = recv_opt(&mut direct_closed_rx) => {
                    warn!("direct udp path closed; reconnecting");
                    return Ok(());
                }
                // Periodic check: when the preferred port was remapped, try
                // to bind it again to see if the NAT has released it.
                _ = release_check.tick() => {
                    #[cfg(feature = "udp")]
                    if preferred_port_remapped && udp_port != 0
                        && nat_udp_release_timeout.as_secs() > 0
                    {
                        if let Some(ref stun_addr) = check_stun_addr {
                            match crate::holepunch::check_reflexive_port(
                                udp_port, *stun_addr,
                            ).await {
                                Some(true) => {
                                    info!(
                                        port = udp_port,
                                        "port :{udp_port} is now PRESERVED on NAT! \
                                         Scheduling immediate direct path upgrade",
                                    );
                                    preferred_port_remapped = false;
                                    upgrade_backoff.reset();
                                    effective_udp_port = udp_port;
                                    // Force an immediate upgrade attempt.
                                    last_upgrade = tokio::time::Instant::now()
                                        .checked_sub(
                                            upgrade_backoff.peek()
                                                + Duration::from_millis(100),
                                        )
                                        .unwrap_or(tokio::time::Instant::now());
                                }
                                Some(false) => {
                                    info!(
                                        port = udp_port,
                                        recheck_s = nat_udp_release_timeout.as_secs(),
                                        "port :{udp_port} still REMAPPED on NAT; \
                                         will re-check in {:?}",
                                        nat_udp_release_timeout,
                                    );
                                }
                                None => {
                                    debug!(
                                        port = udp_port,
                                        "STUN probe for preferred port :{udp_port} \
                                         failed (server unreachable); will retry",
                                    );
                                }
                            }
                        }
                    }
                }
                _ = ctrl_heartbeat.tick() => {
                    if control.send(ClientMessage::Heartbeat).await.is_err() {
                        return Ok(());
                    }
                }
                accepted = listener.accept() => {
                    let (local, addr) = match accepted {
                        Ok(pair) => pair,
                        Err(err) => {
                            warn!(%err, "failed to accept local connection");
                            continue;
                        }
                    };
                    tune_tcp(&local);
                    let opener = match data_path.opener() {
                        Ok(opener) => opener,
                        Err(err) => {
                            warn!(%err, "no data path available, dropping connection");
                            continue;
                        }
                    };
                    info!(?addr, %path, "forwarding local connection over secret tunnel");
                    tokio::spawn(
                        async move {
                            if let Err(err) = forward(local, opener).await {
                                warn!(%err, "proxy connection closed with error");
                            }
                        }
                        .instrument(info_span!("proxy", ?addr, %path)),
                    );
                }
            }
        }
    }
}

/// Spawn a task that resolves the returned receiver once the direct QUIC
/// connection closes (provider restart / idle timeout / graceful close), so the
/// listen loop can tear down and re-negotiate.
#[cfg(feature = "udp")]
fn spawn_closed_monitor(conn: crate::holepunch::DirectConn) -> oneshot::Receiver<()> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        conn.closed().await;
        let _ = tx.send(());
    });
    rx
}

/// Open one extra relay connection for the consumer's carrier pool: dial the
/// server, register as a consumer of `id`, and add its substream opener to `pool`.
/// A drain task reads (and discards) the server heartbeats so the connection stays
/// alive, and marks the carrier dead when it drops (so the pool prunes it).
async fn open_consumer_carrier(
    endpoint: &Endpoint,
    insecure: bool,
    secret: Option<&str>,
    id: &str,
    pool: &Arc<CarrierPool>,
    max: u16,
) -> Result<()> {
    let socket = transport::connect(endpoint, insecure).await?;
    let (opener, _acceptor) = mux::client(socket);
    let mut control = Delimited::with_label(
        opener
            .open()
            .await
            .context("failed to open carrier control stream")?,
        "server/secret/provider",
    );
    control
        .send(ClientMessage::ConnectSecret {
            id: id.to_string(),
            notes: None,
            carriers: 0,
            auto_reconnect: false,
            udp: false,
            local_proxy_port: 0,
            nat_udp_preferred_port: 0,
            nat_udp_release_timeout: 0,
            stun_server: None,
            upnp: false,
            try_port_prediction: false,
            // This is an extra relay carrier of an existing consumer: the server
            // must NOT register an admin entry for it and must NOT reap it (I-2).
            carrier: true,
        })
        .await?;
    if let Some(secret) = secret {
        Authenticator::new(secret)
            .client_handshake(&mut control)
            .await?;
    }
    match control.recv_timeout().await? {
        Some(ServerMessage::Ok) => {}
        Some(ServerMessage::Error(message)) => bail!("server error: {message}"),
        _ => bail!("unexpected response to carrier connect"),
    }
    let carrier = Carrier::new(opener);
    let alive = Arc::clone(&carrier.alive);
    if !pool.push(carrier, max as usize) {
        return Ok(()); // pool already at capacity
    }
    tokio::spawn(async move {
        // The server only sends heartbeats here; drain them so flow control does
        // not stall, and mark the carrier dead when the connection drops.
        while let Ok(Some(_)) = control.recv::<ServerMessage>().await {}
        alive.store(false, Ordering::Relaxed);
    });
    Ok(())
}

/// Resolve when an optional one-shot receiver completes, consuming it. Yields the
/// value (or `None` if the sender was dropped) and clears the slot; stays pending
/// forever when the slot is empty, so an idle `select!` arm never fires.
async fn recv_opt<T>(slot: &mut Option<oneshot::Receiver<T>>) -> Option<T> {
    use std::future::{poll_fn, Future};
    use std::pin::Pin;
    match slot {
        Some(rx) => {
            let res = poll_fn(|cx| Pin::new(&mut *rx).poll(cx)).await;
            *slot = None;
            res.ok()
        }
        None => pending().await,
    }
}

/// Bind a UDP socket and gather this consumer's candidates via STUN (no control
/// channel needed). Shared by the synchronous initial negotiation and the
/// background upgrade task.
#[cfg(feature = "udp")]
async fn gather_consumer_candidates(
    endpoint: &Endpoint,
    stun_server: Option<&str>,
    provider_stun_hint: Option<&str>,
    port_map: bool,
    port_prediction: bool,
    udp_port: u16,
) -> Result<(tokio::net::UdpSocket, UdpCandidateOffer)> {
    use crate::holepunch;
    let socket = holepunch::bind_socket(udp_port).await?;
    let local_addr = socket.local_addr().ok();
    let stun_chain = holepunch::live_stun_target_names_with_hint(
        &endpoint.host,
        endpoint.port,
        stun_server,
        provider_stun_hint,
    );
    info!(
        role = "consumer",
        udp_local_addr = ?local_addr,
        requested_udp_port = udp_port,
        stun_override = stun_server.is_some(),
        provider_stun_hint,
        stun_chain = ?stun_chain,
        "consumer UDP candidate discovery configured"
    );
    let stun_targets = match holepunch::resolve_live_stun_targets_with_hint(
        &endpoint.host,
        endpoint.port,
        stun_server,
        provider_stun_hint,
    )
    .await
    {
        Ok(targets) => targets,
        Err(err) => {
            warn!(%err, "no STUN targets resolved for consumer; offering non-STUN candidates only");
            Vec::new()
        }
    };
    let discovery = holepunch::gather_candidates_from_stun_targets(
        &socket,
        &stun_targets,
        port_map,
        port_prediction,
    )
    .await;
    let selected_stun = discovery.selected_stun.as_ref();
    let selected_stun_name = selected_stun.map(|s| s.requested.as_str());
    let selected_stun_owned = selected_stun.map(|s| s.requested.clone());
    let selected_stun_addr = selected_stun.map(|s| s.addr);
    let stun_source = selected_stun.map(|s| s.source.as_str());
    let reflexive = selected_stun.map(|s| s.reflexive);
    let discovery_local_addr = discovery.local_addr;
    let attempted_stun = discovery.attempted_stun;
    let candidates = discovery.candidates;
    if candidates.is_empty() {
        let stun_info = if attempted_stun == 0 {
            "no STUN targets resolved".to_string()
        } else if selected_stun.is_none() {
            format!("all {attempted_stun} STUN probes failed")
        } else {
            "STUN returned no addresses".to_string()
        };
        bail!(
            "no UDP candidates for consumer: {stun_info} \
             (port_map={port_map}, port_prediction={port_prediction}, \
             local_addr={}); direct path unavailable",
            discovery_local_addr
                .map(|a| a.to_string())
                .unwrap_or_default(),
        );
    }
    info!(
        role = "consumer",
        udp_local_addr = ?discovery_local_addr,
        provider_stun_hint,
        selected_stun = selected_stun_name,
        selected_stun_addr = ?selected_stun_addr,
        stun_source,
        reflexive = ?reflexive,
        attempted_stun,
        stun_aligned = provider_stun_hint.is_some() && provider_stun_hint == selected_stun_name,
        ?candidates,
        "consumer offering udp candidates"
    );
    Ok((
        socket,
        UdpCandidateOffer {
            candidates,
            selected_stun: selected_stun_owned,
            peer_id: 0,
        },
    ))
}

#[cfg(feature = "udp")]
async fn request_provider_stun_hint(
    control: &mut Delimited<mux::Stream>,
) -> Result<Option<String>> {
    control.send(ClientMessage::UdpStunHintRequest).await?;
    let hint = tokio::time::timeout(UDP_NEGOTIATE_TIMEOUT, async {
        loop {
            match control.recv().await? {
                Some(ServerMessage::UdpStunHint { stun_server }) => {
                    return Ok::<_, anyhow::Error>(stun_server);
                }
                Some(ServerMessage::Heartbeat) | Some(ServerMessage::Ok) => continue,
                Some(ServerMessage::Error(err)) => bail!("server error: {err}"),
                Some(other) => warn!(
                    ?other,
                    "unexpected response while waiting for udp stun hint"
                ),
                None => bail!("server closed during udp stun hint request"),
            }
        }
    })
    .await
    .context("timed out waiting for provider STUN hint")??;
    info!(provider_selected_stun = ?hint, "consumer received provider stun hint");
    Ok(hint)
}

/// Punch toward the brokered peer candidates and bring up the direct QUIC mux (no
/// control channel needed). Shared by the initial negotiation and the upgrade task.
#[cfg(feature = "udp")]
async fn finish_direct_consumer(
    socket: tokio::net::UdpSocket,
    secret: Option<&str>,
    nonce: [u8; UDP_NONCE_LEN],
    peer: Vec<SocketAddr>,
    tuning: UdpDirectTuning,
) -> Result<crate::holepunch::DirectConn> {
    use crate::holepunch;
    info!(peer_candidates = ?peer, "consumer received peer candidates, punching + connecting QUIC");
    let token = holepunch::derive_token(secret, &nonce);
    holepunch::connect_direct(socket, peer, token, tuning).await
}

/// Negotiate a direct UDP path as the consumer (QUIC client), synchronously.
/// Used at startup in [`Proxy::new`] (blocking is fine there: no service is live
/// yet). Returns the authenticated direct connection on success, or `None` for the
/// relay. The relay→direct upgrade uses [`upgrade_task`] instead so it never blocks
/// the forwarding loop.
#[cfg(feature = "udp")]
async fn negotiate_direct_consumer(
    control: &mut Delimited<mux::Stream>,
    endpoint: &Endpoint,
    secret: Option<&str>,
    stun_server: Option<&str>,
    port_map: bool,
    port_prediction: bool,
    udp_port: u16,
) -> Result<Option<crate::holepunch::DirectConn>> {
    let provider_stun_hint = request_provider_stun_hint(control).await?;
    let (socket, offer) = gather_consumer_candidates(
        endpoint,
        stun_server,
        provider_stun_hint.as_deref(),
        port_map,
        port_prediction,
        udp_port,
    )
    .await?;
    control
        .send(ClientMessage::UdpCandidateOffer(offer))
        .await?;

    // Await the server's brokering decision, draining heartbeats meanwhile.
    let outcome = tokio::time::timeout(UDP_NEGOTIATE_TIMEOUT, async {
        loop {
            match control.recv().await? {
                Some(ServerMessage::UdpPunch {
                    nonce,
                    peer,
                    peer_selected_stun,
                    tuning,
                    peer_id: _,
                }) => {
                    return Ok::<_, anyhow::Error>(Some((nonce, peer, peer_selected_stun, tuning)));
                }
                Some(ServerMessage::UdpUnavailable) => return Ok(None),
                Some(ServerMessage::Heartbeat) | Some(ServerMessage::Ok) => continue,
                Some(ServerMessage::Error(err)) => bail!("server error: {err}"),
                Some(_) => continue,
                None => bail!("unexpected EOF during udp negotiation"),
            }
        }
    })
    .await;
    let (nonce, peer, peer_selected_stun, tuning) = match outcome {
        Ok(Ok(Some(value))) => value,
        Ok(Ok(None)) => return Ok(None),
        Ok(Err(err)) => return Err(err),
        Err(_) => {
            warn!(
                timeout = ?UDP_NEGOTIATE_TIMEOUT,
                "direct udp negotiation timed out after {UDP_NEGOTIATE_TIMEOUT:?}; \
                 falling back to relay — no UdpPunch received within timeout. \
                 Check STUN server reachability and provider connectivity"
            );
            return Ok(None);
        }
    };
    info!(
        provider_selected_stun = ?peer_selected_stun,
        "consumer received provider metadata for udp negotiation"
    );
    Ok(Some(
        finish_direct_consumer(socket, secret, nonce, peer, tuning).await?,
    ))
}

/// Background relay→direct upgrade attempt. Runs the slow work (STUN gather,
/// punch, QUIC dial) off the forwarding loop. The control I/O is split with the
/// loop, which owns `control`: this task gathers candidates and hands them to the
/// loop via `cand_tx` (the loop sends them on control); the loop forwards the
/// brokered reply back via `punch_rx`; on success this task returns the direct
/// mux over `done_tx`. Dropping any sender signals "give up, stay on relay".
#[cfg(feature = "udp")]
type UdpPunchResult = Option<(
    [u8; UDP_NONCE_LEN],
    Vec<SocketAddr>,
    Option<String>,
    UdpDirectTuning,
)>;

#[cfg(not(feature = "udp"))]
type UdpPunchResult = Option<(
    [u8; UDP_NONCE_LEN],
    Vec<SocketAddr>,
    Option<String>,
    UdpDirectTuning,
)>;

#[cfg(feature = "udp")]
fn spawn_upgrade_attempt(
    endpoint: Endpoint,
    secret: Option<String>,
    stun_server: Option<String>,
    provider_stun_hint: Option<String>,
    port_map: bool,
    port_prediction: bool,
    udp_port: u16,
) -> (
    oneshot::Receiver<UdpCandidateOffer>,
    oneshot::Sender<UdpPunchResult>,
    oneshot::Receiver<crate::holepunch::DirectConn>,
) {
    let (cand_tx, cand_rx) = oneshot::channel();
    let (punch_tx, punch_rx) = oneshot::channel();
    let (done_tx, done_rx) = oneshot::channel();
    tokio::spawn(upgrade_task(
        endpoint,
        secret,
        stun_server,
        provider_stun_hint,
        port_map,
        port_prediction,
        udp_port,
        cand_tx,
        punch_rx,
        done_tx,
    ));
    (cand_rx, punch_tx, done_rx)
}

/// Background relay→direct upgrade attempt. Runs the slow work (STUN gather,
/// punch, QUIC dial) off the forwarding loop. The control I/O is split with the
/// loop, which owns `control`: this task gathers candidates and hands them to the
/// loop via `cand_tx` (the loop sends them on control); the loop forwards the
/// brokered reply back via `punch_rx`; on success this task returns the direct
/// mux over `done_tx`. Dropping any sender signals "give up, stay on relay".
#[cfg(feature = "udp")]
#[allow(clippy::too_many_arguments)]
async fn upgrade_task(
    endpoint: Endpoint,
    secret: Option<String>,
    stun_server: Option<String>,
    provider_stun_hint: Option<String>,
    port_map: bool,
    port_prediction: bool,
    udp_port: u16,
    cand_tx: oneshot::Sender<UdpCandidateOffer>,
    punch_rx: oneshot::Receiver<UdpPunchResult>,
    done_tx: oneshot::Sender<crate::holepunch::DirectConn>,
) {
    let (socket, candidates) = match gather_consumer_candidates(
        &endpoint,
        stun_server.as_deref(),
        provider_stun_hint.as_deref(),
        port_map,
        port_prediction,
        udp_port,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            warn!(
                %err,
                "udp upgrade attempt failed at candidate gathering phase; \
                 staying on relay, will retry on next backoff tick"
            );
            return;
        }
    };
    if cand_tx.send(candidates).is_err() {
        return; // loop gone
    }
    let (nonce, peer, peer_selected_stun, tuning) = match punch_rx.await {
        Ok(Some(v)) => v,
        _ => return, // unavailable / loop dropped the sender
    };
    info!(
        provider_selected_stun = ?peer_selected_stun,
        "consumer received provider metadata for udp upgrade"
    );
    match finish_direct_consumer(socket, secret.as_deref(), nonce, peer, tuning).await {
        Ok(pair) => {
            let _ = done_tx.send(pair);
        }
        Err(err) => warn!(%err, "udp upgrade punch/connect phase failed; staying on relay"),
    }
}

#[cfg(not(feature = "udp"))]
async fn negotiate_direct_consumer(
    _control: &mut Delimited<mux::Stream>,
    _endpoint: &Endpoint,
    _secret: Option<&str>,
    _stun_server: Option<&str>,
    _port_map: bool,
    _port_prediction: bool,
    _udp_port: u16,
) -> Result<Option<DirectUpgrade>> {
    warn!("built without the `udp` feature; ignoring direct-path request");
    Ok(None)
}

/// Forward one accepted local connection over a freshly opened data stream — a
/// relay substream or a direct native QUIC stream, depending on the path.
async fn forward(mut local: TcpStream, opener: StreamOpener) -> Result<()> {
    let mut stream = opener.open().await?;
    // Announce the stream so the peer routes it even if the local service waits
    // to speak first; the marker is consumed on the other end. Flush it so the
    // peer sees the stream promptly (a fresh QUIC stream is silent until flushed).
    stream.write_all(&[mux::STREAM_READY]).await?;
    stream.flush().await?;
    let buf = proxy_buffer_size();
    tokio::io::copy_bidirectional_with_sizes(&mut local, &mut stream, buf, buf).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_is_random_and_nonzero() {
        // CSPRNG-backed: successive nonces differ and are not all-zero.
        let a = new_nonce();
        let b = new_nonce();
        assert_ne!(a, b, "two nonces must differ");
        assert_ne!(a, [0u8; UDP_NONCE_LEN], "nonce must not be all-zero");
    }

    #[test]
    fn provider_stun_hint_reads_registered_metadata() {
        let udp_registry = UdpRegistry::default();
        let (to_provider, _rx) = mpsc::channel(1);
        udp_registry.insert(
            "svc".to_string(),
            UdpReg {
                candidates: vec!["127.0.0.1:3478".parse().unwrap()],
                selected_stun: Some("stun.cloudflare.com:3478".to_string()),
                nonce: [1u8; UDP_NONCE_LEN],
                to_provider,
            },
        );

        assert_eq!(
            provider_stun_hint(&udp_registry, "svc"),
            Some("stun.cloudflare.com:3478".to_string())
        );
        assert_eq!(provider_stun_hint(&udp_registry, "missing"), None);
    }
}
