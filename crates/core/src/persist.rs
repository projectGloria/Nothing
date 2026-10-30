//! On-disk snapshot of the index: raw little-endian array dumps, so loading is a handful of
//! `read_exact` calls straight into the final allocations (no parsing, no per-entry work).
//! The folded arena is not stored — folding is context-free per char, so folding the
//! whole names arena in one pass reproduces it exactly, in parallel, in a few ms.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use crate::fold::fold_into;
use crate::index::{threads, Index, Volume, NONE};

const MAGIC: &[u8; 8] = b"NTHGIDX1";
const TRAILER: &[u8; 8] = b"NTHGEND!";

/// Marker for plain-old-data element types we dump byte-for-byte.
trait Pod: Copy + Default {}
impl Pod for u8 {}
impl Pod for u16 {}
impl Pod for u32 {}
impl Pod for u64 {}

fn bytes<T: Pod>(v: &[T]) -> &[u8] {
    // SAFETY: Pod types have no padding and any bit pattern is valid.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

fn put<T: Pod>(w: &mut impl Write, v: &[T]) -> io::Result<()> {
    w.write_all(&(v.len() as u64).to_le_bytes())?;
    w.write_all(bytes(v))
}

fn get<T: Pod>(r: &mut impl Read) -> io::Result<Vec<T>> {
    let n = u64_(r)? as usize;
    if n.saturating_mul(std::mem::size_of::<T>()) > 1 << 34 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "array too large"));
    }
    let mut v = vec![T::default(); n];
    // SAFETY: same as `bytes`, writable view of the freshly zeroed allocation.
    let b = unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, n * std::mem::size_of::<T>()) };
    r.read_exact(b)?;
    Ok(v)
}

fn u64_(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// Requires a compacted index (no tombstones), which keeps the file and the load path simple.
pub fn save(ix: &Index, path: &Path) -> io::Result<()> {
    assert_eq!(ix.deleted, 0, "compact before saving");
    let tmp = path.with_extension("tmp");
    {
        let f = File::create(&tmp)?;
        let mut w = BufWriter::with_capacity(1 << 20, f);
        w.write_all(MAGIC)?;
        put(&mut w, &ix.parent)?;
        put(&mut w, &ix.name_off)?;
        put(&mut w, &ix.name_len)?;
        put(&mut w, &ix.flags)?;
        put(&mut w, &ix.vol)?;
        put(&mut w, &ix.size)?;
        put(&mut w, &ix.mtime)?;
        put(&mut w, &ix.names)?;
        put(&mut w, &ix.by_name)?;
        put(&mut w, &ix.by_size)?;
        put(&mut w, &ix.by_date)?;
        w.write_all(&(ix.vols.len() as u64).to_le_bytes())?;
        for v in &ix.vols {
            put(&mut w, &[v.letter as u64, v.serial as u64, v.journal_id, v.next_usn as u64, v.root as u64])?;
            put(&mut w, &v.frn)?;
            let mut flat: Vec<u64> = Vec::new();
            for (rec, ids) in &v.links {
                flat.push(*rec);
                flat.push(ids.len() as u64);
                flat.extend(ids.iter().map(|&i| i as u64));
            }
            put(&mut w, &flat)?;
        }
        w.write_all(TRAILER)?;
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

pub fn load(path: &Path) -> io::Result<Index> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(bad("bad magic"));
    }
    let mut ix = Index {
        parent: get(&mut r)?,
        name_off: get(&mut r)?,
        name_len: get(&mut r)?,
        flags: get(&mut r)?,
        vol: get(&mut r)?,
        size: get(&mut r)?,
        mtime: get(&mut r)?,
        names: get(&mut r)?,
        by_name: get(&mut r)?,
        by_size: get(&mut r)?,
        by_date: get(&mut r)?,
        path_dirty: true,
        ..Default::default()
    };
    let nv = u64_(&mut r)?;
    for _ in 0..nv.min(64) {
        let h: Vec<u64> = get(&mut r)?;
        if h.len() != 5 {
            return Err(bad("volume header"));
        }
        let frn: Vec<u32> = get(&mut r)?;
        let flat: Vec<u64> = get(&mut r)?;
        let mut v = Volume {
            letter: h[0] as u8,
            serial: h[1] as u32,
            journal_id: h[2],
            next_usn: h[3] as i64,
            root: h[4] as u32,
            frn,
            ..Default::default()
        };
        let mut i = 0;
        while i + 1 < flat.len() {
            let (rec, k) = (flat[i], flat[i + 1] as usize);
            let ids = flat.get(i + 2..i + 2 + k).ok_or_else(|| bad("links"))?;
            v.links.insert(rec, ids.iter().map(|&x| x as u32).collect());
            i += 2 + k;
        }
        ix.vols.push(v);
    }
    let mut trailer = [0u8; 8];
    r.read_exact(&mut trailer)?;
    if &trailer != TRAILER {
        return Err(bad("truncated"));
    }
    validate(&ix).map_err(|m| bad(m))?;
    ix.folded = fold_arena(&ix.names);
    Ok(ix)
}

/// A corrupt snapshot must cost a rescan, never a crash: check every index we will
/// later use unchecked.
fn validate(ix: &Index) -> Result<(), &'static str> {
    let n = ix.parent.len();
    if [ix.name_off.len(), ix.name_len.len(), ix.flags.len(), ix.vol.len(), ix.size.len(), ix.mtime.len()].iter().any(|&l| l != n) {
        return Err("array lengths");
    }
    if [ix.by_name.len(), ix.by_size.len(), ix.by_date.len()].iter().any(|&l| l != n) {
        return Err("sort lengths");
    }
    let names = std::str::from_utf8(&ix.names).map_err(|_| "names utf8")?;
    for i in 0..n {
        let (o, l) = (ix.name_off[i] as usize, ix.name_len[i] as usize);
        if o + l > names.len() || !names.is_char_boundary(o) || !names.is_char_boundary(o + l) {
            return Err("name range");
        }
        let p = ix.parent[i];
        if p != NONE && p as usize >= n {
            return Err("parent");
        }
        if ix.vol[i] as usize >= ix.vols.len() {
            return Err("vol");
        }
    }
    for p in [&ix.by_name, &ix.by_size, &ix.by_date] {
        if p.iter().any(|&i| i as usize >= n) {
            return Err("sort ids");
        }
    }
    for v in &ix.vols {
        if v.frn.iter().any(|&i| i != NONE && i as usize >= n) || v.links.values().flatten().any(|&i| i as usize >= n) {
            return Err("frn ids");
        }
    }
    Ok(())
}

/// Fold the whole arena in parallel, splitting on char boundaries.
pub fn fold_arena(names: &[u8]) -> Vec<u8> {
    let s = std::str::from_utf8(names).unwrap_or("");
    let t = threads();
    let per = s.len().div_ceil(t).max(1 << 16);
    let mut cuts = vec![0];
    while *cuts.last().unwrap() < s.len() {
        let mut c = (*cuts.last().unwrap() + per).min(s.len());
        while !s.is_char_boundary(c) {
            c += 1;
        }
        cuts.push(c);
    }
    let parts: Vec<Vec<u8>> = std::thread::scope(|sc| {
        let hs: Vec<_> = cuts
            .windows(2)
            .map(|w| {
                let piece = &s[w[0]..w[1]];
                sc.spawn(move || {
                    let mut v = Vec::with_capacity(piece.len());
                    fold_into(piece, &mut v);
                    v
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut out = Vec::with_capacity(names.len());
    for p in parts {
        out.extend_from_slice(&p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    #[test]
    fn round_trip() {
        let mut ix = sample();
        ix.vols[0].links.insert(42, vec![3, 4]);
        ix.vols[0].set(7, 2);
        ix.vols[0].journal_id = 0xABCD;
        ix.vols[0].next_usn = 123456;
        let p = std::env::temp_dir().join(format!("nothing-test-{}.bin", std::process::id()));
        save(&ix, &p).unwrap();
        let back = load(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(back.names, ix.names);
        assert_eq!(back.folded, ix.folded);
        assert_eq!(back.by_size, ix.by_size);
        assert_eq!(back.vols[0].next_usn, 123456);
        assert_eq!(back.vols[0].links[&42], vec![3, 4]);
        assert_eq!(back.vols[0].get(7), 2);
    }
}
