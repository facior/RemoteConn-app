//! Win32 pieces of the service: sessions, tokens, processes, pipe security.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL, WAIT_OBJECT_0};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DuplicateTokenEx, ImpersonateLoggedOnUser, RevertToSelf, SecurityImpersonation,
    SetTokenInformation, TokenPrimary, TokenSessionId, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    TOKEN_ALL_ACCESS,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Pipes::GetNamedPipeClientSessionId;
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId, GetExitCodeProcess,
    OpenProcessToken, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Downloads, SHGetKnownFolderPath, ShellExecuteExW, KF_FLAG_DEFAULT,
    SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

fn os_err(e: windows::core::Error) -> io::Error {
    io::Error::other(e.message())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain([0]).collect()
}

pub struct OwnedHandle(pub HANDLE);

// Kernel handles may be used from any thread.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle is owned by this value.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// Session attached to the physical console, if any.
pub fn active_console_session() -> Option<u32> {
    // SAFETY: plain query.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (id != u32::MAX).then_some(id)
}

pub fn current_session() -> u32 {
    let mut session = 0u32;
    // SAFETY: valid out pointer.
    let _ = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    session
}

/// A process started by [`spawn_in_session`].
pub struct Child {
    process: OwnedHandle,
    _thread: OwnedHandle,
    pub session: u32,
}

impl Child {
    pub fn has_exited(&self) -> bool {
        // SAFETY: the process handle is valid while `self` lives.
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_OBJECT_0 }
    }

    pub fn kill(&self) {
        // SAFETY: as above.
        let _ = unsafe { TerminateProcess(self.process.0, 1) };
    }
}

/// Starts `exe args` as SYSTEM (this service's account) inside `session`,
/// on the interactive desktop. Only a LocalSystem service may do this.
pub fn spawn_in_session(exe: &Path, args: &str, session: u32) -> io::Result<Child> {
    // SAFETY: handles are wrapped in OwnedHandle right away; buffers outlive
    // the calls that use them.
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own).map_err(os_err)?;
        let own = OwnedHandle(own);
        let mut token = HANDLE::default();
        DuplicateTokenEx(
            own.0,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        )
        .map_err(os_err)?;
        let token = OwnedHandle(token);
        SetTokenInformation(
            token.0,
            TokenSessionId,
            &session as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .map_err(os_err)?;

        let app = wide_path(exe);
        let mut cmdline = wide(&format!("\"{}\" {args}", exe.display()));
        let mut desktop = wide("winsta0\\default");
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(app.as_ptr()),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
        .map_err(os_err)?;
        Ok(Child {
            process: OwnedHandle(pi.hProcess),
            _thread: OwnedHandle(pi.hThread),
            session,
        })
    }
}

/// Token of the user logged into `session` (none at the login screen).
pub fn user_token(session: u32) -> Option<OwnedHandle> {
    let mut token = HANDLE::default();
    // SAFETY: valid out pointer; requires SYSTEM, fails otherwise.
    unsafe { WTSQueryUserToken(session, &mut token) }.ok()?;
    Some(OwnedHandle(token))
}

/// Runs `f` with the rights of the user logged into `session`; `None` when
/// nobody is logged in.
pub fn as_user<T>(session: u32, f: impl FnOnce() -> T) -> Option<T> {
    let token = user_token(session)?;
    // SAFETY: impersonation applies to this thread only and is reverted
    // before returning.
    unsafe { ImpersonateLoggedOnUser(token.0) }.ok()?;
    let result = f();
    // SAFETY: undoes the impersonation above.
    let _ = unsafe { RevertToSelf() };
    Some(result)
}

/// The Downloads folder of the user logged into `session`.
pub fn user_downloads(session: u32) -> Option<PathBuf> {
    let token = user_token(session)?;
    // SAFETY: the returned string is freed with CoTaskMemFree.
    unsafe {
        let path =
            SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, Some(token.0)).ok()?;
        let result = path.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(path.0 as *const c_void));
        result
    }
}

/// Security attributes built from an SDDL string, for named pipes.
pub struct SecurityAttrs {
    attrs: SECURITY_ATTRIBUTES,
    descriptor: PSECURITY_DESCRIPTOR,
}

unsafe impl Send for SecurityAttrs {}

impl SecurityAttrs {
    pub fn from_sddl(sddl: &str) -> io::Result<Self> {
        let text = wide(sddl);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: valid string; the descriptor is freed in Drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .map_err(os_err)?;
        }
        Ok(Self {
            attrs: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            },
            descriptor,
        })
    }

    pub fn as_ptr(&mut self) -> *mut c_void {
        &mut self.attrs as *mut SECURITY_ATTRIBUTES as *mut c_void
    }
}

impl Drop for SecurityAttrs {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptor...
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.descriptor.0)));
        }
    }
}

/// Session of the process at the other end of a pipe (server side).
pub fn pipe_client_session(raw: *mut c_void) -> Option<u32> {
    let mut session = 0u32;
    // SAFETY: `raw` is an open pipe handle owned by the caller.
    unsafe { GetNamedPipeClientSessionId(HANDLE(raw), &mut session) }.ok()?;
    Some(session)
}

/// Ctrl+Alt+Del. Works from a service when the "SoftwareSASGeneration"
/// policy allows it (set by the installer).
pub fn send_sas() -> bool {
    // SAFETY: SendSAS(BOOL AsUser) is looked up by name in sas.dll.
    unsafe {
        let Ok(lib) = LoadLibraryW(w!("sas.dll")) else {
            return false;
        };
        let Some(proc) = GetProcAddress(lib, windows::core::s!("SendSAS")) else {
            return false;
        };
        let send: extern "system" fn(windows::core::BOOL) = std::mem::transmute(proc);
        send(false.into());
        true
    }
}

/// Runs `exe args` elevated (UAC prompt) and waits; returns its exit code.
pub fn run_elevated(exe: &Path, args: &str) -> io::Result<u32> {
    let file = wide_path(exe);
    let params = wide(args);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: strings outlive the call; the process handle is closed below.
    unsafe {
        ShellExecuteExW(&mut info).map_err(|e| {
            if e.code().0 as u32 == 0x800704C7 {
                io::Error::new(io::ErrorKind::PermissionDenied, "anulowano w oknie UAC")
            } else {
                os_err(e)
            }
        })?;
        let process = OwnedHandle(info.hProcess);
        WaitForSingleObject(process.0, INFINITE);
        let mut code = 0u32;
        GetExitCodeProcess(process.0, &mut code).map_err(os_err)?;
        Ok(code)
    }
}
