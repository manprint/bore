//! Bounded HTTP/1.1 serving for a transfer-link source.
//!
//! The listener is deliberately loopback-only.  A later vhost client exposes
//! this listener through the existing bore vhost; this module does not add a
//! second public listener or a second routing protocol.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::stream;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{
    HeaderValue, ACCEPT_RANGES, ALLOW, CACHE_CONTROL, CONNECTION, CONTENT_DISPOSITION,
    CONTENT_LENGTH, CONTENT_TYPE, EXPECT, REFERRER_POLICY, TRANSFER_ENCODING, UPGRADE,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::client::BackendPath;

use super::manifest::PreparedSource;
use super::oneshot::{self, OneShotState, OneShotStatus};
use super::source::{
    content_disposition, open_prepared_file, spawn_file_producer, FileProducerHandle,
    LinkConfigError, LinkOptions, PreparedFile, MIME_OCTET_STREAM,
};
use super::stats::{DownloadId, SourceCompletion, SourceMessage};
use super::zip::spawn_archive_producer;

/// Maximum number of request headers accepted by the HTTP/1 parser.
pub const MAX_HEADERS: usize = 64;

/// Maximum parser buffer.  Payload chunks are independently bounded by the
/// source producer and are not constrained by this parser setting.
pub const MAX_HTTP_BUFFER: usize = 16 * 1024;

/// Maximum time allowed for a request header block.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(3);

/// Connections reserved for HEAD and rejected requests in addition to active
/// downloads.
pub const CONNECTION_HEADROOM: usize = 32;

/// The boxed fallible response body used by the transfer-link HTTP layer.
pub type TransferBody = BoxBody<Bytes, io::Error>;

/// Errors returned while binding or running the loopback listener.
#[derive(Debug)]
pub enum HttpServerError {
    /// The loopback listener or accept loop failed.
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// Underlying operating-system error.
        message: String,
    },
    /// Link settings were invalid before a listener was created.
    Config(LinkConfigError),
}

impl HttpServerError {
    fn io(operation: &'static str, error: io::Error) -> Self {
        Self::Io {
            operation,
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for HttpServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { operation, message } => write!(f, "{operation} failed: {message}"),
            Self::Config(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for HttpServerError {}

/// A loopback HTTP/1.1 server for one prepared source and one published path.
///
/// `bind` never exposes the listener beyond `127.0.0.1`; callers hand the
/// returned port to the existing vhost provider.  Each GET opens a fresh file
/// handle and producer, so independent clients have independent offsets while
/// sharing only the bounded download semaphore.
pub struct TransferLinkHttp {
    listener: TcpListener,
    state: Arc<HttpState>,
}

struct HttpState {
    prepared: PreparedSource,
    options: LinkOptions,
    path_registry: Option<Arc<super::path::BackendPathRegistry>>,
    downloads: Arc<Semaphore>,
    active_downloads: Arc<AtomicUsize>,
    connections: Arc<Semaphore>,
    shutdown: CancellationToken,
    next_download: AtomicU64,
}

impl TransferLinkHttp {
    /// Bind an ephemeral loopback listener for the prepared file.
    pub async fn bind(
        prepared: PreparedFile,
        options: LinkOptions,
    ) -> Result<Self, HttpServerError> {
        Self::bind_source(PreparedSource::File(prepared), options).await
    }

    /// Bind an ephemeral loopback listener for a raw file or streaming ZIP.
    pub async fn bind_source(
        prepared: PreparedSource,
        options: LinkOptions,
    ) -> Result<Self, HttpServerError> {
        let options = LinkOptions::new(
            options.filename.clone(),
            options.limits.max_downloads,
            options.limits.stats_interval,
        )
        .map_err(HttpServerError::Config)?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|error| HttpServerError::io("bind transfer-link HTTP listener", error))?;
        let connection_limit = options
            .limits
            .max_downloads
            .saturating_add(CONNECTION_HEADROOM);
        Ok(Self {
            listener,
            state: Arc::new(HttpState {
                prepared,
                downloads: Arc::new(Semaphore::new(options.limits.max_downloads)),
                active_downloads: Arc::new(AtomicUsize::new(0)),
                connections: Arc::new(Semaphore::new(connection_limit)),
                shutdown: CancellationToken::new(),
                next_download: AtomicU64::new(1),
                options,
                path_registry: None,
            }),
        })
    }

    /// Attach the scoped vhost path registry before the listener is spawned.
    pub(crate) fn set_path_registry(&mut self, registry: Arc<super::path::BackendPathRegistry>) {
        Arc::get_mut(&mut self.state)
            .expect("transfer-link HTTP state already shared")
            .path_registry = Some(registry);
    }

    /// Return the actual loopback address selected by the operating system.
    pub fn local_addr(&self) -> Result<SocketAddr, HttpServerError> {
        self.listener
            .local_addr()
            .map_err(|error| HttpServerError::io("read transfer-link HTTP listener address", error))
    }

    /// Cancel this server and all active response bodies.
    pub fn shutdown(&self) {
        self.state.shutdown.cancel();
    }

    /// Accept and serve connections until `cancellation` or [`Self::shutdown`]
    /// is observed.  The method owns and joins every connection task before it
    /// returns.
    pub async fn run(self, cancellation: CancellationToken) -> Result<(), HttpServerError> {
        let mut tasks: JoinSet<io::Result<()>> = JoinSet::new();
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = self.state.shutdown.cancelled() => break,
                joined = tasks.join_next(), if !tasks.is_empty() => {
                    report_connection(joined);
                }
                accepted = self.listener.accept() => {
                    let (stream, peer) = accepted
                        .map_err(|error| HttpServerError::io("accept transfer-link HTTP connection", error))?;
                    let permit = match Arc::clone(&self.state.connections).try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            debug!(%peer, "transfer-link HTTP connection cap reached");
                            drop(stream);
                            continue;
                        }
                    };
                    let state = Arc::clone(&self.state);
                    tasks.spawn(async move {
                        serve_connection(stream, state, permit, peer).await
                    });
                }
            }
        }

        self.state.shutdown.cancel();
        while let Some(joined) = tasks.join_next().await {
            report_connection(Some(joined));
        }
        Ok(())
    }
}

fn report_connection(joined: Option<Result<io::Result<()>, tokio::task::JoinError>>) {
    if let Some(joined) = joined {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                debug!(error = %error, "transfer-link HTTP connection ended with error")
            }
            Err(error) => warn!(error = %error, "transfer-link HTTP connection task failed"),
        }
    }
}

async fn serve_connection(
    stream: TcpStream,
    state: Arc<HttpState>,
    _connection_permit: OwnedSemaphorePermit,
    peer: SocketAddr,
) -> io::Result<()> {
    let io = TokioIo::new(stream);
    let service_state = Arc::clone(&state);
    let connection_outcome = Arc::new(HttpConnectionOutcome::default());
    let service_outcome = Arc::clone(&connection_outcome);
    let service = service_fn(move |request| {
        handle_request(
            Arc::clone(&service_state),
            peer,
            request,
            Arc::clone(&service_outcome),
        )
    });
    let mut builder = http1::Builder::new();
    builder
        .keep_alive(false)
        .max_headers(MAX_HEADERS)
        .max_buf_size(MAX_HTTP_BUFFER)
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    let connection = builder.serve_connection(io, service);
    let result = tokio::select! {
        result = connection => result.map_err(io::Error::other),
        _ = state.shutdown.cancelled() => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "transfer-link HTTP server shutting down",
        )),
    };
    match &result {
        Ok(()) => connection_outcome.connection_succeeded(),
        Err(error) => {
            connection_outcome.connection_failed(error.to_string());
            connection_outcome.cancel_producer();
        }
    }
    if let Err(error) = connection_outcome.join_producer().await {
        warn!(error = %error, peer = %peer, "transfer-link producer supervisor failed to join");
    }
    result
}

async fn handle_request(
    state: Arc<HttpState>,
    peer: SocketAddr,
    request: Request<Incoming>,
    connection_outcome: Arc<HttpConnectionOutcome>,
) -> Result<Response<TransferBody>, Infallible> {
    let response = if request_has_body_or_upgrade(&request) {
        error_response(
            StatusCode::BAD_REQUEST,
            "request body and upgrades are not supported",
        )
    } else if request.method() != Method::GET && request.method() != Method::HEAD {
        let mut response = error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
        response
    } else if request.uri().query().is_some()
        || request.uri().path() != format!("/{}", state.options.path_segment)
    {
        error_response(StatusCode::NOT_FOUND, "not found")
    } else if request.version() == Version::HTTP_10
        && (state.prepared.is_archive() || state.prepared.is_one_shot())
    {
        // HTTP/1.0 has no chunked framing.  A streamed archive has no known
        // length, so accepting it would make a compliant client wait forever
        // for a body terminator that the protocol cannot express.
        error_response(
            StatusCode::HTTP_VERSION_NOT_SUPPORTED,
            "streaming archives require HTTP/1.1",
        )
    } else if request.method() == Method::HEAD {
        metadata_response(&state).await
    } else {
        get_response(state, peer, connection_outcome).await
    };
    Ok(response)
}

fn request_has_body_or_upgrade(request: &Request<Incoming>) -> bool {
    request.headers().contains_key(CONTENT_LENGTH)
        || request.headers().contains_key(TRANSFER_ENCODING)
        || request.headers().contains_key(EXPECT)
        || request.headers().contains_key(UPGRADE)
}

async fn metadata_response(state: &HttpState) -> Response<TransferBody> {
    if let PreparedSource::OneShot(stream) = &state.prepared {
        match stream.state.status() {
            OneShotStatus::Ready | OneShotStatus::Streaming => {}
            OneShotStatus::Consumed | OneShotStatus::Failed => {
                return error_response(StatusCode::GONE, "stream is no longer available");
            }
        }
    }
    if let PreparedSource::Archive(archive) = &state.prepared {
        if let Err(error) = archive.manifest.validate().await {
            debug!(error = %error, "transfer-link archive failed before HEAD response");
            return error_response(StatusCode::SERVICE_UNAVAILABLE, "source is unavailable");
        }
    }
    let body = Full::new(Bytes::new())
        .map_err(|never: Infallible| match never {})
        .boxed();
    build_file_response(state, body, true)
}

async fn get_response(
    state: Arc<HttpState>,
    peer: SocketAddr,
    connection_outcome: Arc<HttpConnectionOutcome>,
) -> Response<TransferBody> {
    // Claim a one-shot before checking the download semaphore so a concurrent
    // GET receives the precise 409 state instead of a misleading capacity 503.
    let claimed_oneshot = match &state.prepared {
        PreparedSource::OneShot(stream) => {
            if let Err(status) = stream.state.claim() {
                let (code, message) = match status {
                    OneShotStatus::Streaming => (StatusCode::CONFLICT, "stream is already in use"),
                    OneShotStatus::Consumed | OneShotStatus::Failed => {
                        (StatusCode::GONE, "stream is no longer available")
                    }
                    OneShotStatus::Ready => unreachable!("claim only fails for non-ready states"),
                };
                return error_response(code, message);
            }
            Some(Arc::clone(&stream.state))
        }
        _ => None,
    };
    let permit = match Arc::clone(&state.downloads).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            if let Some(oneshot) = claimed_oneshot.as_ref() {
                oneshot.failed();
            }
            return error_response(StatusCode::SERVICE_UNAVAILABLE, "download capacity full");
        }
    };

    match &state.prepared {
        PreparedSource::File(prepared) => {
            if let Err(error) = open_prepared_file(prepared).await {
                debug!(error = %error, "transfer-link source failed before response headers");
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "source is unavailable");
            }
        }
        PreparedSource::Archive(archive) => {
            if let Err(error) = archive.manifest.validate().await {
                debug!(error = %error, "transfer-link archive failed before response headers");
                return error_response(StatusCode::SERVICE_UNAVAILABLE, "source is unavailable");
            }
        }
        PreparedSource::OneShot(_) => {}
    }

    let id = state.next_download.fetch_add(1, Ordering::Relaxed);
    let path = state
        .path_registry
        .as_ref()
        .and_then(|registry| registry.path_for(peer));
    let cancellation = state.shutdown.child_token();
    let (producer, size, oneshot) = match &state.prepared {
        PreparedSource::File(prepared) => (
            spawn_file_producer(prepared.clone(), cancellation.clone()),
            Some(prepared.size),
            None,
        ),
        PreparedSource::Archive(archive) => (
            spawn_archive_producer(archive.clone(), cancellation.clone()),
            None,
            None,
        ),
        PreparedSource::OneShot(stream) => {
            let producer = match oneshot::start(&stream.kind, cancellation.clone()).await {
                Ok(producer) => producer,
                Err(error) => {
                    stream.state.failed();
                    warn!(error = %error, "transfer-link one-shot producer failed to start");
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        "producer could not be started",
                    );
                }
            };
            (producer, None, claimed_oneshot)
        }
    };
    state.active_downloads.fetch_add(1, Ordering::Relaxed);
    debug!(download_id = id, "transfer-link download started");
    let download_outcome = Arc::new(DownloadOutcome::new(
        DownloadId(id),
        size,
        path,
        oneshot.clone(),
    ));
    connection_outcome.attach(Arc::clone(&download_outcome));
    let body = body_from_producer(
        producer,
        cancellation,
        Some(permit),
        DownloadId(id),
        size,
        state.options.limits.stats_interval,
        path,
        oneshot,
        Some(download_outcome),
        Some(Arc::clone(&state.active_downloads)),
    );
    // Omit Content-Length for every GET. Hyper must poll the terminal source
    // message so final fingerprint/hash validation and transport outcome are
    // both known before the response is committed. HEAD still advertises the
    // known file size through `metadata_response`.
    build_file_response(&state, body, false)
}

fn build_file_response(
    state: &HttpState,
    body: TransferBody,
    include_content_length: bool,
) -> Response<TransferBody> {
    let disposition = match content_disposition(&state.options.filename) {
        Ok(value) => value,
        Err(error) => {
            debug!(error = %error, "transfer-link content disposition generation failed");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid download filename",
            );
        }
    };
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(
            CONTENT_TYPE,
            if state.prepared.is_archive() {
                "application/zip"
            } else {
                MIME_OCTET_STREAM
            },
        )
        .header(CACHE_CONTROL, "no-store")
        .header(REFERRER_POLICY, "no-referrer")
        .header(ACCEPT_RANGES, "none")
        .header(CONNECTION, "close");
    if include_content_length {
        if let Some(size) = state.prepared.known_size() {
            builder = builder.header(CONTENT_LENGTH, size.to_string());
        }
    }
    match HeaderValue::from_str(&disposition) {
        Ok(value) => builder
            .header(CONTENT_DISPOSITION, value)
            .body(body)
            .unwrap_or_else(|_| {
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "response build failed")
            }),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "invalid response header"),
    }
}

fn error_response(status: StatusCode, message: &'static str) -> Response<TransferBody> {
    let bytes = Bytes::from_static(message.as_bytes());
    let body = Full::new(bytes.clone())
        .map_err(|never: Infallible| match never {})
        .boxed();
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(CONTENT_LENGTH, bytes.len().to_string())
        .header(CONNECTION, "close")
        .body(body)
        .unwrap_or_else(|_| {
            Response::new(
                Full::new(Bytes::new())
                    .map_err(|never: Infallible| match never {})
                    .boxed(),
            )
        })
}

/// Turn a bounded source receiver into a fallible HTTP body.
///
/// This helper is also used by later stream producers (ZIP/stdin/exec).  A
/// receiver close without [`SourceMessage::Complete`] is always an error; it
/// is never translated into a successful HTTP EOF.
pub fn body_from_messages(
    receiver: mpsc::Receiver<SourceMessage>,
    cancellation: CancellationToken,
) -> TransferBody {
    message_body(MessageBodyState {
        receiver,
        owner: None,
        cancellation,
        permit: None,
        complete: false,
        id: None,
        size: None,
        started: Instant::now(),
        next_report: Instant::now() + Duration::from_secs(1),
        bytes_for_http: 0,
        stats_interval: Duration::from_secs(1),
        path: None,
        oneshot: None,
        outcome: None,
        active_downloads: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn body_from_producer(
    producer: FileProducerHandle,
    cancellation: CancellationToken,
    permit: Option<OwnedSemaphorePermit>,
    id: DownloadId,
    size: Option<u64>,
    stats_interval: Duration,
    path: Option<BackendPath>,
    oneshot: Option<Arc<OneShotState>>,
    outcome: Option<Arc<DownloadOutcome>>,
    active_downloads: Option<Arc<AtomicUsize>>,
) -> TransferBody {
    let (receiver, owner, _completion) = producer.into_parts_with_completion();
    if let Some(outcome) = &outcome {
        outcome.attach_producer(Arc::clone(&owner));
    }
    message_body(MessageBodyState {
        receiver,
        owner: Some(owner),
        cancellation,
        permit,
        complete: false,
        id: Some(id),
        size,
        started: Instant::now(),
        next_report: Instant::now() + stats_interval,
        bytes_for_http: 0,
        stats_interval,
        path,
        oneshot,
        outcome,
        active_downloads,
    })
}

struct MessageBodyState {
    receiver: mpsc::Receiver<SourceMessage>,
    owner: Option<Arc<super::source::ProducerTaskOwner>>,
    cancellation: CancellationToken,
    permit: Option<OwnedSemaphorePermit>,
    complete: bool,
    id: Option<DownloadId>,
    size: Option<u64>,
    started: Instant,
    next_report: Instant,
    bytes_for_http: u64,
    stats_interval: Duration,
    path: Option<BackendPath>,
    oneshot: Option<Arc<OneShotState>>,
    outcome: Option<Arc<DownloadOutcome>>,
    active_downloads: Option<Arc<AtomicUsize>>,
}

/// Per-connection hand-off between the Hyper future and its response body.
///
/// A transfer body can observe a valid source completion before Hyper has
/// finished writing the socket.  Keeping the download outcome behind this
/// connection owner prevents that candidate from being published as success
/// until the connection future also reports success.
#[derive(Default)]
struct HttpConnectionOutcome {
    download: Mutex<Option<Arc<DownloadOutcome>>>,
}

impl HttpConnectionOutcome {
    fn attach(&self, download: Arc<DownloadOutcome>) {
        *self
            .download
            .lock()
            .expect("connection outcome mutex poisoned") = Some(download);
    }

    fn connection_succeeded(&self) {
        if let Some(download) = self
            .download
            .lock()
            .expect("connection outcome mutex poisoned")
            .clone()
        {
            download.connection_succeeded();
        }
    }

    fn connection_failed(&self, error: String) {
        if let Some(download) = self
            .download
            .lock()
            .expect("connection outcome mutex poisoned")
            .clone()
        {
            download.connection_failed(error);
        }
    }

    fn cancel_producer(&self) {
        if let Some(download) = self
            .download
            .lock()
            .expect("connection outcome mutex poisoned")
            .clone()
        {
            download.cancel_producer();
        }
    }

    async fn join_producer(&self) -> Result<(), tokio::task::JoinError> {
        let download = self
            .download
            .lock()
            .expect("connection outcome mutex poisoned")
            .clone();
        match download {
            Some(download) => download.join_producer().await,
            None => Ok(()),
        }
    }
}

#[derive(Clone)]
enum SourceTerminal {
    Complete(SourceCompletion),
    Failed(String),
}

/// Single terminal outcome owner for one GET.
struct DownloadOutcome {
    id: DownloadId,
    size: Option<u64>,
    path: Option<BackendPath>,
    started: Instant,
    bytes_for_http: AtomicU64,
    source: Mutex<Option<SourceTerminal>>,
    connection: Mutex<Option<Result<(), String>>>,
    finalized: AtomicBool,
    oneshot: Option<Arc<OneShotState>>,
    producer: Mutex<Option<Arc<super::source::ProducerTaskOwner>>>,
}

impl DownloadOutcome {
    fn new(
        id: DownloadId,
        size: Option<u64>,
        path: Option<BackendPath>,
        oneshot: Option<Arc<OneShotState>>,
    ) -> Self {
        Self {
            id,
            size,
            path,
            started: Instant::now(),
            bytes_for_http: AtomicU64::new(0),
            source: Mutex::new(None),
            connection: Mutex::new(None),
            finalized: AtomicBool::new(false),
            oneshot,
            producer: Mutex::new(None),
        }
    }

    fn attach_producer(&self, owner: Arc<super::source::ProducerTaskOwner>) {
        *self
            .producer
            .lock()
            .expect("download producer mutex poisoned") = Some(owner);
    }

    fn cancel_producer(&self) {
        if let Some(owner) = self
            .producer
            .lock()
            .expect("download producer mutex poisoned")
            .clone()
        {
            owner.cancel();
        }
    }

    async fn join_producer(&self) -> Result<(), tokio::task::JoinError> {
        let owner = self
            .producer
            .lock()
            .expect("download producer mutex poisoned")
            .clone();
        match owner {
            Some(owner) => owner.join().await,
            None => Ok(()),
        }
    }

    fn add_bytes(&self, bytes: u64) {
        self.bytes_for_http.fetch_add(bytes, Ordering::Relaxed);
    }

    fn source_complete(&self, summary: SourceCompletion) {
        self.set_source(SourceTerminal::Complete(summary));
    }

    fn source_failed(&self, error: impl Into<String>) {
        self.set_source(SourceTerminal::Failed(error.into()));
    }

    fn set_source(&self, terminal: SourceTerminal) {
        let mut source = self.source.lock().expect("download outcome mutex poisoned");
        if source.is_none() {
            *source = Some(terminal);
        }
        drop(source);
        self.try_finalize();
    }

    fn connection_succeeded(&self) {
        self.set_connection(Ok(()));
    }

    fn connection_failed(&self, error: impl Into<String>) {
        self.set_connection(Err(error.into()));
    }

    fn set_connection(&self, result: Result<(), String>) {
        let mut connection = self
            .connection
            .lock()
            .expect("download outcome mutex poisoned");
        if connection.is_none() {
            *connection = Some(result);
        }
        drop(connection);
        self.try_finalize();
    }

    fn try_finalize(&self) {
        let source = self
            .source
            .lock()
            .expect("download outcome mutex poisoned")
            .clone();
        let connection = self
            .connection
            .lock()
            .expect("download outcome mutex poisoned")
            .clone();
        let Some(connection) = connection.as_ref() else {
            return;
        };
        let success =
            connection.is_ok() && matches!(source.as_ref(), Some(SourceTerminal::Complete(_)));
        let failure = if connection.is_err() {
            connection.as_ref().err().cloned()
        } else {
            match source.as_ref() {
                Some(SourceTerminal::Failed(error)) => Some(error.clone()),
                Some(SourceTerminal::Complete(_)) => None,
                None => return,
            }
        };
        let summary = match source.as_ref() {
            Some(SourceTerminal::Complete(summary)) => Some(summary.clone()),
            _ => None,
        };
        if self
            .finalized
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        if success {
            if let Some(oneshot) = &self.oneshot {
                oneshot.consumed();
            }
            let sha256 = summary.as_ref().map(|summary| hex::encode(summary.sha256));
            info!(
                download_id = self.id.0,
                path = ?self.path,
                bytes_for_http = self.bytes_for_http.load(Ordering::Relaxed),
                size = self.size,
                elapsed_ms = self.started.elapsed().as_millis() as u64,
                sha256 = ?sha256,
                "transfer-link download completed"
            );
        } else {
            if let Some(oneshot) = &self.oneshot {
                oneshot.failed();
            }
            warn!(
                download_id = self.id.0,
                path = ?self.path,
                bytes_for_http = self.bytes_for_http.load(Ordering::Relaxed),
                size = self.size,
                elapsed_ms = self.started.elapsed().as_millis() as u64,
                error = failure.as_deref().unwrap_or("source did not complete"),
                "transfer-link download failed"
            );
        }
    }
}

impl Drop for MessageBodyState {
    fn drop(&mut self) {
        if !self.complete {
            self.cancellation.cancel();
            if let Some(outcome) = &self.outcome {
                outcome.source_failed("HTTP body dropped before source completion");
            } else if let Some(oneshot) = &self.oneshot {
                oneshot.failed();
            }
            if let Some(owner) = self.owner.as_ref() {
                owner.cancel();
            }
        }
        self.permit.take();
        if let Some(active_downloads) = self.active_downloads.take() {
            active_downloads.fetch_sub(1, Ordering::Relaxed);
        }
        if !self.complete {
            if let Some(id) = self.id {
                debug!(
                    download_id = id.0,
                    bytes_for_http = self.bytes_for_http,
                    "transfer-link download body dropped before completion"
                );
            }
        }
    }
}

enum BodyEvent {
    Message(Option<SourceMessage>),
    Tick,
}

fn log_progress(state: &MessageBodyState) {
    let Some(id) = state.id else {
        return;
    };
    let elapsed = state.started.elapsed().as_secs_f64().max(0.001);
    let active_downloads = state
        .active_downloads
        .as_ref()
        .map(|active| active.load(Ordering::Relaxed));
    info!(
        download_id = id.0,
        path = ?state.path,
        bytes_for_http = state.bytes_for_http,
        size = state.size,
        active_downloads,
        elapsed_ms = state.started.elapsed().as_millis() as u64,
        rate_mib_s = state.bytes_for_http as f64 / elapsed / (1024.0 * 1024.0),
        "transfer-link download progress"
    );
}

fn message_body(state: MessageBodyState) -> TransferBody {
    let stream = stream::unfold(state, |mut state| async move {
        if state.complete {
            return None;
        }
        let event = if state.id.is_some() {
            let delay = state.next_report.saturating_duration_since(Instant::now());
            tokio::select! {
                message = state.receiver.recv() => BodyEvent::Message(message),
                _ = tokio::time::sleep(delay) => BodyEvent::Tick,
            }
        } else {
            BodyEvent::Message(state.receiver.recv().await)
        };
        let item = match event {
            BodyEvent::Tick => {
                log_progress(&state);
                state.next_report = Instant::now() + state.stats_interval;
                Ok(Frame::data(Bytes::new()))
            }
            BodyEvent::Message(message) => match message {
                Some(SourceMessage::Data(bytes)) => {
                    state.bytes_for_http = state.bytes_for_http.saturating_add(bytes.len() as u64);
                    if let Some(outcome) = &state.outcome {
                        outcome.add_bytes(bytes.len() as u64);
                    }
                    Ok(Frame::data(bytes))
                }
                Some(SourceMessage::Complete(summary)) => {
                    state.complete = true;
                    if let Some(outcome) = &state.outcome {
                        outcome.source_complete(summary);
                    } else if let Some(oneshot) = &state.oneshot {
                        // This branch is retained for the small public helper
                        // used by callers that do not install a connection owner.
                        oneshot.consumed();
                    }
                    Ok(Frame::data(Bytes::new()))
                }
                Some(SourceMessage::Failed(failure)) => {
                    state.complete = true;
                    if let Some(outcome) = &state.outcome {
                        outcome.source_failed(failure.error.clone());
                    } else if let Some(oneshot) = &state.oneshot {
                        oneshot.failed();
                    }
                    if let Some(id) = state.id {
                        warn!(
                            download_id = id.0,
                            path = ?state.path,
                            bytes_for_http = state.bytes_for_http,
                            size = state.size,
                            elapsed_ms = state.started.elapsed().as_millis() as u64,
                            error = %failure.error,
                            "transfer-link download failed"
                        );
                    }
                    Err(io::Error::other(failure.error))
                }
                None => {
                    state.complete = true;
                    if let Some(outcome) = &state.outcome {
                        outcome.source_failed("source ended without completion");
                    } else if let Some(oneshot) = &state.oneshot {
                        oneshot.failed();
                    }
                    if let Some(id) = state.id {
                        warn!(
                            download_id = id.0,
                            path = ?state.path,
                            bytes_for_http = state.bytes_for_http,
                            size = state.size,
                            elapsed_ms = state.started.elapsed().as_millis() as u64,
                            "transfer-link download failed without source completion"
                        );
                    }
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "source ended without completion",
                    ))
                }
            },
        };
        Some((item, state))
    });
    StreamBody::new(stream).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    fn completion() -> SourceCompletion {
        SourceCompletion {
            bytes_read: 3,
            sha256: [7; 32],
        }
    }

    fn one_shot_outcome() -> (Arc<DownloadOutcome>, Arc<OneShotState>) {
        let oneshot = Arc::new(OneShotState::new());
        oneshot.claim().unwrap();
        let outcome = Arc::new(DownloadOutcome::new(
            DownloadId(1),
            Some(3),
            None,
            Some(Arc::clone(&oneshot)),
        ));
        (outcome, oneshot)
    }

    #[test]
    fn source_completion_is_only_a_candidate_until_connection_success() {
        let (outcome, oneshot) = one_shot_outcome();
        outcome.source_complete(completion());
        assert_eq!(oneshot.status(), OneShotStatus::Streaming);
        outcome.connection_succeeded();
        assert_eq!(oneshot.status(), OneShotStatus::Consumed);
    }

    #[test]
    fn transport_failure_wins_over_source_success() {
        let (outcome, oneshot) = one_shot_outcome();
        outcome.source_complete(completion());
        outcome.connection_failed("peer reset");
        assert_eq!(oneshot.status(), OneShotStatus::Failed);
    }

    #[test]
    fn body_disconnect_wins_when_source_was_ready_to_complete() {
        let (outcome, oneshot) = one_shot_outcome();
        outcome.source_complete(completion());
        // Hyper reports the client disconnect on the connection future even
        // when the body had already observed the producer's final frame.
        outcome.connection_failed("peer reset");
        assert_eq!(oneshot.status(), OneShotStatus::Failed);
    }

    #[tokio::test]
    async fn download_permit_released_on_body_drop() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&semaphore).try_acquire_owned().unwrap();
        let (_sender, receiver) = mpsc::channel(1);
        let body = message_body(MessageBodyState {
            receiver,
            owner: None,
            cancellation: CancellationToken::new(),
            permit: Some(permit),
            complete: false,
            id: None,
            size: None,
            started: Instant::now(),
            next_report: Instant::now() + Duration::from_secs(1),
            bytes_for_http: 0,
            stats_interval: Duration::from_secs(1),
            path: None,
            oneshot: None,
            outcome: None,
            active_downloads: None,
        });
        assert_eq!(semaphore.available_permits(), 0);
        drop(body);
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[tokio::test]
    async fn progress_timer_emits_without_source_data() {
        let (_sender, receiver) = mpsc::channel(1);
        let active_downloads = Arc::new(AtomicUsize::new(1));
        let mut body = message_body(MessageBodyState {
            receiver,
            owner: None,
            cancellation: CancellationToken::new(),
            permit: None,
            complete: false,
            id: Some(DownloadId(1)),
            size: Some(10),
            started: Instant::now(),
            next_report: Instant::now() + Duration::from_millis(10),
            bytes_for_http: 0,
            stats_interval: Duration::from_millis(10),
            path: None,
            oneshot: None,
            outcome: None,
            active_downloads: Some(Arc::clone(&active_downloads)),
        });
        let frame = tokio::time::timeout(Duration::from_millis(250), body.frame())
            .await
            .expect("progress timer did not wake the body")
            .expect("body ended before progress tick")
            .expect("progress frame failed");
        assert_eq!(
            frame.into_data().expect("progress was not a data frame"),
            Bytes::new()
        );
        drop(body);
        assert_eq!(active_downloads.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn closed_producer_without_complete_is_error() {
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(SourceMessage::Data(Bytes::from_static(b"partial")))
            .await
            .unwrap();
        drop(sender);
        let body = body_from_messages(receiver, CancellationToken::new());
        let error = body
            .collect()
            .await
            .expect_err("incomplete producer unexpectedly succeeded");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}
