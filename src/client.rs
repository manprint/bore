//! Client implementation for the `bore` service.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::{net::TcpStream, time::timeout};
#[cfg(feature = "udp")]
use tracing::{debug, trace};
use tracing::{error, info, info_span, warn, Instrument};

use crate::auth::Authenticator;
use crate::mux;
use crate::shared::{
    tune_tcp, ClientMessage, Delimited, ServerMessage, TunnelOptions, NETWORK_TIMEOUT,
    PROXY_BUFFER_SIZE,
};
use crate::transport::{self, Endpoint};

#[cfg(feature = "udp")]
use std::net::SocketAddr;
use std::time::Duration;
#[cfg(feature = "udp")]
use tokio::net::UdpSocket;
#[cfg(feature = "udp")]
use tokio::sync::{mpsc, Semaphore};

/// State structure for the client.
pub struct Client {
    /// Control substream to the server.
    control: Option<Delimited<mux::Stream>>,

    /// Accepts forwarded connections multiplexed by the server.
    acceptor: Option<mux::Acceptor>,

    /// Local host that is forwarded.
    local_host: String,

    /// Local port that is forwarded.
    local_port: u16,

    /// Port that is publicly available on the remote.
    remote_port: u16,

    /// UDP socket reserved for a direct hole-punched path; `Some` only for a
    /// secret provider that opted into the `udp` direct-path mode.
    #[cfg(feature = "udp")]
    udp_socket: Option<UdpSocket>,

    /// Tunnel secret, retained to derive the direct-path token.
    #[cfg(feature = "udp")]
    secret: Option<String>,

    /// Provider-side direct-path config; `Some` only for a secret provider that
    /// requested `--udp`. Retained so [`Client::listen`] can re-offer UDP
    /// candidates if the initial offer failed, and bound direct substreams.
    #[cfg(feature = "udp")]
    udp_cfg: Option<UdpProviderCfg>,
}

/// Provider-side direct-path configuration, retained on the [`Client`] so the
/// listen loop can (re)offer candidates and bound concurrent direct substreams.
#[cfg(feature = "udp")]
struct UdpProviderCfg {
    endpoint: Endpoint,
    stun_server: Option<String>,
    port_map: bool,
    port_prediction: bool,
    udp_port: u16,
    /// Bounds concurrently served direct-path substreams — the direct-path analog
    /// of the server's relay `--max-conns` (here it protects the provider host).
    permits: Arc<Semaphore>,
}

impl Client {
    /// Create a new client.
    pub async fn new(
        local_host: &str,
        local_port: u16,
        to: &str,
        port: u16,
        secret: Option<&str>,
        insecure: bool,
        options: TunnelOptions,
    ) -> Result<Self> {
        let endpoint = Endpoint::parse(to);
        let socket = transport::connect(&endpoint, insecure).await?;
        let (opener, acceptor) = mux::client(socket);

        // The control substream carries the handshake and heartbeats. It is the
        // only stream the client opens; the server opens one per forwarded
        // connection, which arrive through the acceptor.
        let mut control = Delimited::new(
            opener
                .open()
                .await
                .context("failed to open control stream")?,
        );

        // Send Hello first: yamux opens substreams lazily, so this first write is
        // what announces the control substream (SYN) to the server. During
        // authentication the server speaks first, so without this the server would
        // never see the stream and both sides would deadlock.
        control.send(ClientMessage::Hello(port, options)).await?;
        if let Some(secret) = secret {
            Authenticator::new(secret)
                .client_handshake(&mut control)
                .await?;
        }
        let remote_port = match control.recv_timeout().await? {
            Some(ServerMessage::Hello(remote_port)) => remote_port,
            Some(ServerMessage::Error(message)) => bail!("server error: {message}"),
            Some(ServerMessage::Challenge(_)) => {
                bail!("server requires authentication, but no client secret was provided");
            }
            Some(_) => bail!("unexpected initial non-hello message"),
            None => bail!("unexpected EOF"),
        };
        info!(remote_port, "connected to server");
        info!("listening at {}:{remote_port}", endpoint.host);

        Ok(Client {
            control: Some(control),
            acceptor: Some(acceptor),
            local_host: local_host.to_string(),
            local_port,
            remote_port,
            #[cfg(feature = "udp")]
            udp_socket: None,
            #[cfg(feature = "udp")]
            secret: secret.map(str::to_string),
            #[cfg(feature = "udp")]
            udp_cfg: None,
        })
    }

    /// Create a client that registers as the provider of a named secret tunnel.
    ///
    /// Unlike [`Client::new`], no public port is allocated on the server: the
    /// service is reached only through a `bore proxy` referencing the same
    /// `tcp-secret-id`. The forwarding behaviour ([`Client::listen`]) is shared.
    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(not(feature = "udp"), allow(unused_variables))]
    pub async fn new_secret_provider(
        local_host: &str,
        local_port: u16,
        to: &str,
        tcp_secret_id: &str,
        secret: Option<&str>,
        insecure: bool,
        udp: bool,
        stun_server: Option<&str>,
        port_map: bool,
        port_prediction: bool,
        udp_port: u16,
        max_conns: usize,
    ) -> Result<Self> {
        let endpoint = Endpoint::parse(to);
        let socket = transport::connect(&endpoint, insecure).await?;
        let (opener, acceptor) = mux::client(socket);
        let mut control = Delimited::new(
            opener
                .open()
                .await
                .context("failed to open control stream")?,
        );

        // Send the registration first so the lazily-opened substream is announced
        // before the (server-initiated) auth handshake (see client `new`).
        control
            .send(ClientMessage::HelloSecret(tcp_secret_id.to_string()))
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
            Some(_) => bail!("unexpected response to secret registration"),
            None => bail!("unexpected EOF"),
        }
        info!(tcp_secret_id, "registered secret tunnel");

        // When the direct-path mode is requested, gather UDP candidates and offer
        // them to the server now; the actual punch happens later in `listen` when
        // a consumer arrives and the server replies with `UdpPunch`.
        #[cfg(feature = "udp")]
        let udp_socket = if udp {
            if secret.is_none() {
                warn!(
                    "--udp without --secret: the direct-path token derives from an empty key, so \
                     its security rests only on the (random) server nonce and the control channel. \
                     Pass --secret for a strong token."
                );
            }
            match offer_provider_candidates(
                &mut control,
                &endpoint,
                stun_server,
                port_map,
                port_prediction,
                udp_port,
            )
            .await
            {
                Ok(socket) => Some(socket),
                Err(err) => {
                    warn!(%err, "udp candidate offer failed, relay only");
                    None
                }
            }
        } else {
            None
        };
        #[cfg(not(feature = "udp"))]
        if udp {
            warn!("built without the `udp` feature; ignoring direct-path request");
        }

        #[cfg(feature = "udp")]
        let udp_cfg = udp.then(|| UdpProviderCfg {
            endpoint: endpoint.clone(),
            stun_server: stun_server.map(str::to_string),
            port_map,
            port_prediction,
            udp_port,
            permits: Arc::new(Semaphore::new(max_conns)),
        });

        Ok(Client {
            control: Some(control),
            acceptor: Some(acceptor),
            local_host: local_host.to_string(),
            local_port,
            remote_port: 0,
            #[cfg(feature = "udp")]
            udp_socket,
            #[cfg(feature = "udp")]
            secret: secret.map(str::to_string),
            #[cfg(feature = "udp")]
            udp_cfg,
        })
    }

    /// Returns the port publicly available on the remote.
    pub fn remote_port(&self) -> u16 {
        self.remote_port
    }

    /// Start the client, listening for new connections.
    pub async fn listen(mut self) -> Result<()> {
        let mut control = self.control.take().unwrap();
        let mut acceptor = self.acceptor.take().unwrap();
        #[cfg(feature = "udp")]
        let mut udp_socket = self.udp_socket.take();
        #[cfg(feature = "udp")]
        let secret = self.secret.clone();
        // Once the direct path is up, later `UdpPunch` messages (a new or
        // reconnecting consumer) are forwarded here to re-punch the NAT.
        #[cfg(feature = "udp")]
        let mut punch_tx: Option<mpsc::UnboundedSender<Vec<SocketAddr>>> = None;
        let this = Arc::new(self);
        // Retry the provider's UDP candidate offer if the initial one failed, so a
        // transient bootstrap problem does not leave the provider relay-only for
        // the whole session (the consumer already retries; the provider did not).
        // The first tick fires immediately, re-offering at once if needed.
        let mut udp_retry = tokio::time::interval(Duration::from_secs(15));
        loop {
            tokio::select! {
                // Drain the control substream so the server's heartbeats are read;
                // this also surfaces server errors and connection teardown.
                message = control.recv() => {
                    match message? {
                        Some(ServerMessage::Heartbeat) => (),
                        Some(ServerMessage::Error(err)) => error!(%err, "server error"),
                        Some(ServerMessage::Hello(_)) => warn!("unexpected hello"),
                        Some(ServerMessage::Challenge(_)) => warn!("unexpected challenge"),
                        Some(ServerMessage::Ok) => warn!("unexpected ok"),
                        Some(ServerMessage::UdpPunch { nonce, peer }) => {
                            #[cfg(feature = "udp")]
                            {
                                if let Some(tx) = &punch_tx {
                                    // Direct path already up: re-punch toward the
                                    // new/reconnecting consumer (the nonce is stable).
                                    // Unbounded so a burst of consumers never drops a
                                    // re-punch (payloads are tiny, peers bounded).
                                    let _ = tx.send(peer);
                                } else if let Some(socket) = udp_socket.take() {
                                    let token =
                                        crate::holepunch::derive_token(secret.as_deref(), &nonce);
                                    let (tx, rx) = mpsc::unbounded_channel();
                                    punch_tx = Some(tx);
                                    let permits = this
                                        .udp_cfg
                                        .as_ref()
                                        .map(|c| Arc::clone(&c.permits))
                                        .expect("provider udp cfg present when a socket exists");
                                    let this = Arc::clone(&this);
                                    tokio::spawn(async move {
                                        if let Err(err) =
                                            provider_direct(socket, peer, token, this, rx, permits)
                                                .await
                                        {
                                            warn!(%err, "direct provider path ended");
                                        }
                                    });
                                } else {
                                    warn!("unexpected udp punch");
                                }
                            }
                            #[cfg(not(feature = "udp"))]
                            {
                                let _ = (nonce, peer);
                                warn!("unexpected udp punch");
                            }
                        }
                        Some(ServerMessage::UdpUnavailable) => warn!("unexpected udp unavailable"),
                        None => return Ok(()),
                    }
                }
                // Periodically re-offer UDP candidates if the provider requested
                // `--udp` but has no active socket yet (initial offer failed and no
                // direct path is up). Bounded by STUN's own short timeouts.
                _ = udp_retry.tick() => {
                    #[cfg(feature = "udp")]
                    if punch_tx.is_none() && udp_socket.is_none() {
                        if let Some(cfg) = this.udp_cfg.as_ref() {
                            match offer_provider_candidates(
                                &mut control,
                                &cfg.endpoint,
                                cfg.stun_server.as_deref(),
                                cfg.port_map,
                                cfg.port_prediction,
                                cfg.udp_port,
                            )
                            .await
                            {
                                Ok(socket) => {
                                    info!("provider udp candidate offer succeeded on retry");
                                    udp_socket = Some(socket);
                                }
                                Err(err) => {
                                    debug!(%err, "provider udp re-offer failed; will retry")
                                }
                            }
                        }
                    }
                }
                stream = acceptor.accept() => {
                    let Some(stream) = stream else {
                        return Ok(());
                    };
                    let this = Arc::clone(&this);
                    tokio::spawn(
                        async move {
                            info!("new connection");
                            match this.handle_connection(stream).await {
                                Ok(_) => info!("connection exited"),
                                Err(err) => warn!(%err, "connection exited with error"),
                            }
                        }
                        .instrument(info_span!("proxy")),
                    );
                }
            }
        }
    }

    async fn handle_connection(&self, mut stream: mux::Stream) -> Result<()> {
        // Consume the server's readiness marker before splicing (see mux module).
        let mut marker = [0u8; 1];
        stream.read_exact(&mut marker).await?;
        let mut local_conn = connect_with_timeout(&self.local_host, self.local_port).await?;
        tokio::io::copy_bidirectional_with_sizes(
            &mut local_conn,
            &mut stream,
            PROXY_BUFFER_SIZE,
            PROXY_BUFFER_SIZE,
        )
        .await?;
        Ok(())
    }
}

/// Provider side: bind a UDP socket, discover candidates via STUN, and offer
/// them to the server. The socket is held for the later punch in [`Client::listen`].
#[cfg(feature = "udp")]
async fn offer_provider_candidates(
    control: &mut Delimited<mux::Stream>,
    endpoint: &Endpoint,
    stun_server: Option<&str>,
    port_map: bool,
    port_prediction: bool,
    udp_port: u16,
) -> Result<UdpSocket> {
    use crate::holepunch;
    let stun = holepunch::resolve_stun(&endpoint.host, endpoint.port, stun_server).await?;
    let socket = holepunch::bind_socket(udp_port).await?;
    let candidates = holepunch::gather_candidates(&socket, stun, port_map, port_prediction).await;
    if candidates.is_empty() {
        bail!("no local UDP candidates discovered");
    }
    info!(?candidates, %stun, "provider offering udp candidates (a public IP here means STUN worked)");
    control
        .send(ClientMessage::UdpCandidates(candidates))
        .await?;
    Ok(socket)
}

/// Provider side: punch toward the consumer, run a QUIC server endpoint, and
/// serve each accepted connection's substreams to the local service — reusing
/// [`Client::handle_connection`] exactly as the relay path does. `punch_rx`
/// delivers later consumers' candidates so the endpoint re-punches its NAT (the
/// raw socket is owned by `quinn` after setup), supporting reconnecting and
/// additional consumers without tearing the direct path down.
#[cfg(feature = "udp")]
async fn provider_direct(
    socket: UdpSocket,
    peers: Vec<SocketAddr>,
    token: [u8; crate::holepunch::TOKEN_LEN],
    client: Arc<Client>,
    mut punch_rx: mpsc::UnboundedReceiver<Vec<SocketAddr>>,
    permits: Arc<Semaphore>,
) -> Result<()> {
    let listener = crate::holepunch::DirectListener::new(socket, peers).await?;
    info!("direct udp path ready, accepting connections");
    loop {
        tokio::select! {
            res = listener.accept(token) => {
                match res {
                    Ok(quic) => {
                        // The consumer is the yamux client (opens substreams); the
                        // provider is the yamux server and dials the local service.
                        let (_opener, mut acceptor) = mux::server(quic);
                        let client = Arc::clone(&client);
                        let permits = Arc::clone(&permits);
                        tokio::spawn(async move {
                            while let Some(stream) = acceptor.accept().await {
                                // Bound concurrently served direct substreams, the
                                // direct-path analog of the relay's `--max-conns`;
                                // over the cap, drop (as the relay does).
                                let permit = match Arc::clone(&permits).try_acquire_owned() {
                                    Ok(permit) => permit,
                                    Err(_) => {
                                        warn!("direct path at max-conns, dropping connection");
                                        continue;
                                    }
                                };
                                let client = Arc::clone(&client);
                                tokio::spawn(
                                    async move {
                                        let _permit = permit;
                                        debug!("serving local connection over direct udp path");
                                        if let Err(err) = client.handle_connection(stream).await {
                                            warn!(%err, "direct connection closed with error");
                                        }
                                    }
                                    .instrument(info_span!("direct")),
                                );
                            }
                        });
                    }
                    // A single bad handshake (e.g. token mismatch from a stray
                    // peer) must not tear down the listener; log and keep serving.
                    Err(err) => {
                        trace!(%err, "direct accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            msg = punch_rx.recv() => match msg {
                Some(peers) => {
                    info!(?peers, "re-punching direct udp path toward consumer");
                    listener.punch_via_endpoint(&peers);
                }
                // Control connection gone: close the endpoint gracefully so the
                // consumer detects the teardown at once, then stop.
                None => {
                    listener.close();
                    return Ok(());
                }
            }
        }
    }
}

pub(crate) async fn connect_with_timeout(to: &str, port: u16) -> Result<TcpStream> {
    let stream = match timeout(NETWORK_TIMEOUT, TcpStream::connect((to, port))).await {
        Ok(res) => res,
        Err(err) => Err(err.into()),
    }
    .with_context(|| format!("could not connect to {to}:{port}"))?;
    // TCP_NODELAY (latency) + keepalive (stability on long, quiet transfers).
    tune_tcp(&stream);
    Ok(stream)
}
