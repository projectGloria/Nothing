//! UI side of the pipe protocol, on its own thread.
//!
//! The UI thread never blocks on the service: it posts commands here and gets results
//! back as window messages. Bursts of keystrokes are coalesced — if you type "chernobyl"
//! faster than queries complete, only the latest text is ever sent.

use std::fs::{File, OpenOptions};
use std::sync::mpsc::{channel, Receiver, Sender};

use nothing_core::proto::{self, Request, Row, Status, R};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

pub const FIRST_ROWS: u32 = 120;

pub enum Cmd {
    Query { gen: u32, text: String, sort: u8, desc: bool, opts: u8, first_row: u32, keep: bool },
    Rows { gen: u32, first: u32, count: u32 },
    Status,
}

pub enum Event {
    Result { gen: u32, total: u32, micros: u32, keep: bool, first: u32, rows: Vec<Row> },
    Rows { gen: u32, first: u32, rows: Vec<Row> },
    /// `None` = service unreachable.
    Status(Option<Status>),
}

pub struct Ipc {
    tx: Sender<Cmd>,
}

impl Ipc {
    pub fn spawn(hwnd: HWND, msg: u32) -> Ipc {
        let (tx, rx) = channel();
        let hwnd = hwnd as isize; // HWND isn't Send; the value is just an id
        std::thread::spawn(move || worker(rx, hwnd, msg));
        Ipc { tx }
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }
}

fn post(hwnd: isize, msg: u32, ev: Event) {
    let p = Box::into_raw(Box::new(ev));
    if unsafe { PostMessageW(hwnd as HWND, msg, 0, p as isize) } == 0 {
        drop(unsafe { Box::from_raw(p) }); // window gone
    }
}

struct Conn {
    pipe: Option<File>,
    buf: Vec<u8>,
}

impl Conn {
    fn call(&mut self, req: &Request) -> Option<Vec<u8>> {
        // One reconnect attempt: the service may have restarted since the last call.
        for _ in 0..2 {
            if self.pipe.is_none() {
                self.pipe = OpenOptions::new().read(true).write(true).open(proto::PIPE_NAME).ok();
            }
            let p = self.pipe.as_mut()?;
            if proto::write_request(p, req).is_ok() && proto::read_frame(p, &mut self.buf).is_ok() {
                return Some(self.buf.clone());
            }
            self.pipe = None;
        }
        None
    }

    fn rows(&mut self, gen: u32, first: u32, count: u32) -> Option<(u32, Vec<Row>)> {
        let b = self.call(&Request::Rows { gen, first, count })?;
        let mut r = R::new(&b);
        let (_g, first, n) = (r.u32(), r.u32(), r.u32());
        Some((first, (0..n).map(|_| proto::get_row(&mut r)).collect()))
    }
}

fn worker(rx: Receiver<Cmd>, hwnd: isize, msg: u32) {
    let mut c = Conn { pipe: None, buf: Vec::new() };
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        // Coalesce: newest query wins; row fetches for older generations are moot.
        let latest = batch.iter().rev().find_map(|c| if let Cmd::Query { gen, .. } = c { Some(*gen) } else { None });
        let mut status = false;
        let mut seen_rows = Vec::new();
        for cmd in batch {
            match cmd {
                Cmd::Query { gen, text, sort, desc, opts, first_row, keep } => {
                    if Some(gen) != latest {
                        continue;
                    }
                    let Some(b) = c.call(&Request::Query { gen, sort, desc, opts, text }) else {
                        post(hwnd, msg, Event::Status(None));
                        continue;
                    };
                    let mut r = R::new(&b);
                    let (_, total, micros) = (r.u32(), r.u32(), r.u32());
                    // Ship the first screenful with the count: one round trip to first paint.
                    let start = first_row.min(total.saturating_sub(1));
                    let (first, rows) = c.rows(gen, start, FIRST_ROWS).unwrap_or((start, Vec::new()));
                    post(hwnd, msg, Event::Result { gen, total, micros, keep, first, rows });
                }
                Cmd::Rows { gen, first, count } => {
                    if latest.is_some_and(|l| gen < l) || seen_rows.contains(&(gen, first)) {
                        continue;
                    }
                    seen_rows.push((gen, first));
                    if let Some((first, rows)) = c.rows(gen, first, count) {
                        post(hwnd, msg, Event::Rows { gen, first, rows });
                    }
                }
                Cmd::Status => status = true,
            }
        }
        if status {
            let st = c.call(&Request::Status).map(|b| proto::get_status(&mut R::new(&b)));
            post(hwnd, msg, Event::Status(st));
        }
    }
}
