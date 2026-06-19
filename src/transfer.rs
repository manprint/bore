//! Secure file transfer built on top of bore's secret-tunnel transport.

#![allow(missing_docs)]

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{BufRead, ErrorKind, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs::{self, File};
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio::task::{spawn_blocking, JoinHandle, JoinSet};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::client::{Client, ProviderMeta};
use crate::secret::Proxy;
use crate::server::{DEFAULT_MAX_CARRIERS, DEFAULT_MAX_CONNS};
use crate::shared::tune_tcp;
use crate::transport::Endpoint;

const PROTOCOL_VERSION: u32 = 3;
const FRAME_LIMIT: usize = 16 * 1024 * 1024;
const MANIFEST_CHUNK: usize = 128;
const COPY_BUFFER: usize = 64 * 1024;
const CHUNK_SIZE: u32 = 1024 * 1024;
const MAX_PARALLEL: u16 = 32;
/// Upper bound for *auto* (`--carriers 0`) carrier scaling. Matches the server's default
/// carrier cap so the auto request is not silently truncated by the server. An explicit
/// `--carriers N` is not clamped here (the server still enforces its own `--max-carriers`).
const AUTO_CARRIER_CAP: u16 = DEFAULT_MAX_CARRIERS;
const RESUME_FLUSH_EVERY_CHUNKS: u64 = 8;
const LOCAL_BIND: &str = "127.0.0.1:0";
const LOCAL_HOST: &str = "127.0.0.1";
const LOCAL_CONNECT_RETRIES: usize = 50;
const LOCAL_CONNECT_DELAY: Duration = Duration::from_millis(20);
const PROGRESS_TICK: Duration = Duration::from_millis(250);
const RESUME_STATE_FILE: &str = "state.json";
const MULTI_SOURCE_STAGE_ROOT: &str = ".bore-multi-root";
/// Upper bound on a single stdin StreamChunk payload the receiver will allocate.
/// The sender uses COPY_BUFFER (64 KiB); allow up to CHUNK_SIZE for headroom.
const STREAM_CHUNK_MAX: usize = CHUNK_SIZE as usize;

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
    pub persistent: bool,
    /// Show the incoming file list and ask for y/N before accepting. Ignored when
    /// the sender is streaming stdin (see `display_and_confirm_manifest_sync`).
    pub ask_confirm: bool,
    /// Seconds to wait for --ask-confirm input before rejecting (0 = wait forever).
    pub confirm_timeout: u64,
    /// Abort if no transfer data is received for this many seconds (0 = disabled).
    pub stall_timeout: u64,
}

#[derive(Clone, Debug)]
pub struct SenderOptions {
    pub to: String,
    pub secret: Option<String>,
    pub insecure: bool,
    pub transfer_id: Option<String>,
    pub sources: Vec<PathBuf>,
    pub source_files: Vec<PathBuf>,
    pub ask_confirm: bool,
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
    /// Abort if no transfer data is sent for this many seconds (0 = disabled).
    pub stall_timeout: u64,
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
    /// True when multiple sources are sent without an explicit --output name.
    /// The receiver commits each top-level entry directly into dest_root.
    #[serde(default)]
    multi_source: bool,
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
    WorkerComplete,
    ChunkStart {
        entry_id: u32,
        chunk_index: u32,
        offset: u64,
        len: u32,
        blake3: String,
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
    multi_source: bool,
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
    resume: Option<Arc<ResumeShared>>,
    resumed_bytes: u64,
    resume_plan: Vec<ResumeFilePlan>,
    multi_source: bool,
    /// Set when the destination already holds content matching this manifest, so the transfer
    /// can be re-acknowledged idempotently without re-sending any data (handles a lost
    /// `Completed` frame or an identical re-run). Detected by content comparison — no marker.
    already_complete: bool,
}

#[derive(Clone, Debug)]
struct ResumeShared {
    transfer_id: String,
    state_file: PathBuf,
    stage_root: PathBuf,
    entries: Arc<BTreeMap<u32, ManifestEntry>>,
    runtime: Arc<AsyncMutex<ResumeRuntime>>,
    persist_lock: Arc<AsyncMutex<()>>,
}

#[derive(Clone, Debug)]
struct ResumeRuntime {
    state: ResumeState,
    dirty_paths: BTreeSet<PathBuf>,
    pending_persist: u64,
    fresh_chunks: BTreeMap<u32, u32>,
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
    started_at: Instant,
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
            started_at: Instant::now(),
        });
        let task_shared = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            if !task_shared.enabled {
                return;
            }
            let mut interval = tokio::time::interval(PROGRESS_TICK);
            loop {
                interval.tick().await;
                render_progress(&task_shared, task_shared.started_at.elapsed(), false);
                if task_shared.finished.load(Ordering::Relaxed) {
                    render_progress(&task_shared, task_shared.started_at.elapsed(), true);
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

    async fn finish(mut self) -> Duration {
        let elapsed = self.shared.started_at.elapsed();
        self.shared.finished.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        elapsed
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

    fn add_file(&self) {
        self.shared.files_done.fetch_add(1, Ordering::Relaxed);
    }
}

pub async fn run_listener(options: ListenerOptions) -> Result<TransferOutcome> {
    test_seam::warn_if_active();
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
    // The listener cannot yet see the sender's `--parallel`, so the auto carrier count uses
    // the same cpu-based hint the sender's auto `--parallel` uses; they match in the common
    // (same-class hardware) case. Carriers only matter on the relay fallback path.
    let carriers = resolve_carriers(options.carriers, default_parallel_hint());
    info!(
        transfer_id = %transfer_id,
        dest_path = %options.dest_path.display(),
        udp = !options.relay_only,
        carriers,
        carriers_requested = options.carriers,
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
        carriers,
        ProviderMeta::default(),
        None, // No access logging for transfer
    )
    .await?;
    let mut provider_task = tokio::spawn(provider.listen());

    let (conn_tx, mut conn_rx) = mpsc::unbounded_channel();
    let mut accept_task = tokio::spawn(async move {
        loop {
            let (stream, _) = internal.accept().await?;
            tune_tcp(&stream);
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

    loop {
        let control = tokio::select! {
            maybe = conn_rx.recv() => match maybe {
                Some(s) => s,
                None => bail!("accept channel closed unexpectedly"),
            },
            result = &mut provider_task => match result {
                Ok(Ok(())) => bail!("the listener's connection to the relay server ended while waiting for a sender"),
                Ok(Err(err)) => return Err(err).context("transfer listener transport failed"),
                Err(err) => bail!("transfer listener task failed: {err}"),
            },
            result = &mut accept_task => match result {
                Ok(Ok(())) => bail!("the listener stopped accepting local connections while waiting for a sender"),
                Ok(Err(err)) => return Err(err).context("transfer listener accept loop failed"),
                Err(err) => bail!("transfer listener accept task failed: {err}"),
            },
        };

        let outcome = receive_transfer(
            control,
            &mut conn_rx,
            options.dest_path.clone(),
            options.collision,
            options.ask_confirm,
            options.confirm_timeout,
            options.stall_timeout,
        )
        .await;

        // A late data-stream connection from a previous transfer (only possible behind a
        // persistent listener) is not a real failure: drop it and keep serving.
        if let Some(stray) = outcome
            .as_ref()
            .err()
            .and_then(|err| err.downcast_ref::<StrayWorkerConnection>())
        {
            debug!(%stray, "ignored stray data-stream connection; waiting for the next sender");
            continue;
        }

        match outcome {
            Ok(mut o) => {
                o.transfer_id = transfer_id.clone();
                if !options.persistent {
                    // Allow tunnel relay to drain before aborting — prevents spurious "channel closed" warnings
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    provider_task.abort();
                    accept_task.abort();
                    let _ = provider_task.await;
                    let _ = accept_task.await;
                    return Ok(o);
                }
                println!(
                    "transfer complete, waiting for next transfer {transfer_id} into {}",
                    options.dest_path.display()
                );
                // Drain any leftover worker connections from the completed transfer.
                while conn_rx.try_recv().is_ok() {}
            }
            Err(err) => {
                if !options.persistent {
                    // Allow tunnel relay to drain before aborting — prevents spurious "channel closed" warnings
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    provider_task.abort();
                    accept_task.abort();
                    let _ = provider_task.await;
                    let _ = accept_task.await;
                    return Err(err);
                }
                warn!(%err, transfer_id = %transfer_id, "transfer failed in persistent mode; staging/resume state is kept for a retry — waiting for the next sender");
                // Drain any leftover connections from the failed transfer.
                while conn_rx.try_recv().is_ok() {}
            }
        }
    }
}

pub async fn run_sender(options: SenderOptions) -> Result<TransferOutcome> {
    test_seam::warn_if_active();
    let transfer_id = options
        .transfer_id
        .clone()
        .unwrap_or_else(generate_transfer_id);
    if options.transfer_id.is_none() {
        println!("transfer id: {transfer_id}");
    }

    // Gather all sources
    let mut all_sources = options.sources.clone();
    if !options.source_files.is_empty() {
        let source_files = options.source_files.clone();
        let extra = spawn_blocking(move || read_source_files(&source_files))
            .await
            .context("source-files read task failed")??;
        all_sources.extend(extra);
    }
    if all_sources.is_empty() {
        bail!("no sources specified; use --sources or --source-files");
    }
    // The stdin stream is mutually exclusive with filesystem sources: a single byte stream
    // has no manifest to merge with files. Reject the combination with a clear message
    // instead of failing later with a confusing "failed to stat stdin".
    if all_sources.len() > 1
        && all_sources
            .iter()
            .any(|s| matches!(parse_sender_source(s), SenderSource::Stdin))
    {
        bail!("'stdin' cannot be combined with other sources; send stdin on its own");
    }
    // Always display the source list; --ask-confirm additionally gates on y/N.
    let ask = options.ask_confirm;
    let sources_for_confirm = all_sources.clone();
    let confirmed = spawn_blocking(move || confirm_sources_sync(&sources_for_confirm, ask))
        .await
        .context("confirmation task failed")??;
    if !confirmed {
        bail!("transfer cancelled by user");
    }

    let plan = plan_transfer(transfer_id.clone(), &options, &all_sources).await?;
    // Auto (`--carriers 0`) matches the resolved worker count so each relay substream gets
    // its own TCP carrier (no single-connection HOL). Ignored on the direct UDP path.
    let carriers = resolve_carriers(options.carriers, plan.parallel);
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
        carriers,
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
        carriers,
        carriers_requested = options.carriers,
        requested_parallel = plan.parallel,
        "transfer sender transport ready"
    );
    if !transport.direct_udp && plan.parallel > carriers {
        warn!(
            parallel = plan.parallel,
            carriers,
            "parallel workers exceed carrier connections; relay path may have HOL blocking — \
             raise --carriers (or leave it at 0/auto)"
        );
    }

    let local_addr = proxy.local_addr()?;
    let proxy_task = tokio::spawn(proxy.listen());
    let outcome = async {
        let control = connect_local(local_addr).await?;
        send_transfer(control, local_addr, plan, transport, options.stall_timeout).await
    }
    .await;
    // Allow tunnel relay to drain before aborting — prevents spurious "channel closed" warnings
    tokio::time::sleep(Duration::from_millis(50)).await;
    proxy_task.abort();
    let _ = proxy_task.await;
    outcome
}

async fn send_transfer(
    mut control: TcpStream,
    local_addr: std::net::SocketAddr,
    plan: PlannedTransfer,
    transport: TransportMode,
    stall_timeout: u64,
) -> Result<TransferOutcome> {
    let regular_files_total = plan
        .entries
        .iter()
        .filter(|entry| entry.manifest.kind.is_regular_file())
        .count() as u64;
    let root_name_display = display_component(&plan.root_name);
    let progress = ProgressTracker::new("sender", plan.total_bytes, regular_files_total);
    let handle = progress.handle();
    handle.set_current(root_name_display.clone());

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
            multi_source: plan.multi_source,
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

    // Bug 003: on EOF after sending the manifest the listener is not running.
    // Peer-sent error frames still propagate as "peer reported an error: ..." so
    // tests that assert specific error messages (e.g. "destination already exists")
    // are unaffected.
    let accepted = recv_manifest_accepted(&mut control, &plan.transfer_id).await?;
    let (final_name, parallel, resumed_bytes, resume_plan) = match accepted {
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
        send_stdin_stream(&mut control, &plan, &handle, stall_timeout).await?
    } else {
        let tasks = build_chunk_tasks(&plan, &resume_plan)?;
        // Seed progress with fully-resumed files (no tasks emitted → add_file never called).
        {
            let completed_counts: BTreeMap<u32, usize> = resume_plan
                .iter()
                .map(|e| (e.entry_id, e.completed_chunks.len()))
                .collect();
            for entry in &plan.entries {
                if !entry.manifest.kind.is_regular_file() {
                    continue;
                }
                let done = completed_counts
                    .get(&entry.manifest.id)
                    .copied()
                    .unwrap_or(0);
                if done as u32 == entry.manifest.chunk_count {
                    handle.add_file();
                }
            }
        }
        if !tasks.is_empty() {
            send_chunked_files(
                local_addr,
                &plan.transfer_id,
                tasks,
                parallel,
                handle.clone(),
                stall_timeout,
            )
            .await?;
        }
        summary_from_entries(&plan.entries)?
    };

    send_frame(&mut control, &Frame::TransferSummary(summary.clone())).await?;
    let completed = match recv_frame(&mut control).await? {
        Some(Frame::Completed(done)) => done,
        Some(Frame::Error { message }) => bail!("peer reported an error: {message}"),
        Some(other) => bail!("unexpected final frame from receiver: {other:?}"),
        None => bail!(
            "transfer data fully sent, but the listener closed before confirming completion. \
             The destination may already be complete — re-run the same command to confirm \
             (it will re-verify idempotently)."
        ),
    };
    if completed.transfer_hash != summary.transfer_hash {
        bail!("receiver acknowledged a different transfer hash");
    }

    let elapsed = progress.finish().await;
    print_transfer_done(
        "sender",
        &root_name_display,
        completed.total_bytes,
        elapsed,
        None,
    );
    let _ = control.shutdown().await;
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
    stall_timeout: u64,
) -> Result<()> {
    let injected_limit = injected_fail_after_chunks();
    let sent = Arc::new(AtomicU64::new(0));
    let queue = Arc::new(AsyncMutex::new(VecDeque::from(tasks)));
    let mut joins = JoinSet::new();
    let workers = parallel.clamp(1, MAX_PARALLEL) as usize;

    for _ in 0..workers {
        let queue = Arc::clone(&queue);
        let transfer_id = transfer_id.to_string();
        let progress = progress.clone();
        let sent = Arc::clone(&sent);
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
                let mut file_cache: HashMap<PathBuf, tokio::fs::File> = HashMap::new();
                loop {
                    let task = {
                        let mut guard = queue.lock().await;
                        guard.pop_front()
                    };
                    let Some(task) = task else {
                        send_frame(&mut stream, &Frame::WorkerDone).await?;
                        break Ok::<(), anyhow::Error>(());
                    };
                    if task.chunk_index == 0 {
                        progress.add_file();
                    }
                    if !task.rel_path.is_empty() {
                        progress.set_current(display_rel_path(&task.rel_path));
                    }
                    let chunk = {
                        let file = if let Some(f) = file_cache.get_mut(&task.path) {
                            f
                        } else {
                            let f = tokio::fs::File::open(&task.path).await.with_context(|| {
                                format!("failed to open source file {}", task.path.display())
                            })?;
                            file_cache.entry(task.path.clone()).or_insert(f)
                        };
                        file.seek(std::io::SeekFrom::Start(task.offset)).await?;
                        let mut buf = vec![0u8; task.len as usize];
                        file.read_exact(&mut buf).await?;
                        buf
                    };
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
                    write_all_idle(&mut stream, &chunk, stall_timeout).await?;
                    progress.add_bytes(task.len as u64);
                    let count = sent.fetch_add(1, Ordering::Relaxed) + 1;
                    if let Some(limit) = injected_limit {
                        if count >= limit {
                            bail!("forced transfer interruption after {limit} chunks sent");
                        }
                    }
                }
            }
            .await;
            // Read terminal frame from receiver (WorkerComplete or Error)
            if worker_result.is_ok() {
                let terminal = async {
                    match with_stall(stall_timeout, expect_frame(&mut stream)).await? {
                        Frame::WorkerComplete => Ok(()),
                        Frame::Error { message } => {
                            bail!("receiver worker reported error: {message}")
                        }
                        other => bail!("unexpected terminal worker frame: {other:?}"),
                    }
                }
                .await;
                progress.worker_finished();
                return terminal;
            }
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
    stall_timeout: u64,
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
        write_all_idle(control, &buffer[..read], stall_timeout).await?;
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
    ask_confirm: bool,
    confirm_timeout: u64,
    stall_timeout: u64,
) -> Result<TransferOutcome> {
    // Tracks the stdin temp stage dir so we can clean it up on failure (F5).
    let mut cleanup_stdin_dir: Option<PathBuf> = None;

    let outcome = async {
        let begin = match expect_frame(&mut control).await? {
            Frame::Begin(begin) => begin,
            // A late data-stream connection from a previous (persistent) transfer. Surface it
            // as a typed, non-fatal error so the listener can skip it and keep serving.
            Frame::WorkerHello { transfer_id } => {
                return Err(StrayWorkerConnection { transfer_id }.into());
            }
            other => bail!(
                "expected a Begin frame to start a transfer, got {other:?} \
                 (protocol desync or a connection from an incompatible client)"
            ),
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

        // Idempotent re-completion: the destination already holds content identical to this
        // manifest (a prior run committed but the Completed frame was lost, or the user simply
        // re-ran the same transfer). Re-acknowledge over the normal protocol so the sender
        // finishes cleanly, without re-sending any data and without a false collision.
        if plan.already_complete {
            // The summary is recomputed from the manifest the sender just sent; its
            // transfer_hash must match what the sender will report.
            let summary = summary_from_materialized_entries(&plan.entries)?;
            send_frame(
                &mut control,
                &Frame::ManifestAccepted {
                    final_name: plan.final_name.clone(),
                    parallel: 1,
                    resumed_bytes: summary.total_bytes,
                    resume: all_chunks_complete_plan(&plan.entries),
                },
            )
            .await?;
            // Sender sees all-complete plan → 0 tasks → no workers → sends TransferSummary.
            match expect_frame(&mut control).await? {
                Frame::TransferSummary(sender_summary) => {
                    if sender_summary.transfer_hash != summary.transfer_hash {
                        bail!("re-sent transfer does not match the existing destination content");
                    }
                }
                other => bail!("unexpected frame during idempotent re-completion: {other:?}"),
            }
            let final_path = plan.final_path.clone();
            send_frame(
                &mut control,
                &Frame::Completed(CompletedFrame {
                    final_path: encode_native_path(&final_path),
                    total_bytes: summary.total_bytes,
                    regular_files: summary.regular_files,
                    transfer_hash: summary.transfer_hash.clone(),
                }),
            )
            .await?;
            let _ = control.shutdown().await;
            info!(
                transfer_id = %plan.begin.transfer_id,
                "destination already matches the manifest; re-acknowledged idempotently"
            );
            return Ok(TransferOutcome {
                transfer_id: plan.begin.transfer_id,
                final_path,
                total_bytes: summary.total_bytes,
                regular_files: summary.regular_files,
                transport: plan.begin.transport,
            });
        }

        // Track stdin temp dir for cleanup on failure.
        if plan.begin.root_source == RootSourceKind::Stdin {
            cleanup_stdin_dir = Some(plan.stage_dir.clone());
        }

        // Always display the incoming file list; when ask_confirm is set and the
        // source is not stdin, wait for the user to confirm before proceeding.
        {
            let begin_clone = plan.begin.clone();
            let entries_clone = plan.entries.clone();
            let dest_root_clone = dest_root.clone();
            let confirm_fut = spawn_blocking(move || {
                display_and_confirm_manifest_sync(
                    &begin_clone,
                    &entries_clone,
                    &dest_root_clone,
                    ask_confirm,
                )
            });
            // When confirm_timeout > 0 and ask_confirm is active, wrap the blocking read in
            // a timeout. Note: tokio::time::timeout cancels the async join-handle, but the
            // underlying blocking OS read on /dev/tty keeps running on the blocking-thread pool
            // until it gets input. The async task is unblocked regardless, so the sender
            // receives a timely rejection frame.
            let accepted = if confirm_timeout > 0 && ask_confirm {
                match tokio::time::timeout(
                    Duration::from_secs(confirm_timeout),
                    confirm_fut,
                )
                .await
                {
                    Ok(joined) => joined.context("manifest confirmation task failed")??,
                    Err(_) => {
                        // The single error frame is sent by the outer error handler below
                        // (avoids sending two Error frames for one failure).
                        let _ = tokio::fs::remove_dir_all(&plan.stage_dir).await;
                        cleanup_stdin_dir = None;
                        bail!(
                            "transfer rejected by receiver (confirmation timed out after {confirm_timeout}s)"
                        );
                    }
                }
            } else {
                confirm_fut
                    .await
                    .context("manifest confirmation task failed")??
            };
            if !accepted {
                // Clean up any staging state created by receive_manifest. The error frame that
                // informs the sender is sent once by the outer error handler below.
                let _ = tokio::fs::remove_dir_all(&plan.stage_dir).await;
                cleanup_stdin_dir = None; // already cleaned above
                bail!("transfer rejected by receiver");
            }
        }

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
        handle.set_current(display_component(&plan.final_name));

        let sender_summary = if plan.begin.root_source == RootSourceKind::Stdin {
            receive_stdin_stream(&mut control, &plan, &handle, stall_timeout).await?
        } else {
            receive_filesystem_streams(&mut control, incoming, &plan, &handle, stall_timeout)
                .await?
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
        let elapsed = progress.finish().await;
        let dest_name = plan
            .final_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| plan.final_path.to_string_lossy().to_string());
        print_transfer_done(
            "listener",
            &dest_name,
            local_summary.total_bytes,
            elapsed,
            Some(&plan.final_path),
        );
        let _ = control.shutdown().await;

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
        // Clean up the stdin temp stage dir on failure (F5).
        if let Some(dir) = &cleanup_stdin_dir {
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
    }
    outcome
}

/// Marker error: the first frame on a freshly accepted control connection was a `WorkerHello`,
/// i.e. a late data-stream connection left over from a previous transfer (only possible with a
/// persistent listener). The listener treats this as non-fatal and waits for the next sender
/// instead of failing a real transfer. See `run_listener`.
#[derive(Debug)]
struct StrayWorkerConnection {
    transfer_id: String,
}

impl fmt::Display for StrayWorkerConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "stray data-stream connection from a previous transfer (id {})",
            self.transfer_id
        )
    }
}

impl std::error::Error for StrayWorkerConnection {}

/// Block until the control connection reports EOF or an error. Returns the resulting error.
///
/// Used as a `select!` arm while the receiver waits for data-stream connections: until every
/// worker has connected, the sender sends nothing on the control channel, so a readable or
/// closed control stream means the sender went away (interrupted/killed/network drop). `peek`
/// is used so this never consumes protocol bytes and stays cancellation-safe.
async fn control_disconnected(control: &TcpStream) -> anyhow::Error {
    let mut probe = [0u8; 1];
    match control.peek(&mut probe).await {
        Ok(0) => anyhow!("sender disconnected before all data streams connected"),
        // Nothing legitimate arrives on the control channel during the accept phase.
        Ok(_) => anyhow!("unexpected control data before all data streams connected"),
        Err(err) => anyhow::Error::from(err)
            .context("control connection failed while waiting for data streams"),
    }
}

/// Accept the next worker data stream, aborting promptly (instead of hanging forever) if the
/// sender disappears or stalls before all streams arrive. Watches three things: a new worker
/// connection, the control connection closing (peer death), and — when enabled — an idle
/// `stall_timeout`.
async fn accept_worker_stream(
    incoming: &mut mpsc::UnboundedReceiver<TcpStream>,
    control: &mut TcpStream,
    stall_timeout: u64,
) -> Result<TcpStream> {
    let idle = Duration::from_secs(stall_timeout.max(1));
    tokio::select! {
        maybe = incoming.recv() => {
            maybe.context("worker channel closed before all data streams connected")
        }
        err = control_disconnected(control) => Err(err),
        _ = tokio::time::sleep(idle), if stall_timeout > 0 => {
            bail!("timed out after {stall_timeout}s waiting for the sender's data streams")
        }
    }
}

async fn receive_filesystem_streams(
    control: &mut TcpStream,
    incoming: &mut mpsc::UnboundedReceiver<TcpStream>,
    plan: &ReceiverPlan,
    progress: &ProgressHandle,
    stall_timeout: u64,
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
    //
    // `accept_worker_stream` watches the control connection so that if the sender dies
    // (interrupt/kill/network drop) after `ManifestAccepted` but before opening every data
    // stream, the receiver aborts with a clear error instead of blocking forever.
    for connected in 0..expected_workers {
        let stream = accept_worker_stream(incoming, control, stall_timeout)
            .await
            .with_context(|| {
                format!("only {connected} of {expected_workers} data streams connected")
            })?;
        let resume = resume.clone();
        let progress = progress.clone();
        workers.spawn(async move {
            handle_worker_connection(stream, resume, progress, stall_timeout).await
        });
    }
    while let Some(joined) = workers.join_next().await {
        let joined = joined.context("worker join failed")?;
        joined?;
    }
    with_stall(stall_timeout, async {
        match expect_frame(control).await? {
            Frame::TransferSummary(summary) => Ok(summary),
            other => {
                bail!("unexpected control frame while waiting for transfer summary: {other:?}")
            }
        }
    })
    .await
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

/// Validate that a ChunkStart's geometry matches the manifest for the given entry.
/// Rejects peer-controlled `offset`/`len` before any allocation, bounding allocation to
/// at most CHUNK_SIZE bytes.
fn validate_chunk_geometry(
    entry: &ManifestEntry,
    chunk_index: u32,
    offset: u64,
    len: u32,
) -> Result<()> {
    let size = entry
        .size
        .context("regular file manifest entry missing size")?;
    if chunk_index >= entry.chunk_count {
        bail!(
            "chunk index {chunk_index} out of range ({}) for {}",
            entry.chunk_count,
            display_rel_path(&entry.rel_path)
        );
    }
    let expected_off = chunk_index as u64 * CHUNK_SIZE as u64;
    let expected_len = chunk_len(size, chunk_index);
    if offset != expected_off || len as u64 != expected_len {
        bail!(
            "chunk geometry mismatch for {} chunk {chunk_index}: \
             got off={offset} len={len}, expected off={expected_off} len={expected_len}",
            display_rel_path(&entry.rel_path)
        );
    }
    Ok(())
}

async fn handle_worker_connection(
    mut stream: TcpStream,
    resume: Arc<ResumeShared>,
    progress: ProgressHandle,
    stall_timeout: u64,
) -> Result<()> {
    match with_stall(stall_timeout, expect_frame(&mut stream)).await? {
        Frame::WorkerHello { transfer_id } => {
            if transfer_id != resume.transfer_id {
                bail!("worker connected for unexpected transfer id {transfer_id}");
            }
        }
        other => bail!("unexpected first worker frame: {other:?}"),
    }
    progress.worker_started();
    let mut file_cache: HashMap<u32, tokio::fs::File> = HashMap::new();
    let worker_result = async {
        loop {
            match with_stall(stall_timeout, recv_frame(&mut stream)).await? {
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
                    validate_chunk_geometry(&entry, chunk_index, offset, len)?;
                    let mut payload = vec![0u8; len as usize];
                    read_exact_idle(&mut stream, &mut payload, stall_timeout).await?;
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
                        let file = if let Some(f) = file_cache.get_mut(&entry_id) {
                            f
                        } else {
                            let f = tokio::fs::OpenOptions::new()
                                .create(false)
                                .truncate(false)
                                .write(true)
                                .open(&path)
                                .await
                                .with_context(|| {
                                    format!("failed to open staged file {}", path.display())
                                })?;
                            file_cache.entry(entry_id).or_insert(f)
                        };
                        file.seek(std::io::SeekFrom::Start(offset)).await?;
                        file.write_all(&payload).await?;
                        resume
                            .mark_chunk_complete(entry_id, chunk_index, &path)
                            .await?;
                    }
                    if chunk_index == 0 {
                        progress.add_file();
                    }
                    if !entry.rel_path.is_empty() {
                        progress.set_current(display_rel_path(&entry.rel_path));
                    }
                    progress.add_bytes(len as u64);
                }
                Some(Frame::WorkerDone) => {
                    resume.flush_pending().await?;
                    send_frame(&mut stream, &Frame::WorkerComplete).await?;
                    break Ok::<(), anyhow::Error>(());
                }
                Some(Frame::Error { message }) => bail!("sender worker aborted: {message}"),
                Some(other) => bail!("unexpected worker frame: {other:?}"),
                // A clean WorkerDone always precedes a normal close; EOF here means the sender
                // was interrupted mid-stream. Fail fast with a clear message rather than
                // silently treating a truncated stream as complete (verification would later
                // reject it with a more confusing hash mismatch).
                None => bail!("data stream closed before WorkerDone (sender interrupted?)"),
            }
        }
    }
    .await;
    // Ensure partial progress is persisted even on error/EOF paths
    let flush_result = resume.flush_pending().await;
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
    flush_result?;
    worker_result
}

async fn receive_stdin_stream(
    control: &mut TcpStream,
    plan: &ReceiverPlan,
    progress: &ProgressHandle,
    stall_timeout: u64,
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
        match with_stall(stall_timeout, expect_frame(control)).await? {
            Frame::StreamChunk { len } => {
                if len == 0 || len as usize > STREAM_CHUNK_MAX {
                    bail!("stdin stream chunk length {len} out of bounds (max {STREAM_CHUNK_MAX})");
                }
                let mut buf = vec![0u8; len as usize];
                read_exact_idle(control, &mut buf, stall_timeout).await?;
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
            resume: None,
            resumed_bytes: 0,
            resume_plan: Vec::new(),
            multi_source: false,
            already_complete: false,
        });
    }

    // For multi_source (no --output), stage inside a hidden sub-directory so that
    // commit_stage can rename each top-level child individually into dest_root.
    let multi_source = begin.multi_source;

    let stage_dir = resume_state_dir(dest_root, &begin.transfer_id);

    // Idempotent re-completion (content-based): if the destination already holds content
    // identical to this manifest, re-acknowledge without re-transferring. This must run
    // before `resolve_final_name_local`, which would otherwise reject a `Fail` collision.
    // No on-disk marker is kept — the destination's own content is the source of truth.
    // Skipped when an in-progress resume state exists (that takes priority).
    let candidate_local = decode_component(&begin.root_name);
    let candidate_final = if multi_source {
        dest_root.to_path_buf()
    } else {
        dest_root.join(&candidate_local)
    };
    if !fs::try_exists(stage_dir.join(RESUME_STATE_FILE)).await?
        && destination_satisfies_manifest(&candidate_final, &entries, multi_source).await?
    {
        return Ok(ReceiverPlan {
            begin,
            entries,
            final_name: encode_component_os(&candidate_local),
            final_path: candidate_final,
            stage_dir,
            stage_root: candidate_local.into(),
            resume: None,
            resumed_bytes: 0,
            resume_plan: Vec::new(),
            multi_source,
            already_complete: true,
        });
    }

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
        let (final_name, final_name_local) = if multi_source {
            // Use a fixed staging sub-dir; final_path will be dest_root itself.
            let name = OsString::from(MULTI_SOURCE_STAGE_ROOT);
            let encoded = encode_component_os(OsStr::new(MULTI_SOURCE_STAGE_ROOT));
            (encoded, name)
        } else {
            let requested = decode_component(&begin.root_name);
            let local = resolve_final_name_local(dest_root, &requested, collision).await?;
            let encoded = encode_component_os(&local);
            (encoded, local)
        };
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
        runtime: Arc::new(AsyncMutex::new(ResumeRuntime {
            state,
            dirty_paths: BTreeSet::new(),
            pending_persist: 0,
            fresh_chunks: BTreeMap::new(),
        })),
        persist_lock: Arc::new(AsyncMutex::new(())),
    });

    // For multi_source, final_path is dest_root (already exists); commit_stage
    // will move individual children rather than renaming the whole stage_root.
    let final_path = if multi_source {
        dest_root.to_path_buf()
    } else {
        dest_root.join(&final_name_local)
    };

    Ok(ReceiverPlan {
        begin,
        entries,
        final_name,
        final_path,
        stage_dir,
        stage_root,
        resume: Some(resume),
        resumed_bytes,
        resume_plan,
        multi_source,
        already_complete: false,
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

/// Builds a resume plan that marks every chunk of every regular file as complete.
/// Used for content-based idempotent re-completion (the destination already matches).
fn all_chunks_complete_plan(entries: &[ManifestEntry]) -> Vec<ResumeFilePlan> {
    entries
        .iter()
        .filter(|e| e.kind.is_regular_file())
        .map(|e| ResumeFilePlan {
            entry_id: e.id,
            completed_chunks: (0..e.chunk_count).collect(),
        })
        .collect()
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
            // Skip full re-hash when all chunks were hash-verified during this run.
            // Per-chunk blake3 verification already proves file content; re-reading
            // the staged file would just double the I/O cost with no integrity gain.
            if resume.all_chunks_fresh(entry.id, entry.chunk_count).await {
                continue;
            }
            // Some chunks came from resume state (not verified this run) — re-hash.
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

/// True when `root` already holds content equivalent to `entries` — the basis for
/// content-based idempotent re-completion (see `receive_transfer`). Conservative: a missing
/// or mismatched entry, or any kind that cannot be cheaply/safely verified (symlink/device),
/// yields `false`, so the normal transfer/collision path runs instead. Never reports a match
/// it cannot prove by content, so it can't mask a genuine collision.
async fn destination_satisfies_manifest(
    root: &Path,
    entries: &[ManifestEntry],
    multi_source: bool,
) -> Result<bool> {
    // Single-root transfers (file or dir): the root must exist. Multi-source targets
    // dest_root (always present), so each entry is checked individually.
    if !multi_source && !fs::try_exists(root).await? {
        return Ok(false);
    }
    for entry in entries {
        let path = stage_path(root, &entry.rel_path)?;
        match entry.kind {
            EntryKind::RegularFile => {
                let Some(expected) = entry.full_hash.as_deref() else {
                    return Ok(false);
                };
                let meta = match fs::symlink_metadata(&path).await {
                    Ok(meta) => meta,
                    Err(_) => return Ok(false),
                };
                if !meta.is_file() || Some(meta.len()) != entry.size {
                    return Ok(false);
                }
                if hash_file_async(&path).await? != expected {
                    return Ok(false);
                }
            }
            EntryKind::Directory => match fs::symlink_metadata(&path).await {
                Ok(meta) if meta.is_dir() => {}
                _ => return Ok(false),
            },
            // Not verified here; transfers containing these take the normal path.
            EntryKind::Symlink | EntryKind::CharDevice | EntryKind::BlockDevice => {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

async fn commit_stage(plan: &ReceiverPlan, collision: CollisionPolicy) -> Result<()> {
    if plan.multi_source {
        // Flat multi-source: move each top-level child of stage_root into dest_root.
        let dest_root = &plan.final_path; // set to dest_root in receive_manifest
        let mut read_dir = fs::read_dir(&plan.stage_root).await.with_context(|| {
            format!(
                "failed to list multi-source stage {}",
                plan.stage_root.display()
            )
        })?;
        while let Some(entry) = read_dir.next_entry().await? {
            let name = entry.file_name();
            let src = plan.stage_root.join(&name);
            let dst = dest_root.join(&name);
            if fs::try_exists(&dst).await? {
                match collision {
                    CollisionPolicy::Fail | CollisionPolicy::Rename => {
                        bail!("destination already exists: {}", dst.display());
                    }
                    CollisionPolicy::Overwrite => {
                        let backup = plan
                            .stage_dir
                            .join(format!("overwrite-backup-{}", name.to_string_lossy()));
                        fs::rename(&dst, &backup).await.with_context(|| {
                            format!("failed to stage existing destination {}", dst.display())
                        })?;
                        if let Err(err) = fs::rename(&src, &dst).await {
                            let _ = fs::rename(&backup, &dst).await;
                            return Err(err).with_context(|| {
                                format!("failed to commit staged item to {}", dst.display())
                            });
                        }
                        remove_any(&backup).await?;
                    }
                }
            } else {
                fs::rename(&src, &dst).await.with_context(|| {
                    format!("failed to commit staged item to {}", dst.display())
                })?;
            }
        }
        // The data is committed into dest_root; remove the whole working state directory
        // (staging subdir + resume state). Idempotent re-runs are detected by comparing the
        // destination's content to the manifest, so no on-disk marker is kept.
        let _ = fs::remove_dir_all(&plan.stage_dir).await;
        return Ok(());
    }

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

    if plan.begin.root_source == RootSourceKind::Stdin {
        // Stdin uses a temp stage dir; no persistent marker needed — remove entirely.
        if fs::try_exists(&plan.stage_dir).await? {
            let _ = fs::remove_dir_all(&plan.stage_dir).await;
        }
    } else {
        // Filesystem: the data is committed; remove the whole working state directory
        // (staged content + resume state). Idempotent re-runs are detected by comparing the
        // destination's content to the manifest — no on-disk marker is kept.
        let _ = fs::remove_dir_all(&plan.stage_dir).await;
    }
    Ok(())
}

fn read_source_files(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for file in files {
        let content = std::fs::read_to_string(file)
            .with_context(|| format!("failed to read source list file {}", file.display()))?;
        for line in content.lines() {
            if line.contains('#') {
                continue;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            paths.push(PathBuf::from(trimmed));
        }
    }
    Ok(paths)
}

fn compute_dir_size_sync(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut queue = vec![path.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if let Ok(m) = std::fs::symlink_metadata(&p) {
                if m.is_dir() {
                    queue.push(p);
                } else if m.is_file() {
                    total += m.len();
                }
            }
        }
    }
    total
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn read_confirmation_line() -> Result<String> {
    if let Some(val) = test_seam::confirm_response() {
        return Ok(val);
    }

    let mut input = String::new();
    // Try /dev/tty first so that confirmation works even when bore is invoked
    // via `curl | bash` (where stdin is the pipe, not the terminal).
    #[cfg(unix)]
    {
        if let Ok(tty) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
        {
            std::io::BufReader::new(tty)
                .read_line(&mut input)
                .context("failed to read confirmation from /dev/tty")?;
            return Ok(input);
        }
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "--ask-confirm requires an interactive terminal; \
             stdin is not a tty and /dev/tty is unavailable"
        );
    }
    std::io::stdin()
        .lock()
        .read_line(&mut input)
        .context("failed to read confirmation")?;
    Ok(input)
}

/// Display the source list and, when `ask_confirm` is true, prompt for y/N.
/// Called via `spawn_blocking` so blocking I/O is safe here.
fn confirm_sources_sync(sources: &[PathBuf], ask_confirm: bool) -> Result<bool> {
    use std::io::Write as _;
    println!("Sources to be transferred:");
    for source in sources {
        // The literal "stdin" is a stream sentinel, not a filesystem path: never stat it.
        if matches!(parse_sender_source(source), SenderSource::Stdin) {
            println!("  STDIN (stream)");
            continue;
        }
        let meta = std::fs::symlink_metadata(source)
            .with_context(|| format!("failed to stat {}", source.display()))?;
        if meta.is_dir() {
            let size = compute_dir_size_sync(source);
            println!("  DIR  {} ({})", source.display(), format_bytes(size));
        } else if meta.is_file() {
            println!("  FILE {} ({})", source.display(), format_bytes(meta.len()));
        } else {
            println!("  OTHER {}", source.display());
        }
    }
    if !ask_confirm {
        return Ok(true);
    }
    print!("Proceed? [y/N] ");
    std::io::stdout()
        .flush()
        .context("failed to flush stdout")?;
    let input = read_confirmation_line()?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

/// Display the incoming manifest on the receiver terminal and, when `ask_confirm`
/// is true, wait for y/N before accepting.
///
/// Called via `spawn_blocking` so blocking I/O is safe here.
///
/// # Stdin transfers
///
/// When `begin.root_source == RootSourceKind::Stdin` this function always returns
/// `Ok(true)` regardless of `ask_confirm`. The sender is already streaming live
/// bytes; there is no safe point to pause and wait for receiver confirmation without
/// a two-round-trip protocol extension (manifest-only phase before data begins).
/// This is a known limitation — future work could add a pre-stream negotiation phase.
fn display_and_confirm_manifest_sync(
    begin: &BeginFrame,
    entries: &[ManifestEntry],
    dest_root: &Path,
    ask_confirm: bool,
) -> Result<bool> {
    use std::io::Write as _;

    println!("Incoming transfer {}:", begin.transfer_id);

    if begin.root_source == RootSourceKind::Stdin {
        let name = display_component(&begin.root_name);
        println!("  STDIN  {name}  (size unknown until transfer completes)");
        if ask_confirm {
            // Documented limitation: --ask-confirm is ignored for stdin transfers.
            println!(
                "[listener] --ask-confirm is ignored for stdin transfers; \
                 proceeding automatically."
            );
        }
        return Ok(true);
    }

    if begin.multi_source {
        // List only the top-level items (entries whose rel_path has no '/' separator).
        for entry in entries {
            if entry.rel_path.is_empty() {
                continue;
            }
            if entry.rel_path.contains('/') {
                continue;
            }
            let name = display_component(&entry.rel_path);
            match entry.kind {
                EntryKind::Directory => {
                    // Sum sizes of all regular files rooted under this directory.
                    let prefix = format!("{}/", entry.rel_path);
                    let size: u64 = entries
                        .iter()
                        .filter(|e| e.kind.is_regular_file() && e.rel_path.starts_with(&prefix))
                        .filter_map(|e| e.size)
                        .sum();
                    println!("  DIR  {name} ({})", format_bytes(size));
                }
                EntryKind::RegularFile => {
                    let size = entry.size.unwrap_or(0);
                    println!("  FILE {name} ({})", format_bytes(size));
                }
                _ => println!("  OTHER {name}"),
            }
        }
    } else {
        // Single source: display the root item.
        let name = display_component(&begin.root_name);
        let total = begin.total_bytes.unwrap_or(0);
        match entries.first().map(|e| e.kind) {
            Some(EntryKind::Directory) => println!("  DIR  {name} ({})", format_bytes(total)),
            Some(EntryKind::RegularFile) => println!("  FILE {name} ({})", format_bytes(total)),
            _ => println!("  {name} ({})", format_bytes(total)),
        }
    }

    let regular_count = entries.iter().filter(|e| e.kind.is_regular_file()).count();
    let total = begin.total_bytes.unwrap_or(0);
    println!(
        "  {} file(s), {}  →  {}",
        regular_count,
        format_bytes(total),
        dest_root.display()
    );

    if !ask_confirm {
        return Ok(true);
    }

    print!("Accept transfer? [y/N] ");
    std::io::stdout()
        .flush()
        .context("failed to flush stdout")?;
    let input = read_confirmation_line()?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

fn scan_multi_filesystem_transfer(
    transfer_id: String,
    sources: Vec<PathBuf>,
    output: Option<PathBuf>,
    symlinks: SymlinkMode,
    devices: DeviceMode,
    parallel: u16,
) -> Result<PlannedTransfer> {
    // When --output is provided, wrap everything in a named directory.
    // Without --output, use flat mode: each source lands directly in dest_root.
    let (root_name, multi_source) = if let Some(out) = output {
        (encode_root_name(&out)?, false)
    } else {
        // root_name is used only for display/hash; flat mode ignores it for paths.
        (source_file_name_string(&sources[0])?, true)
    };

    let mut entries = Vec::new();
    let mut total_bytes = 0u64;

    // Virtual root directory (rel_path="")
    entries.push(PlannedEntry {
        manifest: ManifestEntry {
            id: 0,
            rel_path: String::new(),
            kind: EntryKind::Directory,
            size: None,
            full_hash: None,
            chunk_count: 0,
            symlink_target: None,
            device: None,
            mode: None,
        },
        source_path: None,
    });
    let mut next_id = 1u32;

    for source in &sources {
        let name = source
            .file_name()
            .with_context(|| format!("{} has no file name", source.display()))?;
        let rel = PathBuf::from(name);
        scan_entry(
            source,
            &rel,
            symlinks,
            devices,
            &mut entries,
            &mut total_bytes,
            &mut next_id,
        )?;
    }

    if entries.len() <= 1 {
        bail!("nothing to transfer from the specified sources");
    }

    Ok(PlannedTransfer {
        transfer_id,
        root_name,
        root_source: RootSourceKind::Filesystem,
        entries,
        total_bytes: Some(total_bytes),
        parallel,
        multi_source,
    })
}

async fn plan_transfer(
    transfer_id: String,
    options: &SenderOptions,
    all_sources: &[PathBuf],
) -> Result<PlannedTransfer> {
    let parallel = resolve_parallel(options.parallel);
    let symlinks = options.symlinks;
    let devices = options.devices;

    if all_sources.len() == 1 {
        match parse_sender_source(&all_sources[0]) {
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
                    multi_source: false,
                })
            }
            SenderSource::Filesystem(path) => spawn_blocking(move || {
                scan_filesystem_transfer(transfer_id, path, symlinks, devices, parallel)
            })
            .await
            .context("filesystem scan task failed")?,
        }
    } else {
        let sources = all_sources.to_vec();
        let output = options.output.clone();
        spawn_blocking(move || {
            scan_multi_filesystem_transfer(
                transfer_id,
                sources,
                output,
                symlinks,
                devices,
                parallel,
            )
        })
        .await
        .context("multi-source filesystem scan task failed")?
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
        multi_source: false,
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
        let runtime = self.runtime.lock().await;
        let file = runtime
            .state
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

    async fn mark_chunk_complete(
        &self,
        entry_id: u32,
        chunk_index: u32,
        path: &Path,
    ) -> Result<()> {
        let should_flush = {
            let mut runtime = self.runtime.lock().await;
            let file = runtime
                .state
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
            *runtime.fresh_chunks.entry(entry_id).or_default() += 1;
            runtime.dirty_paths.insert(path.to_path_buf());
            runtime.pending_persist += 1;
            runtime.pending_persist >= RESUME_FLUSH_EVERY_CHUNKS
        };
        if should_flush {
            self.flush_pending().await?;
        }
        Ok(())
    }

    async fn flush_pending(&self) -> Result<()> {
        let _persist = self.persist_lock.lock().await;
        let snapshot = {
            let mut runtime = self.runtime.lock().await;
            if runtime.pending_persist == 0 && runtime.dirty_paths.is_empty() {
                None
            } else {
                runtime.pending_persist = 0;
                Some((
                    runtime.state.clone(),
                    std::mem::take(&mut runtime.dirty_paths)
                        .into_iter()
                        .collect::<Vec<_>>(),
                ))
            }
        };
        if let Some((state, paths)) = snapshot {
            sync_staged_files(&paths).await?;
            persist_resume_state(&self.state_file, &state).await?;
        }
        Ok(())
    }

    async fn all_chunks_fresh(&self, entry_id: u32, chunk_count: u32) -> bool {
        let runtime = self.runtime.lock().await;
        runtime.fresh_chunks.get(&entry_id).copied().unwrap_or(0) >= chunk_count
    }

    async fn all_chunks_complete(&self, entry_id: u32) -> Result<bool> {
        let runtime = self.runtime.lock().await;
        let file = runtime
            .state
            .files
            .iter()
            .find(|file| file.entry_id == entry_id)
            .with_context(|| format!("resume state missing entry {}", entry_id))?;
        Ok(file.completed.iter().all(|done| *done))
    }

    async fn reset_file(&self, entry_id: u32) -> Result<()> {
        let _persist = self.persist_lock.lock().await;
        let state = {
            let mut runtime = self.runtime.lock().await;
            let file = runtime
                .state
                .files
                .iter_mut()
                .find(|file| file.entry_id == entry_id)
                .with_context(|| format!("resume state missing entry {}", entry_id))?;
            for done in &mut file.completed {
                *done = false;
            }
            runtime.pending_persist = 0;
            runtime.state.clone()
        };
        persist_resume_state(&self.state_file, &state).await
    }
}

async fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(value)?).await?;
    if fs::try_exists(path).await? {
        let _ = fs::remove_file(path).await;
    }
    fs::rename(&tmp, path).await?;
    Ok(())
}

async fn read_json<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path).await?)?)
}

async fn persist_resume_state(path: &Path, state: &ResumeState) -> Result<()> {
    write_json_atomic(path, state).await
}

async fn load_resume_state(path: &Path) -> Result<ResumeState> {
    read_json(path).await
}

/// Wraps a future in a stall timeout. If `secs == 0`, the future runs without a timeout.
/// When the timeout fires, the future is dropped (cancellation-safe because we always bail
/// immediately and discard the associated stream).
async fn with_stall<T>(secs: u64, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    if secs == 0 {
        return fut.await;
    }
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(r) => r,
        Err(_) => bail!("transfer stalled: no progress for {secs}s"),
    }
}

/// Write the whole buffer, enforcing `secs` as a true **idle** timeout: the deadline is
/// re-armed on every `write` that makes forward progress, so a slow-but-alive peer keeps
/// the transfer going and only a genuine stall (no bytes accepted for `secs`) aborts.
/// `secs == 0` disables the timeout. Unlike wrapping the whole `write_all` in one timeout,
/// this does not impose a per-chunk deadline (the old bug aborted any link below
/// `chunk_size / secs`, e.g. ~17 KiB/s for a 1 MiB chunk at 60 s).
async fn write_all_idle<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buf: &[u8],
    secs: u64,
) -> Result<()> {
    if secs == 0 {
        stream.write_all(buf).await?;
        return Ok(());
    }
    let dur = Duration::from_secs(secs);
    let mut off = 0;
    while off < buf.len() {
        match tokio::time::timeout(dur, stream.write(&buf[off..])).await {
            Ok(Ok(0)) => bail!("transfer stalled: peer is not accepting data"),
            Ok(Ok(n)) => off += n,
            Ok(Err(err)) => return Err(err).context("failed to write transfer data"),
            Err(_) => bail!("transfer stalled: no progress for {secs}s"),
        }
    }
    Ok(())
}

/// Read exactly `buf.len()` bytes, enforcing `secs` as a true **idle** timeout: the deadline
/// is re-armed on every `read` that delivers bytes. Mirrors [`write_all_idle`].
async fn read_exact_idle<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut [u8],
    secs: u64,
) -> Result<()> {
    if secs == 0 {
        stream.read_exact(buf).await?;
        return Ok(());
    }
    let dur = Duration::from_secs(secs);
    let total = buf.len();
    let mut off = 0;
    while off < total {
        match tokio::time::timeout(dur, stream.read(&mut buf[off..])).await {
            Ok(Ok(0)) => bail!("unexpected EOF mid-chunk after {off}/{total} bytes"),
            Ok(Ok(n)) => off += n,
            Ok(Err(err)) => return Err(err).context("failed to read transfer data"),
            Err(_) => bail!("transfer stalled: no progress for {secs}s"),
        }
    }
    Ok(())
}

/// cpu-based default for `--parallel 0` / the listener's carrier hint: one worker per core,
/// floored at 4 and capped at [`MAX_PARALLEL`].
fn default_parallel_hint() -> u16 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u16)
        .unwrap_or(4)
        .clamp(4, MAX_PARALLEL)
}

/// Resolve `--parallel`: `0` ⇒ [`default_parallel_hint`], otherwise the request clamped to
/// `[1, MAX_PARALLEL]`.
fn resolve_parallel(requested: u16) -> u16 {
    if requested == 0 {
        default_parallel_hint()
    } else {
        requested.clamp(1, MAX_PARALLEL)
    }
}

/// Resolve `--carriers` for a transfer. `0` ⇒ **auto**: match `parallel_hint` so each relay
/// worker substream rides its own TCP carrier (independent congestion window, no cross-stream
/// head-of-line blocking), capped at [`AUTO_CARRIER_CAP`]. An explicit value passes through
/// unchanged — `1` forces the byte-for-byte single-connection path. Carriers only affect the
/// relay path; the direct UDP path uses independent QUIC streams regardless.
fn resolve_carriers(requested: u16, parallel_hint: u16) -> u16 {
    if requested == 0 {
        parallel_hint.clamp(1, AUTO_CARRIER_CAP)
    } else {
        requested
    }
}

async fn connect_local(addr: std::net::SocketAddr) -> Result<TcpStream> {
    let mut last_err = None;
    for _ in 0..LOCAL_CONNECT_RETRIES {
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                tune_tcp(&stream);
                return Ok(stream);
            }
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

/// Like `expect_frame` but replaces the generic EOF error with a targeted message
/// that tells the user the listener is not running — without swallowing peer-sent
/// error frames, which still propagate as "peer reported an error: …".
async fn recv_manifest_accepted(stream: &mut TcpStream, transfer_id: &str) -> Result<Frame> {
    match recv_frame(stream).await? {
        Some(Frame::Error { message }) => bail!("peer reported an error: {message}"),
        Some(frame) => Ok(frame),
        None => bail!(
            "listener did not respond to transfer manifest — \
             is 'bore transfer listener' running with --transfer-id {transfer_id}?"
        ),
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

async fn sync_staged_files(paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let paths = paths.to_vec();
    spawn_blocking(move || {
        for path in paths {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| {
                    format!("failed to reopen staged file {} for sync", path.display())
                })?;
            file.sync_data()
                .with_context(|| format!("failed to sync staged file {}", path.display()))?;
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("stage file sync task failed")?
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

/// TEST SEAM: env-var injection points used by integration tests.
/// All names are prefixed with BORE_TEST_ / BORE_TRANSFER_TEST_ so they are
/// never set accidentally in production. In non-test builds a one-time warning
/// is emitted if any seam variable is active at startup.
mod test_seam {
    /// Call once at listener/sender startup to surface accidental seam use in production.
    pub fn warn_if_active() {
        #[cfg(not(test))]
        {
            use std::sync::OnceLock;
            use tracing::warn;
            static WARNED: OnceLock<()> = OnceLock::new();
            let active = [
                "BORE_TRANSFER_TEST_MAX_CHUNKS",
                "BORE_TEST_CONFIRM_RESPONSE",
            ]
            .iter()
            .any(|v| std::env::var(v).is_ok());
            if active {
                WARNED.get_or_init(|| {
                    warn!(
                        "BORE_TEST_* env vars are set — test seams are active in a non-test build"
                    );
                });
            }
        }
    }

    /// Returns `Some(n)` when `BORE_TRANSFER_TEST_MAX_CHUNKS=n`, else `None`.
    pub fn fail_after_chunks() -> Option<u64> {
        std::env::var("BORE_TRANSFER_TEST_MAX_CHUNKS")
            .ok()
            .and_then(|v| v.parse().ok())
    }

    /// Returns `Some(line)` when `BORE_TEST_CONFIRM_RESPONSE` is set, else `None`.
    pub fn confirm_response() -> Option<String> {
        std::env::var("BORE_TEST_CONFIRM_RESPONSE")
            .ok()
            .map(|v| format!("{v}\n"))
    }
}

fn injected_fail_after_chunks() -> Option<u64> {
    test_seam::fail_after_chunks()
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
    let elapsed_secs = elapsed.as_secs_f64().max(0.001);
    let current = shared.current.lock().expect("progress mutex").clone();
    let bytes = shared.bytes_done.load(Ordering::Relaxed);
    let resumed = shared.resumed_bytes.load(Ordering::Relaxed);
    let files = shared.files_done.load(Ordering::Relaxed);
    let workers = shared.workers.load(Ordering::Relaxed);
    let speed = human_bytes((bytes as f64 / elapsed_secs) as u64);
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
    let files_str = if shared.total_files > 0 {
        format!(" files {files}/{}", shared.total_files)
    } else {
        String::new()
    };
    let resumed_str = if resumed > 0 {
        format!(" resumed {}", human_bytes(resumed))
    } else {
        String::new()
    };
    let workers_str = if workers > 0 {
        format!(" workers {workers}")
    } else {
        String::new()
    };
    // Show current file name only during transfer; skip if root entry (empty rel_path)
    let file_str = if !done && !current.is_empty() {
        format!(" {}", truncate_item(&current))
    } else {
        String::new()
    };
    // \x1b[K clears from cursor to end of line, removing leftover chars from shorter \r overwrites
    let suffix = if done { "\n" } else { "\r" };
    eprint!(
        "[{}]{}{}{}{} {}/s{}\x1b[K{}",
        shared.label, ratio, files_str, resumed_str, workers_str, speed, file_str, suffix,
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

fn format_duration(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let m = (secs / 60.0) as u64;
        let s = secs as u64 % 60;
        format!("{m}m{s:02}s")
    }
}

fn print_transfer_done(
    label: &str,
    file_name: &str,
    total_bytes: u64,
    elapsed: Duration,
    dest: Option<&Path>,
) {
    let elapsed_secs = elapsed.as_secs_f64().max(0.001);
    let avg_speed = human_bytes((total_bytes as f64 / elapsed_secs) as u64);
    let dest_str = dest
        .map(|p| format!(" → {}", p.display()))
        .unwrap_or_default();
    eprintln!(
        "[{label}] complete: {file_name} — {} in {} — {avg_speed}/s avg — checksums ok{dest_str}",
        human_bytes(total_bytes),
        format_duration(elapsed_secs),
    );
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

    /// Serializes the tests that mutate the process-global
    /// `BORE_TEST_CONFIRM_RESPONSE` env var so they don't race under the parallel
    /// test runner (one test's value/removal must not leak into another).
    static CONFIRM_ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    // ── chunk math ────────────────────────────────────────────────────────────

    #[test]
    fn chunk_count_zero_byte_file() {
        assert_eq!(chunk_count_for(0), 0);
    }

    #[test]
    fn chunk_count_exactly_one_chunk() {
        assert_eq!(chunk_count_for(CHUNK_SIZE as u64), 1);
    }

    #[test]
    fn chunk_count_one_byte_over_chunk() {
        assert_eq!(chunk_count_for(CHUNK_SIZE as u64 + 1), 2);
    }

    #[test]
    fn chunk_count_large_file() {
        let size = CHUNK_SIZE as u64 * 5 + 1;
        assert_eq!(chunk_count_for(size), 6);
    }

    #[test]
    fn chunk_len_first_chunk_full_size() {
        let size = CHUNK_SIZE as u64 * 3;
        assert_eq!(chunk_len(size, 0), CHUNK_SIZE as u64);
    }

    #[test]
    fn chunk_len_last_chunk_partial() {
        let partial: u64 = 42;
        let size = CHUNK_SIZE as u64 + partial;
        assert_eq!(chunk_len(size, 1), partial);
    }

    #[test]
    fn chunk_len_zero_byte_file_is_zero() {
        assert_eq!(chunk_len(0, 0), 0);
    }

    // ── manifest validation ───────────────────────────────────────────────────

    fn make_begin(root_source: RootSourceKind) -> BeginFrame {
        BeginFrame {
            protocol_version: PROTOCOL_VERSION,
            transfer_id: "test-id".to_string(),
            root_name: encode_component_os(OsStr::new("root")),
            root_source,
            total_entries: 0,
            total_bytes: None,
            transport: TransportMode {
                direct_udp: false,
                relay_tls: false,
            },
            requested_parallel: 1,
            multi_source: false,
        }
    }

    fn make_file_entry(id: u32, rel_path: &str, size: u64) -> ManifestEntry {
        ManifestEntry {
            id,
            rel_path: rel_path.to_string(),
            kind: EntryKind::RegularFile,
            size: Some(size),
            full_hash: Some("aa".repeat(32)),
            chunk_count: chunk_count_for(size),
            symlink_target: None,
            device: None,
            mode: None,
        }
    }

    fn make_dir_entry(id: u32, rel_path: &str) -> ManifestEntry {
        ManifestEntry {
            id,
            rel_path: rel_path.to_string(),
            kind: EntryKind::Directory,
            size: None,
            full_hash: None,
            chunk_count: 0,
            symlink_target: None,
            device: None,
            mode: None,
        }
    }

    #[test]
    fn validate_manifest_single_file_ok() {
        let begin = make_begin(RootSourceKind::Filesystem);
        let entries = vec![make_file_entry(0, "", 100)];
        assert!(validate_manifest(&begin, &entries).is_ok());
    }

    #[test]
    fn validate_manifest_dir_with_child_ok() {
        let begin = make_begin(RootSourceKind::Filesystem);
        let entries = vec![make_dir_entry(0, ""), make_file_entry(1, "child.txt", 10)];
        assert!(validate_manifest(&begin, &entries).is_ok());
    }

    #[test]
    fn validate_manifest_rejects_empty() {
        let begin = make_begin(RootSourceKind::Filesystem);
        assert!(validate_manifest(&begin, &[]).is_err());
    }

    #[test]
    fn validate_manifest_rejects_non_empty_root_rel_path() {
        let begin = make_begin(RootSourceKind::Filesystem);
        let entries = vec![make_file_entry(0, "bad", 10)];
        assert!(validate_manifest(&begin, &entries).is_err());
    }

    #[test]
    fn validate_manifest_rejects_duplicate_ids() {
        let begin = make_begin(RootSourceKind::Filesystem);
        let entries = vec![
            make_dir_entry(0, ""),
            make_file_entry(0, "child.txt", 10), // same id
        ];
        assert!(validate_manifest(&begin, &entries).is_err());
    }

    #[test]
    fn validate_manifest_rejects_duplicate_paths() {
        let begin = make_begin(RootSourceKind::Filesystem);
        let entries = vec![
            make_dir_entry(0, ""),
            make_file_entry(1, "child.txt", 10),
            make_file_entry(2, "child.txt", 20), // duplicate path
        ];
        assert!(validate_manifest(&begin, &entries).is_err());
    }

    #[test]
    fn validate_manifest_rejects_file_missing_size() {
        let begin = make_begin(RootSourceKind::Filesystem);
        let mut entry = make_file_entry(0, "", 100);
        entry.size = None;
        let entries = vec![entry];
        assert!(validate_manifest(&begin, &entries).is_err());
    }

    #[test]
    fn validate_manifest_rejects_wrong_chunk_count() {
        let begin = make_begin(RootSourceKind::Filesystem);
        let mut entry = make_file_entry(0, "", 100);
        entry.chunk_count = 99; // wrong
        let entries = vec![entry];
        assert!(validate_manifest(&begin, &entries).is_err());
    }

    // ── manifest hash ─────────────────────────────────────────────────────────

    #[test]
    fn manifest_hash_is_deterministic() {
        let entries = vec![make_dir_entry(0, ""), make_file_entry(1, "f.txt", 10)];
        let h1 = manifest_hash("root", RootSourceKind::Filesystem, &entries).unwrap();
        let h2 = manifest_hash("root", RootSourceKind::Filesystem, &entries).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn manifest_hash_differs_for_different_entries() {
        let a = vec![make_dir_entry(0, ""), make_file_entry(1, "a.txt", 10)];
        let b = vec![make_dir_entry(0, ""), make_file_entry(1, "b.txt", 10)];
        let ha = manifest_hash("root", RootSourceKind::Filesystem, &a).unwrap();
        let hb = manifest_hash("root", RootSourceKind::Filesystem, &b).unwrap();
        assert_ne!(ha, hb);
    }

    // ── summary ───────────────────────────────────────────────────────────────

    #[test]
    fn summary_counts_entries_correctly() {
        let entries = vec![
            make_dir_entry(0, ""),
            make_file_entry(1, "a.txt", 100),
            make_file_entry(2, "b.txt", 200),
        ];
        let summary = summary_from_materialized_entries(&entries).unwrap();
        assert_eq!(summary.regular_files, 2);
        assert_eq!(summary.directories, 1);
        assert_eq!(summary.total_bytes, 300);
    }

    #[test]
    fn summary_is_deterministic() {
        let entries = vec![make_dir_entry(0, ""), make_file_entry(1, "f.txt", 10)];
        let s1 = summary_from_materialized_entries(&entries).unwrap();
        let s2 = summary_from_materialized_entries(&entries).unwrap();
        assert_eq!(s1.transfer_hash, s2.transfer_hash);
    }

    // ── build_chunk_tasks ─────────────────────────────────────────────────────

    #[test]
    fn build_chunk_tasks_generates_tasks_for_regular_files() {
        let src = PathBuf::from("/tmp/dummy");
        let plan = PlannedTransfer {
            transfer_id: "t".to_string(),
            root_name: encode_component_os(OsStr::new("root")),
            root_source: RootSourceKind::Filesystem,
            entries: vec![
                PlannedEntry {
                    manifest: make_dir_entry(0, ""),
                    source_path: None,
                },
                PlannedEntry {
                    manifest: make_file_entry(1, "f.txt", CHUNK_SIZE as u64 + 1),
                    source_path: Some(src.clone()),
                },
            ],
            total_bytes: None,
            parallel: 1,
            multi_source: false,
        };
        let tasks = build_chunk_tasks(&plan, &[]).unwrap();
        assert_eq!(tasks.len(), 2, "two chunks: full + partial");
        assert_eq!(tasks[0].chunk_index, 0);
        assert_eq!(tasks[1].chunk_index, 1);
    }

    #[test]
    fn build_chunk_tasks_skips_resumed_chunks() {
        let src = PathBuf::from("/tmp/dummy");
        let plan = PlannedTransfer {
            transfer_id: "t".to_string(),
            root_name: encode_component_os(OsStr::new("root")),
            root_source: RootSourceKind::Filesystem,
            entries: vec![PlannedEntry {
                manifest: make_file_entry(0, "", CHUNK_SIZE as u64 * 3),
                source_path: Some(src.clone()),
            }],
            total_bytes: None,
            parallel: 1,
            multi_source: false,
        };
        let resume = vec![ResumeFilePlan {
            entry_id: 0,
            completed_chunks: vec![0, 2], // chunks 0 and 2 already done
        }];
        let tasks = build_chunk_tasks(&plan, &resume).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].chunk_index, 1);
    }

    // ── multi_source flag ─────────────────────────────────────────────────────

    #[test]
    fn scan_multi_filesystem_sets_multi_source_without_output() {
        // Create two temp files to scan
        let dir =
            std::env::temp_dir().join(format!("bore-test-scan-multi-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("a.txt");
        let f2 = dir.join("b.txt");
        std::fs::write(&f1, b"aa").unwrap();
        std::fs::write(&f2, b"bb").unwrap();

        let plan = scan_multi_filesystem_transfer(
            "tid".to_string(),
            vec![f1, f2],
            None, // no --output → flat mode
            SymlinkMode::Exclude,
            DeviceMode::Exclude,
            1,
        )
        .unwrap();

        assert!(
            plan.multi_source,
            "multi_source must be true without --output"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_multi_filesystem_clears_multi_source_with_output() {
        let dir =
            std::env::temp_dir().join(format!("bore-test-scan-output-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("a.txt");
        let f2 = dir.join("b.txt");
        std::fs::write(&f1, b"aa").unwrap();
        std::fs::write(&f2, b"bb").unwrap();

        let plan = scan_multi_filesystem_transfer(
            "tid".to_string(),
            vec![f1, f2],
            Some(PathBuf::from("bundle")),
            SymlinkMode::Exclude,
            DeviceMode::Exclude,
            1,
        )
        .unwrap();

        assert!(
            !plan.multi_source,
            "multi_source must be false with --output"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── path helpers ──────────────────────────────────────────────────────────

    #[test]
    fn encode_decode_relative_path_round_trip() {
        let path = Path::new("dir/sub/file.txt");
        let encoded = encode_relative_path(path).unwrap();
        let decoded = decode_relative_path(&encoded).unwrap();
        assert_eq!(decoded, path);
    }

    #[test]
    fn encode_relative_path_empty_is_empty_string() {
        let encoded = encode_relative_path(Path::new("")).unwrap();
        assert!(encoded.is_empty());
    }

    #[test]
    fn human_bytes_formats_correctly() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1024), "1.0KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0MiB");
    }

    #[test]
    fn format_duration_short() {
        assert_eq!(format_duration(5.5), "5.5s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(90.0), "1m30s");
    }

    // ── receiver confirmation helpers ─────────────────────────────────────────

    fn make_stdin_begin() -> BeginFrame {
        BeginFrame {
            protocol_version: PROTOCOL_VERSION,
            transfer_id: "test-stdin".to_string(),
            root_name: "u:payload.bin".to_string(),
            root_source: RootSourceKind::Stdin,
            total_entries: 1,
            total_bytes: None,
            transport: TransportMode {
                direct_udp: false,
                relay_tls: false,
            },
            requested_parallel: 1,
            multi_source: false,
        }
    }

    fn make_stdin_entry() -> ManifestEntry {
        ManifestEntry {
            id: 0,
            rel_path: String::new(),
            kind: EntryKind::RegularFile,
            size: None,
            full_hash: None,
            chunk_count: 0,
            symlink_target: None,
            device: None,
            mode: None,
        }
    }

    /// display_and_confirm_manifest_sync must return Ok(true) for stdin transfers
    /// even when ask_confirm=true and the canned response is "n".  The stdin
    /// branch exits before calling read_confirmation_line.
    #[test]
    fn receiver_ask_confirm_ignored_for_stdin() {
        let _env = CONFIRM_ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Inject "n" — the stdin branch must bypass this entirely.
        std::env::set_var("BORE_TEST_CONFIRM_RESPONSE", "n");
        let result = display_and_confirm_manifest_sync(
            &make_stdin_begin(),
            &[make_stdin_entry()],
            Path::new("/tmp"),
            true, // ask_confirm = true
        );
        std::env::remove_var("BORE_TEST_CONFIRM_RESPONSE");
        assert!(
            result.unwrap(),
            "stdin transfer with ask_confirm=true must auto-accept"
        );
    }

    /// display_and_confirm_manifest_sync with ask_confirm=false always returns true.
    #[test]
    fn receiver_no_ask_confirm_always_accepts() {
        let _env = CONFIRM_ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Even with "n" injected, ask_confirm=false bypasses the prompt entirely.
        std::env::set_var("BORE_TEST_CONFIRM_RESPONSE", "n");
        let begin = BeginFrame {
            protocol_version: PROTOCOL_VERSION,
            transfer_id: "test-noask".to_string(),
            root_name: "u:file.txt".to_string(),
            root_source: RootSourceKind::Filesystem,
            total_entries: 1,
            total_bytes: Some(10),
            transport: TransportMode {
                direct_udp: false,
                relay_tls: false,
            },
            requested_parallel: 1,
            multi_source: false,
        };
        let entry = ManifestEntry {
            id: 0,
            rel_path: String::new(),
            kind: EntryKind::RegularFile,
            size: Some(10),
            full_hash: None,
            chunk_count: 1,
            symlink_target: None,
            device: None,
            mode: None,
        };
        let result = display_and_confirm_manifest_sync(&begin, &[entry], Path::new("/tmp"), false);
        std::env::remove_var("BORE_TEST_CONFIRM_RESPONSE");
        assert!(result.unwrap(), "no ask_confirm must always auto-accept");
    }

    /// display_and_confirm_manifest_sync with ask_confirm=true and response "y" accepts.
    #[test]
    fn receiver_ask_confirm_accepts_on_y() {
        let _env = CONFIRM_ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("BORE_TEST_CONFIRM_RESPONSE", "y");
        let begin = BeginFrame {
            protocol_version: PROTOCOL_VERSION,
            transfer_id: "test-y".to_string(),
            root_name: "u:file.txt".to_string(),
            root_source: RootSourceKind::Filesystem,
            total_entries: 1,
            total_bytes: Some(5),
            transport: TransportMode {
                direct_udp: false,
                relay_tls: false,
            },
            requested_parallel: 1,
            multi_source: false,
        };
        let entry = ManifestEntry {
            id: 0,
            rel_path: String::new(),
            kind: EntryKind::RegularFile,
            size: Some(5),
            full_hash: None,
            chunk_count: 1,
            symlink_target: None,
            device: None,
            mode: None,
        };
        let result = display_and_confirm_manifest_sync(&begin, &[entry], Path::new("/tmp"), true);
        std::env::remove_var("BORE_TEST_CONFIRM_RESPONSE");
        assert!(result.unwrap(), "response 'y' must accept");
    }

    /// display_and_confirm_manifest_sync with ask_confirm=true and response "n" rejects.
    #[test]
    fn receiver_ask_confirm_rejects_on_n() {
        let _env = CONFIRM_ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("BORE_TEST_CONFIRM_RESPONSE", "n");
        let begin = BeginFrame {
            protocol_version: PROTOCOL_VERSION,
            transfer_id: "test-n".to_string(),
            root_name: "u:file.txt".to_string(),
            root_source: RootSourceKind::Filesystem,
            total_entries: 1,
            total_bytes: Some(5),
            transport: TransportMode {
                direct_udp: false,
                relay_tls: false,
            },
            requested_parallel: 1,
            multi_source: false,
        };
        let entry = ManifestEntry {
            id: 0,
            rel_path: String::new(),
            kind: EntryKind::RegularFile,
            size: Some(5),
            full_hash: None,
            chunk_count: 1,
            symlink_target: None,
            device: None,
            mode: None,
        };
        let result = display_and_confirm_manifest_sync(&begin, &[entry], Path::new("/tmp"), true);
        std::env::remove_var("BORE_TEST_CONFIRM_RESPONSE");
        assert!(!result.unwrap(), "response 'n' must reject");
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

    // ── P1.1: tune_tcp applied to transfer sockets ────────────────────────────

    #[tokio::test]
    async fn connect_local_sets_nodelay() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (connected, _) = tokio::join!(connect_local(addr), async {
            listener.accept().await.unwrap()
        });
        let stream = connected.unwrap();
        assert!(
            stream.nodelay().unwrap(),
            "connect_local must set TCP_NODELAY"
        );
    }

    // ── P1.2: chunk geometry validation ──────────────────────────────────────

    fn make_single_chunk_entry(size: u64) -> ManifestEntry {
        ManifestEntry {
            id: 1,
            rel_path: "test.bin".to_string(),
            kind: EntryKind::RegularFile,
            size: Some(size),
            full_hash: None,
            chunk_count: chunk_count_for(size),
            symlink_target: None,
            device: None,
            mode: None,
        }
    }

    #[test]
    fn chunk_geometry_valid_first_chunk_accepted() {
        let size = CHUNK_SIZE as u64 * 2 + 42;
        let entry = make_single_chunk_entry(size);
        assert!(validate_chunk_geometry(&entry, 0, 0, CHUNK_SIZE).is_ok());
    }

    #[test]
    fn chunk_geometry_valid_last_partial_chunk_accepted() {
        let size = CHUNK_SIZE as u64 + 42;
        let entry = make_single_chunk_entry(size);
        assert!(validate_chunk_geometry(&entry, 1, CHUNK_SIZE as u64, 42).is_ok());
    }

    #[test]
    fn chunk_start_index_out_of_range_rejected() {
        let size = CHUNK_SIZE as u64;
        let entry = make_single_chunk_entry(size); // chunk_count == 1
        let err = validate_chunk_geometry(&entry, 1, CHUNK_SIZE as u64, 0).unwrap_err();
        assert!(
            err.to_string().contains("out of range"),
            "error should say out of range: {err}"
        );
    }

    #[test]
    fn chunk_start_oversized_len_rejected() {
        let size = CHUNK_SIZE as u64 * 2;
        let entry = make_single_chunk_entry(size);
        // correct index/offset, but 4× the expected length
        let err = validate_chunk_geometry(&entry, 0, 0, CHUNK_SIZE * 4).unwrap_err();
        assert!(
            err.to_string().contains("geometry mismatch"),
            "error should say geometry mismatch: {err}"
        );
    }

    #[test]
    fn chunk_start_offset_mismatch_rejected() {
        let size = CHUNK_SIZE as u64 * 2;
        let entry = make_single_chunk_entry(size);
        let err = validate_chunk_geometry(&entry, 0, 1, CHUNK_SIZE).unwrap_err();
        assert!(
            err.to_string().contains("geometry mismatch"),
            "error should say geometry mismatch: {err}"
        );
    }

    #[test]
    fn stream_chunk_max_equals_chunk_size() {
        // STREAM_CHUNK_MAX is the allocation bound for stdin StreamChunk payloads.
        // It must equal CHUNK_SIZE so the geometry guard and the stdin guard are consistent.
        assert_eq!(STREAM_CHUNK_MAX, CHUNK_SIZE as usize);
    }

    // --- with_stall unit tests (P3) ---

    #[tokio::test]
    async fn with_stall_fires_after_timeout() {
        let result = with_stall(1u64, async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<(), anyhow::Error>(())
        })
        .await;
        assert!(result.is_err(), "with_stall should return Err on timeout");
        assert!(
            result.unwrap_err().to_string().contains("stalled"),
            "error message should contain 'stalled'"
        );
    }

    #[tokio::test]
    async fn with_stall_zero_disables_timeout() {
        // secs=0 means disabled; a quick future should return Ok.
        let result = with_stall(0u64, async { Ok::<u32, anyhow::Error>(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn with_stall_passes_through_ok() {
        let result = with_stall(30u64, async { Ok::<&str, anyhow::Error>("done") }).await;
        assert_eq!(result.unwrap(), "done");
    }

    #[tokio::test]
    async fn with_stall_propagates_inner_error() {
        let result = with_stall(30u64, async {
            anyhow::bail!("inner error");
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        })
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("inner error"));
    }

    // ----- Fix A: carrier / parallel resolution -----

    #[test]
    fn resolve_parallel_auto_and_clamp() {
        assert_eq!(resolve_parallel(0), default_parallel_hint());
        assert!((4..=MAX_PARALLEL).contains(&default_parallel_hint()));
        assert_eq!(resolve_parallel(1), 1);
        assert_eq!(resolve_parallel(7), 7);
        assert_eq!(resolve_parallel(1000), MAX_PARALLEL);
    }

    #[test]
    fn resolve_carriers_auto_tracks_parallel_capped() {
        // auto (0): match the parallelism hint, capped at the server's default max.
        assert_eq!(resolve_carriers(0, 8), 8);
        assert_eq!(
            resolve_carriers(0, AUTO_CARRIER_CAP + 100),
            AUTO_CARRIER_CAP
        );
        assert_eq!(resolve_carriers(0, 0), 1); // never below one
    }

    #[test]
    fn resolve_carriers_explicit_passes_through() {
        // Explicit values are untouched here (the server still enforces --max-carriers).
        assert_eq!(resolve_carriers(1, 32), 1); // single-connection path preserved
        assert_eq!(resolve_carriers(4, 32), 4); // even below the hint
        assert_eq!(resolve_carriers(64, 8), 64); // even above the auto cap
    }

    // ----- Fix B: idle (no-progress) stall timeout -----

    #[tokio::test]
    async fn idle_helpers_passthrough_when_disabled() {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        write_all_idle(&mut a, &[9u8; 4096], 0).await.unwrap();
        let mut buf = vec![0u8; 4096];
        read_exact_idle(&mut b, &mut buf, 0).await.unwrap();
        assert!(buf.iter().all(|&x| x == 9));
    }

    #[tokio::test]
    async fn read_exact_idle_tolerates_slow_but_alive_writer() {
        // Six 16 KiB slices, 250 ms apart: every gap is below the 1 s timeout but the total
        // (~1.5 s) exceeds it. A whole-operation deadline (the old bug) would abort; a true
        // idle timeout must not.
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        let writer = tokio::spawn(async move {
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                a.write_all(&[1u8; 16 * 1024]).await.unwrap();
            }
        });
        let mut buf = vec![0u8; 6 * 16 * 1024];
        read_exact_idle(&mut b, &mut buf, 1).await.unwrap();
        writer.await.unwrap();
        assert!(buf.iter().all(|&x| x == 1));
    }

    #[tokio::test]
    async fn read_exact_idle_aborts_on_true_stall() {
        // 16 KiB arrives, then the writer goes silent (stream stays open → no EOF).
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        a.write_all(&[7u8; 16 * 1024]).await.unwrap();
        let mut buf = vec![0u8; 32 * 1024];
        let err = read_exact_idle(&mut b, &mut buf, 1).await.unwrap_err();
        assert!(
            err.to_string().contains("no progress"),
            "expected idle-stall error, got: {err}"
        );
        drop(a); // keep the write half alive until after the assertion
    }

    #[tokio::test]
    async fn write_all_idle_aborts_when_peer_never_drains() {
        // 16 KiB pipe, peer never reads: once the buffer fills, writes make no progress.
        let (_peer, mut b) = tokio::io::duplex(16 * 1024);
        let err = write_all_idle(&mut b, &vec![5u8; 256 * 1024], 1)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no progress") || err.to_string().contains("not accepting"),
            "expected idle-stall error, got: {err}"
        );
    }

    // ----- Worker-accept liveness: receiver must not hang if the sender vanishes -----

    /// Build a connected TCP pair on loopback; returns (client_side, server_side).
    async fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    }

    #[tokio::test]
    async fn accept_worker_stream_aborts_when_control_closes() {
        // Receiver's control stream; the sender side is dropped to simulate the sender dying
        // after ManifestAccepted but before opening any data stream.
        let (client, mut control) = tcp_pair().await;
        let (_tx, mut rx) = mpsc::unbounded_channel::<TcpStream>();
        drop(client); // sender vanishes
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            accept_worker_stream(&mut rx, &mut control, 0),
        )
        .await
        .expect("accept_worker_stream hung after the control connection closed");
        let err = res.expect_err("expected an error when the control connection closed");
        assert!(
            err.to_string().contains("sender disconnected"),
            "expected a sender-disconnected error, got: {err}"
        );
    }

    #[tokio::test]
    async fn accept_worker_stream_returns_a_connected_worker() {
        let (_client, mut control) = tcp_pair().await; // control stays open
        let (tx, mut rx) = mpsc::unbounded_channel::<TcpStream>();
        let (worker, _peer) = tcp_pair().await;
        tx.send(worker).unwrap();
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            accept_worker_stream(&mut rx, &mut control, 0),
        )
        .await
        .expect("accept_worker_stream hung")
        .expect("expected the queued worker stream");
        drop(got);
        drop(_client);
    }

    #[tokio::test]
    async fn accept_worker_stream_times_out_when_idle() {
        // Control open, no worker ever arrives: the idle stall timeout must fire.
        let (_client, mut control) = tcp_pair().await;
        let (_tx, mut rx) = mpsc::unbounded_channel::<TcpStream>();
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            accept_worker_stream(&mut rx, &mut control, 1),
        )
        .await
        .expect("accept_worker_stream hung instead of honoring stall_timeout")
        .expect_err("expected a stall timeout");
        assert!(
            err.to_string().contains("timed out"),
            "expected a timeout error, got: {err}"
        );
        drop(_client);
    }
}
