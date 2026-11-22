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

pub mod subc;

#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: aft_base::test_allocations::CountingAllocator =
    aft_base::test_allocations::CountingAllocator;

pub(crate) use aft_base::agent_commands;
pub(crate) use aft_base::alloc_trace;
pub(crate) use aft_base::bounded_io;
pub(crate) use aft_base::clock;
pub(crate) use aft_base::cold_build_limiter;
pub(crate) use aft_base::compress;
pub(crate) use aft_base::config;
pub(crate) use aft_base::config_fix;
pub(crate) use aft_base::config_resolve;
pub(crate) use aft_base::developer_tools;
pub(crate) use aft_base::dir_identity;
pub(crate) use aft_base::effective_path;
pub(crate) use aft_base::environment;
#[cfg(test)]
pub(crate) use aft_base::executable_cache;
pub(crate) use aft_base::feature_config;
pub(crate) use aft_base::fleet_status;
pub(crate) use aft_base::harness;
pub(crate) use aft_base::host_tool_names;
pub(crate) use aft_base::inspect_category;
pub(crate) use aft_base::jsonc;
pub(crate) use aft_base::language;
pub(crate) use aft_base::launch_nonce;
pub(crate) use aft_base::lock_diagnostics;
pub(crate) use aft_base::log_ctx;
pub(crate) use aft_base::log_redact;
pub(crate) use aft_base::ort_lifecycle;
pub(crate) use aft_base::pending_response;
#[cfg(test)]
pub(crate) use aft_base::persistence_gate;
pub(crate) use aft_base::policy_path;
#[cfg(unix)]
pub(crate) use aft_base::posix_spawn;
pub(crate) use aft_base::private_storage;
pub(crate) use aft_base::process_identity;
pub(crate) use aft_base::process_io;
pub(crate) use aft_base::protocol;
pub(crate) use aft_base::pty_render;
pub(crate) use aft_base::storage_path;
pub(crate) use aft_base::subc_config;
#[cfg(test)]
pub(crate) use aft_base::system_temp;
pub(crate) use aft_base::task_temp_reads;
pub(crate) use aft_base::task_watch_lines;
pub(crate) use aft_base::task_watches;
#[cfg(test)]
pub(crate) use aft_base::test_allocations;
#[cfg(test)]
pub(crate) use aft_base::test_gate;
#[cfg(test)]
pub(crate) use aft_base::test_support;
pub(crate) use aft_base::tool_gate;
pub(crate) use aft_base::tool_progress;
pub(crate) use aft_base::view_storage_reserve;
pub(crate) use aft_base::walk_boundary;
pub(crate) use aft_base::windows_path;
pub(crate) use aft_base::worker_jail_canary;
pub(crate) use aft_base::{slog_debug, slog_error, slog_info, slog_warn};
pub(crate) use aft_db::sandbox_profile;
pub(crate) use aft_engine::access_policy;
pub(crate) use aft_engine::agent_data;
pub(crate) use aft_engine::backup;
pub(crate) use aft_engine::bash_background;
pub(crate) use aft_engine::bash_changed_files;
#[cfg(unix)]
pub(crate) use aft_engine::bash_db_hints;
pub(crate) use aft_engine::bash_permissions;
pub(crate) use aft_engine::bash_rewrite;
pub(crate) use aft_engine::blocking_wait;
pub(crate) use aft_engine::build_breaker;
pub(crate) use aft_engine::cache_location;
pub(crate) use aft_engine::config_live;
pub(crate) use aft_engine::context;
pub(crate) use aft_engine::database_open;
pub(crate) use aft_engine::db;
pub(crate) use aft_engine::edit;
pub(crate) use aft_engine::engram_catalog;
pub(crate) use aft_engine::exec_remote;
pub(crate) use aft_engine::executor;
pub(crate) use aft_engine::file_read_exact;
pub(crate) use aft_engine::fs_lock;
pub(crate) use aft_engine::gh_shim;
pub(crate) use aft_engine::gh_shim_relay;
pub(crate) use aft_engine::gh_shim_speech;
pub(crate) use aft_engine::gh_shim_worker_broker;
pub(crate) use aft_engine::git_root_resolution;
pub(crate) use aft_engine::hashline;
pub(crate) use aft_engine::inspect;
pub(crate) use aft_engine::io_accounting;
pub(crate) use aft_engine::large_read;
pub(crate) use aft_engine::late_result_bus;
pub(crate) use aft_engine::lifecycle_census;
pub(crate) use aft_engine::logging;
pub(crate) use aft_engine::lsp;
pub(crate) use aft_engine::memory;
pub(crate) use aft_engine::memory_census;
pub(crate) use aft_engine::output_store;
pub(crate) use aft_engine::page_collect;
pub(crate) use aft_engine::parent_search;
pub(crate) use aft_engine::parser;
pub(crate) use aft_engine::patch;
pub(crate) use aft_engine::path_identity;
pub(crate) use aft_engine::persisted_format;
pub(crate) use aft_engine::pinned_executable;
pub(crate) use aft_engine::project_content;
pub(crate) use aft_engine::provisional_identity_retry;
pub(crate) use aft_engine::reader_floor;
pub(crate) use aft_engine::revision_read;
pub(crate) use aft_engine::root_cache;
pub(crate) use aft_engine::root_identity;
pub(crate) use aft_engine::runtime_drain;
pub(crate) use aft_engine::sandbox_spawn;
pub(crate) use aft_engine::scoped_key;
pub(crate) use aft_engine::search_index;
pub(crate) use aft_engine::secret;
pub(crate) use aft_engine::secret_proxy;
pub(crate) use aft_engine::semantic_admission;
pub(crate) use aft_engine::semantic_index;
pub(crate) use aft_engine::standing_roots;
pub(crate) use aft_engine::status_push;
pub(crate) use aft_engine::synapse_embed;
#[cfg(test)]
pub(crate) use aft_engine::test_bridge::test_env;
#[cfg(test)]
pub(crate) use aft_engine::test_bridge::test_storage;
pub(crate) use aft_engine::url_fetch;
pub(crate) use aft_engine::watcher_filter;
pub(crate) use aft_engine::write_ledger;
pub(crate) use aft_tools::{
    commands, response_finalize, run_tool_call, subc_format, subc_translate, tool_output,
};

#[cfg(test)]
pub(crate) use aft_tools::tools_test_support::{self, test_app, test_context};
// The same fake LSP builder is used by the facade integration harness.
#[cfg(test)]
use test_support as test_helpers;
#[cfg(test)]
#[path = "../../aft/tests/helpers/fake_lsp.rs"]
pub(crate) mod fake_lsp_test_helper;

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
