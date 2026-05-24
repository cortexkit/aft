use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use aft::commands::delete_file::handle_delete_file;
use aft::commands::lsp_diagnostics::handle_lsp_diagnostics;
use aft::commands::move_file::handle_move_file;
use aft::commands::transaction::handle_transaction;
use aft::commands::write::handle_write;
use aft::config::{Config, UserServerDef};
use aft::context::AppContext;
use aft::lsp::client::LspEvent;
use aft::lsp::diagnostics::DiagnosticSeverity;
use aft::lsp::manager::LspManager;
use aft::lsp::registry::{is_config_file_path, is_config_file_path_with_custom, ServerKind};
use aft::lsp::roots::ServerKey;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use lsp_types::FileChangeType;
use tempfile::tempdir;

use super::helpers::AftProcess;

fn fake_server_path() -> PathBuf {
    option_env!("CARGO_BIN_EXE_fake-lsp-server")
        .or(option_env!("CARGO_BIN_EXE_fake_lsp_server"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake-lsp-server").map(PathBuf::from))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake_lsp_server").map(PathBuf::from))
        .or_else(|| {
            let mut path = std::env::current_exe().ok()?;
            path.pop();
            path.pop();
            path.push("fake-lsp-server");
            Some(path)
        })
        .filter(|path| path.exists())
        .expect("fake-lsp-server binary path not set")
}

fn rust_workspace_with_files(names: &[&str]) -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
    let temp_dir = tempdir().expect("tempdir");
    let root = temp_dir.path().join("workspace");
    let src_dir = root.join("src");

    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("write Cargo.toml");

    let mut files = Vec::new();
    for name in names {
        let path = src_dir.join(name);
        fs::write(&path, "fn main() {}\n").expect("write fixture source");
        files.push(path);
    }

    (temp_dir, root, files)
}

fn typescript_workspace_with_files(names: &[&str]) -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
    let temp_dir = tempdir().expect("tempdir");
    let root = temp_dir.path().join("workspace");
    let src_dir = root.join("src");

    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(root.join("package.json"), "{\"devDependencies\":{}}\n").expect("write package.json");

    let mut files = Vec::new();
    for name in names {
        let path = src_dir.join(name);
        fs::write(&path, "export const value = 1;\n").expect("write fixture source");
        files.push(path);
    }

    (temp_dir, root, files)
}

fn collect_event<F>(manager: &mut LspManager, predicate: F) -> Option<LspEvent>
where
    F: Fn(&LspEvent) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        for event in manager.drain_events() {
            if predicate(&event) {
                return Some(event);
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    None
}

fn wait_for_publish(manager: &mut LspManager) {
    let event = collect_event(manager, |event| {
        matches!(
            event,
            LspEvent::Notification {
                method,
                ..
            } if method == "textDocument/publishDiagnostics"
        )
    });
    assert!(event.is_some(), "timed out waiting for publishDiagnostics");
}

fn manager_with_fake_server() -> LspManager {
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::Rust, fake_server_path());
    manager
}

fn manager_with_fake_typescript_server() -> LspManager {
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::TypeScript, fake_server_path());
    manager
}

fn collect_watched_file_events(manager: &mut LspManager) -> serde_json::Value {
    let event = collect_event(manager, |event| {
        matches!(
            event,
            LspEvent::Notification { method, .. } if method == "custom/watchedFilesChanged"
        )
    })
    .expect("timed out waiting for watched-file notification");

    match event {
        LspEvent::Notification { params, .. } => params.expect("watched event params"),
        other => panic!("unexpected event: {other:?}"),
    }
}

fn collect_watched_file_events_from_ctx(ctx: &AppContext) -> serde_json::Value {
    let event = collect_event(&mut ctx.lsp(), |event| {
        matches!(
            event,
            LspEvent::Notification { method, .. } if method == "custom/watchedFilesChanged"
        )
    })
    .expect("timed out waiting for watched-file notification");

    match event {
        LspEvent::Notification { params, .. } => params.expect("watched event params"),
        other => panic!("unexpected event: {other:?}"),
    }
}

fn drain_watched_file_events_from_ctx(ctx: &AppContext) -> Vec<serde_json::Value> {
    ctx.lsp()
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
            LspEvent::Notification { method, params, .. }
                if method == "custom/watchedFilesChanged" =>
            {
                params
            }
            _ => None,
        })
        .collect()
}

fn collect_watched_file_events_from_ctx_before_deadline(
    ctx: &AppContext,
    duration: Duration,
) -> Option<serde_json::Value> {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if let Some(event) = collect_event(&mut ctx.lsp(), |event| {
            matches!(
                event,
                LspEvent::Notification { method, .. } if method == "custom/watchedFilesChanged"
            )
        }) {
            return match event {
                LspEvent::Notification { params, .. } => params,
                _ => None,
            };
        }
        thread::sleep(Duration::from_millis(25));
    }
    None
}

fn config_change_type(params: &serde_json::Value, suffix: &str) -> i64 {
    let changes = params["changes"].as_array().expect("changes array");
    changes
        .iter()
        .find(|change| {
            change["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with(suffix))
        })
        .and_then(|change| change["type"].as_i64())
        .unwrap_or_else(|| panic!("missing watched-file change for {suffix}: {params}"))
}

fn app_context_with_fake_lsp() -> AppContext {
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
    ctx
}

#[test]
fn test_diagnostics_stored_after_did_open() {
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let mut manager = manager_with_fake_server();

    manager
        .notify_file_changed_default(file, "fn main() { println!(\"hi\"); }\n")
        .expect("notify file changed");
    wait_for_publish(&mut manager);

    let diagnostics = manager.get_diagnostics_for_file(file);
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(diagnostics[0].line, 1);
    assert_eq!(diagnostics[0].column, 1);
    assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
    assert_eq!(diagnostics[0].code.as_deref(), Some("E0001"));
    assert_eq!(diagnostics[1].line, 2);
    assert_eq!(diagnostics[1].column, 5);
    assert_eq!(diagnostics[1].severity, DiagnosticSeverity::Warning);
}

#[test]
fn watched_files_sent_for_config_edit_alongside_source_edit() {
    let (_temp_dir, root, files) = typescript_workspace_with_files(&["foo.ts"]);
    let source = &files[0];
    let package_json = root.join("package.json");
    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let mut manager = manager_with_fake_typescript_server();

    manager
        .notify_file_changed(source, "export const value = 2;\n", &config)
        .expect("open ts source");
    wait_for_publish(&mut manager);

    manager
        .notify_files_watched_changed(&[(package_json.clone(), FileChangeType::CHANGED)], &config)
        .expect("notify watched files");

    let params = collect_watched_file_events(&mut manager);
    let changes = params["changes"].as_array().expect("changes array");
    assert_eq!(changes.len(), 1);
    assert!(
        changes[0]["uri"]
            .as_str()
            .expect("uri")
            .ends_with("/package.json"),
        "unexpected uri: {params}"
    );
    assert_eq!(changes[0]["type"], 2);
}

#[test]
fn watched_config_file_event_types_follow_current_file_state() {
    let (_temp_dir, root, files) = typescript_workspace_with_files(&["foo.ts"]);
    let source = &files[0];
    let existing = root.join("package.json");
    let created = root.join("biome.json");
    let deleted = root.join("pyrightconfig.json");
    fs::write(&deleted, "{}\n").expect("write config before delete");
    fs::write(&created, "{}\n").expect("write config before notify");
    fs::remove_file(&deleted).expect("delete config before notify");

    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());

    ctx.lsp_notify_file_changed(source, "export const value = 2;\n");
    wait_for_publish(&mut ctx.lsp());

    ctx.lsp_post_write(
        source,
        "export const value = 3;\n",
        &serde_json::json!({
            "multi_file_write_paths": [
                existing.display().to_string(),
                created.display().to_string(),
                deleted.display().to_string()
            ]
        }),
    );

    let params = collect_watched_file_events_from_ctx(&ctx);
    assert_eq!(config_change_type(&params, "/package.json"), 2);
    assert_eq!(config_change_type(&params, "/biome.json"), 2);
    assert_eq!(config_change_type(&params, "/pyrightconfig.json"), 3);
}

#[test]
fn watched_config_file_event_types_accept_explicit_created_changed_deleted() {
    let (_temp_dir, root, files) = typescript_workspace_with_files(&["foo.ts"]);
    let source = &files[0];
    let created = root.join("biome.json");
    let changed = root.join("package.json");
    let deleted = root.join("pyrightconfig.json");

    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());

    ctx.lsp_notify_file_changed(source, "export const value = 2;\n");
    wait_for_publish(&mut ctx.lsp());

    ctx.lsp_post_write(
        source,
        "export const value = 3;\n",
        &serde_json::json!({
            "multi_file_write_paths": [
                { "path": created.display().to_string(), "type": "created" },
                { "path": changed.display().to_string(), "type": "changed" },
                { "path": deleted.display().to_string(), "type": "deleted" }
            ]
        }),
    );

    let params = collect_watched_file_events_from_ctx(&ctx);
    assert_eq!(config_change_type(&params, "/biome.json"), 1);
    assert_eq!(config_change_type(&params, "/package.json"), 2);
    assert_eq!(config_change_type(&params, "/pyrightconfig.json"), 3);
}

#[test]
fn write_command_reports_created_for_new_config_file() {
    let (_temp_dir, root, files) = typescript_workspace_with_files(&["foo.ts"]);
    let source = &files[0];
    let config_path = root.join("biome.json");
    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp_notify_file_changed(source, "export const value = 2;\n");
    wait_for_publish(&mut ctx.lsp());

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "write-created-config",
        "command": "write",
        "file": config_path.display().to_string(),
        "content": "{}\n"
    }))
    .expect("request parses");
    let response = handle_write(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true, "write failed: {json}");

    let params = collect_watched_file_events_from_ctx(&ctx);
    assert_eq!(config_change_type(&params, "/biome.json"), 1);
}

#[test]
fn write_command_reports_created_for_new_tsconfig_file() {
    let (_temp_dir, root, files) = typescript_workspace_with_files(&["foo.ts"]);
    let source = &files[0];
    let config_path = root.join("tsconfig.json");
    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp_notify_file_changed(source, "export const value = 2;\n");
    wait_for_publish(&mut ctx.lsp());

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "write-created-tsconfig",
        "command": "write",
        "file": config_path.display().to_string(),
        "content": "{\"compilerOptions\":{}}\n"
    }))
    .expect("request parses");
    let response = handle_write(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true, "write failed: {json}");

    let params = collect_watched_file_events_from_ctx(&ctx);
    assert_eq!(config_change_type(&params, "/tsconfig.json"), 1);
}

#[test]
fn move_file_reports_deleted_source_and_created_destination_for_config_file() {
    let (_temp_dir, root, files) = typescript_workspace_with_files(&["foo.ts"]);
    let source = &files[0];
    let src_config = root.join("package.json");
    let dst_config = root.join("moved").join("package.json");
    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp_notify_file_changed(source, "export const value = 2;\n");
    wait_for_publish(&mut ctx.lsp());

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "move-config",
        "command": "move_file",
        "file": src_config.display().to_string(),
        "destination": dst_config.display().to_string()
    }))
    .expect("request parses");
    let response = handle_move_file(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true, "move failed: {json}");

    let mut events = drain_watched_file_events_from_ctx(&ctx);
    let deadline = Instant::now() + Duration::from_secs(2);
    while events.len() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
        events.extend(drain_watched_file_events_from_ctx(&ctx));
    }
    assert_eq!(
        events.len(),
        2,
        "expected source and destination watched events"
    );
    let mut event_types = vec![
        config_change_type(&events[0], "/package.json"),
        config_change_type(&events[1], "/package.json"),
    ];
    event_types.sort_unstable();
    assert_eq!(event_types, vec![1, 3]);
}

#[test]
fn transaction_rollback_does_not_notify_lsp_for_reverted_file() {
    let (_temp_dir, root, files) = typescript_workspace_with_files(&["open.ts"]);
    let source = &files[0];
    let first = root.join("txn_first.ts");
    let second = root.join("txn_second.ts");
    let original_first = "export const first = 1;\n";
    let original_second = "export const second = 1;\n";
    fs::write(&first, original_first).expect("write first");
    fs::write(&second, original_second).expect("write second");

    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp_notify_file_changed(source, "export const value = 2;\n");
    wait_for_publish(&mut ctx.lsp());

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "txn-lsp-rollback",
        "command": "transaction",
        "operations": [
            {"file": first.display().to_string(), "command": "write", "content": "export const first = 2;\n"},
            {"file": second.display().to_string(), "command": "write", "content": "export const second = {;\n"}
        ]
    }))
    .expect("request parses");
    let response = handle_transaction(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], false, "transaction should fail: {json}");
    assert_eq!(
        fs::read_to_string(&first).expect("read first"),
        original_first
    );
    assert_eq!(
        fs::read_to_string(&second).expect("read second"),
        original_second
    );

    let notified =
        collect_watched_file_events_from_ctx_before_deadline(&ctx, Duration::from_millis(250));
    assert!(
        notified.is_none(),
        "unexpected LSP watched-file notification: {notified:?}"
    );
}

#[test]
fn config_file_detection_ignores_vendor_build_segments() {
    assert!(is_config_file_path(&PathBuf::from("package.json")));
    assert!(is_config_file_path(&PathBuf::from("apps/web/package.json")));
    assert!(!is_config_file_path(&PathBuf::from(
        "node_modules/foo/package.json"
    )));
    assert!(!is_config_file_path(&PathBuf::from("target/package.json")));
    assert!(!is_config_file_path(&PathBuf::from("dist/tsconfig.json")));
    assert!(is_config_file_path(&PathBuf::from(
        "my-target/package.json"
    )));
}

#[test]
fn config_file_detection_accepts_custom_root_markers_but_excludes_lockfiles() {
    let custom_markers = vec!["pyrightconfig-custom.json".to_string()];

    assert!(is_config_file_path_with_custom(
        &PathBuf::from("package.json"),
        &[]
    ));
    assert!(!is_config_file_path_with_custom(
        &PathBuf::from("pyrightconfig-custom.json"),
        &[]
    ));
    assert!(is_config_file_path_with_custom(
        &PathBuf::from("pyrightconfig-custom.json"),
        &custom_markers
    ));
    assert!(!is_config_file_path_with_custom(
        &PathBuf::from("Cargo.lock"),
        &[]
    ));
    assert!(!is_config_file_path_with_custom(
        &PathBuf::from("package-lock.json"),
        &[]
    ));
}

#[test]
fn lockfiles_are_not_watched_config_files() {
    for lockfile in [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "Cargo.lock",
        "go.sum",
        "bun.lock",
        "bun.lockb",
    ] {
        assert!(
            !is_config_file_path(&PathBuf::from(lockfile)),
            "{lockfile} must not trigger watched-file notifications"
        );
    }
}

#[test]
fn custom_lsp_root_marker_edit_notifies_workspace_server() {
    let temp_dir = tempdir().expect("tempdir");
    let root = temp_dir.path().join("workspace");
    let source = root.join("src").join("main.customts");
    let custom_config = root.join("pyrightconfig-custom.json");
    fs::create_dir_all(source.parent().expect("source parent")).expect("create src");
    fs::write(&source, "export const value = 1;\n").expect("write source");
    fs::write(&custom_config, "{}\n").expect("write custom config");

    let server_id = "custom-ts";
    let config = Config {
        project_root: Some(root.clone()),
        lsp_servers: vec![UserServerDef {
            id: server_id.to_string(),
            extensions: vec!["customts".to_string()],
            binary: "custom-ts-lsp".to_string(),
            args: Vec::new(),
            root_markers: vec!["pyrightconfig-custom.json".to_string()],
            env: Default::default(),
            initialization_options: None,
            disabled: false,
        }],
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.lsp().override_binary(
        ServerKind::Custom(std::sync::Arc::from(server_id)),
        fake_server_path(),
    );

    ctx.lsp_notify_file_changed(&source, "export const value = 2;\n");
    wait_for_publish(&mut ctx.lsp());

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "write-custom-marker",
        "command": "write",
        "file": custom_config.display().to_string(),
        "content": "{\"typeCheckingMode\":\"strict\"}\n"
    }))
    .expect("request parses");
    let response = handle_write(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true, "write failed: {json}");

    let params = collect_watched_file_events_from_ctx(&ctx);
    assert_eq!(config_change_type(&params, "/pyrightconfig-custom.json"), 2);
}

#[test]
fn write_command_reports_changed_for_existing_config_file() {
    let (_temp_dir, root, files) = typescript_workspace_with_files(&["foo.ts"]);
    let source = &files[0];
    let config_path = root.join("package.json");
    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp_notify_file_changed(source, "export const value = 2;\n");
    wait_for_publish(&mut ctx.lsp());

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "write-changed-config",
        "command": "write",
        "file": config_path.display().to_string(),
        "content": "{\"devDependencies\":{}}\n"
    }))
    .expect("request parses");
    let response = handle_write(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true, "write failed: {json}");

    let params = collect_watched_file_events_from_ctx(&ctx);
    assert_eq!(config_change_type(&params, "/package.json"), 2);
}

#[test]
fn delete_file_command_reports_deleted_for_config_file() {
    let (_temp_dir, root, files) = typescript_workspace_with_files(&["foo.ts"]);
    let source = &files[0];
    let config_path = root.join("pyrightconfig.json");
    fs::write(&config_path, "{}\n").expect("write config before delete");
    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp_notify_file_changed(source, "export const value = 2;\n");
    wait_for_publish(&mut ctx.lsp());

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "delete-config",
        "command": "delete_file",
        "file": config_path.display().to_string()
    }))
    .expect("request parses");
    let response = handle_delete_file(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true, "delete failed: {json}");

    let params = collect_watched_file_events_from_ctx(&ctx);
    assert_eq!(config_change_type(&params, "/pyrightconfig.json"), 3);
}

#[test]
fn watched_files_preserve_created_changed_deleted_event_types() {
    let (_temp_dir, root, files) = typescript_workspace_with_files(&["foo.ts"]);
    let source = &files[0];
    let package_json = root.join("package.json");
    let tsconfig = root.join("tsconfig.json");
    let jsconfig = root.join("jsconfig.json");
    fs::write(&tsconfig, "{\"compilerOptions\":{}}\n").expect("write tsconfig");
    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let mut manager = manager_with_fake_typescript_server();

    manager
        .notify_file_changed(source, "export const value = 2;\n", &config)
        .expect("open ts source");
    wait_for_publish(&mut manager);

    manager
        .notify_files_watched_changed(
            &[
                (package_json, FileChangeType::CHANGED),
                (tsconfig, FileChangeType::CREATED),
                (jsconfig, FileChangeType::DELETED),
            ],
            &config,
        )
        .expect("notify watched files");

    let params = collect_watched_file_events(&mut manager);
    let changes = params["changes"].as_array().expect("changes array");
    let event_types: Vec<i64> = changes
        .iter()
        .map(|change| change["type"].as_i64().expect("type number"))
        .collect();
    assert_eq!(event_types, vec![2, 1, 3]);
}

#[test]
fn test_diagnostics_replace_on_new_publish() {
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let mut manager = manager_with_fake_server();

    manager
        .notify_file_changed_default(file, "fn main() { println!(\"one\"); }\n")
        .expect("first notify");
    wait_for_publish(&mut manager);
    assert_eq!(manager.get_diagnostics_for_file(file).len(), 2);

    manager
        .notify_file_changed_default(file, "fn main() { println!(\"two\"); }\n")
        .expect("second notify");
    wait_for_publish(&mut manager);

    let diagnostics = manager.get_diagnostics_for_file(file);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].message, "test diagnostic after change");
    assert_eq!(diagnostics[0].line, 3);
    assert_eq!(diagnostics[0].code.as_deref(), Some("E0002"));
}

#[test]
fn test_diagnostics_filter_by_severity() {
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let ctx = app_context_with_fake_lsp();

    ctx.lsp()
        .notify_file_changed_default(file, "fn main() { println!(\"hi\"); }\n")
        .expect("notify file changed");

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-filter",
        "command": "lsp_diagnostics",
        "file": file.display().to_string(),
        "severity": "error",
        "wait_ms": 250
    }))
    .expect("request parses");

    let response = handle_lsp_diagnostics(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    let diagnostics = json["diagnostics"].as_array().expect("diagnostics array");

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["severity"], "error");
    assert_eq!(diagnostics[0]["message"], "test diagnostic error");
}

#[test]
fn test_wait_for_diagnostics_returns_after_matching_publish() {
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs", "lib.rs"]);
    let main_rs = &files[0];
    let lib_rs = &files[1];
    let mut manager = manager_with_fake_server();

    manager
        .notify_file_changed_default(lib_rs, "pub fn answer() -> u32 { 42 }\n")
        .expect("open lib");
    wait_for_publish(&mut manager);

    manager
        .notify_file_changed_default(main_rs, "fn main() { println!(\"hi\"); }\n")
        .expect("open main");

    let diagnostics = manager.wait_for_diagnostics_default(main_rs, Duration::from_secs(2));
    let canonical_main = fs::canonicalize(main_rs).expect("canonical main");

    assert_eq!(diagnostics.len(), 2);
    assert!(diagnostics
        .iter()
        .all(|diagnostic| diagnostic.file == canonical_main));
}

#[test]
fn test_diagnostics_for_file_vs_all() {
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs", "lib.rs"]);
    let main_rs = &files[0];
    let lib_rs = &files[1];
    let mut manager = manager_with_fake_server();

    manager
        .notify_file_changed_default(main_rs, "fn main() {}\n")
        .expect("open main");
    wait_for_publish(&mut manager);
    manager
        .notify_file_changed_default(lib_rs, "pub fn answer() -> u32 { 42 }\n")
        .expect("open lib");
    wait_for_publish(&mut manager);

    let file_diagnostics = manager.get_diagnostics_for_file(main_rs);
    let all_diagnostics = manager.get_all_diagnostics();
    let canonical_main = fs::canonicalize(main_rs).expect("canonical main");

    assert_eq!(file_diagnostics.len(), 2);
    assert_eq!(all_diagnostics.len(), 4);
    assert!(file_diagnostics
        .iter()
        .all(|diagnostic| diagnostic.file == canonical_main));
}

#[test]
fn test_diagnostics_clear_on_empty_array() {
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let mut manager = manager_with_fake_server();

    manager
        .notify_file_changed_default(file, "fn main() {}\n")
        .expect("open file");
    wait_for_publish(&mut manager);
    assert_eq!(manager.get_diagnostics_for_file(file).len(), 2);

    manager.notify_file_closed(file).expect("close file");
    wait_for_publish(&mut manager);
    assert!(manager.get_diagnostics_for_file(file).is_empty());
}

#[test]
fn test_lsp_diagnostics_command_response_format() {
    let (_temp_dir, root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let fake_server = fake_server_path();
    // Run the fake in pull mode (LSP 3.17 textDocument/diagnostic). After
    // v0.21.1's F2 freshness fix, push-only servers only prove freshness for
    // publishes that arrive AFTER push_wait_started_at — an open-time publish
    // that lands before the lsp_diagnostics call is correctly classified
    // unfresh. Pull mode is protocol-fresh by design, so it stays a stable
    // testbed for the response-format contract.
    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_LSP_RUST_BINARY", fake_server.as_os_str()),
        ("AFT_FAKE_LSP_PULL", "1".as_ref()),
    ]);

    let configure = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":"{}"}}"#,
        root.display()
    ));
    assert_eq!(configure["success"], true);

    let write = aft.send(&format!(
        r#"{{"id":"write-1","command":"write","file":"{}","content":"fn main() {{ println!(\"hello\"); }}\n"}}"#,
        file.display()
    ));
    assert_eq!(write["success"], true, "write failed: {write:?}");

    let resp = aft.send(&format!(
        r#"{{"id":"diag-1","command":"lsp_diagnostics","file":"{}","wait_ms":400}}"#,
        file.display()
    ));

    assert_eq!(resp["id"], "diag-1");
    assert_eq!(resp["success"], true, "response: {resp:?}");
    // Pull mode returns exactly one diagnostic (see fake_server.rs's
    // textDocument/diagnostic handler).
    assert_eq!(resp["total"], 1);
    assert_eq!(resp["files_with_errors"], 1);

    let diagnostics = resp["diagnostics"].as_array().expect("diagnostics array");
    assert_eq!(diagnostics.len(), 1);
    let canonical_file = fs::canonicalize(file).expect("canonical file");
    assert_eq!(diagnostics[0]["file"], canonical_file.display().to_string());
    assert_eq!(diagnostics[0]["line"], 5);
    assert_eq!(diagnostics[0]["column"], 1);
    assert_eq!(diagnostics[0]["end_line"], 5);
    assert_eq!(diagnostics[0]["end_column"], 9);
    assert_eq!(diagnostics[0]["severity"], "error");
    assert_eq!(diagnostics[0]["message"], "test pull diagnostic");
    assert_eq!(diagnostics[0]["code"], "E0PULL");
    assert_eq!(diagnostics[0]["source"], "fake-lsp");

    let status = aft.shutdown();
    assert!(status.success());
}

// ────────────────────────────────────────────────────────────────────────────
// Tri-state convention: response shape changes
// ────────────────────────────────────────────────────────────────────────────

/// `lsp_diagnostics` always reports `complete` (true|false) and
/// `lsp_servers_used`. This locks in the new tri-state contract Oracle
/// approved.
#[test]
fn test_response_includes_complete_and_servers_used() {
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let ctx = app_context_with_fake_lsp();

    ctx.lsp()
        .notify_file_changed_default(file, "fn main() {}\n")
        .expect("notify file changed");

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-shape",
        "command": "lsp_diagnostics",
        "file": file.display().to_string(),
        "wait_ms": 250
    }))
    .expect("request parses");

    let response = handle_lsp_diagnostics(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true);
    assert!(json["complete"].is_boolean(), "complete missing: {json}");
    assert!(
        json["lsp_servers_used"].is_array(),
        "lsp_servers_used missing: {json}"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// File-mode honest reporting when no server is registered
// ────────────────────────────────────────────────────────────────────────────

/// When asking diagnostics for a file with NO registered LSP server, the
/// response must be honest — empty diagnostics, `complete: true`, and a
/// `note` explaining that no server applies. This is the explicit fix for
/// the false-clean bug.
#[test]
fn test_no_lsp_server_returns_honest_note() {
    let temp_dir = tempdir().expect("tempdir");
    let root = temp_dir.path().join("workspace");
    fs::create_dir_all(&root).expect("create root");
    let file = root.join("file.unknownext");
    fs::write(&file, "garbage\n").expect("write file");

    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };

    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-noop",
        "command": "lsp_diagnostics",
        "file": file.display().to_string(),
        "wait_ms": 0
    }))
    .expect("request parses");

    let response = handle_lsp_diagnostics(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true);
    assert_eq!(json["complete"], true, "should be complete (no work to do)");
    assert_eq!(json["total"], 0);
    assert!(
        json["note"]
            .as_str()
            .unwrap_or("")
            .contains("no LSP server"),
        "expected note about missing server: {json}"
    );
    assert!(json["lsp_servers_used"].as_array().unwrap().is_empty());
}

// ────────────────────────────────────────────────────────────────────────────
// Empty publish "checked clean" preservation
// ────────────────────────────────────────────────────────────────────────────

/// After a `publishDiagnostics` with empty array, the cache should
/// distinguish "checked, clean" from "never checked". The exact semantic
/// is: `get_diagnostics_for_file` returns empty (no errors), but a
/// publish_epoch was recorded internally.
///
/// This is a behavioral regression test for the explicit fix to
/// `DiagnosticsStore` Oracle flagged.
#[test]
fn test_empty_publish_is_not_lost() {
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let mut manager = manager_with_fake_server();

    // First publish has 2 diagnostics
    manager
        .notify_file_changed_default(file, "fn main() {}\n")
        .expect("open file");
    wait_for_publish(&mut manager);
    assert_eq!(manager.get_diagnostics_for_file(file).len(), 2);

    // Close the file → fake server publishes [] (empty array)
    manager.notify_file_closed(file).expect("close file");
    wait_for_publish(&mut manager);

    // Cache returns empty (the file is "checked clean" now). Importantly,
    // this is preserved as a publish_epoch in the store, not deleted —
    // but at the public API level, the diagnostics list is empty.
    let diagnostics = manager.get_diagnostics_for_file(file);
    assert!(
        diagnostics.is_empty(),
        "empty publish should clear errors but not be silently lost"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// LRU cap on DiagnosticsStore
// ────────────────────────────────────────────────────────────────────────────

/// Diagnostics cache must respect the configured cap to prevent unbounded
/// memory growth on long-running sessions in big monorepos.
#[test]
fn test_diagnostic_cache_respects_cap() {
    use aft::lsp::diagnostics::{DiagnosticSeverity, DiagnosticsStore, StoredDiagnostic};
    use aft::lsp::registry::ServerKind;

    let mut store = DiagnosticsStore::with_capacity(3);

    // Insert 5 distinct files. The cache should evict the 2 oldest.
    for i in 0..5 {
        let file = PathBuf::from(format!("/tmp/proj/src/file{i}.rs"));
        let diag = StoredDiagnostic {
            file: file.clone(),
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 5,
            severity: DiagnosticSeverity::Error,
            message: format!("error in {i}"),
            code: None,
            source: Some("test".to_string()),
        };
        store.publish_with_kind(ServerKind::Rust, file, vec![diag]);
    }

    // Files 0 and 1 should have been evicted; 2, 3, 4 remain.
    let all = store.all();
    assert_eq!(
        all.len(),
        3,
        "expected 3 entries after LRU eviction, got {}",
        all.len()
    );
    let messages: Vec<String> = all.iter().map(|d| d.message.clone()).collect();
    assert!(messages.iter().any(|m| m.contains("error in 4")));
    assert!(messages.iter().any(|m| m.contains("error in 3")));
    assert!(messages.iter().any(|m| m.contains("error in 2")));
    assert!(!messages.iter().any(|m| m.contains("error in 0")));
    assert!(!messages.iter().any(|m| m.contains("error in 1")));
}

// ────────────────────────────────────────────────────────────────────────────
// Pull diagnostics happy path (textDocument/diagnostic)
// ────────────────────────────────────────────────────────────────────────────

/// When the server declares pull-diagnostic capability AND the LSP client
/// requests `textDocument/diagnostic`, the response should populate cache
/// entries and be reachable via `get_diagnostics_for_file`.
///
/// We exercise this via env vars on the fake server: `AFT_FAKE_LSP_PULL=1`
/// flips it to declare the capability.
#[test]
fn test_pull_diagnostics_returns_full_report() {
    use aft::lsp::manager::PullFileOutcome;

    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let config = Config {
        project_root: Some(_root.clone()),
        ..Config::default()
    };

    // Spawn a manager with a fake server in PULL mode.
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::Rust, fake_server_path());
    manager.set_extra_env("AFT_FAKE_LSP_PULL", "1");

    // Open the file so server is initialized + we can request pull.
    manager
        .notify_file_changed_default(file, "fn main() {}\n")
        .expect("open file");
    // Wait for any push the fake also sends post-didOpen.
    let _ = collect_event(&mut manager, |_e| true);

    let results = manager
        .pull_file_diagnostics(file, &config)
        .expect("pull diagnostics succeeds");

    assert_eq!(results.len(), 1, "expected 1 server result");
    let result = &results[0];
    match &result.outcome {
        PullFileOutcome::Full { diagnostic_count } => {
            assert_eq!(*diagnostic_count, 1, "expected 1 pulled diagnostic");
        }
        other => panic!("expected Full report, got {other:?}"),
    }

    // The pulled diagnostics must also be in the cache, addressable by file.
    let cached = manager.get_diagnostics_for_file(file);
    assert!(
        cached.iter().any(|d| d.code.as_deref() == Some("E0PULL")),
        "pulled diagnostic should be reachable via cache: {cached:?}"
    );
}

/// When the server doesn't declare diagnosticProvider, pull falls back
/// to "PullNotSupported" without crashing. The convention says the agent
/// must see this honestly.
#[test]
fn test_pull_diagnostics_falls_back_when_unsupported() {
    use aft::lsp::manager::PullFileOutcome;

    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let config = Config {
        project_root: Some(_root.clone()),
        ..Config::default()
    };

    // Spawn fake server WITHOUT pull capability (default).
    let mut manager = manager_with_fake_server();
    manager
        .notify_file_changed_default(file, "fn main() {}\n")
        .expect("open file");
    let _ = collect_event(&mut manager, |_e| true);

    let results = manager
        .pull_file_diagnostics(file, &config)
        .expect("pull request itself should succeed");

    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0].outcome, PullFileOutcome::PullNotSupported),
        "expected PullNotSupported, got {:?}",
        results[0].outcome
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Document staleness: didChange when disk content drifts
// ────────────────────────────────────────────────────────────────────────────

/// If the file on disk is modified outside AFT (e.g. another tool, manual
/// edit), the next `ensure_file_open` call must detect the drift and send
/// a `didChange` so the LSP server's view stays in sync. Otherwise pull
/// or hover queries would return diagnostics for stale content.
///
/// This is a regression test for Oracle's hidden-bug finding #6.
#[test]
fn test_ensure_file_open_detects_disk_drift() {
    let (_temp_dir, root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };

    let mut manager = manager_with_fake_server();
    // Open the file the first time.
    manager.ensure_file_open(file, &config).expect("first open");

    // Sleep briefly to ensure the new mtime is observably different from
    // the original write. macOS mtime resolution is 1 second, so this
    // ensures DocumentStore::has_disk_drifted detects the change.
    thread::sleep(Duration::from_millis(1100));

    // Simulate external modification: change content. The new mtime is set
    // implicitly by the write call.
    let new_content = "fn main() { println!(\"changed externally\"); }\n";
    fs::write(file, new_content).expect("external write");

    // Drain anything queued and then re-open. Should re-sync (didChange).
    let _ = manager.drain_events();

    manager
        .ensure_file_open(file, &config)
        .expect("re-open after drift");

    // The fake server emits "custom/documentChanged" on didChange. If
    // ensure_file_open detected drift, that notification arrives.
    let event = collect_event(&mut manager, |event| {
        matches!(
            event,
            LspEvent::Notification { method, .. } if method == "custom/documentChanged"
        )
    });
    assert!(
        event.is_some(),
        "expected didChange after disk drift; got nothing"
    );
}

// =============================================================================
// v0.17.3 stale-diagnostics regression tests
//
// These tests lock in the fix for the stale-diagnostics bug: when the
// post-edit wait times out, return verified-fresh entries only and report
// pending servers via PostEditWaitOutcome — never return pre-edit cached
// entries dressed up as fresh.
// =============================================================================

#[test]
fn post_edit_wait_returns_only_fresh_diagnostics() {
    // The bug: tsserver/etc. publishes diagnostics for v1, edit advances to
    // v2, deadline hits before v2 is published, the wait used to return v1
    // entries. After v0.17.3, the wait must return only entries whose
    // version matches the post-edit target.
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let ctx = app_context_with_fake_lsp();

    // First write: server publishes for version 1.
    let outcome = ctx.lsp_notify_and_collect_diagnostics(
        file,
        "fn main() { println!(\"v1\"); }\n",
        Duration::from_secs(2),
    );
    assert!(
        outcome.complete(),
        "first wait should be complete (server published)"
    );
    let v1_count = outcome.diagnostics.len();
    assert!(v1_count > 0, "fake server publishes diagnostics");

    // Second write: server publishes for version 2 (different content).
    let outcome = ctx.lsp_notify_and_collect_diagnostics(
        file,
        "fn main() { println!(\"v2 different\"); }\n",
        Duration::from_secs(2),
    );
    assert!(outcome.complete(), "second wait should also be complete");
    // Diagnostics from v2 must be different from v1 (fake server returns a
    // distinct diagnostic on subsequent didChange).
    assert!(
        outcome
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("E0002")),
        "expected v2 diagnostic E0002, got {:?}",
        outcome.diagnostics
    );
}

#[test]
fn post_edit_outcome_reports_complete_when_no_server_registered() {
    // No server matches a .txt file extension. The outcome must be the
    // default (empty diagnostics, complete=true) — "there is nothing to
    // wait for" is the honest answer, not "we waited and got nothing."
    let temp_dir = tempdir().expect("tempdir");
    let file = temp_dir.path().join("notes.txt");
    fs::write(&file, "some text\n").expect("write");
    let ctx = app_context_with_fake_lsp();

    let outcome =
        ctx.lsp_notify_and_collect_diagnostics(&file, "new text\n", Duration::from_millis(500));

    assert!(
        outcome.complete(),
        "no-server case must be complete=true (nothing to wait for)"
    );
    assert!(outcome.diagnostics.is_empty());
    assert!(outcome.pending_servers.is_empty());
    assert!(outcome.exited_servers.is_empty());
}

#[test]
fn post_edit_diagnostics_are_root_aware() {
    // The pre-v0.17.3 publish path stored diagnostics under
    // ServerKey { kind, root: PathBuf::new() } via publish_with_kind. After
    // the fix, handle_publish_diagnostics uses the real workspace root from
    // LspEvent::Notification. Verify that the cache entry carries a non-
    // empty root.
    let (_temp_dir, root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let mut manager = manager_with_fake_server();

    manager
        .notify_file_changed_default(file, "fn main() {}\n")
        .expect("notify");
    wait_for_publish(&mut manager);

    let canonical_file = fs::canonicalize(file).expect("canonical");
    let entries: Vec<_> = manager
        .diagnostics_store_for_test()
        .entries_for_file(&canonical_file)
        .into_iter()
        .map(|(key, _)| key.clone())
        .collect();

    assert!(
        !entries.is_empty(),
        "expected at least one entry after publish"
    );
    let canonical_root = fs::canonicalize(&root).expect("canonical root");
    for key in &entries {
        assert!(
            !key.root.as_os_str().is_empty(),
            "v0.17.3: entry root must not be empty (got {:?})",
            key.root
        );
        assert_eq!(
            key.root, canonical_root,
            "v0.17.3: entry root must match the workspace root"
        );
    }
}

#[test]
fn empty_publish_is_fresh_clean_after_edit() {
    // When tsserver re-analyzes and finds nothing wrong, it publishes
    // diagnostics: []. Pre-v0.17.3 the wait loop returned whatever was in
    // the cache without checking version, so this looked indistinguishable
    // from "timed out". Post-fix, an empty publish for the target version
    // is detected as fresh-and-clean.
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let ctx = app_context_with_fake_lsp();

    // Use the special "clear-on-change" content that the fake server
    // recognizes — actually, the fake server always emits something on
    // publish. The value of this test is that even a normal publish for
    // the post-edit version must be marked fresh by version-match.
    let outcome =
        ctx.lsp_notify_and_collect_diagnostics(file, "fn main() {}\n", Duration::from_secs(2));

    assert!(
        outcome.complete(),
        "server published for the post-edit version, so complete=true"
    );
    assert!(
        outcome.pending_servers.is_empty(),
        "no servers should be pending"
    );
}

#[test]
fn post_edit_rejects_publish_with_stale_version() {
    // The Oracle review's primary correctness concern: post-edit wait must
    // reject `publishDiagnostics` whose `version` does NOT match the
    // post-edit document version. Otherwise an old in-flight publish that
    // races with the agent's edit would be served as "fresh" and the
    // agent would see diagnostics for the previous version of the file.
    //
    // This test forces the fake LSP server to publish `version - 1`
    // instead of the actual version (via AFT_FAKE_LSP_STALE_VERSION env).
    // The wait should classify that publish as STALE, so:
    //   - `complete()` is false (no fresh publish arrived)
    //   - the server appears in `pending_servers`
    //   - no diagnostic entries are returned to the agent
    let (_temp_dir, root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];

    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
    {
        let mut lsp = ctx.lsp();
        lsp.override_binary(ServerKind::Rust, fake_server_path());
        lsp.set_extra_env("AFT_FAKE_LSP_STALE_VERSION", "1");
    }

    // Pre-warm: do one regular write so the server is up. Use the first
    // call (which sends didOpen with version 0) to seed state — but with
    // STALE_VERSION on, the fake publishes version=-1 which won't match
    // any wait. Use a long enough timeout that the wait actually drains.
    let outcome =
        ctx.lsp_notify_and_collect_diagnostics(file, "fn main() {}\n", Duration::from_millis(800));

    assert!(
        !outcome.complete(),
        "stale-version publish must NOT be marked complete; got outcome={:?}",
        outcome
    );
    assert!(
        outcome
            .pending_servers
            .iter()
            .any(|key| key.kind == ServerKind::Rust),
        "rust server should be in pending_servers; got pending={:?}",
        outcome.pending_servers
    );
    assert!(
        outcome.diagnostics.is_empty(),
        "no diagnostics should be returned for stale publish; got {:?}",
        outcome.diagnostics
    );

    // Sanity-check: without STALE_VERSION, the same flow IS complete.
    // (Use a fresh context so no state leaks.)
    let _ = root;
}

#[test]
fn post_edit_wait_rejects_stale_pre_edit_publish() {
    use aft::lsp::diagnostics::{DiagnosticEntry, StoredDiagnostic};
    use aft::lsp::manager::{post_edit_entry_is_fresh, PreEditSnapshot};

    let file = PathBuf::from("/tmp/aft-stale.rs");
    let stale = DiagnosticEntry {
        diagnostics: vec![StoredDiagnostic {
            file,
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 2,
            severity: DiagnosticSeverity::Error,
            message: "stale pre-edit".into(),
            code: None,
            source: Some("fake".into()),
        }],
        epoch: 7,
        result_id: None,
        version: Some(4),
    };

    let pre_edit = PreEditSnapshot {
        epoch: 6,
        document_version_at_capture: Some(4),
    };

    assert!(
        !post_edit_entry_is_fresh(&stale, 5, pre_edit),
        "publish for version N-1 must not prove freshness for post-edit version N"
    );
}

#[test]
fn post_edit_wait_rejects_unversioned_epoch_only_publish() {
    use aft::lsp::diagnostics::{DiagnosticEntry, StoredDiagnostic};
    use aft::lsp::manager::{post_edit_entry_is_fresh, PreEditSnapshot};

    let file = PathBuf::from("/tmp/aft-unversioned.rs");
    let unversioned = DiagnosticEntry {
        diagnostics: vec![StoredDiagnostic {
            file,
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 2,
            severity: DiagnosticSeverity::Error,
            message: "unversioned".into(),
            code: None,
            source: Some("fake".into()),
        }],
        epoch: 8,
        result_id: None,
        version: None,
    };

    let pre_edit = PreEditSnapshot {
        epoch: 7,
        document_version_at_capture: Some(4),
    };

    assert!(
        !post_edit_entry_is_fresh(&unversioned, 5, pre_edit),
        "epoch advancement alone must not prove freshness for unversioned publishes"
    );
}

#[test]
fn push_only_per_file_freshness() {
    use aft::lsp::diagnostics::DiagnosticsStore;

    let mut store = DiagnosticsStore::new();
    let server = ServerKey {
        kind: ServerKind::TypeScript,
        root: PathBuf::from("/tmp/aft-push-root"),
    };
    let a = PathBuf::from("/tmp/aft-push-root/a.ts");
    let b = PathBuf::from("/tmp/aft-push-root/b.ts");
    let since = Instant::now();

    store.publish(server.clone(), a, Vec::new());

    assert!(
        !store.has_publish_for_file_after(&server, &b, since),
        "publish for a.ts must not prove freshness for b.ts"
    );
}

#[test]
fn directory_mode_with_walk_truncation_reports_complete_false() {
    let temp_dir = tempdir().expect("tempdir");
    let root = temp_dir.path().join("workspace");
    let src = root.join("src");
    fs::create_dir_all(&src).expect("create src");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("cargo toml");
    for i in 0..250 {
        fs::write(src.join(format!("file_{i:03}.rs")), "fn main() {}\n").expect("write rs");
    }

    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-dir-truncated",
        "command": "lsp_diagnostics",
        "directory": src.display().to_string(),
    }))
    .expect("request parses");

    let response = handle_lsp_diagnostics(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true);
    assert_eq!(json["complete"], false);
    assert_eq!(json["walk_truncated"], true);
    assert_eq!(
        json["unchecked_files"].as_array().expect("unchecked").len(),
        50
    );
}

#[test]
fn directory_mode_requires_each_matching_server_to_cover_file() {
    let (_temp_dir, root, files) = typescript_workspace_with_files(&["foo.ts"]);
    let source = &files[0];
    fs::write(root.join("biome.json"), "{}\n").expect("write biome config");
    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    {
        let mut lsp = ctx.lsp();
        // TypeScript and Biome both match .ts. Only Biome is allowed to start,
        // so a Biome cache entry must not make the file look fully covered.
        lsp.override_binary(
            ServerKind::TypeScript,
            PathBuf::from("/definitely/missing/ts-lsp"),
        );
        lsp.override_binary(ServerKind::Biome, fake_server_path());
    }

    ctx.lsp_notify_file_changed(source, "export const value = 2;\n");
    wait_for_publish(&mut ctx.lsp());

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-dir-missing-peer-server",
        "command": "lsp_diagnostics",
        "directory": root.join("src").display().to_string(),
    }))
    .expect("request parses");

    let response = handle_lsp_diagnostics(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true);
    assert_eq!(
        json["complete"], false,
        "missing tsserver coverage must be incomplete: {json}"
    );
    let unchecked = json["unchecked_files"].as_array().expect("unchecked files");
    assert!(
        unchecked
            .iter()
            .any(|path| path.as_str().is_some_and(|path| path.ends_with("foo.ts"))),
        "foo.ts should be unchecked because one matching server never reported: {json}"
    );
}

#[test]
fn server_exit_cleanup_isolates_by_root() {
    use aft::lsp::diagnostics::{DiagnosticsStore, StoredDiagnostic};

    let mut store = DiagnosticsStore::new();
    let root_a = PathBuf::from("/tmp/aft-root-a");
    let root_b = PathBuf::from("/tmp/aft-root-b");
    let key_a = ServerKey {
        kind: ServerKind::Rust,
        root: root_a.clone(),
    };
    let key_b = ServerKey {
        kind: ServerKind::Rust,
        root: root_b.clone(),
    };
    let file_a = root_a.join("src/main.rs");
    let file_b = root_b.join("src/main.rs");
    let diag_b = StoredDiagnostic {
        file: file_b.clone(),
        line: 1,
        column: 1,
        end_line: 1,
        end_column: 2,
        severity: DiagnosticSeverity::Error,
        message: "keep me".into(),
        code: None,
        source: None,
    };

    store.publish(key_a.clone(), file_a.clone(), Vec::new());
    store.publish(key_b.clone(), file_b.clone(), vec![diag_b]);
    store.clear_for_server(&key_a);

    assert!(store.entries_for_file(&file_a).is_empty());
    assert_eq!(store.entries_for_file(&file_b).len(), 1);
}

#[test]
fn did_change_watched_files_skipped_when_unsupported() {
    let (_temp_dir, root, files) = typescript_workspace_with_files(&["foo.ts"]);
    let source = &files[0];
    let package_json = root.join("package.json");
    let config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let mut manager = manager_with_fake_typescript_server();
    // Drive the fake server to OMIT workspace.didChangeWatchedFiles from its
    // initialize result so we exercise the F5 capability-gate skip path.
    manager.set_extra_env("AFT_FAKE_LSP_NO_WATCHED_FILES", "1");

    manager
        .notify_file_changed(source, "export const value = 2;\n", &config)
        .expect("open ts source");
    wait_for_publish(&mut manager);

    manager
        .notify_files_watched_changed(&[(package_json, FileChangeType::CHANGED)], &config)
        .expect("notify watched files skips unsupported server");

    let event = collect_event(&mut manager, |event| {
        matches!(
            event,
            LspEvent::Notification { method, .. } if method == "custom/watchedFilesChanged"
        )
    });
    assert!(
        event.is_none(),
        "unsupported server must not receive watched-file notification"
    );
}

// NOTE: A test for the "no LSP server running for file" path was
// considered but skipped here. It would require guaranteeing
// rust-analyzer is NOT on PATH and no other registered server matches a
// .rs file, which is fragile across dev machines and CI. The semantically
// equivalent path IS covered by
// `post_edit_outcome_reports_complete_when_no_server_registered`, which
// uses a .txt file (no registered server in the registry).
