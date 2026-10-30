//! Image/video thumbnails for the big icon views, rendered by the same shell thumbnail
//! handlers Explorer uses (`IShellItemImageFactory`, which also hits thumbcache.db).
//!
//! Extraction is slow (milliseconds for a cached JPEG, far more for a video), so it runs
//! on a few background STA threads. The UI never queues "everything it painted": on each
//! list paint it rebuilds the queue from the rows around the viewport, in priority order,
//! so flinging through a million results leaves no backlog — rows that scrolled away are
//! simply no longer in the queue. Finished bitmaps live in a byte- and count-bounded LRU.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

use windows_sys::Win32::Foundation::{HWND, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::UI::Shell::{SHCreateItemFromParsingName, SIIGBF_RESIZETOFIT, SIIGBF_THUMBNAILONLY};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::com::{Com, IShellItemImageFactoryVtbl, IID_ISHELLITEMIMAGEFACTORY};
use crate::util::wide;

const WORKERS: usize = 3;
/// Pixel memory for thumbnails outside the viewport. 256 KB per 256×256 thumb, so
/// ~250 jumbo ones; the visible window is never evicted, even if it alone is bigger.
const BUDGET_BYTES: usize = 64 << 20;
/// Every bitmap is a GDI object and a process gets 10,000 of those by default.
const MAX_BITMAPS: usize = 1500;
/// Negative results ("no thumbnail handler") are tiny but still worth bounding.
const MAX_ENTRIES: usize = 20_000;

/// A premultiplied 32bpp top-down DIB section, ready for AlphaBlend. Freed on drop, so
/// a result that is discarded anywhere (stale view, failed PostMessage) can't leak.
pub struct Thumb {
    bmp: isize, // HBITMAP; isize because raw pointers aren't Send
    w: i32,
    h: i32,
}

impl Drop for Thumb {
    fn drop(&mut self) {
        unsafe { DeleteObject(self.bmp as HGDIOBJ) };
    }
}

/// Worker -> UI, boxed through PostMessage.
pub struct Done {
    gen: u32,
    pub row: u32,
    pub path: String,
    thumb: Option<Thumb>,
}

struct Job {
    gen: u32,
    size: i32,
    row: u32,
    path: String,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    /// Paths a worker is rendering right now; not re-queued until they come back.
    busy: Vec<String>,
}

struct Entry {
    thumb: Option<Thumb>,
    tick: u64,
}

pub struct Thumbs {
    shared: Arc<(Mutex<Queue>, Condvar)>,
    /// Bumped whenever the size changes, so late results of the old size are dropped.
    gen: u32,
    /// Requested edge in pixels; 0 = thumbnails off (details / medium view).
    size: i32,
    cache: HashMap<String, Entry>,
    tick: u64,
    bytes: usize,
    bitmaps: usize,
    dc: HDC,
    /// Row range the queue was last built for (viewport + prefetch).
    pub window: (u32, u32),
    /// Rows or result order changed: rebuild the queue even if the window didn't move.
    pub dirty: bool,
}

impl Thumbs {
    pub fn new(hwnd: HWND, msg: u32) -> Thumbs {
        let shared: Arc<(Mutex<Queue>, Condvar)> = Arc::default();
        let h = hwnd as isize;
        for _ in 0..WORKERS {
            let shared = shared.clone();
            std::thread::spawn(move || worker(&shared, h, msg));
        }
        Thumbs {
            shared,
            gen: 0,
            size: 0,
            cache: HashMap::new(),
            tick: 0,
            bytes: 0,
            bitmaps: 0,
            dc: unsafe { CreateCompatibleDC(std::ptr::null_mut()) },
            window: (u32::MAX, 0),
            dirty: true,
        }
    }

    pub fn active(&self) -> bool {
        self.size > 0
    }

    /// Switch thumbnail size (0 = off). Drops every cached bitmap: they're the wrong
    /// size now, and in the details view they'd just be dead memory.
    pub fn set_size(&mut self, size: i32) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.gen = self.gen.wrapping_add(1);
        self.cache.clear();
        self.bytes = 0;
        self.bitmaps = 0;
        self.shared.0.lock().unwrap().jobs.clear();
        self.dirty = true;
    }

    /// True once a real thumbnail is cached: the row then hides its system icon.
    pub fn has(&self, path: &str) -> bool {
        self.cache.get(path).is_some_and(|e| e.thumb.is_some())
    }

    /// Not yet tried (a negative result counts as tried).
    pub fn wants(&self, path: &str) -> bool {
        !self.cache.contains_key(path)
    }

    /// Replace the queue with `rows` (row index, path), highest priority first.
    pub fn schedule<'a>(&self, rows: impl Iterator<Item = (u32, &'a str)>) {
        let (lock, cv) = &*self.shared;
        let mut q = lock.lock().unwrap();
        q.jobs.clear();
        for (row, path) in rows {
            if !q.busy.iter().any(|b| b == path) {
                q.jobs.push_back(Job { gen: self.gen, size: self.size, row, path: path.to_string() });
            }
        }
        if !q.jobs.is_empty() {
            cv.notify_all();
        }
    }

    /// Take a finished job. Returns true if it produced a bitmap worth repainting for.
    pub fn accept(&mut self, d: Done) -> bool {
        if d.gen != self.gen || self.size == 0 {
            return false; // `d` drops here, freeing a stale bitmap
        }
        let got = d.thumb.is_some();
        if let Some(t) = &d.thumb {
            self.bytes += (t.w * t.h * 4) as usize;
            self.bitmaps += 1;
        }
        self.tick += 1;
        // A path can come back twice if it was re-queued while in flight; keep the
        // accounting honest by releasing whatever it replaces.
        if let Some(old) = self.cache.insert(d.path, Entry { thumb: d.thumb, tick: self.tick }) {
            self.forget(&old);
        }
        got
    }

    fn forget(&mut self, e: &Entry) {
        if let Some(t) = &e.thumb {
            self.bytes -= (t.w * t.h * 4) as usize;
            self.bitmaps -= 1;
        }
    }

    pub fn over_budget(&self) -> bool {
        self.bytes > BUDGET_BYTES || self.bitmaps > MAX_BITMAPS || self.cache.len() > MAX_ENTRIES
    }

    /// LRU eviction down to 3/4 of every limit, never touching `keep` (the rows in the
    /// current window, which would otherwise thrash: evict, repaint, re-extract...).
    /// Batched, so the sort is amortised over many inserts.
    pub fn evict(&mut self, keep: &HashSet<&str>) {
        let mut old: Vec<(u64, String)> = self.cache.iter().filter(|(k, _)| !keep.contains(k.as_str())).map(|(k, e)| (e.tick, k.clone())).collect();
        old.sort_unstable_by_key(|(t, _)| *t);
        for (_, k) in old {
            if self.bytes <= BUDGET_BYTES * 3 / 4 && self.bitmaps <= MAX_BITMAPS * 3 / 4 && self.cache.len() <= MAX_ENTRIES * 3 / 4 {
                break;
            }
            if let Some(e) = self.cache.remove(&k) {
                self.forget(&e); // `e` drops after this, deleting the bitmap
            }
        }
    }

    /// Paint the thumbnail for `path` into the item's icon area. Returns false if there
    /// is none (caller leaves the system icon alone).
    pub fn draw(&mut self, hdc: HDC, icon: &RECT, path: &str) -> bool {
        self.tick += 1;
        let Some(e) = self.cache.get_mut(path) else { return false };
        e.tick = self.tick;
        let Some(t) = &e.thumb else { return false };
        // The shell returns the image fitted inside size×size keeping its aspect ratio.
        // Centre that square in the icon area, then sit the image on its bottom edge
        // (like Explorer) so labels of wide and tall pictures line up.
        let s = self.size;
        let x = icon.left + (icon.right - icon.left - s) / 2 + (s - t.w) / 2;
        let y = icon.top + (icon.bottom - icon.top - s) / 2 + (s - t.h);
        unsafe {
            let old = SelectObject(self.dc, t.bmp as HGDIOBJ);
            let bf = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            // Per-pixel alpha over whatever the ListView drew underneath (theme
            // background or the selection box), so transparent PNGs follow the theme.
            AlphaBlend(hdc, x, y, t.w, t.h, self.dc, 0, 0, t.w, t.h, bf);
            SelectObject(self.dc, old);
        }
        true
    }
}

fn worker(shared: &(Mutex<Queue>, Condvar), hwnd: isize, msg: u32) {
    // Thumbnail handlers are apartment-threaded COM objects; STA avoids marshalling.
    unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    let (lock, cv) = shared;
    loop {
        let job = {
            let mut q = lock.lock().unwrap();
            loop {
                if let Some(j) = q.jobs.pop_front() {
                    q.busy.push(j.path.clone());
                    break j;
                }
                q = cv.wait(q).unwrap();
            }
        };
        let thumb = render(&job.path, job.size);
        lock.lock().unwrap().busy.retain(|b| *b != job.path);
        let p = Box::into_raw(Box::new(Done { gen: job.gen, row: job.row, path: job.path, thumb }));
        if unsafe { PostMessageW(hwnd as HWND, msg, 0, p as isize) } == 0 {
            drop(unsafe { Box::from_raw(p) });
        }
    }
}

fn render(path: &str, size: i32) -> Option<Thumb> {
    unsafe {
        let mut p = std::ptr::null_mut();
        if SHCreateItemFromParsingName(wide(path).as_ptr(), std::ptr::null_mut(), &IID_ISHELLITEMIMAGEFACTORY, &mut p) < 0 || p.is_null() {
            return None;
        }
        let f = Com(p);
        let mut hbm: HBITMAP = std::ptr::null_mut();
        // THUMBNAILONLY: fail instead of handing back the type icon, which the system
        // image list already shows for free.
        let hr = (f.vtbl::<IShellItemImageFactoryVtbl>().get_image)(f.0, SIZE { cx: size, cy: size }, SIIGBF_RESIZETOFIT | SIIGBF_THUMBNAILONLY, &mut hbm);
        if hr < 0 || hbm.is_null() {
            return None;
        }
        let t = normalize(hbm);
        DeleteObject(hbm as HGDIOBJ);
        t
    }
}

/// Copy the shell's bitmap into our own premultiplied top-down DIB. Handlers disagree on
/// alpha: JPEG-ish thumbnails often come back with alpha = 0 everywhere (meaning
/// "opaque"), PNGs sometimes straight (non-premultiplied). AlphaBlend wants premultiplied,
/// and fixing it here keeps the paint path a single blit.
fn normalize(src: HBITMAP) -> Option<Thumb> {
    unsafe {
        let mut bm: BITMAP = std::mem::zeroed();
        if GetObjectW(src as HGDIOBJ, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut _) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || h <= 0 || w > 4096 || h > 4096 {
            return None;
        }
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        let lines = GetDIBits(dc, src, 0, h as u32, px.as_mut_ptr() as *mut _, &mut bmi, DIB_RGB_COLORS);
        DeleteDC(dc);
        if lines != h {
            return None;
        }
        // BGRA pixels; w*h*4 bytes, so the remainder is always empty.
        let (pixels, _) = px.as_chunks_mut::<4>();
        if pixels.iter().all(|p| p[3] == 0) {
            for p in pixels.iter_mut() {
                p[3] = 255;
            }
        } else if pixels.iter().any(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3]) {
            // A colour channel above alpha is impossible when premultiplied.
            for p in pixels.iter_mut() {
                let a = p[3] as u32;
                for c in &mut p[..3] {
                    *c = (*c as u32 * a / 255) as u8;
                }
            }
        }
        let mut bits = std::ptr::null_mut();
        let bmp = CreateDIBSection(std::ptr::null_mut(), &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if bmp.is_null() || bits.is_null() {
            return None;
        }
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
        Some(Thumb { bmp: bmp as isize, w, h })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_real_thumbnail() {
        unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        let t = render(r"C:\Windows\Web\Wallpaper\Windows\img0.jpg", 256).expect("jpeg thumbnail");
        assert!(t.w > 0 && t.h > 0 && t.w <= 256 && t.h <= 256, "{}x{}", t.w, t.h);
        // No thumbnail handler for a DLL: THUMBNAILONLY must fail rather than return its icon.
        assert!(render(r"C:\Windows\System32\kernel32.dll", 256).is_none());
    }
}
