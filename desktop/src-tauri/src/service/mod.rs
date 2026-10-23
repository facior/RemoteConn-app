//! Unattended access: the RemoteConn Windows service.
//!
//! ```text
//!  service (SYSTEM, session 0)  ──starts──▶  agent (SYSTEM, user's session)
//!     ▲  pipe RemoteConnService               runs the host: capture, input,
//!     │  (SYSTEM + Administrators)            login screen and UAC prompts
//!     │                                        ▲  pipe RemoteConnAgent-<session>
//!  elevated helper (install / sync)            │  (interactive users, same session)
//!                                           app window (the user)
//! ```
//!
//! - The service keeps one agent running in the active console session and
//!   restarts it when the session changes or it exits. It also performs
//!   Ctrl+Alt+Del (`SendSAS`) for the agent.
//! - The agent shares the screen even before anyone logs in. While the app
//!   window runs, it shows ID/password and the accept dialog through the
//!   agent pipe (same events as without the service).
//! - Settings and secrets reach the service only through an elevated helper
//!   (UAC prompt), so a standard user cannot reconfigure unattended access.
//! - The service binary is a copy in Program Files (admin-only), never the
//!   user-writable file the app was started from.

#[cfg(windows)]
pub mod agent;
#[cfg(windows)]
pub mod client;
#[cfg(windows)]
pub mod install;
#[cfg(windows)]
mod pipe;
#[cfg(windows)]
pub mod scm;
#[cfg(windows)]
mod win;

#[cfg(not(windows))]
pub mod client {
    //! No service outside Windows yet.
    use tauri::AppHandle;

    pub struct ServiceClient;

    type Result<T> = std::result::Result<T, String>;

    /// Never constructed ([`active`] is always `None`); mirrors the Windows
    /// client so the host commands compile everywhere.
    #[allow(dead_code)]
    impl ServiceClient {
        pub async fn status(&self) -> Result<crate::host_cmds::HostStatus> {
            Err(UNSUPPORTED.into())
        }
        pub async fn new_password(&self) -> Result<String> {
            Err(UNSUPPORTED.into())
        }
        pub async fn respond(
            &self,
            _id: u64,
            _accept: bool,
            _permissions: crate::host_cmds::PermissionsDto,
            _trust: bool,
        ) -> Result<()> {
            Err(UNSUPPORTED.into())
        }
        pub async fn chat(&self, _session_id: u64, _text: String) -> Result<()> {
            Err(UNSUPPORTED.into())
        }
        pub async fn disconnect(&self, _session_id: u64) -> Result<()> {
            Err(UNSUPPORTED.into())
        }
        pub async fn send_files(&self, _session_id: u64, _paths: Vec<String>) -> Result<()> {
            Err(UNSUPPORTED.into())
        }
        pub async fn cancel_transfer(&self, _session_id: u64, _id: u64) -> Result<()> {
            Err(UNSUPPORTED.into())
        }
    }

    pub fn active(_app: &AppHandle) -> Option<std::sync::Arc<ServiceClient>> {
        None
    }

    pub fn init(_app: &AppHandle) {}

    pub fn installed() -> bool {
        false
    }

    pub async fn sync_if_installed(_app: &AppHandle) {}

    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ServiceStatusDto {
        supported: bool,
        installed: bool,
        running: bool,
        connected: bool,
    }

    #[tauri::command]
    pub fn service_status() -> ServiceStatusDto {
        ServiceStatusDto {
            supported: false,
            installed: false,
            running: false,
            connected: false,
        }
    }

    const UNSUPPORTED: &str = "Usługa jest dostępna tylko w systemie Windows.";

    #[tauri::command]
    pub async fn service_install() -> Result<ServiceStatusDto, String> {
        Err(UNSUPPORTED.into())
    }

    #[tauri::command]
    pub async fn service_uninstall() -> Result<ServiceStatusDto, String> {
        Err(UNSUPPORTED.into())
    }

    #[tauri::command]
    pub async fn service_sync() -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
}

pub const SERVICE_NAME: &str = "RemoteConn";
pub const SERVICE_DISPLAY_NAME: &str = "RemoteConn — dostęp bez nadzoru";

/// Command-line modes handled before the window starts. Returns the exit
/// code when the process should not open the app.
pub fn run_cli() -> Option<i32> {
    let arg = std::env::args().nth(1)?;
    #[cfg(windows)]
    {
        match arg.as_str() {
            "--service" => return Some(scm::run_dispatcher()),
            "--agent" => return Some(agent::run()),
            "--install-service" => return Some(install::install()),
            "--uninstall-service" => return Some(install::uninstall()),
            "--service-sync" => return Some(install::sync()),
            _ => {}
        }
    }
    let _ = arg;
    None
}

#[cfg(windows)]
pub mod paths {
    use std::path::PathBuf;

    fn env_dir(name: &str, fallback: &str) -> PathBuf {
        std::env::var_os(name)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(fallback))
    }

    /// `C:\Program Files\RemoteConn`
    pub fn install_dir() -> PathBuf {
        env_dir("ProgramW6432", r"C:\Program Files").join("RemoteConn")
    }

    pub fn service_exe() -> PathBuf {
        install_dir().join("remoteconn-service.exe")
    }

    /// `C:\ProgramData\RemoteConn\service`: SYSTEM and Administrators only.
    /// `RC_SERVICE_DATA_DIR` overrides it, to run an agent by hand in tests
    /// (the service itself never has it set).
    pub fn data_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("RC_SERVICE_DATA_DIR") {
            return PathBuf::from(dir);
        }
        env_dir("ProgramData", r"C:\ProgramData")
            .join("RemoteConn")
            .join("service")
    }

    pub const SERVICE_PIPE: &str = r"\\.\pipe\RemoteConnService";

    pub fn agent_pipe(session_id: u32) -> String {
        format!(r"\\.\pipe\RemoteConnAgent-{session_id}")
    }
}

/// Writes logs of the service and agent (no console there) to a file in the
/// data directory, truncated at each start.
#[cfg(windows)]
fn init_file_log(name: &str) {
    use tracing_subscriber::EnvFilter;
    let dir = paths::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::File::create(dir.join(name));
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match file {
        Ok(file) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .try_init();
        }
        Err(_) => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
    }
}
