use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use subc_protocol::scope::{ScopeAttributes, ScopeKind, ScopeStatus};

fn head() -> ScopeStamp {
    ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "head-a",
        7,
        ScopeKind::Head,
    )
    .with_attributes(
        ScopeAttributes::new()
            .with_agent_id(Some("agent-a".into()))
            .with_delegates(true),
    )
    .with_owner_authorized(true)
}

fn live(scope: ScopeStamp) -> ModuleControlResponseToModule {
    ModuleControlResponseToModule::ScopeDescribe {
        status: ScopeStatus::Live,
        scope_epoch: Some(scope.scope_epoch),
        daemon_incarnation: "restarted-daemon".into(),
        owner_synced: true,
        owner_configured: true,
        scope: Some(scope),
    }
}

fn fixture() -> (tempfile::TempDir, Arc<Service>, Arc<AtomicUsize>) {
    let dir = tempfile::tempdir().unwrap();
    crate::db::open(&dir.path().join("aft.db")).unwrap();
    let service = record_service(dir.path());
    let calls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    *service.runtime.lock().unwrap() = Some(Runtime {
        handle: tokio::runtime::Handle::current(),
        relay: Weak::new(),
        describe: Arc::new(move |scope| {
            count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(live(scope)) })
        }),
        generation: "control-a".into(),
        describe_supported: true,
    });
    (dir, service, calls)
}

#[test]
fn binding_reply_diagnostics_distinguish_missing_generation_missing_rows_and_invalid_rows() {
    use serde_json::json;
    let desired = BTreeMap::from([("cortexkit/subconscious".into(), "agent-subc".into())]);
    for (reply, expected) in [
        (json!({"bindings":[]}), "missing_repo_binding_generation"),
        (json!({"repo_binding_generation":36}), "invalid_binding"),
        (
            json!({"repo_binding_generation":36,"bindings":[]}),
            "no_binding_row:",
        ),
        (
            json!({"repo_binding_generation":36,"bindings":[{"repository":"cortexkit/subconscious","agent_id":"agent-subc","app_handle_id":""}]}),
            "invalid_binding",
        ),
    ] {
        let refusal = parse_bindings(&desired, &reply, "agent-subc").unwrap_err();
        assert_eq!(refusal.code, Code::AuthorityUnavailable);
        assert_eq!(refusal.reason, "binding_unverifiable");
        assert!(
            refusal.detail.as_deref().unwrap().starts_with(expected),
            "{refusal:?}"
        );
        assert_eq!(refusal.wire_data()["detail"], refusal.detail.unwrap());
    }
}

#[test]
fn no_binding_row_preserves_main_refusal_wording_and_structured_detail() {
    let desired = BTreeMap::from([("cortexkit/subconscious".into(), "agent-subc".into())]);
    let refusal = parse_bindings(
        &desired,
        &serde_json::json!({"repo_binding_generation":36,"bindings":[]}),
        "agent-subc",
    )
    .unwrap_err();
    assert_eq!(
        refusal.agent_text(),
        "task_gh_authority_unavailable: this command's bot-write permission cannot be verified (binding_unverifiable); start the command again from a live session, and tell the operator if it repeats."
    );
    let wire = refusal.wire_data();
    assert_eq!(wire["message"], refusal.text());
    assert!(wire["detail"]
        .as_str()
        .unwrap()
        .starts_with("no_binding_row:"));
}

#[test]
fn real_plexus_binding_replies_preserve_authority_and_refuse_missing_or_wrong_agents() {
    use crate::gh_shim_relay::bindings_contract::{vector, CASES};
    let desired = BTreeMap::from([
        ("o/x".into(), "agent-x".into()),
        ("o/a".into(), "agent-y".into()),
        ("o/z".into(), "agent-y".into()),
    ]);
    for name in CASES {
        let (_, reply) = vector(name);
        let parsed = parse_bindings(&desired, &reply, "agent-x");
        match name {
            "agent" | "connection" => {
                let bindings = parsed.unwrap();
                assert_eq!(bindings.len(), 1, "{name}");
                assert_eq!(bindings["o/x"].app_handle_id, "app-x");
                assert_eq!(bindings["o/x"].repo_binding_generation, 3);
            }
            "other-agent" => {
                let refusal = parsed.unwrap_err();
                assert_eq!(refusal.code, Code::IdentityConflict);
                assert_eq!(refusal.reason, "binding_changed");
            }
            "none" | "no-grant" => {
                let refusal = parsed.unwrap_err();
                assert_eq!(refusal.code, Code::AuthorityUnavailable, "{name}");
                assert_eq!(refusal.reason, "binding_unverifiable", "{name}");
                assert!(
                    refusal
                        .detail
                        .as_deref()
                        .unwrap()
                        .starts_with("no_binding_row:"),
                    "{name}: {refusal:?}"
                );
            }
            _ => {
                assert_eq!(reply["error"]["code"], "invalid_request", "{name}");
                let refusal = parsed.unwrap_err();
                assert_eq!(refusal.code, Code::AuthorityUnavailable, "{name}");
                assert_eq!(refusal.reason, "binding_unverifiable", "{name}");
            }
        }
    }
    let bindings = parse_bindings(&desired, &vector("other-agent").1, "agent-y").unwrap();
    assert_eq!(
        bindings.keys().map(String::as_str).collect::<Vec<_>>(),
        ["o/a", "o/z"]
    );
    assert!(bindings
        .values()
        .all(|binding| binding.app_handle_id == "app-y" && binding.repo_binding_generation == 3));
}

#[test]
fn binding_reply_diagnostics_name_both_agents_without_accepting_mismatches() {
    use serde_json::json;
    let desired = BTreeMap::from([("cortexkit/subconscious".into(), "agent-subc".into())]);
    let reply = json!({"repo_binding_generation":36,"bindings":[{
        "repository":"cortexkit/subconscious","agent_id":"agent-aft","app_handle_id":"aft-alfonso"
    }]});
    let refusal = parse_bindings(&desired, &reply, "agent-subc").unwrap_err();
    assert_eq!(refusal.code, Code::IdentityConflict);
    assert_eq!(refusal.reason, "binding_changed");
    let detail = refusal.detail.unwrap();
    assert!(
        detail.contains("agent-subc") && detail.contains("agent-aft"),
        "{detail}"
    );
}

#[tokio::test]
async fn binding_read_diagnostics_distinguish_missing_relay_transport_failure_and_timeout() {
    let (_dir, service, _calls) = fixture();
    let refusal = service
        .read_bindings("/project", "session", "agent-a")
        .await
        .unwrap_err();
    assert_eq!(refusal.detail.as_deref(), Some("relay_unavailable"));

    let relay = GhRelay::new(service.root.join("missing-connection.json"));
    service.runtime.lock().unwrap().as_mut().unwrap().relay = Arc::downgrade(&relay);
    let refusal = service
        .read_bindings("/project", "session", "agent-a")
        .await
        .unwrap_err();
    assert!(
        refusal
            .detail
            .as_deref()
            .unwrap()
            .starts_with("binding_read_transport:"),
        "{refusal:?}"
    );

    let daemon = crate::gh_shim_relay::tests::fake_daemon_holding_first_request().await;
    let relay = Arc::new(GhRelay {
        transport: daemon.transport().await,
    });
    service.runtime.lock().unwrap().as_mut().unwrap().relay = Arc::downgrade(&relay);
    let refusal = service
        .read_bindings("/project", "session", "agent-a")
        .await
        .unwrap_err();
    assert_eq!(refusal.detail.as_deref(), Some("binding_read_timeout"));
}

#[tokio::test]
async fn route_loss_scope_probes_are_single_flight_and_expired_answers_are_refreshed() {
    let (_dir, service, calls) = fixture();
    let scope = head();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let (a, b) = tokio::join!(
        service.check_scope(&scope, ScopeRole::Authority, deadline),
        service.check_scope(&scope, ScopeRole::Authority, deadline),
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    service
        .cache
        .lock()
        .unwrap()
        .get_mut(&ScopeKey::from_stamp(&scope))
        .unwrap()
        .sent -= Duration::from_secs(6);
    service
        .check_scope(&scope, ScopeRole::Authority, deadline)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn newer_unknown_invalidates_live_and_late_live_cannot_restore_it() {
    let (_dir, service, _calls) = fixture();
    let scope = head();
    let old = Instant::now() - Duration::from_millis(2);
    service.remember_probe(&scope, "control-a", old, Ok(live(scope.clone())));
    service
        .check_cached_scope(&scope, ScopeRole::Authority)
        .unwrap();
    service.remember_probe(
        &scope,
        "control-a",
        Instant::now(),
        Err("not available".into()),
    );
    service.remember_probe(&scope, "control-a", old, Ok(live(scope.clone())));
    assert_eq!(
        service
            .check_cached_scope(&scope, ScopeRole::Authority)
            .unwrap_err()
            .reason,
        "error"
    );
}

#[tokio::test]
async fn final_send_scope_gate_rejects_evidence_that_expired_during_queue_wait() {
    let (_dir, service, _calls) = fixture();
    let scope = head();
    service.remember_probe(&scope, "control-a", Instant::now(), Ok(live(scope.clone())));
    service
        .check_cached_scope(&scope, ScopeRole::Authority)
        .unwrap();
    service
        .cache
        .lock()
        .unwrap()
        .get_mut(&ScopeKey::from_stamp(&scope))
        .unwrap()
        .sent -= Duration::from_secs(6);
    assert_eq!(
        service
            .check_cached_scope(&scope, ScopeRole::Authority)
            .unwrap_err()
            .reason,
        "timeout"
    );
}

#[tokio::test]
async fn scope_probe_cannot_reset_an_exhausted_shared_codemode_deadline() {
    let (_dir, service, calls) = fixture();
    let scope = head();
    service
        .check_scope(
            &scope,
            ScopeRole::Head,
            tokio::time::Instant::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
    let expired = tokio::time::Instant::now() - Duration::from_millis(1);
    assert_eq!(
        service
            .check_scope(&scope, ScopeRole::Run, expired)
            .await
            .unwrap_err()
            .reason,
        "timeout"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn older_definitive_end_fences_even_after_a_newer_live_or_unknown_answer() {
    for newer_live in [true, false] {
        let (_dir, service, _calls) = fixture();
        let scope = head();
        let weak = Arc::downgrade(&service);
        service.runtime.lock().unwrap().as_mut().unwrap().describe = Arc::new(move |queried| {
            let service = weak.upgrade().unwrap();
            let newer = if newer_live {
                Ok(live(queried.clone()))
            } else {
                Err("newer unknown".into())
            };
            service.remember_probe(&queried, "control-a", Instant::now(), newer);
            Box::pin(async move {
                Ok(ModuleControlResponseToModule::ScopeDescribe {
                    status: ScopeStatus::Ended,
                    scope_epoch: Some(queried.scope_epoch),
                    daemon_incarnation: "restarted-daemon".into(),
                    owner_synced: true,
                    owner_configured: true,
                    scope: Some(queried),
                })
            })
        });
        let refusal = service
            .check_scope(
                &scope,
                ScopeRole::Authority,
                tokio::time::Instant::now() + Duration::from_secs(10),
            )
            .await
            .unwrap_err();
        assert_eq!(
            (refusal.code, refusal.reason.as_str()),
            (Code::ScopeEnded, "ended")
        );
        assert!(service.store.scope_ended(&scope).unwrap());
    }
}
