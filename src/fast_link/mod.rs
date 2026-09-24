//! Fast link transfer: a single-shot, unauthenticated-download HTTP(S) upload/
//! download relay served on its own vhost subdomain.
//!
//! An operator enables it with `--fast-link-transfer` plus a dedicated vhost
//! label (so the existing wildcard certificate covers it) and an HTTP Basic
//! credential that gates uploads only; the generated download link itself
//! carries no further secret beyond its own unguessable id. See
//! `docs/plans/004_plan-FastLinkTransfer/` for the full design.
//!
//! This module is pure and self-contained: it has no server wiring yet (that
//! lands in a later phase). [`request`] parses the HTTP head, target and
//! framing; [`response`] builds the fixed response byte sequences.

mod framing;
mod pump;
mod request;
mod response;

use std::time::Duration;

use ring::rand::SecureRandom;

use crate::basicauth::BasicAuth;

pub use framing::{BodyFramer, FramingError, Progress, MAX_CHUNK_LINE};
pub use request::{
    expects_continue, head_len, host_matches, parse_download_target, parse_head,
    parse_upload_target, preview_verdict, upload_framing, Framing, HeadError, Preview, RequestHead,
};
pub use response::{
    abort_close, chunk, download_head, linger_close, simple_response, upload_head, usage_text,
    CONTINUE, LAST_CHUNK, PREVIEW_HTML,
};

/// Length, in characters, of a generated download id.
pub const FAST_LINK_ID_LEN: usize = 16;

/// Alphabet used by [`generate_id`] and validated by
/// [`request::parse_download_target`].
pub const FAST_LINK_ID_ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Default number of seconds a slot waits for a downloader before expiring.
pub const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 3600;

/// Largest accepted `--fast-link-transfer-wait-timeout` value, in seconds (7 days).
pub const MAX_WAIT_TIMEOUT_SECS: u64 = 604_800;

/// Default number of concurrent uploads a fast link server admits.
pub const DEFAULT_MAX_ACTIVE: usize = 32;

/// Largest accepted `--fast-link-transfer-max-active` value.
pub const MAX_MAX_ACTIVE: usize = 4096;

/// Number of upload bytes retained in RAM so a dropped download can be
/// re-armed without re-reading from the uploader.
pub const REPLAY_WINDOW_BYTES: usize = 4 * 1024 * 1024;

/// A read/write that makes no progress for this long is considered stalled.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(600);

/// How long the waiting upload task blocks for a handoff it has just been
/// told is `InFlight` before treating it as gone.
pub const HANDOFF_RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on the bytes read while looking for a complete request head.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// Cap on the number of header lines accepted by [`request::parse_head`].
pub const MAX_HEADERS: usize = 64;

/// Bound on the best-effort drain-and-close performed by [`response::linger_close`]
/// and [`response::abort_close`].
pub const LINGER_TIMEOUT: Duration = Duration::from_secs(2);

/// Bound on the bytes discarded by [`response::linger_close`].
pub const LINGER_MAX_BYTES: usize = 1024 * 1024;

/// Filename used for an upload target of `/` (no basename given).
pub const DEFAULT_UPLOAD_FILENAME: &str = "upload.bin";

/// Raw `--fast-link-transfer-*` CLI/env inputs, before validation.
pub struct FastLinkServerArgs {
    /// Whether `--fast-link-transfer` was passed.
    pub enabled: bool,
    /// `--fast-link-transfer-vhost` (`BORE_FAST_LINK_TRANSFER_VHOST`).
    pub vhost: Option<String>,
    /// `--fast-link-transfer-auth` (`BORE_FAST_LINK_TRANSFER_AUTH`), raw `USER:PASS`.
    pub auth: Option<String>,
    /// `--fast-link-transfer-wait-timeout`, in seconds.
    pub wait_timeout_secs: u64,
    /// `--fast-link-transfer-max-active`.
    pub max_active: usize,
}

/// Validated fast link transfer configuration.
///
/// Deliberately does not derive `Debug`: the credential must never be logged.
#[derive(Clone)]
pub struct FastLinkConfig {
    /// The normalized, bare vhost host name (e.g. `fast.bore.tld`).
    pub host: String,
    /// The single subdomain label under the vhost base domain (e.g. `fast`).
    pub label: String,
    /// The upload-only HTTP Basic credential.
    pub auth: BasicAuth,
    /// How long a slot waits for a downloader before it expires.
    pub wait_timeout: Duration,
    /// Maximum number of concurrent uploads.
    pub max_active: usize,
}

/// Outcome of [`resolve_server_config`].
pub struct FastLinkResolution {
    /// `Some` when the feature is enabled and every input validated.
    pub config: Option<FastLinkConfig>,
    /// Names of `--fast-link-transfer-*` flags that were set but are ignored
    /// because the feature itself is disabled.
    pub ignored: Vec<&'static str>,
}

const FLAG_VHOST: &str = "--fast-link-transfer-vhost";
const FLAG_AUTH: &str = "--fast-link-transfer-auth";
const FLAG_WAIT_TIMEOUT: &str = "--fast-link-transfer-wait-timeout";
const FLAG_MAX_ACTIVE: &str = "--fast-link-transfer-max-active";

/// Validate `--fast-link-transfer-*` inputs and, when enabled, build a
/// [`FastLinkConfig`].
///
/// When disabled, this never errors: it only reports which otherwise-set
/// flags are being ignored, so an operator does not silently misconfigure the
/// feature. When enabled, every input is validated in the order documented in
/// the plan and the first violation is returned as an [`anyhow::Error`] whose
/// message never echoes the configured credential.
pub fn resolve_server_config(
    args: &FastLinkServerArgs,
    vhost_base_domain: Option<&str>,
) -> anyhow::Result<FastLinkResolution> {
    if !args.enabled {
        let mut ignored = Vec::new();
        if args.vhost.is_some() {
            ignored.push(FLAG_VHOST);
        }
        if args.auth.is_some() {
            ignored.push(FLAG_AUTH);
        }
        if args.wait_timeout_secs != DEFAULT_WAIT_TIMEOUT_SECS {
            ignored.push(FLAG_WAIT_TIMEOUT);
        }
        if args.max_active != DEFAULT_MAX_ACTIVE {
            ignored.push(FLAG_MAX_ACTIVE);
        }
        return Ok(FastLinkResolution {
            config: None,
            ignored,
        });
    }

    let base = match vhost_base_domain {
        Some(base) if !base.is_empty() => base,
        _ => {
            anyhow::bail!(
                "--fast-link-transfer requires a vhost base domain (--vhost-config or --vhost-base-domain)"
            )
        }
    };

    let Some(raw_vhost) = args.vhost.as_deref() else {
        anyhow::bail!(
            "--fast-link-transfer requires --fast-link-transfer-vhost (BORE_FAST_LINK_TRANSFER_VHOST)"
        )
    };

    let host = raw_vhost.trim().to_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host).to_string();
    if host.is_empty() || host.contains(':') || host.contains('/') || host.contains(' ') {
        anyhow::bail!(
            "--fast-link-transfer-vhost must be a bare host name such as fast.<base domain>"
        );
    }

    let Some(label) = crate::vhost::extract_subdomain(&host, base) else {
        anyhow::bail!(
            "--fast-link-transfer-vhost '{host}' must be exactly one label under the vhost base domain '{base}' (for example fast.{base}) so the existing wildcard certificate covers it"
        )
    };

    let Some(raw_auth) = args.auth.as_deref() else {
        anyhow::bail!(
            "--fast-link-transfer requires --fast-link-transfer-auth USER:PASS (BORE_FAST_LINK_TRANSFER_AUTH)"
        )
    };
    let Some((user, pass)) = raw_auth.split_once(':') else {
        anyhow::bail!(
            "--fast-link-transfer-auth must be USER:PASS with a non-empty user and password"
        )
    };
    if user.is_empty() || pass.is_empty() {
        anyhow::bail!(
            "--fast-link-transfer-auth must be USER:PASS with a non-empty user and password"
        );
    }
    let auth = BasicAuth::parse(raw_auth).ok_or_else(|| {
        anyhow::anyhow!(
            "--fast-link-transfer-auth must be USER:PASS with a non-empty user and password"
        )
    })?;

    if !(1..=MAX_WAIT_TIMEOUT_SECS).contains(&args.wait_timeout_secs) {
        anyhow::bail!(
            "--fast-link-transfer-wait-timeout must be between 1 and {MAX_WAIT_TIMEOUT_SECS} seconds"
        );
    }
    if !(1..=MAX_MAX_ACTIVE).contains(&args.max_active) {
        anyhow::bail!("--fast-link-transfer-max-active must be between 1 and {MAX_MAX_ACTIVE}");
    }

    Ok(FastLinkResolution {
        config: Some(FastLinkConfig {
            host,
            label,
            auth,
            wait_timeout: Duration::from_secs(args.wait_timeout_secs),
            max_active: args.max_active,
        }),
        ignored: Vec::new(),
    })
}

/// Generate a random, URL-safe download id: [`FAST_LINK_ID_LEN`] characters
/// drawn from [`FAST_LINK_ID_ALPHABET`] via rejection sampling (bytes `>= 252`
/// are discarded to keep the distribution uniform over the 36-symbol alphabet).
pub fn generate_id() -> anyhow::Result<String> {
    let random = ring::rand::SystemRandom::new();
    let mut output = String::with_capacity(FAST_LINK_ID_LEN);
    let mut byte = [0u8; 1];
    while output.len() < FAST_LINK_ID_LEN {
        random
            .fill(&mut byte)
            .map_err(|_| anyhow::anyhow!("failed to generate a fast link id"))?;
        if byte[0] >= 252 {
            continue;
        }
        output.push(FAST_LINK_ID_ALPHABET[(byte[0] % 36) as usize] as char);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Result<FastLinkResolution, _>::unwrap_err` would require
    /// `FastLinkResolution: Debug`, which it deliberately does not implement
    /// (it can hold a [`FastLinkConfig`], which must never derive `Debug`
    /// because that would print the credential). This extracts the error
    /// without that bound.
    fn expect_err(result: anyhow::Result<FastLinkResolution>) -> anyhow::Error {
        match result {
            Ok(_) => panic!("expected an error, got Ok"),
            Err(e) => e,
        }
    }

    fn base_args() -> FastLinkServerArgs {
        FastLinkServerArgs {
            enabled: true,
            vhost: Some("fast.bore.tld".to_string()),
            auth: Some("u:p".to_string()),
            wait_timeout_secs: DEFAULT_WAIT_TIMEOUT_SECS,
            max_active: DEFAULT_MAX_ACTIVE,
        }
    }

    #[test]
    fn resolve_disabled_ignores_and_lists_flags() {
        let args = FastLinkServerArgs {
            enabled: false,
            vhost: Some("fast.bore.tld".to_string()),
            auth: Some("u:p".to_string()),
            wait_timeout_secs: DEFAULT_WAIT_TIMEOUT_SECS + 1,
            max_active: DEFAULT_MAX_ACTIVE + 1,
        };
        let resolution = resolve_server_config(&args, Some("bore.tld")).unwrap();
        assert!(resolution.config.is_none());
        assert_eq!(
            resolution.ignored,
            vec![FLAG_VHOST, FLAG_AUTH, FLAG_WAIT_TIMEOUT, FLAG_MAX_ACTIVE]
        );

        let none_set = FastLinkServerArgs {
            enabled: false,
            vhost: None,
            auth: None,
            wait_timeout_secs: DEFAULT_WAIT_TIMEOUT_SECS,
            max_active: DEFAULT_MAX_ACTIVE,
        };
        let resolution = resolve_server_config(&none_set, None).unwrap();
        assert!(resolution.config.is_none());
        assert!(resolution.ignored.is_empty());
    }

    #[test]
    fn resolve_enabled_requires_each_input() {
        // Base domain missing.
        let args = base_args();
        let err = expect_err(resolve_server_config(&args, None));
        assert!(err.to_string().contains("vhost base domain"));
        let err = expect_err(resolve_server_config(&args, Some("")));
        assert!(err.to_string().contains("vhost base domain"));

        // Vhost missing.
        let mut args_no_vhost = base_args();
        args_no_vhost.vhost = None;
        let err = expect_err(resolve_server_config(&args_no_vhost, Some("bore.tld")));
        assert!(err.to_string().contains("--fast-link-transfer-vhost"));

        // Auth missing.
        let mut args_no_auth = base_args();
        args_no_auth.auth = None;
        let err = expect_err(resolve_server_config(&args_no_auth, Some("bore.tld")));
        assert!(err.to_string().contains("--fast-link-transfer-auth"));

        // Auth malformed.
        for bad in [":p", "u:", "up"] {
            let mut args_bad_auth = base_args();
            args_bad_auth.auth = Some(bad.to_string());
            let err = expect_err(resolve_server_config(&args_bad_auth, Some("bore.tld")));
            assert!(err.to_string().contains("USER:PASS"), "input {bad:?}");
        }

        // Wait timeout out of range.
        for bad in [0u64, MAX_WAIT_TIMEOUT_SECS + 1] {
            let mut args_bad_timeout = base_args();
            args_bad_timeout.wait_timeout_secs = bad;
            let err = expect_err(resolve_server_config(&args_bad_timeout, Some("bore.tld")));
            assert!(err.to_string().contains("wait-timeout"), "input {bad}");
        }

        // Max active out of range.
        for bad in [0usize, MAX_MAX_ACTIVE + 1] {
            let mut args_bad_max = base_args();
            args_bad_max.max_active = bad;
            let err = expect_err(resolve_server_config(&args_bad_max, Some("bore.tld")));
            assert!(err.to_string().contains("max-active"), "input {bad}");
        }
    }

    #[test]
    fn resolve_rejects_host_not_one_label_under_base() {
        for bad in [
            "a.b.bore.tld",
            "bore.tld",
            "fast.other.tld",
            "fast.bore.tld:443",
            "https://fast.bore.tld",
        ] {
            let mut args = base_args();
            args.vhost = Some(bad.to_string());
            let result = resolve_server_config(&args, Some("bore.tld"));
            assert!(result.is_err(), "input {bad:?} should be rejected");
        }
    }

    #[test]
    fn resolve_normalizes_host() {
        let mut args = base_args();
        args.vhost = Some(" FAST.Bore.TLD. ".to_string());
        let resolution = resolve_server_config(&args, Some("bore.tld")).unwrap();
        let config = resolution.config.unwrap();
        assert_eq!(config.host, "fast.bore.tld");
        assert_eq!(config.label, "fast");
    }

    #[test]
    fn resolve_error_never_echoes_the_password() {
        let mut args = base_args();
        args.auth = Some("user:supersecretpassword".to_string());
        args.wait_timeout_secs = 0;
        let err = expect_err(resolve_server_config(&args, Some("bore.tld")));
        assert!(!err.to_string().contains("supersecretpassword"));
    }

    #[test]
    fn generate_id_is_16_alphabet_chars_and_varies() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for _ in 0..100 {
            let id = generate_id().unwrap();
            assert_eq!(id.len(), FAST_LINK_ID_LEN);
            assert!(id.bytes().all(|b| FAST_LINK_ID_ALPHABET.contains(&b)));
            seen.insert(id);
        }
        assert!(seen.len() >= 99);
    }
}
