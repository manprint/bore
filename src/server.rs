//! Server implementation for the `bore` service.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::{io, ops::RangeInclusive, sync::Arc, time::Duration};

use anyhow::Result;
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
#[cfg(feature = "vpn")]
use tokio::sync::{mpsc, Semaphore};
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, info_span, trace, warn, Instrument};
use uuid::Uuid;

use crate::admin::{ActiveGuard, AdminRegistry, NewEntry, Role};
use crate::admin_http::{self, ServerStatus};
use crate::auth::Authenticator;
use crate::edge;
use crate::holepunch;
use crate::mux;
use crate::pool::{self, Carrier, CarrierPool, PendingCarriers, TokenGuard};
use crate::prefixed::Prefixed;
use crate::secret::{self, Registry, UdpRegistry};
use crate::shared::{
    proxy_buffer_size, tune_tcp, ClientMessage, Delimited, ServerMessage, TunnelOptions,
    UdpDirectTuning, CONTROL_PORT, NETWORK_TIMEOUT,
};
use crate::udp_diagnostic;
use crate::vhost::{self, VhostRegistry};
#[cfg(feature = "vpn")]
use crate::vpn_server;

/// Default cap on the number of concurrently proxied connections per tunnel
/// connection. Bounds memory and file-descriptor use under a connection flood.
/// Overridable with [`Server::set_max_conns`].
pub const DEFAULT_MAX_CONNS: usize = 1024;

/// Default cap on the number of parallel TCP carrier connections a single tunnel
/// may use for its data path. Overridable with [`Server::set_max_carriers`].
pub const DEFAULT_MAX_CARRIERS: u16 = 16;

/// Default HSTS value served on HTTPS control-port HTTP responses.
pub const DEFAULT_CONTROL_HSTS: &str = "max-age=31536000; includeSubDomains";

/// Interval at which the server sends heartbeats to detect a dead client.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// State structure for the server.
pub struct Server {
    /// Range of TCP ports that can be forwarded.
    port_range: RangeInclusive<u16>,

    /// Optional secret used to authenticate clients.
    auth: Option<Authenticator>,

    /// Raw shared secret retained for vhost QUIC token derivation.
    secret: Option<String>,

    /// Limits the number of concurrently proxied connections per client.
    conn_permits: Arc<Semaphore>,

    /// Maximum number of parallel TCP carrier connections a tunnel may use.
    max_carriers: u16,

    /// Direct-UDP transport tuning brokered to peers.
    udp_tuning: UdpDirectTuning,

    /// Pending carrier pools, keyed by the per-tunnel token issued in
    /// [`ServerMessage::CarrierToken`]. An extra connection presenting the token
    /// (via [`ClientMessage::JoinCarrier`]) has its substream opener delivered to
    /// whoever registered the token — a public tunnel ([`Server::serve_tunnel`]) or
    /// a secret provider ([`secret::serve_provider`]).
    pending_carriers: PendingCarriers,

    /// Registry of named secret-tunnel providers, keyed by `tcp-secret-id`.
    providers: Registry,

    /// Registry of UDP-capable providers, used to broker direct hole-punched
    /// paths between a provider and a consumer.
    udp_providers: UdpRegistry,

    /// Whether to broker UDP direct paths and run the STUN responder.
    udp: bool,

    /// Pending paired `bore test-udp` sessions, keyed by diagnostic id.
    udp_tests: udp_diagnostic::Registry,

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

    /// Optional HSTS value added to HTTPS control-port HTTP responses.
    control_hsts: Option<String>,

    /// Registry of live vhost providers, keyed by subdomain label.
    vhost_registry: VhostRegistry,

    /// Hot-swappable vhost config; `None` when vhost is not configured.
    vhost_config: Option<vhost::SharedVhostConfig>,

    /// Hot-swappable TLS acceptor for the vhost HTTPS frontend.
    vhost_tls: Arc<std::sync::RwLock<Option<Arc<TlsAcceptor>>>>,

    /// UDP port used by the vhost QUIC direct path.
    vhost_quic_port: u16,

    /// Whether the vhost QUIC port was explicitly overridden. When false, the
    /// default tracks the active vhost frontend port at startup: HTTPS when the
    /// resolved mode serves HTTPS, otherwise HTTP.
    vhost_quic_port_explicit: bool,

    /// Server-issued nonces for live vhost direct-path negotiations.
    pending_vhost_udp: vhost::PendingVhostUdp,

    /// Path to the vhost config file, retained for the hot-reload task.
    vhost_config_path: Option<PathBuf>,

    /// Whether VPN brokering is enabled.
    #[cfg(feature = "vpn")]
    vpn_enabled: bool,

    /// The overlay address pool for VPN (from --vpn-pool).
    #[cfg(feature = "vpn")]
    vpn_pool: Option<crate::vpn_server::VpnPoolHandle>,

    /// Registry of live VPN providers, keyed by VPN link ID.
    #[cfg(feature = "vpn")]
    vpn_providers: crate::vpn_server::VpnRegistry,

    /// Semaphore bounding concurrent VPN links.
    #[cfg(feature = "vpn")]
    vpn_link_permits: Arc<Semaphore>,

    /// How long the VPN broker waits for the listener's UDP candidates after
    /// the connector's offer before sending `UdpUnavailable` (DEC-3).
    #[cfg(feature = "vpn")]
    vpn_punch_timeout: std::time::Duration,
}

impl Server {
    /// Create a new server with a specified minimum port number.
    pub fn new(port_range: RangeInclusive<u16>, secret: Option<&str>) -> Self {
        assert!(!port_range.is_empty(), "must provide at least one port");
        Server {
            port_range,
            conn_permits: Arc::new(Semaphore::new(DEFAULT_MAX_CONNS)),
            max_carriers: DEFAULT_MAX_CARRIERS,
            udp_tuning: UdpDirectTuning::default(),
            pending_carriers: Arc::new(DashMap::new()),
            providers: Registry::default(),
            udp_providers: UdpRegistry::default(),
            udp: false,
            udp_tests: udp_diagnostic::Registry::default(),
            control_port: CONTROL_PORT,
            tls: None,
            bind_domain: None,
            secret: secret.map(str::to_string),
            auth: secret.map(Authenticator::new),
            bind_addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            bind_tunnels: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            admin: AdminRegistry::default(),
            admin_token: None,
            control_hsts: Some(DEFAULT_CONTROL_HSTS.to_string()),
            vhost_registry: VhostRegistry::default(),
            vhost_config: None,
            vhost_tls: Arc::new(std::sync::RwLock::new(None)),
            vhost_quic_port: 443,
            vhost_quic_port_explicit: false,
            pending_vhost_udp: vhost::PendingVhostUdp::default(),
            vhost_config_path: None,
            #[cfg(feature = "vpn")]
            vpn_enabled: false,
            #[cfg(feature = "vpn")]
            vpn_pool: None,
            #[cfg(feature = "vpn")]
            vpn_providers: Arc::new(DashMap::new()),
            #[cfg(feature = "vpn")]
            vpn_link_permits: Arc::new(Semaphore::new(100)),
            #[cfg(feature = "vpn")]
            vpn_punch_timeout: vpn_server::DEFAULT_VPN_PUNCH_TIMEOUT,
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

    /// Set the maximum number of parallel TCP carrier connections a single tunnel
    /// may use for its data path (the cap on a client's `carriers` request). See
    /// [`DEFAULT_MAX_CARRIERS`].
    pub fn set_max_carriers(&mut self, max_carriers: u16) {
        self.max_carriers = max_carriers;
    }

    /// Set the direct-UDP transport tuning brokered to peers.
    pub fn set_udp_tuning(&mut self, udp_tuning: UdpDirectTuning) {
        self.udp_tuning = udp_tuning;
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

    /// Set the HSTS value served on HTTPS control-port HTTP responses.
    /// Pass `off`, `false`, `none`, or an empty string to disable it.
    pub fn set_control_hsts(&mut self, hsts: &str) {
        let value = hsts.trim();
        self.control_hsts = if value.is_empty()
            || value.eq_ignore_ascii_case("off")
            || value.eq_ignore_ascii_case("false")
            || value.eq_ignore_ascii_case("none")
        {
            None
        } else {
            Some(value.to_string())
        };
    }

    /// Set the UDP port used by the vhost QUIC direct path.
    pub fn set_vhost_quic_port(&mut self, port: u16) {
        self.vhost_quic_port = port;
        self.vhost_quic_port_explicit = true;
    }

    fn default_vhost_quic_port(cfg: &vhost::VhostConfig) -> u16 {
        let mode =
            vhost::resolve_mode(cfg, vhost::cert_present(cfg)).unwrap_or(vhost::VhostMode::Http);
        if mode.serves_https() {
            cfg.https_port
        } else {
            cfg.http_port
        }
    }

    /// Shared handle to the live tunnel registry (for the admin status page).
    pub fn admin_registry(&self) -> AdminRegistry {
        self.admin.clone()
    }

    /// Shared handle to the live vhost registry.
    pub fn vhost_registry(&self) -> VhostRegistry {
        self.vhost_registry.clone()
    }

    /// Enable the vhost frontend with the given config.
    ///
    /// Loads the config once at startup; the hot-reload task updates it later.
    /// Returns an error if the mode requires a cert that is not present.
    pub fn set_vhost(&mut self, cfg: vhost::VhostConfig) -> Result<()> {
        use crate::transport;
        use crate::vhost::{cert_present, resolve_mode};
        use std::sync::RwLock;

        let cert_present = cert_present(&cfg);
        let _ = resolve_mode(&cfg, cert_present)?; // fail fast on bad mode+cert combo

        // Load TLS if cert+key are present.
        let tls_acceptor: Option<Arc<TlsAcceptor>> = if cert_present {
            let cert = cfg.cert_file.as_ref().unwrap();
            let key = cfg.key_file.as_ref().unwrap();
            let acceptor = transport::load_server_tls(
                cert.to_str().unwrap_or_default(),
                key.to_str().unwrap_or_default(),
            )
            .map(Arc::new)?;
            Some(acceptor)
        } else {
            None
        };

        let shared_cfg = Arc::new(RwLock::new(Arc::new(cfg)));
        let shared_tls = Arc::new(RwLock::new(tls_acceptor));
        if !self.vhost_quic_port_explicit {
            self.vhost_quic_port =
                Self::default_vhost_quic_port(shared_cfg.read().unwrap().as_ref());
        }
        self.vhost_config = Some(shared_cfg);
        self.vhost_tls = shared_tls;
        Ok(())
    }

    /// Store the path to the vhost config file so the hot-reload task can poll it.
    pub fn set_vhost_config_path(&mut self, path: PathBuf) {
        self.vhost_config_path = Some(path);
    }

    /// Enable VPN brokering.
    #[cfg(feature = "vpn")]
    pub fn set_vpn(&mut self, enabled: bool) {
        self.vpn_enabled = enabled;
    }

    /// Set the overlay address pool for VPN (from --vpn-pool).
    #[cfg(feature = "vpn")]
    pub fn set_vpn_pool(&mut self, pool: crate::shared::Ipv4Net) -> Result<()> {
        let p = vpn_server::VpnPool::new(pool)?;
        self.vpn_pool = Some(Arc::new(std::sync::Mutex::new(p)));
        Ok(())
    }

    /// Set the maximum number of concurrent VPN links.
    #[cfg(feature = "vpn")]
    pub fn set_vpn_max_links(&mut self, max: usize) {
        self.vpn_link_permits = Arc::new(Semaphore::new(max));
    }

    /// Override the broker's wait for the listener's UDP candidates (DEC-3).
    /// Intended for tests; production uses [`vpn_server::DEFAULT_VPN_PUNCH_TIMEOUT`].
    #[cfg(feature = "vpn")]
    pub fn set_vpn_punch_timeout(&mut self, timeout: std::time::Duration) {
        self.vpn_punch_timeout = timeout;
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

        // When vhost is configured, spawn the HTTP and/or HTTPS frontend listeners.
        if let Some(cfg_arc) = &this.vhost_config {
            let cfg = cfg_arc.read().unwrap().clone();
            let mode = vhost::resolve_mode(&cfg, vhost::cert_present(&cfg))
                .unwrap_or(vhost::VhostMode::Http);
            // In the unified topology the control port doubles as the vhost frontend
            // (it routes HTTP by Host). When a configured frontend port equals the
            // control port, skip the standalone listener so the two don't fight over
            // the same bind ("address in use"); the control port serves it instead.
            let http_unified = cfg.http_port == this.control_port;
            let https_unified = cfg.https_port == this.control_port;
            if http_unified || https_unified {
                info!(
                    control_port = this.control_port,
                    "vhost served on the control port (unified); skipping the duplicate frontend listener"
                );
            }
            if mode.serves_http() && !http_unified {
                let http_listener = TcpListener::bind((this.bind_tunnels, cfg.http_port)).await?;
                let port = http_listener.local_addr()?.port();
                info!(port, "vhost HTTP frontend listening");
                let this2 = Arc::clone(&this);
                tokio::spawn(async move {
                    loop {
                        match http_listener.accept().await {
                            Ok((stream, _addr)) => {
                                tune_tcp(&stream);
                                let permit = match Arc::clone(&this2.conn_permits)
                                    .try_acquire_owned()
                                {
                                    Ok(p) => p,
                                    Err(_) => {
                                        debug!("vhost HTTP connection dropped: max-conns reached");
                                        continue;
                                    }
                                };
                                let this3 = Arc::clone(&this2);
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    if let Err(e) = vhost::handle_http(
                                        stream,
                                        &this3.vhost_registry,
                                        &this3.vhost_config,
                                        mode,
                                    )
                                    .await
                                    {
                                        trace!(%e, "vhost http connection closed");
                                    }
                                });
                            }
                            Err(err) => warn!(%err, "vhost HTTP accept error"),
                        }
                    }
                });
            }
            if mode.serves_https() && !https_unified {
                let https_listener = TcpListener::bind((this.bind_tunnels, cfg.https_port)).await?;
                let port = https_listener.local_addr()?.port();
                info!(port, "vhost HTTPS frontend listening");
                let this2 = Arc::clone(&this);
                tokio::spawn(async move {
                    loop {
                        match https_listener.accept().await {
                            Ok((stream, _addr)) => {
                                tune_tcp(&stream);
                                let permit = match Arc::clone(&this2.conn_permits)
                                    .try_acquire_owned()
                                {
                                    Ok(p) => p,
                                    Err(_) => {
                                        debug!("vhost HTTPS connection dropped: max-conns reached");
                                        continue;
                                    }
                                };
                                let this3 = Arc::clone(&this2);
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    if let Err(e) = vhost::handle_https(
                                        stream,
                                        &this3.vhost_registry,
                                        &this3.vhost_config,
                                        &this3.vhost_tls,
                                    )
                                    .await
                                    {
                                        trace!(%e, "vhost https connection closed");
                                    }
                                });
                            }
                            Err(err) => warn!(%err, "vhost HTTPS accept error"),
                        }
                    }
                });
            }

            // Hot-reload task: poll config + cert mtimes every 2s.
            let this2 = Arc::clone(&this);
            tokio::spawn(vhost::run_reload_task(
                this2.vhost_config.clone(),
                this2.vhost_tls.clone(),
                this2.vhost_config_path.clone(),
            ));
        }

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

        #[cfg(feature = "udp")]
        if this.udp && this.vhost_config.is_some() {
            match tokio::net::UdpSocket::bind((this.bind_addr, this.vhost_quic_port)).await {
                Ok(udp) => match holepunch::vhost_server_endpoint(udp, &this.udp_tuning) {
                    Ok(endpoint) => {
                        info!(
                            port = this.vhost_quic_port,
                            "vhost QUIC direct endpoint listening"
                        );
                        let registry = this.vhost_registry.clone();
                        let pending = this.pending_vhost_udp.clone();
                        let secret = this.secret.clone();
                        let ep = endpoint.clone();
                        tokio::spawn(async move {
                            while let Some(incoming) = ep.accept().await {
                                let registry = registry.clone();
                                let pending = pending.clone();
                                let secret = secret.clone();
                                let endpoint = ep.clone();
                                tokio::spawn(async move {
                                    let conn = match incoming.await {
                                        Ok(conn) => conn,
                                        Err(err) => {
                                            debug!(%err, "vhost QUIC handshake failed");
                                            return;
                                        }
                                    };

                                    let lookup = |subdomain: &str| {
                                        pending.get(subdomain).map(|nonce| {
                                            holepunch::derive_token(
                                                secret.as_deref(),
                                                nonce.value(),
                                            )
                                        })
                                    };

                                    match holepunch::vhost_server_handshake(conn, endpoint, lookup)
                                        .await
                                    {
                                        Ok((subdomain, direct)) => {
                                            let entry = registry
                                                .get(&subdomain)
                                                .map(|entry| Arc::clone(entry.value()));
                                            if let Some(entry) = entry {
                                                match entry.direct.install(direct.clone()) {
                                                    Some(id) => {
                                                        info!(subdomain = %subdomain, id, carriers = entry.direct.len(), "vhost QUIC direct carrier established");
                                                        let registry = registry.clone();
                                                        tokio::spawn(async move {
                                                            direct.closed().await;
                                                            if let Some(entry) = registry
                                                                .get(&subdomain)
                                                                .map(|entry| {
                                                                    Arc::clone(entry.value())
                                                                })
                                                            {
                                                                entry.direct.remove(id);
                                                                debug!(subdomain = %subdomain, id, carriers = entry.direct.len(), "vhost QUIC direct carrier closed");
                                                            }
                                                        });
                                                    }
                                                    None => {
                                                        debug!(subdomain = %subdomain, "vhost QUIC direct pool full; dropping extra carrier");
                                                        direct.close();
                                                    }
                                                }
                                            } else {
                                                debug!(subdomain = %subdomain, "vhost QUIC connection arrived after provider deregistered");
                                            }
                                        }
                                        Err(err) => debug!(%err, "vhost QUIC handshake rejected"),
                                    }
                                });
                            }
                        });
                    }
                    Err(err) => {
                        warn!(%err, "failed to configure vhost QUIC endpoint; vhost --udp disabled")
                    }
                },
                Err(err) => warn!(%err, "failed to bind vhost QUIC endpoint; vhost --udp disabled"),
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
    /// When the admin status page or the vhost frontend is enabled, the first byte
    /// is inspected: an HTTP request is handled by [`Server::serve_control_http`]
    /// (vhost routing by Host, then admin / 404), anything else falls through to the
    /// bore protocol. When neither is enabled this is a thin pass-through to
    /// [`Server::handle_connection`], so the plain protocol path is unchanged.
    async fn route_connection<S: mux::Transport>(
        &self,
        mut socket: S,
        peer: SocketAddr,
    ) -> Result<()> {
        // Neither admin page nor vhost frontend → never inspect; behave exactly as
        // before (the plain bore-protocol path stays byte-for-byte unchanged).
        if self.admin_token.is_none() && self.vhost_config.is_none() {
            return self.handle_connection(socket, peer).await;
        }

        // Peek the first byte to tell an HTTP request from the bore protocol (yamux,
        // first byte 0x00). A bore client writes its Hello eagerly and an HTTP client
        // sends its request line, so this arrives promptly; on timeout we hand the
        // untouched socket to the protocol path.
        let mut first = [0u8; 1];
        match timeout(NETWORK_TIMEOUT, socket.read(&mut first)).await {
            Ok(Ok(0)) | Ok(Err(_)) => Ok(()), // EOF or read error: drop
            Ok(Ok(_)) => {
                let stream = Prefixed::new(first.to_vec(), socket);
                if admin_http::is_http_first_byte(first[0]) {
                    self.serve_control_http(stream).await
                } else {
                    self.handle_connection(stream, peer).await
                }
            }
            // Timed out waiting for the first byte: the byte (if any) is still
            // pending, so forward the untouched socket to the protocol path.
            Err(_) => self.handle_connection(socket, peer).await,
        }
    }

    /// Handle an HTTP request that arrived on the control port. When a vhost
    /// frontend is configured, route by Host header to the matching live subdomain,
    /// so a single public port (e.g. 443) serves both the bore control protocol and
    /// the vhost reverse proxy. A request that matches no subdomain falls through to
    /// the admin status page (if enabled) or a 404.
    async fn serve_control_http<S: mux::Transport>(&self, mut stream: Prefixed<S>) -> Result<()> {
        if let Some(cfg_lock) = &self.vhost_config {
            // Read the request head so we can route by Host; on timeout/error, drop.
            let head = match timeout(NETWORK_TIMEOUT, vhost::read_head_async(&mut stream)).await {
                Ok(Ok(head)) => head,
                _ => return Ok(()),
            };
            let cfg = cfg_lock.read().unwrap().clone();
            let sub = vhost::extract_host_from_head(&head)
                .and_then(|h| vhost::extract_subdomain(h, &cfg.base_domain));
            if let Some(sub) = sub {
                if let Some(entry) = self.vhost_registry.get(&sub).map(|e| Arc::clone(e.value())) {
                    return vhost::relay_vhost(stream, &entry, head).await;
                }
            }
            // Not a vhost route: replay the already-read head for the admin / 404 path.
            let replayed = Prefixed::new(head, stream);
            return self.serve_admin_http(replayed).await;
        }
        self.serve_admin_http(stream).await
    }

    /// Serve the admin status page when an admin token is configured, otherwise a
    /// minimal 404 (the request reached the control port but matched no vhost route).
    async fn serve_admin_http<S: mux::Transport>(&self, mut stream: S) -> Result<()> {
        let control_hsts = if self.tls.is_some() {
            self.control_hsts.as_deref()
        } else {
            None
        };
        match self.admin_token.clone() {
            Some(token) => {
                let server = ServerStatus {
                    control_port: self.control_port,
                    tls: self.tls.is_some(),
                    udp: self.udp,
                };
                if let Err(err) =
                    admin_http::serve(stream, &self.admin, &token, server, control_hsts).await
                {
                    trace!(%err, "admin request failed");
                }
                Ok(())
            }
            None => {
                let _ = admin_http::respond_not_found(&mut stream, control_hsts).await;
                Ok(())
            }
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
            Some(stream) => Delimited::with_label(stream, "server/control"),
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
            Some(ClientMessage::JoinCarrier { token }) => {
                self.serve_carrier(control, opener, token).await
            }
            Some(ClientMessage::HelloSecret {
                id,
                notes,
                basic_auth,
                carriers,
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
                    self.pending_carriers.clone(),
                    self.max_carriers,
                    carriers,
                    self.udp_tuning,
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
                    self.udp_tuning,
                )
                .await
            }
            Some(ClientMessage::HelloVhost {
                subdomain,
                client_id,
                notes,
                basic_auth,
                carriers,
                udp,
            }) => {
                let Some(cfg) = self.vhost_config.clone() else {
                    warn!("vhost not configured on this server");
                    let _ = control
                        .send(ServerMessage::Error("vhost not configured".into()))
                        .await;
                    return Ok(());
                };
                vhost::serve_vhost_provider(
                    control,
                    opener,
                    self.vhost_registry.clone(),
                    cfg,
                    subdomain,
                    client_id,
                    self.admin.clone(),
                    peer,
                    notes,
                    basic_auth,
                    udp,
                    self.pending_carriers.clone(),
                    self.max_carriers,
                    carriers,
                    self.udp,
                    self.vhost_quic_port,
                    self.pending_vhost_udp.clone(),
                    self.secret.clone(),
                    self.udp_tuning,
                )
                .await
            }
            Some(ClientMessage::Authenticate(_)) => {
                warn!("unexpected authenticate");
                Ok(())
            }
            Some(ClientMessage::UdpCandidates(_))
            | Some(ClientMessage::UdpCandidateOffer(_))
            | Some(ClientMessage::UdpStunHintRequest)
            | Some(ClientMessage::VhostUdpRenew { .. }) => {
                warn!("unexpected udp candidates as first message");
                Ok(())
            }
            Some(ClientMessage::TestUdpJoin {
                id,
                candidates,
                summary,
                options,
            }) => {
                udp_diagnostic::serve_peer(
                    control,
                    opener,
                    acceptor,
                    self.udp_tests.clone(),
                    id,
                    peer,
                    candidates,
                    summary,
                    options,
                    self.udp_tuning,
                )
                .await
            }
            Some(ClientMessage::HelloVpn {
                id,
                advertised,
                addr,
                notes,
                ..
            }) => {
                #[cfg(feature = "vpn")]
                if self.vpn_enabled {
                    return vpn_server::serve_vpn_listener(
                        control,
                        opener,
                        self.vpn_providers.clone(),
                        id,
                        advertised,
                        addr,
                        notes,
                        self.admin.clone(),
                        peer,
                        self.udp_providers.clone(),
                        self.udp_tuning,
                        self.vpn_link_permits.clone(),
                    )
                    .await;
                }
                #[cfg(not(feature = "vpn"))]
                let _ = (&advertised, &addr, &notes); // Suppress unused warnings when vpn feature is off
                warn!(%id, "vpn not enabled on this server");
                let _ = control
                    .send(ServerMessage::VpnError(
                        "vpn not supported/enabled on this server".into(),
                    ))
                    .await;
                Ok(())
            }
            Some(ClientMessage::ConnectVpn {
                id,
                advertised,
                addr,
                notes,
            }) => {
                #[cfg(feature = "vpn")]
                if self.vpn_enabled {
                    return vpn_server::serve_vpn_connector(
                        control,
                        acceptor,
                        self.vpn_providers.clone(),
                        self.vpn_pool.clone(),
                        self.conn_permits.clone(),
                        id,
                        advertised,
                        addr,
                        notes,
                        self.admin.clone(),
                        peer,
                        self.udp_providers.clone(),
                        self.udp_tuning,
                        self.vpn_punch_timeout,
                    )
                    .await;
                }
                #[cfg(not(feature = "vpn"))]
                let _ = (&advertised, &addr, &notes); // Suppress unused warnings when vpn feature is off
                warn!(%id, "vpn not enabled on this server");
                let _ = control
                    .send(ServerMessage::VpnError(
                        "vpn not supported/enabled on this server".into(),
                    ))
                    .await;
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

        // Carrier pool: when the client requests more than one carrier, issue a
        // per-tunnel token and tell it how many extra connections to open (clamped
        // to `--max-carriers`). Those connections arrive as separate `JoinCarrier`
        // handshakes and deliver their substream openers through `carrier_rx`. The
        // data path is unchanged — only *which* connection opens each substream.
        let effective = opts.carriers.clamp(1, self.max_carriers.max(1));
        let pool = CarrierPool::new(opener);
        let mut carrier_rx = if opts.carriers > 1 {
            let extra = effective - 1;
            let token = Uuid::new_v4().to_string();
            let (tx, rx) = mpsc::unbounded_channel();
            self.pending_carriers.insert(token.clone(), tx);
            control
                .send(ServerMessage::CarrierToken {
                    token: token.clone(),
                    extra,
                })
                .await?;
            info!(extra, "carrier pool offered");
            // The guard removes the token when this tunnel ends.
            Some((
                rx,
                TokenGuard::new(Arc::clone(&self.pending_carriers), token),
            ))
        } else {
            None
        };

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
                // An extra connection joined the carrier pool. Cap the pool at the
                // effective size so a misbehaving client cannot grow it without bound.
                joined = pool::recv_carrier(carrier_rx.as_mut()) => {
                    if let Some(carrier) = joined {
                        if pool.push(carrier, effective as usize) {
                            info!(size = pool.len(), "carrier joined pool");
                        }
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

                    // Round-robin across the live carriers (prunes dead ones); the
                    // control connection is always carrier 0 and always live here.
                    let opener = match pool.pick() {
                        Some(opener) => opener,
                        None => {
                            warn!("no live carrier, dropping connection");
                            continue;
                        }
                    };
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
                                let buf = proxy_buffer_size();
                                let result = tokio::io::copy_bidirectional_with_sizes(
                                    &mut edge,
                                    &mut stream,
                                    buf,
                                    buf,
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

    /// Serve an extra connection that joins a public tunnel's carrier pool: match
    /// its `token` to the pending tunnel, hand that tunnel this connection's
    /// substream opener, then hold the connection open (reading the control
    /// substream only to detect teardown) so the pool can use it. When the
    /// connection drops, the carrier is marked dead and pruned by the tunnel loop.
    async fn serve_carrier(
        &self,
        mut control: Delimited<mux::Stream>,
        opener: mux::Opener,
        token: String,
    ) -> Result<()> {
        // The token is a 122-bit random UUID, so a hash-map lookup is adequate (it
        // cannot be feasibly guessed; a constant-time scan would only leak the
        // pool count via timing). An unknown/expired token is rejected quietly.
        let Some(tx) = self.pending_carriers.get(&token).map(|e| e.value().clone()) else {
            warn!("carrier join with unknown token");
            return Ok(());
        };
        let carrier = Carrier::new(opener);
        let alive = Arc::clone(&carrier.alive);
        if tx.send(carrier).is_err() {
            // The tunnel ended between the lookup and the send.
            return Ok(());
        }
        info!("carrier connection joined");
        // The client sends nothing more on this substream; recv resolves only when
        // the connection drops (gracefully → None, hard kill → keepalive → Err).
        while let Ok(Some(_)) = control.recv::<ClientMessage>().await {}
        alive.store(false, Ordering::Relaxed);
        Ok(())
    }
}
