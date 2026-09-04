//! Manifest-only callgraph assembly.
//!
//! This module deliberately accepts a manifest plus immutable blob payloads rather
//! than a checkout root.  Extraction is content-addressed and path-free; binding a
//! blob to a manifest path and resolving its cross-file references happens here.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use tree_sitter::Parser;

use crate::callgraph::{self, FileCallData, SymbolMeta};
use crate::imports::{specifier_imported_name, specifier_local_name};
use crate::parser::{grammar_for, LangId};
use crate::symbols::SymbolKind;
use crate::views::{Manifest, ManifestEntry, RelPath};

const TOP_LEVEL_SYMBOL: &str = "<top-level>";
const SOURCE_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs", "rs", "py", "go",
];

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
}

/// Path-free parse output stored for a regular source file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ParseBlob {
    pub extractor_version: String,
    pub language: String,
    pub ast_nodes: Vec<AstPreorderNode>,
    pub symbols: Vec<BlobSymbol>,
    pub default_export_symbol: Option<String>,
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
        let data = callgraph::build_file_data_from_source_with_lang(
            std::path::Path::new("__callgraph_blob__"),
            source,
            lang,
        )
        .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
        let symbols = blob_symbols(source, &data, &ast_nodes);
        let imports = blob_imports(&data, &ast_nodes);
        let mut refs = blob_refs(source, &data, &ast_nodes);
        refs.extend(rust_module_refs(source, lang, &ast_nodes));
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

/// Resolves the current view solely from its manifest and manifest-addressed
/// payloads.  Entries are traversed in manifest byte order and references in
/// `(caller blob key, ref ordinal, caller path)` order.
pub fn join_manifest(
    manifest: &Manifest,
    blobs: &impl ManifestBlobReader,
) -> Result<JoinResult, ManifestJoinError> {
    let state = JoinState::from_manifest(manifest, blobs)?;
    let mut work = Vec::new();
    for file in state.files.values() {
        for raw in &file.blob.refs {
            work.push((
                CallerRefKey {
                    caller_blob_key: file.blob_key.clone(),
                    ref_ordinal: raw.ordinal,
                    caller_path: file.path.clone(),
                },
                raw,
            ));
        }
    }
    work.sort_by(|left, right| left.0.cmp(&right.0));

    let mut rows = BTreeSet::new();
    let mut resolution_order = Vec::with_capacity(work.len());
    for (key, raw) in work {
        resolution_order.push(key.clone());
        rows.insert(resolve_blob_ref(&state, &key, raw));
    }
    Ok(JoinResult {
        rows,
        resolution_order,
    })
}

/// Reassembles only rows affected by manifest changes. A changed source binding
/// refreshes its own references, prior references targeting a changed or removed
/// manifest path refresh through the reverse target index, and a changed
/// configuration or ignore input refreshes all rows. Lockfiles carry
/// `resolution_input = false`, so a lockfile-only change does not require a full
/// re-resolution.
pub fn join_manifest_incremental(
    previous_manifest: &Manifest,
    previous: &JoinResult,
    current_manifest: &Manifest,
    blobs: &impl ManifestBlobReader,
) -> Result<IncrementalJoinResult, ManifestJoinError> {
    let changed_paths = changed_manifest_paths(previous_manifest, current_manifest);
    let full_re_resolve = changed_paths.iter().any(|path| {
        manifest_resolution_input(previous_manifest, path)
            || manifest_resolution_input(current_manifest, path)
    });
    let fresh = join_manifest(current_manifest, blobs)?;
    let current_by_key = fresh
        .rows
        .iter()
        .cloned()
        .map(|row| (row.ref_key(), row))
        .collect::<BTreeMap<_, _>>();
    let previous_by_key = previous
        .rows
        .iter()
        .cloned()
        .map(|row| (row.ref_key(), row))
        .collect::<BTreeMap<_, _>>();

    let mut re_resolved = BTreeSet::new();
    if full_re_resolve {
        re_resolved.extend(fresh.resolution_order.iter().cloned());
    } else {
        for key in &fresh.resolution_order {
            if changed_paths.contains(&key.caller_path) {
                re_resolved.insert(key.clone());
            }
        }
        for row in &previous.rows {
            if row
                .target_path
                .as_ref()
                .is_some_and(|path| changed_paths.contains(path))
                && current_by_key.contains_key(&row.ref_key())
            {
                re_resolved.insert(row.ref_key());
            }
        }
    }

    let mut rows = BTreeSet::new();
    for key in &fresh.resolution_order {
        if !re_resolved.contains(key) {
            if let Some(previous_row) = previous_by_key.get(key) {
                rows.insert(previous_row.clone());
                continue;
            }
        }
        if let Some(row) = current_by_key.get(key) {
            rows.insert(row.clone());
        }
    }

    Ok(IncrementalJoinResult {
        result: JoinResult {
            rows,
            resolution_order: fresh.resolution_order,
        },
        re_resolved,
        full_re_resolve,
    })
}

#[derive(Clone)]
struct FileBinding {
    path: Vec<u8>,
    blob_key: String,
    blob: ParseBlob,
}

#[derive(Clone)]
enum ManifestMember {
    File,
    Symlink(Vec<u8>),
    Gitlink,
    Other,
}

#[derive(Default)]
struct ConfigInputs {
    ts_paths: Vec<TsPathRule>,
    packages: Vec<PackageRoot>,
}

struct TsPathRule {
    config_dir: Vec<u8>,
    base_url: String,
    alias: String,
    targets: Vec<String>,
}

struct PackageRoot {
    name: String,
    root: Vec<u8>,
}

struct JoinState {
    files: BTreeMap<Vec<u8>, FileBinding>,
    members: BTreeMap<Vec<u8>, ManifestMember>,
    config: ConfigInputs,
}

impl JoinState {
    fn from_manifest(
        manifest: &Manifest,
        blobs: &impl ManifestBlobReader,
    ) -> Result<Self, ManifestJoinError> {
        let mut decoded = BTreeMap::<String, CallgraphBlob>::new();
        let mut files = BTreeMap::new();
        let mut members = BTreeMap::new();
        let mut config = ConfigInputs::default();

        for (rel_path, entry) in manifest.entries() {
            let path = rel_path.as_bytes().to_vec();
            match entry {
                ManifestEntry::Regular {
                    planes,
                    resolution_input,
                    ..
                } => {
                    let Some(key) = planes.callgraph.as_deref() else {
                        members.insert(path, ManifestMember::Other);
                        continue;
                    };
                    let blob = decoded_blob(&mut decoded, blobs, key)?;
                    match blob {
                        CallgraphBlob::Parse(parse) if !resolution_input => {
                            files.insert(
                                path.clone(),
                                FileBinding {
                                    path: path.clone(),
                                    blob_key: key.to_string(),
                                    blob: parse.clone(),
                                },
                            );
                            members.insert(path, ManifestMember::File);
                        }
                        CallgraphBlob::Config(config_blob) if *resolution_input => {
                            add_config_input(&mut config, &path, &config_blob)?;
                            members.insert(path, ManifestMember::Other);
                        }
                        CallgraphBlob::Config(_) => {
                            members.insert(path, ManifestMember::Other);
                        }
                        CallgraphBlob::Parse(_) => {
                            return Err(ManifestJoinError::InvalidConfig {
                                path,
                                reason: "resolution input must use language=config".to_string(),
                            });
                        }
                    }
                }
                ManifestEntry::Synthetic { planes, .. } => {
                    let blob = decoded_blob(&mut decoded, blobs, &planes.callgraph)?;
                    let Some(config_blob) = blob.config_source() else {
                        return Err(ManifestJoinError::InvalidConfig {
                            path,
                            reason: "synthetic input must use language=config".to_string(),
                        });
                    };
                    add_config_input(&mut config, &path, config_blob)?;
                    members.insert(rel_path.as_bytes().to_vec(), ManifestMember::Other);
                }
                ManifestEntry::Symlink { target_bytes } => {
                    members.insert(
                        path,
                        ManifestMember::Symlink(target_bytes.as_bytes().to_vec()),
                    );
                }
                ManifestEntry::Gitlink { .. } => {
                    members.insert(path, ManifestMember::Gitlink);
                }
            }
        }

        config.ts_paths.sort_by(|left, right| {
            (&left.config_dir, &left.alias, &left.targets).cmp(&(
                &right.config_dir,
                &right.alias,
                &right.targets,
            ))
        });
        config
            .packages
            .sort_by(|left, right| (&left.name, &left.root).cmp(&(&right.name, &right.root)));
        Ok(Self {
            files,
            members,
            config,
        })
    }

    fn resolve_module(&self, caller_path: &[u8], module_path: &str) -> Option<&FileBinding> {
        if module_path.starts_with('.') {
            return self.resolve_file_like(&join_manifest_path(
                &parent_path(caller_path),
                module_path.as_bytes(),
            ));
        }
        if module_path.starts_with('/') {
            return None;
        }

        for rule in &self.config.ts_paths {
            let Some(capture) = ts_path_capture(&rule.alias, module_path) else {
                continue;
            };
            for target in &rule.targets {
                let target = target.replace('*', capture);
                let base = join_manifest_path(&rule.config_dir, rule.base_url.as_bytes());
                if let Some(file) =
                    self.resolve_file_like(&join_manifest_path(&base, target.as_bytes()))
                {
                    return Some(file);
                }
            }
        }

        for package in &self.config.packages {
            let Some(suffix) = package_suffix(module_path, &package.name) else {
                continue;
            };
            let base = join_manifest_path(&package.root, suffix.as_bytes());
            if let Some(file) = self.resolve_file_like(&base) {
                return Some(file);
            }
            if suffix.is_empty() {
                let source_root = join_manifest_path(&package.root, b"src");
                if let Some(file) = self.resolve_file_like(&source_root) {
                    return Some(file);
                }
            }
        }
        None
    }

    fn resolve_file_like(&self, base: &[u8]) -> Option<&FileBinding> {
        if let Some(file) = self.resolve_member_file(base) {
            return Some(file);
        }
        if !base.contains(&b'.') {
            for extension in SOURCE_EXTENSIONS {
                let mut candidate = base.to_vec();
                candidate.push(b'.');
                candidate.extend_from_slice(extension.as_bytes());
                if let Some(file) = self.resolve_member_file(&candidate) {
                    return Some(file);
                }
            }
        }
        for extension in SOURCE_EXTENSIONS {
            let mut candidate = base.to_vec();
            if !candidate.is_empty() {
                candidate.push(b'/');
            }
            candidate.extend_from_slice(b"index.");
            candidate.extend_from_slice(extension.as_bytes());
            if let Some(file) = self.resolve_member_file(&candidate) {
                return Some(file);
            }
        }
        None
    }

    fn resolve_member_file(&self, candidate: &[u8]) -> Option<&FileBinding> {
        let mut current = normalize_manifest_path(candidate);
        let mut seen = BTreeSet::new();
        while seen.insert(current.clone()) {
            if let Some(ManifestMember::File) = self.members.get(&current) {
                return self.files.get(&current);
            }
            if matches!(self.members.get(&current), Some(ManifestMember::Gitlink)) {
                return None;
            }
            if let Some(ManifestMember::Symlink(target)) = self.members.get(&current) {
                current = join_manifest_path(&parent_path(&current), target);
                continue;
            }

            let mut rewritten = None;
            for split in slash_positions(&current) {
                let prefix = &current[..split];
                let Some(ManifestMember::Symlink(target)) = self.members.get(prefix) else {
                    continue;
                };
                let mut target_path = join_manifest_path(&parent_path(prefix), target);
                if split < current.len() {
                    target_path.push(b'/');
                    target_path.extend_from_slice(&current[split + 1..]);
                }
                rewritten = Some(target_path);
                break;
            }
            let Some(next) = rewritten else {
                return None;
            };
            current = next;
        }
        None
    }
}

fn decoded_blob(
    decoded: &mut BTreeMap<String, CallgraphBlob>,
    blobs: &impl ManifestBlobReader,
    key: &str,
) -> Result<CallgraphBlob, ManifestJoinError> {
    if let Some(blob) = decoded.get(key) {
        return Ok(blob.clone());
    }
    let bytes = blobs
        .read_callgraph_blob(key)?
        .ok_or_else(|| ManifestJoinError::MissingBlob(key.to_string()))?;
    let blob = CallgraphBlob::from_bytes(&bytes)?;
    decoded.insert(key.to_string(), blob.clone());
    Ok(blob)
}

fn add_config_input(
    inputs: &mut ConfigInputs,
    path: &[u8],
    blob: &ConfigBlob,
) -> Result<(), ManifestJoinError> {
    if blob.language != "config" {
        return Err(ManifestJoinError::InvalidConfig {
            path: path.to_vec(),
            reason: "configuration blob language must be config".to_string(),
        });
    }
    let path_text = String::from_utf8_lossy(path);
    if path_text.ends_with("tsconfig.json")
        || path_text.contains("/tsconfig.")
        || path_text.ends_with("jsconfig.json")
    {
        let value: serde_json::Value = serde_json::from_slice(&blob.source).map_err(|error| {
            ManifestJoinError::InvalidConfig {
                path: path.to_vec(),
                reason: error.to_string(),
            }
        })?;
        let compiler_options = value
            .get("compilerOptions")
            .and_then(serde_json::Value::as_object);
        let base_url = compiler_options
            .and_then(|options| options.get("baseUrl"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(".");
        if let Some(paths) = compiler_options
            .and_then(|options| options.get("paths"))
            .and_then(serde_json::Value::as_object)
        {
            for (alias, targets) in paths {
                let Some(targets) = targets.as_array() else {
                    continue;
                };
                let targets = targets
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                if !targets.is_empty() {
                    inputs.ts_paths.push(TsPathRule {
                        config_dir: parent_path(path),
                        base_url: base_url.to_string(),
                        alias: alias.clone(),
                        targets,
                    });
                }
            }
        }
    } else if path_text.ends_with("package.json") {
        let value: serde_json::Value = serde_json::from_slice(&blob.source).map_err(|error| {
            ManifestJoinError::InvalidConfig {
                path: path.to_vec(),
                reason: error.to_string(),
            }
        })?;
        if let Some(name) = value.get("name").and_then(serde_json::Value::as_str) {
            inputs.packages.push(PackageRoot {
                name: name.to_string(),
                root: parent_path(path),
            });
        }
    } else if path_text.ends_with("Cargo.toml") {
        let source = std::str::from_utf8(&blob.source).map_err(|error| {
            ManifestJoinError::InvalidConfig {
                path: path.to_vec(),
                reason: error.to_string(),
            }
        })?;
        let value: toml::Value =
            toml::from_str(source).map_err(|error| ManifestJoinError::InvalidConfig {
                path: path.to_vec(),
                reason: error.to_string(),
            })?;
        if let Some(name) = value
            .get("package")
            .and_then(toml::Value::as_table)
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
        {
            inputs.packages.push(PackageRoot {
                name: name.replace('-', "_"),
                root: parent_path(path),
            });
        }
    }
    Ok(())
}

fn resolve_blob_ref(state: &JoinState, key: &CallerRefKey, raw: &BlobRef) -> DerivedRow {
    let target = state
        .files
        .get(&key.caller_path)
        .and_then(|caller| match raw.kind {
            BlobRefKind::Import | BlobRefKind::Module => raw
                .module_path
                .as_deref()
                .and_then(|module| state.resolve_module(&caller.path, module))
                .map(|file| (file.path.clone(), None)),
            BlobRefKind::Call | BlobRefKind::ValueRef => resolve_callable_ref(state, caller, raw),
        });
    let status = if target.is_some() {
        ResolutionStatus::Resolved
    } else {
        ResolutionStatus::Unresolved
    };
    DerivedRow {
        caller_blob_key: key.caller_blob_key.clone(),
        ref_ordinal: key.ref_ordinal,
        caller_path: key.caller_path.clone(),
        kind: raw.kind,
        status,
        target_path: target.as_ref().map(|(path, _)| path.clone()),
        target_symbol: target.and_then(|(_, symbol)| symbol),
    }
}

fn resolve_callable_ref(
    state: &JoinState,
    caller: &FileBinding,
    raw: &BlobRef,
) -> Option<(Vec<u8>, Option<String>)> {
    let short_name = raw.short_name.as_deref()?;
    let full_ref = raw.full_ref.as_deref().unwrap_or(short_name);

    if let Some((namespace, member)) = full_ref.split_once('.') {
        if let Some(import) = caller
            .blob
            .imports
            .iter()
            .find(|import| import.namespace_import.as_deref() == Some(namespace))
        {
            if let Some(target) = state.resolve_module(&caller.path, &import.module_path) {
                return Some((
                    target.path.clone(),
                    exported_symbol(target, member).or_else(|| Some(member.to_string())),
                ));
            }
        }
    }

    for import in &caller.blob.imports {
        if let Some(specifier) = import
            .names
            .iter()
            .find(|specifier| specifier_local_name(specifier) == short_name)
        {
            if let Some(target) = state.resolve_module(&caller.path, &import.module_path) {
                let requested = specifier_imported_name(specifier);
                return Some((
                    target.path.clone(),
                    exported_symbol(target, requested).or_else(|| Some(requested.to_string())),
                ));
            }
        }
        if import.default_import.as_deref() == Some(short_name) {
            if let Some(target) = state.resolve_module(&caller.path, &import.module_path) {
                let symbol = target
                    .blob
                    .default_export_symbol
                    .clone()
                    .or_else(|| Some("default".to_string()));
                return Some((target.path.clone(), symbol));
            }
        }
    }

    for import in &caller.blob.imports {
        if let Some(target) = state.resolve_module(&caller.path, &import.module_path) {
            if let Some(symbol) = exported_symbol(target, short_name) {
                return Some((target.path.clone(), Some(symbol)));
            }
        }
    }

    caller
        .blob
        .symbols
        .iter()
        .find(|symbol| symbol.name == short_name || symbol.scoped_name == full_ref)
        .map(|symbol| (caller.path.clone(), Some(symbol.scoped_name.clone())))
}

fn exported_symbol(binding: &FileBinding, requested: &str) -> Option<String> {
    if requested == "default" {
        return binding.blob.default_export_symbol.clone();
    }
    binding
        .blob
        .symbols
        .iter()
        .find(|symbol| {
            symbol.exported && (symbol.name == requested || symbol.scoped_name == requested)
        })
        .map(|symbol| symbol.scoped_name.clone())
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

fn parent_path(path: &[u8]) -> Vec<u8> {
    path.rsplitn(2, |byte| *byte == b'/')
        .nth(1)
        .map_or_else(Vec::new, |parent| parent.to_vec())
}

fn join_manifest_path(base: &[u8], tail: &[u8]) -> Vec<u8> {
    let mut joined = base.to_vec();
    if !joined.is_empty() && !tail.is_empty() {
        joined.push(b'/');
    }
    joined.extend_from_slice(tail);
    normalize_manifest_path(&joined)
}

fn normalize_manifest_path(path: &[u8]) -> Vec<u8> {
    let mut parts = Vec::<&[u8]>::new();
    for part in path.split(|byte| *byte == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let mut normalized = Vec::new();
    for part in parts {
        if !normalized.is_empty() {
            normalized.push(b'/');
        }
        normalized.extend_from_slice(part);
    }
    normalized
}

fn slash_positions(path: &[u8]) -> impl Iterator<Item = usize> + '_ {
    path.iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == b'/').then_some(index))
}

fn ts_path_capture<'a>(alias: &str, module_path: &'a str) -> Option<&'a str> {
    match alias.split_once('*') {
        Some((prefix, suffix))
            if module_path.starts_with(prefix) && module_path.ends_with(suffix) =>
        {
            let end = module_path.len().saturating_sub(suffix.len());
            Some(&module_path[prefix.len()..end])
        }
        None if alias == module_path => Some(""),
        _ => None,
    }
}

fn package_suffix(module_path: &str, package_name: &str) -> Option<String> {
    if module_path == package_name {
        return Some(String::new());
    }
    module_path
        .strip_prefix(package_name)
        .and_then(|suffix| suffix.strip_prefix('/'))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::{ManifestEntry, RegularPlanes};

    #[derive(Default)]
    struct MemoryBlobs(BTreeMap<String, Vec<u8>>);

    impl ManifestBlobReader for MemoryBlobs {
        fn read_callgraph_blob(
            &self,
            full_key: &str,
        ) -> Result<Option<Vec<u8>>, ManifestJoinError> {
            Ok(self.0.get(full_key).cloned())
        }
    }

    fn source_entry(key: &str) -> ManifestEntry {
        ManifestEntry::Regular {
            mode: 0o100644,
            planes: RegularPlanes {
                semantic: None,
                callgraph: Some(key.to_string()),
            },
            resolution_input: false,
        }
    }

    fn config_entry(key: &str) -> ManifestEntry {
        ManifestEntry::Regular {
            mode: 0o100644,
            planes: RegularPlanes {
                semantic: None,
                callgraph: Some(key.to_string()),
            },
            resolution_input: true,
        }
    }

    fn put_parse(blobs: &mut MemoryBlobs, key: &str, source: &str, language: &str) {
        let payload = CallgraphBlob::extract(source, language, "join-test-v1")
            .unwrap()
            .to_bytes()
            .unwrap();
        blobs.0.insert(key.to_string(), payload);
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
    }

    #[test]
    fn join_uses_manifest_entries_blobs_and_symlink_targets() {
        let mut blobs = MemoryBlobs::default();
        put_parse(
            &mut blobs,
            "caller",
            "import { target } from './link'; export function caller() { target(); }",
            "typescript",
        );
        put_parse(
            &mut blobs,
            "target",
            "export function target() {}",
            "typescript",
        );
        let manifest = Manifest::new([
            (
                RelPath::new(b"src/caller.ts".to_vec()).unwrap(),
                source_entry("caller"),
            ),
            (
                RelPath::new(b"src/link.ts".to_vec()).unwrap(),
                ManifestEntry::Symlink {
                    target_bytes: b"target.ts".as_slice().into(),
                },
            ),
            (
                RelPath::new(b"src/target.ts".to_vec()).unwrap(),
                source_entry("target"),
            ),
            (
                RelPath::new(b"vendor".to_vec()).unwrap(),
                ManifestEntry::Gitlink {
                    oid: "0123456789012345678901234567890123456789".to_string(),
                },
            ),
        ])
        .unwrap();

        let joined = join_manifest(&manifest, &blobs).unwrap();
        assert!(joined.rows.iter().any(|row| {
            row.caller_path == b"src/caller.ts"
                && row.target_path.as_deref() == Some(b"src/target.ts".as_slice())
                && row.target_symbol.as_deref() == Some("target")
        }));
    }

    #[test]
    fn equal_manifests_have_equal_logical_rows_despite_blob_insertion_order() {
        let mut first = MemoryBlobs::default();
        let mut second = MemoryBlobs::default();
        for (blobs, keys) in [
            (&mut first, ["caller", "target"]),
            (&mut second, ["target", "caller"]),
        ] {
            for key in keys {
                let source = if key == "caller" {
                    "import { target } from './target'; export function caller() { target(); }"
                } else {
                    "export function target() {}"
                };
                put_parse(blobs, key, source, "typescript");
            }
        }
        let first_manifest = Manifest::new([
            (
                RelPath::new(b"src/caller.ts".to_vec()).unwrap(),
                source_entry("caller"),
            ),
            (
                RelPath::new(b"src/target.ts".to_vec()).unwrap(),
                source_entry("target"),
            ),
        ])
        .unwrap();
        let second_manifest = Manifest::new([
            (
                RelPath::new(b"src/target.ts".to_vec()).unwrap(),
                source_entry("target"),
            ),
            (
                RelPath::new(b"src/caller.ts".to_vec()).unwrap(),
                source_entry("caller"),
            ),
        ])
        .unwrap();
        let first = join_manifest(&first_manifest, &first).unwrap();
        let second = join_manifest(&second_manifest, &second).unwrap();
        assert_eq!(first.rows, second.rows);
        assert_eq!(
            first.canonical_serialization(),
            second.canonical_serialization()
        );
        assert!(first
            .resolution_order
            .windows(2)
            .all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn incremental_join_limits_target_invalidation_and_resolves_config_from_blobs() {
        let mut blobs = MemoryBlobs::default();
        put_parse(
            &mut blobs,
            "caller-a",
            "import { target } from '@/target'; export function callerA() { target(); }",
            "typescript",
        );
        put_parse(
            &mut blobs,
            "caller-b",
            "export function callerB() { helperB(); } function helperB() {}",
            "typescript",
        );
        put_parse(
            &mut blobs,
            "target-old",
            "export function target() { helper(); } function helper() {}",
            "typescript",
        );
        put_parse(
            &mut blobs,
            "target-new",
            "export function target() { helper(); return; } function helper() {}",
            "typescript",
        );
        blobs.0.insert(
            "config".to_string(),
            CallgraphBlob::config(
                br#"{"compilerOptions": {"baseUrl": ".", "paths": {"@/*": ["src/*"]}}}"#,
                "join-test-v1",
            )
            .to_bytes()
            .unwrap(),
        );
        blobs.0.insert(
            "config-new".to_string(),
            CallgraphBlob::config(
                br#"{"compilerOptions": {"baseUrl": ".", "paths": {"@/*": ["src/*"]}}}"#,
                "join-test-v1",
            )
            .to_bytes()
            .unwrap(),
        );
        let previous_manifest = Manifest::new([
            (
                RelPath::new(b"src/caller-a.ts".to_vec()).unwrap(),
                source_entry("caller-a"),
            ),
            (
                RelPath::new(b"src/caller-b.ts".to_vec()).unwrap(),
                source_entry("caller-b"),
            ),
            (
                RelPath::new(b"src/target.ts".to_vec()).unwrap(),
                source_entry("target-old"),
            ),
            (
                RelPath::new(b"tsconfig.json".to_vec()).unwrap(),
                config_entry("config"),
            ),
            (
                RelPath::new(b"Cargo.lock".to_vec()).unwrap(),
                ManifestEntry::Regular {
                    mode: 0o100644,
                    planes: RegularPlanes {
                        semantic: None,
                        callgraph: None,
                    },
                    resolution_input: false,
                },
            ),
        ])
        .unwrap();
        let previous = join_manifest(&previous_manifest, &blobs).unwrap();
        assert!(previous.rows.iter().any(|row| {
            row.caller_path == b"src/caller-a.ts"
                && row.target_path.as_deref() == Some(b"src/target.ts".as_slice())
        }));

        let current_manifest = Manifest::new([
            (
                RelPath::new(b"src/caller-a.ts".to_vec()).unwrap(),
                source_entry("caller-a"),
            ),
            (
                RelPath::new(b"src/caller-b.ts".to_vec()).unwrap(),
                source_entry("caller-b"),
            ),
            (
                RelPath::new(b"src/target.ts".to_vec()).unwrap(),
                source_entry("target-new"),
            ),
            (
                RelPath::new(b"tsconfig.json".to_vec()).unwrap(),
                config_entry("config"),
            ),
            (
                RelPath::new(b"Cargo.lock".to_vec()).unwrap(),
                ManifestEntry::Regular {
                    mode: 0o100644,
                    planes: RegularPlanes {
                        semantic: None,
                        callgraph: None,
                    },
                    resolution_input: false,
                },
            ),
        ])
        .unwrap();
        let incremental =
            join_manifest_incremental(&previous_manifest, &previous, &current_manifest, &blobs)
                .unwrap();
        assert!(!incremental.full_re_resolve);
        assert!(incremental
            .re_resolved
            .iter()
            .any(|key| key.caller_path == b"src/target.ts"));
        assert!(incremental
            .re_resolved
            .iter()
            .any(|key| key.caller_path == b"src/caller-a.ts"));
        assert!(!incremental
            .re_resolved
            .iter()
            .any(|key| key.caller_path == b"src/caller-b.ts"));

        let config_changed = Manifest::new([
            (
                RelPath::new(b"src/caller-a.ts".to_vec()).unwrap(),
                source_entry("caller-a"),
            ),
            (
                RelPath::new(b"src/caller-b.ts".to_vec()).unwrap(),
                source_entry("caller-b"),
            ),
            (
                RelPath::new(b"src/target.ts".to_vec()).unwrap(),
                source_entry("target-new"),
            ),
            (
                RelPath::new(b"tsconfig.json".to_vec()).unwrap(),
                config_entry("config-new"),
            ),
            (
                RelPath::new(b"Cargo.lock".to_vec()).unwrap(),
                ManifestEntry::Regular {
                    mode: 0o100644,
                    planes: RegularPlanes {
                        semantic: None,
                        callgraph: None,
                    },
                    resolution_input: false,
                },
            ),
        ])
        .unwrap();
        let full = join_manifest_incremental(
            &current_manifest,
            &incremental.result,
            &config_changed,
            &blobs,
        )
        .unwrap();
        assert!(full.full_re_resolve);
        assert_eq!(full.re_resolved.len(), full.result.resolution_order.len());
    }

    #[test]
    fn lockfile_only_change_does_not_force_resolution() {
        let mut blobs = MemoryBlobs::default();
        put_parse(
            &mut blobs,
            "caller",
            "export function caller() { helper(); } function helper() {}",
            "typescript",
        );
        for key in ["lock-old", "lock-new"] {
            blobs.0.insert(
                key.to_string(),
                CallgraphBlob::config(b"lockfile", "join-test-v1")
                    .to_bytes()
                    .unwrap(),
            );
        }
        let lock = |key: &str| ManifestEntry::Regular {
            mode: 0o100644,
            planes: RegularPlanes {
                semantic: None,
                callgraph: Some(key.to_string()),
            },
            resolution_input: false,
        };
        let previous_manifest = Manifest::new([
            (
                RelPath::new(b"src/caller.ts".to_vec()).unwrap(),
                source_entry("caller"),
            ),
            (
                RelPath::new(b"Cargo.lock".to_vec()).unwrap(),
                lock("lock-old"),
            ),
        ])
        .unwrap();
        let current_manifest = Manifest::new([
            (
                RelPath::new(b"src/caller.ts".to_vec()).unwrap(),
                source_entry("caller"),
            ),
            (
                RelPath::new(b"Cargo.lock".to_vec()).unwrap(),
                lock("lock-new"),
            ),
        ])
        .unwrap();
        let previous = join_manifest(&previous_manifest, &blobs).unwrap();
        let incremental =
            join_manifest_incremental(&previous_manifest, &previous, &current_manifest, &blobs)
                .unwrap();
        assert!(!incremental.full_re_resolve);
        assert!(incremental.re_resolved.is_empty());
        assert_eq!(incremental.result.rows, previous.rows);
    }

    #[test]
    fn join_does_not_probe_live_files() {
        let source = include_str!("join.rs");
        let filesystem = ["std::", "fs"].concat();
        let normalization = ["canonical", "ize"].concat();
        assert!(!source.contains(&filesystem));
        assert!(!source.contains(&normalization));
    }
}
