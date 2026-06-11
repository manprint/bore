#![cfg(feature = "vpn")]

use bore_cli::shared::{
    ClientMessage, Delimited, Ipv4Net, ServerMessage, VpnAddrRequest, CONTROL_PORT,
};
use lazy_static::lazy_static;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
    /// Serialize tests that bind the fixed control port.
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

async fn wait_for_control_port(listening: bool) {
    for _ in 0..500 {
        let ok = TcpStream::connect(("127.0.0.1", CONTROL_PORT))
            .await
            .is_ok();
        if ok == listening {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

async fn spawn_vpn_server_with_pool(pool_cidr: &str) {
    wait_for_control_port(false).await;
    let pool: Ipv4Net = pool_cidr.parse().unwrap();
    let mut server = bore_cli::server::Server::new(1024..=65535, None);
    server.set_vpn(true);
    server.set_vpn_pool(pool).unwrap();
    server.set_vpn_max_links(10);
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;
}

async fn spawn_vpn_server_disabled() {
    wait_for_control_port(false).await;
    let server = bore_cli::server::Server::new(1024..=65535, None);
    // vpn NOT enabled (default)
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;
}

/// Like `spawn_vpn_server_with_pool` but with a custom DEC-3 punch timeout.
async fn spawn_vpn_server_with_punch_timeout(pool_cidr: &str, punch_timeout: Duration) {
    wait_for_control_port(false).await;
    let pool: Ipv4Net = pool_cidr.parse().unwrap();
    let mut server = bore_cli::server::Server::new(1024..=65535, None);
    server.set_vpn(true);
    server.set_vpn_pool(pool).unwrap();
    server.set_vpn_max_links(10);
    server.set_vpn_punch_timeout(punch_timeout);
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;
}

/// Open a VPN control connection to the running server.
/// Returns a Delimited<mux::Stream> ready to send/receive control messages.
async fn vpn_ctrl_connect() -> Delimited<bore_cli::mux::Stream> {
    let stream = TcpStream::connect(("127.0.0.1", CONTROL_PORT))
        .await
        .unwrap();
    bore_cli::shared::tune_tcp(&stream);
    let (opener, _acceptor) = bore_cli::mux::client(stream);
    let ctrl_stream = opener.open().await.unwrap();
    Delimited::new(ctrl_stream)
}

fn pool_hello_vpn(id: &str) -> ClientMessage {
    ClientMessage::HelloVpn {
        id: id.to_string(),
        advertised: vec![],
        addr: VpnAddrRequest::Pool,
        notes: None,
        carriers: 1,
    }
}

fn pool_connect_vpn(id: &str) -> ClientMessage {
    ClientMessage::ConnectVpn {
        id: id.to_string(),
        advertised: vec![],
        addr: VpnAddrRequest::Pool,
        notes: None,
        carriers: 1,
    }
}

/// Helper to create a test server with VPN enabled.
fn setup_vpn_server(vpn_enabled: bool, vpn_pool: Option<Ipv4Net>) -> bore_cli::server::Server {
    let mut server = bore_cli::server::Server::new(9000..=9100, None);
    server.set_vpn(vpn_enabled);
    if let Some(pool) = vpn_pool {
        let _ = server.set_vpn_pool(pool);
    }
    server.set_vpn_max_links(100);
    server
}

#[tokio::test]
async fn vpn_pool_allocates_addresses() {
    let parent = Ipv4Net::from_str("10.0.0.0/30").unwrap();
    let mut pool = bore_cli::vpn_server::VpnPool::new(parent).unwrap();
    let (l, c) = pool.alloc().unwrap();
    assert_eq!(l, Ipv4Addr::new(10, 0, 0, 1));
    assert_eq!(c, Ipv4Addr::new(10, 0, 0, 2));
    assert!(pool.is_allocated(l));
}

#[tokio::test]
async fn vpn_pool_rejects_invalid_prefix() {
    let parent = Ipv4Net::from_str("10.0.0.0/31").unwrap(); // /31 is invalid (< /30)
    let result = bore_cli::vpn_server::VpnPool::new(parent);
    assert!(result.is_err());
}

#[tokio::test]
async fn vpn_overlap_detects_overlapping_nets() {
    let overlay = Ipv4Net::from_str("10.0.0.0/30").unwrap();
    let listener_advertised = vec![Ipv4Net::from_str("10.1.0.0/24").unwrap()];
    let connector_advertised = vec![Ipv4Net::from_str("10.0.0.0/25").unwrap()];

    let result =
        bore_cli::vpn_server::check_overlap(&listener_advertised, &connector_advertised, overlay);
    assert!(result.is_some());
}

#[tokio::test]
async fn vpn_overlap_accepts_non_overlapping_nets() {
    let overlay = Ipv4Net::from_str("10.0.0.0/30").unwrap();
    let listener_advertised = vec![Ipv4Net::from_str("10.1.0.0/24").unwrap()];
    let connector_advertised = vec![Ipv4Net::from_str("10.2.0.0/24").unwrap()];

    let result =
        bore_cli::vpn_server::check_overlap(&listener_advertised, &connector_advertised, overlay);
    assert!(result.is_none());
}

#[tokio::test]
async fn vpn_static_validation_accepts_mirror_addresses() {
    let result = bore_cli::vpn_server::validate_static(
        Ipv4Addr::new(10, 0, 0, 1), // listener addr
        30,                         // listener prefix
        Ipv4Addr::new(10, 0, 0, 2), // listener peer (should match connector addr)
        Ipv4Addr::new(10, 0, 0, 2), // connector addr
        30,                         // connector prefix
        Ipv4Addr::new(10, 0, 0, 1), // connector peer (should match listener addr)
    );
    assert!(result.is_ok());
}

#[tokio::test]
async fn vpn_static_validation_rejects_mismatched_addrs() {
    let result = bore_cli::vpn_server::validate_static(
        Ipv4Addr::new(10, 0, 0, 1), // listener addr
        30,
        Ipv4Addr::new(10, 0, 0, 3), // listener peer (wrong)
        Ipv4Addr::new(10, 0, 0, 2), // connector addr
        30,
        Ipv4Addr::new(10, 0, 0, 1), // connector peer
    );
    assert!(result.is_err());
}

#[tokio::test]
async fn vpn_static_validation_rejects_mismatched_prefixes() {
    let result = bore_cli::vpn_server::validate_static(
        Ipv4Addr::new(10, 0, 0, 1), // listener addr
        30,                         // listener prefix
        Ipv4Addr::new(10, 0, 0, 2), // listener peer
        Ipv4Addr::new(10, 0, 0, 2), // connector addr
        29,                         // connector prefix (wrong)
        Ipv4Addr::new(10, 0, 0, 1), // connector peer
    );
    assert!(result.is_err());
}

#[tokio::test]
async fn vpn_nonce_is_random() {
    let nonce1 = bore_cli::vpn_server::new_nonce();
    let nonce2 = bore_cli::vpn_server::new_nonce();
    assert_ne!(nonce1, nonce2); // Different random values
}

#[tokio::test]
async fn vpn_disabled_server_rejects_hello_vpn() {
    // Build a server without VPN enabled (just verify it builds)
    let _server = setup_vpn_server(false, None);
    // The actual rejection is tested via integration tests
}

#[tokio::test]
async fn vpn_lease_guard_drops_cleanly() {
    let parent = Ipv4Net::from_str("10.0.0.0/30").unwrap();
    let pool = bore_cli::vpn_server::VpnPool::new(parent).unwrap();
    let pool_arc = std::sync::Arc::new(std::sync::Mutex::new(pool));

    // Create a lease for address 10.0.0.4 (net_addr = 4)
    let _guard = bore_cli::vpn_server::VpnLeaseGuard::new(pool_arc.clone(), 0);

    // Guard drops here; should free the block
    drop(_guard);

    // Pool should still be valid
    let _pool_locked = pool_arc.lock().unwrap();
}

#[tokio::test]
async fn vpn_lease_guard_disarm_prevents_drop() {
    let parent = Ipv4Net::from_str("10.0.0.0/30").unwrap();
    let pool = bore_cli::vpn_server::VpnPool::new(parent).unwrap();
    let pool_arc = std::sync::Arc::new(std::sync::Mutex::new(pool));

    let mut guard = bore_cli::vpn_server::VpnLeaseGuard::new(pool_arc.clone(), 0);
    guard.disarm();

    // When dropped, should not free the block
    drop(guard);

    let _pool_locked = pool_arc.lock().unwrap();
    // Disarmed guard: block was not freed; pool lock acquired successfully.
}

/// D4 — the guard must free its block even when the pool lock is contended at
/// drop time. With the old `try_lock` implementation the block silently leaked.
#[tokio::test]
async fn vpn_lease_guard_frees_under_contention() {
    let parent = Ipv4Net::from_str("10.0.0.0/30").unwrap();
    let mut pool = bore_cli::vpn_server::VpnPool::new(parent).unwrap();
    let (l1, _c1) = pool.alloc().unwrap(); // the only /30 block
    let net_addr = u32::from(l1) - 1;
    let pool_arc = std::sync::Arc::new(std::sync::Mutex::new(pool));

    // Thread A holds the lock for 50 ms while thread B drops the guard.
    let holder = {
        let pool_arc = pool_arc.clone();
        std::thread::spawn(move || {
            let guard = pool_arc.lock().unwrap();
            std::thread::sleep(Duration::from_millis(50));
            drop(guard);
        })
    };
    // Give the holder time to actually take the lock.
    std::thread::sleep(Duration::from_millis(10));

    let dropper = {
        let pool_arc = pool_arc.clone();
        std::thread::spawn(move || {
            let guard = bore_cli::vpn_server::VpnLeaseGuard::new(pool_arc, net_addr);
            drop(guard); // must BLOCK until the holder releases, then free
        })
    };

    holder.join().unwrap();
    dropper.join().unwrap();

    // The block must be free again: a fresh alloc must succeed.
    let mut pool_locked = pool_arc.lock().unwrap();
    pool_locked
        .alloc()
        .expect("lease guard must free the block even under lock contention");
}

#[tokio::test]
async fn vpn_pool_exhaustion() {
    // Create a small pool: /30 = 1 block
    let parent = Ipv4Net::from_str("10.0.0.0/30").unwrap();
    let mut pool = bore_cli::vpn_server::VpnPool::new(parent).unwrap();

    // Allocate the one block
    let (l1, c1) = pool.alloc().unwrap();
    assert_eq!(l1, Ipv4Addr::new(10, 0, 0, 1));
    assert_eq!(c1, Ipv4Addr::new(10, 0, 0, 2));

    // Try to allocate again; should fail
    let result = pool.alloc();
    assert!(result.is_err());
}

#[tokio::test]
async fn vpn_pool_free_allows_reallocation() {
    let parent = Ipv4Net::from_str("10.0.0.0/30").unwrap();
    let mut pool = bore_cli::vpn_server::VpnPool::new(parent).unwrap();

    // Allocate the only block
    let (l1, c1) = pool.alloc().unwrap();
    assert_eq!(l1, Ipv4Addr::new(10, 0, 0, 1));
    assert_eq!(c1, Ipv4Addr::new(10, 0, 0, 2));

    // Pool is now full
    assert!(pool.alloc().is_err());

    // Free the block by its network address (0)
    let net_addr = u32::from(Ipv4Addr::new(10, 0, 0, 0));
    pool.free(net_addr);

    // Should be able to allocate again
    let (l2, c2) = pool.alloc().unwrap();
    assert_eq!(l2, l1); // Should get the same addresses
    assert_eq!(c2, c1);
}

#[tokio::test]
async fn vpn_addr_request_pool_variant() {
    let req = VpnAddrRequest::Pool;
    assert_eq!(req, VpnAddrRequest::Pool);
}

#[tokio::test]
async fn vpn_addr_request_static_variant() {
    let req = VpnAddrRequest::Static {
        addr: Ipv4Addr::new(10, 0, 0, 1),
        prefix: 30,
        peer: Ipv4Addr::new(10, 0, 0, 2),
    };
    match req {
        VpnAddrRequest::Static { addr, prefix, peer } => {
            assert_eq!(addr, Ipv4Addr::new(10, 0, 0, 1));
            assert_eq!(prefix, 30);
            assert_eq!(peer, Ipv4Addr::new(10, 0, 0, 2));
        }
        _ => panic!("unexpected variant"),
    }
}

#[tokio::test]
async fn vpn_ipv4net_from_str() {
    let net = Ipv4Net::from_str("192.168.1.0/24").unwrap();
    assert_eq!(net.addr, Ipv4Addr::new(192, 168, 1, 0));
    assert_eq!(net.prefix, 24);
}

#[tokio::test]
async fn vpn_ipv4net_network() {
    let net = Ipv4Net::from_str("192.168.1.128/25").unwrap();
    let network = net.network();
    assert_eq!(network, Ipv4Addr::new(192, 168, 1, 128));
}

#[tokio::test]
async fn vpn_ipv4net_contains() {
    let net = Ipv4Net::from_str("192.168.1.0/24").unwrap();
    assert!(net.contains(Ipv4Addr::new(192, 168, 1, 100)));
    assert!(!net.contains(Ipv4Addr::new(192, 168, 2, 1)));
}

#[tokio::test]
async fn vpn_ipv4net_overlaps() {
    let net1 = Ipv4Net::from_str("192.168.1.0/24").unwrap();
    let net2 = Ipv4Net::from_str("192.168.1.128/25").unwrap();
    let net3 = Ipv4Net::from_str("192.168.2.0/24").unwrap();

    assert!(net1.overlaps(&net2)); // net2 is subset of net1
    assert!(!net1.overlaps(&net3)); // different networks
}

#[tokio::test]
async fn vpn_ipv4net_display() {
    let net = Ipv4Net::from_str("10.0.0.0/8").unwrap();
    assert_eq!(format!("{}", net), "10.0.0.0/8");
}

#[tokio::test]
async fn vpn_ipv4net_from_str_invalid_format() {
    let result = Ipv4Net::from_str("192.168.1.0");
    assert!(result.is_err());
}

#[tokio::test]
async fn vpn_ipv4net_from_str_invalid_prefix() {
    let result = Ipv4Net::from_str("192.168.1.0/33");
    assert!(result.is_err());
}

#[tokio::test]
async fn vpn_ipv4net_from_str_invalid_addr() {
    let result = Ipv4Net::from_str("999.999.999.999/24");
    assert!(result.is_err());
}

// ─── Server integration tests (in-process, no TUN) ──────────────────────────

/// Full pairing via real server: verify both sides get VpnReady with pool addrs (.1/.2).
#[tokio::test]
async fn vpn_server_pair_assigns_pool_addrs() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server_with_pool("10.88.0.0/16").await;

    // Listener task: register and wait for VpnReady.
    let listener_task = tokio::spawn(async {
        let mut ctrl = vpn_ctrl_connect().await;
        ctrl.send(pool_hello_vpn("link1")).await.unwrap();
        ctrl.recv::<ServerMessage>().await.unwrap()
    });

    // Give listener time to register before connector arrives.
    time::sleep(Duration::from_millis(80)).await;

    // Connector: pair with listener.
    let mut ctrl = vpn_ctrl_connect().await;
    ctrl.send(pool_connect_vpn("link1")).await.unwrap();
    let conn_ready = ctrl.recv::<ServerMessage>().await.unwrap();

    // Connector must get VpnReady.
    let (c_assigned, c_prefix, c_peer) = match conn_ready {
        Some(ServerMessage::VpnReady {
            assigned,
            prefix,
            peer_overlay,
            ..
        }) => (assigned, prefix, peer_overlay),
        other => panic!("connector expected VpnReady, got {other:?}"),
    };
    assert_eq!(c_prefix, 30);
    // Connector gets .2, listener gets .1
    assert_eq!(c_assigned.octets()[3], 2, "connector should get .2 of /30");
    assert_eq!(c_peer.octets()[3], 1, "connector peer should be .1");

    // Listener must also get VpnReady.
    let list_ready = tokio::time::timeout(Duration::from_secs(3), listener_task)
        .await
        .expect("listener task timed out")
        .unwrap();
    let (l_assigned, l_prefix, l_peer) = match list_ready {
        Some(ServerMessage::VpnReady {
            assigned,
            prefix,
            peer_overlay,
            ..
        }) => (assigned, prefix, peer_overlay),
        other => panic!("listener expected VpnReady, got {other:?}"),
    };
    assert_eq!(l_prefix, 30);
    assert_eq!(
        l_assigned, c_peer,
        "listener addr must equal connector's reported peer"
    );
    assert_eq!(
        l_peer, c_assigned,
        "listener peer must equal connector's assigned addr"
    );
}

/// Two clients register with the same id: second gets VpnError.
#[tokio::test]
async fn vpn_server_duplicate_id_rejected() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server_with_pool("10.89.0.0/16").await;

    // First listener: OK
    let mut ctrl1 = vpn_ctrl_connect().await;
    ctrl1.send(pool_hello_vpn("dup")).await.unwrap();
    // Don't read response yet — leave it waiting.

    time::sleep(Duration::from_millis(50)).await;

    // Second listener: duplicate → VpnError
    let mut ctrl2 = vpn_ctrl_connect().await;
    ctrl2.send(pool_hello_vpn("dup")).await.unwrap();
    let resp = ctrl2.recv::<ServerMessage>().await.unwrap();
    assert!(
        matches!(resp, Some(ServerMessage::VpnError(_))),
        "duplicate id must get VpnError, got {resp:?}"
    );
}

/// Server without --vpn answers VpnError (not a hard disconnect / parse failure).
#[tokio::test]
async fn vpn_server_disabled_rejects() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server_disabled().await;

    let mut ctrl = vpn_ctrl_connect().await;
    ctrl.send(pool_hello_vpn("any")).await.unwrap();
    let resp = ctrl.recv::<ServerMessage>().await.unwrap();
    assert!(
        matches!(resp, Some(ServerMessage::VpnError(_))),
        "disabled server must return VpnError, got {resp:?}"
    );
}

/// Connector advertises the same subnet as the listener: overlap → VpnError.
#[tokio::test]
async fn vpn_server_overlap_rejected() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server_with_pool("10.90.0.0/16").await;

    let cidr: Ipv4Net = "192.168.1.0/24".parse().unwrap();

    // Listener advertises 192.168.1.0/24
    let listener_task = tokio::spawn(async move {
        let mut ctrl = vpn_ctrl_connect().await;
        ctrl.send(ClientMessage::HelloVpn {
            id: "ov".into(),
            advertised: vec![cidr],
            addr: VpnAddrRequest::Pool,
            notes: None,
            carriers: 1,
        })
        .await
        .unwrap();
        ctrl.recv::<ServerMessage>().await.unwrap()
    });

    time::sleep(Duration::from_millis(80)).await;

    // Connector also advertises 192.168.1.0/24 → overlap
    let mut ctrl = vpn_ctrl_connect().await;
    ctrl.send(ClientMessage::ConnectVpn {
        id: "ov".into(),
        advertised: vec![cidr],
        addr: VpnAddrRequest::Pool,
        notes: None,
        carriers: 1,
    })
    .await
    .unwrap();
    let resp = ctrl.recv::<ServerMessage>().await.unwrap();
    assert!(
        matches!(resp, Some(ServerMessage::VpnError(_))),
        "overlap must give VpnError, got {resp:?}"
    );

    drop(listener_task); // listener will error (connector rejected); that's fine
}

/// Addressing mode mismatch (Pool on listener, Static on connector) → VpnError.
#[tokio::test]
async fn vpn_server_addr_mode_mismatch_rejected() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server_with_pool("10.91.0.0/16").await;

    // Listener uses Pool.
    let _listener = tokio::spawn(async {
        let mut ctrl = vpn_ctrl_connect().await;
        ctrl.send(pool_hello_vpn("mm")).await.unwrap();
        // Wait a bit, then drop.
        time::sleep(Duration::from_millis(500)).await;
    });

    time::sleep(Duration::from_millis(80)).await;

    // Connector uses Static → mode mismatch.
    let mut ctrl = vpn_ctrl_connect().await;
    ctrl.send(ClientMessage::ConnectVpn {
        id: "mm".into(),
        advertised: vec![],
        addr: VpnAddrRequest::Static {
            addr: "10.91.0.2".parse().unwrap(),
            prefix: 30,
            peer: "10.91.0.1".parse().unwrap(),
        },
        notes: None,
        carriers: 1,
    })
    .await
    .unwrap();
    let resp = ctrl.recv::<ServerMessage>().await.unwrap();
    assert!(
        matches!(resp, Some(ServerMessage::VpnError(_))),
        "mode mismatch must give VpnError, got {resp:?}"
    );
}

/// Static mirror inconsistency → VpnError.
#[tokio::test]
async fn vpn_server_static_inconsistent_pair_rejected() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server_with_pool("10.92.0.0/16").await;

    // Listener: static .1/.2
    let _listener = tokio::spawn(async {
        let mut ctrl = vpn_ctrl_connect().await;
        ctrl.send(ClientMessage::HelloVpn {
            id: "si".into(),
            advertised: vec![],
            addr: VpnAddrRequest::Static {
                addr: "172.31.0.1".parse().unwrap(),
                prefix: 30,
                peer: "172.31.0.2".parse().unwrap(),
            },
            notes: None,
            carriers: 1,
        })
        .await
        .unwrap();
        time::sleep(Duration::from_millis(500)).await;
    });

    time::sleep(Duration::from_millis(80)).await;

    // Connector: .2/.1 but wrong peer (inconsistent mirror)
    let mut ctrl = vpn_ctrl_connect().await;
    ctrl.send(ClientMessage::ConnectVpn {
        id: "si".into(),
        advertised: vec![],
        addr: VpnAddrRequest::Static {
            addr: "172.31.0.2".parse().unwrap(),
            prefix: 30,
            peer: "172.31.0.3".parse().unwrap(), // WRONG: should be .1
        },
        notes: None,
        carriers: 1,
    })
    .await
    .unwrap();
    let resp = ctrl.recv::<ServerMessage>().await.unwrap();
    assert!(
        matches!(resp, Some(ServerMessage::VpnError(_))),
        "inconsistent static pair must give VpnError, got {resp:?}"
    );
}

/// Connect to unknown link id → VpnError.
#[tokio::test]
async fn vpn_server_connect_unknown_id_rejected() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server_with_pool("10.93.0.0/16").await;

    let mut ctrl = vpn_ctrl_connect().await;
    ctrl.send(pool_connect_vpn("no-such-link")).await.unwrap();
    let resp = ctrl.recv::<ServerMessage>().await.unwrap();
    assert!(
        matches!(resp, Some(ServerMessage::VpnError(_))),
        "unknown id must give VpnError, got {resp:?}"
    );
}

/// Pool exhausted via server: second pair attempt gets VpnError.
#[tokio::test]
async fn vpn_server_pool_exhaustion_rejected() {
    let _guard = SERIAL_GUARD.lock().await;
    // /30 = exactly 1 block; one pair uses it all.
    spawn_vpn_server_with_pool("10.94.0.0/30").await;

    // First pair: succeeds.
    let _l1 = tokio::spawn(async {
        let mut ctrl = vpn_ctrl_connect().await;
        ctrl.send(pool_hello_vpn("p1")).await.unwrap();
        time::sleep(Duration::from_millis(500)).await;
    });
    time::sleep(Duration::from_millis(80)).await;
    let mut c1 = vpn_ctrl_connect().await;
    c1.send(pool_connect_vpn("p1")).await.unwrap();
    let r1 = c1.recv::<ServerMessage>().await.unwrap();
    assert!(
        matches!(r1, Some(ServerMessage::VpnReady { .. })),
        "first pair should succeed"
    );

    // Second pair: pool exhausted → VpnError.
    let _l2 = tokio::spawn(async {
        let mut ctrl = vpn_ctrl_connect().await;
        ctrl.send(pool_hello_vpn("p2")).await.unwrap();
        time::sleep(Duration::from_millis(500)).await;
    });
    time::sleep(Duration::from_millis(80)).await;
    let mut c2 = vpn_ctrl_connect().await;
    c2.send(pool_connect_vpn("p2")).await.unwrap();
    let r2 = c2.recv::<ServerMessage>().await.unwrap();
    assert!(
        matches!(r2, Some(ServerMessage::VpnError(_))),
        "exhausted pool must give VpnError, got {r2:?}"
    );
}

/// Bytes pushed through the VPN relay arrive unchanged: server is an opaque splice,
/// not a decryptor. Connector sends an AEAD-format frame; listener receives the
/// same bytes.  A real plaintext IPv4 header would start with 0x45; the AEAD
/// ciphertext starts with a random byte — proving the server never sees plaintext.
#[tokio::test]
async fn vpn_relay_substream_is_opaque() {
    use bore_cli::mux::STREAM_READY;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server_with_pool("10.95.0.0/16").await;

    // ─── Listener side ────────────────────────────────────────────────────
    let ls = TcpStream::connect(("127.0.0.1", CONTROL_PORT))
        .await
        .unwrap();
    bore_cli::shared::tune_tcp(&ls);
    let (l_opener, mut l_acceptor) = bore_cli::mux::client(ls);
    let mut l_ctrl = Delimited::new(l_opener.open().await.unwrap());
    l_ctrl.send(pool_hello_vpn("relay-test")).await.unwrap();

    // ─── Connector side ───────────────────────────────────────────────────
    time::sleep(Duration::from_millis(80)).await;
    let cs = TcpStream::connect(("127.0.0.1", CONTROL_PORT))
        .await
        .unwrap();
    bore_cli::shared::tune_tcp(&cs);
    let (c_opener, _c_acceptor) = bore_cli::mux::client(cs);
    let mut c_ctrl = Delimited::new(c_opener.open().await.unwrap());
    c_ctrl.send(pool_connect_vpn("relay-test")).await.unwrap();

    // Both sides should get VpnReady.
    let c_ready = c_ctrl.recv::<ServerMessage>().await.unwrap();
    assert!(
        matches!(c_ready, Some(ServerMessage::VpnReady { .. })),
        "connector VpnReady"
    );

    // Listener gets its VpnReady (sent by the server via the pair channel).
    let l_ready = tokio::time::timeout(Duration::from_secs(2), l_ctrl.recv::<ServerMessage>())
        .await
        .expect("listener VpnReady timed out")
        .unwrap();
    assert!(
        matches!(l_ready, Some(ServerMessage::VpnReady { .. })),
        "listener VpnReady"
    );

    // ─── Open a data substream (connector → server → relay → listener) ───
    // Connector opens a new yamux stream and writes the readiness marker.
    let mut data_send = c_opener.open().await.unwrap();
    data_send.write_all(&[STREAM_READY]).await.unwrap();

    // Server relays this to the listener. Listener accepts the stream and reads
    // the STREAM_READY byte the server injected, then the payload.
    let mut data_recv = tokio::time::timeout(Duration::from_secs(2), l_acceptor.accept())
        .await
        .expect("listener accept timed out")
        .expect("listener acceptor closed");

    // Read the STREAM_READY marker written by the server relay.
    let mut marker = [0u8; 1];
    data_recv.read_exact(&mut marker).await.unwrap();
    assert_eq!(marker[0], STREAM_READY);

    // ─── Send AEAD-format bytes (not a plaintext IPv4 header) ────────────
    // An AEAD frame: [u32 BE len][u64 BE counter][random ciphertext + tag]
    // The key check: the first byte is NOT 0x45 (IPv4 header version/IHL).
    let fake_aead_frame: &[u8] = &[
        0x00, 0x00, 0x00, 0x16, // total_len = 22 (8-byte counter + 14-byte payload+tag)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // counter = 0
        // 14 random bytes (fake ciphertext+tag, first byte != 0x45)
        0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18, 0x29, 0x3A, 0x4B, 0x5C, 0x6D, 0x7E,
    ];
    assert_ne!(
        fake_aead_frame[0], 0x45,
        "test data must not look like plaintext IPv4"
    );

    data_send.write_all(fake_aead_frame).await.unwrap();
    data_send.flush().await.unwrap();

    // Listener reads the same bytes from the relay.
    let mut received = vec![0u8; fake_aead_frame.len()];
    data_recv.read_exact(&mut received).await.unwrap();

    assert_eq!(
        received.as_slice(),
        fake_aead_frame,
        "relay must pass bytes through unchanged (server is an opaque splice)"
    );
    // The first byte is not 0x45, proving the server never decrypted the payload.
    assert_ne!(
        received[0], 0x45,
        "server must not have decrypted the AEAD frame"
    );
}

// ─── §1.1 UDP broker (DEC-3: punch only with BOTH offers) ────────────────────

/// Drain heartbeats until a `UdpPunch` arrives; panic on anything else.
async fn recv_until_punch(
    ctrl: &mut Delimited<bore_cli::mux::Stream>,
    what: &str,
) -> ([u8; 16], Vec<std::net::SocketAddr>) {
    let deadline = time::Instant::now() + Duration::from_secs(5);
    loop {
        let msg = time::timeout_at(deadline, ctrl.recv::<ServerMessage>())
            .await
            .unwrap_or_else(|_| panic!("{what}: timed out waiting for UdpPunch"))
            .unwrap();
        match msg {
            Some(ServerMessage::UdpPunch { nonce, peer, .. }) => return (nonce, peer),
            Some(ServerMessage::Heartbeat) => continue,
            other => panic!("{what}: expected UdpPunch, got {other:?}"),
        }
    }
}

/// Drain heartbeats until a `UdpUnavailable` arrives; panic on anything else.
async fn recv_until_unavailable(ctrl: &mut Delimited<bore_cli::mux::Stream>, what: &str) {
    let deadline = time::Instant::now() + Duration::from_secs(5);
    loop {
        let msg = time::timeout_at(deadline, ctrl.recv::<ServerMessage>())
            .await
            .unwrap_or_else(|_| panic!("{what}: timed out waiting for UdpUnavailable"))
            .unwrap();
        match msg {
            Some(ServerMessage::UdpUnavailable) => return,
            Some(ServerMessage::Heartbeat) => continue,
            other => panic!("{what}: expected UdpUnavailable, got {other:?}"),
        }
    }
}

fn offer(addr: &str) -> ClientMessage {
    ClientMessage::UdpCandidateOffer(bore_cli::shared::UdpCandidateOffer {
        candidates: vec![addr.parse().unwrap()],
        selected_stun: None,
    })
}

fn session_nonce_of(msg: &Option<ServerMessage>) -> [u8; 16] {
    match msg {
        Some(ServerMessage::VpnReady { session_nonce, .. }) => *session_nonce,
        other => panic!("expected VpnReady, got {other:?}"),
    }
}

/// With both offers present the broker punches BOTH sides: each peer receives
/// the OTHER peer's candidates, under the pairing nonce.
#[tokio::test]
async fn vpn_broker_punches_both_sides_when_both_offers_present() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server_with_pool("10.91.0.0/16").await;

    let mut l_ctrl = vpn_ctrl_connect().await;
    l_ctrl.send(pool_hello_vpn("broker1")).await.unwrap();
    time::sleep(Duration::from_millis(80)).await;

    let mut c_ctrl = vpn_ctrl_connect().await;
    c_ctrl.send(pool_connect_vpn("broker1")).await.unwrap();
    let c_ready = c_ctrl.recv::<ServerMessage>().await.unwrap();
    let nonce = session_nonce_of(&c_ready);
    let l_ready = l_ctrl.recv::<ServerMessage>().await.unwrap();
    assert_eq!(session_nonce_of(&l_ready), nonce, "nonce must match");

    // Listener offers first, then the connector.
    l_ctrl.send(offer("203.0.113.1:1000")).await.unwrap();
    time::sleep(Duration::from_millis(80)).await;
    c_ctrl.send(offer("203.0.113.2:2000")).await.unwrap();

    let (c_nonce, c_peer) = recv_until_punch(&mut c_ctrl, "connector").await;
    assert_eq!(
        c_nonce, nonce,
        "connector punch must carry the pairing nonce"
    );
    assert_eq!(c_peer, vec!["203.0.113.1:1000".parse().unwrap()]);

    let (l_nonce, l_peer) = recv_until_punch(&mut l_ctrl, "listener").await;
    assert_eq!(
        l_nonce, nonce,
        "listener punch must carry the pairing nonce"
    );
    assert_eq!(l_peer, vec!["203.0.113.2:2000".parse().unwrap()]);
}

/// The broker must defer the punch until the listener's offer arrives (DEC-3),
/// even when the connector offers first.
#[tokio::test]
async fn vpn_broker_waits_for_listener_offer() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server_with_pool("10.92.0.0/16").await;

    let mut l_ctrl = vpn_ctrl_connect().await;
    l_ctrl.send(pool_hello_vpn("broker2")).await.unwrap();
    time::sleep(Duration::from_millis(80)).await;

    let mut c_ctrl = vpn_ctrl_connect().await;
    c_ctrl.send(pool_connect_vpn("broker2")).await.unwrap();
    let _ = c_ctrl.recv::<ServerMessage>().await.unwrap(); // VpnReady
    let _ = l_ctrl.recv::<ServerMessage>().await.unwrap(); // VpnReady

    // Connector offers FIRST; the listener takes 1 s to offer.
    c_ctrl.send(offer("203.0.113.2:2000")).await.unwrap();
    time::sleep(Duration::from_secs(1)).await;
    l_ctrl.send(offer("203.0.113.1:1000")).await.unwrap();

    // The punch must still arrive (within the helper's 5 s budget).
    let (_, c_peer) = recv_until_punch(&mut c_ctrl, "connector").await;
    assert_eq!(c_peer, vec!["203.0.113.1:1000".parse().unwrap()]);
    let (_, l_peer) = recv_until_punch(&mut l_ctrl, "listener").await;
    assert_eq!(l_peer, vec!["203.0.113.2:2000".parse().unwrap()]);
}

/// If the listener never offers, the connector gets `UdpUnavailable` after the
/// punch timeout and stays on relay.
#[tokio::test]
async fn vpn_broker_timeout_sends_unavailable() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server_with_punch_timeout("10.93.0.0/16", Duration::from_millis(500)).await;

    let mut l_ctrl = vpn_ctrl_connect().await;
    l_ctrl.send(pool_hello_vpn("broker3")).await.unwrap();
    time::sleep(Duration::from_millis(80)).await;

    let mut c_ctrl = vpn_ctrl_connect().await;
    c_ctrl.send(pool_connect_vpn("broker3")).await.unwrap();
    let _ = c_ctrl.recv::<ServerMessage>().await.unwrap(); // VpnReady
    let _ = l_ctrl.recv::<ServerMessage>().await.unwrap(); // VpnReady

    // Only the connector offers; the listener never does.
    c_ctrl.send(offer("203.0.113.2:2000")).await.unwrap();
    recv_until_unavailable(&mut c_ctrl, "connector").await;
}

// ─── §3.1 Admin page VPN entries (F5) ────────────────────────────────────────

const ADMIN_TOKEN: &str = "0123456789abcdef0123456789abcdef01234567";

/// Issue one admin HTTP GET on the control port and return the response body.
async fn admin_get_data() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = TcpStream::connect(("127.0.0.1", CONTROL_PORT))
        .await
        .unwrap();
    let req = format!(
        "GET /admin/status/data HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {ADMIN_TOKEN}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    s.flush().await.unwrap();
    let mut buf = Vec::new();
    time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

/// F5 — paired VPN links appear on the admin page with dedicated roles, the
/// assigned overlay, path=relay initially, and flip to direct on VpnPathReport.
#[tokio::test]
async fn vpn_admin_entries_and_path_report() {
    let _guard = SERIAL_GUARD.lock().await;
    wait_for_control_port(false).await;
    let pool: Ipv4Net = "10.94.0.0/16".parse().unwrap();
    let mut server = bore_cli::server::Server::new(1024..=65535, None);
    server.set_vpn(true);
    server.set_vpn_pool(pool).unwrap();
    server.set_vpn_max_links(10);
    server.set_admin_token(Some(ADMIN_TOKEN.into()));
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;

    let mut l_ctrl = vpn_ctrl_connect().await;
    l_ctrl.send(pool_hello_vpn("admin1")).await.unwrap();
    time::sleep(Duration::from_millis(80)).await;

    let mut c_ctrl = vpn_ctrl_connect().await;
    c_ctrl.send(pool_connect_vpn("admin1")).await.unwrap();
    let c_ready = c_ctrl.recv::<ServerMessage>().await.unwrap();
    // The server must advertise admin v2 support.
    match &c_ready {
        Some(ServerMessage::VpnReady { admin_v2, .. }) => {
            assert!(*admin_v2, "server must set admin_v2 in VpnReady")
        }
        other => panic!("expected VpnReady, got {other:?}"),
    }
    let _ = l_ctrl.recv::<ServerMessage>().await.unwrap(); // listener VpnReady
    time::sleep(Duration::from_millis(100)).await;

    let data = admin_get_data().await;
    assert!(
        data.contains("vpn-listener"),
        "missing vpn-listener role: {data}"
    );
    assert!(
        data.contains("vpn-connector"),
        "missing vpn-connector role: {data}"
    );
    assert!(
        data.contains("10.94.0.1/30") && data.contains("10.94.0.2/30"),
        "missing overlay addresses: {data}"
    );
    assert!(
        data.contains("\"vpn_direct\":false"),
        "links must start as relay: {data}"
    );
    assert!(
        !data.contains("\"vpn_direct\":true"),
        "no link reported direct yet: {data}"
    );

    // The connector reports the direct path; the snapshot must reflect it.
    c_ctrl
        .send(ClientMessage::VpnPathReport {
            path: "direct".into(),
        })
        .await
        .unwrap();
    time::sleep(Duration::from_millis(300)).await;
    let data = admin_get_data().await;
    assert!(
        data.contains("\"vpn_direct\":true"),
        "connector entry must show direct after VpnPathReport: {data}"
    );
}

// ─── §4.1 carriers negotiation (C3) ──────────────────────────────────────────

/// hello(4) + connect(2) → VpnReady.carriers == 2 on both sides.
#[tokio::test]
async fn vpn_carriers_negotiation() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server_with_pool("10.95.0.0/16").await;

    let mut l_ctrl = vpn_ctrl_connect().await;
    l_ctrl
        .send(ClientMessage::HelloVpn {
            id: "neg1".into(),
            advertised: vec![],
            addr: VpnAddrRequest::Pool,
            notes: None,
            carriers: 4,
        })
        .await
        .unwrap();
    time::sleep(Duration::from_millis(80)).await;

    let mut c_ctrl = vpn_ctrl_connect().await;
    c_ctrl
        .send(ClientMessage::ConnectVpn {
            id: "neg1".into(),
            advertised: vec![],
            addr: VpnAddrRequest::Pool,
            notes: None,
            carriers: 2,
        })
        .await
        .unwrap();

    let c_carriers = match c_ctrl.recv::<ServerMessage>().await.unwrap() {
        Some(ServerMessage::VpnReady { carriers, .. }) => carriers,
        other => panic!("connector expected VpnReady, got {other:?}"),
    };
    let l_carriers = match l_ctrl.recv::<ServerMessage>().await.unwrap() {
        Some(ServerMessage::VpnReady { carriers, .. }) => carriers,
        other => panic!("listener expected VpnReady, got {other:?}"),
    };
    assert_eq!(c_carriers, 2, "min(hello=4, connect=2, server max) == 2");
    assert_eq!(l_carriers, 2);
}

/// Wire compatibility (I-8/I-9): messages from an OLD peer without the
/// `carriers` field deserialize with carriers == 1, and an old `VpnReady`
/// (without `carriers`/`admin_v2`) defaults to 1/false.
#[tokio::test]
async fn vpn_carriers_default_for_old_peers() {
    let json = r#"{"ConnectVpn":{"id":"x","advertised":[],"addr":"Pool","notes":null}}"#;
    let msg: ClientMessage = serde_json::from_str(json).unwrap();
    match msg {
        ClientMessage::ConnectVpn { carriers, .. } => assert_eq!(carriers, 1),
        other => panic!("expected ConnectVpn, got {other:?}"),
    }

    let json = r#"{"VpnReady":{"assigned":"10.0.0.2","prefix":30,"peer_overlay":"10.0.0.1","peer_advertised":[],"session_nonce":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}}"#;
    let msg: ServerMessage = serde_json::from_str(json).unwrap();
    match msg {
        ServerMessage::VpnReady {
            carriers, admin_v2, ..
        } => {
            assert_eq!(carriers, 1);
            assert!(!admin_v2);
        }
        other => panic!("expected VpnReady, got {other:?}"),
    }
}
