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
    let key = store::ScopeKey {
        owner: r#"{"kind":"reserved","module_id":"prefrontal-core"}"#.into(),
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
            store::revoke(&conn, &key, &dir.path().join(name)).unwrap();
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
