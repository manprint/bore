use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result};
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
use tracing::info;

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

        /// Prefer a direct UDP hole-punched path (secret tunnels only, requires
        /// --tcp-secret-id); falls back to the server relay if unavailable.
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

        /// Number of parallel TCP carrier connections for the data path (public
        /// tunnels only). 1 = current single-connection behaviour. >1 spreads
        /// proxied connections across several TCP streams to avoid head-of-line
        /// blocking under concurrency (e.g. many parallel transfers); the server
        /// caps it at its --max-carriers. Ignored for secret tunnels.
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

    /// Secure file and directory transfer over secret tunnels.
    Transfer {
        #[clap(subcommand)]
        command: TransferCommand,
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

        /// Maximum number of parallel TCP carrier connections a single tunnel may
        /// use for its data path (the cap on a client's --carriers request). 1
        /// disables the carrier pool server-wide.
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

        /// Number of relay carrier connections for fallback mode.
        #[clap(long, value_name = "N", default_value_t = 1, env = "BORE_CARRIERS")]
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

        /// Number of relay carrier connections for fallback mode.
        #[clap(long, value_name = "N", default_value_t = 1, env = "BORE_CARRIERS")]
        carriers: u16,

        /// Number of parallel data streams for chunked filesystem transfers.
        /// Each stream maps to one QUIC bidi (direct path) or one yamux substream
        /// (relay). 0 = automatic (cpu-count, min 4). On the relay path, matching
        /// --carriers to --parallel avoids HOL blocking. Stdin always uses one stream.
        #[clap(long, value_name = "N", default_value_t = 0)]
        parallel: u16,

        /// Include or exclude symlinks while scanning the source.
        #[clap(long, value_enum, default_value_t = SymlinkMode::Exclude)]
        symlinks: SymlinkMode,

        /// Include or exclude Unix device nodes while scanning the source.
        #[clap(long, value_enum, default_value_t = DeviceMode::Exclude)]
        devices: DeviceMode,
    },
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
            if udp {
                info!(
                    mode = "local",
                    secret_tunnel = tcp_secret_id.is_some(),
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
            match tcp_secret_id {
                Some(id) => {
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
                    let options = TunnelOptions {
                        https,
                        force_https,
                        basic_auth,
                        notes,
                        carriers,
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
                })
                .await?;
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
            if let Some(domain) = bind_domain {
                server.set_bind_domain(domain);
            }
            match (cert_file, key_file) {
                (Some(cert), Some(key)) => {
                    let acceptor = bore_cli::transport::load_server_tls(&cert, &key)?;
                    server.set_tls(acceptor);
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
            server.listen().await?;
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

/// Parse a proxy bind address. A leading ":" (e.g. ":5555") binds all interfaces.
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
}
