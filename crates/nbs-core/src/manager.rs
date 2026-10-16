use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use reqwest::Client;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::engine::{self, now_millis, temp_dir};
use crate::error::{Error, Result};
use crate::limiter::RateLimiter;
use crate::naming::{file_name_from_url, sanitize_file_name};
use crate::probe::probe;
use crate::store::Store;
use crate::types::{
    DownloadRecord, DownloadView, EngineConfig, Event, NewDownload, ProbeResult, SegmentView, Status, MAX_CONNECTIONS,
};

pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

const TICK: Duration = Duration::from_millis(500);

pub(crate) struct TaskControl {
    pub gen: u64,
    pub cancel: CancellationToken,
    pub handle: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub(crate) struct SpeedMeter {
    speed: f64,
    last: Option<Instant>,
}

impl SpeedMeter {
    fn sample(&mut self, bytes: u64) {
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64()).unwrap_or(TICK.as_secs_f64()).max(0.05);
        self.last = Some(now);
        let instant = bytes as f64 / dt;
        self.speed = if self.speed == 0.0 { instant } else { 0.3 * instant + 0.7 * self.speed };
        if self.speed < 1.0 {
            self.speed = 0.0;
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn get(&self) -> u64 {
        self.speed as u64
    }
}

/// Runtime state of one download.
pub(crate) struct Handle {
    pub record: Mutex<DownloadRecord>,
    pub limiter: RateLimiter,
    /// Bytes received since the last progress tick.
    pub counter: AtomicU64,
    pub active_segments: Mutex<HashSet<u32>>,
    pub control: Mutex<Option<TaskControl>>,
    pub speed: Mutex<SpeedMeter>,
    /// Signals the task to re-check its connection count.
    pub rebalance: Notify,
}

impl Handle {
    fn new(record: DownloadRecord) -> Arc<Self> {
        Arc::new(Self {
            limiter: RateLimiter::new(record.speed_limit),
            record: Mutex::new(record),
            counter: AtomicU64::new(0),
            active_segments: Mutex::new(HashSet::new()),
            control: Mutex::new(None),
            speed: Mutex::new(SpeedMeter::default()),
            rebalance: Notify::new(),
        })
    }

    fn view(&self) -> DownloadView {
        let rec = self.record.lock();
        let active = self.active_segments.lock();
        let speed = self.speed.lock().get();
        let downloaded = rec.downloaded();
        let eta = match (rec.total_size, speed) {
            (Some(total), s) if s > 0 && rec.status == Status::Downloading => Some(total.saturating_sub(downloaded) / s),
            _ => None,
        };
        DownloadView {
            id: rec.id.clone(),
            url: rec.url.clone(),
            file_name: rec.file_name.clone(),
            save_dir: rec.save_dir.clone(),
            final_path: rec.final_path.clone(),
            total_size: rec.total_size,
            downloaded,
            resumable: rec.resumable,
            connections: rec.connections,
            active_connections: active.len(),
            speed_limit: rec.speed_limit,
            status: rec.status,
            error: rec.error.clone(),
            segments: rec
                .segments
                .iter()
                .map(|s| SegmentView {
                    id: s.id,
                    start: s.start,
                    len: s.len,
                    downloaded: s.downloaded,
                    merged: s.merged,
                    active: active.contains(&s.id),
                })
                .collect(),
            speed,
            eta,
            content_type: rec.content_type.clone(),
            source: rec.source.clone(),
            created_at: rec.created_at,
            completed_at: rec.completed_at,
        }
    }
}

pub(crate) struct Core {
    pub client: Client,
    rt: tokio::runtime::Handle,
    downloads: RwLock<Vec<Arc<Handle>>>,
    pub global: RateLimiter,
    pub config: RwLock<EngineConfig>,
    store: Store,
    sink: EventSink,
    gen: AtomicU64,
}

impl Core {
    fn find(&self, id: &str) -> Result<Arc<Handle>> {
        self.downloads
            .read()
            .iter()
            .find(|h| h.record.lock().id == id)
            .cloned()
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    pub fn persist(&self) {
        let records: Vec<DownloadRecord> = self.downloads.read().iter().map(|h| h.record.lock().clone()).collect();
        if let Err(e) = self.store.save(&records) {
            eprintln!("nbs-core: failed to save downloads: {e}");
        }
    }

    pub fn emit_update(&self, h: &Handle) {
        (self.sink)(Event::Updated(h.view()));
    }

    pub fn emit_completed(&self, h: &Handle) {
        (self.sink)(Event::Completed(h.view()));
    }

    /// Starts queued downloads while there are free slots.
    pub fn schedule(self: &Arc<Self>) {
        let max = self.config.read().max_concurrent.max(1);
        let list = self.downloads.read().clone();
        let mut running = list.iter().filter(|h| h.record.lock().status.is_active()).count();
        for h in list {
            if running >= max {
                break;
            }
            if h.record.lock().status == Status::Queued {
                self.launch(h);
                running += 1;
            }
        }
    }

    fn launch(self: &Arc<Self>, h: Arc<Handle>) {
        let gen = self.gen.fetch_add(1, Ordering::Relaxed) + 1;
        let cancel = CancellationToken::new();
        let previous = h.control.lock().replace(TaskControl { gen, cancel: cancel.clone(), handle: None });
        if let Some(prev) = &previous {
            prev.cancel.cancel();
        }
        h.record.lock().status = Status::Connecting;
        self.emit_update(&h);

        let core = self.clone();
        let task_h = h.clone();
        let join = self.rt.spawn(async move {
            // Let a task that is still shutting down flush its state first.
            if let Some(handle) = previous.and_then(|p| p.handle) {
                let _ = handle.await;
            }
            engine::run(core, task_h, cancel, gen).await;
        });
        if let Some(ctl) = h.control.lock().as_mut().filter(|c| c.gen == gen) {
            ctl.handle = Some(join);
        }
    }
}

/// Owns all downloads and runs them on the Tokio runtime it was created in.
#[derive(Clone)]
pub struct DownloadManager {
    core: Arc<Core>,
}

impl DownloadManager {
    /// Must be called from within a Tokio runtime.
    pub fn new(state_file: PathBuf, config: EngineConfig, global_limit: Option<u64>, sink: EventSink) -> Result<Self> {
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Invalid("DownloadManager must be created inside a Tokio runtime".into()))?;
        let client = Client::builder()
            .user_agent(config.user_agent.clone())
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(MAX_CONNECTIONS as usize * 2)
            .build()?;
        let store = Store::new(state_file);
        let downloads = store.load().into_iter().map(Handle::new).collect();
        Ok(Self {
            core: Arc::new(Core {
                client,
                rt,
                downloads: RwLock::new(downloads),
                global: RateLimiter::new(global_limit),
                config: RwLock::new(config),
                store,
                sink,
                gen: AtomicU64::new(0),
            }),
        })
    }

    pub fn list(&self) -> Vec<DownloadView> {
        self.core.downloads.read().iter().map(|h| h.view()).collect()
    }

    pub fn get(&self, id: &str) -> Result<DownloadView> {
        Ok(self.core.find(id)?.view())
    }

    pub fn config(&self) -> EngineConfig {
        self.core.config.read().clone()
    }

    pub fn update_config(&self, f: impl FnOnce(&mut EngineConfig)) {
        f(&mut self.core.config.write());
        self.core.schedule();
    }

    pub async fn probe(&self, url: &str, headers: &[(String, String)]) -> Result<ProbeResult> {
        probe(&self.core.client, url.trim(), headers).await
    }

    pub fn add(&self, req: NewDownload) -> Result<DownloadView> {
        let url = req.url.trim().to_string();
        let parsed = reqwest::Url::parse(&url).map_err(|e| Error::Invalid(format!("{url}: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(Error::Invalid(format!("unsupported URL scheme: {}", parsed.scheme())));
        }
        let config = self.config();
        let (file_name, auto_name) = match req.file_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            Some(name) => (sanitize_file_name(name), false),
            None => (sanitize_file_name(&file_name_from_url(&url).unwrap_or_else(|| "download".into())), true),
        };
        let id = uuid::Uuid::new_v4().simple().to_string();
        let record = DownloadRecord {
            id,
            url,
            file_name,
            auto_name,
            save_dir: req.save_dir.filter(|d| !d.as_os_str().is_empty()).unwrap_or(config.default_dir),
            final_path: None,
            total_size: None,
            resumable: false,
            connections: req.connections.unwrap_or(config.default_connections).clamp(1, MAX_CONNECTIONS),
            speed_limit: req.speed_limit.filter(|l| *l > 0),
            status: if req.start { Status::Queued } else { Status::Paused },
            error: None,
            segments: Vec::new(),
            merged_bytes: 0,
            next_segment_id: 0,
            headers: req.headers,
            etag: None,
            last_modified: None,
            content_type: None,
            source: req.source,
            created_at: now_millis(),
            completed_at: None,
        };
        let h = Handle::new(record);
        self.core.downloads.write().push(h.clone());
        self.core.persist();
        self.core.emit_update(&h);
        self.core.schedule();
        Ok(h.view())
    }

    /// Queues a paused or failed download; it starts when a slot is free.
    pub fn resume(&self, id: &str) -> Result<()> {
        let h = self.core.find(id)?;
        {
            let mut rec = h.record.lock();
            if !matches!(rec.status, Status::Paused | Status::Failed) {
                return Ok(());
            }
            rec.status = Status::Queued;
            rec.error = None;
        }
        self.core.emit_update(&h);
        self.core.persist();
        self.core.schedule();
        Ok(())
    }

    pub fn pause(&self, id: &str) -> Result<()> {
        let h = self.core.find(id)?;
        {
            let mut rec = h.record.lock();
            if !(rec.status.is_active() || rec.status == Status::Queued) {
                return Ok(());
            }
            rec.status = Status::Paused;
        }
        if let Some(ctl) = h.control.lock().as_ref() {
            ctl.cancel.cancel();
        }
        h.speed.lock().reset();
        self.core.emit_update(&h);
        self.core.persist();
        self.core.schedule();
        Ok(())
    }

    pub fn pause_all(&self) {
        for view in self.list() {
            let _ = self.pause(&view.id);
        }
    }

    pub fn resume_all(&self) {
        for view in self.list() {
            if view.status == Status::Paused {
                let _ = self.resume(&view.id);
            }
        }
    }

    /// Discards all progress and downloads the file again.
    pub async fn restart(&self, id: &str) -> Result<()> {
        let h = self.core.find(id)?;
        self.stop_task(&h).await;
        let temp = {
            let mut rec = h.record.lock();
            rec.segments.clear();
            rec.merged_bytes = 0;
            rec.next_segment_id = 0;
            rec.final_path = None;
            rec.completed_at = None;
            rec.error = None;
            rec.status = Status::Queued;
            temp_dir(&rec)
        };
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(temp)).await;
        self.core.emit_update(&h);
        self.core.persist();
        self.core.schedule();
        Ok(())
    }

    async fn stop_task(&self, h: &Handle) {
        let control = h.control.lock().take();
        if let Some(ctl) = control {
            ctl.cancel.cancel();
            if let Some(handle) = ctl.handle {
                let _ = handle.await;
            }
        }
        h.active_segments.lock().clear();
        h.speed.lock().reset();
    }

    /// Removes a download from the list, deleting its temporary files and,
    /// when `delete_file` is set, the downloaded file itself.
    pub async fn remove(&self, id: &str, delete_file: bool) -> Result<()> {
        let h = self.core.find(id)?;
        self.core.downloads.write().retain(|d| !Arc::ptr_eq(d, &h));
        self.stop_task(&h).await;
        let (temp, final_path) = {
            let rec = h.record.lock();
            (temp_dir(&rec), rec.final_path.clone())
        };
        let _ = tokio::task::spawn_blocking(move || {
            let _ = std::fs::remove_dir_all(temp);
            if let (true, Some(path)) = (delete_file, final_path) {
                let _ = std::fs::remove_file(path);
            }
        })
        .await;
        self.core.persist();
        (self.core.sink)(Event::Removed(id.to_string()));
        self.core.schedule();
        Ok(())
    }

    /// Removes all completed downloads from the list (files are kept).
    pub fn clear_completed(&self) {
        let removed: Vec<String> = {
            let mut list = self.core.downloads.write();
            let ids = list
                .iter()
                .filter(|h| h.record.lock().status == Status::Completed)
                .map(|h| h.record.lock().id.clone())
                .collect();
            list.retain(|h| h.record.lock().status != Status::Completed);
            ids
        };
        self.core.persist();
        for id in removed {
            (self.core.sink)(Event::Removed(id));
        }
    }

    /// Per-download speed limit in bytes per second (`None` = unlimited).
    pub fn set_speed_limit(&self, id: &str, limit: Option<u64>) -> Result<()> {
        let h = self.core.find(id)?;
        let limit = limit.filter(|l| *l > 0);
        h.record.lock().speed_limit = limit;
        h.limiter.set_rate(limit);
        self.core.emit_update(&h);
        self.core.persist();
        Ok(())
    }

    /// Changes the number of connections; a running download picks up an
    /// increase immediately, a decrease as its connections finish.
    pub fn set_connections(&self, id: &str, connections: u8) -> Result<()> {
        let h = self.core.find(id)?;
        h.record.lock().connections = connections.clamp(1, MAX_CONNECTIONS);
        h.rebalance.notify_one();
        self.core.emit_update(&h);
        self.core.persist();
        Ok(())
    }

    /// Total speed limit shared by all downloads (`None` = unlimited).
    pub fn set_global_speed_limit(&self, limit: Option<u64>) {
        self.core.global.set_rate(limit);
    }

    pub fn global_speed_limit(&self) -> Option<u64> {
        self.core.global.rate()
    }

    pub fn persist(&self) {
        self.core.persist();
    }

    /// Emits progress for running downloads twice a second and saves state
    /// periodically. Runs forever; spawn it once.
    pub async fn run_ticker(&self) {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ticks = 0u64;
        loop {
            interval.tick().await;
            ticks += 1;
            let list = self.core.downloads.read().clone();
            let mut views = Vec::new();
            for h in &list {
                let active = h.record.lock().status.is_active();
                let bytes = h.counter.swap(0, Ordering::Relaxed);
                if active {
                    h.speed.lock().sample(bytes);
                    views.push(h.view());
                }
            }
            if !views.is_empty() {
                (self.core.sink)(Event::Progress(views));
                if ticks % 4 == 0 {
                    self.core.persist();
                }
            }
        }
    }
}
