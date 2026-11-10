//! Measures folding a checkout's segments against building it again whole,
//! from the same start: a checkout reading from many change segments.
//!
//! The checkout at `<root>` is bound to a family store under `<work>`,
//! built cold, and then given `rounds` publications of changes, each
//! appending a comment to `per_round` source files (rotating through them),
//! so it reads from about `rounds` segments. Its segments are then folded
//! ([`Checkout::fold`]); the same number of publications follows, and it is
//! built again whole, as AFT did before folding existed. For each step one
//! line reports the wall time, the CPU time the process spent, and the
//! process's physical footprint (macOS `phys_footprint`, sampled every
//! 10 ms) before, at its peak during, and after the step.
//!
//! A process that has built keeps much of what it freed, so the footprint
//! a later step adds understates it. `mode` runs one part per process:
//! `setup` builds and publishes, leaving the store reading from many
//! segments; `fold` and `build` each bind that store in a fresh process
//! and fold or build it whole. Give `fold` and `build` copies of the store
//! `setup` left, so both start from the same one. `all` (the default) runs
//! every step in one process.
//!
//! Edits are written to `<root>`, so give it a copy of the repository, and
//! give `<work>` a throwaway folder: nothing is written anywhere else.
//!
//! ```text
//! cargo run --profile ci-storm -p agent-file-tools --example fold_measure -- <root> <work> [mode] [rounds] [per_round] [threads] [cache_mib]
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aft::data_layer::checkout::{Checkout, DataLayer, Folded};
use aft_store::ids::{Digest128, FamilyId};

/// The process's physical footprint in bytes.
#[cfg(target_os = "macos")]
fn footprint() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: `usage` is a buffer of the size RUSAGE_INFO_V4 writes.
    let rc = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            usage.as_mut_ptr().cast(),
        )
    };
    if rc != 0 {
        return 0;
    }
    // SAFETY: the call succeeded, so it filled the buffer.
    unsafe { usage.assume_init() }.ri_phys_footprint
}

/// The process's resident memory in bytes, where `phys_footprint` is not
/// available.
#[cfg(not(target_os = "macos"))]
fn footprint() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|statm| statm.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * 4096)
}

/// User and system CPU time the process spent, in milliseconds.
#[cfg(unix)]
fn cpu_ms() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is a buffer of the size getrusage writes.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return 0.0;
    }
    // SAFETY: the call succeeded, so it filled the buffer.
    let usage = unsafe { usage.assume_init() };
    let ms = |time: libc::timeval| time.tv_sec as f64 * 1000.0 + time.tv_usec as f64 / 1000.0;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

/// Windows has no getrusage; the report shows wall time only there.
#[cfg(not(unix))]
fn cpu_ms() -> f64 {
    0.0
}

/// Runs `step`, sampling the footprint meanwhile, and prints what it took.
fn measure<T>(name: &str, step: impl FnOnce() -> T) -> T {
    // Free pages the allocator still holds from earlier steps are handed
    // back first, so the footprint before is what the process holds.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let _ = aft::memory::relieve_allocator_pressure();
    let before = footprint();
    let peak = Arc::new(AtomicU64::new(before));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, stop) = (Arc::clone(&peak), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                peak.fetch_max(footprint(), Ordering::AcqRel);
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let cpu = cpu_ms();
    let started = Instant::now();
    let result = step();
    let wall = started.elapsed().as_secs_f64() * 1000.0;
    let cpu = cpu_ms() - cpu;
    stop.store(true, Ordering::Release);
    sampler.join().unwrap();
    let after = footprint();
    let peak = peak.load(Ordering::Acquire).max(after);
    let mib = |bytes: u64| bytes as f64 / f64::from(1 << 20);
    println!(
        "{name}: {wall:.0} ms wall, {cpu:.0} ms CPU; footprint {:.0} MiB before, {:.0} MiB peak (+{:.0}), {:.0} MiB after",
        mib(before),
        mib(peak),
        mib(peak.saturating_sub(before)),
        mib(after)
    );
    result
}

/// Every file under `root` in a language the call graph parses, outside
/// hidden and dependency folders, in path order.
fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).unwrap().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                folders.push(path);
            } else if kind.is_file()
                && ["ts", "tsx", "rs", "py", "go"]
                    .iter()
                    .any(|extension| name.ends_with(&format!(".{extension}")))
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Publishes `rounds` rounds of changes, each a comment appended to
/// `per_round` of `files` (taken in turn from `next`), and returns the
/// segments the checkout reads from after the last.
fn publish_rounds(
    checkout: &Checkout,
    root: &Path,
    files: &[PathBuf],
    next: &mut usize,
    rounds: usize,
    per_round: usize,
) -> usize {
    let mut segments = 0;
    for round in 0..rounds {
        let mut paths = Vec::new();
        for _ in 0..per_round {
            let file = &files[*next % files.len()];
            *next += 1;
            let mut text = std::fs::read(file).unwrap();
            text.extend_from_slice(format!("\n// fold measure {next}\n").as_bytes());
            std::fs::write(file, text).unwrap();
            paths.push(
                file.strip_prefix(root)
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec(),
            );
        }
        checkout.record_paths(&paths).unwrap();
        checkout.refresh_callgraph().unwrap();
        let report = checkout
            .publish_changes()
            .unwrap()
            .expect("the changes publish");
        segments = report.read_segments;
        eprintln!(
            "round {round}: {} paths, {} segments ({} its own), wants a fold: {}",
            report.paths,
            report.read_segments,
            report.segments,
            report.wants_fold()
        );
    }
    segments
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let root = std::fs::canonicalize(&args[1]).unwrap();
    let work = PathBuf::from(&args[2]);
    let mode = args.get(3).map_or("all", String::as_str);
    let rounds = args.get(4).map_or(20, |value| value.parse().unwrap());
    let per_round = args.get(5).map_or(5, |value| value.parse().unwrap());
    let threads = args.get(6).map_or_else(
        || std::thread::available_parallelism().map_or(4, usize::from),
        |value| value.parse().unwrap(),
    );
    let store = work.join("store");
    std::fs::create_dir_all(&store).unwrap();
    // The block cache the running AFT gives its data layer, unless asked
    // for another size.
    let cache_mib: usize = args.get(7).map_or(256, |value| value.parse().unwrap());
    let layer = Arc::new(DataLayer::new(cache_mib << 20, threads));
    let family = FamilyId::from_digest(Digest128::of(b"fold measure"));
    let checkout = measure("bind", || {
        Checkout::bind(Arc::clone(&layer), &store, family, &root).unwrap()
    });
    let files = source_files(&root);
    assert!(
        !files.is_empty(),
        "no source files under {}",
        root.display()
    );
    match mode {
        "fold" => {
            fold(&checkout, &layer);
            return;
        }
        "build" => {
            build_again(&checkout, &layer);
            return;
        }
        "setup" | "all" => {}
        other => panic!("unknown mode {other}"),
    }

    let built = measure("cold build", || checkout.build().unwrap());
    println!(
        "  {} files, {} parsed, {} segment bytes",
        built.files, built.parsed_files, built.segment_bytes
    );
    let mut next = 0;
    let segments = publish_rounds(&checkout, &root, &files, &mut next, rounds, per_round);
    println!(
        "after {rounds} publications: reads {segments} segments; the store holds {}",
        checkout.store_segments().unwrap()
    );
    if mode == "setup" {
        return;
    }
    fold(&checkout, &layer);
    let segments = publish_rounds(&checkout, &root, &files, &mut next, rounds, per_round);
    println!(
        "after {rounds} more publications: reads {segments} segments; the store holds {}",
        checkout.store_segments().unwrap()
    );
    build_again(&checkout, &layer);
}

fn fold(checkout: &Checkout, layer: &DataLayer) {
    let folded = measure("fold", || checkout.fold().unwrap());
    match folded {
        Folded::Done(report) => println!(
            "  folded {} segments into one of {} bytes: {} text units, {} records, {} fragments, {} segments retired; the store holds {}",
            report.read_before,
            report.segment_bytes,
            report.text_units,
            report.records,
            report.fragments,
            report.retired_segments,
            checkout.store_segments().unwrap()
        ),
        other => println!("  did not fold: {other:?}"),
    }
    println!(
        "  the block cache holds {} MiB",
        layer.cache_stats().resident_bytes >> 20
    );
}

fn build_again(checkout: &Checkout, layer: &DataLayer) {
    let rebuilt = measure("whole build (before folding existed)", || {
        checkout.build().unwrap()
    });
    println!(
        "  {} files, {} parsed ({} records read from the store); the store holds {}",
        rebuilt.files,
        rebuilt.parsed_files,
        rebuilt.stored_records,
        checkout.store_segments().unwrap()
    );
    println!(
        "  the block cache holds {} MiB",
        layer.cache_stats().resident_bytes >> 20
    );
}
