use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::context::AppContext;
use crate::edit;
use crate::error::AftError;
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
/// - Single-file mode: includes signatures (e.g. `E fn  greet(name: string): void 5:12`)
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
        let (file_outlines, skipped_files) =
            match outline_many_files(&discovery.files, ctx, &req.id, project_root.as_deref()) {
                Ok(result) => result,
                Err(resp) => return resp,
            };

        let text = format_multi_file_tree(&file_outlines, MAX_OUTPUT_BYTES, discovery.files.len());
        return Response::success(
            &req.id,
            serde_json::json!({
                "text": text,
                "complete": !discovery.walk_truncated && !discovery.collection_truncated,
                "walk_truncated": discovery.walk_truncated,
                "collection_truncated": discovery.collection_truncated,
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
        return Response::success(
            &req.id,
            serde_json::json!({ "text": text, "complete": true, "skipped_files": skipped_files }),
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
/// 1. Convert every symbol to an `OutlineEntry` and index by name.
/// 2. Walk children (parent.is_some()) and attach them under their parent.
///    For multi-level nesting (e.g. OuterClass.InnerClass.inner_method),
///    we use the `scope_chain` to walk the full parent path.
///
/// Symbols whose parent can't be found in the list are promoted to top level
/// (defensive — shouldn't happen with well-formed parser output).
fn build_outline_tree(symbols: &[Symbol]) -> Vec<OutlineEntry> {
    // Separate top-level and child symbols
    let mut top_level: Vec<OutlineEntry> = Vec::new();
    let mut children: Vec<&Symbol> = Vec::new();

    for sym in symbols {
        if sym.parent.is_none() {
            top_level.push(symbol_to_entry(sym));
        } else {
            children.push(sym);
        }
    }

    // Build a name→index map for top-level entries
    // For multi-level nesting, we need to find entries recursively
    for child in &children {
        let entry = symbol_to_entry(child);
        let scope = &child.scope_chain;

        if scope.is_empty() {
            // Shouldn't happen if parent.is_some(), but be defensive
            top_level.push(entry);
            continue;
        }

        // Walk the scope chain to find the correct parent container
        if !insert_at_scope(&mut top_level, scope, entry.clone()) {
            // Parent not found — promote to top level
            top_level.push(entry);
        }
    }

    top_level
}

/// Recursively walk scope_chain to insert an entry under the correct parent.
///
/// scope_chain = ["OuterClass", "InnerClass"] means:
///   find "OuterClass" at this level → find "InnerClass" in its members → insert there
fn insert_at_scope(
    entries: &mut Vec<OutlineEntry>,
    scope_chain: &[String],
    entry: OutlineEntry,
) -> bool {
    if scope_chain.is_empty() {
        return false;
    }

    let target_name = &scope_chain[0];
    for existing in entries.iter_mut() {
        if existing.name == *target_name {
            if scope_chain.len() == 1 {
                // This is the direct parent — insert here
                existing.members.push(entry);
                return true;
            } else {
                // Recurse deeper
                return insert_at_scope(&mut existing.members, &scope_chain[1..], entry);
            }
        }
    }

    false
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
    bytes: u64,
}

#[derive(Debug, Clone)]
struct OutlineWalkOptions {
    gitignore: Option<Arc<ignore::gitignore::Gitignore>>,
    gitignore_root: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct OutlineFileDiscovery {
    files: Vec<String>,
    walk_truncated: bool,
    collection_truncated: bool,
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

    let mut file_entries = Vec::new();
    let mut walk_truncated = false;
    let mut collection_truncated = false;

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

        for file in discovery.files {
            let file_path = PathBuf::from(file);
            if let Some(entry) = outline_file_entry(&file_path, display_root, ctx) {
                file_entries.push(entry);
            }
        }
    }

    file_entries.sort_by(|a, b| a.path.cmp(&b.path));
    let (text, mut unchecked_files, text_truncated) =
        format_files_table(&file_entries, max_output_bytes);
    if walk_truncated && unchecked_files.is_empty() {
        unchecked_files
            .push("<additional files not discovered: directory walk limit reached>".to_string());
    }
    if collection_truncated && unchecked_files.is_empty() {
        unchecked_files
            .push("<additional files not discovered: collection safety limit reached>".to_string());
    }

    Response::success(
        &req.id,
        serde_json::json!({
            "text": text,
            "files": file_entries,
            "complete": !walk_truncated && !collection_truncated && !text_truncated,
            "walk_truncated": walk_truncated,
            "collection_truncated": collection_truncated,
            "unchecked_files": unchecked_files,
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

fn outline_file_entry(
    path: &Path,
    display_root: &Path,
    ctx: &AppContext,
) -> Option<OutlineFileEntry> {
    let metadata = std::fs::metadata(path).ok()?;
    let rel_path =
        relative_path_from_root(path, display_root).unwrap_or_else(|| path_to_slash(path));
    let bytes = metadata.len();
    let detected_language = detect_language(path).map(language_id);

    if let Some(symbols) = cached_symbol_count(ctx, path, &metadata) {
        return Some(OutlineFileEntry {
            path: rel_path,
            language: detected_language.unwrap_or("unknown").to_string(),
            symbols,
            bytes,
        });
    }

    let Some(language) = detected_language else {
        return Some(OutlineFileEntry {
            path: rel_path,
            language: if file_looks_binary(path) {
                "binary"
            } else {
                "unknown"
            }
            .to_string(),
            symbols: 0,
            bytes,
        });
    };

    if file_looks_binary(path) {
        return Some(OutlineFileEntry {
            path: rel_path,
            language: "binary".to_string(),
            symbols: 0,
            bytes,
        });
    }

    if bytes > MAX_OUTLINE_FILE_BYTES {
        return Some(OutlineFileEntry {
            path: rel_path,
            language: language.to_string(),
            symbols: 0,
            bytes,
        });
    }

    let symbols = ctx
        .provider()
        .list_symbols(path)
        .map(|symbols| symbols.len())
        .unwrap_or(0);

    Some(OutlineFileEntry {
        path: rel_path,
        language: language.to_string(),
        symbols,
        bytes,
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

fn file_looks_binary(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut sample = [0u8; BINARY_SAMPLE_BYTES];
    let Ok(bytes_read) = file.read(&mut sample) else {
        return false;
    };
    bytes_read > 0 && content_inspector::inspect(&sample[..bytes_read]).is_binary()
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
    }
}

fn format_files_table(
    entries: &[OutlineFileEntry],
    max_bytes: usize,
) -> (String, Vec<String>, bool) {
    let path_width = entries
        .iter()
        .map(|entry| entry.path.len())
        .max()
        .unwrap_or(0);
    let language_width = entries
        .iter()
        .map(|entry| entry.language.len())
        .max()
        .unwrap_or("language".len())
        .max(8);

    let mut output = String::new();
    let mut shown = 0usize;
    let mut truncated = false;

    for entry in entries {
        let line = format!(
            "{:<path_width$}  {:<language_width$} {:>5} syms {:>9} bytes\n",
            entry.path,
            entry.language,
            entry.symbols,
            entry.bytes,
            path_width = path_width,
            language_width = language_width,
        );
        if output.len() + line.len() > max_bytes {
            truncated = true;
            break;
        }
        output.push_str(&line);
        shown += 1;
    }

    let unchecked_files = if truncated {
        entries[shown..]
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    if truncated {
        output.push_str(&format!(
            "\n... truncated ({}/{} files shown, {}KB limit)\n\
             Narrow scope with a more specific directory path.\n",
            shown,
            entries.len(),
            max_bytes / 1024,
        ));
    }

    (output, unchecked_files, truncated)
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
    discover_outline_files_with_options(directory, None)
}

fn discover_outline_files_with_options(
    directory: &Path,
    options: Option<&OutlineWalkOptions>,
) -> OutlineFileDiscovery {
    let mut files = Vec::new();
    let mut collection_truncated = false;
    collect_outline_files(directory, &mut files, &mut collection_truncated, options);
    files.sort();

    let walk_truncated = files.len() > OUTLINE_FILE_WALK_CAP;
    if walk_truncated {
        files.truncate(OUTLINE_FILE_WALK_CAP);
    }

    OutlineFileDiscovery {
        files,
        walk_truncated,
        collection_truncated,
    }
}

fn collect_outline_files(
    directory: &Path,
    files: &mut Vec<String>,
    collection_truncated: &mut bool,
    options: Option<&OutlineWalkOptions>,
) {
    if files.len() >= OUTLINE_FILE_COLLECTION_CAP {
        *collection_truncated = true;
        return;
    }

    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        if files.len() >= OUTLINE_FILE_COLLECTION_CAP {
            *collection_truncated = true;
            return;
        }
        let path = entry.path();
        if is_symlink(&path) {
            continue;
        }
        if path.is_dir() {
            if should_skip_directory(&path) || is_ignored_outline_path(&path, true, options) {
                continue;
            }
            collect_outline_files(&path, files, collection_truncated, options);
            if *collection_truncated {
                return;
            }
        } else if path.is_file() {
            if is_ignored_outline_path(&path, false, options) {
                continue;
            }
            files.push(path.to_string_lossy().to_string());
        }
    }
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
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

/// Format a single entry line for single-file mode (with signature).
fn format_entry_with_sig(entry: &OutlineEntry) -> String {
    let vis = if entry.exported { 'E' } else { '-' };
    let kind = kind_abbrev(&entry.kind);
    let sl = entry.range.start_line + 1;
    let el = entry.range.end_line + 1;
    if let Some(ref sig) = entry.signature {
        format!("{} {:<4} {} {}:{}", vis, kind, sig, sl, el)
    } else {
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

        // Emit symbols under file
        render_entries(&fo.entries, dir_parts.len() + 1, &mut output, false);

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
             Narrow scope with a more specific directory path or use filePath for single files.\n",
            files_shown,
            total_requested,
            max_bytes / 1024,
        ));
    }

    output
}

fn symbol_to_entry(sym: &Symbol) -> OutlineEntry {
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
}
