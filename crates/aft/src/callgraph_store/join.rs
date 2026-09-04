//! Manifest-only callgraph assembly.
//!
//! This module deliberately accepts a manifest plus immutable blob payloads rather
//! than a checkout root.  Extraction is content-addressed and path-free; binding a
//! blob to a manifest path and resolving its cross-file references happens here.
//! The existing resolver uses String file identities. Non-UTF-8 source members
//! remain in byte-addressed facts but are reported as unbound rather than being
//! converted lossily; supporting them requires a separate resolver identity change.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use tree_sitter::Parser;

use super::facts::{BlobKey, FactPaths, ManifestFacts, ProjectFacts};
use crate::callgraph::{self, FileCallData, SymbolMeta};
use crate::imports::{ImportBlock, ImportForm, ImportGroup, ImportKind, ImportStatement};
use crate::parser::{grammar_for, LangId};
use crate::symbols::SymbolKind;
use crate::views::{Manifest, ManifestEntry, RelPath};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

const TOP_LEVEL_SYMBOL: &str = "<top-level>";

/// An error raised while decoding or assembling manifest-addressed callgraph data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestJoinError {
    UnsupportedLanguage(String),
    Parse(String),
    InvalidBlob(String),
    MissingBlob(String),
    InvalidConfig { path: Vec<u8>, reason: String },
}

impl fmt::Display for ManifestJoinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedLanguage(language) => {
                write!(formatter, "unsupported callgraph blob language {language}")
            }
            Self::Parse(reason) => write!(formatter, "callgraph blob parse failed: {reason}"),
            Self::InvalidBlob(reason) => write!(formatter, "invalid callgraph blob: {reason}"),
            Self::MissingBlob(key) => write!(
                formatter,
                "manifest references missing callgraph blob {key}"
            ),
            Self::InvalidConfig { path, reason } => write!(
                formatter,
                "invalid manifest config {}: {reason}",
                String::from_utf8_lossy(path)
            ),
        }
    }
}

impl std::error::Error for ManifestJoinError {}

/// Reads immutable payloads by the full key recorded in a manifest entry.
///
/// Implementations may be backed by the family blob store, but this interface
/// intentionally exposes no checkout path or directory operation to the join.
pub trait ManifestBlobReader {
    fn read_callgraph_blob(&self, full_key: &str) -> Result<Option<Vec<u8>>, ManifestJoinError>;
}

/// Tree-sitter node position in canonical pre-order traversal order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AstPreorderNode {
    pub ordinal: u32,
    pub kind: String,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// A symbol captured by extraction.  Its ordinal is the source AST node's
/// pre-order position, not a per-path or database-generated identifier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobSymbol {
    pub ordinal: u32,
    pub name: String,
    pub scoped_name: String,
    pub kind: String,
    pub exported: bool,
    pub is_default_export: bool,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub signature: Option<String>,
}

/// The parse-level class of a reference.  No target path is present in a blob.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobRefKind {
    Call,
    ValueRef,
    Import,
    Module,
    Reexport,
    ExportAlias,
}

/// An unresolved reference extracted from one source blob.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobRef {
    /// Canonical tree-sitter AST pre-order position of this reference.
    pub ordinal: u32,
    pub kind: BlobRefKind,
    pub caller_symbol: Option<String>,
    pub short_name: Option<String>,
    pub full_ref: Option<String>,
    pub module_path: Option<String>,
    pub line: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub path_override: Option<String>,
    pub local_name: Option<String>,
    pub requested_name: Option<String>,
    pub namespace_alias: Option<String>,
    pub wildcard: bool,
    pub import_kind: Option<String>,
}

/// A parsed import retained in the blob so binding can resolve aliases without
/// re-reading source text.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobImport {
    pub ordinal: u32,
    pub module_path: String,
    pub names: Vec<String>,
    pub default_import: Option<String>,
    pub namespace_import: Option<String>,
    pub byte_start: usize,
    pub byte_end: usize,
    pub raw_text: String,
    pub type_only: bool,
    pub side_effect: bool,
}

/// Path-free parse output stored for a regular source file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ParseBlob {
    pub extractor_version: String,
    pub language: String,
    pub ast_nodes: Vec<AstPreorderNode>,
    pub symbols: Vec<BlobSymbol>,
    pub default_export_symbol: Option<String>,
    pub exported_symbols: Vec<String>,
    pub callable_symbols: Vec<String>,
    pub imports: Vec<BlobImport>,
    pub refs: Vec<BlobRef>,
}

/// Raw source is retained only for configuration files and ignore-list members.
/// These blobs are parsed during joining because their interpretation depends on
/// the manifest view they configure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConfigBlob {
    pub extractor_version: String,
    pub language: String,
    pub source: Vec<u8>,
}

/// The immutable callgraph blob payload.  A regular source blob has parse output
/// only; a configuration blob is intentionally raw so its manifest-scoped
/// resolver settings can be interpreted during assembly.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CallgraphBlob {
    Parse(ParseBlob),
    Config(ConfigBlob),
}

impl CallgraphBlob {
    /// Extracts path-free callgraph parse output from source bytes and the
    /// extractor version that names the corresponding content key.
    pub fn extract(
        source: &str,
        language: &str,
        extractor_version: impl Into<String>,
    ) -> Result<Self, ManifestJoinError> {
        let lang = language_id(language)
            .ok_or_else(|| ManifestJoinError::UnsupportedLanguage(language.to_string()))?;
        let extractor_version = extractor_version.into();
        let ast_nodes = ast_preorder_nodes(source, lang)?;
        let mut data = callgraph::build_file_data_from_source_with_lang(
            std::path::Path::new("__callgraph_blob__"),
            source,
            lang,
        )
        .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
        if lang == LangId::Rust {
            super::extend_rust_imports_with_nested_uses(source, &mut data);
        }
        let symbols = blob_symbols(source, &data, &ast_nodes);
        let imports = blob_imports(&data, &ast_nodes);
        let mut refs = blob_refs(source, &data, &ast_nodes);
        refs.extend(rust_module_refs(source, lang, &ast_nodes));
        let empty =
            Manifest::new([]).map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
        let reader = |_: &BlobKey| None;
        let empty_facts = ManifestFacts {
            manifest: &empty,
            blobs: &reader,
        };
        let paths = FactPaths {
            root: Path::new("/"),
            facts: &empty_facts,
        };
        let file = Path::new("/__callgraph_blob__");
        let mut structural =
            super::collect_reexport_refs(paths.root, file, "__callgraph_blob__", source, &paths)
                .raw_refs;
        structural.extend(
            super::collect_source_less_export_alias_refs("__callgraph_blob__", source).raw_refs,
        );
        if lang == LangId::Rust {
            structural.extend(
                super::collect_rust_pub_use_reexport_refs(
                    paths.root,
                    file,
                    "__callgraph_blob__",
                    &data.import_block.imports,
                    &super::LineIndex::new(source),
                    &paths,
                )
                .raw_refs,
            );
        }
        refs.extend(
            structural
                .into_iter()
                .map(|raw| structural_ref(raw, &ast_nodes)),
        );
        let mut exported_symbols = data.exported_symbols.clone();
        exported_symbols.sort();
        let mut callable_symbols = data.calls_by_symbol.keys().cloned().collect::<Vec<_>>();
        callable_symbols.sort();
        refs.sort_by(|left, right| {
            (
                left.ordinal,
                left.kind,
                left.byte_start,
                left.byte_end,
                left.full_ref.as_deref(),
            )
                .cmp(&(
                    right.ordinal,
                    right.kind,
                    right.byte_start,
                    right.byte_end,
                    right.full_ref.as_deref(),
                ))
        });
        refs.dedup_by(|left, right| {
            left.ordinal == right.ordinal
                && left.kind == right.kind
                && left.byte_start == right.byte_start
                && left.byte_end == right.byte_end
                && left.full_ref == right.full_ref
        });

        Ok(Self::Parse(ParseBlob {
            extractor_version,
            language: language.to_string(),
            ast_nodes,
            symbols,
            default_export_symbol: data.default_export_symbol,
            exported_symbols,
            callable_symbols,
            imports,
            refs,
        }))
    }

    /// Builds a manifest configuration input.  The caller must key it with
    /// `language = "config"` and the same extractor version stored here.
    pub fn config(source: impl Into<Vec<u8>>, extractor_version: impl Into<String>) -> Self {
        Self::Config(ConfigBlob {
            extractor_version: extractor_version.into(),
            language: "config".to_string(),
            source: source.into(),
        })
    }

    /// Uses one canonical JSON encoding for the immutable payload bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ManifestJoinError> {
        serde_json::to_vec(self).map_err(|error| ManifestJoinError::InvalidBlob(error.to_string()))
    }

    /// Decodes a payload after the blob store has verified its digest and schema.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ManifestJoinError> {
        serde_json::from_slice(bytes)
            .map_err(|error| ManifestJoinError::InvalidBlob(error.to_string()))
    }

    pub fn parse(&self) -> Option<&ParseBlob> {
        match self {
            Self::Parse(blob) => Some(blob),
            Self::Config(_) => None,
        }
    }

    pub fn config_source(&self) -> Option<&ConfigBlob> {
        match self {
            Self::Parse(_) => None,
            Self::Config(blob) => Some(blob),
        }
    }
}

/// The stable identity of one bound blob reference.  The path breaks ties when
/// identical content is bound at more than one manifest path.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CallerRefKey {
    pub caller_blob_key: String,
    pub ref_ordinal: u32,
    pub caller_path: Vec<u8>,
}

/// The manifest-derived resolution state for one reference.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ResolutionStatus {
    Resolved,
    Unresolved,
}

/// A logical derived row.  It is intentionally independent of SQLite rowids and
/// other physical database details.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DerivedRow {
    pub caller_blob_key: String,
    pub ref_ordinal: u32,
    pub caller_path: Vec<u8>,
    pub kind: BlobRefKind,
    pub status: ResolutionStatus,
    pub target_path: Option<Vec<u8>>,
    pub target_symbol: Option<String>,
}

impl DerivedRow {
    pub fn ref_key(&self) -> CallerRefKey {
        CallerRefKey {
            caller_blob_key: self.caller_blob_key.clone(),
            ref_ordinal: self.ref_ordinal,
            caller_path: self.caller_path.clone(),
        }
    }
}

/// The result of resolving one manifest.  `resolution_order` exposes the exact
/// canonical order consumed by the resolver for deterministic test coverage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinResult {
    pub rows: BTreeSet<DerivedRow>,
    pub resolution_order: Vec<CallerRefKey>,
    pub unbound_non_utf8_paths: Vec<Vec<u8>>,
}

impl JoinResult {
    /// Serializes logical rows in canonical order.  This is the comparison form
    /// for equal manifests; callers must not compare SQLite file bytes.
    pub fn canonical_serialization(&self) -> Vec<u8> {
        let mut output = Vec::new();
        for row in &self.rows {
            append_field(&mut output, row.caller_blob_key.as_bytes());
            append_field(&mut output, &row.ref_ordinal.to_be_bytes());
            append_field(&mut output, &row.caller_path);
            append_field(&mut output, &[row.kind as u8]);
            append_field(
                &mut output,
                &[match row.status {
                    ResolutionStatus::Resolved => 1,
                    ResolutionStatus::Unresolved => 0,
                }],
            );
            append_optional_field(&mut output, row.target_path.as_deref());
            append_optional_field(&mut output, row.target_symbol.as_deref().map(str::as_bytes));
        }
        output
    }
}

/// Incremental assembly details used to verify precise invalidation behavior.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncrementalJoinResult {
    pub result: JoinResult,
    pub re_resolved: BTreeSet<CallerRefKey>,
    pub full_re_resolve: bool,
}

fn changed_manifest_paths(previous: &Manifest, current: &Manifest) -> BTreeSet<Vec<u8>> {
    let previous_entries = previous
        .entries()
        .map(|(path, entry)| (path.as_bytes().to_vec(), entry))
        .collect::<BTreeMap<_, _>>();
    let current_entries = current
        .entries()
        .map(|(path, entry)| (path.as_bytes().to_vec(), entry))
        .collect::<BTreeMap<_, _>>();
    previous_entries
        .keys()
        .chain(current_entries.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| previous_entries.get(*path) != current_entries.get(*path))
        .cloned()
        .collect()
}

fn manifest_resolution_input(manifest: &Manifest, path: &[u8]) -> bool {
    let lookup = if path.first() == Some(&0) {
        return manifest
            .entries()
            .find(|(candidate, _)| candidate.as_bytes() == path)
            .is_some_and(|(_, entry)| matches!(entry, ManifestEntry::Synthetic { .. }));
    } else {
        RelPath::new(path.to_vec()).ok()
    };
    lookup
        .as_ref()
        .and_then(|path| manifest.get(path))
        .is_some_and(|entry| {
            matches!(
                entry,
                ManifestEntry::Regular {
                    resolution_input: true,
                    ..
                }
            )
        })
}

fn append_field(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value);
}

fn append_optional_field(output: &mut Vec<u8>, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            output.push(1);
            append_field(output, value);
        }
        None => output.push(0),
    }
}

fn language_id(language: &str) -> Option<LangId> {
    Some(match language {
        "typescript" => LangId::TypeScript,
        "tsx" => LangId::Tsx,
        "javascript" => LangId::JavaScript,
        "python" => LangId::Python,
        "rust" => LangId::Rust,
        "go" => LangId::Go,
        "c" => LangId::C,
        "cpp" => LangId::Cpp,
        "zig" => LangId::Zig,
        "csharp" => LangId::CSharp,
        "bash" => LangId::Bash,
        "html" => LangId::Html,
        "markdown" => LangId::Markdown,
        "solidity" => LangId::Solidity,
        "scss" => LangId::Scss,
        "vue" => LangId::Vue,
        "json" => LangId::Json,
        "scala" => LangId::Scala,
        "java" => LangId::Java,
        "ruby" => LangId::Ruby,
        "kotlin" => LangId::Kotlin,
        "swift" => LangId::Swift,
        "php" => LangId::Php,
        "lua" => LangId::Lua,
        "perl" => LangId::Perl,
        "yaml" => LangId::Yaml,
        "pascal" => LangId::Pascal,
        "r" => LangId::R,
        "groovy" => LangId::Groovy,
        "objc" => LangId::ObjC,
        _ => return None,
    })
}

fn ast_preorder_nodes(
    source: &str,
    lang: LangId,
) -> Result<Vec<AstPreorderNode>, ManifestJoinError> {
    let mut parser = Parser::new();
    parser
        .set_language(&grammar_for(lang))
        .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| ManifestJoinError::Parse("tree-sitter returned no tree".to_string()))?;
    let mut nodes = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        nodes.push(AstPreorderNode {
            ordinal: nodes.len() as u32,
            kind: node.kind().to_string(),
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
        });
        let children = node.children(&mut node.walk()).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    Ok(nodes)
}

fn blob_symbols(
    source: &str,
    data: &FileCallData,
    ast_nodes: &[AstPreorderNode],
) -> Vec<BlobSymbol> {
    let mut symbols = data
        .symbol_metadata
        .iter()
        .map(|(scoped_name, meta)| {
            blob_symbol(
                source,
                scoped_name,
                meta,
                &data.default_export_symbol,
                ast_nodes,
            )
        })
        .collect::<Vec<_>>();
    symbols.sort_by(|left, right| {
        (
            left.ordinal,
            left.start_line,
            left.start_col,
            left.scoped_name.as_str(),
        )
            .cmp(&(
                right.ordinal,
                right.start_line,
                right.start_col,
                right.scoped_name.as_str(),
            ))
    });
    symbols
}

fn blob_symbol(
    source: &str,
    scoped_name: &str,
    meta: &SymbolMeta,
    default_export: &Option<String>,
    ast_nodes: &[AstPreorderNode],
) -> BlobSymbol {
    let byte_start = byte_offset(source, meta.range.start_line, meta.range.start_col);
    let byte_end = byte_offset(source, meta.range.end_line, meta.range.end_col).max(byte_start);
    BlobSymbol {
        ordinal: ordinal_for_range(ast_nodes, byte_start, byte_end),
        name: unqualified_symbol_name(scoped_name).to_string(),
        scoped_name: scoped_name.to_string(),
        kind: symbol_kind_name(&meta.kind).to_string(),
        exported: meta.exported,
        is_default_export: default_export.as_deref() == Some(scoped_name),
        start_line: meta.range.start_line,
        start_col: meta.range.start_col,
        end_line: meta.range.end_line,
        end_col: meta.range.end_col,
        signature: meta.signature.clone(),
    }
}

fn blob_imports(data: &FileCallData, ast_nodes: &[AstPreorderNode]) -> Vec<BlobImport> {
    let mut imports = data
        .import_block
        .imports
        .iter()
        .map(|import| BlobImport {
            ordinal: ordinal_for_range(ast_nodes, import.byte_range.start, import.byte_range.end),
            module_path: import.module_path.clone(),
            names: import.names.clone(),
            default_import: import.default_import.clone(),
            namespace_import: import.namespace_import.clone(),
            byte_start: import.byte_range.start,
            byte_end: import.byte_range.end,
            raw_text: import.raw_text.clone(),
            type_only: import.kind == ImportKind::Type,
            side_effect: import.kind == ImportKind::SideEffect,
        })
        .collect::<Vec<_>>();
    imports.sort_by(|left, right| {
        (left.ordinal, left.module_path.as_str()).cmp(&(right.ordinal, right.module_path.as_str()))
    });
    imports
}

fn blob_refs(source: &str, data: &FileCallData, ast_nodes: &[AstPreorderNode]) -> Vec<BlobRef> {
    let mut refs = Vec::new();
    for (caller_symbol, calls) in &data.calls_by_symbol {
        for call in calls {
            refs.push(call_ref(caller_symbol, call, BlobRefKind::Call, ast_nodes));
        }
    }
    for (caller_symbol, calls) in &data.value_refs_by_symbol {
        for call in calls {
            refs.push(call_ref(
                caller_symbol,
                call,
                BlobRefKind::ValueRef,
                ast_nodes,
            ));
        }
    }
    for import in &data.import_block.imports {
        refs.push(BlobRef {
            ordinal: ordinal_for_range(ast_nodes, import.byte_range.start, import.byte_range.end),
            kind: BlobRefKind::Import,
            caller_symbol: None,
            short_name: None,
            full_ref: Some(import.module_path.clone()),
            module_path: Some(import.module_path.clone()),
            line: line_for_byte(source, import.byte_range.start),
            byte_start: import.byte_range.start,
            byte_end: import.byte_range.end,
            path_override: None,
            local_name: None,
            requested_name: None,
            namespace_alias: import.namespace_import.clone(),
            wildcard: super::import_is_wildcard(import),
            import_kind: None,
        });
    }
    refs
}

fn call_ref(
    caller_symbol: &str,
    call: &callgraph::CallSite,
    kind: BlobRefKind,
    ast_nodes: &[AstPreorderNode],
) -> BlobRef {
    BlobRef {
        ordinal: ordinal_for_range(ast_nodes, call.byte_start, call.byte_end),
        kind,
        caller_symbol: Some(caller_symbol.to_string()),
        short_name: Some(call.callee_name.clone()),
        full_ref: Some(call.full_callee.clone()),
        module_path: None,
        line: call.line,
        byte_start: call.byte_start,
        byte_end: call.byte_end,
        path_override: None,
        local_name: None,
        requested_name: None,
        namespace_alias: None,
        wildcard: false,
        import_kind: None,
    }
}

fn rust_module_refs(source: &str, lang: LangId, ast_nodes: &[AstPreorderNode]) -> Vec<BlobRef> {
    if lang != LangId::Rust {
        return Vec::new();
    }
    let mut parser = Parser::new();
    if parser.set_language(&grammar_for(lang)).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut refs = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "mod_item"
            && node
                .named_children(&mut node.walk())
                .all(|child| child.kind() != "declaration_list")
        {
            if let Some(name) = node.child_by_field_name("name") {
                let module_name = source[name.byte_range()].to_string();
                refs.push(BlobRef {
                    ordinal: ordinal_for_range(ast_nodes, node.start_byte(), node.end_byte()),
                    kind: BlobRefKind::Module,
                    caller_symbol: None,
                    short_name: Some(module_name.clone()),
                    full_ref: Some(module_name.clone()),
                    module_path: Some(module_name),
                    line: node.start_position().row as u32 + 1,
                    byte_start: node.start_byte(),
                    byte_end: node.end_byte(),
                    path_override: super::rust_module_path_override(source, node)
                        .map(str::to_string),
                    local_name: None,
                    requested_name: None,
                    namespace_alias: None,
                    wildcard: false,
                    import_kind: None,
                });
            }
        }
        let children = node.children(&mut node.walk()).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    refs
}

fn ordinal_for_range(ast_nodes: &[AstPreorderNode], byte_start: usize, byte_end: usize) -> u32 {
    ast_nodes
        .iter()
        .filter(|node| node.byte_start <= byte_start && node.byte_end >= byte_end)
        .min_by_key(|node| (node.byte_end.saturating_sub(node.byte_start), node.ordinal))
        .map(|node| node.ordinal)
        .unwrap_or(0)
}

fn byte_offset(source: &str, line: u32, column: u32) -> usize {
    let mut offset = 0usize;
    for (index, segment) in source.split_inclusive('\n').enumerate() {
        if index as u32 == line {
            return offset + (column as usize).min(segment.len());
        }
        offset += segment.len();
    }
    source.len()
}

fn line_for_byte(source: &str, byte_start: usize) -> u32 {
    source[..byte_start.min(source.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u32
        + 1
}

fn symbol_kind_name(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Class => "class",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Variable => "variable",
        SymbolKind::Heading => "heading",
        SymbolKind::FileSummary => "file_summary",
    }
}

fn unqualified_symbol_name(scoped_name: &str) -> &str {
    if scoped_name == TOP_LEVEL_SYMBOL {
        return scoped_name;
    }
    scoped_name.rsplit("::").next().unwrap_or(scoped_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blob_ordinals_are_tree_sitter_preorder_positions() {
        let source = "export function run() { return helper(); }\nfunction helper() {}\n";
        let blob = CallgraphBlob::extract(source, "typescript", "join-test-v1").unwrap();
        let repeated = CallgraphBlob::extract(source, "typescript", "join-test-v1").unwrap();
        let different_version =
            CallgraphBlob::extract(source, "typescript", "join-test-v2").unwrap();
        assert_eq!(blob.to_bytes().unwrap(), repeated.to_bytes().unwrap());
        assert_ne!(
            blob.to_bytes().unwrap(),
            different_version.to_bytes().unwrap()
        );
        let parse = blob.parse().unwrap();
        assert_eq!(parse.ast_nodes[0].ordinal, 0);
        assert!(parse.refs.iter().all(|reference| parse
            .ast_nodes
            .iter()
            .any(|node| node.ordinal == reference.ordinal)));
        let helper = parse
            .refs
            .iter()
            .find(|reference| reference.short_name.as_deref() == Some("helper"))
            .unwrap();
        let node = parse
            .ast_nodes
            .iter()
            .find(|node| node.ordinal == helper.ordinal)
            .unwrap();
        assert!(node.byte_start <= helper.byte_start && node.byte_end >= helper.byte_end);
        assert_eq!(node.kind, "call_expression");
        let mut parser = Parser::new();
        parser
            .set_language(&grammar_for(LangId::TypeScript))
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut cursor = tree.walk();
        let mut expected = Vec::new();
        'preorder: loop {
            let node = cursor.node();
            expected.push(AstPreorderNode {
                ordinal: expected.len() as u32,
                kind: node.kind().to_string(),
                byte_start: node.start_byte(),
                byte_end: node.end_byte(),
            });
            if cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break 'preorder;
                }
            }
        }
        assert_eq!(parse.ast_nodes, expected);
    }
}

fn structural_ref(raw: super::RawRef, nodes: &[AstPreorderNode]) -> BlobRef {
    BlobRef {
        ordinal: ordinal_for_range(nodes, raw.byte_start, raw.byte_end),
        kind: if raw.kind == "export_alias" {
            BlobRefKind::ExportAlias
        } else {
            BlobRefKind::Reexport
        },
        caller_symbol: raw.caller_symbol,
        short_name: raw.short_name,
        full_ref: raw.full_ref,
        module_path: raw.module_path,
        line: raw.line,
        byte_start: raw.byte_start,
        byte_end: raw.byte_end,
        path_override: None,
        local_name: raw.local_name,
        requested_name: raw.requested_name,
        namespace_alias: raw.namespace_alias,
        wildcard: raw.wildcard,
        import_kind: raw.import_kind,
    }
}

fn bound_name(name: &str, path: &str) -> String {
    name.replace(
        "<default:__callgraph_blob__>",
        &format!(
            "<default:{}>",
            Path::new(path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ),
    )
}

impl ParseBlob {
    fn file_data(&self, path: &str) -> Result<FileCallData, ManifestJoinError> {
        let lang = language_id(&self.language)
            .ok_or_else(|| ManifestJoinError::UnsupportedLanguage(self.language.clone()))?;
        let mut calls_by_symbol: HashMap<String, Vec<callgraph::CallSite>> = HashMap::new();
        let mut value_refs_by_symbol: HashMap<String, Vec<callgraph::CallSite>> = HashMap::new();
        for raw in &self.refs {
            let map = match raw.kind {
                BlobRefKind::Call => &mut calls_by_symbol,
                BlobRefKind::ValueRef => &mut value_refs_by_symbol,
                _ => continue,
            };
            let Some(caller) = &raw.caller_symbol else {
                continue;
            };
            map.entry(bound_name(caller, path))
                .or_default()
                .push(callgraph::CallSite {
                    callee_name: raw.short_name.clone().unwrap_or_default(),
                    full_callee: raw.full_ref.clone().unwrap_or_default(),
                    line: raw.line,
                    byte_start: raw.byte_start,
                    byte_end: raw.byte_end,
                });
        }
        for symbol in &self.callable_symbols {
            calls_by_symbol.entry(bound_name(symbol, path)).or_default();
        }
        let mut symbol_metadata = HashMap::new();
        for symbol in &self.symbols {
            let kind = match symbol.kind.as_str() {
                "function" => SymbolKind::Function,
                "method" => SymbolKind::Method,
                "class" => SymbolKind::Class,
                "struct" => SymbolKind::Struct,
                "interface" => SymbolKind::Interface,
                "enum" => SymbolKind::Enum,
                "type_alias" => SymbolKind::TypeAlias,
                "heading" => SymbolKind::Heading,
                "file_summary" => SymbolKind::FileSummary,
                _ => SymbolKind::Variable,
            };
            symbol_metadata.insert(
                bound_name(&symbol.scoped_name, path),
                SymbolMeta {
                    kind,
                    exported: symbol.exported,
                    signature: symbol.signature.clone(),
                    line: symbol.start_line + 1,
                    range: crate::symbols::Range {
                        start_line: symbol.start_line,
                        start_col: symbol.start_col,
                        end_line: symbol.end_line,
                        end_col: symbol.end_col,
                    },
                    entry_point_attribute: None,
                },
            );
        }
        let imports = self
            .imports
            .iter()
            .map(|import| ImportStatement {
                module_path: import.module_path.clone(),
                names: import.names.clone(),
                default_import: import.default_import.clone(),
                namespace_import: import.namespace_import.clone(),
                kind: if import.type_only {
                    ImportKind::Type
                } else if import.side_effect {
                    ImportKind::SideEffect
                } else {
                    ImportKind::Value
                },
                group: ImportGroup::Internal,
                byte_range: import.byte_start..import.byte_end,
                raw_text: import.raw_text.clone(),
                form: if lang == LangId::Rust {
                    ImportForm::RustUse {
                        visibility: import.default_import.clone(),
                        named: import.names.clone(),
                    }
                } else {
                    ImportForm::Es {
                        default_import: import.default_import.clone(),
                        namespace_import: import.namespace_import.clone(),
                        named: import.names.clone(),
                        type_only: import.type_only,
                        side_effect: import.side_effect,
                        attribute_clause: None,
                        attribute_type: None,
                    }
                },
            })
            .collect::<Vec<_>>();
        Ok(FileCallData {
            calls_by_symbol,
            value_refs_by_symbol,
            symbol_metadata,
            exported_symbols: self
                .exported_symbols
                .iter()
                .map(|s| bound_name(s, path))
                .collect(),
            default_export_symbol: self
                .default_export_symbol
                .as_ref()
                .map(|s| bound_name(s, path)),
            import_block: ImportBlock {
                imports,
                byte_range: None,
            },
            lang,
        })
    }

    fn bind(
        &self,
        path: &str,
        facts: &FactPaths<'_>,
    ) -> Result<super::FileExtract, ManifestJoinError> {
        let data = self.file_data(path)?;
        let nodes = self
            .symbols
            .iter()
            .map(|symbol| {
                let scoped_name = bound_name(&symbol.scoped_name, path);
                super::NodeRecord {
                    id: format!("{path}:{}:{scoped_name}", symbol.ordinal),
                    file_path: path.to_string(),
                    name: bound_name(&symbol.name, path),
                    scoped_name,
                    kind: symbol.kind.clone(),
                    range: crate::symbols::Range {
                        start_line: symbol.start_line,
                        start_col: symbol.start_col,
                        end_line: symbol.end_line,
                        end_col: symbol.end_col,
                    },
                    range_ordinal: symbol.ordinal,
                    signature: symbol.signature.clone(),
                    exported: symbol.exported,
                    is_default_export: symbol.is_default_export,
                    is_type_like: false,
                    is_callgraph_entry_point: false,
                }
            })
            .collect::<Vec<_>>();
        let abs = facts.root.join(path);
        let mut raw_refs = Vec::new();
        for raw in &self.refs {
            let dependencies = if raw.kind == BlobRefKind::Module {
                super::rust_external_module_target(
                    &abs,
                    raw.path_override.as_deref(),
                    raw.module_path.as_deref().unwrap_or_default(),
                    facts,
                )
                .and_then(|p| facts.canonical(&p))
                .map(|p| super::relative_path(facts.root, &p))
                .into_iter()
                .collect()
            } else if let Some(module) = &raw.module_path {
                super::module_dependencies(facts.root, &abs, module, facts)
            } else {
                BTreeSet::new()
            };
            let caller_symbol = raw
                .caller_symbol
                .as_ref()
                .map(|name| bound_name(name, path));
            let caller_node = caller_symbol.as_ref().and_then(|name| {
                nodes
                    .iter()
                    .find(|n| &n.scoped_name == name)
                    .map(|n| n.id.clone())
            });
            raw_refs.push(super::RawRef {
                ref_id: format!("{path}:{}:{:?}", raw.ordinal, raw.kind),
                caller_node,
                caller_symbol,
                caller_file: path.to_string(),
                kind: match raw.kind {
                    BlobRefKind::Call => "call",
                    BlobRefKind::ValueRef => "value_ref",
                    BlobRefKind::Import => "import",
                    BlobRefKind::Module => "module",
                    BlobRefKind::Reexport => "reexport",
                    BlobRefKind::ExportAlias => "export_alias",
                }
                .to_string(),
                short_name: raw.short_name.clone(),
                full_ref: raw.full_ref.clone(),
                module_path: raw.module_path.clone(),
                import_kind: raw.import_kind.clone(),
                local_name: raw.local_name.clone(),
                requested_name: raw.requested_name.clone(),
                namespace_alias: raw.namespace_alias.clone(),
                wildcard: raw.wildcard,
                line: raw.line,
                byte_start: raw.byte_start,
                byte_end: raw.byte_end,
                dependencies,
            });
        }
        Ok(super::FileExtract {
            rel_path: path.to_string(),
            freshness: crate::cache_freshness::FileFreshness {
                mtime: std::time::UNIX_EPOCH,
                size: 0,
                content_hash: crate::cache_freshness::zero_hash(),
            },
            lang: data.lang,
            data,
            nodes,
            raw_refs,
            dispatch_hints: Vec::new(),
            surface_fingerprint: String::new(),
        })
    }
}

/// A manifest-backed index is the ordinary resolver index with different facts.
type ManifestProjectIndex<'a> = super::ProjectIndex<'a>;

impl JoinResult {
    pub fn from_manifest(
        manifest: &Manifest,
        blobs: &impl ManifestBlobReader,
    ) -> Result<Self, ManifestJoinError> {
        let loaded = manifest_payloads(manifest, blobs)?;
        let reader = |key: &BlobKey| loaded.get(key).cloned();
        let facts = Rc::new(ManifestFacts {
            manifest,
            blobs: &reader,
        });
        Self::from_facts(manifest, &loaded, Path::new("/"), facts, None)
    }

    fn from_facts<'a>(
        manifest: &'a Manifest,
        loaded: &BTreeMap<String, Arc<[u8]>>,
        root: &Path,
        facts: Rc<dyn ProjectFacts + 'a>,
        selected: Option<&BTreeSet<CallerRefKey>>,
    ) -> Result<Self, ManifestJoinError> {
        let paths = FactPaths {
            root,
            facts: facts.as_ref(),
        };
        let mut extracts = HashMap::new();
        let mut work = Vec::new();
        let mut unbound_non_utf8_paths = Vec::new();
        for (path, entry) in manifest.entries() {
            let ManifestEntry::Regular { planes, .. } = entry else {
                continue;
            };
            let Some(key) = &planes.callgraph else {
                continue;
            };
            let bytes = loaded
                .get(key)
                .ok_or_else(|| ManifestJoinError::MissingBlob(key.clone()))?;
            let CallgraphBlob::Parse(blob) = CallgraphBlob::from_bytes(bytes)? else {
                continue;
            };
            let Ok(rel) = std::str::from_utf8(path.as_bytes()) else {
                unbound_non_utf8_paths.push(path.as_bytes().to_vec());
                continue;
            };
            let extract = blob.bind(rel, &paths)?;
            for (raw, bound) in blob.refs.iter().zip(&extract.raw_refs) {
                let ref_key = CallerRefKey {
                    caller_blob_key: key.clone(),
                    ref_ordinal: raw.ordinal,
                    caller_path: path.as_bytes().to_vec(),
                };
                if selected.is_none_or(|set| set.contains(&ref_key)) {
                    work.push((ref_key, (raw.kind, bound.clone())));
                }
            }
            extracts.insert(rel.to_string(), extract);
        }
        let files = extracts
            .iter()
            .map(|(path, extract)| {
                (
                    path.clone(),
                    super::DbFileIndex::from_extract(root, extract, &paths),
                )
            })
            .collect();
        let caller_data = extracts
            .iter()
            .map(|(path, extract)| (path.clone(), &extract.data))
            .collect();
        let mut index = ManifestProjectIndex::from_parts(
            root,
            files,
            caller_data,
            super::WorkspaceCratePrefixCache::default(),
            facts,
        );
        index.unbound_non_utf8_paths = unbound_non_utf8_paths;
        if !index.unbound_non_utf8_paths.is_empty() {
            log::warn!(
                "callgraph index left {} non-UTF-8 source paths unbound",
                index.unbound_non_utf8_paths.len()
            );
        }
        let mut result = Self {
            rows: BTreeSet::new(),
            resolution_order: Vec::new(),
            unbound_non_utf8_paths: index.unbound_non_utf8_paths.clone(),
        };
        work.sort_by(|a, b| (&a.0, a.1 .0).cmp(&(&b.0, b.1 .0)));
        for (key, (kind, raw)) in work {
            let resolved = super::resolve_ref(raw, &index)
                .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
            result.rows.insert(DerivedRow {
                caller_blob_key: key.caller_blob_key.clone(),
                ref_ordinal: key.ref_ordinal,
                caller_path: key.caller_path.clone(),
                kind,
                status: if resolved.target_file.is_some() {
                    ResolutionStatus::Resolved
                } else {
                    ResolutionStatus::Unresolved
                },
                target_path: resolved.target_file.map(String::into_bytes),
                target_symbol: resolved.target_symbol,
            });
            result.resolution_order.push(key);
        }
        Ok(result)
    }

    pub fn update(
        &self,
        previous_manifest: &Manifest,
        manifest: &Manifest,
        blobs: &impl ManifestBlobReader,
    ) -> Result<IncrementalJoinResult, ManifestJoinError> {
        let changed = changed_manifest_paths(previous_manifest, manifest);
        let full_re_resolve = changed.iter().any(|path| {
            manifest_resolution_input(previous_manifest, path)
                || manifest_resolution_input(manifest, path)
        });
        let loaded = manifest_payloads(manifest, blobs)?;
        let reader = |key: &BlobKey| loaded.get(key).cloned();
        let mut current_keys = BTreeSet::new();
        for (path, entry) in manifest.entries() {
            if std::str::from_utf8(path.as_bytes()).is_err() {
                continue;
            }
            let ManifestEntry::Regular { planes, .. } = entry else {
                continue;
            };
            let Some(key) = &planes.callgraph else {
                continue;
            };
            if let CallgraphBlob::Parse(blob) = CallgraphBlob::from_bytes(&loaded[key])? {
                current_keys.extend(blob.refs.iter().map(|raw| CallerRefKey {
                    caller_blob_key: key.clone(),
                    ref_ordinal: raw.ordinal,
                    caller_path: path.as_bytes().to_vec(),
                }));
            }
        }
        let previous_rows = self
            .rows
            .iter()
            .map(|row| (row.ref_key(), row))
            .collect::<BTreeMap<_, _>>();
        let selected = current_keys
            .iter()
            .filter(|key| {
                full_re_resolve
                    || changed.contains(&key.caller_path)
                    || previous_rows.get(*key).is_none_or(|row| {
                        row.target_path
                            .as_ref()
                            .is_some_and(|path| changed.contains(path))
                    })
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        let facts = Rc::new(ManifestFacts {
            manifest,
            blobs: &reader,
        });
        let mut result =
            Self::from_facts(manifest, &loaded, Path::new("/"), facts, Some(&selected))?;
        result.rows.extend(
            self.rows
                .iter()
                .filter(|row| {
                    current_keys.contains(&row.ref_key()) && !selected.contains(&row.ref_key())
                })
                .cloned(),
        );
        result.resolution_order = result.rows.iter().map(DerivedRow::ref_key).collect();
        result.resolution_order.sort();
        Ok(IncrementalJoinResult {
            result,
            re_resolved: selected,
            full_re_resolve,
        })
    }
}

fn manifest_payloads(
    manifest: &Manifest,
    blobs: &impl ManifestBlobReader,
) -> Result<BTreeMap<String, Arc<[u8]>>, ManifestJoinError> {
    let mut loaded = BTreeMap::new();
    for (_, entry) in manifest.entries() {
        let ManifestEntry::Regular { planes, .. } = entry else {
            continue;
        };
        let Some(key) = &planes.callgraph else {
            continue;
        };
        if !loaded.contains_key(key) {
            let bytes = blobs
                .read_callgraph_blob(key)?
                .ok_or_else(|| ManifestJoinError::MissingBlob(key.clone()))?;
            loaded.insert(key.clone(), Arc::from(bytes));
        }
    }
    Ok(loaded)
}

#[cfg(test)]
#[path = "../../tests/integration/join_manifest_test.rs"]
mod manifest_integration_tests;
