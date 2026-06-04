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

/// Default per-stream QUIC receive window for the direct UDP path.
pub const DIRECT_QUIC_STREAM_RECEIVE_WINDOW: u32 = 16 * 1024 * 1024;

/// Default total QUIC receive window for one direct UDP connection.
pub const DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW: u32 = 64 * 1024 * 1024;

/// Default upper bound on bytes sent but not yet acknowledged on the direct
/// UDP path.
pub const DIRECT_QUIC_SEND_WINDOW: u64 = 64 * 1024 * 1024;

/// Default UDP socket receive buffer requested for each direct-path socket.
pub const DIRECT_UDP_SOCKET_RECV_BUFFER: usize = 16 * 1024 * 1024;

/// Default UDP socket send buffer requested for each direct-path socket.
pub const DIRECT_UDP_SOCKET_SEND_BUFFER: usize = 16 * 1024 * 1024;

/// Default cap on the number of concurrent native QUIC streams on a direct
/// connection.
pub const MAX_DIRECT_STREAMS: u32 = 4096;

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
    /// Skip the TCP relay benchmark and only run the direct UDP path.
    #[serde(default, skip_serializing_if = "is_false")]
    pub udp_only: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    /// Classified roles of the candidate list, in the same order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_kinds: Vec<UdpCandidateKind>,
    /// STUN host:port selected by this peer, if a STUN probe succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_stun: Option<String>,
    /// Whether the bore server's own STUN responder answered this peer.
    pub bore_stun: Option<bool>,
    /// Number of candidate addresses offered for hole punching.
    pub candidate_count: usize,
    /// Whether the first reflexive mapping preserved the local UDP port.
    pub port_preserved: Option<bool>,
}

/// Role assigned to a paired-UDP candidate address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UdpCandidateKind {
    /// STUN-discovered reflexive address.
    Reflexive,
    /// Router-mapped candidate (UPnP-IGD or equivalent).
    RouterMapped,
    /// Predicted port near the reflexive one for symmetric NATs.
    Predicted,
    /// Primary local address for same-LAN peers.
    Local,
}

impl UdpCandidateKind {
    /// Stable lowercase label used in logs and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            UdpCandidateKind::Reflexive => "reflexive",
            UdpCandidateKind::RouterMapped => "router-mapped",
            UdpCandidateKind::Predicted => "predicted",
            UdpCandidateKind::Local => "local",
        }
    }
}

/// Adaptive-plan mode negotiated for a paired `bore test-udp` session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UdpAdaptiveMode {
    /// Try the direct path first and keep the relay as fallback.
    DirectFirst,
    /// Try the direct path with a small retry budget.
    DirectWithRetry,
    /// Prefer the relay first, but still retain the direct candidates.
    RelayFirst,
    /// Skip the direct path and use the relay only.
    RelayOnly,
}

impl UdpAdaptiveMode {
    /// Stable lowercase label used in logs and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            UdpAdaptiveMode::DirectFirst => "direct-first",
            UdpAdaptiveMode::DirectWithRetry => "direct-with-retry",
            UdpAdaptiveMode::RelayFirst => "relay-first",
            UdpAdaptiveMode::RelayOnly => "relay-only",
        }
    }
}

/// Candidate kinds carried by the adaptive plan ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UdpAdaptiveCandidateKind {
    /// STUN-discovered reflexive address.
    Reflexive,
    /// Primary local address for same-LAN peers.
    Local,
    /// Router-mapped candidate (UPnP-IGD or equivalent).
    RouterMapped,
    /// Predicted port near the reflexive one for symmetric NATs.
    Predicted,
    /// Relay fallback if the direct path should be skipped or exhausted.
    RelayFallback,
}

impl UdpAdaptiveCandidateKind {
    /// Stable lowercase label used in logs and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            UdpAdaptiveCandidateKind::Reflexive => "reflexive",
            UdpAdaptiveCandidateKind::Local => "local",
            UdpAdaptiveCandidateKind::RouterMapped => "router-mapped",
            UdpAdaptiveCandidateKind::Predicted => "predicted",
            UdpAdaptiveCandidateKind::RelayFallback => "relay",
        }
    }
}

/// Compact adaptive plan for a paired `bore test-udp` session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpAdaptivePlan {
    /// Overall strategy chosen for the pair.
    pub mode: UdpAdaptiveMode,
    /// Candidate kinds, in the order the direct path should consider them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_order: Vec<UdpAdaptiveCandidateKind>,
    /// Number of direct-path attempts before falling back.
    pub retry_budget: u8,
    /// Preferred read timeout for the direct-path handshake, in milliseconds.
    pub read_timeout_ms: u64,
    /// Delay before retrying a failed direct-path attempt, in milliseconds.
    pub send_delay_ms: u64,
}

impl UdpAdaptivePlan {
    /// Compact human-readable summary used in the paired diagnostic.
    pub fn summary(&self) -> String {
        let order = self
            .candidate_order
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>()
            .join(" -> ");
        format!(
            "{} (retry {}, read {}ms, delay {}ms, order {})",
            self.mode.as_str(),
            self.retry_budget,
            self.read_timeout_ms,
            self.send_delay_ms,
            order
        )
    }
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

/// Bandwidth-oriented tuning for the direct UDP path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpDirectTuning {
    /// Per-stream QUIC receive window.
    pub stream_receive_window: u32,
    /// Total QUIC receive window for one direct connection.
    pub connection_receive_window: u32,
    /// Bytes sent but not yet acknowledged before the sender blocks.
    pub send_window: u64,
    /// Requested UDP receive buffer.
    pub udp_socket_recv_buffer: usize,
    /// Requested UDP send buffer.
    pub udp_socket_send_buffer: usize,
    /// Max concurrent QUIC bidi streams on the direct connection.
    pub max_direct_streams: u32,
}

impl Default for UdpDirectTuning {
    fn default() -> Self {
        Self {
            stream_receive_window: DIRECT_QUIC_STREAM_RECEIVE_WINDOW,
            connection_receive_window: DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW,
            send_window: DIRECT_QUIC_SEND_WINDOW,
            udp_socket_recv_buffer: DIRECT_UDP_SOCKET_RECV_BUFFER,
            udp_socket_send_buffer: DIRECT_UDP_SOCKET_SEND_BUFFER,
            max_direct_streams: MAX_DIRECT_STREAMS,
        }
    }
}

impl TunnelOptions {
    fn control_frame_summary(&self) -> String {
        format!(
            "https={}, force_https={}, basic_auth={}, notes={}, carriers={}",
            if self.https { "on" } else { "off" },
            if self.force_https { "on" } else { "off" },
            if self.basic_auth.is_some() {
                "on"
            } else {
                "off"
            },
            if self.notes.is_some() {
                "present"
            } else {
                "none"
            },
            self.carriers,
        )
    }
}

impl UdpTestOptions {
    fn control_frame_summary(&self) -> String {
        format!(
            "bandwidth={}, transfer_quota={}, udp_only={}",
            if self.bandwidth { "on" } else { "off" },
            self.transfer_quota,
            if self.udp_only { "on" } else { "off" },
        )
    }
}

impl UdpTestPeerSummary {
    fn control_frame_summary(&self) -> String {
        let candidate_kinds = if self.candidate_kinds.is_empty() {
            "none".to_string()
        } else {
            self.candidate_kinds
                .iter()
                .map(|kind| kind.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "nat_class={}, local_udp={}, primary_local_ip={}, selected_stun={}, bore_stun={}, candidate_count={}, port_preserved={}, candidate_kinds=[{}], reflexive={:?}",
            self.nat_class,
            self.local_udp,
            self.primary_local_ip.as_deref().unwrap_or("<none>"),
            self.selected_stun.as_deref().unwrap_or("<none>"),
            match self.bore_stun {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unknown",
            },
            self.candidate_count,
            match self.port_preserved {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unknown",
            },
            candidate_kinds,
            self.reflexive,
        )
    }
}

impl UdpCandidateOffer {
    fn control_frame_summary(&self) -> String {
        format!(
            "selected_stun={}, candidate_count={}, candidates={:?}",
            self.selected_stun.as_deref().unwrap_or("<none>"),
            self.candidates.len(),
            self.candidates,
        )
    }
}

impl UdpDirectTuning {
    fn control_frame_summary(&self) -> String {
        format!(
            "stream_recv={}, conn_recv={}, send={}, sock_recv={}, sock_send={}, max_streams={}",
            self.stream_receive_window,
            self.connection_receive_window,
            self.send_window,
            self.udp_socket_recv_buffer,
            self.udp_socket_send_buffer,
            self.max_direct_streams,
        )
    }
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
#[allow(clippy::large_enum_variant)]
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
        /// Direct-UDP transport tuning requested by the server.
        #[serde(default)]
        tuning: UdpDirectTuning,
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
        /// Direct-UDP transport tuning requested by the server.
        #[serde(default)]
        tuning: UdpDirectTuning,
        /// Adaptive NAT plan chosen by the server for this paired diagnostic.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        adaptive_plan: Option<UdpAdaptivePlan>,
    },
}

#[doc(hidden)]
pub trait ControlFrameSummary {
    fn control_frame_summary(&self) -> String;
}

impl ControlFrameSummary for ClientMessage {
    fn control_frame_summary(&self) -> String {
        match self {
            ClientMessage::Authenticate(_) => "Authenticate { tag=<redacted> }".to_string(),
            ClientMessage::Hello(port, options) => {
                format!(
                    "Hello {{ port={}, options={{ {} }} }}",
                    port,
                    options.control_frame_summary()
                )
            }
            ClientMessage::HelloSecret {
                id,
                notes,
                basic_auth,
                carriers,
            } => {
                format!(
                    "HelloSecret {{ id={}, notes={}, basic_auth={}, carriers={} }}",
                    id,
                    if notes.is_some() { "present" } else { "none" },
                    if *basic_auth { "on" } else { "off" },
                    carriers,
                )
            }
            ClientMessage::ConnectSecret { id, notes } => {
                format!(
                    "ConnectSecret {{ id={}, notes={} }}",
                    id,
                    if notes.is_some() { "present" } else { "none" },
                )
            }
            ClientMessage::UdpCandidates(candidates) => {
                format!("UdpCandidates {{ candidates={:?} }}", candidates)
            }
            ClientMessage::UdpCandidateOffer(offer) => {
                format!("UdpCandidateOffer {{ {} }}", offer.control_frame_summary())
            }
            ClientMessage::UdpStunHintRequest => "UdpStunHintRequest".to_string(),
            ClientMessage::JoinCarrier { .. } => "JoinCarrier { token=<redacted> }".to_string(),
            ClientMessage::TestUdpJoin {
                id,
                candidates,
                summary,
                options,
            } => {
                format!(
                    "TestUdpJoin {{ id={}, candidates={:?}, summary={{ {} }}, options={{ {} }} }}",
                    id,
                    candidates,
                    summary.control_frame_summary(),
                    options.control_frame_summary(),
                )
            }
        }
    }
}

impl ControlFrameSummary for ServerMessage {
    fn control_frame_summary(&self) -> String {
        match self {
            ServerMessage::Challenge(challenge) => {
                format!("Challenge {{ challenge={} }}", challenge)
            }
            ServerMessage::Hello(port) => format!("Hello {{ port={} }}", port),
            ServerMessage::CarrierToken { token: _, extra } => {
                format!("CarrierToken {{ token=<redacted>, extra={} }}", extra)
            }
            ServerMessage::Ok => "Ok".to_string(),
            ServerMessage::Heartbeat => "Heartbeat".to_string(),
            ServerMessage::Error(err) => format!("Error {{ message={} }}", err),
            ServerMessage::UdpPunch {
                nonce,
                peer,
                peer_selected_stun,
                tuning,
            } => {
                format!(
                    "UdpPunch {{ nonce={}, peer={:?}, peer_selected_stun={}, tuning={{ {} }} }}",
                    hex::encode(nonce),
                    peer,
                    peer_selected_stun.as_deref().unwrap_or("<none>"),
                    tuning.control_frame_summary(),
                )
            }
            ServerMessage::UdpStunHint { stun_server } => {
                format!(
                    "UdpStunHint {{ stun_server={} }}",
                    stun_server.as_deref().unwrap_or("<none>")
                )
            }
            ServerMessage::UdpUnavailable => "UdpUnavailable".to_string(),
            ServerMessage::TestUdpWaiting => "TestUdpWaiting".to_string(),
            ServerMessage::TestUdpStart {
                role,
                nonce,
                peer_candidates,
                peer_summary,
                options,
                tuning,
                adaptive_plan,
            } => {
                format!(
                    "TestUdpStart {{ role={:?}, nonce={}, peer_candidates={:?}, peer_summary={{ {} }}, options={{ {} }}, tuning={{ {} }}, adaptive_plan={} }}",
                    role,
                    hex::encode(nonce),
                    peer_candidates,
                    peer_summary.control_frame_summary(),
                    options.control_frame_summary(),
                    tuning.control_frame_summary(),
                    adaptive_plan.as_ref().map(|plan| plan.summary()).unwrap_or_else(|| "<none>".to_string()),
                )
            }
        }
    }
}

/// Transport stream with JSON frames delimited by null characters.
pub struct Delimited<U> {
    framed: Framed<U, AnyDelimiterCodec>,
    label: &'static str,
}

impl<U: AsyncRead + AsyncWrite + Unpin> Delimited<U> {
    /// Construct a new delimited stream.
    pub fn new(stream: U) -> Self {
        Self::with_label(stream, "control")
    }

    /// Construct a new delimited stream with a log label.
    pub fn with_label(stream: U, label: &'static str) -> Self {
        let codec = AnyDelimiterCodec::new_with_max_length(vec![0], vec![0], MAX_FRAME_LENGTH);
        Self {
            framed: Framed::new(stream, codec),
            label,
        }
    }

    /// Read the next null-delimited JSON instruction from a stream.
    pub async fn recv<T: DeserializeOwned + ControlFrameSummary>(&mut self) -> Result<Option<T>> {
        trace!(
            control = self.label,
            direction = "rx",
            "waiting for control frame"
        );
        if let Some(next_message) = self.framed.next().await {
            let byte_message = next_message.context("frame error, invalid byte length")?;
            let serialized_obj: T =
                serde_json::from_slice(&byte_message).context("unable to parse message")?;
            trace!(
                control = self.label,
                direction = "rx",
                frame = %serialized_obj.control_frame_summary(),
                "control frame received"
            );
            Ok(Some(serialized_obj))
        } else {
            trace!(
                control = self.label,
                direction = "rx",
                frame = "<eof>",
                "control frame closed"
            );
            Ok(None)
        }
    }

    /// Read the next null-delimited JSON instruction, with a default timeout.
    ///
    /// This is useful for parsing the initial message of a stream for handshake or
    /// other protocol purposes, where we do not want to wait indefinitely.
    pub async fn recv_timeout<T: DeserializeOwned + ControlFrameSummary>(
        &mut self,
    ) -> Result<Option<T>> {
        timeout(NETWORK_TIMEOUT, self.recv())
            .await
            .context("timed out waiting for initial message")?
    }

    /// Send a null-terminated JSON instruction on a stream.
    pub async fn send<T: Serialize + ControlFrameSummary>(&mut self, msg: T) -> Result<()> {
        trace!(
            control = self.label,
            direction = "tx",
            frame = %msg.control_frame_summary(),
            "control frame sent"
        );
        self.framed.send(serde_json::to_string(&msg)?).await?;
        Ok(())
    }

    /// Consume this object, returning current buffers and the inner transport.
    pub fn into_parts(self) -> FramedParts<U, AnyDelimiterCodec> {
        self.framed.into_parts()
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
                tuning,
            } => {
                assert_eq!(nonce, [0; UDP_NONCE_LEN]);
                assert_eq!(peer, vec!["127.0.0.1:3478".parse().unwrap()]);
                assert_eq!(peer_selected_stun, None);
                assert_eq!(tuning, UdpDirectTuning::default());
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn test_udp_start_defaults_tuning() {
        let msg: ServerMessage = serde_json::from_str(
            r#"{"TestUdpStart":{"role":"Listener","nonce":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"peer_candidates":["127.0.0.1:3478"],"peer_summary":{"nat_class":"Open","local_udp":"127.0.0.1:12345","primary_local_ip":null,"reflexive":[],"bore_stun":null,"candidate_count":1,"port_preserved":null},"options":{"bandwidth":false,"transfer_quota":1024}}}"#,
        )
        .unwrap();
        match msg {
            ServerMessage::TestUdpStart {
                role,
                nonce,
                peer_candidates,
                peer_summary,
                options,
                tuning,
                adaptive_plan,
            } => {
                assert_eq!(role, UdpTestRole::Listener);
                assert_eq!(nonce, [0; UDP_NONCE_LEN]);
                assert_eq!(peer_candidates, vec!["127.0.0.1:3478".parse().unwrap()]);
                assert_eq!(peer_summary.nat_class, "Open");
                assert!(peer_summary.candidate_kinds.is_empty());
                assert_eq!(peer_summary.selected_stun, None);
                assert!(!options.bandwidth);
                assert_eq!(options.transfer_quota, 1024);
                assert!(!options.udp_only);
                assert_eq!(tuning, UdpDirectTuning::default());
                assert!(adaptive_plan.is_none());
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
}

#[test]
fn control_frame_summary_redacts_sensitive_fields() {
    assert_eq!(
        ClientMessage::Authenticate("secret-tag".into()).control_frame_summary(),
        "Authenticate { tag=<redacted> }"
    );
    assert_eq!(
        ClientMessage::JoinCarrier {
            token: "carrier-token".into(),
        }
        .control_frame_summary(),
        "JoinCarrier { token=<redacted> }"
    );
    assert!(ServerMessage::CarrierToken {
        token: "server-token".into(),
        extra: 2,
    }
    .control_frame_summary()
    .contains("token=<redacted>"));
}

#[test]
fn control_frame_summary_includes_test_udp_plan() {
    let msg = ServerMessage::TestUdpStart {
        role: UdpTestRole::Dialer,
        nonce: [1; UDP_NONCE_LEN],
        peer_candidates: vec!["127.0.0.1:3478".parse().unwrap()],
        peer_summary: UdpTestPeerSummary {
            nat_class: "Cone".to_string(),
            local_udp: "127.0.0.1:12345".to_string(),
            primary_local_ip: Some("127.0.0.1".to_string()),
            reflexive: vec!["198.51.100.10:12345".to_string()],
            candidate_kinds: vec![UdpCandidateKind::Reflexive],
            selected_stun: Some("stun.cloudflare.com:3478".to_string()),
            bore_stun: Some(true),
            candidate_count: 1,
            port_preserved: Some(true),
        },
        options: UdpTestOptions {
            bandwidth: true,
            transfer_quota: 1024,
            udp_only: true,
        },
        tuning: UdpDirectTuning::default(),
        adaptive_plan: Some(UdpAdaptivePlan {
            mode: UdpAdaptiveMode::DirectFirst,
            candidate_order: vec![UdpAdaptiveCandidateKind::Reflexive],
            retry_budget: 1,
            read_timeout_ms: 750,
            send_delay_ms: 0,
        }),
    };

    let summary = msg.control_frame_summary();
    assert!(summary.contains("TestUdpStart"));
    assert!(summary.contains("adaptive_plan=direct-first"));
    assert!(summary.contains("nonce=01010101010101010101010101010101"));
}
