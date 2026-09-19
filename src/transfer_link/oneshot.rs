//! Single-consumer stream sources used by `transfer link --stdin` and
//! `transfer link --exec`.
//!
//! A stream has no seek position and no replay buffer.  The source therefore
//! owns an atomic state machine and can be claimed by exactly one GET.  The
//! HTTP body marks the claim consumed only after it observes the producer's
//! terminal `Complete` message; a disconnect before that point marks it failed.

use std::ffi::OsString;
use std::io::Read;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
#[cfg(unix)]
use tracing::{debug, warn};

use super::source::{validate_filename, FileProducerHandle, SourceError};
use super::stats::{SourceCompletion, SourceFailure, SourceMessage};

const STREAM_CHUNK: usize = 256 * 1024;
const STREAM_QUEUE: usize = 2;
#[cfg(unix)]
const STDERR_CHUNK: usize = 16 * 1024;
#[cfg(unix)]
const STDERR_LOG_LIMIT: usize = 64 * 1024;

/// State of a one-shot stream source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OneShotStatus {
    /// No GET has claimed the source.
    Ready,
    /// A GET owns the source and is still receiving bytes.
    Streaming,
    /// The GET reached a clean producer completion.
    Consumed,
    /// The producer or the HTTP body failed; the source cannot be retried.
    Failed,
}

impl OneShotStatus {
    fn as_u8(self) -> u8 {
        match self {
            Self::Ready => 0,
            Self::Streaming => 1,
            Self::Consumed => 2,
            Self::Failed => 3,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Ready,
            1 => Self::Streaming,
            2 => Self::Consumed,
            _ => Self::Failed,
        }
    }
}

/// Atomic ownership state shared by the CLI preparation, HTTP handler and body.
#[derive(Debug)]
pub(crate) struct OneShotState {
    status: AtomicU8,
}

impl OneShotState {
    pub(crate) fn new() -> Self {
        Self {
            status: AtomicU8::new(OneShotStatus::Ready.as_u8()),
        }
    }

    pub(crate) fn status(&self) -> OneShotStatus {
        OneShotStatus::from_u8(self.status.load(Ordering::Acquire))
    }

    pub(crate) fn claim(&self) -> Result<(), OneShotStatus> {
        self.status
            .compare_exchange(
                OneShotStatus::Ready.as_u8(),
                OneShotStatus::Streaming.as_u8(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(OneShotStatus::from_u8)
    }

    pub(crate) fn consumed(&self) {
        self.status
            .store(OneShotStatus::Consumed.as_u8(), Ordering::Release);
    }

    pub(crate) fn failed(&self) {
        self.status
            .store(OneShotStatus::Failed.as_u8(), Ordering::Release);
    }
}

/// Producer kind retained by a prepared one-shot source.
#[derive(Clone, Debug)]
pub(crate) enum StreamKind {
    /// Read bytes from the bore process's standard input.
    Stdin,
    /// Execute the literal argv vector without a shell (Unix only).
    Exec(Arc<Vec<OsString>>),
}

/// Prepared one-shot source.  It has metadata only; stdin and child stdout are
/// opened after the first valid GET claims the state.
#[derive(Clone, Debug)]
pub struct PreparedStream {
    pub(crate) filename: String,
    pub(crate) state: Arc<OneShotState>,
    pub(crate) kind: StreamKind,
}

/// Prepare a one-shot source backed by the bore process's standard input.
pub fn prepare_stdin(filename: String) -> Result<PreparedStream, SourceError> {
    validate_filename(&filename).map_err(|error| SourceError::Manifest {
        detail: error.to_string(),
    })?;
    Ok(PreparedStream {
        filename,
        state: Arc::new(OneShotState::new()),
        kind: StreamKind::Stdin,
    })
}

/// Prepare a one-shot source backed by a literal Unix command argv vector.
pub fn prepare_exec(
    filename: String,
    command: Vec<OsString>,
) -> Result<PreparedStream, SourceError> {
    validate_filename(&filename).map_err(|error| SourceError::Manifest {
        detail: error.to_string(),
    })?;
    if command.is_empty() {
        return Err(SourceError::Manifest {
            detail: "--exec requires a non-empty command after --".to_owned(),
        });
    }
    Ok(PreparedStream {
        filename,
        state: Arc::new(OneShotState::new()),
        kind: StreamKind::Exec(Arc::new(command)),
    })
}

/// Start the selected producer after its state has been claimed.
pub(crate) async fn start(
    kind: &StreamKind,
    cancellation: CancellationToken,
) -> Result<FileProducerHandle, SourceError> {
    match kind {
        StreamKind::Stdin => spawn_stdin(cancellation),
        StreamKind::Exec(command) => spawn_exec(command, cancellation).await,
    }
}

enum StdinChunk {
    Data(Bytes),
    Eof,
    Error(String),
}

fn spawn_stdin(cancellation: CancellationToken) -> Result<FileProducerHandle, SourceError> {
    let (raw_sender, raw_receiver) = mpsc::channel(STREAM_QUEUE);
    std::thread::Builder::new()
        .name("bore-transfer-stdin".to_owned())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            let mut buffer = vec![0u8; STREAM_CHUNK];
            loop {
                match input.read(&mut buffer) {
                    Ok(0) => {
                        let _ = raw_sender.blocking_send(StdinChunk::Eof);
                        return;
                    }
                    Ok(read) => {
                        if raw_sender
                            .blocking_send(StdinChunk::Data(Bytes::copy_from_slice(
                                &buffer[..read],
                            )))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = raw_sender.blocking_send(StdinChunk::Error(error.to_string()));
                        return;
                    }
                }
            }
        })
        .map_err(|error| SourceError::Task(format!("spawn stdin reader: {error}")))?;

    Ok(spawn_message_producer(cancellation, raw_receiver))
}

fn spawn_message_producer(
    cancellation: CancellationToken,
    mut raw_receiver: mpsc::Receiver<StdinChunk>,
) -> FileProducerHandle {
    let (sender, receiver) = mpsc::channel(STREAM_QUEUE);
    let completion = Arc::new(std::sync::Mutex::new(None));
    let task_completion = Arc::clone(&completion);
    let task = tokio::spawn(async move {
        let mut hasher = Sha256::new();
        let mut bytes_read = 0u64;
        loop {
            let message = tokio::select! {
                _ = cancellation.cancelled() => return,
                message = raw_receiver.recv() => message,
            };
            match message {
                Some(StdinChunk::Data(bytes)) => {
                    bytes_read = match bytes_read.checked_add(bytes.len() as u64) {
                        Some(total) => total,
                        None => {
                            send_failure(&sender, bytes_read, SourceError::SizeOverflow).await;
                            return;
                        }
                    };
                    hasher.update(&bytes);
                    if send_source(&sender, SourceMessage::Data(bytes), &cancellation)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Some(StdinChunk::Eof) => {
                    let completion = SourceCompletion {
                        bytes_read,
                        sha256: hasher.finalize().into(),
                    };
                    *task_completion.lock().expect("completion mutex poisoned") =
                        Some(completion.clone());
                    let _ =
                        send_source(&sender, SourceMessage::Complete(completion), &cancellation)
                            .await;
                    return;
                }
                Some(StdinChunk::Error(error)) => {
                    send_failure(
                        &sender,
                        bytes_read,
                        SourceError::io("read transfer-link stdin", std::io::Error::other(error)),
                    )
                    .await;
                    return;
                }
                None => {
                    send_failure(
                        &sender,
                        bytes_read,
                        SourceError::UnexpectedEof {
                            expected: bytes_read.saturating_add(1),
                            read: bytes_read,
                        },
                    )
                    .await;
                    return;
                }
            }
        }
    });
    FileProducerHandle::from_parts_with_completion(receiver, task, completion)
}

async fn send_source(
    sender: &mpsc::Sender<SourceMessage>,
    message: SourceMessage,
    cancellation: &CancellationToken,
) -> Result<(), ()> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(()),
        result = sender.send(message) => result.map_err(|_| ()),
    }
}

async fn send_failure(sender: &mpsc::Sender<SourceMessage>, bytes_read: u64, error: SourceError) {
    let _ = sender
        .send(SourceMessage::Failed(SourceFailure {
            bytes_read,
            error: error.to_string(),
        }))
        .await;
}

#[cfg(unix)]
async fn spawn_exec(
    command: &Arc<Vec<OsString>>,
    cancellation: CancellationToken,
) -> Result<FileProducerHandle, SourceError> {
    use std::process::Stdio;
    use tokio::process::Command;

    let mut child_command = Command::new(&command[0]);
    child_command
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = child_command
        .spawn()
        .map_err(|error| SourceError::io("spawn transfer-link producer", error))?;
    let stdout = child.stdout.take().ok_or_else(|| SourceError::Manifest {
        detail: "producer stdout was not piped".to_owned(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| SourceError::Manifest {
        detail: "producer stderr was not piped".to_owned(),
    })?;
    let pid = child.id();
    let (sender, receiver) = mpsc::channel(STREAM_QUEUE);
    let completion = Arc::new(std::sync::Mutex::new(None));
    let task_completion = Arc::clone(&completion);
    let task = tokio::spawn(async move {
        let result = produce_exec(&mut child, pid, stdout, stderr, &sender, &cancellation).await;
        match result {
            Ok(completion) => {
                *task_completion.lock().expect("completion mutex poisoned") =
                    Some(completion.clone());
                let _ =
                    send_source(&sender, SourceMessage::Complete(completion), &cancellation).await;
            }
            Err((bytes_read, error)) => {
                send_failure(&sender, bytes_read, error).await;
            }
        }
    });
    Ok(FileProducerHandle::from_parts_with_completion(
        receiver, task, completion,
    ))
}

#[cfg(not(unix))]
async fn spawn_exec(
    _command: &Arc<Vec<OsString>>,
    _cancellation: CancellationToken,
) -> Result<FileProducerHandle, SourceError> {
    Err(SourceError::Manifest {
        detail: "--exec is supported only on Unix platforms".to_owned(),
    })
}

#[cfg(unix)]
async fn produce_exec(
    child: &mut tokio::process::Child,
    pid: Option<u32>,
    mut stdout: tokio::process::ChildStdout,
    mut stderr: tokio::process::ChildStderr,
    sender: &mpsc::Sender<SourceMessage>,
    cancellation: &CancellationToken,
) -> Result<SourceCompletion, (u64, SourceError)> {
    let stderr_cancel = cancellation.clone();
    let mut stderr_task =
        tokio::spawn(async move { drain_stderr(&mut stderr, &stderr_cancel).await });
    let mut buffer = vec![0u8; STREAM_CHUNK];
    let mut hasher = Sha256::new();
    let mut bytes_read = 0u64;
    loop {
        let read = tokio::select! {
            _ = cancellation.cancelled() => {
                terminate_child(child, pid).await;
                let _ = stderr_task.await;
                return Err((bytes_read, SourceError::Cancelled));
            }
            result = stdout.read(&mut buffer) => match result {
                Ok(read) => read,
                Err(error) => {
                    terminate_child(child, pid).await;
                    let _ = stderr_task.await;
                    return Err((bytes_read, SourceError::io("read producer stdout", error)));
                }
            },
        };
        if read == 0 {
            break;
        }
        bytes_read = match bytes_read.checked_add(read as u64) {
            Some(total) => total,
            None => {
                terminate_child(child, pid).await;
                let _ = stderr_task.await;
                return Err((bytes_read, SourceError::SizeOverflow));
            }
        };
        hasher.update(&buffer[..read]);
        if let Err(()) = send_source(
            sender,
            SourceMessage::Data(Bytes::copy_from_slice(&buffer[..read])),
            cancellation,
        )
        .await
        {
            terminate_child(child, pid).await;
            let _ = stderr_task.await;
            return Err((bytes_read, SourceError::ReceiverClosed));
        }
    }

    let status = tokio::select! {
        _ = cancellation.cancelled() => {
            terminate_child(child, pid).await;
            let _ = stderr_task.await;
            return Err((bytes_read, SourceError::Cancelled));
        }
        result = child.wait() => match result {
            Ok(status) => status,
            Err(error) => {
                terminate_child(child, pid).await;
                let _ = stderr_task.await;
                return Err((
                    bytes_read,
                    SourceError::io("wait transfer-link producer", error),
                ));
            }
        },
    };
    match tokio::time::timeout(std::time::Duration::from_secs(5), &mut stderr_task).await {
        Ok(result) => result.map_err(|error| (bytes_read, SourceError::Task(error.to_string())))?,
        Err(_) => {
            // A descendant can inherit stderr after the producer itself exits.
            // Do not let that inherited descriptor hold the transfer open
            // forever; the child process has already supplied its exit status.
            stderr_task.abort();
            let _ = stderr_task.await;
            warn!("transfer-link producer stderr did not close after producer exit");
        }
    }
    if !status.success() {
        return Err((
            bytes_read,
            SourceError::ExecFailed {
                detail: format!("producer exited with status {status}"),
            },
        ));
    }
    Ok(SourceCompletion {
        bytes_read,
        sha256: hasher.finalize().into(),
    })
}

#[cfg(unix)]
async fn drain_stderr(stderr: &mut tokio::process::ChildStderr, cancellation: &CancellationToken) {
    let mut buffer = vec![0u8; STDERR_CHUNK];
    let mut logged = 0usize;
    let mut omitted = 0usize;
    loop {
        let read = tokio::select! {
            _ = cancellation.cancelled() => return,
            result = stderr.read(&mut buffer) => match result {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    debug!(error = %error, "transfer-link producer stderr read failed");
                    break;
                }
            }
        };
        if logged < STDERR_LOG_LIMIT {
            let allowed = (STDERR_LOG_LIMIT - logged).min(read);
            let text = String::from_utf8_lossy(&buffer[..allowed]);
            debug!(producer_stderr = %text, "transfer-link producer stderr");
            logged += allowed;
            omitted += read - allowed;
        } else {
            omitted += read;
        }
    }
    if omitted != 0 {
        warn!(bytes = omitted, "transfer-link producer stderr truncated");
    }
}

#[cfg(unix)]
async fn terminate_child(child: &mut tokio::process::Child, pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
    let needs_reap =
        match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
            Ok(Ok(_)) => false,
            Ok(Err(error)) => {
                debug!(error = %error, "transfer-link producer wait after SIGTERM failed");
                true
            }
            Err(_) => true,
        };
    // The direct child may have exited while a grandchild still owns stdout,
    // stderr or ignores SIGTERM.  Always signal the captured process group so
    // cancellation cannot leave that descendant running in the background.
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    if needs_reap {
        let _ = child.wait().await;
    }
}
