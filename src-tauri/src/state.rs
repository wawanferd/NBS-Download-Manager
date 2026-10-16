use std::path::PathBuf;
use std::sync::Arc;

use nbs_core::DownloadManager;
use parking_lot::Mutex;

use crate::bridge::Bridge;
use crate::settings::Settings;

pub struct AppState {
    pub manager: DownloadManager,
    pub settings: Mutex<Settings>,
    pub settings_path: PathBuf,
    pub bridge: Arc<Bridge>,
}
