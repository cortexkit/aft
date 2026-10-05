use super::*;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn database(sql: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let created = Command::new("sqlite3")
        .arg(dir.path().join("store.db"))
        .arg(sql)
        .output()
        .expect("sqlite3 is required");
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    dir
}

fn failure(dir: &Path, command: &str, enabled: bool) -> Option<String> {
    let result = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(dir)
        .output()
        .unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let job = HintJob::new(
        command,
        dir,
        &HashMap::new(),
        &SpawnPlan::Unsandboxed,
        enabled,
    );
    job.finish(&output, result.status.code())
        .map(str::to_string)
}

#[test]
fn real_missing_table() {
    let dir = database("CREATE TABLE tasks(id TEXT); CREATE TABLE asks(id INTEGER);");
    let hint = failure(
        dir.path(),
        "sqlite3 store.db 'SELECT * FROM manager_tasks'",
        true,
    )
    .unwrap();
    assert!(hint.contains("[aft: no table 'manager_tasks'"), "{hint}");
    assert!(hint.contains("asks, tasks"), "{hint}");
}

#[test]
fn real_candidate_tables_from_join() {
    let dir = database("CREATE TABLE tasks(id TEXT, kind TEXT); CREATE TABLE asks(id INTEGER, message TEXT); CREATE TABLE unrelated(secret BLOB);");
    let hint = failure(
        dir.path(),
        "sqlite3 store.db 'SELECT t.substrate FROM tasks t JOIN asks a ON a.id=t.id'",
        true,
    )
    .unwrap();
    assert!(hint.contains("tasks: id TEXT, kind TEXT"), "{hint}");
    assert!(hint.contains("asks: id INTEGER, message TEXT"), "{hint}");
    assert!(!hint.contains("unrelated"), "{hint}");
}

#[test]
fn real_column_without_from_single_table_and_table_fallback() {
    let dir = database("CREATE TABLE tasks(id TEXT, kind TEXT);");
    let command = "sqlite3 store.db 'SELECT substrate'";
    let hint = failure(dir.path(), command, true).unwrap();
    assert!(hint.contains("tasks: id TEXT, kind TEXT"), "{hint}");
    Command::new("sqlite3")
        .arg(dir.path().join("store.db"))
        .arg("CREATE TABLE asks(id INTEGER)")
        .output()
        .unwrap();
    let hint = failure(dir.path(), command, true).unwrap();
    assert!(hint.contains("2 tables"), "{hint}");
    assert!(hint.contains("asks, tasks"), "{hint}");
}

#[test]
fn real_similarity_survives_table_cap() {
    let mut sql = "CREATE TABLE manager_task(id TEXT);".to_string();
    for i in 0..600 {
        sql.push_str(&format!("CREATE TABLE aaa_unrelated_{i:04}(id TEXT);"));
    }
    let dir = database(&sql);
    let hint = failure(
        dir.path(),
        "sqlite3 store.db 'SELECT * FROM manager_tasks'",
        true,
    )
    .unwrap();
    assert!(hint.contains("601 tables"), "{hint}");
    assert!(
        hint.lines().nth(1).unwrap().starts_with("manager_task"),
        "{hint}"
    );
    assert!(hint.contains("tables omitted"), "{hint}");
    assert!(hint.len() <= BLOCK_CAP, "{}", hint.len());
}

#[test]
fn real_cd_pipeline_detection() {
    let dir = database("CREATE TABLE tasks(id TEXT);");
    let parent = dir.path().parent().unwrap();
    let basename = dir.path().file_name().unwrap().to_str().unwrap();
    let command = format!("cd '{basename}' && sqlite3 store.db 'SELECT missing FROM tasks' | cat");
    let hint = failure(parent, &command, true).unwrap();
    assert!(hint.contains("tasks: id TEXT"), "{hint}");
}

#[test]
fn real_switch_off() {
    let dir = database("CREATE TABLE tasks(id TEXT);");
    assert!(failure(
        dir.path(),
        "sqlite3 store.db 'SELECT missing FROM tasks'",
        false
    )
    .is_none());
}

#[test]
fn real_success_and_unsupported_targets_never_hint() {
    let dir = database("CREATE TABLE tasks(id TEXT);");
    assert!(failure(dir.path(), "sqlite3 store.db 'SELECT id FROM tasks'", true).is_none());
    assert!(failure(
        dir.path(),
        "sqlite3 store.db \"SELECT 'no such column: missing'\"",
        true
    )
    .is_none());
    for command in [
        "sqlite3 :memory: 'SELECT missing'",
        "sqlite3 \"$DB\" 'SELECT missing'",
        "sqlite3 'file:store.db?mode=rwc' 'SELECT missing'",
        "sqlite3 *.db 'SELECT missing'",
        "printf 'no such column: missing'",
    ] {
        assert!(detect(command, dir.path()).is_none(), "{command}");
    }
}

#[test]
fn real_readonly_database_and_quoted_identifiers() {
    let dir = database("CREATE TABLE \"task items\"(\"odd'column\" TEXT, id INTEGER);");
    std::fs::set_permissions(
        dir.path().join("store.db"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let hint = failure(
        dir.path(),
        "sqlite3 -readonly store.db 'SELECT missing FROM \"task items\"'",
        true,
    )
    .unwrap();
    assert!(
        hint.contains("task items: odd'column TEXT, id INTEGER"),
        "{hint}"
    );
    assert!(
        query_tables("SELECT 'FROM fake' FROM \"task items\" -- JOIN wrong\n")
            .contains(&"task items".to_string())
    );
}

#[test]
fn real_probe_timeout_is_counted_and_once_only() {
    let dir = database("CREATE TABLE tasks(id TEXT);");
    let binary = dir.path().join("sqlite3");
    std::fs::write(&binary, "#!/bin/sh\nsleep 5\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let command = format!(
        "'{}' store.db 'SELECT missing FROM tasks'",
        binary.display()
    );
    let before = METRICS.probe_timeout.load(Ordering::Relaxed);
    let job = HintJob::new(
        &command,
        dir.path(),
        &HashMap::new(),
        &SpawnPlan::Unsandboxed,
        true,
    );
    let started = Instant::now();
    assert!(job.finish("no such column: missing", Some(1)).is_none());
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(METRICS.probe_timeout.load(Ordering::Relaxed) > before);
    let before = METRICS.probe_timeout.load(Ordering::Relaxed);
    assert!(job.finish("no such column: missing", Some(1)).is_none());
    assert_eq!(METRICS.probe_timeout.load(Ordering::Relaxed), before);
}

#[test]
fn real_probe_error_is_counted_and_does_not_create_database() {
    let dir = tempfile::tempdir().unwrap();
    let before = METRICS.probe_error.load(Ordering::Relaxed);
    let job = HintJob::new(
        "sqlite3 absent.db 'SELECT missing'",
        dir.path(),
        &HashMap::new(),
        &SpawnPlan::Unsandboxed,
        true,
    );
    assert!(job.finish("no such column: missing", Some(1)).is_none());
    assert!(!dir.path().join("absent.db").exists());
    assert!(METRICS.probe_error.load(Ordering::Relaxed) > before);
}

#[test]
fn real_probe_uses_same_launcher_and_no_user_flags() {
    use crate::sandbox_profile::SandboxProfile;
    let dir = database("CREATE TABLE tasks(id TEXT);");
    let launcher = dir.path().join("launcher");
    let record = dir.path().join("probe-argv");
    std::fs::write(&launcher, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n# Real launchers report unenforceable rules on stderr; that must not cost the hint.\necho 'sandbox-launch: unenforced=[socket_deny]' >&2\n[ \"$1\" = sandbox-launch ] || exit 1\nshift 3\n[ \"$1\" = -- ] || exit 1\nshift\nexec \"$@\"\n", record.display())).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let profile = SandboxProfile {
        data_policy: Default::default(),
        v: crate::sandbox_profile::SANDBOX_PROFILE_VERSION,
        writable_roots: vec![dir.path().to_path_buf()],
        write_deny: vec![],
        write_deny_nested: vec![],
        read_allow: vec![],
        read_deny: vec![],
        socket_deny: vec![],
        cache_roots: vec![],
        temp_dir: dir.path().to_path_buf(),
    };
    let plan = SpawnPlan::launcher_for_test(profile, launcher);
    let job = HintJob::new(
        "sqlite3 -separator ignored -csv store.db 'SELECT missing FROM tasks'",
        dir.path(),
        &HashMap::new(),
        &plan,
        true,
    );
    let hint = job
        .finish("Error: in prepare, no such column: missing", Some(1))
        .unwrap();
    assert!(hint.contains("tasks: id TEXT"), "{hint}");
    let args = std::fs::read_to_string(record).unwrap();
    assert!(args.contains("sandbox-launch\n--profile-json\n"), "{args}");
    assert!(args.contains("-readonly\n"), "{args}");
    assert!(!args.contains("-cmd\n"), "{args}");
    assert!(!args.contains("ignored"), "{args}");
}

#[test]
fn real_readonly_uri_and_startup_connection_changes() {
    let dir = database("CREATE TABLE tasks(id TEXT);");
    let hint = failure(
        dir.path(),
        "sqlite3 'file:store.db?mode=ro' 'SELECT missing FROM tasks'",
        true,
    )
    .unwrap();
    assert!(hint.contains("tasks: id TEXT"), "{hint}");
    for command in [
        "sqlite3 'file:store.db?mode=rw' 'SELECT missing'",
        "sqlite3 -cmd '.open other.db' store.db 'SELECT missing'",
        "sqlite3 -init init.sql store.db 'SELECT missing'",
        "sqlite3 store.db '.open other.db'",
    ] {
        assert!(detect(command, dir.path()).is_none(), "{command}");
    }
}
