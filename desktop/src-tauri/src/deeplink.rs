//! `remoteconn://connect?id=123456789` links, e.g. "Połącz" in the server's
//! admin panel.
//!
//! A link only fills in the ID on the connect form: it never connects by
//! itself and never changes settings, so a malicious link can do no more than
//! a typed ID. The password is always entered by the user.
//!
//! The OS starts a new process for each link; if the app is already running,
//! that process hands the ID over on a loopback socket and exits.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

pub const SCHEME: &str = "remoteconn";
pub const EVENT: &str = "app://connect-to";
const FORWARD_PORT: u16 = 21119;

fn forward_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], FORWARD_PORT))
}

/// The 9-digit device ID from a link; `None` for anything else.
pub fn parse(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix(SCHEME)?.strip_prefix(':')?;
    let rest = rest.trim_start_matches('/');
    let (action, params) = rest.split_once(['?', '/']).unwrap_or((rest, ""));
    if !action.eq_ignore_ascii_case("connect") {
        return None;
    }
    // Browsers may turn `connect?id=` into `connect/?id=`.
    let params = params.trim_start_matches(['/', '?']);
    // `connect?id=123456789` or `connect/123456789`.
    let raw = params
        .split('&')
        .find_map(|p| p.strip_prefix("id="))
        .unwrap_or(params);
    let raw = raw.replace("%20", " ").replace('+', " ");
    if !raw
        .chars()
        .all(|c| c.is_ascii_digit() || c == ' ' || c == '-')
    {
        return None;
    }
    let id: String = raw.chars().filter(char::is_ascii_digit).collect();
    (id.len() == 9).then_some(id)
}

/// A link the OS passed on the command line (valid or not), if any.
pub fn link_arg() -> Option<String> {
    std::env::args().skip(1).find(|a| {
        a.get(..SCHEME.len() + 1)
            .is_some_and(|p| p.eq_ignore_ascii_case(&format!("{SCHEME}:")))
    })
}

/// Hands the ID (if the link had a valid one) to a running instance; false if
/// none answered. An invalid link thus never starts a second instance.
pub fn forward_to_running(id: Option<&str>) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&forward_addr(), Duration::from_millis(500))
    else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = stream.write_all(format!("{}\n", id.unwrap_or("")).as_bytes());
    true
}

/// Accepts IDs from later link launches. If the port is taken (another
/// instance runs), links go to that instance instead.
pub fn listen(app: AppHandle) {
    let listener = match TcpListener::bind(forward_addr()) {
        Ok(l) => l,
        Err(e) => {
            tracing::info!("link forwarding unavailable (another instance?): {e}");
            return;
        }
    };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            // Only a bare 9-digit line is accepted: an HTTP request from a web
            // page that probes the port does not match.
            if BufReader::new(stream.take(64)).read_line(&mut line).is_ok() {
                let id = line.trim();
                if id.len() == 9 && id.bytes().all(|b| b.is_ascii_digit()) {
                    deliver(&app, id.to_string());
                }
            }
        }
    });
}

/// Shows the window and fills in the ID (also if the UI is not loaded yet).
pub fn deliver(app: &AppHandle, id: String) {
    let state = app.state::<crate::state::AppState>();
    *state.pending_link.lock().unwrap() = Some(id.clone());
    let _ = app.emit(EVENT, id);
    crate::show_main_window(app);
}

/// Registers the URL scheme for the current user (no admin rights needed),
/// pointing at this executable.
#[cfg(windows)]
pub fn register_scheme() -> Result<(), String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    fn set(path: &str, name: Option<&str>, value: &str) -> Result<(), String> {
        let mut key = HKEY::default();
        // SAFETY: valid strings and out pointer; the key is closed below.
        let created = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(path),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        };
        created.ok().map_err(|e| format!("{path}: {e}"))?;
        let wide: Vec<u16> = value.encode_utf16().chain([0]).collect();
        // SAFETY: the byte view covers exactly the UTF-16 buffer.
        let bytes =
            unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
        let name = name.map(HSTRING::from);
        let name_ptr = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
        // SAFETY: key is open; name and data outlive the call.
        let result = unsafe { RegSetValueExW(key, name_ptr, None, REG_SZ, Some(bytes)) };
        // SAFETY: closing the key opened above.
        let _ = unsafe { RegCloseKey(key) };
        result.ok().map_err(|e| format!("{path}: {e}"))
    }

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.display().to_string();
    let base = format!(r"Software\Classes\{SCHEME}");
    set(&base, None, "URL:RemoteConn")?;
    set(&base, Some("URL Protocol"), "")?;
    set(
        &format!(r"{base}\DefaultIcon"),
        None,
        &format!("\"{exe}\",0"),
    )?;
    set(
        &format!(r"{base}\shell\open\command"),
        None,
        &format!("\"{exe}\" \"%1\""),
    )
}

/// macOS declares the scheme in Info.plist; links arrive as `RunEvent::Opened`.
#[cfg(not(windows))]
pub fn register_scheme() -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_only_connect_links_with_an_id() {
        let id = Some("123456789".to_string());
        assert_eq!(parse("remoteconn://connect?id=123456789"), id);
        assert_eq!(parse("remoteconn://connect/?id=123456789"), id);
        assert_eq!(parse("remoteconn://connect/123%20456%20789"), id);
        assert_eq!(parse("remoteconn:connect?id=123-456-789"), id);
        assert_eq!(parse("remoteconn://connect?x=1&id=123456789"), id);
        assert_eq!(parse("remoteconn://connect?id=12345678"), None);
        // Other parameters are ignored; nothing but the ID is ever taken.
        assert_eq!(parse("remoteconn://connect?id=123456789&password=x"), id);
        assert_eq!(parse("remoteconn://settings?server=evil"), None);
        assert_eq!(parse("remoteconn://connect?id=12345678a"), None);
        assert_eq!(parse("https://connect?id=123456789"), None);
    }
}
