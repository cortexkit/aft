//! Measures what one data-layer checkout holds in memory while an edit
//! pack is replayed through it the way a running AFT drives it. Unlike the
//! checkout gate (`checkout_gate.rs`), it compares nothing, so no second
//! checkout, cold build or forced decoding adds to what it measures: the
//! numbers are the product's own, for the spec's peak-memory target (G9 in
//! the data-layer specification: peak resident memory at most 1 GB).
//!
//! The pack's base files are written to a folder, bound and built. Each
//! edit is then written to disk, recorded and resolved, as a reader that
//! asks for the call graph after every edit would have it. The changes are
//! published once more than 64 paths changed and at every commit end (the
//! checkout going quiet), and the checkout is built again whole once it
//! reads from too many segments (`DeltaReport::wants_build`), as the runtime
//! does. At every
//! commit end the allocator's free pages are handed back to the system, as
//! the daemon's allocator relief does.
//!
//! A sampler thread reads the process's resident memory and the
//! allocator's in-use bytes every 100 ms, so each line reports the peak
//! between two samples as well as the value at the sample. One JSON line
//! per edit, and per build and commit end, goes to stdout; a summary to
//! stderr. Each edit's line also says how long recording it took
//! (`record_ms`). With PROBE_PARTS=1 a line breaks the call graph's memory
//! down part by part, the engine's and the rest's; with PROBE_ALLOC=1 it
//! says how many bytes recording the edit and refreshing the call graph
//! allocated, and in how many allocations, and the summary adds them up.
//!
//! With PROBE_ENGINES=N, N more checkouts of two small files each are
//! bound, built, edited and refreshed before the pack is replayed, so N + 1
//! call-graph engines are alive at once. Every edit's line says how many
//! threads the process runs (`threads`) and how many of them are resolver
//! threads (`resolver_threads`, named `aft-resolve-N`; Linux only).
//!
//! ```text
//! cargo run --profile ci-storm -p agent-file-tools --example checkout_memory_probe -- <pack.jsonl.gz> [threads]
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::io::BufRead;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aft::data_layer::checkout::{Checkout, DataLayer};
use aft_store::ids::{Digest128, FamilyId};

/// Paths changed past which the runtime publishes the changes at once.
const PUBLISH_AT: usize = 64;
/// Files re-resolved since the last publication at which the runtime
/// publishes the changes too. With PROBE_REFRESHED_RULE=1 the probe applies
/// it, and each edit's line also says what the call graph held after the
/// refresh and before that publication (`refreshed_callgraph_mib`).
const PUBLISH_AT_REFRESHED: usize = 256;

/// Whether [`Counting`] counts: with PROBE_ALLOC=1, each edit's line says
/// how many bytes recording it and refreshing the call graph allocated, and
/// in how many allocations.
static COUNTING: AtomicBool = AtomicBool::new(false);
/// Bytes allocated since the process started, while counting.
static ALLOCATED: AtomicU64 = AtomicU64::new(0);
/// Allocations made since the process started, while counting; a
/// reallocation is not counted as one.
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// The system allocator, counting the bytes each allocation asks for.
struct Counting;

impl Counting {
    fn count(size: usize) {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(size as u64, Ordering::Relaxed);
        }
    }

    fn count_new(size: usize) {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(size as u64, Ordering::Relaxed);
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// SAFETY: every call is passed to the system allocator unchanged.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        Self::count_new(layout.size());
        // SAFETY: the caller's contract is the system allocator's.
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        Self::count_new(layout.size());
        // SAFETY: as above.
        unsafe { std::alloc::System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        Self::count(new_size.saturating_sub(layout.size()));
        // SAFETY: as above.
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: as above.
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The process's resident memory in MiB, from `/proc/self/status`.
fn rss_mib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kib| kib.parse::<u64>().ok())
        .map_or(0, |kib| kib / 1024)
}

/// The process's threads, and how many of them are resolver threads
/// (named `aft-resolve-N`), from `/proc/self/task`; zeros elsewhere.
fn thread_counts() -> (usize, usize) {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return (0, 0);
    };
    let names = tasks
        .flatten()
        .map(|task| std::fs::read_to_string(task.path().join("comm")).unwrap_or_default())
        .collect::<Vec<_>>();
    let resolvers = names
        .iter()
        .filter(|name| name.starts_with("aft-resolve"))
        .count();
    (names.len(), resolvers)
}

/// Binds, builds, edits and refreshes a checkout of two small files under
/// `folder`, so it holds a call-graph engine, and returns it.
fn small_engine(layer: &Arc<DataLayer>, folder: &Path, index: usize) -> Checkout {
    let root = folder.join("files");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(folder.join("store")).unwrap();
    let lib = "export function run() { return 1; }\n";
    let main = "import { run } from './lib';\nexport function main() { run(); }\n";
    std::fs::write(root.join("lib.ts"), lib).unwrap();
    std::fs::write(root.join("main.ts"), main).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    let family = FamilyId::from_digest(Digest128::of(
        format!("checkout memory probe engine {index}").as_bytes(),
    ));
    let checkout = Checkout::bind(Arc::clone(layer), &folder.join("store"), family, &root).unwrap();
    checkout.build().unwrap();
    std::fs::write(
        root.join("lib.ts"),
        "export function run() { return 2; }\nexport function stop() {}\n",
    )
    .unwrap();
    checkout.record_paths(&[b"lib.ts".to_vec()]).unwrap();
    checkout.refresh_callgraph().unwrap();
    checkout
}

/// The bytes glibc's allocator hands out now and holds from the system, in
/// MiB (`mallinfo2`); zero elsewhere.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn allocator_mib() -> (u64, u64) {
    type Mallinfo2 = unsafe extern "C" fn() -> libc::mallinfo2;
    // SAFETY: looking a symbol up by name has no other effect.
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"mallinfo2".as_ptr()) };
    if symbol.is_null() {
        return (0, 0);
    }
    // SAFETY: glibc declares mallinfo2 as `struct mallinfo2 (*)(void)`.
    let mallinfo2 = unsafe { std::mem::transmute::<*mut libc::c_void, Mallinfo2>(symbol) };
    // SAFETY: mallinfo2 only reads the allocator's counters.
    let info = unsafe { mallinfo2() };
    let mapped = info.hblkhd as u64;
    (
        (info.uordblks as u64 + mapped) >> 20,
        (info.arena as u64 + mapped) >> 20,
    )
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn allocator_mib() -> (u64, u64) {
    (0, 0)
}

/// The highest resident memory and allocator in-use bytes seen since the
/// last [`Peaks::take`], read every 100 ms on a thread of its own.
struct Peaks {
    rss: AtomicU64,
    in_use: AtomicU64,
    stop: AtomicBool,
}

impl Peaks {
    fn start() -> Arc<Self> {
        let peaks = Arc::new(Self {
            rss: AtomicU64::new(0),
            in_use: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        let sampler = Arc::clone(&peaks);
        std::thread::spawn(move || {
            while !sampler.stop.load(Ordering::Acquire) {
                sampler.rss.fetch_max(rss_mib(), Ordering::AcqRel);
                sampler
                    .in_use
                    .fetch_max(allocator_mib().0, Ordering::AcqRel);
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        peaks
    }

    /// The peaks since the last call, counting the values now too.
    fn take(&self) -> (u64, u64) {
        (
            self.rss.swap(0, Ordering::AcqRel).max(rss_mib()),
            self.in_use.swap(0, Ordering::AcqRel).max(allocator_mib().0),
        )
    }
}

/// One sample of what the process and the checkout hold.
fn sample(
    label: &str,
    commit: u64,
    path: &str,
    peaks: &Peaks,
    checkout: &Checkout,
    layer: &DataLayer,
) -> serde_json::Value {
    let (peak_rss, peak_in_use) = peaks.take();
    let (in_use, held) = allocator_mib();
    let memory = checkout.memory(&mut aft_store::heap::Seen::new());
    let live = memory.live.unwrap_or_default();
    // The engine part by part, in MiB, when PROBE_PARTS=1: walking every
    // decoded file costs time, so it is off unless asked for.
    let parts = (std::env::var_os("PROBE_PARTS").is_some_and(|value| value == "1"))
        .then(|| checkout.engine_memory_parts())
        .flatten()
        .map(|parts| {
            parts
                .into_iter()
                .map(|(name, bytes)| (name.to_string(), serde_json::json!(bytes >> 20)))
                .collect::<serde_json::Map<_, _>>()
        });
    // Likewise the call graph's part of what the checkout holds apart from
    // its engine (`callgraph_mib`), part by part.
    let callgraph_parts = (std::env::var_os("PROBE_PARTS").is_some_and(|value| value == "1"))
        .then(|| checkout.callgraph_memory_parts())
        .flatten()
        .map(|parts| {
            parts
                .into_iter()
                .map(|(name, bytes)| (name.to_string(), serde_json::json!(bytes >> 20)))
                .collect::<serde_json::Map<_, _>>()
        });
    serde_json::json!({
        "engine_parts_mib": parts,
        "callgraph_parts_mib": callgraph_parts,
        "label": label,
        "commit": commit,
        "path": path,
        "rss_mib": rss_mib(),
        "peak_rss_mib": peak_rss,
        "in_use_mib": in_use,
        "peak_in_use_mib": peak_in_use,
        "held_mib": held,
        "cache_mib": layer.cache_stats().resident_bytes >> 20,
        "engine": format!("{:?}", memory.engine),
        "changes": live.changes,
        "text_overlay_paths": live.text_overlay_paths,
        "text_overlay_kib": live.text_overlay_bytes >> 10,
        "shared_kib": live.shared_bytes >> 10,
        "refreshed_files": live.refreshed_files,
        "callgraph_mib": live.callgraph_bytes >> 20,
    })
}

/// Publishes the checkout's changes, and builds it again whole once it
/// reads from too many segments, as the runtime does. Returns what it did.
fn publish(checkout: &Checkout) -> Option<String> {
    let report = checkout.publish_changes().unwrap()?;
    if report.wants_build() {
        checkout.build().unwrap();
        return Some(format!(
            "published and built again ({} segments)",
            report.segments
        ));
    }
    Some(format!("published ({} segments)", report.segments))
}

fn main() {
    COUNTING.store(
        std::env::var_os("PROBE_ALLOC").is_some_and(|value| value == "1"),
        Ordering::Relaxed,
    );
    let refreshed_rule = std::env::var_os("PROBE_REFRESHED_RULE").is_some_and(|value| value == "1");
    let mut args = std::env::args().skip(1);
    let pack = args.next().unwrap();
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    let peaks = Peaks::start();
    let start_rss = rss_mib();

    let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(
        std::fs::File::open(&pack).unwrap(),
    ));
    let mut lines = reader.lines();
    let first: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    // PROBE_COMMITS=N replays only the pack's first N commits: a small run
    // to try the probe on before the whole pack.
    let commits = std::env::var("PROBE_COMMITS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok());
    let mut seen_commits = Vec::new();
    let edits = lines
        .map(|line| serde_json::from_str::<serde_json::Value>(&line.unwrap()).unwrap())
        .filter(|value| value["type"] == "edit")
        .take_while(|value| {
            let commit = value["commit"].clone();
            if !seen_commits.contains(&commit) {
                seen_commits.push(commit);
            }
            commits.is_none_or(|limit| seen_commits.len() <= limit)
        })
        .collect::<Vec<_>>();
    let safe = |path: &str| {
        !path.is_empty()
            && !path.starts_with('/')
            && !path.split('/').any(|part| part == ".." || part == ".git")
    };

    let work = std::env::temp_dir().join(format!("aft-memory-probe-{}", std::process::id()));
    let root = work.join("files");
    let store = work.join("store");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(&store).unwrap();
    let write = |path: &str, text: &str| {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    };
    for (path, text) in first["files"].as_object().unwrap() {
        if safe(path) {
            write(path, text.as_str().unwrap_or_default());
        }
    }
    let root = std::fs::canonicalize(&root).unwrap();
    // The pack's text, held to replay it, is part of this process too.
    let loaded_rss = rss_mib();

    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    // PROBE_ENGINES=N: N more checkouts, each holding a call-graph engine
    // while the pack is replayed.
    let engines = std::env::var("PROBE_ENGINES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let others = (0..engines)
        .map(|index| small_engine(&layer, &work.join(format!("engine-{index}")), index))
        .collect::<Vec<_>>();
    if engines > 0 {
        let (threads, resolvers) = thread_counts();
        eprintln!(
            "{engines} small checkouts hold engines: {threads} threads, {resolvers} resolver threads"
        );
    }
    let family = FamilyId::from_digest(Digest128::of(b"checkout memory probe"));
    let checkout = Checkout::bind(Arc::clone(&layer), Path::new(&store), family, &root).unwrap();
    peaks.take();
    let started = Instant::now();
    let report = checkout.build_measuring_memory().unwrap();
    let built = sample("built", 0, "", &peaks, &checkout, &layer);
    println!("{built}");
    println!(
        "{}",
        serde_json::json!({
            "label": "cold build parts",
            "scan_ms": report.scan_ms,
            "resolve_ms": report.resolve_ms,
            "write_ms": report.write_ms,
            "publish_ms": report.publish_ms,
            // `ms.<stage>` entries are stage times in milliseconds, the rest
            // bytes shown in MiB.
            "parts": report
                .memory
                .iter()
                .map(|(part, value)| {
                    let shown = if part.starts_with("ms.") { *value } else { value >> 20 };
                    (part.clone(), serde_json::json!(shown))
                })
                .collect::<serde_json::Map<_, _>>(),
        })
    );
    eprintln!(
        "process before binding {start_rss} MiB, with the pack loaded {loaded_rss} MiB; cold build {:.1} s",
        started.elapsed().as_secs_f64()
    );

    let mut worst_edit = (0u64, String::new());
    let mut worst_in_use = 0u64;
    let mut most_after_relief = 0u64;
    // Each edit's record-and-refresh time, without its publication, for
    // comparing builds of the read path.
    let mut refresh_times = Vec::new();
    // Each edit's recording time alone (`record_ms`).
    let mut record_times = Vec::new();
    // With PROBE_ALLOC=1, what recording and refreshing allocated over the
    // whole run: (bytes, allocations) each.
    let mut record_total = (0u64, 0u64);
    let mut refresh_total = (0u64, 0u64);
    for (index, edit) in edits.iter().enumerate() {
        let path = edit["path"].as_str().unwrap_or_default();
        if !safe(path) {
            continue;
        }
        let commit = edit["commit"].as_u64().unwrap_or_default();
        if edit["kind"] == "delete" {
            let _ = std::fs::remove_file(root.join(path));
        } else {
            write(path, edit["text"].as_str().unwrap_or_default());
        }
        let edit_started = std::time::Instant::now();
        let allocated = ALLOCATED.load(Ordering::Relaxed);
        let allocations = ALLOCATIONS.load(Ordering::Relaxed);
        checkout.record_paths(&[path.as_bytes().to_vec()]).unwrap();
        let record_ms = edit_started.elapsed().as_secs_f64() * 1000.0;
        let record_allocated = ALLOCATED.load(Ordering::Relaxed) - allocated;
        let record_allocations = ALLOCATIONS.load(Ordering::Relaxed) - allocations;
        checkout.refresh_callgraph().unwrap();
        let refresh_ms = edit_started.elapsed().as_secs_f64() * 1000.0;
        let refresh_allocated = ALLOCATED.load(Ordering::Relaxed) - allocated - record_allocated;
        let refresh_allocations =
            ALLOCATIONS.load(Ordering::Relaxed) - allocations - record_allocations;
        refresh_times.push(refresh_ms);
        record_times.push(record_ms);
        record_total.0 += record_allocated;
        record_total.1 += record_allocations;
        refresh_total.0 += refresh_allocated;
        refresh_total.1 += refresh_allocations;
        let mut label = "edit".to_string();
        // With the runtime's second rule, what the refresh held before the
        // publication it triggers, as the line after it no longer shows it.
        let mut before_publishing = None;
        let many_refreshed = refreshed_rule && checkout.refreshed_files() >= PUBLISH_AT_REFRESHED;
        if refreshed_rule {
            let memory = checkout.memory(&mut aft_store::heap::Seen::new());
            let live = memory.live.unwrap_or_default();
            before_publishing = Some((live.callgraph_bytes >> 20, live.refreshed_files));
        }
        if checkout.changed_paths() > PUBLISH_AT || many_refreshed {
            if let Some(done) = publish(&checkout) {
                label = format!("edit, then {done}");
            }
        }
        let mut line = sample(&label, commit, path, &peaks, &checkout, &layer);
        if let Some((callgraph_mib, refreshed_files)) = before_publishing {
            line["refreshed_callgraph_mib"] = serde_json::json!(callgraph_mib);
            line["refreshed_files_before_publishing"] = serde_json::json!(refreshed_files);
        }
        line["refresh_ms"] = serde_json::json!((refresh_ms * 10.0).round() / 10.0);
        line["record_ms"] = serde_json::json!((record_ms * 100.0).round() / 100.0);
        let (threads, resolver_threads) = thread_counts();
        line["threads"] = serde_json::json!(threads);
        line["resolver_threads"] = serde_json::json!(resolver_threads);
        line["engines"] = serde_json::json!(others.len() + 1);
        if COUNTING.load(Ordering::Relaxed) {
            line["record_alloc_kib"] = serde_json::json!(record_allocated >> 10);
            line["refresh_alloc_kib"] = serde_json::json!(refresh_allocated >> 10);
            line["record_allocs"] = serde_json::json!(record_allocations);
            line["refresh_allocs"] = serde_json::json!(refresh_allocations);
        }
        let peak = line["peak_rss_mib"].as_u64().unwrap_or_default();
        if peak > worst_edit.0 {
            worst_edit = (peak, path.to_string());
        }
        worst_in_use = worst_in_use.max(line["peak_in_use_mib"].as_u64().unwrap_or_default());
        println!("{line}");
        let last_of_commit = edits
            .get(index + 1)
            .is_none_or(|next| next["commit"] != edit["commit"]);
        if last_of_commit {
            let done = publish(&checkout).unwrap_or_else(|| "nothing to publish".to_string());
            println!(
                "{}",
                sample(
                    &format!("commit end: {done}"),
                    commit,
                    "",
                    &peaks,
                    &checkout,
                    &layer
                )
            );
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let _ = aft::memory::relieve_allocator_pressure();
            let relieved = sample(
                "after allocator relief",
                commit,
                "",
                &peaks,
                &checkout,
                &layer,
            );
            most_after_relief =
                most_after_relief.max(relieved["rss_mib"].as_u64().unwrap_or_default());
            println!("{relieved}");
        }
    }
    peaks.stop.store(true, Ordering::Release);
    record_times.sort_by(f64::total_cmp);
    let record_at = |fraction: f64| {
        record_times
            .get(((record_times.len().saturating_sub(1)) as f64 * fraction).round() as usize)
            .copied()
            .unwrap_or_default()
    };
    eprintln!(
        "record: {} edits, total {:.0} ms, p50 {:.2} ms, p90 {:.2} ms, max {:.2} ms",
        record_times.len(),
        record_times.iter().sum::<f64>(),
        record_at(0.5),
        record_at(0.9),
        record_at(1.0)
    );
    if COUNTING.load(Ordering::Relaxed) {
        eprintln!(
            "allocated: recording {} KiB in {} allocations, refreshing {} KiB in {} allocations",
            record_total.0 >> 10,
            record_total.1,
            refresh_total.0 >> 10,
            refresh_total.1
        );
    }
    refresh_times.sort_by(f64::total_cmp);
    let at = |fraction: f64| {
        refresh_times
            .get(((refresh_times.len().saturating_sub(1)) as f64 * fraction).round() as usize)
            .copied()
            .unwrap_or_default()
    };
    eprintln!(
        "refresh: {} edits, total {:.0} ms, p50 {:.1} ms, p90 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
        refresh_times.len(),
        refresh_times.iter().sum::<f64>(),
        at(0.5),
        at(0.9),
        at(0.99),
        at(1.0)
    );
    eprintln!(
        "cold build: peak {} MiB resident, {} MiB in use, {} MiB after; edits: peak {} MiB resident (at {}), {} MiB in use; \
         at commit ends after allocator relief, at most {} MiB resident",
        built["peak_rss_mib"],
        built["peak_in_use_mib"],
        built["rss_mib"],
        worst_edit.0,
        worst_edit.1,
        worst_in_use,
        most_after_relief
    );
    drop(checkout);
    drop(others);
    let _ = std::fs::remove_dir_all(&work);
}
