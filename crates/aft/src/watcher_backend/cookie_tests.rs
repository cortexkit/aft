//! Backend delivery shim: the native producer is held while the real filter and
//! driver drain remain live. This tests application ordering, not kernel coverage.
use crate::config::Config;
use crate::context::{default_language_provider_factory, AppContext};
use crate::runtime_drain::{drain_watcher_events_bounded, drain_watcher_to_completion_for_test};
use crate::views::contracts::ViewAccess;
use crate::views::first_load::{CheckoutDriver, ConfiguredMembershipWalker, FirstLoadDriver};
use crate::views::manifest_v2::Producers;
use crate::views::registry::FamilyRegistry;
use crate::views::snapshot::WatcherState;
use crate::views::RelPath;
use crate::watcher_filter::{
    run_watcher_thread, watcher_dispatch_channel, WatcherDispatchEvent, WatcherFilterConfig,
};
use notify::{event::CreateKind, event::DataChange, event::ModifyKind, Event, EventKind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::time::Duration;

fn driver(root: &Path, storage: &Path) -> (Arc<CheckoutDriver>, ViewAccess) {
    let registry = FamilyRegistry::open(storage, "family").unwrap();
    let owner = registry
        .register_view_with_storage(registry.storage(), "cookie", root)
        .unwrap();
    let access = ViewAccess::Owner(owner.clone());
    (
        Arc::new(CheckoutDriver::new(
            owner,
            Producers {
                trigram: "unused".into(),
                semantic: None,
                callgraph: "unused".into(),
            },
            None,
            Arc::new(ConfiguredMembershipWalker),
            vec![],
        )),
        access,
    )
}

fn context(root: &Path, driver: &Arc<CheckoutDriver>) -> AppContext {
    let ctx = AppContext::new(default_language_provider_factory(), Config::default());
    ctx.update_config(|config| config.project_root = Some(root.to_path_buf()));
    ctx.set_canonical_cache_root(root.to_path_buf());
    ctx.mark_subc_bound();
    ctx.install_checkout_driver(Arc::clone(driver));
    ctx
}

fn delayed_delivery_past_read() {
    let temp = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let changed = root.join("source.rs");
    std::fs::write(&changed, b"old bytes").unwrap();
    let (driver, _) = driver(&root, storage.path());
    let ctx = context(&root, &driver);
    let (dispatch_tx, dispatch_rx) = watcher_dispatch_channel();
    *ctx.watcher_rx().lock() = Some(dispatch_rx.clone());
    let shutdown = Arc::new(AtomicBool::new(false));
    let (attached_tx, attached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (script_tx, script_rx) = mpsc::channel::<Vec<Event>>();
    let config = WatcherFilterConfig::new(root.clone(), None);
    let thread_shutdown = Arc::clone(&shutdown);
    let filter = std::thread::spawn(move || {
        run_watcher_thread(
            config,
            vec![],
            Arc::new(RwLock::new(None)),
            Arc::new(AtomicU64::new(0)),
            dispatch_tx,
            thread_shutdown,
            move |_, _, raw_tx| {
                // Holding at the backend handoff, not at the reader, reproduces
                // a completed write whose invalidation arrives after the read.
                let shim = std::thread::spawn(move || {
                    attached_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    for event in script_rx.recv().unwrap() {
                        raw_tx.send(Ok(event)).unwrap();
                    }
                    // Keep the raw stream connected until the filter stops.
                    raw_tx
                });
                Ok::<_, String>(shim)
            },
        );
    });
    attached_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let read = std::fs::read(&changed).unwrap();
    std::fs::write(&changed, b"new bytes").unwrap();
    assert_eq!(std::fs::read(&changed).unwrap(), b"new bytes");
    let ticket = driver.begin_watcher_cookie().unwrap();
    let cookie_path = ticket.path().to_path_buf();
    script_tx
        .send(vec![
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
                .add_path(changed.clone()),
            Event::new(EventKind::Create(CreateKind::File)).add_path(cookie_path.clone()),
        ])
        .unwrap();
    drain_watcher_events_bounded(&ctx, 1);
    assert_eq!(read, b"old bytes");
    assert_eq!(
        ticket.wait_timeout(Duration::ZERO),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    assert!(driver
        .installed_snapshot()
        .pending_intent()
        .next()
        .is_none());
    release_tx.send(()).unwrap();

    // Peek by consuming then restoring the complete prefix. This is a bounded
    // handshake, not a timing assumption about how fast the worker is scheduled.
    let first = dispatch_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = dispatch_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(&first, WatcherDispatchEvent::ContentOnlyPaths(paths) if paths == &vec![changed.clone()])
    );
    assert_eq!(
        second,
        WatcherDispatchEvent::CookieObserved(cookie_path.clone())
    );
    assert!(
        !cookie_path.exists(),
        "unlink must precede driver acknowledgment"
    );
    assert_eq!(
        ticket.wait_timeout(Duration::ZERO),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(first).unwrap();
    tx.send(second).unwrap();
    *ctx.watcher_rx().lock() = Some(rx);
    drain_watcher_to_completion_for_test(&ctx, 1);
    let observed = ticket.wait_timeout(Duration::ZERO).unwrap();
    assert!(observed.revision > 0);
    assert!(driver
        .installed_snapshot()
        .pending_intent()
        .any(|path| path == &RelPath::new("source.rs").unwrap()));
    assert!(!driver
        .installed_snapshot()
        .membership()
        .keys()
        .any(|path| path.as_bytes().starts_with(b".aft-cookie-")));
    shutdown.store(true, Ordering::SeqCst);
    filter.join().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn watcher_cookie_delayed_delivery_past_read_inotify() {
    delayed_delivery_past_read();
}

#[cfg(target_os = "macos")]
#[test]
fn watcher_cookie_delayed_delivery_past_read_fsevents() {
    delayed_delivery_past_read();
}

#[cfg(windows)]
#[test]
fn watcher_cookie_delayed_delivery_past_read_windows() {
    delayed_delivery_past_read();
}

#[test]
fn watcher_cookie_is_excluded_from_walk_membership_before_delivery() {
    let temp = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::write(root.join("source.rs"), b"source").unwrap();
    let (driver, access) = driver(&root, storage.path());
    let ticket = driver.begin_watcher_cookie().unwrap();
    assert!(ticket.path().exists());
    let reconciled = driver.reconcile(&access).unwrap();
    assert_eq!(reconciled.entries.len(), 1);
    assert!(reconciled
        .entries
        .contains_key(&RelPath::new("source.rs").unwrap()));
    assert!(
        ticket.path().exists(),
        "only delivery or ticket cleanup unlinks the cookie"
    );
    assert!(!super::flush::FlushControl::default().observe(
        &Event::new(EventKind::Create(CreateKind::File)).add_path(ticket.path().to_path_buf())
    ));
}

#[test]
fn watcher_cookie_rescan_dispatch_marks_driver_and_survives_later_paths() {
    use crate::watcher_filter::RescanReason;
    let temp = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let (driver, access) = driver(&root, storage.path());
    let ctx = context(&root, &driver);
    let (tx, rx) = crossbeam_channel::unbounded();
    *ctx.watcher_rx().lock() = Some(rx);
    for reason in [
        RescanReason::Unknown,
        RescanReason::UserDropped,
        RescanReason::KernelDropped,
        RescanReason::BufferOverflow,
    ] {
        let before = driver.revision(&access);
        let ticket = driver.begin_watcher_cookie().unwrap();
        tx.send(WatcherDispatchEvent::RescanRequired(reason))
            .unwrap();
        tx.send(WatcherDispatchEvent::CookieObserved(
            ticket.path().to_path_buf(),
        ))
        .unwrap();
        drain_watcher_to_completion_for_test(&ctx, 1);
        let observed = ticket.wait_timeout(Duration::ZERO).unwrap();
        assert_eq!(observed.watcher, WatcherState::Overflowed);
        assert!(observed.revision > before);
        driver.record_absolute_change(&root.join("later.rs"));
        assert_eq!(
            driver.installed_snapshot().watcher(),
            WatcherState::Overflowed
        );
    }
}

#[test]
fn watcher_cookie_fences_paths_already_parked_in_an_apply_slice() {
    use crate::context::{WatcherDrainApplyPhase, WatcherDrainPhase, WatcherDrainSliceState};
    let temp = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let (driver, _) = driver(&root, storage.path());
    let ctx = context(&root, &driver);
    let mut state = WatcherDrainSliceState::new(
        ctx.configure_generation(),
        ctx.configure_content_generation(),
    );
    state.phase = WatcherDrainPhase::Apply {
        stage: WatcherDrainApplyPhase::PendingTier2,
        paths: [root.join("parked.rs")].into(),
        remaining: 1,
        oversized_inline_batch: false,
    };
    *ctx.watcher_drain_slice().lock() = Some(state);
    let (tx, rx) = crossbeam_channel::unbounded();
    *ctx.watcher_rx().lock() = Some(rx);
    let ticket = driver.begin_watcher_cookie().unwrap();
    tx.send(WatcherDispatchEvent::CookieObserved(
        ticket.path().to_path_buf(),
    ))
    .unwrap();
    drain_watcher_to_completion_for_test(&ctx, 1);
    assert!(ticket.wait_timeout(Duration::ZERO).unwrap().revision > 0);
    assert!(driver
        .installed_snapshot()
        .pending_intent()
        .any(|path| path == &RelPath::new("parked.rs").unwrap()));
}

#[test]
fn watcher_cookie_coalesced_rescan_gap_still_invalidates_the_current_driver() {
    use crate::watcher_filter::RescanReason;
    let temp = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let (driver, access) = driver(&root, storage.path());
    let ctx = context(&root, &driver);
    let (_tx, rx) = crossbeam_channel::unbounded();
    *ctx.watcher_rx().lock() = Some(rx);
    ctx.watcher_counters().start_rescan();
    assert!(ctx
        .watcher_counters()
        .note_overflow(RescanReason::BufferOverflow, vec![]));
    let before = driver.revision(&access);
    drain_watcher_events_bounded(&ctx, 1);
    assert!(driver.revision(&access) > before);
    assert_eq!(
        driver.installed_snapshot().watcher(),
        WatcherState::Overflowed
    );
    assert_eq!(
        ctx.watcher_counters()
            .snapshot()
            .rescans_buffer_overflow_total,
        0
    );
    assert_eq!(
        ctx.watcher_counters().finish_rescan_walk(),
        Some(RescanReason::BufferOverflow)
    );
}
