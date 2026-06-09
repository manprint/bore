use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
#[cfg(feature = "udp")]
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{bail, ensure, Result};
use bore_cli::{
    client::{Client, ProviderMeta},
    secret::Proxy,
    server::Server,
    shared::TunnelOptions,
    transport::{self, Endpoint},
    vhost::{VhostConfig, VhostModeCfg},
};
#[cfg(feature = "udp")]
use bore_cli::vhost::VhostRegistry;
use rcgen::generate_simple_self_signed;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time;

async fn wait_port(port: u16, listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() == listening {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

async fn read_http_head<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>> {
    const MAX: usize = 16 * 1024;
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let scan_from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if buf[scan_from..].windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX {
            break;
        }
    }
    Ok(buf)
}

async fn write_ws_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    opcode: u8,
    payload: &[u8],
    masked: bool,
) -> Result<()> {
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.push(0x80 | (opcode & 0x0f));

    let mask_bit = if masked { 0x80 } else { 0 };
    match payload.len() {
        0..=125 => frame.push(mask_bit | payload.len() as u8),
        126..=0xffff => {
            frame.push(mask_bit | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        _ => {
            frame.push(mask_bit | 127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
    }

    if masked {
        let mask = [0x13, 0x37, 0x42, 0x99];
        frame.extend_from_slice(&mask);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }
    } else {
        frame.extend_from_slice(payload);
    }

    stream.write_all(&frame).await?;
    Ok(())
}

async fn read_ws_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await?;

    ensure!(header[0] & 0x70 == 0, "RSV bits are unsupported in test helper");
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    let len = match header[1] & 0x7f {
        n @ 0..=125 => n as usize,
        126 => {
            let mut ext = [0u8; 2];
            stream.read_exact(&mut ext).await?;
            u16::from_be_bytes(ext) as usize
        }
        127 => {
            let mut ext = [0u8; 8];
            stream.read_exact(&mut ext).await?;
            let len = u64::from_be_bytes(ext);
            ensure!(len <= usize::MAX as u64, "frame too large for this platform");
            len as usize
        }
        _ => unreachable!("payload length is masked to 7 bits"),
    };

    let mut mask = [0u8; 4];
    if masked {
        stream.read_exact(&mut mask).await?;
    }

    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload).await?;
    }
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }

    Ok((opcode, payload))
}

async fn websocket_echo_peer(mut stream: TcpStream) -> Result<()> {
    let head = read_http_head(&mut stream).await?;
    let request = String::from_utf8_lossy(&head).to_lowercase();
    ensure!(request.starts_with("get "), "expected HTTP GET upgrade request");
    ensure!(request.contains("upgrade: websocket"), "missing websocket upgrade header");
    ensure!(request.contains("connection: upgrade"), "missing connection upgrade header");
    ensure!(request.contains("sec-websocket-key:"), "missing websocket key header");

    stream
        .write_all(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: bore-test\r\n\r\n",
        )
        .await?;

    loop {
        let (opcode, payload) = read_ws_frame(&mut stream).await?;
        match opcode {
            0x1 | 0x2 => write_ws_frame(&mut stream, opcode, &payload, false).await?,
            0x8 => {
                write_ws_frame(&mut stream, 0x8, &payload, false).await?;
                return Ok(());
            }
            0x9 => write_ws_frame(&mut stream, 0xA, &payload, false).await?,
            other => bail!("unexpected websocket opcode {other}"),
        }
    }
}

fn spawn_websocket_echo_listener(listener: TcpListener) {
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(async move {
                let _ = websocket_echo_peer(stream).await;
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    });
}

async fn spawn_websocket_echo_service() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    spawn_websocket_echo_listener(listener);
    Ok(port)
}

async fn websocket_round_trip<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    path: &str,
) -> Result<()> {
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGVzdC1ib3JlLXdzLWtleQ==\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;

    let response = read_http_head(stream).await?;
    let response_text = String::from_utf8_lossy(&response).to_lowercase();
    ensure!(
        response_text.starts_with("http/1.1 101"),
        "expected websocket 101 response, got: {}",
        String::from_utf8_lossy(&response)
    );
    ensure!(response_text.contains("upgrade: websocket"), "missing upgrade response header");
    ensure!(response_text.contains("connection: upgrade"), "missing connection upgrade response header");

    let text_payload = b"hello websocket through bore";
    write_ws_frame(stream, 0x1, text_payload, true).await?;
    let (opcode, payload) = read_ws_frame(stream).await?;
    ensure!(opcode == 0x1, "expected echoed text frame");
    ensure!(payload == text_payload, "text websocket payload mismatch");

    let binary_payload: Vec<u8> = (0..200).map(|n| (n % 251) as u8).collect();
    write_ws_frame(stream, 0x2, &binary_payload, true).await?;
    let (opcode, payload) = read_ws_frame(stream).await?;
    ensure!(opcode == 0x2, "expected echoed binary frame");
    ensure!(payload == binary_payload, "binary websocket payload mismatch");

    write_ws_frame(stream, 0x8, &[], true).await?;
    let (opcode, _) = read_ws_frame(stream).await?;
    ensure!(opcode == 0x8, "expected websocket close frame");
    Ok(())
}

fn self_signed_for(alt_names: Vec<String>) -> Result<(String, String)> {
    let cert = generate_simple_self_signed(alt_names)?;
    Ok((cert.cert.pem(), cert.signing_key.serialize_pem()))
}

fn write_pem_files(cert_pem: &str, key_pem: &str) -> Result<(PathBuf, PathBuf)> {
    let id = uuid::Uuid::new_v4();
    let mut cert_path = std::env::temp_dir();
    cert_path.push(format!("bore_ws_test_{id}_cert.pem"));
    let mut key_path = std::env::temp_dir();
    key_path.push(format!("bore_ws_test_{id}_key.pem"));
    std::fs::write(&cert_path, cert_pem)?;
    std::fs::write(&key_path, key_pem)?;
    Ok((cert_path, key_path))
}

fn http_config(base_domain: &str, http_port: u16) -> VhostConfig {
    VhostConfig {
        base_domain: base_domain.to_string(),
        mode: VhostModeCfg::Http,
        http_port,
        https_port: 443,
        cert_file: None,
        key_file: None,
        default_headers: BTreeMap::new(),
        reservations: vec![],
    }
}

#[cfg(feature = "udp")]
async fn wait_for_vhost_direct(registry: &VhostRegistry, subdomain: &str, expected: bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let direct = registry
            .get(subdomain)
            .map(|entry| entry.direct.read().unwrap().is_some())
            .unwrap_or(false);
        if direct == expected {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for direct={expected} on {subdomain}");
        }
        time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(feature = "udp")]
fn direct_stream_opens(registry: &VhostRegistry, subdomain: &str) -> u64 {
    registry
        .get(subdomain)
        .map(|entry| entry.direct_stream_opens.load(Ordering::Relaxed))
        .unwrap_or(0)
}

#[tokio::test]
async fn public_tunnel_websocket_round_trip() -> Result<()> {
    const CONTROL: u16 = 18100;
    const SECRET: &str = "public-websocket-secret";

    wait_port(CONTROL, false).await;
    let mut server = Server::new(1024..=65535, Some(SECRET));
    server.set_control_port(CONTROL);
    tokio::spawn(server.listen());
    wait_port(CONTROL, true).await;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_port = listener.local_addr()?.port();
    spawn_websocket_echo_listener(listener);

    let client = Client::new(
        "127.0.0.1",
        local_port,
        &format!("localhost:{CONTROL}"),
        0,
        Some(SECRET),
        false,
        Default::default(),
    )
    .await?;
    let addr: SocketAddr = ([127, 0, 0, 1], client.remote_port()).into();
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(50)).await;

    let mut stream = TcpStream::connect(addr).await?;
    websocket_round_trip(&mut stream, "public.example.test", "/socket").await
}

#[tokio::test]
async fn public_tunnel_tls_terminated_websocket_round_trip() -> Result<()> {
    const CONTROL: u16 = 18101;
    const SECRET: &str = "public-websocket-tls-secret";

    let (cert_pem, key_pem) = self_signed_for(vec!["localhost".to_string()])?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;

    wait_port(CONTROL, false).await;
    let mut server = Server::new(1024..=65535, Some(SECRET));
    server.set_control_port(CONTROL);
    server.set_tls(acceptor);
    tokio::spawn(server.listen());
    wait_port(CONTROL, true).await;

    let stub_port = spawn_websocket_echo_service().await?;
    let options = TunnelOptions {
        https: true,
        ..Default::default()
    };
    let client = Client::new(
        "127.0.0.1",
        stub_port,
        &format!("https://localhost:{CONTROL}"),
        0,
        Some(SECRET),
        true,
        options,
    )
    .await?;
    let endpoint = Endpoint {
        host: "127.0.0.1".to_string(),
        port: client.remote_port(),
        tls: true,
    };
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(50)).await;

    let mut tls = transport::connect(&endpoint, true).await?;
    websocket_round_trip(&mut tls, "public.example.test", "/wss").await
}

#[tokio::test]
async fn secret_proxy_websocket_relay_round_trip() -> Result<()> {
    const CONTROL: u16 = 18102;
    const SECRET: &str = "secret-websocket-relay";

    wait_port(CONTROL, false).await;
    let mut server = Server::new(1024..=65535, Some(SECRET));
    server.set_control_port(CONTROL);
    tokio::spawn(server.listen());
    wait_port(CONTROL, true).await;

    let stub_port = spawn_websocket_echo_service().await?;
    let provider = Client::new_secret_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CONTROL}"),
        "secret-ws",
        Some(SECRET),
        false,
        false,
        None,
        false,
        false,
        0,
        0,
        1024,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(provider.listen());
    time::sleep(Duration::from_millis(100)).await;

    let proxy = Proxy::new(
        &format!("localhost:{CONTROL}"),
        "127.0.0.1:0".parse()?,
        "secret-ws",
        Some(SECRET),
        false,
        false,
        None,
        false,
        false,
        0,
        0,
        1,
        None,
    )
    .await?;
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(50)).await;

    let mut stream = TcpStream::connect(addr).await?;
    websocket_round_trip(&mut stream, "secret.example.test", "/relay").await
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_proxy_websocket_direct_udp_round_trip() -> Result<()> {
    const CONTROL: u16 = 18103;
    const SECRET: &str = "secret-websocket-udp";

    wait_port(CONTROL, false).await;
    let mut server = Server::new(1024..=65535, Some(SECRET));
    server.set_control_port(CONTROL);
    server.set_udp(true);
    tokio::spawn(server.listen());
    wait_port(CONTROL, true).await;

    let stub_port = spawn_websocket_echo_service().await?;
    let provider = Client::new_secret_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CONTROL}"),
        "secret-ws-udp",
        Some(SECRET),
        false,
        true,
        None,
        false,
        false,
        0,
        0,
        1024,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(provider.listen());
    time::sleep(Duration::from_millis(250)).await;

    let proxy = Proxy::new(
        &format!("localhost:{CONTROL}"),
        "127.0.0.1:0".parse()?,
        "secret-ws-udp",
        Some(SECRET),
        false,
        true,
        None,
        false,
        false,
        0,
        0,
        1,
        None,
    )
    .await?;
    ensure!(proxy.is_direct(), "expected secret websocket test to use direct UDP");
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(50)).await;

    let mut stream = TcpStream::connect(addr).await?;
    websocket_round_trip(&mut stream, "secret.example.test", "/udp").await
}

#[tokio::test]
async fn vhost_websocket_http_relay_round_trip() -> Result<()> {
    const CONTROL: u16 = 18110;
    const HTTP: u16 = 18111;

    let stub_port = spawn_websocket_echo_service().await?;

    wait_port(CONTROL, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(CONTROL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_vhost(http_config("bore.local", HTTP))?;
    tokio::spawn(server.listen());
    wait_port(CONTROL, true).await;
    wait_port(HTTP, true).await;

    let client = Client::new_vhost_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CONTROL}"),
        "wsapp",
        "client1",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(100)).await;

    let mut stream = TcpStream::connect(("127.0.0.1", HTTP)).await?;
    websocket_round_trip(&mut stream, "wsapp.bore.local", "/chat").await
}

#[tokio::test]
async fn vhost_websocket_https_relay_round_trip() -> Result<()> {
    const CONTROL: u16 = 18112;
    const HTTPS: u16 = 18113;

    let stub_port = spawn_websocket_echo_service().await?;
    let (cert_pem, key_pem) =
        self_signed_for(vec!["*.bore.local".to_string(), "bore.local".to_string()])?;
    let (cert_path, key_path) = write_pem_files(&cert_pem, &key_pem)?;

    let cfg = VhostConfig {
        base_domain: "bore.local".to_string(),
        mode: VhostModeCfg::Https,
        http_port: 80,
        https_port: HTTPS,
        cert_file: Some(cert_path),
        key_file: Some(key_path),
        default_headers: BTreeMap::new(),
        reservations: vec![],
    };

    wait_port(CONTROL, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(CONTROL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_vhost(cfg)?;
    tokio::spawn(server.listen());
    wait_port(CONTROL, true).await;
    wait_port(HTTPS, true).await;

    let client = Client::new_vhost_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CONTROL}"),
        "wssapp",
        "client1",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(100)).await;

    let endpoint = Endpoint {
        host: "127.0.0.1".to_string(),
        port: HTTPS,
        tls: true,
    };
    let mut tls = transport::connect(&endpoint, true).await?;
    websocket_round_trip(&mut tls, "wssapp.bore.local", "/chat").await
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vhost_websocket_https_udp_round_trip() -> Result<()> {
    const CONTROL: u16 = 18114;
    const HTTPS: u16 = 18115;
    const QUIC: u16 = 18116;
    const SECRET: &str = "vhost-websocket-udp";

    let stub_port = spawn_websocket_echo_service().await?;
    let (cert_pem, key_pem) =
        self_signed_for(vec!["*.bore.local".to_string(), "bore.local".to_string()])?;
    let (cert_path, key_path) = write_pem_files(&cert_pem, &key_pem)?;

    let cfg = VhostConfig {
        base_domain: "bore.local".to_string(),
        mode: VhostModeCfg::Https,
        http_port: 80,
        https_port: HTTPS,
        cert_file: Some(cert_path),
        key_file: Some(key_path),
        default_headers: BTreeMap::new(),
        reservations: vec![],
    };

    wait_port(CONTROL, false).await;
    let mut server = Server::new(1024..=65535, Some(SECRET));
    server.set_control_port(CONTROL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_udp(true);
    server.set_vhost(cfg)?;
    server.set_vhost_quic_port(QUIC);
    let registry = server.vhost_registry();
    tokio::spawn(server.listen());
    wait_port(CONTROL, true).await;
    wait_port(HTTPS, true).await;

    let client = Client::new_vhost_provider_with_udp(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CONTROL}"),
        "wssudp",
        "client1",
        Some(SECRET),
        false,
        1,
        true,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());

    wait_for_vhost_direct(&registry, "wssudp", true).await;

    let endpoint = Endpoint {
        host: "127.0.0.1".to_string(),
        port: HTTPS,
        tls: true,
    };
    let mut tls = transport::connect(&endpoint, true).await?;
    websocket_round_trip(&mut tls, "wssudp.bore.local", "/chat").await?;
    ensure!(
        direct_stream_opens(&registry, "wssudp") >= 1,
        "expected websocket session to use the vhost UDP direct path"
    );
    Ok(())
}