use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use aft::lsp::client::LspEvent;
use aft::lsp::document::DocumentStore;
use aft::lsp::manager::LspManager;
use aft::lsp::registry::ServerKind;
use tempfile::tempdir;

fn fake_server_path() -> PathBuf {
    option_env!("CARGO_BIN_EXE_fake-lsp-server")
        .or(option_env!("CARGO_BIN_EXE_fake_lsp_server"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake-lsp-server").map(PathBuf::from))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake_lsp_server").map(PathBuf::from))
        .or_else(|| {
            let mut path = std::env::current_exe().ok()?;
            path.pop();
            path.pop();
            path.push("fake-lsp-server");
            Some(path)
        })
        .filter(|path| path.exists())
        .expect("fake-lsp-server binary path not set")
}

fn rust_fixture_file() -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempdir().expect("tempdir");
    let root = temp_dir.path().join("workspace");
    let src_dir = root.join("src");
    let main_rs = src_dir.join("main.rs");

    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("write Cargo.toml");
    fs::write(&main_rs, "fn main() {}\n").expect("write fixture source");

    (temp_dir, main_rs)
}

fn collect_event<F>(manager: &mut LspManager, predicate: F) -> Option<LspEvent>
where
    F: Fn(&LspEvent) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        for event in manager.drain_events() {
            if predicate(&event) {
                return Some(event);
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    None
}

fn custom_event(manager: &mut LspManager, method: &str) -> serde_json::Value {
    let maybe_event = collect_event(manager, |event| {
        matches!(
            event,
            LspEvent::Notification {
                method: event_method,
                ..
            } if event_method == method
        )
    });

    match maybe_event {
        Some(LspEvent::Notification {
            method: event_method,
            params,
            ..
        }) if event_method == method => params.expect("custom notification params"),
        Some(other) => panic!("unexpected event: {other:?}"),
        None => panic!("timed out waiting for {method}"),
    }
}

fn file_uri(path: &Path) -> String {
    let canonical = fs::canonicalize(path).expect("canonical path");
    let url = url::Url::from_file_path(&canonical).expect("file url");
    url.to_string()
}

#[test]
fn test_notify_file_changed_sends_did_open() {
    let (_temp_dir, main_rs) = rust_fixture_file();
    let expected_uri = file_uri(&main_rs);
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::Rust, fake_server_path());

    manager
        .notify_file_changed_default(&main_rs, "fn main() { println!(\"hi\"); }\n")
        .expect("notify file changed");

    let opened = custom_event(&mut manager, "custom/documentOpened");
    assert_eq!(opened["uri"], expected_uri);
    assert_eq!(opened["version"], 0);
}

#[test]
fn test_notify_file_changed_twice_sends_did_change() {
    let (_temp_dir, main_rs) = rust_fixture_file();
    let expected_uri = file_uri(&main_rs);
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::Rust, fake_server_path());

    manager
        .notify_file_changed_default(&main_rs, "fn main() { println!(\"one\"); }\n")
        .expect("first notify");
    let opened = custom_event(&mut manager, "custom/documentOpened");
    assert_eq!(opened["uri"], expected_uri);
    assert_eq!(opened["version"], 0);

    manager
        .notify_file_changed_default(&main_rs, "fn main() { println!(\"two\"); }\n")
        .expect("second notify");
    let changed = custom_event(&mut manager, "custom/documentChanged");
    assert_eq!(changed["uri"], expected_uri);
    assert_eq!(changed["version"], 1);
}

#[test]
fn test_document_store_version_tracking() {
    let temp_dir = tempdir().expect("tempdir");
    let path = temp_dir.path().join("demo.rs");
    let mut store = DocumentStore::new();

    assert_eq!(store.open(path.clone()), 0);
    assert_eq!(store.version(&path), Some(0));
    assert_eq!(store.bump_version(&path), Some(1));
    assert_eq!(store.bump_version(&path), Some(2));
    assert_eq!(store.version(&path), Some(2));
}

#[test]
fn test_document_store_close_and_reopen() {
    let temp_dir = tempdir().expect("tempdir");
    let path = temp_dir.path().join("demo.rs");
    let mut store = DocumentStore::new();

    assert_eq!(store.open(path.clone()), 0);
    assert_eq!(store.close(&path), Some(0));
    assert!(!store.is_open(&path));
    assert_eq!(store.open(path.clone()), 0);
    assert_eq!(store.version(&path), Some(0));
}
