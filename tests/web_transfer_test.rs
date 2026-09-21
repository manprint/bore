//! Web-transfer integration tests. Serial (`--test-threads=1`): every test
//! owns dynamic ports; no two share a control port.

#[path = "support/web_transfer.rs"]
mod support;

use anyhow::{Context, Result};
use bore_cli::server::Server;
use bore_cli::web_transfer_protocol::{
    decode_room_link_seed, derive_room_link_material, RoomLinkMaterial,
};
use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::TcpStream;

const WEB_ADMIN_TOKEN: &str = "0123456789abcdef0123456789abcdef01234567";

/// T-WEB-CONFIG: a real server with valid flags binds; every invalid relation
/// exits nonzero before the control port accepts; without a base URL the
/// web-transfer state is absent while the legacy listener still works.
#[tokio::test]
async fn t_web_config() -> Result<()> {
    // Valid flags: binds, serves, owns exactly one registry.
    let port = support::free_port().await?;
    let registry = support::spawn_enabled_server(port).await?;
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_ok());
    assert_eq!(
        registry.totals(),
        bore_cli::web_transfer::WebTransferLimits::default()
    );
    drop(registry);

    // Invalid relations exit nonzero before the port accepts.
    let binary = std::env::var_os("CARGO_BIN_EXE_bore")
        .context("CARGO_BIN_EXE_bore is not available for web-transfer config tests")?;
    for extra in [
        vec!["--web-transfer-owner-grace", "4"],
        vec!["--web-transfer-max-rooms", "0"],
        vec!["--web-transfer-max-metadata-total", "1"],
        vec!["--web-transfer-max-rooms", "8"],
        vec!["--web-transfer-no-stun"],
    ] {
        let port = support::free_port().await?;
        let mut cmd = std::process::Command::new(&binary);
        cmd.arg("server")
            .arg("--control-port")
            .arg(port.to_string())
            .args(&extra)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        // Wait for the OUTCOME, never for a fixed moment: a valid config
        // binds, an invalid one exits nonzero, and how long either takes is a
        // property of the machine — a flat 400 ms read as "bound a listener"
        // on a loaded macos-14 runner while the process was still starting.
        let mut status = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            status = child.try_wait()?;
            if status.is_some() || TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                break;
            }
        }
        let refused = TcpStream::connect(("127.0.0.1", port)).await.is_err();
        match status {
            Some(status) => assert!(
                !status.success() && refused,
                "invalid flags {extra:?} must exit nonzero before accepting"
            ),
            None => {
                child.kill()?;
                panic!("invalid flags {extra:?} bound a listener instead of exiting");
            }
        }
    }

    // No base URL: plain server binds, registry absent, legacy path alive.
    let port = support::free_port().await?;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    assert!(server.web_transfer().is_none());
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_ok());
    Ok(())
}

/// T-WEB-REGISTRY-LIFE: with a test-only 200 ms grace, create through the
/// registry, drop the lease, resume before expiry (same room survives), drop
/// again and watch it disappear after expiry; a stale monitor cannot remove a
/// room re-inserted under the same ID.
#[tokio::test]
async fn t_web_registry_life() -> Result<()> {
    use bore_cli::web_transfer::{OwnerLease, RoomId};
    use std::time::Duration;

    let config = bore_cli::web_transfer::WebTransferConfig::new(
        bore_cli::web_transfer::WebTransferBaseUrl::parse("http://127.0.0.1:8080/")?,
        bore_cli::web_transfer::WebTransferLimits::default(),
        bore_cli::web_transfer::IceServerConfig {
            servers: Vec::new(),
        },
    )?;
    let registry = bore_cli::web_transfer::WebTransferRegistry::new(config)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let member_hash = [7u8; 32];
    let owner_byte = 9u8;
    let owner_token = bore_cli::web_transfer::OwnerToken::from_bytes([owner_byte; 32]);
    let owner_hash = owner_token.sha256_hash();

    let lease = OwnerLease::create(&registry, member_hash, owner_hash, false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let id = lease.id();
    let room = lease.room().clone();
    drop(lease);
    OwnerLease::detach_with_grace(&room, Duration::from_millis(200));
    // Resume before expiry: the same room remains, epoch bumped.
    let resumed =
        OwnerLease::resume(&registry, id, &owner_token).map_err(|e| anyhow::anyhow!("{e}"))?;
    assert_eq!(resumed.epoch(), 1);
    assert!(registry.room(id).is_some());
    let room = resumed.room().clone();
    drop(resumed);
    // Drop again: the room disappears after the grace.
    OwnerLease::detach_with_grace(&room, Duration::from_millis(200));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(registry.room(id).is_none());

    // Stale monitor vs reused ID: detach, re-insert under the same ID before
    // the monitor fires, and prove the new room survives it.
    let forced = RoomId::from_bytes([0xabu8; 16]);
    let first = registry
        .create_room_with_id(member_hash, owner_hash, forced, false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    OwnerLease::detach_with_grace(&first, Duration::from_millis(200));
    assert!(registry.remove_room_if_current(forced, &first));
    first.destroy("owner-close");
    let second = registry
        .create_room_with_id([8u8; 32], [8u8; 32], forced, false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let current = registry.room(forced).expect("reused room survives");
    assert!(std::sync::Arc::ptr_eq(&current, &second));
    assert!(!second.is_destroyed());
    Ok(())
}

/// Opens a raw native control substream (no auth server), mirroring
/// `secret_test::raw_control` on a dynamic port.
async fn raw_owner_control(
    port: u16,
) -> Result<(
    bore_cli::mux::Opener,
    bore_cli::shared::Delimited<bore_cli::mux::Stream>,
)> {
    use tokio::net::TcpStream;
    let tcp = TcpStream::connect(("127.0.0.1", port)).await?;
    let (opener, _acc) = bore_cli::mux::client(tcp);
    let stream = opener.open().await?;
    Ok((opener, bore_cli::shared::Delimited::new(stream)))
}

/// T-WEB-NATIVE-WIRE: real transport + yamux + control frames. Create,
/// heartbeat, drop, resume and close against a live server; clear
/// upgrade/configuration errors against a disabled server.
#[tokio::test]
async fn t_web_native_wire() -> Result<()> {
    use bore_cli::shared::{ClientMessage, ServerMessage};
    use bore_cli::web_transfer::{OwnerToken, RoomId, WebTransferLimits};

    let member = OwnerToken::from_bytes([0x31u8; 32]);
    let owner = OwnerToken::from_bytes([0x32u8; 32]);
    let requested_room_id = RoomId::from_bytes([0x41u8; 16]);
    let member_hash = member.sha256_hash();
    let owner_hash = owner.sha256_hash();

    // Enabled server on a dynamic port.
    let port = support::free_port().await?;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    let config =
        bore_cli::web_transfer::resolve_server_config(&support::enabled_args(), false, port)?
            .expect("loopback config resolves");
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    // Create: server answers with the room, the bare origin and epoch 0.
    let (_opener, mut control) = raw_owner_control(port).await?;
    control
        .send(ClientMessage::CreateWebTransferRoom {
            version: 2,
            room_id: requested_room_id,
            member_token_hash: member_hash,
            owner_token_hash: owner_hash,
            relay_only: false,
        })
        .await?;
    let (room_id, epoch) = match control.recv::<ServerMessage>().await? {
        Some(ServerMessage::WebTransferRoomCreated {
            room_id,
            base_url,
            owner_epoch,
            ..
        }) => {
            assert_eq!(base_url, "http://127.0.0.1:8080");
            assert!(!base_url.contains('#'));
            assert_eq!(room_id, requested_room_id);
            (room_id, owner_epoch)
        }
        other => panic!("expected Created, got {other:?}"),
    };
    assert_eq!(epoch, 0);

    // Heartbeat: no reply, but the loop stays alive for what follows.
    control.send(ClientMessage::Heartbeat).await?;
    // Drop: abnormal loss detaches instead of destroying.
    drop(control);
    drop(_opener);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let room = registry.room(room_id).expect("drop detaches, not destroys");
    assert!(matches!(
        room.state.lock().unwrap().owner,
        bore_cli::web_transfer::OwnerState::Detached { .. }
    ));
    drop(room);

    // Resume on a fresh control connection keeps the URL (same room ID).
    let (_opener, mut control) = raw_owner_control(port).await?;
    control
        .send(ClientMessage::ResumeWebTransferRoom {
            version: 2,
            room_id,
            owner_token: owner,
        })
        .await?;
    match control.recv::<ServerMessage>().await? {
        Some(ServerMessage::WebTransferRoomResumed {
            room_id: resumed,
            owner_epoch,
            ..
        }) => {
            assert_eq!(resumed, room_id);
            assert_eq!(owner_epoch, 1);
        }
        other => panic!("expected Resumed, got {other:?}"),
    }
    // Explicit close destroys immediately.
    control
        .send(ClientMessage::CloseWebTransferRoom {
            room_id,
            owner_epoch: 1,
        })
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(registry.room(room_id).is_none());
    drop(control);
    drop(_opener);

    // Disabled server: clear upgrade/configuration error, nothing allocated.
    let port = support::free_port().await?;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    assert!(server.web_transfer().is_none());
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;
    let (_opener, mut control) = raw_owner_control(port).await?;
    control
        .send(ClientMessage::CreateWebTransferRoom {
            version: 2,
            room_id: RoomId::from_bytes([0x42u8; 16]),
            member_token_hash: member_hash,
            owner_token_hash: owner_hash,
            relay_only: false,
        })
        .await?;
    match control.recv::<ServerMessage>().await? {
        Some(ServerMessage::Error(message)) => assert!(message.contains("upgrade"), "{message}"),
        other => panic!("expected upgrade error, got {other:?}"),
    }
    drop(control);
    drop(_opener);

    // Wrong version against an enabled server: error, zero rooms.
    let port = support::free_port().await?;
    let registry = support::spawn_enabled_server(port).await?;
    let (_opener, mut control) = raw_owner_control(port).await?;
    control
        .send(ClientMessage::CreateWebTransferRoom {
            version: 1,
            room_id: RoomId::from_bytes([0x43u8; 16]),
            member_token_hash: member_hash,
            owner_token_hash: owner_hash,
            relay_only: false,
        })
        .await?;
    match control.recv::<ServerMessage>().await? {
        Some(ServerMessage::Error(message)) => assert!(message.contains("version"), "{message}"),
        other => panic!("expected version error, got {other:?}"),
    }
    assert_eq!(registry.totals(), WebTransferLimits::default());
    drop(control);
    drop(_opener);
    Ok(())
}

/// Keeps only the SERVER's own line, and drops everything else the process
/// happens to print.
///
/// These buffers exist to answer "what did the server log?", and the capture
/// is process-wide: at `TRACE` the `log` bridge puts the test's OWN WebSocket
/// client in there, and `tungstenite::protocol` prints
/// `Sending frame: ... payload: b"..."` — the bytes the test just sent. Read
/// as the server's log, that is an accusation of a payload leak the product
/// does not have. MEASURED under `cargo test`'s default parallelism, where
/// the order of the tests decides whether the bridge is at `TRACE` when this
/// capture is installed: `LEAKLINE [relayed payload] ... TRACE
/// tungstenite::protocol: Sending frame: ... frame-CANARY-PAYLOAD-numbat`.
/// The claim is about the server, so the evidence has to be the server's.
fn is_server_line(buf: &[u8]) -> bool {
    String::from_utf8_lossy(buf).contains(" bore_cli")
}

/// Captured tracing output for the log-privacy assertion.
#[derive(Clone)]
struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if is_server_line(buf) {
            self.0.lock().unwrap().extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// T-WEB-OWNER-LEASE: a real owner loop creates one room, survives a forced
/// native connection reset through resume while retaining its URL, closes
/// immediately on each synthetic clean lifecycle event, and expires after
/// the grace when reconnect is prevented. No secret reaches tracing output.
#[tokio::test]
async fn t_web_owner_lease() -> Result<()> {
    use bore_cli::web_transfer::{resolve_server_config, OwnerState};
    use bore_cli::web_transfer_cli::{
        run_owner_lease, OwnerClientConfig, OwnerLifecycle, OwnerShutdown,
    };

    // Capture every log line this process emits (serial suite: no other test
    // writes concurrently).
    let log_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = LogSink(std::sync::Arc::clone(&log_buf));
    let _ = tracing_subscriber::fmt()
        .with_writer(move || sink.clone())
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    // One room created THROUGH a killable proxy: breaking the proxy is a
    // real transport loss for both ends (aborting the listen task would
    // leave the accepted connection alive and prove nothing).
    let server_port = support::free_port().await?;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(server_port);
    let config = resolve_server_config(&support::enabled_args_with_grace(5), false, server_port)?
        .expect("loopback config resolves");
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(server_port, true).await;
    let proxy = support::spawn_proxy(0, server_port).await?;
    let proxy_port = proxy.port();

    let endpoint = format!("127.0.0.1:{proxy_port}");
    let owner_config = || OwnerClientConfig {
        endpoint: endpoint.clone(),
        secret: None,
        insecure: false,
        open_browser: false,
        owner_grace_secs: 5,
        relay_only: false,
    };

    let (created_tx, created_rx) = tokio::sync::oneshot::channel();
    let (_lifecycle_tx, lifecycle_rx) = tokio::sync::mpsc::channel(4);
    let run = tokio::spawn(run_owner_lease(owner_config(), created_tx, lifecycle_rx));
    let created = created_rx.await.expect("room delivered");
    let url = created.display_url.clone();
    let room_id = created.room_id;
    // Forced reset: kill the proxy, restart it at once, and watch the owner
    // resume the SAME room (URL retained) through the fresh path.
    proxy.kill().await;
    let _proxy = support::spawn_proxy(proxy_port, server_port).await?;
    // Resume bumps the epoch on the same room (URL retained).
    let mut epoch_seen = 0u64;
    for _ in 0..500 {
        if let Some(room) = registry.room(room_id) {
            let attached = matches!(
                room.state.lock().unwrap().owner,
                OwnerState::Attached { epoch, .. } if { epoch_seen = epoch; epoch >= 1 }
            );
            if attached {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        epoch_seen >= 1,
        "owner resumed the same room after the reset"
    );
    run.abort();

    // Each clean lifecycle event closes immediately on a fresh run.
    for event in [
        OwnerLifecycle::Interrupt,
        OwnerLifecycle::Terminate,
        OwnerLifecycle::Hangup,
    ] {
        let port = support::free_port().await?;
        let mut server = Server::new(1024..=65535, None);
        server.set_control_port(port);
        let config = resolve_server_config(&support::enabled_args_with_grace(5), false, port)?
            .expect("loopback config resolves");
        server.set_web_transfer(config)?;
        let registry = server.web_transfer().expect("registry enabled");
        tokio::spawn(server.listen());
        support::wait_port(port, true).await;
        let (created_tx, created_rx) = tokio::sync::oneshot::channel();
        let (lifecycle_tx, lifecycle_rx) = tokio::sync::mpsc::channel(4);
        let endpoint = format!("127.0.0.1:{port}");
        let run = tokio::spawn(run_owner_lease(
            OwnerClientConfig {
                endpoint,
                owner_grace_secs: 5,
                ..OwnerClientConfig::default()
            },
            created_tx,
            lifecycle_rx,
        ));
        let created = created_rx.await.expect("room delivered");
        lifecycle_tx.send(event).await.unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("close is immediate")
            .unwrap()?;
        assert_eq!(outcome, OwnerShutdown::CleanClose, "event {event:?}");
        // The close is sent; the server destroys on receipt — poll, since
        // the send only proves the bytes reached the server socket.
        let mut gone = false;
        for _ in 0..200 {
            if registry.room(created.room_id).is_none() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(gone, "event {event:?} destroys at once");
    }

    // Reconnect prevented: the loop expires after the grace and never
    // creates a replacement room. The proxy dies and stays dead, so both
    // ends observe the loss symmetrically (aborting the listen task would
    // leave the accepted connection alive on one side only).
    let server_port = support::free_port().await?;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(server_port);
    let config = resolve_server_config(&support::enabled_args_with_grace(5), false, server_port)?
        .expect("loopback config resolves");
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(server_port, true).await;
    let proxy = support::spawn_proxy(0, server_port).await?;
    let (created_tx, created_rx) = tokio::sync::oneshot::channel();
    let (_lifecycle_tx, lifecycle_rx) = tokio::sync::mpsc::channel(4);
    let endpoint = format!("127.0.0.1:{}", proxy.port());
    let run = tokio::spawn(run_owner_lease(
        OwnerClientConfig {
            endpoint,
            owner_grace_secs: 5,
            ..OwnerClientConfig::default()
        },
        created_tx,
        lifecycle_rx,
    ));
    let created = created_rx.await.expect("room delivered");
    proxy.kill().await;
    let outcome = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("grace expiry terminates the loop")
        .unwrap()?;
    assert_eq!(outcome, OwnerShutdown::GraceExpired);
    // The detached room expires on the server grace; no replacement appears.
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(registry.room(created.room_id).is_none());

    // Log privacy: the fragment secrets and full URL never reach tracing.
    // The first run's URL carries every secret this test generated.
    let logs = String::from_utf8_lossy(&log_buf.lock().unwrap()).into_owned();
    let fragment = url.split('#').nth(1).expect("URL carries a fragment");
    assert!(!logs.contains(&url), "full URL in logs");
    for secret in fragment.split('&') {
        assert!(!secret.is_empty());
        assert!(!logs.contains(secret), "secret {secret} in logs");
    }
    Ok(())
}

/// T-WEB-HANDSHAKE (Phase 6.1): the upgrade itself is admitted.
///
/// A WebSocket handshake is the one stretch of an inbound connection paid for
/// before anything about the caller is known — past the Origin and
/// subprotocol check, and short of every cap that counts rooms, peers or
/// relays. This gate holds every pending-handshake slot and proves the three
/// halves of the bound: an upgrade is refused with a GENERIC 503 (it says
/// nothing about the room, present or absent), the rest of the surface keeps
/// serving, and releasing the slots makes the very same request upgrade.
#[tokio::test]
async fn t_web_handshake_admission() -> Result<()> {
    let port = support::free_port().await?;
    let base = format!("http://127.0.0.1:{port}/");
    let mut args = support::enabled_args();
    args.base_url = Some(base.clone());
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let host = format!("127.0.0.1:{port}");
    let room = "0123456789abcdef0123456789abcdef";
    let upgrade = ws_upgrade_request(
        &host,
        &format!("/transfer/ws/control/{room}"),
        &base[..base.len() - 1],
        Some(bore_cli::web_transfer_protocol::CONTROL_SUBPROTOCOL),
    );

    // Hold every slot. The count is the constant, exactly — a bound nobody
    // can observe is a bound nobody can size.
    let mut held = Vec::new();
    while let Some(permit) = registry.try_acquire_handshake() {
        held.push(permit);
    }
    assert_eq!(
        held.len(),
        bore_cli::web_transfer::WEB_TRANSFER_PENDING_HANDSHAKES,
        "pending-handshake slots"
    );
    assert_eq!(registry.handshake_slots_available(), 0);

    let refused = http_exchange(TcpStream::connect(("127.0.0.1", port)).await?, &upgrade).await?;
    assert!(
        refused.starts_with("HTTP/1.1 503"),
        "saturated upgrade must be a generic 503: {refused:.80}"
    );
    // Generic: no room, no reason, no hint that this ID exists or does not.
    assert!(!refused.contains(room), "503 names the room");

    // The bound is on the HANDSHAKE, not on the surface: the shell still
    // serves, so a saturated server still tells a browser what happened.
    let shell = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &format!("GET /transfer/{room} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )
    .await?;
    assert!(shell.starts_with("HTTP/1.1 200"), "shell: {shell:.60}");

    // Released — by dropping the permits, which is the only way they are
    // ever released — the same request upgrades.
    drop(held);
    assert_eq!(
        registry.handshake_slots_available(),
        bore_cli::web_transfer::WEB_TRANSFER_PENDING_HANDSHAKES
    );
    let accepted =
        ws_handshake_exchange(TcpStream::connect(("127.0.0.1", port)).await?, &upgrade).await?;
    assert!(
        accepted.starts_with("HTTP/1.1 101"),
        "released slot must upgrade: {accepted:.80}"
    );
    // And the completed handshake gave its slot back at once: the permit
    // bounds the upgrade, never the session that follows it.
    for _ in 0..50 {
        if registry.handshake_slots_available()
            == bore_cli::web_transfer::WEB_TRANSFER_PENDING_HANDSHAKES
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        registry.handshake_slots_available(),
        bore_cli::web_transfer::WEB_TRANSFER_PENDING_HANDSHAKES,
        "a live session must not hold a handshake slot"
    );
    Ok(())
}

/// One raw HTTP request over `stream`; reads to close (server always closes).
async fn http_exchange<S>(mut stream: S, request: &str) -> Result<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await??;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// One WebSocket handshake over `stream`; reads only the response head (the
/// server holds the socket open after a 101 in this phase).
async fn ws_handshake_exchange<S>(mut stream: S, request: &str) -> Result<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    let head = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            let scan_from = buf.len().saturating_sub(3);
            buf.extend_from_slice(&chunk[..n]);
            if buf[scan_from..].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            anyhow::ensure!(buf.len() < 16 * 1024, "handshake head too large");
        }
        Ok::<_, anyhow::Error>(String::from_utf8_lossy(&buf).into_owned())
    })
    .await??;
    Ok(head)
}

fn ws_upgrade_request(host: &str, path: &str, origin: &str, protocol: Option<&str>) -> String {
    let proto = protocol
        .map(|p| format!("Sec-WebSocket-Protocol: {p}\r\n"))
        .unwrap_or_default();
    format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGVzdC1ib3JlLXdz\r\nSec-WebSocket-Version: 13\r\n\
         Origin: {origin}\r\n{proto}\r\n"
    )
}

const WEB_CSP: &str = "Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; worker-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Starts a loopback-HTTP server with web-transfer + admin + unified vhost on
/// `port`, using `base_url` as the web authority.
async fn spawn_web_http_server(port: u16, base_url: &str) -> Result<()> {
    use bore_cli::vhost::{VhostConfig, VhostModeCfg};
    let mut args = support::enabled_args();
    args.base_url = Some(base_url.to_string());
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    server.set_web_transfer(config)?;
    // Unified topology (frontend port == control port): no extra bind, the
    // control port routes vhost by Host first — the exact ordering 2.1 must
    // preserve for non-web hosts.
    server.set_vhost(VhostConfig {
        base_domain: "example.invalid".to_string(),
        mode: VhostModeCfg::Http,
        http_port: port,
        https_port: 443,
        cert_file: None,
        key_file: None,
        default_headers: Default::default(),
        default_response_headers: Default::default(),
        reservations: vec![],
    })?;
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;
    Ok(())
}

/// T-WEB-HTTP: real loopback-HTTP and TLS servers serve the room shell and
/// assets and complete an authenticated upgrade check; wrong host, origin or
/// subprotocol fail; admin and vhost-miss routing on the same listener are
/// preserved.
#[tokio::test]
async fn t_web_http() -> Result<()> {
    let room_a = "0123456789abcdef0123456789abcdef";
    let room_b = "ffffffffffffffffffffffffffffffff";

    // Loopback-HTTP server.
    let port = support::free_port().await?;
    let base = format!("http://127.0.0.1:{port}/");
    spawn_web_http_server(port, &base).await?;
    let host = format!("127.0.0.1:{port}");

    // Room shell: 200 + exact security headers for every valid ID, present or
    // absent — the two bodies are identical (existence hidden).
    let req = |path: &str, host: &str| {
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
    };
    let shell_a = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &req(&format!("/transfer/{room_a}"), &host),
    )
    .await?;
    let short_shell = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &req("/transfer/", &host),
    )
    .await?;
    let shell_b = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &req(&format!("/transfer/{room_b}"), &host),
    )
    .await?;
    for (name, shell) in [("a", &shell_a), ("b", &shell_b)] {
        assert!(
            shell.starts_with("HTTP/1.1 200"),
            "shell {name}: {shell:.60}"
        );
        assert!(shell.contains("text/html"), "shell {name} MIME");
        assert!(shell.contains(WEB_CSP), "shell {name} CSP");
        assert!(
            shell.contains("Referrer-Policy: no-referrer"),
            "shell {name}"
        );
        assert!(
            shell.contains("X-Content-Type-Options: nosniff"),
            "shell {name}"
        );
        assert!(
            shell.contains("Permissions-Policy: camera=(), microphone=(), geolocation=()"),
            "shell {name}"
        );
        assert!(shell.contains("Cache-Control: no-cache"), "shell {name}");
        assert!(
            !shell
                .to_ascii_lowercase()
                .contains("access-control-allow-origin"),
            "shell {name} must send no CORS wildcard"
        );
    }
    let body = |resp: &str| resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    assert_eq!(body(&shell_a), body(&shell_b), "shell bodies must match");
    assert!(
        short_shell.starts_with("HTTP/1.1 200"),
        "short shell: {short_shell:.60}"
    );
    assert!(short_shell.contains(WEB_CSP), "short shell CSP");
    assert_eq!(
        body(&shell_a),
        body(&short_shell),
        "short shell body must match"
    );
    assert!(
        !body(&shell_a).is_empty(),
        "shell carries the embedded page"
    );

    // HEAD serves headers with the GET length and no body.
    let head = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &format!("HEAD /transfer/{room_a} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )
    .await?;
    assert!(head.starts_with("HTTP/1.1 200"), "HEAD: {head:.60}");
    assert_eq!(body(&head), "", "HEAD must carry no body");

    // Static assets with exact MIME.
    let js = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &req("/transfer/assets/app.js", &host),
    )
    .await?;
    assert!(js.starts_with("HTTP/1.1 200"), "js: {js:.60}");
    assert!(js.contains("text/javascript"), "js MIME");
    assert!(js.contains(WEB_CSP), "js CSP");
    let css = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &req("/transfer/assets/app.css", &host),
    )
    .await?;
    assert!(css.starts_with("HTTP/1.1 200"), "css: {css:.60}");
    assert!(css.contains("text/css"), "css MIME");

    // Malformed web paths are web-owned 404s; wrong methods are 405.
    for path in [
        "/transfer/%2fetc".to_string(),
        "/transfer//a".to_string(),
        "/transfer/../x".to_string(),
        "/transfer/0123456789ABCDEF0123456789ABCDEF".to_string(),
        "/transfer/short".to_string(),
        "/transfer/nope".to_string(),
    ] {
        let resp = http_exchange(
            TcpStream::connect(("127.0.0.1", port)).await?,
            &req(&path, &host),
        )
        .await?;
        assert!(resp.starts_with("HTTP/1.1 404"), "{path}: {resp:.60}");
        assert!(resp.contains(WEB_CSP), "{path} carries web headers");
    }
    let post = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &format!("POST /transfer/{room_a} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    )
    .await?;
    assert!(post.starts_with("HTTP/1.1 405"), "POST room: {post:.60}");

    // Wrong host: not the web surface — falls through to the admin 404 (no
    // web CSP), proving the interception is narrow.
    let foreign = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &req(&format!("/transfer/{room_a}"), "other.invalid"),
    )
    .await?;
    assert!(
        foreign.starts_with("HTTP/1.1 404"),
        "foreign host: {foreign:.60}"
    );
    assert!(
        !foreign.contains("default-src 'none'"),
        "foreign host must not get web headers"
    );

    // Admin on the same listener is untouched: shell without token, 401
    // without token on data, JSON with token.
    let admin_shell = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &req("/admin/status", "x"),
    )
    .await?;
    assert!(
        admin_shell.starts_with("HTTP/1.1 200"),
        "admin shell: {admin_shell:.60}"
    );
    let admin_401 = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &req("/admin/status/data", "x"),
    )
    .await?;
    assert!(
        admin_401.starts_with("HTTP/1.1 401"),
        "admin data: {admin_401:.60}"
    );
    let admin_ok = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &format!("GET /admin/status/data HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {WEB_ADMIN_TOKEN}\r\nConnection: close\r\n\r\n"),
    )
    .await?;
    assert!(
        admin_ok.starts_with("HTTP/1.1 200"),
        "admin data auth: {admin_ok:.60}"
    );
    assert!(admin_ok.contains("\"tunnels\""), "admin JSON shape");

    // Vhost-first ordering on the same listener: a subdomain Host with no
    // provider misses to the admin 404, not to the web surface.
    let vhost_miss = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &req("/", "ghost.example.invalid"),
    )
    .await?;
    assert!(
        vhost_miss.starts_with("HTTP/1.1 404"),
        "vhost miss: {vhost_miss:.60}"
    );
    assert!(
        !vhost_miss.contains("default-src 'none'"),
        "vhost miss must not get web headers"
    );

    // Authenticated upgrade check on the loopback server.
    let ws_path = format!("/transfer/ws/control/{room_a}");
    let good = ws_handshake_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &ws_upgrade_request(
            &host,
            &ws_path,
            &base[..base.len() - 1],
            Some(bore_cli::web_transfer_protocol::CONTROL_SUBPROTOCOL),
        ),
    )
    .await?;
    assert!(good.starts_with("HTTP/1.1 101"), "ws upgrade: {good:.120}");
    assert!(
        good.to_ascii_lowercase()
            .contains("sec-websocket-protocol: bore-transfer-v1"),
        "ws subprotocol echo: {good:.120}"
    );
    // Wrong origin, wrong subprotocol, missing subprotocol, WS query and
    // wrong host all fail without a 101.
    for (name, request) in [
        (
            "origin",
            ws_upgrade_request(
                &host,
                &ws_path,
                "http://evil.invalid",
                Some(bore_cli::web_transfer_protocol::CONTROL_SUBPROTOCOL),
            ),
        ),
        (
            "protocol",
            ws_upgrade_request(
                &host,
                &ws_path,
                &base[..base.len() - 1],
                Some("bore-transfer-v2"),
            ),
        ),
        (
            "missing-protocol",
            ws_upgrade_request(&host, &ws_path, &base[..base.len() - 1], None),
        ),
        (
            "query",
            ws_upgrade_request(
                &host,
                &format!("{ws_path}?x=1"),
                &base[..base.len() - 1],
                Some(bore_cli::web_transfer_protocol::CONTROL_SUBPROTOCOL),
            ),
        ),
        (
            "host",
            ws_upgrade_request(
                "other.invalid",
                &ws_path,
                &base[..base.len() - 1],
                Some(bore_cli::web_transfer_protocol::CONTROL_SUBPROTOCOL),
            ),
        ),
    ] {
        let resp =
            ws_handshake_exchange(TcpStream::connect(("127.0.0.1", port)).await?, &request).await?;
        assert!(
            !resp.starts_with("HTTP/1.1 101"),
            "ws {name} must fail: {resp:.120}"
        );
    }

    // TLS server: same shell/assets/upgrade over https, with HSTS.
    let tls_port = support::free_port().await?;
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    let (cert, key) = (certified.cert.pem(), certified.signing_key.serialize_pem());
    let acceptor = bore_cli::transport::server_tls_from_pem(cert.as_bytes(), key.as_bytes())?;
    let tls_base = format!("https://localhost:{tls_port}/");
    let mut args = support::enabled_args();
    args.base_url = Some(tls_base.clone());
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, tls_port)?
        .expect("tls config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(tls_port);
    server.set_tls(acceptor);
    server.set_web_transfer(config)?;
    tokio::spawn(server.listen());
    support::wait_port(tls_port, true).await;
    let endpoint = bore_cli::transport::Endpoint {
        host: "localhost".to_string(),
        port: tls_port,
        tls: true,
    };
    let tls_origin = format!("https://localhost:{tls_port}");
    let stream = bore_cli::transport::connect(&endpoint, true).await?;
    let tls_shell = http_exchange(
        stream,
        &req(
            &format!("/transfer/{room_a}"),
            &format!("localhost:{tls_port}"),
        ),
    )
    .await?;
    assert!(
        tls_shell.starts_with("HTTP/1.1 200"),
        "tls shell: {tls_shell:.60}"
    );
    assert!(
        tls_shell.contains("Strict-Transport-Security:"),
        "tls shell HSTS"
    );
    assert!(tls_shell.contains(WEB_CSP), "tls shell CSP");
    let stream = bore_cli::transport::connect(&endpoint, true).await?;
    let tls_js = http_exchange(
        stream,
        &req("/transfer/assets/app.js", &format!("localhost:{tls_port}")),
    )
    .await?;
    assert!(tls_js.starts_with("HTTP/1.1 200"), "tls js: {tls_js:.60}");
    let stream = bore_cli::transport::connect(&endpoint, true).await?;
    let tls_ws = ws_handshake_exchange(
        stream,
        &ws_upgrade_request(
            &format!("localhost:{tls_port}"),
            &ws_path,
            &tls_origin,
            Some(bore_cli::web_transfer_protocol::CONTROL_SUBPROTOCOL),
        ),
    )
    .await?;
    assert!(tls_ws.starts_with("HTTP/1.1 101"), "tls ws: {tls_ws:.120}");
    Ok(())
}

/// Parses one server control message into `(type, body)`.
fn control_msg(text: &str) -> (String, serde_json::Value) {
    let value: serde_json::Value = serde_json::from_str(text).unwrap();
    (
        value["type"].as_str().unwrap().to_string(),
        value["body"].clone(),
    )
}

/// Reads one full snapshot (`begin`, sorted `peer` items, sorted `offer`
/// items, `end`) and asserts its internal consistency: matching revisions,
/// sorted peers, offers after peers.
async fn read_snapshot(
    peer: &mut support::WsPeer,
) -> Result<(u64, Vec<(String, Option<String>)>, Vec<serde_json::Value>)> {
    let wait = Duration::from_secs(5);
    let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("snapshot.begin"));
    assert_eq!(typ, "snapshot.begin");
    let revision = body["revision"].as_u64().unwrap();
    let mut peers = Vec::new();
    let mut offers = Vec::new();
    loop {
        let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("snapshot item"));
        if typ == "snapshot.end" {
            assert_eq!(body["revision"].as_u64(), Some(revision));
            break;
        }
        if typ == "snapshot.offer" {
            assert_eq!(body["revision"].as_u64(), Some(revision));
            offers.push(body);
            continue;
        }
        assert_eq!(typ, "snapshot.peer");
        assert_eq!(body["revision"].as_u64(), Some(revision));
        assert!(offers.is_empty(), "peers precede offers");
        peers.push((
            body["peerId"].as_str().unwrap().to_string(),
            body.get("displayName")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        ));
    }
    let mut sorted = peers.clone();
    sorted.sort();
    assert_eq!(peers, sorted, "snapshot peers arrive sorted");
    Ok((revision, peers, offers))
}

/// T-WEB-PEERS: three real WebSocket clients authenticate, receive stable
/// sorted snapshots and join/rename/leave events; a lagged receiver past 256
/// events resynchronizes with a fresh snapshot; a stalled reader is removed
/// alone while the room and healthy peers survive, and counters return to
/// baseline after every disconnect.
#[tokio::test]
async fn t_web_peers() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};

    // Web-only server on a dynamic port (also proves the demux serves the
    // surface with neither admin nor vhost configured).
    let port = support::free_port().await?;
    let base = format!("http://127.0.0.1:{port}/");
    let mut args = support::enabled_args();
    args.base_url = Some(base);
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x41u8; 32]);
    let owner = OwnerToken::from_bytes([0x42u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(5);

    // A joins named, B and C anonymous: welcome carries identity, room,
    // resolved name, limits and ICE servers; the snapshot is stable.
    let mut a = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    a.hello(&token_hex, Some("Alice")).await?;
    let (typ, body) = control_msg(&a.next_text(wait).await?.expect("welcome A"));
    assert_eq!(typ, "welcome");
    let peer_a = body["peerId"].as_str().unwrap().to_string();
    assert_eq!(body["roomId"].as_str(), Some(room_hex.as_str()));
    assert_eq!(body["displayName"].as_str(), Some("Alice"));
    assert_eq!(body["limits"]["max_peers_per_room"].as_u64(), Some(32));
    assert!(body["iceServers"].is_array());
    let (rev, peers, _) = read_snapshot(&mut a).await?;
    assert_eq!(rev, 1);
    assert_eq!(peers, vec![(peer_a.clone(), Some("Alice".to_string()))]);

    let mut b = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    b.hello(&token_hex, None).await?;
    let (typ, body) = control_msg(&b.next_text(wait).await?.expect("welcome B"));
    assert_eq!(typ, "welcome");
    let peer_b = body["peerId"].as_str().unwrap().to_string();
    let name_b = body["displayName"].as_str().unwrap().to_string();
    assert!(name_b.starts_with("Peer "));
    let (rev, peers, _) = read_snapshot(&mut b).await?;
    assert_eq!(rev, 2);
    assert_eq!(peers.len(), 2);
    let (typ, body) = control_msg(&a.next_text(wait).await?.expect("joined B"));
    assert_eq!(typ, "peer.joined");
    assert_eq!(body["peerId"].as_str(), Some(peer_b.as_str()));
    assert_eq!(body["revision"].as_u64(), Some(2));

    let mut c = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    c.hello(&token_hex, None).await?;
    let (typ, _) = control_msg(&c.next_text(wait).await?.expect("welcome C"));
    assert_eq!(typ, "welcome");
    let (rev, peers, _) = read_snapshot(&mut c).await?;
    assert_eq!(rev, 3);
    assert_eq!(peers.len(), 3);
    for peer in [&mut a, &mut b] {
        let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("joined C"));
        assert_eq!(typ, "peer.joined");
        assert_eq!(body["revision"].as_u64(), Some(3));
    }

    // Rename: A is acked with the normalized name, B/C see one event.
    let rid = "a".repeat(32);
    a.send_text(format!(
        r#"{{"v":1,"type":"peer.rename","requestId":"{rid}","body":{{"displayName":"Alice2"}}}}"#
    ))
    .await?;
    let ack = a.next_text(wait).await?.expect("rename ack");
    let (typ, body) = control_msg(&ack);
    assert_eq!(typ, "ack");
    assert_eq!(body["result"]["displayName"].as_str(), Some("Alice2"));
    for peer in [&mut b, &mut c] {
        let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("renamed"));
        assert_eq!(typ, "peer.renamed");
        assert_eq!(body["peerId"].as_str(), Some(peer_a.as_str()));
        assert_eq!(body["displayName"].as_str(), Some("Alice2"));
        assert_eq!(body["revision"].as_u64(), Some(4));
    }
    // Duplicate (peer,requestId) replays the exact cached response. Our own
    // rename broadcast is also delivered to us, ahead of the replay.
    a.send_text(format!(
        r#"{{"v":1,"type":"peer.rename","requestId":"{rid}","body":{{"displayName":"Alice2"}}}}"#
    ))
    .await?;
    let replay = loop {
        let message = a.next_text(wait).await?.expect("replay");
        let (typ, _) = control_msg(&message);
        if typ == "ack" {
            break message;
        }
        assert_eq!(typ, "peer.renamed");
    };
    assert_eq!(replay, ack);

    // Leave: dropping C delivers exactly one `peer.left` to A and B.
    drop(c);
    for peer in [&mut a, &mut b] {
        let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("left"));
        assert_eq!(typ, "peer.left");
        assert_eq!(body["revision"].as_u64(), Some(5));
    }

    // Ordered load: forty paced renames arrive at B as forty consecutive
    // incrementals (revisions 6..45), never a snapshot — below capacity
    // nothing resynchronizes spuriously.
    for i in 0..40u32 {
        let id = format!("{i:032x}");
        a.send_text(format!(
            r#"{{"v":1,"type":"peer.rename","requestId":"{id}","body":{{"displayName":"load-{i}"}}}}"#
        ))
        .await?;
        let (typ, _) = control_msg(&a.next_text(wait).await?.expect("load ack"));
        assert_eq!(typ, "ack");
        let (typ, _) = control_msg(&a.next_text(wait).await?.expect("load self event"));
        assert_eq!(typ, "peer.renamed");
        tokio::time::sleep(Duration::from_millis(260)).await;
    }
    for i in 0..40u32 {
        let (typ, body) = control_msg(&b.next_text(wait).await?.expect("load event"));
        assert_eq!(typ, "peer.renamed");
        assert_eq!(
            body["displayName"].as_str(),
            Some(format!("load-{i}").as_str())
        );
        assert_eq!(body["revision"].as_u64(), Some(6 + i as u64));
    }

    // Quiet reap: S joins, reads its snapshot, then goes fully silent. The
    // 60 s reaper removes exactly S while A and B stay active (pings plus
    // two renames during the wait); counters and events confirm only S left.
    // (A live 256-event storm cannot exist on loopback: kernel buffers absorb
    // any bucket-legal storm, so the broadcast never lags — the resync
    // mechanism itself is proved deterministically by
    // `lagged_receiver_gets_ordered_snapshot_not_large_aggregate`.)
    let mut s = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    s.hello(&token_hex, None).await?;
    let (typ, _) = control_msg(&s.next_text(wait).await?.expect("welcome S"));
    assert_eq!(typ, "welcome");
    let (rev, _, _) = read_snapshot(&mut s).await?;
    assert_eq!(rev, 46);
    for peer in [&mut a, &mut b] {
        let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("joined S"));
        assert_eq!(typ, "peer.joined");
        assert_eq!(body["revision"].as_u64(), Some(46));
    }
    for (rid, name) in [("c".repeat(32), "Wait1"), ("d".repeat(32), "Wait2")] {
        a.send_text(format!(
            r#"{{"v":1,"type":"peer.rename","requestId":"{rid}","body":{{"displayName":"{name}"}}}}"#
        ))
        .await?;
        let (typ, _) = control_msg(&a.next_text(wait).await?.expect("wait ack"));
        assert_eq!(typ, "ack");
        let (typ, _) = control_msg(&a.next_text(wait).await?.expect("wait self"));
        assert_eq!(typ, "peer.renamed");
        let (typ, _) = control_msg(&b.next_text(wait).await?.expect("wait event"));
        assert_eq!(typ, "peer.renamed");
        tokio::time::sleep(Duration::from_secs(20)).await;
        for peer in [&mut a, &mut b] {
            peer.send_text(r#"{"v":1,"type":"ping","body":{}}"#.to_string())
                .await?;
            let (typ, _) = control_msg(&peer.next_text(wait).await?.expect("wait pong"));
            assert_eq!(typ, "pong");
        }
    }
    // S has now been silent ~40 s (snapshot read to here); the reaper fires
    // at 60 s quiet, so poll up to 30 s more. A/B stay healthy throughout.
    let mut closed = false;
    for _ in 0..60 {
        match s.next_text(Duration::from_millis(500)).await {
            Ok(None) => {
                closed = true;
                break;
            }
            _ => continue,
        }
    }
    assert!(closed, "silent peer must be reaped");
    for _ in 0..100 {
        if registry.current_peers() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(registry.current_peers(), 2);
    // A and B are healthy: the leave plus one more rename both arrive.
    for peer in [&mut a, &mut b] {
        let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("left S"));
        assert_eq!(typ, "peer.left");
        assert_eq!(body["revision"].as_u64(), Some(49));
    }
    let rid = "b".repeat(32);
    a.send_text(format!(
        r#"{{"v":1,"type":"peer.rename","requestId":"{rid}","body":{{"displayName":"Alive"}}}}"#
    ))
    .await?;
    let (typ, _) = control_msg(&a.next_text(wait).await?.expect("alive ack"));
    assert_eq!(typ, "ack");
    let (typ, body) = control_msg(&b.next_text(wait).await?.expect("alive event"));
    assert_eq!(typ, "peer.renamed");
    assert_eq!(body["displayName"].as_str(), Some("Alive"));
    assert_eq!(body["revision"].as_u64(), Some(50));

    // Every disconnect returns the counters to baseline.
    drop(a);
    drop(b);
    drop(lease);
    for _ in 0..100 {
        if registry.current_peers() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(registry.current_peers(), 0);
    Ok(())
}

/// Builds a minimal single-file offer manifest with a real rolling root for
/// the fixed 11-byte leaf (the server recomputes and compares it).
fn catalog_manifest(offer_hex: &str, label: &str, path: &str) -> serde_json::Value {
    let leaf = [0x11u8; 32];
    let root = bore_cli::web_transfer_protocol::file_root(1, &[leaf]).unwrap();
    serde_json::json!({
        "offer": offer_hex,
        "mode": "single",
        "label": label,
        "kind": "file",
        "chunkSize": "1048576",
        "createdAt": "2026-09-14T12:00:00Z",
        "entries": [{
            "id": "0",
            "path": path,
            "size": "11",
            "mtime": "1757779200",
            "chunks": [hex::encode(leaf)],
            "chunkCount": "1",
            "root": hex::encode(root),
        }],
    })
}

/// Publishes one offer and returns the ack body. The server also delivers
/// our own `offer.added` right after the ack; both are consumed here.
async fn catalog_publish(
    peer: &mut support::WsPeer,
    request_id: &str,
    offer_hex: &str,
    manifest: &serde_json::Value,
) -> Result<serde_json::Value> {
    peer.send_text(
        serde_json::json!({
            "v": 1,
            "type": "offer.publish",
            "requestId": request_id,
            "body": {"offerId": offer_hex, "manifest": manifest, "mac": "ee".repeat(32)},
        })
        .to_string(),
    )
    .await?;
    let wait = Duration::from_secs(5);
    let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("publish reply"));
    assert_eq!(typ, "ack");
    let (typ, added) = control_msg(&peer.next_text(wait).await?.expect("self added"));
    assert_eq!(typ, "offer.added");
    assert_eq!(added["offerId"].as_str(), Some(offer_hex));
    Ok(body)
}

/// Captured tracing output for the log-privacy assertion (per-test
/// dispatcher around the spawned server task).
#[derive(Clone)]
struct CatalogLogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CatalogLogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if is_server_line(buf) {
            self.0.lock().unwrap().extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// T-WEB-CATALOG-SERVER: A/B/C publish and withdraw through real control
/// sockets; snapshots and events converge on one catalog; cap rejection
/// leaves it unchanged; B cannot withdraw A; disconnecting B removes only
/// B's offers; captured logs omit the canary filenames.
#[tokio::test]
async fn t_web_catalog_server() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};

    let log_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = CatalogLogSink(std::sync::Arc::clone(&log_buf));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || sink.clone())
        .with_max_level(tracing::Level::TRACE)
        // Without this the field names are wrapped in ANSI escapes, so
        // `bytes=` never appears literally and an assertion on it would fail
        // for a reason that has nothing to do with what is logged.
        .with_ansi(false)
        .finish();
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    // Thread-local dispatcher for this test: tokio propagates it into every
    // spawned server task polled from here, so all server-side logs land in
    // the buffer below (serial suite — no other test logs concurrently).
    let _dispatcher_guard = tracing::dispatcher::set_default(&dispatch);

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x61u8; 32]);
    let owner = OwnerToken::from_bytes([0x62u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(5);

    // Canary filenames travel in labels and paths from here on.
    let label_a = "CANARY-LABEL-aardvark";
    let path_a = "canary-aardvark.txt";
    let offer_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let label_b = "CANARY-LABEL-badger";
    let path_b = "canary-badger.txt";
    let offer_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // A/B/C join; each drains exactly the joins that follow its own.
    let mut a = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    a.hello(&token_hex, Some("A")).await?;
    assert_eq!(
        control_msg(&a.next_text(wait).await?.expect("welcome")).0,
        "welcome"
    );
    read_snapshot(&mut a).await?;
    let mut b = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    b.hello(&token_hex, Some("B")).await?;
    assert_eq!(
        control_msg(&b.next_text(wait).await?.expect("welcome")).0,
        "welcome"
    );
    read_snapshot(&mut b).await?;
    let mut c = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    c.hello(&token_hex, Some("C")).await?;
    assert_eq!(
        control_msg(&c.next_text(wait).await?.expect("welcome")).0,
        "welcome"
    );
    read_snapshot(&mut c).await?;
    for (typ, rev) in [("peer.joined", 2), ("peer.joined", 3)] {
        let (got, body) = control_msg(&a.next_text(wait).await?.expect("join A"));
        assert_eq!(got, typ);
        assert_eq!(body["revision"].as_u64(), Some(rev));
    }
    let (got, body) = control_msg(&b.next_text(wait).await?.expect("join B"));
    assert_eq!(got, "peer.joined");
    assert_eq!(body["revision"].as_u64(), Some(3));

    // A and B publish; every peer sees both events in order (revisions 4,5).
    // Each publish is drained everywhere before the next goes out, so no
    // ack read ever meets another peer's stale event.
    let manifest_a = catalog_manifest(offer_a, label_a, path_a);
    let ack = catalog_publish(&mut a, &"c".repeat(32), offer_a, &manifest_a).await?;
    assert_eq!(ack["result"]["offerId"].as_str(), Some(offer_a));
    for peer in [&mut b, &mut c] {
        let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("added A"));
        assert_eq!(typ, "offer.added");
        assert_eq!(body["offerId"].as_str(), Some(offer_a));
        assert_eq!(body["revision"].as_u64(), Some(4));
    }
    let manifest_b = catalog_manifest(offer_b, label_b, path_b);
    let ack = catalog_publish(&mut b, &"d".repeat(32), offer_b, &manifest_b).await?;
    assert_eq!(ack["result"]["offerId"].as_str(), Some(offer_b));
    for peer in [&mut a, &mut c] {
        let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("added B"));
        assert_eq!(typ, "offer.added");
        assert_eq!(body["offerId"].as_str(), Some(offer_b));
        assert_eq!(body["revision"].as_u64(), Some(5));
    }

    // Convergence: a late joiner snapshots the whole catalog at revision 6.
    let mut d = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    d.hello(&token_hex, Some("D")).await?;
    assert_eq!(
        control_msg(&d.next_text(wait).await?.expect("welcome")).0,
        "welcome"
    );
    let (rev, peers, offers) = read_snapshot(&mut d).await?;
    assert_eq!(rev, 6);
    assert_eq!(peers.len(), 4);
    // Both offers converge in ID order with their exact manifests and MACs.
    assert_eq!(offers.len(), 2);
    assert_eq!(offers[0]["offerId"].as_str(), Some(offer_a));
    assert_eq!(offers[1]["offerId"].as_str(), Some(offer_b));
    assert_eq!(offers[0]["manifest"]["label"].as_str(), Some(label_a));
    assert_eq!(offers[0]["mac"].as_str(), Some("ee".repeat(32).as_str()));
    for peer in [&mut a, &mut b, &mut c] {
        let (typ, _) = control_msg(&peer.next_text(wait).await?.expect("joined D"));
        assert_eq!(typ, "peer.joined");
    }

    // An invalid publish (lying root) is rejected and changes nothing.
    let mut bad = catalog_manifest("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", "Bad", "bad.txt");
    bad["entries"][0]["root"] = serde_json::Value::String("0".repeat(64));
    d.send_text(
        serde_json::json!({
            "v": 1,
            "type": "offer.publish",
            "requestId": "e".repeat(32),
            "body": {"offerId": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", "manifest": bad, "mac": "ee".repeat(32)},
        })
        .to_string(),
    )
    .await?;
    let (typ, body) = control_msg(&d.next_text(wait).await?.expect("invalid reply"));
    assert_eq!(typ, "error");
    assert_eq!(body["code"].as_str(), Some("INVALID_MESSAGE"));

    // B cannot withdraw A's offer: NOT_PARTICIPANT, catalog intact.
    b.send_text(
        serde_json::json!({
            "v": 1,
            "type": "offer.withdraw",
            "requestId": "f".repeat(32),
            "body": {"offerId": offer_a},
        })
        .to_string(),
    )
    .await?;
    let (typ, body) = control_msg(&b.next_text(wait).await?.expect("withdraw reply"));
    assert_eq!(typ, "error");
    assert_eq!(body["code"].as_str(), Some("NOT_PARTICIPANT"));

    // A withdraws its own offer: everyone left sees exactly one removal.
    a.send_text(
        serde_json::json!({
            "v": 1,
            "type": "offer.withdraw",
            "requestId": "a0".repeat(16),
            "body": {"offerId": offer_a},
        })
        .to_string(),
    )
    .await?;
    let (typ, _) = control_msg(&a.next_text(wait).await?.expect("withdraw ack"));
    assert_eq!(typ, "ack");
    // A drains its own removal first so later reads stay aligned.
    let (typ, body) = control_msg(&a.next_text(wait).await?.expect("self removed"));
    assert_eq!(typ, "offer.removed");
    assert_eq!(body["offerId"].as_str(), Some(offer_a));
    for peer in [&mut b, &mut c, &mut d] {
        let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("removed"));
        assert_eq!(typ, "offer.removed");
        assert_eq!(body["offerId"].as_str(), Some(offer_a));
    }

    // Disconnecting B removes only B's offers: the rest see the removal plus
    // the leave, and a fresh snapshot holds A/C/D with no offers at all.
    drop(b);
    for peer in [&mut a, &mut c, &mut d] {
        let mut saw_removed = false;
        let mut saw_left = false;
        for _ in 0..4 {
            let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("drop events"));
            match typ.as_str() {
                "offer.removed" => {
                    assert_eq!(body["offerId"].as_str(), Some(offer_b));
                    saw_removed = true;
                }
                "peer.left" => saw_left = true,
                _ => {}
            }
            if saw_removed && saw_left {
                break;
            }
        }
        assert!(saw_removed && saw_left, "drop must remove offers then peer");
    }
    let mut e = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    e.hello(&token_hex, Some("E")).await?;
    assert_eq!(
        control_msg(&e.next_text(wait).await?.expect("welcome")).0,
        "welcome"
    );
    let (_, peers, _) = read_snapshot(&mut e).await?;
    assert_eq!(peers.len(), 4);
    drop(e);
    drop(d);
    drop(c);
    drop(a);
    drop(lease);

    // Cap rejection on a tight server (2 offers per peer) leaves the catalog
    // exactly as it was: two acked offers, then LIMIT_EXCEEDED.
    let port2 = support::free_port().await?;
    let mut args2 = support::enabled_args();
    args2.base_url = Some(format!("http://127.0.0.1:{port2}/"));
    args2.max_offers_per_peer = 2;
    let config2 =
        bore_cli::web_transfer::resolve_server_config(&args2, false, port2)?.expect("config2");
    let mut server2 = Server::new(1024..=65535, None);
    server2.set_control_port(port2);
    server2.set_web_transfer(config2)?;
    let registry2 = server2.web_transfer().expect("registry2");
    tokio::spawn(server2.listen());
    support::wait_port(port2, true).await;
    let member2 = MemberToken::from_bytes([0x63u8; 32]);
    let owner2 = OwnerToken::from_bytes([0x64u8; 32]);
    let lease2 = OwnerLease::create(
        &registry2,
        member2.sha256_hash(),
        owner2.sha256_hash(),
        false,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room2 = lease2.id().to_string();
    let host2 = format!("127.0.0.1:{port2}");
    let origin2 = format!("http://127.0.0.1:{port2}");
    let mut p = support::WsPeer::connect(&host2, &room2, &origin2).await?;
    p.hello(&member2.to_string(), None).await?;
    assert_eq!(
        control_msg(&p.next_text(wait).await?.expect("welcome")).0,
        "welcome"
    );
    read_snapshot(&mut p).await?;
    for (rid, offer) in [
        ("11".repeat(16), "11111111111111111111111111111111"),
        ("22".repeat(16), "22222222222222222222222222222222"),
    ] {
        let manifest = catalog_manifest(offer, "Cap", "cap.txt");
        let ack = catalog_publish(&mut p, &rid, offer, &manifest).await?;
        assert_eq!(ack["result"]["offerId"].as_str(), Some(offer));
    }
    p.send_text(
        serde_json::json!({
            "v": 1,
            "type": "offer.publish",
            "requestId": "33".repeat(16),
            "body": {
                "offerId": "33333333333333333333333333333333",
                "manifest": catalog_manifest("33333333333333333333333333333333", "Cap", "cap.txt"),
                "mac": "ee".repeat(32),
            },
        })
        .to_string(),
    )
    .await?;
    let (typ, body) = control_msg(&p.next_text(wait).await?.expect("cap reply"));
    assert_eq!(typ, "error");
    assert_eq!(body["code"].as_str(), Some("LIMIT_EXCEEDED"));
    drop(p);
    drop(lease2);

    // Logs captured around the whole session omit every canary.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let logs = String::from_utf8_lossy(&log_buf.lock().unwrap()).into_owned();
    for canary in [
        label_a, path_a, label_b, path_b, "aardvark", "badger", "canary-",
    ] {
        assert!(
            !logs.contains(canary),
            "log carries {canary:?}: {}",
            &logs[..logs.len().min(500)]
        );
    }
    Ok(())
}

/// Server race coverage for 2.6 (no separate T-WEB ID: the plan's e2e field
/// leaves it unnamed): twenty iterations each of publish-vs-disconnect and
/// publish-vs-close over real control sockets, asserting the catalog and
/// all counters return to baseline whatever order wins.
#[tokio::test]
async fn t_web_offer_races() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(5);

    for round in 0..20u32 {
        let member = MemberToken::from_bytes([(round as u8).wrapping_add(0x70); 32]);
        let owner = OwnerToken::from_bytes([(round as u8).wrapping_add(0xa0); 32]);
        if round % 2 == 0 {
            // Publish races the disconnect: fire both, then converge.
            let lease =
                OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            let room_hex = lease.id().to_string();
            let mut peer = support::WsPeer::connect(&host, &room_hex, &origin).await?;
            peer.hello(&member.to_string(), None).await?;
            assert_eq!(
                control_msg(&peer.next_text(wait).await?.expect("welcome")).0,
                "welcome"
            );
            let offer_hex = format!("{round:032x}");
            peer.send_text(
                serde_json::json!({
                    "v": 1,
                    "type": "offer.publish",
                    "requestId": format!("{round:032x}"),
                    "body": {
                        "offerId": offer_hex,
                        "manifest": catalog_manifest(&offer_hex, "Race", "race.txt"),
                        "mac": "ee".repeat(32),
                    },
                })
                .to_string(),
            )
            .await?;
            drop(peer);
            drop(lease);
            for _ in 0..100 {
                if registry.current_peers() == 0 && registry.current_metadata_bytes() == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(registry.current_peers(), 0, "round {round}");
            assert_eq!(registry.current_metadata_bytes(), 0, "round {round}");
        } else {
            // Publish races the explicit close: the room always ends gone
            // with every counter back to baseline.
            let lease =
                OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            let room = lease.room().clone();
            let room_id = lease.id();
            let mut peer = support::WsPeer::connect(&host, &room.id.to_string(), &origin).await?;
            peer.hello(&member.to_string(), None).await?;
            assert_eq!(
                control_msg(&peer.next_text(wait).await?.expect("welcome")).0,
                "welcome"
            );
            let offer_hex = format!("{round:032x}");
            peer.send_text(
                serde_json::json!({
                    "v": 1,
                    "type": "offer.publish",
                    "requestId": format!("{round:032x}"),
                    "body": {
                        "offerId": offer_hex,
                        "manifest": catalog_manifest(&offer_hex, "Race", "race.txt"),
                        "mac": "ee".repeat(32),
                    },
                })
                .to_string(),
            )
            .await?;
            lease.close_explicit(&registry);
            drop(peer);
            for _ in 0..100 {
                if registry.room(room_id).is_none()
                    && registry.current_peers() == 0
                    && registry.current_metadata_bytes() == 0
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(registry.room(room_id).is_none(), "round {round}");
            assert_eq!(registry.current_peers(), 0, "round {round}");
            assert_eq!(registry.current_metadata_bytes(), 0, "round {round}");
            drop(room);
        }
    }
    Ok(())
}

/// T-WEB-TRANSFER-STATE: real control pair drives the Phase 3.1 state
/// machine end to end — request → incoming → source_ready → per-peer
/// relay tickets → recipient cancel → cancelled on both. No events leak
/// across the pair, tickets differ, terminal record answers repeats.
#[tokio::test]
async fn t_web_transfer_state() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};
    use sha2::{Digest, Sha256};

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x71u8; 32]);
    let owner = OwnerToken::from_bytes([0x72u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(5);

    let mut a = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    a.hello(&token_hex, Some("A")).await?;
    let (_, welcome_a) = control_msg(&a.next_text(wait).await?.expect("welcome A"));
    let peer_a = welcome_a["peerId"].as_str().unwrap().to_string();
    read_snapshot(&mut a).await?;
    let mut b = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    b.hello(&token_hex, Some("B")).await?;
    let (_, welcome_b) = control_msg(&b.next_text(wait).await?.expect("welcome B"));
    let peer_b = welcome_b["peerId"].as_str().unwrap().to_string();
    read_snapshot(&mut b).await?;
    // Drain the join notices both ways.
    let (got, _) = control_msg(&a.next_text(wait).await?.expect("join on A"));
    assert_eq!(got, "peer.joined");

    // A publishes one single-file offer; B learns manifest+mac from the event.
    let offer_hex = "dddddddddddddddddddddddddddddddd";
    let manifest = catalog_manifest(offer_hex, "State", "state.txt");
    catalog_publish(&mut a, &"e".repeat(32), offer_hex, &manifest).await?;
    let (got, added) = control_msg(&b.next_text(wait).await?.expect("added on B"));
    assert_eq!(got, "offer.added");
    assert_eq!(added["offerId"].as_str(), Some(offer_hex));
    let mac_hex = added["mac"].as_str().unwrap().to_string();

    // B computes the selection digest exactly like the server does.
    let selection = serde_json::json!({
        "entryIds": ["0"],
        "manifestMac": mac_hex,
        "mode": "raw",
        "offerId": offer_hex,
    });
    let canonical = bore_cli::web_transfer_protocol::canonical_json(&selection).unwrap();
    let digest_hex = hex::encode(Sha256::digest(canonical.as_bytes()));

    // B requests; B gets the ack, A gets transfer.incoming with the attempt.
    b.send_text(
        serde_json::json!({
            "v": 1,
            "type": "transfer.request",
            "requestId": "f".repeat(32),
            "body": {
                "offerId": offer_hex,
                "entryIds": ["0"],
                "selectionDigest": digest_hex,
                "mode": "raw",
            },
        })
        .to_string(),
    )
    .await?;
    let (got, ack) = control_msg(&b.next_text(wait).await?.expect("request ack"));
    assert_eq!(got, "ack", "request reply was {ack}");
    let transfer_id = ack["result"]["transferId"]
        .as_str()
        .unwrap_or_else(|| panic!("request ack carries transferId: {ack}"))
        .to_string();
    let (got, incoming) = control_msg(&a.next_text(wait).await?.expect("incoming"));
    assert_eq!(got, "transfer.incoming");
    assert_eq!(incoming["transferId"].as_str(), Some(transfer_id.as_str()));
    assert_eq!(incoming["offerId"].as_str(), Some(offer_hex));
    assert_eq!(incoming["fromPeerId"].as_str(), Some(peer_b.as_str()));
    let attempt_id = incoming["attemptId"].as_str().unwrap().to_string();

    // A readies with the same digest; both sides get their own ticket.
    a.send_text(
        serde_json::json!({
            "v": 1,
            "type": "transfer.source_ready",
            "requestId": "a0".repeat(16),
            "body": {
                "transferId": transfer_id,
                "attemptId": attempt_id,
                "selectionDigest": digest_hex,
            },
        })
        .to_string(),
    )
    .await?;
    let (got, _) = control_msg(&a.next_text(wait).await?.expect("ready ack"));
    assert_eq!(got, "ack");
    // Phase 4: the ready opens the DIRECT attempt. Declining it is what puts
    // this transfer on the relay, with a fresh attempt ID.
    support::decline_direct(
        &mut a,
        &mut b,
        &transfer_id,
        &attempt_id,
        &"a1".repeat(16),
        wait,
    )
    .await?;
    let (got, ticket_a) = control_msg(&a.next_text(wait).await?.expect("ticket A"));
    assert_eq!(got, "transfer.relay_ticket");
    assert_eq!(ticket_a["transferId"].as_str(), Some(transfer_id.as_str()));
    let relay_attempt = ticket_a["attemptId"].as_str().unwrap().to_string();
    assert_ne!(
        relay_attempt, attempt_id,
        "the fallback mints a new attempt"
    );
    let attempt_id = relay_attempt;
    let (got, ticket_b) = control_msg(&b.next_text(wait).await?.expect("ticket B"));
    assert_eq!(got, "transfer.relay_ticket");
    assert_eq!(ticket_b["transferId"].as_str(), Some(transfer_id.as_str()));
    assert_eq!(ticket_b["attemptId"].as_str(), Some(attempt_id.as_str()));
    assert_ne!(
        ticket_a["ticket"].as_str(),
        ticket_b["ticket"].as_str(),
        "each peer gets only its own ticket"
    );

    // B cancels; both hear transfer.cancelled naming B.
    b.send_text(
        serde_json::json!({
            "v": 1,
            "type": "transfer.cancel",
            "requestId": "b1".repeat(16),
            "body": {"transferId": transfer_id},
        })
        .to_string(),
    )
    .await?;
    let (got, _) = control_msg(&b.next_text(wait).await?.expect("cancel ack"));
    assert_eq!(got, "ack");
    let (got, cancelled_a) = control_msg(&a.next_text(wait).await?.expect("cancelled A"));
    assert_eq!(got, "transfer.cancelled");
    assert_eq!(
        cancelled_a["transferId"].as_str(),
        Some(transfer_id.as_str())
    );
    assert_eq!(cancelled_a["byPeerId"].as_str(), Some(peer_b.as_str()));
    // The canceller heard its ack; no second notice is sent its way.
    assert_ne!(peer_a, peer_b);
    drop(lease);
    Ok(())
}
/// Builds one pump-valid encrypted-frame image (header checked, body opaque
/// filler chosen by the caller).
fn relay_test_frame(seq: u32, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(16 + body.len());
    frame.extend_from_slice(&0x42575431u32.to_be_bytes());
    frame.extend_from_slice(&1u16.to_be_bytes());
    frame.push(1);
    frame.push(0);
    frame.extend_from_slice(&seq.to_be_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body);
    frame
}

/// Reads this process RSS in KiB (`None` off Linux).
fn self_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

/// T-WEB-RELAY-OPAQUE: two relay legs move 64 MiB of ciphertext with an
/// exact end-to-end hash, bounded RSS, immediate cancel and rejected
/// replay/oversize/violation attaches.
#[tokio::test]
async fn t_web_relay_opaque() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};
    use sha2::{Digest, Sha256};

    // Server logs are captured for the canary assertion (serial suite).
    let log_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = CatalogLogSink(std::sync::Arc::clone(&log_buf));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || sink.clone())
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    let _dispatcher_guard = tracing::dispatcher::set_default(&dispatch);

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x81u8; 32]);
    let owner = OwnerToken::from_bytes([0x82u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(15);

    let mut a = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    a.hello(&token_hex, Some("A")).await?;
    let (_, welcome_a) = control_msg(&a.next_text(wait).await?.expect("welcome A"));
    let peer_a = welcome_a["peerId"].as_str().unwrap().to_string();
    read_snapshot(&mut a).await?;
    let mut b = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    b.hello(&token_hex, Some("B")).await?;
    let (_, welcome_b) = control_msg(&b.next_text(wait).await?.expect("welcome B"));
    let peer_b = welcome_b["peerId"].as_str().unwrap().to_string();
    read_snapshot(&mut b).await?;
    let (got, _) = control_msg(&a.next_text(wait).await?.expect("join on A"));
    assert_eq!(got, "peer.joined");

    let offer_hex = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let manifest = catalog_manifest(offer_hex, "Opaque", "opaque.bin");
    catalog_publish(&mut a, &"e".repeat(32), offer_hex, &manifest).await?;
    let (got, added) = control_msg(&b.next_text(wait).await?.expect("added on B"));
    assert_eq!(got, "offer.added");
    let mac_hex = added["mac"].as_str().unwrap().to_string();
    let selection = serde_json::json!({
        "entryIds": ["0"],
        "manifestMac": mac_hex,
        "mode": "raw",
        "offerId": offer_hex,
    });
    let digest_hex = hex::encode(Sha256::digest(
        bore_cli::web_transfer_protocol::canonical_json(&selection)
            .unwrap()
            .as_bytes(),
    ));

    // One request→ready cycle per transfer under test; returns
    // (transfer, attempt, source ticket, recipient ticket).
    let mut request_n = 0u32;
    macro_rules! setup_transfer {
        () => {{
            request_n += 1;
            let rid = format!("{:032x}", request_n);
            b.send_text(
                serde_json::json!({
                    "v": 1,
                    "type": "transfer.request",
                    "requestId": rid,
                    "body": {
                        "offerId": offer_hex,
                        "entryIds": ["0"],
                        "selectionDigest": digest_hex,
                        "mode": "raw",
                    },
                })
                .to_string(),
            )
            .await?;
            let (got, ack) = control_msg(&b.next_text(wait).await?.expect("request ack"));
            assert_eq!(got, "ack");
            let transfer_id = ack["result"]["transferId"].as_str().unwrap().to_string();
            let (got, incoming) = control_msg(&a.next_text(wait).await?.expect("incoming"));
            assert_eq!(got, "transfer.incoming");
            let attempt_id = incoming["attemptId"].as_str().unwrap().to_string();
            a.send_text(
                serde_json::json!({
                    "v": 1,
                    "type": "transfer.source_ready",
                    "requestId": format!("a{:031x}", request_n),
                    "body": {
                        "transferId": transfer_id,
                        "attemptId": attempt_id,
                        "selectionDigest": digest_hex,
                    },
                })
                .to_string(),
            )
            .await?;
            let (got, _) = control_msg(&a.next_text(wait).await?.expect("ready ack"));
            assert_eq!(got, "ack");
            // Phase 4: decline the direct attempt to reach the relay. The
            // tickets are bound to the FRESH attempt the fallback minted, so
            // the attach below must use that one, not the request's.
            support::decline_direct(
                &mut a,
                &mut b,
                &transfer_id,
                &attempt_id,
                &format!("d{:031x}", request_n),
                wait,
            )
            .await?;
            let (got, ticket_a) = control_msg(&a.next_text(wait).await?.expect("ticket A"));
            assert_eq!(got, "transfer.relay_ticket");
            let (got, ticket_b) = control_msg(&b.next_text(wait).await?.expect("ticket B"));
            assert_eq!(got, "transfer.relay_ticket");
            let relay_attempt = ticket_a["attemptId"].as_str().unwrap().to_string();
            assert_ne!(relay_attempt, attempt_id);
            anyhow::Result::<_>::Ok((
                transfer_id,
                relay_attempt,
                ticket_a["ticket"].as_str().unwrap().to_string(),
                ticket_b["ticket"].as_str().unwrap().to_string(),
            ))
        }};
    }
    macro_rules! attach_leg {
        ($transfer:expr, $peer:expr, $attempt:expr, $role:expr, $ticket:expr) => {{
            let mut leg =
                support::RelayLeg::connect(&host, &room_hex, $transfer, &origin).await?;
            leg.send_text(
                serde_json::json!({
                    "v": 1,
                    "peerId": $peer,
                    "transferId": $transfer,
                    "attemptId": $attempt,
                    "role": $role,
                    "ticket": $ticket,
                })
                .to_string(),
            )
            .await?;
            leg
        }};
    }
    // A relay socket is dead when it yields Close or goes silent.
    macro_rules! assert_leg_dead {
        ($leg:expr) => {{
            match $leg.next_msg(Duration::from_secs(5)).await? {
                Some(tokio_tungstenite::tungstenite::Message::Close(_)) | None => {}
                Some(other) => anyhow::bail!("live leg yielded {other:?}"),
            }
        }};
    }
    macro_rules! drain_commit {
        ($peer:expr) => {{
            let (got, commit) = control_msg(&$peer.next_text(wait).await?.expect("path commit"));
            assert_eq!(got, "transfer.path_commit");
            commit
        }};
    }

    // --- T1: 64 MiB fidelity, exact hash, bounded RSS -------------------------
    const FRAME_BODY: usize = 32752;
    const FRAME_COUNT: usize = 2048;
    let (t1, a1, src_t1, rcpt_t1) = setup_transfer!()?;
    let rss_before = self_rss_kib();
    let mut leg_a = attach_leg!(&t1, &peer_a, &a1, "source", &src_t1);
    let mut leg_b = attach_leg!(&t1, &peer_b, &a1, "recipient", &rcpt_t1);
    let commit_a = drain_commit!(a);
    let commit_b = drain_commit!(b);
    assert_eq!(commit_a["transferId"].as_str(), Some(t1.as_str()));
    assert_eq!(commit_b["attemptId"].as_str(), Some(a1.as_str()));
    // Send and receive concurrently: the relay holds at most one frame,
    // so a send-then-drain sequence deadlocks on full socket buffers.
    let sender = tokio::spawn(async move {
        let mut payload = vec![0u8; FRAME_BODY];
        let mut sent_hash = Sha256::new();
        for seq in 0..FRAME_COUNT as u32 {
            // Deterministic pattern + canary in the first frame's ciphertext.
            payload.fill((seq % 251) as u8);
            if seq == 0 {
                payload[..19].copy_from_slice(b"CANARY-RELAY-abcdef");
            }
            let frame = relay_test_frame(seq, &payload);
            sent_hash.update(&frame);
            leg_a.send_binary(frame).await?;
        }
        leg_a.close().await?;
        anyhow::Result::<_>::Ok(hex::encode(sent_hash.finalize()))
    });
    let mut received_hash = Sha256::new();
    let mut received_frames = 0usize;
    loop {
        match leg_b.next_msg(wait).await? {
            Some(tokio_tungstenite::tungstenite::Message::Binary(frame)) => {
                received_hash.update(&frame);
                received_frames += 1;
            }
            Some(tokio_tungstenite::tungstenite::Message::Close(_)) | None => break,
            Some(_) => anyhow::bail!("unexpected relay message"),
        }
    }
    let sent_digest = sender.await??;
    assert_eq!(received_frames, FRAME_COUNT);
    assert_eq!(hex::encode(received_hash.finalize()), sent_digest);
    // The clean pump drains the close echoes (up to the close grace) before
    // releasing its permit: poll the live gauge instead of asserting it.
    for _ in 0..100 {
        if registry.current_relays() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        registry.current_relays(),
        0,
        "clean pump released its permit"
    );
    if let (Some(before), Some(after)) = (rss_before, self_rss_kib()) {
        assert!(
            after < before + 10 * 1024,
            "RSS grew {before} -> {after} KiB across 64 MiB relayed"
        );
    }
    // Close T1 explicitly: the next request must mint a new transfer instead
    // of idempotently re-acking this live selection.
    b.send_text(
        serde_json::json!({
            "v": 1,
            "type": "transfer.cancel",
            "requestId": "d".repeat(32),
            "body": {"transferId": t1},
        })
        .to_string(),
    )
    .await?;
    let (got, _) = control_msg(&b.next_text(wait).await?.expect("t1 cancel ack"));
    assert_eq!(got, "ack");
    let (got, cancelled) = control_msg(&a.next_text(wait).await?.expect("t1 cancelled"));
    assert_eq!(got, "transfer.cancelled");
    assert_eq!(cancelled["transferId"].as_str(), Some(t1.as_str()));

    // --- T2: spent tickets replay closed, no control notice -------------------
    let mut leg_a2 = attach_leg!(&t1, &peer_a, &a1, "source", &src_t1);
    let mut leg_b2 = attach_leg!(&t1, &peer_b, &a1, "recipient", &rcpt_t1);
    assert!(leg_a2.next_msg(Duration::from_secs(3)).await?.is_none());
    assert!(leg_b2.next_msg(Duration::from_secs(3)).await?.is_none());
    assert!(a.next_text(Duration::from_millis(300)).await.is_err());

    // --- T3: binary before attach closes --------------------------------------
    let (t3, _a3, _st3, _rt3) = setup_transfer!()?;
    let mut leg = support::RelayLeg::connect(&host, &room_hex, &t3, &origin).await?;
    leg.send_binary(vec![0u8; 64]).await?;
    assert!(leg.next_msg(Duration::from_secs(3)).await?.is_none());
    b.send_text(
        serde_json::json!({
            "v": 1,
            "type": "transfer.cancel",
            "requestId": "d3".repeat(16),
            "body": {"transferId": t3},
        })
        .to_string(),
    )
    .await?;
    let (got, _) = control_msg(&b.next_text(wait).await?.expect("t3 cancel ack"));
    assert_eq!(got, "ack");
    let (got, cancelled) = control_msg(&a.next_text(wait).await?.expect("t3 cancelled"));
    assert_eq!(got, "transfer.cancelled");
    assert_eq!(cancelled["transferId"].as_str(), Some(t3.as_str()));

    // --- T4: oversize source binary fails the attempt --------------------------
    let (t4, _a4, src_t4, rcpt_t4) = setup_transfer!()?;
    // Peer IDs for the fresh attempt come from the same control pair.
    let mut leg_a4 = attach_leg!(&t4, &peer_a, &_a4, "source", &src_t4);
    let mut leg_b4 = attach_leg!(&t4, &peer_b, &_a4, "recipient", &rcpt_t4);
    drain_commit!(a);
    drain_commit!(b);
    leg_a4.send_binary(vec![0u8; 40960]).await?;
    assert_leg_dead!(leg_b4);
    let (got, body) = control_msg(&b.next_text(wait).await?.expect("attach failure"));
    assert_eq!(got, "error");
    assert_eq!(body["code"].as_str(), Some("DIRECT_FAILED"));
    let (got, body) = control_msg(&a.next_text(wait).await?.expect("attach failure A"));
    assert_eq!(got, "error");
    assert_eq!(body["code"].as_str(), Some("DIRECT_FAILED"));
    assert_eq!(registry.current_relays(), 0);

    // --- T5: recipient payload is a violation ----------------------------------
    let (t5, _a5, src_t5, rcpt_t5) = setup_transfer!()?;
    let mut leg_a5 = attach_leg!(&t5, &peer_a, &_a5, "source", &src_t5);
    let mut leg_b5 = attach_leg!(&t5, &peer_b, &_a5, "recipient", &rcpt_t5);
    drain_commit!(a);
    drain_commit!(b);
    leg_b5.send_binary(vec![0u8; 64]).await?;
    assert_leg_dead!(leg_a5);
    assert_leg_dead!(leg_b5);
    for peer in [&mut a, &mut b] {
        let (got, body) = control_msg(&peer.next_text(wait).await?.expect("violation"));
        assert_eq!(got, "error");
        assert_eq!(body["code"].as_str(), Some("DIRECT_FAILED"));
    }

    // --- T6: control cancel closes both legs promptly ---------------------------
    let (t6, _a6, src_t6, rcpt_t6) = setup_transfer!()?;
    let mut leg_a6 = attach_leg!(&t6, &peer_a, &_a6, "source", &src_t6);
    let mut leg_b6 = attach_leg!(&t6, &peer_b, &_a6, "recipient", &rcpt_t6);
    drain_commit!(a);
    drain_commit!(b);
    let mut payload = vec![0u8; FRAME_BODY];
    for seq in 0..16u32 {
        payload.fill(seq as u8);
        leg_a6.send_binary(relay_test_frame(seq, &payload)).await?;
    }
    b.send_text(
        serde_json::json!({
            "v": 1,
            "type": "transfer.cancel",
            "requestId": "c".repeat(32),
            "body": {"transferId": t6},
        })
        .to_string(),
    )
    .await?;
    let (got, _) = control_msg(&b.next_text(wait).await?.expect("cancel ack"));
    assert_eq!(got, "ack");
    // In-flight frames forwarded before the cancel arrived still drain
    // first; afterwards both legs yield Close or silence.
    for leg in [&mut leg_a6, &mut leg_b6] {
        let mut binaries = 0usize;
        loop {
            match leg.next_msg(Duration::from_secs(5)).await? {
                Some(tokio_tungstenite::tungstenite::Message::Binary(_)) => {
                    binaries += 1;
                    assert!(binaries <= 16, "no post-cancel payload");
                }
                Some(tokio_tungstenite::tungstenite::Message::Close(_)) | None => break,
                Some(other) => anyhow::bail!("live leg yielded {other:?}"),
            }
        }
    }
    let (got, cancelled) = control_msg(&a.next_text(wait).await?.expect("cancelled A"));
    assert_eq!(got, "transfer.cancelled");
    assert_eq!(cancelled["byPeerId"].as_str(), Some(peer_b.as_str()));
    assert_eq!(registry.current_transfers(), 0);
    assert_eq!(registry.current_relays(), 0);

    // Server logs never carried the ciphertext canary (nor any payload).
    tokio::time::sleep(Duration::from_millis(200)).await;
    let logs = String::from_utf8_lossy(&log_buf.lock().unwrap()).into_owned();
    assert!(!logs.contains("CANARY-RELAY"), "payload leaked to logs");
    drop(lease);
    Ok(())
}

// ---------------------------------------------------------------------------
// T-WEB-CLI (3.6)
// ---------------------------------------------------------------------------

/// Sends one signal by pid through the coreutils `kill` binary (the ssh suite's
/// precedent: no libc dependency in the test crate).
#[cfg(unix)]
fn signal_pid(pid: u32, sig: &str) -> Result<()> {
    let status = std::process::Command::new("kill")
        .arg(sig)
        .arg(pid.to_string())
        .status()?;
    anyhow::ensure!(status.success(), "kill {sig} {pid} failed");
    Ok(())
}

/// Reads a text file the way this suite compares it: with `\r\n` folded to
/// `\n`. The windows-latest runner checks the repository out with
/// `core.autocrlf=true`, so every file in the tree arrives CRLF there — and a
/// needle that spans a newline (`"...files or\n> a whole folder..."`) then
/// matches on Linux and macOS and can never match on Windows. The defect is
/// the harness reading a file in one encoding and quoting it in another, not
/// the documentation.
fn read_doc_text(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    Ok(std::fs::read_to_string(path)?.replace("\r\n", "\n"))
}

/// Splits a short room URL into `(room_id_hex, member_token_hex)`. The
/// fragment carries only the seed; credentials are derived independently for
/// the real control probe and never sent to the server as URL material.
fn split_room_url(url: &str) -> Result<(String, String)> {
    let material = room_link_material(url)?;
    Ok((
        material.room_id.to_string(),
        material.member_token.to_string(),
    ))
}

fn room_link_material(url: &str) -> Result<RoomLinkMaterial> {
    let (path, seed_text) = url.split_once('#').context("room URL has no fragment")?;
    let (_, transfer_tail) = path
        .rsplit_once("/transfer/")
        .context("room URL has no /transfer/ path")?;
    anyhow::ensure!(
        transfer_tail.is_empty(),
        "room URL has a non-canonical path"
    );
    let seed = decode_room_link_seed(seed_text)?;
    Ok(derive_room_link_material(&seed))
}

/// `true` when the room still answers a real control hello with a welcome.
/// A destroyed room either refuses the handshake or closes without one.
#[cfg(unix)]
async fn room_alive(host: &str, room: &str, origin: &str, token: &str) -> bool {
    let Ok(mut peer) = support::WsPeer::connect(host, room, origin).await else {
        return false;
    };
    if peer.hello(token, None).await.is_err() {
        return false;
    }
    match peer.next_text(Duration::from_secs(3)).await {
        Ok(Some(text)) => text.contains("\"welcome\""),
        _ => false,
    }
}

/// Reads one stdout line within `wait`.
async fn next_line<R>(lines: &mut tokio::io::Lines<R>, wait: Duration) -> Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    Ok(tokio::time::timeout(wait, lines.next_line()).await??)
}

/// T-WEB-CLI: the real `bore transfer web` process against a real `bore
/// server` process. Prints exactly the two documented lines, opens the
/// browser through an injected opener AFTER them, keeps the same URL across a
/// forced control reset (resume, never a second room), and destroys the room
/// on Ctrl+C, SIGTERM and SIGHUP alike.
///
/// Unix-only, on `t_web_room_life`'s precedent: three of its four legs ARE
/// signals, and a build without them would not sit those legs out — it would
/// spawn the process, never close it, and wait out a ten-second join before
/// failing. A test that cannot perform its own subject is skipped, not
/// weakened. It is also what made the binding below unused on windows-latest
/// and failed `-D warnings` there.
#[cfg(unix)]
#[tokio::test]
async fn t_web_cli() -> Result<()> {
    let binary = std::env::var_os("CARGO_BIN_EXE_bore")
        .context("CARGO_BIN_EXE_bore is not available for web-transfer CLI tests")?;
    let port = support::free_port().await?;
    let mut server = tokio::process::Command::new(&binary)
        .arg("server")
        .arg("--control-port")
        .arg(port.to_string())
        .arg("--web-transfer-base-url")
        .arg(format!("http://127.0.0.1:{port}/"))
        .arg("--web-transfer-owner-grace")
        .arg("5")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawning the web-transfer server")?;
    support::wait_port(port, true).await;
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(20);

    // An injected opener: `BROWSER` is what the launcher consults first, so
    // the real code path runs and the URL it handed over is recorded.
    let marker = std::env::temp_dir().join(format!("bore-web-open-{port}.txt"));
    let opener = std::env::temp_dir().join(format!("bore-web-open-{port}.sh"));
    let _ = std::fs::remove_file(&marker);
    std::fs::write(
        &opener,
        format!("#!/bin/sh\nprintf '%s' \"$1\" > {}\n", marker.display()),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755))?;
    }

    // Leg 1 — Ctrl+C, through a killable proxy so the same child also proves
    // the resume path, with `--open` proving the browser hand-off.
    let proxy = support::spawn_proxy(0, port).await?;
    let proxy_port = proxy.port();
    let mut child = tokio::process::Command::new(&binary)
        .arg("transfer")
        .arg("web")
        .arg("--to")
        .arg(format!("127.0.0.1:{proxy_port}"))
        .arg("--open")
        .env("BROWSER", &opener)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawning bore transfer web")?;
    let pid = child.id().context("child pid")?;
    let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(
        child.stdout.take().context("child stdout")?,
    ));

    let first = next_line(&mut lines, wait).await?.context("room line")?;
    let second = next_line(&mut lines, wait).await?.context("active line")?;
    let url = first
        .strip_prefix("room: ")
        .context("first line is `room: <url>`")?
        .to_string();
    assert_eq!(second, "room active; press Ctrl+C to close");
    assert!(
        url.starts_with(&format!("http://127.0.0.1:{port}/transfer/")),
        "URL {url} must be the server's own origin"
    );
    let (room, token) = split_room_url(&url)?;
    assert!(room_alive(&host, &room, &origin, &token).await, "room live");

    // `--open` handed the same URL to the launcher, after the two lines. The
    // `BROWSER` hook exists only where `webbrowser` consults it: macOS hands
    // the URL to `open` and never reads the variable, so on that platform
    // there is no launcher to observe — and inventing one would test the
    // harness, not the product.
    if cfg!(target_os = "linux") {
        let mut opened = String::new();
        // 600 x 50 ms = 30 s. Un budget in millisecondi descrive la MACCHINA
        // (V-9): sotto il parallelismo pieno della CI il lancio del finto
        // browser puo' arrivare tardi, e un harness lento non deve poter
        // dichiarare che il prodotto ha annunciato l'URL sbagliato.
        for _ in 0..600 {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                if !text.is_empty() {
                    opened = text;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(opened, url, "the browser got exactly the announced URL");
    } else {
        println!("N/A the --open launcher hook needs a platform where webbrowser reads $BROWSER");
    }

    // Forced native control reset: the owner resumes the SAME room and says
    // nothing more on stdout (no second URL, no replacement room).
    proxy.kill().await;
    let _proxy = support::spawn_proxy(proxy_port, port).await?;
    let mut resumed = false;
    // 300 x 100 ms = 30 s, stessa ragione: la grazia del server e' la proprieta'
    // sotto esame, l'attesa dell'harness non deve esserlo.
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if room_alive(&host, &room, &origin, &token).await {
            resumed = true;
            break;
        }
    }
    assert!(
        resumed,
        "the room survives a control reset inside the grace"
    );
    assert!(
        next_line(&mut lines, Duration::from_millis(500))
            .await
            .is_err(),
        "a resume prints nothing: the URL never changes"
    );

    // Ctrl+C: one signal closes the room and the process exits successfully.
    signal_pid(pid, "-INT")?;
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .context("the first signal must end the process")??;
    assert!(status.success(), "a clean close exits zero, got {status}");
    assert!(
        !room_alive(&host, &room, &origin, &token).await,
        "Ctrl+C destroys the room immediately"
    );

    // Legs 2 and 3 — SIGTERM and SIGHUP (a closed shell) do the same.
    for sig in ["-TERM", "-HUP"] {
        let mut child = tokio::process::Command::new(&binary)
            .arg("transfer")
            .arg("web")
            .arg("--to")
            .arg(&host)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id().context("child pid")?;
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(
            child.stdout.take().context("child stdout")?,
        ));
        let first = next_line(&mut lines, wait).await?.context("room line")?;
        let second = next_line(&mut lines, wait).await?.context("active line")?;
        assert_eq!(second, "room active; press Ctrl+C to close");
        let url = first
            .strip_prefix("room: ")
            .context("room line")?
            .to_string();
        let (room, token) = split_room_url(&url)?;
        assert!(
            room_alive(&host, &room, &origin, &token).await,
            "{sig} room live"
        );
        signal_pid(pid, sig)?;
        let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
            .await
            .context("signal must end the process")??;
        assert!(status.success(), "{sig} exits zero, got {status}");
        assert!(
            !room_alive(&host, &room, &origin, &token).await,
            "{sig} destroys the room immediately"
        );
    }

    // A server without the feature is an actionable message, not a wire dump.
    let plain_port = support::free_port().await?;
    let mut plain = tokio::process::Command::new(&binary)
        .arg("server")
        .arg("--control-port")
        .arg(plain_port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    support::wait_port(plain_port, true).await;
    let out = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(&binary)
            .arg("transfer")
            .arg("web")
            .arg("--to")
            .arg(format!("127.0.0.1:{plain_port}"))
            .stdin(Stdio::null())
            .output(),
    )
    .await??;
    assert!(!out.status.success(), "a server without the feature fails");
    assert!(
        out.stdout.is_empty(),
        "no room line for a room that was never created"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "web transfer requires an upgraded server configured with --web-transfer-base-url"
        ),
        "unexpected failure text: {stderr}"
    );
    let _ = plain.kill().await;

    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&opener);
    let _ = server.kill().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// T-WEB-PERF (3.9) — the `pipe` arm: what the server's opaque relay can carry
// ---------------------------------------------------------------------------

/// The reporting helpers are pure, so they are pinned here and not only
/// exercised by the (ignored) bench: a rate with no time behind it must be
/// absent, never `0.0`, or a failed arm enters a median as a number.
#[test]
fn throughput_is_bytes_over_elapsed_and_refuses_zero_time() {
    let mib = 1024u64 * 1024;
    let rate = support::throughput_mib_s(64 * mib, Duration::from_secs(2)).expect("measured");
    assert!(
        (rate - 32.0).abs() < 1e-9,
        "64 MiB in 2 s is 32 MiB/s, got {rate}"
    );
    assert_eq!(support::throughput_mib_s(0, Duration::from_secs(1)), None);
    assert_eq!(support::throughput_mib_s(mib, Duration::ZERO), None);
}

/// A summary that hides its samples cannot be re-checked, and the bug that
/// corrupts a median (a locale-sorted comparison) is invisible in one.
#[test]
fn bench_reports_every_sample_not_only_the_median() {
    let line = support::bench_line("pipe", 64, &[397.46, 264.01, 408.0]);
    assert!(line.contains("median=397.46MiB/s"), "{line}");
    for sample in ["397.46", "264.01", "408.00"] {
        assert!(line.contains(sample), "sample {sample} missing from {line}");
    }
    // An arm that produced nothing says so; it never contributes a 0.
    let empty = support::bench_line("pipe", 64, &[]);
    assert!(empty.contains("median=FAILED"), "{empty}");
    assert_eq!(support::median(&[]), None);
    assert_eq!(support::median(&[2.0, 1.0, 3.0]), Some(2.0));
    assert_eq!(support::median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
}

/// T-WEB-PERF `pipe` arm: N MiB through the real opaque relay with the room
/// throttle disabled, so the number is the server's own ceiling and not the
/// token bucket. Ignored by default (a benchmark is not a gate); the driver
/// `scripts/perf/web_transfer_bench.sh` runs it with `--ignored --nocapture`.
///
/// Sizes and repetitions come from the environment so the same binary serves
/// a quick loopback check and a real campaign:
/// `BORE_PERF_SIZES_MIB=16,64 BORE_PERF_REPS=3`.
#[tokio::test]
#[ignore = "benchmark: run through scripts/perf/web_transfer_bench.sh"]
async fn t_web_perf_relay() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};
    use sha2::{Digest, Sha256};

    let sizes: Vec<u64> = std::env::var("BORE_PERF_SIZES_MIB")
        .unwrap_or_else(|_| "16,64".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .collect();
    let reps: usize = std::env::var("BORE_PERF_REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    anyhow::ensure!(!sizes.is_empty(), "BORE_PERF_SIZES_MIB parsed to nothing");

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    // The throttle is a product feature, not the thing under test: measuring
    // through it measures the bucket (default 100 MiB/s).
    args.relay_rate_bytes_per_s = 0;
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x91u8; 32]);
    let owner = OwnerToken::from_bytes([0x92u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(120);

    let mut a = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    a.hello(&token_hex, Some("A")).await?;
    let (_, welcome_a) = control_msg(&a.next_text(wait).await?.expect("welcome A"));
    let peer_a = welcome_a["peerId"].as_str().unwrap().to_string();
    read_snapshot(&mut a).await?;
    let mut b = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    b.hello(&token_hex, Some("B")).await?;
    let (_, welcome_b) = control_msg(&b.next_text(wait).await?.expect("welcome B"));
    let peer_b = welcome_b["peerId"].as_str().unwrap().to_string();
    read_snapshot(&mut b).await?;
    let (got, _) = control_msg(&a.next_text(wait).await?.expect("join on A"));
    assert_eq!(got, "peer.joined");

    let offer_hex = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let manifest = catalog_manifest(offer_hex, "Bench", "bench.bin");
    catalog_publish(&mut a, &"b".repeat(32), offer_hex, &manifest).await?;
    let (got, added) = control_msg(&b.next_text(wait).await?.expect("added on B"));
    assert_eq!(got, "offer.added");
    let mac_hex = added["mac"].as_str().unwrap().to_string();
    let selection = serde_json::json!({
        "entryIds": ["0"],
        "manifestMac": mac_hex,
        "mode": "raw",
        "offerId": offer_hex,
    });
    let digest_hex = hex::encode(Sha256::digest(
        bore_cli::web_transfer_protocol::canonical_json(&selection)
            .unwrap()
            .as_bytes(),
    ));

    // Frame shape identical to the product's: 24 KiB plaintext + 16-byte
    // header + 16-byte tag, so the per-frame overhead is the real one.
    const FRAME_BODY: usize = 24 * 1024 + 16;
    let mut request_n = 0u32;
    // Every control read in this bench skips whatever the previous
    // repetition left queued (terminal events arrive on both peers) and
    // stops on the message this step actually waits for. Asserting on
    // "whatever arrived first" turns an ordering detail into a failed run.
    macro_rules! wait_for {
        ($peer:expr, $ty:expr) => {{
            loop {
                let text = $peer
                    .next_text(wait)
                    .await?
                    .with_context(|| format!("control closed before {}", $ty))?;
                let (got, body) = control_msg(&text);
                if got == $ty {
                    break body;
                }
                // A protocol error is never "noise to skip": the control
                // plane refusing the bench IS the result.
                anyhow::ensure!(
                    got != "error",
                    "control error while waiting for {}: {}",
                    $ty,
                    body
                );
                println!("PERF note: skipped {got} while waiting for {}", $ty);
            }
        }};
    }
    println!("PERF host=pipe (server opaque relay, throttle off, loopback)");
    for size_mib in &sizes {
        let total = size_mib * 1024 * 1024;
        let frames = (total as usize).div_ceil(FRAME_BODY);
        let mut samples = Vec::new();
        for _ in 0..reps {
            request_n += 1;
            let rid = format!("{request_n:032x}");
            b.send_text(
                serde_json::json!({
                    "v": 1, "type": "transfer.request", "requestId": rid,
                    "body": { "offerId": offer_hex, "entryIds": ["0"],
                              "selectionDigest": digest_hex, "mode": "raw" },
                })
                .to_string(),
            )
            .await?;
            let ack = wait_for!(b, "ack");
            let transfer_id = ack["result"]["transferId"].as_str().unwrap().to_string();
            let incoming = wait_for!(a, "transfer.incoming");
            let attempt_id = incoming["attemptId"].as_str().unwrap().to_string();
            a.send_text(
                serde_json::json!({
                    "v": 1, "type": "transfer.source_ready",
                    "requestId": format!("a{request_n:031x}"),
                    "body": { "transferId": transfer_id, "attemptId": attempt_id,
                              "selectionDigest": digest_hex },
                })
                .to_string(),
            )
            .await?;
            let _ = wait_for!(a, "ack");
            // Phase 4: the relay arm is reached by declining the direct
            // attempt, and the tickets name the fresh attempt.
            support::decline_direct(
                &mut a,
                &mut b,
                &transfer_id,
                &attempt_id,
                &format!("d{request_n:031x}"),
                wait,
            )
            .await?;
            let ticket_a = wait_for!(a, "transfer.relay_ticket");
            let ticket_b = wait_for!(b, "transfer.relay_ticket");
            let attempt_id = ticket_a["attemptId"].as_str().unwrap().to_string();

            let mut leg_a =
                support::RelayLeg::connect(&host, &room_hex, &transfer_id, &origin).await?;
            leg_a
                .send_text(
                    serde_json::json!({
                        "v": 1, "peerId": peer_a, "transferId": transfer_id,
                        "attemptId": attempt_id, "role": "source",
                        "ticket": ticket_a["ticket"].as_str().unwrap(),
                    })
                    .to_string(),
                )
                .await?;
            let mut leg_b =
                support::RelayLeg::connect(&host, &room_hex, &transfer_id, &origin).await?;
            leg_b
                .send_text(
                    serde_json::json!({
                        "v": 1, "peerId": peer_b, "transferId": transfer_id,
                        "attemptId": attempt_id, "role": "recipient",
                        "ticket": ticket_b["ticket"].as_str().unwrap(),
                    })
                    .to_string(),
                )
                .await?;
            let _ = wait_for!(a, "transfer.path_commit");
            let _ = wait_for!(b, "transfer.path_commit");

            // The clock starts at the first byte on the wire and stops when
            // the last one has been READ by the other leg: a relay that
            // buffered would otherwise look infinitely fast.
            let started = std::time::Instant::now();
            let sender = tokio::spawn(async move {
                let payload = vec![0x5au8; FRAME_BODY - 16];
                for seq in 0..frames as u32 {
                    leg_a.send_binary(relay_test_frame(seq, &payload)).await?;
                }
                leg_a.close().await?;
                anyhow::Result::<_>::Ok(())
            });
            let mut received = 0u64;
            loop {
                match leg_b.next_msg(wait).await? {
                    Some(tokio_tungstenite::tungstenite::Message::Binary(frame)) => {
                        received += frame.len() as u64;
                    }
                    Some(tokio_tungstenite::tungstenite::Message::Close(_)) | None => break,
                    Some(_) => anyhow::bail!("unexpected relay message"),
                }
            }
            let elapsed = started.elapsed();
            sender.await??;
            // An arm that delivered nothing is a FAILURE, not a slow sample.
            anyhow::ensure!(
                received >= total,
                "relay delivered {received} of {total} bytes"
            );
            if let Some(rate) = support::throughput_mib_s(received, elapsed) {
                samples.push(rate);
            }
            // Close this transfer before the next repetition: a second
            // request for the SAME live selection is idempotently re-acked
            // with the same transfer, so the source would never be told to
            // start and the repetition would measure nothing.
            b.send_text(
                serde_json::json!({
                    "v": 1, "type": "transfer.cancel",
                    "requestId": format!("c{request_n:031x}"),
                    "body": {"transferId": transfer_id},
                })
                .to_string(),
            )
            .await?;
            let _ = wait_for!(b, "ack");
            // Pace the CONTROL plane between repetitions, outside every
            // measured window: one repetition spends two mutations
            // (request + cancel) against a 4/s bucket with a burst of 8, so
            // an unpaced loop is refused as RATE_LIMITED after four of them
            // — the peer is then dropped and the run reports a transport
            // failure for what is really a client pacing bug.
            tokio::time::sleep(Duration::from_millis(1100)).await;
        }
        println!("{}", support::bench_line("pipe", *size_mib, &samples));
    }
    drop(lease);
    Ok(())
}

// ---------------------------------------------------------------------------
// Sub-phase 3.7 — acceptance: no-storage, limits, lifecycle and legacy
// ---------------------------------------------------------------------------

/// `T-WEB-E2EE` (audit half): the server's own web-transfer modules must not
/// contain a filesystem or anonymous-memory API at all. The invariant is
/// "the server stores no payload", and the cheapest way to keep it true as
/// the modules grow is to refuse the APIs that could break it — scoped to
/// these files, never a global grep that would trip over the rest of bore.
#[test]
fn server_source_contains_no_payload_filesystem_api() {
    const MODULES: &[&str] = &[
        "src/web_transfer.rs",
        "src/web_transfer_http.rs",
        "src/web_transfer_protocol.rs",
        "src/web_transfer_cli.rs",
    ];
    // Writing, mapping or opening anything payload-shaped. `include_str!` and
    // `include_bytes!` are compile-time and deliberately allowed (the browser
    // bundle is embedded that way).
    const FORBIDDEN: &[&str] = &[
        "File::create",
        "File::open",
        "OpenOptions",
        "fs::write",
        "fs::read",
        "fs::File",
        "tempfile",
        "NamedTempFile",
        "memfd",
        "mmap",
        "std::io::copy",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for module in MODULES {
        let whole = std::fs::read_to_string(root.join(module))
            .unwrap_or_else(|e| panic!("{module} must be readable: {e}"));
        // Production code only: the `#[cfg(test)]` tail legitimately reads
        // fixture files, and a test that cannot read its fixtures is not the
        // invariant this guards.
        let source = match whole.find("#[cfg(test)]") {
            Some(at) => whole[..at].to_string(),
            None => whole,
        };
        for needle in FORBIDDEN {
            assert!(
                !source.contains(needle),
                "{module} uses {needle}: the web-transfer server must never touch \
                 the filesystem for payload (plan invariant; see docs/plans/\
                 001_plan-WebTransfer/overview.md)"
            );
        }
    }
}

/// `T-WEB-LIMITS`: every cap, rate and malformed input is refused with a
/// typed error, and a healthy peer in the same room keeps working across all
/// of them. A refusal that also breaks the innocent peer is a denial of
/// service with extra steps, so the healthy-peer check runs after each case.
#[tokio::test]
async fn t_web_limits() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    // A small room cap keeps the admission case cheap; every other limit is
    // the shipped default, because a limit tested at a test-only value proves
    // the mechanism and not the product.
    args.max_peers_per_room = 3;
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let carriers_for_bounds = config.limits.direct_carriers;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x71u8; 32]);
    let owner = OwnerToken::from_bytes([0x72u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(5);

    let mut healthy = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    healthy.hello(&token_hex, Some("healthy")).await?;
    assert_eq!(
        control_msg(&healthy.next_text(wait).await?.expect("welcome")).0,
        "welcome"
    );
    read_snapshot(&mut healthy).await?;

    let mut victim = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    victim.hello(&token_hex, Some("victim")).await?;
    assert_eq!(
        control_msg(&victim.next_text(wait).await?.expect("welcome")).0,
        "welcome"
    );
    read_snapshot(&mut victim).await?;
    // The healthy peer sees the join; from here it is only asked to ping.
    assert_eq!(
        control_msg(&healthy.next_text(wait).await?.expect("join")).0,
        "peer.joined"
    );

    /// Pings the healthy peer and asserts it still answers.
    async fn healthy_still_serves(peer: &mut support::WsPeer, tag: &str) -> Result<()> {
        let wait = Duration::from_secs(5);
        // `ping` is request-free by protocol: adding a `requestId` is itself
        // an INVALID_MESSAGE, and the check would prove nothing.
        peer.send_text(r#"{"v":1,"type":"ping","body":{}}"#.to_string())
            .await?;
        loop {
            let text = peer
                .next_text(wait)
                .await?
                .unwrap_or_else(|| panic!("healthy peer went away after {tag}"));
            let (typ, _) = control_msg(&text);
            if typ == "pong" {
                return Ok(());
            }
            assert_ne!(typ, "error", "healthy peer was punished for {tag}: {text}");
        }
    }

    // --- L1: malformed JSON is refused and the sender survives -------------
    victim.send_text("{not json".to_string()).await?;
    let (typ, body) = control_msg(&victim.next_text(wait).await?.expect("malformed reply"));
    assert_eq!(typ, "error");
    assert_eq!(body["code"].as_str(), Some("INVALID_MESSAGE"));
    healthy_still_serves(&mut healthy, "malformed").await?;

    // --- L2: an unknown type is refused, not ignored ------------------------
    victim
        .send_text(
            r#"{"v":1,"type":"transfer.teleport","requestId":"11111111111111111111111111111111","body":{}}"#
                .to_string(),
        )
        .await?;
    let (typ, body) = control_msg(&victim.next_text(wait).await?.expect("unknown type reply"));
    assert_eq!(typ, "error");
    assert_eq!(body["code"].as_str(), Some("INVALID_MESSAGE"));
    healthy_still_serves(&mut healthy, "unknown").await?;

    // --- L3: a wrong protocol version is named, never guessed ---------------
    victim
        .send_text(r#"{"v":99,"type":"ping","body":{}}"#.to_string())
        .await?;
    let (typ, body) = control_msg(&victim.next_text(wait).await?.expect("version reply"));
    assert_eq!(typ, "error");
    assert_eq!(body["code"].as_str(), Some("UNSUPPORTED_VERSION"));
    healthy_still_serves(&mut healthy, "version").await?;

    // --- L4: the control bucket refuses a flood, and only the flooder -------
    let mut limited = None;
    let flood_started = std::time::Instant::now();
    for _ in 0..1000u32 {
        victim
            .send_text(r#"{"v":1,"type":"ping","body":{}}"#.to_string())
            .await?;
    }
    let short = Duration::from_secs(2);
    let mut pongs = 0u32;
    let mut closed = false;
    for _ in 0..400u32 {
        // A tolerant read: the flood may also earn a close, and a closed or
        // silent socket is an answer, not a test failure.
        let Ok(maybe) = victim.next_text(short).await else {
            break;
        };
        let Some(text) = maybe else {
            closed = true;
            break;
        };
        let (typ, body) = control_msg(&text);
        if typ == "pong" {
            pongs += 1;
        }
        if typ == "error" {
            assert_eq!(body["code"].as_str(), Some("RATE_LIMITED"));
            limited = Some(body);
            break;
        }
    }
    // The burst must meet the bucket AND be answered: before 3.7 the reply
    // could not leave, because the queue's only drainer was the same task
    // that was reading the burst.
    //
    // A token bucket REFILLS while the burst is being served, so an exact
    // count measures how fast this machine drains 200 messages, not where the
    // bucket is. The floor is the burst itself — it must have been full — and
    // the ceiling is everything the configured rate can have added in the time
    // the exchange actually took. MEASURED on macos-14: 63 against a
    // hardcoded 60, i.e. one tenth of a second of refill. Both bounds come
    // from the server's own constants, so a bucket that really moves still
    // fails in either direction.
    let flood_elapsed = flood_started.elapsed();
    // ...and the burst is the one the SERVER sized for its carrier count
    // (B-A042): at one carrier this is the historical constant.
    let burst = bore_cli::web_transfer::web_transfer_control_burst(carriers_for_bounds) as u32;
    let refilled = (bore_cli::web_transfer::WEB_TRANSFER_CONTROL_RATE_PER_SEC
        * flood_elapsed.as_secs_f64())
    .ceil() as u32;
    assert!(
        pongs >= burst && pongs <= burst + refilled,
        "the control burst is {burst} plus at most {refilled} refilled in {flood_elapsed:?} \
         (closed={closed}); {pongs} answered, so the bucket moved"
    );
    assert!(
        limited.is_some(),
        "a 1000-message burst must meet the control bucket"
    );
    healthy_still_serves(&mut healthy, "flood").await?;

    // --- L5: the room admission cap refuses the peer, not the room ----------
    // Two peers are in (healthy, victim); the cap is three.
    let mut third = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    third.hello(&token_hex, Some("third")).await?;
    assert_eq!(
        control_msg(&third.next_text(wait).await?.expect("welcome third")).0,
        "welcome"
    );
    let mut fourth = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    fourth.hello(&token_hex, Some("fourth")).await?;
    // Admission happens before authentication succeeds, and every pre-auth
    // refusal shares one shape on purpose: an exhausted cap must not be
    // distinguishable from a wrong token or an absent room.
    assert!(
        fourth.next_text(wait).await?.is_none(),
        "the fourth peer must be closed, never answered"
    );
    healthy_still_serves(&mut healthy, "peer-cap").await?;

    // --- L6: a bad member token never joins, and says nothing about why -----
    let mut stranger = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    stranger.hello(&"9".repeat(64), Some("stranger")).await?;
    // Pre-authentication failures are DELIBERATELY indistinguishable: the
    // socket is delayed and closed, and no envelope says which of absent
    // room, wrong token or exhausted cap it was.
    assert!(
        stranger.next_text(wait).await?.is_none(),
        "a bad member token must be closed, never answered"
    );
    healthy_still_serves(&mut healthy, "bad-token").await?;

    Ok(())
}

/// Recursive listing of a directory as `(relative path, len)` pairs, sorted.
/// Used to prove that a server process moved 64 MiB and left nothing behind.
fn list_tree(root: &std::path::Path) -> Vec<(String, u64)> {
    fn walk(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, u64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| path.display().to_string());
                out.push((rel, meta.len()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Every open descriptor of `pid` as its resolved link target.
fn fd_targets(pid: u32) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return out;
    };
    for entry in entries.flatten() {
        match std::fs::read_link(entry.path()) {
            Ok(target) => out.push(target.display().to_string()),
            Err(_) => continue,
        }
    }
    out.sort();
    out
}

/// RSS of another process, in KiB.
fn rss_kib_of(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

/// `/proc` is the ONLY oracle for resident memory and the descriptor limit,
/// and P-12's rule forbids letting a log line stand in for a kernel number.
/// macOS and Windows have no `/proc`, so those halves of these tests cannot
/// run there: they print `N/A` and everything else in the test still runs. A
/// check that cannot run must SAY so — passing quietly is the one outcome a
/// gate may never have.
fn proc_fs_available() -> bool {
    std::path::Path::new("/proc/self/status").exists()
}

/// Renders a kernel-read KiB figure, or `n/a` where `/proc` is absent.
fn kib_or_na(value: Option<u64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |v| v.to_string())
}

/// `T-WEB-NOSTORE`: a REAL server process, with an empty working directory
/// and an empty `TMPDIR` of its own, relays 64 MiB carrying a canary and is
/// then examined at the level of the operating system — the directory tree,
/// its open descriptors, its resident memory and everything it printed.
///
/// The in-process suite already proves the relay's behaviour; what it cannot
/// prove is that the PROCESS wrote nothing, because it shares the test
/// harness's own filesystem view. This one runs the shipped binary.
#[tokio::test]
async fn t_web_nostore() -> Result<()> {
    let binary = std::env::var_os("CARGO_BIN_EXE_bore")
        .context("CARGO_BIN_EXE_bore is not available for the no-storage acceptance")?;
    let port = support::free_port().await?;
    // The server's whole writable world: its cwd and its TMPDIR, both empty,
    // both ours. The log goes OUTSIDE it, so capturing output cannot itself
    // create the file the test is looking for.
    let sandbox = std::env::temp_dir().join(format!("bore-web-nostore-{port}"));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(&sandbox)?;
    let log_path = std::env::temp_dir().join(format!("bore-web-nostore-{port}.log"));
    let log = std::fs::File::create(&log_path)?;
    let log_err = log.try_clone()?;

    let mut server = tokio::process::Command::new(&binary)
        .arg("server")
        .arg("--control-port")
        .arg(port.to_string())
        .arg("--web-transfer-base-url")
        .arg(format!("http://127.0.0.1:{port}/"))
        .arg("--web-transfer-relay-rate")
        .arg("0")
        .current_dir(&sandbox)
        .env("TMPDIR", &sandbox)
        .env("RUST_LOG", "bore_cli=trace,debug")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .kill_on_drop(true)
        .spawn()
        .context("spawning the sandboxed web-transfer server")?;
    let server_pid = server.id().context("server pid")?;
    support::wait_port(port, true).await;

    // The owner is a real `bore transfer web`, so the room exists exactly as
    // a user's would.
    let mut owner = tokio::process::Command::new(&binary)
        .arg("transfer")
        .arg("web")
        .arg("--to")
        .arg(format!("127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawning bore transfer web")?;
    let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(
        owner.stdout.take().context("owner stdout")?,
    ));
    let wait = Duration::from_secs(20);
    let first = next_line(&mut lines, wait).await?.context("room line")?;
    let url = first
        .strip_prefix("room: ")
        .context("first line is `room: <url>`")?
        .to_string();
    let (room_hex, token_hex) = split_room_url(&url)?;
    let room_key = room_link_material(&url)?.room_key.to_string();

    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let tree_before = list_tree(&sandbox);
    let fds_before = fd_targets(server_pid).len();
    let rss_before = rss_kib_of(server_pid);

    let mut a = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    a.hello(&token_hex, Some("A")).await?;
    let (_, welcome_a) = control_msg(&a.next_text(wait).await?.expect("welcome A"));
    let peer_a = welcome_a["peerId"].as_str().unwrap().to_string();
    read_snapshot(&mut a).await?;
    let mut b = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    b.hello(&token_hex, Some("B")).await?;
    let (_, welcome_b) = control_msg(&b.next_text(wait).await?.expect("welcome B"));
    let peer_b = welcome_b["peerId"].as_str().unwrap().to_string();
    read_snapshot(&mut b).await?;
    assert_eq!(
        control_msg(&a.next_text(wait).await?.expect("join")).0,
        "peer.joined"
    );

    // The canary travels in the offer's metadata; the payload carries its own.
    const CANARY_PATH: &str = "CANARY-NOSTORE-pangolin.bin";
    let offer_hex = "dddddddddddddddddddddddddddddddd";
    let manifest = catalog_manifest(offer_hex, "CANARY-NOSTORE-label", CANARY_PATH);
    catalog_publish(&mut a, &"d".repeat(32), offer_hex, &manifest).await?;
    let (got, added) = control_msg(&b.next_text(wait).await?.expect("added on B"));
    assert_eq!(got, "offer.added");
    let mac_hex = added["mac"].as_str().unwrap().to_string();
    let selection = serde_json::json!({
        "entryIds": ["0"],
        "manifestMac": mac_hex,
        "mode": "raw",
        "offerId": offer_hex,
    });
    use sha2::Digest as _;
    let digest_hex = hex::encode(sha2::Sha256::digest(
        bore_cli::web_transfer_protocol::canonical_json(&selection)
            .unwrap()
            .as_bytes(),
    ));
    b.send_text(
        serde_json::json!({
            "v": 1,
            "type": "transfer.request",
            "requestId": "00000000000000000000000000000001",
            "body": {
                "offerId": offer_hex,
                "entryIds": ["0"],
                "selectionDigest": digest_hex,
                "mode": "raw",
            },
        })
        .to_string(),
    )
    .await?;
    let (got, ack) = control_msg(&b.next_text(wait).await?.expect("request ack"));
    assert_eq!(got, "ack");
    let transfer_id = ack["result"]["transferId"].as_str().unwrap().to_string();
    let (got, incoming) = control_msg(&a.next_text(wait).await?.expect("incoming"));
    assert_eq!(got, "transfer.incoming");
    let attempt_id = incoming["attemptId"].as_str().unwrap().to_string();
    a.send_text(
        serde_json::json!({
            "v": 1,
            "type": "transfer.source_ready",
            "requestId": "00000000000000000000000000000002",
            "body": {
                "transferId": transfer_id,
                "attemptId": attempt_id,
                "selectionDigest": digest_hex,
            },
        })
        .to_string(),
    )
    .await?;
    assert_eq!(
        control_msg(&a.next_text(wait).await?.expect("ready ack")).0,
        "ack"
    );
    // Phase 4: decline the direct attempt so the payload rides the relay,
    // which is what this test is about. The tickets name the fresh attempt
    // the fallback minted.
    support::decline_direct(
        &mut a,
        &mut b,
        &transfer_id,
        &attempt_id,
        "00000000000000000000000000000003",
        wait,
    )
    .await?;
    let (_, ticket_a) = control_msg(&a.next_text(wait).await?.expect("ticket A"));
    let (_, ticket_b) = control_msg(&b.next_text(wait).await?.expect("ticket B"));
    let attempt_id = ticket_a["attemptId"].as_str().unwrap().to_string();
    assert_ne!(
        ticket_b["attemptId"].as_str(),
        incoming["attemptId"].as_str()
    );

    macro_rules! attach_leg {
        ($peer:expr, $role:expr, $ticket:expr) => {{
            let mut leg =
                support::RelayLeg::connect(&host, &room_hex, &transfer_id, &origin).await?;
            leg.send_text(
                serde_json::json!({
                    "v": 1,
                    "peerId": $peer,
                    "transferId": transfer_id,
                    "attemptId": attempt_id,
                    "role": $role,
                    "ticket": $ticket,
                })
                .to_string(),
            )
            .await?;
            leg
        }};
    }

    let mut leg_a = attach_leg!(&peer_a, "source", ticket_a["ticket"].as_str().unwrap());
    let mut leg_b = attach_leg!(&peer_b, "recipient", ticket_b["ticket"].as_str().unwrap());
    assert_eq!(
        control_msg(&a.next_text(wait).await?.expect("commit A")).0,
        "transfer.path_commit"
    );
    assert_eq!(
        control_msg(&b.next_text(wait).await?.expect("commit B")).0,
        "transfer.path_commit"
    );

    // 64 MiB of opaque frames, each carrying the canary in its body: if the
    // server ever writes payload anywhere, this is the string that shows up.
    const FRAME_BODY: usize = 32752;
    const FRAME_COUNT: usize = 2048;
    let received = tokio::spawn(async move {
        let mut total = 0usize;
        for _ in 0..FRAME_COUNT {
            match leg_b.next_msg(Duration::from_secs(30)).await {
                Ok(Some(tokio_tungstenite::tungstenite::Message::Binary(bytes))) => {
                    total += bytes.len();
                }
                _ => break,
            }
        }
        total
    });
    let mut body = vec![0u8; FRAME_BODY];
    body[..CANARY_PATH.len()].copy_from_slice(CANARY_PATH.as_bytes());
    for seq in 0..FRAME_COUNT as u32 {
        leg_a.send_binary(relay_test_frame(seq, &body)).await?;
    }
    let total = received.await?;
    assert_eq!(
        total,
        (FRAME_BODY + 16) * FRAME_COUNT,
        "every frame arrives"
    );

    let rss_after = rss_kib_of(server_pid);
    let tree_after = list_tree(&sandbox);
    let fds_after = fd_targets(server_pid);

    // 1. Nothing was written: not in the working directory, not in TMPDIR.
    assert_eq!(
        tree_before, tree_after,
        "the server wrote into its own directory while relaying"
    );
    assert!(
        tree_after.is_empty(),
        "the sandbox must stay empty: {tree_after:?}"
    );

    // 2. No descriptor points at a file — deleted, anonymous or otherwise.
    if proc_fs_available() {
        for target in &fds_after {
            assert!(
                !target.contains("(deleted)"),
                "a deleted-file descriptor is a payload file with the name removed: {target}"
            );
            assert!(
                !target.contains("memfd:"),
                "an anonymous memory file is still a payload file: {target}"
            );
            assert!(
                !target.starts_with(format!("{}/", sandbox.display()).as_str()),
                "a descriptor points inside the sandbox: {target}"
            );
        }
        // Sockets come and go; the count must not have grown by a file per frame.
        assert!(
            fds_after.len() < fds_before + 32,
            "descriptors grew from {fds_before} to {} while relaying",
            fds_after.len()
        );
    } else {
        println!("N/A the descriptor inspection needs /proc/<pid>/fd");
    }

    // 3. Resident memory does not follow the payload. The relay holds a
    //    bounded number of frames, so 64 MiB may not cost 64 MiB.
    match (rss_before, rss_after) {
        (Some(before), Some(after)) => {
            let growth_kib = after.saturating_sub(before);
            assert!(
                growth_kib < 16 * 1024,
                "RSS grew {growth_kib} KiB relaying 64 MiB (before {before}, after {after})"
            );
        }
        _ => println!("N/A the RSS budget needs /proc/<pid>/status"),
    }

    // 4. Nothing secret or payload-shaped was printed.
    drop(server.kill().await);
    drop(owner.kill().await);
    let logged = std::fs::read_to_string(&log_path).unwrap_or_default();
    for needle in [CANARY_PATH, "CANARY-NOSTORE-label", &room_key, &token_hex] {
        assert!(
            !logged.contains(needle),
            "the server printed {needle:?}: payload, filenames and secrets never reach a log"
        );
    }
    let _ = std::fs::remove_dir_all(&sandbox);
    let _ = std::fs::remove_file(&log_path);
    Ok(())
}

// ---------------------------------------------------------------------------
// Sub-phase 3.7 — T-WEB-ROOM-LIFE (real processes, real wall clock)
// ---------------------------------------------------------------------------

/// T-WEB-ROOM-LIFE: the owner's death decides the room, measured against the
/// wall clock of a real `bore server --web-transfer-owner-grace 5` and a real
/// `bore transfer web` process.
///
/// Two halves, because they are opposite promises:
///
///   * SIGKILL is an ABNORMAL loss — the owner may be coming back, so the
///     room (and a relay in mid-payload) survives the grace and only then
///     dies, taking the active transfer with it.
///   * SIGTERM is a CLEAN close — nothing is coming back, so the room and
///     every socket on it go immediately, well inside the same grace.
///
/// The in-process twins (`t_web_registry_life`, `t_web_owner_lease`) own the
/// resume-keeps-the-room half with an injected transport; this one is here
/// for what a controlled clock cannot prove: that the grace is a real five
/// seconds of a real server, and that a live relay pair is what expires.
#[cfg(unix)]
#[tokio::test]
async fn t_web_room_life() -> Result<()> {
    let binary = std::env::var_os("CARGO_BIN_EXE_bore")
        .context("CARGO_BIN_EXE_bore is not available for the lifecycle acceptance")?;
    let port = support::free_port().await?;
    let mut server = tokio::process::Command::new(&binary)
        .arg("server")
        .arg("--control-port")
        .arg(port.to_string())
        .arg("--web-transfer-base-url")
        .arg(format!("http://127.0.0.1:{port}/"))
        .arg("--web-transfer-relay-rate")
        .arg("0")
        .arg("--web-transfer-owner-grace")
        .arg("5")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawning the lifecycle server")?;
    support::wait_port(port, true).await;
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(20);

    /// Spawns one real owner and returns its pid with the room it created.
    async fn spawn_owner(
        binary: &std::ffi::OsStr,
        port: u16,
    ) -> Result<(tokio::process::Child, u32, String, String)> {
        let mut owner = tokio::process::Command::new(binary)
            .arg("transfer")
            .arg("web")
            .arg("--to")
            .arg(format!("127.0.0.1:{port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("spawning bore transfer web")?;
        let pid = owner.id().context("owner pid")?;
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(
            owner.stdout.take().context("owner stdout")?,
        ));
        let first = next_line(&mut lines, Duration::from_secs(20))
            .await?
            .context("room line")?;
        let url = first
            .strip_prefix("room: ")
            .context("first line is `room: <url>`")?
            .to_string();
        let (room_hex, token_hex) = split_room_url(&url)?;
        Ok((owner, pid, room_hex, token_hex))
    }

    async fn connect_welcome(
        host: &str,
        room_hex: &str,
        origin: &str,
        token_hex: &str,
        display_name: &str,
    ) -> Result<(support::WsPeer, serde_json::Value)> {
        let mut last_error = None;
        for _ in 0..3 {
            let mut peer = support::WsPeer::connect(host, room_hex, origin).await?;
            peer.hello(token_hex, Some(display_name)).await?;
            match peer.next_text(Duration::from_secs(20)).await? {
                Some(text) => {
                    let (typ, body) = control_msg(&text);
                    if typ == "welcome" {
                        return Ok((peer, body));
                    }
                    last_error = Some(format!("expected welcome, got {typ}"));
                }
                None => last_error = Some("control socket closed before welcome".to_string()),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!(
            "room peer did not receive welcome after retries: {}",
            last_error.unwrap_or_else(|| "unknown error".to_string())
        )
    }

    // --- L1: abnormal loss holds the room for the grace, then destroys it ---
    let (mut owner, owner_pid, room_hex, token_hex) = spawn_owner(&binary, port).await?;

    let (mut a, welcome_a) = connect_welcome(&host, &room_hex, &origin, &token_hex, "A").await?;
    let peer_a = welcome_a["peerId"].as_str().unwrap().to_string();
    read_snapshot(&mut a).await?;
    let (mut b, welcome_b) = connect_welcome(&host, &room_hex, &origin, &token_hex, "B").await?;
    let peer_b = welcome_b["peerId"].as_str().unwrap().to_string();
    read_snapshot(&mut b).await?;
    assert_eq!(
        control_msg(&a.next_text(wait).await?.expect("join")).0,
        "peer.joined"
    );

    let offer_hex = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let manifest = catalog_manifest(offer_hex, "life", "life.bin");
    catalog_publish(&mut a, &"e".repeat(32), offer_hex, &manifest).await?;
    let (got, added) = control_msg(&b.next_text(wait).await?.expect("added on B"));
    assert_eq!(got, "offer.added");
    let selection = serde_json::json!({
        "entryIds": ["0"],
        "manifestMac": added["mac"].as_str().unwrap(),
        "mode": "raw",
        "offerId": offer_hex,
    });
    use sha2::Digest as _;
    let digest_hex = hex::encode(sha2::Sha256::digest(
        bore_cli::web_transfer_protocol::canonical_json(&selection)
            .unwrap()
            .as_bytes(),
    ));
    b.send_text(
        serde_json::json!({
            "v": 1,
            "type": "transfer.request",
            "requestId": "1".repeat(32),
            "body": {
                "offerId": offer_hex,
                "entryIds": ["0"],
                "selectionDigest": digest_hex,
                "mode": "raw",
            },
        })
        .to_string(),
    )
    .await?;
    let (got, ack) = control_msg(&b.next_text(wait).await?.expect("request ack"));
    assert_eq!(got, "ack");
    let transfer_id = ack["result"]["transferId"].as_str().unwrap().to_string();
    let (got, incoming) = control_msg(&a.next_text(wait).await?.expect("incoming"));
    assert_eq!(got, "transfer.incoming");
    let attempt_id = incoming["attemptId"].as_str().unwrap().to_string();
    a.send_text(
        serde_json::json!({
            "v": 1,
            "type": "transfer.source_ready",
            "requestId": "2".repeat(32),
            "body": {
                "transferId": transfer_id,
                "attemptId": attempt_id,
                "selectionDigest": digest_hex,
            },
        })
        .to_string(),
    )
    .await?;
    assert_eq!(
        control_msg(&a.next_text(wait).await?.expect("ready ack")).0,
        "ack"
    );
    // Phase 4: decline the direct attempt so the payload rides the relay,
    // which is what this test is about. The tickets name the fresh attempt
    // the fallback minted.
    support::decline_direct(
        &mut a,
        &mut b,
        &transfer_id,
        &attempt_id,
        &"3".repeat(32),
        wait,
    )
    .await?;
    let (_, ticket_a) = control_msg(&a.next_text(wait).await?.expect("ticket A"));
    let (_, ticket_b) = control_msg(&b.next_text(wait).await?.expect("ticket B"));
    let attempt_id = ticket_a["attemptId"].as_str().unwrap().to_string();
    assert_ne!(
        ticket_b["attemptId"].as_str(),
        incoming["attemptId"].as_str()
    );

    macro_rules! attach_leg {
        ($peer:expr, $role:expr, $ticket:expr) => {{
            let mut leg =
                support::RelayLeg::connect(&host, &room_hex, &transfer_id, &origin).await?;
            leg.send_text(
                serde_json::json!({
                    "v": 1,
                    "peerId": $peer,
                    "transferId": transfer_id,
                    "attemptId": attempt_id,
                    "role": $role,
                    "ticket": $ticket,
                })
                .to_string(),
            )
            .await?;
            leg
        }};
    }

    let mut leg_a = attach_leg!(&peer_a, "source", ticket_a["ticket"].as_str().unwrap());
    let mut leg_b = attach_leg!(&peer_b, "recipient", ticket_b["ticket"].as_str().unwrap());
    assert_eq!(
        control_msg(&a.next_text(wait).await?.expect("commit A")).0,
        "transfer.path_commit"
    );
    assert_eq!(
        control_msg(&b.next_text(wait).await?.expect("commit B")).0,
        "transfer.path_commit"
    );

    // One frame through the live pair: the relay is active, not merely paired.
    let body = vec![0x5au8; 4096];
    leg_a.send_binary(relay_test_frame(0, &body)).await?;
    match leg_b.next_msg(Duration::from_secs(10)).await? {
        Some(tokio_tungstenite::tungstenite::Message::Binary(bytes)) => {
            assert_eq!(bytes.len(), body.len() + 16)
        }
        other => anyhow::bail!("the pair is not relaying: {other:?}"),
    }

    // SIGKILL: the owner cannot say goodbye, so the room is held.
    signal_pid(owner_pid, "-KILL")?;
    let killed_at = std::time::Instant::now();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        room_alive(&host, &room_hex, &origin, &token_hex).await,
        "an abnormal owner loss must hold the room for the grace"
    );
    // And the payload keeps moving while it is held.
    leg_a.send_binary(relay_test_frame(1, &body)).await?;
    match leg_b.next_msg(Duration::from_secs(10)).await? {
        Some(tokio_tungstenite::tungstenite::Message::Binary(bytes)) => {
            assert_eq!(bytes.len(), body.len() + 16)
        }
        other => anyhow::bail!("the held room stopped relaying: {other:?}"),
    }

    // Then the grace runs out and the room goes, whatever is on it.
    let mut died_at = None;
    while killed_at.elapsed() < Duration::from_secs(25) {
        if !room_alive(&host, &room_hex, &origin, &token_hex).await {
            died_at = Some(killed_at.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let died_at = died_at.context("the room outlived its grace by more than 20 s")?;
    assert!(
        died_at >= Duration::from_secs(5),
        "the room died after {died_at:?}, inside its own 5 s grace"
    );

    // The active transfer is aborted with it: the recipient leg closes and no
    // further frame crosses.
    let recipient_end = loop {
        match leg_b.next_msg(Duration::from_secs(10)).await? {
            Some(tokio_tungstenite::tungstenite::Message::Binary(_)) => continue,
            other => break other,
        }
    };
    assert!(
        matches!(
            recipient_end,
            None | Some(tokio_tungstenite::tungstenite::Message::Close(_))
        ),
        "room expiry must abort the active payload, got {recipient_end:?}"
    );
    // Both control sockets are gone too (a dead room keeps no page alive).
    assert!(
        a.next_text(Duration::from_secs(10)).await?.is_none()
            || !room_alive(&host, &room_hex, &origin, &token_hex).await
    );
    drop(owner.kill().await);

    // --- L2: a clean close is immediate, never graced ------------------------
    let (mut owner2, owner2_pid, room2, token2) = spawn_owner(&binary, port).await?;
    let (mut c, _) = connect_welcome(&host, &room2, &origin, &token2, "C").await?;
    signal_pid(owner2_pid, "-TERM")?;
    let closed_at = std::time::Instant::now();
    // The live page's socket ends without waiting for any grace. A farewell
    // notice may precede the close; it is the CLOSE that is the promise.
    let mut notices: Vec<String> = Vec::new();
    loop {
        match c.next_text(Duration::from_secs(5)).await? {
            Some(text) => notices.push(text),
            None => break,
        }
        anyhow::ensure!(
            closed_at.elapsed() < Duration::from_secs(5),
            "a clean close must end the live control socket at once (saw {notices:?})"
        );
    }
    assert!(
        !room_alive(&host, &room2, &origin, &token2).await,
        "a clean close destroys the room immediately"
    );
    assert!(
        closed_at.elapsed() < Duration::from_secs(5),
        "the clean close took {:?}, i.e. it waited for the grace",
        closed_at.elapsed()
    );
    drop(owner2.kill().await);
    drop(server.kill().await);
    Ok(())
}

// ---------------------------------------------------------------------------
// Sub-phase 3.7 — T-WEB-LEGACY (the paths that existed before this feature)
// ---------------------------------------------------------------------------

/// T-WEB-LEGACY: a server with web transfer ENABLED still is the server it
/// was. The feature adds an HTTP surface and a room registry to the same
/// control port, so "it compiles" proves nothing about the two paths that
/// were already there: a public tunnel (the bore protocol itself) and
/// `bore transfer listener|sender` (the native file transfer).
///
/// All three run against ONE server here on purpose — coexistence is the
/// claim, and three separate servers would each prove only that the path
/// works alone.
#[tokio::test]
async fn t_web_legacy() -> Result<()> {
    use bore_cli::client::Client;
    use bore_cli::transfer::{
        CollisionPolicy, DeviceMode, ListenerOptions, SenderOptions, SymlinkMode,
    };
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(config)?;
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;
    let to = format!("localhost:{port}");

    // --- 1. A public tunnel still proxies bytes in both directions ----------
    let local = tokio::net::TcpListener::bind("localhost:0").await?;
    let local_port = local.local_addr()?.port();
    let client = Client::new(
        "localhost",
        local_port,
        &to,
        0,
        None,
        false,
        Default::default(),
        None,
    )
    .await?;
    let public: std::net::SocketAddr = ([127, 0, 0, 1], client.remote_port()).into();
    tokio::spawn(client.listen());
    let echo = tokio::spawn(async move {
        let (mut stream, _) = local.accept().await?;
        let mut buf = [0u8; 11];
        stream.read_exact(&mut buf).await?;
        stream.write_all(b"legacy pong").await?;
        anyhow::Result::<_>::Ok(buf)
    });
    let mut stream = tokio::net::TcpStream::connect(public).await?;
    stream.write_all(b"legacy ping").await?;
    let mut pong = [0u8; 11];
    stream.read_exact(&mut pong).await?;
    assert_eq!(&pong, b"legacy pong");
    assert_eq!(&echo.await??, b"legacy ping");
    drop(stream);

    // --- 2. `bore transfer` still moves a file through the same server ------
    let dir = std::env::temp_dir().join(format!("bore-web-legacy-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("in"))?;
    std::fs::create_dir_all(dir.join("out"))?;
    let source = dir.join("in").join("legacy.bin");
    let payload: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &payload)?;
    let transfer_id = "web-legacy-coexistence".to_string();
    let listener = tokio::spawn(bore_cli::transfer::run_listener(ListenerOptions {
        to: to.clone(),
        secret: None,
        insecure: false,
        transfer_id: Some(transfer_id.clone()),
        dest_path: dir.join("out"),
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        collision: CollisionPolicy::Fail,
        persistent: false,
        ask_confirm: false,
        confirm_timeout: 0,
        stall_timeout: 0,
        no_fsync: true,
    }));
    // The listener registers before the sender dials; a sender that arrives
    // first fails outright, which would be a flake and not a finding.
    tokio::time::sleep(Duration::from_millis(300)).await;
    bore_cli::transfer::run_sender(SenderOptions {
        to: to.clone(),
        secret: None,
        insecure: false,
        transfer_id: Some(transfer_id),
        sources: vec![source.clone()],
        source_files: vec![],
        ask_confirm: false,
        output: None,
        relay_only: true,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        nat_udp_release_timeout: 0,
        carriers: 1,
        parallel: 1,
        symlinks: SymlinkMode::Exclude,
        devices: DeviceMode::Exclude,
        stall_timeout: 0,
    })
    .await
    .context("the native sender failed against a web-enabled server")?;
    let outcome = tokio::time::timeout(Duration::from_secs(60), listener)
        .await
        .context("the native listener did not finish")???;
    assert_eq!(outcome.total_bytes, payload.len() as u64);
    assert_eq!(std::fs::read(&outcome.final_path)?, payload);

    // --- 3. ...and the web surface was live throughout ----------------------
    let shell = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &format!(
            "GET /transfer/{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
            "0".repeat(32)
        ),
    )
    .await?;
    assert!(
        shell.starts_with("HTTP/1.1 200"),
        "the web surface stopped answering: {shell:.60}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// A flag table in the README is a promise; `--help` is the truth. The two
/// drift silently — a default changed in `main.rs` leaves the README quoting
/// yesterday's number, and an operator sizes a server from the wrong one. This
/// test is the docs gate of 3.8: every `--web-transfer-*` flag exists in the
/// binary, in the flag reference and in the feature table, with the same env
/// var and the same default in all three.
#[test]
fn t_web_readme() -> Result<()> {
    /// `--flag <VALUE>` plus the `[env: …]` / `[default: …]` clap annotations.
    #[derive(Debug, Default, PartialEq, Eq)]
    struct FlagDoc {
        env: Option<String>,
        default: Option<String>,
    }

    /// Reads the annotations clap prints (and the README repeats verbatim).
    fn annotations(text: &str) -> FlagDoc {
        let field = |key: &str| {
            text.split_once(&format!("[{key}: ")).and_then(|(_, rest)| {
                rest.split_once(']')
                    .map(|(value, _)| value.trim().to_string())
            })
        };
        FlagDoc {
            env: field("env").map(|env| env.trim_end_matches('=').to_string()),
            // The README annotates a raw byte count with its human size
            // ("104857600 (100 MiB/s)"); the number is what must match.
            default: field("default").map(|value| {
                value
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string()
            }),
        }
    }

    /// Every `--web-transfer-*` line of a `--help`-shaped block.
    fn help_flags(block: &str) -> BTreeMap<String, FlagDoc> {
        let mut out = BTreeMap::new();
        // clap wraps: the flag may be on its own line and the annotations on
        // the next, so a flag collects every line until the following flag.
        let mut current: Option<String> = None;
        let mut buffer = String::new();
        for line in block.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("--") || trimmed.starts_with("-h,") {
                if let Some(flag) = current.take() {
                    out.insert(flag, annotations(&buffer));
                }
                buffer.clear();
                let name = trimmed
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches(',')
                    .to_string();
                if name.starts_with("--web-transfer-") {
                    current = Some(name);
                }
            }
            buffer.push(' ');
            buffer.push_str(trimmed);
        }
        if let Some(flag) = current.take() {
            out.insert(flag, annotations(&buffer));
        }
        out
    }

    let binary = std::env::var_os("CARGO_BIN_EXE_bore")
        .context("CARGO_BIN_EXE_bore is not available for the README docs gate")?;
    let server_help = String::from_utf8(
        std::process::Command::new(&binary)
            .args(["server", "--help"])
            .output()?
            .stdout,
    )?;
    let client_help = String::from_utf8(
        std::process::Command::new(&binary)
            .args(["transfer", "web", "--help"])
            .output()?
            .stdout,
    )?;
    let readme = read_doc_text(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))?;

    // (1) The binary's own flags, straight out of clap.
    let truth = help_flags(&server_help);
    assert!(
        truth.len() >= 14,
        "expected the whole --web-transfer-* group from --help, got {}",
        truth.len()
    );

    // (2) The README's copy of the same help block, inside the flag reference.
    let reference = readme
        .split_once("Browser-to-browser transfer (always available;")
        .context("README has no --web-transfer-* group in the full server flag reference")?
        .1
        .split_once("\nAccess logging")
        .context("the README flag-reference group is not terminated")?
        .0;
    assert_eq!(
        help_flags(reference),
        truth,
        "the README flag reference disagrees with `bore server --help`"
    );

    // (3) The feature section's table, which is what an operator actually reads.
    let mut table: BTreeMap<String, FlagDoc> = BTreeMap::new();
    for row in readme
        .lines()
        .filter(|l| l.starts_with("| `--web-transfer-"))
    {
        let cells: Vec<&str> = row.trim_matches('|').split('|').map(str::trim).collect();
        assert!(cells.len() >= 4, "malformed README flag row: {row}");
        let flag = cells[0]
            .trim_matches('`')
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        let env = cells[1].trim_matches('`');
        let default = cells[2];
        table.insert(
            flag,
            FlagDoc {
                env: (env != "—").then(|| env.to_string()),
                // "*(off)*", "public defaults" and "off" describe a flag clap
                // gives no default for; only a real value is compared.
                default: default
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
                    .then(|| {
                        default
                            .split_whitespace()
                            .next()
                            .unwrap_or_default()
                            .to_string()
                    }),
            },
        );
    }
    // One row per flag, and the check has to be made on the ROWS: the table
    // above is a map, so a flag documented twice with two different defaults
    // would quietly become whichever row came last — the drift this gate
    // exists to refuse, hidden by the data structure that looks for it.
    let mut rows: Vec<&str> = readme
        .lines()
        .filter(|l| l.starts_with("| `--web-transfer-"))
        .map(|l| {
            l.trim_start_matches("| `")
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_end_matches('`')
        })
        .collect();
    let counted = rows.len();
    rows.sort_unstable();
    rows.dedup();
    assert_eq!(
        counted,
        rows.len(),
        "a --web-transfer-* flag is listed more than once in the README table"
    );
    assert_eq!(
        table.keys().collect::<Vec<_>>(),
        truth.keys().collect::<Vec<_>>(),
        "the README feature table lists a different set of flags than `bore server --help`"
    );
    for (flag, doc) in &table {
        let real = &truth[flag];
        assert_eq!(
            &doc.env, &real.env,
            "{flag}: README env differs from --help"
        );
        if let Some(default) = &doc.default {
            assert_eq!(
                Some(default),
                real.default.as_ref(),
                "{flag}: README default differs from --help"
            );
        } else {
            assert!(
                real.default.is_none() || flag.as_str() == "--web-transfer-base-url",
                "{flag}: --help has a default the README table does not quote"
            );
        }
    }

    // (4) The four client flags of `bore transfer web`, same promise.
    for flag in client_help
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("--") || l.starts_with("-t,") || l.starts_with("-s,"))
        .filter_map(|l| l.split_whitespace().find(|t| t.starts_with("--")))
        .map(|f| f.trim_end_matches(','))
        .filter(|f| *f != "--help")
    {
        assert!(
            readme.contains(&format!("`{flag} ")) || readme.contains(&format!("`{flag}`")),
            "`bore transfer web {flag}` is not documented in README.md"
        );
    }

    // (5) The release statement the whole section rests on. Sub-phase 5.3
    // shipped the archive resume and the per-file pick, so the scope moved
    // again: both kinds of download now resume, a folder offer can be opened
    // to take ONE file out of it, and a source that changed under a partial
    // is a named outcome with its own button instead of a silent discard.
    // What is still missing is an arbitrary SUBSET of a folder, and nothing
    // else.
    for promise in [
        "Not yet included:",
        // 7.1: the flag, what enforces it, and the one thing an operator
        // would otherwise have to discover by experiment — that an old
        // server is refused instead of silently obeyed.
        "never sends a peer the message that starts a direct attempt",
        "A server that predates the flag is refused, never silently obeyed",
        "relay (imposto)",
        "WebRTC DataChannel",
        "**opaque WebSocket relay**",
        "tries the direct path first",
        "**a file, a selection of files or\n> a whole folder**",
        "as **one ZIP archive**",
        "`Scarica ZIP`",
        "Both kinds of download resume after an interruption",
        "one file out of it",
        "ZIP download resumes too",
        "prefix of it can be resumed",
        "La sorgente è cambiata",
        "Riparti da zero",
        "empty directory",
    ] {
        assert!(
            readme.contains(promise),
            "README no longer states the release scope ({promise:?})"
        );
    }
    // And the words 5.1 had to leave standing must be GONE, not merely
    // contradicted somewhere else in the page: a README that both refuses
    // and offers the same download is worse than one that is out of date.
    for stale in [
        "downloads a **single-file offer**",
        "downloading folders and multi-file offers",
        "archive/ZIP packaging",
        "resuming an interrupted ZIP download",
        "an archive starts again from the",
        "Cartelle e selezioni multiple non ancora supportate",
    ] {
        assert!(
            !readme.contains(stale),
            "README still refuses a download this release ships ({stale:?})"
        );
    }

    // (6) The network promises (4.5). Three of them are policy — direct is
    // the default, the fallback costs no second click, and bore hands out no
    // TURN — and one is a CONSTANT of the product: the default STUN chain.
    // Prose that quotes a constant drifts from it silently, which is exactly
    // what this gate exists to refuse.
    for promise in [
        "Every transfer tries the **direct path first**",
        "never costs a second click",
        "bore never hands out a TURN server",
        "outbound UDP from both browsers",
        "No inbound port has to be opened anywhere",
        "`--web-transfer-no-stun` offers host candidates only",
    ] {
        assert!(
            readme.contains(promise),
            "README no longer states the direct-path network promise ({promise:?})"
        );
    }
    for server in bore_cli::holepunch::PUBLIC_STUN {
        assert!(
            readme.contains(&format!("`{server}`")),
            "README does not quote the default STUN server {server} the server actually offers"
        );
    }

    // (6b) V003-C4. Troubleshooting used to name ONE cause as certain in two
    // places where the measurement says otherwise, and both mistakes pointed
    // the reader at a setting instead of at the path:
    //   - on ONE LAN the browsers pair on HOST candidates and STUN is not
    //     involved, so `--web-transfer-no-stun` is not what forces the relay
    //     there; what forces it is UDP not crossing between the two machines;
    //   - a slow RELAY transfer is not explained by the shipped throttle,
    //     whose default is 100 MiB/s, and a slow DIRECT transfer was not
    //     mentioned at all although the direct path is measurably bimodal.
    // The needles are single lines on purpose: the windows runner checks the
    // tree out with CRLF and a multi-line needle cannot match there (B-A016).
    for promise in [
        "the browsers normally pair on their **host** candidates and STUN is not involved at all",
        "does **not** by itself force the relay",
        "client isolation (\"AP isolation\", \"guest network\")",
        "Only if the two are on **different** networks does STUN matter",
        "the default is already **100 MiB/s**",
        "and the row reads `diretto`",
        "`drain_longest` in the report is exactly this wait",
    ] {
        assert!(
            readme.contains(promise),
            "README troubleshooting no longer matches the measurement ({promise:?})"
        );
    }
    // The old wording must be GONE, not merely contradicted further down: a
    // reader who finds the wrong row first stops reading.
    for stale in [
        "`--web-transfer-no-stun` is set (host candidates only), or outbound UDP is blocked on one of the two sides",
        "and the relay is throttled per room | Raise or disable",
    ] {
        assert!(
            !readme.contains(stale),
            "README still carries the corrected troubleshooting wording ({stale:?})"
        );
    }

    // (7) Sub-phase 5.5: the page the operator will actually look at, and the
    // end-to-end run they will actually perform. The interface facts are here
    // for the same reason the STUN chain is: prose that describes a product
    // drifts from it silently, and a badge shape or a zone name is exactly the
    // kind of detail a later change moves without noticing the guide.
    for promise in [
        "**What the page looks like.**",
        "`La mia room`",
        "`◆ diretto`",
        "Nothing moves while a transfer runs:",
        "refused with a message",
        // Wrapped across two lines in the guide, so the gate pins the half
        // that names the refusal and cannot be reworded without noticing.
        "Room non disponibile: non è possibile",
        "A browser serves only what it published itself",
        "room-wide \"download everything\" button",
        "#### A complete run, three browsers",
    ] {
        assert!(
            readme.contains(promise),
            "README no longer documents the shipped interface or the full run ({promise:?})"
        );
    }

    // (8) Sub-phase 6.6: the troubleshooting table quotes MESSAGES, and the
    // messages live in `state.js`. A table that quotes a string the page no
    // longer shows sends a reader searching for words that do not exist, and
    // the failure is silent because both files are prose to the compiler.
    // So the table's rows are read back OUT of the product's own error map.
    let error_map = read_doc_text(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/transfer/src/state.js"),
    )
    .context("web/transfer/src/state.js is not readable")?;
    for code in [
        "MULTI_ENTRY",
        "SOURCE_CHANGED",
        "SOURCE_OFFLINE",
        "OFFER_CHANGED",
        "STORAGE_QUOTA",
        "UNSUPPORTED",
        "RELAY_BUSY",
        "MANIFEST_MAC",
        "ROOM_UNAVAILABLE",
        "UNSUPPORTED_VERSION",
    ] {
        let needle = format!("[\"{code}\", \"");
        let text = error_map
            .split_once(&needle)
            .with_context(|| format!("state.js no longer maps {code}"))?
            .1
            .split_once('"')
            .with_context(|| format!("state.js entry for {code} is malformed"))?
            .0;
        assert!(
            readme.contains(text),
            "README does not quote the message the page shows for {code} ({text:?})"
        );
    }

    // Every link the section points at must resolve, in-page anchors
    // included. A guide that sends a reader to a file that moved is a guide
    // that was never checked.
    let section = readme
        .split_once("### Browser-to-browser transfer (`bore transfer web`)")
        .context("README has no browser-to-browser section")?
        .1
        .split_once("\n### ")
        .map(|(body, _)| body)
        .unwrap_or(&readme);
    // The admin fields the guide names must EXIST. An operator reading a
    // field name out of the README and grepping a JSON response for it is
    // the whole point of documenting them, and a renamed field leaves the
    // guide pointing at something no endpoint answers.
    let views =
        read_doc_text(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/admin_views.rs"))
            .context("src/admin_views.rs is not readable")?;
    // Only a name the guide quotes as code IS a field name; a script path
    // (`scripts/perf/web_transfer_bench.sh`) shares the prefix and is not one.
    let mut rest: &str = readme.as_str();
    while let Some((_, after)) = rest.split_once("`web_transfer_") {
        let (token, tail) = after
            .split_once('`')
            .context("unterminated code span naming a web_transfer field")?;
        rest = tail;
        if !token
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            continue;
        }
        let field = format!("web_transfer_{token}");
        assert!(
            views.contains(&format!("pub {field}:")),
            "README names the admin field {field}, which no view publishes"
        );
    }

    // No example may carry a REAL capability. A room id, a room key and a
    // member token are the only long hex strings this section can produce,
    // and a guide that pastes one hands a live room to every reader — the
    // documented links are elided (`8f1c…#m=…&k=…`) for exactly that reason.
    let mut run = 0usize;
    for c in section.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_hexdigit() {
            run += 1;
            continue;
        }
        assert!(
            run < 32,
            "the web-transfer section quotes a {run}-character hex string: a real room id, key or token must never be pasted into the guide"
        );
        run = 0;
    }

    let anchors: Vec<String> = readme
        .lines()
        .filter_map(|line| line.strip_prefix('#'))
        .map(|line| {
            line.trim_start_matches('#')
                .trim()
                .to_ascii_lowercase()
                .chars()
                .filter_map(|c| match c {
                    'a'..='z' | '0'..='9' | '-' => Some(c),
                    ' ' => Some('-'),
                    _ => None,
                })
                .collect()
        })
        .collect();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut rest = section;
    while let Some((_, after)) = rest.split_once("](") {
        let (target, tail) = after
            .split_once(')')
            .context("unterminated markdown link in the web-transfer section")?;
        rest = tail;
        if let Some(anchor) = target.strip_prefix('#') {
            assert!(
                anchors.iter().any(|h| h == anchor),
                "README links to the missing anchor #{anchor}"
            );
        } else if !target.starts_with("http") {
            let path = root.join(target.split('#').next().unwrap_or(target));
            assert!(path.exists(), "README links to a missing file: {target}");
        }
    }

    // And the one thing no example may ever suggest: the command selects no
    // file, so it takes no path. A guide that shows one would send an operator
    // looking for a feature that does not exist and, worse, imply the CLI can
    // read their files.
    for line in readme.lines().filter(|l| l.contains("bore transfer web")) {
        let after = line
            .split_once("bore transfer web")
            .map(|(_, rest)| rest)
            .unwrap_or_default();
        // Stop at a pipe (`… | head -1`) or at the end of an inline code span:
        // beyond either, the words are prose about the command, not arguments.
        let head = after.split(['|', '`']).next().unwrap_or_default();
        let mut tokens = head.split_whitespace();
        while let Some(raw) = tokens.next() {
            let token = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
            match token {
                "" => {}
                "-t" | "--to" | "-s" | "--secret" => {
                    tokens.next();
                }
                "--insecure" | "--open" | "--help" | "-h" => {}
                other => panic!(
                    "README gives `bore transfer web` an argument it does not accept \
                     ({other:?}) — the command selects no file: {line:?}"
                ),
            }
        }
    }
    Ok(())
}

/// T-WEB-SIGNALING (Phase 4.1): two real control peers drive the direct
/// negotiation end to end — fixed roles, singleton offer/answer, the
/// 128-per-side candidate budget, both-ready commit and completion with the
/// relay budget untouched — and every refusal (wrong role, oversize SDP,
/// 129th candidate, stale attempt) leaves the transfer usable. A second
/// transfer in the same room is left unanswered and proves the deadline
/// falls back to the relay by itself.
#[tokio::test]
async fn t_web_signaling() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};
    use sha2::{Digest, Sha256};

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    // ONE carrier, and the reason is the cost of the gate rather than the
    // shape of it: the per-side ICE budget is `128 x carriers` (B-A042), so at
    // the shipped eight this loop would put 1024 control messages per side on
    // a bucket that refills at 30/s — about thirty-five seconds of the test
    // spent waiting for tokens, to prove a multiplication that three unit
    // gates already pin. What only the WIRE can prove is that the budget is
    // per side and that the marker still travels, and one carrier proves that
    // at the historical 128.
    args.direct_carriers = 1;
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let carriers_for_bounds = config.limits.direct_carriers;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x81u8; 32]);
    let owner = OwnerToken::from_bytes([0x82u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(10);
    // Long enough that 128 candidate round trips cannot race the deadline;
    // the timeout half of this test lowers it deliberately.
    registry.set_direct_deadline(Duration::from_secs(60));

    let mut a = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    a.hello(&token_hex, Some("A")).await?;
    let _ = control_msg(&a.next_text(wait).await?.expect("welcome A"));
    read_snapshot(&mut a).await?;
    let mut b = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    b.hello(&token_hex, Some("B")).await?;
    let _ = control_msg(&b.next_text(wait).await?.expect("welcome B"));
    read_snapshot(&mut b).await?;
    let (got, _) = control_msg(&a.next_text(wait).await?.expect("join on A"));
    assert_eq!(got, "peer.joined");

    let offer_hex = "1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d";
    let manifest = catalog_manifest(offer_hex, "Signal", "signal.txt");
    catalog_publish(&mut a, &"1e".repeat(16), offer_hex, &manifest).await?;
    let (got, added) = control_msg(&b.next_text(wait).await?.expect("added on B"));
    assert_eq!(got, "offer.added");
    let mac_hex = added["mac"].as_str().unwrap().to_string();
    let entry_root = added["manifest"]["entries"][0]["root"]
        .as_str()
        .expect("entry root")
        .to_string();
    let selection = serde_json::json!({
        "entryIds": ["0"],
        "manifestMac": mac_hex,
        "mode": "raw",
        "offerId": offer_hex,
    });
    let canonical = bore_cli::web_transfer_protocol::canonical_json(&selection).unwrap();
    let digest_hex = hex::encode(Sha256::digest(canonical.as_bytes()));

    // One helper for the whole test: send, read this peer's own reply.
    macro_rules! send_expect {
        ($peer:expr, $typ:expr, $rid:expr, $body:expr) => {{
            $peer
                .send_text(
                    serde_json::json!({
                        "v": 1, "type": $typ, "requestId": $rid, "body": $body,
                    })
                    .to_string(),
                )
                .await?;
            control_msg(&$peer.next_text(wait).await?.expect("reply"))
        }};
    }

    // --- the transfer reaches NegotiatingDirect -------------------------------
    let (got, ack) = send_expect!(
        b,
        "transfer.request",
        "10".repeat(16),
        serde_json::json!({
            "offerId": offer_hex, "entryIds": ["0"],
            "selectionDigest": digest_hex, "mode": "raw",
        })
    );
    assert_eq!(got, "ack", "{ack}");
    let transfer_id = ack["result"]["transferId"].as_str().unwrap().to_string();
    let (got, incoming) = control_msg(&a.next_text(wait).await?.expect("incoming"));
    assert_eq!(got, "transfer.incoming");
    let attempt_id = incoming["attemptId"].as_str().unwrap().to_string();
    let (got, _) = send_expect!(
        a,
        "transfer.source_ready",
        "11".repeat(16),
        serde_json::json!({
            "transferId": transfer_id, "attemptId": attempt_id,
            "selectionDigest": digest_hex,
        })
    );
    assert_eq!(got, "ack");
    // Roles are announced, fixed and opposite.
    let (got, start_b) = control_msg(&b.next_text(wait).await?.expect("start B"));
    assert_eq!(got, "transfer.direct_start");
    assert_eq!(start_b["role"].as_str(), Some("offerer"));
    assert_eq!(start_b["attemptNumber"].as_u64(), Some(1));
    assert_eq!(start_b["deadlineMs"].as_u64(), Some(60_000));
    let (got, start_a) = control_msg(&a.next_text(wait).await?.expect("start A"));
    assert_eq!(got, "transfer.direct_start");
    assert_eq!(start_a["role"].as_str(), Some("answerer"));
    // No relay slot is held while the direct attempt runs.
    assert_eq!(registry.current_relays(), 0);

    // --- refusals that must not corrupt the attempt ---------------------------
    let sdp_body = |sdp: &str| {
        serde_json::json!({
            "transferId": transfer_id, "attemptId": attempt_id, "sdp": sdp,
        })
    };
    // The source never offers and the recipient never answers.
    let (got, err) = send_expect!(a, "rtc.offer", "12".repeat(16), sdp_body("v=0 wrong"));
    assert_eq!(got, "error", "{err}");
    assert_eq!(err["code"].as_str(), Some("INVALID_MESSAGE"));
    let (got, err) = send_expect!(b, "rtc.answer", "13".repeat(16), sdp_body("v=0 early"));
    assert_eq!(got, "error", "{err}");
    // An oversize SDP dies in the parser, before any state is touched.
    let (got, err) = send_expect!(
        b,
        "rtc.offer",
        "14".repeat(16),
        sdp_body(&"x".repeat(64 * 1024 + 1))
    );
    assert_eq!(got, "error", "{err}");
    assert_eq!(err["code"].as_str(), Some("INVALID_MESSAGE"));
    // So does a stale attempt.
    let (got, err) = send_expect!(
        b,
        "rtc.offer",
        "15".repeat(16),
        serde_json::json!({
            "transferId": transfer_id, "attemptId": "0f".repeat(16), "sdp": "v=0",
        })
    );
    assert_eq!(got, "error", "{err}");
    // And a ready before this side's own signaling step.
    let (got, err) = send_expect!(
        b,
        "transfer.direct_ready",
        "16".repeat(16),
        serde_json::json!({"transferId": transfer_id, "attemptId": attempt_id})
    );
    assert_eq!(got, "error", "{err}");

    // --- the valid order ------------------------------------------------------
    let (got, _) = send_expect!(b, "rtc.offer", "17".repeat(16), sdp_body("v=0 real offer"));
    assert_eq!(got, "ack");
    let (got, forwarded) = control_msg(&a.next_text(wait).await?.expect("offer on A"));
    assert_eq!(got, "rtc.offer");
    assert_eq!(forwarded["sdp"].as_str(), Some("v=0 real offer"));
    // Exactly once.
    let (got, _) = send_expect!(b, "rtc.offer", "18".repeat(16), sdp_body("v=0 again"));
    assert_eq!(got, "error");
    let (got, _) = send_expect!(
        a,
        "rtc.answer",
        "19".repeat(16),
        sdp_body("v=0 real answer")
    );
    assert_eq!(got, "ack");
    let (got, forwarded) = control_msg(&b.next_text(wait).await?.expect("answer on B"));
    assert_eq!(got, "rtc.answer");
    assert_eq!(forwarded["sdp"].as_str(), Some("v=0 real answer"));

    // 128 candidates from the recipient, then the 129th is refused.
    // The per-side budget is `128 x carriers` (B-A042), so the gate asks the
    // same function the server asks: a hardcoded 128 would pass only while the
    // shipped default was one carrier, and this test deliberately runs on the
    // shipped defaults.
    let ice_budget = bore_cli::web_transfer::web_transfer_ice_budget(carriers_for_bounds);
    for n in 0..ice_budget {
        // The control bucket is 30/s with a burst of 60; pace past the burst
        // so this test measures the CANDIDATE budget and not that one.
        if n >= 50 {
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let (got, reply) = send_expect!(
            b,
            "rtc.ice",
            format!("2{n:031x}"),
            serde_json::json!({
                "transferId": transfer_id, "attemptId": attempt_id,
                "candidate": format!("candidate:{n} 1 udp"),
                "sdpMid": "0", "sdpMLineIndex": 0,
            })
        );
        assert_eq!(got, "ack", "candidate {n}: {reply}");
        let (got, seen) = control_msg(&a.next_text(wait).await?.expect("candidate on A"));
        assert_eq!(got, "rtc.ice");
        assert_eq!(
            seen["candidate"].as_str(),
            Some(format!("candidate:{n} 1 udp").as_str())
        );
    }
    let (got, err) = send_expect!(
        b,
        "rtc.ice",
        "3f".repeat(16),
        serde_json::json!({
            "transferId": transfer_id, "attemptId": attempt_id,
            "candidate": "candidate:over-budget 1 udp",
        })
    );
    assert_eq!(got, "error", "{err}");
    assert_eq!(err["code"].as_str(), Some("LIMIT_EXCEEDED"));
    // The source's own budget is untouched by the recipient spending its own,
    // and the end-of-candidates marker travels as a null candidate.
    let (got, _) = send_expect!(
        a,
        "rtc.ice",
        "40".repeat(16),
        serde_json::json!({"transferId": transfer_id, "attemptId": attempt_id})
    );
    assert_eq!(got, "ack");
    let (got, marker) = control_msg(&b.next_text(wait).await?.expect("marker on B"));
    assert_eq!(got, "rtc.ice");
    assert!(marker["candidate"].is_null());

    // --- both ready commits direct, once, recipient first ---------------------
    let ready = serde_json::json!({"transferId": transfer_id, "attemptId": attempt_id});
    let (got, _) = send_expect!(b, "transfer.direct_ready", "41".repeat(16), ready.clone());
    assert_eq!(got, "ack");
    let (got, _) = send_expect!(a, "transfer.direct_ready", "42".repeat(16), ready.clone());
    assert_eq!(got, "ack");
    for peer in [&mut b, &mut a] {
        let (got, commit) = control_msg(&peer.next_text(wait).await?.expect("path commit"));
        assert_eq!(got, "transfer.path_commit");
        assert_eq!(commit["path"].as_str(), Some("direct"));
        assert_eq!(commit["attemptId"].as_str(), Some(attempt_id.as_str()));
    }
    // A repeat does not commit a second time.
    let (got, _) = send_expect!(a, "transfer.direct_ready", "43".repeat(16), ready);
    assert_eq!(got, "error");
    assert_eq!(registry.current_relays(), 0);

    // --- path metrics group (4.4) --------------------------------------------
    // The commit alone carries nothing, so nothing is counted yet: a counter
    // that moved here would be F-12's `direct_stream_opens` again, climbing
    // while the path moved no bytes.
    assert_eq!(
        (registry.direct_carried(), registry.relay_carried()),
        (0, 0),
        "a committed path that has carried nothing is not a carried path"
    );
    // Only the RECIPIENT's report counts, and the notice the SOURCE receives
    // carries the server's own path — read off the wire, not from a log.
    let progress = serde_json::json!({
        "transferId": transfer_id, "attemptId": attempt_id, "receivedBytes": "1",
    });
    let (got, err) = send_expect!(a, "transfer.progress", "45".repeat(16), progress.clone());
    assert_eq!(got, "error", "{err}");
    assert_eq!(err["code"].as_str(), Some("INVALID_MESSAGE"));
    assert_eq!(
        (registry.direct_carried(), registry.relay_carried()),
        (0, 0)
    );
    let (got, _) = send_expect!(b, "transfer.progress", "46".repeat(16), progress);
    assert_eq!(got, "ack");
    let (got, notice) = control_msg(&a.next_text(wait).await?.expect("progress on A"));
    assert_eq!(got, "transfer.progress");
    assert_eq!(notice["path"].as_str(), Some("direct"));
    assert_eq!(notice["transferId"].as_str(), Some(transfer_id.as_str()));
    assert_eq!(
        (registry.direct_carried(), registry.relay_carried()),
        (1, 0),
        "the first verified byte on the direct path is what counts it"
    );
    // A second report on the same attempt is still one carried attempt.
    let (got, _) = send_expect!(
        b,
        "transfer.progress",
        "47".repeat(16),
        serde_json::json!({
            "transferId": transfer_id, "attemptId": attempt_id, "receivedBytes": "2",
        })
    );
    assert_eq!(got, "ack");
    let (got, _) = control_msg(&a.next_text(wait).await?.expect("progress 2 on A"));
    assert_eq!(got, "transfer.progress");
    assert_eq!(
        (registry.direct_carried(), registry.relay_carried()),
        (1, 0)
    );

    // The direct transfer completes, and no relay slot was ever taken.
    let (got, _) = send_expect!(
        b,
        "transfer.complete",
        "44".repeat(16),
        serde_json::json!({
            "transferId": transfer_id, "attemptId": attempt_id, "root": entry_root,
        })
    );
    assert_eq!(got, "ack");
    let (got, completed) = control_msg(&a.next_text(wait).await?.expect("completed on A"));
    assert_eq!(got, "transfer.completed");
    assert_eq!(completed["transferId"].as_str(), Some(transfer_id.as_str()));
    assert_eq!(registry.current_relays(), 0);

    // --- the deadline falls back by itself ------------------------------------
    registry.set_direct_deadline(Duration::from_millis(300));
    let offer2 = "2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d";
    let manifest2 = catalog_manifest(offer2, "Silent", "silent.txt");
    catalog_publish(&mut a, &"2e".repeat(16), offer2, &manifest2).await?;
    let (got, added2) = control_msg(&b.next_text(wait).await?.expect("added2 on B"));
    assert_eq!(got, "offer.added");
    let mac2 = added2["mac"].as_str().unwrap().to_string();
    let selection2 = serde_json::json!({
        "entryIds": ["0"], "manifestMac": mac2, "mode": "raw", "offerId": offer2,
    });
    let digest2 = hex::encode(Sha256::digest(
        bore_cli::web_transfer_protocol::canonical_json(&selection2)
            .unwrap()
            .as_bytes(),
    ));
    let (got, ack2) = send_expect!(
        b,
        "transfer.request",
        "50".repeat(16),
        serde_json::json!({
            "offerId": offer2, "entryIds": ["0"],
            "selectionDigest": digest2, "mode": "raw",
        })
    );
    assert_eq!(got, "ack", "{ack2}");
    let transfer2 = ack2["result"]["transferId"].as_str().unwrap().to_string();
    let (got, incoming2) = control_msg(&a.next_text(wait).await?.expect("incoming2"));
    assert_eq!(got, "transfer.incoming");
    let attempt2 = incoming2["attemptId"].as_str().unwrap().to_string();
    let (got, _) = send_expect!(
        a,
        "transfer.source_ready",
        "51".repeat(16),
        serde_json::json!({
            "transferId": transfer2, "attemptId": attempt2, "selectionDigest": digest2,
        })
    );
    assert_eq!(got, "ack");
    let (got, start) = control_msg(&b.next_text(wait).await?.expect("start2 B"));
    assert_eq!(got, "transfer.direct_start");
    assert_eq!(start["deadlineMs"].as_u64(), Some(300));
    let (got, _) = control_msg(&a.next_text(wait).await?.expect("start2 A"));
    assert_eq!(got, "transfer.direct_start");
    // Nobody answers. The server falls back on its own, once, with a fixed
    // reason code and a fresh attempt on which the tickets are issued.
    for peer in [&mut b, &mut a] {
        let (got, notice) = control_msg(&peer.next_text(wait).await?.expect("timeout notice"));
        assert_eq!(got, "transfer.direct_failed");
        assert_eq!(notice["reason"].as_str(), Some("timeout"));
        assert_eq!(notice["transferId"].as_str(), Some(transfer2.as_str()));
        let (got, ticket) = control_msg(&peer.next_text(wait).await?.expect("relay ticket"));
        assert_eq!(got, "transfer.relay_ticket");
        assert_eq!(ticket["transferId"].as_str(), Some(transfer2.as_str()));
        assert_ne!(
            ticket["attemptId"].as_str(),
            Some(attempt2.as_str()),
            "the fallback mints a fresh attempt"
        );
    }
    assert_eq!(registry.current_relays(), 1, "exactly one relay slot taken");
    // A late failure naming the dead direct attempt is acked and ignored: it
    // must not touch the relay attempt that replaced it.
    let (got, _) = send_expect!(
        a,
        "transfer.direct_failed",
        "52".repeat(16),
        serde_json::json!({
            "transferId": transfer2, "attemptId": attempt2, "reason": "ice-failed",
        })
    );
    assert_eq!(got, "ack");
    assert_eq!(registry.current_relays(), 1);
    // The fallback took a relay SLOT and carried nothing: the relay counter
    // follows verified bytes, never admission.
    assert_eq!(
        (registry.direct_carried(), registry.relay_carried()),
        (1, 0),
        "an admitted relay attempt that carried no byte is not a carried path"
    );
    lease.close_explicit(&registry);
    Ok(())
}

// ---------------------------------------------------------------------------
// Folder manifest group — server-side validation of trees and selections
// (plan 001, sub-phase 5.1). These are pure: they parse manifests and check
// selections, with no server and no ports, because that is exactly the surface
// the server is allowed to have an opinion about — it never holds payload.
// ---------------------------------------------------------------------------

/// Builds a folder manifest: one directory entry (null root, zero size) and
/// `files` file entries, canonical, sorted, sequential IDs from 0.
fn folder_manifest_value(files: &[(&str, u64)]) -> serde_json::Value {
    use bore_cli::web_transfer_protocol::file_root;
    let mut entries = vec![serde_json::json!({
        "id": "0",
        "path": "tree/empty",
        "size": "0",
        "mtime": "0",
        "chunks": [],
        "chunkCount": "0",
        "root": serde_json::Value::Null,
    })];
    for (position, (path, size)) in files.iter().enumerate() {
        let chunk_count = size.div_ceil(1024 * 1024);
        let leaves: Vec<[u8; 32]> = (0..chunk_count)
            .map(|index| {
                let mut leaf = [0u8; 32];
                leaf[0] = position as u8;
                leaf[1] = index as u8;
                leaf
            })
            .collect();
        let root = hex::encode(file_root(chunk_count, &leaves).unwrap());
        entries.push(serde_json::json!({
            "id": (position + 1).to_string(),
            "path": path,
            "size": size.to_string(),
            "mtime": "1757779200",
            "chunks": leaves.iter().map(hex::encode).collect::<Vec<_>>(),
            "chunkCount": chunk_count.to_string(),
            "root": root,
        }));
    }
    serde_json::json!({
        "offer": "0".repeat(32),
        "mode": "multi",
        "label": "tree",
        "kind": "folder",
        "chunkSize": "1048576",
        "createdAt": "2026-09-14T12:00:00Z",
        "entries": entries,
    })
}

/// `reserved_zip_entry_id_is_rejected_in_manifest`: `0xffff_ffff` belongs to
/// the archive a `zip` transfer sends, so a manifest can never name it — and
/// it is refused AS the reserved ID, not as a numbering mistake, because the
/// two say different things about the manifest that arrived.
#[test]
fn reserved_zip_entry_id_is_rejected_in_manifest() {
    use bore_cli::web_transfer::WebTransferLimits;
    use bore_cli::web_transfer_protocol::{parse_manifest, RESERVED_ZIP_ENTRY_ID};
    let limits = WebTransferLimits::default();

    // A well-formed tree is accepted, so the rejection below is about the ID.
    let ok = folder_manifest_value(&[("tree/f1.bin", 3), ("tree/f2.bin", 1_500_000)]);
    let manifest = parse_manifest(&ok, &limits).expect("a canonical folder manifest parses");
    assert_eq!(manifest.entries.len(), 3);
    assert!(
        manifest.entries[0].root.is_none(),
        "the directory has no root"
    );

    let mut reserved = ok.clone();
    reserved["entries"][0]["id"] = serde_json::Value::String(RESERVED_ZIP_ENTRY_ID.to_string());
    let error = parse_manifest(&reserved, &limits)
        .expect_err("a manifest naming the reserved archive ID must be refused")
        .to_string();
    assert!(
        error.contains("reserved"),
        "the reserved ID must be refused as itself, got {error:?}"
    );
}

/// `raw_and_zip_selection_sets_are_exact`: raw is exactly one FILE, zip is
/// exactly the whole manifest — directories included, each one once. The set
/// is what is checked, never the array order: the wire already pins that to
/// the canonical lexicographic order, while the archive writes its entries in
/// manifest order.
#[test]
fn raw_and_zip_selection_sets_are_exact() {
    use bore_cli::web_transfer::WebTransferLimits;
    use bore_cli::web_transfer_protocol::{parse_manifest, validate_selection, Selection};
    let limits = WebTransferLimits::default();
    let value = folder_manifest_value(&[("tree/f1.bin", 3), ("tree/f2.bin", 1_500_000)]);
    let manifest = parse_manifest(&value, &limits).unwrap();
    let ids = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    // raw: exactly one file.
    assert_eq!(
        validate_selection("raw", &ids(&["1"]), &manifest).unwrap(),
        Selection::Raw(1)
    );
    for (selection, why) in [
        (vec!["0"], "a directory has no bytes to send"),
        (vec!["1", "2"], "raw is one entry"),
        (vec![], "raw is one entry"),
        (vec!["7"], "an entry that is not in the manifest"),
        (vec!["01"], "a non-canonical decimal"),
        (vec!["4294967295"], "the reserved archive ID"),
    ] {
        assert!(
            validate_selection("raw", &ids(&selection), &manifest).is_err(),
            "raw must refuse {selection:?}: {why}"
        );
    }

    // zip: exactly the whole manifest, in any array order.
    assert_eq!(
        validate_selection("zip", &ids(&["0", "1", "2"]), &manifest).unwrap(),
        Selection::Zip
    );
    assert_eq!(
        validate_selection("zip", &ids(&["2", "0", "1"]), &manifest).unwrap(),
        Selection::Zip
    );
    for (selection, why) in [
        (vec!["0", "1"], "a subset is not the whole offer"),
        (
            vec!["0", "1", "1"],
            "the same entry twice is not the whole offer",
        ),
        (vec!["0", "1", "2", "2"], "more IDs than the manifest holds"),
        (vec!["0", "1", "7"], "an entry that is not in the manifest"),
    ] {
        assert!(
            validate_selection("zip", &ids(&selection), &manifest).is_err(),
            "zip must refuse {selection:?}: {why}"
        );
    }

    // No other mode exists.
    for mode in ["", "RAW", "tar", "single"] {
        assert!(
            validate_selection(mode, &ids(&["1"]), &manifest).is_err(),
            "mode {mode:?} must be refused"
        );
    }
}

// ---------------------------------------------------------------------------
// Sub-phase 6.1 — resource acceptance: descriptors, fairness and soak
// ---------------------------------------------------------------------------

/// Soft and hard `RLIMIT_NOFILE` of another process, read from the kernel.
///
/// P-12's rule: the log line only proves the server TALKED about the limit,
/// `/proc/<pid>/limits` proves it has one.
fn max_open_files(pid: u32) -> Option<(u64, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/limits")).ok()?;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("Max open files") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let soft = fields.next()?;
        let hard = fields.next()?;
        let parse = |field: &str| -> Option<u64> {
            if field == "unlimited" {
                Some(u64::MAX)
            } else {
                field.parse().ok()
            }
        };
        return Some((parse(soft)?, parse(hard)?));
    }
    None
}

/// `T-WEB-FDBUDGET`: a REAL server process, started under chosen soft and
/// hard descriptor limits, raises its soft limit to cover the browser surface
/// as well as `--max-conns`, and says so in a way the operator can act on
/// when the hard limit is too low to reach.
///
/// The arithmetic is the one thing a unit test already pins; what it cannot
/// pin is that the process ACTUALLY has the limit, which is exactly the shape
/// P-12 was found in (`conn_rejections` stayed 0 while the kernel refused
/// with `EMFILE`). The third arm is the zero-regression half: a server with
/// no browser surface must keep the limit it has always asked for.
#[tokio::test]
async fn t_web_fdbudget() -> Result<()> {
    if !proc_fs_available() {
        println!("N/A T-WEB-FDBUDGET reads the kernel's own view in /proc/<pid>/limits");
        return Ok(());
    }
    let binary = std::env::var("CARGO_BIN_EXE_bore")
        .context("CARGO_BIN_EXE_bore is not available for the descriptor-budget gate")?;
    // The defaults the server derives its web budget from.
    let limits = bore_cli::web_transfer::WebTransferLimits::default();
    let web_fds = bore_cli::fdlimit::web_transfer_fds(
        limits.max_rooms,
        limits.max_peers_global,
        limits.max_relays_global,
    );
    const MAX_CONNS: u64 = 64;
    let headroom = bore_cli::fdlimit::FD_HEADROOM;

    /// One server under a chosen `ulimit` pair; returns its pid, limits and log.
    async fn arm(
        binary: &str,
        port: u16,
        soft: u64,
        hard: u64,
        web: bool,
    ) -> Result<((u64, u64), String, tokio::process::Child)> {
        let log_path = std::env::temp_dir().join(format!("bore-web-fdbudget-{port}.log"));
        let _ = std::fs::remove_file(&log_path);
        let log = std::fs::File::create(&log_path)?;
        let log_err = log.try_clone()?;
        let web_flags = if web {
            format!("--web-transfer-base-url 'http://127.0.0.1:{port}/'")
        } else {
            String::new()
        };
        // `ulimit -n N` would set BOTH limits, and `setrlimit` refuses a hard
        // limit below the current soft limit: lower the soft limit first.
        let script = format!(
            "ulimit -Sn 64; ulimit -Hn {hard}; ulimit -Sn {soft}; \
             exec '{binary}' server --control-port {port} --max-conns {MAX_CONNS} {web_flags}"
        );
        let child = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("RUST_LOG", "bore_cli=debug,info")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .kill_on_drop(true)
            .spawn()
            .context("spawning the server under a chosen descriptor limit")?;
        let pid = child.id().context("server pid")?;
        support::wait_port(port, true).await;
        let limits = max_open_files(pid).context("/proc/<pid>/limits has no Max open files")?;
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        Ok((limits, log, child))
    }

    // Arm 1 — a hard limit with room: the soft limit reaches exactly
    // `--max-conns` + the web surface + the module's own headroom.
    let port = support::free_port().await?;
    let wanted = MAX_CONNS + web_fds + headroom;
    let (limits_1, _log_1, _child_1) = arm(&binary, port, 256, 65536, true).await?;
    assert_eq!(
        limits_1.0, wanted,
        "the soft limit must cover max_conns {MAX_CONNS} + web {web_fds} + headroom {headroom}"
    );
    assert_eq!(limits_1.1, 65536, "the hard limit must not move");

    // Arm 2 — a hard limit that is itself short: raised to the ceiling, and
    // the advisory names the web share so the operator lowers the right knob.
    let port = support::free_port().await?;
    let short_hard = 2048;
    let (limits_2, log_2, _child_2) = arm(&binary, port, 256, short_hard, true).await?;
    assert_eq!(
        limits_2.0, short_hard,
        "a short hard limit must still raise the soft limit to the ceiling"
    );
    assert!(
        log_2.contains("web-transfer surface"),
        "the advisory must name the web share; got: {log_2}"
    );
    assert!(
        log_2.contains("--web-transfer-max-rooms"),
        "the advisory must name the knobs that lower the web share; got: {log_2}"
    );

    // Arm 3 — no browser surface, no inflation: the historical budget exactly.
    let port = support::free_port().await?;
    let (limits_3, _log_3, _child_3) = arm(&binary, port, 256, 65536, false).await?;
    assert_eq!(
        limits_3.0,
        MAX_CONNS + headroom,
        "a server with no web transfer must not raise its limit for a surface it does not serve"
    );
    Ok(())
}

/// One control read that skips whatever else is queued and refuses an error.
async fn expect_type(
    peer: &mut support::WsPeer,
    typ: &str,
    wait: Duration,
) -> Result<serde_json::Value> {
    loop {
        let text = peer
            .next_text(wait)
            .await?
            .with_context(|| format!("control closed before {typ}"))?;
        let (got, body) = control_msg(&text);
        if got == typ {
            return Ok(body);
        }
        anyhow::ensure!(got != "error", "control error waiting for {typ}: {body}");
    }
}

/// Builds one room with a source and a recipient peer, publishes one offer,
/// takes a transfer to the relay and attaches both legs. Returns the pieces a
/// load test drives: the lease (kept alive by the caller), both control peers
/// and both relay legs.
#[allow(clippy::type_complexity)]
async fn relay_pair_room(
    registry: &std::sync::Arc<bore_cli::web_transfer::WebTransferRegistry>,
    host: &str,
    origin: &str,
    seed: u8,
    offer_hex: &str,
) -> Result<(
    bore_cli::web_transfer::OwnerLease,
    support::WsPeer,
    support::WsPeer,
    support::RelayLeg,
    support::RelayLeg,
    String,
)> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};
    use sha2::{Digest, Sha256};
    let wait = Duration::from_secs(30);

    let member = MemberToken::from_bytes([seed; 32]);
    let owner = OwnerToken::from_bytes([seed ^ 0xff; 32]);
    let lease = OwnerLease::create(registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();

    let mut source = support::WsPeer::connect(host, &room_hex, origin).await?;
    source.hello(&token_hex, Some("S")).await?;
    let welcome = expect_type(&mut source, "welcome", wait).await?;
    let peer_source = welcome["peerId"].as_str().unwrap().to_string();
    read_snapshot(&mut source).await?;
    let mut recipient = support::WsPeer::connect(host, &room_hex, origin).await?;
    recipient.hello(&token_hex, Some("R")).await?;
    let welcome = expect_type(&mut recipient, "welcome", wait).await?;
    let peer_recipient = welcome["peerId"].as_str().unwrap().to_string();
    read_snapshot(&mut recipient).await?;

    // The source learns about the recipient before it publishes; leaving that
    // event queued would make the publish helper read it as its own ack.
    let _ = expect_type(&mut source, "peer.joined", wait).await?;

    let manifest = catalog_manifest(offer_hex, "Load", "load.bin");
    catalog_publish(
        &mut source,
        &format!("{seed:02x}").repeat(16),
        offer_hex,
        &manifest,
    )
    .await?;
    let added = expect_type(&mut recipient, "offer.added", wait).await?;
    let mac_hex = added["mac"].as_str().unwrap().to_string();
    let selection = serde_json::json!({
        "entryIds": ["0"],
        "manifestMac": mac_hex,
        "mode": "raw",
        "offerId": offer_hex,
    });
    let digest_hex = hex::encode(Sha256::digest(
        bore_cli::web_transfer_protocol::canonical_json(&selection)
            .unwrap()
            .as_bytes(),
    ));

    recipient
        .send_text(
            serde_json::json!({
                "v": 1, "type": "transfer.request",
                "requestId": format!("{:032x}", u128::from(seed) + 1),
                "body": { "offerId": offer_hex, "entryIds": ["0"],
                          "selectionDigest": digest_hex, "mode": "raw" },
            })
            .to_string(),
        )
        .await?;
    let ack = expect_type(&mut recipient, "ack", wait).await?;
    let transfer_id = ack["result"]["transferId"].as_str().unwrap().to_string();
    let incoming = expect_type(&mut source, "transfer.incoming", wait).await?;
    let attempt_id = incoming["attemptId"].as_str().unwrap().to_string();
    source
        .send_text(
            serde_json::json!({
                "v": 1, "type": "transfer.source_ready",
                "requestId": format!("{:032x}", u128::from(seed) + 2),
                "body": { "transferId": transfer_id, "attemptId": attempt_id,
                          "selectionDigest": digest_hex },
            })
            .to_string(),
        )
        .await?;
    let _ = expect_type(&mut source, "ack", wait).await?;
    support::decline_direct(
        &mut source,
        &mut recipient,
        &transfer_id,
        &attempt_id,
        &format!("{:032x}", u128::from(seed) + 3),
        wait,
    )
    .await?;
    let ticket_source = expect_type(&mut source, "transfer.relay_ticket", wait).await?;
    let ticket_recipient = expect_type(&mut recipient, "transfer.relay_ticket", wait).await?;
    let attempt_id = ticket_source["attemptId"].as_str().unwrap().to_string();

    let mut leg_source = support::RelayLeg::connect(host, &room_hex, &transfer_id, origin).await?;
    leg_source
        .send_text(
            serde_json::json!({
                "v": 1, "peerId": peer_source, "transferId": transfer_id,
                "attemptId": attempt_id, "role": "source",
                "ticket": ticket_source["ticket"].as_str().unwrap(),
            })
            .to_string(),
        )
        .await?;
    let mut leg_recipient =
        support::RelayLeg::connect(host, &room_hex, &transfer_id, origin).await?;
    leg_recipient
        .send_text(
            serde_json::json!({
                "v": 1, "peerId": peer_recipient, "transferId": transfer_id,
                "attemptId": attempt_id, "role": "recipient",
                "ticket": ticket_recipient["ticket"].as_str().unwrap(),
            })
            .to_string(),
        )
        .await?;
    let _ = expect_type(&mut source, "transfer.path_commit", wait).await?;
    let _ = expect_type(&mut recipient, "transfer.path_commit", wait).await?;
    Ok((
        lease,
        source,
        recipient,
        leg_source,
        leg_recipient,
        transfer_id,
    ))
}

/// `T-WEB-FAIRNESS`: a room being throttled pays for its own traffic and for
/// nobody else's.
///
/// The relay rate is a per-room token bucket, so two rooms saturating the
/// relay at the same time must each move their own budget — and the room that
/// is being throttled must not hold anything the other room's pump needs. A
/// shared bucket, or a throttle taken on the registry, shows up here as both
/// rooms taking the SERIALIZED time instead of their own. The control plane
/// is measured in the same window, because a relay that blocks a runtime
/// worker while it sleeps would slow the answer to a `ping` first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_web_fairness() -> Result<()> {
    // 2 MiB/s with the product's own burst (2x the rate) and 12 MiB per room:
    // a room that pays only for itself needs (12 - 4) / 2 = 4 s, two rooms
    // sharing one budget need (24 - 4) / 2 = 10 s. The threshold sits between
    // them with room to spare on either side.
    const RATE: u64 = 2 * 1024 * 1024;
    const PAYLOAD: u64 = 12 * 1024 * 1024;
    const FRAME_BODY: usize = 24 * 1024 + 16;

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    args.relay_rate_bytes_per_s = RATE;
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let (_lease_a, _src_a, _rcp_a, leg_src_a, leg_rcp_a, _id_a) = relay_pair_room(
        &registry,
        &host,
        &origin,
        0x41,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .await?;
    let (_lease_b, _src_b, mut rcp_b, leg_src_b, leg_rcp_b, _id_b) = relay_pair_room(
        &registry,
        &host,
        &origin,
        0x42,
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )
    .await?;

    async fn pump(
        mut source: support::RelayLeg,
        mut recipient: support::RelayLeg,
        total: u64,
    ) -> Result<Duration> {
        let frames = (total as usize).div_ceil(FRAME_BODY);
        let started = std::time::Instant::now();
        let sender = tokio::spawn(async move {
            let payload = vec![0x5au8; FRAME_BODY - 16];
            for seq in 0..frames as u32 {
                source.send_binary(relay_test_frame(seq, &payload)).await?;
            }
            source.close().await?;
            anyhow::Result::<_>::Ok(())
        });
        let mut received = 0u64;
        while let Some(message) = recipient.next_msg(Duration::from_secs(60)).await? {
            match message {
                tokio_tungstenite::tungstenite::Message::Binary(frame) => {
                    received += frame.len() as u64
                }
                tokio_tungstenite::tungstenite::Message::Close(_) => break,
                _ => anyhow::bail!("unexpected relay message"),
            }
        }
        let elapsed = started.elapsed();
        sender.await??;
        anyhow::ensure!(received >= total, "relay delivered {received} of {total}");
        Ok(elapsed)
    }

    // V-9's rule: an absolute millisecond budget describes the MACHINE, not
    // the server. Sample this machine's own idle control round trip first, on
    // the very socket the loaded probe will use — it is what tells the reader
    // of a failure whether the loaded numbers below are the server's or the
    // runner's.
    let mut idle_rtt = Duration::ZERO;
    for _ in 0..3 {
        let started = std::time::Instant::now();
        rcp_b
            .send_text(r#"{"v":1,"type":"ping","body":{}}"#.to_string())
            .await?;
        let _ = expect_type(&mut rcp_b, "pong", Duration::from_secs(10)).await?;
        idle_rtt = idle_rtt.max(started.elapsed());
    }

    // Both rooms pump at once, and room B's control plane is probed while
    // they do: the question is whether either room waits for the other.
    let pump_a = tokio::spawn(pump(leg_src_a, leg_rcp_a, PAYLOAD));
    let pump_b = tokio::spawn(pump(leg_src_b, leg_rcp_b, PAYLOAD));
    let mut rtts: Vec<Duration> = Vec::with_capacity(10);
    for _ in 0..10 {
        let started = std::time::Instant::now();
        rcp_b
            .send_text(r#"{"v":1,"type":"ping","body":{}}"#.to_string())
            .await?;
        let _ = expect_type(&mut rcp_b, "pong", Duration::from_secs(10)).await?;
        rtts.push(started.elapsed());
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let worst_rtt = rtts.iter().copied().max().unwrap_or_default();
    let median_rtt = {
        let mut sorted = rtts.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    };
    let elapsed_a = pump_a.await??;
    let elapsed_b = pump_b.await??;

    // V-11's rule: a harness that prints a summary statistic MUST also print
    // the raw samples, or a failure is unreadable.
    println!(
        "FAIRNESS a={:.2}s b={:.2}s control-rtt median={:.0}ms worst={:.0}ms idle={:.1}ms samples={:?}",
        elapsed_a.as_secs_f64(),
        elapsed_b.as_secs_f64(),
        median_rtt.as_secs_f64() * 1000.0,
        worst_rtt.as_secs_f64() * 1000.0,
        idle_rtt.as_secs_f64() * 1000.0,
        rtts.iter().map(|r| r.as_millis()).collect::<Vec<_>>()
    );
    // The throttle must actually be in force, or the comparison below would
    // pass on a server that never limited anything.
    assert!(
        elapsed_a >= Duration::from_millis(2500) && elapsed_b >= Duration::from_millis(2500),
        "the per-room rate was not applied: a={elapsed_a:?} b={elapsed_b:?}"
    );
    // ... and neither room may pay for the other's bytes.
    assert!(
        elapsed_a < Duration::from_secs(7),
        "room A paid for room B's budget: {elapsed_a:?}"
    );
    assert!(
        elapsed_b < Duration::from_secs(7),
        "room B paid for room A's budget: {elapsed_b:?}"
    );
    // What must be excluded is the control plane WAITING for the relay, and
    // that defect has an arithmetic of its own: a ping issued at t is answered
    // when the relay ends at T, so it costs T - t, and ten of them spaced
    // 200 ms from t = 0 give a MEDIAN of about 0.55 * T and a worst of about
    // T. A healthy plane answers in microseconds. Only the SCHEDULER of a
    // loaded shared runner stretches a round trip, and it stretches SAMPLES,
    // never the median of ten — so the median carries three orders of
    // magnitude of margin while the worst stays as the literal defect shape.
    //
    // V-9, learned on this very assertion: the previous bound was
    // `elapsed_b / 3`, which LOOKS like a ratio and is not one. The relay is
    // throttle-bound, so `elapsed_b` is a configured constant and dividing it
    // produced an absolute millisecond budget in disguise. macos-14 read
    // 1.747 s against a 1.347 s budget while the SAME run's relay took
    // 4.040 s — the control plane was 2.3x faster than the thing it stood
    // accused of waiting for, and one stretched sample out of ten decided it.
    assert!(
        median_rtt < elapsed_b / 4,
        "the control plane of a room relaying under a throttle answered at a median of \
         {median_rtt:?} (bound {:?}, relay {elapsed_b:?}, idle {idle_rtt:?}, samples {rtts:?})",
        elapsed_b / 4
    );
    assert!(
        worst_rtt < elapsed_b,
        "a single control round trip waited out the whole relay: {worst_rtt:?} \
         (relay {elapsed_b:?}, idle {idle_rtt:?}, samples {rtts:?})"
    );
    Ok(())
}

/// One relay transfer, from the request to both legs attached, driven over
/// peers whose sockets are drained in the background (6.1 soak).
#[allow(clippy::too_many_arguments)]
async fn soak_transfer(
    host: &str,
    origin: &str,
    room_hex: &str,
    source: &mut support::PumpedPeer,
    recipient: &mut support::PumpedPeer,
    peer_source: &str,
    peer_recipient: &str,
    offer_hex: &str,
    mac_hex: &str,
    nonce: u128,
) -> Result<(String, support::RelayLeg, support::RelayLeg)> {
    use sha2::{Digest, Sha256};
    let wait = Duration::from_secs(30);
    let selection = serde_json::json!({
        "entryIds": ["0"],
        "manifestMac": mac_hex,
        "mode": "raw",
        "offerId": offer_hex,
    });
    let digest_hex = hex::encode(Sha256::digest(
        bore_cli::web_transfer_protocol::canonical_json(&selection)
            .unwrap()
            .as_bytes(),
    ));
    recipient
        .send_text(
            serde_json::json!({
                "v": 1, "type": "transfer.request", "requestId": format!("{nonce:032x}"),
                "body": { "offerId": offer_hex, "entryIds": ["0"],
                          "selectionDigest": digest_hex, "mode": "raw" },
            })
            .to_string(),
        )
        .await?;
    let ack = recipient.expect("ack", wait).await?;
    let transfer_id = ack["result"]["transferId"]
        .as_str()
        .context("ack carries no transferId")?
        .to_string();
    let incoming = source.expect("transfer.incoming", wait).await?;
    let attempt_id = incoming["attemptId"].as_str().unwrap().to_string();
    source
        .send_text(
            serde_json::json!({
                "v": 1, "type": "transfer.source_ready",
                "requestId": format!("{:032x}", nonce + 1),
                "body": { "transferId": transfer_id, "attemptId": attempt_id,
                          "selectionDigest": digest_hex },
            })
            .to_string(),
        )
        .await?;
    let _ = source.expect("ack", wait).await?;
    // The soak runs the RELAY, so the direct attempt is declined exactly as a
    // browser with no usable DataChannel declines it.
    let _ = recipient.expect("transfer.direct_start", wait).await?;
    let _ = source.expect("transfer.direct_start", wait).await?;
    source
        .send_text(
            serde_json::json!({
                "v": 1, "type": "transfer.direct_failed",
                "requestId": format!("{:032x}", nonce + 2),
                "body": { "transferId": transfer_id, "attemptId": attempt_id,
                          "reason": "unsupported" },
            })
            .to_string(),
        )
        .await?;
    let _ = source.expect("ack", wait).await?;
    let ticket_source = source.expect("transfer.relay_ticket", wait).await?;
    let ticket_recipient = recipient.expect("transfer.relay_ticket", wait).await?;
    let attempt_id = ticket_source["attemptId"].as_str().unwrap().to_string();

    let mut leg_source = support::RelayLeg::connect(host, room_hex, &transfer_id, origin).await?;
    leg_source
        .send_text(
            serde_json::json!({
                "v": 1, "peerId": peer_source, "transferId": transfer_id,
                "attemptId": attempt_id, "role": "source",
                "ticket": ticket_source["ticket"].as_str().unwrap(),
            })
            .to_string(),
        )
        .await?;
    let mut leg_recipient =
        support::RelayLeg::connect(host, room_hex, &transfer_id, origin).await?;
    leg_recipient
        .send_text(
            serde_json::json!({
                "v": 1, "peerId": peer_recipient, "transferId": transfer_id,
                "attemptId": attempt_id, "role": "recipient",
                "ticket": ticket_recipient["ticket"].as_str().unwrap(),
            })
            .to_string(),
        )
        .await?;
    let _ = source.expect("transfer.path_commit", wait).await?;
    let _ = recipient.expect("transfer.path_commit", wait).await?;
    Ok((transfer_id, leg_source, leg_recipient))
}

/// `T-WEB-SOAK`: the exact load of the plan — 32 control peers, 64 offers
/// each, 32 concurrent relays with half of them cancelled and resumed —
/// against a REAL server process whose resident memory is read from the
/// kernel while it runs.
///
/// The server is started at EXACTLY the load's size (32 peers, 32 relays, 2
/// rooms), which is what turns the tail of this test into a proof: after the
/// room closes, a fresh room admitting 32 peers and a relay again can only
/// happen if every permit came back. At the default limits the same tail
/// would prove nothing, because there would be thousands of spare permits to
/// hide a leak in.
///
/// `BORE_WEB_SOAK_SECS` sets the pumping window (default 15 s here; the
/// scripted run in `scripts/web_transfer_e2e.sh` uses the plan's 300 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t_web_soak() -> Result<()> {
    // Three of this soak's instruments are Linux facilities: resident memory
    // and the descriptor list come from `/proc`, and every peer dials from
    // its OWN loopback address (127.0.0.2 and up) because the pre-auth
    // limiter is per IP and 32 peers off one address is exactly what it
    // exists to refuse. macOS binds only 127.0.0.1 to `lo0`, so there the
    // experiment cannot be set up at all — and a gate that cannot run must
    // say so rather than measure something else quietly.
    if !cfg!(target_os = "linux") {
        println!(
            "N/A T-WEB-SOAK needs /proc and a whole 127.0.0.0/8 loopback to give each peer its own IP"
        );
        return Ok(());
    }
    const PEERS: usize = 32;
    const OFFERS_PER_PEER: usize = 64;
    const TRANSFERS: usize = 32;
    const FRAME_BODY: usize = 24 * 1024 + 16;
    const MAC: &str = "ab";
    let window = Duration::from_secs(
        std::env::var("BORE_WEB_SOAK_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15),
    );
    let binary = std::env::var_os("CARGO_BIN_EXE_bore")
        .context("CARGO_BIN_EXE_bore is not available for the soak")?;
    let port = support::free_port().await?;
    let log_path = std::env::temp_dir().join(format!("bore-web-soak-{port}.log"));
    let _ = std::fs::remove_file(&log_path);
    let log = std::fs::File::create(&log_path)?;
    let log_err = log.try_clone()?;
    let mut server = tokio::process::Command::new(&binary)
        .arg("server")
        .arg("--control-port")
        .arg(port.to_string())
        .arg("--web-transfer-base-url")
        .arg(format!("http://127.0.0.1:{port}/"))
        // The throttle is a product feature the fairness gate owns; a soak
        // measures what the server HOLDS, so it runs unthrottled.
        .arg("--web-transfer-relay-rate")
        .arg("0")
        .arg("--web-transfer-max-rooms")
        .arg("2")
        .arg("--web-transfer-max-peers")
        .arg(PEERS.to_string())
        .arg("--web-transfer-max-peers-per-room")
        .arg(PEERS.to_string())
        .arg("--web-transfer-max-relays")
        .arg(TRANSFERS.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .kill_on_drop(true)
        .spawn()
        .context("spawning the soak server")?;
    let server_pid = server.id().context("server pid")?;
    support::wait_port(port, true).await;
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(30);

    /// Opens a room through the real CLI and returns (owner, room, token).
    async fn open_room(
        binary: &std::ffi::OsString,
        port: u16,
    ) -> Result<(tokio::process::Child, String, String)> {
        let mut owner = tokio::process::Command::new(binary)
            .arg("transfer")
            .arg("web")
            .arg("--to")
            .arg(format!("127.0.0.1:{port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("spawning bore transfer web")?;
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(
            owner.stdout.take().context("owner stdout")?,
        ));
        let first = next_line(&mut lines, Duration::from_secs(20))
            .await?
            .context("room line")?;
        let url = first
            .strip_prefix("room: ")
            .context("first line is `room: <url>`")?
            .to_string();
        let (room_hex, token_hex) = split_room_url(&url)?;
        Ok((owner, room_hex, token_hex))
    }

    let (mut owner, room_hex, token_hex) = open_room(&binary, port).await?;
    let rss_before = rss_kib_of(server_pid);
    let fds_before = fd_targets(server_pid).len();

    // --- 32 control peers ---------------------------------------------------
    // Only the envelopes an assertion reads are kept; the catalog flood is
    // read off the socket and counted (see `PumpedPeer`).
    const KEEP: &[&str] = &[
        "ack",
        "transfer.incoming",
        "transfer.direct_start",
        "transfer.relay_ticket",
        "transfer.path_commit",
        "pong",
    ];
    let mut peers = Vec::with_capacity(PEERS);
    let mut peer_ids = Vec::with_capacity(PEERS);
    // Each peer dials from its own loopback address: the pre-auth limiter is
    // per IP and 32 peers off one address is what it exists to refuse.
    let peer_ip = |index: usize| std::net::Ipv4Addr::new(127, 0, 0, 2 + index as u8);
    for index in 0..PEERS {
        let mut peer =
            support::WsPeer::connect_from(peer_ip(index), &host, &room_hex, &origin).await?;
        peer.hello(&token_hex, Some(&format!("P{index}"))).await?;
        let welcome = expect_type(&mut peer, "welcome", wait).await?;
        peer_ids.push(welcome["peerId"].as_str().unwrap().to_string());
        read_snapshot(&mut peer).await?;
        peers.push(peer.into_pumped_keeping(KEEP));
    }

    // --- 64 small offers per peer ------------------------------------------
    // Paced round-robin: the per-peer mutation bucket is 4/s (burst 8), so a
    // tighter loop would measure the rate limiter rather than the server.
    let offer_id = |peer: usize, offer: usize| format!("{peer:016x}{offer:016x}");
    for offer in 0..OFFERS_PER_PEER {
        for (index, peer) in peers.iter_mut().enumerate() {
            let hex = offer_id(index, offer);
            let manifest = catalog_manifest(&hex, "Soak", "soak.bin");
            peer.send_text(
                serde_json::json!({
                    "v": 1, "type": "offer.publish",
                    "requestId": format!("{:032x}", (offer * PEERS + index) as u128),
                    "body": {"offerId": hex, "manifest": manifest, "mac": MAC.repeat(32)},
                })
                .to_string(),
            )
            .await?;
        }
        for peer in peers.iter_mut() {
            let _ = peer.expect("ack", wait).await?;
        }
        tokio::time::sleep(Duration::from_millis(260)).await;
    }
    // Let the mutation buckets refill before the transfer dance.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // --- 32 concurrent relays ----------------------------------------------
    // Peer i serves peer i+1, so every peer is a source once and a recipient
    // once: 2 transfers each, inside the per-peer cap of 8.
    struct Pump {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        sender: tokio::task::JoinHandle<Result<u64>>,
        receiver: tokio::task::JoinHandle<Result<u64>>,
    }
    fn spawn_pump(mut source: support::RelayLeg, mut recipient: support::RelayLeg) -> Pump {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_sender = std::sync::Arc::clone(&stop);
        let sender = tokio::spawn(async move {
            let payload = vec![0x5au8; FRAME_BODY - 16];
            let mut seq = 0u32;
            let mut sent = 0u64;
            while !stop_sender.load(std::sync::atomic::Ordering::Relaxed) {
                source.send_binary(relay_test_frame(seq, &payload)).await?;
                seq = seq.wrapping_add(1);
                sent += FRAME_BODY as u64;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let _ = source.close().await;
            anyhow::Result::<_>::Ok(sent)
        });
        let receiver = tokio::spawn(async move {
            let mut received = 0u64;
            while let Ok(Some(message)) = recipient.next_msg(Duration::from_secs(5)).await {
                match message {
                    tokio_tungstenite::tungstenite::Message::Binary(frame) => {
                        received += frame.len() as u64
                    }
                    tokio_tungstenite::tungstenite::Message::Close(_) => break,
                    _ => break,
                }
            }
            anyhow::Result::<_>::Ok(received)
        });
        Pump {
            stop,
            sender,
            receiver,
        }
    }

    let mut pumps: Vec<Option<Pump>> = Vec::with_capacity(TRANSFERS);
    let mut nonce: u128 = 0x1000;
    for index in 0..TRANSFERS {
        let recipient_index = (index + 1) % PEERS;
        let (source_slice, recipient_slice) = if index < recipient_index {
            let (left, right) = peers.split_at_mut(recipient_index);
            (&mut left[index], &mut right[0])
        } else {
            let (left, right) = peers.split_at_mut(index);
            (&mut right[0], &mut left[recipient_index])
        };
        nonce += 0x10;
        let (_id, leg_source, leg_recipient) = soak_transfer(
            &host,
            &origin,
            &room_hex,
            source_slice,
            recipient_slice,
            &peer_ids[index],
            &peer_ids[recipient_index],
            &offer_id(index, 0),
            &MAC.repeat(32),
            nonce,
        )
        .await?;
        pumps.push(Some(spawn_pump(leg_source, leg_recipient)));
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    // --- pump, sample, cancel and resume half of them -----------------------
    let started = std::time::Instant::now();
    let mut samples: Vec<u64> = Vec::new();
    let mut resumed = false;
    while started.elapsed() < window {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Some(rss) = rss_kib_of(server_pid) {
            samples.push(rss);
        }
        // Keepalive: the control reaper drops a peer silent for 60 s, and a
        // long scripted window is longer than that.
        if samples.len().is_multiple_of(20) {
            for peer in peers.iter_mut() {
                peer.ping().await?;
            }
            for peer in peers.iter_mut() {
                let _ = peer.expect("pong", wait).await?;
            }
        }
        if !resumed && started.elapsed() >= window / 2 {
            resumed = true;
            // Half the transfers are cancelled and then resumed. With exactly
            // as many relay permits as there are transfers, a permit not
            // returned by the cancel makes the resume fail to attach — which
            // is the leak this arm is here to catch.
            for index in (0..TRANSFERS).step_by(2) {
                let Some(pump) = pumps[index].take() else {
                    continue;
                };
                pump.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = pump.sender.await;
                let _ = pump.receiver.await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            for index in (0..TRANSFERS).step_by(2) {
                let recipient_index = (index + 1) % PEERS;
                let (source_slice, recipient_slice) = if index < recipient_index {
                    let (left, right) = peers.split_at_mut(recipient_index);
                    (&mut left[index], &mut right[0])
                } else {
                    let (left, right) = peers.split_at_mut(index);
                    (&mut right[0], &mut left[recipient_index])
                };
                nonce += 0x10;
                let (_id, leg_source, leg_recipient) = soak_transfer(
                    &host,
                    &origin,
                    &room_hex,
                    source_slice,
                    recipient_slice,
                    &peer_ids[index],
                    &peer_ids[recipient_index],
                    &offer_id(index, 1),
                    &MAC.repeat(32),
                    nonce,
                )
                .await?;
                pumps[index] = Some(spawn_pump(leg_source, leg_recipient));
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
        }
    }

    let mut moved = 0u64;
    for pump in pumps.iter_mut().flatten() {
        pump.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    for pump in pumps.iter_mut().filter_map(Option::take) {
        let _ = pump.sender.await;
        if let Ok(Ok(received)) = pump.receiver.await {
            moved += received;
        }
    }
    let rss_peak = samples.iter().copied().max().or(rss_before);
    let fds_peak = fd_targets(server_pid).len();
    let (broadcast, skipped): (u64, u64) = peers.iter().fold((0, 0), |(d, s), peer| {
        (d + peer.dropped(), s + peer.skipped)
    });
    println!(
        "SOAK peers={PEERS} offers={} relays={TRANSFERS} moved={:.1}MiB rss={}->{}KiB \
         fds={}->{} events={broadcast} skipped={skipped}",
        PEERS * OFFERS_PER_PEER,
        moved as f64 / (1024.0 * 1024.0),
        kib_or_na(rss_before),
        kib_or_na(rss_peak),
        fds_before,
        fds_peak
    );
    anyhow::ensure!(moved > 0, "the soak relayed nothing");

    // The plan's bound: baseline + 32 MiB + 2 MiB per relay + 1 MiB per peer.
    match (rss_before, rss_peak) {
        (Some(before), Some(peak)) => {
            let bound_kib = before + (32 + 2 * TRANSFERS as u64 + PEERS as u64) * 1024;
            assert!(
                peak <= bound_kib,
                "server RSS {peak} KiB exceeded the budget {bound_kib} KiB (baseline {before})"
            );
        }
        _ => println!("N/A the soak RSS budget needs /proc/<pid>/status"),
    }
    // Unbounded growth, not the absolute value, is what a leak looks like in a
    // soak: the last three samples must not add a meaningful amount between
    // them (they are a second apart, on an unchanged load).
    if samples.len() >= 3 {
        let tail = &samples[samples.len() - 3..];
        let growth = tail[2].saturating_sub(tail[0]);
        assert!(
            growth <= 8 * 1024,
            "server RSS grew {growth} KiB across the last three samples: {tail:?}"
        );
    }

    // --- teardown: every permit must come back ------------------------------
    drop(peers);
    owner.start_kill()?;
    let _ = owner.wait().await;
    // The room is at the server's whole capacity, so the fresh room below can
    // only be admitted once the closed one has given everything back.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let (mut owner2, room2_hex, token2_hex) = loop {
        match open_room(&binary, port).await {
            Ok(opened) => break opened,
            Err(e) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let _ = e;
            }
            Err(e) => return Err(e).context("the closed room never gave its slot back"),
        }
    };
    // The whole global peer budget must come back, not one permit of it: the
    // fresh room is filled to capacity. The deadline is DERIVED from the
    // server's own bound instead of guessed. A peer that has stopped reading
    // parks its session inside `bounded_ws_send` for one
    // `WEB_TRANSFER_CTRL_SEND_TIMEOUT`, and its `PeerGuard` — so its permit —
    // is released only when that session ends; this soak drops 32 peers whose
    // sockets are full of catalog events, so some of them take exactly that
    // long. MEASURED here: 19 permits come back within 13 ms and the last 13
    // at 10.57 s, one send timeout later. The plan's flat 10 s therefore sat
    // ON the boundary and could not pass on any machine — it was measuring
    // the bound, not a leak.
    let deadline = std::time::Instant::now()
        + bore_cli::web_transfer::WEB_TRANSFER_CTRL_SEND_TIMEOUT * 2
        + Duration::from_secs(5);
    let mut fresh = Vec::with_capacity(PEERS);
    let mut fresh_ids = Vec::with_capacity(PEERS);
    while fresh.len() < PEERS {
        let index = fresh.len();
        let mut peer =
            support::WsPeer::connect_from(peer_ip(index), &host, &room2_hex, &origin).await?;
        peer.hello(&token2_hex, Some(&format!("Q{index}"))).await?;
        match expect_type(&mut peer, "welcome", Duration::from_secs(2)).await {
            Ok(welcome) => {
                fresh_ids.push(welcome["peerId"].as_str().unwrap().to_string());
                read_snapshot(&mut peer).await?;
                fresh.push(peer.into_pumped_keeping(KEEP));
            }
            Err(e) => {
                // KEEP what was already admitted. Dropping the whole attempt
                // and starting over makes the retry compete with its own
                // un-released permits, so the test starves the budget it is
                // measuring — MEASURED on the ubuntu CI runner, refused at 21
                // of 32 by a server that had leaked nothing. Topping up is
                // also the stricter reading: every permit is claimed the
                // moment it comes back.
                drop(peer);
                if std::time::Instant::now() >= deadline {
                    return Err(e).with_context(|| {
                        format!(
                            "peer {index} of {PEERS} was still refused two control-send \
                             timeouts after the room closed: a peer permit leaked"
                        )
                    });
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
    // ... and a relay pair, which proves the relay permits came back too.
    let hex = offer_id(900, 0);
    let manifest = catalog_manifest(&hex, "After", "after.bin");
    fresh[0]
        .send_text(
            serde_json::json!({
                "v": 1, "type": "offer.publish", "requestId": format!("{:032x}", 0xf00du128),
                "body": {"offerId": hex, "manifest": manifest, "mac": MAC.repeat(32)},
            })
            .to_string(),
        )
        .await?;
    let _ = fresh[0].expect("ack", wait).await?;
    let (source_slice, recipient_slice) = {
        let (left, right) = fresh.split_at_mut(1);
        (&mut left[0], &mut right[0])
    };
    let (_id, leg_source, leg_recipient) = soak_transfer(
        &host,
        &origin,
        &room2_hex,
        source_slice,
        recipient_slice,
        &fresh_ids[0],
        &fresh_ids[1],
        &hex,
        &MAC.repeat(32),
        0xbeef,
    )
    .await
    .context("no relay pair could be attached after the soak: a relay permit leaked")?;
    drop(leg_source);
    drop(leg_recipient);

    let rss_after = rss_kib_of(server_pid);
    println!(
        "SOAK settled rss={}KiB (baseline {}KiB)",
        kib_or_na(rss_after),
        kib_or_na(rss_before)
    );
    drop(fresh);
    owner2.start_kill()?;
    let _ = owner2.wait().await;
    server.start_kill()?;
    let _ = server.wait().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sub-phase 6.2 — admin config, metrics and log privacy
// ---------------------------------------------------------------------------

/// One authenticated admin GET, returning the JSON body.
async fn admin_json(port: u16, path: &str) -> Result<serde_json::Value> {
    let response = http_exchange(
        TcpStream::connect(("127.0.0.1", port)).await?,
        &format!(
            "GET {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {WEB_ADMIN_TOKEN}\r\n\
             Connection: close\r\n\r\n"
        ),
    )
    .await?;
    anyhow::ensure!(
        response.starts_with("HTTP/1.1 200"),
        "{path}: {response:.80}"
    );
    let body = response
        .split_once("\r\n\r\n")
        .context("admin response has no body")?
        .1;
    Ok(serde_json::from_str(body)?)
}

/// `T-WEB-ADMIN`: the admin surface answers the two questions an operator has
/// — "what did I configure?" and "what is happening right now?" — and never
/// confuses them.
///
/// Configured totals are read once and must not move under load (P-11: a
/// config endpoint that publishes a live gauge reads 0 on a saturated server,
/// which is indistinguishable from "not configured"). Live gauges must move
/// with the room, come back on cleanup, and show a genuine zero AS zero.
#[tokio::test]
async fn t_web_admin() -> Result<()> {
    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    args.relay_rate_bytes_per_s = 0;
    // One relay slot makes saturation observable in a single transfer, and
    // makes `relay_slots_available` reach the value that matters: zero.
    args.max_relays_global = 1;
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");

    // --- config: the totals, exactly as configured -------------------------
    let config_before = admin_json(port, "/admin/api/v1/config").await?;
    assert_eq!(config_before["web_transfer_enabled"], true);
    let limits = bore_cli::web_transfer::WebTransferLimits::default();
    for (field, expected) in [
        ("web_transfer_max_rooms", limits.max_rooms),
        ("web_transfer_max_peers", limits.max_peers_global),
        ("web_transfer_max_peers_per_room", limits.max_peers_per_room),
        (
            "web_transfer_max_offers_per_peer",
            limits.max_offers_per_peer,
        ),
        (
            "web_transfer_max_entries_per_offer",
            limits.max_entries_per_offer,
        ),
        ("web_transfer_max_offer_bytes", limits.max_offer_bytes),
        (
            "web_transfer_max_metadata_per_room",
            limits.max_metadata_per_room_bytes,
        ),
        (
            "web_transfer_max_metadata_total",
            limits.max_metadata_total_bytes,
        ),
        (
            "web_transfer_max_transfers_per_peer",
            limits.max_transfers_per_peer,
        ),
        ("web_transfer_owner_grace_seconds", limits.owner_grace_secs),
    ] {
        assert_eq!(config_before[field], expected, "config field {field}");
    }
    assert_eq!(config_before["web_transfer_max_relays"], 1);
    assert_eq!(config_before["web_transfer_relay_rate_bytes_per_second"], 0);
    // A COUNT, never the list: the servers themselves are infrastructure.
    assert!(
        config_before["web_transfer_stun_count"].is_u64(),
        "stun count must be a number"
    );
    let serialized = config_before.to_string();
    assert!(
        !serialized.contains("stun:"),
        "the STUN list must never reach the admin view: {serialized:.200}"
    );

    // --- metrics: an idle server reads zero, and says so -------------------
    let idle = admin_json(port, "/admin/api/v1/metrics").await?;
    for field in [
        "web_transfer_rooms_current",
        "web_transfer_peers_current",
        "web_transfer_offers_current",
        "web_transfer_metadata_bytes_current",
        "web_transfer_transfers_active",
        "web_transfer_relays_active",
        "web_transfer_relay_ciphertext_bytes_total",
        "web_transfer_direct_commits_total",
        "web_transfer_relay_commits_total",
        "web_transfer_completed_total",
        "web_transfer_cancelled_total",
        "web_transfer_rejected_total",
    ] {
        assert_eq!(idle[field], 0, "idle metric {field} must be a visible zero");
    }
    assert_eq!(idle["web_transfer_relay_slots_available"], 1);

    // --- load: every gauge moves ------------------------------------------
    let (lease, source, mut recipient, mut leg_source, mut leg_recipient, transfer_id) =
        relay_pair_room(
            &registry,
            &host,
            &origin,
            0x51,
            "cccccccccccccccccccccccccccccccc",
        )
        .await?;
    const FRAME_BODY: usize = 24 * 1024 + 16;
    let payload = vec![0x5au8; FRAME_BODY - 16];
    leg_source
        .send_binary(relay_test_frame(0, &payload))
        .await?;
    let forwarded = leg_recipient.next_msg(Duration::from_secs(10)).await?;
    assert!(
        matches!(
            forwarded,
            Some(tokio_tungstenite::tungstenite::Message::Binary(_))
        ),
        "the relay forwarded no frame"
    );

    let busy = admin_json(port, "/admin/api/v1/metrics").await?;
    assert_eq!(busy["web_transfer_rooms_current"], 1);
    assert_eq!(busy["web_transfer_peers_current"], 2);
    assert_eq!(busy["web_transfer_offers_current"], 1);
    assert_eq!(busy["web_transfer_transfers_active"], 1);
    assert_eq!(busy["web_transfer_relays_active"], 1);
    // Saturated: the one slot is taken, and a ZERO is what says so.
    assert_eq!(busy["web_transfer_relay_slots_available"], 0);
    assert!(
        busy["web_transfer_metadata_bytes_current"]
            .as_u64()
            .unwrap()
            > 0,
        "the catalog holds an offer and reports no metadata"
    );
    assert!(
        busy["web_transfer_relay_ciphertext_bytes_total"]
            .as_u64()
            .unwrap()
            >= FRAME_BODY as u64,
        "the forwarded frame is not counted"
    );

    // Configured totals do NOT move under load — the whole point of P-11.
    let config_busy = admin_json(port, "/admin/api/v1/config").await?;
    for field in [
        "web_transfer_max_rooms",
        "web_transfer_max_peers",
        "web_transfer_max_relays",
        "web_transfer_relay_rate_bytes_per_second",
    ] {
        assert_eq!(
            config_busy[field], config_before[field],
            "config field {field} moved with load"
        );
    }

    // --- saturation: a refusal is counted ----------------------------------
    let rejected_before = busy["web_transfer_rejected_total"].as_u64().unwrap();
    assert!(
        registry.try_acquire_relay().is_err(),
        "the single relay slot is held by the live pair"
    );
    let saturated = admin_json(port, "/admin/api/v1/metrics").await?;
    assert_eq!(
        saturated["web_transfer_rejected_total"],
        rejected_before + 1,
        "a capacity refusal must be counted"
    );

    // --- cancel and cleanup: the gauges come back --------------------------
    recipient
        .send_text(
            serde_json::json!({
                "v": 1, "type": "transfer.cancel",
                "requestId": format!("{:032x}", 0xcafeu128),
                "body": {"transferId": transfer_id},
            })
            .to_string(),
        )
        .await?;
    let _ = expect_type(&mut recipient, "ack", Duration::from_secs(10)).await?;
    let cancelled = admin_json(port, "/admin/api/v1/metrics").await?;
    assert_eq!(
        cancelled["web_transfer_cancelled_total"], 1,
        "a cancelled transfer must be counted"
    );
    drop(leg_source);
    drop(leg_recipient);
    drop(source);
    drop(recipient);
    // Dropping the lease DETACHES with the owner grace, so the room survives
    // on purpose (a reconnecting owner must find it). An explicit close is
    // what an operator's "room gone" means, and it is what releases the slot.
    lease.close_explicit(&registry);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let after = admin_json(port, "/admin/api/v1/metrics").await?;
        let idle_again = after["web_transfer_rooms_current"] == 0
            && after["web_transfer_peers_current"] == 0
            && after["web_transfer_offers_current"] == 0
            && after["web_transfer_relays_active"] == 0
            && after["web_transfer_relay_slots_available"] == 1
            && after["web_transfer_transfers_active"] == 0;
        if idle_again {
            // Totals are cumulative: they must NOT come back.
            assert!(
                after["web_transfer_relay_ciphertext_bytes_total"]
                    .as_u64()
                    .unwrap()
                    > 0,
                "a cumulative total must not reset with the gauges"
            );
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "gauges never came back: {after}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

/// `T-WEB-LOG-PRIVACY`: a canary in every field the phase contract forbids,
/// and proof that none of them reaches the server's logs or its admin JSON —
/// while the opaque identifiers and counts an operator needs stay.
///
/// The forbidden set is the phase contract's own list: user-chosen peer name,
/// offer label, path, filename, manifest, SDP, ICE candidate, token, key and
/// payload marker. Each one is a distinct canary word, so a failure names the
/// field that leaked rather than "something leaked".
#[tokio::test]
async fn t_web_log_privacy() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};
    use sha2::{Digest, Sha256};

    const CANARY_NAME: &str = "CANARY-NAME-pangolin";
    const CANARY_LABEL: &str = "CANARY-LABEL-okapi";
    const CANARY_PATH: &str = "CANARY-PATH-tapir.bin";
    const CANARY_SDP: &str = "CANARY-SDP-quokka";
    const CANARY_ICE: &str = "CANARY-ICE-dugong";
    const CANARY_PAYLOAD: &str = "CANARY-PAYLOAD-numbat";

    let log_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = CatalogLogSink(std::sync::Arc::clone(&log_buf));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || sink.clone())
        .with_max_level(tracing::Level::TRACE)
        // Without this the field names are wrapped in ANSI escapes, so
        // `bytes=` never appears literally and an assertion on it would fail
        // for a reason that has nothing to do with what is logged.
        .with_ansi(false)
        .finish();
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    let _dispatcher_guard = tracing::dispatcher::set_default(&dispatch);

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    let config = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    server.set_web_transfer(config)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x61u8; 32]);
    let owner = OwnerToken::from_bytes([0x62u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(15);

    // A named peer, an offer whose label and path are canaries, a transfer,
    // and real signaling carrying a canary SDP and a canary ICE candidate.
    let mut source = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    source.hello(&token_hex, Some(CANARY_NAME)).await?;
    let welcome = expect_type(&mut source, "welcome", wait).await?;
    let _ = welcome;
    read_snapshot(&mut source).await?;
    let mut recipient = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    recipient.hello(&token_hex, Some("plain")).await?;
    let _ = expect_type(&mut recipient, "welcome", wait).await?;
    read_snapshot(&mut recipient).await?;
    let _ = expect_type(&mut source, "peer.joined", wait).await?;

    let offer_hex = "aaaaaaaabbbbbbbbccccccccdddddddd";
    let manifest = catalog_manifest(offer_hex, CANARY_LABEL, CANARY_PATH);
    catalog_publish(&mut source, &"a1".repeat(16), offer_hex, &manifest).await?;
    let added = expect_type(&mut recipient, "offer.added", wait).await?;
    let mac_hex = added["mac"].as_str().unwrap().to_string();
    let selection = serde_json::json!({
        "entryIds": ["0"],
        "manifestMac": mac_hex,
        "mode": "raw",
        "offerId": offer_hex,
    });
    let digest_hex = hex::encode(Sha256::digest(
        bore_cli::web_transfer_protocol::canonical_json(&selection)
            .unwrap()
            .as_bytes(),
    ));
    recipient
        .send_text(
            serde_json::json!({
                "v": 1, "type": "transfer.request", "requestId": "b1".repeat(16),
                "body": { "offerId": offer_hex, "entryIds": ["0"],
                          "selectionDigest": digest_hex, "mode": "raw" },
            })
            .to_string(),
        )
        .await?;
    let ack = expect_type(&mut recipient, "ack", wait).await?;
    let transfer_id = ack["result"]["transferId"].as_str().unwrap().to_string();
    let incoming = expect_type(&mut source, "transfer.incoming", wait).await?;
    let attempt_id = incoming["attemptId"].as_str().unwrap().to_string();
    source
        .send_text(
            serde_json::json!({
                "v": 1, "type": "transfer.source_ready", "requestId": "b2".repeat(16),
                "body": { "transferId": transfer_id, "attemptId": attempt_id,
                          "selectionDigest": digest_hex },
            })
            .to_string(),
        )
        .await?;
    let _ = expect_type(&mut source, "ack", wait).await?;
    let _ = expect_type(&mut recipient, "transfer.direct_start", wait).await?;
    let _ = expect_type(&mut source, "transfer.direct_start", wait).await?;
    // The recipient is the offerer: its SDP and its candidate are forwarded
    // by the server, which is exactly the traffic that must not be logged.
    recipient
        .send_text(
            serde_json::json!({
                "v": 1, "type": "rtc.offer", "requestId": "b3".repeat(16),
                "body": { "transferId": transfer_id, "attemptId": attempt_id,
                          "sdp": format!("v=0\r\na={CANARY_SDP}") },
            })
            .to_string(),
        )
        .await?;
    let _ = expect_type(&mut recipient, "ack", wait).await?;
    recipient
        .send_text(
            serde_json::json!({
                "v": 1, "type": "rtc.ice", "requestId": "b4".repeat(16),
                "body": { "transferId": transfer_id, "attemptId": attempt_id,
                          "candidate": format!("candidate:1 1 udp 1 {CANARY_ICE} 1 typ host"),
                          "sdpMid": "0", "sdpMLineIndex": 0 },
            })
            .to_string(),
        )
        .await?;
    let _ = expect_type(&mut recipient, "ack", wait).await?;
    // The direct attempt gives up (V003-C3). This is the one event the
    // server used to record NOWHERE: the log showed a relay starting and
    // nothing about the path it replaced. The line it now writes is asserted
    // below — with the fixed reason and the opaque ids, and with none of the
    // canaries this peer has been feeding it.
    recipient
        .send_text(
            serde_json::json!({
                "v": 1, "type": "transfer.direct_failed", "requestId": "b6".repeat(16),
                "body": { "transferId": transfer_id, "attemptId": attempt_id,
                          "reason": "ice-failed", "resumeRanges": [] },
            })
            .to_string(),
        )
        .await?;
    let _ = expect_type(&mut recipient, "ack", wait).await?;
    recipient
        .send_text(
            serde_json::json!({
                "v": 1, "type": "transfer.cancel", "requestId": "b5".repeat(16),
                "body": {"transferId": transfer_id},
            })
            .to_string(),
        )
        .await?;
    let _ = expect_type(&mut recipient, "ack", wait).await?;

    // A RELAY arm too: the relayed bytes are the one payload the server
    // actually touches, and its close line is the log entry that proves the
    // useful half of the contract (an opaque transfer id survives).
    let (relay_lease, relay_source, relay_recipient, mut leg_a, mut leg_b, relay_transfer) =
        relay_pair_room(
            &registry,
            &host,
            &origin,
            0x71,
            "11112222333344445555666677778888",
        )
        .await?;
    leg_a
        .send_binary(relay_test_frame(
            0,
            format!("frame-{CANARY_PAYLOAD}").as_bytes(),
        ))
        .await?;
    let _ = leg_b.next_msg(wait).await?;
    // A peer that has already seen the server's close cannot send one back,
    // and that is a normal end of a relay leg, not a failure of the gate.
    let _ = leg_a.close().await;
    let _ = leg_b.close().await;
    drop(leg_a);
    drop(leg_b);
    drop(relay_source);
    drop(relay_recipient);
    // The close line is written when the relay task observes both halves
    // gone, so the assertion needs the task to have run, not just the sockets
    // to have been dropped.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if String::from_utf8_lossy(&log_buf.lock().unwrap().clone()).contains(&relay_transfer) {
            break;
        }
    }

    // --- the proof ---------------------------------------------------------
    let logs = String::from_utf8_lossy(&log_buf.lock().unwrap().clone()).into_owned();
    let config_json = admin_json(port, "/admin/api/v1/config").await?.to_string();
    let metrics_json = admin_json(port, "/admin/api/v1/metrics").await?.to_string();
    let status_json = admin_json(port, "/admin/status/data").await?.to_string();
    for (what, canary) in [
        ("peer display name", CANARY_NAME),
        ("offer label", CANARY_LABEL),
        ("offer path", CANARY_PATH),
        ("SDP", CANARY_SDP),
        ("ICE candidate", CANARY_ICE),
        ("member token", token_hex.as_str()),
        ("manifest MAC", mac_hex.as_str()),
        ("relayed payload", CANARY_PAYLOAD),
    ] {
        assert!(
            !logs.contains(canary),
            "the {what} reached the server log: {canary}"
        );
        for (surface, json) in [
            ("config", &config_json),
            ("metrics", &metrics_json),
            ("status", &status_json),
        ] {
            assert!(
                !json.contains(canary),
                "the {what} reached the admin {surface} JSON: {canary}"
            );
        }
    }
    // ... and the opaque identifiers that make a log useful are still there:
    // the relay close line carries the transfer id and the byte counts an
    // operator needs, which is what makes this a privacy gate and not a
    // "log nothing" gate.
    assert!(
        logs.contains(&relay_transfer),
        "the relay close line does not name its transfer: {logs:.600}"
    );
    assert!(
        logs.contains("relay pair") && logs.contains("bytes=") && logs.contains("frames="),
        // Clean or not is not the point here — the point is that the line an
        // operator reads carries the transfer, the byte count and the frame
        // count, and none of the canaries.
        "the relay close line lost its operator-facing detail: {}",
        logs.lines()
            .filter(|line| line.contains("relay") || line.contains(&relay_transfer))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // The direct-failure line is the other half of the same contract: an
    // operator asking "why did this transfer end up on the relay?" gets the
    // fixed reason and the two opaque ids, and nothing else. Before V003-C3
    // the answer was silence, and the browser's own trace had no counterpart
    // on the server at all.
    let direct_line = logs
        .lines()
        .find(|line| line.contains("web-transfer direct attempt failed"))
        .unwrap_or_else(|| panic!("no direct-failure line in the log: {logs:.800}"));
    assert!(
        direct_line.contains("reason=\"ice-failed\"") || direct_line.contains("reason=ice-failed"),
        "the direct-failure line lost its fixed reason: {direct_line}"
    );
    assert!(
        direct_line.contains(&transfer_id) && direct_line.contains(&attempt_id),
        "the direct-failure line lost its opaque ids: {direct_line}"
    );
    relay_lease.close_explicit(&registry);
    drop(lease);
    Ok(())
}

/// 6.2: with the feature OFF the admin surface says "not configured" with a
/// null, and with it ON it publishes the CONFIGURED totals.
///
/// Null and zero are different answers and an operator reads them as
/// different answers (P-11): a null means "this server does not run web
/// transfer", a zero means "it does, and there is none right now".
#[tokio::test]
async fn web_config_reports_totals_and_null_when_disabled() -> Result<()> {
    // --- disabled ----------------------------------------------------------
    let port = support::free_port().await?;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let config = admin_json(port, "/admin/api/v1/config").await?;
    assert_eq!(config["web_transfer_enabled"], false);
    const CONFIG_TOTALS: [&str; 13] = [
        "web_transfer_base_origin",
        "web_transfer_max_rooms",
        "web_transfer_max_peers",
        "web_transfer_max_peers_per_room",
        "web_transfer_max_offers_per_peer",
        "web_transfer_max_entries_per_offer",
        "web_transfer_max_offer_bytes",
        "web_transfer_max_metadata_per_room",
        "web_transfer_max_metadata_total",
        "web_transfer_max_transfers_per_peer",
        "web_transfer_max_relays",
        "web_transfer_relay_rate_bytes_per_second",
        "web_transfer_owner_grace_seconds",
    ];
    for field in CONFIG_TOTALS {
        assert!(
            config[field].is_null(),
            "{field} must be null on a server that does not run web transfer"
        );
    }
    let metrics = admin_json(port, "/admin/api/v1/metrics").await?;
    for field in WEB_METRIC_GAUGES {
        assert!(
            metrics[field].is_null(),
            "{field} must be null on a server that does not run web transfer"
        );
    }

    // --- enabled -----------------------------------------------------------
    let port_on = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port_on}/"));
    args.max_rooms = 7;
    args.max_relays_global = 3;
    let resolved = bore_cli::web_transfer::resolve_server_config(&args, false, port_on)?
        .expect("config resolves");
    let mut server_on = Server::new(1024..=65535, None);
    server_on.set_control_port(port_on);
    server_on.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    server_on.set_web_transfer(resolved)?;
    tokio::spawn(server_on.listen());
    support::wait_port(port_on, true).await;

    let config_on = admin_json(port_on, "/admin/api/v1/config").await?;
    assert_eq!(config_on["web_transfer_enabled"], true);
    assert_eq!(config_on["web_transfer_max_rooms"], 7);
    assert_eq!(config_on["web_transfer_max_relays"], 3);
    for field in CONFIG_TOTALS {
        assert!(
            !config_on[field].is_null(),
            "{field} must be published once the feature is configured"
        );
    }
    Ok(())
}

/// The 13 live gauges and totals, in one place so every 6.2 test asserts on
/// the same list and a field added without a test is visible as a diff.
const WEB_METRIC_GAUGES: [&str; 13] = [
    "web_transfer_rooms_current",
    "web_transfer_peers_current",
    "web_transfer_offers_current",
    "web_transfer_metadata_bytes_current",
    "web_transfer_transfers_active",
    "web_transfer_relays_active",
    "web_transfer_relay_slots_available",
    "web_transfer_relay_ciphertext_bytes_total",
    "web_transfer_direct_commits_total",
    "web_transfer_relay_commits_total",
    "web_transfer_completed_total",
    "web_transfer_cancelled_total",
    "web_transfer_rejected_total",
];

/// 6.2 / P-11: the CONFIG endpoint publishes what was configured, and load
/// must not move it by one unit — the defect P-11 records is a config field
/// that was secretly a gauge and read 0 on a saturated server.
#[tokio::test]
async fn web_config_totals_do_not_move_under_load() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    // Small enough that the load below SATURATES it: a total that is really a
    // gauge reads zero exactly here, and nowhere else.
    args.max_rooms = 2;
    args.max_relays_global = 2;
    let resolved = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    server.set_web_transfer(resolved)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let idle = admin_json(port, "/admin/api/v1/config").await?;
    assert_eq!(idle["web_transfer_max_rooms"], 2);
    assert_eq!(idle["web_transfer_max_relays"], 2);

    // Every room, and every relay slot, taken.
    let mut leases = Vec::new();
    for seed in 0u8..2 {
        let member = MemberToken::from_bytes([0x30 + seed; 32]);
        let owner = OwnerToken::from_bytes([0x40 + seed; 32]);
        leases.push(
            OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        );
    }
    let mut relays = Vec::new();
    while let Ok(permit) = registry.try_acquire_relay() {
        relays.push(permit);
    }
    assert_eq!(registry.current_rooms(), 2, "the rooms are not saturated");
    assert_eq!(
        registry.relay_slots_available(),
        0,
        "the relay budget is not saturated"
    );

    let saturated = admin_json(port, "/admin/api/v1/config").await?;
    for field in [
        "web_transfer_enabled",
        "web_transfer_max_rooms",
        "web_transfer_max_peers",
        "web_transfer_max_peers_per_room",
        "web_transfer_max_offers_per_peer",
        "web_transfer_max_entries_per_offer",
        "web_transfer_max_offer_bytes",
        "web_transfer_max_metadata_per_room",
        "web_transfer_max_metadata_total",
        "web_transfer_max_transfers_per_peer",
        "web_transfer_max_relays",
        "web_transfer_relay_rate_bytes_per_second",
        "web_transfer_owner_grace_seconds",
        "web_transfer_stun_count",
    ] {
        assert_eq!(
            saturated[field], idle[field],
            "config field {field} moved under load: it is a gauge, not a total"
        );
    }
    // ... and the METRICS endpoint, which is where a gauge belongs, did move.
    let metrics = admin_json(port, "/admin/api/v1/metrics").await?;
    assert_eq!(metrics["web_transfer_rooms_current"], 2);
    assert_eq!(metrics["web_transfer_relay_slots_available"], 0);

    drop(relays);
    for lease in leases {
        lease.close_explicit(&registry);
    }
    Ok(())
}

/// 6.2 / P-11's frontend half: a live gauge at zero must be published AS zero.
///
/// Zero is the alarming value (no relay slots left, no peers connected), so a
/// surface that omits it, or a reader that treats it as "absent", hides
/// exactly the number an operator needs.
#[tokio::test]
async fn web_metrics_report_live_zero_as_zero_not_null() -> Result<()> {
    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    args.max_relays_global = 1;
    let resolved = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    server.set_web_transfer(resolved)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    // An idle enabled server: every gauge is a number, and that number is 0.
    let idle = admin_json(port, "/admin/api/v1/metrics").await?;
    for field in WEB_METRIC_GAUGES {
        assert!(
            idle[field].is_u64(),
            "{field} must be a number on an enabled server, not {}",
            idle[field]
        );
    }
    for field in WEB_METRIC_GAUGES {
        if field == "web_transfer_relay_slots_available" {
            continue;
        }
        assert_eq!(idle[field], 0, "{field} must read zero on an idle server");
    }
    assert_eq!(idle["web_transfer_relay_slots_available"], 1);

    // The one gauge whose ZERO is the alarming value: hold the only slot.
    let permit = registry.try_acquire_relay().expect("the single relay slot");
    let saturated = admin_json(port, "/admin/api/v1/metrics").await?;
    assert!(
        saturated["web_transfer_relay_slots_available"].is_u64(),
        "a saturated relay budget must publish a zero, never a null"
    );
    assert_eq!(saturated["web_transfer_relay_slots_available"], 0);
    drop(permit);
    Ok(())
}

/// 6.2 / `T-WEB-ADMIN`'s privacy half, as a unit: nothing a user typed, named
/// or negotiated may appear in ANY admin JSON — while the opaque counts stay.
///
/// `t_web_log_privacy` proves the same for the logs; this one is the cheap,
/// always-on guard on the three JSON surfaces alone.
#[tokio::test]
async fn admin_json_never_contains_canary_names_paths_tokens_sdp_candidates_or_manifest(
) -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};

    const NAME: &str = "ADMINCANARY-NAME-caracal";
    const LABEL: &str = "ADMINCANARY-LABEL-serval";
    const PATH: &str = "ADMINCANARY-PATH-civet.bin";

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    let resolved = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    server.set_web_transfer(resolved)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x5du8; 32]);
    let owner = OwnerToken::from_bytes([0x5eu8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(15);

    let mut source = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    source.hello(&token_hex, Some(NAME)).await?;
    let _ = expect_type(&mut source, "welcome", wait).await?;
    read_snapshot(&mut source).await?;
    let offer_hex = "1234123412341234123412341234abcd";
    let manifest = catalog_manifest(offer_hex, LABEL, PATH);
    // `catalog_publish` consumes both the ack and the publisher's own
    // `offer.added`, so there is nothing left to wait for here.
    catalog_publish(&mut source, &"5f".repeat(16), offer_hex, &manifest).await?;

    for path in [
        "/admin/api/v1/config",
        "/admin/api/v1/metrics",
        "/admin/status/data",
    ] {
        let json = admin_json(port, path).await?.to_string();
        for (what, canary) in [
            ("display name", NAME),
            ("offer label", LABEL),
            ("offer path", PATH),
            ("member token", token_hex.as_str()),
            // The manifest itself travels as the two canaries above: they are
            // its label and its path, which is the whole of what a user typed.
        ] {
            assert!(
                !json.contains(canary),
                "the {what} reached {path}: {canary}"
            );
        }
    }
    // The COUNTS are still there — this is a privacy gate, not a silence gate.
    let metrics = admin_json(port, "/admin/api/v1/metrics").await?;
    assert_eq!(metrics["web_transfer_rooms_current"], 1);
    assert_eq!(metrics["web_transfer_peers_current"], 1);
    assert_eq!(metrics["web_transfer_offers_current"], 1);
    assert!(
        metrics["web_transfer_metadata_bytes_current"]
            .as_u64()
            .unwrap()
            > 0,
        "the catalog holds an offer and reports no metadata"
    );
    lease.close_explicit(&registry);
    Ok(())
}

/// 6.2, the WIRING half of the logarithmic sampler (the unit proves the
/// decision, this proves the server actually makes it): eight refusals from
/// one address produce four lines — the 1st, 2nd, 4th and 8th — and none of
/// them says which room was addressed or what the caller sent.
#[tokio::test]
async fn web_pre_auth_failures_are_sampled_in_the_log() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};

    let log_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = CatalogLogSink(std::sync::Arc::clone(&log_buf));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || sink.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    let _dispatcher_guard = tracing::dispatcher::set_default(&dispatch);

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    let resolved = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    server.set_web_transfer(resolved)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x21u8; 32]);
    let owner = OwnerToken::from_bytes([0x22u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    // A syntactically valid token that is not this room's: the refusal is
    // `denied`, which is the shape a real guesser produces.
    let wrong_token = MemberToken::from_bytes([0x23u8; 32]).to_string();

    for _ in 0..8 {
        let mut peer = support::WsPeer::connect(&host, &room_hex, &origin).await?;
        peer.hello(&wrong_token, Some("intruder")).await?;
        // The server delays, then closes: reading until the socket ends is
        // what makes the attempt complete before the next one starts.
        let _ = peer.next_text(Duration::from_secs(5)).await;
        drop(peer);
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let logs = loop {
        let logs = String::from_utf8_lossy(&log_buf.lock().unwrap().clone()).into_owned();
        let lines = logs.matches("web-transfer pre-auth refused").count();
        if lines >= 4 || std::time::Instant::now() >= deadline {
            break logs;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let refusals: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("web-transfer pre-auth refused"))
        .collect();
    assert_eq!(
        refusals.len(),
        4,
        "eight refusals must produce exactly the 1st, 2nd, 4th and 8th line: {refusals:#?}"
    );
    for (expected, line) in ["failures=1", "failures=2", "failures=4", "failures=8"]
        .iter()
        .zip(&refusals)
    {
        assert!(line.contains(*expected), "expected {expected} in {line}");
    }
    for line in &refusals {
        assert!(
            line.contains("peer=127.0.0.1"),
            "a refusal with no address is not actionable: {line}"
        );
        assert!(
            !line.contains(&room_hex) && !line.contains(&wrong_token),
            "the refusal named the room or the token it refused: {line}"
        );
    }
    lease.close_explicit(&registry);
    Ok(())
}

/// 6.2: the admin surface's web-transfer keys are exactly the fixture's, on a
/// server that runs the feature and on one that does not.
///
/// An ADDITIVE field is only additive if the consumers know about it: a key
/// added here without a fixture line, a dashboard row and a null-vs-zero
/// decision is a key an operator reads as absent. The fixture is the diff
/// that makes that decision happen.
#[tokio::test]
async fn admin_web_transfer_fields_are_additive_and_match_the_fixture() -> Result<()> {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/web_transfer/v1/admin-fields.json"))?;
    let expected = |surface: &str| -> Vec<String> {
        fixture[surface]
            .as_array()
            .expect("fixture surface is a list")
            .iter()
            .map(|v| v.as_str().expect("fixture key is a string").to_string())
            .collect()
    };
    let live = |json: &serde_json::Value| -> Vec<String> {
        let mut keys: Vec<String> = json
            .as_object()
            .expect("admin JSON is an object")
            .keys()
            .filter(|key| key.starts_with("web_transfer_"))
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    let sorted = |mut keys: Vec<String>| -> Vec<String> {
        keys.sort();
        keys
    };

    // --- the feature OFF: every key present, every total null -------------
    let port_off = support::free_port().await?;
    let mut off = Server::new(1024..=65535, None);
    off.set_control_port(port_off);
    off.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    tokio::spawn(off.listen());
    support::wait_port(port_off, true).await;
    let config_off = admin_json(port_off, "/admin/api/v1/config").await?;
    let metrics_off = admin_json(port_off, "/admin/api/v1/metrics").await?;
    assert_eq!(
        live(&config_off),
        sorted(expected("config")),
        "the /config web-transfer keys drifted from the fixture"
    );
    assert_eq!(
        live(&metrics_off),
        sorted(expected("metrics")),
        "the /metrics web-transfer keys drifted from the fixture"
    );

    // --- the feature ON: the same keys, now carrying values ---------------
    let port_on = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port_on}/"));
    let resolved = bore_cli::web_transfer::resolve_server_config(&args, false, port_on)?
        .expect("config resolves");
    let mut on = Server::new(1024..=65535, None);
    on.set_control_port(port_on);
    on.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    on.set_web_transfer(resolved)?;
    tokio::spawn(on.listen());
    support::wait_port(port_on, true).await;
    let config_on = admin_json(port_on, "/admin/api/v1/config").await?;
    let metrics_on = admin_json(port_on, "/admin/api/v1/metrics").await?;
    assert_eq!(live(&config_on), sorted(expected("config")));
    assert_eq!(live(&metrics_on), sorted(expected("metrics")));
    for key in expected("metrics") {
        assert!(
            metrics_on[&key].is_u64(),
            "{key} must carry a number once the feature runs"
        );
        assert!(
            metrics_off[&key].is_null(),
            "{key} must be null when the feature is off"
        );
    }
    Ok(())
}

/// UDP sockets this process currently holds, by inode.
///
/// `/proc/net/udp` is per NETWORK NAMESPACE, so it lists sockets this process
/// does not own; `/proc/self/fd` is per process but does not say which
/// sockets are UDP. The intersection is the only honest answer, and it is
/// what makes "did the web surface open a UDP socket?" a question about THIS
/// server rather than about the machine.
#[cfg(target_os = "linux")]
fn udp_socket_inodes() -> std::collections::BTreeSet<u64> {
    let mut listed = std::collections::BTreeSet::new();
    for table in ["/proc/net/udp", "/proc/net/udp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            // The inode is the 10th whitespace-separated column.
            if let Some(inode) = line.split_whitespace().nth(9) {
                if let Ok(inode) = inode.parse::<u64>() {
                    listed.insert(inode);
                }
            }
        }
    }
    let mut mine = std::collections::BTreeSet::new();
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return mine;
    };
    for entry in entries.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let target = target.to_string_lossy().into_owned();
        let Some(rest) = target.strip_prefix("socket:[") else {
            continue;
        };
        let Some(inode) = rest.strip_suffix(']') else {
            continue;
        };
        if let Ok(inode) = inode.parse::<u64>() {
            if listed.contains(&inode) {
                mine.insert(inode);
            }
        }
    }
    mine
}

/// `T-WEB-UDP-ENDPOINT`: the web-transfer surface opens no UDP socket.
///
/// The browser peers punch their own UDP; the SERVER only brokers signalling
/// and, when the direct path fails, relays ciphertext over TCP. A UDP socket
/// appearing here would mean the feature had quietly grown a second endpoint
/// beside the one `--udp` already owns — the exact shape the direct-path
/// invariants forbid (`holepunch::bind_socket` must stay the single funnel,
/// and a second wildcard bind steals the first one's inbound).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn web_transfer_opens_no_udp_socket() -> Result<()> {
    let before = udp_socket_inodes();

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    args.relay_rate_bytes_per_s = 0;
    let resolved = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    server.set_web_transfer(resolved)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");

    // A full relay pair: room, two control sockets, tickets, both legs and
    // real ciphertext through the pump — every socket the feature can open.
    let (lease, source, recipient, mut leg_a, mut leg_b, _transfer) = relay_pair_room(
        &registry,
        &host,
        &origin,
        0x64,
        "abababababababababababababababab",
    )
    .await?;
    leg_a
        .send_binary(relay_test_frame(0, &vec![0x11u8; 4096]))
        .await?;
    let forwarded = leg_b.next_msg(Duration::from_secs(10)).await?;
    assert!(
        matches!(
            forwarded,
            Some(tokio_tungstenite::tungstenite::Message::Binary(_))
        ),
        "the relay forwarded no frame"
    );

    let during = udp_socket_inodes();
    let opened: Vec<u64> = during.difference(&before).copied().collect();
    assert!(
        opened.is_empty(),
        "the web-transfer surface opened {} UDP socket(s): {opened:?}",
        opened.len()
    );

    drop(leg_a);
    drop(leg_b);
    drop(source);
    drop(recipient);
    lease.close_explicit(&registry);

    // A control server with `--udp` OFF holds no UDP socket at all, which is
    // the stronger statement and the one an operator's firewall cares about.
    assert!(
        during.is_empty() || !before.is_empty(),
        "a web-transfer server with no --udp is holding UDP sockets: {during:?}"
    );
    Ok(())
}

/// `T-WEB-MALFORMED`: the hostile corpus against a REAL server, with a
/// healthy peer in the same room throughout.
///
/// The unit corpus proves the decoders refuse. This proves the SERVER stays a
/// server while they do: every case is answered with a stable error on an
/// open socket, the room keeps working for everyone else, and when the
/// corpus is done the attacker's own session is still usable — no wedged
/// peer, no leaked permit, no collateral damage to the neighbour.
#[tokio::test]
async fn t_web_malformed() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    let resolved = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_admin_token(Some(WEB_ADMIN_TOKEN.into()));
    server.set_web_transfer(resolved)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x41u8; 32]);
    let owner = OwnerToken::from_bytes([0x42u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(10);

    // The neighbour: an ordinary peer that must not notice any of this.
    let mut good = support::WsPeer::connect(&host, &room_hex, &origin)
        .await
        .context("connect good")?;
    good.hello(&token_hex, Some("good"))
        .await
        .context("hello good")?;
    let _ = expect_type(&mut good, "welcome", wait)
        .await
        .context("welcome good")?;
    read_snapshot(&mut good).await.context("snapshot good")?;

    let mut bad = support::WsPeer::connect(&host, &room_hex, &origin)
        .await
        .context("connect bad")?;
    bad.hello(&token_hex, Some("bad"))
        .await
        .context("hello bad")?;
    let _ = expect_type(&mut bad, "welcome", wait)
        .await
        .context("welcome bad")?;
    read_snapshot(&mut bad).await.context("snapshot bad")?;
    let _ = expect_type(&mut good, "peer.joined", wait)
        .await
        .context("good sees bad join")?;

    // One request id per case so a replay of the cache cannot mask a parse.
    let corpus: Vec<String> = vec![
        String::new(),
        "{".into(),
        "not json at all".into(),
        "[]".into(),
        "null".into(),
        r#"{"v":1}"#.into(),
        r#"{"v":1,"type":"ping"}"#.into(),
        r#"{"v":1,"type":"ping","body":{},"body":{"x":1}}"#.into(),
        r#"{"v":2,"type":"ping","body":{}}"#.into(),
        r#"{"v":1,"type":"nope","body":{}}"#.into(),
        r#"{"v":1,"type":7,"body":{}}"#.into(),
        r#"{"v":1,"type":"ping","body":[]}"#.into(),
        r#"{"v":1,"type":"peer.rename","requestId":"zz","body":{"displayName":"x"}}"#.into(),
        r#"{"v":1,"type":"peer.rename","requestId":"c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1","body":{"displayName":7}}"#.into(),
        r#"{"v":1,"type":"peer.rename","requestId":"c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2","body":{"displayName":"x","extra":1}}"#.into(),
        r#"{"v":1,"type":"transfer.cancel","requestId":"c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3","body":{"transferId":"00"}}"#.into(),
        r#"{"v":1,"type":"offer.publish","requestId":"c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4","body":{"offerId":"aa","manifest":{},"mac":"bb"}}"#.into(),
        r#"{"v":1,"type":"rtc.offer","requestId":"c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5","body":{"transferId":"00000000000000000000000000000001","attemptId":"00000000000000000000000000000001","sdp":[]}}"#.into(),
        r#"{"v":1,"type":"hello","requestId":"c6c6c6c6c6c6c6c6c6c6c6c6c6c6c6c6","body":{"memberToken":"ff"}}"#.into(),
    ];

    for (index, raw) in corpus.iter().enumerate() {
        bad.send_text(raw.clone())
            .await
            .with_context(|| format!("sending case {index}: {raw:.60}"))?;
        // Every case in this corpus is answered on an OPEN socket: killing
        // the connection would be a denial of service a peer could trigger
        // on itself by mistyping, and it would also hide which case was
        // wrong. The oversize case, which the transport itself refuses, is
        // driven separately below.
        let text = tokio::time::timeout(Duration::from_secs(5), bad.next_text(wait))
            .await
            .unwrap_or_else(|_| panic!("case {index} was never answered: {raw:.60}"))?
            .unwrap_or_else(|| panic!("case {index} closed the socket: {raw:.60}"));
        let value: serde_json::Value = serde_json::from_str(&text)?;
        assert_eq!(
            value["type"], "error",
            "case {index} was answered with {}: {raw:.60}",
            value["type"]
        );
        assert!(
            value["body"]["code"].is_string(),
            "case {index} answered an error with no code: {text:.120}"
        );
        // The neighbour is unaffected: it must still be able to say something
        // and be heard, after EVERY case and not just at the end.
        good.send_text(serde_json::json!({"v": 1, "type": "ping", "body": {}}).to_string())
            .await
            .with_context(|| format!("neighbour ping after case {index}"))?;
        let _ = expect_type(&mut good, "pong", wait)
            .await
            .with_context(|| format!("neighbour pong after case {index}"))?;
    }

    // The attacker's own session survived the whole corpus: the server never
    // needed to kill it, and a rename it sends now still works.
    bad.send_text(
        serde_json::json!({
            "v": 1, "type": "peer.rename",
            "requestId": "d1".repeat(16),
            "body": {"displayName": "reformed"},
        })
        .to_string(),
    )
    .await?;
    // `expect_type` already asserts the message type; what this adds is that
    // the ack is a real one — a rename that came back with an error code
    // would mean the corpus had cost the session its ability to mutate.
    let ack = expect_type(&mut bad, "ack", wait).await?;
    assert!(
        ack["code"].is_null(),
        "the attacker's own session was left degraded: {ack}"
    );

    // The oversize message is the one case the TRANSPORT refuses rather than
    // the decoder: 320 KiB is a WebSocket config bound, so the connection
    // ends. That is correct — there is no envelope to answer inside — and the
    // property worth proving is that it costs the attacker its own socket and
    // nobody else anything.
    let huge = format!(
        r#"{{"v":1,"type":"peer.rename","requestId":"c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7","body":{{"displayName":"{}"}}}}"#,
        "a".repeat(400_000)
    );
    let _ = bad.send_text(huge).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), bad.next_text(wait)).await;
    drop(bad);
    good.send_text(serde_json::json!({"v": 1, "type": "ping", "body": {}}).to_string())
        .await
        .context("neighbour ping after the oversize case")?;
    let _ = expect_type(&mut good, "pong", wait)
        .await
        .context("neighbour pong after the oversize case")?;

    // ... and the room's own accounting comes back to exactly the neighbour,
    // with no offer and no transfer: nothing in the corpus created state, and
    // the killed socket released its peer slot.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let metrics = admin_json(port, "/admin/api/v1/metrics").await?;
        if metrics["web_transfer_peers_current"] == 1 {
            assert_eq!(metrics["web_transfer_rooms_current"], 1);
            assert_eq!(metrics["web_transfer_offers_current"], 0);
            assert_eq!(metrics["web_transfer_transfers_active"], 0);
            assert_eq!(metrics["web_transfer_relays_active"], 0);
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the peer slot never came back: {metrics}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    lease.close_explicit(&registry);
    Ok(())
}

/// T-WEB-XSS-CSRF, server half (6.3): every pre-auth refusal is ONE class.
///
/// A wrong token, a token for a room that does not exist and a token that is
/// not even hex are three different facts, and an attacker who can tell them
/// apart can enumerate rooms with a token they already hold. The server
/// answers all three the same way — same close code, after the same uniform
/// delay — so the only thing the wire carries is "no".
///
/// The delay is asserted as a FLOOR and never as an equality: the network is
/// not constant and a gate that demands constant timing is a flaky gate, not
/// a stronger one. What matters is that no branch returns EARLY, which is
/// the leak a timing oracle actually needs.
#[tokio::test]
/// T-WEB-CARRIER-BUDGET. Signalling for N carriers must not spend the budget
/// the transfer's own lifecycle needs.
///
/// The offer and the answer are singletons PER CARRIER, so with the shipped
/// default of four carriers a source sends four `rtc.answer`s before the
/// transfer has moved one byte. While those were charged to the mutation
/// bucket (4/s, burst 8) the next mutation was refused, and MEASURED on a
/// real browser pair that next mutation was `transfer.source_ready` for the
/// relay leg the server had just ticketed: the leg was never attached, the
/// pair timed out after 30 s and the page read "Percorso interrotto: riprova"
/// with the file half delivered. The bound that matters is the one
/// `apply_signal` already enforces — one offer and one answer per carrier per
/// attempt — plus the control bucket, which still applies to every message.
async fn t_web_carrier_signalling_does_not_spend_the_mutation_budget() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    let resolved = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(resolved)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x71u8; 32]);
    let owner = OwnerToken::from_bytes([0x72u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(5);

    let mut peer = support::WsPeer::connect(&host, &room_hex, &origin).await?;
    peer.hello(&token_hex, None).await?;
    let (typ, _) = control_msg(&peer.next_text(wait).await?.expect("welcome"));
    assert_eq!(typ, "welcome");
    let _ = read_snapshot(&mut peer).await?;

    // Twice the shipped carrier count, so the gate still holds if the default
    // grows. The transfer does not exist, which is the POINT: the answer must
    // name the transfer, never the rate — a refusal here would be the defect,
    // whatever it is called.
    let rounds = 2 * bore_cli::web_transfer::WEB_TRANSFER_MAX_DIRECT_CARRIERS;
    for carrier in 0..rounds {
        let request = format!("{:032x}", 0xA000 + carrier);
        peer.send_text(format!(
            r#"{{"v":1,"type":"rtc.offer","requestId":"{request}","body":{{"transferId":"{t}","attemptId":"{a}","carrier":{c},"sdp":"v=0"}}}}"#,
            t = "dd".repeat(16),
            a = "ee".repeat(16),
            c = carrier % bore_cli::web_transfer::WEB_TRANSFER_MAX_DIRECT_CARRIERS,
        ))
        .await?;
        let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("offer reply"));
        assert_eq!(typ, "error", "an unknown transfer is an error");
        assert_ne!(
            body["code"].as_str(),
            Some("RATE_LIMITED"),
            "carrier {carrier} of {rounds} was refused for RATE, not for the unknown transfer",
        );
    }

    // And the budget the bucket exists for is still there: a real mutation
    // right after the whole signalling round must be served.
    peer.send_text(format!(
        r#"{{"v":1,"type":"peer.rename","requestId":"{}","body":{{"displayName":"Ada"}}}}"#,
        "11".repeat(16),
    ))
    .await?;
    let (typ, body) = control_msg(&peer.next_text(wait).await?.expect("rename reply"));
    assert_eq!(
        typ, "ack",
        "the mutation after the signalling round was refused: {body}",
    );
    Ok(())
}

#[tokio::test]
async fn t_web_auth_refusals_are_one_class() -> Result<()> {
    use bore_cli::web_transfer::{
        MemberToken, OwnerLease, OwnerToken, WEB_TRANSFER_AUTH_FAIL_DELAY,
    };

    let port = support::free_port().await?;
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{port}/"));
    let resolved = bore_cli::web_transfer::resolve_server_config(&args, false, port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(port);
    server.set_web_transfer(resolved)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(port, true).await;

    let member = MemberToken::from_bytes([0x51u8; 32]);
    let owner = OwnerToken::from_bytes([0x52u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let wait = Duration::from_secs(10);

    // A room id that is well-formed and names nothing.
    let absent_room = "9".repeat(room_hex.len());
    let wrong_token = MemberToken::from_bytes([0x53u8; 32]).to_string();

    let good_token = member.to_string();
    let cases: [(&str, &str, &str); 3] = [
        ("wrong token", room_hex.as_str(), wrong_token.as_str()),
        ("absent room", absent_room.as_str(), good_token.as_str()),
        ("malformed token", room_hex.as_str(), "not-hex"),
    ];

    let mut codes = Vec::new();
    for (what, room, token) in cases {
        let started = std::time::Instant::now();
        let mut peer = support::WsPeer::connect(&host, room, &origin)
            .await
            .with_context(|| format!("connect for {what}"))?;
        peer.hello(token, None)
            .await
            .with_context(|| format!("hello for {what}"))?;
        let code = peer
            .close_code(wait)
            .await
            .with_context(|| format!("close for {what}"))?;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= WEB_TRANSFER_AUTH_FAIL_DELAY,
            "{what} was refused in {elapsed:?}, before the uniform delay"
        );
        codes.push((what, code));
    }
    let first = codes[0].1;
    assert!(
        first.is_some(),
        "a refusal carried no close code: {codes:?}"
    );
    for (what, code) in &codes {
        assert_eq!(
            *code, first,
            "{what} is distinguishable from the other refusals: {codes:?}"
        );
    }

    // Positive control: the RIGHT token on the SAME room still authenticates,
    // so the uniformity above is not a server that refuses everything.
    let mut good = support::WsPeer::connect(&host, &room_hex, &origin)
        .await
        .context("connect good")?;
    good.hello(&good_token, Some("good"))
        .await
        .context("hello good")?;
    let _ = expect_type(&mut good, "welcome", wait)
        .await
        .context("welcome good")?;
    Ok(())
}

/// 6.4: the crate ARTIFACT carries the bundle and not the node tree.
///
/// `cargo install bore-cli` must need no npm, which is why `web/transfer/dist`
/// is committed — but the file that makes that true is the PACKAGE file list,
/// not this checkout. A `dist` left out of the package compiles here and
/// serves nothing there, and a `node_modules` swept in makes the crate tens of
/// megabytes of files nobody can use.
///
/// The list is the cheap half of `T-WEB-PACKAGE`
/// (`scripts/web_transfer_package_test.sh` builds and serves from the packed
/// source); this one runs in every ordinary test run, which is where a
/// `Cargo.toml` edit gets made.
#[test]
fn cargo_package_contains_dist_but_not_node_modules() {
    // Its own target dir: a child `cargo` sharing this run's would contend
    // for the build lock, and a test that waits on the test runner is a test
    // that hangs.
    let scratch = std::env::temp_dir().join("bore-package-list");
    let output = std::process::Command::new(env!("CARGO"))
        .args(["package", "--allow-dirty", "--no-verify", "--list"])
        .env("CARGO_TARGET_DIR", &scratch)
        .output()
        .expect("run cargo package --list");
    assert!(
        output.status.success(),
        "cargo package --list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let list = String::from_utf8_lossy(&output.stdout);
    for asset in [
        "web/transfer/dist/index.html",
        "web/transfer/dist/app.js",
        "web/transfer/dist/app.css",
        "web/transfer/dist/offer-worker.js",
        "web/transfer/dist/stage-worker.js",
    ] {
        assert!(
            list.lines().any(|line| line.trim() == asset),
            "the crate does not carry {asset}"
        );
    }
    for unwanted in ["node_modules", "test-results", "playwright-report"] {
        assert!(
            !list.contains(unwanted),
            "the crate carries {unwanted}, which no user of it can use"
        );
    }
}

/// `T-WEB-DEPLOY` (6.5): the base URL is the PUBLIC origin, and the hop from
/// the reverse proxy to bore is a different socket entirely.
///
/// This is the shape every TLS deployment has: a proxy terminates HTTPS on
/// the origin users type and forwards plain HTTP to bore on loopback. bore
/// never reads `X-Forwarded-*` — it compares `Host` and `Origin` against the
/// CONFIGURED base URL — so the question this gate answers is whether that
/// comparison lets the real topology work while still refusing a foreign
/// origin. Both halves matter: a check that passed everything would also
/// "work" here.
///
/// The proxy is a byte pipe, which is exactly what a correctly configured
/// reverse proxy is on these routes: it must not buffer, must pass the
/// `Upgrade` through, and must leave `Host` and `Origin` alone.
#[tokio::test]
async fn t_web_deploy_behind_reverse_proxy() -> Result<()> {
    use bore_cli::web_transfer::{MemberToken, OwnerLease, OwnerToken};

    let proxy_port = support::free_port().await?;
    let server_port = support::free_port().await?;
    // The advertised origin is the PROXY's, not the listener's.
    let mut args = support::enabled_args();
    args.base_url = Some(format!("http://127.0.0.1:{proxy_port}/"));
    let resolved = bore_cli::web_transfer::resolve_server_config(&args, false, server_port)?
        .expect("config resolves");
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(server_port);
    server.set_web_transfer(resolved)?;
    let registry = server.web_transfer().expect("registry enabled");
    tokio::spawn(server.listen());
    support::wait_port(server_port, true).await;
    let proxy = support::spawn_proxy(proxy_port, server_port).await?;
    assert_eq!(proxy.port(), proxy_port);

    let member = MemberToken::from_bytes([0x61u8; 32]);
    let owner = OwnerToken::from_bytes([0x62u8; 32]);
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let room_hex = lease.id().to_string();
    let token_hex = member.to_string();
    let public = format!("127.0.0.1:{proxy_port}");
    let origin = format!("http://127.0.0.1:{proxy_port}");
    let wait = Duration::from_secs(10);

    // The room shell, through the proxy.
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", proxy_port)).await?;
    let response = http_exchange(
        stream,
        &format!(
            "GET /transfer/{room_hex} HTTP/1.1\r\nHost: {public}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await?;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "the shell was not served through the proxy: {}",
        response.lines().next().unwrap_or_default()
    );

    // The control socket, through the proxy, with the public authority: a
    // real transfer's worth of lifecycle is covered elsewhere, what this
    // gate needs is that the same-origin check accepts the deployment.
    let mut peer = support::WsPeer::connect(&public, &room_hex, &origin).await?;
    peer.hello(&token_hex, Some("deploy")).await?;
    let _ = expect_type(&mut peer, "welcome", wait)
        .await
        .context("welcome through the proxy")?;

    // And the other half: the LISTENER's own authority is not the advertised
    // one, so a client that bypasses the proxy is refused. Without this the
    // test above would pass on a server that checks nothing.
    let direct = format!("127.0.0.1:{server_port}");
    let bypass = support::WsPeer::connect(&direct, &room_hex, &format!("http://{direct}")).await;
    assert!(
        bypass.is_err(),
        "a client bypassing the proxy was accepted with the wrong authority"
    );
    Ok(())
}
