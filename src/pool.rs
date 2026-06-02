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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use tokio::sync::mpsc;

use crate::mux;

/// One member of a carrier pool: a substream opener plus a liveness flag, cleared
/// when that connection drops so the pool can prune it.
pub struct Carrier {
    /// Opener for the connection this carrier represents.
    pub opener: mux::Opener,
    /// Set while the connection is alive; cleared by the task holding the
    /// connection when it drops.
    pub alive: Arc<AtomicBool>,
}

impl Carrier {
    /// A carrier marked alive.
    pub fn new(opener: mux::Opener) -> Self {
        Self {
            opener,
            alive: Arc::new(AtomicBool::new(true)),
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
    pub fn new(first: mux::Opener) -> Self {
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
    pub fn pick(&self) -> Option<mux::Opener> {
        let mut carriers = self.carriers.lock().expect("carrier pool mutex");
        carriers.retain(|c| c.alive.load(Ordering::Relaxed));
        if carriers.is_empty() {
            return None;
        }
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % carriers.len();
        Some(carriers[idx].opener.clone())
    }
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
