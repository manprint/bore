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
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{error, info, info_span, trace, warn, Instrument};

use crate::auth::Authenticator;
use crate::mux;
use crate::shared::{
    tune_tcp, ClientMessage, Delimited, ServerMessage, PROXY_BUFFER_SIZE, UDP_NONCE_LEN,
};
use crate::transport::{self, Endpoint};

/// Heartbeat interval on secret-tunnel control substreams.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// How long a consumer waits for the server to broker a UDP direct path before
/// falling back to the relay.
#[cfg(feature = "udp")]
const UDP_NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(5);

/// Registry mapping each `tcp-secret-id` to the provider's substream opener.
pub type Registry = Arc<DashMap<String, mux::Opener>>;

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
}

/// Generate a fresh random session nonce.
fn new_nonce() -> [u8; UDP_NONCE_LEN] {
    let mut nonce = [0u8; UDP_NONCE_LEN];
    for b in nonce.iter_mut() {
        *b = fastrand::u8(..);
    }
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

/// Server side: register this connection as the provider for `id`, then keep it
/// alive with heartbeats until it disconnects (which deregisters it).
pub async fn serve_provider(
    mut control: Delimited<mux::Stream>,
    opener: mux::Opener,
    registry: Registry,
    udp_registry: UdpRegistry,
    id: String,
) -> Result<()> {
    // Register atomically, rejecting a duplicate id rather than hijacking it.
    match registry.entry(id.clone()) {
        Entry::Occupied(_) => {
            warn!(%id, "secret tunnel id already in use");
            let msg = format!("tcp-secret-id '{id}' already in use");
            control.send(ServerMessage::Error(msg)).await?;
            return Ok(());
        }
        Entry::Vacant(slot) => {
            slot.insert(opener);
        }
    }
    let _guard = Deregister {
        registry: registry.clone(),
        udp_registry: udp_registry.clone(),
        id: id.clone(),
    };
    info!(%id, "secret provider registered");
    control.send(ServerMessage::Ok).await?;

    // After registering, the provider only sends a `UdpCandidates` message if it
    // opted into the direct-path mode; otherwise it sends nothing. We heartbeat
    // to detect a dead provider, watch for its candidates, and forward any
    // consumer offer to it as a `UdpPunch` so it can punch back and accept a
    // direct connection.
    let mut offers: Option<mpsc::Receiver<UdpOffer>> = None;
    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if control.send(ServerMessage::Heartbeat).await.is_err() {
                    return Ok(());
                }
            }
            message = control.recv() => {
                match message? {
                    Some(ClientMessage::UdpCandidates(candidates)) => {
                        info!(%id, ?candidates, "provider offered udp candidates");
                        let (tx, rx) = mpsc::channel(4);
                        udp_registry.insert(
                            id.clone(),
                            UdpReg {
                                candidates,
                                nonce: new_nonce(),
                                to_provider: tx,
                            },
                        );
                        offers = Some(rx);
                    }
                    Some(_) => warn!(%id, "unexpected message from provider"),
                    None => return Ok(()),
                }
            }
            offer = recv_offer(&mut offers) => {
                let msg = ServerMessage::UdpPunch {
                    nonce: offer.nonce,
                    peer: offer.peer_candidates,
                };
                if control.send(msg).await.is_err() {
                    return Ok(());
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
pub async fn serve_consumer(
    mut control: Delimited<mux::Stream>,
    mut acceptor: mux::Acceptor,
    registry: Registry,
    udp_registry: UdpRegistry,
    permits: Arc<Semaphore>,
    id: String,
) -> Result<()> {
    info!(%id, "secret consumer connected");
    control.send(ServerMessage::Ok).await?;

    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if control.send(ServerMessage::Heartbeat).await.is_err() {
                    return Ok(());
                }
            }
            // A direct-path consumer offers its candidates here; broker them to
            // the registered provider (if it is UDP-capable) and reply with the
            // provider's candidates + a shared nonce, else say it is unavailable.
            message = control.recv() => {
                match message? {
                    Some(ClientMessage::UdpCandidates(consumer_cands)) => {
                        broker_udp(&mut control, &udp_registry, &id, consumer_cands).await?;
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
                tokio::spawn(
                    async move {
                        let _permit = permit;
                        if let Err(err) = relay(consumer_stream, registry, &id).await {
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
    consumer_cands: Vec<SocketAddr>,
) -> Result<()> {
    // Clone out so no DashMap guard is held across an await point.
    let provider = udp_registry
        .get(id)
        .map(|e| (e.candidates.clone(), e.nonce, e.to_provider.clone()));
    let Some((provider_cands, nonce, to_provider)) = provider else {
        info!(%id, "no udp-capable provider; consumer will use relay");
        control.send(ServerMessage::UdpUnavailable).await?;
        return Ok(());
    };

    info!(%id, ?provider_cands, ?consumer_cands, "brokering udp direct path");
    // Tell the provider first so its QUIC listener is up before the consumer dials.
    let offer = UdpOffer {
        nonce,
        peer_candidates: consumer_cands,
    };
    if to_provider.send(offer).await.is_err() {
        // Provider task is gone; fall back to relay.
        control.send(ServerMessage::UdpUnavailable).await?;
        return Ok(());
    }
    info!(%id, "brokered udp direct path (consumer told to punch)");
    control
        .send(ServerMessage::UdpPunch {
            nonce,
            peer: provider_cands,
        })
        .await?;
    Ok(())
}

/// Splice one consumer substream to a freshly opened provider substream.
async fn relay(mut consumer: mux::Stream, registry: Registry, id: &str) -> Result<()> {
    // Consume the consumer's readiness marker (it announced the substream).
    let mut marker = [0u8; 1];
    consumer.read_exact(&mut marker).await?;

    // Clone the opener out so no DashMap guard is held across an await point.
    let opener = match registry.get(id).map(|entry| entry.value().clone()) {
        Some(opener) => opener,
        None => bail!("no provider registered for '{id}'"),
    };
    let mut provider = opener.open().await.context("provider unavailable")?;
    provider.write_all(&[mux::STREAM_READY]).await?;

    tokio::io::copy_bidirectional_with_sizes(
        &mut consumer,
        &mut provider,
        PROXY_BUFFER_SIZE,
        PROXY_BUFFER_SIZE,
    )
    .await?;
    Ok(())
}

/// Client side of a secret tunnel consumer (`bore proxy`): binds a local listener
/// and forwards each accepted connection to the provider through the server.
pub struct Proxy {
    control: Delimited<mux::Stream>,
    opener: mux::Opener,
    listener: TcpListener,
    /// Whether data flows over a direct UDP path rather than the server relay.
    direct: bool,
    /// The direct path's mux acceptor, kept only to detect that path dying: it
    /// yields `None` when the QUIC connection to the provider closes (e.g. the
    /// provider restarted), which tears the proxy down so it re-negotiates.
    direct_acceptor: Option<mux::Acceptor>,
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
}

/// How often a relay-mode consumer retries the direct UDP path (so it upgrades
/// to direct as soon as the provider becomes reachable, without dropping).
#[cfg(feature = "udp")]
const UDP_UPGRADE_INTERVAL: Duration = Duration::from_secs(10);

impl Proxy {
    /// Connect to the server, register as a consumer of `tcp_secret_id`, and bind
    /// the local proxy listener.
    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<Self> {
        let endpoint = Endpoint::parse(to);
        let socket = transport::connect(&endpoint, insecure).await?;
        let (opener, _acceptor) = mux::client(socket);
        let mut control = Delimited::new(
            opener
                .open()
                .await
                .context("failed to open control stream")?,
        );

        // Send the registration first so the lazily-opened substream is announced
        // before the (server-initiated) auth handshake.
        control
            .send(ClientMessage::ConnectSecret(tcp_secret_id.to_string()))
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

        // Optionally negotiate a direct UDP path; on any failure keep the relay
        // opener so the tunnel still works through the server.
        let mut data_opener = opener;
        let mut direct = false;
        let mut direct_acceptor = None;
        if udp {
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
                Ok(Some((opener, acceptor))) => {
                    info!(%tcp_secret_id, "using direct udp path");
                    data_opener = opener;
                    direct = true;
                    direct_acceptor = Some(acceptor);
                }
                Ok(None) => info!(%tcp_secret_id, "udp unavailable, using relay"),
                Err(err) => warn!(%err, "udp negotiation failed, using relay"),
            }
        }

        let listener = TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("failed to bind {bind_addr}"))?;
        info!(%tcp_secret_id, "proxying {bind_addr} to secret tunnel");

        Ok(Proxy {
            control,
            opener: data_opener,
            listener,
            direct,
            direct_acceptor,
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
            mut opener,
            listener,
            mut direct,
            mut direct_acceptor,
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
        } = self;
        let mut path = if direct { "direct-udp" } else { "relay" };
        #[cfg(feature = "udp")]
        let mut last_upgrade = tokio::time::Instant::now();
        loop {
            // Relay → direct upgrade: while on the relay, periodically retry the
            // direct path and switch to it in place (no dropped session) as soon
            // as the provider becomes reachable. Runs outside the `select!` so it
            // has exclusive use of `control` (no double mutable borrow).
            #[cfg(feature = "udp")]
            if udp && !direct && last_upgrade.elapsed() >= UDP_UPGRADE_INTERVAL {
                last_upgrade = tokio::time::Instant::now();
                match negotiate_direct_consumer(
                    &mut control,
                    &endpoint,
                    secret.as_deref(),
                    stun_server.as_deref(),
                    port_map,
                    port_prediction,
                    udp_port,
                )
                .await
                {
                    Ok(Some((new_opener, new_acceptor))) => {
                        info!("upgraded relay → direct udp path");
                        opener = new_opener;
                        direct_acceptor = Some(new_acceptor);
                        direct = true;
                        path = "direct-udp";
                    }
                    Ok(None) => {} // provider still unreachable for udp; stay on relay
                    Err(err) => warn!(%err, "udp upgrade attempt failed; staying on relay"),
                }
            }

            tokio::select! {
                // Drain control so server heartbeats are read; surfaces teardown.
                message = control.recv() => {
                    match message? {
                        Some(ServerMessage::Heartbeat) | Some(ServerMessage::Ok) => (),
                        Some(ServerMessage::Error(err)) => error!(%err, "server error"),
                        Some(ServerMessage::Hello(_)) => warn!("unexpected hello"),
                        Some(ServerMessage::Challenge(_)) => warn!("unexpected challenge"),
                        Some(ServerMessage::UdpPunch { .. }) => warn!("unexpected udp punch"),
                        Some(ServerMessage::UdpUnavailable) => warn!("unexpected udp unavailable"),
                        None => return Ok(()),
                    }
                }
                // Detect the direct UDP path dying (provider restart / QUIC close)
                // even while the server control channel stays up: tear down so
                // auto-reconnect re-negotiates a fresh path (direct or relay).
                _ = direct_path_closed(&mut direct_acceptor) => {
                    warn!("direct udp path closed; reconnecting");
                    return Ok(());
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
                    let opener = opener.clone();
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

/// Resolve when the direct UDP path closes: the direct mux's acceptor yields
/// `None` once the QUIC connection to the provider is gone (or, unexpectedly, an
/// inbound substream). Stays pending forever in relay mode (no direct acceptor),
/// so the `select!` only watches the control channel there.
async fn direct_path_closed(acceptor: &mut Option<mux::Acceptor>) {
    match acceptor {
        Some(a) => {
            let _ = a.accept().await;
        }
        None => pending().await,
    }
}

/// Negotiate a direct UDP path as the consumer (QUIC client). Returns the direct
/// substream opener **and** acceptor (the latter only to detect the path dying)
/// on success, or `None` to fall back to the server relay.
#[cfg(feature = "udp")]
async fn negotiate_direct_consumer(
    control: &mut Delimited<mux::Stream>,
    endpoint: &Endpoint,
    secret: Option<&str>,
    stun_server: Option<&str>,
    port_map: bool,
    port_prediction: bool,
    udp_port: u16,
) -> Result<Option<(mux::Opener, mux::Acceptor)>> {
    use crate::holepunch;

    let stun = holepunch::resolve_stun(&endpoint.host, endpoint.port, stun_server).await?;
    let socket = holepunch::bind_socket(udp_port).await?;
    let candidates = holepunch::gather_candidates(&socket, stun, port_map, port_prediction).await;
    if candidates.is_empty() {
        bail!("no local UDP candidates discovered");
    }
    info!(?candidates, %stun, "consumer offering udp candidates (a public IP here means STUN worked)");
    control
        .send(ClientMessage::UdpCandidates(candidates))
        .await?;

    // Await the server's brokering decision, draining heartbeats meanwhile.
    let outcome = tokio::time::timeout(UDP_NEGOTIATE_TIMEOUT, async {
        loop {
            match control.recv().await? {
                Some(ServerMessage::UdpPunch { nonce, peer }) => {
                    return Ok::<_, anyhow::Error>(Some((nonce, peer)));
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
    let (nonce, peer) = match outcome {
        Ok(Ok(Some(value))) => value,
        Ok(Ok(None)) => return Ok(None),
        Ok(Err(err)) => return Err(err),
        Err(_) => return Ok(None), // negotiation timed out → relay
    };
    info!(peer_candidates = ?peer, "consumer received peer candidates, punching + connecting QUIC");

    let token = holepunch::derive_token(secret, &nonce);
    let quic = holepunch::connect_direct(socket, peer, token).await?;
    let (opener, acceptor) = mux::client(quic);
    Ok(Some((opener, acceptor)))
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
) -> Result<Option<(mux::Opener, mux::Acceptor)>> {
    warn!("built without the `udp` feature; ignoring direct-path request");
    Ok(None)
}

/// Forward one accepted local connection over a new substream to the server.
async fn forward(mut local: TcpStream, opener: mux::Opener) -> Result<()> {
    let mut stream = opener
        .open()
        .await
        .context("failed to open stream to server")?;
    // Announce the substream so the server routes it even if the local peer waits
    // for the service to speak first; the server consumes this marker byte.
    stream.write_all(&[mux::STREAM_READY]).await?;
    tokio::io::copy_bidirectional_with_sizes(
        &mut local,
        &mut stream,
        PROXY_BUFFER_SIZE,
        PROXY_BUFFER_SIZE,
    )
    .await?;
    Ok(())
}
