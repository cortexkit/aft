use super::*;
use subc_protocol::scope::{ParentState, ScopeAttributes, ScopeKind, ScopeParent, ScopeStatus};

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

fn live(scope: ScopeStamp, daemon: &str, synced: bool) -> ModuleControlResponseToModule {
    ModuleControlResponseToModule::ScopeDescribe {
        status: ScopeStatus::Live,
        scope_epoch: Some(scope.scope_epoch),
        daemon_incarnation: daemon.into(),
        owner_synced: synced,
        owner_configured: true,
        scope: Some(scope),
    }
}

#[test]
fn exact_tuple_continues_after_daemon_restart_without_an_incarnation_pin() {
    let expected = head();
    for daemon in ["old-daemon", "new-daemon"] {
        let answer = validate_scope_identity(
            &expected,
            ScopeRole::Authority,
            &live(expected.clone(), daemon, true),
            "new-control",
            "new-control",
            0,
        )
        .unwrap();
        assert!(matches!(answer,Answer::Live(stamp) if stamp==expected));
    }
}

#[test]
fn full_live_stamp_drift_and_unsynced_live_cannot_authorize() {
    let expected = head();
    for field in ["agent", "delegates", "authorized", "kind", "ref"] {
        let mut actual = expected.clone();
        match field {
            "agent" => actual.attributes.agent_id = Some("agent-b".into()),
            "delegates" => actual.attributes.delegates = false,
            "authorized" => actual.owner_authorized = false,
            "kind" => actual.kind = ScopeKind::Worker,
            "ref" => actual.scope_ref = "other-head".into(),
            _ => unreachable!(),
        }
        assert_eq!(
            validate_scope_identity(
                &expected,
                ScopeRole::Authority,
                &live(actual, "daemon", true),
                "control",
                "control",
                0
            )
            .unwrap_err()
            .reason,
            "unverifiable",
            "{field}"
        );
    }
    assert_eq!(
        validate_scope_identity(
            &expected,
            ScopeRole::Authority,
            &live(expected.clone(), "daemon", false),
            "control",
            "control",
            0
        )
        .unwrap_err()
        .reason,
        "not_synced"
    );
}

#[test]
fn epoch_move_and_late_or_mismatched_end_cannot_construct_a_tuple_fence() {
    let expected = head();
    let mut rebound = expected.clone();
    rebound.scope_epoch += 1;
    assert_eq!(
        validate_scope_identity(
            &expected,
            ScopeRole::Authority,
            &live(rebound, "daemon", true),
            "control",
            "control",
            0
        )
        .unwrap_err()
        .reason,
        "epoch_changed"
    );
    for epoch in [None, Some(8)] {
        let reply = ModuleControlResponseToModule::ScopeDescribe {
            status: ScopeStatus::Ended,
            scope_epoch: epoch,
            daemon_incarnation: "daemon".into(),
            owner_synced: true,
            owner_configured: true,
            scope: None,
        };
        assert_eq!(
            validate_scope_identity(
                &expected,
                ScopeRole::Authority,
                &reply,
                "control",
                "control",
                0
            )
            .unwrap_err()
            .reason,
            "unverifiable"
        );
    }
    let ended = ModuleControlResponseToModule::ScopeDescribe {
        status: ScopeStatus::Ended,
        scope_epoch: Some(7),
        daemon_incarnation: "daemon".into(),
        owner_synced: true,
        owner_configured: true,
        scope: None,
    };
    assert_eq!(
        validate_scope_identity(
            &expected,
            ScopeRole::Authority,
            &ended,
            "new-control",
            "old-control",
            0
        )
        .unwrap_err()
        .reason,
        "unverifiable"
    );
}

#[test]
fn codemode_run_parent_run_id_and_expiry_are_independent_of_head_liveness() {
    let h = head();
    let r = ScopeStamp::new(h.owner.clone(), "run-a", 9, ScopeKind::Ephemeral)
        .with_parent(Some(ScopeParent::new(
            h.owner.clone(),
            h.scope_ref.clone(),
            h.scope_epoch,
        )))
        .with_parent_state(Some(ParentState::Linked))
        .with_attributes(
            ScopeAttributes::new()
                .with_agent_id(Some("agent-a".into()))
                .with_run_id(Some("program-a".into())),
        )
        .with_owner_authorized(true)
        .with_expires_at_ms(Some(100));
    for field in ["parent", "state", "run_id"] {
        let mut actual = r.clone();
        match field {
            "parent" => actual.parent = Some(ScopeParent::new(h.owner.clone(), "other-head", 7)),
            "state" => actual.parent_state = Some(ParentState::Pending),
            "run_id" => actual.attributes.run_id = Some("program-b".into()),
            _ => unreachable!(),
        }
        assert_eq!(
            validate_scope_identity(
                &r,
                ScopeRole::Run,
                &live(actual, "daemon", true),
                "control",
                "control",
                0
            )
            .unwrap_err()
            .reason,
            "unverifiable"
        );
    }
    assert_eq!(
        validate_scope_identity(
            &r,
            ScopeRole::Run,
            &live(r.clone(), "daemon", true),
            "control",
            "control",
            100
        )
        .unwrap_err()
        .reason,
        "ended"
    );
    let invalid_head = h.clone().with_attributes(
        ScopeAttributes::new()
            .with_agent_id(Some("agent-a".into()))
            .with_delegates(false),
    );
    assert!(
        crate::gh_shim_speech::model::LaunchAuthority::codemode(invalid_head, r, None, None)
            .is_err()
    );
}

#[test]
fn scoped_rpc_end_requires_the_exact_selector_role_and_current_generation() {
    let scope = head();
    let selector = subc_client_rs::ScopeSelector::new(
        scope.owner.clone(),
        scope.scope_ref.clone(),
        Some(scope.scope_epoch),
    );
    let proof = ValidatedEnd::scoped_rpc(
        &scope,
        ScopeRole::Authority,
        &selector,
        "current",
        "current",
        ScopedRpcEndSource::Provider,
    )
    .unwrap();
    assert_eq!(proof.key(), &ScopeKey::from_stamp(&scope));
    for (role, actual, request_generation) in [
        (
            ScopeRole::Authority,
            subc_client_rs::ScopeSelector::new(
                scope.owner.clone(),
                scope.scope_ref.clone(),
                Some(scope.scope_epoch + 1),
            ),
            "current",
        ),
        (ScopeRole::Run, selector.clone(), "current"),
        (ScopeRole::Head, selector.clone(), "old-control"),
    ] {
        let error = ValidatedEnd::scoped_rpc(
            &scope,
            role,
            &actual,
            request_generation,
            "current",
            ScopedRpcEndSource::RouteOpen,
        )
        .unwrap_err();
        assert_eq!(
            (error.code, error.reason.as_str()),
            (Code::ScopeUnknown, "unverifiable")
        );
    }
}
