//! Persistent UI settings in %APPDATA%\Nothing\settings.ini (plain INI: easy to hand-edit,
//! e.g. to change the hotkey).

use std::path::PathBuf;

use windows_sys::Win32::System::WindowsProgramming::{GetPrivateProfileIntW, WritePrivateProfileStringW};

use crate::util::wide;

// RegisterHotKey modifiers, as stored in settings.ini ([hotkey] modifiers = sum of):
// Alt = 1, Ctrl = 2, Shift = 4, Win = 8.
pub const MOD_ALT: u32 = 0x1;
pub const MOD_WIN: u32 = 0x8;

#[derive(Clone)]
pub struct Settings {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub maximized: bool,
    /// Column widths at 96 DPI.
    pub cols: [i32; 4],
    pub sort: u8,
    pub desc: bool,
    pub match_case: bool,
    pub match_path: bool,
    pub hotkey_mods: u32,
    pub hotkey_vk: u32,
    /// 0 = follow Windows, 1 = light, 2 = dark, 3 = black (see `app::Theme`).
    pub theme: u8,
    /// 0 = details, 1 = medium icons, 2 = large icons, 3 = extra large icons.
    pub view: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            x: i32::MIN,
            y: i32::MIN,
            w: 1120,
            h: 670,
            maximized: false,
            cols: [230, 520, 120, 150],
            sort: 0,
            desc: false,
            match_case: false,
            match_path: false,
            hotkey_mods: MOD_WIN | MOD_ALT,
            hotkey_vk: 0x20, // Space
            theme: 0,
            view: 0,
        }
    }
}

fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Nothing").join("settings.ini")
}

fn get(file: &[u16], sec: &str, key: &str, def: i32) -> i32 {
    unsafe { GetPrivateProfileIntW(wide(sec).as_ptr(), wide(key).as_ptr(), def, file.as_ptr()) as i32 }
}

fn set(file: &[u16], sec: &str, key: &str, v: i32) {
    unsafe { WritePrivateProfileStringW(wide(sec).as_ptr(), wide(key).as_ptr(), wide(&v.to_string()).as_ptr(), file.as_ptr()) };
}

impl Settings {
    pub fn load() -> Settings {
        let d = Settings::default();
        let p = path();
        if !p.exists() {
            return d;
        }
        let f = wide(&p.display().to_string());
        Settings {
            x: get(&f, "window", "x", d.x),
            y: get(&f, "window", "y", d.y),
            w: get(&f, "window", "w", d.w).max(320),
            h: get(&f, "window", "h", d.h).max(200),
            maximized: get(&f, "window", "maximized", 0) != 0,
            cols: [
                get(&f, "columns", "name", d.cols[0]),
                get(&f, "columns", "path", d.cols[1]),
                get(&f, "columns", "size", d.cols[2]),
                get(&f, "columns", "date", d.cols[3]),
            ],
            sort: get(&f, "search", "sort", 0).clamp(0, 3) as u8,
            desc: get(&f, "search", "desc", 0) != 0,
            match_case: get(&f, "search", "match_case", 0) != 0,
            match_path: get(&f, "search", "match_path", 0) != 0,
            hotkey_mods: get(&f, "hotkey", "modifiers", d.hotkey_mods as i32) as u32,
            hotkey_vk: get(&f, "hotkey", "key", d.hotkey_vk as i32) as u32,
            theme: get(&f, "ui", "theme", 0).clamp(0, 3) as u8,
            view: get(&f, "ui", "view", 0).clamp(0, 3) as u8,
        }
    }

    pub fn save(&self) {
        let p = path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let f = wide(&p.display().to_string());
        set(&f, "window", "x", self.x);
        set(&f, "window", "y", self.y);
        set(&f, "window", "w", self.w);
        set(&f, "window", "h", self.h);
        set(&f, "window", "maximized", self.maximized as i32);
        for (k, v) in ["name", "path", "size", "date"].iter().zip(self.cols) {
            set(&f, "columns", k, v);
        }
        set(&f, "search", "sort", self.sort as i32);
        set(&f, "search", "desc", self.desc as i32);
        set(&f, "search", "match_case", self.match_case as i32);
        set(&f, "search", "match_path", self.match_path as i32);
        set(&f, "hotkey", "modifiers", self.hotkey_mods as i32);
        set(&f, "hotkey", "key", self.hotkey_vk as i32);
        set(&f, "ui", "theme", self.theme as i32);
        set(&f, "ui", "view", self.view as i32);
    }
}
