//! Persistent app settings (`config.json` in the app data directory).
//! Secrets (permanent password) live in separate encrypted files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Theme {
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultPermissions {
    pub view_screen: bool,
    pub control_input: bool,
    pub clipboard: bool,
    pub file_transfer: bool,
}

impl Default for DefaultPermissions {
    fn default() -> Self {
        Self {
            view_screen: true,
            control_input: true,
            clipboard: true,
            file_transfer: true,
        }
    }
}

/// A device allowed in without the accept dialog (it still needs a password).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedDevice {
    /// Full Noise public key (hex): what is compared. Entries from older
    /// versions lack it and never match.
    #[serde(default)]
    pub public_key: String,
    /// Short form, for display only.
    pub fingerprint: String,
    pub name: String,
    pub added_unix: i64,
}

/// Everything the settings screen edits. Missing fields take defaults, so
/// older config files keep working.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    // General
    /// Shown to the other side; empty = computer name.
    pub device_name: String,
    pub theme: Theme,
    pub autostart: bool,
    /// Closing the window hides it to the tray; sharing keeps running.
    pub close_to_tray: bool,
    /// Start sharing as soon as the app opens (useful with autostart).
    pub share_on_start: bool,

    // Security
    pub password_length: u8,
    pub rotate_password_after_session: bool,
    /// Ask in the accept dialog even when the permanent password was used.
    pub confirm_permanent_password: bool,
    pub default_permissions: DefaultPermissions,
    pub trusted_devices: Vec<TrustedDevice>,

    // Session
    /// "balanced" | "quality" | "speed"
    pub default_quality: String,
    pub default_fps: u32,
    /// Empty = `<Downloads>/RemoteConn`.
    pub download_dir: String,
    pub show_remote_cursor: bool,
    pub sound_notifications: bool,

    // Network
    /// Rendezvous server as `host[:port]`; empty = LAN / IP only.
    pub rendezvous_server: String,
    /// The server's access key (`RC_ACCESS_KEY`); empty if it has none.
    pub server_access_key: String,
    pub listen_port: u16,
    pub force_relay: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_name: String::new(),
            theme: Theme::System,
            autostart: false,
            close_to_tray: true,
            share_on_start: false,
            password_length: rc_session::DEFAULT_PASSWORD_LEN as u8,
            rotate_password_after_session: false,
            confirm_permanent_password: false,
            default_permissions: DefaultPermissions::default(),
            trusted_devices: Vec::new(),
            default_quality: "balanced".into(),
            default_fps: 30,
            download_dir: String::new(),
            show_remote_cursor: true,
            sound_notifications: true,
            rendezvous_server: String::new(),
            server_access_key: String::new(),
            listen_port: rc_transport::DEFAULT_PORT,
            force_relay: false,
        }
    }
}

impl Settings {
    /// Clamps values a hand-edited file or the UI could get wrong.
    pub fn normalized(mut self) -> Self {
        self.device_name = self.device_name.trim().chars().take(64).collect();
        self.password_length = self.password_length.clamp(6, 16);
        if !matches!(
            self.default_quality.as_str(),
            "balanced" | "quality" | "speed"
        ) {
            self.default_quality = "balanced".into();
        }
        if ![15, 30, 60].contains(&self.default_fps) {
            self.default_fps = 30;
        }
        if self.listen_port < 1024 {
            self.listen_port = rc_transport::DEFAULT_PORT;
        }
        self.rendezvous_server = self.rendezvous_server.trim().to_string();
        self.server_access_key = self.server_access_key.trim().to_string();
        self.download_dir = self.download_dir.trim().to_string();
        self
    }

    pub fn download_dir(&self) -> PathBuf {
        if self.download_dir.is_empty() {
            rc_session::files::default_download_dir()
        } else {
            PathBuf::from(&self.download_dir)
        }
    }
}

/// The account this device is logged into (empty `name`: none). The session
/// token is a separate encrypted file; nothing here is secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AccountConfig {
    pub name: String,
    pub display_name: String,
    /// Server the account lives on (as entered in the settings).
    pub server: String,
    /// Account public key (hex) this device trusts as a host.
    pub public_key: String,
    /// Revoked device keys (hex) reported by the server.
    pub revoked: Vec<String>,
    pub disabled: bool,
    /// This device's certificate (hex), presented when connecting.
    pub cert: String,
    /// Let devices of the account in without a password.
    pub host_trust: bool,
    /// Device ID last put on the account (rebound when it changes).
    pub bound_device_id: String,
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            display_name: String::new(),
            server: String::new(),
            public_key: String::new(),
            revoked: Vec::new(),
            disabled: false,
            cert: String::new(),
            host_trust: true,
            bound_device_id: String::new(),
        }
    }
}

impl AccountConfig {
    pub fn logged_in(&self) -> bool {
        !self.name.is_empty()
    }

    /// What the host enforces, if passwordless access is on.
    pub fn trust(&self) -> Option<rc_session::AccountTrust> {
        if !self.logged_in() || !self.host_trust {
            return None;
        }
        Some(rc_session::AccountTrust {
            name: self.name.clone(),
            public_key: parse_hex32(&self.public_key)?,
            revoked: self.revoked.iter().filter_map(|r| parse_hex32(r)).collect(),
            disabled: self.disabled,
        })
    }

    pub fn cert_bytes(&self) -> Vec<u8> {
        parse_hex(&self.cert).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    #[serde(flatten)]
    pub settings: Settings,
    /// ID assigned by the rendezvous server, reused on the next registration.
    pub device_id: String,
    /// Certificate fingerprints of rendezvous servers, trusted on first use
    /// (key: normalized server address as entered).
    pub server_pins: BTreeMap<String, String>,
    pub account: AccountConfig,
}

/// Key under which a server's pin is stored.
pub fn pin_key(server: &str) -> String {
    server.trim().to_ascii_lowercase()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    parse_hex(s)?.try_into().ok()
}

pub fn parse_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

pub struct ConfigStore {
    path: PathBuf,
    pub current: Config,
}

impl ConfigStore {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("config.json");
        let mut current: Config = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        current.settings = current.settings.normalized();
        Self { path, current }
    }

    pub fn save(&self) {
        let result = serde_json::to_vec_pretty(&self.current)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&self.path, bytes));
        if let Err(e) = result {
            tracing::warn!("saving settings failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_config_files_still_load() {
        let old = r#"{ "rendezvousServer": "rc.example.com", "deviceId": "123456789" }"#;
        let cfg: Config = serde_json::from_str(old).unwrap();
        assert_eq!(cfg.settings.rendezvous_server, "rc.example.com");
        assert_eq!(cfg.device_id, "123456789");
        assert_eq!(cfg.settings.listen_port, rc_transport::DEFAULT_PORT);
        assert!(cfg.settings.default_permissions.view_screen);
    }

    #[test]
    fn hex_roundtrip() {
        let key = [0xabu8; 32];
        assert_eq!(parse_hex32(&hex(&key)), Some(key));
        assert_eq!(parse_hex32("zz"), None);
    }

    #[test]
    fn normalization_clamps_values() {
        let s = Settings {
            password_length: 99,
            default_fps: 7,
            default_quality: "ultra".into(),
            listen_port: 80,
            device_name: "  biuro  ".into(),
            ..Default::default()
        }
        .normalized();
        assert_eq!(s.password_length, 16);
        assert_eq!(s.default_fps, 30);
        assert_eq!(s.default_quality, "balanced");
        assert_eq!(s.listen_port, rc_transport::DEFAULT_PORT);
        assert_eq!(s.device_name, "biuro");
    }
}
