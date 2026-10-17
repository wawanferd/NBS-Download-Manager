use std::path::PathBuf;
use std::sync::Arc;

use nbs_core::Status;
use serde::{Deserialize, Serialize};

/// What happens after a torrent has finished downloading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UploadMode {
    /// Keep seeding.
    #[default]
    Normal,
    /// Pause torrents once they are complete. Finished files still stream.
    StopWhenDone,
    /// Never send data to peers. Applies to the whole session, so it only
    /// changes when the engine starts.
    Disabled,
}

#[derive(Debug, Clone)]
pub struct TorrentConfig {
    /// Session state, DHT cache and torrent metadata.
    pub data_dir: PathBuf,
    /// Default folder for "Download" mode.
    pub download_dir: PathBuf,
    /// Folder for "Stream" mode.
    pub cache_dir: PathBuf,
    /// BitTorrent listen port; `None` picks a random one.
    pub listen_port: Option<u16>,
    /// Bytes per second; `None` = unlimited.
    pub download_limit: Option<u64>,
    pub upload_limit: Option<u64>,
    pub upload_mode: UploadMode,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineInfo {
    /// Port of the local streaming server on 127.0.0.1.
    pub http_port: u16,
    pub listen_port: Option<u16>,
    pub upload_disabled: bool,
}

/// Where a torrent comes from.
#[derive(Debug, Clone)]
pub enum TorrentSource {
    /// Magnet link, info-hash or http(s) URL of a .torrent file.
    Link(String),
    /// Contents of a .torrent file.
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Video,
    Audio,
    Subtitle,
    Other,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TorrentFile {
    pub index: usize,
    /// Path inside the torrent, `/` separated.
    pub path: String,
    /// Last path component.
    pub name: String,
    pub size: u64,
    pub kind: FileKind,
    pub included: bool,
    pub downloaded: u64,
}

/// Metadata of a torrent that has not been added yet.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TorrentPreview {
    /// Info-hash; pass it to [`crate::TorrentEngine::add`].
    pub id: String,
    pub name: String,
    pub total_size: u64,
    pub files: Vec<TorrentFile>,
    /// The torrent is already in the list.
    pub existing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AddMode {
    /// Into the stream cache; subtitles are always included.
    Stream,
    Download,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddTorrent {
    /// [`TorrentPreview::id`].
    pub id: String,
    pub mode: AddMode,
    /// File indexes to download. Empty in stream mode picks the largest video.
    #[serde(default)]
    pub files: Vec<usize>,
    pub save_dir: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub start: bool,
    pub source: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Snapshot of a torrent sent to the UI. Shares its field names with
/// `nbs_core::DownloadView` so both kinds fit in the same list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TorrentView {
    /// Info-hash.
    pub id: String,
    /// Always `"torrent"`.
    pub kind: &'static str,
    /// Magnet link.
    pub url: String,
    pub file_name: String,
    pub save_dir: PathBuf,
    /// The file on disk when exactly one file is selected.
    pub final_path: Option<PathBuf>,
    /// Size of the selected files.
    pub total_size: Option<u64>,
    pub downloaded: u64,
    pub status: Status,
    pub error: Option<String>,
    /// Bytes per second.
    pub speed: u64,
    pub eta: Option<u64>,
    pub upload_speed: u64,
    pub uploaded: u64,
    pub peers: u32,
    /// Complete and still uploading to peers.
    pub seeding: bool,
    /// Added in stream mode (files live in the stream cache).
    pub stream: bool,
    pub files: Vec<TorrentFile>,
    pub source: Option<String>,
    pub created_at: u64,
    pub completed_at: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum TorrentEvent {
    Updated(TorrentView),
    /// Every torrent, once per second.
    Progress(Vec<TorrentView>),
    /// A torrent finished downloading (sent once).
    Completed(TorrentView),
    Removed(String),
}

pub type TorrentSink = Arc<dyn Fn(TorrentEvent) + Send + Sync>;
