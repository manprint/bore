//! Client implementation for the `bore` service.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::{net::TcpStream, time::timeout};
use tracing::trace;
use tracing::{debug, error, info, info_span, warn, Instrument};

use crate::auth::Authenticator;
use crate::basicauth::{self, BasicAuth, Gate};
use crate::mux;
#[cfg(feature = "udp")]
use crate::shared::UdpCandidateOffer;
use crate::shared::{
    proxy_buffer_size, tune_tcp, ClientMessage, Delimited, ServerMessage, TunnelOptions,
    NETWORK_TIMEOUT,
};
use crate::transport::{self, Endpoint};
use crate::weblog::{AccessLogger, PathLayout};

#[cfg(feature = "udp")]
use std::net::SocketAddr;
use std::time::Duration;
#[cfg(feature = "udp")]
use tokio::net::UdpSocket;
#[cfg(feature = "udp")]
use tokio::sync::Semaphore;

/// Interval at which the client tops the carrier pool back up after a drop.
const CARRIER_REDIAL_INTERVAL: Duration = Duration::from_secs(15);

/// Optional operator-supplied metadata for a secret-tunnel provider.
#[derive(Clone, Default)]
pub struct ProviderMeta {
    /// Free-form note shown on the server's admin status page.
    pub notes: Option<String>,
    /// HTTP Basic auth credentials (`"user:pass"`) the provider enforces itself on
    /// each proxied HTTP connection (both relay and direct). `None` = no auth.
    pub basic_auth: Option<String>,
    /// Whether the provider runs with `--auto-reconnect`. Display-only; forwarded
    /// to the server's admin page (via `HelloVhost`) so the Vhost section can show
    /// it, matching the public-tunnel `TunnelOptions.auto_reconnect` field.
    pub auto_reconnect: bool,
}

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

    /// Whether this client is a secret-tunnel **provider** (registered via
    /// [`ClientMessage::HelloSecret`]). Only secret providers send periodic
    /// [`ClientMessage::Heartbeat`] frames so the server's recv-deadline reaper
    /// (`secret::SECRET_CTRL_TIMEOUT`) can detect a wedged/abandoned control
    /// substream; public and vhost tunnels keep the legacy heartbeat-free path.
    is_secret_provider: bool,

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

    /// Whether this vhost provider requested the QUIC direct data path.
    #[cfg(feature = "udp")]
    vhost_udp: bool,

    /// Control endpoint used to resolve the server's public UDP address for the
    /// direct path (both vhost and public tunnels).
    #[cfg(feature = "udp")]
    direct_endpoint: Option<Endpoint>,

    /// Key for the direct path: for vhost providers it's the subdomain string,
    /// for public tunnels it's `format!("port:{remote_port}")`. `None` when
    /// direct UDP is not configured.
    #[cfg(feature = "udp")]
    direct_key: Option<String>,

    /// Target number of parallel QUIC direct connections (carriers) for the
    /// direct UDP data path. `0` outside the direct-udp case; otherwise the
    /// resolved/clamped `--carriers` count (min 1). The listen loop keeps this many direct
    /// connections established, topping up after a drop.
    #[cfg(feature = "udp")]
    direct_udp_carriers: u16,

    /// HTTP Basic auth enforced on each proxied connection. `Some` only for a
    /// secret-tunnel provider with `--basic-auth` (public tunnels are enforced on
    /// the server instead). Applies to both relay and direct paths.
    basic_auth: Option<BasicAuth>,

    /// Extra carrier connections opened for a public tunnel's pool (each a held-open
    /// control substream + its data acceptor). Empty unless `--carriers > 1`. The
    /// data acceptors are pumped into the listen loop alongside the main one.
    carrier_acceptors: Vec<(Delimited<mux::Stream>, mux::Acceptor)>,

    /// Parameters to re-dial a dropped carrier and keep the pool at full width.
    /// `Some` only for a public tunnel that established a pool.
    carrier_dialer: Option<CarrierDialer>,

    /// Whether this tunnel requests access logging with real caller IP forwarding.
    /// Used to correctly read the extended STREAM_READY header (Phase 3).
    webserver_log: bool,

    /// Access logger registry (Phase 4). `Some` when `--webserver-log` is set.
    access_logger: Option<Arc<AccessLogger>>,

    /// Counter for dropped access log records (when the channel is full).
    access_logger_dropped: Arc<std::sync::atomic::AtomicU64>,

    /// Subdomain label for vhost providers; `None` for public/secret tunnels.
    /// Used to determine the correct log filename (vhost: subdomain.log, public: port.log).
    vhost_subdomain: Option<String>,
}

/// Parameters retained to (re)open a carrier connection for a public tunnel's
/// pool: dial the same server endpoint and present the per-tunnel carrier token.
#[derive(Clone)]
struct CarrierDialer {
    endpoint: Endpoint,
    insecure: bool,
    secret: Option<String>,
    token: String,
    /// Target number of extra carriers (the pool is topped back up to this).
    target_extra: usize,
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
    /// Seconds before re-checking if the preferred UDP port was released by NAT.
    nat_udp_release_timeout: Duration,
}

impl Client {
    /// Create a new client.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        local_host: &str,
        local_port: u16,
        to: &str,
        port: u16,
        secret: Option<&str>,
        insecure: bool,
        options: TunnelOptions,
        access_logger: Option<Arc<AccessLogger>>,
    ) -> Result<Self> {
        let endpoint = Endpoint::parse(to);
        let socket = transport::connect(&endpoint, insecure).await?;
        let (opener, acceptor) = mux::client(socket);

        // The control substream carries the handshake and heartbeats. It is the
        // only stream the client opens; the server opens one per forwarded
        // connection, which arrive through the acceptor.
        let mut control = Delimited::with_label(
            opener
                .open()
                .await
                .context("failed to open control stream")?,
            "client/public",
        );

        // Send Hello first: yamux opens substreams lazily, so this first write is
        // what announces the control substream (SYN) to the server. During
        // authentication the server speaks first, so without this the server would
        // never see the stream and both sides would deadlock.
        let carriers = options.carriers;
        #[cfg(feature = "udp")]
        let options_udp = options.udp;
        let webserver_log_opt = options.webserver_log;
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

        // Carrier pool: if more than one carrier was requested, the server replies
        // with a token and how many extra connections to open. Each extra dials the
        // same endpoint and joins the pool; failures are non-fatal (degrade to the
        // carriers that did connect, the re-dial timer tops up later).
        let mut carrier_acceptors = Vec::new();
        let mut carrier_dialer = None;
        if carriers > 1 {
            match control.recv_timeout().await? {
                Some(ServerMessage::CarrierToken { token, extra }) => {
                    for _ in 0..extra {
                        match open_carrier(&endpoint, insecure, secret, &token).await {
                            Ok(pair) => carrier_acceptors.push(pair),
                            Err(err) => warn!(%err, "failed to open carrier connection"),
                        }
                    }
                    info!(
                        opened = carrier_acceptors.len(),
                        requested = extra,
                        "carrier pool established"
                    );
                    if extra > 0 {
                        carrier_dialer = Some(CarrierDialer {
                            endpoint: endpoint.clone(),
                            insecure,
                            secret: secret.map(str::to_string),
                            token,
                            target_extra: extra as usize,
                        });
                    }
                }
                Some(ServerMessage::Error(message)) => bail!("server error: {message}"),
                other => bail!("expected carrier token, got {other:?}"),
            }
        }

        // For public tunnels with --udp, establish the direct path with:
        // - key = "port:{remote_port}" (the assigned public tunnel port)
        // - endpoint = for resolving server UDP address
        // - carriers = clamped to the direct-carrier limit
        #[cfg(feature = "udp")]
        let (direct_key, direct_endpoint, direct_udp_carriers) = if options_udp {
            (
                Some(format!("port:{}", remote_port)),
                Some(endpoint.clone()),
                crate::vhost::clamp_direct_carriers(carriers),
            )
        } else {
            (None, None, 0)
        };

        Ok(Client {
            control: Some(control),
            acceptor: Some(acceptor),
            local_host: local_host.to_string(),
            local_port,
            remote_port,
            is_secret_provider: false,
            #[cfg(feature = "udp")]
            udp_socket: None,
            #[cfg(feature = "udp")]
            secret: secret.map(str::to_string),
            #[cfg(feature = "udp")]
            udp_cfg: None,
            #[cfg(feature = "udp")]
            vhost_udp: false,
            #[cfg(feature = "udp")]
            direct_endpoint,
            #[cfg(feature = "udp")]
            direct_key,
            #[cfg(feature = "udp")]
            direct_udp_carriers,
            // Public tunnels are basic-auth-gated on the server, not the client.
            basic_auth: None,
            carrier_acceptors,
            carrier_dialer,
            webserver_log: webserver_log_opt,
            access_logger,
            access_logger_dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            vhost_subdomain: None,
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
        nat_udp_release_timeout: u64,
        max_conns: usize,
        carriers: u16,
        meta: ProviderMeta,
        access_logger: Option<Arc<AccessLogger>>,
    ) -> Result<Self> {
        let endpoint = Endpoint::parse(to);
        let socket = transport::connect(&endpoint, insecure).await?;
        let (opener, acceptor) = mux::client(socket);
        let mut control = Delimited::with_label(
            opener
                .open()
                .await
                .context("failed to open control stream")?,
            "client/provider",
        );

        // Send the registration first so the lazily-opened substream is announced
        // before the (server-initiated) auth handshake (see client `new`). The
        // note and a Basic-auth-enabled flag ride along for the admin page; the
        // credentials themselves never leave this provider.
        control
            .send(ClientMessage::HelloSecret {
                id: tcp_secret_id.to_string(),
                notes: meta.notes.clone(),
                basic_auth: meta.basic_auth.is_some(),
                carriers,
                udp,
                auto_reconnect: meta.auto_reconnect,
                webserver_log: access_logger.is_some(),
                nat_udp_preferred_port: udp_port,
                nat_udp_release_timeout,
                stun_server: stun_server.map(|s| s.to_string()),
                upnp: port_map,
                try_port_prediction: port_prediction,
                max_conns,
                local_host: Some(local_host.to_string()),
                local_port,
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
            Some(_) => bail!("unexpected response to secret registration"),
            None => bail!("unexpected EOF"),
        }
        info!(tcp_secret_id, "registered secret tunnel");

        // Carrier pool: like a public tunnel, if more than one carrier is requested
        // the server replies with a token and how many extra connections to open.
        // Each extra joins the pool; the server round-robins relayed substreams
        // across them. Failures are non-fatal (degrade to those that connected; the
        // re-dial timer tops up later).
        let mut carrier_acceptors = Vec::new();
        let mut carrier_dialer = None;
        if carriers > 1 {
            match control.recv_timeout().await? {
                Some(ServerMessage::CarrierToken { token, extra }) => {
                    for _ in 0..extra {
                        match open_carrier(&endpoint, insecure, secret, &token).await {
                            Ok(pair) => carrier_acceptors.push(pair),
                            Err(err) => warn!(%err, "failed to open carrier connection"),
                        }
                    }
                    info!(
                        opened = carrier_acceptors.len(),
                        requested = extra,
                        "provider carrier pool established"
                    );
                    if extra > 0 {
                        carrier_dialer = Some(CarrierDialer {
                            endpoint: endpoint.clone(),
                            insecure,
                            secret: secret.map(str::to_string),
                            token,
                            target_extra: extra as usize,
                        });
                    }
                }
                Some(ServerMessage::Error(message)) => bail!("server error: {message}"),
                other => bail!("expected carrier token, got {other:?}"),
            }
        }

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
            nat_udp_release_timeout: Duration::from_secs(nat_udp_release_timeout),
        });

        Ok(Client {
            control: Some(control),
            acceptor: Some(acceptor),
            local_host: local_host.to_string(),
            local_port,
            remote_port: 0,
            is_secret_provider: true,
            #[cfg(feature = "udp")]
            udp_socket,
            #[cfg(feature = "udp")]
            secret: secret.map(str::to_string),
            #[cfg(feature = "udp")]
            udp_cfg,
            #[cfg(feature = "udp")]
            vhost_udp: false,
            #[cfg(feature = "udp")]
            direct_endpoint: None,
            #[cfg(feature = "udp")]
            direct_key: None,
            #[cfg(feature = "udp")]
            direct_udp_carriers: 0,
            basic_auth: meta.basic_auth.as_deref().and_then(BasicAuth::parse),
            carrier_acceptors,
            carrier_dialer,
            webserver_log: access_logger.is_some(),
            access_logger,
            access_logger_dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            vhost_subdomain: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    /// Connect to a bore server as a vhost subdomain provider.
    ///
    /// Sends `HelloVhost`, authenticates if a secret is provided, receives
    /// `VhostReady` with the public URL(s), then returns a ready `Client` whose
    /// `listen` loop accepts forwarded yamux substreams and splices them to
    /// `local_host:local_port`.
    pub async fn new_vhost_provider(
        local_host: &str,
        local_port: u16,
        to: &str,
        subdomain: &str,
        client_id: &str,
        secret: Option<&str>,
        insecure: bool,
        carriers: u16,
        meta: ProviderMeta,
        access_logger: Option<Arc<AccessLogger>>,
    ) -> Result<Self> {
        Self::new_vhost_provider_with_udp(
            local_host,
            local_port,
            to,
            subdomain,
            client_id,
            secret,
            insecure,
            carriers,
            false,
            meta,
            access_logger,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    /// Connect to a bore server as a vhost subdomain provider, optionally
    /// requesting the vhost QUIC direct data path.
    pub async fn new_vhost_provider_with_udp(
        local_host: &str,
        local_port: u16,
        to: &str,
        subdomain: &str,
        client_id: &str,
        secret: Option<&str>,
        insecure: bool,
        carriers: u16,
        udp: bool,
        meta: ProviderMeta,
        access_logger: Option<Arc<AccessLogger>>,
    ) -> Result<Self> {
        #[cfg(not(feature = "udp"))]
        if udp {
            warn!("built without udp support; ignoring --udp");
        }

        #[cfg(feature = "udp")]
        if udp && carriers as usize > crate::vhost::MAX_DIRECT_CARRIERS {
            warn!(
                requested = carriers,
                cap = crate::vhost::MAX_DIRECT_CARRIERS,
                "vhost --carriers exceeds the QUIC direct-pool cap; clamping (extra carriers would churn against the server cap)"
            );
        }

        let endpoint = Endpoint::parse(to);
        let socket = transport::connect(&endpoint, insecure).await?;
        let (opener, acceptor) = mux::client(socket);
        let mut control = Delimited::with_label(
            opener
                .open()
                .await
                .context("failed to open control stream")?,
            "client/vhost",
        );

        control
            .send(ClientMessage::HelloVhost {
                subdomain: subdomain.to_string(),
                client_id: client_id.to_string(),
                notes: meta.notes.clone(),
                basic_auth: meta.basic_auth.is_some(),
                carriers,
                udp,
                webserver_log: access_logger.is_some(),
                auto_reconnect: meta.auto_reconnect,
                local_host: Some(local_host.to_string()),
                local_port,
            })
            .await?;

        if let Some(secret) = secret {
            Authenticator::new(secret)
                .client_handshake(&mut control)
                .await?;
        }

        match control.recv_timeout().await? {
            Some(ServerMessage::VhostReady {
                http_url,
                https_url,
            }) => {
                if let Some(url) = &http_url {
                    info!(url, "vhost HTTP endpoint");
                }
                if let Some(url) = &https_url {
                    info!(url, "vhost HTTPS endpoint");
                }
            }
            Some(ServerMessage::Error(message)) => bail!("server error: {message}"),
            Some(ServerMessage::Challenge(_)) => {
                bail!("server requires authentication, but no client secret was provided");
            }
            Some(_) => bail!("unexpected response to vhost registration"),
            None => bail!("unexpected EOF"),
        }
        info!(%subdomain, "vhost provider ready");

        let mut carrier_acceptors = Vec::new();
        let mut carrier_dialer = None;
        if carriers > 1 {
            match control.recv_timeout().await? {
                Some(ServerMessage::CarrierToken { token, extra }) => {
                    for _ in 0..extra {
                        match open_carrier(&endpoint, insecure, secret, &token).await {
                            Ok(pair) => carrier_acceptors.push(pair),
                            Err(err) => warn!(%err, "failed to open vhost carrier connection"),
                        }
                    }
                    info!(
                        opened = carrier_acceptors.len(),
                        requested = extra,
                        "vhost carrier pool established"
                    );
                    if extra > 0 {
                        carrier_dialer = Some(CarrierDialer {
                            endpoint: endpoint.clone(),
                            insecure,
                            secret: secret.map(str::to_string),
                            token,
                            target_extra: extra as usize,
                        });
                    }
                }
                Some(ServerMessage::Error(message)) => bail!("server error: {message}"),
                other => bail!("expected carrier token, got {other:?}"),
            }
        }

        #[cfg(feature = "udp")]
        let (direct_key, direct_endpoint_val, direct_udp_carriers) = if udp {
            (
                Some(subdomain.to_string()),
                Some(endpoint.clone()),
                crate::vhost::clamp_direct_carriers(carriers),
            )
        } else {
            (None, None, 0)
        };

        Ok(Client {
            control: Some(control),
            acceptor: Some(acceptor),
            local_host: local_host.to_string(),
            local_port,
            remote_port: 0,
            is_secret_provider: false,
            #[cfg(feature = "udp")]
            udp_socket: None,
            #[cfg(feature = "udp")]
            secret: secret.map(str::to_string),
            #[cfg(feature = "udp")]
            udp_cfg: None,
            #[cfg(feature = "udp")]
            vhost_udp: udp,
            #[cfg(feature = "udp")]
            direct_endpoint: direct_endpoint_val,
            #[cfg(feature = "udp")]
            direct_key,
            #[cfg(feature = "udp")]
            direct_udp_carriers,
            basic_auth: meta.basic_auth.as_deref().and_then(BasicAuth::parse),
            carrier_acceptors,
            carrier_dialer,
            webserver_log: access_logger.is_some(),
            access_logger,
            access_logger_dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            vhost_subdomain: Some(subdomain.to_string()),
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
        let carrier_acceptors = std::mem::take(&mut self.carrier_acceptors);
        let carrier_dialer = self.carrier_dialer.take();
        #[cfg(feature = "udp")]
        let mut udp_socket = self.udp_socket.take();
        #[cfg(feature = "udp")]
        let secret = self.secret.clone();
        #[cfg(feature = "udp")]
        let vhost_udp = self.vhost_udp;
        #[cfg(not(feature = "udp"))]
        let vhost_udp = false;
        #[cfg(feature = "udp")]
        let direct_endpoint = self.direct_endpoint.clone();
        #[cfg(not(feature = "udp"))]
        let _direct_endpoint: Option<Endpoint> = None;
        #[cfg(feature = "udp")]
        let direct_key = self.direct_key.clone();
        #[cfg(not(feature = "udp"))]
        let direct_key: Option<String> = None;
        // Target width of the QUIC direct carrier pool, and a counter of
        // established-or-establishing carriers so each offer only opens the
        // shortfall (no double-provisioning, no exceeding the target).
        #[cfg(feature = "udp")]
        let direct_udp_target = self.direct_udp_carriers.max(1) as usize;
        #[cfg(feature = "udp")]
        let direct_live = Arc::new(AtomicUsize::new(0));
        // A direct carrier signals here once it is established, so the loop can
        // reset the renewal backoff (success path moved off the listen task).
        #[cfg(feature = "udp")]
        let (direct_up_tx, mut direct_up_rx) = mpsc::unbounded_channel::<()>();
        #[cfg(not(feature = "udp"))]
        let (_direct_up_tx, mut direct_up_rx) = mpsc::unbounded_channel::<()>();
        #[cfg(feature = "udp")]
        let (direct_renew_tx, mut direct_renew_rx) = mpsc::unbounded_channel::<()>();
        #[cfg(not(feature = "udp"))]
        let (_direct_renew_tx, mut direct_renew_rx) = mpsc::unbounded_channel::<()>();
        #[cfg(feature = "udp")]
        let mut direct_renew_backoff = crate::reconnect::Backoff::new_with(2, 32);
        #[cfg(feature = "udp")]
        let mut direct_renew_sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
        #[cfg(not(feature = "udp"))]
        let mut direct_renew_sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
        // Once the direct path is up, later `UdpPunch` messages (a new or
        // reconnecting consumer) are forwarded here to re-punch the NAT.
        #[cfg(feature = "udp")]
        let mut punch_tx: Option<mpsc::UnboundedSender<Vec<SocketAddr>>> = None;
        #[cfg(feature = "udp")]
        let mut udp_reoffer_failures: u32 = 0;
        // Port-release detection state for the preferred UDP port.
        #[cfg(feature = "udp")]
        let mut preferred_port_remapped = false;
        #[cfg(feature = "udp")]
        let mut next_preferred_port_check = tokio::time::Instant::now();
        let is_secret_provider = self.is_secret_provider;
        let this = Arc::new(self);

        // Carrier pool: pump each extra carrier's accepted data substreams into a
        // shared channel the listen loop drains exactly like the main acceptor. A
        // liveness counter drives the re-dial timer that keeps the pool full.
        let (carrier_tx, mut carrier_rx) = mpsc::unbounded_channel::<mux::Stream>();
        let carrier_live = Arc::new(AtomicUsize::new(0));
        for (control_keepalive, acc) in carrier_acceptors {
            spawn_carrier_pump(
                control_keepalive,
                acc,
                carrier_tx.clone(),
                Arc::clone(&carrier_live),
            );
        }
        let carrier_redial_inflight = Arc::new(AtomicBool::new(false));
        let mut carrier_redial = tokio::time::interval(CARRIER_REDIAL_INTERVAL);
        // Retry the provider's UDP candidate offer if the initial one failed, so a
        // transient bootstrap problem does not leave the provider relay-only for
        // the whole session (the consumer already retries; the provider did not).
        // The first tick fires immediately, re-offering at once if needed.
        let mut udp_retry = tokio::time::interval(Duration::from_secs(15));
        // Secret providers ping the server periodically so its recv-deadline
        // reaper never trips on a healthy idle provider (a yamux substream hides
        // a half-open peer). Public/vhost tunnels keep the legacy heartbeat-free
        // path (branch disabled below).
        let mut ctrl_heartbeat = {
            let mut t = tokio::time::interval(crate::secret::CTRL_CLIENT_HEARTBEAT);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t
        };
        loop {
            tokio::select! {
                _ = ctrl_heartbeat.tick(), if is_secret_provider => {
                    if control.send(ClientMessage::Heartbeat).await.is_err() {
                        return Ok(());
                    }
                }
                // Drain the control substream so the server's heartbeats are read;
                // this also surfaces server errors and connection teardown.
                message = control.recv() => {
                    match message? {
                        Some(ServerMessage::Heartbeat) => (),
                        Some(ServerMessage::Error(err)) => error!(%err, "server error"),
                        Some(ServerMessage::Hello(_)) => warn!("unexpected hello"),
                        Some(ServerMessage::CarrierToken { .. }) => warn!("unexpected carrier token"),
                        Some(ServerMessage::Challenge(_)) => warn!("unexpected challenge"),
                        Some(ServerMessage::Ok) => warn!("unexpected ok"),
                        Some(ServerMessage::UdpPunch {
                            nonce,
                            peer,
                            peer_selected_stun,
                            tuning,
                            peer_id: _,
                        }) => {
                            #[cfg(feature = "udp")]
                            {
                                info!(
                                    role = "provider",
                                    peer_candidates = ?peer,
                                    peer_selected_stun = ?peer_selected_stun,
                                    "provider received udp punch request"
                                );
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
                                            provider_direct(
                                                socket,
                                                peer,
                                                token,
                                                tuning,
                                                this,
                                                rx,
                                                permits,
                                            )
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
                                    let _ = tuning;
                                let _ = (nonce, peer, peer_selected_stun);
                                warn!("unexpected udp punch");
                            }
                        }
                        Some(ServerMessage::UdpStunHint { stun_server }) => {
                            warn!(?stun_server, "unexpected udp stun hint")
                        }
                        Some(ServerMessage::UdpUnavailable) => warn!("unexpected udp unavailable"),
                        Some(ServerMessage::TestUdpWaiting) => warn!("unexpected udp diagnostic wait"),
                        Some(ServerMessage::TestUdpStart { .. }) => warn!("unexpected udp diagnostic start"),
                        Some(ServerMessage::VhostReady { .. }) => warn!("unexpected vhost ready"),
                        Some(ServerMessage::VhostUdp { port, nonce, tuning }) => {
                            #[cfg(feature = "udp")]
                            if vhost_udp {
                                let Some(endpoint) = direct_endpoint.as_ref() else {
                                    warn!("vhost udp offer received without an endpoint context");
                                    continue;
                                };
                                let Some(key) = direct_key.as_deref() else {
                                    warn!("vhost udp offer received without a subdomain context");
                                    continue;
                                };

                                let server_addr = match resolve_direct_server_addr(endpoint, port).await {
                                    Ok(addr) => addr,
                                    Err(err) => {
                                        warn!(%err, key, "failed to resolve direct udp endpoint; using TCP relay");
                                        let _ = direct_renew_tx.send(());
                                        continue;
                                    }
                                };
                                let token = crate::holepunch::derive_token(secret.as_deref(), &nonce);

                                let current = direct_live.load(Ordering::Relaxed);
                                let need = direct_udp_target.saturating_sub(current);
                                if need == 0 {
                                    continue;
                                }
                                info!(key, need, target = direct_udp_target, "establishing direct udp carriers");
                                for _ in 0..need {
                                    direct_live.fetch_add(1, Ordering::Relaxed);
                                    spawn_direct(
                                        Arc::clone(&this),
                                        server_addr,
                                        key.to_string(),
                                        token,
                                        tuning,
                                        Arc::clone(&direct_live),
                                        direct_renew_tx.clone(),
                                        direct_up_tx.clone(),
                                    );
                                }
                            } else {
                                warn!("unexpected vhost udp offer");
                            }
                            #[cfg(not(feature = "udp"))]
                            {
                                let _ = (port, nonce, tuning);
                                warn!("unexpected vhost udp offer");
                            }
                        }
                        Some(ServerMessage::PublicUdp { port, nonce, tuning }) => {
                            #[cfg(feature = "udp")]
                            if let Some(key) = direct_key.as_deref() {
                                let Some(endpoint) = direct_endpoint.as_ref() else {
                                    warn!("public udp offer received without an endpoint context");
                                    continue;
                                };

                                let server_addr = match resolve_direct_server_addr(endpoint, port).await {
                                    Ok(addr) => addr,
                                    Err(err) => {
                                        warn!(%err, port, "failed to resolve public udp endpoint; using TCP relay");
                                        let _ = direct_renew_tx.send(());
                                        continue;
                                    }
                                };
                                let token = crate::holepunch::derive_token(secret.as_deref(), &nonce);

                                let current = direct_live.load(Ordering::Relaxed);
                                let need = direct_udp_target.saturating_sub(current);
                                if need == 0 {
                                    continue;
                                }
                                info!(port, need, target = direct_udp_target, "establishing public direct udp carriers");
                                for _ in 0..need {
                                    direct_live.fetch_add(1, Ordering::Relaxed);
                                    spawn_direct(
                                        Arc::clone(&this),
                                        server_addr,
                                        key.to_string(),
                                        token,
                                        tuning,
                                        Arc::clone(&direct_live),
                                        direct_renew_tx.clone(),
                                        direct_up_tx.clone(),
                                    );
                                }
                            } else {
                                warn!("unexpected public udp offer (not a public tunnel with --udp)");
                            }
                            #[cfg(not(feature = "udp"))]
                            {
                                let _ = (port, nonce, tuning);
                                warn!("unexpected public udp offer");
                            }
                        }
                        Some(ServerMessage::VpnReady { .. }) => warn!("unexpected vpn ready"),
                        Some(ServerMessage::VpnError(err)) => error!(%err, "vpn error"),
                        Some(ServerMessage::VpnPeerJoin { .. }) => warn!("unexpected vpn peer join in 1:1 mode"),
                        Some(ServerMessage::VpnPeerLeave { .. }) => warn!("unexpected vpn peer leave in 1:1 mode"),
                        None => return Ok(()),
                    }
                }
                _ = async {
                    if let Some(sleep) = &mut direct_renew_sleep {
                        sleep.as_mut().await;
                    }
                }, if direct_renew_sleep.is_some() => {
                    direct_renew_sleep = None;
                    if let Some(key) = direct_key.as_deref() {
                        if vhost_udp {
                            info!(key, "requesting vhost udp renewal");
                            if control
                                .send(ClientMessage::VhostUdpRenew {
                                    subdomain: key.to_string(),
                                })
                                .await
                                .is_err()
                            {
                                return Ok(());
                            }
                        } else {
                            // Public tunnel: extract port from "port:N" key
                            if let Some(port_str) = key.strip_prefix("port:") {
                                if let Ok(port) = port_str.parse::<u16>() {
                                    info!(port, "requesting public udp renewal");
                                    if control
                                        .send(ClientMessage::PublicUdpRenew { port })
                                        .await
                                        .is_err()
                                    {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }
                renew = direct_renew_rx.recv() => {
                    #[cfg(not(feature = "udp"))]
                    let _ = renew;
                    #[cfg(feature = "udp")]
                    if renew.is_some() && direct_key.is_some() && direct_renew_sleep.is_none() {
                        let delay = direct_renew_backoff.next_delay();
                        info!(
                            key = direct_key.as_deref().unwrap_or_default(),
                            next_retry_s = delay.as_secs(),
                            "scheduling direct udp renewal"
                        );
                        direct_renew_sleep = Some(Box::pin(tokio::time::sleep(delay)));
                    }
                }
                // A direct carrier came up: clear the renewal backoff so a later
                // drop retries promptly, and cancel any pending renewal sleep.
                up = direct_up_rx.recv() => {
                    #[cfg(not(feature = "udp"))]
                    let _ = up;
                    #[cfg(feature = "udp")]
                    if up.is_some() {
                        direct_renew_backoff.reset();
                        direct_renew_sleep = None;
                        while direct_renew_rx.try_recv().is_ok() {}
                    }
                }
                // Periodically re-offer UDP candidates if the provider requested
                // `--udp` but has no active socket yet (initial offer failed and no
                // direct path is up). Bounded by STUN's own short timeouts.
                _ = udp_retry.tick() => {
                    #[cfg(feature = "udp")]
                    if punch_tx.is_none() && udp_socket.is_none() {
                        if let Some(cfg) = this.udp_cfg.as_ref() {
                            // Decide which port to use for this offer attempt.
                            let now = tokio::time::Instant::now();
                            let use_preferred = !preferred_port_remapped
                                || now >= next_preferred_port_check;
                            let effective_port = if use_preferred {
                                cfg.udp_port
                            } else {
                                0u16
                            };
                            if use_preferred && preferred_port_remapped {
                                next_preferred_port_check = now + cfg.nat_udp_release_timeout;
                            }
                            let was_checking_preferred = use_preferred && cfg.udp_port != 0
                                && preferred_port_remapped;
                            match offer_provider_candidates(
                                &mut control,
                                &cfg.endpoint,
                                cfg.stun_server.as_deref(),
                                cfg.port_map,
                                cfg.port_prediction,
                                effective_port,
                            )
                            .await
                            {
                                Ok(socket) => {
                                    udp_socket = Some(socket);
                                    udp_reoffer_failures = 0;
                                    // When checking the preferred port, verify preservation.
                                    if was_checking_preferred {
                                        resolve_stun_and_check(
                                            &cfg.endpoint, cfg.stun_server.as_deref(),
                                            cfg.udp_port, cfg.nat_udp_release_timeout,
                                            &mut preferred_port_remapped,
                                        ).await;
                                    } else if effective_port != 0 && !preferred_port_remapped {
                                        let was_remapped = resolve_stun_and_check(
                                            &cfg.endpoint, cfg.stun_server.as_deref(),
                                            cfg.udp_port, cfg.nat_udp_release_timeout,
                                            &mut preferred_port_remapped,
                                        ).await;
                                        if !was_remapped {
                                            info!("provider udp candidate offer succeeded on retry");
                                        }
                                    } else {
                                        info!("provider udp candidate offer succeeded on retry");
                                    }
                                }
                                Err(err) => {
                                    udp_reoffer_failures += 1;
                                    if udp_reoffer_failures == 1 {
                                        warn!(%err, "provider udp re-offer failed; will retry in 15s");
                                    } else {
                                        debug!(%err, "provider udp re-offer failed (attempt {})", udp_reoffer_failures);
                                    }
                                }
                            }
                        }
                    }
                }
                // A data substream from one of the extra carrier connections. We
                // hold `carrier_tx`, so `recv` never yields `None` spuriously; a
                // dead carrier just stops feeding (and is re-dialed below).
                stream = carrier_rx.recv() => {
                    if let Some(stream) = stream {
                        spawn_handle(&this, stream);
                    }
                }
                // Keep the carrier pool topped up: if any carrier dropped, re-dial
                // the shortfall off the loop so accept/forward never stalls.
                _ = carrier_redial.tick() => {
                    if let Some(dialer) = &carrier_dialer {
                        maybe_redial_carriers(
                            dialer,
                            &carrier_tx,
                            &carrier_live,
                            &carrier_redial_inflight,
                        );
                    }
                }
                stream = acceptor.accept() => {
                    let Some(stream) = stream else {
                        return Ok(());
                    };
                    spawn_handle(&this, stream);
                }
            }
        }
    }

    /// Splice one forwarded stream (a yamux substream on the relay path, or a
    /// native QUIC bidi on the direct path) to a fresh local connection. Generic
    /// over the carrier stream so both data paths share the same logic.
    async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut stream: S,
    ) -> Result<()> {
        // Read the server's readiness marker with optional caller IP forwarding (Phase 3).
        let real_ip = mux::read_stream_ready(&mut stream, self.webserver_log).await?;

        // Enforce HTTP Basic auth (secret-tunnel providers only). The gate reads
        // the request head off the substream; on success that head is replayed to
        // the local service, on failure a 401 was already returned to the visitor.
        let prefix = if let Some(auth) = &self.basic_auth {
            match basicauth::gate(&mut stream, auth).await? {
                Gate::Forward(prefix) => prefix,
                Gate::Reject => return Ok(()),
            }
        } else {
            Vec::new()
        };

        let mut local_conn = connect_with_timeout(&self.local_host, self.local_port).await?;
        if !prefix.is_empty() {
            local_conn.write_all(&prefix).await?;
        }
        let buf = proxy_buffer_size();

        // Wrap with access tap if logging is enabled.
        if let Some(logger) = &self.access_logger {
            // Determine the log key and layout based on tunnel type.
            let (key, layout) = match &self.vhost_subdomain {
                Some(sub) => (sub.clone(), PathLayout::Flat),
                None => (self.remote_port.to_string(), PathLayout::Flat),
            };

            let tx = logger.sender_for(&key, layout);
            let mut tap = crate::weblog::HttpAccessTap::new(
                stream,
                real_ip.filter(|s| !s.is_empty()),
                tx,
                Arc::clone(&self.access_logger_dropped),
            );
            tokio::io::copy_bidirectional_with_sizes(&mut local_conn, &mut tap, buf, buf).await?;
        } else {
            // No logging: use stream directly.
            tokio::io::copy_bidirectional_with_sizes(&mut local_conn, &mut stream, buf, buf)
                .await?;
        }

        Ok(())
    }
}

/// Spawn a task that dials the local service for a forwarded substream and splices
/// the two together. Shared by the main acceptor and every carrier-pool acceptor
/// so they handle a forwarded connection identically.
fn spawn_handle(this: &Arc<Client>, stream: mux::Stream) {
    let this = Arc::clone(this);
    tokio::spawn(
        async move {
            info!("new connection");
            match this.handle_connection(stream).await {
                // Per-connection success is high-frequency, low-value: log it at
                // trace, matching the server and secret-relay paths (which trace
                // their per-connection closes). The arrival above stays at info.
                Ok(_) => trace!("connection exited"),
                Err(err) => warn!(%err, "connection exited with error"),
            }
        }
        .instrument(info_span!("proxy")),
    );
}

/// Resolve the public UDP address to dial for the server's direct QUIC endpoint.
/// Used by both vhost providers and public tunnels. The direct path currently
/// uses the IPv4 UDP stack, so prefer IPv4 answers and fail fast if none are available.
#[cfg(feature = "udp")]
async fn resolve_direct_server_addr(endpoint: &Endpoint, port: u16) -> Result<SocketAddr> {
    tokio::net::lookup_host((endpoint.host.as_str(), port))
        .await
        .with_context(|| {
            format!(
                "failed to resolve direct udp endpoint {}:{port}",
                endpoint.host
            )
        })?
        .find(SocketAddr::is_ipv4)
        .context("resolved no IPv4 address for the direct udp endpoint")
}

/// Establish one QUIC direct carrier toward the server and serve its
/// accepted streams to the local service. Works for both vhost providers and
/// public tunnels (server opens streams; client accepts them).
///
/// One of these runs per pooled carrier. The caller has already reserved the slot
/// in `live` (incremented before spawning); this task releases it (`live -= 1`)
/// and signals `renew_tx` whenever the carrier fails to establish or later closes,
/// so the listen loop re-requests an offer and tops the pool back up. On a
/// successful connect it signals `up_tx` so the loop resets its renewal backoff.
#[cfg(feature = "udp")]
#[allow(clippy::too_many_arguments)]
fn spawn_direct(
    client: Arc<Client>,
    server_addr: SocketAddr,
    key: String,
    token: [u8; crate::holepunch::TOKEN_LEN],
    tuning: crate::shared::UdpDirectTuning,
    live: Arc<AtomicUsize>,
    renew_tx: mpsc::UnboundedSender<()>,
    up_tx: mpsc::UnboundedSender<()>,
) {
    tokio::spawn(async move {
        // Release the reserved slot and ask for a renewal. Used on every exit
        // path (bind/connect failure or a later carrier close).
        let release = |live: &AtomicUsize, renew_tx: &mpsc::UnboundedSender<()>| {
            live.fetch_sub(1, Ordering::Relaxed);
            let _ = renew_tx.send(());
        };

        let socket = match crate::holepunch::bind_socket(0).await {
            Ok(socket) => socket,
            Err(err) => {
                warn!(%err, key, "failed to bind direct udp socket; using TCP relay");
                release(&live, &renew_tx);
                return;
            }
        };
        let direct =
            match crate::holepunch::vhost_connect(socket, server_addr, &key, token, tuning).await {
                Ok(direct) => direct,
                Err(err) => {
                    warn!(%err, key, "direct udp carrier unavailable; using TCP relay");
                    release(&live, &renew_tx);
                    return;
                }
            };

        let _ = up_tx.send(());
        info!(key, "direct udp carrier ready, accepting streams");
        loop {
            let stream = match direct.accept_stream().await {
                Ok(stream) => stream,
                Err(err) => {
                    debug!(%err, key, "direct udp carrier closed");
                    break;
                }
            };

            let client = Arc::clone(&client);
            tokio::spawn(async move {
                debug!("serving local connection over direct udp path");
                if let Err(err) = client.handle_connection(stream).await {
                    warn!(%err, "direct connection closed with error");
                }
            });
        }
        release(&live, &renew_tx);
    });
}

/// Open one extra carrier connection for a public tunnel's pool: dial the server,
/// present the carrier `token`, and authenticate (if a secret is set), mirroring
/// the order [`Client::new`] uses. Returns the held-open control substream (kept
/// alive so the server keeps this carrier in the pool) and the data acceptor.
async fn open_carrier(
    endpoint: &Endpoint,
    insecure: bool,
    secret: Option<&str>,
    token: &str,
) -> Result<(Delimited<mux::Stream>, mux::Acceptor)> {
    let socket = transport::connect(endpoint, insecure).await?;
    let (opener, acceptor) = mux::client(socket);
    let mut control = Delimited::with_label(
        opener
            .open()
            .await
            .context("failed to open carrier control stream")?,
        "client/relay-carrier",
    );
    // Send JoinCarrier first to announce the lazily-opened substream (see `new`),
    // then complete the auth challenge if the server requires a secret.
    control
        .send(ClientMessage::JoinCarrier {
            token: token.to_string(),
        })
        .await?;
    if let Some(secret) = secret {
        Authenticator::new(secret)
            .client_handshake(&mut control)
            .await?;
    }
    Ok((control, acceptor))
}

/// Pump a carrier connection's accepted data substreams into the shared channel
/// until the connection drops. Holds the control substream open for the carrier's
/// lifetime (the server uses it to keep the carrier in the pool) and maintains the
/// liveness counter so [`maybe_redial_carriers`] can top the pool back up.
fn spawn_carrier_pump(
    control: Delimited<mux::Stream>,
    mut acceptor: mux::Acceptor,
    tx: mpsc::UnboundedSender<mux::Stream>,
    live: Arc<AtomicUsize>,
) {
    live.fetch_add(1, Ordering::Relaxed);
    tokio::spawn(async move {
        // Held only to keep the substream (and thus the carrier) open; never read.
        let _control = control;
        while let Some(stream) = acceptor.accept().await {
            if tx.send(stream).is_err() {
                break;
            }
        }
        live.fetch_sub(1, Ordering::Relaxed);
    });
}

/// If the carrier pool has dropped below its target width, re-dial the shortfall in
/// a spawned task so the listen loop never blocks on the dial. `inflight` prevents
/// stacking re-dial batches when a carrier stays unreachable.
fn maybe_redial_carriers(
    dialer: &CarrierDialer,
    tx: &mpsc::UnboundedSender<mux::Stream>,
    live: &Arc<AtomicUsize>,
    inflight: &Arc<AtomicBool>,
) {
    let current = live.load(Ordering::Relaxed);
    if current >= dialer.target_extra {
        return;
    }
    // Skip if a previous re-dial batch is still running.
    if inflight.swap(true, Ordering::AcqRel) {
        return;
    }
    let need = dialer.target_extra - current;
    let dialer = dialer.clone();
    let tx = tx.clone();
    let live = Arc::clone(live);
    let inflight = Arc::clone(inflight);
    tokio::spawn(async move {
        for _ in 0..need {
            match open_carrier(
                &dialer.endpoint,
                dialer.insecure,
                dialer.secret.as_deref(),
                &dialer.token,
            )
            .await
            {
                Ok((control, acceptor)) => {
                    spawn_carrier_pump(control, acceptor, tx.clone(), Arc::clone(&live));
                    info!("re-dialed a carrier connection");
                }
                Err(err) => {
                    debug!(%err, "carrier re-dial failed; will retry");
                    break;
                }
            }
        }
        inflight.store(false, Ordering::Release);
    });
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
    let socket = holepunch::bind_socket(udp_port).await?;
    let local_addr = socket.local_addr().ok();
    let stun_chain = holepunch::live_stun_target_names(&endpoint.host, endpoint.port, stun_server);
    info!(
        role = "provider",
        udp_local_addr = ?local_addr,
        requested_udp_port = udp_port,
        stun_override = stun_server.is_some(),
        stun_chain = ?stun_chain,
        "provider UDP candidate discovery configured"
    );
    let stun_targets = match holepunch::resolve_live_stun_targets(
        &endpoint.host,
        endpoint.port,
        stun_server,
    )
    .await
    {
        Ok(targets) => targets,
        Err(err) => {
            warn!(%err, "no STUN targets resolved for provider; offering non-STUN candidates only");
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
            "STUN returned no candidate addresses".to_string()
        };
        bail!(
            "no UDP candidates for provider: {stun_info} \
             (port_map={port_map}, port_prediction={port_prediction}, \
             local_addr={}); direct path unavailable, using relay",
            discovery_local_addr
                .map(|a| a.to_string())
                .unwrap_or_default(),
        );
    }
    info!(
        role = "provider",
        udp_local_addr = ?discovery_local_addr,
        selected_stun = selected_stun_name,
        selected_stun_addr = ?selected_stun_addr,
        stun_source,
        reflexive = ?reflexive,
        attempted_stun,
        ?candidates,
        "provider offering udp candidates"
    );
    control
        .send(ClientMessage::UdpCandidateOffer(UdpCandidateOffer {
            candidates,
            selected_stun: selected_stun_owned,
            peer_id: 0,
        }))
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
    tuning: crate::shared::UdpDirectTuning,
    client: Arc<Client>,
    mut punch_rx: mpsc::UnboundedReceiver<Vec<SocketAddr>>,
    permits: Arc<Semaphore>,
) -> Result<()> {
    let listener = crate::holepunch::DirectListener::new(socket, peers, tuning).await?;
    info!("direct udp path ready, accepting connections");
    loop {
        tokio::select! {
            res = listener.accept(token) => {
                match res {
                    Ok(conn) => {
                        // Each proxied connection rides its own native QUIC stream
                        // (no yamux): accept them and dial the local service per
                        // stream. The consumer opens the streams.
                        let client = Arc::clone(&client);
                        let permits = Arc::clone(&permits);
                        tokio::spawn(async move {
                            loop {
                                let stream = match conn.accept_stream().await {
                                    Ok(stream) => stream,
                                    // Connection closed (consumer gone): stop.
                                    Err(err) => {
                                        trace!(%err, "direct connection closed");
                                        break;
                                    }
                                };
                                // Bound concurrently served direct streams, the
                                // direct-path analog of the relay's `--max-conns`;
                                // over the cap, drop the stream (as the relay does).
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
                    // `accept()` now swallows benign hole-punch strays internally
                    // (BUG-S3), so reaching here means an endpoint-level problem
                    // (the QUIC endpoint closed). Back off briefly before retrying;
                    // intentional teardown is handled by the `punch_rx` `None` arm.
                    // Logged at debug — a closed endpoint during teardown is normal.
                    Err(err) => {
                        debug!(%err, "direct udp endpoint accept error; retrying in 100ms");
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

/// Resolve one STUN target, probe the preferred port, and update the remap
/// flag. Returns true if the port was found to be remapped.
#[cfg(feature = "udp")]
async fn resolve_stun_and_check(
    endpoint: &Endpoint,
    stun_server: Option<&str>,
    preferred_port: u16,
    release_timeout: Duration,
    preferred_port_remapped: &mut bool,
) -> bool {
    let stun_chain =
        crate::holepunch::live_stun_target_names(&endpoint.host, endpoint.port, stun_server);
    let Some(first) = stun_chain.into_iter().next() else {
        return false;
    };
    let Ok(mut addrs) = tokio::net::lookup_host(&first).await else {
        return false;
    };
    let Some(addr) = addrs.find(|a| a.is_ipv4()) else {
        return false;
    };
    match crate::holepunch::check_reflexive_port(preferred_port, addr).await {
        Some(true) => {
            *preferred_port_remapped = false;
            info!(
                port = preferred_port,
                "preferred port :{preferred_port} is now PRESERVED on NAT",
            );
            false
        }
        Some(false) => {
            *preferred_port_remapped = true;
            info!(
                port = preferred_port,
                recheck_s = release_timeout.as_secs(),
                "port :{preferred_port} REMAPPED by NAT; \
                 switching to ephemeral, will re-check in {:?}",
                release_timeout,
            );
            true
        }
        None => {
            debug!(
                port = preferred_port,
                "STUN check for preferred port :{preferred_port} failed (unreachable)",
            );
            *preferred_port_remapped
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
