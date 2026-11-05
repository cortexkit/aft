//! Replays real edits through a [`Checkout`] on real files, the way a
//! running AFT drives it, and checks every answer against a cold build of
//! the same files.
//!
//! The edit pack (see [`super::edit_gate`]) is written to a folder on disk.
//! The checkout binds and builds it, then each edit is written to disk,
//! recorded with [`Checkout::record_paths`] and resolved with
//! [`Checkout::refresh_callgraph`]. At the end of every commit the view is
//! compared with a cold build of the same folder in a store of its own: every
//! call-graph primitive the commands use, and text candidates for a sample of
//! identifiers. Halfway through, the checkout rebuilds (so old segments are
//! retired), and later it is dropped and bound again from the store, as a
//! restarted process would, before that commit's edits are recorded. Runs
//! from `examples/checkout_gate.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use aft_store::ids::{Digest128, FamilyId};
use aft_store::text::{plan_pattern, Scope};

use super::checkout::{Checkout, CheckoutView, DataLayer, PlaneStatus};
use super::edit_gate::Files;

/// One commit end's measurement and check.
#[derive(Debug, Default, serde::Serialize)]
pub struct CommitRecord {
    pub commit: usize,
    pub edits: usize,
    /// What happened before this commit's edits: `rebuild` or `restart`.
    pub event: Option<String>,
    pub record_ms_p50: f64,
    pub record_ms_max: f64,
    pub refresh_ms_p50: f64,
    pub refresh_ms_max: f64,
    pub cold_build_ms: f64,
    /// Call-graph primitives whose answers differ from the cold build's.
    pub differing_primitives: usize,
    /// Sampled identifiers for which one side's candidates miss a file
    /// that really contains the identifier: a wrong answer.
    pub missed_text: usize,
    /// Sampled identifiers whose candidate sets differ only in files that
    /// do not contain the identifier. Candidates are a superset the reader
    /// verifies against file contents, so this is not a wrong answer; it
    /// shows how far the two filters differ.
    pub differing_false_positives: usize,
    pub text_samples: usize,
    /// The view's status was not ready after the last refresh.
    pub not_ready: bool,
    /// Segment files on disk in the checkout's store after the commit.
    pub segment_files: usize,
    pub examples: Vec<String>,
}

/// A folder under the system's temporary folder, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Result<Self, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "aft-checkout-gate-{}-{nanos}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).map_err(|error| error.to_string())?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn family() -> FamilyId {
    FamilyId::from_digest(Digest128::of(b"checkout gate"))
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let index = ((values.len() - 1) as f64 * p).round() as usize;
    values[index]
}

fn write_file(root: &Path, path: &str, text: &str) -> Result<(), String> {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(&full, text).map_err(|error| error.to_string())
}

/// The checkout in `slot`, which is empty only while it is down.
fn live(slot: &Option<Checkout>) -> Result<&Checkout, String> {
    slot.as_ref()
        .ok_or_else(|| "the checkout is down".to_string())
}

/// Whether a pack path is safe to write under the gate's folder.
fn safe(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.split('/').any(|part| part == ".." || part == ".git")
}

/// Identifiers to search for: every 50th symbol name of the parsed files,
/// at least four characters long, so each plans to at least one trigram.
fn text_samples(files: &Files) -> Vec<String> {
    let inputs = files.inputs();
    let mut names = BTreeSet::new();
    for (_, parse) in inputs.parses.values() {
        for symbol in &parse.symbols {
            if symbol.name.len() >= 4 && symbol.name.is_ascii() {
                names.insert(symbol.name.clone());
            }
        }
    }
    names.into_iter().step_by(50).collect()
}

/// Compares `view` with `cold` and fills the record's differences.
fn compare(
    root: &Path,
    view: &CheckoutView,
    cold: &CheckoutView,
    files: &Files,
    record: &mut CommitRecord,
) {
    let inputs = files.inputs();
    let mut queries = BTreeMap::new();
    let mut found = Vec::new();
    super::gate::compare_readers(
        &view.callgraph(),
        &cold.callgraph(),
        &inputs,
        &mut queries,
        &mut found,
    );
    record.differing_primitives = queries
        .values()
        .filter(|(_, differing, _, _)| *differing > 0)
        .count();
    record.examples.extend(found.into_iter().take(10));
    for failures in [view.callgraph_failures(), cold.callgraph_failures()] {
        record.differing_primitives += failures.len();
        record.examples.extend(failures.into_iter().take(5));
    }
    let samples = text_samples(files);
    record.text_samples = samples.len();
    for name in samples {
        let Ok(query) = plan_pattern(&regex::escape(&name), false) else {
            continue;
        };
        let (ours, theirs) = match (
            view.text_candidates(&query, Scope::All),
            cold.text_candidates(&query, Scope::All),
        ) {
            (Ok(ours), Ok(theirs)) => (
                ours.into_iter().collect::<BTreeSet<_>>(),
                theirs.into_iter().collect::<BTreeSet<_>>(),
            ),
            (ours, theirs) => {
                record.missed_text += 1;
                record.examples.push(format!(
                    "text {name}: {:?} vs {:?}",
                    ours.err(),
                    theirs.err()
                ));
                continue;
            }
        };
        if ours == theirs {
            continue;
        }
        // A file only one side lists is a miss on the other side when it
        // really contains the name.
        let contains = |path: &&Vec<u8>| {
            std::str::from_utf8(path)
                .ok()
                .and_then(|path| std::fs::read(root.join(path)).ok())
                .is_some_and(|bytes| {
                    bytes
                        .windows(name.len())
                        .any(|window| window == name.as_bytes())
                })
        };
        let missed = ours
            .symmetric_difference(&theirs)
            .filter(contains)
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect::<Vec<_>>();
        if missed.is_empty() {
            record.differing_false_positives += 1;
        } else {
            record.missed_text += 1;
            if record.examples.len() < 20 {
                record
                    .examples
                    .push(format!("text {name}: one side misses {missed:?}"));
            }
        }
    }
    record.not_ready = view.status().callgraph != PlaneStatus::Ready;
}

/// Replays the pack at `pack` through a checkout under a temporary folder,
/// with `threads` resolver threads. Writes one JSON line per commit to
/// `out`; returns the commits checked and how many differed.
///
/// # Errors
///
/// When the pack cannot be read, or the checkout fails.
pub fn run(
    pack: &Path,
    threads: usize,
    out: &mut dyn std::io::Write,
) -> Result<(usize, usize), String> {
    let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(
        std::fs::File::open(pack).map_err(|error| error.to_string())?,
    ));
    let mut lines = reader.lines();
    let first: serde_json::Value = serde_json::from_str(
        &lines
            .next()
            .ok_or("empty pack")?
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let mut edits = Vec::new();
    for line in lines {
        let line = line.map_err(|error| error.to_string())?;
        let value: serde_json::Value =
            serde_json::from_str(&line).map_err(|error| error.to_string())?;
        if value["type"] == "edit" {
            edits.push(value);
        }
    }

    let folder = Scratch::new("files")?;
    let root: PathBuf = std::fs::canonicalize(folder.path()).map_err(|error| error.to_string())?;
    // A repository of its own, so ignore rules of folders above the
    // temporary root (on a build server, the job's checkout) do not apply.
    std::fs::create_dir(root.join(".git")).map_err(|error| error.to_string())?;
    let mut files = Files::default();
    for (path, text) in first["files"].as_object().ok_or("no base files")? {
        if !safe(path) {
            continue;
        }
        let text = text.as_str().unwrap_or_default();
        write_file(&root, path, text)?;
        files.set(path, text)?;
    }
    let store = Scratch::new("store")?;
    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let bind = || {
        Checkout::bind(Arc::clone(&layer), store.path(), family(), &root)
            .map_err(|error| error.to_string())
    };
    // `None` while the checkout is down for the restart.
    let mut slot = Some(bind()?);
    live(&slot)?.build().map_err(|error| error.to_string())?;

    let commits = edits
        .iter()
        .filter_map(|edit| edit["commit"].as_u64())
        .collect::<BTreeSet<_>>();
    let rebuild_at = commits.iter().nth(commits.len() / 3).copied();
    let restart_at = commits.iter().nth(2 * commits.len() / 3).copied();

    let mut checked = 0;
    let mut failed = 0;
    let mut index = 0;
    while index < edits.len() {
        let commit = edits[index]["commit"].as_u64().unwrap_or_default();
        let mut record = CommitRecord {
            commit: usize::try_from(commit).unwrap_or_default(),
            ..CommitRecord::default()
        };
        if Some(commit) == rebuild_at {
            live(&slot)?.build().map_err(|error| error.to_string())?;
            record.event = Some("rebuild".to_string());
        }
        let restart = Some(commit) == restart_at;
        let mut changed_while_down = Vec::new();
        if restart {
            // A restarted process: the old handle goes away, the commit's
            // edits land while nothing watches, then the checkout binds
            // again and must find them on its own.
            slot = None;
            record.event = Some("restart".to_string());
        }
        let mut record_ms = Vec::new();
        let mut refresh_ms = Vec::new();
        while index < edits.len() && edits[index]["commit"].as_u64().unwrap_or_default() == commit {
            let edit = &edits[index];
            index += 1;
            let path = edit["path"]
                .as_str()
                .ok_or("edit without path")?
                .to_string();
            if !safe(&path) {
                continue;
            }
            if edit["kind"] == "delete" {
                let _ = std::fs::remove_file(root.join(&path));
                files.remove(&path);
            } else {
                let text = edit["text"].as_str().unwrap_or_default();
                write_file(&root, &path, text)?;
                files.set(&path, text)?;
            }
            record.edits += 1;
            if restart {
                changed_while_down.push(path);
                continue;
            }
            let checkout = live(&slot)?;
            let started = Instant::now();
            checkout
                .record_paths(&[path.into_bytes()])
                .map_err(|error| error.to_string())?;
            record_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            let started = Instant::now();
            checkout
                .refresh_callgraph()
                .map_err(|error| error.to_string())?;
            refresh_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        if restart {
            slot = Some(bind()?);
            let started = Instant::now();
            live(&slot)?
                .refresh_callgraph()
                .map_err(|error| error.to_string())?;
            refresh_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            record.examples.push(format!(
                "{} paths changed while down",
                changed_while_down.len()
            ));
        }
        record.record_ms_p50 = percentile(&mut record_ms, 0.5);
        record.record_ms_max = record_ms.iter().copied().fold(0.0, f64::max);
        record.refresh_ms_p50 = percentile(&mut refresh_ms, 0.5);
        record.refresh_ms_max = refresh_ms.iter().copied().fold(0.0, f64::max);

        let cold_store = Scratch::new(&format!("cold-{commit}"))?;
        let started = Instant::now();
        let cold = Checkout::bind(Arc::clone(&layer), cold_store.path(), family(), &root)
            .map_err(|error| error.to_string())?;
        cold.build().map_err(|error| error.to_string())?;
        record.cold_build_ms = started.elapsed().as_secs_f64() * 1000.0;
        let view = live(&slot)?.view().ok_or("the checkout has no view")?;
        let cold_view = cold.view().ok_or("the cold checkout has no view")?;
        compare(&root, &view, &cold_view, &files, &mut record);
        drop((view, cold_view, cold, cold_store));
        record.segment_files = std::fs::read_dir(store.path().join("segments"))
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or_default();
        checked += 1;
        if record.differing_primitives > 0 || record.missed_text > 0 || record.not_ready {
            failed += 1;
        }
        writeln!(
            out,
            "{}",
            serde_json::to_string(&record).map_err(|error| error.to_string())?
        )
        .map_err(|error| error.to_string())?;
    }
    if checked == 0 {
        return Err("the pack has no edits, so nothing was checked".to_string());
    }
    Ok((checked, failed))
}
