//! Install / uninstall: copy binaries to Program Files, register the SYSTEM indexing
//! service, start it, and register the UI to start (hidden, in the tray) at logon.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_LOCAL_MACHINE, REG_SZ};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW,
    QueryServiceStatus, StartServiceW, SC_ACTION, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT, SERVICE_ALL_ACCESS,
    SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL,
    SERVICE_FAILURE_ACTIONSW, SERVICE_NO_CHANGE, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::util::wide;

pub const SERVICE_NAME: &str = "NothingIndex";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn install_dir() -> PathBuf {
    let pf = std::env::var_os("ProgramW6432").or_else(|| std::env::var_os("ProgramFiles")).map(PathBuf::from).unwrap_or_else(|| r"C:\Program Files".into());
    pf.join("Nothing")
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return false;
        }
        let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0;
        let ok = GetTokenInformation(tok, TokenElevation, &mut e as *mut _ as *mut _, 4, &mut len);
        CloseHandle(tok);
        ok != 0 && e.TokenIsElevated != 0
    }
}

/// Re-run this exe elevated with `arg` (UAC prompt) and wait. Returns the exit code.
pub fn elevate_and_wait(arg: &str) -> Option<u32> {
    let exe = std::env::current_exe().ok()?;
    let (verb, file, params) = (wide("runas"), wide(&exe.display().to_string()), wide(arg));
    unsafe {
        let mut sei: SHELLEXECUTEINFOW = std::mem::zeroed();
        sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOCLOSEPROCESS;
        sei.lpVerb = verb.as_ptr();
        sei.lpFile = file.as_ptr();
        sei.lpParameters = params.as_ptr();
        sei.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut sei) == 0 || sei.hProcess.is_null() {
            return None; // UAC declined
        }
        WaitForSingleObject(sei.hProcess, INFINITE);
        let mut code = 1u32;
        windows_sys::Win32::System::Threading::GetExitCodeProcess(sei.hProcess, &mut code);
        CloseHandle(sei.hProcess);
        Some(code)
    }
}

struct Sc(SC_HANDLE);
impl Drop for Sc {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn open_service(access: u32) -> Option<Sc> {
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return None;
        }
        let scm = Sc(scm);
        let s = OpenServiceW(scm.0, wide(SERVICE_NAME).as_ptr(), access);
        (!s.is_null()).then_some(Sc(s))
    }
}

/// Current state of the indexing service, or None if it is not installed.
pub fn service_state() -> Option<u32> {
    let s = open_service(SERVICE_QUERY_STATUS)?;
    let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    (unsafe { QueryServiceStatus(s.0, &mut st) } != 0).then_some(st.dwCurrentState)
}

fn stop_and_wait(s: &Sc) {
    unsafe {
        let mut st: SERVICE_STATUS = std::mem::zeroed();
        ControlService(s.0, SERVICE_CONTROL_STOP, &mut st);
        let t0 = Instant::now();
        while QueryServiceStatus(s.0, &mut st) != 0 && st.dwCurrentState != SERVICE_STOPPED && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn copy_into(src_dir: &Path, dst: &Path, name: &str) -> std::io::Result<()> {
    let from = src_dir.join(name);
    let to = dst.join(name);
    if from == to {
        return Ok(());
    }
    // A running nothing.exe can't be overwritten but can be renamed out of the way.
    if to.exists() && std::fs::copy(&from, &to).is_err() {
        let old = dst.join(format!("{name}.old"));
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&to, &old)?;
    }
    std::fs::copy(&from, &to).map(|_| ())
}

/// Elevated part of `--install`. Returns a process exit code.
pub fn install() -> i32 {
    let src = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
    if !src.join("nothing-svc.exe").exists() {
        crate::app::message("Install", "nothing-svc.exe must sit next to nothing.exe.", true);
        return 2;
    }
    let dst = install_dir();
    if let Err(e) = std::fs::create_dir_all(&dst) {
        crate::app::message("Install", &format!("Cannot create {}: {e}", dst.display()), true);
        return 3;
    }
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS);
        if scm.is_null() {
            return 4;
        }
        let scm = Sc(scm);
        let name = wide(SERVICE_NAME);
        let existing = OpenServiceW(scm.0, name.as_ptr(), SERVICE_ALL_ACCESS);
        if !existing.is_null() {
            stop_and_wait(&Sc(existing));
        }
        for f in ["nothing-svc.exe", "nothing.exe"] {
            if let Err(e) = copy_into(&src, &dst, f) {
                crate::app::message("Install", &format!("Copying {f} failed: {e}"), true);
                return 5;
            }
        }
        let bin = wide(&format!("\"{}\"", dst.join("nothing-svc.exe").display()));
        let svc = if !existing.is_null() {
            let s = Sc(OpenServiceW(scm.0, name.as_ptr(), SERVICE_ALL_ACCESS));
            ChangeServiceConfigW(
                s.0,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                bin.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            );
            let _ = SERVICE_NO_CHANGE;
            s
        } else {
            let s = CreateServiceW(
                scm.0,
                name.as_ptr(),
                wide("Nothing Index").as_ptr(),
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                bin.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(), // LocalSystem: needed to read the raw MFT and USN journal
                std::ptr::null(),
            );
            if s.is_null() {
                crate::app::message("Install", &format!("CreateService failed: {}", std::io::Error::last_os_error()), true);
                return 6;
            }
            Sc(s)
        };
        let mut desc = wide("Keeps the Nothing file-name index up to date from the NTFS change journal.");
        let d = SERVICE_DESCRIPTIONW { lpDescription: desc.as_mut_ptr() };
        ChangeServiceConfig2W(svc.0, SERVICE_CONFIG_DESCRIPTION, &d as *const _ as *const _);
        // Crash => restart after 5 s (twice), then give up for a day.
        let mut actions = [
            SC_ACTION { Type: SC_ACTION_RESTART, Delay: 5000 },
            SC_ACTION { Type: SC_ACTION_RESTART, Delay: 5000 },
            SC_ACTION { Type: 0, Delay: 0 },
        ];
        let fa = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: 86400,
            lpRebootMsg: std::ptr::null_mut(),
            lpCommand: std::ptr::null_mut(),
            cActions: 3,
            lpsaActions: actions.as_mut_ptr(),
        };
        ChangeServiceConfig2W(svc.0, SERVICE_CONFIG_FAILURE_ACTIONS, &fa as *const _ as *const _);
        StartServiceW(svc.0, 0, std::ptr::null());

        let run = wide(&format!("\"{}\" --startup", dst.join("nothing.exe").display()));
        RegSetKeyValueW(HKEY_LOCAL_MACHINE, wide(RUN_KEY).as_ptr(), wide("Nothing").as_ptr(), REG_SZ, run.as_ptr() as *const _, (run.len() * 2) as u32);
    }
    0
}

/// Elevated part of `--uninstall`.
pub fn uninstall() -> i32 {
    if let Some(s) = open_service(SERVICE_ALL_ACCESS) {
        stop_and_wait(&s);
        unsafe { DeleteService(s.0) };
    }
    unsafe { RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, wide(RUN_KEY).as_ptr(), wide("Nothing").as_ptr()) };
    if let Some(pd) = std::env::var_os("ProgramData") {
        let _ = std::fs::remove_dir_all(PathBuf::from(pd).join("Nothing"));
    }
    let dir = install_dir();
    for f in ["nothing-svc.exe", "nothing.exe", "nothing.exe.old", "nothing-svc.exe.old"] {
        let p = dir.join(f);
        if p.exists() && std::fs::remove_file(&p).is_err() {
            // Still running (that's us): delete at next boot.
            unsafe { MoveFileExW(wide(&p.display().to_string()).as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
        }
    }
    if std::fs::remove_dir(&dir).is_err() {
        unsafe { MoveFileExW(wide(&dir.display().to_string()).as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
    }
    0
}
