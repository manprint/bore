//! Secure file transfer built on top of bore's existing secret-tunnel transport.

#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs::{self, File};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::info;
use uuid::Uuid;

use crate::client::{Client, ProviderMeta};
use crate::secret::Proxy;
use crate::server::DEFAULT_MAX_CONNS;
use crate::transport::Endpoint;

const PROTOCOL_VERSION: u32 = 1;
const FRAME_LIMIT: usize = 16 * 1024 * 1024;
const MANIFEST_CHUNK: usize = 128;
const COPY_BUFFER: usize = 64 * 1024;
const LOCAL_BIND: &str = "127.0.0.1:0";
const LOCAL_HOST: &str = "127.0.0.1";
const LOCAL_CONNECT_RETRIES: usize = 50;
const LOCAL_CONNECT_DELAY: Duration = Duration::from_millis(20);

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ManifestEntry {
    rel_path: String,
    kind: EntryKind,
    size: Option<u64>,
    symlink_target: Option<String>,
    device: Option<DeviceDescriptor>,
    mode: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceDescriptor {
    major: u64,
    minor: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
enum Frame {
    Begin(BeginFrame),
    ManifestChunk { entries: Vec<ManifestEntry> },
    ManifestDone,
    ManifestAccepted { final_name: String },
    FileStart { rel_path: String, size: u64 },
    FileEnd { rel_path: String, blake3: String },
    StreamChunk { len: u32 },
    StreamEnd { size: u64, blake3: String },
    TransferSummary(TransferSummary),
    Completed(CompletedFrame),
    Error { message: String },
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
}

#[derive(Clone, Debug, Default)]
struct TransferCounts {
    regular_files: u64,
    directories: u64,
    symlinks: u64,
    devices: u64,
    total_bytes: u64,
}

#[derive(Debug)]
struct ReceiverPlan {
    begin: BeginFrame,
    entries: Vec<ManifestEntry>,
    final_name: String,
    final_path: PathBuf,
    stage_base: PathBuf,
    stage_root: PathBuf,
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
        stun_server = ?options.stun_server.as_deref(),
        carriers = options.carriers,
        relay_security = if endpoint.tls { "tls" } else { "plain" },
        "transfer listener starting"
    );

    let listener = TcpListener::bind(LOCAL_BIND)
        .await
        .context("failed to bind transfer listener loopback port")?;
    let local_port = listener.local_addr()?.port();
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
        options.carriers,
        ProviderMeta::default(),
    )
    .await?;
    let provider_task = tokio::spawn(provider.listen());

    println!(
        "waiting for transfer {transfer_id} into {}",
        options.dest_path.display()
    );
    let outcome = async {
        let (stream, _) = listener
            .accept()
            .await
            .context("failed to accept transfer stream")?;
        receive_transfer(stream, options.dest_path.clone(), options.collision).await
    }
    .await;
    provider_task.abort();
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
    let plan = plan_transfer(
        transfer_id.clone(),
        &options.source,
        options.output.as_deref(),
        options.symlinks,
        options.devices,
    )
    .await?;
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
        options.carriers,
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
        "transfer sender transport ready"
    );

    let local_addr = proxy.local_addr()?;
    let proxy_task = tokio::spawn(proxy.listen());
    let outcome = async {
        let stream = connect_local(local_addr).await?;
        send_transfer(stream, plan, transport).await
    }
    .await;
    proxy_task.abort();
    outcome
}

async fn send_transfer(
    mut stream: TcpStream,
    plan: PlannedTransfer,
    transport: TransportMode,
) -> Result<TransferOutcome> {
    let regular_files_total = plan
        .entries
        .iter()
        .filter(|entry| entry.manifest.kind.is_regular_file())
        .count() as u64;
    let mut progress = ProgressPrinter::new("sender", plan.total_bytes, regular_files_total);
    let begin = BeginFrame {
        protocol_version: PROTOCOL_VERSION,
        transfer_id: plan.transfer_id.clone(),
        root_name: plan.root_name.clone(),
        root_source: plan.root_source,
        total_entries: plan.entries.len() as u64,
        total_bytes: plan.total_bytes,
        transport,
    };
    send_frame(&mut stream, &Frame::Begin(begin.clone())).await?;
    for chunk in plan.entries.chunks(MANIFEST_CHUNK) {
        send_frame(
            &mut stream,
            &Frame::ManifestChunk {
                entries: chunk.iter().map(|entry| entry.manifest.clone()).collect(),
            },
        )
        .await?;
    }
    send_frame(&mut stream, &Frame::ManifestDone).await?;
    let accepted = expect_manifest_accepted(&mut stream).await?;
    info!(
        transfer_id = %plan.transfer_id,
        final_name = %accepted,
        transport = %transport,
        "transfer manifest accepted"
    );

    let mut transfer_hasher = blake3::Hasher::new();
    let mut counts = TransferCounts::default();
    for entry in &plan.entries {
        match entry.manifest.kind {
            EntryKind::Directory => {
                counts.directories += 1;
                update_transfer_hash(&mut transfer_hasher, &entry.manifest, None)?;
            }
            EntryKind::Symlink => {
                counts.symlinks += 1;
                update_transfer_hash(&mut transfer_hasher, &entry.manifest, None)?;
            }
            EntryKind::CharDevice | EntryKind::BlockDevice => {
                counts.devices += 1;
                update_transfer_hash(&mut transfer_hasher, &entry.manifest, None)?;
            }
            EntryKind::RegularFile => {
                counts.regular_files += 1;
                progress.start_path(&display_rel_path(&entry.manifest.rel_path));
                if plan.root_source == RootSourceKind::Stdin && entry.manifest.size.is_none() {
                    send_stdin_payload(&mut stream, &entry.manifest, &mut progress).await?;
                    let frame = expect_frame(&mut stream).await?;
                    let (size, hash) = match frame {
                        Frame::StreamEnd { size, blake3 } => (size, blake3),
                        other => bail!("unexpected frame after stdin payload: {other:?}"),
                    };
                    counts.total_bytes += size;
                    let manifest = ManifestEntry {
                        size: Some(size),
                        ..entry.manifest.clone()
                    };
                    update_transfer_hash(&mut transfer_hasher, &manifest, Some(hash.as_str()))?;
                } else {
                    let size = entry
                        .manifest
                        .size
                        .context("regular file missing manifest size")?;
                    send_frame(
                        &mut stream,
                        &Frame::FileStart {
                            rel_path: entry.manifest.rel_path.clone(),
                            size,
                        },
                    )
                    .await?;
                    let hash = send_regular_file(
                        &mut stream,
                        entry
                            .source_path
                            .as_deref()
                            .context("regular file missing source path")?,
                        size,
                        &entry.manifest.rel_path,
                        &mut progress,
                    )
                    .await?;
                    send_frame(
                        &mut stream,
                        &Frame::FileEnd {
                            rel_path: entry.manifest.rel_path.clone(),
                            blake3: hash.clone(),
                        },
                    )
                    .await?;
                    counts.total_bytes += size;
                    update_transfer_hash(
                        &mut transfer_hasher,
                        &entry.manifest,
                        Some(hash.as_str()),
                    )?;
                }
                progress.finish_file(&display_rel_path(&entry.manifest.rel_path));
            }
        }
    }
    let summary = TransferSummary {
        regular_files: counts.regular_files,
        directories: counts.directories,
        symlinks: counts.symlinks,
        devices: counts.devices,
        total_bytes: counts.total_bytes,
        transfer_hash: transfer_hasher.finalize().to_hex().to_string(),
    };
    send_frame(&mut stream, &Frame::TransferSummary(summary.clone())).await?;
    let completed = match expect_frame(&mut stream).await? {
        Frame::Completed(done) => done,
        other => bail!("unexpected final frame from receiver: {other:?}"),
    };
    if completed.transfer_hash != summary.transfer_hash {
        bail!("receiver acknowledged a different transfer hash");
    }
    progress.finish();
    Ok(TransferOutcome {
        transfer_id: plan.transfer_id,
        final_path: decode_native_path_string(&completed.final_path)?,
        total_bytes: completed.total_bytes,
        regular_files: completed.regular_files,
        transport,
    })
}

async fn receive_transfer(
    mut stream: TcpStream,
    dest_root: PathBuf,
    collision: CollisionPolicy,
) -> Result<TransferOutcome> {
    let outcome = async {
        let begin = match expect_frame(&mut stream).await? {
            Frame::Begin(begin) => begin,
            other => bail!("unexpected first frame: {other:?}"),
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
        let plan = receive_manifest(&mut stream, begin, &dest_root, collision).await?;
        send_frame(
            &mut stream,
            &Frame::ManifestAccepted {
                final_name: plan.final_name.clone(),
            },
        )
        .await?;
        let mut progress = ProgressPrinter::new(
            "listener",
            plan.begin.total_bytes,
            plan.entries
                .iter()
                .filter(|entry| entry.kind.is_regular_file())
                .count() as u64,
        );
        let (summary, completed) = receive_payload(&mut stream, &plan, &mut progress).await?;
        commit_receiver_plan(&plan).await?;
        send_frame(&mut stream, &Frame::Completed(completed.clone())).await?;
        progress.finish();
        Ok(TransferOutcome {
            transfer_id: plan.begin.transfer_id.clone(),
            final_path: plan.final_path,
            total_bytes: summary.total_bytes,
            regular_files: summary.regular_files,
            transport: plan.begin.transport,
        })
    }
    .await;

    if let Err(err) = &outcome {
        let _ = send_frame(
            &mut stream,
            &Frame::Error {
                message: err.to_string(),
            },
        )
        .await;
    }
    outcome
}

async fn receive_payload(
    stream: &mut TcpStream,
    plan: &ReceiverPlan,
    progress: &mut ProgressPrinter,
) -> Result<(TransferSummary, CompletedFrame)> {
    let mut transfer_hasher = blake3::Hasher::new();
    let mut counts = TransferCounts::default();

    create_stage_entries(plan).await?;

    for entry in &plan.entries {
        match entry.kind {
            EntryKind::Directory => {
                counts.directories += 1;
                update_transfer_hash(&mut transfer_hasher, entry, None)?;
            }
            EntryKind::Symlink => {
                counts.symlinks += 1;
                update_transfer_hash(&mut transfer_hasher, entry, None)?;
            }
            EntryKind::CharDevice | EntryKind::BlockDevice => {
                counts.devices += 1;
                update_transfer_hash(&mut transfer_hasher, entry, None)?;
            }
            EntryKind::RegularFile => {
                counts.regular_files += 1;
                progress.start_path(&display_rel_path(&entry.rel_path));
                if plan.begin.root_source == RootSourceKind::Stdin && entry.size.is_none() {
                    let (size, hash) = receive_stdin_payload(
                        stream,
                        stage_path(plan, entry),
                        entry.mode,
                        &entry.rel_path,
                        progress,
                    )
                    .await?;
                    counts.total_bytes += size;
                    let materialized = ManifestEntry {
                        size: Some(size),
                        ..entry.clone()
                    };
                    update_transfer_hash(&mut transfer_hasher, &materialized, Some(hash.as_str()))?;
                    send_frame(stream, &Frame::StreamEnd { size, blake3: hash }).await?;
                } else {
                    let size = entry.size.context("receiver manifest missing file size")?;
                    let (rel_path, announced_size) = match expect_frame(stream).await? {
                        Frame::FileStart { rel_path, size } => (rel_path, size),
                        other => bail!("unexpected frame before file payload: {other:?}"),
                    };
                    if rel_path != entry.rel_path {
                        bail!(
                            "sender file order mismatch: expected {}, got {}",
                            display_rel_path(&entry.rel_path),
                            display_rel_path(&rel_path)
                        );
                    }
                    if announced_size != size {
                        bail!(
                            "sender announced an unexpected file size for {}",
                            display_rel_path(&rel_path)
                        );
                    }
                    let hash = receive_regular_file(
                        stream,
                        stage_path(plan, entry),
                        size,
                        entry.mode,
                        &entry.rel_path,
                        progress,
                    )
                    .await?;
                    match expect_frame(stream).await? {
                        Frame::FileEnd { rel_path, blake3 } => {
                            if rel_path != entry.rel_path {
                                bail!(
                                    "file end path mismatch for {}",
                                    display_rel_path(&entry.rel_path)
                                );
                            }
                            if blake3 != hash {
                                bail!("hash mismatch for {}", display_rel_path(&entry.rel_path));
                            }
                            counts.total_bytes += size;
                            update_transfer_hash(&mut transfer_hasher, entry, Some(hash.as_str()))?;
                        }
                        other => bail!("unexpected frame after file payload: {other:?}"),
                    }
                }
                progress.finish_file(&display_rel_path(&entry.rel_path));
            }
        }
    }

    let summary = match expect_frame(stream).await? {
        Frame::TransferSummary(summary) => summary,
        other => bail!("unexpected frame before transfer completion: {other:?}"),
    };
    let local_summary = TransferSummary {
        regular_files: counts.regular_files,
        directories: counts.directories,
        symlinks: counts.symlinks,
        devices: counts.devices,
        total_bytes: counts.total_bytes,
        transfer_hash: transfer_hasher.finalize().to_hex().to_string(),
    };
    if summary.regular_files != local_summary.regular_files
        || summary.directories != local_summary.directories
        || summary.symlinks != local_summary.symlinks
        || summary.devices != local_summary.devices
        || summary.total_bytes != local_summary.total_bytes
        || summary.transfer_hash != local_summary.transfer_hash
    {
        bail!("sender summary does not match receiver state");
    }
    let completed = CompletedFrame {
        final_path: path_to_platform_string(&plan.final_path)?,
        total_bytes: local_summary.total_bytes,
        regular_files: local_summary.regular_files,
        transfer_hash: local_summary.transfer_hash.clone(),
    };
    Ok((local_summary, completed))
}

async fn receive_manifest(
    stream: &mut TcpStream,
    begin: BeginFrame,
    dest_root: &Path,
    collision: CollisionPolicy,
) -> Result<ReceiverPlan> {
    validate_root_name(&begin.root_name)?;
    let mut entries = Vec::with_capacity(begin.total_entries as usize);
    loop {
        match expect_frame(stream).await? {
            Frame::ManifestChunk { entries: chunk } => entries.extend(chunk),
            Frame::ManifestDone => break,
            Frame::Error { message } => bail!("sender reported an error: {message}"),
            other => bail!("unexpected frame while receiving manifest: {other:?}"),
        }
    }
    if entries.len() as u64 != begin.total_entries {
        bail!("manifest entry count mismatch");
    }
    validate_manifest(&begin, &entries)?;
    let final_name = resolve_final_name(dest_root, &begin.root_name, collision).await?;
    let final_name_local = decode_component_string(&final_name)?;
    let transfer_nonce = Uuid::new_v4().to_string();
    let stage_base = dest_root.join(format!(".bore-transfer-{transfer_nonce}.part"));
    let stage_root = stage_base.join(&final_name_local);
    fs::create_dir_all(&stage_base).await.with_context(|| {
        format!(
            "failed to create staging directory {}",
            stage_base.display()
        )
    })?;
    Ok(ReceiverPlan {
        begin,
        entries,
        final_name: final_name.clone(),
        final_path: dest_root.join(final_name_local),
        stage_base,
        stage_root,
    })
}

async fn create_stage_entries(plan: &ReceiverPlan) -> Result<()> {
    for entry in &plan.entries {
        let path = stage_path(plan, entry);
        match entry.kind {
            EntryKind::Directory => {
                fs::create_dir_all(&path)
                    .await
                    .with_context(|| format!("failed to create directory {}", path.display()))?;
            }
            EntryKind::Symlink => {
                let target = entry
                    .symlink_target
                    .as_ref()
                    .context("symlink entry missing target")?
                    .clone();
                ensure_parent_dir(&path).await?;
                create_symlink(&target, &path).await?;
            }
            EntryKind::CharDevice | EntryKind::BlockDevice => {
                ensure_parent_dir(&path).await?;
                create_device(entry, &path).await?;
            }
            EntryKind::RegularFile => {
                ensure_parent_dir(&path).await?;
            }
        }
    }
    Ok(())
}

async fn commit_receiver_plan(plan: &ReceiverPlan) -> Result<()> {
    let final_exists = fs::try_exists(&plan.final_path).await?;
    let mut backup = None;
    if final_exists {
        let backup_path = plan
            .stage_base
            .join(format!(".overwrite-backup-{}", plan.final_name));
        fs::rename(&plan.final_path, &backup_path)
            .await
            .with_context(|| {
                format!(
                    "failed to stage existing destination {}",
                    plan.final_path.display()
                )
            })?;
        backup = Some(backup_path);
    }
    if let Err(err) = fs::rename(&plan.stage_root, &plan.final_path)
        .await
        .with_context(|| {
            format!(
                "failed to commit staged transfer to {}",
                plan.final_path.display()
            )
        })
    {
        if let Some(backup_path) = &backup {
            let _ = fs::rename(backup_path, &plan.final_path).await;
        }
        return Err(err);
    }
    if let Some(backup_path) = backup {
        remove_any(&backup_path).await?;
    }
    if fs::try_exists(&plan.stage_base).await? {
        let _ = fs::remove_dir_all(&plan.stage_base).await;
    }
    Ok(())
}

async fn send_regular_file(
    stream: &mut TcpStream,
    path: &Path,
    size: u64,
    rel_path: &str,
    progress: &mut ProgressPrinter,
) -> Result<String> {
    let mut file = File::open(path)
        .await
        .with_context(|| format!("failed to open source file {}", path.display()))?;
    let mut buf = vec![0u8; COPY_BUFFER];
    let mut remaining = size;
    let mut hasher = blake3::Hasher::new();
    while remaining > 0 {
        let to_read = remaining.min(buf.len() as u64) as usize;
        let n = file
            .read(&mut buf[..to_read])
            .await
            .with_context(|| format!("failed reading {}", path.display()))?;
        if n == 0 {
            bail!("unexpected EOF while reading {}", path.display());
        }
        stream.write_all(&buf[..n]).await?;
        hasher.update(&buf[..n]);
        remaining -= n as u64;
        progress.advance_bytes(n as u64, &display_rel_path(rel_path));
    }
    Ok(hasher.finalize().to_hex().to_string())
}

async fn receive_regular_file(
    stream: &mut TcpStream,
    path: PathBuf,
    size: u64,
    mode: Option<u32>,
    rel_path: &str,
    progress: &mut ProgressPrinter,
) -> Result<String> {
    let mut file = File::create(&path)
        .await
        .with_context(|| format!("failed to create destination file {}", path.display()))?;
    let mut buf = vec![0u8; COPY_BUFFER];
    let mut remaining = size;
    let mut hasher = blake3::Hasher::new();
    while remaining > 0 {
        let to_read = remaining.min(buf.len() as u64) as usize;
        stream.read_exact(&mut buf[..to_read]).await?;
        file.write_all(&buf[..to_read]).await?;
        hasher.update(&buf[..to_read]);
        remaining -= to_read as u64;
        progress.advance_bytes(to_read as u64, &display_rel_path(rel_path));
    }
    file.flush().await?;
    file.sync_all().await?;
    set_file_mode(&path, mode).await?;
    Ok(hasher.finalize().to_hex().to_string())
}

async fn send_stdin_payload(
    stream: &mut TcpStream,
    entry: &ManifestEntry,
    progress: &mut ProgressPrinter,
) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; COPY_BUFFER];
    let mut hasher = blake3::Hasher::new();
    let mut total = 0u64;
    loop {
        let n = stdin.read(&mut buf).await.context("failed to read stdin")?;
        if n == 0 {
            break;
        }
        send_frame(stream, &Frame::StreamChunk { len: n as u32 }).await?;
        stream.write_all(&buf[..n]).await?;
        hasher.update(&buf[..n]);
        total += n as u64;
        progress.advance_bytes(n as u64, &display_rel_path(&entry.rel_path));
    }
    send_frame(
        stream,
        &Frame::StreamEnd {
            size: total,
            blake3: hasher.finalize().to_hex().to_string(),
        },
    )
    .await?;
    Ok(())
}

async fn receive_stdin_payload(
    stream: &mut TcpStream,
    path: PathBuf,
    mode: Option<u32>,
    rel_path: &str,
    progress: &mut ProgressPrinter,
) -> Result<(u64, String)> {
    let mut file = File::create(&path)
        .await
        .with_context(|| format!("failed to create destination file {}", path.display()))?;
    let mut total = 0u64;
    let mut hasher = blake3::Hasher::new();
    loop {
        match expect_frame(stream).await? {
            Frame::StreamChunk { len } => {
                let mut buf = vec![0u8; len as usize];
                stream.read_exact(&mut buf).await?;
                file.write_all(&buf).await?;
                hasher.update(&buf);
                total += len as u64;
                progress.advance_bytes(len as u64, &display_rel_path(rel_path));
            }
            Frame::StreamEnd { size, blake3 } => {
                file.flush().await?;
                file.sync_all().await?;
                set_file_mode(&path, mode).await?;
                let local_hash = hasher.finalize().to_hex().to_string();
                if size != total {
                    bail!("stdin byte count mismatch");
                }
                if blake3 != local_hash {
                    bail!("stdin stream hash mismatch");
                }
                return Ok((total, local_hash));
            }
            other => bail!("unexpected frame in stdin payload: {other:?}"),
        }
    }
}

async fn plan_transfer(
    transfer_id: String,
    source: &Path,
    output: Option<&Path>,
    symlinks: SymlinkMode,
    devices: DeviceMode,
) -> Result<PlannedTransfer> {
    match parse_sender_source(source) {
        SenderSource::Stdin => {
            let output = output.context("--output is required when --source stdin")?;
            let root_name = single_component_string(output)?;
            Ok(PlannedTransfer {
                transfer_id,
                root_name,
                root_source: RootSourceKind::Stdin,
                entries: vec![PlannedEntry {
                    manifest: ManifestEntry {
                        rel_path: String::new(),
                        kind: EntryKind::RegularFile,
                        size: None,
                        symlink_target: None,
                        device: None,
                        mode: None,
                    },
                    source_path: None,
                }],
                total_bytes: None,
            })
        }
        SenderSource::Filesystem(path) => tokio::task::spawn_blocking(move || {
            scan_filesystem_transfer(transfer_id, path, symlinks, devices)
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
) -> Result<PlannedTransfer> {
    let root_name = file_name_string(&source)?;
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    scan_entry(
        &source,
        Path::new(""),
        symlinks,
        devices,
        &mut entries,
        &mut total_bytes,
        true,
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
    })
}

fn scan_entry(
    source: &Path,
    rel_path: &Path,
    symlinks: SymlinkMode,
    devices: DeviceMode,
    entries: &mut Vec<PlannedEntry>,
    total_bytes: &mut u64,
    is_root: bool,
) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("failed to stat {}", source.display()))?;
    let file_type = metadata.file_type();
    let rel_path_string = rel_path_to_string(rel_path)?;
    let mode = file_mode(&metadata);

    if file_type.is_dir() {
        entries.push(PlannedEntry {
            manifest: ManifestEntry {
                rel_path: rel_path_string,
                kind: EntryKind::Directory,
                size: None,
                symlink_target: None,
                device: None,
                mode,
            },
            source_path: Some(source.to_path_buf()),
        });
        let mut children = std::fs::read_dir(source)
            .with_context(|| format!("failed to read directory {}", source.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("failed to enumerate directory {}", source.display()))?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let child_name = child.file_name();
            let child_rel = if rel_path.as_os_str().is_empty() {
                PathBuf::from(child_name)
            } else {
                rel_path.join(child_name)
            };
            scan_entry(
                &child.path(),
                &child_rel,
                symlinks,
                devices,
                entries,
                total_bytes,
                false,
            )?;
        }
        return Ok(());
    }

    if file_type.is_file() {
        let size = metadata.len();
        *total_bytes = total_bytes.saturating_add(size);
        entries.push(PlannedEntry {
            manifest: ManifestEntry {
                rel_path: rel_path_string,
                kind: EntryKind::RegularFile,
                size: Some(size),
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
                rel_path: rel_path_string,
                kind: EntryKind::Symlink,
                size: None,
                symlink_target: Some(path_to_platform_string(&target)?),
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
            let rdev = metadata.rdev();
            let kind = if file_type.is_char_device() {
                EntryKind::CharDevice
            } else {
                EntryKind::BlockDevice
            };
            entries.push(PlannedEntry {
                manifest: ManifestEntry {
                    rel_path: rel_path_string,
                    kind,
                    size: None,
                    symlink_target: None,
                    device: Some(DeviceDescriptor {
                        major: nix::sys::stat::major(rdev),
                        minor: nix::sys::stat::minor(rdev),
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

fn validate_manifest(begin: &BeginFrame, entries: &[ManifestEntry]) -> Result<()> {
    if entries.is_empty() {
        bail!("empty manifest");
    }
    let root = entries.first().context("manifest missing root entry")?;
    if !root.rel_path.is_empty() {
        bail!("manifest root entry must use an empty relative path");
    }
    match begin.root_source {
        RootSourceKind::Stdin => {
            if root.kind != EntryKind::RegularFile || root.size.is_some() {
                bail!("stdin transfers must have a single file root with unknown size");
            }
        }
        RootSourceKind::Filesystem => {
            if root.kind == EntryKind::RegularFile && root.size.is_none() {
                bail!("regular file manifest entry is missing its size");
            }
        }
    }
    let mut seen = BTreeMap::<PathBuf, EntryKind>::new();
    for entry in entries {
        let path = rel_string_to_path(&entry.rel_path)?;
        validate_relative_path(&path)?;
        if seen.contains_key(&path) {
            bail!(
                "manifest contains a duplicate entry for {}",
                display_rel_path(&entry.rel_path)
            );
        }
        let mut parent = path.parent();
        while let Some(ancestor) = parent {
            if let Some(kind) = seen.get(ancestor) {
                if !kind.is_directory() {
                    bail!(
                        "manifest entry {} would descend through a non-directory ancestor",
                        display_rel_path(&entry.rel_path)
                    );
                }
            }
            parent = ancestor.parent();
        }
        seen.insert(path, entry.kind);
    }
    Ok(())
}

async fn resolve_final_name(
    dest_root: &Path,
    root_name: &str,
    collision: CollisionPolicy,
) -> Result<String> {
    let requested = decode_component_string(root_name)?;
    validate_local_component(&requested)?;
    let candidate = dest_root.join(&requested);
    if !fs::try_exists(&candidate).await? {
        return Ok(root_name.to_string());
    }
    match collision {
        CollisionPolicy::Fail => bail!(
            "destination already exists: {} (use --overwrite or --rename)",
            candidate.display()
        ),
        CollisionPolicy::Overwrite => Ok(root_name.to_string()),
        CollisionPolicy::Rename => {
            let renamed = pick_renamed_root(dest_root, &requested).await?;
            encode_component_string(&renamed)
        }
    }
}

async fn pick_renamed_root(dest_root: &Path, root_name: &OsStr) -> Result<OsString> {
    let root_path = Path::new(root_name);
    let stem = root_path.file_stem();
    let ext = root_path.extension();
    for idx in 1..10_000u32 {
        let renamed = renamed_component(root_name, stem, ext, idx);
        if !fs::try_exists(dest_root.join(&renamed)).await? {
            return Ok(renamed);
        }
    }
    bail!(
        "unable to find a free renamed destination for {}",
        root_path.display()
    )
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

fn parse_sender_source(source: &Path) -> SenderSource {
    if is_stdin_keyword(source) {
        SenderSource::Stdin
    } else {
        SenderSource::Filesystem(source.to_path_buf())
    }
}

fn generate_transfer_id() -> String {
    Uuid::new_v4().to_string()
}

fn stage_path(plan: &ReceiverPlan, entry: &ManifestEntry) -> PathBuf {
    if entry.rel_path.is_empty() {
        plan.stage_root.clone()
    } else {
        plan.stage_root
            .join(rel_string_to_path(&entry.rel_path).expect("validated relative path"))
    }
}

async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create parent directory {}", parent.display()))?;
    }
    Ok(())
}

async fn remove_any(path: &Path) -> Result<()> {
    match fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => {
            fs::remove_dir_all(path)
                .await
                .with_context(|| format!("failed to remove directory {}", path.display()))?;
        }
        Ok(_) => {
            fs::remove_file(path)
                .await
                .with_context(|| format!("failed to remove file {}", path.display()))?;
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", path.display()))
        }
    }
    Ok(())
}

async fn send_frame<S: AsyncWrite + Unpin>(stream: &mut S, frame: &Frame) -> Result<()> {
    let payload = serde_json::to_vec(frame).context("failed to serialize transfer frame")?;
    if payload.len() > FRAME_LIMIT {
        bail!("transfer frame exceeds configured limit");
    }
    stream.write_u32_le(payload.len() as u32).await?;
    stream.write_all(&payload).await?;
    Ok(())
}

async fn expect_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Frame> {
    match recv_frame(stream).await? {
        Some(Frame::Error { message }) => bail!("peer reported an error: {message}"),
        Some(frame) => Ok(frame),
        None => bail!("unexpected EOF on transfer stream"),
    }
}

async fn expect_manifest_accepted<S: AsyncRead + Unpin>(stream: &mut S) -> Result<String> {
    match expect_frame(stream).await? {
        Frame::ManifestAccepted { final_name } => Ok(final_name),
        other => bail!("unexpected manifest response: {other:?}"),
    }
}

async fn recv_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Option<Frame>> {
    let len = match stream.read_u32_le().await {
        Ok(len) => len as usize,
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err).context("failed to read transfer frame header"),
    };
    if len > FRAME_LIMIT {
        bail!("peer sent an oversized transfer frame ({len} bytes)");
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    let frame = serde_json::from_slice(&buf).context("failed to decode transfer frame")?;
    Ok(Some(frame))
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
    let payload = serde_json::to_vec(&record).context("failed to encode transfer hash record")?;
    hasher.update(&(payload.len() as u32).to_le_bytes());
    hasher.update(&payload);
    Ok(())
}

fn validate_root_name(root_name: &str) -> Result<()> {
    let component = decode_component_string(root_name)?;
    validate_local_component(&component)
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

fn rel_path_to_string(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(encode_component_string(part)?),
            _ => bail!("invalid relative path {}", path.display()),
        }
    }
    Ok(parts.join("/"))
}

fn rel_string_to_path(value: &str) -> Result<PathBuf> {
    if value.is_empty() {
        return Ok(PathBuf::new());
    }
    let mut path = PathBuf::new();
    for part in value.split('/') {
        if part.is_empty() {
            bail!("invalid relative path {value}");
        }
        let component = decode_component_string(part)?;
        validate_local_component(&component)?;
        path.push(component);
    }
    Ok(path)
}

fn file_name_string(path: &Path) -> Result<String> {
    let file_name = path
        .file_name()
        .with_context(|| format!("{} does not end with a file name", path.display()))?;
    encode_component_string(file_name)
}

fn path_to_platform_string(path: &Path) -> Result<String> {
    encode_native_path_string(path)
}

fn display_rel_path(rel_path: &str) -> String {
    if rel_path.is_empty() {
        return ".".to_string();
    }
    let mut parts = Vec::new();
    for part in rel_path.split('/') {
        match decode_component_string(part) {
            Ok(component) => parts.push(PathBuf::from(component).display().to_string()),
            Err(_) => parts.push(part.to_string()),
        }
    }
    parts.join("/")
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

async fn create_symlink(target: &str, link: &Path) -> Result<()> {
    let target = decode_native_path_string(target)?;
    let link = link.to_path_buf();
    tokio::task::spawn_blocking(move || {
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
        tokio::task::spawn_blocking(move || {
            use nix::sys::stat::{makedev, mknod, Mode, SFlag};
            let device = entry.device.context("device entry missing metadata")?;
            let flag = match entry.kind {
                EntryKind::CharDevice => SFlag::S_IFCHR,
                EntryKind::BlockDevice => SFlag::S_IFBLK,
                _ => bail!("invalid device entry kind"),
            };
            let mode = Mode::from_bits_truncate(entry.mode.unwrap_or(0o600));
            mknod(&path, flag, mode, makedev(device.major, device.minor))
                .with_context(|| format!("failed to create device {}", path.display()))?;
            Ok(())
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

struct ProgressPrinter {
    label: &'static str,
    enabled: bool,
    total_bytes: Option<u64>,
    total_files: u64,
    bytes_done: u64,
    files_done: u64,
    started: Instant,
    last_print: Instant,
}

impl ProgressPrinter {
    fn new(label: &'static str, total_bytes: Option<u64>, total_files: u64) -> Self {
        let now = Instant::now();
        Self {
            label,
            enabled: std::io::stderr().is_terminal(),
            total_bytes,
            total_files,
            bytes_done: 0,
            files_done: 0,
            started: now,
            last_print: now,
        }
    }

    fn start_path(&mut self, current: &str) {
        self.render(current, false);
    }

    fn advance_bytes(&mut self, delta: u64, current: &str) {
        self.bytes_done = self.bytes_done.saturating_add(delta);
        if self.last_print.elapsed() >= Duration::from_millis(250) {
            self.render(current, false);
        }
    }

    fn finish_file(&mut self, current: &str) {
        self.files_done = self.files_done.saturating_add(1);
        self.render(current, false);
    }

    fn finish(&mut self) {
        self.render("done", true);
    }

    fn render(&mut self, current: &str, done: bool) {
        if !self.enabled {
            return;
        }
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let speed = self.bytes_done as f64 / elapsed;
        let bytes = human_bytes(self.bytes_done);
        let speed = human_bytes(speed as u64);
        let files = if self.total_files > 0 {
            format!(" files {}/{}", self.files_done, self.total_files)
        } else {
            String::new()
        };
        let ratio = match self.total_bytes {
            Some(total) if total > 0 => format!(
                " {}/{} {:>5.1}%",
                bytes,
                human_bytes(total),
                (self.bytes_done as f64 / total as f64) * 100.0
            ),
            Some(total) => format!(" {}/{}", bytes, human_bytes(total)),
            None => format!(" {}", bytes),
        };
        let suffix = if done { "\n" } else { "\r" };
        eprint!(
            "[{}]{}{} speed {}/s current {}{}",
            self.label, ratio, files, speed, current, suffix
        );
        let _ = std::io::stderr().flush();
        self.last_print = Instant::now();
    }
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

fn is_stdin_keyword(path: &Path) -> bool {
    path.components().count() == 1 && path.as_os_str() == OsStr::new("stdin")
}

fn single_component_string(path: &Path) -> Result<String> {
    let components: Vec<_> = path.components().collect();
    if components.len() != 1 {
        bail!("transfer root name must be a single path component");
    }
    match components[0] {
        Component::Normal(part) => encode_component_string(part),
        _ => bail!("transfer root name must be a single path component"),
    }
}

fn encode_component_string(component: &OsStr) -> Result<String> {
    validate_local_component(component)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        if let Ok(text) = std::str::from_utf8(component.as_bytes()) {
            return Ok(format!("u:{}", hex::encode(text.as_bytes())));
        }
        return Ok(format!("b:{}", hex::encode(component.as_bytes())));
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        let wide: Vec<u16> = component.encode_wide().collect();
        if let Ok(text) = String::from_utf16(&wide) {
            return Ok(format!("u:{}", hex::encode(text.as_bytes())));
        }
        let mut bytes = Vec::with_capacity(wide.len() * 2);
        for unit in wide {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        Ok(format!("w:{}", hex::encode(bytes)))
    }

    #[cfg(not(any(unix, windows)))]
    {
        let text = component.to_string_lossy();
        Ok(format!("u:{}", hex::encode(text.as_bytes())))
    }
}

fn decode_component_string(value: &str) -> Result<OsString> {
    let (kind, encoded) = value
        .split_once(':')
        .with_context(|| format!("invalid encoded path component {value:?}"))?;
    match kind {
        "u" => {
            let bytes = hex::decode(encoded)
                .with_context(|| format!("invalid UTF-8 component encoding {value:?}"))?;
            let text = String::from_utf8(bytes)
                .with_context(|| format!("invalid UTF-8 component payload {value:?}"))?;
            let component = OsString::from(text);
            validate_local_component(&component)?;
            Ok(component)
        }
        "b" => decode_unix_component(encoded),
        "w" => decode_windows_component(encoded),
        _ => bail!("unknown encoded path component kind {kind:?}"),
    }
}

fn encode_native_path_string(path: &Path) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        if let Ok(text) = std::str::from_utf8(path.as_os_str().as_bytes()) {
            return Ok(format!("u:{}", hex::encode(text.as_bytes())));
        }
        return Ok(format!("b:{}", hex::encode(path.as_os_str().as_bytes())));
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if let Ok(text) = String::from_utf16(&wide) {
            return Ok(format!("u:{}", hex::encode(text.as_bytes())));
        }
        let mut bytes = Vec::with_capacity(wide.len() * 2);
        for unit in wide {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        Ok(format!("w:{}", hex::encode(bytes)))
    }

    #[cfg(not(any(unix, windows)))]
    {
        let text = path.to_string_lossy();
        Ok(format!("u:{}", hex::encode(text.as_bytes())))
    }
}

fn decode_native_path_string(value: &str) -> Result<PathBuf> {
    let (kind, encoded) = value
        .split_once(':')
        .with_context(|| format!("invalid encoded native path {value:?}"))?;
    match kind {
        "u" => {
            let bytes = hex::decode(encoded)
                .with_context(|| format!("invalid UTF-8 path encoding {value:?}"))?;
            let text = String::from_utf8(bytes)
                .with_context(|| format!("invalid UTF-8 path payload {value:?}"))?;
            Ok(PathBuf::from(text))
        }
        "b" => {
            let bytes = hex::decode(encoded)
                .with_context(|| format!("invalid unix path encoding {value:?}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                Ok(PathBuf::from(OsString::from_vec(bytes)))
            }
            #[cfg(not(unix))]
            {
                let text = String::from_utf8(bytes).context(
                    "receiver platform cannot represent a non-UTF-8 unix path losslessly",
                )?;
                Ok(PathBuf::from(text))
            }
        }
        "w" => {
            let bytes = hex::decode(encoded)
                .with_context(|| format!("invalid windows path encoding {value:?}"))?;
            if bytes.len() % 2 != 0 {
                bail!("invalid windows path payload length");
            }
            let wide: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            #[cfg(windows)]
            {
                use std::os::windows::ffi::OsStringExt;
                Ok(PathBuf::from(OsString::from_wide(&wide)))
            }
            #[cfg(not(windows))]
            {
                let text = String::from_utf16(&wide).context(
                    "receiver platform cannot represent this Windows path losslessly",
                )?;
                Ok(PathBuf::from(text))
            }
        }
        _ => bail!("unknown encoded native path kind {kind:?}"),
    }
}

fn decode_unix_component(encoded: &str) -> Result<OsString> {
    let bytes = hex::decode(encoded).with_context(|| {
        format!("invalid unix component encoding b:{encoded}")
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        let component = OsString::from_vec(bytes);
        validate_local_component(&component)?;
        Ok(component)
    }
    #[cfg(not(unix))]
    {
        let text = String::from_utf8(bytes)
            .context("receiver platform cannot represent a non-UTF-8 unix component losslessly")?;
        let component = OsString::from(text);
        validate_local_component(&component)?;
        Ok(component)
    }
}

fn decode_windows_component(encoded: &str) -> Result<OsString> {
    let bytes = hex::decode(encoded).with_context(|| {
        format!("invalid windows component encoding w:{encoded}")
    })?;
    if bytes.len() % 2 != 0 {
        bail!("invalid windows component payload length");
    }
    let wide: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;

        let component = OsString::from_wide(&wide);
        validate_local_component(&component)?;
        Ok(component)
    }
    #[cfg(not(windows))]
    {
        let text = String::from_utf16(&wide)
            .context("receiver platform cannot represent this Windows component losslessly")?;
        let component = OsString::from(text);
        validate_local_component(&component)?;
        Ok(component)
    }
}

fn validate_local_component(component: &OsStr) -> Result<()> {
    if component.is_empty() {
        bail!("transfer root name cannot be empty");
    }
    let mut components = Path::new(component).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => bail!("transfer root name must be a single path component"),
    }
}

fn renamed_component(
    root_name: &OsStr,
    stem: Option<&OsStr>,
    ext: Option<&OsStr>,
    idx: u32,
) -> OsString {
    let suffix = format!(" ({idx})");
    if let (Some(root_text), Some(stem_text)) = (root_name.to_str(), stem.and_then(|part| part.to_str())) {
        if let Some(ext_text) = ext.and_then(|part| part.to_str()) {
            let root_path = Path::new(root_text);
            if root_path.file_name() == Some(root_name) {
                return OsString::from(format!("{stem_text}{suffix}.{ext_text}"));
            }
        }
        return OsString::from(format!("{root_text}{suffix}"));
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let mut bytes = root_name.as_bytes().to_vec();
        bytes.extend_from_slice(suffix.as_bytes());
        OsString::from_vec(bytes)
    }
    #[cfg(not(unix))]
    {
        let mut renamed = root_name.to_os_string();
        renamed.push(suffix);
        renamed
    }
}
/*
//! Secure file transfer built on top of bore secret tunnels.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::{info, warn};
use uuid::Uuid;

use crate::client::{Client, ProviderMeta};
use crate::secret::Proxy;
use crate::server::DEFAULT_MAX_CONNS;
use crate::transport::Endpoint;

const INTERNAL_BIND_ADDR: &str = "127.0.0.1:0";
const INTERNAL_HOST: &str = "127.0.0.1";
const FRAME_LIMIT: usize = 16 * 1024 * 1024;
const MANIFEST_CHUNK_SIZE: usize = 128;
const COPY_BUFFER_SIZE: usize = 64 * 1024;
const PROGRESS_TICK: Duration = Duration::from_millis(500);
const TRANSFER_PROTOCOL_VERSION: u32 = 1;

/// Transfer conflict handling for an already existing destination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollisionPolicy {
    /// Abort the transfer if the destination root already exists.
    Fail,
    /// Replace the destination root with the staged transfer.
    Overwrite,
    /// Pick a new destination root name when the requested one already exists.
    Rename,
}

/// Whether symlinks should be transferred or skipped.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SymlinkMode {
    /// Preserve symlinks as symlinks.
    Include,
    /// Skip symlinks during the source scan.
    Exclude,
}

/// Whether Unix device nodes should be transferred or skipped.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum DeviceMode {
    /// Preserve supported Unix device nodes.
    Include,
    /// Skip device nodes during the source scan.
    Exclude,
}

/// Transport settings for `bore transfer listener`.
#[derive(Clone, Debug)]
pub struct ListenerOptions {
    /// Bore server endpoint.
    pub to: String,
    /// Optional shared secret.
    pub secret: Option<String>,
    /// Skip TLS certificate verification for `https://` endpoints.
    pub insecure: bool,
    /// Transfer identifier. If omitted, one is generated locally and printed.
    pub transfer_id: Option<String>,
    /// Destination directory on the receiver host.
    pub dest_path: PathBuf,
    /// Disable the direct UDP attempt and stay on relay only.
    pub relay_only: bool,
    /// Optional STUN override.
    pub stun_server: Option<String>,
    /// Enable UPnP-assisted direct UDP discovery.
    pub upnp: bool,
    /// Enable symmetric-NAT port prediction.
    pub try_port_prediction: bool,
    /// Preferred local UDP port for direct mode.
    pub nat_udp_preferred_port: u16,
    /// Preferred-port re-check timeout in seconds.
    pub nat_udp_release_timeout: u64,
    /// Relay carrier pool size.
    pub carriers: u16,
    /// Collision policy for the destination root.
    pub collision_policy: CollisionPolicy,
}

/// Transport and source settings for `bore transfer sender`.
#[derive(Clone, Debug)]
pub struct SenderOptions {
    /// Bore server endpoint.
    pub to: String,
    /// Optional shared secret.
    pub secret: Option<String>,
    /// Skip TLS certificate verification for `https://` endpoints.
    pub insecure: bool,
    /// Transfer identifier. If omitted, one is generated locally and printed.
    pub transfer_id: Option<String>,
    /// Source path or the literal string `stdin`.
    pub source: String,
    /// Output file name for `stdin` transfers.
    pub output: Option<String>,
    /// Disable the direct UDP attempt and stay on relay only.
    pub relay_only: bool,
    /// Optional STUN override.
    pub stun_server: Option<String>,
    /// Enable UPnP-assisted direct UDP discovery.
    pub upnp: bool,
    /// Enable symmetric-NAT port prediction.
    pub try_port_prediction: bool,
    /// Preferred local UDP port for direct mode.
    pub nat_udp_preferred_port: u16,
    /// Preferred-port re-check timeout in seconds.
    pub nat_udp_release_timeout: u64,
    /// Relay carrier pool size.
    pub carriers: u16,
    /// How to handle symlinks while scanning the source.
    pub symlink_mode: SymlinkMode,
    /// How to handle Unix device nodes while scanning the source.
    pub device_mode: DeviceMode,
}

/// Result of one completed transfer command.
#[derive(Clone, Debug)]
pub struct TransferOutcome {
    /// Transfer identifier used for the session.
    pub transfer_id: String,
    /// Final destination path on the receiver.
    pub final_path: PathBuf,
    /// Number of transferred regular files.
    pub files: u64,
    /// Number of transferred payload bytes.
    pub bytes: u64,
    /// Whether the transfer used the direct UDP path.
    pub direct: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BeginFrame {
    protocol_version: u32,
    transfer_id: String,
    root_name: String,
    stdin_source: bool,
    total_entries: u64,
    total_bytes: Option<u64>,
    path: TransferPathInfo,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TransferPathInfo {
    direct: bool,
    relay_tls: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum EntryKind {
    File,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
}

impl EntryKind {
    fn is_regular_file(self) -> bool {
        matches!(self, Self::File)
    }

    fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }

    fn is_static(self) -> bool {
        matches!(
            self,
            Self::Directory | Self::Symlink | Self::CharDevice | Self::BlockDevice
        )
    }
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::CharDevice => "char-device",
            Self::BlockDevice => "block-device",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceDescriptor {
    major: u64,
    minor: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ManifestEntry {
    rel_path: String,
    kind: EntryKind,
    size: Option<u64>,
    symlink_target: Option<String>,
    device: Option<DeviceDescriptor>,
    mode: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SummaryFrame {
    files: u64,
    dirs: u64,
    symlinks: u64,
    devices: u64,
    bytes: u64,
    blake3: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CompletedFrame {
    final_path: String,
    files: u64,
    bytes: u64,
    blake3: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    Begin(BeginFrame),
    ManifestChunk { entries: Vec<ManifestEntry> },
    ManifestDone,
    ManifestAccepted { final_name: String },
    FileStart { rel_path: String, size: u64 },
    FileEnd { rel_path: String, blake3: String },
    StreamStart { rel_path: String },
    StreamChunk { len: u32 },
    StreamEnd {
        rel_path: String,
        size: u64,
        blake3: String,
    },
    Summary(SummaryFrame),
    Completed(CompletedFrame),
    Error { message: String },
}

#[derive(Clone, Debug)]
struct PlannedTransfer {
    root_name: String,
    stdin_source: bool,
    entries: Vec<PlannedEntry>,
    total_bytes: Option<u64>,
}

#[derive(Clone, Debug)]
struct PlannedEntry {
    manifest: ManifestEntry,
    source_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct ReceiverPlan {
    final_name: String,
    final_path: PathBuf,
    stage_base: PathBuf,
    stage_root: PathBuf,
    entries: Vec<ManifestEntry>,
}

#[derive(Clone, Debug, Default)]
struct TransferStats {
    files: u64,
    dirs: u64,
    symlinks: u64,
    devices: u64,
    bytes: u64,
}

#[derive(Serialize)]
struct TransferRecord<'a> {
    rel_path: &'a str,
    kind: EntryKind,
    size: Option<u64>,
    blake3: Option<&'a str>,
    symlink_target: Option<&'a str>,
    device: Option<&'a DeviceDescriptor>,
}

struct ProgressTracker {
    enabled: bool,
    bytes_done: Arc<AtomicU64>,
    files_done: Arc<AtomicU64>,
    current_item: Arc<Mutex<String>>,
    finished: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

impl ProgressTracker {
    fn new(label: &'static str, total_bytes: Option<u64>, total_files: u64) -> Self {
        if !std::io::stderr().is_terminal() {
            return Self {
                enabled: false,
                bytes_done: Arc::new(AtomicU64::new(0)),
                files_done: Arc::new(AtomicU64::new(0)),
                current_item: Arc::new(Mutex::new(String::new())),
                finished: Arc::new(AtomicBool::new(false)),
                task: None,
            };
        }

        let bytes_done = Arc::new(AtomicU64::new(0));
        let files_done = Arc::new(AtomicU64::new(0));
        let current_item = Arc::new(Mutex::new(String::new()));
        let finished = Arc::new(AtomicBool::new(false));
        let task_bytes = Arc::clone(&bytes_done);
        let task_files = Arc::clone(&files_done);
        let task_item = Arc::clone(&current_item);
        let task_finished = Arc::clone(&finished);

        let task = tokio::spawn(async move {
            let started = Instant::now();
            let mut interval = tokio::time::interval(PROGRESS_TICK);
            loop {
                interval.tick().await;
                let done = task_bytes.load(Ordering::Relaxed);
                let files = task_files.load(Ordering::Relaxed);
                let elapsed = started.elapsed().as_secs_f64().max(0.001);
                let speed = done as f64 / elapsed;
                let item = task_item.lock().expect("progress mutex").clone();
                let line = if let Some(total) = total_bytes {
                    let pct = if total == 0 {
                        100.0
                    } else {
                        (done as f64 / total as f64) * 100.0
                    };
                    format!(
                        "\r{label}: {files}/{total_files} files, {} / {} ({pct:.1}%), {}/s {}",
                        format_bytes(done),
                        format_bytes(total),
                        format_bytes(speed as u64),
                        truncate_item(&item),
                    )
                } else {
                    format!(
                        "\r{label}: {files}/{total_files} files, {}, {}/s {}",
                        format_bytes(done),
                        format_bytes(speed as u64),
                        truncate_item(&item),
                    )
                };
                eprint!("{line}");
                let _ = std::io::stderr().flush();
                if task_finished.load(Ordering::Relaxed) {
                    eprintln!();
                    break;
                }
            }
        });

        Self {
            enabled: true,
            bytes_done,
            files_done,
            current_item,
            finished,
            task: Some(task),
        }
    }

    fn set_current(&self, path: &str) {
        if !self.enabled {
            return;
        }
        *self.current_item.lock().expect("progress mutex") = path.to_string();
    }

    fn add_bytes(&self, bytes: u64) {
        self.bytes_done.fetch_add(bytes, Ordering::Relaxed);
    }

    fn file_completed(&self) {
        self.files_done.fetch_add(1, Ordering::Relaxed);
    }

    async fn finish(mut self) {
        if !self.enabled {
            return;
        }
        self.finished.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// Run `bore transfer listener` once.
pub async fn run_listener(options: ListenerOptions) -> Result<TransferOutcome> {
    let transfer_id = options.transfer_id.unwrap_or_else(generate_transfer_id);
    if options.transfer_id.is_none() {
        println!("Generated transfer id: {transfer_id}");
    }
    fs::create_dir_all(&options.dest_path)
        .await
        .with_context(|| format!("failed to create destination root {}", options.dest_path.display()))?;

    let internal = TcpListener::bind(INTERNAL_BIND_ADDR)
        .await
        .context("failed to start internal transfer listener")?;
    let internal_port = internal.local_addr()?.port();

    let provider = Client::new_secret_provider(
        INTERNAL_HOST,
        internal_port,
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
    .await
    .with_context(|| format!("failed to register transfer listener '{transfer_id}'"))?;

    info!(
        transfer_id,
        udp = !options.relay_only,
        relay_tls = Endpoint::parse(&options.to).tls,
        carriers = options.carriers,
        dest = %options.dest_path.display(),
        "transfer listener ready"
    );
    println!("Waiting for sender on transfer id: {transfer_id}");

    let mut provider_task = tokio::spawn(provider.listen());
    let (stream, _) = tokio::select! {
        accepted = internal.accept() => accepted.context("internal transfer listener stopped before a sender connected")?,
        result = &mut provider_task => {
            match result {
                Ok(Ok(())) => bail!("transfer listener transport ended unexpectedly"),
                Ok(Err(err)) => return Err(err).context("transfer listener transport failed"),
                Err(err) => bail!("transfer listener transport task failed: {err}"),
            }
        }
    };

    let received = receive_transfer(stream, &transfer_id, &options.dest_path, options.collision_policy).await;
    provider_task.abort();
    let _ = provider_task.await;
    received
}

/// Run `bore transfer sender` once.
pub async fn run_sender(options: SenderOptions) -> Result<TransferOutcome> {
    let transfer_id = options.transfer_id.unwrap_or_else(generate_transfer_id);
    if options.transfer_id.is_none() {
        println!("Generated transfer id: {transfer_id}");
    }

    let plan = build_plan(
        &options.source,
        options.output.as_deref(),
        options.symlink_mode,
        options.device_mode,
    )
    .await?;
    let endpoint = Endpoint::parse(&options.to);
    let proxy = Proxy::new(
        &options.to,
        INTERNAL_BIND_ADDR.parse().expect("valid internal bind addr"),
        &transfer_id,
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
    .await
    .with_context(|| format!("failed to connect transfer sender '{transfer_id}'"))?;

    let direct = proxy.is_direct();
    let local_addr = proxy.local_addr()?;
    let path_info = TransferPathInfo {
        direct,
        relay_tls: !direct && endpoint.tls,
    };

    if direct {
        info!(transfer_id, transport = "direct-udp", security = "quic-encrypted", "transfer sender path selected");
    } else if options.relay_only {
        info!(transfer_id, transport = "relay", security = if endpoint.tls { "tls" } else { "plain" }, "transfer sender path selected");
    } else {
        info!(transfer_id, transport = "relay", security = if endpoint.tls { "tls" } else { "plain" }, "direct udp unavailable, falling back to relay");
    }

    let mut proxy_task = tokio::spawn(proxy.listen());
    let stream = tokio::select! {
        connected = TcpStream::connect(local_addr) => connected.context("failed to connect internal transfer sender socket")?,
        result = &mut proxy_task => {
            match result {
                Ok(Ok(())) => bail!("transfer sender transport ended before the local session started"),
                Ok(Err(err)) => return Err(err).context("transfer sender transport failed"),
                Err(err) => bail!("transfer sender transport task failed: {err}"),
            }
        }
    };

    let sent = send_transfer(stream, &transfer_id, plan, path_info).await;
    proxy_task.abort();
    let _ = proxy_task.await;
    sent
}

async fn send_transfer(
    mut stream: TcpStream,
    transfer_id: &str,
    plan: PlannedTransfer,
    path: TransferPathInfo,
) -> Result<TransferOutcome> {
    let total_files = plan
        .entries
        .iter()
        .filter(|entry| entry.manifest.kind.is_regular_file())
        .count() as u64;
    let progress = ProgressTracker::new("send", plan.total_bytes, total_files.max(1));

    send_frame(
        &mut stream,
        &Frame::Begin(BeginFrame {
            protocol_version: TRANSFER_PROTOCOL_VERSION,
            transfer_id: transfer_id.to_string(),
            root_name: plan.root_name.clone(),
            stdin_source: plan.stdin_source,
            total_entries: plan.entries.len() as u64,
            total_bytes: plan.total_bytes,
            path,
        }),
    )
    .await?;

    for chunk in plan.entries.chunks(MANIFEST_CHUNK_SIZE) {
        let entries = chunk.iter().map(|entry| entry.manifest.clone()).collect();
        send_frame(&mut stream, &Frame::ManifestChunk { entries }).await?;
    }
    send_frame(&mut stream, &Frame::ManifestDone).await?;

    let final_name = match recv_frame(&mut stream).await? {
        Some(Frame::ManifestAccepted { final_name }) => final_name,
        Some(Frame::Error { message }) => bail!("listener rejected transfer: {message}"),
        Some(other) => bail!("unexpected frame after manifest: {:?}", other),
        None => bail!("listener closed during manifest negotiation"),
    };
    info!(transfer_id, final_name, "transfer manifest accepted");

    let mut aggregate = blake3::Hasher::new();
    let mut stats = TransferStats::default();

    for entry in &plan.entries {
        let rel_path = &entry.manifest.rel_path;
        if entry.manifest.kind.is_static() {
            update_transfer_hash(&mut aggregate, &entry.manifest, None, entry.manifest.size)?;
            bump_static_stats(&mut stats, entry.manifest.kind);
            continue;
        }

        progress.set_current(display_rel_path(rel_path));
        if let Some(size) = entry.manifest.size {
            let source_path = entry
                .source_path
                .as_ref()
                .context("missing source path for regular file")?;
            send_frame(
                &mut stream,
                &Frame::FileStart {
                    rel_path: rel_path.clone(),
                    size,
                },
            )
            .await?;
            let digest = send_file_bytes(&mut stream, source_path, size, &progress).await?;
            send_frame(
                &mut stream,
                &Frame::FileEnd {
                    rel_path: rel_path.clone(),
                    blake3: digest.clone(),
                },
            )
            .await?;
            update_transfer_hash(&mut aggregate, &entry.manifest, Some(digest.as_str()), Some(size))?;
            stats.files += 1;
            stats.bytes += size;
            progress.file_completed();
        } else {
            send_frame(
                &mut stream,
                &Frame::StreamStart {
                    rel_path: rel_path.clone(),
                },
            )
            .await?;
            let (size, digest) = send_stdin_bytes(&mut stream, &progress).await?;
            send_frame(
                &mut stream,
                &Frame::StreamEnd {
                    rel_path: rel_path.clone(),
                    size,
                    blake3: digest.clone(),
                },
            )
            .await?;
            update_transfer_hash(&mut aggregate, &entry.manifest, Some(digest.as_str()), Some(size))?;
            stats.files += 1;
            stats.bytes += size;
            progress.file_completed();
        }
    }

    progress.finish().await;
    let summary = SummaryFrame {
        files: stats.files,
        dirs: stats.dirs,
        symlinks: stats.symlinks,
        devices: stats.devices,
        bytes: stats.bytes,
        blake3: aggregate.finalize().to_hex().to_string(),
    };
    send_frame(&mut stream, &Frame::Summary(summary.clone())).await?;

    match recv_frame(&mut stream).await? {
        Some(Frame::Completed(done)) => {
            if done.blake3 != summary.blake3 {
                bail!(
                    "listener acknowledged transfer with mismatched digest: expected {}, got {}",
                    summary.blake3,
                    done.blake3
                );
            }
            info!(transfer_id, final_path = %done.final_path, bytes = done.bytes, files = done.files, "transfer completed");
            Ok(TransferOutcome {
                transfer_id: transfer_id.to_string(),
                final_path: PathBuf::from(done.final_path),
                files: done.files,
                bytes: done.bytes,
                direct: path.direct,
            })
        }
        Some(Frame::Error { message }) => bail!("transfer failed: {message}"),
        Some(other) => bail!("unexpected completion frame: {:?}", other),
        None => bail!("listener closed before confirming transfer completion"),
    }
}

async fn receive_transfer(
    mut stream: TcpStream,
    transfer_id: &str,
    dest_path: &Path,
    collision_policy: CollisionPolicy,
) -> Result<TransferOutcome> {
    let begin = match recv_frame(&mut stream).await? {
        Some(Frame::Begin(begin)) => begin,
        Some(Frame::Error { message }) => bail!("sender aborted before begin: {message}"),
        Some(other) => bail!("unexpected frame at transfer start: {:?}", other),
        None => bail!("sender closed before sending a transfer header"),
    };
    if begin.protocol_version != TRANSFER_PROTOCOL_VERSION {
        send_protocol_error(
            &mut stream,
            format!(
                "unsupported transfer protocol version {}",
                begin.protocol_version
            ),
        )
        .await;
        bail!("unsupported transfer protocol version {}", begin.protocol_version);
    }
    if begin.transfer_id != transfer_id {
        send_protocol_error(
            &mut stream,
            format!(
                "transfer id mismatch: expected {transfer_id}, got {}",
                begin.transfer_id
            ),
        )
        .await;
        bail!(
            "transfer id mismatch: expected {transfer_id}, got {}",
            begin.transfer_id
        );
    }

    info!(
        transfer_id,
        transport = if begin.path.direct { "direct-udp" } else { "relay" },
        security = transfer_security(&begin.path),
        stdin_source = begin.stdin_source,
        "transfer receiver accepted session"
    );

    let receiver_plan = match receive_manifest(&mut stream, dest_path, &begin, collision_policy).await {
        Ok(plan) => plan,
        Err(err) => {
            send_protocol_error(&mut stream, err.to_string()).await;
            return Err(err);
        }
    };

    send_frame(
        &mut stream,
        &Frame::ManifestAccepted {
            final_name: receiver_plan.final_name.clone(),
        },
    )
    .await?;

    let total_files = receiver_plan
        .entries
        .iter()
        .filter(|entry| entry.kind.is_regular_file())
        .count() as u64;
    let progress = ProgressTracker::new("recv", begin.total_bytes, total_files.max(1));

    let mut aggregate = blake3::Hasher::new();
    let mut stats = TransferStats::default();
    let transfer_result = receive_payload(
        &mut stream,
        &receiver_plan,
        &progress,
        &mut aggregate,
        &mut stats,
    )
    .await;
    progress.finish().await;

    let summary = match transfer_result {
        Ok(summary) => summary,
        Err(err) => {
            let _ = cleanup_stage(&receiver_plan.stage_base).await;
            send_protocol_error(&mut stream, err.to_string()).await;
            return Err(err);
        }
    };

    let expected = SummaryFrame {
        files: stats.files,
        dirs: stats.dirs,
        symlinks: stats.symlinks,
        devices: stats.devices,
        bytes: stats.bytes,
        blake3: aggregate.finalize().to_hex().to_string(),
    };
    if summary != expected {
        let _ = cleanup_stage(&receiver_plan.stage_base).await;
        let message = format!(
            "summary mismatch: expected files={}, dirs={}, symlinks={}, devices={}, bytes={}, blake3={}, got files={}, dirs={}, symlinks={}, devices={}, bytes={}, blake3={}",
            expected.files,
            expected.dirs,
            expected.symlinks,
            expected.devices,
            expected.bytes,
            expected.blake3,
            summary.files,
            summary.dirs,
            summary.symlinks,
            summary.devices,
            summary.bytes,
            summary.blake3,
        );
        send_protocol_error(&mut stream, message.clone()).await;
        bail!(message);
    }

    let final_path = match commit_stage(&receiver_plan, collision_policy).await {
        Ok(path) => path,
        Err(err) => {
            let _ = cleanup_stage(&receiver_plan.stage_base).await;
            send_protocol_error(&mut stream, err.to_string()).await;
            return Err(err);
        }
    };
    let final_path_string = final_path.to_string_lossy().to_string();
    send_frame(
        &mut stream,
        &Frame::Completed(CompletedFrame {
            final_path: final_path_string.clone(),
            files: expected.files,
            bytes: expected.bytes,
            blake3: expected.blake3.clone(),
        }),
    )
    .await?;

    Ok(TransferOutcome {
        transfer_id: transfer_id.to_string(),
        final_path: PathBuf::from(final_path_string),
        files: expected.files,
        bytes: expected.bytes,
        direct: begin.path.direct,
    })
}

async fn receive_manifest(
    stream: &mut TcpStream,
    dest_path: &Path,
    begin: &BeginFrame,
    collision_policy: CollisionPolicy,
) -> Result<ReceiverPlan> {
    let mut entries = Vec::new();
    loop {
        match recv_frame(stream).await? {
            Some(Frame::ManifestChunk { entries: chunk }) => entries.extend(chunk),
            Some(Frame::ManifestDone) => break,
            Some(Frame::Error { message }) => bail!("sender aborted during manifest: {message}"),
            Some(other) => bail!("unexpected frame while receiving manifest: {:?}", other),
            None => bail!("sender closed during manifest transfer"),
        }
    }

    if entries.len() as u64 != begin.total_entries {
        bail!(
            "manifest entry count mismatch: expected {}, got {}",
            begin.total_entries,
            entries.len()
        );
    }
    let root = entries.first().context("manifest is empty")?;
    if !root.rel_path.is_empty() {
        bail!("manifest root entry must use an empty relative path");
    }
    if begin.stdin_source && (root.kind != EntryKind::File || root.size.is_some()) {
        bail!("stdin transfers must expose a single root regular file with unknown size");
    }
    if !begin.stdin_source && root.kind == EntryKind::File && root.size.is_none() {
        bail!("regular file entries require a known size");
    }
    validate_manifest(&entries)?;

    let final_name = resolve_final_name(dest_path, &begin.root_name, collision_policy).await?;
    let stage_base = dest_path.join(format!(".bore-transfer-{}.part", Uuid::new_v4()));
    let stage_root = stage_base.join(&final_name);
    Ok(ReceiverPlan {
        final_name,
        final_path: dest_path.join(begin.root_name.clone()),
        stage_base,
        stage_root,
        entries,
    })
}

async fn receive_payload(
    stream: &mut TcpStream,
    plan: &ReceiverPlan,
    progress: &ProgressTracker,
    aggregate: &mut blake3::Hasher,
    stats: &mut TransferStats,
) -> Result<SummaryFrame> {
    fs::create_dir_all(&plan.stage_base)
        .await
        .with_context(|| format!("failed to create staging directory {}", plan.stage_base.display()))?;

    for entry in &plan.entries {
        if entry.kind.is_static() {
            apply_static_entry(&plan.stage_root, entry).await?;
            update_transfer_hash(aggregate, entry, None, entry.size)?;
            bump_static_stats(stats, entry.kind);
            continue;
        }

        let rel_path = entry.rel_path.clone();
        progress.set_current(display_rel_path(&rel_path));
        if let Some(size) = entry.size {
            let start = match recv_frame(stream).await? {
                Some(Frame::FileStart { rel_path, size }) => (rel_path, size),
                Some(Frame::Error { message }) => bail!("sender aborted during file payload: {message}"),
                Some(other) => bail!("unexpected frame before file payload: {:?}", other),
                None => bail!("sender closed during file payload"),
            };
            if start.0 != entry.rel_path || start.1 != size {
                bail!(
                    "payload header mismatch for '{}': expected size {}, got '{}' size {}",
                    entry.rel_path,
                    size,
                    start.0,
                    start.1
                );
            }
            let path = stage_entry_path(&plan.stage_root, entry)?;
            let digest = receive_file_bytes(stream, &path, size, entry.mode, progress).await?;
            match recv_frame(stream).await? {
                Some(Frame::FileEnd { rel_path, blake3 }) => {
                    if rel_path != entry.rel_path {
                        bail!(
                            "file trailer mismatch: expected '{}', got '{}'",
                            entry.rel_path,
                            rel_path
                        );
                    }
                    if blake3 != digest {
                        bail!(
                            "digest mismatch for '{}': sender={}, receiver={digest}",
                            entry.rel_path,
                            blake3
                        );
                    }
                    update_transfer_hash(aggregate, entry, Some(digest.as_str()), Some(size))?;
                    stats.files += 1;
                    stats.bytes += size;
                    progress.file_completed();
                }
                Some(Frame::Error { message }) => bail!("sender aborted during file trailer: {message}"),
                Some(other) => bail!("unexpected frame after file payload: {:?}", other),
                None => bail!("sender closed before file trailer"),
            }
        } else {
            match recv_frame(stream).await? {
                Some(Frame::StreamStart { rel_path }) => {
                    if rel_path != entry.rel_path {
                        bail!(
                            "stream header mismatch: expected '{}', got '{}'",
                            entry.rel_path,
                            rel_path
                        );
                    }
                }
                Some(Frame::Error { message }) => bail!("sender aborted during stream start: {message}"),
                Some(other) => bail!("unexpected frame before stdin stream: {:?}", other),
                None => bail!("sender closed before stdin stream"),
            }
            let path = stage_entry_path(&plan.stage_root, entry)?;
            let (size, digest) = receive_stream_bytes(stream, &path, entry.mode, progress).await?;
            update_transfer_hash(aggregate, entry, Some(digest.as_str()), Some(size))?;
            stats.files += 1;
            stats.bytes += size;
            progress.file_completed();
        }
    }

    match recv_frame(stream).await? {
        Some(Frame::Summary(summary)) => Ok(summary),
        Some(Frame::Error { message }) => bail!("sender aborted before summary: {message}"),
        Some(other) => bail!("unexpected frame after payload: {:?}", other),
        None => bail!("sender closed before transfer summary"),
    }
}

async fn build_plan(
    source: &str,
    output: Option<&str>,
    symlink_mode: SymlinkMode,
    device_mode: DeviceMode,
) -> Result<PlannedTransfer> {
    if source == "stdin" {
        let output = output.context("--output is required when --source stdin")?;
        validate_output_name(output)?;
        return Ok(PlannedTransfer {
            root_name: output.to_string(),
            stdin_source: true,
            entries: vec![PlannedEntry {
                manifest: ManifestEntry {
                    rel_path: String::new(),
                    kind: EntryKind::File,
                    size: None,
                    symlink_target: None,
                    device: None,
                    mode: None,
                },
                source_path: None,
            }],
            total_bytes: None,
        });
    }

    let source_path = PathBuf::from(source);
    let path_for_scan = source_path.clone();
    tokio::task::spawn_blocking(move || scan_source(path_for_scan, symlink_mode, device_mode))
        .await
        .context("source scan task failed")?
}

fn scan_source(
    source_path: PathBuf,
    symlink_mode: SymlinkMode,
    device_mode: DeviceMode,
) -> Result<PlannedTransfer> {
    let metadata = std::fs::symlink_metadata(&source_path)
        .with_context(|| format!("failed to inspect source {}", source_path.display()))?;
    let root_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .with_context(|| format!("source path {} must end with a valid UTF-8 file name", source_path.display()))?;

    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    scan_path(
        &source_path,
        Path::new(""),
        &metadata,
        symlink_mode,
        device_mode,
        &mut entries,
        &mut total_bytes,
    )?;

    Ok(PlannedTransfer {
        root_name,
        stdin_source: false,
        entries,
        total_bytes: Some(total_bytes),
    })
}

fn scan_path(
    abs_path: &Path,
    rel_path: &Path,
    metadata: &std::fs::Metadata,
    symlink_mode: SymlinkMode,
    device_mode: DeviceMode,
    entries: &mut Vec<PlannedEntry>,
    total_bytes: &mut u64,
) -> Result<()> {
    let file_type = metadata.file_type();
    let rel = rel_path_to_string(rel_path)?;
    let mode = unix_mode(metadata);

    if file_type.is_dir() {
        entries.push(PlannedEntry {
            manifest: ManifestEntry {
                rel_path: rel,
                kind: EntryKind::Directory,
                size: None,
                symlink_target: None,
                device: None,
                mode,
            },
            source_path: None,
        });

        let mut children = std::fs::read_dir(abs_path)
            .with_context(|| format!("failed to read directory {}", abs_path.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("failed to enumerate directory {}", abs_path.display()))?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let child_abs = child.path();
            let child_name = child
                .file_name()
                .into_string()
                .map_err(|_| anyhow!("path {} is not valid UTF-8", child_abs.display()))?;
            let child_rel = if rel_path.as_os_str().is_empty() {
                PathBuf::from(child_name)
            } else {
                rel_path.join(child_name)
            };
            let child_meta = std::fs::symlink_metadata(&child_abs)
                .with_context(|| format!("failed to inspect {}", child_abs.display()))?;
            scan_path(
                &child_abs,
                &child_rel,
                &child_meta,
                symlink_mode,
                device_mode,
                entries,
                total_bytes,
            )?;
        }
        return Ok(());
    }

    if file_type.is_file() {
        let size = metadata.len();
        *total_bytes = total_bytes
            .checked_add(size)
            .context("transfer size exceeds u64")?;
        entries.push(PlannedEntry {
            manifest: ManifestEntry {
                rel_path: rel,
                kind: EntryKind::File,
                size: Some(size),
                symlink_target: None,
                device: None,
                mode,
            },
            source_path: Some(abs_path.to_path_buf()),
        });
        return Ok(());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if file_type.is_symlink() {
            if symlink_mode == SymlinkMode::Exclude {
                if rel_path.as_os_str().is_empty() {
                    bail!("source is a symlink but --symlinks exclude was selected");
                }
                return Ok(());
            }
            let target = std::fs::read_link(abs_path)
                .with_context(|| format!("failed to read symlink {}", abs_path.display()))?;
            let target = target
                .to_str()
                .map(str::to_string)
                .with_context(|| format!("symlink target for {} is not valid UTF-8", abs_path.display()))?;
            entries.push(PlannedEntry {
                manifest: ManifestEntry {
                    rel_path: rel,
                    kind: EntryKind::Symlink,
                    size: None,
                    symlink_target: Some(target),
                    device: None,
                    mode,
                },
                source_path: None,
            });
            return Ok(());
        }
        if file_type.is_char_device() || file_type.is_block_device() {
            if device_mode == DeviceMode::Exclude {
                if rel_path.as_os_str().is_empty() {
                    bail!("source is a device node but --devices exclude was selected");
                }
                return Ok(());
            }
            let descriptor = describe_device(metadata)?;
            entries.push(PlannedEntry {
                manifest: ManifestEntry {
                    rel_path: rel,
                    kind: if file_type.is_char_device() {
                        EntryKind::CharDevice
                    } else {
                        EntryKind::BlockDevice
                    },
                    size: None,
                    symlink_target: None,
                    device: Some(descriptor),
                    mode,
                },
                source_path: None,
            });
            return Ok(());
        }
    }

    bail!("unsupported special file type at {}", abs_path.display())
}

fn validate_manifest(entries: &[ManifestEntry]) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut kinds = BTreeMap::new();
    for entry in entries {
        validate_rel_path(&entry.rel_path)?;
        if !seen.insert(entry.rel_path.clone()) {
            bail!("duplicate manifest path '{}'", display_rel_path(&entry.rel_path));
        }
        let path = PathBuf::from(&entry.rel_path);
        if !entry.rel_path.is_empty() {
            let mut ancestor = path.parent();
            while let Some(parent) = ancestor {
                if let Some(kind) = kinds.get(parent) {
                    if !kind.is_directory() {
                        bail!(
                            "manifest path '{}' is nested under non-directory '{}'",
                            display_rel_path(&entry.rel_path),
                            display_path(parent)
                        );
                    }
                }
                ancestor = parent.parent();
            }
        }
        kinds.insert(path, entry.kind);
    }
    Ok(())
}

async fn resolve_final_name(
    dest_path: &Path,
    requested_name: &str,
    collision_policy: CollisionPolicy,
) -> Result<String> {
    let requested_path = dest_path.join(requested_name);
    if !path_exists(&requested_path).await? {
        return Ok(requested_name.to_string());
    }
    match collision_policy {
        CollisionPolicy::Fail => bail!(
            "destination '{}' already exists",
            requested_path.display()
        ),
        CollisionPolicy::Overwrite => Ok(requested_name.to_string()),
        CollisionPolicy::Rename => unique_name(dest_path, requested_name).await,
    }
}

async fn unique_name(dest_path: &Path, requested_name: &str) -> Result<String> {
    let requested = Path::new(requested_name);
    let stem = requested
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(requested_name);
    let ext = requested.extension().and_then(|ext| ext.to_str());

    for idx in 1..10_000u32 {
        let candidate = match ext {
            Some(ext) if !ext.is_empty() => format!("{stem} ({idx}).{ext}"),
            _ => format!("{requested_name} ({idx})"),
        };
        if !path_exists(&dest_path.join(&candidate)).await? {
            return Ok(candidate);
        }
    }

    bail!("unable to find a unique destination name for '{requested_name}'")
}

async fn commit_stage(plan: &ReceiverPlan, collision_policy: CollisionPolicy) -> Result<PathBuf> {
    let final_path = plan
        .stage_root
        .parent()
        .context("staged root has no parent")?
        .parent()
        .context("staged root has no destination parent")?
        .join(&plan.final_name);

    if path_exists(&final_path).await? {
        match collision_policy {
            CollisionPolicy::Fail => bail!("destination '{}' already exists", final_path.display()),
            CollisionPolicy::Rename => bail!("rename policy should have selected a unique name before commit"),
            CollisionPolicy::Overwrite => {
                let backup = final_path
                    .parent()
                    .context("destination has no parent")?
                    .join(format!(".{}.bore-backup-{}", plan.final_name, Uuid::new_v4()));
                fs::rename(&final_path, &backup)
                    .await
                    .with_context(|| format!("failed to move existing destination {} out of the way", final_path.display()))?;
                if let Err(err) = fs::rename(&plan.stage_root, &final_path).await {
                    let _ = fs::rename(&backup, &final_path).await;
                    return Err(err).with_context(|| {
                        format!(
                            "failed to replace destination {} with staged transfer",
                            final_path.display()
                        )
                    });
                }
                cleanup_path(&backup).await?;
            }
        }
    } else {
        fs::rename(&plan.stage_root, &final_path)
            .await
            .with_context(|| format!("failed to publish staged transfer to {}", final_path.display()))?;
    }

    let _ = cleanup_path(&plan.stage_base).await;
    Ok(final_path)
}

async fn apply_static_entry(stage_root: &Path, entry: &ManifestEntry) -> Result<()> {
    let path = stage_entry_path(stage_root, entry)?;
    match entry.kind {
        EntryKind::Directory => {
            fs::create_dir_all(&path)
                .await
                .with_context(|| format!("failed to create directory {}", path.display()))?;
            apply_mode(&path, entry.mode).await?;
        }
        EntryKind::Symlink => {
            let target = entry
                .symlink_target
                .as_deref()
                .context("symlink entry is missing a target")?
                .to_string();
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).await?;
            }
            create_symlink(target, path).await?;
        }
        EntryKind::CharDevice | EntryKind::BlockDevice => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).await?;
            }
            create_device(&path, entry).await?;
        }
        EntryKind::File => {}
    }
    Ok(())
}

async fn send_file_bytes(
    stream: &mut TcpStream,
    source_path: &Path,
    expected_size: u64,
    progress: &ProgressTracker,
) -> Result<String> {
    let mut file = fs::File::open(source_path)
        .await
        .with_context(|| format!("failed to open source file {}", source_path.display()))?;
    let mut remaining = expected_size;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
    while remaining > 0 {
        let chunk = remaining.min(buffer.len() as u64) as usize;
        let n = file
            .read(&mut buffer[..chunk])
            .await
            .with_context(|| format!("failed to read source file {}", source_path.display()))?;
        if n == 0 {
            bail!(
                "source file {} ended early: expected {} more bytes",
                source_path.display(),
                remaining
            );
        }
        stream.write_all(&buffer[..n]).await?;
        hasher.update(&buffer[..n]);
        remaining -= n as u64;
        progress.add_bytes(n as u64);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

async fn receive_file_bytes(
    stream: &mut TcpStream,
    target_path: &Path,
    expected_size: u64,
    mode: Option<u32>,
    progress: &ProgressTracker,
) -> Result<String> {
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut file = fs::File::create(target_path)
        .await
        .with_context(|| format!("failed to create destination file {}", target_path.display()))?;
    let mut remaining = expected_size;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
    while remaining > 0 {
        let chunk = remaining.min(buffer.len() as u64) as usize;
        stream
            .read_exact(&mut buffer[..chunk])
            .await
            .with_context(|| format!("failed to receive payload for {}", target_path.display()))?;
        file.write_all(&buffer[..chunk]).await?;
        hasher.update(&buffer[..chunk]);
        remaining -= chunk as u64;
        progress.add_bytes(chunk as u64);
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    apply_mode(target_path, mode).await?;
    Ok(hasher.finalize().to_hex().to_string())
}

async fn send_stdin_bytes(stream: &mut TcpStream, progress: &ProgressTracker) -> Result<(u64, String)> {
    let mut stdin = io::stdin();
    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
    let mut total = 0u64;
    let mut hasher = blake3::Hasher::new();
    loop {
        let read = stdin.read(&mut buffer).await.context("failed to read stdin")?;
        if read == 0 {
            break;
        }
        send_frame(stream, &Frame::StreamChunk { len: read as u32 }).await?;
        stream.write_all(&buffer[..read]).await?;
        hasher.update(&buffer[..read]);
        total += read as u64;
        progress.add_bytes(read as u64);
    }
    Ok((total, hasher.finalize().to_hex().to_string()))
}

async fn receive_stream_bytes(
    stream: &mut TcpStream,
    target_path: &Path,
    mode: Option<u32>,
    progress: &ProgressTracker,
) -> Result<(u64, String)> {
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut file = fs::File::create(target_path)
        .await
        .with_context(|| format!("failed to create destination file {}", target_path.display()))?;
    let mut total = 0u64;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];

    loop {
        match recv_frame(stream).await? {
            Some(Frame::StreamChunk { len }) => {
                let len = len as usize;
                if len > buffer.len() {
                    buffer.resize(len, 0);
                }
                stream
                    .read_exact(&mut buffer[..len])
                    .await
                    .with_context(|| format!("failed to receive streamed payload for {}", target_path.display()))?;
                file.write_all(&buffer[..len]).await?;
                hasher.update(&buffer[..len]);
                total += len as u64;
                progress.add_bytes(len as u64);
            }
            Some(Frame::StreamEnd {
                rel_path: _,
                size,
                blake3,
            }) => {
                file.flush().await?;
                file.sync_all().await?;
                drop(file);
                apply_mode(target_path, mode).await?;
                let digest = hasher.finalize().to_hex().to_string();
                if size != total {
                    bail!(
                        "stream size mismatch for {}: sender={}, receiver={total}",
                        target_path.display(),
                        size
                    );
                }
                if blake3 != digest {
                    bail!(
                        "stream digest mismatch for {}: sender={}, receiver={digest}",
                        target_path.display(),
                        blake3
                    );
                }
                return Ok((total, digest));
            }
            Some(Frame::Error { message }) => bail!("sender aborted during stdin stream: {message}"),
            Some(other) => bail!("unexpected frame inside streamed payload: {:?}", other),
            None => bail!("sender closed before the streamed payload ended"),
        }
    }
}

fn update_transfer_hash(
    hasher: &mut blake3::Hasher,
    entry: &ManifestEntry,
    digest: Option<&str>,
    size: Option<u64>,
) -> Result<()> {
    let record = TransferRecord {
        rel_path: &entry.rel_path,
        kind: entry.kind,
        size,
        blake3: digest,
        symlink_target: entry.symlink_target.as_deref(),
        device: entry.device.as_ref(),
    };
    let encoded = serde_json::to_vec(&record)?;
    hasher.update(&(encoded.len() as u32).to_le_bytes());
    hasher.update(&encoded);
    Ok(())
}

fn bump_static_stats(stats: &mut TransferStats, kind: EntryKind) {
    match kind {
        EntryKind::Directory => stats.dirs += 1,
        EntryKind::Symlink => stats.symlinks += 1,
        EntryKind::CharDevice | EntryKind::BlockDevice => stats.devices += 1,
        EntryKind::File => {}
    }
}

async fn send_frame(stream: &mut TcpStream, frame: &Frame) -> Result<()> {
    let payload = serde_json::to_vec(frame)?;
    if payload.len() > FRAME_LIMIT {
        bail!("transfer frame exceeds size limit ({})", payload.len());
    }
    stream.write_u32_le(payload.len() as u32).await?;
    stream.write_all(&payload).await?;
    Ok(())
}

async fn recv_frame(stream: &mut TcpStream) -> Result<Option<Frame>> {
    let len = match stream.read_u32_le().await {
        Ok(len) => len as usize,
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err).context("failed to read transfer frame length"),
    };
    if len > FRAME_LIMIT {
        bail!("transfer frame length {} exceeds limit {}", len, FRAME_LIMIT);
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(Some(serde_json::from_slice(&payload)?))
}

async fn send_protocol_error(stream: &mut TcpStream, message: String) {
    let _ = send_frame(stream, &Frame::Error { message }).await;
}

async fn cleanup_stage(path: &Path) -> Result<()> {
    if path_exists(path).await? {
        cleanup_path(path).await?;
    }
    Ok(())
}

async fn cleanup_path(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).await?;
    if meta.is_dir() {
        fs::remove_dir_all(path).await?;
    } else {
        fs::remove_file(path).await?;
    }
    Ok(())
}

async fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path).await {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn stage_entry_path(stage_root: &Path, entry: &ManifestEntry) -> Result<PathBuf> {
    if entry.rel_path.is_empty() {
        return Ok(stage_root.to_path_buf());
    }
    validate_rel_path(&entry.rel_path)?;
    Ok(stage_root.join(&entry.rel_path))
}

fn validate_rel_path(rel_path: &str) -> Result<()> {
    let path = Path::new(rel_path);
    if path.is_absolute() {
        bail!("manifest path '{}' must be relative", rel_path);
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir => bail!("manifest path '{}' escapes the transfer root", rel_path),
            Component::RootDir | Component::Prefix(_) => {
                bail!("manifest path '{}' must be relative", rel_path)
            }
        }
    }
    Ok(())
}

fn validate_output_name(output: &str) -> Result<()> {
    let path = Path::new(output);
    if output.is_empty() {
        bail!("--output must not be empty");
    }
    if path.components().count() != 1 || !matches!(path.components().next(), Some(Component::Normal(_))) {
        bail!("--output must be a single file name, not a path");
    }
    Ok(())
}

fn rel_path_to_string(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty() {
        return Ok(String::new());
    }
    path.to_str()
        .map(str::to_string)
        .with_context(|| format!("path '{}' is not valid UTF-8", path.display()))
}

fn display_rel_path(rel_path: &str) -> &str {
    if rel_path.is_empty() {
        "."
    } else {
        rel_path
    }
}

fn display_path(path: &Path) -> String {
    let rendered = path.to_string_lossy();
    if rendered.is_empty() {
        ".".to_string()
    } else {
        rendered.into_owned()
    }
}

fn generate_transfer_id() -> String {
    Uuid::new_v4().to_string()
}

fn transfer_security(path: &TransferPathInfo) -> &'static str {
    if path.direct {
        "quic-encrypted"
    } else if path.relay_tls {
        "tls"
    } else {
        "plain"
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn truncate_item(item: &str) -> String {
    const LIMIT: usize = 48;
    if item.chars().count() <= LIMIT {
        item.to_string()
    } else {
        let prefix: String = item.chars().take(LIMIT - 3).collect();
        format!("{prefix}...")
    }
}

fn unix_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return Some(metadata.permissions().mode());
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

async fn apply_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = mode {
            fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[cfg(unix)]
fn describe_device(metadata: &std::fs::Metadata) -> Result<DeviceDescriptor> {
    use nix::sys::stat::{major, minor};
    use std::os::unix::fs::MetadataExt;
    let dev = metadata.rdev();
    Ok(DeviceDescriptor {
        major: major(dev),
        minor: minor(dev),
    })
}

#[cfg(not(unix))]
fn describe_device(_metadata: &std::fs::Metadata) -> Result<DeviceDescriptor> {
    bail!("device node transfer is only supported on Unix")
}

#[cfg(unix)]
async fn create_symlink(target: String, path: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        std::os::unix::fs::symlink(&target, &path)
            .with_context(|| format!("failed to create symlink {} -> {}", path.display(), target))
    })
    .await
    .context("symlink creation task failed")?
}

#[cfg(windows)]
async fn create_symlink(target: String, path: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        std::os::windows::fs::symlink_file(&target, &path)
            .with_context(|| format!("failed to create symlink {} -> {}", path.display(), target))
    })
    .await
    .context("symlink creation task failed")?
}

#[cfg(not(any(unix, windows)))]
async fn create_symlink(_target: String, _path: PathBuf) -> Result<()> {
    bail!("symlink transfer is not supported on this platform")
}

#[cfg(unix)]
async fn create_device(path: &Path, entry: &ManifestEntry) -> Result<()> {
    use nix::sys::stat::{makedev, mknod, Mode, SFlag};

    let descriptor = entry
        .device
        .as_ref()
        .context("device entry is missing major/minor information")?
        .clone();
    let mode = entry.mode.unwrap_or(0o600);
    let flag = match entry.kind {
        EntryKind::CharDevice => SFlag::S_IFCHR,
        EntryKind::BlockDevice => SFlag::S_IFBLK,
        _ => bail!("device creation requested for non-device entry"),
    };
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        mknod(
            &path,
            flag,
            Mode::from_bits_truncate(mode),
            makedev(descriptor.major, descriptor.minor),
        )
        .with_context(|| format!("failed to create device node {}", path.display()))
    })
    .await
    .context("device creation task failed")?
}

#[cfg(not(unix))]
async fn create_device(_path: &Path, _entry: &ManifestEntry) -> Result<()> {
    bail!("device node transfer is only supported on Unix")
}

impl PartialEq for SummaryFrame {
    fn eq(&self, other: &Self) -> bool {
        self.files == other.files
            && self.dirs == other.dirs
            && self.symlinks == other.symlinks
            && self.devices == other.devices
            && self.bytes == other.bytes
            && self.blake3 == other.blake3
    }
}

impl Eq for SummaryFrame {}
*/
