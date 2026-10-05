const LINE_COUNT_BYTES: u64 = 1024 * 1024;
const ENTRY_BUDGET: usize = 10_000;

use std::collections::{HashMap, VecDeque};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::commands::read::{handle_github_outline, is_github_read_target};
use crate::context::AppContext;
use crate::edit;
use crate::error::AftError;
use crate::inspect::job::is_test_file;
use crate::parser::{
    detect_language, node_range_with_decorators, node_text, FileParser, LangId, TreeSitterProvider,
};
use crate::protocol::{RawRequest, Response};
use crate::symbols::{Range, Symbol};
use crate::url_fetch::{fetch_url_to_cache, is_http_url, UrlFetchOptions};

const MAX_OUTLINE_FILE_BYTES: u64 = 50 * 1024 * 1024;
const BINARY_SAMPLE_BYTES: usize = 4 * 1024;
const OUTLINE_FILE_WALK_CAP: usize = 200;
const OUTLINE_FILE_COLLECTION_CAP: usize = 10_000;
// A focused outline still lists all product members. Test bodies use the same
// summary as broad outlines; read/zoom remain the way to inspect those bodies.
const COLLAPSE_SINGLE_FILE_TESTS: bool = true;
const TYPE_MEMBER_PREVIEW_CAP: usize = 3;

/// A single entry in the outline tree.
///
/// Top-level symbols have an empty `members` vec. Classes/structs contain
/// their methods and nested types in `members`, forming a recursive tree.
#[derive(Debug, Clone, Serialize)]
pub struct OutlineEntry {
    pub name: String,
    pub kind: String,
    pub range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub exported: bool,
    pub members: Vec<OutlineEntry>,
}

/// Handle an `outline` request.
///
/// Expects `file` or `files` in request params. Calls `list_symbols()` on the provider,
/// then builds a nested tree and returns compact tree-text output.
///
/// - Single-file mode: includes signatures (e.g. `function greet(name: string): void 5:12`,
///   or `E function greet(...) 5:12` when exported without a visibility keyword in the signature)
/// - Multi-file mode: no signatures, paths relative to project_root
///
/// Output is capped at 30KB; if exceeded, truncates with a narrowing hint.
pub fn handle_outline(req: &RawRequest, ctx: &AppContext) -> Response {
    const MAX_OUTPUT_BYTES: usize = 30 * 1024;

    if req
        .params
        .get("files")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return handle_outline_files_mode(req, ctx, MAX_OUTPUT_BYTES);
    }

    if let Some(directory) = req.params.get("directory").and_then(|v| v.as_str()) {
        let dir_path = match ctx.validate_path(&req.id, Path::new(directory)) {
            Ok(path) => path,
            Err(resp) => return resp,
        };
        if !dir_path.is_dir() {
            return Response::error(
                &req.id,
                "file_not_found",
                format!("directory not found: {}", directory),
            );
        }

        let discovery = discover_outline_files(&dir_path);
        let project_root = ctx.config().project_root.clone();
        let include_tests = include_tests_param(req);
        let files = if include_tests {
            discovery.files.clone()
        } else {
            discovery
                .files
                .iter()
                .filter(|file| {
                    let path = Path::new(file);
                    let relative = project_root
                        .as_deref()
                        .and_then(|root| relative_path_from_root(path, root))
                        .unwrap_or_else(|| path_to_slash(path));
                    !is_test_file(&relative)
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let (file_outlines, skipped_files) =
            match outline_many_files(&files, ctx, &req.id, project_root.as_deref()) {
                Ok(result) => result,
                Err(resp) => return resp,
            };

        let walk_incomplete = discovery.walk_truncated
            || discovery.collection_truncated
            || discovery.skipped_foreign_mounts > 0;
        let rendered = format_multi_file_tree(
            &file_outlines,
            MAX_OUTPUT_BYTES,
            files.len(),
            walk_incomplete,
        );
        return Response::success(
            &req.id,
            serde_json::json!({
                "text": rendered.text,
                "discovered": true,
                "complete": !walk_incomplete && !rendered.truncated,
                "output_truncated": rendered.truncated,
                "files_shown": rendered.shown,
                "structure_footer": rendered.footer,
                "walk_truncated": discovery.walk_truncated,
                "collection_truncated": discovery.collection_truncated,
                "skipped_foreign_mounts": discovery.skipped_foreign_mounts,
                "skipped_files": skipped_files,
            }),
        );
    }

    // Multi-file mode: if "files" array is present, outline each file
    if let Some(files_arr) = req.params.get("files").and_then(|v| v.as_array()) {
        let project_root = ctx.config().project_root.clone();
        let files: Vec<String> = files_arr
            .iter()
            .filter_map(|file_val| file_val.as_str().map(String::from))
            .collect();
        let total_files_requested = files_arr.len();
        let (file_outlines, skipped_files) =
            match outline_many_files(&files, ctx, &req.id, project_root.as_deref()) {
                Ok(result) => result,
                Err(resp) => return resp,
            };

        let rendered = format_multi_file_tree(
            &file_outlines,
            MAX_OUTPUT_BYTES,
            total_files_requested,
            false,
        );
        // Honest reporting: complete only when no requested file was skipped.
        // skipped_files names the gaps (missing/unreadable/unparseable inputs).
        return Response::success(
            &req.id,
            serde_json::json!({
                "text": rendered.text,
                "complete": skipped_files.is_empty() && !rendered.truncated,
                "output_truncated": rendered.truncated,
                "files_shown": rendered.shown,
                "structure_footer": rendered.footer,
                "skipped_files": skipped_files,
            }),
        );
    }

    // Single-file mode (original behavior)
    let file = match req
        .params
        .get("file")
        .or_else(|| req.params.get("target"))
        .and_then(|v| v.as_str())
    {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "outline: missing required param 'file', 'files', or 'directory'",
            );
        }
    };

    if is_github_read_target(file) {
        return handle_github_outline(req, ctx, file);
    }

    let path = match resolve_file_or_url(req, ctx, file) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("file not found: {}", file),
        );
    }

    let symbols = match ctx.provider().list_symbols(&path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    let mut parser = FileParser::new();
    let entries = match outline_structure_entries(
        &path,
        &symbols,
        &mut parser,
        ctx,
        &req.id,
        COLLAPSE_SINGLE_FILE_TESTS,
    ) {
        Ok(entries) => entries,
        Err(response) => return response,
    };
    let filename = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| file.to_string());
    let text = format_single_file_tree(&filename, &entries);

    Response::success(
        &req.id,
        serde_json::json!({ "text": text, "complete": true }),
    )
}

fn include_tests_param(req: &RawRequest) -> bool {
    req.params
        .get("includeTests")
        .or_else(|| req.params.get("include_tests"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn resolve_file_or_url(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
) -> Result<PathBuf, Response> {
    if is_http_url(file) {
        let storage_dir = crate::bash_background::storage_dir(ctx.config().storage_dir.as_deref());
        let allow_private = ctx.config().url_fetch_allow_private
            || req
                .params
                .get("allow_private")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
        return fetch_url_to_cache(
            file,
            &storage_dir,
            UrlFetchOptions {
                allow_private,
                ..UrlFetchOptions::default()
            },
        )
        .map_err(|error| Response::error(&req.id, "url_fetch_failed", error.to_string()));
    }

    ctx.validate_path(&req.id, Path::new(file))
}

/// Build a nested outline tree from a flat symbol list.
///
/// Strategy: two passes.
/// 1. Convert every top-level symbol to an `OutlineEntry` and index sibling names.
/// 2. Walk children (parent.is_some()) and attach them under their parent.
///    For multi-level nesting (e.g. OuterClass.InnerClass.inner_method),
///    we use the `scope_chain` to walk the full parent path.
///
/// Symbols whose parent can't be found in the list are promoted to top level
/// (defensive — shouldn't happen with well-formed parser output).
pub(crate) fn build_outline_tree(symbols: &[Symbol]) -> Vec<OutlineEntry> {
    let mut top_level = Vec::new();
    let mut scope_index = OutlineScopeIndex::default();
    let mut children = Vec::new();

    for sym in symbols {
        if sym.parent.is_none() {
            push_indexed_entry(&mut top_level, &mut scope_index, symbol_to_entry(sym));
        } else {
            children.push(sym);
        }
    }

    for child in children {
        let entry = symbol_to_entry(child);
        let scope = &child.scope_chain;

        if scope.is_empty() {
            push_indexed_entry(&mut top_level, &mut scope_index, entry);
            continue;
        }

        // Preserve the established lookup ladder: try the full display scope,
        // then the direct parent used by languages such as Rust, then promote.
        let entry = match insert_at_scope_indexed(&mut top_level, &mut scope_index, scope, entry) {
            Ok(()) => continue,
            Err(entry) => entry,
        };
        let entry = match child.parent.as_ref() {
            Some(parent) => match insert_at_scope_indexed(
                &mut top_level,
                &mut scope_index,
                std::slice::from_ref(parent),
                entry,
            ) {
                Ok(()) => continue,
                Err(entry) => entry,
            },
            None => entry,
        };
        push_indexed_entry(&mut top_level, &mut scope_index, entry);
    }

    top_level
}

// Small sibling lists are cheaper to scan than to allocate a map for. Once a
// level reaches this bound, every later lookup is indexed and the scan cost is
// capped independently of the file's symbol count.
const OUTLINE_SCOPE_INDEX_THRESHOLD: usize = 8;

#[derive(Default)]
struct OutlineScopeIndex {
    first_by_name: Option<HashMap<String, usize>>,
    children: Vec<OutlineScopeIndex>,
}

impl OutlineScopeIndex {
    fn first_match(&self, entries: &[OutlineEntry], name: &str) -> Option<usize> {
        if let Some(first_by_name) = &self.first_by_name {
            return first_by_name.get(name).copied();
        }
        entries.iter().position(|entry| entry.name == name)
    }

    fn note_pushed(&mut self, entries: &[OutlineEntry]) {
        debug_assert_eq!(self.children.len() + 1, entries.len());
        self.children.push(Self::default());

        if let Some(first_by_name) = &mut self.first_by_name {
            let index = entries.len() - 1;
            let name = &entries[index].name;
            if !first_by_name.contains_key(name) {
                first_by_name.insert(name.clone(), index);
            }
        } else if entries.len() == OUTLINE_SCOPE_INDEX_THRESHOLD {
            let mut first_by_name = HashMap::with_capacity(entries.len());
            for (index, entry) in entries.iter().enumerate() {
                first_by_name.entry(entry.name.clone()).or_insert(index);
            }
            self.first_by_name = Some(first_by_name);
        }
    }
}

fn push_indexed_entry(
    entries: &mut Vec<OutlineEntry>,
    scope_index: &mut OutlineScopeIndex,
    entry: OutlineEntry,
) {
    entries.push(entry);
    scope_index.note_pushed(entries);
}

fn insert_at_scope_indexed(
    entries: &mut Vec<OutlineEntry>,
    scope_index: &mut OutlineScopeIndex,
    scope_chain: &[String],
    entry: OutlineEntry,
) -> Result<(), OutlineEntry> {
    let Some(target_name) = scope_chain.first() else {
        return Err(entry);
    };
    let Some(target_index) = scope_index.first_match(entries, target_name) else {
        return Err(entry);
    };

    let existing = &mut entries[target_index];
    let child_index = &mut scope_index.children[target_index];
    if scope_chain.len() == 1 {
        push_indexed_entry(&mut existing.members, child_index, entry);
        Ok(())
    } else {
        insert_at_scope_indexed(&mut existing.members, child_index, &scope_chain[1..], entry)
    }
}

// ── Tree text formatting ──────────────────────────────────────────────

/// Intermediate representation for multi-file tree rendering.
struct FileOutline {
    path: String, // relative path
    entries: Vec<OutlineEntry>,
    language: Option<LangId>,
}

struct TestSummary {
    name: String,
    range: Range,
    items: usize,
}

struct TestRegion {
    summary: TestSummary,
    included_path: Option<String>,
    implicit_include: bool,
    module_scope: Vec<String>,
    standalone: bool,
}

enum TestNode {
    Module,
    Standalone,
    Block,
}

/// Classify test code only from syntax nodes, attributes and declaration names.
/// In particular, a module named `tests` without `cfg(test)`, a string mentioning
/// `#[test]`, or a product class with a `test_*` method is not a test module.
fn classify_test_node(node: tree_sitter::Node<'_>, source: &str, lang: LangId) -> Option<TestNode> {
    match lang {
        LangId::Rust => match node.kind() {
            "mod_item"
                if rust_attributes(node).into_iter().any(|attr| {
                    attr.named_child(0)
                        .is_some_and(|name| node_text(source, &name) == "cfg")
                        && attr
                            .named_child(1)
                            .is_some_and(|args| cfg_requires_test(args, source))
                }) =>
            {
                Some(TestNode::Module)
            }
            "function_item"
                if node.parent().is_some_and(|parent| {
                    parent.kind() == "source_file"
                        || (parent.kind() == "declaration_list"
                            && parent
                                .parent()
                                .is_some_and(|owner| owner.kind() == "mod_item"))
                }) && rust_attributes(node).into_iter().any(|attr| {
                    attr.named_child_count() == 1
                        && attr
                            .named_child(0)
                            .is_some_and(|name| node_text(source, &name) == "test")
                }) =>
            {
                Some(TestNode::Standalone)
            }
            _ => None,
        },
        LangId::Python => {
            let declaration = node
                .parent()
                .filter(|parent| parent.kind() == "decorated_definition")
                .unwrap_or(node);
            if !declaration
                .parent()
                .is_some_and(|parent| parent.kind() == "module")
            {
                return None;
            }
            let name = node.child_by_field_name("name")?;
            match node.kind() {
                "class_definition" if node_text(source, &name).starts_with("Test") => {
                    Some(TestNode::Module)
                }
                "function_definition" if node_text(source, &name).starts_with("test_") => {
                    Some(TestNode::Standalone)
                }
                _ => None,
            }
        }
        LangId::Go if node.kind() == "function_declaration" => {
            let name = node.child_by_field_name("name")?;
            node_text(source, &name)
                .starts_with("Test")
                .then_some(TestNode::Standalone)
        }
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript
            if node.kind() == "call_expression" =>
        {
            let mut callee = node.child_by_field_name("function")?;
            loop {
                match callee.kind() {
                    "member_expression" => callee = callee.child_by_field_name("object")?,
                    "call_expression" => callee = callee.child_by_field_name("function")?,
                    _ => break,
                }
            }
            if callee.kind() != "identifier"
                || !matches!(node_text(source, &callee), "describe" | "test" | "it")
            {
                return None;
            }
            let args = node.child_by_field_name("arguments")?;
            let mut cursor = args.walk();
            let has_callback = args
                .named_children(&mut cursor)
                .any(|arg| matches!(arg.kind(), "arrow_function" | "function_expression"));
            has_callback.then_some(TestNode::Block)
        }
        _ => None,
    }
}

fn rust_attributes(node: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
    let mut attributes = Vec::new();
    let mut previous = node.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "attribute_item" => {
                if let Some(attribute) = sibling.named_child(0) {
                    attributes.push(attribute);
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = sibling.prev_named_sibling();
    }
    attributes
}

/// `all(test, ...)` requires test compilation; `any(test, feature = ...)`
/// and `not(test)` do not. Token-tree identifiers exclude string lookalikes.
fn cfg_requires_test(args: tree_sitter::Node<'_>, source: &str) -> bool {
    let mut cursor = args.walk();
    let children = args.named_children(&mut cursor).collect::<Vec<_>>();
    match children.as_slice() {
        [name] => name.kind() == "identifier" && node_text(source, name) == "test",
        [name, nested] if name.kind() == "identifier" && node_text(source, name) == "all" => {
            let mut cursor = nested.walk();
            let requires_test = nested
                .named_children(&mut cursor)
                .any(|child| child.kind() == "identifier" && node_text(source, &child) == "test");
            requires_test
        }
        _ => false,
    }
}

fn count_test_blocks(node: tree_sitter::Node<'_>, source: &str, lang: LangId) -> usize {
    let own = usize::from(matches!(
        classify_test_node(node, source, lang),
        Some(TestNode::Block)
    ));
    let mut cursor = node.walk();
    own + node
        .named_children(&mut cursor)
        .map(|child| count_test_blocks(child, source, lang))
        .sum::<usize>()
}

fn collect_test_regions(
    node: tree_sitter::Node<'_>,
    source: &str,
    lang: LangId,
    regions: &mut Vec<TestRegion>,
) {
    if let Some(kind) = classify_test_node(node, source, lang) {
        // Nested suites belong to the outer top-level block, not extra rows.
        let top_level_block = node.parent().is_some_and(|parent| {
            parent.kind() == "expression_statement"
                && parent
                    .parent()
                    .is_some_and(|owner| owner.kind() == "program")
        });
        if !matches!(kind, TestNode::Block) || top_level_block {
            let standalone = matches!(kind, TestNode::Standalone);
            let name = if standalone {
                "tests".to_string()
            } else if matches!(kind, TestNode::Block) {
                let callee = node.child_by_field_name("function").unwrap();
                let label = node
                    .child_by_field_name("arguments")
                    .and_then(|args| args.named_child(0));
                format!(
                    "{} {}",
                    node_text(source, &callee),
                    label
                        .map(|label| node_text(source, &label))
                        .unwrap_or("tests")
                )
            } else {
                node.child_by_field_name("name")
                    .map(|name| node_text(source, &name))
                    .unwrap_or("tests")
                    .to_string()
            };
            let external_module = lang == LangId::Rust
                && node.kind() == "mod_item"
                && node.child_by_field_name("body").is_none();
            let explicit_path = if lang == LangId::Rust
                && node.kind() == "mod_item"
                && node.child_by_field_name("body").is_none()
            {
                rust_attributes(node).into_iter().find_map(|attr| {
                    let name = attr.named_child(0)?;
                    if node_text(source, &name) != "path" {
                        return None;
                    }
                    let value = attr.child_by_field_name("value")?;
                    (value.kind() == "string_literal")
                        .then(|| node_text(source, &value).trim_matches('"').to_string())
                })
            } else {
                None
            };
            let implicit_include = external_module && explicit_path.is_none();
            let included_path =
                explicit_path.or_else(|| external_module.then(|| format!("{name}.rs")));
            let mut module_scope = Vec::new();
            let mut parent = node.parent();
            while let Some(ancestor) = parent {
                if ancestor.kind() == "mod_item" {
                    if let Some(name) = ancestor.child_by_field_name("name") {
                        module_scope.push(node_text(source, &name).to_string());
                    }
                }
                parent = ancestor.parent();
            }
            module_scope.reverse();
            regions.push(TestRegion {
                summary: TestSummary {
                    name,
                    range: node_range_with_decorators(&node, source, lang),
                    // Callback test blocks have no named symbol in the symbol table.
                    items: if matches!(kind, TestNode::Block) {
                        count_test_blocks(node, source, lang)
                    } else {
                        0
                    },
                },
                included_path,
                implicit_include,
                module_scope,
                standalone,
            });
            return;
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_test_regions(child, source, lang, regions);
    }
}

fn range_contains(outer: &Range, inner: &Range) -> bool {
    (outer.start_line, outer.start_col) <= (inner.start_line, inner.start_col)
        && (inner.end_line, inner.end_col) <= (outer.end_line, outer.end_col)
}

/// Collapse only the broad structure map. Single-file outlines and the shared
/// symbol cache retain every symbol so zoom and other tools still see test code.
fn outline_structure_entries(
    path: &Path,
    symbols: &[Symbol],
    parser: &mut FileParser,
    ctx: &AppContext,
    req_id: &str,
    collapse_tests: bool,
) -> Result<Vec<OutlineEntry>, Response> {
    let Some(lang) = detect_language(path).filter(|lang| {
        matches!(
            lang,
            LangId::Rust
                | LangId::Python
                | LangId::Go
                | LangId::TypeScript
                | LangId::Tsx
                | LangId::JavaScript
        )
    }) else {
        return Ok(build_outline_tree(symbols));
    };
    let source = std::fs::read_to_string(path)
        .map_err(|error| Response::error(req_id, "file_not_found", error.to_string()))?;
    let (tree, _) = parser
        .parse_cloned(path)
        .map_err(|error| Response::error(req_id, error.code(), error.to_string()))?;
    let mut regions = Vec::new();
    if collapse_tests {
        collect_test_regions(tree.root_node(), &source, lang, &mut regions);
    }
    let mut product_symbols = Vec::new();
    for symbol in symbols {
        if let Some(region) = regions
            .iter_mut()
            .find(|region| range_contains(&region.summary.range, &symbol.range))
        {
            region.summary.items += 1;
        } else {
            product_symbols.push(symbol.clone());
        }
    }
    let mut summaries: Vec<TestSummary> = Vec::new();
    let mut standalone: Option<TestSummary> = None;
    for mut region in regions {
        if let Some(included_path) = region.included_path {
            let parent = path.parent().unwrap_or(Path::new("."));
            let mut base = parent.to_path_buf();
            if region.implicit_include || !region.module_scope.is_empty() {
                if let Some(stem) = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .filter(|stem| !matches!(*stem, "lib" | "main" | "mod"))
                {
                    base.push(stem);
                }
            }
            for module in region.module_scope {
                base.push(module);
            }
            let mut included = base.join(&included_path);
            if region.implicit_include && !included.is_file() {
                included = base.join(&region.summary.name).join("mod.rs");
            }
            let included_label = relative_path_from_root(&included, parent)
                .unwrap_or_else(|| path_to_slash(&included));
            let included = ctx.validate_path(req_id, &included)?;
            region.summary.items = parser
                .extract_symbols(&included)
                .map_err(|error| Response::error(req_id, error.code(), error.to_string()))?
                .len();
            parser.evict_parse_tree(&included);
            region
                .summary
                .name
                .push_str(&format!(" (path {included_label})"));
        }
        if region.standalone {
            if let Some(summary) = &mut standalone {
                summary.items += region.summary.items;
                summary.range.end_line = region.summary.range.end_line;
                summary.range.end_col = region.summary.range.end_col;
            } else {
                standalone = Some(region.summary);
            }
        } else {
            summaries.push(region.summary);
        }
    }
    summaries.extend(standalone);
    summaries.sort_by_key(|summary| (summary.range.start_line, summary.range.start_col));
    let mut entries = if lang == LangId::Rust {
        rust_module_outline(tree.root_node(), &source, &product_symbols, &summaries, &[])
    } else {
        build_outline_tree(&product_symbols)
    };
    for summary in summaries {
        let entry = OutlineEntry {
            signature: Some(format!(
                "{}: {} items (lines {}-{})",
                summary.name,
                summary.items,
                summary.range.start_line + 1,
                summary.range.end_line + 1
            )),
            name: summary.name,
            kind: "test_summary".into(),
            range: summary.range,
            exported: false,
            members: Vec::new(),
        };
        insert_summary_entry(&mut entries, entry);
    }
    Ok(entries)
}

fn insert_summary_entry(entries: &mut Vec<OutlineEntry>, entry: OutlineEntry) {
    if let Some(module) = entries
        .iter_mut()
        .find(|module| module.kind == "module" && range_contains(&module.range, &entry.range))
    {
        insert_summary_entry(&mut module.members, entry);
        return;
    }
    let position = entries
        .iter()
        .position(|existing| existing.range.start_line >= entry.range.start_line)
        .unwrap_or(entries.len());
    entries.insert(position, entry);
}

/// Module containers are absent from the shared symbol table. Keep them in the
/// outline so nested module functions never masquerade as file-level APIs, and
/// impl methods attach to the type in their own module rather than a namesake.
fn rust_module_outline(
    root: tree_sitter::Node<'_>,
    source: &str,
    symbols: &[Symbol],
    tests: &[TestSummary],
    scope: &[String],
) -> Vec<OutlineEntry> {
    fn immediate_modules<'a>(
        node: tree_sitter::Node<'a>,
        modules: &mut Vec<tree_sitter::Node<'a>>,
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "mod_item" {
                modules.push(child);
            } else {
                immediate_modules(child, modules);
            }
        }
    }
    let mut modules = Vec::new();
    immediate_modules(root, &mut modules);
    let modules = modules
        .into_iter()
        .filter(|node| {
            !tests.iter().any(|test| {
                range_contains(
                    &test.range,
                    &node_range_with_decorators(node, source, LangId::Rust),
                )
            })
        })
        .collect::<Vec<_>>();
    let ranges = modules
        .iter()
        .map(|node| node_range_with_decorators(node, source, LangId::Rust))
        .collect::<Vec<_>>();
    let local = symbols
        .iter()
        .filter(|symbol| {
            !ranges
                .iter()
                .any(|range| range_contains(range, &symbol.range))
        })
        .map(|symbol| {
            let mut symbol = symbol.clone();
            if !scope.is_empty() && symbol.scope_chain.starts_with(scope) {
                symbol.scope_chain.drain(..scope.len());
                symbol.parent = symbol.scope_chain.last().cloned();
            }
            symbol
        })
        .collect::<Vec<_>>();
    let mut entries = build_outline_tree(&local);
    for (module, range) in modules.into_iter().zip(ranges) {
        let name = module
            .child_by_field_name("name")
            .map(|name| node_text(source, &name))
            .unwrap_or("module")
            .to_string();
        let mut module_scope = scope.to_vec();
        module_scope.push(name.clone());
        let module_symbols = symbols
            .iter()
            .filter(|symbol| range_contains(&range, &symbol.range))
            .cloned()
            .collect::<Vec<_>>();
        let members = module
            .child_by_field_name("body")
            .map(|body| rust_module_outline(body, source, &module_symbols, tests, &module_scope))
            .unwrap_or_default();
        let mut cursor = module.walk();
        let exported = module
            .named_children(&mut cursor)
            .any(|child| child.kind() == "visibility_modifier");
        let entry = OutlineEntry {
            name: name.clone(),
            kind: "module".into(),
            range,
            signature: Some(format!("{}mod {name}", if exported { "pub " } else { "" })),
            exported,
            members,
        };
        insert_summary_entry(&mut entries, entry);
    }
    entries
}

#[derive(Debug, Clone, Serialize)]
struct SkippedFile {
    file: String,
    reason: String,
}

impl SkippedFile {
    fn new(file: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct OutlineFileEntry {
    path: String,
    language: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbols: Option<usize>,
    lines: Option<usize>,
    #[serde(skip)]
    absolute_path: PathBuf,
    #[serde(skip)]
    data_doc: bool,
}

#[derive(Debug, Clone, Default)]
struct OutlineDirectoryStats {
    dirs: usize,
    files: usize,
    lines: usize,
    unknown_lines: usize,
    data_doc_files: usize,
    code_files: usize,
    code_lines: usize,
}

#[derive(Debug, Clone)]
struct OutlineDirectoryNode {
    path: String,
    depth: usize,
    direct_files: Vec<usize>,
    children: Vec<usize>,
    stats: OutlineDirectoryStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutlineTableRow {
    File(usize),
    Rollup(usize),
}

/// Rendered outline table output wrapping the formatted text with its budget-measured length (R13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineTable {
    table: String,
    rendered_len: usize,
}

impl std::ops::Deref for OutlineTable {
    type Target = str;
    fn deref(&self) -> &str {
        &self.table
    }
}

impl AsRef<str> for OutlineTable {
    fn as_ref(&self) -> &str {
        &self.table
    }
}

impl std::fmt::Display for OutlineTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.table)
    }
}

impl serde::Serialize for OutlineTable {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.table)
    }
}

impl OutlineTable {
    pub fn len(&self) -> usize {
        self.rendered_len
    }

    pub fn is_empty(&self) -> bool {
        self.rendered_len == 0
    }

    pub fn as_str(&self) -> &str {
        &self.table
    }

    pub fn into_string(self) -> String {
        self.table
    }
}

impl From<OutlineTable> for String {
    fn from(table: OutlineTable) -> Self {
        table.table
    }
}

#[derive(Debug, Clone, Copy)]
struct OutlineFileContentStats {
    binary: bool,
    lines: Option<usize>,
}

#[derive(Debug, Clone)]
struct OutlineFileDiscovery {
    entries_examined: usize,
    files: Vec<String>,
    directories: Vec<String>,
    walk_truncated: bool,
    collection_truncated: bool,
    skipped_foreign_mounts: usize,
}

#[derive(Clone, Default)]
struct OutlineIgnoreStack {
    matchers: Vec<Arc<ignore::gitignore::Gitignore>>,
}

impl OutlineIgnoreStack {
    fn for_target_root(root: &Path) -> Self {
        let root = root.to_path_buf();
        let mut stack = Self::default();

        // Global excludes and .git/info/exclude are lower priority than every
        // .gitignore in the target's ancestor chain. Load them once per walk.
        let (global, _) = ignore::gitignore::GitignoreBuilder::new(&root).build_global();
        if !global.is_empty() {
            stack.matchers.push(Arc::new(global));
        }
        if let Some((repo_root, exclude_path)) = git_info_exclude_for_target(&root) {
            if let Some(matcher) = build_outline_gitignore(&repo_root, &exclude_path) {
                stack.matchers.push(matcher);
            }
        }

        let mut ancestors = root.ancestors().map(Path::to_path_buf).collect::<Vec<_>>();
        ancestors.reverse();
        for directory in ancestors {
            if let Some(matcher) =
                build_outline_gitignore(&directory, &directory.join(".gitignore"))
            {
                stack.matchers.push(matcher);
            }
        }
        stack
    }

    fn for_child_directory(&self, directory: &Path) -> Self {
        let mut child = self.clone();
        if let Some(matcher) = build_outline_gitignore(directory, &directory.join(".gitignore")) {
            child.matchers.push(matcher);
        }
        child
    }

    fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let mut ignored = false;
        for matcher in &self.matchers {
            let matched = matcher.matched_path_or_any_parents(path, is_dir);
            if matched.is_ignore() {
                ignored = true;
            } else if matched.is_whitelist() {
                ignored = false;
            }
        }
        ignored
    }
}

fn build_outline_gitignore(
    root: &Path,
    ignore_file: &Path,
) -> Option<Arc<ignore::gitignore::Gitignore>> {
    if !ignore_file.is_file() {
        return None;
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    let _ = builder.add(ignore_file);
    builder.build().ok().map(Arc::new)
}

fn git_info_exclude_for_target(target: &Path) -> Option<(PathBuf, PathBuf)> {
    for repository_root in target.ancestors() {
        let git_entry = repository_root.join(".git");
        let Ok(metadata) = std::fs::metadata(&git_entry) else {
            continue;
        };
        let git_dir = if metadata.is_dir() {
            git_entry
        } else if metadata.is_file() {
            let contents = std::fs::read_to_string(&git_entry).ok()?;
            let git_dir = contents.trim().strip_prefix("gitdir:")?.trim();
            let git_dir = PathBuf::from(git_dir);
            if git_dir.is_absolute() {
                git_dir
            } else {
                repository_root.join(git_dir)
            }
        } else {
            continue;
        };
        let git_dir = std::fs::canonicalize(&git_dir).unwrap_or(git_dir);
        let common_dir = git_dir.join("commondir");
        let git_common_dir = if let Ok(common) = std::fs::read_to_string(&common_dir) {
            let common = PathBuf::from(common.trim());
            if common.is_absolute() {
                common
            } else {
                git_dir.join(common)
            }
        } else {
            git_dir
        };
        return Some((
            repository_root.to_path_buf(),
            git_common_dir.join("info/exclude"),
        ));
    }
    None
}

fn handle_outline_files_mode(
    req: &RawRequest,
    ctx: &AppContext,
    max_output_bytes: usize,
) -> Response {
    let targets = match outline_files_mode_targets(req) {
        Ok(targets) => targets,
        Err(response) => return response,
    };

    let multiple_targets = targets.len() >= 2;
    let project_root = ctx.config().project_root.clone();
    let include_tests = include_tests_param(req);

    let mut file_entries = Vec::new();
    let mut directory_nodes = Vec::new();
    let mut tree_roots = Vec::new();
    let mut entries_examined = 0usize;
    let mut walk_truncated = false;
    let mut collection_truncated = false;
    let mut skipped_foreign_mounts = 0usize;

    for target in targets {
        let dir_path = match ctx.validate_path(&req.id, Path::new(&target)) {
            Ok(path) => path,
            Err(response) => return response,
        };

        if !dir_path.exists() {
            return Response::error(
                &req.id,
                "file_not_found",
                format!("directory not found: {}", target),
            );
        }
        if !dir_path.is_dir() {
            return Response::error(
                &req.id,
                "invalid_request",
                "files mode requires a directory target",
            );
        }

        let display_root = if multiple_targets {
            project_root.as_deref().unwrap_or(&dir_path)
        } else {
            &dir_path
        };
        let discovery = discover_outline_files_for_files_mode(&dir_path);
        entries_examined += discovery.entries_examined;
        walk_truncated |= discovery.walk_truncated;
        collection_truncated |= discovery.collection_truncated;
        skipped_foreign_mounts += discovery.skipped_foreign_mounts;

        let root = append_outline_directory_tree(
            &dir_path,
            display_root,
            discovery,
            ctx,
            include_tests,
            &mut file_entries,
            &mut directory_nodes,
        );
        tree_roots.push(root);
    }

    for root in &tree_roots {
        aggregate_outline_directory(*root, &mut directory_nodes, &file_entries);
    }
    let rows = plan_outline_file_rows(
        &tree_roots,
        &directory_nodes,
        &file_entries,
        max_output_bytes,
    );
    populate_rendered_file_symbols(&rows, &mut file_entries, ctx);
    let table = format_files_table(&rows, &directory_nodes, &file_entries, max_output_bytes);
    let mut text = table.into_string();
    let unknown_lines = file_entries
        .iter()
        .filter(|entry| entry.lines.is_none() && entry.language != "binary")
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    if !unknown_lines.is_empty() {
        text.push_str("\nLine counts unknown for unreadable files or text exceeding the 1048576-byte count budget; narrow: read a file range.\n");
    }
    let rollup_count = rows
        .iter()
        .filter(|row| matches!(row, OutlineTableRow::Rollup(_)))
        .count();

    let shown = rows
        .iter()
        .filter(|row| matches!(row, OutlineTableRow::File(_)))
        .count();
    let mut budget_rollup_files = 0;
    let mut budget_rollups_present = false;
    for row in &rows {
        if let OutlineTableRow::Rollup(node_id) = row {
            if !directory_is_data_heavy(&directory_nodes[*node_id]) {
                budget_rollups_present = true;
                budget_rollup_files += directory_nodes[*node_id].stats.files;
            }
        }
    }

    let envelope = crate::list_surfaces::outline::build_outline_files_envelope(
        shown,
        budget_rollup_files,
        budget_rollups_present,
        collection_truncated,
        walk_truncated,
        skipped_foreign_mounts,
    );

    let mut unchecked_files = Vec::new();
    if walk_truncated {
        unchecked_files
            .push("<additional files not counted: 10000-file walk limit reached>".to_string());
    }
    if collection_truncated {
        unchecked_files.push("<additional files not counted: directory walk failed or 10000-entry examination budget reached>".to_string());
    }
    if skipped_foreign_mounts > 0 {
        unchecked_files.push(format!(
            "<{skipped_foreign_mounts} foreign filesystem mount(s) not traversed>"
        ));
    }

    file_entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut response_data = serde_json::json!({
        "text": text,
        "files": file_entries,
        "complete": !walk_truncated
            && !collection_truncated
            && skipped_foreign_mounts == 0
            && unknown_lines.is_empty(),
        "line_count_gaps": unknown_lines,
        "entries_examined": entries_examined,
        "walk_truncated": walk_truncated,
        "walk_limit": OUTLINE_FILE_COLLECTION_CAP,
        "collection_truncated": collection_truncated,
        "skipped_foreign_mounts": skipped_foreign_mounts,
        "unchecked_files": unchecked_files,
        "rollup_count": rollup_count,
    });
    if let Some(env) = envelope {
        response_data["files_list_envelope"] = serde_json::to_value(&env).unwrap();
    }
    Response::success(&req.id, response_data)
}

fn outline_files_mode_targets(req: &RawRequest) -> Result<Vec<String>, Response> {
    if let Some(directory) = req.params.get("directory").and_then(|value| value.as_str()) {
        return Ok(vec![directory.to_string()]);
    }

    if let Some(directories) = req
        .params
        .get("directories")
        .and_then(|value| value.as_array())
    {
        let targets = directories
            .iter()
            .filter_map(|value| value.as_str().map(ToOwned::to_owned))
            .collect::<Vec<_>>();
        if !targets.is_empty() {
            return Ok(targets);
        }
    }

    if let Some(targets) = req.params.get("targets") {
        if let Some(target) = targets.as_str() {
            return Ok(vec![target.to_string()]);
        }
        if let Some(targets) = targets.as_array() {
            let targets = targets
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect::<Vec<_>>();
            if !targets.is_empty() {
                return Ok(targets);
            }
        }
    }

    if let Some(target) = req.params.get("target") {
        if let Some(target) = target.as_str() {
            return Ok(vec![target.to_string()]);
        }
        if let Some(targets) = target.as_array() {
            let targets = targets
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect::<Vec<_>>();
            if !targets.is_empty() {
                return Ok(targets);
            }
        }
    }

    if let Some(file) = req.params.get("file").and_then(|value| value.as_str()) {
        return Ok(vec![file.to_string()]);
    }

    Err(Response::error(
        &req.id,
        "invalid_request",
        "files mode requires a directory target",
    ))
}

fn discover_outline_files_for_files_mode(directory: &Path) -> OutlineFileDiscovery {
    discover_outline_files_with_options(directory, true)
}

fn append_outline_directory_tree(
    target_root: &Path,
    display_root: &Path,
    mut discovery: OutlineFileDiscovery,
    ctx: &AppContext,
    include_tests: bool,
    file_entries: &mut Vec<OutlineFileEntry>,
    directory_nodes: &mut Vec<OutlineDirectoryNode>,
) -> usize {
    let root_path = relative_path_from_root(target_root, display_root).unwrap_or_default();
    let root_id = directory_nodes.len();
    directory_nodes.push(OutlineDirectoryNode {
        path: root_path,
        depth: 0,
        direct_files: Vec::new(),
        children: Vec::new(),
        stats: OutlineDirectoryStats::default(),
    });

    discovery.directories.sort_by(|a, b| {
        Path::new(a)
            .components()
            .count()
            .cmp(&Path::new(b).components().count())
            .then_with(|| a.cmp(b))
    });
    let mut directory_ids = HashMap::new();
    directory_ids.insert(target_root.to_path_buf(), root_id);

    for directory in discovery.directories {
        let path = PathBuf::from(directory);
        let Some(parent_id) = path
            .parent()
            .and_then(|parent| directory_ids.get(parent).copied())
        else {
            continue;
        };
        let node_id = directory_nodes.len();
        let node_path =
            relative_path_from_root(&path, display_root).unwrap_or_else(|| path_to_slash(&path));
        let depth = directory_nodes[parent_id].depth + 1;
        directory_nodes.push(OutlineDirectoryNode {
            path: node_path,
            depth,
            direct_files: Vec::new(),
            children: Vec::new(),
            stats: OutlineDirectoryStats::default(),
        });
        directory_nodes[parent_id].children.push(node_id);
        directory_ids.insert(path, node_id);
    }

    for file in discovery.files {
        let path = PathBuf::from(file);
        let test_path = ctx
            .config()
            .project_root
            .as_deref()
            .and_then(|root| relative_path_from_root(&path, root))
            .unwrap_or_else(|| path_to_slash(&path));
        if !include_tests && is_test_file(&test_path) {
            continue;
        }
        let Some(entry) = outline_file_entry(&path, display_root) else {
            continue;
        };
        let Some(parent_id) = path
            .parent()
            .and_then(|parent| directory_ids.get(parent).copied())
        else {
            continue;
        };
        let file_id = file_entries.len();
        file_entries.push(entry);
        directory_nodes[parent_id].direct_files.push(file_id);
    }

    root_id
}

fn aggregate_outline_directory(
    node_id: usize,
    directory_nodes: &mut [OutlineDirectoryNode],
    file_entries: &[OutlineFileEntry],
) -> OutlineDirectoryStats {
    let direct_files = directory_nodes[node_id].direct_files.clone();
    let children = directory_nodes[node_id].children.clone();
    let mut stats = OutlineDirectoryStats::default();

    for file_id in direct_files {
        let entry = &file_entries[file_id];
        stats.files += 1;
        stats.lines += entry.lines.unwrap_or(0);
        stats.unknown_lines += usize::from(entry.lines.is_none() && entry.language != "binary");
        if entry.data_doc {
            stats.data_doc_files += 1;
        } else {
            stats.code_files += 1;
            stats.code_lines += entry.lines.unwrap_or(0);
        }
    }
    for child in children {
        let child_stats = aggregate_outline_directory(child, directory_nodes, file_entries);
        stats.dirs += child_stats.dirs + 1;
        stats.files += child_stats.files;
        stats.lines += child_stats.lines;
        stats.unknown_lines += child_stats.unknown_lines;
        stats.data_doc_files += child_stats.data_doc_files;
        stats.code_files += child_stats.code_files;
        stats.code_lines += child_stats.code_lines;
    }

    directory_nodes[node_id].stats = stats.clone();
    stats
}

fn outline_rows_for_directory(
    node_id: usize,
    directory_nodes: &[OutlineDirectoryNode],
    file_entries: &[OutlineFileEntry],
) -> Vec<OutlineTableRow> {
    let node = &directory_nodes[node_id];
    let mut code_files = node
        .direct_files
        .iter()
        .copied()
        .filter(|file_id| !file_entries[*file_id].data_doc)
        .collect::<Vec<_>>();
    let mut data_files = node
        .direct_files
        .iter()
        .copied()
        .filter(|file_id| file_entries[*file_id].data_doc)
        .collect::<Vec<_>>();
    let mut directories = node.children.clone();
    code_files.sort_by(|a, b| file_entries[*a].path.cmp(&file_entries[*b].path));
    data_files.sort_by(|a, b| file_entries[*a].path.cmp(&file_entries[*b].path));
    directories.sort_by(|a, b| directory_nodes[*a].path.cmp(&directory_nodes[*b].path));

    code_files
        .into_iter()
        .map(OutlineTableRow::File)
        .chain(directories.into_iter().map(OutlineTableRow::Rollup))
        .chain(data_files.into_iter().map(OutlineTableRow::File))
        .collect()
}

fn directory_is_data_heavy(node: &OutlineDirectoryNode) -> bool {
    !node.direct_files.is_empty()
        && node.stats.files > 0
        && node.stats.data_doc_files * 10 >= node.stats.files * 9
}

fn outline_expansion_added_rows(node: &OutlineDirectoryNode) -> usize {
    node.direct_files
        .len()
        .saturating_add(node.children.len())
        .saturating_sub(1)
}

fn compare_directory_code_share(
    a: &OutlineDirectoryNode,
    b: &OutlineDirectoryNode,
) -> std::cmp::Ordering {
    let a_total = if a.stats.lines == 0 {
        a.stats.files.max(1)
    } else {
        a.stats.lines
    };
    let b_total = if b.stats.lines == 0 {
        b.stats.files.max(1)
    } else {
        b.stats.lines
    };
    let a_code = if a.stats.lines == 0 {
        a.stats.code_files
    } else {
        a.stats.code_lines
    };
    let b_code = if b.stats.lines == 0 {
        b.stats.code_files
    } else {
        b.stats.code_lines
    };
    (a_code as u128 * b_total as u128).cmp(&(b_code as u128 * a_total as u128))
}

fn plan_outline_file_rows(
    roots: &[usize],
    directory_nodes: &[OutlineDirectoryNode],
    file_entries: &[OutlineFileEntry],
    max_bytes: usize,
) -> Vec<OutlineTableRow> {
    // A depth-first alphabetical list lets a large crate consume the whole
    // response before later siblings appear. Start with every direct entry,
    // then prefer cheap same-level expansions so the budget reveals breadth
    // before flattening a directory with hundreds of direct files. If a level
    // cannot finish, stop before deeper `src/` trees outrun sibling crates.
    let mut rows = roots
        .iter()
        .flat_map(|root| outline_rows_for_directory(*root, directory_nodes, file_entries))
        .collect::<Vec<_>>();
    let mut considered = std::collections::HashSet::new();

    loop {
        let Some(level) = rows
            .iter()
            .filter_map(|row| match row {
                OutlineTableRow::Rollup(node_id)
                    if !considered.contains(node_id)
                        && !directory_is_data_heavy(&directory_nodes[*node_id]) =>
                {
                    Some(directory_nodes[*node_id].depth)
                }
                _ => None,
            })
            .min()
        else {
            break;
        };

        let mut candidates = rows
            .iter()
            .filter_map(|row| match row {
                OutlineTableRow::Rollup(node_id)
                    if directory_nodes[*node_id].depth == level
                        && !considered.contains(node_id)
                        && !directory_is_data_heavy(&directory_nodes[*node_id]) =>
                {
                    Some(*node_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| {
            let a_node = &directory_nodes[*a];
            let b_node = &directory_nodes[*b];
            outline_expansion_added_rows(a_node)
                .cmp(&outline_expansion_added_rows(b_node))
                .then_with(|| compare_directory_code_share(a_node, b_node).reverse())
                .then_with(|| a_node.path.cmp(&b_node.path))
        });

        let mut level_fully_expanded = true;
        for node_id in candidates {
            considered.insert(node_id);
            let replacement = outline_rows_for_directory(node_id, directory_nodes, file_entries);
            if replacement.is_empty() {
                continue;
            }
            let Some(position) = rows
                .iter()
                .position(|row| *row == OutlineTableRow::Rollup(node_id))
            else {
                continue;
            };
            let mut candidate_rows = rows.clone();
            candidate_rows.splice(position..=position, replacement);
            if format_files_table(&candidate_rows, directory_nodes, file_entries, max_bytes).len()
                <= max_bytes
            {
                rows = candidate_rows;
            } else {
                level_fully_expanded = false;
            }
        }
        if !level_fully_expanded {
            break;
        }
    }

    rows
}

fn outline_file_entry(path: &Path, display_root: &Path) -> Option<OutlineFileEntry> {
    let rel_path =
        relative_path_from_root(path, display_root).unwrap_or_else(|| path_to_slash(path));
    let detected_language = detect_language(path);
    let content = inspect_outline_file_content(path).unwrap_or(OutlineFileContentStats {
        binary: false,
        lines: None,
    });
    let language = if content.binary {
        "binary"
    } else {
        outline_file_language(path, detected_language)
    };

    Some(OutlineFileEntry {
        path: rel_path,
        language: language.to_string(),
        symbols: None,
        lines: content.lines,
        absolute_path: path.to_path_buf(),
        data_doc: is_data_doc_outline_file(path, detected_language),
    })
}

fn populate_rendered_file_symbols(
    rows: &[OutlineTableRow],
    file_entries: &mut [OutlineFileEntry],
    ctx: &AppContext,
) {
    for file_id in rows.iter().filter_map(|row| match row {
        OutlineTableRow::File(file_id) => Some(*file_id),
        OutlineTableRow::Rollup(_) => None,
    }) {
        let entry = &mut file_entries[file_id];
        if entry.symbols.is_some() {
            continue;
        }
        if entry.language == "binary" {
            entry.symbols = Some(0);
            continue;
        }
        let path = entry.absolute_path.clone();
        if detect_language(&path).is_none() {
            entry.symbols = Some(0);
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            entry.symbols = Some(0);
            continue;
        };
        let symbols = cached_symbol_count(ctx, &path, &metadata).unwrap_or_else(|| {
            if metadata.len() > MAX_OUTLINE_FILE_BYTES {
                0
            } else {
                ctx.provider()
                    .list_symbols(&path)
                    .map(|symbols| symbols.len())
                    .unwrap_or(0)
            }
        });
        entry.symbols = Some(symbols);
    }
}

fn relative_path_from_root(path: &Path, root: &Path) -> Option<String> {
    if let Ok(relative) = path.strip_prefix(root) {
        return Some(path_to_slash(relative));
    }

    let canonical_path = std::fs::canonicalize(path).ok()?;
    let canonical_root = std::fs::canonicalize(root).ok()?;
    canonical_path
        .strip_prefix(canonical_root)
        .ok()
        .map(path_to_slash)
}

fn path_to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn cached_symbol_count(
    ctx: &AppContext,
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Option<usize> {
    let mtime = metadata.modified().unwrap_or(UNIX_EPOCH);
    let size = metadata.len();
    let symbol_cache = ctx.symbol_cache();
    let cache = symbol_cache.read().ok()?;
    cache
        .symbol_count_if_metadata_matches(path, mtime, size)
        .or_else(|| cache.get(path, mtime).map(|symbols| symbols.len()))
}

fn inspect_outline_file_content(path: &Path) -> std::io::Result<OutlineFileContentStats> {
    let mut file = std::fs::File::open(path)?;
    let mut sample = [0u8; BINARY_SAMPLE_BYTES];
    let sample_len = file.read(&mut sample)?;
    if sample_len > 0 && content_inspector::inspect(&sample[..sample_len]).is_binary() {
        return Ok(OutlineFileContentStats {
            binary: true,
            lines: None,
        });
    }

    if file.metadata()?.len() > LINE_COUNT_BYTES {
        return Ok(OutlineFileContentStats {
            binary: false,
            lines: None,
        });
    }
    let mut file = file.take(LINE_COUNT_BYTES + 1 - sample_len as u64);
    let mut newline_count = sample[..sample_len]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    let mut total_bytes = sample_len;
    let mut last_byte = sample_len.checked_sub(1).map(|index| sample[index]);
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        newline_count += buffer[..read].iter().filter(|byte| **byte == b'\n').count();
        total_bytes += read;
        last_byte = Some(buffer[read - 1]);
    }

    if total_bytes as u64 > LINE_COUNT_BYTES {
        return Ok(OutlineFileContentStats {
            binary: false,
            lines: None,
        });
    }
    let lines = newline_count + usize::from(total_bytes > 0 && last_byte != Some(b'\n'));
    Ok(OutlineFileContentStats {
        binary: false,
        lines: Some(lines),
    })
}

fn outline_file_language(path: &Path, detected_language: Option<LangId>) -> &'static str {
    if let Some(language) = detected_language {
        return language_id(language);
    }
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == "toml" {
        "toml"
    } else if extension == "lock" || filename.ends_with(".lock") {
        "lock"
    } else if extension == "txt" {
        "text"
    } else if extension == "bazel" || matches!(filename, "BUILD" | "WORKSPACE" | "MODULE.bazel") {
        "bazel"
    } else {
        "unknown"
    }
}

fn is_data_doc_outline_file(path: &Path, detected_language: Option<LangId>) -> bool {
    if matches!(
        detected_language,
        Some(LangId::Json | LangId::Yaml | LangId::Markdown)
    ) {
        return true;
    }
    detected_language.is_none()
        || matches!(
            outline_file_language(path, detected_language),
            "toml" | "lock" | "text" | "bazel" | "unknown"
        )
}

fn language_id(lang: LangId) -> &'static str {
    match lang {
        LangId::TypeScript => "typescript",
        LangId::Tsx => "tsx",
        LangId::JavaScript => "javascript",
        LangId::Python => "python",
        LangId::Rust => "rust",
        LangId::Go => "go",
        LangId::C => "c",
        LangId::Cpp => "cpp",
        LangId::Cuda => "cuda",
        LangId::Metal => "metal",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
        LangId::Html => "html",
        LangId::Markdown => "markdown",
        LangId::Yaml => "yaml",
        LangId::Solidity => "solidity",
        LangId::Scss => "scss",
        LangId::Vue => "vue",
        LangId::Json => "json",
        LangId::Scala => "scala",
        LangId::Java => "java",
        LangId::Ruby => "ruby",
        LangId::Kotlin => "kotlin",
        LangId::Swift => "swift",
        LangId::Php => "php",
        LangId::Lua => "lua",
        LangId::Perl => "perl",
        LangId::Pascal => "pascal",
        LangId::R => "r",
        LangId::Groovy => "groovy",
        LangId::ObjC => "objc",
        LangId::Toml => "toml",
    }
}

fn format_files_table(
    rows: &[OutlineTableRow],
    directory_nodes: &[OutlineDirectoryNode],
    file_entries: &[OutlineFileEntry],
    _max_bytes: usize,
) -> OutlineTable {
    let path_width = rows
        .iter()
        .map(|row| match row {
            OutlineTableRow::File(file_id) => file_entries[*file_id].path.len(),
            OutlineTableRow::Rollup(node_id) => directory_nodes[*node_id].path.len() + 1,
        })
        .max()
        .unwrap_or(0);
    let language_width = rows
        .iter()
        .filter_map(|row| match row {
            OutlineTableRow::File(file_id) => Some(file_entries[*file_id].language.len()),
            OutlineTableRow::Rollup(_) => None,
        })
        .max()
        .unwrap_or("language".len())
        .max(8);
    let file_middle_width = language_width + 11;
    let middle_width = rows
        .iter()
        .filter_map(|row| match row {
            OutlineTableRow::File(_) => None,
            OutlineTableRow::Rollup(node_id) => {
                Some(directory_rollup_summary(&directory_nodes[*node_id].stats).len())
            }
        })
        .max()
        .unwrap_or(0)
        .max(file_middle_width);

    let mut output = String::new();
    for row in rows {
        let (path, middle, lines) = match row {
            OutlineTableRow::File(file_id) => {
                let entry = &file_entries[*file_id];
                (
                    entry.path.clone(),
                    format!(
                        "{:<language_width$} {:>5} syms",
                        entry.language,
                        entry.symbols.unwrap_or(0)
                    ),
                    entry
                        .lines
                        .map(|lines| lines.to_string())
                        .or_else(|| (entry.language == "binary").then(|| "-".to_string())),
                )
            }
            OutlineTableRow::Rollup(node_id) => {
                let node = &directory_nodes[*node_id];
                (
                    format!("{}/", node.path.trim_end_matches('/')),
                    directory_rollup_summary(&node.stats),
                    (node.stats.unknown_lines == 0).then(|| node.stats.lines.to_string()),
                )
            }
        };
        output.push_str(&format!(
            "{path:<path_width$}  {middle:<middle_width$} {lines:>7} lines\n",
            lines = lines.as_deref().unwrap_or("unknown"),
        ));
    }

    let shown = rows
        .iter()
        .filter(|row| matches!(row, OutlineTableRow::File(_)))
        .count();
    let mut budget_rollup_files = 0;
    let mut budget_rollups_present = false;
    for row in rows {
        if let OutlineTableRow::Rollup(node_id) = row {
            if !directory_is_data_heavy(&directory_nodes[*node_id]) {
                budget_rollups_present = true;
                budget_rollup_files += directory_nodes[*node_id].stats.files;
            }
        }
    }

    let rendered_len = if budget_rollups_present {
        let envelope = crate::list_envelope::ListEnvelope::new(
            shown,
            crate::list_envelope::Total::Exact(shown + budget_rollup_files),
            crate::list_envelope::Unit::Files,
            vec![crate::list_envelope::Reason::Budget],
            &["path"],
        );
        let trailer_len = crate::list_surfaces::outline::outline_trailer_byte_len(&envelope);
        output.len() + 2 + trailer_len
    } else {
        output.len()
    };

    OutlineTable {
        table: output,
        rendered_len,
    }
}

fn directory_rollup_summary(stats: &OutlineDirectoryStats) -> String {
    let file_word = if stats.files == 1 { "file" } else { "files" };
    let dir_word = if stats.dirs == 1 { "dir" } else { "dirs" };
    if stats.dirs == 0 {
        format!("{} {file_word}", stats.files)
    } else {
        format!("{} {file_word}, {} {dir_word}", stats.files, stats.dirs)
    }
}

fn outline_many_files(
    files: &[String],
    ctx: &AppContext,
    req_id: &str,
    project_root: Option<&Path>,
) -> Result<(Vec<FileOutline>, Vec<SkippedFile>), Response> {
    let paths = files
        .iter()
        .map(|file| ctx.validate_path(req_id, Path::new(file)))
        .collect::<Result<Vec<_>, _>>()?;
    let display_root = project_root
        .filter(|root| paths.iter().all(|path| path.starts_with(root)))
        .map(Path::to_path_buf)
        .or_else(|| common_outline_path_ancestor(&paths));

    let mut file_outlines: Vec<FileOutline> = Vec::with_capacity(files.len());
    let mut skipped_files: Vec<SkippedFile> = Vec::new();
    // One parser for the whole batch, sharing the provider's symbol cache:
    // the syntax check below leaves its tree in this parser's tree cache, and
    // symbol extraction for the same file then reuses that tree instead of
    // parsing the file a second time. Other providers keep the old path.
    let mut batch_parser = ctx
        .provider()
        .as_any()
        .downcast_ref::<TreeSitterProvider>()
        .map(|provider| FileParser::with_symbol_cache(provider.symbol_cache()));
    let mut fallback_parser = FileParser::new();

    for (file, path) in files.iter().zip(paths) {
        if !path.exists() {
            skipped_files.push(SkippedFile::new(file, "file_not_found"));
            continue;
        }

        let rel_path = display_path(&path, file, display_root.as_deref());
        if let Some(reason) = outline_skip_reason(&path, batch_parser.as_mut()) {
            if let Some(parser) = batch_parser.as_mut() {
                parser.evict_parse_tree(&path);
            }
            skipped_files.push(SkippedFile::new(rel_path, reason));
            continue;
        }

        let symbols = match batch_parser.as_mut() {
            Some(parser) => parser.extract_symbols(&path),
            None => ctx.provider().list_symbols(&path),
        };
        match symbols {
            Ok(symbols) => {
                let mut entries = outline_structure_entries(
                    &path,
                    &symbols,
                    batch_parser.as_mut().unwrap_or(&mut fallback_parser),
                    ctx,
                    req_id,
                    true,
                )?;
                if detect_language(&path) == Some(LangId::Rust) {
                    let parser = batch_parser.as_mut().unwrap_or(&mut fallback_parser);
                    if let (Ok(source), Ok((tree, _))) =
                        (std::fs::read_to_string(&path), parser.parse(&path))
                    {
                        add_trait_member_previews(tree.root_node(), &source, &mut entries);
                    }
                }
                file_outlines.push(FileOutline {
                    path: rel_path,
                    entries,
                    language: detect_language(&path),
                });
            }
            Err(e) => skipped_files.push(SkippedFile::new(rel_path, outline_error_reason(&e))),
        }
        // Extraction and test classification share the validated parse tree.
        if let Some(parser) = batch_parser.as_mut() {
            parser.evict_parse_tree(&path);
        }
        fallback_parser.evict_parse_tree(&path);
    }

    Ok((file_outlines, skipped_files))
}

fn common_outline_path_ancestor(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut common = paths.first()?.parent()?.to_path_buf();
    for path in paths.iter().skip(1) {
        while !path.starts_with(&common) {
            if !common.pop() {
                return None;
            }
        }
    }
    Some(common)
}

fn discover_outline_files(directory: &Path) -> OutlineFileDiscovery {
    let mut discovery = discover_outline_files_with_options(directory, false);
    if discovery.files.len() > OUTLINE_FILE_WALK_CAP {
        discovery.files.truncate(OUTLINE_FILE_WALK_CAP);
        discovery.walk_truncated = true;
    }
    discovery
}

fn discover_outline_files_with_options(
    directory: &Path,
    breadth_first: bool,
) -> OutlineFileDiscovery {
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut entries_examined = 0;
    let mut walk_truncated = false;
    let mut collection_truncated = false;
    let mut skipped_foreign_mounts = 0usize;
    // Check mount boundaries before opening descendants. A disappearing mounted
    // child can otherwise make a directory iterator's destructor abort the daemon.
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(directory);
    if let Ok(boundary) = boundary {
        if breadth_first {
            let ignore_stack = OutlineIgnoreStack::for_target_root(directory);
            entries_examined = collect_outline_files_breadth_first_with_device_lookup(
                directory,
                &mut files,
                &mut directories,
                &mut walk_truncated,
                &mut collection_truncated,
                &mut skipped_foreign_mounts,
                &boundary,
                &ignore_stack,
            );
        } else {
            collect_outline_files_with_device_lookup(
                directory,
                &mut files,
                &mut directories,
                &mut walk_truncated,
                &mut collection_truncated,
                &mut skipped_foreign_mounts,
                &boundary,
                crate::walk_boundary::filesystem_device_id,
            );
        }
    } else {
        collection_truncated = true;
    }
    files.sort();
    directories.sort();

    OutlineFileDiscovery {
        entries_examined,
        files,
        directories,
        walk_truncated,
        collection_truncated,
        skipped_foreign_mounts,
    }
}

fn outline_target_walk_builder(directory: &Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(directory);
    // One ignore-aware walker loads the target's nested rules without reopening
    // each ancestor ignore file for every directory.
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(|left, right| left.cmp(right));
    builder
}

fn collect_outline_files_with_device_lookup<F>(
    directory: &Path,
    files: &mut Vec<String>,
    directories: &mut Vec<String>,
    walk_truncated: &mut bool,
    collection_truncated: &mut bool,
    skipped_foreign_mounts: &mut usize,
    boundary: &crate::walk_boundary::DeviceBoundary,
    device_lookup: F,
) where
    F: Fn(&Path) -> std::io::Result<Option<u64>> + Copy + Send + Sync + 'static,
{
    if files.len() >= OUTLINE_FILE_COLLECTION_CAP {
        *walk_truncated = true;
        return;
    }
    let skipped_mounts = Arc::new(AtomicUsize::new(0));
    let boundary_failed = Arc::new(AtomicBool::new(false));
    let mut builder = outline_target_walk_builder(directory);
    {
        let skipped_mounts = Arc::clone(&skipped_mounts);
        let boundary_failed = Arc::clone(&boundary_failed);
        let boundary = *boundary;
        builder.filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            let Some(file_type) = entry.file_type() else {
                return false;
            };
            if file_type.is_symlink() {
                return false;
            }
            if !file_type.is_dir() {
                return true;
            }
            if should_skip_directory(entry.path()) {
                return false;
            }
            match boundary.should_descend_with(entry.path(), device_lookup) {
                Ok(true) => true,
                Ok(false) => {
                    skipped_mounts.fetch_add(1, Ordering::Relaxed);
                    false
                }
                Err(_) => {
                    boundary_failed.store(true, Ordering::Relaxed);
                    false
                }
            }
        });
    }

    for entry in builder.build() {
        if files.len() >= OUTLINE_FILE_COLLECTION_CAP {
            *walk_truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if entry.depth() == 0 {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.into_path();
        if file_type.is_dir() {
            directories.push(path.to_string_lossy().to_string());
        } else if file_type.is_file() {
            files.push(path.to_string_lossy().to_string());
        }
    }
    *skipped_foreign_mounts += skipped_mounts.load(Ordering::Relaxed);
    if boundary_failed.load(Ordering::Relaxed) {
        *collection_truncated = true;
    }
}

fn collect_outline_files_breadth_first_with_device_lookup(
    directory: &Path,
    files: &mut Vec<String>,
    directories: &mut Vec<String>,
    walk_truncated: &mut bool,
    collection_truncated: &mut bool,
    skipped_foreign_mounts: &mut usize,
    boundary: &crate::walk_boundary::DeviceBoundary,
    root_ignore_stack: &OutlineIgnoreStack,
) -> usize {
    let root_stack = root_ignore_stack.clone();
    let mut pending = VecDeque::from([(directory.to_path_buf(), root_stack)]);
    let mut entries_examined = 0usize;

    while let Some((current, ignore_stack)) = pending.pop_front() {
        if files.len() >= OUTLINE_FILE_COLLECTION_CAP {
            *walk_truncated = true;
            return entries_examined;
        }
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        let remaining = ENTRY_BUDGET.saturating_sub(entries_examined);
        let mut entries = entries
            .take(remaining.saturating_add(1))
            .collect::<Vec<_>>();
        entries_examined += entries.len();
        if entries.len() > remaining {
            entries.truncate(remaining);
            *collection_truncated = true;
        }
        let mut entries = entries.into_iter().flatten().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.path());
        let mut child_directories = Vec::new();
        let mut child_files = Vec::new();
        for entry in entries {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                child_directories.push(entry.path());
            } else if file_type.is_file() {
                child_files.push(entry.path());
            }
        }

        // Queue directories before counting sibling files so the traversal keeps
        // its breadth-first coverage when the 10,000-file limit is reached.
        for path in child_directories {
            if should_skip_directory(&path) || ignore_stack.is_ignored(&path, true) {
                continue;
            }
            match boundary.should_descend(&path) {
                Ok(true) => {
                    directories.push(path.to_string_lossy().to_string());
                    pending.push_back((path.clone(), ignore_stack.for_child_directory(&path)));
                }
                Ok(false) => *skipped_foreign_mounts += 1,
                Err(_) => {
                    *collection_truncated = true;
                    return entries_examined;
                }
            }
        }

        for path in child_files {
            if files.len() >= OUTLINE_FILE_COLLECTION_CAP {
                *walk_truncated = true;
                return entries_examined;
            }
            if !ignore_stack.is_ignored(&path, false) {
                files.push(path.to_string_lossy().to_string());
            }
        }
        if *collection_truncated {
            return entries_examined;
        }
    }
    entries_examined
}

fn should_skip_directory(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(
        name,
        "node_modules"
            | ".git"
            | "dist"
            | "build"
            | "out"
            | ".next"
            | ".nuxt"
            | "target"
            | "__pycache__"
            | ".venv"
            | "venv"
            | "vendor"
            | ".turbo"
            | "coverage"
            | ".nyc_output"
            | ".cache"
    ) || name.starts_with('.')
}

fn display_path(path: &Path, fallback: &str, project_root: Option<&Path>) -> String {
    project_root
        .and_then(|root| path.strip_prefix(root).ok())
        .map(path_to_slash)
        .unwrap_or_else(|| fallback.to_string())
}

fn outline_skip_reason(path: &Path, parser: Option<&mut FileParser>) -> Option<&'static str> {
    if !path.is_file() {
        return Some("file_not_found");
    }

    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return Some("file_not_found"),
    };
    if metadata.len() > MAX_OUTLINE_FILE_BYTES {
        return Some("too_large");
    }

    if detect_language(path).is_none() {
        return Some("unsupported_language");
    }

    // Honest reporting: tree-sitter is fault-tolerant and `list_symbols()` will
    // return whatever symbols it can recover from a partially-broken file rather
    // than surfacing a parse error. To honor the contract that parse-error files
    // land in `skipped_files` (not the rendered outline), we still run
    // `validate_syntax()` here. With a batch parser the tree it builds is
    // reused by the symbol extraction that follows, so the file is parsed once.
    let validated = match parser {
        Some(parser) => edit::validate_syntax_with_parser(parser, path),
        None => edit::validate_syntax(path),
    };
    match validated {
        Ok(Some(false)) => Some("parse_error"),
        Ok(Some(true)) | Ok(None) => None,
        Err(e) => Some(outline_error_reason(&e)),
    }
}

fn outline_error_reason(error: &AftError) -> &'static str {
    match error.code() {
        "invalid_request" => "unsupported_language",
        "parse_error" => "parse_error",
        "file_not_found" => "file_not_found",
        "project_too_large" => "too_large",
        _ => "error",
    }
}

/// Short kind abbreviation for compact display.
fn kind_abbrev(kind: &str) -> &str {
    match kind {
        "function" => "fn",
        "variable" => "var",
        "class" => "cls",
        "interface" => "ifc",
        "type_alias" => "type",
        "enum" => "enum",
        "method" => "mth",
        "property" => "prop",
        "struct" => "st",
        "heading" => "h",
        _ => &kind[..kind.len().min(4)],
    }
}

/// Format a single entry line for multi-file mode (no signature).
fn format_entry_compact(entry: &OutlineEntry) -> String {
    if entry.kind == "test_summary" {
        return entry.signature.clone().unwrap_or_default();
    }
    let vis = if entry.exported { 'E' } else { '-' };
    let kind = kind_abbrev(&entry.kind);
    // Range is serialized 1-based, but internal Range is 0-based.
    // Add 1 to match agent-facing convention.
    let sl = entry.range.start_line + 1;
    let el = entry.range.end_line + 1;
    format!("{} {:<4} {} {}:{}", vis, kind, entry.name, sl, el)
}

/// Visibility / export keywords that, when present in a signature line, already
/// tell the reader whether the symbol is exported: Rust `pub`, Java/C#/Kotlin
/// `public`, Solidity `external`, TypeScript `export`, and friends. Used to
/// decide whether the exported-ness marker still has to be prefixed to a line.
const SIGNATURE_VISIBILITY_KEYWORDS: &[&str] = &[
    "pub",
    "public",
    "export",
    "open",
    "external",
    "internal",
    "private",
    "protected",
];

/// True when the signature text already carries a visibility/export keyword, so
/// the exported-ness marker need not be repeated as a prefix on the line.
fn signature_has_visibility(sig: &str) -> bool {
    sig.split_whitespace().any(|token| {
        // A modifier may carry a qualifier, e.g. Rust `pub(crate)`; judge the
        // keyword ahead of any `(`.
        let head = token.split('(').next().unwrap_or(token);
        SIGNATURE_VISIBILITY_KEYWORDS.contains(&head)
    })
}

/// Format a single entry line for single-file mode (with signature).
///
/// When a signature is present it already names the kind (`fn`, `class`, `def`,
/// ...) and, for languages such as Rust, the visibility (`pub`). Prefixing the
/// line with `{vis} {kind}` would duplicate information already on the line, so
/// the prefix is dropped. The one exception is exported-ness the signature text
/// does not reveal: a TypeScript `export function f()` parses with `export` on
/// the wrapping export_statement (the captured signature is just `function f()`),
/// `export { f }` lists and default-export bindings export a symbol whose
/// declaration has no `export` at all, and Go marks exports by an uppercase first
/// letter rather than a keyword. For exactly those entries — exported, with no
/// visibility keyword in the signature — a minimal `E ` marker is kept so
/// exported-ness is not silently lost.
///
/// When there is no signature (the fallback below), the prefix is the only thing
/// carrying visibility and kind, so it is retained unchanged.
pub(crate) fn format_entry_with_sig(entry: &OutlineEntry) -> String {
    if entry.kind == "test_summary" {
        return entry.signature.clone().unwrap_or_default();
    }
    let sl = entry.range.start_line + 1;
    let el = entry.range.end_line + 1;
    if let Some(ref sig) = entry.signature {
        if entry.exported && !signature_has_visibility(sig) {
            format!("E {} {}:{}", sig, sl, el)
        } else {
            format!("{} {}:{}", sig, sl, el)
        }
    } else {
        let vis = if entry.exported { 'E' } else { '-' };
        let kind = kind_abbrev(&entry.kind);
        format!("{} {:<4} {} {}:{}", vis, kind, entry.name, sl, el)
    }
}

/// Render entries recursively with indentation.
fn render_entries(entries: &[OutlineEntry], indent: usize, output: &mut String, with_sig: bool) {
    let prefix = "  ".repeat(indent);
    let member_prefix = "  ".repeat(indent + 1);
    for entry in entries {
        if with_sig {
            output.push_str(&format!("{}{}\n", prefix, format_entry_with_sig(entry)));
        } else {
            output.push_str(&format!("{}{}\n", prefix, format_entry_compact(entry)));
        }
        if !entry.members.is_empty() {
            for member in &entry.members {
                if member.kind == "test_summary" {
                    output.push_str(&format!(
                        "{}{}\n",
                        member_prefix,
                        format_entry_with_sig(member)
                    ));
                } else if with_sig {
                    output.push_str(&format!(
                        "{}.{}\n",
                        member_prefix,
                        format_entry_with_sig(member)
                    ));
                } else {
                    output.push_str(&format!(
                        "{}.{}\n",
                        member_prefix,
                        format_entry_compact(member)
                    ));
                }
                // Recurse for deeply nested members
                if !member.members.is_empty() {
                    render_entries(&member.members, indent + 2, output, with_sig);
                }
            }
        }
    }
}

/// Broad outlines retain module structure and a bounded preview of type APIs.
fn render_top_level_entries(
    entries: &[OutlineEntry],
    indent: usize,
    output: &mut String,
    with_sig: bool,
    language: Option<LangId>,
) {
    let prefix = "  ".repeat(indent);
    for entry in entries {
        if with_sig {
            output.push_str(&format!("{}{}\n", prefix, format_entry_with_sig(entry)));
        } else {
            output.push_str(&format!("{}{}\n", prefix, format_entry_compact(entry)));
        }
        if entry.kind == "module" {
            render_top_level_entries(&entry.members, indent + 1, output, with_sig, language);
        } else if matches!(
            entry.kind.as_str(),
            "struct" | "enum" | "class" | "interface"
        ) {
            let mut members = entry
                .members
                .iter()
                .filter(|member| matches!(member.kind.as_str(), "function" | "method"))
                .collect::<Vec<_>>();
            members.sort_by_key(|member| {
                (
                    !outline_member_is_public(member, language),
                    member.range.start_line,
                    member.range.start_col,
                )
            });
            for member in members.iter().take(TYPE_MEMBER_PREVIEW_CAP) {
                output.push_str(&format!("{}  .{}\n", prefix, format_entry_with_sig(member)));
            }
            let remaining = members.len().saturating_sub(TYPE_MEMBER_PREVIEW_CAP);
            if remaining > 0 {
                output.push_str(&format!("{}  ({} more)\n", prefix, remaining));
            }
        }
    }
}

fn outline_member_is_public(member: &OutlineEntry, language: Option<LangId>) -> bool {
    match language {
        Some(LangId::Python) => !member.name.starts_with('_'),
        Some(LangId::TypeScript | LangId::Tsx | LangId::JavaScript) => {
            !member.name.starts_with('#')
                && !member
                    .signature
                    .as_deref()
                    .unwrap_or_default()
                    .split_whitespace()
                    .any(|token| matches!(token, "private" | "protected"))
        }
        _ => member.exported,
    }
}

// Trait declarations are containers in the symbol table, but their required
// signatures have no symbols. Add them only to broad API previews; focused
// outlines keep their established product-member listing.
fn add_trait_member_previews(
    node: tree_sitter::Node<'_>,
    source: &str,
    entries: &mut [OutlineEntry],
) {
    if node.kind() == "trait_item" {
        let range = node_range_with_decorators(&node, source, LangId::Rust);
        fn find_trait<'a>(
            entries: &'a mut [OutlineEntry],
            range: &Range,
        ) -> Option<&'a mut OutlineEntry> {
            for entry in entries {
                if entry.kind == "interface" && &entry.range == range {
                    return Some(entry);
                }
                if entry.kind == "module" && range_contains(&entry.range, range) {
                    if let Some(found) = find_trait(&mut entry.members, range) {
                        return Some(found);
                    }
                }
            }
            None
        }
        if let (Some(entry), Some(body)) = (
            find_trait(entries, &range),
            node.child_by_field_name("body"),
        ) {
            let mut cursor = body.walk();
            for method in body
                .named_children(&mut cursor)
                .filter(|child| matches!(child.kind(), "function_item" | "function_signature_item"))
            {
                let Some(name) = method.child_by_field_name("name") else {
                    continue;
                };
                let text = node_text(source, &method);
                let first = text.lines().next().unwrap_or(text).trim_end();
                entry.members.push(OutlineEntry {
                    name: node_text(source, &name).to_string(),
                    kind: "method".into(),
                    range: node_range_with_decorators(&method, source, LangId::Rust),
                    signature: Some(
                        first
                            .strip_suffix('{')
                            .unwrap_or(first)
                            .trim_end()
                            .to_string(),
                    ),
                    exported: false,
                    members: Vec::new(),
                });
            }
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        add_trait_member_previews(child, source, entries);
    }
}

/// Format single-file outline as tree text with signatures.
fn format_single_file_tree(filename: &str, entries: &[OutlineEntry]) -> String {
    let mut output = format!("{}\n", filename);
    render_entries(entries, 1, &mut output, true);
    output
}

/// Build a directory tree structure from file paths and render as text.
///
/// Groups files by directory hierarchy and renders symbols under each file.
/// If output exceeds `max_bytes`, truncates with a narrowing hint.
struct RenderedOutline {
    text: String,
    truncated: bool,
    shown: usize,
    footer: String,
}

fn outline_structure_footer(shown: usize, total: usize, budget: bool, walk: bool) -> String {
    use crate::list_envelope::{ListEnvelope, Reason, Total, Unit};
    let mut causes = Vec::new();
    if walk {
        causes.push(Reason::Walk);
    }
    if budget {
        causes.push(Reason::Budget);
    }
    if causes.is_empty() {
        return String::new();
    }
    let total = if walk {
        Total::AtLeast(total)
    } else {
        Total::Exact(total)
    };
    crate::subc_format::render_envelope_trailer(&ListEnvelope::new(
        shown,
        total,
        Unit::Files,
        causes,
        &["path"],
    ))
    .unwrap_or_default()
}

fn format_multi_file_tree(
    file_outlines: &[FileOutline],
    max_bytes: usize,
    total_requested: usize,
    walk_incomplete: bool,
) -> RenderedOutline {
    // Build a tree of directories → files → symbols
    // Using a simple sorted-path approach with indentation
    let mut output = String::new();
    let mut truncated = false;
    let mut files_shown = 0;
    // Reserve the real list trailer, not a guessed number of bytes. Rendering
    // files atomically keeps every shown type's preview and exact remainder.
    let trailer_reserve =
        outline_structure_footer(total_requested, total_requested, true, walk_incomplete).len() + 2;

    // Sort by path for clean directory grouping
    let mut sorted: Vec<&FileOutline> = file_outlines.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    // Track directory nesting via path components
    let mut prev_parts: Vec<&str> = Vec::new();

    for fo in &sorted {
        let mut file_output = String::new();
        let parts: Vec<&str> = fo.path.split('/').collect();
        let file_name = parts.last().copied().unwrap_or(&fo.path);
        let dir_parts = &parts[..parts.len().saturating_sub(1)];

        // Find common prefix with previous path
        let common = prev_parts
            .iter()
            .zip(dir_parts.iter())
            .take_while(|(a, b)| a == b)
            .count();

        // Emit new directory levels
        for (i, part) in dir_parts.iter().enumerate().skip(common) {
            let indent = "  ".repeat(i);
            file_output.push_str(&format!("{}{}/\n", indent, part));
        }

        // Emit file name
        let file_indent = "  ".repeat(dir_parts.len());
        file_output.push_str(&format!("{}{}\n", file_indent, file_name));

        // Directory outlines are a structure map; members remain available
        // from the single-file outline to keep broad results bounded.
        render_top_level_entries(
            &fo.entries,
            dir_parts.len() + 1,
            &mut file_output,
            false,
            fo.language,
        );

        let reserve = if files_shown + 1 == sorted.len() && !walk_incomplete {
            0
        } else {
            trailer_reserve
        };
        if output.len() + file_output.len() + reserve > max_bytes {
            truncated = true;
            break;
        }
        output.push_str(&file_output);

        files_shown += 1;
        prev_parts = parts.iter().map(|s| *s).collect();
    }

    let footer = outline_structure_footer(files_shown, total_requested, truncated, walk_incomplete);
    if !footer.is_empty() {
        output.push('\n');
        output.push_str(&footer);
    }

    RenderedOutline {
        text: output,
        truncated,
        shown: files_shown,
        footer,
    }
}

pub(crate) fn symbol_to_entry(sym: &Symbol) -> OutlineEntry {
    OutlineEntry {
        name: sym.name.clone(),
        kind: serde_json::to_value(&sym.kind)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{:?}", sym.kind).to_lowercase()),
        range: sym.range.clone(),
        signature: sym.signature.clone(),
        exported: sym.exported,
        members: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::SymbolKind;

    fn summary_regression_text() -> String {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("members.rs");
        std::fs::write(
            &path,
            include_str!("../../tests/fixtures/outline_summaries/members.rs"),
        )
        .unwrap();
        multi_file_text(&[path])
    }

    #[test]
    fn outline_summary_regression_member_cap() {
        let text = summary_regression_text();
        let service = text.split("  E enum Small").next().unwrap();
        assert_eq!(
            service
                .lines()
                .filter(|line| line.trim_start().starts_with('.'))
                .count(),
            3,
            "{text}"
        );
        let members = service
            .lines()
            .filter(|line| line.trim_start().starts_with('.'))
            .collect::<Vec<_>>();
        assert_eq!(
            members,
            vec![
                "    .pub fn first(&self) {} 4:4",
                "    .pub fn second(&self) {} 5:5",
                "    .pub fn third() -> Self { Self } 9:9",
            ]
        );
        assert!(
            text.contains("    .pub fn only(&self) {} 14:14\n"),
            "{text}"
        );
    }

    #[test]
    fn outline_summary_regression_remainder() {
        let text = summary_regression_text();
        assert_eq!(
            text.lines()
                .filter(|line| line.contains(" more)"))
                .collect::<Vec<_>>(),
            vec!["    (3 more)"]
        );
    }

    #[test]
    fn outline_summary_regression_classification() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("persistence.rs");
        std::fs::write(&path, "pub fn product() {}\n#[cfg(test)]\npub(crate) mod work_counts {\n    pub fn record() {}\n    pub(crate) fn reset() {}\n    fn get() {}\n}\n").unwrap();
        assert_eq!(
            multi_file_text(&[path]),
            "persistence.rs\n  E fn   product 1:1\n  work_counts: 3 items (lines 2-7)\n"
        );
    }

    #[test]
    fn outline_tests_classifies_attributes_not_strings_or_module_names() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mixed.rs");
        std::fs::write(&path, "mod tests { pub fn real() {} }\nconst TEXT: &str = \"#[cfg(test)] mod fake {}\";\n#[cfg(not(test))]\nmod production { pub fn keep() {} }\n#[cfg(any(test, feature = \"normal\"))]\nmod both { pub fn keep_both() {} }\n#[cfg(all(test, unix))]\nmod checks { fn helper() {} }\n#[test]\nfn first() {}\npub fn middle() {}\n#[test]\nfn second() {}\n").unwrap();
        let text = multi_file_text(&[path]);
        assert!(
            text.contains("  - modu tests 1:1\n    E fn   real 1:1"),
            "{text}"
        );
        assert!(
            text.contains("keep_both") && text.contains("keep") && text.contains("middle"),
            "{text}"
        );
        assert!(text.contains("checks: 1 items (lines 7-8)"), "{text}");
        assert!(text.contains("tests: 2 items (lines 9-13)"), "{text}");
        assert!(
            !text.contains("fn   first") && !text.contains("fn   second"),
            "{text}"
        );
    }

    #[test]
    fn outline_tests_included_modules_count_helpers_and_expose_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lib.rs");
        std::fs::write(
            &path,
            "pub fn product() {}\n#[cfg(test)]\n#[path = \"checks.rs\"]\nmod checks;\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("checks.rs"),
            "struct Helper;\nfn helper() {}\n#[test]\nfn case() {}\n",
        )
        .unwrap();
        assert_eq!(
            multi_file_text(&[path]),
            "lib.rs\n  E fn   product 1:1\n  checks (path checks.rs): 3 items (lines 2-4)\n"
        );
    }

    #[test]
    fn outline_tests_implicit_modules_follow_rust_file_and_directory_layouts() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("product.rs");
        std::fs::write(&path, "#[cfg(test)]\nmod checks;\nmod outer {\n    #[cfg(test)]\n    #[path = \"custom.rs\"]\n    mod nested;\n}\n").unwrap();
        std::fs::create_dir_all(temp.path().join("product/checks")).unwrap();
        std::fs::create_dir_all(temp.path().join("product/outer")).unwrap();
        std::fs::write(
            temp.path().join("product/checks/mod.rs"),
            "#[test]\nfn case() {}\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("product/outer/custom.rs"),
            "fn helper() {}\n#[test]\nfn case() {}\n",
        )
        .unwrap();
        let text = multi_file_text(&[path]);
        assert!(
            text.contains("checks (path product/checks/mod.rs): 1 items (lines 1-2)"),
            "{text}"
        );
        assert!(
            text.contains("    nested (path product/outer/custom.rs): 2 items (lines 4-6)"),
            "{text}"
        );
    }

    #[test]
    fn outline_tests_typescript_and_javascript_top_level_blocks() {
        for extension in ["ts", "tsx", "js"] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join(format!("product.{extension}"));
            std::fs::write(&path, "export function product() {}\ndescribe('suite', () => {\n  function helper() {}\n  test('case', () => {});\n  describe('nested', () => { it('inner', () => {}); });\n});\nfunction after() {}\ntest.only('standalone', () => {});\nfunction usesDescribe() { describe('product call', () => {}); }\n").unwrap();
            let text = multi_file_text(&[path]);
            assert!(
                text.contains("describe 'suite': 5 items (lines 2-6)"),
                "{text}"
            );
            assert!(
                text.contains("test.only 'standalone': 1 items (lines 8-8)"),
                "{text}"
            );
            assert!(
                text.contains("usesDescribe") && text.contains("after"),
                "{text}"
            );
            assert!(
                !text.contains("helper") && !text.contains("nested"),
                "{text}"
            );
        }
    }

    #[test]
    fn outline_tests_python_classes_and_free_tests() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("product.py");
        std::fs::write(&path, "def product(): pass\nclass TestCache:\n    def test_read(self): pass\n    def helper(self): pass\ndef test_one(): pass\ndef middle(): pass\ndef test_two(): pass\nclass Product:\n    def test_connection(self): pass\n").unwrap();
        let text = multi_file_text(&[path]);
        assert!(text.contains("TestCache: 3 items (lines 2-4)"), "{text}");
        assert!(text.contains("tests: 2 items (lines 5-7)"), "{text}");
        assert!(
            text.contains(".def test_connection(self): pass 9:9") && text.contains("middle"),
            "{text}"
        );
    }

    #[test]
    fn outline_single_file_keeps_full_product_members_and_collapses_tests() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("members.rs");
        std::fs::write(
            &path,
            format!(
                "{}\n#[cfg(test)]\nmod checks {{ fn hidden() {{}} }}\n",
                include_str!("../../tests/fixtures/outline_summaries/members.rs")
            ),
        )
        .unwrap();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let req: RawRequest = serde_json::from_value(
            serde_json::json!({"id":"single", "command":"outline", "file":path}),
        )
        .unwrap();
        let response = serde_json::to_value(handle_outline(&req, &ctx)).unwrap();
        let text = response["text"].as_str().unwrap();
        assert_eq!(
            text.lines()
                .filter(|line| line.trim_start().starts_with('.'))
                .count(),
            7,
            "{text}"
        );
        assert!(
            text.contains("checks: 1 items") && !text.contains("hidden") && !text.contains("more)"),
            "{text}"
        );
        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&path).unwrap();
        let expanded =
            outline_structure_entries(&path, &symbols, &mut parser, &ctx, "expanded", false)
                .unwrap();
        assert!(format_single_file_tree("members.rs", &expanded).contains("hidden"));
    }

    #[test]
    fn outline_modules_keep_namesake_impls_in_their_own_scope() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("modules.rs");
        std::fs::write(&path, "struct Same;\nimpl Same { fn root(&self) {} }\nmod outer {\n    struct Same;\n    impl Same { pub fn inner(&self) {} }\n    mod nested { pub fn nested_fn() {} }\n}\n").unwrap();
        let text = multi_file_text(&[path]);
        assert!(
            text.contains("  - st   Same 1:1\n    .fn root(&self) {} 2:2"),
            "{text}"
        );
        assert!(
            text.contains("    - st   Same 4:4\n      .pub fn inner(&self) {} 5:5"),
            "{text}"
        );
        assert!(
            text.contains("    - modu nested 6:6\n      E fn   nested_fn 6:6"),
            "{text}"
        );
    }

    #[test]
    fn outline_member_previews_cover_typescript_python_go_and_traits() {
        let temp = tempfile::tempdir().unwrap();
        let ts = temp.path().join("product.ts");
        std::fs::write(&ts, "class Service {\n private hidden() {}\n public first() {}\n second() {}\n third() {}\n fourth() {}\n}\ninterface Port {\n a(): void;\n b(): void;\n}\n").unwrap();
        let text = multi_file_text(&[ts]);
        assert!(!text.contains("hidden"), "{text}");
        assert!(
            text.contains(".public first() {} 3:3")
                && text.contains(".third() {} 5:5")
                && text.contains("(2 more)"),
            "{text}"
        );
        assert!(
            text.contains(".a(): void 9:9") && text.contains(".b(): void 10:10"),
            "{text}"
        );

        let py = temp.path().join("product.py");
        std::fs::write(&py, "class Service:\n    def _hidden(self): pass\n    def first(self): pass\n    def second(self): pass\n    def third(self): pass\n    def fourth(self): pass\n").unwrap();
        let text = multi_file_text(&[py]);
        assert!(
            !text.contains("_hidden")
                && text.contains(".def first(self)")
                && text.contains("(2 more)"),
            "{text}"
        );

        let go = temp.path().join("product.go");
        std::fs::write(&go, "package product\ntype Service struct {}\nfunc (s *Service) hidden() {}\nfunc (s *Service) First() {}\nfunc (s Service) Second() {}\nfunc (s *Service) Third() {}\nfunc (s *Service) Fourth() {}\n").unwrap();
        let text = multi_file_text(&[go]);
        assert!(
            !text.contains("hidden")
                && text.contains(".E func (s *Service) First() {}")
                && text.contains("(2 more)"),
            "{text}"
        );

        let rs = temp.path().join("product.rs");
        std::fs::write(&rs, "pub trait Port {\n    fn first(&self);\n    fn second(&self);\n    fn third(&self);\n    fn fourth(&self) {}\n}\n").unwrap();
        let text = multi_file_text(&[rs]);
        assert!(
            text.contains(".fn first(&self); 2:2")
                && text.contains("(1 more)")
                && !text.contains("fourth"),
            "{text}"
        );
    }

    #[test]
    fn outline_structure_byte_budget_keeps_complete_previews_and_exact_file_trailer() {
        let temp = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for i in 0..3 {
            let path = temp.path().join(format!("file{i}.rs"));
            std::fs::write(
                &path,
                include_str!("../../tests/fixtures/outline_summaries/members.rs"),
            )
            .unwrap();
            files.push(path.display().to_string());
        }
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let (outlines, _) = outline_many_files(&files, &ctx, "budget", None).unwrap();
        let rendered = format_multi_file_tree(&outlines, 550, 3, false);
        assert!(rendered.text.len() <= 550, "{} bytes", rendered.text.len());
        assert_eq!(rendered.shown, 2, "{}", rendered.text);
        assert!(rendered.truncated);
        assert!(
            rendered
                .text
                .contains("shown 2 of 3 files (budget) · narrow: path"),
            "{}",
            rendered.text
        );
        assert_eq!(rendered.text.matches("(3 more)").count(), 2);
    }

    #[test]
    fn multi_file_outline_collapses_cfg_test_modules() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("product.rs");
        std::fs::write(
            &path,
            include_str!("../../tests/fixtures/outline_summaries/product.rs"),
        )
        .unwrap();
        assert_eq!(
            multi_file_text(&[path]),
            "product.rs\n  E fn   product 1:1\n  checks: 5 items (lines 2-13)\n  E fn   after 14:14\n"
        );
    }

    #[test]
    fn outline_summary_regression_test_free_golden_is_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test_free.ts");
        std::fs::write(
            &path,
            include_str!("../../tests/fixtures/outline_summaries/test_free.ts"),
        )
        .unwrap();
        assert_eq!(
            multi_file_text(&[path]),
            include_str!("../../tests/fixtures/outline_summaries/test_free.txt")
        );
    }

    /// Multi-file outline checks each file's syntax and then extracts its
    /// symbols. Both steps must share one parse; the syntax check used to
    /// build its own parser and tree, so every outlined file was parsed twice
    /// and every call loaded a grammar.
    #[test]
    fn multi_file_outline_parses_each_file_once() {
        use crate::parser::work_counters::{grammar_loads, tree_parses};
        let temp = tempfile::tempdir().expect("tempdir");
        let mut files = Vec::new();
        for index in 0..20 {
            let path = temp.path().join(format!("f{index:02}.ts"));
            std::fs::write(&path, format!("export function f{index}() {{}}\n")).unwrap();
            files.push(path.display().to_string());
        }
        let broken = temp.path().join("broken.ts");
        std::fs::write(&broken, "function (\n").unwrap();
        files.push(broken.display().to_string());
        let ctx = crate::context::AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "outline-parses",
            "command": "outline",
            "files": files,
        }))
        .unwrap();

        let parses_before = tree_parses();
        let loads_before = grammar_loads();
        let response = serde_json::to_value(handle_outline(&request, &ctx)).unwrap();
        let parses = tree_parses() - parses_before;
        let loads = grammar_loads() - loads_before;

        assert_eq!(parses, 21, "one parse per outlined file");
        assert!(loads <= 1, "grammar loaded {loads} times for one language");
        let text = response["text"].as_str().expect("outline text");
        for index in 0..20 {
            assert!(text.contains(&format!("f{index}")), "{text}");
        }
        let skipped = response["skipped_files"].as_array().expect("skipped files");
        assert_eq!(skipped.len(), 1, "{response}");
        assert_eq!(skipped[0]["reason"], "parse_error");
    }

    fn outline_ignore_fixture(root: &Path) {
        std::fs::create_dir_all(root.join("nested")).expect("create nested directory");
        std::fs::create_dir_all(root.join("ignored-dir")).expect("create ignored directory");
        std::fs::write(root.join(".gitignore"), "ignored.ts\nignored-dir/\n")
            .expect("write root ignore");
        std::fs::write(root.join("visible.ts"), "export function visible() {}\n")
            .expect("write visible file");
        std::fs::write(root.join("ignored.ts"), "export function ignored() {}\n")
            .expect("write ignored file");
        std::fs::write(
            root.join("ignored-dir/dependency.ts"),
            "export function dependency() {}\n",
        )
        .expect("write file in ignored directory");
        std::fs::write(root.join("nested/.gitignore"), "nested-ignored.ts\n")
            .expect("write nested ignore");
        std::fs::write(
            root.join("nested/nested-ignored.ts"),
            "export function nestedIgnored() {}\n",
        )
        .expect("write nested ignored file");
        std::fs::write(
            root.join("nested/nested-visible.ts"),
            "export function nestedVisible() {}\n",
        )
        .expect("write nested visible file");
    }

    fn outline_add_info_exclude_fixture(root: &Path) {
        std::fs::write(root.join(".git/info/exclude"), "info-excluded.ts\n")
            .expect("write repository exclude file");
        std::fs::write(
            root.join("info-excluded.ts"),
            "export function excludedByInfo() {}\n",
        )
        .expect("write info-excluded file");
    }

    fn assert_outline_discovery_honors_target_ignore_rules(discovery: OutlineFileDiscovery) {
        let paths = discovery
            .files
            .iter()
            .map(|path| Path::new(path).file_name().unwrap().to_string_lossy())
            .collect::<Vec<_>>();
        assert!(paths.iter().any(|path| path == "visible.ts"), "{paths:?}");
        assert!(
            paths.iter().any(|path| path == "nested-visible.ts"),
            "{paths:?}"
        );
        assert!(!paths.iter().any(|path| path == "ignored.ts"), "{paths:?}");
        assert!(
            !paths.iter().any(|path| path == "info-excluded.ts"),
            "repository info/exclude rule must apply: {paths:?}"
        );
        assert!(
            !paths.iter().any(|path| path == "nested-ignored.ts"),
            "{paths:?}"
        );
        assert!(
            !paths.iter().any(|path| path == "dependency.ts"),
            "ignored directories must not be traversed: {paths:?}"
        );
    }

    #[test]
    fn directory_outline_honors_target_gitignore_rules() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("project");
        let in_project = project.join("src");
        std::fs::create_dir_all(&in_project).expect("create project directory");
        outline_ignore_fixture(&in_project);
        assert_outline_discovery_honors_target_ignore_rules(discover_outline_files(&in_project));

        let external = temp.path().join("external-git");
        std::fs::create_dir_all(&external).expect("create external repository");
        let git = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&external)
            .status()
            .expect("run git init");
        assert!(git.success(), "git init failed: {git}");
        outline_ignore_fixture(&external);
        outline_add_info_exclude_fixture(&external);
        assert_outline_discovery_honors_target_ignore_rules(discover_outline_files(&external));

        let non_git = temp.path().join("standalone");
        std::fs::create_dir_all(&non_git).expect("create non-git directory");
        outline_ignore_fixture(&non_git);
        assert_outline_discovery_honors_target_ignore_rules(discover_outline_files(&non_git));
    }

    #[test]
    fn files_mode_outline_honors_target_gitignore_rules() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("project");
        let in_project = project.join("src");
        std::fs::create_dir_all(&in_project).expect("create project directory");
        outline_ignore_fixture(&in_project);
        assert_outline_discovery_honors_target_ignore_rules(discover_outline_files_for_files_mode(
            &in_project,
        ));

        let external = temp.path().join("external-git");
        std::fs::create_dir_all(&external).expect("create external repository");
        let git = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&external)
            .status()
            .expect("run git init");
        assert!(git.success(), "git init failed: {git}");
        outline_ignore_fixture(&external);
        outline_add_info_exclude_fixture(&external);
        assert_outline_discovery_honors_target_ignore_rules(discover_outline_files_for_files_mode(
            &external,
        ));

        let non_git = temp.path().join("standalone");
        std::fs::create_dir_all(&non_git).expect("create non-git directory");
        outline_ignore_fixture(&non_git);
        assert_outline_discovery_honors_target_ignore_rules(discover_outline_files_for_files_mode(
            &non_git,
        ));
    }

    #[test]
    fn multi_file_outline_keeps_rust_impl_methods_nested_in_structure_map() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = r#"
pub struct Widget;
pub trait Render { fn render(&self); }
impl Widget { pub fn new() -> Self { Self } }
impl Render for Widget { pub fn render(&self) {} }
pub struct GenericBox<T> { value: T }
pub trait Boxed { fn boxed(&self); }
impl<T> Boxed for GenericBox<T> { pub fn boxed(&self) {} }
"#;
        let path = temp.path().join("sample.rs");
        std::fs::write(&path, source).expect("write Rust outline fixture");

        let symbols = parsed_symbols("rs", source);
        let tree = build_outline_tree(&symbols);
        let output = multi_file_text(&[path]);
        for method_name in ["new", "render", "boxed"] {
            assert!(
                output
                    .lines()
                    .any(|line| line.starts_with("    .") && line.contains(method_name))
                    && !output
                        .lines()
                        .any(|line| line.starts_with("  - mth") && line.contains(method_name)),
                "structure-map previews must nest {method_name} under its type:\n{output}"
            );
        }

        let single_file = format_single_file_tree("sample.rs", &tree);
        let lines = single_file.lines().collect::<Vec<_>>();
        for (type_name, method_name) in [
            ("Widget", "new"),
            ("Widget", "render"),
            ("GenericBox", "boxed"),
        ] {
            let owner_prefix = format!("  pub struct {type_name}");
            let owner_index = lines
                .iter()
                .position(|line| line.starts_with(&owner_prefix))
                .unwrap_or_else(|| panic!("missing owner {type_name}:\n{single_file}"));
            let next_type = lines
                .iter()
                .enumerate()
                .skip(owner_index + 1)
                .find(|(_, line)| line.starts_with("  ") && !line.starts_with("    "))
                .map(|(index, _)| index)
                .unwrap_or(lines.len());
            assert!(
                lines[owner_index + 1..next_type]
                    .iter()
                    .any(|line| line.starts_with("    .") && line.contains(method_name)),
                "single-file outline should nest {method_name} under {type_name}:\n{single_file}"
            );
        }
    }

    #[test]
    fn multi_file_outline_does_not_leak_typescript_class_methods() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("sample.ts");
        std::fs::write(
            &path,
            "export class Greeter { greet(name: string) { return name; } }\n",
        )
        .expect("write TypeScript outline fixture");

        let output = multi_file_text(&[path]);
        assert!(output.contains("  E cls  Greeter "), "{output}");
        assert!(
            output
                .lines()
                .any(|line| line.starts_with("    .") && line.contains("greet")),
            "structure-map output must keep class methods nested:\n{output}"
        );
    }

    #[test]
    fn multi_file_outline_roots_paths_at_their_common_ancestor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let left = temp.path().join("left").join("first.ts");
        let right = temp.path().join("right").join("second.ts");
        std::fs::create_dir_all(left.parent().unwrap()).expect("create left directory");
        std::fs::create_dir_all(right.parent().unwrap()).expect("create right directory");
        std::fs::write(&left, "export function first() {}\n").unwrap();
        std::fs::write(&right, "export function second() {}\n").unwrap();

        let output = multi_file_text(&[left, right]);
        assert!(output.starts_with("left/\n  first.ts\n"), "{output}");
        assert!(output.contains("right/\n  second.ts\n"), "{output}");
        assert!(
            !output.contains(temp.path().to_string_lossy().as_ref()),
            "{output}"
        );
    }

    fn multi_file_text(paths: &[std::path::PathBuf]) -> String {
        let files = paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let ctx = crate::context::AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let (outlines, skipped) = outline_many_files(&files, &ctx, "outline-fixture", None)
            .expect("outline fixture files");
        assert!(skipped.is_empty(), "{skipped:?}");
        format_multi_file_tree(&outlines, 30 * 1024, files.len(), false).text
    }

    #[test]
    fn files_mode_skips_git_and_dependency_directories_before_counting_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let git = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(temp.path())
            .status()
            .expect("run git init");
        assert!(git.success(), "git init failed: {git}");

        for path in [
            "node_modules/package/deep/dependency.ts",
            "src/index.ts",
            "src/other.ts",
        ] {
            let file = temp.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).expect("create fixture directory");
            std::fs::write(file, "export function fixture() {}\n").expect("write fixture");
        }

        let discovery = discover_outline_files_with_options(temp.path(), true);
        // Compare with forward slashes so the same assertions hold on Windows.
        let files = discovery
            .files
            .iter()
            .map(|path| path.replace('\\', "/"))
            .collect::<Vec<_>>();
        assert!(
            files.iter().any(|path| path.ends_with("src/index.ts")),
            "source file missing: {:?}",
            discovery.files
        );
        assert!(
            files.iter().any(|path| path.ends_with("src/other.ts")),
            "source file missing: {files:?}"
        );
        assert!(
            files
                .iter()
                .all(|path| !path.contains("/.git/") && !path.contains("/node_modules/")),
            "Git metadata or dependencies were traversed: {files:?}"
        );
        assert_eq!(
            discovery.entries_examined, 5,
            "count the three root entries and two source files, not skipped directory contents"
        );
    }

    #[test]
    fn outline_walk_skips_and_reports_injected_foreign_mount() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let local = root.join("local");
        let foreign = root.join("foreign");
        std::fs::create_dir_all(&local).expect("create local directory");
        std::fs::create_dir_all(&foreign).expect("create foreign directory");
        std::fs::write(local.join("keep.rs"), "pub fn keep() {}\n").expect("write local file");
        std::fs::write(foreign.join("skip.rs"), "pub fn skip() {}\n").expect("write foreign file");

        let boundary = crate::walk_boundary::DeviceBoundary::from_device_for_test(41);
        let mut files = Vec::new();
        let mut directories = Vec::new();
        let mut walk_truncated = false;
        let mut collection_truncated = false;
        let mut skipped_foreign_mounts = 0usize;
        let lookup = |path: &Path| {
            Ok(Some(
                if path.file_name().is_some_and(|name| name == "foreign") {
                    99
                } else {
                    41
                },
            ))
        };

        collect_outline_files_with_device_lookup(
            &root,
            &mut files,
            &mut directories,
            &mut walk_truncated,
            &mut collection_truncated,
            &mut skipped_foreign_mounts,
            &boundary,
            lookup,
        );

        assert_eq!(skipped_foreign_mounts, 1, "foreign mount is disclosed");
        assert!(
            !walk_truncated,
            "a foreign mount is not the file-count fence"
        );
        assert!(
            !collection_truncated,
            "a known foreign mount is not an I/O failure"
        );
        // Component-based comparison: `files` holds native path strings, so a
        // str::ends_with("local/keep.rs") literal would never match Windows
        // backslash separators.
        let keep = Path::new("local").join("keep.rs");
        let skip = Path::new("foreign").join("skip.rs");
        assert!(files.iter().any(|path| Path::new(path).ends_with(&keep)));
        assert!(
            !files.iter().any(|path| Path::new(path).ends_with(&skip)),
            "foreign-mount contents must not be traversed"
        );
    }

    fn make_symbol(
        name: &str,
        kind: SymbolKind,
        parent: Option<&str>,
        scope_chain: Vec<&str>,
        exported: bool,
    ) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            range: Range {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 0,
            },
            signature: None,
            scope_chain: scope_chain.into_iter().map(String::from).collect(),
            exported,
            parent: parent.map(String::from),
        }
    }

    fn build_outline_tree_reference(symbols: &[Symbol]) -> Vec<OutlineEntry> {
        let mut top_level = Vec::new();
        let mut children = Vec::new();

        for sym in symbols {
            if sym.parent.is_none() {
                top_level.push(symbol_to_entry(sym));
            } else {
                children.push(sym);
            }
        }

        for child in children {
            let entry = symbol_to_entry(child);
            let scope = &child.scope_chain;
            if scope.is_empty() {
                top_level.push(entry);
                continue;
            }
            if !insert_at_scope_reference(&mut top_level, scope, entry.clone()) {
                let parent_scope = child.parent.as_ref().map(std::slice::from_ref);
                if !parent_scope.is_some_and(|scope| {
                    insert_at_scope_reference(&mut top_level, scope, entry.clone())
                }) {
                    top_level.push(entry);
                }
            }
        }

        top_level
    }

    // Frozen pre-index implementation used as the differential oracle.
    fn insert_at_scope_reference(
        entries: &mut Vec<OutlineEntry>,
        scope_chain: &[String],
        entry: OutlineEntry,
    ) -> bool {
        if scope_chain.is_empty() {
            return false;
        }

        let target_name = &scope_chain[0];
        for existing in entries {
            if existing.name == *target_name {
                if scope_chain.len() == 1 {
                    existing.members.push(entry);
                    return true;
                }
                return insert_at_scope_reference(&mut existing.members, &scope_chain[1..], entry);
            }
        }
        false
    }

    fn assert_indexed_matches_reference(case: &str, symbols: &[Symbol]) -> Vec<OutlineEntry> {
        let actual = build_outline_tree(symbols);
        let expected = build_outline_tree_reference(symbols);
        assert_eq!(
            serde_json::to_vec(&actual).expect("serialize indexed outline"),
            serde_json::to_vec(&expected).expect("serialize reference outline"),
            "indexed outline diverged for {case}"
        );
        actual
    }

    fn parsed_symbols(extension: &str, source: &str) -> Vec<Symbol> {
        use crate::parser::FileParser;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("fixture.{extension}"));
        std::fs::write(&path, source).expect("write parser fixture");
        FileParser::new()
            .extract_symbols(&path)
            .expect("extract fixture symbols")
    }

    #[test]
    fn indexed_outline_matches_reference_for_first_match_and_insertion_order() {
        let mut symbols = vec![make_symbol(
            "Duplicate",
            SymbolKind::Class,
            None,
            vec![],
            false,
        )];
        for index in 0..OUTLINE_SCOPE_INDEX_THRESHOLD {
            symbols.push(make_symbol(
                &format!("Filler{index}"),
                SymbolKind::Class,
                None,
                vec![],
                false,
            ));
        }
        symbols.extend([
            make_symbol("Duplicate", SymbolKind::Class, None, vec![], true),
            make_symbol(
                "firstChild",
                SymbolKind::Method,
                Some("Duplicate"),
                vec!["Duplicate"],
                false,
            ),
            make_symbol(
                "secondChild",
                SymbolKind::Method,
                Some("Duplicate"),
                vec!["Duplicate"],
                false,
            ),
        ]);

        let tree = assert_indexed_matches_reference("duplicate siblings", &symbols);
        let duplicates = tree
            .iter()
            .filter(|entry| entry.name == "Duplicate")
            .collect::<Vec<_>>();
        assert_eq!(
            duplicates[0]
                .members
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["firstChild", "secondChild"]
        );
        assert!(
            duplicates[1].members.is_empty(),
            "second duplicate stays unused"
        );
    }

    #[test]
    fn indexed_outline_matches_reference_for_dynamic_deep_parents() {
        let symbols = vec![
            make_symbol("Outer", SymbolKind::Class, None, vec![], false),
            // The parent does not exist yet, so established ordering semantics
            // promote this entry and never reparent it later.
            make_symbol(
                "earlyLeaf",
                SymbolKind::Method,
                Some("Inner"),
                vec!["Outer", "Inner"],
                false,
            ),
            make_symbol(
                "Inner",
                SymbolKind::Class,
                Some("Outer"),
                vec!["Outer"],
                false,
            ),
            make_symbol(
                "lateLeaf",
                SymbolKind::Method,
                Some("Inner"),
                vec!["Outer", "Inner"],
                false,
            ),
            make_symbol(
                "Deep",
                SymbolKind::Class,
                Some("Inner"),
                vec!["Outer", "Inner"],
                false,
            ),
            make_symbol(
                "deepLeaf",
                SymbolKind::Method,
                Some("Deep"),
                vec!["Outer", "Inner", "Deep"],
                false,
            ),
        ];

        let tree = assert_indexed_matches_reference("deep dynamic parents", &symbols);
        assert_eq!(tree[1].name, "earlyLeaf");
        let inner = &tree[0].members[0];
        assert_eq!(inner.name, "Inner");
        assert_eq!(inner.members[0].name, "lateLeaf");
        assert_eq!(inner.members[1].members[0].name, "deepLeaf");
    }

    #[test]
    fn indexed_outline_matches_reference_for_fallbacks_and_orphans() {
        let symbols = vec![
            make_symbol("Widget", SymbolKind::Struct, None, vec![], true),
            make_symbol(
                "fmt",
                SymbolKind::Method,
                Some("Widget"),
                vec!["Display for Widget"],
                true,
            ),
            make_symbol(
                "Orphan",
                SymbolKind::Class,
                Some("Missing"),
                vec!["Missing"],
                false,
            ),
            make_symbol(
                "adoptedLater",
                SymbolKind::Method,
                Some("Orphan"),
                vec!["Orphan"],
                false,
            ),
        ];

        let tree = assert_indexed_matches_reference("fallback and orphan ladder", &symbols);
        assert_eq!(tree[0].members[0].name, "fmt");
        assert_eq!(tree[1].name, "Orphan");
        assert_eq!(tree[1].members[0].name, "adoptedLater");
    }

    #[test]
    fn indexed_outline_matches_reference_at_scale() {
        const PARENTS: usize = 2_048;
        let mut symbols = Vec::with_capacity(PARENTS * 2);
        for index in 0..PARENTS {
            symbols.push(make_symbol(
                &format!("Container{index:04}"),
                SymbolKind::Class,
                None,
                vec![],
                true,
            ));
        }
        for index in 0..PARENTS {
            let parent = format!("Container{index:04}");
            symbols.push(make_symbol(
                &format!("method{index:04}"),
                SymbolKind::Method,
                Some(&parent),
                vec![&parent],
                false,
            ));
        }

        assert_indexed_matches_reference("one child per parent at scale", &symbols);
    }

    #[test]
    fn indexed_outline_matches_reference_for_typescript_and_python() {
        let typescript = parsed_symbols(
            "ts",
            "class Outer {\n  method(): void {}\n  classField = 1;\n}\n",
        );
        assert!(
            typescript.iter().any(|symbol| symbol.parent.is_some()),
            "TypeScript fixture must exercise child insertion"
        );
        assert_indexed_matches_reference("TypeScript parser output", &typescript);

        let python = parsed_symbols(
            "py",
            "class Outer:\n    class Inner:\n        def leaf(self):\n            pass\n\n    def outer(self):\n        pass\n",
        );
        assert!(
            python.iter().any(|symbol| symbol.parent.is_some()),
            "Python fixture must exercise child insertion"
        );
        assert_indexed_matches_reference("Python parser output", &python);
    }

    #[test]
    fn flat_symbols_stay_flat() {
        let symbols = vec![
            make_symbol("greet", SymbolKind::Function, None, vec![], true),
            make_symbol("Config", SymbolKind::Interface, None, vec![], true),
        ];
        let tree = build_outline_tree(&symbols);
        assert_eq!(tree.len(), 2);
        assert!(tree[0].members.is_empty());
        assert!(tree[1].members.is_empty());
    }

    #[test]
    fn methods_nest_under_class() {
        let symbols = vec![
            make_symbol("UserService", SymbolKind::Class, None, vec![], true),
            make_symbol(
                "getUser",
                SymbolKind::Method,
                Some("UserService"),
                vec!["UserService"],
                false,
            ),
            make_symbol(
                "addUser",
                SymbolKind::Method,
                Some("UserService"),
                vec!["UserService"],
                false,
            ),
        ];
        let tree = build_outline_tree(&symbols);
        assert_eq!(tree.len(), 1, "methods should not appear at top level");
        assert_eq!(tree[0].name, "UserService");
        assert_eq!(tree[0].members.len(), 2);
        assert_eq!(tree[0].members[0].name, "getUser");
        assert_eq!(tree[0].members[1].name, "addUser");
    }

    #[test]
    fn parent_fallback_nests_trait_impl_methods_under_type() {
        let symbols = vec![
            make_symbol("Widget", SymbolKind::Struct, None, vec![], true),
            make_symbol(
                "fmt",
                SymbolKind::Method,
                Some("Widget"),
                vec!["Display for Widget"],
                true,
            ),
        ];
        let tree = build_outline_tree(&symbols);
        assert_eq!(
            tree.len(),
            1,
            "trait impl method should nest under parent type"
        );
        assert_eq!(tree[0].name, "Widget");
        assert_eq!(tree[0].members.len(), 1);
        assert_eq!(tree[0].members[0].name, "fmt");
    }

    #[test]
    fn methods_not_duplicated_at_top_level() {
        let symbols = vec![
            make_symbol("Foo", SymbolKind::Class, None, vec![], false),
            make_symbol("bar", SymbolKind::Method, Some("Foo"), vec!["Foo"], false),
        ];
        let tree = build_outline_tree(&symbols);
        // "bar" must NOT appear at top level
        assert!(
            tree.iter().all(|e| e.name != "bar"),
            "method should not be at top level"
        );
        assert_eq!(tree[0].members.len(), 1);
    }

    #[test]
    fn multi_level_nesting_python() {
        // OuterClass → InnerClass → inner_method
        let symbols = vec![
            make_symbol("OuterClass", SymbolKind::Class, None, vec![], false),
            make_symbol(
                "InnerClass",
                SymbolKind::Class,
                Some("OuterClass"),
                vec!["OuterClass"],
                false,
            ),
            make_symbol(
                "inner_method",
                SymbolKind::Method,
                Some("InnerClass"),
                vec!["OuterClass", "InnerClass"],
                false,
            ),
            make_symbol(
                "outer_method",
                SymbolKind::Method,
                Some("OuterClass"),
                vec!["OuterClass"],
                false,
            ),
        ];
        let tree = build_outline_tree(&symbols);
        assert_eq!(tree.len(), 1, "only OuterClass at top level");

        let outer = &tree[0];
        assert_eq!(outer.name, "OuterClass");
        assert_eq!(outer.members.len(), 2, "InnerClass + outer_method");

        let inner = outer
            .members
            .iter()
            .find(|m| m.name == "InnerClass")
            .unwrap();
        assert_eq!(inner.members.len(), 1);
        assert_eq!(inner.members[0].name, "inner_method");
    }

    #[test]
    fn all_symbol_kinds_handled() {
        let symbols = vec![
            make_symbol("f", SymbolKind::Function, None, vec![], false),
            make_symbol("C", SymbolKind::Class, None, vec![], false),
            make_symbol("m", SymbolKind::Method, Some("C"), vec!["C"], false),
            make_symbol("S", SymbolKind::Struct, None, vec![], false),
            make_symbol("I", SymbolKind::Interface, None, vec![], false),
            make_symbol("E", SymbolKind::Enum, None, vec![], false),
            make_symbol("T", SymbolKind::TypeAlias, None, vec![], false),
        ];
        let tree = build_outline_tree(&symbols);

        // 6 top-level (method is nested under class)
        assert_eq!(tree.len(), 6);

        let kinds: Vec<&str> = tree.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"function"));
        assert!(kinds.contains(&"class"));
        assert!(kinds.contains(&"struct"));
        assert!(kinds.contains(&"interface"));
        assert!(kinds.contains(&"enum"));
        assert!(kinds.contains(&"type_alias"));

        // Method under class
        let class_entry = tree.iter().find(|e| e.name == "C").unwrap();
        assert_eq!(class_entry.members.len(), 1);
        assert_eq!(class_entry.members[0].kind, "method");
    }

    #[test]
    fn exported_flag_preserved() {
        let symbols = vec![
            make_symbol("exported_fn", SymbolKind::Function, None, vec![], true),
            make_symbol("internal_fn", SymbolKind::Function, None, vec![], false),
        ];
        let tree = build_outline_tree(&symbols);
        let exported = tree.iter().find(|e| e.name == "exported_fn").unwrap();
        let internal = tree.iter().find(|e| e.name == "internal_fn").unwrap();
        assert!(exported.exported);
        assert!(!internal.exported);
    }

    #[test]
    fn orphan_child_promoted_to_top_level() {
        // A method whose parent doesn't exist in the list
        let symbols = vec![make_symbol(
            "orphan",
            SymbolKind::Method,
            Some("MissingParent"),
            vec!["MissingParent"],
            false,
        )];
        let tree = build_outline_tree(&symbols);
        assert_eq!(tree.len(), 1, "orphan should be promoted to top level");
        assert_eq!(tree[0].name, "orphan");
    }

    fn sig_entry(
        name: &str,
        kind: &str,
        signature: Option<&str>,
        exported: bool,
        start_line: u32,
        end_line: u32,
    ) -> OutlineEntry {
        OutlineEntry {
            name: name.to_string(),
            kind: kind.to_string(),
            range: Range {
                start_line,
                start_col: 0,
                end_line,
                end_col: 0,
            },
            signature: signature.map(String::from),
            exported,
            members: Vec::new(),
        }
    }

    #[test]
    fn signature_lines_drop_the_redundant_vis_kind_prefix() {
        // Rust `pub fn`: visibility and kind are both in the signature, so no prefix.
        assert_eq!(
            format_entry_with_sig(&sig_entry(
                "resolve",
                "function",
                Some("pub fn resolve(id: u64) -> Result<()>"),
                true,
                9,
                20,
            )),
            "pub fn resolve(id: u64) -> Result<()> 10:21"
        );
        // Rust private `fn`: not exported, signature carries the kind; no prefix.
        assert_eq!(
            format_entry_with_sig(&sig_entry(
                "helper",
                "function",
                Some("fn helper()"),
                false,
                0,
                2
            )),
            "fn helper() 1:3"
        );
        // Python `def`: no export concept, so no marker at all.
        assert_eq!(
            format_entry_with_sig(&sig_entry(
                "compute",
                "function",
                Some("def compute(value):"),
                false,
                4,
                7,
            )),
            "def compute(value): 5:8"
        );
    }

    #[test]
    fn exported_without_visibility_keyword_keeps_a_minimal_marker() {
        // TypeScript `export function greet()` parses with `export` on the wrapping
        // export_statement, so the captured signature is just `function greet()`;
        // the `E` marker is the only signal of exported-ness and must be kept.
        assert_eq!(
            format_entry_with_sig(&sig_entry(
                "greet",
                "function",
                Some("function greet(name: string): string"),
                true,
                0,
                2,
            )),
            "E function greet(name: string): string 1:3"
        );
        // Go exports by an uppercase first letter, with no keyword in the signature.
        assert_eq!(
            format_entry_with_sig(&sig_entry(
                "Parse",
                "function",
                Some("func Parse(input string) (*Tree, error)"),
                true,
                0,
                5,
            )),
            "E func Parse(input string) (*Tree, error) 1:6"
        );
        // A signature that already carries a visibility keyword needs no marker,
        // even when exported.
        assert_eq!(
            format_entry_with_sig(&sig_entry(
                "run",
                "method",
                Some("public void run()"),
                true,
                0,
                1
            )),
            "public void run() 1:2"
        );
    }

    #[test]
    fn no_signature_fallback_keeps_the_vis_kind_prefix() {
        // Without a signature the prefix is the only carrier of visibility and
        // kind, so it is retained verbatim.
        assert_eq!(
            format_entry_with_sig(&sig_entry("answer", "variable", None, true, 0, 0)),
            "E var  answer 1:1"
        );
        assert_eq!(
            format_entry_with_sig(&sig_entry("local", "variable", None, false, 1, 1)),
            "- var  local 2:2"
        );
    }

    #[test]
    fn dropping_the_prefix_removes_exactly_the_prefix_bytes() {
        // Golden fixture: the new line must be the old line minus the
        // `{vis} {kind:<4} ` prefix, with the signature and range bytes — and
        // therefore the line positions — left untouched.
        let entry = sig_entry(
            "resolve",
            "function",
            Some("pub fn resolve(id: u64)"),
            true,
            9,
            20,
        );
        let new = format_entry_with_sig(&entry);
        let old = format!("E {:<4} {} {}:{}", "fn", "pub fn resolve(id: u64)", 10, 21);
        assert_eq!(old, "E fn   pub fn resolve(id: u64) 10:21");
        assert_eq!(new, "pub fn resolve(id: u64) 10:21");
        // Exactly the prefix bytes are removed; nothing else moves. Reintroducing
        // the unconditional prefix makes new == old and this delta drops to zero.
        assert_eq!(old.len() - new.len(), "E fn   ".len());
        assert!(
            old.ends_with(&new),
            "only the prefix may change: {old:?} -> {new:?}"
        );
    }

    #[test]
    fn signature_visibility_detection() {
        assert!(signature_has_visibility("pub fn f()"));
        assert!(signature_has_visibility("pub(crate) fn f()"));
        assert!(signature_has_visibility("public void f()"));
        assert!(signature_has_visibility("export function f()"));
        assert!(signature_has_visibility("external function f()"));
        // A symbol name that merely contains a keyword substring is not a marker.
        assert!(!signature_has_visibility("fn publish()"));
        assert!(!signature_has_visibility(
            "function greet(name: string): string"
        ));
        assert!(!signature_has_visibility("def compute(value):"));
        assert!(!signature_has_visibility("func Parse(input string)"));
    }

    fn outline_file_entry_for_test(
        path: &str,
        language: &str,
        symbols: usize,
        lines: Option<usize>,
        data_doc: bool,
    ) -> OutlineFileEntry {
        OutlineFileEntry {
            path: path.to_string(),
            language: language.to_string(),
            symbols: Some(symbols),
            lines,
            absolute_path: PathBuf::from(path),
            data_doc,
        }
    }

    #[test]
    fn outline_file_line_count_matches_text_and_binary_contract() {
        let temp = tempfile::tempdir().expect("tempdir");
        let terminated = temp.path().join("terminated.txt");
        let unterminated = temp.path().join("unterminated.txt");
        let empty = temp.path().join("empty.txt");
        let binary = temp.path().join("binary.dat");
        std::fs::write(&terminated, b"a\nb\nc\n").expect("write terminated");
        std::fs::write(&unterminated, b"a\nb\nc").expect("write unterminated");
        std::fs::write(&empty, b"").expect("write empty");
        std::fs::write(&binary, [0, 159, 146, 150, 0, 1]).expect("write binary");

        assert_eq!(
            inspect_outline_file_content(&terminated)
                .expect("inspect terminated")
                .lines,
            Some(3)
        );
        assert_eq!(
            inspect_outline_file_content(&unterminated)
                .expect("inspect unterminated")
                .lines,
            Some(3)
        );
        assert_eq!(
            inspect_outline_file_content(&empty)
                .expect("inspect empty")
                .lines,
            Some(0)
        );
        let binary_stats = inspect_outline_file_content(&binary).expect("inspect binary");
        assert!(binary_stats.binary);
        assert_eq!(binary_stats.lines, None);
    }

    #[test]
    fn outline_rows_put_code_before_data_files() {
        let files = vec![
            outline_file_entry_for_test("docs/readme.md", "markdown", 1, Some(4), true),
            outline_file_entry_for_test("docs/lib.rs", "rust", 2, Some(8), false),
        ];
        let mut directories = vec![OutlineDirectoryNode {
            path: String::new(),
            depth: 0,
            direct_files: vec![0, 1],
            children: Vec::new(),
            stats: OutlineDirectoryStats::default(),
        }];
        aggregate_outline_directory(0, &mut directories, &files);

        assert_eq!(
            plan_outline_file_rows(&[0], &directories, &files, 30 * 1024),
            vec![OutlineTableRow::File(1), OutlineTableRow::File(0)]
        );
    }

    #[test]
    fn data_only_leaf_stays_one_rollup_with_summed_lines() {
        let files = (0..3)
            .map(|index| {
                outline_file_entry_for_test(
                    &format!("schema/json/{index}.json"),
                    "json",
                    0,
                    Some(2),
                    true,
                )
            })
            .collect::<Vec<_>>();
        let mut directories = vec![
            OutlineDirectoryNode {
                path: String::new(),
                depth: 0,
                direct_files: Vec::new(),
                children: vec![1],
                stats: OutlineDirectoryStats::default(),
            },
            OutlineDirectoryNode {
                path: "schema".to_string(),
                depth: 1,
                direct_files: Vec::new(),
                children: vec![2],
                stats: OutlineDirectoryStats::default(),
            },
            OutlineDirectoryNode {
                path: "schema/json".to_string(),
                depth: 2,
                direct_files: vec![0, 1, 2],
                children: Vec::new(),
                stats: OutlineDirectoryStats::default(),
            },
        ];
        aggregate_outline_directory(0, &mut directories, &files);

        let rows = plan_outline_file_rows(&[0], &directories, &files, 30 * 1024);
        assert_eq!(rows, vec![OutlineTableRow::Rollup(2)]);
        let text = format_files_table(&rows, &directories, &files, 30 * 1024);
        assert!(text.contains("schema/json/"));
        assert!(text.contains("3 files"));
        assert!(text.contains("6 lines"));
        assert!(!text.contains("syms"), "rollup row: {text}");
        assert!(!text.contains("shown as a rollup"));
        assert!(!text.contains(".json  "));
    }

    #[test]
    fn cheapest_same_level_expansion_buys_breadth_before_large_directory() {
        let files = vec![
            outline_file_entry_for_test(
                "z-small/src/long-breadth-marker/lib.rs",
                "rust",
                1,
                Some(1),
                false,
            ),
            outline_file_entry_for_test("a-large/a.rs", "rust", 1, Some(1), false),
            outline_file_entry_for_test("a-large/b.rs", "rust", 1, Some(1), false),
            outline_file_entry_for_test("docs/readme.md", "markdown", 0, Some(1), true),
        ];
        let mut directories = vec![
            OutlineDirectoryNode {
                path: String::new(),
                depth: 0,
                direct_files: Vec::new(),
                children: vec![1, 2, 3],
                stats: OutlineDirectoryStats::default(),
            },
            OutlineDirectoryNode {
                path: "z-small".to_string(),
                depth: 1,
                direct_files: Vec::new(),
                children: vec![4],
                stats: OutlineDirectoryStats::default(),
            },
            OutlineDirectoryNode {
                path: "a-large".to_string(),
                depth: 1,
                direct_files: vec![1, 2],
                children: Vec::new(),
                stats: OutlineDirectoryStats::default(),
            },
            OutlineDirectoryNode {
                path: "docs".to_string(),
                depth: 1,
                direct_files: vec![3],
                children: Vec::new(),
                stats: OutlineDirectoryStats::default(),
            },
            OutlineDirectoryNode {
                path: "z-small/src/long-breadth-marker".to_string(),
                depth: 2,
                direct_files: vec![0],
                children: Vec::new(),
                stats: OutlineDirectoryStats::default(),
            },
        ];
        aggregate_outline_directory(0, &mut directories, &files);
        let small_only = vec![
            OutlineTableRow::Rollup(2),
            OutlineTableRow::Rollup(3),
            OutlineTableRow::Rollup(4),
        ];
        let large_only = vec![
            OutlineTableRow::File(1),
            OutlineTableRow::File(2),
            OutlineTableRow::Rollup(3),
            OutlineTableRow::Rollup(1),
        ];
        let both = vec![
            OutlineTableRow::File(1),
            OutlineTableRow::File(2),
            OutlineTableRow::Rollup(3),
            OutlineTableRow::Rollup(4),
        ];
        let mut budget = 0;
        for _ in 0..8 {
            let required = format_files_table(&small_only, &directories, &files, budget)
                .len()
                .max(format_files_table(&large_only, &directories, &files, budget).len());
            if required == budget {
                break;
            }
            budget = required;
        }
        assert!(format_files_table(&small_only, &directories, &files, budget).len() <= budget);
        assert!(format_files_table(&large_only, &directories, &files, budget).len() <= budget);
        assert!(format_files_table(&both, &directories, &files, budget).len() > budget);

        assert_eq!(
            plan_outline_file_rows(&[0], &directories, &files, budget),
            small_only
        );
    }

    #[test]
    fn top_level_rows_are_never_cut_by_the_budget() {
        let files = vec![
            outline_file_entry_for_test("a/lib.rs", "rust", 1, Some(1), false),
            outline_file_entry_for_test("b/lib.rs", "rust", 1, Some(1), false),
        ];
        let mut directories = vec![
            OutlineDirectoryNode {
                path: String::new(),
                depth: 0,
                direct_files: Vec::new(),
                children: vec![1, 2],
                stats: OutlineDirectoryStats::default(),
            },
            OutlineDirectoryNode {
                path: "a".to_string(),
                depth: 1,
                direct_files: vec![0],
                children: Vec::new(),
                stats: OutlineDirectoryStats::default(),
            },
            OutlineDirectoryNode {
                path: "b".to_string(),
                depth: 1,
                direct_files: vec![1],
                children: Vec::new(),
                stats: OutlineDirectoryStats::default(),
            },
        ];
        aggregate_outline_directory(0, &mut directories, &files);

        let rows = plan_outline_file_rows(&[0], &directories, &files, 1);
        assert_eq!(
            rows,
            vec![OutlineTableRow::Rollup(1), OutlineTableRow::Rollup(2)]
        );
        let text = format_files_table(&rows, &directories, &files, 1);
        assert!(text.contains("a/"));
        assert!(text.contains("b/"));
        assert!(
            text.len() > 1,
            "level zero deliberately exceeds a tiny budget"
        );
    }

    #[test]
    fn binary_file_row_uses_a_dash_for_lines() {
        let files = vec![outline_file_entry_for_test(
            "assets/blob.dat",
            "binary",
            0,
            None,
            true,
        )];
        let text = format_files_table(&[OutlineTableRow::File(0)], &[], &files, 30 * 1024);
        assert!(text.contains("      - lines"), "binary row: {text}");
    }

    /// Manual release-mode probe for the directory walk paid by one outline
    /// `files: true` request on a realistic 10k-file monorepo.
    #[test]
    #[ignore = "manual release-mode outline files performance probe"]
    fn outline_files_walk_perf_probe() {
        const DIRECTORIES: usize = 100;
        const FILES_PER_DIRECTORY: usize = 100;
        const SAMPLES: usize = 9;
        const ITERATIONS: usize = 3;

        let temp = tempfile::tempdir().expect("tempdir");
        for directory in 0..DIRECTORIES {
            let path = temp.path().join(format!("package-{directory:03}/src"));
            std::fs::create_dir_all(&path).expect("create package directory");
            for file in 0..FILES_PER_DIRECTORY {
                std::fs::write(
                    path.join(format!("module-{file:03}.ts")),
                    b"export const value = 1;\n",
                )
                .expect("write source fixture");
            }
        }

        let discovery = discover_outline_files(temp.path());
        assert_eq!(discovery.files.len(), OUTLINE_FILE_WALK_CAP);
        assert!(discovery.walk_truncated);

        let mut micros_per_operation = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = std::time::Instant::now();
            for _ in 0..ITERATIONS {
                let discovery = discover_outline_files(std::hint::black_box(temp.path()));
                std::hint::black_box(discovery);
            }
            micros_per_operation.push(started.elapsed().as_micros() / ITERATIONS as u128);
        }
        micros_per_operation.sort_unstable();
        let median = micros_per_operation[SAMPLES / 2];

        eprintln!(
            "outline files walk: files={} samples={SAMPLES} iterations={ITERATIONS}",
            DIRECTORIES * FILES_PER_DIRECTORY
        );
        eprintln!("microseconds per outline operation: {micros_per_operation:?}");
        eprintln!("median: {median}us per outline operation");
    }
}
