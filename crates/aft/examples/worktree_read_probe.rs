//! Measures what a new Git worktree's first build reads: the bytes, files,
//! wall time and CPU time of each pass that build makes over the new
//! worktree's files. A checkout with nothing published whose repository
//! already has another checkout built starts from that sibling's
//! publication instead of building cold: it scans its own files to learn
//! which differ, publishes the sibling's state for itself, and serves it
//! through `Checkout::bind`'s comparison of the published state with the
//! files on disk.
//!
//! The tree in `<tree.tar[.gz]>` is committed into a fresh repository (the
//! "main" checkout), which is built cold into an index store of its own;
//! then `git worktree add` makes a new worktree of that commit, as an agent
//! harness does, and the passes are measured on it:
//!
//! - `first`: a scan with no earlier state, which reads every admitted
//!   file, as the start from a sibling's first scan does.
//! - `second`: a scan over the first one's state, which reads every record
//!   whose stat is still racy (changed less than two seconds before it was
//!   observed), as the comparison that serves the sibling's state does.
//! - `floor`: the same scan with every record aged out of the racy window,
//!   so nothing is read: the cost of walking and stating alone.
//!
//! Each pass runs on a worktree made `delay` seconds earlier: 0 is a build
//! that starts as soon as the worktree exists, 3 one that starts after a
//! short preparation. Finally two more worktrees, at the same two ages, are
//! bound and built through a checkout, as the runtime does, for the whole
//! build's wall and CPU time. Each line of stdout is one measurement, as
//! JSON.
//!
//! ```text
//! cargo run --release -p agent-file-tools --example worktree_read_probe -- \
//!     <tree.tar[.gz]> [threads] [scratch-dir]
//! ```
//!
//! The scratch folder defaults to a temporary folder under `TMPDIR`; Git
//! runs with no global or system configuration.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aft::data_layer::checkout::{Checkout, DataLayer};
use aft_store::checkout::{CheckoutState, PathRecord, StatEvidence};
use aft_store::ids::{ContentKey, Digest128, FamilyId};
use aft_store::scan::{scan, ContentKind, ContentSink};
use aft_store::stat::RACY_WINDOW_NS;

struct Nothing;

impl ContentSink for Nothing {
    fn read(&self, _path: &[u8], _key: ContentKey, _kind: ContentKind, _bytes: &[u8]) {}
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

#[cfg(not(unix))]
fn cpu_seconds() -> f64 {
    0.0
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=probe",
            "-c",
            "user.email=probe@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every record of `state` aged out of the racy window, so a scan over it
/// reads only files whose stat changed.
fn aged(state: &CheckoutState) -> CheckoutState {
    let records = state
        .paths()
        .iter()
        .map(|record| PathRecord {
            stat: StatEvidence {
                observed_ns: record.stat.mtime_ns.max(record.stat.ctime_ns) + RACY_WINDOW_NS,
                ..record.stat
            },
            ..record.clone()
        })
        .collect();
    CheckoutState::new(state.git.clone(), state.conversion, records).unwrap()
}

fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Runs one scan pass on `root` and prints what it read and cost.
fn pass(label: &str, delay: u64, root: &Path, previous: Option<&CheckoutState>) -> CheckoutState {
    let policy = aft::data_layer::content::scan_policy();
    let cpu = cpu_seconds();
    let started = Instant::now();
    let scanned = scan(root, &policy, previous, &Nothing).unwrap();
    let wall = started.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu;
    println!(
        "{}",
        serde_json::json!({
            "pass": label,
            "worktree_age_s": delay,
            "files": scanned.counters.files,
            "files_read": scanned.counters.read,
            "bytes_read": scanned.counters.bytes_read,
            "reused": scanned.counters.reused,
            "wall_s": round(wall),
            "cpu_s": round(cpu),
        })
    );
    scanned.state
}

fn main() {
    let mut args = std::env::args().skip(1);
    let tree = std::path::PathBuf::from(args.next().expect("a tree archive"));
    let tree = std::fs::canonicalize(tree).unwrap();
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    let held = tempfile::tempdir().unwrap();
    let scratch = args
        .next()
        .map_or_else(|| held.path().to_path_buf(), std::path::PathBuf::from);
    std::fs::create_dir_all(&scratch).unwrap();
    let scratch = std::fs::canonicalize(scratch).unwrap();
    let main_root = scratch.join("main");
    std::fs::create_dir_all(&main_root).unwrap();
    let status = Command::new("tar")
        .arg("-xf")
        .arg(&tree)
        .arg("-C")
        .arg(&main_root)
        .status()
        .unwrap();
    assert!(status.success());
    git(&main_root, &["init", "-q"]);
    git(&main_root, &["add", "-A", "-f"]);
    git(&main_root, &["commit", "-q", "--no-verify", "-m", "tree"]);

    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let family = FamilyId::from_digest(Digest128::of(b"worktree read probe"));
    let store = scratch.join("store");
    let cpu = cpu_seconds();
    let started = Instant::now();
    let main = Checkout::bind(Arc::clone(&layer), &store, family, &main_root).unwrap();
    let report = main.build().unwrap();
    eprintln!(
        "main checkout built cold in {:.1} s wall, {:.1} s CPU: {} files, {} parsed",
        started.elapsed().as_secs_f64(),
        cpu_seconds() - cpu,
        report.files,
        report.parsed_files
    );

    let worktree = |name: &str| {
        let root = scratch.join(name);
        let started = Instant::now();
        git(
            &main_root,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                root.to_str().unwrap(),
                "HEAD",
            ],
        );
        eprintln!(
            "git worktree add {name}: {:.2} s",
            started.elapsed().as_secs_f64()
        );
        root
    };
    for (name, delay) in [("fresh", 0), ("prepared", 3)] {
        let root = worktree(name);
        std::thread::sleep(Duration::from_secs(delay));
        let first = pass("first", delay, &root, None);
        pass("second", delay, &root, Some(&first));
        pass("floor", delay, &root, Some(&aged(&first)));
    }

    for (name, delay) in [("built-fresh", 0), ("built-prepared", 3)] {
        let root = worktree(name);
        std::thread::sleep(Duration::from_secs(delay));
        let cpu = cpu_seconds();
        let started = Instant::now();
        let checkout = Checkout::bind(Arc::clone(&layer), &store, family, &root).unwrap();
        let report = checkout.build().unwrap();
        let wall = started.elapsed().as_secs_f64();
        let cpu = cpu_seconds() - cpu;
        println!(
            "{}",
            serde_json::json!({
                "pass": "whole build",
                "worktree_age_s": delay,
                "wall_s": round(wall),
                "cpu_s": round(cpu),
                "files": report.files,
                "parsed": report.parsed_files,
                "started_from_sibling": report.started_from_sibling,
                "scan_ms": round(report.scan_ms),
                "resolve_ms": round(report.resolve_ms),
                "publish_ms": round(report.publish_ms),
            })
        );
    }
}
