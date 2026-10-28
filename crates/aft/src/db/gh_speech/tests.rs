use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::Digest;
use subc_protocol::scope::{ScopeAttributes, ScopeKind, ScopeStamp, ScopeStatus};
use subc_protocol::session::ModuleControlResponseToModule;

use crate::gh_shim_speech::model::Binding;
use crate::gh_shim_speech::scope::{validate_scope_identity, Answer};
use crate::gh_shim_speech::ScopeRole;

use super::*;

fn stamp() -> ScopeStamp {
    ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "original-head",
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

fn policy(version: u64, bytes: &[u8]) -> PolicySnapshot {
    PolicySnapshot {
        artifact_id: "test-routing".into(),
        version,
        digest: sha2::Sha256::digest(bytes).to_vec(),
        signed_envelope: bytes.to_vec(),
    }
}

fn record() -> AuthorityRecord {
    AuthorityRecord {
        incarnation: "inc-a".into(),
        minting_run: "run-a".into(),
        harness: "test".into(),
        session_id: "head-session".into(),
        task_id: "bash-original".into(),
        project_root: "/project".into(),
        launch: LaunchAuthority::classic(stamp(), None).unwrap(),
        policy: Some(policy(20, b"signature-verified-by-caller fixture")),
        bindings: BTreeMap::from([(
            "owner/repo".into(),
            Binding {
                app_handle_id: "app-a".into(),
                repo_binding_generation: 1,
            },
        )]),
        seal: None,
        ineligible_reason: None,
    }
}

fn fixture() -> (tempfile::TempDir, Store, AuthorityRecord) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    drop(crate::db::open(&path).unwrap());
    (dir, Store::new(path), record())
}

fn activate(store: &Store, record: &AuthorityRecord) -> StoredAuthority {
    store
        .initialize_policy(record.policy.as_ref().unwrap(), 20)
        .unwrap();
    store.capture(record).unwrap();
    store
        .seal(
            &record.incarnation,
            &ExecutionSeal {
                child_pid: 123,
                child_start_time_ms: 456,
                birth_identity: "sealed-os-instance".into(),
            },
        )
        .unwrap();
    store
        .activate(&record.incarnation, &[7; 32], &record.minting_run)
        .unwrap();
    store.get(&record.incarnation).unwrap()
}

fn end(scope: &ScopeStamp) -> ValidatedEnd {
    let reply = ModuleControlResponseToModule::ScopeDescribe {
        status: ScopeStatus::Ended,
        scope_epoch: Some(scope.scope_epoch),
        daemon_incarnation: "daemon-a".into(),
        owner_synced: true,
        owner_configured: true,
        scope: None,
    };
    match validate_scope_identity(
        scope,
        ScopeRole::Authority,
        &reply,
        "control-a",
        "control-a",
        0,
    )
    .unwrap()
    {
        Answer::Ended(proof) => proof,
        _ => panic!("expected definitive end evidence"),
    }
}

#[test]
fn speech_connection_is_full_durable_and_busy_wait_is_short() {
    let (_dir, store, _) = fixture();
    let conn = store.connect().unwrap();
    assert_eq!(
        conn.pragma_query_value(None, "synchronous", |row| row.get::<_, u32>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        conn.pragma_query_value(None, "busy_timeout", |row| row.get::<_, u32>(0))
            .unwrap(),
        50
    );
}

#[test]
fn candidate_revocation_prevents_seal_activation_and_promotion_without_a_digest() {
    let (_dir, store, record) = fixture();
    store.capture(&record).unwrap();
    store.revoke(&record.incarnation, "cancelled").unwrap();
    assert_eq!(
        store
            .seal(
                &record.incarnation,
                &ExecutionSeal {
                    child_pid: 1,
                    child_start_time_ms: 2,
                    birth_identity: "birth".into()
                }
            )
            .unwrap_err()
            .reason,
        "revoked"
    );
    assert_eq!(
        store
            .activate(&record.incarnation, &[9; 32], "run-a")
            .unwrap_err()
            .reason,
        "revoked"
    );
    let conn = store.connect().unwrap();
    let digest: Option<Vec<u8>> = conn
        .query_row(
            "SELECT ticket_digest FROM gh_speech_authorities WHERE incarnation='inc-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(digest, None);
}

#[test]
fn task_marker_can_precede_association_creation() {
    let (_dir, store, record) = fixture();
    store
        .revoke(&record.incarnation, "cancelled-before-capture")
        .unwrap();
    assert_eq!(store.capture(&record).unwrap_err().reason, "revoked");
}

#[test]
fn recorded_end_cannot_revive_for_existing_candidates_or_future_captures() {
    let (_dir, store, record) = fixture();
    activate(&store, &record);
    store.record_end(&end(&record.launch.authority)).unwrap();
    drop(store.connect().unwrap());
    let restarted = Store::new(store.path());
    let denial = restarted.find_digest(&[7; 32], "run-b").unwrap_err();
    assert_eq!(
        (denial.code, denial.reason.as_str()),
        (Code::ScopeEnded, "ended_previously")
    );
    let mut future = record.clone();
    future.incarnation = "inc-future".into();
    future.task_id = "bash-future".into();
    assert_eq!(
        restarted.capture(&future).unwrap_err().reason,
        "ended_previously"
    );
    assert_eq!(
        restarted
            .activate(&record.incarnation, &[7; 32], "run-a")
            .unwrap_err()
            .reason,
        "ended_previously"
    );
}

#[test]
fn tuple_fence_survives_task_row_cleanup_and_unsigned_epoch_range() {
    let (_dir, store, mut record) = fixture();
    record.launch.authority.scope_epoch = u64::MAX;
    store.capture(&record).unwrap();
    store.record_end(&end(&record.launch.authority)).unwrap();
    let mut conn = store.connect().unwrap();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    tx.execute("DELETE FROM gh_speech_authorities", []).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        store.capture(&record).unwrap_err().reason,
        "ended_previously"
    );
}

#[test]
fn own_run_memory_miss_cannot_use_the_cold_path() {
    let (_dir, store, record) = fixture();
    activate(&store, &record);
    assert!(store.find_digest(&[7; 32], "run-a").is_err());
    assert_eq!(
        store
            .find_digest(&[7; 32], "run-b")
            .unwrap()
            .record
            .incarnation,
        "inc-a"
    );
}

#[test]
fn execution_seal_is_write_once_and_outer_identity_drift_is_refused() {
    let (_dir, store, record) = fixture();
    store.capture(&record).unwrap();
    let original = ExecutionSeal {
        child_pid: 123,
        child_start_time_ms: 456,
        birth_identity: "original".into(),
    };
    store.seal(&record.incarnation, &original).unwrap();
    assert_eq!(
        store
            .seal(
                &record.incarnation,
                &ExecutionSeal {
                    child_pid: 999,
                    child_start_time_ms: 456,
                    birth_identity: "other".into()
                }
            )
            .unwrap_err()
            .reason,
        "snapshot_invalid"
    );
    assert_eq!(store.get(&record.incarnation).unwrap().phase, "candidate");
    assert_eq!(
        store.get(&record.incarnation).unwrap().record.seal,
        Some(original)
    );
    let conn = store.connect().unwrap();
    conn.execute(
        "UPDATE gh_speech_authorities SET task_id='different-task'",
        [],
    )
    .unwrap();
    assert_eq!(
        store.get(&record.incarnation).unwrap_err().reason,
        "snapshot_invalid"
    );
}

#[test]
fn policy_rollback_missing_watermark_and_same_version_digest_conflict_fail_closed() {
    let (_dir, store, record) = fixture();
    let p20 = record.policy.as_ref().unwrap();
    store.initialize_policy(p20, 20).unwrap();
    let p21 = policy(21, b"new signed policy");
    assert_eq!(store.advance_policy(&p21).unwrap(), 1);
    assert_eq!(
        store.advance_policy(p20).unwrap_err().reason,
        "manifest_rollback"
    );
    assert_eq!(
        store
            .advance_policy(&policy(21, b"same-version-substitution"))
            .unwrap_err()
            .reason,
        "manifest_rollback"
    );
    let conn = store.connect().unwrap();
    conn.execute("DELETE FROM gh_speech_policy_watermarks", [])
        .unwrap();
    assert_eq!(
        store.initialize_policy(&p21, 21).unwrap_err().reason,
        "manifest_rollback"
    );
}

#[test]
fn idempotency_key_is_committed_before_final_begin_and_cannot_change_payload() {
    let (_dir, store, record) = fixture();
    let stored = activate(&store, &record);
    store
        .prepare_request("nonce-a", &record.incarnation, &[3; 32], "expect-a")
        .unwrap();
    assert!(store
        .prepare_request("nonce-a", &record.incarnation, &[4; 32], "expect-b")
        .is_err());
    let called = AtomicBool::new(false);
    store.final_gate(&record.incarnation,"nonce-a",stored.generation,0,record.policy.as_ref().unwrap(),|| {
        let other=store.connect().unwrap();
        let present:bool=other.query_row("SELECT EXISTS(SELECT 1 FROM gh_speech_requests WHERE request_nonce='nonce-a')",[],|row|row.get(0)).unwrap();
        assert!(present,"a different connection must see the nonce before final-gate commit");
        called.store(true,Ordering::SeqCst);
        Ok(())
    }).unwrap();
    assert!(called.load(Ordering::SeqCst));
}

#[test]
fn final_gate_rereads_task_tuple_and_policy_state_before_local_admission() {
    for change in ["task", "scope", "policy"] {
        let (_dir, store, record) = fixture();
        let stored = activate(&store, &record);
        store
            .prepare_request("nonce-a", &record.incarnation, &[3; 32], "expect-a")
            .unwrap();
        match change {
            "task" => store.revoke(&record.incarnation, "cancelled").unwrap(),
            "scope" => store.record_end(&end(&record.launch.authority)).unwrap(),
            "policy" => {
                store.advance_policy(&policy(21, b"newer-policy")).unwrap();
            }
            _ => unreachable!(),
        }
        let called = AtomicBool::new(false);
        let result = store.final_gate(
            &record.incarnation,
            "nonce-a",
            stored.generation,
            0,
            record.policy.as_ref().unwrap(),
            || {
                called.store(true, Ordering::SeqCst);
                Ok(())
            },
        );
        let expected = match change {
            "task" => "revoked",
            "scope" => "ended_previously",
            "policy" => "manifest_rollback",
            _ => unreachable!(),
        };
        match result {
            Err(GateError::BeforeSend(error)) => assert_eq!(error.reason, expected, "{change}"),
            result => panic!("{change}: expected {expected}, got {result:?}"),
        }
        assert!(
            !called.load(Ordering::SeqCst),
            "{change} must be checked before admission"
        );
    }
}

#[test]
fn nonholder_child() {
    let Some(path) = std::env::var_os("AFT_TEST_SPEECH_DB_PATH") else {
        return;
    };
    let root = std::path::Path::new(&path).parent().unwrap();
    assert!(crate::root_cache::acquire_speech_serving_lease(root).is_err());
    let store = Store::new(path);
    let result = store.revoke("inc-a", "nonholder-cancel");
    if std::env::var("AFT_TEST_SPEECH_DB_EXPECT").unwrap() == "busy" {
        assert_eq!(result.unwrap_err().reason, "storage_busy");
    } else {
        result.unwrap();
    }
    println!("nonholder-revocation-child-executed");
}

fn revoke_in_other_process(store: &Store, expected: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "db::gh_speech::tests::nonholder_child",
            "--nocapture",
        ])
        .env("AFT_TEST_SPEECH_DB_PATH", store.path())
        .env("AFT_TEST_SPEECH_DB_EXPECT", expected)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("nonholder-revocation-child-executed"),
        "{stdout}"
    );
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed"), "{stdout}");
}

#[test]
fn two_process_revocation_committed_first_prevents_gate_admission() {
    let (dir, store, record) = fixture();
    let _holder = crate::root_cache::acquire_speech_serving_lease(dir.path()).unwrap();
    let preflight = activate(&store, &record);
    store
        .prepare_request("nonce-a", &record.incarnation, &[3; 32], "expect-a")
        .unwrap();
    revoke_in_other_process(&store, "committed");
    let admitted = AtomicBool::new(false);
    let result = store.final_gate(
        &record.incarnation,
        "nonce-a",
        preflight.generation,
        0,
        record.policy.as_ref().unwrap(),
        || {
            admitted.store(true, Ordering::SeqCst);
            Ok(())
        },
    );
    match result {
        Err(GateError::BeforeSend(error)) => assert_eq!(error.reason, "revoked"),
        result => panic!("expected revoked, got {result:?}"),
    }
    assert!(
        !admitted.load(Ordering::SeqCst),
        "an earlier cross-process revocation commit must prevent admission"
    );
}

#[test]
fn two_process_nonholder_cannot_commit_between_gate_read_and_enqueue() {
    let (dir, store, record) = fixture();
    let _holder = crate::root_cache::acquire_speech_serving_lease(dir.path()).unwrap();
    let preflight = activate(&store, &record);
    store
        .prepare_request("nonce-a", &record.incarnation, &[3; 32], "expect-a")
        .unwrap();
    let admitted = AtomicBool::new(false);
    store
        .final_gate(
            &record.incarnation,
            "nonce-a",
            preflight.generation,
            0,
            record.policy.as_ref().unwrap(),
            || {
                // Only this test waits inside the transaction to force a competing writer.
                revoke_in_other_process(&store, "busy");
                admitted.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap();
    assert!(admitted.load(Ordering::SeqCst));
    revoke_in_other_process(&store, "committed");
    assert_eq!(
        store.get(&record.incarnation).unwrap_err().reason,
        "revoked"
    );
}

#[test]
fn legacy_bound_cancel_committed_before_enqueue_is_revoked_without_queueing() {
    let (dir, store, record) = fixture();
    let _holder = crate::root_cache::acquire_speech_serving_lease(dir.path()).unwrap();
    let preflight = activate(&store, &record);
    store
        .prepare_request(
            "legacy-before",
            &record.incarnation,
            &[3; 32],
            "legacy-route",
        )
        .unwrap();
    revoke_in_other_process(&store, "committed");
    let enqueued = AtomicBool::new(false);
    let result = store.final_legacy_gate(
        &record.incarnation,
        "legacy-before",
        preflight.generation,
        0,
        record.policy.as_ref().unwrap(),
        "forward-a",
        || {
            enqueued.store(true, Ordering::SeqCst);
            Ok(())
        },
    );
    assert!(
        !enqueued.load(Ordering::SeqCst),
        "an earlier cancellation commit must prevent even local enqueue"
    );
    match result {
        Err(GateError::BeforeSend(error)) => assert_eq!(error.reason, "revoked"),
        result => panic!("expected revoked before enqueue, got {result:?}"),
    }
    assert_eq!(
        store
            .pending_legacy_for_identity(&record.session_id, &record.task_id)
            .unwrap(),
        0
    );
}

#[test]
fn legacy_queue_full_rolls_back_dispatch_without_erasing_prepared_nonce() {
    let (_dir, store, record) = fixture();
    let stored = activate(&store, &record);
    store
        .prepare_request("legacy-full", &record.incarnation, &[3; 32], "legacy-route")
        .unwrap();
    let result: Result<(), GateError> = store.final_legacy_gate(
        &record.incarnation,
        "legacy-full",
        stored.generation,
        0,
        record.policy.as_ref().unwrap(),
        "forward-a",
        || {
            Err(Refusal::new(
                Code::AuthorityUnavailable,
                "dispatch_queue_full",
            ))
        },
    );
    match result {
        Err(GateError::BeforeSend(error)) => assert_eq!(error.reason, "dispatch_queue_full"),
        result => panic!("expected dispatch_queue_full, got {result:?}"),
    }
    assert_eq!(
        store
            .legacy_dispatch_status("legacy-full", &record.incarnation, &[3; 32])
            .unwrap(),
        None
    );
    let conn = store.connect().unwrap();
    let prepared: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM gh_speech_requests WHERE request_nonce='legacy-full'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(prepared, 1);
}

#[test]
fn legacy_enqueue_survives_consumer_loss_as_unknown_and_does_not_enqueue_twice() {
    let (_dir, store, record) = fixture();
    let stored = activate(&store, &record);
    store
        .prepare_request(
            "legacy-crash",
            &record.incarnation,
            &[3; 32],
            "legacy-route",
        )
        .unwrap();
    store
        .final_legacy_gate(
            &record.incarnation,
            "legacy-crash",
            stored.generation,
            0,
            record.policy.as_ref().unwrap(),
            "forward-a",
            || Ok(()),
        )
        .unwrap();
    let restarted = Store::new(store.path());
    assert_eq!(
        restarted
            .legacy_dispatch_status("legacy-crash", &record.incarnation, &[3; 32])
            .unwrap(),
        Some(LegacyDispatchStatus::Queued)
    );
    let second: Result<(), GateError> = restarted.final_legacy_gate(
        &record.incarnation,
        "legacy-crash",
        stored.generation,
        0,
        record.policy.as_ref().unwrap(),
        "forward-b",
        || panic!("an unknown old dispatch cannot be blindly resent"),
    );
    assert!(matches!(second, Err(GateError::AfterSend)));
}

#[test]
fn legacy_cancel_after_enqueue_keeps_the_unsettled_dispatch_visible() {
    let (dir, store, record) = fixture();
    let _holder = crate::root_cache::acquire_speech_serving_lease(dir.path()).unwrap();
    let stored = activate(&store, &record);
    store
        .prepare_request(
            "legacy-after",
            &record.incarnation,
            &[3; 32],
            "legacy-route",
        )
        .unwrap();
    store
        .final_legacy_gate(
            &record.incarnation,
            "legacy-after",
            stored.generation,
            0,
            record.policy.as_ref().unwrap(),
            "forward-a",
            || Ok(()),
        )
        .unwrap();
    revoke_in_other_process(&store, "committed");
    assert_eq!(
        store.get(&record.incarnation).unwrap_err().reason,
        "revoked"
    );
    assert_eq!(
        store
            .pending_legacy_for_identity(&record.session_id, &record.task_id)
            .unwrap(),
        1
    );
    store
        .settle_legacy_dispatch("legacy-after", &record.incarnation, "forward-a")
        .unwrap();
    assert_eq!(
        store
            .pending_legacy_for_identity(&record.session_id, &record.task_id)
            .unwrap(),
        0
    );
}
