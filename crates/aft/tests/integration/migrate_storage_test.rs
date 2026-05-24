use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use aft::harness::Harness;
use aft::migrate_storage::{Args, ExitStatus, Options};

fn args(from: PathBuf, to: PathBuf, log: PathBuf) -> Args {
    Args {
        from: Some(from),
        to,
        harness: Harness::Opencode,
        log: Some(log),
        status: false,
    }
}

fn run(args: Args) -> ExitStatus {
    aft::migrate_storage::run_with_options(args, Options::default())
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn populate_legacy(root: &Path) {
    write(
        &root.join("bash-tasks/session-a/bash-1.json"),
        r#"{"task_id":"bash-1"}"#,
    );
    write(&root.join("bash-tasks/session-a/bash-1.stdout"), "out");
    write(&root.join("bash-tasks/session-a/bash-1.stderr"), "err");
    write(&root.join("bash-tasks/session-a/bash-1.exit"), "0");
    write(
        &root.join("backups/session-a/path-a/meta.json"),
        r#"{"entries":[]}"#,
    );
    write(&root.join("filters/project-a/rules.toml"), "[rules]\n");
    write(&root.join("index/project-a/postings.bin"), "index");
    write(&root.join("semantic/project-a/semantic.bin"), "semantic");
    write(&root.join("symbols/project-a/symbols.bin"), "symbols");
    write(&root.join("last_announced_version"), "0.26.4");
    write(&root.join("last-update-check.json"), r#"{"checked":true}"#);
    write(&root.join("warned_tools.json"), r#"{"bash":true}"#);
    write(
        &root.join("trusted-filter-projects.json"),
        r#"["/project-a"]"#,
    );
}

fn assert_jsonl_parseable(path: &Path) {
    let contents = fs::read_to_string(path).unwrap();
    assert!(!contents.trim().is_empty());
    for line in contents.lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap();
    }
}

#[test]
fn migrate_empty_source_succeeds_as_noop() {
    let temp = tempfile::tempdir().unwrap();
    let from = temp.path().join("missing-legacy");
    let to = temp.path().join("new");
    let log = temp.path().join("logs/migration.jsonl");

    let status = run(args(from, to.clone(), log));

    assert_eq!(status, ExitStatus::Success);
    assert!(to.join("opencode/.migrated_from_legacy").exists());
}

#[test]
fn migrate_happy_path_copies_all_subtrees() {
    let temp = tempfile::tempdir().unwrap();
    let from = temp.path().join("legacy");
    let to = temp.path().join("new");
    let log = temp.path().join("logs/migration.jsonl");
    populate_legacy(&from);

    let status = run(args(from.clone(), to.clone(), log.clone()));

    assert_eq!(status, ExitStatus::Success);
    assert!(to
        .join("opencode/bash-tasks/session-a/bash-1.json")
        .exists());
    assert!(to
        .join("opencode/bash-tasks/session-a/bash-1.exit")
        .exists());
    assert!(to
        .join("opencode/backups/session-a/path-a/meta.json")
        .exists());
    assert!(to.join("opencode/filters/project-a/rules.toml").exists());
    assert!(to.join("opencode/last_announced_version").exists());
    assert!(to.join("opencode/last-update-check.json").exists());
    assert!(to.join("opencode/warned_tools.json").exists());
    assert!(to.join("index/project-a/postings.bin").exists());
    assert!(to.join("semantic/project-a/semantic.bin").exists());
    assert!(to.join("symbols/project-a/symbols.bin").exists());
    assert!(to.join("trusted-filter-projects.json").exists());
    assert!(from.join(".migrated_to_cortexkit").exists());
    assert!(to.join("opencode/.migrated_from_legacy").exists());
    assert_jsonl_parseable(&log);
}

#[test]
fn migrate_idempotent_when_both_markers_exist() {
    let temp = tempfile::tempdir().unwrap();
    let from = temp.path().join("legacy");
    let to = temp.path().join("new");
    let log = temp.path().join("migration.jsonl");
    populate_legacy(&from);
    let first = run(args(from.clone(), to.clone(), log.clone()));
    let second = run(args(from, to, log));

    assert_eq!(first, ExitStatus::Success);
    assert_eq!(second, ExitStatus::Success);
}

#[test]
fn migrate_resumes_when_only_source_marker_exists() {
    let temp = tempfile::tempdir().unwrap();
    let from = temp.path().join("legacy");
    let to = temp.path().join("new");
    let log = temp.path().join("migration.jsonl");
    populate_legacy(&from);
    write(&from.join(".migrated_to_cortexkit"), "{}\n");

    let status = run(args(from, to.clone(), log));

    assert_eq!(status, ExitStatus::Success);
    assert!(to
        .join("opencode/bash-tasks/session-a/bash-1.json")
        .exists());
    assert!(to.join("opencode/.migrated_from_legacy").exists());
}

#[test]
fn migrate_preflight_fails_when_insufficient_disk() {
    let temp = tempfile::tempdir().unwrap();
    let from = temp.path().join("legacy");
    let to = temp.path().join("new");
    write(&from.join("bash-tasks/session-a/bash-1.json"), "1234567890");

    let status = aft::migrate_storage::run_with_options(
        args(from, to, temp.path().join("migration.jsonl")),
        Options {
            lock_timeout: Duration::from_secs(30),
            disk_free_override: Some(1),
        },
    );

    assert_eq!(status, ExitStatus::InsufficientDisk);
}

#[test]
fn migrate_lock_contention_returns_code_3() {
    let temp = tempfile::tempdir().unwrap();
    let from = temp.path().join("missing");
    let to = temp.path().join("new");
    fs::create_dir_all(to.join(".aft")).unwrap();
    let _guard = aft::fs_lock::acquire(&to.join(".aft/migration.lock")).unwrap();

    let status = aft::migrate_storage::run_with_options(
        args(from, to, temp.path().join("migration.jsonl")),
        Options {
            lock_timeout: Duration::from_millis(10),
            disk_free_override: Some(u64::MAX),
        },
    );

    assert_eq!(status, ExitStatus::LockContention);
}

#[cfg(unix)]
#[test]
fn migrate_partial_failure_leaves_staging_dirs() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let from = temp.path().join("legacy");
    let to = temp.path().join("new");
    write(&from.join("bash-tasks/session-a/bash-1.json"), "{}");
    write(&from.join("backups/session-a/path-a/meta.json"), "secret");
    write(&from.join("semantic/project-a/semantic.bin"), "semantic");
    let unreadable = from.join("backups/session-a/path-a/meta.json");
    let mut perms = fs::metadata(&unreadable).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&unreadable, perms).unwrap();

    let status = run(args(
        from.clone(),
        to.clone(),
        temp.path().join("migration.jsonl"),
    ));

    let mut restore = fs::metadata(&unreadable).unwrap().permissions();
    restore.set_mode(0o644);
    fs::set_permissions(&unreadable, restore).unwrap();

    assert_eq!(status, ExitStatus::MigrationFailed);
    assert!(to
        .join("opencode/bash-tasks/session-a/bash-1.json")
        .exists());
    assert!(to.join("semantic/project-a/semantic.bin").exists());
    let staging = fs::read_dir(to.join("opencode"))
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("staging-backups")
        });
    assert!(
        staging,
        "failed subtree should leave a staging-backups directory"
    );
}

#[cfg(unix)]
#[test]
fn migrate_writes_markers_only_on_full_success() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let from = temp.path().join("legacy");
    let to = temp.path().join("new");
    write(&from.join("backups/session-a/path-a/meta.json"), "secret");
    let unreadable = from.join("backups/session-a/path-a/meta.json");
    let mut perms = fs::metadata(&unreadable).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&unreadable, perms).unwrap();

    let status = run(args(
        from.clone(),
        to.clone(),
        temp.path().join("migration.jsonl"),
    ));

    let mut restore = fs::metadata(&unreadable).unwrap().permissions();
    restore.set_mode(0o644);
    fs::set_permissions(&unreadable, restore).unwrap();

    assert_eq!(status, ExitStatus::MigrationFailed);
    assert!(!from.join(".migrated_to_cortexkit").exists());
    assert!(!to.join("opencode/.migrated_from_legacy").exists());
}

#[test]
fn migrate_logs_to_file_not_stderr() {
    let temp = tempfile::tempdir().unwrap();
    let from = temp.path().join("legacy");
    let to = temp.path().join("new");
    let log = temp.path().join("logs/migration.jsonl");
    populate_legacy(&from);

    let output = Command::new(env!("CARGO_BIN_EXE_aft"))
        .arg("migrate-storage")
        .arg("--from")
        .arg(&from)
        .arg("--to")
        .arg(&to)
        .arg("--harness")
        .arg("opencode")
        .arg("--log")
        .arg(&log)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_jsonl_parseable(&log);
}

#[test]
fn status_mode_reports_migrated_when_marker_present() {
    let temp = tempfile::tempdir().unwrap();
    let to = temp.path().join("new");
    let marker = to.join("opencode/.migrated_from_legacy");
    write(
        &marker,
        r#"{"timestamp":"2026-05-19T15:00:00.123Z","source_path":"/legacy/aft","target_path":"/new/aft","harness":"opencode","aft_version":"0.27.0"}"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aft"))
        .arg("migrate-storage")
        .arg("--status")
        .arg("--to")
        .arg(&to)
        .arg("--harness")
        .arg("opencode")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["harness"], "opencode");
    assert_eq!(value["target_root"], to.display().to_string());
    assert_eq!(value["migrated"], true);
    assert_eq!(value["marker_path"], marker.display().to_string());
    assert_eq!(value["migrated_at"], "2026-05-19T15:00:00.123Z");
    assert_eq!(value["source_path"], "/legacy/aft");
    assert_eq!(value["aft_version"], "0.27.0");
}

#[test]
fn status_mode_reports_not_migrated_when_marker_absent() {
    let temp = tempfile::tempdir().unwrap();
    let to = temp.path().join("new");

    let output = Command::new(env!("CARGO_BIN_EXE_aft"))
        .arg("migrate-storage")
        .arg("--status")
        .arg("--to")
        .arg(&to)
        .arg("--harness")
        .arg("opencode")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["harness"], "opencode");
    assert_eq!(value["target_root"], to.display().to_string());
    assert_eq!(value["migrated"], false);
    assert!(value.get("marker_path").is_none());
}

#[test]
fn status_mode_does_not_acquire_lock() {
    let temp = tempfile::tempdir().unwrap();
    let to = temp.path().join("new");
    fs::create_dir_all(to.join(".aft")).unwrap();
    let _guard = aft::fs_lock::acquire(&to.join(".aft/migration.lock")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aft"))
        .arg("migrate-storage")
        .arg("--status")
        .arg("--to")
        .arg(&to)
        .arg("--harness")
        .arg("opencode")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
}

#[test]
fn status_mode_does_not_require_from_or_log() {
    let parsed =
        aft::migrate_storage::parse_cli_args(["--status", "--to", "/new", "--harness", "opencode"])
            .unwrap();

    assert!(parsed.status);
    assert!(parsed.from.is_none());
    assert!(parsed.log.is_none());
}

#[test]
fn migrate_filter_merge_skips_existing_child() {
    let temp = tempfile::tempdir().unwrap();
    let from = temp.path().join("legacy");
    let to = temp.path().join("new");
    write(&from.join("filters/projectA/source.toml"), "source");
    write(&from.join("filters/projectB/source.toml"), "source-b");
    write(
        &to.join("opencode/filters/projectA/existing.toml"),
        "existing",
    );

    let status = run(args(from, to.clone(), temp.path().join("migration.jsonl")));

    assert_eq!(status, ExitStatus::Success);
    assert_eq!(
        fs::read_to_string(to.join("opencode/filters/projectA/existing.toml")).unwrap(),
        "existing"
    );
    assert!(!to.join("opencode/filters/projectA/source.toml").exists());
    assert!(to.join("opencode/filters/projectB/source.toml").exists());
}

#[test]
fn migrate_trust_file_merges_host_global() {
    let temp = tempfile::tempdir().unwrap();
    let from = temp.path().join("legacy");
    let to = temp.path().join("new");
    write(&from.join("trusted-filter-projects.json"), r#"["Y","Z"]"#);
    write(&to.join("trusted-filter-projects.json"), r#"["X","Y"]"#);

    let status = run(args(from, to.clone(), temp.path().join("migration.jsonl")));

    assert_eq!(status, ExitStatus::Success);
    let merged: Vec<String> =
        serde_json::from_slice(&fs::read(to.join("trusted-filter-projects.json")).unwrap())
            .unwrap();
    assert_eq!(merged, vec!["X", "Y", "Z"]);
}

#[test]
fn migrate_unknown_harness_returns_invalid_args() {
    let error = aft::migrate_storage::parse_cli_args([
        "--from",
        "/legacy",
        "--to",
        "/new",
        "--harness",
        "claude_code",
        "--log",
        "/new/migration.log",
    ])
    .unwrap_err();

    assert!(error.contains("invalid harness"));
}
