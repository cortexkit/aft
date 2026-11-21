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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aft_store::cache::BlockCache;
use aft_store::checkout::{CheckoutState, PathKind};
use aft_store::family::{Family, Publication, Snapshot, WriteSession};
use aft_store::ids::{ContentKey, Digest128, Generation, SegmentId};
use aft_store::segment::{KnownSection, Segment};
use aft_store::vector::{
    Chunk, ChunkDetail, ChunkMapKey, FileDetails, Member, MemberSet, ModelId, ModelIdentity,
    SearchRequest, SharedVectorIndex, TextDigest, VectorIndex, VectorSectionsBuilder,
    VectorSegment,
};

use super::checkout::{Checkout, CheckoutView, DataError};
use super::old_index::OldIndex;
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

    /// A producer of vectors stored under `model`, cutting with today's
    /// default caps: for measurements that search and merge a family's
    /// stored vectors and embed nothing.
    #[doc(hidden)]
    #[must_use]
    pub fn for_stored_model(model: ModelIdentity) -> Self {
        Self {
            model,
            caps: EmbedTextCaps::default(),
        }
    }
}

/// `path` (relative to the root, `/`-separated) in the platform's own form,
/// as today's index names files and builds their embedded text. `None` for
/// a path this platform cannot name.
pub(super) fn native_path(path: &[u8]) -> Option<PathBuf> {
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
    native_path(path).is_some_and(|path| is_semantic_indexed_extension(&path) || is_luau(&path))
}

fn is_luau(path: &Path) -> bool {
    crate::parser::detect_language_any(path, cfg!(feature = "wasm-grammars"))
        == Some(crate::parser::LangId::Luau)
}

/// Only Luau chunk maps carry grammar identity. Vector text digests and every
/// built-in map keep their existing inputs, so unchanged text stays reusable.
fn map_content(content: ContentKey, path: &[u8]) -> ContentKey {
    if !native_path(path).is_some_and(|path| is_luau(&path)) {
        return content;
    }
    let mut hash = blake3::Hasher::new_derive_key("aft Luau grammar chunk map");
    hash.update(content.digest().as_bytes());
    hash.update(super::content::keyed_language("luau").as_bytes());
    ContentKey::from_digest(aft_store::ids::Digest128::from_hash(&hash.finalize()))
}

pub(super) const LUAU_OUTLINE_FAILED: &str =
    "Meaning search skipped this Luau file because AFT could not extract its outline.";

fn cut_stored(
    view: &CheckoutView,
    path: &[u8],
    bytes: &[u8],
    caps: EmbedTextCaps,
) -> Result<Cut, DataError> {
    let Some(relative) = native_path(path).filter(|path| is_luau(path)) else {
        return Ok(Cut::Failed(LUAU_OUTLINE_FAILED.into()));
    };
    if view
        .current_state()
        .get(path)
        .is_none_or(|record| record.language != "luau")
    {
        return Ok(Cut::Failed(LUAU_OUTLINE_FAILED.into()));
    }
    let Some(outline) = view.outline_symbols(path)? else {
        return Ok(Cut::Failed(LUAU_OUTLINE_FAILED.into()));
    };
    let symbols = outline
        .symbols
        .iter()
        .filter_map(super::content::symbol_from_outline)
        .collect::<Vec<_>>();
    let source = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => return Ok(Cut::Failed(error.to_string())),
    };
    let chunks = crate::semantic_index::symbols_to_chunks_with_caps(
        &view.root().join(relative),
        &symbols,
        source,
        view.root(),
        caps,
    );
    Ok(Cut::Chunks(
        chunks
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
    ))
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
pub(super) fn kind_of(code: u8) -> Option<SymbolKind> {
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
    // A minified bundle's chunks say nothing a search could use: it is a
    // file with nothing to embed, as one the chunker does not read is.
    if std::str::from_utf8(path).is_ok_and(|path| crate::file_kind::is_minified(path, bytes)) {
        return Cut::Chunks(Vec::new());
    }
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
    /// Files excluded by parsing availability or configuration, not fill failures.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_parses: Vec<(PathBuf, String)>,
    /// Member files considered for meaning search, including named exclusions.
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
    /// Whether every source is accounted for without excluded Luau outlines
    /// or unfilled work. Other files that cannot be read or cut into chunks
    /// remain terminal outcomes: they are counted and never queued again for
    /// the same content, preserving the built-in languages' coverage policy.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.pending.is_empty()
            && self.past_cap == 0
            && self.skipped_parses.is_empty()
            && self.luau_outline_failures().next().is_none()
    }

    pub(crate) fn luau_outline_failures(&self) -> impl Iterator<Item = &(PathBuf, String)> {
        self.files_failed
            .iter()
            .filter(|(_, reason)| cfg!(feature = "wasm-grammars") && reason == LUAU_OUTLINE_FAILED)
    }
}

/// One member file of a checkout state that meaning search applies to, as
/// the store holds it.
struct Surveyed {
    gap: Option<String>,
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
    view: &CheckoutView,
    loaded: &Loaded,
    index: Option<&VectorIndex<'_>>,
    max_files: usize,
) -> Result<Vec<Surveyed>, DataError> {
    let state = view.current_state();
    let mut files = Vec::new();
    let mut applicable = 0;
    for record in state.paths() {
        if record.kind == PathKind::Symlink
            || (record.kind == PathKind::Binary
                && native_path(&record.path).is_some_and(|path| is_luau(&path)))
            || !is_semantic_path(&record.path)
        {
            continue;
        }
        let gap = if !view.planes().callgraph
            && native_path(&record.path).is_some_and(|path| is_luau(&path))
        {
            Some(super::grammar::meaning_callgraph_off_reason())
        } else {
            state.skipped_parses().get(&record.path).cloned()
        };
        let admitted = gap.is_none() && applicable < max_files;
        if gap.is_none() {
            applicable += 1;
        }
        let map = match index.filter(|_| gap.is_none()) {
            Some(index) if record.kind.has_content() => {
                loaded.chunk_map(index, record.content, &record.path)?
            }
            _ => None,
        };
        files.push(Surveyed {
            gap,
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
                None => snapshot
                    .open_shared_segment(entry.id, &parts.known, parts.cache)
                    .map_err(store_error)?,
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
        let content = map_content(content, path);
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
        let content = map_content(content, path);
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
    /// Filtered reads replace the subset cache without changing the text view.
    memory_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// The view's text generation, and the generation the members were
    /// read from: the cache key.
    view: String,
    parse_gaps: aft_store::ids::Digest128,
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
    /// The last subset a search kept, with which files it kept: searches
    /// mostly ask for the same files (aft_search leaves tests out unless
    /// asked), and preparing a subset sorts every member.
    subset: Mutex<Option<(Vec<bool>, Arc<Subset>)>>,
}

/// The members of some of the files, prepared for searching, and each one's
/// position among all members.
struct Subset {
    set: MemberSet,
    positions: Vec<usize>,
}

impl Members {
    fn matches(&self, view: &str, parse_gaps: Digest128, generation: Generation) -> bool {
        self.view == view && self.parse_gaps == parse_gaps && self.generation == generation
    }

    /// The members of the files `included` names, by file, prepared for
    /// `index`; kept for the next search that names the same files.
    fn subset(
        &self,
        index: &VectorIndex<'_>,
        included: Vec<bool>,
    ) -> Result<Arc<Subset>, DataError> {
        let mut kept = self.subset.lock().expect("no panics while holding it");
        if let Some((files, subset)) = kept.as_ref().filter(|(files, _)| *files == included) {
            debug_assert_eq!(files.len(), self.files.len());
            return Ok(Arc::clone(subset));
        }
        let mut members = Vec::new();
        let mut positions = Vec::new();
        for (position, (member, file)) in self.members.iter().zip(&self.file_of).enumerate() {
            if included[*file as usize] {
                members.push(*member);
                positions.push(position);
            }
        }
        let subset = Arc::new(Subset {
            set: index.member_set(members).map_err(store_error)?,
            positions,
        });
        *kept = Some((included, Arc::clone(&subset)));
        self.memory_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(subset)
    }

    fn build(view: &CheckoutView, loaded: &Loaded, max_files: usize) -> Result<Self, DataError> {
        #[cfg(test)]
        STATUS_MEMBER_BUILDS.with(|count| count.set(count.get() + 1));
        let index = loaded.shared.as_ref().map(SharedVectorIndex::index);
        let surveyed = survey(view, loaded, index.as_ref(), max_files)?;
        let absolute = |path: &[u8]| {
            view.root()
                .join(native_path(path).expect("surveyed paths have a native form"))
        };
        let mut members = Self {
            memory_epoch: view.memory_epoch_counter(),
            view: view.text_generation(),
            parse_gaps: view.parse_gaps_identity(),
            generation: loaded.generation,
            files: Vec::new(),
            members: Vec::new(),
            file_of: Vec::new(),
            chunk_of: Vec::new(),
            set: None,
            subset: Mutex::new(None),
            coverage: Coverage {
                files: surveyed.len(),
                ..Coverage::default()
            },
        };
        for file in &surveyed {
            let coverage = &mut members.coverage;
            if let Some(reason) = &file.gap {
                coverage
                    .skipped_parses
                    .push((absolute(&file.path), reason.clone()));
                continue;
            }
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
            subset = members.subset(&index, included)?;
            (&subset.set, Some(&subset.positions))
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

/// How long a fill round waits for another fill of the same repository
/// before it stops and is tried again later. Another round holds the lock
/// while it embeds up to a round of files, which a slow local model can
/// take minutes over.
const FILL_WAIT: Duration = Duration::from_secs(600);

/// The file in a family's folder whose lock a fill round holds while it
/// embeds and publishes (see [`FillLock`]). The store ignores names it does
/// not write.
const FILL_LOCK: &str = "vector-fill.lock";

/// One fill round's hold on its repository: while one round, in any
/// process, embeds texts and publishes them, every other round of the
/// repository waits, and then reads what it published before choosing what
/// to embed. So two checkouts of one repository, in one process or two,
/// never send the same text to the embedder. Released when dropped.
struct FillLock {
    _file: std::fs::File,
}

impl FillLock {
    /// Takes the lock of the family in `dir` if no other fill holds it.
    fn try_take(dir: &Path) -> Result<Option<Self>, DataError> {
        let path = dir.join(FILL_LOCK);
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|error| DataError::Store(format!("{}: {error}", path.display())))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(DataError::Store(format!("{}: {error}", path.display())))
            }
        }
    }

    /// Takes the lock of the family in `dir`, waiting up to [`FILL_WAIT`];
    /// `waiting` hears once if it has to wait.
    fn take(dir: &Path, waiting: &mut dyn FnMut()) -> Result<Self, DataError> {
        let path = dir.join(FILL_LOCK);
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|error| DataError::Store(format!("{}: {error}", path.display())))?;
        let deadline = Instant::now() + FILL_WAIT;
        let mut told = false;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(DataError::Store(format!("{}: {error}", path.display())));
                }
            }
            if !told {
                waiting();
                told = true;
            }
            if crate::executor::current_job_cancelled() {
                return Err(DataError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(DataError::Busy(format!(
                    "another fill of this repository held {} for over {} s",
                    path.display(),
                    FILL_WAIT.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// How far a fill has come, reported after each round it publishes, and
/// when a round starts and stops waiting for another process's fill.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct FillProgress {
    /// Files this fill has finished, of the files it set out to fill.
    pub files_done: usize,
    pub files_total: usize,
    /// Texts embedded so far.
    pub chunks_embedded: usize,
    /// Vectors taken from the old meaning index so far (see
    /// [`super::old_index`]).
    pub chunks_imported: usize,
    /// The round is waiting for another fill of the same repository, in
    /// this process or another, to publish what it is embedding.
    pub waiting: bool,
    /// When the round this report ends stored what it filled (vectors and
    /// chunk maps, published as one segment), in milliseconds since the
    /// Unix epoch; `None` when the round stored nothing, and on the reports
    /// a wait sends.
    pub stored_at_ms: Option<u64>,
}

/// Milliseconds since the Unix epoch now.
pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
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
    /// Luau files waiting for the refresh's forced publication. That
    /// publication starts the next fill; they do not spin a fill job chain.
    #[serde(skip_serializing_if = "no_files")]
    pub files_awaiting_publication: usize,
    /// Files not finished when the fill stopped early.
    pub files_left: usize,
    /// Chunks of the files this fill cut, how many of their texts already
    /// had vectors, how many it took from the old meaning index, how many
    /// it embedded, and how many the embedder refused.
    pub chunks: usize,
    pub chunks_reused: usize,
    pub chunks_imported: usize,
    pub chunks_embedded: usize,
    pub chunks_refused: usize,
    pub embed_calls: usize,
    pub segments_published: usize,
    pub segment_bytes: u64,
    pub read_ms: f64,
    pub chunk_ms: f64,
    /// Time spent reading the old meaning index and matching its rows.
    pub import_ms: f64,
    pub embed_ms: f64,
    pub publish_ms: f64,
    /// Files another fill of the repository published while this one
    /// waited for it, which this one then left alone.
    pub files_filled_elsewhere: usize,
    /// Time spent waiting for other fills of the repository.
    pub wait_ms: f64,
    /// Merges of the family's vector segments this fill ran after its
    /// rounds (see [`VectorPlane::merge_segments`]), the segments they
    /// merged, and their time.
    pub merges: usize,
    pub merged_segments: usize,
    pub merge_ms: f64,
    /// Why the fill stopped before finishing; what it published stands.
    pub error: Option<String>,
}

fn no_files(value: &usize) -> bool {
    *value == 0
}

/// Vector segments of the plane's model a family keeps before a fill round
/// merges them. Each round publishes one, folds never copy vectors, and a
/// vector segment leaves only once no checkout reaches any of its vectors,
/// so without merging their number only grew: 133 of one family's 212
/// segments, and 291 of another's 335, after three days of use.
const MERGE_AT: usize = 8;

/// A vector segment written less than this long ago is left out of a
/// merge. Its vectors may be reached only by a checkout's changes that are
/// not published yet, which no merge can see; a checkout publishes its
/// changes within minutes of going quiet.
const MERGE_GRACE: Duration = Duration::from_secs(600);

/// What a merge of vector segments did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct MergeReport {
    /// Segments holding the model's vectors in the generation it read.
    pub examined: usize,
    /// Segments merged into the new one, and their length together.
    pub merged: usize,
    pub merged_bytes: u64,
    /// Chunk maps and vectors the new segment holds, and its length.
    pub chunk_maps: usize,
    pub vectors: usize,
    pub segment_bytes: u64,
    /// Chunk maps and vectors the merged segments held that are not what a
    /// reader reaches there (none reaches them, or a newer segment holds
    /// them too), left out.
    pub dropped_chunk_maps: usize,
    pub dropped_vectors: usize,
    /// Files the reclamation after publishing removed, or why it could not
    /// run (the publication stands either way).
    pub reclaimed_files: usize,
    pub reclaim_error: Option<String>,
    pub ms: f64,
}

/// One checkout's meaning search: its fill and its searches.
pub struct VectorPlane {
    checkout: Arc<Checkout>,
    producer: Producer,
    max_files: usize,
    loaded: Mutex<Arc<Loaded>>,
    members: Mutex<Option<Arc<Members>>>,
    coverage_rebuild: AtomicU8,
    fill_queue: Mutex<Option<FillQueue>>,
    /// The old meaning index whose vectors fills take where they match.
    old: Option<OldIndex>,
    /// When a fill round merges the family's vector segments.
    merge: MergeLimits,
    merge_due: Mutex<Option<(Generation, u64)>>,
}

const REBUILD_IDLE: u8 = 0;
const REBUILD_RUNNING: u8 = 1;
const REBUILD_DIRTY: u8 = 2;

/// Owns the plane's single queued or running status rebuild. Dropping the
/// job without running it (for example, on executor refusal) makes it idle.
pub(super) struct CoverageRebuild {
    plane: Arc<VectorPlane>,
    active: bool,
}

impl CoverageRebuild {
    pub(super) fn run(mut self, cancelled: impl Fn(&Arc<VectorPlane>) -> bool) -> bool {
        let mut surveyed = false;
        loop {
            // Pick up requests made while queued by reading the latest view.
            // Requests arriving after this point get one more cache check.
            self.plane
                .coverage_rebuild
                .store(REBUILD_RUNNING, Ordering::Release);
            if cancelled(&self.plane) {
                return false;
            }
            if let Some(view) = self.plane.checkout.view() {
                surveyed |= self.plane.snapshot(&view).is_ok();
            }
            if self
                .plane
                .coverage_rebuild
                .compare_exchange(
                    REBUILD_RUNNING,
                    REBUILD_IDLE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.active = false;
                return surveyed && !cancelled(&self.plane);
            }
        }
    }
}

impl Drop for CoverageRebuild {
    fn drop(&mut self) {
        if self.active {
            self.plane
                .coverage_rebuild
                .store(REBUILD_IDLE, Ordering::Release);
        }
    }
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
            coverage_rebuild: AtomicU8::new(REBUILD_IDLE),
            fill_queue: Mutex::new(None),
            old: None,
            merge: MergeLimits::default(),
            merge_due: Mutex::default(),
        }
    }

    /// Makes this plane's merges take vector segments written at least
    /// `grace` ago, instead of [`MERGE_GRACE`]. For tests and measurement.
    #[doc(hidden)]
    #[must_use]
    pub fn with_merge_grace(mut self, grace: Duration) -> Self {
        self.merge.grace = grace;
        self
    }

    /// Makes this plane's fills take a chunk's vector from `old`, the old
    /// meaning index, where it holds one for exactly that chunk (see
    /// [`super::old_index`]), instead of embedding it. An index made for
    /// another model than the plane's is not used.
    #[must_use]
    pub fn reusing_old_index(mut self, old: OldIndex) -> Self {
        if Producer::from_fingerprint(old.configured()).model == self.producer.model {
            self.old = Some(old);
        } else {
            crate::slog_warn!(
                "data layer meaning fill: the old meaning index was offered for another model than {}",
                self.producer.model.name
            );
        }
        self
    }

    /// The old meaning index's vectors for `chunks` of the file at `path`
    /// with content `bytes`, one slot per chunk; empty without an old
    /// index.
    fn old_vectors(&self, path: &[u8], bytes: &[u8], chunks: &[CutChunk]) -> Vec<Option<Vec<f32>>> {
        self.old
            .as_ref()
            .and_then(OldIndex::snapshot)
            .map_or_else(Vec::new, |snapshot| snapshot.vectors(path, bytes, chunks))
    }

    /// The vector the family's newest generation holds for `text` under
    /// this plane's model, if any: for measurements that compare the
    /// vectors of two stores.
    ///
    /// # Errors
    ///
    /// When the store cannot be read.
    pub fn stored_vector(&self, text: &str) -> Result<Option<Vec<f32>>, DataError> {
        let loaded = self.load()?;
        let Some(index) = loaded.shared.as_ref().map(SharedVectorIndex::index) else {
            return Ok(None);
        };
        let Some(unit) = index.locate(TextDigest::of(text.as_bytes())) else {
            return Ok(None);
        };
        index.vector(unit).map(Some).map_err(store_error)
    }

    /// The family's newest generation, read again only when a newer one
    /// has been published. Finding that nothing newer was published costs
    /// one listing of the family's directory: no store file is opened, no
    /// lock taken and no lease written. Only a manifest numbered above the
    /// loaded generation is opened and checked, and only a valid newer one
    /// is read through a snapshot.
    fn load(&self) -> Result<Arc<Loaded>, DataError> {
        let parts = self.checkout.vector_parts();
        let held = Arc::clone(&self.loaded.lock().expect("no panics while holding it"));
        if !parts
            .family
            .has_manifest_above(held.generation)
            .map_err(store_error)?
        {
            return Ok(held);
        }
        let newest = parts.family.newest_generation().map_err(store_error)?;
        if held.generation >= newest {
            return Ok(held);
        }
        let read = Arc::new(Loaded::read(&parts, &self.producer.model, &held)?);
        let mut loaded = self.loaded.lock().expect("no panics while holding it");
        if loaded.generation >= read.generation {
            return Ok(Arc::clone(&loaded));
        }
        let retired = std::mem::replace(&mut *loaded, Arc::clone(&read));
        self.checkout.touch_memory();
        drop(loaded);
        drop(retired);
        Ok(read)
    }

    /// The members of the pinned `view` and `loaded` pair. Only the current
    /// pair can populate the shared cache.
    fn members(&self, view: &CheckoutView, loaded: &Loaded) -> Result<Arc<Members>, DataError> {
        self.members_with(view, loaded, || {
            Members::build(view, loaded, self.max_files)
        })
    }

    fn members_with(
        &self,
        view: &CheckoutView,
        loaded: &Loaded,
        build: impl FnOnce() -> Result<Members, DataError>,
    ) -> Result<Arc<Members>, DataError> {
        let key = view.text_generation();
        let parse_gaps = view.parse_gaps_identity();
        let matches = |members: &Arc<Members>| members.matches(&key, parse_gaps, loaded.generation);
        let cached = self.members.lock().expect("no panics while holding it");
        if let Some(members) = cached.as_ref().filter(|members| matches(members)) {
            return Ok(Arc::clone(members));
        }
        drop(cached);
        let members = Arc::new(build()?);
        let Some((selected, retired)) = self.checkout.with_current_meaning_view(view, || {
            // Keep both identities stable through installation. Searches
            // that pinned older inputs still get their own coherent survey.
            let current = self.loaded.lock().expect("no panics while holding it");
            if current.generation != loaded.generation {
                return (Arc::clone(&members), None);
            }
            let mut cached = self.members.lock().expect("no panics while holding it");
            if let Some(existing) = cached.as_ref().filter(|existing| matches(existing)) {
                return (Arc::clone(existing), None);
            }
            let retired = cached.replace(Arc::clone(&members));
            self.checkout.touch_memory();
            (Arc::clone(&members), retired)
        }) else {
            return Ok(members);
        };
        drop(retired);
        Ok(selected)
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
    /// this plane read last, without looking for a newer one: looking costs
    /// a listing of the family's directory per call. A fill reads the newest
    /// generation as it starts and after each round it publishes;
    /// [`Self::refresh`] reads it on demand.
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

    /// A snapshot only if this exact text view and vector generation were
    /// already surveyed. Status must not rebuild members on its own thread.
    pub(crate) fn cached_snapshot(&self, view: &CheckoutView) -> Option<MeaningSnapshot> {
        let key = view.text_generation();
        let parse_gaps = view.parse_gaps_identity();
        let (loaded, members) = {
            let loaded = self.loaded.try_lock().ok()?;
            let cached = self.members.try_lock().ok()?;
            let members = cached
                .as_ref()
                .filter(|members| members.matches(&key, parse_gaps, loaded.generation))?;
            (Arc::clone(&loaded), Arc::clone(members))
        };
        Some(MeaningSnapshot {
            loaded,
            members,
            root: view.root().to_path_buf(),
            dimensions: self.producer.model.dimensions as usize,
        })
    }

    pub(super) fn request_coverage_rebuild(self: &Arc<Self>) -> Option<CoverageRebuild> {
        let previous = self.coverage_rebuild.swap(REBUILD_DIRTY, Ordering::AcqRel);
        (previous == REBUILD_IDLE).then(|| CoverageRebuild {
            plane: Arc::clone(self),
            active: true,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_members_lock_for_test(&self, run: impl FnOnce()) {
        let _members = self.members.lock().unwrap();
        run();
    }

    #[cfg(feature = "test-hooks")]
    fn hold_members_for_status_test(&self) {
        let Some(gate) = std::env::var_os("AFT_TEST_VECTOR_MEMBERS_GATE").map(PathBuf::from) else {
            return;
        };
        if !gate.exists() {
            return;
        }
        let _owner = self.members.lock().expect("no panics while holding it");
        std::fs::write(gate.with_extension("waiting"), b"").expect("signal vector members gate");
        let deadline = Instant::now() + Duration::from_secs(30);
        while gate.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Reads the family's newest generation, if it is newer than the one
    /// the plane read last. When nothing newer was published this lists the
    /// family's directory and does nothing else (see [`Self::load`]).
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

/// Pending work across background jobs, each of which normally publishes
/// one batch. Keeping just path and content keys does not keep an older
/// checkout view or vector snapshot alive.
struct FillQueue {
    text: String,
    parse_gaps: Digest128,
    segments: Vec<SegmentId>,
    files: usize,
    admitted: usize,
    embedded: usize,
    awaiting_publication: usize,
    /// Reverse path order, so consuming the next path does not shift the rest.
    pending: Vec<(Vec<u8>, ContentKey)>,
}

aft_store::heap_bytes_of_fields!(FillQueue {
    text,
    parse_gaps,
    segments,
    files,
    admitted,
    embedded,
    awaiting_publication,
    pending
});

fn vector_membership(loaded: &Loaded) -> Vec<SegmentId> {
    loaded
        .shared
        .iter()
        .flat_map(|shared| shared.segments().map(|segment| segment.id()))
        .collect()
}

impl FillQueue {
    fn survey(view: &CheckoutView, loaded: &Loaded, max_files: usize) -> Result<Self, DataError> {
        #[cfg(test)]
        FILL_SURVEYS.with(|surveys| surveys.set(surveys.get() + 1));
        let index = loaded.shared.as_ref().map(SharedVectorIndex::index);
        let surveyed = survey(view, loaded, index.as_ref(), max_files)?;
        let mut queue = Self {
            text: view.text_generation(),
            parse_gaps: view.parse_gaps_identity(),
            segments: vector_membership(loaded),
            files: surveyed.len(),
            admitted: 0,
            embedded: 0,
            awaiting_publication: 0,
            pending: Vec::new(),
        };
        for file in surveyed {
            if file.gap.is_some() || !file.admitted {
                continue;
            }
            queue.admitted += 1;
            if !file.kind.has_content() {
                continue;
            }
            if file.done(index.as_ref()) {
                queue.embedded += 1;
            } else if native_path(&file.path).is_some_and(|path| is_luau(&path))
                && view.awaits_parse_publication(&file.path)
            {
                queue.awaiting_publication += 1;
            } else {
                queue.pending.push((file.path, file.content));
            }
        }
        queue.pending.reverse();
        Ok(queue)
    }
}

#[cfg(test)]
thread_local! {
    static FILL_SURVEYS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn check_cancelled() -> Result<(), DataError> {
    if crate::executor::current_job_cancelled() {
        return Err(DataError::Cancelled);
    }
    Ok(())
}

/// Waits up to [`WRITER_WAIT`] for the family's writer.
fn writer(family: &Family) -> Result<WriteSession<'_>, DataError> {
    let deadline = Instant::now() + WRITER_WAIT;
    loop {
        check_cancelled()?;
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
        check_cancelled()?;
        let mut report = FillReport::default();
        let Some(view) = self.checkout.view() else {
            return Ok(report);
        };
        #[cfg(feature = "test-hooks")]
        self.hold_members_for_status_test();
        let parts = self.checkout.vector_parts();
        let model = &self.producer.model;
        let dimensions = model.dimensions as usize;
        let mut loaded = self.load()?;
        let previous = self
            .fill_queue
            .lock()
            .expect("no panics while holding it")
            .take();
        let mut queue = match previous {
            Some(queue)
                if queue.text == view.text_generation()
                    && queue.parse_gaps == view.parse_gaps_identity()
                    && queue.segments == vector_membership(&loaded) =>
            {
                queue
            }
            _ => FillQueue::survey(&view, &loaded, self.max_files)?,
        };
        report.files = queue.files;
        report.files_admitted = queue.admitted;
        report.files_already_embedded = queue.embedded;
        report.files_awaiting_publication = queue.awaiting_publication;
        report.files_queued = queue.pending.len();
        #[cfg(all(
            test,
            feature = "wasm-grammars",
            feature = "test-hooks",
            not(all(target_os = "windows", target_arch = "aarch64"))
        ))]
        super::test_gates::arrive(view.root(), super::test_gates::Point::FillSurvey);
        let per_vector = VectorSectionsBuilder::bytes_per_vector(dimensions);
        let mut embed_rows = |texts: Vec<String>| embed(texts);
        let mut files_done = 0;
        let mut rounds_run = 0;
        let mut reusable = rounds == 1;
        if rounds > 0 && queue.pending.is_empty() {
            self.maybe_merge(&mut loaded, None, &mut report)?;
            reusable &= report.merges == 0;
        }
        while !queue.pending.is_empty() {
            if rounds_run == rounds {
                report.files_left += queue.pending.len();
                break;
            }
            rounds_run += 1;

            // Cut the round's files, and collect the texts no segment holds:
            // those the old meaning index has vectors for, and the rest,
            // which are embedded.
            let mut round = Vec::new();
            let mut wanted: Vec<(TextDigest, String)> = Vec::new();
            let mut imported: HashMap<TextDigest, Vec<f32>> = HashMap::new();
            {
                let index = loaded.shared.as_ref().map(SharedVectorIndex::index);
                let stored = |digest: &TextDigest| {
                    index
                        .as_ref()
                        .is_some_and(|index| index.locate(*digest).is_some())
                };
                let mut seen = HashSet::new();
                while round.len() < budget.files_per_round.max(1)
                    && (wanted.len() + imported.len()).saturating_mul(per_vector) < ROUND_BYTES
                {
                    check_cancelled()?;
                    let Some((path, content)) = queue.pending.pop() else {
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
                    let cut = if native_path(&path).is_some_and(|path| is_luau(&path)) {
                        cut_stored(&view, &path, &bytes, self.producer.caps)?
                    } else {
                        cut_file(parts.root, &path, &bytes, self.producer.caps)
                    };
                    report.chunk_ms += elapsed_ms(started);
                    let chunks = match cut {
                        Cut::Failed(reason) => Err(reason),
                        Cut::Chunks(chunks) => {
                            let digests: Vec<TextDigest> = chunks
                                .iter()
                                .map(|chunk| TextDigest::of(chunk.text.as_bytes()))
                                .collect();
                            let mut old = Vec::new();
                            if digests.iter().any(|digest| !stored(digest)) {
                                let started = Instant::now();
                                old = self.old_vectors(&path, &bytes, &chunks);
                                report.import_ms += elapsed_ms(started);
                            }
                            Ok(chunks
                                .into_iter()
                                .zip(digests)
                                .enumerate()
                                .map(|(at, (chunk, digest))| {
                                    report.chunks += 1;
                                    if stored(&digest) {
                                        report.chunks_reused += 1;
                                    } else if seen.insert(digest) {
                                        match old
                                            .get_mut(at)
                                            .and_then(Option::take)
                                            .filter(|vector| vector.len() == dimensions)
                                        {
                                            Some(vector) => {
                                                imported.insert(digest, vector);
                                            }
                                            None => wanted.push((digest, chunk.text.clone())),
                                        }
                                    }
                                    (digest, chunk)
                                })
                                .collect())
                        }
                    };
                    round.push(RoundFile {
                        path,
                        content,
                        chunks,
                    });
                }
            }

            // Wait for any other fill of the repository to publish, then
            // leave out what it published: its texts' vectors, and the files
            // whose chunk maps it finished.
            let round_lock = if wanted.is_empty() && imported.is_empty() {
                None
            } else {
                let started = Instant::now();
                let waiting = FillProgress {
                    files_done,
                    files_total: report.files_queued,
                    chunks_embedded: report.chunks_embedded,
                    chunks_imported: report.chunks_imported,
                    waiting: true,
                    stored_at_ms: None,
                };
                let lock = FillLock::take(self.checkout.family_dir(), &mut || progress(waiting))?;
                report.wait_ms += elapsed_ms(started);
                loaded = self.load()?;
                reusable &= queue.segments == vector_membership(&loaded);
                progress(FillProgress {
                    waiting: false,
                    ..waiting
                });
                Some(lock)
            };
            let current = Arc::clone(&loaded);
            let index = current.shared.as_ref().map(SharedVectorIndex::index);
            let stored = |digest: &TextDigest| {
                index
                    .as_ref()
                    .is_some_and(|index| index.locate(*digest).is_some())
            };
            let asked = wanted.len() + imported.len();
            wanted.retain(|(digest, _)| !stored(digest));
            imported.retain(|digest, _| !stored(digest));
            let left = wanted.len() + imported.len();
            report.chunks_reused += asked - left;
            if asked > left {
                if let Some(index) = &index {
                    let mut kept = Vec::with_capacity(round.len());
                    for file in round {
                        let done = current
                            .chunk_map(index, file.content, &file.path)?
                            .is_some_and(|(_, chunks)| {
                                chunks
                                    .iter()
                                    .all(|chunk| chunk.failed || stored(&chunk.text))
                            });
                        if done {
                            report.files_filled_elsewhere += 1;
                            files_done += 1;
                        } else {
                            kept.push(file);
                        }
                    }
                    round = kept;
                }
            }

            // Embed the rest, each distinct text once.
            report.chunks_imported += imported.len();
            let mut vectors: HashMap<TextDigest, Vec<f32>> = imported;
            let mut refused: HashMap<TextDigest, String> = HashMap::new();
            let started = Instant::now();
            for batch in wanted.chunks(budget.max_batch.max(1)) {
                check_cancelled()?;
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
                                map_content(file.content, &file.path),
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
                        map_content(file.content, &file.path),
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
            let mut stored_at_ms = None;
            if builder.chunk_maps() > 0 || builder.vectors() > 0 {
                let started = Instant::now();
                let (base, generation, bytes) = Self::publish(&parts, builder)?;
                report.segment_bytes += bytes;
                stored_at_ms = Some(unix_ms());
                report.publish_ms += elapsed_ms(started);
                report.segments_published += 1;
                report.files_filled += filled;
                // Advance over our own publication only, not a publication
                // that might also have changed maps for other queued files.
                reusable &= base == loaded.generation;
                loaded = self.load()?;
                reusable &= loaded.generation == generation;
                self.maybe_merge(&mut loaded, round_lock.as_ref(), &mut report)?;
                reusable &= report.merges == 0;
            }
            progress(FillProgress {
                files_done,
                files_total: report.files_queued,
                chunks_embedded: report.chunks_embedded,
                chunks_imported: report.chunks_imported,
                waiting: false,
                stored_at_ms,
            });
            if report.error.is_some() {
                report.files_left += queue.pending.len();
                break;
            }
        }
        if reusable
            && report.error.is_none()
            && report.files_changed == 0
            && report.files_left == queue.pending.len()
        {
            queue.embedded += report.files_filled + report.files_filled_elsewhere;
            queue.segments = vector_membership(&loaded);
            *self.fill_queue.lock().expect("no panics while holding it") = Some(queue);
            self.checkout.touch_memory();
        }
        Ok(report)
    }

    fn maybe_merge(
        &self,
        loaded: &mut Arc<Loaded>,
        round_lock: Option<&FillLock>,
        report: &mut FillReport,
    ) -> Result<(), DataError> {
        if !self.merge_ready(loaded) {
            return Ok(());
        }
        let taken = if round_lock.is_none() {
            FillLock::try_take(self.checkout.family_dir())?
        } else {
            None
        };
        if round_lock.is_some() || taken.is_some() {
            match self.merge_segments() {
                Ok(Some(merged)) => {
                    report.merges += 1;
                    report.merged_segments += merged.merged;
                    report.merge_ms += merged.ms;
                    *loaded = self.load()?;
                }
                Ok(None) => {}
                Err(error) => crate::slog_warn!(
                    "data layer could not merge the vector segments of {}: {error}",
                    self.checkout.root().display(),
                ),
            }
        }
        Ok(())
    }

    /// Memoize when a second segment ages into eligibility. No timer is
    /// scheduled: another fill or publication checks again, so an idle
    /// checkout can keep some young segments unmerged.
    fn merge_ready(&self, loaded: &Loaded) -> bool {
        if loaded.open.len() <= self.merge.merge_at {
            return false;
        }
        let mut memo = self.merge_due.lock().expect("no panics while holding it");
        let due = match *memo {
            Some((generation, due)) if generation == loaded.generation => due,
            _ => {
                let mut oldest = [u64::MAX; 2];
                for segment in loaded.open.values().filter(|segment| vector_only(segment)) {
                    let created = segment.created_unix_ms();
                    if created < oldest[0] {
                        oldest = [created, oldest[0]];
                    } else if created < oldest[1] {
                        oldest[1] = created;
                    }
                }
                let due = oldest[1].saturating_add(self.merge.grace_ms());
                *memo = Some((loaded.generation, due));
                due
            }
        };
        merge_now_ms() >= due
    }

    /// Publishes `builder` as one new segment, in a generation that
    /// changes no checkout's state. Returns the before and after generations
    /// and the segment's length.
    fn publish(
        parts: &VectorParts<'_>,
        builder: VectorSectionsBuilder,
    ) -> Result<(Generation, Generation, u64), DataError> {
        let mut session = writer(parts.family)?;
        let base = session.base().generation();
        let mut pending = session.create_segment().map_err(store_error)?;
        builder.write(pending.writer()).map_err(store_error)?;
        let id = session.finish_segment(pending).map_err(store_error)?;
        check_cancelled()?;
        let generation = session
            .publish(&Publication {
                add_segments: vec![id],
                ..Publication::default()
            })
            .map_err(store_error)?;
        Ok((
            base,
            generation,
            session.base().segment(id).map_or(0, |entry| entry.len),
        ))
    }

    /// Merges vector segments of this plane's model into one, when the
    /// family holds more than [`MERGE_AT`] of them (see
    /// [`merge_vector_segments`]). The current states of the family's
    /// checkouts bound in this process, published or not, count among the
    /// readers whose vectors the merge keeps. `None` when nothing was
    /// merged.
    ///
    /// # Errors
    ///
    /// When the store cannot be read or written. Nothing is published then.
    pub fn merge_segments(&self) -> Result<Option<MergeReport>, DataError> {
        let bound = self.checkout.bound_states();
        let merged = merge_vector_segments(
            &self.checkout.vector_parts(),
            &self.producer.model,
            &bound,
            self.merge,
        )?;
        if let Some(report) = &merged {
            crate::slog_info!(
                "data layer merged vector segments of {}: examined {} segments of the model; merged {} ({} bytes) into one of {} bytes ({} chunk maps, {} vectors; left out {} chunk maps and {} vectors no checkout reaches), {:.0} ms",
                self.checkout.root().display(),
                report.examined,
                report.merged,
                report.merged_bytes,
                report.segment_bytes,
                report.chunk_maps,
                report.vectors,
                report.dropped_chunk_maps,
                report.dropped_vectors,
                report.ms
            );
        }
        Ok(merged)
    }
}

/// When a merge of vector segments runs and how much it copies.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MergeLimits {
    /// Segments of the model the family must hold more of.
    pub(crate) merge_at: usize,
    /// The vector bytes one merge copies at most.
    pub(crate) budget: usize,
    /// Segments younger than this are left out.
    pub(crate) grace: Duration,
}

impl Default for MergeLimits {
    fn default() -> Self {
        Self {
            merge_at: MERGE_AT,
            budget: ROUND_BYTES,
            grace: MERGE_GRACE,
        }
    }
}

impl MergeLimits {
    fn grace_ms(self) -> u64 {
        u64::try_from(self.grace.as_millis()).unwrap_or(u64::MAX)
    }
}

fn vector_only(segment: &Segment) -> bool {
    segment.sections().all(|(_, section)| {
        aft_store::vector::KNOWN
            .iter()
            .any(|known| known.kind == section.id.kind)
    })
}

fn merge_now_ms() -> u64 {
    #[cfg(test)]
    if let Some(now) = MERGE_NOW_MS.with(std::cell::Cell::get) {
        return now;
    }
    unix_ms()
}

/// Merges vector segments of `model` into one new segment, when the
/// newest generation holds more than `limits.merge_at` of them.
///
/// It takes the vector-only segments older than `limits.grace`, those
/// whose reached vectors take the fewest bytes first, while the vectors it
/// copies stay within `limits.budget`. Into the new segment go the chunk
/// maps and vectors of those segments that a reader reaches, by the rule
/// readers follow ([`needed_vector_segments`]): each chunk map of a
/// checkout's path from the newest segment holding it, and each vector its
/// chunks name from the newest segment holding that. The readers are every
/// checkout state the generation names, and `bound`: the current states of
/// checkouts bound in this process, whose changes not yet published no
/// published state names. Every copied unit is
/// stored with the bytes it had, and the new segment is the newest that
/// holds it, so readers find the same maps and vectors as before. The rest
/// of those segments is what no reader reaches, and leaves with them.
///
/// `None` when there was nothing to merge, or a segment or state of the
/// generation cannot be read by this build (it could need anything).
///
/// # Errors
///
/// When the store cannot be read or written. Nothing is published then.
pub(crate) fn merge_vector_segments(
    parts: &VectorParts<'_>,
    model: &ModelIdentity,
    bound: &[CheckoutState],
    limits: MergeLimits,
) -> Result<Option<MergeReport>, DataError> {
    #[cfg(test)]
    MERGE_WRITER_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
    let started = Instant::now();
    let mut session = writer(parts.family)?;
    // With the writer held, this is the generation the merge changes.
    let snapshot = parts.family.snapshot().map_err(store_error)?;
    let entries = snapshot.manifest().segments().to_vec();
    // Read once, in order: a small cache of its own, as a fold does.
    let copying = Arc::new(BlockCache::new(16 << 20));
    let mut opened = Vec::with_capacity(entries.len());
    for entry in &entries {
        match snapshot.open_segment(entry.id, &parts.known, Arc::clone(&copying)) {
            Ok(segment) => opened.push(segment),
            Err(error) if error.is_unusable_file() => return Ok(None),
            Err(error) => return Err(store_error(error)),
        }
    }
    let vectors = opened
        .iter()
        .map(|segment| VectorSegment::open(segment).map_err(store_error))
        .collect::<Result<Vec<_>, _>>()?;
    let ours = (0..opened.len())
        .filter(|&place| {
            vectors[place]
                .as_ref()
                .is_some_and(|vectors| vectors.model() == model)
        })
        .collect::<Vec<_>>();
    let mut report = MergeReport {
        examined: ours.len(),
        ..MergeReport::default()
    };
    if ours.len() <= limits.merge_at {
        return Ok(None);
    }
    let now = merge_now_ms();
    let eligible = ours
        .iter()
        .copied()
        .filter(|&place| {
            let segment = &opened[place];
            vector_only(segment)
                && now.saturating_sub(segment.created_unix_ms()) >= limits.grace_ms()
        })
        .collect::<Vec<_>>();
    if eligible.len() < 2 {
        return Ok(None);
    }
    let ours_at = |place: usize| vectors[place].as_ref().expect("a segment of the model");

    // The newest holder of each chunk map and vector of the model.
    let mut maps = HashMap::new();
    let mut texts = HashMap::new();
    for &place in &ours {
        for key in ours_at(place).chunk_map_keys().map_err(store_error)? {
            maps.insert(key, place);
        }
        for text in ours_at(place).texts().map_err(store_error)? {
            texts.insert(text, place);
        }
    }
    // What the readers reach, by holder.
    let mut states = Vec::new();
    for state_ref in snapshot.manifest().checkouts() {
        match snapshot.checkout_state(state_ref.checkout) {
            Ok(Some(state)) => states.push(state),
            Ok(None) => {}
            Err(error) if error.is_unusable_file() => return Ok(None),
            Err(error) => return Err(store_error(error)),
        }
    }
    let mut reached_maps = HashMap::<usize, Vec<(ContentKey, Vec<u8>)>>::new();
    let mut reached_texts = HashMap::<usize, HashSet<TextDigest>>::new();
    let mut reach_text = |text| {
        if let Some(&holder) = texts.get(&text) {
            reached_texts.entry(holder).or_default().insert(text);
        }
    };
    for &place in &ours {
        visit_young_map_texts(
            &opened[place],
            ours_at(place),
            now,
            limits.grace_ms(),
            &mut reach_text,
        )?;
    }
    let mut asked = HashSet::new();
    for state in states.iter().chain(bound) {
        for record in state.paths() {
            if !record.kind.has_content() {
                continue;
            }
            let key = ChunkMapKey::new(map_content(record.content, &record.path), &record.path);
            if !asked.insert(key) {
                continue;
            }
            let Some(&holder) = maps.get(&key) else {
                continue;
            };
            let chunks = ours_at(holder)
                .chunk_map(map_content(record.content, &record.path), &record.path)
                .map_err(store_error)?
                .unwrap_or_default();
            reached_maps.entry(holder).or_default().push((
                map_content(record.content, &record.path),
                record.path.clone(),
            ));
            for chunk in chunks {
                reach_text(chunk.text);
            }
        }
    }

    // The segments to merge: vector-only and old enough, fewest reached
    // vector bytes first, cut where the copied bytes would pass the budget.
    let per_vector = VectorSectionsBuilder::bytes_per_vector(model.dimensions as usize);
    let mut candidates = Vec::new();
    for place in eligible {
        let bytes = reached_texts.get(&place).map_or(0, HashSet::len) * per_vector;
        candidates.push((bytes, place));
    }
    candidates.sort_unstable();
    let mut taken = Vec::new();
    let mut copied = 0usize;
    for (bytes, place) in candidates {
        if copied.saturating_add(bytes) > limits.budget {
            break;
        }
        copied += bytes;
        taken.push(place);
    }
    if taken.len() < 2 {
        return Ok(None);
    }
    taken.sort_unstable();

    let mut builder = VectorSectionsBuilder::new(model.clone()).map_err(store_error)?;
    for &place in &taken {
        let segment = ours_at(place);
        let maps_here = reached_maps.remove(&place).unwrap_or_default();
        report.dropped_chunk_maps += usize::try_from(segment.chunk_map_count())
            .unwrap_or(usize::MAX)
            .saturating_sub(maps_here.len());
        for (content, path) in maps_here {
            let chunks = segment
                .chunk_map(content, &path)
                .map_err(store_error)?
                .ok_or_else(|| DataError::Store("a listed chunk map is missing".to_string()))?;
            match segment.chunk_details(content, &path).map_err(store_error)? {
                Some(details) => builder
                    .add_chunk_map_with_details(content, &path, &chunks, details)
                    .map_err(store_error)?,
                None => builder.add_chunk_map(content, &path, &chunks),
            }
        }
        let texts_here = reached_texts.remove(&place).unwrap_or_default();
        report.dropped_vectors += usize::try_from(segment.len())
            .unwrap_or(usize::MAX)
            .saturating_sub(texts_here.len());
        for text in texts_here {
            let ordinal = segment
                .find(text)
                .map_err(store_error)?
                .ok_or_else(|| DataError::Store("a listed vector is missing".to_string()))?;
            builder
                .add_vector(text, &segment.vector(ordinal).map_err(store_error)?)
                .map_err(store_error)?;
        }
        report.merged += 1;
        report.merged_bytes += entries[place].len;
    }
    report.chunk_maps = builder.chunk_maps();
    report.vectors = builder.vectors();
    let mut pending = session.create_segment().map_err(store_error)?;
    builder.write(pending.writer()).map_err(store_error)?;
    let id = session.finish_segment(pending).map_err(store_error)?;
    check_cancelled()?;
    session
        .publish(&Publication {
            add_segments: vec![id],
            remove_segments: taken.iter().map(|&place| entries[place].id).collect(),
            ..Publication::default()
        })
        .map_err(store_error)?;
    report.segment_bytes = session.base().segment(id).map_or(0, |entry| entry.len);
    // A reader still holding the older generation keeps its files.
    match session.reclaim() {
        Ok(reclaimed) => {
            report.reclaimed_files =
                reclaimed.removed_segments + reclaimed.removed_states + reclaimed.removed_manifests;
        }
        Err(error) => report.reclaim_error = Some(error.to_string()),
    }
    report.ms = elapsed_ms(started);
    Ok(Some(report))
}

#[cfg(test)]
thread_local! {
    static MERGE_WRITER_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static MERGE_NOW_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// A young chunk map can belong to another process's unpublished edit and
/// reuse vectors from older segments. Protect those vectors even when no
/// published or locally bound checkout names the map.
fn visit_young_map_texts(
    segment: &Segment,
    vectors: &VectorSegment<'_>,
    now: u64,
    grace: u64,
    mut visit: impl FnMut(TextDigest),
) -> Result<(), DataError> {
    if now.saturating_sub(segment.created_unix_ms()) < grace {
        for ordinal in 0..vectors.chunk_map_count() {
            for chunk in vectors.chunk_map_at(ordinal).map_err(store_error)? {
                visit(chunk.text);
            }
        }
    }
    Ok(())
}

/// Which of `segments` (one generation's, in manifest order) hold vector
/// units a reader of `states` needs, by the rule readers follow: for each
/// member of each state, under each model, the newest segment holding the
/// member's chunk map, and for each chunk of that map the newest segment
/// holding its text's vector. A segment whose vectors and maps no member
/// reaches can be retired once it is past the grace period. Younger vector
/// segments may belong to unpublished changes in another process. Segments
/// without vector sections are left to the other planes' rules.
///
/// # Errors
///
/// When a segment cannot be read.
pub(crate) fn needed_vector_segments(
    segments: &[&Segment],
    states: &[&CheckoutState],
    now: u64,
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
    let grace = u64::try_from(MERGE_GRACE.as_millis()).unwrap_or(u64::MAX);
    for (position, opened) in opened.iter().enumerate() {
        let Some(vectors) = opened else {
            continue;
        };
        needed[position] = now.saturating_sub(segments[position].created_unix_ms()) < grace;
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
    for (position, vectors) in opened.iter().enumerate() {
        if let Some(vectors) = vectors {
            let model = vectors.model().id();
            visit_young_map_texts(segments[position], vectors, now, grace, |text| {
                if let Some(&holder) = texts.get(&(model, text)) {
                    needed[holder] = true;
                }
            })?;
        }
    }
    let mut asked = HashSet::new();
    for state in states {
        for record in state.paths() {
            if !record.kind.has_content() {
                continue;
            }
            let key = ChunkMapKey::new(map_content(record.content, &record.path), &record.path);
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
                    .chunk_map(map_content(record.content, &record.path), &record.path)
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

aft_store::heap_bytes_of_fields!(Coverage {
    skipped_parses,
    files,
    files_embedded,
    files_too_large,
    files_failed,
    pending,
    past_cap,
    chunks,
    chunks_embedded,
    chunks_failed,
});
aft_store::heap_bytes_of_fields!(Loaded {
    generation,
    _snapshot,
    shared,
    maps,
    other,
    open,
});
aft_store::heap_bytes_of_fields!(SearchFile {
    path,
    content,
    holder,
});
aft_store::heap_bytes_of_fields!(Subset { set, positions });
aft_store::heap_bytes_of_fields!(Members {
    memory_epoch,
    view,
    parse_gaps,
    generation,
    files,
    members,
    file_of,
    chunk_of,
    set,
    coverage,
    subset,
});

impl VectorPlane {
    /// What the plane holds in memory, measured as the [`aft_store::heap`]
    /// module describes: the generation of vectors it loaded last (its
    /// snapshot, the index of every vector's text digest, each chunk map's
    /// holder and the segments it opened) and the members it built for the
    /// view searched last. The vectors themselves are read through the
    /// shared block cache, which the process counts once. The checkout the
    /// plane belongs to is counted with the checkout, not here. The old
    /// meaning index, once a fill opened it, adds its index of files and
    /// rows.
    pub(crate) fn memory(&self, seen: &mut aft_store::heap::Seen) -> usize {
        use aft_store::heap::HeapBytes as _;
        let Self {
            checkout: _,
            producer: Producer { model, caps: _ },
            max_files: _,
            loaded,
            members,
            coverage_rebuild: _,
            fill_queue,
            old,
            merge: _,
            merge_due: _,
        } = self;
        model.heap_bytes(seen)
            + loaded.heap_bytes(seen)
            + members.heap_bytes(seen)
            + fill_queue.heap_bytes(seen)
            + old.heap_bytes(seen)
    }
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

    #[test]
    fn vectors_discard_members_built_for_a_superseded_generation() {
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        let checkout = bind(store.path(), dir.path());
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 100);
        let view = checkout.view().unwrap();
        let old = plane.load().unwrap();
        let mut latest = None;
        let survey = plane
            .members_with(&view, &old, || {
                assert!(
                    plane.members.try_lock().is_ok(),
                    "members locked during build"
                );
                assert!(
                    plane.loaded.try_lock().is_ok(),
                    "loaded locked during build"
                );
                let survey = Members::build(&view, &old, 100)?;
                let mut vectors = VectorSectionsBuilder::new(producer().model).unwrap();
                vectors
                    .add_vector(TextDigest::of(b"new"), &vector_of("new"))
                    .unwrap();
                VectorPlane::publish(&checkout.vector_parts(), vectors)?;
                plane.refresh()?;
                latest = Some(plane.snapshot(&view)?);
                Ok(survey)
            })
            .unwrap();
        // The caller still sees its pinned generation, but it cannot replace
        // the survey that a later caller published for the new generation.
        let latest = latest.unwrap();
        assert_eq!(survey.generation, old.generation);
        assert!(latest.loaded.generation > old.generation);
        let cached = plane.cached_snapshot(&view).expect("new survey was lost");
        assert!(Arc::ptr_eq(&cached.members, &latest.members));
    }

    #[test]
    fn vectors_discard_members_built_for_a_superseded_view() {
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        let checkout = bind(store.path(), dir.path());
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 100);
        let view = checkout.view().unwrap();
        let loaded = plane.load().unwrap();
        let mut latest = None;
        plane
            .members_with(&view, &loaded, || {
                let survey = Members::build(&view, &loaded, 100)?;
                write(dir.path(), "src/lib.rs", "pub fn edited() {}\n");
                checkout.record_paths(&[b"src/lib.rs".to_vec()])?;
                latest = Some(plane.snapshot(&checkout.view().unwrap())?);
                Ok(survey)
            })
            .unwrap();
        let settled_view = checkout.view().unwrap();
        assert_ne!(settled_view.text_generation(), view.text_generation());
        let latest = latest.unwrap();
        let cached = plane
            .cached_snapshot(&settled_view)
            .expect("new survey was lost");
        assert!(Arc::ptr_eq(&cached.members, &latest.members));
    }

    #[test]
    fn one_round_jobs_reuse_the_queue_until_the_view_or_shared_vectors_change() {
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        let checkout = bind(store.path(), dir.path());
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 100);
        let round = || {
            plane
                .fill_rounds(
                    &mut |texts| Ok(texts.iter().map(|text| vector_of(text)).collect()),
                    FillBudget {
                        max_batch: 5,
                        files_per_round: 1,
                    },
                    1,
                    &mut |_| {},
                )
                .unwrap()
        };
        FILL_SURVEYS.with(|surveys| surveys.set(0));
        let first = round();
        let second = round();
        assert_eq!(first.files_filled, 1);
        assert_eq!(second.files_filled, 1);
        assert_eq!(second.files_queued + 1, first.files_queued);
        assert_eq!(FILL_SURVEYS.with(std::cell::Cell::get), 1);

        write(dir.path(), "src/lib.rs", "pub fn edited() {}\n");
        checkout.record_paths(&[b"src/lib.rs".to_vec()]).unwrap();
        round();
        assert_eq!(FILL_SURVEYS.with(std::cell::Cell::get), 2);

        let mut external = VectorSectionsBuilder::new(producer().model).unwrap();
        external
            .add_vector(TextDigest::of(b"external"), &vector_of("external"))
            .unwrap();
        VectorPlane::publish(&checkout.vector_parts(), external).unwrap();
        round();
        assert_eq!(FILL_SURVEYS.with(std::cell::Cell::get), 3);
    }

    #[test]
    fn cancelled_fill_neither_dispatches_another_batch_nor_publishes() {
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        let checkout = bind(store.path(), dir.path());
        let family = checkout.vector_parts().family;
        let initial = family.newest_generation().unwrap();
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 100);
        for max_batch in [1, usize::MAX] {
            let token = crate::executor::JobCancellation::new();
            let _context = crate::executor::install_job_cancellation(token.clone());
            let mut calls = 0;
            let result = plane.fill_rounds(
                &mut |texts| {
                    calls += 1;
                    token.request_cancel();
                    Ok(texts.iter().map(|text| vector_of(text)).collect())
                },
                FillBudget {
                    max_batch,
                    files_per_round: 100,
                },
                1,
                &mut |_| {},
            );
            assert!(matches!(result, Err(DataError::Cancelled)));
            assert_eq!(calls, 1);
            assert_eq!(family.newest_generation().unwrap(), initial);
        }
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

    /// A fill dates its progress by what it stores: each round that stores
    /// moves the meaning plane's last-store time and keeps the fill's start,
    /// and what stores nothing (a wait, or a round with nothing left to
    /// fill) leaves it where it was.
    #[test]
    fn each_store_moves_the_fills_progress_time_and_nothing_else_does() {
        use crate::data_layer::meaning::{Meaning, Round};
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        let checkout = bind(store.path(), dir.path());
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 100);
        let meaning = Meaning::default();
        let mut recorder = Recorder::default();
        let mut round = || {
            meaning.note_round_started();
            plane
                .fill_rounds(
                    &mut |texts| recorder.embed(texts),
                    FillBudget {
                        max_batch: 5,
                        files_per_round: 1,
                    },
                    1,
                    &mut |step| meaning.note_progress(step),
                )
                .unwrap()
        };

        assert_eq!(round().segments_published, 1);
        let first = meaning.times();
        let started = first.started_at_ms.expect("the fill started");
        let stored = first.stored_at_ms.expect("the round stored");
        assert!(stored >= started);

        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(round().segments_published, 1);
        let second = meaning.times();
        assert_eq!(second.started_at_ms, Some(started), "one fill, one start");
        assert!(second.stored_at_ms.unwrap() > stored, "a store moves it");

        // A wait for another fill stores nothing.
        meaning.note_progress(FillProgress {
            waiting: true,
            ..FillProgress::default()
        });
        assert_eq!(meaning.times(), second);

        // Fill the rest; once nothing is left, a round stores nothing and
        // the time stays where the last store left it.
        while round().files_left > 0 {}
        let filled = meaning.times();
        std::thread::sleep(Duration::from_millis(5));
        let idle = round();
        assert_eq!((idle.files_queued, idle.segments_published), (0, 0));
        assert_eq!(meaning.times(), filled);

        // A round that finishes the fill ends it; the last store stays.
        meaning.note_round_ended(&Round::Done);
        assert_eq!(
            meaning.times(),
            crate::data_layer::meaning::FillTimes {
                started_at_ms: None,
                stored_at_ms: filled.stored_at_ms,
            }
        );
    }

    /// The plane's memory is what it loaded and built: an unfilled plane
    /// holds its model's identity alone; a filled and searched one also
    /// holds the index of every vector's text and the members of the view
    /// it searched, each at least one entry per vector or member.
    #[test]
    fn the_plane_measures_what_it_loaded_and_built() {
        use aft_store::heap::{heap_bytes_of, Seen};
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        let checkout = bind(store.path(), dir.path());
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 100);
        let identity = heap_bytes_of(&plane.producer.model);
        let empty = plane.memory(&mut Seen::new());
        assert!(empty >= identity);

        fill(&plane, &mut Recorder::default(), 100);
        let view = checkout.view().unwrap();
        let snapshot = plane.snapshot(&view).unwrap();
        let chunks = snapshot.chunks();
        assert!(chunks > 0);
        let loaded = heap_bytes_of(&*plane.loaded.lock().unwrap());
        let members = heap_bytes_of(&*plane.members.lock().unwrap());
        let vectors = snapshot.loaded.shared.as_ref().unwrap().vectors();
        assert!(
            loaded >= vectors * std::mem::size_of::<(TextDigest, aft_store::vector::UnitRef)>(),
            "{loaded} bytes for {vectors} vectors"
        );
        assert!(
            members >= chunks * std::mem::size_of::<Member>(),
            "{members} bytes for {chunks} members"
        );
        assert_eq!(plane.memory(&mut Seen::new()), identity + loaded + members);
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

    #[test]
    fn memory_figure_vector_read_caches_advance_the_checkout_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let store = tempfile::tempdir().unwrap();
        project(&root);
        let checkout = bind(store.path(), &root);
        let filled = VectorPlane::new(Arc::clone(&checkout), producer(), 20_000);
        let mut recorder = Recorder::default();
        assert_eq!(fill(&filled, &mut recorder, 2).error, None);
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 20_000);
        let view = checkout.view().unwrap();
        let epoch = checkout.memory_epoch();
        plane.coverage(&view).unwrap();
        assert!(
            checkout.memory_epoch() > epoch,
            "generation and member caches were loaded"
        );
        let epoch = checkout.memory_epoch();
        plane.coverage(&view).unwrap();
        assert_eq!(
            checkout.memory_epoch(),
            epoch,
            "an unchanged cache stays unchanged"
        );
        let include = |path: &Path| path.starts_with(root.join("src"));
        assert!(!plane
            .search(&view, &vector_of_tie(), 5, &include)
            .unwrap()
            .results
            .is_empty());
        assert!(
            checkout.memory_epoch() > epoch,
            "filtered search cached a subset"
        );
        let epoch = checkout.memory_epoch();
        plane.search(&view, &vector_of_tie(), 5, &include).unwrap();
        assert_eq!(checkout.memory_epoch(), epoch);
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

    /// The family's segments that hold vectors.
    fn vector_segment_count(checkout: &Checkout) -> usize {
        let parts = checkout.vector_parts();
        let snapshot = parts.family.snapshot().unwrap();
        snapshot
            .manifest()
            .segments()
            .iter()
            .filter(|entry| {
                let segment = snapshot
                    .open_segment(entry.id, &parts.known, Arc::new(BlockCache::new(1 << 20)))
                    .unwrap();
                VectorSegment::open(&segment).unwrap().is_some()
            })
            .count()
    }

    /// A file of its own for the merge tests, one function each.
    fn extra_file(root: &Path, index: usize, body: &str) {
        write(
            root,
            &format!("src/extra{index}.rs"),
            &format!("pub fn extra_{index}(value: u8) -> u8 {{\n    {body}\n}}\n"),
        );
    }

    /// What `plane` answers for `view` to every test query.
    fn answers(plane: &VectorPlane, checkout: &Checkout) -> Vec<Vec<String>> {
        let view = checkout.view().unwrap();
        queries()
            .iter()
            .map(|query| rows(&plane.search(&view, query, 10, &|_| true).unwrap().results))
            .collect()
    }

    #[test]
    fn a_fill_round_waits_for_the_second_eligible_vector_segment() {
        let store = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        extra_file(root.path(), 0, "value + 1");
        extra_file(root.path(), 1, "value + 2");
        let checkout = bind(store.path(), root.path());
        let mut plane = VectorPlane::new(Arc::clone(&checkout), producer(), 100);
        plane.merge.merge_at = 1;
        let grace = plane.merge.grace_ms();
        let round = || {
            plane
                .fill_rounds(
                    &mut |texts| Recorder::default().embed(texts),
                    FillBudget {
                        max_batch: 5,
                        files_per_round: 1,
                    },
                    1,
                    &mut |_| {},
                )
                .unwrap()
        };
        assert_eq!(round().segments_published, 1);
        let first = plane.load().unwrap();
        assert_eq!(first.open.len(), 1);
        let first_created = first.open.values().next().unwrap().created_unix_ms();
        drop(first);

        // Write the second segment on a different millisecond. Only the
        // eligibility clock advances by the grace period, not the disk clock.
        let deadline = Instant::now() + Duration::from_secs(2);
        while unix_ms() <= first_created {
            assert!(Instant::now() < deadline, "segment clock did not advance");
            std::thread::sleep(Duration::from_millis(1));
        }
        let now = first_created + grace;
        MERGE_NOW_MS.with(|clock| clock.set(Some(now)));
        MERGE_WRITER_ATTEMPTS.with(|attempts| attempts.set(0));
        let young = round();
        assert_eq!(young.segments_published, 1);
        assert_eq!(young.merges, 0);
        assert_eq!(MERGE_WRITER_ATTEMPTS.with(std::cell::Cell::get), 0);
        let loaded = plane.load().unwrap();
        assert_eq!(loaded.open.len(), 2);
        assert_eq!(
            loaded
                .open
                .values()
                .filter(|segment| now.saturating_sub(segment.created_unix_ms()) >= grace)
                .count(),
            1,
        );
        let due = loaded
            .open
            .values()
            .map(|segment| segment.created_unix_ms())
            .max()
            .unwrap()
            + grace;
        drop(loaded);

        // The next fill sees that the second segment is old enough even
        // though no other writer has published a new manifest.
        MERGE_NOW_MS.with(|clock| clock.set(Some(due)));
        let aged = round();
        MERGE_NOW_MS.with(|clock| clock.set(None));
        assert_eq!(MERGE_WRITER_ATTEMPTS.with(std::cell::Cell::get), 1);
        assert_eq!(aged.segments_published, 0);
        assert_eq!(aged.merges, 1);
        assert_eq!(aged.merged_segments, 2);
        assert_eq!(vector_segment_count(&checkout), 1);
        assert!(plane
            .coverage(&checkout.view().unwrap())
            .unwrap()
            .complete());
    }

    #[test]
    fn compaction_copies_old_vectors_reused_by_a_young_map_from_another_process() {
        let store = tempfile::tempdir().unwrap();
        let editing_root = tempfile::tempdir().unwrap();
        let merging_root = tempfile::tempdir().unwrap();
        let path = b"src/lib.rs";
        let edited = "pub fn changed_in_other_process(value: i32) -> i32 { value + 7 }\n";
        write(editing_root.path(), "src/lib.rs", "pub fn original() {}\n");
        write(merging_root.path(), "src/lib.rs", "pub fn sibling() {}\n");
        // Each bind creates a separate DataLayer, so the merging checkout
        // sees only the editing checkout's published state.
        let editing = bind(store.path(), editing_root.path());
        let merging = bind(store.path(), merging_root.path());
        let editing_plane = VectorPlane::new(Arc::clone(&editing), producer(), 100);
        let mut merging_plane = VectorPlane::new(Arc::clone(&merging), producer(), 100);
        merging_plane.merge.merge_at = 2;
        let parts = editing.vector_parts();

        // Store the reusable vectors with no map, so published states alone
        // cannot cause the merge to copy them.
        let Cut::Chunks(chunks) =
            cut_file(editing.root(), path, edited.as_bytes(), producer().caps)
        else {
            panic!("the edited file must produce chunks");
        };
        let reusable = chunks
            .into_iter()
            .map(|chunk| chunk.text)
            .collect::<HashSet<_>>();
        assert!(!reusable.is_empty());
        let mut old = VectorSectionsBuilder::new(producer().model).unwrap();
        for text in &reusable {
            old.add_vector(TextDigest::of(text.as_bytes()), &vector_of(text))
                .unwrap();
        }
        VectorPlane::publish(&parts, old).unwrap();
        let mut old = VectorSectionsBuilder::new(producer().model).unwrap();
        old.add_vector(TextDigest::of(b"unreferenced"), &vector_of("unreferenced"))
            .unwrap();
        VectorPlane::publish(&parts, old).unwrap();
        let old = editing_plane.load().unwrap();
        let latest_old = old
            .open
            .values()
            .map(|segment| segment.created_unix_ms())
            .max()
            .unwrap();
        let old_ids = old.open.keys().copied().collect::<Vec<_>>();
        assert_eq!(old_ids.len(), 2);
        drop(old);

        let deadline = Instant::now() + Duration::from_secs(2);
        while unix_ms() <= latest_old {
            assert!(Instant::now() < deadline, "segment clock did not advance");
            std::thread::sleep(Duration::from_millis(1));
        }
        write(editing_root.path(), "src/lib.rs", edited);
        editing.record_paths(&[path.to_vec()]).unwrap();
        let view = editing.view().unwrap();
        assert_ne!(
            view.state().get(path).unwrap().content,
            view.current_state().get(path).unwrap().content
        );
        let mut embedder = Recorder::default();
        assert_eq!(fill(&editing_plane, &mut embedder, 1).files_filled, 1);
        assert!(
            embedder.texts.is_empty(),
            "the young map must reuse the old vectors"
        );
        assert!(editing_plane.coverage(&view).unwrap().complete());
        let loaded = editing_plane.load().unwrap();
        let young = *loaded.open.keys().find(|id| !old_ids.contains(id)).unwrap();
        assert!(loaded.open[&young].created_unix_ms() > latest_old);
        drop(loaded);

        MERGE_NOW_MS.with(|clock| clock.set(Some(latest_old + merging_plane.merge.grace_ms())));
        let merged = merging_plane.merge_segments();
        MERGE_NOW_MS.with(|clock| clock.set(None));
        let merged = merged.unwrap().unwrap();
        assert_eq!(merged.merged, 2);
        assert_eq!(merged.vectors, reusable.len());
        let loaded = editing_plane.load().unwrap();
        assert!(
            loaded.open.contains_key(&young),
            "the young map was not merged"
        );
        assert!(old_ids.iter().all(|old| !loaded.open.contains_key(old)));
        assert!(editing_plane.coverage(&view).unwrap().complete());
        for text in reusable {
            assert_eq!(
                editing_plane.stored_vector(&text).unwrap(),
                Some(vector_of(&text))
            );
        }
    }

    /// Merging vector segments keeps exactly what a reader reaches in them:
    /// another checkout's vectors, those only its edit not yet published
    /// reads included, and the merging checkout's alike, so both search and
    /// cover as before; the chunk map and vector of a file content no
    /// checkout holds any more leave with the merged segments. Segments
    /// younger than the grace are not merged.
    #[test]
    fn merged_vector_segments_keep_what_every_checkout_reaches() {
        let store = tempfile::tempdir().unwrap();
        let first_root = tempfile::tempdir().unwrap();
        let first_root = std::fs::canonicalize(first_root.path()).unwrap();
        project(&first_root);
        for index in 0..2 * MERGE_AT {
            extra_file(&first_root, index, &format!("value + {index}"));
        }
        // Both checkouts bound in one process, as a running AFT binds them.
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let bind_here = |root: &Path| {
            let checkout =
                Checkout::bind(Arc::clone(&layer), store.path(), family(), root).unwrap();
            checkout.build().unwrap();
            Arc::new(checkout)
        };
        let first = bind_here(&first_root);
        let first_plane = VectorPlane::new(Arc::clone(&first), producer(), 20_000);
        // Two files a round, so the segment holding the first extra file's
        // vectors holds another file's too, and stays after the first one
        // changes.
        MERGE_WRITER_ATTEMPTS.with(|attempts| attempts.set(0));
        let report = fill(&first_plane, &mut Recorder::default(), 2);
        assert!(report.segments_published > MERGE_AT, "{report:?}");
        assert_eq!(report.merges, 0, "every segment is younger than the grace");
        assert_eq!(MERGE_WRITER_ATTEMPTS.with(std::cell::Cell::get), 0);

        // A second checkout: one function edited, a file added, and the
        // first extra file gone.
        let second_root = tempfile::tempdir().unwrap();
        let second_root = std::fs::canonicalize(second_root.path()).unwrap();
        copy_tree(&first_root, &second_root);
        let lib = std::fs::read_to_string(second_root.join("src/lib.rs")).unwrap();
        write(&second_root, "src/lib.rs", &lib.replace("x + 1", "x + 2"));
        write(
            &second_root,
            "src/second.rs",
            "pub fn second_only(items: &[u8]) -> usize {\n    items.len()\n}\n",
        );
        std::fs::remove_file(second_root.join("src/extra0.rs")).unwrap();
        let second = bind_here(&second_root);
        let second_plane = VectorPlane::new(Arc::clone(&second), producer(), 20_000);
        fill(&second_plane, &mut Recorder::default(), 1);

        // The first checkout's first extra file changes and is published:
        // no checkout holds its old content any more.
        extra_file(&first_root, 0, "value * 7");
        first.record_paths(&[b"src/extra0.rs".to_vec()]).unwrap();
        first.refresh_callgraph().unwrap();
        first.publish_changes().unwrap().expect("published");
        fill(&first_plane, &mut Recorder::default(), 1);

        // The second checkout then edits its own file and does not publish
        // the edit: what its fill embeds for it only its unpublished state
        // reads.
        write(
            &second_root,
            "src/second.rs",
            "pub fn second_only(items: &[u8]) -> usize {\n    items.len() * 3\n}\n",
        );
        second.record_paths(&[b"src/second.rs".to_vec()]).unwrap();
        second.refresh_callgraph().unwrap();
        let unpublished = fill(&second_plane, &mut Recorder::default(), 1);
        assert!(unpublished.chunks_embedded >= 1, "{unpublished:?}");

        let first_before = answers(&first_plane, &first);
        let second_before = answers(&second_plane, &second);
        assert!(first_before.iter().any(|rows| !rows.is_empty()));
        for (plane, checkout) in [(&first_plane, &first), (&second_plane, &second)] {
            let coverage = plane.coverage(&checkout.view().unwrap()).unwrap();
            assert!(coverage.complete(), "before the merge: {coverage:?}");
        }
        let segments_before = vector_segment_count(&first);
        let bound = first.bound_states();
        let limits = MergeLimits {
            grace: Duration::ZERO,
            ..MergeLimits::default()
        };
        let merged =
            merge_vector_segments(&first.vector_parts(), producer().model(), &bound, limits)
                .unwrap()
                .expect("merged");
        assert!(merged.merged >= 2, "{merged:?}");
        assert!(
            merged.dropped_chunk_maps >= 1 && merged.dropped_vectors >= 1,
            "the old content's map and vector leave: {merged:?}"
        );
        assert_eq!(
            vector_segment_count(&first),
            segments_before - merged.merged + 1,
            "{merged:?}"
        );

        for (plane, checkout, before) in [
            (&first_plane, &first, &first_before),
            (&second_plane, &second, &second_before),
        ] {
            plane.refresh().unwrap();
            assert_eq!(&answers(plane, checkout), before);
            let coverage = plane.coverage(&checkout.view().unwrap()).unwrap();
            assert!(coverage.complete(), "{coverage:?}");
            // Nothing is embedded again.
            let mut idle = Recorder::default();
            fill(plane, &mut idle, 1);
            assert!(idle.texts.is_empty(), "{:?}", idle.texts);
        }
    }

    /// Past [`MERGE_AT`] vector segments, a fill round merges them, so a
    /// fill one file per round leaves a bounded number, and its searches
    /// answer as today's index does over the same vectors.
    #[test]
    fn a_fill_merges_vector_segments_past_the_limit() {
        let store = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        project(&root);
        for index in 0..2 * MERGE_AT {
            extra_file(&root, index, &format!("value ^ {index}"));
        }
        let checkout = bind(store.path(), &root);
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 20_000)
            .with_merge_grace(Duration::ZERO);
        let mut recorder = Recorder::default();
        let report = fill(&plane, &mut recorder, 1);
        assert!(report.segments_published > 2 * MERGE_AT, "{report:?}");
        assert!(report.merges >= 1, "{report:?}");
        assert!(
            vector_segment_count(&checkout) <= MERGE_AT + 1,
            "{report:?}: {}",
            vector_segment_count(&checkout)
        );

        let view = checkout.view().unwrap();
        let files: Vec<PathBuf> = view
            .current_state()
            .paths()
            .iter()
            .filter(|record| is_semantic_path(&record.path))
            .map(|record| root.join(native_path(&record.path).unwrap()))
            .collect();
        let today = SemanticIndex::build(
            &root,
            &files,
            &mut |texts| Recorder::default().embed(texts),
            5,
        )
        .unwrap();
        for query in queries() {
            assert_eq!(
                rows(
                    &plane
                        .search(&view, &query, 1000, &|_| true)
                        .unwrap()
                        .results
                ),
                rows(&today.search_filtered(&query, 1000, |_| true)),
            );
        }
    }

    #[test]
    fn vector_segments_no_checkout_needs_are_retired_after_grace() {
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
        // The only file changes. Its old vectors remain eligible for reuse
        // during grace but are no longer needed once the grace has passed.
        write(
            &root,
            "src/only.rs",
            "pub fn second_version(b: u16) -> u16 {\n    b\n}\n",
        );
        checkout.build().unwrap();
        assert_eq!(vector_segments(), before);
        let snapshot = family_store.snapshot().unwrap();
        let old = snapshot
            .open_segment(
                before[0],
                &checkout.vector_parts().known,
                Arc::new(BlockCache::new(1 << 20)),
            )
            .unwrap();
        let view = checkout.view().unwrap();
        let expiry = old.created_unix_ms() + u64::try_from(MERGE_GRACE.as_millis()).unwrap();
        assert_eq!(
            needed_vector_segments(&[&old], &[view.current_state()], expiry).unwrap(),
            [false]
        );
        // A checkout still holding a content keeps its vectors.
        let mut recorder = Recorder::default();
        fill(&plane, &mut recorder, 10);
        assert!(!recorder.texts.is_empty());
        let kept = vector_segments();
        checkout.build().unwrap();
        assert_eq!(vector_segments(), kept);
    }

    #[test]
    fn sibling_publication_keeps_fresh_maps_for_unpublished_content() {
        let store = tempfile::tempdir().unwrap();
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        write(left.path(), "src/lib.rs", "pub fn before() {}\n");
        write(right.path(), "src/lib.rs", "pub fn sibling() {}\n");
        // Separate layers model processes that cannot see each other's live states.
        let left_checkout = bind(store.path(), left.path());
        let right_checkout = bind(store.path(), right.path());
        write(left.path(), "src/lib.rs", "pub fn still_unpublished() {}\n");
        left_checkout
            .record_paths(&[b"src/lib.rs".to_vec()])
            .unwrap();
        let plane = VectorPlane::new(Arc::clone(&left_checkout), producer(), 20_000);
        assert_eq!(fill(&plane, &mut Recorder::default(), 10).files_filled, 1);

        right_checkout.build().unwrap();
        plane.refresh().unwrap();
        let view = left_checkout.view().unwrap();
        let coverage = plane.coverage(&view).unwrap();
        assert_eq!(coverage.files_embedded, 1);
        assert!(coverage.pending.is_empty());
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

    /// A refresh or search after nothing new was published opens no store
    /// file. The test swaps the newest manifest for a directory of the same
    /// name, which any attempt to read it as a record fails on, while the
    /// directory listing still shows the name.
    #[test]
    fn a_refresh_with_nothing_published_opens_no_store_file() {
        let store = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        project(&root);
        let checkout = bind(store.path(), &root);
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 20_000);
        fill(&plane, &mut Recorder::default(), 10);
        plane.refresh().unwrap();
        let generation = plane.loaded.lock().unwrap().generation;
        assert!(generation > Generation::EMPTY);
        let family_dir = checkout.vector_parts().family.dir().to_path_buf();
        let manifest = family_dir.join(format!("MANIFEST-{}", generation.get()));
        std::fs::remove_file(&manifest).unwrap();
        std::fs::create_dir(&manifest).unwrap();
        let view = checkout.view().unwrap();
        for _ in 0..3 {
            plane.refresh().unwrap();
            plane
                .search(&view, &[0.0; DIMENSIONS], 5, &|_| true)
                .unwrap();
        }
        assert_eq!(plane.loaded.lock().unwrap().generation, generation);

        // A newer manifest is still noticed: one numbered above the loaded
        // generation is opened, and a torn one is skipped as before.
        std::fs::write(
            family_dir.join(format!("MANIFEST-{}", generation.get() + 1)),
            b"torn",
        )
        .unwrap();
        assert!(checkout
            .vector_parts()
            .family
            .has_manifest_above(generation)
            .unwrap());
        assert!(
            plane.refresh().is_err(),
            "the newest valid manifest is now unreadable"
        );
    }

    /// What a refresh with nothing new published costs, before and after it
    /// stopped reading the newest manifest: the old check
    /// (`newest_generation`: a listing plus a read, checksum and decode of
    /// the manifest) against the new one (`has_manifest_above`: a listing).
    /// Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "timing measurement"]
    fn refresh_with_nothing_published_timing() {
        let store = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        project(&root);
        let checkout = bind(store.path(), &root);
        let plane = VectorPlane::new(Arc::clone(&checkout), producer(), 20_000);
        // One file per round: one segment per file, a longer manifest.
        fill(&plane, &mut Recorder::default(), 1);
        plane.refresh().unwrap();
        let generation = plane.loaded.lock().unwrap().generation;
        let parts = checkout.vector_parts();
        let rounds = 2_000;
        let started = Instant::now();
        for _ in 0..rounds {
            assert_eq!(parts.family.newest_generation().unwrap(), generation);
        }
        let old = started.elapsed();
        let started = Instant::now();
        for _ in 0..rounds {
            assert!(!parts.family.has_manifest_above(generation).unwrap());
        }
        let new = started.elapsed();
        let started = Instant::now();
        for _ in 0..rounds {
            plane.refresh().unwrap();
        }
        let refresh = started.elapsed();
        let per = |total: Duration| total.as_secs_f64() * 1e6 / f64::from(rounds);
        println!(
            "no-change refresh check: newest_generation {:.1} us, has_manifest_above {:.1} us, refresh {:.1} us",
            per(old),
            per(new),
            per(refresh)
        );
    }
}

#[cfg(test)]
thread_local! {
    static STATUS_MEMBER_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn status_member_builds_for_test() -> usize {
    STATUS_MEMBER_BUILDS.with(|count| count.replace(0))
}
