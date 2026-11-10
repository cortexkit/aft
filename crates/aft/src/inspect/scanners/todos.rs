//! TODO markers in comments.
//!
//! The index store finds a file's markers when it parses the file, with the
//! rules here ([`markers_from_tree`], [`markers_lexical`]), and keeps them in
//! the file's content record. Counting them ([`run_todos_scan`]) reads the
//! records of the root's bound checkout and never parses a file: a file
//! whose record is not there yet makes the count pending, and a root with no
//! records at all reports the count unavailable.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use aft_store::content::{TodoMarker, TodoMarkers};
use tree_sitter::{Node, Tree};

use crate::inspect::job::{is_test_tree_file, ExcludedTestTally};
use crate::inspect::{InspectJob, InspectResult, InspectScanSuccess};
use crate::parser::LangId;

const MAX_LINES_PER_FILE: usize = 100_000;
const MAX_ITEMS: usize = 100;
const MAX_TEXT_CHARS: usize = 200;
const MARKERS: [&str; 5] = ["TODO", "FIXME", "HACK", "XXX", "BUG"];

/// The language a file's comments are read as, to find its TODO markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TodoLanguage {
    /// Comments are the comment nodes of the file's syntax tree.
    Tree(LangId),
    /// A C header that no grammar resolves: comments are found by their
    /// opening characters, outside quoted strings.
    Lexical,
}

impl TodoLanguage {
    /// The language's name as records store it: the parser's language in
    /// lower case, or `lexical`.
    pub(crate) fn name(self) -> String {
        match self {
            Self::Tree(language) => format!("{language:?}").to_lowercase(),
            Self::Lexical => "lexical".to_string(),
        }
    }

    /// The language named `name` (see [`Self::name`]).
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        if name == "lexical" {
            return Some(Self::Lexical);
        }
        crate::callgraph_store::join::language_id(name).map(Self::Tree)
    }
}

/// The language the comments of the file at `path` are read as, judged from
/// the path alone; `None` for a file whose TODOs are not counted: plain JSON
/// (which has no comments and is never read), or a file of no known
/// language. A C header's language depends on its text, so this gives C for
/// one; [`header_matches`] tells whether a header was read as it should be.
pub(crate) fn path_todo_language(path: &Path) -> Option<TodoLanguage> {
    if is_plain_json(path) {
        return None;
    }
    crate::parser::language_for_extension(path).map(TodoLanguage::Tree)
}

/// Whether `path` is a C header, whose comments are read as whichever of C,
/// C++ or Objective-C its text parses as, or lexically when none does.
pub(crate) fn is_header(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("h")
}

/// Whether a header's markers read as `language` (a [`TodoLanguage::name`])
/// are what reading it now would give: the language a header resolves to is
/// chosen when it is parsed, so any of the ones it can resolve to is
/// accepted.
fn header_matches(language: &str) -> bool {
    matches!(language, "c" | "cpp" | "objc" | "lexical")
}

/// Plain JSON cannot hold comments, so its TODOs are never counted and the
/// file is never read; JSON with comments uses a comment-capable extension.
fn is_plain_json(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
}

/// `source` as the text whose comments are read, or `None` when the count
/// does not read it: it holds a NUL byte or is not UTF-8.
fn text_of(source: &[u8]) -> Option<&str> {
    if source.contains(&0) {
        return None;
    }
    std::str::from_utf8(source).ok()
}

/// The TODO markers of `source` parsed as `language` to `tree`: the markers
/// opening a line of a comment node (and of an HTML comment in Markdown).
pub(crate) fn markers_from_tree(source: &[u8], language: LangId, tree: &Tree) -> TodoMarkers {
    let items = text_of(source).map(|text| {
        let mut comment_nodes = Vec::new();
        collect_comment_nodes(tree.root_node(), language, text, &mut comment_nodes);
        comment_nodes.sort_by_key(|node| node.start_byte());
        let mut items = Vec::new();
        for node in comment_nodes {
            scan_comment_node(node, text, &mut items);
        }
        items.sort_by_key(|item| item.line);
        items.dedup_by_key(|item| item.line);
        items
    });
    TodoMarkers {
        language: TodoLanguage::Tree(language).name(),
        items,
    }
}

/// The TODO markers of `source` found without a syntax tree, for a header no
/// grammar resolves.
pub(crate) fn markers_lexical(source: &[u8]) -> TodoMarkers {
    let items = text_of(source).map(|text| {
        let mut state = LexicalState::default();
        text.lines()
            .take(MAX_LINES_PER_FILE)
            .enumerate()
            .filter_map(|(index, line)| scan_lexical_line(line, index + 1, &mut state))
            .collect()
    });
    TodoMarkers {
        language: TodoLanguage::Lexical.name(),
        items,
    }
}

/// What the count found for one file of its scope.
#[derive(Debug)]
pub(crate) enum FileTodos {
    /// Not read: plain JSON, or content that is not text.
    NotScanned,
    /// Read, with these markers.
    Scanned(Vec<TodoMarker>),
    /// Not read, for this agent-visible reason.
    Skipped(&'static str),
}

/// What the count found for its scope from the index store.
#[derive(Debug)]
pub(crate) enum StoredTodos {
    /// Each scope file's markers, in scope order, and the repositories of a
    /// folder of repositories that have no checkout yet (relative to the
    /// folder), whose files the count leaves out.
    Files {
        files: Vec<FileTodos>,
        unindexed: Vec<String>,
    },
    /// Some file has no record yet: the store is building or catching up.
    Pending,
    /// The root has no records at all, for this agent-visible reason.
    Unavailable(&'static str),
}

/// What a file's stored markers make of it in the count: `language` is the
/// one its comments were read as, which must be the one its path calls for.
pub(crate) fn file_todos(path: &Path, todos: TodoMarkers) -> Option<FileTodos> {
    let expected = if is_header(path) {
        header_matches(&todos.language)
    } else {
        path_todo_language(path).is_some_and(|language| language.name() == todos.language)
    };
    if !expected {
        return None;
    }
    Some(match todos.items {
        Some(items) => FileTodos::Scanned(items),
        None => FileTodos::NotScanned,
    })
}

pub fn run_todos_scan(job: &InspectJob) -> InspectResult {
    let started = Instant::now();
    let cancellation = crate::executor::current_job_cancellation();
    let cancelled = || {
        !job.project_root.is_dir()
            || cancellation
                .as_ref()
                .is_some_and(|token| token.cancel_requested_before_commit())
    };
    if cancelled() {
        return InspectResult::failed(
            job,
            "TODO scan cancelled: root unbound or missing",
            started.elapsed(),
        );
    }
    let stored = match crate::inspect::data_sources::stored_todos(
        &job.project_root,
        &job.config,
        &job.scope_files,
    ) {
        Ok(stored) => stored,
        Err(error) => return InspectResult::failed(job, error, started.elapsed()),
    };
    #[cfg(test)]
    note_outcome_for_test(&job.project_root, &stored);
    if cancelled() {
        return InspectResult::failed(
            job,
            "TODO scan cancelled: root unbound or missing",
            started.elapsed(),
        );
    }
    let aggregate = match stored {
        StoredTodos::Files { files, unindexed } => {
            let mut success = aggregate(job, files);
            write_unindexed_gap(&mut success.aggregate, &unindexed);
            return InspectResult::success(job, success, started.elapsed());
        }
        // As dead code reports a call graph still building: the records
        // come with the call-graph build.
        StoredTodos::Pending => serde_json::json!({
            "status": "pending", "reason": "callgraph_building", "complete": false,
        }),
        StoredTodos::Unavailable(reason) => serde_json::json!({
            "unavailable": true,
            "complete": false,
            "gaps": [{"kind": "todo_markers_unavailable", "reason": reason}],
        }),
    };
    InspectResult::success(
        job,
        InspectScanSuccess {
            scanned_files: Vec::new(),
            contributions: Vec::new(),
            aggregate,
        },
        started.elapsed(),
    )
}

/// How many repositories with no checkout yet a gap's reason names; the
/// gap's `repositories` lists them all.
const UNINDEXED_NAMED: usize = 5;

/// Marks `aggregate` partial when some repositories of a folder of
/// repositories (`unindexed`, relative to the folder) have no checkout yet:
/// their TODOs are not counted, and the gap names them.
fn write_unindexed_gap(aggregate: &mut serde_json::Value, unindexed: &[String]) {
    if unindexed.is_empty() {
        return;
    }
    let mut names = unindexed
        .iter()
        .take(UNINDEXED_NAMED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if unindexed.len() > UNINDEXED_NAMED {
        names.push_str(&format!(", and {} more", unindexed.len() - UNINDEXED_NAMED));
    }
    let reason = format!(
        "{}: {names}",
        crate::inspect::data_sources::TODOS_REPOSITORIES_NOT_INDEXED
    );
    aggregate["complete"] = serde_json::Value::Bool(false);
    let gap = serde_json::json!({
        "kind": "analysis_incomplete",
        "reason": reason,
        "repositories": unindexed,
    });
    match aggregate["gaps"].as_array_mut() {
        Some(gaps) => gaps.push(gap),
        None => aggregate["gaps"] = serde_json::json!([gap]),
    }
}

/// The count's result for `files`, each the stored markers of the scope
/// file at the same position.
fn aggregate(job: &InspectJob, files: Vec<FileTodos>) -> InspectScanSuccess {
    let mut scanned_files = Vec::new();
    let mut skipped_files = Vec::new();
    let mut all_items = Vec::new();
    let mut excluded = ExcludedTestTally::default();
    for (path, todos) in job.scope_files.iter().zip(files) {
        let items = match todos {
            FileTodos::NotScanned => continue,
            FileTodos::Skipped(reason) => {
                skipped_files.push((
                    display_file_path(&job.project_root, path),
                    reason.to_string(),
                ));
                continue;
            }
            FileTodos::Scanned(items) => items,
        };
        scanned_files.push(path.clone());
        let file = display_file_path(&job.project_root, path);
        // Markers in test trees (generated fixtures, mutation notes in test
        // comments) are not product work items; tally them separately.
        for item in items {
            if is_test_tree_file(&file) {
                excluded.record(&file);
            } else {
                all_items.push((file.clone(), item));
            }
        }
    }

    let mut by_kind = BTreeMap::new();
    for marker in MARKERS {
        by_kind.insert(marker.to_string(), 0usize);
    }
    for (_, item) in &all_items {
        if let Some(count) = by_kind.get_mut(item.marker.as_str()) {
            *count += 1;
        }
    }

    let total_count = all_items.len();
    let drill_down_capped = total_count > MAX_ITEMS;
    let items = all_items
        .into_iter()
        .take(MAX_ITEMS)
        .map(|(file, item)| {
            serde_json::json!({
                "file": file,
                "line": item.line,
                "marker": item.marker,
                "author": item.author,
                "text": item.text,
            })
        })
        .collect::<Vec<_>>();

    let mut aggregate = serde_json::json!({
        "count": total_count,
        "by_kind": by_kind,
        "items": items,
        "drill_down_capped": drill_down_capped,
    });
    excluded.write_into(&mut aggregate);
    super::source_text::write_skipped_files(&mut aggregate, skipped_files, None);
    InspectScanSuccess {
        scanned_files,
        contributions: Vec::new(),
        aggregate,
    }
}

fn collect_comment_nodes<'tree>(
    root: Node<'tree>,
    language: LangId,
    source: &str,
    comments: &mut Vec<Node<'tree>>,
) {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if is_comment_node(node, language, source) {
            comments.push(node);
            continue;
        }

        let mut children = Vec::new();
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                children.push(cursor.node());
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        pending.extend(children.into_iter().rev());
    }
}

fn is_comment_node(node: Node<'_>, language: LangId, source: &str) -> bool {
    let kind = node.kind();
    if kind == "comment" || kind.ends_with("_comment") {
        return true;
    }

    language == LangId::Markdown
        && matches!(kind, "html_block" | "html_inline")
        && node
            .utf8_text(source.as_bytes())
            .is_ok_and(|text| text.trim_start().starts_with("<!--"))
}

fn scan_comment_node(node: Node<'_>, source: &str, items: &mut Vec<TodoMarker>) {
    let Ok(comment) = node.utf8_text(source.as_bytes()) else {
        return;
    };
    let first_line = node.start_position().row + 1;
    let mut in_block_comment = false;
    for (offset, line) in comment.lines().enumerate() {
        let line_number = first_line + offset;
        if line_number > MAX_LINES_PER_FILE {
            break;
        }
        if let Some(item) = scan_line(line, line_number, &mut in_block_comment) {
            items.push(item);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct QuoteState {
    delimiter: u8,
    raw_hashes: Option<usize>,
}

#[derive(Debug, Default)]
struct LexicalState {
    quote: Option<QuoteState>,
    block_closer: Option<&'static str>,
}

fn scan_lexical_line(
    line: &str,
    line_number: usize,
    state: &mut LexicalState,
) -> Option<TodoMarker> {
    let mut cursor = 0usize;
    while cursor < line.len() {
        if let Some(closer) = state.block_closer {
            let closer_offset = line[cursor..].find(closer);
            let body_end = closer_offset
                .map(|offset| cursor + offset)
                .unwrap_or(line.len());
            let body = strip_block_comment_prefix(&line[cursor..body_end]);
            let item = parse_todo_body(body, line_number);
            let Some(closer_offset) = closer_offset else {
                return item;
            };
            state.block_closer = None;
            cursor += closer_offset + closer.len();
            if item.is_some() {
                return item;
            }
            continue;
        }

        if let Some(quote) = state.quote {
            cursor = advance_quoted_cursor(line, cursor, quote, state);
            continue;
        }

        if let Some((quote, after_opener)) = raw_quote_start(line, cursor) {
            state.quote = Some(quote);
            cursor = after_opener;
            continue;
        }

        let byte = line.as_bytes()[cursor];
        if matches!(byte, b'\'' | b'"' | b'`') {
            state.quote = Some(QuoteState {
                delimiter: byte,
                raw_hashes: None,
            });
            cursor += 1;
            continue;
        }

        let Some((prefix, closer)) = comment_prefix_at(line, cursor) else {
            cursor += next_char_len(line, cursor);
            continue;
        };
        if !is_comment_prefix_boundary(line, cursor, prefix) {
            cursor += prefix.len();
            continue;
        }

        let body_start = cursor + prefix.len();
        if let Some(closer) = closer {
            let closer_offset = line[body_start..].find(closer);
            let body_end = closer_offset
                .map(|offset| body_start + offset)
                .unwrap_or(line.len());
            let body = strip_block_comment_prefix(&line[body_start..body_end]);
            let item = parse_todo_body(body, line_number);
            if let Some(offset) = closer_offset {
                cursor = body_start + offset + closer.len();
            } else {
                state.block_closer = Some(closer);
                return item;
            }
            if item.is_some() {
                return item;
            }
            continue;
        }

        return parse_todo_body(line[body_start..].trim_start(), line_number);
    }

    None
}

fn advance_quoted_cursor(
    line: &str,
    cursor: usize,
    quote: QuoteState,
    state: &mut LexicalState,
) -> usize {
    let byte = line.as_bytes()[cursor];
    if quote.raw_hashes.is_none() && byte == b'\\' {
        let escaped = cursor + 1;
        return escaped + next_char_len(line, escaped);
    }
    if byte != quote.delimiter {
        return cursor + next_char_len(line, cursor);
    }

    let hashes = quote.raw_hashes.unwrap_or_default();
    let after_quote = cursor + 1;
    let hashes_match = line.as_bytes().get(after_quote..).is_some_and(|rest| {
        rest.len() >= hashes && rest[..hashes].iter().all(|byte| *byte == b'#')
    });
    if hashes_match {
        state.quote = None;
        after_quote + hashes
    } else {
        cursor + 1
    }
}

fn raw_quote_start(line: &str, cursor: usize) -> Option<(QuoteState, usize)> {
    if line.as_bytes().get(cursor) != Some(&b'r') {
        return None;
    }

    let mut opener_end = cursor + 1;
    while line.as_bytes().get(opener_end) == Some(&b'#') {
        opener_end += 1;
    }
    if line.as_bytes().get(opener_end) != Some(&b'"') {
        return None;
    }

    Some((
        QuoteState {
            delimiter: b'"',
            raw_hashes: Some(opener_end - cursor - 1),
        },
        opener_end + 1,
    ))
}

fn comment_prefix_at(line: &str, cursor: usize) -> Option<(&'static str, Option<&'static str>)> {
    let rest = &line[cursor..];
    if rest.starts_with("<!--") {
        Some(("<!--", Some("-->")))
    } else if rest.starts_with("//") {
        Some(("//", None))
    } else if rest.starts_with("/*") {
        Some(("/*", Some("*/")))
    } else if rest.starts_with('#') {
        Some(("#", None))
    } else if rest.starts_with("--") {
        Some(("--", None))
    } else {
        None
    }
}

fn next_char_len(line: &str, cursor: usize) -> usize {
    line.get(cursor..)
        .and_then(|rest| rest.chars().next())
        .map(char::len_utf8)
        .unwrap_or_default()
}

fn scan_line(line: &str, line_number: usize, in_block_comment: &mut bool) -> Option<TodoMarker> {
    if *in_block_comment {
        let item = parse_todo_body(strip_block_comment_prefix(line), line_number);
        if line.contains("*/") {
            *in_block_comment = false;
        }
        if item.is_some() {
            return item;
        }
    }

    let mut search_start = 0usize;
    let mut found_item = None;
    while search_start < line.len() {
        let Some(prefix_match) = find_next_comment_prefix(line, search_start) else {
            break;
        };
        let body = &line[prefix_match.body_start..];
        if prefix_match.starts_block_comment && !body.contains("*/") {
            *in_block_comment = true;
        }
        let body = if prefix_match.starts_block_comment {
            strip_block_comment_prefix(body)
        } else {
            body.trim_start()
        };
        if let Some(item) = parse_todo_body(body, line_number) {
            found_item = Some(item);
            break;
        }
        search_start = prefix_match.body_start;
    }

    found_item
}

#[derive(Debug, Clone, Copy)]
struct CommentPrefixMatch {
    body_start: usize,
    starts_block_comment: bool,
}

fn find_next_comment_prefix(line: &str, search_start: usize) -> Option<CommentPrefixMatch> {
    let prefixes = ["<!--", "//", "/*", "#", "--"];
    prefixes
        .iter()
        .filter_map(|prefix| find_prefix_match(line, search_start, prefix))
        .min_by_key(|prefix_match| prefix_match.body_start)
}

fn find_prefix_match(line: &str, search_start: usize, prefix: &str) -> Option<CommentPrefixMatch> {
    let mut cursor = search_start;
    while cursor < line.len() {
        let offset = line[cursor..].find(prefix)?;
        let prefix_start = cursor + offset;
        if is_comment_prefix_boundary(line, prefix_start, prefix) {
            return Some(CommentPrefixMatch {
                body_start: prefix_start + prefix.len(),
                starts_block_comment: prefix == "/*",
            });
        }
        cursor = prefix_start + prefix.len();
    }
    None
}

fn is_comment_prefix_boundary(line: &str, prefix_start: usize, prefix: &str) -> bool {
    if prefix == "<!--" {
        return true;
    }
    prefix_start == 0
        || line[..prefix_start]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
}

fn strip_block_comment_prefix(body: &str) -> &str {
    let mut trimmed = body.trim_start();
    while let Some(rest) = trimmed.strip_prefix('*') {
        trimmed = rest.trim_start();
    }
    trimmed
}

fn parse_todo_body(body: &str, line_number: usize) -> Option<TodoMarker> {
    let body = body.trim_start();
    for marker in MARKERS {
        let Some(rest) = body.strip_prefix(marker) else {
            continue;
        };
        let Some((author, text_start)) = parse_marker_suffix(marker, rest) else {
            continue;
        };
        return Some(TodoMarker {
            // Lines past MAX_LINES_PER_FILE are never scanned.
            line: u32::try_from(line_number).unwrap_or(u32::MAX),
            marker: marker.to_string(),
            author,
            text: truncate_text(strip_comment_closer(text_start)),
        });
    }
    None
}

fn parse_marker_suffix<'a>(
    marker: &'static str,
    rest: &'a str,
) -> Option<(Option<String>, &'a str)> {
    if rest.is_empty() {
        return Some((None, rest));
    }

    let trimmed = rest.trim_start();
    if let Some(after_colon) = trimmed.strip_prefix(':') {
        return Some((None, after_colon.trim_start()));
    }
    if let Some(after_author_start) = trimmed.strip_prefix('(') {
        if !matches!(marker, "TODO" | "FIXME") {
            return None;
        }
        let author_end = after_author_start.find(')')?;
        let author = after_author_start[..author_end].trim();
        if author.is_empty() {
            return None;
        }
        let after_author = &after_author_start[author_end + 1..];
        let after_author = after_author.trim_start();
        let text_start = after_author
            .strip_prefix(':')
            .map(str::trim_start)
            .unwrap_or(after_author);
        return Some((Some(author.to_string()), text_start));
    }
    if rest.chars().next().is_some_and(char::is_whitespace) {
        return Some((None, rest.trim_start()));
    }
    if rest.starts_with("*/") || rest.starts_with("-->") {
        return Some((None, rest));
    }
    None
}

fn strip_comment_closer(text: &str) -> &str {
    let mut trimmed = text.trim();
    loop {
        let without_closer = trimmed
            .strip_suffix("*/")
            .or_else(|| trimmed.strip_suffix("-->"));
        let Some(next) = without_closer else {
            break;
        };
        trimmed = next.trim_end();
    }
    trimmed
}

fn truncate_text(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

fn display_file_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// What each count of a root found, in order, for tests that follow the
/// counts a background refresh takes.
#[cfg(test)]
static OUTCOMES_FOR_TEST: std::sync::Mutex<Vec<(std::path::PathBuf, &'static str)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn note_outcome_for_test(root: &Path, stored: &StoredTodos) {
    let outcome = match stored {
        StoredTodos::Files { .. } => "counted",
        StoredTodos::Pending => "pending",
        StoredTodos::Unavailable(_) => "unavailable",
    };
    OUTCOMES_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((root.to_path_buf(), outcome));
}

/// The outcomes of the counts of `root` so far: `counted`, `pending` or
/// `unavailable`.
#[cfg(test)]
pub(crate) fn outcomes_for_test(root: &Path) -> Vec<&'static str> {
    OUTCOMES_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(held, _)| held == root)
        .map(|(_, outcome)| *outcome)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    use super::*;
    use crate::config::Config;
    use crate::inspect::{InspectCategory, JobKey, JobScope};
    use crate::parser::SymbolCache;

    /// The markers extraction records for `source` at `path`, as the index
    /// store's records hold them.
    fn markers(path: &str, source: &str) -> TodoMarkers {
        let language = crate::data_layer::content::language_for(path, source.as_bytes())
            .expect("a language the store records");
        let mut record =
            crate::data_layer::content::extract(source.as_bytes(), &language).expect("extraction");
        crate::data_layer::content::add_config_markers(&mut record, path, source.as_bytes())
            .expect("configuration markers");
        record.todos.expect("the record holds TODO markers")
    }

    fn lines(todos: &TodoMarkers) -> Vec<(u32, String, String)> {
        todos
            .items
            .as_ref()
            .expect("text")
            .iter()
            .map(|item| (item.line, item.marker.clone(), item.text.clone()))
            .collect()
    }

    /// Every marker must open the comment; an upper-case marker word inside
    /// prose or a string is not a work item.
    #[test]
    fn markers_open_comments_and_strings_never_count() {
        let todos = markers(
            "src/lib.rs",
            "// BUG: leading marker counts\n\
             // this documents a BUG in prose\n\
             // the TODO list and the FIXME tag and a HACK and XXX are prose here\n\
             fn f() { let s = \"// TODO: not a comment\"; }\n\
             /* FIXME(ana): block */\n",
        );
        assert_eq!(todos.language, "rust");
        assert_eq!(
            lines(&todos),
            vec![
                (1, "BUG".into(), "leading marker counts".into()),
                (5, "FIXME".into(), "block".into()),
            ]
        );
        assert_eq!(
            todos.items.as_ref().unwrap()[1].author.as_deref(),
            Some("ana")
        );
    }

    /// Markdown, minified bundles, resolver configuration and headers no
    /// grammar resolves are not parsed by the call graph, and still hold
    /// their markers, read as today's count read them.
    #[test]
    fn files_the_call_graph_skips_still_hold_their_markers() {
        let markdown = markers(
            "docs/notes.md",
            "# TODO heading is not a comment\n<!-- TODO: real comment -->\n```\n<!-- FIXME: in a fence -->\n```\n",
        );
        assert_eq!(markdown.language, "markdown");
        assert_eq!(
            lines(&markdown),
            vec![(2, "TODO".into(), "real comment".into())]
        );

        let minified = format!("/* HACK: shim */{}\n", "var a=1;".repeat(400));
        let bundle = markers("dist-like/app.min.js", &minified);
        assert_eq!(bundle.language, "javascript");
        assert_eq!(lines(&bundle), vec![(1, "HACK".into(), "shim".into())]);

        let cargo = markers("Cargo.toml", "[package]\n# TODO: publish\nname = \"x\"\n");
        assert_eq!(cargo.language, "toml");
        assert_eq!(lines(&cargo), vec![(2, "TODO".into(), "publish".into())]);

        let not_text = crate::inspect::scanners::todos::markers_lexical(b"// TODO: a\0b\n");
        assert_eq!(not_text.items, None);
        let lexical = markers_lexical(b"x = \"// TODO: quoted\"\n# XXX: real\n");
        assert_eq!(lexical.language, "lexical");
        assert_eq!(lines(&lexical), vec![(2, "XXX".into(), "real".into())]);
    }

    fn todos_job(root: &Path, scope_files: Vec<PathBuf>) -> InspectJob {
        let scope = JobScope::for_project(root.to_path_buf());
        InspectJob {
            job_id: 1,
            key: JobKey::for_category_scope(InspectCategory::Todos, &scope),
            category: InspectCategory::Todos,
            scope_files,
            project_root: root.to_path_buf(),
            inspect_dir: root.join("inspect"),
            config: Arc::new(Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            }),
            symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
            inspect_writer: true,
            callgraph_writer: true,
            callgraph_snapshot: None,
        }
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// After a cold start the status bar's TODO refresh can run while the
    /// index store is still building, and come back pending. When the
    /// store publishes, its status listener asks for the refresh again, so
    /// the count arrives with no edit and no `aft_inspect` call.
    #[test]
    fn a_count_pending_while_the_store_builds_is_taken_again_when_it_publishes() {
        use crate::inspect::tier2_scheduler::{
            TIER2_REFRESH_COLD_CACHE_DELAY, TIER2_REFRESH_MIN_INTERVAL,
        };
        use crate::inspect::Tier2TriggerReason;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        fs::create_dir_all(dir.path().join("project")).unwrap();
        fs::create_dir_all(&storage).unwrap();
        // Canonicalize only once the folder exists: the scan keys its outcome by
        // the job's resolved root, which differs from the spelled temp path
        // where the temp folder sits behind a link (/var on macOS) or a short
        // name (Windows).
        let checkout_root = fs::canonicalize(dir.path().join("project")).unwrap();
        let root = crate::path_identity::canonicalize_normalized(&checkout_root);
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()
            .unwrap()
            .success());
        write(
            &root,
            "src/app.rs",
            "// TODO: counted once the store publishes\nfn app() {}\n",
        );
        // The checkout retains configure's canonical path, including Windows's
        // `\\?\` prefix. The build hold must use that exact key, not inspect's
        // normalized outcome key, or the build can publish before we release it.
        let hold = crate::data_layer::runtime::hold_builds_for_test(&checkout_root);
        // A refresh that finds the store building answers pending at once.
        crate::inspect::data_sources::set_todo_store_wait_for_test(&root, Duration::ZERO);

        let ctx = Arc::new(crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.clone()),
                ..Config::default()
            },
        ));
        ctx.bind_status_context();
        let doc = serde_json::json!({"indexes": {"trigram": false, "semantic": false, "callgraph": true}});
        let configure: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "configure",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "storage_dir": storage,
            "config": [{"tier": "user", "source": "/u/aft.jsonc", "doc": doc.to_string()}],
        }))
        .unwrap();
        let response = crate::commands::configure::handle_configure(&configure, &ctx);
        assert!(response.success, "configure failed: {response:?}");
        crate::runtime_drain::standalone_root_configured(&ctx);
        assert_eq!(
            ctx.data_checkout()
                .expect("the index store binds the root")
                .root(),
            checkout_root,
            "the build hold must name the bound checkout"
        );

        // The warm-up refresh runs while the build is held.
        let configured_at = Instant::now()
            .checked_sub(TIER2_REFRESH_COLD_CACHE_DELAY + Duration::from_secs(1))
            .unwrap();
        ctx.reset_tier2_refresh_scheduler_at(configured_at);
        let now = Instant::now();
        assert_eq!(
            ctx.tick_tier2_refresh_scheduler_at(now, 0),
            Some(Tier2TriggerReason::ConfigureWarm)
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        // The outcome is noted inside the scan, before its completion is queued.
        // Drain the completion too, so an in-flight job cannot hide a stray pull.
        while outcomes_for_test(&root).is_empty() || ctx.inspect_manager().tier2_any_in_flight() {
            assert!(Instant::now() < deadline, "the warm-up refresh never ran");
            crate::runtime_drain::drain_inspect_events(&ctx);
            std::thread::sleep(Duration::from_millis(20));
        }
        crate::runtime_drain::drain_inspect_events(&ctx);
        assert_eq!(outcomes_for_test(&root), ["pending"]);
        assert_eq!(ctx.status_bar_count_values().todos, None);
        // Nothing else asks for a refresh: no edit, no inspect call.
        let later = now + TIER2_REFRESH_MIN_INTERVAL;
        assert_eq!(ctx.tick_tier2_refresh_scheduler_at(later, 0), None);

        // The store builds and publishes; its status listener asks for the
        // refresh, which the next maintenance tick starts.
        drop(hold);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match ctx.tick_tier2_refresh_scheduler_at(later, 0) {
                Some(reason) => {
                    assert_eq!(reason, Tier2TriggerReason::Pull);
                    break;
                }
                None => {
                    assert!(Instant::now() < deadline, "no refresh was asked for");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        while ctx.status_bar_count_values().todos != Some(1) {
            assert!(
                Instant::now() < deadline,
                "the count never arrived: {:?}",
                outcomes_for_test(&root)
            );
            crate::runtime_drain::drain_inspect_events(&ctx);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(outcomes_for_test(&root), ["pending", "counted"]);
        // Asked once for the change, not on every later status change.
        assert_eq!(
            ctx.tick_tier2_refresh_scheduler_at(later + TIER2_REFRESH_MIN_INTERVAL, 0),
            None
        );
    }

    /// A root with no checkout bound, and no word that it never gets one,
    /// is pending: it never reports zero TODOs.
    #[test]
    fn a_root_without_records_is_pending_never_zero() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::path_identity::canonicalize_normalized(dir.path());
        write(&root, "src/lib.rs", "// TODO: unseen\n");
        let result = run_todos_scan(&todos_job(&root, vec![root.join("src/lib.rs")]));
        let aggregate = result.outcome.unwrap().aggregate;
        assert_eq!(aggregate["status"], "pending", "{aggregate:#}");
        assert!(aggregate.get("count").is_none(), "{aggregate:#}");

        crate::inspect::data_sources::note_root(
            &root,
            crate::inspect::data_sources::RootKind::Withheld,
        );
        let result = run_todos_scan(&todos_job(&root, vec![root.join("src/lib.rs")]));
        let aggregate = result.outcome.unwrap().aggregate;
        assert_eq!(aggregate["unavailable"], true, "{aggregate:#}");
        assert_eq!(
            aggregate["gaps"][0]["reason"],
            crate::inspect::data_sources::TODOS_NO_INDEX_STORE
        );
    }

    /// Counting the TODOs of a bound, published checkout reads them from the
    /// store's records: it opens no source file and parses nothing. After
    /// edits that add, change and remove markers, the count follows, and
    /// only the edited files are extracted, once each, by the store.
    #[test]
    fn counting_a_bound_checkout_opens_and_parses_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::path_identity::canonicalize_normalized(dir.path());
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()
            .unwrap()
            .success());
        let app = "// TODO: first unparsed marker\nfn app() {}\n";
        let notes = "# Notes\n<!-- FIXME: unparsed markdown marker -->\n";
        let cargo = "[package]\n# HACK: unparsed config marker\nname = \"todo-fixture\"\nversion = \"0.1.0\"\n";
        write(&root, "src/app.rs", app);
        write(&root, "src/plain.rs", "fn plain() {}\n");
        write(&root, "docs/notes.md", notes);
        write(&root, "Cargo.toml", cargo);
        write(&root, "data.json", "{\"note\": \"// TODO: never read\"}\n");
        write(&root, "tests/fixture.rs", "// TODO: test-tree marker\n");
        write(&root, "src/huge.rs", &"// filler\n".repeat(120_000));

        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                project_root: Some(root.clone()),
                ..Config::default()
            },
        );
        crate::data_layer::surfaces::serve_text_index_for_test(&ctx);
        let checkout = ctx.data_checkout().expect("bound");
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        let view = loop {
            let view = checkout.view().expect("published");
            if view.status().callgraph_published && !checkout.checkout().status().building {
                break view;
            }
            assert!(Instant::now() < deadline, "the call graph never published");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let scope = crate::inspect::inventory::InspectInventory::from_checkout(&root, &view)
            .expect("the view stands in for a walk")
            .sources;
        let parses =
            |source: &str| crate::data_layer::content::parses_of_for_test(source.as_bytes());
        let parsed_before = [app, notes, cargo].map(parses);

        let probe = crate::project_content::Probe::new(&root);
        let job = todos_job(&root, scope.clone());
        let aggregate = run_todos_scan(&job).outcome.unwrap().aggregate;
        assert!(probe.paths().is_empty(), "opened {:?}", probe.paths());
        assert_eq!([app, notes, cargo].map(parses), parsed_before);
        assert_eq!(aggregate["count"], 3, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["TODO"], 1, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["FIXME"], 1, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["HACK"], 1, "{aggregate:#}");
        assert_eq!(aggregate["excluded_test_count"], 1, "{aggregate:#}");
        let files = aggregate["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["file"].as_str().unwrap(),
                    item["line"].as_u64().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            files,
            vec![("Cargo.toml", 2), ("docs/notes.md", 2), ("src/app.rs", 1)]
        );
        assert_eq!(
            aggregate["skipped_files"],
            serde_json::json!([{"file": "src/huge.rs", "reason": "over the 1 MiB size limit"}])
        );

        // Add, change and remove markers.
        let plain = "fn plain() {}\n// XXX: added marker\n";
        let changed_app = "fn app() {}\n// TODO: moved and reworded\n";
        write(&root, "src/plain.rs", plain);
        write(&root, "src/app.rs", changed_app);
        write(&root, "docs/notes.md", "# Notes\n");
        checkout.note_paths(&[
            root.join("src/plain.rs"),
            root.join("src/app.rs"),
            root.join("docs/notes.md"),
        ]);
        // Recording the edits read the files; the count must read none.
        drop(probe);
        let probe = crate::project_content::Probe::new(&root);
        let view = checkout.view().unwrap();
        let scope = crate::inspect::inventory::InspectInventory::from_checkout(&root, &view)
            .expect("the view stands in for a walk")
            .sources;
        let aggregate = run_todos_scan(&todos_job(&root, scope))
            .outcome
            .unwrap()
            .aggregate;
        assert!(probe.paths().is_empty(), "opened {:?}", probe.paths());
        assert_eq!(aggregate["count"], 3, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["FIXME"], 0, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["XXX"], 1, "{aggregate:#}");
        let items = aggregate["items"].as_array().unwrap();
        assert_eq!(items[1]["file"], "src/app.rs");
        assert_eq!(items[1]["line"], 2);
        assert_eq!(items[1]["text"], "moved and reworded");
        // The edited files were extracted once, for their changes.
        assert_eq!(parses(plain), 1);
        assert_eq!(parses(changed_app), 1);

        // Publish the changes, then fold the publication's segments into
        // one: the fold copies the stored records, markers and all, so the
        // count reads the same from the folded segment, still opening and
        // parsing nothing.
        checkout.checkout().refresh_callgraph().unwrap();
        checkout.checkout().publish_changes().unwrap();
        let crate::data_layer::checkout::Folded::Done(folded) = checkout.checkout().fold().unwrap()
        else {
            panic!("the fold did not finish");
        };
        assert!(folded.read_before > 1, "{folded:?}");
        assert!(folded.retired_segments > 0, "{folded:?}");
        let view = checkout.view().unwrap();
        let scope = crate::inspect::inventory::InspectInventory::from_checkout(&root, &view)
            .expect("the view stands in for a walk")
            .sources;
        let parsed_before_count = [plain, changed_app].map(parses);
        let folded_aggregate = run_todos_scan(&todos_job(&root, scope))
            .outcome
            .unwrap()
            .aggregate;
        assert!(probe.paths().is_empty(), "opened {:?}", probe.paths());
        assert_eq!(folded_aggregate, aggregate);
        assert_eq!([plain, changed_app].map(parses), parsed_before_count);
    }
}
