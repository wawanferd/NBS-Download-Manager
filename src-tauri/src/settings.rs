use std::fs;
use std::path::{Path, PathBuf};

use nbs_core::{EngineConfig, MAX_CONNECTIONS};
use nbs_torrent::UploadMode;
use serde::{Deserialize, Serialize};

pub const DEFAULT_BRIDGE_PORT: u16 = 18765;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub download_dir: PathBuf,
    pub default_connections: u8,
    pub max_concurrent: usize,
    /// Bytes per second shared by all downloads; `None` = unlimited.
    pub global_speed_limit: Option<u64>,
    /// Port of the local WebSocket server used by the browser extension.
    pub bridge_port: u16,
    /// Start downloads captured from the browser without asking.
    pub auto_start_browser: bool,
    pub close_to_tray: bool,
    pub notify_on_complete: bool,
    /// "system", "light" or "dark".
    pub theme: String,
    /// Default folder for torrents; empty = `download_dir`.
    pub torrent_dir: PathBuf,
    /// Where streamed torrents are kept; empty = the app's cache folder.
    /// Applies after a restart.
    pub stream_dir: PathBuf,
    /// BitTorrent listen port; 0 = random. Applies after a restart.
    pub torrent_port: u16,
    /// Bytes per second for all torrents; `None` = unlimited.
    pub torrent_download_limit: Option<u64>,
    pub torrent_upload_limit: Option<u64>,
    pub torrent_upload_mode: UploadMode,
    /// Media player for "Open in player"; empty = the system default.
    pub external_player: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: PathBuf::new(),
            default_connections: 8,
            max_concurrent: 3,
            global_speed_limit: None,
            bridge_port: DEFAULT_BRIDGE_PORT,
            auto_start_browser: false,
            close_to_tray: true,
            notify_on_complete: true,
            theme: "dark".into(),
            torrent_dir: PathBuf::new(),
            stream_dir: PathBuf::new(),
            torrent_port: 0,
            torrent_download_limit: None,
            torrent_upload_limit: None,
            torrent_upload_mode: UploadMode::Normal,
            external_player: String::new(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|data| serde_json::from_slice::<Settings>(&data).ok())
            .unwrap_or_default()
            .normalized()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self).expect("settings serialize"))?;
        fs::rename(tmp, path)
    }

    /// Clamps values coming from disk or the UI into their valid ranges.
    pub fn normalized(mut self) -> Self {
        self.default_connections = self.default_connections.clamp(1, MAX_CONNECTIONS);
        self.max_concurrent = self.max_concurrent.clamp(1, 16);
        self.global_speed_limit = self.global_speed_limit.filter(|l| *l > 0);
        if self.bridge_port < 1024 {
            self.bridge_port = DEFAULT_BRIDGE_PORT;
        }
        if !matches!(self.theme.as_str(), "system" | "light" | "dark") {
            self.theme = "system".into();
        }
        self.torrent_download_limit = self.torrent_download_limit.filter(|l| *l > 0);
        self.torrent_upload_limit = self.torrent_upload_limit.filter(|l| *l > 0);
        self
    }

    pub fn torrent_download_dir(&self) -> PathBuf {
        if self.torrent_dir.as_os_str().is_empty() {
            self.download_dir.clone()
        } else {
            self.torrent_dir.clone()
        }
    }

    pub fn apply_to(&self, config: &mut EngineConfig) {
        config.default_dir = self.download_dir.clone();
        config.default_connections = self.default_connections;
        config.max_concurrent = self.max_concurrent;
    }
}
