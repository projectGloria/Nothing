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
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi, SystemParametersInfoForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{
    DefSubclassProc, SetWindowSubclass, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, SHIL_EXTRALARGE, SHIL_JUMBO,
    SHIL_LARGE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::icons::Icons;
use crate::ipc::{Cmd, Event, Ipc};
use crate::settings::Settings;
use crate::thumbs::{Done, Thumbs};
use crate::util::{date, from_wide, human_bytes, join_path, loword, rgb, size_kb, thousands, wide};
use crate::{dark, fit, icons, install, shell};

pub const CLASS: &str = "NothingMainWnd";
pub const WM_APP_SHOW: u32 = WM_APP + 4;
/// COPYDATASTRUCT::dwData tag for "search in this folder" (UTF-16 path, no NUL).
pub const COPYDATA_FOLDER: usize = 0x4E46; // "NF"
const WM_APP_IPC: u32 = WM_APP + 1;
const WM_APP_ICON: u32 = WM_APP + 2;
const WM_APP_TRAY: u32 = WM_APP + 3;
const WM_APP_THUMB: u32 = WM_APP + 5;

const PAGE: u32 = 100;
const TIMER_STATUS: usize = 1;
const HOTKEY_ID: i32 = 1;

// Menu command ids.
const ID_OPEN: u16 = 1001;
const ID_REVEAL: u16 = 1002;
const ID_EXIT: u16 = 1003;
const ID_CUT: u16 = 1009;
const ID_COPY: u16 = 1010;
const ID_COPY_PATH: u16 = 1011;
const ID_SELECT_ALL: u16 = 1012;
const ID_CASE: u16 = 1020;
const ID_PATH: u16 = 1021;
const ID_SORT_BASE: u16 = 1030; // +0..3
const ID_SORT_ASC: u16 = 1034;
const ID_SORT_DESC: u16 = 1035;
const ID_REFRESH: u16 = 1036;
const ID_INSTALL: u16 = 1040;
const ID_UNINSTALL: u16 = 1041;
const ID_SYNTAX: u16 = 1050;
const ID_ABOUT: u16 = 1051;
const ID_TRAY_SHOW: u16 = 1060;
const ID_VIEW_BASE: u16 = 1070; // + Settings::view (0 details .. 3 extra large)
const ID_THEME_BASE: u16 = 1080; // + Settings::theme

// Above shell::SHELL_LAST, so they can share the shell's item context menu.
const ID_CTX_REVEAL: u32 = 0x8001;
const ID_CTX_COPY_PATH: u32 = 0x8002;

const VIEW_NAMES: [&str; 4] = ["&Details", "&Medium Icons", "&Large Icons", "E&xtra Large Icons"];
const SORT_NAMES: [&str; 4] = ["&Name", "&Path", "&Size", "Date &Modified"];

/// One cached result row, pre-formatted for painting.
struct RowView {
    name: Vec<u16>,
    path: Vec<u16>,
    size: Vec<u16>,
    date: Vec<u16>,
    icon: i32,
    bytes: u64,
    mtime: u64,
    full: String,
    dir: bool,
}

// Settings::theme values.
const THEME_SYSTEM: u8 = 0;
const THEME_LIGHT: u8 = 1;
const THEME_DARK: u8 = 2;
const THEME_BLACK: u8 = 3;

// Settings::view values.
const VIEW_DETAILS: u8 = 0;
/// Large and up get real thumbnails; at 32px they'd cost far more than they show.
const VIEW_THUMBS_FROM: u8 = 2;

struct Theme {
    dark: bool,
    fg: u32,
    dim: u32,
    /// Orange accent: menu bar, column titles, item count and selection details.
    accent: u32,
    field: u32,
    br_bg: HBRUSH,
    br_field: HBRUSH,
    br_hot: HBRUSH,
    /// Column separators and the header's bottom edge (dark header is painted by us).
    br_line: HBRUSH,
    /// Selected row in Details view: a dim orange fill with an accent-orange frame.
    sel: u32,
    br_sel: HBRUSH,
    br_accent: HBRUSH,
    /// Row under the mouse in Details view: half the selection's orange, no frame.
    hover: u32,
    br_hover: HBRUSH,
    /// Caption close button under the mouse: Windows' own red, in every theme.
    br_close: HBRUSH,
}

impl Theme {
    /// `kind` is a resolved THEME_LIGHT / THEME_DARK / THEME_BLACK, never THEME_SYSTEM.
    fn new(kind: u8) -> Theme {
        // (chrome bg, text, dim text, list/field bg, hover, lines)
        let (bg, fg, dim, field, hot, line) = match kind {
            // Near-black like Explorer's dark list (#191919) but a step darker everywhere;
            // the old 32/16 grey read as "dark grey", not dark.
            THEME_DARK => (rgb(18, 18, 18), rgb(228, 228, 228), rgb(150, 150, 150), rgb(10, 10, 10), rgb(48, 48, 48), rgb(40, 40, 40)),
            // True black for OLED: pixels off, chrome separated only by hairlines.
            THEME_BLACK => (rgb(0, 0, 0), rgb(220, 220, 220), rgb(140, 140, 140), rgb(0, 0, 0), rgb(40, 40, 40), rgb(34, 34, 34)),
            _ => (rgb(243, 243, 243), rgb(20, 20, 20), rgb(96, 96, 96), rgb(255, 255, 255), rgb(222, 222, 222), rgb(220, 220, 220)),
        };
        // Bright orange reads well on near-black; on white it needs to be a few steps
        // darker or it fails contrast (#FF8C00 on white is ~2.3:1, #C25E00 is ~4.6:1).
        let accent = if kind == THEME_LIGHT { rgb(194, 94, 0) } else { rgb(255, 150, 40) };
        // Selection fill = accent mixed into the list background: dark enough on dark
        // themes that white row text stays readable (~25% orange), a light peach on white.
        let sel = mix(accent, field, if kind == THEME_LIGHT { 18 } else { 25 });
        // Hover sits between plain and selected (25 / 18): obvious, but the selection
        // still wins by fill and by its frame.
        let hover = mix(accent, field, if kind == THEME_LIGHT { 12 } else { 18 });
        unsafe {
            Theme {
                dark: kind != THEME_LIGHT,
                fg,
                dim,
                accent,
                field,
                br_bg: CreateSolidBrush(bg),
                br_field: CreateSolidBrush(field),
                br_hot: CreateSolidBrush(hot),
                br_line: CreateSolidBrush(line),
                sel,
                br_sel: CreateSolidBrush(sel),
                br_accent: CreateSolidBrush(accent),
                hover,
                br_hover: CreateSolidBrush(hover),
                br_close: CreateSolidBrush(rgb(196, 43, 28)),
            }
        }
    }
}

/// `pct`% of colour `a` over colour `b`, per channel (COLORREF 0x00BBGGRR).
fn mix(a: u32, b: u32, pct: u32) -> u32 {
    let ch = |c: u32, sh: u32| (c >> sh) & 0xFF;
    let m = |sh: u32| (ch(a, sh) * pct + ch(b, sh) * (100 - pct)) / 100;
    m(0) | m(8) << 8 | m(16) << 16
}

impl Drop for Theme {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.br_bg as _);
            DeleteObject(self.br_field as _);
            DeleteObject(self.br_hot as _);
            DeleteObject(self.br_line as _);
            DeleteObject(self.br_sel as _);
            DeleteObject(self.br_accent as _);
            DeleteObject(self.br_hover as _);
            DeleteObject(self.br_close as _);
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
    thumbs: Thumbs,
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
    hotkey_ok: bool,
    offline_note: String,
    /// Status-bar details for a single selection. Built on selection change, not in
    /// paint: the type name costs a shell call.
    detail: Option<Detail>,
    /// Caption button under the mouse / held down (HT* codes, 0 = none).
    cap_hot: u32,
    cap_down: u32,
    active: bool,
    small_icon: HICON,
    /// Segoe MDL2 Assets at caption-glyph size, per DPI.
    glyphs: HFONT,
    /// Details row under the mouse (-1 = none), tracked in `list_proc`.
    hover_row: i32,
    /// Preserved selected file paths across in-place refreshes.
    saved_selection: HashSet<String>,
    saved_focus: Option<String>,
    restoring_selection: bool,
    cut_paths: HashSet<String>,
    clip_msg: Option<(String, std::time::Instant)>,
}

static TASKBAR_CREATED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

thread_local! {
    static APP: std::cell::Cell<*mut App> = const { std::cell::Cell::new(null_mut()) };
    static APP_BORROWED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static HWND_EDIT: std::cell::Cell<HWND> = const { std::cell::Cell::new(null_mut()) };
    static HWND_LIST: std::cell::Cell<HWND> = const { std::cell::Cell::new(null_mut()) };
    static PENDING_QUERY: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static PENDING_LAYOUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CACHED_THEME: std::cell::Cell<(u32, u32, HBRUSH)> = const { std::cell::Cell::new((0, 0, null_mut())) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    let p = APP.with(|a| a.get());
    if p.is_null() {
        return None;
    }
    if APP_BORROWED.with(|b| b.replace(true)) {
        return None;
    }
    struct BorrowGuard;
    impl Drop for BorrowGuard {
        fn drop(&mut self) {
            APP_BORROWED.with(|b| b.set(false));
            let query = PENDING_QUERY.with(|q| q.take());
            let layout = PENDING_LAYOUT.with(|l| l.replace(false));
            if query.is_some() || layout {
                with_app(|a| {
                    if layout {
                        a.layout();
                    }
                    if let Some(keep) = query {
                        a.run_query(keep);
                    }
                });
            }
        }
    }
    let _guard = BorrowGuard;
    Some(f(unsafe { &mut *p }))
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

fn add(m: HMENU, id: u16, text: &str) {
    unsafe { AppendMenuW(m, MF_STRING, id as usize, wide(text).as_ptr()) };
}

fn sep(m: HMENU) {
    unsafe { AppendMenuW(m, MF_SEPARATOR, 0, null()) };
}

fn sub(m: HMENU, popup: HMENU, text: &str) {
    unsafe { AppendMenuW(m, MF_POPUP, popup as usize, wide(text).as_ptr()) };
}

/// View ▸ submenu, shared by the menu bar and the list's background context menu.
fn view_menu() -> HMENU {
    let m = unsafe { CreatePopupMenu() };
    // Biggest first, like Explorer/Everything.
    for v in (0..4u16).rev() {
        add(m, ID_VIEW_BASE + v, VIEW_NAMES[v as usize]);
    }
    m
}

/// Sort By ▸ submenu, also shared.
fn sort_menu() -> HMENU {
    let m = unsafe { CreatePopupMenu() };
    for (i, n) in SORT_NAMES.iter().enumerate() {
        add(m, ID_SORT_BASE + i as u16, n);
    }
    sep(m);
    add(m, ID_SORT_ASC, "&Ascending");
    add(m, ID_SORT_DESC, "&Descending");
    m
}

/// Radio bullet on/off for one item, found by command id anywhere under `m` (submenus too).
/// Per-item instead of CheckMenuRadioItem because the View group is listed in reverse id order.
fn radio(m: HMENU, id: u16, on: bool) {
    unsafe {
        let mut mii: MENUITEMINFOW = std::mem::zeroed();
        mii.cbSize = std::mem::size_of::<MENUITEMINFOW>() as u32;
        mii.fMask = MIIM_FTYPE | MIIM_STATE;
        mii.fType = MFT_RADIOCHECK;
        mii.fState = if on { MFS_CHECKED } else { MFS_UNCHECKED };
        SetMenuItemInfoW(m, id as u32, 0, &mii);
    }
}

/// Sort / direction / view / theme bullets: the menu bar and the context menu share them.
fn check_radios(m: HMENU, s: &Settings) {
    for i in 0..4u8 {
        radio(m, ID_SORT_BASE + i as u16, s.sort == i);
        radio(m, ID_VIEW_BASE + i as u16, s.view == i);
        radio(m, ID_THEME_BASE + i as u16, s.theme == i);
    }
    radio(m, ID_SORT_ASC, !s.desc);
    radio(m, ID_SORT_DESC, s.desc);
}

/// The caption's menu button pops this up. One menu instead of a bar: the File items at
/// the top level, the rest of what the old bar had as submenus, so nothing lost its only
/// way in (Install / Repair and the theme picker live nowhere else).
fn build_menu() -> HMENU {
    unsafe {
        let bar = CreatePopupMenu();
        add(bar, ID_OPEN, "&Open\tEnter");
        add(bar, ID_REVEAL, "Open &Path\tCtrl+Enter");
        sep(bar);
        add(bar, ID_CUT, "Cu&t\tCtrl+X");
        add(bar, ID_COPY, "&Copy\tCtrl+C");
        add(bar, ID_COPY_PATH, "Copy &Full Path\tCtrl+Shift+C");
        add(bar, ID_SELECT_ALL, "Select &All\tCtrl+A");
        sep(bar);
        let search = CreatePopupMenu();
        add(search, ID_CASE, "Match &Case\tCtrl+I");
        add(search, ID_PATH, "Match &Path\tCtrl+U");
        sep(search);
        sub(search, sort_menu(), "&Sort By");
        let view = view_menu();
        sep(view);
        add(view, ID_REFRESH, "&Refresh\tF5");
        let theme = CreatePopupMenu();
        for (i, n) in ["Use &System Setting", "&Light", "&Dark", "&Black"].iter().enumerate() {
            add(theme, ID_THEME_BASE + i as u16, n);
        }
        let tools = CreatePopupMenu();
        sub(tools, theme, "&Theme");
        sep(tools);
        add(tools, ID_INSTALL, "&Install / Repair Indexing Service…");
        add(tools, ID_UNINSTALL, "&Uninstall…");
        let help = CreatePopupMenu();
        add(help, ID_SYNTAX, "Search &Syntax");
        add(help, ID_ABOUT, "&About Nothing");
        for (m, t) in [(view, "&View"), (search, "&Search"), (tools, "&Tools"), (help, "&Help")] {
            sub(bar, m, t);
        }
        sep(bar);
        add(bar, ID_EXIT, "E&xit");
        bar
    }
}

/// Create the window and run the message loop. Returns the exit code.
/// `folder`: from Explorer's "Nothing" verb (`--folder`), search inside it once shown.
pub fn run(start_hidden: bool, folder: Option<String>) -> i32 {
    unsafe {
        TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), std::sync::atomic::Ordering::Relaxed);
        let hinst = GetModuleHandleW(null());
        let icc = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES };
        InitCommonControlsEx(&icc);

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
            null_mut(), // no menu bar: the caption's menu button pops up `menu`
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

        // No WS_EX_CLIENTEDGE: its themed frame is a bright box in dark mode. The parent
        // paints the field background and hairline instead (see `paint`).
        let edit = CreateWindowExW(
            0,
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
        // No LVS_EX_LABELTIP: no white "full text" tooltip on hover (the status bar shows
        // the selection's details). The list also keeps a tooltip control of its own for
        // truncated labels, so that one is switched off too.
        let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER;
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, ex as usize, ex as isize);
        let tips = SendMessageW(list, LVM_GETTOOLTIPS, 0, 0) as HWND;
        if !tips.is_null() {
            SendMessageW(tips, TTM_ACTIVATE, 0, 0);
        }
        SendMessageW(list, LVM_SETCALLBACKMASK, 0, 0);
        let header = SendMessageW(list, LVM_GETHEADER, 0, 0) as HWND;
        SetWindowSubclass(list, Some(list_proc), 1, 0);
        SetWindowSubclass(edit, Some(edit_proc), 2, 0);
        SetWindowSubclass(header, Some(header_proc), 3, 0);
        HWND_EDIT.with(|h| h.set(edit));
        HWND_LIST.with(|h| h.set(list));

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
            theme: Theme::new(THEME_LIGHT), // placeholder: apply_theme resolves the real one
            ipc: Ipc::spawn(hwnd, WM_APP_IPC),
            icons,
            thumbs: Thumbs::new(hwnd, WM_APP_THUMB),
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
            hotkey_ok: false,
            offline_note: String::new(),
            detail: None,
            cap_hot: 0,
            cap_down: 0,
            active: true,
            small_icon: null_mut(),
            glyphs: null_mut(),
            hover_row: -1,
            saved_selection: HashSet::new(),
            saved_focus: None,
            restoring_selection: false,
            cut_paths: HashSet::new(),
            clip_msg: None,
        });
        APP.with(|a| a.set(Box::into_raw(boxed)));
        with_app(|a| {
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
        });
        with_app(|a| a.apply_theme());
        with_app(|a| a.apply_view());
        with_app(|a| a.update_menu_checks());
        with_app(|a| a.update_sort_arrow());
        with_app(|a| a.add_tray());
        with_app(|a| a.register_hotkey());
        with_app(|a| a.layout());
        with_app(|a| a.run_query(false));
        if !start_hidden {
            with_app(|a| a.show());
            let max = with_app(|a| a.settings.maximized).unwrap_or(false);
            if max {
                ShowWindow(hwnd, SW_MAXIMIZE);
            }
        }
        if let Some(f) = folder {
            with_app(|a| a.open_folder(&f));
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            let mut handled = false;
            let mut main_hwnd = null_mut();
            with_app(|a| {
                main_hwnd = a.hwnd;
                if a.pre_translate(&msg) {
                    handled = true;
                }
            });
            if handled {
                continue;
            }
            if msg.message == WM_KEYDOWN && IsDialogMessageW(main_hwnd, &msg) != 0 {
                continue;
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
        self.thumbs.dirty = true;
        self.thumbs.invalidate_failed();
        if keep {
            let mut saved_sel = HashSet::new();
            let mut saved_foc = None;
            let mut i = -1i32;
            loop {
                i = unsafe { SendMessageW(self.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize) as i32 };
                if i < 0 {
                    break;
                }
                if let Some(r) = self.rows.get(&(i as u32)).or_else(|| self.stale.get(&(i as u32))) {
                    saved_sel.insert(r.full.clone());
                }
            }
            let foc = unsafe { SendMessageW(self.list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize) as i32 };
            if foc >= 0 {
                if let Some(r) = self.rows.get(&(foc as u32)).or_else(|| self.stale.get(&(foc as u32))) {
                    saved_foc = Some(r.full.clone());
                }
            }
            self.saved_selection = saved_sel;
            self.saved_focus = saved_foc;
            let old = std::mem::take(&mut self.rows);
            self.stale = old;
        } else {
            self.saved_selection.clear();
            self.saved_focus = None;
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
            mtime: r.mtime,
            full,
            dir: is_dir,
        }
    }

    fn store_rows(&mut self, first: u32, rows: Vec<Row>) {
        let n = rows.len() as u32;
        self.restoring_selection = true;
        for (i, r) in rows.into_iter().enumerate() {
            let idx = first + i as u32;
            let v = self.make_row(r);
            self.stale.remove(&idx);
            if !self.saved_selection.is_empty() && self.saved_selection.contains(&v.full) {
                let is_foc = self.saved_focus.as_deref() == Some(&v.full);
                let mut st: LVITEMW = unsafe { std::mem::zeroed() };
                st.state = LVIS_SELECTED | if is_foc { LVIS_FOCUSED } else { 0 };
                st.stateMask = LVIS_SELECTED | if is_foc { LVIS_FOCUSED } else { 0 };
                unsafe { SendMessageW(self.list, LVM_SETITEMSTATE, idx as usize, &st as *const _ as isize) };
            }
            self.rows.insert(idx, v);
        }
        self.restoring_selection = false;
        // Newly known paths may fall in the thumbnail window.
        self.thumbs.dirty = true;
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
        // The selected row may just have arrived, or been replaced by a refresh.
        self.update_detail();
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
                        let mut st: LVITEMW = std::mem::zeroed();
                        st.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
                        SendMessageW(self.list, LVM_SETITEMSTATE, usize::MAX, &st as *const _ as isize);
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
            // A single-line EDIT draws its text at the top and ignores EM_SETRECT, so it gets
            // exactly one line of height, centered inside the painted field.
            let f = self.field_rect();
            let inset = scale(6, self.dpi);
            let line_h = self.edit_line_height().min(edit_h - 2);
            let y = f.top + (edit_h - line_h) / 2;
            MoveWindow(self.edit, f.left + inset, y, (f.right - f.left - 2 * inset).max(0), line_h, 1);
            let top = self.caption_h() + edit_h + 2 * pad;
            MoveWindow(self.list, 0, top, w, (h - top - status_h).max(0), 1);
            self.invalidate_status();
        }
    }

    /// The search field's box in client coordinates (painted by us; the EDIT sits inside).
    fn field_rect(&self) -> RECT {
        let mut rc: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut rc) };
        let (pad, edit_h, _) = self.metrics();
        let top = self.caption_h() + pad;
        RECT { left: pad, top, right: rc.right - pad, bottom: top + edit_h }
    }

    /// Height of one line in the current font, i.e. what the EDIT actually needs.
    fn edit_line_height(&self) -> i32 {
        unsafe {
            let hdc = GetDC(self.edit);
            let old = SelectObject(hdc, self.font as _);
            let mut tm: TEXTMETRICW = std::mem::zeroed();
            GetTextMetricsW(hdc, &mut tm);
            SelectObject(hdc, old);
            ReleaseDC(self.edit, hdc);
            tm.tmHeight
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

    /// Rebuild the single-selection details if the selection moved to another row (or the
    /// row behind an index changed after a refresh). Cheap when nothing changed.
    fn update_detail(&mut self) {
        let i = if self.selected_count() == 1 { unsafe { SendMessageW(self.list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) as i32 } } else { -1 };
        let Some(r) = (i >= 0).then(|| self.rows.get(&(i as u32))).flatten() else {
            // Nothing selected, several selected, or the row isn't fetched yet (store_rows
            // calls back here once it is).
            if self.detail.take().is_some() {
                self.invalidate_status();
            }
            return;
        };
        if self.detail.as_ref().is_some_and(|d| d.idx == i as u32 && d.full == r.full && d.bytes == r.bytes && d.mtime == r.mtime) {
            return;
        }
        self.detail = Some(describe(i as u32, r));
        self.invalidate_status();
    }

    /// (count, rest of the left side, plain right side, single-selection details). The
    /// count is split out so paint can colour it on its own. Details and the plain right
    /// side are exclusive: details only when nothing more urgent wants the space.
    fn status_texts(&self) -> (String, String, String, Option<&Detail>) {
        let count = format!("{} {}", thousands(self.total as u64), if self.total == 1 { "item" } else { "items" });
        let mut left = String::new();
        let sel = self.selected_count();
        let mut detail = None;
        let mut right = String::new();
        if sel == 1 {
            left.push_str("   ·   1 selected");
            detail = self.detail.as_ref();
        } else if sel > 0 {
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
        // Service problems trump selection details.
        if !self.connected {
            detail = None;
            right = self.offline_note.clone();
        } else if let Some(s) = &self.status {
            if s.state != state::READY {
                detail = None;
                right = s.message.clone();
            }
        }
        (count, left, right, detail)
    }

    fn paint(&self) {
        unsafe {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(self.hwnd, &mut rc);
            let (pad, edit_h, _) = self.metrics();
            // Caption + strip around the search box: one continuous band.
            let top = RECT { left: 0, top: 0, right: rc.right, bottom: self.caption_h() + edit_h + 2 * pad };
            FillRect(hdc, &top, self.theme.br_bg);
            self.paint_caption(hdc);
            // Search field: field-colored box with the same hairline as the column separators.
            // WS_CLIPCHILDREN keeps this from painting over the EDIT itself.
            let f = self.field_rect();
            FillRect(hdc, &f, self.theme.br_field);
            FrameRect(hdc, &f, self.theme.br_line);
            // Status bar.
            let sr = self.status_rect();
            FillRect(hdc, &sr, self.theme.br_bg);
            let old = SelectObject(hdc, self.font as _);
            SetBkMode(hdc, TRANSPARENT as i32);
            let (count, left, right, detail) = self.status_texts();
            let r = RECT { left: sr.left + scale(8, self.dpi), right: sr.right - scale(10, self.dpi), ..sr };
            // Draws one run at x, returns where the next one starts. DT_CALCRECT only
            // measures; the second call paints, vertically centred in the bar.
            let run = |x: i32, text: &str, color: u32| {
                let mut w = wide(text);
                let mut m = RECT { left: x, ..r };
                DrawTextW(hdc, w.as_mut_ptr(), -1, &mut m, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
                let mut d = RECT { left: x, ..r };
                SetTextColor(hdc, color);
                DrawTextW(hdc, w.as_mut_ptr(), -1, &mut d, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
                m.right
            };
            // Left: orange count, then the selection info in fg.
            let x = run(r.left, &count, self.theme.accent);
            let mut x = run(x, &left, self.theme.fg);
            if let Some((msg, time)) = &self.clip_msg {
                if time.elapsed() < std::time::Duration::from_secs(5) {
                    x = run(x, &format!("   ·   {}", msg), self.theme.accent);
                }
            }
            // Right side gets only what's left, so it can't overprint the count.
            let avail = RECT { left: x + scale(24, self.dpi), ..r };
            if let Some(d) = detail {
                // name · folder · type · size: fields orange, dots in fg. Each field is
                // trimmed on its own (fit.rs): C:\Users\Nihil\Downloads\Chernobyl.. instead
                // of one ellipsis eating whatever happens to be at the end.
                const SEP: &str = "   ·   ";
                let measure = |s: &str| text_width(hdc, s);
                let (head, tail) = fit::fields(&d.name, &d.folder, &d.kind, &d.size, avail.right - avail.left, SEP, &measure);
                let width = |v: &[String]| v.iter().map(|p| measure(p)).sum::<i32>() + measure(SEP) * (v.len() as i32 - 1).max(0);
                let draw = |mut x: i32, v: &[String]| {
                    for (k, p) in v.iter().enumerate() {
                        if k > 0 {
                            x = run(x, SEP, self.theme.fg);
                        }
                        x = run(x, p, self.theme.accent);
                    }
                };
                // Type and size stay right-aligned; name and folder are centred on the
                // whole bar, slid sideways only as far as needed to clear the count on
                // the left and keep one separator's gap before the type. fields() budgeted
                // that separator, so both clamps can hold unless nothing fit at all.
                let tail_x = avail.right - width(&tail);
                let hw = width(&head);
                let hi = if tail.is_empty() { avail.right } else { tail_x - measure(SEP) } - hw;
                let x = ((sr.left + sr.right - hw) / 2).min(hi).max(avail.left);
                draw(x, &head);
                if x <= hi {
                    draw(tail_x, &tail);
                }
            } else {
                SetTextColor(hdc, self.theme.dim);
                let mut rw = wide(&right);
                let mut rr = avail;
                DrawTextW(hdc, rw.as_mut_ptr(), -1, &mut rr, DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
            }
            SelectObject(hdc, old);
            EndPaint(self.hwnd, &ps);
        }
    }

    fn apply_dpi(&mut self, dpi: u32) {
        self.dpi = dpi;
        let old = self.font;
        self.font = make_font(dpi);
        unsafe {
            let (g, ic) = (self.glyphs, self.small_icon);
            self.glyphs = CreateFontW(-scale(10, dpi), 0, 0, 0, FW_NORMAL as i32, 0, 0, 0, DEFAULT_CHARSET as _, OUT_DEFAULT_PRECIS as _, CLIP_DEFAULT_PRECIS as _, CLEARTYPE_QUALITY as _, DEFAULT_PITCH as _, wide("Segoe MDL2 Assets").as_ptr());
            // Loaded at the exact small-icon size for this DPI: LR_SHARED would hand back
            // whatever size was cached first.
            let sz = GetSystemMetricsForDpi(SM_CXSMICON, dpi);
            self.small_icon = LoadImageW(GetModuleHandleW(null()), 1 as _, IMAGE_ICON, sz, sz, 0) as HICON;
            if !g.is_null() {
                DeleteObject(g as _);
            }
            if !ic.is_null() {
                DestroyIcon(ic);
            }
            SendMessageW(self.edit, WM_SETFONT, self.font as usize, 1);
            SendMessageW(self.list, WM_SETFONT, self.font as usize, 1);
            if !old.is_null() {
                DeleteObject(old as _);
            }
        }
        self.layout();
    }

    fn apply_theme(&mut self) {
        let choice = self.settings.theme;
        let kind = match choice {
            THEME_SYSTEM if dark::system_prefers_dark() => THEME_DARK,
            THEME_SYSTEM => THEME_LIGHT,
            k => k,
        };
        // Assigning drops the old Theme, which frees its brushes (see Drop).
        self.theme = Theme::new(kind);
        CACHED_THEME.with(|c| c.set((self.theme.fg, self.theme.field, self.theme.br_field)));
        let dark = self.theme.dark;
        // Popup menus, the shell context menu and scrollbars are themed process-wide,
        // not per window: an explicit choice has to force uxtheme's mode, or a "Dark"
        // pick on a light system would still get white menus.
        dark::set_app_mode(if choice == THEME_SYSTEM { None } else { Some(dark) });
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
            // SWP_FRAMECHANGED re-runs WM_NCCALCSIZE: on the first call this is what takes
            // the native title bar away (the one inside CreateWindowEx came before the
            // window was shown, which some frame state ignores).
            SetWindowPos(self.hwnd, null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
            RedrawWindow(self.hwnd, null(), null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_ALLCHILDREN);
        }
    }

    fn set_theme(&mut self, theme: u8) {
        self.settings.theme = theme;
        self.apply_theme();
        self.update_menu_checks();
        self.settings.save();
    }

    /// Details = report view with the small system list; the icon views swap in a bigger
    /// size of the same system image list (shared index space, so no icon re-resolve).
    fn apply_view(&mut self) {
        unsafe {
            if self.settings.view == VIEW_DETAILS {
                self.thumbs.set_size(0);
                SendMessageW(self.list, LVM_SETVIEW, LV_VIEW_DETAILS as usize, 0);
                return;
            }
            let shil = match self.settings.view {
                1 => SHIL_LARGE,      // 32px @96 DPI
                2 => SHIL_EXTRALARGE, // 48px
                _ => SHIL_JUMBO,      // 256px
            };
            let il = icons::sys_list(shil);
            if il == 0 {
                return;
            }
            SendMessageW(self.list, LVM_SETIMAGELIST, LVSIL_NORMAL as usize, il);
            let (mut cx, mut cy) = (32, 32);
            ImageList_GetIconSize(il as HIMAGELIST, &mut cx, &mut cy);
            self.thumbs.set_size(if self.settings.view >= VIEW_THUMBS_FROM { cx } else { 0 });
            // Cell = icon + room for a two-line label. Without this the ListView keeps
            // the spacing it computed for the previous image size.
            let w = cx + scale(if cx >= 128 { 24 } else { 44 }, self.dpi);
            let h = cy + scale(40, self.dpi);
            SendMessageW(self.list, LVM_SETICONSPACING, 0, (w & 0xFFFF | (h & 0xFFFF) << 16) as isize);
            SendMessageW(self.list, LVM_SETVIEW, LV_VIEW_ICON as usize, 0);
            SendMessageW(self.list, LVM_ARRANGE, LVA_DEFAULT as usize, 0);
        }
    }

    fn set_view(&mut self, view: u8) {
        if self.settings.view == view {
            return;
        }
        // Remember the details column widths before the report view goes away.
        if self.settings.view == VIEW_DETAILS {
            self.save_columns();
        }
        self.settings.view = view;
        self.apply_view();
        self.update_menu_checks();
    }

    fn update_menu_checks(&self) {
        unsafe {
            let chk = |id: u16, on: bool| {
                CheckMenuItem(self.menu, id as u32, MF_BYCOMMAND | if on { MF_CHECKED } else { MF_UNCHECKED });
            };
            chk(ID_CASE, self.settings.match_case);
            chk(ID_PATH, self.settings.match_path);
        }
        check_radios(self.menu, &self.settings);
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
        // Not at logon (--startup stays hidden): only once the user actually opens us.
        shell::prewarm();
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
        }
        // In an icon view the columns are hidden; their widths were saved on the switch.
        if self.settings.view == VIEW_DETAILS {
            self.save_columns();
        }
        self.settings.save();
    }

    fn save_columns(&mut self) {
        for i in 0..4 {
            let w = unsafe { SendMessageW(self.list, LVM_GETCOLUMNWIDTH, i, 0) as i32 };
            if w > 0 {
                self.settings.cols[i] = w * 96 / self.dpi as i32;
            }
        }
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
        let mut result = Vec::new();
        for i in idx {
            if let Some(r) = self.rows.get(&i) {
                result.push(r.full.clone());
            }
        }
        if result.is_empty() && !self.saved_selection.is_empty() {
            return self.saved_selection.iter().take(cap).cloned().collect();
        }
        result
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
                let idx = f + k as u32;
                if !self.saved_selection.is_empty() && !self.saved_selection.contains(&v.full) {
                    let mut desel: LVITEMW = unsafe { std::mem::zeroed() };
                    desel.stateMask = LVIS_SELECTED;
                    unsafe { SendMessageW(self.list, LVM_SETITEMSTATE, idx as usize, &desel as *const _ as isize) };
                }
                self.rows.insert(idx, v);
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

    /// Open the row's folder in Explorer with the row itself selected.
    fn reveal_row(&mut self, i: u32) {
        if !self.rows.contains_key(&i) {
            self.fetch_sync(&[i]);
        }
        if let Some(r) = self.rows.get(&i) {
            shell::reveal(&r.full);
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
        self.cut_paths.clear();
        let n = paths.len();
        let msg = if as_text {
            shell::copy_text(self.hwnd, &paths.join("\r\n"));
            if n == 1 {
                "Copied path to clipboard".to_string()
            } else {
                format!("Copied {} paths to clipboard", thousands(n as u64))
            }
        } else {
            shell::copy_files(self.hwnd, &paths);
            if n == 1 {
                let name = std::path::Path::new(&paths[0])
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&paths[0]);
                format!("Copied '{}' to clipboard", name)
            } else {
                format!("Copied {} items to clipboard", thousands(n as u64))
            }
        };
        self.clip_msg = Some((msg, std::time::Instant::now()));
        unsafe {
            SendMessageW(self.list, LVM_REDRAWITEMS, 0, (self.total.saturating_sub(1)) as isize);
        }
        self.invalidate_status();
    }

    fn cut_selected(&mut self) {
        let paths = self.selected_paths(100_000);
        if paths.is_empty() {
            return;
        }
        self.cut_paths = paths.iter().cloned().collect();
        shell::cut_files(self.hwnd, &paths);
        let n = paths.len();
        let msg = if n == 1 {
            let name = std::path::Path::new(&paths[0])
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&paths[0]);
            format!("Cut '{}' to clipboard", name)
        } else {
            format!("Cut {} items to clipboard", thousands(n as u64))
        };
        self.clip_msg = Some((msg, std::time::Instant::now()));
        unsafe {
            SendMessageW(self.list, LVM_REDRAWITEMS, 0, (self.total.saturating_sub(1)) as isize);
        }
        self.invalidate_status();
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

    /// Show the window searching inside `dir`: the query becomes the quoted folder path
    /// with a trailing backslash. A term containing `\` matches the full path, so that
    /// alone lists everything below the folder (not the folder itself), and the caret
    /// sits after it so typing narrows inside the folder.
    fn open_folder(&mut self, dir: &str) {
        // Explorer quotes %1, and `"C:\"` reaches argv as `C:"` (backslash-quote is an
        // escaped quote to the CRT), so strip stray quotes before normalising.
        let mut d = dir.trim().trim_matches('"').to_string();
        if !d.ends_with('\\') {
            d.push('\\');
        }
        self.show();
        unsafe {
            // SetWindowText fires EN_CHANGE, which runs the query.
            SetWindowTextW(self.edit, wide(&format!("\"{d}\" ")).as_ptr());
            SetFocus(self.edit);
            // Caret at the end. Not a "huge" start: EM_SETSEL truncates it to -1, which
            // means "remove the selection" and leaves the caret at 0.
            let n = GetWindowTextLengthW(self.edit) as usize;
            SendMessageW(self.edit, EM_SETSEL, n, n as isize);
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
                if !self.cut_paths.is_empty() {
                    self.cut_paths.clear();
                    self.clip_msg = None;
                    unsafe {
                        SendMessageW(self.list, LVM_REDRAWITEMS, 0, (self.total.saturating_sub(1)) as isize);
                    }
                    self.invalidate_status();
                    return true;
                }
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
                self.thumbs.invalidate_failed();
                self.run_query(true);
                true
            }
            VK_DELETE if in_list => {
                let p = self.selected_paths(10_000);
                if !p.is_empty() {
                    let shift = unsafe { GetKeyState(VK_SHIFT as i32) < 0 };
                    let msg = if p.len() == 1 {
                        let name = std::path::Path::new(&p[0])
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(&p[0]);
                        if shift {
                            format!("Are you sure you want to permanently delete '{}'?", name)
                        } else {
                            format!("Are you sure you want to move '{}' to the Recycle Bin?", name)
                        }
                    } else {
                        if shift {
                            format!("Are you sure you want to permanently delete these {} items?", p.len())
                        } else {
                            format!("Are you sure you want to move these {} items to the Recycle Bin?", p.len())
                        }
                    };
                    let title = if shift { "Delete Permanently" } else { "Delete" };
                    let ret = unsafe {
                        MessageBoxW(
                            self.hwnd,
                            wide(&msg).as_ptr(),
                            wide(title).as_ptr(),
                            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON1,
                        )
                    };
                    if ret == IDYES {
                        shell::verb(self.hwnd, &p, "delete");
                    }
                }
                true
            }
            VK_BACK if ctrl && in_edit => {
                // Classic EDIT inserts a box glyph for Ctrl+Backspace; delete a word instead.
                let mut sel_start: u32 = 0;
                let mut sel_end: u32 = 0;
                unsafe {
                    SendMessageW(self.edit, EM_GETSEL, &mut sel_start as *mut _ as usize, &mut sel_end as *mut _ as isize);
                }
                if sel_start != sel_end {
                    unsafe {
                        SendMessageW(self.edit, EM_REPLACESEL, 1, wide("").as_ptr() as isize);
                    }
                } else if sel_start > 0 {
                    let text = self.search_text();
                    let units: Vec<u16> = text.encode_utf16().collect();
                    let mut start = (sel_start as usize).min(units.len());
                    while start > 0 && units[start - 1] == ' ' as u16 {
                        start -= 1;
                    }
                    while start > 0 && units[start - 1] != ' ' as u16 && units[start - 1] != '\\' as u16 {
                        start -= 1;
                    }
                    unsafe {
                        SendMessageW(self.edit, EM_SETSEL, start, sel_start as isize);
                        SendMessageW(self.edit, EM_REPLACESEL, 1, wide("").as_ptr() as isize);
                    }
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
                b'X' if in_list => {
                    self.cut_selected();
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
            ID_CUT => self.cut_selected(),
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
            ID_SORT_ASC | ID_SORT_DESC => {
                let desc = id == ID_SORT_DESC;
                if desc != self.settings.desc {
                    // set_sort on the current column is exactly "flip direction".
                    self.set_sort(self.settings.sort);
                }
            }
            ID_REFRESH => {
                self.thumbs.invalidate_failed();
                self.run_query(true);
            }
            x if (ID_VIEW_BASE..ID_VIEW_BASE + 4).contains(&x) => self.set_view((x - ID_VIEW_BASE) as u8),
            x if (ID_THEME_BASE..ID_THEME_BASE + 4).contains(&x) => self.set_theme((x - ID_THEME_BASE) as u8),
            ID_INSTALL => self.install_service(false),
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
                        // With a thumbnail cached the ListView must not draw the type icon:
                        // custom draw paints the picture into the empty icon slot instead.
                        // Hiding it here (not painting over it) keeps the selection box,
                        // which the ListView draws behind the icon, intact.
                        let thumb = self.thumbs.active() && row.is_some_and(|r| !r.dir && self.thumbs.has(&r.full));
                        di.item.iImage = if thumb { I_IMAGENONE } else { row.map(|r| r.icon).unwrap_or(I_IMAGENONE) };
                    }
                    if di.item.mask & LVIF_STATE != 0 {
                        let is_cut = row.is_some_and(|r| self.cut_paths.contains(&r.full));
                        if is_cut {
                            di.item.state |= LVIS_CUT;
                            di.item.stateMask |= LVIS_CUT;
                        }
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
                // Details view never gets past the guard: it keeps the stock paint path.
                // Details: orange selection. Icon views: thumbnails (they keep the stock
                // selection; there the highlight is drawn around icon and label separately,
                // which custom draw can't recolour as one box).
                NM_CUSTOMDRAW if self.thumbs.active() || self.settings.view == VIEW_DETAILS => Some(self.custom_draw(&mut *(lp as *mut NMLVCUSTOMDRAW))),
                NM_DBLCLK => {
                    let ia = &*(lp as *const NMITEMACTIVATE);
                    // The ListView hit-tests the sub-item for us in report view, so a
                    // double-click on the Path cell means "take me there", not "run it".
                    // Icon views always report sub-item 0 and keep the open behaviour.
                    if ia.iItem >= 0 && ia.iSubItem == 1 && self.settings.view == VIEW_DETAILS {
                        self.reveal_row(ia.iItem as u32);
                    } else if ia.iItem >= 0 {
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
                LVN_ITEMCHANGED => {
                    let nmlv = &*(lp as *const NMLISTVIEW);
                    if nmlv.uChanged & LVIF_STATE != 0
                        && (nmlv.uNewState ^ nmlv.uOldState) & LVIS_SELECTED != 0
                        && !self.restoring_selection
                    {
                        self.saved_selection.clear();
                        self.saved_focus = None;
                        let item = nmlv.iItem;
                        if item > 0 {
                            SendMessageW(self.list, LVM_REDRAWITEMS, (item - 1) as usize, (item - 1) as isize);
                        }
                        if (item + 1) < self.total as i32 {
                            SendMessageW(self.list, LVM_REDRAWITEMS, (item + 1) as usize, (item + 1) as isize);
                        }
                    }
                    self.update_detail();
                    self.invalidate_status();
                    None
                }
                LVN_ODSTATECHANGED => {
                    self.update_detail();
                    self.invalidate_status();
                    None
                }
                _ => None,
            }
        }
    }

    // ---- thumbnails ---------------------------------------------------------------------

    fn custom_draw(&mut self, cd: &mut NMLVCUSTOMDRAW) -> LRESULT {
        let details = self.settings.view == VIEW_DETAILS;
        match cd.nmcd.dwDrawStage {
            // Once per paint pass: cheap to re-aim the worker queue at what's on screen.
            CDDS_PREPAINT => {
                if self.thumbs.active() {
                    self.schedule_thumbs();
                }
                CDRF_NOTIFYITEMDRAW as LRESULT
            }
            CDDS_ITEMPREPAINT if details => {
                let i = cd.nmcd.dwItemSpec;
                // Ask the list, not uItemState: an owner-data list doesn't reliably set
                // CDIS_SELECTED there, and the theme decides from the real state anyway.
                let state = unsafe { SendMessageW(self.list, LVM_GETITEMSTATE, i, (LVIS_SELECTED | LVIS_DROPHILITED) as isize) } as u32;
                let selected = (state & (LVIS_SELECTED | LVIS_DROPHILITED)) != 0;
                let is_cut = self.rows.get(&(i as u32)).is_some_and(|r| self.cut_paths.contains(&r.full));
                if !selected {
                    // Hover: same trick with a fainter orange instead of the theme's blue
                    // hot box. Our own hover_row, because an owner-data list reports
                    // neither CDIS_HOT nor LVM_GETHOTITEM (that one needs track-select).
                    let is_hover = i as i32 == self.hover_row;
                    if !is_hover && !is_cut {
                        return CDRF_DODEFAULT as LRESULT;
                    }
                    if is_hover {
                        let rc = self.row_rect(i);
                        unsafe { FillRect(cd.nmcd.hdc, &rc, self.theme.br_hover) };
                        cd.clrTextBk = self.theme.hover;
                    } else {
                        cd.clrTextBk = self.theme.field;
                    }
                    cd.nmcd.uItemState &= !(CDIS_SELECTED | CDIS_FOCUS | CDIS_HOT | 0x0040 | 0x1000);
                    cd.clrText = if is_cut { self.theme.dim } else { self.theme.fg };
                    return CDRF_NEWFONT as LRESULT;
                }
                // Fill the whole row ourselves (clrTextBk alone leaves the icon cell
                // unpainted), then clear the state bits so the visual style doesn't lay
                // its blue selection / hover box over it. Text is then drawn on clrTextBk.
                let rc = self.row_rect(i);
                unsafe { FillRect(cd.nmcd.hdc, &rc, self.theme.br_sel) };
                cd.nmcd.uItemState &= !(CDIS_SELECTED | CDIS_FOCUS | CDIS_HOT | 0x0040 | 0x1000);
                cd.clrText = if is_cut { self.theme.dim } else { self.theme.fg };
                cd.clrTextBk = self.theme.sel;
                (CDRF_NEWFONT | CDRF_NOTIFYPOSTPAINT) as LRESULT
            }
            CDDS_ITEMPOSTPAINT if details => {
                // Only selected rows asked for post-paint: frame them in accent orange.
                // When contiguous rows are selected, avoid double-width colliding borders
                // by omitting the shared horizontal divider between adjacent selected items.
                let i = cd.nmcd.dwItemSpec;
                let rc = self.row_rect(i);
                unsafe {
                    // Left and right borders
                    let left_rc = RECT { left: rc.left, top: rc.top, right: rc.left + 1, bottom: rc.bottom };
                    FillRect(cd.nmcd.hdc, &left_rc, self.theme.br_accent);

                    let right_rc = RECT { left: rc.right - 1, top: rc.top, right: rc.right, bottom: rc.bottom };
                    FillRect(cd.nmcd.hdc, &right_rc, self.theme.br_accent);

                    // Top border: only if item above is not selected
                    let prev_sel = if i > 0 {
                        (SendMessageW(self.list, LVM_GETITEMSTATE, i - 1, (LVIS_SELECTED | LVIS_DROPHILITED) as isize) as u32
                            & (LVIS_SELECTED | LVIS_DROPHILITED))
                            != 0
                    } else {
                        false
                    };
                    if !prev_sel {
                        let top_rc = RECT { left: rc.left, top: rc.top, right: rc.right, bottom: rc.top + 1 };
                        FillRect(cd.nmcd.hdc, &top_rc, self.theme.br_accent);
                    }

                    // Bottom border: only if item below is not selected
                    let next_sel = if (i + 1) < self.total as usize {
                        (SendMessageW(self.list, LVM_GETITEMSTATE, i + 1, (LVIS_SELECTED | LVIS_DROPHILITED) as isize) as u32
                            & (LVIS_SELECTED | LVIS_DROPHILITED))
                            != 0
                    } else {
                        false
                    };
                    if !next_sel {
                        let bot_rc = RECT { left: rc.left, top: rc.bottom - 1, right: rc.right, bottom: rc.bottom };
                        FillRect(cd.nmcd.hdc, &bot_rc, self.theme.br_accent);
                    }
                }
                CDRF_DODEFAULT as LRESULT
            }
            CDDS_ITEMPREPAINT => {
                let i = cd.nmcd.dwItemSpec as u32;
                let want = self.rows.get(&i).or_else(|| self.stale.get(&i)).is_some_and(|r| !r.dir && self.thumbs.has(&r.full));
                (if want { CDRF_NOTIFYPOSTPAINT } else { CDRF_DODEFAULT }) as LRESULT
            }
            CDDS_ITEMPOSTPAINT => {
                let i = cd.nmcd.dwItemSpec as u32;
                if let Some(r) = self.rows.get(&i).or_else(|| self.stale.get(&i)) {
                    let mut rc = RECT { left: LVIR_ICON as i32, top: 0, right: 0, bottom: 0 };
                    if unsafe { SendMessageW(self.list, LVM_GETITEMRECT, i as usize, &mut rc as *mut _ as isize) } != 0 {
                        self.thumbs.draw(cd.nmcd.hdc, &rc, &r.full);
                    }
                }
                CDRF_DODEFAULT as LRESULT
            }
            _ => CDRF_DODEFAULT as LRESULT,
        }
    }

    /// A Details row's full width, as the selection box should cover it.
    fn row_rect(&self, i: usize) -> RECT {
        let mut rc = RECT { left: LVIR_BOUNDS as i32, top: 0, right: 0, bottom: 0 };
        unsafe { SendMessageW(self.list, LVM_GETITEMRECT, i, &mut rc as *mut _ as isize) };
        rc
    }

    /// Rows currently on screen in an icon view. LVM_GETTOPINDEX / GETCOUNTPERPAGE are
    /// useless there (0 / the total), but an owner-data icon view is always a plain
    /// auto-arranged grid, so a few item positions give the whole layout.
    fn visible_range(&self) -> Option<(u32, u32)> {
        if self.total == 0 {
            return None;
        }
        let pos = |i: u32| {
            let mut p = POINT { x: 0, y: 0 };
            unsafe { SendMessageW(self.list, LVM_GETITEMPOSITION, i as usize, &mut p as *mut _ as isize) };
            p
        };
        let mut rc: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.list, &mut rc) };
        let p0 = pos(0);
        let spacing = unsafe { SendMessageW(self.list, LVM_GETITEMSPACING, 0, 0) } as usize;
        let (sx, sy) = ((spacing & 0xFFFF).max(1) as i32, ((spacing >> 16) & 0xFFFF).max(1) as i32);
        // Columns: the spacing estimate, corrected against where the second row starts.
        let mut cols = ((rc.right - p0.x.max(0)) / sx).clamp(1, self.total as i32) as u32;
        while cols > 1 && pos(cols - 1).y != p0.y {
            cols -= 1;
        }
        while cols < self.total && pos(cols).y == p0.y {
            cols += 1;
        }
        // Measured row pitch beats the spacing value if the two ever disagree.
        let pitch = if cols < self.total { (pos(cols).y - p0.y).max(1) } else { sy };
        let r0 = (-p0.y).div_euclid(pitch).max(0) as u32;
        let r1 = (rc.bottom - 1 - p0.y).div_euclid(pitch).max(0) as u32;
        let lo = r0.saturating_mul(cols).min(self.total - 1);
        let hi = (r1 + 1).saturating_mul(cols).min(self.total) - 1;
        Some((lo, hi.max(lo)))
    }

    /// Point the thumbnail workers at the viewport: visible rows first, then one screen
    /// below (where scrolling usually goes), then half a screen above. Everything else
    /// is dropped from the queue, which is what keeps a fast scroll from piling up work.
    fn schedule_thumbs(&mut self) {
        let Some((lo, hi)) = self.visible_range() else { return };
        let n = hi - lo + 1;
        let (wlo, whi) = (lo.saturating_sub(n / 2), (hi + n).min(self.total - 1));
        if (wlo, whi) == self.thumbs.window && !self.thumbs.dirty {
            return;
        }
        self.thumbs.window = (wlo, whi);
        self.thumbs.dirty = false;
        let rows = &self.rows;
        let thumbs = &mut self.thumbs;
        let order = (lo..=hi).chain(hi + 1..=whi).chain((wlo..lo).rev());
        let mut items = Vec::new();
        for i in order {
            if let Some(r) = rows.get(&i) {
                if !r.dir && thumbs.wants(&r.full, r.mtime, r.bytes) {
                    items.push((i, r.full.as_str(), r.mtime, r.bytes));
                }
            }
        }
        thumbs.schedule(items.into_iter());
    }

    fn on_thumb(&mut self, d: Done) {
        // Check the row before `accept` consumes the path: a new query may have moved it.
        let same_row = self.rows.get(&d.row).is_some_and(|r| r.full == d.path);
        let row = d.row;
        let Some(repainted) = self.thumbs.accept(d) else {
            self.thumbs.dirty = true;
            if self.thumbs.active() {
                self.schedule_thumbs();
            }
            return;
        };
        if repainted {
            unsafe {
                if same_row {
                    SendMessageW(self.list, LVM_REDRAWITEMS, row as usize, row as isize);
                } else {
                    InvalidateRect(self.list, null(), 0);
                }
            }
        }
        if self.thumbs.over_budget() {
            let (wlo, whi) = self.thumbs.window;
            let keep: HashSet<&str> = (wlo..=whi).filter_map(|i| self.rows.get(&i)).map(|r| r.full.as_str()).collect();
            self.thumbs.evict(&keep);
        }
    }

    // ---- caption ----------------------------------------------------------------------
    //
    // The title bar is ours: WM_NCCALCSIZE hands its strip to the client area (the side
    // and bottom resize borders, shadow and snap stay native), and WM_NCHITTEST tells
    // Windows which part of that strip is caption, a button or the top resize edge.
    // Windows has no supported way to add a button to its own title bar, which is the
    // only reason for all of this.

    fn caption_h(&self) -> i32 {
        scale(32, self.dpi)
    }

    /// Caption buttons left to right, Win11-sized (46 px wide at 96 DPI).
    fn caption_buttons(&self) -> [(u32, RECT); 4] {
        let mut rc: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut rc) };
        let (bw, h) = (scale(46, self.dpi), self.caption_h());
        let at = |k: i32| RECT { left: rc.right - k * bw, top: 0, right: rc.right - (k - 1) * bw, bottom: h };
        [(HTMENU, at(4)), (HTMINBUTTON, at(3)), (HTMAXBUTTON, at(2)), (HTCLOSE, at(1))]
    }

    fn invalidate_caption(&self) {
        let mut rc: RECT = unsafe { std::mem::zeroed() };
        unsafe {
            GetClientRect(self.hwnd, &mut rc);
            rc.bottom = self.caption_h();
            InvalidateRect(self.hwnd, &rc, 0);
        }
    }

    /// Hit code for a point (client coordinates) that DefWindowProc called client area.
    /// HTMAXBUTTON matters beyond our own clicks: it's what makes Windows 11 show the
    /// snap layouts flyout on hover.
    fn caption_hit(&self, pt: POINT) -> u32 {
        if pt.y >= self.caption_h() {
            return HTCLIENT;
        }
        unsafe {
            // No top border any more, so the top resize edge lives inside the caption.
            if IsZoomed(self.hwnd) == 0 && pt.y < GetSystemMetricsForDpi(SM_CYSIZEFRAME, self.dpi) {
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(self.hwnd, &mut rc);
                let corner = scale(16, self.dpi);
                return if pt.x < corner {
                    HTTOPLEFT
                } else if pt.x >= rc.right - corner {
                    HTTOPRIGHT
                } else {
                    HTTOP
                };
            }
        }
        self.caption_buttons().iter().find(|(_, r)| unsafe { PtInRect(r, pt) } != 0).map_or(HTCAPTION, |b| b.0)
    }

    fn set_caption_hot(&mut self, ht: u32) {
        let ht = if is_caption_button(ht) { ht } else { 0 };
        if ht == self.cap_hot {
            return;
        }
        self.cap_hot = ht;
        if ht != 0 {
            // Non-client hover has no WM_MOUSELEAVE of its own; ask for WM_NCMOUSELEAVE.
            let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE | TME_NONCLIENT, hwndTrack: self.hwnd, dwHoverTime: 0 };
            unsafe { TrackMouseEvent(&mut t) };
        }
        self.invalidate_caption();
    }

    /// Button press. DefWindowProc never sees it: its own handling would run the
    /// Windows 95 button-tracking loop and paint a classic button over ours.
    fn caption_press(&mut self, ht: u32) -> bool {
        self.cap_down = ht;
        self.invalidate_caption();
        ht == HTMENU
    }

    /// Button release: acts only if it lands on the button that was pressed, like a
    /// real button (press, slide off, release = nothing).
    fn caption_release(&mut self, ht: u32) {
        let down = std::mem::take(&mut self.cap_down);
        self.invalidate_caption();
        if down != ht {
            return;
        }
        let sc = match ht {
            HTMINBUTTON => SC_MINIMIZE,
            HTMAXBUTTON if unsafe { IsZoomed(self.hwnd) } != 0 => SC_RESTORE,
            HTMAXBUTTON => SC_MAXIMIZE,
            HTCLOSE => SC_CLOSE, // WM_CLOSE: hides to the tray, same as before
            _ => return,         // HTMENU acts on press, never here
        };
        self.cap_hot = 0;
        unsafe { PostMessageW(self.hwnd, WM_SYSCOMMAND, sc as usize, 0) };
    }

    /// Icon, title and the four buttons. The background is already filled by `paint`.
    fn paint_caption(&self, hdc: HDC) {
        let h = self.caption_h();
        let buttons = self.caption_buttons();
        let text = if self.active { self.theme.fg } else { self.theme.dim };
        unsafe {
            let ic = GetSystemMetricsForDpi(SM_CXSMICON, self.dpi);
            DrawIconEx(hdc, scale(10, self.dpi), (h - ic) / 2, self.small_icon, ic, ic, 0, null_mut(), DI_NORMAL);
            let old = SelectObject(hdc, self.font as _);
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, text);
            let mut t = wide("Nothing");
            let mut r = RECT { left: scale(10, self.dpi) + ic + scale(8, self.dpi), top: 0, right: buttons[0].1.left, bottom: h };
            DrawTextW(hdc, t.as_mut_ptr(), -1, &mut r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
            // Segoe MDL2 Assets: the glyph font Windows draws its own caption buttons with
            // (present on 10 and 11, unlike Segoe Fluent Icons).
            SelectObject(hdc, self.glyphs as _);
            for (ht, rc) in buttons {
                let hot = self.cap_hot == ht;
                if hot || self.cap_down == ht {
                    FillRect(hdc, &rc, if ht == HTCLOSE { self.theme.br_close } else { self.theme.br_hot });
                }
                let glyph = match ht {
                    HTMENU => '\u{E700}', // GlobalNavButton (hamburger)
                    HTMINBUTTON => '\u{E921}',
                    HTMAXBUTTON if IsZoomed(self.hwnd) != 0 => '\u{E923}', // restore
                    HTMAXBUTTON => '\u{E922}',
                    _ => '\u{E8BB}',
                };
                SetTextColor(hdc, match ht {
                    HTCLOSE if hot => rgb(255, 255, 255),
                    HTMENU => self.theme.accent,
                    _ => text,
                });
                let mut g = wide(&glyph.to_string());
                let mut gr = rc;
                DrawTextW(hdc, g.as_mut_ptr(), -1, &mut gr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            }
            SelectObject(hdc, old);
        }
    }
}

fn is_caption_button(ht: u32) -> bool {
    matches!(ht, HTMENU | HTMINBUTTON | HTMAXBUTTON | HTCLOSE)
}

fn dispatch_command(id: u16) {
    enum Action {
        None,
        ConfirmUninstall(HWND),
        Syntax,
        About(bool),
    }
    let act = with_app(|a| {
        match id {
            ID_UNINSTALL => Action::ConfirmUninstall(a.hwnd),
            ID_SYNTAX => Action::Syntax,
            ID_ABOUT => Action::About(a.hotkey_ok),
            _ => {
                a.on_command(id);
                Action::None
            }
        }
    }).unwrap_or(Action::None);

    match act {
        Action::ConfirmUninstall(hwnd) => {
            let r = unsafe {
                MessageBoxW(hwnd, wide("Remove the indexing service and its index?").as_ptr(), wide("Nothing").as_ptr(), MB_YESNO | MB_ICONQUESTION)
            };
            if r == IDYES {
                with_app(|a| a.install_service(true));
            }
        }
        Action::Syntax => {
            message(
                "Search syntax",
                "foo bar\tboth words (AND)\n\
                 foo | bar\teither word (OR)\n\
                 !foo\t\texclude\n\
                 \"a b\"\t\texact phrase with spaces\n\
                 *.mkv  a?c\twildcards (match the whole name)\n\
                 users\\nihil\\\tany \\ in a term matches the full path\n\
                 ext:jpg;png\tby extension\n\
                 !video  !doc\tby type: video audio image doc archive exe\n\
                 file:  folder:\tonly files / only folders\n\
                 case:  path:\tper-term match case / match full path\n\n\
                 Enter open · Ctrl+Enter open folder · Ctrl+C copy · Ctrl+Shift+C copy path\n\
                 Del delete · Alt+Enter properties · Esc clear/hide · Win+Alt+Space show",
                false,
            );
        }
        Action::About(hotkey_ok) => {
            message(
                "About Nothing",
                &format!(
                    "Nothing {}\nInstant file-name search for Windows.\n\nReads the NTFS Master File Table directly and follows the USN change journal.\n{}",
                    env!("CARGO_PKG_VERSION"),
                    if hotkey_ok { "Global hotkey: Win+Alt+Space" } else { "Global hotkey unavailable (already taken)." }
                ),
                false,
            );
        }
        Action::None => {}
    }
}

fn run_caption_menu() {
    let prep = with_app(|a| {
        let r = a.caption_buttons()[0].1;
        a.cap_down = HTMENU;
        a.cap_hot = HTMENU;
        a.invalidate_caption();
        (a.hwnd, a.menu, r)
    });
    let Some((hwnd, menu, mut r)) = prep else { return };
    let cmd = unsafe {
        UpdateWindow(hwnd);
        let mut pts = [POINT { x: r.left, y: r.top }, POINT { x: r.right, y: r.bottom }];
        MapWindowPoints(hwnd, null_mut(), pts.as_mut_ptr(), 2);
        r = RECT { left: pts[0].x, top: pts[0].y, right: pts[1].x, bottom: pts[1].y };
        let tpm = TPMPARAMS { cbSize: std::mem::size_of::<TPMPARAMS>() as u32, rcExclude: r };
        TrackPopupMenuEx(menu, TPM_RETURNCMD | TPM_RIGHTALIGN | TPM_TOPALIGN | TPM_VERTICAL, r.right, r.bottom, hwnd, &tpm)
    };
    with_app(|a| {
        a.cap_down = 0;
        a.cap_hot = 0;
        a.invalidate_caption();
    });
    if cmd > 0 {
        dispatch_command(cmd as u16);
    }
}

fn run_background_menu(pt: POINT) {
    let prep = with_app(|a| {
        let m = unsafe { CreatePopupMenu() };
        sub(m, view_menu(), "&View");
        sub(m, sort_menu(), "&Sort By");
        sep(m);
        add(m, ID_SELECT_ALL, "Select &All\tCtrl+A");
        add(m, ID_REFRESH, "&Refresh\tF5");
        check_radios(m, &a.settings);
        if a.total == 0 {
            unsafe { EnableMenuItem(m, ID_SELECT_ALL as u32, MF_BYCOMMAND | MF_GRAYED) };
        }
        (a.hwnd, m)
    });
    let Some((hwnd, m)) = prep else { return };
    let cmd = unsafe {
        let c = TrackPopupMenuEx(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, hwnd, null());
        DestroyMenu(m);
        c
    };
    if cmd > 0 {
        dispatch_command(cmd as u16);
    }
}

fn run_context_menu(pt: POINT, keyboard: bool) {
    let check = with_app(|a| {
        let mut target_paths = Vec::new();
        if keyboard {
            if a.selected_count() > 0 {
                target_paths = a.selected_paths(10_000);
            }
        } else {
            let mut ht: LVHITTESTINFO = unsafe { std::mem::zeroed() };
            ht.pt = pt;
            unsafe {
                ScreenToClient(a.list, &mut ht.pt);
                let hit = SendMessageW(a.list, LVM_HITTEST, 0, &mut ht as *mut _ as isize) as i32;
                if hit >= 0 {
                    let is_sel = SendMessageW(a.list, LVM_GETITEMSTATE, hit as usize, LVIS_SELECTED as isize) != 0;
                    if !is_sel {
                        // Clear previous selection and select the right-clicked item
                        let mut desel: LVITEMW = std::mem::zeroed();
                        desel.stateMask = LVIS_SELECTED;
                        SendMessageW(a.list, LVM_SETITEMSTATE, usize::MAX, &desel as *const _ as isize);

                        let mut sel: LVITEMW = std::mem::zeroed();
                        sel.state = LVIS_SELECTED | LVIS_FOCUSED;
                        sel.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
                        SendMessageW(a.list, LVM_SETITEMSTATE, hit as usize, &sel as *const _ as isize);
                    }
                    target_paths = a.selected_paths(10_000);
                }
            }
        }
        (a.hwnd, target_paths)
    });
    let Some((hwnd, paths)) = check else { return };
    if paths.is_empty() {
        run_background_menu(pt);
        return;
    }
    let extra = [(ID_CTX_REVEAL, "Open &Path"), (ID_CTX_COPY_PATH, "Copy &Full Path")];
    let result = shell::context_menu(hwnd, &paths, pt, &extra);
    match result {
        Some(ID_CTX_REVEAL) => {
            with_app(|a| a.reveal_selected());
        }
        Some(ID_CTX_COPY_PATH) => {
            with_app(|a| a.copy_selected(true));
        }
        _ => {}
    }
    if let Some(effect) = shell::check_clipboard_effect() {
        if effect == windows_sys::Win32::System::Ole::DROPEFFECT_MOVE {
            with_app(|a| {
                a.cut_paths = paths.iter().cloned().collect();
                let n = paths.len();
                let msg = if n == 1 {
                    let name = std::path::Path::new(&paths[0])
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&paths[0]);
                    format!("Cut '{}' to clipboard", name)
                } else {
                    format!("Cut {} items to clipboard", thousands(n as u64))
                };
                a.clip_msg = Some((msg, std::time::Instant::now()));
                unsafe {
                    SendMessageW(a.list, LVM_REDRAWITEMS, 0, (a.total.saturating_sub(1)) as isize);
                }
                a.invalidate_status();
            });
        } else if effect == windows_sys::Win32::System::Ole::DROPEFFECT_COPY {
            with_app(|a| {
                a.cut_paths.clear();
                let n = paths.len();
                let msg = if n == 1 {
                    let name = std::path::Path::new(&paths[0])
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&paths[0]);
                    format!("Copied '{}' to clipboard", name)
                } else {
                    format!("Copied {} items to clipboard", thousands(n as u64))
                };
                a.clip_msg = Some((msg, std::time::Instant::now()));
                unsafe {
                    SendMessageW(a.list, LVM_REDRAWITEMS, 0, (a.total.saturating_sub(1)) as isize);
                }
                a.invalidate_status();
            });
        }
    }
}

fn run_tray_menu() {
    let hwnd = match with_app(|a| a.hwnd) {
        Some(h) => h,
        None => return,
    };
    let cmd = unsafe {
        let m = CreatePopupMenu();
        AppendMenuW(m, MF_STRING, ID_TRAY_SHOW as usize, wide("&Show Nothing").as_ptr());
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        AppendMenuW(m, MF_STRING, ID_EXIT as usize, wide("E&xit").as_ptr());
        SetMenuDefaultItem(m, ID_TRAY_SHOW as u32, 0);
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let c = TrackPopupMenuEx(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, hwnd, null());
        DestroyMenu(m);
        c
    };
    match cmd as u16 {
        ID_TRAY_SHOW => { with_app(|a| a.show()); }
        ID_EXIT => { with_app(|a| a.exit()); }
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // While a shell context menu is open, its owner-drawn submenus need these messages.
    if matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
        if let Some(r) = shell::forward_menu_msg(msg, wp, lp) {
            return r;
        }
    }
    // Caption strip -> client area: DefWindowProc lays out the standard frame, then the
    // top goes back to the window's top edge. Ahead of the App check because it first
    // runs inside CreateWindowEx.
    if msg == WM_NCCALCSIZE && wp != 0 {
        unsafe {
            let p = &mut *(lp as *mut NCCALCSIZE_PARAMS);
            let (top, left) = (p.rgrc[0].top, p.rgrc[0].left);
            let r = DefWindowProcW(hwnd, msg, wp, lp);
            // A maximized window hangs its borders off-screen on every side; inset the top
            // by the same amount or the caption would start above the monitor.
            p.rgrc[0].top = top + if IsZoomed(hwnd) != 0 { p.rgrc[0].left - left } else { 0 };
            return r;
        }
    }
    unsafe {
        match msg {
            WM_SIZE => {
                if with_app(|a| a.layout()).is_none() {
                    PENDING_LAYOUT.with(|l| l.set(true));
                }
                0
            }
            WM_PAINT => {
                with_app(|a| a.paint());
                0
            }
            WM_ERASEBKGND => 1,
            // The painted field is larger than the EDIT; clicks on its padding still focus it.
            WM_LBUTTONDOWN => {
                let pt = POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 };
                with_app(|a| {
                    if PtInRect(&a.field_rect(), pt) != 0 {
                        SetFocus(a.edit);
                        SendMessageW(a.edit, EM_SETSEL, 0, -1);
                    }
                });
                0
            }
            WM_CTLCOLOREDIT => {
                let (fg, bg, br) = CACHED_THEME.with(|c| c.get());
                if !br.is_null() {
                    let hdc = wp as HDC;
                    SetTextColor(hdc, fg);
                    SetBkColor(hdc, bg);
                    br as LRESULT
                } else {
                    with_app(|a| {
                        let hdc = wp as HDC;
                        SetTextColor(hdc, a.theme.fg);
                        SetBkColor(hdc, a.theme.field);
                        a.theme.br_field as LRESULT
                    }).unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
                }
            }
            WM_COMMAND => {
                let is_edit = HWND_EDIT.with(|h| h.get() == lp as HWND);
                let code = (wp >> 16) as u32;
                if is_edit {
                    if code == EN_CHANGE && with_app(|a| a.run_query(false)).is_none() {
                        PENDING_QUERY.with(|q| q.set(Some(false)));
                    }
                    0
                } else {
                    dispatch_command(loword(wp) as u16);
                    0
                }
            }
            WM_NOTIFY => with_app(|a| a.on_notify(lp)).flatten().unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp)),
            WM_CONTEXTMENU => {
                let is_list = HWND_LIST.with(|h| h.get() == wp as HWND);
                if is_list {
                    let mut pt = POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 };
                    if lp == -1 {
                        // Keyboard (Shift+F10 / menu key): anchor at the focused row.
                        with_app(|a| {
                            let i = SendMessageW(a.list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize);
                            let mut r = RECT { left: LVIR_LABEL as i32, top: 0, right: 0, bottom: 0 };
                            SendMessageW(a.list, LVM_GETITEMRECT, i.max(0) as usize, &mut r as *mut _ as isize);
                            pt = POINT { x: r.left, y: r.bottom };
                            ClientToScreen(a.list, &mut pt);
                        });
                    }
                    run_context_menu(pt, lp == -1);
                    0
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
            WM_TIMER if wp == TIMER_STATUS => {
                with_app(|a| {
                    a.ipc.send(Cmd::Status);
                    if let Some((_, time)) = &a.clip_msg {
                        if time.elapsed() >= std::time::Duration::from_secs(5) {
                            a.clip_msg = None;
                            a.invalidate_status();
                        }
                    }
                });
                0
            }
            WM_APP_IPC => {
                let ev = Box::from_raw(lp as *mut Event);
                with_app(|a| a.on_ipc(*ev));
                0
            }
            WM_APP_ICON => {
                let b = Box::from_raw(lp as *mut (String, i32));
                let (path, idx) = *b;
                with_app(|a| {
                    for (i, r) in a.rows.iter_mut() {
                        if r.full == path && idx >= 0 {
                            r.icon = idx;
                            SendMessageW(a.list, LVM_REDRAWITEMS, *i as usize, *i as isize);
                        }
                    }
                    a.icons.resolved(path, idx);
                });
                0
            }
            WM_APP_THUMB => {
                let d = Box::from_raw(lp as *mut Done);
                with_app(|a| a.on_thumb(*d));
                0
            }
            WM_APP_TRAY => {
                match (lp & 0xFFFF) as u32 {
                    WM_LBUTTONUP => { with_app(|a| a.toggle()); }
                    WM_RBUTTONUP | WM_CONTEXTMENU => run_tray_menu(),
                    _ => {}
                }
                0
            }
            WM_APP_SHOW => {
                with_app(|a| a.show());
                0
            }
            // A second nothing.exe launched by the folder verb hands its folder over.
            WM_COPYDATA => {
                let cds = &*(lp as *const windows_sys::Win32::System::DataExchange::COPYDATASTRUCT);
                if cds.dwData == COPYDATA_FOLDER && !cds.lpData.is_null() {
                    let units = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
                    let folder = String::from_utf16_lossy(units);
                    with_app(|a| a.open_folder(&folder));
                }
                1
            }
            WM_HOTKEY => {
                with_app(|a| a.toggle());
                0
            }
            WM_ACTIVATE => {
                with_app(|a| {
                    if loword(wp) == WA_INACTIVE {
                        let f = GetFocus();
                        if f == a.edit || f == a.list {
                            a.last_focus = f;
                        }
                    } else {
                        SetFocus(a.last_focus);
                    }
                });
                0
            }
            WM_CLOSE => {
                let exiting = with_app(|a| a.exiting).unwrap_or(false);
                if exiting {
                    DefWindowProcW(hwnd, msg, wp, lp)
                } else {
                    with_app(|a| a.hide()); // keep running in the tray: the next summon is instant
                    0
                }
            }
            WM_QUERYENDSESSION => 1,
            WM_ENDSESSION => {
                if wp != 0 {
                    with_app(|a| {
                        a.save_settings();
                        a.remove_tray();
                    });
                }
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            WM_GETMINMAXINFO => {
                let dpi = GetDpiForWindow(hwnd).max(96);
                let mm = &mut *(lp as *mut MINMAXINFO);
                mm.ptMinTrackSize = POINT { x: scale(420, dpi), y: scale(240, dpi) };
                0
            }
            WM_DPICHANGED => {
                let r = &*(lp as *const RECT);
                with_app(|a| a.apply_dpi(loword(wp)));
                SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                0
            }
            WM_SETTINGCHANGE => {
                if lp != 0 {
                    let ptr = lp as *const u16;
                    let mut len = 0;
                    while len < 64 && *ptr.add(len) != 0 {
                        len += 1;
                    }
                    if from_wide(std::slice::from_raw_parts(ptr, len)).starts_with("ImmersiveColorSet") {
                        with_app(|a| a.apply_theme());
                    }
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_NCHITTEST => {
                let r = DefWindowProcW(hwnd, msg, wp, lp);
                if r as u32 != HTCLIENT {
                    return r; // side / bottom borders
                }
                let mut pt = POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 };
                ScreenToClient(hwnd, &mut pt);
                with_app(|a| a.caption_hit(pt) as LRESULT).unwrap_or(r)
            }
            WM_NCMOUSEMOVE => {
                with_app(|a| a.set_caption_hot(wp as u32));
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            // Also fires as the menu's modal loop grabs the mouse: keep the menu button
            // lit while its popup is open (run_caption_menu clears it afterwards).
            WM_NCMOUSELEAVE => {
                with_app(|a| {
                    if a.cap_down != HTMENU {
                        a.cap_down = 0;
                        a.set_caption_hot(0);
                    }
                });
                0
            }
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if is_caption_button(wp as u32) => {
                let open_menu = with_app(|a| a.caption_press(wp as u32)).unwrap_or(false);
                if open_menu {
                    run_caption_menu();
                }
                0
            }
            WM_NCLBUTTONUP if is_caption_button(wp as u32) => {
                with_app(|a| a.caption_release(wp as u32));
                0
            }
            WM_NCACTIVATE => {
                with_app(|a| {
                    a.active = wp != 0;
                    a.invalidate_caption();
                });
                // lParam -1: don't repaint the (now nonexistent) non-client caption.
                DefWindowProcW(hwnd, msg, wp, -1)
            }
            // Alt / F10 with no menu bar: open the caption menu instead. Alt+Space
            // (lParam ' ') still reaches the system menu.
            WM_SYSCOMMAND if (wp & 0xFFF0) as u32 == SC_KEYMENU && lp == 0 => {
                run_caption_menu();
                0
            }
            m => {
                let taskbar = TASKBAR_CREATED.load(std::sync::atomic::Ordering::Relaxed);
                if taskbar != 0 && m == taskbar {
                    with_app(|a| a.add_tray()); // Explorer restarted
                    0
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
        }
    }
}

/// What the status bar shows for a single selection, unformatted: paint trims each field
/// to the room it has (see `fit::fields`), which depends on the window width.
struct Detail {
    idx: u32,
    full: String,
    name: String,
    folder: String,
    kind: String,
    size: String,
    bytes: u64,
    mtime: u64,
}

/// Size comes from the index; the type is a registry lookup by extension
/// (SHGFI_USEFILEATTRIBUTES: no disk access), the same name Explorer's Type column shows.
fn describe(idx: u32, r: &RowView) -> Detail {
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
    use windows_sys::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES};

    let mut kind = String::new();
    unsafe {
        let mut sfi: SHFILEINFOW = std::mem::zeroed();
        let attrs = if r.dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
        if SHGetFileInfoW(wide(&r.full).as_ptr(), attrs, &mut sfi, std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES) != 0 {
            kind = from_wide(&sfi.szTypeName);
        }
    }
    // The type field already says "MKV File", so ".mkv" on the name is the same fact
    // twice, in the field that's always short on room. Kept when there's no type to
    // carry it, for folders ("v1.2" is a name, not an extension) and for dotfiles
    // (".gitignore" would strip to nothing).
    let mut name = from_wide(&r.name);
    if !r.dir && !kind.is_empty() {
        if let Some(i) = name.rfind('.').filter(|&i| i > 0) {
            name.truncate(i);
        }
    }
    Detail {
        idx,
        full: r.full.clone(),
        name,
        folder: from_wide(&r.path),
        kind,
        size: if r.dir { String::new() } else { human_bytes(r.bytes) },
        bytes: r.bytes,
        mtime: r.mtime,
    }
}

/// Pixel width of `s` in the DC's current font.
fn text_width(hdc: HDC, s: &str) -> i32 {
    let w: Vec<u16> = s.encode_utf16().collect();
    let mut sz = SIZE { cx: 0, cy: 0 };
    unsafe { GetTextExtentPoint32W(hdc, w.as_ptr(), w.len() as i32, &mut sz) };
    sz.cx
}

/// Header, painted entirely by us. Custom draw can't do it: DarkMode_ItemsView fills
/// the item backgrounds and the empty area right of the last column with its own fixed
/// grey (#191919-ish) before any CDDS stage lets us in, so it never matches our palette.
fn paint_header(a: &App, hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        // Double-buffer: a column drag repaints on every mouse move.
        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, rc.right.max(1), rc.bottom.max(1));
        let old_bmp = SelectObject(mem, bmp as _);
        FillRect(mem, &rc, a.theme.br_bg);

        // Hover / pressed come from the live cursor: the themed header already
        // invalidates itself when the hot item changes, so this stays in sync.
        let mut ht: HDHITTESTINFO = std::mem::zeroed();
        GetCursorPos(&mut ht.pt);
        ScreenToClient(hwnd, &mut ht.pt);
        let hot = if PtInRect(&rc, ht.pt) != 0 { SendMessageW(hwnd, HDM_HITTEST, 0, &mut ht as *mut _ as isize) as i32 } else { -1 };

        let font = SendMessageW(hwnd, WM_GETFONT, 0, 0);
        let old_font = SelectObject(mem, font as _);
        SetBkMode(mem, TRANSPARENT as i32);
        SetTextColor(mem, a.theme.accent);
        let n = SendMessageW(hwnd, HDM_GETITEMCOUNT, 0, 0) as i32;
        let (pad, line) = (scale(8, a.dpi), scale(1, a.dpi).max(1));
        for i in 0..n {
            let mut r: RECT = std::mem::zeroed();
            SendMessageW(hwnd, HDM_GETITEMRECT, i as usize, &mut r as *mut _ as isize);
            if i == hot {
                FillRect(mem, &r, a.theme.br_hot);
            }
            let mut buf = [0u16; 64];
            let mut it: HDITEMW = std::mem::zeroed();
            it.mask = HDI_TEXT | HDI_FORMAT;
            it.pszText = buf.as_mut_ptr();
            it.cchTextMax = buf.len() as i32;
            SendMessageW(hwnd, HDM_GETITEMW, i as usize, &mut it as *mut _ as isize);
            let mut tr = RECT { left: r.left + pad, right: r.right - pad, ..r };
            let align = if it.fmt & HDF_RIGHT != 0 { DT_RIGHT } else { DT_LEFT };
            DrawTextW(mem, buf.as_mut_ptr(), -1, &mut tr, align | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX);
            // Sort chevron centred on top, where Explorer puts it.
            if it.fmt & (HDF_SORTUP | HDF_SORTDOWN) != 0 {
                let cx = (r.left + r.right) / 2;
                let (w, h) = (scale(4, a.dpi), scale(2, a.dpi).max(2));
                let (y0, y1) = if it.fmt & HDF_SORTUP != 0 { (r.top + h + scale(2, a.dpi), r.top + scale(2, a.dpi)) } else { (r.top + scale(2, a.dpi), r.top + h + scale(2, a.dpi)) };
                let pen = CreatePen(PS_SOLID, line, a.theme.dim);
                let old_pen = SelectObject(mem, pen as _);
                let pts = [POINT { x: cx - w, y: y0 }, POINT { x: cx, y: y1 }, POINT { x: cx + w, y: y0 }];
                Polyline(mem, pts.as_ptr(), 3);
                SelectObject(mem, old_pen);
                DeleteObject(pen as _);
            }
            // Column separator, inset like Explorer's.
            let inset = scale(4, a.dpi);
            let sep = RECT { left: r.right - line, right: r.right, top: r.top + inset, bottom: r.bottom - inset };
            FillRect(mem, &sep, a.theme.br_line);
        }
        let bottom = RECT { top: rc.bottom - line, ..rc };
        FillRect(mem, &bottom, a.theme.br_line);

        BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_font);
        SelectObject(mem, old_bmp);
        DeleteObject(bmp as _);
        DeleteDC(mem);
        EndPaint(hwnd, &ps);
    }
}

/// Header subclass: full custom paint in every theme. Light used to keep the stock themed
/// paint, but the visual style draws item text in its own colour (no CDRF hook changes it
/// reliably), so the orange column titles need our painter there too.
unsafe extern "system" fn header_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => return 1,
            WM_PAINT => {
                if with_app(|a| paint_header(a, hwnd)).is_some() {
                    return 0;
                }
            }
            // The stock header only repaints the hot item it tracked itself; with
            // our own paint a full invalidate on leave avoids a stuck hover.
            WM_MOUSELEAVE => {
                InvalidateRect(hwnd, null(), 0);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

/// ListView subclass: typing-to-search and right-click handling.
unsafe extern "system" fn list_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    unsafe {
        if msg == WM_RBUTTONDOWN {
            let pt = POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 };
            with_app(|_a| {
                let mut ht: LVHITTESTINFO = std::mem::zeroed();
                ht.pt = pt;
                let hit = SendMessageW(hwnd, LVM_HITTEST, 0, &mut ht as *mut _ as isize) as i32;
                if hit >= 0 {
                    let is_sel = SendMessageW(hwnd, LVM_GETITEMSTATE, hit as usize, LVIS_SELECTED as isize) != 0;
                    if !is_sel {
                        // Clear previous selection and select the right-clicked item
                        let mut desel: LVITEMW = std::mem::zeroed();
                        desel.stateMask = LVIS_SELECTED;
                        SendMessageW(hwnd, LVM_SETITEMSTATE, usize::MAX, &desel as *const _ as isize);

                        let mut sel: LVITEMW = std::mem::zeroed();
                        sel.state = LVIS_SELECTED | LVIS_FOCUSED;
                        sel.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
                        SendMessageW(hwnd, LVM_SETITEMSTATE, hit as usize, &sel as *const _ as isize);
                    }
                    SetFocus(hwnd);
                }
            });
        }
        if msg == WM_MOUSEMOVE || msg == WM_MOUSELEAVE {
            let mut suppress = false;
            with_app(|a| {
                let mut row = -1;
                if msg == WM_MOUSEMOVE && a.settings.view == VIEW_DETAILS {
                    let mut ht: LVHITTESTINFO = std::mem::zeroed();
                    ht.pt = POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 };
                    row = SendMessageW(hwnd, LVM_HITTEST, 0, &mut ht as *mut _ as isize) as i32;
                    let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    TrackMouseEvent(&mut t);
                }
                if row != a.hover_row {
                    // Repaint only the row losing and the row gaining the hover.
                    for r in [a.hover_row, row] {
                        if r >= 0 {
                            SendMessageW(hwnd, LVM_REDRAWITEMS, r as usize, r as isize);
                        }
                    }
                    a.hover_row = row;
                }
                // The theme's blue hot box comes from the list's private hot item, which
                // custom draw can't clear (CDIS_HOT is never even set for owner data). The
                // list only learns it from button-less mouse moves, so in Details those
                // stop here. Drag, marquee and click handling are unaffected: they run in
                // the list's own capture loop after WM_LBUTTONDOWN.
                let buttons = 0x1 | 0x2 | 0x10; // MK_LBUTTON | MK_RBUTTON | MK_MBUTTON
                if msg == WM_MOUSEMOVE && a.settings.view == VIEW_DETAILS && wp & buttons == 0 {
                    suppress = true;
                }
            });
            if suppress {
                return 0;
            }
        }
        if msg == WM_CHAR {
            let c = wp as u32;
            // Printable characters (and Backspace) typed into the list go to the search box.
            if (c >= 0x20 && c != 0x7F && GetKeyState(VK_CONTROL as i32) >= 0) || c == 0x08 {
                let handled = with_app(|a| {
                    SetFocus(a.edit);
                    SendMessageW(a.edit, EM_SETSEL, usize::MAX, -1);
                    SendMessageW(a.edit, WM_CHAR, wp, lp);
                }).is_some();
                if handled {
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
