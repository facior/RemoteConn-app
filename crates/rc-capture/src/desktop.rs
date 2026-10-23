//! Following the input desktop.
//!
//! Windows shows the login screen, the lock screen and UAC prompts on a
//! separate "secure" desktop (Winlogon). A thread can only capture or inject
//! into the desktop it is attached to, so capture and input threads call
//! [`follow_input_desktop`] to move to whichever desktop receives input now.
//! Only a process running as SYSTEM (the service agent) may open the secure
//! desktop; for a normal process the call simply reports no change.

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop,
        SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_CREATEMENU,
        DESKTOP_CREATEWINDOW, DESKTOP_ENUMERATE, DESKTOP_HOOKCONTROL, DESKTOP_JOURNALPLAYBACK,
        DESKTOP_JOURNALRECORD, DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS,
        HDESK, UOI_NAME,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;

    thread_local! {
        /// Desktop this thread switched to (kept open while attached).
        static ATTACHED: RefCell<Option<HDESK>> = const { RefCell::new(None) };
    }

    fn name_of(desk: HDESK) -> Option<String> {
        let mut buf = [0u16; 128];
        let mut needed = 0u32;
        // SAFETY: the buffer is large enough for desktop names (<= 64 chars);
        // `needed` receives the size in bytes.
        unsafe {
            GetUserObjectInformationW(
                HANDLE(desk.0),
                UOI_NAME,
                Some(buf.as_mut_ptr().cast()),
                (buf.len() * 2) as u32,
                Some(&mut needed),
            )
            .ok()?;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    pub fn current_input_desktop() -> Option<String> {
        // SAFETY: the handle is closed right after reading its name.
        unsafe {
            let desk =
                OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS).ok()?;
            let name = name_of(desk);
            let _ = CloseDesktop(desk);
            name
        }
    }

    pub fn follow_input_desktop() -> bool {
        let access = DESKTOP_READOBJECTS.0
            | DESKTOP_CREATEWINDOW.0
            | DESKTOP_CREATEMENU.0
            | DESKTOP_HOOKCONTROL.0
            | DESKTOP_JOURNALRECORD.0
            | DESKTOP_JOURNALPLAYBACK.0
            | DESKTOP_ENUMERATE.0
            | DESKTOP_WRITEOBJECTS.0
            | DESKTOP_SWITCHDESKTOP.0;
        // SAFETY: handles are either attached to this thread (and remembered
        // in ATTACHED until replaced) or closed before returning.
        unsafe {
            let Ok(input) = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(access),
            ) else {
                return false;
            };
            let current = GetThreadDesktop(GetCurrentThreadId()).ok();
            let same = match (current.and_then(name_of), name_of(input)) {
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
                _ => true,
            };
            if same || SetThreadDesktop(input).is_err() {
                let _ = CloseDesktop(input);
                return false;
            }
            ATTACHED.with(|slot| {
                if let Some(old) = slot.borrow_mut().replace(input) {
                    let _ = CloseDesktop(old);
                }
            });
            true
        }
    }
}

/// Attaches the calling thread to the desktop that currently receives input;
/// true if it switched. No-op (false) where the desktop cannot be opened.
pub fn follow_input_desktop() -> bool {
    #[cfg(windows)]
    return imp::follow_input_desktop();
    #[cfg(not(windows))]
    false
}

/// Name of the desktop receiving input ("Default", "Winlogon"), if readable.
pub fn current_input_desktop() -> Option<String> {
    #[cfg(windows)]
    return imp::current_input_desktop();
    #[cfg(not(windows))]
    None
}
