//! TLS certificate inspection: parse DER certs to extract expiry, subject, SANs.
//!
//! Used by the admin dashboard certs section (plan §2). Parsing is best-effort
//! and never panics: a malformed cert yields a [`CertView`] with `error` set.

use crate::admin_views::CertView;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::prelude::*;

/// Cache entry: (file mtime, parsed view).
type CacheEntry = (SystemTime, CertView);

/// mtime-keyed cache of parsed certificates, so the certs endpoint does not
/// re-parse on every poll. Keyed by the file path.
static CERT_CACHE: LazyLock<Mutex<HashMap<String, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

const SECS_PER_DAY: i64 = 86_400;

/// Parse a DER certificate and extract subject CN, SANs, validity window and
/// the days remaining until expiry. On parse error returns a [`CertView`] with
/// `error` set (never panics).
pub fn inspect(der: &[u8], label: &str, path: Option<&Path>) -> CertView {
    // Serve from cache if the file is unchanged since last parse.
    if let Some(mtime) = path.and_then(file_mtime) {
        if let Ok(cache) = CERT_CACHE.lock() {
            if let Some(key) = path.map(cache_key) {
                if let Some((cached_mtime, cached_view)) = cache.get(&key) {
                    if *cached_mtime == mtime {
                        return cached_view.clone();
                    }
                }
            }
        }
    }

    let view = match X509Certificate::from_der(der) {
        Ok((_, cert)) => build_view(&cert, label, path),
        Err(e) => CertView {
            label: label.to_string(),
            path: path.map(path_string),
            subject: None,
            sans: vec![],
            not_before: None,
            not_after: None,
            days_remaining: 0,
            expiring: true,
            error: Some(format!("cert parse error: {e}")),
        },
    };

    // Cache successful and failed parses alike (keyed by mtime).
    if let (Some(p), Some(mtime)) = (path, path.and_then(file_mtime)) {
        if let Ok(mut cache) = CERT_CACHE.lock() {
            cache.insert(cache_key(p), (mtime, view.clone()));
        }
    }

    view
}

/// Build a [`CertView`] from a parsed certificate.
fn build_view(cert: &X509Certificate, label: &str, path: Option<&Path>) -> CertView {
    let subject = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(str::to_string);

    // Extract DNS SANs from the SubjectAlternativeName extension.
    let mut sans = Vec::new();
    for ext in cert.extensions() {
        if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
            for gn in &san.general_names {
                if let GeneralName::DNSName(name) = gn {
                    sans.push(name.to_string());
                }
            }
        }
    }

    let not_before = Some(rfc3339(&cert.validity().not_before.to_datetime()));
    let not_after = Some(rfc3339(&cert.validity().not_after.to_datetime()));

    // days_remaining = (expiry - now) / 86400, signed (negative = expired).
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days_remaining =
        (cert.validity().not_after.timestamp() - now_unix).div_euclid(SECS_PER_DAY);

    CertView {
        label: label.to_string(),
        path: path.map(path_string),
        subject,
        sans,
        not_before,
        not_after,
        days_remaining,
        expiring: days_remaining <= 30,
        error: None,
    }
}

/// Format an `OffsetDateTime` as an RFC3339 / ISO-8601 UTC string without
/// pulling in a formatting dependency. The x509 datetimes are always UTC.
/// `::time` is fully qualified: `x509_parser::prelude::*` shadows the bare
/// `time` name with its own private module.
fn rfc3339(dt: &::time::OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

fn cache_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_rustls::rustls::pki_types::{pem::PemObject, CertificateDer};

    /// Extract the first cert's DER from a PEM byte slice, reusing the project's
    /// PEM parser (mirrors `transport::server_tls_from_pem`).
    fn pem_to_der(pem: &[u8]) -> Vec<u8> {
        CertificateDer::pem_slice_iter(pem)
            .next()
            .expect("no cert in PEM")
            .expect("invalid cert PEM")
            .as_ref()
            .to_vec()
    }

    #[test]
    fn t_cert_parse_fixture() {
        let der = pem_to_der(include_bytes!("../tests/fixtures/test_cert.pem"));
        let view = inspect(&der, "control", None);
        assert!(view.error.is_none(), "parse failed: {:?}", view.error);
        assert_eq!(view.subject.as_deref(), Some("valid.bore.test"));
        assert!(
            view.sans.contains(&"valid.bore.test".to_string())
                && view.sans.contains(&"www.bore.test".to_string()),
            "SANs not extracted: {:?}",
            view.sans
        );
        assert!(
            view.not_after.as_deref().unwrap().ends_with('Z'),
            "not_after must be RFC3339: {:?}",
            view.not_after
        );
        assert!(
            view.days_remaining > 0 && !view.expiring,
            "far-future cert must not be expiring (days={})",
            view.days_remaining
        );
    }

    #[test]
    fn t_cert_expired_fixture() {
        let der = pem_to_der(include_bytes!("../tests/fixtures/test_cert_expired.pem"));
        let view = inspect(&der, "control", None);
        assert!(view.error.is_none(), "parse failed: {:?}", view.error);
        assert_eq!(view.subject.as_deref(), Some("expired.bore.test"));
        assert!(
            view.days_remaining < 0 && view.expiring,
            "expired cert must report negative days + expiring (days={})",
            view.days_remaining
        );
    }

    #[test]
    fn t_cert_parse_error_graceful() {
        let view = inspect(&[0xFF, 0xFE, 0xFD], "bad", None);
        assert!(view.error.is_some());
        assert!(view.subject.is_none());
    }
}
