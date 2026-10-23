use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use rc_crypto::DeviceIdentity;
use rc_proto::Permissions;
use rc_session::host::HostHandle;
use rc_session::viewer::ViewerSession;
use tokio::sync::oneshot;

use crate::config::{hex, parse_hex32, pin_key, ConfigStore, Settings};
use crate::secret;

const PERMANENT_PASSWORD_FILE: &str = "permanent.key";
const ACCOUNT_TOKEN_FILE: &str = "account.token";

pub struct RunningHost {
    pub handle: HostHandle,
}

/// An incoming connection waiting for the accept dialog.
pub struct PendingRequest {
    pub reply: oneshot::Sender<Option<Permissions>>,
    pub public_key: [u8; 32],
    pub fingerprint: String,
    pub device_name: String,
}

pub struct AppState {
    pub identity: Arc<DeviceIdentity>,
    data_dir: PathBuf,
    computer_name: String,
    pub config: Mutex<ConfigStore>,
    /// One-time password; survives host restarts (settings changes).
    pub password: Arc<RwLock<String>>,
    pub permanent_password: Arc<RwLock<Option<String>>>,
    pub host: tokio::sync::Mutex<Option<RunningHost>>,
    pub viewer: tokio::sync::RwLock<Option<ViewerSession>>,
    pub pending: Mutex<HashMap<u64, PendingRequest>>,
    next_request: AtomicU64,
    /// Device ID from a `remoteconn://` link the UI has not picked up yet.
    pub pending_link: Mutex<Option<String>>,
    /// Account the host lets in without a password (shared with the host).
    pub account_trust: Arc<RwLock<Option<rc_session::AccountTrust>>>,
}

impl AppState {
    pub fn new(identity: DeviceIdentity, config: ConfigStore, data_dir: PathBuf) -> Self {
        let permanent = match secret::load(&data_dir.join(PERMANENT_PASSWORD_FILE)) {
            Ok(Some(loaded)) => String::from_utf8(loaded.data).ok(),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("cannot read permanent password: {e}");
                None
            }
        };
        let length = config.current.settings.password_length as usize;
        let trust = config.current.account.trust();
        Self {
            identity: Arc::new(identity),
            data_dir,
            computer_name: computer_name(),
            config: Mutex::new(config),
            password: Arc::new(RwLock::new(rc_session::generate_password(length))),
            permanent_password: Arc::new(RwLock::new(permanent)),
            host: Default::default(),
            viewer: Default::default(),
            pending: Default::default(),
            next_request: AtomicU64::new(1),
            pending_link: Mutex::new(None),
            account_trust: Arc::new(RwLock::new(trust)),
        }
    }

    pub fn next_request_id(&self) -> u64 {
        self.next_request.fetch_add(1, Ordering::Relaxed)
    }

    pub fn settings(&self) -> Settings {
        self.config.lock().unwrap().current.settings.clone()
    }

    pub fn computer_name(&self) -> &str {
        &self.computer_name
    }

    /// Name announced to peers: the configured one or the computer name.
    pub fn device_name(&self) -> String {
        let configured = self.settings().device_name;
        if configured.is_empty() {
            self.computer_name.clone()
        } else {
            configured
        }
    }

    pub fn device_id(&self) -> String {
        self.config.lock().unwrap().current.device_id.clone()
    }

    /// Pinned certificate fingerprint for a rendezvous server, if trusted.
    pub fn server_pin(&self, server: &str) -> Option<[u8; 32]> {
        let config = self.config.lock().unwrap();
        config
            .current
            .server_pins
            .get(&pin_key(server))
            .and_then(|h| parse_hex32(h))
    }

    /// Trusts `fingerprint` for `server` if nothing is pinned yet (TOFU).
    pub fn pin_server_if_new(&self, server: &str, fingerprint: &[u8; 32]) {
        let mut config = self.config.lock().unwrap();
        if let std::collections::btree_map::Entry::Vacant(slot) =
            config.current.server_pins.entry(pin_key(server))
        {
            slot.insert(hex(fingerprint));
            config.save();
        }
    }

    /// Trusts `fingerprint` for `server`, replacing any earlier pin (the
    /// fingerprint came from the administrator in a configuration code).
    pub fn pin_server(&self, server: &str, fingerprint: &[u8; 32]) {
        let mut config = self.config.lock().unwrap();
        config
            .current
            .server_pins
            .insert(pin_key(server), hex(fingerprint));
        config.save();
    }

    pub fn forget_server_pin(&self, server: &str) {
        let mut config = self.config.lock().unwrap();
        if config
            .current
            .server_pins
            .remove(&pin_key(server))
            .is_some()
        {
            config.save();
        }
    }

    pub fn has_permanent_password(&self) -> bool {
        self.permanent_password.read().unwrap().is_some()
    }

    pub fn set_permanent_password(&self, password: Option<String>) -> std::io::Result<()> {
        let path = self.data_dir.join(PERMANENT_PASSWORD_FILE);
        match &password {
            Some(p) => secret::save(&path, p.as_bytes())?,
            None => secret::remove(&path)?,
        }
        *self.permanent_password.write().unwrap() = password;
        Ok(())
    }

    pub fn account(&self) -> crate::config::AccountConfig {
        self.config.lock().unwrap().current.account.clone()
    }

    /// Changes the account settings, saves them and updates what the host
    /// enforces.
    pub fn update_account(&self, f: impl FnOnce(&mut crate::config::AccountConfig)) {
        let trust = {
            let mut config = self.config.lock().unwrap();
            f(&mut config.current.account);
            config.save();
            config.current.account.trust()
        };
        *self.account_trust.write().unwrap() = trust;
    }

    pub fn account_token(&self) -> Option<Vec<u8>> {
        match secret::load(&self.data_dir.join(ACCOUNT_TOKEN_FILE)) {
            Ok(Some(loaded)) => Some(loaded.data),
            _ => None,
        }
    }

    pub fn set_account_token(&self, token: Option<&[u8]>) -> std::io::Result<()> {
        let path = self.data_dir.join(ACCOUNT_TOKEN_FILE);
        match token {
            Some(t) => secret::save(&path, t),
            None => secret::remove(&path),
        }
    }

    /// Replaces the one-time password with a fresh one of the configured length.
    pub fn rotate_password(&self) -> String {
        let length = self.settings().password_length as usize;
        let password = rc_session::generate_password(length);
        *self.password.write().unwrap() = password.clone();
        password
    }
}

fn computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "RemoteConn".into())
}
