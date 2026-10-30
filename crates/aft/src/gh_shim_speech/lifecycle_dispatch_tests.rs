use super::*;
use crate::gh_shim_speech::model::AuthorityRecord;

fn rewrite(
    store: &crate::db::gh_speech::Store,
    incarnation: &str,
    change: impl FnOnce(&mut AuthorityRecord),
) {
    let mut record = store.get(incarnation).unwrap().record;
    change(&mut record);
    let text = serde_json::to_string(&record).unwrap();
    store
        .connect()
        .unwrap()
        .execute(
            "UPDATE gh_speech_authorities SET record_json=?2,record_digest=?3 WHERE incarnation=?1",
            rusqlite::params![
                incarnation,
                text,
                blake3::hash(text.as_bytes()).as_bytes().as_slice()
            ],
        )
        .unwrap();
}

async fn foreground() -> Fixture {
    let fixture = Fixture::new_with_options(
        false,
        LaunchOptions {
            foreground: true,
            ..Default::default()
        },
    )
    .await;
    assert!(
        fixture.startup_error.is_none(),
        "{:?}",
        fixture.startup_error
    );
    fixture
}

#[tokio::test]
async fn promotion_between_preflight_and_admission_posts_exactly_once() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = foreground().await;
    let session = fixture.session.clone();
    let task = fixture.task.clone();
    fixture.partners(|partners| {
        partners.before_final_gate = Some(Arc::new(move || {
            crate::gh_shim_ticket::promote_speech_task(&session, &task);
        }))
    });
    let answer = fixture.post(true, "promote-in-flight").await;
    assert!(answer.ok, "{:?}", answer.data);
    let stored = fixture.service.store.get(&fixture.incarnation).unwrap();
    assert_eq!(stored.phase, "active");
    let state = fixture.plex.state.lock().unwrap();
    assert_eq!(state.sends, 1);
    assert_eq!(state.posts, vec![fixture.params["request"].clone()]);
}

#[tokio::test]
async fn seal_spawn_between_preflight_and_admission_posts_exactly_once() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = foreground().await;
    let conn = fixture.service.store.connect().unwrap();
    let row =
        crate::db::bash_tasks::get_bash_task(&conn, "opencode", &fixture.session, &fixture.task)
            .unwrap()
            .unwrap();
    let metadata = serde_json::from_str(&row.metadata).unwrap();
    // Reconstruct the captured candidate before the spawn callback seals it.
    // The callback below uses the real, still-live child's persisted identity.
    rewrite(&fixture.service.store, &fixture.incarnation, |record| {
        record.seal = None
    });
    conn.execute(
        "UPDATE gh_speech_authorities SET generation=generation-1 WHERE incarnation=?1",
        [&fixture.incarnation],
    )
    .unwrap();
    let ticket = fixture.params["ticket"].as_str().unwrap().to_string();
    fixture.partners(|partners| {
        partners.before_final_gate = Some(Arc::new(move || {
            let _spawn = crate::gh_shim_ticket::spawn_ticket_guard(Some(&ticket));
            crate::gh_shim_ticket::seal_spawn(&metadata);
        }))
    });
    let answer = fixture.post(true, "seal-in-flight").await;
    assert!(answer.ok, "{:?}", answer.data);
    assert!(fixture
        .service
        .store
        .get(&fixture.incarnation)
        .unwrap()
        .record
        .seal
        .is_some());
    let state = fixture.plex.state.lock().unwrap();
    assert_eq!(state.sends, 1);
    assert_eq!(state.posts, vec![fixture.params["request"].clone()]);
}

async fn changed_authority_refuses(change: fn(&mut AuthorityRecord)) {
    let _lock = crate::test_env::process_env_lock();
    let fixture = foreground().await;
    let store = fixture.service.store.clone();
    let incarnation = fixture.incarnation.clone();
    fixture.partners(|partners| {
        partners.before_final_gate = Some(Arc::new(move || {
            // Keep generation unchanged: a generation-only refresh is not enough.
            rewrite(&store, &incarnation, change);
        }))
    });
    let answer = fixture.post(true, "changed-in-flight").await;
    assert_eq!(answer.data["refusal_code"], "task_gh_authority_unavailable");
    assert_eq!(
        answer.data["reason"], "snapshot_invalid",
        "{:?}",
        answer.data
    );
    let state = fixture.plex.state.lock().unwrap();
    assert_eq!(state.sends, 0);
    assert!(state.posts.is_empty());
}

#[tokio::test]
async fn changed_agent_between_preflight_and_admission_refuses() {
    changed_authority_refuses(|record| {
        record.launch.authority.attributes.agent_id = Some("different-agent".into())
    })
    .await;
}

#[tokio::test]
async fn changed_scope_owner_between_preflight_and_admission_refuses() {
    changed_authority_refuses(|record| {
        record.launch.authority.owner = subc_protocol::Principal::Reserved {
            module_id: "different-owner".into(),
        }
    })
    .await;
}

#[tokio::test]
async fn changed_scope_ref_between_preflight_and_admission_refuses() {
    changed_authority_refuses(|record| {
        record.launch.authority.scope_ref = "different-scope".into()
    })
    .await;
}

#[tokio::test]
async fn changed_scope_epoch_between_preflight_and_admission_refuses() {
    changed_authority_refuses(|record| record.launch.authority.scope_epoch += 1).await;
}

#[tokio::test]
async fn changed_route_origin_between_preflight_and_admission_refuses() {
    changed_authority_refuses(|record| record.launch.origin.as_mut().unwrap().channel += 1).await;
}

#[tokio::test]
async fn changed_flow_between_preflight_and_admission_refuses() {
    changed_authority_refuses(|record| {
        let head = &record.launch.authority;
        record.launch.run = Some(
            ScopeStamp::new(
                head.owner.clone(),
                "different-run",
                12,
                ScopeKind::Ephemeral,
            )
            .with_parent(Some(subc_protocol::scope::ScopeParent::new(
                head.owner.clone(),
                head.scope_ref.clone(),
                head.scope_epoch,
            )))
            .with_parent_state(Some(subc_protocol::scope::ParentState::Linked))
            .with_owner_authorized(true)
            .with_attributes(
                ScopeAttributes::new()
                    .with_agent_id(head.attributes.agent_id.clone())
                    .with_run_id(Some("different-flow".into())),
            )
            .with_expires_at_ms(Some(crate::db::call_ledger::now_ms() as u64 + 60_000)),
        );
    })
    .await;
}

#[tokio::test]
async fn changed_repository_set_between_preflight_and_admission_refuses() {
    changed_authority_refuses(|record| {
        record.bindings.insert(
            "different/repository".into(),
            record.bindings.values().next().unwrap().clone(),
        );
    })
    .await;
}

#[tokio::test]
async fn changed_binding_identity_between_preflight_and_admission_refuses() {
    changed_authority_refuses(|record| {
        record.bindings.values_mut().next().unwrap().app_handle_id = "different-app".into()
    })
    .await;
}

#[tokio::test]
async fn changed_binding_generation_between_preflight_and_admission_refuses() {
    changed_authority_refuses(|record| {
        record
            .bindings
            .values_mut()
            .next()
            .unwrap()
            .repo_binding_generation += 1
    })
    .await;
}

#[tokio::test]
async fn revoked_between_preflight_and_admission_refuses_without_sending() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = foreground().await;
    let service = fixture.service.clone();
    let incarnation = fixture.incarnation.clone();
    fixture.partners(|partners| {
        partners.before_final_gate = Some(Arc::new(move || {
            service.revoke(&incarnation, "cancelled").unwrap();
        }))
    });
    let answer = fixture.post(true, "revoke-in-flight").await;
    assert_eq!(answer.data["reason"], "revoked");
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn ended_scope_between_preflight_and_admission_refuses_without_sending() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = foreground().await;
    let service = fixture.service.clone();
    let scope = fixture.head.clone();
    fixture.partners(|partners| {
        partners.before_final_gate = Some(Arc::new(move || {
            let generation = service.generation();
            let reply = subc_protocol::session::ModuleControlResponseToModule::ScopeDescribe {
                status: ScopeStatus::Ended,
                scope_epoch: Some(scope.scope_epoch),
                daemon_incarnation: "fixture-daemon".into(),
                owner_synced: true,
                owner_configured: true,
                scope: None,
            };
            let answer = crate::gh_shim_speech::scope::validate_scope_identity(
                &scope,
                crate::gh_shim_speech::ScopeRole::Authority,
                &reply,
                &generation,
                &generation,
                0,
            )
            .unwrap();
            let crate::gh_shim_speech::scope::Answer::Ended(proof) = answer else {
                panic!("expected end evidence")
            };
            service.record_end(&scope, proof).unwrap();
        }))
    });
    let answer = fixture.post(true, "scope-ended-in-flight").await;
    assert_eq!(answer.data["refusal_code"], "task_gh_scope_ended");
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
    assert!(fixture.plex.state.lock().unwrap().posts.is_empty());
}
