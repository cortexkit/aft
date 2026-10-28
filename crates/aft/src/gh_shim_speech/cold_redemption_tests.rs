use super::*;

#[test]
fn cold_redemption_probe_child() {
    let Some(root) = std::env::var_os("AFT_TEST_COLD_ROOT") else {
        return;
    };
    let ticket = std::env::var("AFT_TEST_COLD_TICKET").unwrap();
    let case = std::env::var("AFT_TEST_COLD_CASE").unwrap();
    let service = crate::gh_shim_speech::service::record_service(Path::new(&root));
    let store = &service.store;
    let stored = store
        .find_digest(
            &crate::gh_shim_ticket::speech_digest(&ticket).unwrap(),
            crate::gh_shim_speech::service::run_nonce().unwrap(),
        )
        .unwrap();
    let record = stored.record;
    let conn = store.connect().unwrap();
    let row = crate::db::bash_tasks::get_bash_task(
        &conn,
        &record.harness,
        &record.session_id,
        &record.task_id,
    )
    .unwrap()
    .unwrap();
    let rowid: i64 = conn
        .query_row(
            "SELECT rowid FROM bash_tasks WHERE harness=?1 AND session_id=?2 AND task_id=?3",
            rusqlite::params![record.harness, record.session_id, record.task_id],
            |row| row.get(0),
        )
        .unwrap();
    let mut metadata: Value = serde_json::from_str(&row.metadata).unwrap();
    let original_record = serde_json::to_string(&record).unwrap();
    let mut changed_record = record.clone();
    let seal = changed_record.seal.as_mut().unwrap();
    match case.as_str() {
        "positive" => {}
        "sealed-pid" => {
            seal.child_pid = u32::MAX;
            metadata["child_pid"] = json!(seal.child_pid);
            conn.execute(
                "UPDATE bash_tasks SET pid=?1 WHERE rowid=?2",
                rusqlite::params![seal.child_pid, rowid],
            )
            .unwrap();
        }
        "sealed-birth" => {
            // Keep the mirror consistent so only the live OS birth-time
            // comparison can reject this altered execution seal.
            seal.child_start_time_ms += 1;
            metadata["child_start_time_ms"] = json!(seal.child_start_time_ms);
        }
        "sealed-birth-identity" => seal.birth_identity.push_str("-different-boot"),
        "outer-pid-null-with-pgid" => {
            conn.execute(
                "UPDATE bash_tasks SET pid=NULL,pgid=?1 WHERE rowid=?2",
                rusqlite::params![seal.child_pid, rowid],
            )
            .unwrap();
        }
        "outer-pid-with-pgid" => {
            conn.execute(
                "UPDATE bash_tasks SET pid=1,pgid=?1 WHERE rowid=?2",
                rusqlite::params![seal.child_pid, rowid],
            )
            .unwrap();
        }
        "outer-harness" => {
            conn.execute(
                "UPDATE bash_tasks SET harness='different-harness' WHERE rowid=?1",
                [rowid],
            )
            .unwrap();
        }
        "outer-session" => {
            conn.execute(
                "UPDATE bash_tasks SET session_id='different-session' WHERE rowid=?1",
                [rowid],
            )
            .unwrap();
        }
        "outer-task" => {
            conn.execute(
                "UPDATE bash_tasks SET task_id='different-task' WHERE rowid=?1",
                [rowid],
            )
            .unwrap();
        }
        "outer-project" => {
            // Agree with the inner mirror, but not the authority's project.
            metadata["project_root"] = json!("/different-project");
            let key = crate::path_identity::project_scope_key(Path::new("/different-project"));
            conn.execute(
                "UPDATE bash_tasks SET project_key=?1 WHERE rowid=?2",
                rusqlite::params![key, rowid],
            )
            .unwrap();
        }
        "outer-killing" => {
            conn.execute(
                "UPDATE bash_tasks SET status='killing' WHERE rowid=?1",
                [rowid],
            )
            .unwrap();
        }
        "outer-terminal" => {
            conn.execute(
                "UPDATE bash_tasks SET status='completed' WHERE rowid=?1",
                [rowid],
            )
            .unwrap();
        }
        "inner-harness" => metadata["harness"] = json!("different-harness"),
        "inner-session" => metadata["session_id"] = json!("different-session"),
        "inner-task" => metadata["task_id"] = json!("different-task"),
        "inner-pid" => metadata["child_pid"] = json!(1),
        "inner-birth" => metadata["child_start_time_ms"] = json!(seal.child_start_time_ms + 1),
        "inner-project" => metadata["project_root"] = json!("/different-project"),
        "inner-scope" => metadata["scope"]["scope_epoch"] = json!(999),
        "inner-killing" => metadata["status"] = json!("killing"),
        "inner-terminal" => metadata["status"] = json!("completed"),
        "remote" => {
            metadata["remote"] = json!({"harness":row.harness,"session":row.session_id,
            "connection_file":null,"job_id":null,"last_seq":null,"stdout_len":0,"stderr_len":0,
            "cancel_requested":false,"terminal":null})
        }
        _ => panic!("unknown cold verifier case {case}"),
    }
    serde_json::from_value::<crate::bash_background::persistence::PersistedTask>(metadata.clone())
        .unwrap();
    conn.execute(
        "UPDATE bash_tasks SET metadata=?1 WHERE rowid=?2",
        rusqlite::params![serde_json::to_string(&metadata).unwrap(), rowid],
    )
    .unwrap();
    let changed = serde_json::to_string(&changed_record).unwrap();
    conn.execute(
        "UPDATE gh_speech_authorities SET record_json=?1,record_digest=?2 WHERE incarnation=?3",
        rusqlite::params![
            changed,
            blake3::hash(changed.as_bytes()).as_bytes(),
            record.incarnation
        ],
    )
    .unwrap();
    let result = crate::gh_shim_ticket::redeem_speech_cold(&ticket, &service);
    conn.execute("UPDATE bash_tasks SET harness=?1,session_id=?2,task_id=?3,project_key=?4,status=?5,pid=?6,pgid=?7,metadata=?8 WHERE rowid=?9",
        rusqlite::params![row.harness,row.session_id,row.task_id,row.project_key,row.status,row.pid,row.pgid,row.metadata,rowid]).unwrap();
    conn.execute(
        "UPDATE gh_speech_authorities SET record_json=?1,record_digest=?2 WHERE incarnation=?3",
        rusqlite::params![
            original_record,
            blake3::hash(original_record.as_bytes()).as_bytes(),
            record.incarnation
        ],
    )
    .unwrap();
    if case == "positive" {
        let redeemed = result.unwrap();
        assert_eq!(redeemed.launch.as_ref(), Some(&record.launch));
        assert_eq!(redeemed.session_id, record.session_id);
        assert_eq!(redeemed.task_id, record.task_id);
    } else {
        let refusal = result.unwrap_err();
        assert_eq!(refusal.code, Code::AuthorityUnavailable, "{case}");
        assert_eq!(
            refusal.reason,
            if case == "remote" {
                "guard_unsupported"
            } else {
                "task_unverifiable"
            },
            "{case}"
        );
    }
    println!("COLD_VERIFIER_EXECUTED {case}");
}

async fn cases(cases: &[&str]) {
    let fixture = Fixture::new(false).await;
    for case in std::iter::once("positive").chain(cases.iter().copied()) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "gh_shim::speech_dispatch_tests::cold::cold_redemption_probe_child",
                "--nocapture",
            ])
            .env(
                "AFT_TEST_COLD_ROOT",
                fixture.service.store.path().parent().unwrap(),
            )
            .env(
                "AFT_TEST_COLD_TICKET",
                fixture.params["ticket"].as_str().unwrap(),
            )
            .env("AFT_TEST_COLD_CASE", case)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains(&format!("COLD_VERIFIER_EXECUTED {case}"))
                && stdout.contains("1 passed"),
            "{case}: cold verifier child did not execute"
        );
    }
}

#[tokio::test]
async fn full_cold_redemption_pins_exact_original_process_birth() {
    let _lock = crate::test_env::process_env_lock();
    cases(&["sealed-pid", "sealed-birth", "sealed-birth-identity"]).await;
}

#[tokio::test]
async fn full_cold_redemption_never_uses_pgid_in_place_of_pid() {
    let _lock = crate::test_env::process_env_lock();
    cases(&[
        "outer-pid-null-with-pgid",
        "outer-pid-with-pgid",
        "inner-pid",
        "inner-birth",
    ])
    .await;
}

#[tokio::test]
async fn full_cold_redemption_compares_outer_and_inner_task_identity() {
    let _lock = crate::test_env::process_env_lock();
    cases(&[
        "outer-harness",
        "outer-session",
        "outer-task",
        "inner-harness",
        "inner-session",
        "inner-task",
        "inner-scope",
    ])
    .await;
}

#[tokio::test]
async fn full_cold_redemption_compares_both_project_identities() {
    let _lock = crate::test_env::process_env_lock();
    cases(&["outer-project", "inner-project"]).await;
}

#[tokio::test]
async fn full_cold_redemption_rejects_killing_terminal_and_remote_rows() {
    let _lock = crate::test_env::process_env_lock();
    cases(&[
        "outer-killing",
        "outer-terminal",
        "inner-killing",
        "inner-terminal",
        "remote",
    ])
    .await;
}
