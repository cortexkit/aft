//! aft_search's lexical and exact lanes, read from the data layer.
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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use aft_store::text::{plan_pattern, PathText, Query, Scope};

use crate::commands::semantic_search::exact_lane::{self, ExactEvidenceScope};
use crate::data_layer::checkout::{CheckoutView, DataError, PlaneStatus};
use crate::search_index::{
    read_search_corpus_file, ExactPassMatch, SearchCorpusEligibility, SearchIndexSnapshot,
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
        let view = ctx.data_view()?;
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

    /// The paths whose indexed content holds each of `trigrams`, read once
    /// per distinct trigram.
    fn postings(&self, trigrams: &[u32]) -> io::Result<HashMap<u32, Vec<Vec<u8>>>> {
        let mut postings = HashMap::with_capacity(trigrams.len());
        for &trigram in trigrams {
            if let std::collections::hash_map::Entry::Vacant(entry) = postings.entry(trigram) {
                entry.insert(self.view.paths_with_trigram(trigram).map_err(store_error)?);
            }
        }
        Ok(postings)
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
        relative: &[u8],
        query_trigrams: &[u32],
        postings: &HashMap<u32, Vec<Vec<u8>>>,
    ) -> io::Result<f32> {
        if query_trigrams.is_empty() {
            return Ok(0.0);
        }
        let hits = query_trigrams
            .iter()
            .filter(|trigram| {
                postings.get(trigram).is_some_and(|paths| {
                    paths
                        .binary_search_by(|path| path.as_slice().cmp(relative))
                        .is_ok()
                })
            })
            .count() as u32;
        if hits == 0 {
            return Ok(0.0);
        }
        let distinct = match self.view.path_text(relative).map_err(store_error)? {
            Some(PathText::Indexed { distinct }) => distinct,
            _ => 1,
        };
        Ok((hits as f32) / (1.0 + (distinct.max(1) as f32).ln()))
    }
}

fn store_error(error: DataError) -> io::Error {
    io::Error::other(error.to_string())
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
            Self::Data(text) => {
                selected_pool(text, query_trigrams, candidate_filter, paths_holding)
            }
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
        let mut candidates = BTreeSet::new();
        for (trigram, _) in non_zero.iter().take(initial) {
            candidates.extend(postings[trigram].iter());
        }
        for (trigram, _) in non_zero.iter().skip(initial) {
            if candidates.len() >= depth {
                break;
            }
            candidates.extend(postings[trigram].iter());
        }
        let mut ranked = Vec::new();
        let admitted = candidates
            .into_iter()
            .map(|relative| (relative, text.absolute(relative)))
            .filter(|(_, path)| candidate_filter.is_none_or(|include| include(path)))
            .take(depth);
        for (relative, path) in admitted {
            let score = text.score(relative, query_trigrams, &postings)?;
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
    rarity: impl Fn(u32, &[Vec<u8>]) -> io::Result<u64>,
) -> io::Result<Vec<(PathBuf, f32)>> {
    let unique = unique_trigrams(query_trigrams);
    let postings = text.postings(&unique)?;
    let picked = rarest(&unique, &postings, rarity)?;
    let selected = picked
        .iter()
        .flat_map(|trigram| postings[trigram].iter())
        .collect::<BTreeSet<_>>();
    let mut ranked = Vec::new();
    for relative in selected {
        let path = text.absolute(relative);
        if candidate_filter.is_some_and(|include| !include(&path)) {
            continue;
        }
        let score = text.score(relative, &unique, &postings)?;
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
/// three trigrams on about one real query in sixty and moved their lexical
/// ranking; the legacy index counts files, as this does.
fn paths_holding(_trigram: u32, paths: &[Vec<u8>]) -> io::Result<u64> {
    Ok(paths.len() as u64)
}

/// The (at most) three rarest of `unique` that some path holds, by
/// `rarity` of each trigram and its postings, rarest first, ties to the
/// smaller trigram.
fn rarest(
    unique: &[u32],
    postings: &HashMap<u32, Vec<Vec<u8>>>,
    rarity: impl Fn(u32, &[Vec<u8>]) -> io::Result<u64>,
) -> io::Result<Vec<u32>> {
    let mut held = Vec::new();
    for &trigram in unique {
        let paths = &postings[&trigram];
        if paths.is_empty() {
            continue;
        }
        held.push((rarity(trigram, paths)?, trigram));
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

    let search_root_canon = canonicalize_for_search_membership(search_root);
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
        let SearchCorpusEligibility::Eligible(file) =
            read_search_corpus_file(&path, text.max_file_size)
        else {
            continue;
        };
        let content = String::from_utf8_lossy(&file.bytes);
        let digest = blake3::hash(&file.bytes);
        let Some(found) = exact_lane::verify_cached_exact_matches_in_text_with_member(
            &path,
            &content,
            digest,
            &norm_phrase,
            &content_tokens,
            member.as_ref().map(|member| member.member.as_str()),
        ) else {
            continue;
        };
        let digest = digest.to_hex().to_string();
        for mut candidate in found {
            candidate.evidence.generated = file.generated;
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
        Ok(crate::inspect::job::canonicalize_normalized(&path))
    })
    .unwrap_or_else(|_| crate::inspect::job::normalize_path(path))
}

fn is_within_search_root(search_root: &Path, path: &Path) -> bool {
    crate::inspect::job::normalize_path(path)
        .starts_with(crate::inspect::job::normalize_path(search_root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use aft_store::ids::{Digest128, FamilyId};

    use crate::commands::semantic_search::anchored_lane::{
        discover_anchored_candidate_files, split_query, AnchoredLane,
    };
    use crate::commands::semantic_search::lexical_lane::{
        CanonicalLexicalLane, LEXICAL_ENUMERATION_LIMIT,
    };
    use crate::data_layer::checkout::{Checkout, DataLayer};
    use crate::search_index::{SearchIndex, DEFAULT_MAX_FILE_SIZE};

    /// A checkout's legacy index and its data-layer view, built from the
    /// same files.
    struct Pair {
        root: PathBuf,
        legacy: SearchIndex,
        data: DataText,
        _checkout: Checkout,
        _store: tempfile::TempDir,
    }

    fn pair(root: &Path) -> Pair {
        let root = std::fs::canonicalize(root).unwrap();
        let legacy = SearchIndex::build(&root);
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
            legacy,
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

    /// Which exact evidence a caller of the pass keeps, as `run_engine_ranking`
    /// filters each scope's results.
    #[derive(Clone, Copy, Debug)]
    enum Caller {
        /// Identifier queries and the not-found check keep everything.
        Identifier,
        /// Queries without identifier words drop declarations.
        PhraseAndWindow,
        /// Identifier-fact passes keep phrase evidence only.
        Phrase,
        /// Natural-language queries naming identifiers keep declarations
        /// only of a named identifier.
        DefinitionsMentioning,
    }

    impl Caller {
        const ALL: [Self; 4] = [
            Self::Identifier,
            Self::PhraseAndWindow,
            Self::Phrase,
            Self::DefinitionsMentioning,
        ];

        fn name(self) -> &'static str {
            match self {
                Self::Identifier => "exact_identifier_scope_all",
                Self::PhraseAndWindow => "exact_phrase_and_window",
                Self::Phrase => "exact_phrase",
                Self::DefinitionsMentioning => "exact_definitions_mentioning",
            }
        }

        /// Whether this caller keeps declaration evidence, the one place the
        /// two candidate sets can change the rows (see
        /// [`Self::extra_declarations_name`]).
        fn keeps_declarations(self) -> bool {
            matches!(self, Self::Identifier | Self::DefinitionsMentioning)
        }

        /// The reviewed difference of the declaration-keeping callers: extra
        /// declaration rows from the data layer, never a missing row. The
        /// verifier gives declaration evidence for any declaration a query
        /// word names, whether or not the file holds the phrase, so the rows
        /// depend on which files the candidate query admits. Today's index
        /// ANDs every phrase trigram with its next-character masks; the
        /// view's candidates skip long conjuncts and hash the masks
        /// differently, so they admit a few files today's pruning never
        /// reached. On aft's checkout over the 6,700 real queries:
        ///
        /// - `16 GiB`, scope All: `const GIB` in `crates/aft/src/views/eviction.rs`
        ///   and one in `crates/aft/src/views/base_builder_tests.rs` (the
        ///   files lack the trigram `16 `);
        /// - `/pi/model`, scope All: six `model` declarations in
        ///   `crates/aft/src/commands/semantic_search/rerank/onnx.rs` (it
        ///   lacks `/pi`);
        /// - `fn now_ms`, scope All and DefinitionsMentioning: `now_ms`
        ///   fields and parameters in FILES_NOW_MS (they hold every trigram of
        ///   the phrase; only today's masks left them out).
        fn extra_declarations_name(self) -> &'static str {
            match self {
                Self::Identifier => "allowed_extra_declarations_identifier_scope_all",
                _ => "allowed_extra_declarations_definitions_mentioning",
            }
        }

        fn scope(self, query: &str) -> ExactEvidenceScope {
            match self {
                Self::Identifier => ExactEvidenceScope::All,
                Self::PhraseAndWindow => ExactEvidenceScope::PhraseAndWindow,
                Self::Phrase => ExactEvidenceScope::Phrase,
                Self::DefinitionsMentioning => ExactEvidenceScope::DefinitionsMentioning(
                    crate::commands::semantic_search::definition_identifier_tokens(query)
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                ),
            }
        }

        fn keeps(
            self,
            candidate: &crate::commands::semantic_search::comparator::CandidateResult,
            query: &str,
        ) -> bool {
            use crate::commands::semantic_search::evidence_descriptor::EvidenceKind;
            match self {
                Self::Identifier => true,
                Self::PhraseAndWindow => candidate.evidence.kind != EvidenceKind::Definition,
                Self::Phrase => candidate.evidence.kind == EvidenceKind::E1,
                Self::DefinitionsMentioning => {
                    candidate.evidence.kind != EvidenceKind::Definition
                        || crate::commands::semantic_search::definition_matches_identifier_token(
                            candidate, query,
                        )
                }
            }
        }
    }

    /// An exact pass as its caller keeps it, one line per result in the
    /// exact tier's canonical order, then each file's content digest; and
    /// the raw pass, before the caller's filter.
    fn exact(
        index: TextIndex<'_>,
        query: &str,
        root: &Path,
        filter: Option<&dyn Fn(&Path) -> bool>,
        caller: Caller,
    ) -> (String, String) {
        let scope = caller.scope(query);
        let matches = match index.whole_corpus_exact_pass_scoped(query, root, filter, &scope) {
            Ok(matches) => matches,
            Err(error) => return (format!("error: {error}"), String::new()),
        };
        let mut results = matches
            .iter()
            .map(|found| {
                (
                    crate::commands::semantic_search::comparator::CandidateResult::new_exact(
                        found.path.clone(),
                        found.symbol_range,
                        found.evidence.clone(),
                    ),
                    found.content_digest.clone(),
                )
            })
            .collect::<Vec<_>>();
        results.sort_by(|a, b| {
            crate::commands::semantic_search::comparator::score_free_r3_cmp(&a.0, &b.0)
        });
        let raw = results
            .iter()
            .map(|(result, _)| format!("{result:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        let kept = results
            .iter()
            .filter(|(result, _)| caller.keeps(result, query))
            .map(|(result, digest)| format!("{result:?} {digest}"))
            .collect::<Vec<_>>()
            .join("\n");
        (kept, raw)
    }

    /// The files that may hold a phrase: those that do hold it (what the
    /// missing-literal check decides on), the whole set, and its order.
    fn holders(index: TextIndex<'_>, phrase: &str, root: &Path) -> [String; 3] {
        match index.files_possibly_holding_phrase(phrase, root) {
            Ok(Some(paths)) => {
                let normalized = exact_lane::normalize_exact_phrase(phrase);
                let holding = paths
                    .iter()
                    .filter(|path| {
                        crate::search_index::read_searchable_text(path).is_some_and(|text| {
                            exact_lane::normalize_exact_phrase(&text).contains(&normalized)
                        })
                    })
                    .map(|path| path.display().to_string())
                    .collect::<BTreeSet<_>>();
                let order = paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>();
                let set = order.iter().cloned().collect::<BTreeSet<_>>();
                [
                    holding.into_iter().collect::<Vec<_>>().join("\n"),
                    set.into_iter().collect::<Vec<_>>().join("\n"),
                    order.join("\n"),
                ]
            }
            other => {
                let other = format!("{other:?}");
                [other.clone(), other.clone(), other]
            }
        }
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

    /// [`lane`] from the data layer with rarity counted by distinct
    /// contents (`trigram_frequency`), which the lane does not use; the
    /// parity test reports what it would change.
    fn lane_by_content_rarity(text: &DataText, trigrams: &[u32], root: &Path) -> String {
        let filter = include_filter(root, false);
        let pool = selected_pool(text, trigrams, Some(&filter), |trigram, _| {
            text.frequency(trigram)
        })
        .unwrap();
        let lane = CanonicalLexicalLane::from_scored_candidates(
            pool.into_iter()
                .map(|(path, score)| {
                    crate::commands::semantic_search::lexical_lane::LexicalCandidate::file(
                        path, score,
                    )
                })
                .collect(),
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

    #[test]
    fn the_view_answers_each_lane_question_as_the_legacy_index_does() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "src/upload.rs",
            "pub fn retry_upload() {}\n// retries the upload\n",
        );
        write(
            root,
            "src/client.rs",
            "fn send() { retry_upload(); }\nstruct Client;\n",
        );
        write(
            root,
            "docs/notes.md",
            "The upload is retried with a backoff.\n",
        );
        write(
            root,
            "tests/upload_test.rs",
            "fn upload_test() { retry_upload(); }\n",
        );
        write(root, "data.bin", "\0\0binary retry_upload\0");
        let pair = pair(root);
        let legacy = pair.legacy.snapshot();
        let legacy = TextIndex::Snapshot(&legacy);
        let data = TextIndex::Data(&pair.data);
        for query in [
            "retry upload",
            "retry_upload",
            "Client send",
            "backoff",
            "zz",
        ] {
            let trigrams = trigrams(query);
            for include_tests in [false, true] {
                assert_eq!(
                    lane(legacy, &trigrams, include_tests, &pair.root),
                    lane(data, &trigrams, include_tests, &pair.root),
                    "{query}"
                );
            }
            assert_eq!(
                legacy
                    .lexical_rank_at_depth(&trigrams, None, usize::MAX)
                    .unwrap(),
                data.lexical_rank_at_depth(&trigrams, None, usize::MAX)
                    .unwrap(),
                "{query}"
            );
            for caller in Caller::ALL {
                assert_eq!(
                    exact(legacy, query, &pair.root, None, caller),
                    exact(data, query, &pair.root, None, caller),
                    "{query} {caller:?}"
                );
            }
            // The legacy index lists them in the order it first indexed the
            // files, the view in byte order.
            let legacy_holders = holders(legacy, query, &pair.root);
            let data_holders = holders(data, query, &pair.root);
            assert_eq!(legacy_holders[..2], data_holders[..2], "{query}");
        }
        for path in ["src/upload.rs", "data.bin", "missing.rs"] {
            let path = pair.root.join(path);
            assert_eq!(
                legacy.audit_index_state(&path),
                data.audit_index_state(&path),
                "{}",
                path.display()
            );
        }
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
        let legacy = pair.legacy.snapshot();
        assert_eq!(
            lane(TextIndex::Snapshot(&legacy), &query, true, &pair.root),
            lane(TextIndex::Data(&pair.data), &query, true, &pair.root),
        );
        assert!(
            !lane(TextIndex::Data(&pair.data), &query, true, &pair.root).contains("x1.txt"),
            "the shared content's files are outside the three rarest"
        );
    }

    /// What one comparison found over a query list.
    #[derive(Default)]
    struct Tally {
        queries: usize,
        differing: usize,
        legacy: Duration,
        data: Duration,
        examples: Vec<serde_json::Value>,
    }

    /// One query's answer from either source, with what it cost.
    type Answer = (String, Duration);

    impl Tally {
        /// Counts one query; keeps its lines on both sides for the first
        /// differences, every line when `every_line` (for the reviewed
        /// classes, whose report lists what they add).
        fn record(&mut self, query: &str, legacy: Answer, data: Answer, every_line: bool) {
            self.queries += 1;
            self.legacy += legacy.1;
            self.data += data.1;
            if legacy.0 == data.0 {
                return;
            }
            self.differing += 1;
            if self.examples.len() >= 25 {
                return;
            }
            // Lines only one side gives, each once; and the first line at
            // which they part, for an order difference.
            let left = legacy.0.lines().collect::<Vec<_>>();
            let right = data.0.lines().collect::<Vec<_>>();
            let left_set = left.iter().copied().collect::<BTreeSet<_>>();
            let right_set = right.iter().copied().collect::<BTreeSet<_>>();
            let parted = left.iter().zip(&right).position(|(a, b)| a != b);
            let keep = if every_line { usize::MAX } else { 12 };
            let clip = |lines: Vec<&str>| {
                lines
                    .into_iter()
                    .take(keep)
                    .map(|line| line.chars().take(400).collect::<String>())
                    .collect::<Vec<_>>()
            };
            self.examples.push(serde_json::json!({
                "query": query,
                "legacy_lines": left.len(),
                "data_lines": right.len(),
                "only_legacy": clip(left_set.difference(&right_set).copied().collect()),
                "only_data": clip(right_set.difference(&left_set).copied().collect()),
                "first_parting_line": parted,
                "legacy_at_parting": parted.map(|index| left[index].chars().take(400).collect::<String>()),
                "data_at_parting": parted.map(|index| right[index].chars().take(400).collect::<String>()),
            }));
        }

        fn summary(&self) -> serde_json::Value {
            let per_call =
                |total: Duration| total.as_secs_f64() * 1000.0 / self.queries.max(1) as f64;
            serde_json::json!({
                "queries": self.queries,
                "differing": self.differing,
                "legacy_ms_per_call": per_call(self.legacy),
                "data_ms_per_call": per_call(self.data),
                "examples": self.examples,
            })
        }
    }

    fn timed<T>(work: impl FnOnce() -> T) -> (T, Duration) {
        let started = Instant::now();
        let value = work();
        (value, started.elapsed())
    }

    /// Every comparison for one query: its name, and each source's answer.
    fn compare_query(
        pair: &Pair,
        legacy: TextIndex<'_>,
        data: TextIndex<'_>,
        query: &str,
        exact_too: bool,
    ) -> Vec<(&'static str, Answer, Answer)> {
        let root = &pair.root;
        let zero = Duration::ZERO;
        let mut found = Vec::new();
        let trigrams = trigrams(query);
        found.push((
            "lexical_lane",
            timed(|| lane(legacy, &trigrams, false, root)),
            timed(|| lane(data, &trigrams, false, root)),
        ));
        found.push((
            "lexical_lane_include_tests",
            timed(|| lane(legacy, &trigrams, true, root)),
            timed(|| lane(data, &trigrams, true, root)),
        ));
        found.push((
            "measure_lane_if_rarity_counted_contents",
            (lane(legacy, &trigrams, false, root), zero),
            timed(|| lane_by_content_rarity(&pair.data, &trigrams, root)),
        ));

        // Which three trigrams each rarity picks.
        let unique = unique_trigrams(&trigrams);
        let postings = pair.data.postings(&unique).unwrap();
        let by_files = rarest(&unique, &postings, paths_holding).unwrap();
        let by_frequency = rarest(&unique, &postings, |trigram, _| {
            pair.data.frequency(trigram)
        })
        .unwrap();
        let render = |picks: &[u32]| {
            picks
                .iter()
                .map(|trigram| {
                    let bytes = [(trigram >> 16) as u8, (trigram >> 8) as u8, *trigram as u8];
                    format!(
                        "{:?} files={} frequency={}",
                        String::from_utf8_lossy(&bytes),
                        postings[trigram].len(),
                        pair.data.frequency(*trigram).unwrap()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        found.push((
            "measure_rarest_three_paths_vs_contents",
            (render(&by_files), zero),
            (render(&by_frequency), zero),
        ));
        let as_set = |picks: &[u32]| {
            let mut picks = picks.to_vec();
            picks.sort_unstable();
            format!("{picks:06x?}")
        };
        found.push((
            "measure_rarest_three_paths_vs_contents_as_a_set",
            (as_set(&by_files), zero),
            (as_set(&by_frequency), zero),
        ));
        let rank_lines = |ranked: io::Result<Vec<(PathBuf, f32)>>| match ranked {
            Ok(ranked) => ranked
                .iter()
                .map(|(path, score)| format!("{} {:08x}", path.display(), score.to_bits()))
                .collect::<Vec<_>>()
                .join("\n"),
            Err(error) => format!("error: {error}"),
        };
        let (legacy_ranked, legacy_time) =
            timed(|| rank_lines(legacy.lexical_rank_at_depth(&trigrams, None, usize::MAX)));
        let (data_ranked, data_time) =
            timed(|| rank_lines(data.lexical_rank_at_depth(&trigrams, None, usize::MAX)));
        let sorted = |lines: &str| {
            let mut lines = lines.lines().collect::<Vec<_>>();
            lines.sort_unstable();
            lines.join("\n")
        };
        found.push((
            "unpooled_rank_scores",
            (sorted(&legacy_ranked), legacy_time),
            (sorted(&data_ranked), data_time),
        ));
        // The scores in rank order: equal sequences over equal sets mean the
        // orders differ only among files with the same score.
        let scores = |lines: &str| {
            lines
                .lines()
                .map(|line| line.rsplit(' ').next().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n")
        };
        found.push((
            "unpooled_rank_score_sequence",
            (scores(&legacy_ranked), zero),
            (scores(&data_ranked), zero),
        ));
        // Reviewed and allowed: the audit's ties are in path byte order here
        // and in the legacy index's file-id order there.
        found.push((
            "allowed_audit_ties_in_path_order",
            (legacy_ranked, zero),
            (data_ranked, zero),
        ));
        if !exact_too {
            return found;
        }

        let filter = include_filter(root, false);
        for caller in Caller::ALL {
            let (legacy_pass, legacy_time) =
                timed(|| exact(legacy, query, root, Some(&filter), caller));
            let (data_pass, data_time) = timed(|| exact(data, query, root, Some(&filter), caller));
            if caller.keeps_declarations() {
                // Strict: every row today returns, in today's order, and no
                // extra row other than a declaration. The extra declarations
                // themselves are the reviewed class compared below.
                let today = legacy_pass.0.lines().collect::<HashSet<_>>();
                let checked = data_pass
                    .0
                    .lines()
                    .filter(|line| today.contains(line) || !line.contains("kind: Definition,"))
                    .collect::<Vec<_>>()
                    .join("\n");
                found.push((
                    caller.name(),
                    (legacy_pass.0.clone(), legacy_time),
                    (checked, data_time),
                ));
                found.push((
                    caller.extra_declarations_name(),
                    (legacy_pass.0, zero),
                    (data_pass.0, zero),
                ));
            } else {
                found.push((
                    caller.name(),
                    (legacy_pass.0, legacy_time),
                    (data_pass.0, data_time),
                ));
            }
            if !matches!(caller, Caller::Identifier) {
                found.push((
                    match caller {
                        Caller::PhraseAndWindow => "measure_exact_phrase_and_window_unfiltered",
                        Caller::Phrase => "measure_exact_phrase_unfiltered",
                        _ => "measure_exact_definitions_mentioning_unfiltered",
                    },
                    (legacy_pass.1, zero),
                    (data_pass.1, zero),
                ));
            }
        }
        let phrase = exact_lane::exact_phrase(query);
        let (legacy_holders, legacy_time) = timed(|| holders(legacy, phrase, root));
        let (data_holders, data_time) = timed(|| holders(data, phrase, root));
        let [legacy_holding, legacy_set, legacy_order] = legacy_holders;
        let [data_holding, data_set, data_order] = data_holders;
        found.push((
            "phrase_holders_that_hold_it",
            (legacy_holding, legacy_time),
            (data_holding, data_time),
        ));
        found.push((
            "measure_phrase_possible_holders",
            (legacy_set, zero),
            (data_set, zero),
        ));
        found.push((
            "measure_phrase_possible_holders_order",
            (legacy_order, zero),
            (data_order, zero),
        ));

        let runs = split_query(query).retained_runs;
        let count = |files: io::Result<Vec<PathBuf>>| match files {
            Ok(files) => format!("{} candidate files", files.len()),
            Err(error) => format!("error: {error}"),
        };
        found.push((
            "measure_anchored_candidate_count",
            timed(|| {
                count(discover_anchored_candidate_files(
                    &pair.legacy,
                    root,
                    &runs,
                    false,
                ))
            }),
            timed(|| count(anchored_candidate_files(&pair.data, root, &runs, false))),
        ));
        type Anchored = (
            Vec<crate::commands::semantic_search::comparator::CandidateResult>,
            usize,
            usize,
        );
        let anchored = |executed: &io::Result<Anchored>, with_examined: bool| match executed {
            Ok((results, examined, excluded)) => {
                let mut lines = results
                    .iter()
                    .map(|result| format!("{result:?}"))
                    .collect::<Vec<_>>();
                lines.push(format!("excluded {excluded}"));
                if with_examined {
                    lines.push(format!("examined {examined}"));
                }
                lines.join("\n")
            }
            Err(error) => format!("error: {error}"),
        };
        let (legacy_anchored, legacy_time) = timed(|| {
            AnchoredLane::new().execute_with_admission_report(&pair.legacy, root, query, false)
        });
        let (data_anchored, data_time) =
            timed(|| AnchoredLane::new().execute_on_data_view(&pair.data, root, query, false));
        found.push((
            "anchored_results_and_exclusions",
            (anchored(&legacy_anchored, false), legacy_time),
            (anchored(&data_anchored, false), data_time),
        ));
        found.push((
            "allowed_anchored_examined_count",
            (anchored(&legacy_anchored, true), zero),
            (anchored(&data_anchored, true), zero),
        ));
        found
    }

    /// Runs every lane question for each query of a real query list against
    /// the legacy index and the data layer's view of one checkout, and
    /// writes what differs and what each costs. The gate for wiring
    /// aft_search onto the data layer runs it on aft's own checkout:
    ///
    /// `AFT_SEARCH_PARITY_ROOT=<checkout> AFT_SEARCH_PARITY_QUERIES=<one
    /// query per line> AFT_SEARCH_PARITY_OUT=<report.json> cargo test -p
    /// agent-file-tools --lib -- --ignored lanes_match_the_legacy_index`
    ///
    /// `AFT_SEARCH_PARITY_EXACT_LIMIT` caps how many queries also run the
    /// exact, missing-literal and anchored comparisons (they read files);
    /// `AFT_SEARCH_PARITY_THREADS` spreads the queries over threads.
    #[test]
    #[ignore = "reads the checkout and query list named by AFT_SEARCH_PARITY_*"]
    fn lanes_match_the_legacy_index_on_real_queries() {
        let variable = |name: &str| std::env::var(name).ok();
        let root = PathBuf::from(variable("AFT_SEARCH_PARITY_ROOT").unwrap());
        let queries =
            std::fs::read_to_string(variable("AFT_SEARCH_PARITY_QUERIES").unwrap()).unwrap();
        let out = PathBuf::from(variable("AFT_SEARCH_PARITY_OUT").unwrap());
        let exact_limit = variable("AFT_SEARCH_PARITY_EXACT_LIMIT")
            .and_then(|limit| limit.parse::<usize>().ok())
            .unwrap_or(usize::MAX);
        let threads = variable("AFT_SEARCH_PARITY_THREADS")
            .and_then(|threads| threads.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        let (pair, build) = timed(|| pair(&root));
        let snapshot = pair.legacy.snapshot();
        let view_paths = pair.data.view.state().paths().len();
        let legacy_files = snapshot
            .file_entries()
            .iter()
            .filter(|file| !file.path.as_os_str().is_empty())
            .count();
        eprintln!(
            "built both indexes in {:.0} ms; {view_paths} paths in the view, {legacy_files} files in the legacy index",
            build.as_secs_f64() * 1000.0,
        );

        let mut seen = HashSet::new();
        let queries = queries
            .lines()
            .map(str::trim)
            .filter(|query| !query.is_empty() && seen.insert(query.to_string()))
            .collect::<Vec<_>>();
        // Each worker records its answers as it goes; keeping every answer
        // until the end would hold gigabytes of rendered lists.
        let next = std::sync::atomic::AtomicUsize::new(0);
        let tallies =
            std::sync::Mutex::new(std::collections::BTreeMap::<&'static str, Tally>::new());
        let started = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    let legacy = TextIndex::Snapshot(&snapshot);
                    let data = TextIndex::Data(&pair.data);
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(query) = queries.get(index) else {
                            break;
                        };
                        let found = compare_query(&pair, legacy, data, query, index < exact_limit);
                        let mut tallies = tallies.lock().unwrap();
                        for (name, legacy, data) in found {
                            let every_line = name.starts_with("allowed_extra_declarations");
                            tallies
                                .entry(name)
                                .or_default()
                                .record(query, legacy, data, every_line);
                        }
                        let finished = tallies.get("lexical_lane").map_or(0, |tally| tally.queries);
                        if finished % 500 == 0 {
                            eprintln!(
                                "{finished} queries in {:.0} s",
                                started.elapsed().as_secs_f64()
                            );
                        }
                    }
                });
            }
        });
        let tallies = tallies.into_inner().unwrap();
        let mut report = serde_json::json!({
            "root": root,
            "distinct_queries": queries.len(),
            "threads": threads,
            "exact_limit": exact_limit.min(queries.len()),
            "build_ms_legacy_and_data": build.as_secs_f64() * 1000.0,
            "view_paths": view_paths,
            "legacy_files": legacy_files,
        });
        for (name, tally) in &tallies {
            report[*name] = tally.summary();
            println!(
                "{name}: {} of {} differ; legacy {:.3} ms, data {:.3} ms per call",
                tally.differing,
                tally.queries,
                tally.legacy.as_secs_f64() * 1000.0 / tally.queries.max(1) as f64,
                tally.data.as_secs_f64() * 1000.0 / tally.queries.max(1) as f64,
            );
        }
        std::fs::write(&out, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        // `measure_` comparisons report what another choice would change and
        // `allowed_` ones are reviewed differences whose preconditions the
        // strict comparisons check; every other one must agree on every query.
        let failing = tallies
            .iter()
            .filter(|(name, tally)| {
                !name.starts_with("measure_")
                    && !name.starts_with("allowed_")
                    && tally.differing > 0
            })
            .map(|(name, tally)| format!("{name}: {} of {}", tally.differing, tally.queries))
            .collect::<Vec<_>>();
        assert!(failing.is_empty(), "lane answers differ: {failing:?}");
    }
}
