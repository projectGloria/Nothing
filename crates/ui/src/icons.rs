//! File icons from the system image list (the same one Explorer uses, already in memory).
//!
//! Most icons depend only on the extension, so `SHGFI_USEFILEATTRIBUTES` resolves them
//! without touching the disk, cached per extension. Files that carry their own icon
//! (.exe, .lnk, .ico...) need a real disk read, which happens on a background thread;
//! rows show the generic icon until the real one arrives.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Sender};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::UI::Shell::{SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::com::IID_IIMAGELIST;
use crate::util::wide;

const OWN_ICON: &[&str] = &["exe", "lnk", "ico", "url", "cur", "ani", "msc", "cpl", "scr", "appref-ms", "website"];

pub struct Icons {
    pub image_list: isize,
    folder: i32,
    by_ext: HashMap<String, i32>,
    by_path: HashMap<String, i32>,
    pending: HashSet<String>,
    tx: Sender<String>,
}

fn sys_index(path: &str, attrs: u32, use_attrs: bool) -> (isize, i32) {
    let mut sfi: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let flags = SHGFI_SYSICONINDEX | SHGFI_SMALLICON | if use_attrs { SHGFI_USEFILEATTRIBUTES } else { 0 };
    let h = unsafe { SHGetFileInfoW(wide(path).as_ptr(), attrs, &mut sfi, std::mem::size_of::<SHFILEINFOW>() as u32, flags) };
    (h as isize, if h == 0 { -1 } else { sfi.iIcon })
}

/// A bigger size (`SHIL_LARGE` / `SHIL_EXTRALARGE` / `SHIL_JUMBO`) of the same system image
/// list. Every size shares one index space, so the indices we already resolved for the
/// small list are valid here too: switching view needs no re-resolve. Returns an
/// HIMAGELIST (IImageList* and HIMAGELIST are the same pointer). It is a per-process
/// singleton, so the reference we get is deliberately never released.
pub fn sys_list(shil: u32) -> isize {
    let mut p = std::ptr::null_mut();
    let hr = unsafe { SHGetImageList(shil as i32, &IID_IIMAGELIST, &mut p) };
    if hr < 0 { 0 } else { p as isize }
}

impl Icons {
    pub fn new(hwnd: HWND, msg: u32) -> Icons {
        let (image_list, folder) = sys_index("folder", FILE_ATTRIBUTE_DIRECTORY, true);
        let (tx, rx) = channel::<String>();
        let h = hwnd as isize;
        std::thread::spawn(move || {
            unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
            while let Ok(path) = rx.recv() {
                let (_, idx) = sys_index(&path, 0, false);
                let p = Box::into_raw(Box::new((path, idx)));
                if unsafe { PostMessageW(h as HWND, msg, 0, p as isize) } == 0 {
                    drop(unsafe { Box::from_raw(p) });
                }
            }
        });
        Icons { image_list, folder, by_ext: HashMap::new(), by_path: HashMap::new(), pending: HashSet::new(), tx }
    }

    pub fn for_row(&mut self, name: &str, full: &str, is_dir: bool) -> i32 {
        if is_dir {
            if name.ends_with(':') {
                return self.own(full, self.folder); // drive icon
            }
            return self.folder;
        }
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        let generic = *self.by_ext.entry(ext.clone()).or_insert_with(|| sys_index(&format!("x.{ext}"), FILE_ATTRIBUTE_NORMAL, true).1);
        if OWN_ICON.contains(&ext.as_str()) {
            return self.own(full, generic);
        }
        generic
    }

    fn own(&mut self, full: &str, fallback: i32) -> i32 {
        if let Some(&i) = self.by_path.get(full) {
            return i;
        }
        if self.pending.len() < 4096 && self.pending.insert(full.to_string()) {
            let _ = self.tx.send(full.to_string());
        }
        fallback
    }

    pub fn resolved(&mut self, path: String, idx: i32) {
        self.pending.remove(&path);
        if self.by_path.len() > 50_000 {
            self.by_path.clear();
        }
        if idx >= 0 {
            self.by_path.insert(path, idx);
        }
    }
}
