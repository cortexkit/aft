//! Source inspection, language server management, and diagnostic alerts.

pub mod alert_state;
pub mod inspect;
pub mod lsp;
pub mod lsp_hints;

use aft_base::{
    bounded_io, cache_freshness, cold_build_limiter, config, developer_tools, effective_path,
    executor, file_kind, inspect_category, io_accounting, jsonc, jsonc_edit, logging, memory,
    native_stat, private_storage, project_content, project_walk, protocol, shared_timer,
    slog_debug, slog_error, slog_info, slog_warn, storage_path, symbols, tool_progress,
    walk_boundary, windows_path,
};
use aft_db::{build_breaker, db, fs_lock, path_identity, root_cache, root_identity};
use aft_edit::{format, imports};
use aft_index::{callgraph_store, calls, data_layer, search_index};
use aft_parse::{callgraph, parser};

use aft_base::process_identity;
#[cfg(all(test, unix))]
use aft_base::test_gate;
#[cfg(test)]
use aft_base::test_support as test_helpers;
#[cfg(windows)]
use aft_base::windows_command;
#[cfg(test)]
use aft_base::{
    config_resolve, durability, executable_cache, file_identity, git_probe, test_allocations,
    test_env, test_support, walk_profile,
};
#[cfg(test)]
use aft_index::{watcher_backend, watcher_filter};

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use lsp::test_support::fake_lsp as fake_lsp_test_helper;
