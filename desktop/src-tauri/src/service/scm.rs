//! The service process: keeps the agent running in the console session and
//! answers the service pipe (Ctrl+Alt+Del, new configuration).

use std::ffi::OsString;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use super::pipe::{write_line, LineReader, Message, Request, ServiceCmd, ServiceConfig};
use super::win::{self, Child, SecurityAttrs};
use super::{paths, SERVICE_NAME};

/// SYSTEM and Administrators (an elevated admin, i.e. after a UAC prompt).
const SERVICE_PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)";
const CHECK_INTERVAL: Duration = Duration::from_secs(2);

define_windows_service!(ffi_service_main, service_main);

pub fn run_dispatcher() -> i32 {
    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("--service is started by Windows, not by hand ({e})");
            1
        }
    }
}

enum Event {
    Stop,
    SessionChanged,
    /// Settings changed: restart the agent so it reads them.
    Reload,
}

fn service_main(_args: Vec<OsString>) {
    super::init_file_log("service.log");
    if let Err(e) = run() {
        tracing::error!("service failed: {e}");
    }
}

fn run() -> windows_service::Result<()> {
    let (tx, rx) = mpsc::channel::<Event>();
    let handler_tx = tx.clone();
    let status = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = handler_tx.send(Event::Stop);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(_) => {
            let _ = handler_tx.send(Event::SessionChanged);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let set = |state: ServiceState, accept: ServiceControlAccept| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(5),
            process_id: None,
        })
    };
    set(
        ServiceState::Running,
        ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
    )?;
    tracing::info!("service running");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.spawn(pipe_server(tx.clone()));

    let agent: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let exe = std::env::current_exe().unwrap_or_else(|_| paths::service_exe());
    loop {
        supervise(&agent, &exe);
        match rx.recv_timeout(CHECK_INTERVAL) {
            Ok(Event::Stop) => break,
            Ok(Event::Reload) => {
                if let Some(child) = agent.lock().unwrap().take() {
                    child.kill();
                }
            }
            Ok(Event::SessionChanged) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    tracing::info!("service stopping");
    if let Some(child) = agent.lock().unwrap().take() {
        child.kill();
    }
    runtime.shutdown_timeout(Duration::from_secs(1));
    set(ServiceState::Stopped, ServiceControlAccept::empty())?;
    Ok(())
}

/// One agent in the active console session; restarted when it exits or the
/// console moves to another session (user switching).
fn supervise(agent: &Mutex<Option<Child>>, exe: &std::path::Path) {
    let mut slot = agent.lock().unwrap();
    let session = win::active_console_session();
    let keep = match (&*slot, session) {
        (Some(child), Some(s)) => child.session == s && !child.has_exited(),
        _ => false,
    };
    if keep {
        return;
    }
    if let Some(child) = slot.take() {
        child.kill();
    }
    let Some(session) = session else {
        return;
    };
    match win::spawn_in_session(exe, "--agent", session) {
        Ok(child) => {
            tracing::info!(session, "agent started");
            *slot = Some(child);
        }
        Err(e) => tracing::warn!(session, "starting the agent failed: {e}"),
    }
}

async fn pipe_server(events: mpsc::Sender<Event>) {
    use tokio::net::windows::named_pipe::ServerOptions;
    let mut first = true;
    loop {
        let server = SecurityAttrs::from_sddl(SERVICE_PIPE_SDDL).and_then(|mut attrs| {
            let mut options = ServerOptions::new();
            options
                .first_pipe_instance(first)
                .reject_remote_clients(true);
            // SAFETY: the attributes outlive the call.
            unsafe {
                options.create_with_security_attributes_raw(paths::SERVICE_PIPE, attrs.as_ptr())
            }
        });
        let server = match server {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("service pipe: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        first = false;
        if server.connect().await.is_err() {
            continue;
        }
        let events = events.clone();
        tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut reader = LineReader::new(read);
            while let Some(request) = reader.next::<Request<ServiceCmd>>().await {
                let result = handle(request.cmd, &events).await;
                let reply = Message::Reply {
                    id: request.id,
                    result,
                };
                if write_line(&mut write, &reply).await.is_err() {
                    break;
                }
            }
        });
    }
}

async fn handle(
    cmd: ServiceCmd,
    events: &mpsc::Sender<Event>,
) -> Result<serde_json::Value, String> {
    match cmd {
        ServiceCmd::Sas => {
            if win::send_sas() {
                Ok(serde_json::Value::Null)
            } else {
                Err("SendSAS unavailable".into())
            }
        }
        ServiceCmd::Configure(config) => {
            let applied = tokio::task::spawn_blocking(move || apply(*config))
                .await
                .map_err(|e| e.to_string())?;
            applied.map_err(|e| e.to_string())?;
            let _ = events.send(Event::Reload);
            tracing::info!("configuration updated");
            Ok(serde_json::Value::Null)
        }
    }
}

/// Stores what the app sent, encrypted for SYSTEM where secret. Devices
/// trusted in the agent's accept dialog and its ID are kept.
fn apply(new: ServiceConfig) -> std::io::Result<()> {
    use crate::config::{parse_hex, ConfigStore};
    let dir = paths::data_dir();
    std::fs::create_dir_all(&dir)?;
    let identity = parse_hex(&new.identity)
        .and_then(|b| rc_crypto::DeviceIdentity::from_secret_bytes(&b).ok())
        .ok_or_else(|| std::io::Error::other("invalid identity"))?;
    crate::identity::store(&dir, &identity)?;
    let permanent = dir.join("permanent.key");
    match &new.permanent_password {
        Some(p) => crate::secret::save(&permanent, p.as_bytes())?,
        None => crate::secret::remove(&permanent)?,
    }

    let mut store = ConfigStore::load(&dir);
    let mut config = new.config;
    for t in &store.current.settings.trusted_devices {
        if !config
            .settings
            .trusted_devices
            .iter()
            .any(|x| x.public_key == t.public_key)
        {
            config.settings.trusted_devices.push(t.clone());
        }
    }
    if config.device_id.is_empty() {
        config.device_id = store.current.device_id.clone();
    }
    // The service never presents a certificate; it only hosts.
    config.account.cert.clear();
    // Files go to the logged-in user's Downloads (resolved by the agent).
    config.settings.download_dir.clear();
    store.current = config;
    store.save();
    Ok(())
}
