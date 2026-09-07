//! Import analysis engine: parsing, grouping, deduplication, and insertion.
//!
//! Per-language behavior is provided by [`ImportSyntax`] implementations,
//! resolved through the [`syntax_for`] registry. Each engine extracts imports
//! from tree-sitter ASTs, classifies them into groups, and generates import
//! text. A single import's structured shape is carried by [`ImportForm`].
//!
//! Currently supports: TypeScript, TSX, JavaScript, Python, Rust, Go.

use std::ops::Range;

use tree_sitter::{Node, Parser, Tree};

use crate::parser::{grammar_for, LangId};

mod c;
pub(crate) use c::{classify_group_c_import_kind, normalize_include_module};
mod csharp;
mod java;
mod kotlin;
mod lua;
pub(crate) mod perl;
mod php;
pub(crate) use php::{
    php_grouped_use_matches_module, php_grouped_use_shares_prefix, php_import_matches_module,
    rewrite_php_import_without_module,
};
pub(crate) mod ruby;
mod scala;
pub(crate) use scala::scala_block_uses_scala2_dialect;
mod swift;

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

/// One clause in a PHP `use` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpImportClause {
    /// Fully qualified imported symbol for this clause.
    pub module_path: String,
    /// Local alias introduced by `as`, when present.
    pub alias: Option<String>,
    /// PHP import namespace (`function` or `const`), when present.
    pub import_kind: Option<String>,
}

/// Structured, language-honest representation of a single import's shape.
///
/// This is the migration target that replaces the TS-shaped flat fields
/// (`names`/`default_import`/`namespace_import`) and their per-language
/// overloads (Rust packs `"pub"` into `default_import`; Go packs the alias
/// there). It is introduced additively alongside the flat fields (Stream M of
/// the imports-refactor plan): parsers populate BOTH, readers migrate onto
/// `form` one at a time behind the golden-parity gate, and the flat fields are
/// removed once no reader depends on them. New-language variants (Static,
/// Include, RuntimeRequire, …) are added when their engines land — only the
/// variants the existing engines produce exist today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportForm {
    /// ES modules: TypeScript, TSX, JavaScript.
    /// `named` holds verbatim specifiers (`"useState"`, `"stdin as input"`,
    /// `"type Foo"`, `"type Foo as Bar"`) — see [`specifier_imported_name`].
    Es {
        default_import: Option<String>,
        namespace_import: Option<String>,
        named: Vec<String>,
        /// Statement-level `import type { ... }`.
        type_only: bool,
        /// Side-effect-only `import "mod"` (no bindings).
        side_effect: bool,
        /// Verbatim `with { ... }` or legacy `assert { ... }` clause.
        /// Import attributes select the module type, so dropping this clause can
        /// change a valid import into one the runtime refuses to load.
        attribute_clause: Option<String>,
        /// Decoded value of the clause's `type` attribute, when parsed.
        attribute_type: Option<String>,
    },
    /// Python `import module` (`from_import = false`) or
    /// `from module import a, b` (`from_import = true`).
    Python {
        from_import: bool,
        named: Vec<String>,
    },
    /// Rust `use path;` / `pub use path;`. `visibility` replaces the
    /// `default_import == "pub"` overload (`Some("pub")`, `Some("pub(crate)")`,
    /// …). The brace/use-tree text remains carried by `module_path` per the
    /// lossless-round-trip decision; `named` holds extracted use-list names.
    RustUse {
        visibility: Option<String>,
        named: Vec<String>,
    },
    /// Go import. `alias` replaces the `default_import` overload, including the
    /// blank (`_`) and dot (`.`) import bindings.
    Go { alias: Option<String> },
    /// Solidity import, in one of four forms:
    /// - side-effect: `import "x";` (all empty)
    /// - named: `import { A, B as C } from "x";` (`named`)
    /// - namespace: `import * as A from "x";` (`namespace`)
    /// - whole-file alias: `import "x" as A;` (`alias`)
    ///
    /// `named` holds verbatim specifiers (`"A"`, `"B as C"`) like the ES form,
    /// so [`specifier_imported_name`] / [`specifier_local_name`] apply.
    Solidity {
        named: Vec<String>,
        namespace: Option<String>,
        alias: Option<String>,
    },
    /// One physical PHP `use` declaration. A declaration may contain multiple
    /// comma-separated clauses, each with its own path, alias, and import kind.
    Php { clauses: Vec<PhpImportClause> },
    /// Generic structured form shared by Java, C#, grouped PHP uses, Kotlin,
    /// Scala, Swift, and other engines that use this common schema. Includes all
    /// schema fields so a new engine does not need its own enum variant;
    /// `module_path` on the parent `ImportStatement` stores the full path or
    /// fully qualified name, while `named` stores each specifier exactly as parsed.
    Structured {
        named: Vec<String>,
        namespace: Option<String>,
        alias: Option<String>,
        modifiers: Vec<String>,
        import_kind: Option<String>,
    },
}

/// Structured request to generate a single import line. Superset of the fields
/// the public `aft_import` schema exposes; each engine reads only the subset it
/// supports. New languages add fields here rather than growing positional
/// parameters on every generator signature.
#[derive(Debug, Clone)]
pub struct ImportRequest<'a> {
    pub module_path: &'a str,
    pub names: &'a [String],
    pub default_import: Option<&'a str>,
    /// ES `* as ns` / Solidity `* as A`.
    pub namespace: Option<&'a str>,
    /// Whole-module local alias (Solidity `import "x" as A`).
    pub alias: Option<&'a str>,
    pub type_only: bool,
    /// Statement-level modifier tokens (Java/C# `static`, C# `global`/`unsafe`,
    /// `wildcard`, Swift `@testable`, …). Empty for the legacy engines.
    pub modifiers: &'a [String],
    /// Symbol-kind-specific import (PHP `function`/`const`, Swift `struct`/…,
    /// Scala `given`). Absent for the legacy engines.
    pub import_kind: Option<&'a str>,
}

/// Empty default for the `modifiers` slice so legacy callers need not allocate.
const NO_MODIFIERS: &[String] = &[];

impl<'a> ImportRequest<'a> {
    /// Construct a request carrying only the legacy positional fields; the
    /// structured fields (alias/modifiers/import_kind) default to absent. Used
    /// by the back-compat free-function wrappers.
    pub fn legacy(
        module_path: &'a str,
        names: &'a [String],
        default_import: Option<&'a str>,
        namespace: Option<&'a str>,
        type_only: bool,
    ) -> Self {
        ImportRequest {
            module_path,
            names,
            default_import,
            namespace,
            alias: None,
            type_only,
            modifiers: NO_MODIFIERS,
            import_kind: None,
        }
    }
}

/// A single parsed import statement.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Structured, de-overloaded representation (Stream M migration target).
    /// Populated by every parser alongside the flat fields above; readers
    /// migrate onto this incrementally behind the golden-parity gate.
    pub form: ImportForm,
}

/// A block of parsed imports from a file.
#[derive(Debug, Clone, PartialEq, Eq)]
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

pub(crate) fn import_byte_range(imports: &[ImportStatement]) -> Option<Range<usize>> {
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
// Per-language engine: the ImportSyntax trait + registry
// ---------------------------------------------------------------------------

/// Per-language import engine. One impl per supported language; [`syntax_for`]
/// maps a [`LangId`] to its `&'static dyn ImportSyntax`. This is the single
/// plug-in point that replaces the scattered `match lang` dispatch in
/// `parse_imports` / `generate_import_line_with_namespace` / `classify_group` /
/// `is_supported`. Adding a language is a new impl + one registry arm.
///
/// The existing engines are thin wrappers over the free functions they already
/// used, so routing through the trait is behavior-preserving (golden-gated).
pub trait ImportSyntax: Sync {
    /// Parse all imports from a file's already-parsed tree.
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock;

    /// Generate a single import line from a structured [`ImportRequest`].
    /// Engines read only the fields they support and ignore the rest.
    fn generate_line(&self, req: &ImportRequest) -> String;

    /// Classify a module path into stdlib / external / internal.
    fn classify_group(&self, module_path: &str) -> ImportGroup;
}

/// ES modules engine: TypeScript, TSX, JavaScript.
struct EsSyntax;
impl ImportSyntax for EsSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_ts_imports(source, tree)
    }
    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_ts_import_line(
            req.module_path,
            req.names,
            req.default_import,
            req.namespace,
            req.type_only,
        )
    }
    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_ts(module_path)
    }
}

struct PythonSyntax;
impl ImportSyntax for PythonSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_py_imports(source, tree)
    }
    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_py_import_line(req.module_path, req.names, req.default_import)
    }
    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_py(module_path)
    }
}

struct RustSyntax;
impl ImportSyntax for RustSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_rs_imports(source, tree)
    }
    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_rs_import_line(req.module_path, req.names, req.type_only)
    }
    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_rs(module_path)
    }
}

struct GoSyntax;
impl ImportSyntax for GoSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_go_imports(source, tree)
    }
    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_go_import_line(req.module_path, req.default_import, false)
    }
    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_go(module_path)
    }
}

/// Solidity import engine. Supports named / namespace / whole-file-alias /
/// side-effect forms.
struct SoliditySyntax;
impl ImportSyntax for SoliditySyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_solidity_imports(source, tree)
    }
    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_solidity_import_line(req)
    }
    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_solidity(module_path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VueScriptRangeError {
    MissingScript,
    MultipleScripts,
}

impl VueScriptRangeError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            VueScriptRangeError::MissingScript => "missing_vue_script",
            VueScriptRangeError::MultipleScripts => "ambiguous_vue_script",
        }
    }

    pub(crate) fn message(self, command: &str) -> String {
        match self {
            VueScriptRangeError::MissingScript => format!(
                "{command}: Vue import management requires exactly one <script> block; found none"
            ),
            VueScriptRangeError::MultipleScripts => format!(
                "{command}: Vue import management requires exactly one <script> block; found multiple"
            ),
        }
    }
}

/// Locate the byte range of the single `<script>` block's inner content in a
/// Vue Single-File Component. tree-sitter-vue exposes the script body as a
/// single `raw_text` node; this returns `(start, end)` of that node, or — for an
/// empty `<script></script>` with no `raw_text` child — a zero-width range right
/// after the start tag. Multiple scripts are ambiguous for byte-level edits and
/// no-script SFCs have no safe insertion region, so callers should surface the
/// returned error instead of silently editing byte 0 or the first script.
pub(crate) fn vue_single_script_content_range(
    tree: &Tree,
) -> Result<(usize, usize), VueScriptRangeError> {
    let root = tree.root_node();
    let mut ranges = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "script_element" {
            ranges.push(vue_script_element_content_range(&child));
        }
    }

    match ranges.len() {
        0 => Err(VueScriptRangeError::MissingScript),
        1 => Ok(ranges[0]),
        _ => Err(VueScriptRangeError::MultipleScripts),
    }
}

/// Back-compat convenience wrapper for callers that only need the safe single
/// script range and intentionally treat missing/ambiguous scripts as absent.
pub(crate) fn vue_script_content_range(tree: &Tree) -> Option<(usize, usize)> {
    vue_single_script_content_range(tree).ok()
}

fn vue_script_element_content_range(child: &Node) -> (usize, usize) {
    let mut inner = child.walk();
    for sub in child.named_children(&mut inner) {
        if sub.kind() == "raw_text" {
            return (sub.start_byte(), sub.end_byte());
        }
    }

    // Empty `<script></script>`: insert right after the start tag.
    let mut inner2 = child.walk();
    for sub in child.named_children(&mut inner2) {
        if sub.kind() == "start_tag" {
            return (sub.end_byte(), sub.end_byte());
        }
    }

    (child.end_byte(), child.end_byte())
}

/// Parse imports from a Vue SFC `<script>` block. The script body is re-parsed
/// with the TypeScript grammar (which covers both `lang="ts"` and plain JS
/// import syntax), then every byte offset is remapped from script-relative to
/// whole-file positions so insertion, removal, and organize operate correctly.
fn parse_vue_imports(source: &str, tree: &Tree) -> ImportBlock {
    let Ok((start, end)) = vue_single_script_content_range(tree) else {
        return ImportBlock {
            imports: Vec::new(),
            byte_range: None,
        };
    };
    let inner = &source[start..end];
    let mut parser = Parser::new();
    if parser
        .set_language(&grammar_for(LangId::TypeScript))
        .is_err()
    {
        return ImportBlock {
            imports: Vec::new(),
            byte_range: None,
        };
    }
    let Some(inner_tree) = parser.parse(inner, None) else {
        return ImportBlock {
            imports: Vec::new(),
            byte_range: None,
        };
    };
    let mut block = parse_ts_imports(inner, &inner_tree);
    for imp in &mut block.imports {
        imp.byte_range = (imp.byte_range.start + start)..(imp.byte_range.end + start);
    }
    block.byte_range = block.byte_range.map(|r| (r.start + start)..(r.end + start));
    block
}

/// Vue Single-File Component import engine. The `<script>` body is exposed by
/// tree-sitter-vue as a single `raw_text` node, so we re-parse it with the
/// TypeScript grammar and remap the resulting byte offsets back to whole-file
/// positions. Generation and grouping reuse the ES (TS/JS) engine, since Vue
/// script imports are TypeScript/JavaScript.
struct VueSyntax;
impl ImportSyntax for VueSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_vue_imports(source, tree)
    }
    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_ts_import_line(
            req.module_path,
            req.names,
            req.default_import,
            req.namespace,
            req.type_only,
        )
    }
    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_ts(module_path)
    }
}

static ES_SYNTAX: EsSyntax = EsSyntax;
static PYTHON_SYNTAX: PythonSyntax = PythonSyntax;
static RUST_SYNTAX: RustSyntax = RustSyntax;
static GO_SYNTAX: GoSyntax = GoSyntax;
static SOLIDITY_SYNTAX: SoliditySyntax = SoliditySyntax;
static VUE_SYNTAX: VueSyntax = VueSyntax;

/// Map a language to its import engine, or `None` when imports are unsupported.
pub fn syntax_for(lang: LangId) -> Option<&'static dyn ImportSyntax> {
    match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => Some(&ES_SYNTAX),
        LangId::Python => Some(&PYTHON_SYNTAX),
        LangId::Rust => Some(&RUST_SYNTAX),
        LangId::Go => Some(&GO_SYNTAX),
        LangId::Solidity => Some(&SOLIDITY_SYNTAX),
        LangId::Vue => Some(&VUE_SYNTAX),
        LangId::C => Some(&c::C_SYNTAX),
        LangId::Cpp | LangId::Cuda | LangId::Metal => Some(&c::C_SYNTAX),
        LangId::Java => Some(&java::JAVA_SYNTAX),
        LangId::Kotlin => Some(&kotlin::KOTLIN_SYNTAX),
        LangId::Lua => Some(&lua::LUA_SYNTAX),
        LangId::CSharp => Some(&csharp::CSHARP_SYNTAX),
        LangId::Php => Some(&php::PHP_SYNTAX),
        LangId::Perl => Some(&perl::PERL_SYNTAX),
        LangId::Ruby => Some(&ruby::RUBY_SYNTAX),
        LangId::Scala => Some(&scala::SCALA_SYNTAX),
        LangId::Swift => Some(&swift::SWIFT_SYNTAX),
        LangId::Zig
        | LangId::Bash
        | LangId::Scss
        | LangId::Json
        | LangId::Html
        | LangId::Markdown
        | LangId::Yaml
        | LangId::Pascal
        | LangId::R
        | LangId::Groovy
        | LangId::ObjC
        | LangId::Toml => None,
    }
}

// ---------------------------------------------------------------------------
// Core API
// ---------------------------------------------------------------------------

/// Parse imports from source using the provided tree-sitter tree.
pub fn parse_imports(source: &str, tree: &Tree, lang: LangId) -> ImportBlock {
    match syntax_for(lang) {
        Some(engine) => engine.parse(source, tree),
        None => ImportBlock::empty(),
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

        // Kind must match for dedup (value imports don't dedup against type imports)
        if imp.kind != target_kind && imp.kind != ImportKind::SideEffect {
            continue;
        }

        // Default+namespace imports are one ES binding shape. A plain default
        // import must not satisfy a request for `default, * as ns`, and a
        // different namespace alias must not either.
        if let (Some(def), Some(namespace)) = (default_import, namespace_import) {
            if imp.default_import.as_deref() == Some(def)
                && imp.namespace_import.as_deref() == Some(namespace)
                && names
                    .iter()
                    .all(|n| imp.names.iter().any(|stored| specifier_matches(stored, n)))
            {
                return true;
            }
            continue;
        }

        // Namespace-only requests are satisfied by any existing same-module
        // import that already binds that namespace alias, even if it also has a
        // default binding.
        if names.is_empty()
            && default_import.is_none()
            && namespace_import.is_some()
            && imp.namespace_import.as_deref() == namespace_import
        {
            return true;
        }

        // Check default import match. This branch only handles requests that do
        // not also ask for a namespace; that combined shape is checked above.
        if let Some(def) = default_import {
            if namespace_import.is_none() && imp.default_import.as_deref() == Some(def) {
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

/// Check whether a fully structured add-import request is already present.
///
/// Legacy ES/Python/Rust/Go callers intentionally keep the historical
/// subset/dominance semantics (`import { a, b }` satisfies adding `{ a }`). The
/// newer engines carry language-specific shape in `ImportForm::Structured` (or
/// Solidity's dedicated form), where module path alone is not enough: include
/// delimiters, statement kinds, modifiers, aliases, and runtime import flavors
/// all affect the generated source. Those languages deduplicate on a canonical
/// full-form key so `#include <x>` does not block `#include "x"`, `load` does
/// not block `require`, and side-effect Solidity imports do not block aliases.
pub(crate) fn is_duplicate_import_request(
    lang: LangId,
    block: &ImportBlock,
    req: &ImportRequest<'_>,
) -> bool {
    if lang == LangId::Php
        && block
            .imports
            .iter()
            .any(|imp| php::php_import_satisfies_request(imp, req))
    {
        return true;
    }

    if lang == LangId::Python && req.names.is_empty() && req.default_import.is_none() {
        return block.imports.iter().any(|imp| {
            matches!(
                &imp.form,
                ImportForm::Python {
                    from_import: false,
                    named,
                } if named.iter().any(|specifier| {
                    specifier_imported_name(specifier) == req.module_path
                })
            )
        });
    }

    if !uses_form_aware_dedup(lang) {
        return is_duplicate_with_namespace(
            block,
            req.module_path,
            req.names,
            req.default_import,
            req.namespace,
            req.type_only,
        );
    }

    let target = request_dedup_key(lang, req);
    block
        .imports
        .iter()
        .map(|imp| statement_dedup_key(lang, imp))
        .any(|key| key == target)
}

fn uses_form_aware_dedup(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::Solidity
            | LangId::C
            | LangId::Cpp
            | LangId::Java
            | LangId::CSharp
            | LangId::Php
            | LangId::Kotlin
            | LangId::Scala
            | LangId::Swift
            | LangId::Ruby
            | LangId::Lua
            | LangId::Perl
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportDedupKey {
    module_path: String,
    kind: ImportKind,
    form: ImportForm,
}

fn statement_dedup_key(lang: LangId, imp: &ImportStatement) -> ImportDedupKey {
    canonical_dedup_key(
        lang,
        ImportDedupKey {
            module_path: imp.module_path.clone(),
            kind: imp.kind,
            form: imp.form.clone(),
        },
    )
}

fn request_dedup_key(lang: LangId, req: &ImportRequest<'_>) -> ImportDedupKey {
    let key = match lang {
        LangId::Solidity => {
            let kind = if req.names.is_empty() && req.namespace.is_none() && req.alias.is_none() {
                ImportKind::SideEffect
            } else {
                ImportKind::Value
            };
            ImportDedupKey {
                module_path: req.module_path.to_string(),
                kind,
                form: ImportForm::Solidity {
                    named: req.names.to_vec(),
                    namespace: req.namespace.map(str::to_string),
                    alias: req.alias.map(str::to_string),
                },
            }
        }
        LangId::C | LangId::Cpp => structured_dedup_key(
            req.module_path,
            ImportKind::SideEffect,
            &[],
            None,
            None,
            &[],
            Some(req.import_kind.or(req.default_import).unwrap_or("system")),
        ),
        LangId::Java => {
            let (mut module_path, modifiers) = wildcard_suffix_request(
                req.module_path,
                req.modifiers,
                req.default_import == Some("*"),
            );
            let mut names = req.names.to_vec();
            normalize_java_static_member_key(&mut module_path, &modifiers, &mut names);
            structured_dedup_key(
                &module_path,
                ImportKind::Value,
                &names,
                None,
                None,
                &modifiers,
                None,
            )
        }
        LangId::CSharp => structured_dedup_key(
            req.module_path,
            ImportKind::Value,
            &[],
            None,
            req.alias,
            req.modifiers,
            None,
        ),
        LangId::Php => structured_dedup_key(
            req.module_path,
            ImportKind::Value,
            &[],
            None,
            req.alias,
            req.modifiers,
            req.import_kind,
        ),
        LangId::Kotlin => {
            let wildcard = req.default_import == Some("*") || req.module_path.ends_with(".*");
            let (module_path, modifiers) =
                wildcard_suffix_request(req.module_path, req.modifiers, wildcard);
            let alias = req
                .alias
                .or(req.default_import.filter(|value| *value != "*"));
            structured_dedup_key(
                &module_path,
                ImportKind::Value,
                &[],
                None,
                alias,
                &modifiers,
                None,
            )
        }
        LangId::Scala => scala_request_dedup_key(req),
        LangId::Swift => structured_dedup_key(
            req.module_path,
            ImportKind::Value,
            &[],
            None,
            None,
            req.modifiers,
            req.import_kind,
        ),
        LangId::Ruby => {
            let mut modifiers = req.modifiers.to_vec();
            if !modifiers
                .iter()
                .any(|modifier| modifier == "quote:single" || modifier == "quote:double")
            {
                modifiers.push("quote:single".to_string());
            }
            structured_dedup_key(
                req.module_path,
                ImportKind::SideEffect,
                &[],
                None,
                None,
                &modifiers,
                Some(req.import_kind.unwrap_or("require")),
            )
        }
        LangId::Lua => {
            let alias = req.default_import.or(req.alias);
            let kind = if alias.is_some() {
                ImportKind::Value
            } else {
                ImportKind::SideEffect
            };
            structured_dedup_key(req.module_path, kind, &[], None, alias, req.modifiers, None)
        }
        LangId::Perl => structured_dedup_key(
            req.module_path,
            ImportKind::SideEffect,
            &[],
            None,
            None,
            req.modifiers,
            Some(req.import_kind.unwrap_or("use")),
        ),
        _ => structured_dedup_key(
            req.module_path,
            if req.type_only {
                ImportKind::Type
            } else {
                ImportKind::Value
            },
            req.names,
            req.namespace,
            req.alias,
            req.modifiers,
            req.import_kind,
        ),
    };

    canonical_dedup_key(lang, key)
}

fn structured_dedup_key(
    module_path: &str,
    kind: ImportKind,
    named: &[String],
    namespace: Option<&str>,
    alias: Option<&str>,
    modifiers: &[String],
    import_kind: Option<&str>,
) -> ImportDedupKey {
    ImportDedupKey {
        module_path: module_path.to_string(),
        kind,
        form: ImportForm::Structured {
            named: named.to_vec(),
            namespace: namespace.map(str::to_string),
            alias: alias.map(str::to_string),
            modifiers: modifiers.to_vec(),
            import_kind: import_kind.map(str::to_string),
        },
    }
}

fn wildcard_suffix_request(
    module_path: &str,
    modifiers: &[String],
    wildcard: bool,
) -> (String, Vec<String>) {
    let stripped = module_path.strip_suffix(".*").unwrap_or(module_path);
    let mut modifiers = modifiers.to_vec();
    if (wildcard || stripped.len() != module_path.len())
        && !modifiers.iter().any(|modifier| modifier == "wildcard")
    {
        modifiers.push("wildcard".to_string());
    }
    (stripped.to_string(), modifiers)
}

fn normalize_java_static_member_key(
    module_path: &mut String,
    modifiers: &[String],
    names: &mut Vec<String>,
) {
    let is_static = modifiers.iter().any(|modifier| modifier == "static");
    let is_wildcard = modifiers.iter().any(|modifier| modifier == "wildcard");
    if !is_static || is_wildcard || !names.is_empty() {
        return;
    }

    if let Some((prefix, member)) = module_path.rsplit_once('.') {
        if !prefix.is_empty() && !member.is_empty() {
            names.push(member.to_string());
            *module_path = prefix.to_string();
        }
    }
}

fn scala_request_dedup_key(req: &ImportRequest<'_>) -> ImportDedupKey {
    let mut module_path = req.module_path.to_string();
    let mut names: Vec<String> = req
        .names
        .iter()
        .map(|name| normalize_scala_selector_for_dedup(name))
        .collect();
    let mut identity_modifiers: Vec<String> = req
        .modifiers
        .iter()
        .filter(|modifier| modifier.as_str() != "scala2")
        .cloned()
        .collect();
    let mut import_kind = req.import_kind.map(str::to_string);

    if req.default_import == Some("given") || module_path.ends_with(".given") {
        import_kind.get_or_insert_with(|| "given".to_string());
        if let Some(stripped) = module_path.strip_suffix(".given") {
            module_path = stripped.to_string();
        }
    }

    if matches!(req.default_import, Some("*") | Some("_"))
        || matches!(req.namespace, Some("*") | Some("_"))
        || module_path.ends_with(".*")
        || module_path.ends_with("._")
    {
        if !identity_modifiers
            .iter()
            .any(|modifier| modifier == "wildcard")
        {
            identity_modifiers.push("wildcard".to_string());
        }
        module_path = module_path
            .strip_suffix(".*")
            .or_else(|| module_path.strip_suffix("._"))
            .unwrap_or(&module_path)
            .to_string();
    }

    if names.is_empty() {
        if let Some(alias) = req.alias.filter(|alias| !alias.is_empty()) {
            if let Some((prefix, leaf)) = module_path.rsplit_once('.') {
                names.push(format!("{leaf} as {alias}"));
                module_path = prefix.to_string();
            }
        }
    }

    structured_dedup_key(
        &module_path,
        ImportKind::Value,
        &names,
        None,
        None,
        &identity_modifiers,
        import_kind.as_deref(),
    )
}

fn normalize_scala_selector_for_dedup(name: &str) -> String {
    let trimmed = name.trim();
    if let Some((from, to)) = trimmed.split_once("=>") {
        format!("{} as {}", from.trim(), to.trim())
    } else {
        trimmed.to_string()
    }
}

fn canonical_dedup_key(lang: LangId, mut key: ImportDedupKey) -> ImportDedupKey {
    match &mut key.form {
        ImportForm::Structured { named, .. } | ImportForm::Solidity { named, .. } => {
            sort_named_specifiers(named);
        }
        ImportForm::Es { named, .. } | ImportForm::Python { named, .. } => {
            sort_named_specifiers(named);
        }
        ImportForm::RustUse { named, .. } => {
            sort_named_specifiers(named);
        }
        ImportForm::Go { .. } | ImportForm::Php { .. } => {}
    }

    if matches!(lang, LangId::Java | LangId::Kotlin) {
        if let Some(stripped) = key.module_path.strip_suffix(".*") {
            key.module_path = stripped.to_string();
        }
        if matches!(lang, LangId::Java) {
            if let ImportForm::Structured {
                named, modifiers, ..
            } = &mut key.form
            {
                normalize_java_static_member_key(&mut key.module_path, modifiers, named);
            }
        }
    } else if matches!(lang, LangId::Scala) {
        key.module_path = key
            .module_path
            .strip_suffix(".given")
            .or_else(|| key.module_path.strip_suffix(".*"))
            .or_else(|| key.module_path.strip_suffix("._"))
            .unwrap_or(&key.module_path)
            .to_string();
    }

    key
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

/// Generate a single import line from a structured [`ImportRequest`]. The full
/// entry point — engines read the fields they support; unsupported languages
/// yield an empty string.
pub fn generate_import(lang: LangId, req: &ImportRequest) -> String {
    match syntax_for(lang) {
        Some(engine) => engine.generate_line(req),
        None => String::new(),
    }
}

/// Generate an import line for the given language. Back-compat wrapper over
/// [`generate_import`] for callers that pass only the legacy positional fields.
pub fn generate_import_line(
    lang: LangId,
    module_path: &str,
    names: &[String],
    default_import: Option<&str>,
    type_only: bool,
) -> String {
    generate_import(
        lang,
        &ImportRequest::legacy(module_path, names, default_import, None, type_only),
    )
}

/// Generate an import line including namespace imports
/// (`import * as ns from 'mod'`). Back-compat wrapper over [`generate_import`].
pub fn generate_import_line_with_namespace(
    lang: LangId,
    module_path: &str,
    names: &[String],
    default_import: Option<&str>,
    namespace_import: Option<&str>,
    type_only: bool,
) -> String {
    generate_import_line_with_namespace_and_attribute_clause(
        lang,
        module_path,
        names,
        default_import,
        namespace_import,
        type_only,
        None,
    )
}

/// Generate a line while retaining an existing ES import-attribute clause.
/// Other language engines ignore the clause because it has no meaning there.
pub(crate) fn generate_import_line_with_namespace_and_attribute_clause(
    lang: LangId,
    module_path: &str,
    names: &[String],
    default_import: Option<&str>,
    namespace_import: Option<&str>,
    type_only: bool,
    attribute_clause: Option<&str>,
) -> String {
    if matches!(
        lang,
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript | LangId::Vue
    ) {
        return generate_ts_import_line_with_attribute_clause(
            module_path,
            names,
            default_import,
            namespace_import,
            type_only,
            attribute_clause,
        );
    }

    generate_import(
        lang,
        &ImportRequest::legacy(
            module_path,
            names,
            default_import,
            namespace_import,
            type_only,
        ),
    )
}

/// Check if the given language is supported by the import engine.
pub fn is_supported(lang: LangId) -> bool {
    syntax_for(lang).is_some()
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
    match syntax_for(lang) {
        Some(engine) => engine.classify_group(module_path),
        // Unsupported languages have no grouping policy; External is the
        // historical neutral default.
        None => ImportGroup::External,
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

    let attribute_clause = extract_es_import_attribute_clause(source, node);
    let attribute_type = attribute_clause
        .as_deref()
        .and_then(parse_es_import_attribute_type);
    let form = ImportForm::Es {
        default_import: default_import.clone(),
        namespace_import: namespace_import.clone(),
        named: names.clone(),
        type_only: is_type_only,
        side_effect: matches!(kind, ImportKind::SideEffect),
        attribute_clause,
        attribute_type,
    };

    Some(ImportStatement {
        module_path,
        names,
        default_import,
        namespace_import,
        kind,
        group,
        byte_range,
        raw_text,
        form,
    })
}

/// Capture the exact source spelling because `with` and legacy `assert`
/// clauses have the same semantics but must not be normalized during a rewrite.
fn extract_es_import_attribute_clause(source: &str, node: &Node) -> Option<String> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    let module_end = loop {
        let child = cursor.node();
        if child.kind() == "string" {
            break child.end_byte();
        }
        if !cursor.goto_next_sibling() {
            return None;
        }
    };

    let tail = source[module_end..node.end_byte()].trim();
    let clause = tail.strip_suffix(';').unwrap_or(tail).trim();
    (clause.starts_with("with") || clause.starts_with("assert")).then(|| clause.to_string())
}

/// Return the semantic import-attribute clause attached to an ES import.
pub(crate) fn es_import_attribute_clause(imp: &ImportStatement) -> Option<&str> {
    match &imp.form {
        ImportForm::Es {
            attribute_clause, ..
        } => attribute_clause.as_deref(),
        _ => None,
    }
}

/// Return the semantic value of an ES import's `type` attribute.
pub(crate) fn es_import_attribute_type(imp: &ImportStatement) -> Option<&str> {
    match &imp.form {
        ImportForm::Es { attribute_type, .. } => attribute_type.as_deref(),
        _ => None,
    }
}

/// Decode the module type through one canonical parse. Legacy `assert` and current
/// `with` clauses carry the same attribute semantics even though their spelling
/// must remain unchanged when the original import is rewritten.
fn parse_es_import_attribute_type(clause: &str) -> Option<String> {
    let body = ["with", "assert"].into_iter().find_map(|keyword| {
        let rest = clause.strip_prefix(keyword)?;
        rest.chars()
            .next()
            .is_some_and(|ch| ch.is_whitespace() || ch == '{' || ch == '/')
            .then_some(rest)
    })?;
    let synthetic = format!("import 'module' with{body};");
    let mut parser = Parser::new();
    parser.set_language(&grammar_for(LangId::TypeScript)).ok()?;
    let tree = parser.parse(&synthetic, None)?;
    (!tree.root_node().has_error())
        .then(|| find_type_attribute(&synthetic, tree.root_node()))
        .flatten()
}

fn find_type_attribute(source: &str, node: Node<'_>) -> Option<String> {
    if node.kind() == "pair" {
        let key = node
            .child_by_field_name("key")
            .or_else(|| node.named_child(0))?;
        let value = node
            .child_by_field_name("value")
            .or_else(|| node.named_child(1))?;
        if decode_js_attribute_atom(source.get(key.byte_range())?)?.as_str() == "type" {
            return decode_js_attribute_atom(source.get(value.byte_range())?);
        }
    }

    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .find_map(|child| find_type_attribute(source, child));
    found
}

fn decode_js_attribute_atom(text: &str) -> Option<String> {
    let text = text.trim();
    let quote = text.as_bytes().first().copied();
    if !matches!(quote, Some(b'\'') | Some(b'"')) {
        return Some(text.to_string());
    }
    if text.as_bytes().last().copied() != quote || text.len() < 2 {
        return None;
    }

    let mut chars = text[1..text.len() - 1].chars();
    let mut decoded = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            decoded.push(ch);
            continue;
        }

        match chars.next()? {
            '\n' => {}
            '\r' => {
                if chars.clone().next() == Some('\n') {
                    chars.next();
                }
            }
            'b' => decoded.push('\u{0008}'),
            'f' => decoded.push('\u{000c}'),
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            'v' => decoded.push('\u{000b}'),
            '0' => decoded.push('\0'),
            'x' => decoded.push(decode_hex_escape(&mut chars, 2)?),
            'u' => {
                let digits = if chars.clone().next() == Some('{') {
                    chars.next();
                    let mut digits = String::new();
                    loop {
                        let ch = chars.next()?;
                        if ch == '}' {
                            break;
                        }
                        digits.push(ch);
                    }
                    digits
                } else {
                    chars.by_ref().take(4).collect()
                };
                decoded.push(char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?);
            }
            escaped => decoded.push(escaped),
        }
    }
    Some(decoded)
}

fn decode_hex_escape(chars: &mut impl Iterator<Item = char>, count: usize) -> Option<char> {
    let digits: String = chars.take(count).collect();
    (digits.len() == count)
        .then(|| u32::from_str_radix(&digits, 16).ok())
        .flatten()
        .and_then(char::from_u32)
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
    generate_ts_import_line_with_attribute_clause(
        module_path,
        names,
        default_import,
        namespace_import,
        type_only,
        None,
    )
}

fn generate_ts_import_line_with_attribute_clause(
    module_path: &str,
    names: &[String],
    default_import: Option<&str>,
    namespace_import: Option<&str>,
    type_only: bool,
    attribute_clause: Option<&str>,
) -> String {
    let line = generate_ts_import_line_base(
        module_path,
        names,
        default_import,
        namespace_import,
        type_only,
    );
    let Some(attribute_clause) = attribute_clause else {
        return line;
    };

    let line = line.strip_suffix(';').unwrap_or(&line);
    format!("{line} {};", attribute_clause.trim())
}

fn generate_ts_import_line_base(
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

    // Keep every module specifier verbatim. A single Python import statement can
    // bind several modules, and aliases must remain in the model so removing or
    // merging one binding cannot silently delete its siblings.
    let mut specifiers = Vec::new();
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            let child = c.node();
            if matches!(child.kind(), "dotted_name" | "aliased_import") {
                specifiers.push(source[child.byte_range()].trim().to_string());
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
    let module_path = specifiers
        .first()
        .map(|specifier| specifier_imported_name(specifier).to_string())?;

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
        form: ImportForm::Python {
            from_import: false,
            named: specifiers,
        },
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
                "aliased_import" => {
                    // Store the full `source as local` spelling so later edits
                    // can match either name and reproduce the alias unchanged.
                    names.push(source[child.byte_range()].trim().to_string());
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
        names: names.clone(),
        default_import: None,
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
        form: ImportForm::Python {
            from_import: true,
            named: names,
        },
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

    // Capture the EXACT visibility modifier text (`pub`, `pub(crate)`,
    // `pub(super)`, `pub(in path)`) so organize re-emits it faithfully instead
    // of widening every restricted visibility to a bare `pub`.
    let mut visibility: Option<String> = None;
    let mut use_path = String::new();
    let mut names = Vec::new();

    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            let child = c.node();
            match child.kind() {
                "visibility_modifier" => {
                    visibility = Some(source[child.byte_range()].to_string());
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
        names: names.clone(),
        // `default_import` carries the visibility text for the Rust engine
        // (e.g. "pub", "pub(crate)"). organize re-emits it verbatim.
        default_import: visibility.clone(),
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
        form: ImportForm::RustUse {
            visibility,
            named: names,
        },
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
        let mut sorted_names = names.to_vec();
        sort_named_specifiers(&mut sorted_names);
        format!("use {module_path}::{{{}}};", sorted_names.join(", "))
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
        default_import: alias.clone(),
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
        form: ImportForm::Go { alias },
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
        if node.kind() == "import_declaration" && go_import_declaration_is_grouped(&node) {
            return Some(node.byte_range());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    None
}

pub fn go_import_declarations_range(_source: &str, tree: &Tree) -> Option<Range<usize>> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut range: Option<Range<usize>> = None;
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "import_declaration" {
            let node_range = node.byte_range();
            range = Some(match range {
                Some(existing) => {
                    existing.start.min(node_range.start)..existing.end.max(node_range.end)
                }
                None => node_range,
            });
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    range
}

pub fn go_offset_is_in_grouped_import(_source: &str, tree: &Tree, offset: usize) -> bool {
    let root = tree.root_node();
    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return false;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "import_declaration"
            && node.start_byte() < offset
            && offset < node.end_byte()
            && go_import_declaration_is_grouped(&node)
        {
            return true;
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    false
}

fn go_import_declaration_is_grouped(node: &Node) -> bool {
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            if c.node().kind() == "import_spec_list" {
                return true;
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Solidity implementation
// ---------------------------------------------------------------------------

/// Classify a Solidity import path: relative (`./`, `../`) is internal,
/// everything else (remappings, `@scope/...`, bare) is external. No stdlib.
pub fn classify_group_solidity(module_path: &str) -> ImportGroup {
    if module_path.starts_with('.') {
        ImportGroup::Internal
    } else {
        ImportGroup::External
    }
}

fn parse_solidity_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();
    let mut cursor = root.walk();
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            if node.kind() == "import_directive" {
                if let Some(imp) = parse_solidity_import_directive(source, &node) {
                    imports.push(imp);
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    let byte_range = import_byte_range(&imports);
    ImportBlock {
        imports,
        byte_range,
    }
}

fn strip_one_matching_solidity_quote_pair(value: &str) -> &str {
    if value.len() < 2 {
        return value;
    }

    let bytes = value.as_bytes();
    let quote = bytes[0];
    if matches!(quote, b'\'' | b'"') && bytes.last() == Some(&quote) {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Parse one `import_directive`. The Solidity grammar emits a flat token
/// sequence (verified by grammar fixture test), so the four forms are
/// distinguished by the presence of `{` (named), `*` (namespace), a trailing
/// `as` (whole-file alias), or none (side-effect).
fn parse_solidity_import_directive(source: &str, node: &Node) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();

    let mut children: Vec<(String, String)> = Vec::new();
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            let ch = c.node();
            children.push((ch.kind().to_string(), source[ch.byte_range()].to_string()));
            if !c.goto_next_sibling() {
                break;
            }
        }
    }

    // Every form carries exactly one string literal: the imported file path.
    let module_path = children
        .iter()
        .find(|(k, _)| k == "string")
        .map(|(_, text)| strip_one_matching_solidity_quote_pair(text).to_string())?;
    if module_path.is_empty() {
        return None;
    }

    let has_brace = children.iter().any(|(k, _)| k == "{");
    let has_star = children.iter().any(|(k, _)| k == "*");

    let mut named: Vec<String> = Vec::new();
    let mut namespace: Option<String> = None;
    let mut alias: Option<String> = None;

    if has_brace {
        named = parse_solidity_named_specifiers(&children);
    } else if has_star {
        namespace = solidity_identifier_after_as(&children);
    } else {
        // No `{`, no `*`: a trailing `as IDENT` is a whole-file alias;
        // otherwise a bare side-effect import.
        alias = solidity_identifier_after_as(&children);
    }

    let kind = if named.is_empty() && namespace.is_none() && alias.is_none() {
        ImportKind::SideEffect
    } else {
        ImportKind::Value
    };
    let group = classify_group_solidity(&module_path);

    Some(ImportStatement {
        module_path,
        names: named.clone(),
        default_import: None,
        // Namespace maps to the flat slot so existing readers (dedup) see it;
        // the whole-file alias has no flat slot and lives only in `form`.
        namespace_import: namespace.clone(),
        kind,
        group,
        byte_range,
        raw_text,
        form: ImportForm::Solidity {
            named,
            namespace,
            alias,
        },
    })
}

/// Return the `identifier` token immediately following the first `as`.
fn solidity_identifier_after_as(children: &[(String, String)]) -> Option<String> {
    let as_pos = children.iter().position(|(k, _)| k == "as")?;
    children[as_pos + 1..]
        .iter()
        .find(|(k, _)| k == "identifier")
        .map(|(_, t)| t.clone())
}

/// Collect named specifiers between `{` and `}` into verbatim strings,
/// combining `A as B` into `"A as B"` to match the ES specifier convention.
fn parse_solidity_named_specifiers(children: &[(String, String)]) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_braces = false;
    let mut current: Option<String> = None;
    let mut expect_alias = false;
    for (k, t) in children {
        match k.as_str() {
            "{" => in_braces = true,
            "}" => {
                if let Some(n) = current.take() {
                    names.push(n);
                }
                in_braces = false;
            }
            _ if !in_braces => {}
            "identifier" => {
                if expect_alias {
                    if let Some(n) = current.take() {
                        names.push(format!("{n} as {t}"));
                    }
                    expect_alias = false;
                } else {
                    if let Some(n) = current.take() {
                        names.push(n);
                    }
                    current = Some(t.clone());
                }
            }
            "as" => expect_alias = true,
            "," => {
                if let Some(n) = current.take() {
                    names.push(n);
                }
                expect_alias = false;
            }
            _ => {}
        }
    }
    names
}

/// Generate a Solidity import line in the appropriate form.
fn generate_solidity_import_line(req: &ImportRequest) -> String {
    if !req.names.is_empty() {
        format!(
            "import {{ {} }} from \"{}\";",
            req.names.join(", "),
            req.module_path
        )
    } else if let Some(ns) = req.namespace {
        format!("import * as {} from \"{}\";", ns, req.module_path)
    } else if let Some(al) = req.alias {
        format!("import \"{}\" as {};", req.module_path, al)
    } else {
        format!("import \"{}\";", req.module_path)
    }
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

    // --- ImportForm field-mapping contract (Stream M) ---
    //
    // These assert the additive `form` field faithfully mirrors the flat
    // fields each parser populates. They are the executable field-mapping
    // contract from the migration plan: when a reader is moved off a flat
    // field onto `form`, these guarantee no information was lost in the
    // de-overloading (Rust `pub`, Go alias) or restructuring.

    #[test]
    fn form_es_mirrors_flat_fields() {
        let (_, block) = parse_ts(
            "import Default, { a, b as c } from \"ext\";\nimport type { T } from \"./t\";\nimport \"./side\";\nimport * as ns from \"nspkg\";\n",
        );
        // import Default, { a, b as c } from "ext"
        match &block.imports[0].form {
            ImportForm::Es {
                default_import,
                namespace_import,
                named,
                type_only,
                side_effect,
                attribute_clause,
                attribute_type,
            } => {
                assert_eq!(default_import.as_deref(), Some("Default"));
                assert_eq!(namespace_import, &None);
                assert_eq!(named, &block.imports[0].names);
                assert!(!type_only);
                assert!(!side_effect);
                assert_eq!(attribute_clause, &None);
                assert_eq!(attribute_type, &None);
            }
            other => panic!("expected Es, got {other:?}"),
        }
        // import type { T } from "./t"
        match &block.imports[1].form {
            ImportForm::Es {
                type_only, named, ..
            } => {
                assert!(type_only);
                assert_eq!(named, &block.imports[1].names);
            }
            other => panic!("expected Es type-only, got {other:?}"),
        }
        // import "./side"
        match &block.imports[2].form {
            ImportForm::Es { side_effect, .. } => assert!(side_effect),
            other => panic!("expected Es side-effect, got {other:?}"),
        }
        // import * as ns from "nspkg"
        match &block.imports[3].form {
            ImportForm::Es {
                namespace_import, ..
            } => assert_eq!(namespace_import.as_deref(), Some("ns")),
            other => panic!("expected Es namespace, got {other:?}"),
        }
    }

    #[test]
    fn form_python_mirrors_flat_fields() {
        let (_, block) = parse_py("import os\nfrom sys import argv, path\n");
        match &block.imports[0].form {
            ImportForm::Python { from_import, named } => {
                assert!(!from_import, "`import os` is not a from-import");
                assert_eq!(named, &["os"]);
            }
            other => panic!("expected Python import, got {other:?}"),
        }
        match &block.imports[1].form {
            ImportForm::Python { from_import, named } => {
                assert!(from_import, "`from sys import ...` is a from-import");
                assert_eq!(named, &block.imports[1].names);
            }
            other => panic!("expected Python from-import, got {other:?}"),
        }
    }

    #[test]
    fn form_rust_de_overloads_pub_from_default_import() {
        let (_, block) = parse_rust("pub use crate::a::Exported;\nuse std::fmt::Debug;\n");
        // pub use -> visibility=Some("pub"); flat field still carries the "pub" hack.
        match &block.imports[0].form {
            ImportForm::RustUse { visibility, named } => {
                assert_eq!(visibility.as_deref(), Some("pub"));
                assert_eq!(named, &block.imports[0].names);
            }
            other => panic!("expected RustUse, got {other:?}"),
        }
        assert_eq!(
            block.imports[0].default_import.as_deref(),
            Some("pub"),
            "flat field unchanged during additive migration"
        );
        // plain use -> visibility=None
        match &block.imports[1].form {
            ImportForm::RustUse { visibility, .. } => assert_eq!(visibility, &None),
            other => panic!("expected RustUse, got {other:?}"),
        }
        assert_eq!(block.imports[1].default_import, None);
    }

    #[test]
    fn form_go_de_overloads_alias_from_default_import() {
        // The current Go parser only captures blank (`_`) / dot bindings as the
        // alias (regular package aliases like `al "path"` are a pre-existing
        // parser gap — not extracted into `default_import` today). The contract
        // locked here is that `form.alias` mirrors `default_import` exactly,
        // whatever the parser captures, so the de-overload is information-faithful.
        let (_, block) =
            parse_go("package main\n\nimport (\n\t_ \"github.com/x/y\"\n\t\"fmt\"\n)\n");
        let blank = block
            .imports
            .iter()
            .find(|i| i.module_path == "github.com/x/y")
            .expect("blank import parsed");
        match &blank.form {
            ImportForm::Go { alias } => assert_eq!(alias.as_deref(), Some("_")),
            other => panic!("expected Go blank-aliased, got {other:?}"),
        }
        assert_eq!(
            blank.default_import.as_deref(),
            Some("_"),
            "form.alias mirrors the flat default_import field exactly"
        );
        let plain = block
            .imports
            .iter()
            .find(|i| i.module_path == "fmt")
            .expect("plain import parsed");
        match &plain.form {
            ImportForm::Go { alias } => assert_eq!(alias, &None),
            other => panic!("expected Go plain, got {other:?}"),
        }
        assert_eq!(plain.default_import, None);
    }

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

    fn parse_vue(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Vue);
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Vue);
        (tree, block)
    }

    /// Locks the tree-sitter-vue node kinds the Vue engine depends on: the
    /// `<script>` body is exposed as a single `raw_text` node inside a
    /// `script_element`. If a grammar bump changes this, the engine breaks
    /// silently, so assert it here.
    #[test]
    fn vue_grammar_node_kinds_are_stable() {
        let src = "<template>\n  <div />\n</template>\n\n<script setup lang=\"ts\">\nimport { ref } from 'vue'\n</script>\n";
        let grammar = grammar_for(LangId::Vue);
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();
        let mut cursor = root.walk();
        let script = root
            .named_children(&mut cursor)
            .find(|n| n.kind() == "script_element")
            .expect("expected a script_element node");
        let mut inner = script.walk();
        assert!(
            script
                .named_children(&mut inner)
                .any(|n| n.kind() == "raw_text"),
            "expected script body exposed as raw_text"
        );
    }

    #[test]
    fn vue_parses_script_imports_with_whole_file_offsets() {
        let src = "<template>\n  <div />\n</template>\n\n<script setup lang=\"ts\">\nimport { ref } from 'vue'\nimport Foo from './Foo.vue'\nconst x = ref(0)\n</script>\n";
        let (_tree, block) = parse_vue(src);
        assert_eq!(block.imports.len(), 2, "should find both script imports");
        // Byte ranges must be whole-file (inside the <script> block), not
        // script-relative — verify the raw slice round-trips.
        for imp in &block.imports {
            assert_eq!(&src[imp.byte_range.clone()], imp.raw_text);
            assert!(
                imp.byte_range.start > src.find("<script").unwrap(),
                "import offset must fall inside the script block"
            );
        }
        assert_eq!(block.imports[0].module_path, "vue");
        assert_eq!(block.imports[1].module_path, "./Foo.vue");
    }

    #[test]
    fn vue_without_script_block_has_no_imports() {
        let src = "<template>\n  <div />\n</template>\n\n<style>.x{}</style>\n";
        let (_tree, block) = parse_vue(src);
        assert!(block.imports.is_empty());
        assert!(block.byte_range.is_none());
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
    fn parse_es_import_attributes_preserve_standard_and_legacy_spellings() {
        let source = r#"import data from './data.json' with { type: 'json' };
import legacy from './legacy.json' assert { type: 'json' };
import escaped from './escaped.json' with { "t\u0079pe": "j\u0073on" };
"#;
        let (_, block) = parse_js(source);

        assert_eq!(
            es_import_attribute_clause(&block.imports[0]),
            Some("with { type: 'json' }")
        );
        assert_eq!(
            es_import_attribute_clause(&block.imports[1]),
            Some("assert { type: 'json' }")
        );
        assert_eq!(es_import_attribute_type(&block.imports[0]), Some("json"));
        assert_eq!(es_import_attribute_type(&block.imports[1]), Some("json"));
        assert_eq!(es_import_attribute_type(&block.imports[2]), Some("json"));
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
    fn parse_py_import_statement_preserves_aliases_and_siblings() {
        let source = "import alpha, beta as local_beta\n";
        let (_, block) = parse_py(source);
        let imp = &block.imports[0];
        assert_eq!(imp.module_path, "alpha");
        assert!(imp.names.is_empty());
        match &imp.form {
            ImportForm::Python { from_import, named } => {
                assert!(!from_import);
                assert_eq!(named, &["alpha", "beta as local_beta"]);
                assert!(specifier_matches(&named[1], "beta"));
                assert!(specifier_matches(&named[1], "local_beta"));
            }
            other => panic!("expected Python import, got {other:?}"),
        }
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
    fn parse_py_from_import_preserves_aliases() {
        let source = "from module import alpha, beta as local_beta\n";
        let (_, block) = parse_py(source);
        let imp = &block.imports[0];
        assert_eq!(imp.names, ["alpha", "beta as local_beta"]);
        assert!(specifier_matches(&imp.names[1], "beta"));
        assert!(specifier_matches(&imp.names[1], "local_beta"));
        assert_eq!(
            imp.form,
            ImportForm::Python {
                from_import: true,
                named: vec!["alpha".to_string(), "beta as local_beta".to_string()],
            }
        );
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

    // --- Solidity ---

    fn parse_solidity(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Solidity);
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Solidity);
        (tree, block)
    }

    /// Grammar fixture (council #6): lock the tree-sitter-solidity node kinds the
    /// parser depends on. If the grammar updates and renames these, this test
    /// fails loudly before the parser silently mis-parses.
    #[test]
    fn solidity_grammar_node_kinds_are_stable() {
        let grammar = grammar_for(LangId::Solidity);
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let src = "import { Foo, Bar as Baz } from \"./A.sol\";\nimport * as N from \"./B.sol\";\nimport \"./C.sol\" as C;\nimport \"./D.sol\";\n";
        let tree = parser.parse(src, None).unwrap();
        let mut kinds: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        fn walk(node: tree_sitter::Node, kinds: &mut std::collections::BTreeSet<String>) {
            kinds.insert(node.kind().to_string());
            let mut c = node.walk();
            if c.goto_first_child() {
                loop {
                    walk(c.node(), kinds);
                    if !c.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        walk(tree.root_node(), &mut kinds);
        for required in [
            "import_directive",
            "string",
            "identifier",
            "as",
            "from",
            "*",
            "{",
            "}",
        ] {
            assert!(
                kinds.contains(required),
                "solidity grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn solidity_string_path_strips_one_matching_quote_pair() {
        assert_eq!(
            strip_one_matching_solidity_quote_pair("\"./A.sol\""),
            "./A.sol"
        );
        assert_eq!(
            strip_one_matching_solidity_quote_pair("'./A.sol'"),
            "./A.sol"
        );
        assert_eq!(
            strip_one_matching_solidity_quote_pair("\"./a'b.sol\""),
            "./a'b.sol"
        );
        assert_eq!(
            strip_one_matching_solidity_quote_pair("\"\"./A.sol\"\""),
            "\"./A.sol\""
        );
        assert_eq!(
            strip_one_matching_solidity_quote_pair("'./A.sol\""),
            "'./A.sol\""
        );
        assert_eq!(
            strip_one_matching_solidity_quote_pair("\"./A.sol"),
            "\"./A.sol"
        );
        assert_eq!(strip_one_matching_solidity_quote_pair("\""), "\"");
    }

    #[test]
    fn parse_solidity_all_four_forms() {
        let (_, block) = parse_solidity(
            "import \"./A.sol\";\nimport \"./B.sol\" as B;\nimport * as C from \"./C.sol\";\nimport { Foo, Bar as Baz } from \"./D.sol\";\nimport './E.sol';\nimport './F.sol' as F;\nimport * as G from './G.sol';\nimport { Quux, Corge as Grault } from './H.sol';\n",
        );
        assert_eq!(block.imports.len(), 8);

        // side-effect
        assert_eq!(block.imports[0].module_path, "./A.sol");
        assert_eq!(block.imports[0].kind, ImportKind::SideEffect);
        assert_eq!(
            block.imports[0].form,
            ImportForm::Solidity {
                named: vec![],
                namespace: None,
                alias: None
            }
        );

        // whole-file alias
        assert_eq!(
            block.imports[1].form,
            ImportForm::Solidity {
                named: vec![],
                namespace: None,
                alias: Some("B".to_string())
            }
        );

        // namespace
        match &block.imports[2].form {
            ImportForm::Solidity { namespace, .. } => assert_eq!(namespace.as_deref(), Some("C")),
            other => panic!("expected Solidity namespace, got {other:?}"),
        }
        assert_eq!(block.imports[2].namespace_import.as_deref(), Some("C"));

        // named with alias (verbatim specifier convention)
        match &block.imports[3].form {
            ImportForm::Solidity { named, .. } => {
                assert_eq!(named, &vec!["Foo".to_string(), "Bar as Baz".to_string()]);
            }
            other => panic!("expected Solidity named, got {other:?}"),
        }
        assert_eq!(
            block.imports[3].names,
            vec!["Foo".to_string(), "Bar as Baz".to_string()]
        );

        assert_eq!(
            block.imports[4..]
                .iter()
                .map(|import| import.module_path.as_str())
                .collect::<Vec<_>>(),
            ["./E.sol", "./F.sol", "./G.sol", "./H.sol"]
        );
        assert_eq!(block.imports[4].kind, ImportKind::SideEffect);
        assert!(matches!(
            &block.imports[5].form,
            ImportForm::Solidity {
                alias: Some(alias),
                ..
            } if alias == "F"
        ));
        assert_eq!(block.imports[6].namespace_import.as_deref(), Some("G"));
        assert_eq!(
            block.imports[7].names,
            vec!["Quux".to_string(), "Corge as Grault".to_string()]
        );
    }

    #[test]
    fn generate_solidity_all_forms() {
        // side-effect
        assert_eq!(
            generate_import(
                LangId::Solidity,
                &ImportRequest::legacy("./A.sol", &[], None, None, false)
            ),
            "import \"./A.sol\";"
        );
        // named
        let names = vec!["Foo".to_string(), "Bar as Baz".to_string()];
        assert_eq!(
            generate_import(
                LangId::Solidity,
                &ImportRequest::legacy("./D.sol", &names, None, None, false)
            ),
            "import { Foo, Bar as Baz } from \"./D.sol\";"
        );
        // namespace
        assert_eq!(
            generate_import(
                LangId::Solidity,
                &ImportRequest::legacy("./C.sol", &[], None, Some("C"), false)
            ),
            "import * as C from \"./C.sol\";"
        );
        // whole-file alias
        assert_eq!(
            generate_import(
                LangId::Solidity,
                &ImportRequest {
                    module_path: "./B.sol",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: Some("B"),
                    type_only: false,
                    modifiers: &[],
                    import_kind: None,
                }
            ),
            "import \"./B.sol\" as B;"
        );
    }

    #[test]
    fn solidity_round_trips_through_parse_generate() {
        // Every generated form must parse back to the same structured shape.
        for src in [
            "import \"./A.sol\";",
            "import \"./B.sol\" as B;",
            "import * as C from \"./C.sol\";",
            "import { Foo, Bar as Baz } from \"./D.sol\";",
        ] {
            let (_, block) = parse_solidity(src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (namespace, alias) = match &imp.form {
                ImportForm::Solidity {
                    namespace, alias, ..
                } => (namespace.as_deref(), alias.as_deref()),
                other => panic!("expected Solidity, got {other:?}"),
            };
            let regenerated = generate_import(
                LangId::Solidity,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &imp.names,
                    default_import: None,
                    namespace,
                    alias,
                    type_only: false,
                    modifiers: &[],
                    import_kind: None,
                },
            );
            assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
        }
    }

    #[test]
    fn classify_group_solidity_relative_vs_external() {
        assert_eq!(classify_group_solidity("./A.sol"), ImportGroup::Internal);
        assert_eq!(
            classify_group_solidity("../lib/B.sol"),
            ImportGroup::Internal
        );
        assert_eq!(
            classify_group_solidity("@openzeppelin/contracts/token/ERC20/ERC20.sol"),
            ImportGroup::External
        );
    }
}
