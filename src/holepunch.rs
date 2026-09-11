//! UDP hole-punching with STUN reflexive-address discovery and a QUIC carrier,
//! used for the `udp` direct-path mode of secret tunnels (see [`crate::secret`]).
//!
//! The server only ever brokers candidate addresses over the (authenticated)
//! control channel — it never sees the punched data path. Two peers of the same
//! secret tunnel each open a UDP socket, learn their public (reflexive) mapping
//! via STUN, exchange candidates through the server, simultaneously send UDP
//! packets to open their NAT mappings, then establish a QUIC connection over that
//! socket. Each proxied connection uses its own native QUIC bidirectional stream,
//! so the direct path avoids TCP/yamux head-of-line blocking. If any step fails
//! the caller falls back to the server relay.
//!
//! Authentication of the direct path is a shared token derived from the tunnel
//! secret and a server-issued nonce ([`derive_token`]): both peers prove
//! knowledge of it on the first bytes of the QUIC stream before `yamux` starts,
//! so the self-signed QUIC certificate need not be verified.
//!
//! The QUIC carrier (and thus the actual hole-punch) requires the `udp` feature,
//! which pulls in `quinn`. The signaling primitives the *server* needs to
//! broker a direct path — STUN reflexive discovery, the STUN responder, and the
//! token derivation — carry no `quinn` dependency and are always compiled, so a
//! lean-built server can still rendezvous for `udp`-enabled clients.

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::shared::{
    UdpAdaptiveCandidateKind, UdpAdaptivePlan, UdpCandidateKind, UdpCandidateOffer,
    UdpDirectTuning, UdpNatMapping, UdpNatProfile, UdpTypedCandidate, CONTROL_PORT,
    UDP_CAP_CANDIDATE_V2, UDP_CAP_CHECK_V1,
};

/// Number of consecutive ports predicted past the reflexive one when
/// `--try-port-prediction` is enabled (best-effort symmetric-NAT traversal).
const PREDICT_RANGE: u16 = 4;

/// Upper bound on UDP hole-punch candidate addresses accepted from a peer,
/// offered on the wire, or punched/dialed in one traversal round. Every
/// peer-controlled candidate list is clamped to this bound *before* any
/// proportional allocation or per-candidate task fan-out. The legitimate
/// worst case today (1 reflexive + 4 predicted + 1 UPnP + 1 local) is 7,
/// so 16 leaves room for future manual/multi-interface candidates without
/// letting a hostile peer turn the puncher into a scanner.
pub const MAX_UDP_CANDIDATES: usize = 16;

/// Aggregate counters from [`sanitize_candidates`]: how many entries were
/// dropped and why. Logged as ONE line per round (never one warning per stray).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CandidateSanitation {
    /// Structurally unusable entries (port 0, unspecified, multicast, broadcast).
    pub dropped_invalid: usize,
    /// Exact duplicates of an earlier entry (order-preserving dedup).
    pub dropped_duplicate: usize,
    /// Entries past the [`MAX_UDP_CANDIDATES`] cap.
    pub dropped_overflow: usize,
}

impl CandidateSanitation {
    /// Total number of dropped entries.
    pub fn dropped(&self) -> usize {
        self.dropped_invalid + self.dropped_duplicate + self.dropped_overflow
    }
}

/// Whether `addr` may be offered or punched as a UDP hole-punch candidate.
///
/// Private/CGNAT addresses are VALID — same-LAN peers need them, and the
/// accepted QUIC source is authenticated by token, never by candidate list.
/// Only addresses that are unusable by construction are rejected: port 0,
/// unspecified, multicast, and the IPv4 broadcast address.
pub fn valid_candidate(addr: &SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(ip) => !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast(),
        IpAddr::V6(ip) => !ip.is_unspecified() && !ip.is_multicast(),
    }
}

/// Validate, dedup (order-preserving) and cap a candidate list in place.
/// Returns aggregate drop counters; log them with [`log_dropped_candidates`].
/// Shared by the sender (before the wire), the server broker (before storing
/// or forwarding a peer-controlled list) and the punch/dial entry points
/// (defense in depth) — one implementation so no path drifts.
pub fn sanitize_candidates(candidates: &mut Vec<SocketAddr>) -> CandidateSanitation {
    let mut san = CandidateSanitation::default();
    let mut seen: Vec<SocketAddr> = Vec::with_capacity(MAX_UDP_CANDIDATES);
    candidates.retain(|addr| {
        if !valid_candidate(addr) {
            san.dropped_invalid += 1;
            return false;
        }
        if seen.contains(addr) {
            san.dropped_duplicate += 1;
            return false;
        }
        if seen.len() >= MAX_UDP_CANDIDATES {
            san.dropped_overflow += 1;
            return false;
        }
        seen.push(*addr);
        true
    });
    san
}

/// Sanitize a wire [`UdpCandidateOffer`] in place (server broker / receiver
/// side). Logs one aggregate line when anything was dropped.
pub fn sanitize_offer(offer: &mut UdpCandidateOffer, context: &'static str) {
    let san = sanitize_candidates(&mut offer.candidates);
    log_dropped_candidates(context, offer.candidates.len(), &san);
}

/// One aggregate log line for a sanitized candidate list. `warn` because a
/// non-zero drop count means a peer sent something out of contract (or a local
/// gather bug); still never per-item.
pub fn log_dropped_candidates(context: &'static str, kept: usize, san: &CandidateSanitation) {
    if san.dropped() == 0 {
        return;
    }
    warn!(
        context,
        kept,
        invalid = san.dropped_invalid,
        duplicate = san.dropped_duplicate,
        overflow = san.dropped_overflow,
        limit = MAX_UDP_CANDIDATES,
        "dropped unusable UDP candidates (aggregate)"
    );
}

/// Sanitize the parallel `(candidates, kinds)` vectors produced by candidate
/// discovery, keeping them index-aligned.
fn sanitize_discovery(
    candidates: &mut Vec<SocketAddr>,
    kinds: &mut Vec<UdpCandidateKind>,
) -> CandidateSanitation {
    debug_assert_eq!(candidates.len(), kinds.len());
    let mut san = CandidateSanitation::default();
    let mut out_c = Vec::with_capacity(candidates.len().min(MAX_UDP_CANDIDATES));
    let mut out_k = Vec::with_capacity(candidates.len().min(MAX_UDP_CANDIDATES));
    for (addr, kind) in candidates.iter().zip(kinds.iter()) {
        if !valid_candidate(addr) {
            san.dropped_invalid += 1;
            continue;
        }
        if out_c.contains(addr) {
            san.dropped_duplicate += 1;
            continue;
        }
        if out_c.len() >= MAX_UDP_CANDIDATES {
            san.dropped_overflow += 1;
            continue;
        }
        out_c.push(*addr);
        out_k.push(*kind);
    }
    *candidates = out_c;
    *kinds = out_k;
    san
}

#[cfg(feature = "udp")]
use crate::shared::NETWORK_TIMEOUT;
#[cfg(feature = "udp")]
use quinn::rustls;
#[cfg(feature = "udp")]
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime};
#[cfg(feature = "udp")]
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
#[cfg(feature = "udp")]
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
#[cfg(feature = "udp")]
use tracing::trace;

/// Length of the shared authentication token (HMAC-SHA256 output).
pub const TOKEN_LEN: usize = 32;

/// Maximum key length accepted by the shared vhost/public/jump QUIC
/// authentication stream. Bounds allocation before reading attacker data.
#[cfg(feature = "udp")]
pub(crate) const MAX_SERVER_DIRECT_KEY_LEN: usize = 128;

/// Per-attempt timeout for a STUN binding request (kept short so a missing STUN
/// server fails fast and the caller can fall back to the relay).
const STUN_TIMEOUT: Duration = Duration::from_secs(1);

/// Global budget for the whole STUN chain on a [`UdpTraversalSocket`] (plan
/// Fase 1): with N unreachable targets the legacy serial chain burned
/// N × 3 × [`STUN_TIMEOUT`] (~12 s for 4 targets) before the relay decision;
/// the demuxed chain runs transactions concurrently under this single budget.
pub const STUN_CHAIN_BUDGET: Duration = Duration::from_secs(4);

/// Launch offset between consecutive STUN chain targets: preserves the chain's
/// preference order as a head start (the earlier target usually answers first)
/// without serializing the worst case.
const STUN_CHAIN_STAGGER: Duration = Duration::from_millis(300);

/// Bounded extra wait for the SECOND STUN observation that turns a gather
/// into a structured NAT profile (plan Fase 3). The first two chain targets
/// launch together, so the confirming answer usually arrives within
/// |RTT₂−RTT₁| of the winner; this cap only bites when the second server is
/// slow or dead. Never extends [`STUN_CHAIN_BUDGET`].
const PROFILE_CONFIRM_WAIT: Duration = Duration::from_millis(400);

/// Pacing between outbound connectivity-check requests (plan Fase 2): one
/// request every tick, round-robin across the candidate pairs. Comparable to
/// the legacy punch cadence (5 packets / 50 ms per candidate) but paced across
/// pairs instead of bursting, and it keeps probing for the whole window.
const CHECK_PACE: Duration = Duration::from_millis(50);

/// Default total budget for one connectivity-check round. Bounded (I-11/plan):
/// the relay stays warm the whole time, and on a fully-blocked pair this
/// window is ADDED to the QUIC dial budget before the relay decision — 1 s
/// keeps the worst-case time-to-relay within ~0.75 s of the legacy path
/// (the redundant blind punch is skipped after a check round, winning back
/// ~250 ms; measured deltas documented in docs/nat/NAT_TRAVERSAL.md §16).
pub const CHECK_WINDOW: Duration = Duration::from_millis(1000);

/// Upper bound on connectivity-check responses sent per round — the responder
/// is not an amplifier even under a spoofed-request flood (responses are also
/// never larger than requests).
const CHECK_MAX_RESPONSES: u32 = 256;

/// Launch offset between consecutive candidate-kind groups in a planned check
/// round (plan Fase 3): the preferred kind gets a clean head start, later
/// kinds still launch well within the round window (staggered checklist —
/// neither fully serial nor unlimited fan-out).
const CHECK_GROUP_STAGGER: Duration = Duration::from_millis(150);

/// Hard cap on one check round INCLUDING every retry pass the adaptive plan
/// may grant — the plan governs a single bounded round, never an unbounded
/// prober (the outer retry scheduler stays the VPN grid / secret backoff).
const CHECK_TOTAL_CAP: Duration = Duration::from_secs(3);

/// Upper bound (exclusive) on the per-probe pacing jitter. Strictly below
/// [`CHECK_PACE`] so jitter can only ever de-synchronize the two peers'
/// probe cadence, never reorder the pacing itself. De-synchronizing matters:
/// the Fase-0 pcap analysis proved a conntrack "crossfire" race on masquerade
/// routers where BOTH peers probing in lockstep makes the inbound packet
/// claim the reply-tuple before the outbound punch egresses, remapping the
/// mapping to a random port. Jittered pacing breaks the lockstep.
const CHECK_JITTER_MAX: Duration = Duration::from_millis(15);

/// Deterministic per-peer jitter seed: the shared check key XOR the role byte
/// — both peers derive DIFFERENT sequences (role differs) without any extra
/// wire exchange, and tests get full determinism.
fn check_jitter_seed(key: &[u8; 32], role: u8) -> u64 {
    let mut seed = u64::from_le_bytes(key[..8].try_into().expect("key >= 8 bytes"));
    seed ^= u64::from(role).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    seed
}

/// Bounded deterministic jitter for probe `step` (SplitMix64 over the seed):
/// always in `[0, CHECK_JITTER_MAX)`.
fn check_jitter(seed: u64, step: u64) -> Duration {
    let mut z = seed.wrapping_add(step.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    Duration::from_micros(z % (CHECK_JITTER_MAX.as_micros() as u64))
}

/// ALPN protocol identifier for the direct QUIC carrier.
#[cfg(feature = "udp")]
const ALPN: &[u8] = b"bore-udp";

/// How long to keep a quiet QUIC connection alive with keep-alive pings, and the
/// idle timeout after which it is considered dead. The keep-alive (every 3s)
/// keeps a long but quiet transfer alive; the idle timeout (10s) makes a peer
/// that vanished without a graceful close (hard kill, network partition) be
/// detected within ~10s so the consumer can re-negotiate or fall back.
#[cfg(feature = "udp")]
const QUIC_KEEPALIVE: Duration = Duration::from_secs(3);
#[cfg(feature = "udp")]
const QUIC_MAX_IDLE: Duration = Duration::from_secs(10);

/// Floor on the keep-alive interval: below this the ping traffic itself starts
/// to matter across a large pool of quiet connections.
#[cfg(feature = "udp")]
const QUIC_KEEPALIVE_MIN: Duration = Duration::from_millis(200);
/// Floor on the idle timeout. One second, because anything shorter cannot
/// survive a single lost keep-alive at any legal interval.
#[cfg(feature = "udp")]
const QUIC_MAX_IDLE_MIN: Duration = Duration::from_secs(1);
/// Ceiling on the idle timeout, so a typo cannot pin a dead connection open for
/// hours while every request committed to it hangs.
#[cfg(feature = "udp")]
const QUIC_MAX_IDLE_MAX: Duration = Duration::from_secs(600);

/// Resolved liveness timings for a direct QUIC connection.
///
/// These two constants are the *whole* mechanism behind the residual F-14 gap
/// measured in `docs/performance/`: when the peer goes silent, `open_bi` and the
/// `STREAM_READY` write both succeed locally (neither needs a round trip once
/// stream credit exists), so the request is already committed to the stream and
/// then simply waits for the connection to die. It dies at the idle timeout.
/// The window in which requests are lost after UDP disappears is therefore
/// exactly `max_idle`, and shortening it is the one lever that does not risk
/// abandoning a slow-but-healthy origin.
#[cfg(feature = "udp")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectQuicLiveness {
    /// Interval between keep-alive pings on an otherwise quiet connection.
    pub keepalive: Duration,
    /// Idle timeout after which the connection is declared dead.
    pub max_idle: Duration,
    /// `true` when the requested keep-alive was tightened so that two
    /// consecutive losses stay survivable under the requested idle timeout.
    pub keepalive_adjusted: bool,
}

/// Read a positive millisecond override from the environment.
///
/// A zero or unparseable value is treated as unset rather than as an error:
/// these are diagnostic knobs, and refusing to start over a typo in one would
/// be a worse failure than running the shipped default.
#[cfg(feature = "udp")]
fn env_ms(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|ms| *ms > 0)
}

/// Resolve the direct-path QUIC liveness pair from optional operator overrides.
///
/// Pure, so the policy is unit-testable without an endpoint. `None` for either
/// input means "unset" and yields the shipped constant, so an unconfigured
/// process is byte-identical to before this knob existed.
///
/// The one policy decision: a keep-alive that is not comfortably shorter than
/// the idle timeout tears down HEALTHY connections, because one lost ping then
/// already exceeds the deadline. The resolved pair therefore always satisfies
/// `max_idle >= 3 * keepalive` (two consecutive losses survivable). When the
/// operator's pair does not, the KEEP-ALIVE is tightened rather than the idle
/// timeout relaxed — the operator asked for faster death detection and that is
/// the request honoured, with `keepalive_adjusted` set so the change is
/// reported instead of applied silently (I-2).
#[cfg(feature = "udp")]
pub fn resolve_direct_quic_liveness(
    keepalive_ms: Option<u64>,
    idle_ms: Option<u64>,
) -> DirectQuicLiveness {
    let max_idle = idle_ms
        .map(Duration::from_millis)
        .unwrap_or(QUIC_MAX_IDLE)
        .clamp(QUIC_MAX_IDLE_MIN, QUIC_MAX_IDLE_MAX);
    let requested = keepalive_ms
        .map(Duration::from_millis)
        .unwrap_or(QUIC_KEEPALIVE)
        .max(QUIC_KEEPALIVE_MIN);
    let ceiling = (max_idle / 3).max(QUIC_KEEPALIVE_MIN);
    let keepalive = requested.min(ceiling);
    DirectQuicLiveness {
        keepalive,
        max_idle,
        keepalive_adjusted: keepalive != requested,
    }
}

/// The live direct-path QUIC liveness pair, including operator overrides.
///
/// Read per call — same shape as `vhost::direct_open_timeout` and
/// `sshgw::ssh_open_timeout` — so a gate can change it without a restart.
#[cfg(feature = "udp")]
pub fn direct_quic_liveness() -> DirectQuicLiveness {
    resolve_direct_quic_liveness(
        env_ms("BORE_DIRECT_QUIC_KEEPALIVE_MS"),
        env_ms("BORE_DIRECT_QUIC_IDLE_MS"),
    )
}

/// Initial RTT a direct QUIC endpoint assumes before it has a sample of its
/// own (S-7). RFC 9002's 333 ms default is the value for a connection with NO
/// prior information about the path; a bore direct endpoint is never in that
/// position. It is built only after either a completed authenticated check
/// exchange with this exact peer (the punched paths) or a TCP control
/// connection to this exact host (vhost, public, ssh-jump), so the path has
/// already been traversed at least once before the first QUIC packet.
///
/// The asymmetry of the two errors is what fixes the value. Underestimating
/// costs ONE duplicate Initial on a path slower than the estimate, after which
/// the first real sample governs. Overestimating costs a full PTO of silence
/// whenever the first Initial is lost — and the first Initial of a punched
/// path is exactly the packet most likely to be lost, because it is the first
/// datagram to cross a mapping the peer has only just created. MEASURED on
/// staging 2026-09-11: 9 of 27 direct establishments paid 1036..1162 ms,
/// against 37..53 ms for the other 18, and the constant was quinn's
/// `333 ms + 4 * 166 ms = 999 ms` initial PTO, not the network.
#[cfg(feature = "udp")]
pub const DIRECT_INITIAL_RTT: Duration = Duration::from_millis(100);

/// Lower bound for the resolved initial RTT: below this, a healthy path's
/// first PTO would fire inside its own handshake round trip.
#[cfg(feature = "udp")]
const DIRECT_INITIAL_RTT_MIN: Duration = Duration::from_millis(10);

/// Upper bound: RFC 9002's own default, which is the most pessimistic value
/// that can be called informed.
#[cfg(feature = "udp")]
const DIRECT_INITIAL_RTT_MAX: Duration = Duration::from_millis(333);

/// Resolve the direct-path initial RTT from an optional operator override.
/// Pure, so the policy is unit-testable without an endpoint; `None` yields the
/// shipped constant exactly.
#[cfg(feature = "udp")]
pub fn resolve_direct_initial_rtt(ms: Option<u64>) -> Duration {
    ms.map(Duration::from_millis)
        .unwrap_or(DIRECT_INITIAL_RTT)
        .clamp(DIRECT_INITIAL_RTT_MIN, DIRECT_INITIAL_RTT_MAX)
}

/// The live direct-path initial RTT, including the operator override.
#[cfg(feature = "udp")]
pub fn direct_initial_rtt() -> Duration {
    resolve_direct_initial_rtt(env_ms("BORE_DIRECT_QUIC_INITIAL_RTT_MS"))
}

/// Ack-eliciting threshold requested from the peer through the QUIC ACK
/// Frequency extension (draft-ietf-quic-ack-frequency-04), or `None` for
/// quinn's default behaviour — which is the extension DISABLED and an ACK for
/// every other ack-eliciting packet.
///
/// A threshold of N asks the peer to acknowledge at most once every N+1
/// ack-eliciting packets, which on a saturated direct path removes most of the
/// return-path packet rate. It is an experiment knob, not a default: both ends
/// of a bore direct path are bore, so the extension is always negotiable, but
/// acknowledging less often also delays loss detection, and a congestion
/// controller reacts to what it is told. The default therefore stays quinn's
/// until a measurement on this project's own paths says otherwise, and the
/// measurement is what the env var exists for.
///
/// `reordering_threshold` follows quinn's own recommendation of
/// `packet_threshold - 1` (2 with the default packet threshold of 3), so
/// out-of-order delivery still elicits an immediate ACK and fast retransmit is
/// unaffected by the change.
#[cfg(feature = "udp")]
pub fn direct_ack_eliciting_threshold() -> Option<u64> {
    std::env::var("BORE_DIRECT_QUIC_ACK_THRESHOLD")
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
}

type HmacSha256 = Hmac<Sha256>;

/// Derive the shared QUIC authentication token from the tunnel secret (if any)
/// and the server-issued session nonce. Both peers compute the same value.
pub fn derive_token(secret: Option<&str>, nonce: &[u8]) -> [u8; TOKEN_LEN] {
    let key = secret.map(str::as_bytes).unwrap_or(&[]);
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(nonce);
    let mut token = [0u8; TOKEN_LEN];
    token.copy_from_slice(&mac.finalize().into_bytes());
    token
}

/// Constant-time comparison of two tokens.
#[cfg(feature = "udp")]
fn tokens_match(a: &[u8; TOKEN_LEN], b: &[u8; TOKEN_LEN]) -> bool {
    let mut diff = 0u8;
    for i in 0..TOKEN_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Bind a UDP socket for a hole-punch session. `port` 0 picks a random ephemeral
/// port (the default); a fixed port lets a strict *egress* firewall be opened for
/// exactly that port (use the same value on both peers) and makes the public
/// mapping predictable on a port-preserving NAT. (A fixed port does not help
/// symmetric NATs, which remap per destination regardless of the local port.)
///
/// CRITICAL — NOT `SO_REUSEADDR` (see `docs/plans/udp_flap/EVIDENCE.md`): two
/// wildcard UDP sockets that BOTH set `SO_REUSEADDR` co-bind the same port and the
/// kernel delivers inbound datagrams to the *last* binder. With a shared
/// `--nat-udp-preferred-port`, a second direct-path tunnel's punch (VPN + secret,
/// vhost, public `--udp`) would silently STEAL the inbound QUIC of a live tunnel,
/// idle-closing it — the concurrent-tunnel ~30 s flap. Without `SO_REUSEADDR` the
/// second bind is cleanly refused (`EADDRINUSE`) and we fall back to an ephemeral
/// port. UDP has no TIME_WAIT, so a same-tunnel `--auto-reconnect` still rebinds
/// the fixed port fine once its previous socket has dropped (callers must
/// drop-then-bind, not overlap).
pub async fn bind_socket(port: u16) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    // Build + configure a fresh, unbound UDP socket (buffers + non-blocking).
    // Returned unbound so the fixed-port attempt can be retried on an ephemeral
    // port without reusing a socket whose bind already failed.
    fn make_socket() -> Result<Socket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .context("failed to create UDP socket")?;
        configure_udp_socket_buffers(&socket, &UdpDirectTuning::default());
        socket
            .set_nonblocking(true)
            .context("failed to set UDP socket non-blocking")?;
        Ok(socket)
    }

    let socket = make_socket()?;
    let addr: SocketAddr = (Ipv4Addr::UNSPECIFIED, port).into();
    match socket.bind(&addr.into()) {
        Ok(()) => {
            return UdpSocket::from_std(socket.into())
                .context("failed to register UDP socket with tokio");
        }
        // A fixed port already held (by another tunnel/process): degrade to an
        // ephemeral port instead of stealing the holder's inbound traffic.
        Err(e) if port != 0 && e.kind() == std::io::ErrorKind::AddrInUse => {
            warn!(
                preferred_port = port,
                "fixed UDP port {port} is already in use (another tunnel or process holds it); \
                 falling back to an ephemeral port. Behind a strict egress firewall that only \
                 permits {port}, this tunnel may stay on the relay path."
            );
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "failed to bind fixed UDP port {port} (free? allowed?)"
            )));
        }
    }

    // Ephemeral fallback after a fixed-port collision.
    let socket = make_socket()?;
    let addr: SocketAddr = (Ipv4Addr::UNSPECIFIED, 0).into();
    socket
        .bind(&addr.into())
        .context("failed to bind ephemeral UDP socket after fixed-port fallback")?;
    UdpSocket::from_std(socket.into()).context("failed to register UDP socket with tokio")
}

/// Single-owner UDP traversal socket (plan Fase 1, invariant I-5).
///
/// EXACTLY ONE task — the internal recv actor — owns `recv_from` during
/// discovery, demultiplexing STUN responses by transaction id AND full
/// `ip:port` source. Concurrent STUN transactions can therefore share the
/// socket safely, which is what lets [`Self::discover_reflexive_chain`] probe
/// the whole chain under ONE global budget ([`STUN_CHAIN_BUDGET`]) instead of
/// the legacy serial worst case (~3 s per unreachable target). Non-STUN
/// datagrams (early peer punches, QUIC Initials before handoff) are counted
/// and never consumed by a STUN waiter — Fase 2 will route them to the
/// authenticated connectivity checker.
///
/// [`Self::into_socket`] stops the actor FIRST and only then releases the raw
/// socket for Quinn, so the actor can never steal Quinn's packets (one
/// socket = one reader, always).
pub struct UdpTraversalSocket {
    socket: std::sync::Arc<UdpSocket>,
    inner: std::sync::Arc<TraversalInner>,
    actor: AbortOnDrop,
}

/// Aborts the recv actor when the traversal socket is dropped without a
/// handoff (the actor holds an `Arc` of the socket and would leak otherwise).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct TraversalInner {
    /// Pending STUN transactions, keyed by transaction id.
    pending_stun: std::sync::Mutex<HashMap<[u8; 12], PendingStun>>,
    /// Active connectivity-check round (plan Fase 2); `None` outside a round.
    checks: std::sync::Mutex<Option<CheckState>>,
    /// Datagrams that were not STUN responses (peer punches, QUIC, junk).
    peer_datagrams: AtomicU64,
    /// STUN-shaped datagrams dropped: unknown txid, wrong source, malformed.
    stray_stun: AtomicU64,
    /// Check-shaped datagrams dropped: bad HMAC, wrong generation/role/txid.
    invalid_checks: AtomicU64,
}

struct PendingStun {
    target: SocketAddr,
    /// Accept an answer from ANY port of `target`'s IP.
    ///
    /// Only the RFC 5780 filtering probe sets this, and it is the whole point
    /// of that probe: the answer is REQUESTED from a port the client never
    /// wrote to, so demuxing on the exact 5-tuple would discard exactly the
    /// datagram being measured. Widening the match to the IP keeps the guard
    /// that matters — a stranger cannot resolve someone else's transaction —
    /// while the 12-byte transaction id, which the querier generated from the
    /// system CSPRNG, remains the actual authenticator.
    any_port: bool,
    tx: oneshot::Sender<StunReply>,
}

/// A resolved STUN transaction: the mapped address the server reported, and
/// the address the answer actually CAME FROM.
///
/// The source is carried because for a filtering probe it is the measurement:
/// a server that ignores CHANGE-REQUEST answers from its ordinary port while
/// still filling in whatever attributes it likes, so only the 5-tuple the
/// kernel reports can distinguish "the server changed ports" from "the server
/// does not implement this".
#[derive(Debug, Clone, Copy)]
struct StunReply {
    mapped: SocketAddr,
    from: SocketAddr,
    /// The server's RFC 5780 OTHER-ADDRESS, when it published one. Its
    /// presence — not its value — is what makes a negative filtering result
    /// meaningful: it proves the server COULD have answered from another port.
    other: Option<SocketAddr>,
}

/// Which side of the pair this peer plays during connectivity checks. Mirrors
/// the QUIC roles: the provider/listener accepts, the consumer/dialer dials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckRole {
    /// Provider side (QUIC server).
    Listener,
    /// Consumer side (QUIC client).
    Dialer,
}

impl CheckRole {
    fn byte(self) -> u8 {
        match self {
            CheckRole::Listener => check::ROLE_LISTENER,
            CheckRole::Dialer => check::ROLE_DIALER,
        }
    }
}

/// Configuration for one connectivity-check round.
#[derive(Debug, Clone)]
pub struct CheckConfig {
    /// HMAC key, derived from the direct-path token via [`derive_check_key`].
    pub key: [u8; 32],
    /// Traversal round generation (frames from other rounds are rejected).
    pub generation: u32,
    /// This peer's role.
    pub role: CheckRole,
    /// Total budget for the round (the relay stays warm throughout).
    pub window: Duration,
    /// Planned execution (plan Fase 3): staggered kind groups + retry budget.
    /// `None` keeps the flat Fase-2 round byte-identical.
    pub plan: Option<CheckPlan>,
    /// Fase 7: which half of the sprayed escape to play if — and only if —
    /// the round comes back dry. `None`, which is what every legacy path and
    /// every cell but one produces, keeps the round AND its fallback
    /// byte-identical to before Fase 7.
    #[cfg(feature = "udp")]
    pub spray: Option<spray::SprayRole>,
}

/// Client-side execution plan for one check round (plan Fase 3), resolved
/// from the server's [`UdpAdaptivePlan`] against the actual peer candidates.
#[derive(Debug, Clone, Default)]
pub struct CheckPlan {
    /// Candidate groups in launch order: group `g` starts probing at
    /// `initial_delay + g × CHECK_GROUP_STAGGER`. Addresses only — the kind
    /// grouping is resolved by [`plan_check_groups`] before the round.
    pub groups: Vec<Vec<SocketAddr>>,
    /// Extra passes over the window after a dry first pass (paced with
    /// doubled intervals — backoff). Bounded by [`CHECK_TOTAL_CAP`].
    pub retry_budget: u8,
    /// Delay before the very first probe (the plan's `send_delay_ms`).
    pub initial_delay: Duration,
}

/// Resolve the probe-order groups for a planned check round: candidates are
/// grouped by kind and the groups ordered by `order` (the server plan's
/// `candidate_order`, relay entries skipped) or, absent a plan, by the
/// default data-driven order (same-LAN local first, then reflexive,
/// router-mapped, predicted last). The plan ORDERS candidates, it never adds
/// or drops any: kinds missing from `order` still probe, in a final group —
/// so "no predicted checks when prediction is off" holds by construction
/// (no predicted candidates were offered ⇒ no predicted group exists).
pub fn plan_check_groups(
    typed: &[UdpTypedCandidate],
    fallback: &[SocketAddr],
    order: Option<&[UdpAdaptiveCandidateKind]>,
) -> Vec<Vec<SocketAddr>> {
    if typed.is_empty() {
        // Legacy peer list: no kind metadata, single flat group.
        return if fallback.is_empty() {
            Vec::new()
        } else {
            vec![fallback.to_vec()]
        };
    }
    const DEFAULT_ORDER: [UdpCandidateKind; 4] = [
        UdpCandidateKind::Local,
        UdpCandidateKind::Reflexive,
        UdpCandidateKind::RouterMapped,
        UdpCandidateKind::Predicted,
    ];
    let kind_order: Vec<UdpCandidateKind> = match order {
        Some(order) => order
            .iter()
            .filter_map(|kind| match kind {
                UdpAdaptiveCandidateKind::Local => Some(UdpCandidateKind::Local),
                UdpAdaptiveCandidateKind::Reflexive => Some(UdpCandidateKind::Reflexive),
                UdpAdaptiveCandidateKind::RouterMapped => Some(UdpCandidateKind::RouterMapped),
                UdpAdaptiveCandidateKind::Predicted => Some(UdpCandidateKind::Predicted),
                UdpAdaptiveCandidateKind::RelayFallback => None,
            })
            .collect(),
        None => DEFAULT_ORDER.to_vec(),
    };
    let mut groups: Vec<Vec<SocketAddr>> = Vec::new();
    let mut placed: Vec<SocketAddr> = Vec::new();
    for kind in &kind_order {
        let group: Vec<SocketAddr> = typed
            .iter()
            .filter(|c| c.kind == *kind && !placed.contains(&c.addr))
            .map(|c| c.addr)
            .collect();
        if !group.is_empty() {
            placed.extend(group.iter().copied());
            groups.push(group);
        }
    }
    // Kinds the order forgot + typed dedup leftovers: last group, never dropped.
    let rest: Vec<SocketAddr> = typed
        .iter()
        .filter(|c| !placed.contains(&c.addr))
        .map(|c| c.addr)
        .collect();
    if !rest.is_empty() {
        groups.push(rest);
    }
    groups
}

/// Check-round window from the adaptive plan's `read_timeout_ms`, clamped so
/// a bogus plan can neither starve the round nor stall the relay decision.
pub fn plan_check_window(plan: &UdpAdaptivePlan) -> Duration {
    Duration::from_millis(plan.read_timeout_ms.clamp(500, 1500))
}

/// Outcome of one connectivity-check round.
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    /// First candidate pair proven BIDIRECTIONAL (our authenticated request
    /// got an authenticated response back): the nominated remote address.
    pub nominated: Option<SocketAddr>,
    /// Our own mapped address as observed by the nominated peer, when known.
    pub observed: Option<SocketAddr>,
    /// Whether a peer-reflexive candidate was learned from an authenticated
    /// inbound request arriving from an un-offered source.
    pub learned_prflx: bool,
    /// Final candidate list of the round: the sanitized offer plus any
    /// learned peer-reflexive addresses. The dialer dials THESE (a learned
    /// source must be dialable even when nomination did not complete).
    pub targets: Vec<SocketAddr>,
    /// Wall-clock duration of the round (baseline metric `checks_ms`).
    pub checks_ms: u64,
}

/// State shared between the recv actor and the driver of one check round.
struct CheckState {
    key: [u8; 32],
    generation: u32,
    role: u8,
    /// txid -> the target the request was sent to (responses must come back
    /// from exactly that source).
    pending: HashMap<[u8; 12], SocketAddr>,
    /// Validated pairs: (remote target, observed mapped address).
    validated_tx: tokio::sync::mpsc::UnboundedSender<(SocketAddr, Option<SocketAddr>)>,
    /// Sources of valid inbound requests (peer-reflexive learning).
    inbound_req_tx: tokio::sync::mpsc::UnboundedSender<SocketAddr>,
    /// Responses sent this round (anti-amplification cap).
    responses_sent: u32,
}

/// Derive the connectivity-check HMAC key from the direct-path token
/// (domain-separated so check frames can never be confused with any other
/// token use). Both peers compute the same value.
pub fn derive_check_key(token: &[u8; TOKEN_LEN]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(token).expect("HMAC accepts any key length");
    mac.update(b"bore-connectivity-check-v1");
    let mut key = [0u8; 32];
    key.copy_from_slice(&mac.finalize().into_bytes());
    key
}

impl UdpTraversalSocket {
    /// Bind via [`bind_socket`] (same fixed-port / no-`SO_REUSEADDR`
    /// semantics) and start the recv actor.
    pub async fn bind(port: u16) -> Result<Self> {
        Ok(Self::from_socket(bind_socket(port).await?))
    }

    /// Wrap an already-bound socket and start the recv actor.
    pub fn from_socket(socket: UdpSocket) -> Self {
        let socket = std::sync::Arc::new(socket);
        let inner = std::sync::Arc::new(TraversalInner {
            pending_stun: std::sync::Mutex::new(HashMap::new()),
            checks: std::sync::Mutex::new(None),
            peer_datagrams: AtomicU64::new(0),
            stray_stun: AtomicU64::new(0),
            invalid_checks: AtomicU64::new(0),
        });
        let actor = AbortOnDrop(tokio::spawn(recv_actor(
            std::sync::Arc::clone(&socket),
            std::sync::Arc::clone(&inner),
        )));
        Self {
            socket,
            inner,
            actor,
        }
    }

    /// The socket's local address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.socket.local_addr().ok()
    }

    /// Datagrams received that were not STUN responses (peer punches, QUIC
    /// Initials before handoff). Observability for the punch crossfire.
    pub fn peer_datagrams(&self) -> u64 {
        self.inner.peer_datagrams.load(Ordering::Relaxed)
    }

    /// STUN-shaped datagrams dropped (unknown txid / wrong source / malformed).
    pub fn stray_stun(&self) -> u64 {
        self.inner.stray_stun.load(Ordering::Relaxed)
    }

    /// Check-shaped datagrams dropped (bad HMAC / wrong generation / role /
    /// unknown txid / wrong source). Never answered — an unauthenticated
    /// probe gets NO response, ever.
    pub fn invalid_checks(&self) -> u64 {
        self.inner.invalid_checks.load(Ordering::Relaxed)
    }

    /// Run one authenticated connectivity-check round (plan Fase 2).
    ///
    /// Paced HMAC request/response probes over the candidate pairs; an
    /// authenticated inbound request from an un-offered source becomes a
    /// peer-reflexive candidate and gets an immediate triggered check; the
    /// first pair proven bidirectional is nominated. The relay is untouched:
    /// this only decides which address the QUIC stage should dial first.
    /// On `nominated: None` the caller falls back to the legacy dial-all.
    pub async fn run_connectivity_checks(
        &self,
        peers: &[SocketAddr],
        cfg: &CheckConfig,
    ) -> CheckOutcome {
        let started = Instant::now();
        let mut targets = peers.to_vec();
        let san = sanitize_candidates(&mut targets);
        log_dropped_candidates("connectivity-checks", targets.len(), &san);

        let (validated_tx, mut validated_rx) = tokio::sync::mpsc::unbounded_channel();
        let (inbound_req_tx, mut inbound_req_rx) = tokio::sync::mpsc::unbounded_channel();
        *self.inner.checks.lock().unwrap() = Some(CheckState {
            key: cfg.key,
            generation: cfg.generation,
            role: cfg.role.byte(),
            pending: HashMap::new(),
            validated_tx,
            inbound_req_tx,
            responses_sent: 0,
        });

        let mut learned_prflx = false;
        let mut nominated = None;
        let mut observed = None;
        // Per-source triggered-check throttle (a re-transmitting peer must not
        // make us burst).
        let mut last_triggered: HashMap<SocketAddr, Instant> = HashMap::new();

        // Planned rounds (Fase 3) probe kind groups in launch order with a
        // stagger; a flat (Fase 2) round is one group holding every target.
        let (groups, retry_budget, initial_delay) = match &cfg.plan {
            Some(plan) if !plan.groups.is_empty() => {
                (plan.groups.clone(), plan.retry_budget, plan.initial_delay)
            }
            Some(plan) => (vec![targets.clone()], plan.retry_budget, plan.initial_delay),
            None => (vec![targets.clone()], 0, Duration::ZERO),
        };
        // Probe order builds up as groups activate. Membership stays governed
        // by the sanitized flat list: a planned entry the sanitizer dropped is
        // never probed.
        let mut probe_order: Vec<SocketAddr> = Vec::new();
        let mut next_group = 0usize;

        let started_at = tokio::time::Instant::now();
        let hard_cap = started_at + CHECK_TOTAL_CAP;
        let mut deadline = (started_at + initial_delay + cfg.window).min(hard_cap);
        let mut pass: u32 = 0;
        let seed = check_jitter_seed(&cfg.key, cfg.role.byte());
        let mut probe_step: u64 = 0;
        let mut next_probe = started_at + initial_delay;
        let mut rr = 0usize;
        // Whether the round ended through the listener handoff (S-5) rather
        // than through our own request being answered.
        let mut handoff = false;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(next_probe) => {
                    // Activate every group whose stagger offset has passed.
                    while next_group < groups.len()
                        && tokio::time::Instant::now() >= started_at
                            + initial_delay
                            + CHECK_GROUP_STAGGER * next_group as u32
                    {
                        for addr in &groups[next_group] {
                            if targets.contains(addr) && !probe_order.contains(addr) {
                                probe_order.push(*addr);
                            }
                        }
                        next_group += 1;
                    }
                    // A partial plan (e.g. a cache-seeded head group) must
                    // never EXCLUDE candidates: once every group is live, any
                    // target the groups missed joins the tail of the order.
                    if next_group >= groups.len() && probe_order.len() < targets.len() {
                        for addr in &targets {
                            if !probe_order.contains(addr) {
                                probe_order.push(*addr);
                            }
                        }
                    }
                    // Paced with bounded deterministic jitter (breaks the
                    // conntrack-crossfire lockstep); retry passes back off by
                    // doubling the pace.
                    probe_step += 1;
                    let pace = CHECK_PACE * 2u32.saturating_pow(pass.min(4))
                        + check_jitter(seed, probe_step);
                    next_probe = tokio::time::Instant::now() + pace;
                    if probe_order.is_empty() {
                        continue;
                    }
                    let target = probe_order[rr % probe_order.len()];
                    rr += 1;
                    self.send_check_request(target, cfg).await;
                }
                Some(src) = inbound_req_rx.recv() => {
                    // Authenticated request: the peer can reach US from `src`.
                    if !targets.contains(&src)
                        && valid_candidate(&src)
                        && targets.len() < MAX_UDP_CANDIDATES
                    {
                        info!(
                            %src,
                            "learned peer-reflexive candidate from authenticated check request"
                        );
                        targets.push(src);
                        // A learned source outranks every waiting group: it is
                        // the one address the peer PROVABLY egresses from.
                        probe_order.insert(0, src);
                        learned_prflx = true;
                    }
                    // Triggered check toward the observed source (throttled):
                    // validates the reverse direction without waiting a pace slot.
                    let trigger = last_triggered
                        .get(&src)
                        .is_none_or(|t| t.elapsed() >= Duration::from_millis(200));
                    if trigger && targets.contains(&src) {
                        last_triggered.insert(src, Instant::now());
                        self.send_check_request(src, cfg).await;
                    }
                    // S-5, THE LISTENER HANDOFF. A listener that has just
                    // answered an authenticated request has everything its
                    // half of the round can produce: the peer holds the key,
                    // is on this generation, plays the other role, and reaches
                    // us from `src`. Everything after this point is the
                    // DIALER's move, and the dialer makes it the instant its
                    // own nomination completes — which our answer is what
                    // causes. Staying in the round past that answer does not
                    // improve the path; it only keeps this socket away from
                    // QUIC while the first Initial is already arriving, and a
                    // dropped Initial is not retried until the QUIC PTO.
                    //
                    // MEASURED on staging 2026-09-11 (vm-ws, 27 establishments,
                    // docs/performance/SECRET_STAGING_EVIDENCE_2026-09-11.md
                    // §4.1): the distribution of `direct_ready_ms` was
                    // BIMODAL — 18 runs in 37..53 ms and 9 runs in
                    // 1036..1162 ms, with nothing in between. Every slow run
                    // has a provider log reading `nominated=None
                    // checks_ms=1126` and a consumer that nominated at ~210 ms:
                    // the dialer nominated, tore its round down (late frames
                    // are never answered) and dialed, while the listener — whose
                    // adaptive plan probes the peer's LOCAL candidates first and
                    // only reaches the reflexive group a CHECK_GROUP_STAGGER
                    // later — was still probing an address that had already
                    // stopped answering. It ran to its full window, and the
                    // ~1 s constant is quinn's initial PTO (333 ms initial_rtt
                    // => ~999 ms), not anything on the network.
                    //
                    // `observed` stays None on this path ON PURPOSE: it can
                    // only be learned from a RESPONSE to our own request, and
                    // no caller consumes it. `nominated` is set because the
                    // round's consumers read it as "a pair is proven" — which
                    // gates the Fase 7 sprayed escape, and spending six seconds
                    // spraying for a peer that just reached us would be the
                    // same mistake in a larger size.
                    if cfg.role == CheckRole::Listener && valid_candidate(&src) {
                        nominated = Some(src);
                        handoff = true;
                        break;
                    }
                }
                Some((target, obs)) = validated_rx.recv() => {
                    nominated = Some(target);
                    observed = obs;
                    break;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    // Dry pass: the adaptive plan's retry budget grants
                    // another paced pass (backoff), inside the hard cap —
                    // a single bounded round, never an unbounded prober.
                    if pass < u32::from(retry_budget)
                        && tokio::time::Instant::now() < hard_cap
                    {
                        pass += 1;
                        deadline = (deadline + cfg.window / 2).min(hard_cap);
                        debug!(pass, "check round dry; adaptive retry pass (backoff pacing)");
                        continue;
                    }
                    break;
                }
            }
        }
        // Disable the round: late frames are counted, never answered.
        self.inner.checks.lock().unwrap().take();

        let checks_ms = started.elapsed().as_millis() as u64;
        info!(
            role = ?cfg.role,
            generation = cfg.generation,
            nominated = ?nominated,
            observed = ?observed,
            learned_prflx,
            checks_ms,
            invalid_checks = self.invalid_checks(),
            planned = cfg.plan.is_some(),
            retry_passes = pass,
            handoff,
            "connectivity-check round finished"
        );
        CheckOutcome {
            nominated,
            observed,
            learned_prflx,
            targets,
            checks_ms,
        }
    }

    /// Send one authenticated check request toward `target`, registering its
    /// transaction so only a response from exactly `target` can validate it.
    async fn send_check_request(&self, target: SocketAddr, cfg: &CheckConfig) {
        let txid = check::new_txid();
        let frame = check::request(&cfg.key, cfg.role.byte(), cfg.generation, &txid);
        if let Some(state) = self.inner.checks.lock().unwrap().as_mut() {
            state.pending.insert(txid, target);
        }
        let _ = self.socket.send_to(&frame, target).await;
    }

    /// One STUN binding transaction toward `target`: up to 3 tries of
    /// [`STUN_TIMEOUT`] each, mirroring the legacy serial probe's persistence.
    /// Safe to run concurrently with other transactions on the same socket.
    pub async fn stun_query(&self, target: SocketAddr) -> Result<SocketAddr> {
        self.stun_query_full(target).await.map(|r| r.mapped)
    }

    /// [`Self::stun_query`], keeping the raw answer's source address.
    async fn stun_query_full(&self, target: SocketAddr) -> Result<StunReply> {
        for _attempt in 0..3 {
            let (request, txid) = stun::binding_request();
            match self
                .stun_transaction(target, &request, txid, false, STUN_TIMEOUT)
                .await
            {
                Ok(Some(reply)) => return Ok(reply),
                Ok(None) => continue,
                Err(err) => return Err(err),
            }
        }
        bail!("no STUN response from {target}")
    }

    /// Run ONE STUN transaction through the recv actor. `Ok(None)` is a clean
    /// timeout (the caller decides whether to retry); an `Err` means the send
    /// failed or the actor is gone, neither of which retrying can fix.
    async fn stun_transaction(
        &self,
        target: SocketAddr,
        request: &[u8],
        txid: [u8; 12],
        any_port: bool,
        wait: Duration,
    ) -> Result<Option<StunReply>> {
        let (tx, rx) = oneshot::channel();
        self.inner.pending_stun.lock().unwrap().insert(
            txid,
            PendingStun {
                target,
                any_port,
                tx,
            },
        );
        if let Err(err) = self.socket.send_to(request, target).await {
            self.inner.pending_stun.lock().unwrap().remove(&txid);
            return Err(err).context("STUN send failed");
        }
        match timeout(wait, rx).await {
            Ok(Ok(reply)) => Ok(Some(reply)),
            Ok(Err(_actor_gone)) => bail!("traversal socket recv actor stopped"),
            Err(_) => {
                self.inner.pending_stun.lock().unwrap().remove(&txid);
                Ok(None)
            }
        }
    }

    /// Measure this socket's NAT FILTERING against an RFC 5780 server.
    ///
    /// The traversal-socket twin of [`probe_filtering`]; see that function for
    /// why the answer's SOURCE, and not its RESPONSE-ORIGIN attribute, is what
    /// classifies the result.
    pub async fn probe_filtering(&self, target: SocketAddr) -> FilterProbe {
        // Only a server that publishes OTHER-ADDRESS can be asked, and that
        // has to be learned from a full response, so this runs its own
        // baseline query rather than reusing the chain's.
        let (request, txid) = stun::binding_request();
        let mut supported = false;
        for _ in 0..FILTER_PROBE_ATTEMPTS {
            match self
                .stun_transaction(target, &request, txid, false, STUN_TIMEOUT)
                .await
            {
                Ok(Some(reply)) => {
                    supported = reply.other.is_some();
                    break;
                }
                Ok(None) => continue,
                Err(_) => return FilterProbe::Unsupported,
            }
        }
        if !supported {
            return FilterProbe::Unsupported;
        }
        let (probe, probe_txid) = stun::binding_request_change_port();
        for _ in 0..FILTER_PROBE_ATTEMPTS {
            match self
                .stun_transaction(target, &probe, probe_txid, true, FILTER_PROBE_TIMEOUT)
                .await
            {
                Ok(Some(reply)) if reply.from.port() != target.port() => {
                    return FilterProbe::AddressDependentOrOpen
                }
                // Answered from the ordinary port: the server ignored the
                // change request, which is "cannot ask", never "blocked".
                Ok(Some(_)) => return FilterProbe::Unsupported,
                Ok(None) => continue,
                Err(_) => return FilterProbe::Unsupported,
            }
        }
        FilterProbe::AddressAndPortDependent
    }

    /// Probe a whole STUN chain concurrently under ONE global budget
    /// ([`STUN_CHAIN_BUDGET`]), launches staggered in chain order
    /// ([`STUN_CHAIN_STAGGER`]) so the preferred target keeps a head start.
    /// First success wins; `None` when every target failed or the budget ran
    /// out. Replaces the serial chain whose worst case with N dead targets was
    /// N × 3 × [`STUN_TIMEOUT`].
    pub async fn discover_reflexive_chain(&self, targets: &[StunTarget]) -> Option<SelectedStun> {
        use futures_util::stream::{FuturesUnordered, StreamExt};
        if targets.is_empty() {
            return None;
        }
        let mut probes: FuturesUnordered<_> = targets
            .iter()
            .enumerate()
            .map(|(i, target)| {
                let target = target.clone();
                async move {
                    tokio::time::sleep(STUN_CHAIN_STAGGER * i as u32).await;
                    match self.stun_query(target.addr).await {
                        Ok(reflexive) => Some(SelectedStun {
                            requested: target.requested.clone(),
                            addr: target.addr,
                            source: target.source,
                            reflexive,
                        }),
                        Err(err) => {
                            warn!(
                                %err,
                                stun_server = %target.requested,
                                stun_addr = %target.addr,
                                stun_source = target.source.as_str(),
                                "STUN chain probe failed"
                            );
                            None
                        }
                    }
                }
            })
            .collect();
        timeout(STUN_CHAIN_BUDGET, async {
            while let Some(res) = probes.next().await {
                if res.is_some() {
                    return res;
                }
            }
            None
        })
        .await
        .unwrap_or_else(|_| {
            warn!(
                budget = ?STUN_CHAIN_BUDGET,
                targets = targets.len(),
                "STUN chain exhausted its global budget; no reflexive discovered"
            );
            None
        })
    }

    /// Probe the STUN chain like [`Self::discover_reflexive_chain`], but also
    /// derive a structured [`UdpNatProfile`] from a SECOND observation (plan
    /// Fase 3): identical mapped addresses from two different servers ⇒
    /// endpoint-independent mapping, different ⇒ symmetric.
    ///
    /// Latency contract: the FIRST success still wins the candidate slot
    /// (selection is unchanged), and the first two targets launch TOGETHER
    /// (no stagger between them) so the confirming observation typically
    /// trails the winner by only |RTT₂−RTT₁|; the extra wait for it is
    /// bounded by [`PROFILE_CONFIRM_WAIT`] and never extends the global
    /// [`STUN_CHAIN_BUDGET`]. With zero successes the profile still reports
    /// `observations: 0` (STUN dead — the policy reads that as blocked-ish,
    /// never as "no metadata").
    pub async fn discover_reflexive_profile(
        &self,
        targets: &[StunTarget],
    ) -> (Option<SelectedStun>, UdpNatProfile) {
        use futures_util::stream::{FuturesUnordered, StreamExt};
        let local_port = self.local_addr().map(|a| a.port()).unwrap_or(0);
        let mut profile = UdpNatProfile::default();
        if targets.is_empty() {
            return (None, profile);
        }
        let mut probes: FuturesUnordered<_> = targets
            .iter()
            .enumerate()
            .map(|(i, target)| {
                let target = target.clone();
                async move {
                    // First two together: the second answer IS the profile.
                    let slot = i.saturating_sub(1) as u32;
                    tokio::time::sleep(STUN_CHAIN_STAGGER * slot).await;
                    match self.stun_query_full(target.addr).await {
                        Ok(reply) => Some((
                            SelectedStun {
                                requested: target.requested.clone(),
                                addr: target.addr,
                                source: target.source,
                                reflexive: reply.mapped,
                            },
                            reply.other,
                        )),
                        Err(err) => {
                            warn!(
                                %err,
                                stun_server = %target.requested,
                                stun_addr = %target.addr,
                                stun_source = target.source.as_str(),
                                "STUN chain probe failed"
                            );
                            None
                        }
                    }
                }
            })
            .collect();
        let deadline = tokio::time::Instant::now() + STUN_CHAIN_BUDGET;
        let mut selected: Option<SelectedStun> = None;
        // RFC 5780 OTHER-ADDRESS of whichever server answered FIRST. It is the
        // fallback second observation (below) for a chain that has only one
        // reachable server.
        let mut first_other: Option<SocketAddr> = None;
        loop {
            let next = tokio::time::timeout_at(deadline, probes.next()).await;
            let (observation, other) = match next {
                Ok(Some(Some(obs))) => obs,
                Ok(Some(None)) => continue,
                // Chain exhausted or global budget spent.
                Ok(None) | Err(_) => break,
            };
            match &selected {
                None => {
                    profile.observations = 1;
                    profile.port_preserved =
                        (local_port != 0).then(|| observation.reflexive.port() == local_port);
                    first_other = other;
                    selected = Some(observation);
                    // Bounded wait for ONE confirming observation; never past
                    // the global budget, and only worth it if another target
                    // is still in flight.
                    if probes.is_empty() {
                        break;
                    }
                    let confirm_by = tokio::time::Instant::now() + PROFILE_CONFIRM_WAIT;
                    if confirm_by < deadline {
                        // Shrink the remaining window to the confirm bound by
                        // draining under a nested timeout below.
                        if let Ok(Some(second)) = tokio::time::timeout_at(confirm_by, async {
                            while let Some(res) = probes.next().await {
                                if let Some(obs) = res {
                                    return Some(obs);
                                }
                            }
                            None
                        })
                        .await
                        {
                            apply_second_observation(&mut profile, &selected, &second.0);
                        }
                    }
                    break;
                }
                Some(_) => unreachable!("loop exits after first observation"),
            }
        }
        if selected.is_none() {
            warn!(
                budget = ?STUN_CHAIN_BUDGET,
                targets = targets.len(),
                "STUN chain exhausted its global budget; no reflexive discovered"
            );
        }
        // FILTERING (plan Fase 6), measured against the server that just
        // answered. Running it here rather than in a separate pass is what
        // keeps it free: the mapping is already open, so the probe is two
        // datagrams on a socket that is about to be used anyway.
        //
        // It runs ONLY when a reflexive address was found. Without one there
        // is no mapping to measure, and a probe against an unreachable server
        // would time out and be read as a restrictive filter — the expensive
        // direction to be wrong in, because the plan steers toward the relay
        // on it.
        if let Some(sel) = &selected {
            let probe = self.probe_filtering(sel.addr).await;
            profile.filtering = probe.legacy();
            profile.filtering_probe = match probe {
                FilterProbe::AddressDependentOrOpen => {
                    Some(crate::shared::UdpFilterProbe::AddressDependentOrOpen)
                }
                FilterProbe::AddressAndPortDependent => {
                    Some(crate::shared::UdpFilterProbe::AddressAndPortDependent)
                }
                FilterProbe::Unsupported => None,
            };
        }
        // ORDERING, and it is load-bearing: this runs AFTER the filtering
        // probe above, never before. That probe asks the server to answer from
        // its ALTERNATE port and measures whether the answer gets through —
        // and the mapping fallback WRITES to that exact address, which opens
        // an address+port-dependent filter for it. Measured, not reasoned:
        // with the two swapped, `scripts/udp_nat_netns_test.sh`'s plain
        // `masquerade` router reported `adf-or-eif` instead of `apdf`, i.e.
        // the measurement described the probe's own footprint.
        // MAPPING, fallback path (RFC 5780 §4.3). Classifying the mapping
        // needs two observations from two different server addresses, and a
        // chain often has only one that answers: `--stun-server HOST:PORT`
        // builds a SINGLE-element chain by construction, and a private
        // deployment has no public STUN to fall back on. One observation ⇒
        // `mapping: Unknown` ⇒ every policy that depends on the mapping is
        // disabled, silently, on exactly the deployments that configured
        // their own server.
        //
        // The second address is already published and already trusted: the
        // server's own OTHER-ADDRESS, the same attribute the filtering probe
        // needs. One extra binding request to it, inside the chain's existing
        // budget, answers the question.
        //
        // Deliberately CONSERVATIVE, and this is the part that matters: the
        // alternate socket differs from the primary in PORT only, so a
        // different reflexive proves the mapping is at least port-dependent
        // (symmetric, in this model) while an IDENTICAL one does NOT prove
        // endpoint-independence — an address-dependent NAT would also answer
        // identically here and would still hand a peer at another IP a
        // different port. So this fallback may conclude `Symmetric` and may
        // never conclude `Eim`: being wrong toward "symmetric" costs a retry
        // budget, being wrong toward "endpoint-independent" costs a direct
        // path that was planned and cannot exist.
        if profile.observations < 2 {
            if let (Some(first), Some(other)) = (selected.as_ref(), first_other) {
                // A server bound to a wildcard address publishes its alternate
                // socket as `0.0.0.0:port`, which is honest (it does not know
                // its own public IP) and unreachable. The host is the one that
                // just answered, so repair the address rather than skip the
                // measurement.
                let other = if other.ip().is_unspecified() {
                    SocketAddr::new(first.addr.ip(), other.port())
                } else {
                    other
                };
                if other != first.addr && tokio::time::Instant::now() < deadline {
                    match tokio::time::timeout_at(deadline, self.stun_query(other)).await {
                        Ok(Ok(reflexive)) => {
                            if reflexive != first.reflexive {
                                profile.observations = 2;
                                profile.mapping = UdpNatMapping::Symmetric;
                                debug!(
                                    %other,
                                    first = %first.reflexive,
                                    second = %reflexive,
                                    "mapping classified symmetric from the server's OTHER-ADDRESS"
                                );
                            } else {
                                debug!(
                                    %other,
                                    "OTHER-ADDRESS observation matched the first; \
                                     mapping stays unknown (a same-IP probe cannot \
                                     prove endpoint-independence)"
                                );
                            }
                        }
                        Ok(Err(err)) => debug!(%err, %other, "OTHER-ADDRESS probe failed"),
                        Err(_) => debug!(%other, "OTHER-ADDRESS probe ran out of chain budget"),
                    }
                }
            }
        }
        info!(
            profile = %profile.summary(),
            selected = selected.as_ref().map(|s| s.requested.as_str()),
            "derived structured NAT self-profile from STUN chain"
        );
        (selected, profile)
    }

    /// Stop the recv actor, then release the raw socket for Quinn. The actor
    /// is awaited (not just aborted) so no concurrent reader can survive the
    /// handoff — one socket, one reader, always.
    pub async fn into_socket(self) -> Result<UdpSocket> {
        let UdpTraversalSocket {
            socket, mut actor, ..
        } = self;
        actor.0.abort();
        let _ = (&mut actor.0).await;
        drop(actor);
        std::sync::Arc::try_unwrap(socket)
            .map_err(|_| anyhow::anyhow!("traversal socket still shared at handoff"))
    }
}

/// Fold the confirming (second) STUN observation into the profile: two
/// DIFFERENT servers seeing the same mapped address proves endpoint-
/// independent mapping; different mapped addresses prove a symmetric NAT.
/// A duplicate answer from the same server proves nothing and is ignored.
fn apply_second_observation(
    profile: &mut UdpNatProfile,
    selected: &Option<SelectedStun>,
    second: &SelectedStun,
) {
    let Some(first) = selected else { return };
    if second.addr == first.addr {
        return;
    }
    profile.observations = 2;
    profile.mapping = if second.reflexive == first.reflexive {
        UdpNatMapping::Eim
    } else {
        UdpNatMapping::Symmetric
    };
}

/// Handle one check-shaped datagram: authenticate, then either record a
/// validated pair (response) or hand back the response bytes to send
/// (request). EVERY failure — bad HMAC, wrong generation, wrong role, unknown
/// txid, wrong source, no active round — is counted and produces NO reply:
/// an unauthenticated probe never gets a response (plan Fase 2 property).
fn handle_check_frame(inner: &TraversalInner, buf: &[u8], from: SocketAddr) -> CheckAction {
    let mut guard = inner.checks.lock().unwrap();
    let Some(state) = guard.as_mut() else {
        inner.invalid_checks.fetch_add(1, Ordering::Relaxed);
        return CheckAction::default();
    };
    let Some(frame) = check::parse(&state.key, buf) else {
        inner.invalid_checks.fetch_add(1, Ordering::Relaxed);
        return CheckAction::default();
    };
    // Frames must belong to THIS round and come from the OTHER role (a peer
    // of our own role is a reflection/misconfiguration, never a valid pair).
    if frame.generation != state.generation || frame.role == state.role {
        inner.invalid_checks.fetch_add(1, Ordering::Relaxed);
        return CheckAction::default();
    }
    match frame.kind {
        check::KIND_REQUEST => {
            // Peer-reflexive learning + triggered check happen in the driver,
            // and so does the listener's handoff (S-5) — which ENDS the round
            // and therefore stops this actor. The announcement is carried out
            // of here instead of being sent under the lock so the caller can
            // put the response on the wire FIRST; announcing first would race
            // the round's teardown against the one datagram the dialer is
            // waiting for.
            let announce = state.inbound_req_tx.clone();
            if state.responses_sent >= CHECK_MAX_RESPONSES {
                return CheckAction {
                    reply: None,
                    announce: Some((announce, from)),
                };
            }
            state.responses_sent += 1;
            CheckAction {
                reply: Some(check::response(
                    &state.key,
                    state.role,
                    state.generation,
                    &frame.txid,
                    from,
                )),
                announce: Some((announce, from)),
            }
        }
        check::KIND_RESPONSE => {
            // Only a response from EXACTLY the queried target validates the
            // transaction (txid + full ip:port source, like the STUN demux).
            match state.pending.get(&frame.txid) {
                Some(target) if *target == from => {
                    state.pending.remove(&frame.txid);
                    let _ = state.validated_tx.send((from, frame.observed));
                }
                _ => {
                    inner.invalid_checks.fetch_add(1, Ordering::Relaxed);
                }
            }
            CheckAction::default()
        }
        _ => {
            inner.invalid_checks.fetch_add(1, Ordering::Relaxed);
            CheckAction::default()
        }
    }
}

/// What the recv actor must do with one parsed check frame. Ordering is the
/// whole reason this type exists: the `reply` goes on the wire BEFORE
/// `announce` reaches the driver, because the driver may end the round on that
/// announcement (the listener handoff, S-5) and ending the round stops the
/// actor mid-flight.
#[derive(Default)]
struct CheckAction {
    /// Authenticated response bytes to send back to the requester.
    reply: Option<Vec<u8>>,
    /// Source of a valid inbound request, announced to the driver after the
    /// reply is sent.
    announce: Option<(tokio::sync::mpsc::UnboundedSender<SocketAddr>, SocketAddr)>,
}

/// The single recv owner: demux STUN responses to their transactions, count
/// everything else. Never dies on a recv error (ICMP port-unreachable from
/// punching a dead candidate surfaces as ECONNREFUSED on Linux).
async fn recv_actor(socket: std::sync::Arc<UdpSocket>, inner: std::sync::Arc<TraversalInner>) {
    let mut buf = vec![0u8; 2048];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(5)).await;
                continue;
            }
        };
        let Some(txid) = stun::response_txid(&buf[..n]) else {
            // Connectivity-check frame? (plan Fase 2). Anything else — peer
            // punches, QUIC Initials — is counted and left alone.
            if check::looks_like(&buf[..n]) {
                let action = handle_check_frame(&inner, &buf[..n], from);
                if let Some(reply) = action.reply {
                    // Response bytes are built under the lock, sent outside it.
                    let _ = socket.send_to(&reply, from).await;
                }
                if let Some((tx, src)) = action.announce {
                    // Strictly after the send: see `CheckAction`.
                    let _ = tx.send(src);
                }
                continue;
            }
            inner.peer_datagrams.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let waiter = {
            let mut pending = inner.pending_stun.lock().unwrap();
            match pending.get(&txid) {
                // Demux by txid AND full ip:port source: a response from
                // anyone but the queried server never resolves the
                // transaction (it stays pending for the real answer). The one
                // exception is a filtering probe, which ASKED to be answered
                // from another port of the same server.
                Some(p) if p.target == from => pending.remove(&txid),
                Some(p) if p.any_port && p.target.ip() == from.ip() => pending.remove(&txid),
                _ => None,
            }
        };
        let Some(waiter) = waiter else {
            inner.stray_stun.fetch_add(1, Ordering::Relaxed);
            debug!(%from, "stray STUN response (unknown txid or wrong source); ignoring");
            continue;
        };
        match stun::parse_response(&buf[..n], &txid) {
            Some(mapped) => {
                let other = stun::parse_other_address(&buf[..n]);
                let _ = waiter.tx.send(StunReply {
                    mapped,
                    from,
                    other,
                });
            }
            None => {
                inner.stray_stun.fetch_add(1, Ordering::Relaxed);
                debug!(%from, "malformed STUN response; transaction dropped");
            }
        }
    }
}

#[cfg(all(feature = "udp", windows))]
fn configure_udp_socket_buffers<S: std::os::windows::io::AsSocket>(
    socket: &S,
    tuning: &UdpDirectTuning,
) {
    let socket = socket2::SockRef::from(socket);
    if let Err(err) = socket.set_recv_buffer_size(tuning.udp_socket_recv_buffer) {
        debug!(%err, requested = tuning.udp_socket_recv_buffer, "failed to raise UDP receive buffer");
    }
    if let Err(err) = socket.set_send_buffer_size(tuning.udp_socket_send_buffer) {
        debug!(%err, requested = tuning.udp_socket_send_buffer, "failed to raise UDP send buffer");
    }

    debug!(
        requested_recv = tuning.udp_socket_recv_buffer,
        actual_recv = ?socket.recv_buffer_size().ok(),
        requested_send = tuning.udp_socket_send_buffer,
        actual_send = ?socket.send_buffer_size().ok(),
        "configured UDP socket buffers"
    );
}

#[cfg(all(feature = "udp", target_os = "linux"))]
fn configure_udp_socket_buffers<S: std::os::fd::AsFd>(socket: &S, tuning: &UdpDirectTuning) {
    // CRITICAL for direct-path throughput: the kernel silently clamps
    // SO_SNDBUF/SO_RCVBUF to net.core.{w,r}mem_max (Ubuntu/Debian default
    // 212992 = 208 KiB). A single congestion-controlled QUIC datagram flow is
    // then capped at ~buffer/RTT — e.g. 208 KiB / 20 ms ≈ 10 MB/s — no matter
    // how large a window Quinn negotiates and with the CPU near idle. bore VPN
    // runs with CAP_NET_ADMIN, so use SO_{SND,RCV}BUFFORCE (nix `*BufForce`)
    // which bypass the *mem_max ceiling entirely. Fall back to the clamped
    // setsockopt (socket2) when the cap is absent (EPERM), and verify the
    // result so a clamp that survives is logged LOUDLY (not at debug) with the
    // exact remediation.
    use nix::sys::socket::{getsockopt, setsockopt, sockopt};

    let fd = socket.as_fd();

    // Try the forced setters first; on EPERM (no CAP_NET_ADMIN) fall back to the
    // clamped path so an unprivileged build still gets the best the kernel allows.
    let recv_forced = setsockopt(&fd, sockopt::RcvBufForce, &tuning.udp_socket_recv_buffer).is_ok();
    if !recv_forced {
        let _ = setsockopt(&fd, sockopt::RcvBuf, &tuning.udp_socket_recv_buffer);
    }
    let send_forced = setsockopt(&fd, sockopt::SndBufForce, &tuning.udp_socket_send_buffer).is_ok();
    if !send_forced {
        let _ = setsockopt(&fd, sockopt::SndBuf, &tuning.udp_socket_send_buffer);
    }

    // getsockopt(SO_{SND,RCV}BUF) returns the kernel's internal value, which is
    // 2× the requested size on Linux (kernel doubles for bookkeeping). Compare
    // against the requested size to detect a surviving clamp.
    let actual_recv = getsockopt(&fd, sockopt::RcvBuf).unwrap_or(0);
    let actual_send = getsockopt(&fd, sockopt::SndBuf).unwrap_or(0);
    // A clamp leaves the effective buffer well under the request; the kernel
    // doubling means "healthy" is actual >= requested, so flag actual < requested.
    let recv_clamped = actual_recv < tuning.udp_socket_recv_buffer;
    let send_clamped = actual_send < tuning.udp_socket_send_buffer;

    if recv_clamped || send_clamped {
        tracing::warn!(
            requested_recv = tuning.udp_socket_recv_buffer,
            effective_recv = actual_recv,
            requested_send = tuning.udp_socket_send_buffer,
            effective_send = actual_send,
            recv_forced,
            send_forced,
            "UDP socket buffer clamped below request — direct-path throughput \
             will be limited to roughly buffer/RTT. Run with CAP_NET_ADMIN \
             (privileged) for SO_*BUFFORCE, or raise net.core.rmem_max and \
             net.core.wmem_max (e.g. sysctl -w net.core.rmem_max=16777216 \
             net.core.wmem_max=16777216)"
        );
    } else {
        info!(
            requested_recv = tuning.udp_socket_recv_buffer,
            effective_recv = actual_recv,
            requested_send = tuning.udp_socket_send_buffer,
            effective_send = actual_send,
            forced = recv_forced && send_forced,
            "configured UDP socket buffers"
        );
    }
}

#[cfg(all(feature = "udp", unix, not(target_os = "linux")))]
fn configure_udp_socket_buffers<S: std::os::fd::AsFd>(socket: &S, tuning: &UdpDirectTuning) {
    let socket = socket2::SockRef::from(socket);
    if let Err(err) = socket.set_recv_buffer_size(tuning.udp_socket_recv_buffer) {
        debug!(%err, requested = tuning.udp_socket_recv_buffer, "failed to raise UDP receive buffer");
    }
    if let Err(err) = socket.set_send_buffer_size(tuning.udp_socket_send_buffer) {
        debug!(%err, requested = tuning.udp_socket_send_buffer, "failed to raise UDP send buffer");
    }

    debug!(
        requested_recv = tuning.udp_socket_recv_buffer,
        actual_recv = ?socket.recv_buffer_size().ok(),
        requested_send = tuning.udp_socket_send_buffer,
        actual_send = ?socket.send_buffer_size().ok(),
        "configured UDP socket buffers"
    );
}

#[cfg(not(feature = "udp"))]
fn configure_udp_socket_buffers<S>(_socket: &S, _tuning: &UdpDirectTuning) {}

/// Where a STUN target came from. This is used only for logging/diagnostics: the
/// candidate addresses themselves remain plain `SocketAddr`s on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StunSource {
    /// User supplied `--stun-server` / `BORE_STUN_SERVER`.
    Override,
    /// Built-in public STUN default (Cloudflare/Google).
    PublicDefault,
    /// The bore server's own UDP control/STUN endpoint, used last.
    BoreFallback,
    /// STUN server selected by the peer and advertised by the rendezvous server.
    PeerHint,
    /// A single explicitly resolved target used by legacy/internal callers.
    Single,
}

impl StunSource {
    /// Stable lowercase label used in logs and human-readable diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            StunSource::Override => "override",
            StunSource::PublicDefault => "public-default",
            StunSource::BoreFallback => "bore-fallback",
            StunSource::PeerHint => "peer-hint",
            StunSource::Single => "single",
        }
    }
}

/// A resolved STUN endpoint plus the original host:port that produced it.
#[derive(Debug, Clone)]
pub struct StunTarget {
    /// The configured host:port, before DNS resolution.
    pub requested: String,
    /// The resolved UDP endpoint used for the binding request.
    pub addr: SocketAddr,
    /// Why this target is in the candidate chain.
    pub source: StunSource,
}

/// The STUN server that successfully produced this peer's reflexive address.
#[derive(Debug, Clone)]
pub struct SelectedStun {
    /// Configured host:port before DNS resolution.
    pub requested: String,
    /// Resolved UDP endpoint that answered the binding request.
    pub addr: SocketAddr,
    /// Why this STUN target was part of the chain.
    pub source: StunSource,
    /// Public reflexive address reported by the STUN server.
    pub reflexive: SocketAddr,
}

/// Candidate gathering result with enough metadata for useful operator logs.
#[derive(Debug, Clone)]
pub struct CandidateDiscovery {
    /// Candidate addresses to send over the bore control channel.
    pub candidates: Vec<SocketAddr>,
    /// Roles for the candidate list, in the same order as `candidates`.
    pub candidate_kinds: Vec<UdpCandidateKind>,
    /// Local UDP socket address used for discovery and punching.
    pub local_addr: Option<SocketAddr>,
    /// STUN server that produced the selected reflexive candidate, if any.
    pub selected_stun: Option<SelectedStun>,
    /// Number of resolved STUN targets attempted.
    pub attempted_stun: usize,
    /// Wall-clock time the whole gather took (STUN chain + UPnP + local), in
    /// milliseconds. Baseline traversal metric (`discovery_ms`).
    pub discovery_ms: u64,
    /// Structured NAT self-profile (plan Fase 3): only produced by the
    /// profiled traversal gather; `None` from the legacy serial gather so an
    /// old-path offer stays byte-identical.
    pub profile: Option<UdpNatProfile>,
    /// Managed port-mapping lease (plan Fase 5): present when `--upnp`
    /// acquired a PCP/UPnP mapping. Arc'd so holders that outlive this
    /// discovery (secret provider) can keep the renewal alive; dropping the
    /// last clone releases the mapping best-effort.
    #[cfg(feature = "udp")]
    pub lease: Option<std::sync::Arc<crate::portmap::LeaseHandle>>,
}

impl CandidateDiscovery {
    /// Build the wire offer for this discovery: the legacy plain-address list
    /// (still the source of truth for old peers) plus the typed candidate
    /// model v2 alongside (same addresses; observe-only in Fase 1).
    pub fn to_offer(&self, peer_id: u32, generation: u32) -> UdpCandidateOffer {
        UdpCandidateOffer {
            candidates: self.candidates.clone(),
            selected_stun: self.selected_stun.as_ref().map(|s| s.requested.clone()),
            peer_id,
            typed_candidates: self
                .candidates
                .iter()
                .zip(self.candidate_kinds.iter())
                .enumerate()
                .map(|(index, (addr, kind))| UdpTypedCandidate {
                    addr: *addr,
                    kind: *kind,
                    priority: candidate_priority(*kind, index),
                })
                .collect(),
            generation,
            capabilities: vec![
                UDP_CAP_CANDIDATE_V2.to_string(),
                UDP_CAP_CHECK_V1.to_string(),
                crate::shared::UDP_CAP_SPRAY_V1.to_string(),
            ],
            profile_hint: None,
            profile: self.profile,
        }
    }
}

/// Advisory candidate priority (higher = try earlier). The order becomes
/// data-driven in Fase 3; until then it mirrors the ICE-ish default: local
/// (same-LAN) first, then reflexive, router-mapped, predicted last. The index
/// keeps within-kind ordering stable.
pub fn candidate_priority(kind: UdpCandidateKind, index: usize) -> u32 {
    let base: u32 = match kind {
        UdpCandidateKind::Local => 900,
        UdpCandidateKind::Reflexive => 800,
        UdpCandidateKind::RouterMapped => 700,
        UdpCandidateKind::Predicted => 400,
    };
    base.saturating_sub(index as u32)
}

/// Host:port of the bore server's own STUN responder for a control endpoint.
/// `https://`/`http://` endpoints may front TCP on 443/80, while the STUN
/// responder still lives on bore's control UDP port.
pub fn bore_stun_target(host: &str, port: u16) -> String {
    let stun_port = if port == 443 || port == 80 {
        CONTROL_PORT
    } else {
        port
    };
    format!("{host}:{stun_port}")
}

fn live_stun_target_specs(
    host: &str,
    port: u16,
    override_server: Option<&str>,
) -> Vec<(String, StunSource)> {
    live_stun_target_specs_with_hint(host, port, override_server, None)
}

fn push_unique_stun_target(
    targets: &mut Vec<(String, StunSource)>,
    target: String,
    source: StunSource,
) {
    if !targets.iter().any(|(existing, _)| existing == &target) {
        targets.push((target, source));
    }
}

fn live_stun_target_specs_with_hint(
    host: &str,
    port: u16,
    override_server: Option<&str>,
    peer_hint: Option<&str>,
) -> Vec<(String, StunSource)> {
    if let Some(server) = override_server {
        return vec![(server.to_string(), StunSource::Override)];
    }

    let mut targets = Vec::new();
    if let Some(peer_hint) = peer_hint.filter(|hint| !hint.is_empty()) {
        push_unique_stun_target(&mut targets, peer_hint.to_string(), StunSource::PeerHint);
    }
    for server in PUBLIC_STUN {
        push_unique_stun_target(
            &mut targets,
            (*server).to_string(),
            StunSource::PublicDefault,
        );
    }
    push_unique_stun_target(
        &mut targets,
        bore_stun_target(host, port),
        StunSource::BoreFallback,
    );
    targets
}

/// The live tunnel STUN chain before DNS resolution. Useful for logs/help/tests.
pub fn live_stun_target_names(host: &str, port: u16, override_server: Option<&str>) -> Vec<String> {
    live_stun_target_specs(host, port, override_server)
        .into_iter()
        .map(|(target, _)| target)
        .collect()
}

/// The live STUN chain with an optional peer-selected STUN server tried first.
/// An explicit local override still wins and disables both defaults and hints.
pub fn live_stun_target_names_with_hint(
    host: &str,
    port: u16,
    override_server: Option<&str>,
    peer_hint: Option<&str>,
) -> Vec<String> {
    live_stun_target_specs_with_hint(host, port, override_server, peer_hint)
        .into_iter()
        .map(|(target, _)| target)
        .collect()
}

async fn resolve_stun_target(target: &str) -> Result<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = tokio::net::lookup_host(target)
        .await
        .with_context(|| format!("failed to resolve STUN server {target}"))?
        .collect();
    addrs
        .iter()
        .copied()
        .find(|addr| addr.is_ipv4())
        .or_else(|| addrs.pop())
        .with_context(|| format!("no addresses for STUN server {target}"))
}

/// Resolve the live tunnel STUN chain. With an explicit override the chain has a
/// single element. Without one, public STUN on common ports is tried first and
/// the bore server's own STUN endpoint is kept as the final fallback.
pub async fn resolve_live_stun_targets(
    host: &str,
    port: u16,
    override_server: Option<&str>,
) -> Result<Vec<StunTarget>> {
    resolve_live_stun_targets_with_hint(host, port, override_server, None).await
}

/// Resolve the live tunnel STUN chain, optionally trying the peer-selected STUN
/// first. If the hinted STUN is unreachable, candidate gathering continues with
/// the remaining public/default and bore-server fallback targets.
pub async fn resolve_live_stun_targets_with_hint(
    host: &str,
    port: u16,
    override_server: Option<&str>,
    peer_hint: Option<&str>,
) -> Result<Vec<StunTarget>> {
    let mut targets = Vec::new();
    for (requested, source) in
        live_stun_target_specs_with_hint(host, port, override_server, peer_hint)
    {
        match resolve_stun_target(&requested).await {
            Ok(addr) => {
                debug!(
                    stun_server = %requested,
                    %addr,
                    stun_source = source.as_str(),
                    "resolved STUN server"
                );
                targets.push(StunTarget {
                    requested,
                    addr,
                    source,
                });
            }
            Err(err) => warn!(
                %err,
                stun_server = %requested,
                stun_source = source.as_str(),
                "failed to resolve STUN server; trying next candidate"
            ),
        }
    }
    if targets.is_empty() {
        bail!("no STUN servers could be resolved")
    }
    Ok(targets)
}

/// Options for one candidate gather (plan Fase 5). Replaces the growing list
/// of bool parameters on the traversal gather; the legacy serial gather keeps
/// its old two-bool signature and builds one of these internally.
#[derive(Debug, Clone, Default)]
pub struct GatherOptions {
    /// Ask the router for an explicit port mapping (`--upnp` opt-in; tries
    /// PCP first, then UPnP-IGD — plan Fase 5 managed leases).
    pub port_map: bool,
    /// Advertise predicted symmetric-NAT ports (`--try-port-prediction`).
    pub port_prediction: bool,
    /// Operator-declared public endpoints (`--udp-candidate`, repeatable):
    /// advertised FIRST, as `RouterMapped` typed candidates (an explicit
    /// static port-forward IS a router mapping; reusing the existing kind
    /// keeps the wire enum backward-compatible with old peers).
    pub manual_candidates: Vec<SocketAddr>,
    /// Skip the STUN chain entirely (`--udp-no-stun`): manual + local (+
    /// port-mapped) candidates only. With no manual candidate this most
    /// likely ends on the relay — logged loudly, never silent.
    pub no_stun: bool,
}

impl GatherOptions {
    /// The legacy two-bool shape used by the serial gather and older callers.
    pub fn from_flags(port_map: bool, port_prediction: bool) -> Self {
        Self {
            port_map,
            port_prediction,
            ..Default::default()
        }
    }
}

/// Gather this peer's candidate addresses: the STUN-discovered reflexive address
/// (for traversal across NATs) plus the primary local address (for same-LAN
/// peers). Optionally adds a router-mapped candidate (`port_map`, UPnP-IGD) and
/// predicted symmetric-NAT ports (`port_prediction`). Best-effort: an empty list
/// means no usable candidate was found.
pub async fn gather_candidates(
    socket: &UdpSocket,
    stun: SocketAddr,
    port_map: bool,
    port_prediction: bool,
) -> Vec<SocketAddr> {
    let target = StunTarget {
        requested: stun.to_string(),
        addr: stun,
        source: StunSource::Single,
    };
    gather_candidates_from_stun_targets(socket, &[target], port_map, port_prediction)
        .await
        .candidates
}

/// Gather this peer's candidate addresses using a fallback chain of STUN
/// targets. The first STUN server that returns a reflexive address is selected;
/// later servers are skipped to keep live tunnel setup fast. The local candidate
/// is still added even if every STUN probe fails, so same-LAN peers can connect
/// and all other cases fall back to the relay cleanly.
pub async fn gather_candidates_from_stun_targets(
    socket: &UdpSocket,
    stun_targets: &[StunTarget],
    port_map: bool,
    port_prediction: bool,
) -> CandidateDiscovery {
    let started = Instant::now();
    let local_addr = socket.local_addr().ok();
    let mut selected_stun = None;

    info!(
        udp_local_addr = ?local_addr,
        requested_stun = stun_targets.len(),
        "starting UDP candidate discovery"
    );

    for target in stun_targets {
        debug!(
            stun_server = %target.requested,
            stun_addr = %target.addr,
            stun_source = target.source.as_str(),
            "probing STUN server for UDP candidates"
        );
        match discover_reflexive(socket, target.addr).await {
            Ok(addr) => {
                selected_stun = Some(SelectedStun {
                    requested: target.requested.clone(),
                    addr: target.addr,
                    source: target.source,
                    reflexive: addr,
                });
                break;
            }
            Err(err) => warn!(
                %err,
                stun_server = %target.requested,
                stun_addr = %target.addr,
                stun_source = target.source.as_str(),
                "STUN reflexive discovery failed; trying next STUN server"
            ),
        }
    }

    finish_discovery(
        local_addr,
        selected_stun,
        stun_targets.len(),
        &GatherOptions::from_flags(port_map, port_prediction),
        started,
    )
    .await
}

/// Run candidate discovery on a [`UdpTraversalSocket`]: same candidate
/// assembly as the legacy serial gather, but the STUN chain is probed
/// concurrently under ONE global budget ([`STUN_CHAIN_BUDGET`]) thanks to the
/// socket's single-owner transaction demux (plan Fase 1). Call
/// [`UdpTraversalSocket::into_socket`] afterwards to hand the socket to the
/// punch/QUIC stage.
///
/// With `opts.no_stun` the chain is skipped entirely (plan Fase 5 manual
/// mode): the profile reports zero observations and the offer carries the
/// manual/local/port-mapped candidates only.
pub async fn gather_candidates_traversal(
    tsock: &UdpTraversalSocket,
    stun_targets: &[StunTarget],
    opts: &GatherOptions,
) -> CandidateDiscovery {
    let started = Instant::now();
    let local_addr = tsock.local_addr();
    info!(
        udp_local_addr = ?local_addr,
        requested_stun = stun_targets.len(),
        budget = ?STUN_CHAIN_BUDGET,
        no_stun = opts.no_stun,
        manual = opts.manual_candidates.len(),
        "starting UDP candidate discovery (budgeted traversal chain)"
    );
    let (selected_stun, profile) = if opts.no_stun {
        info!("STUN chain skipped (--udp-no-stun); using manual/local candidates only");
        (None, UdpNatProfile::default())
    } else {
        tsock.discover_reflexive_profile(stun_targets).await
    };
    let mut discovery = finish_discovery(
        local_addr,
        selected_stun,
        if opts.no_stun { 0 } else { stun_targets.len() },
        opts,
        started,
    )
    .await;
    discovery.profile = Some(profile);
    discovery
}

/// Shared tail of candidate discovery: assemble reflexive + predicted +
/// router-mapped + local candidates around the (possibly absent) selected
/// STUN result, then validate/dedup/cap the list (I-11). One implementation
/// so the serial (legacy) and budgeted (traversal) paths can never drift.
async fn finish_discovery(
    local_addr: Option<SocketAddr>,
    selected_stun: Option<SelectedStun>,
    attempted_stun: usize,
    opts: &GatherOptions,
    started: Instant,
) -> CandidateDiscovery {
    let port_map = opts.port_map;
    let port_prediction = opts.port_prediction;
    let local_port = local_addr.map(|a| a.port()).unwrap_or(0);
    let mut candidates = Vec::new();
    let mut candidate_kinds = Vec::new();

    // Operator-declared endpoints go FIRST (plan Fase 5): the operator knows
    // the real public mapping better than any discovery. `RouterMapped` on
    // the wire — an explicit static port-forward IS a router mapping, and
    // reusing the existing kind keeps old peers parsing the offer.
    for addr in &opts.manual_candidates {
        if !candidates.contains(addr) {
            info!(%addr, "advertising manual UDP candidate (--udp-candidate)");
            candidates.push(*addr);
            candidate_kinds.push(UdpCandidateKind::RouterMapped);
        }
    }

    if let Some(selected) = &selected_stun {
        let addr = selected.reflexive;
        info!(
            stun_server = %selected.requested,
            stun_addr = %selected.addr,
            stun_source = selected.source.as_str(),
            reflexive = %addr,
            udp_local_addr = ?local_addr,
            "selected STUN server for UDP candidates"
        );
        candidates.push(addr);
        candidate_kinds.push(UdpCandidateKind::Reflexive);

        // Symmetric NATs allocate a *different* external port per
        // destination, so the port toward the peer differs from the one seen
        // by STUN — often sequentially. When explicitly enabled, advertise a
        // few ports just past the reflexive one as extra candidates. Strictly
        // opt-in: advertising/punching extra ports may look like a scan to
        // strict firewalls.
        if port_prediction {
            let base = addr.port();
            let mut added = 0u16;
            for delta in 1..=PREDICT_RANGE {
                if let Some(port) = base.checked_add(delta) {
                    candidates.push(SocketAddr::new(addr.ip(), port));
                    candidate_kinds.push(UdpCandidateKind::Predicted);
                    added += 1;
                }
            }
            warn!(
                reflexive_port = base,
                predicted = added,
                "port prediction ENABLED — advertising predicted symmetric-NAT ports \
                 (best-effort; may look like a scan to strict firewalls)"
            );
        }
    } else if opts.no_stun {
        if opts.manual_candidates.is_empty() {
            warn!(
                "--udp-no-stun with NO --udp-candidate: only local/port-mapped candidates \
                 will be offered — across NAT/firewalls this will almost certainly fall \
                 back to the relay. Declare your public endpoint with --udp-candidate"
            );
        }
    } else {
        warn!(
            attempted = attempted_stun,
            "all STUN probes failed — no public address discovered; offering only non-STUN \
             candidates. Direct UDP is unlikely across NAT/firewalls and will fall back to \
             the relay if the peer cannot reach them"
        );
    }

    // Router-mapped candidate via a MANAGED lease (plan Fase 5): PCP first,
    // UPnP-IGD fallback. The returned handle keeps the mapping renewed and
    // releases it best-effort on drop; callers that outlive the discovery
    // (secret provider) hold it for the tunnel's life and re-offer when the
    // external endpoint changes.
    #[cfg(feature = "udp")]
    let mut lease = None;
    #[cfg(feature = "udp")]
    if port_map {
        match crate::portmap::acquire_lease(local_port).await {
            Some(handle) => {
                let addr = handle.external;
                warn!(
                    %addr,
                    backend = handle.backend,
                    "managed port mapping ENABLED — added router-mapped candidate (lease renewed automatically)"
                );
                if !candidates.contains(&addr) {
                    candidates.push(addr);
                    candidate_kinds.push(UdpCandidateKind::RouterMapped);
                }
                lease = Some(std::sync::Arc::new(handle));
            }
            None => debug!("no managed port mapping (PCP + UPnP both unavailable); skipping"),
        }
    }
    #[cfg(not(feature = "udp"))]
    let _ = port_map;

    // A local candidate lets two peers behind the same NAT connect directly.
    if let Some(ip) = primary_local_ip() {
        let local = SocketAddr::new(ip, local_port);
        if !candidates.contains(&local) {
            candidates.push(local);
            candidate_kinds.push(UdpCandidateKind::Local);
        }
    }
    // Validate + dedup + cap what we are about to put on the wire, so a local
    // gather bug can never leak an unusable list to the peer (I-11).
    let san = sanitize_discovery(&mut candidates, &mut candidate_kinds);
    log_dropped_candidates("gather", candidates.len(), &san);
    let discovery_ms = started.elapsed().as_millis() as u64;
    info!(
        udp_local_addr = ?local_addr,
        selected_stun = selected_stun.as_ref().map(|s| s.requested.as_str()),
        candidates = ?candidates,
        discovery_ms,
        "finished UDP candidate discovery"
    );
    CandidateDiscovery {
        candidates,
        candidate_kinds,
        local_addr,
        selected_stun,
        attempted_stun,
        discovery_ms,
        profile: None,
        #[cfg(feature = "udp")]
        lease,
    }
}

/// Resolve the STUN server address: the explicit override (`host:port`), or the
/// control endpoint's host and port (a self-hosted bore server with `--udp`
/// doubles as the STUN server).
///
/// The STUN responder binds the server's control port (UDP). When the control
/// endpoint uses a TLS/HTTP default port (`https://` → 443, `http://` → 80),
/// that port fronts the control connection but is *not* where STUN listens, so
/// the default STUN target falls back to the well-known [`CONTROL_PORT`]. Pass
/// an explicit `--stun-server` for any non-standard deployment.
pub async fn resolve_stun(
    host: &str,
    port: u16,
    override_server: Option<&str>,
) -> Result<SocketAddr> {
    let target = match override_server {
        Some(server) => server.to_string(),
        None => bore_stun_target(host, port),
    };
    resolve_stun_target(&target).await
}

/// Determine the primary local IPv4 address by inspecting the kernel's chosen
/// source address for an outbound (unconnected, never-sent) socket.
/// Determine this host's primary local IPv4 address for diagnostic reports and
/// same-LAN UDP candidates.
pub fn primary_local_ip() -> Option<IpAddr> {
    let probe = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // No packets are sent; `connect` only sets the default peer so the kernel
    // resolves a route and assigns a source address we can read back.
    probe.connect((Ipv4Addr::new(8, 8, 8, 8), 53)).ok()?;
    match probe.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(IpAddr::V4(ip)),
        _ => None,
    }
}

/// Quick STUN probe: bind PORT, discover reflexive via a single STUN server,
/// return Some(true) if the NAT preserved the port, Some(false) if remapped,
/// None if the STUN probe itself failed. The socket is closed on return.
pub async fn check_reflexive_port(port: u16, stun_addr: SocketAddr) -> Option<bool> {
    let socket = bind_socket(port).await.ok()?;
    match discover_reflexive(&socket, stun_addr).await {
        Ok(addr) => Some(addr.port() == port),
        Err(_) => None,
    }
}

/// Send a STUN binding request and parse the reflexive address from the reply.
pub async fn discover_reflexive(socket: &UdpSocket, stun: SocketAddr) -> Result<SocketAddr> {
    let (request, txid) = stun::binding_request();
    let mut buf = [0u8; 512];
    for attempt in 0..3 {
        socket.send_to(&request, stun).await?;
        match timeout(STUN_TIMEOUT, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) if from.ip() == stun.ip() => {
                if let Some(addr) = stun::parse_response(&buf[..n], &txid) {
                    return Ok(addr);
                }
                debug!(%stun, attempt, "STUN response has mismatched txid; retrying");
            }
            Ok(Ok((_n, from))) => {
                debug!(%stun, %from, attempt, "STUN response from unexpected source; retrying");
                continue;
            }
            Ok(Err(err)) => {
                return Err(err).context(format!("STUN recv failed (attempt {attempt})"))
            }
            Err(_) => {
                if attempt < 2 {
                    debug!(%stun, retry = attempt + 1, "STUN request timed out, retrying");
                }
                continue;
            }
        }
    }
    warn!(%stun, "no STUN response after 3 attempts");
    bail!("no STUN response from {stun}")
}

/// Public STUN servers (distinct providers) used first by live UDP candidate
/// discovery (unless `--stun-server` overrides it) and probed by `bore test-udp`
/// to classify local NAT mapping behaviour. Cloudflare uses the standard STUN
/// port (3478), which commonly passes firewall policy that blocks bore's control
/// UDP port; Google adds provider diversity and fallback coverage.
pub const PUBLIC_STUN: &[&str] = &[
    "stun.cloudflare.com:3478",
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
];

/// One STUN server's view of our public (reflexive) mapping, gathered on a single
/// shared socket so the *variation* across servers reveals the NAT's mapping
/// behaviour.
#[derive(Debug, Clone)]
pub struct StunObservation {
    /// The STUN server queried (host:port).
    pub server: String,
    /// The reflexive address that server reported for our socket.
    pub reflexive: SocketAddr,
}

/// NAT mapping behaviour classified from multiple [`StunObservation`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NatClass {
    /// No STUN server answered — UDP is most likely blocked outbound.
    Blocked,
    /// A reflexive address equals a local address: a public IP, no NAT.
    Open,
    /// Only one server answered: egress works but mapping can't be classified.
    Inconclusive,
    /// Same public `ip:port` toward every server: endpoint-independent mapping
    /// (full/restricted cone). Hole-punching works.
    Cone,
    /// Public port varies per destination: endpoint-dependent mapping (symmetric
    /// NAT). `sequential` is true when the observed ports increase in small,
    /// regular steps (so `--try-port-prediction` has a chance).
    Symmetric {
        /// Whether the per-destination ports look sequentially allocated.
        sequential: bool,
    },
}

/// Whether an IPv4 address is in the carrier-grade NAT range `100.64.0.0/10`.
fn is_cgnat(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xc0) == 0x40
        }
        IpAddr::V6(_) => false,
    }
}

/// Whether an address is non-routable on the public internet (RFC1918, loopback,
/// link-local, or CGNAT) — a "public" reflexive in this range means another NAT
/// sits upstream.
fn is_non_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local() || is_cgnat(ip),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

/// Short parenthetical tag describing an address's routability, for the report.
fn routability_note(ip: IpAddr) -> &'static str {
    if is_cgnat(ip) {
        " (CGNAT 100.64/10)"
    } else if is_non_routable(ip) {
        " (private)"
    } else {
        ""
    }
}

/// Classify NAT mapping behaviour from STUN observations taken on one socket.
/// `local_ips` are this host's own addresses (to detect a public IP with no NAT).
pub fn classify_nat(local_ips: &[IpAddr], obs: &[StunObservation]) -> NatClass {
    if obs.is_empty() {
        return NatClass::Blocked;
    }
    if obs.iter().any(|o| local_ips.contains(&o.reflexive.ip())) {
        return NatClass::Open;
    }
    if obs.len() == 1 {
        return NatClass::Inconclusive;
    }
    let ports: BTreeSet<u16> = obs.iter().map(|o| o.reflexive.port()).collect();
    let ips: BTreeSet<IpAddr> = obs.iter().map(|o| o.reflexive.ip()).collect();
    if ports.len() == 1 && ips.len() == 1 {
        return NatClass::Cone;
    }
    let sorted: Vec<u16> = ports.into_iter().collect();
    let sequential = sorted
        .windows(2)
        .all(|w| (1..=8).contains(&w[1].saturating_sub(w[0])));
    NatClass::Symmetric { sequential }
}

/// Resolve `host:port` and run one STUN reflexive probe on `socket`.
async fn probe_one(socket: &UdpSocket, hostport: &str) -> Result<SocketAddr> {
    let addr = tokio::net::lookup_host(hostport)
        .await
        .with_context(|| format!("resolve {hostport}"))?
        .next()
        .with_context(|| format!("no addresses for {hostport}"))?;
    discover_reflexive(socket, addr).await
}

/// Query the local UPnP-IGD gateway for its external (WAN) IP, without creating a
/// mapping — a diagnostic probe for whether `--upnp` can do anything here.
#[cfg(feature = "udp")]
async fn upnp_external_ip() -> Result<IpAddr> {
    use igd_next::aio::tokio as igd;
    use igd_next::SearchOptions;
    let options = SearchOptions {
        timeout: Some(Duration::from_secs(2)),
        ..Default::default()
    };
    let gateway = igd::search_gateway(options)
        .await
        .context("no UPnP-IGD gateway found")?;
    gateway
        .get_external_ip()
        .await
        .context("UPnP-IGD external IP query failed")
}

/// Probe this host's UDP / NAT / firewall situation for hole-punching and print a
/// human-readable report with actionable advice. Opens no tunnel; reachable via
/// `bore test-udp`.
///
/// `bore_target` is the `--to` server's `(host, port)` — when given, the bore
/// server's own STUN responder is probed too (testing reachability of *your*
/// deployment's UDP). `stun_override` is an extra `--stun-server host:port`.
/// `preferred_port` mirrors `--nat-udp-preferred-port`: when non-zero the probe
/// binds that exact UDP port, so you can test whether the port you intend to open
/// in a firewall actually works (0 = a random ephemeral port).
pub async fn diagnose(
    bore_target: Option<(String, u16)>,
    stun_override: Option<&str>,
    preferred_port: u16,
) -> Result<()> {
    println!("bore UDP / NAT diagnostic");
    println!("=========================");

    // 1. Socket + local address.
    let socket = bind_socket(preferred_port).await?;
    let local_port = socket.local_addr()?.port();
    let local_ip = primary_local_ip();
    let port_kind = if preferred_port == 0 {
        "ephemeral"
    } else {
        "fixed (--nat-udp-preferred-port)"
    };
    println!();
    println!("Local UDP socket : 0.0.0.0:{local_port} ({port_kind})");
    match local_ip {
        Some(ip) => println!("Primary local IP : {ip}{}", routability_note(ip)),
        None => println!("Primary local IP : <none found>"),
    }

    // 2. Probe public STUN servers on the SAME socket — the variation across
    //    servers is what reveals cone vs symmetric mapping.
    println!();
    println!("STUN probes (a public IP here means UDP egress works):");
    let mut public_obs: Vec<StunObservation> = Vec::new();
    for server in PUBLIC_STUN {
        match probe_one(&socket, server).await {
            Ok(refl) => {
                println!("  [ ok ] {server:<26} -> {refl}");
                public_obs.push(StunObservation {
                    server: (*server).to_string(),
                    reflexive: refl,
                });
            }
            Err(err) => println!("  [FAIL] {server:<26} -> {err}"),
        }
    }
    // The RESOLVED address of every server that answered is kept: the
    // filtering probe below needs to send to the same server again, and
    // re-resolving could land on a different address of a round-robin name,
    // which would silently measure a different NAT path than the one just
    // classified.
    let mut filter_targets: Vec<SocketAddr> = Vec::new();
    if let Some(server) = stun_override {
        match tokio::net::lookup_host(server)
            .await
            .ok()
            .and_then(|mut it| it.next())
        {
            Some(addr) => match discover_reflexive(&socket, addr).await {
                Ok(refl) => {
                    println!("  [ ok ] {server:<26} -> {refl}  (--stun-server)");
                    filter_targets.push(addr);
                }
                Err(err) => println!("  [FAIL] {server:<26} -> {err}  (--stun-server)"),
            },
            None => println!("  [FAIL] {server:<26} -> resolve failed  (--stun-server)"),
        }
    }

    // 3. Probe the bore server's own STUN responder, if --to was given.
    let mut bore_reachable: Option<bool> = None;
    if let Some((host, port)) = bore_target.as_ref() {
        match resolve_stun(host, *port, None).await {
            Ok(addr) => match discover_reflexive(&socket, addr).await {
                Ok(refl) => {
                    println!("  [ ok ] bore server {addr:<20} -> {refl}  (your --to)");
                    bore_reachable = Some(true);
                    // First in the list: it is the deployment the tunnel will
                    // actually use, and the only kind of server that still
                    // implements RFC 5780 behaviour discovery in practice.
                    filter_targets.retain(|t| *t != addr);
                    filter_targets.insert(0, addr);
                }
                Err(err) => {
                    println!("  [FAIL] bore server {addr:<20} -> {err}  (your --to)");
                    bore_reachable = Some(false);
                }
            },
            Err(err) => println!("  [FAIL] bore server resolve -> {err}  (your --to)"),
        }
    }

    // 4. Classify and report a verdict.
    let local_ips: Vec<IpAddr> = local_ip.into_iter().collect();
    let class = classify_nat(&local_ips, &public_obs);
    println!();
    println!("Verdict");
    println!("-------");
    match &class {
        NatClass::Blocked => {
            println!("UDP appears BLOCKED outbound: no public STUN server answered.");
            println!("  -> Direct UDP hole-punching is impossible from this host.");
            println!("  -> Tunnels still work over the TCP relay (--udp simply has no effect).");
            println!("  Fix: allow outbound UDP, or run from a network that permits it.");
        }
        NatClass::Open => {
            println!("PUBLIC IP / no NAT: this socket is directly reachable.");
            println!("  -> Hole-punching trivially works; an ideal provider.");
        }
        NatClass::Inconclusive => {
            println!("UDP egress WORKS but only one server answered — cannot classify the");
            println!("  NAT mapping (need >=2 distinct STUN servers). Re-run to retry.");
        }
        NatClass::Cone => {
            println!("CONE NAT (endpoint-independent mapping): same public port to every server.");
            println!("  -> Hole-punching WORKS from your side. If the direct path still fails,");
            println!("     the *peer* is the blocker (symmetric/CGNAT/UDP-blocked on their end).");
        }
        NatClass::Symmetric { sequential } => {
            println!(
                "SYMMETRIC NAT (endpoint-dependent mapping): public port changes per destination."
            );
            if *sequential {
                println!(
                    "  Ports look SEQUENTIAL -> --try-port-prediction has a chance (best-effort)."
                );
            } else {
                println!("  Ports look RANDOM -> port prediction is unlikely to help.");
            }
            println!(
                "  -> Direct path works only if the *other* peer is cone/open. Symmetric+symmetric"
            );
            println!("     or symmetric+CGNAT cannot punch and falls back to the relay.");
        }
    }

    // 4b. FILTERING (RFC 4787 §5, measured per RFC 5780 §4.4). The class above
    //     is the MAPPING axis, and a real-kernel matrix
    //     (`scripts/udp_nat_netns_test.sh`) shows it is not the axis that
    //     decides a punch against a symmetric peer: `eim:adf` reaches one and
    //     `eim:apdf` does not, with identical mapping and identical verdict
    //     text above. Reporting only the mapping is what §13 of
    //     `docs/nat/NAT_TRAVERSAL.md` listed as a known gap.
    //
    //     The probe costs at most two extra STUN exchanges against a server
    //     that already answered, and it is skipped entirely when no server
    //     published an OTHER-ADDRESS — which is the common case on the public
    //     chain, and is reported as "unknown", never as a restriction.
    let mut filtering = FilterProbe::Unsupported;
    for target in &filter_targets {
        let outcome = probe_filtering(&socket, *target).await;
        if outcome != FilterProbe::Unsupported {
            filtering = outcome;
            break;
        }
    }
    println!();
    println!("NAT filtering    : {}", filtering.describe());
    match filtering {
        FilterProbe::AddressAndPortDependent => {
            println!("  -> A peer whose public port you cannot predict (symmetric NAT, CGNAT,");
            println!("     mobile) cannot reach this socket: that pair falls back to the relay.");
        }
        FilterProbe::AddressDependentOrOpen => {
            println!("  -> A symmetric/CGNAT peer CAN reach this socket once you have sent to");
            println!("     its address: this side is what makes such a pair punchable.");
        }
        FilterProbe::Unsupported => {
            println!("  -> Not measured: no server answered from a second port. Point --to (or");
            println!("     --stun-server) at a `bore server --udp`, which does.");
        }
    }

    // 5. Extra signals: port preservation, CGNAT/double-NAT, bore-server hairpin.
    if let Some(first) = public_obs.first() {
        let refl = first.reflexive;
        println!();
        if refl.port() == local_port {
            println!(
                "Port preservation: YES (local {local_port} == public {}).",
                refl.port()
            );
        } else {
            println!(
                "Port preservation: no  (local {local_port} -> public {}).",
                refl.port()
            );
        }
        if is_cgnat(refl.ip()) {
            println!(
                "CGNAT detected: public address {} is in 100.64.0.0/10.",
                refl.ip()
            );
            println!("  -> P2P is unlikely; the relay is the reliable path here.");
        } else if is_non_routable(refl.ip()) {
            println!(
                "Double-NAT: the 'public' address {} is itself private — another NAT upstream.",
                refl.ip()
            );
        }
    }
    if bore_reachable == Some(false) && !public_obs.is_empty() {
        println!();
        println!("Note: public STUN works but YOUR bore server's UDP did NOT answer.");
        println!("  Likely co-location/hairpin (this host shares the server's machine/LAN),");
        println!("  or UDP to the control port is not open server-side. Run the provider from a");
        println!("  different network, or pass --stun-server <public:port> so candidates still");
        println!("  get a public IP.");
    }

    // 6. UPnP-IGD reachability (home routers).
    println!();
    #[cfg(feature = "udp")]
    match upnp_external_ip().await {
        Ok(ip) => {
            println!(
                "UPnP-IGD router : FOUND, external IP {ip}{}.",
                routability_note(ip)
            );
            println!("  -> --upnp can map a router port here (helps strict home NATs).");
        }
        Err(err) => println!("UPnP-IGD router : none ({err}); --upnp would have no effect here."),
    }
    #[cfg(not(feature = "udp"))]
    println!("UPnP-IGD router : probe skipped (built without the `udp` feature).");

    Ok(())
}

/// Open NAT mappings toward every peer candidate by sending a few small
/// datagrams. QUIC path validation does the real liveness check afterward.
#[cfg(feature = "udp")]
async fn punch(socket: &UdpSocket, peers: &[SocketAddr]) {
    for _ in 0..5 {
        for peer in peers {
            let _ = socket.send_to(b"bore-punch", peer).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// One native QUIC bidirectional stream wrapped as an `AsyncRead`/`AsyncWrite`
/// carrier for a single proxied connection. Keeps the connection and endpoint
/// alive for as long as the stream is in use.
#[cfg(feature = "udp")]
pub struct QuicTransport {
    recv: quinn::RecvStream,
    send: quinn::SendStream,
    /// Phase 03.4 send-side scheduling state for THIS stream. Whoever sends the
    /// bulk demotes itself, so the same code serves both roles (a vhost
    /// download is sent by the provider; an upload by the server) without
    /// having to decide in advance which side starves the other.
    sched: BulkSendState,
    _conn: Connection,
    _endpoint: Endpoint,
}

/// Bytes one QUIC stream may send before it is treated as bulk and demoted in
/// the connection's send scheduler (phase 03.4).
///
/// Same threshold and the same rationale as the relay's
/// `pool::BULK_CLASSIFY_BYTES`: classification is by bytes MOVED (DEC-VE4), so
/// no ordinary asset is ever demoted and every real transfer is demoted
/// promptly. Deliberately a separate constant — the two paths are calibrated
/// independently in phase 03.5 and coupling them would hide that.
#[cfg(feature = "udp")]
pub const DIRECT_BULK_CLASSIFY_BYTES: u64 = 512 * 1024;

/// quinn send priority for a bulk-classified stream. quinn schedules higher
/// values first and every stream starts at `0`, so this is the only value that
/// puts bulk BEHIND ordinary streams without touching anything else.
///
/// DEC-VE8: this replaces "give the starved stream more window". Neither
/// `stream_receive_window` nor `connection_receive_window` moves, and the 16:1
/// ratio `CLAUDE.md` pins stays exactly as it is.
#[cfg(feature = "udp")]
pub const DIRECT_BULK_STREAM_PRIORITY: i32 = -1;

/// quinn streams default to priority `0` and higher values are sent first, so a
/// non-negative bulk priority would make the demotion a silent no-op. Compile
/// time, because there is no reason for it to be a runtime question.
#[cfg(feature = "udp")]
const _: () = assert!(DIRECT_BULK_STREAM_PRIORITY < 0);

/// Bytes a demoted (bulk) stream may hand to the connection in one write.
///
/// Bounds how far ahead one bulk stream can queue between the scheduler's
/// decisions. It is NOT a rate limit: the copy loop simply comes back for the
/// rest, and an undemoted stream is never capped, so a single transfer with no
/// competition is byte-for-byte unaffected.
#[cfg(feature = "udp")]
pub const DIRECT_BULK_SEND_BURST: usize = 128 * 1024;

/// Per-stream send-side scheduling state (phase 03.4).
///
/// Split out as a plain value type with no quinn types in it, because the two
/// decisions it encodes — *when* a stream becomes bulk and *how much* a bulk
/// stream may queue — are the whole of the change and are worth asserting
/// directly rather than through a live QUIC connection.
#[cfg(feature = "udp")]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct BulkSendState {
    /// Bytes accepted by `poll_write` on this stream so far.
    sent: u64,
    /// Whether the stream has already been demoted in the send scheduler.
    demoted: bool,
}

#[cfg(feature = "udp")]
impl BulkSendState {
    /// How many of `want` bytes to offer the connection in this write.
    fn allow(&self, want: usize) -> usize {
        if self.demoted {
            want.min(DIRECT_BULK_SEND_BURST)
        } else {
            want
        }
    }

    /// Account `n` accepted bytes. Returns `true` exactly once: on the write
    /// that takes this stream over the bulk threshold.
    fn record(&mut self, n: usize) -> bool {
        self.sent = self.sent.saturating_add(n as u64);
        if !self.demoted && self.sent >= DIRECT_BULK_CLASSIFY_BYTES {
            self.demoted = true;
            return true;
        }
        false
    }
}

/// An authenticated direct QUIC connection between a consumer and a provider.
/// Proxied connections are carried over **native QUIC streams** (one bidi each,
/// via [`DirectConn::open_stream`] / [`DirectConn::accept_stream`]), so a lost
/// packet on one connection's stream does not stall the others (no head-of-line
/// blocking — unlike multiplexing yamux over a single QUIC stream). Cheap to clone
/// (both fields are handles).
#[cfg(feature = "udp")]
#[derive(Clone)]
pub struct DirectConn {
    conn: Connection,
    endpoint: Endpoint,
}

/// Outcome of a best-effort datagram send on the direct QUIC path.
///
/// `TooLarge` is a transient PER-PACKET condition (the packet is bigger than the
/// current QUIC path-MTU allows), NOT a link failure — the caller drops the
/// packet and keeps the link alive. Genuine link death is an `Err` instead.
#[cfg(feature = "udp")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatagramSend {
    /// Queued for transmission.
    Sent,
    /// Dropped: larger than the current path-MTU datagram limit.
    TooLarge,
}

#[cfg(feature = "udp")]
impl DirectConn {
    /// Open a new native QUIC bidi stream for one proxied connection (consumer).
    pub async fn open_stream(&self) -> Result<QuicTransport> {
        let (send, recv) = self.conn.open_bi().await.context("open_bi failed")?;
        Ok(QuicTransport {
            recv,
            send,
            sched: BulkSendState::default(),
            _conn: self.conn.clone(),
            _endpoint: self.endpoint.clone(),
        })
    }

    /// Accept the next native QUIC bidi stream for one proxied connection (provider).
    pub async fn accept_stream(&self) -> Result<QuicTransport> {
        let (send, recv) = self.conn.accept_bi().await.context("accept_bi failed")?;
        Ok(QuicTransport {
            recv,
            send,
            sched: BulkSendState::default(),
            _conn: self.conn.clone(),
            _endpoint: self.endpoint.clone(),
        })
    }

    /// Resolve when the QUIC connection closes (peer gone, idle timeout, or a
    /// graceful close), so the consumer can re-negotiate or fall back to the relay.
    pub async fn closed(&self) {
        self.conn.closed().await;
    }

    /// Gracefully close the QUIC connection so the peer immediately reverts or renews.
    pub fn close(&self) {
        self.conn.close(0u32.into(), b"vhost direct path closed");
    }

    /// Snapshot the current QUIC connection statistics for diagnostics.
    pub fn stats(&self) -> quinn::ConnectionStats {
        self.conn.stats()
    }

    /// Snapshot the current path MTU-dependent datagram size, if available.
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }

    /// Send an IP packet as a QUIC unreliable datagram. Non-blocking.
    ///
    /// Returns `Ok(DatagramSend::TooLarge)` — NOT `Err` — when the packet
    /// exceeds the current QUIC path-MTU datagram limit. That happens whenever
    /// the TUN MTU runs ahead of the path MTU: throughout the initial MTU
    /// discovery window, and briefly after every switch to the direct path (the
    /// TUN starts at its configured MTU; the PMTU monitor narrows it once QUIC
    /// settles). The caller MUST drop such a packet and keep going — it is a
    /// transient per-packet condition, not a link failure. The VPN bridge
    /// counts these and warns after >10 s.
    ///
    /// `Err` is reserved for genuine link death (`ConnectionLost`, datagrams
    /// `Disabled`/`UnsupportedByPeer`) so the bridge tears down and reconnects.
    ///
    /// quinn 0.11 silently drops the *oldest* queued datagram when the send
    /// buffer is full, so calling this from the uplink hot loop is safe without
    /// backpressure.
    pub fn send_datagram(&self, pkt: bytes::Bytes) -> Result<DatagramSend> {
        match self.conn.send_datagram(pkt) {
            Ok(()) => Ok(DatagramSend::Sent),
            Err(quinn::SendDatagramError::TooLarge) => Ok(DatagramSend::TooLarge),
            Err(e) => Err(anyhow::anyhow!("send_datagram: {e}")),
        }
    }

    /// Send an IP packet as a QUIC datagram, AWAITING send-buffer room instead of
    /// dropping the oldest queued datagram when the buffer is full.
    ///
    /// This is the backpressure path (VPN F3): the plain [`send_datagram`] is
    /// non-blocking and quinn silently drops the OLDEST queued datagram on a full
    /// buffer, which the tunnelled TCP reads as loss and reacts to by collapsing
    /// its window — congestion masquerading as loss. Awaiting room instead pauses
    /// the caller (the uplink task), the kernel TUN queue fills, and the inner
    /// senders pace themselves to the real drain rate. Caller MUST be a dedicated
    /// per-link task: a shared pump would head-of-line block every other flow.
    ///
    /// `TooLarge` stays a per-packet drop (not awaitable); `Err` is link death.
    pub async fn send_datagram_wait(&self, pkt: bytes::Bytes) -> Result<DatagramSend> {
        match self.conn.send_datagram_wait(pkt).await {
            Ok(()) => Ok(DatagramSend::Sent),
            Err(quinn::SendDatagramError::TooLarge) => Ok(DatagramSend::TooLarge),
            Err(e) => Err(anyhow::anyhow!("send_datagram_wait: {e}")),
        }
    }

    /// Read the next QUIC datagram. Resolves when a datagram arrives or the
    /// connection closes (in which case `Err` signals path death to the bridge).
    pub async fn read_datagram(&self) -> Result<bytes::Bytes> {
        self.conn.read_datagram().await.context("read_datagram")
    }

    /// The connection's resolved remote address (the winning peer candidate).
    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Open an ADDITIONAL authenticated QUIC connection to the SAME peer over the
    /// SAME endpoint/socket (VPN direct-path carrier, Fix #3a). The hole-punched
    /// 5-tuple is already open, so no new punch is needed — quinn assigns the new
    /// connection fresh connection IDs and the peer's server endpoint demuxes it.
    /// Each carrier gets its OWN congestion controller, so N carriers give a
    /// single high-BDP VPN flow ~N× the in-flight window (parallel-stream effect),
    /// which a lone loss-bound flow cannot reach. Consumer-side handshake (writes
    /// its token first, then reads the peer's), mirroring [`connect_direct`].
    pub async fn open_sibling(&self, token: [u8; TOKEN_LEN]) -> Result<DirectConn> {
        let peer = self.conn.remote_address();
        let conn = self
            .endpoint
            .connect(peer, "bore")
            .context("failed to start direct carrier connect")?
            .await
            .context("direct carrier QUIC handshake failed")?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .context("carrier auth open_bi failed")?;
        send.write_all(&token).await?;
        send.flush().await?;
        let mut peer_token = [0u8; TOKEN_LEN];
        recv.read_exact(&mut peer_token).await?;
        if !tokens_match(&token, &peer_token) {
            bail!("direct carrier token mismatch");
        }
        let _ = send.finish();
        debug!(%peer, "direct carrier connection established (consumer, token verified)");
        Ok(DirectConn {
            conn,
            endpoint: self.endpoint.clone(),
        })
    }
}

// quinn's streams carry inherent `poll_read`/`poll_write` methods (with quinn's
// own error types) that shadow the trait methods, so delegate with fully
// qualified trait syntax to reach the tokio `AsyncRead`/`AsyncWrite` impls.
#[cfg(feature = "udp")]
impl AsyncRead for QuicTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        AsyncRead::poll_read(Pin::new(&mut self.recv), cx, buf)
    }
}

#[cfg(feature = "udp")]
impl AsyncWrite for QuicTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Phase 03.4: a bulk stream is capped per write and demoted in the
        // connection's send scheduler, so a small stream opened later is not
        // scheduled behind a large in-flight burst. Until a stream crosses the
        // threshold this is `buf` unchanged and one branch on a bool.
        let offer = self.sched.allow(buf.len());
        match AsyncWrite::poll_write(Pin::new(&mut self.send), cx, &buf[..offer]) {
            Poll::Ready(Ok(n)) => {
                if self.sched.record(n) {
                    // `set_priority` fails only on an already-closed stream,
                    // which the write above would have reported; scheduling is
                    // an optimization, so a failure here is never fatal.
                    let priority = DIRECT_BULK_STREAM_PRIORITY;
                    if self.send.set_priority(priority).is_ok() {
                        debug!(
                            bytes = self.sched.sent,
                            priority, "direct stream classified bulk; demoted in send scheduler"
                        );
                    }
                }
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

/// Consumer side: punch toward `peers`, connect a QUIC client over `socket`, and
/// authenticate the connection with `token` on a dedicated stream. Returns the
/// authenticated [`DirectConn`]; proxied connections then ride native QUIC streams
/// opened on it. The consumer opens the auth stream.
#[cfg(feature = "udp")]
pub async fn connect_direct(
    socket: UdpSocket,
    peers: Vec<SocketAddr>,
    token: [u8; TOKEN_LEN],
    tuning: UdpDirectTuning,
) -> Result<DirectConn> {
    connect_direct_inner(socket, peers, None, token, tuning, true).await
}

/// Delay before the non-nominated candidates are dialed when a
/// check-nominated pair exists (Happy-Eyeballs fallback, plan Fase 2): the
/// validated pair gets a head start; the others only race it if it stalls.
#[cfg(feature = "udp")]
const NOMINATED_FALLBACK_DELAY: Duration = Duration::from_millis(500);

/// Like [`connect_direct`], but dial the check-nominated candidate FIRST; the
/// remaining candidates start only after [`NOMINATED_FALLBACK_DELAY`] as a
/// Happy-Eyeballs fallback. Same single socket, same total budget.
#[cfg(feature = "udp")]
pub async fn connect_direct_nominated(
    socket: UdpSocket,
    peers: Vec<SocketAddr>,
    nominated: SocketAddr,
    token: [u8; TOKEN_LEN],
    tuning: UdpDirectTuning,
) -> Result<DirectConn> {
    // The check round already opened the mappings; the blind punch is
    // redundant (QUIC Initials are outbound datagrams themselves).
    connect_direct_inner(socket, peers, Some(nominated), token, tuning, false).await
}

#[cfg(feature = "udp")]
async fn connect_direct_inner(
    socket: UdpSocket,
    mut peers: Vec<SocketAddr>,
    nominated: Option<SocketAddr>,
    token: [u8; TOKEN_LEN],
    tuning: UdpDirectTuning,
    punch_first: bool,
) -> Result<DirectConn> {
    // Defense in depth: the broker already sanitizes peer-controlled lists, but
    // this is the last gate before per-candidate task fan-out (I-11).
    let san = sanitize_candidates(&mut peers);
    log_dropped_candidates("connect_direct", peers.len(), &san);
    if let Some(nominated) = nominated {
        // The nominated pair is check-validated; make sure it is dialed even
        // if it was not in the original offer (peer-reflexive learning).
        if !peers.contains(&nominated) && valid_candidate(&nominated) {
            peers.insert(0, nominated);
        }
    }
    if peers.is_empty() {
        bail!("no peer candidates to connect to (fallback_reason=no-candidates)");
    }
    let started = Instant::now();
    let local_addr = socket.local_addr().ok();
    info!(
        udp_local_addr = ?local_addr,
        peer_candidates = ?peers,
        nominated = ?nominated,
        punch_first,
        "consumer punching UDP peer candidates"
    );
    if punch_first {
        punch(&socket, &peers).await;
    }
    let endpoint = client_endpoint(socket, &tuning)?;

    // Try all candidates concurrently under a single total budget (not a full
    // timeout *per* candidate): with N candidates the serial worst case was
    // N * NETWORK_TIMEOUT (6-21s for predicted/UPnP/local lists). `select_ok`
    // returns the first handshake that completes and verifies its token; the
    // losing connects are dropped (cancelled). Per-candidate errors are collected
    // in a shared Vec so the final warn includes each candidate's failure reason.
    let errors: Arc<Mutex<Vec<(SocketAddr, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let attempts: Vec<_> = peers
        .iter()
        .map(|&peer| {
            let endpoint = endpoint.clone();
            let errors = Arc::clone(&errors);
            Box::pin(async move {
                // Happy-Eyeballs: with a nominated pair, the others wait.
                if nominated.is_some_and(|n| n != peer) {
                    tokio::time::sleep(NOMINATED_FALLBACK_DELAY).await;
                }
                debug!(%peer, "attempting direct QUIC candidate");
                let connecting = match endpoint.connect(peer, "bore") {
                    Ok(connecting) => connecting,
                    Err(err) => {
                        let msg = format!("start failed: {err}");
                        debug!(%peer, %err, "failed to start direct QUIC candidate");
                        errors.lock().unwrap().push((peer, msg));
                        return Err(err.into());
                    }
                };
                let conn = match connecting.await {
                    Ok(conn) => conn,
                    Err(err) => {
                        let msg = format!("{err}");
                        debug!(%peer, %err, "direct QUIC candidate failed");
                        errors.lock().unwrap().push((peer, msg));
                        return Err(err.into());
                    }
                };
                trace!(%peer, "QUIC connected");
                // Authenticate the connection once, on a dedicated stream: consumer
                // writes its token first, then reads the peer's. Data streams opened
                // afterward are trusted (same authenticated QUIC connection).
                let (mut send, mut recv) = conn.open_bi().await.context("auth open_bi failed")?;
                send.write_all(&token).await?;
                send.flush().await?;
                let mut peer_token = [0u8; TOKEN_LEN];
                recv.read_exact(&mut peer_token).await?;
                if !tokens_match(&token, &peer_token) {
                    let msg = "token mismatch".to_string();
                    warn!(%peer, "direct QUIC candidate failed token verification");
                    errors.lock().unwrap().push((peer, msg));
                    bail!("direct path token mismatch");
                }
                let _ = send.finish();
                info!(target_addr = %peer, peer = %conn.remote_address(),
                    "direct udp connection established (consumer, token verified)");
                let dc = DirectConn { conn, endpoint };
                debug!(max_datagram = ?dc.max_datagram_size(), "direct conn established (consumer)");
                anyhow::Ok(dc)
            })
        })
        .collect();

    match timeout(NETWORK_TIMEOUT, futures_util::future::select_ok(attempts)).await {
        Ok(Ok((conn, _losers))) => {
            info!(
                winner = %conn.remote_address(),
                candidates = peers.len(),
                direct_ready_ms = started.elapsed().as_millis() as u64,
                "direct QUIC path ready (consumer)"
            );
            Ok(conn)
        }
        Ok(Err(err)) => {
            let err_summary: Vec<String> = errors
                .lock()
                .unwrap()
                .iter()
                .map(|(addr, msg)| format!("{addr} → {msg}"))
                .collect();
            warn!(
                candidates = ?peers,
                errors = ?err_summary,
                elapsed_ms = started.elapsed().as_millis() as u64,
                fallback_reason = "all-candidates-failed",
                "all {n} direct QUIC candidates failed; falling back to relay",
                n = peers.len(),
            );
            Err(err).context("all direct candidates failed")
        }
        Err(_) => {
            warn!(
                timeout = ?NETWORK_TIMEOUT,
                candidates = ?peers,
                fallback_reason = "budget-exhausted",
                "direct QUIC connect exhausted {NETWORK_TIMEOUT:?} budget \
                 across {n} candidates; none responded — all candidates timed out \
                 (firewall/UDP blocked on both ends, or peer IP unreachable). \
                 Falling back to relay",
                n = peers.len(),
            );
            bail!("direct connect exhausted the {NETWORK_TIMEOUT:?} budget")
        }
    }
}

/// Fase 2 orchestration, listener/provider side: run the authenticated check
/// round (which doubles as the punch — every request is an outbound datagram
/// that opens the NAT), then start the QUIC listener on the SAME socket.
/// The caller keeps its legacy accept loop unchanged.
#[cfg(feature = "udp")]
pub async fn listener_checks_then_quic(
    socket: UdpSocket,
    peers: &[SocketAddr],
    cfg: &CheckConfig,
    tuning: UdpDirectTuning,
) -> Result<(DirectListener, CheckOutcome)> {
    let tsock = UdpTraversalSocket::from_socket(socket);
    let mut outcome = tsock.run_connectivity_checks(peers, cfg).await;
    let socket = tsock.into_socket().await?;
    let socket = sprayed_escape(socket, &mut outcome, cfg).await;
    let listener = DirectListener::from_checked_socket(socket, tuning)?;
    Ok((listener, outcome))
}

/// Fase 7: run the sprayed escape when the check round came back DRY and the
/// broker assigned this side a role, and return the socket QUIC should use.
///
/// Three deliberate properties:
///
/// * it runs ONLY on a dry round. A nominated pair means the ordinary
///   mechanism worked, and spending six seconds re-proving it would make the
///   common case worse to help a rare one;
/// * on the hard side the promoted socket REPLACES the original only when the
///   escape actually won. A failed escape returns the original untouched, so
///   the fallback that follows is the one that would have run anyway;
/// * a win is written back into the `CheckOutcome`, so the caller's existing
///   "direct path established after check round" line reports the nominated
///   pair without knowing the escape exists.
#[cfg(feature = "udp")]
async fn sprayed_escape(
    socket: UdpSocket,
    outcome: &mut CheckOutcome,
    cfg: &CheckConfig,
) -> UdpSocket {
    let Some(role) = cfg.spray else {
        return socket;
    };
    if outcome.nominated.is_some() {
        return socket;
    }
    let tuning = spray::SprayTuning::from_env();
    if !tuning.enabled() {
        debug!("sprayed escape disabled by its tuning");
        return socket;
    }
    let started = Instant::now();
    let (socket, hit) = match role {
        spray::SprayRole::Easy => {
            let hit = spray::easy_side(&socket, &outcome.targets, cfg, &tuning).await;
            (socket, hit)
        }
        spray::SprayRole::Hard => match spray::hard_side(&outcome.targets, cfg, &tuning).await {
            Some((promoted, hit)) => (promoted, Some(hit)),
            None => (socket, None),
        },
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match hit {
        Some(hit) => {
            info!(
                ?role,
                %hit,
                escape_ms = elapsed_ms,
                "sprayed escape found a working pair the ordinary round could not"
            );
            if !outcome.targets.contains(&hit) {
                outcome.targets.push(hit);
            }
            outcome.nominated = Some(hit);
        }
        None => {
            warn!(
                ?role,
                escape_ms = elapsed_ms,
                p_collision = format!("{:.0}%", tuning.collision_probability() * 100.0),
                fallback_reason = "spray-exhausted",
                "sprayed escape found no colliding port within its budget;                  continuing with the ordinary fallback (the relay stays warm)"
            );
        }
    }
    socket
}

/// Fase 2 orchestration, dialer/consumer side: run the check round, then dial
/// the nominated pair first (Happy-Eyeballs fallback on the rest), or — when
/// nothing validated — dial the round's final target list (which includes any
/// learned peer-reflexive address) exactly like the legacy path, minus the
/// now-redundant blind punch.
///
/// `cache_key`, when given, engages the winning-pair cache (plan Fase 3):
/// the last known-good remote is probed FIRST (an extra head group), a
/// successful QUIC handshake refreshes the entry, and ANY direct failure
/// invalidates it immediately.
#[cfg(feature = "udp")]
pub async fn dialer_checks_then_quic(
    socket: UdpSocket,
    mut peers: Vec<SocketAddr>,
    cfg: &CheckConfig,
    token: [u8; TOKEN_LEN],
    tuning: UdpDirectTuning,
    cache_key: Option<&str>,
) -> Result<(DirectConn, CheckOutcome)> {
    let mut cfg = cfg.clone();
    if let Some(key) = cache_key {
        if let Some(cached) = pair_cache::recall(key) {
            info!(
                %cached,
                key,
                "probing cached winning pair first (invalidated on first direct failure)"
            );
            if !peers.contains(&cached) && valid_candidate(&cached) {
                peers.push(cached);
            }
            let plan = cfg.plan.get_or_insert_with(CheckPlan::default);
            plan.groups.insert(0, vec![cached]);
        }
    }
    let tsock = UdpTraversalSocket::from_socket(socket);
    let mut outcome = tsock.run_connectivity_checks(&peers, &cfg).await;
    let socket = tsock.into_socket().await?;
    // Fase 7: a dry round on the one cell the matrix says cannot be won by an
    // ordinary round gets one sprayed escape before the fallback. A win writes
    // itself into `outcome.nominated`, so the dial below needs no new branch.
    let socket = sprayed_escape(socket, &mut outcome, &cfg).await;
    let conn = connect_direct_inner(
        socket,
        outcome.targets.clone(),
        outcome.nominated,
        token,
        tuning,
        false,
    )
    .await;
    if let Some(key) = cache_key {
        match &conn {
            Ok(conn) => pair_cache::remember(key, conn.remote_address()),
            Err(_) => pair_cache::invalidate(key),
        }
    }
    Ok((conn?, outcome))
}

/// Short-lived winning-pair cache (plan Fase 3): remembers, per logical
/// tunnel, the remote address that last completed a direct QUIC handshake so
/// a reconnect probes it FIRST (head group of the next check round). Process-
/// local and advisory only — a stale entry costs one probe, never blocks the
/// round (membership still comes from the fresh offer + sanitizer), and the
/// entry is invalidated on the first direct failure.
pub mod pair_cache {
    use super::*;
    use std::sync::OnceLock;

    /// A NAT mapping rarely outlives its idle timer; two minutes covers the
    /// quick-reconnect case (process still up, `--auto-reconnect`) without
    /// dragging a dead pair into genuinely new network conditions.
    const PAIR_CACHE_TTL: Duration = Duration::from_secs(120);

    fn cache() -> &'static std::sync::Mutex<HashMap<String, (SocketAddr, Instant)>> {
        static CACHE: OnceLock<std::sync::Mutex<HashMap<String, (SocketAddr, Instant)>>> =
            OnceLock::new();
        CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
    }

    /// Hard ceiling on cached pairs (S-9). The cache is process-global and
    /// keyed by tunnel id, so a process that dials many DIFFERENT tunnels over
    /// its lifetime — a test harness, a multi-link VPN, a long-lived box that
    /// reconnects under new ids — accumulated one entry per id forever:
    /// expiry only ever ran inside `recall` FOR THAT KEY, and a key that is
    /// never recalled is never examined. Each entry is small, which is exactly
    /// what makes this the kind of growth nobody notices.
    pub(crate) const PAIR_CACHE_MAX: usize = 256;

    /// Record `addr` as the winning pair for `key`.
    ///
    /// Expired entries are swept here, not only in `recall`, because this is
    /// the one call that is guaranteed to happen for every new key.
    pub fn remember(key: &str, addr: SocketAddr) {
        let mut guard = cache().lock().unwrap();
        guard.retain(|_, (_, at)| at.elapsed() < PAIR_CACHE_TTL);
        if guard.len() >= PAIR_CACHE_MAX && !guard.contains_key(key) {
            // Everything still here is inside its TTL, so there is no "least
            // useful" entry to pick: drop the oldest, which is the one closest
            // to expiring anyway.
            if let Some(oldest) = guard
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone())
            {
                guard.remove(&oldest);
            }
        }
        guard.insert(key.to_string(), (addr, Instant::now()));
    }

    /// The cached winning pair for `key`, if still within TTL (expired
    /// entries are removed on the way out).
    pub fn recall(key: &str) -> Option<SocketAddr> {
        let mut guard = cache().lock().unwrap();
        match guard.get(key) {
            Some((addr, at)) if at.elapsed() < PAIR_CACHE_TTL => Some(*addr),
            Some(_) => {
                guard.remove(key);
                None
            }
            None => None,
        }
    }

    /// Drop the cached pair for `key` (first failure ⇒ immediate invalidation).
    pub fn invalidate(key: &str) {
        cache().lock().unwrap().remove(key);
    }

    /// Live entry count. Exists so the bound can be asserted (S-9); nothing on
    /// the data path reads it.
    #[cfg(test)]
    pub fn len() -> usize {
        cache().lock().unwrap().len()
    }
}

/// Provider side (QUIC client): dial a public bore server's vhost QUIC endpoint
/// and authenticate for `subdomain`. No hole-punching is needed because the
/// server is public; the provider later accepts native QUIC streams on the
/// returned connection, while the server opens them.
#[cfg(feature = "udp")]
pub async fn vhost_connect(
    socket: UdpSocket,
    server_addr: SocketAddr,
    subdomain: &str,
    token: [u8; TOKEN_LEN],
    tuning: UdpDirectTuning,
) -> Result<DirectConn> {
    if subdomain.len() > MAX_SERVER_DIRECT_KEY_LEN {
        bail!("server-direct key exceeds {MAX_SERVER_DIRECT_KEY_LEN} bytes");
    }
    let subdomain_len: u16 = subdomain
        .len()
        .try_into()
        .context("vhost subdomain too long for QUIC auth frame")?;
    let endpoint = client_endpoint(socket, &tuning)?;
    let conn = timeout(
        NETWORK_TIMEOUT,
        endpoint
            .connect(server_addr, "bore")
            .context("failed to start vhost QUIC connect")?,
    )
    .await
    .context("vhost QUIC connect timed out")?
    .context("vhost QUIC handshake failed")?;

    timeout(NETWORK_TIMEOUT, async {
        let (mut send, mut recv) = conn.open_bi().await.context("auth open_bi failed")?;
        send.write_all(&subdomain_len.to_be_bytes()).await?;
        send.write_all(subdomain.as_bytes()).await?;
        send.write_all(&token).await?;
        send.flush().await?;

        let mut peer_token = [0u8; TOKEN_LEN];
        recv.read_exact(&mut peer_token).await?;
        if !tokens_match(&token, &peer_token) {
            bail!("vhost direct path token mismatch");
        }

        let _ = send.finish();
        info!(server = %server_addr, subdomain, "vhost direct udp connection established");
        let dc = DirectConn { conn, endpoint };
        debug!(max_datagram = ?dc.max_datagram_size(), "direct conn established (vhost consumer)");
        Ok(dc)
    })
    .await
    .context("vhost QUIC auth timed out")?
}

/// Provider side: a long-lived QUIC server endpoint that accepts direct
/// connections from punched consumers.
#[cfg(feature = "udp")]
pub struct DirectListener {
    endpoint: Endpoint,
}

#[cfg(feature = "udp")]
impl DirectListener {
    /// Start a QUIC server endpoint over a socket whose NAT path was ALREADY
    /// opened by an authenticated connectivity-check round (plan Fase 2):
    /// no blind punch — the checks were the punch.
    pub fn from_checked_socket(socket: UdpSocket, tuning: UdpDirectTuning) -> Result<Self> {
        // Buffers are configured by `server_endpoint` itself (P-13).
        let endpoint = server_endpoint(socket, &tuning)?;
        Ok(DirectListener { endpoint })
    }

    /// Punch toward `peers` and start a QUIC server endpoint over `socket`.
    pub async fn new(
        socket: UdpSocket,
        mut peers: Vec<SocketAddr>,
        tuning: UdpDirectTuning,
    ) -> Result<Self> {
        let san = sanitize_candidates(&mut peers);
        log_dropped_candidates("direct_listener", peers.len(), &san);
        let local_addr = socket.local_addr().ok();
        info!(
            udp_local_addr = ?local_addr,
            peer_candidates = ?peers,
            "provider punching UDP peer candidates and starting QUIC listener"
        );
        punch(&socket, &peers).await;
        let endpoint = server_endpoint(socket, &tuning)?;
        Ok(DirectListener { endpoint })
    }

    /// Gracefully close the endpoint and all its connections, so the peer detects
    /// the shutdown immediately instead of waiting for the idle timeout.
    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"provider shutting down");
    }

    /// Re-open this endpoint's NAT mapping toward a new (e.g. reconnecting)
    /// consumer once the raw socket is owned by `quinn` and can no longer be used
    /// for [`punch`]. Fires a throwaway outbound QUIC connection per candidate:
    /// the consumer is a QUIC client and won't complete it, but the outbound
    /// packets punch the mapping so the consumer's own connection gets through.
    pub fn punch_via_endpoint(&self, peers: &[SocketAddr]) {
        let mut peers = peers.to_vec();
        let san = sanitize_candidates(&mut peers);
        log_dropped_candidates("punch_via_endpoint", peers.len(), &san);
        info!(peer_candidates = ?peers, "provider re-punching UDP peer candidates");
        for peer in peers {
            if let Ok(connecting) = self.endpoint.connect(peer, "bore") {
                tokio::spawn(async move {
                    let _ = timeout(NETWORK_TIMEOUT, connecting).await;
                });
            }
        }
    }

    /// Accept the next direct connection and authenticate it with `token` on a
    /// dedicated stream. The provider reads the peer's token first, then sends its
    /// own. Returns the authenticated [`DirectConn`]; the provider then accepts
    /// native QUIC streams on it (one per proxied connection).
    pub async fn accept(&self, token: [u8; TOKEN_LEN]) -> Result<DirectConn> {
        // UDP hole-punch crossfire makes both peers fire QUIC Initials at each
        // other, so the endpoint sees incipient connections that never finish the
        // TLS handshake — or finish but present no / a mismatched token. These are
        // BENIGN (the real, token-verified connection arrives alongside them): log
        // each at `debug` and keep accepting, rather than surfacing an alarming
        // "accept failed" to the caller (BUG-S3). Only an endpoint-level failure
        // (the QUIC endpoint itself closed) propagates as `Err`. Callers that wrap
        // this in a timeout (VPN/diagnostic) get a real connection within the
        // window instead of aborting on the first stray.
        loop {
            let incoming = self
                .endpoint
                .accept()
                .await
                .context("QUIC endpoint closed")?;
            let remote = incoming.remote_address();
            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(err) => {
                    debug!(%remote, %err, "stray direct incoming: handshake never completed (hole-punch crossfire); ignoring");
                    continue;
                }
            };
            let peer = conn.remote_address();
            trace!(%peer, "QUIC accepted");
            let (mut send, mut recv) = match conn.accept_bi().await {
                Ok(streams) => streams,
                Err(err) => {
                    debug!(%peer, %err, "stray direct incoming: no auth stream opened; ignoring");
                    continue;
                }
            };
            let mut peer_token = [0u8; TOKEN_LEN];
            if let Err(err) = recv.read_exact(&mut peer_token).await {
                debug!(%peer, %err, "stray direct incoming: auth token read failed; ignoring");
                continue;
            }
            if !tokens_match(&token, &peer_token) {
                debug!(
                    %peer,
                    "stray direct incoming: token mismatch (unrelated peer or mismatched secret); ignoring"
                );
                continue;
            }
            // A verified peer that vanishes before we can send/flush our token
            // reply is a benign stray, not an endpoint-level failure. Propagating
            // it via `?` bubbled out of `accept()` and callers mislabeled it as
            // "endpoint closed" (a misleading log plus a 100ms accept hiccup for
            // an unrelated peer's reset). Treat it like the other stray arms above:
            // log at debug and keep accepting. Only `.accept()`/`accept_bi()`
            // endpoint-level errors remain fatal.
            if let Err(err) = send.write_all(&token).await {
                debug!(%peer, %err, "verified peer vanished before token reply sent; ignoring");
                continue;
            }
            if let Err(err) = send.flush().await {
                debug!(%peer, %err, "verified peer vanished before token reply flushed; ignoring");
                continue;
            }
            let _ = send.finish();
            info!(%peer, "accepted direct udp connection (provider, token verified)");
            let dc = DirectConn {
                conn,
                endpoint: self.endpoint.clone(),
            };
            debug!(max_datagram = ?dc.max_datagram_size(), "direct conn established (provider)");
            return Ok(dc);
        }
    }
}

/// Server side: build a QUIC endpoint for the vhost direct path.
#[cfg(feature = "udp")]
pub(crate) fn vhost_server_endpoint(
    socket: UdpSocket,
    tuning: &UdpDirectTuning,
) -> Result<Endpoint> {
    server_endpoint(socket, tuning)
}

/// Server side (QUIC server): authenticate one accepted vhost direct-path
/// connection and return the verified subdomain plus the trusted connection.
#[cfg(feature = "udp")]
pub async fn vhost_server_handshake(
    conn: quinn::Connection,
    endpoint: Endpoint,
    lookup: impl Fn(&str) -> Option<[u8; TOKEN_LEN]>,
) -> Result<(String, DirectConn)> {
    let peer = conn.remote_address();
    timeout(NETWORK_TIMEOUT, async {
        let (mut send, mut recv) = conn.accept_bi().await.context("auth accept_bi failed")?;

        let mut sub_len = [0u8; 2];
        recv.read_exact(&mut sub_len).await?;
        let sub_len = u16::from_be_bytes(sub_len) as usize;
        if sub_len > MAX_SERVER_DIRECT_KEY_LEN {
            bail!("server-direct key exceeds {MAX_SERVER_DIRECT_KEY_LEN} bytes");
        }

        let mut subdomain = vec![0u8; sub_len];
        recv.read_exact(&mut subdomain).await?;
        let subdomain = String::from_utf8(subdomain).context("vhost auth subdomain is not UTF-8")?;

        let mut received = [0u8; TOKEN_LEN];
        recv.read_exact(&mut received).await?;

        let expected = lookup(&subdomain).context("unknown vhost direct-path subdomain")?;
        if !tokens_match(&expected, &received) {
            warn!(%peer, subdomain = %subdomain, "rejected vhost direct udp connection: token mismatch");
            bail!("vhost direct path token mismatch");
        }

        send.write_all(&expected).await?;
        send.flush().await?;
        let _ = send.finish();
        info!(%peer, subdomain = %subdomain, "accepted vhost direct udp connection");
        let dc = DirectConn { conn, endpoint };
        debug!(max_datagram = ?dc.max_datagram_size(), "direct conn established (vhost provider)");
        Ok((subdomain, dc))
    })
    .await
    .context("vhost QUIC auth timed out")?
}

/// Build a QUIC client endpoint over an already-bound UDP socket.
///
/// The socket buffers are configured HERE, not at the call sites, for the reason
/// spelled out on [`server_endpoint`]: a QUIC endpoint over an untuned socket is
/// a throughput defect that nothing reports, and the only way to make it
/// unrepeatable is to make the constructor the single place that can grant one.
#[cfg(feature = "udp")]
fn client_endpoint(socket: UdpSocket, tuning: &UdpDirectTuning) -> Result<Endpoint> {
    let socket = into_std(socket)?;
    configure_udp_socket_buffers(&socket, tuning);
    let mut endpoint = Endpoint::new(
        EndpointConfig::default(),
        None,
        socket,
        Arc::new(TokioRuntime),
    )
    .context("failed to create QUIC client endpoint")?;
    endpoint.set_default_client_config(client_config(tuning)?);
    Ok(endpoint)
}

/// Build a QUIC server endpoint over an already-bound UDP socket. It also carries
/// a default client config so it can fire outbound connections to punch its NAT
/// toward reconnecting consumers (see [`DirectListener::punch_via_endpoint`]).
///
/// P-13: the socket buffers are configured HERE, inside the constructor, and not
/// by the caller. They used to be the caller's job, and the ONE caller that is
/// the server's own shared endpoint — `vhost_server_endpoint`, which serves the
/// direct path of every vhost, public and ssh-jump tunnel on the process — never
/// did it. That endpoint therefore ran on the kernel default
/// (`net.core.rmem_default`, 212992 bytes on Debian/Ubuntu) while every OTHER
/// UDP socket in this file asked for 16 MiB, and it is the RECEIVING side of
/// every download: the bytes arrive from the provider over QUIC and leave over
/// the public TCP socket. Measured on staging, 2026-09-11: `ss -uapm` reported
/// `rb212992` on the live endpoint, the server received 3.885 GiB to deliver
/// 2.279 GiB through it (1.78x — retransmission of what the socket dropped),
/// goodput was 111 MB/s against the TCP relay's 213, and the direct path cost
/// 13.58 CPU s/GiB against the relay's 5.35-7.04 with 2.9x the kernel softirq
/// per delivered GiB. The doc comment on [`configure_udp_socket_buffers`] had
/// described exactly this failure — "capped at ~buffer/RTT" — since it was
/// written; the defect was that one socket never called it.
///
/// Putting the call in the constructor rather than adding a third call site is
/// deliberate: it makes the invariant structural. No future endpoint can be
/// built over an untuned socket, because there is no path to an `Endpoint` that
/// does not pass through here.
#[cfg(feature = "udp")]
fn server_endpoint(socket: UdpSocket, tuning: &UdpDirectTuning) -> Result<Endpoint> {
    let socket = into_std(socket)?;
    configure_udp_socket_buffers(&socket, tuning);
    let mut endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(server_config(tuning)?),
        socket,
        Arc::new(TokioRuntime),
    )
    .context("failed to create QUIC server endpoint")?;
    endpoint.set_default_client_config(client_config(tuning)?);
    Ok(endpoint)
}

/// Convert a Tokio UDP socket into a nonblocking std socket for `quinn`.
#[cfg(feature = "udp")]
fn into_std(socket: UdpSocket) -> Result<StdUdpSocket> {
    let socket = socket.into_std().context("failed to detach UDP socket")?;
    socket
        .set_nonblocking(true)
        .context("failed to set socket nonblocking")?;
    Ok(socket)
}

#[cfg(feature = "udp")]
fn transport_config(tuning: &UdpDirectTuning) -> quinn::TransportConfig {
    let mut cfg = quinn::TransportConfig::default();
    let live = direct_quic_liveness();
    if live.keepalive_adjusted {
        warn!(
            keepalive_ms = live.keepalive.as_millis() as u64,
            idle_ms = live.max_idle.as_millis() as u64,
            "direct QUIC keep-alive tightened to stay under a third of the requested idle timeout"
        );
    }
    cfg.keep_alive_interval(Some(live.keepalive));
    cfg.max_idle_timeout(Some(live.max_idle.try_into().expect("valid idle timeout")));

    // S-7: the path is already known to work when this endpoint is built, so
    // the handshake does not start from RFC 9002's no-information assumption.
    // See `DIRECT_INITIAL_RTT` for the measurement that priced the default.
    cfg.initial_rtt(direct_initial_rtt());

    // Opt-in only: unset leaves quinn's ACK behaviour byte-identical. See
    // `direct_ack_eliciting_threshold`.
    if let Some(threshold) = direct_ack_eliciting_threshold() {
        let mut ack = quinn::AckFrequencyConfig::default();
        ack.ack_eliciting_threshold(
            quinn::VarInt::from_u64(threshold).unwrap_or(quinn::VarInt::MAX),
        );
        cfg.ack_frequency_config(Some(ack));
    }

    // High-throughput direct transfers need flow-control windows larger than
    // Quinn's defaults. The values come from the brokered tuning struct, so the
    // server can override them without changing the code path that consumes it.
    cfg.stream_receive_window(tuning.stream_receive_window.into());
    cfg.receive_window(tuning.connection_receive_window.into());
    cfg.send_window(tuning.send_window);

    // TCP relay often benefits from kernel BBR. Use Quinn's BBR controller for
    // the direct QUIC path too, so high-BDP peer-to-peer transfers are not stuck
    // with the default CUBIC behavior when the network favors model-based pacing.
    cfg.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));

    // One native QUIC stream per proxied connection: raise the concurrent-stream
    // limit well above quinn's small default so it is not the bottleneck.
    cfg.max_concurrent_bidi_streams(tuning.max_direct_streams.into());

    // VPN datagram path: pre-allocate large buffers so RX/TX bursts of IP
    // packets don't stall waiting for the application loop to drain them.
    cfg.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    cfg.datagram_send_buffer_size(8 * 1024 * 1024);

    cfg
}

/// QUIC client config: accept any server certificate (the token handshake, not
/// the certificate, authenticates the peer).
#[cfg(feature = "udp")]
fn client_config(tuning: &UdpDirectTuning) -> Result<ClientConfig> {
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .context("failed to configure QUIC TLS")?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(SkipVerify))
    .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .context("invalid QUIC client crypto")?;
    let mut config = ClientConfig::new(Arc::new(quic));
    config.transport_config(Arc::new(transport_config(tuning)));
    Ok(config)
}

/// QUIC server config with a self-signed certificate.
#[cfg(feature = "udp")]
fn server_config(tuning: &UdpDirectTuning) -> Result<ServerConfig> {
    let cert = rcgen::generate_simple_self_signed(vec!["bore".to_string()])
        .context("failed to generate self-signed certificate")?;
    let cert_der = cert.cert.der().clone();
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());

    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .context("failed to configure QUIC TLS")?
    .with_no_client_auth()
    .with_single_cert(
        vec![cert_der],
        rustls::pki_types::PrivateKeyDer::Pkcs8(key_der),
    )
    .context("invalid QUIC server certificate")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .context("invalid QUIC server crypto")?;
    let mut config = ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(Arc::new(transport_config(tuning)));
    Ok(config)
}

/// A certificate verifier that accepts any server certificate. Safe here because
/// the peer is authenticated by the shared token, not by its certificate.
#[cfg(feature = "udp")]
#[derive(Debug)]
struct SkipVerify;

#[cfg(feature = "udp")]
impl rustls::client::danger::ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Fase 7 — the sprayed escape for the one cell an ordinary check round
/// cannot win (`docs/nat/NAT_SOTA_COMPARISON.md` §4.1).
///
/// THE CELL. The measured §6 matrix (`scripts/udp_nat_netns_test.sh`) says a
/// pair with exactly one symmetric side goes direct or relays according to the
/// NON-symmetric side's filtering, and the one combination that relays is
/// `eim:apdf × edm`: a side with a stable, endpoint-independent mapping that
/// nevertheless filters per address+port, facing a side whose source port
/// nobody can predict. Neither ordinary mechanism reaches it — port prediction
/// has nothing to predict against `fully-random`, and the check round's
/// probes go to addresses that, by construction, are the wrong port.
///
/// THE ESCAPE. It is a rendezvous in the port space, and it works because the
/// two sides are asymmetric in exactly complementary ways:
///
/// * the EASY side (stable mapping) sprays an authenticated check request at
///   many destination ports of the peer's public IP. Every one of those
///   packets opens its own NAT's filter for that `(peer ip, port)` pair —
///   which is precisely the direction that was blocked — and costs it nothing,
///   because its own source port is the same for all of them;
/// * the HARD side (symmetric mapping) opens many auxiliary sockets and sends
///   from each to the easy side's known, stable address. Each socket buys one
///   more external port in the draw.
///
/// A single collision between the sprayed set and the drawn set opens BOTH
/// directions at once: the easy side's packet reaches the hard side's socket
/// (its NAT has a mapping on that port whose reply tuple is the easy side's
/// stable address), and the hard side's reply reaches the easy side (whose
/// filter that same spray packet opened). That symmetry is why the escape does
/// not need a third party, a second round trip, or a retry protocol — the
/// first frame that arrives anywhere is the answer.
///
/// THE MATH. With `S` sprayed ports over a space of `P` and `Q` auxiliary
/// sockets, `p = 1 − (1 − S/P)^Q`. The defaults below (768 sprayed over
/// 64512, 256 sockets) give ≈ 95%, at a cost of about 2 300 datagrams of
/// 60 bytes across both sides and a few seconds of a budget that would
/// otherwise have been spent falling back to the relay. It is deliberately
/// NOT used for a hard × hard pair: there the easy side's stable port does not
/// exist, both sets are drawn, and reaching 99.9% needs on the order of
/// 170 000 probes (≈ 28 minutes at 100 pkt/s) — priced and rejected in the
/// SOTA comparison.
///
/// WHAT IT COSTS WHEN IT FAILS. Nothing that was not already lost: the escape
/// runs only after a DRY check round on a pair the server already planned
/// relay-first, the relay stays warm throughout, and a failed escape falls
/// through to exactly the QUIC attempt that would have run without it.
#[cfg(feature = "udp")]
pub mod spray {
    use super::*;

    /// Ports the spray may use. Below 1024 is privileged and no NAT allocates
    /// there; the space is the denominator of the birthday math.
    const PORT_LO: u32 = 1024;
    const PORT_HI: u32 = 65535;
    const PORT_SPACE: u32 = PORT_HI - PORT_LO + 1;

    /// Below this many auxiliary sockets the draw is too thin to be worth the
    /// window, so the escape declines rather than spending it (P < 20% at the
    /// default spray size).
    const MIN_AUX_SOCKETS: usize = 32;

    /// Which half of the escape this peer plays. Decided by the broker (the
    /// only party that sees both NAT profiles) and carried on
    /// `UdpAdaptivePlan::spray_role`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SprayRole {
        /// Endpoint-independent mapping: one stable external port. Sprays
        /// destination ports from the socket the round already used — the same
        /// socket on purpose, because its stability is the whole asset.
        Easy,
        /// Endpoint-dependent (symmetric) mapping: a fresh external port per
        /// destination, so no peer can aim at it. Opens auxiliary sockets.
        Hard,
    }

    /// Parse the wire value. An unknown string is `None` — same rule as the
    /// plan's `reason_code`: a newer broker must be able to name a role this
    /// binary predates without that being an error.
    pub fn role_from_wire(value: Option<&str>) -> Option<SprayRole> {
        match value? {
            crate::adaptive_nat::SPRAY_ROLE_EASY => Some(SprayRole::Easy),
            crate::adaptive_nat::SPRAY_ROLE_HARD => Some(SprayRole::Hard),
            _ => None,
        }
    }

    /// Sizing for one escape. Every field has an env override because the one
    /// honest way to gate this mechanism is to drive the collision probability
    /// to ~1 in a harness and observe the path flip, and the defaults that are
    /// right on a real network (bounded burst, bounded conntrack footprint)
    /// are not the ones that make a test deterministic. Same precedent as
    /// `BORE_CTRL_HEARTBEAT_MS` and `BORE_DIRECT_OPEN_TIMEOUT_MS`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SprayTuning {
        /// Distinct destination ports the easy side sprays per pass.
        pub ports_per_pass: usize,
        /// Passes, each with a FRESH random port set. The union is what counts:
        /// the easy side's filter stays open for every port it has already
        /// sprayed (conntrack keeps the entry far longer than this window), so
        /// pass `n` adds to the coverage of passes `1..n` instead of repeating
        /// it. Multiple passes also absorb start skew between the two sides.
        pub passes: usize,
        /// Auxiliary sockets the hard side opens.
        pub sockets: usize,
        /// Gap between two sprayed packets, also the easy side's listen slot.
        pub pace: Duration,
        /// How often each auxiliary socket re-sends. Its mapping only has to
        /// EXIST (conntrack's unreplied UDP timeout is 30 s), so this is
        /// anti-loss insurance, not a keepalive.
        pub repeat: Duration,
        /// Hard cap on the whole escape, both sides.
        pub cap: Duration,
        /// Accept loopback candidates as escape targets.
        ///
        /// False everywhere but an in-process test. On a real network a
        /// loopback candidate can never be the far side of a NAT, so spraying
        /// 768 ports at `127.0.0.1` would be noise — but on loopback the
        /// rendezvous is exactly the same mechanism with the NAT's port
        /// allocation replaced by the kernel's ephemeral range, which is what
        /// makes an honest end-to-end gate possible without a kernel harness.
        pub loopback_candidates: bool,
    }

    impl Default for SprayTuning {
        fn default() -> Self {
            Self {
                ports_per_pass: 256,
                passes: 3,
                sockets: 256,
                pace: Duration::from_micros(3_000),
                repeat: Duration::from_millis(2_500),
                cap: Duration::from_secs(6),
                loopback_candidates: false,
            }
        }
    }

    impl SprayTuning {
        /// Defaults with the `BORE_UDP_SPRAY_*` overrides applied. Read per
        /// call, never cached: a harness sets them per process and a bad value
        /// is ignored rather than fatal.
        pub fn from_env() -> Self {
            let d = Self::default();
            Self {
                ports_per_pass: env_usize("BORE_UDP_SPRAY_PORTS", d.ports_per_pass),
                passes: env_usize("BORE_UDP_SPRAY_PASSES", d.passes).max(1),
                sockets: env_usize("BORE_UDP_SPRAY_SOCKETS", d.sockets),
                pace: Duration::from_micros(env_u64("BORE_UDP_SPRAY_PACE_US", 3_000)),
                repeat: Duration::from_millis(env_u64("BORE_UDP_SPRAY_REPEAT_MS", 2_500)),
                cap: Duration::from_millis(env_u64("BORE_UDP_SPRAY_CAP_MS", 6_000)),
                loopback_candidates: false,
            }
        }

        /// Whether this sizing can win anything at all.
        ///
        /// Zero budget, zero ports or zero sockets is the off switch — a
        /// harness needs to measure the ordinary round on a cell the escape
        /// would otherwise flip, and an operator whose router dislikes a
        /// bounded burst needs the same. (The broader kill switch is
        /// server-side: `--no-udp-adaptive-plan` withholds the plan, and the
        /// role rides the plan.)
        pub fn enabled(&self) -> bool {
            self.cap > Duration::ZERO && self.ports_per_pass > 0 && self.sockets > 0
        }

        /// `1 − (1 − S/P)^Q`: the probability that at least one sprayed port
        /// coincides with one of the auxiliary sockets' external ports.
        ///
        /// Reported in the logs because an operator reading "the escape did
        /// not find a pair" deserves to know whether it was unlucky or
        /// under-provisioned, and pinned by a unit test because these are the
        /// numbers the SOTA comparison quotes.
        pub fn collision_probability(&self) -> f64 {
            let sprayed = (self.ports_per_pass.saturating_mul(self.passes)) as f64;
            let space = f64::from(PORT_SPACE);
            let miss = 1.0 - (sprayed.min(space) / space);
            1.0 - miss.powi(self.sockets.min(i32::MAX as usize) as i32)
        }
    }

    fn env_usize(key: &str, default: usize) -> usize {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(default)
    }

    fn env_u64(key: &str, default: u64) -> u64 {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default)
    }

    /// `n` distinct ports drawn uniformly from the allocatable space.
    ///
    /// Uniform on purpose, and NOT biased toward the peer's observed port: a
    /// NAT whose allocation is near-sequential is already covered by the
    /// `PREDICT_RANGE` candidates the ordinary round probes, so biasing here
    /// would re-spend the budget on the cell that is already handled and leave
    /// the `fully-random` cell — the one this escape exists for — thinner.
    pub fn random_ports(n: usize) -> Vec<u16> {
        let n = n.min(PORT_SPACE as usize);
        let mut seen = std::collections::HashSet::with_capacity(n);
        let mut out = Vec::with_capacity(n);
        // Bounded: with n ≤ 2048 against 64512 slots the expected number of
        // draws is barely above n, and the cap makes the worst case finite
        // regardless.
        let mut draws = 0usize;
        while out.len() < n && draws < n.saturating_mul(8) + 64 {
            draws += 1;
            let port = (PORT_LO + fastrand::u32(0..PORT_SPACE)) as u16;
            if seen.insert(port) {
                out.push(port);
            }
        }
        out
    }

    /// Distinct peer IPs worth spraying, most-preferred first, capped at two.
    ///
    /// The same port set is sprayed at EVERY address returned here, so the
    /// per-address collision probability is preserved and the packet count is
    /// multiplied — which is exactly why the list is capped rather than
    /// unbounded. Two covers the shapes that actually occur (a reflexive
    /// address plus a router-mapped or same-LAN one); a peer that offered
    /// sixteen candidates would otherwise turn a bounded burst into a
    /// sixteen-fold one.
    fn spray_ips(peers: &[SocketAddr], t: &SprayTuning) -> Vec<IpAddr> {
        let mut out: Vec<IpAddr> = Vec::new();
        for addr in peers {
            let ip = addr.ip();
            if !is_routable_v4(ip, t.loopback_candidates) || out.contains(&ip) {
                continue;
            }
            out.push(ip);
            if out.len() == 2 {
                break;
            }
        }
        out
    }

    /// Distinct peer addresses the hard side sends to, capped for the same
    /// reason [`spray_ips`] is: each auxiliary socket writes to every target,
    /// so an extra target multiplies the packet count without adding a single
    /// ticket to the draw (the draw is the number of SOCKETS).
    fn aux_targets(peers: &[SocketAddr], t: &SprayTuning) -> Vec<SocketAddr> {
        let mut out: Vec<SocketAddr> = Vec::new();
        for addr in peers {
            if !is_routable_v4(addr.ip(), t.loopback_candidates) || out.contains(addr) {
                continue;
            }
            out.push(*addr);
            if out.len() == 2 {
                break;
            }
        }
        out
    }

    fn is_routable_v4(ip: IpAddr, loopback: bool) -> bool {
        match ip {
            IpAddr::V4(v4) => {
                (loopback || !v4.is_loopback())
                    && !v4.is_unspecified()
                    && !v4.is_link_local()
                    && !v4.is_broadcast()
                    && !v4.is_multicast()
            }
            // IPv6 has no NAT cell to escape from (Fase 4 scope).
            IpAddr::V6(_) => false,
        }
    }

    /// How many transaction ids one auxiliary socket remembers.
    ///
    /// Peer-controlled input, so it is bounded: a legitimate spray hits any
    /// one socket a handful of times, and a peer that manages hundreds is not
    /// one this socket is going to win with anyway.
    const AUX_SEEN_CAP: usize = 256;

    /// How many copies of the confirm the easy side sends.
    ///
    /// The confirm is what makes the rendezvous single-valued (see
    /// [`easy_accept`]), so losing it costs the escape — but it travels a path
    /// that has just been proven in both directions, so three copies is
    /// insurance, not a retry protocol.
    const CONFIRM_COPIES: usize = 3;

    /// Easy side: authenticate one datagram and, if it is a genuine peer
    /// frame, ANNOUNCE the decision before reporting it.
    ///
    /// The announcement is the whole reason this is not a one-liner. With a
    /// generous draw the two sides collide on SEVERAL ports at once, and each
    /// side would otherwise keep whichever arrived first — two different
    /// firsts, two different pairs, a dial into a socket nobody is listening
    /// on. Measured immediately by the loopback gate, where the collision
    /// count is high by construction.
    ///
    /// So the easy side decides alone and says so: it replies to the frame
    /// that reached it and then sends [`CONFIRM_COPIES`] requests carrying the
    /// transaction id the peer has ALREADY seen. A sprayed request always
    /// carries a fresh 96-bit id, so "an id I have seen before" is a
    /// discriminator the spray itself can never counterfeit, and exactly one
    /// auxiliary socket can ever receive it.
    async fn easy_accept(
        socket: &UdpSocket,
        cfg: &CheckConfig,
        buf: &[u8],
        from: SocketAddr,
    ) -> Option<SocketAddr> {
        let frame = check::parse(&cfg.key, buf)?;
        if frame.generation != cfg.generation || frame.role == cfg.role.byte() {
            return None;
        }
        if frame.kind == check::KIND_REQUEST {
            // An auxiliary socket reached us first: its probe got through
            // because a spray packet had already opened our filter for it.
            let reply =
                check::response(&cfg.key, cfg.role.byte(), cfg.generation, &frame.txid, from);
            let _ = socket.send_to(&reply, from).await;
        }
        let confirm = check::request(&cfg.key, cfg.role.byte(), cfg.generation, &frame.txid);
        for _ in 0..CONFIRM_COPIES {
            let _ = socket.send_to(&confirm, from).await;
        }
        Some(from)
    }

    /// Hard side: one auxiliary socket's view of one datagram.
    ///
    /// Promotes ONLY on a confirm — a request carrying a transaction id this
    /// socket has already seen, either because it answered it or because it
    /// generated it. Everything else is answered where the protocol says to
    /// answer and otherwise ignored, including a response to this socket's own
    /// probe: being seen is not the same as being chosen, and acting on it is
    /// exactly the disagreement the confirm exists to prevent.
    fn aux_accept(
        cfg: &CheckConfig,
        seen: &mut std::collections::HashSet<[u8; 12]>,
        buf: &[u8],
    ) -> Option<AuxAction> {
        let frame = check::parse(&cfg.key, buf)?;
        if frame.generation != cfg.generation || frame.role == cfg.role.byte() {
            return None;
        }
        match frame.kind {
            check::KIND_REQUEST if seen.contains(&frame.txid) => Some(AuxAction::Promote),
            // Full memory ⇒ stop answering NEW ids, for two reasons that
            // happen to coincide. An id this socket cannot remember is one
            // whose confirm it could not recognise, so answering it is a
            // reply it can never act on; and the answer is the same size as
            // the request, so an unbounded answer loop is a 1:1 reflector —
            // the property `CHECK_MAX_RESPONSES` caps on the ordinary round,
            // capped here by the same number.
            check::KIND_REQUEST if seen.len() >= AUX_SEEN_CAP => None,
            check::KIND_REQUEST => Some(AuxAction::Answer(frame.txid)),
            _ => None,
        }
    }

    /// What an auxiliary socket does with an authenticated frame.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum AuxAction {
        /// Answer this transaction id and remember it.
        Answer([u8; 12]),
        /// The easy side confirmed this socket: it is the pair.
        Promote,
    }

    /// Read this socket until `budget` runs out, returning the first
    /// authenticated peer source the easy side accepts.
    async fn listen(
        socket: &UdpSocket,
        cfg: &CheckConfig,
        buf: &mut [u8],
        budget: Duration,
    ) -> Option<SocketAddr> {
        let deadline = Instant::now() + budget;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let (n, from) = match timeout(left, socket.recv_from(buf)).await {
                Ok(Ok(v)) => v,
                // A recv error is TRANSIENT here, and treating it as fatal
                // broke the escape outright on Windows. The spray sends to
                // hundreds of ports of which at most one is open, so almost
                // every packet earns an ICMP port-unreachable; Windows
                // surfaces that on the SENDING socket's next `recv_from` as
                // `WSAECONNRESET`, and Linux does the same as `ECONNREFUSED`
                // once a socket is connected. So the one datagram that proves
                // the escape worked arrives on a socket whose recv queue is
                // full of errors caused by the escape's own probes: a single
                // `return` there discards it. Same precedent, and same reason,
                // as `recv_actor` above — pause briefly so a queued storm
                // cannot become a busy-spin, and let the deadline end the loop.
                Ok(Err(_)) => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                }
                Err(_) => return None,
            };
            if let Some(hit) = easy_accept(socket, cfg, &buf[..n], from).await {
                return Some(hit);
            }
        }
    }

    /// Easy side: spray destination ports from the round's own socket.
    ///
    /// Takes the socket by reference and never replaces it — its stability is
    /// the asset the whole escape is built on, and the caller hands the SAME
    /// socket to QUIC afterwards.
    pub async fn easy_side(
        socket: &UdpSocket,
        peers: &[SocketAddr],
        cfg: &CheckConfig,
        t: &SprayTuning,
    ) -> Option<SocketAddr> {
        let ips = spray_ips(peers, t);
        if ips.is_empty() {
            debug!("sprayed escape skipped: no routable peer IP among the candidates");
            return None;
        }
        let started = Instant::now();
        info!(
            ?ips,
            ports_per_pass = t.ports_per_pass,
            passes = t.passes,
            p_collision = format!("{:.0}%", t.collision_probability() * 100.0),
            "sprayed escape: spraying destination ports (easy side)"
        );
        let mut buf = [0u8; 2048];
        for _ in 0..t.passes {
            if started.elapsed() >= t.cap {
                break;
            }
            for port in random_ports(t.ports_per_pass) {
                if started.elapsed() >= t.cap {
                    break;
                }
                for ip in &ips {
                    let frame = check::request(
                        &cfg.key,
                        cfg.role.byte(),
                        cfg.generation,
                        &check::new_txid(),
                    );
                    let _ = socket.send_to(&frame, SocketAddr::new(*ip, port)).await;
                }
                // Listen DURING the pace slot rather than after the pass: the
                // answer can arrive at any moment and a slot spent sleeping is
                // a slot not spent reading.
                if let Some(hit) = listen(socket, cfg, &mut buf, t.pace).await {
                    return Some(hit);
                }
            }
        }
        // Drain the rest of the budget: the peer's own probe may still be in
        // flight behind the last packet we sent.
        let left = t.cap.saturating_sub(started.elapsed());
        listen(socket, cfg, &mut buf, left).await
    }

    /// Hard side: open auxiliary sockets and return the one that wins.
    ///
    /// The winner is returned BY VALUE, with its task already finished, so the
    /// caller can hand it to Quinn under the same one-socket-one-reader rule
    /// every other direct path obeys. The losers are dropped with their tasks.
    pub async fn hard_side(
        peers: &[SocketAddr],
        cfg: &CheckConfig,
        t: &SprayTuning,
    ) -> Option<(UdpSocket, SocketAddr)> {
        let targets = aux_targets(peers, t);
        if targets.is_empty() {
            debug!("sprayed escape skipped: no routable peer address among the candidates");
            return None;
        }
        let want = aux_socket_budget(t.sockets);
        if want < MIN_AUX_SOCKETS {
            warn!(
                requested = t.sockets,
                affordable = want,
                "sprayed escape declined: not enough file-descriptor headroom for a draw \
                 worth the window; staying on the ordinary fallback"
            );
            return None;
        }

        let mut set = tokio::task::JoinSet::new();
        let mut opened = 0usize;
        for _ in 0..want {
            // Plain bind, NOT `bind_socket`: these sockets carry 60-byte
            // frames and never QUIC, so the multi-megabyte buffer request
            // would be pure waste times N — and the winner is handed to an
            // endpoint constructor, which is where P-13 says the buffers are
            // configured anyway. Ephemeral port, no `SO_REUSEADDR` (the
            // direct-path rule; nothing here co-binds a fixed port).
            let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
                Ok(socket) => socket,
                Err(err) => {
                    debug!(%err, opened, "stopped opening auxiliary sockets");
                    break;
                }
            };
            opened += 1;
            let cfg = cfg.clone();
            let targets = targets.clone();
            let tuning = *t;
            set.spawn(async move { aux_socket_round(socket, targets, cfg, tuning).await });
        }
        if opened < MIN_AUX_SOCKETS {
            warn!(
                opened,
                "sprayed escape declined: could only open {opened} auxiliary sockets"
            );
            set.shutdown().await;
            return None;
        }
        info!(
            sockets = opened,
            ?targets,
            "sprayed escape: drawing external ports (hard side)"
        );

        let deadline = Instant::now() + t.cap;
        let mut winner = None;
        while winner.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match timeout(left, set.join_next()).await {
                // Every socket finished without a hit.
                Ok(None) => break,
                Ok(Some(Ok(Some(hit)))) => winner = Some(hit),
                // A loser, or a task that panicked: keep waiting for the rest.
                Ok(Some(_)) => {}
                Err(_) => break,
            }
        }
        // Losers are aborted and their sockets released here, before the
        // winner is handed on — a leftover reader on a leftover socket is the
        // shape of every direct-path bug this codebase has had.
        set.shutdown().await;
        winner
    }

    /// One auxiliary socket's whole life: punch the targets, listen, answer.
    async fn aux_socket_round(
        socket: UdpSocket,
        targets: Vec<SocketAddr>,
        cfg: CheckConfig,
        t: SprayTuning,
    ) -> Option<(UdpSocket, SocketAddr)> {
        let deadline = Instant::now() + t.cap;
        let mut buf = [0u8; 2048];
        let mut next_send = Instant::now();
        // Transaction ids this socket has taken part in — the ones it
        // generated and the ones it answered. A confirm is a request carrying
        // one of them.
        let mut seen: std::collections::HashSet<[u8; 12]> = std::collections::HashSet::new();
        loop {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            if now >= next_send {
                for target in &targets {
                    let txid = check::new_txid();
                    let frame = check::request(&cfg.key, cfg.role.byte(), cfg.generation, &txid);
                    if socket.send_to(&frame, *target).await.is_ok() {
                        seen.insert(txid);
                    }
                }
                next_send = now + t.repeat;
            }
            let left = next_send
                .min(deadline)
                .saturating_duration_since(Instant::now());
            match timeout(left, socket.recv_from(&mut buf)).await {
                Ok(Ok((n, from))) => match aux_accept(&cfg, &mut seen, &buf[..n]) {
                    Some(AuxAction::Promote) => return Some((socket, from)),
                    Some(AuxAction::Answer(txid)) => {
                        let reply =
                            check::response(&cfg.key, cfg.role.byte(), cfg.generation, &txid, from);
                        let _ = socket.send_to(&reply, from).await;
                        seen.insert(txid);
                    }
                    None => {}
                },
                // Transient, for exactly the reason `listen` documents: this
                // socket's own probes go to ports that are almost all closed,
                // and the resulting ICMP errors are delivered here. Abandoning
                // the socket on one of them throws away the auxiliary port
                // whose draw may already have been won.
                Ok(Err(_)) => tokio::time::sleep(Duration::from_millis(5)).await,
                // Pace slot elapsed: loop around and re-send.
                Err(_) => {}
            }
        }
    }

    /// How many auxiliary sockets this process can afford.
    ///
    /// Never more than half the remaining descriptors: the sockets are a
    /// transient experiment inside a process that is also serving a live
    /// tunnel, and `EMFILE` lands on `accept()` for EVERY listener the process
    /// owns (P-12) — spending the last descriptors on a 63%-shot would trade a
    /// working relay for a broken server.
    fn aux_socket_budget(requested: usize) -> usize {
        match crate::fdlimit::fd_headroom() {
            Some(head) => requested.min((head / 2) as usize),
            None => requested,
        }
    }
}

/// Authenticated connectivity-check frames (plan Fase 2). Not STUN: a fixed
/// 60-byte binary frame, HMAC-authenticated with a key derived from the
/// direct-path token ([`derive_check_key`]).
///
/// Layout (both kinds, always exactly [`check::FRAME_LEN`] bytes — a response
/// is never larger than a request, so the responder cannot amplify):
///
/// ```text
///  0..4    magic  b"bcc1"
///  4       kind   1 = request, 2 = response
///  5       role   1 = listener, 2 = dialer   (sender's role)
///  6..10   generation (u32 BE)
/// 10..22   transaction id (12 bytes; response echoes the request's)
/// 22..28   observed source: IPv4 (4) + port (2 BE); zero in requests
/// 28..60   HMAC-SHA256(key, bytes 0..28)
/// ```
pub mod check {
    use super::*;

    /// Total frame length, both kinds.
    pub const FRAME_LEN: usize = 60;
    const MAGIC: &[u8; 4] = b"bcc1";
    /// Frame kinds.
    pub const KIND_REQUEST: u8 = 1;
    /// See [`KIND_REQUEST`].
    pub const KIND_RESPONSE: u8 = 2;
    /// Sender roles.
    pub const ROLE_LISTENER: u8 = 1;
    /// See [`ROLE_LISTENER`].
    pub const ROLE_DIALER: u8 = 2;
    const HMAC_AT: usize = 28;

    /// A parsed, HMAC-verified frame.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Frame {
        /// [`KIND_REQUEST`] or [`KIND_RESPONSE`].
        pub kind: u8,
        /// Sender's role byte.
        pub role: u8,
        /// Traversal round generation.
        pub generation: u32,
        /// Transaction id (a response echoes the request's).
        pub txid: [u8; 12],
        /// Observed source (responses only; `None` when zeroed/non-IPv4).
        pub observed: Option<SocketAddr>,
    }

    /// Cheap shape test (magic + length) used by the recv actor to decide
    /// whether a datagram should be counted as an (in)valid check frame or as
    /// ordinary peer traffic. Deliberately does NOT authenticate.
    pub fn looks_like(buf: &[u8]) -> bool {
        buf.len() == FRAME_LEN && &buf[..4] == MAGIC
    }

    /// Fresh random transaction id from the system CSPRNG.
    pub fn new_txid() -> [u8; 12] {
        use ring::rand::{SecureRandom, SystemRandom};
        let mut txid = [0u8; 12];
        SystemRandom::new()
            .fill(&mut txid)
            .expect("system CSPRNG must not fail");
        txid
    }

    fn build(
        key: &[u8; 32],
        kind: u8,
        role: u8,
        generation: u32,
        txid: &[u8; 12],
        observed: Option<std::net::SocketAddrV4>,
    ) -> Vec<u8> {
        let mut frame = Vec::with_capacity(FRAME_LEN);
        frame.extend_from_slice(MAGIC);
        frame.push(kind);
        frame.push(role);
        frame.extend_from_slice(&generation.to_be_bytes());
        frame.extend_from_slice(txid);
        match observed {
            Some(v4) => {
                frame.extend_from_slice(&v4.ip().octets());
                frame.extend_from_slice(&v4.port().to_be_bytes());
            }
            None => frame.extend_from_slice(&[0u8; 6]),
        }
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(&frame);
        frame.extend_from_slice(&mac.finalize().into_bytes());
        debug_assert_eq!(frame.len(), FRAME_LEN);
        frame
    }

    /// Build an authenticated check request.
    pub fn request(key: &[u8; 32], role: u8, generation: u32, txid: &[u8; 12]) -> Vec<u8> {
        build(key, KIND_REQUEST, role, generation, txid, None)
    }

    /// Build the authenticated response to a request: echoes the transaction
    /// id and reports the request's observed source address. Non-IPv4 sources
    /// are zeroed (IPv6 is out of scope for the direct path).
    pub fn response(
        key: &[u8; 32],
        role: u8,
        generation: u32,
        txid: &[u8; 12],
        observed: SocketAddr,
    ) -> Vec<u8> {
        let v4 = match observed {
            SocketAddr::V4(v4) => Some(v4),
            SocketAddr::V6(_) => None,
        };
        build(key, KIND_RESPONSE, role, generation, txid, v4)
    }

    /// Parse + authenticate a frame. `None` on ANY mismatch (length, magic,
    /// kind, HMAC) — the caller must not answer, only count.
    pub fn parse(key: &[u8; 32], buf: &[u8]) -> Option<Frame> {
        if !looks_like(buf) {
            return None;
        }
        let kind = buf[4];
        if kind != KIND_REQUEST && kind != KIND_RESPONSE {
            return None;
        }
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(&buf[..HMAC_AT]);
        // Constant-time verification via the hmac crate.
        mac.verify_slice(&buf[HMAC_AT..]).ok()?;

        let role = buf[5];
        let generation = u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]);
        let txid: [u8; 12] = buf[10..22].try_into().ok()?;
        let octets: [u8; 4] = buf[22..26].try_into().ok()?;
        let port = u16::from_be_bytes([buf[26], buf[27]]);
        let ip = Ipv4Addr::from(octets);
        let observed = if ip.is_unspecified() && port == 0 {
            None
        } else {
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        };
        Some(Frame {
            kind,
            role,
            generation,
            txid,
            observed,
        })
    }
}

/// Minimal STUN (RFC 5389) binding-request client and a server responder, used
/// for reflexive-address discovery. Only XOR-MAPPED-ADDRESS is supported.
pub mod stun {
    use super::*;
    use std::net::{Ipv6Addr, SocketAddrV4, SocketAddrV6};

    const MAGIC_COOKIE: u32 = 0x2112_A442;
    const BINDING_REQUEST: u16 = 0x0001;
    const BINDING_SUCCESS: u16 = 0x0101;
    const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
    /// RFC 5780 §7.2. A 4-byte flags word; only two bits are defined.
    const ATTR_CHANGE_REQUEST: u16 = 0x0003;
    /// RFC 5780 §7.4: the server's OTHER address, i.e. the one a
    /// change-request would answer from. Its presence is how a client learns
    /// the server supports behaviour discovery at all.
    const ATTR_OTHER_ADDRESS: u16 = 0x802C;
    /// RFC 5780 §7.3: the address this very response was sent FROM. It is what
    /// lets the client verify that the server really did change ports rather
    /// than answering normally.
    const ATTR_RESPONSE_ORIGIN: u16 = 0x802B;
    const CHANGE_IP: u8 = 0x04;
    const CHANGE_PORT: u8 = 0x02;

    /// Build a STUN binding request, returning the bytes and the transaction id.
    pub fn binding_request() -> (Vec<u8>, [u8; 12]) {
        use ring::rand::{SecureRandom, SystemRandom};
        let mut txid = [0u8; 12];
        SystemRandom::new()
            .fill(&mut txid)
            .expect("system CSPRNG must not fail");
        let mut msg = Vec::with_capacity(20);
        msg.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes()); // message length: no attributes
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&txid);
        (msg, txid)
    }

    /// Build a STUN binding request carrying a RFC 5780 CHANGE-REQUEST that
    /// asks the server to answer from its OTHER port (same IP).
    ///
    /// This is the probe that measures the FILTERING axis, which no amount of
    /// plain binding requests can reach: the response arrives from a
    /// `(ip, port)` the client has never sent to, so whether it gets back in
    /// is exactly the question "does this NAT admit a new port from an address
    /// I have already talked to?". A NAT that lets it through filters at most
    /// by address (ADF, or endpoint-independent); one that drops it filters by
    /// address AND port (APDF), which is the typical home router and the
    /// reason a symmetric peer on the other side cannot be reached.
    ///
    /// Distinguishing ADF from EIF additionally needs the server to answer
    /// from a different IP, which a single-homed deployment cannot do; the
    /// caller reports the pair as "address-dependent or looser" rather than
    /// guessing.
    pub fn binding_request_change_port() -> (Vec<u8>, [u8; 12]) {
        let (mut msg, txid) = binding_request();
        // Message length covers the attributes only: 4 header + 4 value.
        msg[2..4].copy_from_slice(&8u16.to_be_bytes());
        msg.extend_from_slice(&ATTR_CHANGE_REQUEST.to_be_bytes());
        msg.extend_from_slice(&4u16.to_be_bytes());
        msg.extend_from_slice(&[0, 0, 0, CHANGE_PORT]);
        (msg, txid)
    }

    /// The CHANGE-REQUEST flags of a request, as `(change_ip, change_port)`.
    /// A request without the attribute reads `(false, false)`.
    pub fn parse_change_request(request: &[u8]) -> (bool, bool) {
        let mut pos = 20;
        while pos + 4 <= request.len() {
            let attr_type = u16::from_be_bytes([request[pos], request[pos + 1]]);
            let attr_len = u16::from_be_bytes([request[pos + 2], request[pos + 3]]) as usize;
            let value_start = pos + 4;
            if value_start + attr_len > request.len() {
                return (false, false);
            }
            if attr_type == ATTR_CHANGE_REQUEST && attr_len >= 4 {
                let flags = request[value_start + 3];
                return (flags & CHANGE_IP != 0, flags & CHANGE_PORT != 0);
            }
            pos = value_start + attr_len.div_ceil(4) * 4;
        }
        (false, false)
    }

    /// The OTHER-ADDRESS of a binding response, when the server published one.
    /// `None` means the server does not implement RFC 5780 behaviour
    /// discovery, which is a different answer from "the probe was filtered"
    /// and must never be reported as one.
    pub fn parse_other_address(buf: &[u8]) -> Option<SocketAddr> {
        parse_plain_address_attr(buf, ATTR_OTHER_ADDRESS)
    }

    /// The RESPONSE-ORIGIN of a binding response: the address it was sent
    /// from, as the SERVER understands it.
    pub fn parse_response_origin(buf: &[u8]) -> Option<SocketAddr> {
        parse_plain_address_attr(buf, ATTR_RESPONSE_ORIGIN)
    }

    /// OTHER-ADDRESS and RESPONSE-ORIGIN use the plain (NOT xor-ed) MAPPED
    /// ADDRESS encoding of RFC 5389 §15.1.
    fn parse_plain_address_attr(buf: &[u8], want: u16) -> Option<SocketAddr> {
        if buf.len() < 20 || u16::from_be_bytes([buf[0], buf[1]]) != BINDING_SUCCESS {
            return None;
        }
        let mut pos = 20;
        while pos + 4 <= buf.len() {
            let attr_type = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
            let attr_len = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
            let value_start = pos + 4;
            if value_start + attr_len > buf.len() {
                return None;
            }
            if attr_type == want && attr_len >= 8 && buf[value_start + 1] == 0x01 {
                let port = u16::from_be_bytes([buf[value_start + 2], buf[value_start + 3]]);
                let addr = Ipv4Addr::from(u32::from_be_bytes([
                    buf[value_start + 4],
                    buf[value_start + 5],
                    buf[value_start + 6],
                    buf[value_start + 7],
                ]));
                return Some(SocketAddr::V4(SocketAddrV4::new(addr, port)));
            }
            pos = value_start + attr_len.div_ceil(4) * 4;
        }
        None
    }

    /// Encode one plain (non-xor) address attribute.
    fn push_plain_address(msg: &mut Vec<u8>, attr: u16, addr: SocketAddrV4) {
        msg.extend_from_slice(&attr.to_be_bytes());
        msg.extend_from_slice(&8u16.to_be_bytes());
        msg.push(0); // reserved
        msg.push(0x01); // family: IPv4
        msg.extend_from_slice(&addr.port().to_be_bytes());
        msg.extend_from_slice(&u32::from(*addr.ip()).to_be_bytes());
    }

    /// Extract the transaction id from a STUN binding success response
    /// (message type and magic cookie checked). Used by the traversal
    /// socket's recv actor to demultiplex concurrent transactions before
    /// full parsing.
    pub fn response_txid(buf: &[u8]) -> Option<[u8; 12]> {
        if buf.len() < 20 {
            return None;
        }
        if u16::from_be_bytes([buf[0], buf[1]]) != BINDING_SUCCESS {
            return None;
        }
        if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != MAGIC_COOKIE {
            return None;
        }
        buf[8..20].try_into().ok()
    }

    /// Parse the XOR-MAPPED-ADDRESS from a STUN binding success response.
    pub fn parse_response(buf: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
        if buf.len() < 20 {
            return None;
        }
        let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
        if msg_type != BINDING_SUCCESS {
            return None;
        }
        if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != MAGIC_COOKIE {
            return None;
        }
        if &buf[8..20] != txid {
            return None;
        }
        let mut pos = 20;
        while pos + 4 <= buf.len() {
            let attr_type = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
            let attr_len = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
            let value_start = pos + 4;
            if value_start + attr_len > buf.len() {
                return None;
            }
            if attr_type == ATTR_XOR_MAPPED_ADDRESS {
                return parse_xor_mapped(&buf[value_start..value_start + attr_len], txid);
            }
            // Attributes are padded to a 4-byte boundary.
            pos = value_start + attr_len.div_ceil(4) * 4;
        }
        None
    }

    fn parse_xor_mapped(value: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
        if value.len() < 4 {
            return None;
        }
        let family = value[1];
        let xport = u16::from_be_bytes([value[2], value[3]]);
        let port = xport ^ (MAGIC_COOKIE >> 16) as u16;
        match family {
            0x01 if value.len() >= 8 => {
                let xaddr = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
                let addr = Ipv4Addr::from(xaddr ^ MAGIC_COOKIE);
                Some(SocketAddr::V4(SocketAddrV4::new(addr, port)))
            }
            0x02 if value.len() >= 20 => {
                let mut key = [0u8; 16];
                key[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                key[4..].copy_from_slice(txid);
                let mut addr = [0u8; 16];
                for i in 0..16 {
                    addr[i] = value[4 + i] ^ key[i];
                }
                Some(SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::from(addr),
                    port,
                    0,
                    0,
                )))
            }
            _ => None,
        }
    }

    /// Build a STUN binding success response echoing `source` as a
    /// XOR-MAPPED-ADDRESS. Only IPv4 sources are encoded.
    pub fn binding_response(request: &[u8], source: SocketAddr) -> Option<Vec<u8>> {
        binding_response_full(request, source, None, None)
    }

    /// The same, plus the two RFC 5780 attributes a behaviour-discovery client
    /// needs: `origin` is the address this response leaves from, and `other`
    /// is the server's alternate address — the one a CHANGE-REQUEST would be
    /// answered from.
    ///
    /// Publishing `other` is what tells the client the server can be asked the
    /// filtering question at all. Without it the client must report the
    /// filtering as UNKNOWN, never as restrictive: "the server cannot answer"
    /// and "the NAT dropped the answer" look identical on the wire and mean
    /// opposite things.
    pub fn binding_response_full(
        request: &[u8],
        source: SocketAddr,
        origin: Option<SocketAddr>,
        other: Option<SocketAddr>,
    ) -> Option<Vec<u8>> {
        if request.len() < 20
            || u16::from_be_bytes([request[0], request[1]]) != BINDING_REQUEST
            || u32::from_be_bytes([request[4], request[5], request[6], request[7]]) != MAGIC_COOKIE
        {
            return None;
        }
        let SocketAddr::V4(v4) = source else {
            return None;
        };
        let xport = v4.port() ^ (MAGIC_COOKIE >> 16) as u16;
        let xaddr = u32::from(*v4.ip()) ^ MAGIC_COOKIE;

        let origin_v4 = match origin {
            Some(SocketAddr::V4(a)) => Some(a),
            _ => None,
        };
        let other_v4 = match other {
            Some(SocketAddr::V4(a)) => Some(a),
            _ => None,
        };
        // 12 bytes for XOR-MAPPED-ADDRESS, 12 more for each optional address.
        let attr_len =
            12 + if origin_v4.is_some() { 12 } else { 0 } + if other_v4.is_some() { 12 } else { 0 };

        let mut msg = Vec::with_capacity(20 + attr_len);
        msg.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        msg.extend_from_slice(&(attr_len as u16).to_be_bytes()); // attribute length
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&request[8..20]); // echo transaction id
        msg.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        msg.extend_from_slice(&8u16.to_be_bytes()); // value length
        msg.push(0); // reserved
        msg.push(0x01); // family: IPv4
        msg.extend_from_slice(&xport.to_be_bytes());
        msg.extend_from_slice(&xaddr.to_be_bytes());
        if let Some(a) = origin_v4 {
            push_plain_address(&mut msg, ATTR_RESPONSE_ORIGIN, a);
        }
        if let Some(a) = other_v4 {
            push_plain_address(&mut msg, ATTR_OTHER_ADDRESS, a);
        }
        Some(msg)
    }
}

/// Run a minimal STUN responder on `socket`, replying to binding requests with
/// the observed source address. Lets a self-hosted bore server double as the
/// STUN server so no external infrastructure is required.
pub async fn run_stun_responder(socket: UdpSocket) {
    run_stun_responder_with_alt(Arc::new(socket), None).await
}

/// The same responder, plus RFC 5780 behaviour discovery when an ALTERNATE
/// socket is supplied.
///
/// The alternate socket is what turns a plain reflexive-address service into
/// one that can answer the FILTERING question. A binding request carrying
/// CHANGE-REQUEST(change-port) is answered FROM the alternate socket, so the
/// datagram reaches the client from an `ip:port` the client has never sent to
/// — which is precisely the packet a port-dependent filter drops and an
/// address-dependent one lets through. Without it, a client can measure its
/// NAT's MAPPING and nothing else, and mapping alone does not decide whether
/// a direct path is possible (measured: `eim:adf` and `eim:apdf` differ in
/// filtering ONLY, and only the first reaches a symmetric peer — see
/// `scripts/udp_nat_netns_test.sh`).
///
/// Both sockets publish OTHER-ADDRESS naming the other one. That attribute is
/// load-bearing for the CLIENT, not for the server: it is how a client tells
/// "this server cannot answer the question" apart from "my NAT ate the
/// answer", which look identical on the wire and mean opposite things. The
/// address published is whatever the socket is bound to, so on the usual
/// wildcard bind the IP reads `0.0.0.0` and only the PORT is informative —
/// the client resolves the address itself from the source of the datagram it
/// receives, which is the only trustworthy source anyway.
pub async fn run_stun_responder_with_alt(primary: Arc<UdpSocket>, alt: Option<Arc<UdpSocket>>) {
    let primary_addr = primary.local_addr().ok();
    let alt_addr = alt.as_ref().and_then(|a| a.local_addr().ok());
    if let Some(alt) = alt.clone() {
        // The alternate socket serves ordinary binding requests too, so a
        // client may probe it directly. It never changes ports itself: a
        // second hop would need a third socket and answers no new question.
        tokio::spawn(async move { stun_serve(alt, None, primary_addr).await });
    }
    stun_serve(primary, alt, alt_addr).await
}

/// One responder loop. `change_port` is the socket a CHANGE-REQUEST is
/// answered from (absent ⇒ the flag is ignored, which is exactly RFC 5780's
/// "server does not support it"); `other` is the address published as
/// OTHER-ADDRESS.
async fn stun_serve(
    listen: Arc<UdpSocket>,
    change_port: Option<Arc<UdpSocket>>,
    other: Option<SocketAddr>,
) {
    let origin = listen.local_addr().ok();
    let mut buf = [0u8; 512];
    loop {
        match listen.recv_from(&mut buf).await {
            Ok((n, from)) => {
                let (_change_ip, want_change_port) = stun::parse_change_request(&buf[..n]);
                // A change-IP request cannot be honoured by a single-homed
                // server, so it is answered normally rather than dropped:
                // the client compares the SOURCE of what it receives and will
                // read "same port" as "not supported", which is the truth.
                let sender: &Arc<UdpSocket> = match (want_change_port, &change_port) {
                    (true, Some(alt)) => alt,
                    _ => &listen,
                };
                let sent_from = sender.local_addr().ok().or(origin);
                if let Some(reply) = stun::binding_response_full(&buf[..n], from, sent_from, other)
                {
                    if sender.send_to(&reply, from).await.is_ok() {
                        debug!(%from, changed = want_change_port, "STUN reflexive address returned");
                    }
                }
            }
            Err(err) => {
                debug!(%err, "STUN responder recv error");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// The outcome of a NAT FILTERING probe (RFC 4787 §5, measured RFC 5780 §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterProbe {
    /// A datagram from an `ip:port` this host never sent to arrived: the
    /// filter keys on the ADDRESS at most (ADF), or not at all (EIF). One
    /// STUN IP cannot tell those two apart; both reach a symmetric peer,
    /// which is the distinction that decides a direct path.
    AddressDependentOrOpen,
    /// The probe was dropped: the filter keys on address AND port (APDF).
    /// This is the typical home router, and the reason a port-restricted
    /// provider cannot serve a symmetric/mobile consumer.
    AddressAndPortDependent,
    /// The question could not be asked — no STUN server on the chain
    /// published an OTHER-ADDRESS, or the one that did answered the
    /// change-request from its ordinary port. NEVER report this as a
    /// restrictive filter: an absent measurement and a blocked probe look the
    /// same from the client's socket and mean opposite things.
    Unsupported,
}

impl FilterProbe {
    /// Stable lowercase label for logs, reports and the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            FilterProbe::AddressDependentOrOpen => "adf-or-eif",
            FilterProbe::AddressAndPortDependent => "apdf",
            FilterProbe::Unsupported => "unknown",
        }
    }

    /// Human-readable rendering, single-sourced for every report.
    ///
    /// Both diagnostics — the standalone `bore test-udp` report below and the
    /// paired one in `udp_diagnostic` — render the measurement through this
    /// one function on purpose: an operator who runs both must not have to
    /// learn two vocabularies for the same axis. The wording never says
    /// "restricted" for an unmeasured probe, because an absent measurement and
    /// a blocked one look identical from this socket and mean the opposite.
    pub fn describe(self) -> &'static str {
        match self {
            FilterProbe::AddressAndPortDependent => {
                "address+port dependent (apdf) - unreachable by a symmetric peer"
            }
            FilterProbe::AddressDependentOrOpen => {
                "address dependent or open (adf/eif) - reachable by a symmetric peer"
            }
            FilterProbe::Unsupported => {
                "unknown (no RFC 5780 STUN server answered; run against `bore server --udp`)"
            }
        }
    }

    /// The coarser legacy classification carried in [`UdpNatProfile`].
    ///
    /// Only the BLOCKED outcome maps onto a legacy variant, and it maps onto
    /// the one whose documented meaning ("address- or port-dependent") it
    /// literally is. A probe that got through proves the filter is EIF *or*
    /// ADF and a single-homed server cannot say which, so claiming
    /// [`UdpNatFiltering::Eif`] would be a guess — the legacy field stays
    /// `Unknown` and the precise reading rides in the additive
    /// `filtering_probe` field beside it.
    pub fn legacy(self) -> crate::shared::UdpNatFiltering {
        match self {
            FilterProbe::AddressAndPortDependent => {
                crate::shared::UdpNatFiltering::AddressDependent
            }
            _ => crate::shared::UdpNatFiltering::Unknown,
        }
    }
}

/// How long one filtering probe waits for the alternate-port answer.
///
/// It must exceed a full RTT plus the server's own scheduling, and stay well
/// under the STUN chain budget: the probe is an EXTRA measurement and must
/// never delay the candidate gather it rides along with.
pub const FILTER_PROBE_TIMEOUT: Duration = Duration::from_millis(900);
/// Attempts before declaring the probe blocked. UDP loss on a clean path is
/// rare but not zero, and a single lost datagram would otherwise be reported
/// as a restrictive NAT — the expensive direction to be wrong in, because it
/// steers the punch plan toward the relay.
pub const FILTER_PROBE_ATTEMPTS: usize = 3;

/// Measure the FILTERING behaviour of the NAT in front of `socket`, using
/// `stun` as an RFC 5780 server.
///
/// The caller must already have a reflexive address from this very socket:
/// the probe is meaningless without an existing mapping, and creating one is
/// the baseline query's job, not this function's.
///
/// The classification reads the SOURCE of the datagram that comes back, never
/// the RESPONSE-ORIGIN attribute inside it. A server that ignores the change
/// request answers from its ordinary port while happily echoing whatever it
/// likes in its attributes; the 5-tuple the kernel reports cannot be forged by
/// the server and is the only thing that proves the packet really did arrive
/// from an address this host had never written to.
pub async fn probe_filtering(socket: &UdpSocket, stun: SocketAddr) -> FilterProbe {
    // Step 1: does this server implement behaviour discovery at all? Only an
    // OTHER-ADDRESS makes the negative result meaningful.
    let (request, txid) = stun::binding_request();
    let mut buf = [0u8; 512];
    let mut supported = false;
    for _ in 0..FILTER_PROBE_ATTEMPTS {
        if socket.send_to(&request, stun).await.is_err() {
            return FilterProbe::Unsupported;
        }
        match timeout(STUN_TIMEOUT, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) if from == stun => {
                if stun::parse_response(&buf[..n], &txid).is_some() {
                    supported = stun::parse_other_address(&buf[..n]).is_some();
                    break;
                }
            }
            Ok(Ok(_)) => continue,
            _ => continue,
        }
    }
    if !supported {
        debug!(%stun, "STUN server published no OTHER-ADDRESS; filtering stays unknown");
        return FilterProbe::Unsupported;
    }

    // Step 2: ask for the answer from the other port and watch where it lands.
    let (probe, probe_txid) = stun::binding_request_change_port();
    for attempt in 0..FILTER_PROBE_ATTEMPTS {
        if socket.send_to(&probe, stun).await.is_err() {
            return FilterProbe::Unsupported;
        }
        match timeout(FILTER_PROBE_TIMEOUT, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                if stun::parse_response(&buf[..n], &probe_txid).is_none() {
                    // Someone else's datagram on a shared socket: keep waiting
                    // within this attempt's budget by retrying the loop body.
                    continue;
                }
                if from.port() != stun.port() {
                    debug!(%stun, %from, "filtering probe crossed a new port: adf or eif");
                    return FilterProbe::AddressDependentOrOpen;
                }
                debug!(%stun, "STUN server ignored CHANGE-REQUEST; filtering stays unknown");
                return FilterProbe::Unsupported;
            }
            _ => {
                debug!(%stun, attempt, "filtering probe unanswered");
            }
        }
    }
    FilterProbe::AddressAndPortDependent
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "udp")]
    use crate::shared::UDP_NONCE_LEN;

    /// An ephemeral request (`port == 0`) must always yield a bound socket on a
    /// real (non-zero) port.
    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn bind_socket_ephemeral_gets_a_port() {
        let s = bind_socket(0).await.expect("ephemeral bind");
        assert_ne!(s.local_addr().unwrap().port(), 0);
    }

    /// REGRESSION (concurrent-tunnel flap, docs/plans/udp_flap/EVIDENCE.md): a
    /// second `bind_socket` on a port a live socket already holds must NOT co-bind
    /// it (that let the kernel deliver inbound to the last binder, stealing a live
    /// tunnel's QUIC). Without `SO_REUSEADDR` the second bind is refused and falls
    /// back to a DIFFERENT ephemeral port — never the contended one.
    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn bind_socket_fixed_port_collision_falls_back_to_ephemeral() {
        let a = bind_socket(0).await.expect("first bind");
        let p = a.local_addr().unwrap().port();
        let b = bind_socket(p)
            .await
            .expect("second bind must fall back, not error");
        let pb = b.local_addr().unwrap().port();
        assert_ne!(
            pb, p,
            "a second bind must never co-bind the held port (would steal inbound)"
        );
        assert_ne!(pb, 0);
        drop(a);
    }

    /// A free fixed port must be honored exactly (the firewall-friendly /
    /// NAT-predictable use case still works for the first/only claimant).
    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn bind_socket_free_fixed_port_is_honored() {
        // Discover a free port, release it (UDP frees immediately), re-bind it fixed.
        let probe = bind_socket(0).await.expect("probe bind");
        let p = probe.local_addr().unwrap().port();
        drop(probe);
        let s = bind_socket(p).await.expect("fixed bind on a free port");
        assert_eq!(s.local_addr().unwrap().port(), p);
    }

    /// `SO_*BUFFORCE` must lift the UDP buffer past `net.core.{w,r}mem_max` when
    /// the process holds CAP_NET_ADMIN. This is the direct-path throughput fix:
    /// without it the kernel silently clamps the 16 MiB request to the sysctl
    /// ceiling, capping a single QUIC flow at ~buffer/RTT. The test only asserts
    /// the strong (forced) outcome when it actually has the capability — under an
    /// unprivileged CI runner it degrades to documenting the clamp, never fails.
    #[cfg(all(feature = "udp", target_os = "linux"))]
    #[test]
    fn udp_buffers_forced_past_sysctl_clamp() {
        use nix::sys::socket::{getsockopt, sockopt};
        use socket2::{Domain, Protocol, Socket, Type};

        let wmem_max: usize = std::fs::read_to_string("/proc/sys/net/core/wmem_max")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        let socket =
            Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("create socket");
        let tuning = UdpDirectTuning::default();
        configure_udp_socket_buffers(&socket, &tuning);

        let actual_send = getsockopt(&socket, sockopt::SndBuf).expect("getsockopt SndBuf");

        // Can we force? Probe with a fresh socket so the assertion above is not
        // self-referential.
        let probe =
            Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("create probe");
        let can_force = nix::sys::socket::setsockopt(
            &probe,
            sockopt::SndBufForce,
            &tuning.udp_socket_send_buffer,
        )
        .is_ok();

        if can_force {
            // Forced: effective buffer must reach the request (kernel reports ~2×),
            // and crucially exceed the sysctl ceiling that would otherwise clamp it.
            assert!(
                actual_send >= tuning.udp_socket_send_buffer,
                "forced send buffer {actual_send} < requested {} — force ineffective",
                tuning.udp_socket_send_buffer
            );
            if wmem_max > 0 && wmem_max < tuning.udp_socket_send_buffer {
                assert!(
                    actual_send > wmem_max,
                    "forced send buffer {actual_send} did not exceed wmem_max {wmem_max} \
                     — clamp not bypassed"
                );
            }
        } else {
            // No CAP_NET_ADMIN here: the clamped value must still be the best the
            // kernel allows (>= ceiling), proving the fallback path ran.
            if wmem_max > 0 {
                assert!(
                    actual_send >= wmem_max.min(tuning.udp_socket_send_buffer),
                    "fallback send buffer {actual_send} below kernel ceiling {wmem_max}"
                );
            }
        }
    }

    /// Structurally unusable candidates (port 0, unspecified, multicast,
    /// broadcast) are dropped, duplicates deduped order-preserving, and the
    /// list is capped at MAX_UDP_CANDIDATES — with per-cause counters and no
    /// allocation proportional to a hostile count (I-11).
    #[test]
    fn sanitize_rejects_invalid_dedups_and_caps() {
        let mut cands: Vec<SocketAddr> = vec![
            "203.0.113.7:1000".parse().unwrap(),
            "203.0.113.7:0".parse().unwrap(),        // port 0
            "0.0.0.0:1234".parse().unwrap(),         // unspecified v4
            "[::]:1234".parse().unwrap(),            // unspecified v6
            "224.0.0.1:1234".parse().unwrap(),       // multicast v4
            "[ff02::1]:1234".parse().unwrap(),       // multicast v6
            "255.255.255.255:1234".parse().unwrap(), // broadcast
            "203.0.113.7:1000".parse().unwrap(),     // duplicate
            "203.0.113.8:1000".parse().unwrap(),
        ];
        let san = sanitize_candidates(&mut cands);
        assert_eq!(
            cands,
            vec![
                "203.0.113.7:1000".parse::<SocketAddr>().unwrap(),
                "203.0.113.8:1000".parse::<SocketAddr>().unwrap(),
            ]
        );
        assert_eq!(san.dropped_invalid, 6);
        assert_eq!(san.dropped_duplicate, 1);
        assert_eq!(san.dropped_overflow, 0);

        // Overflow: a hostile 10_000-entry list keeps only the cap.
        let mut flood: Vec<SocketAddr> = (0..10_000u32)
            .map(|i| {
                SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(198, 51, (i / 250) as u8, (i % 250) as u8 + 1)),
                    1000 + (i % 60_000) as u16,
                )
            })
            .collect();
        let san = sanitize_candidates(&mut flood);
        assert_eq!(flood.len(), MAX_UDP_CANDIDATES);
        assert_eq!(san.dropped_overflow, 10_000 - MAX_UDP_CANDIDATES);
    }

    /// Private/CGNAT candidates must be preserved: same-LAN peers need them and
    /// the token — not the candidate list — authenticates the path (I-6).
    #[test]
    fn sanitize_preserves_private_and_cgnat_candidates() {
        let mut cands: Vec<SocketAddr> = vec![
            "192.168.1.10:4000".parse().unwrap(),
            "10.0.0.5:4000".parse().unwrap(),
            "100.64.7.9:4000".parse().unwrap(),
            "127.0.0.1:4000".parse().unwrap(),
        ];
        let san = sanitize_candidates(&mut cands);
        assert_eq!(
            cands.len(),
            4,
            "private/CGNAT/loopback candidates must survive"
        );
        assert_eq!(san.dropped(), 0);
    }

    /// Fase 1 gate: concurrent STUN transactions on ONE socket all resolve —
    /// the recv actor demuxes by transaction id (no reader steals another
    /// transaction's response), and non-STUN datagrams are counted, never
    /// consumed by a STUN waiter.
    #[tokio::test]
    async fn traversal_demux_concurrent_transactions() {
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stun_addr = responder.local_addr().unwrap();
        tokio::spawn(run_stun_responder(responder));

        let tsock = UdpTraversalSocket::bind(0).await.unwrap();
        let port = tsock.local_addr().unwrap().port();
        let expected: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        // A peer-punch datagram interleaved with the transactions must be
        // counted as a peer datagram, not break any STUN waiter.
        let noise = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        noise
            .send_to(b"bore-punch", ("127.0.0.1", port))
            .await
            .unwrap();

        let (a, b, c) = tokio::join!(
            tsock.stun_query(stun_addr),
            tsock.stun_query(stun_addr),
            tsock.stun_query(stun_addr),
        );
        assert_eq!(a.unwrap(), expected);
        assert_eq!(b.unwrap(), expected);
        assert_eq!(c.unwrap(), expected);
        // The punch datagram may still be in flight when the queries resolve;
        // poll briefly instead of asserting immediately.
        for _ in 0..50 {
            if tsock.peer_datagrams() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(tsock.peer_datagrams() >= 1, "punch datagram not counted");
    }

    /// Fase 1 gate: a response with the WRONG transaction id, and a correct
    /// response from the WRONG source (ip:port), are both discarded as strays
    /// — only the queried server's genuine response resolves the transaction.
    #[tokio::test]
    async fn traversal_rejects_wrong_txid_and_wrong_source() {
        let real = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let real_addr = real.local_addr().unwrap();
        let spoofer = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Scripted responder: on each request it first sends garbage-txid and
        // wrong-source responses, THEN the genuine one.
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, from)) = real.recv_from(&mut buf).await else {
                    break;
                };
                // (a) right source, wrong txid.
                let mut req_bad = buf[..n].to_vec();
                req_bad[8] ^= 0xff;
                if let Some(reply) =
                    stun::binding_response(&req_bad, "203.0.113.66:6666".parse().unwrap())
                {
                    let _ = real.send_to(&reply, from).await;
                }
                // (b) wrong source, right txid.
                if let Some(reply) =
                    stun::binding_response(&buf[..n], "203.0.113.66:6666".parse().unwrap())
                {
                    let _ = spoofer.send_to(&reply, from).await;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                // (c) the genuine response.
                if let Some(reply) = stun::binding_response(&buf[..n], from) {
                    let _ = real.send_to(&reply, from).await;
                }
            }
        });

        let tsock = UdpTraversalSocket::bind(0).await.unwrap();
        let port = tsock.local_addr().unwrap().port();
        let expected: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mapped = tsock.stun_query(real_addr).await.unwrap();
        assert_eq!(
            mapped, expected,
            "genuine response must win, never the spoofed 203.0.113.66"
        );
        assert!(
            tsock.stray_stun() >= 1,
            "wrong-txid / wrong-source responses must be counted as strays"
        );
    }

    /// Fase 1 gate: a chain of unreachable STUN targets completes within the
    /// GLOBAL budget (legacy serial worst case: 3 s per target).
    #[tokio::test]
    async fn traversal_chain_respects_global_budget() {
        // Bound-but-silent sockets: requests vanish, no ICMP.
        let dead: Vec<UdpSocket> = {
            let mut v = Vec::new();
            for _ in 0..3 {
                v.push(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            }
            v
        };
        let targets: Vec<StunTarget> = dead
            .iter()
            .map(|s| StunTarget {
                requested: "dead".to_string(),
                addr: s.local_addr().unwrap(),
                source: StunSource::PublicDefault,
            })
            .collect();

        let tsock = UdpTraversalSocket::bind(0).await.unwrap();
        let started = Instant::now();
        let selected = tsock.discover_reflexive_chain(&targets).await;
        let elapsed = started.elapsed();
        assert!(selected.is_none());
        assert!(
            elapsed < STUN_CHAIN_BUDGET + Duration::from_secs(1),
            "chain took {elapsed:?}, exceeding the global budget {STUN_CHAIN_BUDGET:?}"
        );
    }

    /// The budgeted traversal gather must produce the SAME candidate shape as
    /// the legacy serial gather (default-equivalence, Fase 1 exit criterion).
    #[tokio::test]
    async fn traversal_gather_matches_legacy_gather() {
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stun_addr = responder.local_addr().unwrap();
        tokio::spawn(run_stun_responder(responder));
        let target = |name: &str| StunTarget {
            requested: name.to_string(),
            addr: stun_addr,
            source: StunSource::Single,
        };

        let legacy_sock = bind_socket(0).await.unwrap();
        let legacy =
            gather_candidates_from_stun_targets(&legacy_sock, &[target("t")], false, true).await;

        let tsock = UdpTraversalSocket::bind(0).await.unwrap();
        let traversal = gather_candidates_traversal(
            &tsock,
            &[target("t")],
            &GatherOptions::from_flags(false, true),
        )
        .await;

        assert_eq!(legacy.candidate_kinds, traversal.candidate_kinds);
        assert_eq!(legacy.candidates.len(), traversal.candidates.len());
        assert_eq!(
            legacy.selected_stun.as_ref().map(|s| s.requested.clone()),
            traversal
                .selected_stun
                .as_ref()
                .map(|s| s.requested.clone()),
        );
        // Both reflexives point at the respective socket's own port (loopback).
        assert_eq!(
            traversal.selected_stun.unwrap().reflexive.port(),
            tsock.local_addr().unwrap().port(),
        );
    }

    /// Handoff: after discovery the raw socket must be fully usable (single
    /// reader — the actor is gone). This is the Quinn-handoff spike in miniature.
    #[tokio::test]
    async fn traversal_into_socket_hands_off_cleanly() {
        let tsock = UdpTraversalSocket::bind(0).await.unwrap();
        let addr = format!("127.0.0.1:{}", tsock.local_addr().unwrap().port());
        let socket = tsock.into_socket().await.expect("handoff");

        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        peer.send_to(b"hello", &addr).await.unwrap();
        let mut buf = [0u8; 16];
        let (n, from) = timeout(Duration::from_secs(2), socket.recv_from(&mut buf))
            .await
            .expect("handoff socket must receive (actor stopped)")
            .unwrap();
        assert_eq!(&buf[..n], b"hello");
        socket.send_to(b"world", from).await.unwrap();
        let (n, _) = timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"world");
    }

    /// `to_offer` carries the typed v2 candidates aligned with the legacy
    /// list, plus the v2 capability — old peers keep reading `candidates`.
    #[tokio::test]
    async fn discovery_to_offer_builds_v2_alongside_legacy() {
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stun_addr = responder.local_addr().unwrap();
        tokio::spawn(run_stun_responder(responder));

        let tsock = UdpTraversalSocket::bind(0).await.unwrap();
        let disc = gather_candidates_traversal(
            &tsock,
            &[StunTarget {
                requested: "t".to_string(),
                addr: stun_addr,
                source: StunSource::Single,
            }],
            &GatherOptions::from_flags(false, true),
        )
        .await;
        let offer = disc.to_offer(7, 3);
        assert_eq!(offer.peer_id, 7);
        assert_eq!(offer.generation, 3);
        assert_eq!(offer.candidates, disc.candidates);
        assert_eq!(offer.typed_candidates.len(), disc.candidates.len());
        for (typed, (addr, kind)) in offer
            .typed_candidates
            .iter()
            .zip(disc.candidates.iter().zip(disc.candidate_kinds.iter()))
        {
            assert_eq!(&typed.addr, addr);
            assert_eq!(&typed.kind, kind);
        }
        assert!(offer.capabilities.iter().any(|c| c == UDP_CAP_CANDIDATE_V2));
        // Priorities: local outranks reflexive outranks predicted.
        let prio = |k: UdpCandidateKind| candidate_priority(k, 0);
        assert!(prio(UdpCandidateKind::Local) > prio(UdpCandidateKind::Reflexive));
        assert!(prio(UdpCandidateKind::Reflexive) > prio(UdpCandidateKind::Predicted));
    }

    /// Check frames: request/response round-trip, exact frame-size equality
    /// (no amplification), and every tampering rejected by parse.
    #[test]
    fn check_frames_roundtrip_and_reject_tampering() {
        let key = [7u8; 32];
        let txid = check::new_txid();
        let req = check::request(&key, check::ROLE_DIALER, 3, &txid);
        let resp = check::response(
            &key,
            check::ROLE_LISTENER,
            3,
            &txid,
            "203.0.113.9:4444".parse().unwrap(),
        );
        assert_eq!(req.len(), check::FRAME_LEN);
        assert_eq!(
            resp.len(),
            req.len(),
            "a response must never be larger than a request (anti-amplification)"
        );

        let p = check::parse(&key, &req).expect("request parses");
        assert_eq!(p.kind, check::KIND_REQUEST);
        assert_eq!(p.role, check::ROLE_DIALER);
        assert_eq!(p.generation, 3);
        assert_eq!(p.txid, txid);
        assert_eq!(p.observed, None);

        let p = check::parse(&key, &resp).expect("response parses");
        assert_eq!(p.kind, check::KIND_RESPONSE);
        assert_eq!(p.observed, Some("203.0.113.9:4444".parse().unwrap()));

        // Tampering: flipped payload byte, flipped HMAC byte, wrong key,
        // truncated, wrong magic — all rejected.
        let mut bad = req.clone();
        bad[6] ^= 1;
        assert!(check::parse(&key, &bad).is_none());
        let mut bad = req.clone();
        bad[59] ^= 1;
        assert!(check::parse(&key, &bad).is_none());
        assert!(check::parse(&[8u8; 32], &req).is_none());
        assert!(check::parse(&key, &req[..59]).is_none());
        let mut bad = req.clone();
        bad[0] = b'X';
        assert!(check::parse(&key, &bad).is_none());
    }

    /// Fase 2 security gate: forged/foreign probes get NO response, ever; a
    /// genuine authenticated request gets exactly one response of the same
    /// size as the request.
    #[tokio::test]
    async fn checks_never_answer_unauthenticated_probes() {
        let key = [3u8; 32];
        let tsock = UdpTraversalSocket::bind(0).await.unwrap();
        let port = tsock.local_addr().unwrap().port();
        let cfg = CheckConfig {
            key,
            generation: 7,
            role: CheckRole::Listener,
            window: Duration::from_millis(800),
            plan: None,
            spray: None,
        };

        let prober = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let probe_task = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let txid = check::new_txid();
            // (a) wrong key
            let forged = check::request(&[9u8; 32], check::ROLE_DIALER, 7, &txid);
            prober.send_to(&forged, ("127.0.0.1", port)).await.unwrap();
            // (b) right key, wrong generation
            let wrong_gen = check::request(&key, check::ROLE_DIALER, 8, &txid);
            prober
                .send_to(&wrong_gen, ("127.0.0.1", port))
                .await
                .unwrap();
            // (c) right key, SAME role (reflection)
            let same_role = check::request(&key, check::ROLE_LISTENER, 7, &txid);
            prober
                .send_to(&same_role, ("127.0.0.1", port))
                .await
                .unwrap();
            // None of the above may be answered.
            //
            // "Answered" means a frame THIS key authenticates, not "a datagram
            // arrived". The distinction is not pedantry: this test shares a
            // process, and a loopback, with the sprayed-escape test, which by
            // design fires thousands of frames at randomly chosen loopback
            // ports and will sooner or later choose this prober's. Asserting on
            // the mere presence of a datagram made this gate fail about one run
            // in eight, reporting a foreign test's crossfire as a security
            // regression — the same benign-stray reasoning the production code
            // follows (strays are `debug`, authentication is the gate).
            let mut buf = [0u8; 128];
            let deadline = Instant::now() + Duration::from_millis(300);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let Ok(Ok((n, _))) = timeout(left, prober.recv_from(&mut buf)).await else {
                    break;
                };
                assert!(
                    check::parse(&key, &buf[..n]).is_none(),
                    "an unauthenticated/foreign probe must NEVER get a response"
                );
            }
            // (d) genuine request → exactly one same-size response.
            //
            // The two windows in this test are deliberately asymmetric. The
            // SILENCE above is short because a violation would be immediate —
            // an implementation that answers a forged probe answers it at once
            // — so a tight bound costs nothing and keeps the test fast. This
            // one is generous because it is a POSITIVE assertion, and a
            // positive assertion bounded tightly measures the machine rather
            // than the code: at 500 ms it failed roughly one run in eight while
            // the workstation was also compiling, which is a gate that reports
            // load as a security regression.
            let good = check::request(&key, check::ROLE_DIALER, 7, &txid);
            prober.send_to(&good, ("127.0.0.1", port)).await.unwrap();
            // Same reason as above: read past any crossfire and judge the first
            // datagram this key authenticates.
            let deadline = Instant::now() + Duration::from_secs(5);
            let (n, frame) = loop {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "genuine request must be answered");
                let (n, _) = timeout(left, prober.recv_from(&mut buf))
                    .await
                    .expect("genuine request must be answered")
                    .unwrap();
                if let Some(frame) = check::parse(&key, &buf[..n]) {
                    break (n, frame);
                }
            };
            assert_eq!(n, check::FRAME_LEN, "response must not exceed request size");
            assert_eq!(frame.kind, check::KIND_RESPONSE);
            assert_eq!(frame.txid, txid);
            // Observed source = the prober's own address as seen by the peer.
            assert_eq!(frame.observed, Some(prober.local_addr().unwrap()));
        };
        let (outcome, ()) = tokio::join!(tsock.run_connectivity_checks(&[], &cfg), probe_task);
        assert!(
            tsock.invalid_checks() >= 3,
            "forged probes must be counted (got {})",
            tsock.invalid_checks()
        );
        // The genuine prober source became a peer-reflexive target.
        assert!(outcome.learned_prflx);
        assert!(outcome.targets.contains(&prober.local_addr().unwrap()));
        // S-5: the genuine request — and ONLY the genuine one — also ends this
        // listener's round, nominating the source it authenticated. The three
        // forged probes above must not have done so, which is why the
        // assertion is on the prober's address and not merely on `is_some`.
        assert_eq!(outcome.nominated, Some(prober.local_addr().unwrap()));
    }

    /// S-5 gate, the listener handoff. A listener whose own probes are never
    /// answered must still END its round the moment the dialer proves it can
    /// reach us — because everything after that answer is the dialer's move,
    /// and the socket has to be a QUIC endpoint before the dialer's first
    /// Initial arrives.
    ///
    /// RED-CHECK: remove the handoff arm and this test does not merely fail,
    /// it takes the full 3 s window and reports `nominated=None` — which is
    /// exactly the shape the staging logs showed on 9 of 27 establishments,
    /// each of them paying quinn's ~1 s initial PTO for the dropped Initial.
    ///
    /// The peer here answers NOTHING on purpose: it models the real case,
    /// where the dialer nominated early and tore its own responder down, so
    /// the listener's probes fall into a hole for the rest of the window.
    #[tokio::test]
    async fn listener_hands_off_as_soon_as_the_dialer_proves_it_can_reach_us() {
        let key = [11u8; 32];
        let tsock = UdpTraversalSocket::bind(0).await.unwrap();
        let port = tsock.local_addr().unwrap().port();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let cfg = CheckConfig {
            key,
            generation: 3,
            // A three-second window: long enough that a listener which waits
            // it out is unmistakable in the result, short enough to stay a
            // unit test.
            window: Duration::from_secs(3),
            role: CheckRole::Listener,
            plan: None,
            spray: None,
        };

        let dialer = async {
            // Let the round start and send a probe or two into the void.
            tokio::time::sleep(Duration::from_millis(120)).await;
            let txid = check::new_txid();
            let req = check::request(&key, check::ROLE_DIALER, 3, &txid);
            peer.send_to(&req, ("127.0.0.1", port)).await.unwrap();
            // The response must be on the wire BEFORE the round tears itself
            // down — the ordering `CheckAction` exists to guarantee. Without
            // it the dialer would never nominate, which on the real path means
            // dialing blind.
            let mut buf = [0u8; 128];
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "the handoff must not eat the response");
                let Ok(Ok((n, _))) = timeout(left, peer.recv_from(&mut buf)).await else {
                    continue;
                };
                if let Some(frame) = check::parse(&key, &buf[..n]) {
                    if frame.kind == check::KIND_RESPONSE && frame.txid == txid {
                        break;
                    }
                }
            }
        };

        let started = Instant::now();
        let peers = [peer_addr];
        let (outcome, ()) = tokio::join!(tsock.run_connectivity_checks(&peers, &cfg), dialer);
        let elapsed = started.elapsed();
        assert_eq!(
            outcome.nominated,
            Some(peer_addr),
            "the listener nominates the source that authenticated to it"
        );
        assert!(
            elapsed < Duration::from_millis(1500),
            "the listener waited {elapsed:?} of its 3 s window after the dialer \
             had already reached it"
        );
        // And `observed` is genuinely unknown on this path: nothing ever
        // answered our own request. Callers must not depend on it.
        assert!(outcome.observed.is_none());
    }

    /// S-7: an unset override yields exactly the shipped constant, and any
    /// override is clamped into a band that can still be called informed.
    #[test]
    #[cfg(feature = "udp")]
    fn direct_initial_rtt_unset_is_the_shipped_constant() {
        assert_eq!(resolve_direct_initial_rtt(None), DIRECT_INITIAL_RTT);
        assert_eq!(
            resolve_direct_initial_rtt(Some(1)),
            DIRECT_INITIAL_RTT_MIN,
            "a value that would fire the first PTO inside the handshake RTT is clamped up"
        );
        assert_eq!(
            resolve_direct_initial_rtt(Some(10_000)),
            DIRECT_INITIAL_RTT_MAX,
            "nothing above RFC 9002's own no-information default is informed"
        );
        assert_eq!(
            resolve_direct_initial_rtt(Some(40)),
            Duration::from_millis(40)
        );
        // The shipped value must stay strictly below the RFC default, or S-7
        // is a no-op that still claims to be a fix.
        assert!(DIRECT_INITIAL_RTT < DIRECT_INITIAL_RTT_MAX);
    }

    /// Fase 2 happy path on loopback: both roles run a round; each side
    /// nominates the other (bidirectional proof) with matching observed
    /// mapped addresses.
    #[tokio::test]
    async fn checks_nominate_bidirectionally_on_loopback() {
        let key = [5u8; 32];
        let a = UdpTraversalSocket::bind(0).await.unwrap();
        let b = UdpTraversalSocket::bind(0).await.unwrap();
        let a_addr: SocketAddr = format!("127.0.0.1:{}", a.local_addr().unwrap().port())
            .parse()
            .unwrap();
        let b_addr: SocketAddr = format!("127.0.0.1:{}", b.local_addr().unwrap().port())
            .parse()
            .unwrap();
        let l_cfg = CheckConfig {
            key,
            generation: 1,
            role: CheckRole::Listener,
            window: Duration::from_secs(2),
            plan: None,
            spray: None,
        };
        let d_cfg = CheckConfig {
            key,
            generation: 1,
            role: CheckRole::Dialer,
            window: Duration::from_secs(2),
            plan: None,
            spray: None,
        };
        let l_peers = [b_addr];
        let d_peers = [a_addr];
        let (l, d) = tokio::join!(
            a.run_connectivity_checks(&l_peers, &l_cfg),
            b.run_connectivity_checks(&d_peers, &d_cfg),
        );
        assert_eq!(l.nominated, Some(b_addr), "listener validates the dialer");
        assert_eq!(d.nominated, Some(a_addr), "dialer validates the listener");
        // Loopback: the observed mapped address is the socket's own address.
        assert_eq!(d.observed, Some(b_addr));
        // The LISTENER's `observed` is asserted conditionally, and that is the
        // S-5 contract rather than a weakened assertion: a listener now ends
        // its round on WHICHEVER of the two proofs arrives first — its own
        // request being answered (which carries `observed`) or an authenticated
        // request from the dialer (which does not, and needs nothing more).
        // Which one wins is a race on loopback, so pinning it would pin the
        // scheduler. What must always hold is the nomination above.
        if let Some(obs) = l.observed {
            assert_eq!(obs, a_addr);
        }
        assert!(l.checks_ms < 2000 && d.checks_ms < 2000);
    }

    /// Fase 3 gate: pacing jitter is bounded (strictly below the pace) and the
    /// two roles derive DIFFERENT sequences from the same key — that is what
    /// breaks the conntrack-crossfire lockstep without any extra wire bytes.
    #[test]
    fn check_jitter_bounded_and_role_diverse() {
        let key = [9u8; 32];
        let l_seed = check_jitter_seed(&key, check::ROLE_LISTENER);
        let d_seed = check_jitter_seed(&key, check::ROLE_DIALER);
        assert_ne!(l_seed, d_seed, "roles must not share a jitter sequence");
        let mut diverged = false;
        for step in 0..1000u64 {
            let l = check_jitter(l_seed, step);
            let d = check_jitter(d_seed, step);
            assert!(l < CHECK_JITTER_MAX && d < CHECK_JITTER_MAX);
            assert!(l < CHECK_PACE && d < CHECK_PACE);
            diverged |= l != d;
            // Deterministic: same seed+step, same value.
            assert_eq!(l, check_jitter(l_seed, step));
        }
        assert!(diverged, "sequences never diverged");
    }

    fn typed(addr: &str, kind: UdpCandidateKind) -> UdpTypedCandidate {
        UdpTypedCandidate {
            addr: addr.parse().unwrap(),
            kind,
            priority: 0,
        }
    }

    /// Fase 3 gate: the plan ORDERS candidates into kind groups but never adds
    /// or drops any — kinds missing from the server order still probe (final
    /// group), and no predicted group exists when no predicted candidate was
    /// offered ("no predicted checks when prediction is off").
    #[test]
    fn plan_check_groups_orders_and_never_drops() {
        let cands = vec![
            typed("198.51.100.1:1000", UdpCandidateKind::Reflexive),
            typed("192.168.1.5:1000", UdpCandidateKind::Local),
            typed("198.51.100.1:1001", UdpCandidateKind::Predicted),
            typed("203.0.113.9:2000", UdpCandidateKind::RouterMapped),
        ];
        // Server order names only predicted + reflexive (+ relay, skipped):
        // local + router-mapped must still probe, in a trailing group.
        let order = [
            UdpAdaptiveCandidateKind::Predicted,
            UdpAdaptiveCandidateKind::Reflexive,
            UdpAdaptiveCandidateKind::RelayFallback,
        ];
        let groups = plan_check_groups(&cands, &[], Some(&order));
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0], vec![cands[2].addr]);
        assert_eq!(groups[1], vec![cands[0].addr]);
        let mut tail = groups[2].clone();
        tail.sort();
        let mut expect = vec![cands[1].addr, cands[3].addr];
        expect.sort();
        assert_eq!(tail, expect, "unnamed kinds must never be dropped");

        // Default order (no plan): local first, predicted last.
        let groups = plan_check_groups(&cands, &[], None);
        assert_eq!(groups[0], vec![cands[1].addr]);
        assert_eq!(groups.last().unwrap(), &vec![cands[2].addr]);

        // No predicted candidates offered ⇒ no predicted group, even when the
        // order asks for one.
        let no_pred: Vec<UdpTypedCandidate> = cands
            .iter()
            .filter(|c| c.kind != UdpCandidateKind::Predicted)
            .copied()
            .collect();
        let order = [UdpAdaptiveCandidateKind::Predicted];
        let groups = plan_check_groups(&no_pred, &[], Some(&order));
        let all: Vec<SocketAddr> = groups.iter().flatten().copied().collect();
        assert_eq!(all.len(), 3);
        assert!(!all.contains(&cands[2].addr));

        // Legacy peer list (no typed metadata): one flat group.
        let fallback = [cands[0].addr, cands[1].addr];
        let groups = plan_check_groups(&[], &fallback, None);
        assert_eq!(groups, vec![fallback.to_vec()]);
        assert!(plan_check_groups(&[], &[], None).is_empty());
    }

    /// Fase 3 gate: a bogus plan can neither starve the check round nor stall
    /// the relay decision.
    #[test]
    fn plan_check_window_clamps() {
        let mk = |ms| UdpAdaptivePlan {
            mode: crate::shared::UdpAdaptiveMode::DirectFirst,
            candidate_order: vec![],
            retry_budget: 0,
            read_timeout_ms: ms,
            send_delay_ms: 0,
            reason_code: None,
            spray_role: None,
        };
        assert_eq!(plan_check_window(&mk(0)), Duration::from_millis(500));
        assert_eq!(plan_check_window(&mk(750)), Duration::from_millis(750));
        assert_eq!(plan_check_window(&mk(99_999)), Duration::from_millis(1500));
    }

    /// Fase 3 gate: the winning-pair cache remembers, recalls, and drops on
    /// invalidation; unknown keys recall nothing.
    /// The pair cache is process-global, so its two gates must not run at the
    /// same time: the bounded one evicts the OLDEST entry, which is exactly
    /// what the other one has just stored.
    static PAIR_CACHE_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn pair_cache_remember_recall_invalidate() {
        let _serial = PAIR_CACHE_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let addr: SocketAddr = "198.51.100.7:4433".parse().unwrap();
        pair_cache::remember("test:pair-cache", addr);
        assert_eq!(pair_cache::recall("test:pair-cache"), Some(addr));
        // Refreshing overwrites.
        let addr2: SocketAddr = "198.51.100.7:4434".parse().unwrap();
        pair_cache::remember("test:pair-cache", addr2);
        assert_eq!(pair_cache::recall("test:pair-cache"), Some(addr2));
        pair_cache::invalidate("test:pair-cache");
        assert_eq!(pair_cache::recall("test:pair-cache"), None);
        assert_eq!(pair_cache::recall("test:never-stored"), None);
    }

    /// S-9 gate: the winning-pair cache is BOUNDED. Expiry used to run only
    /// inside `recall` and only for the key being recalled, so a key that is
    /// never recalled again was never examined again — one entry per tunnel
    /// id, for the life of the process.
    ///
    /// The assertion is on the ceiling and on the survival of the most recent
    /// key, not on which older key was evicted: everything in the map is
    /// inside its TTL, so the eviction order is a tie-break, not a contract.
    #[test]
    fn pair_cache_is_bounded() {
        let _serial = PAIR_CACHE_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let addr: SocketAddr = "198.51.100.9:4433".parse().unwrap();
        for i in 0..(pair_cache::PAIR_CACHE_MAX * 2) {
            pair_cache::remember(&format!("test:bounded-{i}"), addr);
        }
        assert!(
            pair_cache::len() <= pair_cache::PAIR_CACHE_MAX,
            "cache grew to {} entries, past its {} ceiling",
            pair_cache::len(),
            pair_cache::PAIR_CACHE_MAX
        );
        let last = format!("test:bounded-{}", pair_cache::PAIR_CACHE_MAX * 2 - 1);
        assert_eq!(
            pair_cache::recall(&last),
            Some(addr),
            "the newest entry must survive its own insertion"
        );
        for i in 0..(pair_cache::PAIR_CACHE_MAX * 2) {
            pair_cache::invalidate(&format!("test:bounded-{i}"));
        }
    }

    /// Fase 3 gate: two agreeing STUN observations from DIFFERENT servers
    /// classify the mapping as endpoint-independent; the first success still
    /// selects the candidate (latency contract).
    #[tokio::test]
    async fn profile_two_agreeing_observations_is_eim() {
        let r1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let r2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let targets = vec![
            StunTarget {
                requested: "one".into(),
                addr: r1.local_addr().unwrap(),
                source: StunSource::PublicDefault,
            },
            StunTarget {
                requested: "two".into(),
                addr: r2.local_addr().unwrap(),
                source: StunSource::PublicDefault,
            },
        ];
        tokio::spawn(run_stun_responder(r1));
        tokio::spawn(run_stun_responder(r2));
        let tsock = UdpTraversalSocket::bind(0).await.unwrap();
        let port = tsock.local_addr().unwrap().port();
        let (selected, profile) = tsock.discover_reflexive_profile(&targets).await;
        let selected = selected.expect("a reflexive must be discovered");
        assert_eq!(selected.reflexive.port(), port);
        assert_eq!(profile.mapping, UdpNatMapping::Eim);
        assert_eq!(profile.observations, 2);
        assert_eq!(profile.port_preserved, Some(true));
    }

    /// Fase 3 gate: two DISAGREEING observations classify the mapping as
    /// symmetric — the wire profile the plan needs to avoid burning the
    /// retry budget on a hopeless blind punch.
    #[tokio::test]
    async fn profile_disagreeing_observations_is_symmetric() {
        // r1 answers honestly; r2 reports a different mapped address, exactly
        // what a symmetric NAT produces toward a second destination.
        let r1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let r2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let r1_addr = r1.local_addr().unwrap();
        let r2_addr = r2.local_addr().unwrap();
        tokio::spawn(run_stun_responder(r1));
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, from)) = r2.recv_from(&mut buf).await else {
                    break;
                };
                if let Some(reply) =
                    stun::binding_response(&buf[..n], "203.0.113.66:6666".parse().unwrap())
                {
                    let _ = r2.send_to(&reply, from).await;
                }
            }
        });
        let targets = vec![
            StunTarget {
                requested: "honest".into(),
                addr: r1_addr,
                source: StunSource::PublicDefault,
            },
            StunTarget {
                requested: "liar".into(),
                addr: r2_addr,
                source: StunSource::PublicDefault,
            },
        ];
        let tsock = UdpTraversalSocket::bind(0).await.unwrap();
        let (selected, profile) = tsock.discover_reflexive_profile(&targets).await;
        assert!(selected.is_some());
        assert_eq!(profile.mapping, UdpNatMapping::Symmetric);
        assert_eq!(profile.observations, 2);
    }

    /// Fase 5 gate: manual candidates (`--udp-candidate`) are advertised
    /// FIRST as `RouterMapped`, and `--udp-no-stun` skips the chain entirely
    /// (fast — no budget burn) while still emitting a zero-observation
    /// profile. Works with STUN fully dead/blocked.
    #[tokio::test]
    async fn gather_manual_candidates_first_no_stun_skips_chain() {
        let manual: SocketAddr = "203.0.113.10:41641".parse().unwrap();
        // A dead STUN target that would burn the whole budget if probed.
        let dead = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let targets = vec![StunTarget {
            requested: "dead".into(),
            addr: dead.local_addr().unwrap(),
            source: StunSource::PublicDefault,
        }];
        let tsock = UdpTraversalSocket::bind(0).await.unwrap();
        let opts = GatherOptions {
            manual_candidates: vec![manual],
            no_stun: true,
            ..Default::default()
        };
        let disc = gather_candidates_traversal(&tsock, &targets, &opts).await;
        assert_eq!(disc.candidates[0], manual, "manual candidate must lead");
        assert_eq!(disc.candidate_kinds[0], UdpCandidateKind::RouterMapped);
        assert_eq!(disc.attempted_stun, 0);
        assert!(
            disc.discovery_ms < 1000,
            "no_stun must skip the chain (took {}ms)",
            disc.discovery_ms
        );
        let profile = disc.profile.expect("profile still attached");
        assert_eq!(profile.observations, 0);
        // The offer carries the manual candidate to the peer.
        let offer = disc.to_offer(0, 0);
        assert!(offer.candidates.contains(&manual));
        assert_eq!(offer.profile, Some(profile));
    }

    /// Fase 3 gate: zero observations (STUN dead) still yield a profile —
    /// `observations: 0` — never a missing one (the policy reads absence as
    /// "legacy peer", not as "blocked").
    #[tokio::test]
    async fn profile_no_observations_reports_zero() {
        // Bound-but-silent socket: requests vanish.
        let dead = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let targets = vec![StunTarget {
            requested: "dead".into(),
            addr: dead.local_addr().unwrap(),
            source: StunSource::PublicDefault,
        }];
        let tsock = UdpTraversalSocket::bind(0).await.unwrap();
        let (selected, profile) = tsock.discover_reflexive_profile(&targets).await;
        assert!(selected.is_none());
        assert_eq!(profile.observations, 0);
        assert_eq!(profile.mapping, UdpNatMapping::Unknown);
    }

    /// Fase 3 gate: a planned round with a decoy head group (dead predicted
    /// candidate first) still validates the real pair — group stagger orders
    /// probing, it never excludes candidates or blocks nomination.
    #[tokio::test]
    async fn checks_planned_decoy_head_group_still_nominates() {
        let key = [6u8; 32];
        let a = UdpTraversalSocket::bind(0).await.unwrap();
        let b = UdpTraversalSocket::bind(0).await.unwrap();
        let a_addr: SocketAddr = format!("127.0.0.1:{}", a.local_addr().unwrap().port())
            .parse()
            .unwrap();
        let b_addr: SocketAddr = format!("127.0.0.1:{}", b.local_addr().unwrap().port())
            .parse()
            .unwrap();
        // A dead-but-bound decoy so the head group probes into silence.
        let decoy = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let decoy_addr = decoy.local_addr().unwrap();
        let l_cfg = CheckConfig {
            key,
            generation: 2,
            role: CheckRole::Listener,
            window: Duration::from_secs(2),
            plan: None,
            spray: None,
        };
        let d_cfg = CheckConfig {
            key,
            generation: 2,
            role: CheckRole::Dialer,
            window: Duration::from_secs(2),
            plan: Some(CheckPlan {
                groups: vec![vec![decoy_addr], vec![a_addr]],
                retry_budget: 1,
                initial_delay: Duration::ZERO,
            }),
            spray: None,
        };
        let l_peers = [b_addr];
        let d_peers = [decoy_addr, a_addr];
        let (l, d) = tokio::join!(
            a.run_connectivity_checks(&l_peers, &l_cfg),
            b.run_connectivity_checks(&d_peers, &d_cfg),
        );
        assert_eq!(l.nominated, Some(b_addr));
        assert_eq!(
            d.nominated,
            Some(a_addr),
            "real pair must win past the decoy group"
        );
    }

    #[test]
    fn token_is_deterministic_and_keyed() {
        let nonce = [7u8; 16];
        assert_eq!(
            derive_token(Some("s"), &nonce),
            derive_token(Some("s"), &nonce)
        );
        assert_ne!(
            derive_token(Some("s"), &nonce),
            derive_token(Some("t"), &nonce)
        );
        assert_ne!(derive_token(None, &nonce), derive_token(Some("s"), &nonce));
    }

    #[test]
    fn stun_round_trip_ipv4() {
        let (req, txid) = stun::binding_request();
        let source: SocketAddr = "203.0.113.7:51234".parse().unwrap();
        let resp = stun::binding_response(&req, source).expect("ipv4 response");
        assert_eq!(stun::parse_response(&resp, &txid), Some(source));
    }

    #[test]
    fn stun_rejects_wrong_transaction_id() {
        let (req, _) = stun::binding_request();
        let source: SocketAddr = "203.0.113.7:51234".parse().unwrap();
        let resp = stun::binding_response(&req, source).unwrap();
        assert_eq!(stun::parse_response(&resp, &[0u8; 12]), None);
    }

    #[tokio::test]
    async fn stun_default_falls_back_to_control_port_for_tls_ports() {
        // https:// (443) and http:// (80) front the control connection but not
        // the STUN responder, which lives on the control port.
        let by_port = |p| async move { resolve_stun("127.0.0.1", p, None).await.unwrap() };
        assert_eq!(
            by_port(443).await,
            format!("127.0.0.1:{CONTROL_PORT}").parse().unwrap()
        );
        assert_eq!(
            by_port(80).await,
            format!("127.0.0.1:{CONTROL_PORT}").parse().unwrap()
        );
        // A non-default port is the control port itself; use it as-is.
        assert_eq!(by_port(9000).await, "127.0.0.1:9000".parse().unwrap());
        // An explicit override always wins.
        let over = resolve_stun("127.0.0.1", 443, Some("127.0.0.1:1234"))
            .await
            .unwrap();
        assert_eq!(over, "127.0.0.1:1234".parse().unwrap());
    }

    #[test]
    fn live_stun_chain_prefers_public_servers_then_bore_fallback() {
        let chain = live_stun_target_names("bore.example.com", 443, None);
        assert_eq!(
            chain,
            vec![
                "stun.cloudflare.com:3478".to_string(),
                "stun.l.google.com:19302".to_string(),
                "stun1.l.google.com:19302".to_string(),
                format!("bore.example.com:{CONTROL_PORT}"),
            ]
        );
    }

    #[test]
    fn live_stun_chain_override_is_absolute() {
        assert_eq!(
            live_stun_target_names("bore.example.com", 443, Some("stun.example.net:3478")),
            vec!["stun.example.net:3478".to_string()]
        );
    }

    #[test]
    fn live_stun_chain_uses_peer_hint_first_and_deduplicates() {
        let chain = live_stun_target_names_with_hint(
            "bore.example.com",
            443,
            None,
            Some("stun.l.google.com:19302"),
        );
        assert_eq!(
            chain,
            vec![
                "stun.l.google.com:19302".to_string(),
                "stun.cloudflare.com:3478".to_string(),
                "stun1.l.google.com:19302".to_string(),
                format!("bore.example.com:{CONTROL_PORT}"),
            ]
        );

        let specs = live_stun_target_specs_with_hint(
            "bore.example.com",
            443,
            None,
            Some("stun.l.google.com:19302"),
        );
        assert_eq!(specs[0].1, StunSource::PeerHint);
    }

    #[test]
    fn live_stun_chain_override_ignores_peer_hint() {
        assert_eq!(
            live_stun_target_names_with_hint(
                "bore.example.com",
                443,
                Some("stun.operator.example:3478"),
                Some("stun.l.google.com:19302"),
            ),
            vec!["stun.operator.example:3478".to_string()]
        );
    }

    #[tokio::test]
    async fn candidate_discovery_tries_next_stun_after_failed_probe() {
        let bad = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let bad_addr = bad.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            for _ in 0..3 {
                if let Ok((_, from)) = bad.recv_from(&mut buf).await {
                    let _ = bad.send_to(b"not a stun response", from).await;
                }
            }
        });

        let good = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let good_addr = good.local_addr().unwrap();
        tokio::spawn(run_stun_responder(good));

        let socket = bind_socket(0).await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let targets = [
            StunTarget {
                requested: "bad-stun".to_string(),
                addr: bad_addr,
                source: StunSource::PublicDefault,
            },
            StunTarget {
                requested: "good-stun".to_string(),
                addr: good_addr,
                source: StunSource::BoreFallback,
            },
        ];

        let discovery = gather_candidates_from_stun_targets(&socket, &targets, false, false).await;
        let selected = discovery
            .selected_stun
            .expect("second STUN target should be selected");
        let reflexive: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        assert_eq!(selected.requested, "good-stun");
        assert_eq!(selected.addr, good_addr);
        assert_eq!(selected.source, StunSource::BoreFallback);
        assert_eq!(selected.reflexive, reflexive);
        assert_eq!(discovery.attempted_stun, 2);
        assert!(discovery.candidates.contains(&reflexive));
    }

    #[tokio::test]
    async fn candidate_discovery_falls_back_after_failed_peer_hint() {
        let bad = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let bad_addr = bad.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            if let Ok((_, from)) = bad.recv_from(&mut buf).await {
                let _ = bad.send_to(b"not a stun response", from).await;
            }
        });

        let good = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let good_addr = good.local_addr().unwrap();
        tokio::spawn(run_stun_responder(good));

        let socket = bind_socket(0).await.unwrap();
        let targets = [
            StunTarget {
                requested: "provider-hinted-stun".to_string(),
                addr: bad_addr,
                source: StunSource::PeerHint,
            },
            StunTarget {
                requested: "fallback-stun".to_string(),
                addr: good_addr,
                source: StunSource::PublicDefault,
            },
        ];

        let discovery = gather_candidates_from_stun_targets(&socket, &targets, false, false).await;
        let selected = discovery
            .selected_stun
            .expect("fallback STUN target should be selected");

        assert_eq!(selected.requested, "fallback-stun");
        assert_eq!(selected.source, StunSource::PublicDefault);
        assert_eq!(discovery.attempted_stun, 2);
    }

    #[tokio::test]
    async fn port_prediction_advertises_consecutive_ports() {
        // Stand up a local STUN responder and gather with prediction on.
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stun = responder.local_addr().unwrap();
        tokio::spawn(run_stun_responder(responder));

        let socket = bind_socket(0).await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let candidates = gather_candidates(&socket, stun, false, true).await;

        // The reflexive candidate (loopback source) and PREDICT_RANGE ports past it.
        let reflexive: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        assert!(
            candidates.contains(&reflexive),
            "missing reflexive candidate"
        );
        for delta in 1..=PREDICT_RANGE {
            if let Some(p) = port.checked_add(delta) {
                let predicted: SocketAddr = format!("127.0.0.1:{p}").parse().unwrap();
                assert!(
                    candidates.contains(&predicted),
                    "missing predicted port {p}"
                );
            }
        }
    }

    #[tokio::test]
    async fn port_prediction_off_adds_no_extra_ports() {
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stun = responder.local_addr().unwrap();
        tokio::spawn(run_stun_responder(responder));

        let socket = bind_socket(0).await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let candidates = gather_candidates(&socket, stun, false, false).await;
        // No predicted port should appear when prediction is disabled.
        for delta in 1..=PREDICT_RANGE {
            if let Some(p) = port.checked_add(delta) {
                let predicted: SocketAddr = format!("127.0.0.1:{p}").parse().unwrap();
                assert!(
                    !candidates.contains(&predicted),
                    "unexpected predicted port {p}"
                );
            }
        }
    }

    fn obs(server: &str, addr: &str) -> StunObservation {
        StunObservation {
            server: server.to_string(),
            reflexive: addr.parse().unwrap(),
        }
    }

    #[test]
    fn classify_blocked_when_no_observations() {
        assert_eq!(classify_nat(&[], &[]), NatClass::Blocked);
    }

    #[test]
    fn classify_open_when_reflexive_is_a_local_ip() {
        let local: IpAddr = "203.0.113.9".parse().unwrap();
        let obs = [obs("a", "203.0.113.9:40000")];
        assert_eq!(classify_nat(&[local], &obs), NatClass::Open);
    }

    #[test]
    fn classify_inconclusive_with_single_observation() {
        let obs = [obs("a", "198.51.100.1:40000")];
        assert_eq!(classify_nat(&[], &obs), NatClass::Inconclusive);
    }

    #[test]
    fn classify_cone_when_mapping_is_stable() {
        // Endpoint-independent: same public ip:port toward every server.
        let obs = [
            obs("a", "198.51.100.1:40000"),
            obs("b", "198.51.100.1:40000"),
            obs("c", "198.51.100.1:40000"),
        ];
        assert_eq!(classify_nat(&[], &obs), NatClass::Cone);
    }

    #[test]
    fn classify_symmetric_sequential() {
        // Endpoint-dependent with small regular steps -> prediction has a chance.
        let obs = [
            obs("a", "198.51.100.1:40000"),
            obs("b", "198.51.100.1:40001"),
            obs("c", "198.51.100.1:40002"),
        ];
        assert_eq!(
            classify_nat(&[], &obs),
            NatClass::Symmetric { sequential: true }
        );
    }

    #[test]
    fn classify_symmetric_random() {
        // Endpoint-dependent with large/irregular gaps -> prediction won't help.
        let obs = [
            obs("a", "198.51.100.1:40000"),
            obs("b", "198.51.100.1:51234"),
            obs("c", "198.51.100.1:33001"),
        ];
        assert_eq!(
            classify_nat(&[], &obs),
            NatClass::Symmetric { sequential: false }
        );
    }

    #[tokio::test]
    async fn bind_socket_honours_fixed_port_and_ephemeral() {
        // A fixed port binds exactly that port.
        let fixed = bind_socket(0).await.unwrap();
        let want = fixed.local_addr().unwrap().port(); // grab a free port, then reuse it
        drop(fixed);
        let sock = bind_socket(want).await.unwrap();
        assert_eq!(sock.local_addr().unwrap().port(), want);
        // SO_REUSEADDR lets a fresh socket rebind the same port after the first drops.
        drop(sock);
        let again = bind_socket(want).await.unwrap();
        assert_eq!(again.local_addr().unwrap().port(), want);
        // Port 0 is ephemeral (non-zero, and almost surely different).
        let eph = bind_socket(0).await.unwrap();
        assert_ne!(eph.local_addr().unwrap().port(), 0);
    }

    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn vhost_server_handshake_rejects_wrong_token() {
        let tuning = UdpDirectTuning::default();
        let server_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server_socket.local_addr().unwrap();
        let endpoint = vhost_server_endpoint(server_socket, &tuning).unwrap();
        let expected = derive_token(Some("shared-secret"), &[9u8; UDP_NONCE_LEN]);

        let server_task = {
            let endpoint = endpoint.clone();
            tokio::spawn(async move {
                let incoming = endpoint.accept().await.expect("incoming connection");
                let conn = incoming.await.expect("QUIC handshake should complete");
                vhost_server_handshake(conn, endpoint, |subdomain| {
                    (subdomain == "myapp").then_some(expected)
                })
                .await
            })
        };

        let wrong = derive_token(Some("different-secret"), &[9u8; UDP_NONCE_LEN]);
        let client = vhost_connect(
            bind_socket(0).await.unwrap(),
            server_addr,
            "myapp",
            wrong,
            tuning,
        )
        .await;
        assert!(
            client.is_err(),
            "client must fail when the server rejects the vhost direct token"
        );

        let server = tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server handshake task timed out")
            .unwrap();
        assert!(
            server.is_err(),
            "server must reject the wrong vhost direct token"
        );
    }

    #[cfg(feature = "udp")]
    #[test]
    fn shared_direct_auth_key_length_is_bounded() {
        assert!(
            crate::ssh_jump::direct_key(&"a".repeat(crate::ssh_jump::MAX_ALIAS_LEN)).len()
                <= MAX_SERVER_DIRECT_KEY_LEN
        );
        assert!(format!("port:{}", u16::MAX).len() <= MAX_SERVER_DIRECT_KEY_LEN);
        assert_eq!(MAX_SERVER_DIRECT_KEY_LEN, 128);
    }

    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn shared_direct_client_rejects_oversized_key_before_dial() {
        let socket = bind_socket(0).await.unwrap();
        let err = vhost_connect(
            socket,
            "127.0.0.1:9".parse().unwrap(),
            &"x".repeat(MAX_SERVER_DIRECT_KEY_LEN + 1),
            [0; TOKEN_LEN],
            UdpDirectTuning::default(),
        )
        .await
        .err()
        .expect("oversized direct key must be rejected");
        assert!(err.to_string().contains("server-direct key exceeds"));
    }

    #[test]
    fn cgnat_range_is_detected() {
        assert!(is_cgnat("100.64.0.1".parse().unwrap()));
        assert!(is_cgnat("100.127.255.255".parse().unwrap()));
        assert!(!is_cgnat("100.63.0.1".parse().unwrap()));
        assert!(!is_cgnat("100.128.0.1".parse().unwrap()));
        assert!(!is_cgnat("8.8.8.8".parse().unwrap()));
    }

    /// Two in-process QUIC endpoints exchange a datagram round-trip.
    /// Proves that: (a) `send_datagram` / `read_datagram` compile and work,
    /// (b) the datagram buffers configured in `transport_config` are accepted,
    /// (c) the received bytes match the sent bytes.
    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn quic_datagram_loopback_echo() {
        use tokio::sync::oneshot;

        let tuning = UdpDirectTuning::default();

        let srv_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let srv_addr = srv_sock.local_addr().unwrap();
        let srv_ep = server_endpoint(srv_sock, &tuning).unwrap();

        // done_tx signals the server task to exit after the echo completes.
        let (done_tx, done_rx) = oneshot::channel::<()>();

        let srv_task = tokio::spawn(async move {
            let incoming = srv_ep.accept().await.expect("no incoming");
            let conn = incoming.await.expect("QUIC handshake failed");
            let dc = DirectConn {
                conn,
                endpoint: srv_ep,
            };
            let pkt = dc.read_datagram().await.expect("server recv failed");
            dc.send_datagram(pkt.clone())
                .expect("server echo send failed");
            // Keep the connection alive until the client confirms it received the echo.
            let _ = done_rx.await;
            pkt
        });

        let cli_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cli_ep = client_endpoint(cli_sock, &tuning).unwrap();
        let conn = cli_ep.connect(srv_addr, "bore").unwrap().await.unwrap();
        let cli_dc = DirectConn {
            conn,
            endpoint: cli_ep,
        };

        let payload = bytes::Bytes::from("hello-vpn-datagram");
        cli_dc
            .send_datagram(payload.clone())
            .expect("client send failed");
        let echoed = cli_dc
            .read_datagram()
            .await
            .expect("client echo recv failed");
        assert_eq!(echoed, payload);

        // Signal server it can exit.
        let _ = done_tx.send(());

        let srv_recv = tokio::time::timeout(std::time::Duration::from_secs(3), srv_task)
            .await
            .expect("server task timed out")
            .unwrap();
        assert_eq!(srv_recv, payload);
    }

    /// Sending a datagram that exceeds any realistic QUIC datagram limit is
    /// reported as `DatagramSend::TooLarge` (a droppable per-packet condition),
    /// NOT as `Err` (which would kill the VPN link). Regression guard for the
    /// "send_datagram: datagram too large" link-death bug.
    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn datagram_too_large_is_droppable_not_fatal() {
        let tuning = UdpDirectTuning::default();

        let srv_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let srv_addr = srv_sock.local_addr().unwrap();
        let srv_ep = server_endpoint(srv_sock, &tuning).unwrap();
        // Server just needs to exist; it doesn't need to read the datagram.
        let _srv = tokio::spawn(async move {
            if let Some(inc) = srv_ep.accept().await {
                let _ = inc.await;
            }
        });

        let cli_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cli_ep = client_endpoint(cli_sock, &tuning).unwrap();
        let conn = cli_ep.connect(srv_addr, "bore").unwrap().await.unwrap();
        let cli_dc = DirectConn {
            conn,
            endpoint: cli_ep,
        };

        // 65 KB is always larger than any QUIC datagram limit.
        let huge = bytes::Bytes::from(vec![0u8; 65_000]);
        let result = cli_dc.send_datagram(huge);
        assert_eq!(
            result.unwrap(),
            DatagramSend::TooLarge,
            "oversized datagram must be droppable (TooLarge), never a fatal Err"
        );

        // A datagram that fits the path limit is reported as Sent.
        let small = bytes::Bytes::from(vec![0u8; 64]);
        assert_eq!(cli_dc.send_datagram(small).unwrap(), DatagramSend::Sent);
    }

    /// Regression for the "send_datagram: datagram too large" link-death bug:
    /// a Direct `send_batch` must report oversized packets as a DROP COUNT
    /// (`Ok(dropped)`), never as `Err`. The VPN uplink pump treats `Err` as
    /// link death and tears the whole tunnel down, so an oversized packet
    /// leaking out as `Err` here is exactly the bug. Also proves a mixed batch
    /// still delivers its in-limit packets (drop only the oversized ones).
    #[cfg(all(feature = "vpn", target_os = "linux"))]
    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn direct_send_batch_drops_oversized_without_error() {
        use crate::vpn::link::make_direct;

        let tuning = UdpDirectTuning::default();

        let srv_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let srv_addr = srv_sock.local_addr().unwrap();
        let srv_ep = server_endpoint(srv_sock, &tuning).unwrap();
        let _srv = tokio::spawn(async move {
            if let Some(inc) = srv_ep.accept().await {
                let _ = inc.await;
            }
        });

        let cli_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cli_ep = client_endpoint(cli_sock, &tuning).unwrap();
        let conn = cli_ep.connect(srv_addr, "bore").unwrap().await.unwrap();
        let dc = DirectConn {
            conn,
            endpoint: cli_ep,
        };
        let (mut sender, _recver) = make_direct(dc);

        // 65 KB is always larger than any QUIC datagram limit.
        let huge = bytes::Bytes::from(vec![0u8; 65_000]);
        let small = bytes::Bytes::from(vec![0u8; 64]);

        // Oversized packet → counted as 1 drop, NOT an Err.
        assert_eq!(
            sender
                .send_batch(std::slice::from_ref(&huge))
                .await
                .expect("oversized packet must never be a fatal Err"),
            1,
        );
        // In-limit packet → zero drops.
        assert_eq!(
            sender
                .send_batch(std::slice::from_ref(&small))
                .await
                .unwrap(),
            0,
        );
        // Mixed batch → only the oversized packet is dropped; the rest go out.
        let mixed = [small.clone(), huge.clone(), small.clone()];
        assert_eq!(
            sender.send_batch(&mixed).await.unwrap(),
            1,
            "mixed batch must drop only the oversized packet",
        );
    }

    /// recv_batch drains multiple queued datagrams in one call (Direct path).
    /// Proves the drain pattern: queue N datagrams on sender, one recv_batch
    /// call on receiver returns >1 packet.
    #[cfg(all(feature = "vpn", target_os = "linux"))]
    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn recv_batch_drains_queued_datagrams() {
        use crate::vpn::link::make_direct;

        let tuning = UdpDirectTuning::default();

        let srv_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let srv_addr = srv_sock.local_addr().unwrap();
        let srv_ep = server_endpoint(srv_sock, &tuning).unwrap();

        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

        let srv_task = tokio::spawn(async move {
            let incoming = srv_ep.accept().await.expect("no incoming");
            let conn = incoming.await.expect("QUIC handshake failed");
            let dc = DirectConn {
                conn,
                endpoint: srv_ep,
            };

            // Server sends 5 datagrams to the client.
            for i in 0..5 {
                let pkt = bytes::Bytes::from(format!("pkt-{}", i));
                dc.send_datagram(pkt).expect("server send failed");
            }

            // Keep alive until client signals done.
            let _ = done_rx.await;
        });

        let cli_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cli_ep = client_endpoint(cli_sock, &tuning).unwrap();
        let conn = cli_ep.connect(srv_addr, "bore").unwrap().await.unwrap();
        let dc = DirectConn {
            conn,
            endpoint: cli_ep,
        };

        let (_sender, mut recver) = make_direct(dc);

        // Give the server a moment to queue all 5 datagrams.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // First recv_batch call should drain multiple queued datagrams without yielding.
        let mut batch = Vec::new();
        recver
            .recv_batch(&mut batch)
            .await
            .expect("recv_batch failed");

        // Expect >= 2 (proves the drain pattern; exact number depends on QUIC internals).
        assert!(
            batch.len() >= 2,
            "recv_batch should drain multiple queued packets, got only {}",
            batch.len()
        );

        let _ = done_tx.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), srv_task).await;
    }
    /// Phase 03.4. An ordinary asset must never be demoted or capped, and a
    /// real transfer must be demoted exactly once — the cheap half of "do not
    /// assume", asserted directly instead of through a live QUIC connection.
    #[test]
    #[cfg(feature = "udp")]
    fn bulk_send_state_demotes_once_past_the_threshold_and_never_before() {
        let mut st = BulkSendState::default();
        // Below the threshold: full buffer offered, nothing demoted.
        assert_eq!(
            st.allow(1 << 20),
            1 << 20,
            "an unclassified stream is never capped"
        );
        assert!(!st.record(4096), "4 KiB is not a bulk transfer");
        assert!(!st.demoted);
        assert_eq!(st.allow(1 << 20), 1 << 20);

        // Exactly at the threshold: demoted, and only on that write.
        let remaining = (DIRECT_BULK_CLASSIFY_BYTES - 4096) as usize;
        assert!(
            st.record(remaining),
            "crossing DIRECT_BULK_CLASSIFY_BYTES must demote the stream"
        );
        assert!(st.demoted);
        assert!(
            !st.record(1 << 20),
            "demotion must fire once, not on every subsequent write"
        );

        // And a demoted stream is capped per write, without changing anything
        // about the windows (DEC-VE8).
        assert_eq!(st.allow(1 << 20), DIRECT_BULK_SEND_BURST);
        assert_eq!(
            st.allow(1024),
            1024,
            "the cap is a ceiling, not a minimum write size"
        );
    }

    #[test]
    #[cfg(feature = "udp")]
    fn direct_quic_liveness_unset_is_the_shipped_pair() {
        // Red-checks the zero-regression contract: an operator who sets neither
        // variable must get exactly the constants the campaign measured, or
        // every prior throughput and fallback figure stops applying.
        let live = resolve_direct_quic_liveness(None, None);
        assert_eq!(live.keepalive, QUIC_KEEPALIVE);
        assert_eq!(live.max_idle, QUIC_MAX_IDLE);
        assert!(!live.keepalive_adjusted);
    }

    #[test]
    #[cfg(feature = "udp")]
    fn direct_quic_liveness_keeps_two_losses_survivable() {
        // The whole point of the knob is a shorter loss window after UDP dies,
        // so the pair an operator is most likely to ask for is a low idle with
        // the default keep-alive — which alone would tear down HEALTHY
        // connections on a single dropped ping.
        let live = resolve_direct_quic_liveness(None, Some(4_000));
        assert_eq!(live.max_idle, Duration::from_secs(4));
        assert!(
            live.max_idle >= live.keepalive * 3,
            "two consecutive keep-alive losses must stay survivable, got {live:?}"
        );
        assert!(
            live.keepalive_adjusted,
            "tightening the 3s default under a 4s idle must be reported, not silent"
        );
    }

    #[test]
    #[cfg(feature = "udp")]
    fn direct_quic_liveness_honours_a_consistent_pair_untouched() {
        let live = resolve_direct_quic_liveness(Some(1_000), Some(4_000));
        assert_eq!(live.keepalive, Duration::from_secs(1));
        assert_eq!(live.max_idle, Duration::from_secs(4));
        assert!(!live.keepalive_adjusted);
    }

    #[test]
    #[cfg(feature = "udp")]
    fn direct_quic_liveness_clamps_absurd_inputs() {
        // A typo must degrade to something serviceable, never to a connection
        // that is declared dead between two pings or pinned open for hours.
        let tiny = resolve_direct_quic_liveness(Some(1), Some(1));
        assert_eq!(tiny.max_idle, QUIC_MAX_IDLE_MIN);
        assert!(tiny.keepalive >= QUIC_KEEPALIVE_MIN);
        assert!(tiny.max_idle >= tiny.keepalive * 3);

        let huge = resolve_direct_quic_liveness(Some(u64::MAX / 2), Some(u64::MAX / 2));
        assert_eq!(huge.max_idle, QUIC_MAX_IDLE_MAX);
        assert!(huge.max_idle >= huge.keepalive * 3);
    }

    // ── RFC 5780 behaviour discovery (plan Fase 6) ─────────────────────────
    //
    // These pin the FILTERING axis: the one `bore test-udp` used to list as a
    // known gap (docs/nat/NAT_TRAVERSAL.md §13) and the one a real-kernel
    // matrix shows deciding the outcome. `scripts/udp_nat_netns_test.sh`
    // proves `eim:adf` reaches a symmetric peer and `eim:apdf` does not —
    // identical in every other respect, including a fixed port-preserved
    // mapping (the `T-NAT-FIXEDPORT-VS-EDM` control).

    /// A plain binding request must keep reading as "no change requested", and
    /// the change-port request must decode to exactly that one flag.
    ///
    /// The second half is the one that would silently break everything: a
    /// wrong flag bit still parses, still travels, and simply makes every
    /// server answer from its ordinary port — which the client reads as
    /// "server does not support this" and reports as `unknown`. A measurement
    /// that quietly degrades to "unknown" is the failure mode this whole
    /// feature exists to remove.
    #[test]
    fn change_request_flags_round_trip() {
        let (plain, _) = stun::binding_request();
        assert_eq!(stun::parse_change_request(&plain), (false, false));

        let (probe, _) = stun::binding_request_change_port();
        assert_eq!(stun::parse_change_request(&probe), (false, true));

        // The header's length field covers the attributes only, and a wrong
        // value makes a conforming server drop the message with no diagnostic
        // whatsoever.
        let declared = u16::from_be_bytes([probe[2], probe[3]]) as usize;
        assert_eq!(declared, probe.len() - 20);
    }

    /// The response builder must still produce the EXACT legacy bytes when no
    /// optional attribute is asked for.
    ///
    /// Every existing client parses this message, and the two new attributes
    /// change the header's length field — so "added an attribute" and "broke
    /// every reflexive-address lookup in the fleet" are one edit apart.
    #[test]
    fn a_response_without_the_new_attributes_is_the_legacy_encoding() {
        let (req, _) = stun::binding_request();
        let src: SocketAddr = "198.51.100.7:41641".parse().unwrap();
        let legacy = stun::binding_response(&req, src).expect("legacy response");
        assert_eq!(legacy.len(), 32, "20 header + one 12-byte attribute");
        assert_eq!(u16::from_be_bytes([legacy[2], legacy[3]]), 12);
        assert_eq!(
            stun::binding_response_full(&req, src, None, None),
            Some(legacy)
        );
    }

    /// With both optional attributes the mapped address must still parse, and
    /// each new attribute must decode to what was put in.
    #[test]
    fn a_response_with_the_new_attributes_still_carries_the_mapping() {
        let (req, txid) = stun::binding_request();
        let src: SocketAddr = "198.51.100.7:41641".parse().unwrap();
        let origin: SocketAddr = "0.0.0.0:7835".parse().unwrap();
        let other: SocketAddr = "0.0.0.0:53211".parse().unwrap();
        let msg = stun::binding_response_full(&req, src, Some(origin), Some(other))
            .expect("full response");

        assert_eq!(
            u16::from_be_bytes([msg[2], msg[3]]) as usize,
            msg.len() - 20
        );
        assert_eq!(stun::parse_response(&msg, &txid), Some(src));
        assert_eq!(stun::parse_response_origin(&msg), Some(origin));
        assert_eq!(stun::parse_other_address(&msg), Some(other));
        // A legacy response publishes neither, which is what tells a client
        // the server cannot be asked the filtering question.
        let legacy = stun::binding_response(&req, src).expect("legacy response");
        assert_eq!(stun::parse_other_address(&legacy), None);
    }

    /// End to end over real sockets: a server WITH an alternate socket answers
    /// the change-request from the other port, and a client on an unfiltered
    /// path classifies that as address-dependent-or-open.
    ///
    /// Loopback has no NAT, so this cannot prove the blocked case — that needs
    /// a real kernel filter and lives in the netns harness. What it does prove
    /// is the half that an integration test CAN prove and that a unit test
    /// cannot: the request really reaches the server, the server really
    /// switches sockets, and the client really accepts a datagram from a
    /// source it never wrote to (the recv path's `any_port` demux).
    #[tokio::test]
    async fn a_server_with_an_alternate_socket_answers_from_the_other_port() {
        let primary = UdpSocket::bind("127.0.0.1:0").await.expect("primary");
        let alt = UdpSocket::bind("127.0.0.1:0").await.expect("alt");
        let server_addr = primary.local_addr().unwrap();
        let alt_addr = alt.local_addr().unwrap();
        assert_ne!(server_addr.port(), alt_addr.port());
        tokio::spawn(run_stun_responder_with_alt(
            Arc::new(primary),
            Some(Arc::new(alt)),
        ));

        let client = UdpSocket::bind("127.0.0.1:0").await.expect("client");
        assert_eq!(
            probe_filtering(&client, server_addr).await,
            FilterProbe::AddressDependentOrOpen
        );
    }

    /// The same server WITHOUT an alternate socket must make the client report
    /// `Unsupported`, never a restrictive filter.
    ///
    /// This is the asymmetry the whole design turns on: a probe that gets no
    /// answer and a server that cannot answer are indistinguishable at the
    /// socket, and calling the second one "apdf" would mark every peer of
    /// every pre-Fase-6 server as unreachable. Hence OTHER-ADDRESS, and hence
    /// this gate.
    #[tokio::test]
    async fn a_server_without_an_alternate_socket_is_unsupported_not_blocked() {
        let primary = UdpSocket::bind("127.0.0.1:0").await.expect("primary");
        let server_addr = primary.local_addr().unwrap();
        tokio::spawn(run_stun_responder_with_alt(Arc::new(primary), None));

        let client = UdpSocket::bind("127.0.0.1:0").await.expect("client");
        assert_eq!(
            probe_filtering(&client, server_addr).await,
            FilterProbe::Unsupported
        );
    }

    /// A blocked probe must map onto the legacy enum's `AddressDependent`
    /// (whose documented meaning is "address- OR port-dependent") and nothing
    /// else may: claiming `Eif` for a probe that merely got through would tell
    /// an old peer "full cone" on evidence that only rules out APDF.
    #[test]
    fn only_a_blocked_probe_sets_the_legacy_filtering_field() {
        use crate::shared::UdpNatFiltering;
        assert_eq!(
            FilterProbe::AddressAndPortDependent.legacy(),
            UdpNatFiltering::AddressDependent
        );
        assert_eq!(
            FilterProbe::AddressDependentOrOpen.legacy(),
            UdpNatFiltering::Unknown
        );
        assert_eq!(FilterProbe::Unsupported.legacy(), UdpNatFiltering::Unknown);
    }

    // ---------------------------------------------------------------------
    // Fase 7 — the sprayed escape (birthday-paradox rendezvous).
    // ---------------------------------------------------------------------

    /// The numbers the SOTA comparison quotes are the numbers the code uses.
    ///
    /// `p = 1 − (1 − S/P)^Q`. If someone retunes the defaults, this test is
    /// where the new probability has to be written down, which is the point:
    /// a mechanism that succeeds 63% of the time and one that succeeds 95% of
    /// the time are different products, and the difference must not be a side
    /// effect of an unrelated edit.
    #[test]
    fn the_collision_probability_is_the_one_the_documentation_quotes() {
        let one_pass = spray::SprayTuning {
            ports_per_pass: 256,
            passes: 1,
            sockets: 256,
            ..spray::SprayTuning::default()
        };
        let p = one_pass.collision_probability();
        assert!(
            (0.62..0.65).contains(&p),
            "256x256 should be the ~63% figure the comparison quotes, got {p}"
        );

        let defaults = spray::SprayTuning::default();
        let p = defaults.collision_probability();
        assert!(
            (0.94..0.97).contains(&p),
            "the shipped defaults should be the ~95% figure, got {p}"
        );

        // A draw with no tickets can never win, and a spray of nothing can
        // never be drawn — neither may report a probability above zero.
        assert_eq!(
            spray::SprayTuning {
                sockets: 0,
                ..defaults
            }
            .collision_probability(),
            0.0
        );
        assert_eq!(
            spray::SprayTuning {
                ports_per_pass: 0,
                ..defaults
            }
            .collision_probability(),
            0.0
        );
    }

    /// The sprayed set must be distinct and inside the allocatable range:
    /// a duplicate is a wasted packet and a privileged port is one no NAT
    /// hands out, so both would quietly lower the real probability below the
    /// one the line above pins.
    #[test]
    fn sprayed_ports_are_distinct_and_allocatable() {
        let ports = spray::random_ports(1024);
        assert_eq!(ports.len(), 1024, "the draw came up short");
        let unique: std::collections::HashSet<u16> = ports.iter().copied().collect();
        assert_eq!(unique.len(), ports.len(), "the draw contained a duplicate");
        assert!(
            ports.iter().all(|p| *p >= 1024),
            "the draw contained a privileged port"
        );
    }

    /// An unrecognised role is `None`, not a panic and not a default: a newer
    /// broker must be able to name a role this binary predates, exactly as it
    /// may name an unknown `reason_code`.
    #[test]
    fn an_unknown_spray_role_is_no_role() {
        assert_eq!(
            spray::role_from_wire(Some("easy")),
            Some(spray::SprayRole::Easy)
        );
        assert_eq!(
            spray::role_from_wire(Some("hard")),
            Some(spray::SprayRole::Hard)
        );
        assert_eq!(spray::role_from_wire(Some("sideways")), None);
        assert_eq!(spray::role_from_wire(None), None);
    }

    /// The whole rendezvous, end to end, over loopback.
    ///
    /// There is no NAT here, so the "collision" is literal: the easy side
    /// sprays destination ports and wins when one of them is a port the hard
    /// side's auxiliary sockets actually hold. The kernel's ephemeral range is
    /// narrower than the space the escape sprays, which makes the draw
    /// overwhelmingly likely rather than merely likely — deliberately, because
    /// a gate that fails one run in twenty is a gate that gets deleted.
    ///
    /// What this proves and what it does not: it proves the two halves find
    /// each other, that the winner is promoted by value with its reader
    /// stopped, and that the losers are cleaned up. It does NOT prove the
    /// mechanism beats a real NAT — only a kernel with real nftables rules can
    /// say that, which is `T-NAT-SPRAY-*` in `scripts/udp_nat_netns_test.sh`.
    ///
    /// It is also, unexpectedly, the WINDOWS oracle for this module, and the
    /// only one: this test is what caught the escape abandoning its socket on
    /// the first ICMP-driven recv error (`WSAECONNRESET`, earned by the spray's
    /// own probes — see `spray::listen`). The failure cannot be reproduced on
    /// Linux, where an UNCONNECTED UDP socket is not told about ICMP errors at
    /// all, so a green run here says nothing about that path and the
    /// `windows-latest` job is the thing to read. Same standing rule as the
    /// macOS backend: iterate through CI, not locally.
    #[tokio::test]
    async fn the_sprayed_escape_rendezvous_finds_a_pair() {
        let key = [7u8; 32];
        let tuning = spray::SprayTuning {
            ports_per_pass: 6000,
            passes: 2,
            sockets: 384,
            pace: Duration::from_micros(0),
            repeat: Duration::from_millis(250),
            cap: Duration::from_secs(20),
            loopback_candidates: true,
        };

        let easy_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let easy_addr: SocketAddr = (
            Ipv4Addr::LOCALHOST,
            easy_socket.local_addr().unwrap().port(),
        )
            .into();

        let easy_cfg = CheckConfig {
            key,
            generation: 9,
            role: CheckRole::Dialer,
            window: CHECK_WINDOW,
            plan: None,
            spray: Some(spray::SprayRole::Easy),
        };
        let hard_cfg = CheckConfig {
            role: CheckRole::Listener,
            spray: Some(spray::SprayRole::Hard),
            ..easy_cfg.clone()
        };

        let hard_tuning = tuning;
        let hard =
            tokio::spawn(
                async move { spray::hard_side(&[easy_addr], &hard_cfg, &hard_tuning).await },
            );

        // The easy side is handed a peer candidate that is right about the IP
        // and wrong about the port — which is exactly the cell: the only thing
        // it can know about a symmetric peer is its address.
        let peers = vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 1))];
        let easy_hit = spray::easy_side(&easy_socket, &peers, &easy_cfg, &tuning).await;
        let hard_result = hard.await.unwrap();

        let easy_hit = easy_hit.expect("the easy side found no pair");
        let (promoted, hard_hit) = hard_result.expect("the hard side found no pair");

        assert_eq!(
            easy_hit.port(),
            promoted.local_addr().unwrap().port(),
            "the easy side nominated a port the hard side did not promote"
        );
        assert_eq!(
            hard_hit, easy_addr,
            "the hard side nominated the wrong remote"
        );

        // The promoted socket is a live, sole-owner socket: the round's reader
        // is finished, so the caller can hand it straight to Quinn.
        promoted
            .send_to(b"post-handoff", easy_addr)
            .await
            .expect("the promoted socket must still be usable by its new owner");
    }

    /// A peer that claims OUR role is never answered and never nominated.
    ///
    /// On loopback the easy side sprays its own ephemeral range and will
    /// eventually spray its own port, so this is not a hypothetical: without
    /// the guard a single host would nominate itself and hand Quinn a pair
    /// that goes nowhere.
    #[tokio::test]
    async fn the_escape_never_nominates_a_peer_that_claims_its_own_role() {
        let key = [3u8; 32];
        let cfg = CheckConfig {
            key,
            generation: 4,
            role: CheckRole::Dialer,
            window: CHECK_WINDOW,
            plan: None,
            spray: Some(spray::SprayRole::Easy),
        };
        let tuning = spray::SprayTuning {
            ports_per_pass: 1,
            passes: 1,
            pace: Duration::from_millis(1),
            cap: Duration::from_millis(400),
            loopback_candidates: true,
            ..spray::SprayTuning::default()
        };

        let victim = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let victim_addr: SocketAddr =
            (Ipv4Addr::LOCALHOST, victim.local_addr().unwrap().port()).into();
        let impostor = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();

        // Same role as the victim: a loopback of its own frame, or a second
        // copy of the same side. Either way it must be ignored.
        let frame = check::request(&key, CheckRole::Dialer.byte(), 4, &check::new_txid());
        impostor.send_to(&frame, victim_addr).await.unwrap();

        let peers = vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 1))];
        assert_eq!(
            spray::easy_side(&victim, &peers, &cfg, &tuning).await,
            None,
            "a same-role frame was accepted as a winning pair"
        );
    }

    /// The escape must not fire outside the cell it was built for, and must
    /// not fire at all on a nominated round: a pair the ordinary mechanism
    /// already solved would otherwise pay the escape's whole window.
    #[tokio::test]
    async fn the_escape_is_skipped_without_a_role_and_on_a_nominated_round() {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let no_role = CheckConfig {
            key: [1u8; 32],
            generation: 1,
            role: CheckRole::Dialer,
            window: CHECK_WINDOW,
            plan: None,
            spray: None,
        };
        let mut outcome = CheckOutcome {
            nominated: None,
            observed: None,
            learned_prflx: false,
            targets: vec![SocketAddr::from((Ipv4Addr::new(203, 0, 113, 7), 4000))],
            checks_ms: 0,
        };
        let started = Instant::now();
        let socket = sprayed_escape(socket, &mut outcome, &no_role).await;
        assert_eq!(socket.local_addr().unwrap().port(), port, "socket replaced");
        assert_eq!(outcome.nominated, None);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "no role must cost no time"
        );

        let nominated: SocketAddr = (Ipv4Addr::new(203, 0, 113, 7), 4000).into();
        let with_role = CheckConfig {
            spray: Some(spray::SprayRole::Easy),
            ..no_role
        };
        outcome.nominated = Some(nominated);
        let started = Instant::now();
        let socket = sprayed_escape(socket, &mut outcome, &with_role).await;
        assert_eq!(socket.local_addr().unwrap().port(), port, "socket replaced");
        assert_eq!(outcome.nominated, Some(nominated));
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a nominated round must cost no time"
        );
    }
}
