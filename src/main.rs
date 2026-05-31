use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result};
use bore_cli::{client::Client, reconnect, secret::Proxy, server::Server, shared::TunnelOptions};
use clap::{error::ErrorKind, CommandFactory, Parser, Subcommand};

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Starts a local proxy to the remote server.
    Local {
        /// The local port to expose.
        #[clap(env = "BORE_LOCAL_PORT")]
        local_port: u16,

        /// The local host to expose.
        #[clap(short, long, value_name = "HOST", default_value = "localhost")]
        local_host: String,

        /// Address of the remote server to expose local ports to.
        #[clap(short, long, env = "BORE_SERVER")]
        to: String,

        /// Optional port on the remote server to select.
        #[clap(short, long, default_value_t = 0)]
        port: u16,

        /// Optional secret for authentication.
        #[clap(short, long, env = "BORE_SECRET", hide_env_values = true)]
        secret: Option<String>,

        /// Register as a named secret tunnel instead of allocating a public port.
        ///
        /// The service is then reachable only through `bore proxy` with the same
        /// id. When set, --port is ignored.
        #[clap(long, env = "BORE_TCP_SECRET_ID")]
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

        /// STUN server (host:port) for UDP candidate discovery; defaults to the
        /// bore server's control endpoint.
        #[clap(long, env = "BORE_STUN_SERVER")]
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
        #[clap(long, env = "BORE_NAT_UDP_PORT", default_value_t = 0)]
        nat_udp_preferred_port: u16,

        /// Reconnect automatically with backoff if the connection fails or drops.
        #[clap(long, env = "BORE_AUTO_RECONNECT")]
        auto_reconnect: bool,
    },

    /// Connects to a named secret tunnel and exposes it on a local port.
    Proxy {
        /// Local address to listen on for the proxied service, e.g. ":5555" (all
        /// interfaces) or "127.0.0.1:5555".
        #[clap(long, env = "BORE_LOCAL_PROXY_PORT")]
        local_proxy_port: String,

        /// Address of the remote server hosting the secret tunnel.
        #[clap(short, long, env = "BORE_SERVER")]
        to: String,

        /// Optional secret for authentication.
        #[clap(short, long, env = "BORE_SECRET", hide_env_values = true)]
        secret: Option<String>,

        /// Identifier of the secret tunnel to connect to (must match the provider).
        #[clap(long, env = "BORE_TCP_SECRET_ID")]
        tcp_secret_id: String,

        /// Skip TLS certificate verification (for self-signed https:// servers).
        #[clap(long, env = "BORE_INSECURE")]
        insecure: bool,

        /// Prefer a direct UDP hole-punched path; falls back to the server relay
        /// if unavailable.
        #[clap(long, env = "BORE_PREFER_UDP")]
        udp: bool,

        /// STUN server (host:port) for UDP candidate discovery; defaults to the
        /// bore server's control endpoint.
        #[clap(long, env = "BORE_STUN_SERVER")]
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
        #[clap(long, env = "BORE_NAT_UDP_PORT", default_value_t = 0)]
        nat_udp_preferred_port: u16,

        /// Reconnect automatically with backoff if the connection fails or drops.
        #[clap(long, env = "BORE_AUTO_RECONNECT")]
        auto_reconnect: bool,
    },

    /// Runs the remote proxy server.
    Server {
        /// Minimum accepted TCP port number.
        #[clap(long, default_value_t = 1024, env = "BORE_MIN_PORT")]
        min_port: u16,

        /// Maximum accepted TCP port number.
        #[clap(long, default_value_t = 65535, env = "BORE_MAX_PORT")]
        max_port: u16,

        /// Optional secret for authentication.
        #[clap(short, long, env = "BORE_SECRET", hide_env_values = true)]
        secret: Option<String>,

        /// Maximum number of concurrently proxied connections per client.
        #[clap(long, default_value_t = bore_cli::server::DEFAULT_MAX_CONNS, env = "BORE_MAX_CONNS")]
        max_conns: usize,

        /// TCP port the control connection listens on.
        #[clap(long, default_value_t = bore_cli::shared::CONTROL_PORT, env = "BORE_CONTROL_PORT")]
        control_port: u16,

        /// Public domain advertised to clients (informational).
        #[clap(long, env = "BORE_BIND_DOMAIN")]
        bind_domain: Option<String>,

        /// Path to a TLS certificate chain (PEM). With --key-file, serves HTTPS.
        #[clap(long, env = "BORE_CERT_FILE")]
        cert_file: Option<String>,

        /// Path to the TLS private key (PEM). With --cert-file, serves HTTPS.
        #[clap(long, env = "BORE_KEY_FILE")]
        key_file: Option<String>,

        /// IP address to bind to, clients must reach this.
        #[clap(long, default_value = "0.0.0.0")]
        bind_addr: IpAddr,

        /// IP address where tunnels will listen on, defaults to --bind-addr.
        #[clap(long)]
        bind_tunnels: Option<IpAddr>,

        /// Broker UDP direct (hole-punched) paths for secret tunnels and run a
        /// STUN responder on the control port.
        #[clap(long, env = "BORE_UDP")]
        udp: bool,
    },

    /// Diagnose this host's UDP / NAT / firewall for hole-punching (opens no
    /// tunnel). Probes public STUN servers (and your --to server's STUN, if
    /// given), classifies the NAT, and prints advice.
    TestUdp {
        /// Optional bore server (host:port or http(s):// URL) to also test the
        /// reachability of its STUN responder.
        #[clap(short, long, env = "BORE_SERVER")]
        to: Option<String>,

        /// Extra STUN server (host:port) to probe alongside the public ones.
        #[clap(long, env = "BORE_STUN_SERVER")]
        stun_server: Option<String>,

        /// Bind the probe to this fixed UDP port (mirrors --nat-udp-preferred-port)
        /// to test whether exactly that port works through a firewall. 0 = random.
        #[clap(long, env = "BORE_NAT_UDP_PORT", default_value_t = 0)]
        nat_udp_preferred_port: u16,
    },
}

#[tokio::main]
async fn run(command: Command) -> Result<()> {
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
            auto_reconnect,
        } => match tcp_secret_id {
            Some(id) => {
                let connect = move || {
                    let (local_host, to, id, secret, stun_server) = (
                        local_host.clone(),
                        to.clone(),
                        id.clone(),
                        secret.clone(),
                        stun_server.clone(),
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
                        )
                        .await
                    }
                };
                reconnect::run(auto_reconnect, connect, serve_client).await?;
            }
            None => {
                let options = TunnelOptions { https, force_https };
                let connect = move || {
                    let (local_host, to, secret) = (local_host.clone(), to.clone(), secret.clone());
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
        },
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
            auto_reconnect,
        } => {
            let bind_addr = parse_proxy_addr(&local_proxy_port)?;
            let connect = move || {
                let (to, tcp_secret_id, secret, stun_server) = (
                    to.clone(),
                    tcp_secret_id.clone(),
                    secret.clone(),
                    stun_server.clone(),
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
                    )
                    .await
                }
            };
            reconnect::run(auto_reconnect, connect, serve_proxy).await?;
        }
        Command::Server {
            min_port,
            max_port,
            secret,
            max_conns,
            control_port,
            bind_domain,
            cert_file,
            key_file,
            bind_addr,
            bind_tunnels,
            udp,
        } => {
            let port_range = min_port..=max_port;
            if port_range.is_empty() {
                Args::command()
                    .error(ErrorKind::InvalidValue, "port range is empty")
                    .exit();
            }
            let mut server = Server::new(port_range, secret.as_deref());
            server.set_max_conns(max_conns);
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
            stun_server,
            nat_udp_preferred_port,
        } => {
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

    Ok(())
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

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    run(Args::parse().command)
}
