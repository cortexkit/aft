use crate::config::Config;
use crate::context::AppContext;
use crate::language::StubProvider;
use crate::protocol::{RawRequest, Response};
use serde_json::json;
use std::path::{Path, PathBuf};

struct Fixture {
    _storage_env: crate::test_env::StorageOverrideGuard,
    _root: tempfile::TempDir,
    ctx: AppContext,
    store: PathBuf,
    neighbor: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let storage_env = crate::test_env::without_storage_override();
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        std::fs::create_dir(&storage).unwrap();
        let store = storage.join("aft.db");
        let neighbor = root.path().join("neighbor.txt");
        std::fs::write(&store, "original\n").unwrap();
        std::fs::write(&neighbor, "original\n").unwrap();
        let ctx = AppContext::new(
            Box::new(StubProvider),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage),
                restrict_to_project_root: false,
                ..Config::default()
            },
        );
        Self {
            _storage_env: storage_env,
            _root: root,
            ctx,
            store,
            neighbor,
        }
    }

    fn assert_refused(&self, call: impl FnOnce() -> Response) {
        let identity = crate::db::file_identity::identity_of(&self.store);
        let modified = std::fs::metadata(&self.store).unwrap().modified().unwrap();
        let response = call();
        assert_eq!(response.data["code"], "denied_floor", "{response:?}");
        assert_eq!(
            response.data["message"],
            format!(
                "path '{}' is denied by the access policy floor",
                self.store.display()
            )
        );
        assert_eq!(crate::db::file_identity::identity_of(&self.store), identity);
        assert_eq!(
            std::fs::metadata(&self.store).unwrap().modified().unwrap(),
            modified
        );
        assert!(self
            .ctx
            .backup()
            .lock()
            .tracked_files("storage-floor")
            .is_empty());
    }
}

fn has_floor_skip(response: &Response, path: &Path) -> bool {
    let expected = crate::access_policy::policy_path(path);
    response.data["skipped_files"]
        .as_array()
        .is_some_and(|files| {
            files.iter().any(|file| {
                file["reason"] == "denied_floor"
                    && file["file"].as_str().is_some_and(|file| {
                        crate::access_policy::policy_path(Path::new(file)) == expected
                    })
            })
        })
}

fn request(command: &str, params: serde_json::Value) -> RawRequest {
    RawRequest {
        id: "storage-floor".into(),
        command: command.into(),
        session_id: Some("storage-floor".into()),
        lsp_hints: None,
        params,
    }
}

#[test]
fn read_refuses_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::read::handle_read(&request("read", json!({"file": f.store})), &f.ctx)
    });
}

#[test]
fn write_refuses_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::write::handle_write(
            &request("write", json!({"file": f.store, "content": "changed"})),
            &f.ctx,
        )
    });
}

#[test]
fn edit_refuses_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::edit_match::handle_edit_match(
            &request(
                "edit_match",
                json!({"file": f.store, "match": "original", "replacement": "changed"}),
            ),
            &f.ctx,
        )
    });
}

fn patch(path: &Path) -> String {
    format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-original\n+changed\n*** End Patch",
        path.display()
    )
}

#[test]
fn apply_patch_refuses_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::apply_patch::handle_apply_patch(
            &request("apply_patch", json!({"patch_text": patch(&f.store)})),
            &f.ctx,
        )
    });
}

#[test]
fn delete_refuses_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::delete_file::handle_delete_file(
            &request("delete_file", json!({"files": [f.store]})),
            &f.ctx,
        )
    });
}

#[test]
fn move_refuses_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::move_file::handle_move_file(
            &request(
                "move_file",
                json!({"file": f.store, "destination": f.neighbor.with_extension("moved")}),
            ),
            &f.ctx,
        )
    });
    f.assert_refused(|| {
        super::move_file::handle_move_file(
            &request(
                "move_file",
                json!({"file": f.neighbor, "destination": f.store}),
            ),
            &f.ctx,
        )
    });
}

#[test]
fn normal_neighbor_of_storage_floor_still_works() {
    let f = Fixture::new();
    let read = super::read::handle_read(&request("read", json!({"file": f.neighbor})), &f.ctx);
    assert!(read.success, "{read:?}");
    let write = super::write::handle_write(
        &request("write", json!({"file": f.neighbor, "content": "normal"})),
        &f.ctx,
    );
    assert!(write.success, "{write:?}");
    assert_eq!(std::fs::read_to_string(&f.neighbor).unwrap(), "normal");
    let edit = super::edit_match::handle_edit_match(
        &request(
            "edit_match",
            json!({"file": f.neighbor, "match": "normal", "replacement": "original\n"}),
        ),
        &f.ctx,
    );
    assert!(edit.success, "{edit:?}");
    let patch = super::apply_patch::handle_apply_patch(
        &request("apply_patch", json!({"patch_text": patch(&f.neighbor)})),
        &f.ctx,
    );
    assert!(patch.success, "{patch:?}");
    let destination = f.neighbor.with_extension("moved");
    let moved = super::move_file::handle_move_file(
        &request(
            "move_file",
            json!({"file": f.neighbor, "destination": destination}),
        ),
        &f.ctx,
    );
    assert!(moved.success, "{moved:?}");
    let deleted = super::delete_file::handle_delete_file(
        &request("delete_file", json!({"files": [destination]})),
        &f.ctx,
    );
    assert!(deleted.success, "{deleted:?}");
    assert!(!destination.exists());
}

#[test]
fn grep_and_glob_explicit_paths_refuse_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::grep::handle_grep(
            &request("grep", json!({"path": f.store, "pattern": "original"})),
            &f.ctx,
        )
    });
    f.assert_refused(|| {
        super::glob::handle_glob(
            &request("glob", json!({"path": f.store, "pattern": "*"})),
            &f.ctx,
        )
    });
}

#[test]
fn recursive_delete_refuses_ancestor_of_storage_floor() {
    let f = Fixture::new();
    let response = super::delete_file::handle_delete_file(
        &request(
            "delete_file",
            json!({"files": [f._root.path()], "recursive": true}),
        ),
        &f.ctx,
    );
    assert_eq!(response.data["code"], "denied_floor", "{response:?}");
    assert!(f.store.exists());
    assert!(f.neighbor.exists());
}

struct SqliteAliases {
    _connection: crate::db::file_identity::IdentityConnection,
    fixture: Fixture,
    aliases: Vec<PathBuf>,
    history: crate::backup::BackupStore,
}

impl SqliteAliases {
    fn new() -> Self {
        let fixture = Fixture::new();
        std::fs::remove_file(&fixture.store).unwrap();
        // Seed old history before SQLite opens, then replace the project files
        // with aliases. The fixture itself must not read a live SQLite file.
        let mut history = crate::backup::BackupStore::new();
        let aliases = ["", "-wal", "-shm"]
            .into_iter()
            .enumerate()
            .map(|(i, _)| {
                let alias = fixture._root.path().join(format!("alias-{i}.txt"));
                std::fs::write(&alias, "original\n").unwrap();
                history
                    .snapshot_with_op("alias", &alias, "before registration", Some("alias-op"))
                    .unwrap();
                alias
            })
            .collect::<Vec<_>>();
        let connection = rusqlite::Connection::open(&fixture.store).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE source(value TEXT);")
            .unwrap();
        for (alias, suffix) in aliases.iter().zip(["", "-wal", "-shm"]) {
            std::fs::remove_file(alias).unwrap();
            std::fs::hard_link(
                PathBuf::from(format!("{}{suffix}", fixture.store.display())),
                alias,
            )
            .unwrap();
        }
        let connection =
            crate::db::file_identity::IdentityConnection::new(connection, "storage alias fixture");
        Self {
            _connection: connection,
            fixture,
            aliases,
            history,
        }
    }

    fn assert_refused(&self, path: &Path, call: impl FnOnce() -> Response) {
        let before = crate::file_identity::metadata(path).unwrap();
        let response = call();
        assert_eq!(
            response.data["code"], "denied_floor",
            "{path:?}: {response:?}"
        );
        let after = crate::file_identity::metadata(path).unwrap();
        assert_eq!(before.1, after.1);
        assert_eq!(before.0.modified().unwrap(), after.0.modified().unwrap());
    }
}

#[test]
fn storage_floor_live_sqlite_aliases_refuse_read_write_edit() {
    let f = SqliteAliases::new();
    for path in &f.aliases {
        f.assert_refused(path, || {
            super::read::handle_read(&request("read", json!({"file": path})), &f.fixture.ctx)
        });
        f.assert_refused(path, || {
            super::write::handle_write(
                &request("write", json!({"file": path, "content": "changed"})),
                &f.fixture.ctx,
            )
        });
        f.assert_refused(path, || {
            super::edit_match::handle_edit_match(
                &request(
                    "edit_match",
                    json!({"file": path, "match": "original", "replacement": "changed"}),
                ),
                &f.fixture.ctx,
            )
        });
    }
    assert_eq!(
        f._connection
            .query_row("SELECT count(*) FROM source", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn storage_floor_live_sqlite_aliases_refuse_backup_capture_and_undo() {
    let mut f = SqliteAliases::new();
    for path in &f.aliases {
        let before = crate::file_identity::metadata(path).unwrap();
        let mut snapshots = crate::backup::BackupStore::new();
        assert_eq!(
            snapshots
                .snapshot("alias", path, "refused")
                .unwrap_err()
                .code(),
            "denied_floor"
        );
        assert!(crate::backup::CapturedRegularFile::read(path).is_err());
        assert_eq!(
            crate::backup::PathFingerprint::of_path(path),
            crate::backup::PathFingerprint::Other
        );
        assert_eq!(
            f.history.restore_latest("alias", path).unwrap_err().code(),
            "denied_floor"
        );
        let after = crate::file_identity::metadata(path).unwrap();
        assert_eq!(before.1, after.1);
        assert_eq!(before.0.modified().unwrap(), after.0.modified().unwrap());
    }
    assert_eq!(
        f.history
            .restore_last_operation("alias")
            .unwrap_err()
            .code(),
        "denied_floor"
    );
    assert_eq!(
        f._connection
            .query_row("SELECT count(*) FROM source", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn storage_floor_sidecars_created_after_registration_refuse_project_aliases() {
    let fixture = Fixture::new();
    std::fs::remove_file(&fixture.store).unwrap();
    let connection =
        crate::db::file_identity::IdentityConnection::open(&fixture.store, "late sidecar fixture")
            .unwrap();
    connection
        .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE source(value TEXT);")
        .unwrap();
    for suffix in ["-wal", "-shm"] {
        let source = PathBuf::from(format!("{}{suffix}", fixture.store.display()));
        let alias = fixture._root.path().join(format!("late{suffix}.txt"));
        std::fs::hard_link(source, &alias).unwrap();
        let response =
            super::read::handle_read(&request("read", json!({"file": alias})), &fixture.ctx);
        assert_eq!(
            response.data["code"], "denied_floor",
            "{suffix}: {response:?}"
        );
    }
}

#[test]
fn storage_floor_ordinary_project_hard_links_still_work() {
    let f = Fixture::new();
    let alias = f._root.path().join("ordinary-alias.txt");
    std::fs::hard_link(&f.neighbor, &alias).unwrap();
    let read = super::read::handle_read(&request("read", json!({"file": alias})), &f.ctx);
    assert!(read.success, "{read:?}");
    let write = super::write::handle_write(
        &request("write", json!({"file": alias, "content": "normal"})),
        &f.ctx,
    );
    assert!(write.success, "{write:?}");
    std::fs::remove_file(&alias).unwrap();
    std::fs::write(&f.neighbor, "normal").unwrap();
    std::fs::hard_link(&f.neighbor, &alias).unwrap();
    let edit = super::edit_match::handle_edit_match(
        &request(
            "edit_match",
            json!({"file": alias, "match": "normal", "replacement": "changed"}),
        ),
        &f.ctx,
    );
    assert!(edit.success, "{edit:?}");
    std::fs::remove_file(&alias).unwrap();
    std::fs::write(&f.neighbor, "changed").unwrap();
    std::fs::hard_link(&f.neighbor, &alias).unwrap();
    let mut snapshots = crate::backup::BackupStore::new();
    assert!(snapshots
        .snapshot("ordinary", &alias, "normal")
        .unwrap()
        .is_some());
    std::fs::write(&alias, "later").unwrap();
    snapshots.restore_latest("ordinary", &alias).unwrap();
    assert_eq!(std::fs::read_to_string(&alias).unwrap(), "changed");
}

#[test]
fn storage_floor_identity_check_stats_ordinary_file_once() {
    let f = Fixture::new();
    let stats = crate::db::file_identity::stat_probe::count(|| {
        assert!(!crate::db::file_identity::is_live_sqlite_file(&f.neighbor));
    });
    assert_eq!(stats, std::collections::BTreeMap::from([(f.neighbor, 1)]));
}

#[test]
fn storage_floor_glob_edit_match_skips_store_files_before_reading() {
    let mut f = Fixture::new();
    f.ctx = f.ctx.with_test_checkpoint_store();
    let nonmatching = f.store.with_file_name("metadata.txt");
    std::fs::write(&nonmatching, "unrelated storage metadata\n").unwrap();
    let protected = [&f.store, &nonmatching];
    let before = protected
        .iter()
        .map(|path| crate::file_identity::metadata(path).unwrap())
        .collect::<Vec<_>>();
    crate::backup::reset_capture_read_count(&f.store);
    crate::backup::reset_capture_read_count(&nonmatching);
    let response = super::edit_match::handle_edit_match(
        &request(
            "edit_match",
            json!({"file": "**/*", "match": "original", "replacement": "changed"}),
        ),
        &f.ctx,
    );
    assert!(response.success, "{response:?}");
    assert_eq!(std::fs::read_to_string(&f.neighbor).unwrap(), "changed\n");
    assert_eq!(response.data["total_files"], 1);
    assert_eq!(response.data["complete"], false);
    let rendered = crate::subc_format::format_response("edit", &response, false);
    assert!(
        rendered.contains("metadata.txt: denied_floor"),
        "{rendered}"
    );
    for (path, before) in protected.iter().zip(before) {
        assert!(has_floor_skip(&response, path), "{response:?}");
        let after = crate::file_identity::metadata(path).unwrap();
        assert_eq!(before.1, after.1);
        assert_eq!(before.0.modified().unwrap(), after.0.modified().unwrap());
        assert_eq!(crate::backup::capture_read_count(path), 0);
    }
}

#[test]
fn storage_floor_glob_edit_match_reports_skips_on_preview_and_no_match() {
    let f = Fixture::new();
    for params in [
        json!({"file": "**/*", "match": "original", "replacement": "changed", "preview": true}),
        json!({"file": "**/*", "match": "original", "replacement": "original"}),
        json!({"file": "**/*", "match": "not present anywhere", "replacement": "changed"}),
    ] {
        let response = super::edit_match::handle_edit_match(&request("edit_match", params), &f.ctx);
        assert_ne!(response.data["code"], "denied_floor", "{response:?}");
        assert!(response.data["skipped_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["reason"] == "denied_floor"));
        assert!(
            crate::subc_format::format_response("edit", &response, false)
                .contains("aft.db: denied_floor")
        );
        assert_eq!(std::fs::read_to_string(&f.neighbor).unwrap(), "original\n");
    }
}

#[test]
fn storage_floor_outline_lists_skip_protected_leaves_without_opening_them() {
    let f = Fixture::new();
    let ordinary = f._root.path().join("allowed.rs");
    let protected = f.store.with_file_name("secret.rs");
    std::fs::write(&ordinary, "pub fn allowed() {}\n").unwrap();
    std::fs::write(&protected, "pub fn secret() {}\n").unwrap();
    for params in [
        json!({"directory": f._root.path()}),
        json!({"target": f._root.path(), "files": true}),
        json!({"files": [protected, ordinary]}),
    ] {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            (*f.ctx.config()).clone(),
        );
        #[cfg(unix)]
        let ordinary_opens = crate::bounded_io::tests::watch_opens(&ordinary);
        #[cfg(unix)]
        let protected_opens = crate::bounded_io::tests::watch_opens(&protected);
        let response = super::outline::handle_outline(&request("outline", params), &ctx);
        #[cfg(unix)]
        {
            use std::sync::atomic::Ordering;
            assert_eq!(
                protected_opens.opens.load(Ordering::Relaxed),
                0,
                "protected leaf opened"
            );
            assert!(
                ordinary_opens.opens.load(Ordering::Relaxed) > 0,
                "ordinary-file open control"
            );
        }
        assert!(response.success, "{response:?}");
        assert!(
            response.data["text"].as_str().unwrap().contains("allowed"),
            "{response:?}"
        );
        assert_eq!(response.data["complete"], false);
        assert!(
            response.data["skipped_files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["reason"] == "denied_floor"
                    && item["file"].as_str().unwrap().ends_with("secret.rs")),
            "{response:?}"
        );
    }
    let explicit =
        super::outline::handle_outline(&request("outline", json!({"file": protected})), &f.ctx);
    assert_eq!(explicit.data["code"], "denied_floor");
}

#[test]
fn storage_floor_revision_zoom_batch_keeps_floor_refusal_target_local() {
    let f = Fixture::new();
    let ordinary = f._root.path().join("allowed.rs");
    let protected = f.store.with_file_name("secret.rs");
    std::fs::write(&ordinary, "pub fn allowed() {}\n").unwrap();
    std::fs::write(&protected, "pub fn secret() {}\n").unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        let mut command = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(f._root.path()));
        let output = command.args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut config = (*f.ctx.config()).clone();
    config.revision_reads = true;
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let response = super::zoom::handle_zoom(
        &request(
            "zoom",
            json!({"ref": "HEAD", "targets": [
                {"file": protected, "symbols": "secret"}, {"file": ordinary, "symbols": "allowed"}
            ]}),
        ),
        &ctx,
    );
    assert!(response.success, "{response:?}");
    assert_eq!(
        response.data["targets"][0]["response"]["code"],
        "denied_floor"
    );
    assert_eq!(
        response.data["targets"][1]["response"]["success"], true,
        "{response:?}"
    );
}

#[test]
fn storage_floor_grep_and_glob_report_walk_exclusions_without_aborting() {
    let f = Fixture::new();
    let unrelated_db = f._root.path().join("ordinary.db");
    std::fs::write(&unrelated_db, "original\n").unwrap();
    for response in [
        super::grep::handle_grep(&request("grep", json!({"pattern": "original"})), &f.ctx),
        super::glob::handle_glob(&request("glob", json!({"pattern": "**/*"})), &f.ctx),
    ] {
        assert!(response.success, "{response:?}");
        assert!(
            has_floor_skip(&response, f.store.parent().unwrap()),
            "{response:?}"
        );
        assert!(!has_floor_skip(&response, &unrelated_db));
        assert_eq!(response.data["complete"], false);
        assert!(
            response.data["text"]
                .as_str()
                .unwrap()
                .contains("neighbor.txt"),
            "{response:?}"
        );
    }
}

#[test]
fn storage_floor_grep_and_glob_report_new_live_alias_in_cached_index() {
    let f = Fixture::new();
    let alias = f._root.path().join("stale.txt");
    std::fs::write(&alias, "original\n").unwrap();
    let index = crate::search_index::SearchIndex::build(f._root.path());
    assert!(index.is_ready());
    assert!(index.snapshot().has_file_in_scope(&alias));
    *f.ctx.search_index().write().unwrap() = Some(index);
    std::fs::remove_file(&f.store).unwrap();
    let connection = rusqlite::Connection::open(&f.store).unwrap();
    connection
        .execute_batch("CREATE TABLE source(value TEXT);")
        .unwrap();
    let _connection =
        crate::db::file_identity::IdentityConnection::new(connection, "cached alias list fixture");
    std::fs::remove_file(&alias).unwrap();
    std::fs::hard_link(&f.store, &alias).unwrap();
    #[cfg(unix)]
    let alias_opens = crate::bounded_io::tests::watch_opens(&alias);
    for response in [
        super::grep::handle_grep(&request("grep", json!({"pattern": "original"})), &f.ctx),
        super::glob::handle_glob(&request("glob", json!({"pattern": "**/*"})), &f.ctx),
    ] {
        #[cfg(unix)]
        assert_eq!(
            alias_opens.opens.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "indexed live alias opened"
        );
        assert!(response.success, "{response:?}");
        assert!(has_floor_skip(&response, &alias), "{response:?}");
        assert_eq!(response.data["complete"], false);
    }
}

#[test]
fn storage_floor_list_skip_disclosure_keeps_cut_trailer_last() {
    let f = Fixture::new();
    for index in 0..120 {
        std::fs::write(
            f._root.path().join(format!("entry{index:03}.txt")),
            "original\n",
        )
        .unwrap();
    }
    for (tool, envelope_key, response) in [
        (
            "grep",
            "matches_list_envelope",
            super::grep::handle_grep(&request("grep", json!({"pattern": "original"})), &f.ctx),
        ),
        (
            "glob",
            "files_list_envelope",
            super::glob::handle_glob(&request("glob", json!({"pattern": "**/*"})), &f.ctx),
        ),
    ] {
        assert!(response.success, "{response:?}");
        assert!(has_floor_skip(&response, f.store.parent().unwrap()));
        let envelope = serde_json::from_value(response.data[envelope_key].clone()).unwrap();
        let trailer =
            crate::subc_format::render_envelope_trailer(&envelope).expect("cut list trailer");
        let rendered = crate::subc_format::format_response(tool, &response, false);
        assert!(rendered.contains("denied_floor"), "{rendered}");
        assert!(rendered.ends_with(&trailer), "{rendered}");
    }
}

#[test]
fn storage_floor_subdirectory_glob_reports_relative_pattern_omissions() {
    for cached in [false, true] {
        let f = Fixture::new();
        let src = f._root.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let alias = src.join("a.rs");
        std::fs::write(&alias, "original\n").unwrap();
        if cached {
            let index = crate::search_index::SearchIndex::build(f._root.path());
            assert!(index.is_ready());
            *f.ctx.search_index().write().unwrap() = Some(index);
        }
        std::fs::remove_file(&f.store).unwrap();
        let connection = rusqlite::Connection::open(&f.store).unwrap();
        connection
            .execute_batch("CREATE TABLE source(value TEXT);")
            .unwrap();
        let _connection = crate::db::file_identity::IdentityConnection::new(
            connection,
            "subdirectory omission fixture",
        );
        std::fs::remove_file(&alias).unwrap();
        std::fs::hard_link(&f.store, &alias).unwrap();
        let glob = super::glob::handle_glob(
            &request("glob", json!({"path": "src", "pattern": "a.rs"})),
            &f.ctx,
        );
        assert!(glob.success, "cached={cached}: {glob:?}");
        assert!(has_floor_skip(&glob, &alias), "cached={cached}: {glob:?}");
        assert_eq!(glob.data["complete"], false);
        let grep = super::grep::handle_grep(
            &request(
                "grep",
                json!({"path": "src", "pattern": "original", "include": ["src/a.rs"]}),
            ),
            &f.ctx,
        );
        assert!(grep.success, "cached={cached}: {grep:?}");
        assert!(has_floor_skip(&grep, &alias), "cached={cached}: {grep:?}");
        assert_eq!(grep.data["complete"], false);
    }
}

#[test]
fn storage_floor_external_grep_reports_search_root_relative_includes() {
    let f = Fixture::new();
    let external = tempfile::tempdir().unwrap();
    let alias = external.path().join("a.rs");
    std::fs::remove_file(&f.store).unwrap();
    let connection = rusqlite::Connection::open(&f.store).unwrap();
    connection
        .execute_batch("CREATE TABLE source(value TEXT);")
        .unwrap();
    let _connection =
        crate::db::file_identity::IdentityConnection::new(connection, "external omission fixture");
    std::fs::hard_link(&f.store, &alias).unwrap();
    let response = super::grep::handle_grep(
        &request(
            "grep",
            json!({"path": external.path(), "pattern": "original", "include": ["a.rs"]}),
        ),
        &f.ctx,
    );
    assert!(response.success, "{response:?}");
    assert!(has_floor_skip(&response, &alias), "{response:?}");
    assert_eq!(response.data["complete"], false);
}
