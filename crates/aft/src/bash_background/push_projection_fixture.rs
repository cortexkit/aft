//! Shared populated context for push-projection regressions; no real LSP tools are required.
use super::*;
use crate::bash_background::persistence::{task_paths, write_task};
use crate::context::{App, AppContext};
use std::sync::{MutexGuard, OnceLock};

pub(crate) struct BusyRoot {
    pub ctx: Arc<AppContext>,
    pub app: Arc<App>,
    pub contexts: Vec<Arc<AppContext>>,
    pub root: PathBuf,
    pub storage: PathBuf,
    pub key: String,
    _dir: tempfile::TempDir,
    _view: crate::views::registry::ViewRegistration,
}

pub(crate) fn busy_root() -> MutexGuard<'static, BusyRoot> {
    static FIXTURE: OnceLock<Mutex<BusyRoot>> = OnceLock::new();
    FIXTURE
        .get_or_init(|| Mutex::new(build()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn build() -> BusyRoot {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    let root = dir.path().join("prefrontal");
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let app = App::default_shared();
    app.set_open_route_count(90);
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.storage_dir = Some(storage.clone());
    config.views.enabled = true;
    let ctx = Arc::new(AppContext::from_app(app.clone(), config));
    ctx.set_canonical_cache_root(root.clone());
    let key = ctx.memoized_artifact_cache_key(&root);
    ctx.bind_status_context();
    app.register_memory_context(root.clone(), &ctx);
    for i in 0..256 {
        let task_id = format!("bash-{i:016x}");
        let paths = task_paths(&storage, "session", &task_id).unwrap();
        std::fs::create_dir_all(&paths.dir).unwrap();
        std::fs::write(&paths.stdout, format!("test {i} ... ok\n").repeat(64)).unwrap();
        std::fs::write(&paths.stderr, "").unwrap();
        let mut task = PersistedTask::starting(
            task_id,
            "session".into(),
            format!("cargo test -p prefrontal-routing-module --lib -- case_{i}"),
            root.clone(),
            Some(root.clone()),
            Some(30_000),
            true,
            true,
        );
        task.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        write_task(&paths.json, &task).unwrap();
        ctx.bash_background()
            .insert_rehydrated_task(task, paths, true)
            .unwrap();
    }
    ctx.inspect_manager()
        .set_tier2_in_flight_for_test(crate::inspect::InspectCategory::DeadCode, true);
    let mut index = crate::search_index::SearchIndex::new();
    for i in 0..6200 {
        index.index_file(
            &root.join(format!("src/module_{}/file_{i}.rs", i % 64)),
            format!("pub fn symbol_{i}() {{}}\n").as_bytes(),
        );
    }
    index.ready = true;
    *ctx.search_index().write().unwrap() = Some(index);
    *ctx.semantic_index_status().write().unwrap() = crate::context::SemanticIndexStatus::Building {
        stage: "embedding".into(),
        files: Some(6200),
        entries_done: Some(40_000),
        entries_total: Some(52_000),
    };
    let family = crate::views::registry::FamilyRegistry::open(&storage, "push-fixture").unwrap();
    let view = family
        .register_view_with_storage(&storage, "prefrontal", &root)
        .unwrap();
    let mut paths = crate::path_status::PathStatusStore::open(view.view_dir()).unwrap();
    for i in 0..20 {
        paths
            .mark_pending(format!("src/pending_{i}.rs").as_bytes(), "fixture", 1)
            .unwrap();
    }
    paths.mark_failed(b"src/failed.rs", "fixture", 1).unwrap();
    ctx.install_view_runtime(
        crate::context::ViewRuntimeSnapshot {
            query_pin: None,
            storage: storage.clone(),
            family: "push-fixture".into(),
            scope: "prefrontal".into(),
            view_dir: view.view_dir().to_path_buf(),
            generation: Some("1-fixture".into()),
            manifest: None,
            head_fingerprint: String::new(),
            head_metadata: crate::alias::GitHeadMetadata {
                head_path: PathBuf::new(),
                head_mtime: None,
                resolved_ref_path: None,
                resolved_ref_mtime: None,
            },
            pending_paths: Default::default(),
            pending_inputs: Default::default(),
        },
        None,
    );
    for kind in [
        crate::lsp::registry::ServerKind::TypeScript,
        crate::lsp::registry::ServerKind::Rust,
        crate::lsp::registry::ServerKind::Python,
        crate::lsp::registry::ServerKind::Go,
        crate::lsp::registry::ServerKind::Bash,
    ] {
        let mut manager = ctx.lsp();
        let client = crate::lsp::client::LspClient::spawn_with_reclaim_root(
            kind,
            root.clone(),
            Path::new("/bin/cat"),
            &[],
            &HashMap::new(),
            manager.event_sender_for_test(),
            app.lsp_child_registry(),
            None,
            false,
        )
        .unwrap();
        manager.insert_client_for_test(client);
    }
    let mut contexts = vec![ctx.clone()];
    for i in 1..192 {
        let path = dir.path().join(format!(".local/share/cortexkit/alfonso/worktrees/c0c39eea197fcc68/9e09c29b1299fe1375199f0556d08a60e3160822e6787a583c0054270883aec9/bg_{i:016x}"));
        let mut config = crate::config::Config::default();
        config.project_root = Some(path.clone());
        config.storage_dir = Some(storage.clone());
        let other = Arc::new(AppContext::from_app(app.clone(), config));
        app.register_memory_context(path, &other);
        contexts.push(other);
    }
    app.publish_lifecycle_census(crate::lifecycle_census::collect(&app, &contexts));
    assert_eq!(ctx.bash_background().inner.tasks.lock().unwrap().len(), 256);
    assert_eq!(ctx.lsp_server_count(), 5);
    BusyRoot {
        ctx,
        app,
        contexts,
        root,
        storage,
        key,
        _dir: dir,
        _view: view,
    }
}
