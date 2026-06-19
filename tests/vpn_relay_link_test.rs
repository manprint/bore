//! Regression test for the VPN relay data plane (no TUN device required).
//!
//! Drives the real client-side relay link (`vpn::link`) through a real server
//! relay and pumps bulk traffic in BOTH directions simultaneously. This guards
//! against the silent-wedge class of bugs: the original single-substream relay
//! shared one `yamux::Stream` between a reader and a writer task via
//! `tokio::io::split`, and the two tasks overwrote each other's parked waker on
//! the stream's internal channel — the link froze permanently after ~256 KB
//! under load, with no error anywhere. With the old code this test deadlocks;
//! it must complete well within the timeout with the dual-substream link.

#![cfg(feature = "vpn")]

use bore_cli::shared::{ClientMessage, Delimited, Ipv4Net, ServerMessage, VpnAddrRequest};
use bore_cli::vpn::link;
use bytes::Bytes;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time;

static SERIAL_GUARD: Mutex<()> = Mutex::const_new(());

const SECRET: &str = "relay-link-test-secret";
const PKT_LEN: usize = 1350;
const PKT_COUNT: u64 = 5_000;

async fn wait_for_control_port(listening: bool) {
    for _ in 0..500 {
        let ok = TcpStream::connect(("127.0.0.1", bore_cli::shared::CONTROL_PORT))
            .await
            .is_ok();
        if ok == listening {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

async fn spawn_vpn_server(pool_cidr: &str) {
    wait_for_control_port(false).await;
    let pool: Ipv4Net = pool_cidr.parse().unwrap();
    let mut server = bore_cli::server::Server::new(1024..=65535, None);
    server.set_vpn(true);
    server.set_vpn_pool(pool).unwrap();
    server.set_vpn_max_links(10);
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;
}

async fn mux_connect() -> (bore_cli::mux::Opener, bore_cli::mux::Acceptor) {
    let stream = TcpStream::connect(("127.0.0.1", bore_cli::shared::CONTROL_PORT))
        .await
        .unwrap();
    bore_cli::shared::tune_tcp(&stream);
    bore_cli::mux::client(stream)
}

fn test_packet(seq: u64) -> Bytes {
    let mut pkt = vec![0u8; PKT_LEN];
    pkt[..8].copy_from_slice(&seq.to_be_bytes());
    // Mark the rest so corruption is detectable.
    for (i, b) in pkt[8..].iter_mut().enumerate() {
        *b = (seq as usize + i) as u8;
    }
    Bytes::from(pkt)
}

fn check_packet(pkt: &Bytes, expected_seq: u64) {
    assert_eq!(pkt.len(), PKT_LEN, "packet length mismatch");
    let seq = u64::from_be_bytes(pkt[..8].try_into().unwrap());
    assert_eq!(seq, expected_seq, "packet out of order or lost");
    for (i, b) in pkt[8..].iter().enumerate() {
        assert_eq!(*b, (seq as usize + i) as u8, "packet payload corrupted");
    }
}

async fn pump_out(mut sender: link::LinkSender) {
    for seq in 0..PKT_COUNT {
        let pkt = test_packet(seq);
        sender
            .send_batch(std::slice::from_ref(&pkt))
            .await
            .expect("send_batch failed");
    }
    // Keep the sender alive until the test ends so the writer task does not
    // shut the substream down before the peer drains it.
    std::future::pending::<()>().await;
}

async fn pump_in(mut recver: link::LinkRecver) {
    let mut next_seq = 0u64;
    let mut batch = Vec::with_capacity(64);
    while next_seq < PKT_COUNT {
        batch.clear();
        recver
            .recv_batch(&mut batch)
            .await
            .expect("recv_batch failed");
        for pkt in &batch {
            check_packet(pkt, next_seq);
            next_seq += 1;
        }
    }
}

/// Bulk bidirectional transfer through the real server relay must not wedge.
/// 5 000 × 1350 B in each direction (~13.5 MB total) — far beyond the 256 KiB
/// initial yamux window and the 512-frame relay queue, so both flow control
/// replenishment and queue backpressure are exercised under cross-direction
/// contention.
#[tokio::test]
async fn vpn_relay_link_bulk_bidirectional_no_wedge() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server("10.96.0.0/16").await;

    // Listener handshake.
    let (l_opener, mut l_acceptor) = mux_connect().await;
    let mut l_ctrl = Delimited::new(l_opener.open().await.unwrap());
    l_ctrl
        .send(ClientMessage::HelloVpn {
            max_clients: 0,
            id: "relay-link".into(),
            advertised: vec![],
            addr: VpnAddrRequest::Pool,
            notes: None,
            carriers: 1,
            relay_only: false,
            pin_mtu: false,
            mtu: None,
            forward_accept: false,
            nat_masquerade: false,
            route_policy: None,
            nat_udp_preferred_port: 0,
        })
        .await
        .unwrap();

    // Connector handshake.
    time::sleep(Duration::from_millis(80)).await;
    let (c_opener, _c_acceptor) = mux_connect().await;
    let mut c_ctrl = Delimited::new(c_opener.open().await.unwrap());
    c_ctrl
        .send(ClientMessage::ConnectVpn {
            id: "relay-link".into(),
            advertised: vec![],
            addr: VpnAddrRequest::Pool,
            notes: None,
            carriers: 1,
            relay_only: false,
            pin_mtu: false,
            mtu: None,
            forward_accept: false,
            nat_masquerade: false,
            route_policy: None,
            nat_udp_preferred_port: 0,
        })
        .await
        .unwrap();

    let c_nonce = match c_ctrl.recv::<ServerMessage>().await.unwrap() {
        Some(ServerMessage::VpnReady { session_nonce, .. }) => session_nonce,
        other => panic!("connector expected VpnReady, got {other:?}"),
    };
    let l_nonce = match l_ctrl.recv::<ServerMessage>().await.unwrap() {
        Some(ServerMessage::VpnReady { session_nonce, .. }) => session_nonce,
        other => panic!("listener expected VpnReady, got {other:?}"),
    };
    assert_eq!(c_nonce, l_nonce, "both sides must share the session nonce");

    // Build the relay links exactly as the clients do.
    let (c_egress, c_ingress) = link::connect_relay(&c_opener).await.unwrap();
    let (c_sender, c_recver) = link::make_relay(
        c_egress,
        c_ingress,
        bore_cli::vpn::crypto::derive_keys_connector(SECRET, &c_nonce).unwrap(),
    );

    let (l_egress, l_ingress) = link::accept_relay(&mut l_acceptor).await.unwrap();
    let (l_sender, l_recver) = link::make_relay(
        l_egress,
        l_ingress,
        bore_cli::vpn::crypto::derive_keys_listener(SECRET, &l_nonce).unwrap(),
    );

    // Pump both directions at once; receivers gate completion.
    let c_out = tokio::spawn(pump_out(c_sender));
    let l_out = tokio::spawn(pump_out(l_sender));
    let c_in = tokio::spawn(pump_in(c_recver));
    let l_in = tokio::spawn(pump_in(l_recver));

    let both = async {
        c_in.await.unwrap();
        l_in.await.unwrap();
    };
    time::timeout(Duration::from_secs(60), both)
        .await
        .expect("relay link wedged: bulk bidirectional transfer did not complete");

    c_out.abort();
    l_out.abort();
}

// ─── §4.1 multi-carrier relay (C3) ───────────────────────────────────────────

/// Receive PKT_COUNT packets accepting ANY order (DEC-7: round-robin across
/// carriers reorders datagrams); assert the seq set is complete and payloads
/// intact.
async fn pump_in_unordered(mut recver: link::LinkRecver) {
    let mut seen = std::collections::HashSet::new();
    let mut batch = Vec::with_capacity(64);
    while (seen.len() as u64) < PKT_COUNT {
        batch.clear();
        recver
            .recv_batch(&mut batch)
            .await
            .expect("recv_batch failed");
        for pkt in &batch {
            assert_eq!(pkt.len(), PKT_LEN, "packet length mismatch");
            let seq = u64::from_be_bytes(pkt[..8].try_into().unwrap());
            for (i, b) in pkt[8..].iter().enumerate() {
                assert_eq!(*b, (seq as usize + i) as u8, "packet payload corrupted");
            }
            assert!(seen.insert(seq), "duplicate packet seq {seq}");
            assert!(seq < PKT_COUNT, "seq {seq} out of range");
        }
    }
}

/// Pair through the real server with `carriers` substream pairs and return the
/// two link halves per side.
async fn pair_multi(
    id: &str,
    carriers: u16,
) -> (
    (link::LinkSender, link::LinkRecver),
    (link::LinkSender, link::LinkRecver),
) {
    // Listener handshake.
    let (l_opener, mut l_acceptor) = mux_connect().await;
    let mut l_ctrl = Delimited::new(l_opener.open().await.unwrap());
    l_ctrl
        .send(ClientMessage::HelloVpn {
            max_clients: 0,
            id: id.to_string(),
            advertised: vec![],
            addr: VpnAddrRequest::Pool,
            notes: None,
            carriers,
            relay_only: false,
            pin_mtu: false,
            mtu: None,
            forward_accept: false,
            nat_masquerade: false,
            route_policy: None,
            nat_udp_preferred_port: 0,
        })
        .await
        .unwrap();

    time::sleep(Duration::from_millis(80)).await;
    let (c_opener, _c_acceptor) = mux_connect().await;
    let mut c_ctrl = Delimited::new(c_opener.open().await.unwrap());
    c_ctrl
        .send(ClientMessage::ConnectVpn {
            id: id.to_string(),
            advertised: vec![],
            addr: VpnAddrRequest::Pool,
            notes: None,
            carriers,
            relay_only: false,
            pin_mtu: false,
            mtu: None,
            forward_accept: false,
            nat_masquerade: false,
            route_policy: None,
            nat_udp_preferred_port: 0,
        })
        .await
        .unwrap();

    let (c_nonce, c_carriers) = match c_ctrl.recv::<ServerMessage>().await.unwrap() {
        Some(ServerMessage::VpnReady {
            session_nonce,
            carriers,
            ..
        }) => (session_nonce, carriers),
        other => panic!("connector expected VpnReady, got {other:?}"),
    };
    assert_eq!(c_carriers, carriers, "negotiated carrier count");
    let l_nonce = match l_ctrl.recv::<ServerMessage>().await.unwrap() {
        Some(ServerMessage::VpnReady { session_nonce, .. }) => session_nonce,
        other => panic!("listener expected VpnReady, got {other:?}"),
    };
    assert_eq!(c_nonce, l_nonce);

    let (c_egress, c_ingress) = link::connect_relay_multi(&c_opener, carriers)
        .await
        .unwrap();
    let c_halves = link::make_relay_multi(
        c_egress,
        c_ingress,
        bore_cli::vpn::crypto::derive_keys_connector(SECRET, &c_nonce).unwrap(),
    );
    let (l_egress, l_ingress) = link::accept_relay_multi(&mut l_acceptor, carriers)
        .await
        .unwrap();
    let l_halves = link::make_relay_multi(
        l_egress,
        l_ingress,
        bore_cli::vpn::crypto::derive_keys_listener(SECRET, &l_nonce).unwrap(),
    );
    // Keep the control plumbing alive for the duration of the test.
    std::mem::forget((l_ctrl, c_ctrl, l_opener, c_opener, l_acceptor));
    (c_halves, l_halves)
}

/// Bulk bidirectional transfer over 4 carriers: every packet must arrive
/// exactly once (any order), no wedge, no loss.
#[tokio::test]
async fn vpn_relay_multi_carrier_bulk() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server("10.97.0.0/16").await;

    let ((c_sender, c_recver), (l_sender, l_recver)) = pair_multi("multi4", 4).await;

    let c_out = tokio::spawn(pump_out(c_sender));
    let l_out = tokio::spawn(pump_out(l_sender));
    let c_in = tokio::spawn(pump_in_unordered(c_recver));
    let l_in = tokio::spawn(pump_in_unordered(l_recver));

    let both = async {
        c_in.await.unwrap();
        l_in.await.unwrap();
    };
    time::timeout(Duration::from_secs(60), both)
        .await
        .expect("multi-carrier relay wedged: bulk transfer did not complete");

    c_out.abort();
    l_out.abort();
}

/// Killing one of 4 carriers mid-transfer must kill the link with a clean
/// error on the receiving side — no hang, no silent half-degraded state.
#[tokio::test]
async fn vpn_relay_multi_carrier_one_stream_dies() {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_vpn_server("10.98.0.0/16").await;

    // Build the connector side normally but keep raw listener streams so one
    // can be dropped (closing it) mid-test.
    let (l_opener, mut l_acceptor) = mux_connect().await;
    let mut l_ctrl = Delimited::new(l_opener.open().await.unwrap());
    l_ctrl
        .send(ClientMessage::HelloVpn {
            max_clients: 0,
            id: "die1".into(),
            advertised: vec![],
            addr: VpnAddrRequest::Pool,
            notes: None,
            carriers: 4,
            relay_only: false,
            pin_mtu: false,
            mtu: None,
            forward_accept: false,
            nat_masquerade: false,
            route_policy: None,
            nat_udp_preferred_port: 0,
        })
        .await
        .unwrap();
    time::sleep(Duration::from_millis(80)).await;
    let (c_opener, _c_acceptor) = mux_connect().await;
    let mut c_ctrl = Delimited::new(c_opener.open().await.unwrap());
    c_ctrl
        .send(ClientMessage::ConnectVpn {
            id: "die1".into(),
            advertised: vec![],
            addr: VpnAddrRequest::Pool,
            notes: None,
            carriers: 4,
            relay_only: false,
            pin_mtu: false,
            mtu: None,
            forward_accept: false,
            nat_masquerade: false,
            route_policy: None,
            nat_udp_preferred_port: 0,
        })
        .await
        .unwrap();
    let c_nonce = match c_ctrl.recv::<ServerMessage>().await.unwrap() {
        Some(ServerMessage::VpnReady { session_nonce, .. }) => session_nonce,
        other => panic!("connector expected VpnReady, got {other:?}"),
    };
    let _ = l_ctrl.recv::<ServerMessage>().await.unwrap();

    let (c_egress, c_ingress) = link::connect_relay_multi(&c_opener, 4).await.unwrap();
    let (c_sender, c_recver) = link::make_relay_multi(
        c_egress,
        c_ingress,
        bore_cli::vpn::crypto::derive_keys_connector(SECRET, &c_nonce).unwrap(),
    );
    let (mut l_egress, mut l_ingress) = link::accept_relay_multi(&mut l_acceptor, 4).await.unwrap();

    // Listener: drop one carrier pair (closing both substreams) to simulate a
    // dead carrier. The connector's matching ingress reader must error out.
    drop(l_egress.remove(3));
    drop(l_ingress.remove(3));

    // The surviving listener halves still build a (3-carrier) sender so the
    // remaining streams stay open — the connector recver must STILL die.
    let (l_sender, _l_recver) = link::make_relay_multi(
        l_egress,
        l_ingress,
        bore_cli::vpn::crypto::derive_keys_listener(SECRET, &c_nonce).unwrap(),
    );

    // Drive some connector traffic so the link is live.
    let pump = tokio::spawn(pump_out(c_sender));

    // The connector's recv_batch must return Err (its 4th reader hit EOF).
    let mut recver = c_recver;
    let mut batch = Vec::new();
    let died = async {
        loop {
            batch.clear();
            if recver.recv_batch(&mut batch).await.is_err() {
                return;
            }
        }
    };
    time::timeout(Duration::from_secs(15), died)
        .await
        .expect("link did not die after a carrier was killed");

    pump.abort();
    drop(l_sender);
}
