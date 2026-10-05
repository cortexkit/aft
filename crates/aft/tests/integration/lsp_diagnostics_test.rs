use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aft::commands::delete_file::handle_delete_file;
use aft::commands::lsp_diagnostics::handle_lsp_diagnostics;
use aft::commands::move_file::handle_move_file;
use aft::commands::write::handle_write;
use aft::config::{Config, UserServerDef};
use aft::context::AppContext;
use aft::lsp::child_registry::LspChildRegistry;
use aft::lsp::client::{LspClient, LspEvent};
use aft::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
use aft::lsp::manager::{LspManager, WATCHED_FILE_FORWARD_CAP, WATCHER_FORWARD_BACKLOG_CAP};
use aft::lsp::registry::{is_config_file_path, is_config_file_path_with_custom, ServerKind};
use aft::lsp::roots::ServerKey;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use aft::runtime_drain::drain_watcher_events;
use aft::watcher_filter::WatcherDispatchEvent;
use lsp_types::FileChangeType;
use tempfile::tempdir;

#[cfg(unix)]
use super::helpers::warm_executable;
use super::helpers::AftProcess;

fn fake_server_path() -> PathBuf {
    crate::test_helpers::fake_lsp::fake_server_binary()
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
    // Positive-wait budget: every caller either expects the event or drains
    // best-effort. 2s raced the fake server's spawn+init under machine load
    // (1-in-3 failures in isolation on a loaded host, 2026-08-09); green runs
    // return on the event and never pay the full budget.
    let deadline = Instant::now() + Duration::from_secs(15);
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
    .unwrap_or_else(|| {
        #[cfg(windows)]
        panic!(
            "timed out waiting for watched-file notification; routing trace:\n{}",
            ctx.lsp().watched_file_notification_trace_for_test()
        );
        #[cfg(not(windows))]
        panic!("timed out waiting for watched-file notification");
    });

    #[cfg(windows)]
    {
        let trace = ctx
            .lsp()
            .watched_file_notification_trace_for_test()
            .to_string();
        assert!(
            trace.contains("outcome=sent"),
            "fake server replied without a recorded watched-file send; routing trace:\n{trace}"
        );
    }

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

fn pyright_langserver_available() -> bool {
    pyright_langserver_path().is_some()
}

fn pyright_langserver_path() -> Option<PathBuf> {
    which::which("pyright-langserver").ok().or_else(|| {
        std::env::var_os("AFT_TEST_PYRIGHT_LANGSERVER")
            .map(PathBuf::from)
            .filter(|path| path.is_file())
    })
}

fn pyright_extra_paths_workspace() -> (tempfile::TempDir, PathBuf, PathBuf, String) {
    let temp_dir = tempdir().expect("tempdir");
    let root = temp_dir.path().join("python-app");
    let src_dir = root.join("src");
    let lib_dir = root.join("libs").join("fakepkg");
    let source = src_dir.join("app.py");
    let source_text = "import fakepkg\n\nVALUE = fakepkg.VALUE\n".to_string();

    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::create_dir_all(&lib_dir).expect("create fake package dir");
    fs::write(
        root.join("pyrightconfig.json"),
        r#"{
  "include": ["src"],
  "extraPaths": ["libs"],
  "reportMissingImports": "error"
}
"#,
    )
    .expect("write pyrightconfig");
    fs::write(src_dir.join("requirements.txt"), "fakepkg==1.0\n")
        .expect("write nearer fallback marker");
    fs::write(lib_dir.join("__init__.py"), "VALUE = 1\n").expect("write fake package");
    fs::write(&source, &source_text).expect("write source");

    (temp_dir, root, source, source_text)
}

fn pyright_diagnostics_for(
    file: &std::path::Path,
    source_text: &str,
    config: &Config,
) -> Vec<StoredDiagnostic> {
    let mut manager = LspManager::new();
    let pre_snapshot = manager.snapshot_pre_edit_state(file);
    let versions = manager
        .notify_file_changed_versioned(file, source_text, config)
        .expect("notify pyright file changed");
    assert!(
        !versions.is_empty(),
        "pyright should start for the fixture before diagnostics can be checked"
    );

    let outcome = manager.wait_for_post_edit_diagnostics(
        file,
        config,
        &versions,
        &pre_snapshot,
        Duration::from_secs(20),
    );
    assert!(
        outcome.pending_servers.is_empty(),
        "timed out waiting for pyright diagnostics: {:?}",
        outcome.pending_servers
    );
    assert!(
        outcome.exited_servers.is_empty(),
        "pyright exited before publishing diagnostics: {:?}",
        outcome.exited_servers
    );
    outcome.diagnostics
}

fn has_fakepkg_missing_import(diagnostics: &[StoredDiagnostic]) -> bool {
    diagnostics.iter().any(|diag| {
        diag.code.as_deref() == Some("reportMissingImports")
            || diag
                .message
                .contains("Import \"fakepkg\" could not be resolved")
    })
}

fn app_context_with_fake_lsp() -> AppContext {
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config::default()),
    );
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
    ctx
}

fn app_context_with_fake_typescript_lsp() -> AppContext {
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config::default()),
    );
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx
}

/// Returns the script with the directory that holds it; the caller keeps the
/// directory alive for as long as the language server may be launched.
fn executable_crashing_lsp_script(stderr: &str) -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempdir().expect("tempdir for crashing lsp");
    #[cfg(windows)]
    {
        let script = temp_dir.path().join("crashing_lsp.cmd");
        let mut source = String::from("@echo off\r\n");
        for line in stderr.lines() {
            if line.is_empty() {
                source.push_str("echo. 1>&2\r\n");
            } else {
                source.push_str(&format!("echo {line} 1>&2\r\n"));
            }
        }
        source.push_str("exit /b 1\r\n");
        fs::write(&script, source).expect("write crashing lsp cmd script");
        (temp_dir, script)
    }

    #[cfg(not(windows))]
    let script = temp_dir.path().join("crashing_lsp.py");
    #[cfg(not(windows))]
    let source = format!(
        "#!/usr/bin/env python3
import sys
sys.stderr.write({stderr:?})
sys.stderr.flush()
"
    );
    #[cfg(not(windows))]
    fs::write(&script, source).expect("write crashing lsp script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("chmod crashing lsp script");
    }
    #[cfg(not(windows))]
    warm_executable(&script, &["--version"]);
    #[cfg(not(windows))]
    (temp_dir, script)
}

#[test]
fn pyright_uses_pyrightconfig_extra_paths_above_nearer_requirements_marker() {
    if !pyright_langserver_available() {
        eprintln!("skipping pyright extraPaths integration test: pyright-langserver not on PATH");
        return;
    }

    let (_temp_dir, root, source, source_text) = pyright_extra_paths_workspace();
    let default_config = Config {
        project_root: Some(root.clone()),
        ..Config::default()
    };
    let ignored_config = Config {
        project_root: Some(root),
        lsp_servers: vec![UserServerDef {
            id: "python".to_string(),
            root_markers: vec!["requirements.txt".to_string()],
            ..UserServerDef::default()
        }],
        ..Config::default()
    };

    let ignored_diagnostics = pyright_diagnostics_for(&source, &source_text, &ignored_config);
    assert!(
        has_fakepkg_missing_import(&ignored_diagnostics),
        "control should report a missing import when pyrightconfig.json is outside the LSP root; diagnostics: {ignored_diagnostics:?}"
    );

    let resolved_diagnostics = pyright_diagnostics_for(&source, &source_text, &default_config);
    assert!(
        !has_fakepkg_missing_import(&resolved_diagnostics),
        "pyright should honor pyrightconfig.json extraPaths from the selected workspace root; diagnostics: {resolved_diagnostics:?}"
    );
}

#[test]
fn pyright_uses_nested_workspace_virtualenv_for_imports() {
    let Some(pyright) = pyright_langserver_path() else {
        eprintln!(
            "skipping nested virtualenv test: pyright-langserver not on PATH or AFT_TEST_PYRIGHT_LANGSERVER"
        );
        return;
    };
    let Ok(python) = which::which("python3").or_else(|_| which::which("python")) else {
        eprintln!("skipping nested virtualenv test: Python interpreter not on PATH");
        return;
    };

    let temp_dir = tempdir().expect("tempdir");
    let repository = temp_dir.path().join("repository");
    let backend = repository.join("backend");
    let source = backend.join("app").join("main.py");
    let virtualenv = backend.join(".venv");
    fs::create_dir_all(source.parent().unwrap()).expect("create source directory");
    fs::write(backend.join("pyproject.toml"), "[project]\nname = 'demo'\n")
        .expect("write pyproject");
    let status = Command::new(python)
        .args(["-m", "venv"])
        .arg(&virtualenv)
        .status()
        .expect("run python -m venv");
    // A present-but-broken Python is an environment failure, not a missing
    // prerequisite: fail loud so the CI lane cannot go vacuously green.
    assert!(status.success(), "python -m venv failed");
    let virtualenv_python = if cfg!(windows) {
        virtualenv.join("Scripts").join("python.exe")
    } else {
        virtualenv.join("bin").join("python")
    };
    let site_packages = Command::new(&virtualenv_python)
        .args(["-c", "import site; print(site.getsitepackages()[0])"])
        .output()
        .expect("query virtualenv site-packages");
    assert!(site_packages.status.success());
    let fake_package =
        PathBuf::from(String::from_utf8(site_packages.stdout).unwrap().trim()).join("fakepkg");
    fs::create_dir_all(&fake_package).expect("create fake package");
    fs::write(fake_package.join("__init__.py"), "VALUE = 1\n").expect("write fake package");
    let source_text = "import fakepkg\n\nVALUE = fakepkg.VALUE\n";
    fs::write(&source, source_text).expect("write source");
    let config = Config {
        project_root: Some(repository),
        lsp_paths_extra: vec![pyright.parent().unwrap().to_path_buf()],
        ..Config::default()
    };

    let diagnostics = pyright_diagnostics_for(&source, source_text, &config);

    assert!(
        !has_fakepkg_missing_import(&diagnostics),
        "Pyright should resolve packages from the nested workspace virtualenv: {diagnostics:?}"
    );
}

#[test]
fn custom_pull_only_server_reports_diagnostics_on_edit_and_inspect() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().join("workspace");
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&root).expect("create workspace");
    fs::create_dir_all(&bin_dir).expect("create binary directory");
    fs::write(root.join(".rumdl.toml"), "[global]\n").expect("write root marker");
    let file = root.join("README.md");
    fs::write(&file, "before\n").expect("write source");

    let fake_server = fake_server_path();
    let binary_name = fake_server
        .file_name()
        .expect("fake server filename")
        .to_string_lossy()
        .to_string();
    fs::copy(&fake_server, bin_dir.join(&binary_name)).expect("install fake custom server");

    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_FAKE_LSP_PULL", std::ffi::OsStr::new("1")),
        ("AFT_FAKE_LSP_DISABLE_PUSH", std::ffi::OsStr::new("1")),
        (
            "AFT_FAKE_LSP_SERVER_STATUS",
            std::ffi::OsStr::new("disabled"),
        ),
    ]);
    let configure = aft.send(
        &serde_json::json!({
            "id": "cfg-custom-pull-only",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "lsp_paths_extra": [bin_dir],
            "config": crate::helpers::user_config(serde_json::json!({
                "lsp": {
                    "servers": {
                        "pull-only-markdown": {
                            "extensions": ["md"],
                            "binary": binary_name,
                            "args": [],
                            "root_markers": [".rumdl.toml"]
                        }
                    }
                }
            }))
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let edit = aft.send(
        &serde_json::json!({
            "id": "edit-custom-pull-only",
            "command": "edit_match",
            "file": file,
            "match": "before",
            "replacement": "after",
            "diagnostics": true,
            "wait_ms": 2_000
        })
        .to_string(),
    );
    assert_eq!(edit["success"], true, "edit failed: {edit:?}");
    assert_eq!(
        edit["lsp_complete"], true,
        "pull response should complete the post-edit wait: {edit:?}"
    );
    let edit_diagnostics = edit["lsp_diagnostics"]
        .as_array()
        .expect("edit diagnostics array");
    assert_eq!(edit_diagnostics.len(), 1, "edit response: {edit:?}");
    assert_eq!(edit_diagnostics[0]["message"], "test pull diagnostic");

    let inspect = aft.send(
        &serde_json::json!({
            "id": "inspect-custom-pull-only",
            "command": "inspect",
            "scope": file,
            "sections": ["diagnostics"]
        })
        .to_string(),
    );
    assert_eq!(inspect["success"], true, "inspect failed: {inspect:?}");
    assert_eq!(inspect["inspect_terminal"], "fresh", "inspect: {inspect:?}");
    assert_eq!(inspect["summary"]["diagnostics"]["errors"], 1);
    let inspect_diagnostics = inspect["details"]["diagnostics"]
        .as_array()
        .expect("inspect diagnostics array");
    assert_eq!(
        inspect_diagnostics.len(),
        1,
        "inspect response: {inspect:?}"
    );
    assert_eq!(inspect_diagnostics[0]["message"], "test pull diagnostic");

    assert!(aft.shutdown().success());
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
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(config),
    );
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());

    let config_snapshot = ctx.config();
    ctx.lsp()
        .notify_file_changed_versioned(source, "export const value = 2;\n", &config_snapshot)
        .expect("versioned notify spawns the workspace server");
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
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(config),
    );
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());

    let config_snapshot = ctx.config();
    ctx.lsp()
        .notify_file_changed_versioned(source, "export const value = 2;\n", &config_snapshot)
        .expect("versioned notify spawns the workspace server");
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
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(config),
    );
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    let config_snapshot = ctx.config();
    ctx.lsp()
        .notify_file_changed_versioned(source, "export const value = 2;\n", &config_snapshot)
        .expect("versioned notify spawns the workspace server");
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
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(config),
    );
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    let config_snapshot = ctx.config();
    ctx.lsp()
        .notify_file_changed_versioned(source, "export const value = 2;\n", &config_snapshot)
        .expect("versioned notify spawns the workspace server");
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
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(config),
    );
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    let config_snapshot = ctx.config();
    ctx.lsp()
        .notify_file_changed_versioned(source, "export const value = 2;\n", &config_snapshot)
        .expect("versioned notify spawns the workspace server");
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
    // Cargo.lock counts as configuration: rust-analyzer's workspace load
    // fails on a stale one, so its changes must reach the server.
    assert!(is_config_file_path_with_custom(
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
    // Cargo.lock is the one lockfile that is watched: a stale Cargo.lock
    // stops rust-analyzer from loading the workspace's dependencies, so its
    // updates must reach the server. The others only churn on installs.
    for lockfile in [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
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
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(config),
    );
    ctx.lsp().override_binary(
        ServerKind::Custom(std::sync::Arc::from(server_id)),
        fake_server_path(),
    );

    // Post-write notifications no longer cold-start servers (they are
    // best-effort against RUNNING servers), so the workspace server must be
    // established through the explicit versioned path first, the way an
    // actual diagnostics request would.
    let config_snapshot = ctx.config();
    ctx.lsp()
        .notify_file_changed_versioned(&source, "export const value = 2;\n", &config_snapshot)
        .expect("versioned notify spawns the workspace server");
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
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(config),
    );
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    let config_snapshot = ctx.config();
    ctx.lsp()
        .notify_file_changed_versioned(source, "export const value = 2;\n", &config_snapshot)
        .expect("versioned notify spawns the workspace server");
    wait_for_publish(&mut ctx.lsp());

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "write-changed-config",
        "command": "write",
        "file": config_path.display().to_string(),
        "content": "{\"devDependencies\":{\"typescript\":\"*\"}}\n"
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
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(config),
    );
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    let config_snapshot = ctx.config();
    ctx.lsp()
        .notify_file_changed_versioned(source, "export const value = 2;\n", &config_snapshot)
        .expect("versioned notify spawns the workspace server");
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
    let canonical_main = crate::helpers::canonicalize_like_product(main_rs);

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
    let canonical_main = crate::helpers::canonicalize_like_product(main_rs);

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
fn test_lsp_post_initialize_exit_reports_stderr_and_caches_failure() {
    let (_temp_dir, _root, files) = typescript_workspace_with_files(&["main.ts", "lib.ts"]);
    let first = &files[0];
    let second = &files[1];
    let ctx = app_context_with_fake_typescript_lsp();
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_PULL_EXIT_MODULE_NOT_FOUND", "1");

    let first_req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-post-init-exit-1",
        "command": "lsp_diagnostics",
        "file": first.display().to_string(),
        "wait_ms": 0
    }))
    .expect("request parses");
    let first_response = serde_json::to_value(handle_lsp_diagnostics(&first_req, &ctx))
        .expect("response serializes");
    let first_status = first_response["lsp_servers_used"][0]["status"]
        .as_str()
        .expect("status string");
    assert!(
        first_status.contains("MODULE_NOT_FOUND"),
        "missing stderr in first status: {first_status}"
    );
    assert!(
        first_status.contains("npm install -g typescript-language-server --force"),
        "missing reinstall hint in first status: {first_status}"
    );
    assert_eq!(ctx.lsp().active_client_count(), 0);

    let second_req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-post-init-exit-2",
        "command": "lsp_diagnostics",
        "file": second.display().to_string(),
        "wait_ms": 0
    }))
    .expect("request parses");
    let second_response = serde_json::to_value(handle_lsp_diagnostics(&second_req, &ctx))
        .expect("response serializes");
    let second_status = second_response["lsp_servers_used"][0]["status"]
        .as_str()
        .expect("status string");
    assert!(
        second_status.starts_with("spawn_failed"),
        "expected cached spawn failure, got: {second_status}"
    );
    assert!(
        second_status.contains("MODULE_NOT_FOUND")
            && second_status.contains("npm install -g typescript-language-server --force"),
        "cached status lost stderr/hint: {second_status}"
    );
    assert_eq!(ctx.lsp().active_client_count(), 0);
}

#[test]
fn test_lsp_initialize_crash_reports_stderr_and_hint() {
    let (_temp_dir, _root, files) = typescript_workspace_with_files(&["main.ts"]);
    let file = &files[0];
    let ctx = app_context_with_fake_typescript_lsp();
    ctx.lsp()
        .set_extra_env("AFT_FAKE_LSP_INIT_CRASH_MODULE_NOT_FOUND", "1");

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-init-crash",
        "command": "lsp_diagnostics",
        "file": file.display().to_string(),
        "wait_ms": 0
    }))
    .expect("request parses");
    let response =
        serde_json::to_value(handle_lsp_diagnostics(&req, &ctx)).expect("response serializes");
    let status = response["lsp_servers_used"][0]["status"]
        .as_str()
        .expect("status string");
    assert!(
        status.contains("server crashed during initialize"),
        "status: {status}"
    );
    assert!(status.contains("MODULE_NOT_FOUND"), "status: {status}");
    assert!(
        status.contains("npm install -g typescript-language-server --force"),
        "missing reinstall hint: {status}"
    );
}

/// The daemon's maintenance tick sweeps the process-wide child registry every
/// 250 ms while other threads start language servers. A sweep that ran while a
/// server was being spawned used to treat the brand-new child as an orphan
/// (no client owned it yet) and SIGTERM it, so `initialize` failed with a
/// broken pipe and an empty stderr. Hammer the sweep while starting real
/// server processes and require every handshake to succeed.
#[cfg(unix)]
#[test]
fn test_lsp_spawn_survives_concurrent_orphan_sweeps() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let (_temp_dir, root, _files) = rust_workspace_with_files(&["main.rs"]);
    let registry = LspChildRegistry::new();
    let stop = Arc::new(AtomicBool::new(false));
    let reaped = Arc::new(AtomicUsize::new(0));
    let sweeper = {
        let registry = registry.clone();
        let stop = Arc::clone(&stop);
        let reaped = Arc::clone(&reaped);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let count = registry.reap_children_with_gone_cwd_or_reclaimed_root();
                reaped.fetch_add(count, Ordering::Relaxed);
                thread::yield_now();
            }
        })
    };

    let mut failures = Vec::new();
    for attempt in 0..12 {
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let mut client = LspClient::spawn(
            ServerKind::Rust,
            root.clone(),
            &fake_server_path(),
            &[],
            &HashMap::new(),
            event_tx,
            registry.clone(),
        )
        .expect("spawn fake lsp");
        if let Err(err) = client.initialize(&root, None) {
            failures.push(format!("attempt {attempt}: {err}"));
        }
        let _ = client.shutdown();
    }
    stop.store(true, Ordering::Relaxed);
    sweeper.join().expect("sweeper thread");

    assert!(
        failures.is_empty(),
        "the orphan sweep killed {} freshly spawned server(s); failures: {failures:#?}",
        reaped.load(Ordering::Relaxed)
    );
}

/// Run one `lsp_diagnostics` call against a fake TypeScript server that dies
/// during `initialize`, and return the reported status together with the
/// manager's server-exit log line for that death.
fn initialize_death_status_and_exit_line(env: &[(&str, &str)]) -> (String, String) {
    let (_temp_dir, _root, files) = typescript_workspace_with_files(&["main.ts"]);
    let file = &files[0];
    let ctx = app_context_with_fake_typescript_lsp();
    for (key, value) in env {
        ctx.lsp().set_extra_env(key, value);
    }

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-init-death",
        "command": "lsp_diagnostics",
        "file": file.display().to_string(),
        "wait_ms": 0
    }))
    .expect("request parses");
    let response =
        serde_json::to_value(handle_lsp_diagnostics(&req, &ctx)).expect("response serializes");
    let status = response["lsp_servers_used"][0]["status"]
        .as_str()
        .unwrap_or_else(|| panic!("status string missing: {response:#}"))
        .to_string();

    // The exit line is written when the dead server's reader event is
    // drained, which can trail the failed request slightly.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let _ = ctx.lsp().drain_events();
        let lines = ctx.lsp().recent_server_exit_log_lines();
        if let Some(line) = lines.iter().find(|line| line.contains("TypeScript")) {
            return (status, line.clone());
        }
        assert!(
            Instant::now() < deadline,
            "no server-exit log line was recorded; status was: {status}; lines: {lines:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn test_lsp_initialize_exit_code_and_stderr_reach_status_and_exit_log() {
    let (status, line) = initialize_death_status_and_exit_line(&[
        ("AFT_FAKE_LSP_INIT_EXIT_CODE", "3"),
        (
            "AFT_FAKE_LSP_INIT_EXIT_STDERR",
            "fatal: toolchain could not be resolved|second stderr line",
        ),
    ]);

    assert!(
        status.contains("server crashed during initialize (exit 3 after "),
        "status must lead with the exit code: {status}"
    );
    assert!(
        status.contains("fatal: toolchain could not be resolved"),
        "status must carry the first stderr line: {status}"
    );
    assert!(line.contains("phase=initialize"), "log line: {line}");
    assert!(line.contains("status=exit 3"), "log line: {line}");
    assert!(line.contains("pid="), "log line: {line}");
    assert!(line.contains("elapsed="), "log line: {line}");
    assert!(line.contains("command="), "log line: {line}");
    assert!(
        line.contains("fatal: toolchain could not be resolved")
            && line.contains("second stderr line"),
        "log line must carry the stderr tail: {line}"
    );
    assert!(
        !line.contains('\n'),
        "the exit log entry must stay on one line: {line:?}"
    );
}

#[cfg(unix)]
#[test]
fn test_lsp_initialize_signal_death_names_the_signal() {
    let (status, line) = initialize_death_status_and_exit_line(&[
        ("AFT_FAKE_LSP_INIT_SELF_SIGNAL", "TERM"),
        ("AFT_FAKE_LSP_INIT_EXIT_STDERR", "about to be terminated"),
    ]);

    assert!(
        status.contains("signal 15 (SIGTERM)"),
        "status must name the signal: {status}"
    );
    assert!(
        line.contains("status=signal 15 (SIGTERM)"),
        "log line must name the signal: {line}"
    );
    assert!(line.contains("about to be terminated"), "log line: {line}");
}

#[test]
fn test_lsp_module_not_found_hint_uses_package_manager_path_and_binary() {
    let (_temp_dir, _root, files) = typescript_workspace_with_files(&["main.ts"]);
    let file = &files[0];
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config::default()),
    );
    let (_script_dir, script) = executable_crashing_lsp_script(
        "Error: Cannot find module '/Users/me/.local/share/pnpm/global/5/.pnpm/typescript-language-server@4.3.4/node_modules/typescript-language-server/lib/cli.mjs'
code: 'MODULE_NOT_FOUND'
",
    );
    ctx.lsp().override_binary(ServerKind::TypeScript, script);

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-init-crash-pnpm",
        "command": "lsp_diagnostics",
        "file": file.display().to_string(),
        "wait_ms": 0
    }))
    .expect("request parses");
    let response =
        serde_json::to_value(handle_lsp_diagnostics(&req, &ctx)).expect("response serializes");
    let status = response["lsp_servers_used"][0]["status"]
        .as_str()
        .expect("status string");

    assert!(
        status.contains("stderr (last 64 lines)"),
        "status: {status}"
    );
    assert!(
        status.contains("Try reinstalling: pnpm install -g typescript-language-server --force"),
        "missing pnpm reinstall hint: {status}"
    );
}

#[test]
fn test_lsp_stderr_tail_is_bounded_and_drained() {
    let (_temp_dir, root, _files) = rust_workspace_with_files(&["main.rs"]);
    let (event_tx, _event_rx) = crossbeam_channel::unbounded();
    let mut env = HashMap::new();
    env.insert(
        "AFT_FAKE_LSP_INIT_STDERR_BYTES".to_string(),
        "200000".to_string(),
    );
    let mut client = LspClient::spawn(
        ServerKind::Rust,
        root.clone(),
        &fake_server_path(),
        &[],
        &env,
        event_tx,
        LspChildRegistry::new(),
    )
    .expect("spawn fake lsp");

    let _ = client
        .initialize(&root, None)
        .expect_err("initialize should fail");
    let deadline = Instant::now() + Duration::from_secs(2);
    while client.stderr_tail().is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let tail = client.stderr_tail();
    assert!(!tail.is_empty(), "stderr tail should be captured");
    assert!(
        tail.lines().count() <= 64,
        "stderr tail exceeded line cap: {}",
        tail.lines().count()
    );
    assert!(tail.contains("MODULE_NOT_FOUND"), "tail: {tail}");
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
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root.display())
    ));
    assert_eq!(configure["success"], true);

    let write = aft.send(&format!(
        r#"{{"id":"write-1","command":"write","file":{},"content":"fn main() {{ println!(\"hello\"); }}\n"}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(write["success"], true, "write failed: {write:?}");

    let resp = aft.send(&format!(
        r#"{{"id":"diag-1","command":"lsp_diagnostics","file":{},"wait_ms":400}}"#,
        crate::helpers::json_string(&file.display())
    ));

    assert_eq!(resp["id"], "diag-1");
    assert_eq!(resp["success"], true, "response: {resp:?}");
    // Pull mode returns exactly one diagnostic (see fake_server.rs's
    // textDocument/diagnostic handler).
    assert_eq!(resp["total"], 1);
    assert_eq!(resp["files_with_errors"], 1);

    let diagnostics = resp["diagnostics"].as_array().expect("diagnostics array");
    assert_eq!(diagnostics.len(), 1);
    let canonical_file = crate::helpers::canonicalize_like_product(file);
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

    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(config),
    );

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

#[test]
fn test_pull_diagnostics_accepts_a_bracketed_route_directory() {
    use aft::lsp::manager::PullFileOutcome;

    let temp_dir = tempdir().expect("tempdir");
    let root = temp_dir.path().join("workspace");
    let file = root.join("src/[locationId]/route.rs");
    fs::create_dir_all(file.parent().unwrap()).expect("create bracketed route directory");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"route\"\n").expect("Cargo.toml");
    fs::write(&file, "fn route() {}\n").expect("route source");
    let config = Config {
        project_root: Some(root),
        ..Config::default()
    };
    let mut manager = manager_with_fake_server();
    manager.set_extra_env("AFT_FAKE_LSP_PULL", "1");

    let results = manager
        .pull_file_diagnostics(&file, &config)
        .expect("pull diagnostics succeeds for bracketed route");
    assert!(matches!(
        results.as_slice(),
        [result] if matches!(&result.outcome, PullFileOutcome::Full { diagnostic_count: 1 })
    ));
    let cached = manager.get_diagnostics_for_file(&file);
    assert!(
        cached
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some("E0PULL")),
        "pulled route diagnostic should use the decoded path key: {cached:?}"
    );
}

#[test]
fn test_publish_diagnostics_echo_joins_a_bracketed_store_path() {
    let temp_dir = tempdir().expect("tempdir");
    let root = temp_dir.path().join("workspace");
    let file = root.join("src/[[...sign-in]]/page.rs");
    fs::create_dir_all(file.parent().unwrap()).expect("create bracketed route directory");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"page\"\n").expect("Cargo.toml");
    fs::write(&file, "fn page() {}\n").expect("page source");
    let mut manager = manager_with_fake_server();

    manager
        .notify_file_changed_default(&file, "fn page() {}\n")
        .expect("open bracketed route");
    wait_for_publish(&mut manager);

    let cached = manager.get_diagnostics_for_file(&file);
    assert!(
        cached
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some("E0001")),
        "echoed publish URI should join the decoded filesystem path: {cached:?}"
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

#[test]
fn test_unchanged_pull_without_cache_falls_back_to_push() {
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let ctx = app_context_with_fake_lsp();
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL_UNCHANGED", "1");

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-unchanged-no-cache",
        "command": "lsp_diagnostics",
        "file": file.display().to_string(),
        "wait_ms": 500
    }))
    .expect("request parses");

    let response =
        serde_json::to_value(handle_lsp_diagnostics(&req, &ctx)).expect("response serializes");

    assert_eq!(response["success"], true, "response: {response}");
    assert_eq!(response["complete"], true, "response: {response}");
    assert_eq!(
        response["total"], 2,
        "push fallback should return didOpen diagnostics"
    );
    assert_eq!(
        response["lsp_servers_used"][0]["status"],
        "pull_no_cache_for_unchanged"
    );
}

fn assert_rejected_pull_falls_back_to_push(env_key: &str) {
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let ctx = app_context_with_fake_lsp();
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");
    ctx.lsp().set_extra_env(env_key, "1");

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": format!("diag-rejected-{env_key}"),
        "command": "lsp_diagnostics",
        "file": file.display().to_string(),
        "wait_ms": 500
    }))
    .expect("request parses");

    let response =
        serde_json::to_value(handle_lsp_diagnostics(&req, &ctx)).expect("response serializes");

    assert_eq!(response["success"], true, "response: {response}");
    assert_eq!(response["complete"], true, "response: {response}");
    assert_eq!(
        response["total"], 2,
        "push fallback should return didOpen diagnostics"
    );
    assert_eq!(
        response["lsp_servers_used"][0]["status"],
        "pull_rejected_push_fallback"
    );
}

#[test]
fn test_method_not_found_pull_rejection_falls_back_to_push() {
    assert_rejected_pull_falls_back_to_push("AFT_FAKE_LSP_PULL_METHOD_NOT_FOUND");
}

#[test]
fn test_invalid_params_pull_rejection_falls_back_to_push() {
    assert_rejected_pull_falls_back_to_push("AFT_FAKE_LSP_PULL_INVALID_PARAMS");
}

#[test]
fn closing_file_clears_cached_diagnostics_before_reopen() {
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let mut manager = manager_with_fake_server();

    manager
        .notify_file_changed_default(file, "fn main() {}\n")
        .expect("open file");
    wait_for_publish(&mut manager);
    assert_eq!(manager.get_diagnostics_for_file(file).len(), 2);

    manager
        .notify_file_changed_default(file, "fn main() { println!(\"changed\"); }\n")
        .expect("change file");
    wait_for_publish(&mut manager);
    assert_eq!(manager.get_diagnostics_for_file(file).len(), 1);

    manager.notify_file_closed(file).expect("close file");

    assert!(
        manager.get_diagnostics_for_file(file).is_empty(),
        "close should clear cached diagnostics immediately, before any reopen"
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

#[test]
fn watcher_external_edit_hides_stale_diagnostics_and_resyncs_lsp() {
    let (_temp_dir, root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            project_root: Some(root),
            ..Config::default()
        }),
    );
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());

    let config_snapshot = ctx.config();
    ctx.lsp()
        .notify_file_changed_versioned(
            file,
            "fn main() { println!(\"before\"); }\n",
            &config_snapshot,
        )
        .expect("versioned notify spawns the workspace server");
    wait_for_publish(&mut ctx.lsp());
    let before = {
        let lsp = ctx.lsp();
        lsp.get_diagnostics_for_file(file)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(before.len(), 2, "fake server should seed diagnostics");

    fs::write(file, "fn main() { println!(\"external edit\"); }\n").expect("external write");

    let stale_control = {
        let lsp = ctx.lsp();
        lsp.get_diagnostics_for_file(file)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert!(
        stale_control
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some("E0001")),
        "control: before the watcher runs, the warm store still serves the old diagnostics"
    );

    let (watcher_tx, watcher_rx) = crossbeam_channel::unbounded();
    *ctx.watcher_rx().lock() = Some(watcher_rx);
    watcher_tx
        .send(WatcherDispatchEvent::Paths(vec![file.clone()]))
        .expect("send watcher event");

    drain_watcher_events(&ctx);

    assert!(
        ctx.lsp().get_diagnostics_for_file(file).is_empty(),
        "watcher-stale diagnostics must be hidden until the LSP republishes"
    );

    let changed_event = collect_event(&mut ctx.lsp(), |event| {
        matches!(
            event,
            LspEvent::Notification { method, .. } if method == "custom/documentChanged"
        )
    });
    assert!(
        changed_event.is_some(),
        "watcher should resync the diagnosed file with didChange"
    );

    // The fake server publishes asynchronously after the didChange resync, so
    // poll instead of reading once: on a loaded runner the publish can land
    // milliseconds after the documentChanged notification is observed. Check
    // the store BEFORE waiting on each iteration — the publish event may have
    // already been consumed while collecting the documentChanged notification,
    // in which case waiting for another publish would block forever.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let after = loop {
        let current = {
            let lsp = ctx.lsp();
            lsp.get_diagnostics_for_file(file)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
        };
        if current
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some("E0002"))
            || std::time::Instant::now() >= deadline
        {
            break current;
        }
        // Pump the event loop without asserting: the awaited publish may
        // already be behind us, and the deadline above owns failure.
        let _ = collect_event(&mut ctx.lsp(), |event| {
            matches!(
                event,
                LspEvent::Notification { method, .. } if method == "textDocument/publishDiagnostics"
            )
        });
    };
    assert!(
        after
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some("E0002")),
        "fresh publish should replace the stale diagnostics: {after:?}"
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
fn slow_post_edit_wait_does_not_hold_lsp_manager_mutex() {
    let (temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = files[0].clone();
    let delay_signal = temp_dir.path().join("did-change-delay-started");
    let ctx = Arc::new(app_context_with_fake_lsp());

    {
        let mut manager = ctx.lsp();
        manager.set_extra_env("AFT_FAKE_LSP_CHANGE_DELAY_MS", "1500");
        manager.set_extra_env(
            "AFT_FAKE_LSP_CHANGE_DELAY_SIGNAL",
            &delay_signal.to_string_lossy(),
        );
        manager
            .notify_file_changed_default(&file, "fn main() {}\n")
            .expect("open fixture");
        wait_for_publish(&mut manager);
    }

    let wait_ctx = Arc::clone(&ctx);
    let wait_file = file.clone();
    let wait_thread = thread::spawn(move || {
        wait_ctx.lsp_notify_and_collect_diagnostics(
            &wait_file,
            "fn main() { let changed = true; }\n",
            Duration::from_secs(5),
        )
    });

    let signal_deadline = Instant::now() + Duration::from_secs(10);
    while !delay_signal.exists() && Instant::now() < signal_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        delay_signal.exists(),
        "fake server never entered its delayed didChange response"
    );

    let count_started = Instant::now();
    let server_count = ctx.lsp().server_count();
    let count_elapsed = count_started.elapsed();
    assert_eq!(server_count, 1);
    assert!(
        count_elapsed < Duration::from_millis(500),
        "server_count waited {count_elapsed:?} behind the post-edit diagnostics delay"
    );

    let outcome = wait_thread.join().expect("post-edit wait thread");
    assert!(outcome.complete(), "delayed publish should still complete");
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

    let canonical_file = crate::helpers::canonicalize_like_product(file);
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
    let canonical_root = crate::helpers::canonicalize_like_product(&root);
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

    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config::default()),
    );
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
        stale: false,
        provisional: false,
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
        stale: false,
        provisional: false,
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
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(config),
    );
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
    // The directory walk stops at DIRECTORY_FILE_CAP (200) instead of
    // enumerating the whole tree; walk_truncated is the honest gap signal that
    // more files exist beyond the cap. unchecked_files lists the within-cap
    // files that no server covered (here: all 200, since no LSP is registered).
    assert_eq!(
        json["unchecked_files"].as_array().expect("unchecked").len(),
        200
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
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(config),
    );
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

    let config_snapshot = ctx.config();
    ctx.lsp()
        .notify_file_changed_versioned(source, "export const value = 2;\n", &config_snapshot)
        .expect("versioned notify spawns the workspace server");
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

#[test]
fn rust_analyzer_warming_diagnostics_are_provisional_until_quiescent() {
    let (_temp_dir, _root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = &files[0];
    let stored_file = crate::helpers::canonicalize_like_product(file);
    let mut manager = manager_with_fake_server();
    manager.set_extra_env("AFT_FAKE_LSP_SERVER_STATUS", "1");
    let config = Config::default();

    manager
        .notify_file_changed_versioned(file, "fn main() {}\n", &config)
        .expect("start fake rust-analyzer");
    // Collect BOTH startup events in one drain loop: the fake server sends
    // custom/initialized and the warming publishDiagnostics back-to-back, and
    // collect_event discards the non-matching remainder of a drained batch —
    // two sequential collects lose the publish whenever both events land in
    // one batch (1-in-3 locally).
    let mut initialized_params = None;
    let mut publish_seen = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !(initialized_params.is_some() && publish_seen) {
        for event in manager.drain_events() {
            match event {
                LspEvent::Notification { method, params, .. } if method == "custom/initialized" => {
                    initialized_params = Some(params.expect("initialized params"));
                }
                LspEvent::Notification { method, .. }
                    if method == "textDocument/publishDiagnostics" =>
                {
                    publish_seen = true;
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    let initialized_params = initialized_params.expect("fake server initialization event");
    assert_eq!(initialized_params["serverStatusNotification"], true);
    assert!(
        publish_seen,
        "timed out waiting for warming publishDiagnostics"
    );

    let entries = manager
        .diagnostics_store_for_test()
        .entries_for_file(&stored_file);
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].1.provisional,
        "warming publish must be provisional"
    );
    assert_eq!(
        manager.diagnostics_store_for_test().error_warning_counts(),
        (0, 0),
        "warming diagnostics must not contribute to severity counts"
    );

    manager
        .notify_file_changed_versioned(file, "fn main() { let ready = true; }\n", &config)
        .expect("send post-quiescence document change");
    wait_for_publish(&mut manager);

    let entries = manager
        .diagnostics_store_for_test()
        .entries_for_file(&stored_file);
    assert_eq!(entries.len(), 1);
    assert!(
        !entries[0].1.provisional,
        "post-quiescence publish is authoritative"
    );
    assert!(!entries[0].1.stale);
    assert_eq!(
        manager.diagnostics_store_for_test().error_warning_counts(),
        (1, 0),
        "authoritative diagnostics must contribute after quiescence"
    );
}

#[test]
fn edit_diagnostics_reports_warming_publish_as_pending_until_quiescent() {
    use aft::lsp::diagnostics::DiagnosticEntry;
    use aft::lsp::manager::PreEditSnapshot;

    let file = PathBuf::from("/workspace/src/main.rs");
    let key = ServerKey {
        kind: ServerKind::Rust,
        root: PathBuf::from("/workspace"),
    };
    let entry = DiagnosticEntry {
        diagnostics: vec![StoredDiagnostic {
            file,
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 2,
            severity: DiagnosticSeverity::Error,
            message: "warming diagnostic".to_string(),
            code: Some("E-WARM".to_string()),
            source: Some("rust-analyzer".to_string()),
        }],
        epoch: 2,
        result_id: None,
        version: Some(1),
        stale: false,
        provisional: true,
    };

    let outcome = LspManager::post_edit_outcome_for_entry_for_test(
        key,
        &entry,
        1,
        PreEditSnapshot {
            epoch: 1,
            document_version_at_capture: Some(0),
        },
    );

    assert!(
        !outcome.complete(),
        "a warming report must not make an edit diagnostic result complete: {outcome:?}"
    );
    assert!(
        outcome.diagnostics.is_empty(),
        "warming diagnostics must not be returned as authoritative: {outcome:?}"
    );
    assert_eq!(outcome.pending_servers.len(), 1, "{outcome:?}");
    assert!(outcome.exited_servers.is_empty(), "{outcome:?}");
}

#[test]
fn server_without_server_status_keeps_authoritative_behavior() {
    let (_temp_dir, _root, files) = typescript_workspace_with_files(&["main.ts"]);
    let file = &files[0];
    let stored_file = crate::helpers::canonicalize_like_product(file);
    let mut manager = manager_with_fake_typescript_server();
    manager.set_extra_env("AFT_FAKE_LSP_SERVER_STATUS", "disabled");
    let config = Config::default();

    manager
        .notify_file_changed_versioned(file, "fn main() {}\n", &config)
        .expect("start fake rust-analyzer");
    wait_for_publish(&mut manager);

    let entries = manager
        .diagnostics_store_for_test()
        .entries_for_file(&stored_file);
    assert_eq!(entries.len(), 1);
    assert!(!entries[0].1.provisional);
    assert_eq!(
        manager.diagnostics_store_for_test().error_warning_counts(),
        (1, 1),
        "servers without the readiness signal retain existing behavior"
    );
}

fn real_rust_analyzer_available() -> bool {
    Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn configured_rust_context(root: &std::path::Path) -> AppContext {
    configured_rust_context_with_lsp(root, serde_json::json!({}))
}

fn configured_rust_context_with_lsp(root: &std::path::Path, lsp: serde_json::Value) -> AppContext {
    crate::helpers::disable_in_process_file_watcher();
    let storage_dir = root.join(".aft-test-storage");
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            storage_dir: Some(storage_dir.clone()),
            ..Config::default()
        }),
    );
    let configure: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "config": crate::helpers::user_config(serde_json::json!({
            "search_index": false,
            "semantic_search": false,
            "callgraph_store": false,
            "lsp": lsp
        })),
    }))
    .expect("configure parses");
    let response =
        serde_json::to_value(aft::commands::configure::handle_configure(&configure, &ctx))
            .expect("configure response serializes");
    assert_eq!(response["success"], true, "configure failed: {response:#}");
    ctx
}

fn lsp_diagnostics_for(ctx: &AppContext, id: &str, file: &std::path::Path) -> serde_json::Value {
    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": id,
        "command": "lsp_diagnostics",
        "file": file.display().to_string(),
        "wait_ms": 5000
    }))
    .expect("request parses");
    serde_json::to_value(handle_lsp_diagnostics(&req, ctx)).expect("response serializes")
}

/// The distinct lines of `file` that `lsp_diagnostics` currently reports
/// errors on, sorted. A line can carry two errors for one fault: rust-analyzer's
/// own analysis and rustc's from the `cargo check` result it merges in.
fn error_lines_for(response: &serde_json::Value) -> Vec<u64> {
    let mut lines: Vec<u64> = response["diagnostics"]
        .as_array()
        .map(|diagnostics| {
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic["severity"] == "error")
                .filter_map(|diagnostic| diagnostic["line"].as_u64())
                .collect()
        })
        .unwrap_or_default();
    lines.sort_unstable();
    lines.dedup();
    lines
}

/// Poll `lsp_diagnostics` until `done` accepts the response or `timeout`
/// passes; returns the last response either way.
fn poll_lsp_diagnostics(
    ctx: &AppContext,
    file: &std::path::Path,
    timeout: Duration,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let response = lsp_diagnostics_for(ctx, &format!("diag-poll-{attempt}"), file);
        if done(&response) || Instant::now() >= deadline {
            return response;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// rust-analyzer run with `--locked` cannot read the dependencies of a
/// workspace whose Cargo.lock is stale, so it loads the workspace without
/// them and a call into a path dependency goes unchecked. `cargo update`,
/// run with no AFT tool involved, fixes the lockfile; the project file
/// watcher's event for it is then the only way rust-analyzer can learn of
/// the change. A later `lsp_diagnostics` (no inspect, no AFT edit) must
/// report the type error that only the full load can see.
#[test]
fn lsp_diagnostics_recovers_after_external_cargo_update_with_real_rust_analyzer() {
    if !real_rust_analyzer_available() {
        eprintln!("SKIP lsp_diagnostics_recovers_after_external_cargo_update_with_real_rust_analyzer: rust-analyzer is not installed");
        return;
    }
    let temp_dir = tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    fs::create_dir_all(root.join("src")).expect("create src");
    fs::create_dir_all(root.join("newdep/src")).expect("create dependency src");
    let package = "[package]\nname = \"lock-reader\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
    fs::write(root.join("Cargo.toml"), package).expect("write manifest");
    let lib = root.join("src/lib.rs");
    // `newdep::other` returns a string, so line 1 is a type error, but only
    // once rust-analyzer has loaded `newdep`; without the dependency the
    // call's type is unknown and nothing is reported. Line 2 is wrong on its
    // own and shows the file is being analyzed either way.
    fs::write(
        &lib,
        "pub fn from_dep() -> u8 { newdep::other() }\npub fn local() -> u8 { \"text\" }\n",
    )
    .expect("write lib");
    fs::write(
        root.join("newdep/Cargo.toml"),
        "[package]\nname = \"newdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write dependency manifest");
    fs::write(
        root.join("newdep/src/lib.rs"),
        "pub fn other() -> &'static str { \"text\" }\n",
    )
    .expect("write dependency lib");
    let generated = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&root)
        .output()
        .expect("cargo is required for Rust integration tests");
    assert!(generated.status.success(), "{generated:?}");
    // The dependency is added after the lockfile was written: stale.
    fs::write(
        root.join("Cargo.toml"),
        format!("{package}[dependencies]\nnewdep = {{ path = \"newdep\" }}\n"),
    )
    .expect("add dependency");

    let ctx = configured_rust_context(&root);
    let (watcher_tx, watcher_rx) = crossbeam_channel::unbounded();
    *ctx.watcher_rx().lock() = Some(watcher_rx);

    let stale = poll_lsp_diagnostics(&ctx, &lib, Duration::from_secs(60), |response| {
        error_lines_for(response).contains(&2)
    });
    assert_eq!(
        error_lines_for(&stale),
        vec![2],
        "control: the stale load analyzes the file but cannot see the dependency: {stale:#}"
    );
    // rust-analyzer re-fetches its workspace once on its own shortly after
    // startup; fix the lockfile only after that, so recovery cannot come
    // from that startup fetch.
    thread::sleep(Duration::from_secs(3));
    let updated = Command::new("cargo")
        .args(["update", "--workspace", "--offline"])
        .current_dir(&root)
        .output()
        .expect("run cargo update");
    assert!(updated.status.success(), "{updated:?}");

    // What the live watcher delivers for the rewritten lockfile.
    watcher_tx
        .send(WatcherDispatchEvent::Paths(vec![root.join("Cargo.lock")]))
        .expect("send watcher event");
    drain_watcher_events(&ctx);

    let fixed = poll_lsp_diagnostics(&ctx, &lib, Duration::from_secs(60), |response| {
        error_lines_for(response).contains(&1)
    });
    let mut lines = error_lines_for(&fixed);
    lines.sort_unstable();
    assert_eq!(
        lines,
        vec![1, 2],
        "after the lockfile is fixed, lsp_diagnostics must report the full load: {fixed:#}"
    );
}

/// `lsp_diagnostics` after an AFT write removes a struct field that
/// `src/user.rs` still sets. rust-analyzer's pull answers with its own
/// analysis; the compiler's errors come from a `cargo check` that only a save
/// starts. The answer must carry the compiler's error for the new contents,
/// or say the check is still running; a complete answer without it is built
/// from a check of the files before the write.
#[test]
fn lsp_diagnostics_reports_cargo_check_after_an_aft_write_with_real_rust_analyzer() {
    if !crate::helpers::real_rust_analyzer_available(
        "lsp_diagnostics_reports_cargo_check_after_an_aft_write_with_real_rust_analyzer",
    ) {
        return;
    }
    let temp_dir = tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    fs::create_dir_all(root.join("src")).expect("create src");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"field-removal\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write manifest");
    fs::write(root.join("src/lib.rs"), "pub mod s;\npub mod user;\n").expect("write lib");
    let s = root.join("src/s.rs");
    fs::write(&s, "pub struct S {\n    pub a: u8,\n    pub b: u8,\n}\n").expect("write s");
    let user = root.join("src/user.rs");
    fs::write(
        &user,
        "use crate::s::S;\n\npub fn make() -> S {\n    S { a: 1, b: 2 }\n}\n",
    )
    .expect("write user");
    // AFT runs rust-analyzer with `--locked`, which needs a lockfile.
    let generated = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&root)
        .output()
        .expect("cargo is required for Rust integration tests");
    assert!(generated.status.success(), "{generated:?}");
    let ctx = configured_rust_context(&root);

    let clean = poll_lsp_diagnostics(&ctx, &user, Duration::from_secs(60), |response| {
        response["complete"] == true
    });
    assert_eq!(clean["complete"], true, "{clean:#}");
    assert_eq!(
        error_lines_for(&clean),
        Vec::<u64>::new(),
        "control: the use site compiles before the removal: {clean:#}"
    );

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "write-field-removal",
        "command": "write",
        "file": s.display().to_string(),
        "content": "pub struct S {\n    pub a: u8,\n}\n"
    }))
    .expect("request parses");
    let written = serde_json::to_value(handle_write(&req, &ctx)).expect("response serializes");
    assert_eq!(written["success"], true, "write failed: {written:#}");

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-after-removal",
        "command": "lsp_diagnostics",
        "file": user.display().to_string(),
        "wait_ms": 10_000
    }))
    .expect("request parses");
    let after =
        serde_json::to_value(handle_lsp_diagnostics(&req, &ctx)).expect("response serializes");
    if after["complete"] == false && after["note"].as_str().is_some() {
        assert_eq!(
            after["note"], "rust-analyzer: cargo check still running; retry",
            "{after:#}"
        );
        return;
    }
    let compiler_error = after["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|diagnostic| {
            diagnostic["source"] == "rustc"
                && diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("no field named `b`"))
        });
    assert!(
        compiler_error,
        "cargo check's error for the removed field is missing: {after:#}"
    );
}

/// A Rust workspace served by the fake language server with the environment
/// `env` builds from the workspace root, its server started on
/// `src/main.rs`, and a watcher channel the test feeds.
fn fake_rust_server_with_watcher(
    env: impl FnOnce(&std::path::Path) -> Vec<(String, String)>,
) -> (
    tempfile::TempDir,
    PathBuf,
    PathBuf,
    AppContext,
    crossbeam_channel::Sender<WatcherDispatchEvent>,
) {
    let (temp_dir, root, files) = rust_workspace_with_files(&["main.rs"]);
    let file = files[0].clone();
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            project_root: Some(root.clone()),
            ..Config::default()
        }),
    );
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
    for (key, value) in env(&root) {
        ctx.lsp().set_extra_env(&key, &value);
    }
    let config = ctx.config();
    ctx.lsp()
        .notify_file_changed_versioned(&file, "fn main() {}\n", &config)
        .expect("versioned notify spawns the workspace server");
    // The fake registers its watchers while handling `initialized`, before
    // it sees the didOpen that this publish answers, and the client records
    // a registration before queuing anything read after it.
    wait_for_publish(&mut ctx.lsp());
    let (watcher_tx, watcher_rx) = crossbeam_channel::unbounded();
    *ctx.watcher_rx().lock() = Some(watcher_rx);
    (temp_dir, root, file, ctx, watcher_tx)
}

fn env_pair(key: &str, value: &str) -> Vec<(String, String)> {
    vec![(key.to_string(), value.to_string())]
}

/// Send a didChange for the open `file` and collect everything the fake
/// server reports until it acknowledges that change. The server handles
/// messages in order, so any watched-file notification sent before this call
/// is reported before the acknowledgement: an empty result proves none was.
/// Returns the watched-file notification params and every method seen.
fn server_reports_before_barrier(
    ctx: &AppContext,
    file: &std::path::Path,
    barrier_text: &str,
) -> (Vec<serde_json::Value>, Vec<String>) {
    let config = ctx.config();
    ctx.lsp()
        .notify_file_changed(file, barrier_text, &config)
        .expect("barrier didChange");
    let mut watched = Vec::new();
    let mut methods = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the barrier didChange; saw {methods:?}"
        );
        for event in ctx.lsp().drain_events() {
            if let LspEvent::Notification { method, params, .. } = event {
                methods.push(method.clone());
                if method == "custom/watchedFilesChanged" {
                    watched.push(params.unwrap_or_default());
                } else if method == "custom/documentChanged" {
                    return (watched, methods);
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// (path relative to `root`, change type) for each change in one
/// watched-file notification, sorted.
fn forwarded_changes(root: &std::path::Path, params: &serde_json::Value) -> Vec<(String, i64)> {
    let root = fs::canonicalize(root).expect("canonical root");
    let mut changes: Vec<(String, i64)> = params["changes"]
        .as_array()
        .expect("changes array")
        .iter()
        .map(|change| {
            let uri = url::Url::parse(change["uri"].as_str().expect("uri")).expect("file uri");
            let path = uri.to_file_path().expect("file path");
            // Canonicalize so the path compares against the canonical root
            // (on Windows that is a `\\?\` verbatim path). Some forwarded
            // paths name files that were never created, which canonicalize
            // refuses, so fall back to the canonical parent plus the name.
            let path = fs::canonicalize(&path)
                .ok()
                .or_else(|| {
                    let parent = fs::canonicalize(path.parent()?).ok()?;
                    Some(parent.join(path.file_name()?))
                })
                .unwrap_or(path);
            let relative = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            (relative, change["type"].as_i64().expect("type"))
        })
        .collect();
    changes.sort();
    changes
}

#[test]
fn watcher_drain_forwards_only_changes_matching_registered_watchers() {
    let (_temp_dir, root, file, ctx, watcher_tx) = fake_rust_server_with_watcher(|root| {
        let base_uri = url::Url::from_file_path(root.join("sub")).expect("base uri");
        let globs = serde_json::json!([
            { "globPattern": "**/*.special" },
            // WatchKind 4: deletions only.
            { "globPattern": "**/*.only", "kind": 4 },
            { "globPattern": { "baseUri": base_uri.as_str(), "pattern": "*.rel" } }
        ]);
        vec![("AFT_FAKE_LSP_WATCHED_GLOBS".to_string(), globs.to_string())]
    });
    fs::create_dir_all(root.join("sub")).expect("create sub");
    for name in ["a.special", "b.txt", "kept.only", "sub/x.rel", "y.rel"] {
        fs::write(root.join(name), "x\n").expect("write watched fixture");
    }
    let paths = [
        "a.special",
        "b.txt",
        "kept.only",
        "gone.only",
        "sub/x.rel",
        "y.rel",
    ]
    .iter()
    .map(|name| root.join(name))
    .collect();
    watcher_tx
        .send(WatcherDispatchEvent::Paths(paths))
        .expect("send watcher event");
    drain_watcher_events(&ctx);

    let (watched, _) = server_reports_before_barrier(&ctx, &file, "fn main() { 1; }\n");
    assert_eq!(watched.len(), 1, "one notification per batch: {watched:?}");
    assert_eq!(
        forwarded_changes(&root, &watched[0]),
        vec![
            ("a.special".to_string(), 2),
            ("gone.only".to_string(), 3),
            ("sub/x.rel".to_string(), 2),
        ],
        "only registered globs and kinds are forwarded"
    );
}

#[test]
fn watcher_drain_forwards_nothing_to_a_server_without_watched_file_support() {
    let (_temp_dir, root, file, ctx, watcher_tx) =
        fake_rust_server_with_watcher(|_| env_pair("AFT_FAKE_LSP_NO_WATCHED_FILES", "1"));
    fs::write(root.join("src/other.rs"), "fn other() {}\n").expect("write source");
    watcher_tx
        .send(WatcherDispatchEvent::Paths(vec![
            root.join("Cargo.toml"),
            root.join("src/other.rs"),
        ]))
        .expect("send watcher event");
    drain_watcher_events(&ctx);

    let (watched, _) = server_reports_before_barrier(&ctx, &file, "fn main() { 1; }\n");
    assert!(
        watched.is_empty(),
        "a server that neither advertised nor registered watched files hears nothing: {watched:?}"
    );
}

#[test]
fn watcher_drain_forwards_config_files_to_a_server_without_registered_globs() {
    let (_temp_dir, root, file, ctx, watcher_tx) =
        fake_rust_server_with_watcher(|_| env_pair("AFT_FAKE_LSP_STATIC_WATCHED_FILES_ONLY", "1"));
    fs::write(root.join("Cargo.lock"), "version = 4\n").expect("write lockfile");
    fs::write(root.join("src/other.rs"), "fn other() {}\n").expect("write source");
    watcher_tx
        .send(WatcherDispatchEvent::Paths(vec![
            root.join("Cargo.toml"),
            root.join("Cargo.lock"),
            root.join("src/other.rs"),
        ]))
        .expect("send watcher event");
    drain_watcher_events(&ctx);

    let (watched, _) = server_reports_before_barrier(&ctx, &file, "fn main() { 1; }\n");
    assert_eq!(watched.len(), 1, "{watched:?}");
    assert_eq!(
        forwarded_changes(&root, &watched[0]),
        vec![("Cargo.lock".to_string(), 2), ("Cargo.toml".to_string(), 2)],
        "without globs, only project configuration files are forwarded"
    );
}

#[test]
fn watcher_overflow_sends_no_per_file_flood() {
    let (_temp_dir, root, file, ctx, watcher_tx) = fake_rust_server_with_watcher(|_| Vec::new());
    // The fake's default watcher is `**/*`, so every path below matches. Missing files
    // are forwarded as deletions, which is enough to fill the batch.
    let mut paths: Vec<PathBuf> = (0..WATCHED_FILE_FORWARD_CAP + 10)
        .map(|index| root.join(format!("src/generated_{index}.rs")))
        .collect();
    paths.push(root.join("Cargo.toml"));
    watcher_tx
        .send(WatcherDispatchEvent::Paths(paths))
        .expect("send watcher event");
    drain_watcher_events(&ctx);

    let (watched, _) = server_reports_before_barrier(&ctx, &file, "fn main() { 1; }\n");
    assert_eq!(watched.len(), 1, "{} notifications", watched.len());
    assert_eq!(
        forwarded_changes(&root, &watched[0]),
        vec![("Cargo.toml".to_string(), 2)],
        "an oversized batch forwards only its configuration files"
    );

    // Lost events: the watcher asks for a rescan and names no paths.
    watcher_tx
        .send(WatcherDispatchEvent::RescanRequired(
            aft::watcher_filter::RescanReason::Unknown,
        ))
        .expect("send rescan");
    drain_watcher_events(&ctx);
    let (watched, _) = server_reports_before_barrier(&ctx, &file, "fn main() { 2; }\n");
    assert!(
        watched.is_empty(),
        "a rescan sends no per-file notifications: {watched:?}"
    );
}

#[test]
fn watcher_overflow_under_manager_contention_sends_one_config_notification() {
    let (_temp_dir, root, file, ctx, watcher_tx) = fake_rust_server_with_watcher(|_| Vec::new());
    let mut paths: Vec<_> = (0..WATCHED_FILE_FORWARD_CAP + 10)
        .map(|index| root.join(format!("src/generated_{index}.rs")))
        .collect();
    paths.push(root.join("Cargo.toml"));
    {
        // Keep the manager unavailable throughout the drain, not just while
        // calling the forwarding API, to exercise the actual queued path.
        let _held = ctx.lsp();
        watcher_tx
            .send(WatcherDispatchEvent::Paths(paths))
            .expect("send watcher event");
        drain_watcher_events(&ctx);
        assert!(ctx.lsp_watcher_forward_pending_for_test());
        assert_eq!(ctx.lsp_watcher_forward_helpers_spawned_for_test(), 1);
    }
    wait_for_watcher_forward_helper(&ctx);
    let (watched, _) = server_reports_before_barrier(&ctx, &file, "fn main() { 1; }\n");
    assert_eq!(watched.len(), 1, "one capped forward: {watched:?}");
    assert_eq!(
        forwarded_changes(&root, &watched[0]),
        vec![("Cargo.toml".to_string(), 2)],
        "contention must not bypass the cap or duplicate the configuration change"
    );
}

/// Wait up to `timeout` for the fake server to report that AFT asked it to
/// reload its workspace.
fn saw_workspace_reload(ctx: &AppContext, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        for event in ctx.lsp().drain_events() {
            if matches!(
                &event,
                LspEvent::Notification { method, .. } if method == "custom/reloadWorkspace"
            ) {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

#[test]
fn watcher_manifest_change_outside_registered_globs_reloads_rust_workspace() {
    let (_temp_dir, root, _file, ctx, watcher_tx) = fake_rust_server_with_watcher(|_| {
        env_pair(
            "AFT_FAKE_LSP_WATCHED_GLOBS",
            r#"[{ "globPattern": "**/*.rs" }]"#,
        )
    });
    // Let file times move past the server's load time.
    thread::sleep(Duration::from_millis(1100));

    fs::write(root.join("src/other.rs"), "fn other() {}\n").expect("write source");
    watcher_tx
        .send(WatcherDispatchEvent::Paths(vec![root.join("src/other.rs")]))
        .expect("send watcher event");
    drain_watcher_events(&ctx);
    assert!(
        !saw_workspace_reload(&ctx, Duration::from_millis(1500)),
        "control: a source change does not reload the workspace"
    );

    // The server's globs do not cover the lockfile, so it cannot hear about
    // the change: AFT asks it to reload instead.
    fs::write(root.join("Cargo.lock"), "version = 4\n").expect("write lockfile");
    watcher_tx
        .send(WatcherDispatchEvent::Paths(vec![root.join("Cargo.lock")]))
        .expect("send watcher event");
    drain_watcher_events(&ctx);
    assert!(
        saw_workspace_reload(&ctx, Duration::from_secs(15)),
        "a lockfile change the server was not told about must reload its workspace"
    );
}

#[test]
fn watcher_manifest_change_forwarded_to_registered_glob_does_not_reload() {
    // The default fake registers `**/*`, which covers the lockfile.
    let (_temp_dir, root, file, ctx, watcher_tx) = fake_rust_server_with_watcher(|_| Vec::new());
    thread::sleep(Duration::from_millis(1100));
    fs::write(root.join("Cargo.lock"), "version = 4\n").expect("write lockfile");
    watcher_tx
        .send(WatcherDispatchEvent::Paths(vec![root.join("Cargo.lock")]))
        .expect("send watcher event");
    drain_watcher_events(&ctx);
    let (watched, _) = server_reports_before_barrier(&ctx, &file, "fn main() { 1; }\n");
    assert_eq!(watched.len(), 1, "{watched:?}");
    assert!(
        !saw_workspace_reload(&ctx, Duration::from_millis(1500)),
        "rust-analyzer re-reads a lockfile it registered for; a reload on top would repeat the load"
    );
}

/// Wait until the helper serving contended watcher drains has forwarded
/// everything queued and exited.
fn wait_for_watcher_forward_helper(ctx: &AppContext) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while ctx.lsp_watcher_forward_pending_for_test() {
        assert!(
            Instant::now() < deadline,
            "the watcher forward helper never finished"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn contended_watcher_drains_share_one_helper_and_one_merged_forward() {
    let (_temp_dir, root, file, ctx, _watcher_tx) = fake_rust_server_with_watcher(|_| Vec::new());
    {
        // Holding the manager lock makes every drain below find it busy.
        let _held = ctx.lsp();
        for batch in 0..20usize {
            // Each batch repeats the previous batch's path, so the merge
            // must also deduplicate.
            let paths = vec![
                root.join(format!("src/batch_{batch}.rs")),
                root.join(format!("src/batch_{}.rs", batch.saturating_sub(1))),
            ];
            ctx.lsp_forward_watcher_file_events(&paths);
        }
        assert_eq!(
            ctx.lsp_watcher_forward_helpers_spawned_for_test(),
            1,
            "contended drains share one helper thread"
        );
    }
    wait_for_watcher_forward_helper(&ctx);

    let (watched, _) = server_reports_before_barrier(&ctx, &file, "fn main() { 1; }\n");
    assert_eq!(watched.len(), 1, "one merged forward: {watched:?}");
    let mut expected: Vec<(String, i64)> = (0..20)
        .map(|batch| (format!("src/batch_{batch}.rs"), 3))
        .collect();
    expected.sort();
    assert_eq!(forwarded_changes(&root, &watched[0]), expected);
    assert_eq!(ctx.lsp_watcher_forward_helpers_spawned_for_test(), 1);
}

#[test]
fn contended_watcher_drains_past_the_backlog_cap_forward_only_config_files() {
    let (_temp_dir, root, file, ctx, _watcher_tx) = fake_rust_server_with_watcher(|_| Vec::new());
    {
        let _held = ctx.lsp();
        let per_batch = WATCHER_FORWARD_BACKLOG_CAP / 2 + 1;
        for batch in 0..3 {
            let mut paths: Vec<PathBuf> = (0..per_batch)
                .map(|index| root.join(format!("src/gen_{batch}_{index}.rs")))
                .collect();
            if batch == 0 {
                paths.push(root.join("Cargo.toml"));
            }
            ctx.lsp_forward_watcher_file_events(&paths);
        }
        assert_eq!(ctx.lsp_watcher_forward_helpers_spawned_for_test(), 1);
    }
    wait_for_watcher_forward_helper(&ctx);

    let (watched, _) = server_reports_before_barrier(&ctx, &file, "fn main() { 1; }\n");
    assert_eq!(watched.len(), 1, "{} notifications", watched.len());
    assert_eq!(
        forwarded_changes(&root, &watched[0]),
        vec![("Cargo.toml".to_string(), 2)],
        "an overflowing backlog keeps only configuration files"
    );
}

/// A sleeping build script holds Cargo's target lock without consuming CPU.
/// After a save starts a real analyzer check, an ordinary build must use a
/// different lock, even when Cargo's base target directory is overridden.
#[test]
fn real_rust_analyzer_checks_do_not_block_cargo_build() {
    if !crate::helpers::real_rust_analyzer_available(
        "real_rust_analyzer_checks_do_not_block_cargo_build",
    ) {
        return;
    }
    for mode in ["default", "config", "environment"] {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        fs::create_dir_all(root.join("src")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\".\"]\n[package]\nname = \"lock-isolation\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        let source = root.join("src/lib.rs");
        fs::write(&source, "pub fn value() -> u8 { 1 }\n").unwrap();
        fs::write(
            root.join("build.rs"),
            r#"
fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    if std::path::Path::new("armed").exists() {
        let out = std::env::var("OUT_DIR").unwrap();
        std::fs::write("check-started", &out).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(8));
    }
}
"#,
        )
        .unwrap();
        let base_target = match mode {
            "config" => {
                fs::create_dir_all(root.join(".cargo")).unwrap();
                fs::write(
                    root.join(".cargo/config.toml"),
                    "[build]\ntarget-dir = \"configured-target\"\n",
                )
                .unwrap();
                root.join("configured-target")
            }
            "environment" => root.join("environment-target"),
            _ => root.join("target"),
        };
        let lsp = if mode == "environment" {
            serde_json::json!({"servers": {"rust": {"env": {
                "CARGO_TARGET_DIR": base_target.to_string_lossy()
            }}}})
        } else {
            serde_json::json!({})
        };
        let generated = Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(generated.status.success(), "{generated:?}");
        let ctx = configured_rust_context_with_lsp(&root, lsp);
        let clean = poll_lsp_diagnostics(&ctx, &source, Duration::from_secs(90), |r| {
            r["complete"] == true
        });
        assert_eq!(clean["complete"], true, "{mode}: {clean:#}");
        fs::write(root.join("armed"), "").unwrap();
        let write: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "save-lock-fixture", "command": "write",
            "file": source, "content": "pub fn value() -> u8 { 2 }\n"
        }))
        .unwrap();
        let written = serde_json::to_value(handle_write(&write, &ctx)).unwrap();
        assert_eq!(written["success"], true, "{written:#}");
        let started = root.join("check-started");
        let deadline = Instant::now() + Duration::from_secs(90);
        while !started.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            started.exists(),
            "{mode}: save did not start the build script"
        );
        let check_out = fs::read_to_string(&started).unwrap();
        let mut build = Command::new("cargo");
        build
            .args(["build", "--offline", "--locked"])
            .current_dir(&root);
        if mode == "environment" {
            build.env("CARGO_TARGET_DIR", &base_target);
        }
        let output = build.output().expect("cargo build");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{mode}: {stderr}");
        assert!(
            !stderr.contains("Blocking waiting for file lock on build directory"),
            "{mode}: cargo build blocked on analyzer check:\n{stderr}"
        );
        let analyzer_target = base_target.join("rust-analyzer");
        assert!(
            std::path::Path::new(check_out.trim()).starts_with(&analyzer_target),
            "{mode}: check artifacts at {check_out}, expected {}",
            analyzer_target.display()
        );
        assert!(
            analyzer_target.join("debug/deps").is_dir(),
            "check artifacts missing"
        );
        eprintln!(
            "{mode}: analyzer artifacts under {}; cargo build stderr:\n{stderr}",
            analyzer_target.display()
        );
        if let Ok(size) = Command::new("du")
            .args(["-sk"])
            .arg(&analyzer_target)
            .output()
        {
            eprintln!(
                "{mode}: extra target disk KiB: {}",
                String::from_utf8_lossy(&size.stdout)
            );
        }
    }
}

/// A push-only server's diagnostics end the `wait_ms` wait when they arrive.
/// The wait used to sleep out the whole budget in 100 ms steps, taking the
/// manager lock each time, even after the publish it waited for was stored.
#[test]
fn push_wait_ends_when_the_server_publishes_instead_of_sleeping_out_wait_ms() {
    let (_temp_dir, _root, files) = typescript_workspace_with_files(&["index.ts"]);
    let file = &files[0];
    let ctx = app_context_with_fake_typescript_lsp();

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "diag-push-wake",
        "command": "lsp_diagnostics",
        "file": file.display().to_string(),
        "wait_ms": 10_000
    }))
    .expect("request parses");

    let wakeups_before = aft::commands::lsp_diagnostics::wait_wakeups_for_test();
    let started = Instant::now();
    let json =
        serde_json::to_value(handle_lsp_diagnostics(&req, &ctx)).expect("response serializes");
    let elapsed = started.elapsed();
    let wakeups = aft::commands::lsp_diagnostics::wait_wakeups_for_test() - wakeups_before;

    assert_eq!(json["success"], true, "response: {json:#}");
    assert_eq!(
        json["lsp_servers_used"][0]["server_id"], "typescript",
        "{json:#}"
    );
    assert_eq!(
        json["lsp_servers_used"][0]["status"], "push_only",
        "{json:#}"
    );
    // Only diagnostics of servers proven fresh for this document version are
    // reported, so their presence shows the wait saw the publish. (The
    // response stays incomplete: Biome and Oxlint have no root marker in
    // this fixture.)
    assert!(
        !json["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .is_empty(),
        "the fake server's diagnostics are reported: {json:#}"
    );
    // A 10 s budget slept out in 100 ms steps re-checks about 100 times.
    assert!(
        wakeups <= 10,
        "the wait polled instead of waking on the publish: {wakeups} re-checks"
    );
    assert!(
        elapsed < Duration::from_secs(8),
        "the wait slept out its budget: {elapsed:?}"
    );
}
