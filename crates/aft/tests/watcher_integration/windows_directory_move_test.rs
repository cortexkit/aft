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
    directory_move(false, false);
}

#[test]
fn windows_views_on_moved_directory_reaches_ready_grep_and_search() {
    directory_move(true, false);
}

#[test]
fn windows_large_moved_directory_reaches_ready_grep_and_search() {
    directory_move(false, true);
}

#[test]
fn windows_views_on_large_moved_directory_reaches_ready_grep_and_search() {
    directory_move(true, true);
}

fn directory_move(views: bool, large: bool) {
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
    fs::write(
        incoming.join("src/lib.rs"),
        "// moved_windows_marker large_arrival_common_marker\n",
    )
    .unwrap();
    // 2,050 files exceed the 1,024-entry discovery limit. The fallback must
    // publish every file rather than acknowledge a partial listing.
    if large {
        for i in 0..2049 {
            fs::write(
                incoming.join(format!("file-{i:04}.rs")),
                "// large_arrival_common_marker\n",
            )
            .unwrap();
        }
    }

    let mut aft = start(&root, views);
    let absent = search(&mut aft, "moved_windows_marker");
    assert_eq!(absent["success"], true, "{absent}");

    let before = counters(&mut aft);
    // Rename a populated sibling directory, not a directory that was created
    // under the watcher. No child-file write occurs inside the watched root.
    fs::rename(&incoming, root.join("crates/moved")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
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
    let after = counters(&mut aft);
    let fallbacks = after["directory_walk_fallbacks_total"].as_u64().unwrap()
        - before["directory_walk_fallbacks_total"].as_u64().unwrap();
    let corpus_rescans = rescans(&after) - rescans(&before);
    if large {
        let all = grep(&mut aft, "large_arrival_common_marker");
        assert_eq!(all["index_status"], "Ready", "{all}");
        assert_eq!(all["complete"], true, "{all}");
        assert_eq!(all["total_matches"], 2050, "{all}");
        assert!(fallbacks > 0 && corpus_rescans > 0, "{before}\n{after}");
    } else {
        assert_eq!(fallbacks, 0, "{after}");
        assert_eq!(corpus_rescans, 0, "{after}");
    }
    println!("directory_move views={views} large={large} corpus_rescans={corpus_rescans} walk_fallbacks={fallbacks}");
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

#[test]
fn windows_empty_directory_needs_no_walk_or_rescan() {
    let _serial = crate::helpers::watcher_serial_lock();
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    fs::create_dir_all(root.join("crates/old/src")).unwrap();
    fs::write(
        root.join("crates/old/src/lib.rs"),
        "// initial_windows_marker\n",
    )
    .unwrap();
    let mut aft = start(root, false);
    let before = counters(&mut aft);
    let directory = root.join("crates/empty");
    fs::create_dir(&directory).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let after = counters(&mut aft);
        if after["directory_empty_total"].as_u64().unwrap()
            > before["directory_empty_total"].as_u64().unwrap()
        {
            assert_eq!(
                after["directory_subtree_walks_total"],
                before["directory_subtree_walks_total"]
            );
            assert_eq!(rescans(&after), rescans(&before));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "empty directory was not observed: {after}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    fs::write(
        directory.join("file.rs"),
        "// later_empty_directory_marker\n",
    )
    .unwrap();
    loop {
        let response = grep(&mut aft, "later_empty_directory_marker");
        if response["index_status"] == "Ready"
            && response["complete"] == true
            && response["total_matches"] == 1
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "later file event did not refresh the index: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let after = counters(&mut aft);
    assert_eq!(
        after["directory_subtree_walks_total"],
        before["directory_subtree_walks_total"]
    );
    assert_eq!(rescans(&after), rescans(&before));
    assert!(aft.shutdown().success());
}
