use super::*;
use crate::config::Config;

#[test]
fn missing_reader_floor_registration_refuses_storage_preparation() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    let mut config = crate::config::Config {
        storage_dir: Some(storage.clone()),
        ..Default::default()
    };
    let hooks = std::sync::OnceLock::new();
    let warning = super::prepare_cache_root_with_floor_hooks(&mut config, &hooks).unwrap();
    assert!(warning
        .message
        .contains("reader_floor::register_cache_hooks"));
    assert!(config
        .cache_unavailable
        .as_deref()
        .unwrap()
        .contains("reader_floor::register_cache_hooks"));
    assert!(config.cache_lease.is_none());
    assert!(
        !storage.exists(),
        "missing floor checks must not prepare storage"
    );
}

pub fn unenrolled_cache_config(temp: &Path) -> Config {
    let parent = temp.join("cache-volume");
    fs::create_dir(&parent).unwrap();
    Config {
        storage_dir: Some(temp.join("storage")),
        cache_dir: Some(parent),
        ..Default::default()
    }
}

#[cfg(unix)]
pub fn symlinked_storage_config(temp: &Path) -> Config {
    let mut config = unenrolled_cache_config(temp);
    let resolved = temp.join("real-storage");
    fs::create_dir(&resolved).unwrap();
    std::os::unix::fs::symlink(&resolved, config.storage_dir.as_ref().unwrap()).unwrap();
    config.cache_dir = Some(fs::canonicalize(config.cache_dir.unwrap()).unwrap());
    config
}

fn deep_tree(root: &Path) -> PathBuf {
    let mut leaf = root.to_path_buf();
    for _ in 0..80 {
        leaf.push("d");
    }
    canary(&leaf, "keep")
}

#[test]
fn cache_location_measurement_refuses_excessive_depth() {
    let temp = tempfile::tempdir().unwrap();
    let canary = deep_tree(&temp.path().join("index"));
    let parent = PinnedDir::open(temp.path()).unwrap();
    let error = parent
        .repair_child_tree_bytes(std::ffi::OsStr::new("index"), &[])
        .expect_err("cache preflight must refuse an incomplete traversal");
    assert!(error.to_string().contains("traversal limit"), "{error}");
    assert!(canary.is_file());
}

#[test]
fn cache_location_durable_scan_refuses_excessive_depth() {
    let temp = tempfile::tempdir().unwrap();
    let canary = deep_tree(&temp.path().join("backups"));
    let error = deletion_protections(&[temp.path().join("backups")])
        .expect_err("an incomplete durable identity census must refuse deletion");
    assert!(error.to_string().contains("traversal limit"), "{error}");
    assert!(canary.is_file());
}

#[test]
fn process_storage_lease_reuses_live_ownership_and_releases_the_last_handle() {
    let temp = tempfile::tempdir().unwrap();
    let first = acquire_daemon_storage_lease(temp.path()).unwrap();
    let second = acquire_daemon_storage_lease(temp.path()).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    drop(first);
    assert_eq!(
        acquire_cache_cleanup_lease(temp.path()).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(second);
    acquire_cache_cleanup_lease(temp.path()).unwrap();
}

/// Hold the exclusive side of a root's use lock, as cache cleanup does.
pub fn hold_use_lock_exclusively(root: &Path) -> File {
    fs::create_dir_all(root).unwrap();
    let holder = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(IN_USE))
        .unwrap();
    holder.lock().unwrap();
    holder
}

#[cfg(unix)]
#[test]
fn cache_location_coordination_create_exhaustion_is_typed_busy() {
    let temp = tempfile::tempdir().unwrap();
    let root = PinnedDir::repair_open_path(temp.path()).unwrap();
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&attempts);
    let error = crate::pinned_dir::with_openat_file_hook(
        Box::new(move |_, name, flags| {
            assert_eq!(name, std::ffi::OsStr::new(IN_USE));
            observed.lock().unwrap().push(flags);
            Some(Err(io::Error::from_raw_os_error(libc::ENOENT)))
        }),
        || {
            root.repair_coordination_file(std::ffi::OsStr::new(IN_USE))
                .unwrap_err()
        },
    );
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert!(error
        .get_ref()
        .unwrap()
        .is::<crate::pinned_dir::CoordinationCreateContended>());
    let contended = error
        .get_ref()
        .unwrap()
        .downcast_ref::<crate::pinned_dir::CoordinationCreateContended>()
        .unwrap();
    assert_eq!(contended.path(), root.path().join(IN_USE));
    assert_eq!(
        error.to_string(),
        format!("{CACHE_DIR_BUSY}: {CACHE_DIR_BUSY_MESSAGE}")
    );
    let warning = unavailable_warning(root.path(), &error.to_string());
    assert_eq!(warning.code, CACHE_DIR_BUSY);
    assert_eq!(warning.message, CACHE_DIR_BUSY_MESSAGE);
    assert!(!warning.message.contains("is unavailable"));
    let attempts = attempts.lock().unwrap();
    assert_eq!(attempts.len(), 6);
    for (attempt, flags) in attempts.iter().enumerate() {
        assert_eq!(flags & libc::O_CREAT != 0, attempt % 2 == 0);
        assert_ne!(flags & libc::O_NOFOLLOW, 0);
    }
    assert!(!temp.path().join(IN_USE).exists());
}

#[cfg(unix)]
#[test]
fn cache_location_coordination_reopen_refuses_a_sibling_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let outside = temp.path().join("outside");
    fs::write(&outside, b"canary").unwrap();
    fs::create_dir(&storage).unwrap();
    let root = PinnedDir::repair_open_path(&storage).unwrap();
    let destination = outside.clone();
    let link = storage.join(IN_USE);
    let mut first = true;
    let error = crate::pinned_dir::with_openat_file_hook(
        Box::new(move |_, _, _| {
            if !first {
                return None;
            }
            first = false;
            std::os::unix::fs::symlink(&destination, &link).unwrap();
            Some(Err(io::Error::from_raw_os_error(libc::ENOENT)))
        }),
        || {
            root.repair_coordination_file(std::ffi::OsStr::new(IN_USE))
                .unwrap_err()
        },
    );
    assert_ne!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(fs::read(&outside).unwrap(), b"canary");
}

#[test]
fn cache_location_process_scan_names_windows_executables_from_win32_process() {
    // `Name` names the executable; a quoted `CommandLine` path with spaces
    // must not be split into `"C:\Program`.
    let me = 4000;
    let listing =
        |rows: &[&str]| format!("{me}\tcargo-test.exe\tcargo test\r\n{}", rows.join("\r\n"));
    for row in [
        "4100\tsubc.exe\t\"C:\\Program Files\\AFT\\subc.exe\" --connection C:\\run\\c.json",
        "4101\tsubc.exe\t\"C:\\AFT\\subc.exe\" --connection C:\\run\\c.json",
        "4102\tSUBC.EXE\tC:\\AFT\\SUBC.EXE",
        "4103\taft.exe\t\"C:\\Program Files\\AFT\\aft.exe\" --subc C:\\run\\c.json",
        "4104\taft.exe\t\"C:\\AFT\\aft.exe\" --subc=C:\\run\\c.json",
        // A subc daemon whose command line Windows did not disclose.
        "4105\tsubc.exe",
    ] {
        assert!(
            legacy_daemon_present(&listing(&[row]), me).unwrap(),
            "daemon not recognised: {row}"
        );
    }
    for row in [
        // An aft process that is not a daemon.
        "4200\taft.exe\t\"C:\\Program Files\\AFT\\aft.exe\" index",
        // `--subc` inside the quoted program path is not an argument.
        "4201\taft.exe\t\"C:\\x --subc\\aft.exe\" index",
        // Unrelated processes, with and without a disclosed command line.
        "4202\tnotepad.exe\t",
        "4203\tSystem",
    ] {
        assert!(
            !legacy_daemon_present(&listing(&[row]), me).unwrap(),
            "not a daemon: {row}"
        );
    }
    // This process is never its own blocker.
    assert!(!legacy_daemon_present(
        &format!("{me}\tsubc.exe\t\"C:\\Program Files\\AFT\\subc.exe\""),
        me
    )
    .unwrap());
}

#[test]
fn cache_location_process_scan_reads_unix_executables_with_spaces_whole() {
    // `ps -o comm=` gives the executable as the rest of its line, so a home
    // or app folder with a space keeps the whole path; splitting the command
    // line on whitespace would read `/Users/Jane` and miss the daemon.
    let me = 4000;
    let executables = format!(
        "{me} /usr/bin/cargo-test\n\
         4100 /Users/Jane Doe/.local/bin/subc\n\
         4101 /Applications/AFT Tools.app/Contents/MacOS/aft\n\
         4102 /Applications/AFT Tools.app/Contents/MacOS/aft\n\
         4103 /usr/sbin/sshd\n"
    );
    let command_lines = format!(
        "{me} /usr/bin/cargo-test --lib\n\
         4100 /Users/Jane Doe/.local/bin/subc --connection /run/c.json\n\
         4101 /Applications/AFT Tools.app/Contents/MacOS/aft --subc /run/c.json\n\
         4102 /Applications/AFT Tools.app/Contents/MacOS/aft index\n\
         4103 sshd: listener\n"
    );
    let snapshot = compose_ps_snapshot(&executables, &command_lines);
    let only = |pids: &[u32]| {
        snapshot
            .lines()
            .filter(|line| {
                listed_process(line)
                    .is_some_and(|process| process.pid == me || pids.contains(&process.pid))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        legacy_daemon_present(&only(&[4100]), me).unwrap(),
        "{snapshot}"
    );
    assert!(
        legacy_daemon_present(&only(&[4101]), me).unwrap(),
        "{snapshot}"
    );
    assert!(
        !legacy_daemon_present(&only(&[4102, 4103]), me).unwrap(),
        "{snapshot}"
    );
}

#[test]
fn cache_location_process_scan_that_cannot_tell_refuses() {
    // An empty or partial scan, or a process whose executable or (for aft)
    // arguments are unknown, is no evidence that no daemon runs: cleanup must
    // hold rather than read it as "no daemon".
    let me = 4000;
    for processes in [
        String::new(),
        "4300\tsvchost.exe\tC:\\Windows\\svchost.exe".to_string(),
        compose_ps_snapshot("4300 /usr/bin/sshd\n", "4300 sshd\n"),
        compose_ps_snapshot("ps: permission denied\n", ""),
        // The executable query named no executable for one process.
        compose_ps_snapshot(&format!("{me} /opt/test-runner\n4300\n"), ""),
        // An aft process that exited before the arguments query.
        compose_ps_snapshot(
            &format!("{me} /opt/test-runner\n4300 /Users/Jane Doe/bin/aft\n"),
            &format!("{me} /opt/test-runner\n"),
        ),
        // An aft process whose command line Windows did not disclose.
        format!("{me}\tcargo-test.exe\tcargo test\n4300\taft.exe"),
    ] {
        let error = legacy_daemon_present(&processes, me)
            .expect_err("an undecidable scan must not read as no daemon");
        assert!(
            error
                .to_string()
                .starts_with("legacy_daemon_upgrade_window: cannot tell"),
            "{processes:?}: {error}"
        );
    }
    assert!(!legacy_daemon_present(
        &compose_ps_snapshot(
            &format!("{me} /opt/test-runner\n4300 /usr/bin/sshd\n"),
            &format!("{me} /opt/test-runner\n"),
        ),
        me
    )
    .unwrap());
}

/// Runs the real process query on this machine: the scan must list this
/// test process by its executable's name.
#[test]
fn cache_location_process_scan_lists_this_process() {
    let processes =
        legacy_process_query("test process scan", Duration::from_secs(60)).expect("process scan");
    let me = std::process::id();
    let listed = processes
        .lines()
        .filter_map(listed_process)
        .find(|process| process.pid == me)
        .map(|process| process.executable.to_string())
        .unwrap_or_else(|| {
            panic!(
                "scan did not list pid {me} among {} lines",
                processes.lines().count()
            )
        });
    let listed = Path::new(&listed)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let exe = std::env::current_exe().unwrap();
    let name = exe
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    // Linux reports at most the first 15 bytes of a process name.
    assert!(
        !listed.is_empty() && name.starts_with(&listed),
        "{listed} is not {name}"
    );
    // A daemon may really be running here; the scan only has to be usable.
    let verdict = legacy_daemon_present(&processes, me);
    assert!(
        verdict.is_ok()
            || verdict
                .as_ref()
                .is_err_and(|error| error.to_string().contains("arguments of")),
        "{verdict:?}"
    );
}

#[cfg(unix)]
#[test]
fn cache_location_process_scan_stalls_have_named_deadlines() {
    for (script, waiting_for) in [
        ("exec sleep 1", "process snapshot stdout EOF"),
        ("exec 1>&-; exec sleep 1", "process snapshot exit"),
        ("sleep 1 & exit 0", "process snapshot stdout EOF"),
    ] {
        let started = std::time::Instant::now();
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", script]);
        let error =
            legacy_process_snapshot(&mut command, "test process scan", Duration::from_millis(50))
                .expect_err("a stalled process scan must refuse cleanup");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("test process scan"), "{error}");
        assert!(error.to_string().contains(waiting_for), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}

#[cfg(unix)]
#[test]
fn cache_location_process_scan_collects_a_completed_snapshot() {
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", "printf '42 /bin/not-a-daemon\\n'"]);
    assert_eq!(
        legacy_process_snapshot(&mut command, "test process scan", Duration::from_secs(3)).unwrap(),
        "42 /bin/not-a-daemon\n"
    );
}
pub fn canary(root: &Path, relative: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "keep me").unwrap();
    path
}

#[test]
fn cache_location_unprepared_config_is_unavailable_even_at_epoch_zero() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("volume");
    fs::create_dir(&parent).unwrap();
    let config = Config {
        cache_dir: Some(parent.clone()),
        storage_dir: Some(temp.path().join("storage")),
        ..Default::default()
    };
    let root = cache_root(&config);
    assert!(cache_unavailable(&config).is_some());
    assert!(check_cache_access(&root.join("index/new")).is_err());
    assert!(!parent.join("aft").exists());
}

#[test]
fn cache_location_refuses_the_volumes_container_itself() {
    let temp = tempfile::tempdir().unwrap();
    let volumes = temp.path().join("Volumes");
    fs::create_dir(&volumes).unwrap();
    let actual = identity(&volumes).unwrap();
    let error =
        verify_mounted_parent_under(&volumes, actual, &volumes, &[actual.device]).unwrap_err();
    assert!(error.to_string().contains("cannot be /Volumes"));
}

pub fn resolve_user_doc(doc: &str) -> crate::config_resolve::ResolveResult {
    crate::config_resolve::resolve_config(&[crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "user file".into(),
        doc: doc.into(),
    }])
}

pub fn user_tier(doc: &str) -> crate::config_resolve::ConfigTier {
    crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "user file".into(),
        doc: doc.into(),
    }
}

#[test]
fn cache_reclaim_takes_storage_lease_before_preparing_cache_root() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let parent = temp.path().join("cache-volume");
    fs::create_dir(&parent).unwrap();
    let _volume = crate::walk_boundary::simulated_volumes::simulate(&parent, u64::MAX - 17);
    let _daemon = acquire_daemon_storage_lease(&storage).unwrap();
    let tiers = [user_tier(
        &serde_json::json!({"storage":{"cache_dir":parent}}).to_string(),
    )];
    let report = reclaim_configured_inactive_caches(&tiers, &storage);
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("storage lease contention")),
        "{report:?}"
    );
    assert!(
        !parent.join("aft").exists(),
        "reclaim prepared a cache namespace before excluding its owner"
    );
}

#[test]
fn cache_dir_misspelled_storage_keys_are_errors() {
    for (doc, key) in [
        (r#"{"storge":{"cache_dir":"/elsewhere"}}"#, "storge"),
        (r#"{"storage":{"cachedir":"/elsewhere"}}"#, "cachedir"),
        (
            r#"{"harnesses":{"pi":{"storge":{"cache_dir":"/elsewhere"}}}}"#,
            "storge",
        ),
        (
            r#"{"harnesses":{"pi":{"storage":{"cachedir":"/elsewhere"}}}}"#,
            "cachedir",
        ),
    ] {
        let result = resolve_user_doc(doc);
        assert!(
            result.errors.iter().any(|error| error.contains(key)),
            "misspelled {key} silently resolved to defaults: {result:?}"
        );
        assert!(result.config.cache_config_unknown.is_some(), "{result:?}");
    }
}
pub fn add_record(storage: &Path, root: &Path) {
    let path = history_path(storage);
    let mut history: History = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    history.roots.push(RecordedRoot {
        path: root.to_path_buf(),
        identity: Some(real_directory(root).unwrap()),
        registration: None,
    });
    fs::write(path, serde_json::to_vec(&history).unwrap()).unwrap();
}

#[test]
fn cache_location_flat_deserialization_cannot_redirect_cache_root() {
    let config: Config = serde_json::from_value(
        serde_json::json!({"cache_dir":"/evil", "cache_unavailable":null, "cache_epoch":999}),
    )
    .unwrap();
    assert!(config.cache_dir.is_none());
    assert_eq!(config.cache_epoch, 0);
    let result = crate::config_resolve::resolve_config(&[crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "file".into(),
        doc: r#"{"cache_dir":"/evil"}"#.into(),
    }]);
    assert!(result.config.cache_dir.is_none());
}
