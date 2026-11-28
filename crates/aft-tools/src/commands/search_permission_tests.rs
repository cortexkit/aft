use std::path::Path;

fn context() -> crate::context::AppContext {
    crate::tools_test_support::test_context(
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
        std::fs::create_dir_all(root.join(".git")).unwrap();
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
    aft_engine::test_bridge::display_data::surfaces::serve_text_index_for_test(&indexed);

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
        path: aft_base::policy_path::policy_path(&external),
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
            &aft_base::policy_path::policy_path(&external),
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
