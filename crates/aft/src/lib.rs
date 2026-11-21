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

pub use aft_base::{slog_debug, slog_error, slog_info, slog_warn};

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

pub use aft_engine::access_policy;
#[cfg(test)]
mod access_policy_tests;
pub(crate) use aft_base::agent_commands;
pub use aft_base::alert_records;
pub use aft_engine::agent_child_env;
pub use aft_engine::agent_data;
pub use aft_engine::alert_state;
// The `aft` binary installs its allocator; the library sets the operation
// labels it attributes allocations to.
pub use aft_base::alloc_trace;
pub(crate) use aft_base::bash_env;
pub(crate) use aft_base::bounded_io;
pub use aft_engine::ast_grep_lang;
pub use aft_engine::backup;
pub use aft_engine::bash_background;
pub(crate) use aft_engine::bash_changed_files;
#[cfg(unix)]
pub(crate) use aft_engine::bash_db_hints;
pub use aft_engine::bash_parse;
pub use aft_engine::bash_permissions;
pub use aft_engine::bash_rewrite;
pub(crate) use aft_engine::blocking_wait;
pub use aft_parse::ast_grep_hints;
pub(crate) mod bounded_io_command;
pub use aft_base::atomic_replace;
pub use aft_base::build_state;
pub(crate) use aft_base::clock;
pub use aft_base::cold_build_limiter;
pub use aft_engine::build_breaker;
pub use aft_engine::cache_freshness;
pub use aft_engine::cache_location;
pub use aft_engine::callgraph;
pub use aft_engine::callgraph_store;
pub use aft_engine::calls;
pub use aft_engine::checkpoint;
pub use aft_parse::callgraph_extractor;
pub mod commands;
pub use aft_base::compress;
pub use aft_base::config;
pub use aft_base::config_fix;
pub use aft_base::config_resolve;
pub use aft_base::config_sources;
pub(crate) use aft_base::cross_volume;
pub use aft_engine::config_live;
pub use aft_engine::context;
pub mod data_layer;
pub use aft_base::developer_tools;
pub(crate) use aft_base::dir_identity;
pub use aft_base::disk_floor;
pub(crate) use aft_base::durability;
pub use aft_base::effective_path;
#[doc(hidden)]
pub use aft_base::environment;
pub use aft_base::error;
#[cfg(test)]
use aft_base::executable_cache;
pub use aft_base::feature_config;
#[cfg(test)]
pub(crate) use aft_base::file_identity;
pub use aft_base::fleet_status;
pub use aft_base::gh_shim_paths;
pub use aft_base::git_head;
pub(crate) use aft_base::git_probe;
pub(crate) use aft_base::github_address;
pub use aft_base::grammar_state;
pub use aft_base::harness;
pub use aft_base::harness_partition_guard;
pub use aft_base::host_tool_names;
pub use aft_base::inspect_category;
pub use aft_base::jsonc;
pub use aft_base::jsonc_edit;
pub use aft_base::language;
#[cfg(test)]
pub(crate) use aft_base::large_read_counts;
#[doc(hidden)]
pub use aft_base::launch_fds;
pub use aft_base::launch_nonce;
pub use aft_base::list_envelope;
pub use aft_base::list_surfaces;
pub(crate) use aft_base::lock_diagnostics;
pub use aft_base::log_ctx;
pub use aft_base::log_redact;
pub use aft_base::ndjson_text;
pub(crate) use aft_engine::database_open;
pub use aft_engine::db;
pub use aft_engine::edit;
pub use aft_engine::engram_catalog;
pub use aft_engine::exec_remote;
pub use aft_engine::executor;
pub use aft_engine::extract;
pub use aft_engine::feature_status;
pub use aft_engine::file_kind;
pub(crate) use aft_engine::file_read_exact;
pub use aft_engine::format;
pub use aft_engine::fs_lock;
pub use aft_engine::gh_shim;
pub use aft_engine::gh_shim_relay;
pub(crate) use aft_engine::gh_shim_speech;
pub use aft_engine::gh_shim_ticket;
pub(crate) use aft_engine::gh_shim_worker_broker;
pub use aft_engine::git_index_reader;
pub(crate) use aft_engine::git_root_resolution;
pub use aft_engine::github_read;
pub use aft_engine::grammar;
pub use aft_engine::grep_executor;
pub use aft_engine::hashline;
pub use aft_engine::imports;
pub use aft_engine::indent;
pub use aft_engine::inspect;
pub(crate) use aft_engine::io_accounting;
pub use aft_engine::lang_id;
pub(crate) use aft_engine::large_read;
pub(crate) use aft_engine::late_result_bus;
pub(crate) use aft_engine::lifecycle_census;
pub use aft_engine::live_db_files;
pub use aft_engine::local_embed;
pub use aft_engine::logging;
pub use aft_engine::lsp;
pub use aft_engine::lsp_hints;
pub use aft_engine::memory;
pub use aft_engine::memory_census;
pub use aft_engine::native_route;
pub use aft_parse::fuzzy_match;
#[cfg(test)]
mod nested_worktree_tests;
pub use aft_base::ort_lifecycle;
pub use aft_base::pattern_compile;
pub use aft_base::pending_response;
#[cfg(any(not(windows), test))]
pub(crate) use aft_base::pinned_dir;
pub(crate) use aft_base::policy_path;
#[cfg(unix)]
pub(crate) use aft_base::posix_spawn;
use aft_base::privacy_spawn;
#[doc(hidden)]
pub use aft_base::private_storage;
pub use aft_base::process_identity;
pub use aft_base::process_io;
pub use aft_base::project_walk;
pub use aft_base::protocol;
pub use aft_base::pty_render;
pub use aft_base::query_shape;
pub use aft_base::read_policy;
#[cfg(test)]
pub(crate) use aft_base::walk_profile;
pub use aft_engine::ort_pin;
pub(crate) use aft_engine::os_metadata;
pub(crate) use aft_engine::output_store;
pub(crate) use aft_engine::page_collect;
pub use aft_engine::parent_search;
pub use aft_engine::parser;
pub use aft_engine::patch;
pub use aft_engine::path_identity;
pub use aft_engine::persisted_format;
pub use aft_engine::pinned_executable;
pub(crate) use aft_engine::production_storage;
pub(crate) use aft_engine::project_content;
pub(crate) use aft_engine::provisional_identity_retry;
pub use aft_engine::reader_floor;
pub use aft_engine::repeat_state;
pub mod response_finalize;
pub(crate) use aft_engine::revision_read;
pub use aft_engine::root_cache;
pub use aft_engine::root_identity;
pub mod run_tool_call;
pub use aft_base::shell_format;
pub use aft_base::storage_path;
pub use aft_db::sandbox_profile;
pub use aft_engine::runtime_drain;
pub use aft_engine::runtime_registry;
pub use aft_engine::sandbox_spawn;
pub use aft_engine::scoped_key;
pub use aft_engine::search_b2;
pub use aft_engine::search_index;
pub use aft_engine::search_state;
pub use aft_engine::secret;
pub use aft_engine::secret_proxy;
pub use aft_engine::semantic_admission;
pub use aft_engine::semantic_index;
pub use aft_engine::setup_plan;
pub(crate) use aft_engine::shell_env_provider;
pub use aft_engine::standing_roots;
pub(crate) use aft_engine::status_push;
pub(crate) use aft_engine::status_snapshot;
pub mod subc;
pub use aft_base::subc_config;
#[cfg(test)]
#[path = "../tests/helpers/subc_fake.rs"]
mod subc_fake;
pub mod subc_format;
pub mod subc_translate;
pub use aft_engine::symbol_diff;
pub(crate) mod trace_measure_run;
pub use aft_base::symbols;
#[cfg(test)]
pub(crate) use aft_base::system_temp;
pub(crate) use aft_base::task_temp_reads;
pub use aft_base::task_watch_lines;
pub use aft_base::task_watches;
pub use aft_base::tool_gate;
pub use aft_engine::synapse_embed;
#[cfg(test)]
pub(crate) use aft_engine::test_bridge::test_storage;
pub(crate) use aft_engine::tool_call_result;
pub(crate) mod tool_output;
pub use aft_base::tool_path;
pub(crate) use aft_base::tool_progress;
use aft_base::view_storage_reserve;
#[allow(unused_imports)]
pub(crate) use aft_base::volume;
pub(crate) use aft_base::walk_boundary;
pub use aft_base::write_counters;
pub use aft_engine::url_fetch;
pub use aft_engine::watcher;
pub(crate) use aft_engine::watcher_backend;
pub use aft_engine::watcher_filter;
pub use aft_engine::write_intent;
pub use aft_engine::write_ledger;
// Compiled on all platforms so cross-platform unit tests in
// `commands::bash::try_spawn_with_fallback` can exercise the retry
// decision logic without a real Windows runtime. The module itself only
// uses portable APIs; only its callers are Windows-gated.
#[allow(unused_imports)]
pub(crate) use aft_base::windows_command;
pub use aft_base::windows_path;
pub use aft_base::worker_jail_canary;
pub use aft_engine::windows_shell;

#[cfg(all(test, target_os = "linux"))]
mod daemon_heap_probe;
#[cfg(test)]
pub(crate) mod search_hot_path_measurements;
#[cfg(test)]
pub(crate) use aft_base::test_allocations;
#[cfg(test)]
pub(crate) use aft_base::test_gate;
#[cfg(test)]
pub(crate) use aft_base::test_support;
#[cfg(test)]
pub(crate) use aft_engine::test_bridge::test_env;
#[cfg(test)]
mod upper_test_context;
#[cfg(test)]
pub(crate) use upper_test_context::{test_app, test_context};
// The fake LSP builder also compiles in integration tests under this helper name.
#[cfg(test)]
use test_support as test_helpers;

#[cfg(test)]
#[path = "../tests/helpers/fake_lsp.rs"]
pub(crate) mod fake_lsp_test_helper;
#[cfg(test)]
pub(crate) use aft_base::persistence_gate;
