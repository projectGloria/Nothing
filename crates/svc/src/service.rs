//! Windows Service Control Manager glue.

use std::sync::{Arc, OnceLock};

use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_PRESHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_PRESHUTDOWN, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};

use crate::engine::Engine;
use crate::win::{enable_privilege, wide};
use crate::{log, server};

pub const SERVICE_NAME: &str = "NothingIndex";

static ENGINE: OnceLock<Arc<Engine>> = OnceLock::new();
static HANDLE: OnceLock<usize> = OnceLock::new();

/// Hand the main thread to the SCM. Returns false when not started by the SCM.
pub fn dispatch() -> bool {
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None },
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) != 0 }
}

fn set_state(state: u32, wait_hint: u32) {
    let Some(&h) = HANDLE.get() else { return };
    let st = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_PRESHUTDOWN } else { 0 },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: wait_hint,
    };
    unsafe { SetServiceStatus(h as SERVICE_STATUS_HANDLE, &st) };
}

unsafe extern "system" fn handler(ctrl: u32, _: u32, _: *mut core::ffi::c_void, _: *mut core::ffi::c_void) -> u32 {
    match ctrl {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_PRESHUTDOWN | SERVICE_CONTROL_SHUTDOWN => {
            set_state(SERVICE_STOP_PENDING, 15_000);
            if let Some(e) = ENGINE.get() {
                e.request_stop();
            }
            0
        }
        SERVICE_CONTROL_INTERROGATE => 0,
        _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let h = unsafe { RegisterServiceCtrlHandlerExW(wide(SERVICE_NAME).as_ptr(), Some(handler), std::ptr::null()) };
    let _ = HANDLE.set(h as usize);
    set_state(SERVICE_START_PENDING, 5_000);
    log::init(false);
    enable_privilege("SeBackupPrivilege");
    let engine = Engine::new(true);
    let _ = ENGINE.set(engine.clone());
    engine.start();
    {
        let e = engine.clone();
        std::thread::spawn(move || server::serve(e));
    }
    set_state(SERVICE_RUNNING, 0);
    log!("service started");
    unsafe { WaitForSingleObject(engine.stop.0, INFINITE) };
    engine.save();
    log!("service stopped");
    set_state(SERVICE_STOPPED, 0);
}
