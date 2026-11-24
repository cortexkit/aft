pub mod agent_data;
pub mod backup;
pub mod checkpoint;
pub mod edit;
pub mod extract;
#[doc(hidden)]
pub mod file_read_exact;
pub mod format;
pub mod hashline;
pub mod imports;
pub mod indent;
pub mod patch;
pub mod symbol_diff;

#[cfg(all(test, windows))]
pub(crate) use aft_base::windows_command;
pub(crate) use aft_base::{
    bounded_io, config, cross_volume, developer_tools, durability, durable_dir, effective_path,
    environment, error, executor, file_identity, fs_lock, harness, logging, path_identity,
    pinned_dir, private_storage, process_group, production_storage, project_content, project_walk,
    protocol, slog_debug, slog_error, slog_info, slog_warn, storage_path, symbols, system_temp,
    task_temp_reads, tool_path, tool_progress, walk_boundary, write_intent,
};
#[cfg(test)]
pub(crate) use aft_base::{clock, executable_cache, test_env, test_support};
#[cfg(windows)]
pub(crate) use aft_base::{policy_path, windows_path};
#[cfg(all(test, unix))]
pub(crate) use aft_base::{process_identity, test_gate};
pub(crate) use aft_db::{access_policy, persisted_format, reader_floor, write_ledger};
pub(crate) use aft_parse::{fuzzy_match, line_matcher, parser};

mod db {
    pub(crate) use aft_db::db::*;

    #[cfg(test)]
    pub(crate) fn open(path: &std::path::Path) -> Result<TrackedConnection, OpenError> {
        aft_base::test_storage::assert_database(path);
        aft_db::db::open(path)
    }
}
