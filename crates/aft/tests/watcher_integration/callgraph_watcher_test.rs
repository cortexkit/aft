use crate::helpers::{fixture_path, AftProcess};

fn setup_watcher_fixture() -> (tempfile::TempDir, String) {
    let fixtures = fixture_path("callgraph");
    let tmp = tempfile::tempdir().expect("create temp dir");

    // Copy all fixture files into the temp dir
    for entry in std::fs::read_dir(&fixtures).expect("read fixtures dir") {
        let entry = entry.expect("read entry");
        let src = entry.path();
        if src.is_file() {
            let dst = tmp.path().join(entry.file_name());
            std::fs::copy(&src, &dst).expect("copy fixture file");
        }
    }

    let root = tmp.path().display().to_string();
    (tmp, root)
}

/// Poll for a watcher-driven callgraph update with retry.
///
/// Watcher tests are timing-sensitive: macOS FSEvents and Linux inotify
/// can take anywhere from milliseconds to a couple of seconds to deliver
/// file change notifications. This helper mutates, then sends ping → query in
/// a loop until the predicate matches or the timeout elapses. The ping forces
/// `drain_watcher_events` to run, which flushes any pending invalidations into
/// the callgraph.
fn poll_watcher_update_after_mutation<M, F>(
    aft: &mut AftProcess,
    query: &str,
    mut mutate: M,
    predicate: F,
    description: &str,
) -> serde_json::Value
where
    M: FnMut(u32),
    F: Fn(&serde_json::Value) -> bool,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let poll_interval = std::time::Duration::from_millis(100);
    let mut last_response = serde_json::Value::Null;
    let mut ping_id = 1000;
    let mut attempt = 0;

    while std::time::Instant::now() < deadline {
        attempt += 1;
        mutate(attempt);

        // Drain pending watcher events into the callgraph.
        ping_id += 1;
        aft.send(&format!(r#"{{"id":"ping-{}","command":"ping"}}"#, ping_id));

        let resp = aft.send(query);
        if predicate(&resp) {
            return resp;
        }
        last_response = resp;
        std::thread::sleep(poll_interval);
    }

    panic!(
        "watcher update did not propagate within 10s: {}\nlast response: {:?}",
        description, last_response
    );
}

fn initial_callgraph_response(aft: &mut AftProcess, query: &str) -> serde_json::Value {
    // A cold store may still be building after the folder is configured.
    // Retry only that signal; original result assertions still see all errors.
    poll_watcher_update_after_mutation(
        aft,
        query,
        |_| {},
        |response| response["code"] != "callgraph_building",
        "initial callgraph store should become ready",
    )
}

/// File watcher: modify a file to add a new caller, verify it appears.
#[test]
fn callgraph_watcher_add_caller() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let (_tmp, root) = setup_watcher_fixture();
    let mut aft = AftProcess::spawn_with_real_watcher();

    // Configure with temp dir
    let resp = aft.configure_project(std::path::Path::new(&root));
    assert_eq!(
        resp["success"], true,
        "configure should succeed: {:?}",
        resp
    );

    // Query callers of validate — should show processData from utils.ts
    let resp = initial_callgraph_response(
        &mut aft,
        &format!(
            r#"{{"id":"2","command":"callers","file":{},"symbol":"validate","depth":1}}"#,
            crate::helpers::json_string(&format!("{}/helpers.ts", root))
        ),
    );
    assert_eq!(
        resp["success"], true,
        "initial callers should succeed: {:?}",
        resp
    );
    let initial_total = resp["total_callers"].as_u64().unwrap();
    assert!(initial_total > 0, "validate should have initial callers");

    let new_file = std::path::Path::new(&root).join("extra_caller.ts");

    // Poll until the watcher delivers the file-create/modify event and the
    // callgraph picks up the new caller. The mutation is repeated inside the
    // poll loop so a configure response that arrives before the watcher is
    // armed cannot lose the only create event.
    let query = format!(
        r#"{{"id":"4","command":"callers","file":{},"symbol":"validate","depth":1}}"#,
        crate::helpers::json_string(&format!("{}/helpers.ts", root))
    );
    let resp = poll_watcher_update_after_mutation(
        &mut aft,
        &query,
        |attempt| {
            std::fs::write(
                &new_file,
                format!(
                    r#"import {{ validate }} from './helpers';

export function extraCheck(input: string): boolean {{
    // mutation attempt {attempt}
    return validate(input);
}}
"#
                ),
            )
            .expect("write new caller file");
        },
        |r| {
            r["success"] == true
                && r["total_callers"].as_u64().unwrap_or(0) > initial_total
                && r["callers"]
                    .as_array()
                    .map(|cs| {
                        cs.iter()
                            .any(|g| g["file"].as_str().unwrap_or("").contains("extra_caller.ts"))
                    })
                    .unwrap_or(false)
        },
        "extra_caller.ts should appear as a new caller of validate",
    );

    let new_total = resp["total_callers"].as_u64().unwrap();
    assert!(
        new_total > initial_total,
        "adding a caller should increase total_callers: initial={}, new={}",
        initial_total,
        new_total
    );

    aft.shutdown();
}

/// File watcher: remove a call from a file, verify it disappears.
#[test]
fn callgraph_watcher_remove_caller() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let (_tmp, root) = setup_watcher_fixture();
    let mut aft = AftProcess::spawn_with_real_watcher();

    // Configure with temp dir
    let resp = aft.configure_project(std::path::Path::new(&root));
    assert_eq!(
        resp["success"], true,
        "configure should succeed: {:?}",
        resp
    );

    // Query callers of validate — processData from utils.ts should be there
    let resp = initial_callgraph_response(
        &mut aft,
        &format!(
            r#"{{"id":"2","command":"callers","file":{},"symbol":"validate","depth":1}}"#,
            crate::helpers::json_string(&format!("{}/helpers.ts", root))
        ),
    );
    assert_eq!(
        resp["success"], true,
        "initial callers should succeed: {:?}",
        resp
    );
    let callers = resp["callers"].as_array().expect("callers array");
    let utils_group = callers
        .iter()
        .find(|g| g["file"].as_str().unwrap_or("").contains("utils.ts"));
    assert!(
        utils_group.is_some(),
        "validate should initially be called from utils.ts"
    );

    let utils_path = std::path::Path::new(&root).join("utils.ts");

    // Poll until the watcher delivers the file-modify event and the
    // callgraph drops the removed caller. The rewrite is repeated inside the
    // poll loop so a watcher-arming race cannot lose the only modify event.
    let query = format!(
        r#"{{"id":"4","command":"callers","file":{},"symbol":"validate","depth":1}}"#,
        crate::helpers::json_string(&format!("{}/helpers.ts", root))
    );
    poll_watcher_update_after_mutation(
        &mut aft,
        &query,
        |attempt| {
            std::fs::write(
                &utils_path,
                format!(
                    r#"export function processData(input: string): string {{
    // validate call removed on attempt {attempt}
    return input.toUpperCase();
}}
"#
                ),
            )
            .expect("rewrite utils.ts");
        },
        |r| {
            if r["success"] != true {
                return false;
            }
            // The match: utils.ts is either gone from the caller list, or
            // still listed but no longer has a `validate` callee in it.
            let callers = match r["callers"].as_array() {
                Some(cs) => cs,
                None => return false,
            };
            let utils_group = callers
                .iter()
                .find(|g| g["file"].as_str().unwrap_or("").contains("utils.ts"));
            match utils_group {
                None => true, // utils.ts disappeared — strongest signal
                Some(group) => group["callers"]
                    .as_array()
                    .map(|entries| {
                        entries
                            .iter()
                            .all(|e| e["callee"].as_str().unwrap_or("") != "validate")
                    })
                    .unwrap_or(false),
            }
        },
        "validate call should be removed from utils.ts after rewrite",
    );

    aft.shutdown();
}
