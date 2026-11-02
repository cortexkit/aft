use crate::helpers::{user_config, AftProcess, ReleaseOnDrop};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};

fn timing_guard() -> std::sync::MutexGuard<'static, ()> {
    static TIMING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    TIMING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Configure the fixture root with the project intent its case needs.
///
/// Folder admission reads the policy from user config alone. Declaring the
/// fixture's own intent keeps its answer about this folder independent of where
/// the temporary directory happens to live; the refusing case stays a negative
/// decision for the folder itself.
fn configure(
    root: &Path,
    storage: &Path,
    id: &str,
    full_validation: bool,
    admitted: bool,
) -> Value {
    let detection = if admitted {
        json!({"mode": "project"})
    } else {
        json!({"mode": "off"})
    };
    json!({"id":id,"command":"configure","harness":"opencode",
        "project_root":root,"storage_dir":storage,
        "config":user_config(json!({"views":{"enabled":false},
            "project_detection": detection,
            "validate_on_edit": if full_validation { "full" } else { "off" },
            "indexes":{"trigram":false,"semantic":false,"callgraph":false}}))})
}

fn search(id: &str) -> Value {
    json!({"id":id,"command":"tool_call","name":"search",
        "arguments":{"pattern":"callee"}})
}

fn response(aft: &mut AftProcess, id: &str) -> Value {
    loop {
        let frame = aft
            .try_read_next_timeout(Duration::from_secs(10))
            .unwrap_or_else(|| panic!("no response for {id}"));
        if frame["id"] == id {
            return frame;
        }
    }
}

// These cases use the production wall-clock budget, so their child startups
// are serialized instead of competing for that budget with each other.
fn call_while_probe_is_held(project_evidence: bool, full_validation: bool) -> Value {
    let _timing = timing_guard();
    let dir = crate::test_helpers::tempdir();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    if project_evidence {
        crate::test_helpers::project::mark_project(&project).unwrap();
    }
    std::fs::write(
        project.join("main.rs"),
        "fn caller() { callee(); }\nfn callee() {}\n",
    )
    .unwrap();
    let started = dir.path().join("probe-started");
    let release = dir.path().join("probe-release");
    let release_on_drop = ReleaseOnDrop::new(&release);
    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_TEST_STANDALONE_INITIAL_REQUEST_COUNT", "2".as_ref()),
        ("AFT_TEST_ADMISSION_STARTED", started.as_os_str()),
        ("AFT_TEST_ADMISSION_RELEASE", release.as_os_str()),
        ("AFT_TEST_ADMISSION_TRACE", "1".as_ref()),
    ]);
    // Both frames are queued before configure's post-ACK maintenance can run.
    aft.send_silent(
        &configure(
            &project,
            &dir.path().join("storage"),
            "bind",
            full_validation,
            project_evidence,
        )
        .to_string(),
    );
    let first_tool = if full_validation {
        json!({"id":"first-tool", "command":"tool_call", "name":"write",
            "arguments":{"path":"first.txt", "content":"admitted\n"}})
    } else {
        search("first-tool")
    };
    aft.send_silent(&first_tool.to_string());
    let ack = response(&mut aft, "bind");
    assert_eq!(ack["success"], true, "{ack}");
    assert_eq!(ack["folder"]["state"], "pending", "{ack}");
    assert!(
        !dir.path().join("storage/retention/roots").exists(),
        "an admission-pending bind must not record a provisional artifact identity"
    );
    // A fixture that refuses must observe the decision itself: a check that
    // ended on its evidence budget answers evidence_budget_exhausted instead,
    // which is a different (incomplete) classification.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !started.exists() {
        assert!(Instant::now() < deadline, "admission probe did not start");
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(1)) {
            assert_ne!(frame["id"], "first-tool", "{frame}");
        }
    }
    if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(20)) {
        assert_ne!(
            frame["id"], "first-tool",
            "call escaped the held probe: {frame}"
        );
    }
    drop(release_on_drop);
    let tool = response(&mut aft, "first-tool");
    let status = aft.send(r#"{"id":"admitted-status","command":"status"}"#);
    assert_eq!(status["folder"]["state"], "decided", "{status}");
    // The fixture declares its project intent, and the call reports what that
    // decision was: the refusal is the tool held for the decision, not evidence
    // about the fixture folder (the classifier has its own coverage).
    let admitted = full_validation || project_evidence;
    assert_eq!(
        status["folder"]["classification"],
        if admitted { "project" } else { "not_project" },
        "{status}"
    );
    assert_eq!(
        status["folder"]["override_mode"],
        if admitted { "project" } else { "off" },
        "{status}"
    );
    let (status, stderr) = aft.stderr_output();
    if tool["success"] != true {
        eprintln!("{stderr}");
    }
    assert!(status.success(), "{stderr}");
    tool
}

#[test]
fn tool_from_same_burst_waits_for_pending_admission() {
    let tool = call_while_probe_is_held(true, false);
    assert_eq!(tool["success"], true, "{tool}");
    assert_ne!(tool["code"], "folder_not_admitted", "{tool}");
    assert!(tool["text"].as_str().unwrap().contains("callee"), "{tool}");
}

#[test]
fn tool_from_same_burst_waits_for_negative_decision_and_gets_approved_refusal() {
    let tool = call_while_probe_is_held(false, false);
    assert_eq!(tool["success"], false, "{tool}");
    assert_eq!(tool["code"], "folder_not_admitted", "{tool}");
    assert_eq!(tool["reason"], "no_project_evidence", "{tool}");
    assert_eq!(
        tool["message"],
        aft::folder_admission_runtime::OFF_TEXT,
        "{tool}"
    );
}

#[test]
fn full_validation_call_does_not_starve_its_admission_replay() {
    let tool = call_while_probe_is_held(true, true);
    // Persistence opens independently after bind. Its existing initializing
    // refusal is valid here; an admission refusal or timeout is not.
    assert!(
        tool["success"] == true || tool["code"] == "database_initializing",
        "{tool}"
    );
    assert_ne!(tool["code"], "folder_not_admitted", "{tool}");
}

#[test]
fn bash_queued_with_configure_keeps_foreground_orchestration() {
    let project = crate::test_helpers::tempdir();
    let storage = crate::test_helpers::tempdir();
    let mut aft =
        AftProcess::spawn_with_env(&[("AFT_TEST_STANDALONE_INITIAL_REQUEST_COUNT", "2".as_ref())]);
    let mut bind = configure(project.path(), storage.path(), "bash-bind", false, true);
    bind["config"] = crate::test_helpers::project_detection_user_config(json!({
        "bash": {"background": true},
        "indexes": {"trigram": false, "semantic": false, "callgraph": false}
    }));
    aft.send_silent(&bind.to_string());
    aft.send_silent(
        &json!({
            "id": "queued-bash", "command": "bash",
            "params": {"command": "printf admission-orchestrated", "foreground_orchestrate": true}
        })
        .to_string(),
    );
    assert_eq!(
        response(&mut aft, "bash-bind")["folder"]["state"],
        "pending"
    );
    let reply = response(&mut aft, "queued-bash");
    assert_eq!(reply["status"], "completed", "{reply}");
    assert_eq!(reply["exit_code"], 0, "{reply}");
    assert!(reply["output"]
        .as_str()
        .unwrap()
        .contains("admission-orchestrated"));
    assert!(aft.shutdown().success());
}

#[test]
fn timed_out_admission_is_incomplete_and_next_connect_rechecks() {
    let _timing = timing_guard();
    let dir = crate::test_helpers::tempdir();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    crate::test_helpers::project::mark_project(&project).unwrap();
    std::fs::write(project.join("main.rs"), "fn callee() {}\n").unwrap();
    let delay = dir.path().join("delay");
    std::fs::write(&delay, "1500").unwrap();
    let storage = dir.path().join("storage");
    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_TEST_STANDALONE_INITIAL_REQUEST_COUNT", "2".as_ref()),
        ("AFT_TEST_ADMISSION_DELAY_FILE", delay.as_os_str()),
    ]);
    aft.send_silent(&configure(&project, &storage, "slow-bind", false, true).to_string());
    aft.send_silent(&search("slow-tool").to_string());
    assert_eq!(response(&mut aft, "slow-bind")["success"], true);
    let refused = response(&mut aft, "slow-tool");
    assert_eq!(refused["code"], "folder_not_admitted", "{refused}");
    assert_eq!(refused["reason"], "evidence_budget_exhausted", "{refused}");
    assert_eq!(
        refused["message"],
        aft::folder_admission_runtime::OFF_TEXT,
        "{refused}"
    );
    assert_eq!(refused["folder"]["coverage"]["complete"], false);
    let repeated = aft.send(&search("slow-tool-again").to_string());
    assert_eq!(
        repeated["reason"], "evidence_budget_exhausted",
        "{repeated}"
    );
    assert_eq!(
        repeated["message"],
        aft::folder_admission_runtime::OFF_TEXT,
        "{repeated}"
    );
    let status = aft.send(r#"{"id":"health-after-timeout","command":"status"}"#);
    assert_eq!(
        status["folder"]["reason"], "evidence_budget_exhausted",
        "{status}"
    );
    assert_eq!(status["folder"]["coverage"]["complete"], false);
    std::fs::write(&delay, "0").unwrap();
    aft.send_silent(&configure(&project, &storage, "fast-bind", false, true).to_string());
    aft.send_silent(&search("fast-tool").to_string());
    assert_eq!(response(&mut aft, "fast-bind")["success"], true);
    let tool = response(&mut aft, "fast-tool");
    assert_eq!(tool["success"], true, "{tool}");
    assert_ne!(tool["code"], "folder_not_admitted", "{tool}");
    let (status, stderr) = aft.stderr_output();
    assert!(status.success(), "{stderr}");
    assert!(
        stderr.contains("folder admission wait ended without a complete decision"),
        "{stderr}"
    );
}
