//! Persistent device identity, encrypted at rest (see [`crate::secret`]).

use std::path::Path;

use rc_crypto::DeviceIdentity;

use crate::secret;

const FILE: &str = "identity.key";

pub fn load_or_create(dir: &Path) -> std::io::Result<DeviceIdentity> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE);
    match secret::load(&path) {
        Ok(Some(loaded)) => match DeviceIdentity::from_secret_bytes(&loaded.data) {
            Ok(id) => {
                if loaded.legacy {
                    // Written by an older version in plain form: encrypt it now.
                    secret::save(&path, &*id.to_secret_bytes())?;
                }
                return Ok(id);
            }
            Err(e) => tracing::warn!("corrupt identity file, generating a new one: {e}"),
        },
        Ok(None) => {}
        Err(e) => tracing::warn!("cannot read identity, generating a new one: {e}"),
    }
    let id = DeviceIdentity::generate();
    secret::save(&path, &*id.to_secret_bytes())?;
    Ok(id)
}

/// Saves `id` as the identity in `dir` (the service takes over the app's).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn store(dir: &Path, id: &DeviceIdentity) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    secret::save(&dir.join(FILE), &*id.to_secret_bytes())
}
