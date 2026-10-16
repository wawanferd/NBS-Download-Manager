// Prevents an additional console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bridge;
mod commands;
mod native_host;
mod settings;
mod state;

use std::sync::Arc;

use nbs_core::{DownloadManager, EngineConfig, Event, EventSink};
use parking_lot::Mutex;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_notification::NotificationExt;

use crate::bridge::Bridge;
use crate::settings::Settings;
use crate::state::AppState;

pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Forwards engine events to the web UI and shows completion notifications.
fn event_sink(app: AppHandle) -> EventSink {
    Arc::new(move |event| match event {
        Event::Updated(view) => {
            let _ = app.emit("download://updated", view);
        }
        Event::Progress(views) => {
            let _ = app.emit("download://progress", views);
        }
        Event::Removed(id) => {
            let _ = app.emit("download://removed", id);
        }
        Event::Completed(view) => {
            let notify = app.try_state::<AppState>().is_some_and(|s| s.settings.lock().notify_on_complete);
            if notify {
                let _ = app.notification().builder().title("Download complete").body(&view.file_name).show();
            }
            let _ = app.emit("download://completed", view);
        }
    })
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show NBS Download Manager", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause_all", "Pause all", true, None::<&str>)?;
    let resume = MenuItem::with_id(app, "resume_all", "Resume all", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show, &pause, &resume, &separator, &quit])?;

    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("NBS Download Manager")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let state = app.state::<AppState>();
            match event.id().as_ref() {
                "show" => show_main_window(app),
                "pause_all" => state.manager.pause_all(),
                "resume_all" => state.manager.resume_all(),
                "quit" => {
                    state.manager.persist();
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();
    let config_dir = app.path().app_config_dir()?;
    let data_dir = app.path().app_data_dir()?;

    let settings_path = config_dir.join("settings.json");
    let mut settings = Settings::load(&settings_path);
    if settings.download_dir.as_os_str().is_empty() {
        settings.download_dir = app
            .path()
            .download_dir()
            .or_else(|_| app.path().home_dir().map(|h| h.join("Downloads")))?;
    }

    let mut config = EngineConfig::default();
    settings.apply_to(&mut config);
    let sink = event_sink(handle.clone());
    let global_limit = settings.global_speed_limit;
    let manager = tauri::async_runtime::block_on(async move {
        DownloadManager::new(data_dir.join("downloads.json"), config, global_limit, sink)
    })?;

    let bridge = Arc::new(Bridge::default());
    let port = settings.bridge_port;
    app.manage(AppState {
        manager: manager.clone(),
        settings: Mutex::new(settings),
        settings_path,
        bridge: bridge.clone(),
    });

    tauri::async_runtime::spawn(async move { manager.run_ticker().await });
    bridge.start(handle, port);
    build_tray(app)?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main_window(app)))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .setup(setup)
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let to_tray = window.app_handle().state::<AppState>().settings.lock().close_to_tray;
                if to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_downloads,
            commands::probe_url,
            commands::add_download,
            commands::pause_download,
            commands::resume_download,
            commands::restart_download,
            commands::remove_download,
            commands::pause_all,
            commands::resume_all,
            commands::clear_completed,
            commands::set_download_speed_limit,
            commands::set_download_connections,
            commands::get_settings,
            commands::save_settings,
            commands::set_global_speed_limit,
            commands::pick_folder,
            commands::open_download,
            commands::reveal_download,
            commands::bridge_status,
            commands::disk_space,
            commands::open_folder,
            commands::install_native_host,
            commands::app_info,
        ])
        .build(tauri::generate_context!())
        .expect("error while building NBS Download Manager")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                if let Some(state) = app.try_state::<AppState>() {
                    state.manager.persist();
                }
            }
        });
}
