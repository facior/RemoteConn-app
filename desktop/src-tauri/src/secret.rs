//! Secrets at rest (device key, permanent password).
//!
//! Windows: DPAPI, bound to the current user account. Other platforms store
//! the bytes as-is until the Keychain backend lands (stage 3).

use std::path::Path;

/// Prefix marking DPAPI-protected files, so legacy plain files are detected.
const MAGIC: &[u8] = b"RCDP1";

pub fn save(path: &Path, secret: &[u8]) -> std::io::Result<()> {
    let mut out = MAGIC.to_vec();
    out.extend(protect(secret)?);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(tmp, path)
}

/// Reads a secret; `Ok(None)` if the file does not exist. Files written
/// before encryption was introduced are returned as-is (callers re-save).
pub fn load(path: &Path) -> std::io::Result<Option<Loaded>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(Some(match bytes.strip_prefix(MAGIC) {
        Some(sealed) => Loaded {
            data: unprotect(sealed)?,
            legacy: false,
        },
        None => Loaded {
            data: bytes,
            legacy: true,
        },
    }))
}

pub fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

pub struct Loaded {
    pub data: Vec<u8>,
    /// Stored unencrypted by an older version.
    pub legacy: bool,
}

#[cfg(windows)]
fn protect(data: &[u8]) -> std::io::Result<Vec<u8>> {
    dpapi::run(data, true)
}

#[cfg(windows)]
fn unprotect(data: &[u8]) -> std::io::Result<Vec<u8>> {
    dpapi::run(data, false)
}

#[cfg(not(windows))]
fn protect(data: &[u8]) -> std::io::Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(not(windows))]
fn unprotect(data: &[u8]) -> std::io::Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(windows)]
mod dpapi {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    pub fn run(data: &[u8], protect: bool) -> std::io::Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input points to `data` for the duration of the call; the
        // output buffer is allocated by DPAPI and freed with LocalFree below.
        unsafe {
            if protect {
                CryptProtectData(
                    &input,
                    PCWSTR::null(),
                    None,
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    None,
                    None,
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
            .map_err(|e| std::io::Error::other(e.message()))?;
            let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            LocalFree(Some(HLOCAL(output.pbData.cast())));
            Ok(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_legacy_detection() {
        let dir = std::env::temp_dir().join(format!("rc-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.bin");
        save(&path, b"top secret").unwrap();
        let raw = std::fs::read(&path).unwrap();
        if cfg!(windows) {
            assert!(
                !raw.windows(10).any(|w| w == b"top secret"),
                "stored encrypted"
            );
        }
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.data, b"top secret");
        assert!(!loaded.legacy);

        std::fs::write(&path, b"plain").unwrap();
        let legacy = load(&path).unwrap().unwrap();
        assert!(legacy.legacy);
        assert_eq!(legacy.data, b"plain");

        remove(&path).unwrap();
        assert!(load(&path).unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
