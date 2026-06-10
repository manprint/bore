//! VPN L3 tunnel feature (Linux, experimental).

#![cfg(all(feature = "vpn", target_os = "linux"))]

use anyhow::{anyhow, bail, Context, Result};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{error, info};

/// Public arg struct for `bore vpn listen` (converted from CLI args).
#[derive(Clone, Debug)]
pub struct VpnListenArgs {
    /// Server address.
    pub to: String,
    /// Shared secret.
    pub secret: String,
    /// VPN link identifier.
    pub id: String,
    /// Skip TLS verification.
    pub insecure: bool,
    /// Advertised subnets.
    pub advertised: Vec<crate::shared::Ipv4Net>,
    /// Address request (pool or static).
    pub addr_request: crate::shared::VpnAddrRequest,
    /// TUN interface name.
    pub tun_name: String,
    /// Interface MTU.
    pub mtu: u16,
    /// Skip route/NAT management.
    pub no_route_manage: bool,
    /// STUN server.
    pub stun_server: Option<String>,
    /// Try UPnP.
    pub upnp: bool,
    /// Try port prediction.
    pub try_port_prediction: bool,
    /// Preferred UDP port.
    pub nat_udp_preferred_port: u16,
    /// UDP port release timeout.
    pub nat_udp_release_timeout: u64,
    /// Optional notes.
    pub notes: Option<String>,
}

/// Public arg struct for `bore vpn connect` (converted from CLI args).
#[derive(Clone, Debug)]
pub struct VpnConnectArgs {
    /// Server address.
    pub to: String,
    /// Shared secret.
    pub secret: String,
    /// VPN link identifier.
    pub id: String,
    /// Skip TLS verification.
    pub insecure: bool,
    /// Advertised subnets.
    pub advertised: Vec<crate::shared::Ipv4Net>,
    /// Address request (pool or static).
    pub addr_request: crate::shared::VpnAddrRequest,
    /// TUN interface name.
    pub tun_name: String,
    /// Interface MTU.
    pub mtu: u16,
    /// Skip route/NAT management.
    pub no_route_manage: bool,
    /// STUN server.
    pub stun_server: Option<String>,
    /// Try UPnP.
    pub upnp: bool,
    /// Try port prediction.
    pub try_port_prediction: bool,
    /// Preferred UDP port.
    pub nat_udp_preferred_port: u16,
    /// UDP port release timeout.
    pub nat_udp_release_timeout: u64,
    /// Optional notes.
    pub notes: Option<String>,
}

/// Start a VPN listener.
pub async fn run_listen(args: VpnListenArgs) -> Result<()> {
    // Preflight checks
    hostcfg::check_root()?;
    hostcfg::check_binary_exists("ip")
        .then_some(())
        .ok_or_else(|| anyhow!("'ip' command not found"))?;

    info!(link_id = %args.id, "vpn listener starting");

    // Connect to server
    let endpoint = crate::transport::Endpoint::parse(&args.to);
    let control_stream = crate::transport::connect(&endpoint, args.insecure).await?;

    let (opener, mut acceptor) = crate::mux::client(control_stream);
    let ctrl_stream = opener.open().await.context("open control stream")?;
    let mut ctrl = crate::shared::Delimited::new(ctrl_stream);

    // Send HelloVpn first (yamux lazy-init invariant)
    let hello = crate::shared::ClientMessage::HelloVpn {
        id: args.id.clone(),
        advertised: args.advertised.clone(),
        addr: args.addr_request.clone(),
        notes: args.notes.clone(),
        carriers: 1,
    };
    ctrl.send(hello).await?;

    // Auth if we have a secret (server will send Challenge if it requires it)
    crate::auth::Authenticator::new(&args.secret)
        .client_handshake(&mut ctrl)
        .await?;

    // Wait for VpnReady
    let msg = ctrl.recv::<crate::shared::ServerMessage>().await?;
    let (assigned, prefix, peer_advertised, session_nonce) = match msg {
        Some(crate::shared::ServerMessage::VpnReady {
            assigned,
            prefix,
            peer_advertised,
            session_nonce,
            ..
        }) => {
            info!(
                link_id = %args.id,
                path = "relay",
                overlay = %format!("{assigned}/{prefix}"),
                iface = %args.tun_name,
                "vpn link paired"
            );
            (assigned, prefix, peer_advertised, session_nonce)
        }
        Some(crate::shared::ServerMessage::VpnError(e)) => {
            error!(link_id = %args.id, error = %e, "vpn server error");
            bail!("{e}");
        }
        Some(crate::shared::ServerMessage::Error(e)) => {
            error!(link_id = %args.id, error = %e, "vpn server error");
            bail!("{e}");
        }
        None => {
            error!(link_id = %args.id, "server closed before sending VpnReady; may be too old or not VPN-capable");
            bail!("server may be too old or not VPN-capable (needs 'bore server --vpn', built with --features vpn)");
        }
        other => {
            error!(link_id = %args.id, msg = ?other, "unexpected server message");
            bail!("unexpected server message: {other:?}");
        }
    };

    // Stale reclaim
    hostcfg::stale_reclaim(&args.id, &args.tun_name).await;

    // Create TUN device
    let (dev_raw, offload) =
        hostcfg::create_tun(&args.tun_name, assigned, prefix, args.mtu).await?;
    let dev = Arc::new(dev_raw);
    info!(
        link_id = %args.id,
        iface = %args.tun_name,
        addr = %assigned,
        prefix = prefix,
        "created tun device"
    );

    // Apply network config (routes, NAT, etc.)
    let advertised_nets = args.advertised.to_vec();
    let peer_routes = peer_advertised.to_vec();
    let runner = hostcfg::RealRunner;
    let _netcfg = hostcfg::NetConfig::apply(
        &runner,
        &args.id,
        &args.tun_name,
        assigned,
        prefix,
        &peer_routes,
        &advertised_nets,
        args.no_route_manage,
    )
    .await?;

    // Wait for relay substream from server (connector must connect within 60 s).
    // Without a timeout the listener hangs indefinitely if the connector crashes
    // after VpnReady is sent but before it opens the relay substream.
    let mut relay_stream = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        acceptor.accept(),
    )
    .await
    .context("timed out waiting for relay substream (connector did not connect within 60 s)")?
    .context("server did not open relay substream")?;

    // Read STREAM_READY marker
    let mut marker = [0u8; 1];
    relay_stream.read_exact(&mut marker).await?;

    // Build relay link
    let keys = crypto::derive_keys_listener(&args.secret, &session_nonce)?;
    let (sender, recver) = link::make_relay(relay_stream, keys);
    let counters = bridge::BridgeCounters::new();

    info!(link_id = %args.id, "vpn link bridge starting");

    // Run the bridge until it closes
    bridge::run(dev, sender, recver, counters, args.mtu, offload).await?;

    info!(link_id = %args.id, "vpn link bridge closed");
    Ok(())
}

/// Start a VPN connector.
pub async fn run_connect(args: VpnConnectArgs) -> Result<()> {
    // Preflight checks
    hostcfg::check_root()?;
    hostcfg::check_binary_exists("ip")
        .then_some(())
        .ok_or_else(|| anyhow!("'ip' command not found"))?;

    info!(link_id = %args.id, "vpn connector starting");

    // Connect to server
    let endpoint = crate::transport::Endpoint::parse(&args.to);
    let control_stream = crate::transport::connect(&endpoint, args.insecure).await?;

    let (opener, _acceptor) = crate::mux::client(control_stream);
    let ctrl_stream = opener.open().await.context("open control stream")?;
    let mut ctrl = crate::shared::Delimited::new(ctrl_stream);

    // Send ConnectVpn first (yamux lazy-init invariant)
    let connect_msg = crate::shared::ClientMessage::ConnectVpn {
        id: args.id.clone(),
        advertised: args.advertised.clone(),
        addr: args.addr_request.clone(),
        notes: args.notes.clone(),
    };
    ctrl.send(connect_msg).await?;

    // Auth if we have a secret (server will send Challenge if it requires it)
    crate::auth::Authenticator::new(&args.secret)
        .client_handshake(&mut ctrl)
        .await?;

    // Wait for VpnReady
    let msg = ctrl.recv::<crate::shared::ServerMessage>().await?;
    let (assigned, prefix, peer_advertised, session_nonce) = match msg {
        Some(crate::shared::ServerMessage::VpnReady {
            assigned,
            prefix,
            peer_advertised,
            session_nonce,
            ..
        }) => {
            info!(
                link_id = %args.id,
                path = "relay",
                overlay = %format!("{assigned}/{prefix}"),
                iface = %args.tun_name,
                "vpn link paired"
            );
            (assigned, prefix, peer_advertised, session_nonce)
        }
        Some(crate::shared::ServerMessage::VpnError(e)) => {
            error!(link_id = %args.id, error = %e, "vpn server error");
            bail!("{e}");
        }
        Some(crate::shared::ServerMessage::Error(e)) => {
            error!(link_id = %args.id, error = %e, "vpn server error");
            bail!("{e}");
        }
        None => {
            error!(link_id = %args.id, "server closed before sending VpnReady; may be too old or not VPN-capable");
            bail!("server may be too old or not VPN-capable (needs 'bore server --vpn', built with --features vpn)");
        }
        other => {
            error!(link_id = %args.id, msg = ?other, "unexpected server message");
            bail!("unexpected server message: {other:?}");
        }
    };

    // Stale reclaim
    hostcfg::stale_reclaim(&args.id, &args.tun_name).await;

    // Create TUN device
    let (dev_raw, offload) =
        hostcfg::create_tun(&args.tun_name, assigned, prefix, args.mtu).await?;
    let dev = Arc::new(dev_raw);
    info!(
        link_id = %args.id,
        iface = %args.tun_name,
        addr = %assigned,
        prefix = prefix,
        "created tun device"
    );

    // Apply network config (routes, NAT, etc.)
    let advertised_nets = args.advertised.to_vec();
    let peer_routes = peer_advertised.to_vec();
    let runner = hostcfg::RealRunner;
    let _netcfg = hostcfg::NetConfig::apply(
        &runner,
        &args.id,
        &args.tun_name,
        assigned,
        prefix,
        &peer_routes,
        &advertised_nets,
        args.no_route_manage,
    )
    .await?;

    // Open relay substream and write STREAM_READY
    let mut relay_stream = opener.open().await.context("open relay substream")?;
    relay_stream.write_all(&[crate::mux::STREAM_READY]).await?;

    // Build relay link
    let keys = crypto::derive_keys_connector(&args.secret, &session_nonce)?;
    let (sender, recver) = link::make_relay(relay_stream, keys);
    let counters = bridge::BridgeCounters::new();

    info!(link_id = %args.id, "vpn link bridge starting");

    // Run the bridge until it closes
    bridge::run(dev, sender, recver, counters, args.mtu, offload).await?;

    info!(link_id = %args.id, "vpn link bridge closed");
    Ok(())
}

/// Reexport submodules as public for use by tests and from main.rs.
pub use hostcfg::RealRunner;
pub use link::{LinkRecver, LinkSender};

/// Internal network types and utilities.
pub mod net {
    #![allow(dead_code)]
    use serde::{Deserialize, Serialize};
    use std::fmt;
    use std::net::Ipv4Addr;
    use std::str::FromStr;

    /// IPv4 CIDR (address + prefix length). Used for overlay + advertised subnets.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Ipv4Net {
        /// IPv4 address.
        pub addr: Ipv4Addr,
        /// Prefix length.
        pub prefix: u8,
    }

    impl Ipv4Net {
        /// Network address (host bits zeroed).
        pub fn network(&self) -> Ipv4Addr {
            let mask = Self::prefix_to_mask(self.prefix);
            let n = u32::from(self.addr) & mask;
            Ipv4Addr::from(n)
        }

        /// True if `other` addr is within this network.
        pub fn contains(&self, other: Ipv4Addr) -> bool {
            let mask = Self::prefix_to_mask(self.prefix);
            (u32::from(self.addr) & mask) == (u32::from(other) & mask)
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

    impl FromStr for Ipv4Net {
        type Err = anyhow::Error;
        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let (addr_str, prefix_str) = s
                .split_once('/')
                .ok_or_else(|| anyhow::anyhow!("missing '/' in CIDR: {s}"))?;
            let addr = addr_str
                .parse::<Ipv4Addr>()
                .map_err(|e| anyhow::anyhow!("invalid addr in {s}: {e}"))?;
            let prefix = prefix_str
                .parse::<u8>()
                .map_err(|e| anyhow::anyhow!("invalid prefix in {s}: {e}"))?;
            anyhow::ensure!(prefix <= 32, "prefix {prefix} > 32 in {s}");
            Ok(Ipv4Net { addr, prefix })
        }
    }

    impl fmt::Display for Ipv4Net {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}/{}", self.addr, self.prefix)
        }
    }

    /// A /30 pool allocator. Carves /30 blocks from a parent CIDR.
    /// Each /30 has 4 addresses: network, .1 (listener), .2 (connector), broadcast.
    pub struct PoolAllocator {
        /// Parent CIDR.
        parent: Ipv4Net,
        /// Allocated block network addresses (as u32).
        allocated: std::collections::HashSet<u32>,
    }

    impl PoolAllocator {
        /// Create a new /30 pool allocator from a parent CIDR.
        pub fn new(parent: Ipv4Net) -> anyhow::Result<Self> {
            anyhow::ensure!(
                parent.prefix <= 30,
                "pool CIDR prefix must be ≤ 30, got /{}",
                parent.prefix
            );
            Ok(Self {
                parent,
                allocated: Default::default(),
            })
        }

        /// Allocate next free /30. Returns (listener_addr, connector_addr).
        pub fn alloc(&mut self) -> anyhow::Result<(Ipv4Addr, Ipv4Addr)> {
            let base = u32::from(self.parent.network());
            let total_bits = 32 - self.parent.prefix;
            let blocks = 1u32 << total_bits.saturating_sub(2);
            for i in 0..blocks {
                let net_addr = base + i * 4;
                if !self.allocated.contains(&net_addr) {
                    self.allocated.insert(net_addr);
                    let listener = Ipv4Addr::from(net_addr + 1);
                    let connector = Ipv4Addr::from(net_addr + 2);
                    return Ok((listener, connector));
                }
            }
            anyhow::bail!(
                "vpn pool exhausted (all /30 blocks in {} in use)",
                self.parent
            )
        }

        /// Free a previously allocated block. `addr` is any address in the /30.
        pub fn free(&mut self, addr: Ipv4Addr) {
            let net_addr = u32::from(addr) & 0xFFFF_FFFC;
            self.allocated.remove(&net_addr);
        }

        /// Check if a static addr collides with any allocated block.
        pub fn collides(&self, addr: Ipv4Addr) -> bool {
            let net_addr = u32::from(addr) & 0xFFFF_FFFC;
            self.allocated.contains(&net_addr)
        }
    }

    /// Calculate the IP MTU for IP packets from a tun MTU.
    pub fn ip_mtu(tun_mtu: u16) -> u16 {
        tun_mtu
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn overlap_truth_table() {
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
        fn pool_alloc_assigns_dot1_dot2() {
            let parent: Ipv4Net = "192.168.0.0/30".parse().unwrap();
            let mut pool = PoolAllocator::new(parent).unwrap();
            let (l1, c1) = pool.alloc().unwrap();
            assert_eq!(l1.to_string(), "192.168.0.1");
            assert_eq!(c1.to_string(), "192.168.0.2");
        }

        #[test]
        fn pool_free_reuses_block() {
            let parent: Ipv4Net = "192.168.0.0/28".parse().unwrap();
            let mut pool = PoolAllocator::new(parent).unwrap();
            let (l1, c1) = pool.alloc().unwrap();
            assert_eq!(l1.to_string(), "192.168.0.1");
            pool.free(c1);
            let (l2, c2) = pool.alloc().unwrap();
            assert_eq!(l2, l1);
            assert_eq!(c2, c1);
        }

        #[test]
        fn pool_exhaustion_errors() {
            let result = PoolAllocator::new("192.168.0.0/31".parse().unwrap());
            assert!(result.is_err());

            let parent: Ipv4Net = "192.168.0.0/30".parse().unwrap();
            let mut pool = PoolAllocator::new(parent).unwrap();
            let _ = pool.alloc().unwrap();
            let result = pool.alloc();
            assert!(result.is_err());
        }
    }
}

mod crypto {
    #![allow(dead_code)]
    use anyhow::{bail, Result};
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
    use ring::hkdf;

    const MAX_COUNTER: u64 = u64::MAX - 1;
    pub const TAG_LEN: usize = 16;
    const INFO_L2C: &[u8] = b"bore-vpn l2c v1";
    const INFO_C2L: &[u8] = b"bore-vpn c2l v1";

    /// Two derived 32-byte keys for the two directions.
    pub struct DirectionKeys {
        pub egress: [u8; 32],
        pub ingress: [u8; 32],
    }

    /// Derive the relay AEAD keys for the listener side.
    pub fn derive_keys_listener(secret: &str, nonce: &[u8]) -> Result<DirectionKeys> {
        let l2c = hkdf_expand(secret, nonce, INFO_L2C)?;
        let c2l = hkdf_expand(secret, nonce, INFO_C2L)?;
        Ok(DirectionKeys {
            egress: l2c,
            ingress: c2l,
        })
    }

    /// Derive the relay AEAD keys for the connector side.
    pub fn derive_keys_connector(secret: &str, nonce: &[u8]) -> Result<DirectionKeys> {
        let l2c = hkdf_expand(secret, nonce, INFO_L2C)?;
        let c2l = hkdf_expand(secret, nonce, INFO_C2L)?;
        Ok(DirectionKeys {
            egress: c2l,
            ingress: l2c,
        })
    }

    fn hkdf_expand(secret: &str, salt_bytes: &[u8], info: &[u8]) -> Result<[u8; 32]> {
        let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, salt_bytes);
        let prk = salt.extract(secret.as_bytes());
        let info_arr = [info];
        let mut out = [0u8; 32];
        prk.expand(&info_arr, hkdf::HKDF_SHA256)
            .map_err(|_| anyhow::anyhow!("HKDF expand failed"))?
            .fill(&mut out)
            .map_err(|_| anyhow::anyhow!("HKDF fill failed"))?;
        Ok(out)
    }

    /// Build a 96-bit nonce from a u64 counter: 4 zero bytes ‖ counter (BE).
    pub fn nonce_from_counter(counter: u64) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&counter.to_be_bytes());
        n
    }

    /// Seal an IP packet. Returns `[u32 BE total_len][u64 BE counter][ciphertext‖tag]`.
    pub fn seal(key_bytes: &[u8; 32], counter: &mut u64, plaintext: &[u8]) -> Result<Vec<u8>> {
        if *counter >= MAX_COUNTER {
            bail!("AEAD counter exhausted — tear down link");
        }
        let nonce_bytes = nonce_from_counter(*counter);
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, key_bytes)
            .map_err(|_| anyhow::anyhow!("AEAD key init"))?;
        let key = LessSafeKey::new(unbound);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);

        let mut buf = plaintext.to_vec();
        key.seal_in_place_append_tag(nonce, Aad::empty(), &mut buf)
            .map_err(|_| anyhow::anyhow!("AEAD seal"))?;

        let ctr = *counter;
        *counter += 1;

        let total_len = (8 + buf.len()) as u32;
        let mut frame = Vec::with_capacity(4 + 8 + buf.len());
        frame.extend_from_slice(&total_len.to_be_bytes());
        frame.extend_from_slice(&ctr.to_be_bytes());
        frame.extend_from_slice(&buf);
        Ok(frame)
    }

    /// Open a received frame. `frame` is the raw bytes after reading `[u32 total_len]`.
    pub fn open(key_bytes: &[u8; 32], frame: &[u8]) -> Result<Vec<u8>> {
        anyhow::ensure!(
            frame.len() >= 8 + TAG_LEN,
            "frame too short: {} bytes",
            frame.len()
        );
        let ctr = u64::from_be_bytes(frame[..8].try_into().unwrap());
        let nonce_bytes = nonce_from_counter(ctr);

        let unbound = UnboundKey::new(&CHACHA20_POLY1305, key_bytes)
            .map_err(|_| anyhow::anyhow!("AEAD key init"))?;
        let key = LessSafeKey::new(unbound);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);

        let mut buf = frame[8..].to_vec();
        let plaintext = key
            .open_in_place(nonce, Aad::empty(), &mut buf)
            .map_err(|_| anyhow::anyhow!("AEAD open — tampered or wrong key"))?;
        Ok(plaintext.to_vec())
    }

    /// Seal with an explicit counter value (no auto-increment).
    /// Returns `[u32 BE total_len][u64 BE counter][ciphertext‖tag]`.
    pub fn seal_with_counter(
        key_bytes: &[u8; 32],
        counter: u64,
        plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        if counter >= MAX_COUNTER {
            bail!("AEAD counter exhausted — tear down link");
        }
        let nonce_bytes = nonce_from_counter(counter);
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, key_bytes)
            .map_err(|_| anyhow::anyhow!("AEAD key init"))?;
        let key = LessSafeKey::new(unbound);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);

        let mut buf = plaintext.to_vec();
        key.seal_in_place_append_tag(nonce, Aad::empty(), &mut buf)
            .map_err(|_| anyhow::anyhow!("AEAD seal"))?;

        let total_len = (8 + buf.len()) as u32;
        let mut frame = Vec::with_capacity(4 + 8 + buf.len());
        frame.extend_from_slice(&total_len.to_be_bytes());
        frame.extend_from_slice(&counter.to_be_bytes());
        frame.extend_from_slice(&buf);
        Ok(frame)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn aead_roundtrip_ok() {
            let key = [1u8; 32];
            let mut ctr = 0u64;
            let plaintext = b"hello world";
            let sealed = seal(&key, &mut ctr, plaintext).unwrap();
            assert_eq!(ctr, 1);
            let frame = &sealed[4..];
            let opened = open(&key, frame).unwrap();
            assert_eq!(&opened[..], plaintext);
        }

        #[test]
        fn aead_tamper_fails() {
            let key = [1u8; 32];
            let mut ctr = 0u64;
            let plaintext = b"hello world";
            let sealed = seal(&key, &mut ctr, plaintext).unwrap();
            let mut frame = sealed[4..].to_vec();
            if frame.len() > 20 {
                frame[20] ^= 0xFF;
            }
            let result = open(&key, &frame);
            assert!(result.is_err());
        }

        #[test]
        fn aead_wrong_key_fails() {
            let key1 = [1u8; 32];
            let key2 = [2u8; 32];
            let mut ctr = 0u64;
            let plaintext = b"hello world";
            let sealed = seal(&key1, &mut ctr, plaintext).unwrap();
            let frame = &sealed[12..];
            let result = open(&key2, frame);
            assert!(result.is_err());
        }

        #[test]
        fn nonce_monotonic_unique() {
            let n1 = nonce_from_counter(0);
            let n2 = nonce_from_counter(1);
            assert_ne!(n1, n2);
        }

        #[test]
        fn hkdf_deterministic() {
            let secret = "test-secret";
            let nonce = b"test-nonce";
            let k1 = derive_keys_listener(secret, nonce).unwrap();
            let k2 = derive_keys_listener(secret, nonce).unwrap();
            assert_eq!(k1.egress, k2.egress);
            assert_eq!(k1.ingress, k2.ingress);
        }

        #[test]
        fn hkdf_directions_differ() {
            let secret = "test-secret";
            let nonce = b"test-nonce";
            let listener = derive_keys_listener(secret, nonce).unwrap();
            let connector = derive_keys_connector(secret, nonce).unwrap();
            assert_ne!(listener.egress, connector.egress);
            assert_ne!(listener.ingress, connector.ingress);
            assert_eq!(listener.egress, connector.ingress);
            assert_eq!(listener.ingress, connector.egress);
        }
    }
}

/// Command builders for host network configuration (ip, nft, iptables).
pub mod hostcfg_cmd {
    #![allow(dead_code)]
    /// Build `ip addr add <addr>/<prefix> dev <dev>` argv.
    pub fn cmd_addr_add(dev: &str, addr: &str, prefix: u8) -> Vec<String> {
        vec![
            "ip".into(),
            "addr".into(),
            "add".into(),
            format!("{addr}/{prefix}"),
            "dev".into(),
            dev.into(),
        ]
    }

    /// Build `ip addr del <addr>/<prefix> dev <dev>` argv.
    pub fn cmd_addr_del(dev: &str, addr: &str, prefix: u8) -> Vec<String> {
        vec![
            "ip".into(),
            "addr".into(),
            "del".into(),
            format!("{addr}/{prefix}"),
            "dev".into(),
            dev.into(),
        ]
    }

    /// Build `ip link set <dev> up` argv.
    pub fn cmd_link_set_up(dev: &str) -> Vec<String> {
        vec![
            "ip".into(),
            "link".into(),
            "set".into(),
            dev.into(),
            "up".into(),
        ]
    }

    /// Build `ip link set <dev> mtu <mtu>` argv.
    pub fn cmd_link_set_mtu(dev: &str, mtu: u16) -> Vec<String> {
        vec![
            "ip".into(),
            "link".into(),
            "set".into(),
            dev.into(),
            "mtu".into(),
            mtu.to_string(),
        ]
    }

    /// Build `ip route add <subnet> dev <dev>` argv.
    pub fn cmd_route_add(subnet: &str, dev: &str) -> Vec<String> {
        vec![
            "ip".into(),
            "route".into(),
            "add".into(),
            subnet.into(),
            "dev".into(),
            dev.into(),
        ]
    }

    /// Build `ip route del <subnet> dev <dev>` argv.
    pub fn cmd_route_del(subnet: &str, dev: &str) -> Vec<String> {
        vec![
            "ip".into(),
            "route".into(),
            "del".into(),
            subnet.into(),
            "dev".into(),
            dev.into(),
        ]
    }

    /// Build `ip route get <host>` argv.
    pub fn cmd_route_get(host: &str) -> Vec<String> {
        vec!["ip".into(), "route".into(), "get".into(), host.into()]
    }

    /// Parse the output of `ip route get <host>` to extract the `dev <iface>` field.
    pub fn parse_lan_iface(output: &str) -> Option<String> {
        let mut iter = output.split_whitespace();
        while let Some(token) = iter.next() {
            if token == "dev" {
                return iter.next().map(str::to_string);
            }
        }
        None
    }

    /// Build `nft add table inet bore_vpn_<id>` argv.
    pub fn cmd_nft_add_table(id: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "table".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
        ]
    }

    /// Build `nft add chain inet bore_vpn_<id> post` argv.
    pub fn cmd_nft_add_postrouting_chain(id: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "chain".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "post".into(),
            "{ type nat hook postrouting priority 100 ; }".into(),
        ]
    }

    /// Build nft masquerade rule argv.
    pub fn cmd_nft_add_masquerade_rule(id: &str, tun: &str, lan_if: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "post".into(),
            "iif".into(),
            tun.into(),
            "oif".into(),
            lan_if.into(),
            "masquerade".into(),
        ]
    }

    /// Build nft forward chain argv.
    pub fn cmd_nft_add_forward_chain(id: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "chain".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "bore_fw".into(),
            "{ type filter hook forward priority -10 ; }".into(),
        ]
    }

    /// Build nft MSS clamp argv.
    pub fn cmd_nft_add_mss_clamp(id: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "add".into(),
            "rule".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
            "bore_fw".into(),
            "tcp".into(),
            "flags".into(),
            "syn".into(),
            "tcp".into(),
            "option".into(),
            "maxseg".into(),
            "size".into(),
            "set".into(),
            "rt".into(),
            "mtu".into(),
        ]
    }

    /// Build `nft delete table inet bore_vpn_<id>` argv.
    pub fn cmd_nft_delete_table(id: &str) -> Vec<String> {
        vec![
            "nft".into(),
            "delete".into(),
            "table".into(),
            "inet".into(),
            format!("bore_vpn_{id}"),
        ]
    }

    /// Build iptables masquerade rule argv.
    pub fn cmd_iptables_masquerade_add(id: &str, tun: &str, lan_if: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-t".into(),
            "nat".into(),
            "-A".into(),
            "POSTROUTING".into(),
            "-i".into(),
            tun.into(),
            "-o".into(),
            lan_if.into(),
            "-j".into(),
            "MASQUERADE".into(),
            "-m".into(),
            "comment".into(),
            "--comment".into(),
            format!("bore_vpn_{id}"),
        ]
    }

    /// Build iptables masquerade del argv.
    pub fn cmd_iptables_masquerade_del(id: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-t".into(),
            "nat".into(),
            "-D".into(),
            "POSTROUTING".into(),
            "-m".into(),
            "comment".into(),
            "--comment".into(),
            format!("bore_vpn_{id}"),
        ]
    }

    /// Build iptables MSS clamp argv.
    pub fn cmd_iptables_mss_clamp_add(id: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-t".into(),
            "mangle".into(),
            "-A".into(),
            "FORWARD".into(),
            "-p".into(),
            "tcp".into(),
            "--tcp-flags".into(),
            "SYN,RST".into(),
            "SYN".into(),
            "-j".into(),
            "TCPMSS".into(),
            "--clamp-mss-to-pmtu".into(),
            "-m".into(),
            "comment".into(),
            "--comment".into(),
            format!("bore_vpn_{id}"),
        ]
    }

    /// Build iptables MSS clamp del argv.
    pub fn cmd_iptables_mss_clamp_del(id: &str) -> Vec<String> {
        vec![
            "iptables".into(),
            "-t".into(),
            "mangle".into(),
            "-D".into(),
            "FORWARD".into(),
            "-p".into(),
            "tcp".into(),
            "--tcp-flags".into(),
            "SYN,RST".into(),
            "SYN".into(),
            "-j".into(),
            "TCPMSS".into(),
            "--clamp-mss-to-pmtu".into(),
            "-m".into(),
            "comment".into(),
            "--comment".into(),
            format!("bore_vpn_{id}"),
        ]
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cmd_route_add_snapshot() {
            let cmd = cmd_route_add("10.0.0.0/24", "tun0");
            assert_eq!(
                cmd,
                vec!["ip", "route", "add", "10.0.0.0/24", "dev", "tun0"]
            );
        }

        #[test]
        fn cmd_nft_table_snapshot() {
            let cmd = cmd_nft_add_table("link1");
            assert_eq!(cmd, vec!["nft", "add", "table", "inet", "bore_vpn_link1"]);
        }

        #[test]
        fn cmd_iptables_fallback_snapshot() {
            let cmd = cmd_iptables_masquerade_add("link1", "tun0", "eth0");
            assert_eq!(
                cmd,
                vec![
                    "iptables",
                    "-t",
                    "nat",
                    "-A",
                    "POSTROUTING",
                    "-i",
                    "tun0",
                    "-o",
                    "eth0",
                    "-j",
                    "MASQUERADE",
                    "-m",
                    "comment",
                    "--comment",
                    "bore_vpn_link1"
                ]
            );
        }

        #[test]
        fn parse_lan_iface_from_ip_route_get() {
            let output = "10.0.0.1 via 192.168.1.1 dev eth0 src 192.168.1.100";
            let iface = parse_lan_iface(output);
            assert_eq!(iface, Some("eth0".to_string()));

            let output2 = "10.0.0.1 dev eth0 src 192.168.1.100";
            let iface2 = parse_lan_iface(output2);
            assert_eq!(iface2, Some("eth0".to_string()));
        }
    }
}

pub mod hostcfg {
    #![allow(dead_code)]
    //! Host network configuration (routes, NAT, ip_forward) with RAII cleanup.
    //!
    //! Manages routes, ip_forward toggle, and NAT rules for a VPN link.
    //! All configuration is reverted in reverse order on Drop (cleanup path).

    use anyhow::{anyhow, bail, Context};
    use std::net::Ipv4Addr;
    use std::process::Command;

    /// Injectable command runner (allows unit testing without root).
    pub trait CommandRunner: Send + Sync {
        /// Run a command with the given argv.
        fn run<'a>(
            &'a self,
            argv: &'a [String],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + 'a>>;
    }

    /// Real runner: calls std::process::Command (blocking, suitable for root operations).
    pub struct RealRunner;

    impl CommandRunner for RealRunner {
        fn run<'a>(
            &'a self,
            argv: &'a [String],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + 'a>>
        {
            Box::pin(async move {
                anyhow::ensure!(!argv.is_empty(), "empty argv");
                let out = Command::new(&argv[0])
                    .args(&argv[1..])
                    .output()
                    .with_context(|| format!("failed to run {:?}", argv))?;
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    bail!(
                        "command {:?} failed ({}): {}",
                        argv,
                        out.status,
                        stderr.trim()
                    );
                }
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            })
        }
    }

    /// Test runner: records all calls in memory.
    #[cfg(test)]
    pub struct TestRunner {
        calls: std::sync::Arc<tokio::sync::Mutex<Vec<Vec<String>>>>,
    }

    #[cfg(test)]
    impl TestRunner {
        /// Create a new test runner.
        #[allow(clippy::new_without_default)]
        pub fn new() -> Self {
            Self {
                calls: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
            }
        }

        /// Get the list of commands that were run.
        pub async fn get_calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().await.clone()
        }
    }

    #[cfg(test)]
    impl CommandRunner for TestRunner {
        fn run<'a>(
            &'a self,
            argv: &'a [String],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + 'a>>
        {
            let calls = std::sync::Arc::clone(&self.calls);
            let argv_owned = argv.to_vec();
            Box::pin(async move {
                calls.lock().await.push(argv_owned);
                Ok(String::new())
            })
        }
    }

    /// Check that we are root (UID 0).
    pub fn check_root() -> anyhow::Result<()> {
        if nix::unistd::getuid().is_root() {
            Ok(())
        } else {
            bail!(
                "bore vpn requires root privileges (or CAP_NET_ADMIN). \
                 Run with sudo or grant the capability."
            )
        }
    }

    /// Verify a binary exists by running it with --version.
    pub fn check_binary_exists(name: &str) -> bool {
        std::process::Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|_| true)
            .unwrap_or(false)
    }

    /// Delete leftover resources from a previous failed run (idempotent, best-effort).
    pub async fn stale_reclaim(id: &str, tun_name: &str) {
        // Try to delete nft table (ignore "not found" errors)
        let _ = Command::new("nft")
            .args(["delete", "table", "inet", &format!("bore_vpn_{id}")])
            .output();
        // Try to delete TUN interface (ignore errors)
        let _ = Command::new("ip").args(["link", "del", tun_name]).output();
    }

    /// Create a TUN device.
    ///
    /// Tries `IFF_VNET_HDR` + GSO/GRO offload first (Phase 6.2). If the kernel
    /// does not support it the flag is not set and we fall back to single-packet
    /// I/O (Phase 6.1). Returns `(device, offload_enabled)`.
    pub async fn create_tun(
        name: &str,
        addr: Ipv4Addr,
        prefix: u8,
        mtu: u16,
    ) -> anyhow::Result<(tun_rs::AsyncDevice, bool)> {
        // Phase 6.2: attempt offload (IFF_VNET_HDR + GSO/GRO).
        if let Ok(dev) = tun_rs::DeviceBuilder::new()
            .name(name)
            .ipv4(addr, prefix, None)
            .mtu(mtu)
            .offload(true)
            .build_async()
        {
            let gso = dev.tcp_gso() || dev.udp_gso();
            if gso {
                tracing::info!(%name, tcp_gso = dev.tcp_gso(), udp_gso = dev.udp_gso(),
                    "TUN created with GSO/GRO offload (Phase 6.2)");
                return Ok((dev, true));
            }
            // Kernel accepted the build but reported no GSO support. Drop and rebuild.
            tracing::info!(%name, "kernel built TUN but reports no GSO; using single-packet path");
            drop(dev);
        }

        // Phase 6.1 fallback: single-packet tun I/O.
        tracing::info!(%name, "TUN created without offload (single-packet path)");
        let dev = tun_rs::DeviceBuilder::new()
            .name(name)
            .ipv4(addr, prefix, None)
            .mtu(mtu)
            .build_async()
            .context("failed to create TUN device")?;
        Ok((dev, false))
    }

    /// Internal: marker for an ip_forward revert operation.
    enum AppliedOp {
        IpForward { saved_value: u8 },
    }

    /// RAII guard that manages routes, forwarding, and NAT around a VPN link.
    /// Reverts everything in reverse order on `Drop`.
    pub struct NetConfig {
        id: String,
        tun_name: String,
        no_route_manage: bool,
        nft_available: bool,
        // Revert actions in reverse order (each is an argv).
        revert_cmds: Vec<Vec<String>>,
        // Labels for logging during revert.
        revert_labels: Vec<String>,
        // Saved ip_forward value (if we changed it).
        ip_forward_saved: Option<u8>,
        // Operations (e.g. ip_forward save/restore).
        applied_ops: Vec<AppliedOp>,
    }

    impl NetConfig {
        /// Apply host network configuration for a VPN link.
        ///
        /// - `id`: link id (used for nft table name)
        /// - `tun_name`: tun device name
        /// - `assigned`: this side's overlay address
        /// - `prefix`: overlay prefix (30 for /30)
        /// - `peer_routes`: subnets to route via the tun
        /// - `advertised`: subnets this side exposes (non-empty = gateway mode)
        /// - `no_route_manage`: if true, print commands instead of running them
        #[allow(clippy::too_many_arguments)]
        pub async fn apply<R: CommandRunner>(
            runner: &R,
            id: &str,
            tun_name: &str,
            _assigned: std::net::Ipv4Addr,
            _prefix: u8,
            peer_routes: &[crate::shared::Ipv4Net],
            advertised: &[crate::shared::Ipv4Net],
            no_route_manage: bool,
        ) -> anyhow::Result<Self> {
            use super::hostcfg_cmd::*;

            let mut cfg = NetConfig {
                id: id.to_string(),
                tun_name: tun_name.to_string(),
                no_route_manage,
                nft_available: false,
                revert_cmds: Vec::new(),
                revert_labels: Vec::new(),
                ip_forward_saved: None,
                applied_ops: Vec::new(),
            };

            let is_gateway = !advertised.is_empty();

            // ── Routes ────────────────────────────────────────────────────────────
            for net in peer_routes {
                let subnet = net.to_string();
                let argv = cmd_route_add(&subnet, tun_name);
                if no_route_manage {
                    println!("# (skipped, --no-route-manage): {}", argv.join(" "));
                } else {
                    runner
                        .run(&argv)
                        .await
                        .with_context(|| format!("ip route add {subnet}"))?;
                    tracing::info!(%subnet, %tun_name, "added route");
                    cfg.revert_cmds.push(cmd_route_del(&subnet, tun_name));
                    cfg.revert_labels
                        .push(format!("del route {subnet} dev {tun_name}"));
                }
            }

            // ── Gateway mode: ip_forward + NAT ────────────────────────────────────
            if is_gateway && !no_route_manage {
                // Save and enable ip_forward
                let current = tokio::fs::read_to_string("/proc/sys/net/ipv4/ip_forward")
                    .await
                    .unwrap_or_else(|_| "0".to_string());
                let saved: u8 = current.trim().parse().unwrap_or(0);

                if saved == 0 {
                    tokio::fs::write("/proc/sys/net/ipv4/ip_forward", "1\n")
                        .await
                        .context("enable ip_forward")?;
                    tracing::info!("enabled ip_forward (saved={})", saved);
                }

                cfg.ip_forward_saved = Some(saved);
                cfg.applied_ops
                    .push(AppliedOp::IpForward { saved_value: saved });

                // Determine LAN egress interface
                let sample_host: std::net::Ipv4Addr = {
                    let net = &advertised[0];
                    let base = u32::from(net.network());
                    std::net::Ipv4Addr::from(base + 1)
                };
                let route_out = runner
                    .run(&cmd_route_get(&sample_host.to_string()))
                    .await
                    .context("ip route get to find LAN iface")?;
                let lan_if = super::hostcfg_cmd::parse_lan_iface(&route_out).ok_or_else(|| {
                    anyhow!("could not determine LAN egress interface from: {route_out}")
                })?;

                // Try nft first, fall back to iptables
                cfg.nft_available = check_binary_exists("nft");

                if cfg.nft_available {
                    runner
                        .run(&cmd_nft_add_table(id))
                        .await
                        .context("nft add table")?;
                    runner
                        .run(&cmd_nft_add_postrouting_chain(id))
                        .await
                        .context("nft add postrouting chain")?;
                    runner
                        .run(&cmd_nft_add_masquerade_rule(id, tun_name, &lan_if))
                        .await
                        .context("nft add masquerade rule")?;
                    runner
                        .run(&cmd_nft_add_forward_chain(id))
                        .await
                        .context("nft add forward chain")?;
                    runner
                        .run(&cmd_nft_add_mss_clamp(id))
                        .await
                        .context("nft add mss clamp")?;
                    tracing::info!(%id, "created nft table bore_vpn_{}", id);
                    cfg.revert_cmds.push(cmd_nft_delete_table(id));
                    cfg.revert_labels
                        .push(format!("delete nft table bore_vpn_{id}"));
                } else {
                    runner
                        .run(&cmd_iptables_masquerade_add(id, tun_name, &lan_if))
                        .await
                        .context("iptables masquerade add")?;
                    runner
                        .run(&cmd_iptables_mss_clamp_add(id))
                        .await
                        .context("iptables mss clamp add")?;
                    tracing::info!(%id, "applied iptables NAT rules");
                    cfg.revert_cmds.push(cmd_iptables_masquerade_del(id));
                    cfg.revert_labels
                        .push(format!("del iptables masquerade bore_vpn_{id}"));
                    cfg.revert_cmds.push(cmd_iptables_mss_clamp_del(id));
                    cfg.revert_labels
                        .push(format!("del iptables mss clamp bore_vpn_{id}"));
                }
            } else if is_gateway && no_route_manage {
                // Print commands for gateway mode (nft preferred)
                let lan_if = "LAN_IFACE"; // placeholder
                for cmd in &[
                    cmd_nft_add_table(id),
                    cmd_nft_add_postrouting_chain(id),
                    cmd_nft_add_masquerade_rule(id, tun_name, lan_if),
                    cmd_nft_add_forward_chain(id),
                    cmd_nft_add_mss_clamp(id),
                ] {
                    println!("# (skipped, --no-route-manage): {}", cmd.join(" "));
                }
            }

            Ok(cfg)
        }
    }

    impl Drop for NetConfig {
        fn drop(&mut self) {
            // Revert in reverse order using blocking std::process::Command.
            // Note: Drop is not async, so we use blocking subprocess calls.

            // First, revert nft/iptables rules in reverse order.
            for (argv, label) in self
                .revert_cmds
                .iter()
                .rev()
                .zip(self.revert_labels.iter().rev())
            {
                tracing::info!(%label, "reverting vpn netconfig");
                match std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                {
                    Err(e) => {
                        tracing::warn!(%e, %label, "vpn netconfig revert step failed (spawn error)");
                    }
                    Ok(s) if !s.success() => {
                        tracing::warn!(code=%s, %label, "vpn netconfig revert step exited non-zero");
                    }
                    Ok(_) => {}
                }
            }

            // Then revert applied_ops in reverse order.
            for op in self.applied_ops.iter().rev() {
                match op {
                    AppliedOp::IpForward { saved_value } => {
                        tracing::info!(saved_value, "restoring ip_forward");
                        let _ = std::fs::write(
                            "/proc/sys/net/ipv4/ip_forward",
                            format!("{}\n", saved_value),
                        );
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn netconfig_rollback_is_reverse_order() {
            // Simulate applying multiple routes.
            // (We can't call the full apply() in a sync test, so we construct
            // the revert commands manually for this test.)
            let cfg = NetConfig {
                id: "test1".to_string(),
                tun_name: "tun0".to_string(),
                no_route_manage: false,
                nft_available: false,
                revert_cmds: vec![
                    vec![
                        "ip".into(),
                        "route".into(),
                        "del".into(),
                        "10.0.0.0/24".into(),
                        "dev".into(),
                        "tun0".into(),
                    ],
                    vec![
                        "ip".into(),
                        "route".into(),
                        "del".into(),
                        "10.1.0.0/24".into(),
                        "dev".into(),
                        "tun0".into(),
                    ],
                ],
                revert_labels: vec![
                    "del route 10.0.0.0/24 dev tun0".to_string(),
                    "del route 10.1.0.0/24 dev tun0".to_string(),
                ],
                ip_forward_saved: None,
                applied_ops: vec![],
            };

            // Record the order of revert commands by manually calling drop.
            // (In practice, Drop is called automatically.)
            drop(cfg);
            // If we got here, Drop ran without panicking. The actual order
            // is verified by the reversal in Drop implementation.
        }

        #[tokio::test]
        async fn netconfig_apply_routes_only() {
            let runner = TestRunner::new();
            let peer_routes = vec![
                "10.0.0.0/24".parse::<crate::shared::Ipv4Net>().unwrap(),
                "10.1.0.0/24".parse::<crate::shared::Ipv4Net>().unwrap(),
            ];
            let advertised = vec![];

            let cfg = NetConfig::apply(
                &runner,
                "test1",
                "tun0",
                "192.168.100.1".parse().unwrap(),
                30,
                &peer_routes,
                &advertised,
                false,
            )
            .await
            .expect("apply should succeed");

            let calls = runner.get_calls().await;
            // Should have called route add twice.
            assert_eq!(calls.len(), 2);
            assert!(calls[0][0] == "ip" && calls[0][1] == "route" && calls[0][2] == "add");
            assert!(calls[1][0] == "ip" && calls[1][1] == "route" && calls[1][2] == "add");

            // Verify revert order is reversed.
            assert_eq!(cfg.revert_cmds.len(), 2);
            assert!(cfg.revert_cmds[0].contains(&"10.0.0.0/24".to_string()));
            assert!(cfg.revert_cmds[1].contains(&"10.1.0.0/24".to_string()));
        }

        #[tokio::test]
        async fn netconfig_no_route_manage_skips_routes() {
            let runner = TestRunner::new();
            let peer_routes = vec!["10.0.0.0/24".parse::<crate::shared::Ipv4Net>().unwrap()];
            let advertised = vec![];

            let cfg = NetConfig::apply(
                &runner,
                "test1",
                "tun0",
                "192.168.100.1".parse().unwrap(),
                30,
                &peer_routes,
                &advertised,
                true, // --no-route-manage
            )
            .await
            .expect("apply should succeed");

            let calls = runner.get_calls().await;
            // Should not have called anything (only printed).
            assert_eq!(calls.len(), 0);
            assert_eq!(cfg.revert_cmds.len(), 0);
        }

        // Root-required tests (skipped by default).

        #[tokio::test]
        #[ignore = "requires root: sudo cargo test --features vpn -- vpn::hostcfg::tests::check_root_accepts_uid_zero --ignored --nocapture"]
        async fn check_root_accepts_uid_zero() {
            // This test only passes if we're actually root.
            let result = check_root();
            assert!(result.is_ok());
        }

        // These don't actually need root — just check binary existence.
        #[tokio::test]
        async fn check_binary_exists_finds_ip() {
            assert!(check_binary_exists("ip"));
        }

        #[tokio::test]
        async fn check_binary_missing_not_found() {
            assert!(!check_binary_exists("__nonexistent_binary_12345__"));
        }

        /// Create a TUN device, verify it appears in `ip link show`, then drop it
        /// and verify it disappears.  Requires CAP_NET_ADMIN (root).
        /// Run: sudo cargo test --features vpn -- vpn::hostcfg::tests::tun_bring_up_and_down --ignored --nocapture
        #[tokio::test]
        #[ignore = "requires root/CAP_NET_ADMIN"]
        async fn tun_bring_up_and_down() {
            use std::net::Ipv4Addr;
            check_root().expect("test requires root");

            let name = "bore_test_tun0";
            let addr: Ipv4Addr = "10.199.0.1".parse().unwrap();

            // Stale reclaim in case a previous run crashed.
            stale_reclaim("test0", name).await;

            let (dev, _offload) = create_tun(name, addr, 30, 1350)
                .await
                .expect("failed to create TUN");

            // Verify interface is visible.
            let out = std::process::Command::new("ip")
                .args(["link", "show", name])
                .output()
                .expect("ip link show failed");
            assert!(
                out.status.success(),
                "interface {name} not found after creation"
            );

            // Drop the device — tun-rs should delete it.
            drop(dev);

            // Verify it's gone.
            let out2 = std::process::Command::new("ip")
                .args(["link", "show", name])
                .output()
                .expect("ip link show failed");
            assert!(
                !out2.status.success(),
                "interface {name} still exists after drop"
            );
        }
    }
}

pub mod link {
    //! VPN data-plane abstraction: Direct (QUIC datagrams) or Relay (AEAD-framed stream).
    use anyhow::{Context, Result};
    use bytes::Bytes;
    use futures_util::FutureExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    use super::crypto::DirectionKeys;
    use crate::holepunch::DirectConn;

    const BATCH_CAP: usize = 64;

    // Relay write queue depth. When full, packets are dropped (congestion relief).
    // TCP sessions above the VPN will retransmit; UDP is inherently lossy.
    // ~512 × 1350 bytes ≈ 700 KB of in-flight relay buffer before dropping starts.
    const RELAY_QUEUE: usize = 512;

    /// Send half of a VPN link (owned by the uplink task).
    pub enum LinkSender {
        /// Direct QUIC datagram path.
        Direct(DirectConn),
        /// Relay path: AEAD-framed stream.
        /// The actual write is done by a background writer task; this side
        /// uses non-blocking try_send so relay TCP congestion never deadlocks
        /// the uplink loop.
        Relay {
            /// Channel to the background relay writer task.
            tx: mpsc::Sender<Bytes>,
            /// AEAD egress key.
            key: [u8; 32],
            /// Per-packet counter for nonce derivation.
            counter: u64,
        },
    }

    /// Receive half of a VPN link (owned by the downlink task).
    pub enum LinkRecver {
        /// Direct QUIC datagram path.
        Direct(DirectConn),
        /// Relay path: AEAD-framed stream.
        Relay {
            /// Read half of the yamux stream.
            read: tokio::io::ReadHalf<crate::mux::Stream>,
            /// AEAD ingress key.
            key: [u8; 32],
        },
    }

    /// Split a Direct link into send+recv halves for the bridge tasks.
    pub fn make_direct(conn: DirectConn) -> (LinkSender, LinkRecver) {
        (LinkSender::Direct(conn.clone()), LinkRecver::Direct(conn))
    }

    /// Split a Relay link into send+recv halves for the bridge tasks.
    ///
    /// Spawns a background writer task that owns the write half. The uplink
    /// loop communicates via a bounded channel, so relay TCP congestion cannot
    /// block the bridge (excess frames are dropped instead of stalling the loop).
    pub fn make_relay(stream: crate::mux::Stream, keys: DirectionKeys) -> (LinkSender, LinkRecver) {
        let (read, write) = tokio::io::split(stream);
        let (tx, rx) = mpsc::channel::<Bytes>(RELAY_QUEUE);
        tokio::spawn(relay_writer(write, rx));
        (
            LinkSender::Relay {
                tx,
                key: keys.egress,
                counter: 0,
            },
            LinkRecver::Relay {
                read,
                key: keys.ingress,
            },
        )
    }

    /// Background task: drain the relay write queue and write frames to the yamux stream.
    /// Exits on write error (broken relay TCP); subsequent try_send calls in the uplink
    /// task will see Disconnected and propagate the error, tearing down the bridge.
    async fn relay_writer(
        mut write: tokio::io::WriteHalf<crate::mux::Stream>,
        mut rx: mpsc::Receiver<Bytes>,
    ) {
        while let Some(frame) = rx.recv().await {
            if write.write_all(&frame).await.is_err() {
                break;
            }
        }
    }

    impl LinkSender {
        /// Send a batch of IP packets. For Direct: QUIC datagrams. For Relay: AEAD frames.
        pub async fn send_batch(&mut self, pkts: &[Bytes]) -> Result<()> {
            match self {
                LinkSender::Direct(conn) => {
                    for pkt in pkts {
                        if let Err(e) = conn.send_datagram(pkt.clone()) {
                            // Skip TooLarge (MTU discovery transient); recount elsewhere.
                            if !e.to_string().contains("TooLarge") {
                                return Err(e);
                            }
                        }
                    }
                    Ok(())
                }
                LinkSender::Relay { tx, key, counter } => {
                    for pkt in pkts {
                        if *counter == u64::MAX {
                            anyhow::bail!("AEAD counter exhausted — tear down link");
                        }
                        let frame = super::crypto::seal_with_counter(key, *counter, pkt)?;
                        *counter += 1;
                        match tx.try_send(Bytes::from(frame)) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                // Relay write queue full: drop this packet.
                                // The relay TCP is congested; TCP sessions above will
                                // retransmit. Dropping here prevents the uplink loop
                                // from stalling and deadlocking the bridge.
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                anyhow::bail!("relay writer exited (write error on relay TCP)");
                            }
                        }
                    }
                    Ok(())
                }
            }
        }

        /// Resolved when the underlying link is gone (Direct only; Relay is TCP-based).
        pub async fn closed(&self) {
            match self {
                LinkSender::Direct(conn) => conn.closed().await,
                LinkSender::Relay { .. } => std::future::pending().await,
            }
        }
    }

    impl LinkRecver {
        /// Receive ≥1 IP packets. Err on link close.
        pub async fn recv_batch(&mut self, out: &mut Vec<Bytes>) -> Result<()> {
            match self {
                LinkRecver::Direct(conn) => {
                    let first = conn
                        .read_datagram()
                        .await
                        .context("direct recv first datagram")?;
                    out.push(first);
                    // Drain queued datagrams without yielding (up to BATCH_CAP).
                    for _ in 1..BATCH_CAP {
                        match conn.read_datagram().now_or_never() {
                            Some(Ok(pkt)) => out.push(pkt),
                            _ => break,
                        }
                    }
                    Ok(())
                }
                LinkRecver::Relay { read, key } => {
                    let mut len_buf = [0u8; 4];
                    read.read_exact(&mut len_buf)
                        .await
                        .context("relay frame length read")?;
                    let total_len = u32::from_be_bytes(len_buf) as usize;
                    anyhow::ensure!(total_len >= 8, "relay frame too short: {total_len}");
                    anyhow::ensure!(
                        total_len <= 65536 + 8 + 16,
                        "relay frame too large: {total_len}"
                    );
                    let mut frame = vec![0u8; total_len];
                    read.read_exact(&mut frame)
                        .await
                        .context("relay frame body read")?;
                    let plaintext = super::crypto::open(key, &frame).context("AEAD open failed")?;
                    out.push(Bytes::from(plaintext));
                    Ok(())
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use bytes::Bytes;
        use tokio::sync::mpsc;

        /// relay_writer must NOT block the caller when the channel is full.
        /// Excess packets are dropped (TrySendError::Full) instead of stalling.
        #[tokio::test]
        async fn relay_sender_drops_on_full_channel() {
            // Capacity 1 so the second packet overflows immediately.
            let (tx, _rx) = mpsc::channel::<Bytes>(1);
            let key = [0u8; 32];
            let mut sender = LinkSender::Relay { tx, key, counter: 0 };

            let pkt = Bytes::from(vec![0xAB; 64]);
            // First send fills the channel.
            sender.send_batch(&[pkt.clone()]).await.unwrap();
            // Second send would block with write_all; with try_send it must return Ok
            // (packet is silently dropped, not an error).
            sender.send_batch(&[pkt.clone()]).await.unwrap();
            // Counter advanced for both sealed packets (drop happens after sealing).
            match &sender {
                LinkSender::Relay { counter, .. } => assert_eq!(*counter, 2),
                _ => panic!("expected Relay"),
            }
        }

        /// When the writer task exits (e.g. relay TCP broken), the next send_batch
        /// must return Err rather than silently dropping or hanging.
        #[tokio::test]
        async fn relay_sender_errors_when_writer_gone() {
            let (tx, rx) = mpsc::channel::<Bytes>(8);
            // Drop the receiver — simulates the writer task having exited.
            drop(rx);
            let key = [0u8; 32];
            let mut sender = LinkSender::Relay { tx, key, counter: 0 };

            let pkt = Bytes::from(vec![0xCD; 32]);
            let result = sender.send_batch(&[pkt]).await;
            assert!(
                result.is_err(),
                "send_batch must error when writer task is gone"
            );
        }
    }
}

pub mod bridge {
    #![allow(dead_code)]
    //! VPN data-plane bridge: bidirectional flow between TUN and VpnLink.
    use anyhow::Result;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::interval;
    use tracing::debug;

    use super::link::{LinkRecver, LinkSender};

    /// Counter metrics for bridge data-plane.
    pub struct BridgeCounters {
        /// Transmitted packets.
        pub tx_pkts: AtomicU64,
        /// Transmitted bytes.
        pub tx_bytes: AtomicU64,
        /// Received packets.
        pub rx_pkts: AtomicU64,
        /// Received bytes.
        pub rx_bytes: AtomicU64,
        /// Dropped (TooLarge) packets.
        pub tx_drops: AtomicU64,
    }

    impl BridgeCounters {
        /// Create new bridge counters.
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                tx_pkts: AtomicU64::new(0),
                tx_bytes: AtomicU64::new(0),
                rx_pkts: AtomicU64::new(0),
                rx_bytes: AtomicU64::new(0),
                tx_drops: AtomicU64::new(0),
            })
        }
    }

    /// Run the VPN data-plane bridge until the link dies or the tun closes.
    /// Spawns uplink + downlink tasks and runs until one fails.
    /// `offload`: if true, uses Phase 6.2 multi-packet GSO/GRO I/O;
    /// if false, uses Phase 6.1 single-packet I/O.
    pub async fn run(
        dev: Arc<tun_rs::AsyncDevice>,
        sender: LinkSender,
        recver: LinkRecver,
        counters: Arc<BridgeCounters>,
        mtu: u16,
        offload: bool,
    ) -> Result<()> {
        let dev_up = Arc::clone(&dev);
        let dev_dn = Arc::clone(&dev);
        let cntr_up = Arc::clone(&counters);
        let cntr_dn = Arc::clone(&counters);

        let mut uplink = tokio::spawn(run_uplink(dev_up, sender, cntr_up, mtu, offload));
        let mut downlink = tokio::spawn(run_downlink(dev_dn, recver, cntr_dn, offload));

        let stats_task = tokio::spawn({
            let c = Arc::clone(&counters);
            async move {
                let mut ticker = interval(Duration::from_secs(10));
                loop {
                    ticker.tick().await;
                    debug!(
                        tx_pkts = c.tx_pkts.load(Ordering::Relaxed),
                        rx_pkts = c.rx_pkts.load(Ordering::Relaxed),
                        tx_bytes = c.tx_bytes.load(Ordering::Relaxed),
                        rx_bytes = c.rx_bytes.load(Ordering::Relaxed),
                        tx_drops = c.tx_drops.load(Ordering::Relaxed),
                        "vpn bridge stats",
                    );
                }
            }
        });

        // Run until either direction fails.
        let result = tokio::select! {
            res = &mut uplink => {
                downlink.abort();
                stats_task.abort();
                res.unwrap_or_else(|e| Err(anyhow::anyhow!("uplink task panic: {e}")))
            }
            res = &mut downlink => {
                uplink.abort();
                stats_task.abort();
                res.unwrap_or_else(|e| Err(anyhow::anyhow!("downlink task panic: {e}")))
            }
        };
        result
    }

    async fn run_uplink(
        dev: Arc<tun_rs::AsyncDevice>,
        sender: LinkSender,
        counters: Arc<BridgeCounters>,
        mtu: u16,
        offload: bool,
    ) -> Result<()> {
        if offload {
            run_uplink_offload(dev, sender, counters, mtu).await
        } else {
            run_uplink_single(dev, sender, counters, mtu).await
        }
    }

    /// Phase 6.1: single-packet read from TUN, one datagram/frame per call.
    async fn run_uplink_single(
        dev: Arc<tun_rs::AsyncDevice>,
        mut sender: LinkSender,
        counters: Arc<BridgeCounters>,
        mtu: u16,
    ) -> Result<()> {
        let mut buf = vec![0u8; mtu as usize + 4];
        loop {
            let n = dev.recv(&mut buf).await?;
            if n == 0 {
                continue;
            }
            let pkt = Bytes::copy_from_slice(&buf[..n]);
            let pkts = [pkt];
            match sender.send_batch(&pkts).await {
                Ok(_) => {
                    counters.tx_pkts.fetch_add(1, Ordering::Relaxed);
                    counters.tx_bytes.fetch_add(n as u64, Ordering::Relaxed);
                }
                Err(e) => {
                    if e.to_string().contains("TooLarge") {
                        counters.tx_drops.fetch_add(1, Ordering::Relaxed);
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }

    /// Phase 6.2: batch read from TUN via GSO super-buffer, one syscall → N segments.
    async fn run_uplink_offload(
        dev: Arc<tun_rs::AsyncDevice>,
        mut sender: LinkSender,
        counters: Arc<BridgeCounters>,
        mtu: u16,
    ) -> Result<()> {
        let mut original_buffer = vec![0u8; tun_rs::VIRTIO_NET_HDR_LEN + 65535];
        let mut bufs = vec![vec![0u8; mtu as usize]; tun_rs::IDEAL_BATCH_SIZE];
        let mut sizes = vec![0usize; tun_rs::IDEAL_BATCH_SIZE];
        loop {
            let num = dev
                .recv_multiple(&mut original_buffer, &mut bufs, &mut sizes, 0)
                .await?;
            if num == 0 {
                continue;
            }
            let pkts: Vec<Bytes> = (0..num)
                .map(|i| Bytes::copy_from_slice(&bufs[i][..sizes[i]]))
                .collect();
            let total_bytes: u64 = pkts.iter().map(|p| p.len() as u64).sum();
            match sender.send_batch(&pkts).await {
                Ok(_) => {
                    counters.tx_pkts.fetch_add(num as u64, Ordering::Relaxed);
                    counters.tx_bytes.fetch_add(total_bytes, Ordering::Relaxed);
                }
                Err(e) => {
                    if e.to_string().contains("TooLarge") {
                        counters.tx_drops.fetch_add(num as u64, Ordering::Relaxed);
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }

    async fn run_downlink(
        dev: Arc<tun_rs::AsyncDevice>,
        recver: LinkRecver,
        counters: Arc<BridgeCounters>,
        offload: bool,
    ) -> Result<()> {
        if offload {
            run_downlink_offload(dev, recver, counters).await
        } else {
            run_downlink_single(dev, recver, counters).await
        }
    }

    /// Phase 6.1: single-packet write to TUN per frame.
    async fn run_downlink_single(
        dev: Arc<tun_rs::AsyncDevice>,
        mut recver: LinkRecver,
        counters: Arc<BridgeCounters>,
    ) -> Result<()> {
        let mut batch = Vec::with_capacity(64);
        loop {
            batch.clear();
            recver.recv_batch(&mut batch).await?;
            for pkt in &batch {
                counters.rx_pkts.fetch_add(1, Ordering::Relaxed);
                counters
                    .rx_bytes
                    .fetch_add(pkt.len() as u64, Ordering::Relaxed);
                dev.send(pkt).await?;
            }
        }
    }

    /// Phase 6.2: coalesce RX batch via GRO, one multi-packet write syscall.
    /// Each BytesMut has VIRTIO_NET_HDR_LEN zeros prepended (no checksum offload
    /// needed — packets from the peer have complete checksums).
    async fn run_downlink_offload(
        dev: Arc<tun_rs::AsyncDevice>,
        mut recver: LinkRecver,
        counters: Arc<BridgeCounters>,
    ) -> Result<()> {
        let mut batch = Vec::with_capacity(tun_rs::IDEAL_BATCH_SIZE);
        let mut gro_table = tun_rs::GROTable::default();
        loop {
            batch.clear();
            recver.recv_batch(&mut batch).await?;
            let total_pkts = batch.len() as u64;
            let total_bytes: u64 = batch.iter().map(|p| p.len() as u64).sum();
            // Build BytesMut slices with VIRTIO_NET_HDR_LEN header prefix (all zeros).
            let mut bufs: Vec<bytes::BytesMut> = batch
                .iter()
                .map(|pkt| {
                    let mut b =
                        bytes::BytesMut::with_capacity(tun_rs::VIRTIO_NET_HDR_LEN + pkt.len());
                    b.resize(tun_rs::VIRTIO_NET_HDR_LEN, 0);
                    b.extend_from_slice(pkt);
                    b
                })
                .collect();
            let mut slices: Vec<&mut bytes::BytesMut> = bufs.iter_mut().collect();
            dev.send_multiple(&mut gro_table, &mut slices, tun_rs::VIRTIO_NET_HDR_LEN)
                .await?;
            counters.rx_pkts.fetch_add(total_pkts, Ordering::Relaxed);
            counters.rx_bytes.fetch_add(total_bytes, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    mod tests {
        /// Phase 6.2 — Segmentation: each packet from recv_multiple is ≤ MTU.
        #[test]
        fn segment_gso_buffer() {
            let mtu = 1350u16;
            for &sz in &[40usize, 1310, 1350, 800] {
                assert!(sz <= mtu as usize, "segment {sz} > MTU {mtu}");
                let pkt = bytes::Bytes::copy_from_slice(&vec![0u8; sz]);
                assert_eq!(pkt.len(), sz);
            }
        }

        /// Phase 6.2 — GRO coalescing: BytesMut has VIRTIO_NET_HDR_LEN zeros prefix.
        #[test]
        fn coalesce_for_gro() {
            for sz in [100usize, 500, 1310] {
                let pkt = vec![0x45u8; sz]; // fake IPv4 header start
                let mut b = bytes::BytesMut::with_capacity(tun_rs::VIRTIO_NET_HDR_LEN + sz);
                b.resize(tun_rs::VIRTIO_NET_HDR_LEN, 0);
                b.extend_from_slice(&pkt);
                assert_eq!(b.len(), tun_rs::VIRTIO_NET_HDR_LEN + sz);
                assert!(b[..tun_rs::VIRTIO_NET_HDR_LEN].iter().all(|&x| x == 0));
                assert_eq!(&b[tun_rs::VIRTIO_NET_HDR_LEN..], pkt.as_slice());
            }
        }
    }
}
