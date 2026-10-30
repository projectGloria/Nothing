//! Thin RAII + helper layer over raw Win32.

use std::io;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LUID};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// Owned kernel handle, closed on drop.
pub struct Handle(pub HANDLE);

// Kernel handles are process-wide; using one from several threads is fine.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    pub fn checked(h: HANDLE) -> io::Result<Handle> {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Handle(h))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Synchronous DeviceIoControl with plain-struct input; returns bytes written.
pub fn ioctl<I>(h: HANDLE, code: u32, input: Option<&I>, out: &mut [u8]) -> io::Result<usize> {
    let mut got = 0u32;
    let (ip, il) = match input {
        Some(i) => (i as *const I as *const _, std::mem::size_of::<I>() as u32),
        None => (std::ptr::null(), 0),
    };
    let ok = unsafe { DeviceIoControl(h, code, ip, il, out.as_mut_ptr() as *mut _, out.len() as u32, &mut got, null_mut()) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(got as usize)
    }
}

/// Read a POD struct out of a byte buffer (unaligned-safe).
pub fn pod<T: Copy>(b: &[u8]) -> T {
    assert!(b.len() >= std::mem::size_of::<T>());
    unsafe { std::ptr::read_unaligned(b.as_ptr() as *const T) }
}

/// SeBackupPrivilege lets `OpenFileById` read metadata of files whose ACLs would
/// otherwise deny us (other users' profiles, System Volume Information...).
pub fn enable_privilege(name: &str) -> bool {
    unsafe {
        let mut tok = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut tok) == 0 {
            return false;
        }
        let tok = Handle(tok);
        let mut luid = LUID { LowPart: 0, HighPart: 0 };
        if LookupPrivilegeValueW(std::ptr::null(), wide(name).as_ptr(), &mut luid) == 0 {
            return false;
        }
        let tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }] };
        AdjustTokenPrivileges(tok.0, 0, &tp, 0, null_mut(), null_mut()) != 0 && last_error() == 0
    }
}

pub fn data_dir() -> std::path::PathBuf {
    let base = std::env::var_os("ProgramData").map(Into::into).unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"));
    base.join("Nothing")
}
