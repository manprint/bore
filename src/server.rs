//! Server implementation for the `bore` service.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{io, ops::RangeInclusive, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};
use tokio_rustls::TlsAcceptor;
use tracing::{info, info_span, trace, warn, Instrument};

use crate::admin::{ActiveGuard, AdminRegistry, NewEntry, Role};
use crate::admin_http::{self, ServerStatus};
use crate::auth::Authenticator;
use crate::edge;
use crate::holepunch;
use crate::mux;
use crate::prefixed::Prefixed;
use crate::secret::{self, Registry, UdpRegistry};
use crate::shared::{
    tune_tcp, ClientMessage, Delimited, ServerMessage, TunnelOptions, CONTROL_PORT,
    NETWORK_TIMEOUT, PROXY_BUFFER_SIZE,
};

/// Default cap on the number of concurrently proxied connections per tunnel
/// connection. Bounds memory and file-descriptor use under a connection flood.
/// Overridable with [`Server::set_max_conns`].
pub const DEFAULT_MAX_CONNS: usize = 1024;

/// Interval at which the server sends heartbeats to detect a dead client.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// State structure for the server.
pub struct Server {
    /// Range of TCP ports that can be forwarded.
    port_range: RangeInclusive<u16>,

    /// Optional secret used to authenticate clients.
    auth: Option<Authenticator>,

    /// Limits the number of concurrently proxied connections per client.
    conn_permits: Arc<Semaphore>,

    /// Registry of named secret-tunnel providers, keyed by `tcp-secret-id`.
    providers: Registry,

    /// Registry of UDP-capable providers, used to broker direct hole-punched
    /// paths between a provider and a consumer.
    udp_providers: UdpRegistry,

    /// Whether to broker UDP direct paths and run the STUN responder.
    udp: bool,

    /// TCP port the control listener binds to.
    control_port: u16,

    /// TLS acceptor for the control connection; `None` means plain TCP.
    tls: Option<TlsAcceptor>,

    /// Public domain advertised for this server (informational).
    bind_domain: Option<String>,

    /// IP address where the control server will bind to.
    bind_addr: IpAddr,

    /// IP address where tunnels will listen on.
    bind_tunnels: IpAddr,

    /// Live registry of connected tunnels, exposed by the admin status page.
    admin: AdminRegistry,

    /// Admin status-page access token. `None` disables the admin page entirely
    /// (and the HTTP detection on the control port), preserving the plain
    /// bore-protocol behaviour.
    admin_token: Option<String>,
}

impl Server {
    /// Create a new server with a specified minimum port number.
    pub fn new(port_range: RangeInclusive<u16>, secret: Option<&str>) -> Self {
        assert!(!port_range.is_empty(), "must provide at least one port");
        Server {
            port_range,
            conn_permits: Arc::new(Semaphore::new(DEFAULT_MAX_CONNS)),
            providers: Registry::default(),
            udp_providers: UdpRegistry::default(),
            udp: false,
            control_port: CONTROL_PORT,
            tls: None,
            bind_domain: None,
            auth: secret.map(Authenticator::new),
            bind_addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            bind_tunnels: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            admin: AdminRegistry::default(),
            admin_token: None,
        }
    }

    /// Set the TCP port the control listener binds to (default [`CONTROL_PORT`]).
    pub fn set_control_port(&mut self, control_port: u16) {
        self.control_port = control_port;
    }

    /// Enable brokering of UDP direct paths and the STUN responder (bound on the
    /// control port over UDP). See [`crate::holepunch`].
    pub fn set_udp(&mut self, udp: bool) {
        self.udp = udp;
    }

    /// Enable TLS on the control connection using the given acceptor.
    pub fn set_tls(&mut self, acceptor: TlsAcceptor) {
        self.tls = Some(acceptor);
    }

    /// Set the public domain advertised for this server (informational).
    pub fn set_bind_domain(&mut self, domain: String) {
        self.bind_domain = Some(domain);
    }

    /// Set the maximum number of concurrently proxied connections held per client
    /// connection at once. See [`DEFAULT_MAX_CONNS`].
    pub fn set_max_conns(&mut self, max_conns: usize) {
        self.conn_permits = Arc::new(Semaphore::new(max_conns));
    }

    /// Set the IP address where the control server will bind to.
    pub fn set_bind_addr(&mut self, bind_addr: IpAddr) {
        self.bind_addr = bind_addr;
    }

    /// Set the IP address where tunnels will listen on.
    pub fn set_bind_tunnels(&mut self, bind_tunnels: IpAddr) {
        self.bind_tunnels = bind_tunnels;
    }

    /// Enable the admin status page on the control port, guarded by `token`.
    /// `None` (the default) leaves it disabled.
    pub fn set_admin_token(&mut self, token: Option<String>) {
        self.admin_token = token;
    }

    /// Shared handle to the live tunnel registry (for the admin status page).
    pub fn admin_registry(&self) -> AdminRegistry {
        self.admin.clone()
    }

    /// Start the server, listening for new connections.
    pub async fn listen(self) -> Result<()> {
        let this = Arc::new(self);
        let listener = TcpListener::bind((this.bind_addr, this.control_port)).await?;
        info!(
            addr = ?this.bind_addr,
            port = this.control_port,
            domain = ?this.bind_domain,
            tls = this.tls.is_some(),
            udp = this.udp,
            "server listening"
        );

        // When UDP direct paths are enabled, run a STUN responder on the control
        // port over UDP so clients can discover their reflexive address without
        // any external infrastructure.
        if this.udp {
            match tokio::net::UdpSocket::bind((this.bind_addr, this.control_port)).await {
                Ok(udp) => {
                    info!(port = this.control_port, "STUN responder listening");
                    tokio::spawn(holepunch::run_stun_responder(udp));
                }
                Err(err) => warn!(%err, "failed to bind STUN responder; udp disabled"),
            }
        }

        loop {
            let (stream, addr) = listener.accept().await?;
            tune_tcp(&stream);
            let this = Arc::clone(&this);
            tokio::spawn(
                async move {
                    info!("incoming connection");
                    // The TLS handshake (if any) runs here, off the accept path.
                    let result = match &this.tls {
                        Some(acceptor) => match acceptor.accept(stream).await {
                            Ok(tls) => this.route_connection(tls, addr).await,
                            Err(err) => {
                                warn!(%err, "TLS handshake failed");
                                return;
                            }
                        },
                        None => this.route_connection(stream, addr).await,
                    };
                    match result {
                        Ok(_) => info!("connection exited"),
                        Err(err) => warn!(%err, "connection exited with error"),
                    }
                }
                .instrument(info_span!("control", ?addr)),
            );
        }
    }

    async fn create_listener(&self, port: u16) -> Result<TcpListener, &'static str> {
        let try_bind = |port: u16| async move {
            TcpListener::bind((self.bind_tunnels, port))
                .await
                .map_err(|err| match err.kind() {
                    io::ErrorKind::AddrInUse => "port already in use",
                    io::ErrorKind::PermissionDenied => "permission denied",
                    _ => "failed to bind to port",
                })
        };
        if port > 0 {
            // Client requests a specific port number.
            if !self.port_range.contains(&port) {
                return Err("client port number not in allowed range");
            }
            try_bind(port).await
        } else {
            // Client requests any available port in range.
            //
            // In this case, we bind to 150 random port numbers. We choose this value because in
            // order to find a free port with probability at least 1-δ, when ε proportion of the
            // ports are currently available, it suffices to check approximately -2 ln(δ) / ε
            // independently and uniformly chosen ports (up to a second-order term in ε).
            //
            // Checking 150 times gives us 99.999% success at utilizing 85% of ports under these
            // conditions, when ε=0.15 and δ=0.00001.
            for _ in 0..150 {
                let port = fastrand::u16(self.port_range.clone());
                match try_bind(port).await {
                    Ok(listener) => return Ok(listener),
                    Err(_) => continue,
                }
            }
            Err("failed to find an available port")
        }
    }

    /// Route an accepted (and TLS-terminated, if applicable) control connection.
    ///
    /// When the admin status page is enabled, the first byte is inspected: an HTTP
    /// request is served by the admin handler, anything else falls through to the
    /// bore protocol. When the admin page is disabled this is a thin pass-through
    /// to [`Server::handle_connection`], so the plain protocol path is unchanged.
    async fn route_connection<S: mux::Transport>(
        &self,
        mut socket: S,
        peer: SocketAddr,
    ) -> Result<()> {
        // No admin token → never inspect; behave exactly as before.
        let Some(token) = self.admin_token.clone() else {
            return self.handle_connection(socket, peer).await;
        };

        // Peek the first byte to tell an HTTP request (admin page) from the bore
        // protocol (yamux, first byte 0x00). A bore client writes its Hello
        // eagerly and an HTTP client sends its request line, so this arrives
        // promptly; on timeout we hand the untouched socket to the protocol path.
        let mut first = [0u8; 1];
        match timeout(NETWORK_TIMEOUT, socket.read(&mut first)).await {
            Ok(Ok(0)) | Ok(Err(_)) => Ok(()), // EOF or read error: drop
            Ok(Ok(_)) => {
                let stream = Prefixed::new(first.to_vec(), socket);
                if admin_http::is_http_first_byte(first[0]) {
                    let server = ServerStatus {
                        control_port: self.control_port,
                        tls: self.tls.is_some(),
                        udp: self.udp,
                    };
                    if let Err(err) = admin_http::serve(stream, &self.admin, &token, server).await {
                        trace!(%err, "admin request failed");
                    }
                    Ok(())
                } else {
                    self.handle_connection(stream, peer).await
                }
            }
            // Timed out waiting for the first byte: the byte (if any) is still
            // pending, so forward the untouched socket to the protocol path.
            Err(_) => self.handle_connection(socket, peer).await,
        }
    }

    async fn handle_connection<S: mux::Transport>(
        &self,
        socket: S,
        peer: SocketAddr,
    ) -> Result<()> {
        // Multiplex everything over this single connection. The client opens the
        // control substream first; what it requests on it selects the role.
        let (opener, mut acceptor) = mux::server(socket);
        let mut control = match acceptor.accept().await {
            Some(stream) => Delimited::new(stream),
            None => return Ok(()),
        };

        // The client sends its first message before authenticating (it must write
        // to announce the lazily-opened substream; the server speaks first during
        // auth). The request is only acted on once auth succeeds.
        let request = control.recv_timeout().await?;

        if let Some(auth) = &self.auth {
            if let Err(err) = auth.server_handshake(&mut control).await {
                warn!(%err, "server handshake failed");
                control.send(ServerMessage::Error(err.to_string())).await?;
                return Ok(());
            }
        }

        match request {
            Some(ClientMessage::Hello(port, opts)) => {
                self.serve_tunnel(control, opener, port, opts, peer).await
            }
            Some(ClientMessage::HelloSecret {
                id,
                notes,
                basic_auth,
            }) => {
                secret::serve_provider(
                    control,
                    opener,
                    self.providers.clone(),
                    self.udp_providers.clone(),
                    id,
                    self.admin.clone(),
                    peer,
                    notes,
                    basic_auth,
                )
                .await
            }
            Some(ClientMessage::ConnectSecret { id, notes }) => {
                secret::serve_consumer(
                    control,
                    acceptor,
                    self.providers.clone(),
                    self.udp_providers.clone(),
                    self.conn_permits.clone(),
                    id,
                    self.admin.clone(),
                    peer,
                    notes,
                )
                .await
            }
            Some(ClientMessage::Authenticate(_)) => {
                warn!("unexpected authenticate");
                Ok(())
            }
            Some(ClientMessage::UdpCandidates(_)) => {
                warn!("unexpected udp candidates as first message");
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Serve a public-port tunnel: bind a remote port and forward each incoming
    /// connection to the client over a fresh multiplexed substream.
    async fn serve_tunnel(
        &self,
        mut control: Delimited<mux::Stream>,
        opener: mux::Opener,
        port: u16,
        opts: TunnelOptions,
        peer: SocketAddr,
    ) -> Result<()> {
        // TLS termination on the tunnel port reuses the server's certificate.
        if opts.https && self.tls.is_none() {
            control
                .send(ServerMessage::Error(
                    "server has no TLS certificate configured".into(),
                ))
                .await?;
            return Ok(());
        }

        let listener = match self.create_listener(port).await {
            Ok(listener) => listener,
            Err(err) => {
                control.send(ServerMessage::Error(err.into())).await?;
                return Ok(());
            }
        };
        let host = listener.local_addr()?.ip();
        let port = listener.local_addr()?.port();
        info!(?host, ?port, https = opts.https, "new client");
        control.send(ServerMessage::Hello(port)).await?;

        // Register this tunnel in the admin registry for its whole lifetime; the
        // registration is removed when this function returns (client gone).
        let registration = self.admin.register(NewEntry {
            role: Role::Public,
            peer,
            secret_id: None,
            public_port: Some(port),
            notes: opts.notes.clone(),
            basic_auth: opts.basic_auth.is_some(),
            https: opts.https,
            force_https: opts.force_https,
            udp: false,
        });
        let active = registration.active();

        let mut heartbeat = interval(HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    if control.send(ServerMessage::Heartbeat).await.is_err() {
                        // Assume that the client connection has been dropped.
                        return Ok(());
                    }
                }
                result = listener.accept() => {
                    let (stream2, addr) = match result {
                        Ok(pair) => pair,
                        Err(err) => {
                            // A transient accept error (e.g. EMFILE when out of file
                            // descriptors, or a peer that reset before we accepted)
                            // must not tear down the whole tunnel. Back off briefly to
                            // avoid busy-spinning, then keep serving.
                            warn!(%err, "failed to accept tunnel connection");
                            sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    tune_tcp(&stream2);

                    // Bound the number of concurrently proxied connections. At
                    // capacity, drop the connection rather than exhausting memory
                    // and file descriptors under a flood. The permit is released
                    // when the proxied connection finishes.
                    let permit = match Arc::clone(&self.conn_permits).try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            warn!(?addr, ?port, "too many active connections, dropping");
                            continue;
                        }
                    };
                    info!(?addr, ?port, "new connection");

                    let opener = opener.clone();
                    let tls = self.tls.clone();
                    let domain = self.bind_domain.clone();
                    let opts = opts.clone();
                    let active = Arc::clone(&active);
                    tokio::spawn(async move {
                        let _permit = permit;
                        let _active = ActiveGuard::new(active);
                        // Terminate TLS / handle redirects at the edge as needed.
                        let mut edge =
                            match edge::accept(stream2, opts, tls.as_ref(), port, domain.as_deref())
                                .await
                            {
                                Ok(Some(edge)) => edge,
                                Ok(None) => return, // redirected or closed at the edge
                                Err(err) => {
                                    trace!(%err, "edge handling failed");
                                    return;
                                }
                            };
                        match opener.open().await {
                            Ok(mut stream) => {
                                // Announce the lazily-opened substream so the client
                                // dials the local service before any payload flows.
                                if let Err(err) = stream.write_all(&[mux::STREAM_READY]).await {
                                    trace!(%err, "failed to establish multiplexed stream");
                                    return;
                                }
                                let result = tokio::io::copy_bidirectional_with_sizes(
                                    &mut edge,
                                    &mut stream,
                                    PROXY_BUFFER_SIZE,
                                    PROXY_BUFFER_SIZE,
                                )
                                .await;
                                if let Err(err) = result {
                                    trace!(%err, "proxied connection closed");
                                }
                            }
                            Err(err) => warn!(%err, "failed to open multiplexed stream"),
                        }
                    });
                }
            }
        }
    }
}
