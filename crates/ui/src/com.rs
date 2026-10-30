//! Hand-rolled COM vtables for the handful of shell interfaces we call. windows-sys has
//! no COM wrappers, and pulling the full `windows` crate in for five vtables would cost
//! more compile time and binary size than these ~80 lines.

use std::ffi::c_void;

use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::WindowsAndMessaging::HMENU;

pub const IID_ISHELLFOLDER: GUID = GUID::from_u128(0x000214E6_0000_0000_C000_000000000046);
pub const IID_ICONTEXTMENU: GUID = GUID::from_u128(0x000214E4_0000_0000_C000_000000000046);
pub const IID_ICONTEXTMENU2: GUID = GUID::from_u128(0x000214F4_0000_0000_C000_000000000046);
pub const IID_ICONTEXTMENU3: GUID = GUID::from_u128(0xBCFCE0A0_EC17_11D0_8D10_00A0C90F2719);
pub const IID_IDATAOBJECT: GUID = GUID::from_u128(0x0000010E_0000_0000_C000_000000000046);

#[repr(C)]
pub struct IUnknownVtbl {
    pub query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    pub add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    pub release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
pub struct IShellFolderVtbl {
    pub base: IUnknownVtbl,
    _parse_display_name: usize,
    _enum_objects: usize,
    _bind_to_object: usize,
    _bind_to_storage: usize,
    _compare_ids: usize,
    _create_view_object: usize,
    _get_attributes_of: usize,
    pub get_ui_object_of: unsafe extern "system" fn(
        *mut c_void,
        HWND,
        u32,
        *const *const ITEMIDLIST,
        *const GUID,
        *mut u32,
        *mut *mut c_void,
    ) -> HRESULT,
}

#[repr(C)]
pub struct IContextMenuVtbl {
    pub base: IUnknownVtbl,
    pub query_context_menu: unsafe extern "system" fn(*mut c_void, HMENU, u32, u32, u32, u32) -> HRESULT,
    pub invoke_command: unsafe extern "system" fn(*mut c_void, *const c_void) -> HRESULT,
    _get_command_string: usize,
    /// IContextMenu2 only.
    pub handle_menu_msg: unsafe extern "system" fn(*mut c_void, u32, WPARAM, LPARAM) -> HRESULT,
    /// IContextMenu3 only.
    pub handle_menu_msg2: unsafe extern "system" fn(*mut c_void, u32, WPARAM, LPARAM, *mut LRESULT) -> HRESULT,
}

/// Owned COM pointer, released on drop.
pub struct Com(pub *mut c_void);

impl Com {
    /// # Safety
    /// `self.0` must point to an object whose vtable starts with `T`'s layout.
    pub unsafe fn vtbl<T>(&self) -> &T {
        unsafe { &**(self.0 as *mut *const T) }
    }

    pub fn query(&self, iid: &GUID) -> Option<Com> {
        let mut out = std::ptr::null_mut();
        let hr = unsafe { (self.vtbl::<IUnknownVtbl>().query_interface)(self.0, iid, &mut out) };
        (hr >= 0 && !out.is_null()).then_some(Com(out))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (self.vtbl::<IUnknownVtbl>().release)(self.0) };
        }
    }
}
