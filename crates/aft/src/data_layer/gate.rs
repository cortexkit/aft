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
use aft_store::scan::{scan, ContentKind, ContentSink, ScanPolicy};

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
        if kind != ContentKind::Text {
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
    let scanned =
        scan(root, &ScanPolicy::default(), None, &records).map_err(|error| error.to_string())?;
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
        parses: BTreeMap::new(),
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
                resolution_input: crate::views::assembly::is_resolution_input(&record.path),
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
        if join.bindings.get(path) != Some(&file.binding) {
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
        if join.dispatch_type_targets.get(path) != file.dispatch.as_ref() {
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
    let _ = std::fs::remove_dir_all(&database);
    Ok(report)
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
