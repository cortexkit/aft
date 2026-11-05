use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::helpers::{user_config, AftProcess};

fn configure_with_search_index(aft: &mut AftProcess, root: &Path) {
    let configure = aft.send(
        &json!({
            "id": "cfg-search-index",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": user_config(serde_json::json!({ "indexes": { "trigram": true } })),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );
}

fn grep_marker(aft: &mut AftProcess, pattern: &str) -> Value {
    aft.send(
        &json!({
            "id": "grep-marker",
            "command": "grep",
            "pattern": pattern,
        })
        .to_string(),
    )
}

fn wait_for_ready_grep<F>(
    aft: &mut AftProcess,
    label: &str,
    pattern: &str,
    mut predicate: F,
) -> Value
where
    F: FnMut(&Value) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_response = None;
    while Instant::now() < deadline {
        let response = grep_marker(aft, pattern);
        assert_eq!(
            response["success"], true,
            "grep should succeed while waiting for {label}: {response:?}"
        );
        if response["index_status"] == "Ready" && predicate(&response) {
            return response;
        }
        last_response = Some(response);
        thread::sleep(Duration::from_millis(50));
    }

    panic!("timed out waiting for {label}; last response: {last_response:?}");
}

#[test]
fn configure_ignore_change_purges_indexed_file_from_grep() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/secret.rs"),
        "fn secret() { println!(\"purge_secret_marker\"); }\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn_with_real_watcher();
    configure_with_search_index(&mut aft, dir.path());
    wait_for_ready_grep(
        &mut aft,
        "initial indexed secret",
        "purge_secret_marker",
        |response| response["total_matches"] == 1,
    );

    let aftignore = dir.path().join(".aftignore");
    fs::write(&aftignore, "src/secret.rs\n").unwrap();
    for attempt in 0..200 {
        let response = grep_marker(&mut aft, "purge_secret_marker");
        assert_eq!(
            response["success"], true,
            "grep should succeed: {response:?}"
        );
        if response["index_status"] == "Ready" && response["total_matches"] == 0 {
            let shutdown = aft.shutdown();
            assert!(shutdown.success());
            return;
        }
        if attempt % 3 == 0 {
            thread::sleep(Duration::from_millis(100));
        }
    }

    panic!(
        "ignore-rule change did not purge indexed file; last grep: {:?}",
        grep_marker(&mut aft, "purge_secret_marker")
    );
}

#[test]
fn root_gitignore_reload_updates_search_index() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    for (label, original) in [
        ("negated", "*-old*.json\n.gitignore\n!.gitignore\n"),
        ("ordinary", "*-old*.json\n"),
    ] {
        assert_root_gitignore_reload_updates_search_index(label, original);
    }
}

/// A root `.gitignore` that ignores itself: the watcher reports its change only
/// as an ignore-rule change, never as a changed path. Today's index rebuilds
/// its matcher on that event; the data layer hears only changed paths
/// (`data_note_paths` in the watcher drain), so its checkout keeps serving the
/// file the new rule ignores.
#[test]
#[ignore = "data-layer defect: an ignore-rule change the watcher reports without a changed path (a self-ignoring .gitignore) never reaches the data layer"]
fn self_matching_root_gitignore_reload_updates_search_index() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    assert_root_gitignore_reload_updates_search_index("self-matching", "*-old*.json\n.gitignore\n");
}

fn wait_for_data_layer_search_index(aft: &mut AftProcess, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = aft.send(r#"{"id":"search-index-status","command":"status"}"#);
        if status["search_index"]["status"] == "ready" {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{label}: the data layer never published: {status:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn assert_root_gitignore_reload_updates_search_index(label: &str, original: &str) {
    let dir = tempfile::tempdir().unwrap();
    let ignore_path = dir.path().join(".gitignore");
    fs::write(&ignore_path, original).unwrap();
    fs::write(dir.path().join("seed.txt"), "seed_marker\n").unwrap();
    fs::write(dir.path().join("x-zq-0.json"), "indexed_zq_marker\n").unwrap();

    let mut aft = AftProcess::spawn_with_real_watcher();
    configure_with_search_index(&mut aft, dir.path());
    wait_for_ready_grep(
        &mut aft,
        &format!("{label} initial index"),
        "indexed_zq_marker",
        |response| response["total_matches"] == 1,
    );
    // That grep started the data layer's build of the search index; until the
    // data layer publishes it, grep answers from today's trigram index, which
    // this test does not mean to check.
    wait_for_data_layer_search_index(&mut aft, label);

    fs::write(&ignore_path, format!("{original}*-zq*.json\n")).unwrap();
    wait_for_ready_grep(
        &mut aft,
        &format!("{label} retired file"),
        "indexed_zq_marker",
        |response| response["total_matches"] == 0,
    );

    fs::write(dir.path().join("x-zq-1.json"), "new_zq_marker\n").unwrap();
    fs::write(dir.path().join("visible.txt"), "visible_zq_marker\n").unwrap();
    wait_for_ready_grep(
        &mut aft,
        &format!("{label} visible file"),
        "visible_zq_marker",
        |response| response["total_matches"] == 1,
    );
    let ignored = grep_marker(&mut aft, "new_zq_marker");
    assert_eq!(ignored["success"], true, "{label}: {ignored:?}");
    assert_eq!(ignored["total_matches"], 0, "{label}: {ignored:?}");

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn dispatch_stays_responsive_under_ignored_event_flood() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    let source = dir.path().join("src/main.ts");
    fs::write(&source, "export function main() { return 1; }\n").unwrap();
    fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
    let ignored_dir = dir.path().join("ignored");
    fs::create_dir_all(&ignored_dir).unwrap();

    let mut aft = AftProcess::spawn_with_real_watcher();
    let configure = aft.send(
        &json!({
            "id": "cfg-responsive-flood",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "config": user_config(serde_json::json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false }
            })),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let writer_dir = ignored_dir.clone();
    let writer = thread::spawn(move || {
        for i in 0..8_000 {
            fs::write(writer_dir.join(format!("event-{i}.tmp")), b"ignored flood").unwrap();
        }
    });
    thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    let response = aft.send_with_timeout(
        &json!({
            "id": "outline-during-flood",
            "command": "outline",
            "file": source,
        })
        .to_string(),
        Duration::from_secs(5),
    );
    let elapsed = started.elapsed();
    writer.join().unwrap();

    assert_eq!(
        response["success"], true,
        "outline should succeed during ignored event flood: {response:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "outline took {elapsed:?} behind ignored watcher flood"
    );

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[cfg(debug_assertions)]
#[test]
fn watcher_replays_search_edit_seen_during_in_flight_build() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    let source = dir.path().join("src/live.rs");
    fs::write(
        &source,
        r#"fn live() { println!("before_replay_marker"); }
"#,
    )
    .unwrap();

    let mut aft = AftProcess::spawn_with_real_watcher_env(&[(
        "AFT_TEST_SEARCH_REBUILD_PUBLISH_DELAY_MS",
        std::ffi::OsStr::new("1500"),
    )]);
    configure_with_search_index(&mut aft, dir.path());

    let changed = r#"fn live() { println!("after_replay_marker"); }
"#;
    let edit_deadline = Instant::now() + Duration::from_millis(700);
    while Instant::now() < edit_deadline {
        fs::write(&source, changed).unwrap();
        let _ = grep_marker(&mut aft, "after_replay_marker");
        thread::sleep(Duration::from_millis(50));
    }

    wait_for_ready_grep(
        &mut aft,
        "replayed edit after search build",
        "after_replay_marker",
        |response| response["total_matches"] == 1,
    );

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_watcher_honors_deep_nested_aftignore() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let dir = tempfile::tempdir().unwrap();
    let deep_dir = (0..10).fold(dir.path().to_path_buf(), |path, index| {
        path.join(format!("level{index}"))
    });
    fs::create_dir_all(&deep_dir).unwrap();
    fs::write(deep_dir.join(".aftignore"), "ignored.rs\n").unwrap();
    let ignored_file = deep_dir.join("ignored.rs");
    fs::write(
        &ignored_file,
        "fn ignored() { println!(\"deep_ignored_marker_before\"); }\n",
    )
    .unwrap();

    let live_file = dir.path().join("src/live.rs");
    fs::create_dir_all(live_file.parent().unwrap()).unwrap();
    fs::write(&live_file, "fn live() {}\n").unwrap();

    let mut aft = AftProcess::spawn_with_real_watcher();
    configure_with_search_index(&mut aft, dir.path());
    wait_for_ready_grep(
        &mut aft,
        "initial ignored absence",
        "deep_ignored_marker",
        |response| response["total_matches"] == 0,
    );

    let live_contents = "fn live() { println!(\"deep_live_watcher_marker\"); }\n";
    let mut saw_live_watcher_update = false;
    for _ in 0..200 {
        fs::write(&live_file, live_contents).unwrap();
        let response = grep_marker(&mut aft, "deep_live_watcher_marker");
        assert_eq!(
            response["success"], true,
            "grep should succeed: {response:?}"
        );
        if response["index_status"] == "Ready" && response["total_matches"] == 1 {
            saw_live_watcher_update = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        saw_live_watcher_update,
        "watcher should index a non-ignored edit before checking ignored edits"
    );

    let ignored_contents = "fn ignored() { println!(\"deep_ignored_marker_after\"); }\n";
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last_response = None;
    while Instant::now() < deadline {
        fs::write(&ignored_file, ignored_contents).unwrap();
        let response = grep_marker(&mut aft, "deep_ignored_marker_after");
        assert_eq!(
            response["success"], true,
            "grep should succeed: {response:?}"
        );
        assert_eq!(
            response["total_matches"], 0,
            "deep .aftignore should keep watcher from indexing ignored file: {response:?}"
        );
        last_response = Some(response);
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        last_response.is_some(),
        "deep ignored marker should be checked at least once"
    );

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}
