//! Which commands require the root's aft.db runtime before they may execute.

/// `None` means an unclassified command. Callers fail closed for unknown
/// commands; the manifest coverage test requires an explicit decision for new
/// mutating tools instead of silently depending on that fallback.
pub(crate) fn classification(name: &str) -> Option<bool> {
    let name = name.strip_prefix("aft_").unwrap_or(name);
    // Finished task files remain readable when aft.db cannot open. Status still
    // waits for initialization so running tasks can be recovered after a restart.
    if name == "bash_status" {
        return Some(false);
    }
    if name == "bash"
        || name == "powershell"
        || name.starts_with("bash_")
        || name.starts_with("db_get_")
        || name.starts_with("db_set_")
    {
        return Some(true);
    }
    match name {
        "write" | "edit" | "edit_symbol" | "edit_match" | "apply_patch" | "delete"
        | "delete_file" | "move" | "move_file" | "move_symbol" | "extract_function"
        | "inline_symbol" | "ast_replace" | "batch" | "import" | "add_import" | "remove_import"
        | "organize_imports" | "lsp_rename" | "hashline" | "hashline_edit" | "safety" | "undo"
        | "undo_preview" | "edit_history" | "checkpoint" | "checkpoint_paths"
        | "list_checkpoints" | "restore_checkpoint" | "safety_purge" | "safety_purge_paths"
        | "snapshot" => Some(true),
        // This wire operation installs an in-memory subscription, even while
        // the root's database is still opening.
        "bg_events" => Some(false),
        "configure"
        | "ping"
        | "version"
        | "echo"
        | "read"
        | "grep"
        | "glob"
        | "outline"
        | "zoom"
        | "search"
        | "semantic_search"
        | "inspect"
        | "inspect_tier2_run"
        | "status"
        | "conflicts"
        | "git_conflicts"
        | "ast_search"
        | "callgraph"
        | "callers"
        | "impact"
        | "call_tree"
        | "trace_to"
        | "trace_to_symbol"
        | "trace_data"
        | "lsp_diagnostics"
        | "lsp_inspect"
        | "lsp_hover"
        | "lsp_goto_definition"
        | "lsp_find_references"
        | "lsp_prepare_rename"
        | "hashline_preflight"
        | "list_filters"
        | "trust_filter_project"
        | "untrust_filter_project"
        | "tool_call" => Some(false),
        _ => None,
    }
}

pub(crate) fn requires_database(name: &str) -> bool {
    classification(name).unwrap_or(true)
}
