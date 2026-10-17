//! Torrent download & streaming: starts the engine, exposes it to the UI, and
//! handles magnet links / .torrent files from the command line, the browser
//! extension and the OS.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nbs_torrent::{AddTorrent, EngineInfo, TorrentConfig, TorrentEngine, TorrentEvent, TorrentPreview, TorrentSink, TorrentSource, TorrentView};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

use crate::settings::Settings;
use crate::state::AppState;

type CmdResult<T> = Result<T, String>;

const METADATA_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_TORRENT_FILE: u64 = 16 * 1024 * 1024;

/// A magnet link or .torrent file handed to the app, waiting for the UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TorrentOpen {
    /// Magnet link, info-hash or http(s) URL of a .torrent file.
    pub link: Option<String>,
    /// Local .torrent file.
    pub path: Option<String>,
    /// Sent when downloading `link`, e.g. cookies captured from the browser.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub source: Option<String>,
}

#[derive(Default)]
pub struct Torrents {
    engine: RwLock<Option<Arc<TorrentEngine>>>,
    error: Mutex<Option<String>>,
    opens: Mutex<Vec<TorrentOpen>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TorrentStatus {
    pub ready: bool,
    pub error: Option<String>,
    pub info: Option<EngineInfo>,
    pub stream_dir: PathBuf,
}

impl Torrents {
    pub fn engine(&self) -> CmdResult<Arc<TorrentEngine>> {
        if let Some(engine) = self.engine.read().clone() {
            return Ok(engine);
        }
        Err(match self.error.lock().clone() {
            Some(e) => format!("Torrent engine failed to start: {e}"),
            None => "The torrent engine is still starting, try again in a moment".into(),
        })
    }

    pub fn running(&self) -> Option<Arc<TorrentEngine>> {
        self.engine.read().clone()
    }

    /// Queues a magnet link / .torrent file and tells the UI to pick it up.
    pub fn open(&self, app: &AppHandle, open: TorrentOpen) {
        self.opens.lock().push(open);
        crate::show_main_window(app);
        let _ = app.emit("torrent://open-request", ());
    }
}

/// Magnet links and .torrent files among command-line arguments (the OS passes
/// them when the app is the registered handler).
pub fn opens_from_args(args: impl IntoIterator<Item = String>) -> Vec<TorrentOpen> {
    args.into_iter()
        .filter_map(|arg| {
            let lower = arg.to_ascii_lowercase();
            // Linux file managers may pass file:// URLs (the .desktop entry uses %U).
            let arg = match lower.starts_with("file://").then(|| tauri::Url::parse(&arg).ok()?.to_file_path().ok()).flatten() {
                Some(path) => path.to_string_lossy().into_owned(),
                None => arg,
            };
            let lower = arg.to_ascii_lowercase();
            if lower.starts_with("magnet:") {
                Some(TorrentOpen { link: Some(arg), ..Default::default() })
            } else if lower.ends_with(".torrent") && Path::new(&arg).is_file() {
                Some(TorrentOpen { path: Some(arg), ..Default::default() })
            } else {
                None
            }
        })
        .collect()
}

pub fn stream_dir(app: &AppHandle, settings: &Settings) -> PathBuf {
    if !settings.stream_dir.as_os_str().is_empty() {
        return settings.stream_dir.clone();
    }
    app.path().app_cache_dir().unwrap_or_else(|_| std::env::temp_dir().join("nbs-download-manager")).join("stream")
}

fn sink(app: AppHandle) -> TorrentSink {
    Arc::new(move |event| match event {
        TorrentEvent::Updated(view) => {
            let _ = app.emit("torrent://updated", view);
        }
        TorrentEvent::Progress(views) => {
            let _ = app.emit("torrent://progress", views);
        }
        TorrentEvent::Removed(id) => {
            let _ = app.emit("torrent://removed", id);
        }
        TorrentEvent::Completed(view) => {
            let notify = app.try_state::<AppState>().is_some_and(|s| s.settings.lock().notify_on_complete);
            if notify && !view.stream {
                let _ = app.notification().builder().title("Torrent complete").body(&view.file_name).show();
            }
            let _ = app.emit("torrent://completed", view);
        }
    })
}

/// Starts the engine in the background; commands fail with a "starting"
/// message until it is up.
pub fn start(app: AppHandle, torrents: Arc<Torrents>, data_dir: PathBuf, settings: &Settings) {
    let config = TorrentConfig {
        data_dir,
        download_dir: settings.torrent_download_dir(),
        cache_dir: stream_dir(&app, settings),
        listen_port: Some(settings.torrent_port).filter(|p| *p > 0),
        download_limit: settings.torrent_download_limit,
        upload_limit: settings.torrent_upload_limit,
        upload_mode: settings.torrent_upload_mode,
    };
    tauri::async_runtime::spawn(async move {
        match TorrentEngine::start(config, sink(app.clone())).await {
            Ok(engine) => {
                *torrents.engine.write() = Some(engine.clone());
                tauri::async_runtime::spawn(engine.run_ticker());
            }
            Err(e) => *torrents.error.lock() = Some(format!("{e:#}")),
        }
        if let Some(state) = app.try_state::<AppState>() {
            let _ = app.emit("torrent://status", status_of(&app, &state));
        }
    });
}

fn status_of(app: &AppHandle, state: &AppState) -> TorrentStatus {
    let engine = state.torrents.running();
    TorrentStatus {
        ready: engine.is_some(),
        error: state.torrents.error.lock().clone(),
        info: engine.map(|e| e.info()),
        stream_dir: stream_dir(app, &state.settings.lock()),
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn anyhow_err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tauri::command]
pub async fn torrent_status(app: AppHandle, state: State<'_, AppState>) -> CmdResult<TorrentStatus> {
    Ok(status_of(&app, &state))
}

#[tauri::command]
pub async fn torrent_list(state: State<'_, AppState>) -> CmdResult<Vec<TorrentView>> {
    Ok(state.torrents.running().map(|e| e.list()).unwrap_or_default())
}

/// Magnet links / .torrent files that arrived while the UI was not looking.
#[tauri::command]
pub async fn take_torrent_opens(state: State<'_, AppState>) -> CmdResult<Vec<TorrentOpen>> {
    Ok(std::mem::take(&mut *state.torrents.opens.lock()))
}

#[tauri::command]
pub async fn torrent_inspect(state: State<'_, AppState>, open: TorrentOpen) -> CmdResult<TorrentPreview> {
    let engine = state.torrents.engine()?;
    let source = match (open.path, open.link) {
        (Some(path), _) => {
            let len = std::fs::metadata(&path).map_err(|e| format!("{path}: {e}"))?.len();
            if len > MAX_TORRENT_FILE {
                return Err("the file is too large to be a .torrent file".into());
            }
            TorrentSource::Bytes(tokio::fs::read(&path).await.map_err(|e| format!("{path}: {e}"))?)
        }
        (None, Some(link)) if !link.trim().is_empty() => TorrentSource::Link(link),
        _ => return Err("enter a magnet link or choose a .torrent file".into()),
    };
    engine.inspect(source, &open.headers, METADATA_TIMEOUT).await.map_err(anyhow_err)
}

#[tauri::command]
pub async fn torrent_add(state: State<'_, AppState>, request: AddTorrent) -> CmdResult<TorrentView> {
    state.torrents.engine()?.add(request).await.map_err(anyhow_err)
}

#[tauri::command]
pub async fn torrent_pause(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.torrents.engine()?.pause(&id).await.map_err(anyhow_err)
}

#[tauri::command]
pub async fn torrent_resume(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.torrents.engine()?.resume(&id).await.map_err(anyhow_err)
}

#[tauri::command]
pub async fn torrent_remove(state: State<'_, AppState>, id: String, delete_files: bool) -> CmdResult<()> {
    state.torrents.engine()?.remove(&id, delete_files).await.map_err(anyhow_err)
}

#[tauri::command]
pub async fn torrent_set_files(state: State<'_, AppState>, id: String, files: Vec<usize>) -> CmdResult<()> {
    state.torrents.engine()?.set_files(&id, files).await.map_err(anyhow_err)
}

#[tauri::command]
pub async fn torrent_stream_url(state: State<'_, AppState>, id: String, file: usize) -> CmdResult<String> {
    state.torrents.engine()?.stream_url(&id, file).await.map_err(anyhow_err)
}

#[tauri::command]
pub async fn torrent_subtitle(state: State<'_, AppState>, id: String, file: usize) -> CmdResult<String> {
    state.torrents.engine()?.subtitle(&id, file).await.map_err(anyhow_err)
}

#[tauri::command]
pub async fn torrent_keep(state: State<'_, AppState>, id: String) -> CmdResult<TorrentView> {
    state.torrents.engine()?.keep(&id).await.map_err(anyhow_err)
}

/// Opens one file (or, without `file`, the torrent's single file or folder).
#[tauri::command]
pub async fn torrent_open(app: AppHandle, state: State<'_, AppState>, id: String, file: Option<usize>) -> CmdResult<()> {
    let engine = state.torrents.engine()?;
    let path = match file {
        Some(file) => engine.file_path(&id, file).map_err(anyhow_err)?,
        None => {
            let view = engine.get(&id).map_err(anyhow_err)?;
            view.final_path.unwrap_or(view.save_dir)
        }
    };
    if !path.exists() {
        return Err("the file is not on disk yet".into());
    }
    app.opener().open_path(path.to_string_lossy(), None::<&str>).map_err(err)
}

#[tauri::command]
pub async fn torrent_reveal(app: AppHandle, state: State<'_, AppState>, id: String, file: Option<usize>) -> CmdResult<()> {
    let engine = state.torrents.engine()?;
    let view = engine.get(&id).map_err(anyhow_err)?;
    let path = match file {
        Some(file) => Some(engine.file_path(&id, file).map_err(anyhow_err)?),
        None => view.final_path,
    };
    match path.filter(|p| p.exists()) {
        Some(path) => app.opener().reveal_item_in_dir(path).map_err(err),
        None => {
            let dir = if view.save_dir.exists() { view.save_dir } else { view.save_dir.parent().map(Path::to_path_buf).unwrap_or_default() };
            app.opener().open_path(dir.to_string_lossy(), None::<&str>).map_err(err)
        }
    }
}

#[tauri::command]
pub async fn pick_torrent_file(app: AppHandle) -> CmdResult<Option<String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().set_title("Open torrent file").add_filter("Torrent files", &["torrent"]).pick_file(move |path| {
        let _ = tx.send(path.and_then(|p| p.into_path().ok()));
    });
    Ok(rx.await.map_err(err)?.map(|p| p.to_string_lossy().into_owned()))
}

/// Plays a stream in the configured player, or in the system's default player
/// for playlists when none is set.
#[tauri::command]
pub async fn play_external(app: AppHandle, state: State<'_, AppState>, url: String, title: String) -> CmdResult<()> {
    if !url.starts_with("http://127.0.0.1:") {
        return Err("only local streams can be played".into());
    }
    let player = state.settings.lock().external_player.trim().to_string();
    if !player.is_empty() {
        return std::process::Command::new(&player).arg(&url).spawn().map(|_| ()).map_err(|e| format!("cannot start {player}: {e}"));
    }
    let title: String = title.chars().filter(|c| !c.is_control()).collect();
    let playlist = std::env::temp_dir().join("nbs-download-manager-stream.m3u");
    std::fs::write(&playlist, format!("#EXTM3U\n#EXTINF:-1,{title}\n{url}\n")).map_err(err)?;
    app.opener()
        .open_path(playlist.to_string_lossy(), None::<&str>)
        .map_err(|e| format!("no media player is set up to open playlists ({e}); choose one under Options → Torrents"))
}

#[tauri::command]
pub async fn pick_player(app: AppHandle) -> CmdResult<Option<String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().set_title("Choose a media player").pick_file(move |path| {
        let _ = tx.send(path.and_then(|p| p.into_path().ok()));
    });
    Ok(rx.await.map_err(err)?.map(|p| p.to_string_lossy().into_owned()))
}

/// Makes the app the handler for `magnet:` links (and .torrent files on Linux).
#[tauri::command]
pub async fn register_magnet_handler() -> CmdResult<String> {
    let exe = std::env::current_exe().map_err(err)?;
    register(&exe)
}

#[cfg(windows)]
fn register(exe: &Path) -> CmdResult<String> {
    let command = format!("\"{}\" \"%1\"", exe.display());
    let entries: [(&str, Option<&str>, &str); 4] = [
        (r"HKCU\Software\Classes\magnet", None, "URL:Magnet link"),
        (r"HKCU\Software\Classes\magnet", Some("URL Protocol"), ""),
        (r"HKCU\Software\Classes\magnet\DefaultIcon", None, &format!("\"{}\",0", exe.display())),
        (r"HKCU\Software\Classes\magnet\shell\open\command", None, &command),
    ];
    for (key, value, data) in entries {
        let mut cmd = std::process::Command::new("reg");
        cmd.args(["add", key]);
        match value {
            Some(v) => cmd.args(["/v", v]),
            None => cmd.arg("/ve"),
        };
        let status = cmd.args(["/t", "REG_SZ", "/d", data, "/f"]).status().map_err(|e| format!("failed to run reg.exe: {e}"))?;
        if !status.success() {
            return Err(format!("failed to write {key} in the registry"));
        }
    }
    Ok("Magnet links now open in NBS Download Manager. If Windows still asks, pick it under Settings → Default apps.".into())
}

#[cfg(target_os = "linux")]
fn register(exe: &Path) -> CmdResult<String> {
    let dir = dirs::data_dir().ok_or("cannot find the applications folder")?.join("applications");
    std::fs::create_dir_all(&dir).map_err(err)?;
    let name = "nbs-download-manager-torrent.desktop";
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=NBS Download Manager\nExec=\"{}\" %u\nIcon=nbs-download-manager\nTerminal=false\nNoDisplay=true\nMimeType=x-scheme-handler/magnet;application/x-bittorrent;\n",
        exe.display()
    );
    std::fs::write(dir.join(name), entry).map_err(err)?;
    for mime in ["x-scheme-handler/magnet", "application/x-bittorrent"] {
        let status = std::process::Command::new("xdg-mime").args(["default", name, mime]).status().map_err(|e| format!("failed to run xdg-mime: {e}"))?;
        if !status.success() {
            return Err(format!("xdg-mime could not register {mime}"));
        }
    }
    Ok("Magnet links and .torrent files now open in NBS Download Manager.".into())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn register(_exe: &Path) -> CmdResult<String> {
    Err("Choose NBS Download Manager as the magnet link handler in your system settings.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_torrents_in_arguments() {
        let tmp = std::env::temp_dir().join("nbs-args-test.torrent");
        std::fs::write(&tmp, b"d4:infode").unwrap();
        let args = vec![
            "nbs-download-manager".to_string(),
            "magnet:?xt=urn:btih:abc".to_string(),
            tmp.to_string_lossy().into_owned(),
            "missing.torrent".to_string(),
            tauri::Url::from_file_path(&tmp).unwrap().to_string(),
            "--flag".to_string(),
        ];
        let opens = opens_from_args(args);
        assert_eq!(opens.len(), 3);
        assert_eq!(opens[0].link.as_deref(), Some("magnet:?xt=urn:btih:abc"));
        assert_eq!(opens[1].path.as_deref(), Some(tmp.to_str().unwrap()));
        assert_eq!(opens[2].path.as_deref(), Some(tmp.to_str().unwrap()), "file:// URLs become paths");
        std::fs::remove_file(tmp).ok();
    }
}
