use crate::helpers::{user_config, AftProcess};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

fn configure(root: &Path, storage: &Path, id: &str) -> Value {
    json!({"id":id,"command":"configure","harness":"opencode",
        "project_root":root,"storage_dir":storage,
        "config":user_config(json!({"views":{"enabled":false},
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

#[test]
fn tool_from_same_burst_waits_for_pending_admission() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("Makefile"), "").unwrap();
    std::fs::write(
        project.join("main.rs"),
        "fn caller() { callee(); }\nfn callee() {}\n",
    )
    .unwrap();
    let started = dir.path().join("probe-started");
    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_TEST_STANDALONE_INITIAL_REQUEST_COUNT", "2".as_ref()),
        ("AFT_TEST_ADMISSION_DELAY_MS", "50".as_ref()),
        ("AFT_TEST_ADMISSION_STARTED", started.as_os_str()),
    ]);
    // Both frames are queued before configure's post-ACK maintenance can run.
    aft.send_silent(&configure(&project, &dir.path().join("storage"), "bind").to_string());
    aft.send_silent(&search("first-tool").to_string());
    let ack = response(&mut aft, "bind");
    assert_eq!(ack["success"], true, "{ack}");
    assert_eq!(ack["folder"]["state"], "pending", "{ack}");
    let tool = response(&mut aft, "first-tool");
    assert_eq!(tool["success"], true, "{tool}");
    assert_ne!(tool["code"], "folder_not_admitted", "{tool}");
    assert!(tool["text"].as_str().unwrap().contains("callee"), "{tool}");
    assert!(
        started.exists(),
        "the delayed admission probe must have run"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn timed_out_admission_is_incomplete_and_next_connect_rechecks() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("Makefile"), "").unwrap();
    std::fs::write(project.join("main.rs"), "fn callee() {}\n").unwrap();
    let delay = dir.path().join("delay");
    std::fs::write(&delay, "1500").unwrap();
    let storage = dir.path().join("storage");
    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_TEST_STANDALONE_INITIAL_REQUEST_COUNT", "2".as_ref()),
        ("AFT_TEST_ADMISSION_DELAY_FILE", delay.as_os_str()),
    ]);
    aft.send_silent(&configure(&project, &storage, "slow-bind").to_string());
    aft.send_silent(&search("slow-tool").to_string());
    assert_eq!(response(&mut aft, "slow-bind")["success"], true);
    let refused = response(&mut aft, "slow-tool");
    assert_eq!(refused["code"], "folder_not_admitted", "{refused}");
    assert_eq!(refused["reason"], "evidence_budget_exhausted", "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "{refused}"
    );
    assert_eq!(refused["folder"]["coverage"]["complete"], false);
    let status = aft.send(r#"{"id":"health-after-timeout","command":"status"}"#);
    assert_eq!(
        status["folder"]["reason"], "evidence_budget_exhausted",
        "{status}"
    );
    assert_eq!(status["folder"]["coverage"]["complete"], false);
    std::fs::write(&delay, "0").unwrap();
    aft.send_silent(&configure(&project, &storage, "fast-bind").to_string());
    aft.send_silent(&search("fast-tool").to_string());
    assert_eq!(response(&mut aft, "fast-bind")["success"], true);
    let tool = response(&mut aft, "fast-tool");
    assert_eq!(tool["success"], true, "{tool}");
    assert_ne!(tool["code"], "folder_not_admitted", "{tool}");
    assert!(aft.shutdown().success());
}
