use super::*;
use crate::bash_background::BgTaskStatus;

fn checkpoint(
    task: &str,
) -> (
    Arc<crate::gh_shim_ticket::SpeechTicket>,
    crate::bash_background::persistence::PersistedTask,
) {
    let speech = crate::gh_shim_ticket::speech_for_task("dispatch-session", task)
        .pop()
        .expect("eligible task ticket");
    let conn = speech.prepared.context.service.store.connect().unwrap();
    let row = crate::db::bash_tasks::get_bash_task(&conn, "opencode", "dispatch-session", task)
        .unwrap()
        .unwrap();
    let metadata = serde_json::from_str(&row.metadata).unwrap();
    (speech, metadata)
}

async fn marker(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

async fn completed(fixture: &Fixture) {
    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = fixture
                .ctx
                .bash_background()
                .status(&fixture.task, &fixture.session, None, None, 0)
                .unwrap();
            if snapshot.info.status.is_terminal() {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
    assert_eq!(snapshot.exit_code, Some(0));
}

#[tokio::test]
async fn eligible_pty_cannot_execute_user_code_before_the_durable_seal() {
    let _lock = crate::test_env::process_env_lock();
    let command_dir = tempfile::tempdir().unwrap();
    let ran = command_dir.path().join("ran");
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let command = format!(
        "printf started > '{}'; IFS= read -r finish; exit 0",
        ran.display()
    );
    let launch = tokio::spawn(Fixture::new_with_options(
        false,
        LaunchOptions {
            pty: true,
            command: Some(command),
            pause: Some((arrived_tx, release_rx)),
            ..Default::default()
        },
    ));
    let task = tokio::task::spawn_blocking(move || {
        arrived_rx.recv_timeout(Duration::from_secs(15)).unwrap()
    })
    .await
    .unwrap();
    let (speech, metadata) = checkpoint(&task);
    assert_eq!(metadata.status, BgTaskStatus::Starting);
    assert!(speech
        .prepared
        .context
        .service
        .store
        .get(&speech.incarnation)
        .unwrap()
        .record
        .seal
        .is_none());
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !ran.exists(),
        "PTY user code executed while its identity was still unsealed"
    );
    release_tx.send(()).unwrap();
    let fixture = launch.await.unwrap();
    assert!(
        fixture.startup_error.is_none(),
        "{:?}",
        fixture.startup_error
    );
    assert!(fixture
        .service
        .store
        .get(&fixture.incarnation)
        .unwrap()
        .record
        .seal
        .is_some());
    marker(&ran).await;
    fixture
        .ctx
        .bash_background()
        .write_pty(&fixture.task, &fixture.session, b"finish\n")
        .unwrap();
    completed(&fixture).await;
}

#[tokio::test]
async fn pty_seal_failure_revokes_speech_durably_but_runs_the_command_normally() {
    let _lock = crate::test_env::process_env_lock();
    let command_dir = tempfile::tempdir().unwrap();
    let ran = command_dir.path().join("ran");
    let command = format!(
        "printf started > '{}'; IFS= read -r finish; exit 0",
        ran.display()
    );
    let fixture = Fixture::new_with_options(
        false,
        LaunchOptions {
            pty: true,
            command: Some(command),
            seal_failure: Some(Refusal::new(
                Code::AuthorityUnavailable,
                "task_unverifiable",
            )),
            ..Default::default()
        },
    )
    .await;
    assert!(
        fixture.startup_error.is_none(),
        "{:?}",
        fixture.startup_error
    );
    marker(&ran).await;
    assert_eq!(
        fixture
            .service
            .store
            .get(&fixture.incarnation)
            .unwrap_err()
            .reason,
        "revoked"
    );
    let answer = fixture.post(true, "downgraded-pty").await;
    assert_eq!(answer.data["refusal_code"], "task_gh_authority_unavailable");
    assert_eq!(answer.data["reason"], "revoked");
    assert_eq!(answer.data["message"],"this command's bot-write permission cannot be verified (revoked); start the command again from a live session, and tell the operator if it repeats.");
    fixture
        .ctx
        .bash_background()
        .write_pty(&fixture.task, &fixture.session, b"finish\n")
        .unwrap();
    completed(&fixture).await;
    let restarted = crate::db::gh_speech::Store::new(fixture.service.store.path());
    let error = restarted
        .seal(
            &fixture.incarnation,
            &crate::gh_shim_speech::model::ExecutionSeal {
                child_pid: 123,
                child_start_time_ms: 456,
                birth_identity: "birth".into(),
            },
        )
        .unwrap_err();
    assert_eq!(error.reason, "revoked");
    probe_revoked_in_a_new_process(&fixture);
    assert!(fixture.plex.state.lock().unwrap().posts.is_empty());
}

#[tokio::test]
async fn pty_revocation_commit_failure_aborts_and_reaps_without_running_user_code() {
    let _lock = crate::test_env::process_env_lock();
    let command_dir = tempfile::tempdir().unwrap();
    let ran = command_dir.path().join("ran");
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let launch = tokio::spawn(Fixture::new_with_options(
        false,
        LaunchOptions {
            pty: true,
            command: Some(format!("printf forbidden > '{}'", ran.display())),
            seal_failure: Some(Refusal::new(
                Code::AuthorityUnavailable,
                "task_unverifiable",
            )),
            fail_revocation: true,
            pause: Some((arrived_tx, release_rx)),
            ..Default::default()
        },
    ));
    let task = tokio::task::spawn_blocking(move || {
        arrived_rx.recv_timeout(Duration::from_secs(15)).unwrap()
    })
    .await
    .unwrap();
    let (_speech, metadata) = checkpoint(&task);
    let pid = metadata.child_pid.expect("gated child identity checkpoint");
    assert!(crate::bash_background::process::is_process_alive(pid));
    release_tx.send(()).unwrap();
    let fixture = launch.await.unwrap();
    assert_eq!(fixture.startup_error.as_deref(),Some("task_gh_authority_unavailable: this command's bot-write permission cannot be verified (snapshot_invalid); start the command again from a live session, and tell the operator if it repeats."));
    assert!(!ran.exists());
    assert!(
        !crate::bash_background::process::is_process_alive(pid),
        "an aborted PTY launch left its child alive"
    );
    fixture
        .service
        .store
        .connect()
        .unwrap()
        .execute_batch("DROP TRIGGER reject_pty_revocation")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture
            .service
            .store
            .get(&fixture.incarnation)
            .err()
            .is_none_or(|error| error.reason != "revoked")
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn revoked_probe_child() {
    let Some(root) = std::env::var_os("AFT_TEST_REVOKED_PTY_ROOT") else {
        return;
    };
    let incarnation = std::env::var("AFT_TEST_REVOKED_PTY_INCARNATION").unwrap();
    let ticket = std::env::var("AFT_TEST_REVOKED_PTY_TICKET").unwrap();
    let store = crate::db::gh_speech::Store::new(Path::new(&root).join("aft.db"));
    assert_eq!(
        store
            .seal(
                &incarnation,
                &crate::gh_shim_speech::model::ExecutionSeal {
                    child_pid: 123,
                    child_start_time_ms: 456,
                    birth_identity: "new-process".into()
                }
            )
            .unwrap_err()
            .reason,
        "revoked"
    );
    let service = crate::gh_shim_speech::service::record_service(Path::new(&root));
    let cold = crate::gh_shim_ticket::redeem_speech_cold(&ticket, &service).unwrap_err();
    assert_eq!(
        cold.reason, "snapshot_invalid",
        "a revoked unsealed candidate has no redemption digest"
    );
    println!("REVOKED_PTY_PROBE_EXECUTED");
}

fn probe_revoked_in_a_new_process(fixture: &Fixture) {
    let ticket = fixture.params["ticket"].as_str().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "gh_shim::speech_dispatch_tests::pty::revoked_probe_child",
            "--nocapture",
        ])
        .env(
            "AFT_TEST_REVOKED_PTY_ROOT",
            fixture.service.store.path().parent().unwrap(),
        )
        .env("AFT_TEST_REVOKED_PTY_INCARNATION", &fixture.incarnation)
        .env("AFT_TEST_REVOKED_PTY_TICKET", ticket)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("REVOKED_PTY_PROBE_EXECUTED") && stdout.contains("1 passed"));
}
