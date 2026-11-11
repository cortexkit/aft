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
//!     restart <repo> <dir> [threads]
//! cargo run --profile ci-storm -p agent-file-tools --example alloc_trace_probe -- \
//!     replay <pack.jsonl.gz> [publish_every|daemon] [threads]
//! cargo run --profile ci-storm -p agent-file-tools --example alloc_trace_probe -- \
//!     transition <base-root> <target-root> [threads] [sibling|refresh|cold]
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
//! `restart <repo> <dir> [threads]` builds `<repo>` under `<dir>` on its
//! first run; each later run, a new process, binds it again and refreshes
//! the same edit, as the first refresh after a restart does.
//!
//! `replay <pack.jsonl.gz> [publish_every] [threads]` replays an edit pack
//! through one checkout, publishing its changes every `publish_every` edits
//! (8 by default) and at each commit's end, and counts the refreshes that
//! had to start a call-graph engine: before engines moved onto each
//! publication of their changes, every publication dropped one.
//!
//! Each phase also prints its user and system CPU apart, its minor page
//! faults and what glibc's allocator holds at its start and end; the first
//! refreshes print the block reads they made through the layer's cache.
//! With `PROBE_TRIM=1`, `refresh` hands the allocator's free memory back
//! before the restart's refresh.
//!
//! `transition` compares two exported source trees. `PROBE_STAGES=1` counts
//! the live Rust heap at each stage; `PROBE_STAGE_TIMING=1` reports stage
//! wall/CPU times without those counting atomics. Both require `test-hooks`.
//! `PROBE_SKIP_HANDOFF=1` disables pending-record reuse for a timing control;
//! `PROBE_COMPARE=1` compares every resulting engine record with a cold build.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aft::alloc_trace::TracingAllocator;
use aft::data_layer::checkout::{Checkout, DataLayer};
use aft_store::ids::{Digest128, FamilyId};

/// Sends every allocation to the tracer while `TRACE` is set, and to the
/// system allocator otherwise. Memory either side allocated can be freed by
/// the other: the tracer frees through the system allocator too.
struct Switch;

static TRACE: AtomicBool = AtomicBool::new(false);
static COUNTING: AtomicBool = AtomicBool::new(false);
static STAGE_TIMING: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);
static STAGE_RSS: AtomicU64 = AtomicU64::new(0);

fn count(bytes: isize) {
    if COUNTING.load(Ordering::Relaxed) {
        let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for Switch {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = if TRACE.load(Ordering::Relaxed) {
            unsafe { TracingAllocator.alloc(layout) }
        } else {
            unsafe { System.alloc(layout) }
        };
        if !ptr.is_null() {
            count(layout.size() as isize);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = if TRACE.load(Ordering::Relaxed) {
            unsafe { TracingAllocator.alloc_zeroed(layout) }
        } else {
            unsafe { System.alloc_zeroed(layout) }
        };
        if !ptr.is_null() {
            count(layout.size() as isize);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count(-(layout.size() as isize));
        if TRACE.load(Ordering::Relaxed) {
            unsafe { TracingAllocator.dealloc(ptr, layout) }
        } else {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let next = if TRACE.load(Ordering::Relaxed) {
            unsafe { TracingAllocator.realloc(ptr, layout, new_size) }
        } else {
            unsafe { System.realloc(ptr, layout, new_size) }
        };
        if !next.is_null() {
            count(new_size as isize - layout.size() as isize);
        }
        next
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
    let (user, system, _) = usage();
    user + system
}

/// User and system CPU seconds and minor page faults this process has had,
/// its threads included.
#[cfg(unix)]
fn usage() -> (f64, f64, u64) {
    // SAFETY: getrusage only fills the struct it is given.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &raw mut usage);
        usage
    };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + f64::from(time.tv_usec as i32) / 1e6;
    (
        seconds(usage.ru_utime),
        seconds(usage.ru_stime),
        u64::try_from(usage.ru_minflt).unwrap_or(0),
    )
}

#[cfg(not(unix))]
fn usage() -> (f64, f64, u64) {
    (0.0, 0.0, 0)
}

/// glibc's `struct mallinfo2`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[repr(C)]
#[derive(Default)]
struct MallInfo2 {
    arena: usize,
    ordblks: usize,
    smblks: usize,
    hblks: usize,
    hblkhd: usize,
    usmblks: usize,
    fsmblks: usize,
    uordblks: usize,
    fordblks: usize,
    keepcost: usize,
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
unsafe extern "C" {
    fn mallinfo2() -> MallInfo2;
    fn malloc_trim(pad: usize) -> i32;
}

/// What glibc's allocator holds: bytes taken from the system for its heaps,
/// bytes in use, bytes free in its heaps, free chunks, and bytes mapped for
/// large allocations. Main arena only for the counts glibc keeps per arena
/// (`mallinfo2` sums every arena for the byte totals).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn malloc_figures() -> serde_json::Value {
    // SAFETY: mallinfo2 only reads the allocator's own counters.
    let info = unsafe { mallinfo2() };
    serde_json::json!({
        "heap_mib": info.arena >> 20,
        "used_mib": info.uordblks >> 20,
        "free_mib": info.fordblks >> 20,
        "free_chunks": info.ordblks,
        "fast_chunks": info.smblks,
        "mapped_mib": info.hblkhd >> 20,
    })
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn malloc_figures() -> serde_json::Value {
    serde_json::Value::Null
}

/// Hands the allocator's free pages back to the system and merges its free
/// chunks, as the daemon's periodic relief does; a no-op off glibc.
fn trim_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only works on the allocator's own state.
    unsafe {
        malloc_trim(0);
    }
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
            STAGE_RSS.fetch_max(bytes, Ordering::Relaxed);
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

/// Runs `work` as one phase and prints its wall and CPU seconds (user and
/// system apart, with its minor page faults), the peak resident memory
/// sampled while it ran, each label's peak live bytes, and what the
/// allocator holds at its start and end.
fn phase(name: &str, work: impl FnOnce() -> String) {
    let _ = label_peaks();
    let at_start = take_resident_peak();
    let malloc_at_start = malloc_figures();
    let (user, system, faults) = usage();
    let started = Instant::now();
    let detail = work();
    let wall = started.elapsed().as_secs_f64();
    let (user_end, system_end, faults_end) = usage();
    let (user, system) = (user_end - user, system_end - system);
    println!(
        "{}",
        serde_json::json!({
            "phase": name,
            "wall_s": (wall * 100.0).round() / 100.0,
            "cpu_s": ((user + system) * 100.0).round() / 100.0,
            "user_s": (user * 100.0).round() / 100.0,
            "sys_s": (system * 100.0).round() / 100.0,
            "minor_faults": faults_end.saturating_sub(faults),
            "resident_mib_at_start": mib(at_start),
            "resident_peak_mib": mib(take_resident_peak()),
            "label_peak_mib": label_peaks(),
            "malloc_at_start": malloc_at_start,
            "malloc_at_end": malloc_figures(),
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

struct Stage {
    name: &'static str,
    started: Instant,
    cpu: (f64, f64, u64),
    live: isize,
}

static STAGE: Mutex<Option<Stage>> = Mutex::new(None);

/// Stage peaks count the whole Rust heap, including allocations made by a
/// previous stage. Counting is opt-in because atomics on every allocation
/// perturb CPU timings. C-library memory is visible only in the RSS samples.
fn stage(name: &'static str) {
    if !STAGE_TIMING.load(Ordering::Relaxed) {
        return;
    }
    let mut current = STAGE.lock().unwrap();
    let live = LIVE.load(Ordering::Relaxed);
    let peak = PEAK.swap(live, Ordering::Relaxed);
    let rss = resident_bytes().unwrap_or(0);
    let rss_peak = STAGE_RSS.swap(rss, Ordering::Relaxed).max(rss);
    let cpu = usage();
    if let Some(previous) = current.take() {
        println!(
            "{}",
            serde_json::json!({
                "refresh_stage": previous.name,
                "wall_s": previous.started.elapsed().as_secs_f64(),
                "cpu_s": cpu.0 + cpu.1 - previous.cpu.0 - previous.cpu.1,
            "live_bytes_at_start": COUNTING.load(Ordering::Relaxed).then_some(previous.live),
            "peak_live_bytes": COUNTING.load(Ordering::Relaxed).then_some(peak),
            "live_bytes_at_end": COUNTING.load(Ordering::Relaxed).then_some(live),
                "resident_peak_bytes": rss_peak,
                "malloc_at_end": malloc_figures(),
            })
        );
    }
    if name != "end" {
        *current = Some(Stage {
            name,
            started: Instant::now(),
            cpu: usage(),
            live,
        });
    }
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
            |stats| {
                let debug = format!("{stats:?}");
                format!(
                    "rebound {}, resolved {}, replayed {}, dispatch replayed {}, recomputed {}, \
                     sites {}, touched {}; \
                     update {} ms (facts {}, bind {}, resolve {}, dispatch {}, sites {})",
                    stats.rebound,
                    stats.resolved,
                    stats.replayed,
                    stats.dispatch_replayed,
                    stats.dispatch_recomputed,
                    stats.sites_resolved,
                    stats.touched.len(),
                    stat(&debug, "total_ms"),
                    stat(&debug, "facts_ms"),
                    stat(&debug, "bind_ms"),
                    stat(&debug, "resolve_ms"),
                    stat(&debug, "dispatch_ms"),
                    stat(&debug, "sites_ms")
                )
            },
        )
    )
}

/// A count of the update statistics `debug` prints (`{:?}` of
/// `UpdateStats`), by field name; 0 when it has no such field. Read from
/// the debug form so this probe also builds against commits whose
/// statistics lack the field, to measure them alike.
fn stat(debug: &str, name: &str) -> usize {
    debug
        .split(&format!(" {name}: "))
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
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
        with_cache_counts(&layer, || edit_and_refresh(&worktree, &dir))
    });
    println!("engine parts {:?}", worktree.engine_memory_parts());
    // A restart: a new layer, so nothing the first process decoded or
    // cached is reused, and the sibling bound again from the store. With
    // `PROBE_TRIM=1` the allocator's free memory is handed back first, to
    // tell what the first refresh left in the allocator from what it left
    // elsewhere in the process.
    drop(sibling);
    if std::env::var("PROBE_TRIM").is_ok_and(|value| value == "1") {
        phase("trim", || {
            trim_allocator();
            String::new()
        });
    }
    let restart_layer = Arc::new(DataLayer::new(256 << 20, threads));
    let restarted =
        Checkout::bind(Arc::clone(&restart_layer), &store, family, &sibling_dir).unwrap();
    phase("restart_first_refresh", || {
        with_cache_counts(&restart_layer, || {
            edit_and_refresh(&restarted, &sibling_dir)
        })
    });
}

/// `work`'s detail, followed by the block reads it made through `layer`'s
/// cache: how many found their block cached, and how many read and
/// checked it from its segment.
fn with_cache_counts(layer: &DataLayer, work: impl FnOnce() -> String) -> String {
    let before = layer.cache_stats();
    let detail = work();
    let after = layer.cache_stats();
    format!(
        "{detail}; block cache hits {}, misses {}",
        after.hits - before.hits,
        after.misses - before.misses
    )
}

/// Replays an edit pack through one checkout built from its base files, as
/// the runtime drives it: each edit recorded and refreshed, its changes
/// published every `publish_every` edits and at each commit's end, or with
/// `daemon`, as the runtime publishes them: once 64 paths changed or 256
/// files were resolved again since the publication, and at each commit's
/// end, which stands for the agent pausing long enough for the checkout to
/// go quiet. Prints how many refreshes found no engine and started one,
/// the time each kind took, and each refresh's wall time percentiles.
fn replay(mut args: impl Iterator<Item = String>) {
    use std::io::BufRead as _;
    let pack = PathBuf::from(args.next().unwrap());
    let publish_every = args.next().map_or(Some(8), |count| match count.as_str() {
        "daemon" => None,
        count => Some(count.parse::<usize>().unwrap()),
    });
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
    let mut walls = Vec::new();
    let mut publications = 0usize;
    let mut cold_builds = 0usize;
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
            cold_builds += usize::from(stats.as_ref().is_some_and(|stats| stats.cold_build));
            let wall = wall.elapsed().as_secs_f64();
            let cpu = cpu_seconds() - cpu;
            if stats.is_some() {
                let kind = if starts { &mut started } else { &mut reused };
                kind.0 += 1;
                kind.1 += wall;
                kind.2 += cpu;
                walls.push(wall * 1000.0);
            }
            since += 1;
            let commit_ends = edits
                .get(index + 1)
                .is_none_or(|next| next["commit"] != edit["commit"]);
            let due = match publish_every {
                Some(every) => since >= every,
                None => checkout.changed_paths() >= 64 || checkout.refreshed_files() >= 256,
            };
            if due || commit_ends {
                if checkout.publish_changes().unwrap().is_some() {
                    publications += 1;
                }
                since = 0;
            }
        }
        walls.sort_by(f64::total_cmp);
        let at = |p: f64| {
            walls
                .get(((walls.len().saturating_sub(1)) as f64 * p).round() as usize)
                .copied()
                .unwrap_or(0.0)
        };
        format!(
            "{} edits, {publications} publications, {cold_builds} cold switches; refreshes that started an engine: {} \
             ({:.1} s wall, {:.1} s CPU); that reused one: {} ({:.1} s wall, {:.1} s CPU); \
             all: {:.1} s wall, {:.1} s CPU, per refresh p50 {:.0} ms, p90 {:.0} ms, max {:.0} ms",
            edits.len(),
            started.0,
            started.1,
            started.2,
            reused.0,
            reused.1,
            reused.2,
            started.1 + reused.1,
            started.2 + reused.2,
            at(0.5),
            at(0.9),
            at(1.0)
        )
    });
}

/// One refresh in a process that has decoded nothing yet. The first run
/// with `dir` exports `repo` under it, builds it and exits; each later run
/// binds that checkout again as a restarted process does, puts the edited
/// file back as it was built, and makes the probe's edit and refreshes it.
fn restart(mut args: impl Iterator<Item = String>) {
    let repo = PathBuf::from(args.next().unwrap());
    let dir = PathBuf::from(args.next().unwrap());
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    start_tracing();
    let store = dir.join("store");
    let checkout_dir = dir.join("checkout");
    let original = dir.join("edited.original");
    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let family = FamilyId::from_digest(Digest128::of(b"alloc trace restart"));
    if !store.exists() {
        export(&repo, &checkout_dir);
        std::fs::copy(checkout_dir.join(EDITED), &original).unwrap();
        let checkout = Checkout::bind(layer, &store, family, &checkout_dir).unwrap();
        phase("build", || {
            let report = checkout.build().unwrap();
            format!("{} files, {} parsed", report.files, report.parsed_files)
        });
        return;
    }
    std::fs::copy(&original, checkout_dir.join(EDITED)).unwrap();
    let checkout = Checkout::bind(Arc::clone(&layer), &store, family, &checkout_dir).unwrap();
    phase("process_restart_first_refresh", || {
        with_cache_counts(&layer, || edit_and_refresh(&checkout, &checkout_dir))
    });
}

fn main() {
    let _ = env_logger::try_init();
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("overhead") => overhead(args),
        Some("burst") => burst(args),
        Some("refresh") => refresh(args),
        Some("restart") => restart(args),
        Some("replay") => replay(args),
        Some("transition") => transition(args),
        _ => {
            panic!("usage: alloc_trace_probe overhead|burst|refresh|restart|replay|transition ...")
        }
    }
}

/// Measures a revision transition using sibling admission, a refresh of an
/// already published checkout, or a cold rebuild reusing its stored records.
/// Roots must be exported source trees, not repositories with private files.
/// Their contents stay intact; the mutable checkout and store are private.
fn transition(mut args: impl Iterator<Item = String>) {
    let base = std::fs::canonicalize(args.next().unwrap()).unwrap();
    let target = std::fs::canonicalize(args.next().unwrap()).unwrap();
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    let strategy = args.next().unwrap_or_else(|| "sibling".to_string());
    assert!(["sibling", "refresh", "cold"].contains(&strategy.as_str()));
    let profiling = std::env::var_os("PROBE_STAGES").is_some();
    let stage_timing = profiling || std::env::var_os("PROBE_STAGE_TIMING").is_some();
    COUNTING.store(profiling, Ordering::Relaxed);
    STAGE_TIMING.store(stage_timing, Ordering::Relaxed);
    #[cfg(feature = "test-hooks")]
    if stage_timing {
        aft::data_layer::refresh_probe::set_observer(stage);
    }
    #[cfg(not(feature = "test-hooks"))]
    assert!(
        !stage_timing,
        "stage measurement requires --features test-hooks"
    );
    start_tracing();
    let scratch = tempfile::tempdir().unwrap();
    let store = scratch.path().join("store");
    let family = FamilyId::from_digest(Digest128::of(b"alloc trace transition"));
    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let root = scratch.path().join("checkout");
    copy_tree(&base, &root);
    let sibling = Checkout::bind(Arc::clone(&layer), &store, family, &root).unwrap();
    stage("base_start");
    phase("base_build", || {
        let report = sibling.build().unwrap();
        stage("end");
        format!("{report:?}")
    });
    let next = if strategy == "sibling" {
        let next = scratch.path().join("sibling");
        copy_tree(&target, &next);
        next
    } else {
        std::fs::remove_dir_all(&root).unwrap();
        copy_tree(&target, &root);
        root
    };
    stage("bind");
    let worktree = Checkout::bind(Arc::clone(&layer), &store, family, &next).unwrap();
    phase("target_build", || {
        with_cache_counts(&layer, || {
            let detail = if strategy == "refresh" {
                let stats = worktree.refresh_callgraph().unwrap().unwrap();
                let detail = format!(
                    "changed {}, rebound {}, resolved {}, replayed {}, dispatch replayed {}, recomputed {}, sites {}, touched {}; update {} ms (facts {}, bind {}, resolve {}, dispatch {}, sites {})",
                    stats.changed_paths, stats.rebound, stats.resolved, stats.replayed,
                    stats.dispatch_replayed, stats.dispatch_recomputed, stats.sites_resolved,
                    stats.touched.len(), stats.total_ms, stats.facts_ms, stats.bind_ms,
                    stats.resolve_ms, stats.dispatch_ms, stats.sites_ms,
                );
                stage("publication");
                worktree.publish_changes().unwrap();
                detail
            } else {
                format!("{:?}", worktree.build().unwrap())
            };
            stage("end");
            detail
        })
    });
    println!("engine parts {:?}", worktree.engine_memory_parts());
    #[cfg(feature = "test-hooks")]
    if std::env::var_os("PROBE_COMPARE").is_some() {
        let cold = Checkout::bind(
            Arc::clone(&layer),
            &scratch.path().join("cold-store"),
            family,
            &next,
        )
        .unwrap();
        cold.build().unwrap();
        let compared = aft::data_layer::checkout_gate::compare_engines(&worktree, &cold).unwrap();
        println!(
            "forced-switch comparison {}",
            serde_json::to_string(&compared).unwrap()
        );
    }
}

/// Copies an archive's source tree without following links or copying Git's
/// metadata. The empty marker makes ignore rules behave as in a repository.
fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    let mut archive = Command::new("tar")
        .args(["-c", "--exclude=.git", "-C"])
        .arg(source)
        .arg(".")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let status = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(destination)
        .stdin(archive.stdout.take().unwrap())
        .status()
        .unwrap();
    assert!(status.success() && archive.wait().unwrap().success());
    std::fs::create_dir_all(destination.join(".git")).unwrap();
}
