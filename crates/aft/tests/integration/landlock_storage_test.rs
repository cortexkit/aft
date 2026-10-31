use std::fs::{self, File, OpenOptions};
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::subc_detach_test::{
    accept_module, send_connection_goodbye, write_connection_file, ModuleProcess,
};

// Confinement is irreversible, so install it in a separate test process and
// then exec the real daemon. No test-runner thread or pre-opened directory is
// used as an escape from the storage allow-list.
#[test]
#[ignore]
fn confined_aft_child() {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, ABI,
    };

    let allowed = PathBuf::from(std::env::var_os("AFT_TEST_LANDLOCK_ALLOWED").unwrap());
    let binary = PathBuf::from(std::env::var_os("AFT_TEST_LANDLOCK_BINARY").unwrap());
    let abi = ABI::V3;
    let read = AccessFs::ReadFile | AccessFs::ReadDir;
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .unwrap()
        .create()
        .unwrap()
        .add_rule(PathBeneath::new(
            PathFd::new(&allowed).unwrap(),
            AccessFs::from_all(abi),
        ))
        .unwrap()
        .add_rule(PathBeneath::new(
            PathFd::new(&binary).unwrap(),
            AccessFs::ReadFile | AccessFs::Execute,
        ))
        .unwrap();
    // Shared-library and /proc access let the daemon load, copy its running
    // executable and check process identity, without reading storage ancestors.
    for path in ["/lib", "/lib64", "/usr/lib", "/usr/lib64"] {
        if Path::new(path).exists() {
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    PathFd::new(path).unwrap(),
                    read | AccessFs::Execute,
                ))
                .unwrap();
        }
    }
    ruleset = ruleset
        .add_rule(PathBeneath::new(PathFd::new("/proc").unwrap(), read))
        .unwrap();
    for path in ["/etc/ld.so.cache", "/dev/null"] {
        if Path::new(path).exists() {
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    PathFd::new(path).unwrap(),
                    AccessFs::ReadFile,
                ))
                .unwrap();
        }
    }
    assert_eq!(
        ruleset.restrict_self().unwrap().ruleset,
        RulesetStatus::FullyEnforced,
        "this regression requires real Landlock enforcement"
    );
    for path in [Path::new("/"), allowed.parent().unwrap()] {
        assert_eq!(
            File::open(path).unwrap_err().raw_os_error(),
            Some(libc::EACCES)
        );
    }
    eprintln!("Landlock: read-open of / and storage parent denied (EACCES)");

    if std::env::var_os("AFT_TEST_LANDLOCK_SYMLINK_PROBE").is_some() {
        refuse_symlink_below_readable_point(&allowed);
        return;
    }
    let error = Command::new(binary)
        .arg("--subc")
        .arg(allowed.join("subc-connection.json"))
        .exec();
    panic!("exec confined aft: {error}");
}

fn refuse_symlink_below_readable_point(allowed: &Path) {
    // A real symlink below the readable point must still fail, rather than
    // being mistaken for an ancestor whose read permission is unavailable.
    let untrusted = allowed.join("untrusted");
    fs::create_dir(&untrusted).unwrap();
    fs::set_permissions(&untrusted, fs::Permissions::from_mode(0o777)).unwrap();
    fs::create_dir(untrusted.join("real")).unwrap();
    let marker = aft::root_cache::ReadMarker::create(&untrusted.join("real"), "generation")
        .expect("ordinary ancestor walk must work with root reads denied");
    marker.touch().unwrap();
    drop(marker);
    std::os::unix::fs::symlink("real", untrusted.join("link")).unwrap();
    let error = aft::root_cache::ReadMarker::create(&untrusted.join("link"), "generation")
        .err()
        .expect("symlinked cache ancestor must be refused under Landlock");
    assert!(
        matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
            || error.to_string().contains("Not a directory")
            || error.to_string().contains("Too many levels"),
        "symlink refusal must not be a permission-denied shortcut: {error}"
    );
    eprintln!("Landlock: symlinked storage ancestor below readable point refused");

    // Opening an existing SQLite file must tighten its permissions, but never
    // chmod through an untrusted symlinked ancestor.
    let database = untrusted.join("real/legacy.db");
    drop(rusqlite::Connection::open(&database).unwrap());
    fs::set_permissions(&database, fs::Permissions::from_mode(0o644)).unwrap();
    let open_readonly = |path: &Path| {
        aft::db::lifecycle::TrackedConnection::open_with_flags(
            path.to_str().unwrap(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            aft::db::lifecycle::SqliteStore::AftDb,
        )
        .unwrap()
    };
    drop(open_readonly(&database));
    assert_eq!(
        fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::set_permissions(&database, fs::Permissions::from_mode(0o644)).unwrap();
    drop(open_readonly(&untrusted.join("link/legacy.db")));
    assert_eq!(
        fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

fn confined_child_command(allowed: &Path, storage: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "landlock_storage_test::confined_aft_child",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", allowed)
        .env("XDG_CONFIG_HOME", allowed.join("config"))
        .env("XDG_DATA_HOME", allowed.join("data"))
        .env("AFT_STORAGE_DIR", storage)
        .env("AFT_TEST_RAW_PATH", "1")
        .env("AFT_TEST_SUBC_STARTUP_TRACE", "1")
        .env("AFT_TEST_LANDLOCK_ALLOWED", allowed)
        .env(
            "AFT_TEST_LANDLOCK_BINARY",
            crate::test_helpers::aft_binary::aft_binary(),
        )
        .current_dir(allowed)
        .stdin(Stdio::null())
        .process_group(0);
    command
}

#[test]
fn storage_lease_landlock_still_refuses_symlinked_ancestor() {
    let fixture = tempfile::tempdir().unwrap();
    let allowed = fixture.path().canonicalize().unwrap();
    let output = confined_child_command(&allowed, &allowed.join("storage"))
        .env("AFT_TEST_LANDLOCK_SYMLINK_PROBE", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("symlinked storage ancestor below readable point refused"),
        "{output:?}"
    );
}

#[tokio::test]
async fn storage_lease_starts_without_reading_root_under_landlock() {
    for fresh_storage in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let allowed = fixture.path().canonicalize().unwrap();
        let storage = allowed.join("storage");
        if !fresh_storage {
            fs::create_dir(&storage).unwrap();
        }
        let stderr_path = allowed.join("child.stderr");
        let listener = write_connection_file(&allowed).await;
        let child = confined_child_command(&allowed, &storage)
            .stdout(Stdio::null())
            .stderr(File::create(&stderr_path).unwrap())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut module = ModuleProcess { child };
        let mut stream = tokio::select! {
            stream = accept_module(&listener) => stream,
            () = async {
                loop {
                    if let Some(status) = module.child.try_wait().unwrap() {
                        panic!("confined aft exited {status}: {}", fs::read_to_string(&stderr_path).unwrap());
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            } => unreachable!(),
        };
        let stderr = fs::read_to_string(&stderr_path).unwrap();
        assert!(
            stderr.contains("read-open of / and storage parent denied"),
            "{stderr}"
        );
        for phase in ["storage_lease_done", "executable_pin_done", "logging_done"] {
            assert!(stderr.contains(phase), "missing {phase}: {stderr}");
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(storage.join("retention/daemon-use.lock"))
            .unwrap();
        assert!(
            matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "daemon did not hold its storage lease"
        );
        assert!(
            fs::read_dir(storage.join("run"))
                .unwrap()
                .any(|entry| { entry.unwrap().path().join("aft").is_file() }),
            "daemon did not materialize its startup image"
        );
        assert!(fs::read_dir(storage.join("logs")).unwrap().next().is_some());
        send_connection_goodbye(&mut stream).await;
        assert!(module.wait_for_exit("Landlock daemon clean exit").success());
        lock.try_lock()
            .expect("daemon must release its lease on exit");
    }
}
