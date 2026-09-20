//! Source names, limits and metadata contracts for transfer links.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use ring::digest::{Context, SHA256};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, Notify};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use super::stats::{SourceCompletion, SourceFailure, SourceMessage};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// MIME type used for a regular file or an untyped stream.
pub const MIME_OCTET_STREAM: &str = "application/octet-stream";

/// Default payload chunk size used by the bounded file producer.
///
/// 1 MiB keeps each producer bounded while giving QUIC enough in-flight
/// application data to avoid a tiny stream window becoming the throughput
/// limiter on low-latency links.
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

/// Default number of payload chunks retained by the producer channel.
pub const DEFAULT_QUEUE_CAPACITY: usize = 4;

/// Largest queue capacity accepted by the producer options.
pub const MAX_QUEUE_CAPACITY: usize = 4;

/// Maximum encoded filename input accepted by the link API, in bytes.
pub const MAX_FILENAME_BYTES: usize = 255;

/// Default number of simultaneous file downloads.
pub const DEFAULT_MAX_DOWNLOADS: usize = 8;

/// Upper bound for the simultaneous download setting.
pub const MAX_DOWNLOADS: usize = 256;

/// Minimum progress-report interval in seconds.
pub const MIN_STATS_INTERVAL_SECS: u64 = 1;

/// Maximum progress-report interval in seconds.
pub const MAX_STATS_INTERVAL_SECS: u64 = 60;

/// Default progress-report interval.
pub const DEFAULT_STATS_INTERVAL: Duration = Duration::from_secs(1);

/// The regular-file source kind used by the bounded file producer.
///
/// Directory selections and one-shot streams have their own prepared-source
/// types in the manifest and one-shot modules; this enum remains the compact
/// fingerprint tag for regular files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    /// A regular file selected by path.
    File,
}

/// A stable identity and metadata snapshot for a source file.
///
/// Unix identity fields are optional so the same public contract works on
/// platforms that do not expose device, inode or change-time metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFingerprint {
    /// Kind of source represented by this fingerprint.
    pub kind: SourceKind,
    /// File size observed when the fingerprint was collected.
    pub size: u64,
    /// Modification time, when the filesystem provides one.
    pub modified: Option<SystemTime>,
    /// Unix device number, when available.
    pub device: Option<u64>,
    /// Unix inode number, when available.
    pub inode: Option<u64>,
    /// Unix change time as `(seconds, nanoseconds)`, when available.
    pub changed: Option<(i64, u32)>,
}

impl SourceFingerprint {
    /// Construct a fingerprint from the portable fields.
    pub fn new(size: u64, modified: Option<SystemTime>) -> Self {
        Self {
            kind: SourceKind::File,
            size,
            modified,
            device: None,
            inode: None,
            changed: None,
        }
    }

    /// Collect the identity fields available from filesystem metadata.
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let (device, inode, changed) = (
            Some(metadata.dev()),
            Some(metadata.ino()),
            Some((metadata.ctime(), metadata.ctime_nsec() as u32)),
        );
        #[cfg(not(unix))]
        let (device, inode, changed) = (None, None, None);
        Self {
            kind: SourceKind::File,
            size: metadata.len(),
            modified: metadata.modified().ok(),
            device,
            inode,
            changed,
        }
    }

    fn matches(&self, other: &Self) -> bool {
        self == other
    }
}

/// A file selected for a transfer-link session.
///
/// This value contains only the path and metadata.  It never contains file
/// contents and therefore does not create a local spool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedFile {
    /// Path opened for each independent download.
    pub path: PathBuf,
    /// Validated basename advertised to HTTP clients.
    pub filename: String,
    /// Initial size observed for the source.
    pub size: u64,
    /// Initial source identity and metadata.
    pub fingerprint: SourceFingerprint,
}

impl PreparedFile {
    /// Create a prepared-file descriptor after the caller has collected its
    /// filesystem metadata.
    pub fn new(
        path: PathBuf,
        filename: String,
        size: u64,
        fingerprint: SourceFingerprint,
    ) -> Result<Self, FilenameError> {
        validate_filename(&filename)?;
        Ok(Self {
            path,
            filename,
            size,
            fingerprint,
        })
    }

    /// Prepare a regular file without converting its operating-system path
    /// through a lossy string representation.
    pub async fn from_path(
        path: impl Into<PathBuf>,
        override_filename: Option<&str>,
    ) -> Result<Self, SourceError> {
        prepare_file(path, override_filename).await
    }
}

/// An optional deterministic gate used by tests to modify a source between
/// reading and final validation.  Normal producers leave it unset.
#[derive(Clone)]
pub struct ValidationGate {
    reached: Arc<Notify>,
    release: Arc<Notify>,
}

impl ValidationGate {
    /// Create a gate initially closed to the producer.
    pub fn new() -> Self {
        Self {
            reached: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        }
    }

    /// Wait until the producer has read the initial source bytes.
    pub async fn wait_until_reached(&self) {
        self.reached.notified().await;
    }

    /// Permit the producer to run its final source checks.
    pub fn release(&self) {
        self.release.notify_one();
    }

    async fn producer_wait(&self, cancellation: &CancellationToken) -> Result<(), SourceError> {
        self.reached.notify_one();
        tokio::select! {
            _ = self.release.notified() => Ok(()),
            _ = cancellation.cancelled() => Err(SourceError::Cancelled),
        }
    }
}

impl Default for ValidationGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Tunable bounds for a file producer.  Production uses [`Default`]; the
/// smaller chunk size is useful for deterministic unit tests.
pub struct FileProducerOptions {
    /// Payload size of each [`SourceMessage::Data`] chunk.
    pub chunk_size: usize,
    /// Capacity of the producer-to-body channel.
    pub queue_capacity: usize,
    /// Optional test-only validation gate.
    pub validation_gate: Option<ValidationGate>,
}

impl Default for FileProducerOptions {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            validation_gate: None,
        }
    }
}

impl FileProducerOptions {
    /// Construct and validate bounded producer settings.
    pub fn new(chunk_size: usize, queue_capacity: usize) -> Result<Self, SourceError> {
        if chunk_size == 0 || chunk_size > DEFAULT_CHUNK_SIZE {
            return Err(SourceError::InvalidOptions {
                detail: format!("chunk size must be between 1 and {DEFAULT_CHUNK_SIZE} bytes"),
            });
        }
        if !(1..=MAX_QUEUE_CAPACITY).contains(&queue_capacity) {
            return Err(SourceError::InvalidOptions {
                detail: format!("queue capacity must be between 1 and {MAX_QUEUE_CAPACITY} chunks"),
            });
        }
        Ok(Self {
            chunk_size,
            queue_capacity,
            validation_gate: None,
        })
    }
}

/// Errors returned while preparing or producing a source file.
#[derive(Debug)]
pub enum SourceError {
    /// Filesystem operation failed.
    Io {
        /// Operation being attempted.
        operation: &'static str,
        /// Underlying error text.
        message: String,
    },
    /// The selected path is a symbolic link.
    Symlink,
    /// The selected path is not a regular file.
    NotRegular,
    /// File identity or metadata changed during the read.
    Changed,
    /// The file ended before its prepared size was read.
    UnexpectedEof {
        /// Number of bytes promised by the prepared metadata.
        expected: u64,
        /// Number of bytes obtained before EOF.
        read: u64,
    },
    /// The file contained bytes beyond its prepared size.
    Grew,
    /// The byte count could not be represented by a u64.
    SizeOverflow,
    /// Production was cancelled by the owning session.
    Cancelled,
    /// The HTTP body went away before the producer completed.
    ReceiverClosed,
    /// Producer options were outside the bounded contract.
    InvalidOptions {
        /// Human-readable explanation of the rejected option.
        detail: String,
    },
    /// A blocking open task failed to join.
    Task(String),
    /// A prepared multi-path manifest became unavailable.
    Manifest {
        /// Human-readable validation detail.
        detail: String,
    },
    /// An `--exec` producer exited unsuccessfully after emitting stdout.
    ExecFailed {
        /// Human-readable process status.
        detail: String,
    },
}

impl SourceError {
    pub(crate) fn io(operation: &'static str, error: std::io::Error) -> Self {
        Self::Io {
            operation,
            message: error.to_string(),
        }
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, message } => write!(f, "{operation} failed: {message}"),
            Self::Symlink => f.write_str("source path is a symbolic link"),
            Self::NotRegular => f.write_str("source path is not a regular file"),
            Self::Changed => f.write_str("source file changed during preparation or read"),
            Self::UnexpectedEof { expected, read } => {
                write!(f, "source ended at {read} bytes; expected {expected}")
            }
            Self::Grew => f.write_str("source grew while it was being downloaded"),
            Self::SizeOverflow => f.write_str("source byte count overflowed"),
            Self::Cancelled => f.write_str("source producer cancelled"),
            Self::ReceiverClosed => f.write_str("download body closed before source completion"),
            Self::InvalidOptions { detail } => f.write_str(detail),
            Self::Task(detail) => write!(f, "source task failed: {detail}"),
            Self::Manifest { detail } => write!(f, "source manifest unavailable: {detail}"),
            Self::ExecFailed { detail } => write!(f, "producer failed: {detail}"),
        }
    }
}

impl std::error::Error for SourceError {}

/// Prepare a regular source file and verify the opened handle matches its
/// initial path metadata.
pub async fn prepare_file(
    path: impl Into<PathBuf>,
    override_filename: Option<&str>,
) -> Result<PreparedFile, SourceError> {
    let path = path.into();
    let metadata = tokio::fs::symlink_metadata(&path)
        .await
        .map_err(|error| SourceError::io("stat source", error))?;
    if metadata.file_type().is_symlink() {
        return Err(SourceError::Symlink);
    }
    if !metadata.is_file() {
        return Err(SourceError::NotRegular);
    }
    let filename = filename_from_path(&path, override_filename).map_err(|error| {
        SourceError::InvalidOptions {
            detail: error.to_string(),
        }
    })?;
    let fingerprint = SourceFingerprint::from_metadata(&metadata);
    let file = open_file_no_follow(&path).await?;
    let opened = file
        .metadata()
        .await
        .map_err(|error| SourceError::io("stat opened source", error))?;
    if !opened.is_file() || !fingerprint.matches(&SourceFingerprint::from_metadata(&opened)) {
        return Err(SourceError::Changed);
    }
    Ok(PreparedFile {
        path,
        filename,
        size: fingerprint.size,
        fingerprint,
    })
}

/// Open a prepared file with no-follow semantics where the platform exposes
/// them, then verify the handle still has the prepared identity.
pub async fn open_prepared_file(prepared: &PreparedFile) -> Result<File, SourceError> {
    let path_metadata = tokio::fs::symlink_metadata(&prepared.path)
        .await
        .map_err(|error| SourceError::io("stat source", error))?;
    if path_metadata.file_type().is_symlink() {
        return Err(SourceError::Symlink);
    }
    if !path_metadata.is_file()
        || !prepared
            .fingerprint
            .matches(&SourceFingerprint::from_metadata(&path_metadata))
    {
        return Err(SourceError::Changed);
    }
    let file = open_file_no_follow(&prepared.path).await?;
    let opened = file
        .metadata()
        .await
        .map_err(|error| SourceError::io("stat opened source", error))?;
    if !opened.is_file()
        || !prepared
            .fingerprint
            .matches(&SourceFingerprint::from_metadata(&opened))
    {
        return Err(SourceError::Changed);
    }
    Ok(file)
}

async fn open_file_no_follow(path: &Path) -> Result<File, SourceError> {
    let path = path.to_owned();
    let opened = tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
                .open(path)
        }
        #[cfg(not(unix))]
        {
            std::fs::OpenOptions::new().read(true).open(path)
        }
    })
    .await
    .map_err(|error| SourceError::Task(error.to_string()))?
    .map_err(|error| SourceError::io("open source", error))?;
    Ok(File::from_std(opened))
}

/// The non-abortable owner for a source producer task.
///
/// The HTTP body may be dropped while a producer still owns process resources.
/// Cancellation is cooperative; the task is joined by the HTTP connection
/// supervisor after the connection outcome is known.  Dropping this owner
/// never calls `JoinHandle::abort`, because that would bypass process-group
/// teardown in the `--exec` producer.
pub(crate) struct ProducerTaskOwner {
    task: Mutex<Option<JoinHandle<()>>>,
    cancellation: CancellationToken,
}

impl ProducerTaskOwner {
    fn new(task: JoinHandle<()>, cancellation: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            task: Mutex::new(Some(task)),
            cancellation,
        })
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub(crate) async fn join(&self) -> Result<(), JoinError> {
        let task = self
            .task
            .lock()
            .expect("producer task mutex poisoned")
            .take();
        match task {
            Some(task) => task.await,
            None => Ok(()),
        }
    }

    fn into_task(self) -> JoinHandle<()> {
        self.task
            .into_inner()
            .expect("producer task mutex poisoned")
            .expect("producer task owned by handle")
    }
}

/// A bounded handle owning one source producer task and its receiver.
pub struct FileProducerHandle {
    receiver: mpsc::Receiver<SourceMessage>,
    owner: Option<Arc<ProducerTaskOwner>>,
    completion: Arc<std::sync::Mutex<Option<SourceCompletion>>>,
}

impl FileProducerHandle {
    pub(crate) fn from_parts_with_completion(
        receiver: mpsc::Receiver<SourceMessage>,
        task: JoinHandle<()>,
        completion: Arc<std::sync::Mutex<Option<SourceCompletion>>>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            receiver,
            owner: Some(ProducerTaskOwner::new(task, cancellation)),
            completion,
        }
    }

    /// Receive the next bounded source message.
    pub async fn recv(&mut self) -> Option<SourceMessage> {
        self.receiver.recv().await
    }

    /// Try to receive a message without waiting.
    pub fn try_recv(&mut self) -> Result<SourceMessage, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Wait for the producer task after the receiver has been consumed.
    pub async fn join(mut self) -> Result<(), JoinError> {
        self.owner
            .take()
            .expect("producer task owned by handle")
            .join()
            .await
    }

    /// Transfer receiver and task ownership to a caller that supervises both.
    pub fn into_parts(mut self) -> (mpsc::Receiver<SourceMessage>, JoinHandle<()>) {
        let receiver = std::mem::replace(&mut self.receiver, mpsc::channel(1).1);
        let owner = Arc::try_unwrap(self.owner.take().expect("producer task owned by handle"))
            .unwrap_or_else(|_| panic!("producer task has another supervisor"));
        (receiver, owner.into_task())
    }

    pub(crate) fn into_parts_with_completion(
        mut self,
    ) -> (
        mpsc::Receiver<SourceMessage>,
        Arc<ProducerTaskOwner>,
        Arc<std::sync::Mutex<Option<SourceCompletion>>>,
    ) {
        let receiver = std::mem::replace(&mut self.receiver, mpsc::channel(1).1);
        (
            receiver,
            self.owner.take().expect("producer task owned by handle"),
            Arc::clone(&self.completion),
        )
    }
}

impl Drop for FileProducerHandle {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.as_ref() {
            owner.cancel();
        }
    }
}

/// Spawn a producer with the production bounds.
pub fn spawn_file_producer(
    prepared: PreparedFile,
    cancellation: CancellationToken,
) -> FileProducerHandle {
    spawn_file_producer_inner(prepared, cancellation, FileProducerOptions::default())
}

/// Spawn a producer with explicit bounded settings, primarily for tests.
pub fn spawn_file_producer_with_options(
    prepared: PreparedFile,
    cancellation: CancellationToken,
    options: FileProducerOptions,
) -> Result<FileProducerHandle, SourceError> {
    let validated = FileProducerOptions::new(options.chunk_size, options.queue_capacity)?;
    Ok(spawn_file_producer_inner(
        prepared,
        cancellation,
        FileProducerOptions {
            validation_gate: options.validation_gate,
            ..validated
        },
    ))
}

fn spawn_file_producer_inner(
    prepared: PreparedFile,
    cancellation: CancellationToken,
    options: FileProducerOptions,
) -> FileProducerHandle {
    let (sender, receiver) = mpsc::channel(options.queue_capacity);
    let completion = Arc::new(std::sync::Mutex::new(None));
    let task_completion = Arc::clone(&completion);
    let owner_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        run_file_producer(prepared, cancellation, options, sender, task_completion).await;
    });
    FileProducerHandle::from_parts_with_completion(receiver, task, completion, owner_cancellation)
}

async fn run_file_producer(
    prepared: PreparedFile,
    cancellation: CancellationToken,
    options: FileProducerOptions,
    sender: mpsc::Sender<SourceMessage>,
    completion_cell: Arc<std::sync::Mutex<Option<SourceCompletion>>>,
) {
    match produce_file(&prepared, &cancellation, &options, &sender).await {
        Ok(completion) => {
            *completion_cell.lock().expect("completion mutex poisoned") = Some(completion.clone());
            let _ = send_message(&sender, SourceMessage::Complete(completion), &cancellation).await;
        }
        Err((bytes_read, error)) => {
            let _ = sender.try_send(SourceMessage::Failed(SourceFailure {
                bytes_read,
                error: error.to_string(),
            }));
        }
    }
}

async fn produce_file(
    prepared: &PreparedFile,
    cancellation: &CancellationToken,
    options: &FileProducerOptions,
    sender: &mpsc::Sender<SourceMessage>,
) -> Result<SourceCompletion, (u64, SourceError)> {
    let mut file = open_prepared_file(prepared)
        .await
        .map_err(|error| (0, error))?;
    let mut hasher = Context::new(&SHA256);
    let mut bytes_read = 0u64;
    let mut pending: Option<Bytes> = None;
    let mut buffer = vec![0u8; options.chunk_size];

    while bytes_read < prepared.size {
        let remaining = prepared.size - bytes_read;
        let read_len = remaining.min(options.chunk_size as u64) as usize;
        let read = read_with_cancel(&mut file, &mut buffer[..read_len], cancellation)
            .await
            .map_err(|error| (bytes_read, error))?;
        if read == 0 {
            return Err((
                bytes_read,
                SourceError::UnexpectedEof {
                    expected: prepared.size,
                    read: bytes_read,
                },
            ));
        }
        bytes_read = bytes_read
            .checked_add(read as u64)
            .ok_or((bytes_read, SourceError::SizeOverflow))?;
        let current = Bytes::copy_from_slice(&buffer[..read]);
        if let Some(previous) = pending.replace(current) {
            send_message(sender, SourceMessage::Data(previous), cancellation)
                .await
                .map_err(|error| (bytes_read, error))?;
        }
        // Hash after handing the previous chunk to the bounded queue.  This
        // lets the HTTP/transport consumer make progress while the integrity
        // work for the next chunk runs, without retaining an unbounded copy.
        hasher.update(&buffer[..read]);
    }

    let mut extra = [0u8; 1];
    let extra_read = read_with_cancel(&mut file, &mut extra, cancellation)
        .await
        .map_err(|error| (bytes_read, error))?;
    if extra_read != 0 {
        return Err((bytes_read, SourceError::Grew));
    }

    if let Some(gate) = &options.validation_gate {
        gate.producer_wait(cancellation)
            .await
            .map_err(|error| (bytes_read, error))?;
    }

    let handle_metadata = file
        .metadata()
        .await
        .map_err(|error| (bytes_read, SourceError::io("stat source handle", error)))?;
    let path_metadata = tokio::fs::symlink_metadata(&prepared.path)
        .await
        .map_err(|error| (bytes_read, SourceError::io("stat source path", error)))?;
    if path_metadata.file_type().is_symlink()
        || !handle_metadata.is_file()
        || !prepared
            .fingerprint
            .matches(&SourceFingerprint::from_metadata(&handle_metadata))
        || !prepared
            .fingerprint
            .matches(&SourceFingerprint::from_metadata(&path_metadata))
    {
        return Err((bytes_read, SourceError::Changed));
    }

    if let Some(last) = pending {
        send_message(sender, SourceMessage::Data(last), cancellation)
            .await
            .map_err(|error| (bytes_read, error))?;
    }
    Ok(SourceCompletion {
        bytes_read,
        sha256: hasher
            .finish()
            .as_ref()
            .try_into()
            .expect("SHA-256 is 32 bytes"),
    })
}

async fn read_with_cancel(
    file: &mut File,
    buffer: &mut [u8],
    cancellation: &CancellationToken,
) -> Result<usize, SourceError> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(SourceError::Cancelled),
        result = file.read(buffer) => result.map_err(|error| SourceError::io("read source", error)),
    }
}

async fn send_message(
    sender: &mpsc::Sender<SourceMessage>,
    message: SourceMessage,
    cancellation: &CancellationToken,
) -> Result<(), SourceError> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(SourceError::Cancelled),
        result = sender.send(message) => result.map_err(|_| SourceError::ReceiverClosed),
    }
}

/// Validated settings shared by the CLI and the transfer-link engine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkLimits {
    /// Maximum number of body downloads active at once.
    pub max_downloads: usize,
    /// Interval between progress reports.
    pub stats_interval: Duration,
}

impl Default for LinkLimits {
    fn default() -> Self {
        Self {
            max_downloads: DEFAULT_MAX_DOWNLOADS,
            stats_interval: DEFAULT_STATS_INTERVAL,
        }
    }
}

impl LinkLimits {
    /// Validate limits supplied by a caller or command-line parser.
    pub fn validate(&self) -> Result<(), LinkConfigError> {
        validate_limits(self.max_downloads, self.stats_interval)
    }

    /// Build and validate a limits value.
    pub fn new(max_downloads: usize, stats_interval: Duration) -> Result<Self, LinkConfigError> {
        let limits = Self {
            max_downloads,
            stats_interval,
        };
        limits.validate()?;
        Ok(limits)
    }
}

/// Inputs needed by the source/HTTP layers before CLI-specific wiring.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkOptions {
    /// Validated display name used in the URL and Content-Disposition.
    pub filename: String,
    /// Percent-encoded URL path segment for `filename`.
    pub path_segment: String,
    /// Bounded download and progress settings.
    pub limits: LinkLimits,
}

impl LinkOptions {
    /// Validate a filename and construct its encoded path segment.
    pub fn new(
        filename: impl Into<String>,
        max_downloads: usize,
        stats_interval: Duration,
    ) -> Result<Self, LinkConfigError> {
        let filename = filename.into();
        validate_filename(&filename).map_err(LinkConfigError::Filename)?;
        let limits = LinkLimits::new(max_downloads, stats_interval)?;
        let path_segment = encode_path_segment(&filename);
        Ok(Self {
            filename,
            path_segment,
            limits,
        })
    }
}

/// Errors raised while validating a filename.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilenameError {
    /// The name is empty.
    Empty,
    /// The name exceeds [`MAX_FILENAME_BYTES`].
    TooLong {
        /// Number of bytes supplied by the caller.
        bytes: usize,
    },
    /// The name is a path component with a forbidden separator.
    Separator,
    /// The name contains a control character.
    ControlCharacter,
    /// `.` and `..` are not downloadable filenames.
    DotComponent,
    /// The source path has no basename.
    MissingBasename,
    /// The operating-system basename is not valid UTF-8 and no override was supplied.
    NonUtf8,
}

impl fmt::Display for FilenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("filename must not be empty"),
            Self::TooLong { bytes } => write!(
                f,
                "filename is {bytes} bytes; the maximum is {MAX_FILENAME_BYTES}"
            ),
            Self::Separator => f.write_str("filename must be one path component"),
            Self::ControlCharacter => f.write_str("filename contains a control character"),
            Self::DotComponent => f.write_str("filename must not be . or .."),
            Self::MissingBasename => f.write_str("source path has no filename component"),
            Self::NonUtf8 => f.write_str(
                "source filename is not valid UTF-8; provide an explicit --filename value",
            ),
        }
    }
}

impl std::error::Error for FilenameError {}

/// Errors raised while validating bounded link settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LinkConfigError {
    /// The display filename is invalid.
    Filename(FilenameError),
    /// The maximum-download count is outside the safe range.
    MaxDownloads {
        /// Invalid configured value.
        value: usize,
    },
    /// The progress interval is outside the safe range.
    StatsInterval {
        /// Invalid whole-second value (subsecond inputs are reported as zero or their seconds).
        seconds: u64,
    },
}

impl fmt::Display for LinkConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Filename(error) => error.fmt(f),
            Self::MaxDownloads { value } => write!(
                f,
                "max-downloads must be between 1 and {MAX_DOWNLOADS} (got {value})"
            ),
            Self::StatsInterval { seconds } => write!(
                f,
                "stats interval must be between {MIN_STATS_INTERVAL_SECS} and {MAX_STATS_INTERVAL_SECS} seconds (got {seconds})"
            ),
        }
    }
}

impl std::error::Error for LinkConfigError {}

/// Validate a single UTF-8 path component used as a download filename.
pub fn validate_filename(filename: &str) -> Result<(), FilenameError> {
    if filename.is_empty() {
        return Err(FilenameError::Empty);
    }
    let bytes = filename.len();
    if bytes > MAX_FILENAME_BYTES {
        return Err(FilenameError::TooLong { bytes });
    }
    if filename == "." || filename == ".." {
        return Err(FilenameError::DotComponent);
    }
    if filename.chars().any(|character| character.is_control()) {
        return Err(FilenameError::ControlCharacter);
    }
    if filename
        .chars()
        .any(|character| matches!(character, '/' | '\\'))
    {
        return Err(FilenameError::Separator);
    }
    Ok(())
}

/// Select a safe UTF-8 display name from an operating-system path.
///
/// The path itself is retained as a [`PathBuf`] by [`PreparedFile`]; only its
/// basename is converted for protocol headers.  A non-UTF-8 basename therefore
/// fails explicitly instead of being silently rewritten with replacement
/// characters.
pub fn filename_from_path(
    path: &Path,
    override_filename: Option<&str>,
) -> Result<String, FilenameError> {
    if let Some(filename) = override_filename {
        validate_filename(filename)?;
        return Ok(filename.to_owned());
    }
    let basename = path.file_name().ok_or(FilenameError::MissingBasename)?;
    let filename = basename.to_str().ok_or(FilenameError::NonUtf8)?;
    validate_filename(filename)?;
    Ok(filename.to_owned())
}

/// Percent-encode one URL path segment using UTF-8 bytes.
pub fn encode_path_segment(filename: &str) -> String {
    let mut encoded = String::with_capacity(filename.len());
    for byte in filename.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(*byte as char);
        } else {
            encoded.push('%');
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0f));
        }
    }
    encoded
}

/// Build a safe attachment disposition with both ASCII fallback and UTF-8 name.
pub fn content_disposition(filename: &str) -> Result<String, FilenameError> {
    validate_filename(filename)?;
    let mut fallback = String::with_capacity(filename.len());
    for character in filename.chars() {
        if character.is_ascii()
            && (character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    ' ' | '!' | '#' | '$' | '&' | '+' | '-' | '.' | '^' | '_' | '`' | '|' | '~'
                ))
        {
            fallback.push(character);
        } else {
            fallback.push('_');
        }
    }
    if fallback.is_empty() {
        fallback.push_str("download");
    }
    Ok(format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        encode_path_segment(filename)
    ))
}

/// Validate all bounded settings in one place.
pub fn validate_limits(
    max_downloads: usize,
    stats_interval: Duration,
) -> Result<(), LinkConfigError> {
    if !(1..=MAX_DOWNLOADS).contains(&max_downloads) {
        return Err(LinkConfigError::MaxDownloads {
            value: max_downloads,
        });
    }
    let seconds = stats_interval.as_secs();
    if stats_interval.is_zero()
        || stats_interval.subsec_nanos() != 0
        || !(MIN_STATS_INTERVAL_SECS..=MAX_STATS_INTERVAL_SECS).contains(&seconds)
    {
        return Err(LinkConfigError::StatsInterval { seconds });
    }
    Ok(())
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + nibble - 10) as char,
        _ => unreachable!("hex nibble is always four bits"),
    }
}
