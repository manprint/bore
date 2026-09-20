//! Control-channel endpoint parsing and connection, including optional TLS.
//!
//! The client's `--to` value selects where and how to reach the server's control
//! port:
//!
//! - `https://host[:port]` — TLS (default port 443).
//! - `http://host[:port]` — plain TCP (default port 80).
//! - `host[:port]` — plain TCP (default the control port, [`CONTROL_PORT`]).
//!
//! TLS uses the `ring` crypto provider so static (musl) builds keep working.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::ring;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, ServerConfig,
    SignatureScheme,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::client::connect_with_timeout;
use crate::shared::{CONTROL_PORT, NETWORK_TIMEOUT};

/// Failure category for a control connection attempt.  Link supervision uses
/// this typed marker instead of inspecting error strings, so a certificate
/// failure cannot accidentally enter an infinite reconnect loop.
#[derive(Debug)]
pub(crate) enum ConnectFailure {
    /// The TCP connection could not be established or was interrupted.
    Transport,
    /// The TLS handshake timed out or was interrupted by the peer/network.
    TlsTransient,
    /// TLS name verification, certificate parsing or handshake failed.
    Tls,
}

impl std::fmt::Display for ConnectFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport => f.write_str("control transport failure"),
            Self::TlsTransient => f.write_str("transient control TLS transport failure"),
            Self::Tls => f.write_str("control TLS verification failure"),
        }
    }
}

impl std::error::Error for ConnectFailure {}

/// A control connection: either plain TCP or a TLS stream over TCP.
pub enum ControlStream {
    /// Plain TCP.
    Plain(TcpStream),
    /// TLS over TCP (boxed: the TLS stream is much larger than a bare socket).
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for ControlStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ControlStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            ControlStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ControlStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            ControlStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            ControlStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ControlStream::Plain(s) => Pin::new(s).poll_flush(cx),
            ControlStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ControlStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            ControlStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            ControlStream::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            ControlStream::Tls(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            ControlStream::Plain(s) => s.is_write_vectored(),
            ControlStream::Tls(s) => s.is_write_vectored(),
        }
    }
}

/// A parsed control endpoint derived from a `--to` value.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Host to connect to.
    pub host: String,
    /// Control port to connect to.
    pub port: u16,
    /// Whether the connection must be wrapped in TLS.
    pub tls: bool,
}

/// A fully constructed client TLS configuration owned by one scoped client.
///
/// Link sessions use this type to add a private CA while retaining rustls'
/// normal hostname verification.  Keeping the `Arc<ClientConfig>` behind a
/// crate-visible alias makes it possible for every carrier/redial dial to use
/// the exact same trust policy without introducing global mutable state.
pub(crate) type ClientTlsConfig = Arc<ClientConfig>;

impl Endpoint {
    /// Parse a `--to` value, honouring an optional `http://` / `https://` scheme.
    pub fn parse(to: &str) -> Self {
        let (tls, default_port, rest) = if let Some(rest) = to.strip_prefix("https://") {
            (true, 443, rest)
        } else if let Some(rest) = to.strip_prefix("http://") {
            (false, 80, rest)
        } else {
            (false, CONTROL_PORT, to)
        };
        // Drop any trailing path, e.g. "https://bore.tld/".
        let authority = rest.split('/').next().unwrap_or(rest);

        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => match port.parse::<u16>() {
                Ok(port) => (host, port),
                Err(_) => (authority, default_port),
            },
            _ => (authority, default_port),
        };
        Endpoint {
            host: host.to_string(),
            port,
            tls,
        }
    }
}

/// Open a control connection to the endpoint.
///
/// `insecure` only applies to TLS endpoints: when set, the server certificate is
/// not verified (useful for self-signed certificates on a private deployment).
pub async fn connect(endpoint: &Endpoint, insecure: bool) -> Result<ControlStream> {
    connect_with_config(endpoint, insecure, None).await
}

/// Open a control connection, optionally using a caller-owned TLS config.
///
/// `tls_config` is consulted only for TLS endpoints.  A supplied config is
/// always a verified config; `insecure` is retained for legacy callers and is
/// ignored when a scoped config is present.
pub(crate) async fn connect_with_config(
    endpoint: &Endpoint,
    insecure: bool,
    tls_config: Option<ClientTlsConfig>,
) -> Result<ControlStream> {
    let tcp = connect_with_timeout(&endpoint.host, endpoint.port)
        .await
        .map_err(|error| anyhow::Error::new(ConnectFailure::Transport).context(error))?;
    if !endpoint.tls {
        return Ok(ControlStream::Plain(tcp));
    }

    let config = tls_config
        .map(Ok)
        .unwrap_or_else(|| client_config(insecure).map(Arc::new))
        .map_err(|error| anyhow::Error::new(ConnectFailure::Tls).context(error))?;
    let connector = TlsConnector::from(config);
    let server_name = ServerName::try_from(endpoint.host.clone())
        .with_context(|| format!("invalid TLS server name: {}", endpoint.host))
        .map_err(|error| anyhow::Error::new(ConnectFailure::Tls).context(error))?;
    let tls = timeout(NETWORK_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .map_err(|_| {
            anyhow::Error::new(ConnectFailure::TlsTransient)
                .context("timed out during TLS handshake")
        })?
        .map_err(|error| {
            let failure = if is_transient_tls_io(&error) {
                ConnectFailure::TlsTransient
            } else {
                ConnectFailure::Tls
            };
            anyhow::Error::new(failure)
                .context(error)
                .context("TLS handshake failed")
        })?;
    Ok(ControlStream::Tls(Box::new(tls)))
}

/// Classify an error returned while rustls is driving the TCP socket.  Rustls
/// reports certificate/name/protocol failures as `InvalidData`; socket
/// shutdowns and timeouts retain their I/O kind and are safe to retry.
fn is_transient_tls_io(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::Interrupted
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
    )
}

fn client_config(insecure: bool) -> Result<ClientConfig> {
    let builder = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("failed to configure TLS protocol versions")?;
    let mut config = if insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth()
    } else {
        let roots = default_root_store(None)?;
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    // Offer `bore` via ALPN so a server demuxing SSH on the control port
    // (`--ssh-gateway`) can classify this connection at the ClientHello
    // instead of waiting on the post-TLS first-byte peek — a slow first
    // flight can otherwise exceed the peek timeout and be misrouted. Wire
    // compatible: a rustls server with no `alpn_protocols` configured (every
    // bore server, old and new) ignores the offer entirely.
    config.alpn_protocols = vec![b"bore".to_vec()];
    Ok(config)
}

/// Build a verified client configuration with optional additional PEM roots.
///
/// The system/webpki roots remain present.  Extra roots are additive and do
/// not disable hostname verification.  An explicitly supplied empty or
/// malformed PEM is rejected before any network dial.
pub(crate) fn client_config_with_extra_roots(ca_pem: Option<&[u8]>) -> Result<ClientTlsConfig> {
    let builder = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("failed to configure TLS protocol versions")?;
    let roots = default_root_store(ca_pem)?;
    let mut config = builder.with_root_certificates(roots).with_no_client_auth();
    config.alpn_protocols = vec![b"bore".to_vec()];
    Ok(Arc::new(config))
}

fn default_root_store(ca_pem: Option<&[u8]>) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = ca_pem {
        anyhow::ensure!(!pem.is_empty(), "CA certificate file is empty");
        let mut found = false;
        for certificate in CertificateDer::pem_slice_iter(pem) {
            let certificate = certificate.context("failed to parse CA certificate PEM")?;
            roots
                .add(certificate)
                .context("failed to add CA certificate to trust store")?;
            found = true;
        }
        anyhow::ensure!(found, "CA certificate PEM contains no certificates");
    }
    Ok(roots)
}

/// Build a `TlsConnector` for connecting to a vhost provider's local HTTPS
/// backend (`--backend-tls`).
///
/// Certificate verification is skipped (accept-any, reusing [`NoVerifier`]) so a
/// self-signed local backend works with no extra configuration — the target is
/// the provider's own service, reached through the already-authenticated tunnel.
/// Only `http/1.1` is offered via ALPN: bore relays the backend leg as HTTP/1.x
/// (it parses and rewrites HTTP/1.x request/response heads), so negotiating
/// HTTP/2 would corrupt that path; the `bore` control-plane ALPN is deliberately
/// NOT offered to a foreign server.
pub(crate) fn insecure_tls_connector() -> Result<TlsConnector> {
    let mut config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("failed to configure backend TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Parse a backend TLS SNI/server name into an owned (`'static`) rustls
/// `ServerName`. Returns an error (never panics) on an invalid name.
pub(crate) fn backend_server_name(name: &str) -> Result<ServerName<'static>> {
    ServerName::try_from(name.to_owned())
        .with_context(|| format!("invalid backend TLS SNI: {name}"))
}

/// Build a TLS acceptor for the server from PEM-encoded certificate and key.
pub fn server_tls_from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<TlsAcceptor> {
    let certs = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse certificate PEM")?;
    anyhow::ensure!(!certs.is_empty(), "no certificates found in cert file");
    let key = PrivateKeyDer::from_pem_slice(key_pem).context("failed to parse private key PEM")?;

    let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("failed to configure TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("invalid certificate or key")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Build a TLS acceptor by reading the certificate and key from files.
pub fn load_server_tls(cert_file: &str, key_file: &str) -> Result<TlsAcceptor> {
    let cert_pem =
        std::fs::read(cert_file).with_context(|| format!("failed to read {cert_file}"))?;
    let key_pem = std::fs::read(key_file).with_context(|| format!("failed to read {key_file}"))?;
    server_tls_from_pem(&cert_pem, &key_pem)
}

/// A certificate verifier that accepts any server certificate (`--insecure`).
#[derive(Debug)]
struct NoVerifier;

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_only_uses_default_port() {
        let endpoint = Endpoint::parse("bore.tld");
        assert_eq!(endpoint.host, "bore.tld");
        assert_eq!(endpoint.port, CONTROL_PORT);
        assert!(!endpoint.tls);
    }

    #[test]
    fn parse_host_port() {
        let endpoint = Endpoint::parse("bore.tld:1000");
        assert_eq!(endpoint.host, "bore.tld");
        assert_eq!(endpoint.port, 1000);
        assert!(!endpoint.tls);
    }

    #[test]
    fn extra_root_loader_rejects_empty_or_non_pem_input() {
        assert!(client_config_with_extra_roots(Some(&[])).is_err());
        assert!(client_config_with_extra_roots(Some(b"not-a-certificate")).is_err());
        assert!(client_config_with_extra_roots(None).is_ok());
    }

    #[test]
    fn parse_non_numeric_port_is_treated_as_host() {
        let endpoint = Endpoint::parse("bore.tld:nope");
        assert_eq!(endpoint.host, "bore.tld:nope");
        assert_eq!(endpoint.port, CONTROL_PORT);
    }

    #[test]
    fn parse_https_defaults_to_443_and_tls() {
        let endpoint = Endpoint::parse("https://bore.tld");
        assert_eq!(endpoint.host, "bore.tld");
        assert_eq!(endpoint.port, 443);
        assert!(endpoint.tls);
    }

    #[test]
    fn parse_http_defaults_to_80_plain() {
        let endpoint = Endpoint::parse("http://bore.tld");
        assert_eq!(endpoint.host, "bore.tld");
        assert_eq!(endpoint.port, 80);
        assert!(!endpoint.tls);
    }

    #[test]
    fn parse_https_with_explicit_port() {
        let endpoint = Endpoint::parse("https://bore.tld:8443");
        assert_eq!(endpoint.host, "bore.tld");
        assert_eq!(endpoint.port, 8443);
        assert!(endpoint.tls);
    }

    #[test]
    fn parse_https_strips_trailing_path() {
        let endpoint = Endpoint::parse("https://bore.tld/");
        assert_eq!(endpoint.host, "bore.tld");
        assert_eq!(endpoint.port, 443);
    }

    #[test]
    fn insecure_tls_connector_builds() {
        // The backend TLS connector must build with the accept-any verifier and
        // an http/1.1-only ALPN offer.
        assert!(insecure_tls_connector().is_ok());
    }

    #[test]
    fn tls_io_classification_keeps_socket_failures_retryable() {
        for kind in [
            io::ErrorKind::TimedOut,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::BrokenPipe,
        ] {
            assert!(is_transient_tls_io(&io::Error::from(kind)));
        }
        assert!(!is_transient_tls_io(&io::Error::new(
            io::ErrorKind::InvalidData,
            "certificate rejected",
        )));
    }

    #[test]
    fn backend_server_name_valid() {
        assert!(backend_server_name("localhost").is_ok());
        assert!(backend_server_name("app.internal").is_ok());
    }

    #[test]
    fn backend_server_name_rejects_garbage() {
        // An invalid SNI must return an error, never panic.
        assert!(backend_server_name("").is_err());
        assert!(backend_server_name("not a host").is_err());
    }
}
