//! Closed corpus schema and reusable validation for the bash rewrite campaign.
//!
//! The process-level runner lives in the integration test because it needs the
//! public AFT binary. This module owns the versioned schema, fixture
//! materialization, inventories, and aggregate failure formatting so schema
//! checks also run on Windows without executing Unix utilities.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use super::catalog;
use super::observation::{deterministic_filesystem_manifest, FilesystemManifest};
use serde::de::DeserializeOwned;
use serde::Deserialize;

pub const CORPUS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    pub schema_version: u32,
    pub corpus_id: String,
    pub rows: Vec<CorpusRow>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusRow {
    pub id: String,
    pub command: String,
    pub route: String,
    pub basis: String,
    #[serde(default)]
    pub normalizations: Vec<String>,
    #[serde(default)]
    pub expectations: Vec<String>,
    #[serde(default)]
    pub platform: PlatformGate,
    pub decision_class: Option<String>,
    #[serde(default)]
    pub branch_ids: Vec<String>,
    pub semantic_dimensions: Vec<String>,
    #[serde(default = "default_characterization_mode")]
    pub mode: String,
    #[serde(default)]
    pub mutating: bool,
    #[serde(default)]
    pub manifest: Vec<ManifestEntrySpec>,
    #[serde(default)]
    pub workdir: String,
}

fn default_characterization_mode() -> String {
    "characterization-only".to_string()
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PlatformGate {
    #[default]
    All,
    Unix,
    Macos,
    Linux,
    Windows,
}

impl PlatformGate {
    pub fn enabled_on_host(self) -> bool {
        match self {
            Self::All => true,
            Self::Unix => cfg!(unix),
            Self::Macos => cfg!(target_os = "macos"),
            Self::Linux => cfg!(target_os = "linux"),
            Self::Windows => cfg!(windows),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntrySpec {
    pub path: String,
    #[serde(default = "default_file_kind")]
    pub kind: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub content_base64: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
}

fn default_file_kind() -> String {
    "file".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRoute {
    pub native: bool,
    pub rule_id: Option<String>,
}

impl CorpusRow {
    pub fn parsed_route(&self) -> Result<ParsedRoute, String> {
        if self.route == "native" {
            return Ok(ParsedRoute {
                native: true,
                rule_id: None,
            });
        }
        let Some(rule_id) = self.route.strip_prefix("rewritten:") else {
            return Err(format!("row {} has invalid route {}", self.id, self.route));
        };
        if !catalog::rule_exists(rule_id) {
            return Err(format!("row {} names unknown rule {rule_id}", self.id));
        }
        Ok(ParsedRoute {
            native: false,
            rule_id: Some(rule_id.to_string()),
        })
    }

    pub fn materialize(&self, root: &Path) -> Result<(), String> {
        validate_fixture_entries(&self.manifest)?;
        for entry in &self.manifest {
            let path = safe_fixture_path(root, &entry.path)?;
            match entry.kind.as_str() {
                "directory" => fs::create_dir_all(&path)
                    .map_err(|error| format!("create {}: {error}", entry.path))?,
                "file" => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)
                            .map_err(|error| format!("create parent {}: {error}", entry.path))?;
                    }
                    let bytes = entry_bytes(entry)?;
                    fs::write(&path, bytes)
                        .map_err(|error| format!("write {}: {error}", entry.path))?;
                }
                "symlink" => {
                    let target = entry
                        .target
                        .as_deref()
                        .ok_or_else(|| format!("symlink {} is missing target", entry.path))?;
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)
                            .map_err(|error| format!("create parent {}: {error}", entry.path))?;
                    }
                    create_symlink(target, &path)?;
                }
                other => return Err(format!("row {} has unknown fixture kind {other}", self.id)),
            }
        }
        Ok(())
    }

    pub fn initial_manifest(&self, root: &Path) -> Result<FilesystemManifest, String> {
        self.materialize(root)?;
        deterministic_filesystem_manifest(root).map_err(|error| error.to_string())
    }
}

fn create_symlink(target: &str, path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, path).map_err(|error| error.to_string())
    }
    #[cfg(windows)]
    {
        // The schema and validation run on Windows, but Windows corpus rows
        // are not executed. A directory target gets the directory API; all
        // other targets use the file API.
        if target.ends_with('/') || target.ends_with('\\') {
            std::os::windows::fs::symlink_dir(target, path).map_err(|error| error.to_string())
        } else {
            std::os::windows::fs::symlink_file(target, path).map_err(|error| error.to_string())
        }
    }
}

fn safe_fixture_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!("fixture path is not root-relative: {relative:?}"));
    }
    Ok(root.join(path))
}

fn entry_bytes(entry: &ManifestEntrySpec) -> Result<Vec<u8>, String> {
    if entry.content_base64.is_some() && !entry.content.is_empty() {
        return Err(format!(
            "fixture {} specifies both content and content_base64",
            entry.path
        ));
    }
    match entry.content_base64.as_deref() {
        Some(encoded) => {
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
                .map_err(|error| format!("fixture {} has invalid base64: {error}", entry.path))
        }
        None => Ok(entry.content.as_bytes().to_vec()),
    }
}

fn validate_fixture_entries(entries: &[ManifestEntrySpec]) -> Result<(), String> {
    let mut paths = BTreeSet::new();
    for entry in entries {
        safe_fixture_path(Path::new("."), &entry.path)?;
        if !paths.insert(entry.path.clone()) {
            return Err(format!("duplicate fixture path {}", entry.path));
        }
        match entry.kind.as_str() {
            "directory" => {
                if !entry.content.is_empty()
                    || entry.content_base64.is_some()
                    || entry.target.is_some()
                {
                    return Err(format!(
                        "directory {} has file or symlink payload",
                        entry.path
                    ));
                }
            }
            "file" => {
                if entry.target.is_some() {
                    return Err(format!("file {} has symlink target", entry.path));
                }
                let _ = entry_bytes(entry)?;
            }
            "symlink" => {
                if entry.target.is_none()
                    || !entry.content.is_empty()
                    || entry.content_base64.is_some()
                {
                    return Err(format!("symlink {} has invalid payload", entry.path));
                }
            }
            other => return Err(format!("unknown fixture kind {other}")),
        }
    }
    Ok(())
}

pub fn parse_corpus_str<T: DeserializeOwned>(source: &str) -> Result<T, String> {
    toml::from_str(source).map_err(|error| error.to_string())
}

pub fn parse_corpus(source: &str) -> Result<Corpus, String> {
    let corpus: Corpus = parse_corpus_str(source)?;
    validate_corpus(&corpus)?;
    Ok(corpus)
}

pub fn validate_corpus(corpus: &Corpus) -> Result<(), String> {
    catalog::validate_catalog()?;
    if corpus.schema_version != CORPUS_SCHEMA_VERSION {
        return Err(format!(
            "unsupported corpus schema version {}; expected {}",
            corpus.schema_version, CORPUS_SCHEMA_VERSION
        ));
    }
    if corpus.corpus_id.trim().is_empty() {
        return Err("corpus_id must not be empty".to_string());
    }
    if corpus.rows.is_empty() {
        return Err("corpus must contain at least one row".to_string());
    }

    let mut row_ids = BTreeSet::new();
    let mut covered_branches = BTreeSet::new();
    let mut covered_dimensions = BTreeSet::new();
    for row in &corpus.rows {
        if row.id.trim().is_empty() || !row_ids.insert(row.id.clone()) {
            return Err(format!("duplicate or empty row ID {:?}", row.id));
        }
        if row.command.trim().is_empty() {
            return Err(format!("row {} has an empty command", row.id));
        }
        let route = row.parsed_route()?;
        if row.mode != "characterization-only" {
            return Err(format!("row {} must be characterization-only", row.id));
        }
        if row.semantic_dimensions.is_empty() {
            return Err(format!("row {} has no semantic dimensions", row.id));
        }
        for dimension in &row.semantic_dimensions {
            if !catalog::semantic_dimension_exists(dimension) {
                return Err(format!(
                    "row {} names unknown dimension {dimension}",
                    row.id
                ));
            }
            covered_dimensions.insert(dimension.clone());
        }
        if !catalog::COMPARISON_BASES.contains(&row.basis.as_str()) {
            return Err(format!(
                "row {} names unknown comparison basis {}",
                row.id, row.basis
            ));
        }
        if row.normalizations.iter().any(|normalization| {
            !catalog::PRESENTATION_NORMALIZATIONS.contains(&normalization.as_str())
        }) {
            return Err(format!(
                "row {} has unknown presentation normalization",
                row.id
            ));
        }
        if row
            .expectations
            .iter()
            .any(|expectation| !catalog::EXPECTATIONS.contains(&expectation.as_str()))
        {
            return Err(format!("row {} has unknown expectation", row.id));
        }
        if row
            .expectations
            .iter()
            .any(|expectation| expectation == &row.basis)
            || row
                .normalizations
                .iter()
                .any(|normalization| normalization == &row.basis)
        {
            return Err(format!(
                "row {} places a basis ID in a disjoint vocabulary",
                row.id
            ));
        }
        for branch_id in &row.branch_ids {
            if !catalog::branch_exists(branch_id) {
                return Err(format!("row {} names unknown branch {branch_id}", row.id));
            }
            covered_branches.insert(branch_id.clone());
        }
        match route {
            ParsedRoute {
                native: true,
                rule_id: None,
            } => {
                if row.decision_class.is_some() {
                    return Err(format!(
                        "native row {} must not have a decision class",
                        row.id
                    ));
                }
            }
            ParsedRoute {
                native: false,
                rule_id: Some(ref rule_id),
            } => {
                let class_id = row
                    .decision_class
                    .as_deref()
                    .ok_or_else(|| format!("rewritten row {} is missing decision_class", row.id))?;
                let class = catalog::decision_class(class_id).ok_or_else(|| {
                    format!("row {} names unknown decision class {class_id}", row.id)
                })?;
                if class.rule_id != rule_id {
                    return Err(format!(
                        "row {} decision class does not match route",
                        row.id
                    ));
                }
            }
            _ => unreachable!(),
        }
        validate_fixture_entries(&row.manifest)?;
        if row.mutating && !row.semantic_dimensions.iter().any(|dim| dim == "mutation") {
            return Err(format!(
                "mutating row {} must declare mutation dimension",
                row.id
            ));
        }
        if row.workdir.starts_with('/') || row.workdir.contains("..") {
            return Err(format!("row {} workdir must be root-relative", row.id));
        }
    }

    for branch in catalog::BRANCH_INVENTORY {
        if !covered_branches.contains(branch.id) {
            return Err(format!(
                "branch inventory entry {} has no corpus row",
                branch.id
            ));
        }
    }
    for dimension in catalog::SEMANTIC_DIMENSIONS {
        if !covered_dimensions.contains(*dimension) {
            return Err(format!("semantic dimension {dimension} has no corpus row"));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessFailure {
    pub row_id: String,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct AggregateFailures {
    failures: Vec<HarnessFailure>,
}

impl AggregateFailures {
    pub fn push(&mut self, row_id: impl Into<String>, message: impl Into<String>) {
        self.failures.push(HarnessFailure {
            row_id: row_id.into(),
            message: message.into(),
        });
    }

    pub fn is_empty(&self) -> bool {
        self.failures.is_empty()
    }

    pub fn failures(&self) -> &[HarnessFailure] {
        &self.failures
    }

    pub fn finish(self) -> Result<(), String> {
        if self.failures.is_empty() {
            return Ok(());
        }
        let mut report = String::from("bash rewrite differential failures:\n");
        for failure in self.failures {
            let _ = writeln!(report, "- {}: {}", failure.row_id, failure.message);
        }
        Err(report)
    }
}

pub fn manifest_for(root: &Path) -> Result<FilesystemManifest, String> {
    deterministic_filesystem_manifest(root).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
        schema_version = 1
        corpus_id = "test"

        [[rows]]
        id = "native"
        command = "printf ok"
        route = "native"
        basis = "bytes"
        branch_ids = ["dispatch.native.no_rule"]
        semantic_dimensions = ["shell-tokenization"]
        mode = "characterization-only"
    "#;

    #[test]
    fn schema_rejects_unknown_fields_and_bad_version() {
        let unknown = VALID.replace("mode =", "unknown = true\nmode =");
        assert!(parse_corpus(&unknown).is_err());
        let bad_version = VALID.replace("schema_version = 1", "schema_version = 2");
        assert!(parse_corpus(&bad_version).is_err());
    }

    #[test]
    fn aggregate_failures_are_not_first_failure_only() {
        let mut failures = AggregateFailures::default();
        failures.push("r1", "first");
        failures.push("r2", "second");
        let report = failures.finish().unwrap_err();
        assert!(report.contains("r1") && report.contains("r2"));
    }
}
