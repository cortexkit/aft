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
