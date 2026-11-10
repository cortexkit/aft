//! The dead-code gate: dead code as `aft_inspect` computes it on one real
//! checkout, checked against independent computations of the same answer.
//!
//! The rollup reads each file's syntax facts from its content record and
//! the data layer's call graph one file at a time (see
//! `inspect::scanners::dead_code::engine`). This gate builds a data-layer
//! checkout of a directory and computes dead code that way, measuring each
//! phase. In `compare` mode it also parses every file's facts, checks them
//! against the stored ones file by file, and computes dead code a second
//! way, over a call-graph snapshot holding every call row
//! (`dead_code::snapshot`), comparing every field of the two aggregates.
//! It can then edit a few files, wait for the call graph to catch up, and
//! do it all again, so the edit overlay is covered too.
//!
//! With `DEAD_CODE_GATE_DUMP` set, each aggregate is written to that
//! directory, for comparing with another build's run of the same gate.
//!
//! Each phase reports its wall time and the process's peak resident memory
//! during it (Linux: the peak is reset before each phase), so running
//! `new` mode alone measures what `aft_inspect` spends.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::checkout::{CheckoutView, Planes};
use super::runtime::{DataCheckout, Fresh};
use crate::inspect::job::{
    CallgraphExport, CallgraphSnapshot, InspectCategory, InspectJob, JobKey, JobScope,
};
use crate::inspect::scanners::dead_code::section::{Probe, Use};
use crate::inspect::FileContribution;

/// What a gate run computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Dead code as `aft_inspect` computes it, alone.
    New,
    /// That, and the independent computation, compared.
    Compare,
}
/// What a gate run found.
#[derive(Debug, Default)]
pub struct Summary {
    /// Rounds checked against an independent answer.
    pub comparisons: usize,
    /// Rounds whose checks found a difference.
    pub differed: usize,
    /// Rounds whose rollup patched a kept graph.
    pub patched: usize,
}

/// The resident memory now and its peak since the last reset, in MiB, from
/// `/proc/self/status` (zero elsewhere).
fn memory_mib() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kib| kib.parse::<u64>().ok())
            .map_or(0, |kib| kib / 1024)
    };
    (field("VmRSS:"), field("VmHWM:"))
}

/// Resets the peak resident memory to the current value (Linux only).
fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// Returns freed allocator pages to the system, so a phase's peak is not
/// hidden by an earlier phase's freed memory.
fn trim_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only returns free pages to the system.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Runs `phase`, reporting its time and peak memory as one JSON line.
fn measured<T>(out: &mut dyn Write, label: &str, phase: impl FnOnce() -> T) -> T {
    trim_allocator();
    reset_peak();
    let (before, _) = memory_mib();
    let started = Instant::now();
    let value = phase();
    let elapsed = started.elapsed();
    let (after, peak) = memory_mib();
    let _ = writeln!(
        out,
        "{}",
        json!({
            "phase": label,
            "ms": elapsed.as_millis() as u64,
            "rss_before_mib": before,
            "rss_after_mib": after,
            "peak_rss_mib": peak,
            "peak_over_before_mib": peak.saturating_sub(before),
        })
    );
    value
}

fn job(root: &Path, storage: &Path) -> InspectJob {
    InspectJob {
        job_id: 1,
        key: JobKey::for_project_category(InspectCategory::DeadCode),
        category: InspectCategory::DeadCode,
        scope_files: Vec::new(),
        project_root: root.to_path_buf(),
        inspect_dir: storage.join("inspect"),
        config: Arc::new(crate::config::Config {
            project_root: Some(root.to_path_buf()),
            ..crate::config::Config::default()
        }),
        symbol_cache: Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new())),
        inspect_writer: true,
        callgraph_writer: true,
        callgraph_snapshot: None,
    }
}

fn fresh_view(checkout: &DataCheckout) -> Result<CheckoutView, String> {
    match checkout.fresh_view(Duration::from_secs(3600)) {
        Fresh::Ready(view) => Ok(view),
        Fresh::Building => Err("the call graph did not catch up in an hour".to_string()),
        Fresh::Failed(reason) => Err(format!("the call graph failed: {reason}")),
    }
}

/// The dead-code facts of `files`, scanned as `aft_inspect` scans them,
/// with the Oxc engine's facts for TypeScript and JavaScript, and
/// `exported_symbols` (the call graph's exports) for the other languages.
fn scan(
    job: &InspectJob,
    entry_points: Vec<PathBuf>,
    files: &[PathBuf],
    exported_symbols: &[CallgraphExport],
) -> Vec<FileContribution> {
    let mut job = job.clone();
    job.scope_files = files.to_vec();
    let entries = crate::inspect::resolve_entry_points(&job.project_root);
    let options = crate::inspect::oxc_engine::AnalyzeOptions {
        entry_points,
        public_api_files: entries.public_api_files(),
        executable_root_exports: entries.executable_root_exports(),
        force_reparse_files: Vec::new(),
        entry_reachability: true,
    };
    let mut cache = crate::inspect::oxc_engine::OxcFactsCache::new();
    let oxc = crate::inspect::oxc_engine::analyze_files_with_cache(
        &job.project_root,
        files,
        options,
        &mut cache,
    )
    .ok();
    crate::inspect::scanners::dead_code::scan_dead_code_facts(&job, oxc.as_ref(), exported_symbols)
        .unwrap_or_default()
}

/// [`scan`], reading each file's syntax facts from its content record in
/// `snapshot`'s call graph instead of parsing it, as `aft_inspect` does,
/// with the number of files the call graph does not have, which are parsed.
fn scan_stored(
    job: &InspectJob,
    entry_points: Vec<PathBuf>,
    files: &[PathBuf],
    snapshot: &CallgraphSnapshot,
) -> Result<(Vec<FileContribution>, usize), String> {
    let mut job = job.clone();
    job.scope_files = files.to_vec();
    let entries = crate::inspect::resolve_entry_points(&job.project_root);
    let options = crate::inspect::oxc_engine::AnalyzeOptions {
        entry_points,
        public_api_files: entries.public_api_files(),
        executable_root_exports: entries.executable_root_exports(),
        force_reparse_files: Vec::new(),
        entry_reachability: true,
    };
    let mut cache = crate::inspect::oxc_engine::OxcFactsCache::new();
    let oxc = crate::inspect::oxc_engine::analyze_files_with_cache(
        &job.project_root,
        files,
        options,
        &mut cache,
    )
    .ok();
    let view = snapshot
        .data_layer
        .as_ref()
        .ok_or_else(|| "the snapshot carries no call graph".to_string())?;
    let graph_files = view.files();
    let lookup = |relative_file: &str| {
        crate::inspect::scanners::dead_code::engine::stored_facts(&graph_files, relative_file)
    };
    // A data-layer snapshot lists no exports; its view's files hold them.
    let exported_symbols = view.exported_symbols_of(&job.project_root, files)?;
    crate::inspect::scanners::dead_code::scan_dead_code_facts_stored(
        &job,
        oxc.as_ref(),
        &exported_symbols,
        &lookup,
    )?
    .ok_or_else(|| "the scan was cancelled".to_string())
}

/// Compares two scans' facts file by file, reporting the files whose
/// facts differ; whether none do.
fn compare_scans(
    out: &mut dyn Write,
    label: &str,
    parsed: &[FileContribution],
    stored: &[FileContribution],
) -> bool {
    let by_file = |contributions: &[FileContribution]| {
        contributions
            .iter()
            .map(|contribution| {
                (
                    contribution.file_path.clone(),
                    (
                        contribution.contribution.clone(),
                        contribution.type_ref_names.clone(),
                    ),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let (parsed, stored) = (by_file(parsed), by_file(stored));
    let differing = parsed
        .keys()
        .chain(stored.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|file| parsed.get(*file) != stored.get(*file))
        .collect::<Vec<_>>();
    let _ = writeln!(
        out,
        "{}",
        json!({
            "facts": label,
            "files": parsed.len(),
            "identical": differing.is_empty(),
            "differing_files": differing.len(),
            "first_differences": differing
                .iter()
                .take(5)
                .map(|file| json!({
                    "file": file,
                    "parsed": parsed.get(*file).map(|(facts, _)| facts),
                    "stored": stored.get(*file).map(|(facts, _)| facts),
                }))
                .collect::<Vec<_>>(),
        })
    );
    differing.is_empty()
}

/// The snapshot `aft_inspect` reads, and in `compare` mode the snapshot
/// holding every call row, whose exports must be the same.
fn snapshots(
    out: &mut dyn Write,
    label: &str,
    mode: Mode,
    view: &CheckoutView,
    storage: &Path,
) -> Result<(Arc<CallgraphSnapshot>, Option<CallgraphSnapshot>), String> {
    let snapshot = measured(out, &format!("{label}: snapshot"), || {
        view.dead_code_snapshot_in(Some(storage.join("dead-code")))
            .map_err(|error| format!("{error:?}"))
    })?;
    let rows = (mode == Mode::Compare)
        .then(|| {
            measured(out, &format!("{label}: snapshot of every call row"), || {
                view.rows_dead_code_snapshot()
                    .map_err(|error| format!("{error:?}"))
            })
        })
        .transpose()?;
    if let Some(rows) = &rows {
        // A data-layer snapshot lists no exports; its view's files hold them.
        let files = view.dead_code_files();
        let graph = super::dead_code_graph::DeadCodeGraph::open(view.root(), &files)?;
        let exported_symbols = graph.all_exported_symbols();
        let identical = format!("{:?}", rows.exported_symbols) == format!("{exported_symbols:?}")
            && rows.entry_points == snapshot.entry_points
            && rows.files == snapshot.files;
        let _ = writeln!(
            out,
            "{}",
            json!({"exports": exported_symbols.len(), "identical_exports": identical})
        );
        if !identical {
            return Err("the snapshots' files, exports or entry points differ".to_string());
        }
    }
    Ok((snapshot, rows))
}

/// The dead-code aggregate of `contributions` over `snapshot`'s every call
/// row, with its phases' times.
fn aggregate(
    root: &Path,
    snapshot: &CallgraphSnapshot,
    contributions: &[FileContribution],
    timings: &mut Vec<(&'static str, f64)>,
) -> Result<Value, String> {
    let public_api_files = crate::inspect::scanners::dead_code::collect_public_api_files(root);
    let roles = crate::inspect::resolve_project_roles(root);
    crate::inspect::scanners::dead_code::engine::aggregate_with_snapshot(
        root,
        snapshot,
        contributions,
        &public_api_files,
        &roles,
        timings,
    )
}

/// Dead code for `files` as `aft_inspect`'s rollup computes it over a
/// data-layer `snapshot`, with its phases' times: `use_` says whether kept
/// dead-code graphs are read, patched and written, and `probe` gathers what
/// the rollup did.
fn aggregate_from_view(
    job: &InspectJob,
    files: &[PathBuf],
    entry_points: &[PathBuf],
    snapshot: &CallgraphSnapshot,
    timings: &mut Vec<(&'static str, f64)>,
    use_: Use,
    probe: Option<&mut Probe>,
) -> Result<Value, String> {
    let mut job = job.clone();
    job.scope_files = files.to_vec();
    let started = Instant::now();
    let public_api_files =
        crate::inspect::scanners::dead_code::collect_public_api_files(&job.project_root);
    let roles = crate::inspect::resolve_project_roles(&job.project_root);
    timings.push((
        "public API and roles",
        started.elapsed().as_secs_f64() * 1000.0,
    ));
    let oxc = |files: &[PathBuf]| {
        let entries = crate::inspect::resolve_entry_points(&job.project_root);
        let options = crate::inspect::oxc_engine::AnalyzeOptions {
            entry_points: entry_points.to_vec(),
            public_api_files: entries.public_api_files(),
            executable_root_exports: entries.executable_root_exports(),
            force_reparse_files: Vec::new(),
            entry_reachability: true,
        };
        let mut cache = crate::inspect::oxc_engine::OxcFactsCache::new();
        Ok(crate::inspect::oxc_engine::analyze_files_with_cache(
            &job.project_root,
            files,
            options,
            &mut cache,
        )
        .ok())
    };
    crate::inspect::scanners::dead_code::section::aggregate_from_view(
        &job,
        snapshot,
        &oxc,
        &public_api_files,
        &roles,
        timings,
        use_,
        probe,
    )
}

/// Where two aggregates differ, field by field; for finding lists, the
/// findings only one side has, and the first position their orders differ.
fn differences(old: &Value, new: &Value) -> Vec<Value> {
    let mut found = Vec::new();
    let keys = old
        .as_object()
        .into_iter()
        .chain(new.as_object())
        .flat_map(|object| object.keys().cloned())
        .collect::<BTreeSet<_>>();
    for key in keys {
        let (left, right) = (&old[&key], &new[&key]);
        if left == right {
            continue;
        }
        match (left.as_array(), right.as_array()) {
            (Some(left), Some(right)) => {
                let as_set =
                    |items: &[Value]| items.iter().map(Value::to_string).collect::<BTreeSet<_>>();
                let (left_set, right_set) = (as_set(left), as_set(right));
                let first_order_difference = left
                    .iter()
                    .zip(right)
                    .position(|(left, right)| left != right);
                found.push(json!({
                    "field": key,
                    "old_len": left.len(),
                    "new_len": right.len(),
                    "only_old": left_set.difference(&right_set).take(50).collect::<Vec<_>>(),
                    "only_new": right_set.difference(&left_set).take(50).collect::<Vec<_>>(),
                    "first_order_difference": first_order_difference,
                }));
            }
            _ => found.push(json!({"field": key, "old": left, "new": right})),
        }
    }
    found
}

/// Counts of an aggregate, for the report.
fn counts(aggregate: &Value) -> Value {
    json!({
        "count": aggregate["count"],
        "total_count": aggregate["total_count"],
        "generated_count": aggregate["generated_count"],
        "test_only_count": aggregate["test_only_count"],
        "uncertain_count": aggregate["uncertain_count"],
        "excluded_test_count": aggregate["excluded_test_count"],
        "complete": aggregate["complete"],
        "skipped_files_count": aggregate["skipped_files_count"],
    })
}

/// One round: dead code as `aft_inspect` computes it, and checks of it. In
/// the first round the rollup works out every file and keeps its graph, and
/// a second rollup reads that graph back. After an edit the rollup patches
/// the kept graph; in `compare` mode every file is then also worked out from
/// nothing and the two compared: the aggregates, every whole-project index
/// and every file's block. With `rows` and `parsed`, dead code is also
/// computed over the snapshot of every call row from parsed facts.
#[allow(clippy::too_many_arguments)]
fn round(
    out: &mut dyn Write,
    label: &str,
    mode: Mode,
    root: &Path,
    job: &InspectJob,
    files: &[PathBuf],
    entry_points: &[PathBuf],
    snapshot: &CallgraphSnapshot,
    rows: Option<&CallgraphSnapshot>,
    parsed: Option<&[FileContribution]>,
    summary: &mut Summary,
) -> Result<(), String> {
    let phases = |timings: &[(&'static str, f64)]| json!({"phases": timings.iter().map(|(label, value)| json!([label, value])).collect::<Vec<_>>()});
    let mut timings = Vec::new();
    let mut probe = Probe {
        details: mode == Mode::Compare,
        force_patch: mode == Mode::Compare,
        ..Probe::default()
    };
    let new = measured(out, &format!("{label}: dead code"), || {
        aggregate_from_view(
            job,
            files,
            entry_points,
            snapshot,
            &mut timings,
            Use::Kept,
            Some(&mut probe),
        )
    })?;
    let _ = writeln!(out, "{}", phases(&timings));
    let _ = writeln!(
        out,
        "{}",
        json!({"rollup": probe.path, "changed files": probe.changed, "files materialized": probe.materialized, "kept bytes": probe.kept_bytes})
    );
    if probe.path == "patched" {
        summary.patched += 1;
    }
    let kept = if label == "first" {
        let mut timings = Vec::new();
        let again = measured(
            out,
            &format!("{label}: dead code from the kept graph"),
            || {
                aggregate_from_view(
                    job,
                    files,
                    entry_points,
                    snapshot,
                    &mut timings,
                    Use::Kept,
                    None,
                )
            },
        )?;
        let _ = writeln!(out, "{}", phases(&timings));
        Some(again)
    } else {
        None
    };
    let mut found = Vec::new();
    // After an edit the rollup must patch the kept graph: a rollup that
    // falls back to working out every file would hide a delta the patch
    // misses, since there would be nothing patched to compare.
    if mode == Mode::Compare && label != "first" && probe.path != "patched" {
        found.push(json!({"not patched": probe.path}));
    }
    let full = if mode == Mode::Compare && probe.path == "patched" {
        let mut full_probe = Probe {
            details: true,
            ..Probe::default()
        };
        let full = measured(
            out,
            &format!("{label}: dead code worked out in full"),
            || {
                aggregate_from_view(
                    job,
                    files,
                    entry_points,
                    snapshot,
                    &mut Vec::new(),
                    Use::Nothing,
                    Some(&mut full_probe),
                )
            },
        )?;
        let indexes = probe
            .indexes
            .iter()
            .zip(&full_probe.indexes)
            .filter(|(patched, full)| patched != full)
            .map(|(patched, _)| patched.0)
            .collect::<Vec<_>>();
        if !indexes.is_empty() || probe.indexes.len() != full_probe.indexes.len() {
            found.push(json!({"indexes differ": indexes}));
        }
        let blocks = full_probe
            .blocks
            .keys()
            .chain(probe.blocks.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|file| probe.blocks.get(*file) != full_probe.blocks.get(*file))
            .cloned()
            .collect::<Vec<_>>();
        if !blocks.is_empty() {
            let file = &blocks[0];
            found.push(json!({
                "blocks differ": blocks.len(),
                "first": file,
                "patched": probe.blocks.get(file),
                "full": full_probe.blocks.get(file),
            }));
        }
        Some(full)
    } else {
        None
    };
    let independent = match (rows, parsed) {
        (Some(rows), Some(parsed)) => Some(measured(
            out,
            &format!("{label}: dead code over every call row"),
            || aggregate(root, rows, parsed, &mut Vec::new()),
        )?),
        _ => None,
    };
    if let Ok(dir) = std::env::var("DEAD_CODE_GATE_DUMP") {
        for (side, aggregate) in [
            ("new", Some(&new)),
            ("kept", kept.as_ref()),
            ("full", full.as_ref()),
            ("rows", independent.as_ref()),
        ] {
            if let Some(aggregate) = aggregate {
                let _ = std::fs::write(
                    Path::new(&dir).join(format!("{label}-{side}.json")),
                    serde_json::to_vec_pretty(aggregate).unwrap_or_default(),
                );
            }
        }
    }
    let compared = [kept.as_ref(), full.as_ref(), independent.as_ref()]
        .into_iter()
        .flatten()
        .count();
    for other in [kept.as_ref(), full.as_ref(), independent.as_ref()]
        .into_iter()
        .flatten()
    {
        found.extend(differences(other, &new));
    }
    if compared > 0 || !found.is_empty() {
        summary.comparisons += 1;
        if !found.is_empty() {
            summary.differed += 1;
        }
    }
    let mut report = json!({"round": label, "counts": counts(&new)});
    report["identical"] = json!(found.is_empty());
    report["differences"] = json!(found);
    let _ = writeln!(out, "{report}");
    Ok(())
}

/// One round's dead-code facts: read from the content records, and in
/// `compare` mode also parsed from the files.
struct Scans {
    stored: Vec<FileContribution>,
    parsed: Option<Vec<FileContribution>>,
    /// Whether the two scans' facts are the same (true when only one ran).
    identical: bool,
}

fn scans(
    out: &mut dyn Write,
    label: &str,
    job: &InspectJob,
    entry_points: &[PathBuf],
    files: &[PathBuf],
    snapshot: &CallgraphSnapshot,
    rows: Option<&CallgraphSnapshot>,
) -> Result<Scans, String> {
    let (stored, fallbacks) = measured(
        out,
        &format!("{label}: read stored dead-code facts"),
        || scan_stored(job, entry_points.to_vec(), files, snapshot),
    )?;
    let _ = writeln!(
        out,
        "{}",
        json!({"files parsed for want of stored facts": fallbacks})
    );
    let parsed = rows.map(|rows| {
        measured(out, &format!("{label}: parse dead-code facts"), || {
            scan(job, entry_points.to_vec(), files, &rows.exported_symbols)
        })
    });
    let identical = parsed
        .as_ref()
        .is_none_or(|parsed| compare_scans(out, label, parsed, &stored));
    Ok(Scans {
        stored,
        parsed,
        identical,
    })
}

/// Counts the comparison of stored with parsed facts in `summary`.
fn count_facts(scans: Option<&Scans>, summary: &mut Summary) {
    if let Some(scans) = scans {
        summary.comparisons += 1;
        if !scans.identical {
            summary.differed += 1;
        }
    }
}

/// Replaces each contribution of `scans` whose file `rescanned` scanned
/// again.
fn splice(scans: &mut Scans, rescanned: Scans) {
    fn replace(all: &mut [FileContribution], fresh: Vec<FileContribution>) {
        for contribution in fresh {
            if let Some(slot) = all
                .iter_mut()
                .find(|existing| existing.file_path == contribution.file_path)
            {
                *slot = contribution;
            }
        }
    }
    replace(&mut scans.stored, rescanned.stored);
    if let (Some(all), Some(fresh)) = (&mut scans.parsed, rescanned.parsed) {
        replace(all, fresh);
    }
    scans.identical = rescanned.identical;
}

/// Unused exports as `aft_inspect` computes them on the data-layer path:
/// the Oxc engine over the scope files (with a fresh facts cache), then the
/// scanner's aggregate. It reads no call graph; the gate only measures it.
fn unused_exports(root: &Path, storage: &Path, files: &[PathBuf]) -> Value {
    let mut job = job(root, storage);
    job.category = InspectCategory::UnusedExports;
    job.key = JobKey::for_project_category(InspectCategory::UnusedExports);
    job.scope_files = files.to_vec();
    let entries = crate::inspect::resolve_entry_points(root);
    let options = crate::inspect::oxc_engine::AnalyzeOptions {
        entry_points: Vec::new(),
        public_api_files: entries.public_api_files(),
        executable_root_exports: entries.executable_root_exports(),
        force_reparse_files: Vec::new(),
        entry_reachability: false,
    };
    let mut cache = crate::inspect::oxc_engine::OxcFactsCache::new();
    let oxc =
        crate::inspect::oxc_engine::analyze_files_with_cache(root, files, options, &mut cache).ok();
    let result = crate::inspect::scanners::unused_exports::run_unused_exports_scan_with_oxc(
        &job,
        oxc.as_ref(),
    );
    result
        .outcome
        .map(|success| {
            json!({
                "count": success.aggregate["count"],
                "total_count": success.aggregate["total_count"],
                "scanned_files": success.aggregate["scanned_files"],
                "complete": success.aggregate["complete"],
            })
        })
        .unwrap_or_else(|error| json!({"error": error}))
}

/// One change the gate makes to the checkout.
enum Change {
    /// Appends text to a file.
    Append(PathBuf, String),
    /// Creates a file with this text.
    Create(PathBuf, String),
    /// Removes a file.
    Remove(PathBuf),
}

impl Change {
    fn paths(&self) -> Vec<PathBuf> {
        match self {
            Self::Append(path, _) | Self::Create(path, _) | Self::Remove(path) => {
                vec![path.clone()]
            }
        }
    }
}

/// Undoes the gate's changes when it ends, also on failure: each changed
/// file's earlier bytes, `None` for a file the gate created.
struct Restore(Vec<(PathBuf, Option<Vec<u8>>)>);

impl Restore {
    fn change(&mut self, change: Change) -> Result<(), String> {
        let path = change.paths().remove(0);
        let before = std::fs::read(&path).ok();
        self.0.push((path.clone(), before.clone()));
        match change {
            Change::Append(_, text) => {
                let mut bytes = before.unwrap_or_default();
                bytes.extend_from_slice(text.as_bytes());
                std::fs::write(&path, bytes)
            }
            Change::Create(_, text) => std::fs::write(&path, text),
            Change::Remove(_) => std::fs::remove_file(&path),
        }
        .map_err(|error| format!("{}: {error}", path.display()))
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        for (path, bytes) in self.0.iter().rev() {
            let _ = match bytes {
                Some(bytes) => std::fs::write(path, bytes),
                None => std::fs::remove_file(path),
            };
        }
    }
}

/// Changes that alter whole-project index entries other files read, so a
/// patched rollup must find and redo those files: a name exported by one
/// file becomes exported by a second (per language), a TypeScript export
/// becomes imported (its Oxc verdict changes), a Rust module is declared
/// with a new file, and a file is removed. Files in `edited` are left
/// alone.
fn index_changes(
    view: &CheckoutView,
    editable: &[PathBuf],
    edited: &[PathBuf],
) -> Result<Vec<(String, Change)>, String> {
    let files = view.dead_code_files();
    let graph = super::dead_code_graph::DeadCodeGraph::open(view.root(), &files)?;
    let exports = graph.all_exported_symbols();
    let mut exporters = std::collections::BTreeMap::<&str, BTreeSet<&Path>>::new();
    for export in &exports {
        exporters
            .entry(export.symbol.as_str())
            .or_default()
            .insert(export.file.as_path());
    }
    let identifier = |name: &str| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    };
    let extension = |path: &Path| {
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_string()
    };
    let unused = editable
        .iter()
        .filter(|file| !edited.contains(file))
        .cloned()
        .collect::<Vec<_>>();
    let mut taken = BTreeSet::new();
    let mut changes = Vec::new();
    for language in ["rs", "go", "ts", "py"] {
        let Some(target) = unused
            .iter()
            .find(|file| extension(file) == language && !taken.contains(*file))
        else {
            continue;
        };
        let Some(export) = exports.iter().find(|export| {
            extension(&export.file) == language
                && export.file != **target
                && export.kind == "function"
                && identifier(&export.symbol)
                && exporters.get(export.symbol.as_str()).map(BTreeSet::len) == Some(1)
        }) else {
            continue;
        };
        let name = &export.symbol;
        let text = match language {
            "rs" => format!("\npub fn {name}() {{}}\n"),
            "go" => format!("\nfunc {name}() {{}}\n"),
            "ts" => format!("\nexport function {name}() {{}}\n"),
            _ => format!("\ndef {name}():\n    pass\n"),
        };
        taken.insert((*target).clone());
        changes.push((
            format!("second exporter of {name} ({language})"),
            Change::Append((*target).clone(), text),
        ));
    }
    // A TypeScript export imported by a file beside it.
    let typescript = exports.iter().find(|export| {
        extension(&export.file) == "ts"
            && export.kind == "function"
            && identifier(&export.symbol)
            && unused.iter().any(|file| {
                extension(file) == "ts"
                    && file.parent() == export.file.parent()
                    && *file != export.file
                    && !taken.contains(file)
            })
    });
    if let Some(export) = typescript {
        let importer = unused
            .iter()
            .find(|file| {
                extension(file) == "ts"
                    && file.parent() == export.file.parent()
                    && **file != export.file
                    && !taken.contains(*file)
            })
            .expect("found above");
        let stem = export
            .file
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        taken.insert((*importer).clone());
        changes.push((
            format!("import of {}", export.symbol),
            Change::Append(
                (*importer).clone(),
                format!(
                    "\nimport {{ {} as deadCodeGateImported }} from \"./{stem}\";\nexport const deadCodeGateUses = deadCodeGateImported;\n",
                    export.symbol
                ),
            ),
        ));
    }
    // A Rust module declared by a crate or module root, with a new file.
    let root_file = unused.iter().find(|file| {
        extension(file) == "rs"
            && matches!(
                file.file_name().and_then(|name| name.to_str()),
                Some("lib.rs" | "main.rs" | "mod.rs")
            )
            && !taken.contains(*file)
    });
    if let Some(root_file) = root_file {
        let module = root_file
            .parent()
            .unwrap_or(Path::new(""))
            .join("dead_code_gate_module.rs");
        if !module.exists() {
            changes.push((
                "new module file".to_string(),
                Change::Create(module, "pub fn dead_code_gate_module_fn() {}\n".to_string()),
            ));
            taken.insert((*root_file).clone());
            changes.push((
                "module declared".to_string(),
                Change::Append(
                    (*root_file).clone(),
                    "\nmod dead_code_gate_module;\n".to_string(),
                ),
            ));
        }
    }
    // A file removed.
    if let Some(file) = unused.iter().rev().find(|file| !taken.contains(*file)) {
        changes.push((
            format!("removal of {}", file.display()),
            Change::Remove((*file).clone()),
        ));
    }
    Ok(changes)
}

/// Runs the gate on the directory `root`, with the checkout's family store
/// under `storage`. After the first round, `edits` files (Rust, Go,
/// TypeScript or Python, chosen evenly through the file list; any file when
/// the project has none of those) each gain a new function, the call graph
/// catches up, and a round runs after each; the files are restored
/// afterwards.
///
/// # Errors
///
/// When the checkout cannot be built or a call graph cannot be read.
pub fn run(
    root: &Path,
    storage: &Path,
    mode: Mode,
    edits: usize,
    out: &mut dyn Write,
) -> Result<Summary, String> {
    let root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
    let mut summary = Summary::default();
    let checkout = measured(out, "build checkout", || {
        let checkout = DataCheckout::open_planes(
            &root,
            storage,
            None,
            Planes {
                text: false,
                callgraph: true,
            },
        )
        .map_err(|error| format!("{error:?}"))?;
        fresh_view(&checkout)?;
        Ok::<_, String>(checkout)
    })?;
    let view = fresh_view(&checkout)?;
    let job = job(&root, storage);
    let files = crate::inspect::tier2_scope_files(&root, &JobScope::for_project(root.clone()));
    let entry_points = {
        let entries = crate::inspect::resolve_entry_points(&root);
        files
            .iter()
            .filter(|file| entries.is_entry_point(file))
            .cloned()
            .collect::<Vec<_>>()
    };
    let (snapshot, rows) = snapshots(out, "first", mode, &view, storage)?;
    // In `compare` mode, each file's stored facts are checked against its
    // parsed facts, and the parsed facts feed the independent computation.
    let mut all = rows
        .as_ref()
        .map(|rows| {
            scans(
                out,
                "first",
                &job,
                &entry_points,
                &files,
                &snapshot,
                Some(rows),
            )
        })
        .transpose()?;
    count_facts(all.as_ref(), &mut summary);
    let _ = writeln!(out, "{}", json!({"files": files.len()}));
    round(
        out,
        "first",
        mode,
        &root,
        &job,
        &files,
        &entry_points,
        &snapshot,
        rows.as_ref(),
        all.as_ref().and_then(|all| all.parsed.as_deref()),
        &mut summary,
    )?;
    let kept_bytes = std::fs::read_dir(storage.join("dead-code"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "dead-code")
        })
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .collect::<Vec<_>>();
    let _ = writeln!(out, "{}", json!({"kept graphs": kept_bytes}));
    if std::env::var_os("DEAD_CODE_GATE_UNUSED_EXPORTS").is_some() {
        let counts = measured(out, "unused exports (Oxc scan and aggregate)", || {
            unused_exports(&root, storage, &files)
        });
        let _ = writeln!(out, "{}", json!({"unused exports": counts}));
    }
    drop((view, snapshot, rows));
    if edits == 0 {
        return Ok(summary);
    }

    let mut editable = files
        .iter()
        .filter(|file| {
            matches!(
                file.extension().and_then(|extension| extension.to_str()),
                Some("rs" | "go" | "ts" | "py")
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    // A project in other languages gets its edits too, so its patched
    // rollup is checked as well. The baseline gate (`dead_code_gate.sh` runs
    // one built from a commit that still has the old string rollup, as the
    // oracle) edits none of these files, so its "after edits" answer for
    // such a project is the unedited one; this run's answers there are
    // checked against the rollup worked out in full and over every call
    // row only.
    if editable.is_empty() {
        editable = files.clone();
    }
    let step = (editable.len() / edits).max(1);
    let plan = editable
        .iter()
        .step_by(step)
        .take(edits)
        .cloned()
        .collect::<Vec<_>>();
    let mut restore = Restore(Vec::new());
    let mut edited = Vec::new();
    // The same edits the baseline gate makes (for projects with Rust, Go,
    // TypeScript or Python files), one at a time with a round after each;
    // the last round's answer is the one compared with the baseline's.
    for (index, file) in plan.iter().enumerate() {
        let addition = match file.extension().and_then(|extension| extension.to_str()) {
            Some("rs") => format!("\npub fn dead_code_gate_edit_{index}() {{}}\n"),
            Some("go") => format!("\nfunc DeadCodeGateEdit{index}() {{}}\n"),
            Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs") => {
                format!("\nexport function deadCodeGateEdit{index}() {{}}\n")
            }
            Some("py") => format!("\ndef dead_code_gate_edit_{index}():\n    pass\n"),
            Some("c" | "h" | "cpp" | "cu" | "metal" | "m") => {
                format!("\nvoid dead_code_gate_edit_{index}(void) {{}}\n")
            }
            Some("kt") => format!("\nfun deadCodeGateEdit{index}() {{}}\n"),
            Some("swift") => format!("\nfunc deadCodeGateEdit{index}() {{}}\n"),
            Some("lua") => format!("\nfunction dead_code_gate_edit_{index}() end\n"),
            Some("rb") => format!("\ndef dead_code_gate_edit_{index}; end\n"),
            Some("pl" | "pm") => format!("\nsub dead_code_gate_edit_{index} {{}}\n"),
            Some("r" | "R") => format!("\ndead_code_gate_edit_{index} <- function() {{}}\n"),
            Some("sh" | "bash") => format!("\ndead_code_gate_edit_{index}() {{ :; }}\n"),
            Some("zig") => format!("\npub fn deadCodeGateEdit{index}() void {{}}\n"),
            // Languages without a simple top-level function, and data
            // files: a new line still makes a new version of the file.
            _ => "\n".to_string(),
        };
        restore.change(Change::Append(file.clone(), addition))?;
        edited.push(file.clone());
        let _ = writeln!(out, "{}", json!({"edited": [file]}));
        checkout.note_paths(std::slice::from_ref(file));
        let last = index + 1 == plan.len();
        if !last {
            let label = format!("edit {}", index + 1);
            let view = measured(out, &format!("refresh call graph after {label}"), || {
                fresh_view(&checkout)
            })?;
            let (snapshot, _) = snapshots(out, &label, Mode::New, &view, storage)?;
            round(
                out,
                &label,
                mode,
                &root,
                &job,
                &files,
                &entry_points,
                &snapshot,
                None,
                None,
                &mut summary,
            )?;
            continue;
        }
        let view = measured(out, "refresh call graph after edits", || {
            fresh_view(&checkout)
        })?;
        let (snapshot, rows) = snapshots(out, "after edits", mode, &view, storage)?;
        if let Some(all) = &mut all {
            let rescanned = scans(
                out,
                "after edits",
                &job,
                &entry_points,
                &edited,
                &snapshot,
                rows.as_ref(),
            )?;
            splice(all, rescanned);
        }
        count_facts(all.as_ref(), &mut summary);
        round(
            out,
            "after edits",
            mode,
            &root,
            &job,
            &files,
            &entry_points,
            &snapshot,
            rows.as_ref(),
            all.as_ref().and_then(|all| all.parsed.as_deref()),
            &mut summary,
        )?;
    }
    if mode == Mode::Compare {
        let view = fresh_view(&checkout)?;
        let changes = index_changes(&view, &editable, &edited)?;
        drop(view);
        for (label, change) in changes {
            let touched = change.paths();
            let _ = writeln!(out, "{}", json!({"change": label, "paths": touched}));
            restore.change(change)?;
            checkout.note_paths(&touched);
            let view = measured(out, &format!("refresh call graph after {label}"), || {
                fresh_view(&checkout)
            })?;
            let files =
                crate::inspect::tier2_scope_files(&root, &JobScope::for_project(root.clone()));
            let entry_points = {
                let entries = crate::inspect::resolve_entry_points(&root);
                files
                    .iter()
                    .filter(|file| entries.is_entry_point(file))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            let (snapshot, _) = snapshots(out, &label, Mode::New, &view, storage)?;
            round(
                out,
                &label,
                mode,
                &root,
                &job,
                &files,
                &entry_points,
                &snapshot,
                None,
                None,
                &mut summary,
            )?;
        }
    }
    drop(restore);
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::scanners::dead_code::section::DAMAGED_FACTS_FOR_TEST;

    /// A small Rust crate with a dead function, and its checkout.
    struct Fixture {
        _dir: tempfile::TempDir,
        storage: tempfile::TempDir,
        root: PathBuf,
        checkout: Arc<DataCheckout>,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with(|_| {})
        }

        /// The fixture, with `setup` run on its root before the checkout is
        /// built.
        fn with(setup: impl FnOnce(&Path)) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(
                root.join("Cargo.toml"),
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .unwrap();
            std::fs::write(
                root.join("src/lib.rs"),
                "mod util;\npub fn api() { util::helper(); }\n",
            )
            .unwrap();
            std::fs::write(
                root.join("src/util.rs"),
                "pub fn helper() {}\npub fn orphan() {}\n",
            )
            .unwrap();
            setup(&root);
            let storage = tempfile::tempdir().unwrap();
            let checkout = DataCheckout::open_planes(
                &root,
                storage.path(),
                None,
                Planes {
                    text: false,
                    callgraph: true,
                },
            )
            .unwrap();
            Self {
                _dir: dir,
                storage,
                root,
                checkout,
            }
        }

        /// Dead code as `aft_inspect` rolls it up, keeping its graph, and
        /// the path the rollup took.
        fn rollup(&self) -> (Result<Value, String>, &'static str) {
            let view = fresh_view(&self.checkout).unwrap();
            let (snapshot, _) = snapshots(
                &mut std::io::sink(),
                "test",
                Mode::New,
                &view,
                self.storage.path(),
            )
            .unwrap();
            let files = crate::inspect::tier2_scope_files(
                &self.root,
                &JobScope::for_project(self.root.clone()),
            );
            let mut probe = Probe::default();
            let aggregate = aggregate_from_view(
                &job(&self.root, self.storage.path()),
                &files,
                &[],
                &snapshot,
                &mut Vec::new(),
                Use::Kept,
                Some(&mut probe),
            );
            (aggregate, probe.path)
        }

        fn edit(&self, file: &str, addition: &str) {
            let path = self.root.join(file);
            let mut text = std::fs::read_to_string(&path).unwrap();
            text.push_str(addition);
            std::fs::write(&path, text).unwrap();
            self.checkout.note_paths(&[path]);
        }

        /// The one kept file with `extension`.
        fn kept(&self, extension: &str) -> PathBuf {
            let kept = std::fs::read_dir(self.storage.path().join("dead-code"))
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == extension))
                .collect::<Vec<_>>();
            assert_eq!(kept.len(), 1, "{kept:?}");
            kept.into_iter().next().unwrap()
        }

        fn damage_facts(&self, file: &str) {
            DAMAGED_FACTS_FOR_TEST
                .lock()
                .unwrap()
                .push((self.root.clone(), file.to_string()));
        }
    }

    /// Changes one byte of the file at `path` at or after `from` that
    /// `pick` accepts, to `replace(byte)`.
    fn damage(path: &Path, from: usize, pick: impl Fn(u8) -> bool, replace: impl Fn(u8) -> u8) {
        let mut bytes = std::fs::read(path).unwrap();
        let at = (from..bytes.len())
            .find(|at| pick(bytes[*at]))
            .expect("a byte to damage");
        bytes[at] = replace(bytes[at]);
        std::fs::write(path, bytes).unwrap();
    }

    /// The kept graph is a copy of what the stored facts give. A damaged
    /// copy is never read: the rollup works the graph out again and answers
    /// as it did before the damage.
    #[test]
    fn a_damaged_kept_graph_is_worked_out_again() {
        let fixture = Fixture::new();
        let (first, path) = fixture.rollup();
        let first = first.unwrap();
        assert_eq!(path, "full");
        assert_eq!(fixture.rollup().1, "read");

        // One byte in the middle of the base, where its blocks are.
        let base = fixture.kept("dead-code");
        let len = std::fs::metadata(&base).unwrap().len() as usize;
        damage(&base, len / 2, |_| true, |byte| byte ^ 0x01);
        let (again, path) = fixture.rollup();
        assert_eq!(path, "full", "a damaged base must not be read");
        assert_eq!(again.unwrap(), first);

        // One digit of an overlay's body, which leaves it valid JSON.
        fixture.edit("src/util.rs", "\npub fn added() {}\n");
        let (patched, path) = fixture.rollup();
        assert_eq!(path, "patched");
        let patched = patched.unwrap();
        let overlay = fixture.kept("dead-code-overlay");
        let body = std::fs::read(&overlay)
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'\n')
            .nth(3)
            .map(|(at, _)| at + 1)
            .unwrap();
        damage(
            &overlay,
            body,
            |byte| byte.is_ascii_digit(),
            |byte| if byte == b'9' { b'8' } else { byte + 1 },
        );
        let (again, path) = fixture.rollup();
        assert_ne!(path, "read", "a damaged overlay must not be read");
        assert_eq!(again.unwrap(), patched);
    }

    /// The stored facts are what dead code is worked out from. Facts that
    /// cannot be read fail the rollup with the file named, before a graph is
    /// kept and when a kept graph is patched.
    #[test]
    fn damaged_stored_facts_fail_the_rollup() {
        let fixture = Fixture::new();
        fixture.damage_facts("src/util.rs");
        let (failed, _) = fixture.rollup();
        let error = failed.expect_err("damaged facts must fail the first rollup");
        assert!(error.contains("src/util.rs"), "{error}");
        DAMAGED_FACTS_FOR_TEST
            .lock()
            .unwrap()
            .retain(|(root, _)| *root != fixture.root);

        let (first, path) = fixture.rollup();
        first.unwrap();
        assert_eq!(path, "full");
        fixture.edit("src/util.rs", "\npub fn added() {}\n");
        fixture.damage_facts("src/util.rs");
        let (failed, _) = fixture.rollup();
        let error = failed.expect_err("damaged facts must fail a patched rollup");
        assert!(error.contains("src/util.rs"), "{error}");
        DAMAGED_FACTS_FOR_TEST
            .lock()
            .unwrap()
            .retain(|(root, _)| *root != fixture.root);
    }

    /// Every file of the call graph resolves to the path canonicalizing it
    /// gives, with a symlinked file and a symlinked folder in the project,
    /// and only regular files resolve without asking the file system.
    #[cfg(unix)]
    #[test]
    fn store_paths_resolve_as_canonicalizing_them_would() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("shared.rs"), "pub fn shared() {}\n").unwrap();
        let fixture = Fixture::with(|root| {
            std::fs::create_dir_all(root.join("src/real")).unwrap();
            std::fs::write(root.join("src/real/inner.rs"), "pub fn inner() {}\n").unwrap();
            std::os::unix::fs::symlink(root.join("src/real"), root.join("src/linked")).unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("shared.rs"),
                root.join("src/shared.rs"),
            )
            .unwrap();
        });
        let view = fresh_view(&fixture.checkout).unwrap();
        let files = view.dead_code_files();
        let graph = super::super::dead_code_graph::DeadCodeGraph::open_lazy(view.root(), &files);
        let paths = super::super::dead_code_graph::CallGraphFiles::paths(&files);
        assert!(
            paths.iter().any(|path| path == "src/real/inner.rs"),
            "{paths:?}"
        );
        for path in paths.iter().map(String::as_str).chain([
            "src/linked/inner.rs",
            "src/shared.rs",
            "src/../src/lib.rs",
        ]) {
            let canonical = super::super::dead_code::Paths {
                root: view.root(),
                cache: std::collections::HashMap::new(),
            }
            .resolve(path);
            assert_eq!(graph.resolve(path), canonical, "{path}");
        }
        assert!(graph.resolves_lexically("src/real/inner.rs"));
        assert!(!graph.resolves_lexically("src/shared.rs"));
        assert!(!graph.resolves_lexically("src/linked/inner.rs"));
        assert!(!graph.resolves_lexically("src/../src/lib.rs"));
    }
}
