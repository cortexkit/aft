//! Agent-facing text formatters for subc-mode tool results (parity with TS plugins).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::protocol::Response;
use serde_json::Value;

const MAX_UNCHECKED_FILES_IN_FOOTER: usize = 10;

/// Soft threshold for a whole-file read that was NOT truncated: when the
/// returned text exceeds this many bytes, append a gentle note that range
/// parameters exist, so the agent knows it can narrow a large read without
/// being told it lost content (it did not).
const READ_SOFT_NOTE_BYTES: usize = 20 * 1024;

/// One-line, non-scolding note appended when a whole file read is large but
/// complete. Names both parameter shapes the agent can use to narrow it.
const READ_SOFT_NOTE: &str =
    "\n(File is large; use startLine/endLine or offset/limit to read a section.)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlineMode {
    Text,
    Files,
    DirectoryJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatContext {
    pub agent_specified_range: bool,
    pub outline_mode: OutlineMode,
    pub callgraph_op: Option<String>,
    pub callgraph_include_unresolved: bool,
    pub zoom_target_label: Option<String>,
    pub ast_dry_run: bool,
    pub import_op: Option<String>,
    pub import_remove_name: Option<String>,
    pub import_file_arg: Option<String>,
    pub import_module_arg: Option<String>,
    pub move_file_arg: Option<String>,
    pub move_dest_arg: Option<String>,
    pub safety_op: Option<String>,
    pub safety_file_arg: Option<String>,
    pub safety_name_arg: Option<String>,
    /// `outputMode` of a `bash_status` call; PTY status text depends on it.
    pub bash_output_mode: Option<String>,
    /// The caller is a delegated worker. A worker is never woken by a
    /// completion reminder, so a running task's status never promises one.
    pub worker_session: bool,
    /// Whether the caller's tool catalog includes `bash_watch`.
    pub bash_watch_available: Option<bool>,
    /// The caller's names for AFT's command and patch tools, from the call's
    /// envelope; `None` when it sent none. Carried here because this context
    /// travels with a call through every deferred path, bash included.
    pub host_tool_names: Option<crate::host_tool_names::HostToolNames>,
}

impl Default for FormatContext {
    fn default() -> Self {
        Self {
            agent_specified_range: false,
            outline_mode: OutlineMode::Text,
            callgraph_op: None,
            callgraph_include_unresolved: false,
            zoom_target_label: None,
            ast_dry_run: false,
            import_op: None,
            import_remove_name: None,
            import_file_arg: None,
            import_module_arg: None,
            move_file_arg: None,
            move_dest_arg: None,
            safety_op: None,
            safety_file_arg: None,
            safety_name_arg: None,
            bash_output_mode: None,
            worker_session: false,
            bash_watch_available: None,
            host_tool_names: None,
        }
    }
}

impl FormatContext {
    pub fn from_tool_call(bare_name: &str, arguments: &Value, project_root: &Path) -> Self {
        Self {
            agent_specified_range: agent_specified_read_range(arguments),
            outline_mode: outline_mode_for_call(bare_name, arguments, project_root),
            callgraph_op: callgraph_op_for_call(bare_name, arguments),
            callgraph_include_unresolved: callgraph_include_unresolved_for_call(
                bare_name, arguments,
            ),
            zoom_target_label: zoom_target_label_for_call(bare_name, arguments),
            ast_dry_run: ast_replace_dry_run_for_call(bare_name, arguments),
            import_op: import_string_arg_for_call(bare_name, arguments, "op"),
            import_remove_name: import_string_arg_for_call(bare_name, arguments, "removeName"),
            import_file_arg: import_string_arg_for_call(bare_name, arguments, "filePath"),
            import_module_arg: import_string_arg_for_call(bare_name, arguments, "module"),
            move_file_arg: move_string_arg_for_call(bare_name, arguments, "filePath"),
            move_dest_arg: move_string_arg_for_call(bare_name, arguments, "destination"),
            safety_op: safety_string_arg_for_call(bare_name, arguments, "op"),
            safety_file_arg: safety_string_arg_for_call(bare_name, arguments, "filePath"),
            safety_name_arg: safety_string_arg_for_call(bare_name, arguments, "name"),
            bash_output_mode: bash_output_mode_for_call(bare_name, arguments),
            worker_session: false,
            bash_watch_available: None,
            host_tool_names: None,
        }
    }
}

fn bash_output_mode_for_call(bare_name: &str, arguments: &Value) -> Option<String> {
    if bare_name != "bash_status" {
        return None;
    }
    let obj = arguments.as_object()?;
    obj.get("outputMode")
        .or_else(|| obj.get("output_mode"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn agent_specified_read_range(arguments: &Value) -> bool {
    let Some(obj) = arguments.as_object() else {
        return false;
    };
    obj.contains_key("startLine")
        || obj.contains_key("endLine")
        || obj.contains_key("offset")
        || obj.contains_key("limit")
}

fn outline_mode_for_call(bare_name: &str, arguments: &Value, _project_root: &Path) -> OutlineMode {
    if bare_name == "outline"
        && arguments
            .get("files")
            .and_then(crate::subc_translate::model_boolean)
            == Some(true)
    {
        OutlineMode::Files
    } else {
        OutlineMode::Text
    }
}

fn executed_outline_mode(data: &Value) -> OutlineMode {
    // files, files_list_envelope, or unchecked_files identify a files-table reply;
    // discovered=true identifies a directory-discovery reply. Select its renderer
    // from that completed reply, without consulting request hints or the target.
    if data.get("files").is_some_and(Value::is_array)
        || data
            .get("files_list_envelope")
            .is_some_and(Value::is_object)
        || data.get("unchecked_files").is_some_and(Value::is_array)
    {
        OutlineMode::Files
    } else if data.get("discovered").and_then(Value::as_bool) == Some(true) {
        OutlineMode::DirectoryJson
    } else {
        OutlineMode::Text
    }
}

fn callgraph_op_for_call(bare_name: &str, arguments: &Value) -> Option<String> {
    if bare_name != "callgraph" {
        return None;
    }
    arguments
        .as_object()
        .and_then(|obj| obj.get("op"))
        .and_then(Value::as_str)
        .filter(|op| !op.is_empty())
        .map(str::to_string)
}

fn callgraph_include_unresolved_for_call(bare_name: &str, arguments: &Value) -> bool {
    if bare_name != "callgraph" {
        return false;
    }
    arguments
        .as_object()
        .and_then(|obj| obj.get("includeUnresolved"))
        .is_some_and(coerce_boolean)
}

fn zoom_target_label_for_call(bare_name: &str, arguments: &Value) -> Option<String> {
    if bare_name != "zoom" {
        return None;
    }
    let obj = arguments.as_object()?;
    obj.get("batch")
        .and_then(Value::as_array)
        .filter(|entries| entries.len() == 1)
        .and_then(|entries| entries[0].get("target"))
        .or_else(|| obj.get("target"))
        .or_else(|| obj.get("path"))
        .or_else(|| obj.get("filePath"))
        .or_else(|| obj.get("url"))
        .and_then(Value::as_str)
        .filter(|label| !label.is_empty())
        .map(str::to_string)
}

fn ast_replace_dry_run_for_call(bare_name: &str, arguments: &Value) -> bool {
    if bare_name != "ast_replace" {
        return false;
    }
    arguments
        .as_object()
        .and_then(|obj| obj.get("dryRun").or_else(|| obj.get("dry_run")))
        .is_some_and(coerce_boolean)
}

fn import_string_arg_for_call(bare_name: &str, arguments: &Value, key: &str) -> Option<String> {
    if bare_name != "import" {
        return None;
    }
    arguments
        .as_object()
        .and_then(|obj| obj.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn move_string_arg_for_call(bare_name: &str, arguments: &Value, key: &str) -> Option<String> {
    if bare_name != "move" {
        return None;
    }
    arguments
        .as_object()
        .and_then(|obj| obj.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn safety_string_arg_for_call(bare_name: &str, arguments: &Value, key: &str) -> Option<String> {
    if bare_name != "safety" {
        return None;
    }
    arguments
        .as_object()
        .and_then(|obj| obj.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn coerce_boolean(value: &Value) -> bool {
    crate::subc_translate::model_boolean(value).unwrap_or(false)
}

// Return true for agent tools whose text output is formatted on the server.
// The capability manifest and this formatter both identify tools by their bare
// internal routing names, without the agent-facing `aft_` prefix.
fn is_core_agent_tool(bare_name: &str) -> bool {
    crate::subc_translate::supports_tool(bare_name)
}

/// Render the text block for a tool `CallToolResult` from the structured AFT `Response`.
pub fn format_response(
    bare_name: &str,
    response: &Response,
    agent_specified_range: bool,
) -> String {
    let ctx = FormatContext {
        agent_specified_range,
        ..FormatContext::default()
    };
    format_response_with_context(bare_name, response, &ctx)
}

/// Render the text block for a tool `CallToolResult` from the structured AFT `Response`.
pub fn format_response_with_context(
    bare_name: &str,
    response: &Response,
    ctx: &FormatContext,
) -> String {
    let mut text = format_response_unbounded(bare_name, response, ctx);
    crate::response_finalize::enforce_reply_ceiling(bare_name, &mut text);
    text
}

pub(crate) fn format_response_unbounded(
    bare_name: &str,
    response: &Response,
    ctx: &FormatContext,
) -> String {
    // The inspect tool is registered as `aft_inspect`; standalone tool calls
    // keep that spelling while subc hoists it to `inspect`. Both must reach
    // the inspect renderer rather than the raw-JSON fallback for tools the
    // formatter does not know.
    let bare_name = match bare_name {
        "aft_inspect" => "inspect",
        other => other,
    };
    if !is_core_agent_tool(bare_name) {
        return serialized_text_or_failure(
            serde_json::to_string(response),
            "non-core tool response",
        );
    }

    let data = &response.data;
    if !response.success {
        let text = format_error(bare_name, data, ctx);
        return if matches!(bare_name, "edit" | "grep" | "glob") {
            append_ast_skipped_files(text, data.get("skipped_files"))
        } else {
            text
        };
    }

    let mut text = match bare_name {
        "edit" => format_edit_response(data),
        "write" => format_write_response(data),
        "apply_patch" => format_apply_patch(data),
        "read" => format_read(data, ctx.agent_specified_range),
        "grep" => format_grep(data),
        "glob" => format_glob_response(data),
        "search" => format_search(data),
        "outline" => format_outline(response, executed_outline_mode(&response.data)),
        "zoom" => format_zoom(data, ctx),
        "inspect" => format_inspect(response),
        "status" => format_status(data),
        "bash" | "powershell" | "bash_watch" => {
            data["output"].as_str().unwrap_or_default().to_string()
        }
        "callgraph" => format_callgraph(
            ctx.callgraph_op.as_deref().unwrap_or("callgraph"),
            data,
            ctx.callgraph_include_unresolved,
        ),
        "conflicts" => data["text"].as_str().unwrap_or_default().to_string(),
        "ast_search" => format_ast_search(data),
        "ast_replace" => format_ast_replace(data, ctx.ast_dry_run),
        "delete" => format_delete(data),
        "move" => format_move(data, ctx),
        "import" => format_import(data, ctx),
        "safety" => format_safety(data, ctx),
        "bash_status" => format_bash_status(
            data,
            ctx.bash_output_mode.as_deref(),
            ctx.worker_session,
            ctx.bash_watch_available.unwrap_or(ctx.worker_session),
        ),
        "bash_kill" => format_bash_kill(data),
        "bash_write" => format_bash_write(data),
        _ => unreachable!("core agent tools are exhaustive"),
    };
    if bare_name == "edit" {
        text = append_ast_skipped_files(text, data.get("skipped_files"));
    }
    if matches!(bare_name, "read" | "outline" | "zoom") {
        if let Some(header) = revision_header(data) {
            text = if text.is_empty() {
                header
            } else {
                format!("{header}\n{text}")
            };
        }
    }
    crate::response_finalize::append_watcher_query_notice(&mut text, response);
    if bare_name == "callgraph" {
        crate::response_finalize::append_callgraph_pending_notice(&mut text, response);
    }
    if let Some(reason) = data.get("backup_skipped_reason").and_then(Value::as_str) {
        let notice = format!(
            "Undo is unavailable for this change because the backup snapshot was skipped ({reason})."
        );
        if text.is_empty() {
            text = notice;
        } else {
            text.push_str("\n\n");
            text.push_str(&notice);
        }
    }
    if matches!(bare_name, "edit" | "write") {
        if let Some(notice) = created_file_syntax_error_line(data) {
            if text.is_empty() {
                text = notice;
            } else {
                text.push_str("\n\n");
                text.push_str(&notice);
            }
        }
    }
    text
}

/// The first reply line of a `read`, `outline` or `zoom` that read a file at
/// a git revision: the revision as written and the commit it resolved to, so
/// the reader can never mistake the content for the working tree's.
fn revision_header(data: &Value) -> Option<String> {
    let revision = data.get("revision")?;
    let reference = revision.get("ref").and_then(Value::as_str)?;
    let commit = revision.get("commit").and_then(Value::as_str)?;
    Some(format!(
        "Revision `{reference}` = commit {commit} (read from git's object store, not the working tree)"
    ))
}

/// The final reply line for an `edit`/`write` call that created a file which
/// does not parse. The command handlers set `syntax_error_line` only for a
/// file they created; `created` and `syntax_valid` are checked as well so a
/// file that existed before the call can never get this line.
fn created_file_syntax_error_line(data: &Value) -> Option<String> {
    if data.get("created").and_then(Value::as_bool) != Some(true)
        || data.get("syntax_valid").and_then(Value::as_bool) != Some(false)
    {
        return None;
    }
    let line = data
        .get(crate::edit::CREATED_SYNTAX_ERROR_LINE_FIELD)
        .and_then(Value::as_u64)?;
    Some(crate::edit::created_file_syntax_error_notice(line))
}

fn import_string_field(response: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    response
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn import_number_field(response: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    response.get(key).and_then(import_number_value)
}

fn import_number_value(value: &Value) -> Option<String> {
    let number = value.as_number()?;
    if let Some(n) = number.as_i64() {
        Some(n.to_string())
    } else if let Some(n) = number.as_u64() {
        Some(n.to_string())
    } else {
        number.as_f64().map(|n| n.to_string())
    }
}

fn import_module_name(response: &serde_json::Map<String, Value>, ctx: &FormatContext) -> String {
    import_string_field(response, "module")
        .or_else(|| ctx.import_module_arg.clone())
        .unwrap_or_else(|| "(module)".to_string())
}

fn import_file_name(response: &serde_json::Map<String, Value>, ctx: &FormatContext) -> String {
    import_string_field(response, "file")
        .or_else(|| ctx.import_file_arg.clone())
        .unwrap_or_default()
}

fn format_apply_patch(data: &Value) -> String {
    if let Some(output) = data
        .get("output")
        .and_then(Value::as_str)
        .filter(|output| !output.is_empty())
    {
        return output.to_string();
    }

    let Some(files) = data
        .get("metadata")
        .and_then(|metadata| metadata.get("files"))
        .and_then(Value::as_array)
    else {
        return "Unable to format apply_patch: response omitted metadata.files.".to_string();
    };
    let mut lines = Vec::with_capacity(files.len());
    for file in files {
        let Some(kind) = file.get("type").and_then(Value::as_str) else {
            return "Unable to format apply_patch: a file omitted its change type.".to_string();
        };
        let Some(relative_path) = file
            .get("relativePath")
            .or_else(|| file.get("filePath"))
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        else {
            return "Unable to format apply_patch: a file omitted its path.".to_string();
        };
        let line = match kind {
            "add" => format!("Created {relative_path}"),
            "delete" => format!("Deleted {relative_path}"),
            "update" => format!("Updated {relative_path}"),
            "move" => {
                let Some(move_path) = file
                    .get("movePath")
                    .and_then(Value::as_str)
                    .filter(|path| !path.is_empty())
                else {
                    return "Unable to format apply_patch: a move omitted its destination."
                        .to_string();
                };
                format!("Moved {relative_path} → {move_path}")
            }
            other => {
                return format!(
                    "Unable to format apply_patch: response used unknown change type `{other}`."
                );
            }
        };
        lines.push(line);
    }
    lines.join("\n")
}

fn format_delete(data: &Value) -> String {
    let Some(response) = data.as_object() else {
        return "Deleted 0/0 file(s)".to_string();
    };
    let deleted = response
        .get("deleted")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let skipped = response
        .get("skipped_files")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    let mut text = if deleted.len() == 1 && skipped.is_empty() {
        let file = deleted[0]
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or_default();
        format!("Deleted {file}")
    } else {
        let total = deleted.len() + skipped.len();
        format!("Deleted {}/{} file(s)", deleted.len(), total)
    };
    // One `warning <text>` line per warning, the same shape undo uses.
    for warning in deleted
        .iter()
        .flat_map(|entry| string_array(entry.get("warnings")))
    {
        text.push_str("\nwarning ");
        text.push_str(&warning);
    }
    text
}
fn format_move(data: &Value, ctx: &FormatContext) -> String {
    let response = data.as_object();
    let Some(file) = ctx
        .move_file_arg
        .clone()
        .or_else(|| {
            response
                .and_then(|record| import_string_field(record, "file"))
                .map(|path| shorten_path(&path))
        })
        .filter(|path| !path.is_empty())
    else {
        return "Unable to format move: response omitted the source path.".to_string();
    };
    let Some(destination) = ctx
        .move_dest_arg
        .clone()
        .or_else(|| {
            response
                .and_then(|record| import_string_field(record, "destination"))
                .map(|path| shorten_path(&path))
        })
        .filter(|path| !path.is_empty())
    else {
        return "Unable to format move: response omitted the destination path.".to_string();
    };

    // Producer may mark a copy+delete-failed path as incomplete while both
    // paths still exist; never render that as a finished "Moved".
    let source_delete_failed = response
        .and_then(|record| record.get("source_delete_failed"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let incomplete = response
        .and_then(|record| record.get("complete"))
        .and_then(Value::as_bool)
        == Some(false);
    if source_delete_failed || incomplete {
        let message = response
            .and_then(|record| record.get("warning"))
            .and_then(Value::as_str)
            .and_then(extract_move_source_delete_message)
            .unwrap_or("response omitted the source-deletion error");
        return format!(
            "Partially moved {file} → {destination}; destination was written, but source deletion failed: {message}. Both paths exist. Verify the source and destination before retrying or accepting the duplicate."
        );
    }

    format!("Moved {file} → {destination}")
}

/// Pull the OS/error fragment out of the move producer's warning string.
fn extract_move_source_delete_message(warning: &str) -> Option<&str> {
    const PREFIX: &str =
        "destination was written, but source file could not be deleted after copy: ";
    let rest = warning.strip_prefix(PREFIX)?;
    rest.split(". Both paths")
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn format_import(data: &Value, ctx: &FormatContext) -> String {
    let Some(response) = data.as_object() else {
        return "No import result.".to_string();
    };

    match ctx.import_op.as_deref() {
        Some("organize") => {
            let group_text = response
                .get("groups")
                .and_then(Value::as_array)
                .filter(|groups| !groups.is_empty())
                .map(|groups| {
                    groups
                        .iter()
                        .map(|group| {
                            let name = group
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown");
                            let count = group
                                .get("count")
                                .and_then(import_number_value)
                                .unwrap_or_else(|| "0".to_string());
                            format!("{name}: {count}")
                        })
                        .collect::<Vec<_>>()
                        .join(" · ")
                })
                .unwrap_or_else(|| "No imports found".to_string());
            let removed_duplicates = import_number_field(response, "removed_duplicates")
                .unwrap_or_else(|| "0".to_string());
            [
                format!("organized {}", import_file_name(response, ctx)),
                format!("groups {group_text}"),
                format!("duplicates removed {removed_duplicates}"),
            ]
            .join("\n")
        }
        Some("add") => {
            let binding_names = |key| {
                response
                    .get(key)
                    .and_then(Value::as_array)
                    .map(|names| {
                        names
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default()
            };
            let status = if response.contains_key("added_names") {
                let added = binding_names("added_names");
                let present = binding_names("already_imported_names");
                let mut parts = Vec::new();
                if !added.is_empty() {
                    parts.push(format!("added {added}"));
                }
                if !present.is_empty() {
                    parts.push(format!("{present} already imported"));
                }
                parts.join("; ")
            } else {
                let action =
                    if response.get("already_present").and_then(Value::as_bool) == Some(true) {
                        "already present"
                    } else {
                        "added"
                    };
                format!("{action} {}", import_module_name(response, ctx))
            };
            [
                status,
                format!("file {}", import_file_name(response, ctx)),
                format!(
                    "group {}",
                    import_string_field(response, "group").unwrap_or_else(|| "—".to_string())
                ),
            ]
            .join("\n")
        }
        Some("remove") => {
            let module = import_module_name(response, ctx);
            let removed = response.get("removed").and_then(Value::as_bool) != Some(false);
            // Older producers have no remaining-binding metadata. Preserve their
            // scope summary instead of inventing a zero remaining-name count.
            if !response.contains_key("remaining_names") && !response.contains_key("only_name") {
                let status = if removed { "removed" } else { "not present" };
                let scope = ctx
                    .import_remove_name
                    .as_deref()
                    .filter(|name| !name.is_empty())
                    .map(|name| format!("name {name}"))
                    .unwrap_or_else(|| "scope entire import".to_string());
                return [
                    format!("{status} {module}"),
                    format!("file {}", import_file_name(response, ctx)),
                    scope,
                ]
                .join("\n");
            }
            let status = if !removed {
                format!("not present {module}")
            } else if let Some(name) = ctx
                .import_remove_name
                .as_deref()
                .filter(|name| !name.is_empty())
            {
                if response.get("only_name").and_then(Value::as_bool) == Some(true) {
                    format!("removed the {module} import (its only name)")
                } else {
                    let remaining = response
                        .get("remaining_names")
                        .and_then(import_number_value)
                        .unwrap_or_else(|| "0".to_string());
                    format!("removed {name} from the {module} import ({remaining} names remain)")
                }
            } else {
                format!("removed the {module} import")
            };
            [status, format!("file {}", import_file_name(response, ctx))].join("\n")
        }
        _ => "No import result.".to_string(),
    }
}

fn format_safety(data: &Value, ctx: &FormatContext) -> String {
    let Some(response) = data.as_object() else {
        return "No safety result.".to_string();
    };

    match ctx.safety_op.as_deref() {
        Some("undo") => {
            if response.get("operation").and_then(Value::as_bool) == Some(true) {
                let op_id = import_string_field(response, "op_id")
                    .unwrap_or_else(|| "(operation)".to_string());
                let files = import_number_field(response, "restored_count").unwrap_or_else(|| {
                    response
                        .get("restored")
                        .and_then(Value::as_array)
                        .map(|items| items.len().to_string())
                        .unwrap_or_else(|| "0".to_string())
                });
                let mut lines = vec![
                    format!("restored operation {op_id}"),
                    format!("files {files}"),
                ];
                lines.extend(
                    response
                        .get("warnings")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(|warning| format!("warning {warning}")),
                );
                lines.join("\n")
            } else {
                // Prefer the agent's own path spelling: translate resolves a
                // relative filePath against the project root before the undo
                // runs, so the response's `path` is absolute — echoing that
                // back at an agent that said "src/a.ts" reads as a different
                // file and costs a follow-up read.
                let file = ctx
                    .safety_file_arg
                    .clone()
                    .or_else(|| import_string_field(response, "path"))
                    .unwrap_or_else(|| "(file)".to_string());
                let backup =
                    import_string_field(response, "backup_id").unwrap_or_else(|| "—".to_string());
                let mut lines = vec![
                    format!("restored {}", shorten_path(&file)),
                    format!("backup {backup}"),
                ];
                if let Some(warning) = import_string_field(response, "warning") {
                    lines.push(format!("warning {warning}"));
                }
                lines.join("\n")
            }
        }
        Some("history") => {
            let file = import_string_field(response, "file")
                .or_else(|| ctx.safety_file_arg.clone())
                .unwrap_or_else(|| "(file)".to_string());
            let entries = records_field(response, "entries");
            let mut lines = vec![shorten_path(&file)];
            if entries.is_empty() {
                lines.push("No history entries.".to_string());
            } else {
                lines.push(
                    entries
                        .iter()
                        .enumerate()
                        .map(|(index, entry)| {
                            let backup_id = entry
                                .get("backup_id")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("entry-{}", index + 1));
                            let timestamp = entry
                                .get("timestamp")
                                .and_then(format_timestamp)
                                .unwrap_or_else(|| "unknown time".to_string());
                            let description = entry
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let mut line = format!("{}. {backup_id} {timestamp}", index + 1);
                            if !description.is_empty() {
                                line.push_str("\n   ");
                                line.push_str(description);
                            }
                            let mut labels = entry
                                .get("markers")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .filter(|marker| *marker != "external_change_checkpoint")
                                .map(|marker| marker.replace('_', " "))
                                .collect::<Vec<_>>();
                            if let Some(checkpoint) = entry
                                .get("external_change_checkpoint")
                                .and_then(Value::as_str)
                            {
                                labels.push(format!(
                                    "external change saved as checkpoint '{checkpoint}'"
                                ));
                            }
                            if entry.get("previous_file").and_then(Value::as_bool) == Some(true) {
                                let generation = entry
                                    .get("generation")
                                    .and_then(Value::as_u64)
                                    .unwrap_or_default();
                                labels.push(format!(
                                    "previous file at this path (#{})",
                                    generation + 1
                                ));
                            }
                            if !labels.is_empty() {
                                line.push_str("\n   [");
                                line.push_str(&labels.join("; "));
                                line.push(']');
                            }
                            line
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            lines.join("\n")
        }
        Some("checkpoint") => {
            let name = import_string_field(response, "name")
                .or_else(|| ctx.safety_name_arg.clone())
                .unwrap_or_else(|| "(checkpoint)".to_string());
            let files =
                import_number_field(response, "file_count").unwrap_or_else(|| "0".to_string());
            let file_count = number_field(response, "file_count").unwrap_or(0);
            let requested = number_field(response, "requested_count");
            let paths = string_array(response.get("paths"));
            let skipped = records_field(response, "skipped");
            let skipped_text = if skipped.is_empty() {
                "No skipped files.".to_string()
            } else {
                let details = skipped
                    .iter()
                    .map(|entry| {
                        let file = entry
                            .get("file")
                            .and_then(Value::as_str)
                            .unwrap_or("(file)");
                        let error = entry
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error");
                        format!("  ↳ {}: {error}", shorten_path(file))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("skipped\n{details}")
            };
            // A partial or empty checkpoint must not read like a full one.
            let header = if file_count == 0 {
                format!("checkpoint created {name} (empty)")
            } else {
                format!("checkpoint created {name}")
            };
            let files_line = match requested {
                Some(requested) if requested != file_count => {
                    format!("files {files} of {requested} requested")
                }
                _ => format!("files {files}"),
            };
            let mut lines = vec![header, files_line];
            lines.extend(checkpoint_path_lines(&paths, &[]));
            if file_count == 0 {
                lines.push(if requested.is_none() {
                    "Nothing was snapshotted: no files were named and this session has no AFT-edited files to fall back on. Name the files to keep with 'files' or 'path'.".to_string()
                } else {
                    "Nothing was snapshotted.".to_string()
                });
            }
            lines.push(skipped_text);
            let evicted = response
                .get("evicted")
                .and_then(Value::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|names| !names.is_empty());
            if let Some(evicted) = evicted {
                lines.push(format!("evicted {evicted}"));
            }
            if let Some(durability) = import_string_field(response, "durability") {
                lines.push(durability);
            }
            lines.join("\n")
        }
        Some("restore") => {
            let name = import_string_field(response, "name")
                .or_else(|| ctx.safety_name_arg.clone())
                .unwrap_or_else(|| "(checkpoint)".to_string());
            let files =
                import_number_field(response, "file_count").unwrap_or_else(|| "0".to_string());
            let file_count = number_field(response, "file_count").unwrap_or(0);
            let paths = string_array(response.get("paths"));
            let unchanged = string_array(response.get("unchanged"));
            let unchanged_count = i64::try_from(unchanged.len()).unwrap_or(i64::MAX);
            // Only a restore that changed at least one file reads as
            // "checkpoint restored"; an empty checkpoint, or one whose files
            // all already matched, says that nothing changed instead.
            let mut lines = if file_count == 0 {
                vec![
                    format!("checkpoint {name} holds no files; nothing was restored"),
                    format!("files {files}"),
                ]
            } else if unchanged_count >= file_count {
                vec![
                    format!("checkpoint {name}: nothing to restore"),
                    format!("files {files}, all already matched the checkpoint; none changed"),
                ]
            } else if unchanged_count > 0 {
                vec![
                    format!("checkpoint restored {name}"),
                    format!("files {files} ({unchanged_count} already matched the checkpoint)"),
                ]
            } else {
                vec![
                    format!("checkpoint restored {name}"),
                    format!("files {files}"),
                ]
            };
            lines.extend(checkpoint_path_lines(&paths, &unchanged));
            if let Some(durability) = import_string_field(response, "durability") {
                lines.push(durability);
            }
            lines.join("\n")
        }
        Some("list") => {
            let checkpoints = records_field(response, "checkpoints");
            let mut lines = vec![format!("{} checkpoint(s)", checkpoints.len())];
            if checkpoints.is_empty() {
                lines.push("No checkpoints saved.".to_string());
            } else {
                lines.push(
                    checkpoints
                        .iter()
                        .enumerate()
                        .map(|(index, checkpoint)| {
                            let name = checkpoint
                                .get("name")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("checkpoint-{}", index + 1));
                            let file_count = checkpoint
                                .get("file_count")
                                .and_then(import_number_value)
                                .unwrap_or_else(|| "0".to_string());
                            let created = checkpoint
                                .get("created_at")
                                .and_then(format_timestamp)
                                .unwrap_or_else(|| "unknown time".to_string());
                            format!("{}. {name} {file_count} file(s) · {created}", index + 1)
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            if let Some(durability) = import_string_field(response, "durability") {
                lines.push(durability);
            }
            lines.join("\n")
        }
        Some("purge") => format_safety_purge(response),
        _ => "No safety result.".to_string(),
    }
}

/// Reply text for `aft_safety` op `purge`: what was removed, what could not
/// be, and that none of it can be recovered.
fn format_safety_purge(response: &serde_json::Map<String, Value>) -> String {
    let path = import_string_field(response, "path").unwrap_or_else(|| "(path)".to_string());
    let stacks = number_field(response, "undo_stacks_removed").unwrap_or(0);
    let entries = number_field(response, "undo_entries_removed").unwrap_or(0);
    let complete = response.get("complete").and_then(Value::as_bool) != Some(false);
    let mut lines = vec![if complete {
        format!("purged {}", shorten_path(&path))
    } else {
        format!("purged {} (incomplete)", shorten_path(&path))
    }];
    lines.push(if stacks == 0 {
        "undo: no backups of this session held this path".to_string()
    } else {
        format!("undo: removed {entries} backup(s) of {stacks} file(s)")
    });
    let pruned = records_field(response, "checkpoints_pruned")
        .iter()
        .filter_map(|record| {
            let name = record.get("name").and_then(Value::as_str)?;
            let files = record
                .get("files_removed")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Some(format!("{name} ({files} file(s) removed)"))
        })
        .collect::<Vec<_>>();
    let dropped = string_array(response.get("checkpoints_dropped"));
    if pruned.is_empty() && dropped.is_empty() {
        lines.push("checkpoints: none of this session's checkpoints held this path".to_string());
    } else {
        if !pruned.is_empty() {
            lines.push(format!("checkpoints pruned: {}", pruned.join(", ")));
        }
        if !dropped.is_empty() {
            lines.push(format!("checkpoints dropped: {}", dropped.join(", ")));
        }
    }
    for failure in records_field(response, "failures") {
        let target = failure
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("(unknown)");
        let error = failure
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        lines.push(format!("not removed: {target}: {error}"));
    }
    lines.push(
        "This cannot be undone: the removed copies are gone and aft_safety undo or restore cannot bring them back."
            .to_string(),
    );
    lines.join("\n")
}

/// Most paths a checkpoint or restore result lists before summarising the rest.
const CHECKPOINT_PATH_LINE_LIMIT: usize = 10;

/// One `↳ path` line per checkpoint file, marking those in `unchanged`, so an
/// agent can see which files a checkpoint or restore actually covered.
fn checkpoint_path_lines(paths: &[String], unchanged: &[String]) -> Vec<String> {
    let mut lines = paths
        .iter()
        .take(CHECKPOINT_PATH_LINE_LIMIT)
        .map(|path| {
            if unchanged.contains(path) {
                format!("  ↳ {} (unchanged)", shorten_path(path))
            } else {
                format!("  ↳ {}", shorten_path(path))
            }
        })
        .collect::<Vec<_>>();
    if paths.len() > CHECKPOINT_PATH_LINE_LIMIT {
        lines.push(format!(
            "  ↳ … and {} more",
            paths.len() - CHECKPOINT_PATH_LINE_LIMIT
        ));
    }
    lines
}

fn format_timestamp(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str().filter(|text| !text.is_empty()) {
        return Some(text.to_string());
    }
    let number = value.as_f64()?;
    if !number.is_finite() {
        return None;
    }
    let millis = if number > 1_000_000_000_000.0 {
        number
    } else {
        number * 1000.0
    };
    const JS_DATE_MAX_MILLIS: f64 = 8_640_000_000_000_000.0;
    if !millis.is_finite()
        || millis.abs() > JS_DATE_MAX_MILLIS
        || millis < i64::MIN as f64
        || millis > i64::MAX as f64
    {
        return Some(value_to_plain_string(value));
    }
    Some(format_unix_millis_utc(millis.trunc() as i64))
}

pub(crate) fn format_unix_millis_utc(millis: i64) -> String {
    let seconds = div_floor_i64(millis, 1000);
    let millisecond = millis.rem_euclid(1000);
    let days = div_floor_i64(seconds, 86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    if millisecond == 0 {
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}Z")
    } else {
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{millisecond:03}Z")
    }
}

fn div_floor_i64(value: i64, divisor: i64) -> i64 {
    let quotient = value / divisor;
    let remainder = value % divisor;
    if remainder != 0 && ((remainder > 0) != (divisor > 0)) {
        quotient - 1
    } else {
        quotient
    }
}

pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

// Mirrors per-tool OpenCode wrapper error handling in packages/opencode-plugin/src/tools/*.ts.
fn bash_task_id(data: &Value) -> &str {
    data.get("task_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

// Mirrors packages/opencode-plugin/src/tools/bash.ts formatBashStatusText and
// formatPtyStatus, so a catalog consumer shows the model the same status text
// the OpenCode tool does.
pub(crate) fn format_bash_task_status_header(
    task_id: &str,
    status: &str,
    exit_code: Option<i64>,
    status_reason: Option<&str>,
) -> String {
    let mut text = format!("Task {task_id}: {status}");
    if status == "killed" {
        if let Some(label) = crate::bash_background::registry::scope_end_label(status_reason) {
            text.push_str(&format!(" ({label})"));
        }
    }
    if let Some(exit_code) = exit_code {
        text.push_str(&format!(" (exit {exit_code})"));
    }
    text
}

fn format_bash_status(
    data: &Value,
    output_mode: Option<&str>,
    worker_session: bool,
    bash_watch_available: bool,
) -> String {
    let task_id = bash_task_id(data);
    let status = data
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut text = format_bash_task_status_header(
        task_id,
        status,
        data.get("exit_code").and_then(Value::as_i64),
        data.get("status_reason").and_then(Value::as_str),
    );
    let mut paged = false;
    for (stream, cursor) in [("output", "outputOffset"), ("stderr", "stderrOffset")] {
        if let Some(page) = data
            .get(format!("{stream}_page_text"))
            .and_then(Value::as_str)
        {
            text.push_str(&format!(
                "\n{stream} page (next {cursor}: {}, has_more: {}):\n{page}",
                data[format!("{stream}_next_offset")],
                data[format!("{stream}_has_more")]
            ));
            paged = true;
        }
    }
    if paged {
        return text;
    }
    if let Some(duration_ms) = data.get("duration_ms").and_then(Value::as_u64) {
        // Rounded to the nearest second, as the plugin displays it.
        text.push_str(&format!(" {}s", duration_ms.saturating_add(500) / 1000));
    }
    if let Some(summary) = data.get("live_descendants_summary").and_then(Value::as_str) {
        text.push_str(&format!(" · {summary}"));
    }
    if status == "running" {
        if let Some(job_id) = data.get("remote_job_id").and_then(Value::as_str) {
            text.push_str(&format!(" · remote job {job_id}"));
        }
    }
    // A remote task's phase note distinguishes input capture from queueing
    // and reports the runner's stop deadline once execution starts.
    if let Some(note) = data.get("remote_phase_note").and_then(Value::as_str) {
        text.push_str(&format!(" · {note}"));
    }
    let status_reason = data
        .get("status_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty());
    if data.get("output_incomplete").and_then(Value::as_bool) == Some(true)
        || (status == "failed" && status_reason.is_some())
    {
        let reason = status_reason.unwrap_or("PTY output may be incomplete");
        text.push_str(&format!("\n[{reason}]"));
    }
    let running = status == "running";
    if data.get("mode").and_then(Value::as_str) == Some("pty") {
        let raw = data
            .get("pty_raw")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let screen = data
            .get("pty_screen")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match output_mode.unwrap_or("screen") {
            "raw" => {
                if !raw.is_empty() {
                    text.push('\n');
                    text.push_str(raw);
                }
            }
            "both" => {
                // Built by hand so the keys come out as `screen` then `raw`,
                // the order the plugin prints (serde_json would sort them).
                text.push_str(&format!(
                    "\n{{\n  \"screen\": {},\n  \"raw\": {}\n}}",
                    Value::String(screen.to_string()),
                    Value::String(raw.to_string()),
                ));
            }
            _ => {
                if !screen.is_empty() {
                    text.push('\n');
                    text.push_str(screen);
                }
            }
        }
        if running {
            text.push_str(&format!(
                "\nPTY task is still running. Use bash_status({{ taskId: \"{task_id}\", outputMode: \"screen\" }}) to inspect, bash_write({{ taskId: \"{task_id}\", input: \"...\" }}) to send keystrokes."
            ));
        }
    } else {
        let preview = data
            .get("output_preview")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !preview.is_empty() && !running {
            text.push('\n');
            text.push_str(preview);
        }
        if running {
            // Mirrors `runningTaskStatusHint` in the plugins.
            text.push_str(if worker_session && bash_watch_available {
                "\nTo wait for it, call bash_watch; don't poll."
            } else if worker_session {
                "\nIt won't wake you when it finishes. Use bash_status to check whether it has finished."
            } else {
                "\nA completion reminder will be delivered automatically; don't poll."
            });
        }
    }
    text
}

// Formats a kill result exactly as the OpenCode tool does
// (packages/opencode-plugin/src/tools/bash.ts createBashKillTool), so catalog
// consumers show the model the same text.
fn format_bash_kill(data: &Value) -> String {
    let task_id = bash_task_id(data);
    let outcome = if data.get("kill_signaled").and_then(Value::as_bool) == Some(true) {
        let reached = data
            .get("kill_reached")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        format!("Task {task_id}: kill_signaled · reached {reached} live descendants")
    } else {
        let status = data
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("killed");
        if let Some(cancel) = data.get("remote_cancel").and_then(Value::as_str) {
            format!("Task {task_id}: {status} · {cancel}")
        } else {
            format!("Task {task_id}: {status}")
        }
    };
    match data
        .get("bot_write_revocation_warning")
        .and_then(Value::as_str)
    {
        Some(warning) => format!("{outcome}\n{warning}"),
        None => outcome,
    }
}

// Formats a PTY write result exactly as the OpenCode tool does
// (packages/opencode-plugin/src/tools/bash_write.ts createBashWriteTool).
fn format_bash_write(data: &Value) -> String {
    format!(
        "{{\n  \"bytes_written\": {}\n}}",
        data.get("bytes_written").cloned().unwrap_or(Value::Null)
    )
}

fn format_error(bare_name: &str, data: &Value, ctx: &FormatContext) -> String {
    if bare_name == "inspect" {
        if let Some(text) = format_inspect_terminal(data) {
            return text;
        }
    }
    if bare_name == "callgraph" {
        return format_callgraph_error(ctx.callgraph_op.as_deref().unwrap_or("callgraph"), data);
    }
    let code = data
        .get("code")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("request failed");
    match (bare_name, code) {
        ("search", Some(c)) => format!("semantic_search: {c} — {message}"),
        _ => message.to_string(),
    }
}

// Mirrors packages/opencode-plugin/src/tools/hoisted.ts createWriteTool.
fn format_write_response(data: &Value) -> String {
    if let Some(text) = github_comment_text(data) {
        return text;
    }
    if data.get("rolled_back").and_then(Value::as_bool) == Some(true) {
        return "Write rolled back: the content produced invalid syntax, so the file was left unchanged."
            .to_string();
    }

    let mut output = if data.get("created").and_then(Value::as_bool) == Some(true) {
        "Created new file.".to_string()
    } else {
        "File updated.".to_string()
    };
    if is_truthy_formatted(data) {
        output.push_str(" Auto-formatted.");
    }
    if data.get("no_op").and_then(Value::as_bool) == Some(true) {
        output.push_str(
            " No net change — the written content is byte-identical to what was already on disk.",
        );
    }
    append_lsp_error_lines(&mut output, data, true);
    append_lsp_server_notes(&mut output, data);
    if data.get("created").and_then(Value::as_bool) == Some(true)
        && data.get("syntax_valid").and_then(Value::as_bool).is_none()
    {
        if let Some(reason) = data.get("syntax_skipped_reason").and_then(Value::as_str) {
            output.push_str("\n\n");
            output.push_str(&crate::edit::syntax_unknown_notice(reason));
        }
    }
    output
}

// Mirrors packages/opencode-plugin/src/tools/hoisted.ts createEditTool.
fn format_edit_response(data: &Value) -> String {
    if let Some(text) = github_comment_text(data) {
        return text;
    }
    if data.get("hashline").and_then(Value::as_bool) == Some(true) {
        return data
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
    }
    let mut result = format_edit_summary(data);

    if let Some(note) = format_glob_skip_reasons_note(data.get("format_skip_reasons")) {
        result.push_str("\n\n");
        result.push_str(&note);
    }
    if data.get("no_op").and_then(Value::as_bool) == Some(true)
        && data.get("rolled_back").and_then(Value::as_bool) != Some(true)
    {
        result.push_str(
            "\n\nNote: no net file change — the match was found and applied, but the file content is byte-identical to before. Likely causes: oldString and newString are identical, or a formatter normalized the change away.",
        );
    }
    append_lsp_error_lines(&mut result, data, false);
    append_lsp_server_notes(&mut result, data);
    if let Some(warnings) = data.get("warnings").and_then(Value::as_array) {
        for warning in warnings.iter().filter_map(Value::as_str) {
            result.push_str("\n\nWarning: ");
            result.push_str(warning);
        }
    }
    result
}

/// A posted or edited GitHub comment carries its own rendered text (comment
/// URL, ordinal, and the no-undo notice). The file summaries ("Created new
/// file.", "File updated.") would tell the agent the opposite of what
/// happened, so that text is shown as-is.
fn github_comment_text(data: &Value) -> Option<String> {
    data.get("comment_url").and_then(Value::as_str)?;
    data.get("text").and_then(Value::as_str).map(str::to_string)
}

fn format_glob_skip_reasons_note(reasons: Option<&Value>) -> Option<String> {
    let actionable = reasons?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|reason| {
            matches!(
                *reason,
                "formatter_not_installed"
                    | "formatter_version_check_timeout"
                    | "formatter_excluded_path"
                    | "timeout"
                    | "error"
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    if actionable.is_empty() {
        None
    } else {
        Some(format!(
            "Note: formatter skipped some glob edit result file(s): {}. See per-file format_skipped_reason values for details.",
            actionable.into_iter().collect::<Vec<_>>().join(", ")
        ))
    }
}

fn append_lsp_error_lines(output: &mut String, data: &Value, trailing_newline: bool) {
    let errors = data
        .get("lsp_diagnostics")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|d| d.get("severity").and_then(Value::as_str) == Some("error"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if errors.is_empty() {
        return;
    }

    output.push_str("\n\nLSP errors detected, please fix:\n");
    let lines = errors
        .iter()
        .map(|d| {
            let line = d
                .get("line")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "undefined".to_string());
            let message = d
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("undefined");
            format!("  Line {line}: {message}")
        })
        .collect::<Vec<_>>();
    output.push_str(&lines.join("\n"));
    if trailing_newline {
        output.push('\n');
    }
}

fn append_lsp_server_notes(output: &mut String, data: &Value) {
    let pending = string_array(data.get("lsp_pending_servers"));
    if !pending.is_empty() {
        let status = data
            .get("lsp_status")
            .and_then(Value::as_str)
            .unwrap_or("Diagnostics are incomplete for this call");
        output.push_str(&format!(
            "\n\nNote: LSP server(s) did not respond in time: {}. {}; wait for the LSP update and use the next normal aft_inspect, not repeated polling.",
            pending.join(", "), status
        ));
    }
    let exited = string_array(data.get("lsp_exited_servers"));
    if !exited.is_empty() {
        output.push_str(&format!(
            "\n\nNote: LSP server(s) exited during this edit: {}. Their diagnostics could not be collected.",
            exited.join(", ")
        ));
    }
}

#[cfg(test)]
mod edit_diagnostics_tests {
    #[test]
    fn syntax_rollback_omits_no_net_change_hint() {
        let output = super::format_edit_response(&serde_json::json!({
            "rolled_back": true,
            "no_op": true,
            "syntax_valid": false
        }));
        assert!(output.starts_with("Edit rolled back:"), "{output}");
        assert!(!output.contains("Likely causes"), "{output}");
        assert_eq!(
            output,
            "Edit rolled back: the change produced invalid syntax, so the file was left unchanged."
        );
        let no_op = super::format_edit_response(&serde_json::json!({"no_op": true}));
        assert!(no_op.contains("Likely causes"), "{no_op}");
    }

    #[test]
    fn created_file_syntax_line_is_last_and_only_for_created_invalid_files() {
        let render = |name: &str, data: serde_json::Value| {
            super::format_response(name, &crate::protocol::Response::success("1", data), false)
        };
        let expected = "Syntax check: the new file does not parse (first error near line 7); it was written as given.";
        let created = serde_json::json!({
            "created": true, "syntax_valid": false, "syntax_error_line": 7,
            "backup_skipped_reason": "disabled",
        });
        let write = render("write", created.clone());
        assert_eq!(write.lines().last(), Some(expected), "{write}");
        assert_eq!(write.matches("Syntax check:").count(), 1, "{write}");
        let edit = render("edit", created);
        assert_eq!(edit.lines().last(), Some(expected), "{edit}");

        for data in [
            serde_json::json!({"created": false, "syntax_valid": false, "syntax_error_line": 7}),
            serde_json::json!({"created": true, "syntax_valid": true, "syntax_error_line": 7}),
            serde_json::json!({"created": true, "syntax_valid": false}),
        ] {
            let text = render("write", data);
            assert!(!text.contains("Syntax check"), "{text}");
        }
    }

    #[test]
    fn edit_summary_renders_each_loose_match_notice_once() {
        let notice = "oldString matched only after ignoring indentation and trailing whitespace; newString was re-indented to match the file.";
        let data = serde_json::json!({
            "edits_applied": 1,
            "diff": {"additions": 2, "deletions": 2},
            "fuzzy_matches": [
                {"pass": 3, "replaced_text": [], "notice": notice},
                {"pass": 3, "replaced_text": [], "notice": notice},
                {"pass": 4, "replaced_text": ["title = “a”"], "notice": "four"},
            ],
        });
        assert_eq!(
            super::format_edit_summary(&data),
            format!("Edited (+2/-2).\n{notice}\nfour\nReplaced text 1: title = “a”")
        );
        // An exact match carries no fuzzy field and no notice.
        assert_eq!(
            super::format_edit_summary(
                &serde_json::json!({"diff": {"additions": 1, "deletions": 1}})
            ),
            "Edited (+1/-1)."
        );
    }

    #[test]
    fn frozen_lsp_edit_rendering_reports_diagnostics_unknown() {
        let data = serde_json::json!({
            "replacements": 1, "lsp_complete": false, "lsp_diagnostics": [],
            "lsp_pending_servers": ["typescript"],
            "lsp_status": "diagnostics unknown (server not responding)",
        });
        let output = super::format_edit_response(&data);
        assert!(
            output.contains("diagnostics unknown (server not responding)"),
            "{output}"
        );
    }
}

// Mirrors packages/aft-bridge/src/edit-summary.ts formatEditSummary.
fn format_edit_summary(data: &Value) -> String {
    if data.get("rolled_back").and_then(Value::as_bool) == Some(true) {
        return "Edit rolled back: the change produced invalid syntax, so the file was left unchanged."
            .to_string();
    }

    if let Some(n) = data.get("files_modified").and_then(Value::as_u64) {
        let n = n as usize;
        return format!(
            "Applied edits to {} file{}.",
            n,
            if n == 1 { "" } else { "s" }
        );
    }

    if let Some(files) = data.get("total_files").and_then(Value::as_u64) {
        let files = files as usize;
        let reps = data
            .get("total_replacements")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        return format!(
            "Edited {} file{} ({} replacement{}).",
            files,
            if files == 1 { "" } else { "s" },
            reps,
            if reps == 1 { "" } else { "s" }
        );
    }

    let additions = data
        .get("diff")
        .and_then(Value::as_object)
        .and_then(|d| d.get("additions"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let deletions = data
        .get("diff")
        .and_then(Value::as_object)
        .and_then(|d| d.get("deletions"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let counts = format!("+{additions}/-{deletions}");

    if data.get("created").and_then(Value::as_bool) == Some(true) {
        let mut s = format!("Created file ({counts}).");
        if is_truthy_formatted(data) {
            s.push_str(&format_auto_formatted_suffix(data));
        }
        return s;
    }

    let mut detail = counts.clone();
    if let Some(n) = data.get("edits_applied").and_then(Value::as_u64) {
        if n > 1 {
            detail = format!("{counts}, {n} edits");
        }
    } else if let Some(n) = data.get("replacements").and_then(Value::as_u64) {
        if n > 1 {
            detail = format!("{counts}, {n} replacements");
        }
    }

    let mut s = format!("Edited ({detail}).");
    if is_truthy_formatted(data) {
        s.push_str(&format_auto_formatted_suffix(data));
    }
    append_loose_match_notices(&mut s, data);
    s
}

/// One line per loose (fuzzy) match: the producer's `notice`, followed by
/// any file text that differed from `oldString` in more than whitespace. A
/// notice identical to the previous one is not repeated, so a replace-all
/// over many identical matches shows it once. Mirrors the fuzzy block of
/// packages/aft-bridge/src/edit-summary.ts formatEditSummary.
fn append_loose_match_notices(s: &mut String, data: &Value) {
    let entries: Vec<&Value> = match data.get("fuzzy_matches").and_then(Value::as_array) {
        Some(entries) => entries.iter().collect(),
        None => data.get("fuzzy_match").into_iter().collect(),
    };
    let mut previous_notice: Option<&str> = None;
    for entry in entries {
        let notice = entry.get("notice").and_then(Value::as_str);
        if let Some(notice) = notice.filter(|notice| previous_notice != Some(*notice)) {
            s.push('\n');
            s.push_str(notice);
        }
        previous_notice = notice.or(previous_notice);
        for (index, text) in entry
            .get("replaced_text")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .enumerate()
        {
            s.push_str(&format!("\nReplaced text {}: {text}", index + 1));
        }
    }
}

fn is_truthy_formatted(data: &Value) -> bool {
    data.get("formatted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn format_auto_formatted_suffix(data: &Value) -> String {
    let reformatted = data.get("reformatted").and_then(Value::as_object);
    if let Some(text) = reformatted
        .and_then(|r| r.get("text"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return format!(
            "\nAuto-formatted — the formatter reflowed your edit. On disk now:\n{text}"
        );
    }
    if reformatted
        .and_then(|r| r.get("extensive"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return " Auto-formatted — extensive reflow; re-read the file before your next anchored edit."
            .to_string();
    }
    " Auto-formatted.".to_string()
}

// Mirrors packages/opencode-plugin/src/tools/hoisted.ts createReadTool.
fn format_read(data: &Value, agent_specified_range: bool) -> String {
    if let Some(entries) = data.get("entries").and_then(Value::as_array) {
        return entries
            .iter()
            .filter_map(|e| e.as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }

    if let Some(attachment_line) = format_read_attachments(data) {
        return attachment_line;
    }

    if data.get("binary").and_then(Value::as_bool).unwrap_or(false) {
        return data
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Binary file")
            .to_string();
    }

    let mut text = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    text.push_str(&format_read_footer(agent_specified_range, data));
    text
}

fn format_read_attachments(data: &Value) -> Option<String> {
    let attachments = data.get("attachments")?.as_array()?;
    let has_host_attachment = attachments.iter().any(|attachment| {
        attachment.get("mime").and_then(Value::as_str).is_some()
            && attachment.get("data").and_then(Value::as_str).is_some()
    });
    if !has_host_attachment {
        return None;
    }

    if let Some(content) = data
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())
    {
        return Some(content.to_string());
    }

    let first = attachments.first()?.as_object()?;
    let kind = first.get("kind").and_then(Value::as_str).unwrap_or("file");
    let mime = first
        .get("mime")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");
    let size = first
        .get("bytes")
        .and_then(Value::as_u64)
        .map(format_attachment_size);

    if kind == "image" || mime.starts_with("image/") {
        let dimensions = match (
            first.get("width").and_then(Value::as_u64),
            first.get("height").and_then(Value::as_u64),
        ) {
            (Some(width), Some(height)) => format!(", {width}×{height}"),
            _ => String::new(),
        };
        let resized = if first.get("resized").and_then(Value::as_bool) == Some(true) {
            ", resized"
        } else {
            ""
        };
        let size = size.map(|size| format!(", {size}")).unwrap_or_default();
        return Some(format!("Read image ({mime}{dimensions}{resized}{size})."));
    }

    if kind == "pdf" || mime == "application/pdf" {
        let size = size.map(|size| format!(" ({size})")).unwrap_or_default();
        return Some(format!("Read PDF{size}."));
    }

    let size = size.map(|size| format!(", {size}")).unwrap_or_default();
    Some(format!("Read attachment ({mime}{size})."))
}

fn format_attachment_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{bytes} bytes")
    }
}

fn format_read_footer(agent_specified_range: bool, data: &Value) -> String {
    if agent_specified_range {
        return String::new();
    }
    if !data
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        // Not truncated. If the caller read the file whole and it came back
        // large, offer a soft note that ranges exist — but only when the
        // caller did not already pick a range (checked above).
        let content_len = data
            .get("content")
            .and_then(Value::as_str)
            .map(str::len)
            .unwrap_or(0);
        if content_len > READ_SOFT_NOTE_BYTES {
            return READ_SOFT_NOTE.to_string();
        }
        return String::new();
    }
    let start = data.get("start_line").and_then(Value::as_u64);
    let end = data.get("end_line").and_then(Value::as_u64);
    let total = data.get("total_lines").and_then(Value::as_u64);
    match (start, end, total) {
        (Some(start), Some(end), Some(total)) => format!(
            "\n(Showing lines {start}-{end} of {total}. Use startLine/endLine or offset/limit to read other sections.)"
        ),
        _ => String::new(),
    }
}

// Mirrors packages/opencode-plugin/src/tools/search.ts formatGrepOutput.
pub(crate) fn append_glob_envelope_trailer(
    text: &str,
    envelope: &crate::list_envelope::ListEnvelope,
) -> String {
    const GLOB_TRUNCATED_MESSAGE: &str =
        "(Results are truncated: showing first 100 results. Consider using a more specific path or pattern.)";
    let clean = text
        .replace(&format!("\n\n{GLOB_TRUNCATED_MESSAGE}"), "")
        .replace(GLOB_TRUNCATED_MESSAGE, "");
    let Some(trailer) = render_envelope_trailer(envelope) else {
        return clean;
    };
    let suffix = format!("\n\n{trailer}");
    if clean.ends_with(&suffix) {
        clean
    } else {
        format!("{clean}{suffix}")
    }
}

fn format_glob_response(data: &Value) -> String {
    let base = data["text"].as_str().unwrap_or_default();
    let envelope = data
        .get("files_list_envelope")
        .and_then(|v| serde_json::from_value::<crate::list_envelope::ListEnvelope>(v.clone()).ok());
    if let Some(env) = envelope {
        let trailer = render_envelope_trailer(&env).map(|trailer| format!("\n\n{trailer}"));
        let base = trailer
            .as_ref()
            .and_then(|trailer| base.strip_suffix(trailer))
            .unwrap_or(base);
        let base = append_ast_skipped_files(base.to_string(), data.get("skipped_files"));
        append_glob_envelope_trailer(&base, &env)
    } else {
        append_ast_skipped_files(base.to_string(), data.get("skipped_files"))
    }
}

fn format_grep(data: &Value) -> String {
    let envelope = data
        .get("matches_list_envelope")
        .and_then(|v| serde_json::from_value::<crate::list_envelope::ListEnvelope>(v.clone()).ok());

    let base = if let Some(text) = data.get("text").and_then(Value::as_str) {
        if envelope.is_some() {
            text.replace(" (capped)", "")
        } else {
            text.to_string()
        }
    } else {
        let matches = data
            .get("matches")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total_matches = data
            .get("total_matches")
            .and_then(Value::as_u64)
            .unwrap_or(matches.len() as u64);
        let files_with_matches = data
            .get("files_with_matches")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| {
                matches
                    .iter()
                    .filter_map(|m| m.get("file").and_then(Value::as_str))
                    .collect::<std::collections::BTreeSet<_>>()
                    .len() as u64
            });

        if matches.is_empty() {
            crate::commands::grep::found_matches_phrase(
                total_matches as usize,
                files_with_matches as usize,
            )
        } else {
            let body = matches
                .iter()
                .map(|m| {
                    let file = m.get("file").and_then(Value::as_str).unwrap_or("unknown");
                    let line = m.get("line").and_then(Value::as_u64).unwrap_or(0);
                    let text = m
                        .get("line_text")
                        .or_else(|| m.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    format!("{file}:{line}: {text}")
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "{body}\n\n{}",
                crate::commands::grep::found_matches_phrase(
                    total_matches as usize,
                    files_with_matches as usize,
                )
            )
        }
    };

    let base = append_ast_skipped_files(base, data.get("skipped_files"));
    if let Some(env) = envelope {
        if let Some(trailer) = render_envelope_trailer(&env) {
            if base.is_empty() {
                trailer
            } else {
                format!("{base}\n\n{trailer}")
            }
        } else {
            base
        }
    } else {
        base
    }
}

fn format_ast_search(data: &Value) -> String {
    let matches = data.get("matches").and_then(Value::as_array);
    let match_count = data
        .get("total_matches")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| matches.map(|m| m.len() as u64).unwrap_or(0));
    let files_searched = data
        .get("files_searched")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let files_with_matches = data
        .get("files_with_matches")
        .and_then(Value::as_u64)
        .unwrap_or(files_searched);

    let mut output = if data.get("no_files_matched_scope").and_then(Value::as_bool) == Some(true) {
        let mut output =
            "No files matched the scope (paths/globs resolved to zero files)".to_string();
        append_scope_warnings(&mut output, data);
        output
    } else if match_count == 0 {
        let mut output = format!("No matches found (searched {files_searched} files)");
        append_scope_warnings(&mut output, data);
        append_hint(&mut output, data);
        output
    } else {
        let mut output = format!(
            "Found {match_count} match(es) in {files_with_matches} file(s) ({files_searched} searched)\n\n"
        );
        if let Some(matches) = matches {
            for m in matches {
                let rel_file = m.get("file").and_then(Value::as_str).unwrap_or("unknown");
                let line = m.get("line").and_then(Value::as_u64).unwrap_or(0);
                output.push_str(&format!("{rel_file}:{line}\n"));
                let match_text = m.get("text").and_then(Value::as_str).unwrap_or("");
                if !match_text.is_empty() {
                    output.push_str(&format!("  {}\n", match_text.trim()));
                }
                if let Some(meta_vars) = m.get("meta_variables").and_then(Value::as_object) {
                    if !meta_vars.is_empty() {
                        let capture_text = m.get("meta_variable_text").and_then(Value::as_object);
                        for (key, value) in meta_vars {
                            // A `$$$` capture shows its source text, not its
                            // node list, when the engine supplied it.
                            let value = capture_text
                                .and_then(|texts| texts.get(key))
                                .filter(|text| text.is_string())
                                .unwrap_or(value);
                            output.push_str(&format!(
                                "  {}\n",
                                format_ast_search_capture(key, value, match_text, line)
                            ));
                        }
                    }
                }
                output.push('\n');
            }
        }
        output
    };

    if data.get("complete").and_then(Value::as_bool) == Some(false)
        || data
            .get("skipped_files")
            .and_then(Value::as_array)
            .is_some_and(|skipped| !skipped.is_empty())
    {
        output = append_ast_skipped_files(output, data.get("skipped_files"));
    }
    if let Some(env) = data.get("matches_list_envelope").and_then(|value| {
        serde_json::from_value::<crate::list_envelope::ListEnvelope>(value.clone()).ok()
    }) {
        if let Some(trailer) = render_envelope_trailer(&env) {
            output.push_str(&format!("\n\n{trailer}"));
        }
    }
    output
}

const AST_CAPTURE_INLINE_CHAR_LIMIT: usize = 80;

fn format_ast_search_capture(
    key: &str,
    value: &Value,
    match_text: &str,
    match_line: u64,
) -> String {
    let rendered = js_template_string(value);
    let multiline = rendered.contains('\n') || rendered.contains('\r');
    if !multiline && rendered.chars().count() <= AST_CAPTURE_INLINE_CHAR_LIMIT {
        return format!("{key}: {rendered}");
    }

    let first_line = rendered.lines().next().unwrap_or("").trim_start();
    let preview: String = first_line
        .chars()
        .take(AST_CAPTURE_INLINE_CHAR_LIMIT)
        .collect();
    let location = ast_capture_line_range(match_text, value, match_line)
        .map(|(start, end)| {
            if start == end {
                format!("line {start}")
            } else {
                format!("lines {start}-{end}")
            }
        })
        .unwrap_or_else(|| "in matched excerpt".to_string());

    format!("{key}: {location} — {preview}…")
}

fn ast_capture_line_range(match_text: &str, value: &Value, match_line: u64) -> Option<(u64, u64)> {
    let excerpt = match_text.trim();
    let captures: Vec<&str> = match value {
        Value::String(capture) => vec![capture],
        Value::Array(captures) => captures.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if captures.is_empty() {
        return None;
    }

    let mut cursor = 0;
    let mut range: Option<(u64, u64)> = None;
    for capture in captures {
        let needle = capture.trim();
        if needle.is_empty() {
            continue;
        }
        let found = excerpt
            .get(cursor..)
            .and_then(|remaining| remaining.find(needle).map(|offset| cursor + offset))
            .or_else(|| excerpt.find(needle))?;
        let end = found + needle.len();
        cursor = end;
        let start_line = match_line
            + excerpt[..found]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count() as u64;
        let end_line =
            match_line + excerpt[..end].bytes().filter(|byte| *byte == b'\n').count() as u64;
        range = Some(match range {
            Some((start, previous_end)) => (start.min(start_line), previous_end.max(end_line)),
            None => (start_line, end_line),
        });
    }
    range
}

#[cfg(test)]
mod ast_search_format_tests {
    use super::{format_response_with_context, FormatContext};
    use crate::protocol::Response;
    use serde_json::json;

    #[test]
    fn ast_search_multiline_capture_is_rendered_as_excerpt_reference() {
        let body = "async () => {\n  expect(true).toBe(true);\n}";
        let response = Response::success(
            "ast-search",
            json!({
                "matches": [{
                    "file": "src/test.ts",
                    "line": 12,
                    "text": format!("test('name', {body})"),
                    "meta_variables": {"$BODY": body},
                }],
                "total_matches": 1,
                "files_with_matches": 1,
                "files_searched": 1,
            }),
        );
        let structured_payload = response.data.clone();

        let rendered =
            format_response_with_context("ast_search", &response, &FormatContext::default());

        assert!(rendered.contains("$BODY: lines 12-14 — async () => {…"));
        assert_eq!(rendered.matches("expect(true).toBe(true);").count(), 1);
        assert_eq!(response.data, structured_payload);
    }

    #[test]
    fn ast_search_short_capture_stays_inline() {
        let rendered = super::format_ast_search(&json!({
            "matches": [{
                "file": "src/test.ts",
                "line": 1,
                "text": "test('name', $NAME)",
                "meta_variables": {"$NAME": "foo"},
            }],
            "total_matches": 1,
            "files_with_matches": 1,
            "files_searched": 1,
        }));

        assert!(rendered.contains("$NAME: foo"));
    }
}

fn format_ast_replace(data: &Value, dry_run: bool) -> String {
    let matches = data.get("matches").and_then(Value::as_array);
    let match_count = data
        .get("total_replacements")
        .or_else(|| data.get("total_matches"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| matches.map(|m| m.len() as u64).unwrap_or(0));
    let files_searched = data
        .get("files_searched")
        .or_else(|| data.get("total_files"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let files_with_matches = data
        .get("files_with_matches")
        .or_else(|| data.get("total_files"))
        .and_then(Value::as_u64)
        .unwrap_or(files_searched);

    if data.get("no_files_matched_scope").and_then(Value::as_bool) == Some(true) {
        let mut output =
            "No files matched the scope (paths/globs resolved to zero files)".to_string();
        append_scope_warnings(&mut output, data);
        return output;
    }

    if match_count == 0 {
        let mut output = format!("No matches found (searched {files_searched} files)");
        append_scope_warnings(&mut output, data);
        append_hint(&mut output, data);
        return output;
    }

    let mut output = if dry_run {
        format!(
            "[DRY RUN] Would replace {match_count} match(es) in {files_with_matches} file(s) ({files_searched} searched)\n\n"
        )
    } else {
        format!(
            "Replaced {match_count} match(es) in {files_with_matches} file(s) ({files_searched} searched)\n\n"
        )
    };

    if dry_run {
        if let Some(files) = data.get("files").and_then(Value::as_array) {
            if !files.is_empty() {
                append_ast_replace_dry_run_files(
                    &mut output,
                    files,
                    match_count,
                    files_with_matches,
                );
            }
        }
    } else if let Some(matches) = matches {
        for m in matches {
            let rel_file = m.get("file").and_then(Value::as_str).unwrap_or("unknown");
            let line = m.get("line").and_then(Value::as_u64).unwrap_or(0);
            output.push_str(&format!("{rel_file}:{line}\n"));
            if let (Some(text), Some(replacement)) = (
                m.get("text").and_then(Value::as_str),
                m.get("replacement").and_then(Value::as_str),
            ) {
                output.push_str(&format!("  - {}\n", text.trim()));
                output.push_str(&format!("  + {}\n", replacement.trim()));
            }
            output.push('\n');
        }
    } else if let Some(files) = data.get("files").and_then(Value::as_array) {
        if !files.is_empty() {
            for f in files {
                let rel_file = f.get("file").and_then(Value::as_str).unwrap_or("unknown");
                let replacements = f.get("replacements").and_then(Value::as_u64).unwrap_or(0);
                let suffix = if replacements == 1 { "" } else { "s" };
                output.push_str(&format!(
                    "  {rel_file}: {replacements} replacement{suffix}\n"
                ));
            }
        }
    }

    output
}

fn append_ast_replace_dry_run_files(
    output: &mut String,
    files: &[Value],
    match_count: u64,
    files_with_matches: u64,
) {
    const MAX_DIFF_BYTES: usize = 8 * 1024;
    let mut used = 0usize;
    for (index, f) in files.iter().enumerate() {
        let rel_file = f.get("file").and_then(Value::as_str).unwrap_or("unknown");
        let replacements = f.get("replacements").and_then(Value::as_u64).unwrap_or(0);
        let diff = f.get("diff").and_then(Value::as_str).unwrap_or("");
        if used + diff.len() > MAX_DIFF_BYTES {
            let remaining = files.len().saturating_sub(index);
            if remaining > 0 {
                output.push_str(&format!(
                    "\n... ({remaining} more file(s) omitted from preview to stay under {}KB; total {match_count} replacements across {files_with_matches} files)\n",
                    MAX_DIFF_BYTES / 1024
                ));
            }
            break;
        }
        let suffix = if replacements == 1 { "" } else { "s" };
        output.push_str(&format!(
            "{rel_file} ({replacements} replacement{suffix}):\n"
        ));
        output.push_str(diff);
        if !diff.ends_with('\n') {
            output.push('\n');
        }
        output.push('\n');
        used += diff.len();
    }
}

fn append_scope_warnings(output: &mut String, data: &Value) {
    let warnings = string_array(data.get("scope_warnings"));
    if !warnings.is_empty() {
        output.push_str("\n\nScope warnings:\n");
        output.push_str(
            &warnings
                .iter()
                .map(|warning| format!("  {warning}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
}

fn append_hint(output: &mut String, data: &Value) {
    if let Some(hint) = data
        .get("hint")
        .and_then(Value::as_str)
        .filter(|hint| !hint.is_empty())
    {
        output.push_str("\n\n");
        output.push_str(hint);
    }
}

fn append_ast_skipped_files(output: String, skipped_files: Option<&Value>) -> String {
    let Some(skipped_files) = skipped_files.and_then(Value::as_array) else {
        return output;
    };
    if skipped_files.is_empty() {
        return output;
    }
    let lines = skipped_files
        .iter()
        .map(|skipped| {
            let file = skipped
                .get("file")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let reason = skipped
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown reason");
            format!("  {file}: {reason}")
        })
        .collect::<Vec<_>>();
    format!(
        "{output}\n\nIncomplete: skipped {} file(s)\n{}",
        skipped_files.len(),
        lines.join("\n")
    )
}

fn js_template_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_template_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

// Mirrors packages/opencode-plugin/src/tools/semantic.ts semanticTools.
fn format_search(data: &Value) -> String {
    let envelope = data
        .get("results_list_envelope")
        .and_then(|v| serde_json::from_value::<crate::list_envelope::ListEnvelope>(v.clone()).ok());

    let note = if envelope.is_some() {
        search_status_only_note(data)
    } else {
        extra_honesty_note(data)
    };

    let base = if let Some(text) = data
        .get("text")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        match note {
            Some(n) => format!("{text}\n{n}"),
            None => text.to_string(),
        }
    } else if envelope.is_some() {
        match note {
            Some(n) => format!("No results.\n{n}"),
            None => "No results.".to_string(),
        }
    } else {
        semantic_honesty_note(data).unwrap_or_else(|| "No results.".to_string())
    };

    if let Some(env) = envelope {
        if let Some(trailer) = render_envelope_trailer(&env) {
            if base.is_empty() {
                trailer
            } else {
                format!("{base}\n\n{trailer}")
            }
        } else {
            base
        }
    } else {
        base
    }
}

/// Search-status note for an answer whose results were read from files
/// directly because no index served it (the response's `fully_degraded`).
pub(crate) const FILESYSTEM_SCAN_STATUS: &str = "answered by a filesystem scan";

fn search_status_only_note(data: &Value) -> Option<String> {
    let mut notes = Vec::new();
    if data.get("fully_degraded").and_then(Value::as_bool) == Some(true) {
        notes.push(FILESYSTEM_SCAN_STATUS);
    }
    if data.get("complete").and_then(Value::as_bool) == Some(false) {
        notes.push("partial/incomplete");
    }
    if notes.is_empty() {
        None
    } else {
        Some(format!("Search status: {}.", notes.join("; ")))
    }
}

fn semantic_honesty_note(data: &Value) -> Option<String> {
    let mut notes = Vec::new();
    if data.get("more_available").and_then(Value::as_bool) == Some(true) {
        notes.push("more results available");
    }
    if data.get("engine_capped").and_then(Value::as_bool) == Some(true) {
        notes.push("enumeration capped");
    }
    if data.get("fully_degraded").and_then(Value::as_bool) == Some(true) {
        notes.push(FILESYSTEM_SCAN_STATUS);
    }
    if data.get("complete").and_then(Value::as_bool) == Some(false) {
        notes.push("partial/incomplete");
    }
    if notes.is_empty() {
        None
    } else {
        Some(format!("Search status: {}.", notes.join("; ")))
    }
}

fn extra_honesty_note(data: &Value) -> Option<String> {
    let mut notes = Vec::new();
    if data.get("fully_degraded").and_then(Value::as_bool) == Some(true) {
        notes.push(FILESYSTEM_SCAN_STATUS);
    }
    if data.get("complete").and_then(Value::as_bool) == Some(false) {
        notes.push("partial/incomplete");
    }
    if notes.is_empty() {
        None
    } else {
        Some(format!("Search status: {}.", notes.join("; ")))
    }
}

// Mirrors packages/opencode-plugin/src/tools/reading.ts aft_outline dispatch.
fn format_outline(response: &Response, mode: OutlineMode) -> String {
    match mode {
        OutlineMode::Text => format_outline_text(&response.data),
        OutlineMode::Files | OutlineMode::DirectoryJson => {
            format_outline_files_text(&response.data)
        }
    }
}

// Mirrors packages/opencode-plugin/src/tools/reading.ts formatOutlineFilesText.
fn format_outline_files_text(data: &Value) -> String {
    let text = format_outline_text(data);
    // Structure maps already carry their file-budget/walk trailer. Appending
    // the legacy partial-result footer would bury that trailer and duplicate
    // the incomplete-discovery warning.
    if data
        .get("structure_footer")
        .and_then(Value::as_str)
        .is_some_and(|footer| !footer.is_empty())
    {
        return text;
    }
    let envelope = data
        .get("files_list_envelope")
        .and_then(|v| serde_json::from_value::<crate::list_envelope::ListEnvelope>(v.clone()).ok());

    if let Some(env) = envelope {
        if let Some(trailer) = render_envelope_trailer(&env) {
            if text.is_empty() {
                return trailer;
            } else {
                return format!("{text}\n\n{trailer}");
            }
        }
        return text;
    }

    let unchecked: Vec<String> = data
        .get("unchecked_files")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let is_partial = data.get("complete").and_then(Value::as_bool) == Some(false)
        || data.get("walk_truncated").and_then(Value::as_bool) == Some(true)
        || !unchecked.is_empty();

    if !is_partial {
        return text;
    }

    let mut footer = Vec::new();
    if data.get("walk_truncated").and_then(Value::as_bool) == Some(true) {
        let suffix = if !unchecked.is_empty() {
            format!(
                " {} additional files in this directory were not indexed.",
                unchecked.len()
            )
        } else {
            " Some files in this directory were not indexed.".to_string()
        };
        let walk_limit = data
            .get("walk_limit")
            .and_then(Value::as_u64)
            .unwrap_or(200);
        footer.push(format!(
            "⚠ Partial result: walk truncated at {walk_limit} files.{suffix}"
        ));
    } else {
        let suffix = if !unchecked.is_empty() {
            format!(
                " {} files in this directory were not indexed.",
                unchecked.len()
            )
        } else {
            " Some files in this directory were not indexed.".to_string()
        };
        footer.push(format!("⚠ Partial result:{suffix}"));
    }

    if !unchecked.is_empty() {
        footer.push("Unchecked files:".to_string());
        for file in unchecked.iter().take(MAX_UNCHECKED_FILES_IN_FOOTER) {
            footer.push(format!("  {file}"));
        }
        let remaining = unchecked
            .len()
            .saturating_sub(MAX_UNCHECKED_FILES_IN_FOOTER);
        if remaining > 0 {
            footer.push(format!("  ... +{remaining} more"));
        }
    }

    if text.is_empty() {
        footer.join("\n")
    } else {
        format!("{text}\n\n{}", footer.join("\n"))
    }
}

fn format_outline_text(data: &Value) -> String {
    let text = data.get("text").and_then(Value::as_str).unwrap_or("");
    if let Some(footer) = data
        .get("structure_footer")
        .and_then(Value::as_str)
        .filter(|footer| !footer.is_empty())
    {
        if let Some(body) = text.strip_suffix(&format!("\n{footer}")) {
            let mut without_footer = data.clone();
            without_footer["text"] = Value::String(body.to_string());
            without_footer["structure_footer"] = Value::Null;
            // Genuine skipped-file gaps belong before the final list trailer.
            return format!("{}\n{footer}", format_outline_text(&without_footer));
        }
    }
    let skipped = data.get("skipped_files").and_then(Value::as_array);
    let Some(skipped) = skipped.filter(|s| !s.is_empty()) else {
        return text.to_string();
    };

    // A directory outline discovers its own files; an explicit `files` request
    // does not. The command says which it was, rather than the renderer
    // inferring it from some other field's presence.
    //
    // That distinction decides how much a skip is worth saying. If the caller
    // named a file, "I could not parse it" answers their request and belongs in
    // the reply. If we walked a repository, every .gitignore, LICENSE and
    // lockfile is an unsupported language, and naming each one spends the
    // caller's context to tell them something they never asked about: 27 such
    // lines prompted this change. Those are counted instead, while genuine gaps
    // — unreadable, unparseable, vanished — keep their names in both modes,
    // because those are the ones that change what a reader does next.
    let discovered = data
        .get("discovered")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut named: Vec<String> = Vec::new();
    let mut unsupported = 0_usize;
    for item in skipped {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let Some(file) = obj.get("file").and_then(Value::as_str) else {
            continue;
        };
        let reason = obj
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("skipped");
        if discovered && reason == "unsupported_language" {
            unsupported += 1;
            continue;
        }
        named.push(format!("  {file} — {reason}"));
    }

    let mut sections: Vec<String> = Vec::new();
    if !named.is_empty() {
        sections.push(format!(
            "Skipped {} file(s):\n{}",
            named.len(),
            named.join("\n")
        ));
    }
    if unsupported > 0 {
        sections.push(format!(
            "Skipped {unsupported} file(s) with no supported language."
        ));
    }
    if sections.is_empty() {
        return text.to_string();
    }
    let header = if text.is_empty() { "" } else { "\n\n" };
    format!("{text}{header}{}", sections.join("\n"))
}

// Format zoom responses as plain text so direct calls and server-side calls
// produce identical output.
fn format_zoom(data: &Value, ctx: &FormatContext) -> String {
    if let Some(text) = data.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(entries) = data.get("targets").and_then(Value::as_array) {
        return format_zoom_multi_target_result(entries);
    }

    let target_label = ctx.zoom_target_label.as_deref().unwrap_or("(no target)");
    if let Some((names, responses)) = unwrap_rust_zoom_batch_envelope(data) {
        return format_zoom_batch_result(target_label, &names, &responses);
    }
    format_zoom_text(target_label, data)
}

fn format_zoom_multi_target_result(entries: &[Value]) -> String {
    let rendered = entries
        .iter()
        .map(|entry| {
            let target_label = entry
                .get("targetLabel")
                .and_then(Value::as_str)
                .filter(|label| !label.is_empty())
                .unwrap_or("(no target)");
            let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
            let response = entry.get("response");
            if response
                .and_then(|response| response.get("success"))
                .and_then(Value::as_bool)
                == Some(false)
            {
                let message = response
                    .and_then(|response| response.get("message"))
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                    .unwrap_or("zoom failed");
                return (
                    false,
                    format!("Symbol \"{name}\" not found in {target_label}: {message}"),
                );
            }
            match response {
                Some(response) => (true, format_zoom_text(target_label, response)),
                None => (
                    false,
                    format!("Symbol \"{name}\" not found in {target_label}: missing zoom response"),
                ),
            }
        })
        .collect::<Vec<_>>();

    let complete = rendered.iter().all(|(success, _)| *success);
    let mut sections = Vec::new();
    if !complete {
        sections.push("Incomplete zoom results: one or more symbols failed.".to_string());
    }
    sections.extend(rendered.into_iter().map(|(_, content)| content));
    sections.join("\n\n")
}

fn unwrap_rust_zoom_batch_envelope(data: &Value) -> Option<(Vec<String>, Vec<Value>)> {
    let symbols = data.get("symbols")?.as_array()?;
    if symbols.is_empty() {
        return None;
    }

    let mut names = Vec::with_capacity(symbols.len());
    let mut responses = Vec::with_capacity(symbols.len());
    for entry in symbols {
        let row = entry.as_object()?;
        let name = row.get("name")?.as_str()?;
        let response = row.get("response")?;
        if response.is_null() {
            return None;
        }
        names.push(name.to_string());
        responses.push(response.clone());
    }
    Some((names, responses))
}

fn format_zoom_batch_result(target_label: &str, symbols: &[String], responses: &[Value]) -> String {
    let entries = symbols
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let response = responses.get(index);
            if response
                .and_then(|r| r.get("success"))
                .and_then(Value::as_bool)
                == Some(false)
            {
                let message = response
                    .and_then(|r| r.get("message"))
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                    .unwrap_or("zoom failed");
                return (false, format!("Symbol \"{name}\" not found: {message}"));
            }
            match response {
                Some(response) => (true, format_zoom_text(target_label, response)),
                None => (
                    false,
                    format!("Symbol \"{name}\" not found: missing zoom response"),
                ),
            }
        })
        .collect::<Vec<_>>();

    let complete = entries.iter().all(|(success, _)| *success);
    let mut sections = Vec::new();
    if !complete {
        sections.push("Incomplete zoom results: one or more symbols failed.".to_string());
    }
    sections.extend(entries.into_iter().map(|(_, content)| content));
    sections.join("\n\n")
}

const ZOOM_CALL_ANNOTATION_LIMIT: usize = 15;

fn format_zoom_text(target_label: &str, response: &Value) -> String {
    let range = response.get("range");
    let start_line = range
        .and_then(|range| range.get("start_line"))
        .and_then(Value::as_i64)
        .unwrap_or(1);
    let end_line = range
        .and_then(|range| range.get("end_line"))
        .and_then(Value::as_i64)
        .unwrap_or(start_line);
    let kind = response
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("symbol");
    let name = response.get("name").and_then(Value::as_str).unwrap_or("");
    let content_text = response
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("");
    let context_before = string_array(response.get("context_before"));
    let context_after = string_array(response.get("context_after"));

    let header = if kind == "lines" {
        format!("{target_label}:{start_line}-{end_line}")
    } else {
        format!("{target_label}:{start_line}-{end_line} [{kind} {name}]")
            .trim_end()
            .to_string()
    };

    let mut content_lines = content_text.split('\n').collect::<Vec<_>>();
    if content_lines.last() == Some(&"") {
        content_lines.pop();
    }

    let last_displayed_line = end_line + context_after.len() as i64;
    let gutter_width = last_displayed_line.max(1).to_string().len();
    let mut out = vec![header, String::new()];

    let mut line_no = start_line - context_before.len() as i64;
    for text in &context_before {
        out.push(format_zoom_line(line_no, gutter_width, text));
        line_no += 1;
    }
    for text in content_lines {
        out.push(format_zoom_line(line_no, gutter_width, text));
        line_no += 1;
    }
    for text in &context_after {
        out.push(format_zoom_line(line_no, gutter_width, text));
        line_no += 1;
    }

    let annotations = response.get("annotations");
    let calls_out = annotations
        .and_then(|annotations| annotations.get("calls_out"))
        .and_then(Value::as_array);
    let other_calls = annotations
        .and_then(|annotations| annotations.get("other_calls"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if calls_out.is_some_and(|calls| !calls.is_empty()) || other_calls > 0 {
        if let Some(calls_out) = calls_out.filter(|calls| !calls.is_empty()) {
            append_zoom_annotation_calls(&mut out, "calls_out", calls_out);
        } else {
            out.push(String::new());
            out.push("──── calls_out".to_string());
        }
        if other_calls > 0 {
            out.push(format!("  +{other_calls} other calls"));
        }
    }

    let called_by = annotations
        .and_then(|annotations| annotations.get("called_by"))
        .and_then(Value::as_array);
    if let Some(called_by) = called_by.filter(|calls| !calls.is_empty()) {
        append_zoom_annotation_calls(&mut out, "called_by (this file)", called_by);
    }

    out.join("\n")
}

fn append_zoom_annotation_calls(out: &mut Vec<String>, section: &str, calls: &[Value]) {
    out.push(String::new());
    out.push(format!("──── {section}"));
    for call in calls.iter().take(ZOOM_CALL_ANNOTATION_LIMIT) {
        out.push(format_zoom_call_ref(call));
    }
    if calls.len() > ZOOM_CALL_ANNOTATION_LIMIT {
        out.push(format!(
            "  +{} more",
            calls.len() - ZOOM_CALL_ANNOTATION_LIMIT
        ));
    }
}

fn format_zoom_line(line_no: i64, gutter_width: usize, text: &str) -> String {
    format!("{line_no:>gutter_width$}: {text}")
}

fn format_zoom_call_ref(call: &Value) -> String {
    let name = call.get("name").and_then(Value::as_str).unwrap_or("");
    let line = call.get("line").and_then(Value::as_i64).unwrap_or(0);
    let extra = call
        .get("extra_count")
        .and_then(Value::as_i64)
        .filter(|count| *count > 0)
        .map(|count| format!(" +{count}"))
        .unwrap_or_default();
    format!("  {name} (line {line}){extra}")
}

#[cfg(test)]
mod zoom_other_calls_tests {
    use super::*;

    #[test]
    fn zoom_target_labels_accept_batch_and_quiet_aliases() {
        for key in ["target", "path", "filePath", "url"] {
            assert_eq!(
                zoom_target_label_for_call(
                    "zoom",
                    &serde_json::json!({key: "https://example.test/docs"})
                ),
                Some("https://example.test/docs".to_string())
            );
        }
        assert_eq!(
            zoom_target_label_for_call(
                "zoom",
                &serde_json::json!({"batch": [{"target": "src/main.rs", "symbols": "main"}]})
            ),
            Some("src/main.rs".to_string())
        );
        assert_eq!(
            zoom_target_label_for_call(
                "zoom",
                &serde_json::json!({"batch": [{"target": "a.rs"}, {"target": "b.rs"}]})
            ),
            None
        );
    }

    #[test]
    fn zoom_formats_other_calls_once_with_or_without_followable_calls() {
        for calls in [
            serde_json::json!([]),
            serde_json::json!([{ "name": "lock", "line": 2 }]),
        ] {
            let response = serde_json::json!({
                "name": "A", "kind": "function", "content": "body",
                "annotations": { "calls_out": calls, "other_calls": 3 }
            });
            let text = format_zoom_text("fixture.ts", &response);
            let expected_calls = if calls.as_array().unwrap().is_empty() {
                ""
            } else {
                "  lock (line 2)\n"
            };
            assert_eq!(text, format!("fixture.ts:1-1 [function A]\n\n1: body\n\n──── calls_out\n{expected_calls}  +3 other calls"));
        }
    }
}

// Mirrors packages/opencode-plugin/src/tools/inspect.ts inspectTools.
fn format_inspect(response: &Response) -> String {
    if let Some(text) = format_inspect_terminal(&response.data) {
        return text;
    }
    let body = if let Some(text) = response.data.get("text").and_then(Value::as_str) {
        if response.data["inspect_terminal"] == "partial" {
            text.to_string()
        } else {
            append_rendered_diagnostics(text, &response.data)
        }
    } else {
        let json =
            serialized_text_or_failure(serde_json::to_string_pretty(response), "inspect response");
        append_rendered_diagnostics(&json, &response.data)
    };
    match inspect_partial_header(&response.data) {
        Some(header) if !body.starts_with("PARTIAL — ") => format!("{header}\n{body}"),
        _ if response.data["inspect_terminal"] == "fresh" && !body.starts_with("FRESH\n") => {
            format!("FRESH\n{body}")
        }
        Some(_) => body,
        None => body,
    }
}

#[cfg(test)]
pub(crate) fn format_inspect_for_test(response: &Response) -> String {
    format_inspect(response)
}

/// The server owns the one-line status: any unfinished category is partial.
/// Plugins pass this rendered header through without adding another one.
fn inspect_partial_header(data: &Value) -> Option<String> {
    if data.get("inspect_terminal").and_then(Value::as_str) != Some("partial") {
        return None;
    }
    let reason = data
        .get("partial_reason")
        .and_then(Value::as_str)
        .unwrap_or("analysis incomplete; retry aft_inspect.");
    Some(format!("PARTIAL — {reason}"))
}

// Mirrors packages/opencode-plugin/src/tools/inspect.ts appendRenderedDiagnostics.
fn append_rendered_diagnostics(text: &str, data: &Value) -> String {
    if text.starts_with("PARTIAL — ") {
        return text.to_string();
    }
    if text.lines().any(|line| {
        let lower = line.to_lowercase();
        lower.starts_with("diagnostics:") || lower.starts_with("diagnostics ")
    }) {
        return text.to_string();
    }
    let diagnostics = render_inspect_diagnostics(data);
    if diagnostics.is_empty() {
        return text.to_string();
    }
    if text.is_empty() {
        diagnostics
    } else {
        format!("{text}\n\n{diagnostics}")
    }
}

fn render_inspect_diagnostics(data: &Value) -> String {
    let mut lines = Vec::new();
    if let Some(summary_line) = format_diagnostics_summary(data.get("summary")) {
        lines.push(summary_line);
    }

    let detail_lines = format_diagnostics_details(data.get("details"));
    if !detail_lines.is_empty() {
        let provisional = data
            .get("summary")
            .and_then(|summary| summary.get("diagnostics"))
            .is_some_and(|section| {
                section.get("status").and_then(Value::as_str) == Some("pending")
                    || section.get("status").and_then(Value::as_str) == Some("incomplete")
                    || section.get("provisional_counts").is_some()
            });
        lines.push(if provisional {
            "diagnostics details (provisional — analyzer not ready; counts excluded from E/W):"
                .to_string()
        } else {
            "diagnostics details:".to_string()
        });
        for line in detail_lines {
            lines.push(format!("- {line}"));
        }
    }

    lines.join("\n")
}

pub(crate) fn format_diagnostics_summary(summary: Option<&Value>) -> Option<String> {
    format_diagnostics_summary_with(summary, false)
}

/// `format_diagnostics_summary` for a body that has already printed every
/// non-file gap with its full reason (the inspect text does, on its
/// "Incomplete diagnostics: producer ... failed (...)" lines). There the
/// summary names each such producer and points at that line instead of
/// repeating a reason that can be a multi-line cargo error.
pub(crate) fn format_diagnostics_summary_with(
    summary: Option<&Value>,
    gap_reasons_rendered_above: bool,
) -> Option<String> {
    let section = summary?.get("diagnostics")?.as_object()?;
    if section.get("complete").and_then(Value::as_bool) == Some(false) {
        let all_gaps = section
            .get("gaps")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        // Scoped files without a report are counted, not named: a scope can
        // hold hundreds of them, and inspect lists their causes and a bounded
        // sample of paths on separate lines.
        let uncovered_files = all_gaps
            .iter()
            .filter(|gap| gap.get("kind").and_then(Value::as_str) == Some("uncovered_file"))
            .count();
        let mut gaps = all_gaps
            .iter()
            .filter(|gap| gap.get("kind").and_then(Value::as_str) != Some("uncovered_file"))
            .map(|gap| {
                let producer =
                    gap.get("producer").and_then(Value::as_str).map(|producer| {
                        match gap.get("root").and_then(Value::as_str) {
                            Some(root) => format!("{producer} @ {root}"),
                            None => producer.to_string(),
                        }
                    });
                let reason = gap
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unavailable");
                // A `checking_producer` gap is a server whose `cargo check`
                // is still running, not a failure; its reason already names
                // the server, so it is shown as it is.
                if gap.get("kind").and_then(Value::as_str) == Some("checking_producer") {
                    return reason.to_string();
                }
                if let (true, Some(producer)) = (gap_reasons_rendered_above, &producer) {
                    return format!("producer {producer} failed, reason above");
                }
                format!(
                    "{}: {reason}",
                    producer
                        .as_deref()
                        .or_else(|| gap.get("file").and_then(Value::as_str))
                        .unwrap_or("unknown producer"),
                )
            })
            .collect::<Vec<_>>();
        if uncovered_files > 0 {
            let files = if uncovered_files == 1 {
                "file"
            } else {
                "files"
            };
            gaps.push(format!(
                "{uncovered_files} {files} without an authoritative report"
            ));
        }
        let mut text = "diagnostics: unknown".to_string();
        if !gaps.is_empty() {
            text.push_str(&format!(" ({})", gaps.join("; ")));
        }
        if let Some(producers) = section.get("by_producer").and_then(Value::as_object) {
            for (producer, counts) in producers {
                if let (Some(errors), Some(warnings), Some(info), Some(hints)) = (
                    counts.get("errors").and_then(Value::as_u64),
                    counts.get("warnings").and_then(Value::as_u64),
                    counts.get("info").and_then(Value::as_u64),
                    counts.get("hints").and_then(Value::as_u64),
                ) {
                    text.push_str(&format!("; {errors} errors, {warnings} warnings, {info} info, {hints} hints from {producer}"));
                }
            }
        }
        return Some(text);
    }
    let errors = section.get("errors").and_then(Value::as_u64);
    let warnings = section.get("warnings").and_then(Value::as_u64);
    let info = section.get("info").and_then(Value::as_u64);
    let hints = section.get("hints").and_then(Value::as_u64);
    let has_counts = [errors, warnings, info, hints].iter().any(|v| v.is_some());
    let counts = format!(
        "{} errors, {} warnings, {} info, {} hints",
        errors.unwrap_or(0),
        warnings.unwrap_or(0),
        info.unwrap_or(0),
        hints.unwrap_or(0)
    );
    let status = section.get("status").and_then(Value::as_str);
    let provisional_counts = section.get("provisional_counts").and_then(Value::as_object);
    let provisional_text = provisional_counts.map(|counts| {
        format!(
            " ({} errors, {} warnings, {} info, {} hints)",
            counts.get("errors").and_then(Value::as_u64).unwrap_or(0),
            counts.get("warnings").and_then(Value::as_u64).unwrap_or(0),
            counts.get("info").and_then(Value::as_u64).unwrap_or(0),
            counts.get("hints").and_then(Value::as_u64).unwrap_or(0),
        )
    });
    let provisional_framing = || {
        format!(
            "provisional — analyzer not ready; counts excluded from E/W{}",
            provisional_text.as_deref().unwrap_or("")
        )
    };

    match status {
        Some("pending") => Some(format!(
            "diagnostics: {} — still pending (servers: {}); wait for the LSP update and use the next normal aft_inspect, not repeated polling",
            provisional_framing(),
            diagnostics_server_summary(section)
        )),
        Some("incomplete") => Some(format!(
            "diagnostics: {} (incomplete — servers: {})",
            provisional_framing(),
            diagnostics_server_summary(section)
        )),
        _ if provisional_counts.is_some() => Some(format!(
            "diagnostics: {}",
            provisional_framing()
        )),
        _ => {
            if has_counts {
                Some(format!("diagnostics: {counts}"))
            } else {
                None
            }
        }
    }
}

fn diagnostics_server_summary(section: &serde_json::Map<String, Value>) -> String {
    let pending = string_array(section.get("servers_pending"));
    let not_installed = string_array(section.get("servers_not_installed"));
    let mut parts = Vec::new();
    if !pending.is_empty() {
        parts.push(format!("pending: {}", pending.join(", ")));
    }
    if !not_installed.is_empty() {
        parts.push(format!("not installed: {}", not_installed.join(", ")));
    }
    if parts.is_empty() {
        "none reported".to_string()
    } else {
        parts.join("; ")
    }
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn format_diagnostics_details(details: Option<&Value>) -> Vec<String> {
    let Some(details) = details.and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(diagnostics) = details.get("diagnostics").and_then(Value::as_array) else {
        return Vec::new();
    };
    diagnostics
        .iter()
        .filter_map(|item| {
            let d = item.as_object()?;
            let severity = d
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("information");
            let message = d
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(no message)");
            let source = d.get("source").and_then(Value::as_str);
            let suffix = source.map(|s| format!(" [{s}]")).unwrap_or_default();
            Some(format!(
                "{} {} {}{}",
                format_diagnostic_location(d),
                severity,
                message,
                suffix
            ))
        })
        .collect()
}

fn format_diagnostic_location(d: &serde_json::Map<String, Value>) -> String {
    let file = d
        .get("file")
        .and_then(Value::as_str)
        .unwrap_or("(unknown file)");
    let line = d.get("line").and_then(Value::as_u64);
    let column = d.get("column").and_then(Value::as_u64);
    match (line, column) {
        (None, _) => file.to_string(),
        (Some(line), None) => format!("{file}:{line}"),
        (Some(line), Some(col)) => format!("{file}:{line}:{col}"),
    }
}

const UNRESOLVED_SUMMARY_NAME_LIMIT: usize = 10;

pub fn format_callgraph(op: &str, response_data: &Value, include_unresolved: bool) -> String {
    let Some(record) = response_data.as_object() else {
        return "No navigation result.".to_string();
    };
    if let Some(field) = missing_callgraph_collection(op, record) {
        return format!("Unable to format {op}: response omitted required `{field}` collection.");
    }

    let mut sections = match op {
        "call_tree" => format_call_tree_sections(record, include_unresolved),
        "callers" => format_callers_sections(record),
        "trace_to_symbol" => format_trace_to_symbol_sections(record),
        "trace_to" => format_trace_to_sections(record),
        "impact" => format_impact_sections(record),
        _ => format_trace_data_sections(record),
    };
    if matches!(op, "callers" | "impact") {
        if let Some(reason) = string_field(record, "incomplete_reason") {
            sections.insert(0, reason.to_string());
        }
        if let Some(note) = string_field(record, "test_filter_note") {
            sections.insert(0, note.to_string());
        }
    }
    let body = sections.join("\n");
    // A checkout that reads another checkout's callgraph gets a one-line
    // notice that the callers and line numbers below may not match its own
    // files, followed, when the answer depends on files the borrowed graph
    // does not reflect, by a line saying which and what to grep. The notices
    // go first rather than last because a list cut short must end with its
    // "shown N of M" line.
    let notice = |field: &str| {
        record
            .get(field)
            .and_then(Value::as_object)
            .and_then(|notice| string_field(notice, "message"))
    };
    let mut lines: Vec<&str> = ["borrowed_callgraph", "borrowed_coverage"]
        .into_iter()
        .filter_map(notice)
        .collect();
    if lines.is_empty() {
        return body;
    }
    lines.push(&body);
    lines.join("\n")
}

fn missing_callgraph_collection(
    op: &str,
    record: &serde_json::Map<String, Value>,
) -> Option<&'static str> {
    let required = match op {
        "call_tree" => "children",
        "callers" => "callers",
        "trace_to_symbol" => "path",
        "trace_to" => "paths",
        "impact" => "callers",
        _ => "hops",
    };
    let Some(top_level) = record.get(required).and_then(Value::as_array) else {
        // trace_to_symbol answers "no path" with `path: null` and a `reason`
        // (no_path_found, max_depth_exhausted); that is the contract, not an
        // omitted collection. Only a response with neither is malformed.
        if op == "trace_to_symbol"
            && record.get("path").is_none_or(Value::is_null)
            && record.get("reason").and_then(Value::as_str).is_some()
        {
            return None;
        }
        return Some(required);
    };
    if op == "call_tree" && call_tree_descendant_missing_children(top_level) {
        return Some("children");
    }
    let nested = match op {
        "callers" => Some("callers"),
        "trace_to" => Some("hops"),
        _ => None,
    };
    if let Some(nested) = nested {
        let missing_nested = top_level.iter().any(|value| {
            value
                .as_object()
                .and_then(|record| record.get(nested))
                .and_then(Value::as_array)
                .is_none()
        });
        if missing_nested {
            return Some(nested);
        }
    }
    None
}

fn call_tree_descendant_missing_children(children: &[Value]) -> bool {
    children.iter().any(|child| {
        let Some(child) = child.as_object() else {
            return true;
        };
        let Some(grandchildren) = child.get("children").and_then(Value::as_array) else {
            return true;
        };
        call_tree_descendant_missing_children(grandchildren)
    })
}

fn format_callgraph_error(command: &str, data: &Value) -> String {
    let code = data
        .get("code")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("callgraph failed");

    if matches!(
        code,
        Some("ambiguous_target") | Some("target_symbol_not_in_file")
    ) {
        let candidates = callgraph_candidates(data);
        if !candidates.is_empty() {
            let symbol =
                callgraph_error_symbol(data).or_else(|| symbol_from_callgraph_message(message));
            let target = symbol
                .map(|symbol| format!("multiple symbols named \"{symbol}\""))
                .unwrap_or_else(|| strip_terminal_punctuation(message));
            let action = if code == Some("ambiguous_target") {
                "Pass toPath to disambiguate"
            } else {
                "Try one of these files for toPath"
            };
            let mut lines = vec![format!(
                "{command}: {} — {target}. {action}:",
                code.unwrap_or_default()
            )];
            lines.extend(
                candidates
                    .into_iter()
                    .map(|candidate| format!("  - {candidate}")),
            );
            return lines.join("\n");
        }
    }

    let Some(code) = code else {
        return message.to_string();
    };
    let mut lines = vec![format!("{command}: {code} — {message}")];
    if let Some(extras) = collect_callgraph_error_extras(data) {
        lines.push(format!("data: {extras}"));
    }
    lines.join("\n")
}

fn callgraph_candidates(data: &Value) -> Vec<String> {
    data.get("candidates")
        .and_then(Value::as_array)
        .or_else(|| {
            data.get("data")
                .and_then(Value::as_object)
                .and_then(|nested| nested.get("candidates"))
                .and_then(Value::as_array)
        })
        .map(|items| {
            items
                .iter()
                .filter_map(|candidate| {
                    let candidate = candidate.as_object()?;
                    let file = string_field(candidate, "file")?;
                    let line = number_field(candidate, "line");
                    Some(match line {
                        Some(line) => format!("{file}:{line}"),
                        None => file.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn callgraph_error_symbol(data: &Value) -> Option<String> {
    data.get("symbol")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            data.get("data")
                .and_then(Value::as_object)
                .and_then(|nested| nested.get("symbol"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string)
}

fn symbol_from_callgraph_message(message: &str) -> Option<String> {
    extract_between(message, "target symbol '", "'")
        .or_else(|| extract_between(message, "multiple symbols named \"", "\""))
}

fn extract_between(message: &str, prefix: &str, suffix: &str) -> Option<String> {
    let start = message.find(prefix)? + prefix.len();
    let rest = &message[start..];
    let end = rest.find(suffix)?;
    let value = &rest[..end];
    (!value.is_empty()).then(|| value.to_string())
}

fn strip_terminal_punctuation(message: &str) -> String {
    message.trim_end_matches(['.', '!', '?']).to_string()
}

fn collect_callgraph_error_extras(data: &Value) -> Option<String> {
    let obj = data.as_object()?;
    let mut extras = serde_json::Map::new();
    for (key, value) in obj {
        if matches!(
            key.as_str(),
            "id" | "success" | "code" | "message" | "data" | "status_bar" | "bg_completions"
        ) {
            continue;
        }
        extras.insert(key.clone(), value.clone());
    }
    if extras.is_empty() {
        data.get("data").map(stringify_json_pretty)
    } else {
        if let Some(nested) = data.get("data") {
            extras.insert("data".to_string(), nested.clone());
        }
        Some(stringify_json_pretty(&Value::Object(extras)))
    }
}

fn stringify_json_pretty(value: &Value) -> String {
    serialized_text_or_failure(serde_json::to_string_pretty(value), "JSON value")
}

fn serialized_text_or_failure<E: std::fmt::Display>(
    serialized: Result<String, E>,
    subject: &str,
) -> String {
    match serialized {
        Ok(text) => text,
        Err(error) => format!("Unable to format {subject}: serialization failed: {error}"),
    }
}

fn format_call_tree_sections(
    record: &serde_json::Map<String, Value>,
    include_unresolved: bool,
) -> Vec<String> {
    let envelope = record
        .get("tree_list_envelope")
        .and_then(|v| serde_json::from_value::<crate::list_envelope::ListEnvelope>(v.clone()).ok());

    let mut lines = Vec::new();
    render_call_tree_node(record, 0, &mut lines, include_unresolved);
    if let Some(gap) = string_field(record, "gap") {
        lines.push(gap.to_string());
    }
    let hidden_test_callers = number_field(record, "hidden_test_callers").unwrap_or(0);
    if hidden_test_callers > 0 {
        lines.push(format!(
            "{hidden_test_callers} callers in tests hidden — includeTests: true shows them"
        ));
    }
    let is_envelope_governed =
        crate::list_surfaces::find_surface("callgraph", "call_tree", "payload.tree").is_some();
    let truncated = number_field(record, "truncated").unwrap_or(0);
    let warning = if record.contains_key("gap")
        || envelope.is_some()
        || (is_envelope_governed && truncated == 0)
    {
        String::new()
    } else {
        depth_warning(record, "depth_limited", "truncated")
    };
    if !warning.is_empty() {
        lines.push(warning);
    }
    if lines.is_empty() {
        vec!["No call tree available.".to_string()]
    } else {
        if let Some(env) = envelope {
            if let Some(trailer) = render_envelope_trailer(&env) {
                lines.push(String::new());
                lines.push(trailer);
            }
        }
        lines
    }
}

fn render_call_tree_node(
    node: &serde_json::Map<String, Value>,
    depth: usize,
    lines: &mut Vec<String>,
    include_unresolved: bool,
) {
    let name = string_field(node, "name").unwrap_or("(unknown)");
    let file = shorten_path(string_field(node, "file").unwrap_or("(unknown file)"));
    let line = number_field(node, "line");
    let unresolved = if node.get("resolved").and_then(Value::as_bool) == Some(false) {
        " [unresolved]"
    } else {
        ""
    };
    let name_match = name_match_edge_marker(node);
    let location = match line {
        Some(line) => format!("[{file}:{line}]"),
        None => format!("[{file}]"),
    };
    lines.push(tree_line(
        depth,
        &format!("{name} {location}{unresolved}{name_match}"),
    ));

    let children = records_field(node, "children");
    if include_unresolved {
        for child in children {
            render_call_tree_node(child, depth + 1, lines, include_unresolved);
        }
        return;
    }

    let unresolved_indices = children
        .iter()
        .enumerate()
        .filter_map(|(index, child)| is_unresolved_leaf(child).then_some(index))
        .collect::<Vec<_>>();
    if unresolved_indices.is_empty() {
        for child in children {
            render_call_tree_node(child, depth + 1, lines, include_unresolved);
        }
        return;
    }

    let mut summary_inserted = false;
    for (index, child) in children.iter().enumerate() {
        if unresolved_indices.contains(&index) {
            if !summary_inserted {
                let unresolved_leaves = unresolved_indices
                    .iter()
                    .filter_map(|idx| children.get(*idx).copied())
                    .collect::<Vec<_>>();
                lines.push(tree_line(
                    depth + 1,
                    &unresolved_summary_text(&unresolved_leaves),
                ));
                summary_inserted = true;
            }
            continue;
        }
        render_call_tree_node(child, depth + 1, lines, include_unresolved);
    }
}

fn is_unresolved_leaf(node: &serde_json::Map<String, Value>) -> bool {
    node.get("resolved").and_then(Value::as_bool) == Some(false)
        && records_field(node, "children").is_empty()
}

fn unresolved_summary_text(nodes: &[&serde_json::Map<String, Value>]) -> String {
    let mut distinct_names = Vec::new();
    for node in nodes {
        let name = string_field(node, "name").unwrap_or("(unknown)");
        if !distinct_names.iter().any(|seen| seen == name) {
            distinct_names.push(name.to_string());
        }
    }

    let displayed = distinct_names
        .iter()
        .take(UNRESOLVED_SUMMARY_NAME_LIMIT)
        .cloned()
        .collect::<Vec<_>>();
    let hidden = distinct_names.len().saturating_sub(displayed.len());
    let names = if hidden > 0 {
        format!("{}, … (+{hidden} more)", displayed.join(", "))
    } else {
        displayed.join(", ")
    };
    let noun = if nodes.len() == 1 { "call" } else { "calls" };
    format!("+ {} unresolved external {noun}: {names}", nodes.len())
}

fn callback_reference_count_suffix(record: &serde_json::Map<String, Value>) -> String {
    let count = number_field(record, "callback_dependencies").unwrap_or(0);
    if count > 0 {
        format!(" ({count} by callback reference)")
    } else {
        String::new()
    }
}

fn format_callers_sections(record: &serde_json::Map<String, Value>) -> Vec<String> {
    let envelope = record
        .get("callers_list_envelope")
        .and_then(|v| serde_json::from_value::<crate::list_envelope::ListEnvelope>(v.clone()).ok());

    let groups = records_field(record, "callers");
    let hidden_test_callers = number_field(record, "hidden_test_callers").unwrap_or(0);
    let is_envelope_governed =
        crate::list_surfaces::find_surface("callgraph", "callers", "payload.callers").is_some();
    let truncated = number_field(record, "truncated").unwrap_or(0);
    let warning = if envelope.is_some() || (is_envelope_governed && truncated == 0) {
        String::new()
    } else {
        depth_warning(record, "depth_limited", "truncated")
    };
    let hub_summary = if envelope.is_some() {
        None
    } else {
        hub_summary_line(record)
    };
    let total_str = if let Some(env) = &envelope {
        crate::list_envelope::render_total(&env.total)
    } else {
        let total = number_field(record, "total_callers").unwrap_or(0);
        format!("{}", total.saturating_sub(hidden_test_callers))
    };
    let callback_note = callback_reference_count_suffix(record);
    let mut sections = vec![join_non_empty(&[
        Some(format!(
            "{total_str} caller{}{callback_note}",
            if total_str == "1" { "" } else { "s" }
        )),
        Some(format!(
            "{} file group{}",
            groups.len(),
            if groups.len() == 1 { "" } else { "s" }
        )),
        (!warning.is_empty()).then_some(warning),
    ])];
    if let Some(summary) = hub_summary {
        sections.push(summary);
    }
    for group in groups {
        sections.push(render_callers_group_lines(group).join("\n"));
    }
    if hidden_test_callers > 0 {
        sections.push(format!(
            "{hidden_test_callers} callers in tests hidden — includeTests: true shows them"
        ));
    }
    if let Some(note) = record
        .get("macro_note")
        .and_then(Value::as_object)
        .and_then(|note| string_field(note, "message"))
    {
        sections.push(note.to_string());
    }
    if let Some(env) = envelope {
        if let Some(trailer) = render_envelope_trailer(&env) {
            sections.push(String::new());
            sections.push(trailer);
        }
    }
    sections
}

fn render_callers_group_lines(group: &serde_json::Map<String, Value>) -> Vec<String> {
    let file = shorten_path(string_field(group, "file").unwrap_or("(unknown file)"));
    let mut lines = vec![file];
    let callers = records_field(group, "callers");
    let mut by_symbol_provenance: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for caller in callers {
        let symbol = string_field(caller, "symbol").unwrap_or("(unknown)");
        let provenance = if string_field(caller, "resolved_by") == Some("name_match") {
            "name_match"
        } else {
            "exact"
        };
        let via = string_field(caller, "via").unwrap_or("");
        let relation = string_field(caller, "relation").unwrap_or("");
        let key = format!("{symbol}\0{via}\0{relation}\0{provenance}");
        let bucket = by_symbol_provenance.entry(key).or_default();
        if let Some(line) = number_field(caller, "line") {
            bucket.push(line);
        }
    }
    for (key, mut line_nums) in by_symbol_provenance {
        let mut parts = key.split('\0');
        let symbol = parts.next().unwrap_or("(unknown)");
        let via = parts.next().unwrap_or("");
        let relation = parts.next().unwrap_or("");
        let is_name_match = key.ends_with("\0name_match");
        line_nums.sort_unstable();
        let line_part = if line_nums.is_empty() {
            "?".to_string()
        } else {
            line_nums
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let marker = if is_name_match { " ~" } else { "" };
        let via_part = if via.is_empty() {
            String::new()
        } else {
            format!(" via {via}")
        };
        let reference = if relation == "callback_reference" {
            crate::commands::callgraph_store_adapter::CALLBACK_REFERENCE_LABEL
        } else {
            ""
        };
        lines.push(format!(
            "  ↳ {symbol}:{line_part}{marker}{via_part}{reference}"
        ));
    }
    lines
}

fn format_trace_to_symbol_sections(record: &serde_json::Map<String, Value>) -> Vec<String> {
    let path = records_field(record, "path");
    let complete = record.get("complete").and_then(Value::as_bool);
    let reason = string_field(record, "reason");
    if path.is_empty() {
        let prefix = if complete == Some(false) {
            "No complete path"
        } else {
            "No path"
        };
        return vec![match reason {
            Some(reason) => format!("{prefix} ({reason})"),
            None => prefix.to_string(),
        }];
    }

    let mut lines = vec![format!(
        "{} hop{}",
        path.len(),
        if path.len() == 1 { "" } else { "s" }
    )];
    for (index, hop) in path.iter().enumerate() {
        let symbol = string_field(hop, "symbol").unwrap_or("(unknown)");
        let file = shorten_path(string_field(hop, "file").unwrap_or("(unknown file)"));
        let line = number_field(hop, "line");
        let name_match = name_match_edge_marker(hop);
        let location = match line {
            Some(line) => format!("[{file}:{line}]"),
            None => format!("[{file}]"),
        };
        lines.push(tree_line(
            index + 1,
            &format!("{symbol} {location}{name_match}"),
        ));
    }
    lines
}

fn format_trace_to_sections(record: &serde_json::Map<String, Value>) -> Vec<String> {
    let envelope = record
        .get("paths_list_envelope")
        .and_then(|v| serde_json::from_value::<crate::list_envelope::ListEnvelope>(v.clone()).ok());

    let paths = records_field(record, "paths");
    let warning = if envelope.is_some() {
        String::new()
    } else {
        depth_warning(record, "max_depth_reached", "truncated_paths")
    };
    let hub_summary = if envelope.is_some() {
        None
    } else {
        hub_summary_line(record)
    };
    let total_paths = number_field(record, "total_paths").unwrap_or(paths.len() as i64);
    let total_paths_is_lower_bound = record
        .get("total_paths_is_lower_bound")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let total_paths_str = if let Some(env) = &envelope {
        crate::list_envelope::render_total(&env.total)
    } else {
        let prefix = if total_paths_is_lower_bound {
            "at least "
        } else {
            ""
        };
        format!("{prefix}{total_paths}")
    };
    let entry_points = number_field(record, "entry_points_found").unwrap_or(0);
    let mut sections = vec![join_non_empty(&[
        Some(format!(
            "{total_paths_str} path{}",
            if total_paths_str == "1" { "" } else { "s" }
        )),
        Some(format!(
            "{entry_points} entry point{}",
            if entry_points == 1 { "" } else { "s" }
        )),
        (!warning.is_empty()).then_some(warning),
    ])];
    if let Some(summary) = hub_summary {
        sections.push(summary);
    }
    if paths.is_empty() {
        sections.push("No entry paths found.".to_string());
    }
    for (index, path) in paths.iter().enumerate() {
        let mut lines = Vec::new();
        render_trace_path(path, index, &mut lines);
        sections.push(lines.join("\n"));
    }
    if let Some(env) = envelope {
        if let Some(trailer) = render_envelope_trailer(&env) {
            sections.push(trailer);
        }
    }
    sections
}

fn render_trace_path(path: &serde_json::Map<String, Value>, index: usize, lines: &mut Vec<String>) {
    lines.push(format!("Path {}", index + 1));
    for (hop_index, hop) in records_field(path, "hops").iter().enumerate() {
        let symbol = string_field(hop, "symbol").unwrap_or("(unknown)");
        let file = shorten_path(string_field(hop, "file").unwrap_or("(unknown file)"));
        let line = number_field(hop, "line");
        let entry = if hop.get("is_entry_point").and_then(Value::as_bool) == Some(true) {
            " [entry]"
        } else {
            ""
        };
        let name_match = name_match_edge_marker(hop);
        let location = match line {
            Some(line) => format!("[{file}:{line}]"),
            None => format!("[{file}]"),
        };
        lines.push(tree_line(
            hop_index + 1,
            &format!("{symbol}{entry} {location}{name_match}"),
        ));
    }
}

fn format_impact_sections(record: &serde_json::Map<String, Value>) -> Vec<String> {
    let envelope = record
        .get("sites_list_envelope")
        .or_else(|| record.get("callers_list_envelope"))
        .and_then(|v| serde_json::from_value::<crate::list_envelope::ListEnvelope>(v.clone()).ok());

    let callers = records_field(record, "callers");
    let hidden_test_callers = number_field(record, "hidden_test_callers").unwrap_or(0);
    let is_envelope_governed =
        crate::list_surfaces::find_surface("callgraph", "impact", "payload.sites").is_some();
    let truncated = number_field(record, "truncated").unwrap_or(0);
    let warning = if envelope.is_some() || (is_envelope_governed && truncated == 0) {
        String::new()
    } else {
        depth_warning(record, "depth_limited", "truncated")
    };
    let hub_summary = if envelope.is_some() {
        None
    } else {
        hub_summary_line(record)
    };
    let total_str = if let Some(env) = &envelope {
        crate::list_envelope::render_total(&env.total)
    } else {
        let total_affected = number_field(record, "total_affected").unwrap_or(callers.len() as i64);
        format!("{}", total_affected.saturating_sub(hidden_test_callers))
    };
    let affected_files = number_field(record, "affected_files").unwrap_or(0);
    let callback_note = callback_reference_count_suffix(record);
    let mut sections = vec![join_non_empty(&[
        Some(format!(
            "{total_str} affected {}{}{callback_note}",
            if callback_note.is_empty() {
                "call site"
            } else {
                "site"
            },
            if total_str == "1" { "" } else { "s" }
        )),
        Some(format!(
            "{affected_files} file{}",
            if affected_files == 1 { "" } else { "s" }
        )),
        (!warning.is_empty()).then_some(warning),
    ])];
    if let Some(summary) = hub_summary {
        sections.push(summary);
    }
    if callers.is_empty() {
        sections.push("No impacted callers found.".to_string());
    }
    for caller in callers {
        let file = shorten_path(string_field(caller, "caller_file").unwrap_or("(unknown file)"));
        let symbol = string_field(caller, "caller_symbol").unwrap_or("(unknown)");
        let line = number_field(caller, "line").unwrap_or(0);
        let entry = if caller.get("is_entry_point").and_then(Value::as_bool) == Some(true) {
            " [entry]"
        } else {
            ""
        };
        let name_match = name_match_edge_marker(caller);
        let expression = string_field(caller, "call_expression");
        let params = caller
            .get("parameters")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(value_to_plain_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let mut lines = vec![
            format!("{file}:{line}"),
            format!(
                "  ↳ {symbol}{entry}{name_match}{}",
                if string_field(caller, "relation") == Some("callback_reference") {
                    crate::commands::callgraph_store_adapter::CALLBACK_REFERENCE_LABEL
                } else {
                    ""
                }
            ),
        ];
        if let Some(expression) = expression {
            lines.push(format!("  {expression}"));
        }
        if !params.is_empty() {
            lines.push(format!("  params: {params}"));
        }
        sections.push(lines.join("\n"));
    }
    if hidden_test_callers > 0 {
        sections.push(format!(
            "{hidden_test_callers} callers in tests hidden — includeTests: true shows them"
        ));
    }
    if let Some(env) = envelope {
        if let Some(trailer) = render_envelope_trailer(&env) {
            sections.push(String::new());
            sections.push(trailer);
        }
    }
    sections
}

fn format_trace_data_sections(record: &serde_json::Map<String, Value>) -> Vec<String> {
    let envelope = record
        .get("hops_list_envelope")
        .and_then(|v| serde_json::from_value::<crate::list_envelope::ListEnvelope>(v.clone()).ok());

    let hops = records_field(record, "hops");
    let depth_warning_text = if envelope.is_some() {
        None
    } else {
        (record.get("depth_limited").and_then(Value::as_bool) == Some(true))
            .then_some("(depth limited)".to_string())
    };
    let mut sections = vec![join_non_empty(&[
        Some(format!(
            "{} hop{}",
            hops.len(),
            if hops.len() == 1 { "" } else { "s" }
        )),
        depth_warning_text,
    ])];
    if hops.is_empty() {
        sections.push("No data-flow hops found.".to_string());
    }
    for (index, hop) in hops.iter().enumerate() {
        let file = shorten_path(string_field(hop, "file").unwrap_or("(unknown file)"));
        let symbol = string_field(hop, "symbol").unwrap_or("(unknown)");
        let variable = string_field(hop, "variable").unwrap_or("(unknown)");
        let line = number_field(hop, "line").unwrap_or(0);
        let approximate = if hop.get("approximate").and_then(Value::as_bool) == Some(true) {
            " [approx]"
        } else {
            ""
        };
        let name_match = name_match_edge_marker(hop);
        let flow_type = string_field(hop, "flow_type").unwrap_or("flow");
        sections.push(tree_line(
            index,
            &format!("{variable} {flow_type} {symbol} [{file}:{line}]{approximate}{name_match}"),
        ));
    }
    if let Some(env) = envelope {
        if let Some(trailer) = render_envelope_trailer(&env) {
            sections.push(trailer);
        }
    }
    sections
}

fn records_field<'a>(
    record: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Vec<&'a serde_json::Map<String, Value>> {
    record
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default()
}

fn string_field<'a>(record: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    record.get(key).and_then(Value::as_str)
}

fn number_field(record: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    let value = record.get(key)?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
}

fn shorten_path(path: &str) -> String {
    let Some(home) = home_dir() else {
        return path.to_string();
    };
    let home = home.to_string_lossy().to_string();
    if path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    }
}

fn home_dir() -> Option<PathBuf> {
    crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from)
}

fn tree_line(depth: usize, text: &str) -> String {
    format!(
        "{}{}{}",
        "  ".repeat(depth),
        if depth == 0 { "" } else { "↳ " },
        text
    )
}

fn name_match_edge_marker(record: &serde_json::Map<String, Value>) -> &'static str {
    if string_field(record, "resolved_by") == Some("name_match") {
        " ~"
    } else {
        ""
    }
}

fn depth_warning(
    response: &serde_json::Map<String, Value>,
    depth_field: &str,
    truncated_field: &str,
) -> String {
    let limited = response.get(depth_field).and_then(Value::as_bool);
    let truncated = number_field(response, truncated_field).unwrap_or(0);
    if limited != Some(true) && truncated == 0 {
        return String::new();
    }
    let detail = if truncated > 0 {
        format!(", {truncated} truncated")
    } else {
        String::new()
    };
    format!("(depth limited{detail})")
}

fn hub_summary_line(response: &serde_json::Map<String, Value>) -> Option<String> {
    response
        .get("hub_summary")
        .and_then(Value::as_object)
        .and_then(|summary| string_field(summary, "message"))
        .map(str::to_string)
}

/// Shared list-envelope rendering funnel for subcommand and search output.
pub(crate) fn render_envelope_trailer(
    envelope: &crate::list_envelope::ListEnvelope,
) -> Option<String> {
    crate::list_envelope::render_trailer(envelope)
}

/// Count of rendered match rows in a grep response (R24).
///
/// `matches` holds the whole requested page; `rendered_matches` says how many
/// of those rows fit the output byte budget and were printed. Replies without
/// that field printed every row in `matches`.
pub fn rendered_grep_match_count(data: &Value) -> usize {
    let matches_len = data
        .get("matches")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    data.get("rendered_matches")
        .and_then(Value::as_u64)
        .map(|rendered| (rendered as usize).min(matches_len))
        .unwrap_or(matches_len)
}

/// Count of rendered file rows in a glob response after applying display-only selectors (R24).
pub fn rendered_glob_file_count(data: &Value) -> usize {
    let files = data.get("files").and_then(Value::as_array);
    let Some(files) = files else {
        return 0;
    };
    const MAX_FLAT_FILES: usize = 20;
    const MAX_DISPLAY_DIRECTORIES: usize = 6;
    const MAX_DISPLAY_FILES_PER_DIRECTORY: usize = 5;

    if files.len() <= MAX_FLAT_FILES {
        return files.len();
    }
    let mut dir_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut dir_order: Vec<String> = Vec::new();
    for f in files {
        let path_str = f.as_str().unwrap_or("");
        let dir = std::path::Path::new(path_str)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if !dir_counts.contains_key(&dir) {
            dir_order.push(dir.clone());
        }
        *dir_counts.entry(dir).or_default() += 1;
    }
    dir_order
        .iter()
        .take(MAX_DISPLAY_DIRECTORIES)
        .map(|d| dir_counts[d].min(MAX_DISPLAY_FILES_PER_DIRECTORY))
        .sum()
}

/// Generic rendered-row count reporting for list surfaces (R24).
pub fn report_rendered_row_count(command: &str, data: &Value) -> Option<usize> {
    match command {
        "grep" => Some(rendered_grep_match_count(data)),
        "glob" => Some(rendered_glob_file_count(data)),
        _ => None,
    }
}

fn join_non_empty(parts: &[Option<String>]) -> String {
    parts
        .iter()
        .filter_map(|part| part.as_deref())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn value_to_plain_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

// Status has no TypeScript wrapper; preserve the pretty JSON summary and add
// one deliberately thin human-readable view of the raw memory section.
fn format_status(data: &Value) -> String {
    if let Some(text) = data
        .get("text")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return text.to_string();
    }

    // Compact human-readable summary. The full snapshot stays available to
    // first-party consumers via structuredContent; the agent-facing text was
    // previously a pretty-printed JSON dump that reached six figures of
    // characters on fleet-scale daemons.
    let mut lines = Vec::new();
    let version = data.get("version").and_then(Value::as_str).unwrap_or("?");
    let root = data
        .get("project_root")
        .and_then(Value::as_str)
        .unwrap_or("?");
    lines.push(format!("AFT {version} — {root}"));

    // Worktree / read-only borrow is a shared-index arrangement, not a
    // degraded mode. Render it with the same vocabulary the TUI uses so
    // agents do not read cache_role=worktree as "AFT is broken here".
    if let Some(role) = data.get("cache_role").and_then(Value::as_str) {
        if let Some(line) = format_cache_role_line(role) {
            lines.push(line);
        }
    }

    if data.get("degraded").and_then(Value::as_bool) == Some(true) {
        let reasons = data
            .get("degraded_reasons")
            .and_then(Value::as_array)
            .map(|reasons| {
                reasons
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unspecified".to_string());
        lines.push(format!("DEGRADED: {reasons}"));
    }

    let search = status_field(data, "search_index", "status");
    let semantic = {
        let state = status_field(data, "semantic_index", "status");
        let stage = data
            .pointer("/semantic_index/stage")
            .and_then(Value::as_str);
        let model = data
            .pointer("/semantic_index/model")
            .and_then(Value::as_str);
        let mut s = state;
        if let Some(stage) = stage {
            s = format!("{s} ({stage})");
        }
        if let Some(model) = model {
            s = format!("{s} [{model}]");
        }
        s
    };
    let callgraph = data
        .pointer("/features/callgraph_store")
        .and_then(Value::as_bool)
        .map(|on| if on { "enabled" } else { "disabled" })
        .unwrap_or("?");
    lines.push(format!(
        "indexes: search {search} | semantic {semantic} | callgraph {callgraph}"
    ));

    if let Some(features) = data.get("features").and_then(Value::as_object) {
        let flags = features
            .iter()
            .map(|(name, value)| match value {
                Value::Bool(true) => format!("{name} on"),
                Value::Bool(false) => format!("{name} off"),
                other => format!("{name} {}", value_as_display(other)),
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("features: {flags}"));
    }

    if let Some(disk) = data.get("disk").and_then(Value::as_object) {
        let storage = disk
            .get("storage_dir")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let trigram = format_optional_memory_bytes(disk.get("trigram_disk_bytes"));
        let semantic_disk = format_optional_memory_bytes(disk.get("semantic_disk_bytes"));
        lines.push(format!(
            "storage: {storage} (trigram {trigram}, semantic {semantic_disk})"
        ));
    }

    let tracked = data
        .pointer("/session/tracked_files")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let checkpoints = data
        .pointer("/session/checkpoints")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let lsp = data.get("lsp_servers").and_then(Value::as_u64).unwrap_or(0);
    lines.push(format!(
        "session: {tracked} tracked file(s), {checkpoints} checkpoint(s) | lsp servers: {lsp}"
    ));

    if let Some(memory) = data.get("memory") {
        lines.push(String::new());
        lines.push(format_memory_block(memory));
    }

    lines.join("\n")
}

fn format_cache_role_line(role: &str) -> Option<String> {
    match role {
        "worktree" => Some("cache: shared repo index (built by the main checkout)".to_string()),
        "read_only" => Some("cache: sharing the repo index family (read-only borrow)".to_string()),
        _ => None,
    }
}

fn status_field(data: &Value, section: &str, key: &str) -> String {
    data.pointer(&format!("/{section}/{key}"))
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string()
}

fn value_as_display(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn format_memory_block(memory: &Value) -> String {
    let process = memory.get("process").unwrap_or(&Value::Null);
    let rss = format_optional_memory_bytes(process.get("rss_bytes"));
    let attributed = format_optional_memory_bytes(process.get("total_attributed_bytes"));
    let unattributed = format_optional_memory_bytes(process.get("unattributed_bytes"));
    let mut lines = vec![format!(
        "Memory: RSS {rss} | attributed {attributed} | unattributed {unattributed}"
    )];
    if let Some(roots) = memory.get("roots").and_then(Value::as_object) {
        for (root, estimate) in roots {
            let total = format_optional_memory_bytes(estimate.get("attributed_bytes"));
            let subsystems = [
                ("semantic", "semantic"),
                ("trigram", "trigram"),
                ("symbols", "symbols"),
                ("callgraph", "callgraph"),
                ("inspect", "inspect"),
                ("bash", "bash"),
                ("lsp", "lsp"),
                ("parser_pool", "parsers"),
            ]
            .iter()
            .map(|(key, label)| {
                let subsystem = estimate.get(*key).unwrap_or(&Value::Null);
                let value = if subsystem.get("status").and_then(Value::as_str) == Some("busy") {
                    "busy".to_string()
                } else {
                    format_optional_memory_bytes(subsystem.get("estimated_bytes"))
                };
                format!("{label} {value}")
            })
            .collect::<Vec<_>>()
            .join(", ");
            lines.push(format!("  {root}: {total} ({subsystems})"));
        }
    }
    lines.join("\n")
}

fn format_optional_memory_bytes(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return "not estimated".to_string();
    };
    let (sign, magnitude) = if let Some(bytes) = value.as_i64() {
        (if bytes < 0 { "-" } else { "" }, bytes.unsigned_abs())
    } else if let Some(bytes) = value.as_u64() {
        ("", bytes)
    } else {
        return "not estimated".to_string();
    };
    let magnitude = magnitude as f64;
    if magnitude >= 1024.0 * 1024.0 {
        format!("{sign}{:.1} MiB", magnitude / (1024.0 * 1024.0))
    } else if magnitude >= 1024.0 {
        format!("{sign}{:.1} KiB", magnitude / 1024.0)
    } else {
        format!("{sign}{} B", magnitude as u64)
    }
}

fn format_inspect_terminal(data: &Value) -> Option<String> {
    let kind = data
        .get("inspect_terminal")
        .and_then(Value::as_str)
        .map(|kind| kind.to_ascii_lowercase().replace('_', "-"))?;
    match kind.as_str() {
        "phase-failed" => {
            let detail = compact_inspect_terminal_field(data.get("failure_detail"), "not supplied");
            let reason = compact_inspect_terminal_field(data.get("failure_reason"), "not supplied");
            Some(format!(
                "PHASE-FAILED — inspect could not complete: {detail} ({reason}). Retry aft_inspect, or narrow the scope."
            ))
        }
        "interrupted" => {
            Some(
                "INTERRUPTED — inspect stopped before it could complete; no fresh snapshot was produced. Retry aft_inspect, or narrow the scope."
                    .to_string()
            )
        }
        _ => None,
    }
}

fn compact_inspect_terminal_field(value: Option<&Value>, fallback: &str) -> String {
    let Some(value) = value.and_then(Value::as_str) else {
        return fallback.to_string();
    };
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if parts.is_empty() {
        fallback.to_string()
    } else {
        let mut compact = parts.join(" ");
        for (internal, plain) in [
            ("lsp_start", "starting language servers"),
            ("lsp_quiescence", "waiting for language servers"),
            ("stat_verification", "verifying files are unchanged"),
            ("callgraph_ready", "preparing call analysis"),
            ("tier2_rescan", "running code analysis"),
        ] {
            compact = compact.replace(internal, plain);
        }
        compact
    }
}

#[cfg(test)]
mod move_format_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn source_delete_failed_renders_partially_moved_not_moved() {
        let ctx = FormatContext {
            move_file_arg: Some("a.ts".into()),
            move_dest_arg: Some("b.ts".into()),
            ..Default::default()
        };
        let rendered = format_move(
            &json!({
                "file": "/repo/src/a.ts",
                "destination": "/repo/src/b.ts",
                "moved": true,
                "complete": false,
                "source_delete_failed": true,
                "warning": "destination was written, but source file could not be deleted after copy: permission denied. Both paths now exist; retry deleting the source or accept the duplicate."
            }),
            &ctx,
        );

        assert!(
            rendered.starts_with("Partially moved a.ts → b.ts"),
            "expected partial move header:\n{rendered}"
        );
        assert!(
            rendered.contains("source deletion failed: permission denied"),
            "expected extracted delete error:\n{rendered}"
        );
        assert!(
            rendered.contains("Both paths exist"),
            "expected both-paths guidance:\n{rendered}"
        );
        assert!(
            !rendered.starts_with("Moved "),
            "must not look like a finished move:\n{rendered}"
        );
        assert!(
            !rendered.contains("Moved a.ts → b.ts"),
            "finished-move phrasing must be absent:\n{rendered}"
        );
    }

    #[test]
    fn successful_move_still_renders_moved() {
        let ctx = FormatContext {
            move_file_arg: Some("a.ts".into()),
            move_dest_arg: Some("b.ts".into()),
            ..Default::default()
        };
        let rendered = format_move(
            &json!({
                "file": "/repo/src/a.ts",
                "destination": "/repo/src/b.ts",
                "moved": true
            }),
            &ctx,
        );
        assert_eq!(rendered, "Moved a.ts → b.ts");
    }
}

#[cfg(test)]
mod status_memory_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_text_renders_compact_memory_block() {
        let data = json!({
            "version": "test",
            "memory": {
                "process": {
                    "rss_bytes": 8 * 1024 * 1024,
                    "total_attributed_bytes": 3 * 1024 * 1024,
                    "unattributed_bytes": 5 * 1024 * 1024
                },
                "roots": {
                    "/repo": {
                        "attributed_bytes": 3 * 1024 * 1024,
                        "semantic": {"status": "ready", "estimated_bytes": 2 * 1024 * 1024},
                        "trigram": {"status": "ready", "estimated_bytes": 1024 * 1024},
                        "symbols": {"status": "ready", "estimated_bytes": 0},
                        "callgraph": {"status": "ready", "estimated_bytes": null},
                        "inspect": {"status": "ready", "estimated_bytes": 0},
                        "bash": {"status": "ready", "estimated_bytes": 0},
                        "lsp": {"status": "ready", "estimated_bytes": 0},
                        "parser_pool": {"status": "ready", "estimated_bytes": null}
                    }
                }
            }
        });
        let rendered = format_status(&data);
        assert!(
            rendered.contains("Memory: RSS 8.0 MiB | attributed 3.0 MiB | unattributed 5.0 MiB")
        );
        assert!(rendered.contains("/repo: 3.0 MiB (semantic 2.0 MiB, trigram 1.0 MiB"));
        assert!(!rendered.contains("\"memory\""));
    }

    #[test]
    fn status_text_renders_worktree_as_shared_index_not_degraded() {
        let data = json!({
            "version": "test",
            "project_root": "/repo",
            "cache_role": "worktree",
            "degraded": false,
            "degraded_reasons": []
        });
        let rendered = format_status(&data);
        assert!(rendered.contains("cache: shared repo index (built by the main checkout)"));
        assert!(
            !rendered.to_lowercase().contains("degraded"),
            "worktree borrow must not be labeled degraded: {rendered}"
        );
    }

    #[test]
    fn status_text_renders_read_only_as_shared_index_borrow() {
        let data = json!({
            "version": "test",
            "project_root": "/repo",
            "cache_role": "read_only",
            "degraded": false
        });
        let rendered = format_status(&data);
        assert!(rendered.contains("cache: sharing the repo index family (read-only borrow)"));
        assert!(!rendered.to_lowercase().contains("degraded"));
    }

    #[test]
    fn status_text_keeps_real_degraded_reasons_alongside_worktree_borrow() {
        let data = json!({
            "version": "test",
            "project_root": "/repo",
            "cache_role": "worktree",
            "degraded": true,
            "degraded_reasons": ["home_root"]
        });
        let rendered = format_status(&data);
        assert!(rendered.contains("cache: shared repo index (built by the main checkout)"));
        assert!(rendered.contains("DEGRADED: home_root"));
    }
}

#[cfg(test)]
mod safety_checkpoint_format_tests {
    use super::*;
    use serde_json::json;

    fn render(op: &str, data: Value) -> String {
        let ctx = FormatContext::from_tool_call(
            "safety",
            &json!({ "op": op, "name": "cp" }),
            Path::new("/repo"),
        );
        let response = Response {
            id: "1".to_string(),
            success: true,
            data,
        };
        format_response_with_context("safety", &response, &ctx)
    }

    #[test]
    fn checkpoint_lists_snapshotted_files_and_counts_against_the_request() {
        let text = render(
            "checkpoint",
            json!({
                "name": "cp",
                "file_count": 1,
                "requested_count": 2,
                "paths": ["/repo/a.txt"],
                "skipped": [{ "file": "/repo/gone.txt", "error": "No such file" }],
            }),
        );
        assert_eq!(
            text,
            "checkpoint created cp\nfiles 1 of 2 requested\n  ↳ /repo/a.txt\nskipped\n  ↳ /repo/gone.txt: No such file"
        );
    }

    #[test]
    fn empty_tracked_file_checkpoint_says_nothing_was_snapshotted() {
        let text = render(
            "checkpoint",
            json!({ "name": "cp", "file_count": 0, "paths": [] }),
        );
        assert!(
            text.starts_with("checkpoint created cp (empty)\nfiles 0\nNothing was snapshotted"),
            "{text}"
        );
    }

    #[test]
    fn restore_lists_written_files_and_marks_those_already_in_place() {
        let text = render(
            "restore",
            json!({
                "name": "cp",
                "file_count": 2,
                "changed_count": 1,
                "paths": ["/repo/a.txt", "/repo/b.txt"],
                "unchanged": ["/repo/b.txt"],
            }),
        );
        assert_eq!(
            text,
            "checkpoint restored cp\nfiles 2 (1 already matched the checkpoint)\n  ↳ /repo/a.txt\n  ↳ /repo/b.txt (unchanged)"
        );
    }

    #[test]
    fn restore_that_changed_nothing_does_not_read_as_restored() {
        let text = render(
            "restore",
            json!({
                "name": "cp",
                "file_count": 1,
                "changed_count": 0,
                "paths": ["/repo/a.txt"],
                "unchanged": ["/repo/a.txt"],
            }),
        );
        assert!(!text.contains("checkpoint restored"), "{text}");
        assert!(
            text.starts_with(
                "checkpoint cp: nothing to restore\nfiles 1, all already matched the checkpoint; none changed"
            ),
            "{text}"
        );
    }

    #[test]
    fn restore_of_an_empty_checkpoint_does_not_read_as_restored() {
        let text = render(
            "restore",
            json!({ "name": "cp", "file_count": 0, "paths": [], "unchanged": [] }),
        );
        assert!(!text.contains("checkpoint restored"), "{text}");
        assert!(
            text.starts_with("checkpoint cp holds no files; nothing was restored"),
            "{text}"
        );
    }
}

#[cfg(test)]
mod callgraph_format_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serialization_failure_is_explicit_instead_of_substituting_an_empty_object() {
        assert_eq!(
            serialized_text_or_failure::<&str>(Err("forced failure"), "test response"),
            "Unable to format test response: serialization failed: forced failure"
        );
    }

    #[test]
    fn missing_patch_and_move_facts_are_reported_instead_of_substituted() {
        assert_eq!(
            format_apply_patch(&json!({ "metadata": { "files": [{ "type": "update" }] } })),
            "Unable to format apply_patch: a file omitted its path."
        );
        assert_eq!(
            format_move(
                &json!({ "destination": "dest.txt" }),
                &FormatContext::default()
            ),
            "Unable to format move: response omitted the source path."
        );
    }

    #[test]
    fn missing_callgraph_collection_is_reported_instead_of_rendered_as_empty() {
        assert_eq!(
            format_callgraph("trace_to", &json!({ "total_paths": 3 }), false),
            "Unable to format trace_to: response omitted required `paths` collection."
        );
        assert_eq!(
            format_callgraph(
                "trace_to",
                &json!({ "paths": [{ "name": "entry" }] }),
                false
            ),
            "Unable to format trace_to: response omitted required `hops` collection."
        );
    }

    /// `path: null` plus a reason is trace_to_symbol's no-path answer, not an
    /// omitted collection; a response with neither is still malformed.
    #[test]
    fn trace_to_symbol_no_path_with_reason_renders_not_refuses() {
        assert_eq!(
            format_callgraph(
                "trace_to_symbol",
                &json!({ "path": null, "complete": true, "reason": "no_path_found" }),
                false
            ),
            "No path (no_path_found)"
        );
        assert_eq!(
            format_callgraph(
                "trace_to_symbol",
                &json!({ "complete": false, "reason": "max_depth_exhausted" }),
                false
            ),
            "No complete path (max_depth_exhausted)"
        );
        assert_eq!(
            format_callgraph("trace_to_symbol", &json!({ "complete": true }), false),
            "Unable to format trace_to_symbol: response omitted required `path` collection."
        );
    }

    #[test]
    fn trace_to_formats_budgeted_path_count_as_lower_bound() {
        let rendered = format_callgraph(
            "trace_to",
            &json!({
                "total_paths": 7,
                "total_paths_is_lower_bound": true,
                "entry_points_found": 2,
                "hub_summary": {
                    "message": "Next: at least 7 paths — showing 2; traversal capped; narrow with scope"
                },
                "paths": []
            }),
            false,
        );

        assert!(rendered.starts_with("at least 7 paths · 2 entry points"));
        assert!(rendered.contains("Next: at least 7 paths"));
    }

    #[test]
    fn trace_to_exact_path_count_format_is_unchanged() {
        let rendered = format_callgraph(
            "trace_to",
            &json!({
                "total_paths": 1,
                "entry_points_found": 1,
                "paths": []
            }),
            false,
        );

        assert!(rendered.starts_with("1 path · 1 entry point"));
        assert!(!rendered.contains("at least"));
    }

    #[test]
    fn borrowed_callgraph_disclosure_is_the_first_line_and_absent_otherwise() {
        let plain = json!({ "total_callers": 0, "callers": [] });
        let rendered = format_callgraph("callers", &plain, false);
        assert_eq!(rendered, "0 callers · 0 file groups");

        let mut borrowed = plain.clone();
        borrowed["complete"] = json!(false);
        borrowed["borrowed_callgraph"] = json!({ "message": "callgraph: borrowed from /owner" });
        assert_eq!(
            format_callgraph("callers", &borrowed, false),
            "callgraph: borrowed from /owner\n0 callers · 0 file groups"
        );

        // The note about files the borrowed graph cannot see follows the
        // disclosure, ahead of the answer.
        borrowed["borrowed_coverage"] =
            json!({ "message": "callgraph: the borrowed graph does not reflect 2 files here" });
        assert_eq!(
            format_callgraph("callers", &borrowed, false),
            "callgraph: borrowed from /owner\ncallgraph: the borrowed graph does not reflect 2 files here\n0 callers · 0 file groups"
        );
    }
}

#[cfg(test)]
mod zoom_call_annotation_cap_tests {
    use super::format_zoom_text;
    use serde_json::json;

    #[test]
    fn zoom_call_annotation_lists_keep_fifteen_entries_and_count_the_rest() {
        let calls = |prefix: &str, count: usize| {
            (1..=count)
                .map(|line| json!({ "name": format!("{prefix}{line:02}"), "line": line }))
                .collect::<Vec<_>>()
        };
        let response = json!({
            "kind": "function",
            "name": "target",
            "range": { "start_line": 1, "end_line": 1 },
            "content": "fn target() {}",
            "annotations": {
                "calls_out": calls("callee", 17),
                "called_by": calls("caller", 18),
            },
        });

        let rendered = format_zoom_text("src/lib.rs", &response);

        assert!(rendered.contains("callee01 (line 1)"));
        assert!(rendered.contains("callee15 (line 15)"));
        assert!(!rendered.contains("callee16"));
        assert!(rendered.contains("  +2 more"));
        assert!(rendered.contains("caller01 (line 1)"));
        assert!(rendered.contains("caller15 (line 15)"));
        assert!(!rendered.contains("caller16"));
        assert!(rendered.contains("  +3 more"));
        assert!(rendered.contains("──── called_by (this file)"));
        assert!(
            rendered.find("──── calls_out").unwrap() < rendered.find("──── called_by").unwrap()
        );
    }
}

#[cfg(test)]
mod outline_format_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn directory_outline_preserves_walk_truncation_footer() {
        let response = Response::success(
            "1",
            json!({
                "text": "src/\n  a.rs (rs)",
                "complete": false,
                "walk_truncated": true
            }),
        );

        let formatted = format_outline(&response, OutlineMode::DirectoryJson);
        assert!(formatted.contains("src/\n  a.rs (rs)"));
        assert!(formatted.contains("⚠ Partial result: walk truncated at 200 files. Some files in this directory were not indexed."));
    }

    #[test]
    fn structure_outline_trailer_stays_last_after_skips() {
        let footer = "shown 1 of ≥200 files (walk) · narrow: path";
        let response = Response::success(
            "1",
            json!({
                "text": format!("src/\n  a.rs\n\n{footer}"),
                "structure_footer": footer,
                "complete": false,
                "walk_truncated": true,
                "discovered": true,
                "skipped_files": [{"file":"src/b.rs", "reason":"parse_error"}],
            }),
        );
        for mode in [OutlineMode::Text, OutlineMode::DirectoryJson] {
            let formatted = format_outline(&response, mode);
            assert!(formatted.contains("src/b.rs — parse_error"), "{formatted}");
            assert!(formatted.ends_with(footer), "{formatted}");
            assert_eq!(formatted.matches(footer).count(), 1, "{formatted}");
            assert!(!formatted.contains("⚠ Partial result"), "{formatted}");
        }
    }

    #[test]
    fn files_outline_uses_the_counting_walk_limit_in_partial_footer() {
        let response = Response::success(
            "1",
            json!({
                "text": "src/  10000 files",
                "complete": false,
                "walk_truncated": true,
                "walk_limit": 10_000
            }),
        );

        let formatted = format_outline(&response, OutlineMode::Files);
        assert!(formatted.contains("walk truncated at 10000 files"));
        assert!(!formatted.contains("walk truncated at 200 files"));
    }

    /// A repository walk finds many files no parser claims — ignore files,
    /// licences, lockfiles. Naming each one spends the reader's context on
    /// something they never asked about, so a discovered outline counts them.
    #[test]
    fn directory_outline_counts_unsupported_files_instead_of_naming_them() {
        let data = serde_json::json!({
            "text": "src/\n  lib.rs",
            "discovered": true,
            "skipped_files": [
                { "file": ".gitignore", "reason": "unsupported_language" },
                { "file": "LICENSE", "reason": "unsupported_language" },
                { "file": "src/broken.rs", "reason": "parse_error" },
            ],
        });

        let formatted = format_outline_text(&data);

        assert!(formatted.contains("Skipped 2 file(s) with no supported language."));
        assert!(!formatted.contains(".gitignore"));
        assert!(!formatted.contains("LICENSE"));
        // A real gap still earns its name in the same reply.
        assert!(formatted.contains("src/broken.rs — parse_error"));
        assert!(formatted.contains("Skipped 1 file(s):"));
    }

    /// When the caller named the files, "I could not parse it" answers the
    /// request they made and has to survive.
    #[test]
    fn requested_outline_still_names_every_skipped_file() {
        let data = serde_json::json!({
            "text": "",
            "skipped_files": [
                { "file": "notes.txt", "reason": "unsupported_language" },
            ],
        });

        let formatted = format_outline_text(&data);

        assert!(formatted.contains("notes.txt — unsupported_language"));
        assert!(!formatted.contains("no supported language"));
    }
}

#[cfg(test)]
mod bash_companion_format_tests {
    use super::*;
    use serde_json::json;

    fn status_text(data: Value, output_mode: Option<&str>) -> String {
        let ctx = FormatContext::from_tool_call(
            "bash_status",
            &json!({ "taskId": "bash-1", "outputMode": output_mode }),
            Path::new("/project"),
        );
        format_response_with_context("bash_status", &Response::success("1", data), &ctx)
    }

    #[test]
    fn bash_worker_status_names_only_a_wait_tool_the_catalog_serves() {
        let data = json!({
            "task_id": "bash-1",
            "status": "running",
            "mode": "pipes",
            "output_preview": "partial",
        });
        let mut ctx = FormatContext {
            worker_session: true,
            bash_watch_available: Some(false),
            ..Default::default()
        };
        let without_watch = format_response_with_context(
            "bash_status",
            &Response::success("1", data.clone()),
            &ctx,
        );
        assert!(!without_watch.contains("bash_watch"), "{without_watch}");
        assert!(without_watch.contains("bash_status"), "{without_watch}");

        ctx.bash_watch_available = Some(true);
        let with_watch =
            format_response_with_context("bash_status", &Response::success("1", data), &ctx);
        assert!(
            with_watch.contains("To wait for it, call bash_watch; don't poll."),
            "{with_watch}"
        );
    }

    #[test]
    fn running_bash_status_includes_an_assigned_remote_job_id() {
        let text = status_text(
            json!({
                "task_id": "bash-1",
                "status": "running",
                "mode": "pipes",
                "remote_job_id": "0192a64a-1234-7000-8000-000000000001",
                "remote_phase_note": "queued on the runner (1 s)",
            }),
            None,
        );
        assert!(
            text.contains("Task bash-1: running · remote job 0192a64a-1234-7000-8000-000000000001")
        );
        assert!(text.contains("queued on the runner (1 s)"));
    }

    #[test]
    fn running_bash_status_without_an_assigned_remote_job_keeps_its_header() {
        let text = status_text(
            json!({
                "task_id": "bash-1",
                "status": "running",
                "mode": "pipes",
                "remote_phase_note": "submitting to the runner (capturing inputs; not accepted yet)",
            }),
            None,
        );
        assert!(text.starts_with("Task bash-1: running · submitting to the runner"));
        assert!(!text.contains("remote job"));
    }

    #[test]
    fn pty_incomplete_capture_warning_preserves_command_status() {
        let reason = "PTY output may be incomplete: output drain deadline expired before EOF";
        for (exit_code, status) in [(0, "completed"), (17, "failed")] {
            for mode in ["screen", "raw", "both"] {
                let text = status_text(
                    json!({
                        "task_id": "bash-1", "mode": "pty", "status": status,
                        "exit_code": exit_code, "output_incomplete": true,
                        "status_reason": reason, "pty_screen": "prefix", "pty_raw": "prefix",
                    }),
                    Some(mode),
                );
                assert!(text.contains(&format!("Task bash-1: {status} (exit {exit_code})")));
                assert!(text.contains(reason), "status text: {text}");
            }
        }
    }

    #[test]
    fn failed_bash_status_renders_its_reason_without_incomplete_output() {
        let reason = "failed to spawn background bash command: No such file or directory";
        let text = status_text(
            json!({
                "task_id": "bash-1",
                "status": "failed",
                "mode": "pipes",
                "status_reason": reason,
            }),
            None,
        );

        assert_eq!(text, format!("Task bash-1: failed\n[{reason}]"));
    }

    #[test]
    fn bash_status_renders_piped_and_pty_text_like_the_opencode_tool() {
        assert_eq!(
            status_text(
                json!({
                    "task_id": "bash-1",
                    "status": "completed",
                    "mode": "pipes",
                    "exit_code": 0,
                    "duration_ms": 1500,
                    "output_preview": "done",
                }),
                None,
            ),
            "Task bash-1: completed (exit 0) 2s\ndone"
        );
        assert_eq!(
            status_text(
                json!({ "task_id": "bash-1", "status": "running", "mode": "pipes", "output_preview": "partial" }),
                None,
            ),
            "Task bash-1: running\nA completion reminder will be delivered automatically; don't poll."
        );
        assert_eq!(
            status_text(
                json!({ "task_id": "bash-1", "status": "running", "mode": "pty", "pty_screen": ">>> " }),
                None,
            ),
            "Task bash-1: running\n>>> \nPTY task is still running. Use bash_status({ taskId: \"bash-1\", outputMode: \"screen\" }) to inspect, bash_write({ taskId: \"bash-1\", input: \"...\" }) to send keystrokes."
        );
        assert_eq!(
            status_text(
                json!({ "task_id": "bash-1", "status": "completed", "mode": "pty", "pty_screen": "s", "pty_raw": "r" }),
                Some("both"),
            ),
            "Task bash-1: completed\n{\n  \"screen\": \"s\",\n  \"raw\": \"r\"\n}"
        );
        assert_eq!(
            status_text(
                json!({ "task_id": "bash-1", "status": "completed", "mode": "pty", "pty_screen": "s", "pty_raw": "r" }),
                Some("raw"),
            ),
            "Task bash-1: completed\nr"
        );
    }

    #[test]
    fn bash_kill_unknown_outcome_reports_runner_answer() {
        for cancel in [
            "runner confirmed cancellation",
            "runner reports job already ended",
            "remote cancellation unconfirmed; outcome is still unknown: runner unavailable",
        ] {
            assert_eq!(
                format_bash_kill(&json!({
                    "task_id": "bash-unknown", "status": "fate_unknown", "remote_cancel": cancel,
                })),
                format!("Task bash-unknown: fate_unknown · {cancel}")
            );
        }
    }

    #[test]
    fn bash_kill_and_write_render_like_the_opencode_tools() {
        let ctx = FormatContext::default();
        assert_eq!(
            format_response_with_context(
                "bash_kill",
                &Response::success(
                    "1",
                    json!({ "task_id": "bash-1", "status": "killed", "kill_signaled": true, "kill_reached": 2 }),
                ),
                &ctx,
            ),
            "Task bash-1: kill_signaled · reached 2 live descendants"
        );
        assert_eq!(
            format_response_with_context(
                "bash_kill",
                &Response::success("1", json!({ "task_id": "bash-1", "status": "killed" })),
                &ctx,
            ),
            "Task bash-1: killed"
        );
        assert_eq!(
            format_response_with_context(
                "bash_write",
                &Response::success("1", json!({ "bytes_written": 3 })),
                &ctx,
            ),
            "{\n  \"bytes_written\": 3\n}"
        );
        assert_eq!(
            format_response_with_context(
                "bash_write",
                &Response::error(
                    "1",
                    "task_not_pty",
                    "background task is not a PTY task: bash-1"
                ),
                &ctx,
            ),
            "background task is not a PTY task: bash-1"
        );
    }
}

#[cfg(test)]
mod incomplete_diagnostics_tests {
    use super::*;

    #[test]
    fn failed_producer_never_renders_legacy_zero_as_a_total() {
        let summary = serde_json::json!({"diagnostics": {
            "complete": false, "errors": 0, "warnings": 0,
            "gaps": [{"producer": "typescript", "reason": "initialize crashed"}]
        }});
        let text = format_diagnostics_summary(Some(&summary)).unwrap();
        assert_eq!(
            text,
            "diagnostics: unknown (typescript: initialize crashed)"
        );
    }

    #[test]
    fn incomplete_diagnostics_keep_answering_producer_zero_labelled() {
        let summary = serde_json::json!({"diagnostics": {
            "complete": false, "errors": null, "warnings": null,
            "gaps": [{"producer": "typescript", "reason": "initialize timed out"}],
            "by_producer": {"rust": {"errors": 0, "warnings": 1, "info": 0, "hints": 0}}
        }});
        let text = format_diagnostics_summary(Some(&summary)).unwrap();
        assert_eq!(text, "diagnostics: unknown (typescript: initialize timed out); 0 errors, 1 warnings, 0 info, 0 hints from rust");
    }
}

#[cfg(test)]
mod inspect_header_tests {
    use super::*;

    #[test]
    fn inspect_header_passes_through_non_diagnostic_partial_without_a_second_status() {
        let text = "PARTIAL — dead code still building; retry aft_inspect.\ndiagnostics: 0 errors, 0 warnings, 0 info, 0 hints";
        let response = Response::success(
            "inspect-header",
            serde_json::json!({
                "complete": false, "inspect_terminal": "partial",
                "partial_reason": "dead code still building; retry aft_inspect.",
                "text": text,
                "summary": {"diagnostics": {"errors": 0, "warnings": 0, "info": 0, "hints": 0}}
            }),
        );
        assert_eq!(format_inspect(&response), text);
    }

    #[test]
    fn inspect_header_renders_non_diagnostic_partial_for_unheaded_payload() {
        let response = Response::success(
            "inspect-header",
            serde_json::json!({
                "complete": false, "inspect_terminal": "partial",
                "partial_reason": "dead code still building; retry aft_inspect.", "text": "body"
            }),
        );
        assert_eq!(
            format_inspect(&response),
            "PARTIAL — dead code still building; retry aft_inspect.\nbody"
        );
    }
}

#[cfg(test)]
mod import_format_tests {
    use super::{format_import, FormatContext};
    use serde_json::json;

    #[test]
    fn remove_reply_names_the_removed_specifier_and_import_scope() {
        let context = FormatContext {
            import_op: Some("remove".to_string()),
            import_remove_name: Some("linkSync".to_string()),
            ..FormatContext::default()
        };
        assert_eq!(
            format_import(
                &json!({
                    "removed": true,
                    "module": "node:fs",
                    "file": "source.ts",
                    "remaining_names": 8,
                    "only_name": false
                }),
                &context
            ),
            "removed linkSync from the node:fs import (8 names remain)\nfile source.ts"
        );
        assert_eq!(
            format_import(
                &json!({
                    "removed": true,
                    "module": "node:fs",
                    "file": "source.ts",
                    "remaining_names": 0,
                    "only_name": true
                }),
                &context
            ),
            "removed the node:fs import (its only name)\nfile source.ts"
        );
    }
}

#[cfg(test)]
mod model_flag_tests {
    use super::*;

    #[test]
    fn outline_files_model_boolean_matches_execution() {
        for value in [serde_json::json!("true"), serde_json::json!(1.0)] {
            let context = FormatContext::from_tool_call(
                "outline",
                &serde_json::json!({"files": value}),
                Path::new("."),
            );
            assert_eq!(context.outline_mode, OutlineMode::Files);
        }
    }

    #[test]
    fn floating_model_boolean_dry_run_is_not_rendered_as_applied() {
        let context = FormatContext::from_tool_call(
            "ast_replace",
            &serde_json::json!({"dryRun": 1.0}),
            Path::new("."),
        );
        assert!(context.ast_dry_run);
        let context = FormatContext::from_tool_call(
            "callgraph",
            &serde_json::json!({"includeUnresolved": 1.0}),
            Path::new("."),
        );
        assert!(context.callgraph_include_unresolved);
    }
}

#[cfg(test)]
mod run_scope_status_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bash_status_names_scope_revocation() {
        for reason in [
            "scope revoked",
            crate::bash_background::registry::SCOPE_REVOKED_REASON,
        ] {
            let data = json!({"task_id":"bash-dead", "status":"killed", "status_reason":reason});
            let text = format_bash_status(&data, None, false, false);
            assert!(text.contains("killed (scope revoked)"), "{text}");
        }
        let ordinary = format_bash_status(
            &json!({"task_id":"bash-dead", "status":"killed"}),
            None,
            false,
            false,
        );
        assert_eq!(ordinary, "Task bash-dead: killed");
    }
}

#[cfg(test)]
mod glob_skip_reason_note_tests {
    use super::format_glob_skip_reasons_note;
    use serde_json::json;

    #[test]
    fn version_check_timeout_is_reported_in_the_glob_note() {
        let note = format_glob_skip_reasons_note(Some(&json!([
            "formatter_version_check_timeout",
            "unsupported_language"
        ])))
        .expect("a version-check timeout is actionable");
        assert!(note.contains("formatter_version_check_timeout"), "{note}");
        assert!(!note.contains("unsupported_language"), "{note}");
    }
}
#[cfg(test)]
mod executed_outline_mode_tests {
    use super::*;

    #[test]
    fn outline_format_context_does_not_resolve_or_stat_targets() {
        let source = include_str!("subc_format.rs");
        let helper = source
            .split("fn outline_mode_for_call(")
            .nth(1)
            .unwrap()
            .split("fn callgraph_op_for_call")
            .next()
            .unwrap();
        assert!(!helper.contains("std::fs::metadata"));
        assert!(!helper.contains("resolve_path_from_project_root"));
    }

    #[test]
    fn executed_outline_text_does_not_inherit_a_stale_directory_mode() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        let context = FormatContext::from_tool_call(
            "outline",
            &serde_json::json!({"target":"target"}),
            root.path(),
        );
        std::fs::remove_dir(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("target"), "file now").unwrap();
        let response = Response::success(
            "outline",
            serde_json::json!({"text":"file result", "complete":false}),
        );
        assert_eq!(
            format_response_with_context("outline", &response, &context),
            "file result"
        );
    }

    #[test]
    fn executed_outline_files_metadata_selects_its_partial_disclosure() {
        let context = FormatContext::from_tool_call(
            "outline",
            &serde_json::json!({"target":"https://example.test/outline"}),
            Path::new("/absent"),
        );
        let response = Response::success(
            "outline",
            serde_json::json!({"text":"table", "files":[], "complete":false, "unchecked_files":["src/b.ts"]}),
        );
        let rendered = format_response_with_context("outline", &response, &context);
        assert!(rendered.contains("Partial result"), "{rendered}");
        assert!(
            rendered.contains("Unchecked files:\n  src/b.ts"),
            "{rendered}"
        );
    }
}

/// Render a bounded line batch; offsets refer to the original, unmasked stream.
pub fn format_watch_line_batch(
    task_id: &str,
    batch: &crate::bash_background::watch_lines::LineBatch,
) -> String {
    let mut text = String::new();
    for line in &batch.lines {
        let body = if line.unavailable {
            crate::bash_background::watch_lines::unavailable_match_text(
                line.count,
                batch
                    .masking_limit_bytes
                    .map(|limit| limit as usize)
                    .unwrap_or(crate::bash_background::watch_lines::MASKING_LINE_CAP_BYTES),
            )
        } else {
            line.text.clone()
        };
        let repeats = if line.count > 1 {
            format!(" ×{}", line.count)
        } else {
            String::new()
        };
        text.push_str(&format!(
            "{} byte {}{repeats}: {}\n",
            line.stream, line.offset, body
        ));
    }
    if batch.dropped_lines > 0 || batch.skipped_long_lines > 0 {
        let incomplete = if batch.skipped_long_lines > 0 {
            format!(
                " {} lines longer than 64 KiB were skipped, so the scan is incomplete.",
                batch.skipped_long_lines
            )
        } else {
            String::new()
        };
        let cursor = if batch.first_dropped_stream.as_deref() == Some("stderr") {
            "stderr_offset"
        } else {
            "output_offset"
        };
        text.push_str(&format!("{} matching lines dropped.{incomplete} Read them with bash_status({{ task_id: {}, {cursor}: {} }}).\n", batch.dropped_lines, serde_json::to_string(task_id).unwrap_or_default(), batch.first_dropped_offset.unwrap_or(0)));
    }
    if let Some(gap) = &batch.retry_gap {
        let missing = match gap.known_missing_occurrences {
            0 => "Some matches may be missing.".to_string(),
            1 => "At least 1 match is missing.".to_string(),
            count => format!("At least {count} matches are missing."),
        };
        let cursor = if gap.stream.as_deref() == Some("stderr") {
            "stderr_offset"
        } else {
            "output_offset"
        };
        text.push_str(&format!("Some watch notices were lost: a stalled connection dropped {} of them, and they cannot be recovered. {missing} Read the task output with bash_status({{ task_id: {}, {cursor}: {} }}).\n", gap.dropped_notifications, serde_json::to_string(task_id).unwrap_or_default(), gap.first_offset.unwrap_or(0)));
    }
    text.trim_end().to_string()
}

#[cfg(test)]
mod watch_line_format_tests {
    use super::format_watch_line_batch;
    use crate::bash_background::watch_lines::{LineBatch, RetryGap, WatchLine};

    #[test]
    fn retry_gap_wording_golden() {
        for (notices, missing, expected) in [
            (2, 1, "Some watch notices were lost: a stalled connection dropped 2 of them, and they cannot be recovered. At least 1 match is missing. Read the task output with bash_status({ task_id: \"bash-gap\", stderr_offset: 16 })."),
            (1, 0, "Some watch notices were lost: a stalled connection dropped 1 of them, and they cannot be recovered. Some matches may be missing. Read the task output with bash_status({ task_id: \"bash-gap\", stderr_offset: 16 })."),
            (1, 1, "Some watch notices were lost: a stalled connection dropped 1 of them, and they cannot be recovered. At least 1 match is missing. Read the task output with bash_status({ task_id: \"bash-gap\", stderr_offset: 16 })."),
            (2, 3, "Some watch notices were lost: a stalled connection dropped 2 of them, and they cannot be recovered. At least 3 matches are missing. Read the task output with bash_status({ task_id: \"bash-gap\", stderr_offset: 16 })."),
        ] {
            let batch = LineBatch {
                retry_gap: Some(RetryGap {
                    reason: "retry_overflow".into(), dropped_notifications: notices,
                    known_missing_occurrences: missing, first_offset: Some(16), stream: Some("stderr".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert_eq!(format_watch_line_batch("bash-gap", &batch), expected);
        }
    }

    #[test]
    fn each_line_row_wording_golden() {
        let mut batch = LineBatch {
            lines: vec![WatchLine {
                stream: "stdout".into(),
                offset: 18234,
                end_offset: 18257,
                text: "ERROR db timeout".into(),
                unavailable: false,
                count: 1,
            }],
            ..Default::default()
        };
        assert_eq!(
            format_watch_line_batch("task", &batch),
            "stdout byte 18234: ERROR db timeout"
        );
        batch.lines[0].count = 3;
        assert_eq!(
            format_watch_line_batch("task", &batch),
            "stdout byte 18234 ×3: ERROR db timeout"
        );
    }

    #[test]
    fn each_line_drop_wording_golden() {
        let mut batch = LineBatch {
            dropped_lines: 7,
            first_dropped_stream: Some("stdout".into()),
            first_dropped_offset: Some(18240),
            ..Default::default()
        };
        assert_eq!(format_watch_line_batch("task", &batch),
            "7 matching lines dropped. Read them with bash_status({ task_id: \"task\", output_offset: 18240 }).");
        batch.skipped_long_lines = 2;
        batch.first_dropped_stream = Some("stderr".into());
        assert_eq!(format_watch_line_batch("task", &batch),
            "7 matching lines dropped. 2 lines longer than 64 KiB were skipped, so the scan is incomplete. Read them with bash_status({ task_id: \"task\", stderr_offset: 18240 }).");
    }
}

#[cfg(test)]
mod bot_kill_warning_tests {
    use super::*;
    #[test]
    fn kill_warning_keeps_the_reported_kill_outcome_first() {
        let text = format_bash_kill(
            &serde_json::json!({"task_id":"bash-warning","status":"killed","bot_write_revocation_warning":crate::gh_shim_speech::REVOCATION_NOT_DURABLE}),
        );
        assert_eq!(
            text,
            format!(
                "Task bash-warning: killed\n{}",
                crate::gh_shim_speech::REVOCATION_NOT_DURABLE
            )
        );
        assert!(!text.contains("clean cancellation"));
    }
}
