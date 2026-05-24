use std::path::PathBuf;

use serde_json::json;
use tempfile::tempdir;

use super::helpers::AftProcess;

fn empty_path() -> std::ffi::OsString {
    std::ffi::OsString::new()
}

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

#[test]
fn lsp_inspect_reports_no_matching_servers() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("example.foo");
    std::fs::write(&file, "content\n").unwrap();

    let mut aft = AftProcess::spawn();
    let configure = aft.configure(dir.path());
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let resp = aft.send(
        &json!({
            "id": "inspect-none",
            "command": "lsp_inspect",
            "file": file,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], true, "inspect failed: {resp:?}");
    assert_eq!(resp["extension"], "foo");
    assert_eq!(resp["matching_servers"].as_array().unwrap().len(), 0);
    assert_eq!(resp["diagnostics_count"], 0);

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn lsp_inspect_reports_missing_pyright_binary() {
    let dir = tempdir().unwrap();
    let package_dir = dir.path().join("python");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::write(package_dir.join("requirements.txt"), "requests\n").unwrap();
    let file = package_dir.join("__init__.py");
    std::fs::write(&file, "foo\n").unwrap();

    let path = empty_path();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    let configure = aft.configure(dir.path());
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let resp = aft.send(
        &json!({
            "id": "inspect-missing-pyright",
            "command": "lsp_inspect",
            "file": file,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], true, "inspect failed: {resp:?}");
    let servers = resp["matching_servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1, "response: {resp:?}");
    assert_eq!(servers[0]["id"], "python");
    assert_eq!(servers[0]["binary_name"], "pyright-langserver");
    assert_eq!(servers[0]["binary_path"], serde_json::Value::Null);
    assert_eq!(servers[0]["binary_source"], "not_found");
    let canonical_package_dir = std::fs::canonicalize(&package_dir).unwrap();
    assert_eq!(
        servers[0]["workspace_root"],
        canonical_package_dir.display().to_string()
    );
    assert_eq!(servers[0]["spawn_status"], "binary_not_installed");

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn lsp_inspect_reports_custom_server_ok_with_diagnostics() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("fake.toml"), "[project]\n").unwrap();
    let file = root.join("main.fake");
    std::fs::write(&file, "hello\n").unwrap();

    let fake_server = fake_server_path();
    let fake_bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&fake_bin_dir).unwrap();
    let fake_binary_name = fake_server
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    std::fs::copy(&fake_server, fake_bin_dir.join(&fake_binary_name)).unwrap();

    let mut aft = AftProcess::spawn_with_env(&[("AFT_FAKE_LSP_PULL", std::ffi::OsStr::new("1"))]);
    let configure = aft.send(
        &json!({
            "id": "cfg-custom-lsp",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "lsp_paths_extra": [fake_bin_dir],
            "lsp_servers": [{
                "id": "fake",
                "extensions": ["fake"],
                "binary": fake_binary_name,
                "args": [],
                "root_markers": ["fake.toml"]
            }]
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let resp = aft.send(
        &json!({
            "id": "inspect-custom-ok",
            "command": "lsp_inspect",
            "file": file,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], true, "inspect failed: {resp:?}");
    let servers = resp["matching_servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1, "response: {resp:?}");
    assert_eq!(servers[0]["id"], "fake");
    assert_eq!(servers[0]["binary_source"], "lsp_paths_extra");
    assert_eq!(servers[0]["spawn_status"], "ok");
    assert_eq!(resp["diagnostics_count"], 1);
    assert_eq!(resp["diagnostics"][0]["message"], "test pull diagnostic");

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}
