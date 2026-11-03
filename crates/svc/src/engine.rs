//! Service brain: owns the index, boots it (snapshot or MFT scan), keeps it live from the
//! USN journals, and snapshots it to disk.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use nothing_core::index::flag;
use nothing_core::proto::{state, Status};
use nothing_core::{persist, Index, NONE};
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

use crate::usn::{Change, Tail, TailError};
use crate::win::{data_dir, wide, Handle};
use crate::{log, mft, usn};

const DRIVE_FIXED: u32 = 3;
const SAVE_EVERY: Duration = Duration::from_secs(30 * 60);
const ROOT_REC: u64 = 5;

pub struct Engine {
    pub db: RwLock<Index>,
    pub stop: Handle,
    persist: bool,
    dirty: AtomicBool,
    /// Volumes currently being (re)scanned, with progress, for the status line.
    scanning: Mutex<Vec<(u8, u8)>>,
    loading: AtomicBool,
    workers: Mutex<HashSet<u8>>,
}

impl Engine {
    pub fn new(persist: bool) -> Arc<Engine> {
        let stop = Handle::checked(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) }).expect("CreateEvent");
        Arc::new(Engine {
            db: RwLock::new(Index::default()),
            stop,
            persist,
            dirty: AtomicBool::new(false),
            scanning: Mutex::new(Vec::new()),
            loading: AtomicBool::new(true),
            workers: Mutex::new(HashSet::new()),
        })
    }

    /// Engine over a prebuilt index (dev `--walk` mode): no volumes, no journal.
    pub fn with_index(ix: Index) -> Arc<Engine> {
        let e = Engine::new(false);
        *e.db.write().unwrap() = ix;
        e.loading.store(false, Relaxed);
        e
    }

    pub fn request_stop(&self) {
        unsafe { SetEvent(self.stop.0) };
    }

    /// Sleep that wakes early on shutdown. Returns true if stopping.
    fn wait_stop(&self, d: Duration) -> bool {
        unsafe { WaitForSingleObject(self.stop.0, d.as_millis() as u32) == WAIT_OBJECT_0 }
    }

    pub fn status(&self) -> Status {
        let (version, entries) = match self.db.try_read() {
            Ok(ix) => (ix.version, ix.live() as u64),
            // A writer is mid-batch; report "changed" so the UI asks again shortly.
            Err(_) => (u64::MAX, 0),
        };
        let scanning = self.scanning.lock().unwrap().clone();
        let (st, progress, message) = if self.loading.load(Relaxed) {
            (state::LOADING, 0, "Loading index…".to_string())
        } else if let Some(&(l, p)) = scanning.first() {
            (state::SCANNING, p, format!("Indexing {}: {}%", l as char, p))
        } else {
            (state::READY, 100, String::new())
        };
        Status { version, entries, state: st, progress, message }
    }

    fn index_path() -> PathBuf {
        data_dir().join("index.bin")
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        std::thread::spawn(move || me.boot());
    }

    fn boot(self: Arc<Self>) {
        let t0 = Instant::now();
        let mut resumed = Vec::new();
        if self.persist {
            match persist::load(&Self::index_path()) {
                Ok(mut ix) => {
                    for vi in (0..ix.vols.len()).rev() {
                        if !volume_resumable(&ix.vols[vi]) {
                            log!("snapshot of {}: is stale, will rescan", ix.vols[vi].letter as char);
                            ix.remove_volume(vi);
                        }
                    }
                    resumed = ix.vols.iter().map(|v| (v.letter, v.serial)).collect();
                    log!("loaded snapshot: {} entries in {} ms", ix.len(), t0.elapsed().as_millis());
                    ix.version += 1;
                    *self.db.write().unwrap() = ix;
                }
                Err(e) => log!("no usable snapshot ({e}), full scan"),
            }
        }
        self.loading.store(false, Relaxed);
        for (letter, serial) in resumed {
            self.spawn_volume(letter, serial, false);
        }
        self.check_volumes();

        // Housekeeping: notice drives coming/going, snapshot periodically.
        let mut last_save = Instant::now();
        while !self.wait_stop(Duration::from_secs(60)) {
            self.check_volumes();
            if self.dirty.load(Relaxed) && last_save.elapsed() > SAVE_EVERY {
                self.save();
                last_save = Instant::now();
            }
        }
    }

    fn check_volumes(self: &Arc<Self>) {
        let present = ntfs_volumes();
        let mut to_spawn = Vec::new();
        {
            let scanning: Vec<u8> = self.scanning.lock().unwrap().iter().map(|s| s.0).collect();
            let workers = self.workers.lock().unwrap().clone();
            let mut ix = self.db.write().unwrap();
            for vi in (0..ix.vols.len()).rev() {
                let l = ix.vols[vi].letter;
                if !present.iter().any(|p| p.0 == l) && !scanning.contains(&l) && !workers.contains(&l) {
                    log!("volume {}: gone, dropping", l as char);
                    ix.remove_volume(vi);
                    self.dirty.store(true, Relaxed);
                }
            }
            for &(l, serial) in &present {
                if !workers.contains(&l) && !scanning.contains(&l) {
                    let need_scan = ix.vol_index(l).is_none();
                    to_spawn.push((l, serial, need_scan));
                }
            }
        }
        for (l, serial, scan_first) in to_spawn {
            self.spawn_volume(l, serial, scan_first);
        }
    }

    fn set_progress(&self, letter: u8, p: Option<u8>) {
        let mut s = self.scanning.lock().unwrap();
        s.retain(|x| x.0 != letter);
        if let Some(p) = p {
            s.push((letter, p));
        }
    }

    fn spawn_volume(self: &Arc<Self>, letter: u8, serial: u32, scan_first: bool) {
        if scan_first {
            self.set_progress(letter, Some(0));
        }
        self.workers.lock().unwrap().insert(letter);
        let me = self.clone();
        std::thread::spawn(move || {
            me.clone().run_volume(letter, serial, scan_first);
            me.workers.lock().unwrap().remove(&letter);
        });
    }

    /// One thread per volume: (re)scan when needed, then tail the journal forever.
    fn run_volume(self: Arc<Self>, letter: u8, serial: u32, mut need_scan: bool) {
        loop {
            if need_scan {
                self.set_progress(letter, Some(0));
                let r = mft::scan(letter, serial, &|p| self.set_progress(letter, Some(p)));
                match r {
                    Ok((vix, st)) => {
                        let n = vix.len();
                        {
                            let mut ix = self.db.write().unwrap();
                            if let Some(vi) = ix.vol_index(letter) {
                                ix.remove_volume(vi);
                            }
                            ix.append(vix);
                            ix.rebuild_sorts();
                        }
                        self.set_progress(letter, None);
                        log!(
                            "scanned {}: {} entries from {} MFT records ({} MB) in {} ms ({})",
                            letter as char,
                            n,
                            st.records,
                            st.mft_bytes >> 20,
                            st.millis,
                            st.phases
                        );
                        self.dirty.store(true, Relaxed);
                        self.save();
                    }
                    Err(e) => {
                        self.set_progress(letter, None);
                        log!("scan {}: failed: {e}", letter as char);
                        return; // check_volumes retries in a minute
                    }
                }
            }
            match self.tail(letter) {
                TailEnd::Stop => return,
                TailEnd::Rescan => need_scan = true,
            }
        }
    }

    fn tail(&self, letter: u8) -> TailEnd {
        let Some((jid, mut next)) = ({
            let ix = self.db.read().unwrap();
            ix.vol_index(letter).map(|vi| (ix.vols[vi].journal_id, ix.vols[vi].next_usn))
        }) else {
            return TailEnd::Stop;
        };
        let mut tail = match mft::open_volume(letter, true).and_then(Tail::new) {
            Ok(t) => t,
            Err(e) => {
                log!("tail {}: open failed: {e}", letter as char);
                return TailEnd::Stop;
            }
        };
        let mut pending: Vec<(u32, Change)> = Vec::new();
        let mut pending_checkpoint: Option<i64> = None;
        loop {
            match tail.read(jid, next, self.stop.0) {
                Ok((n, mut changes)) => {
                    let batch_start = next;
                    // Disk-touching metadata lookups happen before taking the write lock,
                    // so searches never wait on I/O.
                    for c in changes.iter_mut() {
                        if let Change::Upsert { frn, fetch: true, meta, .. } = c {
                            *meta = usn::fetch_meta(tail.vol.0, *frn);
                        }
                    }
                    let mut ix = self.db.write().unwrap();
                    let Some(vi) = ix.vol_index(letter) else { return TailEnd::Stop };
                    let changed = apply(&mut ix, vi, changes, &mut pending);
                    if pending.is_empty() {
                        pending_checkpoint = None;
                        ix.vols[vi].next_usn = n;
                    } else {
                        if pending_checkpoint.is_none() {
                            pending_checkpoint = Some(batch_start);
                        }
                        ix.vols[vi].next_usn = pending_checkpoint.unwrap();
                    }
                    ix.apply_changes(&changed);
                    if ix.deleted > 200_000 || ix.garbage > 64 << 20 {
                        ix.compact();
                    }
                    next = n;
                    self.dirty.store(true, Relaxed);
                }
                Err(TailError::Stopped) => return TailEnd::Stop,
                Err(TailError::Reset) => {
                    log!("journal {}: reset/overflow, rescanning", letter as char);
                    return TailEnd::Rescan;
                }
                Err(TailError::Io(e)) => {
                    log!("journal {}: {e}", letter as char);
                    if self.wait_stop(Duration::from_secs(5)) || !ntfs_volumes().iter().any(|v| v.0 == letter) {
                        return TailEnd::Stop;
                    }
                }
            }
        }
    }

    pub fn save(&self) {
        if !self.persist {
            return;
        }
        let path = Self::index_path();
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        for _ in 0..3 {
            self.db.write().unwrap().compact();
            let ix = self.db.read().unwrap();
            if ix.deleted != 0 {
                continue; // a journal batch slipped in between; compact again
            }
            if ix.vols.is_empty() {
                return;
            }
            let t0 = Instant::now();
            match persist::save(&ix, &path) {
                Ok(()) => {
                    self.dirty.store(false, Relaxed);
                    log!("saved snapshot: {} entries in {} ms", ix.len(), t0.elapsed().as_millis());
                }
                Err(e) => log!("save failed: {e}"),
            }
            return;
        }
    }
}

enum TailEnd {
    Stop,
    Rescan,
}

fn rec_of(c: &Change) -> u64 {
    match c {
        Change::Delete { rec } | Change::Link { rec, .. } | Change::Upsert { rec, .. } => *rec,
    }
}

fn flags_of(attrs: u32) -> u8 {
    let mut f = 0;
    if attrs & 0x10 != 0 {
        f |= flag::DIR;
    }
    if attrs & 0x2 != 0 {
        f |= flag::HIDDEN;
    }
    if attrs & 0x4 != 0 {
        f |= flag::SYSTEM;
    }
    f
}

/// Apply a journal batch. Changes whose parent folder is not known yet (the folder's own
/// record closes later) are parked in `pending` and retried on the next batches.
pub fn apply(ix: &mut Index, vi: usize, changes: Vec<Change>, pending: &mut Vec<(u32, Change)>) -> Vec<u32> {
    let mut changed = Vec::new();
    for c in changes {
        let rec = rec_of(&c);
        // Any newer event for `rec` supersedes older pending updates for the same record.
        pending.retain(|(_, p)| rec_of(p) != rec);
        if let Err(c) = apply_one(ix, vi, c, &mut changed) {
            pending.push((0, c));
        }
    }
    let retry = std::mem::take(pending);
    for (age, c) in retry {
        if let Err(c) = apply_one(ix, vi, c, &mut changed) {
            if age < 64 {
                pending.push((age + 1, c));
            }
        }
    }
    changed
}

fn apply_one(ix: &mut Index, vi: usize, c: Change, changed: &mut Vec<u32>) -> Result<(), Change> {
    match c {
        Change::Delete { rec } => {
            if rec == ROOT_REC {
                return Ok(());
            }
            let v = &mut ix.vols[vi];
            let id = v.get(rec);
            let links = v.links.remove(&rec).unwrap_or_default();
            if id != NONE {
                v.frn[rec as usize] = NONE;
                ix.delete(id);
                changed.push(id);
            }
            for l in links {
                ix.delete(l);
                changed.push(l);
            }
            Ok(())
        }
        Change::Upsert { rec, parent, ref name, attrs, meta, .. } => {
            let pid = ix.vols[vi].get(parent);
            if pid == NONE || !ix.is_live(pid) {
                return Err(c);
            }
            let fl = flags_of(attrs);
            let id = ix.vols[vi].get(rec);

            // [P1] Protect the volume root from ordinary journal upserts
            if rec == ROOT_REC || id == ix.vols[vi].root {
                if id != NONE && ix.is_live(id) {
                    let i = id as usize;
                    let mut ch = false;
                    let root_fl = fl | flag::DIR;
                    if ix.flags[i] != root_fl {
                        ix.flags[i] = root_fl;
                        ch = true;
                    }
                    if let Some((_, m)) = meta {
                        if ix.mtime[i] != m {
                            ix.mtime[i] = m;
                            ch = true;
                        }
                    }
                    if ch {
                        changed.push(id);
                    }
                }
                return Ok(());
            }

            // [P2] Resolve the affected link: check primary link first, then secondary links in v.links[&rec].
            let target_id = if id != NONE && ix.is_live(id) && ix.parent[id as usize] == pid && ix.name(id) == name {
                Some(id)
            } else if let Some(links) = ix.vols[vi].links.get(&rec) {
                links.iter().copied().find(|&l| l != NONE && ix.is_live(l) && ix.parent[l as usize] == pid && ix.name(l) == name)
            } else {
                None
            };

            let primary_live = id != NONE && ix.is_live(id);
            if let Some(tid) = target_id {
                let i = tid as usize;
                let mut ch = false;
                if ix.flags[i] != fl {
                    ix.flags[i] = fl;
                    ch = true;
                }
                if let Some((s, m)) = meta {
                    let sz = if fl & flag::DIR != 0 { 0 } else { s };
                    if ix.size[i] != sz || ix.mtime[i] != m {
                        ix.size[i] = sz;
                        ix.mtime[i] = m;
                        ch = true;
                    }
                }
                if ch {
                    changed.push(tid);
                }
            } else if primary_live {
                let i = id as usize;
                let mut ch = false;
                if ix.name(id) != name {
                    ix.rename(id, name);
                    ch = true;
                }
                if ix.parent[i] != pid {
                    ix.parent[i] = pid;
                    ch = true;
                }
                if ix.flags[i] != fl {
                    ix.flags[i] = fl;
                    ch = true;
                }
                if let Some((s, m)) = meta {
                    let sz = if fl & flag::DIR != 0 { 0 } else { s };
                    if ix.size[i] != sz || ix.mtime[i] != m {
                        ix.size[i] = sz;
                        ix.mtime[i] = m;
                        ch = true;
                    }
                }
                if ch {
                    changed.push(id);
                }
            } else {
                let (s, m) = meta.unwrap_or((0, 0));
                let sz = if fl & flag::DIR != 0 { 0 } else { s };
                let new_id = ix.push(vi as u8, pid, name, fl, sz, m);
                ix.vols[vi].set(rec, new_id);
                changed.push(new_id);
            }

            // Propagate shared metadata (size, mtime, flags) to all other links of this record
            if let Some((s, m)) = meta {
                let sz = if fl & flag::DIR != 0 { 0 } else { s };
                if primary_live && target_id != Some(id) {
                    let i = id as usize;
                    if ix.size[i] != sz || ix.mtime[i] != m || ix.flags[i] != fl {
                        ix.size[i] = sz;
                        ix.mtime[i] = m;
                        ix.flags[i] = fl;
                        changed.push(id);
                    }
                }
                if let Some(links) = ix.vols[vi].links.get(&rec) {
                    for &l in links {
                        if l != NONE && ix.is_live(l) && Some(l) != target_id {
                            let li = l as usize;
                            if ix.size[li] != sz || ix.mtime[li] != m || ix.flags[li] != fl {
                                ix.size[li] = sz;
                                ix.mtime[li] = m;
                                ix.flags[li] = fl;
                                changed.push(l);
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        Change::Link { rec, parent, ref name, attrs } => {
            if rec == ROOT_REC {
                return Ok(());
            }
            let pid = ix.vols[vi].get(parent);
            if pid == NONE {
                return Ok(());
            }
            let is_it = |ix: &Index, id: u32| id != NONE && ix.is_live(id) && ix.parent[id as usize] == pid && ix.name(id) == name;
            let primary = ix.vols[vi].get(rec);
            let links = ix.vols[vi].links.get(&rec).cloned().unwrap_or_default();
            let already_indexed = is_it(ix, primary) || links.iter().any(|&l| is_it(ix, l));

            // Check if link exists on disk (for real volumes with a valid serial)
            let is_real_vol = ix.vols[vi].serial != 0;
            let link_exists = if is_real_vol {
                let dir = ix.dir_path(pid);
                let full_path = if dir.ends_with('\\') { format!("{dir}{name}") } else { format!("{dir}\\{name}") };
                std::path::Path::new(&full_path).exists()
            } else {
                !already_indexed
            };

            if link_exists {
                if already_indexed {
                    return Ok(());
                }
                let (s, m) = if primary != NONE && ix.is_live(primary) {
                    (ix.size[primary as usize], ix.mtime[primary as usize])
                } else {
                    (0, 0)
                };
                let id = ix.push(vi as u8, pid, name, flags_of(attrs), s, m);
                if primary == NONE || !ix.is_live(primary) {
                    ix.vols[vi].set(rec, id);
                } else {
                    ix.vols[vi].links.entry(rec).or_default().push(id);
                }
                changed.push(id);
            } else if already_indexed {
                if is_it(ix, primary) {
                    ix.delete(primary);
                    changed.push(primary);
                    let promoted = ix.vols[vi].links.get_mut(&rec).and_then(|l| if l.is_empty() { None } else { Some(l.remove(0)) });
                    ix.vols[vi].set(rec, promoted.unwrap_or(NONE));
                } else if let Some(pos) = links.iter().position(|&l| is_it(ix, l)) {
                    let l = links[pos];
                    ix.delete(l);
                    changed.push(l);
                    if let Some(v) = ix.vols[vi].links.get_mut(&rec) {
                        v.retain(|&x| x != l);
                    }
                }
            }
            Ok(())
        }
    }
}

/// Fixed NTFS drives with a letter: (letter, volume serial).
pub fn ntfs_volumes() -> Vec<(u8, u32)> {
    let mut out = Vec::new();
    for l in b'A'..=b'Z' {
        let root = wide(&format!("{}:\\", l as char));
        if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
            continue;
        }
        let mut serial = 0u32;
        let mut fs = [0u16; 32];
        let ok = unsafe {
            GetVolumeInformationW(root.as_ptr(), std::ptr::null_mut(), 0, &mut serial, std::ptr::null_mut(), std::ptr::null_mut(), fs.as_mut_ptr(), fs.len() as u32)
        };
        if ok != 0 && String::from_utf16_lossy(&fs).trim_end_matches('\0') == "NTFS" {
            out.push((l, serial));
        }
    }
    out
}

/// Can we pick up this snapshot volume from its saved journal position?
fn volume_resumable(v: &nothing_core::Volume) -> bool {
    if !ntfs_volumes().iter().any(|&(l, s)| l == v.letter && s == v.serial) {
        return false;
    }
    let Ok(h) = mft::open_volume(v.letter, false) else { return false };
    match usn::query(h.0) {
        Ok(j) => j.id == v.journal_id && v.next_usn >= j.first && v.next_usn <= j.next,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nothing_core::Volume;

    fn fixture() -> Index {
        let mut ix = Index::default();
        let root = ix.push(0, NONE, "C:", flag::DIR, 0, 0);
        let mut v = Volume { letter: b'C', root, ..Default::default() };
        v.set(5, root);
        ix.vols.push(v);
        ix.rebuild_sorts();
        ix
    }

    fn up(rec: u64, parent: u64, name: &str, attrs: u32) -> Change {
        Change::Upsert { rec, frn: rec, parent, name: name.into(), attrs, fetch: false, meta: Some((10, 20)) }
    }

    #[test]
    fn journal_ops() {
        let mut ix = fixture();
        let mut pending = Vec::new();
        // Child arrives before its folder: parked, then resolved by the next batch.
        let ch = apply(&mut ix, 0, vec![up(40, 30, "file.txt", 0x20)], &mut pending);
        assert!(ch.is_empty());
        assert_eq!(pending.len(), 1);
        let ch = apply(&mut ix, 0, vec![up(30, 5, "dir", 0x10)], &mut pending);
        ix.apply_changes(&ch);
        assert!(pending.is_empty());
        let f = ix.vols[0].get(40);
        assert_eq!(ix.dir_path(f), r"C:\dir");
        // Rename + move.
        let ch = apply(&mut ix, 0, vec![up(40, 5, "renamed.txt", 0x20)], &mut pending);
        ix.apply_changes(&ch);
        assert_eq!(ix.name(f), "renamed.txt");
        assert_eq!(ix.dir_path(f), "C:");
        // Hard link add, then remove.
        let link = Change::Link { rec: 40, parent: 30, name: "alias.txt".into(), attrs: 0x20 };
        apply(&mut ix, 0, vec![link], &mut pending);
        assert_eq!(ix.vols[0].links[&40].len(), 1);
        let link = Change::Link { rec: 40, parent: 30, name: "alias.txt".into(), attrs: 0x20 };
        apply(&mut ix, 0, vec![link], &mut pending);
        assert!(ix.vols[0].links[&40].is_empty());
        // Delete.
        let ch = apply(&mut ix, 0, vec![Change::Delete { rec: 40 }], &mut pending);
        ix.apply_changes(&ch);
        assert!(!ix.is_live(f));
        assert_eq!(ix.vols[0].get(40), NONE);
        assert_eq!(ix.by_name.len(), 2);
    }

    #[test]
    fn root_protected_from_upsert() {
        let mut ix = fixture();
        let mut pending = Vec::new();
        let root = ix.vols[0].root;
        // NTFS emits an event for root directory with rec=5 and parent=5
        let ch = apply(&mut ix, 0, vec![up(5, 5, ".", 0x10)], &mut pending);
        ix.apply_changes(&ch);
        assert_eq!(ix.name(root), "C:");
        assert_eq!(ix.parent[root as usize], NONE);
    }

    #[test]
    fn hard_link_upsert_preserves_primary_and_propagates_meta() {
        let mut ix = fixture();
        let mut pending = Vec::new();
        // Create primary a.txt
        let ch = apply(&mut ix, 0, vec![up(40, 5, "a.txt", 0x20)], &mut pending);
        ix.apply_changes(&ch);
        let a = ix.vols[0].get(40);
        assert_eq!(ix.name(a), "a.txt");
        assert_eq!(ix.size[a as usize], 10);

        // Add hard link b.txt
        let link = Change::Link { rec: 40, parent: 5, name: "b.txt".into(), attrs: 0x20 };
        let ch = apply(&mut ix, 0, vec![link], &mut pending);
        ix.apply_changes(&ch);
        let b = ix.vols[0].links[&40][0];
        assert_eq!(ix.name(b), "b.txt");
        assert_eq!(ix.size[b as usize], 10);

        // Upsert targeting b.txt with new size 999
        let upsert_b = Change::Upsert { rec: 40, frn: 40, parent: 5, name: "b.txt".into(), attrs: 0x20, fetch: false, meta: Some((999, 50)) };
        let ch = apply(&mut ix, 0, vec![upsert_b], &mut pending);
        ix.apply_changes(&ch);

        // a.txt must still be named a.txt (not renamed to b.txt), and its size should be updated to 999
        assert_eq!(ix.name(a), "a.txt");
        assert_eq!(ix.size[a as usize], 999);
        assert_eq!(ix.name(b), "b.txt");
        assert_eq!(ix.size[b as usize], 999);
    }

    #[test]
    fn deferred_events_superseded_by_newer_changes() {
        let mut ix = fixture();
        let mut pending = Vec::new();
        // Child arrives before its folder (parent 50 unknown) with old name
        let _ch = apply(&mut ix, 0, vec![up(100, 50, "old.txt", 0x20)], &mut pending);
        assert_eq!(pending.len(), 1);

        // Subsequent successful move/rename to parent 5 with new name
        let ch = apply(&mut ix, 0, vec![up(100, 5, "new.txt", 0x20)], &mut pending);
        ix.apply_changes(&ch);
        let id = ix.vols[0].get(100);
        assert_eq!(ix.name(id), "new.txt");
        assert_eq!(ix.parent[id as usize], ix.vols[0].root);

        // Now parent 50 closes and becomes known
        let ch = apply(&mut ix, 0, vec![up(50, 5, "dir50", 0x10)], &mut pending);
        ix.apply_changes(&ch);

        // Delayed creation must NOT have restored the old name/parent
        assert_eq!(ix.name(id), "new.txt");
        assert_eq!(ix.parent[id as usize], ix.vols[0].root);
    }
}
