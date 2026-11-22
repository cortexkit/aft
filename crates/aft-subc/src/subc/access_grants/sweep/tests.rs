use super::*;
use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
use crate::config::Config;
use crate::subc::permissions;
use serde_json::Value;
use std::path::Path;
use subc_protocol::scope::ScopeStatus;

fn fixture() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Mutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    (dir, db)
}

fn save(db: &Db, path: &Path, name: &str, epoch: u64) -> store::ScopeKey {
    save_for(db, path, name, epoch, "prefrontal")
}

fn save_for(db: &Db, path: &Path, name: &str, epoch: u64, module: &str) -> store::ScopeKey {
    let key = store::ScopeKey {
        owner: store::owner_key(&subc_protocol::Principal::Reserved {
            module_id: module.into(),
        }),
        scope_ref: name.into(),
        epoch,
    };
    let conn = db.lock().unwrap();
    store::register_scope(&conn, &key, None, &Config::default()).unwrap();
    store::register_worker(&conn, &key).unwrap();
    store::insert(
        &conn,
        &key,
        &AccessGrant {
            path: path.into(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        },
    )
    .unwrap();
    key
}

fn reply(status: ScopeStatus, owner_synced: bool) -> ModuleControlResponseToModule {
    ModuleControlResponseToModule::ScopeDescribe {
        status,
        scope_epoch: Some(1),
        daemon_incarnation: "test".into(),
        owner_synced,
        owner_configured: true,
        scope: None,
    }
}

fn listed(db: &Db) -> Value {
    permissions::list(&db.lock().unwrap(), None, &[]).unwrap()
}

#[tokio::test]
async fn saved_grant_sweep_only_uses_the_registered_owner_and_holds_inferred_ends() {
    for module in ["prefrontal", "prefrontal-core", "other"] {
        let (dir, db) = fixture();
        let key = save_for(&db, dir.path(), "agent", 1, module);
        for (status, configured, synced, ends) in [
            (ScopeStatus::Live, false, true, false),
            (ScopeStatus::Ended, false, true, false),
            (ScopeStatus::NotLive, true, false, false),
            (ScopeStatus::Ended, true, true, true),
        ] {
            let stats = tick(db.clone(), &mut None, |stamp| {
                assert_eq!(
                    stamp.owner,
                    subc_protocol::Principal::Reserved {
                        module_id: module.into()
                    }
                );
                std::future::ready(Ok(ModuleControlResponseToModule::ScopeDescribe {
                    status,
                    scope_epoch: Some(1),
                    daemon_incarnation: "test".into(),
                    owner_synced: synced,
                    owner_configured: configured,
                    scope: None,
                }))
            })
            .await;
            assert_eq!(stats.described, 1);
            assert_eq!(stats.deleted_grants, u64::from(ends));
            let conn = db.lock().unwrap();
            assert_eq!(store::scope_ended(&conn, &key).unwrap(), ends);
            assert_eq!(store::list(&conn, &key).unwrap().is_empty(), ends);
        }
    }
}

#[tokio::test]
async fn saved_grant_sweep_rechecks_coverage_before_each_describe() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let (dir, db) = fixture();
    save(&db, dir.path(), "first", 1);
    save(&db, dir.path(), "second", 1);
    let protected = AtomicBool::new(false);
    let calls = AtomicUsize::new(0);
    let stats = tick_unwatched(
        db,
        &mut None,
        |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            protected.store(true, Ordering::Relaxed);
            std::future::ready(Ok(reply(ScopeStatus::Live, true)))
        },
        |_| protected.load(Ordering::Relaxed),
    )
    .await;
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(stats.described, 1);
}

#[tokio::test]
async fn saved_grant_sweep_older_epoch_is_covered_and_watch_loss_restores_polling() {
    use super::super::super::scope_watch::Coverage;
    let (dir, db) = fixture();
    let key = save(&db, dir.path(), "retained", 1);
    let mut newer = stamp(&key).unwrap();
    newer.scope_epoch = 9;
    let covered = Coverage {
        keys: [scope_describe::stamp_key(&newer)].into_iter().collect(),
        all: false,
    };
    let stats = tick_unwatched(
        db.clone(),
        &mut None,
        |_| async { panic!("a watched ref must not be described for an older grant epoch") },
        |stamp| covered.covers_reference(stamp),
    )
    .await;
    assert_eq!(stats.described, 0);
    assert_eq!(store::list(&db.lock().unwrap(), &key).unwrap().len(), 1);
    let stats = tick_unwatched(
        db.clone(),
        &mut None,
        |_| std::future::ready(Ok(reply(ScopeStatus::Ended, true))),
        |stamp| Coverage::default().covers_reference(stamp),
    )
    .await;
    assert_eq!(stats.described, 1);
    assert_eq!(stats.deleted_grants, 1);
    assert!(store::scope_ended(&db.lock().unwrap(), &key).unwrap());
}

#[tokio::test]
async fn saved_grant_sweep_live_table_shape_describe_counts() {
    use super::super::super::scope_watch::Coverage;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (dir, db) = fixture();
    let mut covered = Coverage::default();
    for (index, count) in [3, 4, 1, 1, 1].into_iter().enumerate() {
        for grant in 0..count {
            let key = save(
                &db,
                &dir.path().join(format!("{index}-{grant}")),
                &format!("scope-{index}"),
                1,
            );
            covered
                .keys
                .insert(scope_describe::stamp_key(&stamp(&key).unwrap()));
        }
    }
    assert_eq!(listed(&db)["grants"].as_array().unwrap().len(), 10);
    for (coverage, expected) in [
        (covered, vec![0, 0, 0]),
        (Coverage::default(), vec![4, 1, 0]),
    ] {
        let calls = AtomicUsize::new(0);
        let mut cursor = None;
        let mut per_tick = Vec::new();
        loop {
            let before = calls.load(Ordering::Relaxed);
            tick_unwatched(
                db.clone(),
                &mut cursor,
                |_| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    std::future::ready(Ok(reply(ScopeStatus::Live, true)))
                },
                |stamp| coverage.covers_reference(stamp),
            )
            .await;
            per_tick.push(calls.load(Ordering::Relaxed) - before);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(per_tick, expected);
    }
}

#[tokio::test]
async fn saved_grant_sweep_only_deletes_definite_ends() {
    let (dir, db) = fixture();
    for (name, expected_end) in [
        ("ended", true),
        ("synced", true),
        ("unsynced", false),
        ("error", false),
        ("timeout", false),
        ("live", false),
        ("unknown", false),
    ] {
        let key = save(&db, &dir.path().join(name), name, 1);
        let stats = tick(db.clone(), &mut None, |_| async {
            match name {
                "ended" => Ok(reply(ScopeStatus::Ended, false)),
                "synced" => Ok(reply(ScopeStatus::NotLive, true)),
                "unsynced" => Ok(reply(ScopeStatus::NotLive, false)),
                "error" => Err(readiness::ControlError::ConnectionClosed),
                "timeout" => Err(readiness::ControlError::Timeout),
                "unknown" => Ok(ModuleControlResponseToModule::CatalogUpdate {}),
                _ => Ok(reply(ScopeStatus::Live, true)),
            }
        })
        .await;
        assert_eq!(stats.described, 1);
        assert_eq!(stats.deleted_grants, u64::from(expected_end));
        let conn = db.lock().unwrap();
        assert_eq!(
            store::scope_ended(&conn, &key).unwrap(),
            expected_end,
            "{name}"
        );
        assert_eq!(
            store::list(&conn, &key).unwrap().is_empty(),
            expected_end,
            "{name}"
        );
        if !expected_end {
            // Grants are stored in policy form (on Windows the temp folder's
            // spelling differs), and the sweep must start from an empty table
            // each round so it describes exactly one scope.
            let stored = crate::policy_path::policy_path(&dir.path().join(name));
            assert_eq!(store::revoke(&conn, &key, &stored).unwrap(), 1, "{name}");
        }
        match name {
            "error" => assert_eq!(stats.errors, 1),
            "timeout" => assert_eq!(stats.timeouts, 1),
            "unsynced" | "unknown" => assert_eq!(stats.indeterminate, 1),
            "live" => assert_eq!(stats.live, 1),
            _ => assert_eq!(stats.ended, 1),
        }
    }
}

#[tokio::test]
async fn saved_grant_sweep_is_bounded_deduplicated_and_advances() {
    let (dir, db) = fixture();
    for epoch in 1..=2 {
        save(&db, &dir.path().join("same"), "a", epoch);
    }
    for name in ["b", "c", "d", "e", "f"] {
        save(&db, &dir.path().join(name), name, 1);
    }
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let describe = |_| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async { Ok(reply(ScopeStatus::Live, true)) }
    };
    let mut cursor = None;
    let first = tick(db.clone(), &mut cursor, &describe).await;
    assert_eq!(first.described, 3, "two epochs of a share one describe");
    assert!(calls.load(std::sync::atomic::Ordering::SeqCst) <= PER_TICK);
    let second = tick(db, &mut cursor, &describe).await;
    assert_eq!(second.described, 3);
    assert_eq!(
        cursor.unwrap().scope_ref,
        "f",
        "live early rows must not starve later scopes"
    );
}

#[tokio::test]
async fn saved_grant_sweep_slow_describe_does_not_block_permissions_call() {
    let (dir, db) = fixture();
    save(&db, &dir.path().join("live"), "live", 1);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let worker = tokio::spawn({
        let db = db.clone();
        let entered = entered.clone();
        let release = release.clone();
        async move {
            tick(db, &mut None, |_| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(reply(ScopeStatus::Live, true))
                }
            })
            .await
        }
    });
    entered.notified().await;
    let reply = tokio::time::timeout(
        Duration::from_secs(1),
        tokio::task::spawn_blocking(move || listed(&db)),
    )
    .await
    .expect("permissions.list waited for a daemon describe")
    .unwrap();
    assert_eq!(reply["grants"].as_array().unwrap().len(), 1);
    release.notify_one();
    assert_eq!(worker.await.unwrap().live, 1);
}

#[tokio::test(start_paused = true)]
async fn saved_grant_sweep_deadline_and_database_failure_keep_grants() {
    let (dir, db) = fixture();
    let key = save(&db, &dir.path().join("kept"), "kept", 1);
    let timeout = tick(db.clone(), &mut None, |_| std::future::pending()).await;
    assert_eq!(timeout.timeouts, 1);
    assert_eq!(listed(&db)["grants"].as_array().unwrap().len(), 1);
    db.lock().unwrap().execute_batch("CREATE TRIGGER fail_grant_delete BEFORE DELETE ON access_grants BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let failed = tick(db.clone(), &mut None, |_| async {
        Ok(reply(ScopeStatus::Ended, true))
    })
    .await;
    assert_eq!(failed.database_errors, 1);
    assert!(
        !store::scope_ended(&db.lock().unwrap(), &key).unwrap(),
        "end and delete must roll back together"
    );
    assert_eq!(listed(&db)["grants"].as_array().unwrap().len(), 1);
    let metrics = DispatchPathMetrics::new();
    report(&metrics, timeout);
    report(&metrics, failed);
    let health = metrics
        .scope_descriptions
        .lock()
        .unwrap()
        .grant_sweep_health();
    assert_eq!(
        health["errors"], 2,
        "timeouts and database failures count as errors"
    );
    assert_eq!(health["scopes_checked"], 2);
    assert_eq!(health["cant_tell"], 0);
    assert!(health["last_pass_at_ms"].as_i64().unwrap() > 0);
    assert_eq!(health.as_object().unwrap().len(), 5);
}

#[cfg(unix)]
#[tokio::test]
async fn saved_grant_sweep_permissions_list_size_1650() {
    let (_dir, db) = fixture();
    {
        let conn = db.lock().unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        for index in 0..1650 {
            let scope = index % 1073;
            let key = store::ScopeKey {
                owner: r#"{"kind":"reserved","module_id":"prefrontal-core"}"#.into(),
                scope_ref: format!("worker:{scope:04}"),
                epoch: 1,
            };
            store::register_scope(&tx, &key, None, &Config::default()).unwrap();
            store::register_worker(&tx, &key).unwrap();
            let path = format!("/Volumes/Lexar/targets/bg_{scope:016x}/ci-logs/{index:04}");
            store::insert(
                &tx,
                &key,
                &AccessGrant {
                    path: path.into(),
                    kind: GrantKind::Subtree,
                    operation: AccessOperation::Read,
                },
            )
            .unwrap();
        }
        tx.execute(
            "UPDATE access_grants SET granted_at_ms=1794234135000,source=NULL",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let before = listed(&db);
    assert_eq!(before["grants"].as_array().unwrap().len(), 1650);
    let mut legacy = before.clone();
    for row in legacy["grants"].as_array_mut().unwrap() {
        row.as_object_mut().unwrap().remove("source");
        row.as_object_mut().unwrap().remove("granted_at_bound");
    }
    let mut cursor = None;
    loop {
        let stats = tick(db.clone(), &mut cursor, |stamp| async move {
            let live = stamp.scope_ref[7..].parse::<usize>().unwrap() < 25;
            Ok(reply(
                if live {
                    ScopeStatus::Live
                } else {
                    ScopeStatus::Ended
                },
                true,
            ))
        })
        .await;
        assert!(stats.described <= PER_TICK as u64);
        assert_eq!(stats.database_errors, 0);
        if cursor.is_none() {
            break;
        }
    }
    let after = listed(&db);
    assert_eq!(after["grants"].as_array().unwrap().len(), 50);
    for row in after["grants"].as_array().unwrap() {
        assert!(row["granted_at_ms"].is_i64());
        assert_eq!(row["source"], "unknown");
        assert_eq!(row["granted_at_bound"], "at_or_before");
    }
    println!("permissions_list_size rows=1650 legacy_reply_bytes={} sourced_before_sweep_bytes={} remaining_rows=50 after_sweep_bytes={}", serde_json::to_vec(&legacy).unwrap().len(), serde_json::to_vec(&before).unwrap().len(), serde_json::to_vec(&after).unwrap().len());
}
