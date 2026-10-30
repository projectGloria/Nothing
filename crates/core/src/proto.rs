//! Wire protocol between the UI and the indexing service.
//!
//! Strict request/response over one byte-mode named pipe per client. Every message is
//! `u32 length` + payload. The UI never receives the whole result set — only its size —
//! and then pulls the ~40 rows that are actually visible. That is why scrolling through
//! 1.4M results costs the same as scrolling through 40.

use std::io::{self, Read, Write};

pub const PIPE_NAME: &str = r"\\.\pipe\nothing-index";

pub const REQ_QUERY: u8 = 1;
pub const REQ_ROWS: u8 = 2;
pub const REQ_STATUS: u8 = 3;

pub mod state {
    pub const READY: u8 = 0;
    pub const LOADING: u8 = 1;
    pub const SCANNING: u8 = 2;
}

#[derive(Default, Clone, Debug)]
pub struct Row {
    pub flags: u8,
    pub size: u64,
    pub mtime: u64,
    pub name: String,
    /// Containing folder, e.g. `C:\Users\Nihil`.
    pub path: String,
}

#[derive(Default, Clone, Debug)]
pub struct Status {
    pub version: u64,
    pub entries: u64,
    pub state: u8,
    pub progress: u8,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum Request {
    Query { gen: u32, sort: u8, desc: bool, opts: u8, text: String },
    Rows { gen: u32, first: u32, count: u32 },
    Status,
}

/// Frame builder: reserves the length prefix up front and patches it in `finish`, so a
/// message is a single `write_all` (one syscall, no partial frames on the pipe).
pub struct W(Vec<u8>);

impl Default for W {
    fn default() -> Self {
        Self::new()
    }
}

impl W {
    pub fn new() -> W {
        W(vec![0; 4])
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn str(&mut self, s: &str) -> &mut Self {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    pub fn finish(&mut self) -> &[u8] {
        let n = (self.0.len() - 4) as u32;
        self.0[..4].copy_from_slice(&n.to_le_bytes());
        &self.0
    }
}

/// Bounds-checked reader. Underruns yield zeros/empty instead of panicking: a confused
/// peer gets garbage back, never takes the service down.
pub struct R<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> R<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        R { b, p: 0 }
    }
    fn take(&mut self, n: usize) -> &'a [u8] {
        let end = (self.p + n).min(self.b.len());
        let s = &self.b[self.p..end];
        self.p = end;
        s
    }
    pub fn u8(&mut self) -> u8 {
        self.take(1).first().copied().unwrap_or(0)
    }
    pub fn u32(&mut self) -> u32 {
        let mut a = [0u8; 4];
        let s = self.take(4);
        a[..s.len()].copy_from_slice(s);
        u32::from_le_bytes(a)
    }
    pub fn u64(&mut self) -> u64 {
        let mut a = [0u8; 8];
        let s = self.take(8);
        a[..s.len()].copy_from_slice(s);
        u64::from_le_bytes(a)
    }
    pub fn str(&mut self) -> String {
        let n = self.u32() as usize;
        String::from_utf8_lossy(self.take(n)).into_owned()
    }
}

pub fn read_frame(r: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<()> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n > 64 << 20 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    buf.resize(n, 0);
    r.read_exact(buf)
}

pub fn write_request(w: &mut impl Write, req: &Request) -> io::Result<()> {
    let mut m = W::new();
    match req {
        Request::Query { gen, sort, desc, opts, text } => {
            m.u8(REQ_QUERY).u32(*gen).u8(*sort).u8(*desc as u8).u8(*opts).str(text);
        }
        Request::Rows { gen, first, count } => {
            m.u8(REQ_ROWS).u32(*gen).u32(*first).u32(*count);
        }
        Request::Status => {
            m.u8(REQ_STATUS);
        }
    }
    w.write_all(m.finish())
}

pub fn parse_request(b: &[u8]) -> Option<Request> {
    let mut r = R::new(b);
    Some(match r.u8() {
        REQ_QUERY => Request::Query { gen: r.u32(), sort: r.u8(), desc: r.u8() != 0, opts: r.u8(), text: r.str() },
        REQ_ROWS => Request::Rows { gen: r.u32(), first: r.u32(), count: r.u32() },
        REQ_STATUS => Request::Status,
        _ => return None,
    })
}

pub fn put_row(m: &mut W, row: &Row) {
    m.u8(row.flags).u64(row.size).u64(row.mtime).str(&row.name).str(&row.path);
}

pub fn get_row(r: &mut R) -> Row {
    Row { flags: r.u8(), size: r.u64(), mtime: r.u64(), name: r.str(), path: r.str() }
}

pub fn put_status(m: &mut W, s: &Status) {
    m.u64(s.version).u64(s.entries).u8(s.state).u8(s.progress).str(&s.message);
}

pub fn get_status(r: &mut R) -> Status {
    Status { version: r.u64(), entries: r.u64(), state: r.u8(), progress: r.u8(), message: r.str() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames() {
        let mut pipe = Vec::new();
        write_request(&mut pipe, &Request::Query { gen: 7, sort: 2, desc: true, opts: 1, text: "héllo".into() }).unwrap();
        let mut buf = Vec::new();
        read_frame(&mut &pipe[..], &mut buf).unwrap();
        match parse_request(&buf) {
            Some(Request::Query { gen: 7, sort: 2, desc: true, opts: 1, text }) => assert_eq!(text, "héllo"),
            other => panic!("{other:?}"),
        }
        // Truncated payloads decode to defaults instead of panicking.
        let mut r = R::new(&[1, 2]);
        assert_eq!(r.u64(), 0x0201);
        assert_eq!(r.str(), "");
    }
}
