//! The in-memory file index.
//!
//! Layout is struct-of-arrays keyed by a dense `u32` entry id. A search touches only the
//! `flags` and `folded` arrays, so those stay hot in cache while `size`/`mtime` are never
//! pulled in unless a filter or sort needs them. An array-of-structs layout would drag
//! 30+ bytes of unrelated data through the cache for every name compared.
//!
//! Paths are never stored. A path is rebuilt by walking `parent` up to the volume root,
//! and only for rows that are on screen (or when a query explicitly matches on paths).

use std::cmp::Ordering;
use std::collections::HashMap;
use std::thread;

use crate::fold::fold_into;

pub const NONE: u32 = u32::MAX;

pub mod flag {
    pub const DIR: u8 = 1;
    pub const HIDDEN: u8 = 2;
    pub const SYSTEM: u8 = 4;
    /// Tombstone. Deleted entries keep their id until the next `compact()` so result
    /// sets held by clients stay valid between index updates.
    pub const DELETED: u8 = 8;
}

/// Per-volume bookkeeping: how to map NTFS file reference numbers back to entry ids,
/// and where to resume the USN change journal.
#[derive(Default, Clone, Debug)]
pub struct Volume {
    pub letter: u8,
    pub serial: u32,
    pub journal_id: u64,
    pub next_usn: i64,
    pub root: u32,
    /// MFT record number -> entry id. MFT record numbers are dense (0..record count),
    /// so a flat Vec is both smaller and faster than a HashMap here.
    pub frn: Vec<u32>,
    /// Extra hard links of a record (the first name lives in `frn`). Rare outside WinSxS.
    pub links: HashMap<u64, Vec<u32>>,
}

impl Volume {
    #[inline]
    pub fn get(&self, rec: u64) -> u32 {
        self.frn.get(rec as usize).copied().unwrap_or(NONE)
    }

    pub fn set(&mut self, rec: u64, id: u32) {
        let r = rec as usize;
        if r >= self.frn.len() {
            self.frn.resize(r + 1, NONE);
        }
        self.frn[r] = id;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Sort {
    Name = 0,
    Path = 1,
    Size = 2,
    Date = 3,
}

impl Sort {
    pub fn from_u8(v: u8) -> Sort {
        match v {
            1 => Sort::Path,
            2 => Sort::Size,
            3 => Sort::Date,
            _ => Sort::Name,
        }
    }
}

#[derive(Default)]
pub struct Index {
    pub parent: Vec<u32>,
    pub name_off: Vec<u32>,
    pub name_len: Vec<u16>,
    pub flags: Vec<u8>,
    pub vol: Vec<u8>,
    pub size: Vec<u64>,
    /// FILETIME (100ns ticks since 1601, UTC).
    pub mtime: Vec<u64>,
    /// Original UTF-8 names, back to back.
    pub names: Vec<u8>,
    /// Case-folded twin of `names`, byte-for-byte the same length (see `fold`), so
    /// `name_off`/`name_len` index both arenas.
    pub folded: Vec<u8>,
    pub vols: Vec<Volume>,

    // Presorted permutations of live ids. A query never sorts: it filters one of these.
    pub by_name: Vec<u32>,
    pub by_size: Vec<u32>,
    pub by_date: Vec<u32>,
    /// Built lazily (most people never sort by path) and invalidated by any change.
    pub by_path: Vec<u32>,
    pub path_dirty: bool,

    pub deleted: usize,
    /// Bytes in the arenas no longer referenced (renames/deletes). Drives compaction.
    pub garbage: usize,
    /// Bumped whenever ids are renumbered (compaction). Client result sets from an older
    /// epoch hold ids that now point at different files and must be discarded.
    pub epoch: u64,
    /// Bumped on every visible change; clients poll it to know when to re-query.
    pub version: u64,
}

impl Index {
    #[inline]
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    #[inline]
    pub fn live(&self) -> usize {
        self.len() - self.deleted
    }

    #[inline]
    pub fn is_live(&self, id: u32) -> bool {
        self.flags[id as usize] & flag::DELETED == 0
    }

    #[inline]
    pub fn is_dir(&self, id: u32) -> bool {
        self.flags[id as usize] & flag::DIR != 0
    }

    #[inline]
    pub fn name(&self, id: u32) -> &str {
        let o = self.name_off[id as usize] as usize;
        let l = self.name_len[id as usize] as usize;
        // SAFETY: the arena only ever receives bytes copied from `&str`s.
        unsafe { std::str::from_utf8_unchecked(&self.names[o..o + l]) }
    }

    #[inline]
    pub fn folded(&self, id: u32) -> &[u8] {
        let o = self.name_off[id as usize] as usize;
        let l = self.name_len[id as usize] as usize;
        &self.folded[o..o + l]
    }

    pub fn vol_index(&self, letter: u8) -> Option<usize> {
        self.vols.iter().position(|v| v.letter == letter)
    }

    fn push_name(&mut self, name: &str) -> (u32, u16) {
        // NTFS caps names at 255 UTF-16 units (<= 765 UTF-8 bytes), so u16 never truncates
        // real names; the clamp only guards against garbage input.
        let mut end = name.len().min(u16::MAX as usize);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let name = &name[..end];
        let off = self.names.len() as u32;
        self.names.extend_from_slice(name.as_bytes());
        fold_into(name, &mut self.folded);
        (off, name.len() as u16)
    }

    pub fn push(&mut self, vol: u8, parent: u32, name: &str, flags: u8, size: u64, mtime: u64) -> u32 {
        let id = self.parent.len() as u32;
        let (off, len) = self.push_name(name);
        self.parent.push(parent);
        self.name_off.push(off);
        self.name_len.push(len);
        self.flags.push(flags);
        self.vol.push(vol);
        self.size.push(size);
        self.mtime.push(mtime);
        id
    }

    pub fn rename(&mut self, id: u32, name: &str) {
        if self.name(id) == name {
            return;
        }
        self.garbage += self.name_len[id as usize] as usize;
        let (off, len) = self.push_name(name);
        self.name_off[id as usize] = off;
        self.name_len[id as usize] = len;
    }

    pub fn delete(&mut self, id: u32) {
        let i = id as usize;
        if self.flags[i] & flag::DELETED == 0 {
            self.flags[i] |= flag::DELETED;
            self.deleted += 1;
            self.garbage += self.name_len[i] as usize;
        }
    }

    /// Append the path of `id` (root name, then each folder, then `id`'s own name, joined
    /// by `\`) to `out`. `folded` picks the case-folded arena (for matching).
    pub fn path_into(&self, id: u32, folded: bool, chain: &mut Vec<u32>, out: &mut Vec<u8>) {
        chain.clear();
        let mut cur = id;
        // Depth guard: a corrupt parent cycle must not hang a search thread.
        while cur != NONE && chain.len() < 1024 {
            chain.push(cur);
            cur = self.parent[cur as usize];
        }
        let arena = if folded { &self.folded } else { &self.names };
        for (i, &c) in chain.iter().rev().enumerate() {
            if i > 0 {
                out.push(b'\\');
            }
            let o = self.name_off[c as usize] as usize;
            out.extend_from_slice(&arena[o..o + self.name_len[c as usize] as usize]);
        }
    }

    /// Display path of the folder containing `id` ("C:\Users\x"); empty for a volume root.
    pub fn dir_path(&self, id: u32) -> String {
        let p = self.parent[id as usize];
        if p == NONE {
            return String::new();
        }
        let mut out = Vec::with_capacity(96);
        self.path_into(p, false, &mut Vec::with_capacity(16), &mut out);
        String::from_utf8(out).unwrap_or_default()
    }

    // ---- ordering -------------------------------------------------------------------

    /// Folders first, then case-insensitive name, then exact name, then id (total order,
    /// which binary-search insertion relies on).
    #[inline]
    pub fn cmp_name(&self, a: u32, b: u32) -> Ordering {
        self.is_dir(b)
            .cmp(&self.is_dir(a))
            .then_with(|| self.folded(a).cmp(self.folded(b)))
            .then_with(|| self.name(a).cmp(self.name(b)))
            .then(a.cmp(&b))
    }

    #[inline]
    fn cmp_size(&self, a: u32, b: u32) -> Ordering {
        self.size[a as usize].cmp(&self.size[b as usize]).then_with(|| self.cmp_name(a, b))
    }

    #[inline]
    fn cmp_date(&self, a: u32, b: u32) -> Ordering {
        self.mtime[a as usize].cmp(&self.mtime[b as usize]).then_with(|| self.cmp_name(a, b))
    }

    /// Full rebuild of the name/size/date orders. ~1.4M entries: a few hundred ms, done
    /// once at startup; afterwards `apply_changes` keeps them sorted incrementally.
    pub fn rebuild_sorts(&mut self) {
        let n = self.len();
        let live: Vec<u32> = (0..n as u32).filter(|&i| self.is_live(i)).collect();

        let mut by_name = live.clone();
        par_sort_by(&mut by_name, |&a, &b| self.cmp_name(a, b));

        // Rank of each id in name order. Size/date sorts then use (key, rank) packed into a
        // u128: comparing plain integers is far cheaper than chasing two name strings.
        let mut rank = vec![0u32; n];
        for (i, &id) in by_name.iter().enumerate() {
            rank[id as usize] = i as u32;
        }
        let keyed = |key: &[u64]| -> Vec<u32> {
            let mut k: Vec<u128> = live
                .iter()
                .map(|&id| ((key[id as usize] as u128) << 32) | rank[id as usize] as u128)
                .collect();
            par_sort_by(&mut k, |a, b| a.cmp(b));
            k.into_iter().map(|x| by_name[(x as u32) as usize]).collect()
        };
        let (by_size, by_date) = thread::scope(|s| {
            let a = s.spawn(|| keyed(&self.size));
            let b = keyed(&self.mtime);
            (a.join().unwrap(), b)
        });

        self.by_name = by_name;
        self.by_size = by_size;
        self.by_date = by_date;
        self.by_path = Vec::new();
        self.path_dirty = true;
        self.version += 1;
    }

    /// Re-slot `changed` ids (new, renamed, resized, deleted) into the presorted orders.
    /// Cost is O(n) memcpy + O(k log n) comparisons, i.e. ~ms per batch instead of a
    /// full re-sort. Large bursts (unzipping a big archive) fall back to a rebuild.
    pub fn apply_changes(&mut self, changed: &[u32]) {
        if changed.is_empty() {
            return;
        }
        self.version += 1;
        self.path_dirty = true;
        if changed.len() > 4096 && changed.len() > self.live() / 32 {
            self.rebuild_sorts();
            return;
        }
        let mut mark = vec![0u64; self.len().div_ceil(64)];
        let mut ins = Vec::with_capacity(changed.len());
        for &id in changed {
            let (w, b) = ((id / 64) as usize, id % 64);
            if mark[w] & (1 << b) == 0 {
                mark[w] |= 1 << b;
                if self.is_live(id) {
                    ins.push(id);
                }
            }
        }
        let marked = |id: u32| mark[(id / 64) as usize] & (1 << (id % 64)) != 0;

        let mut by_name = std::mem::take(&mut self.by_name);
        let mut by_size = std::mem::take(&mut self.by_size);
        let mut by_date = std::mem::take(&mut self.by_date);
        for (perm, which) in [(&mut by_name, 0), (&mut by_size, 1), (&mut by_date, 2)] {
            perm.retain(|&id| !marked(id));
            let cmp = |a: u32, b: u32| match which {
                0 => self.cmp_name(a, b),
                1 => self.cmp_size(a, b),
                _ => self.cmp_date(a, b),
            };
            ins.sort_unstable_by(|&a, &b| cmp(a, b));
            merge_into(perm, &ins, cmp);
        }
        self.by_name = by_name;
        self.by_size = by_size;
        self.by_date = by_date;
    }

    /// Path order = depth-first walk of the folder tree with siblings in name order, i.e.
    /// sort by (rank of parent folder in DFS order, files-after-folders, name rank). All
    /// three fit in one u64, so this is an integer sort: ~50 ms for 1.4M entries.
    pub fn ensure_path_sort(&mut self) {
        if !self.path_dirty && self.by_path.len() == self.by_name.len() {
            return;
        }
        let n = self.len();
        let mut rank = vec![0u32; n];
        for (i, &id) in self.by_name.iter().enumerate() {
            rank[id as usize] = i as u32;
        }
        // Child-folder lists in CSR form. Filling them while walking `by_name` leaves each
        // list already in name order, so the DFS needs no sorting at all.
        let mut start = vec![0u32; n + 1];
        for &id in &self.by_name {
            let p = self.parent[id as usize];
            if self.is_dir(id) && p != NONE {
                start[p as usize + 1] += 1;
            }
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let mut fill = start.clone();
        let mut kids = vec![0u32; start[n] as usize];
        for &id in &self.by_name {
            let p = self.parent[id as usize];
            if self.is_dir(id) && p != NONE {
                kids[fill[p as usize] as usize] = id;
                fill[p as usize] += 1;
            }
        }
        let mut drank = vec![u32::MAX; n];
        let mut next = 1u32;
        let mut stack: Vec<u32> = self
            .by_name
            .iter()
            .rev()
            .copied()
            .filter(|&id| self.is_dir(id) && self.parent[id as usize] == NONE)
            .collect();
        while let Some(d) = stack.pop() {
            drank[d as usize] = next;
            next += 1;
            let (a, b) = (start[d as usize] as usize, start[d as usize + 1] as usize);
            stack.extend(kids[a..b].iter().rev());
        }
        let mut keys: Vec<u64> = self
            .by_name
            .iter()
            .map(|&id| {
                let p = self.parent[id as usize];
                let pr = if p == NONE { 0 } else { drank[p as usize].min(0x7FFF_FFFF) as u64 };
                (pr << 33) | ((!self.is_dir(id) as u64) << 32) | rank[id as usize] as u64
            })
            .collect();
        par_sort_by(&mut keys, |a, b| a.cmp(b));
        self.by_path = keys.into_iter().map(|k| self.by_name[(k as u32) as usize]).collect();
        self.path_dirty = false;
    }

    pub fn order(&self, sort: Sort) -> &[u32] {
        match sort {
            Sort::Name => &self.by_name,
            Sort::Path => {
                if self.path_dirty {
                    &self.by_name
                } else {
                    &self.by_path
                }
            }
            Sort::Size => &self.by_size,
            Sort::Date => &self.by_date,
        }
    }

    // ---- structural operations --------------------------------------------------------

    /// Append another index (typically one freshly scanned volume), renumbering its ids.
    /// The caller rebuilds sorts afterwards.
    pub fn append(&mut self, other: Index) {
        let id_off = self.len() as u32;
        let name_off = self.names.len() as u32;
        let vol_off = self.vols.len() as u8;
        let shift = |p: u32| if p == NONE { NONE } else { p + id_off };
        self.parent.extend(other.parent.iter().map(|&p| shift(p)));
        self.name_off.extend(other.name_off.iter().map(|&o| o + name_off));
        self.name_len.extend_from_slice(&other.name_len);
        self.flags.extend_from_slice(&other.flags);
        self.vol.extend(other.vol.iter().map(|&v| v + vol_off));
        self.size.extend_from_slice(&other.size);
        self.mtime.extend_from_slice(&other.mtime);
        self.names.extend_from_slice(&other.names);
        self.folded.extend_from_slice(&other.folded);
        for mut v in other.vols {
            v.root = shift(v.root);
            for f in v.frn.iter_mut() {
                *f = shift(*f);
            }
            for ids in v.links.values_mut() {
                for i in ids.iter_mut() {
                    *i = shift(*i);
                }
            }
            self.vols.push(v);
        }
        self.deleted += other.deleted;
        self.garbage += other.garbage;
        self.version += 1;
    }

    /// Drop a whole volume (unplugged, or about to be rescanned).
    pub fn remove_volume(&mut self, vi: usize) {
        for id in 0..self.len() as u32 {
            let v = self.vol[id as usize] as usize;
            if v == vi {
                self.delete(id);
            } else if v > vi {
                self.vol[id as usize] -= 1;
            }
        }
        self.vols.remove(vi);
        self.compact();
    }

    /// Physically drop tombstones and dead arena bytes, renumbering ids. Order of every
    /// presorted permutation is preserved, so no re-sort is needed.
    pub fn compact(&mut self) {
        if self.deleted == 0 && self.garbage == 0 {
            return;
        }
        let n = self.len();
        let mut map = vec![NONE; n];
        let mut next = 0u32;
        for id in 0..n {
            if self.flags[id] & flag::DELETED == 0 {
                map[id] = next;
                next += 1;
            }
        }
        let m = next as usize;
        let remap = |p: u32| if p == NONE { NONE } else { map[p as usize] };

        let mut out = Index {
            parent: Vec::with_capacity(m),
            name_off: Vec::with_capacity(m),
            name_len: Vec::with_capacity(m),
            flags: Vec::with_capacity(m),
            vol: Vec::with_capacity(m),
            size: Vec::with_capacity(m),
            mtime: Vec::with_capacity(m),
            names: Vec::with_capacity(self.names.len() - self.garbage.min(self.names.len())),
            folded: Vec::new(),
            ..Default::default()
        };
        out.folded.reserve(out.names.capacity());
        for id in 0..n {
            if map[id] == NONE {
                continue;
            }
            let (o, l) = (self.name_off[id] as usize, self.name_len[id] as usize);
            out.name_off.push(out.names.len() as u32);
            out.name_len.push(l as u16);
            out.names.extend_from_slice(&self.names[o..o + l]);
            out.folded.extend_from_slice(&self.folded[o..o + l]);
            out.parent.push(remap(self.parent[id]));
            out.flags.push(self.flags[id]);
            out.vol.push(self.vol[id]);
            out.size.push(self.size[id]);
            out.mtime.push(self.mtime[id]);
        }
        for v in self.vols.iter_mut() {
            v.root = remap(v.root);
            for f in v.frn.iter_mut() {
                *f = remap(*f);
            }
            v.links.retain(|_, ids| {
                ids.retain_mut(|i| {
                    *i = remap(*i);
                    *i != NONE
                });
                !ids.is_empty()
            });
        }
        let fix = |p: &mut Vec<u32>| {
            let mut v: Vec<u32> = p.iter().map(|&i| remap(i)).filter(|&i| i != NONE).collect();
            v.shrink_to_fit();
            *p = v;
        };
        fix(&mut self.by_name);
        fix(&mut self.by_size);
        fix(&mut self.by_date);
        fix(&mut self.by_path);

        self.parent = out.parent;
        self.name_off = out.name_off;
        self.name_len = out.name_len;
        self.flags = out.flags;
        self.vol = out.vol;
        self.size = out.size;
        self.mtime = out.mtime;
        self.names = out.names;
        self.folded = out.folded;
        self.deleted = 0;
        self.garbage = 0;
        self.epoch += 1;
        self.version += 1;
    }

    /// Approximate heap bytes held (for diagnostics / the bytes-per-entry budget).
    pub fn heap_bytes(&self) -> usize {
        let v = |c: usize, sz: usize| c * sz;
        v(self.parent.capacity(), 4)
            + v(self.name_off.capacity(), 4)
            + v(self.name_len.capacity(), 2)
            + self.flags.capacity()
            + self.vol.capacity()
            + v(self.size.capacity(), 8)
            + v(self.mtime.capacity(), 8)
            + self.names.capacity()
            + self.folded.capacity()
            + v(self.by_name.capacity() + self.by_size.capacity() + self.by_date.capacity() + self.by_path.capacity(), 4)
            + self.vols.iter().map(|v| v.frn.capacity() * 4 + v.links.len() * 48).sum::<usize>()
    }

    pub fn shrink(&mut self) {
        self.parent.shrink_to_fit();
        self.name_off.shrink_to_fit();
        self.name_len.shrink_to_fit();
        self.flags.shrink_to_fit();
        self.vol.shrink_to_fit();
        self.size.shrink_to_fit();
        self.mtime.shrink_to_fit();
        self.names.shrink_to_fit();
        self.folded.shrink_to_fit();
        for v in self.vols.iter_mut() {
            v.frn.shrink_to_fit();
        }
    }
}

/// Merge sorted `ins` into sorted `perm`. Binary search finds each insertion point, then
/// whole runs of `perm` are block-copied, so comparisons are O(k log n), not O(n).
fn merge_into(perm: &mut Vec<u32>, ins: &[u32], cmp: impl Fn(u32, u32) -> Ordering) {
    if ins.is_empty() {
        return;
    }
    let mut out = Vec::with_capacity(perm.len() + ins.len());
    let mut last = 0;
    for &x in ins {
        let p = last + perm[last..].partition_point(|&y| cmp(y, x) == Ordering::Less);
        out.extend_from_slice(&perm[last..p]);
        out.push(x);
        last = p;
    }
    out.extend_from_slice(&perm[last..]);
    *perm = out;
}

pub fn threads() -> usize {
    thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(64)
}

/// Parallel sort: sort per-thread chunks, then merge pairs of runs in parallel per level.
pub fn par_sort_by<T, F>(v: &mut Vec<T>, f: F)
where
    T: Copy + Send + Sync,
    F: Fn(&T, &T) -> Ordering + Sync,
{
    let n = v.len();
    let t = threads();
    if n < 65_536 || t < 2 {
        v.sort_unstable_by(&f);
        return;
    }
    let chunk = n.div_ceil(t);
    let f = &f;
    thread::scope(|s| {
        for c in v.chunks_mut(chunk) {
            s.spawn(move || c.sort_unstable_by(f));
        }
    });
    let mut runs: Vec<(usize, usize)> = (0..n).step_by(chunk).map(|a| (a, (a + chunk).min(n))).collect();
    let mut src = std::mem::take(v);
    let mut dst = src.clone();
    while runs.len() > 1 {
        let mut next_runs = Vec::with_capacity(runs.len().div_ceil(2));
        {
            let src_ref = &src;
            let mut rest: &mut [T] = &mut dst[..];
            thread::scope(|s| {
                for pair in runs.chunks(2) {
                    let (start, end) = (pair[0].0, pair[pair.len() - 1].1);
                    let (head, tail) = std::mem::take(&mut rest).split_at_mut(end - start);
                    rest = tail;
                    next_runs.push((start, end));
                    if pair.len() == 2 {
                        let (a, b) = (&src_ref[pair[0].0..pair[0].1], &src_ref[pair[1].0..pair[1].1]);
                        s.spawn(move || merge_runs(a, b, head, f));
                    } else {
                        head.copy_from_slice(&src_ref[start..end]);
                    }
                }
            });
        }
        std::mem::swap(&mut src, &mut dst);
        runs = next_runs;
    }
    *v = src;
}

fn merge_runs<T: Copy, F: Fn(&T, &T) -> Ordering>(a: &[T], b: &[T], out: &mut [T], f: &F) {
    let (mut i, mut j, mut k) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if f(&b[j], &a[i]) == Ordering::Less {
            out[k] = b[j];
            j += 1;
        } else {
            out[k] = a[i];
            i += 1;
        }
        k += 1;
    }
    out[k..k + a.len() - i].copy_from_slice(&a[i..]);
    k += a.len() - i;
    out[k..].copy_from_slice(&b[j..]);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// C: \ {Users\{a.txt, B.txt, docs\{z.pdf}}, Windows\{notepad.exe}}
    pub fn sample() -> Index {
        let mut ix = Index::default();
        let root = ix.push(0, NONE, "C:", flag::DIR, 0, 0);
        let users = ix.push(0, root, "Users", flag::DIR, 0, 50);
        ix.push(0, users, "a.txt", 0, 300, 10);
        ix.push(0, users, "B.txt", 0, 100, 30);
        let docs = ix.push(0, users, "docs", flag::DIR, 0, 20);
        ix.push(0, docs, "z.pdf", 0, 200, 40);
        let win = ix.push(0, root, "Windows", flag::DIR, 0, 60);
        ix.push(0, win, "notepad.exe", 0, 50, 5);
        ix.vols.push(Volume { letter: b'C', root, ..Default::default() });
        ix.rebuild_sorts();
        ix
    }

    fn names(ix: &Index, p: &[u32]) -> Vec<String> {
        p.iter().map(|&i| ix.name(i).to_string()).collect()
    }

    #[test]
    fn sorts() {
        let mut ix = sample();
        assert_eq!(names(&ix, &ix.by_name), ["C:", "docs", "Users", "Windows", "a.txt", "B.txt", "notepad.exe", "z.pdf"]);
        assert_eq!(names(&ix, &ix.by_size)[4..], ["notepad.exe", "B.txt", "z.pdf", "a.txt"]);
        ix.ensure_path_sort();
        assert_eq!(
            names(&ix, &ix.by_path),
            ["C:", "Users", "Windows", "docs", "a.txt", "B.txt", "z.pdf", "notepad.exe"]
        );
        let mut buf = Vec::new();
        ix.path_into(5, false, &mut Vec::new(), &mut buf);
        assert_eq!(std::str::from_utf8(&buf).unwrap(), r"C:\Users\docs\z.pdf");
        assert_eq!(ix.dir_path(5), r"C:\Users\docs");
    }

    #[test]
    fn incremental_matches_rebuild() {
        let mut ix = sample();
        let n = ix.push(0, 1, "aaa.txt", 0, 1000, 99);
        ix.rename(3, "zz.txt");
        ix.size[7] = 5000;
        ix.delete(2);
        ix.apply_changes(&[n, 3, 7, 2]);
        let (a, b, c) = (ix.by_name.clone(), ix.by_size.clone(), ix.by_date.clone());
        ix.rebuild_sorts();
        assert_eq!(a, ix.by_name);
        assert_eq!(b, ix.by_size);
        assert_eq!(c, ix.by_date);
        ix.compact();
        assert_eq!(ix.live(), ix.len());
        assert_eq!(names(&ix, &ix.by_name), ["C:", "docs", "Users", "Windows", "aaa.txt", "notepad.exe", "z.pdf", "zz.txt"]);
    }

    #[test]
    fn par_sort_large() {
        let mut v: Vec<u64> = (0..300_000u64).map(|i| i.wrapping_mul(2654435761) % 1_000_003).collect();
        let mut w = v.clone();
        par_sort_by(&mut v, |a, b| a.cmp(b));
        w.sort();
        assert_eq!(v, w);
    }
}
