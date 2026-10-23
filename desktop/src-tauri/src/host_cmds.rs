//! Sharing this computer: start/stop, accept dialog, sessions.
//!
//! The host runs either in the app's own process or, when the service is
//! installed, in the service agent (see [`crate::service`]). [`HostEnv`] is
//! what differs between the two; the UI events are the same.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;

use rc_proto::{special_key, Permissions};
use rc_rendezvous::{format_id, RendezvousErrorKind};
use rc_session::files::{OutgoingFile, TransferEvent};
use rc_session::host::{self, HostConfig, HostHooks, RendezvousStatus, SessionCmd};
use rc_session::{AccountTrust, AuthMethod, EncoderSettings, PeerInfo};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::oneshot;

use crate::commands::{
    configured_server, err, to_paths, CmdResult, PeerDto, TransferDto, ACCESS_DENIED_MESSAGE,
    PIN_MISMATCH_MESSAGE,
};
use crate::config::{hex, DefaultPermissions, TrustedDevice};
use crate::state::{AppState, PendingRequest, RunningHost};

/// Where the host runs.
pub trait HostEnv: Clone + Send + Sync + 'static {
    fn app_state(&self) -> &AppState;
    fn emit_json(&self, event: &str, payload: serde_json::Value);
    /// Whether someone can answer the accept dialog right now.
    fn ui_available(&self) -> bool {
        true
    }
    /// Ctrl+Alt+Del from a viewer (only the service can do it).
    fn send_sas(&self) {}
    /// Files the host user chose to send. The service agent opens them as
    /// the logged-in user instead of with its own (SYSTEM) rights.
    fn open_outgoing(&self, paths: Vec<PathBuf>) -> Vec<OutgoingFile> {
        paths.into_iter().map(OutgoingFile::Path).collect()
    }
    /// Where received files go; `None` refuses incoming files (no user).
    fn download_dir(&self) -> Option<PathBuf> {
        Some(self.app_state().settings().download_dir())
    }
}

impl HostEnv for AppHandle {
    fn app_state(&self) -> &AppState {
        Manager::state::<AppState>(self).inner()
    }

    fn emit_json(&self, event: &str, payload: serde_json::Value) {
        let _ = self.emit(event, payload);
    }
}

pub fn permissions_from(p: DefaultPermissions) -> Permissions {
    Permissions {
        view_screen: p.view_screen,
        control_input: p.control_input,
        clipboard: p.clipboard,
        file_transfer: p.file_transfer,
        chat: true,
        ..Default::default()
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    pub addresses: Vec<String>,
    pub port: u16,
    pub password: String,
    /// Whether a rendezvous server is configured (an ID will appear).
    pub uses_rendezvous: bool,
    /// Last known ID, formatted; confirmed by a `host://rendezvous` event.
    pub device_id: String,
    pub permanent_password_set: bool,
    /// Sharing runs in the background service (always on).
    #[serde(default)]
    pub service: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RendezvousEvent {
    online: bool,
    device_id: String,
    reason: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct IncomingRequest {
    id: u64,
    peer: PeerDto,
    /// Initial state of the permission checkboxes.
    permissions: DefaultPermissions,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AutoAccepted {
    peer: PeerDto,
    /// "permanentPassword" | "trustedDevice" | "account"
    reason: &'static str,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase", tag = "state")]
enum HostSessionEvent {
    Started { peer: PeerDto },
    Ended { peer: PeerDto, reason: String },
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct HostChat {
    session_id: u64,
    text: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct HostTransfer {
    session_id: u64,
    transfer: TransferDto,
}

struct UiHooks<E: HostEnv>(E);

impl<E: HostEnv> UiHooks<E> {
    fn emit(&self, event: &str, payload: impl Serialize) {
        match serde_json::to_value(payload) {
            Ok(value) => self.0.emit_json(event, value),
            Err(e) => tracing::warn!("event {event}: {e}"),
        }
    }
}

impl<E: HostEnv> HostHooks for UiHooks<E> {
    fn approve(&self, peer: &PeerInfo) -> oneshot::Receiver<Option<Permissions>> {
        let (tx, rx) = oneshot::channel();
        let state = self.0.app_state();
        let settings = state.settings();
        let auto_reason = if peer.auth == AuthMethod::Account {
            Some("account")
        } else if peer.auth == AuthMethod::PermanentPassword && !settings.confirm_permanent_password
        {
            Some("permanentPassword")
        } else if settings
            .trusted_devices
            .iter()
            .any(|t| t.public_key == hex(&peer.public_key))
        {
            Some("trustedDevice")
        } else {
            None
        };
        if let Some(reason) = auto_reason {
            let mut permissions = permissions_from(settings.default_permissions);
            if self.0.download_dir().is_none() {
                permissions.file_transfer = false;
            }
            let _ = tx.send(Some(permissions));
            self.emit(
                "host://auto-accepted",
                AutoAccepted {
                    peer: peer.into(),
                    reason,
                },
            );
            return rx;
        }
        if !self.0.ui_available() {
            // Nobody to ask (login screen, no app window): refuse.
            let _ = tx.send(None);
            return rx;
        }
        let id = state.next_request_id();
        state.pending.lock().unwrap().insert(
            id,
            PendingRequest {
                reply: tx,
                public_key: peer.public_key,
                fingerprint: peer.fingerprint.clone(),
                device_name: peer.device_name.clone(),
            },
        );
        self.emit(
            "host://request",
            IncomingRequest {
                id,
                peer: peer.into(),
                permissions: settings.default_permissions,
            },
        );
        rx
    }

    fn session_started(&self, peer: &PeerInfo) {
        self.emit(
            "host://session",
            HostSessionEvent::Started { peer: peer.into() },
        );
    }

    fn session_ended(&self, peer: &PeerInfo, reason: &str) {
        self.emit(
            "host://session",
            HostSessionEvent::Ended {
                peer: peer.into(),
                reason: reason.into(),
            },
        );
        let state = self.0.app_state();
        if state.settings().rotate_password_after_session {
            let password = state.rotate_password();
            self.emit("host://password", password);
        }
    }

    fn rendezvous(&self, status: RendezvousStatus) {
        let state = self.0.app_state();
        let event = match status {
            RendezvousStatus::Online {
                device_id,
                server_fingerprint,
            } => {
                {
                    let mut config = state.config.lock().unwrap();
                    if config.current.device_id != device_id {
                        config.current.device_id = device_id.clone();
                        config.save();
                    }
                }
                state.pin_server_if_new(&state.settings().rendezvous_server, &server_fingerprint);
                let account = state.account();
                if account.logged_in() && account.bound_device_id != device_id {
                    // Put the (new) ID on the account so it shows up there.
                    let env = self.0.clone();
                    tokio::spawn(async move {
                        if let Err(e) = crate::account_cmds::bind_this_device(env.app_state()).await
                        {
                            tracing::warn!("account: binding this device failed: {e}");
                        }
                    });
                }
                RendezvousEvent {
                    online: true,
                    device_id: format_id(&device_id),
                    reason: String::new(),
                }
            }
            RendezvousStatus::Offline { kind, reason } => RendezvousEvent {
                online: false,
                device_id: format_id(&state.device_id()),
                reason: match kind {
                    RendezvousErrorKind::PinMismatch => PIN_MISMATCH_MESSAGE.into(),
                    RendezvousErrorKind::AccessDenied => ACCESS_DENIED_MESSAGE.into(),
                    _ => reason,
                },
            },
        };
        self.emit("host://rendezvous", event);
    }

    fn transfer(&self, peer: &PeerInfo, event: &TransferEvent) {
        self.emit(
            "host://transfer",
            HostTransfer {
                session_id: peer.session_id,
                transfer: event.into(),
            },
        );
    }

    fn chat(&self, peer: &PeerInfo, text: &str) {
        self.emit(
            "host://chat",
            HostChat {
                session_id: peer.session_id,
                text: text.into(),
            },
        );
    }

    fn account_updated(&self, trust: &AccountTrust) {
        let trust = trust.clone();
        // The host already enforces it; remember it for the next start.
        let state = self.0.app_state();
        let mut config = state.config.lock().unwrap();
        let account = &mut config.current.account;
        if account.name == trust.name {
            account.public_key = hex(&trust.public_key);
            account.revoked = trust.revoked.iter().map(|k| hex(k)).collect();
            account.disabled = trust.disabled;
            config.save();
        }
    }

    fn special_key(&self, peer: &PeerInfo, kind: special_key::Kind) {
        if kind == special_key::Kind::CtrlAltDel {
            tracing::info!(peer = %peer.device_name, "Ctrl+Alt+Del requested");
            self.0.send_sas();
        }
    }
}

/// LAN addresses to show next to the password.
pub fn local_addresses() -> Vec<String> {
    // Connecting a UDP socket sends nothing; it only picks the outgoing interface.
    let primary = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|_| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip())
        .ok()
        .filter(|ip| !ip.is_unspecified());
    match primary {
        Some(ip) => vec![ip.to_string()],
        None => vec![IpAddr::from([127, 0, 0, 1]).to_string()],
    }
}

pub fn status_of(state: &AppState, running: &RunningHost, uses_rendezvous: bool) -> HostStatus {
    HostStatus {
        addresses: local_addresses(),
        port: running.handle.local_addr.port(),
        password: state.password.read().unwrap().clone(),
        uses_rendezvous,
        device_id: format_id(&state.device_id()),
        permanent_password_set: state.has_permanent_password(),
        service: false,
    }
}

/// Starts sharing (no-op if already running) with the current settings.
pub async fn start_host<E: HostEnv>(env: &E) -> CmdResult<HostStatus> {
    let state = env.app_state();
    let mut host = state.host.lock().await;
    let rendezvous = configured_server(state).await?;
    if host.is_none() {
        let settings = state.settings();
        let dir_env = env.clone();
        // Without a logged-in user (service at the login screen) incoming
        // files are refused in approve(); this path is never written then.
        let download_dir: Arc<dyn Fn() -> PathBuf + Send + Sync> = Arc::new(move || {
            dir_env
                .download_dir()
                .unwrap_or_else(|| std::env::temp_dir().join("RemoteConn-no-user"))
        });
        // After a restart the old socket is released asynchronously, so the
        // port can be busy for a moment.
        let mut attempt = 0;
        let handle = loop {
            let result = host::start(
                HostConfig {
                    identity: state.identity.clone(),
                    device_name: state.device_name(),
                    device_id: state.device_id(),
                    password: state.password.clone(),
                    permanent_password: state.permanent_password.clone(),
                    bind: SocketAddr::from(([0, 0, 0, 0], settings.listen_port)),
                    encoder: EncoderSettings::default(),
                    rendezvous: rendezvous.clone().map(|(_, info)| info),
                    download_dir: download_dir.clone(),
                    account: state.account_trust.clone(),
                },
                Arc::new(UiHooks(env.clone())),
            )
            .await;
            match result {
                Ok(handle) => break handle,
                Err(e) if attempt < 15 && is_addr_in_use(&e) => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                Err(e) => return Err(format!("Nie udało się uruchomić udostępniania: {e}")),
            }
        };
        *host = Some(RunningHost { handle });
    }
    let running = host.as_ref().expect("host running");
    Ok(status_of(state, running, rendezvous.is_some()))
}

fn is_addr_in_use(e: &rc_session::SessionError) -> bool {
    matches!(
        e,
        rc_session::SessionError::Transport(rc_transport::TransportError::Io(io))
            if io.kind() == std::io::ErrorKind::AddrInUse
    )
}

/// Applies settings that need a new endpoint (port, server, name, folder,
/// account). Active sessions end; the one-time password is kept.
pub async fn restart_host_if_running<E: HostEnv>(env: &E) -> CmdResult<()> {
    let state = env.app_state();
    if state.host.lock().await.take().is_none() {
        return Ok(());
    }
    reject_pending(state);
    let status = start_host(env).await?;
    match serde_json::to_value(status) {
        Ok(v) => env.emit_json("host://restarted", v),
        Err(e) => tracing::warn!("host://restarted: {e}"),
    }
    Ok(())
}

pub fn reject_pending(state: &AppState) {
    for (_, req) in state.pending.lock().unwrap().drain() {
        let _ = req.reply.send(None);
    }
}

// ---------------------------------------------------------------------------
// Commands. With the service installed they go to the service agent.
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn host_start(app: AppHandle) -> CmdResult<HostStatus> {
    if let Some(client) = crate::service::client::active(&app) {
        return client.status().await;
    }
    start_host(&app).await
}

/// Current status if sharing is on (e.g. started automatically).
#[tauri::command]
pub async fn host_status(app: AppHandle) -> CmdResult<Option<HostStatus>> {
    if let Some(client) = crate::service::client::active(&app) {
        return client.status().await.map(Some);
    }
    let state = app.state::<AppState>();
    let host = state.host.lock().await;
    let uses_rendezvous = !state.settings().rendezvous_server.is_empty();
    Ok(host
        .as_ref()
        .map(|running| status_of(&state, running, uses_rendezvous)))
}

#[tauri::command]
pub async fn host_stop(app: AppHandle) -> CmdResult<()> {
    if crate::service::client::active(&app).is_some() {
        return Err(
            "Udostępnianie działa w usłudze systemowej. Wyłączysz je w Ustawienia → Usługa.".into(),
        );
    }
    let state = app.state::<AppState>();
    // Dropping the handle closes the endpoint and every session.
    state.host.lock().await.take();
    reject_pending(&state);
    Ok(())
}

#[tauri::command]
pub async fn host_new_password(app: AppHandle) -> CmdResult<String> {
    if let Some(client) = crate::service::client::active(&app) {
        return client.new_password().await;
    }
    Ok(app.state::<AppState>().rotate_password())
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsDto {
    pub view_screen: bool,
    pub control_input: bool,
    pub clipboard: bool,
    pub file_transfer: bool,
}

/// Answers an accept dialog (also used by the service agent).
pub fn respond(
    state: &AppState,
    id: u64,
    accept: bool,
    permissions: PermissionsDto,
    trust: bool,
) -> CmdResult<()> {
    let request = state
        .pending
        .lock()
        .unwrap()
        .remove(&id)
        .ok_or("Prośba o połączenie wygasła.")?;
    if accept && trust {
        let mut config = state.config.lock().unwrap();
        let trusted = &mut config.current.settings.trusted_devices;
        let public_key = hex(&request.public_key);
        if !trusted.iter().any(|t| t.public_key == public_key) {
            trusted.push(TrustedDevice {
                public_key,
                fingerprint: request.fingerprint.clone(),
                name: request.device_name.clone(),
                added_unix: unix_now(),
            });
            config.save();
        }
    }
    let decision = accept.then_some(permissions_from(DefaultPermissions {
        view_screen: permissions.view_screen,
        control_input: permissions.control_input,
        clipboard: permissions.clipboard,
        file_transfer: permissions.file_transfer,
    }));
    let _ = request.reply.send(decision);
    Ok(())
}

#[tauri::command]
pub async fn host_respond(
    app: AppHandle,
    id: u64,
    accept: bool,
    permissions: PermissionsDto,
    trust: bool,
) -> CmdResult<()> {
    if let Some(client) = crate::service::client::active(&app) {
        return client.respond(id, accept, permissions, trust).await;
    }
    respond(&app.state::<AppState>(), id, accept, permissions, trust)
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Sends a command to a session of the local host.
pub async fn command(state: &AppState, session_id: u64, cmd: SessionCmd) -> CmdResult<()> {
    let host = state.host.lock().await;
    let running = host.as_ref().ok_or("Udostępnianie jest wyłączone.")?;
    if running.handle.command(session_id, cmd) {
        Ok(())
    } else {
        Err("Sesja już się zakończyła.".into())
    }
}

#[tauri::command]
pub async fn host_send_chat(app: AppHandle, session_id: u64, text: String) -> CmdResult<()> {
    if let Some(client) = crate::service::client::active(&app) {
        return client.chat(session_id, text).await;
    }
    command(&app.state::<AppState>(), session_id, SessionCmd::Chat(text)).await
}

#[tauri::command]
pub async fn host_disconnect(app: AppHandle, session_id: u64) -> CmdResult<()> {
    if let Some(client) = crate::service::client::active(&app) {
        return client.disconnect(session_id).await;
    }
    let _ = command(&app.state::<AppState>(), session_id, SessionCmd::Disconnect).await;
    Ok(())
}

#[tauri::command]
pub async fn host_send_files(app: AppHandle, session_id: u64, paths: Vec<String>) -> CmdResult<()> {
    if let Some(client) = crate::service::client::active(&app) {
        return client.send_files(session_id, paths).await;
    }
    let files = app.open_outgoing(to_paths(paths));
    command(
        &app.state::<AppState>(),
        session_id,
        SessionCmd::SendFiles(files),
    )
    .await
}

#[tauri::command]
pub async fn host_cancel_transfer(app: AppHandle, session_id: u64, id: String) -> CmdResult<()> {
    let id: u64 = id.parse().map_err(err)?;
    if let Some(client) = crate::service::client::active(&app) {
        return client.cancel_transfer(session_id, id).await;
    }
    let _ = command(
        &app.state::<AppState>(),
        session_id,
        SessionCmd::CancelTransfer(id),
    )
    .await;
    Ok(())
}
