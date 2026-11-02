//! nothing_shell.dll: the "Nothing" entry in Windows 11's compact Explorer context menu.
//!
//! Windows 11 only lists `IExplorerCommand` handlers that come from a package in the top
//! menu; classic `shell\verb` keys go under "Show more options". The sparse package
//! (package/AppxManifest.xml) points Explorer at this DLL in the install folder; Explorer
//! loads it in-process and calls `Invoke`, which starts `nothing.exe --folder <dir>`, the
//! same command line the classic verbs use.
//!
//! COM by hand: windows-sys has no interface vtables, and a handful of `#[repr(C)]`
//! structs is cheaper than pulling in the `windows` crate for one class.

// Rust exports DllGetClassObject/DllCanUnloadNow without PRIVATE; link.exe notes it (LNK4104)
// but only import libraries care, and nobody links against this DLL.
#![allow(linker_messages)]

use std::cell::Cell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};

use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree};
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

type Ptr = *mut c_void;

const S_OK: HRESULT = 0;
const S_FALSE: HRESULT = 1;
const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
const CLASS_E_NOAGGREGATION: HRESULT = 0x8004_0110_u32 as i32;
const CLASS_E_CLASSNOTAVAILABLE: HRESULT = 0x8004_0111_u32 as i32;

/// Must match the Clsid attributes in package/AppxManifest.xml.
const CLSID_NOTHING: GUID = GUID::from_u128(0xb3d99d91_bb3e_4ba8_aeef_421b590d8957);
const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_ICLASSFACTORY: GUID = GUID::from_u128(0x00000001_0000_0000_c000_000000000046);
const IID_IEXPLORERCOMMAND: GUID = GUID::from_u128(0xa08ce4d0_fa25_44ab_b57c_c7b1c323e0b9);
const IID_IOBJECTWITHSITE: GUID = GUID::from_u128(0xfc4801a3_2ba9_11cf_a229_00aa003d7352);
const IID_ISERVICEPROVIDER: GUID = GUID::from_u128(0x6d5140c1_7436_11ce_8034_00aa006009fa);
/// Also SID_SFolderView: the folder view answers QueryService under its own IID.
const IID_IFOLDERVIEW: GUID = GUID::from_u128(0xcde725b0_ccc9_4519_917e_325d72fab4ce);
const IID_ISHELLITEM: GUID = GUID::from_u128(0x43826d1e_e718_42ee_bc55_a1e261c37bfe);

const SIGDN_FILESYSPATH: u32 = 0x8005_8000;
const ECS_ENABLED: u32 = 0;
const ECS_HIDDEN: u32 = 2;

/// Our HMODULE, to find nothing.exe next to us (the install folder).
static MODULE: AtomicIsize = AtomicIsize::new(0);
/// Live objects + LockServer count: Explorer polls DllCanUnloadNow to drop idle DLLs.
static OBJECTS: AtomicU32 = AtomicU32::new(0);
static LOCKS: AtomicU32 = AtomicU32::new(0);

fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

fn exe_path() -> PathBuf {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(MODULE.load(Ordering::Relaxed) as _, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let dll = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
    dll.with_file_name("nothing.exe")
}

/// Shell strings are returned in CoTaskMem the caller frees.
unsafe fn co_str(s: &str, out: *mut *mut u16) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let p = CoTaskMemAlloc(w.len() * 2) as *mut u16;
    *out = p;
    if p.is_null() {
        return E_OUTOFMEMORY;
    }
    std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
    S_OK
}

// ---- Foreign interfaces we call (only the slots we use are typed; the rest are padding).

#[repr(C)]
struct UnknownVtbl {
    query_interface: unsafe extern "system" fn(Ptr, *const GUID, *mut Ptr) -> HRESULT,
    add_ref: unsafe extern "system" fn(Ptr) -> u32,
    release: unsafe extern "system" fn(Ptr) -> u32,
}

#[repr(C)]
struct ShellItemArrayVtbl {
    unk: UnknownVtbl,
    _bind_to_handler: usize,
    _get_property_store: usize,
    _get_property_description_list: usize,
    _get_attributes: usize,
    get_count: unsafe extern "system" fn(Ptr, *mut u32) -> HRESULT,
    get_item_at: unsafe extern "system" fn(Ptr, u32, *mut Ptr) -> HRESULT,
}

#[repr(C)]
struct ShellItemVtbl {
    unk: UnknownVtbl,
    _bind_to_handler: usize,
    _get_parent: usize,
    get_display_name: unsafe extern "system" fn(Ptr, u32, *mut *mut u16) -> HRESULT,
}

#[repr(C)]
struct ServiceProviderVtbl {
    unk: UnknownVtbl,
    query_service: unsafe extern "system" fn(Ptr, *const GUID, *const GUID, *mut Ptr) -> HRESULT,
}

#[repr(C)]
struct FolderViewVtbl {
    unk: UnknownVtbl,
    _get_current_view_mode: usize,
    _set_current_view_mode: usize,
    get_folder: unsafe extern "system" fn(Ptr, *const GUID, *mut Ptr) -> HRESULT,
}

/// The vtable of a COM pointer, typed as `T`.
unsafe fn vt<'a, T>(p: Ptr) -> &'a T {
    &**(p as *const *const T)
}

unsafe fn release(p: Ptr) {
    if !p.is_null() {
        (vt::<UnknownVtbl>(p).release)(p);
    }
}

/// Filesystem path of an IShellItem; None for virtual items (This PC, Control Panel...).
unsafe fn item_path(item: Ptr) -> Option<String> {
    let mut s: *mut u16 = std::ptr::null_mut();
    if (vt::<ShellItemVtbl>(item).get_display_name)(item, SIGDN_FILESYSPATH, &mut s) < 0 || s.is_null() {
        return None;
    }
    let n = (0..).take_while(|&i| *s.add(i) != 0).count();
    let path = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
    CoTaskMemFree(s as *const c_void);
    Some(path)
}

/// First selected item. Explorer passes the clicked folder or drive here.
unsafe fn first_item_path(array: Ptr) -> Option<Option<String>> {
    if array.is_null() {
        return None;
    }
    let v = vt::<ShellItemArrayVtbl>(array);
    let mut count = 0;
    if (v.get_count)(array, &mut count) < 0 || count == 0 {
        return None;
    }
    let mut item: Ptr = std::ptr::null_mut();
    if (v.get_item_at)(array, 0, &mut item) < 0 {
        return None;
    }
    let path = item_path(item);
    release(item);
    Some(path)
}

/// Folder shown by the view hosting the menu. Background clicks may arrive with an empty
/// selection; the folder then comes from the site Explorer gave us (site -> folder view
/// -> its folder), which is what Windows Terminal's "Open in Terminal" does too.
unsafe fn site_folder_path(site: Ptr) -> Option<String> {
    if site.is_null() {
        return None;
    }
    let mut sp: Ptr = std::ptr::null_mut();
    if (vt::<UnknownVtbl>(site).query_interface)(site, &IID_ISERVICEPROVIDER, &mut sp) < 0 {
        return None;
    }
    let mut fv: Ptr = std::ptr::null_mut();
    let hr = (vt::<ServiceProviderVtbl>(sp).query_service)(sp, &IID_IFOLDERVIEW, &IID_IFOLDERVIEW, &mut fv);
    release(sp);
    if hr < 0 {
        return None;
    }
    let mut folder: Ptr = std::ptr::null_mut();
    let hr = (vt::<FolderViewVtbl>(fv).get_folder)(fv, &IID_ISHELLITEM, &mut folder);
    release(fv);
    if hr < 0 {
        return None;
    }
    let path = item_path(folder);
    release(folder);
    path
}

// ---- The command object: IExplorerCommand + IObjectWithSite.

#[repr(C)]
struct CommandVtbl {
    unk: UnknownVtbl,
    get_title: unsafe extern "system" fn(Ptr, Ptr, *mut *mut u16) -> HRESULT,
    get_icon: unsafe extern "system" fn(Ptr, Ptr, *mut *mut u16) -> HRESULT,
    get_tool_tip: unsafe extern "system" fn(Ptr, Ptr, *mut *mut u16) -> HRESULT,
    get_canonical_name: unsafe extern "system" fn(Ptr, *mut GUID) -> HRESULT,
    get_state: unsafe extern "system" fn(Ptr, Ptr, i32, *mut u32) -> HRESULT,
    invoke: unsafe extern "system" fn(Ptr, Ptr, Ptr) -> HRESULT,
    get_flags: unsafe extern "system" fn(Ptr, *mut u32) -> HRESULT,
    enum_sub_commands: unsafe extern "system" fn(Ptr, *mut Ptr) -> HRESULT,
}

#[repr(C)]
struct SiteVtbl {
    unk: UnknownVtbl,
    set_site: unsafe extern "system" fn(Ptr, Ptr) -> HRESULT,
    get_site: unsafe extern "system" fn(Ptr, *const GUID, *mut Ptr) -> HRESULT,
}

/// Two interfaces = two vtable pointers at the start; a COM pointer to IObjectWithSite is
/// the address of the second field, so its methods step back one pointer to find `Command`.
#[repr(C)]
struct Command {
    command: &'static CommandVtbl,
    site_iface: &'static SiteVtbl,
    refs: AtomicU32,
    site: Cell<Ptr>,
}

static COMMAND_VTBL: CommandVtbl = CommandVtbl {
    unk: UnknownVtbl { query_interface: cmd_qi, add_ref: cmd_add_ref, release: cmd_release },
    get_title,
    get_icon,
    get_tool_tip,
    get_canonical_name,
    get_state,
    invoke,
    get_flags,
    enum_sub_commands,
};

static SITE_VTBL: SiteVtbl = SiteVtbl {
    unk: UnknownVtbl { query_interface: site_qi, add_ref: site_add_ref, release: site_release },
    set_site,
    get_site,
};

unsafe fn cmd<'a>(this: Ptr) -> &'a Command {
    &*(this as *const Command)
}

unsafe fn from_site(this: Ptr) -> Ptr {
    (this as *mut Ptr).sub(1) as Ptr
}

unsafe extern "system" fn cmd_qi(this: Ptr, riid: *const GUID, ppv: *mut Ptr) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    let c = cmd(this);
    let riid = &*riid;
    *ppv = if guid_eq(riid, &IID_IUNKNOWN) || guid_eq(riid, &IID_IEXPLORERCOMMAND) {
        this
    } else if guid_eq(riid, &IID_IOBJECTWITHSITE) {
        &c.site_iface as *const _ as Ptr
    } else {
        std::ptr::null_mut()
    };
    if (*ppv).is_null() {
        return E_NOINTERFACE;
    }
    c.refs.fetch_add(1, Ordering::Relaxed);
    S_OK
}

unsafe extern "system" fn cmd_add_ref(this: Ptr) -> u32 {
    cmd(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn cmd_release(this: Ptr) -> u32 {
    let left = cmd(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        let b = Box::from_raw(this as *mut Command);
        release(b.site.get());
        drop(b);
        OBJECTS.fetch_sub(1, Ordering::Relaxed);
    }
    left
}

unsafe extern "system" fn site_qi(this: Ptr, riid: *const GUID, ppv: *mut Ptr) -> HRESULT {
    cmd_qi(from_site(this), riid, ppv)
}
unsafe extern "system" fn site_add_ref(this: Ptr) -> u32 {
    cmd_add_ref(from_site(this))
}
unsafe extern "system" fn site_release(this: Ptr) -> u32 {
    cmd_release(from_site(this))
}

unsafe extern "system" fn set_site(this: Ptr, site: Ptr) -> HRESULT {
    let c = cmd(from_site(this));
    if !site.is_null() {
        (vt::<UnknownVtbl>(site).add_ref)(site);
    }
    release(c.site.replace(site));
    S_OK
}

unsafe extern "system" fn get_site(this: Ptr, riid: *const GUID, ppv: *mut Ptr) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    let site = cmd(from_site(this)).site.get();
    if site.is_null() {
        return E_FAIL;
    }
    (vt::<UnknownVtbl>(site).query_interface)(site, riid, ppv)
}

unsafe extern "system" fn get_title(_: Ptr, _: Ptr, out: *mut *mut u16) -> HRESULT {
    co_str("Nothing", out)
}

unsafe extern "system" fn get_icon(_: Ptr, _: Ptr, out: *mut *mut u16) -> HRESULT {
    // "file,index": the app icon resource of nothing.exe, same as the classic verb.
    co_str(&format!("{},0", exe_path().display()), out)
}

unsafe extern "system" fn get_tool_tip(_: Ptr, _: Ptr, out: *mut *mut u16) -> HRESULT {
    if !out.is_null() {
        *out = std::ptr::null_mut();
    }
    E_NOTIMPL
}

unsafe extern "system" fn get_canonical_name(_: Ptr, out: *mut GUID) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = CLSID_NOTHING;
    S_OK
}

unsafe extern "system" fn get_state(_: Ptr, array: Ptr, _ok_to_be_slow: i32, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    // Hide on virtual folders (This PC, Recycle Bin...): there's no path to search.
    *out = match first_item_path(array) {
        Some(None) => ECS_HIDDEN,
        _ => ECS_ENABLED,
    };
    S_OK
}

unsafe extern "system" fn invoke(this: Ptr, array: Ptr, _bind_ctx: Ptr) -> HRESULT {
    let path = match first_item_path(array) {
        Some(p) => p,
        None => site_folder_path(cmd(this).site.get()),
    };
    let Some(path) = path else { return E_FAIL };
    // Explorer owns the foreground (the user just clicked); pass that right on, or
    // nothing.exe (and the running instance it hands off to) only flashes in the taskbar.
    AllowSetForegroundWindow(ASFW_ANY);
    // std quotes the argument by MSVC rules, so "C:\" arrives intact (no `\"` escape).
    match std::process::Command::new(exe_path()).arg("--folder").arg(&path).spawn() {
        Ok(_) => S_OK,
        Err(_) => E_FAIL,
    }
}

unsafe extern "system" fn get_flags(_: Ptr, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = 0; // ECF_DEFAULT: a plain verb, no submenu
    S_OK
}

unsafe extern "system" fn enum_sub_commands(_: Ptr, out: *mut Ptr) -> HRESULT {
    if !out.is_null() {
        *out = std::ptr::null_mut();
    }
    E_NOTIMPL
}

// ---- Class factory: a static singleton, so AddRef/Release are no-ops.

#[repr(C)]
struct FactoryVtbl {
    unk: UnknownVtbl,
    create_instance: unsafe extern "system" fn(Ptr, Ptr, *const GUID, *mut Ptr) -> HRESULT,
    lock_server: unsafe extern "system" fn(Ptr, i32) -> HRESULT,
}

#[repr(C)]
struct Factory(&'static FactoryVtbl);

static FACTORY: Factory = Factory(&FactoryVtbl {
    unk: UnknownVtbl { query_interface: factory_qi, add_ref: factory_add_ref, release: factory_release },
    create_instance,
    lock_server,
});

unsafe extern "system" fn factory_qi(this: Ptr, riid: *const GUID, ppv: *mut Ptr) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    let riid = &*riid;
    if guid_eq(riid, &IID_IUNKNOWN) || guid_eq(riid, &IID_ICLASSFACTORY) {
        *ppv = this;
        S_OK
    } else {
        *ppv = std::ptr::null_mut();
        E_NOINTERFACE
    }
}
unsafe extern "system" fn factory_add_ref(_: Ptr) -> u32 {
    2
}
unsafe extern "system" fn factory_release(_: Ptr) -> u32 {
    1
}

unsafe extern "system" fn create_instance(_: Ptr, outer: Ptr, riid: *const GUID, ppv: *mut Ptr) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    let obj = Box::into_raw(Box::new(Command {
        command: &COMMAND_VTBL,
        site_iface: &SITE_VTBL,
        refs: AtomicU32::new(1),
        site: Cell::new(std::ptr::null_mut()),
    })) as Ptr;
    OBJECTS.fetch_add(1, Ordering::Relaxed);
    // QI adds the caller's reference; dropping ours frees the object if QI failed.
    let hr = cmd_qi(obj, riid, ppv);
    cmd_release(obj);
    hr
}

unsafe extern "system" fn lock_server(_: Ptr, lock: i32) -> HRESULT {
    if lock != 0 {
        LOCKS.fetch_add(1, Ordering::Relaxed);
    } else {
        LOCKS.fetch_sub(1, Ordering::Relaxed);
    }
    S_OK
}

// ---- DLL exports.

#[no_mangle]
unsafe extern "system" fn DllMain(module: Ptr, reason: u32, _: Ptr) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(module as isize, Ordering::Relaxed);
    }
    1
}

#[no_mangle]
unsafe extern "system" fn DllGetClassObject(rclsid: *const GUID, riid: *const GUID, ppv: *mut Ptr) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    if rclsid.is_null() || !guid_eq(&*rclsid, &CLSID_NOTHING) {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    factory_qi(&FACTORY as *const Factory as Ptr, riid, ppv)
}

#[no_mangle]
unsafe extern "system" fn DllCanUnloadNow() -> HRESULT {
    if OBJECTS.load(Ordering::Relaxed) == 0 && LOCKS.load(Ordering::Relaxed) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}
