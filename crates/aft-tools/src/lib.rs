#![allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::double_ended_iterator_last,
    clippy::int_plus_one,
    clippy::large_enum_variant,
    clippy::len_without_is_empty,
    clippy::let_and_return,
    clippy::manual_contains,
    clippy::manual_pattern_char_comparison,
    clippy::manual_repeat_n,
    clippy::manual_strip,
    clippy::manual_unwrap_or_default,
    clippy::map_clone,
    clippy::iter_kv_map,
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::needless_range_loop,
    clippy::new_without_default,
    clippy::obfuscated_if_else,
    clippy::ptr_arg,
    clippy::question_mark,
    clippy::same_item_push,
    clippy::should_implement_trait,
    clippy::single_match,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::unnecessary_sort_by,
    clippy::unnecessary_cast,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_map_or
)]

#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: aft_base::test_allocations::CountingAllocator =
    aft_base::test_allocations::CountingAllocator;

pub(crate) use aft_base::{slog_debug, slog_info, slog_warn};

// Keep the CLI’s `aft::` paths when compiled as part of this library.
extern crate self as aft;

#[doc(hidden)]
pub mod cli;

// ## Note on `.unwrap()` / `.expect()` usage
//
// Some `.unwrap()` and `.expect()` calls in `src/` protect internal invariants:
// - **Tree-sitter query operations** (parser.rs, zoom.rs, extract.rs, inline.rs,
//   outline.rs): These operate on AFT's own compiled grammars and query patterns, which
//   are compile-time constants. Pattern captures and node kinds are guaranteed to exist.
// - **Checkpoint serialization** (checkpoint.rs): serde_json::to_value on known-good
//   HashMap<PathBuf, String> types cannot fail.
// The stdin parse loop lives in main.rs and returns a correlated parse-error response
// for malformed request shapes instead of unwrapping the parse result.
//
// User/agent input should be handled with Result or Response::error. This is not
// a guarantee that every production path is panic-free; public accessors can
// still require a configured context, and each invariant needs local review.

pub(crate) use aft_engine::access_policy;
#[cfg(test)]
mod access_policy_tests;
#[cfg(test)]
mod boundary_source_guard_tests;
#[cfg(all(test, unix))]
pub(crate) use aft_base::agent_commands;
pub(crate) use aft_engine::agent_child_env;
pub(crate) use aft_engine::alert_state;
// The `aft` binary installs its allocator; the library sets the operation
// labels it attributes allocations to.
pub(crate) use aft_base::alloc_trace;
pub(crate) use aft_base::bash_env;
pub(crate) use aft_base::bounded_io;
pub(crate) use aft_engine::ast_grep_lang;
pub(crate) use aft_engine::backup;
pub(crate) use aft_engine::bash_background;
pub(crate) use aft_engine::bash_changed_files;
pub(crate) use aft_engine::bash_permissions;
pub(crate) use aft_engine::bash_rewrite;
pub(crate) use aft_engine::blocking_wait;
pub(crate) use aft_parse::ast_grep_hints;
pub(crate) mod bounded_io_command;
pub(crate) use aft_base::clock;
#[cfg(test)]
pub(crate) use aft_base::cold_build_limiter;
#[cfg(test)]
pub(crate) use aft_engine::cache_freshness;
#[cfg(test)]
pub(crate) use aft_engine::cache_location;
pub(crate) use aft_engine::callgraph;
pub(crate) use aft_engine::callgraph_store;
pub(crate) use aft_engine::calls;
pub(crate) use aft_engine::checkpoint;
pub mod commands;
pub(crate) use aft_base::compress;
pub(crate) use aft_base::config;
pub(crate) use aft_base::config_fix;
pub(crate) use aft_base::config_resolve;
pub(crate) use aft_base::cross_volume;
pub(crate) use aft_engine::config_live;
pub(crate) use aft_engine::context;
pub(crate) use aft_engine::db;
pub mod data_layer;
pub(crate) use aft_base::developer_tools;
#[cfg(all(test, feature = "wasm-grammars"))]
pub(crate) use aft_base::disk_floor;
pub(crate) use aft_base::durability;
pub(crate) use aft_base::effective_path;
pub(crate) use aft_base::environment;
pub(crate) use aft_base::error;
#[cfg(test)]
pub(crate) use aft_base::executable_cache;
#[cfg(test)]
pub(crate) use aft_base::file_identity;
pub(crate) use aft_base::fleet_status;
pub(crate) use aft_base::gh_shim_paths;
pub(crate) use aft_base::git_head;
pub(crate) use aft_base::git_probe;
pub(crate) use aft_base::github_address;
pub(crate) use aft_base::harness;
pub(crate) use aft_base::host_tool_names;
pub(crate) use aft_base::inspect_category;
pub(crate) use aft_base::jsonc_edit;
pub(crate) use aft_base::language;
#[cfg(test)]
pub(crate) use aft_base::large_read_counts;
#[cfg(unix)]
pub(crate) use aft_base::launch_fds;
pub(crate) use aft_base::list_envelope;
pub(crate) use aft_base::list_surfaces;
pub(crate) use aft_base::log_ctx;
pub(crate) use aft_base::ndjson_text;
pub(crate) use aft_engine::database_open;
pub(crate) use aft_engine::edit;
pub(crate) use aft_engine::exec_remote;
pub(crate) use aft_engine::executor;
pub(crate) use aft_engine::extract;
pub(crate) use aft_engine::feature_status;
pub(crate) use aft_engine::file_kind;
pub(crate) use aft_engine::format;
#[cfg(test)]
pub(crate) use aft_engine::fs_lock;
pub(crate) use aft_engine::gh_shim;
#[cfg(all(test, unix))]
pub(crate) use aft_engine::gh_shim_relay;
pub(crate) use aft_engine::gh_shim_speech;
pub(crate) use aft_engine::gh_shim_ticket;
pub(crate) use aft_engine::git_root_resolution;
pub(crate) use aft_engine::github_read;
pub(crate) use aft_engine::grammar;
pub(crate) use aft_engine::grep_executor;
pub(crate) use aft_engine::hashline;
pub(crate) use aft_engine::imports;
pub(crate) use aft_engine::indent;
pub(crate) use aft_engine::inspect;
pub(crate) use aft_engine::large_read;
#[cfg(test)]
pub(crate) use aft_engine::lifecycle_census;
pub(crate) use aft_engine::logging;
pub(crate) use aft_engine::lsp;
pub(crate) use aft_engine::lsp_hints;
pub(crate) use aft_engine::memory;
#[cfg(test)]
pub(crate) use aft_engine::memory_census;
pub(crate) use aft_engine::native_route;
pub(crate) use aft_parse::fuzzy_match;
#[cfg(test)]
mod nested_worktree_tests;
pub(crate) use aft_base::pattern_compile;
pub(crate) use aft_base::pending_response;
#[cfg(any(not(windows), test))]
pub(crate) use aft_base::pinned_dir;
pub(crate) use aft_base::policy_path;
pub(crate) use aft_base::privacy_spawn;
pub(crate) use aft_base::private_storage;
#[cfg(all(test, unix))]
pub(crate) use aft_base::process_identity;
pub(crate) use aft_base::process_io;
pub(crate) use aft_base::project_walk;
pub(crate) use aft_base::protocol;
pub(crate) use aft_base::pty_render;
pub(crate) use aft_base::query_shape;
#[cfg(test)]
pub(crate) use aft_base::walk_profile;
pub(crate) use aft_engine::os_metadata;
pub(crate) use aft_engine::output_store;
pub(crate) use aft_engine::parent_search;
pub(crate) use aft_engine::parser;
pub(crate) use aft_engine::patch;
pub(crate) use aft_engine::path_identity;
pub(crate) use aft_engine::persisted_format;
pub(crate) use aft_engine::pinned_executable;
pub(crate) use aft_engine::production_storage;
pub(crate) use aft_engine::project_content;
pub(crate) use aft_engine::provisional_identity_retry;
pub(crate) use aft_engine::reader_floor;
pub(crate) use aft_engine::repeat_state;
pub mod response_finalize;
pub(crate) use aft_engine::revision_read;
pub(crate) use aft_engine::root_cache;
pub(crate) use aft_engine::root_identity;
pub mod run_tool_call;
pub(crate) use aft_base::shell_format;
pub(crate) use aft_base::storage_path;
pub(crate) use aft_base::subc_config;
#[cfg(any(test, unix))]
pub(crate) use aft_db::sandbox_profile;
pub(crate) use aft_engine::runtime_drain;
pub(crate) use aft_engine::sandbox_spawn;
#[cfg(test)]
pub(crate) use aft_engine::scoped_key;
pub(crate) use aft_engine::search_b2;
pub(crate) use aft_engine::search_index;
pub(crate) use aft_engine::search_state;
pub(crate) use aft_engine::secret;
pub(crate) use aft_engine::semantic_admission;
pub(crate) use aft_engine::semantic_index;
pub(crate) use aft_engine::setup_plan;
pub(crate) use aft_engine::shell_env_provider;
pub(crate) use aft_engine::standing_roots;
pub(crate) use aft_engine::status_snapshot;
pub mod subc_format;
pub mod subc_translate;
pub(crate) mod trace_measure_run;
pub(crate) use aft_base::symbols;
pub(crate) use aft_base::task_temp_reads;
pub(crate) use aft_base::task_watch_lines;
pub(crate) use aft_base::task_watches;
pub(crate) use aft_base::tool_gate;
pub(crate) use aft_engine::synapse_embed;
#[cfg(test)]
pub(crate) use aft_engine::test_bridge::test_storage;
pub(crate) use aft_engine::tool_call_result;
pub mod tool_output;
pub(crate) use aft_base::tool_progress;
#[allow(unused_imports)]
pub(crate) use aft_base::volume;
pub(crate) use aft_base::walk_boundary;
pub(crate) use aft_engine::url_fetch;
pub(crate) use aft_engine::watcher_backend;
pub(crate) use aft_engine::watcher_filter;
pub(crate) use aft_engine::write_intent;
pub(crate) use aft_engine::write_ledger;
// Compiled on all platforms so cross-platform unit tests in
// `commands::bash::try_spawn_with_fallback` can exercise the retry
// decision logic without a real Windows runtime. The module itself only
// uses portable APIs; only its callers are Windows-gated.
#[allow(unused_imports)]
pub(crate) use aft_base::windows_command;
#[cfg(any(test, windows))]
pub(crate) use aft_base::windows_path;
#[cfg(any(test, unix))]
pub(crate) use aft_base::worker_jail_canary;

#[cfg(all(test, target_os = "linux"))]
mod daemon_heap_probe;
#[cfg(test)]
pub(crate) mod search_hot_path_measurements;
#[cfg(test)]
pub(crate) use aft_base::test_allocations;
#[cfg(all(test, unix))]
pub(crate) use aft_base::test_gate;
#[cfg(test)]
pub(crate) use aft_base::test_support;
#[cfg(test)]
pub(crate) use aft_engine::test_bridge::test_env;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod tools_test_support;
#[cfg(any(test, feature = "test-support"))]
mod upper_test_context;
// The shared fake LSP builder uses the facade integration helper's module name.
#[cfg(test)]
use test_support as test_helpers;
#[cfg(test)]
pub(crate) use tools_test_support::{test_app, test_context};

#[cfg(test)]
mod allocator_tests {
    #[test]
    fn lib_test_counts_known_allocation() {
        use aft_base::test_allocations::count_bytes;

        const BYTES: usize = 4_096;
        let (allocation, allocated_bytes) =
            count_bytes(|| std::hint::black_box(Box::new([0u8; BYTES])));
        assert_eq!(allocation.len(), BYTES);
        assert_eq!(allocated_bytes, BYTES);
    }
}
