//! Measures a new worktree's first build: the wall time and the CPU time
//! the process spent, for three checkouts of one repository built in turn
//! into one index store, the one store every checkout of a repository
//! shares, so each later build can reuse what earlier ones stored. The
//! first has nothing to start from; the second is
//! at the same commit; the third is the same commit with `files` of the
//! files that differ in `other` taken from it. Each line of stdout is one
//! build, as JSON, with the build's report.
//!
//! ```text
//! cargo run --profile ci-storm -p agent-file-tools --example worktree_start_probe -- \
//!     <repo> <rev> <other-rev> <files> [threads]
//! ```
//!
//! The checkouts are exported with `git archive` into a temporary folder
//! (under `TMPDIR`), so the repository itself is not touched.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::print_stderr)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

use aft::data_layer::checkout::{Checkout, DataLayer};
use aft_store::ids::{Digest128, FamilyId};

/// Writes the tree of `rev` in `repo` into `dir`.
fn export(repo: &Path, rev: &str, dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let mut archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", rev])
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

/// Takes into `dir` (holding `rev`) the first `files` paths that differ
/// between `rev` and `other`, as `other` has them. Returns them.
fn take_from(repo: &Path, rev: &str, other: &str, files: usize, dir: &Path) -> Vec<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--name-only", "--no-renames", rev, other])
        .output()
        .unwrap();
    let paths = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .take(files)
        .map(str::to_string)
        .collect::<Vec<_>>();
    for path in &paths {
        let shown = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["show", &format!("{other}:{path}")])
            .output()
            .unwrap();
        let target = dir.join(path);
        if shown.status.success() {
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, shown.stdout).unwrap();
        } else {
            let _ = std::fs::remove_file(&target);
        }
    }
    paths
}

/// User and system CPU seconds this process has spent, its threads included.
/// Windows has no getrusage; the probe reports 0 there and wall time only.
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

fn build(label: &str, layer: &Arc<DataLayer>, store: &Path, family: FamilyId, root: &Path) {
    let cpu = cpu_seconds();
    let started = Instant::now();
    let checkout = Checkout::bind(Arc::clone(layer), store, family, root).unwrap();
    let report = checkout.build().unwrap();
    let wall = started.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu;
    println!(
        "{}",
        serde_json::json!({
            "build": label,
            "wall_s": (wall * 100.0).round() / 100.0,
            "cpu_s": (cpu * 100.0).round() / 100.0,
            "report": format!("{report:?}"),
        })
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let repo = std::path::PathBuf::from(args.next().unwrap());
    let rev = args.next().unwrap();
    let other = args.next().unwrap();
    let files = args.next().unwrap().parse::<usize>().unwrap();
    let threads = args.next().map_or(8, |threads| threads.parse().unwrap());
    let scratch = tempfile::tempdir().unwrap();
    let store = scratch.path().join("store");
    let [first, same, near] = ["first", "same", "near"].map(|name| scratch.path().join(name));
    export(&repo, &rev, &first);
    export(&repo, &rev, &same);
    export(&repo, &rev, &near);
    let taken = take_from(&repo, &rev, &other, files, &near);
    eprintln!(
        "the third checkout takes {} files from {other}",
        taken.len()
    );
    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let family = FamilyId::from_digest(Digest128::of(b"worktree start probe"));
    build("nothing to start from", &layer, &store, family, &first);
    build("same commit", &layer, &store, family, &same);
    build(
        &format!("{} files away", taken.len()),
        &layer,
        &store,
        family,
        &near,
    );
}
