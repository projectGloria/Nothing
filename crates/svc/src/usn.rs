//! NTFS USN change journal: the kernel's own log of every create/delete/rename/write.
//! Tailing it keeps the index live with zero polling and zero directory scanning, and
//! lets a restart resume exactly where the last run stopped.

use std::io;

use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    FileBasicInfo, FileIdType, FileStandardInfo, GetFileInformationByHandleEx, OpenFileById, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_ID_DESCRIPTOR, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
};
use windows_sys::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Ioctl::{
    CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects, INFINITE};

use crate::win::{ioctl, last_error, pod, Handle};

const MASK48: u64 = 0xFFFF_FFFF_FFFF;

pub const R_DATA: u32 = 0x1 | 0x2 | 0x4; // overwrite | extend | truncation
pub const R_CREATE: u32 = 0x100;
pub const R_DELETE: u32 = 0x200;
pub const R_BASIC_INFO: u32 = 0x8000;
pub const R_HARD_LINK: u32 = 0x10000;

const ERROR_JOURNAL_DELETE_IN_PROGRESS: u32 = 1178;
const ERROR_JOURNAL_NOT_ACTIVE: u32 = 1179;
const ERROR_JOURNAL_ENTRY_DELETED: u32 = 1181;
const ERROR_INVALID_PARAMETER: u32 = 87;

#[derive(Clone, Copy, Debug)]
pub struct Journal {
    pub id: u64,
    pub first: i64,
    pub next: i64,
}

pub fn query(h: HANDLE) -> io::Result<Journal> {
    let mut out = [0u8; 80];
    ioctl::<()>(h, FSCTL_QUERY_USN_JOURNAL, None, &mut out)?;
    let d: USN_JOURNAL_DATA_V0 = pod(&out);
    Ok(Journal { id: d.UsnJournalID, first: d.FirstUsn, next: d.NextUsn })
}

pub fn query_or_create(h: HANDLE) -> io::Result<Journal> {
    match query(h) {
        Ok(j) => Ok(j),
        Err(e) if matches!(e.raw_os_error().map(|c| c as u32), Some(ERROR_JOURNAL_NOT_ACTIVE | ERROR_JOURNAL_DELETE_IN_PROGRESS)) => {
            // Non-system volumes may have no journal. 32 MB holds days of normal churn.
            let c = CREATE_USN_JOURNAL_DATA { MaximumSize: 32 << 20, AllocationDelta: 8 << 20 };
            ioctl(h, FSCTL_CREATE_USN_JOURNAL, Some(&c), &mut [])?;
            query(h)
        }
        Err(e) => Err(e),
    }
}

#[derive(Debug)]
pub enum Change {
    Delete { rec: u64 },
    /// File (or one of its hard links) appeared or disappeared under this name.
    Link { rec: u64, parent: u64, name: String, attrs: u32 },
    /// Create / rename / move / content or attribute change. `meta` is filled by the
    /// tail thread outside the index lock.
    Upsert { rec: u64, frn: u64, parent: u64, name: String, attrs: u32, fetch: bool, meta: Option<(u64, u64)> },
}

pub enum TailError {
    Stopped,
    /// Journal was recreated, wrapped past us, or deleted: the volume needs a rescan.
    Reset,
    Io(io::Error),
}

/// Overlapped journal reader that can be cancelled by a stop event.
pub struct Tail {
    pub vol: Handle,
    event: Handle,
    buf: Vec<u8>,
}

impl Tail {
    pub fn new(vol: Handle) -> io::Result<Tail> {
        let ev = Handle::checked(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })?;
        Ok(Tail { vol, event: ev, buf: vec![0u8; 256 << 10] })
    }

    /// Block until at least one close record exists after `start`, then return the new
    /// resume position and the decoded changes.
    pub fn read(&mut self, journal_id: u64, start: i64, stop: HANDLE) -> Result<(i64, Vec<Change>), TailError> {
        let req = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: start,
            ReasonMask: 0xFFFF_FFFF,
            // Only records whose handle has closed: they carry the cumulative reasons and
            // final name of an operation, instead of 3-6 intermediate records.
            ReturnOnlyOnClose: 1,
            Timeout: 0,
            BytesToWaitFor: 1,
            UsnJournalID: journal_id,
        };
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.event.0;
        let mut got = 0u32;
        unsafe {
            let ok = DeviceIoControl(
                self.vol.0,
                FSCTL_READ_USN_JOURNAL,
                &req as *const _ as *const _,
                std::mem::size_of_val(&req) as u32,
                self.buf.as_mut_ptr() as *mut _,
                self.buf.len() as u32,
                &mut got,
                &mut ov,
            );
            if ok == 0 {
                let e = last_error();
                if e != ERROR_IO_PENDING {
                    return Err(classify(e));
                }
                let hs = [self.event.0, stop];
                let w = WaitForMultipleObjects(2, hs.as_ptr(), 0, INFINITE);
                if w != WAIT_OBJECT_0 {
                    CancelIoEx(self.vol.0, &ov);
                    GetOverlappedResult(self.vol.0, &ov, &mut got, 1);
                    return Err(TailError::Stopped);
                }
                if GetOverlappedResult(self.vol.0, &ov, &mut got, 0) == 0 {
                    return Err(classify(last_error()));
                }
            }
        }
        let got = got as usize;
        if got < 8 {
            return Ok((start, Vec::new()));
        }
        let next = i64::from_le_bytes(self.buf[0..8].try_into().unwrap());
        Ok((next, decode(&self.buf[8..got])))
    }
}

fn classify(code: u32) -> TailError {
    match code {
        ERROR_JOURNAL_ENTRY_DELETED | ERROR_JOURNAL_NOT_ACTIVE | ERROR_JOURNAL_DELETE_IN_PROGRESS | ERROR_INVALID_PARAMETER => TailError::Reset,
        c => TailError::Io(io::Error::from_raw_os_error(c as i32)),
    }
}

/// Decode USN_RECORD_V2/V3 records into index operations.
pub fn decode(mut b: &[u8]) -> Vec<Change> {
    let mut out = Vec::new();
    while b.len() >= 8 {
        let len = u32::from_le_bytes(b[0..4].try_into().unwrap()) as usize;
        if len < 8 || len > b.len() {
            break;
        }
        let r = &b[..len];
        b = &b[len..];
        let major = u16::from_le_bytes([r[4], r[5]]);
        let rd64 = |o: usize| u64::from_le_bytes(r[o..o + 8].try_into().unwrap());
        let rd32 = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
        let rd16 = |o: usize| u16::from_le_bytes([r[o], r[o + 1]]) as usize;
        // (frn, parent, reason, attrs, name_len, name_off)
        let (frn, parent, reason, attrs, nl, no) = match major {
            2 if len >= 60 => (rd64(8), rd64(16), rd32(40), rd32(52), rd16(56), rd16(58)),
            // V3 uses 128-bit file ids; NTFS ids fit in the low 64 bits.
            3 if len >= 76 => (rd64(8), rd64(24), rd32(56), rd32(68), rd16(72), rd16(74)),
            _ => continue,
        };
        let Some(nb) = r.get(no..no + nl) else { continue };
        let name = String::from_utf16_lossy(&nb.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>());
        let (rec, parent) = (frn & MASK48, parent & MASK48);
        out.push(if reason & R_DELETE != 0 {
            Change::Delete { rec }
        } else if reason & R_HARD_LINK != 0 && reason & R_CREATE == 0 {
            Change::Link { rec, parent, name, attrs }
        } else {
            Change::Upsert { rec, frn, parent, name, attrs, fetch: reason & (R_DATA | R_CREATE | R_BASIC_INFO) != 0, meta: None }
        });
    }
    out
}

/// Current (size, mtime) of a file by its file reference number. Called outside the index
/// lock; files already gone again just keep their old metadata.
pub fn fetch_meta(vol: HANDLE, frn: u64) -> Option<(u64, u64)> {
    unsafe {
        let mut id: FILE_ID_DESCRIPTOR = std::mem::zeroed();
        id.dwSize = std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32;
        id.Type = FileIdType;
        id.Anonymous.FileId = frn as i64;
        let h = OpenFileById(
            vol,
            &id,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            FILE_FLAG_BACKUP_SEMANTICS,
        );
        let h = Handle::checked(h).ok()?;
        let mut bi: FILE_BASIC_INFO = std::mem::zeroed();
        let mut si: FILE_STANDARD_INFO = std::mem::zeroed();
        if GetFileInformationByHandleEx(h.0, FileBasicInfo, &mut bi as *mut _ as *mut _, std::mem::size_of_val(&bi) as u32) == 0 {
            return None;
        }
        let size = if GetFileInformationByHandleEx(h.0, FileStandardInfo, &mut si as *mut _ as *mut _, std::mem::size_of_val(&si) as u32) != 0
            && !si.Directory
        {
            si.EndOfFile as u64
        } else {
            0
        };
        Some((size, bi.LastWriteTime as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec_v2(frn: u64, parent: u64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let u: Vec<u16> = name.encode_utf16().collect();
        let len = (60 + u.len() * 2 + 7) & !7;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attrs.to_le_bytes());
        r[56..58].copy_from_slice(&((u.len() * 2) as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, c) in u.iter().enumerate() {
            r[60 + 2 * i..62 + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
        r
    }

    #[test]
    fn decodes() {
        let mut b = rec_v2(0x0003_0000_0000_0064, 5, R_CREATE | 0x8000_0000, 0x20, "new.txt");
        b.extend(rec_v2(0x65, 5, R_DELETE | 0x8000_0000, 0x20, "gone.txt"));
        b.extend(rec_v2(0x66, 7, R_HARD_LINK | 0x8000_0000, 0x20, "link.txt"));
        let c = decode(&b);
        assert!(matches!(&c[0], Change::Upsert { rec: 100, parent: 5, name, fetch: true, .. } if name == "new.txt"));
        assert!(matches!(c[1], Change::Delete { rec: 0x65 }));
        assert!(matches!(&c[2], Change::Link { rec: 0x66, parent: 7, .. }));
    }
}
