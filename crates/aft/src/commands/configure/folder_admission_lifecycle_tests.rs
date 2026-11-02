use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Config;
use crate::context::AppContext;
use crate::folder_admission_runtime as admission;
use crate::parser::TreeSitterProvider;
use crate::protocol::RawRequest;

fn context(storage: &Path) -> Arc<AppContext> {
    Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(storage.to_path_buf()),
            ..Config::default()
        },
    ))
}

fn request(command: &str, params: Value) -> RawRequest {
    let mut value = params;
    value["id"] = json!(format!("folder-{command}"));
    value["command"] = json!(command);
    serde_json::from_value(value).unwrap()
}

fn configure(
    root: &Path,
    storage: &Path,
    ctx: &AppContext,
    indexes: bool,
    refresh: bool,
) -> crate::protocol::Response {
    super::handle_configure(
        &request(
            "configure",
            json!({
                "project_root": root, "storage_dir": storage, "harness": "opencode", "refresh": refresh,
                "config": [{"tier": "user", "source": "folder fixture", "doc": json!({
                    "indexes": {"trigram": indexes, "semantic": indexes, "callgraph": indexes}, "views": {"enabled": false}
                }).to_string()}]
            }),
        ),
        ctx,
    )
}

fn settle(ctx: &Arc<AppContext>) {
    let mut maintenance = crate::runtime_drain::StandaloneConfigureMaintenance::default();
    for _ in 0..500 {
        if !maintenance.has_pending(ctx) {
            return;
        }
        maintenance.drain_one(ctx);
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("folder configure maintenance did not settle");
}

#[test]
fn folder_admission_regression_timeout_remains_pending_and_rechecks() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    crate::test_project::mark_project(root.path()).unwrap();
    isolated(root.path(), || {
        let ctx = context(storage.path());
        let (started, release) = admission::gate_probe_for_test(&ctx);
        assert!(configure(root.path(), storage.path(), &ctx, false, false).success);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !admission::poll_wait(&ctx) {
            assert!(
                std::time::Instant::now() < deadline,
                "admission wait did not end"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        let folder = admission::snapshot(&ctx).unwrap();
        assert_eq!(folder["state"], "pending", "{folder}");
        assert_eq!(folder["classification"], Value::Null, "{folder}");
        assert_eq!(folder["reason"], "evidence_budget_exhausted", "{folder}");
        assert_eq!(
            folder["coverage"]["root_listing_complete"], false,
            "{folder}"
        );
        assert_eq!(folder["observed"]["entries"], 0, "{folder}");
        for _ in 0..2 {
            let refused = admission::refusal(&ctx, "incomplete", "fixture").unwrap();
            assert_eq!(refused.data["message"], admission::INCOMPLETE_TEXT);
        }
        assert!(std::fs::read_dir(storage.path().join("folder-admission"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true));
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut maintenance = crate::runtime_drain::StandaloneConfigureMaintenance::default();
        loop {
            if maintenance.has_pending(&ctx) {
                assert!(
                    ctx.configure_tail_has_work(),
                    "the daemon must also see the due retry"
                );
                maintenance.drain_one(&ctx);
            }
            let folder = admission::snapshot(&ctx).unwrap();
            if folder["state"] == "decided" && folder["classification"] == "project" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "incomplete admission never rechecked: {folder}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    });
}

fn isolated<R>(root: &Path, body: impl FnOnce() -> R) -> R {
    crate::git_probe::with_child_env_for_test(
        &[(
            "GIT_CEILING_DIRECTORIES",
            root.parent().unwrap().as_os_str(),
        )],
        body,
    )
}

fn scratch(root: &Path) {
    for shard in 0..5 {
        for run in 0..20 {
            let leaf = root.join(format!("shard-{shard}/run-{run}"));
            std::fs::create_dir_all(&leaf).unwrap();
            std::fs::write(leaf.join("seed.dat"), []).unwrap();
            for name in 1..500 {
                std::fs::hard_link(leaf.join("seed.dat"), leaf.join(format!("{name}.dat")))
                    .unwrap();
            }
        }
    }
}

#[test]
fn folder_admission_off_notice_is_pinned_byte_for_byte() {
    assert_eq!(admission::OFF_TEXT, "Indexing is off: this folder does not look like a project. grep and glob still work, with limits. To index it, set project_detection in your AFT user config.");
}

#[test]
fn folder_admission_incomplete_notice_is_pinned_byte_for_byte() {
    // This test pins the notice byte for byte (approved by Ufuk).
    assert_eq!(admission::INCOMPLETE_TEXT, "Indexing is paused: AFT could not check this folder in time and will check again shortly. grep and glob still work, with limits.");
}

#[test]
fn folder_admission_definite_refusal_announces_once_per_session() {
    // A definite refusal uses the full notice once per session, then its code.
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(storage.path());
    assert!(super::handle_configure(&request("configure", json!({
        "project_root": root.path(), "storage_dir": storage.path(), "harness": "opencode",
        "config": [{"tier": "user", "source": "off fixture", "doc": json!({
            "project_detection": {"mode": "off"},
            "indexes": {"trigram": false, "semantic": false, "callgraph": false}, "views": {"enabled": false}
        }).to_string()}]
    })), &ctx).success);
    settle(&ctx);
    let first = admission::refusal(&ctx, "first", "same-session").unwrap();
    assert_eq!(
        crate::subc_format::format_response("search", &first, false),
        admission::OFF_TEXT
    );
    let later = admission::refusal(&ctx, "later", "same-session").unwrap();
    assert_eq!(
        crate::subc_format::format_response("search", &later, false),
        "folder_not_admitted: user_override"
    );
}

#[test]
fn folder_admission_project_override_skips_a_held_probe_worker() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(storage.path());
    let (started, _release) = admission::gate_probe_for_test(&ctx);
    let ack = super::handle_configure(
        &request(
            "configure",
            json!({
                "project_root": root.path(), "storage_dir": storage.path(), "harness": "opencode",
                "config": [{"tier": "user", "source": "declared project", "doc": json!({
                    "project_detection": {"mode": "project"},
                    "indexes": {"trigram": false, "semantic": false, "callgraph": false}, "views": {"enabled": false}
                }).to_string()}]
            }),
        ),
        &ctx,
    );
    assert!(ack.success, "{}", ack.data);
    settle(&ctx);
    assert_eq!(
        admission::snapshot(&ctx).unwrap()["classification"],
        "project"
    );
    assert_eq!(admission::probe_counts_for_test(&ctx), (0, 0));
    assert!(started.try_recv().is_err());
}

#[test]
fn stale_configure_maintenance_preserves_a_replayed_session_binding() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(storage.path());
    assert!(ctx.note_configure_session_binding(root.path().to_path_buf(), "session".into()));
    let earlier = ctx.configure_generation();
    ctx.configure_generation_flag()
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    assert!(!ctx.note_configure_session_binding(root.path().to_path_buf(), "session".into()));
    ctx.forget_configure_session_binding_for_generation(root.path(), "session", earlier);
    assert!(ctx.has_configure_session_binding(root.path(), "session"));
    ctx.forget_configure_session_binding_for_generation(
        root.path(),
        "session",
        ctx.configure_generation(),
    );
    assert!(!ctx.has_configure_session_binding(root.path(), "session"));
}

#[test]
fn folder_admission_replay_does_not_force_a_callgraph_generation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let storage = temp.path().join("storage");
    std::fs::create_dir_all(&root).unwrap();
    crate::test_project::mark_project(&root).unwrap();
    isolated(&root, || {
        let ctx = context(&storage);
        let req = request(
            "configure",
            json!({
                "project_root": root, "storage_dir": storage, "harness": "opencode",
                "config": [{"tier": "user", "source": "folder fixture", "doc": json!({
                    "indexes": {"trigram": false, "semantic": false, "callgraph": true},
                    "views": {"enabled": false}
                }).to_string()}]
            }),
        );
        assert!(super::handle_configure(&req, &ctx).success);
        assert!(!ctx.config().indexes.callgraph);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let replay = loop {
            if let Some(replay) = crate::provisional_identity_retry::poll(&ctx) {
                break replay;
            }
            assert!(std::time::Instant::now() < deadline, "probe did not finish");
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(replay.run(&ctx, |_| {}).success);
        assert!(ctx.config().indexes.callgraph);
        assert_eq!(ctx.pending_callgraph_store_force_token(), None);
    });
}

#[test]
fn folder_admission_timeout_is_incomplete_in_health_and_can_reconnect() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("Makefile"), "").unwrap();
    let ctx = context(storage.path());
    let (started, release) = admission::gate_probe_for_test(&ctx);
    let ack = configure(root.path(), storage.path(), &ctx, false, false);
    assert!(ack.success);
    assert_eq!(admission::probe_counts_for_test(&ctx), (0, 0));
    assert!(!admission::poll_wait(&ctx));
    started.recv_timeout(Duration::from_secs(1)).unwrap();
    while !admission::poll_wait(&ctx) {
        std::thread::sleep(Duration::from_millis(2));
    }
    let health = serde_json::to_value(ctx.try_health_snapshot(root.path())).unwrap();
    assert_eq!(health["folder"]["classification"], Value::Null);
    assert_eq!(health["folder"]["state"], "pending");
    assert_eq!(health["folder"]["reason"], "evidence_budget_exhausted");
    assert_eq!(health["folder"]["coverage"]["complete"], false);
    assert_eq!(health["folder"]["coverage"]["count_kind"], "lower_bound");
    let refusal = admission::refusal(&ctx, "timed-out", "session").unwrap();
    assert_eq!(refusal.data["message"], admission::INCOMPLETE_TEXT);
    release.send(()).unwrap();
    assert!(configure(root.path(), storage.path(), &ctx, false, false).success);
    settle(&ctx);
    assert_eq!(
        admission::snapshot(&ctx).unwrap()["classification"],
        "project"
    );
}

#[test]
fn folder_admission_idle_bind_does_not_consume_the_probe_wait() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let storage = temp.path().join("storage");
    std::fs::create_dir_all(&root).unwrap();
    crate::test_project::mark_project(&root).unwrap();
    isolated(&root, || {
        let ctx = context(&storage);
        assert!(configure(&root, &storage, &ctx, false, false).success);
        std::thread::sleep(admission::wait_budget() + Duration::from_millis(20));
        assert_eq!(admission::probe_counts_for_test(&ctx), (0, 0));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !admission::poll_wait(&ctx) {
            crate::provisional_identity_retry::drain_inline(&ctx);
            assert!(std::time::Instant::now() < deadline, "probe did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["classification"],
            "project"
        );
    });
}

#[test]
fn folder_admission_pending_bind_cannot_acquire_a_shared_writer() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let storage = temp.path().join("storage");
    std::fs::create_dir_all(&root).unwrap();
    crate::test_project::mark_project(&root).unwrap();
    isolated(&root, || {
        let ctx = context(&storage);
        assert!(configure(&root, &storage, &ctx, false, false).success);
        assert!(admission::pending(&ctx));
        let key = "shared-before-admission";
        let lease = crate::root_cache::WriterLease::acquire_shared_with_timeout(
            crate::root_cache::RootCacheDomain::Callgraph,
            &storage.join("callgraph").join(key),
            key,
            &root,
            Duration::ZERO,
        )
        .unwrap();
        assert!(
            lease.is_none(),
            "a pending root acquired shared write authority"
        );
    });
}

#[test]
fn fresh_connect_rechecks_changed_evidence_without_cache_cooldown() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(storage.path());
    isolated(root.path(), || {
        assert!(configure(root.path(), storage.path(), &ctx, false, false).success);
        crate::test_admission::settle(&ctx);
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["classification"],
            "not_project"
        );
        std::fs::write(root.path().join("Makefile"), "").unwrap();
        assert!(configure(root.path(), storage.path(), &ctx, false, false).success);
        crate::test_admission::settle(&ctx);
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["classification"],
            "project"
        );
    });
}

#[test]
fn folder_admission_configure_and_restart_never_load_old_indexes() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    isolated(root.path(), || {
        std::fs::write(root.path().join("sample.rs"), "pub fn off_root_file() {}\n").unwrap();
        let key = crate::search_index::artifact_cache_key(root.path());
        let cache = crate::search_index::resolve_cache_dir_with_key(&key, Some(storage.path()));
        let mut old = crate::search_index::SearchIndex::build(root.path());
        assert!(old.write_to_disk(&cache, None));
        let artifact = cache.join("cache.bin");
        let before = std::fs::metadata(&artifact).unwrap().modified().unwrap();
        scratch(root.path());

        let ctx = context(storage.path());
        let ack = configure(root.path(), storage.path(), &ctx, true, false);
        assert!(ack.success, "{}", ack.data);
        assert_eq!(ack.data["folder"]["state"], "pending");
        assert_eq!(admission::probe_counts_for_test(&ctx), (0, 0));
        assert_eq!(
            super::configure_artifact_load_attempts_for_root_for_test(root.path()),
            0
        );
        assert!(!ctx.watcher_runtime_active());
        settle(&ctx);
        let folder = admission::snapshot(&ctx).unwrap();
        assert_eq!(folder["classification"], Value::Null);
        assert_eq!(folder["reason"], "evidence_budget_exhausted");
        assert_eq!(folder["state"], "pending");
        assert_eq!(folder["coverage"]["complete"], false);
        assert!(folder["observed"]["entries"].as_u64().unwrap() <= 4096);
        let incomplete_message = if folder["coverage"]["stop"] == "time" {
            admission::INCOMPLETE_TEXT
        } else {
            "folder_not_admitted: evidence_budget_exhausted"
        };
        assert_eq!(folder["message"], incomplete_message);
        assert_eq!(admission::probe_counts_for_test(&ctx), (1, 0));
        assert!(ctx.search_index().read().unwrap().is_none());
        assert!(ctx.semantic_index().read().unwrap().is_none());
        assert!(ctx.callgraph_store().read().unwrap().is_none());
        assert!(!ctx.heavy_root_work_allowed());
        assert!(!ctx.watcher_runtime_active());
        let health = serde_json::to_value(ctx.try_health_snapshot(root.path())).unwrap();
        assert_eq!(health["folder"]["classification"], Value::Null);
        assert_eq!(health["folder"]["message"], incomplete_message);
        assert_eq!(health["search_index"]["status"], "disabled");
        assert_eq!(health["semantic_index"]["status"], "disabled");

        let outline = crate::commands::outline::handle_outline(
            &request("outline", json!({"file": root.path().join("sample.rs")})),
            &ctx,
        );
        assert!(outline.success, "{}", outline.data);
        let search = crate::commands::semantic_search::handle_semantic_search(
            &request("semantic_search", json!({"query": "off_root_file"})),
            &ctx,
        );
        assert!(!search.success);
        assert_eq!(search.data["code"], "folder_not_admitted");
        assert_eq!(search.data["message"], incomplete_message);
        let again = crate::commands::semantic_search::handle_semantic_search(
            &request("semantic_search", json!({"query": "off_root_file"})),
            &ctx,
        );
        assert!(!again.data["message"]
            .as_str()
            .unwrap()
            .contains(admission::OFF_TEXT));
        assert_eq!(
            super::configure_artifact_load_attempts_for_root_for_test(root.path()),
            0
        );

        let restarted = context(storage.path());
        assert!(configure(root.path(), storage.path(), &restarted, true, false).success);
        settle(&restarted);
        assert_eq!(admission::probe_counts_for_test(&restarted), (1, 0));
        assert_eq!(
            admission::snapshot(&restarted).unwrap()["classification"],
            Value::Null
        );
        assert!(restarted.search_index().read().unwrap().is_none());
        assert!(!restarted.watcher_runtime_active());
        assert_eq!(
            super::configure_artifact_load_attempts_for_root_for_test(root.path()),
            0
        );
        assert_eq!(
            std::fs::metadata(&artifact).unwrap().modified().unwrap(),
            before
        );
    });
}

#[test]
fn folder_admission_grep_glob_count_rejected_names_before_filtering() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    isolated(root.path(), || {
        std::fs::write(root.path().join(".gitignore"), "*.dat\n").unwrap();
        for name in 0..5000 {
            std::fs::write(root.path().join(format!("{name}.dat")), []).unwrap();
        }
        let ctx = context(storage.path());
        assert!(configure(root.path(), storage.path(), &ctx, true, false).success);
        settle(&ctx);
        let folder = admission::snapshot(&ctx).unwrap();
        assert_eq!(folder["classification"], Value::Null, "{folder}");
        assert_eq!(folder["state"], "pending", "{folder}");
        let expected_notice = if folder["coverage"]["stop"] == "time" {
            admission::INCOMPLETE_TEXT
        } else {
            "folder_not_admitted: evidence_budget_exhausted"
        };
        crate::grep_executor::with_walk_limit(2, || {
            let grep = crate::commands::grep::handle_grep(
                &request("grep", json!({"pattern": "absent", "include": "*.never"})),
                &ctx,
            );
            assert!(grep.success, "{}", grep.data);
            assert_eq!(grep.data["complete"], false);
            assert_eq!(grep.data["query_walk"]["stop"], "entry_budget");
            assert_eq!(grep.data["query_walk"]["entries_visited"], 1024);
            assert_eq!(grep.data["query_walk"]["entry_budget"], 1024);
            assert!(grep.data["text"]
                .as_str()
                .unwrap()
                .contains(expected_notice));
            let glob = crate::commands::glob::handle_glob(
                &request("glob", json!({"pattern": "*.never"})),
                &ctx,
            );
            assert!(glob.success, "{}", glob.data);
            assert_eq!(glob.data["complete"], false);
            assert_eq!(glob.data["query_walk"][0]["stop"], "entry_budget");
            assert_eq!(glob.data["query_walk"][0]["entries_visited"], 1024);
            assert_eq!(glob.data["total"], Value::Null);
            assert!(glob.data.get("total_lower_bound").is_some());
            assert!(!glob.data["text"]
                .as_str()
                .unwrap()
                .contains(expected_notice));
        });
        assert_eq!(
            super::configure_artifact_load_attempts_for_root_for_test(root.path()),
            0
        );
    });
}

#[test]
fn folder_admission_grep_has_an_aggregate_content_byte_bound() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    isolated(root.path(), || {
        let mut content = vec![b'x'; crate::bounded_folder_walk::BYTE_BUDGET + 1024];
        content.extend_from_slice(b"needle_after_budget\n");
        std::fs::write(root.path().join("large.log"), content).unwrap();
        let ctx = context(storage.path());
        assert!(configure(root.path(), storage.path(), &ctx, true, false).success);
        settle(&ctx);
        let grep = crate::commands::grep::handle_grep(
            &request("grep", json!({"pattern": "needle_after_budget"})),
            &ctx,
        );
        assert!(grep.success, "{}", grep.data);
        assert_eq!(grep.data["complete"], false);
        assert_eq!(grep.data["total_matches"], 0);
        assert_eq!(grep.data["query_walk"]["stop"], "byte_budget");
        assert!(
            grep.data["query_walk"]["bytes_read"].as_u64().unwrap()
                <= crate::bounded_folder_walk::BYTE_BUDGET as u64
        );
    });
}

#[test]
fn folder_admission_refresh_and_live_policy_changes_recheck_the_root() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    isolated(root.path(), || {
        let ctx = context(storage.path());
        assert!(configure(root.path(), storage.path(), &ctx, false, false).success);
        settle(&ctx);
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["reason"],
            "no_project_evidence"
        );
        std::fs::write(root.path().join("one.rs"), "pub fn one() {}\n").unwrap();
        std::fs::write(root.path().join("two.rs"), "pub fn two() {}\n").unwrap();
        assert!(configure(root.path(), storage.path(), &ctx, false, true).success);
        settle(&ctx);
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["classification"],
            "project"
        );
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["reason"],
            "small_source_tree"
        );
        assert!(ctx.heavy_root_work_allowed());
        let generation = ctx.configure_generation();
        ctx.update_config(|config| {
            config.project_detection.mode = crate::folder_admission::DetectionMode::Off
        });
        admission::policy_changed(&ctx, &ctx.config());
        assert!(ctx.configure_generation() > generation);
        assert!(!ctx.heavy_root_work_allowed());
        settle(&ctx);
        let folder = admission::snapshot(&ctx).unwrap();
        assert_eq!(folder["state"], "decided");
        assert_eq!(folder["classification"], "not_project");
        assert_eq!(folder["reason"], "user_override");
        assert_eq!(folder["override_mode"], "off");
        assert!(!ctx.watcher_runtime_active());
    });
}

#[test]
fn folder_admission_personal_shape_wins_over_a_root_marker() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    isolated(root.path(), || {
        std::fs::write(root.path().join("Cargo.toml"), []).unwrap();
        for name in ["Desktop", "Downloads", "Documents"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        let ctx = context(storage.path());
        assert!(configure(root.path(), storage.path(), &ctx, true, false).success);
        settle(&ctx);
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["classification"],
            "not_project"
        );
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["reason"],
            "mixed_personal_tree"
        );
        assert_eq!(
            super::configure_artifact_load_attempts_for_root_for_test(root.path()),
            0
        );
    });
}

#[test]
fn folder_admission_does_not_reject_the_home_path_as_such() {
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            unsafe {
                match &self.0 {
                    Some(home) => std::env::set_var("HOME", home),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }
    let _environment = crate::test_env::process_env_lock();
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let _restore = RestoreHome(std::env::var_os("HOME"));
    unsafe {
        std::env::set_var("HOME", root.path());
    }
    isolated(root.path(), || {
        std::fs::write(root.path().join("Cargo.toml"), []).unwrap();
        let ctx = context(storage.path());
        assert!(configure(root.path(), storage.path(), &ctx, false, false).success);
        settle(&ctx);
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["classification"],
            "project"
        );
        assert!(ctx.heavy_root_work_allowed());
        assert!(!ctx
            .degraded_reasons()
            .iter()
            .any(|reason| reason == "home_root"));
    });
}

#[test]
fn folder_admission_discards_a_stale_probe_generation() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(storage.path());
    assert!(configure(root.path(), storage.path(), &ctx, true, false).success);
    let (started, release) = admission::gate_probe_for_test(&ctx);
    let worker_ctx = Arc::clone(&ctx);
    let worker_root = root.path().to_path_buf();
    let worker =
        std::thread::spawn(move || isolated(&worker_root, || admission::resolve(&worker_ctx)));
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        worker.join().unwrap(),
        admission::ResolveResult::Wait
    ));
    ctx.configure_generation_flag()
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    release.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(admission::resolve(&ctx), admission::ResolveResult::Stale) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stale probe did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(admission::snapshot(&ctx).unwrap()["state"], "pending");
    assert_eq!(
        admission::snapshot(&ctx).unwrap()["classification"],
        Value::Null
    );
    assert_eq!(
        super::configure_artifact_load_attempts_for_root_for_test(root.path()),
        0
    );
    assert!(!storage.path().join("folder-admission").exists());
}

fn git(root: &Path, args: &[&str]) {
    let mut command = std::process::Command::new("git");
    crate::test_env::apply_hermetic_git_env(command.current_dir(root));
    let output = command.args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn folder_admission_workspace_views_serve_children_without_a_parent_index() {
    workspace_views_with_ready_children(false);
}

#[test]
fn folder_admission_workspace_views_retry_a_busy_child_identity() {
    workspace_views_with_ready_children(true);
}

fn wait_for_child_index(ctx: &Arc<AppContext>, root: &Path, storage: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        crate::runtime_drain::drain_deferred_configure_maintenance(ctx);
        crate::runtime_drain::drain_search_index_events(ctx);
        let persisted = ctx.cached_artifact_cache_key(root).is_some_and(|key| {
            crate::search_index::resolve_cache_dir_with_key(&key, Some(storage))
                .join("cache.bin")
                .is_file()
        });
        if persisted
            && ctx
                .search_index()
                .read()
                .unwrap()
                .as_ref()
                .is_some_and(|index| index.ready)
        {
            assert!(crate::root_identity::read(root, storage).unwrap().is_some());
            return;
        }
        assert!(std::time::Instant::now() < deadline,
            "child never settled and published its index: folder={:?}, identity={:?}, degraded={:?}",
            admission::snapshot(ctx), ctx.cached_artifact_cache_key(root), ctx.degraded_reasons());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn workspace_views_with_ready_children(busy_child: bool) {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    isolated(root.path(), || {
        let mut children = Vec::new();
        for (name, symbol) in [
            ("alpha", "workspace_alpha_marker"),
            ("beta", "workspace_beta_marker"),
        ] {
            let child = root.path().join(name);
            std::fs::create_dir(&child).unwrap();
            git(&child, &["init", "-q"]);
            std::fs::write(child.join("lib.rs"), format!("pub fn {symbol}() {{}}\n")).unwrap();
            git(&child, &["add", "."]);
            git(
                &child,
                &[
                    "-c",
                    "user.name=Folder",
                    "-c",
                    "user.email=folder@example.invalid",
                    "commit",
                    "-qm",
                    "fixture",
                ],
            );
            let lock = (busy_child && name == "alpha").then(|| {
                // A manually written cache does not establish a settled identity.
                let key = crate::search_index::artifact_cache_key(&child);
                let cache =
                    crate::search_index::resolve_cache_dir_with_key(&key, Some(storage.path()));
                let mut index = crate::search_index::SearchIndex::build(&child);
                assert!(index.write_to_disk(
                    &cache,
                    crate::search_index::current_git_head(&child).as_deref()
                ));
                assert!(crate::root_identity::read(&child, storage.path())
                    .unwrap()
                    .is_none());
                let lock = rusqlite::Connection::open(storage.path().join("aft.db")).unwrap();
                lock.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
                    .unwrap();
                let error =
                    crate::search_index::artifact_cache_key_read_only(&child, storage.path())
                        .expect_err(
                            "an unbound child must not guess an identity while pins are unreadable",
                        );
                assert!(
                    error.to_string().contains("aft.db was busy at bind"),
                    "{error}"
                );
                eprintln!("parent child resolver before readiness: {error}");
                lock
            });
            let child_ctx = context(storage.path());
            let configured = super::handle_configure(
                &request(
                    "configure",
                    json!({
                        "project_root": child, "storage_dir": storage.path(), "harness": "opencode",
                        "config": [{"tier": "user", "source": "child fixture", "doc": crate::test_project::repository_fixture_config(&child, json!({
                            "indexes": {"trigram": true, "semantic": false, "callgraph": false}, "views": {"enabled": false}
                        })).to_string()}]
                    }),
                ),
                &child_ctx,
            );
            assert!(configured.success, "{}", configured.data);
            if let Some(lock) = lock {
                settle(&child_ctx);
                assert!(child_ctx
                    .degraded_reasons()
                    .iter()
                    .any(|reason| reason.contains("aft.db was busy at bind")));
                assert!(child_ctx.cached_artifact_cache_key(&child).is_none());
                eprintln!(
                    "busy child before retry: {:?}",
                    child_ctx.degraded_reasons()
                );
                lock.execute_batch("ROLLBACK").unwrap();
                drop(lock);
            }
            // A derived Git key is not a child bind. Drive the same identity
            // retry as the runtime before the parent reads the child's artifact.
            wait_for_child_index(&child_ctx, &child, storage.path());
            if busy_child && name == "alpha" {
                assert_eq!(
                    crate::search_index::artifact_cache_key_read_only(&child, storage.path())
                        .unwrap(),
                    child_ctx.cached_artifact_cache_key(&child).unwrap()
                );
                eprintln!(
                    "busy child after retry: key={:?} degraded={:?}",
                    child_ctx.cached_artifact_cache_key(&child),
                    child_ctx.degraded_reasons()
                );
            }
            children.push(child_ctx);
        }
        let ctx = context(storage.path());
        let ack = super::handle_configure(
            &request(
                "configure",
                json!({
                    "project_root": root.path(), "storage_dir": storage.path(), "harness": "opencode",
                    "config": [{"tier": "user", "source": "workspace fixture", "doc": crate::test_project::repository_fixture_config(root.path(), json!({
                        "indexes": {"trigram": true, "semantic": false, "callgraph": false}, "views": {"enabled": true}
                    })).to_string()}]
                }),
            ),
            &ctx,
        );
        assert!(ack.success, "{}", ack.data);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let session = loop {
            crate::runtime_drain::drain_deferred_configure_maintenance(&ctx);
            if let Some(session) = crate::views::parent::session_for_root(root.path()) {
                break session;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "parent never settled: folder={:?} degraded={:?}",
                admission::snapshot(&ctx),
                ctx.degraded_reasons()
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["classification"],
            "workspace"
        );
        assert!(!ctx.heavy_root_work_allowed());
        assert!(ctx.search_index().read().unwrap().is_none());
        assert!(!ctx.root_too_broad());
        assert!(admission::workspace_watcher_allowed(&ctx));
        assert_eq!(
            ctx.watcher_runtime_active(),
            !super::file_watcher_disabled_for_test()
        );
        assert!(session.wait_rounds(1, Duration::from_secs(30)));
        let grep = crate::commands::grep::handle_grep(
            &request("grep", json!({"pattern": "workspace_alpha_marker"})),
            &ctx,
        );
        assert!(grep.success, "{}", grep.data);
        assert_eq!(grep.data["total_matches"], 1, "{}", grep.data);
        assert!(grep.data["text"].as_str().unwrap().contains("alpha"));
        let search = crate::commands::semantic_search::handle_semantic_search(
            &request(
                "semantic_search",
                json!({"query": "workspace_alpha_marker", "top_k": 5}),
            ),
            &ctx,
        );
        assert!(search.success, "{}", search.data);
        assert!(
            search.data.to_string().contains("workspace_alpha_marker"),
            "{}",
            search.data
        );
        assert_eq!(
            super::configure_artifact_load_attempts_for_root_for_test(root.path()),
            0
        );
        assert!(ctx.search_index().read().unwrap().is_none());
        assert!(configure(root.path(), storage.path(), &ctx, false, false).success);
        settle(&ctx);
        assert!(ctx.root_too_broad());
        assert!(!admission::workspace_watcher_allowed(&ctx));
        assert!(!ctx.watcher_runtime_active());
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["recursive_watcher"],
            "off"
        );
        crate::views::parent::deactivate(root.path());
    });
}

#[test]
fn folder_admission_only_user_config_can_force_an_export_to_index() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    isolated(root.path(), || {
        for name in 0..300 {
            std::fs::write(root.path().join(format!("{name}.dat")), []).unwrap();
        }
        let ctx = context(storage.path());
        let base = json!({"indexes": {"trigram": false, "semantic": false, "callgraph": false}, "views": {"enabled": false}});
        let project = request(
            "configure",
            json!({"project_root": root.path(), "storage_dir": storage.path(), "harness": "opencode", "config": [
                {"tier": "user", "source": "user fixture", "doc": base.to_string()},
                {"tier": "project", "source": "untrusted fixture", "doc": json!({"project_detection": {"mode": "project"}}).to_string()}
            ]}),
        );
        let ack = super::handle_configure(&project, &ctx);
        assert!(ack.success, "{}", ack.data);
        assert!(ack.data["config_dropped_keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|key| key["key"] == "project_detection"));
        settle(&ctx);
        let folder = admission::snapshot(&ctx).unwrap();
        assert_eq!(folder["classification"], Value::Null, "{folder}");
        assert_eq!(folder["state"], "pending", "{folder}");
        assert_eq!(folder["override_mode"], "auto", "{folder}");
        assert!(!ctx.heavy_root_work_allowed());
        let mut user = base;
        user["project_detection"] =
            json!({"overrides": [{"path": root.path(), "mode": "project"}]});
        let ack = super::handle_configure(
            &request(
                "configure",
                json!({"project_root": root.path(), "storage_dir": storage.path(), "harness": "opencode", "config": [
                    {"tier": "user", "source": "user fixture", "doc": user.to_string()}
                ]}),
            ),
            &ctx,
        );
        assert!(ack.success, "{}", ack.data);
        settle(&ctx);
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["classification"],
            "project"
        );
        assert_eq!(
            admission::snapshot(&ctx).unwrap()["reason"],
            "user_override"
        );
        assert!(ctx.heavy_root_work_allowed());
    });
}
