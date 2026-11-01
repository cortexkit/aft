//! Transport preservation for hashline responses across host paths.
//!
//! NDJSON, subc, MCP, OpenCode, and Pi all share one renderer. Hashline-specific
//! structured fields supplement the shipped mutation display envelope; hosts that
//! strip those fields still render truth from `output` / `metadata` / completion
//! flags. All-failed Phase-2 envelopes keep `success:false` with the complete
//! per-file payload through every shipped host path.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::hashline::apply::{FileClassification, MutationState};
use crate::hashline::syntax::{HashlineRejection, HashlineRejectionCode, RejectionStage};
use crate::hashline::transaction::{FileOutcome, FileRole, TransactionEnvelope};

/// Host transport that must preserve the shared carrier contract.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TransportKind {
    Ndjson,
    Subc,
    Mcp,
    OpenCode,
    Pi,
}

impl TransportKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ndjson => "ndjson",
            Self::Subc => "subc",
            Self::Mcp => "mcp",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
        }
    }

    pub const ALL: &[TransportKind] = &[
        Self::Ndjson,
        Self::Subc,
        Self::Mcp,
        Self::OpenCode,
        Self::Pi,
    ];
}

/// Optional before/after bytes used to build the shipped display envelope.
#[derive(Clone, Debug, Default)]
pub struct DisplayFileBytes {
    pub requested_path: String,
    pub before: Vec<u8>,
    pub after: Option<Vec<u8>>,
    pub remove_file: bool,
    pub move_from: Option<String>,
}

/// Inputs required to render a mutation (or preview) response for hosts.
#[derive(Clone, Debug)]
pub struct MutationRenderInput<'a> {
    pub envelope: &'a TransactionEnvelope,
    /// Patch-order file bytes for diff metadata (primary / destination rows).
    pub display_files: &'a [DisplayFileBytes],
    pub project_root: Option<&'a Path>,
    pub transport: TransportKind,
}

/// Hashline fields that supplement the shipped envelope.
const HASHLINE_STRUCTURED_KEYS: &[&str] = &[
    "hashline",
    "classifications",
    "mutation_states",
    "final_tags",
    "registers_committed",
    "stop_reason",
    "remap_recovery",
    "stage",
    "steering",
];

/// Render a Phase-2 (or preview) envelope for one transport.
pub fn render_mutation_response(input: MutationRenderInput<'_>) -> Value {
    let envelope = input.envelope;
    let (diff, files_meta) = build_display_metadata(input.display_files, input.project_root);
    let diff_omitted = files_meta.iter().any(|file| file["diff_omitted"] == true);
    let file_path = first_display_path(input.display_files, envelope);
    let output = render_agent_output(envelope);
    let title = render_title(envelope);
    let complete = envelope.complete;
    let partial = envelope.success && !envelope.complete;
    let all_failed = !envelope.success;

    let mut payload = json!({
        "output": output,
        "title": title,
        "success": envelope.success,
        "complete": complete,
        "partial": partial,
        "all_failed": all_failed,
        "preview": envelope.preview,
        "metadata": {
            "files": files_meta,
        },
    });
    if let Some(diff) = diff {
        payload["metadata"]["diff"] = json!(diff);
    }
    if diff_omitted {
        crate::mutation_diff::mark_omitted(&mut payload);
    }

    if let Some(path) = file_path {
        payload["filePath"] = json!(path);
    }

    if let Some(op_id) = &envelope.op_id {
        payload["op_id"] = json!(op_id);
    }

    // Hashline supplements — hosts may strip these (A14) and still render.
    payload["hashline"] = json!(true);
    payload["registers_committed"] = json!(envelope.registers_committed);
    if let Some(stop) = envelope.stop_reason {
        payload["stop_reason"] = json!(stop);
    }
    payload["classifications"] = Value::Array(
        envelope
            .files
            .iter()
            .map(|f| {
                json!({
                    "requested_path": f.requested_path,
                    "role": f.role.as_str(),
                    "classification": f.classification.as_str(),
                    "mutation_state": f.mutation_state.as_str(),
                    "final_tag": f.final_tag,
                    "backup_id": f.backup_id,
                    "format_skipped_reason": f.format_skipped_reason,
                    "tag_notice": f.tag_notice,
                    "remove_file": f.remove_file,
                })
            })
            .collect(),
    );
    payload["final_tags"] = Value::Array(
        envelope
            .files
            .iter()
            .filter_map(|f| {
                f.final_tag.as_ref().map(|tag| {
                    json!({
                        "requested_path": f.requested_path,
                        "tag": tag,
                    })
                })
            })
            .collect(),
    );

    adapt_for_transport(payload, input.transport)
}

/// Render a Phase-1 rejection for one transport.
pub fn render_rejection_response(rejection: &HashlineRejection, transport: TransportKind) -> Value {
    let payload = json!({
        "success": false,
        "complete": false,
        "partial": false,
        "all_failed": false,
        "error": rejection.code.as_str(),
        "code": rejection.code.as_str(),
        "stage": rejection.stage.as_str(),
        "message": rejection.message,
        "steering": rejection.steering,
        "output": format!(
            "{} at {}: {}\n{}",
            rejection.code.as_str(),
            rejection.stage.as_str(),
            rejection.message,
            rejection.steering
        ),
        "metadata": {
            "diff": "",
            "files": [],
        },
    });
    adapt_for_transport(payload, transport)
}

/// Strip hashline-specific structured fields, leaving the shipped display envelope.
///
/// A14 locks that hosts consuming only this envelope still render single-file,
/// multi-file, MV, and all-failed results correctly.
pub fn strip_hashline_fields(payload: &Value) -> Value {
    let mut stripped = payload.clone();
    if let Some(obj) = stripped.as_object_mut() {
        for key in HASHLINE_STRUCTURED_KEYS {
            obj.remove(*key);
        }
        // classifications / final_tags already covered; also drop nested hashline-only
        // keys that may appear under metadata in future extensions.
        if let Some(Value::Object(meta)) = obj.get_mut("metadata") {
            meta.remove("hashline");
            meta.remove("classifications");
        }
    }
    stripped
}

/// True when the stripped envelope still carries the fields hosts need to render.
pub fn shipped_envelope_is_renderable(stripped: &Value) -> bool {
    let obj = match stripped.as_object() {
        Some(obj) => obj,
        None => return false,
    };
    obj.contains_key("output")
        && obj.contains_key("complete")
        && obj.contains_key("partial")
        && obj.contains_key("all_failed")
        && obj
            .get("metadata")
            .and_then(|m| m.get("files"))
            .map(|f| f.is_array())
            .unwrap_or(false)
        && obj
            .get("metadata")
            .and_then(|m| m.get("diff"))
            .map(|d| d.is_string())
            .unwrap_or(false)
}

/// Agent-visible text: summary lead-in plus per-file tag carriers / notices.
pub fn render_agent_output(envelope: &TransactionEnvelope) -> String {
    let mut lines = Vec::new();
    lines.push(envelope.summary_text.clone());
    if envelope.preview {
        lines.push("preview: no files were modified".to_string());
    }
    for file in &envelope.files {
        lines.push(render_file_text(file));
    }
    if let Some(stop) = envelope.stop_reason {
        lines.push(format!("stop_reason: {stop}"));
    }
    if let Some(op_id) = &envelope.op_id {
        lines.push(format!("op_id: {op_id}"));
    }
    lines.join("\n")
}

fn render_file_text(file: &FileOutcome) -> String {
    let role = file.role.as_str();
    let class = file.classification.as_str();
    let mut line = format!(
        "{role} {path}: {class} ({state})",
        path = file.requested_path,
        state = file.mutation_state.as_str()
    );
    if let Some(tag) = &file.final_tag {
        line.push_str(&format!("\n[{}#{}]", file.requested_path, tag));
        if !file.affected.ranges.is_empty() {
            // Affected gutters are carried when final bytes are available; the
            // structured classifications already expose the region. Text keeps
            // the tag header so chaining works on field-stripping hosts.
        }
    } else if let Some(notice) = &file.tag_notice {
        line.push_str(&format!("\n{notice}"));
    } else if file.remove_file
        && file.classification.is_applied_star()
        && file.role == FileRole::MvSource
    {
        line.push_str(&format!(
            "\nsource removed: {} (no final tag)",
            file.requested_path
        ));
    } else if file.classification == FileClassification::AppliedTagUnavailable {
        line.push_str("\ntag unavailable: re-read before chaining");
    }
    if let Some(reason) = &file.format_skipped_reason {
        line.push_str(&format!("\nformat_skipped: {reason}"));
    }
    for warning in &file.warnings {
        line.push_str(&format!("\nwarning: {warning}"));
    }
    line
}

fn render_title(envelope: &TransactionEnvelope) -> String {
    if envelope.preview {
        return "Hashline preview".to_string();
    }
    if envelope.complete {
        "Hashline edit applied".to_string()
    } else if envelope.success {
        "Hashline edit partially applied".to_string()
    } else {
        "Hashline edit failed".to_string()
    }
}

fn first_display_path(
    display_files: &[DisplayFileBytes],
    envelope: &TransactionEnvelope,
) -> Option<String> {
    display_files
        .iter()
        .map(|f| f.requested_path.clone())
        .next()
        .or_else(|| {
            envelope
                .files
                .iter()
                .find(|f| f.role != FileRole::MvSource)
                .map(|f| f.requested_path.clone())
        })
}

fn build_display_metadata(
    display_files: &[DisplayFileBytes],
    project_root: Option<&Path>,
) -> (Option<String>, Vec<Value>) {
    let mut files = Vec::with_capacity(display_files.len());
    let mut budget = crate::mutation_diff::DiffBudget::default();
    for file in display_files {
        let before = String::from_utf8_lossy(&file.before);
        let after = if file.remove_file {
            String::new()
        } else {
            file.after
                .as_ref()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .unwrap_or_default()
        };
        let entry_type = if file.move_from.is_some() {
            "move"
        } else if file.remove_file
            || (file.after.as_ref().is_some_and(|a| a.is_empty()) && !file.before.is_empty())
        {
            "delete"
        } else if file.before.is_empty() && file.after.as_ref().is_some_and(|a| !a.is_empty()) {
            "add"
        } else {
            "update"
        };
        let patch = budget.patch(&file.requested_path, &before, &after, 2);
        let (additions, deletions) = line_diff_counts(&before, &after);
        let mut entry = json!({
            "filePath": absolutize(project_root, &file.requested_path),
            "relativePath": relativize(project_root, &file.requested_path),
            "type": entry_type,
            "additions": additions,
            "deletions": deletions,
        });
        if let Some(patch) = patch {
            entry["patch"] = json!(patch);
        } else {
            entry["diff_omitted"] = json!(true);
        }
        if let Some(src) = &file.move_from {
            entry["movePath"] = json!(src);
            // Spec: MV displays the destination diff and names the source.
            entry["sourcePath"] = json!(src);
        }
        files.push(entry);
    }
    let diff = files
        .iter()
        .filter_map(|file| file.get("patch").and_then(Value::as_str))
        .filter(|patch| !patch.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let diff = if diff.is_empty() && files.iter().any(|file| file["diff_omitted"] == true) {
        None
    } else {
        Some(diff)
    };
    (diff, files)
}

fn line_diff_counts(before: &str, after: &str) -> (usize, usize) {
    use similar::ChangeTag;
    let diff = similar::TextDiff::from_lines(before, after);
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {}
        }
    }
    (additions, deletions)
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

/// Transport-specific adaptation while preserving the shared carrier contract.
fn adapt_for_transport(mut payload: Value, transport: TransportKind) -> Value {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("transport".to_string(), json!(transport.as_str()));
        match transport {
            TransportKind::Mcp => {
                // MCP tool results expose text content; keep output as the primary
                // agent-visible carrier and mirror it under content for adapters.
                let output = obj
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                obj.insert(
                    "content".to_string(),
                    json!([{ "type": "text", "text": output }]),
                );
            }
            TransportKind::OpenCode | TransportKind::Pi => {
                // Hoisted adapters read filePath + metadata.diff / metadata.files[].
                // Ensure metadata always exists even on pure rejections.
                obj.entry("metadata".to_string())
                    .or_insert_with(|| json!({ "diff": "", "files": [] }));
            }
            TransportKind::Ndjson | TransportKind::Subc => {
                // Wire JSON as-is; summary_text already leads output.
            }
        }
    }
    payload
}

/// Build display-file rows from a transaction envelope when callers already hold
/// baseline/final bytes on each [`FileOutcome`].
pub fn display_files_from_envelope(
    envelope: &TransactionEnvelope,
    baselines: &[(String, Vec<u8>)],
) -> Vec<DisplayFileBytes> {
    let baseline_map: std::collections::BTreeMap<&str, &[u8]> = baselines
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect();

    let mut out = Vec::new();
    let mut i = 0usize;
    while i < envelope.files.len() {
        let file = &envelope.files[i];
        match file.role {
            FileRole::MvDestination => {
                let source = envelope
                    .files
                    .get(i + 1)
                    .filter(|f| f.role == FileRole::MvSource);
                let before = baseline_map
                    .get(file.requested_path.as_str())
                    .map(|b| b.to_vec())
                    .unwrap_or_default();
                out.push(DisplayFileBytes {
                    requested_path: file.requested_path.clone(),
                    before,
                    after: file.final_bytes.clone(),
                    remove_file: false,
                    move_from: source.map(|s| s.requested_path.clone()),
                });
                if source.is_some() {
                    i += 2;
                    continue;
                }
            }
            FileRole::MvSource => {
                // Source-only row without a preceding destination is unusual;
                // still emit a delete-style entry so hosts see the path.
                let before = baseline_map
                    .get(file.requested_path.as_str())
                    .map(|b| b.to_vec())
                    .unwrap_or_default();
                out.push(DisplayFileBytes {
                    requested_path: file.requested_path.clone(),
                    before,
                    after: None,
                    remove_file: file.remove_file,
                    move_from: None,
                });
            }
            FileRole::Primary => {
                let before = baseline_map
                    .get(file.requested_path.as_str())
                    .map(|b| b.to_vec())
                    .unwrap_or_default();
                out.push(DisplayFileBytes {
                    requested_path: file.requested_path.clone(),
                    before,
                    after: file.final_bytes.clone(),
                    remove_file: file.remove_file,
                    move_from: None,
                });
            }
        }
        i += 1;
    }
    out
}

/// Assert all-failed payloads retain the complete mutation envelope after strip.
pub fn all_failed_payload_preserved(payload: &Value) -> bool {
    let obj = match payload.as_object() {
        Some(obj) => obj,
        None => return false,
    };
    if obj.get("success") != Some(&Value::Bool(false)) {
        return false;
    }
    if obj.get("all_failed") != Some(&Value::Bool(true)) {
        return false;
    }
    let output = obj.get("output").and_then(Value::as_str).unwrap_or("");
    // Text always leads with applied/failed counts so field-stripping hosts
    // still render the truth ("0 of N files applied").
    if !output.contains("files applied") {
        return false;
    }
    let stripped = strip_hashline_fields(payload);
    shipped_envelope_is_renderable(&stripped)
        && stripped
            .get("output")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("files applied"))
}

/// Collect the set of transports that must agree on carrier keys for one payload.
pub fn carrier_keys(payload: &Value) -> BTreeSet<String> {
    const REQUIRED: &[&str] = &[
        "output",
        "success",
        "complete",
        "partial",
        "all_failed",
        "metadata",
    ];
    let mut keys = BTreeSet::new();
    if let Some(obj) = payload.as_object() {
        for key in REQUIRED {
            if obj.contains_key(*key) {
                keys.insert((*key).to_string());
            }
        }
    }
    keys
}

/// Merge transport payloads and verify they share the same carrier contract.
pub fn transports_preserve_carriers(payloads: &[(TransportKind, Value)]) -> bool {
    if payloads.is_empty() {
        return false;
    }
    let expected = carrier_keys(&payloads[0].1);
    if expected.len() < 6 {
        return false;
    }
    payloads
        .iter()
        .all(|(_, payload)| carrier_keys(payload) == expected)
}

/// Build a minimal synthetic envelope for transport tests without disk I/O.
pub fn synthetic_envelope(
    success: bool,
    complete: bool,
    files: Vec<FileOutcome>,
    op_id: Option<String>,
    preview: bool,
) -> TransactionEnvelope {
    let applied = files
        .iter()
        .filter(|f| f.role != FileRole::MvSource && f.classification.is_applied_star())
        .count();
    let total = files
        .iter()
        .filter(|f| !(f.role == FileRole::MvSource && f.classification.is_applied_star()))
        .count();
    TransactionEnvelope {
        success,
        complete,
        files,
        op_id,
        stop_reason: if success {
            None
        } else {
            Some("hashline_baseline_drift")
        },
        registers_committed: false,
        preview,
        summary_text: format!("{applied} of {total} files applied"),
    }
}

/// Convenience constructor for a failed primary file row.
pub fn synthetic_failed_file(path: &str, classification: FileClassification) -> FileOutcome {
    FileOutcome {
        canonical_path: PathBuf::from(path),
        requested_path: path.to_string(),
        role: FileRole::Primary,
        classification,
        mutation_state: classification.mutation_state(),
        final_bytes: None,
        final_tag: None,
        affected: crate::hashline::snapshot::AffectedRegion::default(),
        warnings: Vec::new(),
        format_skipped_reason: None,
        backup_id: None,
        remove_file: false,
        tag_notice: None,
    }
}

/// Convenience constructor for an applied primary file row with tag + bytes.
pub fn synthetic_applied_file(path: &str, _before: &[u8], after: &[u8], tag: &str) -> FileOutcome {
    FileOutcome {
        canonical_path: PathBuf::from(path),
        requested_path: path.to_string(),
        role: FileRole::Primary,
        classification: FileClassification::Applied,
        mutation_state: MutationState::Applied,
        final_bytes: Some(after.to_vec()),
        final_tag: Some(tag.to_string()),
        affected: crate::hashline::snapshot::AffectedRegion::default(),
        warnings: Vec::new(),
        format_skipped_reason: None,
        backup_id: Some("bak-test".to_string()),
        remove_file: false,
        tag_notice: None,
    }
}

/// Flatten a JSON object for stable golden comparison of required keys.
pub fn required_rejection_fields(payload: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(obj) = payload.as_object() {
        for key in ["code", "stage", "message", "steering", "output", "success"] {
            if let Some(value) = obj.get(key) {
                out.insert(key.to_string(), value.clone());
            }
        }
    }
    out
}

/// Transport status for a Phase-1 rejection (always non-mutating error).
pub fn rejection_transport_status(_code: HashlineRejectionCode) -> &'static str {
    "error"
}

/// Registry row locking transport status, stage, and steering for one code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectionTransportContract {
    pub code: HashlineRejectionCode,
    pub stage: RejectionStage,
    pub transport_status: &'static str,
    pub steering: &'static str,
    pub mutates_files: bool,
    pub mutates_stores: bool,
}

/// Full rejection transport registry (A18 transport + steering portions).
pub fn rejection_transport_registry() -> Vec<RejectionTransportContract> {
    use HashlineRejectionCode::*;
    use RejectionStage::*;

    let row = |code: HashlineRejectionCode, stage: RejectionStage, steering: &'static str| {
        RejectionTransportContract {
            code,
            stage,
            transport_status: rejection_transport_status(code),
            steering,
            mutates_files: false,
            mutates_stores: false,
        }
    };

    vec![
        row(
            ParseError,
            Parse,
            "submit only a hashline patch with tagged section headers and valid operations",
        ),
        row(
            MissingTag,
            Header,
            "run `read <path>` on the file, then include the four-hex tag it returns",
        ),
        row(
            MalformedTag,
            Header,
            "run `read <path>` on the file, then include the four-hex tag it returns",
        ),
        row(
            UnknownTag,
            Resolution,
            "run `read <path>` again to mint a fresh tag, then retry the edit",
        ),
        row(
            EvictedTag,
            Resolution,
            "run `read <path>` again to mint a fresh tag, then retry the edit",
        ),
        row(
            AmbiguousTag,
            Resolution,
            "use apply_patch or another available non-hashline edit surface; re-reading preserves this colliding four-hex tag",
        ),
        row(
            AmbiguousTag,
            Recovery,
            "re-address the current tagged content; the stale span has multiple verbatim landings",
        ),
        row(
            StaleTag,
            Verification,
            "run `read <path>` over the addressed range because required boundary context changed",
        ),
        row(
            StaleTag,
            Recovery,
            "re-address the current tagged content; the stale span no longer occurs verbatim",
        ),
        row(
            UnseenLine,
            Eligibility,
            "re-read the file to mint a fresh tag that includes every addressed row and boundary, then retry the edit",
        ),
        row(
            BoundaryIneligible,
            Eligibility,
            "re-read the file to mint a fresh tag that includes every addressed row and boundary, then retry the edit",
        ),
        row(
            UntaggablePath,
            Path,
            "choose a writable regular text file or use an available non-hashline surface",
        ),
        row(
            RegisterOverflow,
            Register,
            "reduce register contents before retrying the patch",
        ),
        row(
            BackupUnavailable,
            Baseline,
            "enable backups or use apply_patch for this destructive change",
        ),
    ]
}

/// Build a rejection whose steering matches the registry for (code, stage).
pub fn rejection_for_contract(contract: &RejectionTransportContract) -> HashlineRejection {
    HashlineRejection::new(contract.code, contract.stage, contract.code.as_str())
}
