//! Agent-facing text formatters for subc-mode tool results (parity with TS plugins).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::protocol::Response;
use crate::subc_translate::resolve_path_from_project_root;
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
        }
    }
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

fn outline_mode_for_call(bare_name: &str, arguments: &Value, project_root: &Path) -> OutlineMode {
    if bare_name != "outline" {
        return OutlineMode::Text;
    }
    let Some(obj) = arguments.as_object() else {
        return OutlineMode::Text;
    };
    if obj.get("files").and_then(Value::as_bool) == Some(true) {
        return OutlineMode::Files;
    }
    let Some(target) = obj.get("target").and_then(Value::as_str) else {
        return OutlineMode::Text;
    };
    if target.starts_with("http://") || target.starts_with("https://") {
        return OutlineMode::Text;
    }
    let resolved = resolve_path_from_project_root(project_root, target);
    if std::fs::metadata(resolved)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
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
    obj.get("filePath")
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
    match value {
        Value::Bool(value) => *value,
        Value::Number(num) => num.as_i64() == Some(1) || num.as_u64() == Some(1),
        Value::String(raw) => {
            let normalized = raw.trim().to_ascii_lowercase();
            normalized == "true" || normalized == "1"
        }
        _ => false,
    }
}

// Return true for agent tools whose text output is formatted on the server.
// The capability manifest and this formatter both identify tools by their bare
// internal routing names, without the agent-facing `aft_` prefix.
fn is_core_agent_tool(bare_name: &str) -> bool {
    matches!(
        bare_name,
        "status"
            | "bash"
            | "powershell"
            | "read"
            | "write"
            | "edit"
            | "apply_patch"
            | "grep"
            | "glob"
            | "search"
            | "outline"
            | "zoom"
            | "inspect"
            | "callgraph"
            | "conflicts"
            | "ast_search"
            | "ast_replace"
            | "delete"
            | "move"
            | "import"
            | "safety"
    )
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
    if !is_core_agent_tool(bare_name) {
        return serialized_text_or_failure(
            serde_json::to_string(response),
            "non-core tool response",
        );
    }

    let data = &response.data;
    if !response.success {
        return format_error(bare_name, data, ctx);
    }

    let mut text = match bare_name {
        "edit" => format_edit_response(data),
        "write" => format_write_response(data),
        "apply_patch" => format_apply_patch(data),
        "read" => format_read(data, ctx.agent_specified_range),
        "grep" => format_grep(data),
        "glob" => data["text"].as_str().unwrap_or_default().to_string(),
        "search" => format_search(data),
        "outline" => format_outline(response, ctx.outline_mode),
        "zoom" => format_zoom(data, ctx),
        "inspect" => format_inspect(response),
        "status" => format_status(data),
        "bash" | "powershell" => data["output"].as_str().unwrap_or_default().to_string(),
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
        _ => unreachable!("core agent tools are exhaustive"),
    };
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
    text
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

    if deleted.len() == 1 && skipped.is_empty() {
        let file = deleted[0]
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return format!("Deleted {file}");
    }

    let total = deleted.len() + skipped.len();
    format!("Deleted {}/{} file(s)", deleted.len(), total)
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
            let status = if response.get("already_present").and_then(Value::as_bool) == Some(true) {
                "already present"
            } else {
                "added"
            };
            [
                format!("{status} {}", import_module_name(response, ctx)),
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
            let status = if response.get("removed").and_then(Value::as_bool) == Some(false) {
                format!("not present {module}")
            } else {
                format!("removed {module}")
            };
            let scope = ctx
                .import_remove_name
                .as_deref()
                .filter(|name| !name.is_empty())
                .map(|name| format!("name {name}"))
                .unwrap_or_else(|| "scope entire import".to_string());
            [
                status,
                format!("file {}", import_file_name(response, ctx)),
                scope,
            ]
            .join("\n")
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
                [
                    format!("restored operation {op_id}"),
                    format!("files {files}"),
                ]
                .join("\n")
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
                [
                    format!("restored {}", shorten_path(&file)),
                    format!("backup {backup}"),
                ]
                .join("\n")
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
            let mut lines = vec![
                format!("checkpoint created {name}"),
                format!("files {files}"),
                skipped_text,
            ];
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
            let mut lines = vec![
                format!("checkpoint restored {name}"),
                format!("files {files}"),
            ];
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
        _ => "No safety result.".to_string(),
    }
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

fn format_unix_millis_utc(millis: i64) -> String {
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

fn civil_from_days(days: i64) -> (i64, i64, i64) {
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
    output
}

// Mirrors packages/opencode-plugin/src/tools/hoisted.ts createEditTool.
fn format_edit_response(data: &Value) -> String {
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
    if data.get("no_op").and_then(Value::as_bool) == Some(true) {
        result.push_str(
            "\n\nNote: no net file change — the match was found and applied, but the file content is byte-identical to before. Likely causes: oldString and newString are identical, or a formatter normalized the change away.",
        );
    }
    append_lsp_error_lines(&mut result, data, false);
    append_lsp_server_notes(&mut result, data);
    result
}

fn format_glob_skip_reasons_note(reasons: Option<&Value>) -> Option<String> {
    let actionable = reasons?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|reason| {
            matches!(
                *reason,
                "formatter_not_installed" | "formatter_excluded_path" | "timeout" | "error"
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
        output.push_str(&format!(
            "\n\nNote: LSP server(s) did not respond in time: {}. Diagnostics are incomplete for this call; wait for the LSP update and use the next normal aft_inspect, not repeated polling.",
            pending.join(", ")
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
    s
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
fn format_grep(data: &Value) -> String {
    if let Some(text) = data.get("text").and_then(Value::as_str) {
        return text.to_string();
    }

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
        return format!("Found {total_matches} match across {files_with_matches} file");
    }

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
    format!("{body}\n\nFound {total_matches} match across {files_with_matches} file")
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
                if let Some(text) = m.get("text").and_then(Value::as_str) {
                    output.push_str(&format!("  {}\n", text.trim()));
                }
                if let Some(meta_vars) = m.get("meta_variables").and_then(Value::as_object) {
                    if !meta_vars.is_empty() {
                        for (key, value) in meta_vars {
                            output.push_str(&format!("  {key}: {}\n", js_template_string(value)));
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
    output
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
    let note = extra_honesty_note(data);
    if let Some(text) = data
        .get("text")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return match note {
            Some(n) => format!("{text}\n{n}"),
            None => text.to_string(),
        };
    }
    semantic_honesty_note(data).unwrap_or_else(|| "No results.".to_string())
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
        notes.push("fully degraded");
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
        notes.push("fully degraded");
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
    let skipped = data.get("skipped_files").and_then(Value::as_array);
    let Some(skipped) = skipped.filter(|s| !s.is_empty()) else {
        return text.to_string();
    };

    let lines: Vec<String> = skipped
        .iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            let file = obj.get("file").and_then(Value::as_str)?;
            let reason = obj
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("skipped");
            Some(format!("  {file} — {reason}"))
        })
        .collect();
    if lines.is_empty() {
        return text.to_string();
    }
    let header = if text.is_empty() { "" } else { "\n\n" };
    format!(
        "{text}{header}Skipped {} file(s):\n{}",
        lines.len(),
        lines.join("\n")
    )
}

// Format zoom responses as plain text so direct calls and server-side calls
// produce identical output.
fn format_zoom(data: &Value, ctx: &FormatContext) -> String {
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
    if let Some(calls_out) = calls_out.filter(|calls| !calls.is_empty()) {
        out.push(String::new());
        out.push("──── calls_out".to_string());
        for call in calls_out {
            out.push(format_zoom_call_ref(call));
        }
    }

    let called_by = annotations
        .and_then(|annotations| annotations.get("called_by"))
        .and_then(Value::as_array);
    if let Some(called_by) = called_by.filter(|calls| !calls.is_empty()) {
        out.push(String::new());
        out.push("──── called_by".to_string());
        for call in called_by {
            out.push(format_zoom_call_ref(call));
        }
    }

    out.join("\n")
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

// Mirrors packages/opencode-plugin/src/tools/inspect.ts inspectTools.
fn format_inspect(response: &Response) -> String {
    if let Some(text) = format_inspect_terminal(&response.data) {
        return text;
    }
    if let Some(text) = response.data.get("text").and_then(Value::as_str) {
        return append_rendered_diagnostics(text, &response.data);
    }
    let json =
        serialized_text_or_failure(serde_json::to_string_pretty(response), "inspect response");
    append_rendered_diagnostics(&json, &response.data)
}

// Mirrors packages/opencode-plugin/src/tools/inspect.ts appendRenderedDiagnostics.
fn append_rendered_diagnostics(text: &str, data: &Value) -> String {
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

fn format_diagnostics_summary(summary: Option<&Value>) -> Option<String> {
    let section = summary?.get("diagnostics")?.as_object()?;
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

    let sections = match op {
        "call_tree" => format_call_tree_sections(record, include_unresolved),
        "callers" => format_callers_sections(record),
        "trace_to_symbol" => format_trace_to_symbol_sections(record),
        "trace_to" => format_trace_to_sections(record),
        "impact" => format_impact_sections(record),
        _ => format_trace_data_sections(record),
    };
    sections.join("\n")
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
                "Pass toFile to disambiguate"
            } else {
                "Try one of these files for toFile"
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
    let mut lines = Vec::new();
    render_call_tree_node(record, 0, &mut lines, include_unresolved);
    let warning = depth_warning(record, "depth_limited", "truncated");
    if !warning.is_empty() {
        lines.push(warning);
    }
    if lines.is_empty() {
        vec!["No call tree available.".to_string()]
    } else {
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

fn format_callers_sections(record: &serde_json::Map<String, Value>) -> Vec<String> {
    let groups = records_field(record, "callers");
    let warning = depth_warning(record, "depth_limited", "truncated");
    let hub_summary = hub_summary_line(record);
    let total = number_field(record, "total_callers").unwrap_or(0);
    let mut sections = vec![join_non_empty(&[
        Some(format!(
            "{total} caller{}",
            if total == 1 { "" } else { "s" }
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
        let key = format!("{symbol}\0{provenance}");
        let bucket = by_symbol_provenance.entry(key).or_default();
        if let Some(line) = number_field(caller, "line") {
            bucket.push(line);
        }
    }
    for (key, mut line_nums) in by_symbol_provenance {
        let symbol = key.split('\0').next().unwrap_or("(unknown)");
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
        lines.push(format!("  ↳ {symbol}:{line_part}{marker}"));
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
    let paths = records_field(record, "paths");
    let warning = depth_warning(record, "max_depth_reached", "truncated_paths");
    let hub_summary = hub_summary_line(record);
    let total_paths = number_field(record, "total_paths").unwrap_or(paths.len() as i64);
    let total_paths_is_lower_bound = record
        .get("total_paths_is_lower_bound")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let entry_points = number_field(record, "entry_points_found").unwrap_or(0);
    let mut sections = vec![join_non_empty(&[
        Some(format!(
            "{}{total_paths} path{}",
            if total_paths_is_lower_bound {
                "at least "
            } else {
                ""
            },
            if total_paths == 1 { "" } else { "s" }
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
    let callers = records_field(record, "callers");
    let warning = depth_warning(record, "depth_limited", "truncated");
    let hub_summary = hub_summary_line(record);
    let total_affected = number_field(record, "total_affected").unwrap_or(callers.len() as i64);
    let affected_files = number_field(record, "affected_files").unwrap_or(0);
    let mut sections = vec![join_non_empty(&[
        Some(format!(
            "{total_affected} affected call site{}",
            if total_affected == 1 { "" } else { "s" }
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
            format!("  ↳ {symbol}{entry}{name_match}"),
        ];
        if let Some(expression) = expression {
            lines.push(format!("  {expression}"));
        }
        if !params.is_empty() {
            lines.push(format!("  params: {params}"));
        }
        sections.push(lines.join("\n"));
    }
    sections
}

fn format_trace_data_sections(record: &serde_json::Map<String, Value>) -> Vec<String> {
    let hops = records_field(record, "hops");
    let mut sections = vec![join_non_empty(&[
        Some(format!(
            "{} hop{}",
            hops.len(),
            if hops.len() == 1 { "" } else { "s" }
        )),
        (record.get("depth_limited").and_then(Value::as_bool) == Some(true))
            .then_some("(depth limited)".to_string()),
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
    let completed = data
        .get("completed_phases")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);

    match kind.as_str() {
        "phase-failed" => {
            let detail = compact_inspect_terminal_field(data.get("failure_detail"), "not supplied");
            let reason = compact_inspect_terminal_field(data.get("failure_reason"), "not supplied");
            Some(format!(
                "inspect could not complete: {detail} ({reason}).\nCompleted phases: {completed}. Retry, or narrow with sections=..."
            ))
        }
        "interrupted" => {
            let phase_label = if completed == 1 { "phase" } else { "phases" };
            Some(format!(
                "inspect was interrupted before it could complete (after {completed} completed {phase_label}); no fresh snapshot was produced.\nRetry is safe; retry inspect, or narrow with sections=..."
            ))
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
        parts.join(" ")
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
}
