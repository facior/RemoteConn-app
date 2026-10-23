//! Settings screen.

use std::time::Instant;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_autostart::ManagerExt;

use rc_rendezvous::{RendezvousError, ServerInfo};

use crate::commands::{resolve_server, CmdResult, ACCESS_DENIED_MESSAGE, PIN_MISMATCH_MESSAGE};
use crate::config::Settings;
use crate::host_cmds::restart_host_if_running;
use crate::state::AppState;

const MIN_PERMANENT_PASSWORD: usize = 8;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    settings: Settings,
    permanent_password_set: bool,
    /// Shown as the placeholder for the device name.
    computer_name: String,
    /// Shown as the placeholder for the download folder.
    default_download_dir: String,
    /// Pinned certificate of the configured server (short form), if any.
    server_fingerprint: Option<String>,
}

/// `3f2a-91c0-77de-0b14`: the first 8 bytes, like device fingerprints.
pub fn short_fingerprint(fp: &[u8; 32]) -> String {
    fp[..8]
        .chunks(2)
        .map(|c| format!("{:02x}{:02x}", c[0], c[1]))
        .collect::<Vec<_>>()
        .join("-")
}

fn view(state: &AppState) -> SettingsView {
    let settings = state.settings();
    SettingsView {
        server_fingerprint: state
            .server_pin(&settings.rendezvous_server)
            .map(|fp| short_fingerprint(&fp)),
        settings,
        permanent_password_set: state.has_permanent_password(),
        computer_name: state.computer_name().to_string(),
        default_download_dir: rc_session::files::default_download_dir()
            .display()
            .to_string(),
    }
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> SettingsView {
    view(&state)
}

/// Validates and saves; restarts sharing when an endpoint-level setting
/// changed (port, server, device name, download folder).
#[tauri::command]
pub async fn set_settings(app: AppHandle, settings: Settings) -> CmdResult<SettingsView> {
    let state = app.state::<AppState>();
    let new = settings.normalized();
    if !new.rendezvous_server.is_empty() {
        resolve_server(&new.rendezvous_server).await?;
    }
    if !new.download_dir.is_empty() {
        std::fs::create_dir_all(&new.download_dir)
            .map_err(|e| format!("Nie można użyć folderu „{}”: {e}", new.download_dir))?;
    }
    let old = {
        let mut config = state.config.lock().unwrap();
        let old = std::mem::replace(&mut config.current.settings, new.clone());
        config.save();
        old
    };

    if old.autostart != new.autostart {
        let autolaunch = app.autolaunch();
        let result = if new.autostart {
            autolaunch.enable()
        } else {
            autolaunch.disable()
        };
        if let Err(e) = result {
            tracing::warn!("autostart: {e}");
        }
    }
    if old.password_length != new.password_length {
        // The current one-time password should follow the new length now.
        let password = state.rotate_password();
        let _ = app.emit("host://password", password);
    }
    let needs_restart = old.listen_port != new.listen_port
        || old.rendezvous_server != new.rendezvous_server
        || old.server_access_key != new.server_access_key
        || old.device_name != new.device_name
        || old.download_dir != new.download_dir;
    if needs_restart {
        restart_host_if_running(&app).await?;
    }
    // The service shares with these settings too (UAC prompt).
    let host_changed = needs_restart
        || old.password_length != new.password_length
        || old.rotate_password_after_session != new.rotate_password_after_session
        || old.confirm_permanent_password != new.confirm_permanent_password
        || old.default_permissions != new.default_permissions
        || old.trusted_devices != new.trusted_devices;
    if host_changed {
        crate::service::client::sync_if_installed(&app).await;
    }
    Ok(view(&state))
}

/// Sets (`Some`) or removes (`None`) the permanent password. Applies to new
/// connections immediately.
#[tauri::command]
pub async fn set_permanent_password(
    app: AppHandle,
    password: Option<String>,
) -> CmdResult<SettingsView> {
    let state = app.state::<AppState>();
    if let Some(p) = &password {
        if p.chars().count() < MIN_PERMANENT_PASSWORD {
            return Err(format!(
                "Stałe hasło musi mieć co najmniej {MIN_PERMANENT_PASSWORD} znaków."
            ));
        }
    }
    state
        .set_permanent_password(password)
        .map_err(|e| format!("Nie udało się zapisać hasła: {e}"))?;
    crate::service::client::sync_if_installed(&app).await;
    Ok(view(&state))
}

#[tauri::command]
pub fn remove_trusted_device(state: State<'_, AppState>, fingerprint: String) -> SettingsView {
    {
        let mut config = state.config.lock().unwrap();
        config
            .current
            .settings
            .trusted_devices
            .retain(|t| t.fingerprint != fingerprint);
        config.save();
    }
    view(&state)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerTest {
    latency_ms: u32,
    fingerprint: String,
    /// True when this test trusted the server for the first time.
    newly_pinned: bool,
}

/// Contacts the rendezvous server with the given access key, checking the
/// pinned certificate (or pinning it on first contact).
#[tauri::command]
pub async fn test_server(
    state: State<'_, AppState>,
    server: String,
    access_key: String,
) -> CmdResult<ServerTest> {
    let server = server.trim().to_string();
    let addr = resolve_server(&server).await?;
    let pin = state.server_pin(&server);
    let info = ServerInfo {
        addr,
        access_key: access_key.trim().to_string(),
        pin,
    };
    let endpoint = rc_transport::client_endpoint(rc_transport::any_addr_for(addr))
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    let fingerprint = rc_rendezvous::ping(&endpoint, &info)
        .await
        .map_err(|e| match e {
            RendezvousError::PinMismatch => PIN_MISMATCH_MESSAGE.to_string(),
            RendezvousError::AccessDenied(_) => ACCESS_DENIED_MESSAGE.to_string(),
            other => format!("Serwer nie odpowiada: {other}"),
        })?;
    let latency_ms = started.elapsed().as_millis() as u32;
    state.pin_server_if_new(&server, &fingerprint);
    Ok(ServerTest {
        latency_ms,
        fingerprint: short_fingerprint(&fingerprint),
        newly_pinned: pin.is_none(),
    })
}

/// Forgets the pinned certificate of `server` (after a deliberate server
/// reinstall); the next connection trusts whatever it finds.
#[tauri::command]
pub fn forget_server_pin(state: State<'_, AppState>, server: String) -> SettingsView {
    state.forget_server_pin(&server);
    view(&state)
}
/// Autostart as registered in the OS (it can be changed outside the app).
#[tauri::command]
pub fn autostart_enabled(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}
