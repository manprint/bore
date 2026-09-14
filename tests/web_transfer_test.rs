//! Web-transfer integration tests. Serial (`--test-threads=1`): every test
//! owns dynamic ports; no two share a control port.

#[path = "support/web_transfer.rs"]
mod support;

use anyhow::{Context, Result};
use bore_cli::server::Server;
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
            Some("bore-transfer-v1"),
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
                Some("bore-transfer-v1"),
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
                Some("bore-transfer-v1"),
            ),
        ),
        (
            "host",
            ws_upgrade_request(
                "other.invalid",
                &ws_path,
                &base[..base.len() - 1],
                Some("bore-transfer-v1"),
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
            Some("bore-transfer-v1"),
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
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash())
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
        self.0.lock().unwrap().extend_from_slice(buf);
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
    let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash())
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
    let lease2 = OwnerLease::create(&registry2, member2.sha256_hash(), owner2.sha256_hash())
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
            let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash())
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
            let lease = OwnerLease::create(&registry, member.sha256_hash(), owner.sha256_hash())
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
