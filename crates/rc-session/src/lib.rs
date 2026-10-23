//! Session logic shared by the desktop app.
//!
//! Connection sequence (after the Noise handshake in `rc-transport`):
//!
//! ```text
//! viewer                          host
//!   Hello                  ─────▶
//!                          ◀─────  HelloAck
//!   (with a valid account certificate in Hello, HelloAck says so and the
//!    password exchange below is skipped)
//!   Pake                   ─────▶
//!                          ◀─────  Pake            (or AuthResult: locked out)
//!   PakeConfirm            ─────▶
//!                          ◀─────  PakeConfirm     (or AuthResult: wrong password)
//!                                  … local user accepts …
//!                          ◀─────  AuthResult OK, Permissions, MonitorList
//!   input / control        ─────▶  ◀─────  video datagrams, VideoConfig
//! ```

pub mod files;
pub mod host;
mod limiter;
mod rate;
mod reassembly;
pub mod viewer;

use std::net::SocketAddr;
use std::time::Duration;

use rand::Rng;
use rc_clipboard::{ClipContent, ClipboardSync};
use rc_proto::{
    auth_result, clipboard_data, envelope, AuthResult, ChatMessage, ClipboardData, Platform,
};

pub use rc_codec::EncoderSettings;
pub use rc_proto as proto;

const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
/// Chat messages are cut to this many characters.
pub const MAX_CHAT_CHARS: usize = 4000;
/// How long the host user has to answer the accept dialog.
pub const APPROVE_TIMEOUT: Duration = Duration::from_secs(60);

/// The other side of a session, as shown in the UI.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    /// Host side: identifies the session for [`host::HostHandle::command`].
    /// Always 0 on the viewer side.
    pub session_id: u64,
    pub device_name: String,
    pub device_id: String,
    pub platform: Platform,
    pub client_version: String,
    /// Fingerprint of the peer's Noise key (short, for display).
    pub fingerprint: String,
    /// The peer's full Noise key: use this, not the fingerprint, to recognise
    /// a device (e.g. trusted devices).
    pub public_key: [u8; 32],
    pub addr: SocketAddr,
    /// True when traffic goes through the relay instead of peer to peer.
    pub relayed: bool,
    /// Host side: how the viewer got in.
    pub auth: AuthMethod,
    /// Host side, with [`AuthMethod::Account`]: the account name.
    pub account: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    OneTimePassword,
    PermanentPassword,
    /// Certificate of the account this host trusts (no password).
    Account,
}

/// The account a host trusts for passwordless access (see rc-crypto
/// `account`), with what the server reported about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountTrust {
    pub name: String,
    pub public_key: [u8; 32],
    /// Noise keys of devices revoked on the account; only ever grows here.
    pub revoked: Vec<[u8; 32]>,
    /// Disabled by the server administrator.
    pub disabled: bool,
}

impl AccountTrust {
    /// Applies an update from the server. The key changes only along
    /// rotations signed by the key trusted now, so the server cannot swap in
    /// a key of its own; revocations and disabling can only take access away
    /// (or restore what the administrator disabled). Returns true if
    /// anything changed.
    pub fn apply(&mut self, update: &rc_rendezvous::AccountUpdate) -> bool {
        let before = self.clone();
        let mut key = self.public_key;
        for r in &update.rotations {
            let (Ok(old), Ok(new), Ok(sig)) = (
                <[u8; 32]>::try_from(r.old_public_key.as_slice()),
                <[u8; 32]>::try_from(r.new_public_key.as_slice()),
                <[u8; 64]>::try_from(r.signature.as_slice()),
            ) else {
                continue;
            };
            if old == key && rc_crypto::account::verify_rotation(&old, &new, &sig) {
                key = new;
            }
        }
        if update.public_key.as_slice() == key {
            self.public_key = key;
        }
        for revoked in &update.revoked {
            if let Ok(k) = <[u8; 32]>::try_from(revoked.as_slice()) {
                if !self.revoked.contains(&k) {
                    self.revoked.push(k);
                }
            }
        }
        self.disabled = update.disabled;
        *self != before
    }

    /// Checks a viewer's certificate (its Noise key is `device`, proven in
    /// the handshake).
    pub fn accepts(&self, cert: &[u8], device: &[u8; 32], now_unix: i64) -> bool {
        if self.disabled || cert.is_empty() || self.revoked.contains(device) {
            return false;
        }
        rc_crypto::account::DeviceCert::verify(cert, &self.public_key, device, now_unix)
            .is_ok_and(|c| c.account_name == self.name)
    }
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Transport(#[from] rc_transport::TransportError),
    #[error(transparent)]
    Capture(#[from] rc_capture::CaptureError),
    #[error(transparent)]
    Codec(#[from] rc_codec::CodecError),
    #[error("timed out")]
    Timeout,
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
}

impl From<tokio::time::error::Elapsed> for SessionError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        SessionError::Timeout
    }
}

pub fn current_platform() -> Platform {
    if cfg!(windows) {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::Macos
    } else {
        Platform::Linux
    }
}

pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default length of [`generate_password`].
pub const DEFAULT_PASSWORD_LEN: usize = 8;

/// One-time session password without look-alikes (0/o, 1/l/i); length is
/// clamped to 6..=16.
pub fn generate_password(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::rngs::OsRng;
    (0..len.clamp(6, 16))
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

fn chat_msg(text: String) -> envelope::Msg {
    let sent_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    envelope::Msg::Chat(ChatMessage { text, sent_unix_ms })
}

/// Watches the local clipboard and forwards changes to the peer; must be
/// called inside a Tokio runtime.
fn clipboard_sync(sender: rc_transport::Sender) -> ClipboardSync {
    let rt = tokio::runtime::Handle::current();
    ClipboardSync::start(move |content| {
        let sender = sender.clone();
        rt.spawn(async move {
            let _ = sender
                .send(envelope::Msg::Clipboard(clip_to_proto(content)))
                .await;
        });
    })
}

fn clip_to_proto(content: ClipContent) -> ClipboardData {
    ClipboardData {
        content: Some(match content {
            ClipContent::Text(t) => clipboard_data::Content::Text(t),
            ClipContent::Png(p) => clipboard_data::Content::Png(p),
        }),
    }
}

fn clip_from_proto(data: ClipboardData) -> Option<ClipContent> {
    match data.content? {
        clipboard_data::Content::Text(t) if t.len() <= rc_clipboard::MAX_TEXT => {
            Some(ClipContent::Text(t))
        }
        clipboard_data::Content::Png(p) if p.len() <= rc_clipboard::MAX_PNG => {
            Some(ClipContent::Png(p))
        }
        _ => None,
    }
}

fn auth_msg(status: auth_result::Status, retry_after_secs: u32) -> envelope::Msg {
    envelope::Msg::AuthResult(AuthResult {
        status: status as i32,
        retry_after_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::AccountTrust;
    use rc_crypto::account::AccountKey;
    use rc_proto::rendezvous::KeyRotation;
    use rc_rendezvous::AccountUpdate;

    #[test]
    fn account_trust_follows_only_signed_rotations() {
        let old = AccountKey::derive("ania", "stare", &[1; 16]).unwrap();
        let new = AccountKey::derive("ania", "nowe", &[2; 16]).unwrap();
        let evil = AccountKey::derive("ania", "serwer", &[3; 16]).unwrap();
        let mut trust = AccountTrust {
            name: "ania".into(),
            public_key: old.public(),
            revoked: Vec::new(),
            disabled: false,
        };
        // A key the server made up, without a rotation signed by the old key.
        let swap = AccountUpdate {
            public_key: evil.public().to_vec(),
            ..Default::default()
        };
        assert!(!trust.apply(&swap));
        assert_eq!(trust.public_key, old.public());

        let device = [9u8; 32];
        let old_cert = old.issue_cert("ania", &device, super::unix_now());
        assert!(trust.accepts(&old_cert, &device, super::unix_now()));

        let rotate = AccountUpdate {
            public_key: new.public().to_vec(),
            rotations: vec![KeyRotation {
                old_public_key: old.public().to_vec(),
                new_public_key: new.public().to_vec(),
                signature: old.sign_rotation(&new.public()).to_vec(),
            }],
            revoked: vec![[4u8; 32].to_vec()],
            disabled: false,
        };
        assert!(trust.apply(&rotate));
        assert_eq!(trust.public_key, new.public());
        assert!(trust.revoked.contains(&[4u8; 32]));
        // Certificates of the old key stop working after a password change.
        assert!(!trust.accepts(&old_cert, &device, super::unix_now()));
        // A later update without the revocation does not lift it.
        let forget = AccountUpdate {
            public_key: new.public().to_vec(),
            ..Default::default()
        };
        trust.apply(&forget);
        assert!(trust.revoked.contains(&[4u8; 32]));
    }

    #[test]
    fn password_shape() {
        let pw = super::generate_password(super::DEFAULT_PASSWORD_LEN);
        assert_eq!(pw.len(), 8);
        assert_eq!(super::generate_password(3).len(), 6);
        assert_eq!(super::generate_password(40).len(), 16);
        assert!(pw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() && !"01ilo".contains(c)));
        assert_ne!(pw, super::generate_password(8));
    }
}
