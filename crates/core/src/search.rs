//! Query language + parallel matcher.
//!
//! Syntax (Everything-compatible subset):
//!   `foo bar`      AND           `foo | bar`   OR (binds tighter than AND)
//!   `!foo`         NOT           `"a b"`       literal phrase with spaces
//!   `*.mkv` `a?c`  wildcards, whole-name match
//!   `\` in a term  match against the full path instead of the name
//!   `ext:jpg;png`  `file:` `folder:` `case:` `path:`   modifiers, stackable (`case:path:Foo`)
//!
//! Execution: every live entry is tested on all cores into a bitset, then the presorted
//! permutation for the requested column is walked and filtered by that bitset. Sorting a
//! result set therefore costs nothing — it is born sorted.

use memchr::memmem::Finder;

use crate::fold::fold;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

use crate::index::{flag, threads, Index, Sort, NONE};
use crate::pool::par_chunks_mut;

pub const OPT_CASE: u8 = 1;
pub const OPT_PATH: u8 = 2;

enum Pat {
    All,
    Sub(Finder<'static>),
    Wild(Vec<u8>),
    Ext(Vec<Vec<u8>>),
}

#[derive(Clone, Copy, PartialEq)]
enum Only {
    Any,
    Files,
    Folders,
}

struct Term {
    /// Path substring terms: index of this term's per-folder memo (see `Ctx::path_hit`).
    slot: usize,
    /// Wildcards only: the longest literal run of the pattern, used as a memmem prefilter.
    pre: Option<Finder<'static>>,
    neg: bool,
    case: bool,
    path: bool,
    only: Only,
    pat: Pat,
}

pub struct Query {
    /// Conjunction of disjunctions: every group must have at least one matching term.
    groups: Vec<Vec<Term>>,
    /// If the query is nothing but positive, case-insensitive name substrings, these are
    /// the needles. Used to prove a new query only narrows the previous one.
    plain: Option<Vec<Vec<u8>>>,
    path_slots: usize,
}

fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut had_quote = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                had_quote = true;
            }
            '|' if !quoted => {
                if !cur.is_empty() || had_quote {
                    out.push(std::mem::take(&mut cur));
                }
                had_quote = false;
                out.push("|".into());
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() || had_quote {
                    out.push(std::mem::take(&mut cur));
                }
                had_quote = false;
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn strip_prefix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    if s.len() >= p.len() && s.is_char_boundary(p.len()) && s[..p.len()].eq_ignore_ascii_case(p) {
        Some(&s[p.len()..])
    } else {
        None
    }
}

impl Query {
    pub fn parse(text: &str, opts: u8) -> Query {
        let mut groups: Vec<Vec<Term>> = Vec::new();
        let mut join_next = false;
        for tok in tokenize(text) {
            if tok == "|" {
                join_next = !groups.is_empty();
                continue;
            }
            let mut t = Term {
                slot: 0,
                pre: None,
                neg: false,
                case: opts & OPT_CASE != 0,
                path: opts & OPT_PATH != 0,
                only: Only::Any,
                pat: Pat::All,
            };
            let mut v: &str = &tok;
            while let Some(r) = v.strip_prefix('!') {
                t.neg = !t.neg;
                v = r;
            }
            let mut ext = None;
            loop {
                if let Some(r) = strip_prefix_ci(v, "case:") {
                    t.case = true;
                    v = r;
                } else if let Some(r) = strip_prefix_ci(v, "nocase:") {
                    t.case = false;
                    v = r;
                } else if let Some(r) = strip_prefix_ci(v, "path:") {
                    t.path = true;
                    v = r;
                } else if let Some(r) = strip_prefix_ci(v, "file:") {
                    t.only = Only::Files;
                    v = r;
                } else if let Some(r) = strip_prefix_ci(v, "folder:") {
                    t.only = Only::Folders;
                    v = r;
                } else if let Some(r) = strip_prefix_ci(v, "ext:") {
                    ext = Some(r);
                    v = "";
                    break;
                } else {
                    break;
                }
            }
            if v.contains('\\') {
                t.path = true;
            }
            let norm = |s: &str| -> Vec<u8> {
                if t.case {
                    s.as_bytes().to_vec()
                } else {
                    fold(s).into_bytes()
                }
            };
            t.pat = if let Some(e) = ext {
                t.only = Only::Files;
                Pat::Ext(e.split([';', ',']).filter(|x| !x.is_empty()).map(|x| fold(x.trim_start_matches('.')).into_bytes()).collect())
            } else if v.is_empty() {
                Pat::All
            } else if v.contains(['*', '?']) {
                let p = norm(v);
                t.pre = p.split(|&c| c == b'*' || c == b'?').max_by_key(|l| l.len()).filter(|l| l.len() >= 2).map(|l| Finder::new(l).into_owned());
                Pat::Wild(p)
            } else {
                Pat::Sub(Finder::new(&norm(v)).into_owned())
            };
            if join_next {
                groups.last_mut().unwrap().push(t);
            } else {
                groups.push(vec![t]);
            }
            join_next = false;
        }
        // Drop groups that can never filter anything (bare `file:` aside, `All` without
        // modifiers is a no-op), so an empty query hits the "everything" fast path.
        groups.retain(|g| !(g.len() == 1 && matches!(g[0].pat, Pat::All) && !g[0].neg && g[0].only == Only::Any));

        let plain = groups
            .iter()
            .map(|g| match &g[..] {
                [t] if !t.neg && !t.case && !t.path && t.only == Only::Any => match &t.pat {
                    Pat::Sub(f) => Some(f.needle().to_vec()),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Option<Vec<_>>>();
        let mut path_slots = 0;
        for t in groups.iter_mut().flatten() {
            if t.path && matches!(t.pat, Pat::Sub(_)) {
                t.slot = path_slots;
                path_slots += 1;
            }
        }
        Query { groups, plain, path_slots }
    }

    pub fn is_all(&self) -> bool {
        self.groups.is_empty()
    }

    /// The most selective single-term name-substring group, if any: it can drive the
    /// arena scan, with the remaining groups checked only on its hits.
    /// Returns (finder, case-sensitive, hit-implies-match).
    fn lead(&self) -> Option<(&Finder<'static>, bool, bool)> {
        self.groups
            .iter()
            .filter_map(|g| match &g[..] {
                [t] if !t.neg && !t.path && t.only == Only::Any => match (&t.pat, &t.pre) {
                    (Pat::Sub(f), _) => Some((f, t.case, true)),
                    (Pat::Wild(_), Some(f)) => Some((f, t.case, false)),
                    _ => None,
                },
                _ => None,
            })
            .max_by_key(|(f, _, exact)| (f.needle().len(), *exact))
    }

    fn memos(&self, n: usize) -> Vec<Vec<AtomicU64>> {
        // vec![0] comes from calloc (pre-zeroed pages); AtomicU64 has u64's exact layout.
        (0..self.path_slots)
            .map(|_| {
                let mut v = std::mem::ManuallyDrop::new(vec![0u64; n]);
                unsafe { Vec::from_raw_parts(v.as_mut_ptr() as *mut AtomicU64, v.len(), v.capacity()) }
            })
            .collect()
    }

    fn needs_path(&self) -> bool {
        self.groups.iter().flatten().any(|t| t.path)
    }

    /// True if every result of `self` is guaranteed to be a result of `prev`: each of
    /// prev's needles occurs inside some needle of ours (typing more characters).
    pub fn narrows(&self, prev: &Query) -> bool {
        match (&self.plain, &prev.plain) {
            (Some(a), Some(b)) => b.iter().all(|nb| a.iter().any(|na| memchr::memmem::find(na, nb).is_some())),
            _ => false,
        }
    }
}

/// Per-thread scratch so path matching never allocates in the hot loop.
struct Ctx<'a> {
    ix: &'a Index,
    /// Per path-term, per folder memo words (see `M_*`); 0 = not computed yet. Shared by
    /// all threads: racing writers store the same value, so Relaxed is enough.
    memos: &'a [Vec<AtomicU64>],
    tail: Vec<u8>,
    chain: Vec<u32>,
    path_f: Vec<u8>,
    path_o: Vec<u8>,
    have_f: bool,
    have_o: bool,
}

impl<'a> Ctx<'a> {
    fn new(ix: &'a Index, memos: &'a [Vec<AtomicU64>]) -> Self {
        Ctx { ix, memos, tail: Vec::with_capacity(512), chain: Vec::with_capacity(32), path_f: Vec::with_capacity(256), path_o: Vec::with_capacity(256), have_f: false, have_o: false }
    }

    #[inline]
    fn matches(&mut self, q: &Query, id: u32) -> bool {
        self.have_f = false;
        self.have_o = false;
        q.groups.iter().all(|g| g.iter().any(|t| self.term(t, id)))
    }

    #[inline]
    fn term(&mut self, t: &Term, id: u32) -> bool {
        let ix = self.ix;
        let is_dir = ix.flags[id as usize] & flag::DIR != 0;
        let ok = match t.only {
            Only::Files => !is_dir,
            Only::Folders => is_dir,
            Only::Any => true,
        } && {
            if let (Pat::Sub(f), true) = (&t.pat, t.path) {
                return self.path_hit(id, f, t.case, t.slot) != t.neg;
            }
            let hay: &[u8] = match (&t.pat, t.path, t.case) {
                (Pat::All, ..) | (Pat::Ext(_), ..) => &[],
                (_, false, false) => ix.folded(id),
                (_, false, true) => ix.name(id).as_bytes(),
                (_, true, false) => {
                    if !self.have_f {
                        self.path_f.clear();
                        ix.path_into(id, true, &mut self.chain, &mut self.path_f);
                        self.have_f = true;
                    }
                    &self.path_f
                }
                (_, true, true) => {
                    if !self.have_o {
                        self.path_o.clear();
                        ix.path_into(id, false, &mut self.chain, &mut self.path_o);
                        self.have_o = true;
                    }
                    &self.path_o
                }
            };
            match &t.pat {
                Pat::All => true,
                Pat::Sub(f) => f.find(hay).is_some(),
                Pat::Wild(p) => glob(p, hay),
                Pat::Ext(set) => {
                    let n = ix.folded(id);
                    match memchr::memrchr(b'.', n) {
                        Some(i) => set.iter().any(|e| e[..] == n[i + 1..]),
                        None => false,
                    }
                }
            }
        };
        ok != t.neg
    }
}

/// Per-folder memo word for path-substring terms.
const M_DONE: u64 = 1 << 63;
const M_MATCH: u64 = 1 << 62;
/// `<folder path>\` itself ends with the whole needle (e.g. needle `users\`): the folder
/// doesn't match but every child does.
const M_KIDS: u64 = 1;
/// Bits 1..=61: bit k set <=> the last k bytes of `<folder path>\` equal the needle's
/// first k bytes, i.e. a match could start in the folder path and finish in a child name.
const M_CROSS: u64 = (1 << 62) - 2;

impl Ctx<'_> {
    /// Path substring match without building paths. A path `<folder>\<name>` matches iff
    /// the folder path matches, or the needle lies in `name`, or it starts in the tail of
    /// the folder path and finishes at the start of `name`. Folder answers (match flag +
    /// "crossing" bitmask) are memoized once per folder and shared by all threads, so a
    /// file costs one memo load plus one scan of its own short name.
    fn path_hit(&mut self, id: u32, f: &Finder, case: bool, slot: usize) -> bool {
        if f.needle().len() > 61 {
            return self.full_path_hit(id, f, case); // crossing mask can't hold it; rare
        }
        let p = self.ix.parent[id as usize];
        let name = name_of(self.ix, id, case);
        if p == NONE {
            return f.find(name).is_some();
        }
        let m = self.dir_memo(p, f, case, slot, 0);
        m & (M_MATCH | M_KIDS) != 0 || crosses(name, f, m)
    }


    fn dir_memo(&mut self, d: u32, f: &Finder, case: bool, slot: usize, depth: u32) -> u64 {
        let v = self.memos[slot][d as usize].load(Relaxed);
        if v != 0 {
            return v;
        }
        // No "in progress" marker (another thread would misread it); corrupt parent
        // cycles are bounded by depth instead.
        if depth > 512 {
            return M_DONE;
        }
        let ix = self.ix;
        let p = ix.parent[d as usize];
        let matched = if p == NONE {
            f.find(name_of(self.ix, d, case)).is_some()
        } else {
            let pm = self.dir_memo(p, f, case, slot, depth + 1);
            pm & (M_MATCH | M_KIDS) != 0 || crosses(name_of(self.ix, d, case), f, pm)
        };
        let mut v = M_DONE;
        if matched {
            v |= M_MATCH;
        } else {
            // Last (needle-1) bytes of `<path of d>\`, gathered by walking up.
            let needle = f.needle();
            let need = needle.len();
            self.chain.clear();
            let (mut acc, mut cur) = (0, d);
            while cur != NONE && acc < need && self.chain.len() < 1024 {
                self.chain.push(cur);
                acc += ix.name_len[cur as usize] as usize + 1;
                cur = ix.parent[cur as usize];
            }
            self.tail.clear();
            for &c in self.chain.iter().rev() {
                let n = if case { ix.name(c).as_bytes() } else { ix.folded(c) };
                self.tail.extend_from_slice(n);
                self.tail.push(b'\\');
            }
            let t = &self.tail;
            for k in 1..need.min(t.len() + 1) {
                if t[t.len() - k..] == needle[..k] {
                    v |= 1 << k;
                }
            }
            if t.ends_with(needle) {
                v |= M_KIDS;
            }
        }
        self.memos[slot][d as usize].store(v, Relaxed);
        v
    }

    /// Fallback for very long needles: build the whole path.
    fn full_path_hit(&mut self, id: u32, f: &Finder, case: bool) -> bool {
        self.tail.clear();
        self.ix.path_into(id, !case, &mut self.chain, &mut self.tail);
        f.find(&self.tail).is_some()
    }
}

#[inline]
fn name_of(ix: &Index, id: u32, case: bool) -> &[u8] {
    if case {
        ix.name(id).as_bytes()
    } else {
        ix.folded(id)
    }
}

#[inline]
fn crosses(name: &[u8], f: &Finder, memo: u64) -> bool {
    if f.find(name).is_some() {
        return true;
    }
    let needle = f.needle();
    let mut ks = memo & M_CROSS;
    while ks != 0 {
        let k = ks.trailing_zeros() as usize;
        ks &= ks - 1;
        if name.starts_with(&needle[k..]) {
            return true;
        }
    }
    false
}

#[inline]
fn utf8_len(b: u8) -> usize {
    match b {
        0..=0x7F => 1,
        0xF0.. => 4,
        0xE0.. => 3,
        _ => 2,
    }
}

/// Whole-string glob with `*` and `?` (`?` = one UTF-8 char). Linear-time greedy
/// backtracking: only the most recent `*` is ever revisited.
pub fn glob(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, si));
            pi += 1;
        } else if pi < p.len() && p[pi] == b'?' {
            pi += 1;
            si += utf8_len(s[si]);
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some((sp, ss)) = star {
            let ss = ss + utf8_len(s[ss]);
            star = Some((sp, ss));
            pi = sp + 1;
            si = ss;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len() && si == s.len()
}

/// Search the whole index. Returns live ids in `sort` order (reversed if `desc`).
/// The caller must have called `ensure_path_sort` if `sort == Path`.
pub fn search(ix: &Index, q: &Query, sort: Sort, desc: bool) -> Vec<u32> {
    let order = ix.order(sort);
    if q.is_all() {
        // Perms hold only live ids (deletions are re-slotted per batch), so this is a copy.
        let mut v = order.to_vec();
        if desc {
            v.reverse();
        }
        return v;
    }
    let n = ix.len();
    let words = n.div_ceil(64);
    let mut bits = vec![0u64; words];
    let t = threads();
    // Path matching is ~10x the work per entry, so it parallelizes from smaller sizes.
    let parallel = t > 1 && (n > 100_000 || (q.needs_path() && n > 20_000));
    let lead = q.lead();
    let memos = q.memos(n);
    let fill = |base_word: usize, out: &mut [u64]| {
        let mut ctx = Ctx::new(ix, &memos);
        let a = base_word * 64;
        let b = (a + out.len() * 64).min(n);
        match lead {
            Some((f, case, exact)) => scan_arena(ix, q, f, case, exact, a, b, out, &mut ctx),
            None => {
                for id in a..b {
                    if ix.flags[id] & flag::DELETED == 0 && ctx.matches(q, id as u32) {
                        out[(id - a) / 64] |= 1 << ((id - a) % 64);
                    }
                }
            }
        }
    };
    if parallel {
        // More chunks than threads evens out skew (some folders are path-deep, some flat).
        let per = words.div_ceil(t * 4).max(1);
        par_chunks_mut(&mut bits, per, |i, out| fill(i * per, out));
    } else {
        fill(0, &mut bits);
    }
    filter_order(order, &bits, desc)
}

/// Candidate generation for the common case (a plain name substring): instead of one
/// `find` call per name, run SIMD memmem over long contiguous stretches of the name arena
/// and map each hit back to its entry. Names are laid out in id order, so a stretch of ids
/// is a stretch of bytes; ids whose name was relocated by a rename just start a new run.
#[allow(clippy::too_many_arguments)]
fn scan_arena(ix: &Index, q: &Query, f: &Finder, case: bool, exact: bool, a: usize, b: usize, out: &mut [u64], ctx: &mut Ctx) {
    let arena: &[u8] = if case { &ix.names } else { &ix.folded };
    let (off, len) = (&ix.name_off, &ix.name_len);
    let nl = f.needle().len();
    let only_lead = exact && q.groups.len() == 1;
    let mut id = a;
    while id < b {
        // Grow a run of ids whose names sit in increasing arena order (small gaps of dead
        // bytes from renames are fine: hits there map to no entry and are skipped).
        let rs = id;
        let s0 = off[id] as usize;
        let mut e0 = s0 + len[id] as usize;
        id += 1;
        while id < b {
            let o = off[id] as usize;
            if o < e0 || o - e0 > 4096 {
                break;
            }
            e0 = o + len[id] as usize;
            id += 1;
        }
        let re = id;
        let (mut k, mut cur) = (rs, s0);
        while k < re && cur < e0 {
            let Some(p) = f.find(&arena[cur..e0]) else { break };
            let pos = cur + p;
            // Invariant: off[k] <= cur <= pos, so the partition point is >= 1.
            k += off[k..re].partition_point(|&o| o as usize <= pos) - 1;
            let ne = off[k] as usize + len[k] as usize;
            if pos + nl <= ne && ix.flags[k] & flag::DELETED == 0 && (only_lead || ctx.matches(q, k as u32)) {
                out[(k - a) / 64] |= 1 << ((k - a) % 64);
            }
            // One hit per name is enough; a hit straddling two names restarts at the next.
            k += 1;
            if k < re {
                cur = off[k] as usize;
            }
        }
    }
}

/// Walk the presorted order keeping ids whose bit is set — split across threads, since
/// for broad queries this pass costs as much as the matching itself.
fn filter_order(order: &[u32], bits: &[u64], desc: bool) -> Vec<u32> {
    let hit = |id: u32| bits[(id / 64) as usize] & (1 << (id % 64)) != 0;
    let t = threads();
    let mut parts: Vec<Vec<u32>> = if order.len() < 200_000 || t < 2 {
        vec![order.iter().copied().filter(|&id| hit(id)).collect()]
    } else {
        let per = order.len().div_ceil(t * 2);
        let chunks: Vec<&[u32]> = order.chunks(per).collect();
        let mut parts: Vec<Vec<u32>> = vec![Vec::new(); chunks.len()];
        par_chunks_mut(&mut parts, 1, |i, out| out[0] = chunks[i].iter().copied().filter(|&id| hit(id)).collect());
        parts
    };
    let total = parts.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    if desc {
        for p in parts.iter_mut().rev() {
            p.reverse();
            out.extend_from_slice(p);
        }
    } else {
        for p in &parts {
            out.extend_from_slice(p);
        }
    }
    out
}

/// Re-filter a previous (already sorted) result set — used when the new query provably
/// narrows the old one, so the work shrinks with every keystroke.
pub fn refine(ix: &Index, q: &Query, prev: &[u32]) -> Vec<u32> {
    let t = threads();
    let memos = q.memos(ix.len());
    if prev.len() < 100_000 || t < 2 {
        let mut ctx = Ctx::new(ix, &memos);
        return prev.iter().copied().filter(|&id| ix.is_live(id) && ctx.matches(q, id)).collect();
    }
    let per = prev.len().div_ceil(t * 2);
    let chunks: Vec<&[u32]> = prev.chunks(per).collect();
    let mut parts: Vec<Vec<u32>> = vec![Vec::new(); chunks.len()];
    par_chunks_mut(&mut parts, 1, |i, out| {
        let mut ctx = Ctx::new(ix, &memos);
        out[0] = chunks[i].iter().copied().filter(|&id| ix.is_live(id) && ctx.matches(q, id)).collect();
    });
    parts.concat()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    fn run(q: &str) -> Vec<String> {
        let ix = sample();
        search(&ix, &Query::parse(q, 0), Sort::Name, false).iter().map(|&i| ix.name(i).to_string()).collect()
    }

    #[test]
    fn syntax() {
        assert_eq!(run("txt"), ["a.txt", "B.txt"]);
        assert_eq!(run("b.TXT"), ["B.txt"]);
        assert_eq!(run("case:b.txt"), Vec::<String>::new());
        assert_eq!(run("*.txt"), ["a.txt", "B.txt"]);
        assert_eq!(run("?.txt"), ["a.txt", "B.txt"]);
        assert_eq!(run("txt | pdf"), ["a.txt", "B.txt", "z.pdf"]);
        assert_eq!(run("txt|pdf !a."), ["B.txt", "z.pdf"]);
        assert_eq!(run("ext:pdf;exe"), ["notepad.exe", "z.pdf"]);
        assert_eq!(run("folder:"), ["C:", "docs", "Users", "Windows"]);
        assert_eq!(run(r"users\ !folder:"), ["a.txt", "B.txt", "z.pdf"]);
        assert_eq!(run(r"docs\z"), ["z.pdf"]);
        assert_eq!(run(r"c:\users\d"), ["docs", "z.pdf"]);
        assert_eq!(run(r"s\do"), ["docs", "z.pdf"]);
        assert_eq!(run(r"case:Users\"), ["docs", "a.txt", "B.txt", "z.pdf"]);
        assert_eq!(run(r"case:users\"), Vec::<String>::new());
        assert_eq!(run(r"\windows\"), ["notepad.exe"]);
        assert_eq!(run("\"note pad\""), Vec::<String>::new());
        assert_eq!(run("").len(), 8);
        assert_eq!(run("s"), ["docs", "Users", "Windows"]);
    }

    #[test]
    fn narrowing() {
        let a = Query::parse("foo", 0);
        assert!(Query::parse("foob", 0).narrows(&a));
        assert!(Query::parse("bar foo", 0).narrows(&a));
        assert!(!Query::parse("fo", 0).narrows(&a));
        assert!(!Query::parse("!foo", 0).narrows(&a));
        assert!(!Query::parse("*foo", 0).narrows(&a));
    }

    #[test]
    fn globbing() {
        assert!(glob(b"*.mkv", b"chernobyl.s01e05.mkv"));
        assert!(glob(b"a*b*c", b"axxbyyc"));
        assert!(!glob(b"a*b*c", b"axxbyy"));
        assert!(glob("?.txt".as_bytes(), "ä.txt".as_bytes()));
        assert!(glob(b"**", b""));
    }
}
