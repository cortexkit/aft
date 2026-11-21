//! Build the checkout call graph used by the measurement harness and data-layer tests.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use aft_store::content::{ContentRecord, RecordBody, RecordKey};
use aft_store::ids::ContentKey;
use aft_store::scan::{scan, ContentKind, ContentSink};

use super::callgraph::{resolve_checkout, CheckoutInputs};
use super::content::{blob_from_record, language_for, record_key};
use super::grammar::extract;
use crate::callgraph_store::join::CallgraphBlob;

pub use crate::trace_measure_run::run;

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
        let key = record_key(content, &language);
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

/// A checkout's call graph as the data layer builds it: every file scanned
/// and extracted to a content record, and every parsed file resolved.
pub(crate) struct Built {
    pub(crate) inputs: CheckoutInputs,
    pub(crate) resolved: BTreeMap<String, super::callgraph::FileResolution>,
    pub(crate) records: HashMap<RecordKey, Arc<ContentRecord>>,
}

pub(crate) fn build(root: &Path, threads: usize) -> Result<Built, String> {
    let records = Records::default();
    let scanned = scan(root, &super::content::scan_policy(), None, &records)
        .map_err(|error| error.to_string())?;
    let failures = records.failures.into_inner().expect("no panics");
    if !failures.is_empty() {
        return Err(format!("extraction failed: {failures:?}"));
    }
    let by_key = records.by_key.into_inner().expect("no panics");
    let by_path = records.by_path.into_inner().expect("no panics");
    let mut inputs = CheckoutInputs {
        state: scanned.state,
        parses: super::callgraph::Parses::default(),
        configs: HashMap::new(),
    };
    for (path, key) in by_path {
        let full_key = key.digest().to_string();
        let record = &by_key[&key];
        match &record.data {
            RecordBody::Parsed { .. } => {
                let CallgraphBlob::Parse(parse) =
                    blob_from_record(record).map_err(|error| error.to_string())?
                else {
                    unreachable!("a parsed record converts to a parse blob");
                };
                inputs.parses.insert(path, (full_key, Arc::new(parse)));
            }
            RecordBody::Config { source } => {
                inputs
                    .configs
                    .insert(path.into_bytes(), Arc::from(source.as_slice()));
            }
            RecordBody::Markers => {}
        }
    }
    let resolved = resolve_checkout(&inputs, threads).map_err(|error| error.to_string())?;
    Ok(Built {
        inputs,
        resolved,
        records: by_key,
    })
}

/// The data layer's call graph of the checkout at `root`, held in memory,
/// for tests of the commands built on it.
#[cfg(test)]
pub(crate) fn memory_reader(root: &Path) -> super::query::SliceReader<super::query::MemorySlices> {
    let built = build(root, 2).unwrap_or_else(|error| panic!("build {}: {error}", root.display()));
    let mut slices = super::query::MemorySlices::default();
    for (path, file) in &built.resolved {
        let (key, parse) = &built.inputs.parses[path];
        slices.insert(super::slice::FileSlice::build(path, key, parse, file));
    }
    super::query::SliceReader::new(slices, root.to_path_buf(), "test".to_string())
}
