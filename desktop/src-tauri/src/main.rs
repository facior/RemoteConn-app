// No console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod account_cmds;
mod commands;
mod config;
mod deeplink;
mod host_cmds;
mod identity;
mod secret;
mod service;
mod settings_cmds;
mod state;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;
use tracing_subscriber::EnvFilter;

/// Passed by the OS autostart entry: start hidden in the tray.
const MINIMIZED_ARG: &str = "--minimized";

pub(crate) fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn main() {
    // Service, agent and elevated helper modes never open the window.
    if let Some(code) = service::run_cli() {
        std::process::exit(code);
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Launched by a link while the app already runs: hand it over and quit.
    let link = deeplink::link_arg().map(|raw| deeplink::parse(&raw.to_ascii_lowercase()));
    if let Some(id) = &link {
        if deeplink::forward_to_running(id.as_deref()) {
            return;
        }
    }
    let link = link.flatten();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![MINIMIZED_ARG]),
        ))
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            let identity = identity::load_or_create(&dir)?;
            let config = config::ConfigStore::load(&dir);
            let settings = config.current.settings.clone();
            app.manage(state::AppState::new(identity, config, dir));
            service::client::init(app.handle());

            let show = MenuItem::with_id(app, "show", "Pokaż RemoteConn", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Zakończ", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("RemoteConn")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            if std::env::args().any(|a| a == MINIMIZED_ARG) {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            // With the service installed the agent shares; the window shows it.
            if settings.share_on_start && service::client::active(app.handle()).is_none() {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = host_cmds::start_host(&handle).await {
                        tracing::warn!("sharing on start failed: {e}");
                    }
                });
            }

            if let Err(e) = deeplink::register_scheme() {
                tracing::warn!("cannot register {}:// links: {e}", deeplink::SCHEME);
            }
            deeplink::listen(app.handle().clone());
            if let Some(id) = link {
                deeplink::deliver(app.handle(), id);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<state::AppState>();
                if state.settings().close_to_tray {
                    // Keep running (and sharing) in the tray.
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::device_info,
            settings_cmds::get_settings,
            settings_cmds::set_settings,
            settings_cmds::set_permanent_password,
            settings_cmds::remove_trusted_device,
            settings_cmds::test_server,
            settings_cmds::forget_server_pin,
            settings_cmds::import_config_code,
            settings_cmds::autostart_enabled,
            host_cmds::host_start,
            host_cmds::host_status,
            host_cmds::host_stop,
            host_cmds::host_new_password,
            host_cmds::host_respond,
            host_cmds::host_send_chat,
            host_cmds::host_disconnect,
            host_cmds::host_send_files,
            host_cmds::host_cancel_transfer,
            commands::viewer_connect,
            commands::viewer_disconnect,
            commands::viewer_mouse,
            commands::viewer_key,
            commands::viewer_select_monitor,
            commands::viewer_request_keyframe,
            commands::viewer_send_chat,
            commands::viewer_send_files,
            commands::viewer_cancel_transfer,
            commands::viewer_set_quality,
            commands::reveal_path,
            commands::take_pending_link,
            commands::viewer_special_key,
            account_cmds::account_status,
            account_cmds::account_register,
            account_cmds::account_login,
            account_cmds::account_logout,
            account_cmds::account_devices,
            account_cmds::account_revoke,
            account_cmds::account_change_password,
            account_cmds::account_set_host_trust,
            service::client::service_status,
            service::client::service_install,
            service::client::service_uninstall,
            service::client::service_sync,
        ])
        .build(tauri::generate_context!())
        .expect("error while building RemoteConn");

    app.run(|_app, _event| {
        // macOS delivers links to the running app as an event.
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Opened { urls } = &_event {
            for url in urls {
                if let Some(id) = deeplink::parse(url.as_str()) {
                    deeplink::deliver(_app, id);
                }
            }
        }
    });
}
