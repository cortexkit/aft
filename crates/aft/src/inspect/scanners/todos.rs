use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
#[cfg(all(test, debug_assertions))]
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Instant, UNIX_EPOCH};

use rayon::prelude::*;
use tree_sitter::{Node, Parser, Tree};

use crate::cache_freshness::{self, FileFreshness};
use crate::inspect::cache::Tier1FileMemo;
use crate::inspect::job::{is_test_tree_file, ExcludedTestTally};
use crate::inspect::{InspectJob, InspectResult, InspectScanSuccess};
use crate::parser::{detect_language, grammar_for, LangId};

const MAX_LINES_PER_FILE: usize = 100_000;
const MAX_ITEMS: usize = 100;
const MAX_TEXT_CHARS: usize = 200;
const MARKERS: [&str; 5] = ["TODO", "FIXME", "HACK", "XXX", "BUG"];

static TODOS_MEMO: OnceLock<Tier1FileMemo<FileScan>> = OnceLock::new();

thread_local! {
    static TODOS_PARSERS: RefCell<HashMap<LangId, Parser>> = RefCell::new(HashMap::new());
}

#[cfg(debug_assertions)]
static FILE_READS: super::read_counts::FileReadCounts = super::read_counts::FileReadCounts::new();

#[derive(Debug, Clone)]
struct TodoItem {
    file: String,
    line: usize,
    marker: &'static str,
    author: Option<String>,
    text: String,
}

#[derive(Debug, Clone)]
struct FileScan {
    scanned_file: Option<PathBuf>,
    items: Vec<TodoItem>,
}

pub fn run_todos_scan(job: &InspectJob) -> InspectResult {
    run_todos_scan_with_memo(job, todos_memo())
}

fn run_todos_scan_with_memo(job: &InspectJob, memo: &Tier1FileMemo<FileScan>) -> InspectResult {
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
    memo.reserve_for_scan(job.scope_files.len());
    let per_file: Vec<FileScan> = job
        .scope_files
        .par_iter()
        .filter_map(|path| {
            let _cancel = cancellation
                .clone()
                .map(crate::executor::install_job_cancellation);
            if cancelled() {
                return None;
            }
            // Overlapping project roots can scan the same source file. Keep
            // TODO paths root-independent in the file memo; aggregation below
            // formats them relative to the root making this request.
            Some(memo.get_or_insert_with(path, |path| scan_file(path, Path::new(""))))
        })
        .collect();
    if cancelled() {
        return InspectResult::failed(
            job,
            "TODO scan cancelled: root unbound or missing",
            started.elapsed(),
        );
    }
    if job.is_full_project_scope() {
        memo.prune_to_scope(&job.project_root, &job.scope_files);
    }

    let mut scanned_files = Vec::new();
    let mut all_items = Vec::new();
    let mut excluded = ExcludedTestTally::default();
    for scan in per_file {
        let file = scan
            .scanned_file
            .as_ref()
            .map(|path| display_file_path(&job.project_root, path));
        if let Some(path) = scan.scanned_file {
            scanned_files.push(path);
        }
        // Markers in test trees (generated fixtures, mutation notes in test
        // comments) are not product work items; tally them separately.
        for mut item in scan.items {
            if let Some(file) = &file {
                item.file.clone_from(file);
            }
            if is_test_tree_file(&item.file) {
                excluded.record(&item.file);
            } else {
                all_items.push(item);
            }
        }
    }

    let mut by_kind = BTreeMap::new();
    for marker in MARKERS {
        by_kind.insert(marker.to_string(), 0usize);
    }
    for item in &all_items {
        if let Some(count) = by_kind.get_mut(item.marker) {
            *count += 1;
        }
    }

    let total_count = all_items.len();
    let drill_down_capped = total_count > MAX_ITEMS;
    let items = all_items
        .into_iter()
        .take(MAX_ITEMS)
        .map(|item| {
            serde_json::json!({
                "file": item.file,
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
    let success = InspectScanSuccess {
        scanned_files,
        contributions: Vec::new(),
        aggregate,
    };
    InspectResult::success(job, success, started.elapsed())
}

fn todos_memo() -> &'static Tier1FileMemo<FileScan> {
    TODOS_MEMO.get_or_init(Tier1FileMemo::default)
}

fn scan_file(path: &Path, project_root: &Path) -> (Option<FileFreshness>, FileScan) {
    let (freshness, source) = read_text_file(path);
    let Some(source) = source else {
        return (
            freshness,
            FileScan {
                scanned_file: None,
                items: Vec::new(),
            },
        );
    };

    let file = display_file_path(project_root, path);
    let items = match detect_language(path) {
        Some(language) => scan_parser_comments(path, language, &source, &file),
        None => scan_lexical_comments(&source, &file),
    };

    (
        freshness,
        FileScan {
            scanned_file: Some(path.to_path_buf()),
            items,
        },
    )
}

fn scan_parser_comments(path: &Path, language: LangId, source: &str, file: &str) -> Vec<TodoItem> {
    let Some(tree) = parse_source(path, language, source) else {
        return Vec::new();
    };

    let mut comment_nodes = Vec::new();
    collect_comment_nodes(tree.root_node(), language, source, &mut comment_nodes);
    comment_nodes.sort_by_key(|node| node.start_byte());

    let mut items = Vec::new();
    for node in comment_nodes {
        scan_comment_node(node, source, file, &mut items);
    }
    items.sort_by_key(|item| item.line);
    items.dedup_by_key(|item| item.line);
    items
}

#[cfg(all(test, debug_assertions))]
type ParseDelay = (
    crossbeam_channel::Sender<()>,
    crossbeam_channel::Receiver<()>,
);

#[cfg(all(test, debug_assertions))]
static PARSE_DELAYS: OnceLock<Mutex<HashMap<PathBuf, ParseDelay>>> = OnceLock::new();

fn parse_source(path: &Path, language: LangId, source: &str) -> Option<Tree> {
    #[cfg(all(test, debug_assertions))]
    {
        let delay = PARSE_DELAYS
            .get()
            .and_then(|delays| delays.lock().unwrap().remove(path));
        if let Some((entered, release)) = delay {
            entered.send(()).expect("TODO file reached parser");
            release
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("release delayed TODO parser");
        }
    }
    TODOS_PARSERS.with(|parsers| {
        let mut parsers = parsers.borrow_mut();
        if let std::collections::hash_map::Entry::Vacant(entry) = parsers.entry(language) {
            let mut parser = Parser::new();
            if parser.set_language(&grammar_for(language)).is_err() {
                return None;
            }
            entry.insert(parser);
        }

        let parser = parsers
            .get_mut(&language)
            .expect("parser inserted for language");
        if crate::executor::current_job_cancelled() || !path.is_file() {
            parser.reset();
            return None;
        }
        // Progress callbacks interrupt large generated files without reparsing
        // them. Throttle filesystem checks so ordinary parsing stays CPU-bound.
        let mut last_check = Instant::now();
        let mut progress = |_: &tree_sitter::ParseState| {
            if last_check.elapsed() >= std::time::Duration::from_millis(100) {
                last_check = Instant::now();
                if crate::executor::current_job_cancelled() || !path.is_file() {
                    return std::ops::ControlFlow::Break(());
                }
            }
            std::ops::ControlFlow::Continue(())
        };
        let bytes = source.as_bytes();
        let tree = parser.parse_with_options(
            &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
            None,
            Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
        );
        if tree.is_none() {
            // A cancelled parser keeps resumable state; the next file must not
            // accidentally resume the abandoned source buffer.
            parser.reset();
        }
        tree
    })
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

fn scan_comment_node(node: Node<'_>, source: &str, file: &str, items: &mut Vec<TodoItem>) {
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
        if let Some(item) = scan_line(line, line_number, file, &mut in_block_comment) {
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

fn scan_lexical_comments(source: &str, file: &str) -> Vec<TodoItem> {
    let mut state = LexicalState::default();
    source
        .lines()
        .take(MAX_LINES_PER_FILE)
        .enumerate()
        .filter_map(|(line_index, line)| scan_lexical_line(line, line_index + 1, file, &mut state))
        .collect()
}

fn scan_lexical_line(
    line: &str,
    line_number: usize,
    file: &str,
    state: &mut LexicalState,
) -> Option<TodoItem> {
    let mut cursor = 0usize;
    while cursor < line.len() {
        if let Some(closer) = state.block_closer {
            let closer_offset = line[cursor..].find(closer);
            let body_end = closer_offset
                .map(|offset| cursor + offset)
                .unwrap_or(line.len());
            let body = strip_block_comment_prefix(&line[cursor..body_end]);
            let item = parse_todo_body(body, line_number, file);
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
            let item = parse_todo_body(body, line_number, file);
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

        return parse_todo_body(line[body_start..].trim_start(), line_number, file);
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

fn read_text_file(path: &Path) -> (Option<FileFreshness>, Option<String>) {
    let metadata = std::fs::metadata(path).ok();
    #[cfg(debug_assertions)]
    bump_file_read_count(path);
    let bytes = crate::project_content::read(path).ok();
    let freshness = metadata
        .as_ref()
        .map(|metadata| freshness_from_metadata(metadata, bytes.as_deref()));

    let Some(bytes) = bytes else {
        return (freshness, None);
    };
    if bytes.contains(&0) {
        return (freshness, None);
    }
    (freshness, String::from_utf8(bytes).ok())
}

fn freshness_from_metadata(metadata: &std::fs::Metadata, bytes: Option<&[u8]>) -> FileFreshness {
    let size = metadata.len();
    let content_hash = if size <= cache_freshness::CONTENT_HASH_SIZE_CAP {
        bytes
            .map(cache_freshness::hash_bytes)
            .unwrap_or_else(cache_freshness::zero_hash)
    } else {
        cache_freshness::zero_hash()
    };

    FileFreshness {
        mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
        size,
        content_hash,
    }
}

fn display_file_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn scan_line(
    line: &str,
    line_number: usize,
    file: &str,
    in_block_comment: &mut bool,
) -> Option<TodoItem> {
    if *in_block_comment {
        let item = parse_todo_body(strip_block_comment_prefix(line), line_number, file);
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
        if let Some(item) = parse_todo_body(body, line_number, file) {
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

fn parse_todo_body(body: &str, line_number: usize, file: &str) -> Option<TodoItem> {
    let body = body.trim_start();
    for marker in MARKERS {
        let Some(rest) = body.strip_prefix(marker) else {
            continue;
        };
        let Some((author, text_start)) = parse_marker_suffix(marker, rest) else {
            continue;
        };
        return Some(TodoItem {
            file: file.to_string(),
            line: line_number,
            marker,
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
#[cfg(debug_assertions)]
fn bump_file_read_count(path: &Path) {
    FILE_READS.record(path);
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn reset_file_read_count_for_debug(project_root: &Path) {
    // Normalized like the scanned paths themselves: bare canonicalize is
    // verbatim on Windows and the starts_with filter would miss every entry.
    let project_root = crate::inspect::job::canonicalize_normalized(project_root);
    FILE_READS.reset(project_root);
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn file_read_count_for_debug(project_root: &Path) -> usize {
    // Normalized like the scanned paths themselves: bare canonicalize is
    // verbatim on Windows and the starts_with filter would miss every entry.
    let project_root = crate::inspect::job::canonicalize_normalized(project_root);
    FILE_READS.count(&project_root)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use std::fs;
    use std::sync::{Arc, RwLock};

    use super::*;

    #[test]
    fn audit05_todo_read_tracking_is_root_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.rs");
        fs::write(&path, "// TODO: marker\n").unwrap();
        let _ = read_text_file(&path);
        assert_eq!(file_read_count_for_debug(dir.path()), 0);
        reset_file_read_count_for_debug(dir.path());
        let _ = read_text_file(&path);
        assert_eq!(file_read_count_for_debug(dir.path()), 1);
    }
    use crate::config::Config;
    use crate::inspect::{InspectCategory, JobKey, JobScope};
    use crate::parser::SymbolCache;

    const OVER_DEFAULT_MEMO_CAPACITY: usize = 4_097;

    #[test]
    fn audit05_memo_paths_are_relative_to_each_root() {
        let dir = tempfile::tempdir().expect("project");
        let parent = dir.path();
        let nested = parent.join("nested");
        fs::create_dir_all(nested.join("src")).expect("test directory");
        let file = nested.join("src/task.rs");
        fs::write(&file, "// TODO: examine marker\n").expect("source");
        let memo = Tier1FileMemo::default();
        let first = run_todos_scan_with_memo(&todos_job(parent, vec![file.clone()]), &memo);
        let second = run_todos_scan_with_memo(&todos_job(&nested, vec![file]), &memo);
        let first = first.outcome.expect("parent scan").aggregate;
        let second = second.outcome.expect("nested scan").aggregate;
        assert_eq!(first["items"][0]["file"], "nested/src/task.rs");
        assert_eq!(second["items"][0]["file"], "src/task.rs");
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

    #[test]
    fn quiesced_root_aborts_real_executor_todos_scan_mid_parse() {
        executor_todos_abandonment(false, false);
    }

    #[test]
    fn deleted_root_aborts_real_executor_todos_scan_mid_parse() {
        executor_todos_abandonment(true, false);
    }

    #[test]
    fn quiesced_root_rebound_within_grace_completes_real_executor_todos_scan() {
        executor_todos_abandonment(false, true);
    }

    fn executor_todos_abandonment(delete_root: bool, rebind: bool) {
        use crate::context::AppContext;
        use crate::executor::{Executor, Lane};
        use crate::parser::TreeSitterProvider;
        use crate::path_identity::ProjectRootId;
        use crate::protocol::Response;
        use std::time::Duration;

        let directory = tempfile::tempdir().expect("TODO project root");
        let storage = tempfile::tempdir().expect("isolated TODO storage");
        let root = ProjectRootId::from_path(directory.path()).expect("canonical TODO root");
        let path = root.as_path().join("lib.rs");
        fs::write(&path, "// TODO: retain this marker\npub fn target() {}\n")
            .expect("write TODO fixture");
        let job = todos_job(root.as_path(), vec![path.clone()]);
        let ctx = Arc::new(AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.as_path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        ));
        ctx.set_unbound_build_abandon_grace_for_test(if rebind {
            Duration::from_secs(600)
        } else {
            Duration::ZERO
        });
        ctx.mark_subc_bound();
        let executor = Executor::new();
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        PARSE_DELAYS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(path, (entered_tx, release_rx));
        let (result_tx, result_rx) = crossbeam_channel::bounded(1);
        let running = executor.submit_maintenance_async(
            root.clone(),
            Lane::HeavyInit,
            "delayed-todos-scan".to_owned(),
            Box::new(move |_| {
                // Run the real scanner and its Rayon file work, not a closure
                // that merely asserts the executor token was cancelled.
                let result = run_todos_scan(&job);
                result_tx.send(result).unwrap();
                Response::success(
                    "delayed-todos-scan",
                    serde_json::json!({ "scan_exited": true }),
                )
            }),
        );
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("real TODO scan read a file and reached its parser");
        if delete_root {
            directory.close().expect("delete TODO root mid-scan");
        } else {
            ctx.mark_subc_unbound();
            executor.cancel_queued_root_maintenance(&root);
            if rebind {
                ctx.mark_subc_bound();
            }
        }
        release_tx.send(()).expect("release TODO parser delay");
        let result = result_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("TODO scan terminates after abandonment");
        if rebind {
            assert!(
                result.outcome.is_ok(),
                "short unbind must preserve running TODO scan: {:?}",
                result.outcome
            );
        } else {
            assert!(
                matches!(result.outcome, Err(ref error) if error.contains("cancelled")),
                "abandoned TODO scan must not report a fresh aggregate: {:?}",
                result.outcome
            );
        }
        let response = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), running)
                    .await
                    .expect("executor releases TODO scan slot")
                    .expect("TODO executor completion")
            });
        assert!(
            response.success,
            "TODO scanner panicked: {:?}",
            response.data
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !executor.actor_is_idle(&root) {
            assert!(
                Instant::now() < deadline,
                "TODO scan retained executor slot"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn full_scope_larger_than_default_capacity_stays_warm() {
        let project = tempfile::tempdir().expect("project");
        let mut files = Vec::with_capacity(OVER_DEFAULT_MEMO_CAPACITY);
        for index in 0..OVER_DEFAULT_MEMO_CAPACITY {
            let path = project.path().join(format!("file-{index:04}.rs"));
            fs::write(
                &path,
                format!("pub fn value_{index}() -> usize {{ {index} }}\n"),
            )
            .expect("write fixture");
            files.push(path);
        }
        let job = todos_job(project.path(), files);
        let memo = Tier1FileMemo::default();

        assert!(run_todos_scan_with_memo(&job, &memo).outcome.is_ok());
        reset_file_read_count_for_debug(project.path());
        assert!(run_todos_scan_with_memo(&job, &memo).outcome.is_ok());

        assert_eq!(
            file_read_count_for_debug(project.path()),
            0,
            "an unchanged full scan must retain every live file above the default memo capacity"
        );
    }

    fn scan_files(files: &[(&str, &str)]) -> serde_json::Value {
        let project = tempfile::tempdir().expect("project");
        let paths = files
            .iter()
            .map(|(relative, contents)| {
                let path = project.path().join(relative);
                fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
                fs::write(&path, contents).expect("write fixture");
                path
            })
            .collect::<Vec<_>>();
        let job = todos_job(project.path(), paths);
        run_todos_scan_with_memo(&job, &Tier1FileMemo::default())
            .outcome
            .expect("todos scan succeeds")
            .aggregate
    }

    #[test]
    fn markers_in_test_trees_are_excluded_from_the_product_count() {
        let aggregate = scan_files(&[
            (
                "src/app.ts",
                "// TODO: product work item\nexport const a = 1;\n",
            ),
            (
                "tests/docker/scenarios/edit/fixture/bulk/bulk.ts",
                "// TODO: fixture item 0\n// TODO: fixture item 1\n",
            ),
            (
                "crates/app/tests/mutation.rs",
                "fn f() -> bool {\n    true // BUG: deliberate mutant\n}\n",
            ),
        ]);

        assert_eq!(aggregate["count"], 1, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["TODO"], 1, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["BUG"], 0, "{aggregate:#}");
        assert_eq!(aggregate["excluded_test_count"], 3, "{aggregate:#}");
        assert_eq!(aggregate["excluded_test_files"], 2, "{aggregate:#}");
        let files = aggregate["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|item| item["file"].as_str().expect("file"))
            .collect::<Vec<_>>();
        assert_eq!(files, vec!["src/app.ts"]);
    }

    /// Every marker must open the comment; an upper-case marker word inside
    /// prose is not a work item. The rule is the same for all five markers.
    #[test]
    fn marker_words_inside_prose_do_not_count() {
        let aggregate = scan_files(&[(
            "src/lib.rs",
            "// BUG: leading marker counts\n\
             // this documents a BUG in prose\n\
             // the TODO list and the FIXME tag and a HACK and XXX are prose here\n\
             fn f() {}\n",
        )]);

        assert_eq!(aggregate["count"], 1, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["BUG"], 1, "{aggregate:#}");
        assert_eq!(aggregate["excluded_test_count"], 0, "{aggregate:#}");
    }
}
