//! Measures what the allocation tracer costs, and shows what it names
//! during a burst like the daemon's.
//!
//! ```text
//! cargo run --profile ci-storm -p agent-file-tools --example alloc_trace_probe -- \
//!     overhead <repo> <pack.jsonl.gz> <on|off> [threads]
//! cargo run --profile ci-storm -p agent-file-tools --example alloc_trace_probe -- \
//!     burst <repo> [worktrees] [threads]
//! cargo run --profile ci-storm -p agent-file-tools --example alloc_trace_probe -- \
//!     refresh <repo> [threads]
//! cargo run --profile ci-storm -p agent-file-tools --example alloc_trace_probe -- \
//!     replay <pack.jsonl.gz> [publish_every] [threads]
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
//! at once against that store: every worktree builds, then every one
//! appends a function to its crate root and refreshes its call graph, then
//! each starts an engine again from its publication, one at a time. Each
//! phase prints its wall and CPU seconds, the peak resident memory sampled
//! every 250 ms (the physical footprint on macOS, `VmRSS` on Linux) and each
//! allocation label's peak live bytes. Every line the tracer writes goes to
//! stderr.
//!
//! `refresh <repo> [threads]` builds one sibling and one worktree started
//! from it, then makes the same edit in the worktree, and in the sibling
//! bound again by a new layer as a restarted process would, refreshing
//! each, each a phase of its own.
//!
//! `replay <pack.jsonl.gz> [publish_every] [threads]` replays an edit pack
//! through one checkout, publishing its changes every `publish_every` edits
//! (8 by default) and at each commit's end, and counts the refreshes that
//! had to start a call-graph engine because a publication dropped it.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
/// Windows has no getrusage; there the probe reports 0 and only wall time
/// is comparable.
#[cfg(not(unix))]
fn cpu_seconds() -> f64 {
    0.0
}

/// User and system CPU seconds this process has spent, its threads included.
#[cfg(unix)]
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

/// The process's resident memory: the physical footprint on macOS, `VmRSS`
/// on Linux.
fn resident_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
        let kib = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
        Some(kib * 1024)
    }
    #[cfg(target_os = "macos")]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
        // SAFETY: the call fills the struct it is given, of the kind named.
        let rc = unsafe {
            libc::proc_pid_rusage(
                libc::getpid(),
                libc::RUSAGE_INFO_V4,
                usage.as_mut_ptr().cast(),
            )
        };
        // SAFETY: filled when the call succeeds.
        (rc == 0).then(|| unsafe { usage.assume_init() }.ri_phys_footprint)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// The highest resident memory sampled since the last [`take_resident_peak`].
static RESIDENT_PEAK: AtomicU64 = AtomicU64::new(0);

/// Samples [`resident_bytes`] every 250 ms for the rest of the run.
fn start_resident_sampler() {
    std::thread::spawn(|| loop {
        if let Some(bytes) = resident_bytes() {
            RESIDENT_PEAK.fetch_max(bytes, Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_millis(250));
    });
}

/// The sampled peak since the last call, and starts the next one from now.
fn take_resident_peak() -> u64 {
    let now = resident_bytes().unwrap_or(0);
    RESIDENT_PEAK.swap(now, Ordering::Relaxed).max(now)
}

fn mib(bytes: u64) -> u64 {
    bytes >> 20
}

/// The labels' peaks since the last call, in MiB, highest first.
fn label_peaks() -> serde_json::Value {
    aft::alloc_trace::drain_now();
    aft::alloc_trace::take_census_peaks(40)
        .into_iter()
        .map(|peak| (peak.label.to_string(), mib(peak.peak_bytes).into()))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// Runs `work` as one phase and prints its wall and CPU seconds, the peak
/// resident memory sampled while it ran, and each label's peak live bytes.
fn phase(name: &str, work: impl FnOnce() -> String) {
    let _ = label_peaks();
    let at_start = take_resident_peak();
    let cpu = cpu_seconds();
    let started = Instant::now();
    let detail = work();
    let wall = started.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu;
    println!(
        "{}",
        serde_json::json!({
            "phase": name,
            "wall_s": (wall * 100.0).round() / 100.0,
            "cpu_s": (cpu * 100.0).round() / 100.0,
            "resident_mib_at_start": mib(at_start),
            "resident_peak_mib": mib(take_resident_peak()),
            "label_peak_mib": label_peaks(),
            "detail": detail,
        })
    );
}

fn start_tracing() {
    aft::alloc_trace::set_line_sink(print_line);
    aft::alloc_trace::start_logger();
    TRACE.store(true, Ordering::SeqCst);
    start_resident_sampler();
}

/// The probe's edit: a function appended to AFT's crate root, which most of
/// the crate reads.
const EDITED: &str = "crates/aft/src/lib.rs";

fn edit_and_refresh(checkout: &Checkout, dir: &Path) -> String {
    let file = dir.join(EDITED);
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str("\nfn alloc_trace_probe_edit() {}\n");
    std::fs::write(&file, &text).unwrap();
    checkout
        .record_paths(&[EDITED.as_bytes().to_vec()])
        .unwrap();
    let started = Instant::now();
    let stats = checkout.refresh_callgraph().unwrap();
    format!(
        "refresh {:.0} ms; {}",
        started.elapsed().as_secs_f64() * 1000.0,
        stats.map_or_else(
            || "nothing to update".to_string(),
            |stats| format!(
                "rebound {}, resolved {}, replayed {}, dispatch replayed {}, recomputed {}, \
                 sites {}, touched {}",
                stats.rebound,
                stats.resolved,
                stats.replayed,
                stats.dispatch_replayed,
                stats.dispatch_recomputed,
                stats.sites_resolved,
                stats.touched.len()
            )
        )
    )
}

fn burst(mut args: impl Iterator<Item = String>) {
    let repo = PathBuf::from(args.next().unwrap());
    let worktrees = args.next().map_or(3, |count| count.parse().unwrap());
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    start_tracing();

    let scratch = tempfile::tempdir().unwrap();
    let store = scratch.path().join("store");
    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let family = FamilyId::from_digest(Digest128::of(b"alloc trace burst"));
    let sibling_dir = scratch.path().join("sibling");
    export(&repo, &sibling_dir);
    let sibling =
        Arc::new(Checkout::bind(Arc::clone(&layer), &store, family, &sibling_dir).unwrap());
    phase("sibling_build", || {
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
    // Each phase starts in every worktree at once and ends when the last
    // one finishes, so its time and peaks belong to that phase alone.
    let checkouts = std::sync::Mutex::new(Vec::new());
    phase("worktree_builds", || {
        let details = std::thread::scope(|scope| {
            let handles = dirs
                .iter()
                .map(|dir| {
                    let layer = Arc::clone(&layer);
                    let store = store.clone();
                    let checkouts = &checkouts;
                    scope.spawn(move || {
                        let checkout = Checkout::bind(layer, &store, family, dir).unwrap();
                        let report = checkout.build().unwrap();
                        checkouts.lock().unwrap().push((dir.clone(), checkout));
                        format!(
                            "started from sibling {:?}, scan {:.0} ms, resolve {:.0} ms, \
                             publish {:.0} ms",
                            report.started_from_sibling,
                            report.scan_ms,
                            report.resolve_ms,
                            report.publish_ms
                        )
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        details.join("; ")
    });
    let checkouts = checkouts.into_inner().unwrap();
    phase("first_refreshes", || {
        std::thread::scope(|scope| {
            let handles = checkouts
                .iter()
                .map(|(dir, checkout)| scope.spawn(move || edit_and_refresh(checkout, dir)))
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
                .join("; ")
        })
    });
    // An engine started again from each worktree's publication, one at a
    // time, so the CPU it takes is its own: the part of the refresh above
    // that started the engine.
    phase("engine_starts", || {
        checkouts
            .iter()
            .map(|(_, checkout)| {
                let timings = checkout.measure_engine_start().unwrap();
                timings
                    .iter()
                    .map(|(step, ms)| format!("{step} {ms:.0} ms"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .collect::<Vec<_>>()
            .join("; ")
    });
}

/// One sibling built cold and one worktree started from it; the worktree's
/// first edit refreshed, then the sibling's after a restart, each a phase of
/// its own.
fn refresh(mut args: impl Iterator<Item = String>) {
    let repo = PathBuf::from(args.next().unwrap());
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    start_tracing();
    let scratch = tempfile::tempdir().unwrap();
    let store = scratch.path().join("store");
    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let family = FamilyId::from_digest(Digest128::of(b"alloc trace refresh"));
    let sibling_dir = scratch.path().join("sibling");
    export(&repo, &sibling_dir);
    let sibling = Checkout::bind(Arc::clone(&layer), &store, family, &sibling_dir).unwrap();
    phase("sibling_build", || {
        let report = sibling.build().unwrap();
        format!("{} files, {} parsed", report.files, report.parsed_files)
    });
    let dir = scratch.path().join("worktree");
    export(&repo, &dir);
    let worktree = Checkout::bind(Arc::clone(&layer), &store, family, &dir).unwrap();
    phase("worktree_build", || {
        let report = worktree.build().unwrap();
        format!("started from sibling {:?}", report.started_from_sibling)
    });
    phase("worktree_first_refresh", || {
        edit_and_refresh(&worktree, &dir)
    });
    println!("engine parts {:?}", worktree.engine_memory_parts());
    // A restart: a new layer, so nothing the first process decoded or
    // cached is reused, and the sibling bound again from the store.
    drop(sibling);
    let restarted = Checkout::bind(
        Arc::new(DataLayer::new(256 << 20, threads)),
        &store,
        family,
        &sibling_dir,
    )
    .unwrap();
    phase("restart_first_refresh", || {
        edit_and_refresh(&restarted, &sibling_dir)
    });
}

/// Replays an edit pack through one checkout built from its base files, as
/// the runtime drives it: each edit recorded and refreshed, its changes
/// published every `publish_every` edits and at each commit's end. A
/// publication drops the call-graph engine, so the next refresh starts one
/// again; prints how many refreshes did, and the time each kind took.
fn replay(mut args: impl Iterator<Item = String>) {
    use std::io::BufRead as _;
    let pack = PathBuf::from(args.next().unwrap());
    let publish_every = args.next().map_or(8, |count| count.parse().unwrap());
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    start_tracing();
    let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(
        std::fs::File::open(&pack).unwrap(),
    ));
    let mut lines = reader.lines();
    let base: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("checkout");
    let write = |path: &str, text: &str| {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    for (path, text) in base["files"].as_object().unwrap() {
        write(path, text.as_str().unwrap_or_default());
    }
    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let family = FamilyId::from_digest(Digest128::of(b"alloc trace replay"));
    let checkout = Checkout::bind(layer, &scratch.path().join("store"), family, &root).unwrap();
    phase("build", || {
        let report = checkout.build().unwrap();
        format!("{} files, {} parsed", report.files, report.parsed_files)
    });
    let edits = lines
        .map(|line| serde_json::from_str::<serde_json::Value>(&line.unwrap()).unwrap())
        .filter(|value| value["type"] == "edit")
        .collect::<Vec<_>>();
    // (refreshes, wall seconds, CPU seconds), for refreshes that started an
    // engine and for those that reused one.
    let mut started = (0usize, 0.0f64, 0.0f64);
    let mut reused = (0usize, 0.0f64, 0.0f64);
    let mut publications = 0usize;
    phase("replay", || {
        let mut since = 0usize;
        for (index, edit) in edits.iter().enumerate() {
            let path = edit["path"].as_str().unwrap();
            write(path, edit["text"].as_str().unwrap_or_default());
            checkout.record_paths(&[path.as_bytes().to_vec()]).unwrap();
            let starts = checkout.engine_memory_parts().is_none();
            let cpu = cpu_seconds();
            let wall = Instant::now();
            let stats = checkout.refresh_callgraph().unwrap();
            let wall = wall.elapsed().as_secs_f64();
            let cpu = cpu_seconds() - cpu;
            if stats.is_some() {
                let kind = if starts { &mut started } else { &mut reused };
                kind.0 += 1;
                kind.1 += wall;
                kind.2 += cpu;
            }
            since += 1;
            let commit_ends = edits
                .get(index + 1)
                .is_none_or(|next| next["commit"] != edit["commit"]);
            if since >= publish_every || commit_ends {
                if checkout.publish_changes().unwrap().is_some() {
                    publications += 1;
                }
                since = 0;
            }
        }
        format!(
            "{} edits, {publications} publications; refreshes that started an engine: {} \
             ({:.1} s wall, {:.1} s CPU); that reused one: {} ({:.1} s wall, {:.1} s CPU)",
            edits.len(),
            started.0,
            started.1,
            started.2,
            reused.0,
            reused.1,
            reused.2
        )
    });
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("overhead") => overhead(args),
        Some("burst") => burst(args),
        Some("refresh") => refresh(args),
        Some("replay") => replay(args),
        _ => panic!("usage: alloc_trace_probe overhead|burst|refresh|replay ..."),
    }
}
