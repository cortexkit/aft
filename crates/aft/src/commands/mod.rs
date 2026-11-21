pub mod add_import;
pub mod apply_patch;
pub mod ast_replace;
pub mod ast_scope;
pub mod ast_search;
pub mod bash;

/// Install typed rewrite targets before serving bash in a process or embedder.
pub fn register_shell_handlers() {
    policy_environment::register();
    crate::bash_rewrite::dispatch::register(crate::bash_rewrite::dispatch::RewriteDispatch {
        read: read::handle_read,
        grep: grep::handle_grep,
        glob: glob::handle_glob,
        edit_match: edit_match::handle_edit_match,
    });
}

#[doc(hidden)]
pub fn register_external_search() {
    crate::parent_search::external::register(data_search::dispatch_external_search);
}
pub mod bash_abort_inflight;
pub mod bash_artifact_owned;
pub mod bash_drain_completions;
pub mod bash_kill;
pub(crate) mod bash_kill_scope;
pub mod bash_notify;
pub mod bash_orchestrate;
pub mod bash_owned_tasks;
pub mod bash_promote;
pub mod bash_regex_match;
pub(crate) mod bash_remote_env;
pub mod bash_status;
pub mod bash_wait_detach;
pub mod bash_watch;
pub mod bash_write;
pub mod batch;
#[cfg(test)]
mod cache_location_tests;
pub(crate) use crate::blocking_wait;
pub mod call_tree;
pub mod callers;
pub use crate::data_layer::{callgraph_roots, callgraph_store_adapter};
pub mod checkpoint;
pub mod configure;
#[cfg(test)]
mod configure_test_env;
pub mod conflicts;
pub mod delete_file;
pub(crate) mod delete_tree;
#[cfg(test)]
mod edit_boundary_tests;
pub mod edit_history;
pub mod edit_match;
pub mod edit_symbol;
#[cfg(test)]
mod executor_tests;
pub mod extract_function;
#[cfg(test)]
mod feature_status_tests;
#[cfg(all(test, unix))]
mod fifo_mutation_tests;
pub mod policy_environment;
pub(crate) use crate::file_read_exact;
pub(crate) mod github_comments;
pub mod glob;
pub mod grep;
#[cfg(test)]
mod grep_text_scope_tests;
pub mod hashline;
pub mod health_digest;
pub mod impact;
mod import_specifier_edit;
pub mod inline_symbol;
pub mod inspect;
#[cfg(test)]
mod inspect_boundary_tests;
pub mod list_checkpoints;
pub mod list_filters;
pub mod lsp_diagnostics;
pub mod lsp_find_references;
pub mod lsp_goto_definition;
pub mod lsp_hover;
pub mod lsp_inspect;
pub mod lsp_navigation;
pub mod lsp_prepare_rename;
pub mod lsp_rename;
pub mod memory_census;
pub mod move_file;
pub mod move_symbol;
pub use crate::grep_executor::multi_path;
pub mod organize_imports;
#[cfg(test)]
mod os_metadata_tests;
pub mod outline;
#[cfg(test)]
mod outline_fetch_tests;
#[cfg(test)]
mod outline_parser_tests;
pub mod read;
pub(crate) mod read_manifest;
pub mod remove_import;
pub mod restore_checkpoint;
#[cfg(test)]
pub(crate) mod rewrite_tests;
mod rollback_outcome;
pub mod safety_purge;
#[cfg(test)]
mod search_boundary_tests;
pub mod semantic_search;
pub mod state;
pub mod status;
pub mod symbol_render;
pub mod tool_call;
pub mod trace_data;
pub mod trace_to;
pub mod trace_to_symbol;
pub mod trust_filter_project;
pub mod undo;
pub mod untrust_filter_project;
pub(crate) mod url_output;
mod url_resolution;
#[cfg(all(test, unix))]
pub(crate) use url_resolution::with_secret_fetch_for_test;
pub mod write;
pub mod writes_census;
pub mod zoom;

#[cfg(test)]
mod storage_floor_tests;
#[cfg(test)]
mod store_diet_tests;

pub(crate) mod data_callgraph;
pub(crate) mod data_search;
pub mod parent_query;
#[cfg(test)]
mod parent_query_tests;

#[cfg(all(test, target_os = "linux"))]
mod main_parity_capture;
