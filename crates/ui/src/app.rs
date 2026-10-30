//! The main window: search box, virtual result list, status bar, menu, tray, hotkey.
//!
//! The list is a `LVS_OWNERDATA` (virtual) ListView: it stores no items at all, only a
//! count. Windows asks for the text of the ~40 rows it is about to paint, and we answer
//! from a small page cache filled asynchronously by the IPC thread. A million results
//! cost exactly as much UI memory as forty.

use std::collections::{HashMap, HashSet};
use std::ptr::{null, null_mut};

use nothing_core::flag;
use nothing_core::proto::{state, Row, Status};
use nothing_core::search::{OPT_CASE, OPT_PATH};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{
    DefSubclassProc, SetWindowSubclass, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::icons::Icons;
use crate::ipc::{Cmd, Event, Ipc};
use crate::settings::Settings;
use crate::util::{date, from_wide, human_bytes, join_path, loword, rgb, size_kb, thousands, wide};
use crate::{dark, install, shell};

pub const CLASS: &str = "NothingMainWnd";
pub const WM_APP_SHOW: u32 = WM_APP + 4;
const WM_APP_IPC: u32 = WM_APP + 1;
const WM_APP_ICON: u32 = WM_APP + 2;
const WM_APP_TRAY: u32 = WM_APP + 3;

const PAGE: u32 = 100;
const TIMER_STATUS: usize = 1;
const HOTKEY_ID: i32 = 1;

// Menu command ids.
const ID_OPEN: u16 = 1001;
const ID_REVEAL: u16 = 1002;
const ID_EXIT: u16 = 1003;
const ID_COPY: u16 = 1010;
const ID_COPY_PATH: u16 = 1011;
const ID_SELECT_ALL: u16 = 1012;
const ID_CASE: u16 = 1020;
const ID_PATH: u16 = 1021;
const ID_SORT_BASE: u16 = 1030; // +0..3
const ID_INSTALL: u16 = 1040;
const ID_UNINSTALL: u16 = 1041;
const ID_SYNTAX: u16 = 1050;
const ID_ABOUT: u16 = 1051;
const ID_TRAY_SHOW: u16 = 1060;

// Undocumented-but-stable messages used to paint the menu bar in dark mode.
const WM_UAHDRAWMENU: u32 = 0x0091;
const WM_UAHDRAWMENUITEM: u32 = 0x0092;

#[repr(C)]
struct UahMenu {
    hmenu: HMENU,
    hdc: HDC,
    flags: u32,
}

#[repr(C)]
struct UahDrawMenuItem {
    dis: DRAWITEMSTRUCT,
    um: UahMenu,
    position: i32,
    _metrics: [u32; 8],
    _popup: [u32; 5],
}

/// One cached result row, pre-formatted for painting.
struct RowView {
    name: Vec<u16>,
    path: Vec<u16>,
    size: Vec<u16>,
    date: Vec<u16>,
    icon: i32,
    bytes: u64,
    full: String,
}

struct Theme {
    dark: bool,
    fg: u32,
    dim: u32,
    field: u32,
    br_bg: HBRUSH,
    br_field: HBRUSH,
    br_hot: HBRUSH,
}

impl Theme {
    fn new(dark: bool) -> Theme {
        let (bg, fg, dim, field, hot) = if dark {
            (rgb(32, 32, 32), rgb(232, 232, 232), rgb(160, 160, 160), rgb(16, 16, 16), rgb(64, 64, 64))
        } else {
            (rgb(243, 243, 243), rgb(20, 20, 20), rgb(96, 96, 96), rgb(255, 255, 255), rgb(222, 222, 222))
        };
        unsafe { Theme { dark, fg, dim, field, br_bg: CreateSolidBrush(bg), br_field: CreateSolidBrush(field), br_hot: CreateSolidBrush(hot) } }
    }
}

impl Drop for Theme {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.br_bg as _);
            DeleteObject(self.br_field as _);
            DeleteObject(self.br_hot as _);
        }
    }
}

pub struct App {
    hwnd: HWND,
    edit: HWND,
    list: HWND,
    header: HWND,
    menu: HMENU,
    font: HFONT,
    dpi: u32,
    theme: Theme,
    ipc: Ipc,
    icons: Icons,
    settings: Settings,

    gen: u32,
    inflight: Option<u32>,
    have_result: bool,
    total: u32,
    micros: u32,
    rows: HashMap<u32, RowView>,
    /// Rows from before an in-place refresh, painted until fresh ones arrive (no flicker).
    stale: HashMap<u32, RowView>,
    requested: HashSet<u32>,

    status: Option<Status>,
    connected: bool,
    last_version: u64,
    visible: bool,
    exiting: bool,
    last_focus: HWND,
    taskbar_created: u32,
    hotkey_ok: bool,
    offline_note: String,
}

thread_local! {
    static APP: std::cell::Cell<*mut App> = const { std::cell::Cell::new(null_mut()) };
}

fn app() -> Option<&'static mut App> {
    // SAFETY: the App lives for the whole message loop and is only touched on the UI thread.
    APP.with(|a| unsafe { a.get().as_mut() })
}

pub fn message(title: &str, text: &str, error: bool) {
    let flags = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe { MessageBoxW(null_mut(), wide(text).as_ptr(), wide(title).as_ptr(), flags | MB_OK) };
}

fn scale(v: i32, dpi: u32) -> i32 {
    (v as i64 * dpi as i64 / 96) as i32
}

fn make_font(dpi: u32) -> HFONT {
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
        ncm.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, ncm.cbSize, &mut ncm as *mut _ as *mut _, 0, dpi);
        CreateFontIndirectW(&ncm.lfMessageFont)
    }
}

fn build_menu() -> HMENU {
    unsafe {
        let add = |m: HMENU, id: u16, text: &str| {
            AppendMenuW(m, MF_STRING, id as usize, wide(text).as_ptr());
        };
        let sep = |m: HMENU| {
            AppendMenuW(m, MF_SEPARATOR, 0, null());
        };
        let bar = CreateMenu();
        let file = CreatePopupMenu();
        add(file, ID_OPEN, "&Open\tEnter");
        add(file, ID_REVEAL, "Open &Path\tCtrl+Enter");
        sep(file);
        add(file, ID_EXIT, "E&xit");
        let edit = CreatePopupMenu();
        add(edit, ID_COPY, "&Copy\tCtrl+C");
        add(edit, ID_COPY_PATH, "Copy &Full Path\tCtrl+Shift+C");
        sep(edit);
        add(edit, ID_SELECT_ALL, "Select &All\tCtrl+A");
        let search = CreatePopupMenu();
        add(search, ID_CASE, "Match &Case\tCtrl+I");
        add(search, ID_PATH, "Match &Path\tCtrl+U");
        sep(search);
        for (i, n) in ["Name", "Path", "Size", "Date Modified"].iter().enumerate() {
            add(search, ID_SORT_BASE + i as u16, &format!("Sort by &{n}"));
        }
        let tools = CreatePopupMenu();
        add(tools, ID_INSTALL, "&Install / Repair Indexing Service…");
        add(tools, ID_UNINSTALL, "&Uninstall…");
        let help = CreatePopupMenu();
        add(help, ID_SYNTAX, "Search &Syntax");
        add(help, ID_ABOUT, "&About Nothing");
        for (m, t) in [(file, "&File"), (edit, "&Edit"), (search, "&Search"), (tools, "&Tools"), (help, "&Help")] {
            AppendMenuW(bar, MF_POPUP, m as usize, wide(t).as_ptr());
        }
        bar
    }
}

/// Create the window and run the message loop. Returns the exit code.
pub fn run(start_hidden: bool) -> i32 {
    unsafe {
        let hinst = GetModuleHandleW(null());
        let icc = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES };
        InitCommonControlsEx(&icc);
        dark::init_app();

        let class = wide(CLASS);
        let icon = LoadImageW(hinst, 1 as _, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE | LR_SHARED) as HICON;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: icon,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: icon,
        };
        RegisterClassExW(&wc);

        let settings = Settings::load();
        let menu = build_menu();
        let (x, y) = if settings.x == i32::MIN { (CW_USEDEFAULT, CW_USEDEFAULT) } else { (settings.x, settings.y) };
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("Nothing").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            x,
            y,
            settings.w,
            settings.h,
            null_mut(),
            menu,
            hinst,
            null(),
        );
        if hwnd.is_null() {
            return 1;
        }
        let dpi = GetDpiForWindow(hwnd).max(96);
        if settings.x == i32::MIN {
            // First run: size for the monitor's DPI (settings store 96-DPI-ish pixels).
            SetWindowPos(hwnd, null_mut(), 0, 0, scale(settings.w, dpi), scale(settings.h, dpi), SWP_NOMOVE | SWP_NOZORDER);
        }

        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            wide("EDIT").as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            0,
            0,
            0,
            0,
            hwnd,
            100 as HMENU,
            hinst,
            null(),
        );
        SendMessageW(edit, EM_SETCUEBANNER, 1, wide("Search").as_ptr() as isize);
        let list = CreateWindowExW(
            0,
            WC_LISTVIEWW,
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | LVS_REPORT | LVS_OWNERDATA | LVS_SHAREIMAGELISTS | LVS_SHOWSELALWAYS,
            0,
            0,
            0,
            0,
            hwnd,
            101 as HMENU,
            hinst,
            null(),
        );
        let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP;
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, ex as usize, ex as isize);
        SendMessageW(list, LVM_SETCALLBACKMASK, 0, 0);
        let header = SendMessageW(list, LVM_GETHEADER, 0, 0) as HWND;
        SetWindowSubclass(list, Some(list_proc), 1, 0);
        SetWindowSubclass(edit, Some(edit_proc), 2, 0);

        let icons = Icons::new(hwnd, WM_APP_ICON);
        SendMessageW(list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, icons.image_list);

        let boxed = Box::new(App {
            hwnd,
            edit,
            list,
            header,
            menu,
            font: null_mut(),
            dpi,
            theme: Theme::new(dark::system_prefers_dark()),
            ipc: Ipc::spawn(hwnd, WM_APP_IPC),
            icons,
            settings,
            gen: 0,
            inflight: None,
            have_result: false,
            total: 0,
            micros: 0,
            rows: HashMap::new(),
            stale: HashMap::new(),
            requested: HashSet::new(),
            status: None,
            connected: true,
            last_version: 0,
            visible: false,
            exiting: false,
            last_focus: edit,
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            hotkey_ok: false,
            offline_note: String::new(),
        });
        APP.with(|a| a.set(Box::into_raw(boxed)));
        let a = app().unwrap();
        for (i, (name, fmt)) in [("Name", LVCFMT_LEFT), ("Path", LVCFMT_LEFT), ("Size", LVCFMT_RIGHT), ("Date Modified", LVCFMT_LEFT)].iter().enumerate() {
            let mut text = wide(name);
            let col = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT | LVCF_SUBITEM,
                fmt: *fmt,
                cx: scale(a.settings.cols[i], dpi),
                pszText: text.as_mut_ptr(),
                cchTextMax: 0,
                iSubItem: i as i32,
                iImage: 0,
                iOrder: 0,
                cxMin: 0,
                cxDefault: 0,
                cxIdeal: 0,
            };
            SendMessageW(list, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
        }
        a.apply_dpi(dpi);
        a.apply_theme();
        a.update_menu_checks();
        a.update_sort_arrow();
        a.add_tray();
        a.register_hotkey();
        a.layout();
        a.run_query(false);
        if !start_hidden {
            a.show();
            if a.settings.maximized {
                ShowWindow(hwnd, SW_MAXIMIZE);
            }
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if let Some(a) = app() {
                if a.pre_translate(&msg) {
                    continue;
                }
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        msg.wParam as i32
    }
}

impl App {
    // ---- querying ---------------------------------------------------------------------

    fn opts(&self) -> u8 {
        (if self.settings.match_case { OPT_CASE } else { 0 }) | (if self.settings.match_path { OPT_PATH } else { 0 })
    }

    fn search_text(&self) -> String {
        unsafe {
            let n = GetWindowTextLengthW(self.edit);
            let mut buf = vec![0u16; n as usize + 1];
            GetWindowTextW(self.edit, buf.as_mut_ptr(), n + 1);
            from_wide(&buf)
        }
    }

    /// `keep` = refresh in place (index changed) instead of a new search.
    fn run_query(&mut self, keep: bool) {
        self.gen = self.gen.wrapping_add(1);
        self.inflight = Some(self.gen);
        self.requested.clear();
        if keep {
            let old = std::mem::take(&mut self.rows);
            self.stale = old;
        } else {
            self.rows.clear();
            self.stale.clear();
        }
        let first_row = if keep { unsafe { SendMessageW(self.list, LVM_GETTOPINDEX, 0, 0) as u32 } } else { 0 };
        self.ipc.send(Cmd::Query {
            gen: self.gen,
            text: self.search_text(),
            sort: self.settings.sort,
            desc: self.settings.desc,
            opts: self.opts(),
            first_row,
            keep,
        });
    }

    fn request_page(&mut self, page: u32) {
        if page * PAGE < self.total && self.requested.insert(page) {
            self.ipc.send(Cmd::Rows { gen: self.gen, first: page * PAGE, count: PAGE });
        }
    }

    fn make_row(&mut self, r: Row) -> RowView {
        let is_dir = r.flags & flag::DIR != 0;
        let full = join_path(&r.path, &r.name);
        let icon = self.icons.for_row(&r.name, &full, is_dir);
        RowView {
            name: wide(&r.name),
            path: wide(&r.path),
            size: wide(&if is_dir { String::new() } else { size_kb(r.size) }),
            date: wide(&date(r.mtime)),
            icon,
            bytes: if is_dir { 0 } else { r.size },
            full,
        }
    }

    fn store_rows(&mut self, first: u32, rows: Vec<Row>) {
        let n = rows.len() as u32;
        for (i, r) in rows.into_iter().enumerate() {
            let idx = first + i as u32;
            let v = self.make_row(r);
            self.stale.remove(&idx);
            self.rows.insert(idx, v);
        }
        // Mark fully covered pages as fetched.
        let mut p = first.div_ceil(PAGE);
        while (p + 1) * PAGE <= first + n {
            self.requested.insert(p);
            p += 1;
        }
        // Bound the cache: keep what's near the viewport.
        if self.rows.len() > 6000 {
            let top = unsafe { SendMessageW(self.list, LVM_GETTOPINDEX, 0, 0) as i64 };
            self.rows.retain(|&k, _| (k as i64 - top).abs() < 2000);
            self.requested.clear();
        }
        if n > 0 {
            unsafe { SendMessageW(self.list, LVM_REDRAWITEMS, first as usize, (first + n - 1) as isize) };
        }
    }

    fn on_ipc(&mut self, ev: Event) {
        match ev {
            Event::Result { gen, total, micros, keep, first, rows } => {
                if gen != self.gen {
                    return;
                }
                self.inflight = None;
                self.have_result = true;
                self.connected = true;
                self.total = total;
                self.micros = micros;
                unsafe {
                    if keep {
                        SendMessageW(self.list, LVM_SETITEMCOUNT, total as usize, (LVSICF_NOSCROLL | LVSICF_NOINVALIDATEALL) as isize);
                    } else {
                        SendMessageW(self.list, LVM_SETITEMCOUNT, total as usize, 0);
                        let mut st: LVITEMW = std::mem::zeroed();
                        st.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
                        SendMessageW(self.list, LVM_SETITEMSTATE, usize::MAX, &st as *const _ as isize);
                        if total > 0 {
                            SendMessageW(self.list, LVM_ENSUREVISIBLE, 0, 0);
                        }
                    }
                }
                self.store_rows(first, rows);
                unsafe { InvalidateRect(self.list, null(), 0) };
                self.invalidate_status();
            }
            Event::Rows { gen, first, rows } => {
                if gen == self.gen {
                    self.store_rows(first, rows);
                }
            }
            Event::Status(st) => {
                match st {
                    Some(s) => {
                        let reconnected = !self.connected || !self.have_result;
                        self.connected = true;
                        let changed = s.version != self.last_version;
                        self.last_version = s.version;
                        self.status = Some(s);
                        if self.inflight.is_none() && (reconnected || changed) {
                            self.run_query(self.have_result);
                        }
                    }
                    None => {
                        if self.connected || self.total != 0 {
                            // Don't show a count for rows we can no longer fetch.
                            self.total = 0;
                            self.rows.clear();
                            self.stale.clear();
                            unsafe { SendMessageW(self.list, LVM_SETITEMCOUNT, 0, 0) };
                        }
                        self.connected = false;
                        self.have_result = false;
                        self.inflight = None;
                        // Asked once per failed poll, not on every repaint.
                        self.offline_note = match install::service_state() {
                            None => "Indexing service not installed — Tools ▸ Install".into(),
                            Some(_) => "Indexing service not running".into(),
                        };
                    }
                }
                self.invalidate_status();
            }
        }
    }

    // ---- painting & layout ------------------------------------------------------------

    fn metrics(&self) -> (i32, i32, i32) {
        let pad = scale(4, self.dpi);
        let edit_h = scale(26, self.dpi);
        let status_h = scale(24, self.dpi);
        (pad, edit_h, status_h)
    }

    fn layout(&self) {
        unsafe {
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(self.hwnd, &mut rc);
            let (pad, edit_h, status_h) = self.metrics();
            let w = rc.right - rc.left;
            let h = rc.bottom - rc.top;
            MoveWindow(self.edit, pad, pad, w - 2 * pad, edit_h, 1);
            let top = edit_h + 2 * pad;
            MoveWindow(self.list, 0, top, w, (h - top - status_h).max(0), 1);
            self.invalidate_status();
        }
    }

    fn status_rect(&self) -> RECT {
        let mut rc: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut rc) };
        rc.top = rc.bottom - self.metrics().2;
        rc
    }

    fn invalidate_status(&self) {
        let rc = self.status_rect();
        unsafe { InvalidateRect(self.hwnd, &rc, 0) };
    }

    fn selected_count(&self) -> u32 {
        unsafe { SendMessageW(self.list, LVM_GETSELECTEDCOUNT, 0, 0) as u32 }
    }

    fn status_texts(&self) -> (String, String) {
        let mut left = format!("{} {}", thousands(self.total as u64), if self.total == 1 { "item" } else { "items" });
        let sel = self.selected_count();
        let mut right = String::new();
        if sel > 0 {
            left.push_str(&format!("   ·   {} selected", thousands(sel as u64)));
            // Total size of the selection, when every selected row is in the cache.
            if sel <= 5000 {
                let mut sum = 0u64;
                let mut files = 0;
                let mut all = true;
                let mut i = -1i32;
                loop {
                    i = unsafe { SendMessageW(self.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize) as i32 };
                    if i < 0 {
                        break;
                    }
                    match self.rows.get(&(i as u32)) {
                        Some(r) => {
                            sum += r.bytes;
                            files += !r.size.starts_with(&[0]) as u32; // folders have no size text
                        }
                        None => {
                            all = false;
                            break;
                        }
                    }
                }
                if all && files > 0 {
                    right = human_bytes(sum);
                }
            }
        }
        if !self.connected {
            right = self.offline_note.clone();
        } else if let Some(s) = &self.status {
            if s.state != state::READY {
                right = s.message.clone();
            }
        }
        (left, right)
    }

    fn paint(&self) {
        unsafe {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(self.hwnd, &mut rc);
            let (pad, edit_h, _) = self.metrics();
            // Strip around the search box.
            let top = RECT { left: 0, top: 0, right: rc.right, bottom: edit_h + 2 * pad };
            FillRect(hdc, &top, self.theme.br_bg);
            // Status bar.
            let sr = self.status_rect();
            FillRect(hdc, &sr, self.theme.br_bg);
            let old = SelectObject(hdc, self.font as _);
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, self.theme.fg);
            let (left, right) = self.status_texts();
            let mut r = RECT { left: sr.left + scale(8, self.dpi), right: sr.right - scale(10, self.dpi), ..sr };
            let mut lw = wide(&left);
            DrawTextW(hdc, lw.as_mut_ptr(), -1, &mut r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            SetTextColor(hdc, self.theme.dim);
            let mut rw = wide(&right);
            DrawTextW(hdc, rw.as_mut_ptr(), -1, &mut r, DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(hdc, old);
            EndPaint(self.hwnd, &ps);
        }
    }

    fn apply_dpi(&mut self, dpi: u32) {
        self.dpi = dpi;
        let old = self.font;
        self.font = make_font(dpi);
        unsafe {
            SendMessageW(self.edit, WM_SETFONT, self.font as usize, 1);
            SendMessageW(self.list, WM_SETFONT, self.font as usize, 1);
            if !old.is_null() {
                DeleteObject(old as _);
            }
        }
        self.layout();
    }

    fn apply_theme(&mut self) {
        let dark = dark::system_prefers_dark();
        self.theme = Theme::new(dark);
        let t = &self.theme;
        unsafe {
            for h in [self.hwnd, self.list, self.header, self.edit] {
                dark::allow_for_window(h, dark);
            }
            dark::refresh();
            dark::title_bar(self.hwnd, dark);
            let (lv, hd, ed) = if dark { ("DarkMode_Explorer", "DarkMode_ItemsView", "DarkMode_CFD") } else { ("ItemsView", "ItemsView", "CFD") };
            SetWindowTheme(self.list, wide(lv).as_ptr(), null());
            SetWindowTheme(self.header, wide(hd).as_ptr(), null());
            SetWindowTheme(self.edit, wide(ed).as_ptr(), null());
            SendMessageW(self.list, LVM_SETBKCOLOR, 0, t.field as isize);
            SendMessageW(self.list, LVM_SETTEXTBKCOLOR, 0, t.field as isize);
            SendMessageW(self.list, LVM_SETTEXTCOLOR, 0, t.fg as isize);
            for h in [self.list, self.header, self.edit] {
                SendMessageW(h, WM_THEMECHANGED, 0, 0);
            }
            DrawMenuBar(self.hwnd);
            InvalidateRect(self.hwnd, null(), 1);
        }
    }

    fn update_menu_checks(&self) {
        unsafe {
            let chk = |id: u16, on: bool| {
                CheckMenuItem(self.menu, id as u32, MF_BYCOMMAND | if on { MF_CHECKED } else { MF_UNCHECKED });
            };
            chk(ID_CASE, self.settings.match_case);
            chk(ID_PATH, self.settings.match_path);
            CheckMenuRadioItem(self.menu, ID_SORT_BASE as u32, ID_SORT_BASE as u32 + 3, ID_SORT_BASE as u32 + self.settings.sort as u32, MF_BYCOMMAND);
        }
    }

    fn update_sort_arrow(&self) {
        unsafe {
            for i in 0..4 {
                let mut it: HDITEMW = std::mem::zeroed();
                it.mask = HDI_FORMAT;
                SendMessageW(self.header, HDM_GETITEM, i, &mut it as *mut _ as isize);
                it.fmt &= !(HDF_SORTUP | HDF_SORTDOWN);
                if i as u8 == self.settings.sort {
                    it.fmt |= if self.settings.desc { HDF_SORTDOWN } else { HDF_SORTUP };
                }
                SendMessageW(self.header, HDM_SETITEMW, i, &it as *const _ as isize);
            }
        }
    }

    fn set_sort(&mut self, col: u8) {
        if self.settings.sort == col {
            self.settings.desc = !self.settings.desc;
        } else {
            self.settings.sort = col;
            // Biggest / newest first is what you want when you click Size or Date.
            self.settings.desc = col >= 2;
        }
        self.update_sort_arrow();
        self.update_menu_checks();
        self.run_query(false);
    }

    // ---- window state -----------------------------------------------------------------

    fn show(&mut self) {
        unsafe {
            if IsIconic(self.hwnd) != 0 {
                ShowWindow(self.hwnd, SW_RESTORE);
            } else {
                ShowWindow(self.hwnd, SW_SHOW);
            }
            SetForegroundWindow(self.hwnd);
            SetFocus(self.edit);
            SendMessageW(self.edit, EM_SETSEL, 0, -1);
            self.visible = true;
            SetTimer(self.hwnd, TIMER_STATUS, 500, None);
        }
        self.ipc.send(Cmd::Status);
    }

    fn hide(&mut self) {
        self.save_settings();
        unsafe {
            KillTimer(self.hwnd, TIMER_STATUS);
            ShowWindow(self.hwnd, SW_HIDE);
        }
        self.visible = false;
    }

    fn toggle(&mut self) {
        let fg = unsafe { GetForegroundWindow() };
        if self.visible && fg == self.hwnd {
            self.hide();
        } else {
            self.show();
        }
    }

    fn save_settings(&mut self) {
        unsafe {
            let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
            wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            if GetWindowPlacement(self.hwnd, &mut wp) != 0 {
                let r = wp.rcNormalPosition;
                self.settings.x = r.left;
                self.settings.y = r.top;
                self.settings.w = r.right - r.left;
                self.settings.h = r.bottom - r.top;
                self.settings.maximized = wp.showCmd == SW_SHOWMAXIMIZED as u32 || IsZoomed(self.hwnd) != 0;
            }
            for i in 0..4 {
                let w = SendMessageW(self.list, LVM_GETCOLUMNWIDTH, i, 0) as i32;
                if w > 0 {
                    self.settings.cols[i] = w * 96 / self.dpi as i32;
                }
            }
        }
        self.settings.save();
    }

    fn exit(&mut self) {
        self.exiting = true;
        self.save_settings();
        self.remove_tray();
        unsafe { DestroyWindow(self.hwnd) };
    }

    fn tray_data(&self) -> NOTIFYICONDATAW {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd;
        nid.uID = 1;
        nid
    }

    fn add_tray(&self) {
        let mut nid = self.tray_data();
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        unsafe {
            nid.hIcon = LoadImageW(GetModuleHandleW(null()), 1 as _, IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), LR_SHARED) as HICON;
        }
        let tip = wide("Nothing — instant file search");
        nid.szTip[..tip.len().min(127)].copy_from_slice(&tip[..tip.len().min(127)]);
        unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
    }

    fn remove_tray(&self) {
        let nid = self.tray_data();
        unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
    }

    fn tray_menu(&mut self) {
        unsafe {
            let m = CreatePopupMenu();
            AppendMenuW(m, MF_STRING, ID_TRAY_SHOW as usize, wide("&Show Nothing").as_ptr());
            AppendMenuW(m, MF_SEPARATOR, 0, null());
            AppendMenuW(m, MF_STRING, ID_EXIT as usize, wide("E&xit").as_ptr());
            SetMenuDefaultItem(m, ID_TRAY_SHOW as u32, 0);
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            // Required for the menu to close when clicking elsewhere (KB135788).
            SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenuEx(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, self.hwnd, null());
            DestroyMenu(m);
            match cmd as u16 {
                ID_TRAY_SHOW => self.show(),
                ID_EXIT => self.exit(),
                _ => {}
            }
        }
    }

    fn register_hotkey(&mut self) {
        const MOD_NOREPEAT: u32 = 0x4000;
        self.hotkey_ok = unsafe { RegisterHotKey(self.hwnd, HOTKEY_ID, self.settings.hotkey_mods | MOD_NOREPEAT, self.settings.hotkey_vk) } != 0;
    }

    // ---- selection & actions ----------------------------------------------------------

    /// Full paths of selected rows (up to `cap`). Rows scrolled out of the cache are
    /// fetched synchronously first — rare, and only on an explicit user action.
    fn selected_paths(&mut self, cap: usize) -> Vec<String> {
        let mut idx = Vec::new();
        let mut i = -1i32;
        loop {
            i = unsafe { SendMessageW(self.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize) as i32 };
            if i < 0 || idx.len() >= cap {
                break;
            }
            idx.push(i as u32);
        }
        let missing: Vec<u32> = idx.iter().copied().filter(|i| !self.rows.contains_key(i)).collect();
        if !missing.is_empty() {
            self.fetch_sync(&missing);
        }
        idx.iter().filter_map(|i| self.rows.get(i).map(|r| r.full.clone())).collect()
    }

    fn fetch_sync(&mut self, missing: &[u32]) {
        // A private pipe session can't see our result set, so re-run the query there and
        // read the needed ranges. Same text, same sort => same order.
        use nothing_core::proto::{self, Request, R};
        let Ok(mut p) = std::fs::OpenOptions::new().read(true).write(true).open(proto::PIPE_NAME) else { return };
        let mut buf = Vec::new();
        let q = Request::Query { gen: 0, sort: self.settings.sort, desc: self.settings.desc, opts: self.opts(), text: self.search_text() };
        if proto::write_request(&mut p, &q).is_err() || proto::read_frame(&mut p, &mut buf).is_err() {
            return;
        }
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for &m in missing {
            match ranges.last_mut() {
                Some((a, n)) if *a + *n == m && *n < 2000 => *n += 1,
                _ => ranges.push((m, 1)),
            }
        }
        for (first, count) in ranges {
            if proto::write_request(&mut p, &Request::Rows { gen: 0, first, count }).is_err() || proto::read_frame(&mut p, &mut buf).is_err() {
                return;
            }
            let mut r = R::new(&buf);
            let (_, f, n) = (r.u32(), r.u32(), r.u32());
            let rows: Vec<Row> = (0..n).map(|_| proto::get_row(&mut r)).collect();
            for (k, row) in rows.into_iter().enumerate() {
                let v = self.make_row(row);
                self.rows.insert(f + k as u32, v);
            }
        }
    }

    fn focused_or_first_selected(&mut self) -> Option<String> {
        let i = unsafe { SendMessageW(self.list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) as i32 };
        if i < 0 {
            return None;
        }
        if !self.rows.contains_key(&(i as u32)) {
            self.fetch_sync(&[i as u32]);
        }
        self.rows.get(&(i as u32)).map(|r| r.full.clone())
    }

    fn open_selected(&mut self) {
        for p in self.selected_paths(32) {
            shell::open(self.hwnd, &p);
        }
    }

    fn reveal_selected(&mut self) {
        if let Some(p) = self.focused_or_first_selected() {
            shell::reveal(&p);
        }
    }

    fn copy_selected(&mut self, as_text: bool) {
        let paths = self.selected_paths(100_000);
        if paths.is_empty() {
            return;
        }
        if as_text {
            shell::copy_text(self.hwnd, &paths.join("\r\n"));
        } else {
            shell::copy_files(self.hwnd, &paths);
        }
    }

    fn select_all_rows(&self) {
        unsafe {
            let mut st: LVITEMW = std::mem::zeroed();
            st.state = LVIS_SELECTED;
            st.stateMask = LVIS_SELECTED;
            SendMessageW(self.list, LVM_SETITEMSTATE, usize::MAX, &st as *const _ as isize);
        }
    }

    fn focus_list(&self) {
        unsafe {
            SetFocus(self.list);
            if self.total > 0 && self.selected_count() == 0 {
                let mut st: LVITEMW = std::mem::zeroed();
                st.state = LVIS_SELECTED | LVIS_FOCUSED;
                st.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
                SendMessageW(self.list, LVM_SETITEMSTATE, 0, &st as *const _ as isize);
                SendMessageW(self.list, LVM_ENSUREVISIBLE, 0, 0);
            }
        }
    }

    fn focus_search(&self) {
        unsafe {
            SetFocus(self.edit);
            SendMessageW(self.edit, EM_SETSEL, 0, -1);
        }
    }

    fn install_service(&mut self, uninstall: bool) {
        let arg = if uninstall { "--uninstall" } else { "--install" };
        match install::elevate_and_wait(arg) {
            Some(0) => {
                if uninstall {
                    message("Nothing", "Uninstalled. The index service is removed; files are deleted at next reboot if in use.", false);
                    self.exit();
                } else {
                    let installed = install::install_dir().join("nothing.exe");
                    let here = std::env::current_exe().ok();
                    if here.as_deref() != Some(installed.as_path()) {
                        // Hand over to the installed copy (unelevated, like this one).
                        crate::release_single_instance();
                        let _ = std::process::Command::new(&installed).spawn();
                        self.exit();
                    } else {
                        self.ipc.send(Cmd::Status);
                    }
                }
            }
            Some(code) => message("Nothing", &format!("Setup failed (code {code})."), true),
            None => {} // UAC declined
        }
    }

    /// Keyboard shortcuts, handled before dispatch. Returns true if consumed.
    fn pre_translate(&mut self, msg: &MSG) -> bool {
        if msg.message != WM_KEYDOWN && msg.message != WM_SYSKEYDOWN {
            return false;
        }
        let vk = msg.wParam as u16;
        let (ctrl, shift) = unsafe { (GetKeyState(VK_CONTROL as i32) < 0, GetKeyState(VK_SHIFT as i32) < 0) };
        let alt = msg.message == WM_SYSKEYDOWN;
        let focus = unsafe { GetFocus() };
        let in_list = focus == self.list;
        let in_edit = focus == self.edit;
        match vk {
            VK_ESCAPE => {
                if !self.search_text().is_empty() {
                    unsafe { SetWindowTextW(self.edit, wide("").as_ptr()) };
                    self.focus_search();
                } else {
                    self.hide();
                }
                true
            }
            VK_RETURN if alt => {
                let p = self.selected_paths(64);
                shell::verb(self.hwnd, &p, "properties");
                true
            }
            VK_RETURN if ctrl => {
                self.reveal_selected();
                true
            }
            VK_RETURN if in_edit => {
                self.focus_list();
                true
            }
            VK_RETURN if in_list => {
                self.open_selected();
                true
            }
            VK_DOWN | VK_NEXT if in_edit => {
                self.focus_list();
                true
            }
            VK_UP if in_list => {
                // Arrow up from the first row goes back to the search box.
                let i = unsafe { SendMessageW(self.list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize) };
                if i <= 0 {
                    self.focus_search();
                    return true;
                }
                false
            }
            VK_TAB if !alt => {
                if in_list {
                    self.focus_search();
                } else {
                    self.focus_list();
                }
                true
            }
            VK_F3 => {
                self.focus_search();
                true
            }
            VK_F5 => {
                self.run_query(true);
                true
            }
            VK_DELETE if in_list => {
                let p = self.selected_paths(10_000);
                shell::verb(self.hwnd, &p, "delete");
                true
            }
            VK_BACK if ctrl && in_edit => {
                // Classic EDIT inserts a box glyph for Ctrl+Backspace; delete a word instead.
                let t: Vec<char> = self.search_text().chars().collect();
                let mut end = t.len();
                while end > 0 && t[end - 1] == ' ' {
                    end -= 1;
                }
                while end > 0 && t[end - 1] != ' ' && t[end - 1] != '\\' {
                    end -= 1;
                }
                let s: String = t[..end].iter().collect();
                unsafe {
                    SetWindowTextW(self.edit, wide(&s).as_ptr());
                    SendMessageW(self.edit, EM_SETSEL, s.encode_utf16().count(), s.encode_utf16().count() as isize);
                }
                true
            }
            _ if ctrl && !alt => match vk as u8 {
                b'F' | b'L' | b'E' => {
                    self.focus_search();
                    true
                }
                b'C' if in_list => {
                    self.copy_selected(shift);
                    true
                }
                b'A' => {
                    if in_list {
                        self.select_all_rows();
                    } else {
                        unsafe { SendMessageW(self.edit, EM_SETSEL, 0, -1) };
                    }
                    true
                }
                b'I' => {
                    self.settings.match_case = !self.settings.match_case;
                    self.update_menu_checks();
                    self.run_query(false);
                    true
                }
                b'U' => {
                    self.settings.match_path = !self.settings.match_path;
                    self.update_menu_checks();
                    self.run_query(false);
                    true
                }
                b'Q' | b'W' => {
                    self.hide();
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn on_command(&mut self, id: u16) {
        match id {
            ID_OPEN => self.open_selected(),
            ID_REVEAL => self.reveal_selected(),
            ID_EXIT => self.exit(),
            ID_COPY => self.copy_selected(false),
            ID_COPY_PATH => self.copy_selected(true),
            ID_SELECT_ALL => self.select_all_rows(),
            ID_CASE => {
                self.settings.match_case = !self.settings.match_case;
                self.update_menu_checks();
                self.run_query(false);
            }
            ID_PATH => {
                self.settings.match_path = !self.settings.match_path;
                self.update_menu_checks();
                self.run_query(false);
            }
            x if (ID_SORT_BASE..ID_SORT_BASE + 4).contains(&x) => {
                let col = (x - ID_SORT_BASE) as u8;
                if col != self.settings.sort {
                    self.set_sort(col);
                }
            }
            ID_INSTALL => self.install_service(false),
            ID_UNINSTALL => {
                let r = unsafe {
                    MessageBoxW(self.hwnd, wide("Remove the indexing service and its index?").as_ptr(), wide("Nothing").as_ptr(), MB_YESNO | MB_ICONQUESTION)
                };
                if r == IDYES {
                    self.install_service(true);
                }
            }
            ID_SYNTAX => message(
                "Search syntax",
                "foo bar\tboth words (AND)\n\
                 foo | bar\teither word (OR)\n\
                 !foo\t\texclude\n\
                 \"a b\"\t\texact phrase with spaces\n\
                 *.mkv  a?c\twildcards (match the whole name)\n\
                 users\\nihil\\\tany \\ in a term matches the full path\n\
                 ext:jpg;png\tby extension\n\
                 file:  folder:\tonly files / only folders\n\
                 case:  path:\tper-term match case / match full path\n\n\
                 Enter open · Ctrl+Enter open folder · Ctrl+C copy · Ctrl+Shift+C copy path\n\
                 Del delete · Alt+Enter properties · Esc clear/hide · Win+Alt+Space show",
                false,
            ),
            ID_ABOUT => message(
                "About Nothing",
                &format!(
                    "Nothing {}\nInstant file-name search for Windows.\n\nReads the NTFS Master File Table directly and follows the USN change journal.\n{}",
                    env!("CARGO_PKG_VERSION"),
                    if self.hotkey_ok { "Global hotkey: Win+Alt+Space" } else { "Global hotkey unavailable (already taken)." }
                ),
                false,
            ),
            _ => {}
        }
    }

    fn on_notify(&mut self, lp: LPARAM) -> Option<LRESULT> {
        let hdr = unsafe { &*(lp as *const NMHDR) };
        if hdr.hwndFrom != self.list {
            return None;
        }
        unsafe {
            match hdr.code {
                LVN_GETDISPINFOW => {
                    let di = &mut *(lp as *mut NMLVDISPINFOW);
                    let i = di.item.iItem as u32;
                    if !self.rows.contains_key(&i) {
                        self.request_page(i / PAGE);
                    }
                    let row = self.rows.get(&i).or_else(|| self.stale.get(&i));
                    if di.item.mask & LVIF_TEXT != 0 && !di.item.pszText.is_null() && di.item.cchTextMax > 0 {
                        let src: &[u16] = match (row, di.item.iSubItem) {
                            (Some(r), 0) => &r.name,
                            (Some(r), 1) => &r.path,
                            (Some(r), 2) => &r.size,
                            (Some(r), 3) => &r.date,
                            _ => &[0],
                        };
                        let n = src.iter().position(|&c| c == 0).unwrap_or(src.len()).min(di.item.cchTextMax as usize - 1);
                        std::ptr::copy_nonoverlapping(src.as_ptr(), di.item.pszText, n);
                        *di.item.pszText.add(n) = 0;
                    }
                    if di.item.mask & LVIF_IMAGE != 0 {
                        di.item.iImage = row.map(|r| r.icon).unwrap_or(I_IMAGENONE);
                    }
                    Some(0)
                }
                LVN_ODCACHEHINT => {
                    let h = &*(lp as *const NMLVCACHEHINT);
                    let (a, b) = (h.iFrom.max(0) as u32, h.iTo.max(0) as u32);
                    for p in a / PAGE..=b / PAGE {
                        if (p * PAGE..(p + 1) * PAGE).any(|i| i <= b && i >= a && !self.rows.contains_key(&i)) {
                            self.request_page(p);
                        }
                    }
                    Some(0)
                }
                LVN_ODFINDITEMW => Some(-1),
                NM_DBLCLK => {
                    let ia = &*(lp as *const NMITEMACTIVATE);
                    if ia.iItem >= 0 {
                        self.open_selected();
                    }
                    Some(0)
                }
                LVN_COLUMNCLICK => {
                    let nl = &*(lp as *const NMLISTVIEW);
                    self.set_sort(nl.iSubItem.clamp(0, 3) as u8);
                    Some(0)
                }
                LVN_BEGINDRAG => {
                    let p = self.selected_paths(10_000);
                    shell::drag(self.hwnd, &p);
                    Some(0)
                }
                LVN_ITEMCHANGED | LVN_ODSTATECHANGED => {
                    self.invalidate_status();
                    None
                }
                _ => None,
            }
        }
    }

    // ---- dark menu bar ----------------------------------------------------------------

    fn uah_draw_bar(&self, lp: LPARAM) {
        unsafe {
            let um = &*(lp as *const UahMenu);
            let mut mbi: MENUBARINFO = std::mem::zeroed();
            mbi.cbSize = std::mem::size_of::<MENUBARINFO>() as u32;
            GetMenuBarInfo(self.hwnd, OBJID_MENU, 0, &mut mbi);
            let mut wr: RECT = std::mem::zeroed();
            GetWindowRect(self.hwnd, &mut wr);
            let mut r = mbi.rcBar;
            OffsetRect(&mut r, -wr.left, -wr.top);
            r.top -= 1; // cover the 1px gap above the bar too
            FillRect(um.hdc, &r, self.theme.br_bg);
        }
    }

    fn uah_draw_item(&self, lp: LPARAM) {
        unsafe {
            let it = &*(lp as *const UahDrawMenuItem);
            let mut buf = [0u16; 128];
            let mut mii: MENUITEMINFOW = std::mem::zeroed();
            mii.cbSize = std::mem::size_of::<MENUITEMINFOW>() as u32;
            mii.fMask = MIIM_STRING;
            mii.dwTypeData = buf.as_mut_ptr();
            mii.cch = 127;
            GetMenuItemInfoW(it.um.hmenu, it.position as u32, 1, &mut mii);
            let st = it.dis.itemState;
            let hot = st & (ODS_HOTLIGHT | ODS_SELECTED) != 0;
            FillRect(it.um.hdc, &it.dis.rcItem, if hot { self.theme.br_hot } else { self.theme.br_bg });
            SetBkMode(it.um.hdc, TRANSPARENT as i32);
            SetTextColor(it.um.hdc, if st & (ODS_GRAYED | ODS_DISABLED) != 0 { self.theme.dim } else { self.theme.fg });
            let mut r = it.dis.rcItem;
            let flags = DT_CENTER | DT_SINGLELINE | DT_VCENTER | if st & ODS_NOACCEL != 0 { DT_HIDEPREFIX } else { 0 };
            DrawTextW(it.um.hdc, buf.as_mut_ptr(), -1, &mut r, flags);
        }
    }

    /// The non-client area paints a light 1px line under the menu bar; paint over it.
    fn uah_fix_line(&self) {
        unsafe {
            let mut cr: RECT = std::mem::zeroed();
            GetClientRect(self.hwnd, &mut cr);
            let mut pts = [POINT { x: cr.left, y: cr.top }, POINT { x: cr.right, y: cr.bottom }];
            MapWindowPoints(self.hwnd, null_mut(), pts.as_mut_ptr(), 2);
            let mut wr: RECT = std::mem::zeroed();
            GetWindowRect(self.hwnd, &mut wr);
            let line = RECT { left: pts[0].x - wr.left, right: pts[1].x - wr.left, top: pts[0].y - wr.top - 1, bottom: pts[0].y - wr.top };
            let dc = GetWindowDC(self.hwnd);
            FillRect(dc, &line, self.theme.br_bg);
            ReleaseDC(self.hwnd, dc);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // While a shell context menu is open, its owner-drawn submenus need these messages.
    if matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
        if let Some(r) = shell::forward_menu_msg(msg, wp, lp) {
            return r;
        }
    }
    let Some(a) = app() else { return unsafe { DefWindowProcW(hwnd, msg, wp, lp) } };
    unsafe {
        match msg {
            WM_SIZE => {
                a.layout();
                0
            }
            WM_PAINT => {
                a.paint();
                0
            }
            WM_ERASEBKGND => 1,
            WM_CTLCOLOREDIT => {
                let hdc = wp as HDC;
                SetTextColor(hdc, a.theme.fg);
                SetBkColor(hdc, a.theme.field);
                a.theme.br_field as LRESULT
            }
            WM_COMMAND => {
                let code = (wp >> 16) as u32;
                if lp as HWND == a.edit {
                    if code == EN_CHANGE {
                        a.run_query(false);
                    }
                    return 0;
                }
                a.on_command(loword(wp) as u16);
                0
            }
            WM_NOTIFY => a.on_notify(lp).unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp)),
            WM_CONTEXTMENU if wp as HWND == a.list => {
                let mut pt = POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 };
                if lp == -1 {
                    // Keyboard (Shift+F10 / menu key): anchor at the focused row.
                    let i = SendMessageW(a.list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize);
                    let mut r = RECT { left: LVIR_LABEL as i32, top: 0, right: 0, bottom: 0 };
                    SendMessageW(a.list, LVM_GETITEMRECT, i.max(0) as usize, &mut r as *mut _ as isize);
                    pt = POINT { x: r.left, y: r.bottom };
                    ClientToScreen(a.list, &mut pt);
                }
                let paths = a.selected_paths(10_000);
                if !paths.is_empty() {
                    shell::context_menu(hwnd, &paths, pt);
                }
                0
            }
            WM_TIMER if wp == TIMER_STATUS => {
                a.ipc.send(Cmd::Status);
                0
            }
            WM_APP_IPC => {
                let ev = Box::from_raw(lp as *mut Event);
                a.on_ipc(*ev);
                0
            }
            WM_APP_ICON => {
                let b = Box::from_raw(lp as *mut (String, i32));
                let (path, idx) = *b;
                for (i, r) in a.rows.iter_mut() {
                    if r.full == path && idx >= 0 {
                        r.icon = idx;
                        SendMessageW(a.list, LVM_REDRAWITEMS, *i as usize, *i as isize);
                    }
                }
                a.icons.resolved(path, idx);
                0
            }
            WM_APP_TRAY => {
                match (lp & 0xFFFF) as u32 {
                    WM_LBUTTONUP => a.toggle(),
                    WM_RBUTTONUP | WM_CONTEXTMENU => a.tray_menu(),
                    _ => {}
                }
                0
            }
            WM_APP_SHOW => {
                a.show();
                0
            }
            WM_HOTKEY => {
                a.toggle();
                0
            }
            WM_ACTIVATE => {
                if loword(wp) == WA_INACTIVE {
                    let f = GetFocus();
                    if f == a.edit || f == a.list {
                        a.last_focus = f;
                    }
                } else {
                    SetFocus(a.last_focus);
                }
                0
            }
            WM_CLOSE => {
                if a.exiting {
                    DefWindowProcW(hwnd, msg, wp, lp)
                } else {
                    a.hide(); // keep running in the tray: the next summon is instant
                    0
                }
            }
            WM_QUERYENDSESSION => 1,
            WM_ENDSESSION => {
                if wp != 0 {
                    a.save_settings();
                    a.remove_tray();
                }
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            WM_GETMINMAXINFO => {
                let mm = &mut *(lp as *mut MINMAXINFO);
                mm.ptMinTrackSize = POINT { x: scale(420, a.dpi), y: scale(240, a.dpi) };
                0
            }
            WM_DPICHANGED => {
                let r = &*(lp as *const RECT);
                a.apply_dpi(loword(wp));
                SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                0
            }
            WM_SETTINGCHANGE => {
                if lp != 0 && from_wide(std::slice::from_raw_parts(lp as *const u16, 64)).starts_with("ImmersiveColorSet") {
                    a.apply_theme();
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_UAHDRAWMENU if a.theme.dark => {
                a.uah_draw_bar(lp);
                1
            }
            WM_UAHDRAWMENUITEM if a.theme.dark => {
                a.uah_draw_item(lp);
                1
            }
            WM_NCPAINT | WM_NCACTIVATE if a.theme.dark => {
                let r = DefWindowProcW(hwnd, msg, wp, lp);
                a.uah_fix_line();
                r
            }
            m if m == a.taskbar_created && m != 0 => {
                a.add_tray(); // Explorer restarted
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// ListView subclass: header text colour in dark mode, and typing-to-search.
unsafe extern "system" fn list_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    unsafe {
        if let Some(a) = app() {
            if msg == WM_NOTIFY && a.theme.dark {
                let hdr = &*(lp as *const NMHDR);
                if hdr.hwndFrom == a.header && hdr.code == NM_CUSTOMDRAW {
                    let cd = &*(lp as *const NMCUSTOMDRAW);
                    if cd.dwDrawStage == CDDS_PREPAINT {
                        return CDRF_NOTIFYITEMDRAW as LRESULT;
                    }
                    if cd.dwDrawStage == CDDS_ITEMPREPAINT {
                        SetTextColor(cd.hdc, a.theme.fg);
                        return CDRF_DODEFAULT as LRESULT;
                    }
                }
            }
            if msg == WM_CHAR {
                let c = wp as u32;
                // Printable characters (and Backspace) typed into the list go to the search box.
                if (c >= 0x20 && c != 0x7F && GetKeyState(VK_CONTROL as i32) >= 0) || c == 0x08 {
                    SetFocus(a.edit);
                    SendMessageW(a.edit, EM_SETSEL, usize::MAX, -1);
                    SendMessageW(a.edit, WM_CHAR, wp, lp);
                    return 0;
                }
            }
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

/// Edit subclass: swallow the beep for keys we handle (Enter/Esc/Ctrl+Backspace chars).
unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    unsafe {
        if msg == WM_CHAR && matches!(wp as u32, 0x0D | 0x1B | 0x7F | 0x0A | 0x01) {
            return 0;
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}
