use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;
use serde_json::{json, Value};
use tree_sitter::{Node, Tree};

use crate::cache_freshness;
use crate::imports::{parse_file_imports, specifier_imported_name, ImportBlock, ImportStatement};
use crate::inspect::{
    FileContribution, InspectCategory, InspectJob, InspectResult, InspectScanSuccess,
};
use crate::parser::{detect_language, LangId};

const JS_MODULE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];
const DRILL_DOWN_LIMIT: usize = 100;

#[derive(Debug, Clone)]
struct ExportSymbol {
    symbol: String,
    kind: String,
    line: u32,
}

#[derive(Debug, Clone)]
struct ImportEdge {
    from_module: String,
    resolved_file: Option<PathBuf>,
    named: Vec<String>,
}

#[derive(Debug)]
struct FileScan {
    file_path: PathBuf,
    relative_file: String,
    contribution: FileContribution,
    exports: Vec<ExportSymbol>,
    imports: Vec<ImportEdge>,
    skipped_language: Option<&'static str>,
}

pub fn run_unused_exports_scan(job: &InspectJob) -> InspectResult {
    let started = Instant::now();
    let ctx = job.worker_ctx();
    let project_root = normalize_path(&ctx.project_root);
    let public_api_entries = crate::inspect::entry_points::resolve_entry_points(&project_root);
    let package_warnings = public_api_entries.warnings().to_vec();

    let per_file = job
        .scope_files
        .par_iter()
        .filter_map(|path| scan_file(path, &project_root))
        .map(|scan| suppress_public_api_exports(scan, &project_root, &public_api_entries))
        .collect::<Vec<_>>();

    let mut imported_by: BTreeMap<(PathBuf, String), BTreeSet<String>> = BTreeMap::new();
    let mut uncertain_by: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    for scan in &per_file {
        for import in &scan.imports {
            let Some(resolved_file) = &import.resolved_file else {
                continue;
            };
            for name in &import.named {
                if name == "*" {
                    uncertain_by
                        .entry(resolved_file.clone())
                        .or_default()
                        .insert(scan.relative_file.clone());
                } else {
                    imported_by
                        .entry((resolved_file.clone(), name.clone()))
                        .or_default()
                        .insert(scan.relative_file.clone());
                }
            }
        }
    }

    let mut count = 0usize;
    let mut items = Vec::new();
    let mut uncertain_count = 0usize;
    let mut uncertain_items = Vec::new();
    for scan in &per_file {
        if public_api_entries.is_public_api_file(&scan.file_path) {
            continue;
        }

        for export in &scan.exports {
            let imported = imported_by
                .get(&(scan.file_path.clone(), export.symbol.clone()))
                .map(|files| !files.is_empty())
                .unwrap_or(false);
            let uncertain = uncertain_by
                .get(&scan.file_path)
                .map(|files| !files.is_empty())
                .unwrap_or(false);

            if imported {
                continue;
            }
            if uncertain {
                uncertain_count += 1;
                if uncertain_items.len() < DRILL_DOWN_LIMIT {
                    uncertain_items.push(json!({
                        "file": scan.relative_file,
                        "symbol": export.symbol,
                        "kind": export.kind,
                        "line": export.line,
                        "reason": "wildcard_import",
                    }));
                }
                continue;
            }

            count += 1;
            if items.len() < DRILL_DOWN_LIMIT {
                items.push(json!({
                    "file": scan.relative_file,
                    "symbol": export.symbol,
                    "kind": export.kind,
                    "line": export.line,
                }));
            }
        }
    }

    let languages_skipped = per_file
        .iter()
        .filter_map(|scan| scan.skipped_language)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let mut aggregate = json!({
        "count": count,
        "items": items,
        "drill_down_capped": count > DRILL_DOWN_LIMIT,
        "scanned_files": per_file.len(),
        "languages_skipped": languages_skipped,
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
    });
    if !package_warnings.is_empty() {
        aggregate["note"] = Value::String(package_warnings.join("; "));
    }

    let success = InspectScanSuccess {
        scanned_files: per_file.iter().map(|scan| scan.file_path.clone()).collect(),
        contributions: per_file.into_iter().map(|scan| scan.contribution).collect(),
        aggregate,
    };
    InspectResult::success(job, success, started.elapsed())
}

fn suppress_public_api_exports(
    mut scan: FileScan,
    project_root: &Path,
    public_api_entries: &crate::inspect::entry_points::EntryPointSet,
) -> FileScan {
    if public_api_entries.is_public_api_file(&scan.file_path) && !scan.exports.is_empty() {
        scan.exports.clear();
        scan.contribution.contribution = contribution_value(
            project_root,
            &scan.relative_file,
            &scan.exports,
            &scan.imports,
        );
    }
    scan
}

fn scan_file(path: &Path, project_root: &Path) -> Option<FileScan> {
    let file_path = absolute_path(project_root, path);
    if !file_path.is_file() {
        return None;
    }

    let relative_file = relative_string(project_root, &file_path);
    let freshness = cache_freshness::collect(&file_path).ok()?;
    let Some(lang) = detect_language(&file_path) else {
        return Some(empty_file_scan(file_path, relative_file, freshness, None));
    };

    if !is_js_ts(lang) {
        return Some(empty_file_scan(
            file_path,
            relative_file,
            freshness,
            Some(language_name(lang)),
        ));
    }

    let Ok((source, tree, import_block)) = parse_file_imports(&file_path, lang) else {
        return Some(empty_file_scan(file_path, relative_file, freshness, None));
    };

    let exports = extract_exports(&source, &tree);
    let namespace_members = namespace_member_accesses(&source, &tree, &import_block);
    let mut imports =
        import_edges_from_block(&import_block, &file_path, project_root, &namespace_members);
    imports.extend(reexport_edges(&source, &tree, &file_path, project_root));

    let contribution = contribution_value(project_root, &relative_file, &exports, &imports);
    Some(FileScan {
        contribution: FileContribution::new(
            InspectCategory::UnusedExports,
            file_path.clone(),
            freshness,
            contribution,
        ),
        file_path,
        relative_file,
        exports,
        imports,
        skipped_language: None,
    })
}

fn empty_file_scan(
    file_path: PathBuf,
    relative_file: String,
    freshness: cache_freshness::FileFreshness,
    skipped_language: Option<&'static str>,
) -> FileScan {
    let contribution = json!({
        "file": relative_file,
        "exports": [],
        "imports": [],
    });
    FileScan {
        contribution: FileContribution::new(
            InspectCategory::UnusedExports,
            file_path.clone(),
            freshness,
            contribution,
        ),
        file_path,
        relative_file,
        exports: Vec::new(),
        imports: Vec::new(),
        skipped_language,
    }
}

fn contribution_value(
    project_root: &Path,
    relative_file: &str,
    exports: &[ExportSymbol],
    imports: &[ImportEdge],
) -> Value {
    let exports_json = exports
        .iter()
        .map(|export| {
            json!({
                "symbol": export.symbol,
                "kind": export.kind,
                "line": export.line,
            })
        })
        .collect::<Vec<_>>();
    let imports_json = imports
        .iter()
        .map(|import| {
            json!({
                "from_module": import.from_module,
                "resolved_file": import
                    .resolved_file
                    .as_ref()
                    .map(|path| relative_string(project_root, path)),
                "named": import.named,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "file": relative_file,
        "exports": exports_json,
        "imports": imports_json,
    })
}

fn import_edges_from_block(
    import_block: &ImportBlock,
    importer_file: &Path,
    project_root: &Path,
    namespace_members: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<ImportEdge> {
    import_block
        .imports
        .iter()
        .map(|import| {
            import_edge_from_statement(import, importer_file, project_root, namespace_members)
        })
        .collect()
}

fn import_edge_from_statement(
    import: &ImportStatement,
    importer_file: &Path,
    project_root: &Path,
    namespace_members: &BTreeMap<String, BTreeSet<String>>,
) -> ImportEdge {
    let mut named = Vec::new();
    if import.default_import.is_some() {
        named.push("default".to_string());
    }
    if let Some(alias) = import.namespace_import.as_deref() {
        if let Some(members) = namespace_members.get(alias) {
            named.extend(members.iter().cloned());
        }
    }
    named.extend(
        import
            .names
            .iter()
            .map(|name| specifier_imported_name(name).to_string()),
    );
    named.sort();
    named.dedup();

    ImportEdge {
        from_module: import.module_path.clone(),
        resolved_file: resolve_module_path(&import.module_path, importer_file, project_root),
        named,
    }
}

fn namespace_member_accesses(
    source: &str,
    tree: &Tree,
    import_block: &ImportBlock,
) -> BTreeMap<String, BTreeSet<String>> {
    let aliases = import_block
        .imports
        .iter()
        .filter_map(|import| import.namespace_import.clone())
        .collect::<BTreeSet<_>>();
    if aliases.is_empty() {
        return BTreeMap::new();
    }

    let mut accesses = BTreeMap::new();
    collect_namespace_member_accesses(source, tree.root_node(), &aliases, &mut accesses);
    accesses
}

fn collect_namespace_member_accesses(
    source: &str,
    node: Node,
    aliases: &BTreeSet<String>,
    accesses: &mut BTreeMap<String, BTreeSet<String>>,
) {
    match node.kind() {
        "member_expression" => {
            if let Some(object) = node.child_by_field_name("object") {
                let alias = node_text(source, &object).trim();
                if aliases.contains(alias) {
                    let member = node
                        .child_by_field_name("property")
                        .and_then(|property| static_member_name(source, &property))
                        .unwrap_or_else(|| "*".to_string());
                    accesses
                        .entry(alias.to_string())
                        .or_default()
                        .insert(member);
                }
            }
        }
        "subscript_expression" => {
            if let Some(object) = node.child_by_field_name("object") {
                let alias = node_text(source, &object).trim();
                if aliases.contains(alias) {
                    let member = node
                        .child_by_field_name("index")
                        .and_then(|index| static_member_name(source, &index))
                        .unwrap_or_else(|| "*".to_string());
                    accesses
                        .entry(alias.to_string())
                        .or_default()
                        .insert(member);
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_namespace_member_accesses(source, cursor.node(), aliases, accesses);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn static_member_name(source: &str, node: &Node) -> Option<String> {
    let text = node_text(source, node).trim();
    if text.is_empty() {
        return None;
    }

    match node.kind() {
        "identifier" | "property_identifier" | "shorthand_property_identifier" => {
            Some(text.to_string())
        }
        "string" | "string_fragment" => {
            let unquoted = strip_quotes(text);
            (!unquoted.is_empty()).then(|| unquoted.to_string())
        }
        _ => None,
    }
}

fn extract_exports(source: &str, tree: &Tree) -> Vec<ExportSymbol> {
    let mut exports = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return exports;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "export_statement" {
            extract_export_statement(source, &node, &mut exports);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    dedupe_exports(exports)
}

fn extract_export_statement(source: &str, node: &Node, exports: &mut Vec<ExportSymbol>) {
    let text = node_text(source, node).trim_start();
    if is_default_export(text) {
        exports.push(ExportSymbol {
            symbol: "default".to_string(),
            kind: default_export_kind(node).to_string(),
            line: line_number(node),
        });
        return;
    }

    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let child = cursor.node();
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => {
                push_named_declaration(source, &child, "function", exports);
            }
            "class_declaration" | "abstract_class_declaration" => {
                push_named_declaration(source, &child, "class", exports);
            }
            "interface_declaration" => {
                push_named_declaration(source, &child, "interface", exports);
            }
            "type_alias_declaration" => {
                push_named_declaration(source, &child, "type", exports);
            }
            "enum_declaration" => {
                push_named_declaration(source, &child, "enum", exports);
            }
            "internal_module" => {
                push_named_declaration(source, &child, "namespace", exports);
            }
            "lexical_declaration" | "variable_declaration" => {
                collect_variable_exports(source, &child, exports);
            }
            "export_clause" => {
                let has_source = export_source_module(source, node).is_some();
                collect_export_clause_symbols(source, &child, has_source, exports);
            }
            _ => {}
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn push_named_declaration(
    source: &str,
    node: &Node,
    kind: &'static str,
    exports: &mut Vec<ExportSymbol>,
) {
    if let Some(name) = declaration_name(source, node) {
        exports.push(ExportSymbol {
            symbol: name,
            kind: kind.to_string(),
            line: line_number(node),
        });
    }
}

fn declaration_name(source: &str, node: &Node) -> Option<String> {
    node.child_by_field_name("name")
        .map(|name| node_text(source, &name).to_string())
}

fn collect_variable_exports(source: &str, node: &Node, exports: &mut Vec<ExportSymbol>) {
    if node.kind() == "variable_declarator" {
        if let Some(name) = node.child_by_field_name("name") {
            if matches!(
                name.kind(),
                "identifier" | "shorthand_property_identifier_pattern"
            ) {
                exports.push(ExportSymbol {
                    symbol: node_text(source, &name).to_string(),
                    kind: "variable".to_string(),
                    line: line_number(node),
                });
            }
        }
        return;
    }

    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        collect_variable_exports(source, &cursor.node(), exports);
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn collect_export_clause_symbols(
    source: &str,
    node: &Node,
    has_source: bool,
    exports: &mut Vec<ExportSymbol>,
) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let child = cursor.node();
        if child.kind() == "export_specifier" {
            if let Some(symbol) = exported_specifier_name(source, &child) {
                exports.push(ExportSymbol {
                    symbol,
                    kind: if has_source { "re_export" } else { "export" }.to_string(),
                    line: line_number(&child),
                });
            }
        } else {
            collect_export_clause_symbols(source, &child, has_source, exports);
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn reexport_edges(
    source: &str,
    tree: &Tree,
    importer_file: &Path,
    project_root: &Path,
) -> Vec<ImportEdge> {
    let mut edges = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return edges;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "export_statement" {
            if let Some(from_module) = export_source_module(source, &node) {
                let mut named = reexport_imported_names(source, &node);
                if named.is_empty() && node_text(source, &node).contains('*') {
                    named.push("*".to_string());
                }
                named.sort();
                named.dedup();
                edges.push(ImportEdge {
                    resolved_file: resolve_module_path(&from_module, importer_file, project_root),
                    from_module,
                    named,
                });
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    edges
}

fn reexport_imported_names(source: &str, node: &Node) -> Vec<String> {
    let mut names = Vec::new();
    collect_reexport_imported_names(source, node, &mut names);
    names
}

fn collect_reexport_imported_names(source: &str, node: &Node, names: &mut Vec<String>) {
    if node.kind() == "export_specifier" {
        if let Some(name) = imported_specifier_name(source, node) {
            names.push(name);
        }
        return;
    }

    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        collect_reexport_imported_names(source, &cursor.node(), names);
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn export_source_module(source: &str, node: &Node) -> Option<String> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        let child = cursor.node();
        if child.kind() == "string" {
            return Some(strip_quotes(node_text(source, &child)).to_string());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    None
}

fn exported_specifier_name(source: &str, node: &Node) -> Option<String> {
    specifier_name_after_as(node_text(source, node)).or_else(|| first_specifier_name(source, node))
}

fn imported_specifier_name(source: &str, node: &Node) -> Option<String> {
    first_specifier_name(source, node)
}

fn specifier_name_after_as(text: &str) -> Option<String> {
    let cleaned = clean_specifier_text(text);
    cleaned
        .split_once(" as ")
        .map(|(_, alias)| alias.trim().to_string())
        .filter(|alias| !alias.is_empty())
}

fn first_specifier_name(source: &str, node: &Node) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        return Some(clean_specifier_text(node_text(source, &name)));
    }

    clean_specifier_text(node_text(source, node))
        .split_whitespace()
        .next()
        .map(str::to_string)
}

fn clean_specifier_text(text: &str) -> String {
    text.trim()
        .trim_start_matches("type ")
        .trim()
        .trim_matches('{')
        .trim_matches('}')
        .trim()
        .to_string()
}

fn is_default_export(text: &str) -> bool {
    text.strip_prefix("export")
        .map(str::trim_start)
        .map(|after_export| after_export.starts_with("default"))
        .unwrap_or(false)
}

fn default_export_kind(node: &Node) -> &'static str {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return "default";
    }
    loop {
        let child = cursor.node();
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => return "function",
            "class_declaration" | "abstract_class_declaration" => return "class",
            _ => {}
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    "default"
}

fn dedupe_exports(exports: Vec<ExportSymbol>) -> Vec<ExportSymbol> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for export in exports {
        if seen.insert((export.symbol.clone(), export.line)) {
            deduped.push(export);
        }
    }
    deduped
}

fn resolve_module_path(
    module_path: &str,
    importer_file: &Path,
    project_root: &Path,
) -> Option<PathBuf> {
    if module_path.starts_with("node:") {
        return None;
    }

    if is_relative_module(module_path) {
        let base = importer_file.parent()?.join(module_path);
        return resolve_local_module(&base, project_root);
    }

    resolve_node_package(module_path, project_root)
}

fn resolve_local_module(base: &Path, project_root: &Path) -> Option<PathBuf> {
    candidate_paths(base)
        .into_iter()
        .map(|candidate| normalize_path(&candidate))
        .find(|candidate| candidate.starts_with(project_root) && candidate.is_file())
}

fn resolve_node_package(module_path: &str, project_root: &Path) -> Option<PathBuf> {
    let package_name = package_name(module_path)?;
    let package_dir = project_root.join("node_modules").join(package_name);
    let package_json = package_dir.join("package.json");
    let value = fs::read_to_string(&package_json)
        .ok()
        .and_then(|source| serde_json::from_str::<Value>(&source).ok())?;

    let mut entries = Vec::new();
    if let Some(main) = value.get("main").and_then(Value::as_str) {
        entries.push(main.to_string());
    }
    if let Some(exports) = value.get("exports") {
        collect_package_export_strings(exports, &mut entries);
    }

    entries
        .iter()
        .filter_map(|entry| resolve_package_entry(&package_dir, entry))
        .next()
}

fn package_name(module_path: &str) -> Option<String> {
    let mut parts = module_path.split('/');
    let first = parts.next()?.to_string();
    if first.starts_with('@') {
        let second = parts.next()?;
        Some(format!("{first}/{second}"))
    } else {
        Some(first)
    }
}

fn collect_package_export_strings(value: &Value, entries: &mut Vec<String>) {
    match value {
        Value::String(entry) => entries.push(entry.clone()),
        Value::Array(values) => {
            for value in values {
                collect_package_export_strings(value, entries);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_package_export_strings(value, entries);
            }
        }
        _ => {}
    }
}

fn resolve_package_entry(package_dir: &Path, entry: &str) -> Option<PathBuf> {
    if entry.starts_with("node:") || entry.contains("://") {
        return None;
    }

    let entry_path = if is_relative_module(entry) {
        package_dir.join(entry)
    } else {
        package_dir.join(entry.trim_start_matches('/'))
    };
    candidate_paths(&entry_path)
        .into_iter()
        .map(|candidate| normalize_path(&candidate))
        .find(|candidate| candidate.is_file())
}

fn candidate_paths(base: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    candidates.push(base.to_path_buf());

    if base.extension().is_none() {
        for extension in JS_MODULE_EXTENSIONS {
            candidates.push(base.with_extension(extension));
        }
    }

    for extension in JS_MODULE_EXTENSIONS {
        candidates.push(base.join(format!("index.{extension}")));
    }

    candidates
}

fn is_relative_module(module_path: &str) -> bool {
    module_path.starts_with("./")
        || module_path.starts_with("../")
        || module_path == "."
        || module_path == ".."
}

fn is_js_ts(lang: LangId) -> bool {
    matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript)
}

fn language_name(lang: LangId) -> &'static str {
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

fn absolute_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(&project_root.join(path))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !result.pop() {
                    result.push(component.as_os_str());
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn relative_string(project_root: &Path, path: &Path) -> String {
    normalize_path(path)
        .strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn strip_quotes(text: &str) -> &str {
    text.trim()
        .trim_start_matches(['\'', '"'])
        .trim_end_matches(['\'', '"'])
}

fn node_text<'a>(source: &'a str, node: &Node) -> &'a str {
    &source[node.byte_range()]
}

fn line_number(node: &Node) -> u32 {
    (node.start_position().row + 1) as u32
}
