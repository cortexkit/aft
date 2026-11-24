//! Shell execution, sandbox provisioning, and background task state.

pub mod agent_child_env;
pub mod bash_background;
#[doc(hidden)]
pub mod bash_changed_files;
#[cfg(unix)]
#[doc(hidden)]
pub mod bash_db_hints;
pub mod bash_parse;
pub mod bash_permissions;
pub mod bash_rewrite;
#[doc(hidden)]
pub mod blocking_wait;
pub mod exec_remote;
#[doc(hidden)]
pub mod gh_shim_worker_broker;
#[doc(hidden)]
pub mod late_result_bus;
#[doc(hidden)]
pub mod output_store;
#[doc(hidden)]
pub mod pinned_config;
pub mod sandbox_spawn;
pub mod secret_proxy;
#[doc(hidden)]
pub mod shell_env_provider;
pub mod windows_shell;

pub use aft_base::{slog_debug, slog_error, slog_info, slog_warn};

#[cfg(unix)]
use aft_base::posix_spawn;
use aft_base::{
    agent_commands, alloc_trace, atomic_replace, bash_env, bounded_io, compress, config,
    cross_volume, developer_tools, effective_path, environment, error, file_identity, file_kind,
    fleet_status, gh_shim_paths, git_revision, harness, host_tool_names, inspect_category,
    list_envelope, list_surfaces, lock_diagnostics, log_ctx, log_redact, memory, pinned_dir,
    pinned_executable, policy_path, privacy_spawn, private_storage, process_identity,
    process_spawn, project_content, protocol, secret, shell_format, storage_path, system_temp,
    task_temp_reads, tool_call_result, windows_command, windows_path, worker_jail_canary,
};
#[cfg(any(test, feature = "test-support"))]
use aft_base::{
    durability, logging, test_env, test_gate, test_storage, test_support, walk_boundary,
};
use aft_db::{
    access_policy, db, fs_lock, path_identity, persisted_format, production_storage, reader_floor,
    root_cache, root_identity, sandbox_profile, write_ledger,
};
use aft_edit::{backup, edit, file_read_exact, format};
#[cfg(unix)]
use aft_github::gh_shim_worker_endpoints;
use aft_github::{gh_shim, gh_shim_relay, gh_shim_speech};
use aft_index::{watcher_backend, watcher_filter};
use aft_inspect::{inspect, lsp};
use aft_parse::parser;

use bash_background::gh_ticket as gh_shim_ticket;

pub use aft_base::{task_watch_lines, task_watches};
