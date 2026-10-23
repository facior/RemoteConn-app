//! The app window's side: talks to the agent while the service is installed,
//! and installs / configures the service through UAC prompts.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::WriteHalf;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::oneshot;
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use super::pipe::{write_line, AgentCmd, LineReader, Message, Request};
use super::{paths, win, SERVICE_NAME};
use crate::commands::CmdResult;
use crate::host_cmds::{HostStatus, PermissionsDto};
use crate::state::AppState;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>;

pub struct ServiceClient {
    writer: tokio::sync::Mutex<Option<WriteHalf<NamedPipeClient>>>,
    pending: Pending,
    next_id: AtomicU64,
    connected: AtomicBool,
    stopped: AtomicBool,
}

/// Managed by Tauri: the client while the service is installed.
#[derive(Default)]
pub struct ServiceMode(Mutex<Option<Arc<ServiceClient>>>);

/// The agent client if the service is installed (connected or not).
pub fn active(app: &AppHandle) -> Option<Arc<ServiceClient>> {
    app.try_state::<ServiceMode>()?.0.lock().unwrap().clone()
}

/// State of the service as Windows reports it.
fn service_state() -> Option<ServiceState> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
        .ok()?;
    service.query_status().ok().map(|s| s.current_state)
}

pub fn installed() -> bool {
    service_state().is_some()
}

/// Called at app start: with the service installed, the window shows the
/// agent's sharing instead of starting its own.
pub fn init(app: &AppHandle) {
    app.manage(ServiceMode::default());
    // RC_SERVICE_CLIENT_TEST: talk to an agent started by hand (tests).
    if installed() || std::env::var_os("RC_SERVICE_CLIENT_TEST").is_some() {
        enable(app);
    }
}

fn enable(app: &AppHandle) {
    let mode = app.state::<ServiceMode>();
    let mut slot = mode.0.lock().unwrap();
    if slot.is_some() {
        return;
    }
    let client = Arc::new(ServiceClient {
        writer: tokio::sync::Mutex::new(None),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        connected: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
    });
    *slot = Some(client.clone());
    tauri::async_runtime::spawn(connect_loop(app.clone(), client));
}

fn disable(app: &AppHandle) {
    if let Some(client) = app.state::<ServiceMode>().0.lock().unwrap().take() {
        client.stopped.store(true, Ordering::Relaxed);
    }
}

async fn connect_loop(app: AppHandle, client: Arc<ServiceClient>) {
    let name = paths::agent_pipe(win::current_session());
    while !client.stopped.load(Ordering::Relaxed) {
        let pipe = match ClientOptions::new().open(&name) {
            Ok(p) => p,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let (read, write) = tokio::io::split(pipe);
        *client.writer.lock().await = Some(write);
        client.connected.store(true, Ordering::Relaxed);
        let _ = app.emit("service://state", json!({ "connected": true }));
        // The agent may have restarted: refresh what the window shows. In a
        // task: the reply is read by the loop below.
        {
            let (app, client) = (app.clone(), client.clone());
            tauri::async_runtime::spawn(async move {
                if let Ok(status) = client.status().await {
                    let _ = app.emit("host://restarted", status);
                }
            });
        }

        let mut reader = LineReader::new(read);
        while let Some(message) = reader.next::<Message>().await {
            match message {
                Message::Event { name, payload } => {
                    let _ = app.emit(&name, payload);
                }
                Message::Reply { id, result } => {
                    if let Some(tx) = client.pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(result);
                    }
                }
            }
            if client.stopped.load(Ordering::Relaxed) {
                break;
            }
        }
        client.connected.store(false, Ordering::Relaxed);
        *client.writer.lock().await = None;
        for (_, tx) in client.pending.lock().unwrap().drain() {
            let _ = tx.send(Err("Połączenie z usługą zostało przerwane.".into()));
        }
        let _ = app.emit("service://state", json!({ "connected": false }));
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

impl ServiceClient {
    async fn request(&self, cmd: AgentCmd) -> CmdResult<Value> {
        if !self.connected.load(Ordering::Relaxed) {
            return Err("Łączenie z usługą RemoteConn… Spróbuj za chwilę.".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        {
            let mut writer = self.writer.lock().await;
            let w = writer.as_mut().ok_or("Brak połączenia z usługą.")?;
            write_line(w, &Request { id, cmd })
                .await
                .map_err(|e| format!("Usługa: {e}"))?;
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            _ => {
                self.pending.lock().unwrap().remove(&id);
                Err("Usługa nie odpowiada.".into())
            }
        }
    }

    pub async fn status(&self) -> CmdResult<HostStatus> {
        serde_json::from_value(self.request(AgentCmd::Status).await?).map_err(|e| e.to_string())
    }

    pub async fn new_password(&self) -> CmdResult<String> {
        serde_json::from_value(self.request(AgentCmd::NewPassword).await?)
            .map_err(|e| e.to_string())
    }

    pub async fn respond(
        &self,
        id: u64,
        accept: bool,
        permissions: PermissionsDto,
        trust: bool,
    ) -> CmdResult<()> {
        self.request(AgentCmd::Respond {
            id,
            accept,
            permissions,
            trust,
        })
        .await
        .map(|_| ())
    }

    pub async fn chat(&self, session_id: u64, text: String) -> CmdResult<()> {
        self.request(AgentCmd::Chat { session_id, text })
            .await
            .map(|_| ())
    }

    pub async fn disconnect(&self, session_id: u64) -> CmdResult<()> {
        self.request(AgentCmd::Disconnect { session_id })
            .await
            .map(|_| ())
    }

    pub async fn send_files(&self, session_id: u64, paths: Vec<String>) -> CmdResult<()> {
        self.request(AgentCmd::SendFiles { session_id, paths })
            .await
            .map(|_| ())
    }

    pub async fn cancel_transfer(&self, session_id: u64, id: u64) -> CmdResult<()> {
        self.request(AgentCmd::CancelTransfer { session_id, id })
            .await
            .map(|_| ())
    }
}

/// Runs this program elevated in a helper mode; the UAC prompt appears.
async fn elevated(mode: &'static str) -> CmdResult<()> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let code = tauri::async_runtime::spawn_blocking(move || win::run_elevated(&exe, mode))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                "Anulowano: potrzebna jest zgoda administratora.".to_string()
            } else {
                e.to_string()
            }
        })?;
    if code == 0 {
        Ok(())
    } else {
        Err(std::fs::read_to_string(super::install::result_file())
            .unwrap_or_else(|_| format!("Operacja nie powiodła się (kod {code}).")))
    }
}

/// After a change of host settings: hand them to the service (UAC prompt).
/// The outcome is reported as a `service://sync` event.
pub async fn sync_if_installed(app: &AppHandle) {
    if active(app).is_none() {
        return;
    }
    let result = elevated("--service-sync").await;
    if let Err(e) = &result {
        tracing::warn!("service sync: {e}");
    }
    let _ = app.emit(
        "service://sync",
        json!({ "ok": result.is_ok(), "error": result.err().unwrap_or_default() }),
    );
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatusDto {
    supported: bool,
    installed: bool,
    running: bool,
    connected: bool,
}

#[tauri::command]
pub fn service_status(app: AppHandle) -> ServiceStatusDto {
    let state = service_state();
    ServiceStatusDto {
        supported: true,
        installed: state.is_some(),
        running: state == Some(ServiceState::Running),
        connected: active(&app).is_some_and(|c| c.connected.load(Ordering::Relaxed)),
    }
}

#[tauri::command]
pub async fn service_install(app: AppHandle) -> CmdResult<ServiceStatusDto> {
    elevated("--install-service").await?;
    // The window's own sharing stops; the agent takes over.
    let state = app.state::<AppState>();
    state.host.lock().await.take();
    crate::host_cmds::reject_pending(&state);
    enable(&app);
    Ok(service_status(app))
}

#[tauri::command]
pub async fn service_uninstall(app: AppHandle) -> CmdResult<ServiceStatusDto> {
    elevated("--uninstall-service").await?;
    disable(&app);
    let _ = app.emit(
        "service://state",
        json!({ "connected": false, "removed": true }),
    );
    Ok(service_status(app))
}

#[tauri::command]
pub async fn service_sync() -> CmdResult<()> {
    elevated("--service-sync").await
}
