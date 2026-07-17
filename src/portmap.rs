//! Managed router port-mapping leases (plan Fase 5).
//!
//! Turns explicit router mappings from best-effort, expiring addresses into
//! LIVE resources: acquired via PCP (RFC 6887 `MAP`, the modern successor)
//! with UPnP-IGD as the fallback, renewed BEFORE their lifetime runs out,
//! re-announced when the external endpoint changes (gateway reboot detected
//! through the PCP Epoch Time), and released best-effort on drop.
//!
//! Strictly opt-in: nothing here runs unless the operator passed `--upnp`
//! (the existing port-mapping opt-in flag — with this module it now means
//! "managed mapping: PCP first, then UPnP-IGD"). A refresh failure never
//! affects the tunnel: the relay keeps working and renewal retries with
//! backoff; the mapping is an extra candidate, not a dependency.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// PCP servers listen on this UDP port on the default gateway (RFC 6887 §19.1).
const PCP_PORT: u16 = 5351;

/// PCP protocol version (RFC 6887).
const PCP_VERSION: u8 = 2;

/// PCP `MAP` opcode.
const PCP_OP_MAP: u8 = 1;

/// Requested mapping lifetime. Two minutes keeps parity with the legacy
/// one-shot UPnP lease while the renewal task makes it effectively permanent
/// for the tunnel's life; a gateway may assign shorter or longer.
const REQUESTED_LIFETIME: Duration = Duration::from_secs(120);

/// Per-request timeout for one PCP exchange (the gateway is one hop away).
const PCP_TIMEOUT: Duration = Duration::from_millis(750);

/// PCP request retries before giving up on PCP for this acquire.
const PCP_TRIES: u32 = 2;

/// Cap on the renewal-failure backoff.
const RENEW_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// UDP protocol number for the MAP opcode.
const PROTO_UDP: u8 = 17;

/// One live, managed port mapping. Hold it for the tunnel's lifetime: a
/// background task renews the mapping at half-lifetime and publishes a new
/// external endpoint on `changed` when the gateway reassigns one (renew
/// answered differently, or the PCP epoch went backwards — a reboot — and
/// the re-acquire landed elsewhere). Dropping the handle aborts the renewal
/// task and releases the mapping best-effort (lifetime-0 PCP request /
/// UPnP `delete_port`).
pub struct LeaseHandle {
    /// The external endpoint peers can be told to punch/dial.
    pub external: SocketAddr,
    /// Which protocol produced the mapping (for logs/reports).
    pub backend: &'static str,
    changed_rx: watch::Receiver<SocketAddr>,
    /// Renewal task guard; abort on drop, then best-effort release.
    _task: AbortAndRelease,
}

impl std::fmt::Debug for LeaseHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseHandle")
            .field("external", &self.external)
            .field("backend", &self.backend)
            .finish()
    }
}

impl LeaseHandle {
    /// A receiver that observes the CURRENT external endpoint; it changes
    /// when a renew/re-acquire lands on a different address. Callers that
    /// can re-offer (provider control loop, VPN retry rounds) watch this and
    /// re-announce their candidates with a fresh generation.
    pub fn changed(&self) -> watch::Receiver<SocketAddr> {
        self.changed_rx.clone()
    }
}

/// Aborts the renewal task on drop and fires the best-effort release.
struct AbortAndRelease {
    task: tokio::task::JoinHandle<()>,
    release: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Drop for AbortAndRelease {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Backend behaviour behind the lease manager. Object-safe-free: the manager
/// is generic, the two real backends are enum-dispatched, and tests plug a
/// mock to drive refresh/change/reboot/delete deterministically.
pub trait MappingBackend: Send + 'static {
    /// Acquire (or re-acquire) a mapping for `local_port`. Returns the
    /// external endpoint and the granted lifetime.
    fn acquire(
        &mut self,
        local_port: u16,
        suggested: Option<SocketAddr>,
    ) -> impl std::future::Future<Output = Result<(SocketAddr, Duration)>> + Send;
    /// Renew the mapping. Backends detect gateway resets here (PCP epoch)
    /// and transparently re-acquire; a CHANGED external endpoint is a valid
    /// outcome the manager must propagate, not an error.
    fn renew(
        &mut self,
        local_port: u16,
        current: SocketAddr,
    ) -> impl std::future::Future<Output = Result<(SocketAddr, Duration)>> + Send;
    /// Best-effort release (drop path; failures only logged).
    fn release_op(
        &self,
        local_port: u16,
        current: SocketAddr,
    ) -> Option<Box<dyn FnOnce() + Send + Sync>>;
    /// Label for logs.
    fn name(&self) -> &'static str;
}

/// Acquire a managed mapping for `local_port`: PCP first (modern, cheap,
/// epoch-aware), UPnP-IGD as the fallback. `None` when neither worked —
/// callers fall back to the discovery-only candidate set, exactly like the
/// legacy one-shot UPnP attempt.
pub async fn acquire_lease(local_port: u16) -> Option<LeaseHandle> {
    match PcpBackend::probe(local_port).await {
        Ok((backend, external, lifetime)) => {
            info!(%external, ?lifetime, "PCP MAP lease acquired");
            return Some(spawn_manager(backend, local_port, external, lifetime));
        }
        Err(err) => debug!(%err, "PCP unavailable; trying UPnP-IGD"),
    }
    match UpnpBackend::probe(local_port).await {
        Ok((backend, external, lifetime)) => {
            info!(%external, ?lifetime, "UPnP-IGD lease acquired");
            Some(spawn_manager(backend, local_port, external, lifetime))
        }
        Err(err) => {
            debug!(%err, "UPnP-IGD port mapping failed; no managed lease");
            None
        }
    }
}

/// Start the renewal loop for an acquired mapping and wrap it in a handle.
/// Public for tests (mock backends); production callers use [`acquire_lease`].
pub fn spawn_manager<B: MappingBackend>(
    backend: B,
    local_port: u16,
    external: SocketAddr,
    lifetime: Duration,
) -> LeaseHandle {
    let (changed_tx, changed_rx) = watch::channel(external);
    let release = backend.release_op(local_port, external);
    let name = backend.name();
    let task = tokio::spawn(renewal_loop(
        backend, local_port, external, lifetime, changed_tx,
    ));
    LeaseHandle {
        external,
        backend: name,
        changed_rx,
        _task: AbortAndRelease { task, release },
    }
}

/// Renew at half-lifetime; on failure retry with doubling backoff (capped)
/// while the previous mapping may still be alive — the relay is never
/// affected either way. A renew that lands on a DIFFERENT endpoint (gateway
/// reassignment or reboot) is published on the watch channel so re-offer
/// paths can announce it with a fresh generation. An expired-then-changed
/// mapping is therefore never re-published to new peers by stale state: the
/// watch always carries the CURRENT endpoint.
async fn renewal_loop<B: MappingBackend>(
    mut backend: B,
    local_port: u16,
    mut current: SocketAddr,
    mut lifetime: Duration,
    changed_tx: watch::Sender<SocketAddr>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let wait = (lifetime / 2).max(Duration::from_secs(5));
        tokio::time::sleep(wait).await;
        match backend.renew(local_port, current).await {
            Ok((external, new_lifetime)) => {
                backoff = Duration::from_secs(1);
                lifetime = new_lifetime;
                if external != current {
                    warn!(
                        backend = backend.name(),
                        old = %current,
                        new = %external,
                        "managed port mapping CHANGED; re-offer with a fresh generation"
                    );
                    current = external;
                    let _ = changed_tx.send(external);
                } else {
                    debug!(backend = backend.name(), %external, ?new_lifetime,
                        "managed port mapping renewed");
                }
            }
            Err(err) => {
                warn!(
                    backend = backend.name(),
                    %err,
                    retry_in = ?backoff,
                    "port-mapping renewal failed; mapping may lapse (relay unaffected), retrying"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RENEW_BACKOFF_MAX);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PCP (RFC 6887) MAP backend
// ---------------------------------------------------------------------------

/// Minimal PCP MAP client toward the default gateway. Tracks the server's
/// Epoch Time: an epoch that goes backwards (or resets under RFC 6887 §8.5
/// tolerance) means the gateway rebooted and lost its state — the renew
/// re-acquires from scratch and reports whatever endpoint comes back.
pub struct PcpBackend {
    socket: UdpSocket,
    server: SocketAddr,
    client_ip: Ipv4Addr,
    nonce: [u8; 12],
    /// Last observed (epoch_seconds, at) pair for reboot detection.
    last_epoch: Option<(u32, std::time::Instant)>,
}

impl PcpBackend {
    /// Find the default gateway, send one MAP request, and return the backend
    /// with its first mapping on success.
    async fn probe(local_port: u16) -> Result<(Self, SocketAddr, Duration)> {
        let gateway = default_gateway_v4().context("no default IPv4 gateway found")?;
        let server = SocketAddr::new(IpAddr::V4(gateway), PCP_PORT);
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .context("bind PCP client socket")?;
        socket.connect(server).await.context("connect PCP socket")?;
        let client_ip = match socket.local_addr()?.ip() {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_) => bail!("PCP v1 path is IPv4-only here"),
        };
        let mut nonce = [0u8; 12];
        use ring::rand::{SecureRandom, SystemRandom};
        SystemRandom::new()
            .fill(&mut nonce)
            .expect("system CSPRNG must not fail");
        let mut backend = Self {
            socket,
            server,
            client_ip,
            nonce,
            last_epoch: None,
        };
        let (external, lifetime) = backend.map_request(local_port, None).await?;
        Ok((backend, external, lifetime))
    }

    /// One PCP MAP exchange (request/response with retries). `suggested`
    /// asks the server to keep a specific external endpoint (renewals).
    async fn map_request(
        &mut self,
        local_port: u16,
        suggested: Option<SocketAddr>,
    ) -> Result<(SocketAddr, Duration)> {
        let request = build_map_request(
            self.client_ip,
            &self.nonce,
            local_port,
            suggested,
            REQUESTED_LIFETIME.as_secs() as u32,
        );
        let mut buf = [0u8; 256];
        for _try in 0..PCP_TRIES {
            self.socket
                .send(&request)
                .await
                .context("PCP send failed")?;
            match tokio::time::timeout(PCP_TIMEOUT, self.socket.recv(&mut buf)).await {
                Ok(Ok(n)) => {
                    let resp = parse_map_response(&buf[..n], &self.nonce)
                        .context("PCP response did not parse")?;
                    self.note_epoch(resp.epoch);
                    if resp.result != 0 {
                        bail!("PCP MAP refused by gateway (result code {})", resp.result);
                    }
                    return Ok((resp.external, Duration::from_secs(u64::from(resp.lifetime))));
                }
                Ok(Err(err)) => return Err(err).context("PCP recv failed"),
                Err(_) => continue, // timeout → retry
            }
        }
        bail!(
            "no PCP response from {} after {PCP_TRIES} tries",
            self.server
        )
    }

    /// RFC 6887 §8.5 epoch validation: the epoch must move forward roughly
    /// with real time; a regression means the PCP server lost state.
    fn note_epoch(&mut self, epoch: u32) -> bool {
        let now = std::time::Instant::now();
        let rebooted = match self.last_epoch {
            Some((prev_epoch, prev_at)) => {
                let elapsed = now.duration_since(prev_at).as_secs();
                // Tolerant check: reboot iff the epoch went BACKWARDS, or
                // advanced far less than wall time (server clock reset).
                u64::from(epoch) + 5 < u64::from(prev_epoch)
                    || u64::from(epoch) + 5 < u64::from(prev_epoch) + elapsed / 2
            }
            None => false,
        };
        self.last_epoch = Some((epoch, now));
        if rebooted {
            warn!(
                epoch,
                "PCP epoch went backwards — gateway rebooted; re-acquiring mapping"
            );
        }
        rebooted
    }
}

impl MappingBackend for PcpBackend {
    async fn acquire(
        &mut self,
        local_port: u16,
        suggested: Option<SocketAddr>,
    ) -> Result<(SocketAddr, Duration)> {
        self.map_request(local_port, suggested).await
    }

    async fn renew(
        &mut self,
        local_port: u16,
        current: SocketAddr,
    ) -> Result<(SocketAddr, Duration)> {
        // Renew by re-requesting with the same nonce + suggested endpoint;
        // after a reboot the gateway simply assigns fresh (possibly equal)
        // values — the manager treats a different answer as "changed".
        self.map_request(local_port, Some(current)).await
    }

    fn release_op(
        &self,
        local_port: u16,
        current: SocketAddr,
    ) -> Option<Box<dyn FnOnce() + Send + Sync>> {
        // Lifetime-0 MAP request deletes the mapping (RFC 6887 §15). Fire
        // and forget on a fresh socket: the handle's own socket is gone by
        // the time Drop runs the closure.
        let server = self.server;
        let client_ip = self.client_ip;
        let nonce = self.nonce;
        Some(Box::new(move || {
            let request = build_map_request(client_ip, &nonce, local_port, Some(current), 0);
            tokio::spawn(async move {
                if let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
                    let _ = sock.send_to(&request, server).await;
                    debug!(%server, local_port, "sent best-effort PCP delete (lifetime 0)");
                }
            });
        }))
    }

    fn name(&self) -> &'static str {
        "pcp"
    }
}

/// A parsed PCP MAP response.
struct MapResponse {
    result: u8,
    lifetime: u32,
    epoch: u32,
    external: SocketAddr,
}

/// Build a PCP v2 MAP request (RFC 6887 §11.1): 24-byte common header +
/// 36-byte MAP opcode payload.
fn build_map_request(
    client_ip: Ipv4Addr,
    nonce: &[u8; 12],
    internal_port: u16,
    suggested: Option<SocketAddr>,
    lifetime_secs: u32,
) -> Vec<u8> {
    let mut req = Vec::with_capacity(60);
    req.push(PCP_VERSION);
    req.push(PCP_OP_MAP); // R bit clear = request
    req.extend_from_slice(&[0, 0]); // reserved
    req.extend_from_slice(&lifetime_secs.to_be_bytes());
    // PCP client IP as an IPv4-mapped IPv6 address.
    req.extend_from_slice(&ipv4_mapped(client_ip));
    // MAP opcode payload.
    req.extend_from_slice(nonce);
    req.push(PROTO_UDP);
    req.extend_from_slice(&[0, 0, 0]); // reserved
    req.extend_from_slice(&internal_port.to_be_bytes());
    let (sugg_port, sugg_ip) = match suggested {
        Some(SocketAddr::V4(v4)) => (v4.port(), ipv4_mapped(*v4.ip())),
        _ => (0, ipv4_mapped(Ipv4Addr::UNSPECIFIED)),
    };
    req.extend_from_slice(&sugg_port.to_be_bytes());
    req.extend_from_slice(&sugg_ip);
    debug_assert_eq!(req.len(), 60);
    req
}

/// Parse a PCP MAP response; `None`-equivalent errors on any shape/nonce
/// mismatch so a stray datagram can never be mistaken for the gateway.
fn parse_map_response(buf: &[u8], nonce: &[u8; 12]) -> Result<MapResponse> {
    if buf.len() < 60 {
        bail!("PCP response too short ({} bytes)", buf.len());
    }
    if buf[0] != PCP_VERSION {
        bail!("PCP version mismatch ({})", buf[0]);
    }
    if buf[1] != (PCP_OP_MAP | 0x80) {
        bail!("not a MAP response (opcode {:#x})", buf[1]);
    }
    let result = buf[3];
    let lifetime = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    let epoch = u32::from_be_bytes(buf[8..12].try_into().unwrap());
    if &buf[24..36] != nonce {
        bail!("PCP nonce mismatch (stray or spoofed response)");
    }
    // MAP response payload: nonce 24..36, protocol 36, reserved 37..40,
    // internal port 40..42, ASSIGNED external port 42..44, IP 44..60.
    let port = u16::from_be_bytes(buf[42..44].try_into().unwrap());
    let ip = parse_mapped_ipv4(&buf[44..60]).context("PCP external address not IPv4")?;
    Ok(MapResponse {
        result,
        lifetime,
        epoch,
        external: SocketAddr::new(IpAddr::V4(ip), port),
    })
}

/// Encode an IPv4 address as the IPv4-mapped IPv6 form PCP uses on the wire.
fn ipv4_mapped(ip: Ipv4Addr) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[10] = 0xff;
    out[11] = 0xff;
    out[12..16].copy_from_slice(&ip.octets());
    out
}

/// Decode an IPv4-mapped IPv6 wire address back to IPv4.
fn parse_mapped_ipv4(buf: &[u8]) -> Option<Ipv4Addr> {
    if buf.len() != 16 || buf[..10] != [0u8; 10] || buf[10] != 0xff || buf[11] != 0xff {
        return None;
    }
    Some(Ipv4Addr::new(buf[12], buf[13], buf[14], buf[15]))
}

/// Default IPv4 gateway from `/proc/net/route` (Linux). Other platforms have
/// no PCP path yet and fall straight to UPnP (which discovers the gateway by
/// SSDP multicast instead).
#[cfg(target_os = "linux")]
fn default_gateway_v4() -> Option<Ipv4Addr> {
    let table = std::fs::read_to_string("/proc/net/route").ok()?;
    for line in table.lines().skip(1) {
        let mut cols = line.split_whitespace();
        let _iface = cols.next()?;
        let dest = cols.next()?;
        let gw = cols.next()?;
        if dest == "00000000" {
            let raw = u32::from_str_radix(gw, 16).ok()?;
            // /proc/net/route stores addresses little-endian.
            return Some(Ipv4Addr::from(raw.swap_bytes().to_be_bytes()));
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn default_gateway_v4() -> Option<Ipv4Addr> {
    None
}

// ---------------------------------------------------------------------------
// UPnP-IGD backend (igd_next, the pre-Fase-5 one-shot path made renewable)
// ---------------------------------------------------------------------------

/// UPnP-IGD backend: same discovery/mapping as the legacy one-shot
/// `upnp_candidate`, plus renewal (`add_port` with the SAME external port
/// refreshes the lease) and delete-on-drop.
pub struct UpnpBackend {
    gateway: igd_next::aio::Gateway<igd_next::aio::tokio::Tokio>,
    local: SocketAddr,
}

impl UpnpBackend {
    async fn probe(local_port: u16) -> Result<(Self, SocketAddr, Duration)> {
        use igd_next::aio::tokio as igd;
        use igd_next::{PortMappingProtocol, SearchOptions};

        let local_ip = crate::holepunch::primary_local_ip().context("no local IPv4 for UPnP")?;
        let local = SocketAddr::new(local_ip, local_port);
        let options = SearchOptions {
            timeout: Some(Duration::from_secs(2)),
            ..Default::default()
        };
        let gateway = igd::search_gateway(options)
            .await
            .context("no UPnP-IGD gateway found")?;
        let external_port = gateway
            .add_any_port(
                PortMappingProtocol::UDP,
                local,
                REQUESTED_LIFETIME.as_secs() as u32,
                "bore",
            )
            .await
            .context("UPnP-IGD port mapping request failed")?;
        let wan = gateway
            .get_external_ip()
            .await
            .context("UPnP-IGD external IP query failed")?;
        Ok((
            Self { gateway, local },
            SocketAddr::new(wan, external_port),
            REQUESTED_LIFETIME,
        ))
    }
}

impl MappingBackend for UpnpBackend {
    async fn acquire(
        &mut self,
        _local_port: u16,
        _suggested: Option<SocketAddr>,
    ) -> Result<(SocketAddr, Duration)> {
        bail!("UPnP acquire happens in probe(); renew is the live path")
    }

    async fn renew(
        &mut self,
        _local_port: u16,
        current: SocketAddr,
    ) -> Result<(SocketAddr, Duration)> {
        use igd_next::PortMappingProtocol;
        // `add_port` on the SAME external port refreshes the lease in place.
        self.gateway
            .add_port(
                PortMappingProtocol::UDP,
                current.port(),
                self.local,
                REQUESTED_LIFETIME.as_secs() as u32,
                "bore",
            )
            .await
            .context("UPnP-IGD lease refresh failed")?;
        // The WAN IP can change under DHCP; re-read it so a change surfaces.
        let wan = self
            .gateway
            .get_external_ip()
            .await
            .context("UPnP-IGD external IP re-query failed")?;
        Ok((SocketAddr::new(wan, current.port()), REQUESTED_LIFETIME))
    }

    fn release_op(
        &self,
        _local_port: u16,
        current: SocketAddr,
    ) -> Option<Box<dyn FnOnce() + Send + Sync>> {
        let gateway = self.gateway.clone();
        Some(Box::new(move || {
            tokio::spawn(async move {
                use igd_next::PortMappingProtocol;
                match gateway
                    .remove_port(PortMappingProtocol::UDP, current.port())
                    .await
                {
                    Ok(()) => debug!(port = current.port(), "UPnP mapping released"),
                    Err(err) => debug!(%err, "best-effort UPnP release failed (ignored)"),
                }
            });
        }))
    }

    fn name(&self) -> &'static str {
        "upnp"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// Scriptable backend: each renew pops the next outcome.
    struct MockBackend {
        script: std::sync::Mutex<Vec<Result<(SocketAddr, Duration)>>>,
        renews: Arc<AtomicU32>,
        released: Arc<std::sync::Mutex<Vec<SocketAddr>>>,
    }

    impl MappingBackend for MockBackend {
        async fn acquire(
            &mut self,
            _local_port: u16,
            _suggested: Option<SocketAddr>,
        ) -> Result<(SocketAddr, Duration)> {
            unreachable!("manager never calls acquire; probe does")
        }

        async fn renew(
            &mut self,
            _local_port: u16,
            current: SocketAddr,
        ) -> Result<(SocketAddr, Duration)> {
            self.renews.fetch_add(1, Ordering::SeqCst);
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                return Ok((current, Duration::from_secs(20)));
            }
            script.remove(0)
        }

        fn release_op(
            &self,
            _local_port: u16,
            current: SocketAddr,
        ) -> Option<Box<dyn FnOnce() + Send + Sync>> {
            let released = Arc::clone(&self.released);
            Some(Box::new(move || {
                released.lock().unwrap().push(current);
            }))
        }

        fn name(&self) -> &'static str {
            "mock"
        }
    }

    fn addr(port: u16) -> SocketAddr {
        format!("198.51.100.9:{port}").parse().unwrap()
    }

    /// Renewal keeps firing at half-lifetime; a renew answering a DIFFERENT
    /// endpoint publishes it on the watch channel (re-offer trigger), and a
    /// failed renew retries with backoff instead of dying. Fake-clock test:
    /// `start_paused` auto-advances time, so no real seconds pass.
    #[tokio::test(start_paused = true)]
    async fn manager_renews_publishes_changes_and_survives_failures() -> Result<()> {
        let renews = Arc::new(AtomicU32::new(0));
        let released = Arc::new(std::sync::Mutex::new(Vec::new()));
        let backend = MockBackend {
            script: std::sync::Mutex::new(vec![
                Ok((addr(1000), Duration::from_secs(20))), // same → no change event
                Err(anyhow::anyhow!("gateway asleep")),    // failure → backoff, retry
                Ok((addr(2000), Duration::from_secs(20))), // CHANGED → publish
            ]),
            renews: Arc::clone(&renews),
            released: Arc::clone(&released),
        };
        let lease = spawn_manager(backend, 4000, addr(1000), Duration::from_secs(20));
        let mut changed = lease.changed();
        assert_eq!(*changed.borrow(), addr(1000));

        // The watch flips to the reassigned endpoint after the failure+retry.
        tokio::time::timeout(Duration::from_secs(120), changed.changed())
            .await
            .expect("no change published")?;
        assert_eq!(*changed.borrow(), addr(2000));
        assert!(
            renews.load(Ordering::SeqCst) >= 3,
            "renew loop must keep running"
        );

        // Drop → release closure fires with the ORIGINAL registration target.
        drop(lease);
        assert_eq!(released.lock().unwrap().as_slice(), &[addr(1000)]);
        Ok(())
    }

    /// PCP wire matrix against a FAKE GATEWAY on loopback: acquire, renew
    /// (same endpoint), gateway reboot (epoch reset → renew re-acquires and
    /// reports the NEW endpoint), and the lifetime-0 delete on drop.
    #[tokio::test]
    async fn pcp_fake_gateway_acquire_renew_reboot_delete() -> Result<()> {
        let gateway = UdpSocket::bind("127.0.0.1:0").await?;
        let gw_addr = gateway.local_addr()?;
        let deletes = Arc::new(AtomicU32::new(0));
        let deletes_srv = Arc::clone(&deletes);
        tokio::spawn(async move {
            let mut buf = [0u8; 128];
            let mut reqs = 0u32;
            loop {
                let Ok((n, from)) = gateway.recv_from(&mut buf).await else {
                    break;
                };
                let req = &buf[..n];
                if n < 60 || req[0] != PCP_VERSION || req[1] != PCP_OP_MAP {
                    continue;
                }
                let lifetime = u32::from_be_bytes(req[4..8].try_into().unwrap());
                if lifetime == 0 {
                    deletes_srv.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
                reqs += 1;
                // Request 1-2: epoch grows, port 6000. Request 3+: REBOOT —
                // epoch resets small AND the assigned port changes.
                let (epoch, port) = match reqs {
                    1 => (1000u32, 6000u16),
                    2 => (1010, 6000),
                    _ => (3, 6001),
                };
                let mut resp = vec![0u8; 60];
                resp[0] = PCP_VERSION;
                resp[1] = PCP_OP_MAP | 0x80;
                resp[3] = 0; // SUCCESS
                resp[4..8].copy_from_slice(&600u32.to_be_bytes());
                resp[8..12].copy_from_slice(&epoch.to_be_bytes());
                resp[24..36].copy_from_slice(&req[24..36]); // echo nonce
                resp[36] = PROTO_UDP;
                resp[40..42].copy_from_slice(&req[40..42]); // internal port
                resp[42..44].copy_from_slice(&port.to_be_bytes()); // assigned port
                resp[44..60].copy_from_slice(&ipv4_mapped("203.0.113.50".parse().unwrap()));
                let _ = gateway.send_to(&resp, from).await;
            }
        });

        // Build a backend wired straight at the fake gateway (no route parse).
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        socket.connect(gw_addr).await?;
        let client_ip = match socket.local_addr()?.ip() {
            IpAddr::V4(ip) => ip,
            _ => unreachable!(),
        };
        let mut backend = PcpBackend {
            socket,
            server: gw_addr,
            client_ip,
            nonce: [7u8; 12],
            last_epoch: None,
        };
        let (external, lifetime) = backend.map_request(5000, None).await?;
        assert_eq!(external.port(), 6000);
        assert_eq!(lifetime, Duration::from_secs(600));

        // Renew: same endpoint, epoch advanced → no reboot.
        let (renewed, _) = backend.renew(5000, external).await?;
        assert_eq!(renewed, external);

        // Reboot: epoch resets, mapping moves → renew reports the NEW port.
        let (after_reboot, _) = backend.renew(5000, external).await?;
        assert_eq!(
            after_reboot.port(),
            6001,
            "re-acquired mapping must surface"
        );

        // Delete on drop: lifetime-0 request reaches the gateway.
        let release = backend.release_op(5000, after_reboot).unwrap();
        release();
        for _ in 0..50 {
            if deletes.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            deletes.load(Ordering::SeqCst) >= 1,
            "lifetime-0 delete not seen"
        );
        Ok(())
    }

    /// The wire builders round-trip and reject tampered responses (wrong
    /// nonce, short frame, wrong opcode) — a stray datagram can never be
    /// mistaken for the gateway.
    #[test]
    fn pcp_frame_roundtrip_and_rejection() {
        let nonce = [9u8; 12];
        let req = build_map_request("192.168.1.10".parse().unwrap(), &nonce, 4321, None, 120);
        assert_eq!(req.len(), 60);
        assert_eq!(req[0], PCP_VERSION);
        assert_eq!(req[1], PCP_OP_MAP);
        assert_eq!(&req[24..36], &nonce);
        assert_eq!(u16::from_be_bytes(req[40..42].try_into().unwrap()), 4321);

        // A well-formed response parses…
        let mut resp = vec![0u8; 60];
        resp[0] = PCP_VERSION;
        resp[1] = PCP_OP_MAP | 0x80;
        resp[4..8].copy_from_slice(&120u32.to_be_bytes());
        resp[8..12].copy_from_slice(&42u32.to_be_bytes());
        resp[24..36].copy_from_slice(&nonce);
        resp[42..44].copy_from_slice(&7777u16.to_be_bytes());
        resp[44..60].copy_from_slice(&ipv4_mapped("203.0.113.7".parse().unwrap()));
        let parsed = parse_map_response(&resp, &nonce).unwrap();
        assert_eq!(parsed.external, "203.0.113.7:7777".parse().unwrap());
        assert_eq!(parsed.epoch, 42);

        // …and every tamper is rejected.
        assert!(parse_map_response(&resp[..30], &nonce).is_err(), "short");
        let mut bad = resp.clone();
        bad[24] ^= 0xff;
        assert!(parse_map_response(&bad, &nonce).is_err(), "nonce");
        let mut bad = resp.clone();
        bad[1] = 0x82; // PEER response (opcode 2), not MAP
        assert!(parse_map_response(&bad, &nonce).is_err(), "opcode");
    }

    /// Epoch validation table (RFC 6887 §8.5-ish): forward is fine, backward
    /// or wildly-behind-wall-time is a reboot.
    #[test]
    fn pcp_epoch_reboot_detection() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let server = socket.local_addr().unwrap();
            let mut b = PcpBackend {
                socket,
                server,
                client_ip: Ipv4Addr::LOCALHOST,
                nonce: [0u8; 12],
                last_epoch: None,
            };
            assert!(!b.note_epoch(1000), "first observation is never a reboot");
            assert!(!b.note_epoch(1001), "forward epoch is healthy");
            assert!(b.note_epoch(3), "epoch regression means reboot");
        });
    }
}
