use std::fs;
use std::io::Write;
use std::path::PathBuf;

use parking_lot::Mutex;

use crate::error::Result;
use crate::types::{DownloadRecord, Status};

/// Persists the download list as JSON, written atomically.
pub struct Store {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    pub fn load(&self) -> Vec<DownloadRecord> {
        let Ok(data) = fs::read(&self.path) else { return Vec::new() };
        let mut records: Vec<DownloadRecord> = serde_json::from_slice(&data).unwrap_or_default();
        for r in &mut records {
            // Whatever was running when the app exited is resumable later.
            if r.status.is_active() || r.status == Status::Queued {
                r.status = Status::Paused;
            }
        }
        records
    }

    pub fn save(&self, records: &[DownloadRecord]) -> Result<()> {
        let _guard = self.lock.lock();
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(records).expect("records serialize"))?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}
