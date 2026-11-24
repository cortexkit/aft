use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const OXC_PROVENANCE: &str = "oxc";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum ExportName {
    Named(String),
    Default,
}

impl ExportName {
    pub fn matches_str(&self, name: &str) -> bool {
        match self {
            Self::Named(value) => value == name,
            Self::Default => name == "default",
        }
    }

    pub fn as_symbol(&self) -> String {
        match self {
            Self::Named(value) => value.clone(),
            Self::Default => "default".to_string(),
        }
    }
}

impl fmt::Display for ExportName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) => f.write_str(name),
            Self::Default => f.write_str("default"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportKind {
    Named,
    Default,
    Namespace,
    SideEffect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReExportKind {
    Named,
    Star,
    Namespace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecoratorFact {
    pub name: String,
    pub segments: Vec<String>,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportFact {
    pub name: ExportName,
    pub local_name: Option<String>,
    pub kind: String,
    pub is_type_only: bool,
    pub line: u32,
    pub declared: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<DecoratorFact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportFact {
    pub usage: ExportUsage,
    pub source: String,
    pub kind: ImportKind,
    pub imported_name: Option<String>,
    pub local_name: Option<String>,
    pub is_type_only: bool,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReExportFact {
    pub source: String,
    pub kind: ReExportKind,
    pub imported_name: Option<String>,
    pub exported_name: Option<String>,
    pub is_type_only: bool,
    pub line: u32,
}

/// Export references selected by a namespace access or dynamic import result.
/// `all` is conservative whole-namespace use; named accesses remain precise.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportUsage {
    pub names: BTreeSet<String>,
    pub all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicImportFact {
    pub source: Option<String>,
    pub is_literal: bool,
    pub usage: ExportUsage,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileFacts {
    pub file_id: FileId,
    pub path: PathBuf,
    pub content_hash: String,
    pub exports: Vec<ExportFact>,
    pub imports: Vec<ImportFact>,
    pub re_exports: Vec<ReExportFact>,
    pub dynamic_imports: Vec<DynamicImportFact>,
    pub same_file_value_references: BTreeSet<String>,
    pub used_import_bindings: BTreeSet<String>,
    pub type_referenced_import_bindings: BTreeSet<String>,
    pub value_referenced_import_bindings: BTreeSet<String>,
    pub parse_error: Option<String>,
}

impl FileFacts {
    pub fn empty(
        file_id: FileId,
        path: PathBuf,
        content_hash: String,
        parse_error: String,
    ) -> Self {
        Self {
            file_id,
            path,
            content_hash,
            exports: Vec::new(),
            imports: Vec::new(),
            re_exports: Vec::new(),
            dynamic_imports: Vec::new(),
            same_file_value_references: BTreeSet::new(),
            used_import_bindings: BTreeSet::new(),
            type_referenced_import_bindings: BTreeSet::new(),
            value_referenced_import_bindings: BTreeSet::new(),
            parse_error: Some(parse_error),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LivenessVerdict {
    Used,
    Unused,
    Uncertain,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OxcReExportContext {
    pub file: String,
    pub line: u32,
    pub exported_name: String,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OxcExportVerdict {
    pub symbol: String,
    pub kind: String,
    pub line: u32,
    pub verdict: LivenessVerdict,
    pub reason: String,
    pub provenance: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_references: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub test_only_reference_files: Vec<String>,
    /// The export is `Used` only because its own file references it; no
    /// other module imports or re-exports it. Dead-code analysis treats it as
    /// live, while unused-export analysis reports it: the `export` keyword can
    /// be dropped. Never set for type-only exports (`type`, `interface`),
    /// which an exported signature in the same file may need.
    #[serde(default, skip_serializing_if = "is_false")]
    pub only_same_file_references: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_reexported: Vec<OxcReExportContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OxcFileVerdicts {
    pub file: PathBuf,
    pub relative_file: String,
    pub exports: Vec<OxcExportVerdict>,
}

impl OxcFileVerdicts {
    pub fn contribution_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "file": self.relative_file,
            "exports": self.exports,
            "provenance": OXC_PROVENANCE,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolverConfigInput {
    pub path: PathBuf,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OxcResolvedEdge {
    pub from_file: PathBuf,
    pub specifier: String,
    pub resolved_file: Option<PathBuf>,
    pub kind: String,
    pub line: u32,
    pub is_type_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OxcEngineStats {
    pub files: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub resolved_edges: usize,
    pub unresolved_edges: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OxcEngineError {
    pub file: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OxcEngineResult {
    pub files: Vec<OxcFileVerdicts>,
    #[serde(default)]
    pub facts: Vec<FileFacts>,
    pub resolver_config_inputs: Vec<ResolverConfigInput>,
    pub resolver_config_fingerprint: String,
    pub edges: Vec<OxcResolvedEdge>,
    pub stats: OxcEngineStats,
    pub errors: Vec<OxcEngineError>,
    #[serde(default)]
    pub skipped_outside_root: Vec<PathBuf>,
}

impl OxcEngineResult {
    pub fn resolver_config_fingerprint(&self) -> &str {
        &self.resolver_config_fingerprint
    }
}
