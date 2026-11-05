//! Meaning search on the data layer: each file content cut into today's
//! chunks, the chunks' vectors stored once per family, and exact searches
//! over a checkout's view.
//!
//! A file's chunks are today's semantic chunks
//! ([`crate::semantic_index::chunk_view_file`]): one per symbol, plus a
//! summary chunk for files with few top-level exports, each with the text
//! today's index embeds. A chunk's vector is keyed by the model and the
//! digest of that text, and a file's chunk map by its content and path (the
//! text names the path), so every checkout and every commit holding the
//! same file at the same path shares them, and a fill embeds only text no
//! checkout embedded before. Each map also keeps what a search shows for
//! its chunks (name, kind, lines, snippet), so results come from the store
//! alone, never from files on disk that may have changed since.
//!
//! Vectors are published in segments of their own, in generations that
//! change no checkout's state. Coverage is worked out when a view is
//! searched, from its members and the chunk maps the newest generation
//! holds. The plane keeps its own snapshot of that generation and pairs it
//! with the view's members; everything here is keyed by content, so any
//! such pairing answers as one generation would.
//!
//! A search returns today's `SemanticIndex::search_filtered` answer over
//! the same vectors: the same chunks, in the same order, with the same
//! score bits. Its members are the view's files in path order and each
//! file's chunks in chunk order, which is the order equal scores keep.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aft_store::cache::BlockCache;
use aft_store::checkout::{CheckoutState, PathKind};
use aft_store::family::{Family, Publication, Snapshot, WriteSession};
use aft_store::ids::{ContentKey, Generation, SegmentId};
use aft_store::segment::{KnownSection, Segment};
use aft_store::vector::{
    Chunk, ChunkDetail, ChunkMapKey, FileDetails, Member, MemberSet, ModelId, ModelIdentity,
    SearchRequest, SharedVectorIndex, TextDigest, VectorIndex, VectorSectionsBuilder,
    VectorSegment,
};

use super::checkout::{Checkout, CheckoutView, DataError};
use crate::semantic_index::{
    chunk_view_file, execute_build_embedding_batch, is_semantic_indexed_extension,
    BuildEmbeddingRow, EmbedTextCaps, SemanticIndexFingerprint, SemanticResult,
};
use crate::symbols::SymbolKind;

/// What the vector plane reads and publishes through, lent by
/// [`Checkout::vector_parts`].
pub(crate) struct VectorParts<'c> {
    pub(crate) family: &'c Family,
    pub(crate) cache: &'c Arc<BlockCache>,
    pub(crate) root: &'c Path,
    /// Every section a segment of the family can hold: vector segments sit
    /// in the same generations as text and call-graph ones.
    pub(crate) known: Vec<KnownSection>,
}

/// Version of the template that builds the text sent to the embedder:
/// today's symbol and file-summary texts. The caps that shorten them are in
/// the model's revision.
const TEXT_TEMPLATE: u32 = 1;

/// Model identity strings are stored up to this length; a longer one is
/// replaced by its digest.
const MAX_IDENTITY_STRING: usize = 4096;

/// How a plane cuts files into chunks and which model embeds them.
#[derive(Clone, Debug)]
pub struct Producer {
    model: ModelIdentity,
    caps: EmbedTextCaps,
}

impl Producer {
    /// The producer of today's semantic index configured as `fingerprint`.
    /// The model's revision is the whole fingerprint (backend, address, text
    /// caps, chunking version and any Synapse identity), so two
    /// configurations share vectors exactly when today's index would reuse
    /// them.
    #[must_use]
    pub fn from_fingerprint(fingerprint: &SemanticIndexFingerprint) -> Self {
        let bounded = |text: String| {
            if text.len() <= MAX_IDENTITY_STRING {
                text
            } else {
                format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex())
            }
        };
        Self {
            model: ModelIdentity {
                name: bounded(fingerprint.model.clone()),
                revision: bounded(fingerprint.as_string()),
                dimensions: u32::try_from(fingerprint.dimension).unwrap_or(u32::MAX),
                // Today's fingerprint does not record it; scores are cosine
                // either way.
                normalized: false,
                text_template: TEXT_TEMPLATE,
                chunk_template: fingerprint.chunking_version,
            },
            caps: fingerprint.embed_text_caps,
        }
    }

    #[must_use]
    pub fn model(&self) -> &ModelIdentity {
        &self.model
    }
}

/// `path` (relative to the root, `/`-separated) in the platform's own form,
/// as today's index names files and builds their embedded text. `None` for
/// a path this platform cannot name.
fn native_path(path: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(path)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(path)
            .ok()
            .map(|path| PathBuf::from(path.replace('/', std::path::MAIN_SEPARATOR_STR)))
    }
}

/// Whether meaning search covers the file at `path`: today's semantic
/// index embeds files with its name or extension.
fn is_semantic_path(path: &[u8]) -> bool {
    native_path(path).is_some_and(|path| is_semantic_indexed_extension(&path))
}

/// The code a chunk's kind is stored as.
fn kind_code(kind: &SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Kernel => 1,
        SymbolKind::Class => 2,
        SymbolKind::Method => 3,
        SymbolKind::Struct => 4,
        SymbolKind::Interface => 5,
        SymbolKind::Enum => 6,
        SymbolKind::TypeAlias => 7,
        SymbolKind::Variable => 8,
        SymbolKind::Heading => 9,
        SymbolKind::FileSummary => 10,
    }
}

/// The kind stored as `code`.
fn kind_of(code: u8) -> Option<SymbolKind> {
    Some(match code {
        0 => SymbolKind::Function,
        1 => SymbolKind::Kernel,
        2 => SymbolKind::Class,
        3 => SymbolKind::Method,
        4 => SymbolKind::Struct,
        5 => SymbolKind::Interface,
        6 => SymbolKind::Enum,
        7 => SymbolKind::TypeAlias,
        8 => SymbolKind::Variable,
        9 => SymbolKind::Heading,
        10 => SymbolKind::FileSummary,
        _ => return None,
    })
}

/// One chunk of a file as a fill embeds it and a search shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct CutChunk {
    /// The exact text sent to the embedder.
    pub text: String,
    pub exported: bool,
    pub detail: ChunkDetail,
}

/// What cutting one file's bytes into chunks gave.
#[derive(Clone, Debug, PartialEq)]
pub enum Cut {
    /// The file's chunks, in today's order; none for a file over today's
    /// size limit or with nothing to embed.
    Chunks(Vec<CutChunk>),
    /// The bytes cannot be chunked (not UTF-8, or a parse failure), as
    /// today's index reports them.
    Failed(String),
}

/// Cuts the content `bytes` of the file at `path` (relative to `root`) into
/// today's chunks under `caps`.
pub fn cut_file(root: &Path, path: &[u8], bytes: &[u8], caps: EmbedTextCaps) -> Cut {
    let Some(relative) = native_path(path) else {
        return Cut::Failed("the path cannot be named on this platform".to_string());
    };
    match chunk_view_file(root, &relative, bytes, caps) {
        Ok(chunks) => Cut::Chunks(
            chunks
                .unwrap_or_default()
                .into_iter()
                .map(|chunk| CutChunk {
                    exported: chunk.exported,
                    detail: ChunkDetail {
                        kind: kind_code(&chunk.kind),
                        start_line: chunk.start_line,
                        end_line: chunk.end_line,
                        name: chunk.name,
                        qualified_name: chunk.qualified_name,
                        snippet: chunk.snippet,
                        failure: None,
                    },
                    text: chunk.embed_text,
                })
                .collect(),
        ),
        Err(reason) => Cut::Failed(reason),
    }
}

fn store_error(error: impl std::fmt::Display + 'static) -> DataError {
    DataError::store(error)
}

/// How much of a checkout meaning search covers, worked out from its
/// members and the chunk maps the store holds. Every member file meaning
/// search applies to is in exactly one of the file counts.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Coverage {
    /// Member files meaning search applies to (today's semantic file
    /// names and extensions).
    pub files: usize,
    /// Files whose every chunk has a vector or a recorded failure,
    /// files with no chunks included.
    pub files_embedded: usize,
    /// Files over today's 4 MiB limit: no chunks, as today.
    pub files_too_large: usize,
    /// Files that could not be read or cut into chunks, with why.
    pub files_failed: Vec<(PathBuf, String)>,
    /// Files within `semantic.max_files` still waiting for vectors.
    pub pending: Vec<PathBuf>,
    /// Files past `semantic.max_files` that no checkout has embedded.
    pub past_cap: usize,
    /// Chunks of the files cut so far; the pending files' are not known
    /// yet.
    pub chunks: usize,
    /// Of those, chunks with a vector.
    pub chunks_embedded: usize,
    /// Chunks the embedder refused: their file, name and why.
    pub chunks_failed: Vec<(PathBuf, String, String)>,
}

impl Coverage {
    /// Whether every file meaning search applies to is accounted for:
    /// embedded, too large, or failed with a reason.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.pending.is_empty() && self.past_cap == 0
    }
}

/// One member file of a checkout state that meaning search applies to, as
/// the store holds it.
struct Surveyed {
    path: Vec<u8>,
    kind: PathKind,
    content: ContentKey,
    /// Within the first `semantic.max_files` such files, in path order.
    admitted: bool,
    /// Its chunk map, and the position of the segment holding it in the
    /// plane's index; `None` when no segment has one.
    map: Option<(u32, Vec<Chunk>)>,
}

impl Surveyed {
    /// Whether every chunk of the stored map has a vector or a recorded
    /// failure.
    fn done(&self, index: Option<&VectorIndex<'_>>) -> bool {
        self.map.as_ref().is_some_and(|(_, chunks)| {
            chunks.iter().all(|chunk| {
                chunk.failed || index.is_some_and(|index| index.locate(chunk.text).is_some())
            })
        })
    }
}

/// The member files of `state` meaning search applies to, in path order,
/// each with its chunk map under `loaded`.
fn survey(
    state: &CheckoutState,
    loaded: &Loaded,
    index: Option<&VectorIndex<'_>>,
    max_files: usize,
) -> Result<Vec<Surveyed>, DataError> {
    let mut files = Vec::new();
    for record in state.paths() {
        if record.kind == PathKind::Symlink || !is_semantic_path(&record.path) {
            continue;
        }
        let admitted = files.len() < max_files;
        let map = match index {
            Some(index) if record.kind.has_content() => {
                loaded.chunk_map(index, record.content, &record.path)?
            }
            _ => None,
        };
        files.push(Surveyed {
            path: record.path.clone(),
            kind: record.kind,
            content: record.content,
            admitted,
            map,
        });
    }
    Ok(files)
}

/// The newest generation of the family the plane has read vectors from.
struct Loaded {
    generation: Generation,
    /// Keeps the generation's files on disk while the plane reads them.
    _snapshot: Option<Snapshot>,
    /// The model's vectors in the generation's segments.
    shared: Option<SharedVectorIndex>,
    /// Each chunk map's newest holder: a position in the index's segments.
    maps: HashMap<ChunkMapKey, u32>,
    /// The generation's segments that hold no vectors of this model, so
    /// the next generation opens none of them again.
    other: HashSet<SegmentId>,
    /// The generation's open segments of this model, by id, for the next
    /// generation to keep.
    open: HashMap<SegmentId, Arc<Segment>>,
}

impl Loaded {
    fn empty() -> Self {
        Self {
            generation: Generation::EMPTY,
            _snapshot: None,
            shared: None,
            maps: HashMap::new(),
            other: HashSet::new(),
            open: HashMap::new(),
        }
    }

    /// The newest generation of the family, read with what `previous`
    /// already read: segments are immutable and their ids never reused, so
    /// a segment `previous` opened is the same file in any generation.
    fn read(
        parts: &VectorParts<'_>,
        model: &ModelIdentity,
        previous: &Self,
    ) -> Result<Self, DataError> {
        let snapshot = parts.family.snapshot().map_err(store_error)?;
        let mut segments = Vec::new();
        let mut other = HashSet::new();
        for entry in snapshot.manifest().segments() {
            if previous.other.contains(&entry.id) {
                other.insert(entry.id);
                continue;
            }
            let segment = match previous.open.get(&entry.id) {
                Some(segment) => Arc::clone(segment),
                None => Arc::new(
                    snapshot
                        .open_segment(entry.id, &parts.known, Arc::clone(parts.cache))
                        .map_err(store_error)?,
                ),
            };
            segments.push(segment);
        }
        let shared = match &previous.shared {
            Some(shared) => shared.rebuild(segments.iter().cloned()),
            None => SharedVectorIndex::build(segments.iter().cloned(), model),
        }
        .map_err(store_error)?;
        let kept: HashSet<SegmentId> = shared.segments().map(|segment| segment.id()).collect();
        let mut open = HashMap::new();
        for segment in segments {
            if kept.contains(&segment.id()) {
                open.insert(segment.id(), segment);
            } else {
                other.insert(segment.id());
            }
        }
        let mut maps = HashMap::new();
        for (position, segment) in shared.index().segments().iter().enumerate() {
            let position = u32::try_from(position).map_err(store_error)?;
            // Later segments are newer: their maps replace older ones.
            for key in segment.chunk_map_keys().map_err(store_error)? {
                maps.insert(key, position);
            }
        }
        Ok(Self {
            generation: snapshot.generation(),
            _snapshot: Some(snapshot),
            shared: (shared.vectors() > 0 || !maps.is_empty()).then_some(shared),
            maps,
            other,
            open,
        })
    }

    /// The newest chunk map of the file at `path` with `content`, and the
    /// position of the segment holding it.
    fn chunk_map(
        &self,
        index: &VectorIndex<'_>,
        content: ContentKey,
        path: &[u8],
    ) -> Result<Option<(u32, Vec<Chunk>)>, DataError> {
        let Some(&holder) = self.maps.get(&ChunkMapKey::new(content, path)) else {
            return Ok(None);
        };
        let chunks = index.segments()[holder as usize]
            .chunk_map(content, path)
            .map_err(store_error)?
            .ok_or_else(|| DataError::Store("a listed chunk map is missing".to_string()))?;
        Ok(Some((holder, chunks)))
    }

    /// The details stored with the chunk map held by segment `holder`.
    fn details(
        index: &VectorIndex<'_>,
        holder: u32,
        content: ContentKey,
        path: &[u8],
    ) -> Result<FileDetails, DataError> {
        index.segments()[holder as usize]
            .chunk_details(content, path)
            .map_err(store_error)?
            .ok_or_else(|| DataError::Store("a chunk map has no details".to_string()))
    }
}

/// One file whose chunks a search may return.
struct SearchFile {
    path: Vec<u8>,
    content: ContentKey,
    /// The position of the segment holding its chunk map.
    holder: u32,
}

/// A view's members under one loaded generation.
struct Members {
    /// The view's text generation, and the generation the members were
    /// read from: the cache key.
    view: String,
    generation: Generation,
    files: Vec<SearchFile>,
    /// Every chunk with a vector, in path then chunk order.
    members: Vec<Member>,
    /// For each member, its file in `files`.
    file_of: Vec<u32>,
    /// For each member, its chunk's position in its file's chunk map.
    chunk_of: Vec<u32>,
    /// `members` prepared for the loaded index; `None` when the generation
    /// holds no vectors of the model.
    set: Option<MemberSet>,
    coverage: Coverage,
}

impl Members {
    fn build(view: &CheckoutView, loaded: &Loaded, max_files: usize) -> Result<Self, DataError> {
        let index = loaded.shared.as_ref().map(SharedVectorIndex::index);
        let surveyed = survey(view.current_state(), loaded, index.as_ref(), max_files)?;
        let absolute = |path: &[u8]| {
            view.root()
                .join(native_path(path).expect("surveyed paths have a native form"))
        };
        let mut members = Self {
            view: view.text_generation(),
            generation: loaded.generation,
            files: Vec::new(),
            members: Vec::new(),
            file_of: Vec::new(),
            chunk_of: Vec::new(),
            set: None,
            coverage: Coverage {
                files: surveyed.len(),
                ..Coverage::default()
            },
        };
        for file in &surveyed {
            let coverage = &mut members.coverage;
            match file.kind {
                PathKind::Oversize => {
                    coverage.files_too_large += 1;
                    continue;
                }
                PathKind::Unreadable => {
                    coverage.files_failed.push((
                        absolute(&file.path),
                        "the file could not be read".to_string(),
                    ));
                    continue;
                }
                _ => {}
            }
            let (Some(index), Some((holder, chunks))) = (index.as_ref(), &file.map) else {
                if file.admitted {
                    coverage.pending.push(absolute(&file.path));
                } else {
                    coverage.past_cap += 1;
                }
                continue;
            };
            let details = || Loaded::details(index, *holder, file.content, &file.path);
            if chunks.is_empty() {
                if let Some(reason) = details()?.failure {
                    coverage.files_failed.push((absolute(&file.path), reason));
                } else {
                    coverage.files_embedded += 1;
                }
                continue;
            }
            let file_index = u32::try_from(members.files.len()).map_err(store_error)?;
            let mut missing = false;
            let mut refused = false;
            for (position, chunk) in chunks.iter().enumerate() {
                coverage.chunks += 1;
                if let Some(unit) = index.locate(chunk.text) {
                    coverage.chunks_embedded += 1;
                    members.members.push(Member {
                        unit,
                        exported: chunk.exported,
                    });
                    members.file_of.push(file_index);
                    members
                        .chunk_of
                        .push(u32::try_from(position).map_err(store_error)?);
                } else if chunk.failed {
                    refused = true;
                } else {
                    missing = true;
                }
            }
            if refused {
                let path = absolute(&file.path);
                for (chunk, detail) in chunks.iter().zip(details()?.chunks) {
                    if chunk.failed && index.locate(chunk.text).is_none() {
                        coverage.chunks_failed.push((
                            path.clone(),
                            detail.name,
                            detail.failure.unwrap_or_default(),
                        ));
                    }
                }
            }
            if !missing {
                coverage.files_embedded += 1;
            } else if file.admitted {
                coverage.pending.push(absolute(&file.path));
            } else {
                coverage.past_cap += 1;
            }
            if members.file_of.last() == Some(&file_index) {
                members.files.push(SearchFile {
                    path: file.path.clone(),
                    content: file.content,
                    holder: *holder,
                });
            }
        }
        if let Some(index) = &index {
            members.set = Some(
                index
                    .member_set(members.members.clone())
                    .map_err(store_error)?,
            );
        }
        Ok(members)
    }
}

/// One view's meaning search over one generation of the family's vectors:
/// what [`VectorPlane::snapshot`] pinned. Every search and coverage read
/// through it answers from that same generation, however many fills land
/// meanwhile.
#[derive(Clone)]
pub struct MeaningSnapshot {
    loaded: Arc<Loaded>,
    members: Arc<Members>,
    root: PathBuf,
    dimensions: usize,
}

impl std::fmt::Debug for MeaningSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MeaningSnapshot")
            .field("generation", &self.loaded.generation)
            .field("view", &self.members.view)
            .field("coverage", &self.members.coverage)
            .finish_non_exhaustive()
    }
}

impl MeaningSnapshot {
    /// How much of the view this generation covers.
    #[must_use]
    pub fn coverage(&self) -> &Coverage {
        &self.members.coverage
    }

    /// The generation of the family's vectors the snapshot reads.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.loaded.generation
    }

    /// The view's text generation the members were read under.
    #[must_use]
    pub fn view_generation(&self) -> &str {
        &self.members.view
    }

    /// The checkout's root folder, which result paths are joined to.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The model's vector length: a query of another length finds nothing.
    #[must_use]
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Chunks with a vector that a search can return.
    #[must_use]
    pub fn chunks(&self) -> usize {
        self.members.members.len()
    }

    /// The `top_k` chunks closest to `query`, among files whose absolute
    /// path `include` accepts: today's `search_filtered` answer over the
    /// same vectors, as its rows. As today, a query of another dimension or
    /// with a component that is not finite finds nothing.
    ///
    /// # Errors
    ///
    /// When the store cannot be read.
    pub fn search(
        &self,
        query: &[f32],
        top_k: usize,
        include: &dyn Fn(&Path) -> bool,
    ) -> Result<Vec<SemanticResult>, DataError> {
        let mut results = Vec::new();
        let members = &self.members;
        let (Some(shared), Some(all)) = (&self.loaded.shared, &members.set) else {
            return Ok(results);
        };
        if top_k == 0
            || shared.vectors() == 0
            || query.len() != self.dimensions
            || query.iter().any(|value| !value.is_finite())
        {
            return Ok(results);
        }
        let index = shared.index();
        let root = &self.root;
        let absolute = |path: &[u8]| root.join(native_path(path).expect("members have one"));
        let included: Vec<bool> = members
            .files
            .iter()
            .map(|file| include(&absolute(&file.path)))
            .collect();
        // Today filters before choosing the top k, so excluded files never
        // take a place. A subset keeps the members' order, and so their ties.
        let subset;
        let (set, positions) = if included.iter().all(|included| *included) {
            (all, None)
        } else {
            let mut kept = Vec::new();
            let mut positions = Vec::new();
            for (position, (member, file)) in
                members.members.iter().zip(&members.file_of).enumerate()
            {
                if included[*file as usize] {
                    kept.push(*member);
                    positions.push(position);
                }
            }
            subset = index.member_set(kept).map_err(store_error)?;
            (&subset, Some(positions))
        };
        let found = index
            .search(
                set,
                &SearchRequest {
                    query,
                    k: top_k,
                    rescore_limit: None,
                },
            )
            .map_err(store_error)?;
        let mut details: HashMap<u32, FileDetails> = HashMap::new();
        for hit in found.hits {
            let position = positions.as_ref().map_or(hit.member as usize, |positions| {
                positions[hit.member as usize]
            });
            let file_index = members.file_of[position];
            let file = &members.files[file_index as usize];
            if let std::collections::hash_map::Entry::Vacant(slot) = details.entry(file_index) {
                slot.insert(Loaded::details(
                    &index,
                    file.holder,
                    file.content,
                    &file.path,
                )?);
            }
            let detail = &details[&file_index].chunks[members.chunk_of[position] as usize];
            let kind = kind_of(detail.kind)
                .ok_or_else(|| DataError::Store("a chunk's kind code is unknown".to_string()))?;
            results.push(SemanticResult {
                file: absolute(&file.path),
                name: detail.name.clone(),
                qualified_name: detail.qualified_name.clone(),
                kind,
                start_line: detail.start_line,
                end_line: detail.end_line,
                exported: members.members[position].exported,
                snippet: detail.snippet.clone(),
                score: hit.score,
                rank_score: hit.score,
                cap_protected: false,
                source: "semantic",
            });
        }
        Ok(results)
    }
}

/// What a meaning search found, and how much of the checkout it covered.
#[derive(Debug)]
pub struct MeaningAnswer {
    /// Today's result rows, best first.
    pub results: Vec<SemanticResult>,
    pub coverage: Coverage,
}

/// How much one fill may do at a time.
#[derive(Clone, Copy, Debug)]
pub struct FillBudget {
    /// Texts per embedder call.
    pub max_batch: usize,
    /// Files whose vectors are published together, so a long fill keeps
    /// its progress in steps and searches see it as it goes.
    pub files_per_round: usize,
}

impl Default for FillBudget {
    fn default() -> Self {
        Self {
            max_batch: 64,
            files_per_round: 256,
        }
    }
}

/// Vector bytes one round collects at most before it publishes, well under
/// the 256 MiB a segment may hold.
const ROUND_BYTES: usize = 64 << 20;

/// How long a fill waits for another process to release the store's
/// writer, as a build does.
const WRITER_WAIT: Duration = Duration::from_secs(60);

/// How far a fill has come, reported after each round it publishes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct FillProgress {
    /// Files this fill has finished, of the files it set out to fill.
    pub files_done: usize,
    pub files_total: usize,
    /// Texts embedded so far.
    pub chunks_embedded: usize,
}

/// What one fill did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct FillReport {
    /// Member files meaning search applies to, and how many of them are
    /// within `semantic.max_files`.
    pub files: usize,
    pub files_admitted: usize,
    /// Admitted files whose chunks some checkout had already embedded.
    pub files_already_embedded: usize,
    /// Files this fill set out to fill.
    pub files_queued: usize,
    /// Files whose chunk maps this fill published.
    pub files_filled: usize,
    /// Of those, files that could not be cut into chunks (recorded).
    pub files_failed: usize,
    /// Files whose bytes on disk no longer match the checkout's state, left
    /// for the fill after the change is recorded.
    pub files_changed: usize,
    /// Files not finished when the fill stopped early.
    pub files_left: usize,
    /// Chunks of the files this fill cut, how many of their texts already
    /// had vectors, how many it embedded, and how many the embedder refused.
    pub chunks: usize,
    pub chunks_reused: usize,
    pub chunks_embedded: usize,
    pub chunks_refused: usize,
    pub embed_calls: usize,
    pub segments_published: usize,
    pub segment_bytes: u64,
    pub read_ms: f64,
    pub chunk_ms: f64,
    pub embed_ms: f64,
    pub publish_ms: f64,
    /// Why the fill stopped before finishing; what it published stands.
    pub error: Option<String>,
}

/// One checkout's meaning search: its fill and its searches.
pub struct VectorPlane {
    checkout: Arc<Checkout>,
    producer: Producer,
    max_files: usize,
    loaded: Mutex<Arc<Loaded>>,
    members: Mutex<Option<Arc<Members>>>,
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

impl VectorPlane {
    /// The plane of `checkout` for `producer`, filling at most `max_files`
    /// files (`semantic.max_files`).
    #[must_use]
    pub fn new(checkout: Arc<Checkout>, producer: Producer, max_files: usize) -> Self {
        Self {
            checkout,
            producer,
            max_files,
            loaded: Mutex::new(Arc::new(Loaded::empty())),
            members: Mutex::new(None),
        }
    }

    /// The family's newest generation, read again only when a newer one
    /// has been published.
    fn load(&self) -> Result<Arc<Loaded>, DataError> {
        let parts = self.checkout.vector_parts();
        let newest = parts.family.newest_generation().map_err(store_error)?;
        let mut loaded = self.loaded.lock().expect("no panics while holding it");
        if loaded.generation >= newest {
            return Ok(Arc::clone(&loaded));
        }
        let read = Arc::new(Loaded::read(&parts, &self.producer.model, &loaded)?);
        *loaded = Arc::clone(&read);
        Ok(read)
    }

    /// The members of `view` under `loaded`, built once per view state and
    /// generation.
    fn members(&self, view: &CheckoutView, loaded: &Loaded) -> Result<Arc<Members>, DataError> {
        let key = view.text_generation();
        let mut cached = self.members.lock().expect("no panics while holding it");
        if let Some(members) = cached
            .as_ref()
            .filter(|members| members.view == key && members.generation == loaded.generation)
        {
            return Ok(Arc::clone(members));
        }
        let members = Arc::new(Members::build(view, loaded, self.max_files)?);
        *cached = Some(Arc::clone(&members));
        Ok(members)
    }

    /// How much of `view` meaning search covers now.
    ///
    /// # Errors
    ///
    /// When the store cannot be read.
    pub fn coverage(&self, view: &CheckoutView) -> Result<Coverage, DataError> {
        let loaded = self.load()?;
        Ok(self.members(view, &loaded)?.coverage.clone())
    }

    /// `view`'s meaning search over the generation of the family's vectors
    /// this plane read last, without looking for a newer one: reading the
    /// family's manifest on every search would cost a directory listing and
    /// a file read per call. A fill reads the newest generation as it starts
    /// and after each round it publishes; [`Self::refresh`] reads it on
    /// demand.
    ///
    /// # Errors
    ///
    /// When the store cannot be read.
    pub fn snapshot(&self, view: &CheckoutView) -> Result<MeaningSnapshot, DataError> {
        let loaded = Arc::clone(&self.loaded.lock().expect("no panics while holding it"));
        let members = self.members(view, &loaded)?;
        Ok(MeaningSnapshot {
            loaded,
            members,
            root: view.root().to_path_buf(),
            dimensions: self.producer.model.dimensions as usize,
        })
    }

    /// Reads the family's newest generation, if it is newer than the one
    /// the plane read last.
    ///
    /// # Errors
    ///
    /// When the store cannot be read.
    pub fn refresh(&self) -> Result<(), DataError> {
        self.load().map(drop)
    }

    /// The producer whose vectors this plane reads and fills.
    #[must_use]
    pub fn producer(&self) -> &Producer {
        &self.producer
    }

    /// The `top_k` chunks of `view` closest to `query`, among files whose
    /// absolute path `include` accepts: today's `search_filtered` answer
    /// over the same vectors, as its rows, with the checkout's coverage.
    /// As today, a query of another dimension or with a component that is
    /// not finite finds nothing.
    ///
    /// # Errors
    ///
    /// When the store cannot be read.
    pub fn search(
        &self,
        view: &CheckoutView,
        query: &[f32],
        top_k: usize,
        include: &dyn Fn(&Path) -> bool,
    ) -> Result<MeaningAnswer, DataError> {
        self.refresh()?;
        let snapshot = self.snapshot(view)?;
        Ok(MeaningAnswer {
            results: snapshot.search(query, top_k, include)?,
            coverage: snapshot.coverage().clone(),
        })
    }
}

/// One file a fill round cut: each chunk with the digest of its text, or
/// why the file could not be cut.
struct RoundFile {
    path: Vec<u8>,
    content: ContentKey,
    chunks: Result<Vec<(TextDigest, CutChunk)>, String>,
}

/// Waits up to [`WRITER_WAIT`] for the family's writer.
fn writer(family: &Family) -> Result<WriteSession<'_>, DataError> {
    let deadline = Instant::now() + WRITER_WAIT;
    loop {
        if let Some(session) = family.try_write().map_err(store_error)? {
            return Ok(session);
        }
        if Instant::now() >= deadline {
            return Err(DataError::Busy(format!(
                "another process held the store's writer for over {} s",
                WRITER_WAIT.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl VectorPlane {
    /// Embeds the chunks the checkout's current files need and no checkout
    /// has embedded: within the first `semantic.max_files` files meaning
    /// search applies to, in path order, each file whose chunk map is
    /// missing or has chunks with neither a vector nor a recorded failure.
    /// Each file is read from disk and cut into today's chunks; only texts
    /// with no stored vector are sent to `embed`, in batches of
    /// `budget.max_batch`. Files are published in rounds, each as one new
    /// segment in a generation that changes no checkout's state, and
    /// `progress` hears after each round.
    ///
    /// A file whose bytes no longer match the checkout's state is left for
    /// the next fill. A text the embedder refuses is recorded as failed
    /// with its reason, and a file that cannot be cut is recorded with
    /// why; neither is tried again under this model. When `embed` fails,
    /// the fill publishes the files it finished and stops, reporting the
    /// error; the rest stay pending.
    ///
    /// # Errors
    ///
    /// When the store cannot be read or written, or the embedder returns
    /// vectors of another dimension than the model's.
    pub fn fill(
        &self,
        embed: &mut dyn FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        budget: FillBudget,
        progress: &mut dyn FnMut(FillProgress),
    ) -> Result<FillReport, DataError> {
        self.fill_rounds(embed, budget, usize::MAX, progress)
    }

    /// [`Self::fill`], stopping after `rounds` rounds; the files it did not
    /// reach are counted in [`FillReport::files_left`]. A running AFT fills
    /// one round per job, so other work on the root runs between rounds.
    ///
    /// # Errors
    ///
    /// As [`Self::fill`].
    pub fn fill_rounds(
        &self,
        embed: &mut dyn FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
        budget: FillBudget,
        rounds: usize,
        progress: &mut dyn FnMut(FillProgress),
    ) -> Result<FillReport, DataError> {
        let mut report = FillReport::default();
        let Some(view) = self.checkout.view() else {
            return Ok(report);
        };
        let parts = self.checkout.vector_parts();
        let model = &self.producer.model;
        let dimensions = model.dimensions as usize;
        let mut loaded = self.load()?;
        let queue = {
            let index = loaded.shared.as_ref().map(SharedVectorIndex::index);
            let surveyed = survey(
                view.current_state(),
                &loaded,
                index.as_ref(),
                self.max_files,
            )?;
            report.files = surveyed.len();
            let mut queue = Vec::new();
            for file in surveyed {
                if !file.admitted {
                    continue;
                }
                report.files_admitted += 1;
                // Too large or unreadable: nothing to embed.
                if !file.kind.has_content() {
                    continue;
                }
                if file.done(index.as_ref()) {
                    report.files_already_embedded += 1;
                } else {
                    queue.push((file.path, file.content));
                }
            }
            queue
        };
        report.files_queued = queue.len();
        let per_vector = VectorSectionsBuilder::bytes_per_vector(dimensions);
        let mut embed_rows = |texts: Vec<String>| embed(texts);
        let mut queue = queue.into_iter().peekable();
        let mut files_done = 0;
        let mut rounds_run = 0;
        while queue.peek().is_some() {
            if rounds_run == rounds {
                report.files_left += queue.count();
                break;
            }
            rounds_run += 1;
            let current = Arc::clone(&loaded);
            let index = current.shared.as_ref().map(SharedVectorIndex::index);
            let stored = |digest: &TextDigest| {
                index
                    .as_ref()
                    .is_some_and(|index| index.locate(*digest).is_some())
            };

            // Cut the round's files, and collect the texts no segment holds.
            let mut round = Vec::new();
            let mut wanted: Vec<(TextDigest, String)> = Vec::new();
            let mut seen = HashSet::new();
            while round.len() < budget.files_per_round.max(1)
                && wanted.len().saturating_mul(per_vector) < ROUND_BYTES
            {
                let Some((path, content)) = queue.next() else {
                    break;
                };
                let started = Instant::now();
                let bytes = native_path(&path)
                    .map(|relative| crate::project_content::read(parts.root.join(relative)));
                report.read_ms += elapsed_ms(started);
                let bytes = match bytes {
                    Some(Ok(bytes)) if ContentKey::of(&bytes) == content => bytes,
                    _ => {
                        report.files_changed += 1;
                        files_done += 1;
                        continue;
                    }
                };
                let started = Instant::now();
                let cut = cut_file(parts.root, &path, &bytes, self.producer.caps);
                report.chunk_ms += elapsed_ms(started);
                let chunks = match cut {
                    Cut::Failed(reason) => Err(reason),
                    Cut::Chunks(chunks) => Ok(chunks
                        .into_iter()
                        .map(|chunk| {
                            let digest = TextDigest::of(chunk.text.as_bytes());
                            report.chunks += 1;
                            if stored(&digest) {
                                report.chunks_reused += 1;
                            } else if seen.insert(digest) {
                                wanted.push((digest, chunk.text.clone()));
                            }
                            (digest, chunk)
                        })
                        .collect()),
                };
                round.push(RoundFile {
                    path,
                    content,
                    chunks,
                });
            }

            // Embed them, each distinct text once.
            let mut vectors: HashMap<TextDigest, Vec<f32>> = HashMap::new();
            let mut refused: HashMap<TextDigest, String> = HashMap::new();
            let started = Instant::now();
            for batch in wanted.chunks(budget.max_batch.max(1)) {
                report.embed_calls += 1;
                let texts = batch.iter().map(|(_, text)| text.clone()).collect();
                let rows = match execute_build_embedding_batch(texts, &mut embed_rows) {
                    Ok(rows) => rows,
                    Err(error) => {
                        report.error = Some(error);
                        break;
                    }
                };
                for ((digest, _), row) in batch.iter().zip(rows) {
                    match row {
                        BuildEmbeddingRow::Embedded { vector, .. } => {
                            if vector.len() != dimensions {
                                return Err(DataError::Store(format!(
                                    "the embedder returned {} dimensions; the model has {dimensions}",
                                    vector.len()
                                )));
                            }
                            vectors.insert(*digest, vector);
                            report.chunks_embedded += 1;
                        }
                        BuildEmbeddingRow::Skipped { reason, .. } => {
                            refused.insert(*digest, reason);
                            report.chunks_refused += 1;
                        }
                    }
                }
            }
            report.embed_ms += elapsed_ms(started);

            // Publish every file whose texts all have a vector or a refusal.
            let mut builder = VectorSectionsBuilder::new(model.clone()).map_err(store_error)?;
            for (digest, vector) in &vectors {
                builder.add_vector(*digest, vector).map_err(store_error)?;
            }
            let mut filled = 0;
            for file in &round {
                let details = match &file.chunks {
                    Err(reason) => {
                        report.files_failed += 1;
                        builder
                            .add_chunk_map_with_details(
                                file.content,
                                &file.path,
                                &[],
                                FileDetails {
                                    failure: Some(reason.clone()),
                                    chunks: Vec::new(),
                                },
                            )
                            .map_err(store_error)?;
                        filled += 1;
                        continue;
                    }
                    Ok(chunks) => chunks,
                };
                let resolved = details.iter().all(|(digest, _)| {
                    stored(digest) || vectors.contains_key(digest) || refused.contains_key(digest)
                });
                if !resolved {
                    report.files_left += 1;
                    continue;
                }
                let mut map = Vec::with_capacity(details.len());
                let mut shown = Vec::with_capacity(details.len());
                for (digest, chunk) in details {
                    let failure = (!stored(digest) && !vectors.contains_key(digest))
                        .then(|| refused[digest].clone());
                    map.push(Chunk {
                        text: *digest,
                        exported: chunk.exported,
                        failed: failure.is_some(),
                    });
                    shown.push(ChunkDetail {
                        failure,
                        ..chunk.detail.clone()
                    });
                }
                builder
                    .add_chunk_map_with_details(
                        file.content,
                        &file.path,
                        &map,
                        FileDetails {
                            failure: None,
                            chunks: shown,
                        },
                    )
                    .map_err(store_error)?;
                filled += 1;
            }
            files_done += filled;
            drop(index);
            if builder.chunk_maps() > 0 || builder.vectors() > 0 {
                let started = Instant::now();
                report.segment_bytes += Self::publish(&parts, builder)?;
                report.publish_ms += elapsed_ms(started);
                report.segments_published += 1;
                report.files_filled += filled;
                loaded = self.load()?;
            }
            progress(FillProgress {
                files_done,
                files_total: report.files_queued,
                chunks_embedded: report.chunks_embedded,
            });
            if report.error.is_some() {
                report.files_left += queue.count();
                break;
            }
        }
        Ok(report)
    }

    /// Publishes `builder` as one new segment, in a generation that
    /// changes no checkout's state. Returns the segment's length.
    fn publish(parts: &VectorParts<'_>, builder: VectorSectionsBuilder) -> Result<u64, DataError> {
        let mut session = writer(parts.family)?;
        let mut pending = session.create_segment().map_err(store_error)?;
        builder.write(pending.writer()).map_err(store_error)?;
        let id = session.finish_segment(pending).map_err(store_error)?;
        session
            .publish(&Publication {
                add_segments: vec![id],
                ..Publication::default()
            })
            .map_err(store_error)?;
        Ok(session.base().segment(id).map_or(0, |entry| entry.len))
    }
}

/// Which of `segments` (one generation's, in manifest order) hold vector
/// units a reader of `states` needs, by the rule readers follow: for each
/// member of each state, under each model, the newest segment holding the
/// member's chunk map, and for each chunk of that map the newest segment
/// holding its text's vector. A segment whose vectors and maps no member
/// reaches is not needed, so vectors of contents no checkout holds any
/// more are retired with their segment. Segments without vector sections
/// are left to the other planes' rules.
///
/// # Errors
///
/// When a segment cannot be read.
pub(crate) fn needed_vector_segments(
    segments: &[&Segment],
    states: &[&CheckoutState],
) -> Result<Vec<bool>, DataError> {
    let mut needed = vec![false; segments.len()];
    let opened = segments
        .iter()
        .map(|segment| VectorSegment::open(segment).map_err(store_error))
        .collect::<Result<Vec<_>, _>>()?;
    // Newest holders, by model: later segments replace earlier ones.
    let mut maps: HashMap<(ModelId, ChunkMapKey), usize> = HashMap::new();
    let mut texts: HashMap<(ModelId, TextDigest), usize> = HashMap::new();
    let mut models = Vec::new();
    for (position, opened) in opened.iter().enumerate() {
        let Some(vectors) = opened else {
            continue;
        };
        let model = vectors.model().id();
        if !models.contains(&model) {
            models.push(model);
        }
        for key in vectors.chunk_map_keys().map_err(store_error)? {
            maps.insert((model, key), position);
        }
        for text in vectors.texts().map_err(store_error)? {
            texts.insert((model, text), position);
        }
    }
    if maps.is_empty() {
        return Ok(needed);
    }
    let mut asked = HashSet::new();
    for state in states {
        for record in state.paths() {
            if !record.kind.has_content() {
                continue;
            }
            let key = ChunkMapKey::new(record.content, &record.path);
            if !asked.insert(key) {
                continue;
            }
            for model in &models {
                let Some(&holder) = maps.get(&(*model, key)) else {
                    continue;
                };
                needed[holder] = true;
                let chunks = opened[holder]
                    .as_ref()
                    .expect("a map holder has vector sections")
                    .chunk_map(record.content, &record.path)
                    .map_err(store_error)?
                    .unwrap_or_default();
                for chunk in chunks {
                    if let Some(&text_holder) = texts.get(&(*model, chunk.text)) {
                        needed[text_holder] = true;
                    }
                }
            }
        }
    }
    Ok(needed)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use aft_store::ids::{Digest128, FamilyId};

    use super::*;
    use crate::data_layer::checkout::DataLayer;
    use crate::semantic_index::SemanticIndex;

    const DIMENSIONS: usize = 16;

    fn family() -> FamilyId {
        FamilyId::from_digest(Digest128::of(b"vector plane tests"))
    }

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn producer() -> Producer {
        Producer {
            model: ModelIdentity {
                name: "test/hashed".to_string(),
                revision: "1".to_string(),
                dimensions: DIMENSIONS as u32,
                normalized: false,
                text_template: TEXT_TEMPLATE,
                chunk_template: 2,
            },
            caps: EmbedTextCaps::default(),
        }
    }

    /// Every text's vector follows from its digest. About one text in four
    /// gets one shared vector, so equal scores occur across files and
    /// within one, and their order is tested too.
    fn vector_of(text: &str) -> Vec<f32> {
        let hash = blake3::hash(text.as_bytes());
        let bytes = hash.as_bytes();
        if bytes[0].is_multiple_of(4) {
            return (0..DIMENSIONS).map(|i| 0.25 + i as f32 / 32.0).collect();
        }
        (0..DIMENSIONS)
            .map(|i| (f32::from(bytes[i]) - 127.5) / 50.0)
            .collect()
    }

    /// An embedder that records every text it is sent.
    #[derive(Default)]
    struct Recorder {
        texts: Vec<String>,
    }

    impl Recorder {
        fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
            let vectors = texts.iter().map(|text| vector_of(text)).collect();
            self.texts.extend(texts);
            Ok(vectors)
        }
    }

    /// Fills `plane` with `recorder`, one round per `files_per_round`.
    fn fill(plane: &VectorPlane, recorder: &mut Recorder, files_per_round: usize) -> FillReport {
        plane
            .fill(
                &mut |texts| recorder.embed(texts),
                FillBudget {
                    max_batch: 5,
                    files_per_round,
                },
                &mut |_| {},
            )
            .unwrap()
    }

    /// A small project in four languages, with exported and private
    /// symbols, tests, and copies of one function so equal texts and
    /// vectors occur.
    fn project(root: &Path) {
        write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(
            root,
            "src/lib.rs",
            "//! A demo library.\nmod util;\n\n/// Opens the store.\npub fn open_store(path: &str) -> usize {\n    let size = path.len();\n    size * 2\n}\n\nfn private_helper(x: u32) -> u32 {\n    x + 1\n}\n\npub struct Config {\n    pub name: String,\n    pub size: usize,\n}\n\nimpl Config {\n    pub fn new(name: &str) -> Self {\n        Self { name: name.to_string(), size: 0 }\n    }\n\n    fn grow(&mut self) {\n        self.size += 1;\n    }\n}\n",
        );
        write(
            root,
            "src/util.rs",
            "pub fn callee() {\n    println!(\"called\");\n}\n\npub fn twin() {\n    println!(\"twin\");\n}\n\npub fn twin_again() {\n    println!(\"twin\");\n}\n",
        );
        write(
            root,
            "web/app.ts",
            "import { load } from './config';\n\nexport class App {\n  run(): number {\n    return load().start();\n  }\n}\n\nexport const make = () => {\n  return new App();\n};\n\nfunction hidden(a: number) {\n  return a * 2;\n}\n",
        );
        write(
            root,
            "tool/main.py",
            "import os\n\nclass Tool:\n    def run(self):\n        return os.getcwd()\n\n    def stop(self):\n        return None\n\ndef helper(value):\n    return value + 1\n",
        );
        write(
            root,
            "deploy/app.yaml",
            "apiVersion: v1\nkind: Service\nmetadata:\n  name: demo\nspec:\n  ports:\n    - port: 80\n",
        );
        write(
            root,
            "tests/store_test.rs",
            "#[test]\nfn opens_the_store() {\n    assert_eq!(demo::open_store(\"ab\"), 4);\n}\n",
        );
        write(root, "README.md", "# Demo\n\nNothing to embed here.\n");
    }

    fn bind(store: &Path, root: &Path) -> Arc<Checkout> {
        let checkout =
            Checkout::bind(Arc::new(DataLayer::new(8 << 20, 2)), store, family(), root).unwrap();
        checkout.build().unwrap();
        Arc::new(checkout)
    }

    /// The fields of a result row, with the score's bits.
    fn row(result: &SemanticResult) -> String {
        format!(
            "{} {} {:?} {:?} {}-{} {} {:08x} {:08x} {} {} {:?}",
            result.file.display(),
            result.name,
            result.qualified_name,
            result.kind,
            result.start_line,
            result.end_line,
            result.exported,
            result.score.to_bits(),
            result.rank_score.to_bits(),
            result.cap_protected,
            result.source,
            result.snippet,
        )
    }

    fn rows(results: &[SemanticResult]) -> Vec<String> {
        results.iter().map(row).collect()
    }

    fn queries() -> Vec<Vec<f32>> {
        let mut queries: Vec<Vec<f32>> = (0..12u8)
            .map(|seed| {
                let hash = blake3::hash(&[seed]);
                (0..DIMENSIONS)
                    .map(|i| (f32::from(hash.as_bytes()[i]) - 127.5) / 40.0)
                    .collect()
            })
            .collect();
        // The shared vector itself: every chunk holding it ties for first.
        queries.push(vector_of_tie());
        // A zero query scores every chunk 0.0: the whole order is ties.
        queries.push(vec![0.0; DIMENSIONS]);
        queries
    }

    fn vector_of_tie() -> Vec<f32> {
        (0..DIMENSIONS).map(|i| 0.25 + i as f32 / 32.0).collect()
    }

    #[test]
    fn a_view_search_returns_what_todays_index_returns_over_the_same_vectors() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let store = tempfile::tempdir().unwrap();
        project(&root);
        let checkout = bind(store.path(), &root);
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 20_000);
        let mut recorder = Recorder::default();
        let report = fill(&plane, &mut recorder, 2);
        assert_eq!(report.error, None);
        assert!(report.segments_published >= 3, "{report:?}");
        let view = checkout.view().unwrap();
        let coverage = plane.coverage(&view).unwrap();
        assert!(coverage.complete(), "{coverage:?}");
        assert_eq!(coverage.files, 7, "{coverage:?}");
        assert_eq!(coverage.files_embedded, 7);
        assert_eq!(coverage.chunks, coverage.chunks_embedded);

        // Today's index, built from the same files in the same order with
        // the same embedder.
        let files: Vec<PathBuf> = view
            .current_state()
            .paths()
            .iter()
            .filter(|record| is_semantic_path(&record.path))
            .map(|record| root.join(native_path(&record.path).unwrap()))
            .collect();
        let mut today_embedder = Recorder::default();
        let today =
            SemanticIndex::build(&root, &files, &mut |texts| today_embedder.embed(texts), 5)
                .unwrap();
        assert_eq!(today.len(), coverage.chunks_embedded);
        // Both embedded the same texts, so equal vectors.
        let embedded: BTreeSet<&String> = recorder.texts.iter().collect();
        let today_texts: BTreeSet<&String> = today_embedder.texts.iter().collect();
        assert_eq!(embedded, today_texts);
        let tied = recorder
            .texts
            .iter()
            .filter(|text| vector_of(text) == vector_of_tie())
            .count();
        assert!(tied >= 3, "the fixture must produce equal scores: {tied}");

        let tests_dir = root.join("tests");
        let src_dir = root.join("src");
        let filters: [(&str, Box<dyn Fn(&Path) -> bool>); 4] = [
            ("everything", Box::new(|_| true)),
            (
                "no tests",
                Box::new(move |file| !file.starts_with(&tests_dir)),
            ),
            ("src only", Box::new(move |file| file.starts_with(&src_dir))),
            ("nothing", Box::new(|_| false)),
        ];
        let mut compared = 0;
        for (label, include) in &filters {
            for query in queries() {
                for k in [1, 3, 8, 1000] {
                    let expected = today.search_filtered(&query, k, |file| include(file));
                    let found = plane.search(&view, &query, k, &**include).unwrap();
                    assert_eq!(
                        rows(&found.results),
                        rows(&expected),
                        "{label}, k {k}, query {query:?}"
                    );
                    compared += expected.len();
                }
            }
        }
        assert!(compared > 500, "{compared} rows compared");

        // As today, queries the model cannot score find nothing.
        let mut bad = vector_of_tie();
        bad[3] = f32::NAN;
        for query in [bad, vec![1.0; DIMENSIONS + 1]] {
            assert!(today.search(&query, 5).is_empty());
            assert!(plane
                .search(&view, &query, 5, &|_| true)
                .unwrap()
                .results
                .is_empty());
        }
        assert!(plane
            .search(&view, &vector_of_tie(), 0, &|_| true)
            .unwrap()
            .results
            .is_empty());

        // A plane bound afresh reads the published vectors without
        // embedding anything, and answers the same.
        let again = VectorPlane::new(Arc::clone(&checkout), producer(), 20_000);
        let mut idle = Recorder::default();
        let report = fill(&again, &mut idle, 2);
        assert!(idle.texts.is_empty(), "{report:?}");
        assert_eq!(report.files_already_embedded, 7);
        for query in queries() {
            assert_eq!(
                rows(&again.search(&view, &query, 10, &|_| true).unwrap().results),
                rows(&today.search(&query, 10)),
            );
        }
    }

    /// Every chunk text of the semantic files under `root`, as a fill cuts
    /// them.
    fn texts_under(root: &Path, state: &CheckoutState) -> BTreeSet<String> {
        let mut texts = BTreeSet::new();
        for record in state.paths() {
            if !is_semantic_path(&record.path) || !record.kind.has_content() {
                continue;
            }
            let bytes = std::fs::read(root.join(native_path(&record.path).unwrap())).unwrap();
            if let Cut::Chunks(chunks) =
                cut_file(root, &record.path, &bytes, EmbedTextCaps::default())
            {
                texts.extend(chunks.into_iter().map(|chunk| chunk.text));
            }
        }
        texts
    }

    /// Copies the files of `from` into `to`.
    fn copy_tree(from: &Path, to: &Path) {
        for entry in walkdir_files(from) {
            let relative = entry.strip_prefix(from).unwrap();
            let target = to.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(&entry, target).unwrap();
        }
    }

    fn walkdir_files(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(walkdir_files(&path));
            } else {
                files.push(path);
            }
        }
        files
    }

    #[test]
    fn a_second_checkout_embeds_only_what_no_checkout_embedded() {
        let store = tempfile::tempdir().unwrap();
        let first_root = tempfile::tempdir().unwrap();
        let first_root = std::fs::canonicalize(first_root.path()).unwrap();
        project(&first_root);
        let first = bind(store.path(), &first_root);
        let first_plane = VectorPlane::new(Arc::clone(&first), producer(), 20_000);
        let mut recorder = Recorder::default();
        fill(&first_plane, &mut recorder, 3);
        let first_texts = texts_under(&first_root, first.view().unwrap().current_state());
        let embedded: BTreeSet<String> = recorder.texts.iter().cloned().collect();
        assert_eq!(embedded, first_texts);
        assert_eq!(recorder.texts.len(), first_texts.len(), "each text once");

        // The same project at another commit: one function's body edited,
        // a file added and a file deleted.
        let second_root = tempfile::tempdir().unwrap();
        let second_root = std::fs::canonicalize(second_root.path()).unwrap();
        copy_tree(&first_root, &second_root);
        let lib = std::fs::read_to_string(second_root.join("src/lib.rs")).unwrap();
        write(&second_root, "src/lib.rs", &lib.replace("x + 1", "x + 2"));
        write(
            &second_root,
            "src/extra.rs",
            "pub fn extra_work(items: &[u8]) -> usize {\n    items.len()\n}\n",
        );
        std::fs::remove_file(second_root.join("tool/main.py")).unwrap();
        let second = bind(store.path(), &second_root);
        let second_plane = VectorPlane::new(Arc::clone(&second), producer(), 20_000);
        let mut recorder = Recorder::default();
        let report = fill(&second_plane, &mut recorder, 3);
        let second_texts = texts_under(&second_root, second.view().unwrap().current_state());
        let expected: BTreeSet<String> = second_texts.difference(&first_texts).cloned().collect();
        let embedded: BTreeSet<String> = recorder.texts.iter().cloned().collect();
        assert_eq!(embedded, expected, "{report:?}");
        // Only the edited function's chunk and the new file's: the edited
        // file's other chunks are shared.
        assert!(!expected.is_empty() && expected.len() <= 4, "{expected:?}");
        assert_eq!(report.files_queued, 2, "{report:?}");
        assert!(report.chunks_reused > 0, "{report:?}");
        assert!(second_plane
            .coverage(&second.view().unwrap())
            .unwrap()
            .complete());

        // Building either checkout again keeps the vectors the other reads:
        // neither fill embeds anything.
        first.build().unwrap();
        second.build().unwrap();
        for (plane, checkout) in [(&first_plane, &first), (&second_plane, &second)] {
            let mut idle = Recorder::default();
            fill(plane, &mut idle, 3);
            assert!(idle.texts.is_empty(), "{:?}", idle.texts);
            let view = checkout.view().unwrap();
            assert!(plane.coverage(&view).unwrap().complete());
            assert!(!plane
                .search(&view, &vector_of_tie(), 3, &|_| true)
                .unwrap()
                .results
                .is_empty());
        }
    }

    #[test]
    fn vector_segments_no_checkout_needs_are_retired() {
        let store = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        write(
            &root,
            "src/only.rs",
            "pub fn first_version(a: u8) -> u8 {\n    a\n}\n",
        );
        let checkout = bind(store.path(), &root);
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 20_000);
        fill(&plane, &mut Recorder::default(), 10);
        let family_store = aft_store::family::Family::open(store.path(), family()).unwrap();
        let vector_segments = || {
            let snapshot = family_store.snapshot().unwrap();
            let known = checkout.vector_parts().known;
            snapshot
                .manifest()
                .segments()
                .iter()
                .filter(|entry| {
                    let segment = snapshot
                        .open_segment(entry.id, &known, Arc::new(BlockCache::new(1 << 20)))
                        .unwrap();
                    VectorSegment::open(&segment).unwrap().is_some()
                })
                .map(|entry| entry.id)
                .collect::<Vec<_>>()
        };
        let before = vector_segments();
        assert_eq!(before.len(), 1);
        // The only file changes: no checkout holds the old content, so its
        // vectors go with the next build.
        write(
            &root,
            "src/only.rs",
            "pub fn second_version(b: u16) -> u16 {\n    b\n}\n",
        );
        let report = checkout.build().unwrap();
        assert!(report.retired_segments >= 1, "{report:?}");
        assert!(vector_segments().is_empty());
        // A checkout still holding a content keeps its vectors.
        let mut recorder = Recorder::default();
        fill(&plane, &mut recorder, 10);
        assert!(!recorder.texts.is_empty());
        let kept = vector_segments();
        checkout.build().unwrap();
        assert_eq!(vector_segments(), kept);
    }

    #[test]
    fn coverage_and_progress_are_reported_honestly() {
        let store = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        // In path order: a.rs, b.rs (not UTF-8), c.rs (one chunk the
        // embedder refuses), d.yaml (too large), e.rs and f.rs.
        write(&root, "a.rs", "pub fn alpha() {\n    one();\n}\n");
        std::fs::write(root.join("b.rs"), b"pub fn beta() {\n    \xff\xfe();\n}\n").unwrap();
        write(
            &root,
            "c.rs",
            "pub fn refuse_me() {\n    two();\n}\n\npub fn gamma() {\n    three();\n}\n",
        );
        let line = "key: value with some words in it\n";
        write(&root, "d.yaml", &line.repeat((5 << 20) / line.len()));
        write(&root, "e.rs", "pub fn epsilon() {\n    five();\n}\n");
        write(&root, "f.rs", "pub fn phi() {\n    six();\n}\n");
        let checkout = bind(store.path(), &root);
        let view = checkout.view().unwrap();
        let path = |name: &str| root.join(name);

        // Three files admitted; nothing embedded yet.
        let capped = VectorPlane::new(Arc::clone(&checkout), producer(), 3);
        let coverage = capped.coverage(&view).unwrap();
        assert_eq!(coverage.files, 6);
        assert_eq!(
            coverage.pending,
            vec![path("a.rs"), path("b.rs"), path("c.rs")]
        );
        assert_eq!(coverage.files_too_large, 1, "{coverage:?}");
        assert_eq!(coverage.past_cap, 2);
        assert_eq!((coverage.chunks, coverage.chunks_embedded), (0, 0));
        assert!(!coverage.complete());
        assert!(capped
            .search(&view, &vector_of_tie(), 5, &|_| true)
            .unwrap()
            .results
            .is_empty());

        // The embedder fails after its first batch: what was finished is
        // published and the rest stays pending.
        let mut calls = 0;
        let mut failing = |texts: Vec<String>| {
            calls += 1;
            if calls > 1 {
                return Err("backend unreachable".to_string());
            }
            Ok(texts.iter().map(|text| vector_of(text)).collect())
        };
        let report = capped
            .fill(
                &mut failing,
                FillBudget {
                    max_batch: 1,
                    files_per_round: 10,
                },
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(report.error.as_deref(), Some("backend unreachable"));
        assert!(report.files_left >= 1, "{report:?}");
        // The file that cannot be chunked needs no embedder: it is recorded.
        assert_eq!(report.files_failed, 1, "{report:?}");
        let coverage = capped.coverage(&view).unwrap();
        assert!(!coverage.pending.is_empty(), "{coverage:?}");

        // A fill that refuses one text, one file per round.
        let mut progress = Vec::new();
        let mut refusing = |texts: Vec<String>| {
            // Only the symbol's own chunk: the file's summary names it too.
            let refused = |text: &str| text.starts_with("name:refuse_me ");
            crate::semantic_index::report_build_rows_for_test(&texts, |text| {
                refused(text).then(|| "provider refused the input".to_string())
            });
            Ok::<_, String>(
                texts
                    .iter()
                    .map(|text| {
                        if refused(text) {
                            Vec::new()
                        } else {
                            vector_of(text)
                        }
                    })
                    .collect(),
            )
        };
        let report = capped
            .fill(
                &mut refusing,
                FillBudget {
                    max_batch: 8,
                    files_per_round: 1,
                },
                &mut |step| progress.push(step),
            )
            .unwrap();
        assert_eq!(report.error, None, "{report:?}");
        assert_eq!(report.files_admitted, 3);
        assert_eq!(report.chunks_refused, 1, "{report:?}");
        assert_eq!(
            report.files_queued, 2,
            "b.rs was recorded by the first fill"
        );
        assert_eq!(
            progress.last().map(|step| step.files_done),
            Some(report.files_queued)
        );
        assert!(progress
            .windows(2)
            .all(|pair| pair[0].files_done <= pair[1].files_done));
        let coverage = capped.coverage(&view).unwrap();
        assert!(coverage.pending.is_empty(), "{coverage:?}");
        assert_eq!(coverage.past_cap, 2);
        assert_eq!(coverage.files_failed.len(), 1, "{coverage:?}");
        assert_eq!(coverage.files_failed[0].0, path("b.rs"));
        assert!(
            coverage.files_failed[0].1.to_lowercase().contains("utf-8"),
            "{coverage:?}"
        );
        assert_eq!(
            coverage.chunks_failed,
            vec![(
                path("c.rs"),
                "refuse_me".to_string(),
                "provider refused the input".to_string()
            )]
        );
        assert_eq!(coverage.files_embedded, 2, "a.rs and c.rs");
        assert_eq!(coverage.chunks, coverage.chunks_embedded + 1);
        assert!(!coverage.complete(), "files past the cap are not embedded");
        // The refused chunk is never returned, and never sent again.
        let results = capped
            .search(&view, &vector_of_tie(), 100, &|_| true)
            .unwrap()
            .results;
        assert!(results.iter().all(|result| result.name != "refuse_me"));
        let mut recorder = Recorder::default();
        fill(&capped, &mut recorder, 10);
        assert!(recorder.texts.is_empty(), "{:?}", recorder.texts);

        // With the cap raised, the rest is embedded and coverage completes.
        let open = VectorPlane::new(Arc::clone(&checkout), producer(), 20_000);
        let report = fill(&open, &mut recorder, 10);
        assert_eq!(report.files_queued, 2, "{report:?}");
        let coverage = open.coverage(&view).unwrap();
        assert!(coverage.complete(), "{coverage:?}");
        assert_eq!(coverage.files_embedded, 4);
        // Files past one plane's cap are searched once another embedded them.
        let names: BTreeSet<String> = capped
            .search(&view, &vector_of_tie(), 100, &|_| true)
            .unwrap()
            .results
            .into_iter()
            .map(|result| result.name)
            .collect();
        assert!(
            names.contains("epsilon") && names.contains("phi"),
            "{names:?}"
        );
        assert_eq!(capped.coverage(&view).unwrap().past_cap, 0);
    }

    #[test]
    fn an_edit_is_searched_at_its_new_content_once_filled() {
        let store = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        project(&root);
        let checkout = bind(store.path(), &root);
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 20_000);
        fill(&plane, &mut Recorder::default(), 10);
        write(
            &root,
            "src/util.rs",
            "pub fn renamed_after_edit() {\n    println!(\"edited\");\n}\n",
        );
        checkout.record_paths(&[b"src/util.rs".to_vec()]).unwrap();
        let view = checkout.view().unwrap();
        let coverage = plane.coverage(&view).unwrap();
        assert_eq!(
            coverage.pending,
            vec![root.join("src/util.rs")],
            "{coverage:?}"
        );
        let names = |view: &CheckoutView| {
            plane
                .search(view, &[0.0; DIMENSIONS], 1000, &|_| true)
                .unwrap()
                .results
                .into_iter()
                .map(|result| result.name)
                .collect::<BTreeSet<_>>()
        };
        // The old content's chunks are not the file's any more.
        assert!(!names(&view).contains("twin"));
        let mut recorder = Recorder::default();
        let report = fill(&plane, &mut recorder, 10);
        assert_eq!(report.files_queued, 1, "{report:?}");
        let view = checkout.view().unwrap();
        assert!(plane.coverage(&view).unwrap().complete());
        assert!(names(&view).contains("renamed_after_edit"));
    }
}
