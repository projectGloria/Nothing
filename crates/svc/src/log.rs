//! Minimal logger: stderr when running in a console, a small rolling file always.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
pub static CONSOLE: AtomicBool = AtomicBool::new(false);

pub fn init(console: bool) {
    CONSOLE.store(console, Ordering::Relaxed);
    let dir = crate::win::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("service.log");
    // Keep the log from growing forever: start over once it passes 1 MB.
    if std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        *FILE.lock().unwrap() = Some(f);
    }
}

pub fn write(msg: &str) {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let line = format!("[{secs}] {msg}\n");
    if CONSOLE.load(Ordering::Relaxed) {
        eprint!("{line}");
    }
    if let Some(f) = FILE.lock().unwrap().as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log::write(&format!($($t)*)) };
}
