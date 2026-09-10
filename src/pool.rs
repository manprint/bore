//! Shared "carrier pool" primitives.
//!
//! A tunnel's data path can span several parallel connections; proxied substreams
//! are round-robined across them to avoid yamux's single-connection head-of-line
//! blocking and to give each connection its own TCP congestion window. The pool is
//! used by public tunnels ([`crate::server::Server::serve_tunnel`]) and by secret
//! providers ([`crate::secret::serve_provider`]) — the latter keeps its pool in the
//! shared provider registry so the relay tasks can round-robin across it.
//!
//! The mechanism is uniform: the first connection issues a per-tunnel token
//! ([`crate::shared::ServerMessage::CarrierToken`]); extra connections present it
//! ([`crate::shared::ClientMessage::JoinCarrier`]) and the server delivers their
//! substream openers here via [`PendingCarriers`].

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use tokio::sync::mpsc;

use crate::mux;

/// One member of a carrier pool: a substream opener plus a liveness flag, cleared
/// when that connection drops so the pool can prune it.
pub struct Carrier {
    /// Opener for the connection this carrier represents.
    pub opener: mux::LinkOpener,
    /// Set while the connection is alive; cleared by the task holding the
    /// connection when it drops.
    pub alive: Arc<AtomicBool>,
    /// How many proxied connections currently pinned to this carrier have been
    /// classified as bulk (phase 03.2). A proxied connection is pinned to one
    /// carrier for its whole life, so a small request that lands on a carrier a
    /// bulk transfer is saturating waits behind it — measured as 2.57 ms idle
    /// versus 28.25 ms with one bulk transfer in flight (F-15). This counter is
    /// what lets [`CarrierPool::pick_avoiding_bulk`] send it elsewhere.
    pub bulk: Arc<AtomicUsize>,
}

impl Carrier {
    /// A carrier marked alive.
    pub fn new(opener: mux::LinkOpener) -> Self {
        Self {
            opener,
            alive: Arc::new(AtomicBool::new(true)),
            bulk: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// Pending carrier pools keyed by a per-tunnel token. An extra connection
/// presenting the token has its [`Carrier`] delivered to whoever registered the
/// token (a public-tunnel loop or a secret provider).
pub type PendingCarriers = Arc<DashMap<String, mpsc::UnboundedSender<Carrier>>>;

/// Removes a pending carrier token from the registry when the owning tunnel ends,
/// so the token map does not leak entries across tunnel lifetimes.
pub struct TokenGuard {
    registry: PendingCarriers,
    token: String,
}

impl TokenGuard {
    /// Hold this guard for the tunnel's lifetime; dropping it frees the token.
    pub fn new(registry: PendingCarriers, token: String) -> Self {
        Self { registry, token }
    }
}

impl Drop for TokenGuard {
    fn drop(&mut self) {
        self.registry.remove(&self.token);
    }
}

/// Bytes a single proxied connection must move before it counts as bulk.
///
/// DEC-VE4: classification is bytes already moved, never `Content-Type`,
/// `Content-Length` or a path pattern. A single monotonic counter is
/// transport-agnostic, cannot be lied to by a wrong declaration, and needs no
/// HTTP parsing on a path that is deliberately a byte pipe after the first head.
///
/// 512 KiB is two full proxy copy buffers ([`crate::shared::proxy_buffer_size`]
/// defaults to 256 KiB), so an ordinary asset served in one or two buffer fills
/// never trips it, while any real transfer trips it in its first fraction of a
/// second. Calibration is recorded in
/// `docs/plans/plan_VhostEnhancements/phase_03.md` §3.5.
pub const BULK_CLASSIFY_BYTES: u64 = 512 * 1024;

/// Per-proxied-connection bulk classification, and the carrier occupancy it
/// contributes to (phase 03.1).
///
/// Classification is **one-way**: a connection becomes bulk and stays bulk. A
/// connection oscillating between classes would produce exactly the scheduling
/// churn that is worse than either steady state.
///
/// Occupancy is released by `Drop`, so every way a proxied connection can end —
/// completion, error, provider death, task abort — releases it. There is no
/// second place to remember.
pub struct BulkTicket {
    bytes: AtomicU64,
    threshold: u64,
    classified: AtomicBool,
    carrier_bulk: Option<Arc<AtomicUsize>>,
}

impl BulkTicket {
    /// A ticket contributing to `carrier_bulk` once it classifies. `None` for a
    /// path with no carrier to account against (the QUIC direct path, which is
    /// scheduled per stream rather than per carrier — see phase 03.4).
    pub fn new(carrier_bulk: Option<Arc<AtomicUsize>>) -> Arc<Self> {
        Self::with_threshold(carrier_bulk, BULK_CLASSIFY_BYTES)
    }

    /// A ticket with an explicit threshold, so a test does not have to move
    /// half a megabyte to exercise the transition.
    pub fn with_threshold(carrier_bulk: Option<Arc<AtomicUsize>>, threshold: u64) -> Arc<Self> {
        Arc::new(Self {
            bytes: AtomicU64::new(0),
            threshold,
            classified: AtomicBool::new(false),
            carrier_bulk,
        })
    }

    /// Record `n` bytes moved on this connection, in either direction. Returns
    /// `true` on the call that crosses into bulk, so a caller can log the
    /// transition exactly once.
    ///
    /// On the hot path (already classified, or an uninteresting read) this is a
    /// single relaxed load or add: it runs per `poll_read`/`poll_write` of a
    /// splice measured at ~11 000 proxied connections per second (F-11).
    pub fn record(&self, n: u64) -> bool {
        if n == 0 || self.classified.load(Ordering::Relaxed) {
            return false;
        }
        let total = self.bytes.fetch_add(n, Ordering::Relaxed) + n;
        if total < self.threshold {
            return false;
        }
        // Two concurrent halves of the same connection can cross together;
        // only one may count against the carrier.
        if self
            .classified
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        if let Some(bulk) = &self.carrier_bulk {
            bulk.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    /// Whether this connection has been classified as bulk.
    pub fn is_bulk(&self) -> bool {
        self.classified.load(Ordering::Relaxed)
    }

    /// Bytes recorded so far.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

impl Drop for BulkTicket {
    fn drop(&mut self) {
        if self.classified.load(Ordering::Relaxed) {
            if let Some(bulk) = &self.carrier_bulk {
                bulk.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

/// A round-robin pool of live carriers. Thread-safe: a secret provider's pool is
/// shared in the registry and picked concurrently by many relay tasks. The lock is
/// never held across an `.await` (pick/push only clone an opener or push a value).
pub struct CarrierPool {
    carriers: Mutex<Vec<Carrier>>,
    next: AtomicUsize,
}

impl CarrierPool {
    /// A pool seeded with the first connection's opener (always considered live by
    /// its owner — when the first connection dies, the whole tunnel is torn down).
    pub fn new(first: mux::LinkOpener) -> Self {
        Self {
            carriers: Mutex::new(vec![Carrier::new(first)]),
            next: AtomicUsize::new(0),
        }
    }

    /// Add a carrier, capped at `max` total members. Returns `false` (dropping the
    /// carrier) when already at capacity.
    pub fn push(&self, carrier: Carrier, max: usize) -> bool {
        let mut carriers = self.carriers.lock().expect("carrier pool mutex");
        if carriers.len() >= max {
            return false;
        }
        carriers.push(carrier);
        true
    }

    /// Number of carriers currently in the pool (including dead ones not yet pruned).
    pub fn len(&self) -> usize {
        self.carriers.lock().expect("carrier pool mutex").len()
    }

    /// Whether the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Pick the next live opener round-robin, pruning any that have died. Returns
    /// `None` only if every carrier has dropped.
    pub fn pick(&self) -> Option<mux::LinkOpener> {
        let mut carriers = self.carriers.lock().expect("carrier pool mutex");
        carriers.retain(|c| c.alive.load(Ordering::Relaxed));
        if carriers.is_empty() {
            return None;
        }
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % carriers.len();
        Some(carriers[idx].opener.clone())
    }

    /// Pick a live carrier, preferring the one holding the fewest bulk-classified
    /// connections, and return its bulk counter so the caller can account its own
    /// connection against it (phase 03.2).
    ///
    /// Deliberately a SEPARATE entry point rather than a change to [`pick`]: the
    /// pool is shared by the secret, vhost and SSH-jump paths, and keeping those
    /// call sites literally untouched is what keeps the blast radius of this
    /// scheduling change visible in the diff.
    ///
    /// With no bulk connections anywhere this reduces to [`pick`]'s round-robin
    /// **exactly**, including the shared `next` cursor — the campaign measured
    /// that carriers slightly *hurt* on a clean idle path (median c4/c1 ratio
    /// 0.941), so a scheduler that changed behaviour with nothing to schedule
    /// would itself be the regression.
    pub fn pick_avoiding_bulk(&self) -> Option<(mux::LinkOpener, Arc<AtomicUsize>)> {
        let mut carriers = self.carriers.lock().expect("carrier pool mutex");
        carriers.retain(|c| c.alive.load(Ordering::Relaxed));
        if carriers.is_empty() {
            return None;
        }
        let start = self.next.fetch_add(1, Ordering::Relaxed) % carriers.len();
        let loads: Vec<usize> = carriers
            .iter()
            .map(|c| c.bulk.load(Ordering::Relaxed))
            .collect();
        let best = choose_carrier(&loads, start);
        Some((
            carriers[best].opener.clone(),
            Arc::clone(&carriers[best].bulk),
        ))
    }
}

/// Index of the carrier to use, given each live carrier's bulk occupancy and
/// the round-robin cursor position.
///
/// Split out as a pure function because this is the one piece of the change
/// that has to be exactly right in three separate regimes, and each is cheap to
/// assert directly: a single carrier, no bulk anywhere, and one saturated
/// carrier among idle ones.
///
/// The cursor is consulted FIRST and strictly-less-than wins, so with equal
/// occupancy the result is `start` — i.e. plain round-robin, unchanged.
fn choose_carrier(loads: &[usize], start: usize) -> usize {
    debug_assert!(!loads.is_empty());
    let mut best = start;
    let mut best_load = loads[start];
    for step in 1..loads.len() {
        let idx = (start + step) % loads.len();
        if loads[idx] < best_load {
            best = idx;
            best_load = loads[idx];
        }
    }
    best
}

/// Await the next pooled carrier, or pend forever when there is no pool (so the
/// arm sits harmlessly in a `select!`). Borrows the receiver + its drop guard.
pub async fn recv_carrier(
    rx: Option<&mut (mpsc::UnboundedReceiver<Carrier>, TokenGuard)>,
) -> Option<Carrier> {
    match rx {
        Some((rx, _guard)) => rx.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DEC-VE6: a one-element pool has nothing to choose. This must be
    /// byte-for-byte today's behaviour, which is a standing project invariant
    /// rather than a nicety.
    #[test]
    fn choose_carrier_single_carrier_is_always_index_zero() {
        for start in 0..4 {
            let _ = start;
            assert_eq!(choose_carrier(&[0], 0), 0);
        }
        // Even a saturated single carrier: there is nowhere else to go, and
        // refusing to pick would fail the request.
        assert_eq!(choose_carrier(&[7], 0), 0);
    }

    /// With nothing to schedule the scheduler must be inert: the campaign
    /// measured carriers slightly HURTING on a clean idle path (median c4/c1
    /// ratio 0.941), so changing behaviour here would itself be the regression.
    #[test]
    fn choose_carrier_without_bulk_is_plain_round_robin() {
        let loads = [0, 0, 0, 0];
        for start in 0..loads.len() {
            assert_eq!(
                choose_carrier(&loads, start),
                start,
                "equal occupancy must yield the cursor's own carrier"
            );
        }
    }

    /// The point of the change: a small request must not land on the carrier a
    /// bulk transfer is saturating (F-15: 2.57 ms idle -> 28.25 ms behind one
    /// bulk transfer).
    #[test]
    fn choose_carrier_avoids_the_carrier_holding_bulk() {
        // Carrier 0 is busy; 1..3 are idle. No cursor position may select 0.
        let loads = [1, 0, 0, 0];
        for start in 0..loads.len() {
            assert_ne!(
                choose_carrier(&loads, start),
                0,
                "start={start} selected the bulk-loaded carrier"
            );
        }
        // Least-loaded, not merely non-zero: 2 is the only minimum here.
        assert_eq!(choose_carrier(&[3, 2, 1, 2], 0), 2);
        assert_eq!(choose_carrier(&[3, 2, 1, 2], 3), 2);
        // All equally loaded (every carrier saturated): back to round-robin,
        // because spreading is all that is left to do.
        for start in 0..4 {
            assert_eq!(choose_carrier(&[2, 2, 2, 2], start), start);
        }
    }

    /// Phase 03.1, DEC-VE4: classification is bytes moved, one-way, and its
    /// occupancy is released by `Drop` on every exit path.
    #[test]
    fn bulk_ticket_classifies_once_and_releases_on_drop() {
        let carrier = Arc::new(AtomicUsize::new(0));
        {
            let ticket = BulkTicket::with_threshold(Some(Arc::clone(&carrier)), 1000);

            assert!(!ticket.record(400), "below threshold is not bulk");
            assert!(!ticket.is_bulk());
            assert_eq!(carrier.load(Ordering::Relaxed), 0);

            assert!(ticket.record(600), "crossing the threshold classifies");
            assert!(ticket.is_bulk());
            assert_eq!(carrier.load(Ordering::Relaxed), 1);

            // One-way and counted once: an oscillating classification would
            // cause exactly the scheduling churn this avoids.
            assert!(!ticket.record(10_000));
            assert!(ticket.is_bulk());
            assert_eq!(
                carrier.load(Ordering::Relaxed),
                1,
                "a classified connection must occupy its carrier exactly once"
            );
        }
        assert_eq!(
            carrier.load(Ordering::Relaxed),
            0,
            "dropping the connection must free the carrier occupancy"
        );
    }

    #[test]
    fn bulk_ticket_below_threshold_never_touches_the_carrier() {
        let carrier = Arc::new(AtomicUsize::new(0));
        {
            let ticket = BulkTicket::with_threshold(Some(Arc::clone(&carrier)), 1000);
            for _ in 0..9 {
                assert!(!ticket.record(100));
            }
            assert_eq!(ticket.bytes(), 900);
            assert!(!ticket.is_bulk());
        }
        assert_eq!(carrier.load(Ordering::Relaxed), 0);
    }

    /// The direct QUIC path has no carrier to account against; a ticket with no
    /// counter must still classify without panicking.
    #[test]
    fn bulk_ticket_without_a_carrier_still_classifies() {
        let ticket = BulkTicket::with_threshold(None, 100);
        assert!(ticket.record(200));
        assert!(ticket.is_bulk());
    }

    #[test]
    fn bulk_ticket_ignores_zero_byte_records() {
        let ticket = BulkTicket::with_threshold(None, 1);
        assert!(!ticket.record(0));
        assert_eq!(ticket.bytes(), 0);
        assert!(!ticket.is_bulk());
    }

    // Pins the RAII contract the server relies on for BUG (CarrierToken leak):
    // the token must be created into `pending_carriers` and a `TokenGuard` built
    // BEFORE the fallible `CarrierToken` send, so that a send failure (`?`
    // early-return) unwinds through this Drop and removes the orphaned token.
    #[test]
    fn token_guard_drop_removes_token() {
        let registry: PendingCarriers = Arc::new(DashMap::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        registry.insert("tok-1".to_string(), tx);
        {
            let _guard = TokenGuard::new(Arc::clone(&registry), "tok-1".to_string());
            assert!(registry.contains_key("tok-1"));
        }
        // Guard dropped (as it would on a `?` early-return before the send
        // succeeds): the token is gone, no leak.
        assert!(!registry.contains_key("tok-1"));
        assert!(registry.is_empty());
    }
}
