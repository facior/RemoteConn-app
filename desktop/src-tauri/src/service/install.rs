//! Elevated helper modes (`--install-service`, `--uninstall-service`,
//! `--service-sync`), started by the app through a UAC prompt.
//!
//! Errors are written to `%TEMP%\remoteconn-service-result.txt` (same user,
//! so the app can show them); the exit code says whether it worked.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use super::pipe::{ServiceCmd, ServiceConfig};
use super::{paths, SERVICE_DISPLAY_NAME, SERVICE_NAME};
use crate::config::{hex, ConfigStore};

const FIREWALL_RULE: &str = "RemoteConn (usługa)";
const SAS_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
const OUR_KEY: &str = r"HKLM\SOFTWARE\RemoteConn";

pub fn result_file() -> PathBuf {
    std::env::temp_dir().join("remoteconn-service-result.txt")
}

fn finish(result: Result<(), String>) -> i32 {
    let _ = std::fs::remove_file(result_file());
    match result {
        Ok(()) => 0,
        Err(e) => {
            let _ = std::fs::write(result_file(), &e);
            1
        }
    }
}

pub fn install() -> i32 {
    super::init_file_log("install.log");
    finish(do_install())
}

pub fn uninstall() -> i32 {
    finish(do_uninstall())
}

pub fn sync() -> i32 {
    finish(do_sync())
}

fn manager() -> Result<ServiceManager, String> {
    ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| {
        format!("Brak dostępu do menedżera usług ({e}). Potrzebne są uprawnienia administratora.")
    })
}

fn stop_and_wait(manager: &ServiceManager) {
    let Ok(service) = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP,
    ) else {
        return;
    };
    let _ = service.stop();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match service.query_status() {
            Ok(s) if s.current_state == ServiceState::Stopped => return,
            Err(_) => return,
            _ => std::thread::sleep(Duration::from_millis(300)),
        }
    }
}

fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Copies with retries: the old service process may need a moment to exit.
fn copy_exe(target: &Path) -> Result<(), String> {
    let source = std::env::current_exe().map_err(|e| e.to_string())?;
    if source == target {
        return Ok(());
    }
    std::fs::create_dir_all(target.parent().expect("install dir")).map_err(|e| e.to_string())?;
    let mut last = String::new();
    for _ in 0..20 {
        match std::fs::copy(&source, target) {
            Ok(_) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "Nie można skopiować programu do {}: {last}",
        target.display()
    ))
}

fn do_install() -> Result<(), String> {
    let manager = manager()?;
    stop_and_wait(&manager);

    let exe = paths::service_exe();
    copy_exe(&exe)?;

    // Data directory: SYSTEM and Administrators only (keys, settings).
    let data = paths::data_dir();
    std::fs::create_dir_all(&data).map_err(|e| e.to_string())?;
    let data_str = data.display().to_string();
    run(
        "icacls",
        &[
            &data_str,
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
        ],
    )?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::START
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::STOP;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(existing) => {
            existing
                .change_config(&info)
                .map_err(|e| format!("Aktualizacja usługi: {e}"))?;
            existing
        }
        Err(_) => manager
            .create_service(&info, access)
            .map_err(|e| format!("Tworzenie usługi: {e}"))?,
    };
    let _ = service.set_description(
        "Udostępnia ekran tego komputera w RemoteConn także przed zalogowaniem i przy oknach UAC.",
    );
    let _ = service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            };
            3
        ]),
    });

    enable_software_sas();
    let exe_str = exe.display().to_string();
    let _ = run(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={FIREWALL_RULE}"),
        ],
    );
    if let Err(e) = run(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &format!("name={FIREWALL_RULE}"),
            "dir=in",
            "action=allow",
            &format!("program={exe_str}"),
            "protocol=UDP",
            "enable=yes",
        ],
    ) {
        tracing::warn!("firewall rule: {e}");
    }

    service
        .start::<&str>(&[])
        .or_else(|e| {
            // Already running is fine.
            match service.query_status() {
                Ok(s) if s.current_state == ServiceState::Running => Ok(()),
                _ => Err(e),
            }
        })
        .map_err(|e| format!("Uruchamianie usługi: {e}"))?;
    do_sync()
}

/// Ctrl+Alt+Del from a service needs the "SoftwareSASGeneration" policy.
/// Set it only if unset, and remember that we did (to undo on uninstall).
fn enable_software_sas() {
    let current = run("reg", &["query", SAS_KEY, "/v", "SoftwareSASGeneration"]).ok();
    let set = current
        .as_deref()
        .map(|out| out.contains("0x0\r") || out.contains("0x0\n"))
        .unwrap_or(true);
    if set
        && run(
            "reg",
            &[
                "add",
                SAS_KEY,
                "/v",
                "SoftwareSASGeneration",
                "/t",
                "REG_DWORD",
                "/d",
                "1",
                "/f",
            ],
        )
        .is_ok()
    {
        let _ = run(
            "reg",
            &[
                "add",
                OUR_KEY,
                "/v",
                "SasPolicySet",
                "/t",
                "REG_DWORD",
                "/d",
                "1",
                "/f",
            ],
        );
    }
}

fn do_uninstall() -> Result<(), String> {
    let manager = manager()?;
    stop_and_wait(&manager);
    if let Ok(service) = manager.open_service(SERVICE_NAME, ServiceAccess::DELETE) {
        service
            .delete()
            .map_err(|e| format!("Usuwanie usługi: {e}"))?;
    }
    let _ = run(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={FIREWALL_RULE}"),
        ],
    );
    if run("reg", &["query", OUR_KEY, "/v", "SasPolicySet"]).is_ok() {
        let _ = run(
            "reg",
            &["delete", SAS_KEY, "/v", "SoftwareSASGeneration", "/f"],
        );
        let _ = run("reg", &["delete", OUR_KEY, "/f"]);
    }
    // Copies of keys and settings go away with the service.
    let _ = std::fs::remove_dir_all(paths::data_dir());
    for _ in 0..10 {
        if std::fs::remove_dir_all(paths::install_dir()).is_ok() || !paths::install_dir().exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// The app's data directory of the user running this (elevated) helper.
fn user_data_dir() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA")
        .map(|d| PathBuf::from(d).join("com.remoteconn.desktop"))
        .ok_or_else(|| "Nie znaleziono folderu danych aplikacji.".into())
}

/// Sends this user's settings, identity and permanent password to the
/// service.
fn do_sync() -> Result<(), String> {
    let dir = user_data_dir()?;
    let identity = crate::identity::load_or_create(&dir).map_err(|e| e.to_string())?;
    let config = ConfigStore::load(&dir).current;
    let permanent = crate::secret::load(&dir.join("permanent.key"))
        .ok()
        .flatten()
        .and_then(|l| String::from_utf8(l.data).ok());
    let payload = ServiceConfig {
        config,
        identity: hex(&*identity.to_secret_bytes()),
        permanent_password: permanent,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime
        .block_on(super::agent::service_request(ServiceCmd::Configure(
            Box::new(payload),
        )))
        .map(|_| ())
        .map_err(|e| format!("Przekazanie ustawień do usługi: {e}"))
}
