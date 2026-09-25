//! A Mac without the Xcode command-line tools has `/usr/bin/git` as Apple's
//! launcher: running it opens the "Install Command Line Developer Tools"
//! dialog. These tests stand a recording stub in for that launcher and check
//! that AFT never runs it, while a real git earlier on PATH keeps working.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use super::helpers::AftProcess;

/// Write an executable script that appends its arguments to `log`.
fn recording_script(path: &Path, log: &Path, tail: &str) {
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{tail}\n",
        log.display()
    );
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn invocations(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// The stand-in for Apple's launcher: records the call and fails the way the
/// launcher does on a machine without developer tools.
fn launcher_stub(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let log = dir.join("invocations.log");
    recording_script(
        &dir.join("git"),
        &log,
        "echo 'xcode-select: note: No developer tools were found, requesting install.' >&2\nexit 1",
    );
    log
}

fn host_git() -> PathBuf {
    which::which("git").expect("tests need a working git on the host PATH")
}

fn spawn(path: &std::ffi::OsStr, launcher_dir: &Path) -> AftProcess {
    AftProcess::spawn_with_env(&[
        ("PATH", path),
        ("AFT_TEST_DEVELOPER_TOOLS", "absent".as_ref()),
        ("AFT_TEST_XCODE_LAUNCHER_DIRS", launcher_dir.as_os_str()),
    ])
}

fn configure(aft: &mut AftProcess, root: &Path) {
    let response = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
}

fn status(aft: &mut AftProcess) -> serde_json::Value {
    aft.send(&json!({ "id": "status", "command": "status" }).to_string())
}

#[test]
fn configure_never_runs_the_launcher_git_without_developer_tools() {
    let temp = tempfile::tempdir().unwrap();
    let launcher_dir = temp.path().join("usr-bin");
    let log = launcher_stub(&launcher_dir);
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("main.rs"), "fn main() {}\n").unwrap();

    let path = std::env::join_paths([launcher_dir.clone(), PathBuf::from("/bin")]).unwrap();
    let mut aft = spawn(&path, &launcher_dir);
    configure(&mut aft, &project);
    let grep = aft.send(
        &json!({ "id": "grep", "command": "grep", "pattern": "main", "path": project })
            .to_string(),
    );
    assert_eq!(grep["success"], true, "grep failed: {grep:?}");
    let first = status(&mut aft);
    let second = status(&mut aft);
    assert!(aft.shutdown().success());

    assert_eq!(
        invocations(&log),
        Vec::<String>::new(),
        "AFT ran the developer-tools launcher"
    );
    for snapshot in [&first, &second] {
        assert_eq!(snapshot["git"]["available"], false, "status: {snapshot:?}");
        assert_eq!(
            snapshot["git"]["reason"], "macos_developer_tools_missing",
            "status: {snapshot:?}"
        );
    }
}

#[test]
fn a_real_git_earlier_on_path_is_still_used() {
    let temp = tempfile::tempdir().unwrap();
    let launcher_dir = temp.path().join("usr-bin");
    let launcher_log = launcher_stub(&launcher_dir);
    let homebrew = temp.path().join("homebrew-bin");
    fs::create_dir_all(&homebrew).unwrap();
    let homebrew_log = homebrew.join("invocations.log");
    let real_git = host_git();
    recording_script(
        &homebrew.join("git"),
        &homebrew_log,
        &format!("exec '{}' \"$@\"", real_git.display()),
    );

    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("main.rs"), "fn main() {}\n").unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "."][..],
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e.x",
            "commit",
            "-qm",
            "init",
        ][..],
    ] {
        assert!(Command::new(&real_git)
            .current_dir(&project)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(args)
            .status()
            .unwrap()
            .success());
    }

    let path = std::env::join_paths([homebrew.clone(), launcher_dir.clone(), "/bin".into()])
        .unwrap();
    let mut aft = spawn(&path, &launcher_dir);
    configure(&mut aft, &project);
    let snapshot = status(&mut aft);
    assert!(aft.shutdown().success());

    assert_eq!(snapshot["git"]["available"], true, "status: {snapshot:?}");
    assert!(
        !invocations(&homebrew_log).is_empty(),
        "the git earlier on PATH was never used"
    );
    assert_eq!(invocations(&launcher_log), Vec::<String>::new());
}
