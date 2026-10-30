use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

struct BoundedChild {
    child: Child,
    stdout: fs::File,
    stderr: fs::File,
    label: String,
}

impl BoundedChild {
    fn spawn(command: &mut Command, label: &str) -> Self {
        // Capture in files so waiting for the child does not depend on every
        // inherited pipe writer closing its output handle.
        let stdout = tempfile::tempfile().unwrap();
        let stderr = tempfile::tempfile().unwrap();
        command
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        Self {
            child: command
                .spawn()
                .unwrap_or_else(|error| panic!("spawn {label}: {error}")),
            stdout,
            stderr,
            label: label.into(),
        }
    }

    fn captured(&mut self) -> (Vec<u8>, Vec<u8>) {
        let read = |file: &mut fs::File| {
            file.seek(SeekFrom::Start(0)).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            bytes
        };
        (read(&mut self.stdout), read(&mut self.stderr))
    }

    fn wait(&mut self, timeout: Duration, waiting_for: &str) -> Result<ExitStatus, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                let (stdout, stderr) = self.captured();
                return Err(format!(
                    "{}: deadline {timeout:?} exceeded waiting for {waiting_for}; stdout: {}; stderr: {}",
                    self.label, String::from_utf8_lossy(&stdout), String::from_utf8_lossy(&stderr)
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn output(mut self, timeout: Duration) -> Output {
        let status = self
            .wait(timeout, "process exit")
            .unwrap_or_else(|error| panic!("{error}"));
        let (stdout, stderr) = self.captured();
        Output {
            status,
            stdout,
            stderr,
        }
    }

    fn await_ready(&mut self, ready: &Path, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        while !ready.is_file() {
            let status = self.child.try_wait().unwrap();
            if status.is_some() || Instant::now() >= deadline {
                let (stdout, stderr) = self.captured();
                let reason = if status.is_some() {
                    "child exited"
                } else {
                    "deadline exceeded"
                };
                return Err(format!(
                    "{}: {reason} waiting for storage-lock acquisition readiness at {} (deadline {timeout:?}, status {status:?}); stdout: {}; stderr: {}",
                    self.label, ready.display(), String::from_utf8_lossy(&stdout), String::from_utf8_lossy(&stderr)
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

impl Drop for BoundedChild {
    fn drop(&mut self) {
        if self.child.try_wait().is_ok_and(|status| status.is_some()) {
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        if let Err(error) = self.wait(Duration::from_secs(2), "killed child to be reaped") {
            eprintln!("{error}");
        }
    }
}

struct StorageLockChild {
    process: BoundedChild,
    signals: tempfile::TempDir,
}

impl StorageLockChild {
    fn spawn(storage: &Path, home: &Path) -> Self {
        let signals = tempfile::tempdir().unwrap();
        let mut command = fixture_command("cache_root_storage_lock_child");
        command
            .env("AFT_TEST_CACHE_STORAGE_LOCK_CHILD", storage)
            .env("AFT_TEST_CACHE_LOCK_SIGNALS", signals.path())
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_RUNTIME_DIR", home.join("runtime"));
        let mut process = BoundedChild::spawn(&mut command, "storage-lock child");
        process
            .await_ready(&signals.path().join("ready"), CHILD_TIMEOUT)
            .unwrap_or_else(|error| panic!("{error}"));
        Self { process, signals }
    }

    fn stop(self) {
        fs::write(self.signals.path().join("release"), b"release").unwrap();
        let output = self.process.output(CHILD_TIMEOUT);
        assert!(output.status.success(), "storage-lock release: {output:?}");
    }
}

fn fixture_command(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        &format!("cache_root_doctor_test::{name}"),
        "--nocapture",
        "--ignored",
    ]);
    command
}

#[cfg(unix)]
fn process_snapshot_fixture(temp: &Path, snapshot: &str) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt;
    let bin = temp.join("process-snapshot-bin");
    fs::create_dir(&bin).unwrap();
    let ps = bin.join("ps");
    // Exercise the normal scan subprocesses without consulting the host's
    // daemons. The scan asks `ps` twice, for executables (`pid=,comm=`) and
    // for command lines (`pid=,args=`); the snapshot is given as command
    // lines whose first word is the executable. A real scan lists the
    // scanning aft process itself (the script's parent), and a scan that does
    // not is refused as incomplete, so it is listed first.
    fs::write(
        &ps,
        "#!/bin/sh\ncase \"$2\" in\n*comm=*) echo \"$PPID /opt/aft\"; exec /usr/bin/awk '{print $1, $2}' \"$0.snapshot\" ;;\n*) echo \"$PPID /opt/aft fix-config\"; exec /bin/cat \"$0.snapshot\" ;;\nesac\n",
    )
    .unwrap();
    fs::write(bin.join("ps.snapshot"), snapshot).unwrap();
    fs::set_permissions(ps, fs::Permissions::from_mode(0o700)).unwrap();
    let mut paths = vec![bin];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(paths).unwrap()
}

#[test]
#[ignore = "subprocess fixture started by the cache-root doctor test"]
fn cache_root_storage_lock_child() {
    let storage = std::env::var_os("AFT_TEST_CACHE_STORAGE_LOCK_CHILD")
        .expect("storage root supplied by the parent test");
    let signals = PathBuf::from(std::env::var_os("AFT_TEST_CACHE_LOCK_SIGNALS").unwrap());
    let _lease = aft::root_cache::acquire_daemon_storage_lease(Path::new(&storage)).unwrap();
    fs::write(signals.join("ready"), b"ready").unwrap();
    // An explicit release signal does not depend on every inherited pipe writer closing.
    let deadline = Instant::now() + Duration::from_secs(90);
    while !signals.join("release").is_file() {
        assert!(
            Instant::now() < deadline,
            "storage-lock child: deadline exceeded waiting for parent release signal"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "delay fixture for the bounded child regression"]
fn cache_root_delayed_child() {
    std::thread::sleep(Duration::from_secs(1));
}

#[test]
fn cache_root_doctor_child_waits_have_named_deadlines() {
    let temp = tempfile::tempdir().unwrap();
    let mut child = BoundedChild::spawn(
        &mut fixture_command("cache_root_delayed_child"),
        "delayed exit fixture",
    );
    let error = child
        .wait(Duration::from_millis(50), "fixture exit")
        .unwrap_err();
    assert!(error.contains("delayed exit fixture"), "{error}");
    assert!(error.contains("deadline"), "{error}");
    assert!(error.contains("waiting for fixture exit"), "{error}");
    drop(child);
    let mut child = BoundedChild::spawn(
        &mut fixture_command("cache_root_delayed_child"),
        "missing readiness fixture",
    );
    let error = child
        .await_ready(&temp.path().join("never-ready"), Duration::from_millis(50))
        .unwrap_err();
    assert!(error.contains("missing readiness fixture"), "{error}");
    assert!(
        error.contains("deadline exceeded waiting for storage-lock acquisition readiness"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn cache_root_doctor_process_scan_timeout_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("storage")).unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let ps = bin.join("ps");
    fs::write(&ps, "#!/bin/sh\nexec /bin/sleep 12\n").unwrap();
    fs::set_permissions(ps, fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_aft"));
    command
        .arg("fix-config")
        .current_dir(temp.path())
        .env("PATH", bin)
        .env("HOME", temp.path().join("home"))
        .env("USERPROFILE", temp.path().join("home"))
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_DATA_HOME", temp.path().join("data"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .env("AFT_STORAGE_DIR", temp.path().join("storage"));
    let output = BoundedChild::spawn(&mut command, "aft fix-config with stalled process scan")
        .output(CHILD_TIMEOUT);
    assert!(!output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let error = report["cache_cleanup"]["error"]
        .as_str()
        .expect("cleanup must fail closed on a stalled scan");
    assert!(error.contains("legacy-daemon process scan"), "{error}");
    assert!(
        error.contains("deadline 10s exceeded waiting for process snapshot stdout EOF"),
        "{error}"
    );
}

/// Every path under `root`, so a test can show a run created nothing.
#[cfg(unix)]
fn tree(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() && !path.is_symlink() {
                pending.push(path.clone());
            }
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

/// On a fresh install there is no storage root: the repair succeeds, cache
/// cleanup reports nothing to clean, and no storage folder is created.
#[cfg(unix)]
#[test]
fn cache_root_doctor_fresh_install_without_storage_succeeds_and_creates_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    // No daemon is listed, so a scan (if one ran) could not refuse.
    let path = process_snapshot_fixture(temp.path(), "");
    let before = tree(temp.path());
    let mut command = Command::new(env!("CARGO_BIN_EXE_aft"));
    command
        .arg("fix-config")
        .current_dir(&home)
        .env("PATH", path)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_DATA_HOME", temp.path().join("data"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .env_remove("AFT_STORAGE_DIR");
    let output = BoundedChild::spawn(&mut command, "aft fix-config on a fresh install")
        .output(CHILD_TIMEOUT);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["cache_cleanup"]["error"].is_null(), "{report}");
    assert_eq!(report["cache_cleanup"]["removed"], serde_json::json!([]));
    assert_eq!(
        tree(temp.path()),
        before,
        "fix-config created files: {report}"
    );
}

#[cfg(unix)]
#[test]
fn cache_root_doctor_prelease_process_snapshot_refuses_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let precious = canary(&storage, "index/keep");
    let path = process_snapshot_fixture(
        temp.path(),
        "999999 /opt/ck-aft --subc /run/connection.json\n",
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_aft"));
    command
        .arg("fix-config")
        .current_dir(temp.path())
        .env("PATH", path)
        .env("HOME", temp.path().join("home"))
        .env("USERPROFILE", temp.path().join("home"))
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_DATA_HOME", temp.path().join("data"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .env("AFT_STORAGE_DIR", storage);
    let output = BoundedChild::spawn(
        &mut command,
        "aft fix-config with pre-lease process snapshot",
    )
    .output(CHILD_TIMEOUT);
    assert!(!output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["cache_cleanup"]["error"]
            .as_str()
            .unwrap()
            .starts_with("legacy_daemon_upgrade_window: stop the running subc daemon"),
        "{report}"
    );
    assert!(precious.is_file());
}

fn canary(root: &Path, relative: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "keep").unwrap();
    path
}

/// Where the real-volume doctor scenario puts its cache roots. First
/// enrollment of a cache root needs a second filesystem. `AFT_TEST_CACHE_VOLUME`
/// names one explicitly (for example an external drive on macOS); otherwise
/// Linux CI's `/dev/shm` tmpfs is used when it is a different device from the
/// temporary directory. `Err` carries the reason the scenario cannot run here.
fn doctor_cache_volume() -> Result<PathBuf, String> {
    if let Some(volume) = std::env::var_os("AFT_TEST_CACHE_VOLUME").filter(|v| !v.is_empty()) {
        // An explicit volume is a request to run: a bad one fails in the
        // child rather than turning into a skip.
        return Ok(PathBuf::from(volume));
    }
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() {
        return Err("AFT_TEST_CACHE_VOLUME is not set and /dev/shm does not exist".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let temp = std::env::temp_dir();
        let shm_device = fs::metadata(shm).map_err(|error| format!("stat /dev/shm: {error}"))?;
        let temp_device =
            fs::metadata(&temp).map_err(|error| format!("stat {}: {error}", temp.display()))?;
        if shm_device.dev() == temp_device.dev() {
            return Err(format!(
                "AFT_TEST_CACHE_VOLUME is not set and /dev/shm is on the same device as {}",
                temp.display()
            ));
        }
        Ok(shm.to_path_buf())
    }
    #[cfg(not(unix))]
    {
        Err("AFT_TEST_CACHE_VOLUME is not set and /dev/shm is not a separate volume here".into())
    }
}

#[test]
fn native_doctor_fixes_only_marked_roots_and_old_cache_cleanup_is_explicit() {
    let volume = match doctor_cache_volume() {
        Ok(volume) => volume,
        Err(reason) => {
            // Printed so a skipped run is visible in the test output; set
            // AFT_TEST_CACHE_VOLUME to a mounted second filesystem to run it.
            eprintln!("SKIPPED native cache-root doctor scenario: {reason}");
            return;
        }
    };
    eprintln!(
        "cache-root doctor: running real-volume scenario on {}",
        volume.display()
    );
    let mut command = fixture_command("cache_root_doctor_body_child");
    command.env("AFT_TEST_CACHE_VOLUME", &volume);
    let output = BoundedChild::spawn(&mut command, "native cache-root doctor scenario")
        .output(Duration::from_secs(150));
    assert!(output.status.success(), "{output:?}");
    // The child prints this progress line only after it has prepared both
    // cache roots on the second volume, so its presence proves the scenario
    // ran rather than stopping early.
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("cache-root doctor: acquiring first child storage lease"),
        "{output:?}"
    );
}

#[test]
#[ignore = "real-volume scenario supervised by the cache-root doctor test"]
fn cache_root_doctor_body_child() {
    let temp = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let process_path = process_snapshot_fixture(temp.path(), "");
    // First enrollment requires a real second filesystem, chosen by the
    // supervising test and passed in AFT_TEST_CACHE_VOLUME.
    let volume = std::env::var_os("AFT_TEST_CACHE_VOLUME")
        .map(PathBuf::from)
        .expect("the supervising test supplies AFT_TEST_CACHE_VOLUME");
    let cache_volume = tempfile::tempdir_in(&volume).unwrap_or_else(|error| {
        panic!(
            "AFT_TEST_CACHE_VOLUME={} must be a mounted, writable second filesystem: {error}",
            volume.display()
        )
    });
    let storage = temp.path().join("storage");
    let legacy = canary(&storage, "index/old");
    let legacy_harness = canary(&storage, "opencode/callgraph/old");
    let protected = [
        "aft.db",
        "backups/keep",
        "checkpoints/keep",
        "bash-tasks/keep",
    ]
    .map(|path| canary(&storage, path));
    let mut external = Vec::new();
    for name in ["old-volume", "active-volume"] {
        eprintln!("cache-root doctor: preparing {name}");
        let parent = cache_volume.path().join(name);
        fs::create_dir(&parent).unwrap();
        let mut config = aft::config::Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(parent.clone()),
            ..Default::default()
        };
        assert!(aft::root_cache::prepare_cache_root(&mut config).is_none());
        let data = canary(&aft::root_cache::cache_root(&config), "index/cache");
        external.push((parent, data));
    }
    let home = temp.path().join("home");
    let xdg = temp.path().join("config");
    let user = xdg.join("cortexkit/aft.jsonc");
    fs::create_dir_all(user.parent().unwrap()).unwrap();
    fs::write(
        user,
        serde_json::json!({"storage":{"cache_dir":external[1].0}}).to_string(),
    )
    .unwrap();
    let run = |args: &[&str]| {
        let label = format!("aft {}", args.join(" "));
        eprintln!("cache-root doctor: waiting for {label}");
        let mut command = Command::new(env!("CARGO_BIN_EXE_aft"));
        command
            .args(args)
            .current_dir(temp.path())
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("XDG_DATA_HOME", temp.path().join("data"))
            .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
            .env("SUBC_CONNECTION_FILE", "")
            .env("AFT_STORAGE_DIR", &storage);
        #[cfg(unix)]
        command.env("PATH", &process_path);
        BoundedChild::spawn(&mut command, &label).output(CHILD_TIMEOUT)
    };
    eprintln!("cache-root doctor: acquiring first child storage lease");
    let daemon = StorageLockChild::spawn(&storage, &home);
    let busy = run(&["fix-config"]);
    assert!(!busy.status.success(), "{busy:?}");
    assert!(
        String::from_utf8_lossy(&busy.stdout)
            .contains("stop the running daemon using storage root"),
        "{busy:?}"
    );
    assert!(
        external[0].1.is_file(),
        "a held storage lease must stop doctor before deletion"
    );
    eprintln!("cache-root doctor: releasing first child storage lease");
    daemon.stop();
    let doctor = run(&["fix-config"]);
    assert!(
        doctor.status.success(),
        "{} {}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    assert!(
        !external[0].1.exists(),
        "doctor should reclaim the marked inactive root"
    );
    assert!(external[1].1.is_file());
    assert!(
        legacy.is_file(),
        "doctor must leave unmarked storage-root caches alone"
    );
    let refused = run(&["clean-old-caches"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stdout).contains("old_cache_confirmation_required"));
    assert!(legacy.is_file());
    eprintln!("cache-root doctor: acquiring second child storage lease");
    let daemon = StorageLockChild::spawn(&storage, &home);
    let busy = run(&["clean-old-caches", "--yes"]);
    assert!(!busy.status.success(), "{busy:?}");
    assert!(
        String::from_utf8_lossy(&busy.stdout)
            .contains("stop the running daemon using storage root"),
        "{busy:?}"
    );
    assert!(legacy.is_file());
    eprintln!("cache-root doctor: releasing second child storage lease");
    daemon.stop();
    let cleaned = run(&["clean-old-caches", "--yes"]);
    assert!(
        cleaned.status.success(),
        "{} {}",
        String::from_utf8_lossy(&cleaned.stdout),
        String::from_utf8_lossy(&cleaned.stderr)
    );
    assert!(String::from_utf8_lossy(&cleaned.stdout).contains("Total:"));
    assert!(!legacy.exists());
    assert!(
        legacy_harness.is_file(),
        "the seven-domain legacy exception must not reach harness folders"
    );
    assert!(external[1].1.is_file());
    for path in protected {
        assert!(path.is_file(), "durable state removed: {}", path.display());
    }
}
