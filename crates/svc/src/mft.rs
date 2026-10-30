//! Raw NTFS Master File Table reader — this is where Everything-class speed comes from.
//!
//! Instead of asking the filesystem about 1.4M files one directory at a time (millions of
//! syscalls, random I/O), we read the MFT itself: one sequential stream of 1 KB records,
//! each describing a file (name, parent, size, timestamps). Reads run unbuffered in 4 MB
//! chunks on two reader threads; every other core parses records as chunks arrive.

use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use nothing_core::{flag, Index, Volume, NONE};
use windows_sys::Win32::Foundation::{GENERIC_READ, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_GET_RETRIEVAL_POINTERS, NTFS_VOLUME_DATA_BUFFER};

use crate::usn;
use crate::win::{ioctl, pod, wide, Handle};

const MASK48: u64 = 0xFFFF_FFFF_FFFF;
const ROOT_REC: u64 = 5;
/// Records 0..16 are NTFS metadata ($MFT, $LogFile, $Bitmap, $Extend...), never shown.
const FIRST_USER_REC: u64 = 16;

const A_IN_USE: u32 = 0x8000_0000;
const A_DIR: u32 = 0x4000_0000;
const FILE_ATTRIBUTE_HIDDEN: u32 = 2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 4;

const CHUNK: usize = 4 << 20;

pub fn open_volume(letter: u8, overlapped: bool) -> io::Result<Handle> {
    let path = wide(&format!(r"\\.\{}:", letter as char));
    let flags = if overlapped { FILE_FLAG_OVERLAPPED } else { FILE_FLAG_NO_BUFFERING };
    let h = unsafe {
        CreateFileW(path.as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, flags, std::ptr::null_mut())
    };
    Handle::checked(h)
}

// ---- pure record parsing (unit-tested) -------------------------------------------------

#[inline]
fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
#[inline]
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Undo NTFS "update sequence" protection: the last 2 bytes of every stride were swapped
/// out on write (torn-write detection) and must be restored from the array in the header.
pub fn fixup(r: &mut [u8]) -> bool {
    let usa_off = u16_at(r, 4) as usize;
    let usa_cnt = u16_at(r, 6) as usize;
    if usa_cnt < 2 || usa_off + usa_cnt * 2 > r.len() {
        return false;
    }
    let stride = r.len() / (usa_cnt - 1);
    if stride < 256 {
        return false;
    }
    let check = [r[usa_off], r[usa_off + 1]];
    for i in 1..usa_cnt {
        let end = i * stride;
        if end > r.len() || r[end - 2..end] != check {
            return false; // torn / corrupt record
        }
        r[end - 2] = r[usa_off + 2 * i];
        r[end - 1] = r[usa_off + 2 * i + 1];
    }
    true
}

pub enum Ev<'a> {
    /// Base record header: it exists and is in use.
    Header { rec: u64, dir: bool },
    StdInfo { base: u64, mtime: u64, attrs: u32 },
    /// `name` is raw UTF-16LE bytes.
    Name { base: u64, parent: u64, ns: u8, name: &'a [u8] },
    Data { base: u64, size: u64 },
}

/// Parse one MFT record (fixups applied in place). Extension records report attributes
/// against their base record, which is how big files/hard links spill across records.
pub fn parse_record(r: &mut [u8], rec_no: u64, f: &mut impl FnMut(Ev)) -> bool {
    if r.len() < 64 || &r[0..4] != b"FILE" {
        return false;
    }
    let hflags = u16_at(r, 22);
    if hflags & 1 == 0 || !fixup(r) {
        return false;
    }
    let base_ref = u64_at(r, 32) & MASK48;
    let base = if base_ref == 0 { rec_no } else { base_ref };
    if base_ref == 0 {
        f(Ev::Header { rec: rec_no, dir: hflags & 2 != 0 });
    }
    let used = (u32_at(r, 24) as usize).min(r.len());
    let mut off = u16_at(r, 20) as usize;
    while off + 16 <= used {
        let ty = u32_at(r, off);
        if ty == 0xFFFF_FFFF {
            break;
        }
        let len = u32_at(r, off + 4) as usize;
        if len < 16 || off + len > used {
            break;
        }
        let a = &r[off..off + len];
        let nonres = a[8] != 0;
        let attr_name_len = a[9];
        let resident_value = || -> Option<&[u8]> {
            if nonres || len < 24 {
                return None;
            }
            let (vl, vo) = (u32_at(a, 16) as usize, u16_at(a, 20) as usize);
            a.get(vo..vo.checked_add(vl)?)
        };
        match ty {
            0x10 => {
                if let Some(v) = resident_value().filter(|v| v.len() >= 36) {
                    f(Ev::StdInfo { base, mtime: u64_at(v, 8), attrs: u32_at(v, 32) });
                }
            }
            0x30 => {
                if let Some(v) = resident_value().filter(|v| v.len() >= 66) {
                    let n = v[64] as usize;
                    if let Some(name) = v.get(66..66 + 2 * n) {
                        f(Ev::Name { base, parent: u64_at(v, 0) & MASK48, ns: v[65], name });
                    }
                }
            }
            0x80 if attr_name_len == 0 => {
                if nonres {
                    // Only the first extent (starting VCN 0) carries the real size.
                    if len >= 56 && u64_at(a, 16) == 0 {
                        f(Ev::Data { base, size: u64_at(a, 48) });
                    }
                } else if len >= 24 {
                    f(Ev::Data { base, size: u32_at(a, 16) as u64 });
                }
            }
            _ => {}
        }
        off += len;
    }
    true
}

/// Decode an NTFS mapping-pairs runlist into (first LCN, cluster count). Sparse runs come
/// back with LCN = u64::MAX so VCN positions stay correct.
pub fn decode_runs(mut p: &[u8]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut lcn: i64 = 0;
    while let Some(&h) = p.first() {
        if h == 0 {
            break;
        }
        let (ls, os) = ((h & 0xF) as usize, (h >> 4) as usize);
        if ls == 0 || ls > 8 || os > 8 || 1 + ls + os > p.len() {
            break;
        }
        let mut len = 0u64;
        for i in 0..ls {
            len |= (p[1 + i] as u64) << (8 * i);
        }
        if os == 0 {
            out.push((u64::MAX, len));
        } else {
            let mut d = 0i64;
            for i in 0..os {
                d |= (p[1 + ls + i] as i64) << (8 * i);
            }
            let shift = 64 - 8 * os as u32;
            d = (d << shift) >> shift; // sign-extend
            lcn += d;
            out.push((lcn as u64, len));
        }
        p = &p[1 + ls + os..];
    }
    out
}

// ---- I/O ---------------------------------------------------------------------------------

/// 4 KB-aligned heap buffer, required for FILE_FLAG_NO_BUFFERING reads.
struct ABuf {
    ptr: *mut u8,
    len: usize,
}
unsafe impl Send for ABuf {}
impl ABuf {
    fn new(len: usize) -> ABuf {
        let layout = std::alloc::Layout::from_size_align(len, 4096).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };
        assert!(!ptr.is_null());
        ABuf { ptr, len }
    }
    fn slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}
impl Drop for ABuf {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, std::alloc::Layout::from_size_align(self.len, 4096).unwrap()) }
    }
}

/// Positional read on a synchronous handle (OVERLAPPED only carries the offset).
fn read_at(h: HANDLE, off: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.Anonymous.Anonymous.Offset = off as u32;
    ov.Anonymous.Anonymous.OffsetHigh = (off >> 32) as u32;
    let mut got = 0u32;
    if unsafe { ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut got, &mut ov) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(got as usize)
}

/// MFT extents as (byte offset on disk, byte length), in VCN order.
fn mft_extents(letter: u8, vol: HANDLE, vd: &NTFS_VOLUME_DATA_BUFFER) -> io::Result<Vec<(u64, u64)>> {
    let cluster = vd.BytesPerCluster as u64;
    // Preferred: ask NTFS for $MFT's retrieval pointers (complete even if the MFT is so
    // fragmented that its own runlist spills into extension records).
    let path = wide(&format!(r"{}:\$MFT", letter as char));
    let h = unsafe {
        CreateFileW(path.as_ptr(), FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut())
    };
    if let Ok(h) = Handle::checked(h) {
        let mut out = vec![0u8; 1 << 20];
        let start: i64 = 0;
        if let Ok(n) = ioctl(h.0, FSCTL_GET_RETRIEVAL_POINTERS, Some(&start), &mut out) {
            if n >= 16 {
                let count = u32_at(&out, 0) as usize;
                let mut vcn = u64_at(&out, 8);
                let mut ext = Vec::with_capacity(count);
                for i in 0..count {
                    let o = 16 + i * 16;
                    if o + 16 > n {
                        break;
                    }
                    let (next, lcn) = (u64_at(&out, o), u64_at(&out, o + 8));
                    if lcn != u64::MAX {
                        ext.push((lcn * cluster, (next - vcn) * cluster));
                    }
                    vcn = next;
                }
                if !ext.is_empty() {
                    return Ok(ext);
                }
            }
        }
    }
    // Fallback: decode the runlist of $MFT's own $DATA attribute from record 0.
    let rs = vd.BytesPerFileRecordSegment as usize;
    let mut buf = ABuf::new(rs.max(cluster as usize).max(4096));
    read_at(vol, vd.MftStartLcn as u64 * cluster, buf.slice())?;
    let r = &mut buf.slice()[..rs];
    if &r[0..4] != b"FILE" || !fixup(r) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "MFT record 0 unreadable"));
    }
    let mut off = u16_at(r, 20) as usize;
    while off + 16 <= rs {
        let (ty, len) = (u32_at(r, off), u32_at(r, off + 4) as usize);
        if ty == 0xFFFF_FFFF || len < 16 || off + len > rs {
            break;
        }
        if ty == 0x80 && r[off + 8] != 0 && r[off + 9] == 0 {
            let a = &r[off..off + len];
            let runs = decode_runs(&a[u16_at(a, 32) as usize..]);
            return Ok(runs.into_iter().filter(|r| r.0 != u64::MAX).map(|(l, n)| (l * cluster, n * cluster)).collect());
        }
        off += len;
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "MFT $DATA not found"))
}

pub struct ScanStats {
    pub records: u64,
    pub mft_bytes: u64,
    pub millis: u128,
}

/// Scan one NTFS volume into a standalone single-volume `Index` (vol 0, root "C:").
/// `progress` receives 0..=100.
pub fn scan(letter: u8, serial: u32, progress: &(dyn Fn(u8) + Sync)) -> io::Result<(Index, ScanStats)> {
    let t0 = Instant::now();
    let vol = open_volume(letter, false)?;
    // Journal position is captured *before* reading the MFT: anything that changes while
    // we scan is replayed afterwards, so nothing slips between scan and live updates.
    let journal = usn::query_or_create(vol.0)?;

    let mut vdb = [0u8; 128];
    ioctl::<()>(vol.0, FSCTL_GET_NTFS_VOLUME_DATA, None, &mut vdb)?;
    let vd: NTFS_VOLUME_DATA_BUFFER = pod(&vdb);
    let rs = vd.BytesPerFileRecordSegment as usize;
    let total_recs = (vd.MftValidDataLength as u64 / rs as u64) as usize;
    let extents = mft_extents(letter, vol.0, &vd)?;

    // Plan chunk reads: (disk offset, first record number, byte length).
    let mut plan = Vec::new();
    let mut rec = 0u64;
    'outer: for (off, len) in extents {
        let mut done = 0u64;
        while done < len {
            if rec as usize >= total_recs {
                break 'outer;
            }
            let n = (len - done).min(CHUNK as u64).min((total_recs as u64 - rec) * rs as u64);
            // Unbuffered reads need sector-multiple lengths; record size is one.
            plan.push((off + done, rec, n as usize));
            rec += n / rs as u64;
            done += n;
        }
    }

    let size: Vec<AtomicU64> = (0..total_recs).map(|_| AtomicU64::new(0)).collect();
    let mtime: Vec<AtomicU64> = (0..total_recs).map(|_| AtomicU64::new(0)).collect();
    let attr: Vec<AtomicU32> = (0..total_recs).map(|_| AtomicU32::new(0)).collect();
    let parsed_recs = AtomicU64::new(0);

    struct NameRec {
        rec: u32,
        ns: u8,
        parent: u64,
        off: u32,
        len: u16,
    }
    struct Part {
        names: Vec<NameRec>,
        bytes: Vec<u8>,
    }

    let readers = 2usize;
    let workers = nothing_core::index::threads().saturating_sub(1).max(1);
    let (tx, rx) = sync_channel::<(u64, usize, ABuf)>(workers * 2);
    let rx = Arc::new(Mutex::new(rx));
    let pool: Mutex<Vec<ABuf>> = Mutex::new(Vec::new());
    let plan = &plan;
    let read_err: Mutex<Option<io::Error>> = Mutex::new(None);

    let parts: Vec<Part> = std::thread::scope(|s| {
        for r in 0..readers {
            let tx = tx.clone();
            let (vol, pool, read_err) = (&vol, &pool, &read_err);
            s.spawn(move || {
                for (i, &(off, first, len)) in plan.iter().enumerate() {
                    if i % readers != r {
                        continue;
                    }
                    let mut b = pool.lock().unwrap().pop().unwrap_or_else(|| ABuf::new(CHUNK));
                    match read_at(vol.0, off, &mut b.slice()[..len]) {
                        Ok(n) => {
                            if tx.send((first, n - n % rs, b)).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            *read_err.lock().unwrap() = Some(e);
                            return;
                        }
                    }
                }
            });
        }
        drop(tx);
        let hs: Vec<_> = (0..workers)
            .map(|_| {
                let rx = rx.clone();
                let (size, mtime, attr, pool, parsed_recs) = (&size, &mtime, &attr, &pool, &parsed_recs);
                s.spawn(move || {
                    let mut part = Part { names: Vec::new(), bytes: Vec::new() };
                    loop {
                        let msg = rx.lock().unwrap().recv();
                        let Ok((first, len, mut b)) = msg else { break };
                        let buf = &mut b.slice()[..len];
                        for (i, r) in buf.chunks_exact_mut(rs).enumerate() {
                            let rec_no = first + i as u64;
                            parse_record(r, rec_no, &mut |ev| match ev {
                                Ev::Header { rec, dir } => {
                                    if let Some(a) = attr.get(rec as usize) {
                                        a.fetch_or(A_IN_USE | if dir { A_DIR } else { 0 }, Relaxed);
                                    }
                                }
                                Ev::StdInfo { base, mtime: t, attrs } => {
                                    if let (Some(m), Some(a)) = (mtime.get(base as usize), attr.get(base as usize)) {
                                        m.store(t, Relaxed);
                                        a.fetch_or(attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM), Relaxed);
                                    }
                                }
                                Ev::Data { base, size: sz } => {
                                    if let Some(x) = size.get(base as usize) {
                                        x.store(sz, Relaxed);
                                    }
                                }
                                Ev::Name { base, parent, ns, name } => {
                                    // 8.3 short names (namespace 2) duplicate the real name.
                                    if ns == 2 || base < FIRST_USER_REC || base as usize >= total_recs {
                                        return;
                                    }
                                    let off = part.bytes.len();
                                    let units = name.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]));
                                    let mut tmp = [0u8; 4];
                                    for ch in char::decode_utf16(units) {
                                        let ch = ch.unwrap_or('\u{FFFD}');
                                        part.bytes.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                                    }
                                    part.names.push(NameRec { rec: base as u32, ns, parent, off: off as u32, len: (part.bytes.len() - off) as u16 });
                                }
                            });
                        }
                        let done = parsed_recs.fetch_add((len / rs) as u64, Relaxed) + (len / rs) as u64;
                        progress((done * 100 / total_recs.max(1) as u64).min(99) as u8);
                        pool.lock().unwrap().push(b);
                    }
                    part
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    if let Some(e) = read_err.into_inner().unwrap() {
        return Err(e);
    }

    // ---- assemble ----
    let mut ix = Index::default();
    let root_name = format!("{}:", letter as char);
    let root = ix.push(0, NONE, &root_name, flag::DIR, 0, mtime.get(ROOT_REC as usize).map(|m| m.load(Relaxed)).unwrap_or(0));
    let mut v = Volume { letter, serial, journal_id: journal.id, next_usn: journal.next, root, frn: vec![NONE; total_recs], ..Default::default() };
    v.set(ROOT_REC, root);

    let total_names: usize = parts.iter().map(|p| p.names.len()).sum();
    let mut parent_rec: Vec<u64> = Vec::with_capacity(total_names + 1);
    parent_rec.push(u64::MAX);
    for p in &parts {
        for nr in &p.names {
            let rec = nr.rec as usize;
            let a = attr[rec].load(Relaxed);
            if a & A_IN_USE == 0 {
                continue;
            }
            let _ = nr.ns;
            let name = std::str::from_utf8(&p.bytes[nr.off as usize..nr.off as usize + nr.len as usize]).unwrap_or("?");
            let mut fl = 0u8;
            if a & A_DIR != 0 {
                fl |= flag::DIR;
            }
            if a & FILE_ATTRIBUTE_HIDDEN != 0 {
                fl |= flag::HIDDEN;
            }
            if a & FILE_ATTRIBUTE_SYSTEM != 0 {
                fl |= flag::SYSTEM;
            }
            let sz = if fl & flag::DIR != 0 { 0 } else { size[rec].load(Relaxed) };
            let id = ix.push(0, NONE, name, fl, sz, mtime[rec].load(Relaxed));
            parent_rec.push(nr.parent);
            if v.frn[rec] == NONE {
                v.frn[rec] = id;
            } else {
                v.links.entry(rec as u64).or_default().push(id);
            }
        }
    }
    drop(parts);
    for id in 1..ix.len() {
        ix.parent[id] = v.get(parent_rec[id]);
    }
    drop(parent_rec);

    // Drop anything not reachable from the root: children of skipped metadata folders
    // ($Extend\...), or orphans from records caught mid-update.
    let n = ix.len();
    let mut state = vec![0u8; n]; // 0 unknown, 1 reachable, 2 orphan
    state[root as usize] = 1;
    let mut chain = Vec::new();
    for id in 0..n {
        chain.clear();
        let mut cur = id as u32;
        let verdict = loop {
            if cur == NONE || chain.len() > 4096 {
                break 2;
            }
            match state[cur as usize] {
                0 => {
                    chain.push(cur);
                    // Self-parented non-root records would loop forever without this.
                    if state[cur as usize] == 0 && ix.parent[cur as usize] == cur {
                        break 2;
                    }
                    state[cur as usize] = 3; // in progress (cycle guard)
                    cur = ix.parent[cur as usize];
                }
                3 => break 2,
                s => break s,
            }
        };
        for &c in &chain {
            state[c as usize] = verdict;
        }
    }
    for id in 0..n {
        if state[id] != 1 {
            ix.delete(id as u32);
        }
    }
    ix.vols.push(v);
    ix.compact();
    ix.shrink();
    progress(100);
    let stats = ScanStats { records: total_recs as u64, mft_bytes: total_recs as u64 * rs as u64, millis: t0.elapsed().as_millis() };
    Ok((ix, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 1 KB FILE record with fixups applied, the way NTFS writes it to disk.
    fn record(base: u64, dir: bool, attrs: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&48u16.to_le_bytes()); // usa offset
        r[6..8].copy_from_slice(&3u16.to_le_bytes()); // usa count (1 + 2 strides of 512)
        r[20..22].copy_from_slice(&56u16.to_le_bytes()); // first attribute
        r[22..24].copy_from_slice(&(1u16 | if dir { 2 } else { 0 }).to_le_bytes());
        r[32..40].copy_from_slice(&base.to_le_bytes());
        let mut off = 56;
        for (ty, body) in attrs {
            r[off..off + 4].copy_from_slice(&ty.to_le_bytes());
            r[off + 4..off + 8].copy_from_slice(&(body.len() as u32 + 8).to_le_bytes());
            r[off + 8..off + 8 + body.len()].copy_from_slice(body);
            off += body.len() + 8;
        }
        r[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        r[24..28].copy_from_slice(&((off + 8) as u32).to_le_bytes());
        // Apply update sequence: save stride tails into the array, stamp the USN.
        let usn = [0x34, 0x12];
        r[48..50].copy_from_slice(&usn);
        for i in 1..3 {
            let end = i * 512;
            r[48 + 2 * i] = r[end - 2];
            r[48 + 2 * i + 1] = r[end - 1];
            r[end - 2..end].copy_from_slice(&usn);
        }
        r
    }

    /// Resident attribute body (everything after type+length).
    fn resident(value: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 16];
        // nonres=0, name_len=0; value length @16-8=8, value offset @20-8=12 (relative to attr start = 24)
        b[8..12].copy_from_slice(&(value.len() as u32).to_le_bytes());
        b[12..14].copy_from_slice(&24u16.to_le_bytes());
        b.extend_from_slice(value);
        while (b.len() + 8) % 8 != 0 {
            b.push(0);
        }
        b
    }

    fn file_name(parent: u64, ns: u8, name: &str) -> Vec<u8> {
        let u: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 66];
        v[0..8].copy_from_slice(&(parent | (3 << 48)).to_le_bytes());
        v[64] = u.len() as u8;
        v[65] = ns;
        for c in u {
            v.extend_from_slice(&c.to_le_bytes());
        }
        v
    }

    #[test]
    fn parses_synthetic_record() {
        let mut si = vec![0u8; 48];
        si[8..16].copy_from_slice(&777u64.to_le_bytes());
        si[32..36].copy_from_slice(&2u32.to_le_bytes());
        let mut r = record(
            0,
            false,
            &[(0x10, resident(&si)), (0x30, resident(&file_name(5, 1, "Hello wörld.txt"))), (0x30, resident(&file_name(5, 2, "HELLOW~1.TXT"))), (0x80, resident(&[1, 2, 3, 4, 5]))],
        );
        let mut names = Vec::new();
        let (mut mt, mut sz, mut hdr) = (0, 0, false);
        assert!(parse_record(&mut r, 42, &mut |ev| match ev {
            Ev::Header { rec, dir } => {
                assert_eq!((rec, dir), (42, false));
                hdr = true;
            }
            Ev::StdInfo { mtime, attrs, .. } => {
                mt = mtime;
                assert_eq!(attrs, 2);
            }
            Ev::Name { base, parent, ns, name } => {
                assert_eq!((base, parent), (42, 5));
                let u: Vec<u16> = name.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                names.push((ns, String::from_utf16(&u).unwrap()));
            }
            Ev::Data { size, .. } => sz = size,
        }));
        assert!(hdr);
        assert_eq!(mt, 777);
        assert_eq!(sz, 5);
        assert_eq!(names, vec![(1, "Hello wörld.txt".to_string()), (2, "HELLOW~1.TXT".to_string())]);
    }

    #[test]
    fn extension_record_reports_base() {
        let mut r = record(100 | (7 << 48), false, &[(0x30, resident(&file_name(9, 1, "link2")))]);
        let mut seen = None;
        parse_record(&mut r, 555, &mut |ev| {
            if let Ev::Name { base, .. } = ev {
                seen = Some(base)
            }
            assert!(!matches!(ev, Ev::Header { .. }), "extension records have no header event");
        });
        assert_eq!(seen, Some(100));
    }

    #[test]
    fn torn_record_rejected() {
        let mut r = record(0, false, &[]);
        r[1022] ^= 0xFF;
        assert!(!parse_record(&mut r, 1, &mut |_| {}));
    }

    #[test]
    fn runlists() {
        // 0x21: 1-byte length, 2-byte offset. Run 1: 0x18 clusters @ 0x5634. Run 2: 0x10 @ -0x100 relative.
        let rl = [0x21, 0x18, 0x34, 0x56, 0x21, 0x10, 0x00, 0xFF, 0x00];
        assert_eq!(decode_runs(&rl), vec![(0x5634, 0x18), (0x5534, 0x10)]);
        assert_eq!(decode_runs(&[0x01, 0x05, 0x00]), vec![(u64::MAX, 5)]);
    }
}
