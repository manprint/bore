//! Shared data structures, utilities, and protocol definitions.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_util::codec::{AnyDelimiterCodec, Framed, FramedParts};
use tracing::trace;
use uuid::Uuid;

/// TCP port used for control connections with the server.
pub const CONTROL_PORT: u16 = 7835;

/// Maximum byte length for a JSON frame in the stream.
///
/// Large enough to hold a small list of UDP hole-punch candidate addresses
/// (IPv6 + a nonce) used by the `udp` feature's signaling, while still bounding
/// untrusted input on the control channel.
pub const MAX_FRAME_LENGTH: usize = 1024;

/// Number of random bytes in a UDP hole-punch session nonce. The shared QUIC
/// authentication token is derived from this nonce and the tunnel secret.
pub const UDP_NONCE_LEN: usize = 16;

/// Per-direction buffer size used when proxying data between two TCP streams.
///
/// Larger than Tokio's 8 KiB default to improve throughput on connections with a
/// high bandwidth-delay product, at a modest, bounded memory cost per proxied
/// connection.
pub const PROXY_BUFFER_SIZE: usize = 64 * 1024;

/// Timeout for network connections and initial protocol messages.
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(3);

/// Idle time before the first TCP keepalive probe, and the interval between
/// probes. Kept well under common NAT/firewall idle timeouts so that long but
/// quiet transfers (e.g. a slow `tar | rclone rcat`) keep their middlebox
/// mappings alive and dead peers are detected — bore itself never times out an
/// established data stream.
const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(15);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Apply the standard socket options to a proxied or control TCP stream:
/// `TCP_NODELAY` (latency) and `SO_KEEPALIVE` with a short interval (stability).
///
/// Best-effort: failures to set an option are ignored rather than dropping the
/// connection.
pub fn tune_tcp(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let keepalive = TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE_TIME)
        .with_interval(TCP_KEEPALIVE_INTERVAL);
    let _ = SockRef::from(stream).set_tcp_keepalive(&keepalive);
}

/// Per-tunnel options requested by the client for a public-port tunnel.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TunnelOptions {
    /// Terminate TLS on the tunnel port (the server must have a certificate).
    pub https: bool,
    /// Redirect plain HTTP requests on the tunnel port to `https://`.
    pub force_https: bool,
}

/// A message from the client on the control substream.
#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Response to an authentication challenge from the server.
    Authenticate(String),

    /// Initial client message specifying a port to forward and its options.
    Hello(u16, TunnelOptions),

    /// Register as the provider for a named secret tunnel (no public port).
    HelloSecret(String),

    /// Connect as a consumer of a named secret tunnel; data substreams opened on
    /// this connection are routed to the matching provider.
    ConnectSecret(String),

    /// Offer this peer's UDP hole-punch candidate addresses to the server, which
    /// brokers them to the other peer of the same secret tunnel. Sent only when
    /// both ends opt into the `udp` direct-path mode.
    UdpCandidates(Vec<SocketAddr>),
}

/// A message from the server on the control substream.
#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Authentication challenge, sent as the first message, if enabled.
    Challenge(Uuid),

    /// Response to a client's initial message, with actual public port.
    Hello(u16),

    /// Acknowledges a secret-tunnel registration ([`ClientMessage::HelloSecret`]
    /// or [`ClientMessage::ConnectSecret`]).
    Ok,

    /// No-op used to test if the client is still reachable.
    Heartbeat,

    /// Indicates a server error that terminates the connection.
    Error(String),

    /// Begin a UDP hole-punch toward the other peer: `nonce` seeds the shared
    /// QUIC authentication token, `peer` lists the other peer's candidate
    /// addresses to punch toward. The provider acts as the QUIC server, the
    /// consumer as the QUIC client.
    UdpPunch {
        /// Session nonce; both peers derive the same token from it + the secret.
        nonce: [u8; UDP_NONCE_LEN],
        /// The other peer's candidate addresses to send hole-punch packets to.
        peer: Vec<SocketAddr>,
    },

    /// The direct UDP path is unavailable (the other peer did not opt in, or no
    /// peer is registered yet); proceed over the server relay instead.
    UdpUnavailable,
}

/// Transport stream with JSON frames delimited by null characters.
pub struct Delimited<U>(Framed<U, AnyDelimiterCodec>);

impl<U: AsyncRead + AsyncWrite + Unpin> Delimited<U> {
    /// Construct a new delimited stream.
    pub fn new(stream: U) -> Self {
        let codec = AnyDelimiterCodec::new_with_max_length(vec![0], vec![0], MAX_FRAME_LENGTH);
        Self(Framed::new(stream, codec))
    }

    /// Read the next null-delimited JSON instruction from a stream.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        trace!("waiting to receive json message");
        if let Some(next_message) = self.0.next().await {
            let byte_message = next_message.context("frame error, invalid byte length")?;
            let serialized_obj =
                serde_json::from_slice(&byte_message).context("unable to parse message")?;
            Ok(serialized_obj)
        } else {
            Ok(None)
        }
    }

    /// Read the next null-delimited JSON instruction, with a default timeout.
    ///
    /// This is useful for parsing the initial message of a stream for handshake or
    /// other protocol purposes, where we do not want to wait indefinitely.
    pub async fn recv_timeout<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        timeout(NETWORK_TIMEOUT, self.recv())
            .await
            .context("timed out waiting for initial message")?
    }

    /// Send a null-terminated JSON instruction on a stream.
    pub async fn send<T: Serialize>(&mut self, msg: T) -> Result<()> {
        trace!("sending json message");
        self.0.send(serde_json::to_string(&msg)?).await?;
        Ok(())
    }

    /// Consume this object, returning current buffers and the inner transport.
    pub fn into_parts(self) -> FramedParts<U, AnyDelimiterCodec> {
        self.0.into_parts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn tune_tcp_sets_nodelay_and_keepalive() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).await.unwrap();
        tune_tcp(&stream);
        assert!(stream.nodelay().unwrap(), "TCP_NODELAY must be set");
        assert!(
            SockRef::from(&stream).keepalive().unwrap(),
            "SO_KEEPALIVE must be set"
        );
    }
}
