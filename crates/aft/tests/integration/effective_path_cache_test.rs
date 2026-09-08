#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use filetime::FileTime;
use serde_json::{json, Value};

const LIVENESS_CEILING: Duration = Duration::from_secs(30);

fn aft_binary() -> PathBuf {
    std::env::var_os("AFT_TEST_AFT_BINARY")
        .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_aft"))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_aft"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aft")))
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn input_stamp(path: PathBuf) -> Value {
    match fs::metadata(&path) {
        Ok(metadata) => json!({
            "file": path,
            "mtime_ns": i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()),
            "size": metadata.len(),
        }),
        Err(_) => json!({ "file": path, "mtime_ns": null, "size": null }),
    }
}

fn write_cache(storage: &Path, shell: &Path, home: &Path, path: Option<&str>) -> PathBuf {
    let inputs = [
        PathBuf::from("/etc/profile"),
        home.join(".bash_profile"),
        home.join(".bash_login"),
        home.join(".profile"),
        home.join(".bashrc"),
    ]
    .into_iter()
    .map(input_stamp)
    .collect::<Vec<_>>();
    let cache_path = storage.join("aft/effective-path.json");
    fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
    fs::write(
        &cache_path,
        serde_json::to_vec(&json!({
            "schema": 1,
            "shell": shell,
            "path": path,
            "probed_at_unix": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            "inputs": inputs,
        }))
        .unwrap(),
    )
    .unwrap();
    cache_path
}

fn read_counter(marker: &Path) -> usize {
    fs::read_to_string(marker)
        .map(|s| s.matches('x').count())
        .unwrap_or(0)
}

fn wait_with_liveness_ceiling(
    mut child: std::process::Child,
    timeout: Duration,
) -> std::process::Output {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child.wait_with_output().expect("read output after exit");
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("process exceeded {timeout:?} liveness ceiling");
            }
            Ok(None) => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                let _ = child.kill();
                panic!("failed to wait for child: {error}");
            }
        }
    }
}

fn run_ping(storage: &Path, home: &Path, candidates: &OsStr, marker: &Path) -> Value {
    let mut child = Command::new(aft_binary())
        .env("AFT_CACHE_DIR", storage)
        .env("AFT_TEST_RAW_PATH", "0")
        .env("AFT_TEST_LOGIN_SHELL_CANDIDATES", candidates)
        .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
        .env("AFT_TEST_PATH_MARKER", marker)
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn aft binary");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"id\":\"1\",\"command\":\"ping\"}\n")
        .unwrap();
    let output = wait_with_liveness_ceiling(child, LIVENESS_CEILING);
    assert!(output.status.success(), "aft failed: {output:?}");
    let response = String::from_utf8(output.stdout).unwrap();
    let response = response.lines().last().expect("ping response");
    serde_json::from_str(response).unwrap()
}

fn run_bash_get_path(
    storage: &Path,
    home: &Path,
    candidates: &OsStr,
    marker: &Path,
    output_path: &Path,
) -> Value {
    let mut child = Command::new(aft_binary())
        .env("AFT_CACHE_DIR", storage)
        .env("AFT_TEST_RAW_PATH", "0")
        .env("AFT_TEST_LOGIN_SHELL_CANDIDATES", candidates)
        .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
        .env("AFT_TEST_PATH_MARKER", marker)
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn aft binary");

    let cmd = serde_json::json!({
        "id": "1",
        "command": "bash",
        "params": {
            "command": format!("printf %s \"$PATH\" > \"{}\"", output_path.display())
        }
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{cmd}\n").as_bytes())
        .unwrap();

    let output = wait_with_liveness_ceiling(child, LIVENESS_CEILING);
    assert!(output.status.success(), "aft failed: {output:?}");

    let deadline = Instant::now() + LIVENESS_CEILING;
    while !output_path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        output_path.exists(),
        "bash did not write served path within liveness ceiling"
    );

    let response = String::from_utf8(output.stdout).unwrap();
    let response = response.lines().last().expect("bash response");
    serde_json::from_str(response).unwrap()
}

fn wait_for_marker(marker: &Path) {
    let deadline = Instant::now() + LIVENESS_CEILING;
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        marker.exists(),
        "detached probe did not execute its shell within liveness ceiling"
    );
}

#[test]
fn valid_cache_skips_sleeping_shell_and_returns_ping_quickly() {
    let fixture = tempfile::tempdir().unwrap();
    let storage = fixture.path().join("storage");
    let home = fixture.path().join("home");
    let shell = fixture.path().join("bash");
    let marker = fixture.path().join("shell-ran");
    let served_path_file = fixture.path().join("served_path.txt");
    fs::create_dir_all(&home).unwrap();
    write_executable(
        &shell,
        "#!/bin/sh\nprintf x >> \"$AFT_TEST_PATH_MARKER\"\nsleep 10\n",
    );
    write_cache(
        &storage,
        &shell,
        &home,
        Some("/cached/login/bin:/usr/bin:/bin"),
    );

    let response = run_bash_get_path(
        &storage,
        &home,
        shell.as_os_str(),
        &marker,
        &served_path_file,
    );

    assert_eq!(response["id"], "1");
    assert_eq!(
        read_counter(&marker),
        0,
        "the cache-hit request executed the sleeping login shell"
    );
    let served_path = fs::read_to_string(&served_path_file).expect("served path file written");
    assert!(
        std::env::split_paths(&served_path).any(|p| p == Path::new("/cached/login/bin")),
        "served PATH {served_path:?} does not include cached entry /cached/login/bin"
    );
}

#[test]
fn changing_or_creating_a_recorded_rc_file_invalidates_the_cache() {
    for initially_exists in [true, false] {
        let fixture = tempfile::tempdir().unwrap();
        let storage = fixture.path().join("storage");
        let home = fixture.path().join("home");
        let shell = fixture.path().join("bash");
        let marker = fixture.path().join("probe-ran");
        let bashrc = home.join(".bashrc");
        fs::create_dir_all(&home).unwrap();
        if initially_exists {
            fs::write(&bashrc, "export PATH=/before\n").unwrap();
        }
        write_executable(
            &shell,
            "#!/bin/sh\nprintf x >> \"$AFT_TEST_PATH_MARKER\"\neval \"$2\"\n",
        );
        let cache_path = write_cache(
            &storage,
            &shell,
            &home,
            Some("/cached/login/bin:/usr/bin:/bin"),
        );
        let initial_cache: Value = serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
        let initial_inputs = initial_cache["inputs"].clone();

        if initially_exists {
            let future = FileTime::from_unix_time(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
                    + 2,
                0,
            );
            filetime::set_file_mtime(&bashrc, future).unwrap();
        } else {
            fs::write(&bashrc, "export PATH=/created\n").unwrap();
        }

        let response = run_ping(&storage, &home, shell.as_os_str(), &marker);

        assert_eq!(response["id"], "1");
        assert_eq!(
            read_counter(&marker),
            1,
            "rc-file change did not run the probe exactly once"
        );
        let updated_cache: Value = serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
        assert_ne!(
            initial_inputs, updated_cache["inputs"],
            "cache file inputs must change after rc-file modification"
        );
    }
}

#[test]
fn timed_out_probe_is_cached_and_second_binary_start_is_fast() {
    let fixture = tempfile::tempdir().unwrap();
    let storage = fixture.path().join("storage");
    let home = fixture.path().join("home");
    let shell = fixture.path().join("bash");
    let marker = fixture.path().join("probe-count");
    fs::create_dir_all(&home).unwrap();
    write_executable(
        &shell,
        "#!/bin/sh\nprintf x >> \"$AFT_TEST_PATH_MARKER\"\nsleep 10\n",
    );

    let first_response = run_ping(&storage, &home, shell.as_os_str(), &marker);
    assert_eq!(first_response["id"], "1");
    let cache: Value = serde_json::from_slice(
        &fs::read(storage.join("aft/effective-path.json")).expect("timeout cache"),
    )
    .unwrap();
    assert!(cache["path"].is_null(), "timeout must cache null PATH");
    assert!(!cache["inputs"].as_array().unwrap().is_empty());
    let count_after_first = read_counter(&marker);
    assert_eq!(
        count_after_first, 1,
        "first run should have invoked the shell once"
    );

    let second_response = run_ping(&storage, &home, shell.as_os_str(), &marker);
    assert_eq!(second_response["id"], "1");
    let count_after_second = read_counter(&marker);
    let delta = count_after_second - count_after_first;
    assert_eq!(
        delta, 0,
        "cached timeout started another login-shell probe (counter delta {delta})"
    );
}

#[test]
fn fallback_result_is_cached_for_the_requested_hanging_shell() {
    let fixture = tempfile::tempdir().unwrap();
    let storage = fixture.path().join("storage");
    let home = fixture.path().join("home");
    let hanging_shell = fixture.path().join("hanging-bash");
    let fallback_shell = fixture.path().join("fallback-bash");
    let marker = fixture.path().join("hanging-count");
    fs::create_dir_all(&home).unwrap();
    write_executable(
        &hanging_shell,
        "#!/bin/sh\nprintf x >> \"$AFT_TEST_PATH_MARKER\"\nsleep 10\n",
    );
    write_executable(&fallback_shell, "#!/bin/sh\neval \"$2\"\n");
    let candidates = std::env::join_paths([&hanging_shell, &fallback_shell]).unwrap();

    let first_response = run_ping(&storage, &home, &candidates, &marker);
    assert_eq!(first_response["id"], "1");
    let count_after_first = read_counter(&marker);
    assert_eq!(
        count_after_first, 1,
        "first run should have attempted the hanging shell once"
    );
    let cache: Value = serde_json::from_slice(
        &fs::read(storage.join("aft/effective-path.json")).expect("fallback cache"),
    )
    .unwrap();
    assert_eq!(
        cache["shell"],
        hanging_shell.to_string_lossy().as_ref(),
        "fallback must cache against the requested shell"
    );
    assert!(
        cache["path"].is_string(),
        "fallback probe should succeed and cache non-null path"
    );

    let second_response = run_ping(&storage, &home, &candidates, &marker);
    assert_eq!(second_response["id"], "1");
    let count_after_second = read_counter(&marker);
    let delta = count_after_second - count_after_first;
    assert_eq!(
        delta, 0,
        "cached fallback result retried the requested hanging shell (counter delta {delta})"
    );
}

#[test]
fn inline_probe_total_budget_caps_two_hanging_candidates() {
    let fixture = tempfile::tempdir().unwrap();
    let storage = fixture.path().join("storage");
    let home = fixture.path().join("home");
    let first = fixture.path().join("first-bash");
    let second = fixture.path().join("second-bash");
    let first_start = fixture.path().join("first-start");
    let second_start = fixture.path().join("second-start");
    fs::create_dir_all(&home).unwrap();
    write_executable(
        &first,
        &format!(
            "#!/bin/sh\ndate +%s > \"{}\"\nsleep 10\n",
            first_start.display()
        ),
    );
    write_executable(
        &second,
        &format!(
            "#!/bin/sh\ndate +%s > \"{}\"\nsleep 10\n",
            second_start.display()
        ),
    );
    let candidates = std::env::join_paths([&first, &second]).unwrap();

    let response = run_ping(
        &storage,
        &home,
        &candidates,
        &fixture.path().join("probe-ran"),
    );
    assert_eq!(response["id"], "1");

    assert!(
        first_start.exists(),
        "first hanging candidate must have started"
    );
    if second_start.exists() {
        let first_ts: u64 = fs::read_to_string(&first_start)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let second_ts: u64 = fs::read_to_string(&second_start)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            second_ts >= first_ts,
            "second candidate started before first candidate: {second_ts} < {first_ts}"
        );
    }
    let cache: Value = serde_json::from_slice(
        &fs::read(storage.join("aft/effective-path.json")).expect("cache file after probe"),
    )
    .unwrap();
    assert!(
        cache["path"].is_null(),
        "total budget cap on two hanging candidates must cache null PATH"
    );
    assert_eq!(cache["shell"], first.to_string_lossy().as_ref());
}

#[test]
fn cache_hit_starts_a_detached_refresh_helper_in_production() {
    let fixture = tempfile::tempdir().unwrap();
    let storage = fixture.path().join("storage");
    let home = fixture.path().join("home");
    let shell = fixture.path().join("bash");
    let marker = fixture.path().join("helper-ran");
    fs::create_dir_all(&home).unwrap();
    write_executable(
        &shell,
        "#!/bin/sh\nprintf helper-ran > \"$AFT_TEST_PATH_MARKER\"\neval \"$2\"\n",
    );
    write_cache(
        &storage,
        &shell,
        &home,
        Some("/cached/login/bin:/usr/bin:/bin"),
    );

    let mut child = Command::new(aft_binary())
        .env("AFT_CACHE_DIR", &storage)
        .env("AFT_TEST_RAW_PATH", "0")
        .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
        .env("AFT_TEST_PATH_MARKER", &marker)
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .env("SHELL", &shell)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn aft binary");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"id\":\"1\",\"command\":\"ping\"}\n")
        .unwrap();
    let output = wait_with_liveness_ceiling(child, LIVENESS_CEILING);
    assert!(output.status.success());

    wait_for_marker(&marker);
}
