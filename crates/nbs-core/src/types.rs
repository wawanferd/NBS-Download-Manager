use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Connecting,
    Downloading,
    Paused,
    Merging,
    Completed,
    Failed,
}

impl Status {
    /// A task is currently attached to the download.
    pub fn is_active(self) -> bool {
        matches!(self, Status::Connecting | Status::Downloading | Status::Merging)
    }
}

/// A contiguous byte range of the remote file, downloaded by one connection
/// into its own part file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub id: u32,
    pub start: u64,
    /// Length in bytes; `None` while the total size is unknown.
    pub len: Option<u64>,
    pub downloaded: u64,
    /// The part file has already been appended to the output file.
    #[serde(default)]
    pub merged: bool,
}

impl Segment {
    pub fn remaining(&self) -> Option<u64> {
        self.len.map(|l| l.saturating_sub(self.downloaded))
    }

    pub fn is_complete(&self) -> bool {
        self.remaining() == Some(0)
    }

    pub fn end_exclusive(&self) -> Option<u64> {
        self.len.map(|l| self.start + l)
    }
}

/// Persistent state of a download.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRecord {
    pub id: String,
    pub url: String,
    pub file_name: String,
    /// The file name was derived automatically and may be replaced by the
    /// server-provided one (Content-Disposition) when the download starts.
    #[serde(default)]
    pub auto_name: bool,
    pub save_dir: PathBuf,
    pub final_path: Option<PathBuf>,
    pub total_size: Option<u64>,
    pub resumable: bool,
    pub connections: u8,
    pub speed_limit: Option<u64>,
    pub status: Status,
    pub error: Option<String>,
    pub segments: Vec<Segment>,
    pub merged_bytes: u64,
    pub next_segment_id: u32,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub source: Option<String>,
    pub created_at: u64,
    pub completed_at: Option<u64>,
}

impl DownloadRecord {
    pub fn downloaded(&self) -> u64 {
        self.segments.iter().map(|s| s.downloaded).sum()
    }
}

/// Request for a new download.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewDownload {
    pub url: String,
    pub file_name: Option<String>,
    pub save_dir: Option<PathBuf>,
    pub connections: Option<u8>,
    /// Bytes per second; `None` or `0` means unlimited.
    pub speed_limit: Option<u64>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Start right away (or queue) instead of adding in the paused state.
    #[serde(default = "default_true")]
    pub start: bool,
    pub source: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentView {
    pub id: u32,
    pub start: u64,
    pub len: Option<u64>,
    pub downloaded: u64,
    pub merged: bool,
    pub active: bool,
}

/// Snapshot of a download sent to the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadView {
    pub id: String,
    pub url: String,
    pub file_name: String,
    pub save_dir: PathBuf,
    pub final_path: Option<PathBuf>,
    pub total_size: Option<u64>,
    pub downloaded: u64,
    pub resumable: bool,
    pub connections: u8,
    pub active_connections: usize,
    pub speed_limit: Option<u64>,
    pub status: Status,
    pub error: Option<String>,
    pub segments: Vec<SegmentView>,
    /// Bytes per second, smoothed.
    pub speed: u64,
    /// Estimated seconds remaining.
    pub eta: Option<u64>,
    pub content_type: Option<String>,
    pub source: Option<String>,
    pub created_at: u64,
    pub completed_at: Option<u64>,
}

/// Result of inspecting a URL before downloading it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub url: String,
    pub final_url: String,
    pub file_name: String,
    pub total_size: Option<u64>,
    pub resumable: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Metadata or status of a download changed.
    Updated(DownloadView),
    /// Periodic progress of all running downloads.
    Progress(Vec<DownloadView>),
    /// A download finished successfully (sent once).
    Completed(DownloadView),
    Removed(String),
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub default_dir: PathBuf,
    pub default_connections: u8,
    pub max_concurrent: usize,
    pub max_retries: u32,
    /// Segments are never planned or split below this size.
    pub min_segment_size: u64,
    pub user_agent: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            default_dir: std::env::temp_dir(),
            default_connections: 4,
            max_concurrent: 3,
            max_retries: 5,
            min_segment_size: 256 * 1024,
            user_agent: concat!("NBS-Download-Manager/", env!("CARGO_PKG_VERSION")).to_string(),
        }
    }
}

pub const MAX_CONNECTIONS: u8 = 8;
