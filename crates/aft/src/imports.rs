//! Import analysis engine: parsing, grouping, deduplication, and insertion.
//!
//! Provides per-language import handling dispatched by `LangId`. Each language
//! implementation extracts imports from tree-sitter ASTs, classifies them into
//! groups, and generates import text.
//!
//! Currently supports: TypeScript, TSX, JavaScript.

use std::ops::Range;

use tree_sitter::{Node, Parser, Tree};

use crate::parser::{grammar_for, LangId};

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// What kind of import this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    /// `import { X } from 'y'` or `import X from 'y'`
    Value,
    /// `import type { X } from 'y'`
    Type,
    /// `import './side-effect'`
    SideEffect,
}

/// Which logical group an import belongs to (language-specific).
///
/// Ordering matches conventional import group sorting:
///   Stdlib (first) < External < Internal (last)
///
/// Language mapping:
///   - TS/JS/TSX: External (no `.` prefix), Internal (`.`/`..` prefix)
///   - Python:    Stdlib, External (third-party), Internal (relative `.`/`..`)
///   - Rust:      Stdlib (std/core/alloc), External (crates), Internal (crate/self/super)
///   - Go:        Stdlib (no dots in path), External (dots in path)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ImportGroup {
    /// Standard library (Python stdlib, Rust std/core/alloc, Go stdlib).
    /// TS/JS don't use this group.
    Stdlib,
    /// External/third-party packages.
    External,
    /// Internal/relative imports (TS relative, Python local, Rust crate/self/super).
    Internal,
}

impl ImportGroup {
    /// Human-readable label for the group.
    pub fn label(&self) -> &'static str {
        match self {
            ImportGroup::Stdlib => "stdlib",
            ImportGroup::External => "external",
            ImportGroup::Internal => "internal",
        }
    }
}

/// A single parsed import statement.
#[derive(Debug, Clone)]
pub struct ImportStatement {
    /// The module path (e.g., `react`, `./utils`, `../config`).
    pub module_path: String,
    /// Named imports (e.g., `["useState", "useEffect"]`).
    pub names: Vec<String>,
    /// Default import name (e.g., `React` from `import React from 'react'`).
    pub default_import: Option<String>,
    /// Namespace import name (e.g., `path` from `import * as path from 'path'`).
    pub namespace_import: Option<String>,
    /// What kind: value, type, or side-effect.
    pub kind: ImportKind,
    /// Which group this import belongs to.
    pub group: ImportGroup,
    /// Byte range in the original source.
    pub byte_range: Range<usize>,
    /// Raw text of the import statement.
    pub raw_text: String,
}

/// A block of parsed imports from a file.
#[derive(Debug, Clone)]
pub struct ImportBlock {
    /// All parsed import statements, in source order.
    pub imports: Vec<ImportStatement>,
    /// Overall byte range covering all import statements (start of first to end of last).
    /// `None` if no imports found.
    pub byte_range: Option<Range<usize>>,
}

impl ImportBlock {
    pub fn empty() -> Self {
        ImportBlock {
            imports: Vec::new(),
            byte_range: None,
        }
    }
}

fn import_byte_range(imports: &[ImportStatement]) -> Option<Range<usize>> {
    imports.first().zip(imports.last()).map(|(first, last)| {
        let start = first.byte_range.start;
        let end = last.byte_range.end;
        start..end
    })
}

// ---------------------------------------------------------------------------
// Specifier helpers (TS/JS verbatim-string format)
// ---------------------------------------------------------------------------

/// Return the local binding name for a TS/JS named-import specifier stored in
/// `ImportStatement::names`. Specifiers are stored verbatim — e.g.
/// `"stdin as input"`, `"type Foo"`, `"type Foo as Bar"`, `"useState"` — so
/// callers that want the name actually introduced into scope must strip the
/// optional `type ` prefix and prefer the post-`as` identifier when present.
///
/// Examples:
///   `"useState"`            → `"useState"`
///   `"stdin as input"`      → `"input"`
///   `"type Foo"`            → `"Foo"`
///   `"type Foo as Bar"`     → `"Bar"`
pub fn specifier_local_name(spec: &str) -> &str {
    let trimmed = spec.trim();
    let after_type = trimmed
        .strip_prefix("type ")
        .unwrap_or(trimmed)
        .trim_start();
    if let Some(idx) = after_type.find(" as ") {
        after_type[idx + 4..].trim()
    } else {
        after_type
    }
}

/// Return the imported (pre-`as`) name for a TS/JS named-import specifier.
/// Used by dedup, remove, and any caller that needs the source-side name.
///
/// Examples:
///   `"useState"`            → `"useState"`
///   `"stdin as input"`      → `"stdin"`
///   `"type Foo"`            → `"Foo"`
///   `"type Foo as Bar"`     → `"Foo"`
pub fn specifier_imported_name(spec: &str) -> &str {
    let trimmed = spec.trim();
    let after_type = trimmed
        .strip_prefix("type ")
        .unwrap_or(trimmed)
        .trim_start();
    after_type
        .find(" as ")
        .map(|idx| after_type[..idx].trim())
        .unwrap_or(after_type)
}

/// Whether a stored specifier matches a target name. Matches against either
/// the imported name or the local binding so callers can pass whichever name
/// they observed in source. Useful for `remove_import` where the agent may
/// reference an aliased import by either name.
pub fn specifier_matches(spec: &str, target: &str) -> bool {
    specifier_imported_name(spec) == target || specifier_local_name(spec) == target
}

// ---------------------------------------------------------------------------
// Core API
// ---------------------------------------------------------------------------

/// Parse imports from source using the provided tree-sitter tree.
pub fn parse_imports(source: &str, tree: &Tree, lang: LangId) -> ImportBlock {
    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => parse_ts_imports(source, tree),
        LangId::Python => parse_py_imports(source, tree),
        LangId::Rust => parse_rs_imports(source, tree),
        LangId::Go => parse_go_imports(source, tree),
        LangId::C
        | LangId::Cpp
        | LangId::Zig
        | LangId::CSharp
        | LangId::Bash
        | LangId::Solidity
        | LangId::Vue
        | LangId::Json
        | LangId::Scala
        | LangId::Java
        | LangId::Ruby
        | LangId::Kotlin
        | LangId::Swift
        | LangId::Php
        | LangId::Lua
        | LangId::Perl => ImportBlock::empty(),
        LangId::Html | LangId::Markdown => ImportBlock::empty(),
    }
}

/// Check if an import with the given module + name combination already exists.
///
/// For dedup: same module path and matching binding shape. Side-effect imports
/// are only duplicates of side-effect imports; namespace imports are distinct
/// from side-effect imports and from other namespace aliases.
pub fn is_duplicate(
    block: &ImportBlock,
    module_path: &str,
    names: &[String],
    default_import: Option<&str>,
    type_only: bool,
) -> bool {
    is_duplicate_with_namespace(block, module_path, names, default_import, None, type_only)
}

/// Check if an import with the given module + complete binding shape already exists.
pub fn is_duplicate_with_namespace(
    block: &ImportBlock,
    module_path: &str,
    names: &[String],
    default_import: Option<&str>,
    namespace_import: Option<&str>,
    type_only: bool,
) -> bool {
    let target_kind = if type_only {
        ImportKind::Type
    } else {
        ImportKind::Value
    };

    for imp in &block.imports {
        if imp.module_path != module_path {
            continue;
        }

        // For side-effect imports (no names/default/namespace): module path
        // match is sufficient only when the existing import is also a
        // side-effect import. Namespace imports like `import * as fs from 'fs'`
        // are distinct local bindings and must not be conflated with
        // `import 'fs'`.
        if names.is_empty()
            && default_import.is_none()
            && namespace_import.is_none()
            && imp.names.is_empty()
            && imp.default_import.is_none()
            && imp.namespace_import.is_none()
        {
            return true;
        }

        // For side-effect imports specifically (TS/JS): module match is enough
        if names.is_empty()
            && default_import.is_none()
            && namespace_import.is_none()
            && imp.kind == ImportKind::SideEffect
        {
            return true;
        }

        if names.is_empty()
            && default_import.is_none()
            && namespace_import.is_some()
            && imp.names.is_empty()
            && imp.default_import.is_none()
            && imp.namespace_import.as_deref() == namespace_import
        {
            return true;
        }

        // Kind must match for dedup (value imports don't dedup against type imports)
        if imp.kind != target_kind && imp.kind != ImportKind::SideEffect {
            continue;
        }

        // Check default import match
        if let Some(def) = default_import {
            if imp.default_import.as_deref() == Some(def) && imp.namespace_import.is_none() {
                return true;
            }
        }

        // Check named imports — if ALL requested names already exist.
        // Compare on the imported (pre-`as`) name so adding `Foo` is a
        // no-op when `Foo as Bar` is already imported, but adding
        // `Foo as Bar` is NOT a duplicate of bare `Foo` (different
        // local bindings).
        if !names.is_empty()
            && names
                .iter()
                .all(|n| imp.names.iter().any(|stored| specifier_matches(stored, n)))
        {
            return true;
        }
    }

    false
}

fn sort_named_specifiers(names: &mut [String]) {
    names.sort_by(|a, b| {
        specifier_imported_name(a)
            .cmp(specifier_imported_name(b))
            .then_with(|| a.cmp(b))
    });
}

/// Find the byte offset where a new import should be inserted.
///
/// Strategy:
/// - Find all existing imports in the same group.
/// - Within that group, find the alphabetical position by module path.
/// - Type imports sort after value imports within the same group and module-sort position.
/// - If no imports exist in the target group, insert after the last import of the
///   nearest preceding group (or before the first import of the nearest following
///   group, or at file start if no groups exist).
/// - Returns (byte_offset, needs_newline_before, needs_newline_after)
pub fn find_insertion_point(
    source: &str,
    block: &ImportBlock,
    group: ImportGroup,
    module_path: &str,
    type_only: bool,
) -> (usize, bool, bool) {
    if block.imports.is_empty() {
        // No imports at all — insert at start of file
        return (0, false, source.is_empty().then_some(false).unwrap_or(true));
    }

    let target_kind = if type_only {
        ImportKind::Type
    } else {
        ImportKind::Value
    };

    // Collect imports in the target group
    let group_imports: Vec<&ImportStatement> =
        block.imports.iter().filter(|i| i.group == group).collect();

    if group_imports.is_empty() {
        // No imports in this group yet — find nearest neighbor group
        // Try preceding groups (lower ordinal) first
        let preceding_last = block.imports.iter().filter(|i| i.group < group).last();

        if let Some(last) = preceding_last {
            let end = last.byte_range.end;
            let insert_at = skip_newline(source, end);
            return (insert_at, true, true);
        }

        // No preceding group — try following groups (higher ordinal)
        let following_first = block.imports.iter().find(|i| i.group > group);

        if let Some(first) = following_first {
            return (first.byte_range.start, false, true);
        }

        // Shouldn't reach here if block is non-empty, but handle gracefully
        let first_byte = import_byte_range(&block.imports)
            .map(|range| range.start)
            .unwrap_or(0);
        return (first_byte, false, true);
    }

    // Find position within the group (alphabetical by module path, type after value)
    for imp in &group_imports {
        let cmp = module_path.cmp(&imp.module_path);
        match cmp {
            std::cmp::Ordering::Less => {
                // Insert before this import
                return (imp.byte_range.start, false, false);
            }
            std::cmp::Ordering::Equal => {
                // Same module — type imports go after value imports
                if target_kind == ImportKind::Type && imp.kind == ImportKind::Value {
                    // Insert after this value import
                    let end = imp.byte_range.end;
                    let insert_at = skip_newline(source, end);
                    return (insert_at, false, false);
                }
                // Insert before (or it's a duplicate, caller should have checked)
                return (imp.byte_range.start, false, false);
            }
            std::cmp::Ordering::Greater => continue,
        }
    }

    // Module path sorts after all existing imports in this group — insert at end
    let Some(last) = group_imports.last() else {
        return (
            import_byte_range(&block.imports)
                .map(|range| range.end)
                .unwrap_or(0),
            false,
            false,
        );
    };
    let end = last.byte_range.end;
    let insert_at = skip_newline(source, end);
    (insert_at, false, false)
}

/// Generate an import line for the given language.
pub fn generate_import_line(
    lang: LangId,
    module_path: &str,
    names: &[String],
    default_import: Option<&str>,
    type_only: bool,
) -> String {
    generate_import_line_with_namespace(lang, module_path, names, default_import, None, type_only)
}

/// Generate an import line for the given language, including namespace imports
/// for TS/JS/TSX (`import * as ns from 'mod'`).
pub fn generate_import_line_with_namespace(
    lang: LangId,
    module_path: &str,
    names: &[String],
    default_import: Option<&str>,
    namespace_import: Option<&str>,
    type_only: bool,
) -> String {
    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => generate_ts_import_line(
            module_path,
            names,
            default_import,
            namespace_import,
            type_only,
        ),
        LangId::Python => generate_py_import_line(module_path, names, default_import),
        LangId::Rust => generate_rs_import_line(module_path, names, type_only),
        LangId::Go => generate_go_import_line(module_path, default_import, false),
        LangId::C
        | LangId::Cpp
        | LangId::Zig
        | LangId::CSharp
        | LangId::Bash
        | LangId::Solidity
        | LangId::Vue
        | LangId::Json
        | LangId::Scala
        | LangId::Java
        | LangId::Ruby
        | LangId::Kotlin
        | LangId::Swift
        | LangId::Php
        | LangId::Lua
        | LangId::Perl => String::new(),
        LangId::Html | LangId::Markdown => String::new(),
    }
}

/// Check if the given language is supported by the import engine.
pub fn is_supported(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::TypeScript
            | LangId::Tsx
            | LangId::JavaScript
            | LangId::Python
            | LangId::Rust
            | LangId::Go
    )
}

/// Classify a module path into a group for TS/JS/TSX.
pub fn classify_group_ts(module_path: &str) -> ImportGroup {
    if module_path.starts_with('.') {
        ImportGroup::Internal
    } else {
        ImportGroup::External
    }
}

/// Classify a module path into a group for the given language.
pub fn classify_group(lang: LangId, module_path: &str) -> ImportGroup {
    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => classify_group_ts(module_path),
        LangId::Python => classify_group_py(module_path),
        LangId::Rust => classify_group_rs(module_path),
        LangId::Go => classify_group_go(module_path),
        LangId::C
        | LangId::Cpp
        | LangId::Zig
        | LangId::CSharp
        | LangId::Bash
        | LangId::Solidity
        | LangId::Vue
        | LangId::Json
        | LangId::Scala
        | LangId::Java
        | LangId::Ruby
        | LangId::Kotlin
        | LangId::Swift
        | LangId::Php
        | LangId::Lua
        | LangId::Perl => ImportGroup::External,
        LangId::Html | LangId::Markdown => ImportGroup::External,
    }
}

/// Parse a file from disk and return its import block.
/// Convenience wrapper that handles parsing.
pub fn parse_file_imports(
    path: &std::path::Path,
    lang: LangId,
) -> Result<(String, Tree, ImportBlock), crate::error::AftError> {
    let source =
        std::fs::read_to_string(path).map_err(|e| crate::error::AftError::FileNotFound {
            path: format!("{}: {}", path.display(), e),
        })?;

    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|e| crate::error::AftError::ParseError {
            message: format!("grammar init failed for {:?}: {}", lang, e),
        })?;

    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| crate::error::AftError::ParseError {
            message: format!("tree-sitter parse returned None for {}", path.display()),
        })?;

    let block = parse_imports(&source, &tree, lang);
    Ok((source, tree, block))
}

// ---------------------------------------------------------------------------
// TS/JS/TSX implementation
// ---------------------------------------------------------------------------

/// Parse imports from a TS/JS/TSX file.
///
/// Walks the AST root's direct children looking for `import_statement` nodes (D041).
fn parse_ts_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();

    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return ImportBlock::empty();
    }

    loop {
        let node = cursor.node();
        if node.kind() == "import_statement" {
            if let Some(imp) = parse_single_ts_import(source, &node) {
                imports.push(imp);
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    let byte_range = import_byte_range(&imports);

    ImportBlock {
        imports,
        byte_range,
    }
}

/// Parse a single `import_statement` node into an `ImportStatement`.
fn parse_single_ts_import(source: &str, node: &Node) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();

    // Find the source module (string/string_fragment child of the import)
    let module_path = extract_module_path(source, node)?;

    // Determine if this is a type-only import: `import type ...`
    let is_type_only = has_type_keyword(node);

    // Extract import clause details
    let mut names = Vec::new();
    let mut default_import = None;
    let mut namespace_import = None;

    let mut child_cursor = node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            match child.kind() {
                "import_clause" => {
                    extract_import_clause(
                        source,
                        &child,
                        &mut names,
                        &mut default_import,
                        &mut namespace_import,
                    );
                }
                // In some grammars, the default import is a direct identifier child
                "identifier" => {
                    let text = &source[child.byte_range()];
                    if text != "import" && text != "from" && text != "type" {
                        default_import = Some(text.to_string());
                    }
                }
                _ => {}
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    // Classify kind
    let kind = if names.is_empty() && default_import.is_none() && namespace_import.is_none() {
        ImportKind::SideEffect
    } else if is_type_only {
        ImportKind::Type
    } else {
        ImportKind::Value
    };

    let group = classify_group_ts(&module_path);

    Some(ImportStatement {
        module_path,
        names,
        default_import,
        namespace_import,
        kind,
        group,
        byte_range,
        raw_text,
    })
}

/// Extract the module path string from an import_statement node.
///
/// Looks for a `string` child node and extracts the content without quotes.
fn extract_module_path(source: &str, node: &Node) -> Option<String> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let child = cursor.node();
        if child.kind() == "string" {
            // Get the text and strip quotes
            let text = &source[child.byte_range()];
            let stripped = text
                .trim_start_matches(|c| c == '\'' || c == '"')
                .trim_end_matches(|c| c == '\'' || c == '"');
            return Some(stripped.to_string());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    None
}

/// Check if the import_statement has a `type` keyword (import type ...).
///
/// In tree-sitter-typescript, `import type { X } from 'y'` produces a `type`
/// node as a direct child of `import_statement`, between `import` and `import_clause`.
fn has_type_keyword(node: &Node) -> bool {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return false;
    }

    loop {
        let child = cursor.node();
        if child.kind() == "type" {
            return true;
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    false
}

/// Extract named imports, default import, and namespace import from an import_clause.
fn extract_import_clause(
    source: &str,
    node: &Node,
    names: &mut Vec<String>,
    default_import: &mut Option<String>,
    namespace_import: &mut Option<String>,
) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let child = cursor.node();
        match child.kind() {
            "identifier" => {
                // This is a default import: `import Foo from 'bar'`
                let text = &source[child.byte_range()];
                if text != "type" {
                    *default_import = Some(text.to_string());
                }
            }
            "named_imports" => {
                // `{ name1, name2 }`
                extract_named_imports(source, &child, names);
            }
            "namespace_import" => {
                // `* as name`
                extract_namespace_import(source, &child, namespace_import);
            }
            _ => {}
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// Extract individual names from a named_imports node (`{ a, b, c }`).
///
/// Each name is stored verbatim including any alias and per-name `type`
/// modifier so the regenerator can round-trip them losslessly. Examples of
/// captured forms:
///
/// - `useState`               (plain)
/// - `stdin as input`         (renamed)
/// - `type Foo`               (per-specifier type-only)
/// - `type Foo as Bar`        (per-specifier type-only with rename)
///
/// **Why verbatim strings instead of a struct field per attribute:** dedup,
/// sort, dropping a single import, and the regenerator are all driven by
/// `Vec<String>` today. Encoding the alias inside the string preserves the
/// shape so the rest of the pipeline (organize, remove_import, move_symbol)
/// keeps working without a workspace-wide refactor. The cost is that callers
/// who want the canonical name (e.g. dedup) must compare on the leading
/// identifier only — see `extract_canonical_name` if you need that.
fn extract_named_imports(source: &str, node: &Node, names: &mut Vec<String>) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let child = cursor.node();
        if child.kind() == "import_specifier" {
            // Capture the full text of the specifier so per-name `type` markers
            // and `as alias` clauses are preserved across organize/regenerate
            // round-trips. Falls back to the imported name if the specifier
            // text is empty for any reason.
            let raw = source[child.byte_range()].trim().to_string();
            if !raw.is_empty() {
                names.push(raw);
            } else if let Some(name_node) = child.child_by_field_name("name") {
                names.push(source[name_node.byte_range()].to_string());
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// Extract the alias name from a namespace_import node (`* as name`).
fn extract_namespace_import(source: &str, node: &Node, namespace_import: &mut Option<String>) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let child = cursor.node();
        if child.kind() == "identifier" {
            *namespace_import = Some(source[child.byte_range()].to_string());
            return;
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// Generate an import line for TS/JS/TSX.
fn generate_ts_import_line(
    module_path: &str,
    names: &[String],
    default_import: Option<&str>,
    namespace_import: Option<&str>,
    type_only: bool,
) -> String {
    let type_prefix = if type_only { "type " } else { "" };

    // Side-effect import
    if names.is_empty() && default_import.is_none() && namespace_import.is_none() {
        return format!("import '{module_path}';");
    }

    // Namespace import only
    if names.is_empty() && default_import.is_none() {
        if let Some(namespace) = namespace_import {
            return format!("import {type_prefix}* as {namespace} from '{module_path}';");
        }
    }

    // Default + namespace import
    if names.is_empty() {
        if let (Some(def), Some(namespace)) = (default_import, namespace_import) {
            return format!("import {type_prefix}{def}, * as {namespace} from '{module_path}';");
        }
    }

    // Default import only
    if names.is_empty() && namespace_import.is_none() {
        if let Some(def) = default_import {
            return format!("import {type_prefix}{def} from '{module_path}';");
        }
    }

    // Named imports only
    if default_import.is_none() && namespace_import.is_none() {
        let mut sorted_names = names.to_vec();
        sort_named_specifiers(&mut sorted_names);
        let names_str = sorted_names.join(", ");
        return format!("import {type_prefix}{{ {names_str} }} from '{module_path}';");
    }

    // Namespace + named imports
    if default_import.is_none() {
        if let Some(namespace) = namespace_import {
            let mut sorted_names = names.to_vec();
            sort_named_specifiers(&mut sorted_names);
            let names_str = sorted_names.join(", ");
            return format!(
                "import {type_prefix}{{ {names_str} }}, * as {namespace} from '{module_path}';"
            );
        }
    }

    // Default + named + namespace imports
    if let (Some(def), Some(namespace)) = (default_import, namespace_import) {
        let mut sorted_names = names.to_vec();
        sort_named_specifiers(&mut sorted_names);
        let names_str = sorted_names.join(", ");
        return format!(
            "import {type_prefix}{def}, {{ {names_str} }}, * as {namespace} from '{module_path}';"
        );
    }

    // Both default and named imports
    if let Some(def) = default_import {
        let mut sorted_names = names.to_vec();
        sort_named_specifiers(&mut sorted_names);
        let names_str = sorted_names.join(", ");
        return format!("import {type_prefix}{def}, {{ {names_str} }} from '{module_path}';");
    }

    // Shouldn't reach here, but handle gracefully
    format!("import '{module_path}';")
}

// ---------------------------------------------------------------------------
// Python implementation
// ---------------------------------------------------------------------------

/// Python 3.x standard library module names (top-level modules).
/// Used for import group classification. Covers the commonly-used modules;
/// unknown modules are assumed third-party.
const PYTHON_STDLIB: &[&str] = &[
    "__future__",
    "_thread",
    "abc",
    "aifc",
    "argparse",
    "array",
    "ast",
    "asynchat",
    "asyncio",
    "asyncore",
    "atexit",
    "audioop",
    "base64",
    "bdb",
    "binascii",
    "bisect",
    "builtins",
    "bz2",
    "calendar",
    "cgi",
    "cgitb",
    "chunk",
    "cmath",
    "cmd",
    "code",
    "codecs",
    "codeop",
    "collections",
    "colorsys",
    "compileall",
    "concurrent",
    "configparser",
    "contextlib",
    "contextvars",
    "copy",
    "copyreg",
    "cProfile",
    "crypt",
    "csv",
    "ctypes",
    "curses",
    "dataclasses",
    "datetime",
    "dbm",
    "decimal",
    "difflib",
    "dis",
    "distutils",
    "doctest",
    "email",
    "encodings",
    "enum",
    "errno",
    "faulthandler",
    "fcntl",
    "filecmp",
    "fileinput",
    "fnmatch",
    "fractions",
    "ftplib",
    "functools",
    "gc",
    "getopt",
    "getpass",
    "gettext",
    "glob",
    "grp",
    "gzip",
    "hashlib",
    "heapq",
    "hmac",
    "html",
    "http",
    "idlelib",
    "imaplib",
    "imghdr",
    "importlib",
    "inspect",
    "io",
    "ipaddress",
    "itertools",
    "json",
    "keyword",
    "lib2to3",
    "linecache",
    "locale",
    "logging",
    "lzma",
    "mailbox",
    "mailcap",
    "marshal",
    "math",
    "mimetypes",
    "mmap",
    "modulefinder",
    "multiprocessing",
    "netrc",
    "numbers",
    "operator",
    "optparse",
    "os",
    "pathlib",
    "pdb",
    "pickle",
    "pickletools",
    "pipes",
    "pkgutil",
    "platform",
    "plistlib",
    "poplib",
    "posixpath",
    "pprint",
    "profile",
    "pstats",
    "pty",
    "pwd",
    "py_compile",
    "pyclbr",
    "pydoc",
    "queue",
    "quopri",
    "random",
    "re",
    "readline",
    "reprlib",
    "resource",
    "rlcompleter",
    "runpy",
    "sched",
    "secrets",
    "select",
    "selectors",
    "shelve",
    "shlex",
    "shutil",
    "signal",
    "site",
    "smtplib",
    "sndhdr",
    "socket",
    "socketserver",
    "sqlite3",
    "ssl",
    "stat",
    "statistics",
    "string",
    "stringprep",
    "struct",
    "subprocess",
    "symtable",
    "sys",
    "sysconfig",
    "syslog",
    "tabnanny",
    "tarfile",
    "tempfile",
    "termios",
    "textwrap",
    "threading",
    "time",
    "timeit",
    "tkinter",
    "token",
    "tokenize",
    "tomllib",
    "trace",
    "traceback",
    "tracemalloc",
    "tty",
    "turtle",
    "types",
    "typing",
    "unicodedata",
    "unittest",
    "urllib",
    "uuid",
    "venv",
    "warnings",
    "wave",
    "weakref",
    "webbrowser",
    "wsgiref",
    "xml",
    "xmlrpc",
    "zipapp",
    "zipfile",
    "zipimport",
    "zlib",
];

/// Classify a Python import into a group.
pub fn classify_group_py(module_path: &str) -> ImportGroup {
    // Relative imports start with '.'
    if module_path.starts_with('.') {
        return ImportGroup::Internal;
    }
    // Check stdlib: use the top-level module name (before first '.')
    let top_module = module_path.split('.').next().unwrap_or(module_path);
    if PYTHON_STDLIB.contains(&top_module) {
        ImportGroup::Stdlib
    } else {
        ImportGroup::External
    }
}

/// Parse imports from a Python file.
fn parse_py_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();

    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return ImportBlock::empty();
    }

    loop {
        let node = cursor.node();
        match node.kind() {
            "import_statement" => {
                if let Some(imp) = parse_py_import_statement(source, &node) {
                    imports.push(imp);
                }
            }
            "import_from_statement" => {
                if let Some(imp) = parse_py_import_from_statement(source, &node) {
                    imports.push(imp);
                }
            }
            _ => {}
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    let byte_range = import_byte_range(&imports);

    ImportBlock {
        imports,
        byte_range,
    }
}

/// Parse `import X` or `import X.Y` Python statements.
fn parse_py_import_statement(source: &str, node: &Node) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();

    // Find the dotted_name child (the module name)
    let mut module_path = String::new();
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            if c.node().kind() == "dotted_name" {
                module_path = source[c.node().byte_range()].to_string();
                break;
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
    if module_path.is_empty() {
        return None;
    }

    let group = classify_group_py(&module_path);

    Some(ImportStatement {
        module_path,
        names: Vec::new(),
        default_import: None,
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
    })
}

/// Parse `from X import Y, Z` or `from . import Y` Python statements.
fn parse_py_import_from_statement(source: &str, node: &Node) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();

    let mut module_path = String::new();
    let mut names = Vec::new();

    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            let child = c.node();
            match child.kind() {
                "dotted_name" => {
                    // Could be the module name or an imported name
                    // The module name comes right after `from`, imported names come after `import`
                    // Use position: if we haven't set module_path yet and this comes
                    // before the `import` keyword, it's the module.
                    if module_path.is_empty()
                        && !has_seen_import_keyword(source, node, child.start_byte())
                    {
                        module_path = source[child.byte_range()].to_string();
                    } else {
                        // It's an imported name
                        names.push(source[child.byte_range()].to_string());
                    }
                }
                "relative_import" => {
                    // from . import X or from ..module import X
                    module_path = source[child.byte_range()].to_string();
                }
                _ => {}
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }

    // module_path must be non-empty for a valid import
    if module_path.is_empty() {
        return None;
    }

    let group = classify_group_py(&module_path);

    Some(ImportStatement {
        module_path,
        names,
        default_import: None,
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
    })
}

/// Check if the `import` keyword appears before the given byte position in a from...import node.
fn has_seen_import_keyword(_source: &str, parent: &Node, before_byte: usize) -> bool {
    let mut c = parent.walk();
    if c.goto_first_child() {
        loop {
            let child = c.node();
            if child.kind() == "import" && child.start_byte() < before_byte {
                return true;
            }
            if child.start_byte() >= before_byte {
                return false;
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

/// Generate a Python import line.
fn generate_py_import_line(
    module_path: &str,
    names: &[String],
    _default_import: Option<&str>,
) -> String {
    if names.is_empty() {
        // `import module`
        format!("import {module_path}")
    } else {
        // `from module import name1, name2`
        let mut sorted = names.to_vec();
        sorted.sort();
        let names_str = sorted.join(", ");
        format!("from {module_path} import {names_str}")
    }
}

// ---------------------------------------------------------------------------
// Rust implementation
// ---------------------------------------------------------------------------

/// Classify a Rust use path into a group.
pub fn classify_group_rs(module_path: &str) -> ImportGroup {
    // Extract the first path segment (before ::)
    let first_seg = module_path.split("::").next().unwrap_or(module_path);
    match first_seg {
        "std" | "core" | "alloc" => ImportGroup::Stdlib,
        "crate" | "self" | "super" => ImportGroup::Internal,
        _ => ImportGroup::External,
    }
}

/// Parse imports from a Rust file.
fn parse_rs_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();

    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return ImportBlock::empty();
    }

    loop {
        let node = cursor.node();
        if node.kind() == "use_declaration" {
            if let Some(imp) = parse_rs_use_declaration(source, &node) {
                imports.push(imp);
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    let byte_range = import_byte_range(&imports);

    ImportBlock {
        imports,
        byte_range,
    }
}

/// Parse a single `use` declaration from Rust.
fn parse_rs_use_declaration(source: &str, node: &Node) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();

    // Check for `pub` visibility modifier
    let mut has_pub = false;
    let mut use_path = String::new();
    let mut names = Vec::new();

    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            let child = c.node();
            match child.kind() {
                "visibility_modifier" => {
                    has_pub = true;
                }
                "scoped_identifier" | "identifier" | "use_as_clause" => {
                    // Full path like `std::collections::HashMap` or just `serde`
                    use_path = source[child.byte_range()].to_string();
                }
                "scoped_use_list" => {
                    // e.g. `serde::{Deserialize, Serialize}`
                    use_path = source[child.byte_range()].to_string();
                    // Also extract the individual names from the use_list
                    extract_rs_use_list_names(source, &child, &mut names);
                }
                _ => {}
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }

    if use_path.is_empty() {
        return None;
    }

    let group = classify_group_rs(&use_path);

    Some(ImportStatement {
        module_path: use_path,
        names,
        default_import: if has_pub {
            Some("pub".to_string())
        } else {
            None
        },
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
    })
}

/// Extract individual names from a Rust `scoped_use_list` node.
fn extract_rs_use_list_names(source: &str, node: &Node, names: &mut Vec<String>) {
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            let child = c.node();
            if child.kind() == "use_list" {
                // Walk into the use_list to find identifiers
                let mut lc = child.walk();
                if lc.goto_first_child() {
                    loop {
                        let lchild = lc.node();
                        if lchild.kind() == "identifier" || lchild.kind() == "scoped_identifier" {
                            names.push(source[lchild.byte_range()].to_string());
                        }
                        if !lc.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Generate a Rust import line.
fn generate_rs_import_line(module_path: &str, names: &[String], _type_only: bool) -> String {
    if names.is_empty() {
        format!("use {module_path};")
    } else {
        // If names are provided, generate `use prefix::{names};`
        // But the caller may pass module_path as the full path including the item,
        // e.g., "serde::Deserialize". For simple cases, just use the module_path directly.
        format!("use {module_path};")
    }
}

// ---------------------------------------------------------------------------
// Go implementation
// ---------------------------------------------------------------------------

/// Classify a Go import path into a group.
pub fn classify_group_go(module_path: &str) -> ImportGroup {
    // stdlib paths don't contain dots (e.g., "fmt", "os", "net/http")
    // external paths contain dots (e.g., "github.com/pkg/errors")
    if module_path.contains('.') {
        ImportGroup::External
    } else {
        ImportGroup::Stdlib
    }
}

/// Parse imports from a Go file.
fn parse_go_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();

    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return ImportBlock::empty();
    }

    loop {
        let node = cursor.node();
        if node.kind() == "import_declaration" {
            parse_go_import_declaration(source, &node, &mut imports);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    let byte_range = import_byte_range(&imports);

    ImportBlock {
        imports,
        byte_range,
    }
}

/// Parse a single Go import_declaration (may contain one or multiple specs).
fn parse_go_import_declaration(source: &str, node: &Node, imports: &mut Vec<ImportStatement>) {
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            let child = c.node();
            match child.kind() {
                "import_spec" => {
                    if let Some(imp) = parse_go_import_spec(source, &child) {
                        imports.push(imp);
                    }
                }
                "import_spec_list" => {
                    // Grouped imports: walk into the list
                    let mut lc = child.walk();
                    if lc.goto_first_child() {
                        loop {
                            if lc.node().kind() == "import_spec" {
                                if let Some(imp) = parse_go_import_spec(source, &lc.node()) {
                                    imports.push(imp);
                                }
                            }
                            if !lc.goto_next_sibling() {
                                break;
                            }
                        }
                    }
                }
                _ => {}
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Parse a single Go import_spec node.
fn parse_go_import_spec(source: &str, node: &Node) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();

    let mut import_path = String::new();
    let mut alias = None;

    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            let child = c.node();
            match child.kind() {
                "interpreted_string_literal" => {
                    // Extract the path without quotes
                    let text = source[child.byte_range()].to_string();
                    import_path = text.trim_matches('"').to_string();
                }
                "identifier" | "blank_identifier" | "dot" => {
                    // This is an alias (e.g., `alias "path"` or `. "path"` or `_ "path"`)
                    alias = Some(source[child.byte_range()].to_string());
                }
                _ => {}
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }

    if import_path.is_empty() {
        return None;
    }

    let group = classify_group_go(&import_path);

    Some(ImportStatement {
        module_path: import_path,
        names: Vec::new(),
        default_import: alias,
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
    })
}

/// Public API for Go import line generation (used by add_import handler).
pub fn generate_go_import_line_pub(
    module_path: &str,
    alias: Option<&str>,
    in_group: bool,
) -> String {
    generate_go_import_line(module_path, alias, in_group)
}

/// Generate a Go import line (public API for command handler).
///
/// `in_group` controls whether to generate a spec for insertion into an
/// existing grouped import (`\t"path"`) or a standalone import (`import "path"`).
fn generate_go_import_line(module_path: &str, alias: Option<&str>, in_group: bool) -> String {
    if in_group {
        // Spec for grouped import block
        match alias {
            Some(a) => format!("\t{a} \"{module_path}\""),
            None => format!("\t\"{module_path}\""),
        }
    } else {
        // Standalone import
        match alias {
            Some(a) => format!("import {a} \"{module_path}\""),
            None => format!("import \"{module_path}\""),
        }
    }
}

/// Check if a Go import block has a grouped import declaration.
/// Returns the byte range of the full import_declaration if found.
pub fn go_has_grouped_import(_source: &str, tree: &Tree) -> Option<Range<usize>> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "import_declaration" {
            let mut c = node.walk();
            if c.goto_first_child() {
                loop {
                    if c.node().kind() == "import_spec_list" {
                        return Some(node.byte_range());
                    }
                    if !c.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    None
}

/// Skip past a newline character at the given position.
fn skip_newline(source: &str, pos: usize) -> usize {
    if pos < source.len() {
        let bytes = source.as_bytes();
        if bytes[pos] == b'\n' {
            return pos + 1;
        }
        if bytes[pos] == b'\r' {
            if pos + 1 < source.len() && bytes[pos + 1] == b'\n' {
                return pos + 2;
            }
            return pos + 1;
        }
    }
    pos
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ts(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::TypeScript);
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::TypeScript);
        (tree, block)
    }

    fn parse_js(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::JavaScript);
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::JavaScript);
        (tree, block)
    }

    // --- Basic parsing ---

    #[test]
    fn parse_ts_named_imports() {
        let source = "import { useState, useEffect } from 'react';\n";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 1);
        let imp = &block.imports[0];
        assert_eq!(imp.module_path, "react");
        assert!(imp.names.contains(&"useState".to_string()));
        assert!(imp.names.contains(&"useEffect".to_string()));
        assert_eq!(imp.kind, ImportKind::Value);
        assert_eq!(imp.group, ImportGroup::External);
    }

    #[test]
    fn parse_ts_default_import() {
        let source = "import React from 'react';\n";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 1);
        let imp = &block.imports[0];
        assert_eq!(imp.default_import.as_deref(), Some("React"));
        assert_eq!(imp.kind, ImportKind::Value);
    }

    #[test]
    fn parse_ts_side_effect_import() {
        let source = "import './styles.css';\n";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 1);
        assert_eq!(block.imports[0].kind, ImportKind::SideEffect);
        assert_eq!(block.imports[0].module_path, "./styles.css");
    }

    #[test]
    fn parse_ts_relative_import() {
        let source = "import { helper } from './utils';\n";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 1);
        assert_eq!(block.imports[0].group, ImportGroup::Internal);
    }

    #[test]
    fn parse_ts_multiple_groups() {
        let source = "\
import React from 'react';
import { useState } from 'react';
import { helper } from './utils';
import { Config } from '../config';
";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 4);

        let external: Vec<_> = block
            .imports
            .iter()
            .filter(|i| i.group == ImportGroup::External)
            .collect();
        let relative: Vec<_> = block
            .imports
            .iter()
            .filter(|i| i.group == ImportGroup::Internal)
            .collect();
        assert_eq!(external.len(), 2);
        assert_eq!(relative.len(), 2);
    }

    #[test]
    fn parse_ts_namespace_import() {
        let source = "import * as path from 'path';\n";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 1);
        let imp = &block.imports[0];
        assert_eq!(imp.namespace_import.as_deref(), Some("path"));
        assert_eq!(imp.kind, ImportKind::Value);
    }

    #[test]
    fn parse_js_imports() {
        let source = "import { readFile } from 'fs';\nimport { helper } from './helper';\n";
        let (_, block) = parse_js(source);
        assert_eq!(block.imports.len(), 2);
        assert_eq!(block.imports[0].group, ImportGroup::External);
        assert_eq!(block.imports[1].group, ImportGroup::Internal);
    }

    // --- Group classification ---

    #[test]
    fn classify_external() {
        assert_eq!(classify_group_ts("react"), ImportGroup::External);
        assert_eq!(classify_group_ts("@scope/pkg"), ImportGroup::External);
        assert_eq!(classify_group_ts("lodash/map"), ImportGroup::External);
    }

    #[test]
    fn classify_relative() {
        assert_eq!(classify_group_ts("./utils"), ImportGroup::Internal);
        assert_eq!(classify_group_ts("../config"), ImportGroup::Internal);
        assert_eq!(classify_group_ts("./"), ImportGroup::Internal);
    }

    // --- Dedup ---

    #[test]
    fn dedup_detects_same_named_import() {
        let source = "import { useState } from 'react';\n";
        let (_, block) = parse_ts(source);
        assert!(is_duplicate(
            &block,
            "react",
            &["useState".to_string()],
            None,
            false
        ));
    }

    #[test]
    fn dedup_misses_different_name() {
        let source = "import { useState } from 'react';\n";
        let (_, block) = parse_ts(source);
        assert!(!is_duplicate(
            &block,
            "react",
            &["useEffect".to_string()],
            None,
            false
        ));
    }

    #[test]
    fn dedup_detects_default_import() {
        let source = "import React from 'react';\n";
        let (_, block) = parse_ts(source);
        assert!(is_duplicate(&block, "react", &[], Some("React"), false));
    }

    #[test]
    fn dedup_side_effect() {
        let source = "import './styles.css';\n";
        let (_, block) = parse_ts(source);
        assert!(is_duplicate(&block, "./styles.css", &[], None, false));
    }

    #[test]
    fn dedup_namespace_import_distinct_from_side_effect_import() {
        let side_effect_source = "import 'fs';\n";
        let (_, side_effect_block) = parse_ts(side_effect_source);
        assert!(!is_duplicate_with_namespace(
            &side_effect_block,
            "fs",
            &[],
            None,
            Some("fs"),
            false
        ));

        let namespace_source = "import * as fs from 'fs';\n";
        let (_, namespace_block) = parse_ts(namespace_source);
        assert!(!is_duplicate(&namespace_block, "fs", &[], None, false));
        assert!(is_duplicate_with_namespace(
            &namespace_block,
            "fs",
            &[],
            None,
            Some("fs"),
            false
        ));
        assert!(!is_duplicate_with_namespace(
            &namespace_block,
            "fs",
            &[],
            None,
            Some("other"),
            false
        ));
    }

    #[test]
    fn dedup_type_vs_value() {
        let source = "import { FC } from 'react';\n";
        let (_, block) = parse_ts(source);
        // Type import should NOT match a value import of the same name
        assert!(!is_duplicate(
            &block,
            "react",
            &["FC".to_string()],
            None,
            true
        ));
    }

    // --- Generation ---

    #[test]
    fn generate_named_import() {
        let line = generate_import_line(
            LangId::TypeScript,
            "react",
            &["useState".to_string(), "useEffect".to_string()],
            None,
            false,
        );
        assert_eq!(line, "import { useEffect, useState } from 'react';");
    }

    #[test]
    fn generate_named_import_sorts_by_imported_name() {
        let line = generate_import_line(
            LangId::TypeScript,
            "x",
            &[
                "useState".to_string(),
                "type Foo".to_string(),
                "stdin as input".to_string(),
                "type Bar".to_string(),
            ],
            None,
            false,
        );
        assert_eq!(
            line,
            "import { type Bar, type Foo, stdin as input, useState } from 'x';"
        );
    }

    #[test]
    fn generate_default_import() {
        let line = generate_import_line(LangId::TypeScript, "react", &[], Some("React"), false);
        assert_eq!(line, "import React from 'react';");
    }

    #[test]
    fn generate_type_import() {
        let line =
            generate_import_line(LangId::TypeScript, "react", &["FC".to_string()], None, true);
        assert_eq!(line, "import type { FC } from 'react';");
    }

    #[test]
    fn generate_side_effect_import() {
        let line = generate_import_line(LangId::TypeScript, "./styles.css", &[], None, false);
        assert_eq!(line, "import './styles.css';");
    }

    #[test]
    fn generate_default_and_named() {
        let line = generate_import_line(
            LangId::TypeScript,
            "react",
            &["useState".to_string()],
            Some("React"),
            false,
        );
        assert_eq!(line, "import React, { useState } from 'react';");
    }

    #[test]
    fn parse_ts_type_import() {
        let source = "import type { FC } from 'react';\n";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 1);
        let imp = &block.imports[0];
        assert_eq!(imp.kind, ImportKind::Type);
        assert!(imp.names.contains(&"FC".to_string()));
        assert_eq!(imp.group, ImportGroup::External);
    }

    // --- Insertion point ---

    #[test]
    fn insertion_empty_file() {
        let source = "";
        let (_, block) = parse_ts(source);
        let (offset, _, _) =
            find_insertion_point(source, &block, ImportGroup::External, "react", false);
        assert_eq!(offset, 0);
    }

    #[test]
    fn insertion_alphabetical_within_group() {
        let source = "\
import { a } from 'alpha';
import { c } from 'charlie';
";
        let (_, block) = parse_ts(source);
        let (offset, _, _) =
            find_insertion_point(source, &block, ImportGroup::External, "bravo", false);
        // Should insert before 'charlie' (which starts at line 2)
        let before_charlie = source.find("import { c }").unwrap();
        assert_eq!(offset, before_charlie);
    }

    // --- Python parsing ---

    fn parse_py(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Python);
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Python);
        (tree, block)
    }

    #[test]
    fn parse_py_import_statement() {
        let source = "import os\nimport sys\n";
        let (_, block) = parse_py(source);
        assert_eq!(block.imports.len(), 2);
        assert_eq!(block.imports[0].module_path, "os");
        assert_eq!(block.imports[1].module_path, "sys");
        assert_eq!(block.imports[0].group, ImportGroup::Stdlib);
    }

    #[test]
    fn parse_py_from_import() {
        let source = "from collections import OrderedDict\nfrom typing import List, Optional\n";
        let (_, block) = parse_py(source);
        assert_eq!(block.imports.len(), 2);
        assert_eq!(block.imports[0].module_path, "collections");
        assert!(block.imports[0].names.contains(&"OrderedDict".to_string()));
        assert_eq!(block.imports[0].group, ImportGroup::Stdlib);
        assert_eq!(block.imports[1].module_path, "typing");
        assert!(block.imports[1].names.contains(&"List".to_string()));
        assert!(block.imports[1].names.contains(&"Optional".to_string()));
    }

    #[test]
    fn parse_py_relative_import() {
        let source = "from . import utils\nfrom ..config import Settings\n";
        let (_, block) = parse_py(source);
        assert_eq!(block.imports.len(), 2);
        assert_eq!(block.imports[0].module_path, ".");
        assert!(block.imports[0].names.contains(&"utils".to_string()));
        assert_eq!(block.imports[0].group, ImportGroup::Internal);
        assert_eq!(block.imports[1].module_path, "..config");
        assert_eq!(block.imports[1].group, ImportGroup::Internal);
    }

    #[test]
    fn classify_py_groups() {
        assert_eq!(classify_group_py("os"), ImportGroup::Stdlib);
        assert_eq!(classify_group_py("sys"), ImportGroup::Stdlib);
        assert_eq!(classify_group_py("json"), ImportGroup::Stdlib);
        assert_eq!(classify_group_py("collections"), ImportGroup::Stdlib);
        assert_eq!(classify_group_py("os.path"), ImportGroup::Stdlib);
        assert_eq!(classify_group_py("requests"), ImportGroup::External);
        assert_eq!(classify_group_py("flask"), ImportGroup::External);
        assert_eq!(classify_group_py("."), ImportGroup::Internal);
        assert_eq!(classify_group_py("..config"), ImportGroup::Internal);
        assert_eq!(classify_group_py(".utils"), ImportGroup::Internal);
    }

    #[test]
    fn parse_py_three_groups() {
        let source = "import os\nimport sys\n\nimport requests\n\nfrom . import utils\n";
        let (_, block) = parse_py(source);
        let stdlib: Vec<_> = block
            .imports
            .iter()
            .filter(|i| i.group == ImportGroup::Stdlib)
            .collect();
        let external: Vec<_> = block
            .imports
            .iter()
            .filter(|i| i.group == ImportGroup::External)
            .collect();
        let internal: Vec<_> = block
            .imports
            .iter()
            .filter(|i| i.group == ImportGroup::Internal)
            .collect();
        assert_eq!(stdlib.len(), 2);
        assert_eq!(external.len(), 1);
        assert_eq!(internal.len(), 1);
    }

    #[test]
    fn generate_py_import() {
        let line = generate_import_line(LangId::Python, "os", &[], None, false);
        assert_eq!(line, "import os");
    }

    #[test]
    fn generate_py_from_import() {
        let line = generate_import_line(
            LangId::Python,
            "collections",
            &["OrderedDict".to_string()],
            None,
            false,
        );
        assert_eq!(line, "from collections import OrderedDict");
    }

    #[test]
    fn generate_py_from_import_multiple() {
        let line = generate_import_line(
            LangId::Python,
            "typing",
            &["Optional".to_string(), "List".to_string()],
            None,
            false,
        );
        assert_eq!(line, "from typing import List, Optional");
    }

    // --- Rust parsing ---

    fn parse_rust(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Rust);
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Rust);
        (tree, block)
    }

    #[test]
    fn parse_rs_use_std() {
        let source = "use std::collections::HashMap;\nuse std::io::Read;\n";
        let (_, block) = parse_rust(source);
        assert_eq!(block.imports.len(), 2);
        assert_eq!(block.imports[0].module_path, "std::collections::HashMap");
        assert_eq!(block.imports[0].group, ImportGroup::Stdlib);
        assert_eq!(block.imports[1].group, ImportGroup::Stdlib);
    }

    #[test]
    fn parse_rs_use_external() {
        let source = "use serde::{Deserialize, Serialize};\n";
        let (_, block) = parse_rust(source);
        assert_eq!(block.imports.len(), 1);
        assert_eq!(block.imports[0].group, ImportGroup::External);
        assert!(block.imports[0].names.contains(&"Deserialize".to_string()));
        assert!(block.imports[0].names.contains(&"Serialize".to_string()));
    }

    #[test]
    fn parse_rs_use_crate() {
        let source = "use crate::config::Settings;\nuse super::parent::Thing;\n";
        let (_, block) = parse_rust(source);
        assert_eq!(block.imports.len(), 2);
        assert_eq!(block.imports[0].group, ImportGroup::Internal);
        assert_eq!(block.imports[1].group, ImportGroup::Internal);
    }

    #[test]
    fn parse_rs_pub_use() {
        let source = "pub use super::parent::Thing;\n";
        let (_, block) = parse_rust(source);
        assert_eq!(block.imports.len(), 1);
        // `pub` is stored in default_import as a marker
        assert_eq!(block.imports[0].default_import.as_deref(), Some("pub"));
    }

    #[test]
    fn classify_rs_groups() {
        assert_eq!(
            classify_group_rs("std::collections::HashMap"),
            ImportGroup::Stdlib
        );
        assert_eq!(classify_group_rs("core::mem"), ImportGroup::Stdlib);
        assert_eq!(classify_group_rs("alloc::vec"), ImportGroup::Stdlib);
        assert_eq!(
            classify_group_rs("serde::Deserialize"),
            ImportGroup::External
        );
        assert_eq!(classify_group_rs("tokio::runtime"), ImportGroup::External);
        assert_eq!(classify_group_rs("crate::config"), ImportGroup::Internal);
        assert_eq!(classify_group_rs("self::utils"), ImportGroup::Internal);
        assert_eq!(classify_group_rs("super::parent"), ImportGroup::Internal);
    }

    #[test]
    fn generate_rs_use() {
        let line = generate_import_line(LangId::Rust, "std::fmt::Display", &[], None, false);
        assert_eq!(line, "use std::fmt::Display;");
    }

    // --- Go parsing ---

    fn parse_go(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Go);
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Go);
        (tree, block)
    }

    #[test]
    fn parse_go_single_import() {
        let source = "package main\n\nimport \"fmt\"\n";
        let (_, block) = parse_go(source);
        assert_eq!(block.imports.len(), 1);
        assert_eq!(block.imports[0].module_path, "fmt");
        assert_eq!(block.imports[0].group, ImportGroup::Stdlib);
    }

    #[test]
    fn parse_go_grouped_import() {
        let source =
            "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n\n\t\"github.com/pkg/errors\"\n)\n";
        let (_, block) = parse_go(source);
        assert_eq!(block.imports.len(), 3);
        assert_eq!(block.imports[0].module_path, "fmt");
        assert_eq!(block.imports[0].group, ImportGroup::Stdlib);
        assert_eq!(block.imports[1].module_path, "os");
        assert_eq!(block.imports[1].group, ImportGroup::Stdlib);
        assert_eq!(block.imports[2].module_path, "github.com/pkg/errors");
        assert_eq!(block.imports[2].group, ImportGroup::External);
    }

    #[test]
    fn parse_go_mixed_imports() {
        // Single + grouped
        let source = "package main\n\nimport \"fmt\"\n\nimport (\n\t\"os\"\n\t\"github.com/pkg/errors\"\n)\n";
        let (_, block) = parse_go(source);
        assert_eq!(block.imports.len(), 3);
    }

    #[test]
    fn classify_go_groups() {
        assert_eq!(classify_group_go("fmt"), ImportGroup::Stdlib);
        assert_eq!(classify_group_go("os"), ImportGroup::Stdlib);
        assert_eq!(classify_group_go("net/http"), ImportGroup::Stdlib);
        assert_eq!(classify_group_go("encoding/json"), ImportGroup::Stdlib);
        assert_eq!(
            classify_group_go("github.com/pkg/errors"),
            ImportGroup::External
        );
        assert_eq!(
            classify_group_go("golang.org/x/tools"),
            ImportGroup::External
        );
    }

    #[test]
    fn generate_go_standalone() {
        let line = generate_go_import_line("fmt", None, false);
        assert_eq!(line, "import \"fmt\"");
    }

    #[test]
    fn generate_go_grouped_spec() {
        let line = generate_go_import_line("fmt", None, true);
        assert_eq!(line, "\t\"fmt\"");
    }

    #[test]
    fn generate_go_with_alias() {
        let line = generate_go_import_line("github.com/pkg/errors", Some("errs"), false);
        assert_eq!(line, "import errs \"github.com/pkg/errors\"");
    }
}
