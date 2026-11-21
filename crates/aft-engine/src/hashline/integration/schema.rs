//! Edit schema selection and governed schema/manifest artifacts.
//!
//! One binary ships both arms. The installed session effective value selects
//! exactly one agent-visible `edit` schema. This module is the exclusive owner
//! of the hashline-side governed schema and manifest definitions; host wiring
//! regenerates committed artifacts from these constants.

use serde_json::{json, Value};

use super::binding::effective_for_capture;
use super::binding::BindingGuard;

/// Which edit schema arm a session publishes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EditSchemaArm {
    /// Legacy match/line/symbol edit surface (gate-off / unregistered).
    Legacy,
    /// Hashline patch language with required `patch` field (gate-on).
    Hashline,
}

impl EditSchemaArm {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Hashline => "hashline",
        }
    }

    pub const fn is_hashline(self) -> bool {
        matches!(self, Self::Hashline)
    }
}

/// Select the sole edit schema arm from effective mode.
pub fn select_edit_schema(effective: bool) -> EditSchemaArm {
    if effective {
        EditSchemaArm::Hashline
    } else {
        EditSchemaArm::Legacy
    }
}

/// Select from an optional captured binding (unregistered → legacy).
pub fn select_edit_schema_for_capture(guard: Option<&BindingGuard>) -> EditSchemaArm {
    select_edit_schema(effective_for_capture(guard))
}

/// Agent-visible description for the legacy edit tool.
pub const LEGACY_EDIT_DESCRIPTION: &str = "Edit a file by finding and replacing text, or by targeting named symbols. To write or overwrite a whole file, use the `write` tool — `edit` requires an explicit edit mode and will not silently overwrite a file from `content` alone.";

/// Agent-visible description for the hashline edit tool.
pub const HASHLINE_EDIT_DESCRIPTION: &str = concat!(
    "Apply a hashline patch. Arguments are exactly `{patch}` where `patch` is a non-empty string. ",
    "Server-owned preview control is outside this schema.\n\n",
    "Quick reference:\n",
    "- Header: `[path#TAG]`; TAG is exactly four hexadecimal digits from a current tagged read. ",
    "Read every addressed row and gap boundary; REM and MV require a whole-file tagged read. ",
    "Re-read after an edit before chaining: an edit-response tag can retain only changed context.\n",
    "- Same canonical path: multiple sections compose in patch order against pre-request coordinates.\n",
    "- Addresses: `0` (BOF), `N` (one line), `N.=M` (range; `N..=M`/`N..M` also work), ",
    "`<N`/`>N` (gap before/after), `N*`/`<N*`/`>N*` (block), and `$`/`$-K` (EOF-relative). ",
    "A plain `N` PUT replaces; use `<N` or `>N` to insert.\n",
    "- PUT text: `PUT <address>:` followed by one or more `+` body rows (`+` alone is blank). ",
    "A final patch newline is allowed. PUT without `:` copies `@name` (or the anonymous register) and takes no body; names use `@` plus ASCII letters, digits, `_`, or `-`.\n",
    "- CUT: `CUT <address> [@name]`. REM: bare `REM` only, removing the whole file. ",
    "MV: `MV <destination>` (one whitespace-free path, optional matching quotes), once and after any line operations. ",
    "`*** Begin Patch`/`*** End Patch` is an optional envelope.\n",
    "- Only `read` (and accepted AFT `cat`/`head`/`tail` rewrites) mint hashline tags. ",
    "`aft_zoom`, `aft_outline`, `grep`, `aft_search`, and conflict snippets do not. ",
    "After navigation, call `read` on every file and range the patch addresses."
);

/// JSON Schema for the legacy edit arm (gate-off).
pub fn legacy_edit_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "filePath": {
                "description": "Path to the file to edit (absolute or relative to project root)",
                "type": "string"
            },
            "symbol": {
                "description": "Named symbol to replace (function, class, type)",
                "type": "string"
            },
            "content": {
                "description": "Replacement content for symbol mode. For whole-file writes, use the `write` tool.",
                "type": "string"
            },
            "appendContent": {
                "description": "Text to append to the end of path; creates the file if needed",
                "type": "string"
            },
            "edits": {
                "description": "Batch edits — non-empty array of { oldString, newString }, { oldString, newString, replaceAll: true }, or { startLine, endLine, content } objects",
                "minItems": 1,
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "oldString": {
                            "description": "Text to find for a batch find/replace edit",
                            "type": "string"
                        },
                        "newString": {
                            "description": "Replacement text for a batch find/replace edit",
                            "type": "string"
                        },
                        "replaceAll": {
                            "description": "Replace every occurrence for this batch item",
                            "type": "boolean"
                        },
                        "occurrence": {
                            "description": "1-based occurrence for this batch item (1 = first match)",
                            "type": "integer",
                            "minimum": 1
                        },
                        "startLine": {
                            "description": "1-based start line for a batch line-range edit",
                            "type": "integer",
                            "minimum": 1
                        },
                        "endLine": {
                            "description": "1-based end line for a batch line-range edit",
                            "type": "integer",
                            "minimum": 1
                        },
                        "content": {
                            "description": "Replacement text for a batch line-range edit",
                            "type": "string"
                        }
                    }
                }
            }
        },
        "required": ["filePath"],
        "description": LEGACY_EDIT_DESCRIPTION
    })
}

/// JSON Schema for the hashline edit arm (gate-on).
pub fn hashline_edit_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "patch": {
                "type": "string",
                "minLength": 1,
                "description": "Hashline patch text with one or more [path#TAG] sections and PUT/CUT/REM/MV operations"
            }
        },
        "required": ["patch"],
        "description": HASHLINE_EDIT_DESCRIPTION
    })
}

/// Schema JSON for the selected arm.
pub fn edit_schema_for(arm: EditSchemaArm) -> Value {
    match arm {
        EditSchemaArm::Legacy => legacy_edit_schema(),
        EditSchemaArm::Hashline => hashline_edit_schema(),
    }
}

/// Description string for the selected arm.
pub fn edit_description_for(arm: EditSchemaArm) -> &'static str {
    match arm {
        EditSchemaArm::Legacy => LEGACY_EDIT_DESCRIPTION,
        EditSchemaArm::Hashline => HASHLINE_EDIT_DESCRIPTION,
    }
}

/// Native command name translation routes to when gate-on.
pub const HASHLINE_EDIT_COMMAND: &str = "hashline_edit";

/// Native command name for syntactic preflight (Phase-1 parse only).
pub const HASHLINE_PREFLIGHT_COMMAND: &str = "hashline_preflight";

/// Governed tool-manifest entry for one edit arm.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernedEditManifestEntry {
    pub name: &'static str,
    pub arm: EditSchemaArm,
    pub description: &'static str,
    pub schema: Value,
    pub supports_tool: bool,
    pub hoisted: bool,
    pub lane: &'static str,
}

/// Build the governed manifest entry hosts lock against for registration parity.
pub fn governed_edit_manifest_entry(arm: EditSchemaArm) -> GovernedEditManifestEntry {
    GovernedEditManifestEntry {
        name: "edit",
        arm,
        description: edit_description_for(arm),
        schema: edit_schema_for(arm),
        supports_tool: true,
        hoisted: true,
        lane: "mutation",
    }
}

/// Serialize both arms into the governed dual-mode artifact document.
///
/// Regeneration of committed `subc_tool_schemas.json` / plugin manifests must
/// source the hashline arm exclusively from this document. The legacy arm is
/// included so a single binary can publish either without rebuilding.
pub fn regenerate_governed_edit_artifacts() -> Value {
    let legacy = governed_edit_manifest_entry(EditSchemaArm::Legacy);
    let hashline = governed_edit_manifest_entry(EditSchemaArm::Hashline);
    json!({
        "tool": "edit",
        "dual_mode": true,
        "selection": "session_effective_hashline",
        "arms": {
            "legacy": {
                "arm": legacy.arm.as_str(),
                "description": legacy.description,
                "schema": legacy.schema,
                "supports_tool": legacy.supports_tool,
                "hoisted": legacy.hoisted,
                "lane": legacy.lane,
                "command": "edit",
            },
            "hashline": {
                "arm": hashline.arm.as_str(),
                "description": hashline.description,
                "schema": hashline.schema,
                "supports_tool": hashline.supports_tool,
                "hoisted": hashline.hoisted,
                "lane": hashline.lane,
                "command": HASHLINE_EDIT_COMMAND,
                "preflight_command": HASHLINE_PREFLIGHT_COMMAND,
            }
        },
        "invariant": "a session never exposes both edit schemas",
    })
}

/// Gate-on translation: accept only `{patch}` and route to `hashline_edit`.
///
/// Must run before shared path-argument normalization and every legacy edit-shape
/// check. Legacy keys are never ignored or routed to a legacy handler.
pub fn translate_gate_on_edit(
    arguments: &Value,
) -> Result<GateOnTranslation, crate::hashline::syntax::HashlineRejection> {
    let request = crate::hashline::syntax::validate_raw_arguments(arguments)?;
    Ok(GateOnTranslation {
        command: HASHLINE_EDIT_COMMAND,
        patch: request.patch,
    })
}

/// Successful gate-on translation product.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GateOnTranslation {
    pub command: &'static str,
    pub patch: String,
}

impl GateOnTranslation {
    pub fn to_native_args(&self) -> Value {
        json!({ "patch": self.patch })
    }
}

/// Dispatch edit translation using the captured binding's effective mode.
///
/// - Effective on → hashline arm only (`hashline_edit`).
/// - Effective off / unregistered → caller keeps the legacy translation path;
///   this returns `None` so existing gate-off goldens stay byte-identical.
pub fn translate_edit_for_session(
    guard: Option<&BindingGuard>,
    arguments: &Value,
) -> Result<Option<GateOnTranslation>, crate::hashline::syntax::HashlineRejection> {
    if !effective_for_capture(guard) {
        return Ok(None);
    }
    Ok(Some(translate_gate_on_edit(arguments)?))
}
