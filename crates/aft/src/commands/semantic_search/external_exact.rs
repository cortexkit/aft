//! Exact-occurrence sweep for identifier queries against another project.
//!
//! `aft_search` with a `path` outside the session's project borrows that
//! project's on-disk AFT index when one exists. This session never refreshes a
//! borrowed index, so its trigram postings can predate the files on disk: an
//! identifier added after the index was written is invisible to it. The ranked
//! lexical lanes then fill the page with files that merely share the
//! identifier's sub-tokens (`credential`, `stamps`), and in practice those are
//! large captured JSON dumps whose single giant line holds every common word.
//! Without any index the old answer was a substring scan in modification-time
//! order, so data files could use up the time budget before source was read.
//!
//! For an identifier the useful answer is every line that contains it, so this
//! module finds those lines directly:
//!
//! - files are enumerated once, under a deadline checked at every walk entry,
//!   then searched source first, documentation and other text second, data
//!   (JSON, logs, minified bundles, ...) last;
//! - a file whose size and modification time still match the borrowed index is
//!   answered by the index's trigram postings and read only when they say it
//!   can contain a searched term; every other file is read from disk;
//! - lines containing the identifier rank before lines containing only a
//!   spelling variant (`snake_case`, `PascalCase`, ...); within each group,
//!   source files holding a definition come first; variant lines never come
//!   from data files or from overlong lines;
//! - the reply states whether the sweep covered the whole project or stopped,
//!   and points at grep for an exhaustive check when it stopped.
//!
//! Token-split lexical matches are never part of this reply: they are what
//! padded identifier results with unrelated dump files.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::extensions::{RawQuery, Token};
use super::SearchShape;
use crate::list_envelope::{ListEnvelope, Reason, Total, Unit};
use crate::search_index::{
    decompose_regex, read_searchable_text, FileEntry, GrepMatch, SearchIndex,
};

/// Wall-clock budget shared by enumeration and reading, the same budget the
/// other bounded fallback walks use.
pub(super) const SWEEP_BUDGET: Duration = crate::grep_executor::FALLBACK_WALK_BUDGET;

/// Largest file the sweep reads. Higher than the index's per-file limit:
/// captured logs and dumps routinely exceed that limit, and an exact pass that
/// silently skipped them would not match what grep finds. The time budget, not
/// this cap, bounds the total work.
pub(super) const SWEEP_MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Lines longer than this never supply a spelling-variant hit. A line this
/// long is embedded content (a captured request body, a minified bundle), and
/// a near-spelling inside it is not evidence about the identifier.
pub(super) const VARIANT_MAX_LINE_BYTES: usize = 512;

/// Extensions whose files hold data rather than code or prose.
const DATA_EXTENSIONS: &[&str] = &[
    "json", "jsonl", "ndjson", "geojson", "log", "csv", "tsv", "map", "lock",
];

/// Words that introduce a declaration of the name that follows them.
const DEFINITION_KEYWORDS: &[&str] = &[
    "fn",
    "function",
    "function*",
    "def",
    "class",
    "struct",
    "enum",
    "trait",
    "interface",
    "type",
    "typedef",
    "union",
    "record",
    "object",
    "const",
    "let",
    "var",
    "val",
    "func",
    "mod",
    "module",
    "namespace",
    "static",
    "readonly",
    "#define",
    "macro_rules!",
];

/// The identifier a query names, plus the spelling variants the variants lane
/// would generate for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct IdentifierTerms {
    pub identifier: String,
    pub variants: Vec<String>,
}

impl IdentifierTerms {
    /// Terms for a query the router classifies as a single identifier, or
    /// `None` for every other query shape.
    pub(super) fn from_query(query: &str) -> Option<Self> {
        let raw = RawQuery::new(query);
        let (shape, facts) = crate::search_b2::router::classify(&raw);
        if shape != SearchShape::Identifier {
            return None;
        }
        let identifier = crate::search_b2::router::exact_input(&raw, shape, &facts)
            .trim()
            .to_string();
        if identifier.is_empty() {
            return None;
        }
        let variants = crate::search_b2::variants::generate_variants(Token {
            index: 0,
            text: &identifier,
        })
        .into_iter()
        .map(|variant| variant.text)
        .filter(|variant| !variant.is_empty() && *variant != identifier)
        .collect();
        Some(Self {
            identifier,
            variants,
        })
    }

    fn all_terms(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.identifier.as_str()).chain(self.variants.iter().map(String::as_str))
    }
}

/// What a file holds, in the order the sweep searches and ranks files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum FileClass {
    Source,
    Text,
    Data,
}

pub(super) fn classify_file(path: &Path) -> FileClass {
    use crate::parser::LangId;

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let minified = [".min.js", ".min.mjs", ".min.cjs", ".min.css"]
        .iter()
        .any(|suffix| name.ends_with(suffix));
    let data_extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            DATA_EXTENSIONS
                .iter()
                .any(|data| extension.eq_ignore_ascii_case(data))
        });
    if minified || data_extension {
        return FileClass::Data;
    }
    match crate::parser::detect_language(path) {
        Some(LangId::Markdown | LangId::Json | LangId::Yaml | LangId::Toml) | None => {
            FileClass::Text
        }
        Some(_) => FileClass::Source,
    }
}

/// Whether the hit's line contains the identifier itself or only a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum HitKind {
    Exact,
    Variant,
}

impl HitKind {
    fn result_source(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Variant => "variants",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct SweepHit {
    pub grep: GrepMatch,
    /// The matched line as displayed: bounded in length, match kept visible.
    pub snippet: String,
    pub kind: HitKind,
    pub class: FileClass,
    pub definition: bool,
}

#[derive(Debug, Default)]
pub(super) struct SweepOutcome {
    /// Every hit found, already in ranked order.
    pub hits: Vec<SweepHit>,
    /// Files the walk enumerated and kept (tests excluded unless requested).
    pub files_discovered: usize,
    /// Files whose outcome is known: ruled out by the index or read from disk.
    pub files_checked: usize,
    /// The walk itself stopped at the deadline, so the file total is unknown.
    pub enumeration_stopped: bool,
    /// Reading stopped at the deadline before every discovered file was checked.
    pub scan_stopped: bool,
    /// Whether a borrowed index was consulted.
    pub used_index: bool,
    /// Unchanged indexed files the index showed cannot contain a searched
    /// term, so they were not read.
    pub index_answered: usize,
    /// Files read from disk: changed, new, or index candidates.
    pub files_read: usize,
    /// Files skipped because they exceed [`SWEEP_MAX_FILE_BYTES`].
    pub oversized_skipped: usize,
}

impl SweepOutcome {
    /// True when every file under the root was checked.
    pub(super) fn fully_covered(&self) -> bool {
        !self.enumeration_stopped && !self.scan_stopped && self.oversized_skipped == 0
    }

    fn files_with_hits(&self) -> usize {
        self.hits
            .iter()
            .map(|hit| hit.grep.file.as_path())
            .collect::<HashSet<_>>()
            .len()
    }

    /// One plain-language line saying how much of the project was searched.
    pub(super) fn coverage_line(&self, root: &Path) -> String {
        let source = if self.used_index {
            format!(
                "this project's own AFT index answered {} unchanged files without reading them and {} files were read from disk",
                self.index_answered, self.files_read
            )
        } else {
            "no AFT index for this project was usable, so files were read from disk".to_string()
        };
        let oversized = if self.oversized_skipped > 0 {
            format!(
                "; {} files larger than {} bytes were not searched",
                self.oversized_skipped, SWEEP_MAX_FILE_BYTES
            )
        } else {
            String::new()
        };
        if self.enumeration_stopped || self.scan_stopped {
            let coverage = if self.enumeration_stopped {
                format!(
                    "{} files; total unknown because enumeration stopped",
                    self.files_checked
                )
            } else {
                format!(
                    "{} of {} discovered files",
                    self.files_checked, self.files_discovered
                )
            };
            format!(
                "exact pass: bounded ({} files, time limit); checked {coverage} ({source}){oversized}; source code and other files under {} not fully searched; use grep for an exhaustive check",
                self.files_checked,
                root.display()
            )
        } else if self.oversized_skipped > 0 {
            format!(
                "exact pass: checked {} of {} files under {} ({source}){oversized}; use grep for an exhaustive check",
                self.files_checked,
                self.files_discovered,
                root.display()
            )
        } else {
            format!(
                "exact pass: complete; checked all {} files under {} ({source})",
                self.files_checked,
                root.display()
            )
        }
    }
}

/// One page of the sweep, rendered for an `aft_search` reply.
pub(super) struct SweepReply {
    pub results: Vec<serde_json::Value>,
    pub text: String,
    pub more_available: bool,
    pub envelope: Option<ListEnvelope>,
    pub summary: serde_json::Value,
}

/// Search `root` for every line containing the identifier or one of its
/// spelling variants. `index`, when given, is the project's own AFT index; it
/// rules out unchanged files without reading them.
pub(super) fn sweep(
    root: &Path,
    terms: &IdentifierTerms,
    index: Option<&SearchIndex>,
    include_tests: bool,
    deadline: Instant,
) -> SweepOutcome {
    let mut outcome = SweepOutcome {
        used_index: index.is_some(),
        ..SweepOutcome::default()
    };

    // Enumerate first, checking the deadline at every walk entry (directories
    // and rejected entries included), so a huge tree cannot hold the request.
    let skipped_foreign_mounts = Arc::new(AtomicUsize::new(0));
    let walker = crate::grep_executor::fallback_project_walk_builder(root, skipped_foreign_mounts);
    let mut files: Vec<(FileClass, PathBuf)> = Vec::new();
    for entry in walker.build() {
        if Instant::now() >= deadline || crate::executor::current_job_cancelled() {
            outcome.enumeration_stopped = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if !super::path_allowed_by_include_tests(&path, root, include_tests) {
            continue;
        }
        files.push((classify_file(&path), path));
    }
    outcome.files_discovered = files.len();

    order_for_search(&mut files, root);

    let candidates = index.map(|index| index_candidates(index, terms));
    let mut hits = Vec::new();
    for (class, path) in &files {
        if Instant::now() >= deadline || crate::executor::current_job_cancelled() {
            outcome.scan_stopped = true;
            break;
        }
        if let (Some(index), Some(candidates)) = (index, candidates.as_ref()) {
            if let Some(&file_id) = index.path_to_id.get(path) {
                let unchanged = index
                    .files
                    .get(file_id as usize)
                    .is_some_and(|entry| stat_matches(entry, path));
                if unchanged && !candidates.contains(&file_id) {
                    outcome.index_answered += 1;
                    outcome.files_checked += 1;
                    continue;
                }
            }
        }
        let Ok(metadata) = std::fs::metadata(path) else {
            // Gone since enumeration: nothing left to search.
            outcome.files_checked += 1;
            continue;
        };
        if metadata.len() > SWEEP_MAX_FILE_BYTES {
            outcome.oversized_skipped += 1;
            continue;
        }
        outcome.files_read += 1;
        outcome.files_checked += 1;
        // Binary and non-UTF-8 files are checked but cannot hold a text hit.
        if let Some(content) = read_searchable_text(path) {
            scan_content(path, &content, *class, terms, &mut hits);
        }
    }

    rank_hits(&mut hits, root);
    outcome.hits = hits;
    outcome
}

/// Source before text before data, so a sweep that runs out of time has read
/// the code first; byte-wise relative path order inside each class keeps
/// bounded pages deterministic.
fn order_for_search(files: &mut [(FileClass, PathBuf)], root: &Path) {
    files.sort_by(|(left_class, left), (right_class, right)| {
        left_class
            .cmp(right_class)
            .then_with(|| relative_bytes(left, root).cmp(relative_bytes(right, root)))
    });
}

fn relative_bytes<'a>(path: &'a Path, root: &Path) -> &'a [u8] {
    path.strip_prefix(root)
        .unwrap_or(path)
        .as_os_str()
        .as_encoded_bytes()
}

fn stat_matches(entry: &FileEntry, path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| {
        metadata.len() == entry.size
            && metadata
                .modified()
                .is_ok_and(|modified| modified == entry.modified)
    })
}

/// Indexed files whose trigrams could contain any searched term.
fn index_candidates(index: &SearchIndex, terms: &IdentifierTerms) -> HashSet<u32> {
    let mut candidates = HashSet::new();
    for term in terms.all_terms() {
        let query = decompose_regex(&regex::escape(term));
        candidates.extend(index.candidates(&query));
    }
    candidates
}

fn scan_content(
    path: &Path,
    content: &str,
    class: FileClass,
    terms: &IdentifierTerms,
    hits: &mut Vec<SweepHit>,
) {
    let bytes = content.as_bytes();
    let variant_terms: Vec<&str> = if class == FileClass::Data {
        Vec::new()
    } else {
        terms
            .variants
            .iter()
            .map(String::as_str)
            .filter(|variant| memchr::memmem::find(bytes, variant.as_bytes()).is_some())
            .collect()
    };
    let has_identifier = memchr::memmem::find(bytes, terms.identifier.as_bytes()).is_some();
    if !has_identifier && variant_terms.is_empty() {
        return;
    }

    let line_starts = crate::grep_executor::line_starts(content);
    let mut reported_lines = HashSet::new();
    let mut push_hits = |term: &str, kind: HitKind, reported: &mut HashSet<u32>| {
        for offset in memchr::memmem::find_iter(bytes, term.as_bytes()) {
            let line_index = match line_starts.binary_search(&offset) {
                Ok(index) => index,
                Err(index) => index.saturating_sub(1),
            };
            if !reported.insert(line_index as u32) {
                continue;
            }
            let line_start = line_starts[line_index];
            let line_end = line_starts
                .get(line_index + 1)
                .map_or(content.len(), |next| next - 1);
            let raw_line = content[line_start..line_end].trim_end_matches('\r');
            if kind == HitKind::Variant && raw_line.len() > VARIANT_MAX_LINE_BYTES {
                continue;
            }
            let (line, column, line_text) =
                crate::grep_executor::line_details(content, &line_starts, offset);
            hits.push(SweepHit {
                grep: GrepMatch {
                    file: path.to_path_buf(),
                    line,
                    column,
                    line_text,
                    match_text: term.to_string(),
                },
                // A long line is cut around the match so the match stays visible.
                snippet: super::snippet_bounds::window_snippet_line(raw_line, offset - line_start),
                kind,
                class,
                definition: class == FileClass::Source && is_definition_line(raw_line, term),
            });
        }
    };
    if has_identifier {
        push_hits(&terms.identifier, HitKind::Exact, &mut reported_lines);
    }
    for variant in variant_terms {
        push_hits(variant, HitKind::Variant, &mut reported_lines);
    }
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

/// Whether `line` declares `term`: a declaration keyword directly before a
/// whole-word occurrence (`fn name`, `export const name`, `class Name`), or an
/// optional-property declaration (`name?: boolean`).
pub(super) fn is_definition_line(line: &str, term: &str) -> bool {
    let mut search_from = 0;
    while let Some(found) = line[search_from..].find(term) {
        let start = search_from + found;
        let end = start + term.len();
        search_from = start + term.len().max(1);
        let before = &line[..start];
        let after = &line[end..];
        if before.chars().next_back().is_some_and(is_identifier_char)
            || after.chars().next().is_some_and(is_identifier_char)
        {
            continue;
        }
        if after.trim_start().starts_with("?:") {
            return true;
        }
        let previous_word = before
            .trim_end()
            .rsplit(char::is_whitespace)
            .next()
            .unwrap_or_default();
        if DEFINITION_KEYWORDS.contains(&previous_word) {
            return true;
        }
        if search_from >= line.len() {
            break;
        }
    }
    false
}

/// Order: identifier lines before variant-only lines; inside each group,
/// source files holding a definition, other source files, text, then data;
/// then path, definition lines first, then line number. A file's lines stay
/// together so the grouped text rendering keeps this order.
fn rank_hits(hits: &mut [SweepHit], root: &Path) {
    let defining_files: HashSet<(HitKind, PathBuf)> = hits
        .iter()
        .filter(|hit| hit.definition)
        .map(|hit| (hit.kind, hit.grep.file.clone()))
        .collect();
    let group_rank = |hit: &SweepHit| -> u8 {
        match hit.class {
            FileClass::Source if defining_files.contains(&(hit.kind, hit.grep.file.clone())) => 0,
            FileClass::Source => 1,
            FileClass::Text => 2,
            FileClass::Data => 3,
        }
    };
    hits.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| group_rank(left).cmp(&group_rank(right)))
            .then_with(|| {
                relative_bytes(&left.grep.file, root).cmp(relative_bytes(&right.grep.file, root))
            })
            .then_with(|| right.definition.cmp(&left.definition))
            .then_with(|| left.grep.line.cmp(&right.grep.line))
    });
}

/// Render one page of `outcome` for the reply.
pub(super) fn reply(
    outcome: &SweepOutcome,
    terms: &IdentifierTerms,
    root: &Path,
    display_root: &Path,
    offset: usize,
    top_k: usize,
) -> SweepReply {
    let total = outcome.hits.len();
    let page: Vec<&SweepHit> = outcome.hits.iter().skip(offset).take(top_k).collect();
    let shown = page.len();
    let more_available = offset.saturating_add(shown) < total;
    let exact_count = outcome
        .hits
        .iter()
        .filter(|hit| hit.kind == HitKind::Exact)
        .count();
    let mut text = render_hits(&page, display_root);
    text.push_str("\n\n");
    if total == 0 {
        text.push_str(&format!(
            "No searched line contains `{}`{}.\n",
            terms.identifier,
            variant_list(terms)
        ));
    } else if exact_count < total {
        text.push_str(&format!(
            "Lines marked [variant: ...] contain only a spelling variant of `{}`; they are listed after every line containing `{}` itself.\n",
            terms.identifier, terms.identifier
        ));
    }
    text.push_str(&outcome.coverage_line(root));

    let results = page
        .iter()
        .map(|hit| {
            let mut value = super::grep_match_to_json(&hit.grep, hit.kind.result_source());
            // Ranked aft_search results carry an `exact` field saying whether
            // the result holds the query verbatim; these results keep it.
            value["exact"] = serde_json::json!(hit.kind == HitKind::Exact);
            value
        })
        .collect();
    let envelope = if outcome.fully_covered() {
        more_available.then(|| {
            ListEnvelope::new(
                shown,
                Total::Exact(total),
                Unit::Results,
                vec![Reason::Cap],
                crate::list_surfaces::search::SEARCH_NARROW,
            )
        })
    } else {
        let mut causes = vec![Reason::Walk];
        if more_available {
            causes.push(Reason::Cap);
        }
        Some(ListEnvelope::new(
            shown,
            Total::AtLeast(total.max(offset.saturating_add(shown))),
            Unit::Results,
            causes,
            crate::list_surfaces::search::SEARCH_NARROW,
        ))
    };
    let summary = serde_json::json!({
        "identifier": terms.identifier,
        "variants": terms.variants,
        "exact_lines": exact_count,
        "variant_lines": total - exact_count,
        "files_discovered": if outcome.enumeration_stopped {
            serde_json::Value::Null
        } else {
            serde_json::json!(outcome.files_discovered)
        },
        "files_checked": outcome.files_checked,
        "files_with_hits": outcome.files_with_hits(),
        "complete": outcome.fully_covered(),
        "used_index": outcome.used_index,
        "index_answered_files": outcome.index_answered,
        "files_read": outcome.files_read,
        "oversized_files_skipped": outcome.oversized_skipped,
        // The sentence the reply text ends with, saying whether the sweep
        // checked every file under the root or stopped at its time limit; a
        // renderer that draws results from this JSON shows it too.
        "coverage": outcome.coverage_line(root),
    });
    SweepReply {
        results,
        text,
        more_available,
        envelope,
        summary,
    }
}

fn variant_list(terms: &IdentifierTerms) -> String {
    if terms.variants.is_empty() {
        String::new()
    } else {
        format!(" or a spelling variant ({})", terms.variants.join(", "))
    }
}

/// Render hits the way ranked `aft_search` results read: a `path:line`
/// header with an `[exact]` or `[variant: ...]` marker, the matched line
/// indented below it, and a `Found N result(s).` footer. The reply is held to
/// the same byte budget as ranked results.
fn render_hits(page: &[&SweepHit], display_root: &Path) -> String {
    if page.is_empty() {
        return "Found 0 results.".to_string();
    }
    let mut out =
        super::snippet_bounds::BudgetedText::new(super::snippet_bounds::SEARCH_MAX_OUTPUT_BYTES);
    let mut shown = 0usize;
    for hit in page {
        let path = hit
            .grep
            .file
            .strip_prefix(display_root)
            .unwrap_or(&hit.grep.file)
            .display();
        let marker = match hit.kind {
            HitKind::Exact => "[exact]".to_string(),
            HitKind::Variant => format!("[variant: {}]", hit.grep.match_text),
        };
        let separator = if out.is_empty() { "" } else { "\n\n" };
        if !out.push(&format!("{separator}{path}:{} {marker}", hit.grep.line)) {
            break;
        }
        let line = super::snippet_bounds::cap_snippet_line(&hit.snippet);
        if !out.push(&format!("\n      {line}")) {
            break;
        }
        shown += 1;
    }
    let cut = out.is_cut();
    let mut text = out.into_string();
    if cut {
        text.push_str(&format!(
            "\n\n(Output reached the {}-byte limit after {shown} of {} results; the rest were not printed. Lower topK or page with offset.)",
            super::snippet_bounds::SEARCH_MAX_OUTPUT_BYTES,
            page.len()
        ));
    }
    text.push_str(&format!("\n\nFound {} result(s).", page.len()));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTIFIER: &str = "requireCredentialStamps";

    /// Six source files use the identifier (one declares it), a README
    /// mentions it, and captured JSON dumps repeat its sub-tokens on one giant
    /// line without ever spelling it.
    pub(crate) fn identifier_project() -> tempfile::TempDir {
        let project = tempfile::tempdir().expect("project");
        let root = project.path();
        let sources = [
            (
                "src/store/pool.ts",
                "export interface PoolOptions {\n  requireCredentialStamps?: boolean\n}\n",
            ),
            (
                "src/store/mutate.ts",
                "if (options.requireCredentialStamps) {\n  rejectUnstamped()\n}\n",
            ),
            (
                "src/store/runtime.ts",
                "const strict = config.requireCredentialStamps ?? false\n",
            ),
            (
                "src/store/schema.ts",
                "// requireCredentialStamps turns unbound rows into errors\n",
            ),
            (
                "src/store/torn.ts",
                "export function check(o) { return o.requireCredentialStamps }\n",
            ),
            (
                "src/guard.ts",
                "assert(opts.requireCredentialStamps === true)\n",
            ),
        ];
        for (relative, content) in sources {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        std::fs::write(
            root.join("README.md"),
            "Set `requireCredentialStamps` to reject unbound rows.\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/unrelated.ts"),
            "export const credential = 'stamps'\n",
        )
        .unwrap();
        let dumps = root.join("research/evidence/dumps");
        std::fs::create_dir_all(&dumps).unwrap();
        let body = format!(
            "{{\"instructions\":\"{}\"}}",
            "require credential stamps; the Credential store must require Stamps. ".repeat(3_000)
        );
        for index in 0..6 {
            std::fs::write(dumps.join(format!("{index:04}-main.body.json")), &body).unwrap();
        }
        project
    }

    fn terms() -> IdentifierTerms {
        IdentifierTerms::from_query(IDENTIFIER).expect("identifier query")
    }

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn hit_files(outcome: &SweepOutcome, root: &Path) -> Vec<String> {
        outcome
            .hits
            .iter()
            .map(|hit| {
                hit.grep
                    .file
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn identifier_terms_come_from_the_router_identifier_shape() {
        let terms = terms();
        assert_eq!(terms.identifier, IDENTIFIER);
        assert!(terms
            .variants
            .contains(&"require_credential_stamps".to_string()));
        assert!(IdentifierTerms::from_query("how are credential stamps required").is_none());
    }

    #[test]
    fn definition_lines_are_declarations_of_the_whole_word() {
        assert!(is_definition_line(
            "  requireCredentialStamps?: boolean",
            IDENTIFIER
        ));
        assert!(is_definition_line(
            "export function requireCredentialStamps(row) {",
            IDENTIFIER
        ));
        assert!(is_definition_line("pub(crate) fn needle() {}", "needle"));
        assert!(!is_definition_line(
            "if (options.requireCredentialStamps) {",
            IDENTIFIER
        ));
        assert!(!is_definition_line("const x = needle_more", "needle"));
    }

    #[test]
    fn files_are_classified_source_text_or_data() {
        assert_eq!(classify_file(Path::new("src/a.ts")), FileClass::Source);
        assert_eq!(classify_file(Path::new("README.md")), FileClass::Text);
        assert_eq!(classify_file(Path::new("notes.txt")), FileClass::Text);
        assert_eq!(classify_file(Path::new("x.body.json")), FileClass::Data);
        assert_eq!(classify_file(Path::new("run.jsonl")), FileClass::Data);
        assert_eq!(classify_file(Path::new("app.log")), FileClass::Data);
        assert_eq!(classify_file(Path::new("bundle.min.js")), FileClass::Data);
    }

    #[test]
    fn sweep_returns_every_source_use_first_and_no_dump() {
        let project = identifier_project();
        let root = project.path();
        let outcome = sweep(root, &terms(), None, false, far_deadline());
        let files = hit_files(&outcome, root);

        assert_eq!(
            files,
            vec![
                "src/store/pool.ts",
                "src/guard.ts",
                "src/store/mutate.ts",
                "src/store/runtime.ts",
                "src/store/schema.ts",
                "src/store/torn.ts",
                "README.md",
            ],
            "the declaring file leads, then the other source uses, then docs"
        );
        assert!(outcome.hits.iter().all(|hit| hit.kind == HitKind::Exact));
        assert!(outcome.fully_covered());
        assert!(!outcome.used_index);
        assert_eq!(outcome.files_checked, outcome.files_discovered);
    }

    #[test]
    fn variant_lines_follow_every_exact_line_and_never_come_from_data() {
        let project = identifier_project();
        let root = project.path();
        std::fs::write(
            root.join("src/legacy.py"),
            "def require_credential_stamps(row):\n    return True\n",
        )
        .unwrap();
        std::fs::write(
            root.join("research/evidence/dumps/variant.json"),
            "{\"note\":\"require_credential_stamps\"}\n",
        )
        .unwrap();
        let outcome = sweep(root, &terms(), None, false, far_deadline());
        let files = hit_files(&outcome, root);

        let last = outcome.hits.last().expect("hits");
        assert_eq!(last.kind, HitKind::Variant);
        assert_eq!(files.last().unwrap(), "src/legacy.py");
        assert!(outcome.hits[..outcome.hits.len() - 1]
            .iter()
            .all(|hit| hit.kind == HitKind::Exact));
        assert!(!files.iter().any(|file| file.ends_with(".json")));
    }

    #[test]
    fn unchanged_files_are_answered_by_the_index_and_changed_files_are_read() {
        let project = identifier_project();
        let root = std::fs::canonicalize(project.path()).unwrap();
        // Build the index while `src/late.ts` holds no searched term, then add
        // the identifier to it, so the index is stale for that one file.
        let late = root.join("src/late.ts");
        std::fs::write(&late, "export const placeholder = 1\n").unwrap();
        let index = {
            let mut index = SearchIndex::build(&root);
            index.set_ready(true);
            index
        };
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&late, "use(requireCredentialStamps)\n// grown\n").unwrap();

        let outcome = sweep(&root, &terms(), Some(&index), false, far_deadline());
        let files = hit_files(&outcome, &root);

        assert!(files.contains(&"src/late.ts".to_string()), "{files:?}");
        assert_eq!(files.len(), 8, "{files:?}");
        assert!(outcome.used_index);
        assert!(
            outcome.index_answered > 0,
            "unchanged files without the identifier are ruled out by the index"
        );
        assert!(outcome.files_read < outcome.files_checked);
        assert!(outcome.fully_covered());
        let line = outcome.coverage_line(&root);
        assert!(
            line.contains("this project's own AFT index answered"),
            "{line}"
        );
    }

    #[test]
    fn files_are_searched_source_first_then_text_then_data() {
        let root = Path::new("/repo");
        let mut files: Vec<(FileClass, PathBuf)> = [
            "research/dumps/0001.body.json",
            "docs/guide.md",
            "src/z.ts",
            "app.log",
            "lib/a.rs",
            "README.md",
        ]
        .into_iter()
        .map(|relative| {
            let path = root.join(relative);
            (classify_file(&path), path)
        })
        .collect();
        order_for_search(&mut files, root);
        let order: Vec<_> = files
            .iter()
            .map(|(_, path)| {
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(
            order,
            vec![
                "lib/a.rs",
                "src/z.ts",
                "README.md",
                "docs/guide.md",
                "app.log",
                "research/dumps/0001.body.json",
            ]
        );
    }

    #[test]
    fn complete_sweep_says_it_checked_every_file() {
        let project = identifier_project();
        let root = project.path();
        let outcome = sweep(root, &terms(), None, false, far_deadline());
        let line = outcome.coverage_line(root);
        assert!(
            line.starts_with(&format!(
                "exact pass: complete; checked all {} files under ",
                outcome.files_discovered
            )),
            "{line}"
        );
        assert!(!line.contains("use grep"), "{line}");
    }

    #[test]
    fn expired_deadline_reports_unknown_total_and_points_at_grep() {
        let project = identifier_project();
        let root = project.path();
        let outcome = sweep(root, &terms(), None, false, Instant::now());
        assert!(outcome.enumeration_stopped);
        assert!(!outcome.fully_covered());
        let line = outcome.coverage_line(root);
        assert!(
            line.starts_with("exact pass: bounded (0 files, time limit)"),
            "{line}"
        );
        assert!(
            line.contains("total unknown because enumeration stopped"),
            "{line}"
        );
        assert!(line.contains(&root.display().to_string()), "{line}");
        assert!(
            line.ends_with("not fully searched; use grep for an exhaustive check"),
            "{line}"
        );

        let reply = reply(&outcome, &terms(), root, root, 0, 10);
        let envelope = reply
            .envelope
            .expect("a bounded sweep always carries a trailer");
        assert_eq!(envelope.reason, Some(Reason::Walk));
        assert!(envelope.total.is_at_least());
        assert_eq!(reply.summary["complete"], false);
        assert!(reply.summary["files_discovered"].is_null());
    }

    #[test]
    fn stopped_scan_names_checked_and_discovered_counts() {
        let outcome = SweepOutcome {
            files_discovered: 10,
            files_checked: 3,
            files_read: 3,
            scan_stopped: true,
            ..SweepOutcome::default()
        };
        let line = outcome.coverage_line(Path::new("/repo"));
        assert!(line.contains("checked 3 of 10 discovered files"), "{line}");
        assert!(line.contains("use grep for an exhaustive check"), "{line}");
    }

    #[test]
    fn complete_reply_pages_with_an_exact_total() {
        let project = identifier_project();
        let root = project.path();
        let outcome = sweep(root, &terms(), None, false, far_deadline());
        let first = reply(&outcome, &terms(), root, root, 0, 3);
        assert_eq!(first.results.len(), 3);
        assert!(first.more_available);
        let envelope = first.envelope.expect("cut page has a trailer");
        assert_eq!(envelope.total, Total::Exact(7));
        assert_eq!(envelope.reason, Some(Reason::Cap));

        let all = reply(&outcome, &terms(), root, root, 0, 25);
        assert!(
            all.envelope.is_none(),
            "a complete, uncut list has no trailer"
        );
        assert!(all.text.contains("exact pass: complete"), "{}", all.text);
        assert_eq!(all.results[0]["source"], "exact");
    }

    /// The agent-visible text of one `search` tool call, rendered the way each
    /// transport renders it: the standalone NDJSON `tool_call` path formats the
    /// response without a finalizer; the subc path also runs the shared
    /// finalizer, which appends the status bar and checkout notes.
    fn rendered_search_text(
        ctx: &crate::context::AppContext,
        session_root: &Path,
        external_root: &Path,
        query: &str,
        subc: bool,
    ) -> String {
        use crate::run_tool_call::{run_tool_call, ToolCallContext, ToolCallOutcome};

        let args = serde_json::json!({
            "query": query,
            "path": external_root.display().to_string(),
            "topK": 25,
        });
        let format_context =
            crate::subc_format::FormatContext::from_tool_call("search", &args, session_root);
        let tool_ctx = ToolCallContext {
            project_root: session_root.to_path_buf(),
            session_id: Some("external-render".to_string()),
            request_id: "external-render".to_string(),
            diagnostics_on_edit: false,
            preview: false,
            edit_slot_survives: None,
            report_registration_downgrade: false,
            standard_edit_grammar: false,
            disabled_tools: None,
            worker_session: false,
        };
        let dispatch = |request: crate::protocol::RawRequest, app: &crate::context::AppContext| {
            assert_eq!(request.command, "semantic_search");
            crate::commands::semantic_search::handle_semantic_search(&request, app)
        };
        let finalizer = |response: &mut crate::protocol::Response, text: &mut String| {
            crate::response_finalize::finalize_tool_response(
                response,
                text,
                ctx,
                "external-render",
                "search",
                false,
            );
        };
        let finalizer_ref: Option<&crate::run_tool_call::FinalizeFn<'_>> =
            if subc { Some(&finalizer) } else { None };
        match run_tool_call(
            "search",
            args,
            &format_context,
            &tool_ctx,
            ctx,
            &dispatch,
            finalizer_ref,
            None,
        ) {
            ToolCallOutcome::Unary(result) => result.text,
        }
    }

    fn git_init(root: &Path) {
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .expect("git init");
        assert!(status.success());
    }

    fn render_context(session: &Path, storage: &Path) -> crate::context::AppContext {
        crate::context::AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(session.to_path_buf()),
                storage_dir: Some(storage.to_path_buf()),
                ..crate::config::Config::default()
            },
        )
    }

    /// Checks one rendered identifier reply line by line: every result is a
    /// `path:line [marker]` header followed by its matched line, then the
    /// footer and the coverage line, and on the first search of an unopened
    /// project the notice that its semantic search is off. Nothing else may
    /// appear (no status note, no score, no empty snippet, no data file).
    fn assert_clean_identifier_text(text: &str, root: &Path, coverage_source: &str) {
        let root_text = root.display().to_string();
        // Paths render with the platform's separator.
        let shown = |relative: &str| {
            relative
                .split('/')
                .fold(root.to_path_buf(), |path, part| path.join(part))
                .display()
                .to_string()
        };
        let expected_head = format!(
            "{}:2 [exact]\n        requireCredentialStamps?: boolean\n\n{}:1 [exact]\n      assert(opts.requireCredentialStamps === true)\n",
            shown("src/store/pool.ts"),
            shown("src/guard.ts")
        );
        assert!(text.starts_with(&expected_head), "{text}");
        let (results, footer) = text.split_once("\n\nFound 8 result(s).\n\n").expect(text);
        let blocks: Vec<&str> = results.split("\n\n").collect();
        assert_eq!(blocks.len(), 8, "{text}");
        for block in &blocks {
            let (header, snippet) = block.split_once('\n').expect(block);
            let (location, marker) = header.split_once(' ').expect(header);
            let (path, line) = location.rsplit_once(':').expect(location);
            assert!(path.starts_with(&root_text), "absolute path: {header}");
            assert!(Path::new(path).is_file(), "existing path: {header}");
            assert!(line.parse::<u32>().is_ok_and(|line| line >= 1), "{header}");
            assert!(
                marker == "[exact]" || marker == "[variant: require_credential_stamps]",
                "{header}"
            );
            assert!(snippet.starts_with("      "), "{block}");
            assert!(!snippet.trim().is_empty(), "{block}");
            assert!(!snippet.contains('\n'), "one matched line: {block}");
        }
        assert!(
            blocks[7].starts_with(&format!(
                "{}:1 [variant: require_credential_stamps]\n      def require_credential_stamps(row):",
                shown("src/legacy.py")
            )),
            "{text}"
        );
        let footer_lines: Vec<&str> = footer.lines().collect();
        // An identifier `query` keeps the semantic lane, so the first search of
        // an unopened project also carries the once-per-session notice that
        // its semantic search is off.
        let notice = footer_lines
            .get(2)
            .is_some_and(|line| line.starts_with("semantic search is off for "));
        assert_eq!(footer_lines.len(), 2 + usize::from(notice), "{text}");
        assert!(
            footer_lines[0].starts_with("Lines marked [variant: ...]"),
            "{text}"
        );
        assert_eq!(
            footer_lines[1],
            format!(
                "exact pass: complete; checked all 15 files under {root_text} ({coverage_source})"
            )
        );
        // The notice names the user's config file (`aft.jsonc`), so the
        // forbidden-text check reads the reply without it.
        let checked = match notice {
            true => text.replace(footer_lines[2], ""),
            false => text.to_string(),
        };
        for forbidden in [
            "NaN",
            "score",
            "Search status",
            "[interpreted_as",
            ".json",
            "saved AFT index",
        ] {
            assert!(!checked.contains(forbidden), "{forbidden:?} in {text}");
        }
    }

    #[test]
    fn external_identifier_text_reads_cleanly_on_both_transports() {
        let session = tempfile::tempdir().expect("session");
        let storage = tempfile::tempdir().expect("storage");
        let project = identifier_project();
        let root = std::fs::canonicalize(project.path()).unwrap();
        std::fs::write(
            root.join("src/legacy.py"),
            "def require_credential_stamps(row):\n    return True\n",
        )
        .unwrap();
        git_init(&root);
        let ctx = render_context(session.path(), storage.path());
        for subc in [false, true] {
            let text = rendered_search_text(&ctx, session.path(), &root, IDENTIFIER, subc);
            assert_clean_identifier_text(
                &text,
                &root,
                "no AFT index for this project was usable, so files were read from disk",
            );
        }

        let cache_dir = crate::search_index::resolve_cache_dir(&root, Some(storage.path()));
        let mut index = SearchIndex::build(&root);
        index.write_to_disk(&cache_dir, None);
        let ctx = render_context(session.path(), storage.path());
        for subc in [false, true] {
            let text = rendered_search_text(&ctx, session.path(), &root, IDENTIFIER, subc);
            assert_clean_identifier_text(
                &text,
                &root,
                "this project's own AFT index answered 7 unchanged files without reading them and 8 files were read from disk",
            );
        }
    }

    /// A saved index compared with every file on disk ends the reply with one
    /// line saying so; one whose comparison stopped at the time limit keeps
    /// the unchecked-index notice and says how far the comparison got.
    #[test]
    fn saved_index_answers_say_how_far_they_were_checked_on_both_transports() {
        let session = tempfile::tempdir().expect("session");
        let storage = tempfile::tempdir().expect("storage");
        let project = identifier_project();
        let root = std::fs::canonicalize(project.path()).unwrap();
        git_init(&root);
        let cache_dir = crate::search_index::resolve_cache_dir(&root, Some(storage.path()));
        let mut index = SearchIndex::build(&root);
        index.write_to_disk(&cache_dir, None);
        let ctx = render_context(session.path(), storage.path());
        // Later queries in the loop reuse the first check, so the line may
        // also say how long ago it was made ("on disk 0 s ago; ...").
        let checked = format!(
            "Checked the saved AFT index of {} against all {} files on disk",
            root.display(),
            index.file_count()
        );
        let unchecked = format!(
            "Answered from the saved AFT index of {} (saved ",
            root.display()
        );
        let queries = ["how are unbound rows rejected", "\"unbound rows\""];
        for query in queries {
            for subc in [false, true] {
                let text = rendered_search_text(&ctx, session.path(), &root, query, subc);
                assert!(text.contains(&checked), "{query}: {text}");
                assert!(
                    text.contains("; none changed since it was saved."),
                    "{query}: {text}"
                );
                assert!(!text.contains(&unchecked), "{query}: {text}");
                assert!(!text.contains("exact pass:"), "{query}: {text}");
            }
        }
        // A zero walk budget stops every check before its first file; the
        // override also turns reuse off, so each query checks again.
        for query in queries {
            for subc in [false, true] {
                let text = super::super::external_disk_check::with_budgets_for_test(
                    Duration::ZERO,
                    Duration::from_secs(5),
                    || rendered_search_text(&ctx, session.path(), &root, query, subc),
                );
                assert!(text.contains(&unchecked), "{query}: {text}");
                assert!(
                    text.contains(
                        "which was only partly checked against the files on disk: only 0 files on disk were compared with it before the time limit. Text added or changed since it was saved may be missing from the files not checked. Use grep with path for an exhaustive check."
                    ),
                    "{query}: {text}"
                );
                assert!(!text.contains(&checked), "{query}: {text}");
            }
        }
    }

    /// Without any index, a prose query is answered by a scan for its literal
    /// text, and the reply says that in plain words: that no index exists, how
    /// many files the scan read, and that the grep tool gives an exhaustive
    /// check. No status jargon, no index label and no empty list trailer.
    #[test]
    fn unindexed_prose_reply_says_what_was_searched_on_both_transports() {
        let session = tempfile::tempdir().expect("session");
        let storage = tempfile::tempdir().expect("storage");
        let project = identifier_project();
        let root = std::fs::canonicalize(project.path()).unwrap();
        git_init(&root);
        let ctx = render_context(session.path(), storage.path());
        let query = "how are unbound rows rejected";
        let expected = format!(
            "No AFT index exists for {root}, so this query could not be ranked by meaning or keywords; AFT only looked for its exact text \"{query}\" and found no line containing it. It read all 14 text files under {root}. Use grep with path for an exhaustive check, for example on one distinctive word of the query.",
            root = root.display()
        );
        for subc in [false, true] {
            let text = rendered_search_text(&ctx, session.path(), &root, query, subc);
            assert!(text.starts_with(&expected), "{text}");
            for forbidden in [
                "fully degraded",
                "Search status",
                "[index: fallback]",
                "[interpreted_as",
                "shown 0 of",
            ] {
                assert!(!text.contains(forbidden), "{forbidden:?} in {text}");
            }
        }
    }

    #[test]
    #[ignore = "requires an isolated copied corpus in AFT_EXTERNAL_REPRO_ROOT"]
    fn copied_external_identifier_reproduction() {
        use crate::commands::semantic_search::handle_semantic_search;
        use crate::context::AppContext;

        let root = PathBuf::from(std::env::var_os("AFT_EXTERNAL_REPRO_ROOT").expect("corpus"));
        let storage = std::env::var_os("AFT_EXTERNAL_REPRO_STORAGE").map(PathBuf::from);
        let session = tempfile::tempdir().expect("session");
        let ctx = AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(session.path().to_path_buf()),
                storage_dir: storage,
                ..crate::config::Config::default()
            },
        );
        let query =
            std::env::var("AFT_EXTERNAL_REPRO_QUERY").unwrap_or_else(|_| IDENTIFIER.to_string());
        let req: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "external-repro",
            "command": "semantic_search",
            "query": query,
            "top_k": 25,
            "path": root,
        }))
        .unwrap();
        let started = Instant::now();
        let response = serde_json::to_value(handle_semantic_search(&req, &ctx)).unwrap();
        eprintln!("ELAPSED {:?}", started.elapsed());
        eprintln!("TEXT\n{}", response["text"].as_str().unwrap_or_default());
        for key in [
            "semantic_status",
            "complete",
            "exact_sweep",
            "results_list_envelope",
        ] {
            eprintln!("{key}: {}", response[key]);
        }
        for subc in [false, true] {
            let text = rendered_search_text(&ctx, session.path(), &root, &query, subc);
            eprintln!("RENDERED (subc={subc})\n{text}\nEND");
        }
    }
}
