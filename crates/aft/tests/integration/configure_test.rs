use serde_json::json;

use super::helpers::AftProcess;

fn empty_path() -> std::ffi::OsString {
    std::ffi::OsString::new()
}

fn warning_with_kind<'a>(
    configure: &'a serde_json::Value,
    kind: &str,
    key: &str,
    value: &str,
) -> Option<&'a serde_json::Value> {
    configure["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|warning| {
            warning["kind"] == kind
                && warning.get(key).and_then(|entry| entry.as_str()) == Some(value)
        })
}

#[test]
fn configure_accepts_boolean_validate_on_edit() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();

    let configure = aft.send(
        &json!({
            "id": "cfg-validate-bool",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "validate_on_edit": true,
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure should accept boolean validate_on_edit: {configure:?}"
    );
    assert!(
        configure["warnings"].as_array().is_some(),
        "configure responses should always include warnings: {configure:?}"
    );

    let status = aft.send(r#"{"id":"status-validate-bool","command":"status"}"#);
    assert_eq!(status["success"], true, "status should succeed: {status:?}");
    assert_eq!(status["features"]["validate_on_edit"], "syntax");

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_warnings_frame_after_main_response() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.ts"), "const x = 1;\n").unwrap();
    let path = empty_path();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);

    let configure = aft.send(
        &json!({
            "id": "cfg-warning-order",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_auto_install_binaries": ["typescript-language-server"]
        })
        .to_string(),
    );

    assert_eq!(configure["id"], "cfg-warning-order");
    assert_eq!(configure["success"], true);
    let frame = aft.merge_configure_warnings(configure.clone());
    assert_eq!(frame["id"], "cfg-warning-order");

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_warns_for_missing_formatter_and_checker_tools() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.ts"), "const x = 1;\n").unwrap();
    std::fs::write(dir.path().join("biome.json"), "{}\n").unwrap();

    let path = empty_path();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);

    let configure = aft.send(
        &json!({
            "id": "cfg-missing-format-check",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path()
        })
        .to_string(),
    );

    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );
    // Warnings are now produced asynchronously after configure returns.
    let configure = aft.merge_configure_warnings(configure);
    let formatter = warning_with_kind(&configure, "formatter_not_installed", "tool", "biome")
        .expect("missing formatter warning");
    assert_eq!(formatter["language"], "typescript");
    assert!(formatter["hint"]
        .as_str()
        .unwrap()
        .contains("bun add -d --workspace-root @biomejs/biome"));

    let checker = warning_with_kind(&configure, "checker_not_installed", "tool", "biome")
        .expect("missing checker warning");
    assert_eq!(checker["language"], "typescript");

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_warns_for_missing_explicit_tsgo_checker() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.ts"), "const x = 1;\n").unwrap();

    let path = empty_path();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);

    let configure = aft.send(
        &json!({
            "id": "cfg-missing-tsgo",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "checker": {
                "typescript": "tsgo"
            }
        })
        .to_string(),
    );

    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );
    let configure = aft.merge_configure_warnings(configure);
    let checker = warning_with_kind(&configure, "checker_not_installed", "tool", "tsgo")
        .expect("missing tsgo warning");
    assert_eq!(checker["language"], "typescript");
    assert!(checker["hint"]
        .as_str()
        .unwrap()
        .contains("@typescript/native-preview"));

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_only_warns_for_languages_present() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.ts"), "const x = 1;\n").unwrap();
    std::fs::write(dir.path().join("pyrightconfig.json"), "{}\n").unwrap();

    let path = empty_path();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);

    let configure = aft.send(
        &json!({
            "id": "cfg-language-present",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path()
        })
        .to_string(),
    );

    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );
    assert!(
        warning_with_kind(&configure, "checker_not_installed", "tool", "pyright").is_none(),
        "should not warn about Python checker without Python files: {configure:?}"
    );

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_warns_for_missing_builtin_and_custom_lsp_binaries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("script.sh"), "echo hi\n").unwrap();

    let path = empty_path();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);

    let configure = aft.send(
        &json!({
            "id": "cfg-missing-lsp",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_auto_install_binaries": ["bash-language-server"],
            "lsp_servers": [{
                "id": "tinymist",
                "extensions": ["typ"],
                "binary": "tinymist",
                "args": [],
                "root_markers": ["typst.toml"]
            }]
        })
        .to_string(),
    );

    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );
    let configure = aft.merge_configure_warnings(configure);
    let bash = warning_with_kind(
        &configure,
        "lsp_binary_missing",
        "binary",
        "bash-language-server",
    )
    .expect("missing built-in bash LSP warning");
    assert_eq!(bash["server"], "bash-language-server");
    assert!(bash["hint"]
        .as_str()
        .unwrap()
        .contains("npm install -g bash-language-server"));

    let custom = warning_with_kind(&configure, "lsp_binary_missing", "binary", "tinymist")
        .expect("missing custom LSP warning");
    assert_eq!(custom["server"], "tinymist");

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_does_not_warn_for_file_discovered_non_auto_installable_lsp() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Program.cs"), "class Program {}\n").unwrap();

    let path = empty_path();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);

    let configure = aft.send(
        &json!({
            "id": "cfg-no-roslyn-warning",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_auto_install_binaries": ["typescript-language-server"]
        })
        .to_string(),
    );

    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );
    assert!(
        warning_with_kind(
            &configure,
            "lsp_binary_missing",
            "binary",
            "roslyn-language-server"
        )
        .is_none(),
        "should not warn for non-auto-installable file-discovered LSP: {configure:?}"
    );

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_warns_for_file_discovered_auto_installable_lsp() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.ts"), "const x = 1;\n").unwrap();

    let path = empty_path();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);

    let configure = aft.send(
        &json!({
            "id": "cfg-typescript-lsp-warning",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_auto_install_binaries": ["typescript-language-server"]
        })
        .to_string(),
    );

    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );
    let configure = aft.merge_configure_warnings(configure);
    let warning = warning_with_kind(
        &configure,
        "lsp_binary_missing",
        "binary",
        "typescript-language-server",
    )
    .expect("missing TypeScript LSP warning");
    assert_eq!(warning["server"], "typescript-language-server");

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_warns_for_custom_lsp_regardless_of_auto_install_set() {
    let dir = tempfile::tempdir().unwrap();

    let path = empty_path();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);

    let configure = aft.send(
        &json!({
            "id": "cfg-custom-lsp-warning",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_auto_install_binaries": [],
            "lsp_servers": [{
                "id": "custom-thing",
                "extensions": ["thing"],
                "binary": "nonexistent-binary",
                "args": [],
                "root_markers": [".git"]
            }]
        })
        .to_string(),
    );

    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );
    let configure = aft.merge_configure_warnings(configure);
    let warning = warning_with_kind(
        &configure,
        "lsp_binary_missing",
        "binary",
        "nonexistent-binary",
    )
    .expect("missing custom LSP warning");
    assert_eq!(warning["server"], "custom-thing");

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_skips_builtin_lsp_warnings_when_auto_install_disabled() {
    // Regression for the recurring "AFT keeps bugging me about cannot install"
    // UX bug after users set `lsp.auto_install: false` in aft.jsonc.
    //
    // Repro: project has a `.ts` file → built-in `typescript-language-server`
    // matches. User sets `lsp.auto_install: false`. Plugins now send
    // `lsp_auto_install_binaries: []` to Rust to short-circuit the built-in
    // server walk in `detect_missing_lsp_binaries`. The result should be ZERO
    // `lsp_binary_missing` warnings for the built-in. (Explicit `lsp.servers`
    // entries are unaffected — see `configure_warns_for_custom_lsp_*`.)
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.ts"), "const x = 1;\n").unwrap();

    let path = empty_path();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);

    let configure = aft.send(
        &json!({
            "id": "cfg-auto-install-off",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            // Plugins send an EMPTY list when `lsp.auto_install: false`.
            "lsp_auto_install_binaries": [],
        })
        .to_string(),
    );

    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );
    let configure = aft.merge_configure_warnings(configure);
    assert!(
        warning_with_kind(
            &configure,
            "lsp_binary_missing",
            "binary",
            "typescript-language-server",
        )
        .is_none(),
        "no built-in lsp_binary_missing warning expected when auto_install_binaries is empty: {configure:?}"
    );

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_warnings_wait_consumes_pending_frame_before_reading_stdout() {
    let mut aft = AftProcess::spawn();

    // Deterministic regression for the macOS CI race: when a push frame lands
    // on stdout before the matching configure response, `send()` queues it in
    // the helper. The later warnings wait must consume that queued frame first,
    // not block waiting for another line that will never arrive.
    aft.queue_pending_frame_for_test(json!({
        "type": "progress",
        "message": "unrelated push frame"
    }));
    aft.queue_pending_frame_for_test(json!({
        "type": "configure_warnings",
        "warnings": [{
            "kind": "lsp_binary_missing",
            "server": "custom-thing",
            "binary": "nonexistent-binary"
        }],
        "source_file_count": 0,
        "source_file_count_exceeds_max": false
    }));

    let configure = aft.merge_configure_warnings(json!({
        "id": "cfg-custom-lsp-warning",
        "success": true,
        "warnings": [],
        "warnings_pending": true
    }));

    let warning = warning_with_kind(
        &configure,
        "lsp_binary_missing",
        "binary",
        "nonexistent-binary",
    )
    .expect("missing custom LSP warning from pending frame");
    assert_eq!(warning["server"], "custom-thing");
    assert_eq!(configure["source_file_count"], 0);
    assert_eq!(configure["source_file_count_exceeds_max"], false);
    assert_eq!(configure["warnings_pending"], false);

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_suppresses_missing_lsp_warning_for_inflight_install() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.ts"), "const x = 1;\n").unwrap();

    let path = empty_path();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);

    let configure = aft.send(
        &json!({
            "id": "cfg-typescript-lsp-inflight",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_auto_install_binaries": ["typescript-language-server"],
            "lsp_inflight_installs": ["typescript-language-server"]
        })
        .to_string(),
    );

    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );
    assert!(
        warning_with_kind(
            &configure,
            "lsp_binary_missing",
            "binary",
            "typescript-language-server",
        )
        .is_none(),
        "should not warn while TypeScript LSP install is in flight: {configure:?}"
    );

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_accepts_custom_lsp_servers() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();

    let configure = aft.send(
        &json!({
            "id": "cfg-lsp-custom",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "experimental_lsp_ty": true,
            "lsp_servers": [{
                "id": "tinymist",
                "extensions": ["typ"],
                "binary": "tinymist",
                "args": [],
                "root_markers": [".git", "typst.toml"],
                "env": {
                    "TINYMIST_FONT_PATHS": "/tmp/fonts"
                },
                "initialization_options": {
                    "exportPdf": "never"
                },
                "disabled": false
            }],
            "disabled_lsp": ["Pyright"]
        })
        .to_string(),
    );

    assert_eq!(
        configure["success"], true,
        "configure should accept custom lsp server config: {configure:?}"
    );

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_rejects_lsp_server_env_with_non_string_values() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();

    let configure = aft.send(
        &json!({
            "id": "cfg-lsp-bad-env",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_servers": [{
                "id": "tinymist",
                "extensions": ["typ"],
                "binary": "tinymist",
                "env": {
                    "TINYMIST_FONT_PATHS": 42
                }
            }]
        })
        .to_string(),
    );

    assert_eq!(configure["success"], false);
    assert_eq!(configure["code"], "invalid_request");
    assert!(configure["message"]
        .as_str()
        .unwrap()
        .contains("env.TINYMIST_FONT_PATHS must be a string"));

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_rejects_malformed_lsp_servers() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();

    let configure = aft.send(
        &json!({
            "id": "cfg-lsp-bad",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_servers": [{
                "id": "tinymist",
                "extensions": [],
                "binary": "tinymist"
            }]
        })
        .to_string(),
    );

    assert_eq!(configure["success"], false);
    assert_eq!(configure["code"], "invalid_request");
    assert!(configure["message"]
        .as_str()
        .unwrap()
        .contains("extensions must not be empty"));

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

/// `lsp_paths_extra` provided by the plugin should reach the Rust LSP resolver,
/// so a binary placed in one of those directories is picked up before PATH.
///
/// This is the contract that the plugin-side auto-installer depends on:
/// the plugin maintains its own LSP cache directory, sends it as
/// `lsp_paths_extra` on configure, and Rust resolves binaries from there
/// without needing them on PATH. Stage 5 of the auto-install design hinges
/// on this passing.
#[test]
fn configure_accepts_lsp_paths_extra() {
    let dir = tempfile::tempdir().unwrap();
    let existing_bin = dir.path().join("lsp-cache").join("typescript").join(".bin");
    let pending_bin = dir.path().join("lsp-cache").join("clangd").join("bin");
    std::fs::create_dir_all(&existing_bin).unwrap();
    let mut aft = AftProcess::spawn();

    let configure = aft.send(
        &json!({
            "id": "cfg-lsp-paths-extra",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_paths_extra": [
                existing_bin,
                pending_bin,
            ],
        })
        .to_string(),
    );

    assert_eq!(
        configure["success"], true,
        "configure should accept lsp_paths_extra: {configure:?}"
    );

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn configure_rejects_existing_file_lsp_paths_extra() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-directory");
    std::fs::write(&file, "not a directory").unwrap();
    let mut aft = AftProcess::spawn();

    let configure = aft.send(
        &json!({
            "id": "cfg-lsp-paths-file",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_paths_extra": [file],
        })
        .to_string(),
    );

    assert_eq!(configure["success"], false);
    assert_eq!(configure["code"], "invalid_request");
    assert!(configure["message"]
        .as_str()
        .unwrap()
        .contains("must resolve to a directory"));

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

/// Malformed `lsp_paths_extra` (non-array, empty strings, or non-absolute
/// paths) must be rejected with `invalid_request`. This guards against the
/// plugin sending bad data — Rust must not silently accept it because the
/// resolver would then fail late and in confusing ways.
#[test]
fn configure_rejects_malformed_lsp_paths_extra() {
    let dir = tempfile::tempdir().unwrap();

    // Non-array → invalid_request.
    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &json!({
            "id": "cfg-lsp-paths-not-array",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_paths_extra": "not-an-array",
        })
        .to_string(),
    );
    assert_eq!(configure["success"], false);
    assert_eq!(configure["code"], "invalid_request");
    let shutdown = aft.shutdown();
    assert!(shutdown.success());

    // Empty string entry → invalid_request.
    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &json!({
            "id": "cfg-lsp-paths-empty",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_paths_extra": [""],
        })
        .to_string(),
    );
    assert_eq!(configure["success"], false);
    assert_eq!(configure["code"], "invalid_request");
    let shutdown = aft.shutdown();
    assert!(shutdown.success());

    // Relative path → invalid_request.
    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &json!({
            "id": "cfg-lsp-paths-relative",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "lsp_paths_extra": ["relative/path"],
        })
        .to_string(),
    );
    assert_eq!(configure["success"], false);
    assert_eq!(configure["code"], "invalid_request");
    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}
