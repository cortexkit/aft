use aft_github::gh_shim_speech::{
    model::LaunchAuthority,
    service::{record_service_with_task_decoder, service_without_task_decoder_for_test},
    test_bridge::{redeem_speech_cold, speech_digest, AuthorityRecord, ExecutionSeal},
    Code,
};
use serde_json::{json, Value};
use subc_protocol::{scope::ScopeKind, Principal};

use crate::{
    bash_background::persistence::PersistedTask,
    db::bash_tasks::{upsert_bash_task, BashTaskRow},
    gh_shim_ticket::decode_cold_task,
    protocol::BgTaskStatus,
};

#[test]
fn cold_redemption_requires_a_complete_shell_record_and_a_wired_decoder() {
    let local = tempfile::tempdir().unwrap();
    let root = local.path().canonicalize().unwrap();
    let project = root.display().to_string();
    let service = record_service_with_task_decoder(&root, decode_cold_task);
    let conn = crate::db::open(&root.join("aft.db")).unwrap();
    let harness = "test";
    let session = "cold-shell-record-session";
    let task = "cold-shell-record-task";
    let incarnation = "cold-shell-record-incarnation";
    let mint = "prior-run";
    let ticket = "00112233445566778899aabbccddeeff";
    let scope = subc_protocol::scope::ScopeStamp::new(
        Principal::Reserved {
            module_id: "prefrontal".into(),
        },
        "head-ref",
        7,
        ScopeKind::Head,
    )
    .with_attributes(
        subc_protocol::scope::ScopeAttributes::new()
            .with_agent_id(Some("agent".into()))
            .with_delegates(true),
    )
    .with_owner_authorized(true);
    let launch = LaunchAuthority::classic(scope.clone(), None).unwrap();
    let record = AuthorityRecord::unsealed_for_test(
        incarnation.into(),
        mint.into(),
        harness.into(),
        session.into(),
        task.into(),
        project.clone(),
        launch,
    );
    let pid = std::process::id();
    let start = crate::process_identity::process_start_time_ms(pid).unwrap();
    let birth = aft_base::fs_lock::speech_process_identity(pid).unwrap();
    service.store.test_capture(&record).unwrap();
    service
        .store
        .test_seal(incarnation, &ExecutionSeal::for_test(pid, start, birth))
        .unwrap();
    // Model a ticket already activated by a previous daemon; activation itself
    // is tested separately and is not part of the cold redemption path.
    assert_eq!(conn.execute(
        "UPDATE gh_speech_authorities SET phase='active', ticket_digest=?2, generation=generation+1 WHERE incarnation=?1",
        rusqlite::params![incarnation, speech_digest(ticket).unwrap().as_slice()],
    ).unwrap(), 1);

    let mut metadata = PersistedTask::starting(
        task.into(),
        session.into(),
        "echo fixture".into(),
        root.clone(),
        Some(root.clone()),
        None,
        false,
        false,
    );
    metadata.harness = Some(harness.into());
    metadata.scope = Some(scope);
    metadata.status = BgTaskStatus::Running;
    metadata.child_pid = Some(pid);
    metadata.child_start_time_ms = Some(start);
    let row = BashTaskRow {
        harness: harness.into(),
        session_id: session.into(),
        task_id: task.into(),
        project_key: crate::path_identity::project_scope_key(&root),
        command: metadata.command.clone(),
        cwd: project.clone(),
        status: "running".into(),
        exit_code: None,
        pid: Some(i64::from(pid)),
        pgid: None,
        started_at: 0,
        completed_at: None,
        stdout_path: None,
        stderr_path: None,
        compressed: false,
        timeout_ms: None,
        completion_delivered: true,
        output_bytes: None,
        metadata: serde_json::to_string(&metadata).unwrap(),
    };
    upsert_bash_task(&conn, &row).unwrap();

    let actual = redeem_speech_cold(ticket, &service).unwrap();
    assert_eq!(
        (actual.session_id, actual.task_id, actual.project_root),
        (session.into(), task.into(), project)
    );
    let invalid = |result: Result<_, aft_github::gh_shim_speech::Refusal>| {
        let error = result.unwrap_err();
        assert_eq!(error.code, Code::AuthorityUnavailable);
        assert_eq!(error.reason, "task_unverifiable");
    };
    invalid(redeem_speech_cold(
        ticket,
        &service_without_task_decoder_for_test(&root),
    ));
    let mut malformed: Value = serde_json::from_str(&row.metadata).unwrap();
    malformed["command"] = json!(123);
    assert_eq!(
        conn.execute(
            "UPDATE bash_tasks SET metadata=?1 WHERE harness=?2 AND session_id=?3 AND task_id=?4",
            rusqlite::params![malformed.to_string(), harness, session, task],
        )
        .unwrap(),
        1
    );
    invalid(redeem_speech_cold(ticket, &service));
}
