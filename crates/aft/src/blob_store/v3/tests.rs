use super::*;
use std::sync::{MutexGuard, OnceLock};
use tempfile::tempdir;

const A: &str = "0123456789abcdef";
const B: &str = "fedcba9876543210";
pub(super) fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    set_free_space_for_test(u64::MAX, u64::MAX / 2);
    guard
}
fn key() -> FamilyKey {
    FamilyKey::new(FamilyPlane::Semantic, [7; 32])
}
fn seed(storage: &Path, family: &str) {
    publish(storage, family, 1024, None, |p| {
        p.put(&key(), b"v3 data", 1)
    })
    .unwrap();
}

#[test]
fn put_batch_keeps_conflict_winners_and_commits_other_rows_in_one_transaction() {
    let _guard = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let next = FamilyKey::new(FamilyPlane::Semantic, [8; 32]);
    let rows = vec![
        (key(), b"different".to_vec()),
        (next, b"new row".to_vec()),
        (key(), b"v3 data".to_vec()),
        (next, b"new row".to_vec()),
        (next, b"different new row".to_vec()),
    ];
    work_profile::take();
    let outcomes = publish(root.path(), A, 1 << 20, None, |p| {
        p.put_or_touch_batch(FamilyPlane::Semantic, &rows, 1)
    })
    .unwrap();
    assert!(matches!(
        outcomes[0],
        Err(Error::Refused("payload_mismatch"))
    ));
    assert!(matches!(outcomes[1], Ok(true)));
    assert!(matches!(outcomes[2], Ok(false)));
    assert!(matches!(outcomes[3], Ok(false)));
    assert!(matches!(
        outcomes[4],
        Err(Error::Refused("payload_mismatch"))
    ));
    assert_eq!(work_profile::count("put_transaction"), 1);
    let reader = PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic).unwrap();
    assert_eq!(reader.get(&key()).unwrap().unwrap(), b"v3 data");
    assert_eq!(reader.get(&next).unwrap().unwrap(), b"new row");
}

#[test]
fn publication_survives_a_sidecar_disappearing_during_accounting() {
    let _guard = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    // A detached sidecar stands in for one removed by a closing SQLite handle.
    // Do not unlink a sidecar that an active database connection still uses.
    let sidecar = family_dir(root.path(), A)
        .unwrap()
        .join("closed.sqlite-wal");
    fs::write(&sidecar, b"transient WAL bytes").unwrap();
    let removed = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = std::rc::Rc::clone(&removed);
    WALK_BEFORE_STAT.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |path| {
            if path == sidecar {
                fs::remove_file(path).unwrap();
                observed.set(true);
            }
        }));
    });
    let chunk = FamilyKey::new(FamilyPlane::Semantic, [8; 32]);
    let run = FamilyKey::new(FamilyPlane::Semantic, [9; 32]);
    let rows = vec![(chunk, b"chunk".to_vec()), (run, b"run".to_vec())];
    let result = publish(root.path(), A, 1 << 20, None, |publication| {
        publication.put_or_touch_batch(FamilyPlane::Semantic, &rows, 1)
    });
    WALK_BEFORE_STAT.with(|hook| *hook.borrow_mut() = None);

    assert!(
        removed.get(),
        "the accounting walk did not reach the sidecar"
    );
    let outcomes = result.unwrap();
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes
        .into_iter()
        .all(|outcome| matches!(outcome, Ok(true))));
    let reader = PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic).unwrap();
    assert_eq!(reader.get(&chunk).unwrap().unwrap(), b"chunk");
    assert_eq!(reader.get(&run).unwrap().unwrap(), b"run");
}
#[cfg(unix)]
pub(super) fn inventory(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                inventory(&path)
            } else {
                meta.blocks() * 512
            }
        })
        .sum()
}
fn db(storage: &Path, family: &str) -> PathBuf {
    family_dir(storage, family).unwrap().join("members.sqlite")
}

#[test]
fn floor_refusal_before_creation_and_budget_recheck_leave_no_plane_or_row() {
    let _guard = serial();
    let root = tempdir().unwrap();
    set_free_space_for_test(NAMESPACE_BUDGET - 1, 8 * NAMESPACE_BUDGET);
    let error = publish(root.path(), A, 1, None, |_| Ok(())).unwrap_err();
    assert_eq!(error.to_string(), "device_free_space_floor");
    assert!(!root.path().join("blobs/v3").exists());
    assert!(!root.path().join("reader-floor.json").exists());
    set_free_space_for_test(u64::MAX, u64::MAX / 2);
    let error = publish(root.path(), A, NAMESPACE_BUDGET, None, |_| -> Result<()> {
        panic!("budget must refuse before body")
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "v3_namespace_budget");
    let ledger = LedgerReader::open_existing(root.path(), A).unwrap();
    assert_eq!(ledger.accounting().unwrap().reserved_bytes, 0);
    assert_eq!(
        ledger
            .database
            .connection()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM reservations", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert!(!plane_path(root.path(), A, FamilyPlane::Semantic)
        .unwrap()
        .exists());
}

struct Recheck;
impl DurabilityObserver for Recheck {
    fn reached(&self, step: DurabilityStep) {
        if step == DurabilityStep::V3FloorRaised {
            set_free_space_for_test(NAMESPACE_BUDGET, 8 * NAMESPACE_BUDGET);
        }
    }
}
#[test]
fn floor_recheck_refuses_without_reservation_or_plane_store() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let error = publish(root.path(), A, 1, Some(&Recheck), |_| -> Result<()> {
        panic!("floor must refuse")
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "device_free_space_floor");
    assert!(db(root.path(), A).exists());
    let ledger = LedgerReader::open_existing(root.path(), A).unwrap();
    assert_eq!(
        ledger
            .database
            .connection()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM reservations", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert!(!plane_path(root.path(), A, FamilyPlane::Semantic)
        .unwrap()
        .exists());
}

#[test]
fn sequential_reservations_sum_other_family_without_mutating_it() {
    let _guard = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut a = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let reservation = a.reserve(100).unwrap();
    let before = a.accounting().unwrap();
    set_free_space_for_test(NAMESPACE_BUDGET + 150, 8 * NAMESPACE_BUDGET);
    let started = std::time::Instant::now();
    let error = publish_locked(root.path(), B, 100, &lock, None, |_| Ok(())).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(error.to_string(), "device_free_space_floor");
    assert_eq!(a.accounting().unwrap(), before);
    assert_eq!(
        LedgerReader::open_existing(root.path(), A)
            .unwrap()
            .accounting()
            .unwrap(),
        before
    );
    a.release(reservation).unwrap();
}

#[test]
fn unreadable_peer_ledger_refuses_instead_of_ignoring_reserved_bytes() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let dir = family_dir(root.path(), A).unwrap();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("members.sqlite"), b"not sqlite").unwrap();
    let error = publish(root.path(), B, 100, None, |_| Ok(())).unwrap_err();
    assert_eq!(error.to_string(), "reservation_ledger_unreadable");
    assert!(!db(root.path(), B).exists());
}

#[test]
fn publication_observes_floor_before_files_and_flush_before_commit() {
    let _guard = serial();
    let root = tempdir().unwrap();
    struct Observer<'a>(&'a Path, std::cell::RefCell<Vec<DurabilityStep>>);
    impl DurabilityObserver for Observer<'_> {
        fn reached(&self, step: DurabilityStep) {
            self.1.borrow_mut().push(step);
            match step {
                DurabilityStep::V3FloorRaised => {
                    assert!(!self.0.join("blobs/v3").exists());
                    let floor = fs::read_to_string(self.0.join("reader-floor.json")).unwrap();
                    assert!(floor.contains("views_v3_registry"));
                    assert!(floor.contains("views_v3_store"));
                }
                DurabilityStep::ClosureFlushed => {
                    let ledger = LedgerReader::open_existing(self.0, A).unwrap();
                    assert!(ledger.accounting().unwrap().reserved_bytes > 0);
                    assert_eq!(
                        PlaneReader::open_existing(self.0, A, FamilyPlane::Semantic)
                            .unwrap()
                            .get(&key())
                            .unwrap(),
                        Some(b"v3 data".to_vec())
                    );
                }
                DurabilityStep::BlobCommitted
                | DurabilityStep::SegmentRowRecorded
                | DurabilityStep::SegmentFileSynced
                | DurabilityStep::SegmentDurable
                | DurabilityStep::ManifestWritten
                | DurabilityStep::ManifestParentSynced
                | DurabilityStep::PointerCas
                | DurabilityStep::PointerSynced => panic!("v2 boundary in v3 publication"),
                other => panic!("base-reference boundary {other:?} in a copy publication"),
            }
        }
    }
    let observer = Observer(root.path(), std::cell::RefCell::new(Vec::new()));
    publish(root.path(), A, 1024, Some(&observer), |p| {
        p.put(&key(), b"v3 data", 1)
    })
    .unwrap();
    assert_eq!(
        *observer.1.borrow(),
        vec![
            DurabilityStep::V3FloorRaised,
            DurabilityStep::ClosureFlushed
        ]
    );
    for step in DurabilityStep::V3_ONLY {
        assert!(DurabilityStep::ALL.contains(&step));
        assert_eq!(DurabilityStep::parse(step.as_str()), Some(step));
    }
    assert_eq!(
        DurabilityStep::parse("v3-floor-raised"),
        Some(DurabilityStep::V3FloorRaised)
    );
    assert_eq!(
        DurabilityStep::parse("closure-flushed"),
        Some(DurabilityStep::ClosureFlushed)
    );
    assert!(!root.path().join("blobs/v2").exists());
}

#[cfg(unix)]
#[test]
fn accounting_matches_independent_inventory_after_twenty_reuse_cycles() {
    let _guard = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    for _ in 0..20 {
        publish(root.path(), A, 1024, None, |p| p.put(&key(), b"v3 data", 1)).unwrap();
        let lock = ReservationLock::acquire(root.path()).unwrap();
        let writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
        assert_eq!(
            writer.accounting().unwrap().usage(),
            inventory(&family_dir(root.path(), A).unwrap())
        );
    }
}

#[cfg(unix)]
#[test]
fn registry_dominated_growth_is_charged_and_deletion_waits_for_measurement() {
    let _guard = serial();
    let root = tempdir().unwrap();
    publish(root.path(), A, 2 * 1024 * 1024, None, |p| {
        for i in 0..1000 {
            p.registry
                .register_member(&format!("scope-{i}"), "base", None)?;
        }
        Ok(())
    })
    .unwrap();
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let baseline = writer.accounting().unwrap().usage();
    assert!(baseline > 100_000);
    assert_eq!(baseline, inventory(&writer.dir));
    let file = writer.dir.join("garbage");
    fs::write(&file, vec![3; 1024 * 1024]).unwrap();
    writer.covering_measurement().unwrap();
    let larger = writer.accounting().unwrap().usage();
    assert_eq!(larger, inventory(&writer.dir));
    fs::remove_file(file).unwrap();
    assert_eq!(writer.accounting().unwrap().usage(), larger);
    writer.covering_measurement().unwrap();
    assert_eq!(writer.accounting().unwrap().usage(), baseline);
}

#[cfg(unix)]
#[test]
fn failed_plane_commit_releases_reservation_but_keeps_orphan_charge() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let error = publish(root.path(), A, 2 * 1024 * 1024, None, |p| {
        p.put(&key(), &vec![5; 1024 * 1024], 1)?;
        Err::<(), _>(Error::Refused("injected_failure"))
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "injected_failure");
    let released = LedgerReader::open_existing(root.path(), A)
        .unwrap()
        .accounting()
        .unwrap();
    assert_eq!(released.reserved_bytes, 0);
    assert!(released.usage() >= inventory(&family_dir(root.path(), A).unwrap()));
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let usage = writer.accounting().unwrap();
    assert_eq!(usage.reserved_bytes, 0);
    assert_eq!(usage.usage(), inventory(&writer.dir));
    assert!(usage.usage() >= 1024 * 1024);
    let error = publish_locked(
        root.path(),
        A,
        NAMESPACE_BUDGET - 1024 * 1024,
        &lock,
        None,
        |_| Ok(()),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "v3_namespace_budget");
    assert_eq!(
        PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic)
            .unwrap()
            .get(&key())
            .unwrap()
            .unwrap()
            .len(),
        1024 * 1024
    );
}

fn check_drift(sql: &str, expected: &str) {
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let path = db(root.path(), A);
    let c = Connection::open(&path).unwrap();
    c.execute_batch(sql).unwrap();
    drop(c);
    let error = LedgerReader::open_existing(root.path(), A)
        .err()
        .expect("must refuse");
    assert!(error.to_string().ends_with(expected), "{error}");
    assert!(UnsupportedPersistedFormat::is_refusal_message(
        &error.to_string()
    ));
}
#[test]
fn stored_schema_drift_is_refused() {
    let _g = serial();
    check_drift("UPDATE format_meta SET schema_text='drift'", "schema_drift");
}
#[test]
fn added_schema_object_is_refused() {
    let _g = serial();
    check_drift("CREATE VIEW extra AS SELECT 1", "schema_drift");
}
#[test]
fn application_id_swap_is_refused() {
    let _g = serial();
    check_drift(
        &format!("PRAGMA application_id={PLANE_APPLICATION_ID}"),
        "application_id",
    );
}
#[test]
fn unknown_layout_tag_is_refused() {
    let _g = serial();
    check_drift("UPDATE format_meta SET layout_tag=99", "layout_tag:99");
}
#[test]
fn pragma_mismatch_is_refused_by_name() {
    let _g = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    BAD_PRAGMA.with(|bad| bad.set(Some("query_only")));
    let error = LedgerReader::open_existing(root.path(), A)
        .err()
        .expect("must refuse");
    BAD_PRAGMA.with(|bad| bad.set(None));
    assert!(error.to_string().ends_with("pragma:query_only"), "{error}");
}
#[test]
fn drift_refusal_is_cleared_after_valid_replacement_and_never_becomes_floor() {
    let _g = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let path = db(root.path(), A);
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE VIEW extra AS SELECT 1")
        .unwrap();
    let old = LedgerReader::open_existing(root.path(), A)
        .err()
        .unwrap()
        .to_string();
    assert!(persisted_format::refusal_covering(PersistedStore::ViewsV3Registry, &path).is_some());
    assert_eq!(crate::db::file_identity::open_connections(&path), 0);
    for name in ["members.sqlite", "members.sqlite-wal", "members.sqlite-shm"] {
        match fs::remove_file(path.with_file_name(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("{error}"),
        }
    }
    publish(root.path(), A, 0, None, |_| Ok(())).unwrap();
    LedgerReader::open_existing(root.path(), A).unwrap();
    assert!(persisted_format::refusal_covering(PersistedStore::ViewsV3Registry, &path).is_none());
    assert!(UnsupportedPersistedFormat::is_refusal_message(&old));
}

#[cfg(unix)]
#[test]
fn drift_refusal_covers_an_unresolved_symlinked_storage_parent() {
    let _g = serial();
    let root = tempdir().unwrap();
    let target = root.path().join("target");
    let alias = root.path().join("storage-alias");
    fs::create_dir_all(target.join("nested")).unwrap();
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let storage = alias.join("nested").join("..");
    seed(&storage, A);
    let path = db(&storage, A);
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE VIEW extra AS SELECT 1")
        .unwrap();
    let refusal = LedgerReader::open_existing(&storage, A).err().unwrap();
    let recorded = persisted_format::refusal_covering(PersistedStore::ViewsV3Registry, &path)
        .expect("a production refusal must cover the caller's unresolved path");
    assert_eq!(recorded.path, fs::canonicalize(&path).unwrap());
    assert_eq!(refusal.to_string(), recorded.to_string());
}

fn child(storage: &Path, family: &str, mode: &str, control: &Path) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "blob_store::v3::tests::v3_child", "--nocapture"])
        .env("AFT_V3_TEST_STORAGE", storage)
        .env("AFT_V3_TEST_FAMILY", family)
        .env("AFT_V3_TEST_MODE", mode)
        .env("AFT_V3_TEST_CONTROL", control)
        .env("AFT_V3_TEST_ADMISSION_CONTROL", control.parent().unwrap())
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}
/// Writes a child's control file whole. A plain `fs::write` creates the file
/// before writing its contents, and the parent waits only for the file to
/// exist, so it could read an empty outcome. Renaming a finished temporary
/// file into place makes existence imply complete contents.
fn write_control(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    fs::write(&partial, contents)?;
    fs::rename(&partial, path)
}
fn wait_file(path: &Path, child: &mut std::process::Child) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before {}",
            path.display()
        );
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("child never reached {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn v3_child() {
    let Some(storage) = std::env::var_os("AFT_V3_TEST_STORAGE") else {
        return;
    };
    let storage = PathBuf::from(storage);
    let family = std::env::var("AFT_V3_TEST_FAMILY").unwrap();
    let mode = std::env::var("AFT_V3_TEST_MODE").unwrap();
    let control = PathBuf::from(std::env::var_os("AFT_V3_TEST_CONTROL").unwrap());
    set_free_space_for_test(u64::MAX, u64::MAX / 2);
    if mode.starts_with("init-") {
        publish(&storage, &family, 0, None, |p| {
            if mode.starts_with("init-plane-") {
                p.put(&key(), b"v3 data", 1)?;
            }
            Ok(())
        })
        .unwrap();
        panic!("initialization boundary was not reached: {mode}");
    }
    if mode == "abandoned-reservation" {
        let lock = ReservationLock::acquire(&storage).unwrap();
        let mut writer = RegistryWriter::open_existing(&storage, &family, &lock).unwrap();
        let _reservation = writer.reserve(100).unwrap();
        write_control(&control, b"reserved").unwrap();
        loop {
            std::thread::park();
        }
    }
    if mode == "wal-writer" {
        let lock = ReservationLock::acquire(&storage).unwrap();
        let mut writer = RegistryWriter::open_existing(&storage, &family, &lock).unwrap();
        let _reservation = writer.reserve(1234).unwrap();
        write_control(&control, b"uncheckpointed").unwrap();
        loop {
            std::thread::park();
        }
    }
    if matches!(
        mode.as_str(),
        "ledger-audit" | "ledger-held" | "ledger-transaction"
    ) {
        read_audit::enable();
        let ledger = LedgerReader::open_existing(&storage, &family).unwrap();
        if mode == "ledger-transaction" {
            ledger
                .database
                .connection()
                .unwrap()
                .execute_batch("BEGIN")
                .unwrap();
        }
        let accounting = ledger.accounting().unwrap();
        // SAFETY: the connection guard keeps the SQLite handle alive.
        let readonly = unsafe {
            rusqlite::ffi::sqlite3_db_readonly(
                ledger.database.connection().unwrap().handle(),
                c"main".as_ptr(),
            )
        };
        let before = read_audit::counts();
        write_control(
            &control,
            serde_json::to_vec(
                &serde_json::json!({"reserved":accounting.reserved_bytes,"readonly":readonly,"before":before}),
            )
            .unwrap(),
        )
        .unwrap();
        if mode != "ledger-audit" {
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !control.with_extension("release").exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "parent did not release ledger child"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        drop(ledger);
        write_control(&control,serde_json::to_vec(&serde_json::json!({"reserved":accounting.reserved_bytes,"readonly":readonly,"before":before,"after":read_audit::counts()})).unwrap()).unwrap();
        return;
    }
    if mode == "reserve" {
        set_free_space_for_test(NAMESPACE_BUDGET + 150, 8 * NAMESPACE_BUDGET);
        let result = (|| -> Result<()> {
            let rendezvous = control.parent().unwrap();
            fs::write(rendezvous.join(format!("started-{family}")), b"ready")?;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !(rendezvous.join(format!("started-{A}")).exists()
                && rendezvous.join(format!("started-{B}")).exists())
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "second reservation child did not start"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            let lock = ReservationLock::acquire(&storage)?;
            let mut writer = RegistryWriter::open_existing(&storage, &family, &lock)?;
            let reservation = writer.reserve(100)?;
            drop(writer);
            drop(lock);
            write_control(&control, b"reserved")?;
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !control.with_extension("release").exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "parent did not release reservation"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            let lock = ReservationLock::acquire(&storage)?;
            RegistryWriter::open_existing(&storage, &family, &lock)?.release(reservation)
        })();
        if let Err(error) = result {
            write_control(&control, error.to_string()).unwrap();
        }
        return;
    }
    if mode == "normal" {
        seed(&storage, &family);
        return;
    }
    struct Park<'a> {
        step: DurabilityStep,
        control: &'a Path,
    }
    impl DurabilityObserver for Park<'_> {
        fn reached(&self, step: DurabilityStep) {
            if step == self.step {
                write_control(self.control, step.as_str()).unwrap();
                loop {
                    std::thread::park();
                }
            }
        }
    }
    let observer = Park {
        step: DurabilityStep::parse(&mode).unwrap(),
        control: &control,
    };
    publish(&storage, &family, 2 * 1024 * 1024, Some(&observer), |p| {
        p.put(&key(), &vec![9; 1024 * 1024], 1)
    })
    .unwrap();
    panic!("child did not stop at its requested durability boundary");
}

#[test]
fn two_processes_reserving_in_different_families_have_only_one_winner() {
    let _g = serial();
    let root = tempdir().unwrap();
    let control = tempdir().unwrap();
    publish(root.path(), A, 0, None, |_| Ok(())).unwrap();
    publish(root.path(), B, 0, None, |_| Ok(())).unwrap();
    let ready_a = control.path().join("a");
    let ready_b = control.path().join("b");
    let mut a = child(root.path(), A, "reserve", &ready_a);
    let mut b = child(root.path(), B, "reserve", &ready_b);
    wait_file(&ready_a, &mut a);
    wait_file(&ready_b, &mut b);
    let outcomes = [
        fs::read_to_string(&ready_a).unwrap(),
        fs::read_to_string(&ready_b).unwrap(),
    ];
    fs::write(ready_a.with_extension("release"), b"go").unwrap();
    fs::write(ready_b.with_extension("release"), b"go").unwrap();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    assert_eq!(
        outcomes.iter().filter(|r| r.as_str() == "reserved").count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| r.as_str() == "device_free_space_floor")
            .count(),
        1,
        "{outcomes:?}"
    );
}

#[cfg(unix)]
#[test]
fn killed_v3_publishers_recover_at_both_new_boundaries_without_losing_orphans() {
    let _g = serial();
    for step in DurabilityStep::V3_ONLY {
        let root = tempdir().unwrap();
        let control = tempdir().unwrap();
        let ready = control.path().join("ready");
        let mut publisher = child(root.path(), A, step.as_str(), &ready);
        wait_file(&ready, &mut publisher);
        publisher.kill().unwrap();
        publisher.wait().unwrap();
        assert!(fs::read_to_string(root.path().join("reader-floor.json"))
            .unwrap()
            .contains("views_v3_store"));
        match step {
            DurabilityStep::V3FloorRaised => {
                assert!(!root.path().join("blobs/v3").exists());
                seed(root.path(), A);
            }
            DurabilityStep::ClosureFlushed => {
                let outstanding = LedgerReader::open_existing(root.path(), A)
                    .unwrap()
                    .accounting()
                    .unwrap();
                assert!(outstanding.reserved_bytes > 0);
                let lock = ReservationLock::acquire(root.path()).unwrap();
                let writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
                let accounting = writer.accounting().unwrap();
                assert_eq!(accounting.reserved_bytes, 0);
                assert_eq!(accounting.usage(), inventory(&writer.dir));
                assert!(accounting.usage() >= 1024 * 1024);
                let released: u64 = writer
                    .connection()
                    .unwrap()
                    .query_row(
                        "SELECT COUNT(*) FROM reservations WHERE state='released'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(released, 1);
                assert_eq!(
                    PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic)
                        .unwrap()
                        .get(&key())
                        .unwrap()
                        .unwrap(),
                    vec![9; 1024 * 1024]
                );
            }
            DurabilityStep::BlobCommitted
            | DurabilityStep::SegmentRowRecorded
            | DurabilityStep::SegmentFileSynced
            | DurabilityStep::SegmentDurable
            | DurabilityStep::ManifestWritten
            | DurabilityStep::ManifestParentSynced
            | DurabilityStep::PointerCas
            | DurabilityStep::PointerSynced => panic!("V3_ONLY contains a v2 boundary"),
            other => panic!("V3_ONLY contains the base-path boundary {other:?}"),
        }
    }
}

#[cfg(unix)]
#[test]
fn normal_child_exit_keeps_data_and_inventory_charge() {
    let _g = serial();
    let root = tempdir().unwrap();
    let control = tempdir().unwrap();
    assert!(child(root.path(), A, "normal", control.path())
        .wait()
        .unwrap()
        .success());
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    assert_eq!(writer.accounting().unwrap().usage(), inventory(&writer.dir));
    assert_eq!(
        PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic)
            .unwrap()
            .get(&key())
            .unwrap(),
        Some(b"v3 data".to_vec())
    );
}

#[test]
fn ledger_reads_share_live_owner_without_mutation_or_measurement() {
    let _g = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let reservation = writer.reserve(100).unwrap();
    let before = writer.accounting().unwrap();
    let changes = writer.connection().unwrap().total_changes();
    let ledger = LedgerReader::open_existing(root.path(), A).unwrap();
    assert!(Arc::ptr_eq(&ledger.database, &writer.database));
    assert_eq!(ledger.accounting().unwrap(), before);
    assert_eq!(writer.connection().unwrap().total_changes(), changes);
    assert_eq!(
        writer
            .connection()
            .unwrap()
            .pragma_query_value(None, "query_only", |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        crate::db::file_identity::open_connections(&db(root.path(), A)),
        1
    );
    drop(ledger);
    assert_eq!(
        crate::db::file_identity::open_connections(&db(root.path(), A)),
        1
    );
    let second = writer.reserve(1).unwrap();
    writer.release(second).unwrap();
    writer.release(reservation).unwrap();
}

pub(super) fn admission_hook() {
    let Some(control) = std::env::var_os("AFT_V3_TEST_ADMISSION_CONTROL") else {
        return;
    };
    let control = PathBuf::from(control);
    let family = std::env::var("AFT_V3_TEST_FAMILY").unwrap();
    fs::write(control.join(format!("checked-{family}")), b"checked").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while !(control.join(format!("checked-{A}")).exists()
        && control.join(format!("checked-{B}")).exists())
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn nested_writer_uses_existing_root_guard_and_does_not_touch_peer_ledger() {
    let _g = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let a = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let before = a.connection().unwrap().total_changes();
    let accounting = a.accounting().unwrap();
    let started = std::time::Instant::now();
    publish_locked(root.path(), B, 1024, &lock, None, |p| {
        p.put(&key(), b"v3 data", 1)
    })
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(a.accounting().unwrap(), accounting);
    assert_eq!(a.connection().unwrap().total_changes(), before);
}

#[test]
fn capacity_percentage_floor_applies_above_the_four_gib_minimum() {
    let _g = serial();
    let root = tempdir().unwrap();
    set_free_space_for_test(2 * NAMESPACE_BUDGET, 200 * NAMESPACE_BUDGET);
    assert_eq!(
        publish(root.path(), A, 1, None, |_| Ok(()))
            .unwrap_err()
            .to_string(),
        "device_free_space_floor"
    );
    assert!(!root.path().join("blobs/v3").exists());
}

#[cfg(unix)]
#[test]
fn committed_metadata_growth_is_charged_before_next_writer_open() {
    let _g = serial();
    let root = tempdir().unwrap();
    publish(root.path(), A, 2 * 1024 * 1024, None, |p| {
        for i in 0..1000 {
            p.registry
                .register_member(&format!("scope-{i}"), "view", None)?;
        }
        Ok(())
    })
    .unwrap();
    let accounting = LedgerReader::open_existing(root.path(), A)
        .unwrap()
        .accounting()
        .unwrap();
    assert!(
        accounting.committed_charges > 100_000,
        "metadata growth must replace the reservation: {accounting:?}"
    );
    assert!(accounting.usage() >= inventory(&family_dir(root.path(), A).unwrap()));
}

#[cfg(unix)]
#[test]
fn reclaimed_plane_pages_reduce_usage_only_at_the_next_covering_measurement() {
    let _g = serial();
    let root = tempdir().unwrap();
    // The existing reclaim policy deliberately retains free lists below 256 MiB.
    publish(root.path(), A, 300 * 1024 * 1024, None, |p| {
        p.put(&key(), &vec![7; 272 * 1024 * 1024], 1)
    })
    .unwrap();
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut registry = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let before = registry.accounting().unwrap().usage();
    let path = plane_path(root.path(), A, FamilyPlane::Semantic).unwrap();
    let c = open_db(&path, Kind::Plane(FamilyPlane::Semantic), false, false).unwrap();
    c.execute("DELETE FROM blob_payloads", []).unwrap();
    checkpoint(&c).unwrap();
    for _ in 0..16 {
        super::super::reclaim::reclaim(&c, None, &|| false).unwrap();
    }
    drop(c);
    assert_eq!(registry.accounting().unwrap().usage(), before);
    registry.covering_measurement().unwrap();
    let after = registry.accounting().unwrap().usage();
    assert_eq!(after, inventory(&registry.dir));
    let freed = before - after;
    assert!(freed > 0, "incremental reclaim must free allocated pages");
    let reservation = registry.reserve(NAMESPACE_BUDGET - before + freed).unwrap();
    registry.release(reservation).unwrap();
}

#[test]
fn newer_format_keeps_artifact_refusal_and_malformed_file_reports_found_zero() {
    let _g = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let path = db(root.path(), A);
    let future = REGISTRY_VERIFICATION_VERSION + 1;
    Connection::open(&path)
        .unwrap()
        // Keep testing a future registry, not drift within a supported version.
        .execute("UPDATE format_meta SET format_version=?1", [future])
        .unwrap();
    match LedgerReader::open_existing(root.path(), A).err().unwrap() {
        Error::Incompatible(error) => {
            assert_eq!(error.found, u64::from(future));
            assert_eq!(error.source, persisted_format::RefusalSource::Artifact);
        }
        error => panic!("{error}"),
    }
    fs::write(&path, b"not sqlite").unwrap();
    match LedgerReader::open_existing(root.path(), A).err().unwrap() {
        Error::Incompatible(error) => {
            assert_eq!(error.found, 0);
            assert_eq!(
                error.source,
                persisted_format::RefusalSource::Incompatible(IncompatibleReason::SchemaDrift)
            );
        }
        error => panic!("{error}"),
    }
}

#[test]
fn plane_open_enforces_the_same_schema_application_and_pragma_contract() {
    let _g = serial();
    for (sql, reason) in [
        ("CREATE VIEW extra AS SELECT 1", "schema_drift"),
        ("UPDATE format_meta SET schema_text='wrong'", "schema_drift"),
        ("PRAGMA application_id=1095128146", "application_id"),
        ("UPDATE format_meta SET layout_tag=88", "layout_tag:88"),
    ] {
        let root = tempdir().unwrap();
        seed(root.path(), A);
        let path = plane_path(root.path(), A, FamilyPlane::Semantic).unwrap();
        Connection::open(&path).unwrap().execute_batch(sql).unwrap();
        let error = PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic)
            .err()
            .expect("plane contract must refuse");
        assert!(error.to_string().ends_with(reason), "{error}");
    }
    let root = tempdir().unwrap();
    seed(root.path(), A);
    BAD_PRAGMA.with(|bad| bad.set(Some("synchronous")));
    let error = publish(root.path(), A, 1, None, |_| Ok(())).unwrap_err();
    BAD_PRAGMA.with(|bad| bad.set(None));
    assert!(error.to_string().ends_with("pragma:synchronous"), "{error}");
}

#[test]
fn member_kind_constraint_and_v3_namespace_are_not_v2_registration() {
    let _g = serial();
    let root = tempdir().unwrap();
    publish(root.path(), A, 1024, None, |p| {
        p.registry
            .register_member("view-scope", "view", Some(root.path()))?;
        p.registry.register_member("base-scope", "base", None)?;
        assert!(p
            .registry
            .register_member("invalid-scope", "other", None)
            .is_err());
        Ok(())
    })
    .unwrap();
    let ledger = LedgerReader::open_existing(root.path(), A).unwrap();
    let count:u64=ledger.database.connection().unwrap().query_row("SELECT COUNT(*) FROM members WHERE kind IN ('view','base') AND state='active' AND missing_sweeps=0 AND last_bind_ms>0",[],|r|r.get(0)).unwrap();
    assert_eq!(count, 2);
    assert!(!root.path().join("blobs/v2").exists());
    assert!(view_dir(root.path(), A, "view-scope")
        .unwrap()
        .starts_with(family_dir(root.path(), A).unwrap()));
}

#[test]
fn read_only_first_owner_upgrades_without_a_second_live_connection() {
    let _g = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let ledger = LedgerReader::open_existing(root.path(), A).unwrap();
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    assert!(Arc::ptr_eq(&ledger.database, &writer.database));
    assert_eq!(
        crate::db::file_identity::open_connections(&db(root.path(), A)),
        1
    );
    let reservation = writer.reserve(1).unwrap();
    assert_eq!(ledger.accounting().unwrap().reserved_bytes, 1);
    writer.release(reservation).unwrap();
}

#[test]
fn plane_reader_reuses_the_publication_connection() {
    let _g = serial();
    let root = tempdir().unwrap();
    publish(root.path(), A, 1024, None, |p| {
        p.put(&key(), b"v3 data", 1)?;
        let reader = PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic)?;
        assert!(Arc::ptr_eq(&reader.database, &p.planes[0].1));
        assert_eq!(reader.get(&key())?, Some(b"v3 data".to_vec()));
        assert_eq!(
            crate::db::file_identity::open_connections(&plane_path(
                root.path(),
                A,
                FamilyPlane::Semantic
            )?),
            1
        );
        Ok(())
    })
    .unwrap();
}

fn audit_result(control: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(control).unwrap()).unwrap()
}
fn assert_reader_io_is_read_only(result: &serde_json::Value) {
    assert_eq!(result["readonly"], 1);
    for phase in ["before", "after"] {
        let counts: read_audit::Counts = serde_json::from_value(result[phase].clone()).unwrap();
        assert_eq!(
            (counts.main.writes, counts.main.truncates, counts.main.syncs),
            (0, 0, 0),
            "{phase}: {counts:?}"
        );
        assert_eq!(
            (counts.wal.writes, counts.wal.truncates, counts.wal.syncs),
            (0, 0, 0),
            "{phase}: {counts:?}"
        );
        assert_eq!(
            (
                counts.other.writes,
                counts.other.truncates,
                counts.other.syncs
            ),
            (0, 0, 0),
            "{phase}: {counts:?}"
        );
        assert_eq!(counts.main.opens, 1);
        assert!(counts.wal.opens > 0);
    }
    let after: read_audit::Counts = serde_json::from_value(result["after"].clone()).unwrap();
    assert_eq!(after.main.closes, 1);
    assert_eq!(after.wal.closes, after.wal.opens);
}

#[test]
fn native_cross_process_ledger_read_has_no_main_or_wal_write_truncate_or_sync() {
    let _g = serial();
    let root = tempdir().unwrap();
    let control = tempdir().unwrap();
    seed(root.path(), A);
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let reservation = writer.reserve(1234).unwrap();
    assert!(
        fs::metadata(db(root.path(), A).with_file_name("members.sqlite-wal"))
            .unwrap()
            .len()
            > 32
    );
    let before = writer.accounting().unwrap();
    let ready = control.path().join("audit.json");
    assert!(child(root.path(), A, "ledger-audit", &ready)
        .wait()
        .unwrap()
        .success());
    let result = audit_result(&ready);
    assert_eq!(result["reserved"], 1234);
    assert_reader_io_is_read_only(&result);
    assert_eq!(writer.accounting().unwrap(), before);
    writer.release(reservation).unwrap();
}

#[test]
fn read_only_close_does_not_checkpoint_after_the_writer_is_killed() {
    let _g = serial();
    let root = tempdir().unwrap();
    let control = tempdir().unwrap();
    seed(root.path(), A);
    let writer_ready = control.path().join("writer");
    let reader_ready = control.path().join("reader.json");
    let mut writer = child(root.path(), A, "wal-writer", &writer_ready);
    wait_file(&writer_ready, &mut writer);
    let mut reader = child(root.path(), A, "ledger-transaction", &reader_ready);
    wait_file(&reader_ready, &mut reader);
    assert_eq!(audit_result(&reader_ready)["reserved"], 1234);
    writer.kill().unwrap();
    writer.wait().unwrap();
    let wal = db(root.path(), A).with_file_name("members.sqlite-wal");
    let length = fs::metadata(&wal).unwrap().len();
    assert!(length > 32);
    fs::write(reader_ready.with_extension("release"), b"close").unwrap();
    assert!(reader.wait().unwrap().success());
    assert_reader_io_is_read_only(&audit_result(&reader_ready));
    assert_eq!(fs::metadata(wal).unwrap().len(), length);
}

#[test]
fn reader_vfs_audit_counts_real_scratch_writes_truncates_and_syncs() {
    let _g = serial();
    let root = tempdir().unwrap();
    read_audit::reset();
    let c = read_audit::open(&root.path().join("scratch.sqlite"), OpenFlags::default()).unwrap();
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data(payload); INSERT INTO data VALUES(zeroblob(8192));").unwrap();
    checkpoint(&c).unwrap();
    drop(c);
    let counts = read_audit::counts();
    eprintln!("audited scratch I/O: {counts:?}");
    assert!(
        counts.main.writes > 0 && counts.main.syncs > 0,
        "{counts:?}"
    );
    assert!(
        counts.wal.writes > 0 && counts.wal.truncates > 0 && counts.wal.syncs > 0,
        "{counts:?}"
    );
    assert_eq!(counts.main.closes, counts.main.opens);
    assert_eq!(counts.wal.closes, counts.wal.opens);
}

#[cfg(unix)]
#[test]
fn reader_created_shm_stays_within_the_covering_inventory_at_the_budget_boundary() {
    let _g = serial();
    let root = tempdir().unwrap();
    let control = tempdir().unwrap();
    seed(root.path(), A);
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let covered = writer.accounting().unwrap().usage();
    assert_eq!(covered, inventory(&writer.dir));
    drop(writer);
    drop(lock);
    let shm = db(root.path(), A).with_file_name("members.sqlite-shm");
    assert!(!shm.exists());
    let ready = control.path().join("reader.json");
    let mut reader = child(root.path(), A, "ledger-held", &ready);
    wait_file(&ready, &mut reader);
    assert!(shm.exists());
    assert!(inventory(&family_dir(root.path(), A).unwrap()) <= covered);
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let reservation = writer.reserve(NAMESPACE_BUDGET - covered).unwrap();
    writer.release(reservation).unwrap();
    fs::write(ready.with_extension("release"), b"close").unwrap();
    assert!(reader.wait().unwrap().success());
    assert_reader_io_is_read_only(&audit_result(&ready));
}

#[cfg(unix)]
#[test]
fn file_identity_aliases_share_one_sqlite_owner() {
    let _g = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let alias = root.path().join("registry-alias.sqlite");
    fs::hard_link(db(root.path(), A), &alias).unwrap();
    let database = connections::shared(&alias, Kind::Registry, false, true).unwrap();
    assert!(Arc::ptr_eq(&database, &writer.database));
    assert_eq!(
        crate::db::file_identity::open_connections(&db(root.path(), A)),
        1
    );
}

#[test]
fn reservation_release_rejects_a_different_registry_with_the_same_row_id() {
    let _g = serial();
    let root = tempdir().unwrap();
    publish(root.path(), A, 0, None, |_| Ok(())).unwrap();
    publish(root.path(), B, 0, None, |_| Ok(())).unwrap();
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut a = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let mut b = RegistryWriter::open_existing(root.path(), B, &lock).unwrap();
    let reservation_a = a.reserve(100).unwrap();
    let reservation_b = b.reserve(200).unwrap();
    assert_eq!(reservation_a.id, reservation_b.id);
    let before_a = a.accounting().unwrap();
    let before_b = b.accounting().unwrap();
    let error = b
        .release(reservation_a)
        .expect_err("a different registry must reject the reservation token");
    assert!(
        matches!(error, Error::ReservationRegistryMismatch),
        "{error}"
    );
    assert_eq!(a.accounting().unwrap(), before_a);
    assert_eq!(b.accounting().unwrap(), before_b);
    b.release(reservation_b).unwrap();
}

fn publish_after_dead_owner(target: &str) {
    let _g = serial();
    let root = tempdir().unwrap();
    let control = tempdir().unwrap();
    publish(root.path(), A, 0, None, |_| Ok(())).unwrap();
    let ready = control.path().join("owner");
    let mut owner = child(root.path(), A, "abandoned-reservation", &ready);
    wait_file(&ready, &mut owner);
    owner.kill().unwrap();
    owner.wait().unwrap();
    assert_eq!(
        LedgerReader::open_existing(root.path(), A)
            .unwrap()
            .accounting()
            .unwrap()
            .reserved_bytes,
        100
    );
    set_free_space_for_test(NAMESPACE_BUDGET + 150, 8 * NAMESPACE_BUDGET);
    publish(root.path(), target, 100, None, |_| Ok(()))
        .expect("dead reservations must be recovered before the final floor decision");
    assert_eq!(
        LedgerReader::open_existing(root.path(), A)
            .unwrap()
            .accounting()
            .unwrap()
            .reserved_bytes,
        0
    );
}
#[test]
fn dead_same_family_reservation_is_recovered_before_floor_admission() {
    publish_after_dead_owner(A);
}
#[test]
fn dead_peer_reservation_is_recovered_before_floor_admission() {
    publish_after_dead_owner(B);
}

pub(super) fn initialization_hook(kind: Kind, step: &str) {
    let prefix = if matches!(kind, Kind::Registry) {
        "registry"
    } else {
        "plane"
    };
    if std::env::var("AFT_V3_TEST_MODE").ok().as_deref()
        == Some(format!("init-{prefix}-{step}").as_str())
    {
        fs::write(
            PathBuf::from(std::env::var_os("AFT_V3_TEST_CONTROL").unwrap()),
            step,
        )
        .unwrap();
        loop {
            std::thread::park();
        }
    }
}
fn interrupted_initialization(kind: Kind, step: &str, check_visibility: bool) {
    let _g = serial();
    let root = tempdir().unwrap();
    let control = tempdir().unwrap();
    let ready = control.path().join("ready");
    let prefix = if matches!(kind, Kind::Registry) {
        "registry"
    } else {
        "plane"
    };
    let mut publisher = child(root.path(), A, &format!("init-{prefix}-{step}"), &ready);
    wait_file(&ready, &mut publisher);
    let mut visible = if check_visibility {
        if matches!(kind, Kind::Registry) {
            LedgerReader::open_existing(root.path(), A).is_ok()
        } else {
            PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic).is_ok()
        }
    } else {
        false
    };
    let partial_schema = if check_visibility {
        let path = match kind {
            Kind::Registry => db(root.path(), A),
            Kind::Plane(plane) => plane_path(root.path(), A, plane).unwrap(),
        };
        (|| -> Result<bool> {
            if crate::db::file_identity::open_connections(&path) != 0 {
                return Err(Error::InvalidIdentity);
            }
            let connection = IdentityConnection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY,
                "v3-initialization-visibility",
            )?;
            Ok(!schema_rows(&connection)?.is_empty())
        })()
    } else {
        Ok(false)
    };
    publisher.kill().unwrap();
    publisher.wait().unwrap();
    visible |= partial_schema.expect("SQLite-only initialization visibility probe");
    assert!(
        !visible,
        "initialization became visible before its final commit: {prefix}/{step}"
    );
    let retry = publish(root.path(), A, 1024, None, |p| p.put(&key(), b"v3 data", 1));
    assert!(
        retry.is_ok(),
        "interrupted {prefix}/{step} must be retryable: {retry:?}"
    );
    assert_eq!(
        LedgerReader::open_existing(root.path(), A)
            .unwrap()
            .accounting()
            .unwrap()
            .reserved_bytes,
        0
    );
    assert_eq!(
        PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic)
            .unwrap()
            .get(&key())
            .unwrap(),
        Some(b"v3 data".to_vec())
    );
}
macro_rules! interrupted_initialization_cases {
    ($kind:expr, $($name:ident => $step:literal),+ $(,)?) => { $(
        #[test] fn $name() { interrupted_initialization($kind,$step,false); }
    )+ };
}
interrupted_initialization_cases!(Kind::Registry,
    interrupted_registry_auto_vacuum_is_retryable=>"auto_vacuum",
    interrupted_registry_meta_is_retryable=>"meta",
    interrupted_registry_schema_is_retryable=>"schema",
    interrupted_registry_format_is_retryable=>"format",
    interrupted_registry_application_id_is_retryable=>"application_id",
    interrupted_registry_accounting_is_retryable=>"accounting",
);
interrupted_initialization_cases!(Kind::Plane(FamilyPlane::Semantic),
    interrupted_plane_auto_vacuum_is_retryable=>"auto_vacuum",
    interrupted_plane_meta_is_retryable=>"meta",
    interrupted_plane_schema_is_retryable=>"schema",
    interrupted_plane_format_is_retryable=>"format",
    interrupted_plane_application_id_is_retryable=>"application_id",
    interrupted_plane_store_meta_is_retryable=>"store_meta",
);
#[test]
fn registry_initialization_is_invisible_until_its_commit() {
    interrupted_initialization(Kind::Registry, "accounting", true);
}
#[test]
fn plane_initialization_is_invisible_until_its_commit() {
    interrupted_initialization(Kind::Plane(FamilyPlane::Semantic), "store_meta", true);
}

fn legacy_bootstrap(storage: &Path, kind: Kind, stage: u8, populated: bool) {
    let directory = family_dir(storage, A).unwrap();
    fs::create_dir_all(&directory).unwrap();
    let path = match kind {
        Kind::Registry => directory.join("members.sqlite"),
        Kind::Plane(plane) => directory.join(format!("{}.sqlite", plane.as_str())),
    };
    let c = IdentityConnection::open(&path, "v3-bootstrap-fixture").unwrap();
    c.pragma_update(None, "auto_vacuum", 2).unwrap();
    if stage >= 1 {
        c.execute_batch(META).unwrap();
    }
    if stage >= 2 {
        c.execute_batch(kind.schema()).unwrap();
    }
    if stage >= 3 {
        c.execute(
            "INSERT INTO format_meta VALUES(1,?1,?2,?3,?4)",
            params![
                FORMAT_VERSION,
                SCHEMA_VERSION,
                kind.layout(),
                format!("{META}{}", kind.schema())
            ],
        )
        .unwrap();
    }
    if stage >= 4 {
        c.pragma_update(None, "application_id", kind.app_id())
            .unwrap();
    }
    if stage >= 5 {
        match kind {
            Kind::Registry => {
                c.execute("INSERT INTO accounting VALUES(1,0,0)", [])
                    .unwrap();
            }
            Kind::Plane(plane) => {
                c.execute("INSERT INTO store_meta VALUES(1,?1,0)", [plane.as_str()])
                    .unwrap();
            }
        }
    }
    if populated {
        c.execute("INSERT INTO members(scope,kind,view_dir,registered_at_ms,last_bind_ms,state) VALUES('keep','base','keep',0,0,'active')",[]).unwrap();
    }
}
#[test]
fn legacy_partial_registry_bootstraps_do_not_block_another_family() {
    let _g = serial();
    for stage in 0..=5 {
        let root = tempdir().unwrap();
        legacy_bootstrap(root.path(), Kind::Registry, stage, false);
        publish(root.path(), B, 1, None, |_| Ok(())).unwrap();
        let lock = ReservationLock::acquire(root.path()).unwrap();
        let writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
        assert_eq!(writer.accounting().unwrap().reserved_bytes, 0);
    }
}
#[test]
fn legacy_partial_plane_bootstraps_are_retryable() {
    let _g = serial();
    for stage in 0..=5 {
        let root = tempdir().unwrap();
        legacy_bootstrap(
            root.path(),
            Kind::Plane(FamilyPlane::Semantic),
            stage,
            false,
        );
        publish(root.path(), A, 1024, None, |p| p.put(&key(), b"v3 data", 1)).unwrap();
        assert_eq!(
            PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic)
                .unwrap()
                .get(&key())
                .unwrap(),
            Some(b"v3 data".to_vec())
        );
    }
}
#[test]
fn incomplete_marker_with_application_data_is_not_reset() {
    let _g = serial();
    let root = tempdir().unwrap();
    legacy_bootstrap(root.path(), Kind::Registry, 2, true);
    let error = publish(root.path(), B, 1, None, |_| Ok(())).unwrap_err();
    assert_eq!(error.to_string(), "reservation_ledger_unreadable");
    let path = db(root.path(), A);
    assert_eq!(crate::db::file_identity::open_connections(&path), 0);
    let c = IdentityConnection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
        "v3-bootstrap-check",
    )
    .unwrap();
    assert_eq!(
        c.query_row(
            "SELECT COUNT(*) FROM members WHERE scope='keep'",
            [],
            |row| row.get::<_, u64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        c.query_row("SELECT COUNT(*) FROM format_meta", [], |row| row
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn relocated_v3_database_keeps_the_storage_route_for_floor_and_refusal_scope() {
    let _g = serial();
    let root = tempdir().unwrap();
    let storage = root.path().join("storage");
    let moved = root.path().join("another-disk-blobs");
    seed(&storage, A);
    let path = db(&storage, A);
    assert_eq!(crate::db::file_identity::open_connections(&path), 0);
    fs::rename(storage.join("blobs"), &moved).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&moved, storage.join("blobs")).unwrap();
    #[cfg(windows)]
    {
        let output = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(storage.join("blobs"))
            .arg(&moved)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction creation failed: {output:?}"
        );
    }
    let displayed = fs::canonicalize(&path).unwrap();
    let floor = UnsupportedPersistedFormat::floor(
        PersistedStore::ViewsV3Registry,
        storage.join("reader-floor.json"),
        99,
    );
    persisted_format::record(&floor, &storage);
    let error = LedgerReader::open_existing(&storage, A)
        .err()
        .expect("the owning storage floor must be checked before a relocated v3 database");
    assert!(matches!(error, Error::Incompatible(refusal) if refusal==floor));
    persisted_format::replace_floor_refusals(&storage, &[]);
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE VIEW extra AS SELECT 1")
        .unwrap();
    let error = LedgerReader::open_existing(&storage, A).err().unwrap();
    let refusal = persisted_format::refusal_covering(PersistedStore::ViewsV3Registry, &path)
        .expect(
            "v3 must retain its logical storage route after canonicalizing its connection identity",
        );
    assert_eq!(refusal.path, displayed);
    assert_eq!(error.to_string(), refusal.to_string());
    assert_eq!(
        persisted_format::refusals_under(&storage),
        vec![refusal.clone()]
    );
    assert_eq!(
        persisted_format::refusals_for_push(&storage, None),
        (vec![refusal], 0)
    );
}

#[test]
fn nested_v3_roots_select_the_inner_owner_floor_without_hiding_outer_refusals() {
    let _g = serial();
    let fixture = tempdir().unwrap();
    let outer = fixture.path().join("outer");
    let target = fixture.path().join("other-disk");
    fs::create_dir_all(outer.join("padding")).unwrap();
    fs::create_dir_all(&target).unwrap();
    let inner = outer.join("inner");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &inner).unwrap();
    #[cfg(windows)]
    {
        let output = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(&inner)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction creation failed: {output:?}"
        );
    }
    let outer = outer.join("padding/..");
    let routed_inner = outer.join("inner");
    seed(&outer, A);
    seed(&routed_inner, A);
    let store = PersistedStore::ViewsV3Registry;
    let outer_floor = UnsupportedPersistedFormat::floor(store, outer.join("reader-floor.json"), 99);
    let inner_floor =
        UnsupportedPersistedFormat::floor(store, inner.join("reader-floor.json"), 100);
    persisted_format::record(&outer_floor, &outer);
    persisted_format::record(&inner_floor, &inner);
    let error = LedgerReader::open_existing(&routed_inner, A).err().unwrap();
    assert!(
        matches!(error, Error::Incompatible(refusal) if refusal==inner_floor),
        "the v3 comparison route must use its most specific owner"
    );
    let error = LedgerReader::open_existing(&outer, A).err().unwrap();
    assert!(matches!(error, Error::Incompatible(refusal) if refusal==outer_floor));
    assert_eq!(
        persisted_format::refusals_under(&inner),
        vec![inner_floor.clone()]
    );
    assert_eq!(
        persisted_format::refusals_under(&outer),
        vec![outer_floor.clone()]
    );
    assert_eq!(
        persisted_format::refusals_for_push(&inner, None),
        (vec![inner_floor], 0)
    );
    assert_eq!(
        persisted_format::refusals_for_push(&outer, None),
        (vec![outer_floor], 0)
    );
}

/// When the deadline stops the recheck after the measurement's own commit
/// allocated registry pages the walk did not count, the result is owed and
/// the saved total is still never below what the family really uses.
#[cfg(unix)]
#[test]
fn a_recheck_stopped_by_the_deadline_never_saves_a_total_below_usage() {
    let _guard = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let before = inventory(&writer.dir);
    grow_and_expire_recheck_for_test();
    let measured = writer
        .covering_measurement_until(std::time::Instant::now() + Duration::from_secs(60))
        .unwrap();
    let actual = inventory(&writer.dir);
    assert!(actual > before, "the seam allocated no registry pages");
    let saved = LedgerReader::open_existing(root.path(), A)
        .unwrap()
        .accounting()
        .unwrap()
        .usage();
    assert!(
        saved >= actual,
        "saved {saved} is below the family's {actual}"
    );
    assert_eq!(measured, Measurement::Owed);
}
