//! Manifest-only callgraph joining.
//!
//! This module deliberately accepts a manifest plus immutable blob payloads rather
//! than a checkout root.  Extraction is content-addressed and path-free; binding a
//! blob to a manifest path and resolving its cross-file references happens here.
//! The existing resolver uses String file identities. Non-UTF-8 source members
//! remain in byte-addressed facts but are reported as unbound rather than being
//! converted lossily; supporting them requires a separate resolver identity change.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[path = "dispatch.rs"]
pub mod dispatch;

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

    fn read_callgraph_blob_decoded(
        &self,
        full_key: &str,
    ) -> Result<Option<Arc<CallgraphBlob>>, ManifestJoinError> {
        self.read_callgraph_blob(full_key)?
            .map(|payload| CallgraphBlob::from_bytes(&payload).map(Arc::new))
            .transpose()
    }

    /// Decoded blobs for `full_keys`, in order, stopping after the first
    /// missing blob (its `None` is the last element) or at the first error,
    /// exactly like reading them one by one. Readers that can verify and
    /// decode in parallel override this.
    fn read_callgraph_blobs_decoded(
        &self,
        full_keys: &[&str],
    ) -> Result<Vec<Option<Arc<CallgraphBlob>>>, ManifestJoinError> {
        let mut blobs = Vec::with_capacity(full_keys.len());
        for key in full_keys {
            let blob = self.read_callgraph_blob_decoded(key)?;
            let missing = blob.is_none();
            blobs.push(blob);
            if missing {
                break;
            }
        }
        Ok(blobs)
    }
}

/// Shares a decoded object while a consumer owns it, without retaining an
/// all decoded blobs after emission has finished.
#[derive(Default)]
pub(crate) struct DecodedBlobCache(
    std::cell::RefCell<HashMap<String, std::sync::Weak<CallgraphBlob>>>,
);

impl DecodedBlobCache {
    pub(crate) fn read(
        &self,
        key: &str,
        read: impl FnOnce() -> Result<Option<Arc<CallgraphBlob>>, ManifestJoinError>,
    ) -> Result<Option<Arc<CallgraphBlob>>, ManifestJoinError> {
        if let Some(blob) = self.0.borrow().get(key).and_then(std::sync::Weak::upgrade) {
            return Ok(Some(blob));
        }
        let blob = read()?;
        if let Some(blob) = &blob {
            self.0
                .borrow_mut()
                .insert(key.to_string(), Arc::downgrade(blob));
        }
        Ok(blob)
    }
}

pub(crate) struct SharedBlobReader<'a, R> {
    pub(crate) inner: &'a R,
    pub(crate) decoded: DecodedBlobCache,
}

impl<R: ManifestBlobReader> ManifestBlobReader for SharedBlobReader<'_, R> {
    fn read_callgraph_blob(&self, key: &str) -> Result<Option<Vec<u8>>, ManifestJoinError> {
        self.inner.read_callgraph_blob(key)
    }

    fn read_callgraph_blob_decoded(
        &self,
        key: &str,
    ) -> Result<Option<Arc<CallgraphBlob>>, ManifestJoinError> {
        self.decoded
            .read(key, || self.inner.read_callgraph_blob_decoded(key))
    }
}

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
        let lang = language_id(language)
            .ok_or_else(|| ManifestJoinError::UnsupportedLanguage(language.to_string()))?;
        let extractor_version = extractor_version.into();
        let mut parser = Parser::new();
        parser
            .set_language(&grammar_for(lang))
            .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| ManifestJoinError::Parse("tree-sitter returned no tree".to_string()))?;
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
        let mut structural = super::collect_reexport_refs_with_line_index(
            paths.root,
            file,
            "__callgraph_blob__",
            source,
            &crate::callgraph::ModuleResolutionMemo::default(),
            &paths,
            &line_index,
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
        dispatch::complete_members(source, lang, root, &mut parse)?;
        parse.dispatch = dispatch::extract(source, lang, root, &parse)?;
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

    fn file_data(&self, path: &str) -> Result<FileCallData, ManifestJoinError> {
        let lang = language_id(&self.language)
            .ok_or_else(|| ManifestJoinError::UnsupportedLanguage(self.language.clone()))?;
        let mut calls_by_symbol: HashMap<String, Vec<callgraph::CallSite>> = HashMap::new();
        let mut value_refs_by_symbol: HashMap<String, Vec<callgraph::CallSite>> = HashMap::new();
        let mut macro_body_calls_by_macro: HashMap<String, Vec<callgraph::CallSite>> =
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
            macro_mentions_by_symbol: HashMap::new(),
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

    fn bind(
        &self,
        path: &str,
        facts: &FactPaths<'_>,
    ) -> Result<super::FileExtract, ManifestJoinError> {
        self.bind_with_dependencies(path, facts, None)
    }

    fn bind_with_dependencies(
        &self,
        path: &str,
        facts: &FactPaths<'_>,
        cached: Option<&BTreeMap<u32, BTreeSet<String>>>,
    ) -> Result<super::FileExtract, ManifestJoinError> {
        let data = self.file_data(path)?;
        let nodes = self
            .symbols
            .iter()
            .map(|symbol| {
                let scoped_name = bound_name(&symbol.scoped_name, path);
                super::NodeRecord {
                    is_test: symbol.is_test,
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
                    is_callgraph_entry_point: self
                        .entry_point_symbols
                        .contains(&symbol.scoped_name),
                }
            })
            .collect::<Vec<_>>();
        // Overloads can share a scoped name. Preserve the first node chosen by
        // the source-order scan without rescanning all symbols for every call.
        let mut caller_nodes = HashMap::with_capacity(nodes.len());
        for node in &nodes {
            #[cfg(test)]
            CALLER_NODE_LOOKUP_WORK.with(|work| work.set(work.get() + 1));
            caller_nodes.entry(&node.scoped_name).or_insert(&node.id);
        }
        let abs = facts.root.join(path);
        let mut raw_refs = Vec::new();
        for (position, (raw, reference_key)) in
            self.refs.iter().zip(self.reference_keys()).enumerate()
        {
            let position = u32::try_from(position).expect("reference vector fits u32");
            let dependencies =
                if let Some(dependencies) = cached.and_then(|cache| cache.get(&position)) {
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
                    super::module_dependencies(facts.root, &abs, module, facts)
                } else {
                    BTreeSet::new()
                };
            let caller_symbol = raw
                .caller_symbol
                .as_ref()
                .map(|name| bound_name(name, path));
            let caller_node = caller_symbol.as_ref().and_then(|name| {
                #[cfg(test)]
                CALLER_NODE_LOOKUP_WORK.with(|work| work.set(work.get() + 1));
                #[cfg(test)]
                if SCAN_CALLER_NODES.with(std::cell::Cell::get) {
                    return nodes
                        .iter()
                        .find(|node| {
                            CALLER_NODE_LOOKUP_WORK.with(|work| work.set(work.get() + 1));
                            &node.scoped_name == name
                        })
                        .map(|node| node.id.clone());
                }
                caller_nodes.get(name).map(|id| (*id).clone())
            });
            raw_refs.push(super::RawRef {
                ref_id: format!("{path}:{reference_key}:{:?}", raw.kind),
                caller_node,
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
            source_identity: None,
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
        let mut receiver_sites = BTreeSet::new();
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
            receiver_sites.extend(
                blob.dispatch
                    .sites
                    .iter()
                    .map(|site| (path.as_bytes().to_vec(), site.ordinal)),
            );
            let extract = blob.bind(rel, &paths)?;
            for ((raw, bound), reference_key) in blob
                .refs
                .iter()
                .zip(&extract.raw_refs)
                .zip(blob.reference_keys())
            {
                let ref_key = CallerRefKey {
                    caller_blob_key: key.clone(),
                    ref_ordinal: reference_key,
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
            let mut resolved = super::resolve_ref(raw, &index)
                .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
            if receiver_sites.contains(&(key.caller_path.clone(), key.ref_ordinal)) {
                // Receiver sites are resolved only by the ruled hint resolver.
                // Ordinary name binding must not invent an object-method edge.
                resolved.target_file = None;
                resolved.target_symbol = None;
            }
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
                current_keys.extend(blob.reference_keys().into_iter().map(|reference_key| {
                    CallerRefKey {
                        caller_blob_key: key.clone(),
                        ref_ordinal: reference_key,
                        caller_path: path.as_bytes().to_vec(),
                    }
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

/// The receiver type names of one Rust file that method-call resolution may
/// look up through ordinary Rust path binding: declared types and their bases,
/// method owners and traits, field and return types, and each call site's
/// receiver with any field or return-value wrapper removed.
fn dispatch_type_names(parse: &ParseBlob) -> BTreeSet<String> {
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

/// Bind one Rust file's receiver type names with the same workspace, module and
/// re-export rules as ordinary Rust paths. A name maps to its target file and
/// symbol only when the target names a node of the index.
fn resolve_dispatch_type_names<I: super::ResolverIndex>(
    index: &I,
    file: &str,
    parse: &ParseBlob,
    data: &FileCallData,
) -> BTreeMap<String, (String, String)> {
    let mut targets = BTreeMap::new();
    for name in dispatch_type_names(parse) {
        let short = name.rsplit("::").next().unwrap_or(&name);
        let raw = super::RawRef {
            ref_id: String::new(),
            caller_node: None,
            caller_symbol: None,
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
            line: 0,
            byte_start: 0,
            byte_end: 0,
            dependencies: BTreeSet::new(),
        };
        let resolved = super::resolve_rust_target(index, file, &name, short, data, &raw);
        if let Some((_, target_file, symbol)) = resolved {
            if super::ResolverIndex::node_for_symbol(index, &target_file, &symbol).is_some() {
                targets.insert(name, (target_file, symbol));
            }
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

    /// True when a path this resolution probed was added or removed.
    pub(crate) fn probes_any(&self, paths: &BTreeSet<String>) -> bool {
        !self.probes.is_disjoint(paths)
    }

    /// True when every recorded answer that depends on one of `changed` is
    /// unchanged in `index`. An answer depending on nothing in `changed` reads
    /// only index entries that are the same as when it was recorded. Traces
    /// share queries (a crate's module parents, for one), so answers are kept
    /// in `answers` for the next trace checked against the same index.
    fn replays<'q>(
        &'q self,
        index: &impl super::ResolverIndex,
        changed: &BTreeSet<String>,
        answers: &mut BTreeMap<&'q ViewSurfaceQuery, String>,
    ) -> bool {
        self.queries.iter().all(|(query, expected)| {
            !query
                .dependencies()
                .iter()
                .any(|dependency| changed.contains(dependency))
                || *answers.entry(query).or_insert_with(|| query.answer(index)) == *expected
        })
    }
}

/// One Rust file's receiver type targets, keyed by the type name as written.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DispatchTypeTargets {
    pub targets: BTreeMap<String, (String, String)>,
    pub trace: DispatchTypeTargetTrace,
}

/// The previous generation's traces of the files whose trace depends on one of
/// the given paths or index domains.
pub(crate) type DispatchTypeTargetTraces<'p> =
    &'p dyn Fn(
        &BTreeSet<String>,
    ) -> Result<BTreeMap<String, DispatchTypeTargetTrace>, ManifestJoinError>;

/// Which Rust files a join resolves receiver type targets for.
pub(crate) enum DispatchTypeTargetPlan<'p> {
    /// Every Rust file of the manifest, as a cold build does.
    All,
    /// Resolve every file of `recompute`. Other Rust files are checked only
    /// when their previous trace depends on a path of `recompute`, on a path
    /// of `membership_changed`, on a file whose index entry this join had to
    /// rebuild, or, when `module_domain_changed` is set, on the crate-wide
    /// module domain. The owner clears it only when no input of the module
    /// index changed, so no domain answer can differ. A checked file is resolved
    /// again when its trace probed an added or removed path, or when one of
    /// the index answers it recorded is different now. Every other file keeps
    /// its previous targets.
    Selected {
        recompute: &'p BTreeSet<String>,
        membership_changed: &'p BTreeSet<String>,
        module_domain_changed: bool,
        traces: DispatchTypeTargetTraces<'p>,
    },
}

#[cfg(test)]
#[path = "../../tests/integration/join_manifest_test.rs"]
mod manifest_integration_tests;

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
    #[serde(skip)]
    surface: Option<ViewFileSurface>,
}

impl ViewBindingDependencies {
    pub(crate) fn from_surface_json(payload: &str) -> Result<Self, serde_json::Error> {
        Ok(Self {
            surface: Some(serde_json::from_str(payload)?),
            ..Self::default()
        })
    }

    pub(crate) fn copy_surface_from(&mut self, source: &Self) {
        self.surface.clone_from(&source.surface);
    }

    pub(crate) fn surface_json(&self) -> Result<Option<String>, serde_json::Error> {
        self.surface.as_ref().map(serde_json::to_string).transpose()
    }
}

/// Inputs that determine a reference's target, excluding call-site identity.
/// Rust qualified imports are visible only after their declaration; the count
/// of visible imports identifies that monotone prefix even across nested uses.
#[derive(Hash, PartialEq, Eq)]
struct ViewResolutionBinding {
    kind: String,
    full_ref: Option<String>,
    short_name: Option<String>,
    visible_rust_imports: usize,
}

impl ViewResolutionBinding {
    fn new(raw: &super::RawRef, caller: &FileCallData) -> Self {
        Self {
            kind: raw.kind.clone(),
            full_ref: raw.full_ref.clone(),
            short_name: raw.short_name.clone(),
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

pub(crate) struct SelectedManifestJoin {
    pub result: JoinResult,
    pub bindings: BTreeMap<String, ViewBindingDependencies>,
    pub resolved_callers: BTreeSet<String>,
    pub rebuilt_surface_entries: usize,
    pub rebuilt_surface_paths: BTreeSet<String>,
    pub decoded_caller_blobs: usize,
    pub resolved_bindings: usize,
    /// Receiver type targets of the Rust files the join's
    /// [`DispatchTypeTargetPlan`] resolved; other files keep their previous ones.
    pub dispatch_type_targets: BTreeMap<String, DispatchTypeTargets>,
    /// Parse blobs decoded only to resolve receiver type targets.
    pub dispatch_type_target_decoded_blobs: usize,
    /// Previous traces of other files checked against the new index.
    pub dispatch_type_target_traces: usize,
    /// Checked traces whose only dependency among the seeds was the module
    /// domain.
    pub module_domain_traces: usize,
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

    fn restore(&self) -> super::DbFileIndex {
        super::DbFileIndex {
            lang: language_id(&self.language),
            exports: self.exports.iter().cloned().collect(),
            default_export: self.default_export.clone(),
            export_aliases: self.export_aliases.clone().into_iter().collect(),
            node_by_scoped: self.node_by_scoped.clone().into_iter().collect(),
            node_by_bare: self.node_by_bare.clone().into_iter().collect(),
            node_kind_by_id: self.node_kind_by_id.clone().into_iter().collect(),
            module_targets: self.module_targets.clone().into_iter().collect(),
            declared_module_targets: self.declared_module_targets.clone().into_iter().collect(),
            macro_use_module_targets: self.macro_use_module_targets.clone(),
            reexports: self
                .reexports
                .iter()
                .map(|(target_file, named, wildcard)| super::ReexportIndex {
                    target_file: target_file.clone(),
                    named: named.clone().into_iter().collect(),
                    wildcard: *wildcard,
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
}
type ConsultationMemoKey = (std::path::PathBuf, String, String);

/// Memo answers and their provenance belong to one immutable manifest join.
/// Replaying provenance on hits keeps missing candidates and facts attributable.
struct ViewBindingFacts<'a> {
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
}

impl<'a> ViewBindingFacts<'a> {
    fn new(inner: Rc<dyn ProjectFacts + 'a>) -> Self {
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
        }
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
        // Directory discovery may probe thousands of missing package manifests.
        // One membership domain rechecks those bindings on config add/remove;
        // individual config content changes use the field consultations instead.
        if view_resolution_config(path) {
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
}

impl ProjectFacts for ViewBindingFacts<'_> {
    fn records_config_facts(&self) -> bool {
        true
    }
    fn config_fact(&self, rel: &[u8], name: &str) {
        self.record(rel);
        if matches!(name, "workspaces" | "packages") {
            self.record(VIEW_CONFIG_MEMBERSHIP_DOMAIN.as_bytes());
        }
        self.config_event(rel, Some(name));
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
        is_file
    }
    fn is_dir(&self, rel: &[u8]) -> bool {
        self.inner.is_dir(rel)
    }
    fn config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
        self.consultations.borrow_mut().unattributed = true;
        for (_, trace) in self.memo_stack.borrow_mut().iter_mut() {
            trace.config.unattributed = true;
        }
        self.attributed_config_bytes(rel)
    }
    fn attributed_config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
        self.config_event(rel, None);
        self.record(rel);
        if let Some(value) = self.config_cache.borrow().get(rel) {
            return value.clone();
        }
        let value = self.inner.config_bytes(rel);
        self.config_cache
            .borrow_mut()
            .insert(rel.to_vec(), value.clone());
        value
    }
    fn symlink_target(&self, rel: &[u8]) -> Option<&[u8]> {
        self.inner.symlink_target(rel)
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
        canonical
    }
    fn list_dir(&self, rel: &[u8]) -> Vec<super::facts::DirEntry> {
        if let Some(value) = self.directory_cache.borrow().get(rel) {
            return value.clone();
        }
        let value = self.inner.list_dir(rel);
        self.directory_cache
            .borrow_mut()
            .insert(rel.to_vec(), value.clone());
        value
    }
}

/// Resolve selected callers against the complete symbol index. Cached binding
/// dependencies avoid resolving unchanged imports merely to rebuild that index.
/// `selected=None` is the cold path; otherwise the owner supplies the transitive
/// reverse-dependency closure and caches from the same base generation.
#[cfg(test)]
pub(crate) fn join_selected_manifest(
    manifest: &Manifest,
    blobs: &impl ManifestBlobReader,
    selected: Option<&BTreeSet<String>>,
    cached: &BTreeMap<String, ViewBindingDependencies>,
) -> Result<SelectedManifestJoin, ManifestJoinError> {
    join_manifest_with_surfaces(manifest, blobs, selected, cached, None, None, true)
}

/// Rebind only changed callers or callers that probed changed membership. Other
/// candidates replay their prior consumer-specific surface queries and retain
/// their reference rows when every answer is unchanged.
#[cfg(test)]
pub(crate) fn join_selected_manifest_reusing_surfaces(
    manifest: &Manifest,
    blobs: &impl ManifestBlobReader,
    selected: Option<&BTreeSet<String>>,
    cached: &BTreeMap<String, ViewBindingDependencies>,
    changed: &BTreeSet<String>,
    membership_changed: &BTreeSet<String>,
    fact_invalidated: &BTreeSet<String>,
    dispatch_type_targets: Option<DispatchTypeTargetPlan<'_>>,
) -> Result<SelectedManifestJoin, ManifestJoinError> {
    join_manifest_with_surfaces(
        manifest,
        blobs,
        selected,
        cached,
        Some((changed, membership_changed, fact_invalidated)),
        dispatch_type_targets,
        true,
    )
}

/// Materialization consumes sorted rows, not the duplicate resolution-order
/// audit vector returned by standalone joins.
pub(crate) fn join_selected_manifest_for_materialization(
    manifest: &Manifest,
    blobs: &impl ManifestBlobReader,
    selected: Option<&BTreeSet<String>>,
    cached: &BTreeMap<String, ViewBindingDependencies>,
    changed: &BTreeSet<String>,
    membership_changed: &BTreeSet<String>,
    fact_invalidated: &BTreeSet<String>,
    dispatch_type_targets: Option<DispatchTypeTargetPlan<'_>>,
) -> Result<SelectedManifestJoin, ManifestJoinError> {
    join_manifest_with_surfaces(
        manifest,
        blobs,
        selected,
        cached,
        Some((changed, membership_changed, fact_invalidated)),
        dispatch_type_targets,
        false,
    )
}

fn join_manifest_with_surfaces(
    manifest: &Manifest,
    blobs: &impl ManifestBlobReader,
    selected: Option<&BTreeSet<String>>,
    cached: &BTreeMap<String, ViewBindingDependencies>,
    reuse: Option<(&BTreeSet<String>, &BTreeSet<String>, &BTreeSet<String>)>,
    dispatch_type_target_plan: Option<DispatchTypeTargetPlan<'_>>,
    record_order: bool,
) -> Result<SelectedManifestJoin, ManifestJoinError> {
    let mut profile = crate::views::materialization::profile::PhaseTimer::new("join");
    let loaded = std::cell::RefCell::new(BTreeMap::<BlobKey, Arc<[u8]>>::new());
    let load = |key: &BlobKey| -> Result<Arc<[u8]>, ManifestJoinError> {
        if let Some(bytes) = loaded.borrow().get(key) {
            return Ok(bytes.clone());
        }
        let bytes: Arc<[u8]> = blobs
            .read_callgraph_blob(key)?
            .ok_or_else(|| ManifestJoinError::MissingBlob(key.clone()))?
            .into();
        loaded.borrow_mut().insert(key.clone(), bytes.clone());
        Ok(bytes)
    };
    // Standalone selective joins have no emission owner to keep changed
    // caller blobs alive across surface replay. Cold materialization already
    // shares those objects through its emission reader.
    let selective_decoded = std::cell::RefCell::new(HashMap::<BlobKey, Arc<CallgraphBlob>>::new());
    let decode = |key: &BlobKey| -> Result<Arc<CallgraphBlob>, ManifestJoinError> {
        if let Some(blob) = selective_decoded.borrow().get(key) {
            return Ok(blob.clone());
        }
        let blob = blobs
            .read_callgraph_blob_decoded(key)?
            .ok_or_else(|| ManifestJoinError::MissingBlob(key.clone()))?;
        if selected.is_some() {
            selective_decoded
                .borrow_mut()
                .insert(key.clone(), blob.clone());
        }
        Ok(blob)
    };
    let read_error = std::cell::RefCell::new(None);
    let reader = |key: &BlobKey| match load(key) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            *read_error.borrow_mut() = Some(error);
            None
        }
    };
    profile.finish("load_payloads");
    let facts = Rc::new(ViewBindingFacts::new(Rc::new(ManifestFacts {
        manifest,
        blobs: &reader,
    })));
    let root = Path::new("/");
    let paths = FactPaths {
        root,
        facts: facts.as_ref(),
    };
    let mut files = HashMap::new();
    let mut bindings = BTreeMap::new();
    let mut unbound_non_utf8_paths = Vec::new();
    let mut rebuilt_surface_paths = BTreeSet::new();
    let mut decoded_caller_blobs = 0;
    let mut restored_surfaces = 0;
    let mut rebuilt_changed = 0;
    let mut rebuilt_facts = 0;
    let mut rebuilt_membership = 0;
    let mut rebuilt_missing = 0;
    #[cfg(test)]
    let mut heap_parts = [0isize; 5];
    for (path, entry) in manifest.entries() {
        let ManifestEntry::Regular { planes, .. } = entry else {
            continue;
        };
        let Some(key) = &planes.callgraph else {
            continue;
        };
        let Ok(rel) = std::str::from_utf8(path.as_bytes()) else {
            if matches!(decode(key)?.as_ref(), CallgraphBlob::Parse(_)) {
                unbound_non_utf8_paths.push(path.as_bytes().to_vec());
            }
            continue;
        };
        let resolve = selected.is_none_or(|set| set.contains(rel));
        let cache = cached.get(rel).filter(|cache| {
            if let Some((changed, membership, fact_invalidated)) = reuse {
                selected.is_some()
                    && !changed.contains(rel)
                    && !fact_invalidated.contains(rel)
                    && cache.dependencies.is_disjoint(membership)
            } else {
                !resolve
            }
        });
        if let Some((cache, surface)) =
            cache.and_then(|cache| cache.surface.as_ref().map(|surface| (cache, surface)))
        {
            restored_surfaces += 1;
            files.insert(rel.to_string(), surface.restore());
            if resolve {
                bindings.insert(rel.to_string(), cache.clone());
            }
            continue;
        }
        #[cfg(test)]
        let before = crate::test_allocations::live_bytes();
        let decoded_blob = decode(key)?;
        #[cfg(test)]
        {
            heap_parts[0] += crate::test_allocations::live_bytes() as isize - before as isize;
        }
        let CallgraphBlob::Parse(blob) = decoded_blob.as_ref() else {
            continue;
        };
        decoded_caller_blobs += 1;
        facts.take();
        facts.take_config();
        #[cfg(test)]
        let before = crate::test_allocations::live_bytes();
        let extract =
            blob.bind_with_dependencies(rel, &paths, cache.map(|cache| &cache.references))?;
        #[cfg(test)]
        {
            heap_parts[1] += crate::test_allocations::live_bytes() as isize - before as isize;
        }
        #[cfg(test)]
        let before = crate::test_allocations::live_bytes();
        let file_index = super::DbFileIndex::from_extract(root, &extract, &paths);
        #[cfg(test)]
        {
            heap_parts[2] += crate::test_allocations::live_bytes() as isize - before as isize;
        }
        rebuilt_surface_paths.insert(rel.to_string());
        match reuse {
            Some((changed, _, _)) if changed.contains(rel) => rebuilt_changed += 1,
            Some((_, _, invalidated)) if invalidated.contains(rel) => rebuilt_facts += 1,
            Some((_, membership, _))
                if cached
                    .get(rel)
                    .is_some_and(|old| !old.dependencies.is_disjoint(membership)) =>
            {
                rebuilt_membership += 1;
            }
            _ => rebuilt_missing += 1,
        }
        #[cfg(test)]
        let before = crate::test_allocations::live_bytes();
        let mut binding = cache.cloned().unwrap_or_default();
        binding.surface = Some(ViewFileSurface::capture(&blob.language, &file_index));
        files.insert(rel.to_string(), file_index);
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
            binding.dependencies = binding.references.values().flatten().cloned().collect();
            binding
                .dependencies
                .extend(binding.binding_probes.iter().cloned());
        }
        bindings.insert(rel.to_string(), binding);
        #[cfg(test)]
        {
            heap_parts[3] += crate::test_allocations::live_bytes() as isize - before as isize;
        }
        // Only the resolver surface survives this pass. Nodes, caller data and
        // raw references are rebuilt for one selected caller at a time below.
    }
    #[cfg(test)]
    if std::env::var_os("AFT_VIEW_PROFILE").is_some() {
        eprintln!(
            "view_heap join.parts decoded={} extracts={} file_index={} bindings={} work={}",
            heap_parts[0], heap_parts[1], heap_parts[2], heap_parts[3], heap_parts[4]
        );
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if selected.is_none() {
        crate::memory::relieve_allocator_pressure();
    }
    profile.finish("decode_bind_index_entries");
    let mut replay_stage = crate::views::materialization::profile::StageTimer::new("join");
    let index = ManifestProjectIndex::from_parts(
        root,
        files,
        HashMap::new(),
        super::WorkspaceCratePrefixCache::default(),
        facts.clone(),
    );
    replay_stage.finish("project_index_from_parts", format_args!(""));
    let replayed_callers = std::cell::Cell::new(0usize);
    let replayed_queries = std::cell::Cell::new(0usize);
    let mut result = JoinResult {
        rows: BTreeSet::new(),
        resolution_order: Vec::new(),
        unbound_non_utf8_paths,
    };
    let resolved_callers = bindings
        .keys()
        .filter(|path| {
            if selected.is_some_and(|set| !set.contains(*path)) {
                return false;
            }
            let Some((changed, _, _)) = reuse else {
                return true;
            };
            if selected.is_none() || changed.contains(*path) {
                return true;
            }
            cached.get(*path).is_none_or(|old| {
                replayed_callers.set(replayed_callers.get() + 1);
                old.references != bindings[*path].references
                    || old.surface_queries.iter().any(|(query, expected)| {
                        replayed_queries.set(replayed_queries.get() + 1);
                        query.answer(&index) != *expected
                    })
            })
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    replay_stage.finish(
        "surface_replay",
        format_args!(
            "selected={} replayed_callers={} replayed_queries={} resolved_callers={}",
            selected.map_or(0, BTreeSet::len),
            replayed_callers.get(),
            replayed_queries.get(),
            resolved_callers.len()
        ),
    );
    for (path, binding) in &mut bindings {
        if resolved_callers.contains(path) {
            binding.resolved_dependencies.clear();
            binding.surface_queries.clear();
            binding.resolution_facts = ConfigConsultations::default();
        } else if let Some(old) = cached.get(path) {
            binding.resolved_dependencies = old.resolved_dependencies.clone();
            binding.surface_queries = old.surface_queries.clone();
        }
    }
    profile.finish("index_and_surface_replay");
    let index = ManifestProjectIndex::from_parts(
        root,
        index.files,
        HashMap::new(),
        super::WorkspaceCratePrefixCache::default(),
        facts.clone(),
    );
    profile.finish("decode_resolved_callers");
    let bases: BTreeMap<_, BTreeSet<_>> = resolved_callers
        .iter()
        .map(|caller| {
            let binding = &bindings[caller];
            (
                caller.clone(),
                binding
                    .references
                    .values()
                    .flatten()
                    .chain(binding.binding_probes.iter())
                    .cloned()
                    .collect(),
            )
        })
        .collect();
    let mut resolved_bindings = 0;
    for caller in &resolved_callers {
        let path = RelPath::new(caller.as_bytes().to_vec()).expect("bound manifest path");
        let Some(ManifestEntry::Regular { planes, .. }) = manifest.get(&path) else {
            continue;
        };
        let key = planes.callgraph.as_ref().expect("bound caller key");
        let decoded_blob = decode(key)?;
        let CallgraphBlob::Parse(blob) = decoded_blob.as_ref() else {
            continue;
        };
        if !rebuilt_surface_paths.contains(caller) {
            decoded_caller_blobs += 1;
        }
        let extract =
            blob.bind_with_dependencies(caller, &paths, Some(&bindings[caller].references))?;
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
        let receiver_ordinals = blob
            .dispatch
            .sites
            .iter()
            .map(|s| s.ordinal)
            .collect::<BTreeSet<_>>();
        let caller_data = &extract.data;
        let surface_index = ViewSurfaceIndex {
            inner: &index,
            queries: Default::default(),
            caller: Some((caller, caller_data)),
        };
        let basis = &bases[caller];
        let binding = bindings.get_mut(caller).expect("bound caller dependencies");
        let mut resolutions =
            HashMap::<ViewResolutionBinding, (Option<String>, Option<String>)>::new();
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
                        let resolved = super::resolve_ref(raw, &surface_index)
                            .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
                        // The memo table is local to each caller. Extending the
                        // binding's facts once keeps its configuration dependencies
                        // associated with that caller even on cache hits.
                        binding.resolution_facts.extend(&facts.take_config());
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
            if record_order {
                result.resolution_order.push(key.clone());
            }
            result.rows.insert(DerivedRow {
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
        resolved_bindings += resolutions.len();
        binding.surface_queries = caller_queries.into_iter().collect();
    }
    result.resolution_order.sort();
    profile.finish("resolve_and_record");
    for binding in bindings.values_mut() {
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
    profile.finish("dependency_union");
    let mut dispatch_type_targets = BTreeMap::new();
    let mut dispatch_type_target_decoded_blobs = 0;
    let mut dispatch_type_target_traces = 0;
    let mut module_domain_traces = 0;
    if let Some(plan) = dispatch_type_target_plan {
        let mut stage = crate::views::materialization::profile::StageTimer::new("join");
        let (recompute, replayed) = match plan {
            DispatchTypeTargetPlan::All => (None, 0),
            DispatchTypeTargetPlan::Selected {
                recompute,
                membership_changed,
                module_domain_changed,
                traces,
            } => {
                let mut seeds = recompute.clone();
                seeds.extend(rebuilt_surface_paths.iter().cloned());
                seeds.extend(membership_changed.iter().cloned());
                if module_domain_changed {
                    seeds.insert(VIEW_RUST_MODULE_DOMAIN.to_string());
                }
                let traces = traces(&seeds)?;
                if module_domain_changed {
                    module_domain_traces = traces
                        .values()
                        .filter(|trace| {
                            trace.dependencies().iter().all(|dependency| {
                                dependency == VIEW_RUST_MODULE_DOMAIN || !seeds.contains(dependency)
                            })
                        })
                        .count();
                }
                dispatch_type_target_traces = traces.len();
                stage.finish(
                    "dispatch_type_target_traces",
                    format_args!("seeds={} traces={}", seeds.len(), traces.len()),
                );
                let mut files = recompute.clone();
                let mut answers = BTreeMap::new();
                for (file, trace) in &traces {
                    if !files.contains(file)
                        && (trace.probes_any(membership_changed)
                            || !trace.replays(&index, &seeds, &mut answers))
                    {
                        files.insert(file.clone());
                    }
                }
                (Some(files), traces.len())
            }
        };
        for (path, entry) in manifest.entries() {
            let ManifestEntry::Regular { planes, .. } = entry else {
                continue;
            };
            let Some(key) = &planes.callgraph else {
                continue;
            };
            let Ok(file) = std::str::from_utf8(path.as_bytes()) else {
                continue;
            };
            if recompute
                .as_ref()
                .is_some_and(|files| !files.contains(file))
            {
                continue;
            }
            let decoded_blob = decode(key)?;
            let CallgraphBlob::Parse(blob) = decoded_blob.as_ref() else {
                continue;
            };
            if blob.language != "rust" {
                continue;
            }
            // The caller data is the file's own parse; binding consults path
            // facts only for reference dependencies, which are not wanted here.
            if !rebuilt_surface_paths.contains(file) && !resolved_callers.contains(file) {
                dispatch_type_target_decoded_blobs += 1;
            }
            let data = blob.file_data(file)?;
            facts.take();
            facts.take_config();
            let recording = ViewSurfaceIndex {
                inner: &index,
                queries: Default::default(),
                caller: None,
            };
            let targets = resolve_dispatch_type_names(&recording, file, blob, &data);
            let probes = facts.take();
            facts.take_config();
            dispatch_type_targets.insert(
                file.to_string(),
                DispatchTypeTargets {
                    targets,
                    trace: DispatchTypeTargetTrace {
                        queries: recording.take().into_iter().collect(),
                        probes,
                    },
                },
            );
        }
        stage.finish(
            "dispatch_type_targets",
            format_args!(
                "replayed={replayed} resolved_files={} decoded={dispatch_type_target_decoded_blobs}",
                dispatch_type_targets.len()
            ),
        );
    }
    if std::env::var_os("AFT_VIEW_PROFILE").is_some() {
        eprintln!("view_profile join.surfaces restored={restored_surfaces} rebuilt_changed={rebuilt_changed} rebuilt_facts={rebuilt_facts} rebuilt_membership={rebuilt_membership} rebuilt_missing={rebuilt_missing} selected={} resolved={} skipped={}", selected.map_or(bindings.len(), BTreeSet::len), resolved_callers.len(), bindings.keys().filter(|path| selected.is_none_or(|set| set.contains(*path)) && !resolved_callers.contains(*path)).count());
    }
    if let Some(error) = read_error.borrow_mut().take() {
        return Err(error);
    }
    Ok(SelectedManifestJoin {
        result,
        bindings,
        resolved_callers,
        rebuilt_surface_entries: rebuilt_surface_paths.len(),
        rebuilt_surface_paths,
        decoded_caller_blobs,
        resolved_bindings,
        dispatch_type_targets,
        dispatch_type_target_decoded_blobs,
        dispatch_type_target_traces,
        module_domain_traces,
    })
}

/// Consumer-specific export surface: record the answers the resolver actually
/// used, rather than invalidating every caller when an unrelated export changes.
/// Querying these answers against a new index is cheaper than walking references
/// and is sound only while the caller's immutable parse blob remains unchanged.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
enum ViewSurfaceQuery {
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

pub(crate) const VIEW_CONFIG_MEMBERSHIP_DOMAIN: &str = "\0view:config-membership";
pub(crate) const VIEW_RUST_MODULE_DOMAIN: &str = "\0view:rust-module-index";
/// Recorded by every Go package file listing. A listing reads the directory,
/// not the files in it, so a file added to the package is no recorded
/// dependency of the callers that listed it; the owner rechecks this domain
/// whenever a `.go` file is added or removed.
pub(crate) const VIEW_GO_PACKAGE_DOMAIN: &str = "\0view:go-package-files";

impl ViewSurfaceQuery {
    fn dependencies(&self) -> Vec<String> {
        match self {
            Self::Crate(_) => vec![VIEW_CONFIG_MEMBERSHIP_DOMAIN.into()],
            Self::CrateRoot(file) => {
                vec![file.clone(), VIEW_CONFIG_MEMBERSHIP_DOMAIN.into()]
            }
            Self::Parent(file) | Self::Inline(file, ..) => {
                vec![file.clone(), VIEW_RUST_MODULE_DOMAIN.into()]
            }
            Self::GoPackage(..) => vec![VIEW_GO_PACKAGE_DOMAIN.into()],
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

struct ViewSurfaceIndex<'a, I> {
    inner: &'a I,
    queries: std::cell::RefCell<BTreeMap<ViewSurfaceQuery, String>>,
    caller: Option<(&'a str, &'a FileCallData)>,
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
        let value = self.inner.rust_crate_root_file(file);
        self.record(ViewSurfaceQuery::CrateRoot(file.into()), &value);
        value
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
        let manifest = Manifest::new([(
            RelPath::new(b"package.json".to_vec()).unwrap(),
            ManifestEntry::Regular {
                mode: 0o100644,
                planes: crate::views::RegularPlanes {
                    callgraph: Some("config".into()),
                    semantic: None,
                },
                resolution_input: true,
            },
        )])
        .unwrap();
        let bytes: Arc<[u8]> = CallgraphBlob::config(b"{}".to_vec(), "fixture")
            .to_bytes()
            .unwrap()
            .into();
        let reader = |_: &BlobKey| Some(bytes.clone());
        let facts = ViewBindingFacts::new(Rc::new(ManifestFacts {
            manifest: &manifest,
            blobs: &reader,
        }));
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

#[cfg(test)]
thread_local! {
    static CALLER_NODE_LOOKUP_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    // Select the original caller-node scan in tests so the benchmark can
    // compare scan and indexed lookup in the same process and host-load window.
    pub(crate) static SCAN_CALLER_NODES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[test]
fn binding_caller_node_lookup_is_linear_and_preserves_first_duplicate() {
    let source = (0..200)
        .map(|i| format!("export function f{i}() {{ f0(); }}\n"))
        .collect::<String>();
    let blob = CallgraphBlob::extract(&source, "typescript", "lookup-test").unwrap();
    let mut parse = blob.parse().unwrap().clone();
    let mut duplicate = parse.symbols[0].clone();
    duplicate.ordinal += 10000;
    parse.symbols.push(duplicate);
    let manifest = Manifest::new([]).unwrap();
    let reader = |_: &BlobKey| None;
    let facts = ManifestFacts {
        manifest: &manifest,
        blobs: &reader,
    };
    let paths = FactPaths {
        root: Path::new("/"),
        facts: &facts,
    };
    CALLER_NODE_LOOKUP_WORK.with(|work| work.set(0));
    let extract = parse
        .bind_with_dependencies("caller.ts", &paths, None)
        .unwrap();
    let work = CALLER_NODE_LOOKUP_WORK.with(std::cell::Cell::get);
    assert!(work > 0, "lookup work must actually be observed");
    assert!(
        work <= parse.symbols.len() + parse.refs.len(),
        "caller node lookup must be linear: {work} operations for {} symbols and {} refs",
        parse.symbols.len(),
        parse.refs.len()
    );
    for reference in &extract.raw_refs {
        let expected = reference.caller_symbol.as_ref().and_then(|caller| {
            extract
                .nodes
                .iter()
                .find(|node| &node.scoped_name == caller)
                .map(|node| node.id.clone())
        });
        assert_eq!(reference.caller_node, expected);
    }
}
