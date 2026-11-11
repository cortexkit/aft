//! TODO marker extraction from syntax-tree comments and lexical headers.

use crate::parser::LangId;
use aft_store::content::{TodoMarker, TodoMarkers};
use std::path::Path;
use tree_sitter::{Node, Tree};

pub(crate) const MAX_LINES_PER_FILE: usize = 100_000;
pub(crate) const MAX_TEXT_CHARS: usize = 200;
pub(crate) const MARKERS: [&str; 5] = ["TODO", "FIXME", "HACK", "XXX", "BUG"];

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
pub(crate) fn header_matches(language: &str) -> bool {
    matches!(language, "c" | "cpp" | "objc" | "lexical")
}

/// Plain JSON cannot hold comments, so its TODOs are never counted and the
/// file is never read; JSON with comments uses a comment-capable extension.
pub(crate) fn is_plain_json(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
}

/// `source` as the text whose comments are read, or `None` when the count
/// does not read it: it holds a NUL byte or is not UTF-8.
pub(crate) fn text_of(source: &[u8]) -> Option<&str> {
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
pub(crate) fn collect_comment_nodes<'tree>(
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

pub(crate) fn is_comment_node(node: Node<'_>, language: LangId, source: &str) -> bool {
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

pub(crate) fn scan_comment_node(node: Node<'_>, source: &str, items: &mut Vec<TodoMarker>) {
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
pub(crate) struct QuoteState {
    delimiter: u8,
    raw_hashes: Option<usize>,
}

#[derive(Debug, Default)]
pub(crate) struct LexicalState {
    quote: Option<QuoteState>,
    block_closer: Option<&'static str>,
}

pub(crate) fn scan_lexical_line(
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

pub(crate) fn advance_quoted_cursor(
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

pub(crate) fn raw_quote_start(line: &str, cursor: usize) -> Option<(QuoteState, usize)> {
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

pub(crate) fn comment_prefix_at(
    line: &str,
    cursor: usize,
) -> Option<(&'static str, Option<&'static str>)> {
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

pub(crate) fn next_char_len(line: &str, cursor: usize) -> usize {
    line.get(cursor..)
        .and_then(|rest| rest.chars().next())
        .map(char::len_utf8)
        .unwrap_or_default()
}

pub(crate) fn scan_line(
    line: &str,
    line_number: usize,
    in_block_comment: &mut bool,
) -> Option<TodoMarker> {
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
pub(crate) struct CommentPrefixMatch {
    body_start: usize,
    starts_block_comment: bool,
}

pub(crate) fn find_next_comment_prefix(
    line: &str,
    search_start: usize,
) -> Option<CommentPrefixMatch> {
    let prefixes = ["<!--", "//", "/*", "#", "--"];
    prefixes
        .iter()
        .filter_map(|prefix| find_prefix_match(line, search_start, prefix))
        .min_by_key(|prefix_match| prefix_match.body_start)
}

pub(crate) fn find_prefix_match(
    line: &str,
    search_start: usize,
    prefix: &str,
) -> Option<CommentPrefixMatch> {
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

pub(crate) fn is_comment_prefix_boundary(line: &str, prefix_start: usize, prefix: &str) -> bool {
    if prefix == "<!--" {
        return true;
    }
    prefix_start == 0
        || line[..prefix_start]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
}

pub(crate) fn strip_block_comment_prefix(body: &str) -> &str {
    let mut trimmed = body.trim_start();
    while let Some(rest) = trimmed.strip_prefix('*') {
        trimmed = rest.trim_start();
    }
    trimmed
}

pub(crate) fn parse_todo_body(body: &str, line_number: usize) -> Option<TodoMarker> {
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

pub(crate) fn parse_marker_suffix<'a>(
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

pub(crate) fn strip_comment_closer(text: &str) -> &str {
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

pub(crate) fn truncate_text(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}
