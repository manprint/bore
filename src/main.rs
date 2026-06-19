use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result};
#[cfg(all(feature = "vpn", target_os = "linux"))]
use bore_cli::shared::{AdvertiseEntry, Ipv4Net, VpnAddrRequest};
#[cfg(all(feature = "vpn", target_os = "linux"))]
use bore_cli::vpn;
use bore_cli::{
    client::{Client, ProviderMeta},
    reconnect,
    secret::Proxy,
    server::Server,
    shared::{TunnelOptions, UdpDirectTuning, UdpTestOptions, MAX_DIRECT_STREAMS, MAX_NOTES_LEN},
    transfer::{
        CollisionPolicy, DeviceMode, ListenerOptions as TransferListenerOptions,
        SenderOptions as TransferSenderOptions, SymlinkMode,
    },
};
use clap::{error::ErrorKind, ArgAction, CommandFactory, Parser, Subcommand};
use tracing::{info, warn};

/// Full version string: "bore 1.0.0 - <branch> - <sha8>".
const FULL_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " - ",
    env!("GIT_BRANCH"),
    " - ",
    env!("GIT_SHA_SHORT"),
);
const DEFAULT_SERVER: &str = "https://bore.0912345.xyz";

#[derive(Parser, Debug)]
#[clap(name = "bore", author, version = FULL_VERSION, about)]
struct Args {
    /// Increase log verbosity: -v for debug, -vv for trace (RUST_LOG overrides).
    #[clap(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,

    #[clap(subcommand)]
    command: Command,
}

// The `Server` variant carries many CLI options and is larger than the others.
// This enum is parsed exactly once at startup and immediately destructured, so the
// per-variant size never matters (boxing would only obscure the dispatch).
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
enum Command {
    /// Starts a local proxy to the remote server.
    Local {
        /// The local port to expose.
        #[clap(value_name = "PORT", env = "BORE_LOCAL_PORT")]
        local_port: u16,

        /// The local host to expose.
        #[clap(short, long, value_name = "HOST", default_value = "localhost")]
        local_host: String,

        /// Address of the remote server to expose local ports to.
        #[clap(short, long, value_name = "ADDR", env = "BORE_SERVER", default_value = DEFAULT_SERVER)]
        to: String,

        /// Optional port on the remote server to select.
        #[clap(short, long, value_name = "PORT", default_value_t = 0)]
        port: u16,

        /// Optional secret for authentication.
        #[clap(
            short,
            long,
            value_name = "SECRET",
            env = "BORE_SECRET",
            hide_env_values = true
        )]
        secret: Option<String>,

        /// Register as a named secret tunnel (reached via `bore proxy` with the same
        /// id) instead of allocating a public port; --port is then ignored.
        #[clap(long, value_name = "ID", env = "BORE_TCP_SECRET_ID")]
        tcp_secret_id: Option<String>,

        /// Skip TLS certificate verification (for self-signed https:// servers).
        #[clap(long, env = "BORE_INSECURE")]
        insecure: bool,

        /// Terminate TLS on the tunnel port, so it is reachable over https://
        /// (the server must have a certificate). Plain and raw access still work.
        #[clap(long, env = "BORE_HTTPS")]
        https: bool,

        /// Redirect plain HTTP requests on the tunnel port to https:// (requires
        /// --https). Raw TCP and https:// keep working.
        #[clap(long, requires = "https", env = "BORE_FORCE_HTTPS")]
        force_https: bool,

        /// Prefer a direct UDP/QUIC data path; falls back to the server relay if
        /// unavailable. Public tunnels use a server→client QUIC path (server is
        /// public, no hole-punch; needs `bore server --udp`). Secret tunnels
        /// (`--tcp-secret-id`) use a peer-to-peer hole-punched path. `--carriers N`
        /// opens N independent QUIC connections.
        #[clap(long, env = "BORE_PREFER_UDP")]
        udp: bool,

        /// STUN server (host:port) for UDP candidate discovery. Overrides the
        /// default chain (Cloudflare, Google, then the bore server's UDP control
        /// endpoint).
        #[clap(long, value_name = "HOST:PORT", env = "BORE_STUN_SERVER")]
        stun_server: Option<String>,

        /// Try UPnP-IGD to map a port on the local router (helps strict home
        /// routers; no effect behind carrier-grade NAT). Direct UDP path only.
        #[clap(long, env = "BORE_UPNP")]
        upnp: bool,

        /// Also advertise predicted symmetric-NAT ports as hole-punch candidates.
        /// Opt-in, best-effort: it may look like a port scan to strict firewalls.
        #[clap(long, env = "BORE_TRY_PORT_PREDICTION")]
        try_port_prediction: bool,

        /// Bind the UDP hole-punch socket to this fixed port instead of a random
        /// one. Open it for egress in a strict firewall (and use the same value on
        /// the peer) to allow the direct path; on a port-preserving NAT it also
        /// makes the public mapping predictable. 0 = random. Direct path only.
        #[clap(
            long,
            value_name = "PORT",
            env = "BORE_NAT_UDP_PORT",
            default_value_t = 0
        )]
        nat_udp_preferred_port: u16,

        /// How long (seconds) to wait before re-checking the preferred UDP port
        /// after the NAT remapped it. During this window ephemeral ports are used
        /// so the NAT entry for the preferred port expires naturally. 0 = disable.
        #[clap(
            long,
            value_name = "SECS",
            env = "BORE_NAT_UDP_RELEASE_TIMEOUT",
            default_value_t = bore_cli::shared::NAT_UDP_RELEASE_TIMEOUT,
        )]
        nat_udp_release_timeout: u64,

        /// Maximum concurrent connections served over a direct UDP path (the
        /// direct-path analog of the server's --max-conns; bounds this provider's
        /// resources). Secret-tunnel providers only.
        #[clap(long, value_name = "N", default_value_t = bore_cli::server::DEFAULT_MAX_CONNS, env = "BORE_MAX_CONNS")]
        max_conns: usize,

        /// Protect the tunnel with HTTP Basic auth ("user:pass"): HTTP requests
        /// without valid credentials get a 401. Public tunnels are enforced on the
        /// server; secret tunnels on this provider. Non-HTTP traffic is unaffected.
        #[clap(
            long,
            value_name = "USER:PASS",
            env = "BORE_BASIC_AUTH",
            hide_env_values = true
        )]
        basic_auth: Option<String>,

        /// Free-form note shown on the server's admin status page (no behaviour).
        #[clap(long, value_name = "TEXT", env = "BORE_NOTES")]
        notes: Option<String>,

        /// Number of parallel TCP carrier connections for the relay data path.
        /// Public tunnels spread inbound proxied connections across them; secret
        /// providers (`--tcp-secret-id`) spread relayed consumer substreams
        /// across them. 1 keeps the current single-connection behaviour. >1
        /// avoids head-of-line blocking under concurrency; server-managed pools
        /// are capped by `bore server --max-carriers`. Direct UDP ignores it.
        #[clap(long, value_name = "N", default_value_t = 1, env = "BORE_CARRIERS")]
        carriers: u16,

        /// Reconnect automatically with backoff if the connection fails or drops.
        #[clap(long, env = "BORE_AUTO_RECONNECT")]
        auto_reconnect: bool,
    },

    /// Connects to a named secret tunnel and exposes it on a local port.
    Proxy {
        /// Local address to listen on for the proxied service, e.g. ":5555" (all
        /// interfaces) or "127.0.0.1:5555".
        #[clap(long, value_name = "ADDR", env = "BORE_LOCAL_PROXY_PORT")]
        local_proxy_port: String,

        /// Address of the remote server hosting the secret tunnel.
        #[clap(short, long, value_name = "ADDR", env = "BORE_SERVER", default_value = DEFAULT_SERVER)]
        to: String,

        /// Optional secret for authentication.
        #[clap(
            short,
            long,
            value_name = "SECRET",
            env = "BORE_SECRET",
            hide_env_values = true
        )]
        secret: Option<String>,

        /// Identifier of the secret tunnel to connect to (must match the provider).
        #[clap(long, value_name = "ID", env = "BORE_TCP_SECRET_ID")]
        tcp_secret_id: String,

        /// Skip TLS certificate verification (for self-signed https:// servers).
        #[clap(long, env = "BORE_INSECURE")]
        insecure: bool,

        /// Prefer a direct UDP hole-punched path; falls back to the server relay
        /// if unavailable.
        #[clap(long, env = "BORE_PREFER_UDP")]
        udp: bool,

        /// STUN server (host:port) for UDP candidate discovery. Overrides the
        /// provider-selected STUN hint and the default chain (Cloudflare, Google,
        /// then the bore server's UDP control endpoint).
        #[clap(long, value_name = "HOST:PORT", env = "BORE_STUN_SERVER")]
        stun_server: Option<String>,

        /// Try UPnP-IGD to map a port on the local router (helps strict home
        /// routers; no effect behind carrier-grade NAT). Direct UDP path only.
        #[clap(long, env = "BORE_UPNP")]
        upnp: bool,

        /// Also advertise predicted symmetric-NAT ports as hole-punch candidates.
        /// Opt-in, best-effort: it may look like a port scan to strict firewalls.
        #[clap(long, env = "BORE_TRY_PORT_PREDICTION")]
        try_port_prediction: bool,

        /// Bind the UDP hole-punch socket to this fixed port instead of a random
        /// one. Open it for egress in a strict firewall (and use the same value on
        /// the peer) to allow the direct path; on a port-preserving NAT it also
        /// makes the public mapping predictable. 0 = random. Direct path only.
        #[clap(
            long,
            value_name = "PORT",
            env = "BORE_NAT_UDP_PORT",
            default_value_t = 0
        )]
        nat_udp_preferred_port: u16,

        /// How long (seconds) to wait before re-checking the preferred UDP port
        /// after the NAT remapped it. During this window ephemeral ports are used
        /// so the NAT entry for the preferred port expires naturally. 0 = disable.
        #[clap(
            long,
            value_name = "SECS",
            env = "BORE_NAT_UDP_RELEASE_TIMEOUT",
            default_value_t = bore_cli::shared::NAT_UDP_RELEASE_TIMEOUT,
        )]
        nat_udp_release_timeout: u64,

        /// Free-form note shown on the server's admin status page (no behaviour).
        #[clap(long, value_name = "TEXT", env = "BORE_NOTES")]
        notes: Option<String>,

        /// Number of parallel TCP carrier connections for the relay data path
        /// (consumer→server). 1 = single connection. >1 spreads forwarded
        /// connections across several TCP streams to avoid head-of-line blocking
        /// under concurrency. Applies to the relay path; the direct UDP path
        /// (`--udp`) already uses independent QUIC streams.
        #[clap(long, value_name = "N", default_value_t = 1, env = "BORE_CARRIERS")]
        carriers: u16,

        /// Reconnect automatically with backoff if the connection fails or drops.
        #[clap(long, env = "BORE_AUTO_RECONNECT")]
        auto_reconnect: bool,
    },

    /// Expose a local service via HTTP(S) subdomain routing on the remote server.
    ///
    /// The server's public HTTP/HTTPS frontend routes requests by Host header to
    /// the registered subdomain and forwards them to the local target.
    ///
    /// Example: bore vhost 127.0.0.1:8080 --subdomain myapp --id my-client-id
    ///   → https://myapp.bore.mydomain.com
    Vhost {
        /// Local host:port to forward requests to.
        #[clap(value_name = "TARGET")]
        target: String,

        /// Subdomain label to register (e.g. `myapp` → `myapp.bore.mydomain.com`).
        #[clap(long, value_name = "LABEL", env = "BORE_VHOST_SUBDOMAIN")]
        subdomain: String,

        /// Client identifier for reservation matching in the server's vhost.yml.
        #[clap(long, value_name = "ID", env = "BORE_VHOST_ID")]
        id: String,

        /// Address of the remote bore server.
        #[clap(short, long, value_name = "ADDR", env = "BORE_SERVER", default_value = DEFAULT_SERVER)]
        to: String,

        /// Optional secret for authentication.
        #[clap(
            short,
            long,
            value_name = "SECRET",
            env = "BORE_SECRET",
            hide_env_values = true
        )]
        secret: Option<String>,

        /// Skip TLS certificate verification (for self-signed https:// servers).
        #[clap(long, env = "BORE_INSECURE")]
        insecure: bool,

        /// Free-form note shown on the server's admin status page (no behaviour).
        #[clap(long, value_name = "TEXT", env = "BORE_NOTES")]
        notes: Option<String>,

        /// Whether the provider enforces HTTP Basic auth itself (display-only flag).
        #[clap(
            long,
            value_name = "USER:PASS",
            env = "BORE_BASIC_AUTH",
            hide_env_values = true
        )]
        basic_auth: Option<String>,

        /// Number of parallel carrier connections for the data path. Spreads
        /// proxied connections across N transports so per-connection congestion
        /// (and yamux head-of-line) is isolated. Sizes the TCP relay carrier pool
        /// (capped by `bore server --max-carriers`); with `--udp` it ALSO sets how
        /// many parallel QUIC direct connections the provider opens. Only helps
        /// when many connections run concurrently — a single transfer rides one
        /// connection regardless.
        #[clap(long, value_name = "N", default_value_t = 1, env = "BORE_CARRIERS")]
        carriers: u16,

        /// Prefer a direct QUIC data path for the server→provider vhost hop;
        /// falls back to the TCP carrier relay if UDP is unavailable. With
        /// `--carriers N` the provider opens N parallel QUIC connections and the
        /// server round-robins proxied requests across them.
        #[clap(long, env = "BORE_VHOST_UDP")]
        udp: bool,

        /// Reconnect automatically with backoff if the connection fails or drops.
        #[clap(long, env = "BORE_AUTO_RECONNECT")]
        auto_reconnect: bool,
    },

    /// Secure file and directory transfer over secret tunnels.
    Transfer {
        #[clap(subcommand)]
        command: TransferCommand,
    },

    /// Linux point-to-point VPN overlay (requires --features vpn; needs root / CAP_NET_ADMIN).
    #[cfg(all(feature = "vpn", target_os = "linux"))]
    Vpn {
        #[clap(subcommand)]
        command: VpnCommand,
    },

    /// Runs the remote proxy server.
    Server {
        /// Minimum accepted TCP port number.
        #[clap(
            long,
            value_name = "PORT",
            default_value_t = 1024,
            env = "BORE_MIN_PORT"
        )]
        min_port: u16,

        /// Maximum accepted TCP port number.
        #[clap(
            long,
            value_name = "PORT",
            default_value_t = 65535,
            env = "BORE_MAX_PORT"
        )]
        max_port: u16,

        /// Optional secret for authentication.
        #[clap(
            short,
            long,
            value_name = "SECRET",
            env = "BORE_SECRET",
            hide_env_values = true
        )]
        secret: Option<String>,

        /// Maximum number of concurrently proxied connections per client.
        #[clap(long, value_name = "N", default_value_t = bore_cli::server::DEFAULT_MAX_CONNS, env = "BORE_MAX_CONNS")]
        max_conns: usize,

        /// Maximum number of parallel TCP carrier connections the server grants
        /// to one server-managed carrier pool (public tunnels, secret providers,
        /// vhost providers). Does not cap `bore proxy`, whose relay carriers are
        /// client-side `ConnectSecret` connections. 1 disables server-managed
        /// carrier pools.
        #[clap(long, value_name = "N", default_value_t = bore_cli::server::DEFAULT_MAX_CARRIERS, env = "BORE_MAX_CARRIERS")]
        max_carriers: u16,

        /// TCP port the control connection listens on.
        #[clap(long, value_name = "PORT", default_value_t = bore_cli::shared::CONTROL_PORT, env = "BORE_CONTROL_PORT")]
        control_port: u16,

        /// Public domain advertised to clients (informational).
        #[clap(long, value_name = "DOMAIN", env = "BORE_BIND_DOMAIN")]
        bind_domain: Option<String>,

        /// Path to a TLS certificate chain (PEM). With --key-file, serves HTTPS.
        #[clap(long, value_name = "PATH", env = "BORE_CERT_FILE")]
        cert_file: Option<String>,

        /// Path to the TLS private key (PEM). With --cert-file, serves HTTPS.
        #[clap(long, value_name = "PATH", env = "BORE_KEY_FILE")]
        key_file: Option<String>,

        /// IP address to bind to, clients must reach this.
        #[clap(long, value_name = "IP", default_value = "0.0.0.0")]
        bind_addr: IpAddr,

        /// IP address where tunnels will listen on, defaults to --bind-addr.
        #[clap(long, value_name = "IP")]
        bind_tunnels: Option<IpAddr>,

        /// Broker UDP direct (hole-punched) paths for secret tunnels and run a
        /// STUN responder on the control port.
        #[clap(long, env = "BORE_UDP")]
        udp: bool,

        /// QUIC receive window per direct-UDP stream on the server-brokered
        /// direct path. Accepts raw bytes or KB/MB/GB/KiB/MiB/GiB suffixes.
        #[clap(
            long,
            value_name = "SIZE",
            default_value = "16MiB",
            env = "BORE_UDP_STREAM_RECEIVE_WINDOW"
        )]
        udp_stream_receive_window: String,

        /// Aggregate QUIC receive window per direct-UDP connection. Accepts raw
        /// bytes or KB/MB/GB/KiB/MiB/GiB suffixes.
        #[clap(
            long,
            value_name = "SIZE",
            default_value = "64MiB",
            env = "BORE_UDP_CONNECTION_RECEIVE_WINDOW"
        )]
        udp_connection_receive_window: String,

        /// QUIC send window for the server-brokered direct UDP path. Accepts raw
        /// bytes or KB/MB/GB/KiB/MiB/GiB suffixes.
        #[clap(
            long,
            value_name = "SIZE",
            default_value = "64MiB",
            env = "BORE_UDP_SEND_WINDOW"
        )]
        udp_send_window: String,

        /// UDP socket receive buffer requested by the server for direct UDP.
        /// Accepts raw bytes or KB/MB/GB/KiB/MiB/GiB suffixes.
        #[clap(
            long,
            value_name = "SIZE",
            default_value = "16MiB",
            env = "BORE_UDP_SOCKET_RECV_BUFFER"
        )]
        udp_socket_recv_buffer: String,

        /// UDP socket send buffer requested by the server for direct UDP.
        /// Accepts raw bytes or KB/MB/GB/KiB/MiB/GiB suffixes.
        #[clap(
            long,
            value_name = "SIZE",
            default_value = "16MiB",
            env = "BORE_UDP_SOCKET_SEND_BUFFER"
        )]
        udp_socket_send_buffer: String,

        /// Max native QUIC bidi streams the server allows on a direct UDP
        /// connection. 4096 matches the current default.
        #[clap(long, value_name = "N", default_value_t = MAX_DIRECT_STREAMS, env = "BORE_UDP_MAX_STREAMS")]
        udp_max_streams: u32,

        /// Enable the admin status page at /admin/status on the control port,
        /// guarded by this token (min 32 chars). Unset = the page is disabled.
        #[clap(
            long,
            value_name = "TOKEN",
            env = "BORE_ADMIN_TOKEN",
            hide_env_values = true
        )]
        admin_token: Option<String>,

        /// HSTS value served on HTTPS control-port HTTP responses (admin page,
        /// vhost-miss 404 on the control port). Use `off` to disable.
        #[clap(
            long,
            value_name = "VALUE|off",
            default_value = bore_cli::server::DEFAULT_CONTROL_HSTS,
            env = "BORE_CONTROL_HSTS"
        )]
        control_hsts: String,

        /// Path to a vhost.yml config file. Optional: the vhost frontend
        /// (subdomain-routed reverse proxy) is enabled by either this file or
        /// --vhost-base-domain. A file is only needed for reservations and
        /// default_headers/default_response_headers; everything else can be set
        /// via the flags below.
        #[clap(long, value_name = "PATH", env = "BORE_VHOST_CONFIG")]
        vhost_config: Option<PathBuf>,

        /// Base domain for the vhost frontend, e.g. `bore.mydomain.com`. Enables
        /// vhost without a config file, and overrides `base_domain` from the file
        /// when both are set.
        #[clap(long, value_name = "DOMAIN", env = "BORE_VHOST_BASE_DOMAIN")]
        vhost_base_domain: Option<String>,

        /// Override the HTTP frontend port from vhost.yml (yaml default 80).
        #[clap(long, value_name = "PORT", env = "BORE_VHOST_HTTP_PORT")]
        vhost_http_port: Option<u16>,

        /// Override the HTTPS frontend port from vhost.yml (yaml default 443).
        #[clap(long, value_name = "PORT", env = "BORE_VHOST_HTTPS_PORT")]
        vhost_https_port: Option<u16>,

        /// UDP port for the vhost QUIC direct path. Unset = use the resolved
        /// vhost HTTPS port on UDP.
        #[clap(long, value_name = "PORT", env = "BORE_VHOST_QUIC_PORT")]
        vhost_quic_port: Option<u16>,

        /// Override the frontend mode from vhost.yml.
        /// Values: http | https | both | redirect-https | auto
        #[clap(long, value_name = "MODE", env = "BORE_VHOST_MODE")]
        vhost_mode: Option<String>,

        /// TLS certificate chain (PEM) for the vhost HTTPS frontend. Overrides
        /// `cert_file` from vhost.yml. Use a wildcard cert for `*.<base-domain>`.
        #[clap(long, value_name = "PATH", env = "BORE_VHOST_CERT_FILE")]
        vhost_cert_file: Option<PathBuf>,

        /// TLS private key (PEM) for the vhost HTTPS frontend. Overrides
        /// `key_file` from vhost.yml.
        #[clap(long, value_name = "PATH", env = "BORE_VHOST_KEY_FILE")]
        vhost_key_file: Option<PathBuf>,

        /// Enable VPN link brokering (requires --features vpn on the client).
        #[cfg(feature = "vpn")]
        #[clap(long, env = "BORE_VPN")]
        vpn: bool,

        /// Overlay address pool for VPN links (CIDR, e.g. 10.99.0.0/16).
        /// Required when clients use pool-mode addressing.
        #[cfg(feature = "vpn")]
        #[clap(long, value_name = "CIDR", env = "BORE_VPN_POOL")]
        vpn_pool: Option<String>,

        /// Maximum number of concurrent VPN links.
        #[cfg(feature = "vpn")]
        #[clap(
            long,
            value_name = "N",
            default_value_t = 32usize,
            env = "BORE_VPN_MAX_LINKS"
        )]
        vpn_max_links: usize,

        /// Overlay subnet prefix allocated per hub from --vpn-pool (default 24).
        #[cfg(feature = "vpn")]
        #[clap(
            long,
            value_name = "P",
            default_value_t = 24u8,
            env = "BORE_VPN_HUB_PREFIX"
        )]
        vpn_hub_prefix: u8,
    },

    /// Diagnose this host's UDP / NAT / firewall for hole-punching (opens no
    /// tunnel). Probes public STUN servers (and your --to server's STUN, if
    /// given), classifies the NAT, and prints advice.
    TestUdp {
        /// Optional bore server (host:port or http(s):// URL) to also test the
        /// reachability of its STUN responder. Required with --tcp-secret-id.
        #[clap(short, long, value_name = "ADDR", env = "BORE_SERVER", default_value = DEFAULT_SERVER)]
        to: Option<String>,

        /// Optional secret for server authentication and direct-path token derivation.
        #[clap(
            short,
            long,
            value_name = "SECRET",
            env = "BORE_SECRET",
            hide_env_values = true
        )]
        secret: Option<String>,

        /// Pair with another test-udp peer using this diagnostic id. When set,
        /// the command connects to --to, waits for the peer, tests UDP direct and
        /// TCP relay paths, and prints a paired report.
        #[clap(long, value_name = "ID", env = "BORE_TCP_SECRET_ID")]
        tcp_secret_id: Option<String>,

        /// Skip TLS certificate verification for https:// servers.
        #[clap(long, env = "BORE_INSECURE")]
        insecure: bool,

        /// STUN server (host:port). In standalone diagnostics it is probed in
        /// addition to the public list; in paired mode it overrides the live
        /// tunnel STUN chain for candidate discovery.
        #[clap(long, value_name = "HOST:PORT", env = "BORE_STUN_SERVER")]
        stun_server: Option<String>,

        /// Try UPnP-IGD to add a router-mapped UDP candidate in paired mode.
        #[clap(long, env = "BORE_UPNP")]
        upnp: bool,

        /// Also advertise predicted symmetric-NAT ports in paired mode.
        #[clap(long, env = "BORE_TRY_PORT_PREDICTION")]
        try_port_prediction: bool,

        /// Bind the probe to this fixed UDP port (mirrors --nat-udp-preferred-port)
        /// to test whether exactly that port works through a firewall. 0 = random.
        #[clap(
            long,
            value_name = "PORT",
            env = "BORE_NAT_UDP_PORT",
            default_value_t = 0
        )]
        nat_udp_preferred_port: u16,

        /// Also run bidirectional bandwidth tests. Alias: --test-bandwith.
        #[clap(long = "test-bandwidth", alias = "test-bandwith")]
        test_bandwidth: bool,

        /// Bytes to transfer per direction and per path for --test-bandwidth.
        /// Accepts raw bytes or KB/MB/GB/KiB/MiB/GiB suffixes.
        #[clap(long, value_name = "SIZE", default_value = "64MB")]
        test_transfer_quota: String,

        /// Skip the TCP relay benchmark and only test the direct UDP path.
        #[clap(long)]
        udp_only: bool,
    },
}

#[derive(Subcommand, Debug)]
enum TransferCommand {
    /// Receive a transfer into a destination directory.
    #[clap(visible_alias = "listner")]
    Listener {
        /// Destination directory where the transfer is committed.
        #[clap(long, value_name = "DIR")]
        dest_path: PathBuf,

        /// Address of the remote server hosting the transfer rendezvous.
        #[clap(short, long, value_name = "ADDR", env = "BORE_SERVER", default_value = DEFAULT_SERVER)]
        to: String,

        /// Optional secret for authentication.
        #[clap(
            short,
            long,
            value_name = "SECRET",
            env = "BORE_SECRET",
            hide_env_values = true
        )]
        secret: Option<String>,

        /// Transfer identifier; aliases the existing --tcp-secret-id flag.
        #[clap(long = "transfer-id", alias = "tcp-secret-id", value_name = "ID")]
        transfer_id: Option<String>,

        /// Skip TLS certificate verification (for self-signed https:// servers).
        #[clap(long, env = "BORE_INSECURE")]
        insecure: bool,

        /// Disable the default direct-UDP attempt and force the relay path.
        #[clap(long)]
        relay_only: bool,

        /// STUN server (host:port) for UDP candidate discovery.
        #[clap(long, value_name = "HOST:PORT", env = "BORE_STUN_SERVER")]
        stun_server: Option<String>,

        /// Try UPnP-IGD to map a port on the local router.
        #[clap(long, env = "BORE_UPNP")]
        upnp: bool,

        /// Also advertise predicted symmetric-NAT ports as hole-punch candidates.
        #[clap(long, env = "BORE_TRY_PORT_PREDICTION")]
        try_port_prediction: bool,

        /// Bind the UDP hole-punch socket to this fixed port.
        #[clap(
            long,
            value_name = "PORT",
            env = "BORE_NAT_UDP_PORT",
            default_value_t = 0
        )]
        nat_udp_preferred_port: u16,

        /// How long (seconds) to wait before re-checking the preferred UDP port.
        #[clap(
            long,
            value_name = "SECS",
            env = "BORE_NAT_UDP_RELEASE_TIMEOUT",
            default_value_t = bore_cli::shared::NAT_UDP_RELEASE_TIMEOUT,
        )]
        nat_udp_release_timeout: u64,

        /// Number of relay carrier connections, used only on the TCP fallback path.
        /// 0 = auto: match the local worker hint, capped at 16 so each relay
        /// stream can ride its own TCP connection — independent congestion
        /// window, no head-of-line blocking. The server may still clamp lower.
        /// 1 forces a single connection. Ignored on direct UDP.
        #[clap(long, value_name = "N", default_value_t = 0, env = "BORE_CARRIERS")]
        carriers: u16,

        /// Overwrite an existing destination root.
        #[clap(long, conflicts_with = "rename")]
        overwrite: bool,

        /// Rename the destination root if it already exists.
        #[clap(long, conflicts_with = "overwrite")]
        rename: bool,

        /// Do not exit after the transfer completes; keep waiting for more senders
        /// with the same transfer-id. Errors from a single transfer are logged but
        /// the listener stays up.
        #[clap(long)]
        persistent: bool,

        /// Show the incoming file list and ask for y/N before accepting the transfer.
        /// Ignored when the sender is streaming stdin.
        #[clap(long)]
        ask_confirm: bool,

        /// Seconds to wait for --ask-confirm input before rejecting automatically
        /// (0 = wait forever; default 120).
        #[clap(long, default_value_t = 120)]
        confirm_timeout: u64,

        /// Abort if no transfer data is received for this many seconds (0 = disabled; default 60).
        #[clap(long, default_value_t = 60)]
        stall_timeout: u64,
    },

    /// Send a file, directory, or stdin stream.
    Sender {
        /// Source paths (files or directories) to transfer. May be specified multiple times
        /// or as a space-separated list. Use the literal "stdin" to read from standard input.
        #[clap(long = "sources", alias = "source", value_name = "PATH|stdin", num_args = 1..)]
        sources: Vec<PathBuf>,

        /// Text files containing source paths to transfer, one per line. Lines containing
        /// '#' are treated as comments and ignored.
        #[clap(long, value_name = "FILE", num_args = 1..)]
        source_files: Vec<PathBuf>,

        /// Print each source with its size and ask for confirmation before sending.
        #[clap(long)]
        ask_confirm: bool,

        /// Output file name when --sources stdin is used.
        #[clap(long, value_name = "NAME")]
        output: Option<PathBuf>,

        /// Address of the remote server hosting the transfer rendezvous.
        #[clap(short, long, value_name = "ADDR", env = "BORE_SERVER", default_value = DEFAULT_SERVER)]
        to: String,

        /// Optional secret for authentication.
        #[clap(
            short,
            long,
            value_name = "SECRET",
            env = "BORE_SECRET",
            hide_env_values = true
        )]
        secret: Option<String>,

        /// Transfer identifier; aliases the existing --tcp-secret-id flag.
        #[clap(long = "transfer-id", alias = "tcp-secret-id", value_name = "ID")]
        transfer_id: Option<String>,

        /// Skip TLS certificate verification (for self-signed https:// servers).
        #[clap(long, env = "BORE_INSECURE")]
        insecure: bool,

        /// Disable the default direct-UDP attempt and force the relay path.
        #[clap(long)]
        relay_only: bool,

        /// STUN server (host:port) for UDP candidate discovery.
        #[clap(long, value_name = "HOST:PORT", env = "BORE_STUN_SERVER")]
        stun_server: Option<String>,

        /// Try UPnP-IGD to map a port on the local router.
        #[clap(long, env = "BORE_UPNP")]
        upnp: bool,

        /// Also advertise predicted symmetric-NAT ports as hole-punch candidates.
        #[clap(long, env = "BORE_TRY_PORT_PREDICTION")]
        try_port_prediction: bool,

        /// Bind the UDP hole-punch socket to this fixed port.
        #[clap(
            long,
            value_name = "PORT",
            env = "BORE_NAT_UDP_PORT",
            default_value_t = 0
        )]
        nat_udp_preferred_port: u16,

        /// How long (seconds) to wait before re-checking the preferred UDP port.
        #[clap(
            long,
            value_name = "SECS",
            env = "BORE_NAT_UDP_RELEASE_TIMEOUT",
            default_value_t = bore_cli::shared::NAT_UDP_RELEASE_TIMEOUT,
        )]
        nat_udp_release_timeout: u64,

        /// Number of relay carrier connections, used only on the TCP fallback path.
        /// 0 = auto: match the resolved worker parallelism, capped at 16 so each
        /// relay stream can ride its own TCP connection — independent congestion
        /// window, no head-of-line blocking. The server may still clamp lower on
        /// provider-side pools. 1 forces a single connection. Ignored on direct UDP.
        #[clap(long, value_name = "N", default_value_t = 0, env = "BORE_CARRIERS")]
        carriers: u16,

        /// Number of parallel data streams for chunked filesystem transfers.
        /// Each stream maps to one QUIC bidi (direct path) or one yamux substream
        /// (relay). 0 = automatic (cpu-count, min 4). With --carriers 0 (auto) the relay
        /// carrier count tracks this value, so the relay path avoids HOL blocking out of
        /// the box. Stdin always uses one stream.
        #[clap(long, value_name = "N", default_value_t = 0)]
        parallel: u16,

        /// Include or exclude symlinks while scanning the source.
        #[clap(long, value_enum, default_value_t = SymlinkMode::Exclude)]
        symlinks: SymlinkMode,

        /// Include or exclude Unix device nodes while scanning the source.
        #[clap(long, value_enum, default_value_t = DeviceMode::Exclude)]
        devices: DeviceMode,

        /// Abort if no transfer data is sent for this many seconds (0 = disabled; default 60).
        #[clap(long, default_value_t = 60)]
        stall_timeout: u64,
    },
}

#[cfg(all(feature = "vpn", target_os = "linux"))]
#[derive(Subcommand, Debug)]
enum VpnCommand {
    /// Register a VPN link id and wait for a connector.
    Listen(VpnListenArgs),
    /// Dial a VPN link id registered by a listener.
    Connect(VpnConnectArgs),
}

#[cfg(all(feature = "vpn", target_os = "linux"))]
#[derive(clap::Args, Debug)]
struct VpnListenArgs {
    /// Server address (host or https://host).
    #[clap(short, long, value_name = "ADDR", env = "BORE_SERVER", default_value = DEFAULT_SERVER)]
    to: String,

    /// Shared secret for authentication (required).
    #[clap(
        short,
        long,
        value_name = "SECRET",
        env = "BORE_SECRET",
        hide_env_values = true,
        required = true
    )]
    secret: String,

    /// VPN link identifier.
    #[clap(long, value_name = "ID", env = "BORE_VPN_ID", required = true)]
    id: String,

    /// Skip TLS certificate verification for self-signed servers.
    #[clap(long, env = "BORE_INSECURE")]
    insecure: bool,

    /// Reconnect on disconnect (with exponential backoff).
    #[clap(long, env = "BORE_AUTO_RECONNECT")]
    auto_reconnect: bool,

    /// Subnets this side exposes (comma-separated CIDRs, e.g. 192.168.50.0/24).
    /// Omit for host-only mode; presence enables gateway mode.
    #[clap(
        long,
        value_name = "CIDR[,CIDR...]",
        env = "BORE_VPN_ADVERTISE",
        value_delimiter = ','
    )]
    advertise: Vec<String>,

    /// Static overlay IPv4 address with prefix (e.g. 172.31.0.1/30). Omit to use server pool.
    #[clap(long, value_name = "IP/PREFIX", env = "BORE_VPN_ADDR")]
    vpn_addr: Option<String>,

    /// Peer's overlay address for static mode (required with --vpn-addr).
    #[clap(
        long,
        value_name = "IP",
        env = "BORE_VPN_PEER_ADDR",
        requires = "vpn_addr"
    )]
    vpn_peer_addr: Option<String>,

    /// TUN interface name ("auto" = first free boreN, lets many instances share a host).
    #[clap(long, value_name = "NAME", default_value = "auto")]
    tun_name: String,

    /// Interface MTU.
    #[clap(long, value_name = "N", default_value_t = 1350u16)]
    mtu: u16,

    /// Pin --mtu: keep it fixed for the link lifetime (tests/benchmarks). The
    /// direct-path PMTU monitor then only WARNS when the path MTU is smaller,
    /// instead of resizing the TUN to follow it. Off by default (auto-tune).
    #[clap(long)]
    pin_mtu: bool,

    /// Print route/NAT commands instead of running them (interface is still created).
    #[clap(long)]
    no_route_manage: bool,

    /// Masquerade NAT'd (`real@exposed`) subnets toward the LAN so peers reach every
    /// host behind the gateway, not just the gateway itself (needed when the gateway
    /// is not the LAN's router). Off by default: preserves the peer's source IP for
    /// per-peer LAN ACLs / site↔site identical-LAN.
    #[clap(long)]
    nat_masquerade: bool,

    /// Insert an ACCEPT for the tun↔LAN pair into the iptables FORWARD chain so
    /// peers reach hosts BEHIND this gateway on a default-deny FORWARD host (the
    /// Docker daemon sets `-P FORWARD DROP`; ufw/hardened hosts too). Without it,
    /// bore only DETECTS a default-deny FORWARD and warns with the manual fix —
    /// only the gateway host itself would be reachable. Reverted on exit (RAII).
    #[clap(long)]
    forward_accept: bool,

    /// STUN server (host:port).
    #[clap(long, value_name = "HOST:PORT", env = "BORE_STUN_SERVER")]
    stun_server: Option<String>,

    /// Try UPnP-IGD to add a router-mapped UDP candidate.
    #[clap(long, env = "BORE_UPNP")]
    upnp: bool,

    /// Also advertise predicted symmetric-NAT ports.
    #[clap(long, env = "BORE_TRY_PORT_PREDICTION")]
    try_port_prediction: bool,

    /// Bind the UDP hole-punch socket to this fixed port.
    #[clap(
        long,
        value_name = "PORT",
        default_value_t = 0u16,
        env = "BORE_NAT_UDP_PREFERRED_PORT"
    )]
    nat_udp_preferred_port: u16,

    /// How long (seconds) to wait before re-checking the preferred UDP port.
    #[clap(
        long,
        value_name = "SECS",
        default_value_t = 0u64,
        env = "BORE_NAT_UDP_RELEASE_TIMEOUT"
    )]
    nat_udp_release_timeout: u64,

    /// Never attempt the direct UDP path; stay on the server relay.
    #[clap(long, env = "BORE_VPN_RELAY_ONLY")]
    relay_only: bool,

    /// Number of parallel relay carrier substream pairs (1-16). Both sides and
    /// the server's --max-carriers must agree; the minimum wins.
    #[clap(
        long,
        value_name = "N",
        default_value_t = 1u16,
        env = "BORE_VPN_CARRIERS"
    )]
    carriers: u16,

    /// Number of TUN queues (Linux IFF_MULTI_QUEUE, 1-8). One uplink pump per
    /// queue; useful on multi-Gbit links where a single pump is CPU-bound.
    #[clap(
        long,
        value_name = "N",
        default_value_t = 1u8,
        value_parser = clap::value_parser!(u8).range(1..=8),
        env = "BORE_VPN_TUN_QUEUES"
    )]
    tun_queues: u8,

    /// Optional operator note.
    #[clap(long, value_name = "TEXT", env = "BORE_NOTES")]
    notes: Option<String>,

    /// Max concurrent connectors (hub mode). 1 = legacy 1:1 path (byte-for-byte unchanged).
    #[clap(
        long,
        value_name = "N",
        default_value_t = 1u16,
        env = "BORE_VPN_MAX_CLIENTS"
    )]
    max_clients: u16,
}

#[cfg(all(feature = "vpn", target_os = "linux"))]
#[derive(clap::Args, Debug)]
struct VpnConnectArgs {
    /// Server address (host or https://host).
    #[clap(short, long, value_name = "ADDR", env = "BORE_SERVER", default_value = DEFAULT_SERVER)]
    to: String,

    /// Shared secret for authentication (required).
    #[clap(
        short,
        long,
        value_name = "SECRET",
        env = "BORE_SECRET",
        hide_env_values = true,
        required = true
    )]
    secret: String,

    /// VPN link identifier.
    #[clap(long, value_name = "ID", env = "BORE_VPN_ID", required = true)]
    id: String,

    /// Skip TLS certificate verification for self-signed servers.
    #[clap(long, env = "BORE_INSECURE")]
    insecure: bool,

    /// Reconnect on disconnect (with exponential backoff).
    #[clap(long, env = "BORE_AUTO_RECONNECT")]
    auto_reconnect: bool,

    /// Subnets this side exposes (comma-separated CIDRs, e.g. 192.168.50.0/24).
    /// Omit for host-only mode; presence enables gateway mode.
    #[clap(
        long,
        value_name = "CIDR[,CIDR...]",
        env = "BORE_VPN_ADVERTISE",
        value_delimiter = ','
    )]
    advertise: Vec<String>,

    /// Static overlay IPv4 address with prefix (e.g. 172.31.0.2/30). Omit to use server pool.
    #[clap(long, value_name = "IP/PREFIX", env = "BORE_VPN_ADDR")]
    vpn_addr: Option<String>,

    /// Peer's overlay address for static mode (required with --vpn-addr).
    #[clap(
        long,
        value_name = "IP",
        env = "BORE_VPN_PEER_ADDR",
        requires = "vpn_addr"
    )]
    vpn_peer_addr: Option<String>,

    /// TUN interface name ("auto" = first free boreN, lets many instances share a host).
    #[clap(long, value_name = "NAME", default_value = "auto")]
    tun_name: String,

    /// Interface MTU.
    #[clap(long, value_name = "N", default_value_t = 1350u16)]
    mtu: u16,

    /// Pin --mtu: keep it fixed for the link lifetime (tests/benchmarks). The
    /// direct-path PMTU monitor then only WARNS when the path MTU is smaller,
    /// instead of resizing the TUN to follow it. Off by default (auto-tune).
    #[clap(long)]
    pin_mtu: bool,

    /// Print route/NAT commands instead of running them (interface is still created).
    #[clap(long)]
    no_route_manage: bool,

    /// Masquerade NAT'd (`real@exposed`) subnets toward the LAN so peers reach every
    /// host behind the gateway, not just the gateway itself (needed when the gateway
    /// is not the LAN's router). Off by default: preserves the peer's source IP for
    /// per-peer LAN ACLs / site↔site identical-LAN.
    #[clap(long)]
    nat_masquerade: bool,

    /// Insert an ACCEPT for the tun↔LAN pair into the iptables FORWARD chain so
    /// peers reach hosts BEHIND this gateway on a default-deny FORWARD host (the
    /// Docker daemon sets `-P FORWARD DROP`; ufw/hardened hosts too). Without it,
    /// bore only DETECTS a default-deny FORWARD and warns with the manual fix —
    /// only the gateway host itself would be reachable. Reverted on exit (RAII).
    #[clap(long)]
    forward_accept: bool,

    /// STUN server (host:port).
    #[clap(long, value_name = "HOST:PORT", env = "BORE_STUN_SERVER")]
    stun_server: Option<String>,

    /// Try UPnP-IGD to add a router-mapped UDP candidate.
    #[clap(long, env = "BORE_UPNP")]
    upnp: bool,

    /// Also advertise predicted symmetric-NAT ports.
    #[clap(long, env = "BORE_TRY_PORT_PREDICTION")]
    try_port_prediction: bool,

    /// Bind the UDP hole-punch socket to this fixed port.
    #[clap(
        long,
        value_name = "PORT",
        default_value_t = 0u16,
        env = "BORE_NAT_UDP_PREFERRED_PORT"
    )]
    nat_udp_preferred_port: u16,

    /// How long (seconds) to wait before re-checking the preferred UDP port.
    #[clap(
        long,
        value_name = "SECS",
        default_value_t = 0u64,
        env = "BORE_NAT_UDP_RELEASE_TIMEOUT"
    )]
    nat_udp_release_timeout: u64,

    /// Never attempt the direct UDP path; stay on the server relay.
    #[clap(long, env = "BORE_VPN_RELAY_ONLY")]
    relay_only: bool,

    /// Number of parallel relay carrier substream pairs (1-16). Both sides and
    /// the server's --max-carriers must agree; the minimum wins.
    #[clap(
        long,
        value_name = "N",
        default_value_t = 1u16,
        env = "BORE_VPN_CARRIERS"
    )]
    carriers: u16,

    /// Number of TUN queues (Linux IFF_MULTI_QUEUE, 1-8). One uplink pump per
    /// queue; useful on multi-Gbit links where a single pump is CPU-bound.
    #[clap(
        long,
        value_name = "N",
        default_value_t = 1u8,
        value_parser = clap::value_parser!(u8).range(1..=8),
        env = "BORE_VPN_TUN_QUEUES"
    )]
    tun_queues: u8,

    /// Optional operator note.
    #[clap(long, value_name = "TEXT", env = "BORE_NOTES")]
    notes: Option<String>,

    /// Accept exactly these advertised routes (exact-or-subset). Comma-separated CIDRs.
    #[clap(
        long,
        value_name = "CIDR[,CIDR...]",
        env = "BORE_VPN_ACCEPT_ROUTES",
        value_delimiter = ','
    )]
    accept_routes: Vec<String>,

    /// Accept every route the listener advertises.
    #[clap(long, env = "BORE_VPN_ACCEPT_ALL_ROUTES")]
    accept_all_routes: bool,

    /// Subtract these routes from the accepted set. Comma-separated CIDRs.
    #[clap(
        long,
        value_name = "CIDR[,CIDR...]",
        env = "BORE_VPN_REFUSE_ROUTES",
        value_delimiter = ','
    )]
    refuse_routes: Vec<String>,

    /// Accept nothing (== default; for explicit, self-documenting scripts).
    #[clap(long, env = "BORE_VPN_REFUSE_ALL_ROUTES")]
    refuse_all_routes: bool,
}

#[tokio::main]
async fn run(command: Command) -> Result<()> {
    // Race the command against a shutdown signal so Ctrl-C / SIGTERM (e.g.
    // `docker stop`, systemd) exit cleanly with a log line instead of an abrupt
    // kill mid-transfer.
    tokio::select! {
        res = dispatch(command) => res,
        _ = shutdown_signal() => {
            info!("shutdown signal received, exiting");
            Ok(())
        }
    }
}

/// Wait for Ctrl-C, or SIGTERM on Unix (what `docker stop` / systemd send).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

async fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Local {
            local_host,
            local_port,
            to,
            port,
            secret,
            tcp_secret_id,
            insecure,
            https,
            force_https,
            udp,
            stun_server,
            upnp,
            try_port_prediction,
            nat_udp_preferred_port,
            nat_udp_release_timeout,
            max_conns,
            basic_auth,
            notes,
            carriers,
            auto_reconnect,
        } => {
            let notes = clamp_notes(notes);
            if let Some(creds) = &basic_auth {
                if !creds.contains(':') {
                    Args::command()
                        .error(
                            ErrorKind::InvalidValue,
                            "--basic-auth must be in the form \"user:pass\"",
                        )
                        .exit();
                }
            }
            // The direct-UDP path (hole-punched QUIC) exists only for secret
            // tunnels: a public tunnel always relays through the server, which owns
            // the public port, so there is no peer to punch to. These flags are
            // inert on the public path — warn loudly rather than dropping them
            // silently, and only emit the "resolved UDP settings" line when it is
            // actually a secret tunnel.
            match tcp_secret_id {
                Some(id) => {
                    if udp {
                        info!(
                            mode = "local",
                            udp,
                            stun_server = ?stun_server.as_deref(),
                            upnp,
                            try_port_prediction,
                            nat_udp_preferred_port,
                            nat_udp_release_timeout,
                            max_conns,
                            carriers,
                            "resolved UDP optimization settings",
                        );
                    }
                    let meta = ProviderMeta { notes, basic_auth };
                    let connect = move || {
                        let (local_host, to, id, secret, stun_server, meta) = (
                            local_host.clone(),
                            to.clone(),
                            id.clone(),
                            secret.clone(),
                            stun_server.clone(),
                            meta.clone(),
                        );
                        async move {
                            Client::new_secret_provider(
                                &local_host,
                                local_port,
                                &to,
                                &id,
                                secret.as_deref(),
                                insecure,
                                udp,
                                stun_server.as_deref(),
                                upnp,
                                try_port_prediction,
                                nat_udp_preferred_port,
                                nat_udp_release_timeout,
                                max_conns,
                                carriers,
                                meta,
                            )
                            .await
                        }
                    };
                    reconnect::run(auto_reconnect, connect, serve_client).await?;
                }
                None => {
                    if upnp
                        || try_port_prediction
                        || stun_server.is_some()
                        || nat_udp_preferred_port != 0
                    {
                        warn!(
                            upnp,
                            try_port_prediction,
                            stun_server = ?stun_server.as_deref(),
                            nat_udp_preferred_port,
                            "ignoring secret-tunnel-only UDP options on a public tunnel: \
                             --upnp / --stun-server / --try-port-prediction / \
                             --nat-udp-preferred-port apply to secret tunnels only \
                             (pass --tcp-secret-id). Use --udp on public tunnels for \
                             QUIC direct path.",
                        );
                    }
                    let options = TunnelOptions {
                        https,
                        force_https,
                        basic_auth,
                        notes,
                        carriers,
                        udp,
                        auto_reconnect,
                    };
                    let connect = move || {
                        let (local_host, to, secret, options) = (
                            local_host.clone(),
                            to.clone(),
                            secret.clone(),
                            options.clone(),
                        );
                        async move {
                            Client::new(
                                &local_host,
                                local_port,
                                &to,
                                port,
                                secret.as_deref(),
                                insecure,
                                options,
                            )
                            .await
                        }
                    };
                    reconnect::run(auto_reconnect, connect, serve_client).await?;
                }
            }
        }
        Command::Proxy {
            local_proxy_port,
            to,
            secret,
            tcp_secret_id,
            insecure,
            udp,
            stun_server,
            upnp,
            try_port_prediction,
            nat_udp_preferred_port,
            nat_udp_release_timeout,
            notes,
            carriers,
            auto_reconnect,
        } => {
            let bind_addr = parse_proxy_addr(&local_proxy_port)?;
            let notes = clamp_notes(notes);
            if udp {
                info!(
                    mode = "proxy",
                    udp,
                    stun_server = ?stun_server.as_deref(),
                    upnp,
                    try_port_prediction,
                    nat_udp_preferred_port,
                    nat_udp_release_timeout,
                    carriers,
                    "resolved UDP optimization settings",
                );
            }
            let connect = move || {
                let (to, tcp_secret_id, secret, stun_server, notes) = (
                    to.clone(),
                    tcp_secret_id.clone(),
                    secret.clone(),
                    stun_server.clone(),
                    notes.clone(),
                );
                async move {
                    Proxy::new(
                        &to,
                        bind_addr,
                        &tcp_secret_id,
                        secret.as_deref(),
                        insecure,
                        udp,
                        stun_server.as_deref(),
                        upnp,
                        try_port_prediction,
                        nat_udp_preferred_port,
                        nat_udp_release_timeout,
                        carriers,
                        notes,
                    )
                    .await
                }
            };
            reconnect::run(auto_reconnect, connect, serve_proxy).await?;
        }
        Command::Transfer { command } => match command {
            TransferCommand::Listener {
                dest_path,
                to,
                secret,
                transfer_id,
                insecure,
                relay_only,
                stun_server,
                upnp,
                try_port_prediction,
                nat_udp_preferred_port,
                nat_udp_release_timeout,
                carriers,
                overwrite,
                rename,
                persistent,
                ask_confirm,
                confirm_timeout,
                stall_timeout,
            } => {
                let collision = match (overwrite, rename) {
                    (true, false) => CollisionPolicy::Overwrite,
                    (false, true) => CollisionPolicy::Rename,
                    _ => CollisionPolicy::Fail,
                };
                if !relay_only {
                    info!(
                        mode = "transfer-listener",
                        stun_server = ?stun_server.as_deref(),
                        upnp,
                        try_port_prediction,
                        nat_udp_preferred_port,
                        nat_udp_release_timeout,
                        carriers,
                        "resolved transfer UDP settings"
                    );
                }
                bore_cli::transfer::run_listener(TransferListenerOptions {
                    to,
                    secret,
                    insecure,
                    transfer_id,
                    dest_path,
                    relay_only,
                    stun_server,
                    upnp,
                    try_port_prediction,
                    nat_udp_preferred_port,
                    nat_udp_release_timeout,
                    carriers,
                    collision,
                    persistent,
                    ask_confirm,
                    confirm_timeout,
                    stall_timeout,
                })
                .await?;
            }
            TransferCommand::Sender {
                sources,
                source_files,
                ask_confirm,
                output,
                to,
                secret,
                transfer_id,
                insecure,
                relay_only,
                stun_server,
                upnp,
                try_port_prediction,
                nat_udp_preferred_port,
                nat_udp_release_timeout,
                carriers,
                parallel,
                symlinks,
                devices,
                stall_timeout,
            } => {
                if !relay_only {
                    info!(
                        mode = "transfer-sender",
                        stun_server = ?stun_server.as_deref(),
                        upnp,
                        try_port_prediction,
                        nat_udp_preferred_port,
                        nat_udp_release_timeout,
                        carriers,
                        "resolved transfer UDP settings"
                    );
                }
                bore_cli::transfer::run_sender(TransferSenderOptions {
                    to,
                    secret,
                    insecure,
                    transfer_id,
                    sources,
                    source_files,
                    ask_confirm,
                    output,
                    relay_only,
                    stun_server,
                    upnp,
                    try_port_prediction,
                    nat_udp_preferred_port,
                    nat_udp_release_timeout,
                    carriers,
                    parallel,
                    symlinks,
                    devices,
                    stall_timeout,
                })
                .await?;
            }
        },
        #[cfg(all(feature = "vpn", target_os = "linux"))]
        Command::Vpn { command } => match command {
            VpnCommand::Listen(args) => {
                let advertise_entries: Result<Vec<_>> = args
                    .advertise
                    .iter()
                    .map(|s| s.parse::<AdvertiseEntry>())
                    .collect();
                let advertise_entries =
                    advertise_entries.context("failed to parse --advertise CIDRs")?;

                let addr_request = match (&args.vpn_addr, &args.vpn_peer_addr) {
                    (None, None) => VpnAddrRequest::Pool,
                    (Some(addr_str), Some(peer_str)) => {
                        let (addr_s, prefix_s) = addr_str
                            .split_once('/')
                            .context("missing '/' in --vpn-addr (format: 172.31.0.1/30)")?;
                        let addr = addr_s.parse().context("invalid IP in --vpn-addr")?;
                        let prefix = prefix_s.parse().context("invalid prefix in --vpn-addr")?;
                        let peer = peer_str.parse().context("invalid IP in --vpn-peer-addr")?;
                        VpnAddrRequest::Static { addr, prefix, peer }
                    }
                    (Some(_), None) => anyhow::bail!("--vpn-addr requires --vpn-peer-addr"),
                    (None, Some(_)) => anyhow::bail!("--vpn-peer-addr requires --vpn-addr"),
                };

                let vpn_args = vpn::VpnListenArgs {
                    to: args.to,
                    secret: args.secret,
                    id: args.id,
                    insecure: args.insecure,
                    advertise_entries,
                    addr_request,
                    tun_name: args.tun_name,
                    mtu: args.mtu,
                    pin_mtu: args.pin_mtu,
                    no_route_manage: args.no_route_manage,
                    nat_masquerade: args.nat_masquerade,
                    forward_accept: args.forward_accept,
                    stun_server: args.stun_server,
                    upnp: args.upnp,
                    try_port_prediction: args.try_port_prediction,
                    nat_udp_preferred_port: args.nat_udp_preferred_port,
                    nat_udp_release_timeout: args.nat_udp_release_timeout,
                    relay_only: args.relay_only,
                    auto_reconnect: args.auto_reconnect,
                    carriers: args.carriers,
                    tun_queues: args.tun_queues as usize,
                    notes: args.notes,
                    max_clients: args.max_clients,
                };

                vpn::run_listen(vpn_args).await?;
            }
            VpnCommand::Connect(args) => {
                let advertise_entries: Result<Vec<_>> = args
                    .advertise
                    .iter()
                    .map(|s| s.parse::<AdvertiseEntry>())
                    .collect();
                let advertise_entries =
                    advertise_entries.context("failed to parse --advertise CIDRs")?;

                let accept_routes: Result<Vec<_>> = args
                    .accept_routes
                    .iter()
                    .map(|s| s.parse::<Ipv4Net>())
                    .collect();
                let accept_routes =
                    accept_routes.context("failed to parse --accept-routes CIDRs")?;

                let refuse_routes: Result<Vec<_>> = args
                    .refuse_routes
                    .iter()
                    .map(|s| s.parse::<Ipv4Net>())
                    .collect();
                let refuse_routes =
                    refuse_routes.context("failed to parse --refuse-routes CIDRs")?;

                let addr_request = match (&args.vpn_addr, &args.vpn_peer_addr) {
                    (None, None) => VpnAddrRequest::Pool,
                    (Some(addr_str), Some(peer_str)) => {
                        let (addr_s, prefix_s) = addr_str
                            .split_once('/')
                            .context("missing '/' in --vpn-addr (format: 172.31.0.2/30)")?;
                        let addr = addr_s.parse().context("invalid IP in --vpn-addr")?;
                        let prefix = prefix_s.parse().context("invalid prefix in --vpn-addr")?;
                        let peer = peer_str.parse().context("invalid IP in --vpn-peer-addr")?;
                        VpnAddrRequest::Static { addr, prefix, peer }
                    }
                    (Some(_), None) => anyhow::bail!("--vpn-addr requires --vpn-peer-addr"),
                    (None, Some(_)) => anyhow::bail!("--vpn-peer-addr requires --vpn-addr"),
                };

                let vpn_args = vpn::VpnConnectArgs {
                    to: args.to,
                    secret: args.secret,
                    id: args.id,
                    insecure: args.insecure,
                    advertise_entries,
                    addr_request,
                    tun_name: args.tun_name,
                    mtu: args.mtu,
                    pin_mtu: args.pin_mtu,
                    no_route_manage: args.no_route_manage,
                    nat_masquerade: args.nat_masquerade,
                    forward_accept: args.forward_accept,
                    stun_server: args.stun_server,
                    upnp: args.upnp,
                    try_port_prediction: args.try_port_prediction,
                    nat_udp_preferred_port: args.nat_udp_preferred_port,
                    nat_udp_release_timeout: args.nat_udp_release_timeout,
                    relay_only: args.relay_only,
                    auto_reconnect: args.auto_reconnect,
                    carriers: args.carriers,
                    tun_queues: args.tun_queues as usize,
                    notes: args.notes,
                    accept_routes,
                    accept_all_routes: args.accept_all_routes,
                    refuse_routes,
                    refuse_all_routes: args.refuse_all_routes,
                };

                vpn::run_connect(vpn_args).await?;
            }
        },
        Command::Server {
            min_port,
            max_port,
            secret,
            max_conns,
            max_carriers,
            control_port,
            bind_domain,
            cert_file,
            key_file,
            bind_addr,
            bind_tunnels,
            udp,
            udp_stream_receive_window,
            udp_connection_receive_window,
            udp_send_window,
            udp_socket_recv_buffer,
            udp_socket_send_buffer,
            udp_max_streams,
            admin_token,
            control_hsts,
            vhost_config,
            vhost_base_domain,
            vhost_http_port,
            vhost_https_port,
            vhost_quic_port,
            vhost_mode,
            vhost_cert_file,
            vhost_key_file,
            #[cfg(feature = "vpn")]
            vpn,
            #[cfg(feature = "vpn")]
            vpn_pool,
            #[cfg(feature = "vpn")]
            vpn_max_links,
            #[cfg(feature = "vpn")]
            vpn_hub_prefix,
        } => {
            let port_range = min_port..=max_port;
            if port_range.is_empty() {
                Args::command()
                    .error(ErrorKind::InvalidValue, "port range is empty")
                    .exit();
            }
            // The admin token must be hard to guess; enforce a minimum length.
            if let Some(token) = &admin_token {
                if token.chars().count() < 32 {
                    Args::command()
                        .error(
                            ErrorKind::InvalidValue,
                            "--admin-token must be at least 32 characters",
                        )
                        .exit();
                }
            }
            let mut server = Server::new(port_range, secret.as_deref());
            server.set_admin_token(admin_token);
            server.set_control_hsts(&control_hsts);
            server.set_max_conns(max_conns);
            server.set_max_carriers(max_carriers);
            server.set_udp_tuning(parse_udp_tuning(
                &udp_stream_receive_window,
                &udp_connection_receive_window,
                &udp_send_window,
                &udp_socket_recv_buffer,
                &udp_socket_send_buffer,
                udp_max_streams,
            )?);
            server.set_control_port(control_port);
            if let Some(ref domain) = bind_domain {
                server.set_bind_domain(domain.clone());
            }
            // Store config values before they might be moved/consumed.
            #[cfg(feature = "vpn")]
            let config_vpn_pool = vpn_pool.clone();
            let config_vhost_base_domain = vhost_base_domain.clone();
            let config_tls = cert_file.is_some() && key_file.is_some();

            match (cert_file, key_file) {
                (Some(cert), Some(key)) => {
                    let acceptor = bore_cli::transport::load_server_tls(&cert, &key)?;
                    server.set_tls(acceptor);
                    server.set_tls_cert_path(Some(std::path::PathBuf::from(cert.clone())));
                }
                (None, None) => {}
                _ => {
                    Args::command()
                        .error(
                            ErrorKind::ArgumentConflict,
                            "--cert-file and --key-file must be provided together",
                        )
                        .exit();
                }
            }
            server.set_bind_addr(bind_addr);
            server.set_bind_tunnels(bind_tunnels.unwrap_or(bind_addr));
            server.set_udp(udp);
            // VPN brokering (only available when compiled with --features vpn).

            #[cfg(feature = "vpn")]
            {
                if vpn {
                    server.set_vpn(true);
                    server.set_vpn_max_links(vpn_max_links);
                    server.set_vpn_hub_prefix(vpn_hub_prefix);
                    if let Some(pool_cidr) = vpn_pool {
                        let net: bore_cli::shared::Ipv4Net = pool_cidr
                            .parse()
                            .with_context(|| format!("invalid --vpn-pool CIDR: {pool_cidr}"))?;
                        server.set_vpn_pool(net).context("invalid --vpn-pool")?;
                    }
                }
            }
            // Build the vhost config from a yaml file and/or env/CLI flags. vhost
            // is enabled when either a config file or a base domain is provided, so
            // a simple deployment needs no file (env-only), matching the rest of
            // bore's env-driven configuration. A file is only required for
            // reservations and default_headers.
            let vhost_cfg = if let Some(ref config_path) = vhost_config {
                let yaml = std::fs::read_to_string(config_path).with_context(|| {
                    format!("failed to read vhost config: {}", config_path.display())
                })?;
                Some(bore_cli::vhost::parse_config(&yaml)?)
            } else if vhost_base_domain.is_some() {
                Some(bore_cli::vhost::VhostConfig {
                    base_domain: String::new(), // filled from the flag below
                    mode: bore_cli::vhost::VhostModeCfg::Auto,
                    http_port: 80,
                    https_port: 443,
                    cert_file: None,
                    key_file: None,
                    default_headers: Default::default(),
                    default_response_headers: Default::default(),
                    reservations: Vec::new(),
                })
            } else {
                None
            };

            if let Some(mut cfg) = vhost_cfg {
                if let Some(domain) = vhost_base_domain {
                    cfg.base_domain = domain;
                }
                if let Some(port) = vhost_http_port {
                    cfg.http_port = port;
                }
                if let Some(port) = vhost_https_port {
                    cfg.https_port = port;
                }
                if let Some(cert) = vhost_cert_file {
                    cfg.cert_file = Some(cert);
                }
                if let Some(key) = vhost_key_file {
                    cfg.key_file = Some(key);
                }
                if let Some(ref mode_str) = vhost_mode {
                    cfg.mode = match mode_str.as_str() {
                        "http" => bore_cli::vhost::VhostModeCfg::Http,
                        "https" => bore_cli::vhost::VhostModeCfg::Https,
                        "both" => bore_cli::vhost::VhostModeCfg::Both,
                        "redirect-https" => bore_cli::vhost::VhostModeCfg::RedirectHttps,
                        "auto" => bore_cli::vhost::VhostModeCfg::Auto,
                        other => {
                            Args::command()
                                .error(
                                    ErrorKind::InvalidValue,
                                    format!("unknown --vhost-mode '{other}'; expected: http, https, both, redirect-https, auto"),
                                )
                                .exit();
                        }
                    };
                }
                if cfg.base_domain.trim().is_empty() {
                    Args::command()
                        .error(
                            ErrorKind::InvalidValue,
                            "vhost requires a base domain (set `base_domain` in --vhost-config or pass --vhost-base-domain / BORE_VHOST_BASE_DOMAIN)",
                        )
                        .exit();
                }
                if let Some(port) = vhost_quic_port {
                    server.set_vhost_quic_port(port);
                }
                server.set_vhost(cfg)?;
                if let Some(config_path) = vhost_config {
                    server.set_vhost_config_path(config_path);
                }
            }
            // Build and store the server configuration snapshot (D11: sanitized, no secrets).
            let udp_socket_send_buffer =
                bore_cli::shared::parse_size_bytes(&udp_socket_send_buffer).map(|b| b as usize);
            let udp_socket_recv_buffer =
                bore_cli::shared::parse_size_bytes(&udp_socket_recv_buffer).map(|b| b as usize);

            let config_view = bore_cli::admin_views::ConfigView {
                port_range: format!("{}-{}", min_port, max_port),
                control_port,
                max_conns: max_conns as u32,
                max_carriers,
                bind_addr: bind_addr.to_string(),
                bind_tunnels: bind_tunnels.unwrap_or(bind_addr).to_string(),
                udp,
                udp_socket_send_buffer,
                udp_socket_recv_buffer,
                udp_stream_receive_window,
                udp_connection_receive_window,
                udp_send_window,
                udp_max_streams,
                bind_domain: bind_domain.clone(),
                control_hsts,
                #[cfg(feature = "vpn")]
                vpn_enabled: vpn,
                #[cfg(feature = "vpn")]
                vpn_pool: config_vpn_pool,
                #[cfg(feature = "vpn")]
                vpn_max_links: vpn_max_links as u32,
                #[cfg(feature = "vpn")]
                vpn_hub_prefix,
                #[cfg(feature = "vpn")]
                vpn_punch_timeout: Some(bore_cli::vpn_server::DEFAULT_VPN_PUNCH_TIMEOUT.as_secs()),
                vhost_enabled: !server.vhost_registry().is_empty()
                    || config_vhost_base_domain.is_some(),
                vhost_base_domain: config_vhost_base_domain,
                vhost_http_port,
                vhost_https_port,
                vhost_quic_port,
                vhost_mode: vhost_mode.clone(),
                tls: config_tls,
            };
            server.set_config_view(config_view);
            server.listen().await?;
        }
        Command::Vhost {
            target,
            subdomain,
            id,
            to,
            secret,
            insecure,
            notes,
            basic_auth,
            carriers,
            udp,
            auto_reconnect,
        } => {
            let (local_host, local_port) = parse_vhost_target(&target)?;
            let notes = clamp_notes(notes);
            let meta = bore_cli::client::ProviderMeta {
                notes,
                basic_auth: basic_auth.clone(),
            };
            let connect = move || {
                let (local_host, to, subdomain, id, secret, meta) = (
                    local_host.clone(),
                    to.clone(),
                    subdomain.clone(),
                    id.clone(),
                    secret.clone(),
                    meta.clone(),
                );
                async move {
                    bore_cli::client::Client::new_vhost_provider_with_udp(
                        &local_host,
                        local_port,
                        &to,
                        &subdomain,
                        &id,
                        secret.as_deref(),
                        insecure,
                        carriers,
                        udp,
                        meta,
                    )
                    .await
                }
            };
            reconnect::run(auto_reconnect, connect, serve_client).await?;
        }
        Command::TestUdp {
            to,
            secret,
            tcp_secret_id,
            insecure,
            stun_server,
            upnp,
            try_port_prediction,
            nat_udp_preferred_port,
            test_bandwidth,
            test_transfer_quota,
            udp_only,
        } => {
            if let Some(id) = tcp_secret_id {
                let Some(to) = to else {
                    Args::command()
                        .error(
                            ErrorKind::MissingRequiredArgument,
                            "--to is required with --tcp-secret-id",
                        )
                        .exit();
                };
                let transfer_quota = parse_transfer_quota(&test_transfer_quota)?;
                info!(
                    mode = "test-udp",
                    paired = true,
                    stun_server = ?stun_server.as_deref(),
                    upnp,
                    try_port_prediction,
                    udp_only,
                    nat_udp_preferred_port,
                    "resolved UDP optimization settings",
                );
                bore_cli::udp_diagnostic::run_peer_test(
                    &to,
                    &id,
                    secret.as_deref(),
                    insecure,
                    stun_server.as_deref(),
                    upnp,
                    try_port_prediction,
                    nat_udp_preferred_port,
                    UdpTestOptions {
                        bandwidth: test_bandwidth,
                        transfer_quota,
                        udp_only,
                    },
                )
                .await?;
            } else {
                if test_bandwidth {
                    Args::command()
                        .error(
                            ErrorKind::ArgumentConflict,
                            "--test-bandwidth requires --tcp-secret-id so two peers can be paired",
                        )
                        .exit();
                }
                info!(
                    mode = "test-udp",
                    paired = false,
                    stun_server = ?stun_server.as_deref(),
                    upnp,
                    try_port_prediction,
                    udp_only,
                    nat_udp_preferred_port,
                    "resolved UDP optimization settings",
                );
                let bore_target = to.map(|t| {
                    let ep = bore_cli::transport::Endpoint::parse(&t);
                    (ep.host, ep.port)
                });
                bore_cli::holepunch::diagnose(
                    bore_target,
                    stun_server.as_deref(),
                    nat_udp_preferred_port,
                )
                .await?;
            }
        }
    }

    Ok(())
}

/// Truncate an operator note to [`MAX_NOTES_LEN`] characters (on a char boundary)
/// so it always fits the control-channel frame.
fn clamp_notes(notes: Option<String>) -> Option<String> {
    notes.map(|mut n| {
        if n.chars().count() > MAX_NOTES_LEN {
            n = n.chars().take(MAX_NOTES_LEN).collect();
        }
        n
    })
}

/// Run a connected client until its connection ends.
async fn serve_client(client: Client) -> Result<()> {
    client.listen().await
}

/// Run a connected proxy until its connection ends.
async fn serve_proxy(proxy: Proxy) -> Result<()> {
    proxy.listen().await
}

/// Parse a vhost forward target `host:port`. A leading ":" (e.g. ":8080") means
/// `localhost`. The host may be an IP literal or a hostname (resolved at connect
/// time), matching the local/proxy/transfer subcommands.
fn parse_vhost_target(target: &str) -> Result<(String, u16)> {
    let normalized = match target.strip_prefix(':') {
        Some(port) => format!("localhost:{port}"),
        None => target.to_string(),
    };
    // Prefer a full socket address (handles IPv4 and bracketed IPv6 literals).
    if let Ok(addr) = normalized.parse::<SocketAddr>() {
        return Ok((addr.ip().to_string(), addr.port()));
    }
    // Otherwise accept host:port where host is a name resolved at connect time.
    let (host, port) = normalized
        .rsplit_once(':')
        .with_context(|| format!("invalid vhost target '{target}'; expected host:port"))?;
    if host.is_empty() {
        anyhow::bail!("invalid vhost target '{target}'; expected host:port");
    }
    let port: u16 = port
        .parse()
        .with_context(|| format!("invalid port in vhost target '{target}'; expected host:port"))?;
    Ok((host.to_string(), port))
}

fn parse_proxy_addr(value: &str) -> Result<SocketAddr> {
    let normalized = match value.strip_prefix(':') {
        Some(port) => format!("0.0.0.0:{port}"),
        None => value.to_string(),
    };
    normalized
        .parse()
        .with_context(|| format!("invalid --local-proxy-port: {value}"))
}

fn parse_transfer_quota(value: &str) -> Result<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        anyhow::bail!("--test-transfer-quota cannot be empty");
    }
    let split_at = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split_at);
    let bytes: u64 = number
        .parse()
        .with_context(|| format!("invalid --test-transfer-quota: {value}"))?;
    let multiplier = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "ki" | "kib" => 1024,
        "mi" | "mib" => 1024 * 1024,
        "gi" | "gib" => 1024 * 1024 * 1024,
        other => anyhow::bail!("unsupported --test-transfer-quota suffix: {other}"),
    };
    bytes
        .checked_mul(multiplier)
        .context("--test-transfer-quota is too large")
}

fn parse_udp_tuning(
    stream_receive_window: &str,
    connection_receive_window: &str,
    send_window: &str,
    udp_socket_recv_buffer: &str,
    udp_socket_send_buffer: &str,
    max_direct_streams: u32,
) -> Result<UdpDirectTuning> {
    Ok(UdpDirectTuning {
        stream_receive_window: parse_transfer_quota(stream_receive_window)?
            .try_into()
            .context("--udp-stream-receive-window is too large")?,
        connection_receive_window: parse_transfer_quota(connection_receive_window)?
            .try_into()
            .context("--udp-connection-receive-window is too large")?,
        send_window: parse_transfer_quota(send_window)?,
        udp_socket_recv_buffer: parse_transfer_quota(udp_socket_recv_buffer)?
            .try_into()
            .context("--udp-socket-recv-buffer is too large")?,
        udp_socket_send_buffer: parse_transfer_quota(udp_socket_send_buffer)?
            .try_into()
            .context("--udp-socket-send-buffer is too large")?,
        max_direct_streams,
    })
}

/// Initialize logging: `RUST_LOG` wins if set; otherwise default to `info`, or
/// `debug`/`trace` with `-v`/`-vv`. Logs go to stderr (keeping stdout clean), and
/// ANSI colors are enabled only on a terminal so redirected/Docker/journald logs
/// stay plain.
fn init_logging(verbose: u8) {
    use std::io::IsTerminal;
    use tracing_subscriber::EnvFilter;
    let filter = if std::env::var_os("RUST_LOG").is_some() {
        EnvFilter::from_default_env()
    } else {
        let level = match verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        };
        EnvFilter::new(format!("bore_cli={level},bore={level}"))
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(args.verbose);
    run(args.command)
}

#[cfg(test)]
mod tests {
    use super::*;

    use lazy_static::lazy_static;
    use std::sync::Mutex;

    lazy_static! {
        static ref ENV_GUARD: Mutex<()> = Mutex::new(());
    }

    #[test]
    fn server_udp_tuning_defaults_match_current_values() {
        let _guard = ENV_GUARD.lock().unwrap();
        let args = Args::parse_from(["bore", "server"]);
        let Command::Server {
            udp_stream_receive_window,
            udp_connection_receive_window,
            udp_send_window,
            udp_socket_recv_buffer,
            udp_socket_send_buffer,
            udp_max_streams,
            ..
        } = args.command
        else {
            panic!("expected server command");
        };
        let tuning = parse_udp_tuning(
            &udp_stream_receive_window,
            &udp_connection_receive_window,
            &udp_send_window,
            &udp_socket_recv_buffer,
            &udp_socket_send_buffer,
            udp_max_streams,
        )
        .unwrap();
        assert_eq!(tuning, UdpDirectTuning::default());
    }

    #[test]
    fn server_udp_tuning_flags_override_defaults() {
        let _guard = ENV_GUARD.lock().unwrap();
        let args = Args::parse_from([
            "bore",
            "server",
            "--udp-stream-receive-window",
            "32MiB",
            "--udp-connection-receive-window",
            "96MiB",
            "--udp-send-window",
            "128MiB",
            "--udp-socket-recv-buffer",
            "8MiB",
            "--udp-socket-send-buffer",
            "12MiB",
            "--udp-max-streams",
            "512",
        ]);
        let Command::Server {
            udp_stream_receive_window,
            udp_connection_receive_window,
            udp_send_window,
            udp_socket_recv_buffer,
            udp_socket_send_buffer,
            udp_max_streams,
            ..
        } = args.command
        else {
            panic!("expected server command");
        };
        let tuning = parse_udp_tuning(
            &udp_stream_receive_window,
            &udp_connection_receive_window,
            &udp_send_window,
            &udp_socket_recv_buffer,
            &udp_socket_send_buffer,
            udp_max_streams,
        )
        .unwrap();
        assert_eq!(
            tuning,
            UdpDirectTuning {
                stream_receive_window: 32 * 1024 * 1024,
                connection_receive_window: 96 * 1024 * 1024,
                send_window: 128 * 1024 * 1024,
                udp_socket_recv_buffer: 8 * 1024 * 1024,
                udp_socket_send_buffer: 12 * 1024 * 1024,
                max_direct_streams: 512,
            }
        );
    }

    #[test]
    fn server_udp_tuning_env_overrides_defaults() {
        let _guard = ENV_GUARD.lock().unwrap();
        let saved = [
            (
                "BORE_UDP_STREAM_RECEIVE_WINDOW",
                std::env::var_os("BORE_UDP_STREAM_RECEIVE_WINDOW"),
            ),
            (
                "BORE_UDP_CONNECTION_RECEIVE_WINDOW",
                std::env::var_os("BORE_UDP_CONNECTION_RECEIVE_WINDOW"),
            ),
            (
                "BORE_UDP_SEND_WINDOW",
                std::env::var_os("BORE_UDP_SEND_WINDOW"),
            ),
            (
                "BORE_UDP_SOCKET_RECV_BUFFER",
                std::env::var_os("BORE_UDP_SOCKET_RECV_BUFFER"),
            ),
            (
                "BORE_UDP_SOCKET_SEND_BUFFER",
                std::env::var_os("BORE_UDP_SOCKET_SEND_BUFFER"),
            ),
            (
                "BORE_UDP_MAX_STREAMS",
                std::env::var_os("BORE_UDP_MAX_STREAMS"),
            ),
        ];

        std::env::set_var("BORE_UDP_STREAM_RECEIVE_WINDOW", "48MiB");
        std::env::set_var("BORE_UDP_CONNECTION_RECEIVE_WINDOW", "112MiB");
        std::env::set_var("BORE_UDP_SEND_WINDOW", "80MiB");
        std::env::set_var("BORE_UDP_SOCKET_RECV_BUFFER", "24MiB");
        std::env::set_var("BORE_UDP_SOCKET_SEND_BUFFER", "20MiB");
        std::env::set_var("BORE_UDP_MAX_STREAMS", "2048");

        let args = Args::parse_from(["bore", "server"]);
        let Command::Server {
            udp_stream_receive_window,
            udp_connection_receive_window,
            udp_send_window,
            udp_socket_recv_buffer,
            udp_socket_send_buffer,
            udp_max_streams,
            ..
        } = args.command
        else {
            panic!("expected server command");
        };
        let tuning = parse_udp_tuning(
            &udp_stream_receive_window,
            &udp_connection_receive_window,
            &udp_send_window,
            &udp_socket_recv_buffer,
            &udp_socket_send_buffer,
            udp_max_streams,
        )
        .unwrap();
        assert_eq!(
            tuning,
            UdpDirectTuning {
                stream_receive_window: 48 * 1024 * 1024,
                connection_receive_window: 112 * 1024 * 1024,
                send_window: 80 * 1024 * 1024,
                udp_socket_recv_buffer: 24 * 1024 * 1024,
                udp_socket_send_buffer: 20 * 1024 * 1024,
                max_direct_streams: 2048,
            }
        );

        for (key, value) in saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn test_udp_accepts_udp_only_flag() {
        let _guard = ENV_GUARD.lock().unwrap();
        let saved = std::env::var_os("BORE_SERVER");
        std::env::remove_var("BORE_SERVER");

        let args = Args::parse_from(["bore", "test-udp", "--udp-only"]);
        let Command::TestUdp { to, udp_only, .. } = args.command else {
            panic!("expected test-udp command");
        };
        assert_eq!(to.as_deref(), Some(DEFAULT_SERVER));
        assert!(udp_only);

        match saved {
            Some(value) => std::env::set_var("BORE_SERVER", value),
            None => std::env::remove_var("BORE_SERVER"),
        }
    }

    #[test]
    fn local_uses_default_server_when_to_omitted() {
        let _guard = ENV_GUARD.lock().unwrap();
        let saved = std::env::var_os("BORE_SERVER");
        std::env::remove_var("BORE_SERVER");

        let args = Args::parse_from(["bore", "local", "8080"]);
        let Command::Local { to, .. } = args.command else {
            panic!("expected local command");
        };
        assert_eq!(to, DEFAULT_SERVER);

        match saved {
            Some(value) => std::env::set_var("BORE_SERVER", value),
            None => std::env::remove_var("BORE_SERVER"),
        }
    }

    #[test]
    fn proxy_uses_default_server_when_to_omitted() {
        let _guard = ENV_GUARD.lock().unwrap();
        let saved = std::env::var_os("BORE_SERVER");
        std::env::remove_var("BORE_SERVER");

        let args = Args::parse_from([
            "bore",
            "proxy",
            "--local-proxy-port",
            ":5555",
            "--tcp-secret-id",
            "svc",
        ]);
        let Command::Proxy { to, .. } = args.command else {
            panic!("expected proxy command");
        };
        assert_eq!(to, DEFAULT_SERVER);

        match saved {
            Some(value) => std::env::set_var("BORE_SERVER", value),
            None => std::env::remove_var("BORE_SERVER"),
        }
    }

    #[test]
    fn local_udp_without_secret_id_is_a_public_tunnel() {
        // `bore local --udp` with no --tcp-secret-id parses as a PUBLIC tunnel
        // (tcp_secret_id == None). The dispatch warns that the direct-UDP options
        // are inert on the public path and runs the relay; the behavioural
        // assertion (no UDP attempt, warn emitted) lives in the netns e2e harness.
        // This guards that the combo stays parseable and that --udp does not
        // implicitly imply a secret tunnel (BUG-LP1 regression guard).
        let _guard = ENV_GUARD.lock().unwrap();
        let saved_udp = std::env::var_os("BORE_PREFER_UDP");
        let saved_id = std::env::var_os("BORE_TCP_SECRET_ID");
        std::env::remove_var("BORE_PREFER_UDP");
        std::env::remove_var("BORE_TCP_SECRET_ID");

        let args = Args::parse_from(["bore", "local", "8080", "--udp"]);
        let Command::Local {
            udp, tcp_secret_id, ..
        } = args.command
        else {
            panic!("expected local command");
        };
        assert!(udp, "--udp must parse as true");
        assert!(
            tcp_secret_id.is_none(),
            "--udp alone must NOT imply a secret tunnel; it stays a public tunnel"
        );

        match saved_udp {
            Some(value) => std::env::set_var("BORE_PREFER_UDP", value),
            None => std::env::remove_var("BORE_PREFER_UDP"),
        }
        match saved_id {
            Some(value) => std::env::set_var("BORE_TCP_SECRET_ID", value),
            None => std::env::remove_var("BORE_TCP_SECRET_ID"),
        }
    }

    #[test]
    fn local_server_env_overrides_default_server() {
        let _guard = ENV_GUARD.lock().unwrap();
        let saved = std::env::var_os("BORE_SERVER");
        std::env::set_var("BORE_SERVER", "https://env.example.test");

        let args = Args::parse_from(["bore", "local", "8080"]);
        let Command::Local { to, .. } = args.command else {
            panic!("expected local command");
        };
        assert_eq!(to, "https://env.example.test");

        match saved {
            Some(value) => std::env::set_var("BORE_SERVER", value),
            None => std::env::remove_var("BORE_SERVER"),
        }
    }

    #[test]
    fn transfer_listener_accepts_rename_policy() {
        let _guard = ENV_GUARD.lock().unwrap();
        let saved = std::env::var_os("BORE_SERVER");
        std::env::remove_var("BORE_SERVER");

        let args = Args::parse_from([
            "bore",
            "transfer",
            "listener",
            "--dest-path",
            "/tmp/inbox",
            "--rename",
        ]);
        let Command::Transfer { command } = args.command else {
            panic!("expected transfer command");
        };
        let TransferCommand::Listener {
            to,
            rename,
            overwrite,
            ..
        } = command
        else {
            panic!("expected transfer listener command");
        };
        assert_eq!(to, DEFAULT_SERVER);
        assert!(rename);
        assert!(!overwrite);

        match saved {
            Some(value) => std::env::set_var("BORE_SERVER", value),
            None => std::env::remove_var("BORE_SERVER"),
        }
    }

    #[test]
    fn transfer_sender_accepts_stdin_and_output() {
        let _guard = ENV_GUARD.lock().unwrap();
        let saved = std::env::var_os("BORE_SERVER");
        std::env::remove_var("BORE_SERVER");

        let args = Args::parse_from([
            "bore",
            "transfer",
            "sender",
            "--source",
            "stdin",
            "--output",
            "archive.tar.gz",
        ]);
        let Command::Transfer { command } = args.command else {
            panic!("expected transfer command");
        };
        let TransferCommand::Sender {
            to,
            sources,
            output,
            ..
        } = command
        else {
            panic!("expected transfer sender command");
        };
        assert_eq!(to, DEFAULT_SERVER);
        assert_eq!(sources, vec![PathBuf::from("stdin")]);
        assert_eq!(
            output.as_deref(),
            Some(PathBuf::from("archive.tar.gz").as_path())
        );

        match saved {
            Some(value) => std::env::set_var("BORE_SERVER", value),
            None => std::env::remove_var("BORE_SERVER"),
        }
    }

    #[test]
    fn parse_vhost_target_accepts_ip_hostname_and_shorthand() {
        // IPv4 literal.
        assert_eq!(
            parse_vhost_target("127.0.0.1:8080").unwrap(),
            ("127.0.0.1".to_string(), 8080)
        );
        // Hostname (resolved at connect time) — must NOT require an IP literal.
        assert_eq!(
            parse_vhost_target("localhost:8080").unwrap(),
            ("localhost".to_string(), 8080)
        );
        // `:port` shorthand → localhost.
        assert_eq!(
            parse_vhost_target(":8080").unwrap(),
            ("localhost".to_string(), 8080)
        );
        // IPv6 literal.
        assert_eq!(
            parse_vhost_target("[::1]:8080").unwrap(),
            ("::1".to_string(), 8080)
        );
    }

    #[test]
    fn parse_vhost_target_rejects_malformed() {
        assert!(parse_vhost_target("no-port").is_err());
        assert!(parse_vhost_target("localhost:not-a-port").is_err());
        assert!(parse_vhost_target(":99999").is_err()); // port out of u16 range
    }

    #[test]
    fn server_vhost_port_flags_default_to_none() {
        let _guard = ENV_GUARD.lock().unwrap();
        // With no --vhost-http-port/--vhost-https-port flags, the options are None
        // so the dispatch leaves the vhost.yml ports untouched (regression: the old
        // u16 defaults of 80/443 silently clobbered the config).
        let args = Args::parse_from(["bore", "server"]);
        let Command::Server {
            vhost_http_port,
            vhost_https_port,
            ..
        } = args.command
        else {
            panic!("expected server command");
        };
        assert_eq!(vhost_http_port, None);
        assert_eq!(vhost_https_port, None);
    }

    #[test]
    fn server_vhost_port_flags_parse_when_present() {
        let _guard = ENV_GUARD.lock().unwrap();
        let saved_http = std::env::var_os("BORE_VHOST_HTTP_PORT");
        let saved_https = std::env::var_os("BORE_VHOST_HTTPS_PORT");
        let saved_quic = std::env::var_os("BORE_VHOST_QUIC_PORT");
        std::env::remove_var("BORE_VHOST_HTTP_PORT");
        std::env::remove_var("BORE_VHOST_HTTPS_PORT");
        std::env::remove_var("BORE_VHOST_QUIC_PORT");

        let args = Args::parse_from([
            "bore",
            "server",
            "--vhost-http-port",
            "8080",
            "--vhost-https-port",
            "8443",
            "--vhost-quic-port",
            "9443",
        ]);
        let Command::Server {
            vhost_http_port,
            vhost_https_port,
            vhost_quic_port,
            ..
        } = args.command
        else {
            panic!("expected server command");
        };
        assert_eq!(vhost_http_port, Some(8080));
        assert_eq!(vhost_https_port, Some(8443));
        assert_eq!(vhost_quic_port, Some(9443));

        match saved_quic {
            Some(value) => std::env::set_var("BORE_VHOST_QUIC_PORT", value),
            None => std::env::remove_var("BORE_VHOST_QUIC_PORT"),
        }

        match saved_http {
            Some(v) => std::env::set_var("BORE_VHOST_HTTP_PORT", v),
            None => std::env::remove_var("BORE_VHOST_HTTP_PORT"),
        }
        match saved_https {
            Some(v) => std::env::set_var("BORE_VHOST_HTTPS_PORT", v),
            None => std::env::remove_var("BORE_VHOST_HTTPS_PORT"),
        }
    }

    #[test]
    fn vhost_udp_flag_parses() {
        let _guard = ENV_GUARD.lock().unwrap();
        let saved = std::env::var_os("BORE_VHOST_UDP");
        std::env::remove_var("BORE_VHOST_UDP");

        let args = Args::parse_from([
            "bore",
            "vhost",
            "127.0.0.1:8080",
            "--subdomain",
            "myapp",
            "--id",
            "client1",
            "--udp",
        ]);
        let Command::Vhost { udp, .. } = args.command else {
            panic!("expected vhost command");
        };
        assert!(udp);

        match saved {
            Some(value) => std::env::set_var("BORE_VHOST_UDP", value),
            None => std::env::remove_var("BORE_VHOST_UDP"),
        }
    }

    #[test]
    fn server_vhost_config_via_cli_flags() {
        let _guard = ENV_GUARD.lock().unwrap();
        // The vhost frontend is fully configurable without a yaml file: base domain,
        // cert and key all come from flags (env-backed), so a Docker/compose
        // deployment needs no mounted config file for the common case.
        let args = Args::parse_from([
            "bore",
            "server",
            "--vhost-base-domain",
            "bore.example.com",
            "--vhost-cert-file",
            "/certs/fullchain.pem",
            "--vhost-key-file",
            "/certs/privkey.pem",
            "--vhost-mode",
            "both",
        ]);
        let Command::Server {
            vhost_config,
            vhost_base_domain,
            vhost_cert_file,
            vhost_key_file,
            vhost_mode,
            ..
        } = args.command
        else {
            panic!("expected server command");
        };
        assert_eq!(vhost_config, None);
        assert_eq!(vhost_base_domain.as_deref(), Some("bore.example.com"));
        assert_eq!(vhost_cert_file, Some(PathBuf::from("/certs/fullchain.pem")));
        assert_eq!(vhost_key_file, Some(PathBuf::from("/certs/privkey.pem")));
        assert_eq!(vhost_mode.as_deref(), Some("both"));
    }

    #[test]
    fn server_vhost_base_domain_via_env() {
        let _guard = ENV_GUARD.lock().unwrap();
        let saved = std::env::var_os("BORE_VHOST_BASE_DOMAIN");
        std::env::set_var("BORE_VHOST_BASE_DOMAIN", "env.example.com");

        let args = Args::parse_from(["bore", "server"]);
        let Command::Server {
            vhost_base_domain, ..
        } = args.command
        else {
            panic!("expected server command");
        };
        assert_eq!(vhost_base_domain.as_deref(), Some("env.example.com"));

        match saved {
            Some(v) => std::env::set_var("BORE_VHOST_BASE_DOMAIN", v),
            None => std::env::remove_var("BORE_VHOST_BASE_DOMAIN"),
        }
    }

    // ─── VPN CLI tests ────────────────────────────────────────────────────────

    #[cfg(all(feature = "vpn", target_os = "linux"))]
    #[test]
    fn cli_vpn_help_renders() {
        // --help exits with a non-zero code; clap returns Err(DisplayHelp).
        let result = Args::try_parse_from(["bore", "vpn", "--help"]);
        assert!(result.is_err(), "vpn --help should exit (clap Err)");
    }

    #[cfg(all(feature = "vpn", target_os = "linux"))]
    #[test]
    fn cli_vpn_requires_secret() {
        let result = Args::try_parse_from([
            "bore",
            "vpn",
            "listen",
            "--id",
            "x",
            "--to",
            "server.example.com",
        ]);
        assert!(result.is_err(), "listen without --secret must fail");
    }

    #[cfg(all(feature = "vpn", target_os = "linux"))]
    #[test]
    fn cli_vpn_static_requires_peer_addr() {
        // --vpn-peer-addr requires --vpn-addr (clap `requires` constraint).
        let result = Args::try_parse_from([
            "bore",
            "vpn",
            "connect",
            "--to",
            "server.example.com",
            "--secret",
            "s",
            "--id",
            "x",
            "--vpn-peer-addr",
            "10.0.0.2",
        ]);
        assert!(
            result.is_err(),
            "--vpn-peer-addr without --vpn-addr must fail"
        );
    }

    #[cfg(all(feature = "vpn", target_os = "linux"))]
    #[test]
    fn cli_vpn_parses_advertise_list() {
        let args = Args::parse_from([
            "bore",
            "vpn",
            "listen",
            "--to",
            "s",
            "--secret",
            "sec",
            "--id",
            "mylink",
            "--advertise",
            "192.168.1.0/24,192.168.2.0/24",
        ]);
        let Command::Vpn {
            command: VpnCommand::Listen(la),
        } = args.command
        else {
            panic!("expected vpn listen");
        };
        assert_eq!(la.advertise.len(), 2);
        assert_eq!(la.advertise[0], "192.168.1.0/24");
        assert_eq!(la.advertise[1], "192.168.2.0/24");
    }
}
