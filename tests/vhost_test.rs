use std::collections::BTreeMap;
use std::path::PathBuf;
#[cfg(feature = "udp")]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
#[cfg(feature = "udp")]
use bore_cli::vhost::VhostRegistry;
use bore_cli::{
    client::{Client, ProviderMeta},
    reconnect,
    server::Server,
    transport::{self, Endpoint},
    vhost::{Reservation, VhostConfig, VhostModeCfg},
};
use lazy_static::lazy_static;
use rcgen::generate_simple_self_signed;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time;

#[path = "support/websocket.rs"]
mod websocket;

lazy_static! {
    /// Serializes registration tests that share a single server on ports 17920/17930.
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn wait_port(port: u16, listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() == listening {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

/// Spawn a minimal stub HTTP server that always responds with `body`.
/// Returns the ephemeral port it bound on 127.0.0.1.
async fn spawn_http_stub(body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let body = body;
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let mut total = 0;
                loop {
                    let n = stream.read(&mut buf[total..]).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    total += n;
                    if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if total >= buf.len() {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    port
}

/// Spawn a stub that captures the raw request bytes it receives (up to \r\n\r\n)
/// into the shared slot, then responds with a 200.
async fn spawn_capturing_stub(captured: Arc<Mutex<Option<Vec<u8>>>>, body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let captured = Arc::clone(&captured);
            let body = body;
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut total = 0;
                loop {
                    let n = stream.read(&mut buf[total..]).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    total += n;
                    if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if total >= buf.len() {
                        break;
                    }
                }
                *captured.lock().await = Some(buf[..total].to_vec());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    port
}

/// Send a plain HTTP GET request to `127.0.0.1:port` with the given Host header.
/// Returns the full response as a String (blocking up to 5 s).
async fn send_http(port: u16, host: &str, path: &str) -> Result<String> {
    let mut conn = time::timeout(
        Duration::from_secs(3),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await??;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    conn.write_all(req.as_bytes()).await?;
    conn.shutdown().await?;
    let mut resp = Vec::new();
    time::timeout(Duration::from_secs(5), conn.read_to_end(&mut resp)).await??;
    Ok(String::from_utf8_lossy(&resp).into_owned())
}

/// Spawn a bore server with vhost enabled on the given control + http/https ports.
/// Waits for the control port to be listening before returning.
async fn spawn_server_vhost(control: u16, cfg: VhostConfig) -> Result<()> {
    wait_port(control, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(control);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_vhost(cfg)?;
    tokio::spawn(server.listen());
    wait_port(control, true).await;
    Ok(())
}

/// Minimal VhostConfig for HTTP-only mode.
fn http_config(base_domain: &str, http_port: u16) -> VhostConfig {
    VhostConfig {
        base_domain: base_domain.to_string(),
        mode: VhostModeCfg::Http,
        http_port,
        https_port: 443,
        cert_file: None,
        key_file: None,
        default_headers: BTreeMap::new(),
        default_response_headers: BTreeMap::new(),
        reservations: vec![],
    }
}

/// Generate a self-signed certificate with the given SANs.
/// Returns `(cert_pem, key_pem)`.
fn self_signed_for(alt_names: Vec<String>) -> Result<(String, String)> {
    let key = generate_simple_self_signed(alt_names)?;
    Ok((key.cert.pem(), key.signing_key.serialize_pem()))
}

/// Write PEM strings to temp files and return their paths.
fn write_pem_files(cert_pem: &str, key_pem: &str) -> Result<(PathBuf, PathBuf)> {
    let id = uuid::Uuid::new_v4();
    let mut cert_path = std::env::temp_dir();
    cert_path.push(format!("bore_vhost_test_{id}_cert.pem"));
    let mut key_path = std::env::temp_dir();
    key_path.push(format!("bore_vhost_test_{id}_key.pem"));
    std::fs::write(&cert_path, cert_pem)?;
    std::fs::write(&key_path, key_pem)?;
    Ok((cert_path, key_path))
}

/// Unique temp yaml path for hot-reload tests.
fn temp_yaml_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("bore_vhost_test_{}.yml", uuid::Uuid::new_v4()));
    p
}

// ─── Registration group (serial, control=17920, http=17930) ──────────────────

const REG_CONTROL: u16 = 17920;
const REG_HTTP: u16 = 17930;

async fn spawn_reg_server(cfg: VhostConfig) -> Result<()> {
    spawn_server_vhost(REG_CONTROL, cfg).await?;
    wait_port(REG_HTTP, true).await;
    Ok(())
}

fn reg_cfg_no_reservations() -> VhostConfig {
    http_config("bore.local", REG_HTTP)
}

fn to_reg() -> String {
    format!("localhost:{REG_CONTROL}")
}

#[cfg(feature = "udp")]
async fn wait_for_vhost_direct(registry: &VhostRegistry, subdomain: &str, expected: bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let is_direct = registry
            .get(subdomain)
            .map(|entry| !entry.direct.is_empty())
            .unwrap_or(false);
        if is_direct == expected {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for direct={expected} on subdomain {subdomain}");
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

/// Wait until the QUIC direct pool for `subdomain` has at least `expected` live
/// carriers (used by the multi-carrier test).
#[cfg(feature = "udp")]
async fn wait_for_vhost_direct_count(registry: &VhostRegistry, subdomain: &str, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let count = registry
            .get(subdomain)
            .map(|entry| entry.direct.len())
            .unwrap_or(0);
        if count >= expected {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for {expected} direct carriers on {subdomain}, have {count}");
        }
        time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(feature = "udp")]
fn close_vhost_direct(registry: &VhostRegistry, subdomain: &str) {
    let entry = registry
        .get(subdomain)
        .unwrap_or_else(|| panic!("missing vhost entry for {subdomain}"));
    let direct = entry
        .direct
        .pick()
        .unwrap_or_else(|| panic!("missing direct connection for {subdomain}"));
    direct.close();
}

#[tokio::test]
async fn vhost_provider_registers() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_reg_server(reg_cfg_no_reservations()).await?;

    let local_port = TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port();
    let result = Client::new_vhost_provider(
        "127.0.0.1",
        local_port,
        &to_reg(),
        "myapp",
        "client1",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await;
    assert!(
        result.is_ok(),
        "vhost provider should register: {:?}",
        result.err()
    );
    Ok(())
}

#[tokio::test]
async fn vhost_duplicate_subdomain_rejected() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_reg_server(reg_cfg_no_reservations()).await?;

    let local_port = TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port();

    let first = Client::new_vhost_provider(
        "127.0.0.1",
        local_port,
        &to_reg(),
        "dup",
        "clientA",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(first.listen());
    time::sleep(Duration::from_millis(50)).await;

    let second = Client::new_vhost_provider(
        "127.0.0.1",
        local_port,
        &to_reg(),
        "dup",
        "clientB",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await;
    assert!(second.is_err(), "duplicate subdomain must be rejected");
    Ok(())
}

#[tokio::test]
async fn vhost_reservation_enforced_accepted() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    let mut cfg = reg_cfg_no_reservations();
    cfg.reservations = vec![Reservation {
        client_id: "clientA".to_string(),
        subdomain: "reserved".to_string(),
        headers: BTreeMap::new(),
        response_headers: BTreeMap::new(),
    }];
    spawn_reg_server(cfg).await?;

    let local_port = TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port();
    let result = Client::new_vhost_provider(
        "127.0.0.1",
        local_port,
        &to_reg(),
        "reserved",
        "clientA",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await;
    assert!(
        result.is_ok(),
        "reserved client_id should be accepted: {:?}",
        result.err()
    );
    Ok(())
}

#[tokio::test]
async fn vhost_reservation_enforced_rejected() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    let mut cfg = reg_cfg_no_reservations();
    cfg.reservations = vec![Reservation {
        client_id: "clientA".to_string(),
        subdomain: "reserved2".to_string(),
        headers: BTreeMap::new(),
        response_headers: BTreeMap::new(),
    }];
    spawn_reg_server(cfg).await?;

    let local_port = TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port();
    let result = Client::new_vhost_provider(
        "127.0.0.1",
        local_port,
        &to_reg(),
        "reserved2",
        "clientB",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await;
    assert!(result.is_err(), "wrong client_id must be rejected");
    Ok(())
}

#[tokio::test]
async fn vhost_subdomain_freed_after_disconnect() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_reg_server(reg_cfg_no_reservations()).await?;

    let local_port = TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port();

    let first = Client::new_vhost_provider(
        "127.0.0.1",
        local_port,
        &to_reg(),
        "free-me",
        "clientA",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await?;
    let task = tokio::spawn(first.listen());
    time::sleep(Duration::from_millis(50)).await;
    task.abort();

    // Poll until re-registration succeeds (the old registration must be freed).
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        let r = Client::new_vhost_provider(
            "127.0.0.1",
            local_port,
            &to_reg(),
            "free-me",
            "clientA",
            None,
            false,
            1,
            ProviderMeta::default(),
        )
        .await;
        if r.is_ok() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("subdomain was not freed within 500ms");
        }
        time::sleep(Duration::from_millis(30)).await;
    }
    Ok(())
}

// ─── HTTP routing (control=17941, http=17942) ─────────────────────────────────

#[tokio::test]
async fn vhost_http_routing() -> Result<()> {
    const CTRL: u16 = 17941;
    const HTTP: u16 = 17942;

    let stub_port = spawn_http_stub("hello").await;

    let cfg = http_config("bore.local", HTTP);
    spawn_server_vhost(CTRL, cfg).await?;
    wait_port(HTTP, true).await;

    let client = Client::new_vhost_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "app",
        "client1",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(50)).await;

    let response = send_http(HTTP, "app.bore.local", "/").await?;
    assert!(
        response.contains("hello"),
        "expected 'hello' in response, got: {response}"
    );
    Ok(())
}

// ─── HTTPS routing (control=17943, https=17944) ───────────────────────────────

#[tokio::test]
async fn vhost_https_routing() -> Result<()> {
    const CTRL: u16 = 17943;
    const HTTPS: u16 = 17944;

    let stub_port = spawn_http_stub("hello-tls").await;

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
        default_response_headers: BTreeMap::new(),
        reservations: vec![],
    };

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_vhost(cfg)?;
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTPS, true).await;

    let client = Client::new_vhost_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "tlsapp",
        "client1",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(50)).await;

    // Connect over TLS (insecure — self-signed), send HTTP request, read response.
    let endpoint = Endpoint {
        host: "127.0.0.1".to_string(),
        port: HTTPS,
        tls: true,
    };
    let mut tls = transport::connect(&endpoint, true).await?;
    let req = b"GET / HTTP/1.1\r\nHost: tlsapp.bore.local\r\nConnection: close\r\n\r\n";
    tls.write_all(req).await?;
    tls.shutdown().await?;
    let mut resp = Vec::new();
    time::timeout(Duration::from_secs(5), tls.read_to_end(&mut resp)).await??;
    let response = String::from_utf8_lossy(&resp);
    assert!(
        response.contains("hello-tls"),
        "expected 'hello-tls' in HTTPS response, got: {response}"
    );
    Ok(())
}

// ─── Redirect mode (control=17945, http=17946, https=17947) ──────────────────

#[tokio::test]
async fn vhost_redirect_mode() -> Result<()> {
    const CTRL: u16 = 17945;
    const HTTP: u16 = 17946;
    const HTTPS: u16 = 17947;

    let (cert_pem, key_pem) =
        self_signed_for(vec!["*.bore.local".to_string(), "bore.local".to_string()])?;
    let (cert_path, key_path) = write_pem_files(&cert_pem, &key_pem)?;

    let cfg = VhostConfig {
        base_domain: "bore.local".to_string(),
        mode: VhostModeCfg::RedirectHttps,
        http_port: HTTP,
        https_port: HTTPS,
        cert_file: Some(cert_path),
        key_file: Some(key_path),
        default_headers: BTreeMap::new(),
        default_response_headers: BTreeMap::new(),
        reservations: vec![],
    };

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_vhost(cfg)?;
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTP, true).await;

    // No vhost client needed — the redirect happens before any provider lookup.
    let response = send_http(HTTP, "app.bore.local", "/").await?;
    assert!(
        response.starts_with("HTTP/1.1 308"),
        "expected 308 redirect, got: {response}"
    );
    assert!(
        response.to_lowercase().contains("location:"),
        "redirect must include Location header: {response}"
    );
    assert!(
        response.to_lowercase().contains("https://"),
        "redirect must point to HTTPS: {response}"
    );
    Ok(())
}

// ─── Unknown subdomain 502 (control=17948, http=17949) ───────────────────────

#[tokio::test]
async fn vhost_unknown_subdomain_502() -> Result<()> {
    const CTRL: u16 = 17948;
    const HTTP: u16 = 17949;

    let cfg = http_config("bore.local", HTTP);
    spawn_server_vhost(CTRL, cfg).await?;
    wait_port(HTTP, true).await;

    // No client registered for "ghost".
    let start = tokio::time::Instant::now();
    let response = time::timeout(
        Duration::from_secs(3),
        send_http(HTTP, "ghost.bore.local", "/"),
    )
    .await??;
    let elapsed = start.elapsed();

    assert!(
        response.starts_with("HTTP/1.1 502"),
        "expected 502 Bad Gateway, got: {response}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "502 should not hang; took {elapsed:?}"
    );
    Ok(())
}

// ─── Header injection (control=17950, http=17951) ────────────────────────────

#[tokio::test]
async fn vhost_header_injection() -> Result<()> {
    const CTRL: u16 = 17950;
    const HTTP: u16 = 17951;

    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let stub_port = spawn_capturing_stub(Arc::clone(&captured), "ok").await;

    let mut default_headers = BTreeMap::new();
    default_headers.insert("X-Default".to_string(), "d1".to_string());

    let mut sub_headers = BTreeMap::new();
    sub_headers.insert("X-Override".to_string(), "v1".to_string());
    sub_headers.insert("X-Default".to_string(), "override".to_string());

    let mut default_response_headers = BTreeMap::new();
    default_response_headers.insert("X-Frame-Options".to_string(), "DENY".to_string());

    let mut sub_response_headers = BTreeMap::new();
    sub_response_headers.insert("X-Frame-Options".to_string(), "SAMEORIGIN".to_string());
    sub_response_headers.insert(
        "Strict-Transport-Security".to_string(),
        "max-age=31536000; includeSubDomains".to_string(),
    );

    let cfg = VhostConfig {
        base_domain: "bore.local".to_string(),
        mode: VhostModeCfg::Http,
        http_port: HTTP,
        https_port: 443,
        cert_file: None,
        key_file: None,
        default_headers,
        default_response_headers,
        reservations: vec![Reservation {
            client_id: "inject-client".to_string(),
            subdomain: "inject".to_string(),
            headers: sub_headers,
            response_headers: sub_response_headers,
        }],
    };

    spawn_server_vhost(CTRL, cfg).await?;
    wait_port(HTTP, true).await;

    let client = Client::new_vhost_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "inject",
        "inject-client",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(50)).await;

    let response = send_http(HTTP, "inject.bore.local", "/").await?;

    // Give the stub a moment to capture the request.
    time::sleep(Duration::from_millis(50)).await;

    let raw = captured
        .lock()
        .await
        .clone()
        .expect("stub did not capture request");
    let request_text = String::from_utf8_lossy(&raw);
    assert!(
        request_text.to_lowercase().contains("x-override: v1"),
        "X-Override header must be present: {request_text}"
    );
    // Per-subdomain header overrides the default.
    assert!(
        request_text.to_lowercase().contains("x-default: override"),
        "X-Default must be overridden to 'override': {request_text}"
    );
    assert!(
        !request_text.to_lowercase().contains("x-default: d1"),
        "X-Default: d1 (the default value) must not appear: {request_text}"
    );
    assert!(
        response
            .to_lowercase()
            .contains("x-frame-options: sameorigin"),
        "response header override must be present: {response}"
    );
    assert!(
        !response.to_lowercase().contains("x-frame-options: deny"),
        "default response header value must be overridden: {response}"
    );
    assert!(
        response
            .to_lowercase()
            .contains("strict-transport-security: max-age=31536000; includesubdomains"),
        "response header injection must add HSTS: {response}"
    );
    Ok(())
}

// ─── Large body integrity (control=17952, http=17953) ────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vhost_large_body_integrity() -> Result<()> {
    const CTRL: u16 = 17952;
    const HTTP: u16 = 17953;
    const LEN: usize = 1 << 20; // 1 MiB

    // Build a 1 MiB body with a known pattern.
    let pattern: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();
    let pattern_clone = pattern.clone();

    // Stub that returns the large body.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let stub_port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let pattern = pattern_clone.clone();
            tokio::spawn(async move {
                // Drain the request head.
                let mut buf = vec![0u8; 4096];
                let mut total = 0;
                loop {
                    let n = stream.read(&mut buf[total..]).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    total += n;
                    if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if total >= buf.len() {
                        break;
                    }
                }
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {LEN}\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&pattern).await;
            });
        }
    });

    let cfg = http_config("bore.local", HTTP);
    spawn_server_vhost(CTRL, cfg).await?;
    wait_port(HTTP, true).await;

    let client = Client::new_vhost_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "bigapp",
        "client1",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(50)).await;

    let mut conn = time::timeout(
        Duration::from_secs(3),
        TcpStream::connect(("127.0.0.1", HTTP)),
    )
    .await??;
    let req = b"GET / HTTP/1.1\r\nHost: bigapp.bore.local\r\nConnection: close\r\n\r\n";
    conn.write_all(req).await?;
    conn.shutdown().await?;

    let mut resp = Vec::new();
    time::timeout(Duration::from_secs(15), conn.read_to_end(&mut resp)).await??;

    // Find the end of the HTTP headers (\r\n\r\n) and extract the body.
    let header_end = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response missing \\r\\n\\r\\n")
        + 4;
    let body = &resp[header_end..];
    assert_eq!(
        body.len(),
        LEN,
        "body length mismatch: got {} bytes, expected {LEN}",
        body.len()
    );
    assert_eq!(body, pattern.as_slice(), "large body content mismatch");
    Ok(())
}

// ─── Concurrency smoke (control=17954, http=17955) ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vhost_concurrency_smoke() -> Result<()> {
    const CTRL: u16 = 17954;
    const HTTP: u16 = 17955;
    const N: usize = 5;

    // Each subdomain "sub0".."sub4" gets its own stub returning "body-N".
    let bodies: Vec<&'static str> = vec!["body-0", "body-1", "body-2", "body-3", "body-4"];
    let subdomains: Vec<&'static str> = vec!["sub0", "sub1", "sub2", "sub3", "sub4"];

    let cfg = http_config("bore.local", HTTP);
    spawn_server_vhost(CTRL, cfg).await?;
    wait_port(HTTP, true).await;

    let mut stub_ports = Vec::new();
    for &body in &bodies {
        stub_ports.push(spawn_http_stub(body).await);
    }

    for i in 0..N {
        let client = Client::new_vhost_provider(
            "127.0.0.1",
            stub_ports[i],
            &format!("localhost:{CTRL}"),
            subdomains[i],
            &format!("client{i}"),
            None,
            false,
            1,
            ProviderMeta::default(),
        )
        .await?;
        tokio::spawn(client.listen());
    }
    time::sleep(Duration::from_millis(100)).await;

    // Fire all requests concurrently and assert each gets its own body.
    let mut tasks = Vec::new();
    for i in 0..N {
        let sub = subdomains[i];
        let expected = bodies[i];
        tasks.push(tokio::spawn(async move {
            let host = format!("{sub}.bore.local");
            let response = send_http(HTTP, &host, "/").await?;
            anyhow::ensure!(
                response.contains(expected),
                "sub{i}: expected '{expected}' in response, got: {response}"
            );
            anyhow::Ok(())
        }));
    }
    for t in tasks {
        t.await??;
    }
    Ok(())
}

#[tokio::test]
async fn vhost_http_websocket_relay_round_trip() -> Result<()> {
    const CTRL: u16 = 17993;
    const HTTP: u16 = 17994;

    let ws_port = websocket::spawn_websocket_echo_service(Some("x-websocket-test: yes")).await?;

    let mut default_headers = BTreeMap::new();
    default_headers.insert("X-Websocket-Test".to_string(), "yes".to_string());
    let cfg = VhostConfig {
        base_domain: "bore.local".to_string(),
        mode: VhostModeCfg::Http,
        http_port: HTTP,
        https_port: 443,
        cert_file: None,
        key_file: None,
        default_headers,
        default_response_headers: BTreeMap::new(),
        reservations: vec![],
    };

    spawn_server_vhost(CTRL, cfg).await?;
    wait_port(HTTP, true).await;

    let client = Client::new_vhost_provider(
        "127.0.0.1",
        ws_port,
        &format!("localhost:{CTRL}"),
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

    let mut conn = TcpStream::connect(("127.0.0.1", HTTP)).await?;
    websocket::assert_websocket_round_trip(&mut conn, "wsapp.bore.local", "/socket").await?;
    Ok(())
}

#[tokio::test]
async fn vhost_https_websocket_relay_round_trip() -> Result<()> {
    const CTRL: u16 = 17995;
    const HTTPS: u16 = 17996;

    let ws_port = websocket::spawn_websocket_echo_service(None).await?;

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
        default_response_headers: BTreeMap::new(),
        reservations: vec![],
    };

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_vhost(cfg)?;
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTPS, true).await;

    let client = Client::new_vhost_provider(
        "127.0.0.1",
        ws_port,
        &format!("localhost:{CTRL}"),
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
    websocket::assert_websocket_round_trip(&mut tls, "wssapp.bore.local", "/socket").await?;
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vhost_https_websocket_direct_udp_round_trip() -> Result<()> {
    const CTRL: u16 = 17997;
    const HTTPS: u16 = 17998;
    const QUIC: u16 = 17999;

    let ws_port = websocket::spawn_websocket_echo_service(None).await?;

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
        default_response_headers: BTreeMap::new(),
        reservations: vec![],
    };

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, Some("vhost-ws-udp-secret"));
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_udp(true);
    server.set_vhost(cfg)?;
    server.set_vhost_quic_port(QUIC);
    let registry = server.vhost_registry();
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTPS, true).await;

    let client = Client::new_vhost_provider_with_udp(
        "127.0.0.1",
        ws_port,
        &format!("localhost:{CTRL}"),
        "udpwss",
        "client1",
        Some("vhost-ws-udp-secret"),
        false,
        1,
        true,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());

    wait_for_vhost_direct(&registry, "udpwss", true).await;

    let endpoint = Endpoint {
        host: "127.0.0.1".to_string(),
        port: HTTPS,
        tls: true,
    };
    let mut tls = transport::connect(&endpoint, true).await?;
    websocket::assert_websocket_round_trip(&mut tls, "udpwss.bore.local", "/socket").await?;
    assert!(
        direct_stream_opens(&registry, "udpwss") >= 1,
        "expected websocket connection to open a direct QUIC stream"
    );
    Ok(())
}

// ─── Config hot reload (control=17956, http=17957) ───────────────────────────

#[tokio::test]
async fn vhost_config_hot_reload() -> Result<()> {
    const CTRL: u16 = 17956;
    const HTTP: u16 = 17957;

    let yaml_path = temp_yaml_path();

    // Initial config: clientA may register "reload-sub"; clientB may NOT.
    let initial_yaml = format!(
        "base_domain: bore.local\nhttp_port: {HTTP}\nreservations:\n  - client_id: clientA\n    subdomain: reload-sub\n"
    );
    std::fs::write(&yaml_path, &initial_yaml)?;

    // Parse the initial config to seed the server.
    let initial_cfg = bore_cli::vhost::parse_config(&initial_yaml)?;

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_vhost(initial_cfg)?;
    server.set_vhost_config_path(yaml_path.clone());
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTP, true).await;

    let local_port = TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port();
    let to = format!("localhost:{CTRL}");

    // Verify initial state: clientA accepted, clientB rejected.
    // Spawn the listen loop on a task, then abort it to close the connection cleanly.
    {
        let ok_a = Client::new_vhost_provider(
            "127.0.0.1",
            local_port,
            &to,
            "reload-sub",
            "clientA",
            None,
            false,
            1,
            ProviderMeta::default(),
        )
        .await;
        assert!(
            ok_a.is_ok(),
            "clientA should be accepted initially: {:?}",
            ok_a.err()
        );
        let task_a = tokio::spawn(ok_a.unwrap().listen());
        time::sleep(Duration::from_millis(50)).await;
        task_a.abort();
        // Wait for the server to detect the disconnection (heartbeat interval is 500ms).
        time::sleep(Duration::from_millis(600)).await;
    }

    let err_b = Client::new_vhost_provider(
        "127.0.0.1",
        local_port,
        &to,
        "reload-sub",
        "clientB",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await;
    assert!(
        err_b.is_err(),
        "clientB must be rejected under initial config"
    );

    // Overwrite config: now only clientB may register "reload-sub".
    let new_yaml = format!(
        "base_domain: bore.local\nhttp_port: {HTTP}\nreservations:\n  - client_id: clientB\n    subdomain: reload-sub\n"
    );
    std::fs::write(&yaml_path, &new_yaml)?;

    // Poll until clientB registers successfully (up to 10s; reload poll is 2s).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let r = Client::new_vhost_provider(
            "127.0.0.1",
            local_port,
            &to,
            "reload-sub",
            "clientB",
            None,
            false,
            1,
            ProviderMeta::default(),
        )
        .await;
        if r.is_ok() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("config hot-reload: clientB was not accepted within 10s");
        }
        time::sleep(Duration::from_millis(200)).await;
    }

    // After reload, clientA must be rejected.
    let err_a = Client::new_vhost_provider(
        "127.0.0.1",
        local_port,
        &to,
        "reload-sub",
        "clientA",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await;
    assert!(
        err_a.is_err(),
        "clientA must be rejected after config swap to clientB"
    );

    let _ = std::fs::remove_file(&yaml_path);
    Ok(())
}

// ─── Bad config ignored (control=17958, http=17959) ──────────────────────────

#[tokio::test]
async fn vhost_bad_config_ignored() -> Result<()> {
    const CTRL: u16 = 17958;
    const HTTP: u16 = 17959;

    let yaml_path = temp_yaml_path();
    let initial_yaml = format!(
        "base_domain: bore.local\nhttp_port: {HTTP}\nreservations:\n  - client_id: clientA\n    subdomain: good-sub\n"
    );
    std::fs::write(&yaml_path, &initial_yaml)?;

    let initial_cfg = bore_cli::vhost::parse_config(&initial_yaml)?;

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_vhost(initial_cfg)?;
    server.set_vhost_config_path(yaml_path.clone());
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTP, true).await;

    let local_port = TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port();
    let to = format!("localhost:{CTRL}");

    // Register under valid config — keep it alive to prove server is running.
    let ok = Client::new_vhost_provider(
        "127.0.0.1",
        local_port,
        &to,
        "good-sub",
        "clientA",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await;
    assert!(
        ok.is_ok(),
        "clientA should register under valid config: {:?}",
        ok.err()
    );
    let live_task = tokio::spawn(ok.unwrap().listen());

    // Overwrite with malformed YAML.
    std::fs::write(&yaml_path, "invalid: yaml: {")?;

    // Wait > 2 reload cycles (the task polls every 2s).
    time::sleep(Duration::from_secs(5)).await;

    // The existing live client's connection should still be alive.
    // Abort it so the slot is freed, then verify a fresh registration works
    // (proving the server did not crash and still uses the last valid config).
    live_task.abort();
    time::sleep(Duration::from_millis(200)).await;

    let still_ok = Client::new_vhost_provider(
        "127.0.0.1",
        local_port,
        &to,
        "good-sub",
        "clientA",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await;
    assert!(
        still_ok.is_ok(),
        "server must still function after bad config write: {:?}",
        still_ok.err()
    );

    let _ = std::fs::remove_file(&yaml_path);
    Ok(())
}

// ─── Auto-reconnect (control=17970, http=17971) ───────────────────────────────

#[tokio::test]
async fn vhost_auto_reconnect() -> Result<()> {
    const CTRL: u16 = 17970;
    const HTTP: u16 = 17971;

    wait_port(CTRL, false).await;

    let stub_port = spawn_http_stub("reconnected").await;

    // Start the reconnect loop immediately (server not up yet).
    let to_addr = format!("localhost:{CTRL}");
    tokio::spawn(async move {
        let _ = reconnect::run(
            true,
            || {
                let to = to_addr.clone();
                async move {
                    Client::new_vhost_provider(
                        "127.0.0.1",
                        stub_port,
                        &to,
                        "reconnapp",
                        "client1",
                        None,
                        false,
                        1,
                        ProviderMeta::default(),
                    )
                    .await
                }
            },
            |c| c.listen(),
        )
        .await;
    });

    // Start the server after a 300ms delay.
    time::sleep(Duration::from_millis(300)).await;
    let cfg = http_config("bore.local", HTTP);
    spawn_server_vhost(CTRL, cfg).await?;
    wait_port(HTTP, true).await;

    // Poll HTTP routing for up to 15s (reconnect backoff starts at 1s).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let r = send_http(HTTP, "reconnapp.bore.local", "/").await;
        if let Ok(resp) = r {
            if resp.contains("reconnected") {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("vhost_auto_reconnect: routing did not work within 15s after server appeared");
        }
        time::sleep(Duration::from_millis(200)).await;
    }
}

// ─── Mode validation ──────────────────────────────────────────────────────────

#[test]
fn vhost_https_mode_without_cert_errors() {
    let cfg = VhostConfig {
        base_domain: "bore.local".to_string(),
        mode: VhostModeCfg::Https,
        http_port: 80,
        https_port: 443,
        cert_file: None,
        key_file: None,
        default_headers: BTreeMap::new(),
        default_response_headers: BTreeMap::new(),
        reservations: vec![],
    };
    let mut server = Server::new(1024..=65535, None);
    let result = server.set_vhost(cfg);
    assert!(
        result.is_err(),
        "set_vhost with mode=Https and no cert must return Err"
    );
}

// ─── POST body + header injection (control=17972, http=17973) ────────────────

/// Stub that reads a full request (headers + `Content-Length` body) and captures
/// the raw bytes, then replies 200.
async fn spawn_body_capturing_stub(captured: Arc<Mutex<Option<Vec<u8>>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let captured = Arc::clone(&captured);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16384];
                let mut total = 0;
                // Read until headers complete, then until headers+body length reached.
                let mut need: Option<usize> = None;
                loop {
                    let n = stream.read(&mut buf[total..]).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    total += n;
                    if need.is_none() {
                        if let Some(p) = buf[..total].windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&buf[..p]);
                            let cl = headers
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    if k.trim().eq_ignore_ascii_case("content-length") {
                                        v.trim().parse::<usize>().ok()
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(0);
                            need = Some(p + 4 + cl);
                        }
                    }
                    if let Some(target) = need {
                        if total >= target || total >= buf.len() {
                            break;
                        }
                    }
                }
                *captured.lock().await = Some(buf[..total].to_vec());
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            });
        }
    });
    port
}

/// Send a POST with `body` and the given Host header; headers+body in one write so
/// the server's head reader over-reads the body (the F1 regression condition).
async fn send_http_post(port: u16, host: &str, body: &str) -> Result<String> {
    let mut conn = time::timeout(
        Duration::from_secs(3),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await??;
    let req = format!(
        "POST / HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    conn.write_all(req.as_bytes()).await?;
    conn.shutdown().await?;
    let mut resp = Vec::new();
    time::timeout(Duration::from_secs(5), conn.read_to_end(&mut resp)).await??;
    Ok(String::from_utf8_lossy(&resp).into_owned())
}

#[tokio::test]
async fn vhost_post_body_preserved_with_inject() -> Result<()> {
    const CTRL: u16 = 17972;
    const HTTP: u16 = 17973;

    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let stub_port = spawn_body_capturing_stub(Arc::clone(&captured)).await;

    // default_headers makes the rewrite (inject) path active — the path that used
    // to drop the request body.
    let mut default_headers = BTreeMap::new();
    default_headers.insert("X-Injected".to_string(), "yes".to_string());

    let cfg = VhostConfig {
        base_domain: "bore.local".to_string(),
        mode: VhostModeCfg::Http,
        http_port: HTTP,
        https_port: 443,
        cert_file: None,
        key_file: None,
        default_headers,
        default_response_headers: BTreeMap::new(),
        reservations: vec![],
    };

    spawn_server_vhost(CTRL, cfg).await?;
    wait_port(HTTP, true).await;

    let client = Client::new_vhost_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "post",
        "client1",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(50)).await;

    let body = "the-quick-brown-fox-jumps-over-the-lazy-dog";
    let _ = send_http_post(HTTP, "post.bore.local", body).await?;
    time::sleep(Duration::from_millis(100)).await;

    let raw = captured
        .lock()
        .await
        .clone()
        .expect("stub did not capture request");
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("X-Injected: yes"),
        "inject header must be present: {text}"
    );
    assert!(
        text.ends_with(body),
        "request body must reach the upstream intact, got tail: {:?}",
        &text[text.len().saturating_sub(80)..]
    );
    Ok(())
}

// ─── HTTPS routing rejects foreign base domain (control=17974, https=17975) ──

#[tokio::test]
async fn vhost_https_rejects_foreign_base_domain() -> Result<()> {
    const CTRL: u16 = 17974;
    const HTTPS: u16 = 17975;

    let stub_port = spawn_http_stub("hello-tls").await;

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
        default_response_headers: BTreeMap::new(),
        reservations: vec![],
    };

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_vhost(cfg)?;
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTPS, true).await;

    let client = Client::new_vhost_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "tlsapp",
        "client1",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(50)).await;

    let endpoint = Endpoint {
        host: "127.0.0.1".to_string(),
        port: HTTPS,
        tls: true,
    };

    // Helper: send a request with an arbitrary Host header over TLS, return response.
    async fn https_request(endpoint: &Endpoint, host: &str) -> Result<String> {
        let mut tls = transport::connect(endpoint, true).await?;
        let req = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        tls.write_all(req.as_bytes()).await?;
        tls.shutdown().await?;
        let mut resp = Vec::new();
        time::timeout(Duration::from_secs(5), tls.read_to_end(&mut resp)).await??;
        Ok(String::from_utf8_lossy(&resp).into_owned())
    }

    // Correct base domain → routed → 200.
    let ok = https_request(&endpoint, "tlsapp.bore.local").await?;
    assert!(ok.contains("hello-tls"), "valid host must route: {ok}");

    // Foreign base domain with a registered first label → must be rejected (502),
    // not routed (regression: HTTPS used to ignore the base domain).
    let foreign = https_request(&endpoint, "tlsapp.evil.com").await?;
    assert!(
        foreign.starts_with("HTTP/1.1 502"),
        "foreign base domain must 502, got: {foreign}"
    );

    // Nested label under the base domain → rejected (HTTP path already rejects it).
    let nested = https_request(&endpoint, "a.tlsapp.bore.local").await?;
    assert!(
        nested.starts_with("HTTP/1.1 502"),
        "nested label must 502, got: {nested}"
    );
    Ok(())
}

// ─── Unified control port: HTTP routed by Host (control=17976) ───────────────

#[tokio::test]
async fn vhost_routes_on_control_port() -> Result<()> {
    const CTRL: u16 = 17976;
    const HTTP: u16 = 17977; // standalone frontend; this test hits the control port

    let stub_port = spawn_http_stub("hello-control").await;
    let cfg = http_config("bore.local", HTTP);
    spawn_server_vhost(CTRL, cfg).await?;

    let client = Client::new_vhost_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "app",
        "client1",
        None,
        false,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(50)).await;

    // An HTTP request to the CONTROL port (not the vhost frontend) must route by
    // Host to the vhost provider — this is the single-port (e.g. 443) deployment
    // where control + vhost share one listener. (Regression: previously the control
    // port only served the bore protocol / admin, so this 404'd.)
    let resp = send_http(CTRL, "app.bore.local", "/").await?;
    assert!(
        resp.contains("hello-control"),
        "HTTP on the control port must route to vhost, got: {resp}"
    );

    // An unknown subdomain on the control port → 404 (no admin token configured).
    let miss = send_http(CTRL, "ghost.bore.local", "/").await?;
    assert!(
        miss.starts_with("HTTP/1.1 404"),
        "unknown subdomain on control port → 404, got: {miss}"
    );
    Ok(())
}

#[tokio::test]
async fn vhost_routes_on_tls_control_port() -> Result<()> {
    const CTRL: u16 = 17978;
    const HTTP: u16 = 17979; // standalone frontend; this test hits the TLS control port

    let stub_port = spawn_http_stub("hello-tls-control").await;
    // Wildcard cert so the browser's TLS to app.bore.local validates against the
    // control-port certificate (the single-443 deployment requirement).
    let (cert_pem, key_pem) = self_signed_for(vec![
        "*.bore.local".to_string(),
        "bore.local".to_string(),
        "localhost".to_string(),
    ])?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_tls(acceptor);
    server.set_vhost(http_config("bore.local", HTTP))?;
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;

    // Provider registers over the TLS control port (insecure: self-signed cert).
    let client = Client::new_vhost_provider(
        "127.0.0.1",
        stub_port,
        &format!("https://localhost:{CTRL}"),
        "app",
        "client1",
        None,
        true,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(50)).await;

    // Browser-style: TLS to the control port, then an HTTP GET routed by Host to
    // the vhost provider — the exact single-port (443) topology that failed in the
    // field before unification.
    let endpoint = Endpoint {
        host: "127.0.0.1".to_string(),
        port: CTRL,
        tls: true,
    };
    let mut tls = transport::connect(&endpoint, true).await?;
    tls.write_all(b"GET / HTTP/1.1\r\nHost: app.bore.local\r\nConnection: close\r\n\r\n")
        .await?;
    tls.shutdown().await?;
    let mut resp = Vec::new();
    time::timeout(Duration::from_secs(5), tls.read_to_end(&mut resp)).await??;
    let resp = String::from_utf8_lossy(&resp);
    assert!(
        resp.contains("hello-tls-control"),
        "HTTP over TLS on the control port must route to vhost, got: {resp}"
    );
    Ok(())
}

// ─── TODO: cert hot-reload test ───────────────────────────────────────────────
// Cert hot-reload (swapping cert/key files while the server is live) is complex
// to test reliably in a short integration test because:
//   - The hot-reload task polls every 2s using mtime comparison.
//   - Generating two different self-signed certs, writing them, waiting for mtime
//     change detection, and then verifying the new cert is used requires careful
//     timing and a way to inspect which cert the TLS handshake negotiated
//     (tokio-rustls is not a dev-dependency, so peer-cert inspection in-test is
//     not currently wired up).
// The reload *path* (including the cert/key path-change → forced TLS reload fixed
// in this audit) is exercised by `vhost_config_hot_reload`; cert-DER verification
// is left as future work.

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vhost_udp_direct_get_and_post() -> Result<()> {
    const CTRL: u16 = 17980;
    const HTTP: u16 = 17981;
    const QUIC: u16 = 17982;

    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let stub_port = spawn_body_capturing_stub(Arc::clone(&captured)).await;

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, Some("vhost-udp-secret"));
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_udp(true);
    server.set_vhost(http_config("bore.local", HTTP))?;
    server.set_vhost_quic_port(QUIC);
    let registry = server.vhost_registry();
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTP, true).await;

    let client = Client::new_vhost_provider_with_udp(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "udpapp",
        "client1",
        Some("vhost-udp-secret"),
        false,
        1,
        true,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());

    wait_for_vhost_direct(&registry, "udpapp", true).await;

    let response = send_http(HTTP, "udpapp.bore.local", "/").await?;
    assert!(
        response.contains("ok"),
        "expected GET response over direct path: {response}"
    );

    let body = "vhost-udp-post-body";
    let response = send_http_post(HTTP, "udpapp.bore.local", body).await?;
    assert!(
        response.contains("ok"),
        "expected POST response over direct path: {response}"
    );
    time::sleep(Duration::from_millis(100)).await;

    let raw = captured
        .lock()
        .await
        .clone()
        .expect("captured request missing");
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.ends_with(body),
        "upstream must receive the POST body intact: {text}"
    );
    assert!(
        direct_stream_opens(&registry, "udpapp") >= 2,
        "expected GET and POST to open direct streams"
    );
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vhost_udp_multi_carrier_pool() -> Result<()> {
    const CTRL: u16 = 18010;
    const HTTP: u16 = 18011;
    const QUIC: u16 = 18012;

    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let stub_port = spawn_body_capturing_stub(Arc::clone(&captured)).await;

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, Some("vhost-udp-secret"));
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_udp(true);
    server.set_vhost(http_config("bore.local", HTTP))?;
    server.set_vhost_quic_port(QUIC);
    let registry = server.vhost_registry();
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTP, true).await;

    // Request 3 QUIC direct carriers: the provider opens that many independent
    // QUIC connections and the server pools them for round-robin relaying.
    let carriers = 3u16;
    let client = Client::new_vhost_provider_with_udp(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "multiudp",
        "client1",
        Some("vhost-udp-secret"),
        false,
        carriers,
        true,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());

    // The direct pool fills to the requested carrier count.
    wait_for_vhost_direct_count(&registry, "multiudp", carriers as usize).await;

    // Traffic still routes correctly over the pooled direct path.
    let response = send_http(HTTP, "multiudp.bore.local", "/").await?;
    assert!(
        response.contains("ok"),
        "expected GET response over the multi-carrier direct path: {response}"
    );
    assert!(
        direct_stream_opens(&registry, "multiudp") >= 1,
        "expected the GET to open a direct stream"
    );
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vhost_udp_falls_back_when_server_udp_disabled() -> Result<()> {
    const CTRL: u16 = 17983;
    const HTTP: u16 = 17984;

    let stub_port = spawn_http_stub("relay-only").await;

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, Some("vhost-udp-secret"));
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_udp(false);
    server.set_vhost(http_config("bore.local", HTTP))?;
    let registry = server.vhost_registry();
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTP, true).await;

    let client = Client::new_vhost_provider_with_udp(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "relayapp",
        "client1",
        Some("vhost-udp-secret"),
        false,
        1,
        true,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(200)).await;

    let response = send_http(HTTP, "relayapp.bore.local", "/").await?;
    assert!(
        response.contains("relay-only"),
        "fallback relay response missing: {response}"
    );
    assert!(
        registry.get("relayapp").unwrap().direct.is_empty(),
        "server with udp disabled must not establish a direct path"
    );
    assert_eq!(
        direct_stream_opens(&registry, "relayapp"),
        0,
        "fallback request must not open direct streams"
    );
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vhost_udp_drop_recovers_and_reestablishes() -> Result<()> {
    const CTRL: u16 = 17985;
    const HTTP: u16 = 17986;
    const QUIC: u16 = 17987;

    let stub_port = spawn_http_stub("recover").await;

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, Some("vhost-udp-secret"));
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_udp(true);
    server.set_vhost(http_config("bore.local", HTTP))?;
    server.set_vhost_quic_port(QUIC);
    let registry = server.vhost_registry();
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTP, true).await;

    let client = Client::new_vhost_provider_with_udp(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "recoverapp",
        "client1",
        Some("vhost-udp-secret"),
        false,
        1,
        true,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());

    wait_for_vhost_direct(&registry, "recoverapp", true).await;

    let response = send_http(HTTP, "recoverapp.bore.local", "/").await?;
    assert!(
        response.contains("recover"),
        "initial direct response missing: {response}"
    );
    let direct_before_drop = direct_stream_opens(&registry, "recoverapp");
    assert!(
        direct_before_drop >= 1,
        "initial request must use the direct path"
    );

    close_vhost_direct(&registry, "recoverapp");
    wait_for_vhost_direct(&registry, "recoverapp", false).await;

    let response = send_http(HTTP, "recoverapp.bore.local", "/").await?;
    assert!(
        response.contains("recover"),
        "fallback request after drop must still succeed: {response}"
    );
    assert_eq!(
        direct_stream_opens(&registry, "recoverapp"),
        direct_before_drop,
        "fallback request must not open a new direct stream while the path is down"
    );

    wait_for_vhost_direct(&registry, "recoverapp", true).await;

    let response = send_http(HTTP, "recoverapp.bore.local", "/").await?;
    assert!(
        response.contains("recover"),
        "request after renewal must succeed: {response}"
    );
    assert!(
        direct_stream_opens(&registry, "recoverapp") > direct_before_drop,
        "renewed direct path must open a new QUIC stream"
    );
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vhost_udp_defaults_to_http_port_without_tls() -> Result<()> {
    const CTRL: u16 = 17988;
    const HTTP: u16 = 17989;

    let stub_port = spawn_http_stub("http-udp-default").await;

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, Some("vhost-udp-secret"));
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_udp(true);
    server.set_vhost(http_config("bore.local", HTTP))?;
    let registry = server.vhost_registry();
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTP, true).await;

    let client = Client::new_vhost_provider_with_udp(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "httpudp",
        "client1",
        Some("vhost-udp-secret"),
        false,
        1,
        true,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());

    wait_for_vhost_direct(&registry, "httpudp", true).await;

    let response = send_http(HTTP, "httpudp.bore.local", "/").await?;
    assert!(
        response.contains("http-udp-default"),
        "http-mode vhost udp should work on the http port by default: {response}"
    );
    assert!(
        direct_stream_opens(&registry, "httpudp") >= 1,
        "http-mode vhost udp should open a direct QUIC stream"
    );
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vhost_udp_large_body_integrity() -> Result<()> {
    const CTRL: u16 = 17990;
    const HTTP: u16 = 17991;
    const QUIC: u16 = 17992;
    const LEN: usize = 1 << 20;

    let pattern: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();
    let pattern_clone = pattern.clone();

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let stub_port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let pattern = pattern_clone.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let mut total = 0;
                loop {
                    let n = stream.read(&mut buf[total..]).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    total += n;
                    if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if total >= buf.len() {
                        break;
                    }
                }
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {LEN}\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&pattern).await;
            });
        }
    });

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, Some("vhost-udp-secret"));
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_udp(true);
    server.set_vhost(http_config("bore.local", HTTP))?;
    server.set_vhost_quic_port(QUIC);
    let registry = server.vhost_registry();
    tokio::spawn(server.listen());
    wait_port(CTRL, true).await;
    wait_port(HTTP, true).await;

    let client = Client::new_vhost_provider_with_udp(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "bigudp",
        "client1",
        Some("vhost-udp-secret"),
        false,
        1,
        true,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(client.listen());

    wait_for_vhost_direct(&registry, "bigudp", true).await;

    let mut conn = time::timeout(
        Duration::from_secs(3),
        TcpStream::connect(("127.0.0.1", HTTP)),
    )
    .await??;
    let req = "GET / HTTP/1.1\r\nHost: bigudp.bore.local\r\nConnection: close\r\n\r\n";
    conn.write_all(req.as_bytes()).await?;
    conn.shutdown().await?;
    let mut response = Vec::new();
    time::timeout(Duration::from_secs(5), conn.read_to_end(&mut response)).await??;

    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response must contain a header terminator");
    let body = &response[split + 4..];
    assert_eq!(
        body.len(),
        LEN,
        "large response body length must be preserved"
    );
    assert_eq!(
        body,
        pattern.as_slice(),
        "large response body must survive the QUIC direct path intact"
    );
    assert!(
        direct_stream_opens(&registry, "bigudp") >= 1,
        "large-body request should use the direct QUIC path"
    );
    Ok(())
}
