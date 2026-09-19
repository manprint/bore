//! Bounded observation of the transport used for a transfer-link backend.
//!
//! The vhost client dials the local HTTP listener.  The HTTP side can identify
//! that connection by the peer address reported by `accept`, which is the same
//! address the client sees as `TcpStream::local_addr()`.  A lease is inserted
//! immediately after the backend connect and removed by id when the splice
//! ends.  Removal by id matters when the operating system reuses an ephemeral
//! port for a newer download after an older connection has closed.

// The registry is installed by the Link supervisor in the next phase; keep
// the complete hook/lease API compiled while that wiring is still additive.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::client::{BackendPath, BackendPathHook};

#[derive(Debug)]
struct Entry {
    peer: SocketAddr,
    path: BackendPath,
}

#[derive(Debug)]
struct RegistryState {
    entries: HashMap<u64, Entry>,
}

/// Bounded map of currently active backend connections.
pub(crate) struct BackendPathRegistry {
    limit: usize,
    next_id: AtomicU64,
    state: Mutex<RegistryState>,
}

impl BackendPathRegistry {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit: limit.max(1),
            next_id: AtomicU64::new(1),
            state: Mutex::new(RegistryState {
                entries: HashMap::new(),
            }),
        })
    }

    /// Return a callback suitable for [`crate::client::ClientScope`].
    pub(crate) fn hook(self: &Arc<Self>) -> BackendPathHook {
        let registry = Arc::clone(self);
        Arc::new(move |peer, path| {
            registry
                .insert(peer, path)
                .map(|lease| Box::new(lease) as Box<dyn Send + Sync>)
        })
    }

    fn insert(self: &Arc<Self>, peer: SocketAddr, path: BackendPath) -> Option<BackendPathLease> {
        let mut state = self
            .state
            .lock()
            .expect("transfer link path registry poisoned");
        if state.entries.len() >= self.limit {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        state.entries.insert(id, Entry { peer, path });
        Some(BackendPathLease {
            registry: Arc::clone(self),
            id,
            peer,
        })
    }

    /// Return the newest active path for a peer.  The HTTP handler calls this
    /// after its socket has been accepted and before it starts reading a body.
    pub(crate) fn path_for(&self, peer: SocketAddr) -> Option<BackendPath> {
        let state = self
            .state
            .lock()
            .expect("transfer link path registry poisoned");
        state
            .entries
            .iter()
            .filter(|(_, entry)| entry.peer == peer)
            .max_by_key(|(id, _)| *id)
            .map(|(_, entry)| entry.path)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.state
            .lock()
            .expect("transfer link path registry poisoned")
            .entries
            .len()
    }
}

/// RAII removal token for one backend connection.
pub(crate) struct BackendPathLease {
    registry: Arc<BackendPathRegistry>,
    id: u64,
    peer: SocketAddr,
}

impl Drop for BackendPathLease {
    fn drop(&mut self) {
        let mut state = self
            .registry
            .state
            .lock()
            .expect("transfer link path registry poisoned");
        if let Some(entry) = state.entries.remove(&self.id) {
            debug_assert_eq!(entry.peer, self.peer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn backend_path_lease_precedes_forwarded_bytes() {
        let registry = BackendPathRegistry::new(4);
        let hook = registry.hook();
        let observed = hook(peer(41001), BackendPath::DirectQuic).expect("capacity available");

        // The HTTP owner can resolve the path before it consumes its first
        // request byte.  The lease remains alive for the complete splice.
        assert_eq!(
            registry.path_for(peer(41001)),
            Some(BackendPath::DirectQuic)
        );
        drop(observed);
        assert_eq!(registry.path_for(peer(41001)), None);
    }

    #[test]
    fn stale_backend_lease_cannot_remove_new_entry() {
        let registry = BackendPathRegistry::new(4);
        let first = registry.hook()(peer(41002), BackendPath::RelayTcp).expect("first lease");
        let second = registry.hook()(peer(41002), BackendPath::DirectQuic).expect("second lease");

        assert_eq!(
            registry.path_for(peer(41002)),
            Some(BackendPath::DirectQuic)
        );
        drop(first);
        assert_eq!(
            registry.path_for(peer(41002)),
            Some(BackendPath::DirectQuic),
            "dropping an old lease must not remove the replacement"
        );
        drop(second);
        assert_eq!(registry.len(), 0);
    }
}
