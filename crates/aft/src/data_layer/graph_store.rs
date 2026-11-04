//! A checkout's call graph stored in a segment and read back as slices.
//!
//! The segment holds the content record of every parsed file and one
//! fragment per file: the file's resolution, keyed by its record and path.
//! Each fragment carries the terms queries find fragments by (the targets
//! its calls reach, its reference and symbol names, its language), so a
//! query reads only the fragments that can hold its answer. A reader maps the
//! checkout's paths to their record and fragment once, and assembles a
//! file's slice the first time a query reads it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use aft_store::content::{ContentRecord, ContentSectionsBuilder, ContentSegment, RecordKey};
use aft_store::graph::{FragmentKey, GraphSectionsBuilder, GraphSegment, TermKind};
use aft_store::ids::Digest128;
use aft_store::segment::{KnownSection, Segment, SegmentMeta, SegmentWriter};

use super::callgraph::{CheckoutInputs, FileResolution};
use super::content::blob_from_record;
use super::query::{symbol_keys, SliceSource};
use super::slice::FileSlice;
use crate::callgraph_store::join::CallgraphBlob;

/// The sections a call-graph segment holds.
#[must_use]
pub(crate) fn known_sections() -> Vec<KnownSection> {
    aft_store::content::KNOWN
        .into_iter()
        .chain(aft_store::graph::KNOWN)
        .collect()
}

/// The key of the fragment for the file at `path` whose content record has
/// the key `record_key`.
#[must_use]
pub(crate) fn fragment_key(record_key: &str, path: &str) -> FragmentKey {
    FragmentKey::from_digest(Digest128::of(
        format!("aft call-graph fragment 1\0{record_key}\0{path}").as_bytes(),
    ))
}

/// The terms queries find a slice's fragment by.
fn slice_terms(slice: &FileSlice) -> Vec<(TermKind, Vec<u8>)> {
    let mut terms = vec![(TermKind::Language, slice.language.as_bytes().to_vec())];
    for edge in slice.edges.values() {
        terms.push((
            TermKind::CallTarget,
            call_target(&edge.target_file, &edge.target_symbol),
        ));
    }
    for reference in slice.refs.values() {
        if let Some(name) = &reference.short_name {
            terms.push((TermKind::ReferenceName, name.as_bytes().to_vec()));
        }
    }
    for name in symbol_keys(slice) {
        terms.push((TermKind::SymbolName, name.into_bytes()));
    }
    terms
}

fn call_target(file: &str, symbol: &str) -> Vec<u8> {
    format!("{file}\0{symbol}").into_bytes()
}

/// Writes the call graph of a checkout into a new segment at `path`.
/// `records` holds the content record of every parse, by its key.
///
/// # Errors
///
/// When the segment cannot be written.
pub(crate) fn write_segment(
    path: &Path,
    meta: SegmentMeta,
    inputs: &CheckoutInputs,
    resolved: &BTreeMap<String, FileResolution>,
    records: &HashMap<String, (RecordKey, Arc<ContentRecord>)>,
) -> Result<(), String> {
    let mut content = ContentSectionsBuilder::new();
    let mut graph = GraphSectionsBuilder::new();
    for (file, resolution) in resolved {
        let (key, parse) = &inputs.parses[file];
        let (record_key, record) = records
            .get(key)
            .ok_or_else(|| format!("no content record for {file}"))?;
        content.add(*record_key, record);
        let slice = FileSlice::build(file, key, parse, resolution);
        let terms = slice_terms(&slice);
        graph.add(
            fragment_key(key, file),
            super::fragment::encode(resolution),
            terms.iter().map(|(kind, term)| (*kind, term.as_slice())),
        );
    }
    let mut writer = SegmentWriter::create(path, meta).map_err(|error| error.to_string())?;
    content
        .write(&mut writer)
        .map_err(|error| error.to_string())?;
    graph
        .write(&mut writer)
        .map_err(|error| error.to_string())?;
    writer.finish().map_err(|error| error.to_string())?;
    Ok(())
}

/// Where one path's record and fragment are.
#[derive(Clone, Debug)]
struct Located {
    record_key: String,
    record: u32,
    fragment: u32,
}

/// A checkout's slices read from one segment.
///
/// A read that fails (a checksum, a malformed record) is recorded in
/// [`StoredSlices::failures`] and the file is treated as absent, because the
/// slice source answers with options; the gate fails on any recorded
/// failure.
pub(crate) struct StoredSlices<'s> {
    content: ContentSegment<'s>,
    graph: GraphSegment<'s>,
    paths: BTreeMap<String, Located>,
    by_fragment: HashMap<u32, String>,
    cache: Mutex<HashMap<String, Arc<FileSlice>>>,
    failures: Mutex<Vec<String>>,
}

impl<'s> StoredSlices<'s> {
    /// Opens the slices of `inputs`'s parsed files in `segment`.
    ///
    /// # Errors
    ///
    /// When the segment lacks the call-graph sections, or a parsed file has
    /// no record or fragment in it.
    pub(crate) fn open(segment: &'s Segment, inputs: &CheckoutInputs) -> Result<Self, String> {
        let content = ContentSegment::open(segment)
            .map_err(|error| error.to_string())?
            .ok_or("the segment has no content sections")?;
        let graph = GraphSegment::open(segment)
            .map_err(|error| error.to_string())?
            .ok_or("the segment has no graph sections")?;
        let mut paths = BTreeMap::new();
        let mut by_fragment = HashMap::new();
        for (file, (key, _)) in &inputs.parses {
            let record_key = record_key_of(key)?;
            let record = content
                .find(record_key)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("no stored record for {file}"))?;
            let fragment = graph
                .find(fragment_key(key, file))
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("no stored fragment for {file}"))?;
            by_fragment.insert(fragment, file.clone());
            paths.insert(
                file.clone(),
                Located {
                    record_key: key.clone(),
                    record,
                    fragment,
                },
            );
        }
        Ok(Self {
            content,
            graph,
            paths,
            by_fragment,
            cache: Mutex::new(HashMap::new()),
            failures: Mutex::new(Vec::new()),
        })
    }

    /// Reads that failed.
    pub(crate) fn failures(&self) -> Vec<String> {
        self.failures
            .lock()
            .expect("no panics while holding it")
            .clone()
    }

    fn fail(&self, what: String) {
        self.failures
            .lock()
            .expect("no panics while holding it")
            .push(what);
    }

    fn load(&self, file: &str, located: &Located) -> Result<FileSlice, String> {
        let record = self
            .content
            .record(located.record)
            .map_err(|error| error.to_string())?;
        let CallgraphBlob::Parse(parse) =
            blob_from_record(&record).map_err(|error| error.to_string())?
        else {
            return Err(format!("{file}: its stored record is not a parse"));
        };
        let bytes = self
            .graph
            .fragment(located.fragment)
            .map_err(|error| error.to_string())?;
        let resolution = super::fragment::decode(&located.record_key, file, &bytes)
            .map_err(|error| error.0.to_string())?;
        Ok(FileSlice::build(
            file,
            &located.record_key,
            &parse,
            &resolution,
        ))
    }

    fn files_for(&self, kind: TermKind, term: &[u8]) -> BTreeSet<String> {
        match self.graph.candidates(kind, term) {
            Ok(ordinals) => ordinals
                .into_iter()
                .filter_map(|ordinal| self.by_fragment.get(&ordinal).cloned())
                .collect(),
            Err(error) => {
                self.fail(format!("{kind:?} lookup: {error}"));
                BTreeSet::new()
            }
        }
    }
}

/// The record key of a parse as the content plane stores it: the parse's
/// key is that record key's digest, spelled in hex.
fn record_key_of(key: &str) -> Result<RecordKey, String> {
    let mut bytes = [0u8; Digest128::LEN];
    if key.len() != 2 * Digest128::LEN {
        return Err(format!("record key {key} is not a digest"));
    }
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&key[2 * index..2 * index + 2], 16)
            .map_err(|_| format!("record key {key} is not a digest"))?;
    }
    Ok(RecordKey::from_digest(Digest128::from_bytes(bytes)))
}

impl SliceSource for StoredSlices<'_> {
    fn slice(&self, path: &str) -> Option<Arc<FileSlice>> {
        if let Some(slice) = self
            .cache
            .lock()
            .expect("no panics while holding it")
            .get(path)
        {
            return Some(Arc::clone(slice));
        }
        let located = self.paths.get(path)?;
        match self.load(path, located) {
            Ok(slice) => {
                let slice = Arc::new(slice);
                self.cache
                    .lock()
                    .expect("no panics while holding it")
                    .insert(path.to_string(), Arc::clone(&slice));
                Some(slice)
            }
            Err(error) => {
                self.fail(format!("{path}: {error}"));
                None
            }
        }
    }

    fn caller_files(&self, target_file: &str, target_symbol: &str) -> BTreeSet<String> {
        self.files_for(
            TermKind::CallTarget,
            &call_target(target_file, target_symbol),
        )
    }

    fn files_with_ref_name(&self, short_name: &str) -> BTreeSet<String> {
        self.files_for(TermKind::ReferenceName, short_name.as_bytes())
    }

    fn files_with_symbol_name(&self, name: &str) -> BTreeSet<String> {
        self.files_for(TermKind::SymbolName, name.as_bytes())
    }

    fn files_of_language(&self, language: &str) -> BTreeSet<String> {
        self.files_for(TermKind::Language, language.as_bytes())
    }

    fn files(&self) -> Vec<String> {
        self.paths.keys().cloned().collect()
    }
}
