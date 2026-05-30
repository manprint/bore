//! Client implementation for the `bore` service.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::{net::TcpStream, time::timeout};
use tracing::{error, info, info_span, warn, Instrument};

use crate::auth::Authenticator;
use crate::mux;
use crate::shared::{
    ClientMessage, Delimited, ServerMessage, TunnelOptions, NETWORK_TIMEOUT, PROXY_BUFFER_SIZE,
};
use crate::transport::{self, Endpoint};

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
        })
    }

    /// Create a client that registers as the provider of a named secret tunnel.
    ///
    /// Unlike [`Client::new`], no public port is allocated on the server: the
    /// service is reached only through a `bore proxy` referencing the same
    /// `tcp-secret-id`. The forwarding behaviour ([`Client::listen`]) is shared.
    pub async fn new_secret_provider(
        local_host: &str,
        local_port: u16,
        to: &str,
        tcp_secret_id: &str,
        secret: Option<&str>,
        insecure: bool,
    ) -> Result<Self> {
        let socket = transport::connect(&Endpoint::parse(to), insecure).await?;
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

        Ok(Client {
            control: Some(control),
            acceptor: Some(acceptor),
            local_host: local_host.to_string(),
            local_port,
            remote_port: 0,
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
        let this = Arc::new(self);
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
                        None => return Ok(()),
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

pub(crate) async fn connect_with_timeout(to: &str, port: u16) -> Result<TcpStream> {
    let stream = match timeout(NETWORK_TIMEOUT, TcpStream::connect((to, port))).await {
        Ok(res) => res,
        Err(err) => Err(err.into()),
    }
    .with_context(|| format!("could not connect to {to}:{port}"))?;
    // Disable Nagle's algorithm: proxied traffic is latency-sensitive and we do
    // our own buffering, so delaying small writes only adds latency.
    stream.set_nodelay(true)?;
    Ok(stream)
}
