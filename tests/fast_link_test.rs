//! Integration tests for the fast link transfer service (plan 004, sub-phase
//! 1.3): a real `bore server` with the fast link vhost host wired up, hit
//! over real TLS with hand-written HTTP/1.1, exercising every ingress the
//! feature routes on (dedicated vhost frontend, unified control port, plain
//! HTTP) plus admin visibility, native-registration reservation and
//! `--max-conns`.
//!
//! `rcgen` (the self-signed certificate generator) lives behind the `udp`
//! feature, same as `tests/vhost_test.rs`.
#![cfg(feature = "udp")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use bore_cli::{
    client::{Client, ProviderMeta},
    fast_link::{self, FastLink, FastLinkServerArgs},
    server::Server,
    transport,
    vhost::{VhostConfig, VhostModeCfg},
};
use lazy_static::lazy_static;
use rcgen::generate_simple_self_signed;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::crypto::ring;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

lazy_static! {
    /// Serializes every test in this file: the fixed 18400-18499 port block is
    /// shared and a `#[tokio::test]` runtime only releases its listeners after
    /// the body drops `SERIAL`.
    static ref SERIAL: Mutex<()> = Mutex::new(());
}

const HOST: &str = "fast.bore.local";
const AUTH: &str = "u:p";
const PORT_WAIT_BUDGET: Duration = Duration::from_secs(30);

// ─── Small helpers shared with the rest of the vhost test suite (duplicated
// here rather than shared, matching the existing per-file pattern) ─────────

async fn wait_port(port: u16, listening: bool) {
    let deadline = time::Instant::now() + PORT_WAIT_BUDGET;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() == listening {
            return;
        }
        assert!(
            time::Instant::now() < deadline,
            "port {port} never became {} within {PORT_WAIT_BUDGET:?}",
            if listening { "reachable" } else { "free" },
        );
        time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_bound(port: u16, handle: tokio::task::JoinHandle<Result<()>>) {
    let deadline = time::Instant::now() + PORT_WAIT_BUDGET;
    loop {
        if handle.is_finished() {
            panic!(
                "server on port {port} stopped before it served: {:?}",
                handle.await
            );
        }
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        assert!(
            time::Instant::now() < deadline,
            "server on port {port} never accepted within {PORT_WAIT_BUDGET:?}",
        );
        time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_slots_empty(fast: &FastLink) {
    let deadline = time::Instant::now() + Duration::from_secs(10);
    loop {
        if fast.slots_len() == 0 {
            return;
        }
        assert!(
            time::Instant::now() < deadline,
            "a fast link slot was left registered"
        );
        time::sleep(Duration::from_millis(20)).await;
    }
}

/// Generate a self-signed cert covering the wildcard base domain (SANs
/// `*.bore.local` and `bore.local`, as required by the sub-phase contract).
fn self_signed_cert() -> Result<(String, String)> {
    let key =
        generate_simple_self_signed(vec!["*.bore.local".to_string(), "bore.local".to_string()])?;
    Ok((key.cert.pem(), key.signing_key.serialize_pem()))
}

fn write_pem_files(cert_pem: &str, key_pem: &str) -> Result<(PathBuf, PathBuf)> {
    let id = uuid::Uuid::new_v4();
    let mut cert_path = std::env::temp_dir();
    cert_path.push(format!("bore_fast_link_test_{id}_cert.pem"));
    let mut key_path = std::env::temp_dir();
    key_path.push(format!("bore_fast_link_test_{id}_key.pem"));
    std::fs::write(&cert_path, cert_pem)?;
    std::fs::write(&key_path, key_pem)?;
    Ok((cert_path, key_path))
}

/// A `TlsConnector` that trusts exactly the generated test certificate (not
/// the insecure accept-any verifier `vhost_test.rs` uses elsewhere): the
/// contract for this file calls for a real root store and SNI `fast.bore.local`.
fn fast_tls_connector(cert_pem: &str) -> Result<TlsConnector> {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(cert_pem.as_bytes()) {
        roots.add(cert.context("parse test certificate")?)?;
    }
    let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("configure TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

async fn https_connect(port: u16, connector: &TlsConnector) -> Result<TlsStream<TcpStream>> {
    let tcp = time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await??;
    let name = ServerName::try_from(HOST.to_string())?;
    Ok(connector.clone().connect(name, tcp).await?)
}

fn basic_auth_header() -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(AUTH.as_bytes());
    format!("Authorization: Basic {token}\r\n")
}

// ─── Server topologies ──────────────────────────────────────────────────────

/// Which frontend shape serves the fast host: a dedicated vhost HTTPS/HTTP
/// frontend (its own ports), or the unified single control port (vhost
/// `https_port == control_port`, TLS installed on the control port itself).
enum Topology {
    /// `both`: the vhost frontend serves plain HTTP on `http_port` and HTTPS
    /// on its own `https_port`, both distinct from `control`.
    Dedicated { http_port: u16, https_port: u16 },
    /// The vhost frontend serves plain HTTP on `http_port`; HTTPS for
    /// everything (including the fast host) arrives through the control
    /// port's own TLS acceptor.
    Unified { http_port: u16 },
}

/// Spawn a real `bore server` with the fast link transfer service enabled,
/// wired onto the requested topology. Returns the shared engine (for
/// `slots_len()`/metrics assertions) and the PEM certificate the caller can
/// build a matching `TlsConnector` from.
async fn spawn_fast_server(
    control: u16,
    topology: Topology,
    max_conns: Option<usize>,
    admin_token: Option<&str>,
) -> Result<(Arc<FastLink>, String)> {
    let (cert_pem, key_pem) = self_signed_cert()?;
    let (cert_path, key_path) = write_pem_files(&cert_pem, &key_pem)?;

    wait_port(control, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(control);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    if let Some(n) = max_conns {
        server.set_max_conns(n);
    }
    if let Some(token) = admin_token {
        server.set_admin_token(Some(token.to_string()));
    }

    let cfg = match topology {
        Topology::Dedicated {
            http_port,
            https_port,
        } => VhostConfig {
            base_domain: "bore.local".to_string(),
            mode: VhostModeCfg::Auto,
            http_port,
            https_port,
            cert_file: Some(cert_path),
            key_file: Some(key_path),
            default_headers: Default::default(),
            default_response_headers: Default::default(),
            reservations: vec![],
        },
        Topology::Unified { http_port } => {
            let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
            server.set_tls(acceptor);
            VhostConfig {
                base_domain: "bore.local".to_string(),
                mode: VhostModeCfg::Http,
                http_port,
                https_port: 443,
                cert_file: None,
                key_file: None,
                default_headers: Default::default(),
                default_response_headers: Default::default(),
                reservations: vec![],
            }
        }
    };
    server.set_vhost(cfg)?;

    let args = FastLinkServerArgs {
        enabled: true,
        vhost: Some(HOST.to_string()),
        auth: Some(AUTH.to_string()),
        wait_timeout_secs: fast_link::DEFAULT_WAIT_TIMEOUT_SECS,
        max_active: fast_link::DEFAULT_MAX_ACTIVE,
    };
    let resolution =
        fast_link::resolve_server_config(&args, server.vhost_base_domain().as_deref())?;
    server.set_fast_link(resolution.config.expect("fast link transfer enabled"))?;
    let fast = server.fast_link().expect("fast link transfer installed");

    let handle = tokio::spawn(server.listen());
    wait_bound(control, handle).await;

    Ok((fast, cert_pem))
}

// ─── Wire-level request/response helpers ───────────────────────────────────

/// Read bytes from `stream` into `pending` until it contains a complete
/// `"\r\n\r\n"`-terminated head, then drain and return that head as text.
async fn read_head<R: AsyncRead + Unpin>(stream: &mut R, pending: &mut Vec<u8>) -> Result<String> {
    loop {
        if let Some(pos) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&pending[..pos + 4]).into_owned();
            pending.drain(0..pos + 4);
            return Ok(head);
        }
        let mut buf = [0u8; 4096];
        let n = time::timeout(Duration::from_secs(20), stream.read(&mut buf)).await??;
        anyhow::ensure!(n > 0, "connection closed before a complete response head");
        pending.extend_from_slice(&buf[..n]);
    }
}

/// Read exactly one chunked-transfer-coding chunk from `stream`/`pending`.
/// Returns `None` on the terminator (`"0\r\n\r\n"`), `Some(payload)` otherwise.
/// Robust to the payload arriving split across any number of `read()` calls.
async fn read_one_chunk<R: AsyncRead + Unpin>(
    stream: &mut R,
    pending: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    loop {
        if let Some(pos) = pending.windows(2).position(|w| w == b"\r\n") {
            let size_line = std::str::from_utf8(&pending[..pos])
                .context("non-utf8 chunk size line")?
                .trim();
            let size = usize::from_str_radix(size_line, 16).context("bad chunk size")?;
            let needed = pos + 2 + size + 2;
            if pending.len() >= needed {
                let payload = pending[pos + 2..pos + 2 + size].to_vec();
                pending.drain(0..needed);
                if size == 0 {
                    return Ok(None);
                }
                return Ok(Some(payload));
            }
        }
        let mut buf = [0u8; 16384];
        let n = time::timeout(Duration::from_secs(20), stream.read(&mut buf)).await??;
        anyhow::ensure!(n > 0, "connection closed mid-chunk");
        pending.extend_from_slice(&buf[..n]);
    }
}

/// Read every remaining chunk until the terminator, concatenating decoded
/// payloads (text status lines from the uploader, or raw body bytes from a
/// chunked downloader).
async fn read_all_chunks<R: AsyncRead + Unpin>(
    stream: &mut R,
    pending: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(payload) = read_one_chunk(stream, pending).await? {
        out.extend_from_slice(&payload);
    }
    Ok(out)
}

/// Read exactly `len` body bytes (a `Content-Length` download), consuming
/// `pending` first.
async fn read_exact_body<R: AsyncRead + Unpin>(
    stream: &mut R,
    pending: &mut Vec<u8>,
    len: usize,
) -> Result<Vec<u8>> {
    while pending.len() < len {
        let mut buf = [0u8; 65536];
        let n = time::timeout(Duration::from_secs(20), stream.read(&mut buf)).await??;
        anyhow::ensure!(n > 0, "connection closed before the full body arrived");
        pending.extend_from_slice(&buf[..n]);
    }
    Ok(pending[..len].to_vec())
}

/// Extract the download link (`https://.../<id>/<name>`) from the uploader's
/// first status chunk.
fn extract_link(status_text: &str) -> String {
    status_text
        .lines()
        .find(|l| l.starts_with("https://"))
        .unwrap_or_else(|| panic!("no download link in uploader status: {status_text:?}"))
        .trim()
        .to_string()
}

/// The `<id>/<name>` path (and query, if any) from a full download link.
fn link_path(link: &str) -> String {
    let after_scheme = link.split_once("://").expect("scheme").1;
    let path = after_scheme.split_once('/').expect("path").1;
    format!("/{path}")
}

// ─── T-FL-I1: dedicated HTTPS frontend, Content-Length upload ──────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dedicated_https_frontend_streams_a_cl_upload() -> Result<()> {
    let _guard = SERIAL.lock().await;
    const CTRL: u16 = 18400;
    const HTTP: u16 = 18401;
    const HTTPS: u16 = 18402;

    let (fast, cert_pem) = spawn_fast_server(
        CTRL,
        Topology::Dedicated {
            http_port: HTTP,
            https_port: HTTPS,
        },
        None,
        None,
    )
    .await?;
    wait_port(HTTPS, true).await;
    let connector = fast_tls_connector(&cert_pem)?;

    let payload: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let expected_sha = Sha256::digest(&payload);

    let uploader = https_connect(HTTPS, &connector).await?;
    let (mut u_read, mut u_write) = tokio::io::split(uploader);

    let head = format!(
        "PUT /archive.tar HTTP/1.1\r\nHost: {HOST}\r\n{}Content-Length: {}\r\n\r\n",
        basic_auth_header(),
        payload.len()
    );
    u_write.write_all(head.as_bytes()).await?;

    let body = payload.clone();
    let writer = tokio::spawn(async move {
        u_write.write_all(&body).await?;
        u_write.flush().await?;
        anyhow::Ok(u_write)
    });

    let mut u_pending = Vec::new();
    let u_head = read_head(&mut u_read, &mut u_pending).await?;
    assert!(u_head.starts_with("HTTP/1.1 200 OK"), "got: {u_head}");

    // First chunk: the link. Second: the waiting notice.
    let link_chunk = read_one_chunk(&mut u_read, &mut u_pending)
        .await?
        .expect("link chunk");
    let link = extract_link(&String::from_utf8_lossy(&link_chunk));
    assert!(
        link.starts_with(&format!("https://{HOST}")),
        "unexpected link: {link}"
    );
    let waiting_chunk = read_one_chunk(&mut u_read, &mut u_pending)
        .await?
        .expect("waiting chunk");
    assert!(String::from_utf8_lossy(&waiting_chunk).starts_with("# waiting"));

    // Now claim the download while the uploader is still streaming its body.
    let mut downloader = https_connect(HTTPS, &connector).await?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n",
        link_path(&link)
    );
    downloader.write_all(req.as_bytes()).await?;
    let mut d_pending = Vec::new();
    let d_head = read_head(&mut downloader, &mut d_pending).await?;
    assert!(d_head.starts_with("HTTP/1.1 200 OK"), "got: {d_head}");
    let cl: usize = d_head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .expect("Content-Length header")
        .trim()
        .parse()?;
    assert_eq!(cl, payload.len());
    let downloaded = read_exact_body(&mut downloader, &mut d_pending, cl).await?;
    assert_eq!(
        downloaded, payload,
        "downloaded bytes must match byte-for-byte"
    );
    assert_eq!(
        Sha256::digest(&downloaded).as_slice(),
        expected_sha.as_slice()
    );

    writer.await??;

    // Remaining chunks on the uploader: "# download started", then "# done:".
    let rest = read_all_chunks(&mut u_read, &mut u_pending).await?;
    let rest_text = String::from_utf8_lossy(&rest);
    assert!(rest_text.contains("# download started"), "got: {rest_text}");
    assert!(
        rest_text.contains(&format!("# done: {} bytes", payload.len())),
        "got: {rest_text}"
    );

    wait_slots_empty(&fast).await;
    Ok(())
}

// ─── T-FL-I2: unified control port, chunked upload ─────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unified_control_port_streams_a_chunked_upload() -> Result<()> {
    let _guard = SERIAL.lock().await;
    const CTRL: u16 = 18403;
    const HTTP: u16 = 18404;

    let (fast, cert_pem) =
        spawn_fast_server(CTRL, Topology::Unified { http_port: HTTP }, None, None).await?;
    let connector = fast_tls_connector(&cert_pem)?;

    let payload: Vec<u8> = (0..600_000).map(|i| ((i * 7) % 253) as u8).collect();

    let uploader = https_connect(CTRL, &connector).await?;
    let (mut u_read, mut u_write) = tokio::io::split(uploader);

    let head = format!(
        "PUT /dir.tar HTTP/1.1\r\nHost: {HOST}\r\n{}Transfer-Encoding: chunked\r\n\r\n",
        basic_auth_header()
    );
    u_write.write_all(head.as_bytes()).await?;

    // The relay forwards the uploader's chunked wire framing verbatim (never
    // re-decoded/re-encoded), so the "wire" length the completion message
    // reports includes the chunk-size lines and terminator, not just the
    // decoded payload length.
    let mut wire = Vec::new();
    for piece in payload.chunks(64 * 1024) {
        wire.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
        wire.extend_from_slice(piece);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"0\r\n\r\n");
    let wire_len = wire.len();
    let writer = tokio::spawn(async move {
        u_write.write_all(&wire).await?;
        u_write.flush().await?;
        anyhow::Ok(u_write)
    });

    let mut u_pending = Vec::new();
    let u_head = read_head(&mut u_read, &mut u_pending).await?;
    assert!(u_head.starts_with("HTTP/1.1 200 OK"), "got: {u_head}");
    let link_chunk = read_one_chunk(&mut u_read, &mut u_pending)
        .await?
        .expect("link chunk");
    let link = extract_link(&String::from_utf8_lossy(&link_chunk));
    let _waiting = read_one_chunk(&mut u_read, &mut u_pending).await?;

    let mut downloader = https_connect(CTRL, &connector).await?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n",
        link_path(&link)
    );
    downloader.write_all(req.as_bytes()).await?;
    let mut d_pending = Vec::new();
    let d_head = read_head(&mut downloader, &mut d_pending).await?;
    assert!(d_head.starts_with("HTTP/1.1 200 OK"), "got: {d_head}");
    assert!(
        d_head.to_lowercase().contains("transfer-encoding: chunked"),
        "a chunked upload with unknown length must be forwarded chunked, got: {d_head}"
    );
    let downloaded = read_all_chunks(&mut downloader, &mut d_pending).await?;
    assert_eq!(
        downloaded, payload,
        "chunked download must reassemble byte-for-byte"
    );

    writer.await??;
    let rest = read_all_chunks(&mut u_read, &mut u_pending).await?;
    let rest_text = String::from_utf8_lossy(&rest);
    assert!(
        rest_text.contains(&format!("# done: {wire_len} bytes")),
        "got: {rest_text}"
    );

    wait_slots_empty(&fast).await;
    Ok(())
}

// ─── T-FL-I3: plain HTTP frontend refuses uploads, redirects downloads ─────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plain_http_frontend_refuses_uploads_and_redirects_downloads() -> Result<()> {
    let _guard = SERIAL.lock().await;
    const CTRL: u16 = 18405;
    const HTTP: u16 = 18406;
    const HTTPS: u16 = 18407;

    let (fast, _cert_pem) = spawn_fast_server(
        CTRL,
        Topology::Dedicated {
            http_port: HTTP,
            https_port: HTTPS,
        },
        None,
        None,
    )
    .await?;
    wait_port(HTTP, true).await;
    wait_port(HTTPS, true).await;

    // PUT over plain HTTP -> 403.
    let mut conn = TcpStream::connect(("127.0.0.1", HTTP)).await?;
    let req = format!(
        "PUT /f.bin HTTP/1.1\r\nHost: {HOST}\r\n{}Content-Length: 3\r\nConnection: close\r\n\r\nabc",
        basic_auth_header()
    );
    conn.write_all(req.as_bytes()).await?;
    let mut resp = Vec::new();
    time::timeout(Duration::from_secs(5), conn.read_to_end(&mut resp)).await??;
    let resp = String::from_utf8_lossy(&resp);
    assert!(resp.starts_with("HTTP/1.1 403"), "got: {resp}");

    // GET over plain HTTP -> 308 to the real HTTPS port.
    let mut conn = TcpStream::connect(("127.0.0.1", HTTP)).await?;
    let req = format!("GET /x HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n");
    conn.write_all(req.as_bytes()).await?;
    let mut resp = Vec::new();
    time::timeout(Duration::from_secs(5), conn.read_to_end(&mut resp)).await??;
    let resp = String::from_utf8_lossy(&resp);
    assert!(resp.starts_with("HTTP/1.1 308"), "got: {resp}");
    let location = resp
        .lines()
        .find_map(|l| l.strip_prefix("Location: "))
        .expect("Location header")
        .trim();
    assert!(
        location.starts_with(&format!("https://{HOST}:{HTTPS}")),
        "expected redirect to the real HTTPS port {HTTPS}, got: {location}"
    );

    wait_slots_empty(&fast).await;
    Ok(())
}

// ─── T-FL-I4: native vhost registration of the fast label is rejected ──────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_vhost_registration_of_the_fast_label_is_rejected() -> Result<()> {
    let _guard = SERIAL.lock().await;
    const CTRL: u16 = 18408;
    const HTTP: u16 = 18409;
    const HTTPS: u16 = 18410;

    let (_fast, _cert_pem) = spawn_fast_server(
        CTRL,
        Topology::Dedicated {
            http_port: HTTP,
            https_port: HTTPS,
        },
        None,
        None,
    )
    .await?;

    let stub = TcpListener::bind("127.0.0.1:0").await?;
    let stub_port = stub.local_addr()?.port();
    drop(stub);

    let result = Client::new_vhost_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "fast",
        "client-fast",
        None,
        false,
        1,
        ProviderMeta::default(),
        None,
    )
    .await;
    let err = match result {
        Ok(_) => panic!("the 'fast' subdomain must be rejected"),
        Err(e) => e,
    };
    assert!(
        err.to_string()
            .contains("reserved for the fast link transfer service"),
        "unexpected error: {err}"
    );

    let ok = Client::new_vhost_provider(
        "127.0.0.1",
        stub_port,
        &format!("localhost:{CTRL}"),
        "app",
        "client-app",
        None,
        false,
        1,
        ProviderMeta::default(),
        None,
    )
    .await;
    assert!(
        ok.is_ok(),
        "an unrelated label must still register: {}",
        ok.err().map(|e| e.to_string()).unwrap_or_default()
    );
    Ok(())
}

// ─── T-FL-I5: a disabled server routes the fast host like any vhost ────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disabled_server_routes_the_fast_host_like_any_vhost() -> Result<()> {
    let _guard = SERIAL.lock().await;
    const CTRL: u16 = 18411;
    const HTTP: u16 = 18412;
    const HTTPS: u16 = 18413;

    let (cert_pem, key_pem) = self_signed_cert()?;
    let (cert_path, key_path) = write_pem_files(&cert_pem, &key_pem)?;
    let cfg = VhostConfig {
        base_domain: "bore.local".to_string(),
        mode: VhostModeCfg::Auto,
        http_port: HTTP,
        https_port: HTTPS,
        cert_file: Some(cert_path),
        key_file: Some(key_path),
        default_headers: Default::default(),
        default_response_headers: Default::default(),
        reservations: vec![],
    };

    wait_port(CTRL, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(CTRL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_vhost(cfg)?;
    let handle = tokio::spawn(server.listen());
    wait_bound(CTRL, handle).await;
    wait_port(HTTPS, true).await;

    let connector = fast_tls_connector(&cert_pem)?;
    let mut conn = https_connect(HTTPS, &connector).await?;
    let req = format!("GET /x HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n");
    conn.write_all(req.as_bytes()).await?;
    let mut resp = Vec::new();
    time::timeout(Duration::from_secs(5), conn.read_to_end(&mut resp)).await??;
    let resp = String::from_utf8_lossy(&resp);
    assert!(
        resp.starts_with("HTTP/1.1 502"),
        "a disabled server must fall through to the pre-feature 502, got: {resp}"
    );
    Ok(())
}

// ─── T-FL-I6: admin reports config and metrics without secrets ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_reports_fast_link_config_and_metrics_without_secrets() -> Result<()> {
    let _guard = SERIAL.lock().await;
    const CTRL: u16 = 18414;
    const HTTP: u16 = 18415;
    const HTTPS: u16 = 18416;
    const TOKEN: &str = "0123456789abcdef0123456789abcdef01234567";

    let (fast, cert_pem) = spawn_fast_server(
        CTRL,
        Topology::Dedicated {
            http_port: HTTP,
            https_port: HTTPS,
        },
        None,
        Some(TOKEN),
    )
    .await?;
    wait_port(HTTPS, true).await;
    let connector = fast_tls_connector(&cert_pem)?;

    // One small end-to-end transfer so the metrics have something to report.
    let payload = b"hello fast link admin metrics".to_vec();
    let uploader = https_connect(HTTPS, &connector).await?;
    let (mut u_read, mut u_write) = tokio::io::split(uploader);
    let head = format!(
        "PUT /note.txt HTTP/1.1\r\nHost: {HOST}\r\n{}Content-Length: {}\r\n\r\n",
        basic_auth_header(),
        payload.len()
    );
    u_write.write_all(head.as_bytes()).await?;
    let body = payload.clone();
    let writer = tokio::spawn(async move {
        u_write.write_all(&body).await?;
        u_write.flush().await?;
        anyhow::Ok(u_write)
    });
    let mut u_pending = Vec::new();
    let _u_head = read_head(&mut u_read, &mut u_pending).await?;
    let link_chunk = read_one_chunk(&mut u_read, &mut u_pending)
        .await?
        .expect("link chunk");
    let link = extract_link(&String::from_utf8_lossy(&link_chunk));
    let _waiting = read_one_chunk(&mut u_read, &mut u_pending).await?;

    let mut downloader = https_connect(HTTPS, &connector).await?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n",
        link_path(&link)
    );
    downloader.write_all(req.as_bytes()).await?;
    let mut d_pending = Vec::new();
    let d_head = read_head(&mut downloader, &mut d_pending).await?;
    assert!(d_head.starts_with("HTTP/1.1 200 OK"));
    let cl: usize = d_head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .expect("Content-Length")
        .trim()
        .parse()?;
    let downloaded = read_exact_body(&mut downloader, &mut d_pending, cl).await?;
    assert_eq!(downloaded, payload);
    writer.await??;
    let _rest = read_all_chunks(&mut u_read, &mut u_pending).await?;

    wait_slots_empty(&fast).await;

    // Admin API lives on the plain control port, gated by the bearer token.
    let config_body = admin_get(CTRL, "/admin/api/v1/config", TOKEN).await?;
    let metrics_body = admin_get(CTRL, "/admin/api/v1/metrics", TOKEN).await?;

    let config_json: serde_json::Value = serde_json::from_str(&config_body)?;
    assert_eq!(config_json["fast_link"]["host"], HOST);

    let metrics_json: serde_json::Value = serde_json::from_str(&metrics_body)?;
    assert_eq!(metrics_json["fast_link"]["completed_total"], 1);
    assert_eq!(metrics_json["fast_link"]["bytes_total"], payload.len());

    let token_b64 = base64::engine::general_purpose::STANDARD.encode(AUTH.as_bytes());
    for body in [&config_body, &metrics_body] {
        assert!(!body.contains(AUTH), "credential leaked in admin JSON");
        assert!(
            !body.contains(&token_b64),
            "base64 credential leaked in admin JSON"
        );
    }
    Ok(())
}

/// Plain HTTP GET against the admin API on the control port, bearer-token
/// gated. Returns the response body only (status already asserted `200`).
async fn admin_get(port: u16, path: &str, token: &str) -> Result<String> {
    let mut conn = TcpStream::connect(("127.0.0.1", port)).await?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: admin\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    conn.write_all(req.as_bytes()).await?;
    let mut resp = Vec::new();
    time::timeout(Duration::from_secs(5), conn.read_to_end(&mut resp)).await??;
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = resp
        .split_once("\r\n\r\n")
        .context("malformed admin response")?;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "admin request failed: {head}"
    );
    Ok(body.to_string())
}

// ─── T-FL-I7: the unified path honours --max-conns ─────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unified_path_honours_max_conns() -> Result<()> {
    let _guard = SERIAL.lock().await;
    const CTRL: u16 = 18417;
    const HTTP: u16 = 18418;

    let (fast, cert_pem) =
        spawn_fast_server(CTRL, Topology::Unified { http_port: HTTP }, Some(1), None).await?;
    let connector = fast_tls_connector(&cert_pem)?;

    // Open one upload that registers and starts waiting for a downloader:
    // its connection (and `--max-conns` permit) stays held.
    let mut uploader = https_connect(CTRL, &connector).await?;
    let head = format!(
        "PUT /f.bin HTTP/1.1\r\nHost: {HOST}\r\n{}Content-Length: 1000000\r\n\r\n",
        basic_auth_header()
    );
    uploader.write_all(head.as_bytes()).await?;
    let mut u_pending = Vec::new();
    let u_head = read_head(&mut uploader, &mut u_pending).await?;
    assert!(u_head.starts_with("HTTP/1.1 200 OK"), "got: {u_head}");
    let _link_chunk = read_one_chunk(&mut uploader, &mut u_pending).await?;
    let _waiting_chunk = read_one_chunk(&mut uploader, &mut u_pending).await?;

    // A second connection to the same fast host must be rejected outright.
    let mut second = https_connect(CTRL, &connector).await?;
    let req = format!("HEAD /x HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n");
    second.write_all(req.as_bytes()).await?;
    let mut resp = Vec::new();
    time::timeout(Duration::from_secs(5), second.read_to_end(&mut resp)).await??;
    let resp = String::from_utf8_lossy(&resp);
    assert!(
        resp.starts_with("HTTP/1.1 503"),
        "a second connection must be rejected while --max-conns is at capacity, got: {resp}"
    );

    drop(uploader);
    wait_slots_empty(&fast).await;
    Ok(())
}
