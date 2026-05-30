use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result};
use bore_cli::{client::Client, secret::Proxy, server::Server};
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

        /// IP address to bind to, clients must reach this.
        #[clap(long, default_value = "0.0.0.0")]
        bind_addr: IpAddr,

        /// IP address where tunnels will listen on, defaults to --bind-addr.
        #[clap(long)]
        bind_tunnels: Option<IpAddr>,
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
        } => {
            let client = match tcp_secret_id {
                Some(id) => {
                    Client::new_secret_provider(
                        &local_host,
                        local_port,
                        &to,
                        &id,
                        secret.as_deref(),
                    )
                    .await?
                }
                None => Client::new(&local_host, local_port, &to, port, secret.as_deref()).await?,
            };
            client.listen().await?;
        }
        Command::Proxy {
            local_proxy_port,
            to,
            secret,
            tcp_secret_id,
        } => {
            let bind_addr = parse_proxy_addr(&local_proxy_port)?;
            let proxy = Proxy::new(&to, bind_addr, &tcp_secret_id, secret.as_deref()).await?;
            proxy.listen().await?;
        }
        Command::Server {
            min_port,
            max_port,
            secret,
            max_conns,
            control_port,
            bind_addr,
            bind_tunnels,
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
            server.set_bind_addr(bind_addr);
            server.set_bind_tunnels(bind_tunnels.unwrap_or(bind_addr));
            server.listen().await?;
        }
    }

    Ok(())
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
