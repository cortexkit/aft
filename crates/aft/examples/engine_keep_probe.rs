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

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::io::BufRead as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

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

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("replay") => replay(args),
        _ => panic!("usage: engine_keep_probe replay <pack.jsonl.gz> [publish_every] [threads]"),
    }
}
