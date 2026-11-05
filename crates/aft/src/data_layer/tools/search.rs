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
        Some(
            relative
                .to_string_lossy()
                .replace('\\', "/")
                .into_bytes(),
        )
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
    /// score, best first. Ties on rarity go to the smaller trigram.
    ///
    /// Rarity is counted differently by the two sources. The legacy index
    /// counts the checkout's files holding the trigram; the data layer counts
    /// [`CheckoutView::trigram_frequency`]: contents in the family's segments
    /// holding it, so two files with one content count once, and a content
    /// no file of this checkout holds any longer still counts.
    pub(crate) fn lexical_selected_pool(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
    ) -> io::Result<Vec<(PathBuf, f32)>> {
        let text = match self {
            Self::Snapshot(snapshot) => {
                return snapshot.lexical_selected_pool(query_trigrams, candidate_filter)
            }
            Self::Data(text) => text,
        };
        let unique = unique_trigrams(query_trigrams);
        let postings = text.postings(&unique)?;
        let picked = rarest(text, &unique, &postings, Rarity::Frequency)?;
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

    /// Every file holding one of the rarest query trigrams, widened by the
    /// next rarest until at least `depth` files are found, scored and cut to
    /// the best `depth`; as `SearchIndexSnapshot::lexical_rank_at_depth`.
    /// The data layer ranks rarity by [`CheckoutView::trigram_frequency`],
    /// as the legacy index ranks it by its stored posting counts: both
    /// count records, not the checkout's files.
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
        let mut non_zero = Vec::new();
        for &trigram in query_trigrams {
            let count = text.frequency(trigram)?;
            if count > 0 {
                non_zero.push((trigram, count));
            }
        }
        if non_zero.is_empty() {
            return Ok(Vec::new());
        }
        non_zero.sort_unstable_by_key(|(_, count)| *count);
        let postings = text.postings(query_trigrams)?;
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

/// How the lexical lane ranks query trigrams by rarity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rarity {
    /// By [`CheckoutView::trigram_frequency`]: contents in the family's
    /// segments.
    Frequency,
    /// By the checkout's files holding the trigram, as the legacy index
    /// ranks them; the parity test measures what the difference moves.
    #[cfg(test)]
    Files,
}

/// The (at most) three rarest of `unique` that some path holds, rarest
/// first, ties to the smaller trigram.
fn rarest(
    text: &DataText,
    unique: &[u32],
    postings: &HashMap<u32, Vec<Vec<u8>>>,
    rarity: Rarity,
) -> io::Result<Vec<u32>> {
    let mut held = Vec::new();
    for &trigram in unique {
        let paths = &postings[&trigram];
        if paths.is_empty() {
            continue;
        }
        let count = match rarity {
            Rarity::Frequency => text.frequency(trigram)?,
            #[cfg(test)]
            Rarity::Files => paths.len() as u64,
        };
        held.push((count, trigram));
    }
    held.sort_unstable();
    Ok(held.into_iter().take(3).map(|(_, trigram)| trigram).collect())
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
    if let Some(bound) = evidence_bound(text, scope, &norm_phrase, &content_tokens, &mut holders)?
    {
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

    /// The lexical lane's canonical order, with each score's bits.
    fn lane(index: TextIndex<'_>, trigrams: &[u32], include_tests: bool, root: &Path) -> String {
        let filter = |path: &Path| {
            crate::commands::semantic_search::path_allowed_by_include_tests(
                path,
                root,
                include_tests,
            )
        };
        let lane = CanonicalLexicalLane::from_text_index(
            index,
            trigrams,
            Some(&filter),
            LEXICAL_ENUMERATION_LIMIT,
        )
        .unwrap();
        let order = lane
            .canonical_order()
            .iter()
            .map(|candidate| (candidate.result.path.clone(), candidate.raw_score.to_bits()))
            .collect::<Vec<_>>();
        format!("{}:{order:?}", lane.selected_pool_size())
    }

    #[test]
    fn the_view_answers_each_lane_question_as_the_legacy_index_does() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/upload.rs", "pub fn retry_upload() {}\n// retries the upload\n");
        write(root, "src/client.rs", "fn send() { retry_upload(); }\nstruct Client;\n");
        write(root, "docs/notes.md", "The upload is retried with a backoff.\n");
        write(root, "tests/upload_test.rs", "fn upload_test() { retry_upload(); }\n");
        write(root, "data.bin", "\0\0binary retry_upload\0");
        let pair = pair(root);
        let legacy = pair.legacy.snapshot();
        let legacy = TextIndex::Snapshot(&legacy);
        let data = TextIndex::Data(&pair.data);
        for query in ["retry upload", "retry_upload", "Client send", "backoff", "zz"] {
            let trigrams = trigrams(query);
            for include_tests in [false, true] {
                assert_eq!(
                    lane(legacy, &trigrams, include_tests, &pair.root),
                    lane(data, &trigrams, include_tests, &pair.root),
                    "{query}"
                );
            }
            assert_eq!(
                legacy.lexical_rank_at_depth(&trigrams, None, usize::MAX).unwrap(),
                data.lexical_rank_at_depth(&trigrams, None, usize::MAX).unwrap(),
                "{query}"
            );
            for scope in [
                ExactEvidenceScope::All,
                ExactEvidenceScope::PhraseAndWindow,
                ExactEvidenceScope::Phrase,
            ] {
                assert_eq!(
                    legacy
                        .whole_corpus_exact_pass_scoped(query, &pair.root, None, &scope)
                        .unwrap(),
                    data.whole_corpus_exact_pass_scoped(query, &pair.root, None, &scope)
                        .unwrap(),
                    "{query} {scope:?}"
                );
            }
            assert_eq!(
                legacy
                    .files_possibly_holding_phrase(query, &pair.root)
                    .unwrap(),
                data.files_possibly_holding_phrase(query, &pair.root)
                    .unwrap(),
                "{query}"
            );
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

    /// The legacy index ranks query trigrams by the files holding them; the
    /// data layer by `trigram_frequency`, which counts one content once.
    /// Two files with the same content therefore make a trigram rarer to the
    /// data layer than to the legacy index, and the three picked trigrams
    /// can differ.
    #[test]
    fn a_duplicated_content_counts_once_in_the_data_layers_rarity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "x1.txt", "xxx\n");
        write(root, "x2.txt", "xxx\n");
        for (name, text) in [("a", "aaa"), ("b", "bbb"), ("c", "ccc")] {
            write(root, &format!("{name}1.txt"), &format!("{text} one\n"));
            write(root, &format!("{name}2.txt"), &format!("{text} two\n"));
        }
        let pair = pair(root);
        let unique = unique_trigrams(&trigrams("xxx aaa bbb ccc"));
        let postings = pair.data.postings(&unique).unwrap();
        let pack = |text: &[u8]| aft_store::text::pack(text[0], text[1], text[2]);
        assert_eq!(
            rarest(&pair.data, &unique, &postings, Rarity::Files).unwrap(),
            vec![pack(b"aaa"), pack(b"bbb"), pack(b"ccc")]
        );
        assert_eq!(
            rarest(&pair.data, &unique, &postings, Rarity::Frequency).unwrap(),
            vec![pack(b"xxx"), pack(b"aaa"), pack(b"bbb")]
        );
    }

    /// Statistics of one comparison over a query list.
    #[derive(Default, serde::Serialize)]
    struct Tally {
        queries: usize,
        differing: usize,
        legacy: Duration,
        data: Duration,
        examples: Vec<serde_json::Value>,
    }

    impl Tally {
        fn record(
            &mut self,
            query: &str,
            legacy: (String, Duration),
            data: (String, Duration),
        ) {
            self.queries += 1;
            self.legacy += legacy.1;
            self.data += data.1;
            if legacy.0 != data.0 {
                self.differing += 1;
                if self.examples.len() < 20 {
                    self.examples.push(serde_json::json!({
                        "query": query,
                        "legacy": legacy.0.chars().take(4000).collect::<String>(),
                        "data": data.0.chars().take(4000).collect::<String>(),
                    }));
                }
            }
        }

        fn summary(&self) -> serde_json::Value {
            let per_call = |total: Duration| {
                if self.queries == 0 {
                    0.0
                } else {
                    total.as_secs_f64() * 1000.0 / self.queries as f64
                }
            };
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

    /// Runs every lane question for each query of a real query list against
    /// the legacy index and the data layer's view of one checkout, and
    /// writes what differs and what each costs. The gate for wiring
    /// aft_search onto the data layer runs it on aft's own checkout:
    ///
    /// `AFT_SEARCH_PARITY_ROOT=<checkout> AFT_SEARCH_PARITY_QUERIES=<one
    /// query per line> AFT_SEARCH_PARITY_OUT=<report.json> cargo test -p
    /// agent-file-tools --lib -- --ignored lanes_match_the_legacy_index`
    #[test]
    #[ignore = "reads the checkout and query list named by AFT_SEARCH_PARITY_*"]
    fn lanes_match_the_legacy_index_on_real_queries() {
        let root = PathBuf::from(std::env::var("AFT_SEARCH_PARITY_ROOT").unwrap());
        let queries = std::fs::read_to_string(std::env::var("AFT_SEARCH_PARITY_QUERIES").unwrap())
            .unwrap();
        let out = PathBuf::from(std::env::var("AFT_SEARCH_PARITY_OUT").unwrap());
        let exact_limit = std::env::var("AFT_SEARCH_PARITY_EXACT_LIMIT")
            .ok()
            .and_then(|limit| limit.parse::<usize>().ok())
            .unwrap_or(usize::MAX);
        let (pair, build) = timed(|| pair(&root));
        let snapshot = pair.legacy.snapshot();
        let legacy = TextIndex::Snapshot(&snapshot);
        let data = TextIndex::Data(&pair.data);
        let root = pair.root.clone();

        let mut lanes = Tally::default();
        let mut lanes_with_tests = Tally::default();
        let mut picks_by_frequency = Tally::default();
        let mut picks_as_sets = Tally::default();
        let mut pool_by_file_rarity = Tally::default();
        let mut unpooled = Tally::default();
        let mut exact = Tally::default();
        let mut phrase_holders = Tally::default();
        let mut anchored = Tally::default();
        let mut seen = HashSet::new();
        let mut distinct = 0usize;
        for query in queries.lines().map(str::trim).filter(|query| !query.is_empty()) {
            if !seen.insert(query.to_string()) {
                continue;
            }
            distinct += 1;
            let trigrams = trigrams(query);
            lanes.record(
                query,
                timed(|| lane(legacy, &trigrams, false, &root)),
                timed(|| lane(data, &trigrams, false, &root)),
            );
            lanes_with_tests.record(
                query,
                timed(|| lane(legacy, &trigrams, true, &root)),
                timed(|| lane(data, &trigrams, true, &root)),
            );

            // Which three trigrams each rarity picks, and what the pool would
            // be if the data layer ranked rarity by files.
            let unique = unique_trigrams(&trigrams);
            let postings = pair.data.postings(&unique).unwrap();
            let by_files = rarest(&pair.data, &unique, &postings, Rarity::Files).unwrap();
            let by_frequency = rarest(&pair.data, &unique, &postings, Rarity::Frequency).unwrap();
            let zero = Duration::ZERO;
            picks_by_frequency.record(
                query,
                (format!("{by_files:06x?}"), zero),
                (format!("{by_frequency:06x?}"), zero),
            );
            let as_set = |picks: &[u32]| picks.iter().copied().collect::<BTreeSet<u32>>();
            picks_as_sets.record(
                query,
                (format!("{:06x?}", as_set(&by_files)), zero),
                (format!("{:06x?}", as_set(&by_frequency)), zero),
            );
            let pool = |picks: &[u32]| {
                picks
                    .iter()
                    .flat_map(|trigram| postings[trigram].iter())
                    .collect::<BTreeSet<_>>()
                    .len()
            };
            pool_by_file_rarity.record(
                query,
                (format!("{}", pool(&by_files)), zero),
                (format!("{}", pool(&by_frequency)), zero),
            );
            unpooled.record(
                query,
                timed(|| format!("{:?}", legacy.lexical_rank_at_depth(&trigrams, None, usize::MAX))),
                timed(|| format!("{:?}", data.lexical_rank_at_depth(&trigrams, None, usize::MAX))),
            );

            if exact.queries < exact_limit {
                let filter = |path: &Path| {
                    crate::commands::semantic_search::path_allowed_by_include_tests(
                        path, &root, false,
                    )
                };
                for scope in [
                    ExactEvidenceScope::All,
                    ExactEvidenceScope::PhraseAndWindow,
                    ExactEvidenceScope::Phrase,
                ] {
                    exact.record(
                        &format!("{scope:?} {query}"),
                        timed(|| {
                            format!(
                                "{:?}",
                                legacy.whole_corpus_exact_pass_scoped(
                                    query,
                                    &root,
                                    Some(&filter),
                                    &scope
                                )
                            )
                        }),
                        timed(|| {
                            format!(
                                "{:?}",
                                data.whole_corpus_exact_pass_scoped(
                                    query,
                                    &root,
                                    Some(&filter),
                                    &scope
                                )
                            )
                        }),
                    );
                }
                let phrase = exact_lane::exact_phrase(query);
                phrase_holders.record(
                    query,
                    timed(|| format!("{:?}", legacy.files_possibly_holding_phrase(phrase, &root))),
                    timed(|| format!("{:?}", data.files_possibly_holding_phrase(phrase, &root))),
                );
                let runs = split_query(query).retained_runs;
                anchored.record(
                    query,
                    timed(|| {
                        let candidates =
                            discover_anchored_candidate_files(&pair.legacy, &root, &runs, false);
                        let executed = AnchoredLane::new()
                            .execute_with_admission_report(&pair.legacy, &root, query, false);
                        format!("{:?}\n{executed:?}", candidates.map(|files| files.len()))
                    }),
                    timed(|| {
                        let candidates = anchored_candidate_files(&pair.data, &root, &runs, false);
                        let executed =
                            AnchoredLane::new().execute_on_data_view(&pair.data, &root, query, false);
                        format!("{:?}\n{executed:?}", candidates.map(|files| files.len()))
                    }),
                );
            }
        }
        let report = serde_json::json!({
            "root": root,
            "distinct_queries": distinct,
            "build_ms_legacy_and_data": build.as_secs_f64() * 1000.0,
            "lexical_lane": lanes.summary(),
            "lexical_lane_include_tests": lanes_with_tests.summary(),
            "rarest_three_in_order": picks_by_frequency.summary(),
            "rarest_three_as_a_set": picks_as_sets.summary(),
            "pool_size_files_vs_frequency": pool_by_file_rarity.summary(),
            "unpooled_rank": unpooled.summary(),
            "exact_pass": exact.summary(),
            "files_possibly_holding_phrase": phrase_holders.summary(),
            "anchored": anchored.summary(),
        });
        std::fs::write(&out, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        for (name, tally) in [
            ("lexical_lane", &lanes),
            ("lexical_lane_include_tests", &lanes_with_tests),
            ("rarest_three_in_order", &picks_by_frequency),
            ("rarest_three_as_a_set", &picks_as_sets),
            ("pool_size_files_vs_frequency", &pool_by_file_rarity),
            ("unpooled_rank", &unpooled),
            ("exact_pass", &exact),
            ("files_possibly_holding_phrase", &phrase_holders),
            ("anchored", &anchored),
        ] {
            println!(
                "{name}: {} of {} differ; legacy {:.3} ms, data {:.3} ms per call",
                tally.differing,
                tally.queries,
                tally.legacy.as_secs_f64() * 1000.0 / tally.queries.max(1) as f64,
                tally.data.as_secs_f64() * 1000.0 / tally.queries.max(1) as f64,
            );
        }
    }
}
