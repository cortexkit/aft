//! Live reload of config file edits through the real project watcher.

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::helpers::AftProcess;

fn configure(aft: &mut AftProcess, root: &Path, user_config: &Path) {
    let response = aft.send(
        &json!({
            "id": "cfg-live-reload",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "cortexkit_user_config_path": user_config,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
}

fn bash_true(aft: &mut AftProcess) -> Value {
    aft.send(
        &json!({
            "id": "live-reload-bash",
            "command": "bash",
            "params": { "command": "true" }
        })
        .to_string(),
    )
}

fn wait_for_bash_disabled(aft: &mut AftProcess) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        last = bash_true(aft);
        if last["code"] == "bash_disabled" {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("the config edit was never applied; last bash response: {last:?}");
}

fn live_edit_disables_bash(gitignore: Option<&str>) {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(root.join(".cortexkit")).unwrap();
    if let Some(gitignore) = gitignore {
        fs::write(root.join(".gitignore"), gitignore).unwrap();
    }
    let project_config = root.join(".cortexkit/aft.jsonc");
    fs::write(&project_config, r#"{ "bash": { "enabled": true } }"#).unwrap();
    let user_config = dir.path().join("xdg/cortexkit/aft.jsonc");
    fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    fs::write(&user_config, "{}").unwrap();

    let mut aft = AftProcess::spawn_with_real_watcher();
    configure(&mut aft, &root, &user_config);
    assert_eq!(bash_true(&mut aft)["success"], true);

    // Edit the file and send no configure: the next requests see the change.
    thread::sleep(Duration::from_millis(300));
    fs::write(&project_config, r#"{ "bash": { "enabled": false } }"#).unwrap();
    wait_for_bash_disabled(&mut aft);
}

#[test]
fn project_config_edit_applies_live_through_the_project_watcher() {
    live_edit_disables_bash(None);
}

#[test]
fn project_config_edit_applies_live_when_cortexkit_is_gitignored() {
    live_edit_disables_bash(Some(".cortexkit/\n"));
}

#[test]
fn user_config_edit_applies_live() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(&root).unwrap();
    let user_config = dir.path().join("xdg/cortexkit/aft.jsonc");
    fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    fs::write(&user_config, "{}").unwrap();

    let mut aft = AftProcess::spawn_with_real_watcher();
    configure(&mut aft, &root, &user_config);
    assert_eq!(bash_true(&mut aft)["success"], true);

    thread::sleep(Duration::from_millis(300));
    fs::write(&user_config, r#"{ "bash": { "enabled": false } }"#).unwrap();
    wait_for_bash_disabled(&mut aft);
}

#[cfg(any(unix, windows))]
fn config_symlink(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(target, link).unwrap();
}

fn write_env_config(path: &Path, value: &str) {
    fs::write(
        path,
        json!({ "bash": { "env": { "SYMLINK_CONFIG_MARK": value } } }).to_string(),
    )
    .unwrap();
}

fn bash_env(aft: &mut AftProcess) -> Value {
    #[cfg(windows)]
    let command = "Write-Host -NoNewline $env:SYMLINK_CONFIG_MARK";
    #[cfg(not(windows))]
    let command = "printf '%s' \"$SYMLINK_CONFIG_MARK\"";
    aft.send(
        &json!({
            "id": "live-reload-env",
            "command": "bash",
            "params": { "command": command, "foreground_orchestrate": true, "wait": true }
        })
        .to_string(),
    )
}

fn wait_for_bash_env(aft: &mut AftProcess, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        last = bash_env(aft);
        if last["success"] == true
            && last["output"]
                .as_str()
                .is_some_and(|output| output.contains(expected))
        {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("bash.env never became {expected}; last response: {last:?}");
}

#[cfg(any(unix, windows))]
#[test]
fn symlinked_user_config_target_edits_apply_live() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let home = dir.path().join("home");
    let xdg = dir.path().join("xdg");
    let dotfiles = dir.path().join("dotfiles");
    for path in [&root, &home, &xdg.join("cortexkit"), &dotfiles] {
        fs::create_dir_all(path).unwrap();
    }
    // The target deliberately has a different name and is outside the project
    // and the link's directory. Its writes cannot wake either existing watch.
    let target = dotfiles.join("settings.jsonc");
    let user_config = xdg.join("cortexkit/aft.jsonc");
    write_env_config(&target, "symlink-initial");
    config_symlink(&target, &user_config);
    let mut aft = AftProcess::spawn_with_real_watcher_env(&[
        ("HOME", home.as_os_str()),
        ("XDG_CONFIG_HOME", xdg.as_os_str()),
    ]);
    configure(&mut aft, &root, &user_config);
    wait_for_bash_env(&mut aft, "symlink-initial");

    write_env_config(&target, "symlink-edited");
    wait_for_bash_env(&mut aft, "symlink-edited");

    let replacement = dotfiles.join("save.jsonc");
    write_env_config(&replacement, "symlink-atomic");
    fs::rename(&replacement, &target).unwrap();
    wait_for_bash_env(&mut aft, "symlink-atomic");
    write_env_config(&target, "symlink-after-atomic");
    wait_for_bash_env(&mut aft, "symlink-after-atomic");

    fs::remove_file(&target).unwrap();
    thread::sleep(Duration::from_millis(700));
    assert!(bash_env(&mut aft)["output"]
        .as_str()
        .unwrap()
        .contains("symlink-after-atomic"));
    write_env_config(&target, "symlink-recovered");
    wait_for_bash_env(&mut aft, "symlink-recovered");

    let other = dir.path().join("other-dotfiles");
    fs::create_dir_all(&other).unwrap();
    let other_target = other.join("alternate.jsonc");
    write_env_config(&other_target, "symlink-retargeted");
    fs::remove_file(&user_config).unwrap();
    config_symlink(&other_target, &user_config);
    wait_for_bash_env(&mut aft, "symlink-retargeted");
    write_env_config(&other_target, "symlink-retarget-edit");
    wait_for_bash_env(&mut aft, "symlink-retarget-edit");
}

#[cfg(any(unix, windows))]
#[test]
fn symlinked_project_config_directory_target_edits_apply_live() {
    let _watcher_guard = crate::helpers::watcher_serial_lock();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let dotfiles = dir.path().join("dotfiles");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&dotfiles).unwrap();
    let target = dotfiles.join("aft.jsonc");
    write_env_config(&target, "project-symlink-initial");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&dotfiles, root.join(".cortexkit")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&dotfiles, root.join(".cortexkit")).unwrap();
    let user_config = dir.path().join("user.jsonc");
    fs::write(&user_config, "{}").unwrap();

    // AftProcess gives each child its own throwaway HOME and XDG directories.
    let mut aft = AftProcess::spawn_with_real_watcher();
    configure(&mut aft, &root, &user_config);
    let trust = aft.send(
        &json!({
            "id": "trust-config-project",
            "command": "trust_filter_project",
            "params": { "project_root": root }
        })
        .to_string(),
    );
    assert_eq!(trust["success"], true, "trust failed: {trust:?}");
    wait_for_bash_env(&mut aft, "project-symlink-initial");
    write_env_config(&target, "project-symlink-edited");
    wait_for_bash_env(&mut aft, "project-symlink-edited");
}
