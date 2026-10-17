//! The librqbit session behind NBS's torrent support. Ported from TrntStream's
//! engine (github.com/wawanferd/Torrent-Stream, `rust/src/api/engine.rs`).

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use librqbit::api::{Api, TorrentIdOrHash};
use librqbit::http_api::{HttpApi, HttpApiOptions};
use librqbit::{
    AddTorrentOptions, AddTorrentResponse, ListOnlyResponse, ManagedTorrent, ManagedTorrentState, Session, SessionOptions,
    SessionPersistenceConfig, TorrentStatsState,
};
use nbs_core::naming::sanitize_file_name;
use nbs_core::Status;
use parking_lot::{Mutex, RwLock};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::types::*;
use crate::util::{kind_of, normalize_link, now_millis, to_webvtt};

type Handle = Arc<ManagedTorrent>;

const MAX_TORRENT_FILE: usize = 16 * 1024 * 1024;
const MAX_SUBTITLE_FILE: u64 = 8 * 1024 * 1024;
const INIT_TIMEOUT: Duration = Duration::from_secs(300);

/// What NBS remembers about a torrent besides librqbit's own session state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    created_at: u64,
    completed_at: Option<u64>,
    source: Option<String>,
}

/// Metadata resolved by [`TorrentEngine::inspect`], waiting for [`TorrentEngine::add`].
struct Pending {
    name: String,
    files: Vec<TorrentFile>,
    torrent_bytes: bytes::Bytes,
    seen_peers: Vec<SocketAddr>,
}

pub struct TorrentEngine {
    session: Arc<Session>,
    info: EngineInfo,
    download_dir: RwLock<PathBuf>,
    cache_dir: PathBuf,
    meta_path: PathBuf,
    meta: Mutex<HashMap<String, Meta>>,
    pending: Mutex<HashMap<String, Pending>>,
    /// Magnet links by info-hash (building one parses the .torrent).
    magnets: Mutex<HashMap<String, String>>,
    stop_seeding: AtomicBool,
    client: reqwest::Client,
    sink: TorrentSink,
}

fn limit(bps: Option<u64>) -> Option<NonZeroU32> {
    bps.and_then(|b| NonZeroU32::new(b.min(u32::MAX as u64) as u32))
}

impl TorrentEngine {
    /// Starts the session (restoring the torrents of the last run) and the
    /// local streaming server.
    pub async fn start(config: TorrentConfig, sink: TorrentSink) -> anyhow::Result<Arc<Self>> {
        for dir in [&config.data_dir, &config.download_dir, &config.cache_dir] {
            std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let disable_upload = config.upload_mode == UploadMode::Disabled;
        let make_opts = |ipv4_only: bool| {
            let mut opts = SessionOptions {
                fastresume: true,
                persistence: Some(SessionPersistenceConfig::Json { folder: Some(config.data_dir.join("session")) }),
                ratelimits: librqbit::limits::LimitsConfig {
                    download_bps: limit(config.download_limit),
                    upload_bps: limit(config.upload_limit),
                },
                client_name_and_version: Some(format!("NBS Download Manager {}", env!("CARGO_PKG_VERSION"))),
                disable_upload,
                ipv4_only,
                ..Default::default()
            };
            if let Some(p) = opts.dht.as_mut().and_then(|dht| dht.persistence.as_mut()) {
                p.config_filename = Some(config.data_dir.join("dht.json"));
            }
            let mut listen = librqbit::ListenerOptions { ipv4_only, ..Default::default() };
            let ip = if ipv4_only { Ipv4Addr::UNSPECIFIED.into() } else { listen.listen_addr.ip() };
            listen.listen_addr = SocketAddr::new(ip, config.listen_port.unwrap_or(0));
            opts.listen = Some(listen);
            opts
        };
        let session = match Session::new_with_opts(config.download_dir.clone(), make_opts(false)).await {
            Ok(s) => s,
            // Systems with IPv6 disabled cannot open the default dual-stack socket.
            Err(e) => {
                tracing::warn!("cannot start the torrent session with IPv6, retrying with IPv4 only: {e:#}");
                Session::new_with_opts(config.download_dir.clone(), make_opts(true))
                    .await
                    .context("cannot start the torrent session")?
            }
        };

        // Read-only HTTP API on loopback: players stream files from it with
        // Range requests, and librqbit fetches the pieces around each read
        // position first.
        let listener = librqbit_dualstack_sockets::TcpListener::bind_tcp(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            librqbit_dualstack_sockets::BindOpts { request_dualstack: false, ..Default::default() },
        )
        .context("cannot bind the streaming server")?;
        let http_port = listener.socket().local_addr()?.port();
        let http =
            HttpApi::new(Api::new(session.clone(), None, None), Some(HttpApiOptions { read_only: true, ..Default::default() }));
        tokio::spawn(async move {
            if let Err(e) = http.make_http_api_and_run(listener, None).await {
                tracing::error!("torrent streaming server stopped: {e:#}");
            }
        });

        let meta_path = config.data_dir.join("torrents.json");
        let meta: HashMap<String, Meta> =
            std::fs::read(&meta_path).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default();
        let client = reqwest::Client::builder()
            .user_agent(concat!("NBS-Download-Manager/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(60))
            .build()?;

        let engine = Arc::new(Self {
            info: EngineInfo { http_port, listen_port: session.announce_port(), upload_disabled: disable_upload },
            session,
            download_dir: RwLock::new(config.download_dir),
            cache_dir: config.cache_dir,
            meta_path,
            meta: Mutex::new(meta),
            pending: Mutex::new(HashMap::new()),
            magnets: Mutex::new(HashMap::new()),
            stop_seeding: AtomicBool::new(config.upload_mode == UploadMode::StopWhenDone),
            client,
            sink,
        });
        engine.adopt_restored();
        Ok(engine)
    }

    pub fn info(&self) -> EngineInfo {
        self.info.clone()
    }

    /// Pauses every torrent so the session state on disk is final.
    pub async fn shutdown(&self) {
        self.session.stop().await;
    }

    // -----------------------------------------------------------------------
    // Settings that apply while running
    // -----------------------------------------------------------------------

    pub fn set_rate_limits(&self, download: Option<u64>, upload: Option<u64>) {
        self.session.ratelimits.set_download_bps(limit(download));
        self.session.ratelimits.set_upload_bps(limit(upload));
    }

    /// `Disabled` only takes effect after a restart; until then it behaves
    /// like `Normal`.
    pub fn set_upload_mode(&self, mode: UploadMode) {
        self.stop_seeding.store(mode == UploadMode::StopWhenDone, Ordering::Relaxed);
    }

    /// Default folder for torrents added in download mode from now on.
    pub fn set_download_dir(&self, dir: PathBuf) {
        *self.download_dir.write() = dir;
    }

    // -----------------------------------------------------------------------
    // Adding torrents
    // -----------------------------------------------------------------------

    /// Resolves the metadata (for magnets this asks peers) and returns the file
    /// list without starting anything. `headers` are sent when fetching a
    /// .torrent over HTTP, e.g. cookies captured from the browser.
    pub async fn inspect(
        &self,
        source: TorrentSource,
        headers: &[(String, String)],
        timeout: Duration,
    ) -> anyhow::Result<TorrentPreview> {
        let add = match source {
            TorrentSource::Bytes(bytes) => librqbit::AddTorrent::from_bytes(bytes),
            TorrentSource::Link(link) => {
                let link = normalize_link(&link);
                if link.starts_with("magnet:") {
                    // Torrents already in the list need no metadata round-trip.
                    let hash = librqbit::Magnet::parse(&link).ok().and_then(|m| m.as_id20());
                    if let Some(h) = hash.and_then(|h| self.session.get(TorrentIdOrHash::Hash(h))) {
                        if h.metadata.load().is_some() {
                            return Ok(self.preview_of(&h));
                        }
                    }
                    librqbit::AddTorrent::from_url(link)
                } else if link.starts_with("http://") || link.starts_with("https://") {
                    librqbit::AddTorrent::from_bytes(self.fetch_torrent_file(&link, headers).await?)
                } else {
                    bail!("enter a magnet link, an info-hash or the address of a .torrent file");
                }
            }
        };
        let response = tokio::time::timeout(
            timeout,
            self.session.add_torrent(add, Some(AddTorrentOptions { list_only: true, ..Default::default() })),
        )
        .await
        .context("timed out fetching the torrent metadata (no peers found)")??;

        match response {
            AddTorrentResponse::ListOnly(ListOnlyResponse { info_hash, info, seen_peers, torrent_bytes, .. }) => {
                let id = info_hash.as_string();
                let files = file_entries(&info, None, &[]);
                let name = info.name().map(|n| n.into_owned()).unwrap_or_else(|| id.clone());
                let existing = self.session.get(TorrentIdOrHash::Hash(info_hash)).is_some();
                let preview = TorrentPreview {
                    id: id.clone(),
                    name: name.clone(),
                    total_size: files.iter().map(|f| f.size).sum(),
                    files: files.clone(),
                    existing,
                };
                self.pending.lock().insert(id, Pending { name, files, torrent_bytes, seen_peers });
                Ok(preview)
            }
            AddTorrentResponse::AlreadyManaged(_, h) | AddTorrentResponse::Added(_, h) => Ok(self.preview_of(&h)),
        }
    }

    fn preview_of(&self, h: &Handle) -> TorrentPreview {
        let view = self.view_of(h, &self.meta.lock());
        TorrentPreview {
            id: view.id,
            name: view.file_name,
            total_size: view.files.iter().map(|f| f.size).sum(),
            files: view.files,
            existing: true,
        }
    }

    async fn fetch_torrent_file(&self, url: &str, headers: &[(String, String)]) -> anyhow::Result<Vec<u8>> {
        let mut req = self.client.get(url);
        for (name, value) in headers {
            req = req.header(name.as_str(), value.as_str());
        }
        let mut resp = req.send().await.context("cannot download the .torrent file")?.error_for_status()?;
        if resp.content_length().is_some_and(|l| l > MAX_TORRENT_FILE as u64) {
            bail!("the file is too large to be a .torrent file");
        }
        let mut data = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            data.extend_from_slice(&chunk);
            if data.len() > MAX_TORRENT_FILE {
                bail!("the file is too large to be a .torrent file");
            }
        }
        if !data.starts_with(b"d") {
            bail!("the link does not point to a .torrent file");
        }
        Ok(data)
    }

    /// Starts a torrent returned by [`Self::inspect`].
    pub async fn add(&self, request: AddTorrent) -> anyhow::Result<TorrentView> {
        if let Ok(h) = self.handle(&request.id) {
            return Ok(self.view_of(&h, &self.meta.lock()));
        }
        let pending = self.pending.lock().remove(&request.id).context("the torrent information expired, add it again")?;

        let mut only_files = request.files.clone();
        if request.mode == AddMode::Stream {
            if only_files.is_empty() {
                let video = pending.files.iter().filter(|f| f.kind == FileKind::Video).max_by_key(|f| f.size);
                only_files.extend(video.map(|f| f.index));
            }
            only_files.extend(pending.files.iter().filter(|f| f.kind == FileKind::Subtitle).map(|f| f.index));
        }
        only_files.retain(|i| pending.files.iter().any(|f| f.index == *i));
        only_files.sort_unstable();
        only_files.dedup();
        if only_files.is_empty() {
            self.pending.lock().insert(request.id, pending);
            bail!("select at least one file");
        }

        let base = match (request.save_dir.filter(|d| !d.as_os_str().is_empty()), request.mode) {
            (_, AddMode::Stream) => self.cache_dir.clone(),
            (Some(dir), AddMode::Download) => dir,
            (None, AddMode::Download) => self.download_dir.read().clone(),
        };
        let output_folder = if pending.files.len() > 1 { base.join(sanitize_file_name(&pending.name)) } else { base };

        self.meta
            .lock()
            .insert(request.id.clone(), Meta { created_at: now_millis(), completed_at: None, source: request.source.clone() });
        self.save_meta();
        let response = self
            .session
            .add_torrent(
                librqbit::AddTorrent::from_bytes(pending.torrent_bytes),
                Some(AddTorrentOptions {
                    paused: !request.start,
                    only_files: Some(only_files),
                    output_folder: Some(output_folder.to_string_lossy().into_owned()),
                    overwrite: true,
                    initial_peers: (!pending.seen_peers.is_empty()).then_some(pending.seen_peers),
                    ..Default::default()
                }),
            )
            .await;
        let h = match response {
            Ok(AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h)) => h,
            Ok(AddTorrentResponse::ListOnly(_)) => bail!("unexpected list-only response"),
            Err(e) => {
                self.meta.lock().remove(&request.id);
                self.save_meta();
                return Err(e);
            }
        };
        let view = self.view_of(&h, &self.meta.lock());
        (self.sink)(TorrentEvent::Updated(view.clone()));
        Ok(view)
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    fn handle(&self, id: &str) -> anyhow::Result<Handle> {
        let key = TorrentIdOrHash::parse(id).with_context(|| format!("invalid torrent id {id}"))?;
        self.session.get(key).context("the torrent is no longer in the list")
    }

    fn handles(&self) -> Vec<Handle> {
        self.session.with_torrents(|it| it.map(|(_, h)| h.clone()).collect())
    }

    pub fn list(&self) -> Vec<TorrentView> {
        let meta = self.meta.lock();
        self.handles().iter().map(|h| self.view_of(h, &meta)).collect()
    }

    pub fn get(&self, id: &str) -> anyhow::Result<TorrentView> {
        let h = self.handle(id)?;
        Ok(self.view_of(&h, &self.meta.lock()))
    }

    /// Absolute path of one file of a torrent (it may be incomplete).
    pub fn file_path(&self, id: &str, file: usize) -> anyhow::Result<PathBuf> {
        let view = self.get(id)?;
        let f = view.files.iter().find(|f| f.index == file).context("file not found")?;
        Ok(join_torrent_path(&view.save_dir, &f.path))
    }

    /// Address a player can open while the file downloads; it supports Range
    /// requests, so seeking works. Selects the file and resumes the torrent
    /// if needed, then waits until the torrent finished checking its data.
    pub async fn stream_url(&self, id: &str, file: usize) -> anyhow::Result<String> {
        let h = self.prepare_file(id, file).await?;
        let name = h
            .metadata
            .load()
            .as_ref()
            .and_then(|m| file_entries(&m.info, None, &[]).into_iter().find(|f| f.index == file))
            .map(|f| f.name)
            .unwrap_or_else(|| "file".into());
        Ok(format!(
            "http://127.0.0.1:{}/torrents/{}/stream/{}/{}",
            self.info.http_port,
            h.info_hash().as_string(),
            file,
            utf8_percent_encode(&name, NON_ALPHANUMERIC)
        ))
    }

    /// Reads a subtitle file of the torrent (downloading it first if needed)
    /// and returns it as WebVTT.
    pub async fn subtitle(&self, id: &str, file: usize) -> anyhow::Result<String> {
        let h = self.prepare_file(id, file).await?;
        let mut stream = h.clone().stream(file).await?.take(MAX_SUBTITLE_FILE);
        let mut data = Vec::new();
        tokio::time::timeout(Duration::from_secs(120), stream.read_to_end(&mut data))
            .await
            .context("timed out downloading the subtitle")??;
        Ok(to_webvtt(&String::from_utf8_lossy(&data)))
    }

    /// librqbit's own `is_paused()` flag stays set when a pause requested
    /// during the initial data check is dropped, so look at the state instead.
    fn is_paused(h: &Handle) -> bool {
        h.with_state(|s| matches!(s, ManagedTorrentState::Paused(_)))
    }

    async fn prepare_file(&self, id: &str, file: usize) -> anyhow::Result<Handle> {
        let h = self.handle(id)?;
        if let Some(only) = h.only_files() {
            if !only.contains(&file) {
                let mut set: HashSet<usize> = only.into_iter().collect();
                set.insert(file);
                self.session.update_only_files(&h, &set).await?;
            }
        }
        // Finished torrents stream fine while paused (and stay paused when
        // seeding is off); incomplete ones must run to fetch data.
        if Self::is_paused(&h) && !h.stats().finished {
            self.session.unpause(&h).await?;
        }
        tokio::time::timeout(INIT_TIMEOUT, h.wait_until_initialized()).await.context("timed out preparing the torrent")??;
        Ok(h)
    }

    // -----------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------

    pub async fn pause(&self, id: &str) -> anyhow::Result<()> {
        let h = self.handle(id)?;
        if h.with_state(|s| matches!(s, ManagedTorrentState::Live(_) | ManagedTorrentState::Initializing(_))) {
            self.session.pause(&h).await?;
        }
        self.emit_updated(&h);
        Ok(())
    }

    pub async fn resume(&self, id: &str) -> anyhow::Result<()> {
        let h = self.handle(id)?;
        if !h.with_state(|s| matches!(s, ManagedTorrentState::Live(_))) {
            self.session.unpause(&h).await?;
        }
        self.emit_updated(&h);
        Ok(())
    }

    /// Pauses every running torrent, seeding ones included.
    pub async fn pause_all(&self) {
        for h in self.handles() {
            if h.with_state(|s| matches!(s, ManagedTorrentState::Live(_) | ManagedTorrentState::Initializing(_))) {
                if let Err(e) = self.session.pause(&h).await {
                    tracing::warn!("cannot pause torrent: {e:#}");
                }
                self.emit_updated(&h);
            }
        }
    }

    /// Resumes paused torrents that still have data to download.
    pub async fn resume_all(&self) {
        for h in self.handles() {
            if Self::is_paused(&h) && !h.stats().finished {
                if let Err(e) = self.session.unpause(&h).await {
                    tracing::warn!("cannot resume torrent: {e:#}");
                }
                self.emit_updated(&h);
            }
        }
    }

    pub async fn remove(&self, id: &str, delete_files: bool) -> anyhow::Result<()> {
        let h = self.handle(id)?;
        let folder = h.output_folder().to_path_buf();
        let own_folder = has_own_folder(&h);
        let hash = h.info_hash().as_string();
        self.session.delete(TorrentIdOrHash::Hash(h.info_hash()), delete_files).await?;
        if delete_files && own_folder {
            // Ignored if anything else is left in it.
            remove_empty_dirs(&folder);
        }
        self.meta.lock().remove(&hash);
        self.magnets.lock().remove(&hash);
        self.save_meta();
        (self.sink)(TorrentEvent::Removed(hash));
        Ok(())
    }

    pub async fn set_files(&self, id: &str, files: Vec<usize>) -> anyhow::Result<()> {
        let h = self.handle(id)?;
        let set: HashSet<usize> = files.into_iter().collect();
        if set.is_empty() {
            bail!("select at least one file");
        }
        self.session.update_only_files(&h, &set).await?;
        // New files to fetch: the torrent is no longer complete.
        if let Some(m) = self.meta.lock().get_mut(&h.info_hash().as_string()) {
            m.completed_at = None;
        }
        self.save_meta();
        self.emit_updated(&h);
        Ok(())
    }

    /// Moves a streamed torrent out of the cache into the download folder and
    /// keeps downloading/seeding it from there.
    pub async fn keep(&self, id: &str) -> anyhow::Result<TorrentView> {
        let h = self.handle(id)?;
        let view = self.view_of(&h, &self.meta.lock());
        if !view.stream {
            return Ok(view);
        }
        let torrent_bytes = h.metadata.load().as_ref().context("the torrent metadata is not ready yet")?.torrent_bytes.clone();
        let only_files = h.only_files();
        let paused = Self::is_paused(&h);
        let old = view.save_dir.clone();
        let download_dir = self.download_dir.read().clone();
        let new = if old == self.cache_dir {
            download_dir
        } else {
            download_dir.join(old.file_name().context("invalid output folder")?)
        };

        self.session.delete(TorrentIdOrHash::Hash(h.info_hash()), false).await?;
        let moved: Vec<String> = view.files.iter().filter(|f| f.included).map(|f| f.path.clone()).collect();
        let own_folder = has_own_folder(&h);
        let moved = tokio::task::spawn_blocking({
            let (old, new) = (old.clone(), new.clone());
            move || {
                move_files(&old, &new, &moved)?;
                if own_folder {
                    remove_empty_dirs(&old);
                }
                anyhow::Ok(())
            }
        })
        .await?;
        // Re-add even if moving failed, so the torrent does not disappear.
        let target = if moved.is_ok() { &new } else { &old };
        let response = self
            .session
            .add_torrent(
                librqbit::AddTorrent::from_bytes(torrent_bytes),
                Some(AddTorrentOptions {
                    paused,
                    only_files,
                    output_folder: Some(target.to_string_lossy().into_owned()),
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await?;
        moved?;
        let h = match response {
            AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h) => h,
            AddTorrentResponse::ListOnly(_) => bail!("unexpected list-only response"),
        };
        let view = self.view_of(&h, &self.meta.lock());
        (self.sink)(TorrentEvent::Updated(view.clone()));
        Ok(view)
    }

    // -----------------------------------------------------------------------
    // Progress
    // -----------------------------------------------------------------------

    /// Sends progress once per second, reports finished torrents and pauses
    /// them when seeding is turned off.
    pub async fn run_ticker(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let handles = self.handles();
            let stop_seeding = self.stop_seeding.load(Ordering::Relaxed);
            let mut completed = Vec::new();
            let views: Vec<TorrentView> = {
                let mut meta = self.meta.lock();
                handles
                    .iter()
                    .map(|h| {
                        let mut view = self.view_of(h, &meta);
                        if view.status == Status::Completed && view.completed_at.is_none() {
                            let m = meta.entry(view.id.clone()).or_default();
                            m.completed_at = Some(now_millis());
                            view.completed_at = m.completed_at;
                            completed.push(view.clone());
                        }
                        view
                    })
                    .collect()
            };
            if !completed.is_empty() {
                self.save_meta();
            }
            if stop_seeding {
                for h in handles.iter().filter(|h| {
                    let s = h.stats();
                    s.finished && matches!(s.state, TorrentStatsState::Live)
                }) {
                    if let Err(e) = self.session.pause(h).await {
                        tracing::warn!("cannot pause finished torrent: {e:#}");
                    }
                }
            }
            for view in completed {
                (self.sink)(TorrentEvent::Completed(view));
            }
            (self.sink)(TorrentEvent::Progress(views));
        }
    }

    fn emit_updated(&self, h: &Handle) {
        (self.sink)(TorrentEvent::Updated(self.view_of(h, &self.meta.lock())));
    }

    /// Gives torrents restored from librqbit's session an entry in the
    /// metadata file, and forgets entries of torrents that are gone.
    fn adopt_restored(&self) {
        let hashes: HashSet<String> = self.handles().iter().map(|h| h.info_hash().as_string()).collect();
        let mut meta = self.meta.lock();
        meta.retain(|hash, _| hashes.contains(hash));
        for hash in hashes {
            meta.entry(hash).or_insert_with(|| Meta { created_at: now_millis(), ..Default::default() });
        }
        drop(meta);
        self.save_meta();
    }

    fn save_meta(&self) {
        let data = serde_json::to_vec_pretty(&*self.meta.lock()).expect("metadata serializes");
        let tmp = self.meta_path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, data).and_then(|_| std::fs::rename(&tmp, &self.meta_path)) {
            tracing::warn!("cannot save torrent metadata: {e}");
        }
    }

    fn view_of(&self, h: &Handle, meta: &HashMap<String, Meta>) -> TorrentView {
        let hash = h.info_hash().as_string();
        let m = meta.get(&hash).cloned().unwrap_or_default();
        let s = h.stats();
        let metadata = h.metadata.load();
        let only_files = h.only_files();
        let files =
            metadata.as_ref().map(|md| file_entries(&md.info, only_files.as_deref(), &s.file_progress)).unwrap_or_default();
        let name = h
            .name()
            .or_else(|| metadata.as_ref().and_then(|md| md.info.name().map(|n| n.into_owned())))
            .unwrap_or_else(|| hash.clone());
        let url = match metadata.as_ref() {
            Some(md) => self
                .magnets
                .lock()
                .entry(hash.clone())
                .or_insert_with(|| magnet_link(&hash, &name, &announce_urls(&md.torrent_bytes)))
                .clone(),
            None => magnet_link(&hash, &name, &[]),
        };
        let output_folder = h.output_folder().to_path_buf();

        let (speed, upload_speed, peers) = s
            .live
            .as_ref()
            .map(|l| (l.download_speed.as_bytes(), l.upload_speed.as_bytes(), l.snapshot.peer_stats.live))
            .unwrap_or((0, 0, 0));
        let status = match s.state {
            TorrentStatsState::Error => Status::Failed,
            TorrentStatsState::Initializing { .. } => Status::Connecting,
            _ if s.finished => Status::Completed,
            TorrentStatsState::Paused => Status::Paused,
            TorrentStatsState::Live => Status::Downloading,
        };
        let remaining = s.total_bytes.saturating_sub(s.progress_bytes);
        let eta = (status == Status::Downloading && speed > 0).then(|| remaining.div_ceil(speed));
        let mut selected = files.iter().filter(|f| f.included);
        let final_path = match (selected.next(), selected.next()) {
            (Some(f), None) => Some(join_torrent_path(&output_folder, &f.path)),
            _ => None,
        };

        TorrentView {
            kind: "torrent",
            url,
            file_name: name,
            stream: output_folder.starts_with(&self.cache_dir),
            save_dir: output_folder,
            final_path,
            total_size: metadata.as_ref().map(|_| s.total_bytes),
            downloaded: s.progress_bytes,
            status,
            error: s.error,
            speed,
            eta,
            upload_speed,
            uploaded: s.uploaded_bytes,
            peers,
            seeding: s.finished && matches!(s.state, TorrentStatsState::Live) && !self.info.upload_disabled,
            files,
            source: m.source,
            created_at: m.created_at,
            completed_at: m.completed_at,
            id: hash,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn file_entries<B: AsRef<[u8]>>(
    info: &librqbit::ValidatedTorrentMetaV1Info<B>,
    only_files: Option<&[usize]>,
    progress: &[u64],
) -> Vec<TorrentFile> {
    info.iter_file_details()
        .enumerate()
        .filter(|(_, d)| !d.attrs().padding)
        .map(|(idx, d)| {
            let parts = d.filename.to_vec();
            let name = parts.last().cloned().unwrap_or_default();
            TorrentFile {
                index: idx,
                path: parts.join("/"),
                kind: kind_of(&name),
                name,
                size: d.len,
                included: only_files.map(|o| o.contains(&idx)).unwrap_or(true),
                downloaded: progress.get(idx).copied().unwrap_or(0),
            }
        })
        .collect()
}

/// Tracker URLs listed in .torrent bytes.
fn announce_urls(torrent_bytes: &[u8]) -> Vec<String> {
    librqbit::torrent_from_bytes(torrent_bytes)
        .map(|t| t.iter_announce().filter_map(|a| std::str::from_utf8(a.as_ref()).ok().map(str::to_owned)).collect())
        .unwrap_or_default()
}

fn magnet_link(hash: &str, name: &str, trackers: &[String]) -> String {
    let mut link = format!("magnet:?xt=urn:btih:{hash}&dn={}", utf8_percent_encode(name, NON_ALPHANUMERIC));
    let mut seen = HashSet::new();
    for tr in trackers.iter().filter(|t| seen.insert(t.as_str())) {
        link.push_str("&tr=");
        link.extend(utf8_percent_encode(tr, NON_ALPHANUMERIC));
    }
    link
}

fn join_torrent_path(folder: &Path, rel: &str) -> PathBuf {
    let mut path = folder.to_path_buf();
    path.extend(rel.split('/'));
    path
}

/// Moves `files` (paths relative to `from`) into `to`, falling back to copy
/// when the folders are on different volumes.
fn move_files(from: &Path, to: &Path, files: &[String]) -> anyhow::Result<()> {
    for rel in files {
        let src = join_torrent_path(from, rel);
        if !src.exists() {
            continue;
        }
        let dst = join_torrent_path(to, rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::rename(&src, &dst).is_err() {
            std::fs::copy(&src, &dst).with_context(|| format!("cannot copy {}", src.display()))?;
            std::fs::remove_file(&src).ok();
        }
    }
    Ok(())
}

/// Multi-file torrents get a folder of their own (see [`TorrentEngine::add`]);
/// single files sit directly in the download or cache folder.
fn has_own_folder(h: &Handle) -> bool {
    h.metadata.load().as_ref().is_some_and(|m| file_entries(&m.info, None, &[]).len() > 1)
}

/// Removes `dir` and its subfolders if they contain no files.
fn remove_empty_dirs(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                remove_empty_dirs(&entry.path());
            }
        }
    }
    std::fs::remove_dir(dir).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_magnet_links() {
        let link = magnet_link("abc", "My Movie (2024)", &["udp://t.org:80".into(), "udp://t.org:80".into()]);
        assert_eq!(link, "magnet:?xt=urn:btih:abc&dn=My%20Movie%20%282024%29&tr=udp%3A%2F%2Ft%2Eorg%3A80");
    }

    async fn engine(dir: &Path, port: u16) -> Arc<TorrentEngine> {
        TorrentEngine::start(
            TorrentConfig {
                data_dir: dir.join("data"),
                download_dir: dir.join("dl"),
                cache_dir: dir.join("cache"),
                listen_port: Some(port),
                download_limit: None,
                upload_limit: None,
                upload_mode: UploadMode::Normal,
            },
            Arc::new(|_| {}),
        )
        .await
        .unwrap()
    }

    /// Seeds a generated "movie" from one engine and streams it from another
    /// over loopback: covers inspect, add (stream + download), the HTTP stream
    /// with Range requests, subtitles, keep, pause/resume and remove.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn streams_from_a_local_seeder() {
        let tmp = tempfile::tempdir().unwrap();
        let content = tmp.path().join("content/Local Movie");
        std::fs::create_dir_all(&content).unwrap();
        let movie: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(content.join("movie.mp4"), &movie).unwrap();
        std::fs::write(content.join("movie.srt"), "1\r\n00:00:01,000 --> 00:00:02,000\r\nHi\r\n").unwrap();
        std::fs::write(content.join("notes.txt"), "hello").unwrap();

        let seeder = engine(&tmp.path().join("seeder"), 47_301).await;
        let spawner = librqbit::spawn_utils::BlockingSpawner::new(4);
        let torrent = librqbit::create_torrent(
            &content,
            librqbit::CreateTorrentOptions { name: None, trackers: vec![], piece_length: Some(65_536) },
            &spawner,
        )
        .await
        .unwrap()
        .as_bytes()
        .unwrap()
        .to_vec();

        let preview = seeder.inspect(TorrentSource::Bytes(torrent.clone()), &[], Duration::from_secs(10)).await.unwrap();
        assert_eq!(preview.name, "Local Movie");
        assert_eq!(preview.files.len(), 3);
        assert!(!preview.existing);
        let seeding = seeder
            .add(AddTorrent {
                id: preview.id.clone(),
                mode: AddMode::Download,
                files: preview.files.iter().map(|f| f.index).collect(),
                save_dir: Some(tmp.path().join("content")),
                start: true,
                source: None,
            })
            .await
            .unwrap();
        assert_eq!(seeding.save_dir, content);
        for _ in 0..100 {
            if seeder.get(&preview.id).unwrap().status == Status::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(seeder.get(&preview.id).unwrap().status, Status::Completed, "seeder did not verify its data");

        // Leecher: stream mode picks the video and includes subtitles.
        let leecher = engine(&tmp.path().join("leecher"), 47_302).await;
        let preview = leecher.inspect(TorrentSource::Bytes(torrent), &[], Duration::from_secs(10)).await.unwrap();
        leecher.pending.lock().get_mut(&preview.id).unwrap().seen_peers.push("127.0.0.1:47301".parse().unwrap());
        let view = leecher
            .add(AddTorrent {
                id: preview.id.clone(),
                mode: AddMode::Stream,
                files: vec![],
                save_dir: None,
                start: true,
                source: None,
            })
            .await
            .unwrap();
        assert!(view.stream);
        let mut included: Vec<&str> = view.files.iter().filter(|f| f.included).map(|f| f.name.as_str()).collect();
        included.sort_unstable();
        assert_eq!(included, ["movie.mp4", "movie.srt"]);
        assert!(view.url.starts_with(&format!("magnet:?xt=urn:btih:{}", view.id)));

        let video = view.files.iter().find(|f| f.name == "movie.mp4").unwrap().index;
        let url = leecher.stream_url(&view.id, video).await.unwrap();
        assert!(url.starts_with("http://127.0.0.1:") && url.ends_with("/movie%2Emp4"), "{url}");
        let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(60)).build().unwrap();
        let resp = client.get(&url).header("Range", "bytes=2000000-2099999").send().await.unwrap();
        assert_eq!(resp.status(), 206);
        assert_eq!(resp.bytes().await.unwrap(), movie[2_000_000..2_100_000]);

        let srt = view.files.iter().find(|f| f.name == "movie.srt").unwrap().index;
        let vtt = leecher.subtitle(&view.id, srt).await.unwrap();
        assert_eq!(vtt, "WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.000\nHi\n");

        // Streaming a file that was not selected selects it.
        let notes = view.files.iter().find(|f| f.name == "notes.txt").unwrap().index;
        leecher.stream_url(&view.id, notes).await.unwrap();
        assert!(leecher.get(&view.id).unwrap().files.iter().all(|f| f.included));

        for _ in 0..200 {
            if leecher.get(&view.id).unwrap().status == Status::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let done = leecher.get(&view.id).unwrap();
        assert_eq!(done.status, Status::Completed);
        assert_eq!(done.downloaded, done.total_size.unwrap());

        // Keep: moves out of the stream cache into the download folder.
        let kept = leecher.keep(&view.id).await.unwrap();
        assert!(!kept.stream);
        assert_eq!(kept.save_dir, tmp.path().join("leecher/dl/Local Movie"));
        assert_eq!(std::fs::read(kept.save_dir.join("movie.mp4")).unwrap(), movie);
        assert!(!tmp.path().join("leecher/cache/Local Movie").exists());

        let h = leecher.handle(&view.id).unwrap();
        h.wait_until_initialized().await.unwrap();
        leecher.pause(&view.id).await.unwrap();
        assert!(TorrentEngine::is_paused(&h));
        assert_eq!(leecher.get(&view.id).unwrap().status, Status::Completed, "finished torrents stay complete when paused");
        leecher.resume(&view.id).await.unwrap();
        assert!(h.with_state(|s| matches!(s, ManagedTorrentState::Live(_))));
        leecher.resume(&view.id).await.unwrap();

        leecher.remove(&view.id, true).await.unwrap();
        assert!(leecher.list().is_empty());
        assert!(!kept.save_dir.exists());
        assert!(seeder.list().len() == 1 && content.join("movie.mp4").exists(), "seeder data untouched");
    }

    /// Torrents, their file selection, pause state and NBS metadata survive a restart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restores_torrents_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let content = tmp.path().join("content/Pack");
        std::fs::create_dir_all(&content).unwrap();
        std::fs::write(content.join("a.bin"), vec![1u8; 200_000]).unwrap();
        std::fs::write(content.join("b.bin"), vec![2u8; 100_000]).unwrap();
        let spawner = librqbit::spawn_utils::BlockingSpawner::new(2);
        let options = librqbit::CreateTorrentOptions { name: None, trackers: vec![], piece_length: Some(65_536) };
        let torrent = librqbit::create_torrent(&content, options, &spawner).await.unwrap().as_bytes().unwrap().to_vec();

        let (id, created_at) = {
            let engine = engine(&tmp.path().join("app"), 0).await;
            let preview = engine.inspect(TorrentSource::Bytes(torrent), &[], Duration::from_secs(10)).await.unwrap();
            let request = AddTorrent {
                id: preview.id.clone(),
                mode: AddMode::Download,
                files: vec![1],
                save_dir: None,
                start: false,
                source: Some("browser".into()),
            };
            let view = engine.add(request).await.unwrap();
            engine.shutdown().await;
            (view.id, view.created_at)
        };

        let engine = engine(&tmp.path().join("app"), 0).await;
        let views = engine.list();
        assert_eq!(views.len(), 1);
        let view = &views[0];
        assert_eq!(view.id, id);
        assert_eq!(view.created_at, created_at);
        assert_eq!(view.source.as_deref(), Some("browser"));
        assert_eq!(view.files.iter().filter(|f| f.included).count(), 1);
        assert_eq!(view.save_dir, tmp.path().join("app/dl/Pack"));
        let h = engine.handle(&id).unwrap();
        h.wait_until_initialized().await.unwrap();
        assert_eq!(engine.get(&id).unwrap().status, Status::Paused);
    }

    #[test]
    fn moves_files_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("cache/Show");
        let to = tmp.path().join("dl/Show");
        std::fs::create_dir_all(from.join("Season 1")).unwrap();
        std::fs::write(from.join("Season 1/e1.mkv"), b"video").unwrap();
        move_files(&from, &to, &["Season 1/e1.mkv".into(), "missing.srt".into()]).unwrap();
        remove_empty_dirs(&from);
        assert_eq!(std::fs::read(to.join("Season 1/e1.mkv")).unwrap(), b"video");
        assert!(!from.exists());
    }
}
