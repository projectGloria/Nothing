//! Dark mode for a raw Win32 app.
//!
//! Windows has no public dark-mode API for classic controls. Explorer, Notepad and
//! Everything all use the same undocumented uxtheme ordinals (stable since 1809):
//! 135 SetPreferredAppMode, 133 AllowDarkModeForWindow, 104 RefreshImmersiveColorPolicyState,
//! 136 FlushMenuThemes. Context menus and scrollbars then go dark for free; the menu bar
//! needs custom painting (see `app::uah_*`).

use windows_sys::Win32::Foundation::{HWND, ERROR_SUCCESS};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

use crate::util::wide;

const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;

/// Apps theme from Settings > Personalization > Colors.
pub fn system_prefers_dark() -> bool {
    // NOTHING_THEME=light|dark overrides the system setting.
    match std::env::var("NOTHING_THEME").as_deref() {
        Ok("light") => return false,
        Ok("dark") => return true,
        _ => {}
    }
    let mut v: u32 = 1;
    let mut sz = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize").as_ptr(),
            wide("AppsUseLightTheme").as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut v as *mut _ as *mut _,
            &mut sz,
        )
    };
    r == ERROR_SUCCESS && v == 0
}

fn uxtheme(ordinal: usize) -> Option<unsafe extern "system" fn() -> isize> {
    unsafe {
        let h = LoadLibraryExW(wide("uxtheme.dll").as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if h.is_null() {
            return None;
        }
        GetProcAddress(h, ordinal as *const u8)
    }
}

/// Process-wide PreferredAppMode for popup/context menus and scrollbars:
/// `None` = follow the system (AllowDark), `Some(true)` = ForceDark, `Some(false)` = ForceLight.
/// Menus cache their theme, so `refresh` (FlushMenuThemes) must follow every change.
pub fn set_app_mode(force: Option<bool>) {
    let mode = match force {
        None => 1,
        Some(true) => 2,
        Some(false) => 3,
    };
    unsafe {
        if let Some(f) = uxtheme(135) {
            let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_mode(mode);
        }
        refresh();
    }
}

pub fn refresh() {
    unsafe {
        if let Some(f) = uxtheme(104) {
            let r: unsafe extern "system" fn() = std::mem::transmute(f);
            r();
        }
        if let Some(f) = uxtheme(136) {
            let r: unsafe extern "system" fn() = std::mem::transmute(f);
            r();
        }
    }
}

pub fn allow_for_window(hwnd: HWND, dark: bool) {
    unsafe {
        if let Some(f) = uxtheme(133) {
            let allow: unsafe extern "system" fn(HWND, i32) -> i32 = std::mem::transmute(f);
            allow(hwnd, dark as i32);
        }
    }
}

pub fn title_bar(hwnd: HWND, dark: bool) {
    let v: i32 = dark as i32;
    unsafe { DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &v as *const _ as *const _, 4) };
}
