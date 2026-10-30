//! Persistent worker pool for per-keystroke parallelism.
//!
//! Spawning 16 OS threads costs ~1 ms on Windows — as much as the search itself. These
//! workers are created once and parked on a condvar; waking them costs microseconds.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

#[derive(Clone, Copy)]
struct Job {
    // Lifetime-erased borrow of the caller's closure; `par_for` blocks until every worker
    // has finished with it, so it never dangles.
    f: *const (dyn Fn(usize) + Sync),
    n: usize,
}
unsafe impl Send for Job {}

struct State {
    gen: u64,
    job: Option<Job>,
    active: usize,
}

struct Pool {
    state: Mutex<State>,
    start: Condvar,
    done: Condvar,
    next: AtomicUsize,
    workers: usize,
    /// One parallel job at a time; concurrent callers (two UI clients) queue up.
    run: Mutex<()>,
}

fn pool() -> &'static Pool {
    static P: OnceLock<&'static Pool> = OnceLock::new();
    P.get_or_init(|| {
        let workers = crate::index::threads().saturating_sub(1);
        let p: &'static Pool = Box::leak(Box::new(Pool {
            state: Mutex::new(State { gen: 0, job: None, active: 0 }),
            start: Condvar::new(),
            done: Condvar::new(),
            next: AtomicUsize::new(0),
            workers,
            run: Mutex::new(()),
        }));
        for _ in 0..workers {
            std::thread::spawn(move || worker(p));
        }
        p
    })
}

fn worker(p: &'static Pool) {
    let mut seen = 0;
    loop {
        let job = {
            let mut s = p.state.lock().unwrap();
            while s.gen == seen {
                s = p.start.wait(s).unwrap();
            }
            seen = s.gen;
            s.job
        };
        if let Some(j) = job {
            loop {
                let i = p.next.fetch_add(1, Ordering::Relaxed);
                if i >= j.n {
                    break;
                }
                unsafe { (*j.f)(i) };
            }
        }
        let mut s = p.state.lock().unwrap();
        s.active -= 1;
        if s.active == 0 {
            p.done.notify_one();
        }
    }
}

/// Run `f(0..n)` across all cores (the caller works too). Returns when all are done.
pub fn par_for(n: usize, f: impl Fn(usize) + Sync) {
    let p = pool();
    if n <= 1 || p.workers == 0 {
        (0..n).for_each(f);
        return;
    }
    let _g = p.run.lock().unwrap();
    let fr: &(dyn Fn(usize) + Sync) = &f;
    // SAFETY: erase the borrow's lifetime; we wait for `active == 0` before returning.
    let job = Job { f: unsafe { std::mem::transmute::<&(dyn Fn(usize) + Sync), &'static (dyn Fn(usize) + Sync)>(fr) }, n };
    p.next.store(0, Ordering::Relaxed);
    {
        let mut s = p.state.lock().unwrap();
        s.job = Some(job);
        s.active = p.workers;
        s.gen += 1;
    }
    p.start.notify_all();
    loop {
        let i = p.next.fetch_add(1, Ordering::Relaxed);
        if i >= n {
            break;
        }
        f(i);
    }
    let mut s = p.state.lock().unwrap();
    while s.active > 0 {
        s = p.done.wait(s).unwrap();
    }
    s.job = None;
}

/// Split `v` into `chunk`-sized pieces and process them in parallel with exclusive access.
pub fn par_chunks_mut<T: Send>(v: &mut [T], chunk: usize, f: impl Fn(usize, &mut [T]) + Sync) {
    let chunk = chunk.max(1);
    let len = v.len();
    let n = len.div_ceil(chunk);
    let base = SendPtr(v.as_mut_ptr());
    par_for(n, |i| {
        let a = i * chunk;
        let b = (a + chunk).min(len);
        let base = &base;
        // SAFETY: chunk i covers [a, b), disjoint from every other chunk.
        let s = unsafe { std::slice::from_raw_parts_mut(base.0.add(a), b - a) };
        f(i, s);
    });
}

struct SendPtr<T>(*mut T);
unsafe impl<T> Sync for SendPtr<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_everything_once() {
        for _ in 0..50 {
            let hits: Vec<AtomicUsize> = (0..1000).map(|_| AtomicUsize::new(0)).collect();
            par_for(1000, |i| {
                hits[i].fetch_add(1, Ordering::Relaxed);
            });
            assert!(hits.iter().all(|h| h.load(Ordering::Relaxed) == 1));
        }
        let mut v = vec![0u32; 10_000];
        par_chunks_mut(&mut v, 777, |i, c| c.iter_mut().for_each(|x| *x = i as u32));
        assert_eq!(v[776], 0);
        assert_eq!(v[777], 1);
        assert_eq!(v[9999], 12);
    }
}
