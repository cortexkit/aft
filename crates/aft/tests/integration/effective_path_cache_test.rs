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

fn run_ping(storage: &Path, home: &Path, candidates: &OsStr, marker: &Path) -> (Duration, Value) {
    let started = Instant::now();
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
    let output = child.wait_with_output().expect("wait for aft binary");
    assert!(output.status.success(), "aft failed: {output:?}");
    let response = String::from_utf8(output.stdout).unwrap();
    let response = response.lines().last().expect("ping response");
    (started.elapsed(), serde_json::from_str(response).unwrap())
}

fn wait_for_marker(marker: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(marker.exists(), "detached probe did not execute its shell");
}

#[test]
fn valid_cache_skips_sleeping_shell_and_returns_ping_quickly() {
    let fixture = tempfile::tempdir().unwrap();
    let storage = fixture.path().join("storage");
    let home = fixture.path().join("home");
    let shell = fixture.path().join("bash");
    let marker = fixture.path().join("shell-ran");
    fs::create_dir_all(&home).unwrap();
    write_executable(
        &shell,
        "#!/bin/sh\nprintf shell-ran > \"$AFT_TEST_PATH_MARKER\"\nsleep 10\n",
    );
    write_cache(
        &storage,
        &shell,
        &home,
        Some("/cached/login/bin:/usr/bin:/bin"),
    );

    let (elapsed, response) = run_ping(&storage, &home, shell.as_os_str(), &marker);

    assert!(
        elapsed < Duration::from_millis(500),
        "cache hit took {elapsed:?}"
    );
    assert_eq!(response["id"], "1");
    assert!(
        !marker.exists(),
        "the cache-hit request executed the sleeping login shell"
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
            "#!/bin/sh\nprintf probe-ran > \"$AFT_TEST_PATH_MARKER\"\neval \"$2\"\n",
        );
        write_cache(
            &storage,
            &shell,
            &home,
            Some("/cached/login/bin:/usr/bin:/bin"),
        );
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

        let (_, response) = run_ping(&storage, &home, shell.as_os_str(), &marker);

        assert_eq!(response["id"], "1");
        assert!(marker.exists(), "rc-file change did not run the probe");
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

    let (first_elapsed, first_response) = run_ping(&storage, &home, shell.as_os_str(), &marker);
    assert!(
        first_elapsed >= Duration::from_secs(2) && first_elapsed < Duration::from_millis(4500),
        "first timeout path took {first_elapsed:?}"
    );
    assert_eq!(first_response["id"], "1");
    let cache: Value = serde_json::from_slice(
        &fs::read(storage.join("aft/effective-path.json")).expect("timeout cache"),
    )
    .unwrap();
    assert!(cache["path"].is_null(), "timeout must cache null PATH");
    assert!(!cache["inputs"].as_array().unwrap().is_empty());
    assert_eq!(fs::read_to_string(&marker).unwrap(), "x");

    let (second_elapsed, second_response) = run_ping(&storage, &home, shell.as_os_str(), &marker);
    assert!(
        second_elapsed < Duration::from_millis(500),
        "cached timeout path took {second_elapsed:?}"
    );
    assert_eq!(second_response["id"], "1");
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        "x",
        "cached timeout started another login-shell probe"
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

    let (first_elapsed, first_response) = run_ping(&storage, &home, &candidates, &marker);
    assert!(
        first_elapsed >= Duration::from_secs(2) && first_elapsed < Duration::from_millis(4500),
        "first fallback path took {first_elapsed:?}"
    );
    assert_eq!(first_response["id"], "1");
    assert_eq!(fs::read_to_string(&marker).unwrap(), "x");

    let (second_elapsed, second_response) = run_ping(&storage, &home, &candidates, &marker);
    assert!(
        second_elapsed < Duration::from_millis(500),
        "cached fallback path took {second_elapsed:?}"
    );
    assert_eq!(second_response["id"], "1");
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        "x",
        "cached fallback result retried the requested hanging shell"
    );
}

#[test]
fn inline_probe_total_budget_caps_two_hanging_candidates() {
    let fixture = tempfile::tempdir().unwrap();
    let storage = fixture.path().join("storage");
    let home = fixture.path().join("home");
    let first = fixture.path().join("first-bash");
    let second = fixture.path().join("second-bash");
    fs::create_dir_all(&home).unwrap();
    write_executable(&first, "#!/bin/sh\nsleep 10\n");
    write_executable(&second, "#!/bin/sh\nsleep 10\n");
    let candidates = std::env::join_paths([&first, &second]).unwrap();

    let (elapsed, response) = run_ping(
        &storage,
        &home,
        &candidates,
        &fixture.path().join("probe-ran"),
    );

    assert!(
        elapsed < Duration::from_millis(4500),
        "two hanging candidates exceeded total budget: {elapsed:?}"
    );
    assert_eq!(response["id"], "1");
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
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());

    wait_for_marker(&marker);
}
