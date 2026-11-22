//! Fixtures and observations for daemon tests that exercise the tool handlers.

mod bash;
mod external_search;
#[cfg(unix)]
mod remote;

#[cfg(unix)]
pub use bash::completed_spawn_for_test;
pub use bash::wait_fixture;
pub use external_search::external_search_fixture;
#[cfg(unix)]
pub use remote::{handle_with_policy, rendered_status};

pub use crate::commands::configure::{
    configure_unit_records_for_root, gate_configure_commit_for_test,
    gate_configure_tail_stage_for_test, ConfigureTailStageGateGuard, ConfigureUnitRecord,
};
pub use crate::commands::delete_file::install_delete_gate_for_test;
pub use crate::commands::inspect::{
    deferred_inspect_root_count_for_test, deferred_inspect_test_lock,
    install_deferred_inspect_body_gate_for_request_for_test,
    install_deferred_inspect_body_gate_for_test, install_deferred_inspect_stat_gate_for_test,
};
pub use crate::commands::lsp_navigation::{
    deferred_navigation_test_lock, deferred_navigation_worker_count_for_test,
    install_deferred_navigation_gate_for_test, take_deferred_navigation_seen_restrict_for_test,
};
pub use crate::commands::semantic_search::direct_external_search_for_test;
#[cfg(unix)]
pub use crate::commands::url_resolution::with_secret_fetch_for_test;
pub use crate::commands::url_resolution::with_secret_fetch_test_hooks;
#[cfg(unix)]
pub use crate::commands::{
    glob::glob_root_files_for_test, outline::discover_outline_paths_for_test,
};
pub use crate::upper_test_context::{test_app, test_context};

/// Whether the checkout's text index is ready to answer a tool call.
pub fn text_index_is_ready(ctx: &crate::context::AppContext) -> bool {
    crate::commands::data_search::DataText::ready(ctx).is_some()
}

/// Whether the watch restart callback has been installed.
pub fn watch_restart_registered() -> bool {
    crate::commands::bash_watch::restart::registered()
}

/// Put an already constructed watch wait in the signal's waiting state.
pub fn install_watch_wait_fixture(
    signal: &crate::commands::bash_watch::YieldSignal,
    wait: crate::commands::bash_watch::restart::WatchWait,
) {
    crate::commands::bash_watch::restart::watch_waiting(signal, wait);
}

/// Render two ranked hits: one too large to send first, and a small successor.
pub fn ranked_oversized_first_response(
    request: &crate::protocol::RawRequest,
    root: &std::path::Path,
) -> crate::protocol::Response {
    use crate::commands::semantic_search::HybridResult;
    let first = HybridResult {
        file: root
            .join("oversized-first-result")
            .join("x".repeat(60 * 1024)),
        name: "first".into(),
        kind: crate::symbols::SymbolKind::FileSummary,
        start_line: 1,
        end_line: 1,
        exported: false,
        score: 1.0,
        source: "lexical",
        semantic_score: None,
        lexical_score: Some(1.0),
        hybrid_boosted: false,
        exact: true,
        exact_phrase_count: 1,
        exact_window_lines: None,
        fusion_score: 1.0,
        snippet: "first match".into(),
    };
    let second = HybridResult {
        file: root.join("second.rs"),
        name: "second".into(),
        ..first.clone()
    };
    crate::commands::semantic_search::ranked_response_for_test(request, root, vec![first, second])
}
