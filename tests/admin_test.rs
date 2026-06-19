//! End-to-end tests for the admin status page served on the control port.

use std::time::Duration;

use anyhow::Result;
use bore_cli::{
    client::{Client, ProviderMeta},
    secret::Proxy,
    server::Server,
    shared::CONTROL_PORT,
    transport::{self, Endpoint},
    vhost::{VhostConfig, VhostModeCfg},
};
use lazy_static::lazy_static;
use rcgen::generate_simple_self_signed;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

// A realistic >=32-char admin token.
const TOKEN: &str = "0123456789abcdef0123456789abcdef01234567";

async fn wait_port(port: u16, listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("localhost", port)).await.is_ok() == listening {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

async fn spawn_echo_service() -> Result<u16> {
    let listener = TcpListener::bind("localhost:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await?;
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                loop {
                    let n = stream.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    stream.write_all(&buf[..n]).await?;
                }
                anyhow::Ok(())
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    });
    Ok(port)
}

fn self_signed() -> Result<(String, String)> {
    let key = generate_simple_self_signed(["localhost".to_string()])?;
    Ok((key.cert.pem(), key.signing_key.serialize_pem()))
}

/// Issue one HTTP/1.1 GET over `stream` and return the full response text.
async fn http_get<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    path: &str,
    token: Option<&str>,
) -> Result<String> {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;
    let mut buf = Vec::new();
    time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await??;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_disabled_does_not_serve_http() -> Result<()> {
    // With no --admin-token the control port speaks only the bore protocol: an
    // HTTP request is not served (the connection is dropped), preserving behaviour.
    let _g = SERIAL_GUARD.lock().await;
    wait_port(CONTROL_PORT, false).await;
    tokio::spawn(Server::new(1024..=65535, None).listen());
    wait_port(CONTROL_PORT, true).await;

    let mut s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    s.write_all(b"GET /admin/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await?;
    let mut buf = vec![0u8; 128];
    let n = time::timeout(Duration::from_secs(2), s.read(&mut buf))
        .await
        .unwrap_or(Ok(0))
        .unwrap_or(0);
    let resp = String::from_utf8_lossy(&buf[..n]);
    assert!(
        !resp.contains("HTTP/1.1 2") && !resp.contains("<html"),
        "admin must not serve HTTP when disabled, got: {resp:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_enabled_serves_page_and_guards_data() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    wait_port(CONTROL_PORT, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_admin_token(Some(TOKEN.into()));
    tokio::spawn(server.listen());
    wait_port(CONTROL_PORT, true).await;

    // The SPA shell is served without a token (D7).
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let page = http_get(s, "/admin/status", None).await?;
    assert!(page.starts_with("HTTP/1.1 200"), "page status: {page:.40}");
    assert!(page.contains("text/html"), "page content-type");
    assert!(page.contains("<!DOCTYPE"), "page should be HTML");
    assert!(
        page.contains("Content-Security-Policy:"),
        "page must have CSP header"
    );

    // The data endpoint requires the token.
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let unauth = http_get(s, "/admin/status/data", None).await?;
    assert!(
        unauth.starts_with("HTTP/1.1 401"),
        "data without token must be 401: {unauth:.40}"
    );

    // A wrong token is also rejected.
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let wrong = http_get(s, "/admin/status/data", Some("wrong-token")).await?;
    assert!(wrong.starts_with("HTTP/1.1 401"), "wrong token must be 401");

    // The correct token returns the JSON snapshot.
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let data = http_get(s, "/admin/status/data", Some(TOKEN)).await?;
    assert!(data.starts_with("HTTP/1.1 200"), "data status: {data:.40}");
    assert!(data.contains("application/json"), "data content-type");
    assert!(data.contains("\"server\""));
    assert!(data.contains("\"tunnels\""));

    // An unknown path is a 404.
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let nf = http_get(s, "/nope", Some(TOKEN)).await?;
    assert!(nf.starts_with("HTTP/1.1 404"), "unknown path must be 404");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_data_reflects_live_tunnels() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    wait_port(CONTROL_PORT, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_admin_token(Some(TOKEN.into()));
    tokio::spawn(server.listen());
    wait_port(CONTROL_PORT, true).await;

    let echo = spawn_echo_service().await?;
    let provider = Client::new_secret_provider(
        "localhost",
        echo,
        "localhost",
        "livesvc",
        None,
        false,
        false,
        None,
        false,
        false,
        0,
        0, // release timeout
        1024,
        1, // carriers
        ProviderMeta {
            notes: Some("hello-note".into()),
            basic_auth: None,
        },
    )
    .await?;
    tokio::spawn(provider.listen());

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "livesvc",
        None,
        false,
        false,
        None,
        false,
        false,
        0,
        0, // release timeout
        1, // carriers
        Some("consumer-note".into()),
    )
    .await?;
    let consumer = tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(200)).await;

    // Both the provider and the consumer (and the notes) show up.
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let data = http_get(s, "/admin/status/data", Some(TOKEN)).await?;
    assert!(data.contains("livesvc"), "secret id must appear: {data}");
    assert!(data.contains("hello-note"), "provider note must appear");
    assert!(data.contains("consumer-note"), "consumer note must appear");
    assert!(
        data.contains("secret-provider"),
        "provider role must appear"
    );
    assert!(
        data.contains("secret-consumer"),
        "consumer role must appear"
    );

    // After the consumer disconnects, it disappears from the snapshot.
    consumer.abort();
    time::sleep(Duration::from_millis(400)).await;
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let data2 = http_get(s, "/admin/status/data", Some(TOKEN)).await?;
    assert!(
        !data2.contains("secret-consumer"),
        "consumer entry must be gone after disconnect: {data2}"
    );
    assert!(
        data2.contains("secret-provider"),
        "provider must still be present"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_served_over_tls() -> Result<()> {
    const PORT: u16 = 17960;
    let _g = SERIAL_GUARD.lock().await;
    wait_port(PORT, false).await;

    let (cert, key) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert.as_bytes(), key.as_bytes())?;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(PORT);
    server.set_tls(acceptor);
    server.set_admin_token(Some(TOKEN.into()));
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    // The page must be served over the same TLS the control port uses (https).
    let ep = Endpoint {
        host: "localhost".to_string(),
        port: PORT,
        tls: true,
    };
    let stream = transport::connect(&ep, true).await?; // insecure: self-signed
    let page = http_get(stream, "/admin/status", None).await?;
    assert!(page.starts_with("HTTP/1.1 200"), "tls page: {page:.40}");
    assert!(
        page.contains("<!DOCTYPE"),
        "tls page should be the HTML shell"
    );
    assert!(
        page.contains("Strict-Transport-Security: max-age=31536000; includeSubDomains"),
        "tls page must include HSTS"
    );

    // And the guarded data endpoint over TLS too.
    let stream = transport::connect(&ep, true).await?;
    let data = http_get(stream, "/admin/status/data", Some(TOKEN)).await?;
    assert!(data.starts_with("HTTP/1.1 200"), "tls data: {data:.40}");
    assert!(data.contains("\"server\""));
    assert!(
        data.contains("Strict-Transport-Security: max-age=31536000; includeSubDomains"),
        "tls data must include HSTS"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_disabled_over_tls_returns_404_with_hsts() -> Result<()> {
    const PORT: u16 = 17961;
    const VHOST_HTTP: u16 = 17962;
    let _g = SERIAL_GUARD.lock().await;
    wait_port(PORT, false).await;

    let (cert, key) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert.as_bytes(), key.as_bytes())?;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(PORT);
    server.set_tls(acceptor);
    server.set_vhost(VhostConfig {
        base_domain: "bore.local".to_string(),
        mode: VhostModeCfg::Http,
        http_port: VHOST_HTTP,
        https_port: 443,
        cert_file: None,
        key_file: None,
        default_headers: Default::default(),
        default_response_headers: Default::default(),
        reservations: vec![],
    })?;
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let ep = Endpoint {
        host: "localhost".to_string(),
        port: PORT,
        tls: true,
    };
    let stream = transport::connect(&ep, true).await?;
    let resp = http_get(stream, "/", None).await?;
    assert!(resp.starts_with("HTTP/1.1 404"), "tls 404: {resp:.60}");
    assert!(
        resp.contains("Strict-Transport-Security: max-age=31536000; includeSubDomains"),
        "tls 404 must include HSTS"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_tls_hsts_can_be_disabled() -> Result<()> {
    const PORT: u16 = 17963;
    let _g = SERIAL_GUARD.lock().await;
    wait_port(PORT, false).await;

    let (cert, key) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert.as_bytes(), key.as_bytes())?;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(PORT);
    server.set_tls(acceptor);
    server.set_admin_token(Some(TOKEN.into()));
    server.set_control_hsts("off");
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let ep = Endpoint {
        host: "localhost".to_string(),
        port: PORT,
        tls: true,
    };
    let stream = transport::connect(&ep, true).await?;
    let page = http_get(stream, "/admin/status", None).await?;
    assert!(page.starts_with("HTTP/1.1 200"), "tls page: {page:.40}");
    assert!(
        !page.contains("Strict-Transport-Security:"),
        "HSTS must be absent when disabled"
    );

    Ok(())
}

// ─── Phase 0 tests (§0.1 – §0.4) ───────────────────────────────────

#[test]
fn t_views_serialize_stable() {
    use bore_cli::admin_views::*;

    // Test that ConfigView serializes with expected snake_case keys (T-SANITIZE gate).
    let config = ConfigView {
        port_range: "5000-6000".into(),
        control_port: 7835,
        max_conns: 100,
        max_carriers: 4,
        bind_addr: "0.0.0.0".into(),
        bind_tunnels: "0.0.0.0".into(),
        udp: false,
        udp_socket_send_buffer: None,
        udp_socket_recv_buffer: None,
        udp_stream_receive_window: "16MiB".into(),
        udp_connection_receive_window: "16MiB".into(),
        udp_send_window: "64MiB".into(),
        udp_max_streams: 4096,
        bind_domain: None,
        control_hsts: "max-age=31536000".into(),
        #[cfg(feature = "vpn")]
        vpn_enabled: false,
        #[cfg(feature = "vpn")]
        vpn_pool: None,
        #[cfg(feature = "vpn")]
        vpn_max_links: 32,
        #[cfg(feature = "vpn")]
        vpn_hub_prefix: 24,
        #[cfg(feature = "vpn")]
        vpn_punch_timeout: Some(10),
        vhost_enabled: false,
        vhost_base_domain: None,
        vhost_http_port: None,
        vhost_https_port: None,
        vhost_quic_port: None,
        vhost_mode: None,
        tls: false,
    };

    let json = serde_json::to_value(&config).expect("serialize ConfigView");
    assert!(json["port_range"].is_string(), "port_range must be string");
    assert!(
        json["control_port"].is_number(),
        "control_port must be number"
    );
    assert!(json["max_conns"].is_number());

    // (T-SANITIZE) Forbidden keys must NOT appear.
    assert!(
        json.get("admin_token").is_none(),
        "admin_token must be sanitized"
    );
    assert!(json.get("secret").is_none(), "secret must be sanitized");
    assert!(json.get("key").is_none(), "key must be sanitized");
    assert!(json.get("password").is_none(), "password must be sanitized");
    // tls must be a bool, not a path.
    assert!(json["tls"].is_boolean(), "tls must be boolean");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_api_requires_token() -> Result<()> {
    // (T-AUTH) Each /admin/api/v1/* returns 401 without token, 200 with valid Bearer
    // and 200 with valid X-Admin-Token.
    const PORT: u16 = 17970;
    let _g = SERIAL_GUARD.lock().await;
    wait_port(PORT, false).await;

    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(PORT);
    server.set_admin_token(Some(TOKEN.into()));
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    // Test each endpoint without token → 401.
    for path in &["/admin/api/v1/summary", "/admin/api/v1/tunnels"] {
        let s = TcpStream::connect(("127.0.0.1", PORT)).await?;
        let resp = http_get(s, path, None).await?;
        assert!(
            resp.starts_with("HTTP/1.1 401"),
            "endpoint {} must return 401 without token: {}",
            path,
            &resp[..40.min(resp.len())]
        );
    }

    // With Bearer token → 200.
    for path in &["/admin/api/v1/summary", "/admin/api/v1/tunnels"] {
        let s = TcpStream::connect(("127.0.0.1", PORT)).await?;
        let resp = http_get(s, path, Some(TOKEN)).await?;
        assert!(
            resp.starts_with("HTTP/1.1 200"),
            "endpoint {} must return 200 with Bearer token: {}",
            path,
            &resp[..100.min(resp.len())]
        );
        assert!(
            resp.contains("{") || resp.contains("["),
            "response must be JSON: {}",
            &resp[..200.min(resp.len())]
        );
    }

    // With X-Admin-Token header (using a custom request).
    let s = TcpStream::connect(("127.0.0.1", PORT)).await?;
    let req = format!(
        "GET /admin/api/v1/summary HTTP/1.1\r\nHost: x\r\nX-Admin-Token: {}\r\nConnection: close\r\n\r\n",
        TOKEN
    );
    let mut stream = s;
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;
    let mut buf = Vec::new();
    time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await??;
    let resp = String::from_utf8_lossy(&buf);
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "X-Admin-Token header must work"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_api_tunnels_shape() -> Result<()> {
    // (T-COMPAT part 1) JSON response has expected keys even when empty.
    const PORT: u16 = 17971;
    let _g = SERIAL_GUARD.lock().await;
    wait_port(PORT, false).await;

    let mut server = Server::new(10000..=65535, None);
    server.set_control_port(PORT);
    server.set_admin_token(Some(TOKEN.into()));
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    // Query the tunnels endpoint (no clients yet).
    let s = TcpStream::connect(("127.0.0.1", PORT)).await?;
    let resp = http_get(s, "/admin/api/v1/tunnels", Some(TOKEN)).await?;
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "response: {}",
        &resp[..100.min(resp.len())]
    );
    assert!(
        resp.contains("["),
        "tunnels JSON must be an array (even if empty)"
    );

    // Query summary endpoint.
    let s = TcpStream::connect(("127.0.0.1", PORT)).await?;
    let resp = http_get(s, "/admin/api/v1/summary", Some(TOKEN)).await?;
    assert!(resp.starts_with("HTTP/1.1 200"));
    assert!(
        resp.contains("\"control_port\""),
        "summary must have control_port"
    );
    assert!(resp.contains("\"version\""), "summary must have version");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_assets_table_nonempty() -> Result<()> {
    // (Phase 3.1) ADMIN_ASSETS contains the index.html and at least app.js/style.css.
    use bore_cli::admin_http::ADMIN_ASSETS;

    let has_index = ADMIN_ASSETS
        .iter()
        .any(|(url, _, _)| *url == "/admin/ui/index.html");
    assert!(has_index, "ADMIN_ASSETS must contain /admin/ui/index.html");

    let has_app_js = ADMIN_ASSETS
        .iter()
        .any(|(url, _, _)| *url == "/admin/ui/app.js");
    assert!(has_app_js, "ADMIN_ASSETS must contain /admin/ui/app.js");

    let has_style = ADMIN_ASSETS
        .iter()
        .any(|(url, _, _)| *url == "/admin/ui/style.css");
    assert!(has_style, "ADMIN_ASSETS must contain /admin/ui/style.css");

    // Check content types.
    let html_type = ADMIN_ASSETS
        .iter()
        .find(|(url, _, _)| *url == "/admin/ui/index.html")
        .map(|(_, _, ct)| ct);
    assert_eq!(html_type, Some(&"text/html; charset=utf-8"));

    let js_type = ADMIN_ASSETS
        .iter()
        .find(|(url, _, _)| *url == "/admin/ui/app.js")
        .map(|(_, _, ct)| ct);
    assert_eq!(js_type, Some(&"text/javascript; charset=utf-8"));

    let css_type = ADMIN_ASSETS
        .iter()
        .find(|(url, _, _)| *url == "/admin/ui/style.css")
        .map(|(_, _, ct)| ct);
    assert_eq!(css_type, Some(&"text/css; charset=utf-8"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_asset_exact_key_only() -> Result<()> {
    // (Phase 3.2) Asset route only serves exact keys from ADMIN_ASSETS, no filesystem or path traversal.
    const PORT: u16 = 17972;
    let _g = SERIAL_GUARD.lock().await;
    wait_port(PORT, false).await;

    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(PORT);
    server.set_admin_token(Some(TOKEN.into()));
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    // Try to traverse: /admin/ui/../secret → 404
    let s = TcpStream::connect(("127.0.0.1", PORT)).await?;
    let resp = http_get(s, "/admin/ui/../secret", None).await?;
    assert!(
        resp.starts_with("HTTP/1.1 404"),
        "path traversal attempt must be 404: {resp:.50}"
    );

    // Percent-encoded traversal: /admin/ui/%2e%2esecret → 404
    let s = TcpStream::connect(("127.0.0.1", PORT)).await?;
    let resp = http_get(s, "/admin/ui/%2e%2esecret", None).await?;
    assert!(
        resp.starts_with("HTTP/1.1 404"),
        "percent-encoded traversal must be 404"
    );

    // Real asset: /admin/ui/style.css → 200
    let s = TcpStream::connect(("127.0.0.1", PORT)).await?;
    let resp = http_get(s, "/admin/ui/style.css", None).await?;
    assert!(resp.starts_with("HTTP/1.1 200"), "real asset must be 200");
    assert!(
        resp.contains("text/css"),
        "asset must have correct content-type"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_shell_served() -> Result<()> {
    // (Phase 3.2) /admin/status returns 200 text/html.
    const PORT: u16 = 17973;
    let _g = SERIAL_GUARD.lock().await;
    wait_port(PORT, false).await;

    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(PORT);
    server.set_admin_token(Some(TOKEN.into()));
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let s = TcpStream::connect(("127.0.0.1", PORT)).await?;
    let resp = http_get(s, "/admin/status", None).await?;
    assert!(resp.starts_with("HTTP/1.1 200"), "shell must be 200");
    assert!(resp.contains("text/html"), "shell must be HTML");
    assert!(
        resp.contains("<!DOCTYPE"),
        "shell must contain HTML doctype"
    );
    assert!(
        resp.contains("Content-Security-Policy:"),
        "shell must have CSP header"
    );

    // Also test the /admin/ alias.
    let s = TcpStream::connect(("127.0.0.1", PORT)).await?;
    let resp = http_get(s, "/admin/", None).await?;
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "/admin/ must also serve the shell"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_legacy_data_compat() -> Result<()> {
    // (Phase 5.2 / T-COMPAT) /admin/status/data must have EXACTLY the legacy shape.
    // Top-level keys: "server" object and "tunnels" array.
    // Server object must have: control_port, tls, udp
    // Tunnels array contains entries with standard EntryView fields.
    const PORT: u16 = 17974;
    let _g = SERIAL_GUARD.lock().await;
    wait_port(PORT, false).await;

    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(PORT);
    server.set_admin_token(Some(TOKEN.into()));
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let s = TcpStream::connect(("127.0.0.1", PORT)).await?;
    let resp = http_get(s, "/admin/status/data", Some(TOKEN)).await?;
    assert!(resp.starts_with("HTTP/1.1 200"), "legacy data must be 200");

    // Parse JSON and verify the exact legacy structure
    let body_start = resp.find('{').unwrap_or(0);
    let body = &resp[body_start..];
    let data: serde_json::Value =
        serde_json::from_str(body).expect("legacy data must be valid JSON");

    // Top-level must be an object with exactly "server" and "tunnels" keys
    assert!(data.is_object(), "legacy data root must be an object");
    let obj = data.as_object().expect("already checked is_object");
    assert!(
        obj.contains_key("server"),
        "legacy data must have 'server' key at top level"
    );
    assert!(
        obj.contains_key("tunnels"),
        "legacy data must have 'tunnels' key at top level"
    );

    // The "server" object must contain control_port, tls, udp
    let server_obj = &obj["server"];
    assert!(server_obj.is_object(), "server must be an object");
    let server_data = server_obj.as_object().expect("already checked is_object");
    assert!(
        server_data.contains_key("control_port"),
        "server must have control_port"
    );
    assert!(
        server_data["control_port"].is_number(),
        "control_port must be a number"
    );
    assert!(server_data.contains_key("tls"), "server must have tls");
    assert!(server_data["tls"].is_boolean(), "tls must be a boolean");
    assert!(server_data.contains_key("udp"), "server must have udp");
    assert!(server_data["udp"].is_boolean(), "udp must be a boolean");

    // The "tunnels" array must be present (and can be empty)
    let tunnels = &obj["tunnels"];
    assert!(tunnels.is_array(), "tunnels must be an array");
    // If there are tunnel entries, they should have basic EntryView fields
    // (spot-check a few expected fields from EntryView struct definition in admin.rs:102-136)
    let tunnel_entries = tunnels.as_array().expect("already checked is_array");
    for entry in tunnel_entries {
        assert!(entry.is_object(), "each tunnel entry must be an object");
        let entry_obj = entry.as_object().expect("already checked is_object");
        // These are the core EntryView fields defined in src/admin.rs:102-136
        assert!(entry_obj.contains_key("id"), "entry must have id");
        assert!(entry_obj.contains_key("role"), "entry must have role");
        assert!(entry_obj.contains_key("peer"), "entry must have peer");
        assert!(entry_obj.contains_key("active"), "entry must have active");
    }

    Ok(())
}
