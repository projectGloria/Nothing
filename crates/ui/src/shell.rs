//! Explorer integration: open, reveal, the real shell context menu, drag-out, clipboard.
//! Everything goes through PIDLs + the owning IShellFolder, exactly as Explorer does, so
//! shell extensions (7-Zip, "Open with Code", Git...) show up in our menus too.

use std::cell::Cell;
use std::ffi::c_void;

use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::{CF_UNICODETEXT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::Shell::{
    ILFindLastID, SHBindToParent, SHDoDragDrop, SHOpenFolderAndSelectItems, SHParseDisplayName, ShellExecuteW, CMF_EXTENDEDVERBS, CMF_NORMAL,
    CMIC_MASK_CONTROL_DOWN, CMIC_MASK_PTINVOKE, CMIC_MASK_SHIFT_DOWN, CMINVOKECOMMANDINFOEX,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetMenuDefaultItem, InsertMenuW, TrackPopupMenuEx, MF_BYPOSITION, MF_SEPARATOR, MF_STRING, SW_SHOWNORMAL, TPM_RETURNCMD,
    TPM_RIGHTBUTTON,
};

use crate::com::{
    Com, IContextMenuVtbl, IShellFolderVtbl, IShellItemArrayVtbl, BHID_DATAOBJECT, BHID_SFUIOBJECT,
    IID_ICONTEXTMENU, IID_ICONTEXTMENU2, IID_ICONTEXTMENU3, IID_IDATAOBJECT, IID_ISHELLFOLDER,
};
use crate::util::wide;

const CMIC_MASK_UNICODE: u32 = 0x4000;

/// Absolute PIDL, freed on drop.
struct Pidl(*mut ITEMIDLIST);

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(self.0 as *const c_void) };
    }
}

fn pidl(path: &str) -> Option<Pidl> {
    let mut p = std::ptr::null_mut();
    let hr = unsafe { SHParseDisplayName(wide(path).as_ptr(), std::ptr::null_mut(), &mut p, 0, std::ptr::null_mut()) };
    (hr >= 0 && !p.is_null()).then_some(Pidl(p))
}

fn parent_dir(p: &str) -> &str {
    p.trim_end_matches('\\').rsplit_once('\\').map(|(d, _)| d).unwrap_or("")
}

#[link(name = "shell32")]
extern "system" {
    fn SHCreateShellItemArrayFromIDLists(
        cidl: u32,
        rgpidl: *const *const ITEMIDLIST,
        ppsiItemArray: *mut *mut c_void,
    ) -> windows_sys::core::HRESULT;
}

enum ShellSelection {
    Array(Com),
    Single(Items),
}

fn by_parent(paths: &[String]) -> Vec<Vec<String>> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for p in paths {
        let dir = parent_dir(p);
        if let Some(g) = groups.iter_mut().find(|(d, _)| d.eq_ignore_ascii_case(dir)) {
            g.1.push(p.clone());
        } else {
            groups.push((dir.to_string(), vec![p.clone()]));
        }
    }
    groups.into_iter().map(|(_, list)| list).collect()
}

fn selection(paths: &[String]) -> Option<ShellSelection> {
    if paths.is_empty() {
        return None;
    }
    let abs: Vec<Pidl> = paths.iter().filter_map(|p| pidl(p)).collect();
    if abs.is_empty() {
        return None;
    }
    let pidl_ptrs: Vec<*const ITEMIDLIST> = abs.iter().map(|p| p.0 as *const ITEMIDLIST).collect();
    let mut array_ptr = std::ptr::null_mut();
    let hr = unsafe {
        SHCreateShellItemArrayFromIDLists(pidl_ptrs.len() as u32, pidl_ptrs.as_ptr(), &mut array_ptr)
    };
    if hr >= 0 && !array_ptr.is_null() {
        return Some(ShellSelection::Array(Com(array_ptr)));
    }
    items(paths).map(ShellSelection::Single)
}

fn guid_eq(a: &windows_sys::core::GUID, b: &windows_sys::core::GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

fn selection_ui_object(sel: &ShellSelection, hwnd: HWND, iid: &windows_sys::core::GUID) -> Option<Com> {
    match sel {
        ShellSelection::Array(com) => {
            let bhid = if guid_eq(iid, &IID_IDATAOBJECT) {
                &BHID_DATAOBJECT
            } else {
                &BHID_SFUIOBJECT
            };
            let mut out = std::ptr::null_mut();
            let hr = unsafe {
                (com.vtbl::<IShellItemArrayVtbl>().bind_to_handler)(com.0, std::ptr::null_mut(), bhid, iid, &mut out)
            };
            (hr >= 0 && !out.is_null()).then_some(Com(out))
        }
        ShellSelection::Single(it) => ui_object(it, hwnd, iid),
    }
}

/// The IShellFolder owning the items, plus their child-relative PIDLs.
struct Items {
    folder: Com,
    _abs: Vec<Pidl>,
    kids: Vec<*const ITEMIDLIST>,
}

fn items(paths: &[String]) -> Option<Items> {
    let first = paths.first()?;
    let dir = parent_dir(first);
    let abs: Vec<Pidl> = paths.iter().filter(|p| parent_dir(p).eq_ignore_ascii_case(dir)).filter_map(|p| pidl(p)).collect();
    let head = abs.first()?;
    let mut folder = std::ptr::null_mut();
    let hr = unsafe { SHBindToParent(head.0, &IID_ISHELLFOLDER, &mut folder, std::ptr::null_mut()) };
    if hr < 0 || folder.is_null() {
        return None;
    }
    let kids = abs.iter().map(|p| unsafe { ILFindLastID(p.0) as *const ITEMIDLIST }).collect();
    Some(Items { folder: Com(folder), _abs: abs, kids })
}

fn ui_object(it: &Items, hwnd: HWND, iid: &windows_sys::core::GUID) -> Option<Com> {
    let mut out = std::ptr::null_mut();
    let hr = unsafe {
        (it.folder.vtbl::<IShellFolderVtbl>().get_ui_object_of)(it.folder.0, hwnd, it.kids.len() as u32, it.kids.as_ptr(), iid, std::ptr::null_mut(), &mut out)
    };
    (hr >= 0 && !out.is_null()).then_some(Com(out))
}

pub fn open(hwnd: HWND, path: &str) {
    unsafe { ShellExecuteW(hwnd, std::ptr::null(), wide(path).as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
}

/// Open the containing folder in Explorer with the item selected.
pub fn reveal(path: &str) {
    if let Some(p) = pidl(path) {
        unsafe { SHOpenFolderAndSelectItems(p.0, 0, std::ptr::null(), 0) };
    }
}

thread_local! {
    /// The context menu currently on screen. Owner-drawn submenus ("Open with", "Send
    /// to") need WM_INITMENUPOPUP/WM_DRAWITEM/... forwarded to it while it is open.
    static ACTIVE: Cell<(*mut c_void, u8)> = const { Cell::new((std::ptr::null_mut(), 0)) };
}

/// Call from the window procedure for menu messages. Some(result) = handled.
pub fn forward_menu_msg(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    let (p, level) = ACTIVE.with(|a| a.get());
    if p.is_null() {
        return None;
    }
    unsafe {
        let v = &**(p as *mut *const IContextMenuVtbl);
        if level == 3 {
            let mut r: LRESULT = 0;
            if (v.handle_menu_msg2)(p, msg, wp, lp, &mut r) >= 0 {
                return Some(r);
            }
        } else if level == 2 && (v.handle_menu_msg)(p, msg, wp, lp) >= 0 {
            return Some(0);
        }
    }
    None
}

fn invoke(cm: &Com, hwnd: HWND, verb_a: *const u8, verb_w: *const u16, pt: POINT) {
    let mut mask = CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE;
    unsafe {
        if GetKeyState(VK_SHIFT as i32) < 0 {
            mask |= CMIC_MASK_SHIFT_DOWN;
        }
        if GetKeyState(VK_CONTROL as i32) < 0 {
            mask |= CMIC_MASK_CONTROL_DOWN;
        }
        let mut ci: CMINVOKECOMMANDINFOEX = std::mem::zeroed();
        ci.cbSize = std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32;
        ci.fMask = mask;
        ci.hwnd = hwnd;
        ci.lpVerb = verb_a;
        ci.lpVerbW = verb_w;
        ci.nShow = SW_SHOWNORMAL;
        ci.ptInvoke = pt;
        (cm.vtbl::<IContextMenuVtbl>().invoke_command)(cm.0, &ci as *const _ as *const c_void);
    }
}

/// Shell handlers get command ids 1..=SHELL_LAST; ids above it are free for the caller's
/// own items (`extra`), so one TrackPopupMenu can serve both.
pub const SHELL_LAST: u32 = 0x7FFF;

/// Show Explorer's context menu for `paths` at screen point `pt`, with `extra` items
/// (id > SHELL_LAST, text; id 0 = separator) inserted right under the default verb.
/// Shell commands run here; a picked `extra` id is returned for the caller to handle.
pub fn context_menu(hwnd: HWND, paths: &[String], pt: POINT, extra: &[(u32, &str)]) -> Option<u32> {
    let sel = selection(paths)?;
    let cm = selection_ui_object(&sel, hwnd, &IID_ICONTEXTMENU)
        .or_else(|| items(paths).and_then(|it| ui_object(&it, hwnd, &IID_ICONTEXTMENU)))?;
    unsafe {
        let menu = CreatePopupMenu();
        let flags = CMF_NORMAL | if GetKeyState(VK_SHIFT as i32) < 0 { CMF_EXTENDEDVERBS } else { 0 };
        if (cm.vtbl::<IContextMenuVtbl>().query_context_menu)(cm.0, menu, 0, 1, SHELL_LAST, flags) < 0 {
            DestroyMenu(menu);
            return None;
        }
        // Under the bold default verb ("Open"), like Everything does; top if there is none.
        let def = GetMenuDefaultItem(menu, 1, 0);
        let first = if def == u32::MAX { 0 } else { def + 1 };
        for (pos, &(id, text)) in (first..).zip(extra.iter().chain(std::iter::once(&(0, "")))) {
            // Bind the UTF-16 buffer first: `wide(text).as_ptr()` inside the `if` would
            // point into a temporary freed before InsertMenuW reads it.
            let w = wide(text);
            let (f, t) = if id == 0 { (MF_SEPARATOR, std::ptr::null()) } else { (MF_STRING, w.as_ptr()) };
            InsertMenuW(menu, pos, MF_BYPOSITION | f, id as usize, t);
        }
        // Prefer the richest interface for owner-drawn submenu forwarding.
        let (cm3, cm2) = (cm.query(&IID_ICONTEXTMENU3), cm.query(&IID_ICONTEXTMENU2));
        let active = match (&cm3, &cm2) {
            (Some(c), _) => (c.0, 3),
            (None, Some(c)) => (c.0, 2),
            _ => (std::ptr::null_mut(), 0),
        };
        ACTIVE.with(|a| a.set(active));
        let cmd = TrackPopupMenuEx(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, hwnd, std::ptr::null()) as u32;
        ACTIVE.with(|a| a.set((std::ptr::null_mut(), 0)));
        let picked = match cmd {
            0 => None,
            c if c > SHELL_LAST => Some(c),
            c => {
                // Verb by ordinal: MAKEINTRESOURCE(cmd - idCmdFirst). The menu must still
                // exist here: some handlers look their items up during InvokeCommand.
                let ord = (c - 1) as usize;
                invoke(&cm, hwnd, ord as *const u8, ord as *const u16, pt);
                None
            }
        };
        DestroyMenu(menu);
        picked
    }
}

/// The first context menu in a process is slow (2-3 s on a machine with WinRAR, 7-Zip,
/// PowerToys, Defender, Drive...): QueryContextMenu loads every shell extension DLL
/// cold, and the user sees "right-click does nothing". Building and throwing away one
/// menu on a background STA pays that once, off the UI thread; the DLLs then stay loaded
/// for the process and the real right-click lands warm (< 1 s, like Explorer's).
pub fn prewarm() {
    // Once per process: show() calls this on every summon.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| unsafe { warm() });
    });
}

unsafe fn warm() {
    unsafe {
        windows_sys::Win32::System::Ole::OleInitialize(std::ptr::null());
        if let Some(exe) = std::env::current_exe().ok().and_then(|p| p.to_str().map(String::from)) {
            if let Some(it) = items(&[exe]) {
                if let Some(cm) = ui_object(&it, std::ptr::null_mut(), &IID_ICONTEXTMENU) {
                    let menu = CreatePopupMenu();
                    (cm.vtbl::<IContextMenuVtbl>().query_context_menu)(cm.0, menu, 0, 1, SHELL_LAST, CMF_NORMAL);
                    DestroyMenu(menu);
                }
            }
        }
        // `it` and `cm` are dropped (Released) above, before the apartment goes away.
        windows_sys::Win32::System::Ole::OleUninitialize();
    }
}

/// Run a canonical verb ("delete", "properties", ...) the way Explorer would, including
/// its confirmation dialogs and Recycle Bin behaviour.
pub fn verb(hwnd: HWND, paths: &[String], verb: &str) {
    if let Some(sel) = selection(paths) {
        if let Some(cm) = selection_ui_object(&sel, hwnd, &IID_ICONTEXTMENU) {
            unsafe {
                let menu = CreatePopupMenu();
                (cm.vtbl::<IContextMenuVtbl>().query_context_menu)(cm.0, menu, 0, 1, 0x7FFF, CMF_NORMAL);
                let a: Vec<u8> = verb.bytes().chain(std::iter::once(0)).collect();
                let w = wide(verb);
                invoke(&cm, hwnd, a.as_ptr(), w.as_ptr(), POINT { x: 0, y: 0 });
                DestroyMenu(menu);
                return;
            }
        }
    }
    // Fallback: explicitly handle every parent group
    for group in by_parent(paths) {
        let Some(it) = items(&group) else { continue };
        let Some(cm) = ui_object(&it, hwnd, &IID_ICONTEXTMENU) else { continue };
        unsafe {
            let menu = CreatePopupMenu();
            (cm.vtbl::<IContextMenuVtbl>().query_context_menu)(cm.0, menu, 0, 1, 0x7FFF, CMF_NORMAL);
            let a: Vec<u8> = verb.bytes().chain(std::iter::once(0)).collect();
            let w = wide(verb);
            invoke(&cm, hwnd, a.as_ptr(), w.as_ptr(), POINT { x: 0, y: 0 });
            DestroyMenu(menu);
        }
    }
}

/// Drag the items out of the list into Explorer, a browser upload box, an editor...
pub fn drag(hwnd: HWND, paths: &[String]) {
    let Some(sel) = selection(paths) else { return };
    let Some(data) = selection_ui_object(&sel, hwnd, &IID_IDATAOBJECT) else { return };
    let mut effect = 0;
    unsafe { SHDoDragDrop(hwnd, data.0, std::ptr::null_mut(), DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK, &mut effect) };
}

#[repr(C)]
#[allow(non_snake_case, clippy::upper_case_acronyms)]
struct DROPFILES {
    pFiles: u32,
    pt: POINT,
    fNC: i32,
    fWide: i32,
}

fn set_clipboard_files(hwnd: HWND, paths: &[String], cut: bool) {
    if paths.is_empty() {
        return;
    }
    let mut data: Vec<u16> = Vec::new();
    for p in paths {
        data.extend(wide(p));
    }
    data.push(0);
    let df_size = std::mem::size_of::<DROPFILES>();
    let total = df_size + data.len() * 2;
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, total);
        if !h.is_null() {
            let ptr = GlobalLock(h) as *mut u8;
            if !ptr.is_null() {
                let df = DROPFILES { pFiles: df_size as u32, pt: POINT { x: 0, y: 0 }, fNC: 0, fWide: 1 };
                std::ptr::copy_nonoverlapping(&df as *const _ as *const u8, ptr, df_size);
                std::ptr::copy_nonoverlapping(data.as_ptr() as *const u8, ptr.add(df_size), data.len() * 2);
                GlobalUnlock(h);
                if SetClipboardData(15 /* CF_HDROP */, h).is_null() {
                    GlobalFree(h);
                }
            }
        }
        let cf_drop = RegisterClipboardFormatW(wide("Preferred DropEffect").as_ptr());
        if cf_drop != 0 {
            let he = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of::<u32>());
            if !he.is_null() {
                let pe = GlobalLock(he) as *mut u32;
                if !pe.is_null() {
                    *pe = if cut { DROPEFFECT_MOVE } else { DROPEFFECT_COPY };
                    GlobalUnlock(he);
                    if SetClipboardData(cf_drop, he).is_null() {
                        GlobalFree(he);
                    }
                }
            }
        }
        let text = paths.join("\r\n");
        let w = wide(&text);
        let ht = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        if !ht.is_null() {
            let pt = GlobalLock(ht) as *mut u16;
            if !pt.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), pt, w.len());
                GlobalUnlock(ht);
                if SetClipboardData(CF_UNICODETEXT as u32, ht).is_null() {
                    GlobalFree(ht);
                }
            }
        }
        CloseClipboard();
    }
}

/// Ctrl+C: put the files themselves on the clipboard (paste in Explorer copies them).
pub fn copy_files(hwnd: HWND, paths: &[String]) {
    set_clipboard_files(hwnd, paths, false);
}

/// Ctrl+X: put the files on the clipboard marked for move (paste in Explorer moves them).
pub fn cut_files(hwnd: HWND, paths: &[String]) {
    set_clipboard_files(hwnd, paths, true);
}

/// Check if the clipboard currently has a Preferred DropEffect (e.g. DROPEFFECT_MOVE or DROPEFFECT_COPY).
pub fn check_clipboard_effect() -> Option<u32> {
    unsafe {
        let cf = RegisterClipboardFormatW(wide("Preferred DropEffect").as_ptr());
        if cf == 0 {
            return None;
        }
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let h = GetClipboardData(cf);
        let effect = if !h.is_null() {
            let p = GlobalLock(h) as *const u32;
            let val = if !p.is_null() { *p } else { 0 };
            GlobalUnlock(h);
            Some(val)
        } else {
            None
        };
        CloseClipboard();
        effect
    }
}

pub fn copy_text(hwnd: HWND, text: &str) {
    let w = wide(text);
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        if !h.is_null() {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                GlobalUnlock(h);
                if SetClipboardData(CF_UNICODETEXT as u32, h).is_null() {
                    GlobalFree(h);
                }
            }
        }
        CloseClipboard();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_context_menu_query() {
        unsafe {
            windows_sys::Win32::System::Ole::OleInitialize(std::ptr::null());
            let path = "C:\\Windows\\notepad.exe".to_string();
            let sel = selection(&[path]).expect("selection should succeed");
            let cm = selection_ui_object(&sel, std::ptr::null_mut(), &IID_ICONTEXTMENU);
            assert!(cm.is_some(), "IContextMenu must be retrieved");
        }
    }
}

