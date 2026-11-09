//! Path-free callgraph extraction and checkout-backed reference resolution.
//!
//! The index store keeps immutable parse payloads. Binding a payload to its
//! checkout path and resolving cross-file references uses the checkout's
//! [`ProjectFacts`], not a separate manifest or artifact store.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[path = "dispatch.rs"]
pub mod dispatch;
#[path = "join_record.rs"]
pub(crate) mod record;

#[cfg(test)]
pub(crate) use extraction_work::CountingParser as Parser;
use serde::{Deserialize, Serialize};
#[cfg(not(test))]
use tree_sitter::Parser;

#[cfg(test)]
#[path = "extraction_work.rs"]
pub(crate) mod extraction_work;

#[cfg(test)]
#[path = "extraction_tests.rs"]
mod extraction_tests;

#[cfg(test)]
#[path = "payload_tests.rs"]
mod payload_tests;

#[path = "extraction_index.rs"]
mod extraction_index;

#[path = "single_walk.rs"]
pub(crate) mod single_walk;

#[path = "syntax_index.rs"]
pub(crate) mod syntax_index;

use super::facts::{BlobKey, FactPaths, NoProjectFacts, ProjectFacts};
use crate::callgraph::{self, FileCallData, SymbolMeta};
use crate::imports::{ImportBlock, ImportForm, ImportGroup, ImportKind, ImportStatement};
use crate::parser::{grammar_for, LangId};
use crate::symbols::SymbolKind;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

const TOP_LEVEL_SYMBOL: &str = "<top-level>";

/// The reason a view publication fence gives when it refuses a publication
/// because the checkout's view owner was superseded. A cancelled join
/// ([`ManifestJoinError::Cancelled`]) reports the same text.
pub const SUPERSEDED_PUBLISH: &str = "checkout view owner superseded; publication refused";

/// An error raised while decoding parse payloads or resolving checkout references.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestJoinError {
    Cancelled,
    UnsupportedLanguage(String),
    Parse(String),
    InvalidBlob(String),
}

impl fmt::Display for ManifestJoinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str(SUPERSEDED_PUBLISH),
            Self::UnsupportedLanguage(language) => {
                write!(formatter, "unsupported callgraph blob language {language}")
            }
            Self::Parse(reason) => write!(formatter, "callgraph blob parse failed: {reason}"),
            Self::InvalidBlob(reason) => write!(formatter, "invalid callgraph blob: {reason}"),
        }
    }
}

impl std::error::Error for ManifestJoinError {}

/// Tree-sitter node position in canonical pre-order traversal order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AstPreorderNode {
    pub ordinal: u32,
    /// Grammar names are static during extraction; legacy payload decoding owns
    /// the same text.
    pub kind: Cow<'static, str>,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// A symbol captured by extraction.  Its ordinal is the source AST node's
/// pre-order position, not a per-path or database-generated identifier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobSymbol {
    #[serde(default)]
    pub is_test: Option<bool>,
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
    /// Whether the program starts here, as the symbol's language defines it
    /// (`SymbolMeta::program_start`).
    #[serde(default)]
    pub program_start: bool,
    /// The entry attribute extraction recorded for the symbol
    /// (`SymbolMeta::entry_point_attribute`), such as `no_mangle`.
    #[serde(default)]
    pub entry_attribute: Option<String>,
}

/// The parse-level class of a reference.  No target path is present in a blob.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobRefKind {
    Construction,
    CallbackRef,
    Call,
    ValueRef,
    Import,
    Module,
    Reexport,
    ExportAlias,
    /// A name inside a Rust macro whose tokens tree-sitter could not parse:
    /// a call there is invisible to the graph, so callers of that name note
    /// where it occurs. Never resolved.
    MacroMention,
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
    pub entry_point_symbols: Vec<String>,
    #[serde(default)]
    pub dispatch: dispatch::DispatchFacts,
    pub extractor_version: String,
    pub language: String,
    /// Extraction uses node spans to assign ordinals. All later readers use the
    /// ordinals already copied into symbols, imports, references and dispatch.
    /// Keep accepting older payloads, but do not persist the temporary index.
    #[serde(skip_serializing, default)]
    pub ast_nodes: Vec<AstPreorderNode>,
    pub symbols: Vec<BlobSymbol>,
    pub default_export_symbol: Option<String>,
    pub exported_symbols: Vec<String>,
    pub callable_symbols: Vec<String>,
    pub imports: Vec<BlobImport>,
    #[serde(default)]
    pub python_bindings: Vec<callgraph::python::CallBinding>,
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
        single_walk::extract(source, language, extractor_version.into())
    }

    /// [`Self::extract_tree`], also returning the file's symbols as the
    /// parser extracts them for outline: the same table, from the same walk,
    /// before the call graph qualifies, adds or merges any.
    pub(crate) fn extract_tree_with_outline(
        source: &str,
        language: &str,
        extractor_version: impl Into<String>,
        tree: &tree_sitter::Tree,
    ) -> Result<(Self, Vec<crate::symbols::Symbol>), ManifestJoinError> {
        let mut outline = Vec::new();
        let blob = single_walk::extract_tree(
            source,
            language,
            extractor_version.into(),
            tree,
            Some(&mut outline),
        )?;
        Ok((blob, outline))
    }

    /// The extractor before the single walk: the same passes, each walking
    /// the tree itself and asking tree-sitter for parents and siblings. It
    /// stays only so the extraction gate (`data_layer::extract_gate`) can
    /// check both give the same output.
    pub fn extract_reference(
        source: &str,
        language: &str,
        extractor_version: impl Into<String>,
    ) -> Result<Self, ManifestJoinError> {
        let lang = language_id(language)
            .ok_or_else(|| ManifestJoinError::UnsupportedLanguage(language.to_string()))?;
        let extractor_version = extractor_version.into();
        let tree = match crate::parser::cached_header_tree(source, lang) {
            Some(tree) => tree,
            None => {
                let mut parser = Parser::new();
                parser
                    .set_language(&grammar_for(lang))
                    .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
                parser.parse(source, None).ok_or_else(|| {
                    ManifestJoinError::Parse("tree-sitter returned no tree".to_string())
                })?
            }
        };
        let root = tree.root_node();
        let ast_nodes = ast_preorder_nodes(root);
        let line_index = super::LineIndex::new(source);
        let mut data = callgraph::build_file_data_from_tree(
            std::path::Path::new("__callgraph_blob__"),
            source,
            lang,
            &tree,
            &line_index,
        )
        .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
        if lang == LangId::Rust {
            super::extend_rust_imports_from_tree(source, root, &mut data);
        }
        let mut symbols = blob_symbols(&data);
        let mut imports = blob_imports(&data);
        let mut refs = blob_refs(&line_index, &data);
        refs.extend(rust_module_refs(source, lang, root));
        let paths = FactPaths {
            root: Path::new("/"),
            facts: &NoProjectFacts,
        };
        let file = Path::new("/__callgraph_blob__");
        let mut structural = super::collect_reexport_refs_with_line_index(
            paths.root,
            file,
            "__callgraph_blob__",
            source,
            lang,
            &crate::callgraph::ModuleResolutionMemo::default(),
            &paths,
            &line_index,
            Some(root),
        )
        .raw_refs;
        structural.extend(
            super::collect_source_less_export_alias_refs_with_line_index(
                "__callgraph_blob__",
                source,
                &line_index,
            )
            .raw_refs,
        );
        if lang == LangId::Rust {
            structural.extend(
                super::collect_rust_pub_use_reexport_refs(
                    paths.root,
                    file,
                    "__callgraph_blob__",
                    &data.import_block.imports,
                    &line_index,
                    &paths,
                )
                .raw_refs,
            );
        }
        refs.extend(structural.into_iter().map(structural_ref));
        assign_ordinals(
            &ast_nodes,
            &line_index,
            &mut symbols,
            &mut imports,
            &mut refs,
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
                left.caller_symbol.as_deref(),
            )
                .cmp(&(
                    right.ordinal,
                    right.kind,
                    right.byte_start,
                    right.byte_end,
                    right.full_ref.as_deref(),
                    right.caller_symbol.as_deref(),
                ))
        });
        refs.dedup_by(|left, right| {
            left.ordinal == right.ordinal
                && left.kind == right.kind
                && left.byte_start == right.byte_start
                && left.byte_end == right.byte_end
                && left.full_ref == right.full_ref
                && left.caller_symbol == right.caller_symbol
        });

        let mut parse = ParseBlob {
            entry_point_symbols: data
                .symbol_metadata
                .iter()
                .filter(|(_, meta)| meta.entry_point_attribute.is_some())
                .map(|(name, _)| name.clone())
                .collect(),
            dispatch: dispatch::DispatchFacts::default(),
            extractor_version,
            language: language.to_string(),
            ast_nodes,
            symbols,
            default_export_symbol: data.default_export_symbol,
            exported_symbols,
            callable_symbols,
            imports,
            python_bindings: data.python_bindings,
            refs,
        };
        parse.entry_point_symbols.sort();
        parse.dispatch = dispatch::extract(source, lang, root, &mut parse)?;
        Ok(Self::Parse(parse))
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

pub(crate) fn language_id(language: &str) -> Option<LangId> {
    Some(match language {
        "typescript" => LangId::TypeScript,
        "tsx" => LangId::Tsx,
        "javascript" => LangId::JavaScript,
        "python" => LangId::Python,
        "rust" => LangId::Rust,
        "go" => LangId::Go,
        "c" => LangId::C,
        "cpp" => LangId::Cpp,
        "cuda" => LangId::Cuda,
        "metal" => LangId::Metal,
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
        "toml" => LangId::Toml,
        _ => return None,
    })
}

fn ast_preorder_nodes(root: tree_sitter::Node<'_>) -> Vec<AstPreorderNode> {
    let mut nodes = Vec::new();
    for node in extraction_index::preorder(root) {
        let kind = Cow::Borrowed(node.kind());
        #[cfg(test)]
        extraction_work::note(|work| {
            work.kind_copies += usize::from(matches!(kind, Cow::Owned(_)))
        });
        nodes.push(AstPreorderNode {
            ordinal: nodes.len() as u32,
            kind,
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
        });
    }
    nodes
}

fn blob_symbols(data: &FileCallData) -> Vec<BlobSymbol> {
    data.symbol_metadata
        .iter()
        .map(|(scoped_name, meta)| blob_symbol(scoped_name, meta, &data.default_export_symbol))
        .collect()
}

fn sort_blob_symbols(symbols: &mut [BlobSymbol]) {
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
}

fn blob_symbol(
    scoped_name: &str,
    meta: &SymbolMeta,
    default_export: &Option<String>,
) -> BlobSymbol {
    BlobSymbol {
        is_test: meta.is_test,
        ordinal: 0,
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
        program_start: meta.program_start,
        entry_attribute: meta.entry_point_attribute.clone(),
    }
}

fn blob_imports(data: &FileCallData) -> Vec<BlobImport> {
    data.import_block
        .imports
        .iter()
        .map(|import| BlobImport {
            ordinal: 0,
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
        .collect()
}

fn blob_refs(line_index: &super::LineIndex, data: &FileCallData) -> Vec<BlobRef> {
    let mut refs = Vec::new();
    for (caller_symbol, calls) in ordered_symbol_sites(&data.calls_by_symbol) {
        for call in calls {
            refs.push(call_ref(caller_symbol, call, BlobRefKind::Call));
        }
    }
    for (caller_symbol, calls) in ordered_symbol_sites(&data.value_refs_by_symbol) {
        for call in calls {
            refs.push(call_ref(caller_symbol, call, BlobRefKind::ValueRef));
        }
    }
    for (caller_symbol, calls) in ordered_symbol_sites(&data.macro_body_calls_by_macro) {
        for call in calls {
            refs.push(call_ref(caller_symbol, call, BlobRefKind::Call));
        }
    }
    for (caller_symbol, mentions) in ordered_symbol_sites(&data.macro_mentions_by_symbol) {
        for mention in mentions {
            refs.push(call_ref(caller_symbol, mention, BlobRefKind::MacroMention));
        }
    }
    for import in &data.import_block.imports {
        refs.push(BlobRef {
            ordinal: 0,
            kind: BlobRefKind::Import,
            caller_symbol: None,
            short_name: None,
            full_ref: Some(import.module_path.clone()),
            module_path: Some(import.module_path.clone()),
            line: line_index.byte_to_line(import.byte_range.start),
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

/// Reference construction must not depend on a hash map's randomized order.
/// One source call can belong to several enclosing symbols, and each caller is
/// distinct evidence even when the call's AST range is identical.
fn ordered_symbol_sites(
    sites: &HashMap<String, Vec<callgraph::CallSite>>,
) -> Vec<(&str, &[callgraph::CallSite])> {
    let mut ordered = sites
        .iter()
        .map(|(caller, calls)| (caller.as_str(), calls.as_slice()))
        .collect::<Vec<_>>();
    ordered.sort_unstable_by_key(|(caller, _)| *caller);
    ordered
}

fn call_ref(caller_symbol: &str, call: &callgraph::CallSite, kind: BlobRefKind) -> BlobRef {
    BlobRef {
        ordinal: 0,
        kind: match call.kind {
            callgraph::CallSiteKind::Construction => BlobRefKind::Construction,
            callgraph::CallSiteKind::CallbackReference => BlobRefKind::CallbackRef,
            callgraph::CallSiteKind::Call => kind,
        },
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

fn rust_module_refs(source: &str, lang: LangId, root: tree_sitter::Node<'_>) -> Vec<BlobRef> {
    if lang != LangId::Rust {
        return Vec::new();
    }
    let mut refs = Vec::new();
    for node in extraction_index::preorder(root) {
        if node.kind() == "mod_item"
            && node
                .named_children(&mut node.walk())
                .all(|child| child.kind() != "declaration_list")
        {
            if let Some(name) = node.child_by_field_name("name") {
                let module_name = source[name.byte_range()].to_string();
                refs.push(BlobRef {
                    ordinal: 0,
                    kind: BlobRefKind::Module,
                    caller_symbol: None,
                    short_name: Some(module_name.clone()),
                    full_ref: Some(if super::rust_module_has_macro_use(source, node) {
                        format!("#[macro_use] mod {module_name};")
                    } else {
                        module_name.clone()
                    }),
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
    }
    refs
}

fn assign_ordinals(
    ast_nodes: &[AstPreorderNode],
    lines: &super::LineIndex,
    symbols: &mut [BlobSymbol],
    imports: &mut [BlobImport],
    refs: &mut [BlobRef],
) {
    let ranges = ast_nodes
        .iter()
        .map(|n| (n.byte_start, n.byte_end))
        .collect::<Vec<_>>();
    let queries = symbols
        .iter()
        .map(|symbol| {
            let start = lines.inclusive_byte_offset(symbol.start_line, symbol.start_col);
            let end = lines
                .inclusive_byte_offset(symbol.end_line, symbol.end_col)
                .max(start);
            (start, end)
        })
        .chain(imports.iter().map(|i| (i.byte_start, i.byte_end)))
        .chain(refs.iter().map(|r| (r.byte_start, r.byte_end)))
        .collect::<Vec<_>>();
    let ordinals =
        extraction_index::range_minima(&ranges, &queries, extraction_index::LookupWork::Ordinal);
    for (ordinal, target) in ordinals.into_iter().zip(
        symbols
            .iter_mut()
            .map(|s| &mut s.ordinal)
            .chain(imports.iter_mut().map(|i| &mut i.ordinal))
            .chain(refs.iter_mut().map(|r| &mut r.ordinal)),
    ) {
        *target = ordinal.map(|i| ast_nodes[i].ordinal).unwrap_or(0);
    }
    sort_blob_symbols(symbols);
    imports.sort_by(|left, right| {
        (left.ordinal, left.module_path.as_str()).cmp(&(right.ordinal, right.module_path.as_str()))
    });
}

fn symbol_kind_name(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Kernel => "kernel",
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

    /// The replay's quick dependency check agrees with the full list for
    /// every kind of surface query, whichever of its dependencies changed.
    #[test]
    fn a_surface_query_depends_on_exactly_its_listed_dependencies() {
        let file = "crates/demo/src/a/b.rs".to_string();
        let symbol = "f".to_string();
        let queries = [
            ViewSurfaceQuery::Language(file.clone()),
            ViewSurfaceQuery::Module(file.clone(), symbol.clone()),
            ViewSurfaceQuery::MacroUse(file.clone()),
            ViewSurfaceQuery::Parent(file.clone()),
            ViewSurfaceQuery::Reexports(file.clone()),
            ViewSurfaceQuery::Node(file.clone(), symbol.clone()),
            ViewSurfaceQuery::Callable(file.clone(), symbol.clone()),
            ViewSurfaceQuery::Alias(file.clone(), symbol.clone()),
            ViewSurfaceQuery::Export(file.clone(), symbol.clone()),
            ViewSurfaceQuery::Default(file.clone()),
            ViewSurfaceQuery::Contains(file.clone()),
            ViewSurfaceQuery::Crate("demo".to_string()),
            ViewSurfaceQuery::CrateRoot(file.clone()),
            ViewSurfaceQuery::Inline(
                file.clone(),
                vec!["a".to_string(), "b".to_string()],
                symbol.clone(),
            ),
            ViewSurfaceQuery::GoPackage(file.clone(), Some("example.com/m".to_string())),
        ];
        let mut candidates = queries
            .iter()
            .flat_map(ViewSurfaceQuery::dependencies)
            .collect::<BTreeSet<_>>();
        candidates.insert("crates/demo/src/other.rs".to_string());
        candidates.insert(VIEW_RUST_MODULE_DOMAIN.to_string());
        candidates.insert(VIEW_GO_PACKAGE_DOMAIN.to_string());
        candidates.insert(VIEW_GO_MODULES_DOMAIN.to_string());
        for query in &queries {
            let listed = query.dependencies();
            assert!(!query.depends_on_any(&BTreeSet::new()), "{query:?}");
            for candidate in &candidates {
                let changed = BTreeSet::from([candidate.clone()]);
                assert_eq!(
                    query.depends_on_any(&changed),
                    listed.contains(candidate),
                    "{query:?} with {candidate:?} changed"
                );
            }
        }
    }

    #[test]
    fn blob_extraction_is_deterministic_for_nested_callers() {
        let source = include_str!("../../../../benchmarks/aft-search/author_real_query.py");
        let first = CallgraphBlob::extract(source, "python", "determinism-test-v1").unwrap();
        let expected = first.to_bytes().unwrap();
        for attempt in 1..32 {
            let actual = CallgraphBlob::extract(source, "python", "determinism-test-v1").unwrap();
            assert!(
                actual.to_bytes().unwrap() == expected,
                "serialization changed at attempt {attempt}"
            );
        }
        let call_start = source.find("subprocess.run(command,").unwrap();
        let callers = first
            .parse()
            .unwrap()
            .refs
            .iter()
            .filter(|r| {
                r.full_ref.as_deref() == Some("subprocess.run") && r.byte_start == call_start
            })
            .filter_map(|r| r.caller_symbol.as_deref())
            .collect::<BTreeSet<_>>();
        assert_eq!(callers, BTreeSet::from(["at_pin", "pinned_domains"]));

        // Check the construction order as well as final serialization: later
        // stable sorts preserve input order when several references share a key.
        // Fresh maps exercise different hash seeds without depending on a seed
        // exposed by the standard library.
        let expected = (0..64)
            .map(|i| format!("caller_{i:02}"))
            .collect::<Vec<_>>();
        for _ in 0..16 {
            let sites = expected
                .iter()
                .rev()
                .map(|name| (name.clone(), Vec::new()))
                .collect::<HashMap<_, _>>();
            let actual = ordered_symbol_sites(&sites)
                .into_iter()
                .map(|(caller, _)| caller)
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        }
    }

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
                kind: Cow::Borrowed(node.kind()),
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

fn structural_ref(raw: super::RawRef) -> BlobRef {
    BlobRef {
        ordinal: 0,
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
    /// Bind Python dispatch hints to the same keys as their call references.
    /// Nested bodies retain each enclosing caller, so their hints must use
    /// caller-specific keys rather than the shared syntax ordinal.
    pub(crate) fn bound_dispatch_sites(&self) -> Vec<dispatch::SiteHint> {
        let mut sites = self.dispatch.sites.clone();
        if self.language == "python" {
            let keys = self
                .refs
                .iter()
                .zip(self.reference_keys())
                .filter(|(reference, _)| reference.kind == BlobRefKind::Call)
                .map(|(reference, key)| {
                    ((reference.ordinal, reference.caller_symbol.as_deref()), key)
                })
                .collect::<HashMap<_, _>>();
            for site in &mut sites {
                if let Some(key) = keys.get(&(site.ordinal, site.caller.as_deref())) {
                    site.ordinal = *key;
                }
            }
        }
        sites
    }

    /// AST ordinals stay intact in the blob. When several facts occupy one
    /// ordinal, their bound keys must also distinguish source span and caller.
    /// Singleton references retain their historical key; collision suffixes
    /// hash only stable, path-free source facts, never map order or rowids.
    pub(crate) fn reference_keys(&self) -> Vec<u32> {
        let mut groups: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        for (position, reference) in self.refs.iter().enumerate() {
            groups.entry(reference.ordinal).or_default().push(position);
        }
        let mut used = groups.keys().copied().collect::<BTreeSet<_>>();
        let mut keys = self.refs.iter().map(|r| r.ordinal).collect::<Vec<_>>();
        for (ordinal, positions) in groups {
            if positions.len() < 2 {
                continue;
            }
            let calls = positions
                .iter()
                .copied()
                .filter(|p| self.refs[*p].kind == BlobRefKind::Call)
                .collect::<Vec<_>>();
            let primary = if calls.len() == 1 {
                calls[0]
            } else {
                positions[0]
            };
            for position in positions {
                if position == primary {
                    continue;
                }
                let reference = &self.refs[position];
                let bytes = serde_json::to_vec(&(
                    "bound-reference-v2",
                    ordinal,
                    reference.kind,
                    reference.byte_start,
                    reference.byte_end,
                    &reference.caller_symbol,
                    &reference.full_ref,
                    &reference.module_path,
                    &reference.local_name,
                    &reference.requested_name,
                ))
                .expect("source facts serialize");
                let hash = blake3::hash(&bytes);
                let mut key =
                    u32::from_le_bytes(hash.as_bytes()[..4].try_into().expect("four hash bytes"));
                while !used.insert(key) {
                    key = key.wrapping_add(1);
                }
                keys[position] = key;
            }
        }
        keys
    }

    pub(super) fn file_data(&self, path: &str) -> Result<FileCallData, ManifestJoinError> {
        let lang = language_id(&self.language)
            .ok_or_else(|| ManifestJoinError::UnsupportedLanguage(self.language.clone()))?;
        let mut calls_by_symbol: HashMap<String, Vec<callgraph::CallSite>> = HashMap::new();
        let mut value_refs_by_symbol: HashMap<String, Vec<callgraph::CallSite>> = HashMap::new();
        let mut macro_body_calls_by_macro: HashMap<String, Vec<callgraph::CallSite>> =
            HashMap::new();
        let mut macro_mentions_by_symbol: HashMap<String, Vec<callgraph::CallSite>> =
            HashMap::new();
        for raw in &self.refs {
            let map = match raw.kind {
                BlobRefKind::Call
                    if raw
                        .caller_symbol
                        .as_deref()
                        .is_some_and(|s| s.ends_with('!')) =>
                {
                    &mut macro_body_calls_by_macro
                }
                BlobRefKind::Call | BlobRefKind::Construction => &mut calls_by_symbol,
                BlobRefKind::ValueRef | BlobRefKind::CallbackRef => &mut value_refs_by_symbol,
                BlobRefKind::MacroMention => &mut macro_mentions_by_symbol,
                _ => continue,
            };
            let Some(caller) = &raw.caller_symbol else {
                continue;
            };
            map.entry(bound_name(caller, path))
                .or_default()
                .push(callgraph::CallSite {
                    kind: match raw.kind {
                        BlobRefKind::Construction => callgraph::CallSiteKind::Construction,
                        BlobRefKind::CallbackRef => callgraph::CallSiteKind::CallbackReference,
                        _ => callgraph::CallSiteKind::Call,
                    },
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
                    is_test: symbol.is_test,
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
                    entry_point_attribute: self
                        .entry_point_symbols
                        .contains(&symbol.scoped_name)
                        .then(|| "initializer_or_attribute".to_string()),
                    program_start: symbol.program_start,
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
            macro_body_calls_by_macro,
            macro_mentions_by_symbol,
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
            python_bindings: self.python_bindings.clone(),
            lang,
        })
    }

    fn bind_with_dependencies(
        &self,
        path: &str,
        facts: &FactPaths<'_>,
        cached: Option<&BTreeMap<u32, BTreeSet<String>>>,
    ) -> Result<super::FileExtract, ManifestJoinError> {
        let data = self.file_data(path)?;
        // A node id names the file, the scoped name, and which symbol of that
        // name it is (overloads share one). It leaves out the AST position, so
        // an edit that only moves code keeps every id, and resolutions that
        // recorded a node answer stay valid.
        let mut occurrences = HashMap::<String, usize>::new();
        let nodes = self
            .symbols
            .iter()
            .map(|symbol| {
                let scoped_name = bound_name(&symbol.scoped_name, path);
                let occurrence = occurrences.entry(scoped_name.clone()).or_default();
                let id = format!("{path}:{occurrence}:{scoped_name}");
                *occurrence += 1;
                super::NodeRecord {
                    id,
                    name: bound_name(&symbol.name, path),
                    scoped_name,
                    kind: symbol.kind.clone(),
                }
            })
            .collect::<Vec<_>>();
        let abs = facts.root.join(path);
        let mut raw_refs = Vec::new();
        // A module path's dependencies are computed once per file and reused
        // by every reference that names it (imports also produce re-export
        // rows). The result is a function of the file, the module path and
        // the checkout, and every read the computation makes is recorded for
        // this file the first time, so a reuse records nothing new. Each
        // computation keeps its own memo: a memo shared between module paths
        // returns cached targets without the candidate paths the dependency
        // set lists.
        let mut module_dependencies = HashMap::new();
        for (position, (raw, reference_key)) in
            self.refs.iter().zip(self.reference_keys()).enumerate()
        {
            let position = u32::try_from(position).expect("reference vector fits u32");
            let dependencies = if let Some(dependencies) =
                cached.and_then(|cache| cache.get(&position))
            {
                dependencies.clone()
            } else if raw.kind == BlobRefKind::Module {
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
                module_dependencies
                    .entry(module.clone())
                    .or_insert_with(|| super::module_dependencies(facts.root, &abs, module, facts))
                    .clone()
            } else {
                BTreeSet::new()
            };
            let caller_symbol = raw
                .caller_symbol
                .as_ref()
                .map(|name| bound_name(name, path));
            raw_refs.push(super::RawRef {
                ref_id: format!("{path}:{reference_key}:{:?}", raw.kind),
                caller_symbol,
                caller_file: path.to_string(),
                kind: match raw.kind {
                    BlobRefKind::Call
                        if raw
                            .caller_symbol
                            .as_deref()
                            .is_some_and(|s| s.ends_with('!')) =>
                    {
                        super::MACRO_BODY_CALL_REF_KIND
                    }
                    BlobRefKind::Call => "call",
                    BlobRefKind::Construction => "construction",
                    BlobRefKind::CallbackRef => "callback_ref",
                    BlobRefKind::ValueRef => "value_ref",
                    BlobRefKind::Import => "import",
                    BlobRefKind::Module => "module",
                    BlobRefKind::Reexport => "reexport",
                    BlobRefKind::ExportAlias => "export_alias",
                    BlobRefKind::MacroMention => super::MACRO_MENTION_REF_KIND,
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
            lang: data.lang,
            data,
            nodes,
            raw_refs,
        })
    }
}

/// The receiver type names of one file that method-call resolution may
/// look up through ordinary Rust path binding: declared types and their bases,
/// method owners and traits, field and return types, and each call site's
/// receiver with any field or return-value wrapper removed.
pub(super) fn dispatch_type_names(parse: &ParseBlob) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for ty in &parse.dispatch.types {
        names.insert(ty.name.clone());
        names.extend(ty.bases.iter().cloned());
    }
    for method in &parse.dispatch.methods {
        names.insert(method.owner.clone());
        names.extend(method.trait_name.iter().cloned());
    }
    names.extend(parse.dispatch.fields.iter().map(|f| f.ty.clone()));
    names.extend(parse.dispatch.returns.iter().map(|r| r.ty.clone()));
    for site in &parse.dispatch.sites {
        if let Some(receiver) = &site.receiver {
            // A self call in a macro template names its target already;
            // there is no type name to bind.
            if receiver.starts_with(dispatch::TEMPLATE_SELF_RECEIVER) {
                continue;
            }
            let mut name = receiver.as_str();
            while let Some(expression) = name.strip_prefix("@field:") {
                name = expression
                    .rsplit_once('|')
                    .map(|(base, _)| base)
                    .unwrap_or(expression);
            }
            names.insert(name.strip_prefix("@return:").unwrap_or(name).to_string());
        }
    }
    names
}

/// Bind receiver names through ordinary language import resolution. Rust names
/// also retain lexical call contexts; different bindings remain ambiguous.
pub(super) fn resolve_dispatch_type_names<I: super::ResolverIndex>(
    index: &I,
    file: &str,
    parse: &ParseBlob,
    data: &FileCallData,
) -> BTreeMap<String, (String, String)> {
    if parse.language != "rust" {
        let mut targets = BTreeMap::new();
        for name in dispatch_type_names(parse) {
            if name.starts_with('@')
                || !parse.imports.iter().any(|import| {
                    !import.type_only
                        && (import.default_import.as_deref() == Some(&name)
                            || import
                                .names
                                .iter()
                                .any(|spec| crate::imports::specifier_local_name(spec) == name))
                })
            {
                continue;
            }
            if let Some((_, target_file, symbol)) =
                super::resolve_js_ts_target(index, file, &name, &name, data)
            {
                if super::ResolverIndex::node_for_symbol(index, &target_file, &symbol).is_some() {
                    targets.insert(name, (target_file, symbol));
                }
            }
        }
        return targets;
    }
    let mut names = dispatch_type_names(parse);
    for import in &data.import_block.imports {
        names.extend(callgraph::rust_import_local_names(import));
    }
    let references = parse
        .refs
        .iter()
        .filter(|raw| matches!(raw.kind, BlobRefKind::Call | BlobRefKind::Construction))
        .map(|raw| (raw.ordinal, raw))
        .collect::<HashMap<_, _>>();
    // Path binding depends on the enclosing scope and the visible import prefix,
    // not on a call's identity. Resolve repeated uses in the same context once,
    // while retaining distinct inline modules, impl owners and import scopes.
    fn context_key<'a>(raw: &'a BlobRef, data: &FileCallData) -> (Option<(&'a str, bool)>, usize) {
        (
            raw.caller_symbol.as_deref().map(|caller| {
                (
                    caller.rsplit_once("::").map_or("", |(scope, _)| scope),
                    data.symbol_metadata
                        .get(caller)
                        .is_some_and(|meta| meta.kind == SymbolKind::Method),
                )
            }),
            data.import_block
                .imports
                .partition_point(|import| import.byte_range.start <= raw.byte_start),
        )
    }
    let mut contexts = HashMap::<_, BTreeMap<_, &BlobRef>>::new();
    for raw in references.values() {
        if let Some(name) = raw.full_ref.as_ref().filter(|name| names.contains(*name)) {
            contexts
                .entry(name.as_str())
                .or_default()
                .insert(context_key(raw, data), raw);
        }
    }
    for site in &parse.dispatch.sites {
        let (Some(receiver), Some(reference)) = (&site.receiver, references.get(&site.ordinal))
        else {
            continue;
        };
        let mut name = receiver.as_str();
        while let Some(expression) = name.strip_prefix("@field:") {
            name = expression
                .rsplit_once('|')
                .map(|(base, _)| base)
                .unwrap_or(expression);
        }
        name = name.strip_prefix("@return:").unwrap_or(name);
        contexts
            .entry(name)
            .or_default()
            .insert(context_key(reference, data), reference);
    }
    let mut targets = BTreeMap::new();
    for name in names {
        let short = name.rsplit("::").next().unwrap_or(&name);
        let sites = contexts
            .get(name.as_str())
            .map(|refs| refs.values().copied().map(Some).collect::<Vec<_>>())
            .unwrap_or_else(|| vec![None]);
        let mut bindings = BTreeSet::new();
        let mut incomplete = false;
        for site in sites {
            let raw = super::RawRef {
                ref_id: String::new(),
                caller_symbol: site.and_then(|raw| raw.caller_symbol.clone()),
                caller_file: file.to_string(),
                kind: "call".into(),
                short_name: Some(short.into()),
                full_ref: Some(name.clone()),
                module_path: None,
                import_kind: None,
                local_name: None,
                requested_name: None,
                namespace_alias: None,
                wildcard: false,
                line: site.map_or(0, |raw| raw.line),
                byte_start: site.map_or(0, |raw| raw.byte_start),
                byte_end: site.map_or(0, |raw| raw.byte_end),
                dependencies: BTreeSet::new(),
            };
            if let Some((_, target_file, symbol)) =
                super::resolve_rust_target(index, file, &name, short, data, &raw)
            {
                if super::ResolverIndex::node_for_symbol(index, &target_file, &symbol).is_some() {
                    bindings.insert((target_file, symbol));
                } else {
                    incomplete = true;
                }
            } else {
                incomplete = true;
            }
        }
        if !incomplete && bindings.len() == 1 {
            targets.insert(name, bindings.into_iter().next().unwrap());
        }
    }
    targets
}

/// What resolving one Rust file's receiver type names consulted: every
/// resolver index answer it read, and every path whose presence it probed.
/// While each recorded answer is unchanged and no probed path was added or
/// removed, the file's receiver type targets are unchanged too, because the
/// file's own parse blob is the only other input.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct DispatchTypeTargetTrace {
    queries: Vec<(ViewSurfaceQuery, String)>,
    probes: BTreeSet<String>,
    /// Every checkout read with its answer, when answers were recorded.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) answers: super::fact_answers::FactAnswers,
}

impl DispatchTypeTargetTrace {
    /// Paths and index domains whose change can alter a recorded answer or
    /// probe, so the owner knows which traces to check after a manifest diff.
    pub(crate) fn dependencies(&self) -> BTreeSet<String> {
        self.queries
            .iter()
            .flat_map(|(query, _)| query.dependencies())
            .chain(self.probes.iter().cloned())
            .collect()
    }

    /// Every path whose presence this resolution probed.
    pub(crate) fn probes(&self) -> &BTreeSet<String> {
        &self.probes
    }

    /// True when a path this resolution probed was added or removed.
    pub(crate) fn probes_any(&self, paths: &BTreeSet<String>) -> bool {
        !self.probes.is_disjoint(paths)
    }

    /// True when every recorded answer that depends on one of `changed` is
    /// unchanged in `index`. An answer depending on nothing in `changed` reads
    /// only index entries that are the same as when it was recorded. Traces
    /// share queries (a crate's module parents, for one), so answers are kept
    /// in `answers` for the next trace checked against the same index. The
    /// memo owns its queries, so the traces checked need not outlive it.
    pub(crate) fn replays(
        &self,
        index: &impl super::ResolverIndex,
        changed: &BTreeSet<String>,
        answers: &mut BTreeMap<ViewSurfaceQuery, String>,
    ) -> bool {
        self.queries.iter().all(|(query, expected)| {
            !query.depends_on_any(changed) || answer_is(query, expected, index, answers)
        })
    }

    /// True when every recorded surface answer is unchanged in `index`.
    pub(crate) fn all_answers_hold<'q>(
        &'q self,
        index: &impl super::ResolverIndex,
        answers: &mut BTreeMap<&'q ViewSurfaceQuery, String>,
    ) -> bool {
        self.queries.iter().all(|(query, expected)| {
            *answers.entry(query).or_insert_with(|| query.answer(index)) == *expected
        })
    }
}

/// One Rust file's receiver type targets, keyed by the type name as written.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DispatchTypeTargets {
    pub targets: BTreeMap<String, (String, String)>,
    pub trace: DispatchTypeTargetTrace,
}

#[cfg(test)]
#[path = "../../tests/integration/join_manifest_test.rs"]
mod disk_facts_tests;

/// Bound reference dependencies and resolver probes are generation-specific, unlike
/// parse blobs. The owner must invalidate these whenever a probed path changes.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ViewBindingDependencies {
    // AST ordinals can collide for structural references. Vector positions are
    // unique and stable within the immutable caller blob used to validate reuse.
    pub references: BTreeMap<u32, BTreeSet<String>>,
    pub dependencies: BTreeSet<String>,
    #[serde(default)]
    pub consulted_facts: BTreeSet<(String, String)>,
    #[serde(default)]
    pub unattributed: bool,
    #[serde(default)]
    binding_facts: ConfigConsultations,
    #[serde(default)]
    resolution_facts: ConfigConsultations,
    binding_probes: BTreeSet<String>,
    resolved_dependencies: BTreeSet<String>,
    surface_queries: Vec<(ViewSurfaceQuery, String)>,
    /// Every checkout read binding made, with its answer, when answers were
    /// recorded (see [`super::fact_answers`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) binding_answers: super::fact_answers::FactAnswers,
    /// Likewise for resolution's reads beyond binding's.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) resolution_answers: super::fact_answers::FactAnswers,
    #[serde(skip)]
    surface: Option<ViewFileSurface>,
}

impl ViewBindingDependencies {
    /// This binding without its bound file surface, which is not stored:
    /// binding computes it again from the file's own parse.
    pub(crate) fn without_surface(&self) -> Self {
        Self {
            surface: None,
            ..self.clone()
        }
    }

    /// Drops the bound file surface in place, as a stored resolution never
    /// carries it. Resolution never reads it; whoever compares surfaces
    /// reads them from the bound file.
    pub(crate) fn clear_surface(&mut self) {
        self.surface = None;
    }

    /// Whether binding gave this file the surface `other` recorded, so no
    /// other file's resolution can read a different answer from it.
    pub(crate) fn same_surface(&self, other: &Self) -> bool {
        self.surface == other.surface
    }

    /// True when every surface query this resolution asked that depends on
    /// one of `changed` still has the answer it recorded in `index`. A query
    /// depending on nothing in `changed` reads only index entries that are
    /// unchanged. Answers are kept in `answers` for the next file replayed
    /// against the same index. The memo owns its queries, so the resolutions
    /// replayed need not outlive it.
    pub(crate) fn surface_replays(
        &self,
        index: &impl super::ResolverIndex,
        changed: &BTreeSet<String>,
        answers: &mut BTreeMap<ViewSurfaceQuery, String>,
    ) -> bool {
        self.surface_queries.iter().all(|(query, expected)| {
            !query.depends_on_any(changed) || answer_is(query, expected, index, answers)
        })
    }

    /// Carries a resolution over from `old`, whose recorded answers replay,
    /// onto this file's new binding.
    pub(crate) fn reuse_resolution(&mut self, old: &Self) {
        self.resolved_dependencies = old.resolved_dependencies.clone();
        self.surface_queries = old.surface_queries.clone();
        self.resolution_facts = old.resolution_facts.clone();
        self.resolution_answers = old.resolution_answers.clone();
        finish_binding(self);
    }

    /// The input keys binding recorded, before resolution added its own:
    /// what [`finish_binding`] gives a freshly bound file.
    pub(crate) fn binding_keys(&self) -> BTreeSet<String> {
        self.references
            .values()
            .flatten()
            .cloned()
            .chain(self.binding_probes.iter().cloned())
            .collect()
    }

    /// Every checkout read binding and resolution made, with its answer.
    pub(crate) fn answers(
        &self,
    ) -> impl Iterator<
        Item = (
            &super::fact_answers::FactQuestion,
            &super::fact_answers::FactAnswer,
        ),
    > {
        self.binding_answers
            .iter()
            .chain(self.resolution_answers.iter())
    }

    /// True when every surface query resolution asked still has its
    /// recorded answer in `index`, whatever changed.
    pub(crate) fn all_surface_answers_hold<'q>(
        &'q self,
        index: &impl super::ResolverIndex,
        answers: &mut BTreeMap<&'q ViewSurfaceQuery, String>,
    ) -> bool {
        self.surface_queries.iter().all(|(query, expected)| {
            *answers.entry(query).or_insert_with(|| query.answer(index)) == *expected
        })
    }
}

/// Inputs that determine a reference's target, excluding call-site identity.
/// Rust qualified imports are visible only after their declaration; the count
/// of visible imports identifies that monotone prefix even across nested uses.
/// Python calls include the selected lexical target or an explicit unresolved binding.
#[derive(Hash, PartialEq, Eq)]
struct ViewResolutionBinding {
    kind: String,
    full_ref: Option<String>,
    short_name: Option<String>,
    visible_rust_imports: usize,
    // Distinguish no lexical binding from a binding that explicitly rejects resolution.
    python_binding: Option<Option<(String, String)>>,
}

impl ViewResolutionBinding {
    fn new(raw: &super::RawRef, caller: &FileCallData) -> Self {
        Self {
            kind: raw.kind.clone(),
            full_ref: raw.full_ref.clone(),
            short_name: raw.short_name.clone(),
            python_binding: if caller.lang == LangId::Python {
                caller
                    .python_bindings
                    .binary_search_by_key(&(raw.byte_start, raw.byte_end), |binding| {
                        (binding.byte_start, binding.byte_end)
                    })
                    .ok()
                    .map(|position| caller.python_bindings[position].target.clone())
            } else {
                None
            },
            visible_rust_imports: if caller.lang == LangId::Rust {
                caller
                    .import_block
                    .imports
                    .partition_point(|import| import.byte_range.start <= raw.byte_start)
            } else {
                0
            },
        }
    }
}

/// Compact, deterministic snapshot of one file's resolver index. It deliberately
/// excludes source, AST nodes, and call sites: only callers actually resolved need
/// those payloads. Membership probes invalidate module/reexport targets along with
/// bindings; source changes invalidate the snapshot through the manifest diff.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ViewFileSurface {
    language: String,
    exports: BTreeSet<String>,
    default_export: Option<String>,
    export_aliases: BTreeMap<String, String>,
    node_by_scoped: BTreeMap<String, String>,
    node_by_bare: BTreeMap<String, String>,
    node_kind_by_id: BTreeMap<String, String>,
    module_targets: BTreeMap<String, Option<String>>,
    declared_module_targets: BTreeMap<String, Option<String>>,
    macro_use_module_targets: BTreeSet<String>,
    reexports: Vec<(Option<String>, BTreeMap<String, String>, bool)>,
}

impl ViewFileSurface {
    fn capture(language: &str, index: &super::DbFileIndex) -> Self {
        Self {
            language: language.into(),
            exports: index.exports.iter().cloned().collect(),
            default_export: index.default_export.clone(),
            export_aliases: index.export_aliases.clone().into_iter().collect(),
            node_by_scoped: index.node_by_scoped.clone().into_iter().collect(),
            node_by_bare: index.node_by_bare.clone().into_iter().collect(),
            node_kind_by_id: index.node_kind_by_id.clone().into_iter().collect(),
            module_targets: index.module_targets.clone().into_iter().collect(),
            declared_module_targets: index.declared_module_targets.clone().into_iter().collect(),
            macro_use_module_targets: index.macro_use_module_targets.clone(),
            reexports: index
                .reexports
                .iter()
                .map(|r| {
                    (
                        r.target_file.clone(),
                        r.named.clone().into_iter().collect(),
                        r.wildcard,
                    )
                })
                .collect(),
        }
    }
}

/// Configuration files read by the manifest resolver's workspace/package and
/// tsconfig lookup (callgraph.rs), and Rust crate lookup (callgraph_store).
/// Directory discovery uses a compact membership domain for these names. Their
/// content changes invalidate only callers that consulted changed fields.
pub(crate) fn view_resolution_config(path: &[u8]) -> bool {
    matches!(
        path.rsplit(|byte| *byte == b'/').next(),
        Some(
            b"package.json" | b"tsconfig.json" | b"pnpm-workspace.yaml" | b"Cargo.toml" | b"go.mod"
        )
    )
}

/// Field identities are generation-owned. Raw-read paths are transient validation
/// evidence, cleared after classifying a caller so memo-hit timing is not persisted.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
struct ConfigConsultations {
    facts: BTreeSet<(String, String)>,
    reads: BTreeSet<String>,
    unattributed: bool,
}
impl ConfigConsultations {
    fn extend(&mut self, other: &Self) {
        self.facts.extend(other.facts.iter().cloned());
        self.reads.extend(other.reads.iter().cloned());
        self.unattributed |= other.unattributed;
    }
    fn unattributed(&self) -> bool {
        self.unattributed
            || self
                .reads
                .iter()
                .any(|path| !self.facts.iter().any(|(input, _)| input == path))
    }
}

#[derive(Clone, Default)]
struct ConsultationTrace {
    config: ConfigConsultations,
    probes: BTreeSet<String>,
    answers: super::fact_answers::FactAnswers,
}
type ConsultationMemoKey = (std::path::PathBuf, String, String);

/// Memo answers and their provenance belong to one immutable manifest join.
/// Replaying provenance on hits keeps missing candidates and facts attributable.
pub(crate) struct ViewBindingFacts<'a> {
    inner: Rc<dyn ProjectFacts + 'a>,
    probes: std::cell::RefCell<BTreeSet<String>>,
    consultations: std::cell::RefCell<ConfigConsultations>,
    memo_stack: std::cell::RefCell<Vec<(ConsultationMemoKey, ConsultationTrace)>>,
    memo_traces: std::cell::RefCell<BTreeMap<ConsultationMemoKey, ConsultationTrace>>,
    workspace_packages:
        std::cell::RefCell<BTreeMap<(std::path::PathBuf, String), Option<std::path::PathBuf>>>,
    workspace_members:
        std::cell::RefCell<BTreeMap<std::path::PathBuf, Arc<Vec<std::path::PathBuf>>>>,
    canonical_cache: std::cell::RefCell<HashMap<Vec<u8>, Option<Vec<u8>>>>,
    file_cache: std::cell::RefCell<HashMap<Vec<u8>, bool>>,
    config_cache: std::cell::RefCell<HashMap<Vec<u8>, Option<Arc<[u8]>>>>,
    directory_cache: std::cell::RefCell<HashMap<Vec<u8>, Vec<super::facts::DirEntry>>>,
    /// Nonzero while a repository-wide structure is built: its reads are
    /// not the caller's inputs.
    paused: std::cell::Cell<u32>,
    /// Whether every read is also kept as a typed question with its answer
    /// (see [`super::fact_answers`]).
    typed: bool,
    answers: std::cell::RefCell<super::fact_answers::FactAnswers>,
    /// Every answer this recorder has computed. The checkout does not
    /// change while a recorder lives, so a read asked again, by any file,
    /// reuses its answer rather than reading (or parsing a config file)
    /// again.
    answer_cache: std::cell::RefCell<
        HashMap<super::fact_answers::FactQuestion, super::fact_answers::FactAnswer>,
    >,
}

impl<'a> ViewBindingFacts<'a> {
    pub(crate) fn new(inner: Rc<dyn ProjectFacts + 'a>) -> Self {
        Self {
            inner,
            probes: Default::default(),
            consultations: Default::default(),
            memo_stack: Default::default(),
            memo_traces: Default::default(),
            workspace_packages: Default::default(),
            workspace_members: Default::default(),
            canonical_cache: Default::default(),
            file_cache: Default::default(),
            config_cache: Default::default(),
            directory_cache: Default::default(),
            paused: Default::default(),
            typed: false,
            answers: Default::default(),
            answer_cache: Default::default(),
        }
    }

    /// Like [`Self::new`], and also keeps every read as a typed question
    /// with its answer, which [`Self::take_answers`] hands over.
    pub(crate) fn typed(inner: Rc<dyn ProjectFacts + 'a>) -> Self {
        Self {
            typed: true,
            ..Self::new(inner)
        }
    }

    /// The questions read since the last call, with their answers.
    pub(crate) fn take_answers(&self) -> super::fact_answers::FactAnswers {
        std::mem::take(&mut *self.answers.borrow_mut())
    }

    fn answered(
        &self,
        question: super::fact_answers::FactQuestion,
        answer: impl FnOnce() -> super::fact_answers::FactAnswer,
    ) {
        if !self.typed || self.paused.get() > 0 {
            return;
        }
        let cached = self.answer_cache.borrow().get(&question).cloned();
        let answer = cached.unwrap_or_else(|| {
            let answer = answer();
            self.answer_cache
                .borrow_mut()
                .insert(question.clone(), answer.clone());
            answer
        });
        for (_, trace) in self.memo_stack.borrow_mut().iter_mut() {
            trace.answers.insert(question.clone(), answer.clone());
        }
        self.answers.borrow_mut().insert(question, answer);
    }

    fn file_fact(&self, rel: &[u8]) -> bool {
        if let Some(value) = self.file_cache.borrow().get(rel) {
            return *value;
        }
        let value = self.inner.is_file(rel);
        self.file_cache.borrow_mut().insert(rel.to_vec(), value);
        value
    }

    fn record(&self, path: &[u8]) {
        if self.paused.get() > 0 {
            return;
        }
        // Directory discovery may probe thousands of missing package manifests.
        // One membership domain rechecks those bindings on config add/remove;
        // individual config content changes use the field consultations instead.
        // A typed recorder (the data layer) also keeps every probe, found or
        // missing, as a question with its answer, so a config file added or
        // removed reaches exactly the files whose answer it changes; it keeps
        // a found config file as a path like any other.
        if !self.typed && view_resolution_config(path) {
            self.record(VIEW_CONFIG_MEMBERSHIP_DOMAIN.as_bytes());
            return;
        }
        if let Ok(path) = std::str::from_utf8(path) {
            let mut parts = Vec::new();
            for part in path.split('/') {
                match part {
                    "" | "." => {}
                    ".." => {
                        parts.pop();
                    }
                    _ => parts.push(part),
                }
            }
            let path = parts.join("/");
            self.probes.borrow_mut().insert(path.clone());
            for (_, trace) in self.memo_stack.borrow_mut().iter_mut() {
                trace.probes.insert(path.clone());
            }
        }
    }

    fn take_config(&self) -> ConfigConsultations {
        let mut result = std::mem::take(&mut *self.consultations.borrow_mut());
        result.unattributed = result.unattributed();
        result.reads.clear();
        result
    }

    fn config_event(&self, path: &[u8], name: Option<&str>) {
        if self.paused.get() > 0 {
            return;
        }
        let Ok(path) = std::str::from_utf8(path) else {
            self.consultations.borrow_mut().unattributed = true;
            return;
        };
        let mut event = ConfigConsultations::default();
        if let Some(name) = name {
            event.facts.insert((path.into(), name.into()));
        } else {
            event.reads.insert(path.into());
        }
        self.consultations.borrow_mut().extend(&event);
        for (_, trace) in self.memo_stack.borrow_mut().iter_mut() {
            trace.config.extend(&event);
        }
    }

    fn take(&self) -> BTreeSet<String> {
        std::mem::take(&mut *self.probes.borrow_mut())
    }
    /// A configuration file's bytes, read once and cached.
    fn config_value(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
        let cached = self.config_cache.borrow().get(rel).cloned();
        cached.unwrap_or_else(|| {
            let value = self.inner.config_bytes(rel);
            self.config_cache
                .borrow_mut()
                .insert(rel.to_vec(), value.clone());
            value
        })
    }
}

impl ProjectFacts for ViewBindingFacts<'_> {
    fn records_config_facts(&self) -> bool {
        true
    }
    fn records_answers(&self) -> bool {
        self.typed
    }
    fn config_fact(&self, rel: &[u8], name: &str) {
        self.record(rel);
        if !self.typed && matches!(name, "workspaces" | "packages") {
            // The members a workspace list names depend on which package
            // manifests exist under its globs, not only on this field. A
            // typed recorder keys each package name a reader looked up
            // instead ([`js_package_key`]).
            self.record(VIEW_CONFIG_MEMBERSHIP_DOMAIN.as_bytes());
        }
        self.config_event(rel, Some(name));
        // The resolver records a field each time it uses one, though it reads
        // the file's bytes only the first time it parses it, so the field is
        // recorded with its value here, for every file that uses it.
        self.answered(
            super::fact_answers::FactQuestion::ConfigField(rel.to_vec(), name.to_string()),
            || {
                super::fact_answers::config_field_answer(
                    rel,
                    self.config_value(rel).as_deref(),
                    name,
                )
            },
        );
    }
    fn memo_start(&self, path: &Path, kind: &str, name: &str) {
        self.memo_stack.borrow_mut().push((
            (path.into(), kind.into(), name.into()),
            ConsultationTrace::default(),
        ));
    }
    fn memo_finish(&self, path: &Path, kind: &str, name: &str) {
        let (key, trace) = self
            .memo_stack
            .borrow_mut()
            .pop()
            .expect("balanced resolver memo recording");
        debug_assert_eq!(key, (path.into(), kind.into(), name.into()));
        self.memo_traces.borrow_mut().insert(key, trace);
    }
    fn record_domain(&self, key: &str) {
        self.record(key.as_bytes());
    }
    fn pause_recording(&self) {
        self.paused.set(self.paused.get() + 1);
    }
    fn resume_recording(&self) {
        self.paused.set(self.paused.get().saturating_sub(1));
    }
    fn memo_replay(&self, path: &Path, kind: &str, name: &str) {
        let traces = self.memo_traces.borrow();
        let Some(trace) = traces.get(&(path.into(), kind.into(), name.into())) else {
            self.consultations.borrow_mut().unattributed = true;
            return;
        };
        self.consultations.borrow_mut().extend(&trace.config);
        self.probes
            .borrow_mut()
            .extend(trace.probes.iter().cloned());
        for (_, parent) in self.memo_stack.borrow_mut().iter_mut() {
            parent.config.extend(&trace.config);
            parent.probes.extend(trace.probes.iter().cloned());
        }
        if self.typed && self.paused.get() == 0 {
            for (_, parent) in self.memo_stack.borrow_mut().iter_mut() {
                parent
                    .answers
                    .extend(trace.answers.iter().map(|(q, a)| (q.clone(), a.clone())));
            }
            self.answers
                .borrow_mut()
                .extend(trace.answers.iter().map(|(q, a)| (q.clone(), a.clone())));
        }
    }
    fn workspace_package(&self, root: &Path, name: &str) -> Option<Option<std::path::PathBuf>> {
        self.workspace_packages
            .borrow()
            .get(&(root.into(), name.into()))
            .cloned()
    }
    fn remember_workspace_package(
        &self,
        root: &Path,
        name: &str,
        value: Option<std::path::PathBuf>,
    ) {
        self.workspace_packages
            .borrow_mut()
            .insert((root.into(), name.into()), value);
    }
    fn workspace_members(&self, root: &Path) -> Option<Arc<Vec<std::path::PathBuf>>> {
        self.workspace_members.borrow().get(root).cloned()
    }
    fn remember_workspace_members(&self, root: &Path, value: Arc<Vec<std::path::PathBuf>>) {
        self.workspace_members
            .borrow_mut()
            .insert(root.into(), value);
    }
    fn is_file(&self, rel: &[u8]) -> bool {
        let is_file = self.file_fact(rel);
        if is_file {
            self.record(rel);
        }
        self.answered(
            super::fact_answers::FactQuestion::IsFile(rel.to_vec()),
            || super::fact_answers::FactAnswer::Bool(is_file),
        );
        is_file
    }
    fn is_dir(&self, rel: &[u8]) -> bool {
        let is_dir = self.inner.is_dir(rel);
        self.answered(
            super::fact_answers::FactQuestion::IsDir(rel.to_vec()),
            || super::fact_answers::FactAnswer::Bool(is_dir),
        );
        is_dir
    }
    fn config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
        self.consultations.borrow_mut().unattributed = true;
        for (_, trace) in self.memo_stack.borrow_mut().iter_mut() {
            trace.config.unattributed = true;
        }
        let value = self.attributed_config_bytes(rel);
        // Read without naming fields: the whole file is the input.
        self.answered(
            super::fact_answers::FactQuestion::Config(rel.to_vec()),
            || super::fact_answers::config_answer(value.as_deref()),
        );
        value
    }
    fn attributed_config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
        self.config_event(rel, None);
        self.record(rel);
        // The fields the resolver reads from these bytes are recorded, with
        // their values, as it uses them (`config_fact`).
        self.config_value(rel)
    }
    fn symlink_target(&self, rel: &[u8]) -> Option<&[u8]> {
        let target = self.inner.symlink_target(rel);
        self.answered(
            super::fact_answers::FactQuestion::Link(rel.to_vec()),
            || super::fact_answers::FactAnswer::Path(target.map(<[u8]>::to_vec)),
        );
        target
    }
    fn canonical(&self, rel: &[u8]) -> Option<Vec<u8>> {
        // FactPaths canonicalizes before testing existence, so misses must be
        // recorded here as well as in is_file (not only after canonicalization).
        let cached = self.canonical_cache.borrow().get(rel).cloned();
        let canonical = cached.unwrap_or_else(|| {
            let value = self.inner.canonical(rel);
            self.canonical_cache
                .borrow_mut()
                .insert(rel.to_vec(), value.clone());
            value
        });
        // Existing directory probes are workspace-discovery implementation detail.
        // Config add/remove rechecks that discovery through its membership domain;
        // source-file probes and misses remain caller-specific dependencies.
        if canonical.as_ref().is_none_or(|path| self.file_fact(path)) {
            self.record(rel);
        }
        self.answered(
            super::fact_answers::FactQuestion::Canonical(rel.to_vec()),
            || super::fact_answers::FactAnswer::Path(canonical.clone()),
        );
        canonical
    }
    fn list_dir(&self, rel: &[u8]) -> Vec<super::facts::DirEntry> {
        let cached = self.directory_cache.borrow().get(rel).cloned();
        let value = cached.unwrap_or_else(|| {
            let value = self.inner.list_dir(rel);
            self.directory_cache
                .borrow_mut()
                .insert(rel.to_vec(), value.clone());
            value
        });
        self.answered(
            super::fact_answers::FactQuestion::ListDir(rel.to_vec()),
            || super::fact_answers::listing_answer(&value),
        );
        value
    }
}

/// Binds one file's parse against its checkout's path facts. Returns the
/// file's resolver surface, and its binding: which paths each reference's
/// binding probed, and the paths and configuration fields binding consulted.
/// With `cache`, reference dependencies come from that earlier binding of the
/// same parse instead of being recorded again.
pub(crate) fn bind_file(
    rel: &str,
    blob: &ParseBlob,
    facts: &ViewBindingFacts<'_>,
    cache: Option<&ViewBindingDependencies>,
) -> Result<(super::DbFileIndex, ViewBindingDependencies), ManifestJoinError> {
    let root = Path::new("/");
    let paths = FactPaths { root, facts };
    facts.take();
    facts.take_config();
    facts.take_answers();
    let extract = blob.bind_with_dependencies(rel, &paths, cache.map(|cache| &cache.references))?;
    let file_index = super::DbFileIndex::from_extract(root, &extract, &paths);
    let mut binding = cache.cloned().unwrap_or_default();
    binding.surface = Some(ViewFileSurface::capture(&blob.language, &file_index));
    if cache.is_none() {
        binding.references = blob
            .refs
            .iter()
            .zip(&extract.raw_refs)
            .enumerate()
            .map(|(position, (_, bound))| {
                (
                    u32::try_from(position).expect("reference vector fits u32"),
                    bound.dependencies.clone(),
                )
            })
            .collect();
        binding.binding_probes = facts.take();
        binding.binding_facts = facts.take_config();
        binding.binding_answers = facts.take_answers();
        binding.dependencies = binding.references.values().flatten().cloned().collect();
        binding
            .dependencies
            .extend(binding.binding_probes.iter().cloned());
    }
    Ok((file_index, binding))
}

/// Resolves every reference of one caller against `index`, adding one row per
/// reference to `rows`. Records in `binding` the surface queries resolution
/// asked with their answers, the paths it consulted beyond binding's, and the
/// configuration fields it read. Returns how many distinct bindings it
/// resolved (references sharing a binding are resolved once).
#[expect(
    clippy::too_many_arguments,
    reason = "one caller's resolution needs every one of these inputs"
)]
pub(crate) fn resolve_file(
    caller: &str,
    key: &BlobKey,
    blob: &ParseBlob,
    binding: &mut ViewBindingDependencies,
    index: &super::ProjectIndex<'_>,
    facts: &ViewBindingFacts<'_>,
    mut resolution_order: Option<&mut Vec<CallerRefKey>>,
    rows: &mut BTreeSet<DerivedRow>,
) -> Result<usize, ManifestJoinError> {
    let root = Path::new("/");
    let paths = FactPaths { root, facts };
    let basis: BTreeSet<String> = binding
        .references
        .values()
        .flatten()
        .chain(binding.binding_probes.iter())
        .cloned()
        .collect();
    facts.take_answers();
    let extract = blob.bind_with_dependencies(caller, &paths, Some(&binding.references))?;
    binding.resolution_answers = facts.take_answers();
    let mut caller_work = blob
        .refs
        .iter()
        .zip(extract.raw_refs)
        .zip(blob.reference_keys())
        .map(|((raw, bound), reference_key)| {
            (
                CallerRefKey {
                    caller_blob_key: key.clone(),
                    ref_ordinal: reference_key,
                    caller_path: caller.as_bytes().to_vec(),
                },
                (raw.kind, bound),
            )
        })
        .collect::<Vec<_>>();
    caller_work.sort_by(|a, b| (&a.0, a.1 .0).cmp(&(&b.0, b.1 .0)));
    // Scoped and self calls can already have declaration-proven bindings.
    // Let dispatch refine those bindings instead of skipping them in advance.
    let associated_ordinals = blob
        .refs
        .iter()
        .filter(|reference| {
            blob.language == "rust"
                && reference.full_ref.as_deref().is_some_and(|name| {
                    dispatch::associated_reference(name) || name.starts_with("self.")
                })
        })
        .map(|reference| reference.ordinal)
        .collect::<BTreeSet<_>>();
    let receiver_ordinals = blob
        .dispatch
        .sites
        .iter()
        .filter(|site| !associated_ordinals.contains(&site.ordinal))
        .map(|s| s.ordinal)
        .collect::<BTreeSet<_>>();
    let caller_data = &extract.data;
    let surface_index = ViewSurfaceIndex::for_caller(index, caller, caller_data);
    let mut resolutions = HashMap::<ViewResolutionBinding, (Option<String>, Option<String>)>::new();
    let mut caller_queries = BTreeMap::new();
    for (key, (kind, raw)) in caller_work {
        let memo_key = ViewResolutionBinding::new(&raw, caller_data);
        // Dependencies belonging to a call site are not part of the memoized
        // target. Preserve them even when another reference resolved its binding.
        binding.resolved_dependencies.extend(
            raw.dependencies
                .iter()
                .filter(|dependency| !basis.contains(*dependency))
                .cloned(),
        );
        let (target_file, target_symbol) = if receiver_ordinals.contains(&key.ref_ordinal) {
            (None, None)
        } else {
            match resolutions.entry(memo_key) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.get().clone(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    facts.take();
                    facts.take_config();
                    facts.take_answers();
                    let resolved = super::resolve_ref(raw, &surface_index)
                        .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
                    // The memo table is local to each caller. Extending the
                    // binding's facts once keeps its configuration dependencies
                    // associated with that caller even on cache hits.
                    binding.resolution_facts.extend(&facts.take_config());
                    binding.resolution_answers.extend(facts.take_answers());
                    caller_queries.extend(surface_index.take());
                    binding.resolved_dependencies.extend(
                        resolved
                            .dependencies
                            .into_iter()
                            .chain(facts.take())
                            .filter(|dependency| !basis.contains(dependency)),
                    );
                    entry
                        .insert((resolved.target_file, resolved.target_symbol))
                        .clone()
                }
            }
        };
        if let Some(order) = resolution_order.as_deref_mut() {
            order.push(key.clone());
        }
        rows.insert(DerivedRow {
            caller_blob_key: key.caller_blob_key,
            ref_ordinal: key.ref_ordinal,
            caller_path: key.caller_path,
            kind,
            status: if target_file.is_some() {
                ResolutionStatus::Resolved
            } else {
                ResolutionStatus::Unresolved
            },
            target_path: target_file.map(String::into_bytes),
            target_symbol,
        });
    }
    binding.surface_queries = caller_queries.into_iter().collect();
    Ok(resolutions.len())
}

/// Fills a binding's summary fields once binding and resolution are done:
/// every configuration field it consulted, whether any configuration read
/// was opaque, and every path its answers depend on.
pub(crate) fn finish_binding(binding: &mut ViewBindingDependencies) {
    binding.consulted_facts = binding
        .binding_facts
        .facts
        .union(&binding.resolution_facts.facts)
        .cloned()
        .collect();
    binding.unattributed =
        binding.binding_facts.unattributed() || binding.resolution_facts.unattributed();
    binding.dependencies = binding
        .references
        .values()
        .flatten()
        .cloned()
        .chain(binding.binding_probes.iter().cloned())
        .chain(binding.resolved_dependencies.iter().cloned())
        .chain(
            binding
                .surface_queries
                .iter()
                .flat_map(|(query, _)| query.dependencies()),
        )
        .collect();
}

/// The project files the type names of one Rust, TypeScript or JavaScript
/// file resolve to, for method-call resolution, with the surface queries and
/// path probes they consulted.
pub(crate) fn dispatch_type_targets_for(
    file: &str,
    blob: &ParseBlob,
    index: &super::ProjectIndex<'_>,
    facts: &ViewBindingFacts<'_>,
) -> Result<DispatchTypeTargets, ManifestJoinError> {
    // The caller data is the file's own parse; binding consults path facts
    // only for reference dependencies, which are not wanted here.
    let data = blob.file_data(file)?;
    facts.take();
    facts.take_config();
    facts.take_answers();
    let recording = ViewSurfaceIndex::reading_inner(index, &data);
    let targets = resolve_dispatch_type_names(&recording, file, blob, &data);
    let probes = facts.take();
    facts.take_config();
    Ok(DispatchTypeTargets {
        targets,
        trace: DispatchTypeTargetTrace {
            queries: recording.take().into_iter().collect(),
            probes,
            answers: facts.take_answers(),
        },
    })
}

/// Consumer-specific export surface: record the answers the resolver actually
/// used, rather than invalidating every caller when an unrelated export changes.
/// Querying these answers against a new index is cheaper than walking references
/// and is sound only while the caller's immutable parse blob remains unchanged.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) enum ViewSurfaceQuery {
    Language(String),
    Module(String, String),
    MacroUse(String),
    Parent(String),
    Reexports(String),
    Node(String, String),
    Callable(String, String),
    Alias(String, String),
    Export(String, String),
    Default(String),
    Contains(String),
    Crate(String),
    CrateRoot(String),
    Inline(String, Vec<String>, String),
    /// The `.go` files of the package a Go caller looks a symbol up in: its own
    /// directory, or the directory of an imported module path.
    GoPackage(String, Option<String>),
}

fn surface_value(value: &impl Serialize) -> String {
    blake3::hash(&serde_json::to_vec(value).expect("resolver surface serializes"))
        .to_hex()
        .to_string()
}

fn reexport_surface(value: &[super::ReexportIndex]) -> String {
    surface_value(
        &value
            .iter()
            .map(|entry| {
                (
                    &entry.target_file,
                    entry.named.iter().collect::<BTreeMap<_, _>>(),
                    entry.wildcard,
                )
            })
            .collect::<Vec<_>>(),
    )
}

/// Recorded by today's views for every config file a resolution found, and
/// by every JavaScript workspace member list; set when any config file is
/// added or removed.
pub(crate) const VIEW_CONFIG_MEMBERSHIP_DOMAIN: &str = "\0view:config-membership";
/// The prefix of the data layer's JavaScript workspace package keys
/// ([`js_package_key`]). The data layer keeps every other config probe as a
/// path and a recorded answer, so it does not use
/// [`VIEW_CONFIG_MEMBERSHIP_DOMAIN`].
pub(crate) const VIEW_JS_WORKSPACES_DOMAIN: &str = "\0view:js-workspaces";

/// The key a reader records for looking up the package `name` among the
/// members of the JavaScript workspace at `root` (checkout-relative, `/`
/// separators), found or not. Finding it walks the workspace's member folders
/// and reads each member's `package.json` name; recording those reads in
/// every file with a bare import cost a monorepo about 1,300 recorded reads
/// per file. The key replaces them: a `package.json` or `pnpm-workspace.yaml`
/// added, removed or edited, or a symlink changed, sets the key of each name
/// whose resolved member changed, so only the files that looked that name up
/// are bound again.
pub(crate) fn js_package_key(root: &str, name: &str) -> String {
    format!("{VIEW_JS_WORKSPACES_DOMAIN}\0{root}\0{name}")
}
/// The Rust module tree: which files exist and which modules each declares.
/// `Parent` answers read it; a file whose module declarations change, or a
/// Rust file added or removed, sets it.
pub(crate) const VIEW_RUST_MODULE_DOMAIN: &str = "\0view:rust-module-index";

/// The key of what Rust files at `segments` under `src_prefix` contain, for
/// inline module lookups that consult those files.
pub(crate) fn rust_module_key(src_prefix: &str, segments: &[String]) -> String {
    format!("\0view:rust-module:{src_prefix}:{}", segments.join("::"))
}

/// [`rust_module_key`] for the Rust file at `path`: the key its surface
/// changes set.
pub(crate) fn rust_module_key_for_file(path: &str) -> String {
    rust_module_key(
        &super::rust_src_prefix(path),
        &super::rust_module_segments_for_rel(path),
    )
}
/// Recorded by every Go package file listing. A listing reads the directory,
/// not the files in it, so a file added to the package is no recorded
/// dependency of the callers that listed it; the owner rechecks this domain
/// whenever a `.go` file is added or removed.
pub(crate) const VIEW_GO_PACKAGE_DOMAIN: &str = "\0view:go-package-files";
/// The Go module map: every `go.mod` and the module path it declares. A
/// reader records this one key instead of the map's inputs, so its recorded
/// inputs are the same whichever reader built the map, and an edit that
/// changes the map (a `go.mod` added, removed or rewritten) re-checks every
/// reader through the `GoPackage` answers they recorded.
pub(crate) const VIEW_GO_MODULES_DOMAIN: &str = "\0view:go-modules";
/// The Rust workspace's crate names and source folders, read from every
/// `Cargo.toml`. Recorded like [`VIEW_GO_MODULES_DOMAIN`]: one key for the
/// map, set by any `Cargo.toml` added, removed or rewritten, with the
/// answers each reader used kept as `Crate` queries. Today's views set this
/// key; the data layer sets [`rust_crate_key`] for each crate name whose
/// entry changed instead.
pub(crate) const VIEW_RUST_CRATES_DOMAIN: &str = "\0view:rust-crates";

/// The key a reader records for looking up the workspace crate `name`, found
/// or not. A `Cargo.toml` edit that adds, removes, renames or moves a crate
/// then reaches only the files that looked that name up, instead of every
/// Rust file: adding a crate nothing imports rebinds nothing, and a file
/// whose `use` named a crate that did not exist yet is rebound when it
/// appears.
pub(crate) fn rust_crate_key(name: &str) -> String {
    format!("{VIEW_RUST_CRATES_DOMAIN}\0{name}")
}

impl ViewSurfaceQuery {
    fn dependencies(&self) -> Vec<String> {
        match self {
            // Both crate keys: today's views set the map's key, the data
            // layer the looked-up name's.
            Self::Crate(name) => vec![
                VIEW_CONFIG_MEMBERSHIP_DOMAIN.into(),
                VIEW_RUST_CRATES_DOMAIN.into(),
                rust_crate_key(name),
            ],
            // The nearest `Cargo.toml` above the file decides its crate:
            // today's views set the config domain when any is added or
            // removed, the data layer the path of each one that was.
            Self::CrateRoot(file) => {
                let mut dependencies = vec![file.clone(), VIEW_CONFIG_MEMBERSHIP_DOMAIN.into()];
                let mut directory = std::path::Path::new(file).parent();
                while let Some(current) = directory {
                    dependencies.push(if current.as_os_str().is_empty() {
                        "Cargo.toml".to_string()
                    } else {
                        format!("{}/Cargo.toml", current.display())
                    });
                    directory = current.parent();
                }
                dependencies
            }
            Self::Parent(file) => vec![file.clone(), VIEW_RUST_MODULE_DOMAIN.into()],
            // An inline lookup checks the files whose module path is a
            // proper prefix of the looked-up path, in the caller's source
            // folder. Which files those are follows from paths alone, so it
            // changes only when Rust files are added or removed (the module
            // domain); what they contain is keyed by their module path.
            Self::Inline(file, segments, _) => {
                let prefix = super::rust_src_prefix(file);
                let mut dependencies = vec![file.clone(), VIEW_RUST_MODULE_DOMAIN.into()];
                dependencies.extend(
                    (0..segments.len()).map(|len| rust_module_key(&prefix, &segments[..len])),
                );
                dependencies
            }
            // Which directory an import path names comes from the module map,
            // so a `go.mod` edit alone can change the answer.
            Self::GoPackage(..) => {
                vec![VIEW_GO_PACKAGE_DOMAIN.into(), VIEW_GO_MODULES_DOMAIN.into()]
            }
            Self::Language(file)
            | Self::MacroUse(file)
            | Self::Module(file, _)
            | Self::Reexports(file)
            | Self::Node(file, _)
            | Self::Callable(file, _)
            | Self::Alias(file, _)
            | Self::Export(file, _)
            | Self::Default(file)
            | Self::Contains(file) => vec![file.clone()],
        }
    }

    /// Whether one of [`Self::dependencies`] is in `changed`, without
    /// building the list for the queries whose dependencies are their file
    /// and fixed domain keys: a replay asks this of every recorded query of
    /// every file it replays, most of them naming one file.
    fn depends_on_any(&self, changed: &BTreeSet<String>) -> bool {
        match self {
            Self::Parent(file) => {
                changed.contains(file) || changed.contains(VIEW_RUST_MODULE_DOMAIN)
            }
            Self::GoPackage(..) => {
                changed.contains(VIEW_GO_PACKAGE_DOMAIN) || changed.contains(VIEW_GO_MODULES_DOMAIN)
            }
            Self::Language(file)
            | Self::MacroUse(file)
            | Self::Module(file, _)
            | Self::Reexports(file)
            | Self::Node(file, _)
            | Self::Callable(file, _)
            | Self::Alias(file, _)
            | Self::Export(file, _)
            | Self::Default(file)
            | Self::Contains(file) => changed.contains(file),
            Self::Crate(..) | Self::CrateRoot(..) | Self::Inline(..) => self
                .dependencies()
                .iter()
                .any(|dependency| changed.contains(dependency)),
        }
    }

    fn answer(&self, index: &impl super::ResolverIndex) -> String {
        match self {
            Self::Language(file) => {
                surface_value(&index.lang_for(file).map(|lang| format!("{lang:?}")))
            }
            Self::Module(file, module) => surface_value(&index.module_target(file, module)),
            Self::MacroUse(file) => surface_value(&index.macro_use_modules(file)),
            Self::Parent(file) => surface_value(&index.module_parent(file)),
            Self::Reexports(file) => reexport_surface(&index.reexports_for(file)),
            Self::Node(file, symbol) => surface_value(&index.node_for_symbol(file, symbol)),
            Self::Callable(file, node) => surface_value(&index.node_is_callable(file, node)),
            Self::Alias(file, symbol) => surface_value(&index.export_alias(file, symbol)),
            Self::Export(file, symbol) => surface_value(&index.has_export(file, symbol)),
            Self::Default(file) => surface_value(&index.default_export(file)),
            Self::Contains(file) => surface_value(&index.contains_file(file)),
            Self::Crate(name) => surface_value(&index.crate_src_prefix(name)),
            Self::CrateRoot(file) => surface_value(&index.rust_crate_root_file(file)),
            Self::Inline(file, segments, symbol) => {
                surface_value(&index.inline_scoped_target(file, segments, symbol))
            }
            Self::GoPackage(file, module) => {
                surface_value(&index.go_package_files(file, module.as_deref()))
            }
        }
    }
}

/// Whether `query`'s answer in `index` is `expected`, asking `index` once
/// per memo: the memo keeps a copy of each query it answers, made only the
/// first time.
fn answer_is(
    query: &ViewSurfaceQuery,
    expected: &str,
    index: &impl super::ResolverIndex,
    answers: &mut BTreeMap<ViewSurfaceQuery, String>,
) -> bool {
    if let Some(answer) = answers.get(query) {
        return answer == expected;
    }
    let answer = query.answer(index);
    let same = answer == expected;
    answers.insert(query.clone(), answer);
    same
}

/// A resolver index over one file's resolution that records each surface
/// query it answers. The callers gather what every reference of the file
/// recorded into one set for the whole file, so a question already answered
/// for this file is not asked, or recorded, again. A large file asks the
/// same crate-root and module questions for every reference.
struct ViewSurfaceIndex<'a, I> {
    inner: &'a I,
    queries: std::cell::RefCell<BTreeMap<ViewSurfaceQuery, String>>,
    caller: Option<(&'a str, &'a FileCallData)>,
    crate_roots: std::cell::RefCell<HashMap<String, Option<String>>>,
    caller_modules: std::cell::RefCell<HashMap<String, Vec<String>>>,
    /// The file data whose `use` answers this index keeps: the data of the
    /// file being resolved, borrowed for this index's whole life, so no
    /// other value can share its address.
    alias_owner: Option<&'a FileCallData>,
    caller_aliases: std::cell::OnceCell<std::rc::Rc<[super::RustModuleAliases]>>,
}

impl<'a, I> ViewSurfaceIndex<'a, I> {
    /// An index for resolving `caller`'s references, which reads its own
    /// caller data from `caller` rather than from `inner`.
    fn for_caller(inner: &'a I, caller: &'a str, data: &'a FileCallData) -> Self {
        Self::new(inner, Some((caller, data)), Some(data))
    }

    /// An index that reads every file's caller data from `inner`, keeping
    /// the `use` answers of `aliases_of` (the file being resolved).
    fn reading_inner(inner: &'a I, aliases_of: &'a FileCallData) -> Self {
        Self::new(inner, None, Some(aliases_of))
    }

    fn new(
        inner: &'a I,
        caller: Option<(&'a str, &'a FileCallData)>,
        alias_owner: Option<&'a FileCallData>,
    ) -> Self {
        Self {
            inner,
            queries: Default::default(),
            caller,
            crate_roots: Default::default(),
            caller_modules: Default::default(),
            alias_owner,
            caller_aliases: Default::default(),
        }
    }

    /// Whether this index keeps answers. Tests that compare cached and
    /// uncached module lookups turn it off.
    fn keeps_answers() -> bool {
        #[cfg(test)]
        if super::rust_module_work::disabled() {
            return false;
        }
        true
    }
}

impl<I> ViewSurfaceIndex<'_, I> {
    fn record(&self, query: ViewSurfaceQuery, value: &impl Serialize) {
        self.queries
            .borrow_mut()
            .insert(query, surface_value(value));
    }
    fn take(&self) -> BTreeMap<ViewSurfaceQuery, String> {
        std::mem::take(&mut *self.queries.borrow_mut())
    }
}

impl<I: super::ResolverIndex> super::ResolverIndex for ViewSurfaceIndex<'_, I> {
    fn macro_use_modules(&self, file: &str) -> Vec<String> {
        let value = self.inner.macro_use_modules(file);
        self.record(ViewSurfaceQuery::MacroUse(file.into()), &value);
        value
    }
    fn go_package_files(&self, caller_file: &str, module: Option<&str>) -> Vec<String> {
        let value = self.inner.go_package_files(caller_file, module);
        self.record(
            ViewSurfaceQuery::GoPackage(caller_file.into(), module.map(str::to_string)),
            &value,
        );
        value
    }
    fn caller_data(&self, file: &str) -> Option<&FileCallData> {
        // resolve_ref reads only its own caller data; changed callers never reuse
        // surface answers, so the immutable blob itself guards this input.
        self.caller
            .filter(|(caller, _)| *caller == file)
            .map(|(_, data)| data)
            .or_else(|| self.inner.caller_data(file))
    }
    fn local_symbol(&self, file: &str, data: &FileCallData, query: &str) -> Option<String> {
        // Local symbols come from the caller blob, not a consulted file surface.
        self.inner.local_symbol(file, data, query)
    }
    fn lang_for(&self, file: &str) -> Option<LangId> {
        let value = self.inner.lang_for(file);
        self.record(
            ViewSurfaceQuery::Language(file.into()),
            &value.map(|lang| format!("{lang:?}")),
        );
        value
    }
    fn module_target(&self, file: &str, module: &str) -> Option<String> {
        let value = self.inner.module_target(file, module);
        self.record(ViewSurfaceQuery::Module(file.into(), module.into()), &value);
        value
    }
    fn module_parent(&self, file: &str) -> Option<(String, String)> {
        let value = self.inner.module_parent(file);
        self.record(ViewSurfaceQuery::Parent(file.into()), &value);
        value
    }
    fn reexports_for(&self, file: &str) -> Vec<super::ReexportIndex> {
        let value = self.inner.reexports_for(file);
        self.queries.borrow_mut().insert(
            ViewSurfaceQuery::Reexports(file.into()),
            reexport_surface(&value),
        );
        value
    }
    fn node_for_symbol(&self, file: &str, symbol: &str) -> Option<String> {
        let value = self.inner.node_for_symbol(file, symbol);
        self.record(ViewSurfaceQuery::Node(file.into(), symbol.into()), &value);
        value
    }
    fn node_is_callable(&self, file: &str, node: &str) -> bool {
        let value = self.inner.node_is_callable(file, node);
        self.record(ViewSurfaceQuery::Callable(file.into(), node.into()), &value);
        value
    }
    fn export_alias(&self, file: &str, symbol: &str) -> Option<String> {
        let value = self.inner.export_alias(file, symbol);
        self.record(ViewSurfaceQuery::Alias(file.into(), symbol.into()), &value);
        value
    }
    fn has_export(&self, file: &str, symbol: &str) -> bool {
        let value = self.inner.has_export(file, symbol);
        self.record(ViewSurfaceQuery::Export(file.into(), symbol.into()), &value);
        value
    }
    fn default_export(&self, file: &str) -> Option<String> {
        let value = self.inner.default_export(file);
        self.record(ViewSurfaceQuery::Default(file.into()), &value);
        value
    }
    fn contains_file(&self, file: &str) -> bool {
        let value = self.inner.contains_file(file);
        self.record(ViewSurfaceQuery::Contains(file.into()), &value);
        value
    }
    fn crate_src_prefix(&self, name: &str) -> Option<String> {
        let value = self.inner.crate_src_prefix(name);
        self.record(ViewSurfaceQuery::Crate(name.into()), &value);
        value
    }
    fn rust_crate_root_file(&self, file: &str) -> Option<String> {
        if let Some(value) = self.crate_roots.borrow().get(file) {
            return value.clone();
        }
        let value = self.inner.rust_crate_root_file(file);
        self.record(ViewSurfaceQuery::CrateRoot(file.into()), &value);
        if Self::keeps_answers() {
            self.crate_roots
                .borrow_mut()
                .insert(file.to_string(), value.clone());
        }
        value
    }
    fn rust_caller_module_segments(&self, caller_file: &str) -> Vec<String> {
        if let Some(segments) = self.caller_modules.borrow().get(caller_file) {
            return segments.clone();
        }
        // Computed through this index, so the queries it asks are recorded.
        let segments = super::rust_caller_module_segments_uncached(self, caller_file);
        if Self::keeps_answers() {
            self.caller_modules
                .borrow_mut()
                .insert(caller_file.to_string(), segments.clone());
        }
        segments
    }
    fn rust_module_aliases(&self, data: &FileCallData) -> std::rc::Rc<[super::RustModuleAliases]> {
        // Kept only for the data this index borrows for its whole life
        // (`alias_owner`), so no other value can share its address.
        match self.alias_owner {
            Some(owner) if std::ptr::eq(owner, data) && Self::keeps_answers() => {
                std::rc::Rc::clone(
                    self.caller_aliases
                        .get_or_init(|| super::rust_module_aliases_of(data)),
                )
            }
            _ => super::rust_module_aliases_of(data),
        }
    }
    fn inline_scoped_target(
        &self,
        file: &str,
        segments: &[String],
        symbol: &str,
    ) -> Option<(String, String)> {
        let value = self.inner.inline_scoped_target(file, segments, symbol);
        self.record(
            ViewSurfaceQuery::Inline(file.into(), segments.to_vec(), symbol.into()),
            &value,
        );
        value
    }
}

#[cfg(test)]
mod consultation_tests {
    use super::*;

    #[test]
    fn opaque_config_read_is_unattributed_even_after_a_known_field_read() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("package.json"), b"{}").unwrap();
        let facts = ViewBindingFacts::new(Rc::new(super::super::disk_facts::DiskFacts::new(
            root.path(),
        )));
        facts.config_fact(b"package.json", "name");
        assert!(facts.attributed_config_bytes(b"package.json").is_some());
        assert!(!facts.consultations.borrow().unattributed());
        facts.memo_start(Path::new("/"), "test", "");
        assert!(facts.config_bytes(b"package.json").is_some());
        facts.memo_finish(Path::new("/"), "test", "");
        assert!(facts.take_config().unattributed());
        facts.memo_replay(Path::new("/"), "test", "");
        assert!(facts.take_config().unattributed());
    }
}

aft_store::heap_bytes_none!(BlobRefKind, ResolutionStatus);
aft_store::heap_bytes_of_fields!(ParseBlob {
    entry_point_symbols,
    dispatch,
    extractor_version,
    language,
    ast_nodes,
    symbols,
    default_export_symbol,
    exported_symbols,
    callable_symbols,
    imports,
    python_bindings,
    refs,
});
aft_store::heap_bytes_of_fields!(AstPreorderNode {
    ordinal,
    kind,
    byte_start,
    byte_end,
});
aft_store::heap_bytes_of_fields!(BlobSymbol {
    is_test,
    ordinal,
    name,
    scoped_name,
    kind,
    exported,
    is_default_export,
    start_line,
    start_col,
    end_line,
    end_col,
    signature,
    program_start,
    entry_attribute,
});
aft_store::heap_bytes_of_fields!(BlobRef {
    ordinal,
    kind,
    caller_symbol,
    short_name,
    full_ref,
    module_path,
    line,
    byte_start,
    byte_end,
    path_override,
    local_name,
    requested_name,
    namespace_alias,
    wildcard,
    import_kind,
});
aft_store::heap_bytes_of_fields!(BlobImport {
    ordinal,
    module_path,
    names,
    default_import,
    namespace_import,
    byte_start,
    byte_end,
    raw_text,
    type_only,
    side_effect,
});
aft_store::heap_bytes_of_fields!(DerivedRow {
    caller_blob_key,
    ref_ordinal,
    caller_path,
    kind,
    status,
    target_path,
    target_symbol,
});
aft_store::heap_bytes_of_fields!(DispatchTypeTargets { targets, trace });
aft_store::heap_bytes_of_fields!(DispatchTypeTargetTrace {
    queries,
    probes,
    answers,
});
aft_store::heap_bytes_of_fields!(ViewBindingDependencies {
    references,
    dependencies,
    consulted_facts,
    unattributed,
    binding_facts,
    resolution_facts,
    binding_probes,
    resolved_dependencies,
    surface_queries,
    binding_answers,
    resolution_answers,
    surface,
});
aft_store::heap_bytes_of_fields!(ConfigConsultations {
    facts,
    reads,
    unattributed,
});
aft_store::heap_bytes_of_fields!(ViewFileSurface {
    language,
    exports,
    default_export,
    export_aliases,
    node_by_scoped,
    node_by_bare,
    node_kind_by_id,
    module_targets,
    declared_module_targets,
    macro_use_module_targets,
    reexports,
});

impl aft_store::heap::HeapBytes for ViewSurfaceQuery {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        match self {
            Self::Language(a)
            | Self::MacroUse(a)
            | Self::Parent(a)
            | Self::Reexports(a)
            | Self::Default(a)
            | Self::Contains(a)
            | Self::Crate(a)
            | Self::CrateRoot(a) => a.heap_bytes(seen),
            Self::Module(a, b)
            | Self::Node(a, b)
            | Self::Callable(a, b)
            | Self::Alias(a, b)
            | Self::Export(a, b) => a.heap_bytes(seen) + b.heap_bytes(seen),
            Self::Inline(a, names, b) => {
                a.heap_bytes(seen) + names.heap_bytes(seen) + b.heap_bytes(seen)
            }
            Self::GoPackage(a, b) => a.heap_bytes(seen) + b.heap_bytes(seen),
        }
    }
}
