//! Unit tests for parent folder discovery and the session registry.

use super::*;

fn isolated<R>(root: &Path, body: impl FnOnce() -> R) -> R {
    crate::git_probe::with_child_env_for_test(
        &[(
            "GIT_CEILING_DIRECTORIES",
            root.parent().unwrap().as_os_str(),
        )],
        body,
    )
}

fn discover(root: &Path, cap: usize) -> Option<Discovery> {
    isolated(root, || super::discover(root, cap))
}
fn prepare(root: &Path, planes: RequestedPlanes) -> bool {
    isolated(root, || super::prepare(root, planes))
}
fn prepare_with_budget(root: &Path, planes: RequestedPlanes, budget: Duration) -> bool {
    isolated(root, || super::prepare_with_budget(root, planes, budget))
}

#[cfg(unix)]
#[test]
fn discovery_skips_repositories_on_different_devices() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    git_init(&root.join("local"));
    git_init(&root.join("mounted").join("external"));
    crate::watcher_filter::EXCLUSION_TEST_DEVICE
        .with(|device| *device.borrow_mut() = Some(root.join("mounted")));
    let found = discover(&root, DEFAULT_MAX_CHILD_REPOS).unwrap();
    crate::watcher_filter::EXCLUSION_TEST_DEVICE.with(|device| *device.borrow_mut() = None);
    assert_eq!(found.children, vec![root.join("local")]);
    assert!(found.outside.contains(&root.join("mounted")));
}

fn git_init(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let mut command = std::process::Command::new("git");
    crate::test_env::apply_hermetic_git_env(command.current_dir(dir));
    assert!(command.args(["init", "-q"]).status().unwrap().success());
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
fn admitted_child_aliases_are_not_reported_as_outside_projects() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    for name in ["a", "b"] {
        git_init(&root.join(name));
    }
    let mut admission = isolated(&root, || {
        crate::folder_admission::probe(
            &root,
            &Default::default(),
            crate::folder_admission::ProbeBudgets {
                time: Duration::from_secs(5),
                ..Default::default()
            },
        )
    });
    assert_eq!(
        admission.classification,
        crate::folder_admission::FolderClassification::Workspace
    );
    admission.coverage.workspace_complete = false;
    admission.immediate_directories = vec![root.join("a/../a"), root.join("b/../b")];

    let found = discovery_from_admission(&root, &admission, DEFAULT_MAX_CHILD_REPOS);
    assert_eq!(found.children, vec![root.join("a"), root.join("b")]);
    assert!(found.skipped.is_empty());
    assert!(found.outside.is_empty(), "{:?}", found.outside);
}

#[test]
fn discovery_refuses_checkouts_and_folders_without_repositories() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    assert!(discover(&root, DEFAULT_MAX_CHILD_REPOS).is_none());
    git_init(&root.join("child"));
    assert!(discover(&root, DEFAULT_MAX_CHILD_REPOS).is_none());
    git_init(&root.join("second"));
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

const PLANES: RequestedPlanes = RequestedPlanes {
    trigram: true,
    semantic: false,
    callgraph: true,
};

#[test]
fn cold_parent_search_scans_unpublished_children_and_respects_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    for name in ["alpha", "beta"] {
        let child = root.join(name);
        git_init(&child);
        std::fs::create_dir_all(child.join("src")).unwrap();
        std::fs::write(
            child.join("src/lib.rs"),
            "pub fn explicit_conversion_requires_confirmation() {}\n",
        )
        .unwrap();
    }
    let storage = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.views.enabled = true;
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    assert!(prepare(&root, PLANES));
    assert!(activate(
        &ctx,
        &root,
        storage.path(),
        &ctx.config().semantic
    ));
    let session = session_for_root(&root).unwrap();
    assert!(session.wait_rounds(1, Duration::from_secs(30)));
    assert!(session
        .children()
        .iter()
        .all(|child| !child.trigram_ready()));
    for (path, expected) in [(None, 2), (Some(root.join("alpha")), 1)] {
        let mut value = serde_json::json!({
            "id": "cold-parent-search", "command": "semantic_search",
            "query": "explicit_conversion_requires_confirmation",
        });
        if let Some(path) = path {
            value["path"] = serde_json::json!(path);
        }
        let req = serde_json::from_value(value).unwrap();
        let answer = super::engine::search(&req, &ctx, &session);
        assert!(answer.success, "{answer:?}");
        let rows = answer.data["results"].as_array().unwrap();
        assert_eq!(rows.len(), expected, "{answer:?}");
        assert!(rows.iter().any(|row| row["file"] == "alpha/src/lib.rs"));
        assert_eq!(answer.data["complete"], false, "{answer:?}");
        assert_eq!(answer.data["lexical_only_fallback"], true, "{answer:?}");
        assert!(answer.data["text"]
            .as_str()
            .unwrap()
            .contains("explicit_conversion"));
        assert!(answer.data["text"]
            .as_str()
            .unwrap()
            .contains("bounded disk scan"));
    }
    assert_eq!(std::fs::read_dir(storage.path()).unwrap().count(), 0);
    deactivate(&root);
    assert!(prepare(
        &root,
        RequestedPlanes {
            trigram: false,
            semantic: false,
            callgraph: false
        }
    ));
    assert!(activate(
        &ctx,
        &root,
        storage.path(),
        &ctx.config().semantic
    ));
    let session = session_for_root(&root).unwrap();
    assert!(session.wait_rounds(1, Duration::from_secs(30)));
    let req = serde_json::from_value(serde_json::json!({
        "id": "parent-search-disabled", "command": "semantic_search", "query": "explicit_conversion_requires_confirmation",
    })).unwrap();
    let answer = super::engine::search(&req, &ctx, &session);
    assert_eq!(answer.data["status"], "unavailable", "{answer:?}");
    assert!(
        !answer.data["text"].as_str().unwrap().contains("No results"),
        "{answer:?}"
    );
    deactivate(&root);
}

#[test]
fn activate_requires_prepare_and_deactivate_stops_the_session() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    git_init(&root.join("child"));
    git_init(&root.join("second"));
    let storage = tempfile::tempdir().unwrap();
    let semantic = crate::config::SemanticBackendConfig::default();
    let ctx = context();
    assert!(!activate(&ctx, &root, storage.path(), &semantic));
    assert!(session_for_root(&root).is_none());
    assert!(prepare(&root, PLANES));
    assert!(activate(&ctx, &root, storage.path(), &semantic));
    let session = session_for_root(&root).unwrap();
    assert!(session.wait_rounds(1, Duration::from_secs(30)));
    // The child has published nothing: each plane is a named gap, and the
    // worker created nothing in the shared storage.
    let child = &session.children()[0];
    assert!(matches!(&*read(&child.trigram), Plane::Gap(_)));
    assert!(matches!(&*read(&child.callgraph), Plane::Gap(_)));
    assert!(matches!(&*read(&child.semantic), Plane::Gap(_)));
    assert_eq!(std::fs::read_dir(storage.path()).unwrap().count(), 0);
    // Rebinding with the same planes reuses the running session.
    assert!(prepare(&root, PLANES));
    assert!(activate(&ctx, &root, storage.path(), &semantic));
    assert!(Arc::ptr_eq(&session, &session_for_root(&root).unwrap()));
    deactivate(&root);
    assert!(session_for_root(&root).is_none());
    assert!(session.stopped());
}

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
    git_init(&root.join("second"));
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
    git_init(&root.join("second"));
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
fn configure_probe_is_bounded_by_wall_clock_time() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    git_init(&root.join("one"));
    git_init(&root.join("two"));
    let started = Instant::now();
    // An injected zero deadline rejects evidence without relying on disk speed.
    assert!(!prepare_with_budget(&root, PLANES, Duration::ZERO));
    assert!(started.elapsed() < Duration::from_millis(600));
    assert!(prepare_with_budget(
        &root,
        PLANES,
        Duration::from_millis(200)
    ));
}

#[test]
fn inspect_merge_sums_counts_and_prefixes_item_paths() {
    let mut merged = serde_json::Map::new();
    let first = serde_json::json!({"count": 2, "items": [{"file": "src/a.rs"}], "partial": false});
    let second = serde_json::json!({"count": 3, "items": [{"file": "lib/b.rs"}], "partial": true});
    super::inspect::merge(&mut merged, first.as_object().unwrap(), Path::new("alpha"));
    super::inspect::merge(&mut merged, second.as_object().unwrap(), Path::new("beta"));
    assert_eq!(
        serde_json::Value::Object(merged),
        serde_json::json!({
            "count": 5,
            "items": [{"file": "alpha/src/a.rs"}, {"file": "beta/lib/b.rs"}],
            "partial": true,
        })
    );
}
