//! Deterministic userspace NAT lab for hole-punch traversal tests (plan Fase 0).
//!
//! Emulates a NAT middlebox with REAL UDP sockets on loopback, so the actual
//! production traversal code (`holepunch::gather_candidates_from_stun_targets`,
//! `connect_direct`, `DirectListener`) runs unmodified through configurable
//! mapping (EIM/ADM/APDM), filtering (EIF/ADF/APDF) and port-allocation
//! (preserve/sequential/random) behaviour — no root, no netns, no public STUN.
//!
//! Topology per emulated NAT (one inside peer per box):
//!
//! ```text
//!   inside peer socket (0.0.0.0:p, seen as 127.0.0.1:p)
//!        │  sends to a LAN "alias" socket, one per WAN destination
//!        ▼
//!   alias  (LAN ip 127.0.1.N)  ── outbound policy ──►  ext mapping socket
//!                                                      (WAN ip 127.0.0.N)
//!        ▲                                                   │
//!        └── inbound filtering policy ◄──────────────────────┘
//! ```
//!
//! The harness plays the "routing table": every WAN address a peer must reach
//! (STUN server, the other peer's candidates) is rewritten to that peer's NAT
//! alias for it — exactly what a default route does on a real LAN. Inbound
//! datagrams from a *new* WAN source are delivered from a freshly created alias,
//! which is how a real peer observes a peer-reflexive source. Distinct WAN IPs
//! per box (127.0.0.0/8) keep ADF (address-dependent filtering) meaningful.
//!
//! Linux-only: binding arbitrary 127.x.y.z addresses needs no setup on Linux
//! but fails on macOS; the test files gate on `target_os = "linux"`.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Weak};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const BUF: usize = 65_536;

/// NAT mapping behaviour (RFC 4787 terminology).
///
/// Some variants/helpers (ADM, EIF, `flush_mappings`) are built for upcoming
/// phases' scenarios (remap, filtering matrices) and are not exercised by
/// every test binary that includes this support module.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    /// Endpoint-independent mapping: one external mapping for all destinations.
    Eim,
    /// Address-dependent mapping: one mapping per destination IP.
    Adm,
    /// Address-and-port-dependent mapping (symmetric): one per destination.
    Apdm,
}

/// NAT filtering behaviour (RFC 4787 terminology).
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filtering {
    /// Endpoint-independent filtering: any WAN source may reach the mapping.
    Eif,
    /// Address-dependent filtering: any source IP previously contacted.
    Adf,
    /// Address-and-port-dependent filtering: only exact addresses contacted.
    Apdf,
}

/// External port allocation strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortAlloc {
    /// Try to preserve the internal source port.
    Preserve,
    /// Allocate sequentially: each new mapping = previous port + delta.
    Sequential(u16),
    /// Kernel-chosen ephemeral port (models random allocation).
    Random,
}

/// Full NAT behaviour profile for one emulated box.
#[derive(Clone, Copy, Debug)]
pub struct NatPolicy {
    pub mapping: Mapping,
    pub filtering: Filtering,
    pub alloc: PortAlloc,
    pub hairpin: bool,
    /// Drop ALL traffic (models outbound-UDP-blocked networks).
    pub drop_all: bool,
}

impl NatPolicy {
    /// Classic home cone NAT: EIM + APDF, port-preserving.
    pub fn cone() -> Self {
        Self {
            mapping: Mapping::Eim,
            filtering: Filtering::Apdf,
            alloc: PortAlloc::Preserve,
            hairpin: false,
            drop_all: false,
        }
    }

    /// Symmetric NAT: APDM + APDF, random allocation.
    pub fn symmetric() -> Self {
        Self {
            mapping: Mapping::Apdm,
            filtering: Filtering::Apdf,
            alloc: PortAlloc::Random,
            hairpin: false,
            drop_all: false,
        }
    }

    /// UDP fully blocked.
    pub fn blocked() -> Self {
        Self {
            drop_all: true,
            ..Self::cone()
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum MapKey {
    Any,
    Ip(IpAddr),
    Addr(SocketAddr),
}

struct MappingEntry {
    ext: Arc<UdpSocket>,
    ext_addr: SocketAddr,
    internal: SocketAddr,
    /// WAN destinations this mapping has sent to (drives ADF/APDF filtering).
    allowed: std::sync::Mutex<HashSet<SocketAddr>>,
}

#[derive(Default)]
struct NatState {
    internal: Option<SocketAddr>,
    /// WAN destination -> LAN-side alias socket the inside peer dials.
    aliases: HashMap<SocketAddr, Arc<UdpSocket>>,
    mappings: HashMap<MapKey, Arc<MappingEntry>>,
    by_ext: HashMap<SocketAddr, Arc<MappingEntry>>,
    last_seq_port: Option<u16>,
    dropped_inbound: u64,
}

/// One emulated NAT middlebox with a single inside peer.
pub struct NatBox {
    pub policy: NatPolicy,
    wan_ip: Ipv4Addr,
    lan_ip: Ipv4Addr,
    state: Mutex<NatState>,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for NatBox {
    fn drop(&mut self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

impl NatBox {
    /// Create box number `n` (1..=250): WAN ip `127.0.0.(n)`, LAN ip `127.0.1.(n)`.
    /// Use distinct `n` per box so ADF (per-IP filtering) is meaningful.
    pub fn numbered(n: u8, policy: NatPolicy) -> Arc<Self> {
        assert!(n >= 1, "box number must be >= 1");
        Arc::new(Self {
            policy,
            wan_ip: Ipv4Addr::new(127, 0, 0, n.saturating_add(1)),
            lan_ip: Ipv4Addr::new(127, 0, 1, n.saturating_add(1)),
            state: Mutex::new(NatState::default()),
            tasks: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Number of inbound datagrams the filtering policy dropped.
    pub async fn dropped_inbound(&self) -> u64 {
        self.state.lock().await.dropped_inbound
    }

    /// Expire every mapping (models a NAT remap/reboot): ext sockets close,
    /// the next outbound datagram allocates fresh mappings. Built for the
    /// remap scenarios of later phases.
    #[allow(dead_code)]
    pub async fn flush_mappings(&self) {
        let mut st = self.state.lock().await;
        st.mappings.clear();
        st.by_ext.clear();
    }

    /// LAN-side address the inside peer must dial to reach `wan_dst` —
    /// the lab's stand-in for "the default route goes through the NAT".
    pub async fn alias(self: &Arc<Self>, wan_dst: SocketAddr) -> Result<SocketAddr> {
        let sock = self.alias_socket(wan_dst).await?;
        sock.local_addr().context("alias local_addr")
    }

    async fn alias_socket(self: &Arc<Self>, wan_dst: SocketAddr) -> Result<Arc<UdpSocket>> {
        let mut st = self.state.lock().await;
        if let Some(sock) = st.aliases.get(&wan_dst) {
            return Ok(Arc::clone(sock));
        }
        let sock = Arc::new(
            UdpSocket::bind((self.lan_ip, 0))
                .await
                .context("bind alias socket")?,
        );
        st.aliases.insert(wan_dst, Arc::clone(&sock));
        drop(st);

        let weak: Weak<Self> = Arc::downgrade(self);
        let task_sock = Arc::clone(&sock);
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; BUF];
            loop {
                // NEVER break on a recv error: Linux surfaces ICMP
                // port-unreachable (from punches to not-yet-bound predicted
                // ports) as ECONNREFUSED on unconnected UDP sockets; a real
                // NAT does not die on ICMP, so neither may the lab.
                let (n, src) = match task_sock.recv_from(&mut buf).await {
                    Ok(ok) => ok,
                    Err(_) => continue,
                };
                let Some(nat) = weak.upgrade() else { break };
                nat.outbound(src, wan_dst, &buf[..n]).await;
            }
        });
        self.tasks.lock().unwrap().push(task);
        Ok(sock)
    }

    /// Inside peer -> WAN: apply mapping policy, remember the destination for
    /// filtering, forward from the external mapping socket.
    async fn outbound(self: &Arc<Self>, lan_src: SocketAddr, wan_dst: SocketAddr, payload: &[u8]) {
        if self.policy.drop_all {
            return;
        }
        let entry = {
            let mut st = self.state.lock().await;
            match st.internal {
                None => st.internal = Some(lan_src),
                Some(cur) => assert_eq!(
                    cur, lan_src,
                    "natlab supports exactly one inside peer per box"
                ),
            }
            let key = match self.policy.mapping {
                Mapping::Eim => MapKey::Any,
                Mapping::Adm => MapKey::Ip(wan_dst.ip()),
                Mapping::Apdm => MapKey::Addr(wan_dst),
            };
            match st.mappings.get(&key) {
                Some(entry) => Arc::clone(entry),
                None => match self.bind_mapping(&mut st, lan_src).await {
                    Ok(entry) => {
                        st.mappings.insert(key, Arc::clone(&entry));
                        st.by_ext.insert(entry.ext_addr, Arc::clone(&entry));
                        entry
                    }
                    Err(_) => return,
                },
            }
        };
        entry.allowed.lock().unwrap().insert(wan_dst);

        // Hairpin: destination is one of our own external mappings.
        let hairpin_target = self.state.lock().await.by_ext.get(&wan_dst).cloned();
        if let Some(target) = hairpin_target {
            if self.policy.hairpin {
                self.inbound(&target, entry.ext_addr, payload).await;
            } else {
                self.state.lock().await.dropped_inbound += 1;
            }
            return;
        }
        let _ = entry.ext.send_to(payload, wan_dst).await;
    }

    async fn bind_mapping(
        self: &Arc<Self>,
        st: &mut NatState,
        internal: SocketAddr,
    ) -> Result<Arc<MappingEntry>> {
        let tries: Vec<u16> = match self.policy.alloc {
            PortAlloc::Preserve => vec![internal.port(), 0],
            PortAlloc::Random => vec![0],
            PortAlloc::Sequential(delta) => match st.last_seq_port {
                None => vec![0],
                Some(last) => (1..=16u16)
                    .map(|i| last.wrapping_add(delta.wrapping_mul(i)).max(1024))
                    .chain(std::iter::once(0))
                    .collect(),
            },
        };
        let mut ext = None;
        for port in tries {
            if let Ok(sock) = UdpSocket::bind((self.wan_ip, port)).await {
                ext = Some(Arc::new(sock));
                break;
            }
        }
        let ext = ext.context("no bindable external port")?;
        let ext_addr = ext.local_addr()?;
        st.last_seq_port = Some(ext_addr.port());

        let entry = Arc::new(MappingEntry {
            ext: Arc::clone(&ext),
            ext_addr,
            internal,
            allowed: std::sync::Mutex::new(HashSet::new()),
        });

        let weak: Weak<Self> = Arc::downgrade(self);
        let task_entry = Arc::clone(&entry);
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; BUF];
            loop {
                // See the alias task: ICMP-driven ECONNREFUSED must not kill
                // the mapping (a punch toward an unbound predicted port is
                // routine); skip the error and keep forwarding.
                let (n, wan_src) = match task_entry.ext.recv_from(&mut buf).await {
                    Ok(ok) => ok,
                    Err(_) => continue,
                };
                let Some(nat) = weak.upgrade() else { break };
                nat.inbound(&task_entry, wan_src, &buf[..n]).await;
            }
        });
        self.tasks.lock().unwrap().push(task);
        Ok(entry)
    }

    /// WAN -> inside peer: apply the filtering policy, deliver from the alias
    /// for the observed WAN source (a new source appears as a new alias —
    /// exactly how a real peer observes a peer-reflexive address).
    ///
    /// Boxed future: `alias_socket` spawns tasks that call `outbound`, which
    /// calls `inbound` (hairpin), which calls `alias_socket` — boxing here
    /// breaks the async opaque-type cycle (E0391).
    fn inbound<'a>(
        self: &'a Arc<Self>,
        entry: &'a Arc<MappingEntry>,
        wan_src: SocketAddr,
        payload: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let pass = {
                let allowed = entry.allowed.lock().unwrap();
                match self.policy.filtering {
                    Filtering::Eif => true,
                    Filtering::Adf => allowed.iter().any(|a| a.ip() == wan_src.ip()),
                    Filtering::Apdf => allowed.contains(&wan_src),
                }
            };
            if !pass {
                self.state.lock().await.dropped_inbound += 1;
                return;
            }
            let Ok(alias) = self.alias_socket(wan_src).await else {
                return;
            };
            let _ = alias.send_to(payload, entry.internal).await;
        })
    }
}
