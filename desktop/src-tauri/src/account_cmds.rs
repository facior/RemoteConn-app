//! Account: sign-up, login, "my computers", revoking devices.
//!
//! Logging in on a device does two things (see rc-crypto `account`):
//! - this device gets a certificate, so it can open the account's computers
//!   without their passwords;
//! - this computer trusts the account (unless switched off), so the
//!   account's other devices can open it without a password.

use rc_rendezvous::{AccountLogin, AccountOp, RendezvousError, ServerInfo};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::{configured_server, CmdResult, ACCESS_DENIED_MESSAGE, PIN_MISMATCH_MESSAGE};
use crate::config::{hex, parse_hex32};
use crate::host_cmds::{restart_host_if_running, unix_now};
use crate::state::AppState;

const MIN_ACCOUNT_PASSWORD: usize = 10;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    logged_in: bool,
    name: String,
    display_name: String,
    server: String,
    host_trust: bool,
    disabled: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDeviceDto {
    /// Formatted ID, empty for devices that never shared their screen.
    device_id: String,
    name: String,
    online: bool,
    /// Hex Noise key (identifies the device for revoking).
    public_key: String,
    fingerprint: String,
    this_device: bool,
    added_unix: i64,
}

/// Tells the windows (home screen, settings) that the account changed.
fn changed(app: &AppHandle) -> AccountView {
    let current = view(&app.state::<AppState>());
    let _ = app.emit("account://changed", current.clone());
    current
}

fn view(state: &AppState) -> AccountView {
    let a = state.account();
    AccountView {
        logged_in: a.logged_in(),
        name: a.name,
        display_name: a.display_name,
        server: a.server,
        host_trust: a.host_trust,
        disabled: a.disabled,
    }
}

fn message(e: RendezvousError) -> String {
    match e {
        RendezvousError::Server(m) => format!("{}{}", m[..1].to_uppercase(), &m[1..]),
        RendezvousError::PinMismatch => PIN_MISMATCH_MESSAGE.into(),
        RendezvousError::AccessDenied(_) => ACCESS_DENIED_MESSAGE.into(),
        other => format!("Serwer nie odpowiada: {other}"),
    }
}

async fn server(state: &AppState) -> CmdResult<(String, ServerInfo, rc_transport::Endpoint)> {
    let (name, info) = configured_server(state)
        .await?
        .ok_or("Konta działają przez serwer ID: ustaw go w Ustawienia → Sieć.")?;
    let endpoint = rc_transport::client_endpoint(rc_transport::any_addr_for(info.addr))
        .map_err(|e| e.to_string())?;
    Ok((name, info, endpoint))
}

/// The account's server, which must still be the configured one.
async fn account_server(
    state: &AppState,
) -> CmdResult<(ServerInfo, rc_transport::Endpoint, Vec<u8>)> {
    let account = state.account();
    if !account.logged_in() {
        return Err("Nie jesteś zalogowany na konto.".into());
    }
    let (name, info, endpoint) = server(state).await?;
    if crate::config::pin_key(&name) != crate::config::pin_key(&account.server) {
        return Err(format!(
            "Konto jest na serwerze „{}”, a w ustawieniach jest inny. Wyloguj się i zaloguj ponownie.",
            account.server
        ));
    }
    let token = state
        .account_token()
        .ok_or("Sesja konta wygasła. Zaloguj się ponownie.")?;
    Ok((info, endpoint, token))
}

#[tauri::command]
pub fn account_status(state: State<'_, AppState>) -> AccountView {
    view(&state)
}

fn check_password(password: &str) -> CmdResult<()> {
    if password.chars().count() < MIN_ACCOUNT_PASSWORD {
        return Err(format!(
            "Hasło konta musi mieć co najmniej {MIN_ACCOUNT_PASSWORD} znaków."
        ));
    }
    Ok(())
}

#[tauri::command]
pub async fn account_register(
    app: AppHandle,
    name: String,
    password: String,
    invite: String,
    display_name: String,
) -> CmdResult<AccountView> {
    check_password(&password)?;
    let state = app.state::<AppState>();
    let (server_name, info, endpoint) = server(&state).await?;
    let login =
        rc_rendezvous::account_register(&endpoint, &info, &name, &password, &invite, &display_name)
            .await
            .map_err(message)?;
    finish_login(&app, server_name, login).await
}

#[tauri::command]
pub async fn account_login(
    app: AppHandle,
    name: String,
    password: String,
) -> CmdResult<AccountView> {
    let state = app.state::<AppState>();
    let (server_name, info, endpoint) = server(&state).await?;
    let login = rc_rendezvous::account_login(&endpoint, &info, &name, &password)
        .await
        .map_err(message)?;
    finish_login(&app, server_name, login).await
}

async fn finish_login(
    app: &AppHandle,
    server_name: String,
    login: AccountLogin,
) -> CmdResult<AccountView> {
    let state = app.state::<AppState>();
    state.pin_server_if_new(&server_name, &login.server_fingerprint);
    // The private key is used here once and dropped with `login`.
    let cert = login
        .key
        .issue_cert(&login.name, &state.identity.noise_public(), unix_now());
    let public_key = login.key.public();
    state
        .set_account_token(Some(&login.token))
        .map_err(|e| format!("Nie udało się zapisać sesji konta: {e}"))?;
    let previous = state.account();
    state.update_account(|a| {
        let same_account = a.name == login.name && a.public_key == hex(&public_key);
        *a = crate::config::AccountConfig {
            name: login.name.clone(),
            display_name: login.display_name.clone(),
            server: server_name,
            public_key: hex(&public_key),
            revoked: if same_account {
                a.revoked.clone()
            } else {
                Vec::new()
            },
            disabled: false,
            cert: hex(&cert),
            host_trust: if previous.logged_in() {
                previous.host_trust
            } else {
                true
            },
            bound_device_id: String::new(),
        };
    });
    drop(login);
    if let Err(e) = bind_this_device(&state).await {
        tracing::warn!("account: binding this device failed: {e}");
    }
    // The host registers again, now announcing the account.
    restart_host_if_running(app).await?;
    crate::service::client::sync_if_installed(app).await;
    Ok(changed(app))
}

/// Puts this device (with its current ID, if any) on the account.
pub async fn bind_this_device(state: &AppState) -> CmdResult<()> {
    let (info, endpoint, token) = account_server(state).await?;
    let account = state.account();
    let device_id = state.device_id();
    let noise = state.identity.noise_public();
    let signature = state.identity.sign(&rc_crypto::account::bind_message(
        &account.name,
        &device_id,
        &noise,
    ));
    rc_rendezvous::account_call(
        &endpoint,
        &info,
        &token,
        AccountOp::BindDevice {
            device_id: device_id.clone(),
            noise_public: noise,
            verifying_key: state.identity.verifying_key(),
            signature,
            device_name: state.device_name(),
        },
    )
    .await
    .map_err(message)?;
    state.update_account(|a| a.bound_device_id = device_id);
    Ok(())
}

#[tauri::command]
pub async fn account_logout(app: AppHandle) -> CmdResult<AccountView> {
    let state = app.state::<AppState>();
    if let Ok((info, endpoint, token)) = account_server(&state).await {
        let noise = state.identity.noise_public();
        let _ =
            rc_rendezvous::account_call(&endpoint, &info, &token, AccountOp::UnbindDevice(noise))
                .await;
        let _ = rc_rendezvous::account_call(&endpoint, &info, &token, AccountOp::Logout).await;
    }
    let _ = state.set_account_token(None);
    state.update_account(|a| *a = crate::config::AccountConfig::default());
    restart_host_if_running(&app).await?;
    crate::service::client::sync_if_installed(&app).await;
    Ok(changed(&app))
}

#[tauri::command]
pub async fn account_devices(state: State<'_, AppState>) -> CmdResult<Vec<AccountDeviceDto>> {
    let (info, endpoint, token) = account_server(&state).await?;
    let devices = rc_rendezvous::account_call(&endpoint, &info, &token, AccountOp::ListDevices)
        .await
        .map_err(message)?;
    let own = state.identity.noise_public();
    let mut list: Vec<AccountDeviceDto> = devices
        .into_iter()
        .filter_map(|d| {
            let key: [u8; 32] = d.noise_public.as_slice().try_into().ok()?;
            Some(AccountDeviceDto {
                device_id: rc_rendezvous::format_id(&d.device_id),
                name: rc_proto::text::clean_label(&d.name, 64),
                online: d.online,
                public_key: hex(&key),
                fingerprint: rc_crypto::identity::fingerprint(&key),
                this_device: key == own,
                added_unix: d.added_unix,
            })
        })
        .collect();
    list.sort_by_key(|d| (d.this_device, !d.online, d.name.to_lowercase()));
    Ok(list)
}

#[tauri::command]
pub async fn account_revoke(state: State<'_, AppState>, public_key: String) -> CmdResult<()> {
    let key = parse_hex32(&public_key).ok_or("Nieprawidłowy klucz urządzenia.")?;
    if key == state.identity.noise_public() {
        return Err("Tego urządzenia nie możesz zablokować z niego samego; wyloguj się.".into());
    }
    let (info, endpoint, token) = account_server(&state).await?;
    rc_rendezvous::account_call(&endpoint, &info, &token, AccountOp::RevokeDevice(key))
        .await
        .map_err(message)?;
    Ok(())
}

#[tauri::command]
pub async fn account_change_password(
    app: AppHandle,
    old_password: String,
    new_password: String,
) -> CmdResult<AccountView> {
    check_password(&new_password)?;
    let state = app.state::<AppState>();
    let (info, endpoint, token) = account_server(&state).await?;
    let name = state.account().name;
    let key = rc_rendezvous::change_password(
        &endpoint,
        &info,
        &token,
        &name,
        &old_password,
        &new_password,
    )
    .await
    .map_err(message)?;
    let cert = key.issue_cert(&name, &state.identity.noise_public(), unix_now());
    let public_key = key.public();
    drop(key);
    state.update_account(|a| {
        a.public_key = hex(&public_key);
        a.cert = hex(&cert);
    });
    crate::service::client::sync_if_installed(&app).await;
    Ok(changed(&app))
}

/// Whether devices of the account may open this computer without password.
#[tauri::command]
pub async fn account_set_host_trust(app: AppHandle, enabled: bool) -> CmdResult<AccountView> {
    let state = app.state::<AppState>();
    state.update_account(|a| a.host_trust = enabled);
    crate::service::client::sync_if_installed(&app).await;
    Ok(changed(&app))
}
