//! A bridge spawned while the plugin is still downloading ONNX Runtime must end
//! up with a working semantic index without a restart.
//!
//! The race, as seen on a clean install: the first tool call spawns `aft`
//! before the plugin's ONNX Runtime download finishes, so the process starts
//! with no `ORT_DYLIB_PATH`, fails its semantic build, and never recovers.
//! These tests reproduce it deterministically with the real artefacts the
//! downloader produces: its install lock file (the pid of a live process on the
//! first line) and, once the "download" finishes, the runtime library at
//! `<storage>/onnxruntime/<version>/`, published before the lock is released,
//! in the same order the installer uses. Loading goes through the real `dlopen`
//! and the real `ort` crate.
#![cfg(unix)]

#[path = "helpers/mod.rs"]
mod test_helpers;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use test_helpers::AftProcess;

#[cfg(target_os = "macos")]
const LIB_NAME: &str = "libonnxruntime.dylib";
#[cfg(not(target_os = "macos"))]
const LIB_NAME: &str = "libonnxruntime.so";

/// A runtime already on the loader's default search path hides the race: the
/// bare-name `dlopen` the daemon falls back to would succeed.
fn system_runtime_loadable() -> bool {
    let name = std::ffi::CString::new(LIB_NAME).unwrap();
    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW) };
    if handle.is_null() {
        return false;
    }
    unsafe { libc::dlclose(handle) };
    true
}

fn skip_reason() -> Option<String> {
    if system_runtime_loadable() {
        return Some(format!("{LIB_NAME} loads from the system search path"));
    }
    None
}

/// The daemon under test must start with no runtime path, as it does when the
/// bridge spawns it before the download finishes, whatever the test process
/// itself inherited.
const REMOVED_ENV: &[&str] = &["ORT_DYLIB_PATH"];

/// Directory holding a real ONNX Runtime to "download": `AFT_TEST_ORT_LIBRARY_DIR`,
/// else the newest runtime AFT already manages for this user.
fn real_runtime_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AFT_TEST_ORT_LIBRARY_DIR") {
        let dir = PathBuf::from(dir);
        return dir.join(LIB_NAME).is_file().then_some(dir);
    }
    let base = dirs_home()?.join(".local/share/cortexkit/aft/onnxruntime");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(base)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join(LIB_NAME).is_file())
        .collect();
    versions.sort();
    versions.pop()
}

/// A model cache that already holds MiniLM, so the build does not download it:
/// `AFT_TEST_FASTEMBED_CACHE`, else the cache AFT keeps for this user.
fn real_model_cache() -> Option<PathBuf> {
    let dir = std::env::var_os("AFT_TEST_FASTEMBED_CACHE")
        .map(PathBuf::from)
        .or_else(|| Some(dirs_home()?.join(".local/share/cortexkit/aft/semantic/models")))?;
    dir.join("models--Qdrant--all-MiniLM-L6-v2-onnx")
        .is_dir()
        .then_some(dir)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn start_download(storage: &Path) -> PathBuf {
    let onnx_dir = storage.join("onnxruntime");
    std::fs::create_dir_all(&onnx_dir).expect("onnxruntime dir");
    let lock = onnx_dir.join(".aft-onnx-installing");
    // Same content the installer writes: the owner's pid, then a timestamp.
    // The owner here is this test process, which is alive for the whole test.
    std::fs::write(
        &lock,
        format!("{}\n2026-01-01T00:00:00.000Z\n", std::process::id()),
    )
    .expect("write install lock");
    lock
}

fn finish_download(storage: &Path, lock: &Path, runtime: &Path) {
    let target = storage.join("onnxruntime").join("1.24.4");
    std::fs::create_dir_all(&target).expect("version dir");
    for entry in std::fs::read_dir(runtime)
        .expect("read runtime dir")
        .flatten()
    {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("libonnxruntime") {
            // `fs::copy` follows symlinks, so every name ends up a real file.
            std::fs::copy(entry.path(), target.join(&name)).expect("copy runtime library");
        }
    }
    std::fs::remove_file(lock).expect("release install lock");
}

fn configure(aft: &mut AftProcess, project: &Path, storage: &Path) {
    let response = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.display().to_string(),
            "storage_dir": storage.display().to_string(),
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response}");
}

fn semantic_status(aft: &mut AftProcess) -> Value {
    let status = aft.send(&json!({ "id": "status", "command": "status" }).to_string());
    status["semantic_index"].clone()
}

fn wait_for_semantic(
    aft: &mut AftProcess,
    timeout: Duration,
    what: &str,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let semantic = semantic_status(aft);
        if done(&semantic) {
            return semantic;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; last semantic status: {semantic}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn project() -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("project dir");
    std::fs::write(
        project.path().join("app.ts"),
        "export function greetUser(name: string) {\n  return `hello ${name}`;\n}\n",
    )
    .expect("write source");
    project
}

fn is_waiting(semantic: &Value) -> bool {
    // The status command calls a build in progress "loading".
    matches!(semantic["status"].as_str(), Some("loading" | "building"))
        && semantic["stage"] == "waiting_for_onnx_runtime_download"
}

/// While the download runs, the index waits (and says so) instead of failing
/// with "ONNX Runtime not found"; once the download ends without publishing a
/// runtime, it fails with the doctor hint.
#[test]
fn semantic_build_waits_for_a_running_download_then_fails_when_none_arrives() {
    if let Some(reason) = skip_reason() {
        eprintln!("skipping: {reason}");
        return;
    }
    let project = project();
    let storage = tempfile::tempdir().expect("storage dir");
    let lock = start_download(storage.path());
    let mut aft = AftProcess::spawn_with_semantic_env_without(&[], REMOVED_ENV);
    configure(&mut aft, project.path(), storage.path());

    let waiting = wait_for_semantic(
        &mut aft,
        Duration::from_secs(30),
        "the waiting-for-download stage",
        |semantic| is_waiting(semantic) || semantic["status"] == "failed",
    );
    assert!(is_waiting(&waiting), "expected to wait, got {waiting}");

    let search = aft.send(
        &json!({
            "id": "search",
            "command": "semantic_search",
            "query": "where do we greet the user by name",
        })
        .to_string(),
    );
    let text = search["text"].as_str().expect("search text");
    assert!(
        text.starts_with("Semantic search is waiting for the ONNX Runtime download to finish"),
        "{text}"
    );

    std::fs::remove_file(&lock).expect("release install lock");
    let failed = wait_for_semantic(
        &mut aft,
        Duration::from_secs(30),
        "the failure after the download ended",
        |semantic| semantic["status"] == "failed",
    );
    assert!(
        failed["error"]
            .as_str()
            .is_some_and(|error| error.starts_with("ONNX Runtime not found.")
                && error.contains("doctor --fix")),
        "{failed}"
    );
    assert!(aft.shutdown().success());
}

/// The drill's race: the daemon starts during the download, and the index
/// reaches ready in the same process once the runtime is published.
#[test]
fn semantic_index_becomes_ready_when_the_download_finishes_after_spawn() {
    if let Some(reason) = skip_reason() {
        eprintln!("skipping: {reason}");
        return;
    }
    let (Some(runtime), Some(models)) = (real_runtime_dir(), real_model_cache()) else {
        eprintln!(
            "skipping: needs a real ONNX Runtime (AFT_TEST_ORT_LIBRARY_DIR) and a MiniLM model cache (AFT_TEST_FASTEMBED_CACHE)"
        );
        return;
    };
    let project = project();
    let storage = tempfile::tempdir().expect("storage dir");
    let lock = start_download(storage.path());
    let mut aft = AftProcess::spawn_with_semantic_env_without(
        &[("FASTEMBED_CACHE_DIR", OsStr::new(models.as_os_str()))],
        REMOVED_ENV,
    );
    configure(&mut aft, project.path(), storage.path());

    let waiting = wait_for_semantic(
        &mut aft,
        Duration::from_secs(30),
        "the waiting-for-download stage",
        |semantic| is_waiting(semantic) || semantic["status"] == "failed",
    );
    assert!(is_waiting(&waiting), "expected to wait, got {waiting}");

    finish_download(storage.path(), &lock, &runtime);
    let ready = wait_for_semantic(
        &mut aft,
        Duration::from_secs(120),
        "a ready semantic index",
        |semantic| semantic["status"] == "ready" || semantic["status"] == "failed",
    );
    assert_eq!(ready["status"], "ready", "{ready}");
    assert!(aft.shutdown().success());
}
