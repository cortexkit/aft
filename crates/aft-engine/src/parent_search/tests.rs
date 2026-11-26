//! Unit tests for parent folder discovery and the session registry.

use super::*;

#[cfg(unix)]
#[test]
fn discovery_skips_repositories_on_different_devices() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    git_init(&root.join("local"));
    git_init(&root.join("mounted").join("external"));
    crate::watcher_filter::EXCLUSION_TEST_DEVICE
        .with(|device| *device.borrow_mut() = Some(root.join("mounted")));
    let found = walk(&root, MAX_DISCOVERY_ENTRIES, &|_| false);
    crate::watcher_filter::EXCLUSION_TEST_DEVICE.with(|device| *device.borrow_mut() = None);
    assert_eq!(found.repositories, vec![root.join("local")]);
}

fn git_init(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    // Discovery only looks for the `.git` marker, so an empty one suffices.
    std::fs::create_dir_all(dir.join(".git")).unwrap();
}

#[test]
fn discovery_finds_children_and_reports_the_rest_as_skipped_and_outside() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    for name in ["b", "a", "c"] {
        git_init(&root.join(name));
    }
    git_init(&root.join("group").join("nested"));
    std::fs::write(root.join("notes.md"), "notes").unwrap();
    std::fs::create_dir_all(root.join("node_modules").join("pkg")).unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    git_init(&root.join(".hidden").join("repo"));

    let found = discover(&root, 3).unwrap();
    assert_eq!(
        found.children,
        vec![root.join("a"), root.join("b"), root.join("c")]
    );
    assert_eq!(found.skipped, vec![root.join("group").join("nested")]);
    assert_eq!(found.outside, vec![root.join("notes.md")]);
}

#[test]
fn discovery_refuses_checkouts_and_folders_without_repositories() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    assert!(discover(&root, DEFAULT_MAX_CHILD_REPOS).is_none());
    git_init(&root.join("child"));
    assert!(discover(&root, DEFAULT_MAX_CHILD_REPOS).is_some());
    // A subfolder of a checkout keeps the checkout's own indexes.
    assert!(discover(&root.join("child"), DEFAULT_MAX_CHILD_REPOS).is_none());
    std::fs::create_dir_all(root.join("child").join("src")).unwrap();
    assert!(discover(&root.join("child").join("src"), DEFAULT_MAX_CHILD_REPOS).is_none());
}

#[test]
fn default_cap_covers_the_operators_projects_folder() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    for index in 0..39 {
        git_init(&root.join(format!("repo-{index:02}")));
    }
    let found = discover(&root, DEFAULT_MAX_CHILD_REPOS).unwrap();
    assert_eq!(found.children.len(), 39);
    assert!(found.skipped.is_empty());
    let capped = discover(&root, 32).unwrap();
    assert_eq!(capped.children.len(), 32);
    assert_eq!(capped.skipped.len(), 7);
}

fn context() -> crate::context::AppContext {
    crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config::default(),
    )
}

#[test]
fn external_search_uses_the_callers_saved_worker_grants() {
    use crate::access_policy::{AccessGrant, AccessOperation, AskRoute, GrantKind};
    use crate::db::access_grants::{self, ScopeKey};
    use crate::db::remote_exec::PolicyKey;
    use std::sync::{Arc, Mutex};

    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let worker_root = base.join("worker");
    let external = base.join("external");
    let unapproved = base.join("unapproved");
    std::fs::create_dir(&worker_root).unwrap();
    for root in [&external, &unapproved] {
        git_init(root);
        std::fs::write(root.join("lib.rs"), "pub fn external_launch_marker() {}\n").unwrap();
    }
    let storage = base.join("storage");
    let caller = context();
    caller.update_config(|config| {
        config.project_root = Some(worker_root.clone());
        config.storage_dir = Some(storage.clone());
        config.indexes.semantic = false;
    });
    let indexed = context();
    indexed.update_config(|config| {
        config.project_root = Some(external.clone());
        config.storage_dir = Some(storage.clone());
        config.indexes.trigram = true;
        config.indexes.semantic = false;
        config.indexes.callgraph = false;
    });
    crate::data_layer::surfaces::serve_text_index_for_test(&indexed);

    caller.set_db(Arc::new(Mutex::new(
        crate::db::open(&storage.join("aft.db")).unwrap(),
    )));
    let launch = PolicyKey {
        project_root: worker_root.display().to_string(),
        harness: "runner".into(),
        session: "worker-session".into(),
        principal: "reserved:broca".into(),
        owner: r#"{"kind":"reserved","module_id":"prefrontal"}"#.into(),
        scope_ref: "external-search-worker".into(),
        epoch: 1,
        preset: "worker".into(),
    };
    let scope = ScopeKey::from_policy(&launch);
    let grant = AccessGrant {
        path: crate::policy_path::policy_path(&external),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Read,
    };
    {
        let db = caller.db().unwrap();
        let conn = db.lock().unwrap();
        access_grants::register_scope(&conn, &scope, None, &caller.config()).unwrap();
        access_grants::register_worker(&conn, &scope).unwrap();
        access_grants::materialize(
            &conn,
            &launch,
            &serde_json::json!([{"path": grant.path, "mode": "read"}]),
            &[(grant.clone(), None)],
        )
        .unwrap();
    }

    let _scope = crate::access_policy::scope_guard(Some(scope));
    let _route = crate::access_policy::ask_route_guard(Some(AskRoute::Consent {
        once: Vec::new(),
        launch: Some(launch),
    }));
    let policy = caller.access_policy("external-search-policy").unwrap();
    assert_eq!(policy.grants, vec![grant.clone()]);
    assert_eq!(
        policy.decide(
            AccessOperation::Read,
            &crate::policy_path::policy_path(&external),
            false,
        ),
        crate::access_policy::AccessDecision::Allow
    );
    caller
        .validate_search_path(
            "external-search-validation",
            "worker-session",
            &external,
            crate::context::WorkerSearchTool::AftSearch,
        )
        .unwrap();
    let request = |command: &str, root: &Path| {
        let mut params = serde_json::json!({
            "path": root,
            "session_id": "worker-session",
        });
        if command == "semantic_search" {
            params["query"] = serde_json::json!("external_launch_marker");
            params["top_k"] = serde_json::json!(5);
        } else {
            params["pattern"] = serde_json::json!("external_launch_marker");
        }
        params["id"] = serde_json::json!(format!("external-search-{command}"));
        params["command"] = serde_json::json!(command);
        serde_json::from_value(params).unwrap()
    };

    let search = crate::commands::semantic_search::handle_semantic_search(
        &request("semantic_search", &external),
        &caller,
    );
    assert!(
        search.success,
        "approved external search failed: {search:?}"
    );
    assert!(
        search.data["text"]
            .as_str()
            .is_some_and(|text| text.contains("external_launch_marker")),
        "approved external search returned no marker: {search:?}"
    );

    let search = crate::commands::semantic_search::handle_semantic_search(
        &request("semantic_search", &unapproved),
        &caller,
    );
    let grep = crate::commands::grep::handle_grep(&request("grep", &unapproved), &caller);
    assert!(
        !search.success,
        "unapproved external search passed: {search:?}"
    );
    assert!(!grep.success, "unapproved external grep passed: {grep:?}");
    assert_eq!(
        search.data["code"], grep.data["code"],
        "search: {search:?}; grep: {grep:?}"
    );
    assert_ne!(search.data["code"], "grants_unavailable");
}

const PLANES: RequestedPlanes = RequestedPlanes {
    trigram: true,
    semantic: false,
    callgraph: true,
};

fn wait_stopped(session: &ParentSession) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !session.stopped() {
        assert!(
            Instant::now() < deadline,
            "the parent session never stopped"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn session_stops_when_its_only_context_binds_another_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    git_init(&root.join("child"));
    let storage = tempfile::tempdir().unwrap();
    let semantic = crate::config::SemanticBackendConfig::default();
    let first = context();
    let second = context();
    assert!(prepare(&root, PLANES));
    assert!(activate(&first, &root, storage.path(), &semantic));
    assert!(prepare(&root, PLANES));
    assert!(activate(&second, &root, storage.path(), &semantic));
    let session = session_for_root(&root).unwrap();
    // One holder leaves: the other still holds the session.
    release_context_except(&first, &root.join("child"));
    assert!(session.wait_rounds(1, Duration::from_secs(30)));
    std::thread::sleep(Duration::from_millis(600));
    assert!(!session.stopped());
    // The last holder binds another root: the session stops and leaves the
    // registry.
    release_context_except(&second, &root.join("child"));
    wait_stopped(&session);
    assert!(session_for_root(&root).is_none());
}

#[test]
fn session_stops_when_its_context_is_dropped() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    git_init(&root.join("child"));
    let storage = tempfile::tempdir().unwrap();
    let semantic = crate::config::SemanticBackendConfig::default();
    let ctx = context();
    assert!(prepare(&root, PLANES));
    assert!(activate(&ctx, &root, storage.path(), &semantic));
    let session = session_for_root(&root).unwrap();
    drop(ctx);
    wait_stopped(&session);
    assert!(session_for_root(&root).is_none());
}

#[test]
fn discovery_limits_stop_the_directory_iterator_before_sorting() {
    use std::cell::Cell;

    let visits = Cell::new(0);
    let entries = || {
        (0..10).map(|i| {
            visits.set(visits.get() + 1);
            if i == 0 {
                Err(())
            } else {
                Ok(i)
            }
        })
    };
    let mut remaining = 3;
    assert_eq!(
        read_discovery_entries(entries(), &mut remaining, || false),
        [1, 2]
    );
    assert_eq!(visits.get(), 3);
    assert_eq!(remaining, 0);

    visits.set(0);
    let mut remaining = 10;
    assert_eq!(
        read_discovery_entries(entries(), &mut remaining, || visits.get() >= 2),
        [1]
    );
    assert_eq!(visits.get(), 2);
    assert_eq!(remaining, 8);
}

#[test]
fn exhausted_discovery_discloses_uncertain_scope_not_known_children() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    // Only one child is read, independently of filesystem enumeration order.
    git_init(&root.join("one"));
    git_init(&root.join("two"));
    let found = discover_with_limit(&root, DEFAULT_MAX_CHILD_REPOS, 1).unwrap();
    assert!(found.entry_limit_reached);
    assert_eq!(found.children.len(), 1);
    let mut gaps = query::Gaps::default();
    gaps.discovery_scope(&found, &found.children[0].join("src"));
    assert!(gaps.into_values().is_empty());

    let absent = if found.children[0].ends_with("one") {
        "two"
    } else {
        "one"
    };
    let mut gaps = query::Gaps::default();
    gaps.discovery_scope(&found, &root.join(absent));
    let mut reply = serde_json::json!({"complete": true, "text": "No matches"});
    query::attach_search_gaps(&mut reply, gaps.into_values());
    assert_eq!(reply["complete"], false);
    assert_eq!(reply["gaps"][0]["kind"], "parent_discovery_entry_limit");
    assert_eq!(reply["gaps"][0]["path"], absent);
    assert_eq!(reply["text"], format!("(Parent folder: not covered by this answer:\n- {absent}: AFT stopped looking for repositories here after 20,000 entries, so some repositories in this folder were not searched.)\n\nNo matches"));
}

#[test]
fn configure_probe_is_bounded_by_wall_clock_time() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    for index in 0..20 {
        std::fs::create_dir_all(root.join(format!("plain-{index:02}"))).unwrap();
    }
    git_init(&root.join("zz-repo"));
    set_discovery_entry_delay(&root, Duration::from_millis(50));
    let started = Instant::now();
    // The only repository sorts last, past the budget: the probe gives up
    // and the root keeps its own index rather than stalling configure.
    assert!(!prepare_with_budget(
        &root,
        PLANES,
        Duration::from_millis(200)
    ));
    assert!(started.elapsed() < Duration::from_millis(600));
    set_discovery_entry_delay(&root, Duration::ZERO);
    assert!(prepare_with_budget(
        &root,
        PLANES,
        Duration::from_millis(200)
    ));
}
