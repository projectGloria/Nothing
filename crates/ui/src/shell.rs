//! Explorer integration: open, reveal, the real shell context menu, drag-out, clipboard.
//! Everything goes through PIDLs + the owning IShellFolder, exactly as Explorer does, so
//! shell extensions (7-Zip, "Open with Code", Git...) show up in our menus too.

use std::cell::Cell;
use std::ffi::c_void;

use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::{OleFlushClipboard, OleSetClipboard, CF_UNICODETEXT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::Shell::{
    ILFindLastID, SHBindToParent, SHDoDragDrop, SHOpenFolderAndSelectItems, SHParseDisplayName, ShellExecuteW, CMF_EXTENDEDVERBS, CMF_NORMAL,
    CMIC_MASK_CONTROL_DOWN, CMIC_MASK_PTINVOKE, CMIC_MASK_SHIFT_DOWN, CMINVOKECOMMANDINFOEX,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreatePopupMenu, DestroyMenu, TrackPopupMenuEx, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON};

use crate::com::{Com, IContextMenuVtbl, IShellFolderVtbl, IID_ICONTEXTMENU, IID_ICONTEXTMENU2, IID_ICONTEXTMENU3, IID_IDATAOBJECT, IID_ISHELLFOLDER};
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

/// The IShellFolder owning the items, plus their child-relative PIDLs. Shell UI objects
/// (menus, data objects) must come from a single folder, so items living elsewhere than
/// the first one's folder are left out.
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

/// Show Explorer's context menu for `paths` at screen point `pt`.
pub fn context_menu(hwnd: HWND, paths: &[String], pt: POINT) {
    let Some(it) = items(paths) else { return };
    let Some(cm) = ui_object(&it, hwnd, &IID_ICONTEXTMENU) else { return };
    unsafe {
        let menu = CreatePopupMenu();
        let flags = CMF_NORMAL | if GetKeyState(VK_SHIFT as i32) < 0 { CMF_EXTENDEDVERBS } else { 0 };
        if (cm.vtbl::<IContextMenuVtbl>().query_context_menu)(cm.0, menu, 0, 1, 0x7FFF, flags) < 0 {
            DestroyMenu(menu);
            return;
        }
        // Prefer the richest interface for owner-drawn submenu forwarding.
        let (cm3, cm2) = (cm.query(&IID_ICONTEXTMENU3), cm.query(&IID_ICONTEXTMENU2));
        let active = match (&cm3, &cm2) {
            (Some(c), _) => (c.0, 3),
            (None, Some(c)) => (c.0, 2),
            _ => (std::ptr::null_mut(), 0),
        };
        ACTIVE.with(|a| a.set(active));
        let cmd = TrackPopupMenuEx(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, hwnd, std::ptr::null());
        ACTIVE.with(|a| a.set((std::ptr::null_mut(), 0)));
        if cmd > 0 {
            // Verb by ordinal: MAKEINTRESOURCE(cmd - idCmdFirst).
            let ord = (cmd - 1) as usize;
            invoke(&cm, hwnd, ord as *const u8, ord as *const u16, pt);
        }
        DestroyMenu(menu);
    }
}

/// Run a canonical verb ("delete", "properties", ...) the way Explorer would, including
/// its confirmation dialogs and Recycle Bin behaviour.
pub fn verb(hwnd: HWND, paths: &[String], verb: &str) {
    let Some(it) = items(paths) else { return };
    let Some(cm) = ui_object(&it, hwnd, &IID_ICONTEXTMENU) else { return };
    unsafe {
        // Several handlers only accept InvokeCommand after QueryContextMenu.
        let menu = CreatePopupMenu();
        (cm.vtbl::<IContextMenuVtbl>().query_context_menu)(cm.0, menu, 0, 1, 0x7FFF, CMF_NORMAL);
        let a: Vec<u8> = verb.bytes().chain(std::iter::once(0)).collect();
        let w = wide(verb);
        invoke(&cm, hwnd, a.as_ptr(), w.as_ptr(), POINT { x: 0, y: 0 });
        DestroyMenu(menu);
    }
}

/// Drag the items out of the list into Explorer, a browser upload box, an editor...
pub fn drag(hwnd: HWND, paths: &[String]) {
    let Some(it) = items(paths) else { return };
    let Some(data) = ui_object(&it, hwnd, &IID_IDATAOBJECT) else { return };
    let mut effect = 0;
    unsafe { SHDoDragDrop(hwnd, data.0, std::ptr::null_mut(), DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK, &mut effect) };
}

/// Ctrl+C: put the files themselves on the clipboard (paste in Explorer copies them).
pub fn copy_files(hwnd: HWND, paths: &[String]) {
    let Some(it) = items(paths) else { return };
    let Some(data) = ui_object(&it, hwnd, &IID_IDATAOBJECT) else { return };
    unsafe {
        if OleSetClipboard(data.0) >= 0 {
            // Render now so the clipboard survives us exiting.
            OleFlushClipboard();
        }
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
