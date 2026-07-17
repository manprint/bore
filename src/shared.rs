//! Shared data structures, utilities, and protocol definitions.

use std::net::SocketAddr;
use std::sync::OnceLock;
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
/// Sized for the largest legitimate control frame: an `UdpPunch` carrying a
/// full candidate-model-v2 rider — up to [`crate::holepunch::MAX_UDP_CANDIDATES`]
/// (16) addresses BOTH as the legacy plain list and as typed candidates, plus
/// capabilities, the structured NAT profile, and the server-computed adaptive
/// plan (plan Fase 3; ~2.5 KiB worst case at 16 candidates). The old 1024-byte
/// cap silently broke UDP negotiation once port prediction pushed a v2 offer
/// past it ("frame error, invalid byte length" → relay fallback). Still a
/// tight bound on untrusted control-channel input.
pub const MAX_FRAME_LENGTH: usize = 8192;

/// Number of random bytes in a UDP hole-punch session nonce. The shared QUIC
/// authentication token is derived from this nonce and the tunnel secret.
pub const UDP_NONCE_LEN: usize = 16;

/// Transparent byte-counting wrapper around a relay stream: bytes READ from the
/// inner stream bump `rx`/`grx`, bytes WRITTEN to it bump `tx`/`gtx`. The adds
/// happen on every poll, so the per-entry and global counters reflect traffic
/// LIVE — while the connection is still open — not only when it closes. Used by
/// every data-plane splice (public/secret/vhost/vpn relay) so the admin page's
/// TX/RX columns update for long-lived connections instead of reading 0 forever.
///
/// It is byte-for-byte transparent (only atomic adds; never alters or buffers
/// payload) and forwards `poll_shutdown`, so half-close propagation is preserved.
pub(crate) struct CountingStream<S> {
    pub(crate) inner: S,
    /// Counter for bytes read from `inner` (per-entry rx).
    pub(crate) rx: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Counter for bytes written to `inner` (per-entry tx).
    pub(crate) tx: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Global rx counter (server total).
    pub(crate) grx: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Global tx counter (server total).
    pub(crate) gtx: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<S> CountingStream<S> {
    /// Wrap `inner`, counting reads into `rx`/`grx` and writes into `tx`/`gtx`.
    pub(crate) fn new(
        inner: S,
        rx: std::sync::Arc<std::sync::atomic::AtomicU64>,
        tx: std::sync::Arc<std::sync::atomic::AtomicU64>,
        grx: std::sync::Arc<std::sync::atomic::AtomicU64>,
        gtx: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            inner,
            rx,
            tx,
            grx,
            gtx,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for CountingStream<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let res = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &res {
            let n = (buf.filled().len() - before) as u64;
            self.rx.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            self.grx.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        }
        res
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CountingStream<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let res = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &res {
            self.tx
                .fetch_add(*n as u64, std::sync::atomic::Ordering::Relaxed);
            self.gtx
                .fetch_add(*n as u64, std::sync::atomic::Ordering::Relaxed);
        }
        res
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Serde default for VPN relay carrier counts: old peers without the field
/// behave exactly like a single-carrier build.
fn default_vpn_carriers() -> u16 {
    1
}

/// Default per-direction buffer used when proxying data between two streams.
///
/// 256 KiB is tuned for large-file throughput on high bandwidth-delay-product
/// links (far above Tokio's 8 KiB default), while keeping per-connection memory
/// bounded. Override at runtime with [`proxy_buffer_size`].
pub const DEFAULT_PROXY_BUFFER_SIZE: usize = 256 * 1024;

/// Lower clamp for the resolved proxy buffer size (a tiny buffer would throttle
/// throughput and is almost never intended).
const MIN_PROXY_BUFFER_SIZE: usize = 4 * 1024;

/// Upper clamp for the resolved proxy buffer size. Bounds the worst-case memory
/// an operator can request per proxied direction.
const MAX_PROXY_BUFFER_SIZE: usize = 16 * 1024 * 1024;

/// Per-direction buffer size used when proxying data between two streams.
///
/// Honors the `BORE_PROXY_BUFFER_SIZE` environment variable (raw bytes or a
/// `KB`/`MB`/`GB`/`KiB`/`MiB`/`GiB` suffix), clamped to
/// `[4 KiB, 16 MiB]`; defaults to [`DEFAULT_PROXY_BUFFER_SIZE`] when unset or
/// unparseable. Resolved once and cached for the process lifetime.
///
/// Set it on the **server** to size the relay-side copy buffers (public tunnel,
/// secret relay, and vhost), and on a **client/provider** process (`bore local`,
/// `bore proxy`, `bore vhost`) to size that side's local splice. A larger buffer
/// trades memory for fewer wakeups on high-throughput, high-latency links.
pub fn proxy_buffer_size() -> usize {
    static SIZE: OnceLock<usize> = OnceLock::new();
    *SIZE.get_or_init(|| match std::env::var("BORE_PROXY_BUFFER_SIZE") {
        Ok(raw) => match parse_size_bytes(&raw) {
            Some(bytes) => {
                let resolved = (bytes as usize).clamp(MIN_PROXY_BUFFER_SIZE, MAX_PROXY_BUFFER_SIZE);
                trace!(
                    requested = bytes,
                    resolved,
                    "proxy buffer size set via BORE_PROXY_BUFFER_SIZE"
                );
                resolved
            }
            None => {
                trace!(
                    value = %raw,
                    "ignoring unparseable BORE_PROXY_BUFFER_SIZE; using default"
                );
                DEFAULT_PROXY_BUFFER_SIZE
            }
        },
        Err(_) => DEFAULT_PROXY_BUFFER_SIZE,
    })
}

/// Parse a byte size with an optional unit suffix (`B`/`KB`/`MB`/`GB` decimal,
/// `KiB`/`MiB`/`GiB` binary). Returns `None` for empty, non-numeric, or
/// overflowing input. Shared shape with the server's `--udp-*` size parsing.
pub fn parse_size_bytes(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let split_at = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split_at);
    let bytes: u64 = number.parse().ok()?;
    let multiplier = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "ki" | "kib" => 1024,
        "mi" | "mib" => 1024 * 1024,
        "gi" | "gib" => 1024 * 1024 * 1024,
        _ => return None,
    };
    bytes.checked_mul(multiplier)
}

/// Timeout for network connections and initial protocol messages.
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(3);

/// Default timeout (seconds) before re-checking the preferred UDP port after
/// detecting it was remapped by NAT. During this window the socket binds on an
/// ephemeral port so the NAT entry for the preferred port expires naturally.
pub const NAT_UDP_RELEASE_TIMEOUT: u64 = 600;

/// Default per-stream QUIC receive window for the direct UDP path.
pub const DIRECT_QUIC_STREAM_RECEIVE_WINDOW: u32 = 16 * 1024 * 1024;

/// Default total QUIC receive window for one direct UDP connection.
///
/// CRITICAL for the multiplexed `local`/`vhost --udp` paths: a single direct
/// connection (the `--carriers 1` default) carries EVERY proxied connection as
/// its own QUIC bidi stream. The receiver only returns connection-level
/// flow-control credit as it DRAINS each stream into its public socket, so a
/// slow/paused public reader (a browser pausing some assets while it parses
/// blocking CSS/JS) lets quinn buffer up to one full `STREAM_RECEIVE_WINDOW`
/// (16 MiB) of unread data per stalled stream AGAINST this shared window. Once
/// `CONNECTION_RECEIVE_WINDOW / STREAM_RECEIVE_WINDOW` streams stall, the whole
/// connection runs out of credit and EVERY other stream on it starves —
/// requests hang "pending" until a stalled reader drains. At 64 MiB only ~4
/// stalled streams triggered this (the carriers=1 stall reproduced by
/// `scripts/vhost_udp_concurrency_repro.sh`). 256 MiB tolerates ~16 — the same
/// headroom four 64 MiB carriers gave, now in the single-connection default.
/// It is a CEILING (quinn only buffers what actually arrives unread), not a
/// reservation, so a healthy tunnel that keeps draining costs ~0. Tunable via
/// the direct-path tuning flags for hosts that need a tighter memory bound.
pub const DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW: u32 = 256 * 1024 * 1024;

/// Default upper bound on bytes sent but not yet acknowledged on the direct
/// UDP path. Kept at parity with the connection receive window so the sender's
/// own connection-level send budget never becomes the bottleneck before the
/// peer's advertised receive window does.
pub const DIRECT_QUIC_SEND_WINDOW: u64 = 256 * 1024 * 1024;

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

/// Per-tunnel HTTPS behavior requested by the client. `None` (an `Option<HttpsPolicy>`)
/// means "inherit the server default".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum HttpsPolicy {
    /// No TLS termination, no redirect; plain HTTP/raw only.
    Off,
    /// Terminate TLS; serve both HTTP and HTTPS; no redirect.
    On,
    /// Terminate TLS and 308-redirect plain HTTP to HTTPS.
    Redirect,
}

/// Resolve a requested per-tunnel HTTPS policy against server capability into
/// the effective `(https, force_https)` edge flags.
///
/// `capable` = the server can terminate TLS for this tunnel family
/// (public: `self.tls.is_some()`; vhost: cert present and mode serves https).
/// Returns `(https, force_https, downgraded)` where `downgraded` is true when
/// the client asked for TLS (`On`/`Redirect`) but the server is not capable,
/// so the caller must warn and fall back to plain HTTP.
pub fn resolve_https_policy(policy: HttpsPolicy, capable: bool) -> (bool, bool, bool) {
    match policy {
        HttpsPolicy::Off => (false, false, false),
        HttpsPolicy::On if capable => (true, false, false),
        HttpsPolicy::On => (false, false, true),
        HttpsPolicy::Redirect if capable => (true, true, false),
        HttpsPolicy::Redirect => (false, false, true),
    }
}

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
    /// Whether the client wants the QUIC direct data path for this public tunnel.
    /// `#[serde(default)]` keeps the wire format backward-compatible.
    #[serde(default)]
    pub udp: bool,
    /// Whether the client runs with `--auto-reconnect` (a client-side reconnect
    /// loop). Sent purely so the admin status page can show it; the server takes
    /// no action on it. `#[serde(default)]` keeps the wire format
    /// backward-compatible (an old client omits it ⇒ reads as `false`).
    #[serde(default)]
    pub auto_reconnect: bool,
    /// Whether the client wants access logging with real caller IP forwarding.
    /// `#[serde(default)]` keeps the wire format backward-compatible.
    #[serde(default)]
    pub webserver_log: bool,
    /// The client's `--max-conns` cap on concurrent proxied connections.
    /// Display-only. `#[serde(default)]` keeps the wire format backward-compatible.
    #[serde(default)]
    pub max_conns: usize,
    /// The client's local service host (`-l/--local-host`). Display-only.
    #[serde(default)]
    pub local_host: Option<String>,
    /// The client's local service port. `0` = unknown. Display-only.
    #[serde(default)]
    pub local_port: u16,
    /// Per-tunnel HTTPS policy. `None` = inherit the server default (byte-identical
    /// to the pre-policy behavior). `#[serde(default)]` keeps the wire backward-compatible.
    #[serde(default)]
    pub https_policy: Option<HttpsPolicy>,
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

/// Capability label a peer advertises in [`UdpCandidateOffer::capabilities`]
/// when it understands the typed candidate model v2 (plan Fase 1). V2
/// semantics only ever activate when BOTH peers advertise it; absent ⇒ the
/// legacy `candidates: Vec<SocketAddr>` path is authoritative.
pub const UDP_CAP_CANDIDATE_V2: &str = "cand-v2";

/// Capability label for authenticated connectivity checks (plan Fase 2):
/// HMAC request/response probes that validate a candidate pair and learn
/// peer-reflexive sources BEFORE QUIC. Only active when BOTH peers advertise
/// it; a legacy peer keeps the blind punch + dial-all path byte-identical.
pub const UDP_CAP_CHECK_V1: &str = "check-v1";

/// Observed NAT *mapping* behaviour in RFC 4787 terms, self-reported in a
/// structured enum so the server-side policy never parses text labels (plan
/// Fase 3). Derived live from two independent STUN observations on the same
/// socket: identical mapped addresses ⇒ endpoint-independent, different ⇒
/// symmetric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum UdpNatMapping {
    /// Fewer than two observations — never treated as hostile by the policy.
    #[default]
    Unknown,
    /// Endpoint-independent mapping: same external `ip:port` toward any peer.
    Eim,
    /// Address/port-dependent mapping (symmetric NAT): fresh mapping per
    /// destination, so the STUN-observed port is not the port the peer sees.
    Symmetric,
}

impl UdpNatMapping {
    /// Stable lowercase label used in logs and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            UdpNatMapping::Unknown => "unknown",
            UdpNatMapping::Eim => "eim",
            UdpNatMapping::Symmetric => "symmetric",
        }
    }
}

/// Observed NAT *filtering* behaviour (RFC 4787). A live gather cannot observe
/// filtering without a two-IP STUN server (plan Fase 6), so this is always
/// `Unknown` today — carried on the wire so the policy and frames need no
/// change when Fase 6 lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum UdpNatFiltering {
    /// Not observed (the only value produced today).
    #[default]
    Unknown,
    /// Endpoint-independent filtering (full cone).
    Eif,
    /// Address- or port-dependent filtering.
    AddressDependent,
}

impl UdpNatFiltering {
    /// Stable lowercase label used in logs and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            UdpNatFiltering::Unknown => "unknown",
            UdpNatFiltering::Eif => "eif",
            UdpNatFiltering::AddressDependent => "address-dependent",
        }
    }
}

/// Structured NAT self-profile attached to a candidate offer (plan Fase 3).
///
/// The server computes an adaptive plan ONLY when BOTH peers attach one;
/// absence keeps the legacy behaviour (a missing profile must NEVER be read
/// as "relay only"). All fields are additive serde defaults so legacy frames
/// stay byte-identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UdpNatProfile {
    /// Observed mapping behaviour.
    #[serde(default)]
    pub mapping: UdpNatMapping,
    /// Observed filtering behaviour (always `Unknown` until Fase 6).
    #[serde(default)]
    pub filtering: UdpNatFiltering,
    /// Whether the NAT preserved the local UDP port in the mapping.
    #[serde(default)]
    pub port_preserved: Option<bool>,
    /// Independent STUN observations backing `mapping` — the policy's
    /// confidence signal (`< 2` ⇒ mapping is at best a guess).
    #[serde(default)]
    pub observations: u8,
}

impl UdpNatProfile {
    /// Compact log label: `mapping/filtering/port_preserved (N obs)`.
    pub fn summary(&self) -> String {
        format!(
            "{}/{} port_preserved={} ({} obs)",
            self.mapping.as_str(),
            self.filtering.as_str(),
            match self.port_preserved {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unknown",
            },
            self.observations
        )
    }
}

/// One typed hole-punch candidate (candidate model v2, plan Fase 1).
///
/// Carried ALONGSIDE the legacy plain-address list — same addresses, extra
/// metadata — so an old peer keeps working off `candidates` unchanged.
/// Observe-only in Fase 1 (logged, not yet steering connects).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpTypedCandidate {
    /// The candidate address (also present in the legacy list).
    pub addr: SocketAddr,
    /// How this candidate was obtained.
    pub kind: UdpCandidateKind,
    /// Relative preference, higher = try earlier (advisory until Fase 3).
    #[serde(default)]
    pub priority: u32,
}

/// V2 traversal metadata rider on [`ServerMessage::UdpPunch`] (plan Fase 1).
///
/// `None` for old peers and for paths that have not adopted v2 yet (VPN
/// adopts in Fase 3); everything here is observe-only in Fase 1. A missing
/// or partial rider must NEVER be interpreted as `RelayOnly` — absence of
/// metadata means "use the legacy behaviour".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UdpPunchV2 {
    /// Traversal round generation the peer candidates belong to.
    #[serde(default)]
    pub generation: u32,
    /// The peer's typed candidates (same addresses as the legacy `peer` list).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peer_typed: Vec<UdpTypedCandidate>,
    /// The peer's advertised traversal capabilities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peer_capabilities: Vec<String>,
    /// Server-computed traversal plan (Fase 3; always `None` before that).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<UdpAdaptivePlan>,
}

impl UdpPunchV2 {
    /// `Some(generation)` when the peer advertised authenticated connectivity
    /// checks ([`UDP_CAP_CHECK_V1`]) — the gate for the Fase 2 check round.
    pub fn check_generation(&self) -> Option<u32> {
        self.peer_capabilities
            .iter()
            .any(|c| c == UDP_CAP_CHECK_V1)
            .then_some(self.generation)
    }

    /// Build the punch rider from a peer's offer. `None` when the offer
    /// carried no v2 metadata (legacy peer) so the relayed wire frame stays
    /// byte-identical for legacy pairs. The adaptive `plan` starts `None`;
    /// the broker attaches one when both peers reported a structured profile
    /// (plan Fase 3). Shared by the secret broker and the VPN server so the
    /// two rider builders can never drift.
    pub fn from_offer(offer: &UdpCandidateOffer) -> Option<Self> {
        if offer.typed_candidates.is_empty()
            && offer.capabilities.is_empty()
            && offer.generation == 0
            && offer.profile.is_none()
        {
            return None;
        }
        Some(Self {
            generation: offer.generation,
            peer_typed: offer.typed_candidates.clone(),
            peer_capabilities: offer.capabilities.clone(),
            plan: None,
        })
    }
}

/// UDP candidate offer with optional metadata about the STUN server that
/// produced the primary reflexive candidate. The metadata is advisory: the wire
/// path still brokers plain candidate addresses, and peers fall back to the
/// normal STUN chain if the hinted server is unreachable from their network.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UdpCandidateOffer {
    /// Candidate addresses this peer can be punched at.
    pub candidates: Vec<SocketAddr>,
    /// STUN host:port selected by this peer, if a STUN probe succeeded.
    #[serde(default)]
    pub selected_stun: Option<String>,
    /// Hub mode: which peer this offer is for; 0 in 1:1.
    #[serde(default)]
    pub peer_id: u32,
    /// Candidate model v2 (plan Fase 1): typed candidates, same addresses as
    /// `candidates`. Old peers ignore it; new peers still read the legacy
    /// list as the source of truth until both sides advertise
    /// [`UDP_CAP_CANDIDATE_V2`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub typed_candidates: Vec<UdpTypedCandidate>,
    /// Traversal round generation these candidates belong to (0 = first).
    #[serde(default)]
    pub generation: u32,
    /// Traversal capabilities this peer understands (e.g. `cand-v2`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Compact self-reported NAT profile hint (observe-only; a server MUST
    /// NOT derive `RelayOnly` from a missing hint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_hint: Option<String>,
    /// Structured NAT self-profile (plan Fase 3). The server computes an
    /// adaptive plan only when BOTH peers attach one; `None` keeps the
    /// legacy/Fase-2 behaviour and the legacy wire frame byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<UdpNatProfile>,
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
            "https={}, force_https={}, basic_auth={}, notes={}, carriers={}, udp={}",
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
            if self.udp { "on" } else { "off" },
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
            "selected_stun={}, candidate_count={}, candidates={:?}, peer_id={}",
            self.selected_stun.as_deref().unwrap_or("<none>"),
            self.candidates.len(),
            self.candidates,
            self.peer_id,
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

/// An IPv4 CIDR (address + prefix length). Used for overlay + advertised subnets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ipv4Net {
    /// The IPv4 address.
    pub addr: std::net::Ipv4Addr,
    /// The prefix length (0-32).
    pub prefix: u8,
}

impl std::str::FromStr for Ipv4Net {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr_str, prefix_str) = s
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("missing '/' in CIDR: {s}"))?;
        let addr = addr_str
            .parse::<std::net::Ipv4Addr>()
            .map_err(|e| anyhow::anyhow!("invalid addr in {s}: {e}"))?;
        let prefix = prefix_str
            .parse::<u8>()
            .map_err(|e| anyhow::anyhow!("invalid prefix in {s}: {e}"))?;
        anyhow::ensure!(prefix <= 32, "prefix {prefix} > 32 in {s}");
        Ok(Ipv4Net { addr, prefix })
    }
}

impl std::fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl Ipv4Net {
    /// Network address (host bits zeroed).
    pub fn network(&self) -> std::net::Ipv4Addr {
        let mask = Self::prefix_to_mask(self.prefix);
        std::net::Ipv4Addr::from(u32::from(self.addr) & mask)
    }

    /// True if `addr` is within this network.
    pub fn contains(&self, addr: std::net::Ipv4Addr) -> bool {
        let mask = Self::prefix_to_mask(self.prefix);
        (u32::from(self.addr) & mask) == (u32::from(addr) & mask)
    }

    /// True if `other` network overlaps with this one.
    pub fn overlaps(&self, other: &Ipv4Net) -> bool {
        let mask = if self.prefix <= other.prefix {
            Self::prefix_to_mask(self.prefix)
        } else {
            Self::prefix_to_mask(other.prefix)
        };
        (u32::from(self.addr) & mask) == (u32::from(other.addr) & mask)
    }

    fn prefix_to_mask(prefix: u8) -> u32 {
        if prefix == 0 {
            0
        } else {
            !0u32 << (32 - prefix)
        }
    }
}

/// One `--advertise` item, optionally NAT-mapped (overlapping-subnet NAT, "E3").
///
/// Parsed from `<real>` (plain, no NAT) or `<real>@<exposed>` (1:1 stateless
/// netmap). The **real** subnet is the actual LAN behind this gateway; the
/// **exposed** subnet is what peers route and address. They are equal for a
/// plain entry. Only the exposed CIDR is ever serialized on the wire (N3/I-NAT2):
/// real subnets are gateway-local. Equal prefix length is enforced (N4): the
/// netmap preserves host bits 1:1, so a `/24@/25` is a hard parse error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdvertiseEntry {
    /// Real local subnet (the actual LAN behind this gateway).
    pub real: Ipv4Net,
    /// Subnet exposed to peers over the wire. Equals `real` when no `@` mapping is given.
    pub exposed: Ipv4Net,
}

impl AdvertiseEntry {
    /// True when this entry maps a real subnet to a distinct exposed (virtual) one.
    pub fn is_nat(&self) -> bool {
        self.real != self.exposed
    }
}

impl std::str::FromStr for AdvertiseEntry {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.split_once('@') {
            None => {
                let n: Ipv4Net = s.parse()?;
                Ok(Self {
                    real: n,
                    exposed: n,
                })
            }
            Some((r, v)) => {
                anyhow::ensure!(!v.contains('@'), "advertise '{s}': at most one '@'");
                let real: Ipv4Net = r.parse().with_context(|| format!("advertise real '{r}'"))?;
                let exposed: Ipv4Net = v
                    .parse()
                    .with_context(|| format!("advertise virtual '{v}'"))?;
                anyhow::ensure!(
                    real.prefix == exposed.prefix,
                    "advertise '{s}': real /{} and virtual /{} must have equal prefix length (1:1 netmap)",
                    real.prefix,
                    exposed.prefix
                );
                Ok(Self { real, exposed })
            }
        }
    }
}

impl std::fmt::Display for AdvertiseEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_nat() {
            write!(f, "{}@{}", self.real, self.exposed)
        } else {
            write!(f, "{}", self.real)
        }
    }
}

/// How a side wants its overlay address assigned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VpnAddrRequest {
    /// Server allocates a /30 from its pool.
    Pool,
    /// Client specifies its own address, prefix, and peer address.
    Static {
        /// The overlay address requested.
        addr: std::net::Ipv4Addr,
        /// The prefix length.
        prefix: u8,
        /// The peer's overlay address.
        peer: std::net::Ipv4Addr,
    },
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
        /// Whether the provider requested the UDP/QUIC direct path (`--udp`).
        /// Display-only.
        #[serde(default)]
        udp: bool,
        /// Whether the provider runs with `--auto-reconnect`. Display-only.
        #[serde(default)]
        auto_reconnect: bool,
        /// Whether the provider requested HTTP access logging (`--webserver-log`).
        /// Display-only.
        #[serde(default)]
        webserver_log: bool,
        /// The provider's `--nat-udp-preferred-port` (0 = unset/ephemeral).
        /// Display-only.
        #[serde(default)]
        nat_udp_preferred_port: u16,
        /// The provider's `--nat-udp-release-timeout` seconds. Display-only.
        #[serde(default)]
        nat_udp_release_timeout: u64,
        /// The provider's selected `--stun-server`, if any. Display-only.
        #[serde(default)]
        stun_server: Option<String>,
        /// Whether the provider enabled `--upnp`. Display-only.
        #[serde(default)]
        upnp: bool,
        /// Whether the provider enabled `--try-port-prediction`. Display-only.
        #[serde(default)]
        try_port_prediction: bool,
        /// The provider's `--max-conns` cap. Display-only.
        #[serde(default)]
        max_conns: usize,
        /// The provider's local service host (`-l/--local-host`). Display-only.
        #[serde(default)]
        local_host: Option<String>,
        /// The provider's local service port. `0` = unknown. Display-only.
        #[serde(default)]
        local_port: u16,
    },

    /// Connect as a consumer of a named secret tunnel; data substreams opened on
    /// this connection are routed to the matching provider. `notes` is shown on
    /// the admin page.
    ConnectSecret {
        /// The secret-tunnel identifier to connect to.
        id: String,
        /// Optional operator note for the admin status page.
        notes: Option<String>,
        /// Number of parallel relay carrier connections the consumer requested
        /// (`--carriers`). Display-only. `#[serde(default)]` keeps the wire format
        /// backward-compatible (an old client omits it ⇒ reads as `0`).
        #[serde(default)]
        carriers: u16,
        /// Whether the consumer runs with `--auto-reconnect` (client-side reconnect
        /// loop). Display-only; the server takes no action on it.
        #[serde(default)]
        auto_reconnect: bool,
        /// Whether the consumer requested the UDP/QUIC direct path (`--udp`).
        /// Display-only.
        #[serde(default)]
        udp: bool,
        /// The consumer's local proxy listen port (`--local-proxy-port`). `0` =
        /// unset/unknown. Display-only.
        #[serde(default)]
        local_proxy_port: u16,
        /// The consumer's `--nat-udp-preferred-port` (0 = unset/ephemeral).
        /// Display-only.
        #[serde(default)]
        nat_udp_preferred_port: u16,
        /// The consumer's `--nat-udp-release-timeout` seconds. Display-only.
        #[serde(default)]
        nat_udp_release_timeout: u64,
        /// The consumer's selected `--stun-server`, if any. Display-only.
        #[serde(default)]
        stun_server: Option<String>,
        /// Whether the consumer enabled `--upnp`. Display-only.
        #[serde(default)]
        upnp: bool,
        /// Whether the consumer enabled `--try-port-prediction`. Display-only.
        #[serde(default)]
        try_port_prediction: bool,
        /// Whether this connection is an extra relay *carrier* of an existing
        /// consumer rather than a primary consumer (`--carriers N` opens N-1 of
        /// these). A carrier creates NO admin entry and is NOT reaped by the
        /// consumer control-liveness check — its liveness is owned by the
        /// consumer's main control connection (I-2/I-3). `#[serde(default)]` keeps
        /// the wire backward-compatible (an old client omits it ⇒ `false` ⇒ the
        /// legacy behaviour of registering a per-connection entry).
        #[serde(default)]
        carrier: bool,
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

    /// Register as the provider for a vhost subdomain reverse-proxy tunnel.
    HelloVhost {
        /// Subdomain label to register (e.g. `myapp` in `myapp.bore.example.com`).
        subdomain: String,
        /// Client identifier used for reservation matching in `vhost.yml`.
        client_id: String,
        /// Optional operator note shown on the admin status page.
        notes: Option<String>,
        /// Whether the provider enforces HTTP Basic auth itself (display only).
        basic_auth: bool,
        /// Number of parallel TCP carrier connections to request.
        #[serde(default)]
        carriers: u16,
        /// Whether the provider wants the QUIC direct data path (`bore vhost --udp`).
        /// `#[serde(default)]` keeps the wire format backward-compatible.
        #[serde(default)]
        udp: bool,
        /// Whether the provider wants access logging with real caller IP forwarding.
        /// `#[serde(default)]` keeps the wire format backward-compatible.
        #[serde(default)]
        webserver_log: bool,
        /// Whether the provider runs with `--auto-reconnect` (client-side reconnect
        /// loop). Sent purely so the admin status page can show it; the server takes
        /// no action on it. `#[serde(default)]` keeps the wire format
        /// backward-compatible (an old client omits it ⇒ reads as `false`).
        #[serde(default)]
        auto_reconnect: bool,
        /// The provider's local target host. Display-only.
        #[serde(default)]
        local_host: Option<String>,
        /// The provider's local target port. `0` = unknown. Display-only.
        #[serde(default)]
        local_port: u16,
        /// Per-tunnel HTTPS policy. `None` = inherit the server default (byte-identical
        /// to the pre-policy behavior). `#[serde(default)]` keeps the wire backward-compatible.
        #[serde(default)]
        https_policy: Option<HttpsPolicy>,
    },

    /// Ask the server to issue a fresh vhost-UDP nonce so the provider can
    /// re-dial the direct QUIC path after it dropped.
    VhostUdpRenew {
        /// Subdomain whose direct path should be renewed.
        subdomain: String,
    },

    /// Ask the server to issue a fresh public-UDP nonce so the client can
    /// re-dial the direct QUIC path after it dropped.
    PublicUdpRenew {
        /// Public tunnel port whose direct path should be renewed.
        port: u16,
    },

    /// Register as the listener for a VPN link id.
    HelloVpn {
        /// VPN link identifier to register under.
        id: String,
        /// CIDRs this side exposes (empty = host-only).
        advertised: Vec<Ipv4Net>,
        /// How this side wants its overlay address assigned.
        addr: VpnAddrRequest,
        /// Optional operator note for the admin status page.
        notes: Option<String>,
        /// Number of relay carrier connections (always 1 in v1; field reserved for v2).
        #[serde(default)]
        carriers: u16,
        /// Max concurrent connectors (hub mode). 0/absent → treated as 1 = legacy 1:1.
        #[serde(default)]
        max_clients: u16,
        /// Display-only: relay-only mode enabled (no direct QUIC).
        #[serde(default)]
        relay_only: bool,
        /// Display-only: MTU pinning enabled.
        #[serde(default)]
        pin_mtu: bool,
        /// Display-only: TUN interface MTU.
        #[serde(default)]
        mtu: Option<u16>,
        /// Display-only: forward-accept iptables rule inserted.
        #[serde(default)]
        forward_accept: bool,
        /// Display-only: NAT masquerade enabled.
        #[serde(default)]
        nat_masquerade: bool,
        /// Display-only: route accept/refuse policy summary (e.g., "accept-all", "refuse-all", "accept:2 refuse:1").
        #[serde(default)]
        route_policy: Option<String>,
        /// Display-only: client's `--nat-udp-preferred-port` (0 = unset/ephemeral).
        #[serde(default)]
        nat_udp_preferred_port: u16,
    },

    /// Connect as the connector for a VPN link id.
    ConnectVpn {
        /// VPN link identifier to connect to.
        id: String,
        /// CIDRs this side exposes (empty = host-only).
        advertised: Vec<Ipv4Net>,
        /// How this side wants its overlay address assigned.
        addr: VpnAddrRequest,
        /// Optional operator note.
        notes: Option<String>,
        /// Requested number of relay carrier substream pairs. Old peers omit
        /// the field → 1 → single-pair path, byte-identical to before (I-9).
        #[serde(default = "default_vpn_carriers")]
        carriers: u16,
        /// Display-only: relay-only mode enabled (no direct QUIC).
        #[serde(default)]
        relay_only: bool,
        /// Display-only: MTU pinning enabled.
        #[serde(default)]
        pin_mtu: bool,
        /// Display-only: TUN interface MTU.
        #[serde(default)]
        mtu: Option<u16>,
        /// Display-only: forward-accept iptables rule inserted.
        #[serde(default)]
        forward_accept: bool,
        /// Display-only: NAT masquerade enabled.
        #[serde(default)]
        nat_masquerade: bool,
        /// Display-only: route accept/refuse policy summary (e.g., "accept-all", "refuse-all", "accept:2 refuse:1").
        #[serde(default)]
        route_policy: Option<String>,
        /// Display-only: client's `--nat-udp-preferred-port` (0 = unset/ephemeral).
        #[serde(default)]
        nat_udp_preferred_port: u16,
    },

    /// Report the active VPN data-plane path (`"relay"` or `"direct"`) for the
    /// admin page. Sent only when the server advertised support via
    /// [`ServerMessage::VpnReady`]'s `admin_v2` flag (an old server would fail
    /// to deserialize an unknown enum variant).
    VpnPathReport {
        /// `"relay"` or `"direct"`.
        path: String,
    },

    /// Periodic liveness ping from a secret provider/consumer control loop. The
    /// control channel is a yamux substream, so a half-open/abandoned peer is
    /// invisible to the server's `send`/`recv` (send buffers, recv blocks). This
    /// frame gives the server a positive liveness signal to time out on; the
    /// server treats it as a no-op. Appended last so the wire stays
    /// backward-compatible (an old server fails to deserialize this variant, so
    /// clients must be deployed before / together with the server).
    Heartbeat,
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
        /// Hub mode: which peer this punch belongs to; 0 in 1:1.
        #[serde(default)]
        peer_id: u32,
        /// Candidate model v2 rider (typed candidates, generation,
        /// capabilities, future plan). `None` from old servers / non-v2 paths;
        /// observe-only in Fase 1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        v2: Option<UdpPunchV2>,
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

    /// Acknowledges a vhost registration ([`ClientMessage::HelloVhost`]) and
    /// returns the public URL(s) the client should print to the user.
    VhostReady {
        /// Public HTTP URL (e.g. `http://myapp.bore.example.com`), when the
        /// server's frontend serves plain HTTP for this mode.
        http_url: Option<String>,
        /// Public HTTPS URL (e.g. `https://myapp.bore.example.com`), when the
        /// server's frontend serves HTTPS.
        https_url: Option<String>,
    },

    /// Offer the vhost QUIC direct path to a provider that requested `--udp`.
    VhostUdp {
        /// Server UDP port to dial for the QUIC direct path.
        port: u16,
        /// Session nonce; server and provider derive the same token from it + secret.
        nonce: [u8; UDP_NONCE_LEN],
        /// Direct-UDP transport tuning requested by the server.
        #[serde(default)]
        tuning: UdpDirectTuning,
    },

    /// Offer the QUIC direct path to a public-tunnel client that requested `--udp`.
    PublicUdp {
        /// Server UDP port to dial for the QUIC direct path.
        port: u16,
        /// Session nonce; server and client derive the same token from it + secret.
        nonce: [u8; UDP_NONCE_LEN],
        /// Direct-UDP transport tuning requested by the server.
        #[serde(default)]
        tuning: UdpDirectTuning,
    },

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
        /// Whether this server supports retry re-candidating: a peer may send a
        /// fresh [`ClientMessage::TestUdpJoin`] after a failed direct attempt
        /// and the server re-brokers a new round (fresh nonce + candidates +
        /// plan). Old servers omit it (`false`) → the client skips retries
        /// instead of re-punching stale candidates from a dead socket.
        #[serde(default)]
        recandidate: bool,
        /// Traversal round number: 0 for the initial pairing, incremented for
        /// every re-brokered retry round. Retries MUST carry a new generation
        /// so stale-round candidates are never mixed into a fresh attempt.
        #[serde(default)]
        generation: u32,
    },

    /// VPN link paired; contains overlay addressing and the direct-path nonce.
    VpnReady {
        /// This side's overlay address.
        assigned: std::net::Ipv4Addr,
        /// Overlay link prefix length (always 30 for /30).
        prefix: u8,
        /// The other end's overlay address.
        peer_overlay: std::net::Ipv4Addr,
        /// Install routes toward these CIDRs via the tun interface.
        peer_advertised: Vec<Ipv4Net>,
        /// Seeds both the AEAD key derivation and the direct-path token derivation.
        session_nonce: [u8; UDP_NONCE_LEN],
        /// Direct-UDP transport tuning.
        #[serde(default)]
        tuning: UdpDirectTuning,
        /// Whether this server accepts [`ClientMessage::VpnPathReport`]
        /// (admin page v2). `#[serde(default)]` keeps old peers compatible:
        /// old server → field absent → false → client never sends the report.
        #[serde(default)]
        admin_v2: bool,
        /// Effective number of relay carrier substream pairs, negotiated as
        /// `min(listener, connector, server max)`. Old server → absent → 1.
        #[serde(default = "default_vpn_carriers")]
        carriers: u16,
    },

    /// VPN pairing failed (duplicate id, pool exhausted, overlap, etc.).
    VpnError(String),

    /// A new connector joined the hub (hub mode only, server → listener).
    /// Sent after the connector is fully paired and ready to accept data substreams.
    VpnPeerJoin {
        /// Hub-assigned peer identifier (monotonic within a hub session).
        peer_id: u32,
        /// This peer's overlay address.
        peer_overlay: std::net::Ipv4Addr,
        /// CIDRs this peer exposes (empty = host-only; connectors don't advertise in v1).
        peer_advertised: Vec<Ipv4Net>,
        /// Seeds the AEAD key derivation for this peer's relay streams.
        session_nonce: [u8; UDP_NONCE_LEN],
        /// Number of relay carrier substream pairs for this peer (negotiated min).
        carriers: u16,
    },

    /// A peer disconnected from the hub (hub mode only, server → listener).
    VpnPeerLeave {
        /// The peer's identifier (from the matching VpnPeerJoin).
        peer_id: u32,
    },

    /// Non-fatal advisory from the server. The client prints it and CONTINUES.
    /// Sent only to policy-aware clients (they sent https_policy = Some). Old clients never receive it.
    Warning(String),
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
                ..
            } => {
                format!(
                    "HelloSecret {{ id={}, notes={}, basic_auth={}, carriers={} }}",
                    id,
                    if notes.is_some() { "present" } else { "none" },
                    if *basic_auth { "on" } else { "off" },
                    carriers,
                )
            }
            ClientMessage::ConnectSecret { id, notes, .. } => {
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
            ClientMessage::HelloVhost {
                subdomain,
                client_id,
                notes,
                basic_auth,
                carriers,
                udp,
                webserver_log,
                auto_reconnect,
                ..
            } => {
                format!(
                    "HelloVhost {{ subdomain={}, client_id={}, notes={}, basic_auth={}, carriers={}, udp={}, webserver_log={}, auto_reconnect={} }}",
                    subdomain,
                    client_id,
                    if notes.is_some() { "present" } else { "none" },
                    if *basic_auth { "on" } else { "off" },
                    carriers,
                    if *udp { "on" } else { "off" },
                    if *webserver_log { "on" } else { "off" },
                    if *auto_reconnect { "on" } else { "off" },
                )
            }
            ClientMessage::VhostUdpRenew { subdomain } => {
                format!("VhostUdpRenew {{ subdomain={} }}", subdomain)
            }
            ClientMessage::PublicUdpRenew { port } => {
                format!("PublicUdpRenew {{ port={} }}", port)
            }
            ClientMessage::HelloVpn {
                id,
                advertised,
                addr,
                notes,
                carriers,
                max_clients,
                ..
            } => {
                format!(
                    "HelloVpn {{ id={}, advertised={:?}, addr={:?}, notes={}, carriers={}, max_clients={} }}",
                    id,
                    advertised,
                    addr,
                    if notes.is_some() { "present" } else { "none" },
                    carriers,
                    max_clients,
                )
            }
            ClientMessage::ConnectVpn {
                id,
                advertised,
                addr,
                notes,
                carriers,
                ..
            } => {
                format!(
                    "ConnectVpn {{ id={}, advertised={:?}, addr={:?}, notes={}, carriers={} }}",
                    id,
                    advertised,
                    addr,
                    if notes.is_some() { "present" } else { "none" },
                    carriers,
                )
            }
            ClientMessage::VpnPathReport { path } => {
                format!("VpnPathReport {{ path={} }}", path)
            }
            ClientMessage::Heartbeat => "Heartbeat".to_string(),
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
                peer_id,
                v2,
            } => {
                format!(
                    "UdpPunch {{ nonce={}, peer={:?}, peer_selected_stun={}, tuning={{ {} }}, peer_id={}, v2={} }}",
                    hex::encode(nonce),
                    peer,
                    peer_selected_stun.as_deref().unwrap_or("<none>"),
                    tuning.control_frame_summary(),
                    peer_id,
                    match v2 {
                        Some(v2) => format!(
                            "{{ generation={}, typed={}, capabilities={:?}, plan={} }}",
                            v2.generation,
                            v2.peer_typed.len(),
                            v2.peer_capabilities,
                            v2.plan
                                .as_ref()
                                .map(|p| p.summary())
                                .unwrap_or_else(|| "<none>".to_string()),
                        ),
                        None => "<none>".to_string(),
                    },
                )
            }
            ServerMessage::UdpStunHint { stun_server } => {
                format!(
                    "UdpStunHint {{ stun_server={} }}",
                    stun_server.as_deref().unwrap_or("<none>")
                )
            }
            ServerMessage::UdpUnavailable => "UdpUnavailable".to_string(),
            ServerMessage::VhostReady {
                http_url,
                https_url,
            } => format!(
                "VhostReady {{ http_url={}, https_url={} }}",
                http_url.as_deref().unwrap_or("<none>"),
                https_url.as_deref().unwrap_or("<none>"),
            ),
            ServerMessage::VhostUdp {
                port,
                nonce,
                tuning,
            } => {
                format!(
                    "VhostUdp {{ port={}, nonce={}, tuning={{ {} }} }}",
                    port,
                    hex::encode(nonce),
                    tuning.control_frame_summary(),
                )
            }
            ServerMessage::PublicUdp {
                port,
                nonce,
                tuning,
            } => {
                format!(
                    "PublicUdp {{ port={}, nonce={}, tuning={{ {} }} }}",
                    port,
                    hex::encode(nonce),
                    tuning.control_frame_summary(),
                )
            }
            ServerMessage::TestUdpWaiting => "TestUdpWaiting".to_string(),
            ServerMessage::TestUdpStart {
                role,
                nonce,
                peer_candidates,
                peer_summary,
                options,
                tuning,
                adaptive_plan,
                recandidate,
                generation,
            } => {
                format!(
                    "TestUdpStart {{ role={:?}, nonce={}, peer_candidates={:?}, peer_summary={{ {} }}, options={{ {} }}, tuning={{ {} }}, adaptive_plan={}, recandidate={}, generation={} }}",
                    role,
                    hex::encode(nonce),
                    peer_candidates,
                    peer_summary.control_frame_summary(),
                    options.control_frame_summary(),
                    tuning.control_frame_summary(),
                    adaptive_plan.as_ref().map(|plan| plan.summary()).unwrap_or_else(|| "<none>".to_string()),
                    recandidate,
                    generation,
                )
            }
            ServerMessage::VpnReady {
                assigned,
                prefix,
                peer_overlay,
                peer_advertised,
                session_nonce,
                tuning,
                admin_v2,
                carriers,
            } => {
                format!(
                    "VpnReady {{ assigned={}, prefix={}, peer_overlay={}, peer_advertised={:?}, session_nonce={}, tuning={{ {} }}, admin_v2={}, carriers={} }}",
                    assigned,
                    prefix,
                    peer_overlay,
                    peer_advertised,
                    hex::encode(session_nonce),
                    tuning.control_frame_summary(),
                    admin_v2,
                    carriers,
                )
            }
            ServerMessage::VpnError(msg) => {
                format!("VpnError {{ message={} }}", msg)
            }
            ServerMessage::VpnPeerJoin {
                peer_id,
                peer_overlay,
                peer_advertised,
                session_nonce,
                carriers,
            } => {
                format!(
                    "VpnPeerJoin {{ peer_id={}, peer_overlay={}, peer_advertised={:?}, session_nonce={}, carriers={} }}",
                    peer_id,
                    peer_overlay,
                    peer_advertised,
                    hex::encode(session_nonce),
                    carriers,
                )
            }
            ServerMessage::VpnPeerLeave { peer_id } => {
                format!("VpnPeerLeave {{ peer_id={} }}", peer_id)
            }
            ServerMessage::Warning(msg) => {
                format!("Warning {{ message={} }}", msg)
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
    fn t_wire_secret_default_compat() {
        // D3/I-1: a legacy ConnectSecret carrying only `id`+`notes` must still
        // deserialize on a new server, with every added display field defaulting.
        let msg: ClientMessage =
            serde_json::from_str(r#"{"ConnectSecret":{"id":"db","notes":null}}"#)
                .expect("legacy ConnectSecret must still deserialize");
        match msg {
            ClientMessage::ConnectSecret {
                id,
                carriers,
                auto_reconnect,
                udp,
                local_proxy_port,
                nat_udp_preferred_port,
                upnp,
                try_port_prediction,
                carrier,
                ..
            } => {
                assert_eq!(id, "db");
                assert_eq!(carriers, 0);
                assert!(!auto_reconnect);
                assert!(!udp);
                assert_eq!(local_proxy_port, 0);
                assert_eq!(nat_udp_preferred_port, 0);
                assert!(!upnp);
                assert!(!try_port_prediction);
                assert!(!carrier, "legacy ConnectSecret must default carrier=false");
            }
            other => panic!("unexpected message: {other:?}"),
        }

        // A legacy HelloSecret (id/notes/basic_auth/carriers only) likewise.
        let msg: ClientMessage = serde_json::from_str(
            r#"{"HelloSecret":{"id":"db","notes":null,"basic_auth":false,"carriers":2}}"#,
        )
        .expect("legacy HelloSecret must still deserialize");
        match msg {
            ClientMessage::HelloSecret {
                id,
                carriers,
                udp,
                auto_reconnect,
                webserver_log,
                max_conns,
                local_host,
                ..
            } => {
                assert_eq!(id, "db");
                assert_eq!(carriers, 2);
                assert!(!udp);
                assert!(!auto_reconnect);
                assert!(!webserver_log);
                assert_eq!(max_conns, 0);
                assert_eq!(local_host, None);
            }
            other => panic!("unexpected message: {other:?}"),
        }

        // Round-trip a fully-populated ConnectSecret survives.
        let full = ClientMessage::ConnectSecret {
            id: "db".into(),
            notes: Some("n".into()),
            carriers: 4,
            auto_reconnect: true,
            udp: true,
            local_proxy_port: 5432,
            nat_udp_preferred_port: 443,
            nat_udp_release_timeout: 30,
            stun_server: Some("stun.example:3478".into()),
            upnp: true,
            try_port_prediction: true,
            carrier: true,
        };
        let back: ClientMessage =
            serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
        match back {
            ClientMessage::ConnectSecret {
                carriers,
                local_proxy_port,
                nat_udp_preferred_port,
                carrier,
                ..
            } => {
                assert_eq!(carriers, 4);
                assert_eq!(local_proxy_port, 5432);
                assert_eq!(nat_udp_preferred_port, 443);
                assert!(carrier, "carrier flag must round-trip on the wire");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tunnelopts_compat_missing_new_fields_default() {
        // I-COMPAT (D4): an old client omits `carriers`/`udp`/`auto_reconnect`;
        // a new server must read them as their defaults rather than failing.
        let opts: TunnelOptions = serde_json::from_str(
            r#"{"https":true,"force_https":false,"basic_auth":null,"notes":null}"#,
        )
        .expect("legacy TunnelOptions must still deserialize");
        assert!(opts.https);
        assert_eq!(opts.carriers, 0);
        assert!(!opts.udp);
        assert!(
            !opts.auto_reconnect,
            "missing auto_reconnect defaults to false"
        );

        // Round-trip with the new field set survives.
        let full = TunnelOptions {
            https: true,
            force_https: true,
            basic_auth: None,
            notes: None,
            carriers: 4,
            udp: true,
            auto_reconnect: true,
            webserver_log: false,
            max_conns: 0,
            local_host: None,
            local_port: 0,
            https_policy: None,
        };
        let json = serde_json::to_string(&full).unwrap();
        let back: TunnelOptions = serde_json::from_str(&json).unwrap();
        assert!(back.auto_reconnect);
        assert_eq!(back.carriers, 4);
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
                peer_id: _,
                v2,
            } => {
                assert_eq!(nonce, [0; UDP_NONCE_LEN]);
                assert_eq!(peer, vec!["127.0.0.1:3478".parse().unwrap()]);
                assert_eq!(peer_selected_stun, None);
                assert_eq!(tuning, UdpDirectTuning::default());
                assert!(v2.is_none(), "old frame must decode with v2 = None");
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    /// Wire matrix old/new (Fase 1): an old-format offer (plain address list
    /// only) decodes with empty v2 fields; a v2 offer round-trips; and a v2
    /// offer serialized then read by an "old" decoder still exposes the legacy
    /// list (serde_json ignores unknown struct fields).
    #[test]
    fn udp_offer_v2_wire_matrix() {
        // old client → new server
        let old: UdpCandidateOffer = serde_json::from_str(
            r#"{"candidates":["198.51.100.1:4000"],"selected_stun":"s:3478","peer_id":0}"#,
        )
        .unwrap();
        assert_eq!(old.candidates.len(), 1);
        assert!(old.typed_candidates.is_empty());
        assert_eq!(old.generation, 0);
        assert!(old.capabilities.is_empty());
        assert!(old.profile_hint.is_none());
        assert!(old.profile.is_none());

        // new → new roundtrip
        let full = UdpCandidateOffer {
            candidates: vec!["198.51.100.1:4000".parse().unwrap()],
            selected_stun: Some("s:3478".into()),
            peer_id: 3,
            typed_candidates: vec![UdpTypedCandidate {
                addr: "198.51.100.1:4000".parse().unwrap(),
                kind: UdpCandidateKind::Reflexive,
                priority: 800,
            }],
            generation: 2,
            capabilities: vec![UDP_CAP_CANDIDATE_V2.to_string()],
            profile_hint: Some("cone".into()),
            profile: Some(UdpNatProfile {
                mapping: UdpNatMapping::Eim,
                filtering: UdpNatFiltering::Unknown,
                port_preserved: Some(true),
                observations: 2,
            }),
        };
        let json = serde_json::to_string(&full).unwrap();
        let back: UdpCandidateOffer = serde_json::from_str(&json).unwrap();
        assert_eq!(back.typed_candidates, full.typed_candidates);
        assert_eq!(back.generation, 2);
        assert_eq!(back.capabilities, full.capabilities);
        assert_eq!(back.profile_hint.as_deref(), Some("cone"));
        assert_eq!(back.profile, full.profile);
        // The legacy list is intact inside the v2 frame (old server reads it).
        assert_eq!(back.candidates, full.candidates);

        // A profile-less offer must not even carry the `profile` key, so a
        // legacy pair's frame stays byte-identical (Fase 3 zero-regression).
        let no_profile = UdpCandidateOffer {
            profile: None,
            ..full.clone()
        };
        let json = serde_json::to_string(&no_profile).unwrap();
        assert!(
            !json.contains("\"profile\""),
            "profile key must be absent when None: {json}"
        );
    }

    /// The WORST-CASE control frame — an `UdpPunch` whose v2 rider carries the
    /// full 16-candidate list (legacy + typed), capabilities, and an adaptive
    /// plan — must fit `MAX_FRAME_LENGTH` with headroom. Fase-3 regression
    /// pin: the old 1024-byte cap silently broke UDP negotiation ("frame
    /// error, invalid byte length") once port prediction pushed the rider
    /// past it.
    #[test]
    fn udp_punch_worst_case_fits_frame_limit() {
        let addr =
            |i: u16| -> SocketAddr { format!("203.111.222.133:{}", 40_000 + i).parse().unwrap() };
        let peers: Vec<SocketAddr> = (0..16).map(addr).collect();
        let msg = ServerMessage::UdpPunch {
            nonce: [255; UDP_NONCE_LEN],
            peer: peers.clone(),
            peer_selected_stun: Some("stun.some-long-hostname.example.com:3478".into()),
            tuning: UdpDirectTuning::default(),
            peer_id: u32::MAX,
            v2: Some(UdpPunchV2 {
                generation: u32::MAX,
                peer_typed: peers
                    .iter()
                    .map(|&addr| UdpTypedCandidate {
                        addr,
                        kind: UdpCandidateKind::RouterMapped,
                        priority: u32::MAX,
                    })
                    .collect(),
                peer_capabilities: vec![
                    UDP_CAP_CANDIDATE_V2.to_string(),
                    UDP_CAP_CHECK_V1.to_string(),
                ],
                plan: Some(UdpAdaptivePlan {
                    mode: UdpAdaptiveMode::DirectWithRetry,
                    candidate_order: vec![
                        UdpAdaptiveCandidateKind::Local,
                        UdpAdaptiveCandidateKind::Reflexive,
                        UdpAdaptiveCandidateKind::RouterMapped,
                        UdpAdaptiveCandidateKind::Predicted,
                        UdpAdaptiveCandidateKind::RelayFallback,
                    ],
                    retry_budget: u8::MAX,
                    read_timeout_ms: u64::MAX,
                    send_delay_ms: u64::MAX,
                }),
            }),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.len() < MAX_FRAME_LENGTH - 1024,
            "worst-case UdpPunch is {} bytes; needs >1 KiB headroom under \
             MAX_FRAME_LENGTH ({MAX_FRAME_LENGTH})",
            json.len()
        );
    }

    /// Wire matrix old/new (Fase 1): `UdpPunch` with a v2 rider round-trips,
    /// and a v2-less legacy frame stays legacy. A legacy pair's frame must not
    /// even carry the `v2` key (skip_serializing_if).
    #[test]
    fn udp_punch_v2_wire_matrix() {
        let with_v2 = ServerMessage::UdpPunch {
            nonce: [1; UDP_NONCE_LEN],
            peer: vec!["198.51.100.1:4000".parse().unwrap()],
            peer_selected_stun: None,
            tuning: UdpDirectTuning::default(),
            peer_id: 0,
            v2: Some(UdpPunchV2 {
                generation: 5,
                peer_typed: vec![UdpTypedCandidate {
                    addr: "198.51.100.1:4000".parse().unwrap(),
                    kind: UdpCandidateKind::Reflexive,
                    priority: 800,
                }],
                peer_capabilities: vec![UDP_CAP_CANDIDATE_V2.to_string()],
                plan: None,
            }),
        };
        let json = serde_json::to_string(&with_v2).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        match back {
            ServerMessage::UdpPunch { v2: Some(v2), .. } => {
                assert_eq!(v2.generation, 5);
                assert_eq!(v2.peer_typed.len(), 1);
                assert!(v2.plan.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Rider WITH an adaptive plan (Fase 3) round-trips, and an old
        // (Fase 1/2) client deserializes it fine — plan is serde(default).
        let with_plan = ServerMessage::UdpPunch {
            nonce: [1; UDP_NONCE_LEN],
            peer: vec!["198.51.100.1:4000".parse().unwrap()],
            peer_selected_stun: None,
            tuning: UdpDirectTuning::default(),
            peer_id: 0,
            v2: Some(UdpPunchV2 {
                generation: 6,
                peer_typed: vec![],
                peer_capabilities: vec![UDP_CAP_CHECK_V1.to_string()],
                plan: Some(UdpAdaptivePlan {
                    mode: UdpAdaptiveMode::DirectWithRetry,
                    candidate_order: vec![
                        UdpAdaptiveCandidateKind::Local,
                        UdpAdaptiveCandidateKind::Reflexive,
                        UdpAdaptiveCandidateKind::RelayFallback,
                    ],
                    retry_budget: 2,
                    read_timeout_ms: 500,
                    send_delay_ms: 25,
                }),
            }),
        };
        let json = serde_json::to_string(&with_plan).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        match back {
            ServerMessage::UdpPunch { v2: Some(v2), .. } => {
                assert_eq!(v2.check_generation(), Some(6));
                let plan = v2.plan.expect("plan must round-trip");
                assert_eq!(plan.mode, UdpAdaptiveMode::DirectWithRetry);
                assert_eq!(plan.retry_budget, 2);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let legacy = ServerMessage::UdpPunch {
            nonce: [1; UDP_NONCE_LEN],
            peer: vec![],
            peer_selected_stun: None,
            tuning: UdpDirectTuning::default(),
            peer_id: 0,
            v2: None,
        };
        let json = serde_json::to_string(&legacy).unwrap();
        assert!(
            !json.contains("\"v2\""),
            "legacy pair frames must stay byte-identical (no v2 key): {json}"
        );
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
                recandidate,
                generation,
            } => {
                assert_eq!(role, UdpTestRole::Listener);
                assert_eq!(nonce, [0; UDP_NONCE_LEN]);
                assert_eq!(peer_candidates, vec!["127.0.0.1:3478".parse().unwrap()]);
                assert_eq!(peer_summary.nat_class, "Open");
                assert!(peer_summary.candidate_kinds.is_empty());
                assert_eq!(peer_summary.selected_stun, None);
                // Old-server frame: the retry-round capability fields default off.
                assert!(!recandidate);
                assert_eq!(generation, 0);
                assert!(!options.bandwidth);
                assert_eq!(options.transfer_quota, 1024);
                assert!(!options.udp_only);
                assert_eq!(tuning, UdpDirectTuning::default());
                assert!(adaptive_plan.is_none());
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn hello_vhost_defaults_udp_false_for_old_wire_format() {
        let msg: ClientMessage = serde_json::from_str(
            r#"{"HelloVhost":{"subdomain":"myapp","client_id":"client-1","notes":null,"basic_auth":false,"carriers":2}}"#,
        )
        .unwrap();

        match msg {
            ClientMessage::HelloVhost {
                subdomain,
                client_id,
                notes,
                basic_auth,
                carriers,
                udp,
                webserver_log,
                auto_reconnect,
                ..
            } => {
                assert_eq!(subdomain, "myapp");
                assert_eq!(client_id, "client-1");
                assert_eq!(notes, None);
                assert!(!basic_auth);
                assert_eq!(carriers, 2);
                assert!(!udp);
                assert!(!webserver_log);
                // D6: old wire format without auto_reconnect ⇒ serde default false.
                assert!(!auto_reconnect);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn vhost_udp_round_trips() {
        let msg = ServerMessage::VhostUdp {
            port: 443,
            nonce: [7; UDP_NONCE_LEN],
            tuning: UdpDirectTuning::default(),
        };

        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: ServerMessage = serde_json::from_str(&encoded).unwrap();
        match decoded {
            ServerMessage::VhostUdp {
                port,
                nonce,
                tuning,
            } => {
                assert_eq!(port, 443);
                assert_eq!(nonce, [7; UDP_NONCE_LEN]);
                assert_eq!(tuning, UdpDirectTuning::default());
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
        recandidate: true,
        generation: 0,
    };

    let summary = msg.control_frame_summary();
    assert!(summary.contains("TestUdpStart"));
    assert!(summary.contains("adaptive_plan=direct-first"));
    assert!(summary.contains("nonce=01010101010101010101010101010101"));
}

#[test]
fn hello_vhost_round_trips_and_fits_frame() {
    let msg = ClientMessage::HelloVhost {
        subdomain: "myapp".to_string(),
        client_id: "client-a".to_string(),
        notes: Some("test note".to_string()),
        basic_auth: false,
        carriers: 1,
        udp: false,
        webserver_log: false,
        auto_reconnect: true,
        local_host: None,
        local_port: 0,
        https_policy: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(
        json.len() < MAX_FRAME_LENGTH,
        "HelloVhost must fit in MAX_FRAME_LENGTH"
    );
    let round: ClientMessage = serde_json::from_str(&json).unwrap();
    match round {
        ClientMessage::HelloVhost {
            subdomain,
            client_id,
            notes,
            basic_auth,
            carriers,
            udp,
            webserver_log,
            auto_reconnect,
            ..
        } => {
            assert_eq!(subdomain, "myapp");
            assert_eq!(client_id, "client-a");
            assert_eq!(notes.as_deref(), Some("test note"));
            assert!(!basic_auth);
            assert_eq!(carriers, 1);
            assert!(!udp);
            assert!(!webserver_log);
            // D6: auto_reconnect round-trips on the wire.
            assert!(auto_reconnect);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn vhost_ready_round_trips() {
    let msg = ServerMessage::VhostReady {
        http_url: Some("http://myapp.bore.example.com".to_string()),
        https_url: Some("https://myapp.bore.example.com".to_string()),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let round: ServerMessage = serde_json::from_str(&json).unwrap();
    match round {
        ServerMessage::VhostReady {
            http_url,
            https_url,
        } => {
            assert_eq!(http_url.as_deref(), Some("http://myapp.bore.example.com"));
            assert_eq!(https_url.as_deref(), Some("https://myapp.bore.example.com"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn serde_roundtrip_vpn_messages() {
    let msg = ClientMessage::HelloVpn {
        id: "test".into(),
        advertised: vec![Ipv4Net {
            addr: "10.0.0.0".parse().unwrap(),
            prefix: 24,
        }],
        addr: VpnAddrRequest::Pool,
        notes: None,
        carriers: 1,
        max_clients: 0,
        relay_only: false,
        pin_mtu: false,
        mtu: None,
        forward_accept: false,
        nat_masquerade: false,
        route_policy: None,
        nat_udp_preferred_port: 0,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let back: ClientMessage = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, ClientMessage::HelloVpn { .. }));

    let msg = ServerMessage::VpnReady {
        assigned: "10.99.0.1".parse().unwrap(),
        prefix: 30,
        peer_overlay: "10.99.0.2".parse().unwrap(),
        peer_advertised: vec![],
        session_nonce: [0u8; 16],
        tuning: UdpDirectTuning::default(),
        admin_v2: true,
        carriers: 1,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let back: ServerMessage = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, ServerMessage::VpnReady { .. }));

    let msg = ServerMessage::VpnError("test error".into());
    let json = serde_json::to_string(&msg).unwrap();
    let back: ServerMessage = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, ServerMessage::VpnError(_)));
}

#[test]
fn forward_compat_unknown_fields_default() {
    let json = r#"{"HelloVpn":{"id":"x","advertised":[],"addr":"Pool","notes":null}}"#;
    let msg: ClientMessage = serde_json::from_str(json).unwrap();
    if let ClientMessage::HelloVpn { carriers, .. } = msg {
        assert_eq!(carriers, 0);
    } else {
        panic!("unexpected variant");
    }
}

#[test]
fn t_vpnwire_old_hello_deserializes_with_defaults() {
    // Old client sends HelloVpn without the new display fields; server deserializes with defaults.
    let json = r#"{"HelloVpn":{"id":"test","advertised":[],"addr":"Pool","notes":null,"carriers":1,"max_clients":0}}"#;
    let msg: ClientMessage = serde_json::from_str(json).unwrap();
    if let ClientMessage::HelloVpn {
        relay_only,
        pin_mtu,
        mtu,
        forward_accept,
        nat_masquerade,
        route_policy,
        ..
    } = msg
    {
        assert!(!relay_only);
        assert!(!pin_mtu);
        assert_eq!(mtu, None);
        assert!(!forward_accept);
        assert!(!nat_masquerade);
        assert_eq!(route_policy, None);
    } else {
        panic!("unexpected variant");
    }
}

#[test]
fn t_vpnwire_hello_roundtrip_with_flags() {
    // New client sends HelloVpn with all display flags set; roundtrip preserves them.
    let msg = ClientMessage::HelloVpn {
        id: "test".to_string(),
        advertised: vec![],
        addr: VpnAddrRequest::Pool,
        notes: None,
        carriers: 1,
        max_clients: 0,
        relay_only: true,
        pin_mtu: true,
        mtu: Some(1500),
        forward_accept: true,
        nat_masquerade: true,
        route_policy: Some("accept:2 refuse:1".to_string()),
        nat_udp_preferred_port: 8443,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let back: ClientMessage = serde_json::from_str(&json).unwrap();
    if let ClientMessage::HelloVpn {
        relay_only,
        pin_mtu,
        mtu,
        forward_accept,
        nat_masquerade,
        route_policy,
        nat_udp_preferred_port,
        ..
    } = back
    {
        assert!(relay_only);
        assert!(pin_mtu);
        assert_eq!(mtu, Some(1500));
        assert!(forward_accept);
        assert!(nat_masquerade);
        assert_eq!(route_policy, Some("accept:2 refuse:1".to_string()));
        assert_eq!(nat_udp_preferred_port, 8443);
    } else {
        panic!("unexpected variant");
    }
}

#[test]
fn t_vpnwire_old_connect_deserializes_with_defaults() {
    // Old client sends ConnectVpn without the new display fields.
    let json =
        r#"{"ConnectVpn":{"id":"test","advertised":[],"addr":"Pool","notes":null,"carriers":1}}"#;
    let msg: ClientMessage = serde_json::from_str(json).unwrap();
    if let ClientMessage::ConnectVpn {
        relay_only,
        pin_mtu,
        mtu,
        forward_accept,
        nat_masquerade,
        route_policy,
        ..
    } = msg
    {
        assert!(!relay_only);
        assert!(!pin_mtu);
        assert_eq!(mtu, None);
        assert!(!forward_accept);
        assert!(!nat_masquerade);
        assert_eq!(route_policy, None);
    } else {
        panic!("unexpected variant");
    }
}

#[test]
fn ipv4net_overlaps() {
    let a: Ipv4Net = "10.0.0.0/24".parse().unwrap();
    let b: Ipv4Net = "10.0.0.0/25".parse().unwrap();
    let c: Ipv4Net = "10.0.1.0/24".parse().unwrap();
    assert!(a.overlaps(&b));
    assert!(b.overlaps(&a));
    assert!(!a.overlaps(&c));
    let d: Ipv4Net = "192.168.0.0/30".parse().unwrap();
    let e: Ipv4Net = "192.168.0.4/30".parse().unwrap();
    assert!(!d.overlaps(&e));
}

#[test]
fn advertise_parse_plain_no_at() {
    let e: AdvertiseEntry = "192.168.50.0/24".parse().unwrap();
    assert_eq!(e.real, e.exposed);
    assert!(!e.is_nat());
    assert_eq!(e.real, "192.168.50.0/24".parse::<Ipv4Net>().unwrap());
    assert_eq!(e.to_string(), "192.168.50.0/24");
}

#[test]
fn advertise_parse_nat_at() {
    let e: AdvertiseEntry = "192.168.1.0/24@10.50.1.0/24".parse().unwrap();
    assert_eq!(e.real, "192.168.1.0/24".parse::<Ipv4Net>().unwrap());
    assert_eq!(e.exposed, "10.50.1.0/24".parse::<Ipv4Net>().unwrap());
    assert!(e.is_nat());
    assert_eq!(e.to_string(), "192.168.1.0/24@10.50.1.0/24");
}

#[test]
fn advertise_parse_rejects_prefix_mismatch() {
    let r = "192.168.1.0/24@10.50.1.0/25".parse::<AdvertiseEntry>();
    assert!(r.is_err());
    assert!(r.unwrap_err().to_string().contains("equal prefix length"));
}

#[test]
fn advertise_parse_rejects_double_at() {
    assert!("192.168.1.0/24@10.50.1.0/24@x"
        .parse::<AdvertiseEntry>()
        .is_err());
}

#[test]
fn advertise_parse_rejects_bad_cidr() {
    assert!("not-a-cidr".parse::<AdvertiseEntry>().is_err());
    assert!("192.168.1.0/24@not-a-cidr"
        .parse::<AdvertiseEntry>()
        .is_err());
}

#[test]
fn advertise_parse_mixed_list() {
    let items = ["192.168.1.0/24@10.50.1.0/24", "172.16.0.0/24"];
    let entries: Vec<AdvertiseEntry> = items.iter().map(|s| s.parse().unwrap()).collect();
    assert_eq!(entries.len(), 2);
    assert!(entries[0].is_nat());
    assert!(!entries[1].is_nat());
    let exposed: Vec<Ipv4Net> = entries.iter().map(|e| e.exposed).collect();
    assert_eq!(exposed[0], "10.50.1.0/24".parse::<Ipv4Net>().unwrap());
    assert_eq!(exposed[1], "172.16.0.0/24".parse::<Ipv4Net>().unwrap());
}

#[test]
fn hello_vpn_serde_roundtrip_with_and_without_max_clients() {
    // Test legacy payload without max_clients deserializes to 0
    let json =
        r#"{"HelloVpn":{"id":"test","advertised":[],"addr":"Pool","notes":null,"carriers":1}}"#;
    let msg: ClientMessage = serde_json::from_str(json).unwrap();
    if let ClientMessage::HelloVpn { max_clients, .. } = msg {
        assert_eq!(max_clients, 0);
    } else {
        panic!("unexpected variant");
    }

    // Test roundtrip with max_clients set
    let msg = ClientMessage::HelloVpn {
        id: "hub1".to_string(),
        advertised: vec!["192.168.0.0/24".parse().unwrap()],
        addr: VpnAddrRequest::Pool,
        notes: Some("test hub".to_string()),
        carriers: 2,
        max_clients: 4,
        relay_only: false,
        pin_mtu: false,
        mtu: None,
        forward_accept: false,
        nat_masquerade: false,
        route_policy: None,
        nat_udp_preferred_port: 0,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let back: ClientMessage = serde_json::from_str(&json).unwrap();
    if let ClientMessage::HelloVpn { max_clients, .. } = back {
        assert_eq!(max_clients, 4);
    } else {
        panic!("unexpected variant");
    }
}

#[test]
fn t_wire_heartbeat_roundtrip() {
    // T-WIRE1: the appended `ClientMessage::Heartbeat` variant round-trips and
    // does not disturb the existing variants (it is the secret-tunnel liveness
    // ping the server's reaper relies on).
    let json = serde_json::to_string(&ClientMessage::Heartbeat).unwrap();
    let back: ClientMessage = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, ClientMessage::Heartbeat));
    // A pre-existing variant still decodes unchanged alongside the new one.
    let auth: ClientMessage = serde_json::from_str(r#"{"Authenticate":"tok"}"#).unwrap();
    assert!(matches!(auth, ClientMessage::Authenticate(_)));
}

#[test]
fn vpn_peer_join_leave_serde_roundtrip() {
    let msg = ServerMessage::VpnPeerJoin {
        peer_id: 1,
        peer_overlay: "10.99.0.2".parse().unwrap(),
        peer_advertised: vec!["192.168.0.0/24".parse().unwrap()],
        session_nonce: [1u8; UDP_NONCE_LEN],
        carriers: 2,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let back: ServerMessage = serde_json::from_str(&json).unwrap();
    if let ServerMessage::VpnPeerJoin { peer_id, .. } = back {
        assert_eq!(peer_id, 1);
    } else {
        panic!("unexpected variant");
    }

    let msg = ServerMessage::VpnPeerLeave { peer_id: 2 };
    let json = serde_json::to_string(&msg).unwrap();
    let back: ServerMessage = serde_json::from_str(&json).unwrap();
    if let ServerMessage::VpnPeerLeave { peer_id } = back {
        assert_eq!(peer_id, 2);
    } else {
        panic!("unexpected variant");
    }
}

#[test]
fn udp_punch_peer_id_default_zero() {
    // Test legacy UdpPunch JSON without peer_id deserializes to 0
    let json = r#"{"UdpPunch":{"nonce":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"peer":[],"peer_selected_stun":null,"tuning":{"stream_receive_window":0,"connection_receive_window":0,"send_window":0,"udp_socket_recv_buffer":0,"udp_socket_send_buffer":0,"max_direct_streams":0}}}"#;
    let msg: ServerMessage = serde_json::from_str(json).unwrap();
    if let ServerMessage::UdpPunch { peer_id, .. } = msg {
        assert_eq!(peer_id, 0);
    } else {
        panic!("unexpected variant");
    }
}

#[test]
fn udp_candidate_offer_peer_id_default_zero() {
    // Test legacy UdpCandidateOffer JSON without peer_id deserializes to 0
    let json = r#"{"candidates":[],"selected_stun":null}"#;
    let offer: UdpCandidateOffer = serde_json::from_str(json).unwrap();
    assert_eq!(offer.peer_id, 0);
}

#[test]
fn tunnel_options_serde_default_webserver_log() {
    // Deserialize TunnelOptions without webserver_log field → defaults to false
    let json = r#"{"https":false,"force_https":false,"basic_auth":null,"notes":null,"carriers":0,"udp":false,"auto_reconnect":false}"#;
    let opts: TunnelOptions = serde_json::from_str(json).unwrap();
    assert!(!opts.webserver_log);
}

#[test]
fn https_policy_serde_roundtrip() {
    assert_eq!(
        serde_json::to_string(&HttpsPolicy::Redirect).unwrap(),
        "\"redirect\""
    );
    assert_eq!(serde_json::to_string(&HttpsPolicy::On).unwrap(), "\"on\"");
    assert_eq!(serde_json::to_string(&HttpsPolicy::Off).unwrap(), "\"off\"");
    let redirect: HttpsPolicy = serde_json::from_str("\"redirect\"").unwrap();
    assert_eq!(redirect, HttpsPolicy::Redirect);
    let on: HttpsPolicy = serde_json::from_str("\"on\"").unwrap();
    assert_eq!(on, HttpsPolicy::On);
    let off: HttpsPolicy = serde_json::from_str("\"off\"").unwrap();
    assert_eq!(off, HttpsPolicy::Off);
}

#[test]
fn https_policy_valueenum_parse() {
    use clap::ValueEnum;
    let on = HttpsPolicy::from_str("on", false).unwrap();
    assert_eq!(on, HttpsPolicy::On);
    let off = HttpsPolicy::from_str("off", false).unwrap();
    assert_eq!(off, HttpsPolicy::Off);
    let redirect = HttpsPolicy::from_str("redirect", false).unwrap();
    assert_eq!(redirect, HttpsPolicy::Redirect);
    let invalid = HttpsPolicy::from_str("bogus", false);
    assert!(invalid.is_err());
}

#[test]
fn resolve_policy_off() {
    let (https, force_https, downgraded) = resolve_https_policy(HttpsPolicy::Off, false);
    assert!(!https);
    assert!(!force_https);
    assert!(!downgraded);
    let (https, force_https, downgraded) = resolve_https_policy(HttpsPolicy::Off, true);
    assert!(!https);
    assert!(!force_https);
    assert!(!downgraded);
}

#[test]
fn resolve_policy_on_capable() {
    let (https, force_https, downgraded) = resolve_https_policy(HttpsPolicy::On, true);
    assert!(https);
    assert!(!force_https);
    assert!(!downgraded);
}

#[test]
fn resolve_policy_on_incapable() {
    let (https, force_https, downgraded) = resolve_https_policy(HttpsPolicy::On, false);
    assert!(!https);
    assert!(!force_https);
    assert!(downgraded);
}

#[test]
fn resolve_policy_redirect_capable() {
    let (https, force_https, downgraded) = resolve_https_policy(HttpsPolicy::Redirect, true);
    assert!(https);
    assert!(force_https);
    assert!(!downgraded);
}

#[test]
fn resolve_policy_redirect_incapable() {
    let (https, force_https, downgraded) = resolve_https_policy(HttpsPolicy::Redirect, false);
    assert!(!https);
    assert!(!force_https);
    assert!(downgraded);
}

#[test]
fn server_message_warning_roundtrip() {
    let msg = ServerMessage::Warning("test warning".to_string());
    let json = serde_json::to_string(&msg).unwrap();
    let back: ServerMessage = serde_json::from_str(&json).unwrap();
    match back {
        ServerMessage::Warning(text) => assert_eq!(text, "test warning"),
        _ => panic!("unexpected variant"),
    }
}

#[test]
fn server_message_warning_is_last_variant() {
    let ok_msg = serde_json::from_str::<ServerMessage>(r#"{"Ok":null}"#).unwrap();
    match ok_msg {
        ServerMessage::Ok => {}
        _ => panic!("unexpected variant"),
    }
}

#[test]
fn tunnel_options_default_policy_none() {
    let opts = TunnelOptions::default();
    assert!(opts.https_policy.is_none());
}

#[test]
fn hello_vhost_serde_omits_default_policy() {
    let msg = ClientMessage::HelloVhost {
        subdomain: "test".to_string(),
        client_id: "id".to_string(),
        notes: None,
        basic_auth: false,
        carriers: 0,
        udp: false,
        webserver_log: false,
        auto_reconnect: false,
        local_host: None,
        local_port: 0,
        https_policy: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let round: ClientMessage = serde_json::from_str(&json).unwrap();
    match round {
        ClientMessage::HelloVhost { https_policy, .. } => {
            assert!(https_policy.is_none());
        }
        _ => panic!("unexpected variant"),
    }
    let legacy_json = r#"{"HelloVhost":{"subdomain":"test","client_id":"id","notes":null,"basic_auth":false,"carriers":0,"udp":false,"webserver_log":false,"auto_reconnect":false,"local_host":null,"local_port":0}}"#;
    let legacy: ClientMessage = serde_json::from_str(legacy_json).unwrap();
    match legacy {
        ClientMessage::HelloVhost { https_policy, .. } => {
            assert!(https_policy.is_none());
        }
        _ => panic!("unexpected variant"),
    }
}
