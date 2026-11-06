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



#[cfg(target_os = "linux")]
#[test]
fn watcher_cookie_delayed_delivery_past_read_inotify() {
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

