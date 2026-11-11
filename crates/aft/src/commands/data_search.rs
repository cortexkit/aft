//! aft_search's lexical and exact lanes, read from the data layer; its
//! semantic lane is in [`meaning`].
//!
//! The lanes ask the text index four things: which files hold a query's
//! trigrams and how many distinct trigrams each file has (the lexical
//! score), which files may hold a literal (the exact pass, the anchored lane
//! and the missing-literal check), and how a file is held (the recall
//! audit). [`TextIndex`] answers them from either source: the legacy
//! [`SearchIndexSnapshot`], or a data-layer [`CheckoutView`] whose text plane
//! is ready. Each data-layer answer follows the legacy one's rules, so a
//! query ranks the same files the same way from either source; where the two
//! sources count differently, the function says so.
//!
//! The view names paths relative to its root with `/` separators; the lanes
//! work with absolute paths, joined to the view's root.

use crate::semantic_index::{SemanticIndex, SemanticResult};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

pub(crate) use crate::data_layer::meaning_query as meaning;
pub(crate) use crate::parent_search::external::ExternalSearch;

pub(crate) use meaning::{DataMeaning, DataSemantic};

use aft_store::text::{plan_pattern, PathText, Query, Scope};

use crate::commands::semantic_search::exact_lane::{self, ExactEvidenceScope};
use crate::commands::semantic_search::waits::{self, SearchWait};
use crate::data_layer::checkout::{CheckoutView, DataError, PlaneStatus};
use crate::search_index::{
    read_request_corpus_file, ExactPassMatch, RequestCorpusFile, SearchIndexSnapshot,
};

/// A data-layer view whose text plane covers every path of the checkout,
/// with what the lanes need beside it.
pub(crate) struct DataText {
    view: CheckoutView,
    /// The absolute folder the view's relative paths are joined to.
    root: PathBuf,
    /// The largest file the exact pass reads, as the legacy index's limit.
    max_file_size: u64,
}

impl std::fmt::Debug for DataText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DataText")
            .field("root", &self.root)
            .field("max_file_size", &self.max_file_size)
            .finish()
    }
}

impl DataText {
    /// This root's data-layer view when the process runs the data layer
    /// and the view's text plane is ready; `None` otherwise, and the caller
    /// keeps reading the legacy index.
    pub(crate) fn ready(ctx: &crate::context::AppContext) -> Option<Self> {
        // `AppContext::data_view`, with its two waits timed apart: the
        // context's checkout slot, held while a checkout binds (a bind scans
        // the folder), and the view, cloned under the checkout's `live` lock.
        let checkout = waits::timed(SearchWait::CheckoutSlot, || ctx.data_checkout())?;
        let view = waits::timed(SearchWait::TextView, || checkout.view())
            .filter(|view| view.planes().text)?;
        if view.status().text != PlaneStatus::Ready {
            return None;
        }
        let max_file_size = ctx.config().search_index_max_file_size;
        Some(Self::from_view(view, max_file_size))
    }

    /// Wraps `view`, whose exact pass reads files of up to `max_file_size`
    /// bytes.
    pub(crate) fn from_view(view: CheckoutView, max_file_size: u64) -> Self {
        let root = view.root().to_path_buf();
        Self {
            view,
            root,
            max_file_size,
        }
    }

    /// Changes whenever any text answer of the view can change: a new
    /// publication, or a change recorded over it. The exact lane's memo is
    /// keyed by it.
    pub(crate) fn generation(&self) -> String {
        self.view.text_generation()
    }

    /// Logs, at debug level, that a ranking read this view. The data-layer
    /// parity harness (`benchmarks/aft-search/compare_data_layer.py`) counts
    /// these lines to show which source answered each request.
    pub(crate) fn note_read(&self, request_id: &str) {
        crate::slog_debug!(
            "aft_search {request_id} read the data layer view at {}",
            self.generation()
        );
    }

    fn absolute(&self, relative: &[u8]) -> PathBuf {
        absolute(&self.root, relative)
    }

    /// `path` relative to the view's root, as the view names it; `None`
    /// when it is outside the root.
    fn relative(&self, path: &Path) -> Option<Vec<u8>> {
        relative(&self.root, path)
    }

    /// The paths that may hold a match of `pattern`, read as a regular
    /// expression with ASCII case folded the way the index folds it; every
    /// path whose content is not indexed is among them.
    fn candidates(&self, pattern: &str) -> io::Result<BTreeSet<Vec<u8>>> {
        let query = plan_pattern(pattern, false)
            .map_err(|error| io::Error::other(format!("lexical query plan failed: {error}")))?;
        self.candidates_for(&query)
    }

    fn candidates_for(&self, query: &Query) -> io::Result<BTreeSet<Vec<u8>>> {
        Ok(self
            .view
            .text_candidates(query, Scope::All)
            .map_err(store_error)?
            .into_iter()
            .collect())
    }

    /// Every path a text query can return: the legacy index's active files.
    fn every_path(&self) -> io::Result<BTreeSet<Vec<u8>>> {
        self.candidates_for(&Query::All)
    }

    /// The files the project walk over `root` would list and keep with
    /// `matches`, newest first as that walk orders them, read from this
    /// view's path list instead of the folder. `admit_relative` is a cheap
    /// test of a member's `/`-separated relative path that every path
    /// `matches` keeps must pass; only those members are spelled out and
    /// tested with `matches`.
    ///
    /// The index store's scan applies the walk's ignore files, skipped
    /// folders, hidden-path and nested-worktree rules; the walk's other
    /// exclusions (AFT's storage, watcher markers, OS metadata files,
    /// database files) are applied here per path, and a member that is not
    /// a regular file on disk now is left out, as the walk would not reach
    /// it. Symlinks are never listed, as the walk does not follow them.
    ///
    /// `None` when the view cannot stand in for the walk: it is of another
    /// folder, a rescan against the disk is still pending, or the walk at
    /// `root` uses other ignore settings than the store's scan.
    pub(crate) fn files_matching(
        &self,
        root: &Path,
        admit_relative: impl Fn(&[u8]) -> bool,
        matches: impl Fn(&Path) -> bool,
    ) -> Option<Vec<PathBuf>> {
        use crate::path_identity::canonicalize_normalized;
        if self.view.rescanning()
            || canonicalize_normalized(&self.root) != canonicalize_normalized(root)
            || walk_uses_other_ignore_settings(root)
        {
            return None;
        }
        let mut files = self
            .view
            .current_state()
            .paths()
            .iter()
            .filter(|record| record.kind != aft_store::checkout::PathKind::Symlink)
            .filter(|record| admit_relative(&record.path))
            .map(|record| absolute(root, &record.path))
            .filter(|path| {
                !crate::project_content::is_excluded_file_by_path(path)
                    && matches(path)
                    && std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
            })
            .collect::<Vec<_>>();
        crate::search_index::sort_paths_by_mtime_desc(&mut files, root);
        Some(files)
    }

    /// The paths whose indexed content holds each of `trigrams`, read once
    /// per distinct trigram.
    fn postings(&self, trigrams: &[u32]) -> io::Result<HashMap<u32, Holders>> {
        let published = self.view.state().paths().len();
        let mut postings = HashMap::with_capacity(trigrams.len());
        for &trigram in trigrams {
            if let std::collections::hash_map::Entry::Vacant(entry) = postings.entry(trigram) {
                let holders = self.view.trigram_holders(trigram).map_err(store_error)?;
                entry.insert(Holders::new(holders, published));
            }
        }
        Ok(postings)
    }

    /// The relative path of `candidate`.
    fn candidate_path<'a>(&'a self, candidate: &Candidate<'a>) -> &'a [u8] {
        match candidate {
            Candidate::Published(index) => self.view.published_path(*index),
            Candidate::Changed(path) => path,
        }
    }

    /// The union of `trigrams`' holders, in byte order of their paths.
    fn union<'a>(&'a self, trigrams: impl IntoIterator<Item = &'a Holders>) -> Vec<Candidate<'a>> {
        let mut published = vec![0u64; self.view.state().paths().len().div_ceil(64)];
        let mut changed = BTreeSet::new();
        for holders in trigrams {
            for (word, bits) in published.iter_mut().zip(&holders.bits) {
                *word |= bits;
            }
            changed.extend(holders.changed.iter().map(Vec::as_slice));
        }
        let published = published.iter().enumerate().flat_map(|(word, bits)| {
            (0..64)
                .filter(move |bit| bits & (1 << bit) != 0)
                .map(move |bit| word * 64 + bit)
        });
        merge_in_path_order(self, published, changed.into_iter())
    }

    #[cfg(test)]
    fn frequency(&self, trigram: u32) -> io::Result<u64> {
        self.view.trigram_frequency(trigram).map_err(store_error)
    }

    /// The lexical score of the path, as the legacy index scores a file:
    /// the query trigrams it holds, over one plus the log of its distinct
    /// trigram count.
    fn score(
        &self,
        candidate: &Candidate<'_>,
        query_trigrams: &[u32],
        postings: &HashMap<u32, Holders>,
    ) -> io::Result<f32> {
        if query_trigrams.is_empty() {
            return Ok(0.0);
        }
        let hits = query_trigrams
            .iter()
            .filter(|trigram| {
                postings
                    .get(trigram)
                    .is_some_and(|holders| holders.holds(candidate))
            })
            .count() as u32;
        if hits == 0 {
            return Ok(0.0);
        }
        let text = match candidate {
            Candidate::Published(index) => Some(self.view.published_path_text(*index)),
            Candidate::Changed(path) => self.view.path_text(path).transpose(),
        };
        let distinct = match text.transpose().map_err(store_error)? {
            Some(PathText::Indexed { distinct }) => distinct,
            _ => 1,
        };
        Ok((hits as f32) / (1.0 + (distinct.max(1) as f32).ln()))
    }
}

/// One query trigram's holders in a view, kept as positions so scoring tests
/// membership with a bit instead of comparing path bytes: a lexical query
/// scores thousands of candidates against dozens of trigrams.
pub(crate) struct Holders {
    /// A bit per published path, by position.
    bits: Vec<u64>,
    /// How many published paths hold the trigram.
    published: usize,
    /// Paths changed since the publication that hold it, in byte order.
    changed: Vec<Vec<u8>>,
}

impl Holders {
    fn new(holders: aft_store::text::TrigramHolders, paths: usize) -> Self {
        let mut bits = vec![0u64; paths.div_ceil(64)];
        for &index in &holders.published {
            bits[index / 64] |= 1 << (index % 64);
        }
        Self {
            bits,
            published: holders.published.len(),
            changed: holders.changed,
        }
    }

    /// How many paths hold the trigram.
    pub(crate) fn len(&self) -> usize {
        self.published + self.changed.len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn holds(&self, candidate: &Candidate<'_>) -> bool {
        match candidate {
            Candidate::Published(index) => self
                .bits
                .get(index / 64)
                .is_some_and(|word| word & (1 << (index % 64)) != 0),
            Candidate::Changed(path) => self
                .changed
                .binary_search_by(|held| held.as_slice().cmp(path))
                .is_ok(),
        }
    }
}

/// A path the lexical lane may score: a published path by its position, or
/// a path changed since the publication. No path is both (see
/// [`aft_store::text::TrigramHolders`]).
enum Candidate<'a> {
    Published(usize),
    Changed(&'a [u8]),
}

/// `published` positions (ascending) and `changed` paths (in byte order)
/// as one list in byte order of their paths, as the lanes rank ties.
fn merge_in_path_order<'a>(
    text: &'a DataText,
    published: impl Iterator<Item = usize>,
    changed: impl Iterator<Item = &'a [u8]>,
) -> Vec<Candidate<'a>> {
    let mut published = published.peekable();
    let mut changed = changed.peekable();
    let mut merged = Vec::new();
    loop {
        let next = match (published.peek(), changed.peek()) {
            (Some(&index), Some(path)) => {
                if text.view.published_path(index) <= *path {
                    published.next().map(Candidate::Published)
                } else {
                    changed.next().map(Candidate::Changed)
                }
            }
            (Some(_), None) => published.next().map(Candidate::Published),
            (None, Some(_)) => changed.next().map(Candidate::Changed),
            (None, None) => None,
        };
        let Some(next) = next else {
            return merged;
        };
        merged.push(next);
    }
}

fn store_error(error: DataError) -> io::Error {
    io::Error::other(error.to_string())
}

/// Whether the project walk at `root` reads ignore files differently from
/// the index store's scan, so the view's members cannot stand in for the
/// walk's files: under confinement the walk reads no ignore file, and in a
/// Jujutsu-only repository it applies Git's rules where the scan does not.
/// `Inventory::from_checkout` (in `inspect::inventory`) makes the same check
/// before it lists a checkout's sources.
fn walk_uses_other_ignore_settings(root: &Path) -> bool {
    crate::context::confinement::active()
        || (root.ancestors().any(|path| path.join(".jj").exists())
            && !root.ancestors().any(|path| path.join(".git").exists()))
}

/// The absolute path of the view's `relative` path under `root`.
fn absolute(root: &Path, relative: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        root.join(std::ffi::OsStr::from_bytes(relative))
    }
    #[cfg(not(unix))]
    {
        let relative = String::from_utf8_lossy(relative);
        root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
    }
}

fn relative(root: &Path, path: &Path) -> Option<Vec<u8>> {
    let relative = path.strip_prefix(root).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(relative.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        Some(relative.to_string_lossy().replace('\\', "/").into_bytes())
    }
}

/// Where aft_search's lexical and exact lanes read the text index from.
#[derive(Clone, Copy)]
pub(crate) enum TextIndex<'a> {
    /// The legacy trigram index (or a views projection of it).
    Snapshot(&'a SearchIndexSnapshot),
    /// The data layer's view.
    Data(&'a DataText),
}

/// The query trigrams in their first-seen order, each once.
fn unique_trigrams(query_trigrams: &[u32]) -> Vec<u32> {
    let mut seen = HashSet::with_capacity(query_trigrams.len());
    query_trigrams
        .iter()
        .copied()
        .filter(|trigram| seen.insert(*trigram))
        .collect()
}

impl TextIndex<'_> {
    /// The lexical lane's pool, scored: the files holding one of the three
    /// rarest query trigrams that some file holds, each with its lexical
    /// score, best first. Ties on rarity go to the smaller trigram. A
    /// trigram's rarity is how many of this checkout's paths hold it (see
    /// [`rarest`]).
    pub(crate) fn lexical_selected_pool(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
    ) -> io::Result<Vec<(PathBuf, f32)>> {
        match self {
            Self::Snapshot(snapshot) => {
                snapshot.lexical_selected_pool(query_trigrams, candidate_filter)
            }
            Self::Data(text) => waits::timed(SearchWait::TextSearch, || {
                selected_pool(text, query_trigrams, candidate_filter, paths_holding)
            }),
        }
    }

    /// Every file holding one of the rarest query trigrams, widened by the
    /// next rarest until at least `depth` files are found, scored and cut to
    /// the best `depth`; as `SearchIndexSnapshot::lexical_rank_at_depth`.
    /// Rarity is how many of this checkout's paths hold a trigram
    /// ([`paths_holding`]), as for [`Self::lexical_selected_pool`].
    pub(crate) fn lexical_rank_at_depth(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        depth: usize,
    ) -> io::Result<Vec<(PathBuf, f32)>> {
        let text = match self {
            Self::Snapshot(snapshot) => {
                return snapshot
                    .lexical_rank_at_depth(query_trigrams, candidate_filter, depth)
                    .map(|result| result.files)
            }
            Self::Data(text) => text,
        };
        if query_trigrams.is_empty() || depth == 0 {
            return Ok(Vec::new());
        }
        let _wait = waits::time(SearchWait::TextSearch);
        let postings = text.postings(query_trigrams)?;
        let mut non_zero = query_trigrams
            .iter()
            .map(|trigram| (*trigram, postings[trigram].len()))
            .filter(|(_, count)| *count > 0)
            .collect::<Vec<_>>();
        if non_zero.is_empty() {
            return Ok(Vec::new());
        }
        non_zero.sort_unstable_by_key(|(_, count)| *count);
        let initial = non_zero.len().min(3);
        let mut picked = non_zero[..initial]
            .iter()
            .map(|(trigram, _)| &postings[trigram])
            .collect::<Vec<_>>();
        let mut candidates = text.union(picked.iter().copied());
        for (trigram, _) in non_zero.iter().skip(initial) {
            if candidates.len() >= depth {
                break;
            }
            picked.push(&postings[trigram]);
            candidates = text.union(picked.iter().copied());
        }
        let mut ranked = Vec::new();
        let admitted = candidates
            .iter()
            .map(|candidate| (candidate, text.absolute(text.candidate_path(candidate))))
            .filter(|(_, path)| candidate_filter.is_none_or(|include| include(path)))
            .take(depth);
        for (candidate, path) in admitted {
            let score = text.score(candidate, query_trigrams, &postings)?;
            if score > 0.0 {
                ranked.push((path, score));
            }
        }
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(depth);
        Ok(ranked)
    }

    /// How the index holds `path`, for the benchmark-only recall audit:
    /// `indexed`, `unindexed` (known, content not indexed) or `absent`.
    pub(crate) fn audit_index_state(&self, path: &Path) -> &'static str {
        let text = match self {
            Self::Snapshot(snapshot) => return snapshot.audit_index_state(path),
            Self::Data(text) => text,
        };
        let Some(relative) = text.relative(path) else {
            return "absent";
        };
        match text.view.path_text(&relative) {
            Ok(Some(PathText::Indexed { .. })) => "indexed",
            Ok(Some(PathText::Unindexed)) => "unindexed",
            Ok(Some(PathText::Excluded) | None) | Err(_) => "absent",
        }
    }
}

/// The lexical lane's scored pool from the data layer's view, picking the
/// three rarest query trigrams by `rarity` (the lane passes
/// [`paths_holding`]).
fn selected_pool(
    text: &DataText,
    query_trigrams: &[u32],
    candidate_filter: Option<&dyn Fn(&Path) -> bool>,
    rarity: impl Fn(u32, &Holders) -> io::Result<u64>,
) -> io::Result<Vec<(PathBuf, f32)>> {
    let unique = unique_trigrams(query_trigrams);
    let postings = text.postings(&unique)?;
    let picked = rarest(&unique, &postings, rarity)?;
    let selected = text.union(picked.iter().map(|trigram| &postings[trigram]));
    let mut ranked = Vec::new();
    for candidate in &selected {
        let path = text.absolute(text.candidate_path(candidate));
        if candidate_filter.is_some_and(|include| !include(&path)) {
            continue;
        }
        let score = text.score(candidate, &unique, &postings)?;
        if score > 0.0 {
            ranked.push((path, score));
        }
    }
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(ranked)
}

/// A query trigram's rarity for the lexical lane: how many of this
/// checkout's paths hold it (`paths`, its postings), not how many distinct
/// contents do. A content shared by identical files, or one left in older
/// segments of the family, is no rarer to an agent than the files it can
/// actually hit here. Counting contents (`trigram_frequency`) picked another
/// three trigrams for 113 of 6,700 recorded aft_search queries on aft's own
/// checkout and moved their lexical ranking; the legacy index counts files,
/// as this does.
fn paths_holding(_trigram: u32, holders: &Holders) -> io::Result<u64> {
    Ok(holders.len() as u64)
}

/// The (at most) three rarest of `unique` that some path holds, by
/// `rarity` of each trigram and its postings, rarest first, ties to the
/// smaller trigram.
fn rarest(
    unique: &[u32],
    postings: &HashMap<u32, Holders>,
    rarity: impl Fn(u32, &Holders) -> io::Result<u64>,
) -> io::Result<Vec<u32>> {
    let mut held = Vec::new();
    for &trigram in unique {
        let holders = &postings[&trigram];
        if holders.is_empty() {
            continue;
        }
        held.push((rarity(trigram, holders)?, trigram));
    }
    held.sort_unstable();
    Ok(held
        .into_iter()
        .take(3)
        .map(|(_, trigram)| trigram)
        .collect())
}

impl TextIndex<'_> {
    /// The whole-corpus exact pass, as
    /// `SearchIndexSnapshot::whole_corpus_exact_pass_scoped`: the files that
    /// may hold the query's phrase, its words or its member name, narrowed to
    /// those able to hold the evidence `scope` keeps, each read and verified.
    pub(crate) fn whole_corpus_exact_pass_scoped(
        &self,
        query: &str,
        search_root: &Path,
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        scope: &ExactEvidenceScope,
    ) -> io::Result<Vec<ExactPassMatch>> {
        match self {
            Self::Snapshot(snapshot) => {
                snapshot.whole_corpus_exact_pass_scoped(query, search_root, candidate_filter, scope)
            }
            Self::Data(text) => exact_pass(text, query, search_root, candidate_filter, scope),
        }
    }

    /// [`Self::whole_corpus_exact_pass_scoped`] keeping every evidence kind.
    pub(crate) fn whole_corpus_exact_pass(
        &self,
        query: &str,
        search_root: &Path,
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
    ) -> io::Result<Vec<ExactPassMatch>> {
        self.whole_corpus_exact_pass_scoped(
            query,
            search_root,
            candidate_filter,
            &ExactEvidenceScope::All,
        )
    }

    /// Indexed files under `search_root` that may hold `phrase`, as
    /// `SearchIndexSnapshot::files_possibly_holding_phrase`; `None` when no
    /// whitespace-separated piece of it has a trigram.
    pub(crate) fn files_possibly_holding_phrase(
        &self,
        phrase: &str,
        search_root: &Path,
    ) -> io::Result<Option<Vec<PathBuf>>> {
        let text = match self {
            Self::Snapshot(snapshot) => {
                return snapshot.files_possibly_holding_phrase(phrase, search_root)
            }
            Self::Data(text) => text,
        };
        let _wait = waits::time(SearchWait::TextSearch);
        let mut found: Option<BTreeSet<Vec<u8>>> = None;
        for piece in phrase.split_whitespace().filter(|piece| piece.len() >= 3) {
            let holders = text.candidates(&regex::escape(piece))?;
            found = Some(match found {
                Some(found) => found.intersection(&holders).cloned().collect(),
                None => holders,
            });
        }
        let Some(found) = found else {
            return Ok(None);
        };
        let search_root_canon = canonicalize_for_search_membership(search_root);
        Ok(Some(
            found
                .iter()
                .map(|relative| text.absolute(relative))
                .filter(|path| {
                    is_within_search_root(search_root, path)
                        || is_within_search_root(&search_root_canon, path)
                })
                .collect(),
        ))
    }
}

/// Memoized "files possibly containing" answers for one exact pass, by
/// literal: `None` when the literal has no trigram, so any file may hold it.
type LiteralHolders = HashMap<String, Option<BTreeSet<Vec<u8>>>>;

fn exact_pass(
    text: &DataText,
    query: &str,
    search_root: &Path,
    candidate_filter: Option<&dyn Fn(&Path) -> bool>,
    scope: &ExactEvidenceScope,
) -> io::Result<Vec<ExactPassMatch>> {
    let phrase = exact_lane::exact_phrase(query);
    let norm_phrase = exact_lane::normalize_exact_phrase(phrase);
    let content_tokens = exact_lane::exact_verification_tokens(query);
    let literal = crate::search_b2::router::is_hyphenated_literal(query);
    let member = exact_lane::qualified_member_name(query);
    let mut holders = LiteralHolders::new();
    let mut candidates = BTreeSet::new();
    let candidate_wait = waits::time(SearchWait::TextSearch);

    // The verbatim phrase's trigrams.
    if phrase.len() >= 3 {
        candidates.extend(text.candidates(&regex::escape(phrase))?);
    }
    // Each content word's trigrams, for window evidence.
    if content_tokens.len() >= 2 {
        for token in content_tokens.iter().filter(|token| token.len() >= 3) {
            match possibly_containing(text, token, &mut holders)? {
                Some(paths) => candidates.extend(paths.iter().cloned()),
                None => candidates.extend(text.every_path()?),
            }
        }
    }
    // A `receiver.member` query: files holding the member name, which its
    // declaration holds without the receiver.
    let member_paths = match member.as_ref() {
        Some(member) => Some(text.candidates(&regex::escape(&member.member))?),
        None => None,
    };
    if let Some(paths) = &member_paths {
        candidates.extend(paths.iter().cloned());
    }
    // A query with no trigram to look up reads every file; a hyphenated
    // literal matches only through its own phrase trigrams.
    if candidates.is_empty() && (phrase.len() < 3 || (content_tokens.is_empty() && !literal)) {
        candidates.extend(text.every_path()?);
    }
    if let Some(bound) = evidence_bound(text, scope, &norm_phrase, &content_tokens, &mut holders)? {
        candidates.retain(|path| {
            bound.contains(path)
                || member_paths
                    .as_ref()
                    .is_some_and(|paths| paths.contains(path))
        });
    }
    drop(candidate_wait);

    let search_root_canon = canonicalize_for_search_membership(search_root);
    let request_files = crate::commands::semantic_search::request_files::current();
    let mut matches = Vec::new();
    for relative in candidates {
        let path = text.absolute(&relative);
        let in_scope = is_within_search_root(search_root, &path)
            || is_within_search_root(&search_root_canon, &path)
            || is_within_search_root(
                &search_root_canon,
                &canonicalize_for_search_membership(&path),
            );
        if !in_scope || candidate_filter.is_some_and(|include| !include(&path)) {
            continue;
        }
        let RequestCorpusFile::Eligible {
            file,
            generated,
            fresh,
        } = waits::timed(SearchWait::FileRead, || {
            read_request_corpus_file(request_files.as_deref(), &path, text.max_file_size)
        })
        else {
            continue;
        };
        let digest = file.digest();
        let found = exact_lane::verify_cached_exact_matches_in_text_with_member(
            &path,
            file.lossy_text(),
            digest,
            &norm_phrase,
            &content_tokens,
            member.as_ref().map(|member| member.member.as_str()),
        );
        let Some(found) = found else {
            if fresh {
                // Read only for this check, and it found nothing: later
                // phases are not expected to want the file.
                if let Some(files) = &request_files {
                    files.release(&path);
                }
            }
            continue;
        };
        let digest = digest.to_hex().to_string();
        for mut candidate in found {
            candidate.evidence.generated = generated;
            matches.push(ExactPassMatch {
                path: candidate.path,
                symbol_range: candidate.symbol_range,
                evidence: candidate.evidence,
                content_digest: digest.clone(),
            });
        }
    }
    Ok(matches)
}

/// The files able to hold the evidence `scope` keeps, as the legacy
/// `exact_evidence_bound`; `None` when every file may.
fn evidence_bound(
    text: &DataText,
    scope: &ExactEvidenceScope,
    norm_phrase: &str,
    content_tokens: &[String],
    holders: &mut LiteralHolders,
) -> io::Result<Option<BTreeSet<Vec<u8>>>> {
    let (keeps_window, declaration_mentions) = match scope {
        ExactEvidenceScope::All => return Ok(None),
        ExactEvidenceScope::Phrase => (false, None),
        ExactEvidenceScope::PhraseAndWindow => (true, None),
        ExactEvidenceScope::DefinitionsMentioning(tokens) => (true, Some(tokens.as_slice())),
    };
    let mut bound = BTreeSet::new();
    if !norm_phrase.is_empty() {
        let Some(paths) = containing_all(text, norm_phrase.split(' '), holders)? else {
            return Ok(None);
        };
        bound.extend(paths);
    }
    if keeps_window && content_tokens.len() >= 2 {
        let Some(paths) = containing_all(text, content_tokens.iter().map(String::as_str), holders)?
        else {
            return Ok(None);
        };
        bound.extend(paths);
    }
    if let Some(mentions) = declaration_mentions {
        if !content_tokens.is_empty() && !mentions.is_empty() {
            let named = containing_any(text, content_tokens.iter().map(String::as_str), holders)?;
            let mentioning = containing_any(text, mentions.iter().map(String::as_str), holders)?;
            let declaring = match (named, mentioning) {
                (Some(named), Some(mentioning)) => {
                    named.intersection(&mentioning).cloned().collect()
                }
                (Some(paths), None) | (None, Some(paths)) => paths,
                (None, None) => return Ok(None),
            };
            bound.extend(declaring);
        }
    }
    Ok(Some(bound))
}

fn containing_all<'a>(
    text: &DataText,
    words: impl Iterator<Item = &'a str>,
    holders: &mut LiteralHolders,
) -> io::Result<Option<BTreeSet<Vec<u8>>>> {
    let mut found: Option<BTreeSet<Vec<u8>>> = None;
    for word in words {
        let Some(paths) = possibly_containing(text, word, holders)? else {
            continue;
        };
        found = Some(match found {
            Some(found) => found.intersection(paths).cloned().collect(),
            None => paths.clone(),
        });
        if found.as_ref().is_some_and(BTreeSet::is_empty) {
            break;
        }
    }
    Ok(found)
}

fn containing_any<'a>(
    text: &DataText,
    words: impl Iterator<Item = &'a str>,
    holders: &mut LiteralHolders,
) -> io::Result<Option<BTreeSet<Vec<u8>>>> {
    let mut found = BTreeSet::new();
    for word in words {
        let Some(paths) = possibly_containing(text, word, holders)? else {
            return Ok(None);
        };
        found.extend(paths.iter().cloned());
    }
    Ok(Some(found))
}

/// The files that may hold `literal`; `None` when it has no trigram.
fn possibly_containing<'h>(
    text: &DataText,
    literal: &str,
    holders: &'h mut LiteralHolders,
) -> io::Result<Option<&'h BTreeSet<Vec<u8>>>> {
    if !holders.contains_key(literal) {
        let query = plan_pattern(&regex::escape(literal), false)
            .map_err(|error| io::Error::other(format!("lexical query plan failed: {error}")))?;
        let paths = match query {
            Query::All => None,
            query => Some(text.candidates_for(&query)?),
        };
        holders.insert(literal.to_string(), paths);
    }
    Ok(holders.get(literal).and_then(Option::as_ref))
}

/// The anchored lane's candidate files, as `discover_anchored_candidate_files`
/// finds them in the legacy index: the files that may hold any retained run,
/// case-insensitively, under `search_root`, and allowed by the test-file
/// setting.
pub(crate) fn anchored_candidate_files(
    text: &DataText,
    search_root: &Path,
    retained_runs: &[String],
    include_tests: bool,
) -> io::Result<Vec<PathBuf>> {
    if retained_runs.is_empty() {
        return Ok(Vec::new());
    }
    let mut candidates = BTreeSet::new();
    for run in retained_runs {
        let query = plan_pattern(&regex::escape(run), true)
            .map_err(|error| io::Error::other(format!("lexical query plan failed: {error}")))?;
        candidates.extend(text.candidates_for(&query)?);
    }
    let canonical_root =
        std::fs::canonicalize(search_root).unwrap_or_else(|_| search_root.to_path_buf());
    Ok(candidates
        .iter()
        .map(|relative| text.absolute(relative))
        .filter(|path| {
            std::fs::canonicalize(path)
                .unwrap_or_else(|_| path.clone())
                .starts_with(&canonical_root)
        })
        .filter(|path| {
            crate::commands::semantic_search::path_allowed_by_include_tests(
                path,
                search_root,
                include_tests,
            )
        })
        .collect())
}

/// As the legacy index's private helper of the same name: a path's
/// canonical form for scope checks, without a Windows verbatim prefix.
fn canonicalize_for_search_membership(path: &Path) -> PathBuf {
    crate::bounded_io::run(path, None, |path| {
        Ok(crate::path_identity::canonicalize_normalized(&path))
    })
    .unwrap_or_else(|_| crate::path_identity::normalize_path(path))
}

fn is_within_search_root(search_root: &Path, path: &Path) -> bool {
    crate::path_identity::normalize_path(path)
        .starts_with(crate::path_identity::normalize_path(search_root))
}

impl ExternalSearch {
    /// Answers `req` from the repository's checkout. Its files are compared
    /// with the checkout's state first (a repository outside the root has
    /// no watcher), so text answers read what changed from disk. Its meaning
    /// plane reads the vectors the repository's store holds for `ctx`'s
    /// model and never fills: semantic search is not admitted for a
    /// repository no session opened. The query is embedded with `ctx`'s
    /// model, lent to the engine for the call. Returns the reply and how many
    /// paths differ from the publication the answer read.
    pub(crate) fn search(
        &self,
        req: &crate::protocol::RawRequest,
        ctx: &crate::context::AppContext,
    ) -> (crate::protocol::Response, usize) {
        if !self
            .just_bound
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            if let Err(error) = self.checkout.checkout().rescan() {
                crate::slog_warn!(
                    "data layer could not compare {} with its files: {error}",
                    self.checkout.root().display()
                );
            }
        }
        let config = ctx.config();
        let mut lent = ctx.semantic_embedding_model().lock().take();
        if config.indexes.semantic {
            if lent.is_none() {
                lent =
                    crate::semantic_index::EmbeddingModel::from_config_for_query(&config.semantic)
                        .ok();
            }
            if let Some(fingerprint) = lent
                .as_mut()
                .and_then(|model| model.fingerprint(&config.semantic).ok())
            {
                if let Err(error) = self
                    .checkout
                    .read_meaning_only(&config.semantic, &fingerprint)
                {
                    crate::slog_warn!(
                        "data layer cannot read the meaning plane of {}: {error}",
                        self.checkout.root().display()
                    );
                }
            }
        }
        *self.engine.semantic_embedding_model().lock() = lent;
        let response = crate::commands::semantic_search::with_absolute_result_paths(|| {
            crate::commands::semantic_search::handle_parent_child_search(req, &self.engine)
        });
        let returned = self.engine.semantic_embedding_model().lock().take();
        let mut model = ctx.semantic_embedding_model().lock();
        if model.is_none() {
            *model = returned;
        }
        let drift = self
            .checkout
            .view()
            .map_or(0, |view| view.changed_paths().len());
        (response, drift)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use aft_store::ids::{Digest128, FamilyId};

    use crate::commands::semantic_search::lexical_lane::{
        CanonicalLexicalLane, LEXICAL_ENUMERATION_LIMIT,
    };
    use crate::data_layer::checkout::{Checkout, DataLayer};
    use crate::search_index::{SearchIndex, DEFAULT_MAX_FILE_SIZE};

    /// A checkout's legacy index and its data-layer view, built from the
    /// same files.
    struct Pair {
        root: PathBuf,
        data: DataText,
        _checkout: Checkout,
        _store: tempfile::TempDir,
    }

    fn pair(root: &Path) -> Pair {
        let root = std::fs::canonicalize(root).unwrap();
        let store = tempfile::tempdir().unwrap();
        let layer = Arc::new(DataLayer::new(64 << 20, 4));
        let family = FamilyId::from_digest(Digest128::of(b"search-parity"));
        let checkout = Checkout::bind(layer, store.path(), family, &root).unwrap();
        checkout.build().unwrap();
        let view = checkout.view().unwrap();
        assert_eq!(view.status().text, PlaneStatus::Ready);
        Pair {
            data: DataText::from_view(view, DEFAULT_MAX_FILE_SIZE),
            root,
            _checkout: checkout,
            _store: store,
        }
    }

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn trigrams(query: &str) -> Vec<u32> {
        let tokens = crate::query_shape::extract_content_tokens(query);
        let refs = tokens.iter().map(String::as_str).collect::<Vec<_>>();
        SearchIndex::query_trigrams_from_tokens(&refs)
    }

    fn include_filter(root: &Path, include_tests: bool) -> impl Fn(&Path) -> bool + '_ {
        move |path: &Path| {
            crate::commands::semantic_search::path_allowed_by_include_tests(
                path,
                root,
                include_tests,
            )
        }
    }

    /// The lexical lane's canonical order, one candidate and its score's
    /// bits per line, after the pool size.
    fn lane(index: TextIndex<'_>, trigrams: &[u32], include_tests: bool, root: &Path) -> String {
        let filter = include_filter(root, include_tests);
        let lane = CanonicalLexicalLane::from_text_index(
            index,
            trigrams,
            Some(&filter),
            LEXICAL_ENUMERATION_LIMIT,
        )
        .unwrap();
        render_lane(&lane)
    }

    fn render_lane(lane: &CanonicalLexicalLane) -> String {
        let mut lines = vec![format!("pool {}", lane.selected_pool_size())];
        lines.extend(lane.canonical_order().iter().map(|candidate| {
            format!(
                "{} {:08x}",
                candidate.result.path.display(),
                candidate.raw_score.to_bits()
            )
        }));
        lines.join("\n")
    }

    /// Two files with one content hold its trigrams twice. The lane counts
    /// rarity by the checkout's paths, as the legacy index counts files, so
    /// the duplicated trigram is no rarer than the others and the pool is
    /// the legacy one; counting distinct contents would pick it first.
    #[test]
    fn a_content_shared_by_two_files_counts_twice_in_the_lanes_rarity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "x1.txt", "xxx\n");
        write(root, "x2.txt", "xxx\n");
        for (name, text) in [("a", "aaa"), ("b", "bbb"), ("c", "ccc")] {
            write(root, &format!("{name}1.txt"), &format!("{text} one\n"));
            write(root, &format!("{name}2.txt"), &format!("{text} two\n"));
        }
        let pair = pair(root);
        let query = trigrams("xxx aaa bbb ccc");
        let unique = unique_trigrams(&query);
        let postings = pair.data.postings(&unique).unwrap();
        let pack = |text: &[u8]| aft_store::text::pack(text[0], text[1], text[2]);
        assert_eq!(
            rarest(&unique, &postings, paths_holding).unwrap(),
            vec![pack(b"aaa"), pack(b"bbb"), pack(b"ccc")]
        );
        assert_eq!(
            rarest(&unique, &postings, |trigram, _| pair
                .data
                .frequency(trigram))
            .unwrap(),
            vec![pack(b"xxx"), pack(b"aaa"), pack(b"bbb")]
        );
        assert!(
            !lane(TextIndex::Data(&pair.data), &query, true, &pair.root).contains("x1.txt"),
            "the shared content's files are outside the three rarest"
        );
    }
}

impl DataSemantic {
    /// This root's meaning search, with the data layer on and semantic
    /// search switched on and admitted for the root: binds the checkout if
    /// nothing has, and starts a fill when files wait for vectors. `None`
    /// otherwise (the root has no checkout in the data layer, or semantic
    /// search is off), and the caller keeps reading today's index.
    pub(crate) fn current(
        ctx: &(impl crate::data_layer::host::IndexHost + ?Sized),
    ) -> Option<Self> {
        if !ctx.config().indexes.semantic {
            return None;
        }
        // The context's checkout slot is held while a checkout binds (a bind
        // scans the folder); the meaning plane's snapshot can wait for a
        // generation reload or a member-cache build.
        let checkout = waits::timed(SearchWait::CheckoutSlot, || ctx.data_checkout())?;
        let _wait = waits::time(SearchWait::MeaningView);
        Self::from_state(checkout.meaning(|| ctx.data_meaning_config()))
    }
}

/// Where aft_search's semantic lane reads chunk vectors from.
#[derive(Clone, Copy)]
pub(crate) enum MeaningIndex<'a> {
    /// Today's semantic index.
    Snapshot(&'a SemanticIndex),
    /// The data layer's meaning plane.
    Data(&'a DataMeaning),
}

impl MeaningIndex<'_> {
    /// The `top_k` chunks closest to `query` among files whose absolute
    /// path `include` accepts, best first: `SemanticIndex::search_filtered`.
    ///
    /// # Errors
    ///
    /// When the data layer's store cannot be read; today's index never
    /// fails.
    pub(crate) fn search_filtered(
        &self,
        query: &[f32],
        top_k: usize,
        include: &dyn Fn(&Path) -> bool,
    ) -> io::Result<Vec<SemanticResult>> {
        match self {
            Self::Snapshot(index) => Ok(waits::timed(SearchWait::VectorSearch, || {
                index.search_filtered(query, top_k, include)
            })),
            Self::Data(meaning) => waits::timed(SearchWait::VectorSearch, || {
                meaning.snapshot.search(query, top_k, include)
            })
            .map_err(|error| io::Error::other(error.to_string())),
        }
    }

    /// The vector length a query must have; `None` while the index holds no
    /// vector, when any length is let through.
    pub(crate) fn dimension(&self) -> Option<usize> {
        match self {
            Self::Snapshot(index) => (index.len() > 0).then(|| index.dimension()),
            Self::Data(meaning) => meaning.dimension(),
        }
    }
}
