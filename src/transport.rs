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
    let tcp = connect_with_timeout(&endpoint.host, endpoint.port).await?;
    if !endpoint.tls {
        return Ok(ControlStream::Plain(tcp));
    }

    let connector = TlsConnector::from(Arc::new(client_config(insecure)?));
    let server_name = ServerName::try_from(endpoint.host.clone())
        .with_context(|| format!("invalid TLS server name: {}", endpoint.host))?;
    let tls = timeout(NETWORK_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .context("timed out during TLS handshake")?
        .context("TLS handshake failed")?;
    Ok(ControlStream::Tls(Box::new(tls)))
}

fn client_config(insecure: bool) -> Result<ClientConfig> {
    let builder = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("failed to configure TLS protocol versions")?;
    let config = if insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth()
    } else {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Ok(config)
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
}
