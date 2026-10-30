//! Named-pipe server: one thread per connected UI, strict request/response.

use std::fs::File;
use std::io::Write;
use std::os::windows::io::FromRawHandle;
use std::sync::Arc;
use std::time::Instant;

use nothing_core::proto::{self, Request, Row, W};
use nothing_core::search::{refine, search, Query};
use nothing_core::{Index, Sort};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use crate::engine::Engine;
use crate::log;
use crate::win::{last_error, wide};

/// SYSTEM and Administrators: full. Interactive users: read + write data, but NOT
/// FILE_CREATE_PIPE_INSTANCE (0x4), so an unprivileged process can't squat the pipe name.
const SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019b;;;IU)";

pub fn serve(engine: Arc<Engine>) {
    let mut psd = std::ptr::null_mut();
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(SDDL).as_ptr(), SDDL_REVISION_1, &mut psd, std::ptr::null_mut()) };
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: psd, bInheritHandle: 0 };
    let name = wide(proto::PIPE_NAME);
    let mut first = true;
    loop {
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 },
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                1 << 16,
                1 << 16,
                0,
                &sa,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            log!("CreateNamedPipe failed: {} (another instance running?)", last_error());
            if first {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        first = false;
        let ok = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0 || last_error() == ERROR_PIPE_CONNECTED;
        if !ok {
            unsafe { CloseHandle(h) };
            continue;
        }
        let e = engine.clone();
        // SAFETY: we own `h`; File closes it when the client thread ends.
        let pipe = unsafe { File::from_raw_handle(h as _) };
        std::thread::spawn(move || client(e, pipe));
    }
}

/// What a client last searched, kept so scrolling and refining are cheap.
struct Session {
    ids: Vec<u32>,
    epoch: u64,
    version: u64,
    sort: Sort,
    desc: bool,
    query: Option<Query>,
}

fn client(engine: Arc<Engine>, mut pipe: File) {
    let mut s = Session { ids: Vec::new(), epoch: u64::MAX, version: u64::MAX, sort: Sort::Name, desc: false, query: None };
    let mut buf = Vec::new();
    let mut path_cache: (u32, String) = (u32::MAX, String::new());
    while proto::read_frame(&mut pipe, &mut buf).is_ok() {
        let Some(req) = proto::parse_request(&buf) else { break };
        let mut m = W::new();
        match req {
            Request::Query { gen, sort, desc, opts, text } => {
                let t0 = Instant::now();
                let q = Query::parse(&text, opts);
                let sort = Sort::from_u8(sort);
                if sort == Sort::Path && engine.db.read().unwrap().path_dirty {
                    engine.db.write().unwrap().ensure_path_sort();
                }
                let ix = engine.db.read().unwrap();
                let can_refine = s.epoch == ix.epoch
                    && s.version == ix.version
                    && s.sort == sort
                    && s.desc == desc
                    && s.query.as_ref().is_some_and(|p| q.narrows(p));
                s.ids = if can_refine { refine(&ix, &q, &s.ids) } else { search(&ix, &q, sort, desc) };
                s.epoch = ix.epoch;
                s.version = ix.version;
                s.sort = sort;
                s.desc = desc;
                s.query = Some(q);
                path_cache.0 = u32::MAX;
                m.u32(gen).u32(s.ids.len() as u32).u32(t0.elapsed().as_micros() as u32);
            }
            Request::Rows { gen, first, count } => {
                let ix = engine.db.read().unwrap();
                m.u32(gen).u32(first);
                if ix.epoch != s.epoch {
                    m.u32(0); // ids were renumbered; the UI re-queries on the version bump
                } else {
                    let a = (first as usize).min(s.ids.len());
                    let b = (a + count.min(2000) as usize).min(s.ids.len());
                    m.u32((b - a) as u32);
                    for &id in &s.ids[a..b] {
                        proto::put_row(&mut m, &row(&ix, id, &mut path_cache));
                    }
                }
            }
            Request::Status => proto::put_status(&mut m, &engine.status()),
        }
        if pipe.write_all(m.finish()).is_err() {
            break;
        }
    }
}

fn row(ix: &Index, id: u32, cache: &mut (u32, String)) -> Row {
    let p = ix.parent[id as usize];
    // Neighbouring rows usually share a folder (name/path sort): skip re-walking it.
    if cache.0 != p {
        *cache = (p, ix.dir_path(id));
    }
    Row { flags: ix.flags[id as usize], size: ix.size[id as usize], mtime: ix.mtime[id as usize], name: ix.name(id).to_string(), path: cache.1.clone() }
}
