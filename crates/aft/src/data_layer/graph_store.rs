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

/// The key of a fragment: the file's record and path, the fragment's own
/// bytes, and the paths its recorded reads consulted (`subjects`, sorted).
/// Two checkouts can resolve the same content at the same path differently
/// (their other files differ), so the bytes are part of the key. They can
/// also record the same answers through different paths, for example
/// different symlink chains to one target; an update finds the files a
/// change reaches by those paths, so each checkout needs its own. Two
/// checkouts that resolved a file the same way through the same paths
/// share one fragment.
#[must_use]
pub(crate) fn fragment_key(
    record_key: &str,
    path: &str,
    bytes: &[u8],
    subjects: &[&[u8]],
) -> FragmentKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aft call-graph fragment 2\0");
    hasher.update(record_key.as_bytes());
    hasher.update(b"\0");
    hasher.update(path.as_bytes());
    hasher.update(b"\0");
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
    for subject in subjects {
        hasher.update(&(subject.len() as u64).to_le_bytes());
        hasher.update(subject);
    }
    FragmentKey::from_digest(Digest128::from_hash(&hasher.finalize()))
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

/// The terms that file a resolution under what it recorded as its inputs,
/// so an update finds the files a change can reach without reading every
/// fragment. Each kind matches what the incremental engine keeps in memory
/// for the same resolution. `subjects` memoises what each recorded read
/// consulted, since many files record the same reads.
fn input_terms(
    resolution: &FileResolution,
    facts: &super::callgraph::CheckoutFacts<'_>,
    subjects: &mut HashMap<crate::callgraph_store::fact_answers::FactQuestion, Vec<Vec<u8>>>,
) -> Vec<(TermKind, Vec<u8>)> {
    let binding = &resolution.binding;
    let mut terms = Vec::new();
    for key in binding.binding_keys() {
        terms.push((TermKind::BindingKey, key.into_bytes()));
    }
    for key in &binding.dependencies {
        terms.push((TermKind::ResolutionKey, key.as_bytes().to_vec()));
    }
    let mut questions = binding
        .answers()
        .map(|(question, _)| question)
        .collect::<Vec<_>>();
    if let Some(dispatch) = &resolution.dispatch {
        let trace = &dispatch.trace;
        for key in trace
            .dependencies()
            .into_iter()
            .chain(trace.probes().iter().cloned())
        {
            terms.push((TermKind::DispatchKey, key.into_bytes()));
        }
        questions.extend(trace.answers.keys());
    }
    for key in &resolution.site_dependencies {
        terms.push((TermKind::SiteKey, key.as_bytes().to_vec()));
    }
    let mut seen = BTreeSet::new();
    if binding.unattributed {
        terms.push((
            TermKind::FactSubject,
            super::incremental::UNATTRIBUTED_TERM.to_vec(),
        ));
    }
    for question in questions {
        let filed = subjects.entry(question.clone()).or_insert_with(|| {
            super::subjects::subjects(question, facts)
                .iter()
                .map(super::subjects::Subject::term)
                .collect()
        });
        for term in filed.iter() {
            if seen.insert(term.clone()) {
                terms.push((TermKind::FactSubject, term.clone()));
            }
        }
    }
    terms
}

/// The terms that find a file by the methods it declares and, for Go, by
/// the methods its interfaces require: what the method-call resolver looks
/// files up by (see `callgraph::MethodCallInputs`).
fn method_terms(parse: &crate::callgraph_store::join::ParseBlob) -> Vec<(TermKind, Vec<u8>)> {
    let mut terms = Vec::new();
    for method in &parse.dispatch.methods {
        terms.push((TermKind::MethodName, method.name.as_bytes().to_vec()));
    }
    if parse.language == "go" {
        for method in &parse.dispatch.methods {
            if parse
                .dispatch
                .types
                .iter()
                .any(|ty| ty.interface && ty.name == method.owner)
            {
                terms.push((TermKind::GoRequired, method.name.as_bytes().to_vec()));
            }
        }
    }
    terms
}

fn call_target(file: &str, symbol: &str) -> Vec<u8> {
    format!("{file}\0{symbol}").into_bytes()
}

/// Writes the call graph of a checkout into a new segment at `path`.
/// `records` holds the content record of every parse, by its key. Returns
/// each file's fragment key, which the checkout's state records.
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
) -> Result<BTreeMap<String, FragmentKey>, String> {
    let mut writer = SegmentWriter::create(path, meta).map_err(|error| error.to_string())?;
    let keys = write_sections(&mut writer, inputs, resolved, records, true)?;
    writer.finish().map_err(|error| error.to_string())?;
    Ok(keys)
}

/// Writes the content records and fragments of `resolved`'s files into
/// `writer`, as [`write_segment`] does, for a segment that may hold other
/// planes' sections too. Returns each file's fragment key. With
/// `all_records`, every file's content record must be in `records`;
/// without, a file whose record is missing keeps the one already stored.
///
/// # Errors
///
/// When a file has no content record and `all_records` asks for one, or
/// the sections cannot be written.
pub(crate) fn write_sections(
    writer: &mut SegmentWriter,
    inputs: &CheckoutInputs,
    resolved: &BTreeMap<String, FileResolution>,
    records: &HashMap<String, (RecordKey, Arc<ContentRecord>)>,
    all_records: bool,
) -> Result<BTreeMap<String, FragmentKey>, String> {
    let mut content = ContentSectionsBuilder::new();
    // Configuration files' records too, so a reader of the segment can give
    // the resolver the same configuration bytes the build read.
    for record in inputs.state.paths() {
        if record.language != "config" {
            continue;
        }
        let key = RecordKey::new(
            record.content,
            &record.language,
            super::content::EXTRACTOR_VERSION,
        );
        if let Some((record_key, stored)) = records.get(&key.digest().to_string()) {
            content.add(*record_key, stored);
        }
    }
    let mut graph = GraphSectionsBuilder::new();
    let mut keys = BTreeMap::new();
    let facts = super::callgraph::CheckoutFacts { inputs };
    let mut subjects = HashMap::new();
    for (file, resolution) in resolved {
        let (key, parse) = &inputs.parses[file];
        // A segment holding only what changed since a publication writes
        // the records of changed contents; an unchanged one stays where the
        // publication stored it.
        match records.get(key) {
            Some((record_key, record)) => content.add(*record_key, record),
            None if all_records => return Err(format!("no content record for {file}")),
            None => {}
        }
        let slice = FileSlice::build(file, key, parse, resolution);
        let mut terms = slice_terms(&slice);
        terms.extend(input_terms(resolution, &facts, &mut subjects));
        terms.extend(method_terms(parse));
        let bytes = super::fragment::encode(resolution);
        // A test stores a fragment that does not decode, as a damaged one
        // reads back.
        #[cfg(test)]
        let bytes = if DAMAGED_FRAGMENT.with(|damaged| damaged.borrow().as_deref() == Some(file)) {
            vec![0xFF, 0x00]
        } else {
            bytes
        };
        let mut subjects = terms
            .iter()
            .filter(|(kind, _)| *kind == TermKind::FactSubject)
            .map(|(_, term)| term.as_slice())
            .collect::<Vec<_>>();
        subjects.sort_unstable();
        subjects.dedup();
        let fragment = fragment_key(key, file, &bytes, &subjects);
        keys.insert(file.clone(), fragment);
        graph.add(
            fragment,
            bytes,
            terms.iter().map(|(kind, term)| (*kind, term.as_slice())),
        );
    }
    content.write(writer).map_err(|error| error.to_string())?;
    graph.write(writer).map_err(|error| error.to_string())?;
    Ok(keys)
}

#[cfg(test)]
thread_local! {
    /// The file whose fragment a test's build stores damaged.
    pub(crate) static DAMAGED_FRAGMENT: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Where one path's record and fragment are: each as (index into the
/// reader's segments, ordinal in that segment).
#[derive(Clone, Debug)]
struct Located {
    record_key: String,
    record: (u32, u32),
    fragment: (u32, u32),
}

/// A checkout's slices read from its segments.
///
/// The reader owns its segments, so a checkout's snapshot can keep it for
/// as long as the snapshot lives. A file's record and fragment can be in
/// different segments, and a later segment can hold a newer fragment of a
/// file; the checkout's state names which fragment is current, and lookups
/// skip every other one.
///
/// A read that fails (a checksum, a malformed record) is recorded in
/// [`StoredSlices::failures`] and the file is treated as absent, because the
/// slice source answers with options; callers fail on any recorded failure.
pub(crate) struct StoredSlices {
    segments: Vec<Arc<Segment>>,
    paths: BTreeMap<String, Located>,
    by_fragment: HashMap<(u32, u32), String>,
    cache: Mutex<HashMap<String, Arc<FileSlice>>>,
    failures: Mutex<Vec<String>>,
}

/// Finds `key` in the newest of `segments` that holds it.
fn find_newest<T>(
    segments: &[Option<T>],
    find: impl Fn(&T) -> Result<Option<u32>, aft_store::segment::SegmentError>,
) -> Result<Option<(u32, u32)>, String> {
    for (index, segment) in segments.iter().enumerate().rev() {
        let Some(segment) = segment else {
            continue;
        };
        if let Some(ordinal) = find(segment).map_err(|error| error.to_string())? {
            let index = u32::try_from(index).map_err(|_| "too many segments".to_string())?;
            return Ok(Some((index, ordinal)));
        }
    }
    Ok(None)
}

impl StoredSlices {
    /// Opens the slices of `inputs`'s parsed files in `segments` (oldest
    /// first), each with the fragment `fragments` names for it.
    ///
    /// # Errors
    ///
    /// When a segment is malformed, or a parsed file has no record or
    /// fragment in any segment.
    pub(crate) fn open(
        segments: Vec<Arc<Segment>>,
        inputs: &CheckoutInputs,
        fragments: &BTreeMap<String, FragmentKey>,
    ) -> Result<Self, String> {
        let files = inputs
            .parses
            .iter()
            .map(|(file, (key, _))| {
                let fragment = fragments
                    .get(file)
                    .ok_or_else(|| format!("no fragment key for {file}"))?;
                Ok((file.clone(), key.clone(), *fragment))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Self::open_files(segments, &files)
    }

    /// Opens the slices of `files`, each a (path, parse key, fragment key),
    /// in `segments` (oldest first).
    ///
    /// # Errors
    ///
    /// When a segment is malformed, or a file has no record or fragment in
    /// any segment.
    pub(crate) fn open_files(
        segments: Vec<Arc<Segment>>,
        files: &[(String, String, FragmentKey)],
    ) -> Result<Self, String> {
        let mut paths = BTreeMap::new();
        let mut by_fragment = HashMap::new();
        {
            let contents = segments
                .iter()
                .map(|segment| ContentSegment::open(segment))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            let graphs = segments
                .iter()
                .map(|segment| GraphSegment::open(segment))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            for (file, key, fragment_key) in files {
                let record_key = record_key_of(key)?;
                let record = find_newest(&contents, |content| content.find(record_key))?
                    .ok_or_else(|| format!("no stored record for {file}"))?;
                let fragment = find_newest(&graphs, |graph| graph.find(*fragment_key))?
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
        }
        Ok(Self {
            segments,
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

    fn content(&self, index: u32) -> Result<ContentSegment<'_>, String> {
        ContentSegment::open(&self.segments[index as usize])
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "a record's segment has no content sections".to_string())
    }

    fn graph(&self, index: u32) -> Result<GraphSegment<'_>, String> {
        GraphSegment::open(&self.segments[index as usize])
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "a fragment's segment has no graph sections".to_string())
    }

    fn load(&self, file: &str, located: &Located) -> Result<FileSlice, String> {
        let record = self
            .content(located.record.0)?
            .record(located.record.1)
            .map_err(|error| error.to_string())?;
        let CallgraphBlob::Parse(parse) =
            blob_from_record(&record).map_err(|error| error.to_string())?
        else {
            return Err(format!("{file}: its stored record is not a parse"));
        };
        let bytes = self
            .graph(located.fragment.0)?
            .fragment(located.fragment.1)
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

    /// The configuration bytes of every path of `state` the resolver reads
    /// as configuration, from their stored records. A configuration file
    /// whose record no segment holds is left out, as a missing file is.
    ///
    /// # Errors
    ///
    /// When a record cannot be read.
    pub(crate) fn configs(
        &self,
        state: &aft_store::checkout::CheckoutState,
    ) -> Result<HashMap<Vec<u8>, Arc<[u8]>>, String> {
        let contents = self
            .segments
            .iter()
            .map(|segment| ContentSegment::open(segment))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let mut configs = HashMap::new();
        for record in state.paths() {
            if record.language != "config" {
                continue;
            }
            let key = RecordKey::new(
                record.content,
                &record.language,
                super::content::EXTRACTOR_VERSION,
            );
            let Some((index, ordinal)) = find_newest(&contents, |content| content.find(key))?
            else {
                continue;
            };
            let stored = contents[index as usize]
                .as_ref()
                .expect("found in it")
                .record(ordinal)
                .map_err(|error| error.to_string())?;
            if let aft_store::content::RecordBody::Config { source } = stored.data {
                configs.insert(record.path.clone(), Arc::from(source));
            }
        }
        Ok(configs)
    }

    /// Every file's parse and resolution, decoded from its stored record
    /// and fragment.
    ///
    /// # Errors
    ///
    /// When a record or fragment cannot be read.
    #[allow(clippy::type_complexity)]
    pub(crate) fn resolutions(
        &self,
    ) -> Result<
        (
            BTreeMap<String, (String, Arc<crate::callgraph_store::join::ParseBlob>)>,
            BTreeMap<String, FileResolution>,
        ),
        String,
    > {
        self.resolutions_timed(&mut Vec::new())
    }

    /// [`Self::resolutions`], adding the time spent reading records,
    /// converting them to parse output, and decoding fragments to `timings`.
    /// Every file with a stored resolution, with its record key, in path
    /// order.
    pub(crate) fn parsed_files(&self) -> impl Iterator<Item = (&String, &str)> {
        self.paths
            .iter()
            .map(|(file, located)| (file, located.record_key.as_str()))
    }

    /// `file`'s record key and parse, read and decoded now. A read that
    /// fails is recorded in [`Self::failures`].
    pub(crate) fn parse_of(
        &self,
        file: &str,
    ) -> Option<(String, Arc<crate::callgraph_store::join::ParseBlob>)> {
        let located = self.paths.get(file)?;
        let read = || -> Result<_, String> {
            let record = self
                .content(located.record.0)?
                .record(located.record.1)
                .map_err(|error| error.to_string())?;
            match blob_from_record(&record).map_err(|error| error.to_string())? {
                CallgraphBlob::Parse(parse) => Ok(parse),
                CallgraphBlob::Config(_) => {
                    Err(format!("{file}: its stored record is not a parse"))
                }
            }
        };
        match read() {
            Ok(parse) => Some((located.record_key.clone(), Arc::new(parse))),
            Err(error) => {
                self.fail(format!("parse of {file}: {error}"));
                None
            }
        }
    }

    /// `file`'s stored resolution, read and decoded now. A read that fails
    /// is recorded in [`Self::failures`].
    pub(crate) fn resolution_of(&self, file: &str) -> Option<FileResolution> {
        let located = self.paths.get(file)?;
        let read = || -> Result<_, String> {
            let bytes = self
                .graph(located.fragment.0)?
                .fragment(located.fragment.1)
                .map_err(|error| error.to_string())?;
            super::fragment::decode(&located.record_key, file, &bytes)
                .map_err(|error| error.0.to_string())
        };
        match read() {
            Ok(resolution) => Some(resolution),
            Err(error) => {
                self.fail(format!("resolution of {file}: {error}"));
                None
            }
        }
    }

    pub(crate) fn resolutions_timed(
        &self,
        timings: &mut Vec<(&'static str, f64)>,
    ) -> Result<
        (
            BTreeMap<String, (String, Arc<crate::callgraph_store::join::ParseBlob>)>,
            BTreeMap<String, FileResolution>,
        ),
        String,
    > {
        let mut parses = BTreeMap::new();
        let mut resolved = BTreeMap::new();
        let (mut records, mut convert, mut fragments) = (0.0, 0.0, 0.0);
        let ms = |started: std::time::Instant| started.elapsed().as_secs_f64() * 1000.0;
        for (file, located) in &self.paths {
            let started = std::time::Instant::now();
            let record = self
                .content(located.record.0)?
                .record(located.record.1)
                .map_err(|error| error.to_string())?;
            records += ms(started);
            let started = std::time::Instant::now();
            let CallgraphBlob::Parse(parse) =
                blob_from_record(&record).map_err(|error| error.to_string())?
            else {
                return Err(format!("{file}: its stored record is not a parse"));
            };
            convert += ms(started);
            let started = std::time::Instant::now();
            let bytes = self
                .graph(located.fragment.0)?
                .fragment(located.fragment.1)
                .map_err(|error| error.to_string())?;
            let resolution = super::fragment::decode(&located.record_key, file, &bytes)
                .map_err(|error| error.0.to_string())?;
            fragments += ms(started);
            parses.insert(file.clone(), (located.record_key.clone(), Arc::new(parse)));
            resolved.insert(file.clone(), resolution);
        }
        timings.push(("read content records", records));
        timings.push(("convert records to parse output", convert));
        timings.push(("read and decode fragments", fragments));
        Ok((parses, resolved))
    }

    /// The files whose current fragment is listed under `term`, in every
    /// segment that has graph sections.
    /// The files whose stored resolution recorded `key` as an input of
    /// `kind`. Terms are stored as hashes, so a few files may be listed that
    /// did not record it; callers check the resolution itself.
    pub(crate) fn files_recording(&self, kind: TermKind, key: &[u8]) -> BTreeSet<String> {
        self.files_for(kind, key)
    }

    fn files_for(&self, kind: TermKind, term: &[u8]) -> BTreeSet<String> {
        let mut files = BTreeSet::new();
        for (index, segment) in self.segments.iter().enumerate() {
            let graph = match GraphSegment::open(segment) {
                Ok(Some(graph)) => graph,
                Ok(None) => continue,
                Err(error) => {
                    self.fail(format!("{kind:?} lookup: {error}"));
                    continue;
                }
            };
            let index = u32::try_from(index).expect("fewer than 2^32 segments");
            match graph.candidates(kind, term) {
                Ok(ordinals) => files.extend(
                    ordinals
                        .into_iter()
                        .filter_map(|ordinal| self.by_fragment.get(&(index, ordinal)).cloned()),
                ),
                Err(error) => self.fail(format!("{kind:?} lookup: {error}")),
            }
        }
        files
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

impl SliceSource for StoredSlices {
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

aft_store::heap_bytes_of_fields!(Located {
    record_key,
    record,
    fragment,
});
aft_store::heap_bytes_of_fields!(StoredSlices {
    segments,
    paths,
    by_fragment,
    cache,
    failures,
});
