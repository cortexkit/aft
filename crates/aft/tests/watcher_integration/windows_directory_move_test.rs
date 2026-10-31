#![cfg(windows)]

use std::fs;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::helpers::{user_config, AftProcess};

fn grep(aft: &mut AftProcess, marker: &str) -> Value {
    aft.send(
        &json!({
            "id": "directory-move-grep", "command": "grep", "pattern": marker, "path": "crates"
        })
        .to_string(),
    )
}

fn search(aft: &mut AftProcess, marker: &str) -> Value {
    aft.send(
        &json!({
            "id": "directory-move-search", "command": "semantic_search",
            "query": marker, "hint": "literal", "include_tests": true
        })
        .to_string(),
    )
}

#[test]
fn windows_moved_directory_reaches_ready_grep_and_search() {
    let _serial = crate::helpers::watcher_serial_lock();
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("project");
    let incoming = fixture.path().join("incoming");
    fs::create_dir_all(root.join("crates/old/src")).unwrap();
    fs::write(
        root.join("crates/old/src/lib.rs"),
        "// initial_windows_marker\n",
    )
    .unwrap();
    fs::create_dir_all(incoming.join("src")).unwrap();
    fs::write(incoming.join("src/lib.rs"), "// moved_windows_marker\n").unwrap();

    let mut aft = AftProcess::spawn_with_real_watcher();
    let configured = aft.send(
        &json!({
            "id": "directory-move-configure", "command": "configure",
            "harness": "opencode", "project_root": root,
            "config": user_config(json!({
                "indexes": {"trigram": true, "semantic": false, "callgraph": false},
                "views": {"enabled": false}
            }))
        })
        .to_string(),
    );
    assert_eq!(configured["success"], true, "{configured}");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let response = grep(&mut aft, "initial_windows_marker");
        assert_eq!(response["success"], true, "{response}");
        if response["index_status"] == "Ready" && response["total_matches"] == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "initial index did not become ready: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let absent = search(&mut aft, "moved_windows_marker");
    assert_eq!(absent["success"], true, "{absent}");

    // Rename a populated sibling directory, not a directory that was created
    // under the watcher. No child-file write occurs inside the watched root.
    fs::rename(&incoming, root.join("crates/moved")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let grep = grep(&mut aft, "moved_windows_marker");
        let search = search(&mut aft, "moved_windows_marker");
        let found = search["results"].as_array().is_some_and(|rows| {
            rows.iter().any(|row| {
                row["file"].as_str().is_some_and(|path| {
                    path.replace('\\', "/").ends_with("crates/moved/src/lib.rs")
                })
            })
        });
        if grep["index_status"] == "Ready" && grep["total_matches"] == 1 && found {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "moved child missing after native watcher delivery:\ngrep: {grep}\nsearch: {search}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(aft.shutdown().success());
}
