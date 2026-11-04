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
use aft_store::scan::{ContentKind, ContentSink, ScanPolicy, scan};

use super::callgraph::{CheckoutInputs, resolve_checkout};
use super::content::{EXTRACTOR_VERSION, blob_from_record, extract, language_for};
use crate::callgraph_store::join::{
    CallgraphBlob, DispatchTypeTargetPlan, ManifestBlobReader, ManifestJoinError,
    join_selected_manifest_for_materialization,
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
    Ok(report)
}
