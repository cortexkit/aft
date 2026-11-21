//! Test seams for the search, edit, and inspect command adapters.

use std::path::{Path, PathBuf};

pub use crate::edit::{A03_HANDLER_READS, A03_SYNTAX_PARSES};
pub use crate::grep_executor::{with_ignore_probe_state_for_test, IgnoreProbeStateForTest};
pub use crate::hashline::snapshot::{
    helper_state_scope as hashline_helper_state_scope, SOURCE_AFTER_READ, SOURCE_READS,
};

pub const STEP_BYTES: usize = crate::large_read::STEP_BYTES;
pub const KEY_IDLE_EXPIRY: std::time::Duration = crate::repeat_state::KEY_IDLE_EXPIRY;
pub const PATH_ORDER_BATCH_ROW_BUDGET: usize = crate::search_index::PATH_ORDER_BATCH_ROW_BUDGET;

pub fn nearest_git_marker(root: &Path) -> std::io::Result<Option<PathBuf>> {
    crate::root_identity::nearest_git_marker(root)
}

pub fn thread_census() -> crate::lifecycle_census::ThreadCensus {
    crate::lifecycle_census::thread_census()
}

pub fn cleanup_expired(
    harness: &Path,
    retention: std::time::Duration,
) -> crate::output_store::CleanupReport {
    crate::output_store::cleanup_expired(harness, retention)
}

pub fn watcher_path_is_infra_skip_under(root: &Path, path: &Path) -> bool {
    crate::watcher_filter::watcher_path_is_infra_skip_under(root, path)
}

pub fn bounded_fallback_walk_files(
    filter_root: &Path,
    search_root: &Path,
    filters: &crate::search_index::PathFilters,
    tool: &'static str,
) -> crate::grep_executor::FallbackWalkOutcome {
    crate::grep_executor::bounded_fallback_walk_files(filter_root, search_root, filters, tool)
}

pub fn decompose_grep_pattern(
    pattern: &crate::pattern_compile::CompiledPattern,
) -> crate::search_index::RegexQuery {
    crate::search_index::decompose_grep_pattern(pattern)
}

pub fn keep_path_ordered_batch<T>(
    kept: &mut Vec<T>,
    max_results: usize,
    allowance: usize,
    per_file: Vec<(Vec<T>, bool)>,
    search_again: impl FnMut(usize, usize) -> (Vec<T>, bool),
    held: impl FnMut(usize),
) -> bool {
    crate::search_index::keep_path_ordered_batch(
        kept,
        max_results,
        allowance,
        per_file,
        search_again,
        held,
    )
}

pub fn bounded_grep_line_text_bytes(line: &[u8]) -> String {
    crate::search_index::bounded_grep_line_text_bytes(line)
}

pub mod edit_bash {
    pub use crate::edit::bash::{
        find_bash, reported_line, run_bash, with_runner, CheckOutcome, Runner, CHECK_TIMEOUT,
        SYNTAX_ERROR_STATUS,
    };
}

pub mod edit_swift {
    pub use crate::edit::swift::{
        cached_lookup, parse_outcome, parse_with_lookup, run_command, with_runner, LookupOutcome,
        ParseOutcome, Runner, BROKEN, TUPLE,
    };
}

pub mod executor_test_support {
    pub use crate::executor::test_support::test_executor;
}

pub use crate::edit::mutation::set_before_check;
pub use crate::grep_executor::{fallback_grep_with_limits, grep_test_fixture::literal};

pub fn build_display_metadata(
    display_files: &[crate::hashline::integration::DisplayFileBytes],
    project_root: Option<&Path>,
) -> (String, Vec<serde_json::Value>, crate::edit::DiffBudget) {
    crate::hashline::integration::build_display_metadata(display_files, project_root)
}

pub fn diff_budget_remaining(budget: &crate::edit::DiffBudget) -> usize {
    budget.remaining
}

pub fn set_diff_budget_remaining(budget: &mut crate::edit::DiffBudget, remaining: usize) {
    budget.remaining = remaining;
}

pub fn diff_budget_omitted(budget: &crate::edit::DiffBudget) -> bool {
    budget.omitted
}

pub struct BoundedDiffText(crate::edit::BoundedDiffText);

impl BoundedDiffText {
    pub fn new(limit: usize) -> Self {
        Self(crate::edit::BoundedDiffText {
            text: String::new(),
            limit,
        })
    }

    pub fn text(&self) -> &String {
        &self.0.text
    }
}

impl std::fmt::Write for BoundedDiffText {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        std::fmt::Write::write_str(&mut self.0, text)
    }
}

pub mod watcher;

#[derive(Debug, PartialEq, Eq)]
pub enum RootIdentity {
    Path,
    Git {
        root_commit: String,
        toplevel: PathBuf,
        relative: Option<String>,
    },
}

pub fn read_root_identity(root: &Path, storage: &Path) -> std::io::Result<Option<RootIdentity>> {
    use crate::root_identity::Identity;
    crate::root_identity::read(root, storage).map(|identity| {
        identity.map(|value| match value {
            Identity::Path => RootIdentity::Path,
            Identity::Git {
                root_commit,
                toplevel,
                relative,
            } => RootIdentity::Git {
                root_commit,
                toplevel,
                relative,
            },
        })
    })
}

pub const PROVISIONAL_IDENTITY_EXHAUSTED_REASON: &str =
    crate::root_identity::key::PROVISIONAL_IDENTITY_EXHAUSTED_REASON;
pub const GIT_IDENTITY_UNAVAILABLE_REASON: &str =
    crate::root_identity::key::GIT_IDENTITY_UNAVAILABLE_REASON;

pub fn artifact_key_from_git_identity(root_commit: &str) -> String {
    crate::root_identity::key::artifact_key_from_git_identity(root_commit)
}

pub fn share_lane_pause(root: &Path, pause: &crate::synapse_embed::LanePause) {
    crate::synapse_embed::share_lane_pause(root, pause);
}

pub fn record_tier2_attempt_outcome_for_test(
    manager: &crate::inspect::InspectManager,
    category: crate::inspect::InspectCategory,
    outcome: crate::inspect::JobOutcome,
) {
    manager.record_tier2_attempt_outcome_for_test(category, outcome);
}

pub use crate::inspect::cache::foreign_writer_fixture::{foreign_writer, foreign_writer_process};

#[cfg(unix)]
pub mod lsp_startup {
    pub use crate::lsp::manager::startup_fixture::{
        attempt_reason_for_test, fixture, locked, pause_first_spawn, WAIT,
    };

    pub fn install_client(
        ctx: &crate::context::AppContext,
        kind: crate::lsp::registry::ServerKind,
        root: std::path::PathBuf,
        binary: &std::path::Path,
        args: &[String],
        note: Option<&str>,
    ) -> std::io::Result<()> {
        let mut client = crate::lsp::client::LspClient::spawn_with_reclaim_root(
            kind,
            root,
            binary,
            args,
            &std::collections::HashMap::new(),
            ctx.lsp().event_sender_for_test(),
            crate::lsp::child_registry::LspChildRegistry::new(),
            None,
            false,
        )?;
        client.runtime_note = note.map(str::to_owned);
        ctx.lsp().insert_client_for_test(client);
        Ok(())
    }
}

pub mod inspect;
pub mod semantic;

pub fn language_id_for_path(path: &Path, source: Option<&str>) -> &'static str {
    crate::lsp::manager::language_id_for_path(path, source)
}

pub mod rerank;
pub use crate::search_state::request_files::read_probe::ReadProbe;

pub fn without_request_files_for_test<R>(run: impl FnOnce() -> R) -> R {
    crate::search_state::request_files::without_request_files_for_test(run)
}

pub fn request_files(
    guard: &crate::search_state::request_files::Guard,
) -> &std::sync::Arc<crate::search_state::request_files::RequestFiles> {
    guard.files()
}

pub fn set_grep_scan_order(root: &Path, reversed: bool) {
    crate::grep_executor::scan_order_for_test::set(root, reversed);
}

pub fn take_path_order_held_rows(root: &Path) -> usize {
    crate::search_index::path_order_held_rows_for_test::take(root)
}

#[cfg(unix)]
pub fn grep_root_for_test(
    ctx: &crate::context::AppContext,
    search_root: &Path,
    pattern: &str,
) -> crate::search_index::GrepResult {
    crate::grep_executor::grep_root_for_test(ctx, search_root, pattern)
}

pub fn record_request_file_read(path: &Path) {
    crate::search_state::request_files::record_disk_read(path);
}

pub fn keep_request_file(
    guard: &crate::search_state::request_files::Guard,
    path: &Path,
    bytes: Vec<u8>,
    stat: Option<crate::search_state::request_files::FileStat>,
) -> std::sync::Arc<crate::search_state::request_files::RequestFile> {
    guard.files().keep_read(path, bytes, stat)
}

pub mod output;
pub mod search;
pub use crate::edit::{
    A03_DIFF_CONSTRUCTIONS, A03_LINE_INDEX_PASSES, FORCE_MODE_RESTORE_FAILURE,
    LINE_OFFSET_BYTES_SCANNED,
};

pub fn session_call_counts(
    breaker: &crate::repeat_state::RepeatBreaker,
) -> std::collections::HashMap<String, usize> {
    breaker.session_call_counts()
}

pub trait BackupStoreTest {
    fn add_created_dir_for_test(&mut self, session: &str, key: &Path, dir: PathBuf);
    fn rekey_for_test(&mut self, session: &str, from: &Path, to: PathBuf);
}

impl BackupStoreTest for crate::backup::BackupStore {
    fn add_created_dir_for_test(&mut self, session: &str, key: &Path, dir: PathBuf) {
        self.add_created_dir_for_test(session, key, dir);
    }

    fn rekey_for_test(&mut self, session: &str, from: &Path, to: PathBuf) {
        self.rekey_for_test(session, from, to);
    }
}
