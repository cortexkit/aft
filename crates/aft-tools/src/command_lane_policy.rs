//! Shared command lanes for standalone bookkeeping and daemon scheduling.

use crate::executor::Lane;

pub(crate) fn is_read_only_command(command: &str) -> bool {
    matches!(
        command_lane_explicit(command),
        Some(Lane::PureRead | Lane::SerialLspStatus | Lane::HeavyInit)
    )
}

pub fn command_lane_explicit(command: &str) -> Option<Lane> {
    match command {
        "ping"
        | "version"
        | "echo"
        | "bash_drain_completions"
        | "bash_regex_match"
        | "bash_artifact_owned"
        | "bash_owned_tasks"
        | "bash_wait_detach"
        | "db_get_state"
        | "db_get_host_state"
        | "read"
        | "undo_preview"
        | "edit_history"
        | "checkpoint_paths"
        | "safety_purge_paths"
        | "list_checkpoints"
        | "hashline_preflight"
        | "conflicts"
        | "glob"
        | "grep"
        | "git_conflicts"
        | "ast_search" => Some(Lane::PureRead),

        // Lazy reads mutate parser/terminal/url caches on a miss, but are still
        // classified onto the reader pool; install races are handled at the
        // individual cache sites.
        "bash_status" | "outline" | "zoom" => Some(Lane::PureRead),

        // Only validates the call and hands its wait to its own thread.
        "bash_watch" => Some(Lane::PureRead),

        // Terminal input changes no project state, and a write can wait (up
        // to its deadline) for a program to read its input; on the writer
        // lane that wait would queue every other call on the root.
        "bash_write" => Some(Lane::PureRead),

        "status"
        | "inspect"
        | "lsp_diagnostics"
        | "lsp_inspect"
        | "lsp_hover"
        | "lsp_goto_definition"
        | "lsp_find_references"
        | "lsp_prepare_rename" => Some(Lane::SerialLspStatus),

        "semantic_search" | "search" | "callgraph" | "callers" | "impact" | "call_tree"
        | "trace_to" | "trace_to_symbol" | "trace_data" | "inspect_tier2_run" => {
            Some(Lane::HeavyInit)
        }

        "bash"
        | "powershell"
        | "bash_abort_inflight"
        | "bash_ack_completions"
        | "bash_notify"
        | "bash_unnotify"
        | "bash_promote"
        | "bash_kill"
        | "db_set_state"
        | "db_set_host_state"
        | "undo"
        | "checkpoint"
        | "restore_checkpoint"
        | "safety_purge"
        | "write"
        | "apply_patch"
        | "delete_file"
        | "delete"
        | "move_file"
        | "move"
        | "edit"
        | "edit_symbol"
        | "edit_match"
        | "batch"
        | "add_import"
        | "import"
        | "remove_import"
        | "organize_imports"
        | "configure"
        | "move_symbol"
        | "extract_function"
        | "inline_symbol"
        | "ast_replace"
        | "safety"
        | "lsp_rename"
        | "list_filters"
        | "trust_filter_project"
        | "untrust_filter_project"
        | "snapshot" => Some(Lane::Mutating),

        _ => None,
    }
}
