//! The agent: the host, started by the service as SYSTEM in the console
//! session. It shares the screen also at the login screen and UAC prompts;
//! the app window (if running) talks to it over the agent pipe.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rc_session::files::OutgoingFile;
use rc_session::host::SessionCmd;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::paths;
use super::pipe::{write_line, AgentCmd, LineReader, Message, Request, ServiceCmd};
use super::win::{self, SecurityAttrs};
use crate::config::ConfigStore;
use crate::host_cmds::{self, HostEnv};
use crate::state::AppState;

/// SYSTEM, and interactive users; the session is checked on connect too.
const AGENT_PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GRGW;;;IU)";

struct Inner {
    state: AppState,
    session: u32,
    events: broadcast::Sender<String>,
    clients: AtomicUsize,
}

#[derive(Clone)]
struct AgentEnv(Arc<Inner>);

impl HostEnv for AgentEnv {
    fn app_state(&self) -> &AppState {
        &self.0.state
    }

    fn emit_json(&self, event: &str, payload: Value) {
        let message = Message::Event {
            name: event.to_string(),
            payload,
        };
        if let Ok(line) = serde_json::to_string(&message) {
            let _ = self.0.events.send(line);
        }
    }

    fn ui_available(&self) -> bool {
        self.0.clients.load(Ordering::Relaxed) > 0
    }

    fn send_sas(&self) {
        tokio::spawn(async {
            if let Err(e) = service_request(ServiceCmd::Sas).await {
                tracing::warn!("Ctrl+Alt+Del: {e}");
            }
        });
    }

    fn open_outgoing(&self, paths: Vec<PathBuf>) -> Vec<OutgoingFile> {
        // Opened with the user's rights: the agent itself could read anything.
        win::as_user(self.0.session, || {
            paths
                .into_iter()
                .filter_map(|path| {
                    let file = std::fs::File::open(&path)
                        .map_err(|e| tracing::info!("{}: {e}", path.display()))
                        .ok()?;
                    let name = path.file_name()?.to_string_lossy().into_owned();
                    Some(OutgoingFile::Opened { name, file })
                })
                .collect()
        })
        .unwrap_or_default()
    }

    fn download_dir(&self) -> Option<PathBuf> {
        win::user_downloads(self.0.session).map(|d| d.join("RemoteConn"))
    }
}

pub fn run() -> i32 {
    super::init_file_log("agent.log");
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("runtime: {e}");
            return 1;
        }
    };
    runtime.block_on(async {
        match start().await {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!("agent failed: {e}");
                1
            }
        }
    })
}

async fn start() -> Result<(), String> {
    let dir = paths::data_dir();
    let identity = crate::identity::load_or_create(&dir).map_err(|e| e.to_string())?;
    let config = ConfigStore::load(&dir);
    let session = win::current_session();
    let (events, _) = broadcast::channel(256);
    let env = AgentEnv(Arc::new(Inner {
        state: AppState::new(identity, config, dir),
        session,
        events,
        clients: AtomicUsize::new(0),
    }));
    tracing::info!(session, "agent starting");

    // The port may still be held by the app's own host for a moment.
    let mut delay = Duration::from_secs(1);
    loop {
        match host_cmds::start_host(&env).await {
            Ok(status) => {
                tracing::info!(id = %status.device_id, "agent sharing");
                break;
            }
            Err(e) => {
                tracing::warn!("starting the host failed: {e}");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
    pipe_server(env).await;
    Ok(())
}

async fn pipe_server(env: AgentEnv) {
    use std::os::windows::io::AsRawHandle;
    use tokio::net::windows::named_pipe::ServerOptions;
    let name = paths::agent_pipe(env.0.session);
    let mut first = true;
    loop {
        let server = SecurityAttrs::from_sddl(AGENT_PIPE_SDDL).and_then(|mut attrs| {
            let mut options = ServerOptions::new();
            options
                .first_pipe_instance(first)
                .reject_remote_clients(true);
            // SAFETY: the attributes outlive the call.
            unsafe { options.create_with_security_attributes_raw(&name, attrs.as_ptr()) }
        });
        let server = match server {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("agent pipe: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        first = false;
        if server.connect().await.is_err() {
            continue;
        }
        // Only programs in this session (the logged-in user's app).
        if win::pipe_client_session(server.as_raw_handle()) != Some(env.0.session) {
            tracing::warn!("agent pipe: client from another session refused");
            continue;
        }
        let env = env.clone();
        tokio::spawn(async move {
            env.0.clients.fetch_add(1, Ordering::Relaxed);
            serve_client(&env, server).await;
            env.0.clients.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

async fn serve_client(env: &AgentEnv, pipe: tokio::net::windows::named_pipe::NamedPipeServer) {
    let (read, write) = tokio::io::split(pipe);
    let write = Arc::new(tokio::sync::Mutex::new(write));
    let mut events = env.0.events.subscribe();
    let event_writer = write.clone();
    let forward = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(line) => {
                    let mut w = event_writer.lock().await;
                    use tokio::io::AsyncWriteExt;
                    if w.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
    });
    let mut reader = LineReader::new(read);
    while let Some(request) = reader.next::<Request<AgentCmd>>().await {
        let result = handle(env, request.cmd).await;
        let reply = Message::Reply {
            id: request.id,
            result,
        };
        if write_line(&mut *write.lock().await, &reply).await.is_err() {
            break;
        }
    }
    forward.abort();
    // Requests waiting for this window can no longer be answered.
    if env.0.clients.load(Ordering::Relaxed) <= 1 {
        host_cmds::reject_pending(&env.0.state);
    }
}

async fn handle(env: &AgentEnv, cmd: AgentCmd) -> Result<Value, String> {
    let state = &env.0.state;
    match cmd {
        AgentCmd::Status => {
            let host = state.host.lock().await;
            let running = host.as_ref().ok_or("Usługa uruchamia udostępnianie…")?;
            let uses_rendezvous = !state.settings().rendezvous_server.is_empty();
            let mut status = host_cmds::status_of(state, running, uses_rendezvous);
            status.service = true;
            serde_json::to_value(status).map_err(|e| e.to_string())
        }
        AgentCmd::NewPassword => {
            let password = state.rotate_password();
            env.emit_json("host://password", json!(password));
            Ok(json!(password))
        }
        AgentCmd::Respond {
            id,
            accept,
            permissions,
            trust,
        } => {
            let mut permissions = permissions;
            if env.download_dir().is_none() {
                permissions.file_transfer = false;
            }
            host_cmds::respond(state, id, accept, permissions, trust).map(|()| Value::Null)
        }
        AgentCmd::Chat { session_id, text } => {
            host_cmds::command(state, session_id, SessionCmd::Chat(text))
                .await
                .map(|()| Value::Null)
        }
        AgentCmd::Disconnect { session_id } => {
            let _ = host_cmds::command(state, session_id, SessionCmd::Disconnect).await;
            Ok(Value::Null)
        }
        AgentCmd::SendFiles { session_id, paths } => {
            let env2 = env.clone();
            let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
            let files = tokio::task::spawn_blocking(move || env2.open_outgoing(paths))
                .await
                .map_err(|e| e.to_string())?;
            if files.is_empty() {
                return Err("Nie można otworzyć wybranych plików.".into());
            }
            host_cmds::command(state, session_id, SessionCmd::SendFiles(files))
                .await
                .map(|()| Value::Null)
        }
        AgentCmd::CancelTransfer { session_id, id } => {
            let _ = host_cmds::command(state, session_id, SessionCmd::CancelTransfer(id)).await;
            Ok(Value::Null)
        }
    }
}

/// One request to the service pipe (agent → service, elevated helper →
/// service).
pub async fn service_request(cmd: ServiceCmd) -> Result<Value, String> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let mut attempts = 0;
    let pipe = loop {
        match ClientOptions::new().open(paths::SERVICE_PIPE) {
            Ok(p) => break p,
            Err(e) if attempts < 50 => {
                attempts += 1;
                let _ = e;
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => return Err(format!("usługa nie odpowiada: {e}")),
        }
    };
    let (read, mut write) = tokio::io::split(pipe);
    write_line(&mut write, &Request { id: 1, cmd })
        .await
        .map_err(|e| e.to_string())?;
    let mut reader = LineReader::new(read);
    match tokio::time::timeout(Duration::from_secs(30), reader.next::<Message>()).await {
        Ok(Some(Message::Reply { result, .. })) => result,
        _ => Err("usługa nie odpowiedziała".into()),
    }
}
