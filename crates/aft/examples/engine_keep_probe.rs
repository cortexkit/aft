//! Measures what keeping a checkout's call-graph engine across its
//! publications holds and saves.
//!
//! ```text
//! cargo run --profile ci-storm -p agent-file-tools --example engine_keep_probe -- \
//!     replay <pack.jsonl.gz> [publish_every] [threads]
//! ```
//!
//! Replays an edit pack through one checkout built from its base files, as
//! the runtime drives it: each edit recorded and refreshed, its changes
//! published every `publish_every` edits (8 by default) and at each
//! commit's end. After every refresh it measures the engine as the engine
//! budget counts it, and how long that walk took. Prints one JSON line
//! every 50 edits and a summary: engine starts, refresh time, and the
//! engine's bytes and walk times.
//!
//! `roots <budget_mib> <rounds> <idle_seconds> <root> <edit_path> ...`
//! builds source-only copies, then alternates edits and publications across
//! them. Each root must be a disposable copy: this mode rewrites edit_path.
//! It reports component bytes before/after publication and after an idle
//! pause, plus per-refresh CPU, wall time, starts, releases and sampled RSS.
//! Set TMPDIR to a scratch directory to isolate its temporary index store.
//! With PROBE_COMPARE=1, first warm at 8 GiB, then visit the same checkouts
//! at the requested budget, without repeating cold builds. Both arms start
//! after a broad edit and idle pause; the second arm reverses the refactor.
//! PROBE_WARM_DIFF
//! and PROBE_WARM_PATHS name a real refactor patch and its changed-path list;
//! the patch is applied only to copies of this repository before the visits.
//!
//! `paired <copy> <edit-path>` accepts `cold <sequence>`, `edit <sequence>` and
//! `census <sequence>` lines on stdin, replying with one JSON line per command.
//! A coordinator can keep base and candidate binaries alive and alternate them
//! after every edit. Each `cold` command uses a fresh data layer and store.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::io::BufRead as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aft::data_layer::checkout::{Checkout, DataLayer};
use aft_store::ids::{Digest128, FamilyId};

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

#[cfg(not(unix))]
fn cpu_seconds() -> f64 {
    0.0
}

fn percentile(values: &[f64], p: f64) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    values
        .get(((values.len().saturating_sub(1)) as f64 * p).round() as usize)
        .copied()
        .unwrap_or(0.0)
}

fn replay(mut args: impl Iterator<Item = String>) {
    let pack = PathBuf::from(args.next().unwrap());
    let publish_every = args.next().map_or(8, |count| count.parse().unwrap());
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
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
    // Nothing is let go for idleness or the budget while it replays.
    layer.set_engine_budget(u64::MAX);
    let family = FamilyId::from_digest(Digest128::of(b"engine keep probe"));
    let checkout = Checkout::bind(layer, &scratch.path().join("store"), family, &root).unwrap();
    let started = Instant::now();
    let report = checkout.build().unwrap();
    println!(
        "{}",
        serde_json::json!({
            "build_s": started.elapsed().as_secs_f64(),
            "files": report.files,
            "parsed": report.parsed_files,
        })
    );
    let edits = lines
        .map(|line| serde_json::from_str::<serde_json::Value>(&line.unwrap()).unwrap())
        .filter(|value| value["type"] == "edit")
        .collect::<Vec<_>>();
    let (mut refresh_wall, mut refresh_cpu) = (0.0f64, 0.0f64);
    let mut bytes = Vec::new();
    let mut walks = Vec::new();
    let mut publications = 0usize;
    let mut since = 0usize;
    for (index, edit) in edits.iter().enumerate() {
        let path = edit["path"].as_str().unwrap();
        write(path, edit["text"].as_str().unwrap_or_default());
        checkout.record_paths(&[path.as_bytes().to_vec()]).unwrap();
        let cpu = cpu_seconds();
        let wall = Instant::now();
        checkout.refresh_callgraph().unwrap();
        refresh_wall += wall.elapsed().as_secs_f64();
        refresh_cpu += cpu_seconds() - cpu;
        let walk = Instant::now();
        if let Some(held) = checkout.engine_census_bytes() {
            walks.push(walk.elapsed().as_secs_f64() * 1000.0);
            bytes.push(held as f64 / f64::from(1 << 20));
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
        if index % 50 == 49 {
            println!(
                "{}",
                serde_json::json!({
                    "edits": index + 1,
                    "engine_starts": checkout.engine_starts(),
                    "engine_mib": bytes.last(),
                    "walk_ms": walks.last(),
                })
            );
        }
    }
    println!(
        "{}",
        serde_json::json!({
            "edits": edits.len(),
            "publications": publications,
            "engine_starts": checkout.engine_starts(),
            "refresh_wall_s": refresh_wall,
            "refresh_cpu_s": refresh_cpu,
            "engine_mib_p50": percentile(&bytes, 0.5),
            "engine_mib_p90": percentile(&bytes, 0.9),
            "engine_mib_max": percentile(&bytes, 1.0),
            "walk_ms_p50": percentile(&walks, 0.5),
            "walk_ms_p90": percentile(&walks, 0.9),
            "walk_ms_max": percentile(&walks, 1.0),
        })
    );
}

fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kib| kib.parse::<u64>().ok())
        .unwrap_or_default()
        * 1024
}

struct RssPeak {
    bytes: AtomicU64,
    stop: AtomicBool,
}

impl RssPeak {
    fn sample() -> Arc<Self> {
        let peak = Arc::new(Self {
            bytes: AtomicU64::new(rss_bytes()),
            stop: AtomicBool::new(false),
        });
        let sampler = Arc::clone(&peak);
        std::thread::spawn(move || {
            while !sampler.stop.load(Ordering::Acquire) {
                sampler.bytes.fetch_max(rss_bytes(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        peak
    }

    fn take(&self) -> u64 {
        self.bytes
            .swap(rss_bytes(), Ordering::Relaxed)
            .max(rss_bytes())
    }
}

fn components(checkout: &Checkout) -> serde_json::Value {
    serde_json::json!({
        "census_bytes": checkout.engine_census_bytes(),
        "budget_bytes": checkout.engine_budget_bytes(),
        "parts_bytes": checkout.engine_memory_parts().map(|parts| {
            parts.into_iter().collect::<std::collections::BTreeMap<_, _>>()
        }),
        "callgraph_parts_bytes": checkout.callgraph_memory_parts().map(|parts| {
            parts.into_iter().collect::<std::collections::BTreeMap<_, _>>()
        }),
        "cache_note": "parts attribute shared allocations to the first component reaching them; census excludes publication",
    })
}

fn roots(mut args: impl Iterator<Item = String>) {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();
    let budget_mib = args.next().unwrap().parse::<u64>().unwrap();
    let rounds = args.next().unwrap().parse::<usize>().unwrap();
    let idle_seconds = args.next().unwrap().parse::<u64>().unwrap();
    let paths = args.collect::<Vec<_>>();
    assert!(!paths.is_empty() && paths.len() % 2 == 0);
    let scratch = tempfile::tempdir().unwrap();
    let layer = Arc::new(DataLayer::new(256 << 20, 8));
    let compare = std::env::var_os("PROBE_COMPARE").is_some();
    layer.set_engine_budget(if compare {
        8192 << 20
    } else {
        budget_mib << 20
    });
    let peak = RssPeak::sample();
    let mut checkouts = Vec::new();
    for (index, pair) in paths.chunks_exact(2).enumerate() {
        let root = std::fs::canonicalize(&pair[0]).unwrap();
        // Source archives lack .git; the scanner still needs a repository boundary.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let family = FamilyId::from_digest(Digest128::of(format!("kept root {index}").as_bytes()));
        let store = scratch.path().join(format!("store-{index}"));
        std::fs::create_dir_all(&store).unwrap();
        let checkout = Checkout::bind(Arc::clone(&layer), &store, family, &root).unwrap();
        let cpu = cpu_seconds();
        let wall = Instant::now();
        peak.take();
        let report = checkout.build().unwrap();
        let build_wall = wall.elapsed().as_secs_f64();
        let build_cpu = cpu_seconds() - cpu;
        println!(
            "{}",
            serde_json::json!({
                "event": "build", "root": index, "files": report.files,
                "parsed": report.parsed_files, "wall_s": build_wall, "cpu_s": build_cpu,
                "peak_rss_bytes": peak.take(), "rss_bytes": rss_bytes(),
            })
        );
        let text = std::fs::read_to_string(root.join(&pair[1])).unwrap();
        checkouts.push((checkout, root, pair[1].clone(), text));
        aft::memory::relieve_allocator_pressure();
    }
    let warm = |checkouts: &mut Vec<(Checkout, PathBuf, String, String)>, reverse: bool| {
        if let Ok(diff) = std::env::var("PROBE_WARM_DIFF") {
            let paths = std::fs::read_to_string(std::env::var("PROBE_WARM_PATHS").unwrap())
                .unwrap()
                .lines()
                .map(|path| path.as_bytes().to_vec())
                .collect::<Vec<_>>();
            for (index, (checkout, root, path, text)) in checkouts.iter_mut().enumerate() {
                if !root.join("crates/aft/src/lib.rs").exists() {
                    continue;
                }
                // A real boundary prevents git apply from finding the enclosing checkout.
                assert!(std::process::Command::new("git")
                    .args(["init", "--quiet"])
                    .current_dir(&*root)
                    .status()
                    .unwrap()
                    .success());
                assert!(std::process::Command::new("git")
                    .arg("apply")
                    .args(if reverse { vec!["--reverse"] } else { vec![] })
                    .arg(&diff)
                    .current_dir(&*root)
                    .status()
                    .unwrap()
                    .success());
                checkout.record_paths(&paths).unwrap();
                let changed_paths = checkout.changed_paths();
                assert!(
                    changed_paths > 50,
                    "the refactor must change many indexed paths"
                );
                let cpu = cpu_seconds();
                let wall = Instant::now();
                let rss_before = rss_bytes();
                peak.take();
                assert!(checkout.refresh_callgraph().unwrap().is_some());
                let wall = wall.elapsed().as_secs_f64();
                let cpu = cpu_seconds() - cpu;
                let peak_rss = peak.take();
                let before = components(checkout);
                let publish_cpu = cpu_seconds();
                let publish_wall = Instant::now();
                assert!(checkout.publish_changes().unwrap().is_some());
                let publish_wall_s = publish_wall.elapsed().as_secs_f64();
                let publish_cpu_s = cpu_seconds() - publish_cpu;
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "semantic_warmup", "reverse": reverse, "root": index, "paths": paths.len(), "changed_paths": changed_paths,
                        "cpu_s": cpu, "wall_s": wall, "rss_before_bytes": rss_before,
                        "peak_rss_bytes": peak_rss, "before_publish": before,
                        "publish_wall_s": publish_wall_s, "publish_cpu_s": publish_cpu_s,
                        "after_publish": components(checkout),
                    })
                );
                *text = std::fs::read_to_string(root.join(path)).unwrap();
                aft::memory::relieve_allocator_pressure();
            }
        }
    };
    warm(&mut checkouts, false);
    let budgets = if compare {
        vec![8192, budget_mib]
    } else {
        vec![budget_mib]
    };
    for (arm, budget_mib) in budgets.into_iter().enumerate() {
        if arm > 0 {
            layer.set_engine_budget(8192 << 20);
            warm(&mut checkouts, true);
        }
        if compare {
            println!(
                "{}",
                serde_json::json!({
                    "event": "warm_before_idle", "budget_mib": budget_mib,
                    "engines": checkouts.iter().map(|(c, ..)| components(c)).collect::<Vec<_>>(),
                    "rss_bytes": rss_bytes(), "cache_bytes": layer.cache_stats().resident_bytes,
                })
            );
            std::thread::sleep(Duration::from_secs(idle_seconds));
            println!(
                "{}",
                serde_json::json!({
                    "event": "warm_after_idle", "budget_mib": budget_mib, "seconds": idle_seconds,
                    "engines": checkouts.iter().map(|(c, ..)| components(c)).collect::<Vec<_>>(),
                    "rss_bytes": rss_bytes(), "cache_bytes": layer.cache_stats().resident_bytes,
                })
            );
        }
        layer.set_engine_budget(budget_mib << 20);
        let starts_before = checkouts
            .iter()
            .map(|(c, ..)| c.engine_starts())
            .sum::<u64>();
        let mut releases = 0;
        let mut restarts = 0;
        let (mut refresh_wall, mut refresh_cpu) = (0.0, 0.0);
        let (mut restart_wall, mut restart_cpu) = (0.0, 0.0);
        for round in 0..rounds {
            for (index, (checkout, root, path, text)) in checkouts.iter().enumerate() {
                let was_started = checkout.engine_starts() != 0;
                let missing = checkout.engine_census_bytes().is_none();
                std::fs::write(
                    root.join(path),
                    format!("{text}\n// kept-engine probe round {round}\n"),
                )
                .unwrap();
                checkout.record_paths(&[path.as_bytes().to_vec()]).unwrap();
                let held_before = checkouts
                    .iter()
                    .map(|(c, ..)| c.engine_census_bytes().is_some())
                    .collect::<Vec<_>>();
                let starts = checkout.engine_starts();
                let cpu = cpu_seconds();
                let wall = Instant::now();
                let rss_before = rss_bytes();
                peak.take();
                let refreshed = checkout.refresh_callgraph().unwrap();
                assert!(refreshed.is_some(), "a recorded edit must refresh");
                let wall = wall.elapsed().as_secs_f64();
                let cpu = cpu_seconds() - cpu;
                let peak_rss = peak.take();
                let started = checkout.engine_starts() - starts;
                let restarted = was_started && started > 0;
                refresh_wall += wall;
                refresh_cpu += cpu;
                if restarted {
                    restarts += 1;
                    restart_wall += wall;
                    restart_cpu += cpu;
                }
                let before_publish = components(checkout);
                let publication = Instant::now();
                let publish_cpu = cpu_seconds();
                let published = checkout.publish_changes().unwrap().is_some();
                let publish_wall = publication.elapsed().as_secs_f64();
                let publish_cpu = cpu_seconds() - publish_cpu;
                assert!(published);
                let released = checkouts
                    .iter()
                    .zip(&held_before)
                    .enumerate()
                    .filter_map(|(i, ((c, ..), held))| {
                        (*held && c.engine_census_bytes().is_none()).then_some(i)
                    })
                    .collect::<Vec<_>>();
                releases += released.len();
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "refresh_publish", "budget_mib": budget_mib,
                        "round": round, "root": index, "started": started, "restarted": restarted,
                        "missing_before": missing, "released_roots": released,
                        "wall_s": wall, "cpu_s": cpu, "publish_wall_s": publish_wall, "publish_cpu_s": publish_cpu,
                        "rss_bytes": rss_bytes(), "rss_before_bytes": rss_before, "peak_rss_bytes": peak_rss,
                        "before_publish": before_publish, "after_publish": components(checkout),
                    })
                );
                aft::memory::relieve_allocator_pressure();
            }
        }
        println!(
            "{}",
            serde_json::json!({
                "event": "before_idle", "budget_mib": budget_mib,
                "engines": checkouts.iter().map(|(c, ..)| components(c)).collect::<Vec<_>>(),
                "rss_bytes": rss_bytes(), "cache_bytes": layer.cache_stats().resident_bytes,
            })
        );
        std::thread::sleep(Duration::from_secs(idle_seconds));
        let mut seen = aft_store::heap::Seen::new();
        let unique_engine_bytes = checkouts
            .iter()
            .map(|(c, ..)| match c.memory(&mut seen).engine {
                aft::data_layer::checkout::EngineMemory::Running {
                    state_bytes, bytes, ..
                } => state_bytes + bytes,
                _ => 0,
            })
            .sum::<u64>();
        println!(
            "{}",
            serde_json::json!({
                "event": "after_idle", "budget_mib": budget_mib, "seconds": idle_seconds,
                "engines": checkouts.iter().map(|(c, ..)| components(c)).collect::<Vec<_>>(),
                "rss_bytes": rss_bytes(), "cache_bytes": layer.cache_stats().resident_bytes,
                "unique_engine_bytes": unique_engine_bytes,
                "sum_engine_census_bytes": checkouts.iter().filter_map(|(c, ..)| c.engine_census_bytes()).sum::<u64>(),
            })
        );
        println!(
            "{}",
            serde_json::json!({
                "event": "summary", "budget_mib": budget_mib, "roots": checkouts.len(), "rounds": rounds,
                "releases": releases, "restarts": restarts,
                "starts": checkouts.iter().map(|(c, ..)| c.engine_starts()).sum::<u64>() - starts_before,
                "refresh_wall_s": refresh_wall, "refresh_cpu_s": refresh_cpu,
                "restart_wall_s": restart_wall, "restart_cpu_s": restart_cpu,
            })
        );
    }
    peak.stop.store(true, Ordering::Release);
}

/// A line-driven probe lets a coordinator alternate two persistent binaries
/// after each edit rather than comparing long runs at different host loads.
fn paired(mut args: impl Iterator<Item = String>) {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .target(env_logger::Target::Stderr)
        .init();
    let root = std::fs::canonicalize(args.next().unwrap()).unwrap();
    let path = args.next().unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let original = std::fs::read_to_string(root.join(&path)).unwrap();
    let mut current = None;
    let mut store = None;
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let mut words = line.split_whitespace();
        let command = words.next().unwrap();
        let sequence = words.next().unwrap().parse::<usize>().unwrap();
        match command {
            "cold" => {
                drop(current.take());
                drop(store.take());
                aft::memory::relieve_allocator_pressure();
                std::fs::write(root.join(&path), &original).unwrap();
                store = Some(tempfile::tempdir().unwrap());
                let layer = Arc::new(DataLayer::new(256 << 20, 8));
                layer.set_engine_budget(u64::MAX);
                let family = FamilyId::from_digest(Digest128::of(b"paired probe"));
                let checkout =
                    Checkout::bind(layer, store.as_ref().unwrap().path(), family, &root).unwrap();
                let cpu = cpu_seconds();
                let wall = Instant::now();
                let report = checkout.build().unwrap();
                let wall = wall.elapsed().as_secs_f64();
                let cpu = cpu_seconds() - cpu;
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "paired_cold", "sequence": sequence,
                        "cpu_s": cpu, "wall_s": wall, "files": report.files, "parsed": report.parsed_files,
                    })
                );
                current = Some(checkout);
                aft::memory::relieve_allocator_pressure();
            }
            "edit" => {
                let checkout = current.as_ref().expect("build first");
                std::fs::write(
                    root.join(&path),
                    format!("{original}\n// paired probe edit {sequence}\n"),
                )
                .unwrap();
                let cpu = cpu_seconds();
                let wall = Instant::now();
                checkout.record_paths(&[path.as_bytes().to_vec()]).unwrap();
                let record_wall = wall.elapsed().as_secs_f64();
                let record_cpu = cpu_seconds() - cpu;
                let cpu = cpu_seconds();
                let wall = Instant::now();
                assert!(checkout.refresh_callgraph().unwrap().is_some());
                let refresh_wall = wall.elapsed().as_secs_f64();
                let refresh_cpu = cpu_seconds() - cpu;
                let cpu = cpu_seconds();
                let wall = Instant::now();
                assert!(checkout.publish_changes().unwrap().is_some());
                let publish_wall = wall.elapsed().as_secs_f64();
                let publish_cpu = cpu_seconds() - cpu;
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "paired_edit", "sequence": sequence,
                        "record_cpu_s": record_cpu, "record_wall_s": record_wall,
                        "refresh_cpu_s": refresh_cpu, "refresh_wall_s": refresh_wall,
                        "publish_cpu_s": publish_cpu, "publish_wall_s": publish_wall,
                    })
                );
                aft::memory::relieve_allocator_pressure();
            }
            "census" => {
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "paired_census", "sequence": sequence,
                        "engine": components(current.as_ref().expect("build first")),
                    })
                );
            }
            _ => panic!("unknown paired command {command}"),
        }
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("replay") => replay(args),
        Some("roots") => roots(args),
        Some("paired") => paired(args),
        _ => panic!("usage: engine_keep_probe replay <pack.jsonl.gz> [publish_every] [threads]; roots <budget_mib> <rounds> <idle_seconds> <root> <edit_path> ..."),
    }
}
