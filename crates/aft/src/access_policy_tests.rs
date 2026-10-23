use crate::config::Config;
use crate::context::AppContext;
use crate::language::StubProvider;
use crate::protocol::RawRequest;
use serde_json::json;
use std::path::{Path, PathBuf};

#[path = "access_policy_task_tests.rs"]
mod task_records;

fn context(root: &Path, denied: &Path) -> AppContext {
    let mut config = Config {
        project_root: Some(root.to_path_buf()),
        restrict_to_project_root: false,
        ..Config::default()
    };
    config.sandbox.read_deny = vec![denied.to_path_buf()];
    AppContext::new(Box::new(StubProvider), config)
}

fn request(command: &str, params: serde_json::Value) -> RawRequest {
    RawRequest {
        id: "policy-test".into(),
        command: command.into(),
        session_id: Some("policy-session".into()),
        lsp_hints: None,
        params,
    }
}

#[test]
fn access_policy_absolute_floor_inside_own_root() {
    let root = tempfile::tempdir().unwrap();
    let denied = root.path().join("secret");
    std::fs::write(&denied, "protected").unwrap();
    let ctx = context(root.path(), &denied);
    for result in [
        ctx.validate_read_path("read", "policy-session", &denied),
        ctx.validate_path("write", &denied),
        ctx.validate_write_location("unlink", &denied),
    ] {
        let error = result.expect_err("the absolute floor overrides an unrestricted own root");
        assert_eq!(error.data["code"], "denied_floor");
    }
    assert_eq!(std::fs::read_to_string(denied).unwrap(), "protected");
}

#[test]
fn access_policy_move_checks_both_locations_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let denied = root.path().join("floor");
    std::fs::write(&source, "source").unwrap();
    let ctx = context(root.path(), &denied);
    let response = crate::commands::move_file::handle_move_file(
        &request("move_file", json!({"file": source, "destination": denied})),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert!(source.exists());
    assert!(!denied.exists());
    assert!(ctx
        .backup()
        .lock()
        .tracked_files("policy-session")
        .is_empty());
    std::fs::write(&denied, "protected").unwrap();
    let response = crate::commands::move_file::handle_move_file(
        &request(
            "move_file",
            json!({"file": denied, "destination": root.path().join("other")}),
        ),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert_eq!(std::fs::read_to_string(denied).unwrap(), "protected");
}

#[test]
fn access_policy_delete_batch_checks_floor_before_first_unlink() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let denied = root.path().join("floor");
    std::fs::write(&source, "source").unwrap();
    std::fs::write(&denied, "protected").unwrap();
    let ctx = context(root.path(), &denied);
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"files": [source, denied]})),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert_eq!(std::fs::read_to_string(source).unwrap(), "source");
    assert_eq!(std::fs::read_to_string(denied).unwrap(), "protected");
    assert!(ctx
        .backup()
        .lock()
        .tracked_files("policy-session")
        .is_empty());
}

#[test]
fn access_policy_patch_checks_floor_before_first_hunk() {
    let root = tempfile::tempdir().unwrap();
    let allowed = root.path().join("allowed");
    let denied = root.path().join("floor");
    let ctx = context(root.path(), &denied);
    let patch = "*** Begin Patch\n*** Add File: allowed\n+allowed\n*** Add File: floor\n+protected\n*** End Patch";
    let response = crate::commands::apply_patch::handle_apply_patch(
        &request("apply_patch", json!({"patch_text": patch})),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert!(!allowed.exists());
    assert!(!denied.exists());
}

#[test]
fn access_policy_checkpoint_checks_floor_before_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let allowed = root.path().join("allowed");
    let denied = root.path().join("floor");
    std::fs::write(&allowed, "allowed").unwrap();
    std::fs::write(&denied, "protected").unwrap();
    let ctx = context(root.path(), &denied);
    let response = crate::commands::checkpoint::handle_checkpoint(
        &request(
            "checkpoint",
            json!({"name": "floor", "files": [allowed, denied]}),
        ),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert!(ctx
        .checkpoint()
        .lock()
        .file_paths("policy-session", "floor")
        .is_err());
}

fn saved_files(root: &Path) -> [PathBuf; 2] {
    let paths = [root.join("allowed"), root.join("floor")];
    for path in &paths {
        std::fs::write(path, "old").unwrap();
    }
    paths
}

#[test]
fn access_policy_restore_checks_all_stored_locations_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let paths = saved_files(root.path());
    let ctx = context(root.path(), &paths[1]);
    ctx.checkpoint()
        .lock()
        .create_for_files("policy-session", "saved", paths.to_vec())
        .unwrap();
    for path in &paths {
        std::fs::write(path, "new").unwrap();
    }
    let response = crate::commands::restore_checkpoint::handle_restore_checkpoint(
        &request("restore_checkpoint", json!({"name": "saved"})),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    for path in paths {
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new");
    }
}

#[test]
fn access_policy_undo_checks_all_operation_locations_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let paths = saved_files(root.path());
    let ctx = context(root.path(), &paths[1]);
    for path in &paths {
        ctx.backup()
            .lock()
            .snapshot_with_op("policy-session", path, "fixture", Some("saved"))
            .unwrap();
        std::fs::write(path, "new").unwrap();
    }
    let response = crate::commands::undo::handle_undo(&request("undo", json!({})), &ctx);
    assert_eq!(response.data["code"], "denied_floor");
    for path in paths {
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new");
    }
    assert_eq!(
        ctx.backup()
            .lock()
            .preview_last_operation_paths("policy-session")
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn access_policy_storage_floor_has_no_own_root_carve_out() {
    let store = tempfile::tempdir().unwrap();
    let root = store.path().join("own-worktree");
    std::fs::create_dir(&root).unwrap();
    let own = root.join("own");
    let sibling = store.path().join("other-worktree/file");
    let mut config = Config {
        project_root: Some(root.clone()),
        storage_dir: Some(store.path().to_path_buf()),
        ..Config::default()
    };
    config.sandbox.write_allow = vec![store.path().to_path_buf()];
    let ctx = AppContext::new(Box::new(StubProvider), config);
    for result in [
        ctx.validate_path("own", &own),
        ctx.validate_read_path("own-read", "policy-session", &own),
        ctx.validate_path("sibling", &sibling),
        ctx.validate_read_path("read", "policy-session", &sibling),
    ] {
        assert_eq!(result.unwrap_err().data["code"], "denied_floor");
    }
}

#[test]
fn access_policy_recursive_delete_cannot_remove_floor_descendant() {
    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    let denied = tree.join("floor");
    std::fs::write(&denied, "protected").unwrap();
    let ctx = context(root.path(), &denied);
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": tree, "recursive": true})),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert_eq!(std::fs::read_to_string(denied).unwrap(), "protected");
}

#[test]
fn access_policy_legacy_outcomes_are_byte_identical() {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_path = outside.path().canonicalize().unwrap().join("missing.txt");
    for configured_root in [None, Some(root_path.clone())] {
        for restrict in [false, true] {
            let ctx = AppContext::new(
                Box::new(StubProvider),
                Config {
                    project_root: configured_root.clone(),
                    restrict_to_project_root: restrict,
                    ..Config::default()
                },
            );
            for path in [Path::new("./missing.txt"), outside_path.as_path()] {
                for check in 0..3 {
                    let actual = match check {
                        0 => ctx.validate_path("parity", path),
                        1 => ctx.validate_read_path("parity", "policy-session", path),
                        _ => ctx.validate_write_location("parity", path),
                    };
                    if configured_root.is_some() && restrict && path == outside_path {
                        let expected = crate::protocol::Response::error(
                            "parity",
                            "path_outside_root",
                            format!(
                                "path '{}' is outside the project root '{}'",
                                path.display(),
                                root_path.display()
                            ),
                        );
                        assert_eq!(
                            serde_json::to_vec(&actual.unwrap_err()).unwrap(),
                            serde_json::to_vec(&expected).unwrap()
                        );
                    } else {
                        // Unrestricted, an absolute path keeps its spelling
                        // and a relative one is rooted where the tools resolve
                        // it: the project root, or the working directory when
                        // none is configured.
                        let expected = if configured_root.is_some() && restrict {
                            root_path.join("missing.txt")
                        } else if path.is_relative() {
                            configured_root
                                .clone()
                                .unwrap_or_else(|| std::env::current_dir().unwrap())
                                .join(path)
                        } else {
                            path.to_path_buf()
                        };
                        assert_eq!(
                            serde_json::to_vec(&actual.unwrap()).unwrap(),
                            serde_json::to_vec(&expected).unwrap()
                        );
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn access_policy_floor_follows_parent_alias_but_preserves_final_link() {
    let root = tempfile::tempdir().unwrap();
    let denied = root.path().join("floor");
    std::fs::create_dir(&denied).unwrap();
    std::fs::write(denied.join("file"), "protected").unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&denied, &alias).unwrap();
    let ctx = context(root.path(), &denied);
    assert_eq!(
        ctx.validate_path("read-alias", &alias.join("file"))
            .unwrap_err()
            .data["code"],
        "denied_floor"
    );
    assert_eq!(
        ctx.validate_write_location("write-alias", &alias.join("new"))
            .unwrap_err()
            .data["code"],
        "denied_floor"
    );
    assert_eq!(
        ctx.validate_write_location("unlink-alias", &alias).unwrap(),
        alias
    );
}

#[test]
fn access_policy_restricted_floor_refusal_keeps_legacy_bytes() {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("floor");
    let mut config = Config {
        project_root: Some(root_path.clone()),
        restrict_to_project_root: true,
        ..Config::default()
    };
    config.sandbox.read_deny = vec![path.clone()];
    let ctx = AppContext::new(Box::new(StubProvider), config);
    let expected = crate::protocol::Response::error(
        "parity-floor",
        "path_outside_root",
        format!(
            "path '{}' is outside the project root '{}'",
            path.display(),
            root_path.display()
        ),
    );
    for result in [
        ctx.validate_path("parity-floor", &path),
        ctx.validate_write_location("parity-floor", &path),
        ctx.validate_read_path("parity-floor", "session", &path),
    ] {
        assert_eq!(
            serde_json::to_vec(&result.unwrap_err()).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }
}

#[cfg(unix)]
#[test]
fn access_policy_preserved_location_resolves_parent_before_dotdot() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("worktree");
    let store = fixture.path().join("store");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir_all(store.join("child")).unwrap();
    let denied = store.join("secret");
    std::fs::write(&denied, "protected").unwrap();
    std::fs::write(root.join("secret"), "own").unwrap();
    std::os::unix::fs::symlink(store.join("child"), root.join("link")).unwrap();
    let candidate = root.join("link/../secret");
    let ctx = context(&root, &denied);
    assert_eq!(
        ctx.validate_write_location("parent-dotdot", &candidate)
            .unwrap_err()
            .data["code"],
        "denied_floor"
    );
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": candidate})),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert_eq!(std::fs::read_to_string(denied).unwrap(), "protected");
    assert_eq!(std::fs::read_to_string(root.join("secret")).unwrap(), "own");
}

fn artifact_read_context() -> (tempfile::TempDir, AppContext, PathBuf) {
    use crate::bash_background::persistence::{task_paths, write_task, PersistedTask};
    use crate::bash_background::BgTaskStatus;

    let directory = tempfile::tempdir().unwrap();
    let resolved = directory.path().canonicalize().unwrap();
    let root = resolved.join("project");
    let storage = resolved.join("storage");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&storage).unwrap();
    let config = Config {
        project_root: Some(root.clone()),
        storage_dir: Some(storage.clone()),
        restrict_to_project_root: false,
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let paths = task_paths(&storage, "policy-session", "bash-0123456789abcdef").unwrap();
    let mut metadata = PersistedTask::starting(
        "bash-0123456789abcdef".into(),
        "policy-session".into(),
        "echo artifact".into(),
        root.clone(),
        Some(root),
        None,
        false,
        false,
    );
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task(&paths.json, &metadata).unwrap();
    std::fs::write(&paths.stdout, "fn artifact() {}\n").unwrap();
    std::fs::write(&paths.stderr, "").unwrap();
    std::fs::write(&paths.exit, "0\n").unwrap();
    ctx.bash_background()
        .replay_session(&storage, "policy-session")
        .unwrap();
    assert!(ctx
        .bash_background()
        .is_session_owned_artifact_path("policy-session", &paths.stdout));
    ctx.set_harness(crate::harness::Harness::Opencode);
    (directory, ctx, paths.stdout)
}

pub(crate) fn assert_artifact_read_policy(
    handler: impl Fn(&RawRequest, &AppContext) -> crate::protocol::Response,
    command: &str,
    params: impl Fn(&Path) -> serde_json::Value,
) {
    let (_directory, ctx, artifact) = artifact_read_context();
    let owned = handler(&request(command, params(&artifact)), &ctx);
    for code in ["denied_floor", "path_outside_root"] {
        assert_ne!(
            owned.data["code"], code,
            "owner did not reach {command}: {:?}",
            owned.data
        );
    }
    if owned.data["code"] == "invalid_request" {
        // Output artifacts have no source-language extension. Reaching the
        // parser's extension error proves authorization passed, not parsing.
        assert_eq!(
            owned.data["message"],
            "invalid request: unsupported file extension: stdout"
        );
    }
    let unregistered = artifact.parent().unwrap().join("unregistered");
    std::fs::write(&unregistered, "fn artifact() {}\n").unwrap();
    for (session, path) in [
        ("other-session", artifact.as_path()),
        ("policy-session", unregistered.as_path()),
        ("policy-session", artifact.parent().unwrap()),
    ] {
        let mut req = request(command, params(path));
        req.session_id = Some(session.into());
        let denied = handler(&req, &ctx);
        assert!(!denied.success, "{command} leaked: {:?}", denied.data);
        assert_eq!(
            denied.data["code"], "denied_floor",
            "{command}: {:?}",
            denied.data
        );
        assert_eq!(
            denied.data["message"],
            format!(
                "path '{}' is denied by the access policy floor",
                path.display()
            )
        );
    }
}

macro_rules! artifact_read_tool_test {
    ($name:ident, $handler:expr, $command:literal, $params:expr) => {
        #[test]
        fn $name() {
            assert_artifact_read_policy($handler, $command, $params);
        }
    };
}

artifact_read_tool_test!(
    artifact_read_grep,
    crate::commands::grep::handle_grep,
    "grep",
    |path| json!({"path": path, "pattern": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_glob,
    crate::commands::glob::handle_glob,
    "glob",
    |path| json!({"path": path, "pattern": "*"})
);
artifact_read_tool_test!(
    artifact_read_outline,
    crate::commands::outline::handle_outline,
    "outline",
    |path| json!({"file": path})
);

#[test]
fn artifact_read_grep_and_glob_exact_file_under_root_restriction() {
    for restrict in [false, true] {
        let (_directory, ctx, artifact) = artifact_read_context();
        let mut config = (*ctx.config()).clone();
        config.restrict_to_project_root = restrict;
        ctx.set_config(config);
        let grep = crate::commands::grep::handle_grep(
            &request("grep", json!({"path": artifact, "pattern": "artifact"})),
            &ctx,
        );
        assert!(grep.success, "{restrict}: {:?}", grep.data);
        assert_eq!(grep.data["matches"][0]["line_text"], "fn artifact() {}");
        let glob = crate::commands::glob::handle_glob(
            &request("glob", json!({"path": artifact, "pattern": "*"})),
            &ctx,
        );
        assert!(glob.success, "{restrict}: {:?}", glob.data);
        let files = glob.data["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{restrict}: {:?}", glob.data);
        // Glob resolves paths before returning them, so compare file identities rather than
        // temp-directory aliases (macOS) or verbatim path prefixes (Windows).
        assert_eq!(
            std::fs::canonicalize(files[0].as_str().unwrap()).unwrap(),
            std::fs::canonicalize(&artifact).unwrap()
        );
    }
}

#[test]
fn artifact_read_storage_directory_grep_does_not_open_owned_outputs() {
    let (_directory, ctx, artifact) = artifact_read_context();
    let storage = ctx.config().storage_dir.clone().unwrap();
    let probe = crate::project_content::Probe::new(&storage);
    // Use storage as the default search root so this tests walker exclusion,
    // not the access policy's earlier refusal of an explicit storage path.
    ctx.update_config(|config| config.project_root = Some(storage.clone()));
    let grep =
        crate::commands::grep::handle_grep(&request("grep", json!({"pattern": "artifact"})), &ctx);
    assert!(grep.success, "{:?}", grep.data);
    assert_eq!(grep.data["total_matches"], 0, "{:?}", grep.data);
    assert!(probe.paths().is_empty(), "{:?}", probe.paths());
    assert!(ctx
        .bash_background()
        .explicit_task_artifacts("policy-session", &storage)
        .is_none());
    assert!(ctx
        .bash_background()
        .is_session_owned_artifact_path("policy-session", &artifact));
}

#[cfg(unix)]
#[test]
fn artifact_read_owned_hard_link_is_refused_by_grep_and_read() {
    let (_directory, ctx, artifact) = artifact_read_context();
    let ordinary = artifact.parent().unwrap().join("ordinary.txt");
    std::fs::write(&ordinary, "ordinary secret marker\n").unwrap();

    let normal_read =
        crate::commands::read::handle_read(&request("read", json!({"file": artifact})), &ctx);
    assert!(normal_read.success, "{:?}", normal_read.data);
    assert!(normal_read.data["content"]
        .as_str()
        .unwrap()
        .contains("fn artifact() {}"));

    std::fs::remove_file(&artifact).unwrap();
    std::fs::hard_link(&ordinary, &artifact).unwrap();
    assert!(ctx
        .bash_background()
        .is_session_owned_artifact_path("policy-session", &artifact));
    let grep = crate::commands::grep::handle_grep(
        &request(
            "grep",
            json!({"path": artifact, "pattern": "ordinary secret marker"}),
        ),
        &ctx,
    );
    assert!(grep.success, "{:?}", grep.data);
    assert_eq!(grep.data["total_matches"], 0, "{:?}", grep.data);

    let read =
        crate::commands::read::handle_read(&request("read", json!({"file": artifact})), &ctx);
    assert!(
        !read.success,
        "hard-linked artifact was readable: {:?}",
        read.data
    );
    assert_eq!(read.data["code"], "artifact_refused", "{:?}", read.data);
}

#[cfg(unix)]
#[test]
fn artifact_read_owned_sqlite_suffix_alias_is_refused() {
    let (_directory, ctx, artifact) = artifact_read_context();
    for suffix in [".db", ".sqlite", "-wal", "-shm", "-journal"] {
        let target = ctx
            .config()
            .storage_dir
            .as_ref()
            .unwrap()
            .join(format!("store{suffix}"));
        std::fs::write(&target, "not readable task output").unwrap();
        std::fs::remove_file(&artifact).unwrap();
        std::fs::hard_link(&target, &artifact).unwrap();
        assert!(ctx
            .bash_background()
            .is_session_owned_artifact_path("policy-session", &artifact));
        let response =
            crate::commands::read::handle_read(&request("read", json!({"file": artifact})), &ctx);
        assert!(!response.success, "{suffix}: {response:?}");
        assert!(
            matches!(
                response.data["code"].as_str(),
                Some("denied_floor" | "artifact_refused")
            ),
            "{response:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn artifact_read_owned_live_sqlite_alias_is_refused() {
    let (_directory, ctx, artifact) = artifact_read_context();
    let storage = ctx.config().storage_dir.clone().unwrap();
    let database = storage.join("live.db");
    let _connection = crate::db::file_identity::IdentityConnection::new(
        rusqlite::Connection::open(&database).unwrap(),
        "owned artifact alias test",
    );
    std::fs::remove_file(&artifact).unwrap();
    std::fs::hard_link(&database, &artifact).unwrap();
    assert!(ctx
        .bash_background()
        .is_session_owned_artifact_path("policy-session", &artifact));
    let read =
        crate::commands::read::handle_read(&request("read", json!({"file": artifact})), &ctx);
    assert!(!read.success, "{read:?}");
    let probe = crate::project_content::Probe::new(&storage);
    assert!(ctx
        .bash_background()
        .explicit_task_artifacts("policy-session", &artifact)
        .unwrap()
        .is_empty());
    let grep = crate::commands::grep::handle_grep(
        &request("grep", json!({"path": artifact, "pattern": "."})),
        &ctx,
    );
    assert_eq!(grep.data["code"], "denied_floor", "{:?}", grep.data);
    let glob = crate::commands::glob::handle_glob(
        &request("glob", json!({"path": artifact, "pattern": "*"})),
        &ctx,
    );
    assert_eq!(glob.data["code"], "denied_floor", "{:?}", glob.data);
    assert!(probe.paths().is_empty(), "{:?}", probe.paths());
}

artifact_read_tool_test!(
    artifact_read_outline_many,
    crate::commands::outline::handle_outline,
    "outline",
    |path| json!({"files": [path]})
);
artifact_read_tool_test!(
    artifact_read_zoom,
    crate::commands::zoom::handle_zoom,
    "zoom",
    |path| json!({"file": path, "symbol": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_ast_search,
    crate::commands::ast_search::handle_ast_search,
    "ast_search",
    |path| json!({"paths": [path], "lang": "rust", "pattern": "fn $NAME() { $$$ }"})
);
artifact_read_tool_test!(
    artifact_read_search,
    crate::commands::semantic_search::handle_semantic_search,
    "semantic_search",
    |path| json!({"path": path, "query": "artifact"})
);

#[test]
fn artifact_read_search_plain_storage_matches_main_bytes() {
    let _storage_override = crate::test_env::without_storage_override();
    let (_directory, ctx, artifact) = artifact_read_context();
    assert_artifact_search_main_bytes(&ctx, &artifact);
}

fn assert_artifact_search_main_bytes(ctx: &AppContext, path: &Path) {
    assert!(ctx
        .bash_background()
        .is_session_owned_artifact_path("policy-session", path));
    let response = crate::commands::semantic_search::handle_semantic_search(
        &request(
            "semantic_search",
            json!({"path": path, "query": "artifact"}),
        ),
        ctx,
    );
    assert_eq!(
        response.data["code"], "not_a_git_root",
        "{:?}",
        response.data
    );
    // Pin the complete wire response from before plain-folder scans existed,
    // including its message and the caller's original path spelling.
    let message = serde_json::to_string(&format!(
        "aft_search path must be the root of (or inside) another Git project to search: {}. To filter a subfolder of the current project, use grep or glob with path.",
        path.display()
    ))
    .unwrap();
    let expected = format!(
        "{{\"id\":\"policy-test\",\"success\":false,\"code\":\"not_a_git_root\",\"message\":{message}}}"
    );
    assert_eq!(serde_json::to_vec(&response).unwrap(), expected.as_bytes());
}

#[cfg(unix)]
#[test]
fn artifact_read_search_symlinked_storage_matches_main_bytes() {
    let _storage_override = crate::test_env::without_storage_override();
    let (directory, original, artifact) = artifact_read_context();
    let alias = directory.path().join("parent-alias");
    std::os::unix::fs::symlink(directory.path(), &alias).unwrap();
    let storage = alias.join("storage");
    let mut config = (*original.config()).clone();
    config.storage_dir = Some(storage.clone());
    // Even a project-root carve-out must not turn AFT storage into a plain scan.
    config.project_root = Some(storage.clone());
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.bash_background()
        .replay_session(&storage, "policy-session")
        .unwrap();
    ctx.set_harness(crate::harness::Harness::Opencode);
    let aliased_artifact = storage.join(
        artifact
            .strip_prefix(original.config().storage_dir.as_ref().unwrap())
            .unwrap(),
    );
    assert_ne!(storage, crate::access_policy::policy_path(&storage));
    assert_eq!(
        ctx.validate_read_path("policy-test", "policy-session", artifact.parent().unwrap())
            .unwrap_err()
            .data["code"],
        "denied_floor"
    );

    for path in [&artifact, &aliased_artifact] {
        assert_artifact_search_main_bytes(&ctx, path);
    }
}

#[test]
fn access_policy_cached_plain_scan_root_rechecks_protection() {
    let project = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let ctx = context(project.path(), &project.path().join("denied"));
    let path = folder.path().to_str().unwrap();
    assert!(ctx
        .resolve_external_git_root(project.path(), path, |_| false)
        .is_ok());
    let result = ctx.resolve_external_git_root(project.path(), path, |_| true);
    assert!(
        matches!(
            result,
            Err(crate::readonly_artifacts::GitRootResolutionError::ProtectedScanRoot)
        ),
        "{result:?}"
    );
}

artifact_read_tool_test!(
    artifact_read_callers,
    crate::commands::callers::handle_callers,
    "callers",
    |path| json!({"file": path, "symbol": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_call_tree,
    crate::commands::call_tree::handle_call_tree,
    "call_tree",
    |path| json!({"file": path, "symbol": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_impact,
    crate::commands::impact::handle_impact,
    "impact",
    |path| json!({"file": path, "symbol": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_trace_to,
    crate::commands::trace_to::handle_trace_to,
    "trace_to",
    |path| json!({"file": path, "symbol": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_trace_to_symbol,
    crate::commands::trace_to_symbol::handle_trace_to_symbol,
    "trace_to_symbol",
    |path| json!({"file": path, "symbol": "artifact", "toSymbol": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_trace_data,
    crate::commands::trace_data::handle_trace_data,
    "trace_data",
    |path| json!({"file": path, "symbol": "artifact", "expression": "value"})
);
artifact_read_tool_test!(
    artifact_read_lsp_hover,
    crate::commands::lsp_hover::handle_lsp_hover,
    "lsp_hover",
    |path| json!({"file": path, "line": 1, "character": 1})
);
artifact_read_tool_test!(
    artifact_read_lsp_goto_definition,
    crate::commands::lsp_goto_definition::handle_lsp_goto_definition,
    "lsp_goto_definition",
    |path| json!({"file": path, "line": 1, "character": 1})
);
artifact_read_tool_test!(
    artifact_read_lsp_find_references,
    crate::commands::lsp_find_references::handle_lsp_find_references,
    "lsp_find_references",
    |path| json!({"file": path, "line": 1, "character": 1})
);
artifact_read_tool_test!(
    artifact_read_lsp_prepare_rename,
    crate::commands::lsp_prepare_rename::handle_lsp_prepare_rename,
    "lsp_prepare_rename",
    |path| json!({"file": path, "line": 1, "character": 1})
);
artifact_read_tool_test!(
    artifact_read_lsp_inspect,
    crate::commands::lsp_inspect::handle_lsp_inspect,
    "lsp_inspect",
    |path| json!({"file": path})
);
artifact_read_tool_test!(
    artifact_read_lsp_diagnostics,
    crate::commands::lsp_diagnostics::handle_lsp_diagnostics,
    "lsp_diagnostics",
    |path| json!({"file": path})
);
artifact_read_tool_test!(
    artifact_read_inspect,
    crate::commands::inspect::handle_inspect,
    "inspect",
    |path| json!({"scope": path, "sections": ["todos"]})
);
artifact_read_tool_test!(
    artifact_read_conflicts,
    |req, ctx| crate::commands::conflicts::handle_git_conflicts(ctx, req),
    "conflicts",
    |path| json!({"path": path})
);

#[test]
fn artifact_read_ast_replace_still_requires_write_access() {
    let (_directory, ctx, artifact) = artifact_read_context();
    let response = crate::commands::ast_replace::handle_ast_replace(
        &request(
            "ast_replace",
            json!({"paths": [artifact], "lang": "rust", "pattern": "fn $NAME() { $$$ }", "rewrite": "fn changed() {}"}),
        ),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert_eq!(
        std::fs::read_to_string(&artifact).unwrap(),
        "fn artifact() {}\n"
    );
}

#[test]
fn artifact_read_search_does_not_authorize_containing_git_store() {
    // Storage resolution gives AFT_STORAGE_DIR precedence over configuration.
    // Keep other tests' process-wide overrides out of this private-store fixture.
    let _storage_override = crate::test_env::without_storage_override();
    let (_directory, ctx, artifact) = artifact_read_context();
    let storage = ctx.config().storage_dir.clone().unwrap();
    let initialized = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .arg(&storage)
        .status()
        .unwrap();
    assert!(initialized.success());
    std::fs::write(storage.join("private.rs"), "fn private_artifact() {}\n").unwrap();
    let mut index = crate::search_index::SearchIndex::build(&storage);
    let cache_dir = crate::search_index::resolve_cache_dir(&storage, Some(&storage));
    assert!(index.write_to_disk(&cache_dir, None));
    let response = crate::commands::semantic_search::handle_semantic_search(
        &request(
            "semantic_search",
            json!({"path": artifact, "pattern": "artifact"}),
        ),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor", "{:?}", response.data);
    assert_eq!(
        response.data["message"],
        format!(
            "path '{}' is denied by the access policy floor",
            std::fs::canonicalize(storage).unwrap().display()
        )
    );
}

fn scratch_read_context() -> (
    tempfile::TempDir,
    AppContext,
    PathBuf,
    (
        crate::access_policy::ScopeGuard,
        crate::access_policy::AskRouteGuard,
    ),
) {
    let (directory, ctx, _) = artifact_read_context();
    let scratch = directory
        .path()
        .canonicalize()
        .unwrap()
        .join(".sandbox-tmp-scratch");
    std::fs::create_dir(&scratch).unwrap();
    std::fs::write(scratch.join("scratch.rs"), "fn scratch_marker() {}\n").unwrap();
    let task = ctx
        .bash_background()
        .task_for_test("bash-0123456789abcdef")
        .unwrap();
    task.lock_state().unwrap().metadata.sandbox_temp_dir = Some(scratch.clone());
    let db = std::sync::Arc::new(std::sync::Mutex::new(
        crate::db::open(&directory.path().join("grants.db")).unwrap(),
    ));
    let stamp = serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
        "ref": "scratch-worker", "kind": "worker", "scope_epoch": 1,
        "owner_authorized": true
    }))
    .unwrap();
    let scope = crate::db::access_grants::ScopeKey::from_stamp(&stamp);
    crate::db::access_grants::register_scope(&db.lock().unwrap(), &scope, None, &ctx.config())
        .unwrap();
    crate::db::access_grants::register_worker(&db.lock().unwrap(), &scope).unwrap();
    ctx.set_db(db);
    let guard = crate::access_policy::scope_guard(Some(scope));
    let ask =
        crate::access_policy::ask_route_guard(Some(crate::access_policy::AskRoute::Consent {
            once: Vec::new(),
            launch: None,
        }));
    (directory, ctx, scratch, (guard, ask))
}

#[test]
fn scratch_read_own_file_and_directory_skip_permission_ask() {
    let (_directory, ctx, scratch, _scope) = scratch_read_context();
    for (command, params, handler) in [
        (
            "read",
            json!({"file": scratch.join("scratch.rs")}),
            crate::commands::read::handle_read
                as fn(&RawRequest, &AppContext) -> crate::protocol::Response,
        ),
        (
            "read",
            json!({"file": scratch}),
            crate::commands::read::handle_read,
        ),
        (
            "grep",
            json!({"path": scratch, "pattern": "scratch_marker"}),
            crate::commands::grep::handle_grep,
        ),
        (
            "glob",
            json!({"path": scratch, "pattern": "*.rs"}),
            crate::commands::glob::handle_glob,
        ),
    ] {
        let response = handler(&request(command, params), &ctx);
        assert!(response.success, "{command}: {:?}", response.data);
        assert!(
            response.data.to_string().contains("scratch"),
            "{command}: {:?}",
            response.data
        );
    }
}

#[test]
fn scratch_read_foreign_and_unregistered_paths_still_ask() {
    let (directory, ctx, scratch, _scope) = scratch_read_context();
    let unregistered = directory.path().join("unregistered");
    std::fs::create_dir(&unregistered).unwrap();
    std::fs::write(unregistered.join("scratch.rs"), "foreign scratch\n").unwrap();
    for path in [&scratch, &unregistered] {
        for command in ["read", "grep", "glob"] {
            let params = match command {
                "read" => json!({"file": path.join("scratch.rs")}),
                "grep" => json!({"path": path, "pattern": "scratch"}),
                _ => json!({"path": path, "pattern": "*"}),
            };
            let mut req = request(command, params);
            req.session_id = Some(
                if path == &scratch {
                    "foreign-session"
                } else {
                    "policy-session"
                }
                .into(),
            );
            let response = match command {
                "read" => crate::commands::read::handle_read(&req, &ctx),
                "grep" => crate::commands::grep::handle_grep(&req, &ctx),
                _ => crate::commands::glob::handle_glob(&req, &ctx),
            };
            assert!(!response.success, "{command} leaked: {:?}", response.data);
            assert_eq!(
                response.data["code"], "access_ask_required",
                "{command}: {:?}",
                response.data
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn scratch_read_symlink_escape_still_asks() {
    let (directory, ctx, scratch, _scope) = scratch_read_context();
    let outside = directory.path().join("secret.rs");
    std::fs::write(&outside, "fn outside_secret() {}\n").unwrap();
    let link = scratch.join("escape.rs");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let response =
        crate::commands::read::handle_read(&request("read", json!({"file": link})), &ctx);
    assert!(!response.success, "symlink leaked: {:?}", response.data);
    assert_eq!(
        response.data["code"], "access_ask_required",
        "{:?}",
        response.data
    );
    let grep = crate::commands::grep::handle_grep(
        &request(
            "grep",
            json!({"path": scratch, "pattern": "outside_secret"}),
        ),
        &ctx,
    );
    assert!(grep.success, "{:?}", grep.data);
    assert_eq!(grep.data["total_matches"], 0, "{:?}", grep.data);
}

#[test]
fn scratch_read_does_not_authorize_writes() {
    let (_directory, ctx, scratch, _scope) = scratch_read_context();
    let file = scratch.join("scratch.rs");
    let response = crate::commands::ast_replace::handle_ast_replace(
        &request(
            "ast_replace",
            json!({"paths": [file], "lang": "rust", "pattern": "fn $NAME() { $$$ }", "rewrite": "fn changed() {}"}),
        ),
        &ctx,
    );
    assert!(!response.success, "{:?}", response.data);
    assert_eq!(
        response.data["code"], "access_ask_required",
        "{:?}",
        response.data
    );
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        "fn scratch_marker() {}\n"
    );
}

#[test]
fn scratch_read_all_source_tools_reach_the_reader() {
    let (_directory, ctx, scratch, _scope) = scratch_read_context();
    let file = scratch.join("scratch.rs");
    let tools: &[(
        &str,
        serde_json::Value,
        fn(&RawRequest, &AppContext) -> crate::protocol::Response,
    )] = &[
        (
            "outline",
            json!({"file": file}),
            crate::commands::outline::handle_outline,
        ),
        (
            "zoom",
            json!({"file": file, "symbol": "scratch_marker"}),
            crate::commands::zoom::handle_zoom,
        ),
        (
            "callers",
            json!({"file": file, "symbol": "scratch_marker"}),
            crate::commands::callers::handle_callers,
        ),
        (
            "call_tree",
            json!({"file": file, "symbol": "scratch_marker"}),
            crate::commands::call_tree::handle_call_tree,
        ),
        (
            "ast_search",
            json!({"paths": [file], "lang": "rust", "pattern": "fn $NAME() { $$$ }"}),
            crate::commands::ast_search::handle_ast_search,
        ),
        (
            "inspect",
            json!({"scope": file, "sections": ["todos"]}),
            crate::commands::inspect::handle_inspect,
        ),
    ];
    for (command, params, handler) in tools {
        let own = handler(&request(command, params.clone()), &ctx);
        if matches!(*command, "callers" | "call_tree") {
            // Callgraph queries remain project-indexed; an external scratch
            // target passes authorization before the existing index refusal.
            assert_eq!(
                own.data["code"], "path_outside_project_root",
                "{command}: {:?}",
                own.data
            );
        } else if *command == "inspect" && own.data["code"] == "inspect_not_fresh" {
            // Authorization precedes scanning; a busy scanner's independent
            // freshness deadline does not turn an authorized read into an ask.
            assert!(
                own.data["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("todos did not complete"),
                "{:?}",
                own.data
            );
        } else {
            assert!(own.success, "{command}: {:?}", own.data);
        }
        let mut foreign = request(command, params.clone());
        foreign.session_id = Some("foreign-session".into());
        let denied = handler(&foreign, &ctx);
        assert_eq!(
            denied.data["code"], "access_ask_required",
            "{command}: {:?}",
            denied.data
        );
    }
}

#[test]
fn scratch_read_bridge_capability_is_session_scoped() {
    let (_directory, ctx, scratch, _scope) = scratch_read_context();
    for path in [&scratch, &scratch.join("scratch.rs")] {
        for (session, expected) in [("policy-session", true), ("foreign-session", false)] {
            let mut req = request("bash_artifact_owned", json!({"path": path}));
            req.session_id = Some(session.into());
            let response = crate::commands::bash_artifact_owned::handle(&req, &ctx);
            assert_eq!(response.data["owned"], expected, "{:?}", response.data);
        }
    }
    assert!(!crate::bash_background::BgTaskRegistry::default()
        .is_session_owned_scratch_path("policy-session", &scratch));
    let task = ctx
        .bash_background()
        .task_for_test("bash-0123456789abcdef")
        .unwrap();
    task.lock_state().unwrap().metadata.sandbox_temp_dir = None;
    assert!(!ctx
        .bash_background()
        .is_session_owned_scratch_path("policy-session", &scratch));
    let response = crate::commands::read::handle_read(
        &request("read", json!({"file": scratch.join("scratch.rs")})),
        &ctx,
    );
    assert_eq!(
        response.data["code"], "access_ask_required",
        "{:?}",
        response.data
    );
}

#[test]
fn scratch_read_respects_absolute_floor_and_live_sqlite() {
    let (_directory, ctx, scratch, _scope) = scratch_read_context();
    let file = scratch.join("scratch.rs");
    ctx.update_config(|config| config.sandbox.read_deny.push(file.clone()));
    let response =
        crate::commands::read::handle_read(&request("read", json!({"file": file})), &ctx);
    assert_eq!(response.data["code"], "denied_floor", "{:?}", response.data);
    let database = scratch.join("live.db");
    let _connection = crate::db::file_identity::IdentityConnection::new(
        rusqlite::Connection::open(&database).unwrap(),
        "scratch read live sqlite test",
    );
    assert!(!ctx
        .bash_background()
        .is_session_owned_scratch_path("policy-session", &database));
    assert!(crate::task_temp_reads::open(&database).is_err());
}

#[cfg(unix)]
#[test]
fn scratch_read_inner_scratch_name_cannot_become_trusted_root() {
    let (directory, ctx, scratch, _scope) = scratch_read_context();
    let inner = scratch.join(".sandbox-tmp-inner");
    std::fs::create_dir(&inner).unwrap();
    let file = inner.join("file.txt");
    std::fs::write(&file, "own scratch\n").unwrap();
    let validated = ctx
        .validate_read_path("policy-test", "policy-session", &file)
        .unwrap();
    assert_eq!(validated, file);
    assert_eq!(
        crate::bounded_io::read(&validated, None).unwrap(),
        b"own scratch\n"
    );
    assert_eq!(
        crate::project_content::read(&validated).unwrap(),
        b"own scratch\n"
    );

    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("file.txt"), "outside secret\n").unwrap();
    std::fs::remove_file(&file).unwrap();
    std::fs::remove_dir(&inner).unwrap();
    std::os::unix::fs::symlink(&outside, &inner).unwrap();

    let read = crate::bounded_io::read(&validated, None);
    assert!(read.is_err(), "inner scratch symlink leaked: {read:?}");
    assert_eq!(
        read.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        crate::project_content::read(&validated).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        crate::task_temp_reads::open(&validated).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[cfg(unix)]
#[test]
fn scratch_read_parent_of_inner_scratch_name_cannot_escape() {
    let (directory, ctx, scratch, _scope) = scratch_read_context();
    let parent = scratch.join("nested");
    let inner = parent.join(".sandbox-tmp-inner");
    std::fs::create_dir_all(&inner).unwrap();
    let file = inner.join("file.txt");
    std::fs::write(&file, "own scratch\n").unwrap();
    let validated = ctx
        .validate_read_path("policy-test", "policy-session", &file)
        .unwrap();
    assert_eq!(validated, file);

    let outside = directory.path().join("outside");
    std::fs::create_dir_all(outside.join(".sandbox-tmp-inner")).unwrap();
    std::fs::write(
        outside.join(".sandbox-tmp-inner/file.txt"),
        "outside secret\n",
    )
    .unwrap();
    std::fs::remove_dir_all(&parent).unwrap();
    std::os::unix::fs::symlink(&outside, &parent).unwrap();

    let read = crate::bounded_io::read(&validated, None);
    assert!(
        read.is_err(),
        "inner scratch parent symlink leaked: {read:?}"
    );
    assert_eq!(
        read.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        crate::project_content::read(&validated).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        crate::task_temp_reads::open(&validated).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[cfg(unix)]
#[test]
fn scratch_read_replaced_root_symlink_is_refused() {
    let (directory, ctx, scratch, _scope) = scratch_read_context();
    let file = scratch.join("scratch.rs");
    let validated = ctx
        .validate_read_path("policy-test", "policy-session", &file)
        .unwrap();
    assert_eq!(validated, file);
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("scratch.rs"), "outside secret\n").unwrap();
    std::fs::remove_file(&file).unwrap();
    std::fs::remove_dir(&scratch).unwrap();
    std::os::unix::fs::symlink(&outside, &scratch).unwrap();

    let read = crate::bounded_io::read(&validated, None);
    assert!(read.is_err(), "scratch root symlink leaked: {read:?}");
    assert_eq!(
        read.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        crate::project_content::read(&validated).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        crate::task_temp_reads::open(&validated).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[cfg(unix)]
#[test]
fn scratch_read_opened_handle_cannot_escape_after_authorization() {
    let (directory, ctx, scratch, _scope) = scratch_read_context();
    let nested = scratch.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let file = nested.join("secret.rs");
    std::fs::write(&file, "own scratch\n").unwrap();
    assert!(ctx
        .validate_read_path("policy-test", "policy-session", &file)
        .is_ok());
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.rs"), "outside secret\n").unwrap();
    std::fs::remove_file(&file).unwrap();
    std::fs::remove_dir(&nested).unwrap();
    std::os::unix::fs::symlink(&outside, &nested).unwrap();
    let read = crate::bounded_io::read(&file, None);
    assert!(read.is_err(), "opened handle escaped: {read:?}");
    assert!(crate::project_content::read(&file).is_err());
    assert!(crate::task_temp_reads::open(&file).is_err());
}

fn assert_scratch_read_unregistered_path(path: &Path, project_root: &Path) {
    let config = Config {
        project_root: Some(project_root.to_path_buf()),
        restrict_to_project_root: false,
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(StubProvider), config);
    let validated = ctx
        .validate_read_path("policy-test", "policy-session", path)
        .unwrap();
    // A relative path names a file under the project root, and the validator
    // returns that rooted location so the read touches the checked file.
    assert_eq!(validated, project_root.join(path));
    let expected = b"unregistered scratch\n";
    assert_eq!(crate::bounded_io::read(&validated, None).unwrap(), expected);
    assert_eq!(crate::project_content::read(&validated).unwrap(), expected);
    let mut file = crate::task_temp_reads::open(&validated).unwrap();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes).unwrap();
    assert_eq!(bytes, expected);
    let response =
        crate::commands::read::handle_read(&request("read", json!({"file": path})), &ctx);
    assert!(response.success, "{:?}", response.data);
}

#[test]
fn scratch_read_relative_unregistered_path_succeeds() {
    let cwd = std::env::current_dir().unwrap();
    let directory = tempfile::tempdir_in(&cwd).unwrap();
    let scratch = directory.path().join(".sandbox-tmp-unregistered");
    std::fs::create_dir(&scratch).unwrap();
    let file = scratch.join("a.txt");
    std::fs::write(&file, "unregistered scratch\n").unwrap();
    assert_scratch_read_unregistered_path(
        file.strip_prefix(directory.path()).unwrap(),
        directory.path(),
    );
}

#[cfg(unix)]
#[test]
fn scratch_read_relative_symlinked_prefix_succeeds() {
    let cwd = std::env::current_dir().unwrap();
    let directory = tempfile::tempdir_in(&cwd).unwrap();
    let real = directory.path().join("real");
    let scratch = real.join(".sandbox-tmp-unregistered");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(scratch.join("a.txt"), "unregistered scratch\n").unwrap();
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let file = alias.join(".sandbox-tmp-unregistered/a.txt");
    assert_scratch_read_unregistered_path(
        file.strip_prefix(directory.path()).unwrap(),
        directory.path(),
    );
    assert_scratch_read_unregistered_path(&file, directory.path());
}

#[cfg(target_os = "macos")]
#[test]
fn scratch_read_var_alias_prefix_succeeds() {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    assert_ne!(directory.path(), directory.path().canonicalize().unwrap());
    let scratch = directory.path().join(".sandbox-tmp-unregistered");
    std::fs::create_dir(&scratch).unwrap();
    let file = scratch.join("a.txt");
    std::fs::write(&file, "unregistered scratch\n").unwrap();
    assert_scratch_read_unregistered_path(&file, directory.path());
}

#[cfg(unix)]
#[test]
fn scratch_read_canonicalizes_alias_before_opening() {
    let (_directory, ctx, scratch, _scope) = scratch_read_context();
    let alias = ctx
        .config()
        .project_root
        .clone()
        .unwrap()
        .join("scratch-alias");
    std::os::unix::fs::symlink(&scratch, &alias).unwrap();
    let resolved = ctx
        .validate_read_path("policy-test", "policy-session", &alias.join("scratch.rs"))
        .unwrap();
    assert_eq!(resolved, scratch.join("scratch.rs"));
    let response = crate::commands::read::handle_read(
        &request("read", json!({"file": alias.join("scratch.rs")})),
        &ctx,
    );
    assert!(response.success, "{:?}", response.data);
}

#[cfg(unix)]
#[test]
fn scratch_read_allocator_path_uses_the_task_record() {
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = crate::sandbox_spawn::cleanup_task_temp_dir(&self.0);
        }
    }
    let (_directory, ctx, _scratch, _scope) = scratch_read_context();
    let task = ctx
        .bash_background()
        .task_for_test("bash-0123456789abcdef")
        .unwrap();
    let allocated =
        Cleanup(crate::sandbox_spawn::create_task_temp_dir(&task.paths.io_dir).unwrap());
    let file = allocated.0.join("worker.diff");
    std::fs::write(&file, "own scratch diff\n").unwrap();
    task.lock_state().unwrap().metadata.sandbox_temp_dir = Some(allocated.0.clone());
    let response =
        crate::commands::read::handle_read(&request("read", json!({"file": file})), &ctx);
    assert!(response.success, "{:?}", response.data);
    assert!(response.data["content"]
        .as_str()
        .unwrap()
        .contains("own scratch diff"));
    assert!(!ctx
        .bash_background()
        .is_session_owned_scratch_path("foreign-session", &file));
}

/// A finished command's scratch is kept for a while after it exits. A file the
/// command wrote to its `$TMPDIR` is therefore readable by the same session's
/// read tools in the next step, with no permission ask. Another session still
/// asks, and a write still goes through the normal policy.
#[cfg(unix)]
#[test]
fn scratch_read_right_after_command_exit_reads_kept_scratch_without_ask() {
    let (_directory, ctx, _scratch, _scope) = scratch_read_context();
    let task = ctx
        .bash_background()
        .task_for_test("bash-0123456789abcdef")
        .unwrap();
    let scratch = crate::sandbox_spawn::create_task_temp_dir(&task.paths.io_dir).unwrap();
    task.lock_state().unwrap().metadata.sandbox_temp_dir = Some(scratch.clone());
    let written = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("printf 'fn kept_scratch() {}\\n' > \"$TMPDIR/kept.rs\"")
        .env("TMPDIR", &scratch)
        .status()
        .unwrap();
    assert!(written.success());
    // The same terminal cleanup a real command exit runs: it keeps the
    // scratch and its recorded path instead of deleting them.
    assert!(ctx
        .bash_background()
        .cleanup_terminal_task_temp(&task)
        .unwrap());
    let file = scratch.join("kept.rs");
    assert!(file.is_file(), "command exit deleted the scratch");
    let run = |session: &str, command: &str| {
        let params = match command {
            "read" | "outline" => json!({"file": file}),
            "grep" => json!({"path": scratch, "pattern": "kept_scratch"}),
            _ => json!({"path": scratch, "pattern": "*.rs"}),
        };
        let mut req = request(command, params);
        req.session_id = Some(session.into());
        match command {
            "read" => crate::commands::read::handle_read(&req, &ctx),
            "outline" => crate::commands::outline::handle_outline(&req, &ctx),
            "grep" => crate::commands::grep::handle_grep(&req, &ctx),
            _ => crate::commands::glob::handle_glob(&req, &ctx),
        }
    };
    for command in ["read", "outline", "grep", "glob"] {
        let own = run("policy-session", command);
        assert!(own.success, "{command} after exit: {:?}", own.data);
        assert!(
            own.data.to_string().contains("kept"),
            "{command}: {:?}",
            own.data
        );
        let foreign = run("foreign-session", command);
        assert_eq!(
            foreign.data["code"], "access_ask_required",
            "{command}: {:?}",
            foreign.data
        );
    }
    let write = crate::commands::write::handle_write(
        &request(
            "write",
            json!({"file": file, "content": "fn rewritten() {}\n"}),
        ),
        &ctx,
    );
    assert_eq!(
        write.data["code"], "access_ask_required",
        "write into kept scratch: {:?}",
        write.data
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn kept_scratch() {}\n"
    );
    let _ = crate::sandbox_spawn::cleanup_task_temp_dir(&scratch);
}

/// The worker flow seen in production: one shell call writes into its own
/// `$TMPDIR` (the task's allocated scratch), and a later read or outline names
/// that file. While the task still records its scratch the read succeeds; once
/// cleanup has removed the scratch after the command exited, the same read
/// answers not-found instead of raising a permission ask. Foreign sessions,
/// writes and a recreated root that links elsewhere keep asking.
#[cfg(unix)]
#[test]
fn scratch_read_after_command_exit_is_not_found_without_ask() {
    let (directory, ctx, _scratch, _scope) = scratch_read_context();
    let task = ctx
        .bash_background()
        .task_for_test("bash-0123456789abcdef")
        .unwrap();
    let scratch = crate::sandbox_spawn::create_task_temp_dir(&task.paths.io_dir).unwrap();
    task.lock_state().unwrap().metadata.sandbox_temp_dir = Some(scratch.clone());
    let written = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("printf 'fn worker_scratch() {}\\n' > \"$TMPDIR/ict.rs\"")
        .env("TMPDIR", &scratch)
        .status()
        .unwrap();
    assert!(written.success());
    let file = scratch.join("ict.rs");
    let read = |session: &str, command: &str| {
        let params = json!({"file": file});
        let mut req = request(command, params);
        req.session_id = Some(session.into());
        match command {
            "read" => crate::commands::read::handle_read(&req, &ctx),
            _ => crate::commands::outline::handle_outline(&req, &ctx),
        }
    };
    for command in ["read", "outline"] {
        let live = read("policy-session", command);
        assert!(live.success, "{command} while live: {:?}", live.data);
        assert!(
            live.data.to_string().contains("worker_scratch"),
            "{command}: {:?}",
            live.data
        );
    }

    // The registry's own terminal cleanup deletes the scratch and clears the
    // recorded path, exactly as it does once the retention time after a real
    // command's exit has passed (the clock is moved past it here).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !ctx
        .bash_background()
        .cleanup_terminal_task_temp_at(&task, u64::MAX)
        .unwrap()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "scratch cleanup did not finish"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!scratch.exists(), "cleanup must remove the scratch");
    assert!(task
        .lock_state()
        .unwrap()
        .metadata
        .sandbox_temp_dir
        .is_none());

    for command in ["read", "outline"] {
        let gone = read("policy-session", command);
        assert!(!gone.success, "{command}: {:?}", gone.data);
        assert_eq!(gone.data["code"], "not_found", "{command}: {:?}", gone.data);
        assert!(
            gone.data["message"]
                .as_str()
                .unwrap()
                .contains("task scratch is removed when its command exits"),
            "{command}: {:?}",
            gone.data
        );
        let foreign = read("foreign-session", command);
        assert_eq!(
            foreign.data["code"], "access_ask_required",
            "{command}: {:?}",
            foreign.data
        );
    }
    // Workers often spell the scratch through the /tmp alias on macOS.
    #[cfg(target_os = "macos")]
    {
        let alias = Path::new("/tmp").join(file.strip_prefix("/private/tmp").unwrap());
        let response =
            crate::commands::read::handle_read(&request("read", json!({"file": alias})), &ctx);
        assert_eq!(response.data["code"], "not_found", "{:?}", response.data);
    }
    for (session, expected) in [("policy-session", true), ("foreign-session", false)] {
        let mut req = request("bash_artifact_owned", json!({"path": file}));
        req.session_id = Some(session.into());
        let response = crate::commands::bash_artifact_owned::handle(&req, &ctx);
        assert_eq!(response.data["owned"], expected, "{:?}", response.data);
    }
    let write = crate::commands::write::handle_write(
        &request(
            "write",
            json!({"file": file, "content": "fn rewritten() {}\n"}),
        ),
        &ctx,
    );
    assert_eq!(
        write.data["code"], "access_ask_required",
        "{:?}",
        write.data
    );
    assert!(!file.exists());

    // A root name recreated as a link to another folder is not the removed
    // scratch: the read resolves outside it and asks as before.
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("ict.rs"), "fn outside_secret() {}\n").unwrap();
    std::os::unix::fs::symlink(&outside, &scratch).unwrap();
    let linked = read("policy-session", "read");
    let _ = std::fs::remove_file(&scratch);
    assert_eq!(
        linked.data["code"], "access_ask_required",
        "{:?}",
        linked.data
    );
}
