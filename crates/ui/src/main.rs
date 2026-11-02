//! nothing.exe — the search window.
//!
//!   nothing.exe              open (or bring the running instance to front)
//!   nothing.exe --startup    start hidden in the tray (used by the logon Run entry)
//!   nothing.exe --folder D   search inside folder D (Explorer's "Nothing" verb)
//!   nothing.exe --install    install/repair the indexing service (asks for elevation)
//!   nothing.exe --uninstall  remove it

#![windows_subsystem = "windows"]

mod app;
mod com;
mod dark;
mod fit;
mod icons;
mod install;
mod ipc;
mod settings;
mod shell;
mod thumbs;
mod util;

use std::sync::atomic::{AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::Ole::OleInitialize;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, PostMessageW, SendMessageW, WM_COPYDATA};

use util::wide;

static INSTANCE_MUTEX: AtomicIsize = AtomicIsize::new(0);

pub fn release_single_instance() {
    let h = INSTANCE_MUTEX.swap(0, Ordering::SeqCst);
    if h != 0 {
        unsafe { CloseHandle(h as _) };
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    let folder = args.iter().position(|a| a == "--folder").and_then(|i| args.get(i + 1)).cloned();

    if has("--install") || has("--uninstall") {
        let uninstall = has("--uninstall");
        let code = if install::is_elevated() {
            if uninstall { install::uninstall() } else { install::install() }
        } else {
            // Re-launch elevated, then hand over to the installed (unelevated) UI.
            match install::elevate_and_wait(if uninstall { "--uninstall" } else { "--install" }) {
                Some(0) if !uninstall => {
                    let _ = std::process::Command::new(install::install_dir().join("nothing.exe")).spawn();
                    0
                }
                Some(0) => {
                    app::message("Nothing", "Nothing has been removed. Files still in use are deleted at the next restart.", false);
                    0
                }
                Some(c) => c as i32,
                None => 1,
            }
        };
        std::process::exit(code);
    }

    // Single instance: a second launch just brings the first window forward.
    unsafe {
        let m = CreateMutexW(std::ptr::null(), 1, wide("Local\\NothingSearchUI").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let w = FindWindowW(wide(app::CLASS).as_ptr(), std::ptr::null());
            if !w.is_null() && !has("--startup") {
                // We were launched by the user (Explorer, a shortcut), so we may take the
                // foreground; lend that right to the running instance or its window only
                // flashes in the taskbar.
                let mut pid = 0;
                GetWindowThreadProcessId(w, &mut pid);
                AllowSetForegroundWindow(pid);
                match &folder {
                    Some(f) => {
                        let units: Vec<u16> = f.encode_utf16().collect();
                        let cds = COPYDATASTRUCT { dwData: app::COPYDATA_FOLDER, cbData: (units.len() * 2) as u32, lpData: units.as_ptr() as *mut _ };
                        // Sent, not posted: the buffer must outlive the receiver reading it.
                        SendMessageW(w, WM_COPYDATA, 0, &cds as *const _ as isize);
                    }
                    None => {
                        PostMessageW(w, app::WM_APP_SHOW, 0, 0);
                    }
                }
            }
            return;
        }
        INSTANCE_MUTEX.store(m as isize, Ordering::SeqCst);
        // OLE (not just COM): drag-and-drop and the file clipboard need it.
        OleInitialize(std::ptr::null());
    }
    let code = app::run(has("--startup"), folder);
    release_single_instance();
    std::process::exit(code);
}
