//! Streaming ZIP STORED producer for multi-path transfer links.
//!
//! Each HTTP GET owns one writer and one bounded duplex buffer.  The manifest
//! is shared, but offsets, CRCs, central directory and SHA-256 are private to
//! that GET.  No archive is spooled to disk.

use std::path::Path;

use async_zip::{base::write::ZipFileWriter, Compression, ZipEntryBuilder};
use bytes::Bytes;
use futures_lite::io::AsyncWriteExt as FuturesAsyncWriteExt;
use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::manifest::{fingerprint, ManifestEntryKind, PreparedArchive};
use super::source::{FileProducerHandle, SourceError};
use super::stats::{SourceCompletion, SourceFailure, SourceMessage};

const ZIP_PIPE_CAPACITY: usize = 256 * 1024;
const ZIP_READ_CHUNK: usize = 256 * 1024;

/// Spawn one independent streaming ZIP producer.
pub(crate) fn spawn_archive_producer(
    archive: PreparedArchive,
    cancellation: CancellationToken,
) -> FileProducerHandle {
    let (sender, receiver) = mpsc::channel(2);
    let completion = std::sync::Arc::new(std::sync::Mutex::new(None));
    let task_completion = std::sync::Arc::clone(&completion);
    let owner_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        match produce_archive(&archive, &cancellation, &sender).await {
            Ok(completion) => {
                *task_completion.lock().expect("completion mutex poisoned") =
                    Some(completion.clone());
                let _ =
                    send_message(&sender, SourceMessage::Complete(completion), &cancellation).await;
            }
            Err((bytes_read, error)) => {
                if invalidates_manifest(&error) {
                    archive.manifest.invalidate();
                }
                let _ = sender.try_send(SourceMessage::Failed(SourceFailure {
                    bytes_read,
                    error: error.to_string(),
                }));
            }
        }
    });
    FileProducerHandle::from_parts_with_completion(receiver, task, completion, owner_cancellation)
}

fn invalidates_manifest(error: &SourceError) -> bool {
    !matches!(error, SourceError::Cancelled | SourceError::ReceiverClosed)
}

async fn produce_archive(
    archive: &PreparedArchive,
    cancellation: &CancellationToken,
    sender: &mpsc::Sender<SourceMessage>,
) -> Result<SourceCompletion, (u64, SourceError)> {
    archive
        .manifest
        .validate()
        .await
        .map_err(|error| (0, error))?;

    let (writer_io, mut reader) = tokio::io::duplex(ZIP_PIPE_CAPACITY);
    let manifest = archive.manifest.clone();
    let writer_cancellation = cancellation.clone();
    let writer_task =
        tokio::spawn(async move { write_zip(manifest, writer_io, writer_cancellation).await });
    let mut writer_guard = JoinGuard(Some(writer_task));

    let mut hasher = Sha256::new();
    let mut bytes_read = 0u64;
    let mut buffer = vec![0u8; ZIP_READ_CHUNK];
    loop {
        let read = tokio::select! {
            _ = cancellation.cancelled() => {
                return Err((bytes_read, SourceError::Cancelled));
            }
            result = reader.read(&mut buffer) => result
                .map_err(|error| (bytes_read, SourceError::io("read ZIP pipeline", error)))?,
        };
        if read == 0 {
            break;
        }
        bytes_read = bytes_read
            .checked_add(read as u64)
            .ok_or((bytes_read, SourceError::SizeOverflow))?;
        hasher.update(&buffer[..read]);
        send_message(
            sender,
            SourceMessage::Data(Bytes::copy_from_slice(&buffer[..read])),
            cancellation,
        )
        .await
        .map_err(|error| (bytes_read, error))?;
    }

    let writer_result = writer_guard
        .0
        .take()
        .expect("ZIP writer task owned by guard")
        .await
        .map_err(|error| (bytes_read, SourceError::Task(error.to_string())))?;
    writer_result.map_err(|error| (bytes_read, error))?;
    Ok(SourceCompletion {
        bytes_read,
        sha256: hasher.finalize().into(),
    })
}

struct JoinGuard(Option<JoinHandle<Result<(), SourceError>>>);

impl Drop for JoinGuard {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

async fn write_zip(
    manifest: super::manifest::SourceManifest,
    writer_io: tokio::io::DuplexStream,
    cancellation: CancellationToken,
) -> Result<(), SourceError> {
    let mut writer = ZipFileWriter::with_tokio(writer_io).force_zip64();
    for entry in manifest.entries.iter() {
        if cancellation.is_cancelled() {
            return Err(SourceError::Cancelled);
        }
        let builder = ZipEntryBuilder::new(entry.zip_name.clone().into(), Compression::Stored)
            .uncompressed_size(entry.size)
            .compressed_size(entry.size);
        match entry.kind {
            ManifestEntryKind::Directory => {
                writer
                    .write_entry_whole(builder, &[])
                    .await
                    .map_err(|error| SourceError::Manifest {
                        detail: format!("ZIP directory entry failed: {error}"),
                    })?
            }
            ManifestEntryKind::File => {
                let mut entry_writer =
                    writer.write_entry_stream(builder).await.map_err(|error| {
                        SourceError::Manifest {
                            detail: format!("ZIP file entry failed: {error}"),
                        }
                    })?;
                let result = copy_file_entry(entry, &mut entry_writer, &cancellation).await;
                result?;
                entry_writer
                    .close()
                    .await
                    .map_err(|error| SourceError::Manifest {
                        detail: format!("ZIP file entry close failed: {error}"),
                    })?;
            }
        }
    }
    manifest.validate().await?;
    writer
        .close()
        .await
        .map_err(|error| SourceError::Manifest {
            detail: format!("ZIP archive close failed: {error}"),
        })?;
    Ok(())
}

async fn copy_file_entry(
    entry: &super::manifest::ManifestEntry,
    writer: &mut async_zip::base::write::EntryStreamWriter<
        '_,
        tokio_util::compat::Compat<tokio::io::DuplexStream>,
    >,
    cancellation: &CancellationToken,
) -> Result<(), SourceError> {
    let mut file = open_manifest_file(entry).await?;
    let mut remaining = entry.size;
    let mut buffer = vec![0u8; ZIP_READ_CHUNK];
    while remaining > 0 {
        let read_len = remaining.min(ZIP_READ_CHUNK as u64) as usize;
        let read = tokio::select! {
            _ = cancellation.cancelled() => return Err(SourceError::Cancelled),
            result = file.read(&mut buffer[..read_len]) => result
                .map_err(|error| SourceError::io("read ZIP source", error))?,
        };
        if read == 0 {
            return Err(SourceError::UnexpectedEof {
                expected: entry.size,
                read: entry.size - remaining,
            });
        }
        writer
            .write_all(&buffer[..read])
            .await
            .map_err(|error| SourceError::Manifest {
                detail: format!("write ZIP entry failed: {error}"),
            })?;
        remaining -= read as u64;
    }
    let mut extra = [0u8; 1];
    let extra_read = file
        .read(&mut extra)
        .await
        .map_err(|error| SourceError::io("check ZIP source tail", error))?;
    if extra_read != 0 {
        return Err(SourceError::Grew);
    }
    let handle_metadata = file
        .metadata()
        .await
        .map_err(|error| SourceError::io("stat ZIP source handle", error))?;
    let path_metadata = tokio::fs::symlink_metadata(&entry.source_path)
        .await
        .map_err(|error| SourceError::io("stat ZIP source path", error))?;
    if path_metadata.file_type().is_symlink()
        || !handle_metadata.is_file()
        || fingerprint(&handle_metadata, ManifestEntryKind::File) != entry.fingerprint
        || fingerprint(&path_metadata, ManifestEntryKind::File) != entry.fingerprint
    {
        return Err(SourceError::Changed);
    }
    Ok(())
}

async fn open_manifest_file(entry: &super::manifest::ManifestEntry) -> Result<File, SourceError> {
    let path_metadata = tokio::fs::symlink_metadata(&entry.source_path)
        .await
        .map_err(|error| SourceError::io("stat ZIP source", error))?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || fingerprint(&path_metadata, ManifestEntryKind::File) != entry.fingerprint
    {
        return Err(SourceError::Changed);
    }
    let path = entry.source_path.clone();
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
    .map_err(|error| SourceError::io("open ZIP source", error))?;
    let opened = File::from_std(opened);
    let metadata = opened
        .metadata()
        .await
        .map_err(|error| SourceError::io("stat opened ZIP source", error))?;
    if !metadata.is_file() || fingerprint(&metadata, ManifestEntryKind::File) != entry.fingerprint {
        return Err(SourceError::Changed);
    }
    Ok(opened)
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

// Keep the path import in the module's public documentation meaningful on all
// supported targets; it also makes the no-follow intent explicit to reviewers.
#[allow(dead_code)]
fn _source_path(_path: &Path) {}
