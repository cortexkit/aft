//! Handler for the `organize_imports` command: re-group, sort, deduplicate, and
//! optionally merge imports in a file.
//!
//! For all languages: extracts imports, groups by convention, sorts alphabetically
//! within groups, deduplicates, and regenerates the import block with blank-line
//! separators between groups.
//!
//! For Rust: merges separate `use` declarations sharing a common prefix into
//! `use` trees (e.g. `use std::path::Path;` + `use std::path::PathBuf;` →
//! `use std::path::{Path, PathBuf};`). This implements D045's deferred merging.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::path::Path;

use crate::context::AppContext;
use crate::edit;
use crate::imports::{self, ImportForm, ImportGroup, ImportKind, ImportStatement};
use crate::parser::{detect_language, LangId};
use crate::protocol::{RawRequest, Response};

/// Handle an `organize_imports` request.
///
/// Params:
///   - `file` (string, required) — target file path
///
/// Returns: `{ file, groups: [{name, count}], removed_duplicates, syntax_valid?, backup_id? }`
pub fn handle_organize_imports(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "organize_imports: missing required param 'file'",
            );
        }
    };

    // --- Validate ---
    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("organize_imports: file not found: {}", file),
        );
    }

    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "unsupported_language",
                format!(
                    "organize_imports: unsupported file extension: {}",
                    path.extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("<none>")
                ),
            );
        }
    };

    if !imports::is_supported(lang) {
        return Response::error(
            &req.id,
            "unsupported_language",
            format!(
                "organize_imports: import management not yet supported for {:?}",
                lang
            ),
        );
    }

    // --- Parse file and imports ---
    let (source, _tree, block) = match imports::parse_file_imports(&path, lang) {
        Ok(result) => result,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    if lang == LangId::Vue {
        if let Err(err) = imports::vue_single_script_content_range(&_tree) {
            return Response::error(&req.id, err.code(), err.message("organize_imports"));
        }
    }

    if block.imports.is_empty() {
        log::debug!("organize_imports: {} (no imports)", file);
        return Response::success(
            &req.id,
            serde_json::json!({
                "file": file,
                "groups": [],
                "removed_duplicates": 0,
                "no_op": true,
            }),
        );
    }

    let spans_multiple_regions = if lang == LangId::Go {
        go_import_declarations_span_multiple_code_regions(&source, &_tree)
    } else {
        imports_span_multiple_code_regions(&source, lang, &block.imports)
    };

    if spans_multiple_regions {
        return Response::error_with_data(
            &req.id,
            "multi_region_imports",
            format!(
                "organize_imports: imports in {file} span multiple code regions; refusing to organize because replacing the combined import range would corrupt intervening code"
            ),
            serde_json::json!({ "file": file }),
        );
    }

    if lang == LangId::Go && go_grouped_import_block_has_comments(&_tree) {
        return Response::error_with_data(
            &req.id,
            "unsupported_import_comments",
            format!(
                "organize_imports: Go grouped import block in {file} contains comments; refusing to organize because regrouping would drop or detach them"
            ),
            serde_json::json!({ "file": file }),
        );
    }

    if lang == LangId::Rust && rust_use_declarations_have_outer_attributes(&_tree) {
        return Response::error_with_data(
            &req.id,
            "unsupported_import_attributes",
            format!(
                "organize_imports: Rust use declaration in {file} has an outer attribute; refusing to organize because sorting or merging could attach the attribute to a different import"
            ),
            serde_json::json!({ "file": file }),
        );
    }

    // --- Auto-backup ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "organize_imports: pre-edit backup",
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // --- Organize: group, sort, dedup ---
    let original_count = block.imports.len();
    let comments = collect_import_comments(&source, lang, &block.imports);
    let comment_gaps = comments
        .unattached_gaps
        .iter()
        .any(|gap| import_gap_has_comment(&source, lang, gap.clone()));
    let (mut grouped, mut removed_duplicates) = organize(&block.imports, lang);

    // --- Generate new import block ---
    let grouped_go_range = if matches!(lang, LangId::Go) {
        imports::go_has_grouped_import(&source, &_tree)
    } else {
        None
    };
    let go_import_declarations_range = if matches!(lang, LangId::Go) && grouped_go_range.is_some() {
        imports::go_import_declarations_range(&source, &_tree)
    } else {
        None
    };
    let new_import_text = if matches!(lang, LangId::Go) && grouped_go_range.is_some() {
        generate_go_grouped_block(&grouped)
    } else if comment_gaps {
        let (preserved_grouped, preserved_removed, preserved_text) =
            organize_preserving_comment_gaps(&source, lang, &block.imports, &comments);
        grouped = preserved_grouped;
        removed_duplicates = preserved_removed;
        preserved_text
    } else {
        generate_organized_block(&grouped, lang, &comments.attached)
    };

    // --- Replace import region ---
    let mut import_range = match go_import_declarations_range
        .as_ref()
        .or(grouped_go_range.as_ref())
        .or(block.byte_range.as_ref())
    {
        Some(range) => range.clone(),
        None => {
            return Response::error(
                &req.id,
                "parse_error",
                format!(
                    "organize_imports: missing import byte range for {} despite parsed imports",
                    file
                ),
            );
        }
    };
    // The regenerated block already carries the last import's same-line
    // comment, so the original copy after the block must be replaced too.
    // Go's grouped rewrite does not emit attached comments; leave its range.
    if grouped_go_range.is_none() {
        if let Some(end) = comments.block_end {
            import_range.end = import_range.end.max(end);
        }
    }
    let new_source = format!(
        "{}{}{}",
        &source[..import_range.start],
        new_import_text,
        &source[import_range.end..],
    );

    // --- Write, format, and validate ---
    let _mutation = crate::runtime_drain::record_file_mutation(ctx, [path.as_path()]);
    let tool_policy = &crate::sandbox_spawn::ToolLaunchPolicy::for_request(ctx);
    let mut write_result = match edit::write_format_validate(
        tool_policy,
        &path,
        &new_source,
        &ctx.config(),
        &req.params,
    ) {
        Ok(r) => r,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    if let Ok(final_content) = std::fs::read_to_string(&path) {
        write_result.lsp_outcome = ctx.lsp_post_write(&path, &final_content, &req.params);
    }

    // A rollback means post-write syntax validation failed and the file was
    // restored — imports were NOT reorganized. Report that honestly with an
    // error instead of claiming `organized: true`.
    if write_result.rolled_back {
        return Response::error(
            &req.id,
            "generated_invalid_syntax",
            format!(
                "organize_imports: reorganizing imports in {file} would produce invalid syntax; file left unchanged"
            ),
        );
    }

    log::debug!("organize_imports: {}", file);

    // --- Build response ---
    let groups_info: Vec<serde_json::Value> = grouped
        .iter()
        .map(|(group, imps)| {
            serde_json::json!({
                "name": group.label(),
                "count": imps.len(),
            })
        })
        .collect();

    let _ = original_count; // used for removed_duplicates calculation above

    let mut result = serde_json::json!({
        "file": file,
        "groups": groups_info,
        "removed_duplicates": removed_duplicates,
        "formatted": write_result.formatted,
    });

    if let Some(valid) = write_result.syntax_valid {
        result["syntax_valid"] = serde_json::json!(valid);
    }

    if let Some(ref reason) = write_result.format_skipped_reason {
        result["format_skipped_reason"] = serde_json::json!(reason);
    }

    if write_result.validate_requested {
        result["validation_errors"] = serde_json::json!(write_result.validation_errors);
    }
    if let Some(ref reason) = write_result.validate_skipped_reason {
        result["validate_skipped_reason"] = serde_json::json!(reason);
    }

    if let Some(ref id) = backup_id {
        result["backup_id"] = serde_json::json!(id);
    }

    write_result.append_lsp_diagnostics_to(&mut result);
    write_result.append_reformatted_excerpt_to(&mut result);
    Response::success(&req.id, result)
}

fn go_import_declarations_span_multiple_code_regions(
    source: &str,
    tree: &tree_sitter::Tree,
) -> bool {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            if node.kind() == "import_declaration" {
                ranges.push(node.byte_range());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    ranges.windows(2).any(|pair| {
        let previous = &pair[0];
        let next = &pair[1];
        previous.end > next.start
            || !import_gap_is_trivia(source, LangId::Go, previous.end..next.start)
    })
}

fn go_grouped_import_block_has_comments(tree: &tree_sitter::Tree) -> bool {
    let root = tree.root_node();
    let mut cursor = root.walk();
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            if node.kind() == "import_declaration"
                && go_import_declaration_is_grouped(&node)
                && node_contains_comment(node)
            {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    false
}

fn go_import_declaration_is_grouped(node: &tree_sitter::Node<'_>) -> bool {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().kind() == "import_spec_list" {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    false
}

fn node_contains_comment(node: tree_sitter::Node<'_>) -> bool {
    if node.kind() == "comment" {
        return true;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if node_contains_comment(cursor.node()) {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    false
}

fn rust_use_declarations_have_outer_attributes(tree: &tree_sitter::Tree) -> bool {
    let root = tree.root_node();
    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return false;
    }

    let mut pending_attribute = false;
    loop {
        let node = cursor.node();
        match node.kind() {
            "attribute_item" => pending_attribute = true,
            "use_declaration" => {
                if pending_attribute || node_contains_kind(node, "attribute_item") {
                    return true;
                }
                pending_attribute = false;
            }
            _ if node.is_extra() => {}
            _ => pending_attribute = false,
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    false
}

fn node_contains_kind(node: tree_sitter::Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == kind || node_contains_kind(child, kind) {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    false
}

pub(crate) fn imports_span_multiple_code_regions(
    source: &str,
    lang: LangId,
    imports: &[ImportStatement],
) -> bool {
    imports.windows(2).any(|pair| {
        let previous = &pair[0];
        let next = &pair[1];
        if previous.byte_range.end > next.byte_range.start {
            return true;
        }

        !import_gap_is_trivia(source, lang, previous.byte_range.end..next.byte_range.start)
    })
}

#[derive(Debug, Clone, Copy)]
struct ImportGapTrivia {
    has_comment: bool,
}

fn import_gap_is_trivia(source: &str, lang: LangId, range: Range<usize>) -> bool {
    scan_import_gap(source, lang, range).is_some()
}

fn import_gap_has_comment(source: &str, lang: LangId, range: Range<usize>) -> bool {
    scan_import_gap(source, lang, range)
        .map(|gap| gap.has_comment)
        .unwrap_or(false)
}

fn scan_import_gap(source: &str, lang: LangId, range: Range<usize>) -> Option<ImportGapTrivia> {
    let gap = source.get(range)?;

    let mut offset = 0;
    let mut has_comment = false;
    while offset < gap.len() {
        let rest = &gap[offset..];
        let ch = rest
            .chars()
            .next()
            .expect("offset is within the trivia gap");

        if ch.is_whitespace() {
            offset += ch.len_utf8();
            continue;
        }

        match comment_len_at(rest, lang) {
            CommentAt::Comment(len) => {
                offset += len;
                has_comment = true;
            }
            CommentAt::Unterminated | CommentAt::NotComment => return None,
        }
    }

    Some(ImportGapTrivia { has_comment })
}

/// Whether `rest` starts with a comment in `lang`, and how long it is.
enum CommentAt {
    /// A complete comment of this many bytes. Line comments stop before the
    /// newline.
    Comment(usize),
    /// A block comment opener with no closer.
    Unterminated,
    NotComment,
}

/// For a Lua long comment opener at the start of `rest` (`--[[`, `--[=[`,
/// `--[==[`, ...), the closer it needs: `]]`, `]=]`, `]==]`, ... with the same
/// number of `=`. Lua ends a long comment only at the matching closer, so
/// `--[==[` is not a line comment and `]]` does not close it.
fn lua_long_comment_closer(rest: &str) -> Option<String> {
    let after_open = rest.strip_prefix("--[")?;
    let level = after_open.bytes().take_while(|byte| *byte == b'=').count();
    after_open[level..]
        .starts_with('[')
        .then(|| format!("]{}]", "=".repeat(level)))
}

fn comment_len_at(rest: &str, lang: LangId) -> CommentAt {
    let block = |opener: &str, closer: &str| match rest[opener.len()..].find(closer) {
        Some(end) => CommentAt::Comment(opener.len() + end + closer.len()),
        None => CommentAt::Unterminated,
    };
    if lang == LangId::Lua {
        if let Some(closer) = lua_long_comment_closer(rest) {
            let opener_len = closer.len() + 2;
            return match rest[opener_len..].find(&closer) {
                Some(end) => CommentAt::Comment(opener_len + end + closer.len()),
                None => CommentAt::Unterminated,
            };
        }
    }
    if (lang == LangId::Lua && rest.starts_with("--"))
        || (supports_slash_line_comments(lang) && rest.starts_with("//"))
        || (supports_hash_line_comments(lang) && rest.starts_with('#'))
    {
        return CommentAt::Comment(line_comment_len(rest));
    }
    if supports_block_comments(lang) && rest.starts_with("/*") {
        return block("/*", "*/");
    }
    CommentAt::NotComment
}

/// What follows an import statement on the rest of its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SameLineTail {
    /// Bytes from the statement end through the last comment on the line;
    /// zero when the line holds no comment.
    pub(crate) comment_end: usize,
    /// Bytes from the statement end to the line break (or the end of the text).
    pub(crate) line_end: usize,
}

/// Reads the text after an import statement up to its line break. Returns
/// `None` when that stretch holds anything other than spaces, tabs and
/// comments that end on the same line (for example a second statement, or a
/// block comment that continues on the next line).
pub(crate) fn same_line_tail(after_statement: &str, lang: LangId) -> Option<SameLineTail> {
    let mut offset = 0;
    let mut comment_end = 0;
    loop {
        let rest = &after_statement[offset..];
        match rest.chars().next() {
            None | Some('\n') | Some('\r') => {
                return Some(SameLineTail {
                    comment_end,
                    line_end: offset,
                })
            }
            Some(' ') | Some('\t') => offset += 1,
            Some(_) => match comment_len_at(rest, lang) {
                CommentAt::Comment(len) if !rest[..len].contains(['\n', '\r']) => {
                    offset += len;
                    comment_end = offset;
                }
                _ => return None,
            },
        }
    }
}

/// True when an import statement's own text contains a comment, such as a
/// note inside a multi-line named-import list. Quoted text is skipped so a
/// module path like `"https://..."` is not mistaken for a comment.
fn statement_has_comment(raw: &str, lang: LangId) -> bool {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, ch) in raw.char_indices() {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == open {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '`' => quote = Some(ch),
            // A Rust `'` starts a lifetime or char, never an import path.
            '\'' if lang != LangId::Rust => quote = Some(ch),
            _ => {
                if !matches!(comment_len_at(&raw[index..], lang), CommentAt::NotComment) {
                    return true;
                }
            }
        }
    }
    false
}

/// Length of a line comment: everything up to the line break. A line break
/// is `\n`, `\r\n` or a lone `\r` (old Mac files), so stopping at `\n`
/// alone would run a comment across every following line of a CR-only file.
fn line_comment_len(rest: &str) -> usize {
    rest.find(['\n', '\r']).unwrap_or(rest.len())
}

fn supports_slash_line_comments(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::TypeScript
            | LangId::Tsx
            | LangId::JavaScript
            | LangId::Go
            | LangId::Rust
            | LangId::Solidity
            | LangId::Java
            | LangId::Kotlin
            | LangId::Scala
            | LangId::CSharp
            | LangId::Php
            | LangId::Swift
            | LangId::C
            | LangId::Cpp
            | LangId::Vue
    )
}

fn supports_block_comments(lang: LangId) -> bool {
    supports_slash_line_comments(lang)
}

fn supports_hash_line_comments(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::Python | LangId::Ruby | LangId::Perl | LangId::Php
    )
}

/// Comments that belong to one import statement and move with it when
/// organize reorders, merges or deduplicates that statement.
#[derive(Debug, Clone, Default)]
struct AttachedComments {
    /// Comment lines directly above the statement, with no blank line in
    /// between, verbatim and without their final line break.
    leading: Option<String>,
    /// The comment after the statement on the same line, including the
    /// whitespace that separates it from the statement.
    trailing: Option<String>,
}

/// Attached comments keyed by the start byte of the statement they belong to.
type AttachedCommentMap = HashMap<usize, AttachedComments>;

/// How the comments inside an import block are distributed.
struct ImportComments {
    attached: AttachedCommentMap,
    /// For each pair of neighbouring imports, the part of the gap between them
    /// that no statement claimed. A comment left here, such as a section
    /// header separated from the next import by a blank line, still splits
    /// organize into independently sorted segments.
    unattached_gaps: Vec<Range<usize>>,
    /// Where the import block ends once the last statement's same-line comment
    /// is included, so that comment is replaced together with its statement.
    block_end: Option<usize>,
}

/// Assigns the comments between imports to the statements they describe:
/// a comment on the same line as a statement belongs to that statement, and
/// comment lines directly above a statement (no blank line between) belong to
/// the statement below them.
///
/// Comments above the FIRST import are deliberately left alone. They sit
/// outside the replaced range and usually are file headers, licence text or
/// tool directives that must stay at the top of the file.
fn collect_import_comments(
    source: &str,
    lang: LangId,
    imports: &[ImportStatement],
) -> ImportComments {
    let mut attached: AttachedCommentMap = HashMap::new();
    let mut unattached_gaps = Vec::new();

    for pair in imports.windows(2) {
        let (previous, next) = (&pair[0], &pair[1]);
        let gap_range = previous.byte_range.end..next.byte_range.start;
        let Some(gap) = source.get(gap_range.clone()) else {
            unattached_gaps.push(gap_range);
            continue;
        };
        let (trailing_end, leading_start) = split_import_gap(source, lang, gap_range.clone());
        if trailing_end > 0 {
            attached
                .entry(previous.byte_range.start)
                .or_default()
                .trailing = Some(gap[..trailing_end].to_string());
        }
        if leading_start < gap.len() {
            let leading = gap[leading_start..].trim_end();
            attached.entry(next.byte_range.start).or_default().leading = Some(leading.to_string());
        }
        unattached_gaps.push(gap_range.start + trailing_end..gap_range.start + leading_start);
    }

    let mut block_end = None;
    if let Some(last) = imports.last() {
        if let Some(tail) = source
            .get(last.byte_range.end..)
            .and_then(|after| same_line_tail(after, lang))
            .filter(|tail| tail.comment_end > 0)
        {
            let end = last.byte_range.end + tail.comment_end;
            attached.entry(last.byte_range.start).or_default().trailing =
                Some(source[last.byte_range.end..end].to_string());
            block_end = Some(end);
        }
    }

    ImportComments {
        attached,
        unattached_gaps,
        block_end,
    }
}

/// Splits the gap between two imports into the previous statement's
/// same-line comment, an unclaimed middle, and the comment lines directly
/// above the next statement. Returns `(trailing_end, leading_start)` as
/// offsets into the gap; `0` and the gap length mean that side claims nothing.
/// Nothing is claimed unless the unclaimed middle is itself whitespace and
/// complete comments, so a block comment is never cut in two.
fn split_import_gap(source: &str, lang: LangId, gap_range: Range<usize>) -> (usize, usize) {
    let unclaimed = (0, gap_range.len());
    let Some(gap) = source.get(gap_range.clone()) else {
        return unclaimed;
    };
    let Some(tail) = same_line_tail(gap, lang) else {
        return unclaimed;
    };
    let trailing_end = tail.comment_end;
    // The next import must start on a later line for either claim to apply.
    let Some(newline) = gap[tail.line_end..].find('\n') else {
        return unclaimed;
    };
    let body_start = tail.line_end + newline + 1;

    // Leading comments are the lines after the last blank line in the gap.
    let mut leading_start = body_start;
    let mut offset = body_start;
    for line in gap[body_start..].split_inclusive('\n') {
        offset += line.len();
        if line.ends_with('\n') && line.trim().is_empty() {
            leading_start = offset;
        }
    }
    let leading_is_comment =
        scan_import_gap(source, lang, gap_range.start + leading_start..gap_range.end)
            .is_some_and(|trivia| trivia.has_comment);
    if !leading_is_comment {
        leading_start = gap.len();
    }

    let middle_is_trivia = scan_import_gap(
        source,
        lang,
        gap_range.start + trailing_end..gap_range.start + leading_start,
    )
    .is_some();
    if middle_is_trivia {
        (trailing_end, leading_start)
    } else {
        unclaimed
    }
}

fn organize_preserving_comment_gaps(
    source: &str,
    lang: LangId,
    imports: &[ImportStatement],
    comments: &ImportComments,
) -> (Vec<(ImportGroup, Vec<OrganizedImport>)>, usize, String) {
    let mut grouped = Vec::new();
    let mut removed_duplicates = 0;
    let mut output = String::new();
    let mut segment_start = 0;

    for idx in 0..imports.len() {
        let next_gap = comments.unattached_gaps.get(idx).map(|range| {
            (
                import_gap_has_comment(source, lang, range.clone()),
                range.clone(),
            )
        });
        let is_boundary = next_gap
            .as_ref()
            .map(|(has_comment, _)| *has_comment)
            .unwrap_or(true);

        if !is_boundary {
            continue;
        }

        let mut refs: Vec<&ImportStatement> = imports[segment_start..=idx].iter().collect();
        refs.sort_by_key(|imp| imp.byte_range.start);
        let (segment_grouped, segment_removed) = organize_ordered_import_refs(&refs, lang);
        output.push_str(&generate_organized_block(
            &segment_grouped,
            lang,
            &comments.attached,
        ));
        grouped.extend(segment_grouped);
        removed_duplicates += segment_removed;

        if let Some((true, range)) = next_gap {
            if let Some(gap) = source.get(range) {
                output.push_str(gap);
            }
        }

        segment_start = idx + 1;
    }

    (grouped, removed_duplicates, output)
}

/// Organize imports: group by convention, sort within groups, deduplicate.
/// Returns (grouped imports in order, count of removed duplicates).
fn organize(
    imports: &[ImportStatement],
    lang: LangId,
) -> (Vec<(ImportGroup, Vec<OrganizedImport>)>, usize) {
    let mut refs: Vec<&ImportStatement> = imports.iter().collect();
    refs.sort_by_key(|imp| imp.byte_range.start);
    organize_ordered_import_refs(&refs, lang)
}

fn organize_ordered_import_refs(
    refs: &[&ImportStatement],
    lang: LangId,
) -> (Vec<(ImportGroup, Vec<OrganizedImport>)>, usize) {
    if preserves_side_effect_order(lang)
        && refs.iter().any(|imp| imp.kind == ImportKind::SideEffect)
    {
        return organize_preserving_side_effect_order(refs, lang);
    }

    organize_import_refs(refs, lang)
}

fn organize_import_refs(
    imports: &[&ImportStatement],
    lang: LangId,
) -> (Vec<(ImportGroup, Vec<OrganizedImport>)>, usize) {
    // Group imports
    let mut groups: BTreeMap<ImportGroup, Vec<&ImportStatement>> = BTreeMap::new();
    for imp in imports {
        groups.entry(imp.group).or_default().push(*imp);
    }

    let mut result: Vec<(ImportGroup, Vec<OrganizedImport>)> = Vec::new();
    let mut total_removed = 0;

    for (group, imps) in &groups {
        let (organized, removed) = if matches!(lang, LangId::Rust) {
            organize_rust_group(imps)
        } else if should_preserve_raw_on_organize(lang) {
            organize_raw_preserving_group(imps, lang)
        } else {
            organize_generic_group(imps, lang)
        };
        total_removed += removed;
        if !organized.is_empty() {
            result.push((*group, organized));
        }
    }

    (result, total_removed)
}

fn preserves_side_effect_order(lang: LangId) -> bool {
    // C/C++ includes textually expand under the preprocessor state at their
    // source position. Include guards are optional, so both reordering and
    // deduplicating directives can change the compiled program.
    matches!(
        lang,
        LangId::TypeScript
            | LangId::Tsx
            | LangId::JavaScript
            | LangId::Vue
            | LangId::Lua
            | LangId::C
            | LangId::Cpp
    )
}

fn organize_preserving_side_effect_order(
    imports: &[&ImportStatement],
    lang: LangId,
) -> (Vec<(ImportGroup, Vec<OrganizedImport>)>, usize) {
    let mut result = Vec::new();
    let mut total_removed = 0;
    let mut segment: Vec<&ImportStatement> = Vec::new();

    for imp in imports {
        if imp.kind == ImportKind::SideEffect {
            let (mut grouped, removed) = organize_import_refs(&segment, lang);
            result.append(&mut grouped);
            total_removed += removed;
            segment.clear();

            result.push((imp.group, vec![organized_from_statement(imp, lang)]));
        } else {
            segment.push(*imp);
        }
    }

    let (mut grouped, removed) = organize_import_refs(&segment, lang);
    result.append(&mut grouped);
    total_removed += removed;

    (result, total_removed)
}

fn organized_from_statement(imp: &ImportStatement, lang: LangId) -> OrganizedImport {
    let mut names = imp.names.clone();
    sort_named_specifiers(&mut names);
    let raw_override = (should_preserve_raw_on_organize(lang)
        || statement_has_comment(&imp.raw_text, lang))
    .then(|| imp.raw_text.trim().to_string())
    .filter(|raw| !raw.is_empty());

    OrganizedImport {
        module_path: imp.module_path.clone(),
        names,
        default_import: imp.default_import.clone(),
        namespace_import: imp.namespace_import.clone(),
        kind: imp.kind,
        attribute_clause: imports::es_import_attribute_clause(imp).map(str::to_string),
        style: Some(imports::quotes::statement_style(&imp.raw_text)),
        raw_override,
        origins: vec![imp.byte_range.start],
    }
}

fn should_preserve_raw_on_organize(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::Scala
            | LangId::Java
            | LangId::CSharp
            | LangId::Php
            | LangId::Kotlin
            | LangId::Solidity
            | LangId::Swift
            | LangId::Ruby
            | LangId::Lua
            | LangId::Perl
            | LangId::C
            | LangId::Cpp
            | LangId::Vue
            // Python: the structured parser captures only the FIRST module of a
            // multi-module `import os, sys` statement, so the generic regenerate
            // path dropped `sys` (silent data loss). Raw-preserve keeps each
            // statement's source text verbatim — no merge of `from X import a` +
            // `from X import b`, but zero corruption. Grouping/sort/dedup still
            // apply per group.
            | LangId::Python
    )
}

/// An organized import ready for code generation.
#[derive(Debug, Clone)]
struct OrganizedImport {
    module_path: String,
    names: Vec<String>,
    default_import: Option<String>,
    namespace_import: Option<String>,
    kind: ImportKind,
    attribute_clause: Option<String>,
    /// Quote and terminator of the source statement, kept when regenerating it.
    style: Option<imports::quotes::EsImportStyle>,
    /// When set, the import is rendered verbatim from this string instead of
    /// being regenerated from the structured fields. Used by dialect-sensitive
    /// languages (e.g. Scala) where re-rendering would normalize across
    /// incompatible syntax variants and corrupt the source, and for any
    /// statement whose own text contains a comment, which regeneration drops.
    raw_override: Option<String>,
    /// Start bytes of the source statements this import was built from (more
    /// than one after deduplication or a Rust merge). Their attached comments
    /// are emitted around the generated line.
    origins: Vec<usize>,
}

/// Organize a group of non-Rust imports: sort by module path, deduplicate.
///
/// Sorting ESM value imports is not semantically free: modules evaluate in the
/// order they are first requested, so reordering two imports of side-effecting
/// modules changes initialization order. That is a deliberate accepted cost,
/// not an oversight — every mainstream JS organizer sorts value imports the
/// same way (verified against Biome, which reorders an identical fixture), so
/// pinning them here would make AFT the only tool that silently declines to
/// organize a JS file, and would break the far commoner case of a developer
/// wanting tidy imports in a codebase of side-effect-free modules.
///
/// The escape hatch is the one the language itself provides and the one those
/// tools honour: a bare `import "./x.js"` is a side-effect import, which this
/// organizer already treats as a hard ordering barrier. Code that depends on
/// evaluation order should express it that way.
///
/// Languages whose EVERY import form is order-dependent (C/C++ textual
/// includes) are excluded from this path entirely — see
/// `preserves_side_effect_order`.
fn organize_generic_group(
    imps: &[&ImportStatement],
    lang: LangId,
) -> (Vec<OrganizedImport>, usize) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut organized: Vec<OrganizedImport> = Vec::new();
    let mut removed = 0;

    let mut side_effects: Vec<&&ImportStatement> = imps
        .iter()
        .filter(|imp| imp.kind == ImportKind::SideEffect)
        .collect();
    let mut sorted: Vec<&&ImportStatement> = imps
        .iter()
        .filter(|imp| imp.kind != ImportKind::SideEffect)
        .collect();
    sorted.sort_by(|a, b| a.module_path.cmp(&b.module_path));

    // Side-effect imports are evaluation-order sensitive. Keep their original
    // relative source order as a pinned subgroup before value/type imports.
    side_effects.extend(sorted);

    for imp in side_effects {
        // Build dedup key from every semantic part of the import. Import
        // attributes select the module type, so attributed and unattributed
        // imports of the same path must remain distinct.
        let names_key = {
            let mut n = imp.names.clone();
            sort_named_specifiers(&mut n);
            n.join(",")
        };
        let attribute_clause = imports::es_import_attribute_clause(imp);
        // Regenerating a statement drops comments inside it, so such a
        // statement is kept verbatim and never merged away as a duplicate.
        let raw_override =
            statement_has_comment(&imp.raw_text, lang).then(|| imp.raw_text.trim().to_string());
        let dedup_key = format!(
            "{}|{:?}|{}|{}|{}|{}|{}",
            imp.module_path,
            imp.kind,
            names_key,
            imp.default_import.as_deref().unwrap_or(""),
            imp.namespace_import.as_deref().unwrap_or(""),
            attribute_clause.unwrap_or(""),
            raw_override.as_deref().unwrap_or("")
        );

        if let Some(&kept) = seen.get(&dedup_key) {
            organized[kept].origins.push(imp.byte_range.start);
            removed += 1;
            continue;
        }
        seen.insert(dedup_key, organized.len());

        let mut names = imp.names.clone();
        sort_named_specifiers(&mut names);

        organized.push(OrganizedImport {
            module_path: imp.module_path.clone(),
            names,
            default_import: imp.default_import.clone(),
            namespace_import: imp.namespace_import.clone(),
            kind: imp.kind,
            attribute_clause: attribute_clause.map(str::to_string),
            style: Some(imports::quotes::statement_style(&imp.raw_text)),
            raw_override,
            origins: vec![imp.byte_range.start],
        });
    }

    (organized, removed)
}

fn organize_raw_preserving_group(
    imps: &[&ImportStatement],
    lang: LangId,
) -> (Vec<OrganizedImport>, usize) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut kept: Vec<(&ImportStatement, Vec<usize>)> = Vec::new();
    let mut removed = 0;

    for imp in imps {
        let raw = imp.raw_text.trim();
        if raw.is_empty() {
            continue;
        }

        if !import_form_reexecutes_target(lang, &imp.form) {
            let mut key = raw_preserving_dedup_key(imp);
            // A duplicate is dropped, so one with a comment inside it is
            // kept apart rather than losing that comment.
            if statement_has_comment(raw, lang) {
                key.push('|');
                key.push_str(raw);
            }
            if let Some(&index) = seen.get(&key) {
                kept[index].1.push(imp.byte_range.start);
                removed += 1;
                continue;
            }
            seen.insert(key, kept.len());
        }
        kept.push((*imp, vec![imp.byte_range.start]));
    }

    let (mut side_effects, mut sorted): (Vec<_>, Vec<_>) = kept
        .into_iter()
        .partition(|(imp, _)| imp.kind == ImportKind::SideEffect);

    if lang == LangId::Python {
        sorted.sort_by(|(a, _), (b, _)| python_sort_key(a).cmp(&python_sort_key(b)));
    } else {
        sorted.sort_by(|(a, _), (b, _)| a.raw_text.trim().cmp(b.raw_text.trim()));
    }
    side_effects.extend(sorted);

    let organized = side_effects
        .into_iter()
        .map(|(imp, origins)| OrganizedImport {
            module_path: imp.module_path.clone(),
            names: imp.names.clone(),
            default_import: imp.default_import.clone(),
            namespace_import: imp.namespace_import.clone(),
            kind: imp.kind,
            attribute_clause: imports::es_import_attribute_clause(imp).map(str::to_string),
            style: Some(imports::quotes::statement_style(&imp.raw_text)),
            raw_override: Some(imp.raw_text.trim().to_string()),
            origins,
        })
        .collect();

    (organized, removed)
}

/// True for import forms that invoke user code on every occurrence.
/// Deduplicating these forms changes program behavior even when their underlying
/// module loader is idempotent, so they bypass the dedup pass entirely.
/// isort's default order within a section: every `import x` line first, then
/// every `from x import ...` line, each run ordered by module name compared
/// case-insensitively. The raw text breaks remaining ties so the order is
/// deterministic.
fn python_sort_key(imp: &ImportStatement) -> (bool, String, &str, &str) {
    let from_import = matches!(
        imp.form,
        ImportForm::Python {
            from_import: true,
            ..
        }
    );
    (
        from_import,
        imp.module_path.to_lowercase(),
        imp.module_path.as_str(),
        imp.raw_text.trim(),
    )
}

fn import_form_reexecutes_target(lang: LangId, form: &ImportForm) -> bool {
    match (lang, form) {
        (
            LangId::Ruby,
            ImportForm::Structured {
                import_kind: Some(import_kind),
                ..
            },
        ) => import_kind == crate::imports::ruby::RUBY_LOAD_KIND,
        (
            LangId::Perl,
            ImportForm::Structured {
                import_kind: Some(import_kind),
                ..
            },
        ) => matches!(
            import_kind.as_str(),
            crate::imports::perl::PERL_USE_KIND | crate::imports::perl::PERL_NO_KIND
        ),
        _ => false,
    }
}

fn raw_preserving_dedup_key(imp: &ImportStatement) -> String {
    let mut form = imp.form.clone();
    match &mut form {
        ImportForm::Structured { named, .. }
        | ImportForm::Solidity { named, .. }
        | ImportForm::Es { named, .. }
        | ImportForm::Python { named, .. }
        | ImportForm::RustUse { named, .. } => sort_named_specifiers(named),
        ImportForm::Go { .. } | ImportForm::Php { .. } => {}
    }

    format!("{}|{:?}|{:?}", imp.module_path, imp.kind, form)
}

fn sort_named_specifiers(names: &mut [String]) {
    names.sort_by(|a, b| {
        imports::specifier_imported_name(a)
            .cmp(imports::specifier_imported_name(b))
            .then_with(|| a.cmp(b))
    });
}

/// Organize Rust use declarations: sort, deduplicate, and merge common prefixes.
fn organize_rust_group(imps: &[&ImportStatement]) -> (Vec<OrganizedImport>, usize) {
    use std::collections::BTreeMap as BMap;

    // First pass: collect all use paths. For items like `use std::path::Path;`,
    // extract prefix `std::path` and item `Path`. For items like `use serde::{Deserialize, Serialize}`,
    // keep as-is (already a tree).
    #[derive(Debug)]
    struct UsePath {
        /// Full original module_path (e.g. "std::path::Path" or "serde::{Deserialize, Serialize}")
        full_path: String,
        /// Prefix for merging (e.g. "std::path")
        prefix: Option<String>,
        /// Leaf item(s) for merging (e.g. ["Path"])
        items: Vec<String>,
        kind: ImportKind,
        /// Exact visibility text (`pub`, `pub(crate)`, …) or None for private.
        /// Part of the merge key so different visibilities don't merge, and
        /// re-emitted verbatim so `pub(crate) use` isn't widened to `pub use`.
        visibility: Option<String>,
        /// Start byte of the source statement, for its attached comments.
        origin: usize,
    }

    let mut paths: Vec<UsePath> = Vec::new();
    let mut removed = 0;
    // Statements with a comment inside them are emitted verbatim and never
    // merged, because rebuilding a use tree would drop the comment.
    let mut verbatim: Vec<OrganizedImport> = Vec::new();

    for imp in imps {
        if statement_has_comment(&imp.raw_text, LangId::Rust) {
            verbatim.push(OrganizedImport {
                module_path: imports::rust_use_tree(imp),
                names: vec![],
                default_import: imp.default_import.clone(),
                namespace_import: None,
                kind: imp.kind,
                attribute_clause: None,
                style: None,
                raw_override: Some(imp.raw_text.trim().to_string()),
                origins: vec![imp.byte_range.start],
            });
            continue;
        }
        let origin = imp.byte_range.start;
        let visibility = imp.default_import.clone();
        // The parser separates a use-list prefix from its top-level entries.
        // This organizer works on full use trees, unlike add/remove operations.
        let use_tree = imports::rust_use_tree(imp);
        let mp = &use_tree;

        // Check if this already has a use list (contains '{')
        if mp.contains('{') {
            // Already a tree like "serde::{Deserialize, Serialize}"
            // Extract prefix and items
            if let Some(brace_pos) = mp.find("::{").filter(|_| !imp.module_path.is_empty()) {
                let prefix = mp[..brace_pos].to_string();
                let items_str = &mp[brace_pos + 3..mp.len() - 1]; // strip ::{ and }
                                                                  // Split on TOP-LEVEL commas only. A naive split(',') corrupts
                                                                  // nested use trees like `hash_map::{Entry, HashMap}, BTreeMap`
                                                                  // into `hash_map::{Entry` / `HashMap}` / `BTreeMap`, which then
                                                                  // sort and regroup into invalid Rust. Brace-aware splitting keeps
                                                                  // each nested subtree intact as one opaque item, so re-emitting
                                                                  // `prefix::{items}` stays syntactically valid.
                let items: Vec<String> = split_top_level_commas(items_str)
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                paths.push(UsePath {
                    full_path: mp.clone(),
                    prefix: Some(prefix),
                    items,
                    kind: imp.kind,
                    visibility: visibility.clone(),
                    origin,
                });
            } else {
                paths.push(UsePath {
                    full_path: mp.clone(),
                    prefix: None,
                    items: vec![],
                    kind: imp.kind,
                    visibility: visibility.clone(),
                    origin,
                });
            }
        } else if let Some(last_sep) = mp.rfind("::") {
            // Simple path like "std::path::Path" → prefix "std::path", item "Path"
            let prefix = mp[..last_sep].to_string();
            let item = mp[last_sep + 2..].to_string();
            paths.push(UsePath {
                full_path: mp.clone(),
                prefix: Some(prefix),
                items: vec![item],
                kind: imp.kind,
                visibility: visibility.clone(),
                origin,
            });
        } else {
            // Single-segment like "serde" — no prefix to merge on
            paths.push(UsePath {
                full_path: mp.clone(),
                prefix: None,
                items: vec![],
                kind: imp.kind,
                visibility: visibility.clone(),
                origin,
            });
        }
    }

    // Group by (prefix, kind, visibility) for merging. Visibility is part of the
    // key so `pub(crate) use a::X` never merges with `pub use a::Y`, and it is
    // re-emitted verbatim (was: widened to bare `pub`).
    let mut merge_groups: BMap<(String, u8, Option<String>), (Vec<String>, Vec<usize>)> =
        BMap::new();
    let mut no_prefix: Vec<OrganizedImport> = Vec::new();

    for up in &paths {
        if let Some(ref prefix) = up.prefix {
            let kind_d = match up.kind {
                ImportKind::Value => 0,
                ImportKind::Type => 1,
                ImportKind::SideEffect => 2,
            };
            let key = (prefix.clone(), kind_d, up.visibility.clone());
            let (entry, origins) = merge_groups.entry(key).or_default();
            origins.push(up.origin);
            for item in &up.items {
                if !entry.contains(item) {
                    entry.push(item.clone());
                } else {
                    removed += 1;
                }
            }
        } else {
            // Check for duplicate
            let already = no_prefix.iter_mut().find(|o| {
                o.module_path == up.full_path
                    && o.kind == up.kind
                    && o.default_import == up.visibility
            });
            if let Some(kept) = already {
                kept.origins.push(up.origin);
                removed += 1;
            } else {
                no_prefix.push(OrganizedImport {
                    module_path: up.full_path.clone(),
                    names: vec![],
                    default_import: up.visibility.clone(),
                    namespace_import: None,
                    kind: up.kind,
                    attribute_clause: None,
                    style: None,
                    raw_override: None,
                    origins: vec![up.origin],
                });
            }
        }
    }

    // Convert merge groups into OrganizedImport entries
    let mut organized: Vec<OrganizedImport> = Vec::new();

    for ((prefix, kind_d, visibility), (mut items, origins)) in merge_groups {
        items.sort();
        let kind = match kind_d {
            1 => ImportKind::Type,
            2 => ImportKind::SideEffect,
            _ => ImportKind::Value,
        };

        let module_path =
            if items.len() == 1 && items[0] != "self" && !items[0].starts_with("self as ") {
                // Single item — no braces needed
                format!("{}::{}", prefix, items[0])
            } else {
                // Multiple items — use tree
                format!("{}::{{{}}}", prefix, items.join(", "))
            };

        organized.push(OrganizedImport {
            module_path,
            names: vec![],
            default_import: visibility,
            namespace_import: None,
            kind,
            attribute_clause: None,
            style: None,
            raw_override: None,
            origins,
        });
    }

    // Add no-prefix items and sort everything by module_path
    organized.extend(no_prefix);
    organized.extend(verbatim);
    organized.sort_by(|a, b| a.module_path.cmp(&b.module_path));

    // Track how many original imports were merged away
    let final_count = organized.len();
    let original_count = imps.len();
    if original_count > final_count + removed {
        removed = original_count - final_count;
    }

    (organized, removed)
}

/// Split a Rust use-list body on TOP-LEVEL commas only, treating nested
/// `{...}` (and defensively `[...]`/`(...)`) as opaque so commas inside a
/// nested subtree do not split it.
///
/// `"hash_map::{Entry, HashMap}, BTreeMap"` -> `["hash_map::{Entry, HashMap}", "BTreeMap"]`
/// `"Deserialize, Serialize"`               -> `["Deserialize", "Serialize"]`
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth -= 1,
            ',' if depth == 0 => {
                items.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(s[start..].to_string());
    items
}

/// Generate the full organized import block text.
fn generate_organized_block(
    grouped: &[(ImportGroup, Vec<OrganizedImport>)],
    lang: LangId,
    attached: &AttachedCommentMap,
) -> String {
    let mut output = String::new();
    let mut previous_group: Option<ImportGroup> = None;

    for (group, imps) in grouped {
        let mut lines: Vec<String> = Vec::new();
        for imp in imps {
            let line = generate_organized_line(imp, lang);
            lines.push(with_attached_comments(line, &imp.origins, attached));
        }
        if lines.is_empty() {
            continue;
        }

        if !output.is_empty() {
            if previous_group == Some(*group) {
                output.push('\n');
            } else {
                output.push_str("\n\n");
            }
        }
        output.push_str(&lines.join("\n"));
        previous_group = Some(*group);
    }

    output
}

/// Surrounds a generated import line with the comments attached to the source
/// statements it came from: their leading comment lines above it and the first
/// same-line comment after it. When deduplication or a Rust merge folds several
/// commented statements into one line, the extra same-line comments become
/// comment lines above it, so no comment is lost.
fn with_attached_comments(
    line: String,
    origins: &[usize],
    attached: &AttachedCommentMap,
) -> String {
    let mut above: Vec<&str> = Vec::new();
    let mut trailing: Option<&str> = None;
    let mut sorted_origins = origins.to_vec();
    sorted_origins.sort_unstable();
    for origin in sorted_origins {
        let Some(comments) = attached.get(&origin) else {
            continue;
        };
        if let Some(leading) = comments.leading.as_deref() {
            above.push(leading);
        }
        if let Some(comment) = comments.trailing.as_deref() {
            if trailing.is_none() {
                trailing = Some(comment);
            } else {
                above.push(comment.trim_start());
            }
        }
    }
    if above.is_empty() && trailing.is_none() {
        return line;
    }
    let mut output = String::new();
    for comment in above {
        output.push_str(comment);
        output.push('\n');
    }
    output.push_str(&line);
    output.push_str(trailing.unwrap_or(""));
    output
}

fn generate_go_grouped_block(grouped: &[(ImportGroup, Vec<OrganizedImport>)]) -> String {
    let mut lines = Vec::new();
    lines.push("import (".to_string());
    for (group_idx, (_, imps)) in grouped.iter().enumerate() {
        if group_idx > 0 {
            lines.push(String::new());
        }
        for imp in imps {
            if let Some(ref alias) = imp.default_import {
                lines.push(format!("\t{} \"{}\"", alias, imp.module_path));
            } else {
                lines.push(format!("\t\"{}\"", imp.module_path));
            }
        }
    }
    lines.push(")".to_string());
    lines.join("\n")
}

/// Generate a single import line from an OrganizedImport.
fn generate_organized_line(imp: &OrganizedImport, lang: LangId) -> String {
    if let Some(ref raw) = imp.raw_override {
        return raw.clone();
    }
    match lang {
        LangId::Rust => {
            // default_import carries the exact visibility text (`pub`,
            // `pub(crate)`, `pub(super)`, …); emit it verbatim so restricted
            // visibilities aren't widened to bare `pub`.
            match imp.default_import.as_deref() {
                Some(vis) if !vis.is_empty() => format!("{} use {};", vis, imp.module_path),
                _ => format!("use {};", imp.module_path),
            }
        }
        LangId::Go => {
            // Go organize: regenerate as standalone imports
            // (organize_imports for Go would need grouped import rewrite — keep simple for now)
            if let Some(ref alias) = imp.default_import {
                format!("import {} \"{}\"", alias, imp.module_path)
            } else {
                format!("import \"{}\"", imp.module_path)
            }
        }
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
            imports::generate_import_line_with_namespace_and_attribute_clause_and_style(
                lang,
                &imp.module_path,
                &imp.names,
                imp.default_import.as_deref(),
                imp.namespace_import.as_deref(),
                imp.kind == ImportKind::Type,
                imp.attribute_clause.as_deref(),
                imp.style,
            )
        }
        _ => {
            // TS/JS/TSX/Python — use the standard generator
            imports::generate_import_line_with_namespace(
                lang,
                &imp.module_path,
                &imp.names,
                imp.default_import.as_deref(),
                imp.namespace_import.as_deref(),
                imp.kind == ImportKind::Type,
            )
        }
    }
}
