//! Bounded HTTP/1.1 serving for a transfer-link source.
//!
//! The listener is deliberately loopback-only.  A later vhost client exposes
//! this listener through the existing bore vhost; this module does not add a
//! second public listener or a second routing protocol.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::client::BackendPath;

use super::manifest::PreparedSource;
use super::oneshot::{self, OneShotState, OneShotStatus};
use super::source::{
    content_disposition, open_prepared_file, spawn_file_producer, FileProducerHandle,
    LinkConfigError, LinkOptions, PreparedFile, MIME_OCTET_STREAM,
};
use super::stats::{DownloadId, SourceMessage};
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
    let service =
        service_fn(move |request| handle_request(Arc::clone(&service_state), peer, request));
    let mut builder = http1::Builder::new();
    builder
        .keep_alive(false)
        .max_headers(MAX_HEADERS)
        .max_buf_size(MAX_HTTP_BUFFER)
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    let connection = builder.serve_connection(io, service);
    tokio::select! {
        result = connection => result.map_err(io::Error::other),
        _ = state.shutdown.cancelled() => Ok(()),
    }
}

async fn handle_request(
    state: Arc<HttpState>,
    peer: SocketAddr,
    request: Request<Incoming>,
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
        get_response(state, peer).await
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

async fn get_response(state: Arc<HttpState>, peer: SocketAddr) -> Response<TransferBody> {
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
    debug!(download_id = id, "transfer-link download started");
    let body = body_from_producer(
        producer,
        cancellation,
        Some(permit),
        DownloadId(id),
        size,
        state.options.limits.stats_interval,
        path,
        oneshot,
    );
    // Deliberately omit Content-Length for a streaming GET. Hyper can then
    // poll through SourceMessage::Complete and observe the producer's final
    // fingerprint/hash validation before ending the response. HEAD still
    // advertises the known size through `metadata_response`.
    build_file_response(
        &state,
        body,
        matches!(&state.prepared, PreparedSource::File(_)),
    )
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
        task: None,
        cancellation,
        permit: None,
        complete: false,
        id: None,
        size: None,
        started: Instant::now(),
        last_report: Instant::now(),
        bytes_for_http: 0,
        stats_interval: Duration::from_secs(1),
        path: None,
        completion: None,
        oneshot: None,
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
) -> TransferBody {
    let (receiver, task, completion) = producer.into_parts_with_completion();
    message_body(MessageBodyState {
        receiver,
        task: Some(task),
        cancellation,
        permit,
        complete: false,
        id: Some(id),
        size,
        started: Instant::now(),
        last_report: Instant::now(),
        bytes_for_http: 0,
        stats_interval,
        path,
        completion: Some(completion),
        oneshot,
    })
}

struct MessageBodyState {
    receiver: mpsc::Receiver<SourceMessage>,
    task: Option<JoinHandle<()>>,
    cancellation: CancellationToken,
    permit: Option<OwnedSemaphorePermit>,
    complete: bool,
    id: Option<DownloadId>,
    size: Option<u64>,
    started: Instant,
    last_report: Instant,
    bytes_for_http: u64,
    stats_interval: Duration,
    path: Option<BackendPath>,
    completion: Option<Arc<std::sync::Mutex<Option<super::stats::SourceCompletion>>>>,
    oneshot: Option<Arc<OneShotState>>,
}

impl Drop for MessageBodyState {
    fn drop(&mut self) {
        // A streaming archive/one-shot has no advertised size.  Once its
        // terminal Complete frame was observed, it is just as successful as a
        // sized file that reached its byte boundary.  Treating `size=None` as
        // incomplete here would mark a successful one-shot Failed while the
        // HTTP response was being dropped.
        let fully_sent = self.complete
            || self
                .size
                .map(|size| self.bytes_for_http >= size)
                .unwrap_or(false);
        if fully_sent {
            if !self.complete {
                if let (Some(id), Some(completion)) = (self.id, self.completion.take()) {
                    let path = self.path;
                    let size = self.size;
                    let bytes_for_http = self.bytes_for_http;
                    let started = self.started;
                    tokio::spawn(async move {
                        for _ in 0..100 {
                            if let Some(summary) = completion
                                .lock()
                                .expect("completion mutex poisoned")
                                .clone()
                            {
                                info!(
                                    download_id = id.0,
                                    path = ?path,
                                    bytes_for_http,
                                    size,
                                    elapsed_ms = started.elapsed().as_millis() as u64,
                                    sha256 = %hex::encode(summary.sha256),
                                    "transfer-link download completed"
                                );
                                return;
                            }
                            tokio::task::yield_now().await;
                        }
                        warn!(
                            download_id = id.0,
                            "transfer-link completion metadata unavailable"
                        );
                    });
                }
            }
            // The producer has already emitted every validated data byte.  It
            // may still be placing its terminal Complete message in the small
            // queue; let that task finish instead of aborting it at the HTTP
            // content-length boundary.
            self.task.take();
        } else {
            self.cancellation.cancel();
            if let Some(oneshot) = &self.oneshot {
                oneshot.failed();
            }
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
        self.permit.take();
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

fn message_body(state: MessageBodyState) -> TransferBody {
    let stream = stream::unfold(state, |mut state| async move {
        if state.complete {
            return None;
        }
        let message = state.receiver.recv().await;
        let item = match message {
            Some(SourceMessage::Data(bytes)) => {
                state.bytes_for_http = state.bytes_for_http.saturating_add(bytes.len() as u64);
                if state.last_report.elapsed() >= state.stats_interval {
                    if let Some(id) = state.id {
                        let elapsed = state.started.elapsed().as_secs_f64().max(0.001);
                        info!(
                            download_id = id.0,
                            path = ?state.path,
                            bytes_for_http = state.bytes_for_http,
                            size = state.size,
                            elapsed_ms = state.started.elapsed().as_millis() as u64,
                            rate_mib_s = state.bytes_for_http as f64 / elapsed / (1024.0 * 1024.0),
                            "transfer-link download progress"
                        );
                    }
                    state.last_report = Instant::now();
                }
                Ok(Frame::data(bytes))
            }
            Some(SourceMessage::Complete(summary)) => {
                state.complete = true;
                if let Some(oneshot) = &state.oneshot {
                    oneshot.consumed();
                }
                if let Some(id) = state.id {
                    info!(
                        download_id = id.0,
                        path = ?state.path,
                        bytes_for_http = state.bytes_for_http,
                        size = state.size,
                        elapsed_ms = state.started.elapsed().as_millis() as u64,
                        sha256 = %hex::encode(summary.sha256),
                        "transfer-link download completed"
                    );
                }
                Ok(Frame::data(Bytes::new()))
            }
            Some(SourceMessage::Failed(failure)) => {
                state.complete = true;
                if let Some(oneshot) = &state.oneshot {
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
                if let Some(oneshot) = &state.oneshot {
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
        };
        Some((item, state))
    });
    StreamBody::new(stream).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn download_permit_released_on_body_drop() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&semaphore).try_acquire_owned().unwrap();
        let (_sender, receiver) = mpsc::channel(1);
        let body = message_body(MessageBodyState {
            receiver,
            task: None,
            cancellation: CancellationToken::new(),
            permit: Some(permit),
            complete: false,
            id: None,
            size: None,
            started: Instant::now(),
            last_report: Instant::now(),
            bytes_for_http: 0,
            stats_interval: Duration::from_secs(1),
            path: None,
            completion: None,
            oneshot: None,
        });
        assert_eq!(semaphore.available_permits(), 0);
        drop(body);
        assert_eq!(semaphore.available_permits(), 1);
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
