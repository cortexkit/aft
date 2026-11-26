use super::*;

async fn assert_worker_read_missing(f: &Fixture, requested: &Path) {
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let reply = f.read(&f.worker, requested, 7).await;
    assert_eq!(code(&reply), "not_found", "{reply}");
    assert_eq!(
        text(&reply),
        format!("read: file not found: {}", requested.display())
    );
    assert!(f.fake.calls("consent.request").is_empty());
    assert!(f.asks().is_empty());
}

#[tokio::test]
async fn consent_worker_ambiguous_longest_indexed_suffix_has_no_suggestion() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = suffix_fixture();
    worker_suffix_file(&f, "crates/broca-session-loop/src/run.rs");
    worker_suffix_file(&f, "other/broca-session-loop/src/run.rs");
    build_suffix_inventory(&f);
    assert_worker_read_missing(&f, &f.dir.path().join("broca-session-loop/src/run.rs")).await;
}

#[tokio::test]
async fn consent_worker_one_component_indexed_suffix_has_no_suggestion() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = suffix_fixture();
    worker_suffix_file(&f, "crates/broca-session-loop/src/run.rs");
    build_suffix_inventory(&f);
    assert_worker_read_missing(&f, &f.dir.path().join("not-src/run.rs")).await;
}

#[tokio::test]
async fn consent_worker_unready_indexed_suffix_has_no_suggestion() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = suffix_fixture();
    worker_suffix_file(&f, "crates/broca-session-loop/src/run.rs");
    assert_worker_read_missing(&f, &f.dir.path().join("broca-session-loop/src/run.rs")).await;
}

#[tokio::test]
async fn consent_worker_existing_outside_path_still_asks_once() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let target = f.dir.path().join("elsewhere.txt");

    let reply = f.read(&f.worker, &target, 7).await;

    assert_worker_outside_read_denied(&f, &reply);
}

#[tokio::test]
async fn consent_worker_missing_path_without_checkout_counterpart_skips_ask() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let name = format!(
        "missing-worker-path-{}",
        f.dir.path().file_name().unwrap().to_string_lossy()
    );
    let target = worker_filesystem_root(&f).join(name);
    assert_eq!(
        std::fs::symlink_metadata(&target).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_worker_read_missing(&f, &target).await;
}

#[cfg(unix)]
#[tokio::test]
async fn consent_head_reader_missing_outside_paths_do_not_hold() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    remote_policy::catalog(json!({"preset":"reader"}), &f.head, &f.ctx).unwrap();
    let missing = f.dir.path().join("missing");
    let cases: [(&str, Value, DispatchFn, &str, &str); 4] = [
        (
            "search",
            json!({"query":"needle", "path":missing}),
            aft_search_dispatch,
            "path_not_found",
            "path does not exist",
        ),
        (
            "grep",
            json!({"pattern":"needle", "path":missing}),
            grep_dispatch,
            "path_not_found",
            "grep: search path does not exist",
        ),
        (
            "glob",
            json!({"pattern":"*.rs", "path":missing}),
            glob_dispatch,
            "path_not_found",
            "glob: search path does not exist",
        ),
        (
            "read",
            json!({"filePath":missing}),
            read_dispatch,
            "not_found",
            "read: file not found",
        ),
    ];
    for (index, (name, args, dispatch, expected_code, message)) in cases.into_iter().enumerate() {
        let (_, mut replies) = f
            .send_tool_options(
                &f.head,
                name,
                args,
                10 + index as u64,
                dispatch,
                false,
                Some("reader"),
            )
            .await;
        let frame = tokio::time::timeout(Duration::from_secs(10), replies.recv())
            .await
            .unwrap()
            .unwrap();
        let reply: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(code(&reply), expected_code, "{name}: {reply}");
        assert_eq!(
            reply["structuredContent"]["message"],
            format!("{message}: {}", missing.display()),
            "{reply}"
        );
    }
    assert!(f.fake.calls("consent.request").is_empty());
    assert!(asks::for_scope(
        &f.ctx.db().unwrap().lock().unwrap(),
        &access_grants::scope_key(&f.head)
    )
    .unwrap()
    .is_empty());
    let (_, mut replies) = f
        .send_tool_options(
            &f.head,
            "read",
            json!({"filePath": f.dir.path().join("elsewhere.txt")}),
            20,
            read_dispatch,
            false,
            Some("reader"),
        )
        .await;
    let frame = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    let reply: Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(code(&reply), "access_pending", "{reply}");
    assert_eq!(f.fake.calls("consent.request").len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn consent_head_outside_missing_write_and_dangling_link_still_hold() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let missing = f.dir.path().join("missing.txt");
    let link = f.dir.path().join("dangling.txt");
    std::os::unix::fs::symlink(&missing, &link).unwrap();

    let read = f.read(&f.head, &link, 10).await;
    assert_eq!(code(&read), "access_pending", "{read}");
    let write = f
        .checkout_tool(
            &f.head,
            "write",
            json!({"filePath": missing, "content": "new"}),
            11,
            scope_dispatch,
        )
        .await;
    assert_eq!(code(&write), "access_pending", "{write}");
    let requests = f.fake.calls("consent.request");
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(!missing.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn consent_head_read_under_inaccessible_outside_parent_still_holds() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let parent = f.dir.path().join("inaccessible");
    std::fs::create_dir(&parent).unwrap();
    let target = parent.join("unknown.txt");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0)).unwrap();
    let before = std::fs::symlink_metadata(&target).unwrap_err().kind();
    let reply = f.read(&f.head, &target, 10).await;
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        before,
        std::io::ErrorKind::PermissionDenied,
        "requires an unprivileged test user"
    );
    assert_eq!(code(&reply), "access_pending", "{reply}");
    assert_eq!(f.fake.calls("consent.request").len(), 1);
}
