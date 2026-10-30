//! nothing-svc: the indexing service.
//!
//!   nothing-svc                       run under the Service Control Manager
//!   nothing-svc --console             same engine in the foreground (elevated), Ctrl+C to stop
//!   nothing-svc --scan C: [--bench q ...]   one-shot MFT scan + stats (elevated)
//!   nothing-svc --walk DIR [--serve] [--bench q ...]   dev: directory-walk index, unelevated

mod engine;
mod log;
mod mft;
mod server;
mod service;
mod usn;
mod walk;
mod win;

use std::sync::OnceLock;
use std::time::Instant;

use nothing_core::search::{search, Query};
use nothing_core::{Index, Sort};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};

static CONSOLE_ENGINE: OnceLock<std::sync::Arc<engine::Engine>> = OnceLock::new();

unsafe extern "system" fn on_ctrl_c(_: u32) -> windows_sys::core::BOOL {
    if let Some(e) = CONSOLE_ENGINE.get() {
        e.request_stop();
    }
    1
}

fn bench(ix: &mut Index, queries: &[String]) {
    ix.ensure_path_sort();
    println!("{:<28} {:>10} {:>10} {:>10}", "query", "results", "best ms", "median ms");
    for q in queries {
        let mut times = Vec::new();
        let mut n = 0;
        for _ in 0..15 {
            let t = Instant::now();
            let parsed = Query::parse(q, 0);
            n = search(ix, &parsed, Sort::Name, false).len();
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("{:<28} {:>10} {:>10.2} {:>10.2}", format!("{q:?}"), n, times[0], times[times.len() / 2]);
    }
}

fn report(ix: &Index, what: &str, ms: u128) {
    println!(
        "{what}: {} entries in {} ms | index heap {} MB = {} bytes/entry",
        ix.live(),
        ms,
        ix.heap_bytes() >> 20,
        ix.heap_bytes() / ix.live().max(1)
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bench_qs: Vec<String> = args.iter().skip_while(|a| *a != "--bench").skip(1).cloned().collect();
    match args.first().map(String::as_str) {
        None => {
            if !service::dispatch() {
                eprintln!("nothing-svc: run as a service, or use --console / --scan C: / --walk DIR");
                std::process::exit(2);
            }
        }
        Some("--console") => {
            log::init(true);
            win::enable_privilege("SeBackupPrivilege");
            let e = engine::Engine::new(true);
            let _ = CONSOLE_ENGINE.set(e.clone());
            unsafe { SetConsoleCtrlHandler(Some(on_ctrl_c), 1) };
            e.start();
            let e2 = e.clone();
            std::thread::spawn(move || server::serve(e2));
            unsafe { WaitForSingleObject(e.stop.0, INFINITE) };
            e.save();
        }
        Some("--scan") => {
            log::init(true);
            win::enable_privilege("SeBackupPrivilege");
            let letter = args.get(1).and_then(|s| s.bytes().next()).unwrap_or(b'C').to_ascii_uppercase();
            let serial = engine::ntfs_volumes().into_iter().find(|v| v.0 == letter).map(|v| v.1).unwrap_or(0);
            match mft::scan(letter, serial, &|_| {}) {
                Ok((mut ix, st)) => {
                    let t = Instant::now();
                    ix.rebuild_sorts();
                    let sort_ms = t.elapsed().as_millis();
                    println!("MFT: {} records, {} MB read+parsed in {} ms", st.records, st.mft_bytes >> 20, st.millis);
                    println!("sort orders built in {sort_ms} ms");
                    report(&ix, "index", st.millis + sort_ms);
                    if !bench_qs.is_empty() {
                        bench(&mut ix, &bench_qs);
                    }
                }
                Err(e) => {
                    eprintln!("scan failed: {e} (needs an elevated prompt)");
                    std::process::exit(1);
                }
            }
        }
        Some("--walk") => {
            log::init(true);
            let root = args.get(1).cloned().unwrap_or_else(|| "C:\\".into());
            let t = Instant::now();
            let mut ix = walk::walk(&root);
            report(&ix, "walk", t.elapsed().as_millis());
            if !bench_qs.is_empty() {
                bench(&mut ix, &bench_qs);
            }
            if args.iter().any(|a| a == "--serve") {
                println!("serving on {}", nothing_core::proto::PIPE_NAME);
                server::serve(engine::Engine::with_index(ix));
            }
        }
        Some(other) => {
            eprintln!("unknown argument {other}");
            std::process::exit(2);
        }
    }
}
