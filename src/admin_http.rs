//! Minimal HTTP/1.1 handler for the admin status page, served on the control
//! port (plain or TLS, matching how the server was started).
//!
//! It speaks just enough HTTP to serve one request per connection: the embedded
//! SPA shell at `/admin/status`, static assets at `/admin/ui/*`, token-guarded
//! JSON API at `/admin/api/v1/*`, and the legacy `/admin/status/data` snapshot.
//! No framework, no persistence — the data is a live read of [`AdminRegistry`].

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::admin::{AdminRegistry, EntryView};
use crate::basicauth::constant_time_eq;
use crate::shared::NETWORK_TIMEOUT;

// Include the auto-generated admin assets table from build.rs.
// Generated code is pub static ADMIN_ASSETS.
include!(concat!(env!("OUT_DIR"), "/admin_assets.rs"));

/// Cap on the request-head bytes read.
const MAX_HEAD: usize = 8 * 1024;

/// Server-level facts shown at the top of the admin page.
#[derive(Serialize, Clone, Copy)]
pub struct ServerStatus {
    /// The control port the server listens on.
    pub control_port: u16,
    /// Whether the control connection is TLS (so the page is served over https).
    pub tls: bool,
    /// Whether UDP direct-path brokering is enabled.
    pub udp: bool,
}

#[derive(Serialize)]
struct StatusView {
    server: ServerStatus,
    tunnels: Vec<EntryView>,
}

/// Whether the first byte of a control connection starts an HTTP request line, so
/// it should be served by the admin page rather than the bore protocol (whose
/// first yamux byte is `0x00`). Covers GET/POST/PUT/HEAD/DELETE/OPTIONS/PATCH/
/// TRACE/CONNECT.
pub fn is_http_first_byte(b: u8) -> bool {
    matches!(b, b'G' | b'P' | b'H' | b'D' | b'O' | b'T' | b'C')
}

/// Serve a single admin HTTP request on `stream`, then let the caller close it.
pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    registry: &AdminRegistry,
    token: &str,
    server_status: ServerStatus,
    control_hsts: Option<&str>,
    server: Option<&crate::server::Server>,
) -> Result<()> {
    let head = match read_head(&mut stream).await? {
        Some(h) => h,
        None => return Ok(()),
    };
    let (method, path) = request_line(&head);
    if method != "GET" {
        return respond(
            &mut stream,
            405,
            "text/plain",
            b"method not allowed",
            control_hsts,
        )
        .await;
    }

    // Route on the path without its query string.
    let path_only = path.split('?').next().unwrap_or(&path);

    // CSP header for all responses
    let csp = "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'";

    // New API endpoints (D1, D10).
    if path_only.starts_with("/admin/api/v1/") {
        if !authorized(&head, token) {
            return respond_with_csp(
                &mut stream,
                401,
                "text/plain",
                b"unauthorized",
                control_hsts,
                csp,
            )
            .await;
        }

        // Dispatch to the appropriate builder and return JSON.
        let Some(srv) = server else {
            return respond_with_csp(
                &mut stream,
                404,
                "text/plain",
                b"not found",
                control_hsts,
                csp,
            )
            .await;
        };

        let body = match path_only {
            "/admin/api/v1/summary" => serde_json::to_vec(&crate::admin_api::summary(srv))?,
            "/admin/api/v1/tunnels" => serde_json::to_vec(&crate::admin_api::tunnels(srv))?,
            "/admin/api/v1/secret" => serde_json::to_vec(&crate::admin_api::secret(srv))?,
            "/admin/api/v1/vhost" => serde_json::to_vec(&crate::admin_api::vhost(srv))?,
            "/admin/api/v1/vpn" => {
                #[cfg(feature = "vpn")]
                {
                    serde_json::to_vec(&crate::admin_api::vpn(srv))?
                }
                #[cfg(not(feature = "vpn"))]
                {
                    serde_json::to_vec(&serde_json::json!({ "links": [] }))?
                }
            }
            "/admin/api/v1/certs" => serde_json::to_vec(&crate::admin_api::certs(srv))?,
            "/admin/api/v1/config" => serde_json::to_vec(&crate::admin_api::config(srv))?,
            "/admin/api/v1/metrics" => serde_json::to_vec(&crate::admin_api::metrics(srv))?,
            _ => {
                return respond_with_csp(
                    &mut stream,
                    404,
                    "text/plain",
                    b"not found",
                    control_hsts,
                    csp,
                )
                .await;
            }
        };
        return respond_with_csp(
            &mut stream,
            200,
            "application/json",
            &body,
            control_hsts,
            csp,
        )
        .await;
    }

    // Asset serving: /admin/ui/<path> → exact key lookup in ADMIN_ASSETS
    if path_only.starts_with("/admin/ui/") {
        if let Some((_, bytes, content_type)) =
            ADMIN_ASSETS.iter().find(|(url, _, _)| *url == path_only)
        {
            return respond_with_csp(&mut stream, 200, content_type, bytes, control_hsts, csp)
                .await;
        }
        return respond_with_csp(
            &mut stream,
            404,
            "text/plain",
            b"not found",
            control_hsts,
            csp,
        )
        .await;
    }

    // SPA shell: /admin/status, /admin/status/, /admin, /admin/ → index.html
    if matches!(
        path_only,
        "/admin/status" | "/admin/status/" | "/admin" | "/admin/"
    ) {
        if let Some((_, bytes, content_type)) = ADMIN_ASSETS
            .iter()
            .find(|(url, _, _)| *url == "/admin/ui/index.html")
        {
            return respond_with_csp(&mut stream, 200, content_type, bytes, control_hsts, csp)
                .await;
        }
        return respond_with_csp(
            &mut stream,
            404,
            "text/plain",
            b"not found",
            control_hsts,
            csp,
        )
        .await;
    }

    // Legacy /admin/status/data endpoint (D2: must stay byte-identical)
    if path_only == "/admin/status/data" {
        if !authorized(&head, token) {
            return respond_with_csp(
                &mut stream,
                401,
                "text/plain",
                b"unauthorized",
                control_hsts,
                csp,
            )
            .await;
        }
        let view = StatusView {
            server: server_status,
            tunnels: registry.snapshot(),
        };
        let body = serde_json::to_vec(&view).context("serialize admin status")?;
        return respond_with_csp(
            &mut stream,
            200,
            "application/json",
            &body,
            control_hsts,
            csp,
        )
        .await;
    }

    respond_with_csp(
        &mut stream,
        404,
        "text/plain",
        b"not found",
        control_hsts,
        csp,
    )
    .await
}

/// Write the minimal control-port 404 response, with optional HSTS.
pub(crate) async fn respond_not_found<S: AsyncWrite + Unpin>(
    stream: &mut S,
    control_hsts: Option<&str>,
) -> Result<()> {
    respond(stream, 404, "text/plain", b"", control_hsts).await
}

/// Read the HTTP request head (up to `\r\n\r\n`, capped, time-bounded).
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Option<Vec<u8>>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let read = async {
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_HEAD {
                break;
            }
        }
        Ok::<(), std::io::Error>(())
    };
    match timeout(NETWORK_TIMEOUT, read).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => {} // timeout: use whatever arrived
    }
    if buf.is_empty() {
        Ok(None)
    } else {
        Ok(Some(buf))
    }
}

/// Extract `(method, path)` from the request line.
fn request_line(head: &[u8]) -> (String, String) {
    let line = head
        .split(|&b| b == b'\r' || b == b'\n')
        .next()
        .unwrap_or(&[]);
    let text = String::from_utf8_lossy(line);
    let mut parts = text.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    (method, path)
}

/// Whether the request carries the admin token via `Authorization: Bearer <t>` or
/// `X-Admin-Token: <t>`, compared in constant time.
fn authorized(head: &[u8], token: &str) -> bool {
    let text = String::from_utf8_lossy(head);
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            let bearer = value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "));
            if let Some(t) = bearer {
                if constant_time_eq(t.trim().as_bytes(), token.as_bytes()) {
                    return true;
                }
            }
        } else if name.eq_ignore_ascii_case("x-admin-token")
            && constant_time_eq(value.as_bytes(), token.as_bytes())
        {
            return true;
        }
    }
    false
}

/// Write a complete HTTP/1.1 response with CSP header and close the connection.
async fn respond_with_csp<S: AsyncWrite + Unpin>(
    stream: &mut S,
    code: u16,
    content_type: &str,
    body: &[u8],
    control_hsts: Option<&str>,
    csp: &str,
) -> Result<()> {
    let reason = match code {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let hsts = control_hsts
        .map(|value| format!("Strict-Transport-Security: {value}\r\n"))
        .unwrap_or_default();
    let header = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Content-Security-Policy: {csp}\r\n\
         {hsts}\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    // Shut the stream down cleanly so a TLS peer gets a `close_notify` (and a
    // plain peer a prompt FIN) instead of an "unexpected EOF".
    let _ = stream.shutdown().await;
    Ok(())
}

/// Write a complete HTTP/1.1 response and close the connection.
async fn respond<S: AsyncWrite + Unpin>(
    stream: &mut S,
    code: u16,
    content_type: &str,
    body: &[u8],
    control_hsts: Option<&str>,
) -> Result<()> {
    let reason = match code {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let hsts = control_hsts
        .map(|value| format!("Strict-Transport-Security: {value}\r\n"))
        .unwrap_or_default();
    let header = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         {hsts}\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    // Shut the stream down cleanly so a TLS peer gets a `close_notify` (and a
    // plain peer a prompt FIN) instead of an "unexpected EOF".
    let _ = stream.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_byte_discriminates_http_from_yamux() {
        assert!(is_http_first_byte(b'G'));
        assert!(is_http_first_byte(b'P'));
        assert!(!is_http_first_byte(0x00)); // yamux version byte
        assert!(!is_http_first_byte(0x16)); // TLS handshake
    }

    #[test]
    fn parses_request_line() {
        let (m, p) = request_line(b"GET /admin/status/data?x=1 HTTP/1.1\r\nHost: a\r\n\r\n");
        assert_eq!(m, "GET");
        assert_eq!(p, "/admin/status/data?x=1");
    }

    #[test]
    fn authorized_accepts_bearer_and_header() {
        let token = "0123456789abcdef0123456789abcdef";
        let bearer = format!("GET /x HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n");
        assert!(authorized(bearer.as_bytes(), token));
        let xhdr = format!("GET /x HTTP/1.1\r\nX-Admin-Token: {token}\r\n\r\n");
        assert!(authorized(xhdr.as_bytes(), token));
        assert!(!authorized(
            b"GET /x HTTP/1.1\r\nAuthorization: Bearer wrong\r\n\r\n",
            token
        ));
        assert!(!authorized(b"GET /x HTTP/1.1\r\n\r\n", token));
    }
}
