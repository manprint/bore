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

/// Default timeout (seconds) before re-checking the preferred UDP port after
/// detecting it was remapped by NAT. During this window the socket binds on an
/// ephemeral port so the NAT entry for the preferred port expires naturally.
pub const NAT_UDP_RELEASE_TIMEOUT: u64 = 600;

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

/// Maximum length (in characters) of a user-supplied `--notes` string. Kept well
/// within [`MAX_FRAME_LENGTH`] so the control-channel frame carrying it (alongside
/// the tunnel id / options) never exceeds the codec's limit.
pub const MAX_NOTES_LEN: usize = 256;

/// Per-tunnel options requested by the client for a public-port tunnel.
///
/// No longer `Copy`: it now carries owned `String`s (basic-auth credentials and a
/// free-form note), so the server clones it per proxied connection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TunnelOptions {
    /// Terminate TLS on the tunnel port (the server must have a certificate).
    pub https: bool,
    /// Redirect plain HTTP requests on the tunnel port to `https://`.
    pub force_https: bool,
    /// Optional HTTP Basic auth credentials (`"user:pass"`) the **server** enforces
    /// on the public tunnel port: HTTP requests without valid credentials get a
    /// `401`. Non-HTTP connections are forwarded unprotected. `None` = no auth.
    pub basic_auth: Option<String>,
    /// Optional free-form note shown on the admin status page (no behavior).
    pub notes: Option<String>,
    /// Number of parallel TCP carrier connections the client wants for this
    /// tunnel's data path. `0`/`1` mean the single-connection default (current
    /// behavior). `>1` requests a carrier pool: the server replies with a
    /// [`ServerMessage::CarrierToken`] and the client opens extra connections that
    /// join the pool, spreading proxied connections across several TCP streams to
    /// avoid yamux's single-connection head-of-line blocking. `#[serde(default)]`
    /// keeps the wire format backward-compatible (a missing field reads as `0`).
    #[serde(default)]
    pub carriers: u16,
}

/// Options negotiated by two `bore test-udp` peers once the server pairs them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct UdpTestOptions {
    /// Whether to run throughput tests in addition to the latency checks.
    pub bandwidth: bool,
    /// Bytes sent per direction and per path when [`UdpTestOptions::bandwidth`] is enabled.
    pub transfer_quota: u64,
}

/// Role assigned by the server to a paired `bore test-udp` peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UdpTestRole {
    /// Wait for the peer's direct QUIC connection.
    Listener,
    /// Dial the peer's direct QUIC listener.
    Dialer,
}

/// Compact NAT/candidate summary exchanged between paired `bore test-udp` peers.
///
/// The local command prints the detailed STUN verdict directly; this summary is
/// intentionally small enough to fit comfortably inside the bounded control frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpTestPeerSummary {
    /// NAT class derived from this peer's public STUN probes.
    pub nat_class: String,
    /// Local UDP socket used by the peer for the direct-path test.
    pub local_udp: String,
    /// Primary local IP, if one could be discovered.
    pub primary_local_ip: Option<String>,
    /// Public reflexive mappings reported by public STUN servers.
    pub reflexive: Vec<String>,
    /// Whether the bore server's own STUN responder answered this peer.
    pub bore_stun: Option<bool>,
    /// Number of candidate addresses offered for hole punching.
    pub candidate_count: usize,
    /// Whether the first reflexive mapping preserved the local UDP port.
    pub port_preserved: Option<bool>,
}

/// UDP candidate offer with optional metadata about the STUN server that
/// produced the primary reflexive candidate. The metadata is advisory: the wire
/// path still brokers plain candidate addresses, and peers fall back to the
/// normal STUN chain if the hinted server is unreachable from their network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpCandidateOffer {
    /// Candidate addresses this peer can be punched at.
    pub candidates: Vec<SocketAddr>,
    /// STUN host:port selected by this peer, if a STUN probe succeeded.
    #[serde(default)]
    pub selected_stun: Option<String>,
}

/// A message from the client on the control substream.
#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Response to an authentication challenge from the server.
    Authenticate(String),

    /// Initial client message specifying a port to forward and its options.
    Hello(u16, TunnelOptions),

    /// Register as the provider for a named secret tunnel (no public port).
    /// `notes` is shown on the admin page; `basic_auth` only flags (for display)
    /// that the **provider** enforces HTTP Basic auth itself — the credentials
    /// never leave the provider.
    HelloSecret {
        /// The secret-tunnel identifier to register under.
        id: String,
        /// Optional operator note for the admin status page.
        notes: Option<String>,
        /// Whether the provider enforces HTTP Basic auth itself (display only).
        basic_auth: bool,
        /// Number of parallel TCP carrier connections the provider wants for the
        /// relay data path (server→provider). `0`/`1` = single connection; `>1`
        /// requests a carrier pool (server replies with [`ServerMessage::CarrierToken`]
        /// and round-robins relayed substreams across the joined connections).
        /// `#[serde(default)]` keeps the wire format backward-compatible.
        #[serde(default)]
        carriers: u16,
    },

    /// Connect as a consumer of a named secret tunnel; data substreams opened on
    /// this connection are routed to the matching provider. `notes` is shown on
    /// the admin page.
    ConnectSecret {
        /// The secret-tunnel identifier to connect to.
        id: String,
        /// Optional operator note for the admin status page.
        notes: Option<String>,
    },

    /// Offer this peer's UDP hole-punch candidate addresses to the server, which
    /// brokers them to the other peer of the same secret tunnel. Sent only when
    /// both ends opt into the `udp` direct-path mode. Legacy format without STUN
    /// metadata; new clients should prefer [`ClientMessage::UdpCandidateOffer`].
    UdpCandidates(Vec<SocketAddr>),

    /// Offer UDP candidates plus the selected STUN server metadata. The server
    /// stores the provider's selected STUN and returns it to consumers as a
    /// first-choice hint before they gather their own candidates.
    UdpCandidateOffer(UdpCandidateOffer),

    /// Ask the server which STUN server the registered provider selected, if the
    /// provider is already UDP-capable. Consumers send this before gathering
    /// candidates so the provider's STUN can be tried first.
    UdpStunHintRequest,

    /// First message on an extra connection that joins a public tunnel's carrier
    /// pool. `token` is the per-tunnel value issued in [`ServerMessage::CarrierToken`];
    /// the server matches it to the pending tunnel and adds this connection's
    /// substream opener to the pool. Sent before authenticating (lazy-open reason,
    /// like `Hello`).
    JoinCarrier {
        /// The carrier token issued by the server for this tunnel.
        token: String,
    },

    /// Join a two-peer UDP/NAT diagnostic session. The first peer waits; the
    /// second peer with the same `id` triggers server coordination. This is used
    /// only by `bore test-udp --tcp-secret-id` and is separate from production
    /// tunnel registration.
    TestUdpJoin {
        /// Diagnostic session identifier; both peers must use the same value.
        id: String,
        /// Candidate addresses this peer can be punched at.
        candidates: Vec<SocketAddr>,
        /// Compact local NAT summary shown in the peer's final report.
        summary: UdpTestPeerSummary,
        /// Requested diagnostic options.
        options: UdpTestOptions,
    },
}

/// A message from the server on the control substream.
#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Authentication challenge, sent as the first message, if enabled.
    Challenge(Uuid),

    /// Response to a client's initial message, with actual public port.
    Hello(u16),

    /// Sent after [`ServerMessage::Hello`] when the client requested a carrier
    /// pool (`TunnelOptions::carriers > 1`). `token` authorizes extra connections
    /// to join this tunnel's pool via [`ClientMessage::JoinCarrier`]; `extra` is
    /// how many additional carrier connections the client should open (after the
    /// server clamps the request to its `--max-carriers`; may be `0`).
    CarrierToken {
        /// Per-tunnel token an extra connection presents to join the pool.
        token: String,
        /// Number of additional carrier connections to open.
        extra: u16,
    },

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
        /// STUN server selected by the other peer, when known. Log-only metadata
        /// used to confirm whether both sides aligned on the same STUN server.
        #[serde(default)]
        peer_selected_stun: Option<String>,
    },

    /// Provider-selected STUN server hint returned to a consumer before it
    /// gathers candidates. `None` means no UDP-capable provider or no successful
    /// provider STUN probe is known yet; the consumer should use its normal chain.
    UdpStunHint {
        /// STUN host:port selected by the provider, if available.
        stun_server: Option<String>,
    },

    /// The direct UDP path is unavailable (the other peer did not opt in, or no
    /// peer is registered yet); proceed over the server relay instead.
    UdpUnavailable,

    /// A `bore test-udp` peer is registered and waiting for another peer with the
    /// same diagnostic id.
    TestUdpWaiting,

    /// Start the paired `bore test-udp` run with the peer's candidates and the
    /// assigned direct-QUIC role.
    TestUdpStart {
        /// This peer's role for direct QUIC establishment.
        role: UdpTestRole,
        /// Shared nonce used to derive the direct-path authentication token.
        nonce: [u8; UDP_NONCE_LEN],
        /// The other peer's candidate addresses.
        peer_candidates: Vec<SocketAddr>,
        /// The other peer's compact NAT summary.
        peer_summary: UdpTestPeerSummary,
        /// Effective options both peers will use.
        options: UdpTestOptions,
    },
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

    #[test]
    fn udp_punch_defaults_missing_peer_selected_stun() {
        let msg: ServerMessage = serde_json::from_str(
            r#"{"UdpPunch":{"nonce":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"peer":["127.0.0.1:3478"]}}"#,
        )
        .unwrap();
        match msg {
            ServerMessage::UdpPunch {
                nonce,
                peer,
                peer_selected_stun,
            } => {
                assert_eq!(nonce, [0; UDP_NONCE_LEN]);
                assert_eq!(peer, vec!["127.0.0.1:3478".parse().unwrap()]);
                assert_eq!(peer_selected_stun, None);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
}
