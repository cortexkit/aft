use std::path::PathBuf;

use serde_json::json;
use tempfile::tempdir;

use super::helpers::{canonicalize_like_product, user_config, warm_executable, AftProcess};

fn empty_path() -> std::ffi::OsString {
    std::ffi::OsString::new()
}

fn fake_server_path() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_fake_lsp_server")
        .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_fake-lsp-server"))
        .map(PathBuf::from)
        .or_else(|| {
            option_env!("CARGO_BIN_EXE_fake-lsp-server")
                .or(option_env!("CARGO_BIN_EXE_fake_lsp_server"))
                .map(PathBuf::from)
        })
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
    // Workspace roots are reported in the product's normalized form
    // (verbatim-stripped); bare canonicalize is verbatim on Windows.
    let canonical_package_dir = crate::helpers::canonicalize_like_product(&package_dir);
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
    let installed_fake_server = fake_bin_dir.join(&fake_binary_name);
    std::fs::copy(&fake_server, &installed_fake_server).unwrap();
    // The fake LSP has no CLI probe; closed stdin makes its protocol loop exit.
    warm_executable(&installed_fake_server, &[]);

    let mut aft = AftProcess::spawn_with_env(&[("AFT_FAKE_LSP_PULL", std::ffi::OsStr::new("1"))]);
    let configure = aft.send(
        &json!({
            "id": "cfg-custom-lsp",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "lsp_paths_extra": [fake_bin_dir],
            "config": user_config(serde_json::json!({
                "lsp": {
                    "servers": {
                        "fake": {
                            "extensions": ["fake"],
                            "binary": fake_binary_name,
                            "args": [],
                            "root_markers": ["fake.toml"]
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

#[test]
fn lsp_inspect_reports_nested_python_virtualenv_binary() {
    let dir = tempdir().unwrap();
    let backend = dir.path().join("backend");
    let file = backend.join("app").join("main.py");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(backend.join("pyproject.toml"), "[project]\nname = 'demo'\n").unwrap();
    std::fs::write(&file, "print('ok')\n").unwrap();

    let fake_server = fake_server_path();
    let virtualenv_bin = if cfg!(windows) {
        backend.join(".venv").join("Scripts")
    } else {
        backend.join(".venv").join("bin")
    };
    std::fs::create_dir_all(&virtualenv_bin).unwrap();
    let installed_server = if cfg!(windows) {
        virtualenv_bin.join("pyright-langserver.exe")
    } else {
        virtualenv_bin.join("pyright-langserver")
    };
    std::fs::copy(&fake_server, &installed_server).unwrap();
    warm_executable(&installed_server, &["--stdio"]);

    let mut aft = AftProcess::spawn_with_env(&[("PATH", empty_path().as_os_str())]);
    let configure = aft.send(
        &json!({
            "id": "cfg-nested-python-lsp",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let response = aft.send(
        &json!({
            "id": "inspect-nested-python-lsp",
            "command": "lsp_inspect",
            "file": file,
        })
        .to_string(),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:?}");
    let servers = response["matching_servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1, "response: {response:?}");
    assert_eq!(servers[0]["id"], "python");
    assert_eq!(servers[0]["binary_source"], "project_virtualenv");
    assert_eq!(
        servers[0]["workspace_root"],
        canonicalize_like_product(&backend).display().to_string()
    );
    assert_eq!(
        servers[0]["binary_path"],
        canonicalize_like_product(&installed_server)
            .display()
            .to_string()
    );
    assert_eq!(servers[0]["spawn_status"], "ok");
    assert_eq!(response["diagnostics_complete"], false);
    assert_eq!(response["diagnostics_gaps"][0]["server_id"], "python");
    assert_eq!(
        response["diagnostics_gaps"][0]["reason"],
        "pull_not_supported"
    );

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn lsp_inspect_classifies_nested_python_node_modules_binary() {
    let dir = tempdir().unwrap();
    let repository = dir.path().join("repository");
    let backend = repository.join("backend");
    let file = backend.join("app").join("main.py");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(backend.join("pyproject.toml"), "[project]\nname = 'demo'\n").unwrap();
    std::fs::write(&file, "print('ok')\n").unwrap();

    let nested_bin = backend.join("node_modules").join(".bin");
    std::fs::create_dir_all(&nested_bin).unwrap();
    let installed_server = if cfg!(windows) {
        nested_bin.join("pyright-langserver.exe")
    } else {
        nested_bin.join("pyright-langserver")
    };
    std::fs::copy(fake_server_path(), &installed_server).unwrap();
    warm_executable(&installed_server, &["--stdio"]);

    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_FAKE_LSP_PULL", std::ffi::OsStr::new("1")),
        ("PATH", empty_path().as_os_str()),
    ]);
    let configure = aft.send(
        &json!({
            "id": "cfg-nested-python-node-lsp",
            "command": "configure",
            "harness": "opencode",
            "project_root": repository,
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let response = aft.send(
        &json!({
            "id": "inspect-nested-python-node-lsp",
            "command": "lsp_inspect",
            "file": file,
        })
        .to_string(),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:?}");
    let server = &response["matching_servers"][0];
    assert_eq!(server["binary_source"], "project_node_modules");
    assert_eq!(
        server["workspace_root"],
        canonicalize_like_product(&backend).display().to_string()
    );
    assert_eq!(
        server["binary_path"],
        canonicalize_like_product(&installed_server)
            .display()
            .to_string()
    );
    assert_eq!(server["spawn_status"], "ok");

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}

#[test]
fn lsp_inspect_classifies_non_python_binary_against_project_root() {
    let dir = tempdir().unwrap();
    let repository = dir.path().join("repository");
    let package = repository.join("packages").join("app");
    let file = package.join("main.ts");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("package.json"), "{\"name\":\"app\"}\n").unwrap();
    std::fs::write(&file, "export const value = 1;\n").unwrap();

    let fake_server = fake_server_path();
    let project_bin = repository.join("node_modules").join(".bin");
    std::fs::create_dir_all(&project_bin).unwrap();
    let installed_server = if cfg!(windows) {
        project_bin.join("typescript-language-server.exe")
    } else {
        project_bin.join("typescript-language-server")
    };
    std::fs::copy(&fake_server, &installed_server).unwrap();
    warm_executable(&installed_server, &["--stdio"]);

    let mut aft = AftProcess::spawn_with_env(&[("AFT_FAKE_LSP_PULL", std::ffi::OsStr::new("1"))]);
    let configure = aft.send(
        &json!({
            "id": "cfg-nested-typescript-lsp",
            "command": "configure",
            "harness": "opencode",
            "project_root": repository,
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let response = aft.send(
        &json!({
            "id": "inspect-nested-typescript-lsp",
            "command": "lsp_inspect",
            "file": file,
        })
        .to_string(),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:?}");
    let server = response["matching_servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["id"] == "typescript")
        .expect("TypeScript server inspection");
    assert_eq!(server["binary_source"], "project_node_modules");
    assert_eq!(
        server["workspace_root"],
        canonicalize_like_product(&package).display().to_string()
    );
    assert_eq!(
        server["binary_path"],
        installed_server.display().to_string()
    );
    assert_eq!(server["spawn_status"], "ok");

    let shutdown = aft.shutdown();
    assert!(shutdown.success());
}
