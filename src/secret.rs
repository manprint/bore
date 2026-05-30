//! Named "secret" tunnels: a provider and a consumer rendezvous on the server by
//! a shared `tcp-secret-id`, with no public port allocated.
//!
//! - The **provider** (`bore local --tcp-secret-id <id>`) is an ordinary client
//!   ([`crate::client::Client::new_secret_provider`]) that registers under `id`
//!   instead of requesting a public port.
//! - The **consumer** ([`Proxy`], `bore proxy`) binds a local listener and opens
//!   one substream per accepted connection to the server.
//! - The **server** relays each consumer substream to the registered provider
//!   over a freshly opened substream, splicing the two together. No port is bound.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{error, info, info_span, trace, warn, Instrument};

use crate::auth::Authenticator;
use crate::mux;
use crate::shared::{ClientMessage, Delimited, ServerMessage, PROXY_BUFFER_SIZE};
use crate::transport::{self, Endpoint};

/// Heartbeat interval on secret-tunnel control substreams.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// Registry mapping each `tcp-secret-id` to the provider's substream opener.
pub type Registry = Arc<DashMap<String, mux::Opener>>;

/// Removes a provider registration when the provider connection ends.
struct Deregister {
    registry: Registry,
    id: String,
}

impl Drop for Deregister {
    fn drop(&mut self) {
        self.registry.remove(&self.id);
    }
}

/// Server side: register this connection as the provider for `id`, then keep it
/// alive with heartbeats until it disconnects (which deregisters it).
pub async fn serve_provider(
    mut control: Delimited<mux::Stream>,
    opener: mux::Opener,
    registry: Registry,
    id: String,
) -> Result<()> {
    // Register atomically, rejecting a duplicate id rather than hijacking it.
    match registry.entry(id.clone()) {
        Entry::Occupied(_) => {
            warn!(%id, "secret tunnel id already in use");
            let msg = format!("tcp-secret-id '{id}' already in use");
            control.send(ServerMessage::Error(msg)).await?;
            return Ok(());
        }
        Entry::Vacant(slot) => {
            slot.insert(opener);
        }
    }
    let _guard = Deregister {
        registry: registry.clone(),
        id: id.clone(),
    };
    info!(%id, "secret provider registered");
    control.send(ServerMessage::Ok).await?;

    // The provider sends nothing after registering; a failed heartbeat send means
    // the connection is gone, at which point `_guard` deregisters it.
    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        heartbeat.tick().await;
        if control.send(ServerMessage::Heartbeat).await.is_err() {
            return Ok(());
        }
    }
}

/// Server side: relay every substream the consumer opens to the provider
/// registered under `id`. No port is bound; the server is a pure substream relay.
pub async fn serve_consumer(
    mut control: Delimited<mux::Stream>,
    mut acceptor: mux::Acceptor,
    registry: Registry,
    permits: Arc<Semaphore>,
    id: String,
) -> Result<()> {
    info!(%id, "secret consumer connected");
    control.send(ServerMessage::Ok).await?;

    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if control.send(ServerMessage::Heartbeat).await.is_err() {
                    return Ok(());
                }
            }
            inbound = acceptor.accept() => {
                let Some(consumer_stream) = inbound else {
                    return Ok(());
                };
                // Bound concurrently relayed connections; drop excess under a flood.
                let permit = match Arc::clone(&permits).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        warn!(%id, "too many active connections, dropping");
                        continue;
                    }
                };
                let registry = registry.clone();
                let id = id.clone();
                tokio::spawn(
                    async move {
                        let _permit = permit;
                        if let Err(err) = relay(consumer_stream, registry, &id).await {
                            trace!(%err, "secret relay closed");
                        }
                    }
                    .instrument(info_span!("relay")),
                );
            }
        }
    }
}

/// Splice one consumer substream to a freshly opened provider substream.
async fn relay(mut consumer: mux::Stream, registry: Registry, id: &str) -> Result<()> {
    // Consume the consumer's readiness marker (it announced the substream).
    let mut marker = [0u8; 1];
    consumer.read_exact(&mut marker).await?;

    // Clone the opener out so no DashMap guard is held across an await point.
    let opener = match registry.get(id).map(|entry| entry.value().clone()) {
        Some(opener) => opener,
        None => bail!("no provider registered for '{id}'"),
    };
    let mut provider = opener.open().await.context("provider unavailable")?;
    provider.write_all(&[mux::STREAM_READY]).await?;

    tokio::io::copy_bidirectional_with_sizes(
        &mut consumer,
        &mut provider,
        PROXY_BUFFER_SIZE,
        PROXY_BUFFER_SIZE,
    )
    .await?;
    Ok(())
}

/// Client side of a secret tunnel consumer (`bore proxy`): binds a local listener
/// and forwards each accepted connection to the provider through the server.
pub struct Proxy {
    control: Delimited<mux::Stream>,
    opener: mux::Opener,
    listener: TcpListener,
}

impl Proxy {
    /// Connect to the server, register as a consumer of `tcp_secret_id`, and bind
    /// the local proxy listener.
    pub async fn new(
        to: &str,
        bind_addr: SocketAddr,
        tcp_secret_id: &str,
        secret: Option<&str>,
        insecure: bool,
    ) -> Result<Self> {
        let socket = transport::connect(&Endpoint::parse(to), insecure).await?;
        let (opener, _acceptor) = mux::client(socket);
        let mut control = Delimited::new(
            opener
                .open()
                .await
                .context("failed to open control stream")?,
        );

        // Send the registration first so the lazily-opened substream is announced
        // before the (server-initiated) auth handshake.
        control
            .send(ClientMessage::ConnectSecret(tcp_secret_id.to_string()))
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
            Some(_) => bail!("unexpected response to secret connect"),
            None => bail!("unexpected EOF"),
        }

        let listener = TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("failed to bind {bind_addr}"))?;
        info!(%tcp_secret_id, "proxying {bind_addr} to secret tunnel");

        Ok(Proxy {
            control,
            opener,
            listener,
        })
    }

    /// Returns the local address the proxy is listening on.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Start forwarding: accept local connections, relay each to the provider.
    pub async fn listen(self) -> Result<()> {
        let Proxy {
            mut control,
            opener,
            listener,
        } = self;
        loop {
            tokio::select! {
                // Drain control so server heartbeats are read; surfaces teardown.
                message = control.recv() => {
                    match message? {
                        Some(ServerMessage::Heartbeat) | Some(ServerMessage::Ok) => (),
                        Some(ServerMessage::Error(err)) => error!(%err, "server error"),
                        Some(ServerMessage::Hello(_)) => warn!("unexpected hello"),
                        Some(ServerMessage::Challenge(_)) => warn!("unexpected challenge"),
                        None => return Ok(()),
                    }
                }
                accepted = listener.accept() => {
                    let (local, addr) = match accepted {
                        Ok(pair) => pair,
                        Err(err) => {
                            warn!(%err, "failed to accept local connection");
                            continue;
                        }
                    };
                    let _ = local.set_nodelay(true);
                    let opener = opener.clone();
                    tokio::spawn(
                        async move {
                            if let Err(err) = forward(local, opener).await {
                                warn!(%err, "proxy connection closed with error");
                            }
                        }
                        .instrument(info_span!("proxy", ?addr)),
                    );
                }
            }
        }
    }
}

/// Forward one accepted local connection over a new substream to the server.
async fn forward(mut local: TcpStream, opener: mux::Opener) -> Result<()> {
    let mut stream = opener
        .open()
        .await
        .context("failed to open stream to server")?;
    // Announce the substream so the server routes it even if the local peer waits
    // for the service to speak first; the server consumes this marker byte.
    stream.write_all(&[mux::STREAM_READY]).await?;
    tokio::io::copy_bidirectional_with_sizes(
        &mut local,
        &mut stream,
        PROXY_BUFFER_SIZE,
        PROXY_BUFFER_SIZE,
    )
    .await?;
    Ok(())
}
