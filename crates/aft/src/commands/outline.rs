const LINE_COUNT_BYTES: u64 = 1024 * 1024;
const ENTRY_BUDGET: usize = 10_000;

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde::Serialize;

use super::url_resolution::resolve_file_or_url;
use crate::commands::read::{handle_github_outline, is_github_read_target};
use crate::context::AppContext;
use crate::edit;
use crate::error::AftError;
use crate::inspect::job::is_test_file;
use crate::parser::{
    detect_language, node_range_with_decorators, node_text, FileParser, LangId, ParsedFile,
    RustModule, TreeSitterProvider,
};
use crate::protocol::{RawRequest, Response};
use crate::symbols::{Range, Symbol};
use crate::url_fetch::is_http_url;

const MAX_OUTLINE_FILE_BYTES: u64 = 50 * 1024 * 1024;
const BINARY_SAMPLE_BYTES: usize = 4 * 1024;
const OUTLINE_FILE_WALK_CAP: usize = 200;
const OUTLINE_FILE_COLLECTION_CAP: usize = 10_000;
const TYPE_MEMBER_PREVIEW_CAP: usize = 3;

#[cfg(test)]
thread_local! {
    static TABLE_RENDERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

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
    let _fetch_temp = super::url_resolution::FetchTempScope::enter();
    ctx.with_read_policy_snapshot(&req.id, || handle_outline_with_snapshot(req, ctx))
}

fn handle_outline_with_snapshot(req: &RawRequest, ctx: &AppContext) -> Response {
    let mut response = crate::bounded_io::command(req, ctx, handle_outline_inner);
    super::url_output::redact_url_cache_paths(req, ctx, &mut response);
    super::read_manifest::normalize_response_content_files(ctx, &mut response.data);
    response
}

fn handle_outline_inner(req: &RawRequest, ctx: &AppContext) -> Response {
    const MAX_OUTPUT_BYTES: usize = 30 * 1024;
    if let Some(reference) = crate::revision_read::requested_ref(req) {
        return handle_revision_outline(req, ctx, reference);
    }
    let excluded = excluded_read_paths(req, ctx);

    if req
        .params
        .get("files")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return handle_outline_files_mode(req, ctx, MAX_OUTPUT_BYTES);
    }

    if let Some(directory) = req.params.get("directory").and_then(|v| v.as_str()) {
        let dir_path = match ctx.validate_read_path(&req.id, req.session(), Path::new(directory)) {
            Ok(path) => path,
            Err(resp) => return resp,
        };
        // In a confined call, refuse a directory outside the confined root or
        // denied by the read policy before any walk starts there.
        if let Err(refusal) = crate::context::confinement::check_walk_root(&req.id, &dir_path) {
            return refusal;
        }
        if !crate::bounded_io::metadata(&dir_path).is_ok_and(|m| m.is_dir()) {
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
        let walk_incomplete = discovery.walk_truncated
            || discovery.collection_truncated
            || discovery.skipped_foreign_mounts > 0;
        let (rendered, skipped_files) = match outline_many_files(
            &files,
            ctx,
            req,
            project_root.as_deref(),
            &excluded,
            MultiFileBudget {
                max_bytes: MAX_OUTPUT_BYTES,
                total_requested: files.len(),
                walk_incomplete,
                files_capped: false,
            },
        ) {
            Ok(result) => result,
            Err(resp) => return resp,
        };

        let mut body = serde_json::json!({
            "text": rendered.text,
            "discovered": true,
            "complete": !skipped_files.iter().any(|file| file.reason == "denied_floor") && !walk_incomplete && !rendered.truncated && !rendered.cut_short && !rendered.content_incomplete,
            "output_truncated": rendered.truncated,
            "files_shown": rendered.shown,
            "structure_footer": rendered.footer,
            "walk_truncated": discovery.walk_truncated,
            "collection_truncated": discovery.collection_truncated,
            "skipped_foreign_mounts": discovery.skipped_foreign_mounts,
            "skipped_files": skipped_files,
            "content_files": rendered.content_files,
        });
        if rendered.deadline_reached {
            body["deadline_reached"] = serde_json::json!(true);
        }
        crate::context::confinement::disclose_denied(&mut body);
        return Response::success(&req.id, body);
    }

    // Multi-file mode: if "files" array is present, outline each file
    if let Some(files_arr) = req.params.get("files").and_then(|v| v.as_array()) {
        let project_root = ctx.config().project_root.clone();
        let (files, files_capped) = requested_outline_files(files_arr);
        let files = files.into_iter().map(String::from).collect::<Vec<_>>();
        let total_files_requested = files_arr.len();
        let (rendered, skipped_files) = match outline_many_files(
            &files,
            ctx,
            req,
            project_root.as_deref(),
            &excluded,
            MultiFileBudget {
                max_bytes: MAX_OUTPUT_BYTES,
                total_requested: total_files_requested,
                walk_incomplete: false,
                files_capped,
            },
        ) {
            Ok(result) => result,
            Err(resp) => return resp,
        };

        // Honest reporting: complete only when no requested file was skipped
        // and every requested file was examined. skipped_files names the gaps
        // (missing/unreadable/unparseable inputs); the footer counts files
        // left unexamined by the file cap, output budget or deadline.
        let mut body = serde_json::json!({
            "text": rendered.text,
            "complete": skipped_files.is_empty() && !rendered.truncated && !rendered.cut_short && !rendered.content_incomplete,
            "output_truncated": rendered.truncated,
            "files_shown": rendered.shown,
            "structure_footer": rendered.footer,
            "skipped_files": skipped_files,
            "content_files": rendered.content_files,
        });
        if rendered.deadline_reached {
            body["deadline_reached"] = serde_json::json!(true);
        }
        return Response::success(&req.id, body);
    }

    // Single-file mode (original behavior)
    let file = match req
        .params
        .get("file")
        .or_else(|| req.params.get("target"))
        .or_else(|| req.params.get("url"))
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

    outline_single_target(req, ctx, file)
}

/// Resolve and outline one target, sharing URL fetching and rendering between
/// single-target calls and entries in a batch.
fn outline_single_target(req: &RawRequest, ctx: &AppContext, file: &str) -> Response {
    if is_github_read_target(file) {
        return handle_github_outline(req, ctx, file);
    }

    let path = match resolve_file_or_url(req, ctx, file) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if crate::bounded_io::metadata(&path).is_err() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("file not found: {}", file),
        );
    }

    if is_http_url(file)
        && crate::parser::detect_language(&path) == Some(crate::parser::LangId::Json)
    {
        let source = match crate::bounded_io::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => return Response::error(&req.id, "url_fetch_failed", error.to_string()),
        };
        let mut response = super::url_output::json_preview(req, &source, None);
        super::url_output::disclose_download(&path, &mut response);
        let mut text = response.data["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        text = format_url_outline(file, &text);
        super::url_output::cap_text(
            &mut text,
            super::url_output::URL_OUTPUT_BYTES,
            "a filtered URL",
        );
        // Formatting the preview must not discard its metadata. URL content
        // does not contribute any local files to read authorization.
        response.data["text"] = text.into();
        response.data["content_files"] = serde_json::json!([]);
        return response;
    }

    outline_single_file(req, ctx, path, file)
}

/// Outline one file already resolved to a readable local `path`; `file` is
/// the caller's spelling (a URL for a downloaded document).
fn outline_single_file(req: &RawRequest, ctx: &AppContext, path: PathBuf, file: &str) -> Response {
    let symbols = match crate::parser::list_symbols_shared(ctx.provider(), &path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    let mut parser = ctx
        .provider()
        .as_any()
        .downcast_ref::<TreeSitterProvider>()
        .map(|provider| FileParser::with_symbol_cache(provider.symbol_cache()))
        .unwrap_or_else(FileParser::new);
    let structure = if detect_language(&path) == Some(LangId::Rust)
        && ctx.provider().as_any().is::<TreeSitterProvider>()
    {
        parser
            .extract_rust_outline(&path)
            .map(|(symbols, modules)| rust_modules_outline(&modules, &symbols, &[], &[]))
    } else {
        outline_structure_entries(
            &path,
            &symbols,
            &mut parser,
            ctx,
            &req.id,
            req.session(),
            false,
            None,
        )
    };
    let entries = match structure {
        Ok(entries) => entries,
        Err(error) => return Response::error(&req.id, error.code(), error.to_string()),
    };
    let filename = if is_http_url(file) {
        file.to_string()
    } else {
        path.file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| file.to_string())
    };
    let mut text = format_single_file_tree(&filename, &entries);
    let mut complete = true;
    if is_http_url(file) {
        let notice = crate::url_fetch::download_notice(&path);
        if notice.is_some() {
            complete = false;
        }
        text = format_url_outline_with_header(file, &text, notice.as_deref());
        complete &= text.len() <= super::url_output::URL_OUTPUT_BYTES;
        super::url_output::cap_text(
            &mut text,
            super::url_output::URL_OUTPUT_BYTES,
            "a smaller URL or section",
        );
    }

    Response::success(
        &req.id,
        serde_json::json!({ "text": text, "complete": complete, "content_files": if is_http_url(file) { vec![] } else { vec![path] } }),
    )
}

fn format_url_outline(url: &str, contents: &str) -> String {
    format!(
        "{url}\nSections are read with aft_zoom using the same url plus a heading.\n\n{contents}"
    )
}

fn format_url_outline_with_header(url: &str, text: &str, notice: Option<&str>) -> String {
    let body = text
        .strip_prefix(url)
        .and_then(|remainder| remainder.strip_prefix('\n'))
        .unwrap_or(text);
    let contents = match notice {
        Some(notice) => format!("{notice}\n\n{body}"),
        None => body.to_string(),
    };
    format_url_outline(url, &contents)
}

/// Outline one file as it is at the git revision `reference`. Only a single
/// local file can be outlined at a revision: directories, `files` mode and
/// URLs read the working tree or the network, never the object store.
fn handle_revision_outline(req: &RawRequest, ctx: &AppContext, reference: &str) -> Response {
    if let Err(refusal) = crate::revision_read::check_enabled(req, ctx) {
        return refusal;
    }
    let unsupported = |what: &str| {
        Response::error(
            &req.id,
            "invalid_request",
            format!("outline: 'ref' outlines a single repository file; {what} cannot be outlined at a revision"),
        )
    };
    if req.params.get("files").is_some() {
        return unsupported("files mode or several targets");
    }
    if req.params.get("directory").is_some() {
        return unsupported("a directory");
    }
    let Some(file) = req
        .params
        .get("file")
        .or_else(|| req.params.get("target"))
        .and_then(|v| v.as_str())
    else {
        return Response::error(
            &req.id,
            "invalid_request",
            "outline: missing required param 'file'",
        );
    };
    if is_http_url(file) || is_github_read_target(file) {
        return unsupported("a URL or GitHub address");
    }
    let requested = match ctx.validate_read_path(&req.id, req.session(), Path::new(file)) {
        Ok(path) => path,
        Err(refusal) => return refusal,
    };
    let session = match crate::revision_read::RevisionSession::open(
        req,
        ctx,
        reference,
        Some(Path::new(file)),
    ) {
        Ok(session) => session,
        Err(refusal) => return refusal,
    };
    let staged = match session.materialize(req, ctx, Path::new(file), file) {
        Ok(staged) => staged,
        Err(refusal) => return refusal,
    };
    let mut response = outline_single_file(req, ctx, staged, file);
    // Host adapters authorize the files an outline drew from; name the
    // requested path, not the private staged copy.
    if let Some(data) = response.data.as_object_mut() {
        if data.contains_key("content_files") {
            data.insert("content_files".into(), serde_json::json!([requested]));
        }
    }
    session.annotate(&mut response);
    response
}

/// Host adapters can re-render a held outline after applying their own read policy.
/// This transport-only field is not part of the model-facing tool schema.
fn excluded_read_paths(req: &RawRequest, ctx: &AppContext) -> HashSet<PathBuf> {
    let mut excluded = HashSet::new();
    for raw_path in req
        .params
        .get("_exclude_read_paths")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
    {
        let path = PathBuf::from(raw_path);
        excluded.insert(path.clone());
        // Restricted reads can use a canonical path internally even when the
        // host denied the file using the project-root path supplied by the caller.
        if let Ok(canonical) = std::fs::canonicalize(ctx.resolve_relative_path(&path)) {
            excluded.insert(crate::inspect::job::normalize_path(&canonical));
        }
    }
    excluded
}

/// Read manifests spell a file beneath the project root with the configured
/// root, after resolving symlinks, so a host's exclusion names that resolved
/// file. A walk through a symlinked directory reaches the same file under
/// another spelling, so exclusions must also match the walked file's canonical
/// path.
fn is_read_excluded(excluded: &HashSet<PathBuf>, path: &Path) -> bool {
    if excluded.is_empty() {
        return false;
    }
    excluded.contains(path)
        || std::fs::canonicalize(path).is_ok_and(|canonical| {
            excluded.contains(&crate::inspect::job::normalize_path(&canonical))
        })
}

fn include_tests_param(req: &RawRequest) -> bool {
    req.params
        .get("includeTests")
        .or_else(|| req.params.get("include_tests"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
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
    absolute_path: PathBuf,
    path: String, // relative path
    entries: Vec<OutlineEntry>,
    language: Option<LangId>,
}

#[derive(Clone)]
struct TestSummary {
    name: String,
    range: Range,
    items: usize,
    unknown_reason: Option<&'static str>,
}

#[derive(Clone)]
struct TestRegion {
    summary: TestSummary,
    included_path: Option<String>,
    implicit_include: bool,
    module_scope: Vec<String>,
    standalone: bool,
}

/// What the multi-file structure map needs from one parse of a file beyond
/// its symbols: the test regions it collapses and, for Rust, the methods
/// declared in each trait it previews. Both depend only on the file's own
/// text, so they are stored next to the symbols in the shared symbol cache
/// under the same freshness key, and a warm outline renders the file without
/// reading or parsing it. Test includes are resolved at render time, because
/// they depend on other files and on the caller's read policy.
pub(crate) struct OutlineParseFacts {
    test_regions: Vec<TestRegion>,
    trait_previews: Vec<(Range, Vec<OutlineEntry>)>,
}

impl OutlineParseFacts {
    /// Rough resident size, for the symbol cache's memory estimate.
    pub(crate) fn estimated_bytes(&self) -> u64 {
        fn entry_bytes(entry: &OutlineEntry) -> u64 {
            (std::mem::size_of::<OutlineEntry>()
                + entry.name.len()
                + entry.kind.len()
                + entry.signature.as_ref().map_or(0, String::len)) as u64
                + entry.members.iter().map(entry_bytes).sum::<u64>()
        }
        let regions = self
            .test_regions
            .iter()
            .map(|region| {
                (std::mem::size_of::<TestRegion>()
                    + region.summary.name.len()
                    + region.included_path.as_ref().map_or(0, String::len)
                    + region.module_scope.iter().map(String::len).sum::<usize>())
                    as u64
            })
            .sum::<u64>();
        let previews = self
            .trait_previews
            .iter()
            .map(|(_, methods)| {
                std::mem::size_of::<(Range, Vec<OutlineEntry>)>() as u64
                    + methods.iter().map(entry_bytes).sum::<u64>()
            })
            .sum::<u64>();
        std::mem::size_of::<Self>() as u64 + regions + previews
    }
}

/// Languages whose structure map collapses test code into summary rows.
fn structure_language(path: &Path) -> Option<LangId> {
    detect_language(path).filter(|lang| {
        matches!(
            lang,
            LangId::Rust
                | LangId::Python
                | LangId::Go
                | LangId::TypeScript
                | LangId::Tsx
                | LangId::JavaScript
        )
    })
}

/// Derive [`OutlineParseFacts`] from a parsed file. A file with syntax errors
/// is skipped by the structure map, so its regions are not collected.
fn outline_parse_facts(path: &Path, parsed: &ParsedFile) -> OutlineParseFacts {
    let root = parsed.tree.root_node();
    let mut test_regions = Vec::new();
    if let Some(lang) = structure_language(path) {
        if !root.has_error() {
            collect_test_regions(root, &parsed.source, lang, &mut test_regions);
        }
    }
    let mut trait_previews = Vec::new();
    if parsed.lang == LangId::Rust {
        collect_trait_member_previews(root, &parsed.source, &mut trait_previews);
    }
    OutlineParseFacts {
        test_regions,
        trait_previews,
    }
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
                    unknown_reason: None,
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
    session_id: &str,
    collapse_tests: bool,
    parsed: Option<&ParsedFile>,
) -> Result<Vec<OutlineEntry>, AftError> {
    let Some(lang) = structure_language(path) else {
        return Ok(build_outline_tree(symbols));
    };
    if !collapse_tests && lang != LangId::Rust {
        return Ok(build_outline_tree(symbols));
    }
    let captured;
    let (source, tree) = if let Some(parsed) = parsed {
        (parsed.source.as_str(), &parsed.tree)
    } else {
        let source = crate::parser::read_source(path).map_err(|error| AftError::FileNotFound {
            path: format!("{}: {error}", path.display()),
        })?;
        // Parse the bytes just read so test regions are located in the same
        // source they are sliced from, even if the file changed since the
        // symbols were extracted.
        let tree = crate::parser::parse_source_with_cached_parser(path, &source, lang)?;
        captured = (source, tree);
        (captured.0.as_str(), &captured.1)
    };
    if collapse_tests && tree.root_node().has_error() {
        return Err(AftError::ParseError {
            message: format!("syntax errors in {}", path.display()),
        });
    }
    let mut regions = Vec::new();
    if collapse_tests {
        collect_test_regions(tree.root_node(), &source, lang, &mut regions);
    }
    let scanned_modules;
    let modules = if lang == LangId::Rust {
        Some(match parsed.and_then(|parsed| parsed.modules.as_deref()) {
            Some(modules) => modules,
            None => {
                scanned_modules = crate::parser::rust_modules(tree.root_node(), source);
                scanned_modules.as_slice()
            }
        })
    } else {
        None
    };
    Ok(structure_entries_from_regions(
        path, symbols, regions, modules, parser, ctx, req_id, session_id,
    ))
}

/// The multi-file structure map for one file from its cached or freshly
/// derived [`OutlineParseFacts`], without touching its tree. `modules` is the
/// file's Rust module layout and must be present for Rust files.
#[allow(clippy::too_many_arguments)]
fn structure_entries_from_facts(
    path: &Path,
    symbols: &[Symbol],
    modules: Option<&[RustModule]>,
    has_error: bool,
    facts: &OutlineParseFacts,
    parser: &mut FileParser,
    ctx: &AppContext,
    req_id: &str,
    session_id: &str,
) -> Result<Vec<OutlineEntry>, AftError> {
    let Some(lang) = structure_language(path) else {
        return Ok(build_outline_tree(symbols));
    };
    if has_error {
        return Err(AftError::ParseError {
            message: format!("syntax errors in {}", path.display()),
        });
    }
    if lang == LangId::Rust && modules.is_none() {
        return Err(AftError::ParseError {
            message: format!("module layout missing for {}", path.display()),
        });
    }
    let mut entries = structure_entries_from_regions(
        path,
        symbols,
        facts.test_regions.clone(),
        modules,
        parser,
        ctx,
        req_id,
        session_id,
    );
    if lang == LangId::Rust {
        apply_trait_member_previews(&facts.trait_previews, &mut entries);
    }
    Ok(entries)
}

/// Fold `symbols` that fall inside a test region into that region's summary
/// row, resolve test includes, and build the remaining product outline.
/// `modules` is the Rust module layout (`None` for other languages).
#[allow(clippy::too_many_arguments)]
fn structure_entries_from_regions(
    path: &Path,
    symbols: &[Symbol],
    regions: Vec<TestRegion>,
    modules: Option<&[RustModule]>,
    parser: &mut FileParser,
    ctx: &AppContext,
    req_id: &str,
    session_id: &str,
) -> Vec<OutlineEntry> {
    let mut regions = regions;
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
            let mut label_path = PathBuf::from(&included_path);
            if region.implicit_include {
                // Even an existence probe can follow an include symlink outside
                // the pinned tree. Validate before choosing the module layout.
                let probe = if crate::context::confinement::active() {
                    ctx.validate_read_path(req_id, session_id, &included).ok()
                } else {
                    Some(included.clone())
                };
                if !probe.is_some_and(|path| {
                    crate::bounded_io::metadata(&path).is_ok_and(|m| m.is_file())
                }) {
                    label_path = PathBuf::from(&region.summary.name).join("mod.rs");
                    included = base.join(&label_path);
                }
            }
            let included_label = test_module_include_label(parent, &base, &label_path);
            // A test include is navigation metadata, not a requested file. A
            // denied or unavailable include must not hide the declaring file's
            // product API or bypass the project's path restrictions.
            match ctx.validate_read_path(req_id, session_id, &included) {
                Ok(included) => {
                    let count = match outline_skip_reason(&included, Some(&mut *parser)) {
                        Some(reason) => Err(reason),
                        None => parser
                            .extract_symbols(&included)
                            .map(|symbols| symbols.len())
                            .map_err(|error| outline_error_reason(&error)),
                    };
                    match count {
                        Ok(items) => region.summary.items = items,
                        Err(reason) => region.summary.unknown_reason = Some(reason),
                    }
                    parser.evict_parse_tree(&included);
                }
                Err(_) => region.summary.unknown_reason = Some("path_denied"),
            }
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
    let mut entries = match modules {
        Some(modules) => rust_modules_outline(modules, &product_symbols, &summaries, &[]),
        None => build_outline_tree(&product_symbols),
    };
    for summary in summaries {
        let description = match summary.unknown_reason {
            Some(reason) => format!(
                "items unknown ({reason}) (lines {}-{})",
                summary.range.start_line + 1,
                summary.range.end_line + 1
            ),
            None => format!(
                "{} items (lines {}-{})",
                summary.items,
                summary.range.start_line + 1,
                summary.range.end_line + 1
            ),
        };
        let entry = OutlineEntry {
            signature: Some(format!("{}: {}", summary.name, description)),
            name: summary.name,
            kind: "test_summary".into(),
            range: summary.range,
            exported: false,
            members: Vec::new(),
        };
        insert_summary_entry(&mut entries, entry);
    }
    entries
}

fn test_module_include_label(parent: &Path, base: &Path, included_path: &Path) -> String {
    // Joining `..` onto a Windows verbatim path normalizes it immediately,
    // before a relative label can be recovered. Build the label from the
    // module's relative base and the declared path instead of its read path.
    let relative_base = base.strip_prefix(parent).unwrap_or(base);
    path_to_slash(&relative_base.join(included_path))
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
fn rust_modules_outline(
    modules: &[RustModule],
    symbols: &[Symbol],
    tests: &[TestSummary],
    scope: &[String],
) -> Vec<OutlineEntry> {
    let modules = modules
        .iter()
        .filter(|module| {
            !tests
                .iter()
                .any(|test| range_contains(&test.range, &module.range))
        })
        .collect::<Vec<_>>();
    let ranges = modules
        .iter()
        .map(|module| module.range.clone())
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
        let name = module.name.clone();
        let mut module_scope = scope.to_vec();
        module_scope.push(name.clone());
        let module_symbols = symbols
            .iter()
            .filter(|symbol| range_contains(&range, &symbol.range))
            .cloned()
            .collect::<Vec<_>>();
        let members = rust_modules_outline(&module.children, &module_symbols, tests, &module_scope);
        let exported = module.exported;
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
    ignored_entries: usize,
}

#[derive(Clone, Default)]
struct OutlineIgnoreStack {
    matchers: Vec<Arc<ignore::gitignore::Gitignore>>,
    target_root: PathBuf,
    /// Set for a confined walk, which uses only the confined root's own
    /// ignore files and checks entries against the read policy.
    confined: Option<Arc<crate::context::confinement::ConfinedIgnoreRules>>,
}

impl OutlineIgnoreStack {
    fn for_target_root(root: &Path) -> Self {
        let root = root.to_path_buf();
        let mut stack = Self {
            target_root: root.clone(),
            ..Self::default()
        };
        // Ignore discovery can open global files, ancestors, or symlinks outside
        // the pinned tree. Confined outlines read only the root's own files.
        if crate::context::confinement::active() {
            stack.confined = crate::context::confinement::ConfinedIgnoreRules::current(
                crate::context::confinement::OUTLINE_FILES_IGNORE_FILES,
            );
            return stack;
        }

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
        if crate::context::confinement::active() {
            return child;
        }
        if let Some(matcher) = build_outline_gitignore(directory, &directory.join(".gitignore")) {
            child.matchers.push(matcher);
        }
        child
    }

    fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        if let Some(confined) = &self.confined {
            return confined.is_ignored(path, is_dir);
        }
        let mut ignored = false;
        for matcher in &self.matchers {
            let mut current = path;
            let mut current_is_dir = is_dir;
            // The caller explicitly selected the target. Ancestor exclusions
            // must not hide its contents, but rules for descendants still apply.
            while current != self.target_root && current.starts_with(&self.target_root) {
                let matched = matcher.matched(current, current_is_dir);
                if matched.is_ignore() {
                    ignored = true;
                    break;
                } else if matched.is_whitelist() {
                    ignored = false;
                    break;
                }
                let Some(parent) = current.parent() else {
                    break;
                };
                current = parent;
                current_is_dir = true;
            }
        }
        ignored
    }

    /// The confined read policy check for a walked entry; unconfined walks
    /// admit everything.
    fn admits(&self, path: &Path) -> bool {
        self.confined
            .as_ref()
            .map_or(true, |confined| confined.admits(path))
    }
}

fn build_outline_gitignore(
    root: &Path,
    ignore_file: &Path,
) -> Option<Arc<ignore::gitignore::Gitignore>> {
    if !ignore_file.is_file() {
        return None;
    }
    if crate::project_content::storage_root_for_path(ignore_file).is_some()
        || crate::db::file_identity::is_live_sqlite_file(ignore_file)
    {
        return None;
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    let _ = builder.add(ignore_file);
    builder.build().ok().map(Arc::new)
}

pub(crate) fn git_info_exclude_for_target(target: &Path) -> Option<(PathBuf, PathBuf)> {
    for repository_root in target.ancestors() {
        let git_entry = repository_root.join(".git");
        let Ok(metadata) = crate::bounded_io::metadata(&git_entry) else {
            continue;
        };
        let git_dir = if metadata.is_dir() {
            git_entry
        } else if metadata.is_file() {
            let contents = crate::bounded_io::read_to_string(&git_entry).ok()?;
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
        let git_common_dir = if let Ok(common) = crate::bounded_io::read_to_string(&common_dir) {
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
    let excluded = excluded_read_paths(req, ctx);

    let mut file_entries = Vec::new();
    let mut directory_nodes = Vec::new();
    let mut tree_roots = Vec::new();
    let mut entries_examined = 0usize;
    let mut walk_truncated = false;
    let mut collection_truncated = false;
    let mut skipped_foreign_mounts = 0usize;
    let mut empty_targets = Vec::new();
    let mut skipped_files = Vec::new();

    for target in targets {
        if is_http_url(&target) {
            return Response::error(
                &req.id,
                "invalid_request",
                "files mode requires directory targets; URLs aren't directories",
            );
        }
        let dir_path = match ctx.validate_read_path(&req.id, req.session(), Path::new(&target)) {
            Ok(path) => path,
            Err(response) if multiple_targets && response.data["code"] == "denied_floor" => {
                skipped_files.push(SkippedFile::new(&target, "denied_floor"));
                continue;
            }
            Err(response) => return response,
        };
        // In a confined call, refuse a directory outside the confined root or
        // denied by the read policy before any walk starts there.
        if let Err(refusal) = crate::context::confinement::check_walk_root(&req.id, &dir_path) {
            return refusal;
        }

        if crate::bounded_io::metadata(&dir_path).is_err() {
            return Response::error(
                &req.id,
                "file_not_found",
                format!("directory not found: {}", target),
            );
        }
        if !crate::bounded_io::metadata(&dir_path).is_ok_and(|m| m.is_dir()) {
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
        let mut discovery = discover_outline_files_for_files_mode(&dir_path);
        discovery
            .files
            .retain(|file| !is_read_excluded(&excluded, Path::new(file)));
        let mut admitted_files = Vec::new();
        for file in std::mem::take(&mut discovery.files) {
            match ctx.validate_read_path(&req.id, req.session(), Path::new(&file)) {
                Ok(_) => admitted_files.push(file),
                Err(response) if response.data["code"] == "denied_floor" => {
                    skipped_files.push(SkippedFile::new(file, "denied_floor"));
                    discovery.ignored_entries += 1;
                }
                Err(response) => return response,
            }
        }
        discovery.files = admitted_files;
        entries_examined += discovery.entries_examined;
        walk_truncated |= discovery.walk_truncated;
        collection_truncated |= discovery.collection_truncated;
        skipped_foreign_mounts += discovery.skipped_foreign_mounts;

        let empty_reason = if discovery.walk_truncated
            || discovery.collection_truncated
            || discovery.skipped_foreign_mounts > 0
        {
            "directory walk incomplete"
        } else if discovery.files.is_empty() {
            if discovery.ignored_entries > 0 {
                "all files ignored or excluded"
            } else if discovery.entries_examined == 0 {
                "empty folder"
            } else {
                "no regular files found"
            }
        } else {
            "test files excluded; set includeTests: true to include them"
        };
        let files_before = file_entries.len();

        let root = append_outline_directory_tree(
            &dir_path,
            display_root,
            discovery,
            ctx,
            include_tests,
            &mut file_entries,
            &mut directory_nodes,
        );
        if file_entries.len() == files_before {
            empty_targets.push(format!(
                "0 files under {} ({empty_reason})\n",
                display_path(&dir_path, &target, project_root.as_deref())
            ));
        }
        tree_roots.push(root);
    }

    for root in &tree_roots {
        aggregate_outline_directory(*root, &mut directory_nodes, &file_entries);
    }
    let rows = plan_outline_file_rows(
        &tree_roots,
        &directory_nodes,
        &file_entries,
        max_output_bytes.saturating_sub(empty_targets.iter().map(String::len).sum::<usize>()),
    );
    populate_rendered_file_symbols(&rows, &mut file_entries, ctx);
    let table = format_files_table(&rows, &directory_nodes, &file_entries, max_output_bytes);
    let mut text = table.into_string();
    for explanation in empty_targets {
        text.push_str(&explanation);
    }
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

    let content_files = rows
        .iter()
        .filter_map(|row| match row {
            OutlineTableRow::File(file_id) => Some(file_entries[*file_id].absolute_path.clone()),
            OutlineTableRow::Rollup(_) => None,
        })
        // A read manifest is a set: sorted and without duplicates, whatever
        // order the rows are displayed in.
        .collect::<BTreeSet<_>>();
    file_entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut response_data = serde_json::json!({
        "text": text,
        "content_files": content_files,
        "files": file_entries,
        "complete": !walk_truncated
            && !collection_truncated
            && skipped_foreign_mounts == 0
            && skipped_files.is_empty()
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
    if !skipped_files.is_empty() {
        response_data["skipped_files"] = serde_json::json!(skipped_files);
    }
    if let Some(env) = envelope {
        response_data["files_list_envelope"] = serde_json::to_value(&env).unwrap();
    }
    crate::context::confinement::disclose_denied(&mut response_data);
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
    plan_outline_file_rows_with_length(
        roots,
        directory_nodes,
        file_entries,
        max_bytes,
        files_table_rendered_len,
    )
}

fn plan_outline_file_rows_with_length(
    roots: &[usize],
    directory_nodes: &[OutlineDirectoryNode],
    file_entries: &[OutlineFileEntry],
    max_bytes: usize,
    mut rendered_len: impl FnMut(
        &[OutlineTableRow],
        &[OutlineDirectoryNode],
        &[OutlineFileEntry],
    ) -> usize,
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
            if rendered_len(&candidate_rows, directory_nodes, file_entries) <= max_bytes {
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
        let Ok(metadata) = crate::bounded_io::metadata(&path) else {
            entry.symbols = Some(0);
            continue;
        };
        let symbols = cached_symbol_count(ctx, &path, &metadata).unwrap_or_else(|| {
            if metadata.len() > MAX_OUTLINE_FILE_BYTES {
                0
            } else {
                crate::parser::list_symbols_shared(ctx.provider(), &path)
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
    crate::parser::cached_symbol_count_shared(&symbol_cache, path, mtime, size)
}

fn inspect_outline_file_content(path: &Path) -> std::io::Result<OutlineFileContentStats> {
    #[cfg(all(test, unix))]
    crate::bounded_io::tests::note_open(path);
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
    #[cfg(test)]
    TABLE_RENDERS.with(|count| count.set(count.get() + 1));
    let (path_width, language_width, middle_width) =
        files_table_widths(rows, directory_nodes, file_entries);
    let mut output = String::new();
    for row in rows {
        let (path, middle, lines) =
            files_table_fields(row, directory_nodes, file_entries, language_width);
        output.push_str(&format!(
            "{path:<path_width$}  {middle:<middle_width$} {lines:>7} lines\n",
            lines = lines.as_deref().unwrap_or("unknown"),
        ));
    }
    let rendered_len = output.len() + files_table_trailer_len(rows, directory_nodes);
    OutlineTable {
        table: output,
        rendered_len,
    }
}

fn files_table_widths(
    rows: &[OutlineTableRow],
    directory_nodes: &[OutlineDirectoryNode],
    file_entries: &[OutlineFileEntry],
) -> (usize, usize, usize) {
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

    (path_width, language_width, middle_width)
}

fn files_table_fields(
    row: &OutlineTableRow,
    directory_nodes: &[OutlineDirectoryNode],
    file_entries: &[OutlineFileEntry],
    language_width: usize,
) -> (String, String, Option<String>) {
    match row {
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
    }
}

/// Rust string formatting pads by character count, but the output budget counts
/// bytes. Keep both quantities so Unicode paths make the same budget decisions.
fn files_table_rendered_len(
    rows: &[OutlineTableRow],
    directory_nodes: &[OutlineDirectoryNode],
    file_entries: &[OutlineFileEntry],
) -> usize {
    let (path_width, language_width, middle_width) =
        files_table_widths(rows, directory_nodes, file_entries);
    let mut bytes = files_table_trailer_len(rows, directory_nodes);
    for row in rows {
        let (path, middle, lines) =
            files_table_fields(row, directory_nodes, file_entries, language_width);
        let lines = lines.as_deref().unwrap_or("unknown");
        bytes += path.len()
            + path_width.saturating_sub(path.chars().count())
            + middle.len()
            + middle_width.saturating_sub(middle.chars().count())
            + lines.len()
            + 7usize.saturating_sub(lines.chars().count())
            + 10;
    }
    bytes
}

fn files_table_trailer_len(
    rows: &[OutlineTableRow],
    directory_nodes: &[OutlineDirectoryNode],
) -> usize {
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

    if budget_rollups_present {
        let envelope = crate::list_envelope::ListEnvelope::new(
            shown,
            crate::list_envelope::Total::Exact(shown + budget_rollup_files),
            crate::list_envelope::Unit::Files,
            vec![crate::list_envelope::Reason::Budget],
            &["path"],
        );
        let trailer_len = crate::list_surfaces::outline::outline_trailer_byte_len(&envelope);
        2 + trailer_len
    } else {
        0
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

/// The limits one multi-file structure map renders under.
struct MultiFileBudget {
    /// Rendered output budget in bytes.
    max_bytes: usize,
    /// Files the caller asked for or the walk found, for the footer's total.
    total_requested: usize,
    /// The directory walk stopped early, so the total is a lower bound.
    walk_incomplete: bool,
    /// The request's file list was cut to [`OUTLINE_FILE_WALK_CAP`] before
    /// outlining, so some requested files are never examined.
    files_capped: bool,
}

/// Time kept back from the request deadline, so that a batch stopped between
/// files still has time to render and return its partial result before the
/// deadline fails the whole call.
const OUTLINE_BATCH_RETURN_MARGIN: std::time::Duration = std::time::Duration::from_secs(1);

#[cfg(test)]
thread_local! {
    /// Makes [`outline_batch_deadline_reached`] report the deadline once this
    /// many files have been examined, so tests can stop a batch part-way
    /// without depending on timing.
    static OUTLINE_BATCH_DEADLINE_AFTER_FILES: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

/// Whether a multi-file outline should stop before examining another file
/// because the request deadline is less than [`OUTLINE_BATCH_RETURN_MARGIN`]
/// away. Parsing is not deadline-aware, so this check between files keeps a
/// long batch from starting another file it has no time for; a file already
/// being parsed can still run past the deadline.
fn outline_batch_deadline_reached(files_examined: usize) -> bool {
    #[cfg(test)]
    if let Some(limit) = OUTLINE_BATCH_DEADLINE_AFTER_FILES.with(std::cell::Cell::get) {
        return files_examined >= limit;
    }
    let _ = files_examined;
    crate::bounded_io::current().is_some_and(|budget| {
        std::time::Instant::now() + OUTLINE_BATCH_RETURN_MARGIN >= budget.deadline
    })
}

/// The string entries of an outline `files` list, cut at
/// [`OUTLINE_FILE_WALK_CAP`] (the same cap a directory outline applies to its
/// walk). Returns whether entries were left out; those count toward the
/// footer's total but are never probed, validated, read or parsed.
fn requested_outline_files(files: &[serde_json::Value]) -> (Vec<&str>, bool) {
    let mut requested = files.iter().filter_map(|file| file.as_str());
    let kept = requested
        .by_ref()
        .take(OUTLINE_FILE_WALK_CAP)
        .collect::<Vec<_>>();
    let capped = requested.next().is_some();
    (kept, capped)
}

/// The `files` entries an outline request examines: none for a revision,
/// files-mode or directory outline (those branches never read the list),
/// otherwise the entries [`requested_outline_files`] keeps. The pre-handler
/// access probe in [`crate::bounded_io::command`] uses this so that it never
/// probes an entry the handler skips.
pub(crate) fn outline_examined_files(req: &RawRequest) -> Vec<&str> {
    if crate::revision_read::requested_ref(req).is_some()
        || req
            .params
            .get("files")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        || req
            .params
            .get("directory")
            .and_then(|value| value.as_str())
            .is_some()
    {
        return Vec::new();
    }
    req.params
        .get("files")
        .and_then(|value| value.as_array())
        .map(|files| requested_outline_files(files).0)
        .unwrap_or_default()
}

/// Outline `files` and render their structure map within `max_bytes`.
///
/// Files are outlined in the order the map shows them (by display path), and
/// outlining stops as soon as the rendered output budget is spent: the first
/// file that does not fit has been read and parsed, but later files are not.
/// The files not shown still count in `total_requested`, so the footer
/// reports them as not shown. Outlining
/// also stops when the request deadline is close (see
/// [`outline_batch_deadline_reached`]), returning the files shown so far.
#[allow(clippy::too_many_arguments)]
fn outline_many_files(
    files: &[String],
    ctx: &AppContext,
    req: &RawRequest,
    project_root: Option<&Path>,
    excluded: &HashSet<PathBuf>,
    budget: MultiFileBudget,
) -> Result<(RenderedOutline, Vec<SkippedFile>), Response> {
    let mut paths = Vec::new();
    let mut admitted_files = Vec::new();
    // Keep request indices so denied leaves and parse failures share one list.
    let mut skipped_files: Vec<(usize, SkippedFile)> = Vec::new();
    for (index, file) in files.iter().enumerate() {
        if is_http_url(file) {
            admitted_files.push((index, file, None));
            continue;
        }
        match ctx.validate_read_path(&req.id, req.session(), Path::new(file)) {
            Ok(path) => {
                paths.push(path.clone());
                admitted_files.push((index, file, Some(path)));
            }
            Err(response) if response.data["code"] == "denied_floor" => {
                skipped_files.push((index, SkippedFile::new(file, "denied_floor")));
            }
            Err(response) => return Err(response),
        }
    }
    let display_root = project_root
        .filter(|root| paths.iter().all(|path| path.starts_with(root)))
        .map(Path::to_path_buf)
        .or_else(|| common_outline_path_ancestor(&paths));

    // Visit files in rendering order (stable, so equal display paths keep
    // their request order) so that the budget check below can stop early.
    let mut ordered = admitted_files
        .into_iter()
        .map(|(index, file, path)| {
            let rel_path = path
                .as_deref()
                .map(|path| display_path(path, file, display_root.as_deref()))
                .unwrap_or_else(|| file.to_string());
            (index, file, path, rel_path)
        })
        .collect::<Vec<_>>();
    ordered.sort_by(|a, b| a.3.cmp(&b.3));

    let mut writer = MultiFileTreeWriter::new(&budget);
    // Skipped files are reported in request order, keyed by request index.
    // One parser for the whole batch, sharing the provider's symbol cache:
    // the syntax check below leaves its tree in this parser's tree cache, and
    // symbol extraction for the same file then reuses that tree instead of
    // parsing the file a second time. Other providers keep the old path.
    let mut batch_parser = ctx
        .provider()
        .as_any()
        .downcast_ref::<TreeSitterProvider>()
        .map(|provider| {
            FileParser::with_symbol_cache(provider.symbol_cache()).with_source_capture()
        });
    let mut fallback_parser = FileParser::new();

    for (examined, (index, file, path, rel_path)) in ordered.into_iter().enumerate() {
        if path
            .as_deref()
            .is_some_and(|path| is_read_excluded(excluded, path))
        {
            continue;
        }
        if outline_batch_deadline_reached(examined) {
            writer.stop_at_deadline();
            break;
        }
        let Some(path) = path else {
            let response = outline_single_target(req, ctx, file);
            if !response.success {
                let code = response.data["code"].as_str().unwrap_or("error");
                let reason = response.data["message"]
                    .as_str()
                    .map(|message| format!("{code}: {message}"))
                    .unwrap_or_else(|| code.to_string());
                skipped_files.push((index, SkippedFile::new(file, reason)));
                continue;
            }
            writer.content_incomplete |= response.data["complete"] == false;
            if !writer.push_text(
                response.data["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                Vec::new(),
                None,
            ) {
                break;
            }
            continue;
        };
        if crate::bounded_io::metadata(&path).is_err() {
            skipped_files.push((index, SkippedFile::new(file, "file_not_found")));
            continue;
        }

        if let Some(reason) = outline_skip_reason(&path, batch_parser.as_mut()) {
            if let Some(parser) = batch_parser.as_mut() {
                parser.evict_parse_tree(&path);
            }
            skipped_files.push((index, SkippedFile::new(rel_path, reason)));
            continue;
        }

        let structure = match batch_parser.as_mut() {
            Some(parser) => {
                let structure = batch_file_structure(&path, parser, ctx, &req.id, req.session());
                // Extraction and test classification share the validated parse tree.
                parser.evict_parse_tree(&path);
                structure
            }
            None => {
                let structure = fallback_file_structure(
                    &path,
                    &mut fallback_parser,
                    ctx,
                    &req.id,
                    req.session(),
                );
                fallback_parser.evict_parse_tree(&path);
                structure
            }
        };
        match structure {
            Ok(entries) => {
                let outline = FileOutline {
                    absolute_path: path.clone(),
                    path: rel_path,
                    entries,
                    language: detect_language(&path),
                };
                if !writer.push(&outline) {
                    break;
                }
            }
            Err(error) => skipped_files.push((
                index,
                SkippedFile::new(rel_path, outline_error_reason(&error)),
            )),
        }
    }

    skipped_files.sort_by_key(|(index, _)| *index);
    Ok((
        writer.finish(),
        skipped_files
            .into_iter()
            .map(|(_, skipped)| skipped)
            .collect(),
    ))
}

/// One file's structure-map entries through the batch parser. A fresh cache
/// entry with outline facts renders without reading or parsing the file;
/// otherwise the file is parsed once and the facts are stored for next time.
fn batch_file_structure(
    path: &Path,
    parser: &mut FileParser,
    ctx: &AppContext,
    req_id: &str,
    session_id: &str,
) -> Result<Vec<OutlineEntry>, AftError> {
    if let Some(cached) = parser.cached_outline(path) {
        // A Rust entry whose module layout was dropped cannot render its
        // modules; parse it again instead.
        if detect_language(path) != Some(LangId::Rust) || cached.modules.is_some() {
            return structure_entries_from_facts(
                path,
                &cached.symbols,
                cached.modules.as_deref().map(Vec::as_slice),
                cached.has_error,
                &cached.facts,
                parser,
                ctx,
                req_id,
                session_id,
            );
        }
    }
    let parsed = parser.parse_snapshot(path)?;
    let symbols = parser.extract_symbols_from_snapshot(path, &parsed)?;
    let facts = Arc::new(outline_parse_facts(path, &parsed));
    parser.store_outline_facts(path, &parsed, Arc::clone(&facts));
    structure_entries_from_facts(
        path,
        &symbols,
        parsed.modules.as_deref().map(Vec::as_slice),
        parsed.tree.root_node().has_error(),
        &facts,
        parser,
        ctx,
        req_id,
        session_id,
    )
}

/// One file's structure-map entries for a provider other than tree-sitter,
/// which has no shared symbol cache to keep parse facts in.
fn fallback_file_structure(
    path: &Path,
    parser: &mut FileParser,
    ctx: &AppContext,
    req_id: &str,
    session_id: &str,
) -> Result<Vec<OutlineEntry>, AftError> {
    let symbols = crate::parser::list_symbols_shared(ctx.provider(), path)?;
    let mut entries =
        outline_structure_entries(path, &symbols, parser, ctx, req_id, session_id, true, None)?;
    if detect_language(path) == Some(LangId::Rust) {
        if let (Ok(source), Ok((tree, _))) = (crate::parser::read_source(path), parser.parse(path))
        {
            add_trait_member_previews(tree.root_node(), &source, &mut entries);
        }
    }
    Ok(entries)
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
    // The walkers below never filter their own starting directory, so a
    // confined walk checks it here, before anything under it is opened. A
    // refused directory is an empty, complete discovery.
    if !crate::context::confinement::admits_walk_root(directory) {
        return OutlineFileDiscovery {
            entries_examined: 0,
            files: Vec::new(),
            directories: Vec::new(),
            walk_truncated: false,
            collection_truncated: false,
            skipped_foreign_mounts: 0,
            ignored_entries: 0,
        };
    }
    crate::bounded_io::run(directory, None, move |directory| {
        Ok(discover_outline_files_unbounded(&directory, breadth_first))
    })
    .unwrap_or_else(|_| OutlineFileDiscovery {
        entries_examined: 0,
        files: Vec::new(),
        directories: Vec::new(),
        walk_truncated: true,
        collection_truncated: true,
        skipped_foreign_mounts: 0,
        ignored_entries: 0,
    })
}

/// The files and directories one outline walk discovers below `directory`
/// (`files_mode` picks the files-mode walk), so tests outside this module can
/// see what a walk yields without the request's earlier path checks.
#[cfg(all(test, unix))]
pub(crate) fn discover_outline_paths_for_test(directory: &Path, files_mode: bool) -> Vec<String> {
    let discovery = discover_outline_files_with_options(directory, files_mode);
    let mut paths = discovery.files;
    paths.extend(discovery.directories);
    paths
}

fn discover_outline_files_unbounded(directory: &Path, breadth_first: bool) -> OutlineFileDiscovery {
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut entries_examined = 0;
    let mut walk_truncated = false;
    let mut collection_truncated = false;
    let mut skipped_foreign_mounts = 0usize;
    let mut ignored_entries = 0usize;
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
                &mut ignored_entries,
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
        ignored_entries,
    }
}

fn outline_target_walk_builder(directory: &Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(directory);
    // One ignore-aware walker loads the target's nested rules without reopening
    // each ancestor ignore file for every directory.
    builder
        .hidden(false)
        .ignore(!crate::context::confinement::active())
        .git_ignore(!crate::context::confinement::active())
        .git_global(!crate::context::confinement::active())
        .git_exclude(!crate::context::confinement::active())
        .parents(!crate::context::confinement::active())
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
    // A confined walk loads none of the crate's ignore files; it applies the
    // confined root's own ignore files and the read policy here instead.
    let confined = crate::context::confinement::ConfinedIgnoreRules::current(
        crate::context::confinement::OUTLINE_IGNORE_FILES,
    );
    {
        let skipped_mounts = Arc::clone(&skipped_mounts);
        let boundary_failed = Arc::clone(&boundary_failed);
        let boundary = *boundary;
        builder.filter_entry(move |entry| {
            if !crate::bash_background::registry::safe_task_entry(entry.path()) {
                return false;
            }
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
                return confined.as_ref().map_or(true, |confined| {
                    !file_type.is_file()
                        || (!confined.is_ignored(entry.path(), false)
                            && confined.admits(entry.path()))
                });
            }
            if should_skip_directory(entry.path()) {
                return false;
            }
            if let Some(confined) = &confined {
                if confined.is_ignored(entry.path(), true) || !confined.admits(entry.path()) {
                    return false;
                }
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
    ignored_entries: &mut usize,
) -> usize {
    let root_stack = root_ignore_stack.clone();
    let mut pending = VecDeque::from([(directory.to_path_buf(), root_stack, false)]);
    let mut entries_examined = 0usize;

    while let Some((current, inherited_ignores, load_child_rules)) = pending.pop_front() {
        if entries_examined >= ENTRY_BUDGET {
            *collection_truncated = true;
            return entries_examined;
        }
        if files.len() >= OUTLINE_FILE_COLLECTION_CAP {
            *walk_truncated = true;
            return entries_examined;
        }
        // Load rules only for directories the entry budget lets us enter.
        let ignore_stack = if load_child_rules {
            inherited_ignores.for_child_directory(&current)
        } else {
            inherited_ignores
        };
        let Ok(entries) = std::fs::read_dir(&current) else {
            *collection_truncated = true;
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
        let mut entries = entries
            .into_iter()
            .filter_map(|entry| match entry {
                Ok(entry) => Some(entry),
                Err(_) => {
                    *collection_truncated = true;
                    None
                }
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.path());
        let mut child_directories = Vec::new();
        let mut child_files = Vec::new();
        for entry in entries {
            if !crate::bash_background::registry::safe_task_entry(&entry.path()) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                *collection_truncated = true;
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
                *ignored_entries += 1;
                continue;
            }
            if !ignore_stack.admits(&path) {
                continue;
            }
            match boundary.should_descend(&path) {
                Ok(true) => {
                    directories.push(path.to_string_lossy().to_string());
                    pending.push_back((path, ignore_stack.clone(), true));
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
            if ignore_stack.is_ignored(&path, false) {
                *ignored_entries += 1;
            } else if ignore_stack.admits(&path) {
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

    let metadata = match crate::bounded_io::metadata(path) {
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
    // A fresh symbol-cache entry already records the verdict of the parse its
    // symbols came from, so a warm file is not parsed at all.
    let validated = match parser {
        Some(parser) => match parser.cached_has_error(path) {
            Some(has_error) => Ok(Some(!has_error)),
            None => edit::validate_syntax_with_parser(parser, path),
        },
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
    let mut previews = Vec::new();
    collect_trait_member_previews(node, source, &mut previews);
    apply_trait_member_previews(&previews, entries);
}

/// Collect each trait's range and its method rows from the tree,
/// so they can be cached and attached later without the tree.
fn collect_trait_member_previews(
    node: tree_sitter::Node<'_>,
    source: &str,
    previews: &mut Vec<(Range, Vec<OutlineEntry>)>,
) {
    if node.kind() == "trait_item" {
        if let Some(body) = node.child_by_field_name("body") {
            let range = node_range_with_decorators(&node, source, LangId::Rust);
            let mut methods = Vec::new();
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
                methods.push(OutlineEntry {
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
            previews.push((range, methods));
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_trait_member_previews(child, source, previews);
    }
}

/// Append each collected trait's method rows to the outline entry for that
/// trait. A trait with no entry (for example one inside collapsed test code)
/// gets nothing.
fn apply_trait_member_previews(
    previews: &[(Range, Vec<OutlineEntry>)],
    entries: &mut [OutlineEntry],
) {
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
    for (range, methods) in previews {
        if let Some(entry) = find_trait(entries, range) {
            entry.members.extend(methods.iter().cloned());
        }
    }
}

/// Format single-file outline as tree text with signatures.
fn format_single_file_tree(filename: &str, entries: &[OutlineEntry]) -> String {
    let mut output = format!("{}\n", filename);
    render_entries(entries, 1, &mut output, true);
    output
}

/// The rendered structure map for a batch of files, built incrementally by
/// [`MultiFileTreeWriter`] in path order.
struct RenderedOutline {
    text: String,
    /// The output byte budget cut the map.
    truncated: bool,
    /// Files were left unexamined by the file-list cap or the deadline.
    cut_short: bool,
    /// Outlining stopped because the request deadline was close.
    deadline_reached: bool,
    /// A shown URL was incomplete even though its outline fit the batch budget.
    content_incomplete: bool,
    shown: usize,
    footer: String,
    /// The files whose outlines were rendered, as a sorted set like every
    /// other read manifest.
    content_files: BTreeSet<PathBuf>,
}

/// The list trailer for a structure map: see
/// [`crate::list_surfaces::outline::build_outline_structure_envelope`].
fn outline_structure_footer(
    shown: usize,
    total: usize,
    budget: bool,
    walk: bool,
    time_limit: bool,
) -> String {
    crate::list_surfaces::outline::build_outline_structure_envelope(
        shown, total, budget, walk, time_limit,
    )
    .and_then(|envelope| crate::subc_format::render_envelope_trailer(&envelope))
    .unwrap_or_default()
}

/// Renders a directory tree of files and their structure entries, one file
/// at a time in path order, within an output byte budget. Files are rendered
/// atomically, so every shown type keeps its preview and exact remainder.
///
/// The writer is fed while files are still being outlined, so a batch can
/// stop reading and parsing as soon as [`MultiFileTreeWriter::push`] reports
/// that the budget is spent. The files never offered count toward the total
/// in the footer but are not shown.
struct MultiFileTreeWriter {
    output: String,
    truncated: bool,
    /// The request's file list was cut before outlining.
    files_capped: bool,
    /// Outlining stopped near the request deadline.
    deadline_reached: bool,
    content_incomplete: bool,
    files_shown: usize,
    content_files: BTreeSet<PathBuf>,
    /// Where each shown file's text starts in `output`, and its path, so a
    /// longer deadline footer can take back the last file if it must.
    shown_blocks: Vec<(usize, Option<PathBuf>)>,
    /// Path components of the last shown file, for directory grouping.
    prev_parts: Vec<String>,
    max_bytes: usize,
    total_requested: usize,
    walk_incomplete: bool,
    trailer_reserve: usize,
    /// A rendered file that fits only if no trailer follows it, which is
    /// known only once no later file is offered.
    pending: Option<(String, Vec<String>, Option<PathBuf>)>,
}

impl MultiFileTreeWriter {
    fn new(budget: &MultiFileBudget) -> Self {
        // Reserve the real list trailer, not a guessed number of bytes.
        let trailer_reserve = outline_structure_footer(
            budget.total_requested,
            budget.total_requested,
            true,
            budget.walk_incomplete,
            false,
        )
        .len()
            + 2;
        Self {
            output: String::new(),
            truncated: false,
            files_capped: budget.files_capped,
            deadline_reached: false,
            content_incomplete: false,
            files_shown: 0,
            content_files: BTreeSet::new(),
            shown_blocks: Vec::new(),
            prev_parts: Vec::new(),
            max_bytes: budget.max_bytes,
            total_requested: budget.total_requested,
            walk_incomplete: budget.walk_incomplete,
            trailer_reserve,
            pending: None,
        }
    }

    /// Files are known to be left unshown whatever the output budget, so a
    /// footer always follows the last file.
    fn cut_short(&self) -> bool {
        self.files_capped || self.deadline_reached
    }

    /// Record that outlining stopped at the request deadline with files
    /// still unexamined.
    fn stop_at_deadline(&mut self) {
        self.deadline_reached = true;
    }
    /// Offer the next file in path order. Returns `false` once the budget is
    /// spent: this file and every later one would not be shown, so the
    /// caller stops outlining files.
    fn push(&mut self, fo: &FileOutline) -> bool {
        let mut file_output = String::new();
        let parts: Vec<&str> = fo.path.split('/').collect();
        let file_name = parts.last().copied().unwrap_or(&fo.path);
        let dir_parts = &parts[..parts.len().saturating_sub(1)];

        // Find common prefix with previous path
        let common = self
            .prev_parts
            .iter()
            .zip(dir_parts.iter())
            .take_while(|(a, b)| a.as_str() == **b)
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

        let parts = parts.iter().map(|part| part.to_string()).collect();
        self.push_text(file_output, parts, Some(fo.absolute_path.clone()))
    }

    /// URL blocks use their single-target rendering and have no local read manifest.
    fn push_text(
        &mut self,
        file_output: String,
        parts: Vec<String>,
        path: Option<PathBuf>,
    ) -> bool {
        if self.truncated {
            return false;
        }
        if self.pending.take().is_some() {
            // The held file was not the last one, so it needed room for the
            // trailer and does not fit.
            self.truncated = true;
            return false;
        }
        let used = self.output.len() + file_output.len();
        if used + self.trailer_reserve <= self.max_bytes {
            self.accept(file_output, parts, path);
            return true;
        }
        if !self.walk_incomplete && !self.cut_short() && used <= self.max_bytes {
            // Fits only as the last file; keep looking for a later one.
            self.pending = Some((file_output, parts, path));
            return true;
        }
        self.truncated = true;
        false
    }

    fn accept(&mut self, file_output: String, parts: Vec<String>, path: Option<PathBuf>) {
        self.shown_blocks.push((self.output.len(), path.clone()));
        self.output.push_str(&file_output);
        if let Some(path) = path {
            self.content_files.insert(path);
        }
        self.files_shown += 1;
        self.prev_parts = parts;
    }

    /// Remove the last shown file from the output so a longer footer fits.
    /// Returns `false` when no file is shown.
    fn take_back_last_file(&mut self) -> bool {
        let Some((start, path)) = self.shown_blocks.pop() else {
            return false;
        };
        self.output.truncate(start);
        // A `files` list can name the same file twice. The read manifest must
        // keep a path while any block still shown in the text came from it,
        // or a host checking read rules against the manifest would miss it.
        if let Some(path) = path.filter(|path| {
            !self
                .shown_blocks
                .iter()
                .any(|(_, shown)| shown.as_ref() == Some(path))
        }) {
            self.content_files.remove(&path);
        }
        self.files_shown -= 1;
        self.truncated = true;
        true
    }

    fn finish(mut self) -> RenderedOutline {
        if let Some((file_output, parts, path)) = self.pending.take() {
            if self.cut_short() {
                // Stopped at the deadline after this file: the footer follows
                // it after all, and there is no room for both.
                self.truncated = true;
            } else {
                self.accept(file_output, parts, path);
            }
        }
        // The output budget and the file-list cap are budget stops; the
        // request deadline is a time-limit stop.
        let mut footer;
        loop {
            footer = outline_structure_footer(
                self.files_shown,
                self.total_requested,
                self.truncated || self.files_capped,
                self.walk_incomplete,
                self.deadline_reached,
            );
            // Files were admitted with room for the shorter budget footer;
            // a deadline footer that does not fit takes back the last file.
            let fits = self.output.len() + footer.len() + 1 <= self.max_bytes;
            if !self.deadline_reached || footer.is_empty() || fits {
                break;
            }
            if !self.take_back_last_file() {
                break;
            }
        }
        let mut output = self.output;
        if !footer.is_empty() {
            output.push('\n');
            output.push_str(&footer);
        }
        RenderedOutline {
            text: output,
            truncated: self.truncated,
            cut_short: self.files_capped || self.deadline_reached,
            deadline_reached: self.deadline_reached,
            content_incomplete: self.content_incomplete,
            shown: self.files_shown,
            footer,
            content_files: self.content_files,
        }
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

    #[test]
    fn outline_files_mode_refuses_raw_url_array_targets() {
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let req: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "url-directories",
            "command": "outline",
            "files": true,
            "target": ["https://example.com/docs"]
        }))
        .unwrap();
        let response = handle_outline(&req, &ctx);
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["code"], "invalid_request");
        assert!(response.data["message"]
            .as_str()
            .unwrap()
            .contains("URLs aren't directories"));
    }

    fn navigation_fixture() -> (tempfile::TempDir, Vec<PathBuf>, AppContext) {
        let temp = tempfile::tempdir().unwrap();
        let sources = [
            ("a.ts", "export function alpha() {}\nfunction beta() {}\n"),
            ("b.tsx", "export function alpha() {}\nfunction beta() {}\n"),
            ("c.js", "export function alpha() {}\nfunction beta() {}\n"),
            ("d.py", "def alpha(): pass\ndef beta(): pass\n"),
            ("e.go", "package main\nfunc alpha() {}\nfunc beta() {}\n"),
            (
                "f.rs",
                "pub mod nested {\n    pub fn alpha() {}\n}\n#[test]\nfn beta() {}\n",
            ),
            ("g.cpp", "void alpha() {}\nvoid beta() {}\n"),
        ];
        let files = sources
            .iter()
            .map(|(name, source)| {
                let path = temp.path().join(name);
                std::fs::write(&path, source).unwrap();
                path
            })
            .collect();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        (temp, files, ctx)
    }

    #[test]
    fn outline_content_files_and_internal_exclusions_preserve_allowed_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let allowed = root.join("allowed.ts");
        let denied = root.join("denied.ts");
        std::fs::write(&allowed, "export function publicSymbol() {}\n").unwrap();
        std::fs::write(&denied, "export function secretSymbol() {}\n").unwrap();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        for files_mode in [false, true] {
            for exclude in [false, true] {
                let args = serde_json::json!({"target":root, "files":files_mode,
                    "_exclude_read_paths":if exclude { vec![denied.clone()] } else { vec![] }});
                let translated =
                    crate::subc_translate::subc_translate("outline", &args, &root).unwrap();
                let mut request = serde_json::to_value(translated.args).unwrap();
                request["id"] = serde_json::json!("read-policy");
                request["command"] = serde_json::json!("outline");
                let request: RawRequest = serde_json::from_value(request).unwrap();
                let response = handle_outline(&request, &ctx);
                assert!(response.success);
                let paths = response.data["content_files"].as_array().unwrap();
                assert!(
                    paths.contains(&serde_json::json!(crate::inspect::job::normalize_path(
                        &allowed
                    )))
                );
                assert_eq!(
                    paths.contains(&serde_json::json!(crate::inspect::job::normalize_path(
                        &denied
                    ))),
                    !exclude
                );
                let text = response.data["text"].as_str().unwrap();
                assert!(text.contains("allowed.ts"));
                assert_eq!(text.contains("denied.ts"), !exclude);
                if !files_mode {
                    assert_eq!(text.contains("secretSymbol"), !exclude);
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn outline_manifest_uses_symlinked_project_root_spelling_and_sorts_paths() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real_root = temp.path().join("project");
        std::fs::create_dir(&real_root).unwrap();
        let request_root = temp.path().join("project-alias");
        symlink(&real_root, &request_root).unwrap();
        let barrel = request_root.join("barrel.ts");
        let secret = request_root.join("secret.ts");
        std::fs::write(
            &real_root.join("barrel.ts"),
            "export function barrel() {}\n",
        )
        .unwrap();
        std::fs::write(
            &real_root.join("secret.ts"),
            "export function secret() {}\n",
        )
        .unwrap();
        // The manifest takes the spelling of the configured project root, not
        // of the request: an alias root (like macOS `/var` for `/private/var`)
        // keeps its alias, and a real root names the real files even when the
        // request goes through the alias.
        for configured_root in [&request_root, &real_root] {
            let ctx = AppContext::new(
                Box::new(TreeSitterProvider::new()),
                crate::config::Config {
                    project_root: Some(configured_root.clone()),
                    restrict_to_project_root: true,
                    ..crate::config::Config::default()
                },
            );
            let request = serde_json::from_value(serde_json::json!({
                "id": "symlinked-outline-manifest", "command": "outline",
                "files": [secret, barrel],
            }))
            .unwrap();

            let response = handle_outline(&request, &ctx);
            assert!(response.success, "{:?}", response.data);
            let manifest = response.data["content_files"].as_array().unwrap();
            let expected: Vec<_> = [
                configured_root.join("barrel.ts"),
                configured_root.join("secret.ts"),
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
            assert_eq!(response.data["content_files"], serde_json::json!(expected));
            assert!(manifest
                .windows(2)
                .all(|pair| pair[0].as_str().unwrap() < pair[1].as_str().unwrap()));
        }
    }

    #[cfg(unix)]
    #[test]
    fn outline_excludes_a_file_denied_at_its_real_location_when_walked_through_a_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir(root.join("a")).unwrap();
        symlink(root.join("src"), root.join("a/link")).unwrap();
        std::fs::write(root.join("src/ok.ts"), "export function okSymbol() {}\n").unwrap();
        std::fs::write(
            root.join("src/secret.ts"),
            "export function secretSymbol() {}\n",
        )
        .unwrap();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.clone()),
                ..crate::config::Config::default()
            },
        );
        // The manifest names the walked file at `src/secret.ts`; a host that
        // denies that path sends it back as the exclusion.
        for params in [
            serde_json::json!({"directory": root.join("a/link")}),
            serde_json::json!({"files": [root.join("a/link/ok.ts"), root.join("a/link/secret.ts")]}),
        ] {
            let mut args = params;
            args["id"] = serde_json::json!("symlinked-exclusion");
            args["command"] = serde_json::json!("outline");
            let request: RawRequest = serde_json::from_value(args.clone()).unwrap();
            let unfiltered = handle_outline(&request, &ctx);
            assert!(
                unfiltered.data["text"]
                    .as_str()
                    .unwrap()
                    .contains("secretSymbol"),
                "{args}"
            );
            assert!(unfiltered.data["content_files"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(root.join("src/secret.ts"))));

            args["_exclude_read_paths"] = serde_json::json!([root.join("src/secret.ts")]);
            let request: RawRequest = serde_json::from_value(args.clone()).unwrap();
            let response = handle_outline(&request, &ctx);
            assert!(response.success, "{args}: {:?}", response.data);
            let text = response.data["text"].as_str().unwrap();
            assert!(text.contains("okSymbol"), "{args}: {text}");
            assert!(!text.contains("secretSymbol"), "{args}: {text}");
            assert_eq!(
                response.data["content_files"],
                serde_json::json!([root.join("src/ok.ts")]),
                "{args}"
            );
        }
    }

    #[test]
    fn outline_content_files_only_lists_rendered_structure_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let shown = root.join("a.ts");
        let hidden = root.join("z.ts");
        let source = (0..700)
            .map(|index| format!("export function symbol_{index:04}() {{}}\n"))
            .collect::<String>();
        std::fs::write(&shown, &source).unwrap();
        std::fs::write(&hidden, &source).unwrap();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        for params in [
            serde_json::json!({"directory": root}),
            serde_json::json!({"files": [hidden, shown]}),
        ] {
            let mut args = params;
            args["id"] = serde_json::json!("bounded-content");
            args["command"] = serde_json::json!("outline");
            let request = serde_json::from_value(args).unwrap();
            let response = handle_outline(&request, &ctx);
            assert!(response.success);
            let text = response.data["text"].as_str().unwrap();
            assert!(response.data["output_truncated"].as_bool().unwrap());
            assert_eq!(response.data["files_shown"], 1);
            assert!(text.contains("a.ts"));
            assert!(!text.contains("z.ts"));
            assert_eq!(
                response.data["content_files"],
                serde_json::json!([crate::inspect::job::normalize_path(&shown)])
            );
        }
    }

    #[test]
    fn outline_content_files_only_lists_rendered_table_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let shown = root.join("a.ts");
        std::fs::write(&shown, "export function visible() {}\n").unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();
        for index in 0..12 {
            std::fs::write(
                root.join(format!("nested/hidden_{index:02}.ts")),
                "export function hidden() {}\n",
            )
            .unwrap();
        }
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let request = serde_json::from_value(serde_json::json!({
            "id": "bounded-table-content", "command": "outline", "directory": root, "files": true,
        }))
        .unwrap();
        let response = handle_outline_files_mode(&request, &ctx, 250);
        assert!(response.success);
        let text = response.data["text"].as_str().unwrap();
        assert!(text.contains("a.ts"));
        assert!(text.contains("nested/"));
        assert!(!text.contains("hidden_"));
        assert_eq!(response.data["files_list_envelope"]["shown"], 1);
        assert_eq!(response.data["content_files"], serde_json::json!([shown]));
    }

    /// Both directory renderers display files in their own order (the table
    /// lists a directory's own files before its subdirectories; the tree sorts
    /// relative path text), but the read manifest is always sorted in `Path`
    /// component order, before and after the response wrapper rewrites it.
    #[test]
    fn outline_content_files_are_sorted_whatever_the_display_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("a")).unwrap();
        for file in ["z.rs", "a-b.rs", "a/b.rs"] {
            std::fs::write(root.join(file), "pub fn f() {}\n").unwrap();
        }
        let normalized = |value: &serde_json::Value| -> Vec<PathBuf> {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|path| crate::inspect::job::normalize_path(Path::new(path.as_str().unwrap())))
                .collect()
        };
        let root = crate::inspect::job::normalize_path(&root);
        let expected: Vec<_> = ["a/b.rs", "a-b.rs", "z.rs"]
            .into_iter()
            .map(|file| crate::inspect::job::normalize_path(&root.join(file)))
            .collect();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.clone()),
                ..crate::config::Config::default()
            },
        );
        for files_mode in [true, false] {
            let request: RawRequest = serde_json::from_value(serde_json::json!({
                "id": "sorted-manifest", "command": "outline", "directory": root, "files": files_mode,
            }))
            .unwrap();
            let inner = handle_outline_inner(&request, &ctx);
            assert!(inner.success, "{files_mode}: {:?}", inner.data);
            let text = inner.data["text"].as_str().unwrap();
            let position = |needle: &str| text.find(needle).unwrap();
            if files_mode {
                assert!(position("z.rs") < position("a/b.rs"), "{text}");
            } else {
                assert!(position("a-b.rs") < position("a/"), "{text}");
            }
            assert_eq!(
                normalized(&inner.data["content_files"]),
                expected,
                "{files_mode}"
            );
            let wrapped = handle_outline(&request, &ctx);
            assert_eq!(
                normalized(&wrapped.data["content_files"]),
                expected,
                "{files_mode}"
            );
        }
    }

    fn navigation_counts(
        request: &RawRequest,
        ctx: &AppContext,
    ) -> (serde_json::Value, usize, usize) {
        use crate::parser::work_counters::{tree_parses, SOURCE_READS};
        let reads = SOURCE_READS.with(|count| count.get());
        let parses = tree_parses();
        let response = serde_json::to_value(handle_outline(request, ctx)).unwrap();
        (
            response,
            SOURCE_READS.with(|count| count.get()) - reads,
            tree_parses() - parses,
        )
    }

    #[test]
    fn revision_outline_zoom_cache_retains_no_staged_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("lib.rs"), "pub fn historical() {}\n").unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["add", "lib.rs"],
            vec![
                "-c",
                "user.name=AFT",
                "-c",
                "user.email=aft@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        ] {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let storage = tempfile::tempdir().unwrap();
        let provider = TreeSitterProvider::new();
        let cache = provider.symbol_cache();
        let ctx = AppContext::new(
            Box::new(provider),
            crate::config::Config {
                project_root: Some(root.clone()),
                storage_dir: Some(storage.path().to_path_buf()),
                revision_reads: true,
                ..Default::default()
            },
        );
        for _ in 0..3 {
            for (command, params) in [
                ("outline", serde_json::json!({"file": root.join("lib.rs")})),
                (
                    "zoom",
                    serde_json::json!({"file": root.join("lib.rs"), "symbol": "historical"}),
                ),
            ] {
                let mut value = params;
                value["id"] = serde_json::json!(command);
                value["command"] = serde_json::json!(command);
                value["ref"] = serde_json::json!("HEAD");
                let request = serde_json::from_value(value).unwrap();
                let response = if command == "outline" {
                    handle_outline(&request, &ctx)
                } else {
                    crate::commands::zoom::handle_zoom(&request, &ctx)
                };
                assert!(response.success, "{response:?}");
                assert!(response.data[if command == "outline" {
                    "text"
                } else {
                    "content"
                }]
                .as_str()
                .unwrap()
                .contains("historical"));
                assert_eq!(
                    cache.read().unwrap().len(),
                    0,
                    "{command} left a staged cache entry"
                );
            }
        }
    }

    #[test]
    fn single_outline_cache_hit_does_no_source_work() {
        let (_temp, files, ctx) = navigation_fixture();
        let mut counts = Vec::new();
        for path in files {
            let request = serde_json::from_value(
                serde_json::json!({"id":"single-work", "command":"outline", "file":path}),
            )
            .unwrap();
            let (cold, reads, parses) = navigation_counts(&request, &ctx);
            counts.push((reads, parses));
            let (warm, reads, parses) = navigation_counts(&request, &ctx);
            assert_eq!(warm, cold);
            counts.push((reads, parses));
        }
        assert_eq!(counts, [(1, 1), (0, 0)].repeat(7));
    }

    #[test]
    fn multi_outline_shares_one_source_snapshot() {
        let (_temp, files, ctx) = navigation_fixture();
        let mut counts = Vec::new();
        for path in &files {
            let request = serde_json::from_value(
                serde_json::json!({"id":"multi-work", "command":"outline", "files":[path]}),
            )
            .unwrap();
            let (cold, reads, parses) = navigation_counts(&request, &ctx);
            counts.push((reads, parses));
            let (warm, reads, parses) = navigation_counts(&request, &ctx);
            assert_eq!(warm, cold);
            counts.push((reads, parses));
        }
        // A warm multi-file outline renders from the parse facts cached next
        // to each file's symbols, as a warm single-file outline already did.
        assert_eq!(counts, [(1, 1), (0, 0)].repeat(7));
        let batch_ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let request = serde_json::from_value(
            serde_json::json!({"id":"mixed-work", "command":"outline", "files":files}),
        )
        .unwrap();
        let (cold, reads, parses) = navigation_counts(&request, &batch_ctx);
        assert_eq!((reads, parses), (7, 7));
        let (warm, reads, parses) = navigation_counts(&request, &batch_ctx);
        assert_eq!((reads, parses), (0, 0));
        assert_eq!(warm, cold);
    }

    #[test]
    fn rust_disk_and_manual_tables_hydrate_layout_once() {
        let (temp, files, ctx) = navigation_fixture();
        let path = &files[5];
        let provider = ctx
            .provider()
            .as_any()
            .downcast_ref::<TreeSitterProvider>()
            .unwrap();
        let symbols = ctx.provider().list_symbols(path).unwrap();
        let cache = provider.symbol_cache();
        let storage = tempfile::tempdir().unwrap();
        cache
            .write()
            .unwrap()
            .set_project_root(temp.path().to_path_buf());
        let disk_cache = cache.read().unwrap().clone();
        crate::symbol_cache_disk::write_to_disk(&disk_cache, storage.path(), "layout").unwrap();
        for disk in [true, false] {
            let mut loaded = crate::parser::SymbolCache::new();
            if disk {
                assert_eq!(
                    loaded.load_from_disk(storage.path(), "layout", temp.path()),
                    1
                );
            } else {
                let meta = std::fs::metadata(path).unwrap();
                loaded.insert(
                    path.clone(),
                    meta.modified().unwrap(),
                    meta.len(),
                    blake3::hash(&std::fs::read(path).unwrap()),
                    symbols.clone(),
                );
            }
            let ctx = AppContext::new(
                Box::new(TreeSitterProvider::with_symbol_cache(Arc::new(
                    std::sync::RwLock::new(loaded),
                ))),
                crate::config::Config::default(),
            );
            let request = serde_json::from_value(
                serde_json::json!({"id":"layout", "command":"outline", "file":path}),
            )
            .unwrap();
            let (first, reads, parses) = navigation_counts(&request, &ctx);
            assert_eq!((reads, parses), (1, 1), "disk={disk}");
            let (warm, reads, parses) = navigation_counts(&request, &ctx);
            assert_eq!((reads, parses), (0, 0), "disk={disk}");
            assert_eq!(warm, first);
        }
    }

    #[test]
    fn rust_outline_layout_tracks_symbol_freshness_and_invalidation() {
        let (_temp, files, ctx) = navigation_fixture();
        let path = &files[5];
        let request = serde_json::from_value(
            serde_json::json!({"id":"fresh", "command":"outline", "file":path}),
        )
        .unwrap();
        let (first, _, _) = navigation_counts(&request, &ctx);
        assert!(first["text"].as_str().unwrap().contains("pub mod nested"));
        std::fs::write(
            path,
            "pub mod replacement { pub fn alpha() {} }\nfn beta() {}\n",
        )
        .unwrap();
        let (changed, reads, parses) = navigation_counts(&request, &ctx);
        assert_eq!((reads, parses), (1, 1));
        assert!(changed["text"]
            .as_str()
            .unwrap()
            .contains("pub mod replacement"));
        assert!(!changed["text"].as_str().unwrap().contains("nested"));
        let (warm, reads, parses) = navigation_counts(&request, &ctx);
        assert_eq!((reads, parses), (0, 0));
        assert_eq!(warm, changed);
        ctx.provider()
            .as_any()
            .downcast_ref::<TreeSitterProvider>()
            .unwrap()
            .symbol_cache()
            .write()
            .unwrap()
            .invalidate(path);
        let (invalidated, reads, parses) = navigation_counts(&request, &ctx);
        assert_eq!((reads, parses), (1, 1));
        assert_eq!(invalidated, changed);
    }

    #[test]
    fn rust_batch_derives_module_layout_once() {
        let (_temp, files, ctx) = navigation_fixture();
        let request = serde_json::from_value(
            serde_json::json!({"id":"layout-work", "command":"outline", "files":[files[5]]}),
        )
        .unwrap();
        // The first call scans the file root and its nested module once each;
        // the warm call reuses the cached layout and scans nothing.
        for expected in [2, 0] {
            let before = crate::parser::work_counters::RUST_MODULE_SCANS.with(|count| count.get());
            let response = handle_outline(&request, &ctx);
            assert!(response.success);
            let scans =
                crate::parser::work_counters::RUST_MODULE_SCANS.with(|count| count.get()) - before;
            assert_eq!(scans, expected);
        }
    }

    #[test]
    fn outline_snapshot_does_not_change_extracted_symbols_or_ranges() {
        let (_temp, files, ctx) = navigation_fixture();
        for path in files {
            let mut parser = FileParser::new();
            let parsed = parser.parse_snapshot(&path).unwrap();
            let reference =
                crate::parser::extract_symbols_from_tree(&parsed.source, &parsed.tree, parsed.lang)
                    .unwrap();
            assert_eq!(
                parser
                    .extract_symbols_from_snapshot(&path, &parsed)
                    .unwrap()
                    .as_ref(),
                &reference
            );
            let request = serde_json::from_value(
                serde_json::json!({"id":"symbols", "command":"outline", "file":path}),
            )
            .unwrap();
            assert!(handle_outline(&request, &ctx).success);
            assert_eq!(ctx.provider().list_symbols(&path).unwrap(), reference);
        }
    }

    #[test]
    fn zoom_navigation_output_goldens() {
        type Leaf<'a> = ([usize; 4], &'a str, &'a [&'a str], &'a [&'a str]);
        let ts: [Leaf<'_>; 2] = [
            (
                [1, 1, 1, 27],
                "export function alpha() {}",
                &[],
                &["function beta() {}"],
            ),
            (
                [2, 1, 2, 19],
                "function beta() {}",
                &["export function alpha() {}"],
                &[],
            ),
        ];
        let cases: [[Leaf<'_>; 2]; 7] = [
            ts,
            ts,
            ts,
            [
                (
                    [1, 1, 1, 18],
                    "def alpha(): pass",
                    &[],
                    &["def beta(): pass"],
                ),
                (
                    [2, 1, 2, 17],
                    "def beta(): pass",
                    &["def alpha(): pass"],
                    &[],
                ),
            ],
            [
                (
                    [2, 1, 2, 16],
                    "func alpha() {}",
                    &["package main"],
                    &["func beta() {}"],
                ),
                (
                    [3, 1, 3, 15],
                    "func beta() {}",
                    &["package main", "func alpha() {}"],
                    &[],
                ),
            ],
            [
                (
                    [2, 5, 2, 22],
                    "    pub fn alpha() {}",
                    &["pub mod nested {"],
                    &["}", "#[test]", "fn beta() {}"],
                ),
                (
                    [4, 1, 5, 13],
                    "#[test]\nfn beta() {}",
                    &["pub mod nested {", "    pub fn alpha() {}", "}"],
                    &[],
                ),
            ],
            [
                ([1, 1, 1, 16], "void alpha() {}", &[], &["void beta() {}"]),
                ([2, 1, 2, 15], "void beta() {}", &["void alpha() {}"], &[]),
            ],
        ];
        let (_temp, files, ctx) = navigation_fixture();
        for (path, leaves) in files.iter().zip(cases) {
            // Without a project root the manifest names the canonical file.
            let manifest_path = crate::inspect::job::canonicalize_normalized(path);
            let symbols = ["alpha", "beta"].into_iter().zip(leaves).map(|(name, (range, content, before, after))| serde_json::json!({
                "name":name, "response":{
                    "id":"zoom-golden", "success":true, "name":name, "kind":"function",
                    "range":{"start_line":range[0], "start_col":range[1], "end_line":range[2], "end_col":range[3]},
                    "content":content, "context_before":before, "context_after":after,
                    "annotations":{"calls_out":[], "called_by":[]},
                    "content_files":[&manifest_path]
                }
            })).collect::<Vec<_>>();
            let expected = serde_json::json!({"id":"zoom-golden", "success":true, "complete":true, "symbols":symbols, "content_files":[manifest_path]});
            let request = serde_json::from_value(serde_json::json!({"id":"zoom-golden", "command":"zoom", "file":path, "symbols":["alpha","beta"], "context_lines":3})).unwrap();
            for _ in 0..2 {
                assert_eq!(
                    serde_json::to_value(super::super::zoom::handle_zoom(&request, &ctx)).unwrap(),
                    expected
                );
            }
        }
    }

    #[test]
    fn navigation_output_goldens() {
        let (_temp, files, ctx) = navigation_fixture();
        let request = serde_json::from_value(
            serde_json::json!({"id":"golden", "command":"outline", "files":files}),
        )
        .unwrap();
        let response =
            canonical_content_files(serde_json::to_value(handle_outline(&request, &ctx)).unwrap());
        assert_eq!(
            response,
            serde_json::json!({"id":"golden","success":true,"text":concat!(
            "a.ts\n  E fn   alpha 1:1\n  - fn   beta 2:2\n",
            "b.tsx\n  E fn   alpha 1:1\n  - fn   beta 2:2\n",
            "c.js\n  E fn   alpha 1:1\n  - fn   beta 2:2\n",
            "d.py\n  - fn   alpha 1:1\n  - fn   beta 2:2\n",
            "e.go\n  - fn   alpha 2:2\n  - fn   beta 3:3\n",
            "f.rs\n  E modu nested 1:3\n    E fn   alpha 2:2\n  tests: 1 items (lines 4-5)\n",
            "g.cpp\n  - fn   alpha 1:1\n  - fn   beta 2:2\n"),
            "complete":true,"output_truncated":false,"files_shown":7,"structure_footer":"","skipped_files":[],
            "content_files":files.iter().map(|file| file.canonicalize().unwrap()).collect::<Vec<_>>()})
        );
        let bodies = [
            "  E function alpha() {} 1:1\n  function beta() {} 2:2\n",
            "  E function alpha() {} 1:1\n  function beta() {} 2:2\n",
            "  E function alpha() {} 1:1\n  function beta() {} 2:2\n",
            "  def alpha(): pass 1:1\n  def beta(): pass 2:2\n",
            "  func alpha() {} 2:2\n  func beta() {} 3:3\n",
            "  pub mod nested 1:3\n    .pub fn alpha() {} 2:2\n  fn beta() {} 4:5\n",
            "  void alpha() {} 1:1\n  void beta() {} 2:2\n",
        ];
        for (path, body) in files.iter().zip(bodies) {
            let request = serde_json::from_value(
                serde_json::json!({"id":"golden", "command":"outline", "file":path}),
            )
            .unwrap();
            let expected = serde_json::json!({"id":"golden", "success":true, "complete":true,
                "content_files":[path.canonicalize().unwrap()],
                "text":format!("{}\n{body}", path.file_name().unwrap().to_str().unwrap())});
            assert_eq!(
                canonical_content_files(
                    serde_json::to_value(handle_outline(&request, &ctx)).unwrap()
                ),
                expected
            );
        }
    }

    /// `content_files` names the files that were read, in the spelling the
    /// request resolved them to. On Windows that can be an 8.3 short name or a
    /// verbatim path, so the golden compares the files' canonical identities.
    fn canonical_content_files(mut response: serde_json::Value) -> serde_json::Value {
        if let Some(files) = response
            .get_mut("content_files")
            .and_then(serde_json::Value::as_array_mut)
        {
            for file in files {
                let path = PathBuf::from(file.as_str().expect("content_files holds paths"));
                *file = serde_json::json!(path.canonicalize().unwrap());
            }
        }
        response
    }

    fn planner_fixture() -> (Vec<OutlineDirectoryNode>, Vec<OutlineFileEntry>) {
        let files = (0..10_000)
            .map(|i| OutlineFileEntry {
                path: format!("root/d{:03}/f{i:05}.ts", i / 100),
                language: "typescript".into(),
                symbols: Some(i % 150),
                lines: Some(i + 1),
                absolute_path: PathBuf::new(),
                data_doc: false,
            })
            .collect::<Vec<_>>();
        let mut nodes = vec![OutlineDirectoryNode {
            path: "root".into(),
            depth: 0,
            direct_files: vec![],
            children: (1..=100).collect(),
            stats: OutlineDirectoryStats::default(),
        }];
        for i in 0..100 {
            nodes.push(OutlineDirectoryNode {
                path: format!("root/d{i:03}"),
                depth: 1,
                direct_files: (i * 100..(i + 1) * 100).collect(),
                children: vec![],
                stats: OutlineDirectoryStats {
                    files: 100,
                    code_files: 100,
                    lines: 1000,
                    code_lines: 1000,
                    ..Default::default()
                },
            });
        }
        (nodes, files)
    }

    #[test]
    fn files_planner_materializes_only_the_final_table() {
        let (nodes, files) = planner_fixture();
        let before = TABLE_RENDERS.with(|count| count.get());
        let rows = plan_outline_file_rows(&[0], &nodes, &files, 30 * 1024);
        assert!(!format_files_table(&rows, &nodes, &files, 30 * 1024).is_empty());
        assert_eq!(TABLE_RENDERS.with(|count| count.get()) - before, 1);
    }

    #[test]
    fn files_mode_cold_counts_do_not_copy_symbol_tables() {
        let (_temp, paths, ctx) = navigation_fixture();
        let mut entries = paths
            .iter()
            .map(|path| OutlineFileEntry {
                path: path_to_slash(path),
                language: "code".into(),
                symbols: None,
                lines: Some(2),
                absolute_path: path.clone(),
                data_doc: false,
            })
            .collect::<Vec<_>>();
        let rows = (0..entries.len())
            .map(OutlineTableRow::File)
            .collect::<Vec<_>>();
        let before = crate::parser::work_counters::OWNED_SYMBOL_COPIES.with(|count| count.get());
        populate_rendered_file_symbols(&rows, &mut entries, &ctx);
        assert!(entries.iter().all(|entry| entry.symbols == Some(2)));
        assert_eq!(
            crate::parser::work_counters::OWNED_SYMBOL_COPIES.with(|count| count.get()) - before,
            0
        );
    }

    #[test]
    fn files_planner_lengths_and_acceptance_match_rendering() {
        let (mut nodes, mut files) = planner_fixture();
        nodes[1].path = "root/界/🦀".repeat(30);
        nodes[2].stats.data_doc_files = 100;
        nodes[2].stats.code_files = 0;
        nodes[3].stats.unknown_lines = 1;
        files[0].path = "界/🦀/e\u{301}.ts".into();
        files[0].language = "🦀lang".into();
        files[0].symbols = Some(123456789);
        files[0].lines = Some(123456789);
        files[1].lines = None;
        files[1].language = "binary".into();
        for budget in [
            0,
            1,
            999,
            1000,
            1001,
            10_000,
            30 * 1024,
            30 * 1024 + 1,
            usize::MAX,
        ] {
            let mut candidates = 0;
            let measured = plan_outline_file_rows_with_length(
                &[0, 3],
                &nodes,
                &files,
                budget,
                |rows, nodes, files| {
                    candidates += 1;
                    let bytes = files_table_rendered_len(rows, nodes, files);
                    let rendered = format_files_table(rows, nodes, files, budget).len();
                    assert_eq!(bytes, rendered);
                    for boundary in [rendered.saturating_sub(1), rendered, rendered + 1] {
                        assert_eq!(bytes <= boundary, rendered <= boundary);
                    }
                    bytes
                },
            );
            let reference = plan_outline_file_rows_with_length(
                &[0, 3],
                &nodes,
                &files,
                budget,
                |rows, nodes, files| format_files_table(rows, nodes, files, budget).len(),
            );
            assert!(candidates > 0);
            assert_eq!(measured, reference);
            assert_eq!(
                format_files_table(&measured, &nodes, &files, budget),
                format_files_table(&reference, &nodes, &files, budget)
            );
        }
    }

    #[test]
    #[ignore = "manual release-mode files planner performance probe"]
    fn files_planner_perf_probe() {
        let (nodes, files) = planner_fixture();
        let mut times = Vec::new();
        for _ in 0..21 {
            let start = std::time::Instant::now();
            let rows = plan_outline_file_rows(&[0], &nodes, &files, 30 * 1024);
            std::hint::black_box(format_files_table(&rows, &nodes, &files, 30 * 1024));
            times.push(start.elapsed().as_micros());
        }
        times.sort();
        println!(
            "PLANNER PERF entries=10000 p50_us={} p95_us={}",
            times[10], times[19]
        );
    }

    #[test]
    #[ignore = "manual release-mode navigation performance probe"]
    fn navigation_perf_probe() {
        use std::time::Instant;
        for lines in [1_000, 10_000] {
            let temp = tempfile::tempdir().unwrap();
            let source = "// padding\n".repeat(lines - 2)
                + "export function alpha() {}\nfunction beta() {}\n";
            let files = (0..200)
                .map(|i| {
                    let path = temp.path().join(format!("f{i:03}.ts"));
                    std::fs::write(&path, &source).unwrap();
                    path
                })
                .collect::<Vec<_>>();
            let request = serde_json::from_value(
                serde_json::json!({"id":"perf", "command":"outline", "files":files}),
            )
            .unwrap();
            for cold in [true, false] {
                let mut timings = Vec::new();
                let mut ctx = AppContext::new(
                    Box::new(TreeSitterProvider::new()),
                    crate::config::Config::default(),
                );
                if !cold {
                    handle_outline(&request, &ctx);
                }
                for _ in 0..11 {
                    if cold {
                        ctx = AppContext::new(
                            Box::new(TreeSitterProvider::new()),
                            crate::config::Config::default(),
                        );
                    }
                    let start = Instant::now();
                    let (response, reads, parses) = navigation_counts(&request, &ctx);
                    timings.push(start.elapsed().as_micros());
                    assert_eq!(response["files_shown"], 200);
                    if timings.len() == 1 {
                        println!("lines={lines} cold={cold} reads={reads} parses={parses}");
                    }
                }
                timings.sort();
                println!(
                    "NAV PERF lines={lines} cold={cold} p50_us={} p95_us={}",
                    timings[5], timings[10]
                );
            }
        }
    }

    /// The same harness can be appended to the older outline implementation.
    /// Comparing separately compiled revisions avoids a reference path that
    /// accidentally includes the optimization it is supposed to measure.
    #[test]
    #[ignore = "manual paired-revision release-mode outline performance probe"]
    fn navigation_paired_revision_perf_probe() {
        use crate::parser::work_counters::tree_parses;
        let lines = std::env::var("AFT_OUTLINE_BENCH_LINES")
            .unwrap_or_else(|_| "1000".into())
            .parse::<usize>()
            .unwrap();
        let language = std::env::var("AFT_OUTLINE_BENCH_LANGUAGE").unwrap_or_else(|_| "ts".into());
        assert!(matches!(language.as_str(), "ts" | "rs"));
        let mode = std::env::var("AFT_OUTLINE_BENCH_MODE").unwrap_or_else(|_| "multi".into());
        assert!(matches!(mode.as_str(), "single" | "multi"));
        let warm = std::env::var("AFT_OUTLINE_BENCH_CACHE").is_ok_and(|value| value == "warm");
        let before =
            std::env::var("AFT_OUTLINE_BENCH_REVISION").is_ok_and(|value| value == "before");
        let temporary = tempfile::tempdir().unwrap();
        let fixture = std::env::var_os("AFT_OUTLINE_BENCH_FIXTURE")
            .map(PathBuf::from)
            .unwrap_or_else(|| temporary.path().to_path_buf());
        let source = "// padding\n".repeat(lines - 2)
            + if language == "ts" {
                "export function alpha() {}\nfunction beta() {}\n"
            } else {
                "pub fn alpha() {}\nfn beta() {}\n"
            };
        let files = (0..200)
            .map(|i| fixture.join(format!("f{i:03}.{language}")))
            .collect::<Vec<_>>();
        if std::env::var_os("AFT_OUTLINE_BENCH_FIXTURE").is_none() {
            for file in &files {
                std::fs::write(file, &source).unwrap();
            }
        }
        let mut grammar_warmup = FileParser::new();
        grammar_warmup.parse(&files[0]).unwrap();
        let request: RawRequest = serde_json::from_value(if mode == "multi" {
            serde_json::json!({"id":"paired", "command":"outline", "files":files})
        } else {
            serde_json::json!({"id":"paired", "command":"outline", "file":files[0]})
        })
        .unwrap();
        let iterations = if mode == "single" { 100 } else { 1 };
        let contexts = (0..iterations)
            .map(|_| {
                AppContext::new(
                    Box::new(TreeSitterProvider::new()),
                    crate::config::Config::default(),
                )
            })
            .collect::<Vec<_>>();
        if warm {
            for ctx in &contexts {
                assert!(handle_outline(&request, ctx).success);
            }
        }
        let parses_before = tree_parses();
        let start = std::time::Instant::now();
        let responses = contexts
            .iter()
            .map(|ctx| handle_outline(&request, ctx))
            .collect::<Vec<_>>();
        let elapsed_ns = start.elapsed().as_nanos();
        let parses = tree_parses() - parses_before;
        let expected_parses = if mode == "multi" {
            200
        } else if warm {
            if before {
                100
            } else {
                0
            }
        } else if before {
            200
        } else {
            100
        };
        assert_eq!(parses, expected_parses);
        let mut digest = blake3::Hasher::new();
        for response in responses {
            assert!(response.success);
            if mode == "multi" {
                assert_eq!(response.data["files_shown"], 200);
            }
            digest.update(&serde_json::to_vec(&response).unwrap());
        }
        println!(
            "AFT_PAIR_SAMPLE={}",
            serde_json::json!({
                "lines":lines, "language":language, "mode":mode, "cache":if warm {"warm"} else {"cold"},
                "iterations":iterations, "elapsed_ns":elapsed_ns, "parses":parses,
                "digest":digest.finalize().to_hex().to_string(),
            })
        );
    }

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
    fn outline_revision_regression_single_file_keeps_full_product_members_and_lists_tests() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("members.rs");
        std::fs::write(
            &path,
            format!(
                "{}\n#[cfg(test)]\nmod checks {{\n    #[test]\n    fn first_test() {{}}\n    #[test]\n    fn second_test() {{}}\n}}\n",
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
            text.split("  mod checks")
                .next()
                .unwrap()
                .lines()
                .filter(|line| line.trim_start().starts_with('.'))
                .count(),
            7,
            "{text}"
        );
        assert!(
            text.contains("first_test")
                && text.contains("second_test")
                && !text.contains("items")
                && !text.contains("more)"),
            "{text}"
        );
        for params in [
            serde_json::json!({"files":[path]}),
            serde_json::json!({"directory":temp.path()}),
        ] {
            let req = RawRequest {
                id: "broad".into(),
                command: "outline".into(),
                session_id: None,
                lsp_hints: None,
                params,
            };
            let response = handle_outline(&req, &ctx);
            assert!(response.success, "{response:?}");
            let broad = response.data["text"].as_str().unwrap();
            assert_eq!(
                broad
                    .lines()
                    .filter(|line| line.trim_start().starts_with('.'))
                    .count(),
                4,
                "{broad}"
            );
            assert!(
                broad.contains("(3 more)") && broad.contains("checks: 2 items"),
                "{broad}"
            );
            assert!(
                !broad.contains("first_test") && !broad.contains("second_test"),
                "{broad}"
            );
        }
    }

    fn restricted_outline_context(root: &Path) -> AppContext {
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.to_path_buf()),
                restrict_to_project_root: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn outline_included_test_outside_root_has_unknown_items() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("lib.rs");
        std::fs::write(
            &path,
            "pub fn product() {}\n#[cfg(test)]\n#[path = \"../outside.rs\"]\nmod checks;\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("outside.rs"), "fn outside_secret() {}\n").unwrap();
        let ctx = restricted_outline_context(&root);
        let req: RawRequest = serde_json::from_value(
            serde_json::json!({"id":"outside-include", "command":"outline", "files":[path]}),
        )
        .unwrap();
        let response = handle_outline(&req, &ctx);
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["complete"], true, "{response:?}");
        assert_eq!(response.data["skipped_files"], serde_json::json!([]));
        let text = response.data["text"].as_str().unwrap();
        assert!(
            text.contains("checks (path ../outside.rs): items unknown (path_denied) (lines 2-4)"),
            "{text}"
        );
        assert!(
            text.contains("product") && !text.contains("outside_secret"),
            "{text}"
        );
    }

    #[test]
    fn outline_missing_and_unparseable_test_includes_have_unknown_items() {
        for (source, reason) in [(None, "file_not_found"), (Some("fn (\n"), "parse_error")] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("lib.rs");
            std::fs::write(
                &path,
                "pub fn product() {}\n#[cfg(test)]\n#[path = \"checks.rs\"]\nmod checks;\n",
            )
            .unwrap();
            if let Some(source) = source {
                std::fs::write(temp.path().join("checks.rs"), source).unwrap();
            }
            let ctx = restricted_outline_context(temp.path());
            let req: RawRequest = serde_json::from_value(serde_json::json!({"id":"unavailable-include", "command":"outline", "files":[path]})).unwrap();
            let response = handle_outline(&req, &ctx);
            assert!(response.success, "{response:?}");
            assert_eq!(response.data["complete"], true, "{response:?}");
            assert_eq!(response.data["skipped_files"], serde_json::json!([]));
            let text = response.data["text"].as_str().unwrap();
            assert!(
                text.contains(&format!(
                    "checks (path checks.rs): items unknown ({reason}) (lines 2-4)"
                )),
                "{text}"
            );
            assert!(text.contains("product"), "{text}");
        }
    }

    #[test]
    fn outline_revision_regression_directory_bad_include_keeps_other_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("bad_include.rs"), "pub fn bad_include_product() {}\n#[cfg(test)]\n#[path = \"../outside.rs\"]\nmod checks;\n").unwrap();
        std::fs::write(root.join("good.rs"), "pub fn good_product() {}\n").unwrap();
        std::fs::write(temp.path().join("outside.rs"), "fn outside_secret() {}\n").unwrap();
        let ctx = restricted_outline_context(&root);
        let req: RawRequest = serde_json::from_value(serde_json::json!({"id":"directory-bad-include", "command":"outline", "directory":root})).unwrap();
        let response = handle_outline(&req, &ctx);
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["complete"], true, "{response:?}");
        assert_eq!(response.data["skipped_files"], serde_json::json!([]));
        let text = response.data["text"].as_str().unwrap();
        assert!(
            text.contains("bad_include_product") && text.contains("good_product"),
            "{text}"
        );
        assert!(
            text.contains("checks (path ../outside.rs): items unknown (path_denied)"),
            "{text}"
        );
        assert!(!text.contains("outside_secret"), "{text}");
    }

    #[test]
    fn outline_structure_read_and_parse_failures_skip_only_the_affected_file() {
        // Change a file after syntax validation and symbol extraction, without
        // relying on scheduling a concurrent writer at exactly the right time.
        struct ChangingProvider {
            affected: PathBuf,
            break_syntax: bool,
        }
        impl crate::language::LanguageProvider for ChangingProvider {
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn resolve_symbol(
                &self,
                file: &Path,
                name: &str,
            ) -> Result<Vec<crate::symbols::SymbolMatch>, AftError> {
                crate::language::LanguageProvider::resolve_symbol(
                    &TreeSitterProvider::new(),
                    file,
                    name,
                )
            }
            fn list_symbols(&self, file: &Path) -> Result<Vec<Symbol>, AftError> {
                let symbols = FileParser::new().extract_symbols(file)?;
                if file == self.affected {
                    if self.break_syntax {
                        std::fs::write(file, "fn (\n").unwrap();
                    } else {
                        std::fs::remove_file(file).unwrap();
                    }
                }
                Ok(symbols)
            }
        }
        for (break_syntax, reason) in [(false, "file_not_found"), (true, "parse_error")] {
            let temp = tempfile::tempdir().unwrap();
            let affected = temp.path().join("affected.rs");
            let good = temp.path().join("good.rs");
            std::fs::write(&affected, "pub fn affected() {}\n").unwrap();
            std::fs::write(&good, "pub fn good() {}\n").unwrap();
            let ctx = AppContext::new(
                Box::new(ChangingProvider {
                    affected: affected.clone(),
                    break_syntax,
                }),
                crate::config::Config::default(),
            );
            let req: RawRequest = serde_json::from_value(serde_json::json!({"id":"changed-file", "command":"outline", "files":[affected,good]})).unwrap();
            let response = handle_outline(&req, &ctx);
            assert!(response.success, "{response:?}");
            assert_eq!(response.data["complete"], false);
            assert_eq!(
                response.data["skipped_files"],
                serde_json::json!([{"file":"affected.rs", "reason":reason}])
            );
            let text = response.data["text"].as_str().unwrap();
            assert!(
                text.contains("good.rs") && !text.contains("affected.rs"),
                "{text}"
            );
        }
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
        let (rendered, _) = outline_many_files(
            &files,
            &ctx,
            &files_request(&files),
            None,
            &HashSet::new(),
            MultiFileBudget {
                max_bytes: 550,
                total_requested: 3,
                walk_incomplete: false,
                files_capped: false,
            },
        )
        .unwrap();
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

    #[test]
    fn outline_portability_goldens_stay_lf_with_autocrlf() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = format!("{}/", path_to_slash(temp.path()));
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("canonicalize repository root");
        for (name, expected) in [
            (
                "test_free.txt",
                include_str!("../../tests/fixtures/outline_summaries/test_free.txt"),
            ),
            (
                "structure.txt",
                include_str!("../../tests/fixtures/outline_summaries/structure.txt"),
            ),
            (
                "structure_common_root.txt",
                include_str!("../../tests/fixtures/outline_summaries/structure_common_root.txt"),
            ),
        ] {
            let relative = format!("crates/aft/tests/fixtures/outline_summaries/{name}");
            let output = std::process::Command::new("git")
                .args(["-c", "safe.directory=", "-c", "core.autocrlf=true"])
                .arg("-c")
                .arg(format!("safe.directory={}", root.display()))
                .args([
                    "checkout-index",
                    "--force",
                    &format!("--prefix={prefix}"),
                    "--",
                    &relative,
                ])
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let bytes = std::fs::read(temp.path().join(relative)).unwrap();
            assert!(
                !bytes.contains(&b'\r'),
                "{name} was converted to CRLF during checkout"
            );
            assert_eq!(
                bytes,
                expected.as_bytes(),
                "{name} checkout changed its golden bytes"
            );
        }
    }

    #[test]
    fn outline_portability_include_labels_preserve_parent_segments() {
        let temp = tempfile::tempdir().unwrap();
        let parent = std::fs::canonicalize(temp.path()).unwrap();
        assert_eq!(
            test_module_include_label(&parent, &parent, Path::new("../outside.rs")),
            "../outside.rs"
        );
        assert_eq!(
            test_module_include_label(
                &parent,
                &parent.join("product/outer"),
                Path::new("../checks.rs")
            ),
            "product/outer/../checks.rs"
        );
        assert_eq!(
            test_module_include_label(&parent, &parent.join("product"), Path::new("tests/mod.rs")),
            "product/tests/mod.rs"
        );
    }

    #[test]
    fn outline_portability_crlf_source_renders_lf_text() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test_free.ts");
        let source = include_str!("../../tests/fixtures/outline_summaries/test_free.ts");
        std::fs::write(&path, source.replace('\n', "\r\n")).unwrap();
        let text = multi_file_text(&[path]);
        assert_eq!(
            text,
            include_str!("../../tests/fixtures/outline_summaries/test_free.txt")
        );
        assert!(!text.contains('\r'));
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

    /// A warm multi-file outline renders test summaries, module layout and
    /// trait method previews from the facts cached next to the symbols, with
    /// no read or parse, and gives the same text as the cold call. A file
    /// edited since is read and parsed again and shows its new content.
    #[test]
    fn multi_file_outline_warm_cache_skips_parse_and_reparses_edited_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rust = temp.path().join("lib.rs");
        let script = temp.path().join("suite.ts");
        std::fs::write(
            &rust,
            "pub trait Shape {\n    fn area(&self) -> f64;\n    fn name(&self) -> String;\n}\npub mod inner {\n    pub fn helper() {}\n}\npub fn product() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn checks() {}\n}\n",
        )
        .unwrap();
        std::fs::write(
            &script,
            "export function visible() {}\ndescribe(\"suite\", () => {\n  it(\"works\", () => {});\n});\n",
        )
        .unwrap();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "warm-batch", "command": "outline", "files": [&rust, &script],
        }))
        .unwrap();

        let (cold, reads, parses) = navigation_counts(&request, &ctx);
        assert_eq!((reads, parses), (2, 2), "{cold}");
        let text = cold["text"].as_str().unwrap();
        for expected in [
            ".fn area(&self) -> f64;",
            "modu inner",
            "tests: 1 items",
            "describe \"suite\"",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text}");
        }

        let (warm, reads, parses) = navigation_counts(&request, &ctx);
        assert_eq!((reads, parses), (0, 0), "warm outline read or parsed");
        assert_eq!(warm, cold);

        std::fs::write(
            &rust,
            "pub trait Shape {\n    fn perimeter(&self) -> f64;\n}\npub fn renamed_product() {}\n",
        )
        .unwrap();
        let (edited, reads, parses) = navigation_counts(&request, &ctx);
        assert_eq!(
            (reads, parses),
            (1, 1),
            "only the edited file is parsed again"
        );
        let text = edited["text"].as_str().unwrap();
        assert!(
            text.contains("renamed_product") && text.contains(".fn perimeter"),
            "{text}"
        );
        assert!(!text.contains("area") && !text.contains("tests:"), "{text}");

        let (again, reads, parses) = navigation_counts(&request, &ctx);
        assert_eq!((reads, parses), (0, 0));
        assert_eq!(again, edited);
    }

    /// Once the rendered output budget is spent, the remaining files are not
    /// read or parsed: they are reported as not shown through `files_shown`
    /// and the footer. Only the file that overflowed the budget is parsed
    /// beyond the shown ones.
    #[test]
    fn multi_file_outline_stops_parsing_once_output_budget_is_spent() {
        use crate::parser::work_counters::tree_parses;
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().canonicalize().unwrap();
        // About 21 KB of rendered rows per file: one fits the 30 KB budget.
        let source = (0..700)
            .map(|index| format!("export function symbol_{index:04}() {{}}\n"))
            .collect::<String>();
        let mut files = Vec::new();
        for index in 0..10 {
            let path = root.join(format!("file_{index:02}.ts"));
            std::fs::write(&path, &source).unwrap();
            files.push(path);
        }
        for params in [
            serde_json::json!({"files": files}),
            serde_json::json!({"directory": root}),
        ] {
            // A fresh context each time, so no file is warm in the cache.
            let ctx = AppContext::new(
                Box::new(TreeSitterProvider::new()),
                crate::config::Config::default(),
            );
            let mut args = params;
            args["id"] = serde_json::json!("budget-stop");
            args["command"] = serde_json::json!("outline");
            let request: RawRequest = serde_json::from_value(args).unwrap();
            let before = tree_parses();
            let response = handle_outline(&request, &ctx);
            let parses = tree_parses() - before;
            assert!(response.success, "{response:?}");
            assert_eq!(response.data["files_shown"], 1);
            assert_eq!(response.data["complete"], false);
            assert_eq!(response.data["output_truncated"], true);
            let text = response.data["text"].as_str().unwrap();
            assert!(text.contains("shown 1 of 10 files (budget)"), "{text}");
            assert_eq!(parses, 2, "parsed files that could not be shown");
        }
    }

    fn small_ts_files(root: &Path, count: usize) -> Vec<PathBuf> {
        (0..count)
            .map(|index| {
                let path = root.join(format!("f{index:03}.ts"));
                std::fs::write(&path, format!("export function f{index}() {{}}\n")).unwrap();
                path
            })
            .collect()
    }

    fn files_request(files: &[impl serde::Serialize]) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": "files-bound", "command": "outline", "files": files,
        }))
        .unwrap()
    }

    /// A `files` list longer than the directory cap is cut to it before any
    /// path is validated or parsed; the footer counts the rest as not shown.
    #[test]
    fn multi_file_outline_caps_the_file_list_at_the_directory_cap() {
        use crate::parser::work_counters::tree_parses;
        let temp = tempfile::tempdir().expect("tempdir");
        let files = small_ts_files(temp.path(), OUTLINE_FILE_WALK_CAP + 5);
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let before = tree_parses();
        let response = handle_outline(&files_request(&files), &ctx);
        let parses = tree_parses() - before;
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["files_shown"], OUTLINE_FILE_WALK_CAP);
        assert_eq!(response.data["complete"], false);
        assert_eq!(response.data["output_truncated"], false);
        let text = response.data["text"].as_str().unwrap();
        assert!(text.contains("shown 200 of 205 files (budget)"), "{text}");
        assert!(
            text.contains("f199.ts") && !text.contains("f200.ts"),
            "{text}"
        );
        assert_eq!(parses, OUTLINE_FILE_WALK_CAP);
    }

    /// The access probe that runs before the outline handler checks only the
    /// `files` entries the handler examines: the first 200, or none when a
    /// directory outline ignores the list.
    #[test]
    fn pre_handler_probe_checks_only_the_examined_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let files = small_ts_files(temp.path(), 1_000);
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        for (mut params, expected) in [
            (serde_json::json!({"files": files}), OUTLINE_FILE_WALK_CAP),
            (
                serde_json::json!({"files": files, "directory": temp.path()}),
                1,
            ),
        ] {
            params["id"] = serde_json::json!("probe-bound");
            params["command"] = serde_json::json!("outline");
            let request: RawRequest = serde_json::from_value(params).unwrap();
            crate::bounded_io::COMMAND_INPUT_PROBES.with(|probes| probes.set(0));
            let response = handle_outline(&request, &ctx);
            assert!(response.success, "{response:?}");
            assert_eq!(
                crate::bounded_io::COMMAND_INPUT_PROBES.with(std::cell::Cell::get),
                expected
            );
        }
    }

    /// A batch that reaches the request deadline between files returns the
    /// files outlined so far as a partial result instead of running on.
    #[test]
    fn multi_file_outline_returns_partial_result_at_the_deadline() {
        use crate::parser::work_counters::tree_parses;
        let temp = tempfile::tempdir().expect("tempdir");
        let files = small_ts_files(temp.path(), 10);
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        OUTLINE_BATCH_DEADLINE_AFTER_FILES.with(|limit| limit.set(Some(3)));
        let before = tree_parses();
        let response = handle_outline(&files_request(&files), &ctx);
        let parses = tree_parses() - before;
        OUTLINE_BATCH_DEADLINE_AFTER_FILES.with(|limit| limit.set(None));
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["files_shown"], 3);
        assert_eq!(response.data["complete"], false);
        assert_eq!(response.data["deadline_reached"], true);
        let text = response.data["text"].as_str().unwrap();
        assert!(
            text.contains("shown 3 of ≥10 files (time limit) · narrow: path"),
            "{text}"
        );
        assert_eq!(
            response.data["structure_footer"],
            "shown 3 of ≥10 files (time limit) · narrow: path"
        );
        assert!(
            text.contains("f002.ts") && !text.contains("f003.ts"),
            "{text}"
        );
        assert_eq!(parses, 3);
    }

    /// The deadline check reads the real request budget: with less than the
    /// return margin left, the batch stops before outlining any file and
    /// still answers successfully.
    #[test]
    fn multi_file_outline_stops_when_the_request_deadline_is_near() {
        let temp = tempfile::tempdir().expect("tempdir");
        let files = small_ts_files(temp.path(), 4);
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let budget = crate::bounded_io::Budget::after(std::time::Duration::from_millis(900));
        let response = crate::bounded_io::with_budget(Some(budget), || {
            handle_outline(&files_request(&files), &ctx)
        });
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["files_shown"], 0);
        assert_eq!(response.data["complete"], false);
        assert_eq!(response.data["deadline_reached"], true);
        let text = response.data["text"].as_str().unwrap();
        assert!(text.contains("shown 0 of ≥4 files (time limit)"), "{text}");
    }

    /// A deadline stop reports the time-limit reason, not the budget one,
    /// and its longer footer still fits the output budget: files were
    /// admitted with room for the shorter budget footer, so the last one is
    /// taken back when the time-limit footer would not fit.
    #[test]
    fn deadline_footer_reads_time_limit_and_stays_within_the_budget() {
        let temp = tempfile::tempdir().expect("tempdir");
        let files = small_ts_files(temp.path(), 2)
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let run = |max_bytes: usize| {
            OUTLINE_BATCH_DEADLINE_AFTER_FILES.with(|limit| limit.set(Some(1)));
            let (rendered, _) = outline_many_files(
                &files,
                &ctx,
                &files_request(&files),
                None,
                &HashSet::new(),
                MultiFileBudget {
                    max_bytes,
                    total_requested: 2,
                    walk_incomplete: false,
                    files_capped: false,
                },
            )
            .unwrap();
            OUTLINE_BATCH_DEADLINE_AFTER_FILES.with(|limit| limit.set(None));
            rendered
        };
        let roomy = run(30 * 1024);
        assert_eq!(roomy.shown, 1);
        assert_eq!(
            roomy.footer,
            "shown 1 of ≥2 files (time limit) · narrow: path"
        );
        assert!(!roomy.footer.contains("budget"));

        // Room for the first file plus the budget footer the writer reserves,
        // but not for the longer time-limit footer.
        let block = roomy.text.len() - 1 - roomy.footer.len();
        let reserved = outline_structure_footer(2, 2, true, false, false).len() + 2;
        assert!(roomy.footer.len() + 1 > reserved);
        let tight = run(block + reserved);
        assert!(tight.text.len() <= block + reserved, "{}", tight.text);
        assert_eq!(tight.shown, 0);
        assert!(tight.content_files.is_empty());
        assert!(tight.footer.contains("(time limit)"), "{}", tight.footer);
    }

    /// Run a three-file batch that stops at the deadline after two files,
    /// with `max_bytes` from `budget(first two blocks)`.
    fn deadline_after_two(
        files: &[String],
        ctx: &AppContext,
        budget: impl Fn(usize) -> usize,
    ) -> RenderedOutline {
        let run = |max_bytes: usize| {
            OUTLINE_BATCH_DEADLINE_AFTER_FILES.with(|limit| limit.set(Some(2)));
            let (rendered, _) = outline_many_files(
                files,
                ctx,
                &files_request(files),
                None,
                &HashSet::new(),
                MultiFileBudget {
                    max_bytes,
                    total_requested: files.len(),
                    walk_incomplete: false,
                    files_capped: false,
                },
            )
            .unwrap();
            OUTLINE_BATCH_DEADLINE_AFTER_FILES.with(|limit| limit.set(None));
            rendered
        };
        let roomy = run(30 * 1024);
        assert_eq!(roomy.shown, 2, "{}", roomy.text);
        run(budget(roomy.text.len() - 1 - roomy.footer.len()))
    }

    fn manifest_names(rendered: &RenderedOutline) -> Vec<String> {
        rendered
            .content_files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// When the deadline footer takes back the last shown block, the read
    /// manifest keeps that block's path if another block still shown came
    /// from the same file (a `files` list can repeat a path), and drops it
    /// when the dropped block was the file's only one.
    #[test]
    fn deadline_take_back_keeps_a_path_still_shown_in_the_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = small_ts_files(temp.path(), 3)
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        // Room for both blocks plus the budget footer the writer reserves,
        // but not for the longer time-limit footer: the second block goes.
        let reserved = outline_structure_footer(3, 3, true, false, false).len() + 2;
        let tight = |blocks: usize| blocks + reserved;

        let repeated = vec![paths[0].clone(), paths[0].clone(), paths[2].clone()];
        let rendered = deadline_after_two(&repeated, &ctx, tight);
        assert_eq!(rendered.shown, 1, "{}", rendered.text);
        assert!(rendered.text.contains("f000.ts"), "{}", rendered.text);
        assert!(
            rendered.footer.contains("(time limit)"),
            "{}",
            rendered.footer
        );
        assert_eq!(
            manifest_names(&rendered),
            ["f000.ts"],
            "the copy still shown must stay in the read manifest"
        );

        let distinct = paths.clone();
        let rendered = deadline_after_two(&distinct, &ctx, tight);
        assert_eq!(rendered.shown, 1, "{}", rendered.text);
        assert!(!rendered.text.contains("f001.ts"), "{}", rendered.text);
        assert_eq!(
            manifest_names(&rendered),
            ["f000.ts"],
            "a file whose only block was dropped must leave the manifest"
        );
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

    fn files_mode_ignore_fixture(root: &Path, rules: &str) {
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(root.join(".git/info/exclude"), rules).unwrap();
    }

    fn files_mode_fixture_response(root: &Path, target: &Path) -> Response {
        let ctx = restricted_outline_context(root);
        let req: RawRequest = serde_json::from_value(serde_json::json!({
            "id":"files-mode-fixture", "command":"outline", "directory":target, "files":true,
        }))
        .unwrap();
        handle_outline(&req, &ctx)
    }

    #[test]
    fn files_mode_revision_regression_explicit_ignored_target_lists_files() {
        let temp = tempfile::tempdir().unwrap();
        files_mode_ignore_fixture(
            temp.path(),
            "ignored-parent/\nignored-parent/evidence/nested/\nignored-parent/evidence/drop.ts\n",
        );
        let target = temp.path().join("ignored-parent/evidence");
        std::fs::create_dir_all(target.join("nested")).unwrap();
        std::fs::write(target.join("keep.ts"), "export function keep() {}\n").unwrap();
        std::fs::write(target.join("drop.ts"), "export function drop() {}\n").unwrap();
        std::fs::write(
            target.join("nested/private.ts"),
            "export function private() {}\n",
        )
        .unwrap();
        let response = files_mode_fixture_response(temp.path(), &target);
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["complete"], true, "{response:?}");
        let files = response.data["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{response:?}");
        assert_eq!(files[0]["path"], "keep.ts");
        let text = response.data["text"].as_str().unwrap();
        assert!(
            text.contains("keep.ts") && !text.contains("private.ts") && !text.contains("drop.ts"),
            "{text}"
        );
    }

    #[test]
    fn files_mode_revision_regression_nested_ignored_folder_stays_skipped() {
        let temp = tempfile::tempdir().unwrap();
        files_mode_ignore_fixture(temp.path(), "nested/\n");
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        std::fs::write(temp.path().join("keep.ts"), "export function keep() {}\n").unwrap();
        std::fs::write(
            temp.path().join("nested/private.ts"),
            "export function private() {}\n",
        )
        .unwrap();
        let response = files_mode_fixture_response(temp.path(), temp.path());
        assert!(response.success, "{response:?}");
        let files = response.data["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{response:?}");
        assert_eq!(files[0]["path"], "keep.ts");
        assert_eq!(response.data["complete"], true);
    }

    #[test]
    fn files_mode_zero_results_explain_empty_ignored_and_test_only_targets() {
        for (kind, reason) in [
            ("empty", "empty folder"),
            ("ignored", "all files ignored or excluded"),
            ("tests", "test files excluded"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let target = temp.path().join("target");
            std::fs::create_dir(&target).unwrap();
            match kind {
                "ignored" => {
                    files_mode_ignore_fixture(temp.path(), "target/nested/\n");
                    std::fs::create_dir(target.join("nested")).unwrap();
                    std::fs::write(
                        target.join("nested/private.ts"),
                        "export function private() {}\n",
                    )
                    .unwrap();
                }
                "tests" => {
                    std::fs::write(target.join("sample_test.rs"), "#[test]\nfn case() {}\n")
                        .unwrap();
                }
                _ => {}
            }
            let response = files_mode_fixture_response(temp.path(), &target);
            assert!(response.success, "{response:?}");
            assert_eq!(response.data["complete"], true, "{response:?}");
            assert_eq!(response.data["files"], serde_json::json!([]));
            let text = response.data["text"].as_str().unwrap();
            assert!(
                text.contains("0 files under") && text.contains(reason),
                "{kind}: {text}"
            );
        }
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
        let (rendered, skipped) = outline_many_files(
            &files,
            &ctx,
            &files_request(&files),
            None,
            &HashSet::new(),
            MultiFileBudget {
                max_bytes: 30 * 1024,
                total_requested: files.len(),
                walk_incomplete: false,
                files_capped: false,
            },
        )
        .expect("outline fixture files");
        assert!(skipped.is_empty(), "{skipped:?}");
        rendered.text
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

#[cfg(test)]
mod audit11_regressions {
    use super::*;

    /// The parser's tree cache treats an equal-size, equal-mtime file as
    /// unchanged. Test regions must still be located in the bytes outline just
    /// read, not in a cached tree of the file's earlier contents.
    #[test]
    fn structure_entries_parse_the_source_they_read() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("product.rs");
        let earlier = "#[cfg(test)]\r\nmod checks { fn case() {} }\r\npub fn product() {}\r\n";
        let current = "pub fn product() {}\r\n#[cfg(test)]\r\nmod checks { fn case() {} }\r\n";
        assert_eq!(earlier.len(), current.len());
        let mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);

        std::fs::write(&path, earlier).unwrap();
        filetime::set_file_mtime(&path, mtime).unwrap();
        let mut parser = FileParser::new();
        parser.parse(&path).unwrap();

        std::fs::write(&path, current).unwrap();
        filetime::set_file_mtime(&path, mtime).unwrap();
        let symbols = crate::parser::parse_source_with_cached_parser(&path, current, LangId::Rust)
            .and_then(|tree| crate::parser::extract_symbols_from_tree(current, &tree, LangId::Rust))
            .unwrap();

        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let entries = outline_structure_entries(
            &path,
            &symbols,
            &mut parser,
            &ctx,
            "outline-fixture",
            "fixture-session",
            true,
            None,
        )
        .unwrap();
        let rows = entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.range.start_line))
            .collect::<Vec<_>>();
        assert_eq!(rows, vec![("product", 0), ("checks", 1)], "{entries:?}");
    }
}
