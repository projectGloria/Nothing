//! Dev-only fallback: build an index by walking directories (no admin needed). Orders of
//! magnitude slower than the MFT reader; exists to test the engine/UI unelevated.

use nothing_core::{flag, Index, Volume, NONE};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
};

use crate::win::wide;

pub fn walk(root: &str) -> Index {
    let mut ix = Index::default();
    let root = root.trim_end_matches('\\');
    let mut parts = root.split('\\');
    let drive = parts.next().unwrap_or("C:");
    let mut cur = ix.push(0, NONE, drive, flag::DIR, 0, 0);
    ix.vols.push(Volume { letter: drive.as_bytes()[0].to_ascii_uppercase(), root: cur, ..Default::default() });
    for p in parts {
        cur = ix.push(0, cur, p, flag::DIR, 0, 0);
    }
    let mut stack = vec![(cur, root.to_string())];
    while let Some((dir, path)) = stack.pop() {
        let pat = wide(&format!("{path}\\*"));
        let mut fd: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        let h = unsafe { FindFirstFileExW(pat.as_ptr(), FindExInfoBasic, &mut fd as *mut _ as *mut _, FindExSearchNameMatch, std::ptr::null(), FIND_FIRST_EX_LARGE_FETCH) };
        if h == INVALID_HANDLE_VALUE {
            continue;
        }
        loop {
            let n = fd.cFileName.iter().position(|&c| c == 0).unwrap_or(260);
            let name = String::from_utf16_lossy(&fd.cFileName[..n]);
            if name != "." && name != ".." {
                let a = fd.dwFileAttributes;
                let mut fl = 0;
                if a & 0x10 != 0 {
                    fl |= flag::DIR;
                }
                if a & 2 != 0 {
                    fl |= flag::HIDDEN;
                }
                if a & 4 != 0 {
                    fl |= flag::SYSTEM;
                }
                let size = ((fd.nFileSizeHigh as u64) << 32) | fd.nFileSizeLow as u64;
                let mtime = ((fd.ftLastWriteTime.dwHighDateTime as u64) << 32) | fd.ftLastWriteTime.dwLowDateTime as u64;
                let id = ix.push(0, dir, &name, fl, if fl & flag::DIR != 0 { 0 } else { size }, mtime);
                // Don't follow junctions/symlinks: they create cycles and duplicates.
                if fl & flag::DIR != 0 && a & 0x400 == 0 {
                    stack.push((id, format!("{path}\\{name}")));
                }
            }
            if unsafe { FindNextFileW(h, &mut fd) } == 0 {
                break;
            }
        }
        unsafe { FindClose(h) };
    }
    ix.shrink();
    ix.rebuild_sorts();
    ix
}
