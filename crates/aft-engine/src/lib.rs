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

pub mod access_policy;
pub mod agent_child_env;
pub(crate) use aft_base::agent_commands;
pub use aft_base::alert_records;
pub use aft_edit::agent_data;
pub mod alert_state;
// The `aft` binary installs its allocator; the library sets the operation
// labels it attributes allocations to.
pub use aft_base::alloc_trace;
pub use aft_parse::ast_grep_hints;
pub mod ast_grep_lang;
pub use aft_edit::backup;
pub mod bash_background;
#[doc(hidden)]
pub mod bash_changed_files;
#[cfg(unix)]
#[doc(hidden)]
pub mod bash_db_hints;
pub(crate) use aft_base::bash_env;
pub mod bash_parse;
pub mod bash_permissions;
pub mod bash_rewrite;
#[doc(hidden)]
pub mod blocking_wait;
pub use aft_base::atomic_replace;
pub(crate) use aft_base::bounded_io;
pub use aft_base::build_state;
pub use aft_db::build_breaker;
pub mod cache_freshness;
pub mod cache_location;
pub mod callgraph;
pub use aft_parse::callgraph_extractor;
pub use aft_index::callgraph_store;
pub use aft_index::calls;
pub(crate) use aft_base::clock;
pub use aft_base::cold_build_limiter;
pub use aft_base::compress;
pub use aft_edit::checkpoint;
#[cfg(test)]
mod compress_boundary_tests;
#[cfg(test)]
mod fleet_status_boundary_tests;
pub use aft_base::config;
pub use aft_base::config_fix;
pub mod config_live;
pub use aft_base::config_resolve;
pub use aft_base::config_sources;
pub mod context;
pub(crate) use aft_base::cross_volume;
pub use aft_index::data_layer;
#[doc(hidden)]
pub mod database_open;
pub mod db;
pub use aft_base::developer_tools;
pub(crate) use aft_base::dir_identity;
pub use aft_base::disk_floor;
pub(crate) use aft_base::durability;
pub mod edit;
#[cfg(test)]
mod edit_composed_tests;
mod edit_lsp;
pub use aft_base::effective_path;
pub mod engram_catalog;
#[doc(hidden)]
pub use aft_base::environment;
#[cfg(test)]
mod engine_source_guard_tests;
pub use aft_base::error;
pub mod exec_remote;
#[cfg(test)]
use aft_base::executable_cache;
pub mod executor;
pub use aft_base::feature_config;
pub use aft_edit::extract;
pub use aft_index::feature_status;
pub(crate) use aft_base::file_identity;
pub mod file_kind;
pub use aft_base::fleet_status;
#[doc(hidden)]
pub use aft_edit::file_read_exact;
pub use aft_edit::format;
pub use aft_db::fs_lock;
pub use aft_parse::fuzzy_match;
pub mod gh_shim;
pub use aft_base::gh_shim_paths;
pub mod gh_shim_relay;
#[doc(hidden)]
pub mod gh_shim_speech;
pub mod gh_shim_ticket;
#[doc(hidden)]
pub mod gh_shim_worker_broker;
#[cfg(unix)]
#[doc(hidden)]
pub mod gh_shim_worker_endpoints;
pub use aft_base::git_head;
pub use aft_index::git_index_reader;
#[cfg(test)]
pub(crate) use aft_base::git_probe;
pub(crate) use aft_base::git_revision;
#[doc(hidden)]
pub mod git_root_resolution;
pub(crate) use aft_base::github_address;
pub mod github_read;
pub use aft_base::grammar_state;
pub use aft_index::grammar;
pub use aft_index::grep_executor;
pub use aft_base::harness;
pub use aft_base::harness_partition_guard;
pub use aft_base::host_tool_names;
pub use aft_edit::hashline;
pub use aft_edit::{imports, indent};
pub mod inspect;
pub use aft_base::inspect_category;
#[doc(hidden)]
pub mod io_accounting;
pub use aft_base::jsonc;
pub use aft_base::jsonc_edit;
pub mod lang_id;
pub use aft_base::language;
pub(crate) use aft_base::large_read_counts;
#[doc(hidden)]
pub use aft_index::large_read;
#[doc(hidden)]
pub mod late_result_bus;
#[doc(hidden)]
pub use aft_base::launch_fds;
pub use aft_base::launch_nonce;
#[doc(hidden)]
pub mod lifecycle_census;
pub use aft_base::list_envelope;
pub use aft_base::list_surfaces;
pub mod live_db_files;
pub use aft_index::local_embed;
pub(crate) use aft_base::lock_diagnostics;
pub use aft_base::log_ctx;
pub use aft_base::log_redact;
pub mod logging;
pub mod lsp;
pub mod lsp_hints;
pub mod memory;
pub use aft_index::memory_census;
pub mod native_route;
pub(crate) use aft_base::native_stat;
pub use aft_base::ndjson_text;
pub use aft_base::ort_lifecycle;
pub use aft_index::ort_pin;
#[doc(hidden)]
pub mod os_metadata;
#[doc(hidden)]
pub mod output_store;
#[doc(hidden)]
pub mod page_collect;
pub mod parent_search;
pub mod parser;
pub use aft_edit::patch;
pub use aft_db::path_identity;
pub use aft_base::pattern_compile;
pub use aft_base::pending_response;
pub mod persisted_format;
pub(crate) use aft_base::pinned_dir;
pub mod pinned_executable;
use aft_base::platform_tls;
pub(crate) use aft_base::policy_path;
#[cfg(unix)]
pub(crate) use aft_base::posix_spawn;
use aft_base::privacy_spawn;
#[doc(hidden)]
pub use aft_base::private_storage;
pub use aft_base::process_identity;
pub use aft_base::process_io;
pub(crate) use aft_base::process_spawn;
#[doc(hidden)]
pub mod production_storage;
#[doc(hidden)]
pub mod project_content;
pub use aft_base::project_walk;
pub use aft_base::protocol;
#[cfg(test)]
pub(crate) use aft_base::walk_profile;
#[doc(hidden)]
pub mod provisional_identity_retry;
pub use aft_base::pty_render;
pub use aft_base::query_shape;
pub use aft_base::read_policy;
pub use aft_db::reader_floor;
pub mod repeat_state;
#[doc(hidden)]
pub mod revision_read;
pub mod root_cache;
pub use aft_db::root_identity;
pub mod runtime_drain;
pub mod runtime_registry;
pub use aft_db::sandbox_profile;
pub mod sandbox_spawn;
pub use aft_db::scoped_key;
pub use aft_index::search_b2;
pub use aft_index::search_index;
pub use aft_index::search_state;
pub mod secret;
pub mod secret_proxy;
pub use aft_index::semantic_admission;
pub use aft_index::semantic_index;
pub mod setup_plan;
pub(crate) use aft_base::shared_timer;
#[doc(hidden)]
pub mod shell_env_provider;
pub use aft_base::shell_format;
pub mod standing_roots;
#[doc(hidden)]
pub mod status_push;
#[doc(hidden)]
pub mod status_snapshot;
pub use aft_base::storage_path;
#[cfg(all(test, unix))]
mod storage_permissions_tests;
pub use aft_base::subc_config;
#[cfg(test)]
#[path = "../tests/helpers/subc_fake.rs"]
mod subc_fake;
pub use aft_base::symbols;
pub use aft_edit::symbol_diff;
pub use aft_index::synapse_embed;
pub(crate) use aft_base::system_temp;
pub(crate) use aft_base::task_temp_reads;
pub use aft_base::task_watch_lines;
pub use aft_base::task_watches;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_storage;
#[doc(hidden)]
pub use aft_base::tool_call_result;
pub use aft_base::tool_gate;
pub use aft_base::tool_path;
pub(crate) use aft_base::tool_progress;
pub use aft_index::url_fetch;
use aft_base::view_storage_reserve;
#[allow(unused_imports)]
pub(crate) use aft_base::volume;
pub(crate) use aft_base::walk_boundary;
pub use aft_index::watcher;
#[doc(hidden)]
pub use aft_index::watcher_backend;
pub use aft_index::watcher_filter;
pub(crate) use aft_base::watcher_markers;
pub use aft_base::write_counters;
pub mod write_intent;
pub mod write_ledger;
// Compiled on all platforms so cross-platform unit tests in
// `commands::bash::try_spawn_with_fallback` can exercise the retry
// decision logic without a real Windows runtime. The module itself only
// uses portable APIs; only its callers are Windows-gated.
#[allow(unused_imports)]
pub(crate) use aft_base::windows_command;
pub use aft_base::windows_path;
pub mod windows_shell;
pub use aft_base::worker_jail_canary;
#[cfg(test)]
mod gate_hermeticity_tests;
#[cfg(test)]
pub(crate) use aft_base::search_hot_path_measurements as search_test_counts;
#[cfg(test)]
pub(crate) use aft_base::test_allocations;
#[cfg(any(test, feature = "test-support"))]
#[path = "base_tests/test_env.rs"]
pub(crate) mod test_env;
#[cfg(all(test, unix))]
pub(crate) use aft_base::test_gate;
#[cfg(all(test, windows))]
pub(crate) use aft_base::test_read_denial;
#[cfg(test)]
pub(crate) use aft_base::test_support;
// The fake LSP builder also compiles in integration tests under this helper name.
#[cfg(any(test, feature = "test-support"))]
use aft_base::test_support as test_helpers;

#[cfg(all(test, target_os = "macos"))]
mod disk_write_hunt;

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
#[path = "test_fixtures/fake_lsp.rs"]
pub mod fake_lsp_test_helper;
pub(crate) use aft_base::persistence_gate;

#[cfg(feature = "test-support")]
pub mod test_bridge;
