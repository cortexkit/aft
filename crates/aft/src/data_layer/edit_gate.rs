//! Replays real edits through [`LiveCheckout`] and checks each result
//! against a cold resolution of the same files.
//!
//! The edits come from an edit pack (`explore/edit_replay/make_pack.py` on
//! the edit-replay branch): a repository's call-graph inputs at a base
//! commit, then every later commit as one edit per diff hunk, in order, the
//! way an agent's edit tool applies them. Runs from
//! `examples/edit_gate.rs`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use aft_store::cache::BlockCache;
use aft_store::checkout::{CheckoutState, PathKind, PathRecord, StatEvidence};
use aft_store::content::{ContentRecord, RecordBody, RecordKey};
use aft_store::ids::{ContentKey, Digest128};
use aft_store::ids::{FamilyId, SegmentId, WriterIncarnation};
use aft_store::segment::{Segment, SegmentMeta};

use super::callgraph::{resolve_checkout, CheckoutInputs, FileResolution};
use super::content::{blob_from_record, extract, language_for, EXTRACTOR_VERSION};
use super::graph_store::{known_sections, write_segment, StoredSlices};
use super::incremental::{LiveCheckout, UpdateStats};
use super::overlay::OverlaySlices;
use super::query::{MemorySlices, SliceReader};
use super::slice::FileSlice;
use crate::callgraph_store::join::{CallgraphBlob, ParseBlob};

/// One edit's measurement and check.
#[derive(Debug, serde::Serialize)]
pub struct EditRecord {
    pub commit: usize,
    pub seq: usize,
    pub path: String,
    pub kind: String,
    #[serde(flatten)]
    pub stats: UpdateStatsRecord,
    pub extract_ms: f64,
    /// Files whose rows, kept sites or dispatch targets differ from the
    /// cold resolution; `None` when this edit was not checked.
    pub differing_files: Option<usize>,
    /// Files whose recorded inputs differ from the cold resolution's.
    pub differing_recordings: Option<usize>,
    /// Files whose recorded answers no longer hold after this edit, though
    /// the update kept them: checked after every edit.
    pub undiscovered: usize,
    pub discovery_ms: f64,
    /// Files the overlay over the base segment replaces or removes; `None`
    /// when the overlay was not checked at this edit.
    pub overlay_files: Option<usize>,
    /// Store primitives whose answer over the overlay differs from slices
    /// of a cold resolution.
    pub overlay_differing: Option<usize>,
    pub overlay_check_ms: Option<f64>,
    pub examples: Vec<String>,
}

/// [`UpdateStats`] as a flat record.
#[derive(Debug, Default, serde::Serialize)]
pub struct UpdateStatsRecord {
    pub answers_checked: usize,
    pub answers_changed: usize,
    pub facts_ms: f64,
    pub rebound: usize,
    pub rebound_by_keys: usize,
    pub rebound_by_answers: usize,
    pub rebound_top_keys: Vec<(String, usize)>,
    pub surfaces_changed: usize,
    pub replayed: usize,
    pub kept_after_replay: usize,
    pub resolved: usize,
    pub dispatch_replayed: usize,
    pub dispatch_recomputed: usize,
    pub facts_changed: usize,
    pub sites_resolved: usize,
    pub bind_ms: f64,
    pub resolve_ms: f64,
    pub dispatch_ms: f64,
    pub sites_ms: f64,
    pub total_ms: f64,
}

impl From<UpdateStats> for UpdateStatsRecord {
    fn from(stats: UpdateStats) -> Self {
        Self {
            answers_checked: stats.answers_checked,
            answers_changed: stats.answers_changed,
            facts_ms: stats.facts_ms,
            rebound: stats.rebound,
            rebound_by_keys: stats.rebound_by_keys,
            rebound_by_answers: stats.rebound_by_answers,
            rebound_top_keys: stats.rebound_top_keys,
            surfaces_changed: stats.surfaces_changed,
            replayed: stats.replayed,
            kept_after_replay: stats.kept_after_replay,
            resolved: stats.resolved,
            dispatch_replayed: stats.dispatch_replayed,
            dispatch_recomputed: stats.dispatch_recomputed,
            facts_changed: stats.facts_changed,
            sites_resolved: stats.sites_resolved,
            bind_ms: stats.bind_ms,
            resolve_ms: stats.resolve_ms,
            dispatch_ms: stats.dispatch_ms,
            sites_ms: stats.sites_ms,
            total_ms: stats.total_ms,
        }
    }
}

/// What one file of the checkout holds.
#[derive(Clone)]
enum Parsed {
    Parse(String, Arc<ParseBlob>),
    Config(Arc<[u8]>),
}

/// The checkout as text files, with each file's extraction kept so an edit
/// extracts only the file it changes.
#[derive(Default)]
pub(super) struct Files {
    entries: BTreeMap<String, (ContentKey, String, Option<Parsed>)>,
    records: HashMap<RecordKey, Option<Parsed>>,
    /// Every parsed record, by the key its parse carries.
    contents: HashMap<String, (RecordKey, Arc<ContentRecord>)>,
}

impl Files {
    pub(super) fn set(&mut self, path: &str, text: &str) -> Result<(), String> {
        let bytes = text.as_bytes();
        let content = ContentKey::of(bytes);
        let language = language_for(path, bytes).unwrap_or_default();
        let parsed = if language.is_empty() {
            None
        } else {
            let key = RecordKey::new(content, &language, EXTRACTOR_VERSION);
            if let Some(known) = self.records.get(&key) {
                known.clone()
            } else {
                let record =
                    extract(bytes, &language).map_err(|error| format!("{path}: {error}"))?;
                let parsed = match &record.data {
                    RecordBody::Config { source } => {
                        Some(Parsed::Config(Arc::from(source.as_slice())))
                    }
                    RecordBody::Parsed { .. } => {
                        match blob_from_record(&record).map_err(|error| error.to_string())? {
                            CallgraphBlob::Parse(parse) => {
                                let parse_key = key.digest().to_string();
                                self.contents
                                    .insert(parse_key.clone(), (key, Arc::new(record.clone())));
                                Some(Parsed::Parse(parse_key, Arc::new(parse)))
                            }
                            CallgraphBlob::Config(_) => None,
                        }
                    }
                };
                self.records.insert(key, parsed.clone());
                parsed
            }
        };
        self.entries
            .insert(path.to_string(), (content, language, parsed));
        Ok(())
    }

    pub(super) fn remove(&mut self, path: &str) {
        self.entries.remove(path);
    }

    pub(super) fn inputs(&self) -> CheckoutInputs {
        let records = self
            .entries
            .iter()
            .map(|(path, (content, language, _))| PathRecord {
                path: path.as_bytes().to_vec(),
                mode: 0o100_644,
                kind: PathKind::Text,
                content: *content,
                link: Vec::new(),
                stat: StatEvidence::default(),
                language: language.clone(),
            })
            .collect();
        let state = CheckoutState::new(Vec::new(), Digest128::of(b""), records)
            .expect("paths are unique and short");
        let mut inputs = CheckoutInputs {
            state,
            parses: super::callgraph::Parses::default(),
            configs: HashMap::new(),
        };
        for (path, (_, _, parsed)) in &self.entries {
            match parsed {
                Some(Parsed::Parse(key, parse)) => {
                    inputs
                        .parses
                        .insert(path.clone(), (key.clone(), Arc::clone(parse)));
                }
                Some(Parsed::Config(source)) => {
                    inputs
                        .configs
                        .insert(path.as_bytes().to_vec(), Arc::clone(source));
                }
                None => {}
            }
        }
        inputs
    }
}

/// Files of `live` whose answers or recordings differ from `cold`'s.
fn compare(
    live: &BTreeMap<String, FileResolution>,
    cold: &BTreeMap<String, FileResolution>,
    examples: &mut Vec<String>,
) -> (usize, usize) {
    let mut answers = 0;
    let mut recordings = 0;
    let paths = live.keys().chain(cold.keys()).collect::<BTreeSet<_>>();
    for path in paths {
        let (Some(ours), Some(theirs)) = (live.get(path), cold.get(path)) else {
            answers += 1;
            examples.push(format!("{path}: present on one side only"));
            continue;
        };
        let mut differs = Vec::new();
        if ours.rows != theirs.rows {
            differs.push("rows");
        }
        if ours.sites != theirs.sites {
            differs.push("sites");
        }
        if ours.dispatch.as_ref().map(|dispatch| &dispatch.targets)
            != theirs.dispatch.as_ref().map(|dispatch| &dispatch.targets)
        {
            differs.push("dispatch targets");
        }
        if !differs.is_empty() {
            answers += 1;
            if examples.len() < 10 {
                examples.push(format!("{path}: {}", differs.join(", ")));
            }
        }
        if ours.binding != theirs.binding
            || ours.site_dependencies != theirs.site_dependencies
            || ours.dispatch != theirs.dispatch
        {
            recordings += 1;
            if examples.len() < 20 && differs.is_empty() {
                let what = [
                    (ours.binding != theirs.binding, "binding"),
                    (
                        ours.site_dependencies != theirs.site_dependencies,
                        "site dependencies",
                    ),
                    (ours.dispatch != theirs.dispatch, "dispatch trace"),
                ]
                .into_iter()
                .filter(|(differs, _)| *differs)
                .map(|(_, name)| name)
                .collect::<Vec<_>>();
                examples.push(format!("{path}: recorded {}", what.join(", ")));
                // Recorded checkout reads only one side made, or answered
                // differently, by name.
                let reads = |file: &FileResolution, part: &str| -> BTreeMap<String, String> {
                    let answers = match part {
                        "binding" => &file.binding.binding_answers,
                        "resolution" => &file.binding.resolution_answers,
                        _ => match &file.dispatch {
                            Some(dispatch) => &dispatch.trace.answers,
                            None => return BTreeMap::new(),
                        },
                    };
                    answers
                        .iter()
                        .map(|(question, answer)| {
                            (describe_question(question), format!("{answer:?}"))
                        })
                        .collect()
                };
                for part in ["binding", "resolution", "dispatch"] {
                    let (ours_reads, theirs_reads) = (reads(ours, part), reads(theirs, part));
                    for (question, answer) in &ours_reads {
                        if theirs_reads.get(question) != Some(answer) {
                            examples
                                .push(format!("  {part} read only live: {question} = {answer}"));
                        }
                    }
                    for (question, answer) in &theirs_reads {
                        if ours_reads.get(question) != Some(answer) {
                            examples
                                .push(format!("  {part} read only cold: {question} = {answer}"));
                        }
                    }
                }
                // Which lines of the two recordings' debug forms differ.
                let ours_text = format!(
                    "{:#?}",
                    (&ours.binding, &ours.site_dependencies, &ours.dispatch)
                );
                let theirs_text = format!(
                    "{:#?}",
                    (&theirs.binding, &theirs.site_dependencies, &theirs.dispatch)
                );
                let ours_lines = ours_text.lines().collect::<BTreeSet<_>>();
                let theirs_lines = theirs_text.lines().collect::<BTreeSet<_>>();
                for line in ours_lines.difference(&theirs_lines).take(6) {
                    examples.push(format!("  live: {}", line.trim()));
                }
                for line in theirs_lines.difference(&ours_lines).take(6) {
                    examples.push(format!("  cold: {}", line.trim()));
                }
            }
        }
    }
    (answers, recordings)
}

/// A recorded checkout read, with its path as text.
pub(super) fn describe_question(
    question: &crate::callgraph_store::fact_answers::FactQuestion,
) -> String {
    use crate::callgraph_store::fact_answers::FactQuestion as Q;
    let (name, path) = match question {
        Q::IsFile(path) => ("is_file", path),
        Q::IsDir(path) => ("is_dir", path),
        Q::Link(path) => ("link", path),
        Q::Canonical(path) => ("canonical", path),
        Q::ListDir(path) => ("list_dir", path),
        Q::Config(path) => ("config", path),
        Q::ConfigField(path, field) => {
            return format!("config_field({}, {field})", String::from_utf8_lossy(path));
        }
    };
    format!("{name}({})", String::from_utf8_lossy(path))
}

/// Replays the pack at `pack`, checking every `check_every`-th edit and
/// every commit's last edit against a cold resolution. Writes one JSON line
/// per edit to `out`.
///
/// # Errors
///
/// When the pack cannot be read or resolution fails.
/// Resolves one file of the checkout at `root` again and again, for
/// profiling: binds every file the checkout tracks once, then times
/// `iterations` rounds of resolving `file` (passes 2 and 3) and of its
/// method-call sites (pass 4), on one thread. Prints each round's times to
/// standard error.
///
/// # Errors
///
/// When a file cannot be read or extracted, or resolution fails.
pub fn bench_file(root: &Path, file: &str, iterations: usize) -> Result<(), String> {
    let listed = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .map_err(|error| error.to_string())?;
    let mut files = Files::default();
    for path in listed
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = String::from_utf8_lossy(path).into_owned();
        let Ok(text) = std::fs::read_to_string(root.join(&path)) else {
            continue;
        };
        files.set(&path, &text)?;
    }
    let inputs = files.inputs();
    let error = |error: crate::callgraph_store::join::ManifestJoinError| error.to_string();
    super::callgraph::pool(1).map_err(error)?.install(|| {
        let paths = inputs.parses.keys().collect::<Vec<_>>();
        let bound = super::callgraph::BindCache::default();
        bound.bind_missing(&inputs, &paths).map_err(error)?;
        let resolved = super::callgraph::resolve_files(&inputs, &bound, &paths).map_err(error)?;
        let target = paths
            .iter()
            .find(|path| path.as_str() == file)
            .copied()
            .ok_or_else(|| format!("{file} is not a parsed file"))?;
        let mut calls = super::callgraph::MethodCallInputs::build(&inputs, &resolved);
        for round in 0..iterations {
            let started = std::time::Instant::now();
            let one = super::callgraph::resolve_files(&inputs, &bound, &[target]).map_err(error)?;
            let resolve_ms = started.elapsed().as_secs_f64() * 1000.0;
            let started = std::time::Instant::now();
            let sites = calls.resolve_sites(&inputs, &resolved, &[target]);
            let sites_ms = started.elapsed().as_secs_f64() * 1000.0;
            eprintln!(
                "round {round}: resolve {resolve_ms:.0} ms ({} rows), sites {sites_ms:.0} ms ({} sites)",
                one[target].rows.len(),
                sites.first().map_or(0, |(_, sites, _)| sites.len()),
            );
        }
        Ok(())
    })
}

pub fn run(
    pack: &Path,
    threads: usize,
    check_every: usize,
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
    let mut files = Files::default();
    let started = Instant::now();
    for (path, text) in first["files"].as_object().ok_or("no base files")? {
        files.set(path, text.as_str().unwrap_or_default())?;
    }
    let extract_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let base_inputs = files.inputs();
    let mut live = LiveCheckout::cold(files.inputs()).map_err(|error| error.to_string())?;
    let cold_ms = started.elapsed().as_secs_f64() * 1000.0;

    // The base segment the overlay is checked over: the cold resolution,
    // stored once, never republished during the run.
    let directory = std::env::temp_dir().join(format!("aft-edit-gate-{}", std::process::id()));
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let segment_path = directory.join("1.seg");
    let family = FamilyId::from_digest(Digest128::of(b"edit gate"));
    let fragments = write_segment(
        &segment_path,
        SegmentMeta {
            family,
            segment: SegmentId::new(1),
            incarnation: WriterIncarnation::new(1),
            created_unix_ms: 0,
        },
        &base_inputs,
        &live.resolved.to_map(),
        &files.contents,
    )?;
    let segment = Arc::new(
        Segment::open(
            &segment_path,
            family,
            &known_sections(),
            Arc::new(BlockCache::new(256 << 20)),
        )
        .map_err(|error| error.to_string())?,
    );
    let base = base_inputs
        .parses
        .iter()
        .map(|(path, (key, _))| {
            let bytes = super::fragment::encode(&live.resolved[path]);
            (path.clone(), (key.clone(), bytes))
        })
        .collect::<BTreeMap<_, _>>();
    writeln!(
        out,
        "{}",
        serde_json::json!({"base": true, "files": files.entries.len(), "extract_ms": extract_ms,
            "cold_ms": cold_ms,
            "segment_bytes": std::fs::metadata(&segment_path).map(|m| m.len()).unwrap_or(0)})
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
    // `EDIT_GATE_EDITS=N` replays only the pack's first N edits: a short run
    // for checking a change before the whole pack. Its last edit counts as
    // the end of a commit, so the run ends with a full check.
    if let Some(limit) = std::env::var("EDIT_GATE_EDITS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    {
        edits.truncate(limit);
    }
    let mut failures = 0;
    let mut checked = 0;
    let timing_only = std::env::var_os("EDIT_GATE_TIMING_ONLY").is_some_and(|value| value == "1");
    for (index, edit) in edits.iter().enumerate() {
        let path = edit["path"]
            .as_str()
            .ok_or("edit without path")?
            .to_string();
        let kind = edit["kind"].as_str().unwrap_or_default().to_string();
        let started = Instant::now();
        if kind == "delete" {
            files.entries.remove(&path);
        } else {
            files.set(&path, edit["text"].as_str().unwrap_or_default())?;
        }
        let extract_ms = started.elapsed().as_secs_f64() * 1000.0;
        let stats = live
            .update(files.inputs(), &BTreeSet::from([path.clone()]))
            .map_err(|error| error.to_string())?;
        let last_of_commit = edits
            .get(index + 1)
            .is_none_or(|next| next["commit"] != edit["commit"]);
        let mut record = EditRecord {
            commit: edit["commit"].as_u64().unwrap_or_default() as usize,
            seq: edit["seq"].as_u64().unwrap_or_default() as usize,
            path,
            kind,
            stats: stats.into(),
            extract_ms,
            differing_files: None,
            differing_recordings: None,
            undiscovered: 0,
            discovery_ms: 0.0,
            overlay_files: None,
            overlay_differing: None,
            overlay_check_ms: None,
            examples: Vec::new(),
        };
        // `EDIT_GATE_TIMING_ONLY=1` skips every check, so a profile of the
        // run shows only the updates. Such a run proves nothing about
        // correctness; the example says so and exits as a failure.
        if timing_only {
            writeln!(
                out,
                "{}",
                serde_json::to_string(&record).map_err(|error| error.to_string())?
            )
            .map_err(|error| error.to_string())?;
            continue;
        }
        let started = Instant::now();
        let undiscovered = live.stale_recordings();
        record.discovery_ms = started.elapsed().as_secs_f64() * 1000.0;
        record.undiscovered = undiscovered.len();
        if !undiscovered.is_empty() {
            failures += 1;
            record.examples.extend(
                undiscovered
                    .iter()
                    .take(5)
                    .map(|path| format!("{path}: recorded answers no longer hold")),
            );
        }
        if last_of_commit || (index + 1) % check_every.max(1) == 0 {
            let cold =
                resolve_checkout(&live.inputs, threads).map_err(|error| error.to_string())?;
            let (answers, recordings) =
                compare(&live.resolved.to_map(), &cold, &mut record.examples);
            record.differing_files = Some(answers);
            record.differing_recordings = Some(recordings);
            checked += 1;
            // A different recording is a failure too: a stored resolution
            // with stale recorded inputs cannot be reused safely elsewhere.
            if answers > 0 || recordings > 0 {
                failures += 1;
            }
            if last_of_commit {
                let started = Instant::now();
                let (files_laid, differing) = check_overlay(
                    &segment,
                    &base_inputs,
                    &fragments,
                    &base,
                    &live,
                    &cold,
                    &mut record.examples,
                )?;
                record.overlay_files = Some(files_laid);
                record.overlay_differing = Some(differing);
                record.overlay_check_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
                if differing > 0 {
                    failures += 1;
                }
            }
        }
        writeln!(
            out,
            "{}",
            serde_json::to_string(&record).map_err(|error| error.to_string())?
        )
        .map_err(|error| error.to_string())?;
        // Investigating a failure needs only the first one.
        if failures > 0 && std::env::var_os("EDIT_GATE_STOP_AT_FIRST").is_some() {
            break;
        }
    }
    drop(segment);
    let _ = std::fs::remove_dir_all(&directory);
    Ok((checked, failures))
}

/// Lays every file whose parse or resolution changed since the base over
/// the base segment, and compares every store primitive over that overlay
/// with slices of the cold resolution `cold`. Returns how many files the
/// overlay holds and how many primitives answered differently.
fn check_overlay(
    segment: &Arc<Segment>,
    base_inputs: &CheckoutInputs,
    fragments: &BTreeMap<String, aft_store::graph::FragmentKey>,
    base: &BTreeMap<String, (String, Vec<u8>)>,
    live: &LiveCheckout,
    cold: &BTreeMap<String, FileResolution>,
    examples: &mut Vec<String>,
) -> Result<(usize, usize), String> {
    let mut overlay = OverlaySlices::new(StoredSlices::open(
        vec![Arc::clone(segment)],
        base_inputs,
        fragments,
    )?);
    for (path, (key, parse)) in &live.inputs.parses {
        let resolution = &live.resolved[path];
        let same = base.get(path).is_some_and(|(base_key, bytes)| {
            base_key == key && *bytes == super::fragment::encode(resolution)
        });
        if !same {
            overlay.replace(FileSlice::build(path, key, parse, resolution));
        }
    }
    for path in base.keys() {
        if !live.inputs.parses.contains_key(path) {
            overlay.remove(path);
        }
    }
    let laid = overlay.len();
    let root = std::path::PathBuf::from("/");
    let ours = SliceReader::new(overlay, root.clone(), "gate".to_string());
    let mut slices = MemorySlices::default();
    for (path, file) in cold {
        let (key, parse) = &live.inputs.parses[path];
        slices.insert(FileSlice::build(path, key, parse, file));
    }
    let theirs = SliceReader::new(slices, root, "gate".to_string());
    let mut queries = BTreeMap::new();
    let mut found = Vec::new();
    super::gate::compare_readers(&ours, &theirs, &live.inputs, &mut queries, &mut found);
    let failures = ours.source().base().failures();
    examples.extend(failures.iter().take(5).cloned());
    examples.extend(found.into_iter().take(20));
    let differing = queries
        .values()
        .filter(|(_, differing, _, _)| *differing > 0)
        .count()
        + failures.len();
    Ok((laid, differing))
}

/// One commit-end checkout resolved by reusing other checkouts' stored
/// resolutions, against its cold resolution.
#[derive(Debug, serde::Serialize)]
pub struct ReuseRecord {
    pub commit: usize,
    pub cold_ms: f64,
    #[serde(flatten)]
    pub stats: super::reuse::ReuseStats,
    /// Files whose rows, sites or dispatch targets differ from the cold
    /// resolution, and files whose recorded reads differ.
    pub differing_files: usize,
    pub differing_recordings: usize,
    pub examples: Vec<String>,
}

/// The cross-checkout gate. Builds the checkout at the end of every commit
/// of the pack and resolves each cold. Then resolves each again reusing
/// only resolutions other commits made, and compares with its cold
/// resolution: reuse must give exactly the same answers and recorded reads.
/// Writes one JSON line per commit to `out`; returns the commits checked
/// and how many differed.
///
/// # Errors
///
/// When the pack cannot be read or resolution fails.
pub fn run_reuse(
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
    let mut files = Files::default();
    for (path, text) in first["files"].as_object().ok_or("no base files")? {
        files.set(path, text.as_str().unwrap_or_default())?;
    }
    let mut edits = Vec::new();
    for line in lines {
        let line = line.map_err(|error| error.to_string())?;
        let value: serde_json::Value =
            serde_json::from_str(&line).map_err(|error| error.to_string())?;
        if value["type"] == "edit" {
            edits.push(value);
        }
    }
    // The checkout at the end of each commit, resolved cold. Each
    // resolution is kept as its fragment bytes, as the store keeps it, and
    // identical fragments are kept once, so the gate's memory does not grow
    // with every commit's decoded resolution.
    let mut checkouts = Vec::new();
    let mut cold_ms = Vec::new();
    // For each (record key, path): its distinct fragments, and which
    // commits resolved to each.
    let mut distinct = BTreeMap::<(String, String), Vec<(Vec<u8>, BTreeSet<usize>)>>::new();
    for (index, edit) in edits.iter().enumerate() {
        let path = edit["path"].as_str().ok_or("edit without path")?;
        if edit["kind"] == "delete" {
            files.entries.remove(path);
        } else {
            let text = edit["text"].as_str().ok_or("edit text is not a string")?;
            files.set(path, text)?;
        }
        let last_of_commit = edits
            .get(index + 1)
            .is_none_or(|next| next["commit"] != edit["commit"]);
        if last_of_commit {
            let commit = checkouts.len();
            let inputs = files.inputs();
            let started = Instant::now();
            let cold = resolve_checkout(&inputs, threads).map_err(|error| error.to_string())?;
            cold_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            for (path, file) in &cold {
                let key = inputs.parses[path].0.clone();
                let bytes = super::fragment::encode(file);
                let variants = distinct.entry((key, path.clone())).or_default();
                match variants.iter_mut().find(|(known, _)| *known == bytes) {
                    Some((_, sources)) => {
                        sources.insert(commit);
                    }
                    None => variants.push((bytes, BTreeSet::from([commit]))),
                }
            }
            checkouts.push(inputs);
        }
    }
    let decode = |key: &str, path: &str, bytes: &[u8]| {
        super::fragment::decode(key, path, bytes)
            .map_err(|error| format!("{path}: fragment does not decode: {error:?}"))
    };
    let mut failures = 0;
    for (commit, inputs) in checkouts.iter().enumerate() {
        // Only resolutions some other commit made, decoded for this commit
        // alone.
        let mut owned = Vec::<((String, String), Vec<FileResolution>)>::new();
        let mut cold = BTreeMap::new();
        for ((key, path), variants) in &distinct {
            let mut others = Vec::new();
            for (bytes, sources) in variants {
                if sources.contains(&commit) {
                    cold.insert(path.clone(), decode(key, path, bytes)?);
                }
                if sources.iter().any(|source| *source != commit) {
                    others.push(decode(key, path, bytes)?);
                }
            }
            if !others.is_empty() {
                owned.push(((key.clone(), path.clone()), others));
            }
        }
        let pool = owned
            .iter()
            .map(|((key, path), variants)| {
                (
                    (key.as_str(), path.as_str()),
                    variants.iter().collect::<Vec<_>>(),
                )
            })
            .collect();
        let (reused, stats) = super::reuse::resolve_reusing(inputs, &pool, threads)
            .map_err(|error| error.to_string())?;
        drop(pool);
        drop(owned);
        let mut record = ReuseRecord {
            commit,
            cold_ms: cold_ms[commit],
            stats,
            differing_files: 0,
            differing_recordings: 0,
            examples: Vec::new(),
        };
        // The cold side was decoded from fragment bytes, and a fragment does
        // not keep what is computed on load (the bound surface), so the
        // reused side goes through the same encoding before comparing.
        let reused = reused
            .into_iter()
            .map(|(path, file)| {
                let key = &inputs.parses[&path].0;
                let bytes = super::fragment::encode(&file);
                decode(key, &path, &bytes).map(|file| (path, file))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let (answers, recordings) = compare(&reused, &cold, &mut record.examples);
        record.differing_files = answers;
        record.differing_recordings = recordings;
        if answers > 0 || recordings > 0 {
            failures += 1;
        }
        writeln!(
            out,
            "{}",
            serde_json::to_string(&record).map_err(|error| error.to_string())?
        )
        .map_err(|error| error.to_string())?;
    }
    Ok((checkouts.len(), failures))
}
