#[test]
fn configured_context_and_filter_thread_share_root_counters() {
    let tmp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let ctx = crate::context::AppContext::new(
        crate::context::default_language_provider_factory(),
        crate::config::Config::default(),
    );
    ctx.update_config(|config| config.project_root = Some(root.clone()));
    let config = WatcherFilterConfig::new(root, None);

    config.note_raw_event_for_test();

    assert_eq!(ctx.watcher_counters().snapshot().raw_events_total, 1);
}
use crate::watcher_filter::WatcherFilterConfig;
use tempfile::TempDir;

#[test]
fn watcher_full_dispatch_channel_holds_and_merges_batches_without_blocking() {
    crate::watcher_filter::test_helpers::verify_full_dispatch_backlog(|root, rx| {
        let ctx = crate::context::AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config::default(),
        );
        ctx.update_config(|config| config.project_root = Some(root.to_path_buf()));
        *ctx.watcher_rx().lock() = Some(rx);
        Box::new(move || ctx.watcher_query_has_pending_changes())
    });
}
