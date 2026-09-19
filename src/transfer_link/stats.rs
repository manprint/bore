//! Download identifiers and terminal outcomes.

use std::time::Duration;

use bytes::Bytes;

/// Monotonic identifier assigned to a download within one link session.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DownloadId(pub u64);

/// Terminal state of a download attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadState {
    /// The source reached EOF and passed all final validation checks.
    Completed,
    /// A source, protocol or transport error stopped the attempt.
    Failed,
    /// The session was cancelled before completion.
    Cancelled,
}

/// A terminal download report used by logs and statistics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadOutcome {
    /// Download identifier.
    pub id: DownloadId,
    /// Terminal state.
    pub state: DownloadState,
    /// Bytes made available to the HTTP body before termination.
    pub bytes_for_http: u64,
    /// Total elapsed time for the attempt.
    pub elapsed: Duration,
    /// SHA-256 of the complete validated source, when available.
    pub sha256: Option<[u8; 32]>,
    /// Human-readable failure detail, when the state is failed.
    pub error: Option<String>,
}

/// Alias used by producers that return a terminal report.
pub type DownloadResult = DownloadOutcome;

/// Digest and byte count emitted after a source has passed final validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceCompletion {
    /// Number of source bytes read and offered to the HTTP body.
    pub bytes_read: u64,
    /// SHA-256 digest of those bytes in source order.
    pub sha256: [u8; 32],
}

/// Failure emitted by a source producer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFailure {
    /// Number of source bytes read before the failure.
    pub bytes_read: u64,
    /// Stable human-readable diagnostic.
    pub error: String,
}

/// Bounded producer-to-body messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceMessage {
    /// A bounded payload chunk.
    Data(Bytes),
    /// A source that reached EOF and passed all final checks.
    Complete(SourceCompletion),
    /// A source that cannot be considered complete.
    Failed(SourceFailure),
}
