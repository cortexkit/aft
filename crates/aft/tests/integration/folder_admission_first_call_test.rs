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

fn configure(root: &Path, storage: &Path, id: &str, full_validation: bool) -> Value {
    json!({"id":id,"command":"configure","harness":"opencode",
        "project_root":root,"storage_dir":storage,
        "config":user_config(json!({"views":{"enabled":false},
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
    let dir = tempfile::tempdir().unwrap();
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
    let deadline = Instant::now() + Duration::from_secs(2);
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
    assert_eq!(tool["success"], true, "{tool}");
    assert_ne!(tool["code"], "folder_not_admitted", "{tool}");
}

#[test]
fn timed_out_admission_is_incomplete_and_next_connect_rechecks() {
    let _timing = timing_guard();
    let dir = tempfile::tempdir().unwrap();
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
    aft.send_silent(&configure(&project, &storage, "slow-bind", false).to_string());
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
    aft.send_silent(&configure(&project, &storage, "fast-bind", false).to_string());
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
