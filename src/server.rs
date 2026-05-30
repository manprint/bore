//! Server implementation for the `bore` service.

use std::net::{IpAddr, Ipv4Addr};
use std::{io, ops::RangeInclusive, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::{interval, sleep, MissedTickBehavior};
use tokio_rustls::TlsAcceptor;
use tracing::{info, info_span, trace, warn, Instrument};

use crate::auth::Authenticator;
use crate::edge;
use crate::mux;
use crate::secret::{self, Registry};
use crate::shared::{
    ClientMessage, Delimited, ServerMessage, TunnelOptions, CONTROL_PORT, PROXY_BUFFER_SIZE,
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
}

impl Server {
    /// Create a new server with a specified minimum port number.
    pub fn new(port_range: RangeInclusive<u16>, secret: Option<&str>) -> Self {
        assert!(!port_range.is_empty(), "must provide at least one port");
        Server {
            port_range,
            conn_permits: Arc::new(Semaphore::new(DEFAULT_MAX_CONNS)),
            providers: Registry::default(),
            control_port: CONTROL_PORT,
            tls: None,
            bind_domain: None,
            auth: secret.map(Authenticator::new),
            bind_addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            bind_tunnels: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        }
    }

    /// Set the TCP port the control listener binds to (default [`CONTROL_PORT`]).
    pub fn set_control_port(&mut self, control_port: u16) {
        self.control_port = control_port;
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

    /// Start the server, listening for new connections.
    pub async fn listen(self) -> Result<()> {
        let this = Arc::new(self);
        let listener = TcpListener::bind((this.bind_addr, this.control_port)).await?;
        info!(
            addr = ?this.bind_addr,
            port = this.control_port,
            domain = ?this.bind_domain,
            tls = this.tls.is_some(),
            "server listening"
        );

        loop {
            let (stream, addr) = listener.accept().await?;
            let _ = stream.set_nodelay(true);
            let this = Arc::clone(&this);
            tokio::spawn(
                async move {
                    info!("incoming connection");
                    // The TLS handshake (if any) runs here, off the accept path.
                    let result = match &this.tls {
                        Some(acceptor) => match acceptor.accept(stream).await {
                            Ok(tls) => this.handle_connection(tls).await,
                            Err(err) => {
                                warn!(%err, "TLS handshake failed");
                                return;
                            }
                        },
                        None => this.handle_connection(stream).await,
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

    async fn handle_connection<S: mux::Transport>(&self, socket: S) -> Result<()> {
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
                self.serve_tunnel(control, opener, port, opts).await
            }
            Some(ClientMessage::HelloSecret(id)) => {
                secret::serve_provider(control, opener, self.providers.clone(), id).await
            }
            Some(ClientMessage::ConnectSecret(id)) => {
                secret::serve_consumer(
                    control,
                    acceptor,
                    self.providers.clone(),
                    self.conn_permits.clone(),
                    id,
                )
                .await
            }
            Some(ClientMessage::Authenticate(_)) => {
                warn!("unexpected authenticate");
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
                    let _ = stream2.set_nodelay(true);

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
                    tokio::spawn(async move {
                        let _permit = permit;
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
