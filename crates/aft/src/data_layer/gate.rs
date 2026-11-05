//! Checks the data layer's call-graph resolution against today's join on a
//! real checkout: the same inputs must give the same rows, bindings and
//! dispatch targets. Runs from `examples/resolve_gate.rs`; it goes away with
//! today's join.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aft_store::checkout::PathKind;
use aft_store::content::{ContentRecord, RecordBody, RecordKey};
use aft_store::ids::ContentKey;
use aft_store::scan::{scan, ContentKind, ContentSink};

use super::callgraph::{resolve_checkout, CheckoutInputs};
use super::content::{blob_from_record, extract, language_for, EXTRACTOR_VERSION};
use crate::callgraph_store::join::{
    join_selected_manifest_for_materialization, CallgraphBlob, DispatchTypeTargetPlan,
    ManifestBlobReader, ManifestJoinError,
};
use crate::views::{ByteString, Manifest, ManifestEntry, RegularPlanes, RelPath};

/// What the gate measured and found.
#[derive(Debug, Default)]
pub struct ResolveGateReport {
    pub files: usize,
    pub parsed_files: usize,
    pub config_files: usize,
    pub scan_and_extract_ms: f64,
    pub resolve_ms: f64,
    pub resolve_threads: usize,
    pub join_ms: f64,
    pub rows: usize,
    pub rows_only_new: usize,
    pub rows_only_join: usize,
    pub binding_mismatches: usize,
    pub dispatch_mismatches: usize,
    /// Assembled rows against the views database today's materialization
    /// writes from the same inputs, per table: (rows here, rows there,
    /// rows that differ).
    pub tables: BTreeMap<&'static str, (usize, usize, usize)>,
    pub assemble_ms: f64,
    pub materialize_ms: f64,
    /// Fragments that did not decode to the resolution they were encoded
    /// from (bound surfaces aside, which are not stored).
    pub fragment_mismatches: usize,
    /// Every file's fragment: encoded bytes, and the same compressed one
    /// fragment at a time.
    pub fragment_bytes: usize,
    pub fragment_zlib_bytes: usize,
    pub fragment_encode_ms: f64,
    pub fragment_decode_ms: f64,
    /// Per query primitive: (calls compared, calls that differ, ms over
    /// slices, ms over today's database).
    pub queries: BTreeMap<&'static str, (usize, usize, f64, f64)>,
    pub slices_ms: f64,
    /// The same queries over slices written to a segment and read back.
    pub segment_queries: BTreeMap<&'static str, (usize, usize, f64, f64)>,
    pub segment_write_ms: f64,
    pub segment_bytes: u64,
    /// Reads from the segment that failed.
    pub segment_failures: usize,
    /// Dead-code snapshot parts (files, exports, outbound calls, entry
    /// points, entry-point symbols, Go initializers) that differ from
    /// today's projection of the database.
    pub dead_code_differing: Vec<String>,
    pub dead_code_ms: f64,
    pub dead_code_today_ms: f64,
    /// Up to twenty paths whose results differ, for a first look.
    pub examples: Vec<String>,
}

#[derive(Default)]
struct Records {
    by_key: Mutex<HashMap<RecordKey, Arc<ContentRecord>>>,
    by_path: Mutex<Vec<(String, RecordKey)>>,
    failures: Mutex<Vec<String>>,
}

impl ContentSink for Records {
    fn read(&self, path: &[u8], content: ContentKey, kind: ContentKind, bytes: &[u8]) {
        if kind == ContentKind::Binary {
            return;
        }
        let Ok(path) = std::str::from_utf8(path) else {
            return;
        };
        let Some(language) = language_for(path, bytes) else {
            return;
        };
        let key = RecordKey::new(content, &language, EXTRACTOR_VERSION);
        let known = self
            .by_key
            .lock()
            .expect("no panics while holding it")
            .contains_key(&key);
        if !known {
            match extract(bytes, &language) {
                Ok(record) => {
                    self.by_key
                        .lock()
                        .expect("no panics while holding it")
                        .insert(key, Arc::new(record));
                }
                Err(error) => {
                    self.failures
                        .lock()
                        .expect("no panics while holding it")
                        .push(format!("{path}: {error}"));
                    return;
                }
            }
        }
        self.by_path
            .lock()
            .expect("no panics while holding it")
            .push((path.to_string(), key));
    }
}

struct Blobs(HashMap<String, Arc<CallgraphBlob>>);

impl ManifestBlobReader for Blobs {
    fn read_callgraph_blob(&self, full_key: &str) -> Result<Option<Vec<u8>>, ManifestJoinError> {
        self.0.get(full_key).map(|blob| blob.to_bytes()).transpose()
    }

    fn read_callgraph_blob_decoded(
        &self,
        full_key: &str,
    ) -> Result<Option<Arc<CallgraphBlob>>, ManifestJoinError> {
        Ok(self.0.get(full_key).cloned())
    }
}

/// Resolves the checkout at `root` both ways and compares the results.
///
/// # Errors
///
/// When the checkout cannot be scanned or either resolution fails.
pub fn run(root: &Path, threads: usize) -> Result<ResolveGateReport, String> {
    let started = Instant::now();
    let records = Records::default();
    // Today's views read no file past the text-index limit, while the data
    // layer parses source files up to the larger read limit. This gate
    // compares resolution and assembly on the same inputs, so it scans with
    // today's limit; the larger files are the data layer's own improvement,
    // and the checkout and edit gates cover them against cold builds.
    let policy = aft_store::scan::ScanPolicy {
        read_large: None,
        ..super::content::scan_policy()
    };
    let scanned = scan(root, &policy, None, &records).map_err(|error| error.to_string())?;
    let failures = records.failures.into_inner().expect("no panics");
    if !failures.is_empty() {
        return Err(format!("extraction failed: {failures:?}"));
    }
    let by_key = records.by_key.into_inner().expect("no panics");
    let by_path = records.by_path.into_inner().expect("no panics");
    let blobs = by_key
        .iter()
        .map(|(key, record)| {
            blob_from_record(record)
                .map(|blob| (key.digest().to_string(), Arc::new(blob)))
                .map_err(|error| error.to_string())
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    let mut inputs = CheckoutInputs {
        state: scanned.state,
        parses: super::callgraph::Parses::default(),
        configs: HashMap::new(),
    };
    let mut keys_by_path = HashMap::new();
    for (path, key) in by_path {
        let full_key = key.digest().to_string();
        match &by_key[&key].data {
            RecordBody::Parsed { .. } => {
                let CallgraphBlob::Parse(parse) = blobs[&full_key].as_ref() else {
                    unreachable!("a parsed record converts to a parse blob");
                };
                inputs
                    .parses
                    .insert(path.clone(), (full_key.clone(), Arc::new(parse.clone())));
            }
            RecordBody::Config { source } => {
                inputs
                    .configs
                    .insert(path.as_bytes().to_vec(), Arc::from(source.as_slice()));
            }
        }
        keys_by_path.insert(path, full_key);
    }
    let mut report = ResolveGateReport {
        files: inputs.state.paths().len(),
        parsed_files: inputs.parses.len(),
        config_files: inputs.configs.len(),
        scan_and_extract_ms: started.elapsed().as_secs_f64() * 1000.0,
        resolve_threads: threads,
        ..ResolveGateReport::default()
    };

    let started = Instant::now();
    let resolved = resolve_checkout(&inputs, threads).map_err(|error| error.to_string())?;
    report.resolve_ms = started.elapsed().as_secs_f64() * 1000.0;

    let manifest = Manifest::new(inputs.state.paths().iter().map(|record| {
        let path = RelPath::new(record.path.clone()).expect("a checkout path is relative");
        let entry = if record.kind == PathKind::Symlink {
            ManifestEntry::Symlink {
                target_bytes: ByteString::new(record.link.clone()),
            }
        } else {
            let text = std::str::from_utf8(&record.path).unwrap_or_default();
            ManifestEntry::Regular {
                mode: record.mode,
                planes: RegularPlanes {
                    semantic: None,
                    callgraph: keys_by_path.get(text).cloned(),
                },
                resolution_input: crate::callgraph_extractor::is_resolution_input(&record.path),
            }
        };
        (path, entry)
    }))
    .map_err(|error| error.to_string())?;
    let blobs_again = Blobs(blobs.clone());
    let started = Instant::now();
    let join = join_selected_manifest_for_materialization(
        &manifest,
        &Blobs(blobs),
        None,
        &BTreeMap::new(),
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeSet::new(),
        Some(DispatchTypeTargetPlan::All),
    )
    .map_err(|error| error.to_string())?;
    report.join_ms = started.elapsed().as_secs_f64() * 1000.0;

    let new_rows = resolved
        .values()
        .flat_map(|file| file.rows.iter().cloned())
        .collect::<BTreeSet<_>>();
    report.rows = new_rows.len();
    report.rows_only_new = new_rows.difference(&join.result.rows).count();
    report.rows_only_join = join.result.rows.difference(&new_rows).count();
    for row in new_rows.symmetric_difference(&join.result.rows).take(5) {
        report.examples.push(format!(
            "row {} #{}",
            String::from_utf8_lossy(&row.caller_path),
            row.ref_ordinal
        ));
    }
    for (path, file) in &resolved {
        // The data layer also records every checkout read with its answer;
        // today's join records none, so those are left out of the comparison.
        let binding = file.binding.without_answers();
        let dispatch = file.dispatch.as_ref().map(|dispatch| {
            let mut dispatch = dispatch.clone();
            dispatch.trace.answers.clear();
            dispatch
        });
        if join.bindings.get(path) != Some(&binding) {
            report.binding_mismatches += 1;
            if report.examples.len() < 20 {
                report.examples.push(format!("binding {path}"));
                // The lines of the two recordings' debug forms that differ.
                let ours = format!("{:#?}", file.binding);
                let theirs = format!("{:#?}", join.bindings.get(path));
                let ours_lines = ours.lines().collect::<BTreeSet<_>>();
                let theirs_lines = theirs.lines().collect::<BTreeSet<_>>();
                for line in ours_lines.difference(&theirs_lines).take(8) {
                    report.examples.push(format!("  ours:  {}", line.trim()));
                }
                for line in theirs_lines.difference(&ours_lines).take(8) {
                    report.examples.push(format!("  join:  {}", line.trim()));
                }
            }
        }
        if join.dispatch_type_targets.get(path) != dispatch.as_ref() {
            report.dispatch_mismatches += 1;
            if report.examples.len() < 20 {
                report.examples.push(format!("dispatch {path}"));
            }
        }
    }
    let unresolved_by_new = join
        .bindings
        .keys()
        .filter(|path| !resolved.contains_key(*path))
        .count();
    report.binding_mismatches += unresolved_by_new;

    for (path, file) in &resolved {
        let started = Instant::now();
        let bytes = super::fragment::encode(file);
        report.fragment_encode_ms += started.elapsed().as_secs_f64() * 1000.0;
        report.fragment_bytes += bytes.len();
        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut zlib, &bytes).map_err(|error| error.to_string())?;
        report.fragment_zlib_bytes += zlib.finish().map_err(|error| error.to_string())?.len();
        let key = &inputs.parses[path].0;
        let started = Instant::now();
        let decoded =
            super::fragment::decode(key, path, &bytes).map_err(|error| error.0.to_string())?;
        report.fragment_decode_ms += started.elapsed().as_secs_f64() * 1000.0;
        let expected = super::callgraph::FileResolution {
            rows: file.rows.clone(),
            binding: file.binding.without_surface(),
            dispatch: file.dispatch.clone(),
            sites: file.sites.clone(),
            site_dependencies: file.site_dependencies.clone(),
        };
        if decoded != expected {
            report.fragment_mismatches += 1;
            if report.examples.len() < 30 {
                report.examples.push(format!("fragment {path}"));
            }
        }
    }

    let started = Instant::now();
    let rows = super::graph_rows::assemble(&inputs, &resolved);
    report.assemble_ms = started.elapsed().as_secs_f64() * 1000.0;
    let database = std::env::temp_dir().join(format!("aft-resolve-gate-{}", std::process::id()));
    std::fs::create_dir_all(&database).map_err(|error| error.to_string())?;
    let path = database.join("view.sqlite");
    let started = Instant::now();
    crate::views::materialization::materialize_from_blob_reader(&path, &manifest, &blobs_again)
        .map_err(|error| error.to_string())?;
    report.materialize_ms = started.elapsed().as_secs_f64() * 1000.0;
    let connection = rusqlite::Connection::open(&path).map_err(|error| error.to_string())?;
    compare_tables(&connection, &rows, &mut report).map_err(|error| error.to_string())?;
    drop(connection);
    compare_dead_code(root, &rows, &path, &mut report)?;
    let records = by_key
        .iter()
        .map(|(key, record)| (key.digest().to_string(), (*key, Arc::clone(record))))
        .collect::<HashMap<_, _>>();
    compare_queries(root, &inputs, &resolved, &records, &path, &mut report)?;
    let _ = std::fs::remove_dir_all(&database);
    Ok(report)
}

/// Builds the dead-code snapshot from the rows and projects today's from the
/// database, and records which parts differ.
fn compare_dead_code(
    root: &Path,
    rows: &super::graph_rows::GraphRows,
    database: &Path,
    report: &mut ResolveGateReport,
) -> Result<(), String> {
    let started = Instant::now();
    let ours = super::dead_code::snapshot(root, rows);
    report.dead_code_ms = started.elapsed().as_secs_f64() * 1000.0;
    let store = crate::callgraph_store::ReadonlyCallGraphStore::open_materialized_database(
        root.to_path_buf(),
        database.to_path_buf(),
    )
    .map_err(|error| error.to_string())?;
    let started = Instant::now();
    let (_, theirs, _, _) =
        crate::callgraph_store::dead_code_projection::project_dead_code_snapshot_from_view(&store)
            .map_err(|error| error.to_string())?;
    report.dead_code_today_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut part = |name: &str, left: String, right: String| {
        if left != right {
            report.dead_code_differing.push(name.to_string());
            let left = left.lines().collect::<BTreeSet<_>>();
            let right = right.lines().collect::<BTreeSet<_>>();
            for line in left.difference(&right).take(4) {
                report
                    .examples
                    .push(format!("dead code {name} ours:  {}", line.trim()));
            }
            for line in right.difference(&left).take(4) {
                report
                    .examples
                    .push(format!("dead code {name} today: {}", line.trim()));
            }
            if left == right {
                report
                    .examples
                    .push(format!("dead code {name}: same rows, different order"));
            }
        }
    };
    part(
        "files",
        format!("{:#?}", ours.files),
        format!("{:#?}", theirs.files),
    );
    part(
        "exported_symbols",
        format!("{:#?}", ours.exported_symbols),
        format!("{:#?}", theirs.exported_symbols),
    );
    part(
        "outbound_calls",
        format!("{:#?}", ours.outbound_calls),
        format!("{:#?}", theirs.outbound_calls),
    );
    part(
        "entry_points",
        format!("{:#?}", ours.entry_points),
        format!("{:#?}", theirs.entry_points),
    );
    part(
        "entry_point_symbols",
        format!("{:#?}", ours.entry_point_symbols),
        format!("{:#?}", theirs.entry_point_symbols),
    );
    part(
        "go_initializers",
        format!("{:#?}", ours.go_initializers),
        format!("{:#?}", theirs.go_initializers),
    );
    Ok(())
}

/// Runs every store primitive the commands use over slices and over today's
/// database, for a sample of symbols and names, and records differences.
///
/// `GATE_SAMPLE_SCALE=N` samples N times more sparsely, for iterating; the
/// default (1) is the sample a milestone gate uses.
fn compare_queries(
    root: &Path,
    inputs: &super::callgraph::CheckoutInputs,
    resolved: &BTreeMap<String, super::callgraph::FileResolution>,
    records: &HashMap<String, (RecordKey, Arc<ContentRecord>)>,
    database: &Path,
    report: &mut ResolveGateReport,
) -> Result<(), String> {
    use aft_store::cache::BlockCache;
    use aft_store::ids::{FamilyId, SegmentId, WriterIncarnation};
    use aft_store::segment::{Segment, SegmentMeta};

    let started = Instant::now();
    let mut slices = super::query::MemorySlices::default();
    for (path, file) in resolved {
        let (key, parse) = &inputs.parses[path];
        slices.insert(super::slice::FileSlice::build(path, key, parse, file));
    }
    let ours = super::query::SliceReader::new(slices, root.to_path_buf(), "gate".to_string());
    report.slices_ms = started.elapsed().as_secs_f64() * 1000.0;
    let theirs = crate::callgraph_store::ReadonlyCallGraphStore::open_materialized_database(
        root.to_path_buf(),
        database.to_path_buf(),
    )
    .map_err(|error| error.to_string())?;

    let mut examples = Vec::new();
    compare_readers(&ours, &theirs, inputs, &mut report.queries, &mut examples);

    // The same queries over slices written to a segment and read back.
    let directory =
        std::env::temp_dir().join(format!("aft-resolve-gate-segment-{}", std::process::id()));
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let segment_path = directory.join("1.seg");
    let family = FamilyId::from_digest(aft_store::ids::Digest128::of(b"resolve gate"));
    let started = Instant::now();
    let fragments = super::graph_store::write_segment(
        &segment_path,
        SegmentMeta {
            family,
            segment: SegmentId::new(1),
            incarnation: WriterIncarnation::new(1),
            created_unix_ms: 0,
        },
        inputs,
        resolved,
        records,
    )?;
    report.segment_write_ms = started.elapsed().as_secs_f64() * 1000.0;
    report.segment_bytes = std::fs::metadata(&segment_path)
        .map_err(|error| error.to_string())?
        .len();
    let segment = Segment::open(
        &segment_path,
        family,
        &super::graph_store::known_sections(),
        Arc::new(BlockCache::new(256 << 20)),
    )
    .map_err(|error| error.to_string())?;
    let segment = Arc::new(segment);
    let stored =
        super::graph_store::StoredSlices::open(vec![Arc::clone(&segment)], inputs, &fragments)?;
    let stored = super::query::SliceReader::new(stored, root.to_path_buf(), "gate".to_string());
    compare_readers(
        &stored,
        &theirs,
        inputs,
        &mut report.segment_queries,
        &mut examples,
    );
    let failures = stored.source().failures();
    report.segment_failures = failures.len();
    examples.extend(failures.into_iter().take(10));
    drop(stored);
    drop(segment);
    let _ = std::fs::remove_dir_all(&directory);
    report.examples.extend(examples);
    eprintln!("query examples: {:#?}", report.examples);
    Ok(())
}

/// Runs every store primitive the commands use on `ours` and on `theirs`,
/// for a sample of symbols and names, and records each primitive's calls,
/// differences and time in `queries`.
pub(crate) fn compare_readers(
    ours: &impl crate::callgraph_store::CallGraphRead,
    theirs: &impl crate::callgraph_store::CallGraphRead,
    inputs: &super::callgraph::CheckoutInputs,
    queries: &mut BTreeMap<&'static str, (usize, usize, f64, f64)>,
    examples_out: &mut Vec<String>,
) {
    use crate::parser::LangId;

    let mut examples = std::mem::take(examples_out);
    let mut compare = |name: &'static str,
                       ours_run: &mut dyn FnMut() -> String,
                       theirs_run: &mut dyn FnMut() -> String,
                       examples: &mut Vec<String>| {
        let started = Instant::now();
        let left = ours_run();
        let ours_ms = started.elapsed().as_secs_f64() * 1000.0;
        let started = Instant::now();
        let right = theirs_run();
        let theirs_ms = started.elapsed().as_secs_f64() * 1000.0;
        let entry = queries.entry(name).or_default();
        entry.0 += 1;
        entry.2 += ours_ms;
        entry.3 += theirs_ms;
        if left != right {
            entry.1 += 1;
            if examples.len() < 60 && entry.1 <= 3 {
                let left_lines = left.lines().collect::<BTreeSet<_>>();
                let right_lines = right.lines().collect::<BTreeSet<_>>();
                examples.push(format!("query {name} differs"));
                for line in left_lines.difference(&right_lines).take(4) {
                    examples.push(format!("  slices: {}", line.trim()));
                }
                for line in right_lines.difference(&left_lines).take(4) {
                    examples.push(format!("  views:  {}", line.trim()));
                }
            }
        }
    };
    let show = |value: &dyn std::fmt::Debug| format!("{value:#?}");

    // A deterministic sample of symbols: every 20th node in path order.
    let mut symbols = Vec::new();
    for (path, (_, parse)) in &inputs.parses {
        for symbol in &parse.symbols {
            symbols.push((
                path.clone(),
                symbol.scoped_name.clone(),
                symbol.name.clone(),
            ));
        }
    }
    let sample = symbols
        .iter()
        .step_by(20 * sample_scale())
        .cloned()
        .collect::<Vec<_>>();
    for chunk in sample.chunks(200) {
        let targets = chunk
            .iter()
            .map(|(file, scoped, _)| (file.clone(), scoped.clone()))
            .collect::<Vec<_>>();
        let sorted = |map: std::collections::HashMap<
            (String, String),
            Vec<crate::callgraph_store::StoreCallSite>,
        >| { map.into_iter().collect::<BTreeMap<_, _>>() };
        compare(
            "direct_callers_for_symbols",
            &mut || {
                show(
                    &ours
                        .direct_callers_for_symbols(&targets)
                        .map(sorted)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut || {
                show(
                    &theirs
                        .direct_callers_for_symbols(&targets)
                        .map(sorted)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut examples,
        );
        compare(
            "direct_caller_counts_of",
            &mut || {
                show(
                    &ours
                        .direct_caller_counts_of(&targets)
                        .map(|m| m.into_iter().collect::<BTreeMap<_, _>>())
                        .map_err(|e| e.to_string()),
                )
            },
            &mut || {
                show(
                    &theirs
                        .direct_caller_counts_of(&targets)
                        .map(|m| m.into_iter().collect::<BTreeMap<_, _>>())
                        .map_err(|e| e.to_string()),
                )
            },
            &mut examples,
        );
        compare(
            "outgoing_calls_for_symbols",
            &mut || {
                show(
                    &ours
                        .outgoing_calls_for_symbols(&targets)
                        .map(sorted)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut || {
                show(
                    &theirs
                        .outgoing_calls_for_symbols(&targets)
                        .map(sorted)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut examples,
        );
    }
    for (file, scoped, name) in &sample {
        for query in [scoped, name] {
            compare(
                "nodes_for_stored",
                &mut || {
                    show(
                        &ours
                            .nodes_for_stored(file, query)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut || {
                    show(
                        &theirs
                            .nodes_for_stored(file, query)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut examples,
            );
        }
        compare(
            "nodes_matching",
            &mut || show(&ours.nodes_matching(name).map_err(|e| e.to_string())),
            &mut || show(&theirs.nodes_matching(name).map_err(|e| e.to_string())),
            &mut examples,
        );
        compare(
            "trace_to_symbol_candidates",
            &mut || {
                show(
                    &ours
                        .trace_to_symbol_candidates(scoped)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut || {
                show(
                    &theirs
                        .trace_to_symbol_candidates(scoped)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut examples,
        );
        compare(
            "node_for",
            &mut || {
                show(
                    &ours
                        .node_for(Path::new(file), scoped)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut || {
                show(
                    &theirs
                        .node_for(Path::new(file), scoped)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut examples,
        );
        compare(
            "stored_content_hash",
            &mut || show(&ours.stored_content_hash(file).map_err(|e| e.to_string())),
            &mut || show(&theirs.stored_content_hash(file).map_err(|e| e.to_string())),
            &mut examples,
        );
        let Ok(nodes) = theirs.nodes_for_stored(file, scoped) else {
            continue;
        };
        for node in &nodes {
            compare(
                "outgoing_calls_of",
                &mut || show(&ours.outgoing_calls_of(node).map_err(|e| e.to_string())),
                &mut || show(&theirs.outgoing_calls_of(node).map_err(|e| e.to_string())),
                &mut examples,
            );
            compare(
                "resolved_self_calls_of",
                &mut || show(&ours.resolved_self_calls_of(node).map_err(|e| e.to_string())),
                &mut || {
                    show(
                        &theirs
                            .resolved_self_calls_of(node)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut examples,
            );
            compare(
                "unresolved_calls_of",
                &mut || show(&ours.unresolved_calls_of(node).map_err(|e| e.to_string())),
                &mut || show(&theirs.unresolved_calls_of(node).map_err(|e| e.to_string())),
                &mut examples,
            );
        }
    }

    // Names used by references: every 50th distinct short name.
    let mut names = BTreeSet::new();
    for (_, parse) in inputs.parses.values() {
        for reference in &parse.refs {
            if let Some(name) = &reference.short_name {
                names.insert(name.clone());
            }
        }
    }
    let languages = [
        LangId::Rust,
        LangId::TypeScript,
        LangId::JavaScript,
        LangId::Python,
        LangId::Go,
    ];
    for name in names.iter().step_by(50 * sample_scale()) {
        for kind in ["call", "construction", "value_ref"] {
            compare(
                "ref_sites_named",
                &mut || {
                    show(
                        &ours
                            .ref_sites_named(kind, name, 50)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut || {
                    show(
                        &theirs
                            .ref_sites_named(kind, name, 50)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut examples,
            );
        }
        for language in languages {
            compare(
                "unresolved_method_sites_named",
                &mut || {
                    show(
                        &ours
                            .unresolved_method_sites_named(name, language)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut || {
                    show(
                        &theirs
                            .unresolved_method_sites_named(name, language)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut examples,
            );
            for include_tests in [false, true] {
                compare(
                    "unresolved_call_site_count_named",
                    &mut || {
                        show(
                            &ours
                                .unresolved_call_site_count_named(name, language, include_tests)
                                .map_err(|e| e.to_string()),
                        )
                    },
                    &mut || {
                        show(
                            &theirs
                                .unresolved_call_site_count_named(name, language, include_tests)
                                .map_err(|e| e.to_string()),
                        )
                    },
                    &mut examples,
                );
            }
        }
    }
    for language in languages {
        compare(
            "unresolved_method_sites_named (all)",
            &mut || {
                show(
                    &ours
                        .unresolved_method_sites_named("", language)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut || {
                show(
                    &theirs
                        .unresolved_method_sites_named("", language)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut examples,
        );
        for include_tests in [false, true] {
            compare(
                "unresolved_call_site_count",
                &mut || {
                    show(
                        &ours
                            .unresolved_call_site_count(language, include_tests)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut || {
                    show(
                        &theirs
                            .unresolved_call_site_count(language, include_tests)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut examples,
            );
        }
    }
    compare(
        "indexed_file_count",
        &mut || show(&ours.indexed_file_count().map_err(|e| e.to_string())),
        &mut || show(&theirs.indexed_file_count().map_err(|e| e.to_string())),
        &mut examples,
    );
    // Every edge, one per line, so a difference names the edges that differ
    // rather than only a count.
    let edges = |snapshot: crate::callgraph_store::Result<
        BTreeSet<crate::callgraph_store::StoredEdge>,
    >| match snapshot {
        Ok(edges) => edges
            .iter()
            .map(|edge| format!("{edge:?}"))
            .collect::<Vec<_>>()
            .join("\n"),
        Err(error) => format!("error: {error}"),
    };
    compare(
        "edge_snapshot",
        &mut || edges(ours.edge_snapshot()),
        &mut || edges(theirs.edge_snapshot()),
        &mut examples,
    );

    // The public operations, as the commands compose them from the
    // primitives above: what an agent's call-graph reply is built from.
    // Every 400th symbol in path order; each operation runs at the depth
    // the commands default to and with tests both left out and included.
    use crate::commands::callgraph_store_adapter as adapter;
    let public = symbols
        .iter()
        .step_by(400 * sample_scale())
        .cloned()
        .collect::<Vec<_>>();
    for (index, (file, scoped, _)) in public.iter().enumerate() {
        let path = Path::new(file);
        for include_tests in [false, true] {
            compare(
                "callers (public)",
                &mut || {
                    show(&adapter::callers_result(
                        ours,
                        path,
                        scoped,
                        2,
                        include_tests,
                    ))
                },
                &mut || {
                    show(&adapter::callers_result(
                        theirs,
                        path,
                        scoped,
                        2,
                        include_tests,
                    ))
                },
                &mut examples,
            );
            compare(
                "impact (public)",
                &mut || {
                    show(&adapter::impact_result(
                        ours,
                        path,
                        scoped,
                        2,
                        include_tests,
                    ))
                },
                &mut || {
                    show(&adapter::impact_result(
                        theirs,
                        path,
                        scoped,
                        2,
                        include_tests,
                    ))
                },
                &mut examples,
            );
            compare(
                "call_tree (public)",
                &mut || {
                    show(&adapter::call_tree_result(
                        ours,
                        path,
                        scoped,
                        2,
                        include_tests,
                    ))
                },
                &mut || {
                    show(&adapter::call_tree_result(
                        theirs,
                        path,
                        scoped,
                        2,
                        include_tests,
                    ))
                },
                &mut examples,
            );
            compare(
                "trace_to (public)",
                &mut || {
                    show(&adapter::trace_to_result(
                        ours,
                        path,
                        scoped,
                        4,
                        include_tests,
                    ))
                },
                &mut || {
                    show(&adapter::trace_to_result(
                        theirs,
                        path,
                        scoped,
                        4,
                        include_tests,
                    ))
                },
                &mut examples,
            );
        }
        // From this symbol to the next sampled one, named with its file.
        if let Some((to_file, to_scoped, _)) = public.get(index + 1) {
            let to_path = Path::new(to_file);
            compare(
                "trace_to_symbol (public)",
                &mut || {
                    show(&adapter::trace_to_symbol_result(
                        ours,
                        path,
                        scoped,
                        to_scoped,
                        Some(to_path),
                        4,
                        false,
                    ))
                },
                &mut || {
                    show(&adapter::trace_to_symbol_result(
                        theirs,
                        path,
                        scoped,
                        to_scoped,
                        Some(to_path),
                        4,
                        false,
                    ))
                },
                &mut examples,
            );
        }
    }
    *examples_out = examples;
}

/// Every row as one string of its compared columns, so two tables compare
/// as sets of strings.
fn table(connection: &rusqlite::Connection, sql: &str) -> rusqlite::Result<BTreeSet<String>> {
    let mut statement = connection.prepare(sql)?;
    let columns = statement.column_count();
    let rows = statement
        .query_map([], |row| {
            (0..columns)
                .map(|index| {
                    Ok(match row.get_ref(index)? {
                        rusqlite::types::ValueRef::Null => "NULL".to_string(),
                        rusqlite::types::ValueRef::Integer(value) => value.to_string(),
                        rusqlite::types::ValueRef::Real(value) => value.to_string(),
                        rusqlite::types::ValueRef::Text(value)
                        | rusqlite::types::ValueRef::Blob(value) => {
                            String::from_utf8_lossy(value).into_owned()
                        }
                    })
                })
                .collect::<rusqlite::Result<Vec<_>>>()
                .map(|fields| fields.join("\u{1f}"))
        })?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    Ok(rows)
}

fn opt(value: Option<&str>) -> String {
    value.map_or_else(|| "NULL".to_string(), str::to_string)
}

fn join_fields(fields: &[String]) -> String {
    fields.join("\u{1f}")
}

fn compare_tables(
    connection: &rusqlite::Connection,
    rows: &super::graph_rows::GraphRows,
    report: &mut ResolveGateReport,
) -> rusqlite::Result<()> {
    let mut record = |name: &'static str, ours: BTreeSet<String>, theirs: BTreeSet<String>| {
        let differing = ours.symmetric_difference(&theirs).count();
        for row in ours.symmetric_difference(&theirs).take(3) {
            if report.examples.len() < 40 {
                let side = if ours.contains(row) { "ours" } else { "views" };
                report
                    .examples
                    .push(format!("{name} {side}: {}", row.replace('\u{1f}', " | ")));
            }
        }
        report
            .tables
            .insert(name, (ours.len(), theirs.len(), differing));
    };
    let b = |value: bool| i64::from(value).to_string();
    record(
        "files",
        rows.files
            .iter()
            .map(|(path, (key, language))| {
                join_fields(&[path.clone(), key.clone(), language.clone()])
            })
            .collect(),
        table(connection, "SELECT path, content_hash, lang FROM files")?,
    );
    record(
        "nodes",
        rows.nodes
            .values()
            .map(|node| {
                join_fields(&[
                    node.id.clone(),
                    node.file_path.clone(),
                    node.name.clone(),
                    node.scoped_name.clone(),
                    node.kind.clone(),
                    node.start_line.to_string(),
                    node.start_col.to_string(),
                    node.end_line.to_string(),
                    node.end_col.to_string(),
                    node.range_ordinal.to_string(),
                    opt(node.signature.as_deref()),
                    b(node.exported),
                    b(node.is_default_export),
                    b(node.entry_point),
                    node.is_test.map_or_else(|| "NULL".to_string(), b),
                ])
            })
            .collect(),
        table(
            connection,
            "SELECT id, file_path, name, scoped_name, kind, start_line, start_col, end_line,
                    end_col, range_ordinal, signature, exported, is_default_export,
                    is_callgraph_entry_point, is_test FROM nodes",
        )?,
    );
    record(
        "refs",
        rows.refs
            .values()
            .map(|reference| {
                join_fields(&[
                    reference.ref_id.clone(),
                    opt(reference.caller_node.as_deref()),
                    reference.caller_file.clone(),
                    reference.kind.to_string(),
                    opt(reference.short_name.as_deref()),
                    opt(reference.full_ref.as_deref()),
                    opt(reference.module_path.as_deref()),
                    opt(reference.import_kind.as_deref()),
                    opt(reference.local_name.as_deref()),
                    opt(reference.requested_name.as_deref()),
                    opt(reference.namespace_alias.as_deref()),
                    b(reference.wildcard),
                    reference.line.to_string(),
                    reference.byte_start.to_string(),
                    reference.byte_end.to_string(),
                    reference.status.to_string(),
                    opt(reference.target_node.as_deref()),
                    opt(reference.target_file.as_deref()),
                    opt(reference.target_symbol.as_deref()),
                ])
            })
            .collect(),
        table(
            connection,
            "SELECT ref_id, caller_node, caller_file, kind, short_name, full_ref, module_path,
                    import_kind, local_name, requested_name, namespace_alias, wildcard, line,
                    byte_start, byte_end, status, target_node, target_file, target_symbol
             FROM refs",
        )?,
    );
    record(
        "edges",
        rows.edges
            .values()
            .map(|edge| {
                join_fields(&[
                    edge.edge_id.clone(),
                    edge.ref_id.clone(),
                    edge.source_node.clone(),
                    opt(edge.target_node.as_deref()),
                    edge.target_file.clone(),
                    edge.target_symbol.clone(),
                    edge.kind.to_string(),
                    edge.line.to_string(),
                    edge.provenance.clone(),
                ])
            })
            .collect(),
        table(
            connection,
            "SELECT edge_id, ref_id, source_node, target_node, target_file, target_symbol,
                    kind, line, provenance FROM edges",
        )?,
    );
    record(
        "view_dispatch_sites",
        rows.sites
            .values()
            .map(|site| {
                join_fields(&[
                    site.caller_file.clone(),
                    site.ordinal.to_string(),
                    site.language.clone(),
                    site.unresolved.to_string(),
                    site.dynamic.to_string(),
                    site.external.to_string(),
                ])
            })
            .collect(),
        table(
            connection,
            "SELECT caller_file, ordinal, language, unresolved_receiver_sites,
                    dynamic_member_sites, external FROM view_dispatch_sites",
        )?,
    );
    record(
        "view_unknown_live",
        rows.unknown_live
            .iter()
            .map(|(caller, file, symbol)| {
                join_fields(&[caller.clone(), file.clone(), symbol.clone()])
            })
            .collect(),
        table(
            connection,
            "SELECT caller_file, target_file, target_symbol FROM view_unknown_live",
        )?,
    );
    Ok(())
}

/// How much more sparsely than the default the query comparison samples.
fn sample_scale() -> usize {
    std::env::var("GATE_SAMPLE_SCALE")
        .ok()
        .and_then(|scale| scale.parse::<usize>().ok())
        .filter(|scale| *scale > 0)
        .unwrap_or(1)
}
