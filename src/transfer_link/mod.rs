//! Building blocks for the HTTP transfer-link feature.
//!
//! The module is intentionally independent from the command line and vhost
//! wiring.  The later phases add the bounded producer and HTTP listener while
//! keeping the validation and public data contracts in this module.

mod http;
mod manifest;
mod oneshot;
mod path;
mod source;
mod stats;
mod zip;

#[allow(unused_imports)]
pub(crate) use path::BackendPathRegistry;

pub use http::{
    body_from_messages, HttpServerError, TransferBody, TransferLinkHttp, CONNECTION_HEADROOM,
    HEADER_READ_TIMEOUT, MAX_HEADERS, MAX_HTTP_BUFFER,
};

pub use manifest::{
    prepare_selection, DirectorySnapshot, ManifestChild, ManifestEntry, ManifestEntryKind,
    ManifestError, NodeFingerprint, PreparedArchive, PreparedSource, SourceManifest,
    MAX_MANIFEST_DEPTH, MAX_MANIFEST_ENTRIES, MAX_MANIFEST_PATH_BYTES,
};
pub use oneshot::{prepare_exec, prepare_stdin, PreparedStream};
pub use source::{
    content_disposition, encode_path_segment, filename_from_path, validate_filename,
    validate_limits, FilenameError, LinkConfigError, LinkLimits, LinkOptions, PreparedFile,
    SourceFingerprint, SourceKind, DEFAULT_CHUNK_SIZE, DEFAULT_MAX_DOWNLOADS,
    DEFAULT_QUEUE_CAPACITY, DEFAULT_STATS_INTERVAL, MAX_DOWNLOADS, MAX_FILENAME_BYTES,
    MAX_STATS_INTERVAL_SECS, MIME_OCTET_STREAM, MIN_STATS_INTERVAL_SECS,
};
pub use source::{
    open_prepared_file, prepare_file, spawn_file_producer, spawn_file_producer_with_options,
    FileProducerHandle, FileProducerOptions, SourceError, ValidationGate,
};
pub use stats::{
    DownloadId, DownloadOutcome, DownloadResult, DownloadState, SourceCompletion, SourceFailure,
    SourceMessage,
};
