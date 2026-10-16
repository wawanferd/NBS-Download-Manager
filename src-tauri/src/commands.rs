use std::path::PathBuf;

use nbs_core::{DownloadView, NewDownload, ProbeResult, Status};
use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::bridge::BridgeStatus;
use crate::native_host;
use crate::settings::Settings;
use crate::state::AppState;

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[tauri::command]
pub async fn list_downloads(state: State<'_, AppState>) -> CmdResult<Vec<DownloadView>> {
    Ok(state.manager.list())
}

#[tauri::command]
pub async fn probe_url(state: State<'_, AppState>, url: String, headers: Option<Vec<(String, String)>>) -> CmdResult<ProbeResult> {
    state.manager.probe(&url, &headers.unwrap_or_default()).await.map_err(err)
}

#[tauri::command]
pub async fn add_download(state: State<'_, AppState>, request: NewDownload) -> CmdResult<DownloadView> {
    state.manager.add(request).map_err(err)
}

#[tauri::command]
pub async fn pause_download(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.manager.pause(&id).map_err(err)
}

#[tauri::command]
pub async fn resume_download(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.manager.resume(&id).map_err(err)
}

#[tauri::command]
pub async fn restart_download(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.manager.restart(&id).await.map_err(err)
}

#[tauri::command]
pub async fn remove_download(state: State<'_, AppState>, id: String, delete_file: bool) -> CmdResult<()> {
    state.manager.remove(&id, delete_file).await.map_err(err)
}

#[tauri::command]
pub async fn pause_all(state: State<'_, AppState>) -> CmdResult<()> {
    state.manager.pause_all();
    Ok(())
}

#[tauri::command]
pub async fn resume_all(state: State<'_, AppState>) -> CmdResult<()> {
    state.manager.resume_all();
    Ok(())
}

#[tauri::command]
pub async fn clear_completed(state: State<'_, AppState>) -> CmdResult<()> {
    state.manager.clear_completed();
    Ok(())
}

#[tauri::command]
pub async fn set_download_speed_limit(state: State<'_, AppState>, id: String, limit: Option<u64>) -> CmdResult<()> {
    state.manager.set_speed_limit(&id, limit).map_err(err)
}

#[tauri::command]
pub async fn set_download_connections(state: State<'_, AppState>, id: String, connections: u8) -> CmdResult<()> {
    state.manager.set_connections(&id, connections).map_err(err)
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> CmdResult<Settings> {
    Ok(state.settings.lock().clone())
}

#[tauri::command]
pub async fn save_settings(app: AppHandle, state: State<'_, AppState>, settings: Settings) -> CmdResult<Settings> {
    let settings = settings.normalized();
    let old_port = {
        let mut current = state.settings.lock();
        let old = current.bridge_port;
        *current = settings.clone();
        old
    };
    settings.save(&state.settings_path).map_err(err)?;
    state.manager.update_config(|c| settings.apply_to(c));
    state.manager.set_global_speed_limit(settings.global_speed_limit);
    if old_port != settings.bridge_port {
        state.bridge.start(app, settings.bridge_port);
    }
    Ok(settings)
}

/// Quick global limit control from the sidebar.
#[tauri::command]
pub async fn set_global_speed_limit(state: State<'_, AppState>, limit: Option<u64>) -> CmdResult<()> {
    let limit = limit.filter(|l| *l > 0);
    state.manager.set_global_speed_limit(limit);
    let settings = {
        let mut s = state.settings.lock();
        s.global_speed_limit = limit;
        s.clone()
    };
    settings.save(&state.settings_path).map_err(err)
}

#[tauri::command]
pub async fn pick_folder(app: AppHandle, current: Option<String>) -> CmdResult<Option<String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut dialog = app.dialog().file().set_title("Choose download folder");
    if let Some(dir) = current.filter(|d| !d.is_empty()) {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder(move |path| {
        let _ = tx.send(path.and_then(|p| p.into_path().ok()));
    });
    Ok(rx.await.map_err(err)?.map(|p| p.to_string_lossy().into_owned()))
}

fn completed_path(state: &AppState, id: &str) -> CmdResult<(Option<PathBuf>, PathBuf)> {
    let view = state.manager.get(id).map_err(err)?;
    let file = view.final_path.filter(|p| view.status == Status::Completed && p.exists());
    Ok((file, view.save_dir))
}

#[tauri::command]
pub async fn open_download(app: AppHandle, state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let (file, _) = completed_path(&state, &id)?;
    let file = file.ok_or("the file is not available")?;
    app.opener().open_path(file.to_string_lossy(), None::<&str>).map_err(err)
}

#[tauri::command]
pub async fn reveal_download(app: AppHandle, state: State<'_, AppState>, id: String) -> CmdResult<()> {
    match completed_path(&state, &id)? {
        (Some(file), _) => app.opener().reveal_item_in_dir(file).map_err(err),
        (None, dir) => app.opener().open_path(dir.to_string_lossy(), None::<&str>).map_err(err),
    }
}

#[tauri::command]
pub async fn open_folder(app: AppHandle, path: String) -> CmdResult<()> {
    std::fs::create_dir_all(&path).map_err(err)?;
    app.opener().open_path(path, None::<&str>).map_err(err)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskSpace {
    total: u64,
    free: u64,
}

/// Capacity of the disk holding `path` (or its nearest existing parent).
#[tauri::command]
pub async fn disk_space(path: String) -> CmdResult<DiskSpace> {
    let mut dir = PathBuf::from(path);
    while !dir.exists() {
        if !dir.pop() {
            return Err("folder does not exist".into());
        }
    }
    Ok(DiskSpace {
        total: fs2::total_space(&dir).map_err(err)?,
        free: fs2::available_space(&dir).map_err(err)?,
    })
}

#[tauri::command]
pub async fn bridge_status(state: State<'_, AppState>) -> CmdResult<BridgeStatus> {
    Ok(state.bridge.status())
}

#[tauri::command]
pub async fn install_native_host(browser: String) -> CmdResult<String> {
    native_host::install(&browser).map(|p| p.to_string_lossy().into_owned())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    version: &'static str,
    chrome_extension_id: &'static str,
    firefox_extension_id: &'static str,
}

#[tauri::command]
pub fn app_info() -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION"),
        chrome_extension_id: native_host::CHROME_EXTENSION_ID,
        firefox_extension_id: native_host::FIREFOX_EXTENSION_ID,
    }
}
