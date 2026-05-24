use std::fs;
use std::path::Path;

use aft::config::Config;
use aft::context::AppContext;
use aft::language::StubProvider;

use super::helpers::AftProcess;

fn assert_error_code(resp: &serde_json::Value, code: &str) {
    assert_eq!(
        resp["success"], false,
        "expected failure response: {resp:?}"
    );
    assert_eq!(resp["code"], code, "unexpected error response: {resp:?}");
}

fn assert_validate_path_outside_root(ctx: &AppContext, path: &Path) {
    match ctx.validate_path("validate-broken-symlink", path) {
        Ok(validated) => panic!("validate_path unexpectedly succeeded: {validated:?}"),
        Err(resp) => assert_eq!(
            serde_json::to_value(resp).unwrap()["code"],
            "path_outside_root"
        ),
    }
}

fn restricted_context(root: &Path) -> AppContext {
    AppContext::new(
        Box::new(StubProvider),
        Config {
            project_root: Some(root.to_path_buf()),
            restrict_to_project_root: true,
            ..Config::default()
        },
    )
}

fn configure_restricted(aft: &mut AftProcess, root: &Path) {
    let configure = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "restrict_to_project_root": true,
        }))
        .unwrap(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );
}

#[cfg(unix)]
fn create_dir_symlink(src: &Path, dst: &Path) {
    std::os::unix::fs::symlink(src, dst).expect("create symlink");
}

#[cfg(windows)]
fn create_dir_symlink(src: &Path, dst: &Path) {
    std::os::windows::fs::symlink_dir(src, dst).expect("create symlink");
}

#[cfg(unix)]
fn create_file_symlink(src: &Path, dst: &Path) {
    std::os::unix::fs::symlink(src, dst).expect("create symlink");
}

#[cfg(windows)]
fn create_file_symlink(src: &Path, dst: &Path) {
    std::os::windows::fs::symlink_file(src, dst).expect("create symlink");
}

#[test]
fn write_blocks_parent_dir_traversal_outside_project_root() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    configure_restricted(&mut aft, &root);

    let attempted = root.join("../outside/escape.txt");
    let resp = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "write-parent-traversal",
            "command": "write",
            "file": attempted.display().to_string(),
            "content": "blocked",
        }))
        .unwrap(),
    );

    assert_error_code(&resp, "path_outside_root");
    assert!(!outside.join("escape.txt").exists());

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn read_blocks_absolute_path_outside_project_root() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let outside = dir.path().join("outside.txt");
    fs::create_dir_all(&root).unwrap();
    fs::write(&outside, "secret").unwrap();
    configure_restricted(&mut aft, &root);

    let resp = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "read-outside-root",
            "command": "read",
            "file": outside.display().to_string(),
        }))
        .unwrap(),
    );

    assert_error_code(&resp, "path_outside_root");

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(any(unix, windows))]
#[test]
fn write_blocks_symlink_traversal_outside_project_root() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    create_dir_symlink(&outside, &root.join("link"));
    configure_restricted(&mut aft, &root);

    let attempted = root.join("link/newdir/escape.txt");
    let resp = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "write-symlink-traversal",
            "command": "write",
            "file": attempted.display().to_string(),
            "content": "blocked",
        }))
        .unwrap(),
    );

    assert_error_code(&resp, "path_outside_root");
    assert!(!outside.join("newdir/escape.txt").exists());

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(any(unix, windows))]
#[test]
fn write_blocks_broken_symlink_escape_from_project_root() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let outside = dir.path().join("escape-target");
    fs::create_dir_all(&root).unwrap();
    create_file_symlink(&outside, &root.join("broken-link"));
    configure_restricted(&mut aft, &root);

    let attempted = root.join("broken-link");
    let resp = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "write-broken-symlink-traversal",
            "command": "write",
            "file": attempted.display().to_string(),
            "content": "blocked",
        }))
        .unwrap(),
    );

    assert_error_code(&resp, "path_outside_root");
    assert!(!outside.exists());

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(any(unix, windows))]
#[test]
fn validate_path_rejects_broken_absolute_symlink_escape() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let outside = dir.path().join("outside").join("foo");
    fs::create_dir_all(&root).unwrap();
    create_file_symlink(&outside, &root.join("escape"));

    let ctx = restricted_context(&root);
    assert_validate_path_outside_root(&ctx, &root.join("escape"));
    assert!(!outside.exists());
}

#[cfg(any(unix, windows))]
#[test]
fn validate_path_rejects_broken_relative_symlink_escape() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(&root).unwrap();
    create_file_symlink(Path::new("../../etc/passwd"), &root.join("escape"));

    let ctx = restricted_context(&root);
    assert_validate_path_outside_root(&ctx, &root.join("escape"));
}

#[cfg(any(unix, windows))]
#[test]
fn validate_path_rejects_broken_symlink_chain_escape() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&root).unwrap();
    create_file_symlink(Path::new("b"), &root.join("a"));
    create_file_symlink(&outside, &root.join("b"));

    let ctx = restricted_context(&root);
    assert_validate_path_outside_root(&ctx, &root.join("a"));
    assert!(!outside.exists());
}

#[test]
fn write_resolves_relative_dotdot_path_within_project_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(root.join("nested")).unwrap();

    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, &root);

    let resp = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "write-relative-dotdot",
            "command": "write",
            "file": "nested/../resolved.txt",
            "content": "ok",
        }))
        .unwrap(),
    );

    assert_eq!(resp["success"], true, "write failed: {resp:?}");
    assert_eq!(fs::read_to_string(root.join("resolved.txt")).unwrap(), "ok");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn validate_path_resolves_relative_paths_against_project_root_not_process_cwd() {
    let cwd = std::env::current_dir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let file = root.join("src").join("main.rs");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "fn main() {}\n").unwrap();

    let ctx = restricted_context(&root);
    let validated = ctx
        .validate_path("relative-project-root", Path::new("src/main.rs"))
        .expect("relative path should resolve inside project root");

    assert_eq!(validated, std::fs::canonicalize(&file).unwrap());
    assert_eq!(std::env::current_dir().unwrap(), cwd);
}

#[test]
fn validate_path_returns_canonical_path_that_write_uses() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(root.join("nested")).unwrap();

    let requested = root.join("nested/../canonical.txt");
    let expected_root = std::fs::canonicalize(&root).unwrap_or(root.clone());
    let expected = expected_root.join("canonical.txt");

    let ctx = AppContext::new(
        Box::new(StubProvider),
        Config {
            project_root: Some(root.clone()),
            restrict_to_project_root: true,
            ..Config::default()
        },
    );

    let validated = match ctx.validate_path("validate-path", &requested) {
        Ok(path) => path,
        Err(resp) => panic!("validate_path failed unexpectedly: {resp:?}"),
    };
    assert_eq!(validated, expected);

    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, &root);
    let resp = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "write-canonical-path",
            "command": "write",
            "file": requested.display().to_string(),
            "content": "canonical",
        }))
        .unwrap(),
    );

    assert_eq!(resp["success"], true, "write failed: {resp:?}");
    assert_eq!(fs::read_to_string(&validated).unwrap(), "canonical");

    let status = aft.shutdown();
    assert!(status.success());
}
