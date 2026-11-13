use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const HASH: &str = "56b2a562244e6f35f7b320bce59099126185389df1277a93e450ba4156c0e039";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/grammars")
        .join(format!("{HASH}.wasm"))
}

fn command(root: &Path, download: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aft"));
    command
        .arg("grammar-smoke")
        .arg("--storage-dir")
        .arg(root.join("storage"));
    if download {
        command.arg("--download");
    }
    for variable in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "LOCALAPPDATA",
        "APPDATA",
    ] {
        let folder = root.join(variable);
        std::fs::create_dir_all(&folder).unwrap();
        command.env(variable, folder);
    }
    command
        .env_remove("AFT_STORAGE_DIR")
        .env_remove("AFT_CACHE_DIR")
        .env_remove("AFT_TEST_GRAMMAR_MANIFEST")
        .env_remove("AFT_TEST_GRAMMAR_STORAGE")
        .env_remove("AFT_TEST_GRAMMAR_FAULT")
        .env_remove("AFT_TEST_GRAMMAR_QUERIES")
        .env_remove("AFT_TEST_GRAMMAR_CLOCK_FILE")
        .env_remove("AFT_TEST_GRAMMAR_EXTRACTOR_VERSION")
        .env_remove("AFT_TEST_GRAMMAR_TRANSPORT_FILE");
    command
}

#[test]
fn grammar_loader_retained_file_parsers_compile_once_and_never_download() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("storage/grammars");
    std::fs::create_dir_all(&directory).unwrap();
    let stored = directory.join(format!("{HASH}.wasm"));
    std::fs::copy(fixture(), &stored).unwrap();
    let source = root.path().join("source.luau");
    std::fs::write(&source, "local function f() return 1 end").unwrap();
    let first = result(
        command(root.path(), false)
            .arg("--file")
            .arg(&source)
            .arg("--parallel")
            .output()
            .unwrap(),
    );
    assert_eq!(first["compile_count"], 1);
    let second = result(
        command(root.path(), false)
            .arg("--file")
            .arg(&source)
            .output()
            .unwrap(),
    );
    assert_eq!(second["compile_count"], 1);
    assert_eq!(first["tree"], second["tree"]);
    std::fs::remove_file(stored).unwrap();
    let missing = command(root.path(), false)
        .arg("--file")
        .arg(&source)
        .env("AFT_TEST_GRAMMAR_TRANSPORT_FILE", fixture())
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("\"state\":\"not_downloaded\""));
    assert!(!directory.join(format!("{HASH}.wasm")).exists());
}

fn result(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn grammar_loader_restart_compiles_stored_bytes_and_missing_next_process_downloads() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("storage/grammars");
    std::fs::create_dir_all(&directory).unwrap();
    let stored = directory.join(format!("{HASH}.wasm"));
    std::fs::copy(fixture(), &stored).unwrap();
    let first = result(command(root.path(), false).output().unwrap());
    let second = result(command(root.path(), false).output().unwrap());
    assert_eq!(first["compile_count"], 1);
    assert_eq!(second["compile_count"], 1);
    assert_eq!(first["hash"], HASH);
    assert_eq!(first["tree"], second["tree"]);
    std::fs::remove_file(stored).unwrap();
    let output = command(root.path(), false).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not_downloaded"));
    let third = result(
        command(root.path(), true)
            .env("AFT_TEST_GRAMMAR_TRANSPORT_FILE", fixture())
            .output()
            .unwrap(),
    );
    assert_eq!(third["compile_count"], 1);
    assert_eq!(third["tree"], first["tree"]);
    let names: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from(format!("{HASH}.wasm"))]);
}

#[test]
fn grammar_loader_two_processes_publish_without_storage_retry() {
    let root = tempfile::tempdir().unwrap();
    let a = command(root.path(), true)
        .env("AFT_TEST_GRAMMAR_TRANSPORT_FILE", fixture())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let b = command(root.path(), true)
        .env("AFT_TEST_GRAMMAR_TRANSPORT_FILE", fixture())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let b = result(b.wait_with_output().unwrap());
    let a = result(a.wait_with_output().unwrap());
    assert_eq!(a["tree"], b["tree"]);
    assert_eq!(a["compile_count"], 1);
    assert_eq!(b["compile_count"], 1);
    let names: Vec<_> = std::fs::read_dir(root.path().join("storage/grammars"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from(format!("{HASH}.wasm"))]);
}
