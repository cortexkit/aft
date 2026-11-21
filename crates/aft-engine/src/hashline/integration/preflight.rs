//! Syntactic preflight and permission orchestration.
//!
//! Hosts never parse the hashline grammar. They call `hashline_preflight`
//! (Phase-1 parse only, zero mutation) to obtain affected paths and a per-file
//! operation summary, run permission / external-dir checks on that result, then
//! preview, then apply — mirroring the shipped `apply_patch` affected_paths flow.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::hashline::syntax::{
    parse_hashline_patch, validate_raw_arguments, HashlineRejection, Operation, Patch,
};

/// One operation row inside a preflight file summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreflightOperation {
    pub kind: &'static str,
    /// Destination path spelling for MV; otherwise unused.
    pub destination: Option<String>,
}

/// Per-file summary returned by syntactic preflight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreflightFileSummary {
    pub requested_path: String,
    pub tag: String,
    pub operations: Vec<PreflightOperation>,
}

/// Mutation-free preflight product for host permission orchestration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreflightResult {
    pub files: Vec<PreflightFileSummary>,
    /// Absolute (or host-canonicalized) paths in patch order, de-duplicated.
    pub affected_paths: Vec<String>,
    /// Project-relative spellings parallel to `affected_paths` when a root is known.
    pub affected_rel_paths: Vec<String>,
    /// MV destinations included so permission checks cover both ends of a move.
    pub mv_destinations: Vec<String>,
}

impl PreflightResult {
    /// Patterns suitable for host `askEditPermission` / external-dir checks.
    pub fn permission_patterns(&self) -> Vec<String> {
        let mut patterns = BTreeSet::new();
        for path in self
            .affected_paths
            .iter()
            .chain(self.mv_destinations.iter())
        {
            patterns.insert(path.clone());
        }
        for path in &self.affected_rel_paths {
            patterns.insert(path.clone());
        }
        patterns.into_iter().collect()
    }

    /// JSON shape hosts consume (mirrors apply_patch preview path fields).
    pub fn to_json(&self) -> Value {
        json!({
            "preview": false,
            "preflight": true,
            "affected_paths": self.affected_paths,
            "affected_rel_paths": self.affected_rel_paths,
            "mv_destinations": self.mv_destinations,
            "files": self.files.iter().map(|file| {
                json!({
                    "requested_path": file.requested_path,
                    "tag": file.tag,
                    "operations": file.operations.iter().map(|op| {
                        let mut row = json!({ "kind": op.kind });
                        if let Some(dest) = &op.destination {
                            row["destination"] = json!(dest);
                        }
                        row
                    }).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>(),
        })
    }
}

/// Run syntactic preflight from raw tool arguments (`{patch}` only).
pub fn hashline_preflight_from_args(
    arguments: &Value,
    project_root: Option<&Path>,
) -> Result<PreflightResult, HashlineRejection> {
    let request = validate_raw_arguments(arguments)?;
    hashline_preflight(&request.patch, project_root)
}

/// Phase-1 parse only: no snapshot lookup, no baseline load, no mutation.
pub fn hashline_preflight(
    patch_text: &str,
    project_root: Option<&Path>,
) -> Result<PreflightResult, HashlineRejection> {
    let patch = parse_hashline_patch(patch_text)?;
    summarize_patch(&patch, project_root)
}

fn summarize_patch(
    patch: &Patch,
    project_root: Option<&Path>,
) -> Result<PreflightResult, HashlineRejection> {
    if patch.is_empty() {
        return Err(HashlineRejection::parse(
            "preflight requires at least one patch section",
        ));
    }

    let mut files = Vec::with_capacity(patch.sections.len());
    let mut ordered_paths: Vec<String> = Vec::new();
    let mut seen_paths = BTreeSet::new();
    let mut mv_destinations = Vec::new();
    let mut seen_dests = BTreeSet::new();

    for section in &patch.sections {
        let requested = section.header.requested_path.clone();
        if seen_paths.insert(requested.clone()) {
            ordered_paths.push(requested.clone());
        }

        let mut operations = Vec::with_capacity(section.operations.len());
        for operation in &section.operations {
            let (kind, destination) = match operation {
                Operation::Put(_) => ("PUT", None),
                Operation::Cut(_) => ("CUT", None),
                Operation::Rem(_) => ("REM", None),
                Operation::Mv(mv) => {
                    if seen_dests.insert(mv.destination.clone()) {
                        mv_destinations.push(mv.destination.clone());
                    }
                    ("MV", Some(mv.destination.clone()))
                }
            };
            operations.push(PreflightOperation { kind, destination });
        }

        files.push(PreflightFileSummary {
            requested_path: requested,
            tag: section.header.tag.clone(),
            operations,
        });
    }

    let affected_paths = ordered_paths
        .iter()
        .map(|p| absolutize(project_root, p))
        .collect::<Vec<_>>();
    let affected_rel_paths = ordered_paths
        .iter()
        .map(|p| relativize(project_root, p))
        .collect::<Vec<_>>();
    let mv_destinations = mv_destinations
        .into_iter()
        .map(|p| absolutize(project_root, &p))
        .collect();

    Ok(PreflightResult {
        files,
        affected_paths,
        affected_rel_paths,
        mv_destinations,
    })
}

fn absolutize(project_root: Option<&Path>, requested: &str) -> String {
    let path = PathBuf::from(requested);
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    match project_root {
        Some(root) => root.join(path).to_string_lossy().into_owned(),
        None => requested.to_string(),
    }
}

fn relativize(project_root: Option<&Path>, requested: &str) -> String {
    let path = PathBuf::from(requested);
    if let Some(root) = project_root {
        if path.is_absolute() {
            if let Ok(rel) = path.strip_prefix(root) {
                return rel.to_string_lossy().replace('\\', "/");
            }
        }
    }
    requested.replace('\\', "/")
}

/// Host permission orchestration plan: preflight → permission → preview → apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionPhase {
    Preflight,
    PermissionCheck,
    Preview,
    Apply,
}

impl PermissionPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::PermissionCheck => "permission_check",
            Self::Preview => "preview",
            Self::Apply => "apply",
        }
    }
}

/// Ordered host flow for a hashline edit under permission-gated hosts.
pub const PERMISSION_ORCHESTRATION_ORDER: &[PermissionPhase] = &[
    PermissionPhase::Preflight,
    PermissionPhase::PermissionCheck,
    PermissionPhase::Preview,
    PermissionPhase::Apply,
];

/// Build the permission-metadata object hosts attach when asking the user.
pub fn permission_metadata(preflight: &PreflightResult) -> Value {
    json!({
        "tool": "edit",
        "surface": "hashline",
        "affected_paths": preflight.affected_paths,
        "affected_rel_paths": preflight.affected_rel_paths,
        "mv_destinations": preflight.mv_destinations,
        "file_count": preflight.files.len(),
    })
}
