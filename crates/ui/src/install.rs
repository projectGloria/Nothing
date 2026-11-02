//! Install / uninstall: copy binaries to Program Files, register the SYSTEM indexing
//! service, start it, and register the UI to start (hidden, in the tray) at logon.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW, HKEY_LOCAL_MACHINE, REG_DWORD, REG_MULTI_SZ, REG_SZ, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ,
};
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
/// Makes Nothing show up (and uninstallable) in Settings > Apps > Installed apps.
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Nothing";
/// Explorer right-click verbs: on a folder, on a drive, and on the empty space inside an
/// open folder (`%V` = that folder). Classic shell verbs: on Windows 11 they appear under
/// "Show more options"; the compact menu only takes packaged IExplorerCommand handlers.
const VERBS: [(&str, &str); 3] = [
    (r"Software\Classes\Directory\shell\Nothing", "%1"),
    (r"Software\Classes\Drive\shell\Nothing", "%1"),
    (r"Software\Classes\Directory\Background\shell\Nothing", "%V"),
];
/// Sparse identity package (package/AppxManifest.xml) that registers nothing_shell.dll as
/// the compact-menu handler. Name = the manifest's Identity Name.
const PACKAGE_NAME: &str = "projectGloria.Nothing";
/// Compact-menu payload. Optional: a dev build run with `nothing.exe --install` has none of
/// it and just gets the classic verbs.
const PACKAGE_FILES: [&str; 3] = ["nothing_shell.dll", "nothing.msix", "nothing.png"];

fn reg_sz(key: &str, name: &str, value: &str) {
    let v = wide(value);
    unsafe { RegSetKeyValueW(HKEY_LOCAL_MACHINE, wide(key).as_ptr(), wide(name).as_ptr(), REG_SZ, v.as_ptr() as *const _, (v.len() * 2) as u32) };
}

fn reg_dword(key: &str, name: &str, value: u32) {
    unsafe { RegSetKeyValueW(HKEY_LOCAL_MACHINE, wide(key).as_ptr(), wide(name).as_ptr(), REG_DWORD, &value as *const u32 as *const _, 4) };
}

/// Close every other running nothing.exe: an older UI sitting in the tray holds its exe
/// open and would keep running the old version.
fn stop_other_uis() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let args: Vec<String> = std::env::args().collect();
    let keep = args.iter().position(|a| a == "--keep-pid").and_then(|i| args.get(i + 1)).and_then(|p| p.parse::<u32>().ok());
    let mut cmd = std::process::Command::new("taskkill");
    cmd.args(["/F", "/IM", "nothing.exe", "/FI", &format!("PID ne {}", std::process::id())]);
    if let Some(k) = keep {
        cmd.args(["/FI", &format!("PID ne {k}")]);
    }
    let _ = cmd.creation_flags(CREATE_NO_WINDOW).output();
}

fn register_uninstaller(dst: &Path) {
    let exe = dst.join("nothing.exe");
    let k = UNINSTALL_KEY;
    reg_sz(k, "DisplayName", "Nothing");
    reg_sz(k, "DisplayVersion", env!("CARGO_PKG_VERSION"));
    reg_sz(k, "Publisher", "projectGloria");
    reg_sz(k, "DisplayIcon", &format!("{},0", exe.display()));
    reg_sz(k, "InstallLocation", &dst.display().to_string());
    reg_sz(k, "UninstallString", &format!("\"{}\" --uninstall", exe.display()));
    reg_sz(k, "URLInfoAbout", "https://github.com/projectGloria/Nothing");
    reg_dword(k, "NoModify", 1);
    reg_dword(k, "NoRepair", 1);
    let bytes: u64 = ["nothing.exe", "nothing-svc.exe", "nothing_shell.dll"].iter().filter_map(|f| std::fs::metadata(dst.join(f)).ok()).map(|m| m.len()).sum();
    reg_dword(k, "EstimatedSize", (bytes / 1024) as u32);
}

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
    // --keep-pid: the elevated child must not close us (we wait for it, then hand over).
    let params = format!("{arg} --keep-pid {}", std::process::id());
    let (verb, file, params) = (wide("runas"), wide(&exe.display().to_string()), wide(&params));
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

const SESSION_MANAGER: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager";

/// Undo an earlier uninstall's delete-at-reboot entries for `dir`. Uninstall can't delete
/// the running nothing.exe (or the folder holding it), so it queues them with MoveFileEx;
/// installing again before a reboot would otherwise lose the fresh nothing.exe at the next
/// boot. The queue is a REG_MULTI_SZ of (source, target) pairs, target "" meaning delete;
/// only those delete pairs under `dir` go, everything else is written back untouched.
fn cancel_pending_deletes(dir: &Path) {
    // Newer builds spill into PendingFileRenameOperations2 once the first value is large.
    for value in ["PendingFileRenameOperations", "PendingFileRenameOperations2"] {
        let Some(list) = read_multi_sz(SESSION_MANAGER, value) else { continue };
        let Some(kept) = without_deletes_under(&list, dir) else { continue };
        if kept.is_empty() {
            unsafe { RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, wide(SESSION_MANAGER).as_ptr(), wide(value).as_ptr()) };
        } else {
            let mut data: Vec<u16> = kept.iter().flat_map(|s| s.encode_utf16().chain(std::iter::once(0))).collect();
            data.push(0); // the empty string that ends a MULTI_SZ
            unsafe {
                RegSetKeyValueW(
                    HKEY_LOCAL_MACHINE,
                    wide(SESSION_MANAGER).as_ptr(),
                    wide(value).as_ptr(),
                    REG_MULTI_SZ,
                    data.as_ptr() as *const _,
                    (data.len() * 2) as u32,
                )
            };
        }
    }
}

/// The pending-rename list minus delete pairs whose source is `dir` or inside it, flattened
/// back to strings; None if nothing of ours was queued (so the value is left alone).
fn without_deletes_under(list: &[String], dir: &Path) -> Option<Vec<String>> {
    let d = dir.display().to_string().to_lowercase();
    // Sources look like `\??\C:\...`, on recent builds prefixed `*1` (a flags marker).
    let ours = |src: &str| {
        let p = src.trim_start_matches(|c: char| c == '*' || c.is_ascii_digit());
        let p = p.strip_prefix(r"\??\").unwrap_or(p).to_lowercase();
        // `C:\Program Files\Nothing2` must not match `C:\Program Files\Nothing`.
        p == d || p.starts_with(&format!("{d}\\"))
    };
    // An odd trailing element is the list's own terminator, not a pair.
    let pairs: Vec<&[String]> = list.chunks(2).filter(|p| p.len() == 2 && !p[0].is_empty()).collect();
    let kept: Vec<&[String]> = pairs.iter().copied().filter(|p| !(p[1].is_empty() && ours(&p[0]))).collect();
    (kept.len() != pairs.len()).then(|| kept.concat())
}

/// All strings of a REG_MULTI_SZ, empty ones included (they're meaningful above), so the
/// data is split by its byte length rather than at the first double NUL.
fn read_multi_sz(key: &str, value: &str) -> Option<Vec<String>> {
    let (k, v) = (wide(key), wide(value));
    let mut size = 0u32;
    let get = |buf: *mut u16, size: &mut u32| unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), v.as_ptr(), RRF_RT_REG_MULTI_SZ, std::ptr::null_mut(), buf as *mut _, size)
    };
    if get(std::ptr::null_mut(), &mut size) != 0 {
        return None; // no such value: nothing is queued
    }
    let mut buf = vec![0u16; size as usize / 2 + 1];
    size = (buf.len() * 2) as u32;
    if get(buf.as_mut_ptr(), &mut size) != 0 {
        return None;
    }
    buf.truncate(size as usize / 2);
    // Drop the terminating NUL of the last string; what's left splits into the strings.
    if buf.last() == Some(&0) {
        buf.pop();
    }
    Some(buf.split(|&c| c == 0).map(String::from_utf16_lossy).collect())
}

/// Elevated part of `--install`. Returns a process exit code.
pub fn install() -> i32 {
    let src = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
    if !src.join("nothing-svc.exe").exists() {
        crate::app::message("Install", "nothing-svc.exe must sit next to nothing.exe.", true);
        return 2;
    }
    let dst = install_dir();
    stop_other_uis();
    cancel_pending_deletes(&dst);
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
        // Explorer keeps nothing_shell.dll loaded; copy_into renames it out of the way.
        for f in PACKAGE_FILES.into_iter().filter(|f| src.join(f).exists()) {
            let _ = copy_into(&src, &dst, f);
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
    }
    reg_sz(RUN_KEY, "Nothing", &format!("\"{}\" --startup", dst.join("nothing.exe").display()));
    register_uninstaller(&dst);
    register_verbs(&dst);
    register_package(&dst);
    0
}

fn register_verbs(dst: &Path) {
    let exe = dst.join("nothing.exe");
    for (key, arg) in VERBS {
        reg_sz(key, "", "Nothing");
        reg_sz(key, "Icon", &format!("\"{}\",0", exe.display()));
        reg_sz(&format!(r"{key}\command"), "", &format!("\"{}\" --folder \"{arg}\"", exe.display()));
    }
}

/// Windows build number (22000+ = Windows 11), from the registry: GetVersionEx lies to
/// exes without a matching supportedOS entry, and this one is a plain string anyway.
fn windows_build() -> u32 {
    let mut buf = [0u16; 16];
    let mut len = (buf.len() * 2) as u32;
    let key = wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let ok = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), wide("CurrentBuildNumber").as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut len)
    } == 0;
    if ok { crate::util::from_wide(&buf).parse().unwrap_or(0) } else { 0 }
}

/// Run a PowerShell snippet hidden; true if it exited 0. The Appx cmdlets have no Win32
/// equivalent (PackageManager is WinRT), and this runs once per install.
fn powershell(script: &str) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"])
        .arg(format!("$ErrorActionPreference = 'Stop'; {script}"))
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|s| s.success())
}

/// PowerShell single-quoted string literal.
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Remove every registration of the identity package, for all users.
fn unregister_package() {
    powershell(&format!("Get-AppxPackage -AllUsers -Name {} | Remove-AppxPackage -AllUsers", ps_quote(PACKAGE_NAME)));
}

/// Windows 11 compact menu: register the sparse package with the install folder as its
/// external location. It's unsigned (see crates/setup/build.rs), which Windows only
/// accepts elevated and installs for all users; -AllowUnsigned is harmless on a signed
/// build. On success the classic folder verbs go, or "Show more options" lists Nothing
/// twice; the Drive verb stays because the package schema has no Drive item type.
fn register_package(dst: &Path) {
    let msix = dst.join("nothing.msix");
    if windows_build() < 22000 || !msix.exists() || !dst.join("nothing_shell.dll").exists() {
        return; // Windows 10 has no compact menu: the classic verbs are the menu
    }
    // Re-registering the same version fails (0x80073CF9), so always start clean.
    unregister_package();
    let ok = powershell(&format!(
        "Add-AppxPackage -Path {} -ExternalLocation {} -AllowUnsigned",
        ps_quote(&msix.display().to_string()),
        ps_quote(&dst.display().to_string())
    ));
    if ok {
        for (key, _) in VERBS.iter().filter(|(k, _)| !k.contains(r"\Drive\")) {
            unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, wide(key).as_ptr()) };
        }
    }
}

/// Elevated part of `--uninstall`.
pub fn uninstall() -> i32 {
    if let Some(s) = open_service(SERVICE_ALL_ACCESS) {
        stop_and_wait(&s);
        unsafe { DeleteService(s.0) };
    }
    stop_other_uis();
    unregister_package();
    unsafe {
        RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, wide(RUN_KEY).as_ptr(), wide("Nothing").as_ptr());
        RegDeleteTreeW(HKEY_LOCAL_MACHINE, wide(UNINSTALL_KEY).as_ptr());
        for (key, _) in VERBS {
            RegDeleteTreeW(HKEY_LOCAL_MACHINE, wide(key).as_ptr());
        }
    }
    if let Some(pd) = std::env::var_os("ProgramData") {
        let _ = std::fs::remove_dir_all(PathBuf::from(pd).join("Nothing"));
    }
    let dir = install_dir();
    // Explorer may still have nothing_shell.dll loaded: that one usually goes at reboot.
    let files = ["nothing-svc.exe", "nothing.exe", "nothing_shell.dll", "nothing.msix", "nothing.png"];
    for f in files.iter().flat_map(|f| [f.to_string(), format!("{f}.old")]) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pending_deletes_only_ours_go() {
        let dir = Path::new(r"C:\Program Files\Nothing");
        // As read from this machine after an uninstall, plus a stray terminator element.
        let list = strs(&[r"*1\??\C:\Program Files\Nothing\nothing.exe", "", r"*1\??\C:\Program Files\Nothing", "", ""]);
        assert_eq!(without_deletes_under(&list, dir), Some(vec![]));

        let list = strs(&[
            r"\??\C:\Windows\Temp\other.dll", "",                                   // someone else's delete
            r"\??\c:\program files\nothing\NOTHING.EXE", "",                        // ours, any case
            r"\??\C:\Program Files\Nothing2\x.exe", "",                             // prefix lookalike
            r"\??\C:\Temp\new.dll", r"!\??\C:\Program Files\Nothing\nothing_shell.dll", // rename into our dir
        ]);
        let kept = strs(&[
            r"\??\C:\Windows\Temp\other.dll", "",
            r"\??\C:\Program Files\Nothing2\x.exe", "",
            r"\??\C:\Temp\new.dll", r"!\??\C:\Program Files\Nothing\nothing_shell.dll",
        ]);
        assert_eq!(without_deletes_under(&list, dir), Some(kept));

        // Nothing of ours queued: leave the value alone.
        assert_eq!(without_deletes_under(&strs(&[r"\??\C:\a", ""]), dir), None);
        assert_eq!(without_deletes_under(&[], dir), None);
    }
}
