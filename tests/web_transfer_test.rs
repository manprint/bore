//! Web-transfer integration tests. Serial (`--test-threads=1`): every test
//! owns dynamic ports; no two share a control port.

#[path = "support/web_transfer.rs"]
mod support;

use anyhow::{Context, Result};
use bore_cli::server::Server;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::TcpStream;

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
        // Give the process a moment: a VALID config would bind by now, an
        // invalid one exits nonzero first.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let refused = TcpStream::connect(("127.0.0.1", port)).await.is_err();
        match child.try_wait()? {
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

    let lease = OwnerLease::create(&registry, member_hash, owner_hash)
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
        .create_room_with_id(member_hash, owner_hash, forced)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    OwnerLease::detach_with_grace(&first, Duration::from_millis(200));
    assert!(registry.remove_room_if_current(forced, &first));
    first.destroy("owner-close");
    let second = registry
        .create_room_with_id([8u8; 32], [8u8; 32], forced)
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
    use bore_cli::web_transfer::{OwnerToken, WebTransferLimits};

    let member = OwnerToken::from_bytes([0x31u8; 32]);
    let owner = OwnerToken::from_bytes([0x32u8; 32]);
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
            version: 1,
            member_token_hash: member_hash,
            owner_token_hash: owner_hash,
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
            version: 1,
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
            version: 1,
            member_token_hash: member_hash,
            owner_token_hash: owner_hash,
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
            version: 2,
            member_token_hash: member_hash,
            owner_token_hash: owner_hash,
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

/// Captured tracing output for the log-privacy assertion.
#[derive(Clone)]
struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
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
