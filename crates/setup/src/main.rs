//! nothing-setup.exe: a single self-contained installer.
//!
//! It carries both release binaries inside itself (`include_bytes!`), so there is nothing
//! to unzip and no installer framework to ship. Flow: confirm -> unpack to a temp folder
//! -> run `nothing.exe --install` elevated (one UAC prompt; that code copies to Program
//! Files, registers the service, the logon entry and the Apps & features entry) -> start
//! the installed UI unelevated.
//!
//!   nothing-setup.exe          interactive
//!   nothing-setup.exe /S       silent (still one UAC prompt unless already elevated)

#![windows_subsystem = "windows"]

use std::path::PathBuf;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Controls::{
    TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOG_BUTTON, TDCBF_CANCEL_BUTTON, TDCBF_CLOSE_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION,
    TDF_USE_COMMAND_LINKS, TDF_VERIFICATION_FLAG_CHECKED,
};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

static UI: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/nothing.exe"));
static SVC: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/nothing-svc.exe"));

const VERSION: &str = env!("CARGO_PKG_VERSION");
const ID_INSTALL: i32 = 100;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn install_dir() -> PathBuf {
    let pf = std::env::var_os("ProgramW6432").or_else(|| std::env::var_os("ProgramFiles")).map(PathBuf::from).unwrap_or_else(|| r"C:\Program Files".into());
    pf.join("Nothing")
}

/// Task dialog with our icon. Returns (button id, verification checkbox state).
fn dialog(instruction: &str, content: &str, buttons: &[(i32, &str)], common: i32, verify: Option<&str>) -> (i32, bool) {
    let title = wide("Nothing Setup");
    let (ins, con) = (wide(instruction), wide(content));
    let texts: Vec<Vec<u16>> = buttons.iter().map(|b| wide(b.1)).collect();
    let btns: Vec<TASKDIALOG_BUTTON> = buttons.iter().zip(&texts).map(|(b, t)| TASKDIALOG_BUTTON { nButtonID: b.0, pszButtonText: t.as_ptr() }).collect();
    let vtext = verify.map(wide);
    unsafe {
        let mut c: TASKDIALOGCONFIG = std::mem::zeroed();
        c.cbSize = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
        c.hInstance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null());
        c.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION | TDF_USE_COMMAND_LINKS | if verify.is_some() { TDF_VERIFICATION_FLAG_CHECKED } else { 0 };
        c.dwCommonButtons = common;
        c.pszWindowTitle = title.as_ptr();
        c.Anonymous1.pszMainIcon = 1 as _; // our app icon resource
        c.pszMainInstruction = ins.as_ptr();
        c.pszContent = con.as_ptr();
        c.cButtons = btns.len() as u32;
        c.pButtons = btns.as_ptr();
        c.nDefaultButton = buttons.first().map(|b| b.0).unwrap_or(0);
        if let Some(v) = &vtext {
            c.pszVerificationText = v.as_ptr();
        }
        let (mut pressed, mut checked) = (0i32, 0);
        TaskDialogIndirect(&c, &mut pressed, null_mut(), &mut checked);
        (pressed, checked != 0)
    }
}

fn error(msg: &str) {
    dialog("Setup failed", msg, &[], TDCBF_CLOSE_BUTTON, None);
}

/// Run `exe args` elevated and wait. None = the user declined the UAC prompt.
fn run_elevated(exe: &std::path::Path, args: &str) -> Option<u32> {
    let (verb, file, params) = (wide("runas"), wide(&exe.display().to_string()), wide(args));
    unsafe {
        let mut sei: SHELLEXECUTEINFOW = std::mem::zeroed();
        sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOCLOSEPROCESS;
        sei.lpVerb = verb.as_ptr();
        sei.lpFile = file.as_ptr();
        sei.lpParameters = params.as_ptr();
        sei.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut sei) == 0 || sei.hProcess.is_null() {
            return None;
        }
        WaitForSingleObject(sei.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(sei.hProcess, &mut code);
        CloseHandle(sei.hProcess);
        Some(code)
    }
}

fn main() {
    let silent = std::env::args().skip(1).any(|a| a.eq_ignore_ascii_case("/S") || a == "--silent");
    let dir = install_dir();
    let upgrading = dir.join("nothing.exe").exists();

    let mut launch = true;
    if !silent {
        let content = format!(
            "Instant file-name search for your NTFS drives.\n\n\
             • Installs to {}\n\
             • Adds the \"Nothing Index\" background service (reads the NTFS file table; needs admin once)\n\
             • Starts with Windows in the tray — press Win+Alt+Space anywhere\n\
             • Uninstall any time from Settings ▸ Apps",
            dir.display()
        );
        let label = if upgrading { format!("Update to {VERSION}\nKeeps your index and settings") } else { format!("Install Nothing {VERSION}") };
        let (pressed, checked) =
            dialog(&format!("Nothing {VERSION}"), &content, &[(ID_INSTALL, &label)], TDCBF_CANCEL_BUTTON, Some("Open Nothing when setup finishes"));
        if pressed != ID_INSTALL {
            return;
        }
        launch = checked;
    }

    // Unpack the payload next to each other (install() expects nothing-svc.exe beside nothing.exe).
    let tmp = std::env::temp_dir().join(format!("nothing-setup-{}", std::process::id()));
    let unpacked = std::fs::create_dir_all(&tmp)
        .and_then(|_| std::fs::write(tmp.join("nothing.exe"), UI))
        .and_then(|_| std::fs::write(tmp.join("nothing-svc.exe"), SVC));
    if let Err(e) = unpacked {
        error(&format!("Could not unpack to {}: {e}", tmp.display()));
        return;
    }

    let keep = format!("--install --keep-pid {}", std::process::id());
    let result = run_elevated(&tmp.join("nothing.exe"), &keep);
    let _ = std::fs::remove_dir_all(&tmp);
    match result {
        None => {
            if !silent {
                error("Administrator permission is needed once, to install the indexing service.");
            }
        }
        Some(0) => {
            if launch {
                let _ = std::process::Command::new(dir.join("nothing.exe")).spawn();
            }
            if !silent && !launch {
                dialog("Nothing is installed", "Press Win+Alt+Space anywhere to search.", &[], TDCBF_CLOSE_BUTTON, None);
            }
        }
        Some(code) => {
            if !silent {
                error(&format!("The installer step returned code {code}."));
            }
            std::process::exit(code as i32);
        }
    }
}
