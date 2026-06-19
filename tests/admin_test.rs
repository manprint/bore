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
        None,
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

/// Recursively walk all keys in a JSON object and assert no (case-insensitive) match
/// to secret, key, password, or token. Exempts public metadata field names like
/// `secret_id`, `secret_tunnels` (counters, not secret values).
fn assert_no_secret_keys(value: &serde_json::Value, path: &str) {
    if let Some(obj) = value.as_object() {
        for (key, val) in obj.iter() {
            // Check key name (case-insensitive).
            let key_lower = key.to_lowercase();
            // Exempt specific non-secret metadata fields:
            // - `secret_id`: public tunnel identifier (not a secret value)
            // - `secret_tunnels`: a counter of tunnels with secret IDs (not a secret value)
            let is_exempt = key_lower == "secret_id" || key_lower == "secret_tunnels";
            if !is_exempt {
                assert!(
                    !key_lower.contains("secret")
                        && !key_lower.contains("key")
                        && !key_lower.contains("password")
                        && !key_lower.contains("token"),
                    "forbidden key '{}' at path {}{}",
                    key,
                    path,
                    if path.is_empty() { "" } else { "." }
                );
            }
            // Recurse into nested objects and arrays.
            if val.is_object() {
                let new_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", path, key)
                };
                assert_no_secret_keys(val, &new_path);
            } else if let Some(arr) = val.as_array() {
                let new_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", path, key)
                };
                for (idx, item) in arr.iter().enumerate() {
                    if item.is_object() {
                        assert_no_secret_keys(item, &format!("{}[{}]", new_path, idx));
                    }
                }
            }
        }
    }
}

#[test]
fn t_views_serialize_stable() {
    use bore_cli::admin_views::*;

    // Test that all admin views serialize safely without leaking secrets (T-SANITIZE gate).
    // Build representative instances of each view and verify no forbidden keys appear
    // (recursively, including nested objects/arrays). Key names checked case-insensitively.
    // Exempts `secret_id` (public tunnel identifier, not a secret value).

    // ConfigView.
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
        vhost_config: None,
        vhost_cert_file: None,
        tls: false,
    };
    let json_config = serde_json::to_value(&config).expect("serialize ConfigView");
    assert_no_secret_keys(&json_config, "");

    // SummaryView.
    let summary = SummaryView {
        version: "1.0.0 - main - abc1234".into(),
        control_port: 7835,
        tls: true,
        udp: false,
        vpn_enabled: false,
        vhost_enabled: true,
        uptime_secs: 42,
        public_tunnels: 1,
        secret_tunnels: 2,
        vhost_domains: 2,
        #[cfg(feature = "vpn")]
        vpn_links: 0,
        vhost_http_port: Some(80),
        vhost_https_port: Some(443),
        vhost_quic_port: Some(443),
        port_range: "5000-6000".into(),
        bind_tunnels: "0.0.0.0".into(),
    };
    let json_summary = serde_json::to_value(&summary).expect("serialize SummaryView");
    assert_no_secret_keys(&json_summary, "");

    // TunnelView (public tunnel).
    let tunnel = TunnelView {
        id: 1,
        peer: "192.0.2.1:54321".into(),
        public_port: Some(5000),
        notes: Some("test".into()),
        basic_auth: false,
        https: false,
        force_https: false,
        carriers: 1,
        auto_reconnect: false,
        udp: false,
        overlay: None,
        vpn_direct: false,
        active: 3,
        uptime_secs: 100,
        relay_tx_bytes: 1024,
        relay_rx_bytes: 2048,
    };
    let json_tunnel = serde_json::to_value(&tunnel).expect("serialize TunnelView");
    assert_no_secret_keys(&json_tunnel, "");

    // SecretView (secret tunnel with secret_id field, which must be exempted).
    let secret = SecretView {
        id: 2,
        role: "SecretProvider".into(),
        peer: "192.0.2.2:54322".into(),
        secret_id: Some("abc123def456".into()), // Allowed field; exempted in check.
        notes: None,
        basic_auth: false,
        carriers: 1,
        udp: false,
        active: 0,
        uptime_secs: 50,
        relay_tx_bytes: 512,
        relay_rx_bytes: 1024,
    };
    let json_secret = serde_json::to_value(&secret).expect("serialize SecretView");
    assert_no_secret_keys(&json_secret, "");
    // Verify secret_id is actually present (not stripped).
    assert_eq!(json_secret["secret_id"], "abc123def456");

    // VhostView.
    let vhost = VhostView {
        subdomain: "example".into(),
        active: 2,
        carriers: 1,
        direct_stream_opens: 10,
        request_headers: vec!["x-forwarded-for".into()],
        response_headers: vec!["x-custom".into()],
        request_header_pairs: vec![("x-app".into(), "value".into())],
        response_header_pairs: vec![("x-app-version".into(), "1.0".into())],
        direct_pool: 5,
        tls: true,
    };
    let json_vhost = serde_json::to_value(&vhost).expect("serialize VhostView");
    assert_no_secret_keys(&json_vhost, "");

    // MetricsView (includes new counters).
    let metrics = MetricsView {
        uptime_secs: 300,
        mem_rss_bytes: Some(50_000_000),
        bandwidth_tx_bytes: 1_000_000,
        bandwidth_rx_bytes: 2_000_000,
        public_tunnels: 1,
        secret_tunnels: 2,
        vhost_domains: 1,
        #[cfg(feature = "vpn")]
        vpn_links: 0,
        active_connections: 5,
        auth_failures: 0,
        conn_rejections: 0,
        direct_fallbacks: 0,
    };
    let json_metrics = serde_json::to_value(&metrics).expect("serialize MetricsView");
    assert_no_secret_keys(&json_metrics, "");

    // VpnLinkView (cfg vpn).
    #[cfg(feature = "vpn")]
    {
        let vpn_link = VpnLinkView {
            id: 3,
            link_id: "site-a".into(),
            role: "vpnlistener".into(),
            peer: "192.0.2.3:54323".into(),
            notes: Some("edge gateway".into()),
            overlay: Some("10.99.0.1/32".into()),
            advertised: vec!["10.0.0.0/24".into()],
            carriers: 4,
            direct: false,
            path: "relay".into(),
            relay_tx_bytes: 256,
            relay_rx_bytes: 512,
            uptime_secs: 75,
            mode: "1:1".into(),
            auto_reconnect: true,
            relay_only: false,
            pin_mtu: false,
            mtu: Some(1350),
            forward_accept: true,
            nat_masquerade: false,
            route_policy: Some("accept-all".into()),
            nat_udp_port: Some(443),
            hub_peers: None,
        };
        let json_vpn = serde_json::to_value(&vpn_link).expect("serialize VpnLinkView");
        assert_no_secret_keys(&json_vpn, "");

        // VpnLinkView with hub_peers (nested VpnPeerView objects).
        let vpn_hub = VpnLinkView {
            id: 4,
            link_id: "hub1".into(),
            role: "vpnlistener".into(),
            peer: "192.0.2.4:54324".into(),
            notes: None,
            overlay: Some("10.99.0.1/25".into()),
            advertised: vec![],
            carriers: 2,
            direct: true,
            path: "direct".into(),
            relay_tx_bytes: 512,
            relay_rx_bytes: 1024,
            uptime_secs: 120,
            mode: "hub".into(),
            auto_reconnect: false,
            relay_only: false,
            pin_mtu: false,
            mtu: None,
            forward_accept: false,
            nat_masquerade: false,
            route_policy: None,
            nat_udp_port: None,
            hub_peers: Some(vec![
                VpnPeerView {
                    peer_id: 1,
                    overlay: "10.99.0.2/32".into(),
                    peer: "192.0.2.100:12345".into(),
                    advertised: vec!["192.168.1.0/24".into()],
                },
                VpnPeerView {
                    peer_id: 2,
                    overlay: "10.99.0.3/32".into(),
                    peer: "192.0.2.101:12346".into(),
                    advertised: vec!["192.168.2.0/24".into()],
                },
            ]),
        };
        let json_vpn_hub = serde_json::to_value(&vpn_hub).expect("serialize VpnLinkView with hub");
        assert_no_secret_keys(&json_vpn_hub, "");
    }
}

/// T-VPNPANEL: the VPN section builder must source carriers / notes / advertised /
/// NAT-UDP-port / flags from the long-lived admin entry (NOT the ephemeral provider
/// registry, which is gone after a 1:1 link pairs), and expose the shared `link_id`
/// so the UI can group the listener and connector together. Regression guard for the
/// "Carriers: 1, no notes, no nat port, everything disconnected" bug cluster.
#[cfg(feature = "vpn")]
#[test]
fn t_vpn_panel_groups_and_fields() {
    use bore_cli::admin::{NewEntry, Role};
    use bore_cli::server::Server;

    fn vpn_entry(role: Role, peer: &str, note: &str) -> NewEntry {
        NewEntry {
            role,
            peer: peer.parse().unwrap(),
            secret_id: Some("vpn:site-a".into()),
            public_port: None,
            notes: Some(note.into()),
            basic_auth: false,
            https: false,
            force_https: false,
            carriers: 4,
            auto_reconnect: false,
            udp: false,
            vpn_relay_only: false,
            vpn_pin_mtu: false,
            vpn_mtu: Some(1350),
            vpn_forward_accept: true,
            vpn_nat_masquerade: false,
            vpn_route_policy: None,
            vpn_advertised: vec!["10.10.0.0/24".into()],
            vpn_nat_udp_port: Some(443),
        }
    }

    let server = Server::new(1024..=65535, None);
    // No provider registry entry exists (mirrors a paired 1:1 link). The builder
    // must still report the right data straight from the admin entries.
    let _l = server.admin_registry().register(vpn_entry(
        Role::VpnListener,
        "192.0.2.3:5001",
        "edge gateway",
    ));
    let _c = server.admin_registry().register(vpn_entry(
        Role::VpnConnector,
        "192.0.2.9:5002",
        "branch office",
    ));

    let view = bore_cli::admin_api::vpn(&server);
    assert_eq!(view.links.len(), 2, "both endpoints present");

    // Both sides share the grouping key.
    for link in &view.links {
        assert_eq!(link.link_id, "site-a", "shared link_id for grouping");
        assert_eq!(
            link.carriers, 4,
            "carriers from entry, not provider default 1"
        );
        assert_eq!(
            link.nat_udp_port,
            Some(443),
            "nat-udp-preferred-port surfaced"
        );
        assert_eq!(link.advertised, vec!["10.10.0.0/24".to_string()]);
        assert!(link.forward_accept, "flag badges fed from entry");
        assert_eq!(link.mtu, Some(1350));
        assert!(link.notes.is_some(), "operator notes surfaced");
    }

    let listener = view
        .links
        .iter()
        .find(|l| l.role == "vpnlistener")
        .expect("listener side");
    assert_eq!(listener.notes.as_deref(), Some("edge gateway"));
    let connector = view
        .links
        .iter()
        .find(|l| l.role == "vpnconnector")
        .expect("connector side");
    assert_eq!(connector.notes.as_deref(), Some("branch office"));
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
async fn t_csp() -> Result<()> {
    // (Phase 4 / F10) CSP header must have img-src 'self' only (no data:).
    const PORT: u16 = 17975;
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

    // Extract and verify CSP header
    let csp_line = resp
        .lines()
        .find(|l| l.starts_with("Content-Security-Policy:"))
        .expect("CSP header must be present");

    assert!(
        csp_line.contains("img-src 'self'"),
        "CSP must contain img-src 'self'"
    );
    assert!(
        !csp_line.contains("img-src 'self' data:"),
        "CSP must not contain data: in img-src"
    );
    assert!(
        !csp_line.contains("img-src 'self' data:") && !csp_line.contains("img-src 'self', data:"),
        "CSP img-src must not allow data: URIs"
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
