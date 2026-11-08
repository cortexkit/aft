//! Measures what the allocation tracer costs, and shows what it names
//! during a burst like the daemon's.
//!
//! ```text
//! cargo run --profile ci-storm -p agent-file-tools --example alloc_trace_probe -- \
//!     overhead <repo> <pack.jsonl.gz> <on|off> [threads]
//! cargo run --profile ci-storm -p agent-file-tools --example alloc_trace_probe -- \
//!     burst <repo> [worktrees] [threads]
//! ```
//!
//! `overhead` runs two workloads in one process and prints one JSON line
//! for each, with its wall and CPU seconds: a cold data-layer build of
//! `<repo>`'s HEAD (exported with `git archive` under `TMPDIR`) into a new
//! store, then the edit pack replayed through the incremental call-graph
//! engine with its checks off (as `EDIT_GATE_TIMING_ONLY=1` runs it). With
//! `on` every allocation passes through the tracer, as in the `aft` binary;
//! with `off` straight to the system allocator. Both arms go through the
//! same switch, so the difference between them is the tracer's cost.
//!
//! `burst` exports `<repo>` once and builds it, publishing its index, then
//! exports `worktrees` more checkouts of the same commit and starts them all
//! at once against that store, each building and then resolving its call
//! graph after an edit, while the first warms its symbols. Every line the
//! tracer writes goes to stderr.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use aft::alloc_trace::TracingAllocator;
use aft::data_layer::checkout::{Checkout, DataLayer};
use aft_store::ids::{Digest128, FamilyId};

/// Sends every allocation to the tracer while `TRACE` is set, and to the
/// system allocator otherwise. Memory either side allocated can be freed by
/// the other: the tracer frees through the system allocator too.
struct Switch;

static TRACE: AtomicBool = AtomicBool::new(false);

unsafe impl GlobalAlloc for Switch {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACE.load(Ordering::Relaxed) {
            unsafe { TracingAllocator.alloc(layout) }
        } else {
            unsafe { System.alloc(layout) }
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if TRACE.load(Ordering::Relaxed) {
            unsafe { TracingAllocator.alloc_zeroed(layout) }
        } else {
            unsafe { System.alloc_zeroed(layout) }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if TRACE.load(Ordering::Relaxed) {
            unsafe { TracingAllocator.dealloc(ptr, layout) }
        } else {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if TRACE.load(Ordering::Relaxed) {
            unsafe { TracingAllocator.realloc(ptr, layout, new_size) }
        } else {
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
}

#[global_allocator]
static GLOBAL: Switch = Switch;

/// Writes the tree of HEAD in `repo` into `dir`.
fn export(repo: &Path, dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let mut archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", "HEAD"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let status = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(dir)
        .stdin(archive.stdout.take().unwrap())
        .status()
        .unwrap();
    assert!(status.success() && archive.wait().unwrap().success());
}

/// User and system CPU seconds this process has spent, its threads included.
fn cpu_seconds() -> f64 {
    // SAFETY: getrusage only fills the struct it is given.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &raw mut usage);
        usage
    };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + f64::from(time.tv_usec as i32) / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

fn measure(phase: &str, arm: &str, work: impl FnOnce() -> String) {
    let cpu = cpu_seconds();
    let started = Instant::now();
    let detail = work();
    let wall = started.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu;
    println!(
        "{}",
        serde_json::json!({
            "phase": phase,
            "arm": arm,
            "wall_s": (wall * 1000.0).round() / 1000.0,
            "cpu_s": (cpu * 1000.0).round() / 1000.0,
            "detail": detail,
        })
    );
}

fn print_line(line: &str) {
    eprintln!("alloc_trace: {line}");
}

fn overhead(mut args: impl Iterator<Item = String>) {
    let repo = PathBuf::from(args.next().unwrap());
    let pack = PathBuf::from(args.next().unwrap());
    let arm = args.next().unwrap();
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    let on = match arm.as_str() {
        "on" => true,
        "off" => false,
        other => panic!("arm must be on or off, not {other}"),
    };
    if on {
        aft::alloc_trace::set_line_sink(print_line);
        aft::alloc_trace::start_logger();
    }
    TRACE.store(on, Ordering::SeqCst);

    let scratch = tempfile::tempdir().unwrap();
    let checkout_dir = scratch.path().join("checkout");
    export(&repo, &checkout_dir);
    measure("cold_build", &arm, || {
        let layer = Arc::new(DataLayer::new(256 << 20, threads));
        let family = FamilyId::from_digest(Digest128::of(b"alloc trace probe"));
        let checkout =
            Checkout::bind(layer, &scratch.path().join("store"), family, &checkout_dir).unwrap();
        let report = checkout.build().unwrap();
        format!("{} files, {} parsed", report.files, report.parsed_files)
    });
    // SAFETY: set before the replay reads it, while no other thread of
    // this process reads the environment.
    unsafe { std::env::set_var("EDIT_GATE_TIMING_ONLY", "1") };
    measure("edit_replay", &arm, || {
        let (edits, _) =
            aft::data_layer::edit_gate::run(&pack, threads, usize::MAX, &mut std::io::sink())
                .unwrap();
        format!("{edits} edits checked (timing only)")
    });
    aft::alloc_trace::drain_now();
}

fn burst(mut args: impl Iterator<Item = String>) {
    let repo = PathBuf::from(args.next().unwrap());
    let worktrees = args.next().map_or(3, |count| count.parse().unwrap());
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    aft::alloc_trace::set_line_sink(print_line);
    aft::alloc_trace::start_logger();
    TRACE.store(true, Ordering::SeqCst);

    let scratch = tempfile::tempdir().unwrap();
    let store = scratch.path().join("store");
    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let family = FamilyId::from_digest(Digest128::of(b"alloc trace burst"));
    let sibling_dir = scratch.path().join("sibling");
    export(&repo, &sibling_dir);
    let sibling =
        Arc::new(Checkout::bind(Arc::clone(&layer), &store, family, &sibling_dir).unwrap());
    measure("sibling_build", "on", || {
        let report = sibling.build().unwrap();
        format!("{} files, {} parsed", report.files, report.parsed_files)
    });
    let dirs = (0..worktrees)
        .map(|index| {
            let dir = scratch.path().join(format!("worktree-{index}"));
            export(&repo, &dir);
            dir
        })
        .collect::<Vec<_>>();
    measure("parallel_starts", "on", || {
        std::thread::scope(|scope| {
            for dir in &dirs {
                let layer = Arc::clone(&layer);
                let store = store.clone();
                scope.spawn(move || {
                    let checkout = Checkout::bind(layer, &store, family, dir).unwrap();
                    checkout.build().unwrap();
                    // An edit, so that the call graph has something to resolve
                    // and its engine starts.
                    let file = dir.join("crates/aft/src/lib.rs");
                    let mut text = std::fs::read_to_string(&file).unwrap();
                    text.push_str("\nfn alloc_trace_probe_edit() {}\n");
                    std::fs::write(&file, &text).unwrap();
                    checkout
                        .record_paths(&[b"crates/aft/src/lib.rs".to_vec()])
                        .unwrap();
                    let _ = checkout.refresh_callgraph();
                });
            }
        });
        format!("{worktrees} worktrees")
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    aft::alloc_trace::drain_now();
    let peaks = aft::alloc_trace::take_census_peaks(5);
    println!(
        "{}",
        serde_json::json!({ "census_allocation_labels": peaks })
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("overhead") => overhead(args),
        Some("burst") => burst(args),
        _ => panic!("usage: alloc_trace_probe overhead|burst ..."),
    }
}
