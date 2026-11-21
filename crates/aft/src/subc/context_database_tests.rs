mod shared_db_tests {
    use crate::backup::BackupStore;
    use crate::config::Config;
    use crate::context::{database_test_support::*, App, AppContext};
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;
    #[cfg(unix)]
    use std::time::Instant;
    use tempfile::tempdir;

    fn bound_database_root(app: Arc<App>, root: &Path, storage: &Path) -> Arc<AppContext> {
        crate::commands::configure::register_runtime_hooks();
        let ctx = Arc::new(AppContext::from_app(
            app,
            Config {
                project_root: Some(root.to_path_buf()),
                storage_dir: Some(storage.to_path_buf()),
                ..Config::default()
            },
        ));
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx.set_canonical_cache_root(root.to_path_buf());
        ctx.bash_background()
            .set_harness(crate::harness::Harness::Opencode);
        {
            let mut backup = ctx.backup().lock();
            backup.set_storage_dir(storage.to_path_buf(), 72);
            backup.set_db_harness(crate::harness::Harness::Opencode);
        }
        ctx.begin_database_runtime(
            root.to_path_buf(),
            storage.to_path_buf(),
            "idle-rebind".into(),
        );
        crate::database_open::run_staged_open(
            &ctx,
            crate::database_open::DatabaseOpenRunner::ConfigureTail,
            true,
        );
        ctx.mark_subc_bound();
        assert_eq!(database_state(ctx.as_ref()), 2);
        ctx
    }

    fn assert_rebound_backup_is_persisted(ctx: &AppContext, root: &Path, storage: &Path) {
        let file = root.join("edited.txt");
        std::fs::write(&file, "before rebind edit").unwrap();
        let id = ctx
            .backup()
            .lock()
            .snapshot("idle-rebind", &file, "before edit after rebind")
            .unwrap()
            .unwrap();
        std::fs::write(&file, "after rebind edit").unwrap();
        ctx.backup().lock().record_post_mutation_states();
        let conn = ctx.db().unwrap();
        let count: u32 = conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM backups WHERE backup_id = ?1 AND harness = 'opencode'
             AND session_id = 'idle-rebind' AND project_key = ?2",
                rusqlite::params![id, crate::path_identity::project_scope_key(root)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "backup taken after rebind must be mirrored to aft.db"
        );

        // A fresh store must recover the persisted backup rather than reuse
        // the original store's in-memory backup entries.
        let mut recovered = BackupStore::new();
        recovered.set_storage_dir(storage.to_path_buf(), 72);
        recovered.set_db_harness(crate::harness::Harness::Opencode);
        recovered.set_db_project_key(crate::path_identity::project_scope_key(root));
        recovered.set_db_pool(conn);
        let (entry, warning) = recovered.restore_latest("idle-rebind", &file).unwrap();
        assert_eq!(entry.backup_id, id);
        assert!(warning.is_none(), "unexpected restore warning: {warning:?}");
        assert_eq!(std::fs::read_to_string(file).unwrap(), "before rebind edit");
    }

    #[cfg(unix)]
    fn completed_rebind_bash_task(ctx: &AppContext, root: &Path, storage: &Path) -> String {
        let id = ctx
            .bash_background()
            .spawn(
                crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                "printf 'idle rebind persistence\\n'",
                "idle-rebind".into(),
                root.to_path_buf(),
                std::collections::HashMap::new(),
                crate::bash_background::HardKill::After(Duration::from_secs(30)),
                None,
                storage.to_path_buf(),
                16,
                true,
                true,
                Some(root.to_path_buf()),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(completion) = ctx
                .bash_background()
                .drain_completions_for_session(Some("idle-rebind"))
                .into_iter()
                .find(|completion| completion.task_id == id)
            {
                assert_eq!(completion.exit_code, Some(0));
                return id;
            }
            assert!(Instant::now() < deadline, "bash task {id} did not complete");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn assert_rebind_bash_row(ctx: &AppContext, id: &str) {
        let count: u32 = ctx
            .db()
            .unwrap()
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM bash_tasks WHERE task_id = ?1 AND status = 'completed'",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "bash task {id} must have an aft.db row");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let events: u32 = ctx
                .db()
                .unwrap()
                .lock()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM compression_events WHERE task_id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap();
            if events == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "bash task {id} lost its compression event"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn ready_root_rebind_reattaches_pools_after_idle_release() {
        let storage = tempdir().unwrap();
        let root_a = tempdir().unwrap();
        let root_b = tempdir().unwrap();
        let app = App::default_shared();
        let a = bound_database_root(app.clone(), root_a.path(), storage.path());
        let b = bound_database_root(app.clone(), root_b.path(), storage.path());
        let shared = b.db().unwrap();
        assert!(Arc::ptr_eq(&a.db().unwrap(), &shared));
        let first = completed_rebind_bash_task(&a, root_a.path(), storage.path());
        assert_rebind_bash_row(&a, &first);

        a.mark_subc_unbound();
        a.release_idle_reopenable_resources_in_background()
            .join()
            .unwrap();
        assert!(!a.bash_background().uses_db_pool(&shared));
        assert!(!a.backup().lock().uses_db_pool(&shared));
        assert!(!b.subc_unbound_quiesced());
        assert!(b.bash_background().uses_db_pool(&shared));
        assert!(b.backup().lock().uses_db_pool(&shared));
        assert!(Arc::ptr_eq(
            &app.db_for_path(&storage.path().join("aft.db")).unwrap(),
            &shared
        ));
        a.begin_database_runtime(
            root_a.path().to_path_buf(),
            storage.path().to_path_buf(),
            "idle-rebind".into(),
        );
        a.mark_subc_bound();
        assert_eq!(database_open_attempts(&a), 1);
        assert!(!a.database_runtime_initializing());
        let second = completed_rebind_bash_task(&a, root_a.path(), storage.path());
        assert_ne!(first, second);
        assert_rebind_bash_row(&a, &second);
        assert_rebound_backup_is_persisted(&a, root_a.path(), storage.path());
    }

    #[test]
    fn ready_root_reconfigure_repairs_each_detached_pool_without_another_open() {
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        let ctx = bound_database_root(App::default_shared(), root.path(), storage.path());
        let shared = ctx.db().unwrap();
        ctx.backup().lock().clear_db_pool();
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "idle-rebind".into(),
        );
        assert!(ctx.backup().lock().uses_db_pool(&shared));
        assert!(ctx.bash_background().uses_db_pool(&shared));
        ctx.bash_background().clear_db_pool();
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "idle-rebind".into(),
        );
        assert!(ctx.backup().lock().uses_db_pool(&shared));
        assert!(ctx.bash_background().uses_db_pool(&shared));
        assert!(!ctx.database_runtime_initializing());
        assert_eq!(database_open_attempts(&ctx), 1);
    }

    #[test]
    fn ready_root_rebind_orders_pool_attach_after_release_and_before_late_release() {
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        let ctx = bound_database_root(App::default_shared(), root.path(), storage.path());
        ctx.mark_subc_unbound();

        // Configure commits before the bound transition. Force idle release into
        // that gap, so repairing pools only during configure cannot pass.
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "idle-rebind".into(),
        );
        ctx.release_idle_reopenable_resources_in_background()
            .join()
            .unwrap();
        ctx.mark_subc_bound();
        assert_rebound_backup_is_persisted(&ctx, root.path(), storage.path());

        // After the bind, an idle release must not clear the database pools.
        ctx.release_idle_reopenable_resources_in_background()
            .join()
            .unwrap();
        assert_rebound_backup_is_persisted(&ctx, root.path(), storage.path());
        assert_eq!(database_open_attempts(&ctx), 1);
    }

    fn failed_database_context() -> (tempfile::TempDir, tempfile::TempDir, AppContext) {
        crate::commands::configure::register_runtime_hooks();
        // Each context owns its storage fixture. A process-wide override would
        // redirect unrelated contexts while libtest runs them in parallel.
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        let path = storage.path().join("aft.db");
        let conn = crate::db::open(&path).unwrap();
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            [crate::db::CURRENT_SCHEMA_VERSION + 1],
        )
        .unwrap();
        drop(conn);
        let ctx = AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "database-recovery".into(),
        );
        crate::database_open::run_staged_open(
            &ctx,
            crate::database_open::DatabaseOpenRunner::ConfigureTail,
            true,
        );
        assert_eq!(database_state(&ctx), 3);
        assert_eq!(database_open_attempts(&ctx), 1);
        let response = ctx.database_runtime_refusal("failed", "bash").unwrap();
        assert!(serde_json::to_value(response).unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("storage_requires_newer_reader"));
        assert!(ctx.db().is_none());
        (storage, root, ctx)
    }

    #[test]
    fn database_fixtures_keep_storage_local_to_each_context() {
        let (first_storage, _first_root, first) = failed_database_context();
        let (second_storage, _second_root, second) = failed_database_context();
        assert_eq!(second.storage_dir(), second_storage.path());
        assert_eq!(first.storage_dir(), first_storage.path());
    }

    #[test]
    fn failed_database_open_recovers_after_backoff_without_rebind() {
        let (storage, _root, ctx) = failed_database_context();
        // The failed open held no connection. Repair only this test's database,
        // closing the repair connection before AFT attempts to reopen it.
        let conn = rusqlite::Connection::open(storage.path().join("aft.db")).unwrap();
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            [crate::db::CURRENT_SCHEMA_VERSION],
        )
        .unwrap();
        drop(conn);
        std::thread::sleep(Duration::from_millis(2_100));

        let refusal = persistence_call(&ctx);
        assert!(
            refusal.is_none(),
            "repaired database stayed latched: {refusal:?}"
        );
        assert_eq!(database_open_attempts(&ctx), 2);
        assert!(ctx.db().is_some());
        let (error, retry_at, backoff) = database_failure(&ctx);
        assert!(error.is_none());
        assert!(retry_at.is_none());
        assert_eq!(backoff, Duration::ZERO);
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "persist", "command": "db_set_host_state",
            "params": {"key": "recovered", "value": "without-rebind"}
        }))
        .unwrap();
        assert!(crate::commands::state::handle_db_set_host_state(&request, &ctx).success);
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        assert_eq!(
            crate::db::state::get_host_state(&conn, "recovered").unwrap(),
            Some("without-rebind".into())
        );
    }

    #[test]
    fn failed_database_open_does_not_reopen_inside_backoff() {
        let (_storage, _root, ctx) = failed_database_context();
        for _ in 0..10 {
            assert!(persistence_call(&ctx).is_some());
        }
        assert_eq!(database_open_attempts(&ctx), 1);
        assert!(ctx.database_runtime_refusal("read", "read").is_none());
    }

    fn failed_floor_context() -> (tempfile::TempDir, tempfile::TempDir, AppContext) {
        crate::commands::configure::register_runtime_hooks();
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        std::fs::write(
            storage.path().join(crate::reader_floor::FLOOR_FILE),
            serde_json::to_vec(&serde_json::json!({
                "floor_schema": crate::reader_floor::FLOOR_SCHEMA,
                "stores": { "aft.db": crate::db::CURRENT_SCHEMA_VERSION + 1 }
            }))
            .unwrap(),
        )
        .unwrap();
        crate::reader_floor::prepare(storage.path());
        let ctx = AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "floor-recovery".into(),
        );
        crate::database_open::run_staged_open(
            &ctx,
            crate::database_open::DatabaseOpenRunner::ConfigureTail,
            true,
        );
        let refusal = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
        assert!(refusal["message"]
            .as_str()
            .unwrap()
            .contains("reader floor"));
        assert!(!storage.path().join("aft.db").exists());
        (storage, root, ctx)
    }

    #[test]
    fn reader_floor_retry_recovers_after_operator_lowers_floor_without_restart() {
        let (storage, _root, ctx) = failed_floor_context();
        std::fs::write(
            storage.path().join(crate::reader_floor::FLOOR_FILE),
            serde_json::to_vec(&serde_json::json!({
                "floor_schema": crate::reader_floor::FLOOR_SCHEMA,
                "stores": { "aft.db": crate::db::CURRENT_SCHEMA_VERSION }
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(persistence_call(&ctx).is_some(), "backoff must still apply");
        assert_eq!(database_open_attempts(&ctx), 1);
        assert_eq!(
            crate::persisted_format::refusal_covering(
                crate::persisted_format::PersistedStore::AftDb,
                &storage.path().join("aft.db")
            )
            .unwrap()
            .found,
            u64::from(crate::db::CURRENT_SCHEMA_VERSION + 1),
            "the floor must not be refreshed before the retry backoff expires"
        );
        make_database_retry_due(&ctx);
        assert!(
            persistence_call(&ctx).is_none(),
            "operator-lowered reader floor remained latched after the retry backoff"
        );
        let request = serde_json::from_value(serde_json::json!({
            "id": "floor-recovered", "command": "db_set_host_state",
            "params": { "key": "floor-recovered", "value": "yes" }
        }))
        .unwrap();
        let response = crate::commands::state::handle_db_set_host_state(&request, &ctx);
        assert!(response.success, "{response:?}");
        let db = ctx.db().unwrap();
        assert_eq!(
            crate::db::state::get_host_state(&db.lock().unwrap(), "floor-recovered").unwrap(),
            Some("yes".into())
        );
    }

    #[test]
    fn reader_floor_retry_refuses_unparseable_floor_written_mid_run() {
        let (storage, _root, ctx) = failed_floor_context();
        let path = storage.path().join(crate::reader_floor::FLOOR_FILE);
        std::fs::write(&path, b"not a reader floor").unwrap();
        for _ in 0..2 {
            make_database_retry_due(&ctx);
            let refusal = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
            assert!(
                refusal["message"]
                    .as_str()
                    .unwrap()
                    .contains("cannot be parsed"),
                "{refusal}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"not a reader floor");
            assert!(!storage.path().join("aft.db").exists());
            assert_eq!(
                crate::persisted_format::refusals_under(storage.path()).len(),
                crate::persisted_format::PersistedStore::ALL.len()
            );
        }
    }

    fn resident_database_context() -> (tempfile::TempDir, tempfile::TempDir, AppContext) {
        crate::commands::configure::register_runtime_hooks();
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        let ctx = AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "resident-recovery".into(),
        );
        crate::database_open::run_staged_open(
            &ctx,
            crate::database_open::DatabaseOpenRunner::ConfigureTail,
            true,
        );
        assert_eq!(database_state(&ctx), 2);
        (storage, root, ctx)
    }

    // A separate process, not a second fd in this process, changes the live
    // fixture while the App retains its SQLite handle and WAL locks.
    fn change_schema_in_child(path: &Path, version: u32) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!(
                    "{}::repaired_resident_database_rechecks_schema_and_pushes_recovered_status",
                    module_path!().split_once("::").unwrap().1
                ),
                "--nocapture",
            ])
            .env("AFT_TEST_SCHEMA_REPAIR_PATH", path)
            .env("AFT_TEST_SCHEMA_REPAIR_VERSION", version.to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "schema repair child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    fn assert_database_status(ctx: &AppContext, refused: bool) {
        let status = ctx.build_status_snapshot();
        let reasons = status["degraded_reasons"].as_array().unwrap();
        assert_eq!(
            reasons
                .iter()
                .any(|reason| reason == "storage_requires_newer_reader:aft.db"),
            refused,
            "unexpected database degraded reasons: {reasons:?}"
        );
        assert_eq!(
            status["storage_refusals"].as_array().unwrap().len(),
            usize::from(refused)
        );
    }

    #[test]
    fn repaired_resident_database_rechecks_schema_and_pushes_recovered_status() {
        if let Some(path) = std::env::var_os("AFT_TEST_SCHEMA_REPAIR_PATH") {
            crate::test_storage::assert_database(Path::new(&path));
            let version: u32 = std::env::var("AFT_TEST_SCHEMA_REPAIR_VERSION")
                .unwrap()
                .parse()
                .unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            conn.execute("UPDATE schema_version SET version = ?1", [version])
                .unwrap();
            conn.execute_batch("COMMIT").unwrap();
            return;
        }
        let (storage, _root, owner) = resident_database_context();
        let resident = owner.db().unwrap();
        let path = storage.path().join("aft.db");
        change_schema_in_child(&path, crate::db::CURRENT_SCHEMA_VERSION + 1);
        let affected_root = tempdir().unwrap();
        let ctx = Arc::new(AppContext::from_app(
            owner.app(),
            Config {
                project_root: Some(affected_root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        ));
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx.set_degraded_reasons(vec!["artifact_owner_read_only".into()]);
        ctx.set_canonical_cache_root(affected_root.path().to_path_buf());
        ctx.begin_database_runtime(
            affected_root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "affected-route".into(),
        );
        let peeks = crate::db::schema_peek_count_for_test();
        crate::database_open::run_staged_open(
            &ctx,
            crate::database_open::DatabaseOpenRunner::ConfigureTail,
            true,
        );
        assert!(persistence_call(&ctx).is_some());
        assert_database_status(&ctx, true);

        change_schema_in_child(&path, crate::db::CURRENT_SCHEMA_VERSION);
        // Expire the real two-second backoff deterministically without sleeping.
        assert_eq!(database_failure(&ctx).2, Duration::from_secs(2));
        make_database_retry_due(&ctx);
        let (tx, rx) = std::sync::mpsc::channel();
        ctx.bind_status_context();
        ctx.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            let _ = tx.send(frame);
        }))));
        assert!(
            persistence_call(&ctx).is_none(),
            "repaired schema remained refused"
        );
        assert_database_status(&ctx, false);
        assert!(Arc::ptr_eq(&resident, &ctx.db().unwrap()));
        assert_eq!(
            crate::db::schema_peek_count_for_test(),
            peeks,
            "recovery opened a second descriptor"
        );
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "persist", "command": "db_set_host_state",
            "params": {"key": "recovered", "value": "resident"}
        }))
        .unwrap();
        assert!(crate::commands::state::handle_db_set_host_state(&request, &ctx).success);
        assert_eq!(
            crate::db::state::get_host_state(&resident.lock().unwrap(), "recovered").unwrap(),
            Some("resident".into())
        );
        let frame = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("recovery must push status_changed without another index event");
        let crate::protocol::PushFrame::StatusChanged(status) = frame else {
            panic!("unexpected frame: {frame:?}")
        };
        assert!(!status.snapshot["degraded_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "storage_requires_newer_reader:aft.db"));
        assert!(status.snapshot["degraded_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "artifact_owner_read_only"));
    }

    #[test]
    fn resident_database_busy_retry_preserves_refusal_until_schema_is_rechecked() {
        let (storage, _root, ctx) = resident_database_context();
        let resident = ctx.db().unwrap();
        let path = storage.path().join("aft.db");
        change_schema_in_child(&path, crate::db::CURRENT_SCHEMA_VERSION + 1);
        ctx.finish_database_runtime_error("recheck schema".into(), false);
        make_database_retry_due(&ctx);
        assert!(persistence_call(&ctx).is_some());
        assert_database_status(&ctx, true);
        let peeks = crate::db::schema_peek_count_for_test();
        let held = resident.lock().unwrap();
        make_database_retry_due(&ctx);
        let response = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
        assert!(response["message"]
            .as_str()
            .unwrap()
            .contains("process-shared database connection is in use"));
        drop(held);
        assert_database_status(&ctx, true);
        change_schema_in_child(&path, crate::db::CURRENT_SCHEMA_VERSION);
        assert!(
            persistence_call(&ctx).is_none(),
            "busy refusal stayed latched after releasing the connection"
        );
        assert_database_status(&ctx, false);
        assert!(Arc::ptr_eq(&resident, &ctx.db().unwrap()));
        assert_eq!(crate::db::schema_peek_count_for_test(), peeks);
    }

    #[test]
    fn busy_database_retry_recovers_from_brief_database_slot_contention() {
        crate::commands::configure::register_runtime_hooks();
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        let ctx = Arc::new(AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        ));
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        // Recreate the reported busy state after deferred initialization exhausted
        // its budget. SQLite is available now, but another reader owns the App slot.
        ctx.finish_database_runtime_error(
            "sqlite PRAGMA journal_mode=WAL: database is locked".into(),
            true,
        );
        let exhausted = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
        assert_eq!(exhausted["code"], "database_unavailable", "{exhausted}");
        assert_eq!(database_state(&ctx), 5);

        let app = ctx.app();
        let held = hold_database_slot(&app);
        let (contended_tx, contended_rx) = std::sync::mpsc::channel();
        let (released_tx, released_rx) = std::sync::mpsc::channel();
        let retry_ctx = Arc::clone(&ctx);
        let retry = std::thread::spawn(move || {
            on_database_slot_contention(move || {
                contended_tx.send(()).unwrap();
                released_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            });
            persistence_call(&retry_ctx)
        });
        contended_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(held);
        released_tx.send(()).unwrap();
        let refusal = retry
            .join()
            .unwrap()
            .map(|response| serde_json::to_value(response).unwrap());
        eprintln!(
            "write after SQLite became available, with transient App slot contention: {refusal:?}"
        );
        assert!(
            refusal.is_none(),
            "available database was refused: {refusal:?}"
        );
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "released", "command": "db_set_host_state",
            "params": { "key": "released", "value": "yes" }
        }))
        .unwrap();
        let response = crate::commands::state::handle_db_set_host_state(&request, &ctx);
        assert!(response.success, "{response:?}");
        let db = ctx.db().unwrap();
        assert_eq!(
            crate::db::state::get_host_state(&db.lock().unwrap(), "released").unwrap(),
            Some("yes".into())
        );
    }

    #[test]
    fn resident_database_open_slot_busy_recovers_on_next_call() {
        let (_storage, _root, ctx) = resident_database_context();
        let resident = ctx.db().unwrap();
        let peeks = crate::db::schema_peek_count_for_test();
        ctx.finish_database_runtime_error("retry open".into(), false);
        make_database_retry_due(&ctx);
        let app = ctx.app();
        let held = hold_database_slot(&app);
        let response = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
        assert!(response["message"]
            .as_str()
            .unwrap()
            .contains("process-shared database connection is in use"));
        assert_eq!(database_state(&ctx), 5);
        drop(held);
        assert!(
            persistence_call(&ctx).is_none(),
            "busy refusal stayed latched after releasing the open slot"
        );
        assert!(Arc::ptr_eq(&resident, &ctx.db().unwrap()));
        assert_eq!(crate::db::schema_peek_count_for_test(), peeks);
    }

    #[test]
    fn permanent_database_error_is_rechecked_with_doubling_capped_backoff() {
        let (storage, _root, ctx) = failed_database_context();
        assert_eq!(database_failure(&ctx).2, Duration::from_secs(2));
        // A different unsupported version must appear in the next refusal,
        // proving that retries re-check the file rather than replaying an error.
        let conn = rusqlite::Connection::open(storage.path().join("aft.db")).unwrap();
        conn.execute(
            "UPDATE schema_version SET version = ?1",
            [crate::db::CURRENT_SCHEMA_VERSION + 2],
        )
        .unwrap();
        drop(conn);
        for (index, seconds) in [4, 8, 16, 32, 60, 60].into_iter().enumerate() {
            make_database_retry_due(&ctx);
            assert!(ctx.database_runtime_refusal("read", "read").is_none());
            assert!(!ctx.claim_database_runtime_retry("read"));
            let response = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
            let message = response["message"].as_str().unwrap();
            assert!(message.contains("storage_requires_newer_reader"));
            assert!(message.contains(&format!(
                "format version {}",
                crate::db::CURRENT_SCHEMA_VERSION + 2
            )));
            assert!(message.contains("next persistence-requiring call after"));
            assert!(message.contains("AFT retries"));
            assert!(!message.contains("rebind to retry"));
            assert_eq!(response["retryable"], true);
            assert_eq!(database_failure(&ctx).2, Duration::from_secs(seconds));
            assert_eq!(database_open_attempts(&ctx), index as u64 + 2);
            assert!(persistence_call(&ctx).is_some());
            assert_eq!(database_open_attempts(&ctx), index as u64 + 2);
            assert!(ctx.db().is_none());
        }
    }

    #[test]
    fn only_one_concurrent_call_claims_a_failed_database_retry() {
        let (_storage, _root, ctx) = failed_database_context();
        let ctx = Arc::new(ctx);
        make_database_retry_due(&ctx);
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let calls = (0..8)
            .map(|_| {
                let ctx = Arc::clone(&ctx);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    ctx.claim_database_runtime_retry("bash")
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            calls
                .into_iter()
                .map(|call| call.join().unwrap())
                .filter(|claimed| *claimed)
                .count(),
            1
        );
        ctx.retry_database_runtime();
        assert_eq!(database_open_attempts(&ctx), 2);
    }

    #[test]
    fn failed_database_retry_reuses_and_preserves_a_resident_connection() {
        crate::commands::configure::register_runtime_hooks();
        let storage = tempdir().unwrap();
        let root = tempdir().unwrap();
        let ctx = AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        let path = storage.path().join("aft.db");
        let resident = ctx.app().open_db(&path).unwrap();
        let peeks = crate::db::schema_peek_count_for_test();
        resident
            .lock()
            .unwrap()
            .execute(
                "UPDATE schema_version SET version = ?1",
                [crate::db::CURRENT_SCHEMA_VERSION + 1],
            )
            .unwrap();
        ctx.begin_database_runtime(
            root.path().to_path_buf(),
            storage.path().to_path_buf(),
            "resident".into(),
        );
        crate::database_open::run_staged_open(
            &ctx,
            crate::database_open::DatabaseOpenRunner::ConfigureTail,
            true,
        );
        assert_eq!(
            crate::db::schema_peek_count_for_test(),
            peeks,
            "resident schema checks must not open another descriptor"
        );
        let response = serde_json::to_value(persistence_call(&ctx).unwrap()).unwrap();
        assert!(response["message"]
            .as_str()
            .unwrap()
            .contains("storage_requires_newer_reader"));
        assert!(Arc::ptr_eq(
            &resident,
            &ctx.db().expect("refusal must preserve the shared handle")
        ));
        resident
            .lock()
            .unwrap()
            .execute(
                "UPDATE schema_version SET version = ?1",
                [crate::db::CURRENT_SCHEMA_VERSION],
            )
            .unwrap();
        make_database_retry_due(&ctx);
        assert!(persistence_call(&ctx).is_none());
        assert_eq!(
            crate::db::schema_peek_count_for_test(),
            peeks,
            "retries must not open another descriptor on a resident database"
        );
        assert!(Arc::ptr_eq(&resident, &ctx.db().unwrap()));
        assert_eq!(database_open_attempts(&ctx), 2);
    }

    #[test]
    fn database_retry_without_configured_root_refuses_without_panicking() {
        crate::commands::configure::register_runtime_hooks();
        let ctx = AppContext::from_app(App::default_shared(), Config::default());
        assert!(ctx.canonical_cache_root_opt().is_none());
        ctx.finish_database_runtime_error("database busy".into(), true);
        assert!(ctx
            .database_runtime_refusal("first", "db_set_host_state")
            .is_some());
        assert!(ctx.claim_database_runtime_retry("db_set_host_state"));
        ctx.retry_database_runtime();
        let response = serde_json::to_value(
            ctx.database_runtime_refusal("retry", "db_set_host_state")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response["code"], "database_unavailable");
        assert_eq!(response["retryable"], true);
        assert!(response["message"]
            .as_str()
            .unwrap()
            .contains("configured database root is missing"));
        assert!(!ctx.claim_database_runtime_retry("db_set_host_state"));
        assert!(ctx.database_runtime_refusal("read", "read").is_none());
    }
}
