//! Relative paths name files under the project root. With root restriction
//! off, the access check and the tool must agree on that location: the tool
//! must never resolve the spelling against the daemon's own working directory.

use serde_json::json;

use crate::helpers::AftProcess;

fn send(aft: &mut AftProcess, request: serde_json::Value) -> serde_json::Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

#[test]
fn relative_write_read_and_delete_touch_only_the_project_root() {
    let project = crate::test_helpers::tempdir();
    let cwd = crate::test_helpers::tempdir();
    std::fs::write(project.path().join("decoy.txt"), "project decoy").unwrap();
    std::fs::write(cwd.path().join("decoy.txt"), "cwd decoy").unwrap();

    let mut aft = AftProcess::spawn_in_dir(cwd.path());
    let configured = aft.configure(project.path());
    assert_eq!(configured["success"], true, "{configured:?}");

    let write = send(
        &mut aft,
        json!({"id": "w", "command": "write", "file": "new/sub/file.txt", "content": "rooted\n"}),
    );
    assert_eq!(write["success"], true, "{write:?}");
    assert_eq!(
        std::fs::read_to_string(project.path().join("new/sub/file.txt")).unwrap(),
        "rooted\n"
    );
    assert!(
        !cwd.path().join("new").exists(),
        "a relative write must not create anything under the daemon's cwd"
    );

    let read = send(
        &mut aft,
        json!({"id": "r", "command": "read", "file": "decoy.txt"}),
    );
    assert_eq!(read["success"], true, "{read:?}");
    let text = read.to_string();
    assert!(text.contains("project decoy"), "{read:?}");
    assert!(!text.contains("cwd decoy"), "{read:?}");

    let delete = send(
        &mut aft,
        json!({"id": "d", "command": "delete_file", "file": "decoy.txt"}),
    );
    assert_eq!(delete["success"], true, "{delete:?}");
    assert!(!project.path().join("decoy.txt").exists());
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("decoy.txt")).unwrap(),
        "cwd decoy"
    );

    let mut cwd_entries: Vec<_> = std::fs::read_dir(cwd.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    cwd_entries.sort();
    assert_eq!(cwd_entries, vec![std::ffi::OsString::from("decoy.txt")]);

    assert!(aft.shutdown().success());
}
