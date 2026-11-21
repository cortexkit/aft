use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use crate::query_shape::{extract_content_tokens, minimum_content_token_window};
use crate::search_state::candidate::{CandidateResult, SymbolOffsetRange};
use crate::search_state::evidence_descriptor::EvidenceDescriptor;
use crate::search_state::waits::{self, SearchWait};

/// Exact phrase extraction: strips surrounding single/double quotes if balanced.
pub fn exact_phrase(query: &str) -> &str {
    let trimmed = query.trim();
    if trimmed.len() < 2 {
        return trimmed;
    }
    let first = trimmed.as_bytes()[0];
    let last = trimmed.as_bytes()[trimmed.len() - 1];
    if matches!(first, b'\'' | b'"') && first == last {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

/// Normalized exact phrase for verbatim comparison.
pub fn normalize_exact_phrase(text: &str) -> String {
    // Normalizing never lengthens the text. Exact-size storage keeps a
    // prepared body's cache charge within its source range.
    let mut out = String::with_capacity(text.len());
    push_normalized_exact_phrase(text, &mut out);
    out.shrink_to_fit();
    out
}

/// Appends `text`'s normalized exact phrase to `out`: its words joined by
/// single spaces, ASCII-lowercased. Callers that normalize many lines into one
/// buffer use this so they can't drift from [`normalize_exact_phrase`].
pub fn push_normalized_exact_phrase(text: &str, out: &mut String) {
    let start = out.len();
    for word in text.split_whitespace() {
        if out.len() > start {
            out.push(' ');
        }
        out.push_str(word);
    }
    out[start..].make_ascii_lowercase();
}

/// Return whether a file can possibly contain an E2 window for all query tokens.
pub fn e2_window_scan_needed(normalized_text: &str, content_tokens: &[String]) -> bool {
    content_tokens.len() >= 2
        && content_tokens
            .iter()
            .all(|token| normalized_text.contains(token))
}

/// The query words that may stand in for the whole query as exact evidence: a
/// declaration named after one of them, or all of them within a short window.
///
/// A single hyphenated token is a literal name, so it gets none: its parts are
/// not words the user typed, and only a file containing the whole string (E1)
/// is exact evidence for it.
pub fn exact_verification_tokens(query: &str) -> Vec<String> {
    if crate::search_b2::router::is_hyphenated_literal(query) {
        Vec::new()
    } else {
        extract_content_tokens(query)
    }
}

/// Whether a declaration-shaped line in `path` may count as a definition.
///
/// Only source files of a language with a parser qualify. Documents and data
/// (Markdown notes, JSON, YAML, TOML, HTML, stylesheets, or a file with no
/// recognised language) often quote a declaration or carry a `name:` key, but
/// they do not define the name; giving them definition credit let a planning
/// note outrank the source file that declares the identifier.
pub fn is_definition_source(path: &Path) -> bool {
    use crate::parser::LangId;
    crate::parser::detect_language(path).is_some_and(|language| {
        !matches!(
            language,
            LangId::Markdown
                | LangId::Json
                | LangId::Yaml
                | LangId::Toml
                | LangId::Html
                | LangId::Scss
        )
    })
}

/// The member name a `receiver.member` query asks for, lowercased.
///
/// A query such as `ask.list_pending_for_user` is how a call site reads, so
/// its exact phrase only ever matches call sites: the declaration says
/// `fn list_pending_for_user`, without the receiver. The member name is
/// returned only when the whole exact phrase is one dotted name and the last
/// segment is itself identifier-shaped (snake_case or camelCase); a plain
/// word such as `lock` or `check` names too many unrelated declarations to
/// rank as the one the query means.
pub fn qualified_member_name(query: &str) -> Option<QualifiedMember> {
    let phrase = exact_phrase(query);
    let segments = phrase.split('.').collect::<Vec<_>>();
    let is_name = |segment: &str| {
        segment
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_' || first == '$')
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$')
    };
    if segments.len() < 2 || !segments.iter().all(|segment| is_name(segment)) {
        return None;
    }
    let member = segments[segments.len() - 1];
    if member.len() < 3 || !crate::search_b2::router::is_identifier_shaped_token(member) {
        return None;
    }
    Some(QualifiedMember {
        receiver: segments[segments.len() - 2].to_ascii_lowercase(),
        member: member.to_ascii_lowercase(),
    })
}

/// The parts of a `receiver.member` query that the exact lane uses; see
/// [`qualified_member_name`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualifiedMember {
    /// The segment before the member, lowercased (`ask` in `ask.list_pending`).
    pub receiver: String,
    /// The member name, lowercased.
    pub member: String,
}

impl QualifiedMember {
    /// Whether `path` names the receiver, as a cheap stand-in for resolving
    /// the receiver's type: a directory or file stem that contains the
    /// receiver (`ask` in `ask/store.rs`) or abbreviates it the way variable
    /// names do (`ctx` for `context.rs`: same first letter, the receiver's
    /// letters appear in order). `self` and `this` name no type and never match.
    pub fn path_names_receiver(&self, path: &Path) -> bool {
        let receiver = self.receiver.as_str();
        if receiver.len() < 2 || matches!(receiver, "self" | "this") {
            return false;
        }
        let stem = path.file_stem();
        path.parent()
            .into_iter()
            .flat_map(Path::components)
            .map(|component| component.as_os_str())
            .chain(stem)
            .filter_map(|component| component.to_str())
            .map(str::to_ascii_lowercase)
            .any(|component| component.contains(receiver) || is_abbreviation(receiver, &component))
    }
}

/// `short` starts with the same letter as `long` and its letters appear in
/// `long` in order (`ctx` abbreviates `context`, `cfg` abbreviates `config`).
fn is_abbreviation(short: &str, long: &str) -> bool {
    if short.len() >= long.len() || short.as_bytes().first() != long.as_bytes().first() {
        return false;
    }
    let mut letters = long.bytes();
    short
        .bytes()
        .all(|wanted| letters.by_ref().any(|letter| letter == wanted))
}

/// Verify exact matches in file text and return candidates.
pub fn verify_exact_matches_in_text(
    file_path: &Path,
    text: &str,
    norm_phrase: &str,
    content_tokens: &[String],
) -> Option<Vec<CandidateResult>> {
    verify_exact_matches_in_text_with_member(file_path, text, norm_phrase, content_tokens, None)
}

/// [`verify_exact_matches_in_text`], where a declaration of `member` (the
/// lowercased last segment of a `receiver.member` query, see
/// [`qualified_member_name`]) also counts as a definition hit.
///
/// Definition credit goes only to a file for which [`is_definition_source`]
/// holds. In any other file a declaration-shaped line is treated like any
/// other text: it is E1 evidence when it contains the phrase.
pub fn verify_exact_matches_in_text_with_member(
    file_path: &Path,
    text: &str,
    norm_phrase: &str,
    content_tokens: &[String],
    member: Option<&str>,
) -> Option<Vec<CandidateResult>> {
    let prepared = PreparedExactText::new(text);
    verify_prepared_exact_matches(
        file_path,
        text,
        &prepared,
        norm_phrase,
        content_tokens,
        member,
    )
}

struct PreparedExactSymbol {
    name: String,
    range: SymbolOffsetRange,
    normalized: OnceLock<String>,
    member_declaration: bool,
}

pub(crate) struct PreparedExactText {
    normalized: String,
    symbols: Vec<PreparedExactSymbol>,
}

impl PreparedExactText {
    pub(crate) fn normalized(&self) -> &str {
        &self.normalized
    }

    fn new(text: &str) -> Self {
        let symbols = scan_symbols_in_text(text)
            .into_iter()
            .filter_map(|(name, range)| {
                // A scanner offset must address a UTF-8 boundary, even for CRLF
                // input. A bad offset must not panic the search actor.
                let Some(symbol_text) = text.get(range.start..range.end.min(text.len())) else {
                    debug_assert!(false, "symbol range {range:?} is not char-boundary aligned");
                    return None;
                };
                Some(PreparedExactSymbol {
                    member_declaration: !symbol_text.trim_start().starts_with(name.as_str()),
                    name: name.to_ascii_lowercase(),
                    range,
                    normalized: OnceLock::new(),
                })
            })
            .collect();
        Self {
            normalized: normalize_exact_phrase(text),
            symbols,
        }
    }

    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.normalized.capacity()
            + self.symbols.capacity() * std::mem::size_of::<PreparedExactSymbol>()
            + self
                .symbols
                .iter()
                // Whitespace folding and ASCII case folding cannot grow the
                // source slice. Reserve its full size before lazy preparation
                // so parallel verifications cannot exceed the cache charge.
                .map(|symbol| symbol.name.capacity() + symbol.range.end - symbol.range.start)
                .sum::<usize>()
    }
}

struct PreparedExactEntry {
    text: Arc<OnceLock<Arc<PreparedExactText>>>,
    last_used: u64,
    bytes: usize,
}

// Charge map slack and the Arc/OnceLock allocation even before text is ready.
// Small files must not hit an unrelated entry limit while bytes remain free.
const PREPARED_EXACT_ENTRY_BYTES: usize = 256;

#[derive(Default)]
struct PreparedExactCacheState {
    entries: HashMap<blake3::Hash, PreparedExactEntry>,
    clock: u64,
    bytes: usize,
}

impl PreparedExactCacheState {
    fn evict_oldest(&mut self) {
        if let Some(key) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| *key)
        {
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes -= entry.bytes;
            }
        }
    }
}

struct PreparedExactCache {
    state: parking_lot::Mutex<PreparedExactCacheState>,
    max_bytes: usize,
    #[cfg(test)]
    preparations: std::sync::atomic::AtomicUsize,
}

impl PreparedExactCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            state: parking_lot::Mutex::new(PreparedExactCacheState::default()),
            max_bytes,
            #[cfg(test)]
            preparations: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn prepare(&self, digest: blake3::Hash, text: &str) -> Arc<PreparedExactText> {
        // A placeholder found unfilled means another request is preparing the
        // same content: waiting for it is a wait, preparing it ourselves is not.
        let (cell, another_preparing) = {
            let mut state = waits::timed(SearchWait::PrepareCache, || self.state.lock());
            state.clock = state.clock.wrapping_add(1);
            let clock = state.clock;
            if let Some(entry) = state.entries.get_mut(&digest) {
                entry.last_used = clock;
                waits::record_prepared_text(|counts| counts.hits += 1);
                if let Some(prepared) = entry.text.get() {
                    return Arc::clone(prepared);
                }
                (Arc::clone(&entry.text), true)
            } else {
                waits::record_prepared_text(|counts| counts.misses += 1);
                // Reserve metadata before insertion, so in-flight placeholders
                // share the same byte budget as completed preparations.
                while state.bytes + PREPARED_EXACT_ENTRY_BYTES > self.max_bytes
                    && !state.entries.is_empty()
                {
                    state.evict_oldest();
                    waits::record_prepared_text(|counts| counts.evicted_by_bytes += 1);
                }
                let cell = Arc::new(OnceLock::new());
                if PREPARED_EXACT_ENTRY_BYTES <= self.max_bytes {
                    state.entries.insert(
                        digest,
                        PreparedExactEntry {
                            text: Arc::clone(&cell),
                            last_used: clock,
                            bytes: PREPARED_EXACT_ENTRY_BYTES,
                        },
                    );
                    state.bytes += PREPARED_EXACT_ENTRY_BYTES;
                }
                (cell, false)
            }
        };
        let other_wait = another_preparing.then(|| waits::time(SearchWait::PrepareCache));
        let prepared = cell.get_or_init(|| {
            #[cfg(test)]
            self.preparations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            #[cfg(test)]
            crate::search_test_counts::record(|counts| counts.exact_preparations += 1);
            Arc::new(PreparedExactText::new(text))
        });
        drop(other_wait);
        let mut state = waits::timed(SearchWait::PrepareCache, || self.state.lock());
        if let Some(entry) = state.entries.get_mut(&digest) {
            if Arc::ptr_eq(&entry.text, &cell) && entry.bytes == PREPARED_EXACT_ENTRY_BYTES {
                entry.bytes += prepared.retained_bytes();
                state.bytes += prepared.retained_bytes();
            }
        }
        while state.bytes > self.max_bytes {
            state.evict_oldest();
            waits::record_prepared_text(|counts| counts.evicted_by_bytes += 1);
        }
        Arc::clone(prepared)
    }
}

/// Reuse only query-independent work, addressed by the bytes just read from
/// disk. A new content hash cannot reuse old preparation, even before a watcher
/// notices a write. No candidate selection, evidence, or digest check is cached.
pub(crate) fn verify_cached_exact_matches_in_text_with_member(
    file_path: &Path,
    text: &str,
    digest: blake3::Hash,
    norm_phrase: &str,
    content_tokens: &[String],
    member: Option<&str>,
) -> Option<Vec<CandidateResult>> {
    let prepared = prepare_exact_text(text, digest);
    verify_prepared_exact_matches(
        file_path,
        text,
        &prepared,
        norm_phrase,
        content_tokens,
        member,
    )
}

/// Lexical verification and the exact lane normalize the same bytes, which
/// one search request reads once (see `request_files`). Share their existing
/// bounded preparation, keyed by the bytes' digest, not their phrase, window
/// or definition match decisions; a later request that reads changed bytes
/// gets a new digest and prepares them afresh.
pub(crate) fn prepare_exact_text(text: &str, digest: blake3::Hash) -> Arc<PreparedExactText> {
    static CACHE: OnceLock<PreparedExactCache> = OnceLock::new();
    // The process-wide budget includes text, lazy symbol normalization, and
    // per-entry metadata. Only bytes drive eviction: a count cap discarded
    // reusable small files while the byte budget still had room.
    CACHE
        .get_or_init(|| PreparedExactCache::new(64 * 1024 * 1024))
        .prepare(digest, text)
}

fn verify_prepared_exact_matches(
    file_path: &Path,
    text: &str,
    prepared: &PreparedExactText,
    norm_phrase: &str,
    content_tokens: &[String],
    member: Option<&str>,
) -> Option<Vec<CandidateResult>> {
    let mut matches = Vec::new();
    let norm_text = &prepared.normalized;
    let definition_source = is_definition_source(file_path);
    // A normalized symbol body is a substring of the normalized whole file.
    // Missing phrases need no symbol-body preparation or per-symbol scan.
    let phrase_present = !norm_phrase.is_empty() && norm_text.contains(norm_phrase);

    // 1. Check symbols first (for symbol-level candidates like `cap_chars`)
    for symbol in &prepared.symbols {
        let range = &symbol.range;
        let lowercase_name = &symbol.name;
        // The member stand-in counts only item declarations (`fn`, `def`,
        // `function`, `class`, ...), whose line starts with a keyword. A
        // `name:` line is a struct field or an object-literal key, and for a
        // member name the keys are mostly test doubles (`clearPluginCache:
        // async () => ...`) that would outrank the real declaration.
        let names_member =
            member.is_some_and(|member| lowercase_name == member && symbol.member_declaration);
        let names_query =
            content_tokens.iter().any(|token| lowercase_name == token) || names_member;
        if definition_source && names_query {
            matches.push(CandidateResult::new_exact(
                file_path.to_path_buf(),
                Some(*range),
                EvidenceDescriptor::for_definition(true, false),
            ));
        } else if phrase_present {
            let norm_sym_text = symbol.normalized.get_or_init(|| {
                normalize_exact_phrase(&text[range.start..range.end.min(text.len())])
            });
            if norm_sym_text.contains(norm_phrase) {
                let occ = norm_sym_text.matches(norm_phrase).count();
                matches.push(CandidateResult::new_exact(
                    file_path.to_path_buf(),
                    Some(*range),
                    EvidenceDescriptor::for_e1(occ, true, false),
                ));
            }
        }
    }

    // 2. If no symbol matched, check verbatim phrase match in whole file (E1)
    if matches.is_empty() && phrase_present {
        let occ = norm_text.matches(norm_phrase).count();
        if occ > 0 {
            matches.push(CandidateResult::new_exact(
                file_path.to_path_buf(),
                None,
                EvidenceDescriptor::for_e1(occ, true, false),
            ));
        }
    }

    // 3. Check 3-line window match (E2) if no E1 match on file-level.
    // Files missing any query token cannot contain an all-token window, so reject
    // them before scanning identifiers and tracking line coverage.
    if matches.is_empty() && e2_window_scan_needed(norm_text, content_tokens) {
        if let Some(width) = minimum_content_token_window(text, content_tokens) {
            matches.push(CandidateResult::new_exact(
                file_path.to_path_buf(),
                None,
                EvidenceDescriptor::for_e2(width, true, false),
            ));
        }
    }

    if matches.is_empty() {
        None
    } else {
        Some(matches)
    }
}

/// Lightweight symbol scanner finding functions / structs / classes in source files.
///
/// Offsets are true byte positions into `text`. The walk uses
/// `split_inclusive('\n')` rather than `lines()` because `lines()` strips a
/// trailing `\r` as well as the `\n`, so summing `line.len() + 1` drifts one
/// byte left per CRLF line; on a large CRLF file with non-ASCII content the
/// drifted start landed inside a multibyte character and slicing it panicked
/// the search actor.
pub fn scan_symbols_in_text(text: &str) -> Vec<(String, SymbolOffsetRange)> {
    let mut symbols = Vec::new();
    let mut current_offset = 0;

    for segment in text.split_inclusive('\n') {
        let line = segment
            .strip_suffix('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .unwrap_or(segment);
        let trimmed = line.trim_start();
        let leading_spaces = line.len() - trimmed.len();
        let line_offset = current_offset + leading_spaces;

        let (name_opt, declaration_line_only) = if let Some(rest) = trimmed.strip_prefix("pub fn ")
        {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("fn ") {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("pub(crate) fn ") {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("def ") {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("export function ") {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("function ") {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("pub struct ") {
            (extract_identifier(rest), false)
        } else if let Some(rest) = trimmed.strip_prefix("struct ") {
            (extract_identifier(rest), false)
        } else {
            (
                [
                    "export namespace ",
                    "namespace ",
                    "export class ",
                    "class ",
                    "export interface ",
                    "interface ",
                    "export enum ",
                    "pub enum ",
                    "enum ",
                    "pub trait ",
                    "trait ",
                    "export type ",
                    "pub type ",
                    "type ",
                    "export const ",
                    "pub const ",
                    "const ",
                ]
                .into_iter()
                .find_map(|prefix| trimmed.strip_prefix(prefix).and_then(extract_identifier))
                .or_else(|| extract_field_identifier(trimmed)),
                true,
            )
        };

        if let Some(name) = name_opt {
            let start = line_offset;
            debug_assert!(text.is_char_boundary(start));
            // The lightweight span is byte-addressed because downstream ranges
            // slice UTF-8 source. Clamp the approximate end to a character boundary.
            let mut end = if declaration_line_only {
                (current_offset + line.len()).min(text.len())
            } else {
                (start + 500).min(text.len())
            };
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            symbols.push((name, SymbolOffsetRange::new(start, end)));
        }

        current_offset += segment.len();
    }

    // Approximate bodies must not borrow evidence from the next declaration.
    for index in 0..symbols.len().saturating_sub(1) {
        symbols[index].1.end = symbols[index].1.end.min(symbols[index + 1].1.start);
    }
    symbols
}

#[cfg(test)]
#[test]
fn audit01_approximate_span_stops_at_next_declaration() {
    let source = "pub fn first() { }\n\npub fn second() { target_phrase(); }\n";
    let symbols = scan_symbols_in_text(source);
    assert_eq!(symbols.len(), 2);
    assert!(symbols[0].1.end <= symbols[1].1.start);
    assert!(!source[symbols[0].1.start..symbols[0].1.end].contains("target_phrase"));
    assert!(source[symbols[1].1.start..symbols[1].1.end].contains("target_phrase"));

    let matches = verify_exact_matches_in_text(
        Path::new("fixture.rs"),
        source,
        "target_phrase",
        &["target_phrase".to_string()],
    )
    .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].symbol_range, Some(symbols[1].1));
}

fn extract_field_identifier(line: &str) -> Option<String> {
    let (candidate, suffix) = line.split_once(':')?;
    if suffix.starts_with(':') {
        return None;
    }
    let candidate = candidate.trim();
    if candidate.is_empty()
        || !candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    Some(candidate.to_string())
}

fn extract_identifier(s: &str) -> Option<String> {
    let ident: String = s
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if ident.is_empty() {
        None
    } else {
        Some(ident)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_symbol_normalization_is_lazy_and_reserves_its_capacity() {
        let text = "pub fn first() {\r\n    // Réused\u{2003}marker Réused marker\r\n}\r\npub fn second() {\n    // Réused marker\n}\n";
        let prepared = PreparedExactText::new(text);
        let reserved = prepared.retained_bytes();
        assert_eq!(prepared.symbols.len(), 2);
        assert!(prepared
            .symbols
            .iter()
            .all(|symbol| symbol.normalized.get().is_none()));
        assert!(verify_prepared_exact_matches(
            Path::new("lazy.rs"),
            text,
            &prepared,
            "absent phrase",
            &[],
            None,
        )
        .is_none());
        assert!(prepared
            .symbols
            .iter()
            .all(|symbol| symbol.normalized.get().is_none()));

        let results = verify_prepared_exact_matches(
            Path::new("lazy.rs"),
            text,
            &prepared,
            "réused marker",
            &[],
            None,
        )
        .unwrap();
        assert_eq!(
            results
                .iter()
                .map(|result| result.evidence.occurrences)
                .collect::<Vec<_>>(),
            vec![Some(2), Some(1)]
        );
        for symbol in &prepared.symbols {
            let normalized = symbol
                .normalized
                .get()
                .expect("matched phrase prepares the body");
            assert!(normalized.capacity() <= symbol.range.end - symbol.range.start);
        }
        assert_eq!(
            prepared.retained_bytes(),
            reserved,
            "lazy allocation is charged before it runs"
        );
    }

    #[test]
    fn exact_preparation_keeps_more_than_1024_small_contents_within_byte_budget() {
        let cache = PreparedExactCache::new(1024 * 1024);
        let texts = (0..1100)
            .map(|index| format!("fn cached_{index}() {{}}\n"))
            .collect::<Vec<_>>();
        let prepared = texts
            .iter()
            .map(|text| cache.prepare(blake3::hash(text.as_bytes()), text))
            .collect::<Vec<_>>();
        for (text, expected) in texts.iter().zip(&prepared) {
            let actual = cache.prepare(blake3::hash(text.as_bytes()), text);
            assert!(Arc::ptr_eq(expected, &actual));
        }
        assert_eq!(
            cache
                .preparations
                .load(std::sync::atomic::Ordering::Relaxed),
            texts.len(),
            "unchanged small contents must not be prepared again at the old count limit"
        );
        let state = cache.state.lock();
        assert_eq!(state.entries.len(), texts.len());
        assert_eq!(
            state.bytes,
            prepared
                .iter()
                .map(|text| text.retained_bytes())
                .sum::<usize>()
                + texts.len() * PREPARED_EXACT_ENTRY_BYTES
        );
        assert!(state.bytes <= cache.max_bytes);
    }

    #[test]
    fn exact_preparation_metadata_and_text_share_one_budget() {
        let text = "fn bounded() {}\n";
        let digest = blake3::hash(text.as_bytes());
        let charge = PreparedExactText::new(text).retained_bytes() + PREPARED_EXACT_ENTRY_BYTES;
        let cache = PreparedExactCache::new(charge - 1);
        cache.prepare(digest, text);
        assert!(cache.state.lock().entries.is_empty());
        assert_eq!(cache.state.lock().bytes, 0);
        let fits = PreparedExactCache::new(charge);
        fits.prepare(digest, text);
        assert_eq!(fits.state.lock().entries.len(), 1);
        assert_eq!(fits.state.lock().bytes, charge);
    }

    #[test]
    fn exact_preparation_single_flights_and_obeys_retained_byte_bound() {
        let cache = PreparedExactCache::new(1024 * 1024);
        let text = "fn café() {}\r\nfn beta() {}\r\n";
        let digest = blake3::hash(text.as_bytes());
        std::thread::scope(|scope| {
            let handles = (0..16)
                .map(|_| scope.spawn(|| cache.prepare(digest, text)))
                .collect::<Vec<_>>();
            let prepared = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>();
            assert!(prepared
                .iter()
                .all(|entry| Arc::ptr_eq(entry, &prepared[0])));
        });
        assert_eq!(
            cache
                .preparations
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let tiny = PreparedExactCache::new(1);
        tiny.prepare(digest, text);
        tiny.prepare(digest, text);
        assert_eq!(
            tiny.preparations.load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        assert_eq!(tiny.state.lock().bytes, 0);
        assert!(tiny.state.lock().entries.is_empty());
    }
}
