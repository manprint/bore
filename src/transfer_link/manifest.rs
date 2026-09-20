//! Deterministic, metadata-only manifests for multi-path transfer links.
//!
//! A manifest is built before the vhost URL is announced.  It contains names,
//! filesystem paths and bounded metadata only; file contents are opened by the
//! per-download producer later.  Symlinks and special files are rejected at
//! every traversal step, so a source cannot silently escape the selected
//! roots.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use tokio::fs;

use super::oneshot::PreparedStream;
use super::source::{validate_filename, PreparedFile, SourceError};

/// Maximum number of ZIP entries retained by one transfer-link manifest.
pub const MAX_MANIFEST_ENTRIES: usize = 100_000;
/// Maximum cumulative source-path and ZIP-name bytes retained by a manifest.
pub const MAX_MANIFEST_PATH_BYTES: usize = 32 * 1024 * 1024;
/// Maximum number of path components in one ZIP entry.
pub const MAX_MANIFEST_DEPTH: usize = 256;

/// The kind of filesystem node represented by one manifest entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManifestEntryKind {
    /// A directory entry.  Its ZIP name ends in `/`.
    Directory,
    /// A regular file entry.
    File,
}

/// Portable identity and metadata used to detect source replacement/mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeFingerprint {
    /// Node kind captured at preparation time.
    pub kind: ManifestEntryKind,
    /// Size reported by the filesystem.
    pub size: u64,
    /// Modification timestamp when available.
    pub modified: Option<SystemTime>,
    /// Unix device number when available.
    pub device: Option<u64>,
    /// Unix inode number when available.
    pub inode: Option<u64>,
    /// Unix change timestamp when available.
    pub changed: Option<(i64, u32)>,
}

/// One direct child recorded in a directory membership snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestChild {
    /// UTF-8 basename as it appeared in the directory.
    pub name: String,
    /// Child kind.
    pub kind: ManifestEntryKind,
    /// Child identity and metadata.
    pub fingerprint: NodeFingerprint,
}

/// A directory's metadata plus its sorted direct membership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectorySnapshot {
    /// Directory identity and metadata.
    pub fingerprint: NodeFingerprint,
    /// Direct children in deterministic UTF-8 order.
    pub children: Vec<ManifestChild>,
}

/// One stable manifest entry.  The manifest never stores payload bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestEntry {
    /// Relative ZIP name, with `/` for directory entries.
    pub zip_name: String,
    /// Original filesystem path opened by a download producer.
    pub source_path: PathBuf,
    /// Entry kind.
    pub kind: ManifestEntryKind,
    /// File size, or zero for a directory.
    pub size: u64,
    /// File metadata captured during preparation.
    pub fingerprint: NodeFingerprint,
    /// Direct directory membership, present only for directories.
    pub directory: Option<DirectorySnapshot>,
}

/// A deterministic source manifest shared read-only by all downloads.
#[derive(Clone, Debug)]
pub struct SourceManifest {
    /// Selected roots, retained for diagnostics.
    pub roots: Vec<PathBuf>,
    /// Entries in deterministic archive order.
    pub entries: Arc<Vec<ManifestEntry>>,
    /// Cumulative path/name bytes used by this manifest.
    pub path_bytes: usize,
    /// True after any validation failure.  Once unstable, all future GETs are
    /// rejected until the sender starts a new command.
    invalidated: Arc<AtomicBool>,
}

impl SourceManifest {
    /// Return the number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether the manifest contains no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return true after a source mutation or read-side metadata failure.
    pub fn is_invalidated(&self) -> bool {
        self.invalidated.load(Ordering::Acquire)
    }

    /// Revalidate every file, directory and direct membership snapshot.
    pub async fn validate(&self) -> Result<(), SourceError> {
        if self.is_invalidated() {
            return Err(SourceError::Manifest {
                detail: "source manifest was previously invalidated".to_owned(),
            });
        }
        for entry in self.entries.iter() {
            if let Err(error) = validate_entry(entry).await {
                self.invalidated.store(true, Ordering::Release);
                return Err(SourceError::Manifest {
                    detail: error.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Mark this source unavailable after a producer-side mutation/read error.
    pub fn invalidate(&self) {
        self.invalidated.store(true, Ordering::Release);
    }
}

/// Prepared archive source used by the HTTP and ZIP layers.
#[derive(Clone, Debug)]
pub struct PreparedArchive {
    /// Immutable source manifest.
    pub manifest: SourceManifest,
    /// Attachment name advertised to clients.
    pub filename: String,
}

impl PreparedArchive {
    /// Build a deterministic archive manifest for the selected paths.
    pub async fn from_paths(
        paths: Vec<PathBuf>,
        override_filename: Option<&str>,
    ) -> Result<Self, ManifestError> {
        build_manifest(paths, override_filename).await
    }
}

/// Build a prepared source: preserve a lone regular file's raw-byte behavior,
/// and use an archive for any directory or multi-path selection.
pub async fn prepare_selection(
    paths: Vec<PathBuf>,
    override_filename: Option<&str>,
) -> Result<PreparedSource, SourceError> {
    if paths.is_empty() {
        return Err(SourceError::Manifest {
            detail: "at least one transfer-link source path is required".to_owned(),
        });
    }
    if paths.len() == 1 {
        let metadata = fs::symlink_metadata(&paths[0])
            .await
            .map_err(|error| SourceError::io("stat transfer-link source", error))?;
        if metadata.file_type().is_symlink() {
            return Err(SourceError::Symlink);
        }
        if metadata.is_file() {
            return super::source::prepare_file(
                paths.into_iter().next().unwrap(),
                override_filename,
            )
            .await
            .map(PreparedSource::File);
        }
    }
    PreparedArchive::from_paths(paths, override_filename)
        .await
        .map(PreparedSource::Archive)
        .map_err(|error| SourceError::Manifest {
            detail: error.to_string(),
        })
}

/// A source selected by the command line.
#[derive(Clone, Debug)]
pub enum PreparedSource {
    /// One regular file streamed byte-for-byte.
    File(PreparedFile),
    /// Multiple paths or a directory streamed as ZIP STORED.
    Archive(PreparedArchive),
    /// A one-shot stdin or supervised-exec stream.
    OneShot(PreparedStream),
}

impl PreparedSource {
    /// Attachment filename advertised by HTTP.
    pub fn filename(&self) -> &str {
        match self {
            Self::File(file) => &file.filename,
            Self::Archive(archive) => &archive.filename,
            Self::OneShot(stream) => &stream.filename,
        }
    }

    /// Known exact body size, available only for raw files.
    pub fn known_size(&self) -> Option<u64> {
        match self {
            Self::File(file) => Some(file.size),
            Self::Archive(_) => None,
            Self::OneShot(_) => None,
        }
    }

    /// Return true when this source is a streaming ZIP archive.
    pub fn is_archive(&self) -> bool {
        matches!(self, Self::Archive(_))
    }

    /// Return true when this source is a one-shot stream with no known length.
    pub fn is_one_shot(&self) -> bool {
        matches!(self, Self::OneShot(_))
    }
}

/// Manifest preparation failures are explicit and user-actionable.
#[derive(Debug)]
pub enum ManifestError {
    /// No path was supplied.
    EmptySelection,
    /// Filesystem operation failed.
    Io {
        /// Filesystem operation being attempted.
        operation: &'static str,
        /// Underlying operating-system error text.
        message: String,
    },
    /// A symbolic link was selected or appeared during traversal.
    Symlink {
        /// Rejected symbolic-link path.
        path: PathBuf,
    },
    /// A non-file/non-directory node was selected.
    Special {
        /// Rejected non-file/non-directory path.
        path: PathBuf,
    },
    /// A path/name is not representable as a safe ZIP UTF-8 name.
    InvalidName {
        /// Filesystem path whose archive component was invalid.
        path: PathBuf,
        /// Validation detail.
        detail: String,
    },
    /// Two roots or entries would collide in the archive.
    Collision {
        /// First colliding archive name.
        first: String,
        /// Second colliding archive name.
        second: String,
    },
    /// One selected root contains another selected root.
    Overlap {
        /// First overlapping selected path.
        first: PathBuf,
        /// Second overlapping selected path.
        second: PathBuf,
    },
    /// Entry count exceeds the bounded contract.
    TooManyEntries {
        /// Configured maximum.
        limit: usize,
    },
    /// Cumulative path/name bytes exceed the bounded contract.
    TooManyPathBytes {
        /// Configured maximum.
        limit: usize,
    },
    /// Entry nesting exceeds the bounded contract.
    TooDeep {
        /// ZIP path that exceeded the limit.
        path: String,
        /// Configured maximum depth.
        limit: usize,
    },
    /// Source metadata changed while preparing the manifest.
    Changed {
        /// Source path whose metadata changed.
        path: PathBuf,
    },
    /// Filename override is invalid.
    Filename(String),
}

impl ManifestError {
    fn io(operation: &'static str, error: std::io::Error) -> Self {
        Self::Io {
            operation,
            message: error.to_string(),
        }
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySelection => f.write_str("at least one source path is required"),
            Self::Io { operation, message } => write!(f, "{operation} failed: {message}"),
            Self::Symlink { path } => {
                write!(f, "source path is a symbolic link: {}", path.display())
            }
            Self::Special { path } => write!(
                f,
                "source path is not a regular file or directory: {}",
                path.display()
            ),
            Self::InvalidName { path, detail } => {
                write!(f, "invalid ZIP name for {}: {detail}", path.display())
            }
            Self::Collision { first, second } => {
                write!(f, "ZIP entry collision between {first:?} and {second:?}")
            }
            Self::Overlap { first, second } => write!(
                f,
                "selected source paths overlap: {} and {}",
                first.display(),
                second.display()
            ),
            Self::TooManyEntries { limit } => write!(f, "manifest exceeds the {limit} entry limit"),
            Self::TooManyPathBytes { limit } => {
                write!(f, "manifest exceeds the {limit} path/name byte limit")
            }
            Self::TooDeep { path, limit } => {
                write!(f, "ZIP path {path:?} exceeds the depth limit {limit}")
            }
            Self::Changed { path } => write!(
                f,
                "source changed while building manifest: {}",
                path.display()
            ),
            Self::Filename(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for ManifestError {}

async fn build_manifest(
    paths: Vec<PathBuf>,
    override_filename: Option<&str>,
) -> Result<PreparedArchive, ManifestError> {
    if paths.is_empty() {
        return Err(ManifestError::EmptySelection);
    }
    let filename = match override_filename {
        Some(name) => {
            validate_filename(name).map_err(|error| ManifestError::Filename(error.to_string()))?;
            name.to_owned()
        }
        None => "download.zip".to_owned(),
    };

    let mut roots: Vec<PathBuf> = Vec::with_capacity(paths.len());
    let mut root_names = HashMap::<String, String>::new();
    for path in paths {
        let path = lexical_clean(path);
        let metadata = fs::symlink_metadata(&path)
            .await
            .map_err(|error| ManifestError::io("stat manifest root", error))?;
        let kind = node_kind(&metadata, &path)?;
        let basename = path.file_name().ok_or_else(|| ManifestError::InvalidName {
            path: path.clone(),
            detail: "source has no basename".to_owned(),
        })?;
        let basename = basename
            .to_str()
            .ok_or_else(|| ManifestError::InvalidName {
                path: path.clone(),
                detail: "basename is not valid UTF-8".to_owned(),
            })?;
        validate_zip_component(basename).map_err(|detail| ManifestError::InvalidName {
            path: path.clone(),
            detail,
        })?;
        let key = basename.to_ascii_lowercase();
        if let Some(previous) = root_names.insert(key, basename.to_owned()) {
            return Err(ManifestError::Collision {
                first: previous,
                second: basename.to_owned(),
            });
        }
        for previous in &roots {
            if previous == &path || previous.starts_with(&path) || path.starts_with(previous) {
                return Err(ManifestError::Overlap {
                    first: previous.clone(),
                    second: path,
                });
            }
        }
        let _ = kind;
        roots.push(path);
    }

    let mut builder = ManifestBuilder {
        entries: Vec::new(),
        seen_names: HashSet::new(),
        seen_folded: HashMap::new(),
        path_bytes: 0,
        reserved_entries: 0,
    };
    for root in &roots {
        let metadata = fs::symlink_metadata(root)
            .await
            .map_err(|error| ManifestError::io("restat manifest root", error))?;
        let kind = node_kind(&metadata, root)?;
        let name = root
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| ManifestError::InvalidName {
                path: root.to_path_buf(),
                detail: "basename is not valid UTF-8".to_owned(),
            })?;
        let zip_name = if kind == ManifestEntryKind::Directory {
            format!("{name}/")
        } else {
            name.to_owned()
        };
        builder.walk(root.to_path_buf(), zip_name, kind, 1).await?;
    }
    builder
        .entries
        .sort_by(|left, right| left.zip_name.cmp(&right.zip_name));
    Ok(PreparedArchive {
        manifest: SourceManifest {
            roots,
            entries: Arc::new(builder.entries),
            path_bytes: builder.path_bytes,
            invalidated: Arc::new(AtomicBool::new(false)),
        },
        filename,
    })
}

struct ManifestBuilder {
    entries: Vec<ManifestEntry>,
    seen_names: HashSet<String>,
    seen_folded: HashMap<String, String>,
    path_bytes: usize,
    /// Entries reserved while enumerating directories, including pending
    /// nodes that have not yet been walked.  This keeps the memory bound
    /// global rather than applying it independently to each directory.
    reserved_entries: usize,
}

struct PendingNode {
    path: PathBuf,
    zip_name: String,
    kind: ManifestEntryKind,
    depth: usize,
    /// Directory enumeration already reserved this node's manifest budget.
    reserved: bool,
}

impl ManifestBuilder {
    async fn walk(
        &mut self,
        root: PathBuf,
        root_zip_name: String,
        root_kind: ManifestEntryKind,
        root_depth: usize,
    ) -> Result<(), ManifestError> {
        let mut stack = vec![PendingNode {
            path: root,
            zip_name: root_zip_name,
            kind: root_kind,
            depth: root_depth,
            reserved: false,
        }];
        while let Some(node) = stack.pop() {
            if node.depth > MAX_MANIFEST_DEPTH {
                return Err(ManifestError::TooDeep {
                    path: node.zip_name,
                    limit: MAX_MANIFEST_DEPTH,
                });
            }
            if !node.reserved {
                self.reserve_entry(&node.path, &node.zip_name)?;
            }
            let metadata = fs::symlink_metadata(&node.path)
                .await
                .map_err(|error| ManifestError::io("stat manifest entry", error))?;
            let actual_kind = node_kind(&metadata, &node.path)?;
            if actual_kind != node.kind {
                return Err(ManifestError::Changed { path: node.path });
            }
            let node_fingerprint = fingerprint(&metadata, node.kind);
            if node.kind == ManifestEntryKind::Directory {
                let (children, pending) = self.read_directory(&node.path, &node.zip_name).await?;
                let after = fs::symlink_metadata(&node.path)
                    .await
                    .map_err(|error| ManifestError::io("restat manifest directory", error))?;
                if fingerprint(&after, ManifestEntryKind::Directory) != node_fingerprint {
                    return Err(ManifestError::Changed { path: node.path });
                }
                self.entries.push(ManifestEntry {
                    zip_name: node.zip_name.clone(),
                    source_path: node.path.clone(),
                    kind: node.kind,
                    size: 0,
                    fingerprint: node_fingerprint,
                    directory: Some(DirectorySnapshot {
                        fingerprint: fingerprint(&after, ManifestEntryKind::Directory),
                        children,
                    }),
                });
                for child in pending.into_iter().rev() {
                    stack.push(child);
                }
            } else {
                self.entries.push(ManifestEntry {
                    zip_name: node.zip_name,
                    source_path: node.path,
                    kind: node.kind,
                    size: metadata.len(),
                    fingerprint: node_fingerprint,
                    directory: None,
                });
            }
        }
        Ok(())
    }

    fn reserve_entry(&mut self, source_path: &Path, zip_name: &str) -> Result<(), ManifestError> {
        if self.reserved_entries >= MAX_MANIFEST_ENTRIES {
            return Err(ManifestError::TooManyEntries {
                limit: MAX_MANIFEST_ENTRIES,
            });
        }
        let bytes = source_path_bytes(source_path)
            .checked_add(zip_name.len())
            .ok_or(ManifestError::TooManyPathBytes {
                limit: MAX_MANIFEST_PATH_BYTES,
            })?;
        let next = self
            .path_bytes
            .checked_add(bytes)
            .ok_or(ManifestError::TooManyPathBytes {
                limit: MAX_MANIFEST_PATH_BYTES,
            })?;
        if next > MAX_MANIFEST_PATH_BYTES {
            return Err(ManifestError::TooManyPathBytes {
                limit: MAX_MANIFEST_PATH_BYTES,
            });
        }
        let folded = zip_name.trim_end_matches('/').to_ascii_lowercase();
        if !self.seen_names.insert(zip_name.to_owned()) {
            return Err(ManifestError::Collision {
                first: zip_name.to_owned(),
                second: zip_name.to_owned(),
            });
        }
        if let Some(previous) = self.seen_folded.insert(folded, zip_name.to_owned()) {
            return Err(ManifestError::Collision {
                first: previous,
                second: zip_name.to_owned(),
            });
        }
        self.path_bytes = next;
        self.reserved_entries += 1;
        Ok(())
    }

    async fn read_directory(
        &mut self,
        path: &Path,
        parent_zip_name: &str,
    ) -> Result<(Vec<ManifestChild>, Vec<PendingNode>), ManifestError> {
        let remaining_entries = MAX_MANIFEST_ENTRIES.saturating_sub(self.reserved_entries);
        let remaining_path_bytes = MAX_MANIFEST_PATH_BYTES.saturating_sub(self.path_bytes);
        let children = enumerate_directory(
            path,
            parent_zip_name,
            remaining_entries,
            remaining_path_bytes,
        )
        .await?;
        let mut snapshots = Vec::with_capacity(children.len());
        let mut pending = Vec::with_capacity(children.len());
        for (name, child_path, kind, child_fingerprint) in children {
            let zip_name = zip_name_for_child(parent_zip_name, &name, kind);
            let depth = zip_name.trim_end_matches('/').split('/').count();
            self.reserve_entry(&child_path, &zip_name)?;
            snapshots.push(ManifestChild {
                name,
                kind,
                fingerprint: child_fingerprint,
            });
            pending.push(PendingNode {
                path: child_path,
                zip_name,
                kind,
                depth,
                reserved: true,
            });
        }
        Ok((snapshots, pending))
    }
}

type RawDirectoryChild = (String, PathBuf, ManifestEntryKind, NodeFingerprint);

async fn enumerate_directory(
    path: &Path,
    parent_zip_name: &str,
    max_entries: usize,
    max_path_bytes: usize,
) -> Result<Vec<RawDirectoryChild>, ManifestError> {
    let mut directory = fs::read_dir(path)
        .await
        .map_err(|error| ManifestError::io("read manifest directory", error))?;
    let mut children = Vec::new();
    let mut path_bytes = 0usize;
    while let Some(entry) = directory
        .next_entry()
        .await
        .map_err(|error| ManifestError::io("read manifest directory entry", error))?
    {
        let child_path = entry.path();
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| ManifestError::InvalidName {
            path: child_path.clone(),
            detail: "entry name is not valid UTF-8".to_owned(),
        })?;
        validate_zip_component(name).map_err(|detail| ManifestError::InvalidName {
            path: child_path.clone(),
            detail,
        })?;
        let metadata = fs::symlink_metadata(&child_path)
            .await
            .map_err(|error| ManifestError::io("stat manifest child", error))?;
        let kind = node_kind(&metadata, &child_path)?;
        if children.len() >= max_entries {
            return Err(ManifestError::TooManyEntries {
                limit: MAX_MANIFEST_ENTRIES,
            });
        }
        let zip_name = zip_name_for_child(parent_zip_name, name, kind);
        let bytes = source_path_bytes(&child_path)
            .checked_add(zip_name.len())
            .ok_or(ManifestError::TooManyPathBytes {
                limit: MAX_MANIFEST_PATH_BYTES,
            })?;
        let next_path_bytes =
            path_bytes
                .checked_add(bytes)
                .ok_or(ManifestError::TooManyPathBytes {
                    limit: MAX_MANIFEST_PATH_BYTES,
                })?;
        if next_path_bytes > max_path_bytes {
            return Err(ManifestError::TooManyPathBytes {
                limit: MAX_MANIFEST_PATH_BYTES,
            });
        }
        path_bytes = next_path_bytes;
        children.push((
            name.to_owned(),
            child_path,
            kind,
            fingerprint(&metadata, kind),
        ));
    }
    children.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(children)
}

fn zip_name_for_child(parent_zip_name: &str, name: &str, kind: ManifestEntryKind) -> String {
    if kind == ManifestEntryKind::Directory {
        format!("{parent_zip_name}{name}/")
    } else {
        format!("{parent_zip_name}{name}")
    }
}

async fn validate_entry(entry: &ManifestEntry) -> Result<(), ManifestError> {
    let metadata = fs::symlink_metadata(&entry.source_path)
        .await
        .map_err(|error| ManifestError::io("stat source during validation", error))?;
    let kind = node_kind(&metadata, &entry.source_path)?;
    if kind != entry.kind || fingerprint(&metadata, kind) != entry.fingerprint {
        return Err(ManifestError::Changed {
            path: entry.source_path.clone(),
        });
    }
    if kind == ManifestEntryKind::Directory {
        let children = enumerate_directory(
            &entry.source_path,
            &entry.zip_name,
            MAX_MANIFEST_ENTRIES,
            MAX_MANIFEST_PATH_BYTES,
        )
        .await?;
        let children = children
            .into_iter()
            .map(|(name, _path, kind, fingerprint)| ManifestChild {
                name,
                kind,
                fingerprint,
            })
            .collect::<Vec<_>>();
        let expected = entry
            .directory
            .as_ref()
            .ok_or_else(|| ManifestError::Changed {
                path: entry.source_path.clone(),
            })?;
        if children != expected.children {
            return Err(ManifestError::Changed {
                path: entry.source_path.clone(),
            });
        }
    }
    Ok(())
}

fn node_kind(
    metadata: &std::fs::Metadata,
    path: &Path,
) -> Result<ManifestEntryKind, ManifestError> {
    if metadata.file_type().is_symlink() {
        return Err(ManifestError::Symlink {
            path: path.to_owned(),
        });
    }
    if metadata.is_dir() {
        Ok(ManifestEntryKind::Directory)
    } else if metadata.is_file() {
        Ok(ManifestEntryKind::File)
    } else {
        Err(ManifestError::Special {
            path: path.to_owned(),
        })
    }
}

pub(crate) fn fingerprint(
    metadata: &std::fs::Metadata,
    kind: ManifestEntryKind,
) -> NodeFingerprint {
    #[cfg(unix)]
    let (device, inode, changed) = {
        use std::os::unix::fs::MetadataExt;
        (
            Some(metadata.dev()),
            Some(metadata.ino()),
            Some((metadata.ctime(), metadata.ctime_nsec() as u32)),
        )
    };
    #[cfg(not(unix))]
    let (device, inode, changed) = (None, None, None);
    NodeFingerprint {
        kind,
        size: metadata.len(),
        modified: metadata.modified().ok(),
        device,
        inode,
        changed,
    }
}

fn validate_zip_component(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." {
        return Err("empty, . and .. components are not allowed".to_owned());
    }
    if name.chars().any(|character| character.is_control()) {
        return Err("control characters are not allowed".to_owned());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("path separators are not allowed in one component".to_owned());
    }
    Ok(())
}

fn lexical_clean(path: PathBuf) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push(component.as_os_str());
                }
            }
            Component::RootDir | Component::Prefix(_) => result.push(component.as_os_str()),
            Component::Normal(value) => result.push(value),
        }
    }
    if result.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        result
    }
}

fn source_path_bytes(path: &Path) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().len()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn directory_enumeration_rejects_entry_budget_before_collecting() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("payload"), b"payload")
            .await
            .expect("create child");

        let error = enumerate_directory(directory.path(), "root/", 0, MAX_MANIFEST_PATH_BYTES)
            .await
            .expect_err("zero entry budget must reject the child");
        assert!(matches!(error, ManifestError::TooManyEntries { .. }));
    }

    #[tokio::test]
    async fn directory_enumeration_rejects_path_budget_before_collecting() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("payload"), b"payload")
            .await
            .expect("create child");

        let error = enumerate_directory(directory.path(), "root/", 1, 0)
            .await
            .expect_err("zero path budget must reject the child");
        assert!(matches!(error, ManifestError::TooManyPathBytes { .. }));
    }
}
