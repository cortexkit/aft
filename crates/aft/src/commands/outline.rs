use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::commands::read::{handle_github_outline, is_github_read_target};
use crate::context::AppContext;
use crate::edit;
use crate::error::AftError;
use crate::inspect::job::is_test_file;
use crate::parser::{detect_language, LangId};
use crate::protocol::{RawRequest, Response};
use crate::symbols::{Range, Symbol};
use crate::url_fetch::{fetch_url_to_cache, is_http_url, UrlFetchOptions};

const MAX_OUTLINE_FILE_BYTES: u64 = 50 * 1024 * 1024;
const BINARY_SAMPLE_BYTES: usize = 4 * 1024;
const OUTLINE_FILE_WALK_CAP: usize = 200;
const OUTLINE_FILE_COLLECTION_CAP: usize = 10_000;

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

        let text = format_multi_file_tree(&file_outlines, MAX_OUTPUT_BYTES, files.len());
        return Response::success(
            &req.id,
            serde_json::json!({
                "text": text,
                "complete": !discovery.walk_truncated
                    && !discovery.collection_truncated
                    && discovery.skipped_foreign_mounts == 0,
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

        let text = format_multi_file_tree(&file_outlines, MAX_OUTPUT_BYTES, total_files_requested);
        // Honest reporting: complete only when no requested file was skipped.
        // skipped_files names the gaps (missing/unreadable/unparseable inputs).
        return Response::success(
            &req.id,
            serde_json::json!({
                "text": text,
                "complete": skipped_files.is_empty(),
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

    let entries = build_outline_tree(&symbols);
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
    symbols: usize,
    lines: Option<usize>,
    #[serde(skip)]
    data_doc: bool,
}

#[derive(Debug, Clone, Default)]
struct OutlineDirectoryStats {
    dirs: usize,
    files: usize,
    lines: usize,
    symbols: usize,
    code_files: usize,
    code_lines: usize,
    languages: HashMap<String, usize>,
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

#[derive(Debug, Clone, Copy)]
struct OutlineFileContentStats {
    binary: bool,
    lines: Option<usize>,
}

#[derive(Debug, Clone)]
struct OutlineWalkOptions {
    gitignore: Option<Arc<ignore::gitignore::Gitignore>>,
    gitignore_root: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct OutlineFileDiscovery {
    files: Vec<String>,
    directories: Vec<String>,
    walk_truncated: bool,
    collection_truncated: bool,
    skipped_foreign_mounts: usize,
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
        let discovery = discover_outline_files_for_files_mode(&dir_path, ctx);
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
    let text = format_files_table(&rows, &directory_nodes, &file_entries, max_output_bytes);
    let rollup_count = rows
        .iter()
        .filter(|row| matches!(row, OutlineTableRow::Rollup(_)))
        .count();

    let mut unchecked_files = Vec::new();
    if walk_truncated {
        unchecked_files
            .push("<additional files not counted: 10000-file walk limit reached>".to_string());
    }
    if collection_truncated {
        unchecked_files.push("<additional files not counted: directory walk failed>".to_string());
    }
    if skipped_foreign_mounts > 0 {
        unchecked_files.push(format!(
            "<{skipped_foreign_mounts} foreign filesystem mount(s) not traversed>"
        ));
    }

    file_entries.sort_by(|a, b| a.path.cmp(&b.path));
    Response::success(
        &req.id,
        serde_json::json!({
            "text": text,
            "files": file_entries,
            "complete": !walk_truncated
                && !collection_truncated
                && skipped_foreign_mounts == 0,
            "walk_truncated": walk_truncated,
            "walk_limit": OUTLINE_FILE_COLLECTION_CAP,
            "collection_truncated": collection_truncated,
            "skipped_foreign_mounts": skipped_foreign_mounts,
            "unchecked_files": unchecked_files,
            "rollup_count": rollup_count,
        }),
    )
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

fn discover_outline_files_for_files_mode(
    directory: &Path,
    ctx: &AppContext,
) -> OutlineFileDiscovery {
    let gitignore = ctx.gitignore();
    let gitignore_root = ctx
        .config()
        .project_root
        .as_ref()
        .and_then(|root| std::fs::canonicalize(root).ok());
    let options = OutlineWalkOptions {
        gitignore,
        gitignore_root,
    };
    discover_outline_files_with_options(directory, Some(&options))
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
        let Some(entry) = outline_file_entry(&path, display_root, ctx) else {
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
        *stats.languages.entry(entry.language.clone()).or_default() += 1;
        if !entry.data_doc {
            stats.symbols += entry.symbols;
            stats.code_files += 1;
            stats.code_lines += entry.lines.unwrap_or(0);
        }
    }
    for child in children {
        let child_stats = aggregate_outline_directory(child, directory_nodes, file_entries);
        stats.dirs += child_stats.dirs + 1;
        stats.files += child_stats.files;
        stats.lines += child_stats.lines;
        stats.symbols += child_stats.symbols;
        stats.code_files += child_stats.code_files;
        stats.code_lines += child_stats.code_lines;
        for (language, count) in child_stats.languages {
            *stats.languages.entry(language).or_default() += count;
        }
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

fn directory_is_data_heavy(node: &OutlineDirectoryNode, file_entries: &[OutlineFileEntry]) -> bool {
    !node.direct_files.is_empty()
        && node
            .direct_files
            .iter()
            .filter(|file_id| file_entries[**file_id].data_doc)
            .count()
            * 10
            >= node.direct_files.len() * 9
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
    // A depth-first alphabetical list lets a schema subtree consume the whole
    // response before later crates appear. Start with every direct entry and
    // replace rollups breadth-first only when the complete rendered view fits.
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
                        && !directory_is_data_heavy(&directory_nodes[*node_id], file_entries) =>
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
                        && !directory_is_data_heavy(&directory_nodes[*node_id], file_entries) =>
                {
                    Some(*node_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| {
            compare_directory_code_share(&directory_nodes[*a], &directory_nodes[*b])
                .reverse()
                .then_with(|| directory_nodes[*a].path.cmp(&directory_nodes[*b].path))
        });

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
            }
        }
    }

    rows
}

fn outline_file_entry(
    path: &Path,
    display_root: &Path,
    ctx: &AppContext,
) -> Option<OutlineFileEntry> {
    let metadata = std::fs::metadata(path).ok()?;
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
    let data_doc = is_data_doc_outline_file(path, detected_language);

    let symbols = if content.binary {
        0
    } else if let Some(symbols) = cached_symbol_count(ctx, path, &metadata) {
        symbols
    } else if detected_language.is_none() || metadata.len() > MAX_OUTLINE_FILE_BYTES {
        0
    } else {
        ctx.provider()
            .list_symbols(path)
            .map(|symbols| symbols.len())
            .unwrap_or(0)
    };

    Some(OutlineFileEntry {
        path: rel_path,
        language: language.to_string(),
        symbols,
        lines: content.lines,
        data_doc,
    })
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
    }
}

fn format_files_table(
    rows: &[OutlineTableRow],
    directory_nodes: &[OutlineDirectoryNode],
    file_entries: &[OutlineFileEntry],
    max_bytes: usize,
) -> String {
    let rendered_rows = rows
        .iter()
        .map(|row| match row {
            OutlineTableRow::File(file_id) => {
                let entry = &file_entries[*file_id];
                (
                    entry.path.clone(),
                    entry.language.clone(),
                    entry.symbols,
                    entry.lines.map(|lines| lines.to_string()),
                )
            }
            OutlineTableRow::Rollup(node_id) => {
                let node = &directory_nodes[*node_id];
                (
                    format!("{}/", node.path.trim_end_matches('/')),
                    directory_rollup_language(&node.stats),
                    node.stats.symbols,
                    Some(node.stats.lines.to_string()),
                )
            }
        })
        .collect::<Vec<_>>();
    let path_width = rendered_rows
        .iter()
        .map(|(path, _, _, _)| path.len())
        .max()
        .unwrap_or(0);
    let language_width = rendered_rows
        .iter()
        .map(|(_, language, _, _)| language.len())
        .max()
        .unwrap_or("language".len())
        .max(8);

    let mut output = String::new();
    for (path, language, symbols, lines) in rendered_rows {
        output.push_str(&format!(
            "{path:<path_width$}  {language:<language_width$} {symbols:>5} syms {lines:>7} lines\n",
            lines = lines.as_deref().unwrap_or("-"),
        ));
    }

    let rollup_count = rows
        .iter()
        .filter(|row| matches!(row, OutlineTableRow::Rollup(_)))
        .count();
    if rollup_count > 0 {
        let (directory_word, rollup_phrase, expand_phrase) = if rollup_count == 1 {
            ("directory", "a rollup", "it")
        } else {
            ("directories", "rollups", "one")
        };
        output.push_str(&format!(
            "\n{rollup_count} {directory_word} shown as {rollup_phrase} (budget: {}); \
             expand {expand_phrase} with aft_outline <dir> files:true\n",
            format_outline_budget(max_bytes),
        ));
    }
    output
}

fn directory_rollup_language(stats: &OutlineDirectoryStats) -> String {
    let file_word = if stats.files == 1 { "file" } else { "files" };
    let dir_word = if stats.dirs == 1 { "dir" } else { "dirs" };
    let mut languages = stats.languages.iter().collect::<Vec<_>>();
    languages.sort_by(|(a, _), (b, _)| a.cmp(b));

    let files = if let [(language, _)] = languages.as_slice() {
        format!("{} {} {file_word}", stats.files, language)
    } else {
        format!("{} {file_word}", stats.files)
    };
    if stats.dirs == 0 {
        files
    } else if stats.files == 0 {
        format!("{} {dir_word}", stats.dirs)
    } else {
        format!("{files}, {} {dir_word}", stats.dirs)
    }
}

fn format_outline_budget(max_bytes: usize) -> String {
    if max_bytes >= 1024 && max_bytes % 1024 == 0 {
        format!("{}KB", max_bytes / 1024)
    } else {
        format!("{max_bytes} bytes")
    }
}

fn outline_many_files(
    files: &[String],
    ctx: &AppContext,
    req_id: &str,
    project_root: Option<&Path>,
) -> Result<(Vec<FileOutline>, Vec<SkippedFile>), Response> {
    let mut file_outlines: Vec<FileOutline> = Vec::with_capacity(files.len());
    let mut skipped_files: Vec<SkippedFile> = Vec::new();

    for file in files {
        let path = match ctx.validate_path(req_id, Path::new(file)) {
            Ok(path) => path,
            Err(resp) => return Err(resp),
        };
        if !path.exists() {
            skipped_files.push(SkippedFile::new(file, "file_not_found"));
            continue;
        }

        let rel_path = display_path(&path, file, project_root);
        if let Some(reason) = outline_skip_reason(&path) {
            skipped_files.push(SkippedFile::new(rel_path, reason));
            continue;
        }

        match ctx.provider().list_symbols(&path) {
            Ok(symbols) => {
                let entries = build_outline_tree(&symbols);
                file_outlines.push(FileOutline {
                    path: rel_path,
                    entries,
                });
            }
            Err(e) => skipped_files.push(SkippedFile::new(rel_path, outline_error_reason(&e))),
        }
    }

    Ok((file_outlines, skipped_files))
}

fn discover_outline_files(directory: &Path) -> OutlineFileDiscovery {
    let mut discovery = discover_outline_files_with_options(directory, None);
    if discovery.files.len() > OUTLINE_FILE_WALK_CAP {
        discovery.files.truncate(OUTLINE_FILE_WALK_CAP);
        discovery.walk_truncated = true;
    }
    discovery
}

fn discover_outline_files_with_options(
    directory: &Path,
    options: Option<&OutlineWalkOptions>,
) -> OutlineFileDiscovery {
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut walk_truncated = false;
    let mut collection_truncated = false;
    let mut skipped_foreign_mounts = 0usize;
    // A vanished mounted child can make std::fs::ReadDir::drop panic after
    // closedir returns ENXIO, aborting the daemon. Fence recursion before opening
    // such a child instead of trying to catch the uncatchable destructor panic.
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(directory);
    if let Ok(boundary) = boundary {
        let mut device_lookup = crate::walk_boundary::filesystem_device_id;
        collect_outline_files_with_device_lookup(
            directory,
            &mut files,
            &mut directories,
            &mut walk_truncated,
            &mut collection_truncated,
            &mut skipped_foreign_mounts,
            options,
            &boundary,
            &mut device_lookup,
        );
    } else {
        collection_truncated = true;
    }
    files.sort();
    directories.sort();

    OutlineFileDiscovery {
        files,
        directories,
        walk_truncated,
        collection_truncated,
        skipped_foreign_mounts,
    }
}

fn collect_outline_files_with_device_lookup<F>(
    directory: &Path,
    files: &mut Vec<String>,
    directories: &mut Vec<String>,
    walk_truncated: &mut bool,
    collection_truncated: &mut bool,
    skipped_foreign_mounts: &mut usize,
    options: Option<&OutlineWalkOptions>,
    boundary: &crate::walk_boundary::DeviceBoundary,
    device_lookup: &mut F,
) where
    F: FnMut(&Path) -> std::io::Result<Option<u64>>,
{
    if files.len() >= OUTLINE_FILE_COLLECTION_CAP {
        *walk_truncated = true;
        return;
    }

    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        if files.len() >= OUTLINE_FILE_COLLECTION_CAP {
            *walk_truncated = true;
            return;
        }
        // Query the directory entry once. Calling Path::is_dir, Path::is_file,
        // and symlink_metadata separately issues up to three metadata lookups per
        // file, which dominates `files: true` outlines on large repositories.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }

        let path = entry.path();
        if file_type.is_dir() {
            if should_skip_directory(&path) || is_ignored_outline_path(&path, true, options) {
                continue;
            }
            match boundary.should_descend_with(&path, |child| device_lookup(child)) {
                Ok(true) => {}
                Ok(false) => {
                    *skipped_foreign_mounts += 1;
                    continue;
                }
                Err(_) => {
                    *collection_truncated = true;
                    return;
                }
            }
            directories.push(path.to_string_lossy().to_string());
            collect_outline_files_with_device_lookup(
                &path,
                files,
                directories,
                walk_truncated,
                collection_truncated,
                skipped_foreign_mounts,
                options,
                boundary,
                device_lookup,
            );
            if *walk_truncated || *collection_truncated {
                return;
            }
        } else if file_type.is_file() {
            if is_ignored_outline_path(&path, false, options) {
                continue;
            }
            files.push(path.to_string_lossy().to_string());
            if files.len() >= OUTLINE_FILE_COLLECTION_CAP {
                *walk_truncated = true;
                return;
            }
        }
    }
}

fn is_ignored_outline_path(
    path: &Path,
    is_dir: bool,
    options: Option<&OutlineWalkOptions>,
) -> bool {
    let Some(options) = options else {
        return false;
    };
    let Some(gitignore) = options.gitignore.as_ref() else {
        return false;
    };

    let candidate = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(root) = options.gitignore_root.as_ref() {
        if !candidate.starts_with(root) {
            return false;
        }
    }

    gitignore
        .matched_path_or_any_parents(candidate, is_dir)
        .is_ignore()
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
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| fallback.to_string())
}

fn outline_skip_reason(path: &Path) -> Option<&'static str> {
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
    // `validate_syntax()` here. The cost is one extra parse per file, but
    // Track 0's parser cache (per-language reused `Parser`, global compiled
    // `Query`) makes that parse cheap relative to the full symbol-extraction
    // pass that follows.
    match edit::validate_syntax(path) {
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
                if with_sig {
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

/// Render only top-level entries. Directory/multi-file outlines are a structure
/// map; callers can request a specific file when they need methods or fields.
fn render_top_level_entries(
    entries: &[OutlineEntry],
    indent: usize,
    output: &mut String,
    with_sig: bool,
) {
    let prefix = "  ".repeat(indent);
    for entry in entries {
        if with_sig {
            output.push_str(&format!("{}{}\n", prefix, format_entry_with_sig(entry)));
        } else {
            output.push_str(&format!("{}{}\n", prefix, format_entry_compact(entry)));
        }
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
fn format_multi_file_tree(
    file_outlines: &[FileOutline],
    max_bytes: usize,
    total_requested: usize,
) -> String {
    // Build a tree of directories → files → symbols
    // Using a simple sorted-path approach with indentation
    let mut output = String::new();
    let mut truncated = false;
    let mut files_shown = 0;

    // Sort by path for clean directory grouping
    let mut sorted: Vec<&FileOutline> = file_outlines.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    // Track directory nesting via path components
    let mut prev_parts: Vec<&str> = Vec::new();

    for fo in &sorted {
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
            output.push_str(&format!("{}{}/\n", indent, part));
        }

        // Emit file name
        let file_indent = "  ".repeat(dir_parts.len());
        output.push_str(&format!("{}{}\n", file_indent, file_name));

        // Emit only top-level symbols under each file. Full nested members stay
        // available via the single-file outline path.
        render_top_level_entries(&fo.entries, dir_parts.len() + 1, &mut output, false);

        files_shown += 1;
        prev_parts = parts.iter().map(|s| *s).collect();

        // Check size cap
        if output.len() > max_bytes {
            truncated = true;
            break;
        }
    }

    if truncated {
        output.push_str(&format!(
            "\n... truncated ({}/{} files shown, {}KB limit)\n\
             Narrow scope with a more specific directory path, or pass a single file as target.\n",
            files_shown,
            total_requested,
            max_bytes / 1024,
        ));
    }

    output
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
        let mut lookup = |path: &Path| {
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
            None,
            &boundary,
            &mut lookup,
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
            symbols,
            lines,
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
        assert!(text.contains("3 json files"));
        assert!(text.contains("6 lines"));
        assert!(text.contains("1 directory shown as a rollup (budget: 30KB)"));
        assert!(!text.contains(".json  "));
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
