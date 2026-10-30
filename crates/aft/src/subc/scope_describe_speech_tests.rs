use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn fenced_end_retries_one_failed_read_and_kills_tasks_with_scope_revoked_reason() {
    use crate::bash_background::{BashShell, BgTaskStatus, HardKill, TaskSlot};
    use crate::gh_shim_speech::{Code, Refusal, ScopeRole};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.storage_dir = Some(dir.path().join("store"));
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    let db_path = ctx.storage_dir().join("aft.db");
    ctx.set_db(Arc::new(StdMutex::new(crate::db::open(&db_path).unwrap())));
    let stamp: ScopeStamp = serde_json::from_value(json!({
        "owner":{"kind":"reserved","module_id":"prefrontal-core"},
        "ref":"fenced-read-error", "scope_epoch":10, "kind":"head", "owner_authorized":true
    }))
    .unwrap();
    let task = crate::bash_background::registry::with_scope_spawn(Some(stamp.clone()), || {
        ctx.bash_background().spawn_with_shell_in_slot(
            crate::sandbox_spawn::SpawnPlan::Unsandboxed,
            "sleep 30",
            BashShell::Bash,
            crate::bash_background::registry::resolve_posix_shell(),
            "fanout-session".into(),
            root.clone(),
            HashMap::new(),
            HardKill::After(Duration::from_secs(30)),
            None,
            ctx.storage_dir(),
            TaskSlot::Background { max: 10 },
            true,
            false,
            Some(root.clone()),
        )
    })
    .unwrap();
    let store = crate::db::gh_speech::Store::new(&db_path);
    let answer = ModuleControlResponseToModule::ScopeDescribe {
        status: subc_protocol::scope::ScopeStatus::Ended,
        scope_epoch: Some(10),
        daemon_incarnation: "daemon-a".into(),
        owner_synced: true,
        owner_configured: true,
        scope: None,
    };
    let proof = match crate::gh_shim_speech::scope::validate_scope_identity(
        &stamp,
        ScopeRole::Authority,
        &answer,
        "control",
        "control",
        0,
    )
    .unwrap()
    {
        crate::gh_shim_speech::scope::Answer::Ended(proof) => proof,
        _ => panic!("expected exact end proof"),
    };
    store.record_end(&proof).unwrap();
    let executor = Arc::new(Executor::new());
    executor.register_actor(ProjectRootId::from_path(&root).unwrap(), ctx.clone());
    let reads = AtomicUsize::new(0);
    let notice = tokio::time::timeout(
        Duration::from_secs(5),
        finish_fenced_notice(executor, stamp.clone(), || {
            if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(Refusal::new(Code::AuthorityUnavailable, "storage_busy"))
            } else {
                store.scope_ended(&stamp)
            }
        }),
    )
    .await
    .unwrap();
    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = ctx
                .bash_background()
                .status(
                    &task,
                    "fanout-session",
                    Some(&root),
                    Some(&ctx.storage_dir()),
                    0,
                )
                .unwrap();
            if notice.is_none() || snapshot.info.status == BgTaskStatus::Killed {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let _ = ctx.bash_background().kill(&task, "fanout-session");
    assert_eq!(
        snapshot.info.status_reason.as_deref(),
        Some("scope_revoked")
    );
    assert_eq!(snapshot.info.status, BgTaskStatus::Killed);
    assert!(notice.is_some());
    assert_eq!(reads.load(Ordering::SeqCst), 2);
}
