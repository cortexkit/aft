use super::*;

fn fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Arc<CheckoutDriver>,
    ViewAccess,
    crate::watcher_backend::subscription::Subscription,
) {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.rs"), b"fn content() {}\n").unwrap();
    let subscription =
        crate::watcher_backend::subscription::start(&std::fs::canonicalize(root.path()).unwrap());
    let policy = crate::blob_store::v2::TrigramPolicy {
        max_file_size: 1 << 20,
    };
    let driver = Arc::new(
        CheckoutDriver::new_v3(
            storage.path(),
            "family",
            "worker",
            root.path(),
            Producers {
                trigram: policy.fingerprint_hex(),
                semantic: None,
                callgraph: "unregistered".into(),
            },
            Arc::new(ConfiguredMembershipWalker),
            vec![Arc::new(TrigramBridge::new(
                storage.path().to_owned(),
                policy,
            ))],
        )
        .unwrap(),
    );
    let access = driver.access();
    let (snapshot, revision) = driver
        .verified_bind(&access, driver.installed_snapshot().generation().clone())
        .unwrap();
    driver.install(&access, &snapshot, revision).unwrap();
    driver.take_reconcile_work();
    (root, storage, driver, access, subscription)
}

#[test]
fn completed_strict_reconcile_recovers_the_overflow_epoch_it_covered() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let (_root, _storage, driver, access, _subscription) = fixture();
    driver.record_watcher_overflow();
    let epoch = driver.live.lock().unwrap().epoch();
    let (snapshot, revision) = driver
        .verified_bind(&access, driver.installed_snapshot().generation().clone())
        .unwrap();
    assert_eq!(driver.take_reconcile_work().walks, 1);
    assert_eq!(snapshot.reconciled_epoch(), Some(epoch));
    assert_eq!(
        driver.live.lock().unwrap().watcher(),
        super::super::snapshot::WatcherState::Overflowed
    );
    driver.install(&access, &snapshot, revision).unwrap();
    assert_eq!(
        driver.live.lock().unwrap().watcher(),
        super::super::snapshot::WatcherState::Healthy
    );
}

#[test]
fn overflow_during_strict_reconcile_cannot_be_cleared_by_its_older_cut() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let (_root, _storage, driver, access, _subscription) = fixture();
    driver.record_watcher_overflow();
    let covered = driver.live.lock().unwrap().epoch();
    driver.observed.lock().unwrap().stats.clear();
    let weak = Arc::downgrade(&driver);
    *driver.read_hook.lock().unwrap() = Some(Box::new(move |_, _| {
        weak.upgrade().unwrap().record_watcher_overflow()
    }));
    let (snapshot, revision) = driver
        .verified_bind(&access, driver.installed_snapshot().generation().clone())
        .unwrap();
    assert_eq!(driver.take_reconcile_work().walks, 1);
    assert_eq!(snapshot.reconciled_epoch(), Some(covered));
    assert!(driver.live.lock().unwrap().epoch() > covered);
    assert!(driver.install(&access, &snapshot, revision).is_err());
    assert_eq!(
        driver.live.lock().unwrap().watcher(),
        super::super::snapshot::WatcherState::Overflowed
    );
}
