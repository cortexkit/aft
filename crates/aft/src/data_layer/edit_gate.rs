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
struct Files {
    entries: BTreeMap<String, (ContentKey, String, Option<Parsed>)>,
    records: HashMap<RecordKey, Option<Parsed>>,
    /// Every parsed record, by the key its parse carries.
    contents: HashMap<String, (RecordKey, Arc<ContentRecord>)>,
}

impl Files {
    fn set(&mut self, path: &str, text: &str) -> Result<(), String> {
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

    fn inputs(&self) -> CheckoutInputs {
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
            parses: BTreeMap::new(),
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

/// Replays the pack at `pack`, checking every `check_every`-th edit and
/// every commit's last edit against a cold resolution. Writes one JSON line
/// per edit to `out`.
///
/// # Errors
///
/// When the pack cannot be read or resolution fails.
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
    let mut live =
        LiveCheckout::cold(files.inputs(), threads).map_err(|error| error.to_string())?;
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
        &live.resolved,
        &files.contents,
    )?;
    let segment = Segment::open(
        &segment_path,
        family,
        &known_sections(),
        Arc::new(BlockCache::new(256 << 20)),
    )
    .map_err(|error| error.to_string())?;
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
    let mut failures = 0;
    let mut checked = 0;
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
            overlay_files: None,
            overlay_differing: None,
            overlay_check_ms: None,
            examples: Vec::new(),
        };
        if last_of_commit || (index + 1) % check_every.max(1) == 0 {
            let cold =
                resolve_checkout(&live.inputs, threads).map_err(|error| error.to_string())?;
            let (answers, recordings) = compare(&live.resolved, &cold, &mut record.examples);
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
    segment: &Segment,
    base_inputs: &CheckoutInputs,
    fragments: &BTreeMap<String, aft_store::graph::FragmentKey>,
    base: &BTreeMap<String, (String, Vec<u8>)>,
    live: &LiveCheckout,
    cold: &BTreeMap<String, FileResolution>,
    examples: &mut Vec<String>,
) -> Result<(usize, usize), String> {
    let mut overlay = OverlaySlices::new(StoredSlices::open(segment, base_inputs, fragments)?);
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
