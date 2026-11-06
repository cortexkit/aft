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
//! reads from more than eight segments, as the runtime does. At every
//! commit end the allocator's free pages are handed back to the system, as
//! the daemon's allocator relief does.
//!
//! A sampler thread reads the process's resident memory and the
//! allocator's in-use bytes every 100 ms, so each line reports the peak
//! between two samples as well as the value at the sample. One JSON line
//! per edit, and per build and commit end, goes to stdout; a summary to
//! stderr.
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
/// Segments past which the runtime builds a checkout again whole.
const COMPACT_AT: usize = 8;

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
    serde_json::json!({
        "engine_parts_mib": parts,
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
    })
}

/// Publishes the checkout's changes, and builds it again whole once it
/// reads from more than [`COMPACT_AT`] segments. Returns what it did.
fn publish(checkout: &Checkout) -> Option<String> {
    let report = checkout.publish_changes().unwrap()?;
    if report.segments > COMPACT_AT {
        checkout.build().unwrap();
        return Some(format!(
            "published and built again ({} segments)",
            report.segments
        ));
    }
    Some(format!("published ({} segments)", report.segments))
}

fn main() {
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
            "parts_mib": report
                .memory
                .iter()
                .map(|(part, bytes)| (part.clone(), serde_json::json!(bytes >> 20)))
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
        checkout.record_paths(&[path.as_bytes().to_vec()]).unwrap();
        checkout.refresh_callgraph().unwrap();
        let mut label = "edit".to_string();
        if checkout.changed_paths() > PUBLISH_AT {
            if let Some(done) = publish(&checkout) {
                label = format!("edit, then {done}");
            }
        }
        let line = sample(&label, commit, path, &peaks, &checkout, &layer);
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
    let _ = std::fs::remove_dir_all(&work);
}
