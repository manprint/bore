//! Scoped orchestration for the public transfer-link client.
//!
//! The command-line parser is added in the next sub-phase.  This module owns
//! the parts that must exist before a URL can be printed: strict endpoint and
//! CA validation, cryptographically random identity generation, loopback HTTP
//! ownership, and the cancellable vhost registration supervisor.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use url::Url;

use crate::client::{Client, ClientScope, ProviderMeta, VhostAttemptError};
use crate::reconnect::Backoff;
use crate::shared::HttpsPolicy;
use crate::transfer_link::{
    encode_path_segment, LinkOptions, PreparedFile, PreparedSource, TransferLinkHttp,
};
use crate::transport::{self, ConnectFailure, Endpoint};

/// Number of characters in a transfer-link vhost label.
pub const LINK_LABEL_LENGTH: usize = 16;

/// Alphabet used for the public transfer-link label.
pub const LINK_LABEL_ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Maximum time allowed for a registration rejection after a URL was already
/// published.  This is longer than the server's default control reaper so the
/// old registration can disappear before a retry.
pub const REGISTRATION_RETRY_GRACE: Duration = Duration::from_secs(75);

/// Lifecycle state of one transfer-link session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkState {
    /// Listener and identities are prepared, registration has not succeeded.
    Initializing,
    /// Vhost registration and the loopback listener are serving.
    Ready,
    /// The old registration was closed and a new one is being attempted.
    Reconnecting,
    /// Shutdown was requested and owned tasks are draining.
    Stopping,
    /// All owned tasks and sockets have been released.
    Stopped,
}

/// Classification used by the retry policy.  It is deliberately independent
/// from human-readable error text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkAttemptClass {
    /// A network disconnect or timeout that can be retried.
    Transient,
    /// The server rejected registration with a generic wire error.
    RegistrationRejected,
    /// TLS certificate/name verification failed.
    TlsVerification,
    /// Authentication failed or credentials were missing.
    Authentication,
    /// The peer violated the registration protocol.
    Protocol,
    /// A local configuration or announced URL is invalid.
    Permanent,
}

/// Configuration needed by the scoped supervisor.  The CLI owns source and
/// filename validation; this type owns only control-plane and transport data.
#[derive(Clone, Debug)]
pub struct LinkSupervisorConfig {
    /// Strict HTTPS control endpoint supplied by `--to`.
    pub to: String,
    /// Optional server authentication secret.
    pub secret: Option<String>,
    /// Optional PEM file containing additional trusted roots.
    pub ca_cert: Option<PathBuf>,
    /// Disable the QUIC direct data path and use the TCP relay.
    pub relay_only: bool,
    /// Number of control/data carriers requested from the server.
    pub carriers: u16,
}

/// Prepared transfer-link supervisor.
pub struct TransferLinkSupervisor {
    http: Option<TransferLinkHttp>,
    http_port: u16,
    endpoint: Endpoint,
    endpoint_text: String,
    label: String,
    client_id: String,
    secret: Option<String>,
    relay_only: bool,
    carriers: u16,
    tls_config: transport::ClientTlsConfig,
    path_registry: Arc<crate::transfer_link::BackendPathRegistry>,
    state: Arc<Mutex<LinkState>>,
}

impl std::fmt::Debug for TransferLinkSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferLinkSupervisor")
            .field("http_port", &self.http_port)
            .field("endpoint", &self.endpoint)
            .field("label", &self.label)
            .field("relay_only", &self.relay_only)
            .field("carriers", &self.carriers)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl LinkSupervisorConfig {
    /// Validate and normalize the control endpoint before any network dial.
    pub fn validate_endpoint(&self) -> Result<Endpoint> {
        validate_link_endpoint(&self.to)
    }
}

impl TransferLinkSupervisor {
    /// Construct a supervisor around an already-bound loopback HTTP listener.
    ///
    /// All local validation, including the optional CA file, happens here.  A
    /// caller can therefore guarantee that a malformed command never registers
    /// a vhost or prints a public URL.
    pub fn new(mut http: TransferLinkHttp, config: LinkSupervisorConfig) -> Result<Self> {
        let endpoint = config.validate_endpoint()?;
        let ca_pem = config
            .ca_cert
            .as_deref()
            .map(read_ca_certificate)
            .transpose()?;
        let tls_config = transport::client_config_with_extra_roots(ca_pem.as_deref())?;
        let http_port = http.local_addr()?.port();
        let label = generate_link_label()?;
        let client_id = generate_client_id()?;
        let path_registry = crate::transfer_link::BackendPathRegistry::new(256);
        http.set_path_registry(Arc::clone(&path_registry));
        Ok(Self {
            http: Some(http),
            http_port,
            endpoint,
            endpoint_text: config.to,
            label,
            client_id,
            secret: config.secret,
            relay_only: config.relay_only,
            carriers: config.carriers,
            tls_config,
            path_registry,
            state: Arc::new(Mutex::new(LinkState::Initializing)),
        })
    }

    /// Return the fixed random label used for this session's vhost.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Return the current lifecycle state.
    pub fn state(&self) -> LinkState {
        *self.state.lock().expect("transfer-link state poisoned")
    }

    /// Run registration, serving, reconnect and shutdown until cancellation.
    ///
    /// The one-shot sender receives the validated HTTPS base URL exactly once.
    /// The supervisor never writes to stdout; the CLI decides how to append the
    /// filename and flush the single user-visible line.
    pub async fn run(
        mut self,
        shutdown: CancellationToken,
        ready: oneshot::Sender<String>,
    ) -> Result<()> {
        let http = self
            .http
            .take()
            .expect("transfer-link supervisor HTTP listener already consumed");
        let http_cancel = shutdown.child_token();
        let http_task = tokio::spawn(http.run(http_cancel.clone()));
        let mut ready = Some(ready);
        let mut expected_url: Option<String> = None;
        let mut published = false;
        let mut rejection_started: Option<Instant> = None;
        let mut backoff = Backoff::new();

        let result = loop {
            if shutdown.is_cancelled() {
                break Ok(());
            }
            self.set_state(if published {
                LinkState::Reconnecting
            } else {
                LinkState::Initializing
            });
            let scope = ClientScope::new_with_transport(
                Some(self.path_registry.hook()),
                Some(self.tls_config.clone()),
            );
            let client = Client::new_vhost_provider_with_udp_scoped(
                "127.0.0.1",
                self.http_port,
                &self.endpoint_text,
                &self.label,
                &self.client_id,
                self.secret.as_deref(),
                false,
                self.carriers,
                !self.relay_only,
                ProviderMeta {
                    https_policy: Some(HttpsPolicy::Redirect),
                    ..ProviderMeta::default()
                },
                None,
                Arc::clone(&scope),
            )
            .await;

            let client = match client {
                Ok(client) => client,
                Err(error) => {
                    let class = classify_attempt_error(&error);
                    scope.shutdown().await;
                    if should_stop_after_error(class, published, rejection_started) {
                        break Err(error);
                    }
                    if class == LinkAttemptClass::RegistrationRejected {
                        rejection_started.get_or_insert_with(Instant::now);
                    }
                    self.wait_backoff(&shutdown, &mut backoff).await;
                    continue;
                }
            };

            let urls = client.vhost_urls().cloned().unwrap_or_default();
            let url = match validate_ready_https_url(urls.https_url.as_deref(), &self.label) {
                Ok(url) => url,
                Err(error) => {
                    scope.shutdown().await;
                    break Err(error);
                }
            };
            if let Some(previous) = &expected_url {
                if previous != &url {
                    scope.shutdown().await;
                    break Err(anyhow::anyhow!("vhost HTTPS URL changed during reconnect"));
                }
            } else {
                expected_url = Some(url.clone());
            }
            if !published {
                if let Some(sender) = ready.take() {
                    let _ = sender.send(url.clone());
                }
                published = true;
            }
            rejection_started = None;
            backoff.reset();
            self.set_state(LinkState::Ready);
            info!(label = %self.label, "transfer-link vhost ready");

            let listen_result = tokio::select! {
                _ = shutdown.cancelled() => Ok(()),
                result = client.listen() => result,
            };
            scope.shutdown().await;
            if shutdown.is_cancelled() {
                break Ok(());
            }
            if let Err(error) = listen_result {
                debug!(error = %error, "transfer-link registration ended; reconnecting");
            }
            self.set_state(LinkState::Reconnecting);
            self.wait_backoff(&shutdown, &mut backoff).await;
        };

        self.set_state(LinkState::Stopping);
        http_cancel.cancel();
        match timeout(Duration::from_secs(5), http_task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                if result.is_ok() {
                    return Err(anyhow::anyhow!(error));
                }
                debug!(error = %error, "transfer-link HTTP task ended after supervisor failure");
            }
            Ok(Err(error)) => warn!(error = %error, "transfer-link HTTP task join failed"),
            Err(_) => warn!("transfer-link HTTP task did not stop within 5 seconds"),
        }
        self.set_state(LinkState::Stopped);
        result
    }

    async fn wait_backoff(&self, shutdown: &CancellationToken, backoff: &mut Backoff) {
        let delay = backoff.next_delay();
        tokio::select! {
            _ = shutdown.cancelled() => {}
            _ = sleep(delay) => {}
        }
    }

    fn set_state(&self, state: LinkState) {
        *self.state.lock().expect("transfer-link state poisoned") = state;
    }
}

/// Validate a transfer-link control endpoint without invoking permissive
/// legacy endpoint parsing.
pub fn validate_link_endpoint(to: &str) -> Result<Endpoint> {
    let url = Url::parse(to).context("invalid --to URL")?;
    anyhow::ensure!(
        url.scheme() == "https",
        "transfer link --to must use https://"
    );
    let host = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| anyhow::anyhow!("transfer link --to has no host"))?;
    anyhow::ensure!(
        url.username().is_empty(),
        "transfer link --to cannot contain userinfo"
    );
    anyhow::ensure!(
        url.password().is_none(),
        "transfer link --to cannot contain userinfo"
    );
    anyhow::ensure!(
        url.query().is_none(),
        "transfer link --to cannot contain a query"
    );
    anyhow::ensure!(
        url.fragment().is_none(),
        "transfer link --to cannot contain a fragment"
    );
    anyhow::ensure!(
        url.path().is_empty() || url.path() == "/",
        "transfer link --to cannot contain a path"
    );
    let port = url.port().unwrap_or(443);
    Ok(Endpoint {
        host: host.to_string(),
        port,
        tls: true,
    })
}

/// Validate the HTTPS URL announced by the vhost server.
pub fn validate_ready_https_url(raw: Option<&str>, label: &str) -> Result<String> {
    let raw = raw.ok_or_else(|| anyhow::anyhow!("server did not announce an HTTPS vhost URL"))?;
    let url = Url::parse(raw).context("server announced a malformed HTTPS URL")?;
    anyhow::ensure!(
        url.scheme() == "https",
        "server announced a non-HTTPS vhost URL"
    );
    anyhow::ensure!(
        url.host_str().is_some(),
        "server announced an HTTPS URL without a host"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "server announced URL userinfo"
    );
    anyhow::ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "server announced URL query/fragment"
    );
    anyhow::ensure!(
        url.path().is_empty() || url.path() == "/",
        "server announced URL with an unexpected path"
    );
    let host = url.host_str().unwrap_or_default();
    let mut labels = host.split('.');
    let first = labels.next().unwrap_or_default();
    anyhow::ensure!(
        first == label && labels.next().is_some(),
        "server announced HTTPS URL for a different vhost label"
    );
    Ok(url.to_string())
}

/// Append a validated filename as the only public URL path segment.
pub fn public_file_url(base: &str, filename: &str) -> Result<String> {
    let mut url = Url::parse(base).context("invalid announced HTTPS URL")?;
    anyhow::ensure!(
        url.path().is_empty() || url.path() == "/",
        "announced URL has a base path"
    );
    url.set_path(&format!("/{}", encode_path_segment(filename)));
    Ok(url.to_string())
}

/// Generate a uniformly distributed lowercase base-36 label with rejection
/// sampling, avoiding modulo bias (256 is not divisible by 36).
pub fn generate_link_label() -> Result<String> {
    let random = SystemRandom::new();
    let mut output = String::with_capacity(LINK_LABEL_LENGTH);
    let mut byte = [0u8; 1];
    while output.len() < LINK_LABEL_LENGTH {
        random
            .fill(&mut byte)
            .map_err(|_| anyhow::anyhow!("failed to generate link identity"))?;
        if byte[0] >= 252 {
            continue;
        }
        output.push(LINK_LABEL_ALPHABET[(byte[0] % 36) as usize] as char);
    }
    Ok(output)
}

/// Generate a distinct opaque client identifier for vhost ownership checks.
pub fn generate_client_id() -> Result<String> {
    let random = SystemRandom::new();
    let mut bytes = [0u8; 32];
    random
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("failed to generate client identity"))?;
    Ok(hex::encode(bytes))
}

fn read_ca_certificate(path: &Path) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read CA certificate {}", path.display()))?;
    anyhow::ensure!(!bytes.is_empty(), "CA certificate file is empty");
    Ok(bytes)
}

/// Classify a connection attempt from typed markers only.
pub fn classify_attempt_error(error: &anyhow::Error) -> LinkAttemptClass {
    if error.downcast_ref::<VhostAttemptError>().is_some() {
        match error.downcast_ref::<VhostAttemptError>().unwrap() {
            VhostAttemptError::Authentication => LinkAttemptClass::Authentication,
            VhostAttemptError::RegistrationRejected => LinkAttemptClass::RegistrationRejected,
            VhostAttemptError::Protocol => LinkAttemptClass::Protocol,
        }
    } else if error.downcast_ref::<ConnectFailure>().is_some() {
        match error.downcast_ref::<ConnectFailure>().unwrap() {
            ConnectFailure::Transport => LinkAttemptClass::Transient,
            ConnectFailure::Tls => LinkAttemptClass::TlsVerification,
        }
    } else {
        LinkAttemptClass::Transient
    }
}

/// Decide whether an attempt error ends the supervisor.
pub fn should_stop_after_error(
    class: LinkAttemptClass,
    published: bool,
    rejection_started: Option<Instant>,
) -> bool {
    match class {
        LinkAttemptClass::Transient => false,
        LinkAttemptClass::RegistrationRejected => {
            !published
                || rejection_started
                    .map(|started| started.elapsed() >= REGISTRATION_RETRY_GRACE)
                    .unwrap_or(false)
        }
        LinkAttemptClass::TlsVerification
        | LinkAttemptClass::Authentication
        | LinkAttemptClass::Protocol
        | LinkAttemptClass::Permanent => true,
    }
}

/// Prepare a regular-file HTTP listener and its supervisor in one bounded
/// helper.  The CLI uses this after source validation and before registration.
pub async fn bind_file_supervisor(
    prepared: PreparedFile,
    options: LinkOptions,
    config: LinkSupervisorConfig,
) -> Result<TransferLinkSupervisor> {
    bind_source_supervisor(PreparedSource::File(prepared), options, config).await
}

/// Prepare a source-aware loopback listener and its vhost supervisor.
pub async fn bind_source_supervisor(
    prepared: PreparedSource,
    options: LinkOptions,
    config: LinkSupervisorConfig,
) -> Result<TransferLinkSupervisor> {
    let http = TransferLinkHttp::bind_source(prepared, options)
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    TransferLinkSupervisor::new(http, config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_validation_rejects_plain_and_malformed_inputs() {
        assert!(validate_link_endpoint("http://example.test").is_err());
        assert!(validate_link_endpoint("https://example.test/path").is_err());
        assert!(validate_link_endpoint("https://user@example.test").is_err());
        assert!(validate_link_endpoint("https://example.test:bad").is_err());
        assert!(validate_link_endpoint("https://[::1]:8443/").is_ok());
    }

    #[test]
    fn ready_url_validation_requires_the_requested_label_and_https() {
        assert!(validate_ready_https_url(None, "abc").is_err());
        assert!(validate_ready_https_url(Some("http://abc.example/"), "abc").is_err());
        assert!(validate_ready_https_url(Some("https://other.example/"), "abc").is_err());
        assert_eq!(
            validate_ready_https_url(Some("https://abc.example:8443"), "abc").unwrap(),
            "https://abc.example:8443/"
        );
    }

    #[test]
    fn generated_labels_have_fixed_uniform_alphabet() {
        for _ in 0..64 {
            let label = generate_link_label().unwrap();
            assert_eq!(label.len(), LINK_LABEL_LENGTH);
            assert!(label
                .bytes()
                .all(|byte| LINK_LABEL_ALPHABET.contains(&byte)));
        }
    }

    #[test]
    fn retry_policy_keeps_ready_url_for_transient_failures() {
        assert!(!should_stop_after_error(
            LinkAttemptClass::Transient,
            true,
            None
        ));
        assert!(!should_stop_after_error(
            LinkAttemptClass::RegistrationRejected,
            true,
            Some(Instant::now())
        ));
        assert!(should_stop_after_error(
            LinkAttemptClass::Authentication,
            true,
            None
        ));
    }

    #[test]
    fn public_url_encodes_filename_without_changing_host() {
        assert_eq!(
            public_file_url("https://abc.example/", "my file.bin").unwrap(),
            "https://abc.example/my%20file.bin"
        );
    }
}
