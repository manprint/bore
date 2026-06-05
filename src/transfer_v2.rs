//! Secure file transfer built on top of bore's secret-tunnel transport.

#![allow(missing_docs)]

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self as std_io, ErrorKind, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs::{self, File};
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio::task::{spawn_blocking, JoinHandle, JoinSet};
use tracing::{info, warn};
use uuid::Uuid;

use crate::client::{Client, ProviderMeta};
use crate::secret::Proxy;
use crate::server::DEFAULT_MAX_CONNS;
use crate::transport::Endpoint;

const PROTOCOL_VERSION: u32 = 2;
const FRAME_LIMIT: usize = 16 * 1024 * 1024;
const MANIFEST_CHUNK: usize = 128;
const COPY_BUFFER: usize = 64 * 1024;
const CHUNK_SIZE: u32 = 256 * 1024;
const DEFAULT_PARALLEL: u16 = 4;
const MAX_PARALLEL: u16 = 32;
const LOCAL_BIND: &str = "127.0.0.1:0";
const LOCAL_HOST: &str = "127.0.0.1";
const LOCAL_CONNECT_RETRIES: usize = 50;
const LOCAL_CONNECT_DELAY: Duration = Duration::from_millis(20);
const PROGRESS_TICK: Duration = Duration::from_millis(250);
const RESUME_STATE_FILE: &str = "state.json";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum CollisionPolicy {
    #[default]
    Fail,
    Overwrite,
    Rename,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum SymlinkMode {
    Include,
    #[default]
    Exclude,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum DeviceMode {
    #[default]
    Exclude,
    Include,
}

#[derive(Clone, Debug)]
pub struct ListenerOptions {
    pub to: String,
    pub secret: Option<String>,
    pub insecure: bool,
    pub transfer_id: Option<String>,
    pub dest_path: PathBuf,
    pub relay_only: bool,
    pub stun_server: Option<String>,
    pub upnp: bool,
    pub try_port_prediction: bool,
    pub nat_udp_preferred_port: u16,
    pub nat_udp_release_timeout: u64,
    pub carriers: u16,
    pub collision: CollisionPolicy,
}

#[derive(Clone, Debug)]
pub struct SenderOptions {
    pub to: String,
    pub secret: Option<String>,
    pub insecure: bool,
    pub transfer_id: Option<String>,
    pub source: PathBuf,
    pub output: Option<PathBuf>,
    pub relay_only: bool,
    pub stun_server: Option<String>,
    pub upnp: bool,
    pub try_port_prediction: bool,
    pub nat_udp_preferred_port: u16,
    pub nat_udp_release_timeout: u64,
    pub carriers: u16,
    pub parallel: u16,
    pub symlinks: SymlinkMode,
    pub devices: DeviceMode,
}

#[derive(Clone, Debug)]
pub struct TransferOutcome {
    pub transfer_id: String,
    pub final_path: PathBuf,
    pub total_bytes: u64,
    pub regular_files: u64,
    pub transport: TransportMode,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct TransportMode {
    pub direct_udp: bool,
    pub relay_tls: bool,
}

impl TransportMode {
    fn label(self) -> &'static str {
        if self.direct_udp {
            "direct-udp"
        } else {
            "relay"
        }
    }

    fn security(self) -> &'static str {
        if self.direct_udp {
            "quic-encrypted"
        } else if self.relay_tls {
            "tls"
        } else {
            "plain"
        }
    }
}

impl fmt::Display for TransportMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.label(), self.security())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
enum RootSourceKind {
    Filesystem,
    Stdin,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
enum EntryKind {
    RegularFile,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
}

impl EntryKind {
    fn is_regular_file(self) -> bool {
        matches!(self, Self::RegularFile)
    }

    fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BeginFrame {
    protocol_version: u32,
    transfer_id: String,
    root_name: String,
    root_source: RootSourceKind,
    total_entries: u64,
    total_bytes: Option<u64>,
    transport: TransportMode,
    requested_parallel: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ManifestEntry {
    id: u32,
    rel_path: String,
    kind: EntryKind,
    size: Option<u64>,
    full_hash: Option<String>,
    chunk_count: u32,
    symlink_target: Option<String>,
    device: Option<DeviceDescriptor>,
    mode: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceDescriptor {
    major: u64,
    minor: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
struct TransferSummary {
    regular_files: u64,
    directories: u64,
    symlinks: u64,
    devices: u64,
    total_bytes: u64,
    transfer_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CompletedFrame {
    final_path: String,
    total_bytes: u64,
    regular_files: u64,
    transfer_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TransferRecord {
    rel_path: String,
    kind: EntryKind,
    size: Option<u64>,
    symlink_target: Option<String>,
    device: Option<DeviceDescriptor>,
    content_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ResumeFilePlan {
    entry_id: u32,
    completed_chunks: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ResumeState {
    protocol_version: u32,
    transfer_id: String,
    manifest_hash: String,
    final_name: String,
    files: Vec<FileResumeState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileResumeState {
    entry_id: u32,
    completed: Vec<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Frame {
    Begin(BeginFrame),
    ManifestChunk {
        entries: Vec<ManifestEntry>,
    },
    ManifestDone,
    ManifestAccepted {
        final_name: String,
        parallel: u16,
        resumed_bytes: u64,
        resume: Vec<ResumeFilePlan>,
    },
    WorkerHello {
        transfer_id: String,
    },
    WorkerDone,
    ChunkStart {
        entry_id: u32,
        chunk_index: u32,
        offset: u64,
        len: u32,
        blake3: String,
    },
    ChunkAck {
        entry_id: u32,
        chunk_index: u32,
    },
    StreamChunk {
        len: u32,
    },
    StreamEnd {
        size: u64,
        blake3: String,
    },
    StreamVerified {
        size: u64,
        blake3: String,
    },
    TransferSummary(TransferSummary),
    Completed(CompletedFrame),
    Error {
        message: String,
    },
}

#[derive(Clone, Debug)]
enum SenderSource {
    Filesystem(PathBuf),
    Stdin,
}

#[derive(Clone, Debug)]
struct PlannedEntry {
    manifest: ManifestEntry,
    source_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct PlannedTransfer {
    transfer_id: String,
    root_name: String,
    root_source: RootSourceKind,
    entries: Vec<PlannedEntry>,
    total_bytes: Option<u64>,
    parallel: u16,
}

#[derive(Clone, Debug)]
struct ChunkTask {
    entry_id: u32,
    path: PathBuf,
    rel_path: String,
    offset: u64,
    len: u32,
    chunk_index: u32,
}

#[derive(Clone, Debug)]
struct ReceiverPlan {
    begin: BeginFrame,
    entries: Vec<ManifestEntry>,
    final_name: String,
    final_path: PathBuf,
    stage_dir: PathBuf,
    stage_root: PathBuf,
    _manifest_hash: String,
    resume: Option<Arc<ResumeShared>>,
    resumed_bytes: u64,
    resume_plan: Vec<ResumeFilePlan>,
}

#[derive(Clone, Debug)]
struct ResumeShared {
    transfer_id: String,
    state_file: PathBuf,
    stage_root: PathBuf,
    entries: Arc<BTreeMap<u32, ManifestEntry>>,
    state: Arc<AsyncMutex<ResumeState>>,
}

#[derive(Clone, Debug, Default)]
struct TransferCounts {
    regular_files: u64,
    directories: u64,
    symlinks: u64,
    devices: u64,
    total_bytes: u64,
}

struct ProgressTracker {
    shared: Arc<ProgressShared>,
    task: Option<JoinHandle<()>>,
}

#[derive(Clone)]
struct ProgressHandle {
    shared: Arc<ProgressShared>,
}

struct ProgressShared {
    label: &'static str,
    enabled: bool,
    total_bytes: Option<u64>,
    total_files: u64,
    bytes_done: AtomicU64,
    resumed_bytes: AtomicU64,
    files_done: AtomicU64,
    workers: AtomicU64,
    current: StdMutex<String>,
    finished: AtomicBool,
}

impl ProgressTracker {
    fn new(label: &'static str, total_bytes: Option<u64>, total_files: u64) -> Self {
        let shared = Arc::new(ProgressShared {
            label,
            enabled: std::io::stderr().is_terminal(),
            total_bytes,
            total_files,
            bytes_done: AtomicU64::new(0),
            resumed_bytes: AtomicU64::new(0),
            files_done: AtomicU64::new(0),
            workers: AtomicU64::new(0),
            current: StdMutex::new(String::new()),
            finished: AtomicBool::new(false),
        });
        let task_shared = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            if !task_shared.enabled {
                return;
            }
            let started = Instant::now();
            let mut interval = tokio::time::interval(PROGRESS_TICK);
            loop {
                interval.tick().await;
                render_progress(&task_shared, started.elapsed(), false);
                if task_shared.finished.load(Ordering::Relaxed) {
                    render_progress(&task_shared, started.elapsed(), true);
                    break;
                }
            }
        });
        Self {
            shared,
            task: Some(task),
        }
    }

    fn handle(&self) -> ProgressHandle {
        ProgressHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    async fn finish(mut self) {
        self.shared.finished.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl ProgressHandle {
    fn set_current(&self, value: impl Into<String>) {
        *self.shared.current.lock().expect("progress mutex") = value.into();
    }

    fn add_bytes(&self, bytes: u64) {
        self.shared.bytes_done.fetch_add(bytes, Ordering::Relaxed);
    }

    fn add_resumed_bytes(&self, bytes: u64) {
        self.shared
            .resumed_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    fn worker_started(&self) {
        self.shared.workers.fetch_add(1, Ordering::Relaxed);
    }

    fn worker_finished(&self) {
        self.shared.workers.fetch_sub(1, Ordering::Relaxed);
    }
}

pub async fn run_listener(options: ListenerOptions) -> Result<TransferOutcome> {
    let transfer_id = options
        .transfer_id
        .clone()
        .unwrap_or_else(generate_transfer_id);
    if options.transfer_id.is_none() {
        println!("transfer id: {transfer_id}");
    }
    fs::create_dir_all(&options.dest_path)
        .await
        .with_context(|| {
            format!(
                "failed to create destination root {}",
                options.dest_path.display()
            )
        })?;

    let endpoint = Endpoint::parse(&options.to);
    info!(
        transfer_id = %transfer_id,
        dest_path = %options.dest_path.display(),
        udp = !options.relay_only,
        carriers = options.carriers,
        relay_security = if endpoint.tls { "tls" } else { "plain" },
        "transfer listener starting"
    );

    let internal = TcpListener::bind(LOCAL_BIND)
        .await
        .context("failed to bind transfer listener loopback port")?;
    let local_port = internal.local_addr()?.port();
    let provider = Client::new_secret_provider(
        LOCAL_HOST,
        local_port,
        &options.to,
        &transfer_id,
        options.secret.as_deref(),
        options.insecure,
        !options.relay_only,
        options.stun_server.as_deref(),
        options.upnp,
        options.try_port_prediction,
        options.nat_udp_preferred_port,
        options.nat_udp_release_timeout,
        DEFAULT_MAX_CONNS,
        options.carriers.max(1),
        ProviderMeta::default(),
    )
    .await?;
    let mut provider_task = tokio::spawn(provider.listen());

    let (conn_tx, mut conn_rx) = mpsc::unbounded_channel();
    let mut accept_task = tokio::spawn(async move {
        loop {
            let (stream, _) = internal.accept().await?;
            if conn_tx.send(stream).is_err() {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    println!(
        "waiting for transfer {transfer_id} into {}",
        options.dest_path.display()
    );

    let control = tokio::select! {
        maybe = conn_rx.recv() => maybe.context("failed to accept control transfer stream")?,
        result = &mut provider_task => match result {
            Ok(Ok(())) => bail!("transfer listener transport ended before a sender connected"),
            Ok(Err(err)) => return Err(err).context("transfer listener transport failed"),
            Err(err) => bail!("transfer listener task failed: {err}"),
        },
        result = &mut accept_task => match result {
            Ok(Ok(())) => bail!("transfer listener stopped accepting before a sender connected"),
            Ok(Err(err)) => return Err(err).context("transfer listener accept loop failed"),
            Err(err) => bail!("transfer listener accept task failed: {err}"),
        },
    };

    let outcome =
        receive_transfer(control, &mut conn_rx, options.dest_path, options.collision).await;
    provider_task.abort();
    accept_task.abort();
    let _ = provider_task.await;
    let _ = accept_task.await;
    outcome.map(|mut transfer| {
        transfer.transfer_id = transfer_id;
        transfer
    })
}

pub async fn run_sender(options: SenderOptions) -> Result<TransferOutcome> {
    let transfer_id = options
        .transfer_id
        .clone()
        .unwrap_or_else(generate_transfer_id);
    if options.transfer_id.is_none() {
        println!("transfer id: {transfer_id}");
    }
    let plan = plan_transfer(transfer_id.clone(), &options).await?;
    let endpoint = Endpoint::parse(&options.to);
    let proxy = Proxy::new(
        &options.to,
        LOCAL_BIND.parse().expect("local bind addr"),
        &plan.transfer_id,
        options.secret.as_deref(),
        options.insecure,
        !options.relay_only,
        options.stun_server.as_deref(),
        options.upnp,
        options.try_port_prediction,
        options.nat_udp_preferred_port,
        options.nat_udp_release_timeout,
        options.carriers.max(1),
        None,
    )
    .await?;
    let transport = TransportMode {
        direct_udp: proxy.is_direct(),
        relay_tls: endpoint.tls,
    };
    if !transport.direct_udp && !options.relay_only {
        info!(
            transfer_id = %plan.transfer_id,
            relay_security = transport.security(),
            "direct UDP unavailable, falling back to relay"
        );
    }
    info!(
        transfer_id = %plan.transfer_id,
        transport = %transport,
        carriers = options.carriers,
        requested_parallel = plan.parallel,
        "transfer sender transport ready"
    );

    let local_addr = proxy.local_addr()?;
    let proxy_task = tokio::spawn(proxy.listen());
    let outcome = async {
        let control = connect_local(local_addr).await?;
        send_transfer(control, local_addr, plan, transport).await
    }
    .await;
    proxy_task.abort();
    let _ = proxy_task.await;
    outcome
}

async fn send_transfer(
    mut control: TcpStream,
    local_addr: std::net::SocketAddr,
    plan: PlannedTransfer,
    transport: TransportMode,
) -> Result<TransferOutcome> {
    let regular_files_total = plan
        .entries
        .iter()
        .filter(|entry| entry.manifest.kind.is_regular_file())
        .count() as u64;
    let progress = ProgressTracker::new("sender", plan.total_bytes, regular_files_total);
    let handle = progress.handle();

    send_frame(
        &mut control,
        &Frame::Begin(BeginFrame {
            protocol_version: PROTOCOL_VERSION,
            transfer_id: plan.transfer_id.clone(),
            root_name: plan.root_name.clone(),
            root_source: plan.root_source,
            total_entries: plan.entries.len() as u64,
            total_bytes: plan.total_bytes,
            transport,
            requested_parallel: plan.parallel,
        }),
    )
    .await?;
    for chunk in plan.entries.chunks(MANIFEST_CHUNK) {
        send_frame(
            &mut control,
            &Frame::ManifestChunk {
                entries: chunk.iter().map(|entry| entry.manifest.clone()).collect(),
            },
        )
        .await?;
    }
    send_frame(&mut control, &Frame::ManifestDone).await?;

    let (final_name, parallel, resumed_bytes, resume_plan) =
        match expect_frame(&mut control).await? {
            Frame::ManifestAccepted {
                final_name,
                parallel,
                resumed_bytes,
                resume,
            } => (final_name, parallel, resumed_bytes, resume),
            other => bail!("unexpected manifest response: {other:?}"),
        };
    info!(
        transfer_id = %plan.transfer_id,
        final_name = %display_component(&final_name),
        resumed_bytes,
        parallel,
        "transfer manifest accepted"
    );
    handle.add_resumed_bytes(resumed_bytes);

    let summary = if plan.root_source == RootSourceKind::Stdin {
        send_stdin_stream(&mut control, &plan, &handle).await?
    } else {
        let tasks = build_chunk_tasks(&plan, &resume_plan)?;
        if !tasks.is_empty() {
            send_chunked_files(
                local_addr,
                &plan.transfer_id,
                tasks,
                parallel,
                handle.clone(),
            )
            .await?;
        }
        summary_from_entries(&plan.entries)?
    };

    send_frame(&mut control, &Frame::TransferSummary(summary.clone())).await?;
    let completed = match expect_frame(&mut control).await? {
        Frame::Completed(done) => done,
        other => bail!("unexpected final frame from receiver: {other:?}"),
    };
    if completed.transfer_hash != summary.transfer_hash {
        bail!("receiver acknowledged a different transfer hash");
    }

    progress.finish().await;
    Ok(TransferOutcome {
        transfer_id: plan.transfer_id,
        final_path: decode_native_path(&completed.final_path),
        total_bytes: completed.total_bytes,
        regular_files: completed.regular_files,
        transport,
    })
}

async fn send_chunked_files(
    local_addr: std::net::SocketAddr,
    transfer_id: &str,
    tasks: Vec<ChunkTask>,
    parallel: u16,
    progress: ProgressHandle,
) -> Result<()> {
    let injected_limit = injected_fail_after_chunks();
    let acked = Arc::new(AtomicU64::new(0));
    let queue = Arc::new(AsyncMutex::new(VecDeque::from(tasks)));
    let mut joins = JoinSet::new();
    let workers = parallel.clamp(1, MAX_PARALLEL) as usize;

    for _ in 0..workers {
        let queue = Arc::clone(&queue);
        let transfer_id = transfer_id.to_string();
        let progress = progress.clone();
        let acked = Arc::clone(&acked);
        joins.spawn(async move {
            let mut stream = connect_local(local_addr).await?;
            send_frame(
                &mut stream,
                &Frame::WorkerHello {
                    transfer_id: transfer_id.clone(),
                },
            )
            .await?;
            progress.worker_started();
            let worker_result = async {
                loop {
                    let task = {
                        let mut guard = queue.lock().await;
                        guard.pop_front()
                    };
                    let Some(task) = task else {
                        send_frame(&mut stream, &Frame::WorkerDone).await?;
                        break Ok::<(), anyhow::Error>(());
                    };
                    progress.set_current(display_rel_path(&task.rel_path));
                    let chunk = read_chunk_from_file(&task.path, task.offset, task.len).await?;
                    let digest = blake3::hash(&chunk).to_hex().to_string();
                    send_frame(
                        &mut stream,
                        &Frame::ChunkStart {
                            entry_id: task.entry_id,
                            chunk_index: task.chunk_index,
                            offset: task.offset,
                            len: task.len,
                            blake3: digest,
                        },
                    )
                    .await?;
                    stream.write_all(&chunk).await?;
                    match expect_frame(&mut stream).await? {
                        Frame::ChunkAck {
                            entry_id,
                            chunk_index,
                        } => {
                            if entry_id != task.entry_id || chunk_index != task.chunk_index {
                                bail!(
                                    "chunk ack mismatch: expected entry {} chunk {}, got entry {} chunk {}",
                                    task.entry_id,
                                    task.chunk_index,
                                    entry_id,
                                    chunk_index
                                );
                            }
                            progress.add_bytes(task.len as u64);
                            let count = acked.fetch_add(1, Ordering::Relaxed) + 1;
                            if let Some(limit) = injected_limit {
                                if count >= limit {
                                    bail!(
                                        "forced transfer interruption after {limit} acknowledged chunks"
                                    );
                                }
                            }
                        }
                        other => bail!("unexpected worker reply: {other:?}"),
                    }
                }
            }
            .await;
            progress.worker_finished();
            if let Err(err) = &worker_result {
                warn!(%err, "transfer worker failed");
            }
            worker_result
        });
    }

    while let Some(joined) = joins.join_next().await {
        joined.context("worker join failed")??;
    }
    Ok(())
}

async fn send_stdin_stream(
    control: &mut TcpStream,
    plan: &PlannedTransfer,
    progress: &ProgressHandle,
) -> Result<TransferSummary> {
    let entry = plan
        .entries
        .first()
        .context("stdin transfer is missing its manifest entry")?;
    progress.set_current("stdin");
    let mut stdin = io::stdin();
    let mut buffer = vec![0u8; COPY_BUFFER];
    let mut total = 0u64;
    let mut hasher = blake3::Hasher::new();
    loop {
        let read = stdin
            .read(&mut buffer)
            .await
            .context("failed to read stdin")?;
        if read == 0 {
            break;
        }
        send_frame(control, &Frame::StreamChunk { len: read as u32 }).await?;
        control.write_all(&buffer[..read]).await?;
        hasher.update(&buffer[..read]);
        total += read as u64;
        progress.add_bytes(read as u64);
    }
    let digest = hasher.finalize().to_hex().to_string();
    send_frame(
        control,
        &Frame::StreamEnd {
            size: total,
            blake3: digest.clone(),
        },
    )
    .await?;
    match expect_frame(control).await? {
        Frame::StreamVerified { size, blake3 } => {
            if size != total || blake3 != digest {
                bail!("receiver did not verify the same stdin stream");
            }
        }
        other => bail!("unexpected stdin verification frame: {other:?}"),
    }
    let manifest = ManifestEntry {
        size: Some(total),
        full_hash: Some(digest),
        ..entry.manifest.clone()
    };
    summary_from_materialized_entries(&[manifest])
}

async fn receive_transfer(
    mut control: TcpStream,
    incoming: &mut mpsc::UnboundedReceiver<TcpStream>,
    dest_root: PathBuf,
    collision: CollisionPolicy,
) -> Result<TransferOutcome> {
    let outcome = async {
        let begin = match expect_frame(&mut control).await? {
            Frame::Begin(begin) => begin,
            other => bail!("unexpected first control frame: {other:?}"),
        };
        if begin.protocol_version != PROTOCOL_VERSION {
            bail!(
                "unsupported transfer protocol version {}",
                begin.protocol_version
            );
        }
        info!(
            transfer_id = %begin.transfer_id,
            transport = %begin.transport,
            relay_security = begin.transport.security(),
            "transfer incoming"
        );

        let plan = receive_manifest(&mut control, begin, &dest_root, collision).await?;
        send_frame(
            &mut control,
            &Frame::ManifestAccepted {
                final_name: plan.final_name.clone(),
                parallel: plan.begin.requested_parallel.clamp(1, MAX_PARALLEL),
                resumed_bytes: plan.resumed_bytes,
                resume: plan.resume_plan.clone(),
            },
        )
        .await?;

        let total_files = plan
            .entries
            .iter()
            .filter(|entry| entry.kind.is_regular_file())
            .count() as u64;
        let progress = ProgressTracker::new("listener", plan.begin.total_bytes, total_files);
        let handle = progress.handle();
        handle.add_resumed_bytes(plan.resumed_bytes);

        let sender_summary = if plan.begin.root_source == RootSourceKind::Stdin {
            receive_stdin_stream(&mut control, &plan, &handle).await?
        } else {
            receive_filesystem_streams(&mut control, incoming, &plan, &handle).await?
        };

        let local_summary = verify_summary(&plan).await?;
        if sender_summary != local_summary {
            bail!("sender summary does not match receiver state");
        }
        commit_stage(&plan, collision).await?;
        send_frame(
            &mut control,
            &Frame::Completed(CompletedFrame {
                final_path: encode_native_path(&plan.final_path),
                total_bytes: local_summary.total_bytes,
                regular_files: local_summary.regular_files,
                transfer_hash: local_summary.transfer_hash.clone(),
            }),
        )
        .await?;
        progress.finish().await;

        Ok(TransferOutcome {
            transfer_id: plan.begin.transfer_id,
            final_path: plan.final_path,
            total_bytes: local_summary.total_bytes,
            regular_files: local_summary.regular_files,
            transport: plan.begin.transport,
        })
    }
    .await;

    if let Err(err) = &outcome {
        let _ = send_frame(
            &mut control,
            &Frame::Error {
                message: err.to_string(),
            },
        )
        .await;
    }
    outcome
}

async fn receive_filesystem_streams(
    control: &mut TcpStream,
    incoming: &mut mpsc::UnboundedReceiver<TcpStream>,
    plan: &ReceiverPlan,
    progress: &ProgressHandle,
) -> Result<TransferSummary> {
    let resume = plan
        .resume
        .as_ref()
        .context("filesystem transfer is missing resume state")?
        .clone();
    let mut workers = JoinSet::new();
    let expected_workers = expected_worker_connections(plan);

    // `expect_frame(control)` is not cancellation-safe: if a `select!` branch drops it
    // after the 4-byte length prefix was read, the next read starts at the JSON body.
    // Accept worker streams first, then read the summary once all workers drained.
    for _ in 0..expected_workers {
        let stream = incoming
            .recv()
            .await
            .context("worker channel closed before all worker streams connected")?;
        let resume = resume.clone();
        let progress = progress.clone();
        workers.spawn(async move { handle_worker_connection(stream, resume, progress).await });
    }
    while let Some(joined) = workers.join_next().await {
        let joined = joined.context("worker join failed")?;
        joined?;
    }
    match expect_frame(control).await? {
        Frame::TransferSummary(summary) => Ok(summary),
        other => bail!("unexpected control frame while waiting for transfer summary: {other:?}"),
    }
}

fn expected_worker_connections(plan: &ReceiverPlan) -> usize {
    let completed: BTreeMap<u32, usize> = plan
        .resume_plan
        .iter()
        .map(|entry| (entry.entry_id, entry.completed_chunks.len()))
        .collect();
    let pending_chunks = plan
        .entries
        .iter()
        .filter(|entry| entry.kind.is_regular_file())
        .map(|entry| {
            let done = completed.get(&entry.id).copied().unwrap_or_default() as u32;
            entry.chunk_count.saturating_sub(done) as usize
        })
        .sum::<usize>();
    if pending_chunks == 0 {
        0
    } else {
        plan.begin.requested_parallel.clamp(1, MAX_PARALLEL) as usize
    }
}

async fn handle_worker_connection(
    mut stream: TcpStream,
    resume: Arc<ResumeShared>,
    progress: ProgressHandle,
) -> Result<()> {
    match expect_frame(&mut stream).await? {
        Frame::WorkerHello { transfer_id } => {
            if transfer_id != resume.transfer_id {
                bail!("worker connected for unexpected transfer id {transfer_id}");
            }
        }
        other => bail!("unexpected first worker frame: {other:?}"),
    }
    progress.worker_started();
    let worker_result = async {
        loop {
            match recv_frame(&mut stream).await? {
                Some(Frame::ChunkStart {
                    entry_id,
                    chunk_index,
                    offset,
                    len,
                    blake3,
                }) => {
                    let entry = resume
                        .entries
                        .get(&entry_id)
                        .with_context(|| format!("unknown entry id {entry_id}"))?
                        .clone();
                    if !entry.kind.is_regular_file() {
                        bail!("chunk data is only valid for regular files");
                    }
                    let mut payload = vec![0u8; len as usize];
                    stream.read_exact(&mut payload).await?;
                    let local_hash = blake3::hash(&payload).to_hex().to_string();
                    if local_hash != blake3 {
                        bail!(
                            "chunk hash mismatch for {} chunk {}",
                            display_rel_path(&entry.rel_path),
                            chunk_index
                        );
                    }
                    if !resume.is_chunk_complete(entry_id, chunk_index).await? {
                        let path = stage_path(&resume.stage_root, &entry.rel_path)?;
                        write_chunk_to_file(
                            &path,
                            offset,
                            &payload,
                            entry.size.context("regular file missing size")?,
                        )
                        .await?;
                        resume.mark_chunk_complete(entry_id, chunk_index).await?;
                    }
                    progress.set_current(display_rel_path(&entry.rel_path));
                    progress.add_bytes(len as u64);
                    send_frame(
                        &mut stream,
                        &Frame::ChunkAck {
                            entry_id,
                            chunk_index,
                        },
                    )
                    .await?;
                }
                Some(Frame::WorkerDone) => break Ok::<(), anyhow::Error>(()),
                Some(Frame::Error { message }) => bail!("sender worker aborted: {message}"),
                Some(other) => bail!("unexpected worker frame: {other:?}"),
                None => break Ok(()),
            }
        }
    }
    .await;
    if let Err(err) = &worker_result {
        let _ = send_frame(
            &mut stream,
            &Frame::Error {
                message: err.to_string(),
            },
        )
        .await;
    }
    progress.worker_finished();
    worker_result
}

async fn receive_stdin_stream(
    control: &mut TcpStream,
    plan: &ReceiverPlan,
    progress: &ProgressHandle,
) -> Result<TransferSummary> {
    let entry = plan
        .entries
        .first()
        .context("stdin receiver plan is missing its manifest entry")?
        .clone();
    progress.set_current("stdin");
    let path = stage_path(&plan.stage_root, &entry.rel_path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut file = File::create(&path)
        .await
        .with_context(|| format!("failed to create destination file {}", path.display()))?;
    let mut total = 0u64;
    let mut hasher = blake3::Hasher::new();
    loop {
        match expect_frame(control).await? {
            Frame::StreamChunk { len } => {
                let mut buf = vec![0u8; len as usize];
                control.read_exact(&mut buf).await?;
                file.write_all(&buf).await?;
                hasher.update(&buf);
                total += len as u64;
                progress.add_bytes(len as u64);
            }
            Frame::StreamEnd { size, blake3 } => {
                file.flush().await?;
                file.sync_all().await?;
                set_file_mode(&path, entry.mode).await?;
                let local = hasher.finalize().to_hex().to_string();
                if size != total {
                    bail!("stdin byte count mismatch");
                }
                if blake3 != local {
                    bail!("stdin stream hash mismatch");
                }
                send_frame(
                    control,
                    &Frame::StreamVerified {
                        size,
                        blake3: local.clone(),
                    },
                )
                .await?;
                let materialized = ManifestEntry {
                    size: Some(size),
                    full_hash: Some(local),
                    ..entry
                };
                return summary_from_materialized_entries(&[materialized]);
            }
            other => bail!("unexpected frame in stdin stream: {other:?}"),
        }
    }
}

async fn receive_manifest(
    control: &mut TcpStream,
    begin: BeginFrame,
    dest_root: &Path,
    collision: CollisionPolicy,
) -> Result<ReceiverPlan> {
    let mut entries = Vec::with_capacity(begin.total_entries as usize);
    loop {
        match expect_frame(control).await? {
            Frame::ManifestChunk { entries: chunk } => entries.extend(chunk),
            Frame::ManifestDone => break,
            other => bail!("unexpected frame while receiving manifest: {other:?}"),
        }
    }
    if entries.len() as u64 != begin.total_entries {
        bail!("manifest entry count mismatch");
    }
    validate_manifest(&begin, &entries)?;
    let manifest_hash = manifest_hash(&begin.root_name, begin.root_source, &entries)?;

    if begin.root_source == RootSourceKind::Stdin {
        let requested = decode_component(&begin.root_name);
        let final_name_local = resolve_final_name_local(dest_root, &requested, collision).await?;
        let final_name = encode_component_os(&final_name_local);
        let stage_dir = temp_stage_dir(dest_root, &begin.transfer_id);
        let stage_root = stage_dir.join(&final_name_local);
        fs::create_dir_all(&stage_dir)
            .await
            .with_context(|| format!("failed to create stage dir {}", stage_dir.display()))?;
        return Ok(ReceiverPlan {
            begin,
            entries,
            final_name,
            final_path: dest_root.join(final_name_local),
            stage_dir,
            stage_root,
            _manifest_hash: manifest_hash,
            resume: None,
            resumed_bytes: 0,
            resume_plan: Vec::new(),
        });
    }

    let stage_dir = resume_state_dir(dest_root, &begin.transfer_id);
    let state_file = stage_dir.join(RESUME_STATE_FILE);
    let (final_name, final_name_local, state, resumed_bytes, resume_plan) = if fs::try_exists(
        &state_file,
    )
    .await?
    {
        let state = load_resume_state(&state_file).await?;
        if state.protocol_version != PROTOCOL_VERSION {
            bail!(
                "resume state for transfer {} was created by protocol version {}",
                begin.transfer_id,
                state.protocol_version
            );
        }
        if state.transfer_id != begin.transfer_id || state.manifest_hash != manifest_hash {
            bail!(
                "resume state for transfer {} does not match the current manifest; remove {} to start fresh",
                begin.transfer_id,
                stage_dir.display()
            );
        }
        let (resumed_bytes, resume_plan) = build_resume_plan(&entries, &state)?;
        let final_name_local = decode_component(&state.final_name);
        (
            state.final_name.clone(),
            final_name_local,
            state,
            resumed_bytes,
            resume_plan,
        )
    } else {
        fs::create_dir_all(&stage_dir)
            .await
            .with_context(|| format!("failed to create state directory {}", stage_dir.display()))?;
        let requested = decode_component(&begin.root_name);
        let final_name_local = resolve_final_name_local(dest_root, &requested, collision).await?;
        let final_name = encode_component_os(&final_name_local);
        let state = ResumeState {
            protocol_version: PROTOCOL_VERSION,
            transfer_id: begin.transfer_id.clone(),
            manifest_hash: manifest_hash.clone(),
            final_name: final_name.clone(),
            files: entries
                .iter()
                .filter(|entry| entry.kind.is_regular_file())
                .map(|entry| FileResumeState {
                    entry_id: entry.id,
                    completed: vec![false; entry.chunk_count as usize],
                })
                .collect(),
        };
        persist_resume_state(&state_file, &state).await?;
        (final_name, final_name_local, state, 0, Vec::new())
    };
    let stage_root = stage_dir.join(&final_name_local);
    prepare_stage_entries(&stage_root, &entries).await?;
    let resume = Arc::new(ResumeShared {
        transfer_id: begin.transfer_id.clone(),
        state_file,
        stage_root: stage_root.clone(),
        entries: Arc::new(
            entries
                .iter()
                .cloned()
                .map(|entry| (entry.id, entry))
                .collect(),
        ),
        state: Arc::new(AsyncMutex::new(state)),
    });

    Ok(ReceiverPlan {
        begin,
        entries,
        final_name,
        final_path: dest_root.join(final_name_local),
        stage_dir,
        stage_root,
        _manifest_hash: manifest_hash,
        resume: Some(resume),
        resumed_bytes,
        resume_plan,
    })
}

async fn prepare_stage_entries(stage_root: &Path, entries: &[ManifestEntry]) -> Result<()> {
    for entry in entries {
        let path = stage_path(stage_root, &entry.rel_path)?;
        match entry.kind {
            EntryKind::Directory => {
                fs::create_dir_all(&path).await?;
            }
            EntryKind::RegularFile => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).await?;
                }
                prepare_regular_file(&path, entry.size.unwrap_or(0)).await?;
            }
            EntryKind::Symlink => {
                if path_exists(&path).await? {
                    continue;
                }
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).await?;
                }
                let target = decode_native_path(
                    entry
                        .symlink_target
                        .as_deref()
                        .context("symlink entry is missing its target")?,
                );
                create_symlink(&target, &path).await?;
            }
            EntryKind::CharDevice | EntryKind::BlockDevice => {
                if path_exists(&path).await? {
                    continue;
                }
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).await?;
                }
                create_device(entry, &path).await?;
            }
        }
    }
    Ok(())
}

async fn prepare_regular_file(path: &Path, size: u64) -> Result<()> {
    let path = path.to_path_buf();
    spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open staged file {}", path.display()))?;
        file.set_len(size)
            .with_context(|| format!("failed to resize staged file {}", path.display()))?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("regular file preparation task failed")?
}

fn validate_manifest(begin: &BeginFrame, entries: &[ManifestEntry]) -> Result<()> {
    if entries.is_empty() {
        bail!("manifest is empty");
    }
    let root = entries.first().context("manifest is empty")?;
    if !root.rel_path.is_empty() {
        bail!("manifest root entry must use an empty relative path");
    }
    let mut seen_ids = BTreeMap::new();
    let mut seen_paths = BTreeMap::<PathBuf, EntryKind>::new();
    for entry in entries {
        if seen_ids.insert(entry.id, ()).is_some() {
            bail!("duplicate manifest entry id {}", entry.id);
        }
        let path = decode_relative_path(&entry.rel_path)?;
        validate_relative_path(&path)?;
        if seen_paths.insert(path.clone(), entry.kind).is_some() {
            bail!(
                "duplicate manifest entry {}",
                display_rel_path(&entry.rel_path)
            );
        }
        let mut parent = path.parent();
        while let Some(ancestor) = parent {
            if let Some(kind) = seen_paths.get(ancestor) {
                if !kind.is_directory() {
                    bail!(
                        "manifest entry {} would descend through a non-directory ancestor",
                        display_rel_path(&entry.rel_path)
                    );
                }
            }
            parent = ancestor.parent();
        }
        if entry.kind.is_regular_file() {
            match begin.root_source {
                RootSourceKind::Filesystem => {
                    if entry.size.is_none() || entry.full_hash.is_none() {
                        bail!(
                            "regular file {} is missing size or hash metadata",
                            display_rel_path(&entry.rel_path)
                        );
                    }
                    if entry.chunk_count != chunk_count_for(entry.size.unwrap()) {
                        bail!(
                            "regular file {} has an invalid chunk count",
                            display_rel_path(&entry.rel_path)
                        );
                    }
                }
                RootSourceKind::Stdin => {
                    if !entry.rel_path.is_empty() || entry.size.is_some() {
                        bail!("stdin transfers must have a single root file with unknown size");
                    }
                }
            }
        }
    }
    Ok(())
}

fn manifest_hash(
    root_name: &str,
    root_source: RootSourceKind,
    entries: &[ManifestEntry],
) -> Result<String> {
    let payload = serde_json::to_vec(&(root_name, root_source, entries))?;
    Ok(blake3::hash(&payload).to_hex().to_string())
}

fn build_resume_plan(
    entries: &[ManifestEntry],
    state: &ResumeState,
) -> Result<(u64, Vec<ResumeFilePlan>)> {
    let by_id: BTreeMap<u32, &ManifestEntry> =
        entries.iter().map(|entry| (entry.id, entry)).collect();
    let mut resumed_bytes = 0u64;
    let mut plans = Vec::new();
    for file in &state.files {
        let entry = by_id
            .get(&file.entry_id)
            .with_context(|| format!("resume state references unknown entry {}", file.entry_id))?;
        if file.completed.len() != entry.chunk_count as usize {
            bail!(
                "resume state chunk layout does not match entry {}",
                file.entry_id
            );
        }
        let mut completed_chunks = Vec::new();
        for (index, done) in file.completed.iter().copied().enumerate() {
            if done {
                completed_chunks.push(index as u32);
                resumed_bytes += chunk_len(entry.size.unwrap_or(0), index as u32);
            }
        }
        plans.push(ResumeFilePlan {
            entry_id: file.entry_id,
            completed_chunks,
        });
    }
    Ok((resumed_bytes, plans))
}

async fn verify_summary(plan: &ReceiverPlan) -> Result<TransferSummary> {
    if plan.begin.root_source == RootSourceKind::Stdin {
        let entry = plan
            .entries
            .first()
            .context("stdin receiver plan is missing its manifest entry")?
            .clone();
        let path = stage_path(&plan.stage_root, &entry.rel_path)?;
        let metadata = fs::metadata(&path)
            .await
            .with_context(|| format!("failed to stat staged stdin file {}", path.display()))?;
        let materialized = ManifestEntry {
            size: Some(metadata.len()),
            full_hash: Some(hash_file_async(&path).await?),
            ..entry
        };
        return summary_from_materialized_entries(&[materialized]);
    }
    if let Some(resume) = &plan.resume {
        for entry in &plan.entries {
            if !entry.kind.is_regular_file() {
                continue;
            }
            if !resume.all_chunks_complete(entry.id).await? {
                bail!(
                    "receiver is still missing chunks for {}",
                    display_rel_path(&entry.rel_path)
                );
            }
            let expected = entry
                .full_hash
                .as_deref()
                .context("regular file manifest is missing its hash")?;
            let actual = hash_file_async(&stage_path(&plan.stage_root, &entry.rel_path)?).await?;
            if expected != actual {
                resume.reset_file(entry.id).await?;
                bail!(
                    "final file hash mismatch for {}",
                    display_rel_path(&entry.rel_path)
                );
            }
        }
    }
    summary_from_materialized_entries(&plan.entries)
}

async fn commit_stage(plan: &ReceiverPlan, collision: CollisionPolicy) -> Result<()> {
    if fs::try_exists(&plan.final_path).await? {
        match collision {
            CollisionPolicy::Fail | CollisionPolicy::Rename => {
                bail!("destination already exists: {}", plan.final_path.display());
            }
            CollisionPolicy::Overwrite => {
                let backup = plan.stage_dir.join("overwrite-backup");
                fs::rename(&plan.final_path, &backup)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to stage existing destination {}",
                            plan.final_path.display()
                        )
                    })?;
                if let Err(err) = fs::rename(&plan.stage_root, &plan.final_path).await {
                    let _ = fs::rename(&backup, &plan.final_path).await;
                    return Err(err).with_context(|| {
                        format!(
                            "failed to commit staged transfer to {}",
                            plan.final_path.display()
                        )
                    });
                }
                remove_any(&backup).await?;
            }
        }
    } else {
        fs::rename(&plan.stage_root, &plan.final_path)
            .await
            .with_context(|| {
                format!(
                    "failed to commit staged transfer to {}",
                    plan.final_path.display()
                )
            })?;
    }
    if fs::try_exists(&plan.stage_dir).await? {
        let _ = fs::remove_dir_all(&plan.stage_dir).await;
    }
    Ok(())
}

async fn plan_transfer(transfer_id: String, options: &SenderOptions) -> Result<PlannedTransfer> {
    let parallel = if options.parallel == 0 {
        options.carriers.clamp(1, DEFAULT_PARALLEL)
    } else {
        options.parallel.clamp(1, MAX_PARALLEL)
    };
    let symlinks = options.symlinks;
    let devices = options.devices;
    match parse_sender_source(&options.source) {
        SenderSource::Stdin => {
            let output = options
                .output
                .as_ref()
                .context("--output is required when --source stdin")?;
            let root_name = encode_root_name(output)?;
            Ok(PlannedTransfer {
                transfer_id,
                root_name,
                root_source: RootSourceKind::Stdin,
                entries: vec![PlannedEntry {
                    manifest: ManifestEntry {
                        id: 0,
                        rel_path: String::new(),
                        kind: EntryKind::RegularFile,
                        size: None,
                        full_hash: None,
                        chunk_count: 0,
                        symlink_target: None,
                        device: None,
                        mode: None,
                    },
                    source_path: None,
                }],
                total_bytes: None,
                parallel: 1,
            })
        }
        SenderSource::Filesystem(path) => spawn_blocking(move || {
            scan_filesystem_transfer(transfer_id, path, symlinks, devices, parallel)
        })
        .await
        .context("filesystem scan task failed")?,
    }
}

fn scan_filesystem_transfer(
    transfer_id: String,
    source: PathBuf,
    symlinks: SymlinkMode,
    devices: DeviceMode,
    parallel: u16,
) -> Result<PlannedTransfer> {
    let root_name = source_file_name_string(&source)?;
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    let mut next_id = 0u32;
    scan_entry(
        &source,
        Path::new(""),
        symlinks,
        devices,
        &mut entries,
        &mut total_bytes,
        &mut next_id,
    )?;
    if entries.is_empty() {
        bail!("nothing to transfer from {}", source.display());
    }
    Ok(PlannedTransfer {
        transfer_id,
        root_name,
        root_source: RootSourceKind::Filesystem,
        entries,
        total_bytes: Some(total_bytes),
        parallel,
    })
}

fn scan_entry(
    source: &Path,
    rel_path: &Path,
    symlinks: SymlinkMode,
    devices: DeviceMode,
    entries: &mut Vec<PlannedEntry>,
    total_bytes: &mut u64,
    next_id: &mut u32,
) -> Result<()> {
    let is_root = rel_path.as_os_str().is_empty();
    let metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("failed to stat {}", source.display()))?;
    let file_type = metadata.file_type();
    let rel_path_string = encode_relative_path(rel_path)?;
    let mode = file_mode(&metadata);
    let id = *next_id;
    *next_id = next_id
        .checked_add(1)
        .context("too many manifest entries")?;

    if file_type.is_dir() {
        entries.push(PlannedEntry {
            manifest: ManifestEntry {
                id,
                rel_path: rel_path_string,
                kind: EntryKind::Directory,
                size: None,
                full_hash: None,
                chunk_count: 0,
                symlink_target: None,
                device: None,
                mode,
            },
            source_path: None,
        });
        let mut children = std::fs::read_dir(source)
            .with_context(|| format!("failed to read directory {}", source.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("failed to enumerate directory {}", source.display()))?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let child_rel = if rel_path.as_os_str().is_empty() {
                PathBuf::from(child.file_name())
            } else {
                rel_path.join(child.file_name())
            };
            scan_entry(
                &child.path(),
                &child_rel,
                symlinks,
                devices,
                entries,
                total_bytes,
                next_id,
            )?;
        }
        return Ok(());
    }

    if file_type.is_file() {
        let size = metadata.len();
        let full_hash = hash_file_sync(source)?;
        *total_bytes = total_bytes
            .checked_add(size)
            .context("transfer size overflow")?;
        entries.push(PlannedEntry {
            manifest: ManifestEntry {
                id,
                rel_path: rel_path_string,
                kind: EntryKind::RegularFile,
                size: Some(size),
                full_hash: Some(full_hash),
                chunk_count: chunk_count_for(size),
                symlink_target: None,
                device: None,
                mode,
            },
            source_path: Some(source.to_path_buf()),
        });
        return Ok(());
    }

    if file_type.is_symlink() {
        if symlinks == SymlinkMode::Exclude {
            if is_root {
                bail!(
                    "source {} is a symlink but --symlinks=exclude",
                    source.display()
                );
            }
            return Ok(());
        }
        let target = std::fs::read_link(source)
            .with_context(|| format!("failed to read symlink {}", source.display()))?;
        entries.push(PlannedEntry {
            manifest: ManifestEntry {
                id,
                rel_path: rel_path_string,
                kind: EntryKind::Symlink,
                size: None,
                full_hash: None,
                chunk_count: 0,
                symlink_target: Some(encode_native_path(&target)),
                device: None,
                mode,
            },
            source_path: None,
        });
        return Ok(());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if file_type.is_char_device() || file_type.is_block_device() {
            if devices == DeviceMode::Exclude {
                if is_root {
                    bail!(
                        "source {} is a device but --devices=exclude",
                        source.display()
                    );
                }
                return Ok(());
            }
            let kind = if file_type.is_char_device() {
                EntryKind::CharDevice
            } else {
                EntryKind::BlockDevice
            };
            let rdev = metadata.rdev();
            entries.push(PlannedEntry {
                manifest: ManifestEntry {
                    id,
                    rel_path: rel_path_string,
                    kind,
                    size: None,
                    full_hash: None,
                    chunk_count: 0,
                    symlink_target: None,
                    device: Some(DeviceDescriptor {
                        major: device_major(rdev),
                        minor: device_minor(rdev),
                    }),
                    mode,
                },
                source_path: None,
            });
            return Ok(());
        }
    }

    bail!("unsupported special file {}", source.display())
}

fn build_chunk_tasks(
    plan: &PlannedTransfer,
    resume_plan: &[ResumeFilePlan],
) -> Result<Vec<ChunkTask>> {
    let completed: BTreeMap<u32, Vec<u32>> = resume_plan
        .iter()
        .map(|entry| (entry.entry_id, entry.completed_chunks.clone()))
        .collect();
    let mut tasks = Vec::new();
    for entry in &plan.entries {
        if !entry.manifest.kind.is_regular_file() {
            continue;
        }
        let size = entry.manifest.size.unwrap_or(0);
        let chunk_count = entry.manifest.chunk_count;
        let done = completed
            .get(&entry.manifest.id)
            .cloned()
            .unwrap_or_default();
        let done: BTreeMap<u32, ()> = done.into_iter().map(|index| (index, ())).collect();
        for chunk_index in 0..chunk_count {
            if done.contains_key(&chunk_index) {
                continue;
            }
            tasks.push(ChunkTask {
                entry_id: entry.manifest.id,
                path: entry
                    .source_path
                    .clone()
                    .context("regular file is missing its source path")?,
                rel_path: entry.manifest.rel_path.clone(),
                offset: chunk_index as u64 * CHUNK_SIZE as u64,
                len: chunk_len(size, chunk_index) as u32,
                chunk_index,
            });
        }
    }
    Ok(tasks)
}

fn summary_from_entries(entries: &[PlannedEntry]) -> Result<TransferSummary> {
    summary_from_materialized_entries(
        &entries
            .iter()
            .map(|entry| entry.manifest.clone())
            .collect::<Vec<_>>(),
    )
}

fn summary_from_materialized_entries(entries: &[ManifestEntry]) -> Result<TransferSummary> {
    let mut counts = TransferCounts::default();
    let mut hasher = blake3::Hasher::new();
    for entry in entries {
        match entry.kind {
            EntryKind::RegularFile => {
                counts.regular_files += 1;
                counts.total_bytes += entry.size.unwrap_or(0);
            }
            EntryKind::Directory => counts.directories += 1,
            EntryKind::Symlink => counts.symlinks += 1,
            EntryKind::CharDevice | EntryKind::BlockDevice => counts.devices += 1,
        }
        update_transfer_hash(&mut hasher, entry, entry.full_hash.as_deref())?;
    }
    Ok(TransferSummary {
        regular_files: counts.regular_files,
        directories: counts.directories,
        symlinks: counts.symlinks,
        devices: counts.devices,
        total_bytes: counts.total_bytes,
        transfer_hash: hasher.finalize().to_hex().to_string(),
    })
}

fn update_transfer_hash(
    hasher: &mut blake3::Hasher,
    entry: &ManifestEntry,
    content_hash: Option<&str>,
) -> Result<()> {
    let record = TransferRecord {
        rel_path: entry.rel_path.clone(),
        kind: entry.kind,
        size: entry.size,
        symlink_target: entry.symlink_target.clone(),
        device: entry.device.clone(),
        content_hash: content_hash.map(ToOwned::to_owned),
    };
    let payload = serde_json::to_vec(&record)?;
    hasher.update(&(payload.len() as u32).to_le_bytes());
    hasher.update(&payload);
    Ok(())
}

impl ResumeShared {
    async fn is_chunk_complete(&self, entry_id: u32, chunk_index: u32) -> Result<bool> {
        let state = self.state.lock().await;
        let file = state
            .files
            .iter()
            .find(|file| file.entry_id == entry_id)
            .with_context(|| format!("resume state missing entry {}", entry_id))?;
        Ok(file
            .completed
            .get(chunk_index as usize)
            .copied()
            .unwrap_or(false))
    }

    async fn mark_chunk_complete(&self, entry_id: u32, chunk_index: u32) -> Result<()> {
        let mut state = self.state.lock().await;
        let file = state
            .files
            .iter_mut()
            .find(|file| file.entry_id == entry_id)
            .with_context(|| format!("resume state missing entry {}", entry_id))?;
        let slot = file
            .completed
            .get_mut(chunk_index as usize)
            .with_context(|| {
                format!(
                    "chunk {} is out of range for entry {}",
                    chunk_index, entry_id
                )
            })?;
        *slot = true;
        persist_resume_state(&self.state_file, &state).await
    }

    async fn all_chunks_complete(&self, entry_id: u32) -> Result<bool> {
        let state = self.state.lock().await;
        let file = state
            .files
            .iter()
            .find(|file| file.entry_id == entry_id)
            .with_context(|| format!("resume state missing entry {}", entry_id))?;
        Ok(file.completed.iter().all(|done| *done))
    }

    async fn reset_file(&self, entry_id: u32) -> Result<()> {
        let mut state = self.state.lock().await;
        let file = state
            .files
            .iter_mut()
            .find(|file| file.entry_id == entry_id)
            .with_context(|| format!("resume state missing entry {}", entry_id))?;
        for done in &mut file.completed {
            *done = false;
        }
        persist_resume_state(&self.state_file, &state).await
    }
}

async fn persist_resume_state(path: &Path, state: &ResumeState) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(state)?).await?;
    if fs::try_exists(path).await? {
        let _ = fs::remove_file(path).await;
    }
    fs::rename(&tmp, path).await?;
    Ok(())
}

async fn load_resume_state(path: &Path) -> Result<ResumeState> {
    Ok(serde_json::from_slice(&fs::read(path).await?)?)
}

async fn connect_local(addr: std::net::SocketAddr) -> Result<TcpStream> {
    let mut last_err = None;
    for _ in 0..LOCAL_CONNECT_RETRIES {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(LOCAL_CONNECT_DELAY).await;
            }
        }
    }
    Err(last_err
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow!("failed to connect local transfer socket")))
}

async fn send_frame<S: AsyncWrite + Unpin>(stream: &mut S, frame: &Frame) -> Result<()> {
    let payload = serde_json::to_vec(frame).context("failed to encode transfer frame")?;
    if payload.len() > FRAME_LIMIT {
        bail!("transfer frame exceeds configured limit");
    }
    stream.write_u32_le(payload.len() as u32).await?;
    stream.write_all(&payload).await?;
    Ok(())
}

async fn recv_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Option<Frame>> {
    let len = match stream.read_u32_le().await {
        Ok(len) => len as usize,
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err).context("failed to read transfer frame header"),
    };
    if len > FRAME_LIMIT {
        bail!("peer sent an oversized transfer frame ({len} bytes)");
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

async fn expect_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Frame> {
    match recv_frame(stream).await? {
        Some(Frame::Error { message }) => bail!("peer reported an error: {message}"),
        Some(frame) => Ok(frame),
        None => bail!("unexpected EOF on transfer stream"),
    }
}

fn generate_transfer_id() -> String {
    Uuid::new_v4().to_string()
}

fn parse_sender_source(source: &Path) -> SenderSource {
    if source.as_os_str() == OsStr::new("stdin") && source.components().count() == 1 {
        SenderSource::Stdin
    } else {
        SenderSource::Filesystem(source.to_path_buf())
    }
}

fn encode_root_name(path: &Path) -> Result<String> {
    let mut components = path.components();
    let first = components.next().context("output path is empty")?;
    if components.next().is_some() {
        bail!("transfer root name must be a single path component");
    }
    match first {
        Component::Normal(name) => Ok(encode_component_os(name)),
        _ => bail!("transfer root name must be a single path component"),
    }
}

fn source_file_name_string(path: &Path) -> Result<String> {
    let file_name = path
        .file_name()
        .with_context(|| format!("{} does not end with a file name", path.display()))?;
    Ok(encode_component_os(file_name))
}

fn encode_relative_path(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty() {
        return Ok(String::new());
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => parts.push(encode_component_os(name)),
            _ => bail!("invalid relative path {}", path.display()),
        }
    }
    Ok(parts.join("/"))
}

fn decode_relative_path(encoded: &str) -> Result<PathBuf> {
    if encoded.is_empty() {
        return Ok(PathBuf::new());
    }
    let mut path = PathBuf::new();
    for part in encoded.split('/') {
        if part.is_empty() {
            bail!("invalid relative path encoding {encoded}");
        }
        path.push(decode_component(part));
    }
    Ok(path)
}

fn validate_relative_path(path: &Path) -> Result<()> {
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => bail!("invalid relative path component in {}", path.display()),
        }
    }
    Ok(())
}

fn encode_component_os(value: &OsStr) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        match std::str::from_utf8(value.as_bytes()) {
            Ok(text) => format!("u:{}", hex::encode(text.as_bytes())),
            Err(_) => format!("b:{}", hex::encode(value.as_bytes())),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = value.encode_wide().collect();
        match String::from_utf16(&wide) {
            Ok(text) => format!("u:{}", hex::encode(text.as_bytes())),
            Err(_) => {
                let mut bytes = Vec::with_capacity(wide.len() * 2);
                for unit in wide {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
                format!("w:{}", hex::encode(bytes))
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        format!("u:{}", hex::encode(value.to_string_lossy().as_bytes()))
    }
}

fn decode_component(encoded: &str) -> OsString {
    if let Some(payload) = encoded.strip_prefix("u:") {
        return match hex::decode(payload)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
        {
            Some(text) => safe_component(text),
            None => OsString::from(format!("_bore_bad_utf8_{payload}")),
        };
    }
    if let Some(payload) = encoded.strip_prefix("b:") {
        return decode_unix_component(payload);
    }
    if let Some(payload) = encoded.strip_prefix("w:") {
        return decode_windows_component(payload);
    }
    OsString::from(format!("_bore_invalid_{encoded}"))
}

fn encode_native_path(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        match std::str::from_utf8(path.as_os_str().as_bytes()) {
            Ok(text) => format!("u:{}", hex::encode(text.as_bytes())),
            Err(_) => format!("b:{}", hex::encode(path.as_os_str().as_bytes())),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        match String::from_utf16(&wide) {
            Ok(text) => format!("u:{}", hex::encode(text.as_bytes())),
            Err(_) => {
                let mut bytes = Vec::with_capacity(wide.len() * 2);
                for unit in wide {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
                format!("w:{}", hex::encode(bytes))
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        format!("u:{}", hex::encode(path.to_string_lossy().as_bytes()))
    }
}

fn decode_native_path(encoded: &str) -> PathBuf {
    PathBuf::from(decode_component(encoded))
}

fn decode_unix_component(payload: &str) -> OsString {
    let bytes = hex::decode(payload).unwrap_or_default();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes)
    }
    #[cfg(not(unix))]
    {
        match String::from_utf8(bytes) {
            Ok(text) => safe_component(text),
            Err(_) => OsString::from(format!("_bore_bytes_{payload}")),
        }
    }
}

fn decode_windows_component(payload: &str) -> OsString {
    let bytes = hex::decode(payload).unwrap_or_default();
    let mut wide = Vec::new();
    for chunk in bytes.chunks_exact(2) {
        wide.push(u16::from_le_bytes([chunk[0], chunk[1]]));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        return OsString::from_wide(&wide);
    }
    #[cfg(not(windows))]
    {
        match String::from_utf16(&wide) {
            Ok(text) => safe_component(text),
            Err(_) => OsString::from(format!("_bore_wide_{payload}")),
        }
    }
}

fn safe_component(text: String) -> OsString {
    #[cfg(windows)]
    {
        if is_safe_windows_component(&text) {
            return OsString::from(text);
        }
        return OsString::from(format!("_bore_utf8_{}", hex::encode(text.as_bytes())));
    }
    #[cfg(not(windows))]
    {
        OsString::from(text)
    }
}

#[cfg(windows)]
fn is_safe_windows_component(text: &str) -> bool {
    !text.is_empty()
        && text != "."
        && text != ".."
        && !text.ends_with(' ')
        && !text.ends_with('.')
        && !text.chars().any(|ch| {
            matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || ch <= '\u{1F}'
        })
        && !is_reserved_windows_component(text)
}

#[cfg(windows)]
fn is_reserved_windows_component(text: &str) -> bool {
    let stem = text.split('.').next().unwrap_or(text);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn display_component(encoded: &str) -> String {
    decode_component(encoded).to_string_lossy().to_string()
}

fn display_rel_path(encoded: &str) -> String {
    if encoded.is_empty() {
        ".".to_string()
    } else {
        decode_relative_path(encoded)
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|_| encoded.to_string())
    }
}

async fn resolve_final_name_local(
    dest_root: &Path,
    requested: &OsStr,
    collision: CollisionPolicy,
) -> Result<OsString> {
    let candidate = dest_root.join(requested);
    if !fs::try_exists(&candidate).await? {
        return Ok(requested.to_os_string());
    }
    match collision {
        CollisionPolicy::Fail => bail!(
            "destination already exists: {} (use --overwrite or --rename)",
            candidate.display()
        ),
        CollisionPolicy::Overwrite => Ok(requested.to_os_string()),
        CollisionPolicy::Rename => {
            for idx in 1..10_000u32 {
                let renamed = rename_component(requested, idx);
                if !fs::try_exists(dest_root.join(&renamed)).await? {
                    return Ok(renamed);
                }
            }
            bail!("unable to find a free renamed destination")
        }
    }
}

fn rename_component(name: &OsStr, idx: u32) -> OsString {
    let suffix = format!(" ({idx})");
    if let Some(text) = name.to_str() {
        let path = Path::new(text);
        if let (Some(stem), Some(ext)) = (
            path.file_stem().and_then(|part| part.to_str()),
            path.extension().and_then(|part| part.to_str()),
        ) {
            return OsString::from(format!("{stem}{suffix}.{ext}"));
        }
        return OsString::from(format!("{text}{suffix}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let mut bytes = name.as_bytes().to_vec();
        bytes.extend_from_slice(suffix.as_bytes());
        OsString::from_vec(bytes)
    }
    #[cfg(not(unix))]
    {
        let mut value = name.to_os_string();
        value.push(&suffix);
        value
    }
}

fn resume_state_dir(dest_root: &Path, transfer_id: &str) -> PathBuf {
    let digest = blake3::hash(transfer_id.as_bytes()).to_hex().to_string();
    dest_root.join(format!(".bore-transfer-state-{digest}"))
}

fn temp_stage_dir(dest_root: &Path, transfer_id: &str) -> PathBuf {
    dest_root.join(format!(".bore-transfer-{transfer_id}-{}", Uuid::new_v4()))
}

fn stage_path(stage_root: &Path, rel_path: &str) -> Result<PathBuf> {
    if rel_path.is_empty() {
        Ok(stage_root.to_path_buf())
    } else {
        Ok(stage_root.join(decode_relative_path(rel_path)?))
    }
}

fn chunk_count_for(size: u64) -> u32 {
    if size == 0 {
        0
    } else {
        size.div_ceil(CHUNK_SIZE as u64) as u32
    }
}

fn chunk_len(size: u64, chunk_index: u32) -> u64 {
    let offset = chunk_index as u64 * CHUNK_SIZE as u64;
    size.saturating_sub(offset).min(CHUNK_SIZE as u64)
}

async fn read_chunk_from_file(path: &Path, offset: u64, len: u32) -> Result<Vec<u8>> {
    let path = path.to_path_buf();
    spawn_blocking(move || {
        let file = std::fs::File::open(&path)
            .with_context(|| format!("failed to open source file {}", path.display()))?;
        let mut buf = vec![0u8; len as usize];
        read_exact_at(&file, offset, &mut buf)
            .with_context(|| format!("failed to read chunk from {}", path.display()))?;
        Ok::<Vec<u8>, anyhow::Error>(buf)
    })
    .await
    .context("chunk read task failed")?
}

async fn write_chunk_to_file(path: &Path, offset: u64, payload: &[u8], size: u64) -> Result<()> {
    let path = path.to_path_buf();
    let payload = payload.to_vec();
    spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open staged file {}", path.display()))?;
        file.set_len(size)
            .with_context(|| format!("failed to size staged file {}", path.display()))?;
        write_all_at(&file, offset, &payload)
            .with_context(|| format!("failed to write chunk into {}", path.display()))?;
        file.sync_data()
            .with_context(|| format!("failed to sync staged file {}", path.display()))?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("chunk write task failed")?
}

async fn hash_file_async(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    spawn_blocking(move || hash_file_sync(&path))
        .await
        .context("file hash task failed")?
}

fn hash_file_sync(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; COPY_BUFFER];
    loop {
        let read = file
            .read(&mut buf)
            .with_context(|| format!("failed to read {} for hashing", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(unix)]
fn read_exact_at(file: &std::fs::File, offset: u64, buf: &mut [u8]) -> std_io::Result<()> {
    use std::os::unix::fs::FileExt;
    let mut read = 0usize;
    while read < buf.len() {
        let count = file.read_at(&mut buf[read..], offset + read as u64)?;
        if count == 0 {
            return Err(std_io::Error::new(
                ErrorKind::UnexpectedEof,
                "unexpected EOF while reading at offset",
            ));
        }
        read += count;
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &std::fs::File, offset: u64, buf: &mut [u8]) -> std_io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut read = 0usize;
    while read < buf.len() {
        let count = file.seek_read(&mut buf[read..], offset + read as u64)?;
        if count == 0 {
            return Err(std_io::Error::new(
                ErrorKind::UnexpectedEof,
                "unexpected EOF while reading at offset",
            ));
        }
        read += count;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn read_exact_at(_file: &std::fs::File, _offset: u64, _buf: &mut [u8]) -> std_io::Result<()> {
    Err(std_io::Error::new(
        ErrorKind::Unsupported,
        "random-access reads are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn write_all_at(file: &std::fs::File, offset: u64, buf: &[u8]) -> std_io::Result<()> {
    use std::os::unix::fs::FileExt;
    let mut written = 0usize;
    while written < buf.len() {
        let count = file.write_at(&buf[written..], offset + written as u64)?;
        if count == 0 {
            return Err(std_io::Error::new(
                ErrorKind::WriteZero,
                "failed to write chunk at offset",
            ));
        }
        written += count;
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &std::fs::File, offset: u64, buf: &[u8]) -> std_io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut written = 0usize;
    while written < buf.len() {
        let count = file.seek_write(&buf[written..], offset + written as u64)?;
        if count == 0 {
            return Err(std_io::Error::new(
                ErrorKind::WriteZero,
                "failed to write chunk at offset",
            ));
        }
        written += count;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn write_all_at(_file: &std::fs::File, _offset: u64, _buf: &[u8]) -> std_io::Result<()> {
    Err(std_io::Error::new(
        ErrorKind::Unsupported,
        "random-access writes are unsupported on this platform",
    ))
}

async fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path).await {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

async fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path).await {
        Ok(meta) if meta.file_type().is_dir() => fs::remove_dir_all(path)
            .await
            .with_context(|| format!("failed to remove directory {}", path.display()))?,
        Ok(_) => fs::remove_file(path)
            .await
            .with_context(|| format!("failed to remove file {}", path.display()))?,
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", path.display()))
        }
    }
    Ok(())
}

fn file_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

async fn set_file_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = mode {
            fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
                .await
                .with_context(|| format!("failed to set permissions on {}", path.display()))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

async fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    let target = target.to_path_buf();
    let link = link.to_path_buf();
    spawn_blocking(move || {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &link).with_context(|| {
                format!(
                    "failed to create symlink {} -> {}",
                    link.display(),
                    target.display()
                )
            })
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(&target, &link)
                .or_else(|_| std::os::windows::fs::symlink_dir(&target, &link))
                .with_context(|| {
                    format!(
                        "failed to create symlink {} -> {}",
                        link.display(),
                        target.display()
                    )
                })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (&target, &link);
            bail!("symlink transfer is unsupported on this platform")
        }
    })
    .await
    .context("symlink creation task failed")?
}

async fn create_device(entry: &ManifestEntry, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let entry = entry.clone();
        let path = path.to_path_buf();
        spawn_blocking(move || {
            use nix::sys::stat::{mknod, Mode, SFlag};
            let device = entry.device.context("device entry missing metadata")?;
            let flag = match entry.kind {
                EntryKind::CharDevice => SFlag::S_IFCHR,
                EntryKind::BlockDevice => SFlag::S_IFBLK,
                _ => bail!("invalid device entry kind"),
            };
            let mode = Mode::from_bits_truncate(entry.mode.unwrap_or(0o600) as nix::libc::mode_t);
            mknod(
                &path,
                flag,
                mode,
                device_makedev(device.major, device.minor),
            )
            .with_context(|| format!("failed to create device {}", path.display()))?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("device creation task failed")?
    }
    #[cfg(not(unix))]
    {
        let _ = (entry, path);
        bail!("device transfer is unsupported on this platform")
    }
}

fn injected_fail_after_chunks() -> Option<u64> {
    std::env::var("BORE_TRANSFER_TEST_MAX_ACKED_CHUNKS")
        .ok()
        .and_then(|value| value.parse().ok())
}

#[cfg(unix)]
fn device_major(device: u64) -> u64 {
    nix::libc::major(device as nix::libc::dev_t) as u64
}

#[cfg(unix)]
fn device_minor(device: u64) -> u64 {
    nix::libc::minor(device as nix::libc::dev_t) as u64
}

#[cfg(unix)]
fn device_makedev(major: u64, minor: u64) -> nix::libc::dev_t {
    nix::libc::makedev(major as _, minor as _)
}

fn render_progress(shared: &ProgressShared, elapsed: Duration, done: bool) {
    if !shared.enabled {
        return;
    }
    let elapsed = elapsed.as_secs_f64().max(0.001);
    let current = shared.current.lock().expect("progress mutex").clone();
    let bytes = shared.bytes_done.load(Ordering::Relaxed);
    let resumed = shared.resumed_bytes.load(Ordering::Relaxed);
    let files = shared.files_done.load(Ordering::Relaxed);
    let workers = shared.workers.load(Ordering::Relaxed);
    let speed = human_bytes((bytes as f64 / elapsed) as u64);
    let ratio = match shared.total_bytes {
        Some(total) if total > 0 => format!(
            " {}/{} {:>5.1}%",
            human_bytes(bytes + resumed),
            human_bytes(total),
            ((bytes + resumed) as f64 / total as f64) * 100.0
        ),
        Some(total) => format!(" {}/{}", human_bytes(bytes + resumed), human_bytes(total)),
        None => format!(" {}", human_bytes(bytes)),
    };
    let files = if shared.total_files > 0 {
        format!(" files {files}/{}", shared.total_files)
    } else {
        String::new()
    };
    let resumed = if resumed > 0 {
        format!(" resumed {}", human_bytes(resumed))
    } else {
        String::new()
    };
    let workers = if workers > 0 {
        format!(" workers {workers}")
    } else {
        String::new()
    };
    let suffix = if done { "\n" } else { "\r" };
    eprint!(
        "[{}]{}{}{}{} speed {}/s current {}{}",
        shared.label,
        ratio,
        files,
        resumed,
        workers,
        speed,
        truncate_item(&current),
        suffix,
    );
    let _ = std::io::stderr().flush();
}

fn human_bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0usize;
    let mut value = value as f64;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{}{}", value as u64, UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

fn truncate_item(value: &str) -> String {
    const LIMIT: usize = 48;
    if value.chars().count() <= LIMIT {
        value.to_string()
    } else {
        let prefix: String = value.chars().take(LIMIT - 3).collect();
        format!("{prefix}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_codec_round_trips_utf8_component() {
        let original =
            OsString::from("report-za\u{017c}\u{00f3}\u{0142}\u{0107}-\u{4f8b}\u{5b50}.txt");
        let encoded = encode_component_os(original.as_os_str());

        assert_eq!(decode_component(&encoded), original);
    }

    #[cfg(unix)]
    #[test]
    fn path_codec_round_trips_unix_raw_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let original =
            OsString::from_vec(vec![b'r', b'a', b'w', b'-', 0xff, b'.', b'b', b'i', b'n']);
        let encoded = encode_component_os(OsStr::from_bytes(original.as_bytes()));

        assert!(encoded.starts_with("b:"));
        assert_eq!(decode_component(&encoded), original);
    }

    #[cfg(windows)]
    #[test]
    fn path_codec_sanitizes_windows_reserved_names() {
        for reserved in ["CON", "con.txt", "PRN", "nul.log", "Com1", "lpt9.txt"] {
            assert!(!is_safe_windows_component(reserved));

            let encoded = format!("u:{}", hex::encode(reserved.as_bytes()));
            let expected =
                OsString::from(format!("_bore_utf8_{}", hex::encode(reserved.as_bytes())));
            assert_eq!(decode_component(&encoded), expected);
        }
    }

    #[cfg(windows)]
    #[test]
    fn path_codec_round_trips_windows_wide_units() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let original = OsString::from_wide(&[0x0077, 0xD800, 0x0069, 0x0064, 0x0065]);
        let encoded = encode_component_os(original.as_os_str());
        let decoded = decode_component(&encoded);

        assert!(encoded.starts_with("w:"));
        assert_eq!(
            decoded.encode_wide().collect::<Vec<_>>(),
            original.encode_wide().collect::<Vec<_>>()
        );
    }
}
