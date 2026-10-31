#![cfg(windows)]

use std::fs;
use std::path::Path;
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
    directory_move(false);
}

#[test]
fn windows_views_on_moved_directory_reaches_ready_grep_and_search() {
    directory_move(true);
}

fn directory_move(views: bool) {
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

    let mut aft = start(&root, views);
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
        if grep["index_status"] == "Ready"
            && grep["complete"] == true
            && grep["total_matches"] == 1
            && found
        {
            if views {
                assert_eq!(grep["files_read_directly"], 0, "{grep}");
            }
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

fn start(root: &Path, views: bool) -> AftProcess {
    let mut aft = AftProcess::spawn_with_real_watcher();
    let configured = aft.send(
        &json!({
            "id": "directory-move-configure", "command": "configure",
            "harness": "opencode", "project_root": root,
            "config": user_config(json!({
                "indexes": {"trigram": true, "semantic": false, "callgraph": false},
                "views": {"enabled": views}
            }))
        })
        .to_string(),
    );
    assert_eq!(configured["success"], true, "{configured}");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let response = grep(&mut aft, "initial_windows_marker");
        if response["index_status"] == "Ready"
            && response["complete"] == true
            && response["total_matches"] == 1
        {
            return aft;
        }
        assert!(
            Instant::now() < deadline,
            "initial index did not become ready: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn counters(aft: &mut AftProcess) -> Value {
    let status = aft.send(&json!({"id":"directory-counters", "command":"status"}).to_string());
    assert_eq!(status["success"], true, "{status}");
    status["watcher"].clone()
}

fn rescans(counters: &Value) -> u64 {
    [
        "rescans_buffer_overflow_total",
        "rescans_kernel_dropped_total",
        "rescans_user_dropped_total",
        "rescans_unknown_total",
    ]
    .into_iter()
    .map(|key| counters[key].as_u64().unwrap())
    .sum()
}

#[test]
fn windows_directory_arrival_cost_200() {
    let _serial = crate::helpers::watcher_serial_lock();
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    fs::create_dir_all(root.join("crates/old/src")).unwrap();
    fs::create_dir(root.join("crates/measure")).unwrap();
    fs::write(
        root.join("crates/old/src/lib.rs"),
        "// initial_windows_marker\n",
    )
    .unwrap();
    let mut aft = start(root, false);
    let before = counters(&mut aft);
    for i in 0..200 {
        let directory = root.join(format!("crates/measure/arrival-{i:03}"));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("file.rs"), "// directory_cost_marker\n").unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(90);
    let after = loop {
        let response = grep(&mut aft, "directory_cost_marker");
        let observed = counters(&mut aft);
        if observed["directory_arrivals_total"].as_u64().unwrap()
            >= before["directory_arrivals_total"].as_u64().unwrap() + 200
            && response["index_status"] == "Ready"
            && response["complete"] == true
            && response["total_matches"] == 200
        {
            break observed;
        }
        assert!(
            Instant::now() < deadline,
            "arrival workload did not settle:\n{response}\n{observed}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let rescan_count = rescans(&after) - rescans(&before);
    let walks = after["directory_subtree_walks_total"].as_u64().unwrap()
        - before["directory_subtree_walks_total"].as_u64().unwrap();
    println!("directory_arrival_cost mkdir=200 writes=200 corpus_rescans={rescan_count} subtree_walks={walks}");
    assert!(aft.shutdown().success());
    assert_eq!(
        rescan_count, 0,
        "small directory arrivals must not rebuild the corpus"
    );
    assert!(walks <= 200);
}
