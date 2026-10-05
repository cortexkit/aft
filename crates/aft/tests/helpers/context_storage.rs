#![allow(dead_code)]
//! Integration tests link AFT without cfg(test), so they need explicit storage.
use aft::config::Config;
use std::sync::{Mutex, OnceLock};
use tempfile::TempDir;

fn retained() -> &'static Mutex<Vec<TempDir>> {
    static DIRS: OnceLock<Mutex<Vec<TempDir>>> = OnceLock::new();
    DIRS.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) fn isolate(mut config: Config) -> Config {
    if config
        .storage_dir
        .as_ref()
        .is_none_or(|p| p.as_os_str().is_empty())
    {
        let dir = tempfile::Builder::new()
            .prefix("aft-integration-context-")
            .tempdir()
            .expect("private integration storage");
        config.storage_dir = Some(dir.path().to_path_buf());
        // Some helpers return only AppContext and hand it to background work.
        // Retain the directory through the test process, not a stack-local guard.
        retained()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(dir);
    }
    config
}

#[test]
fn integration_context_storage_is_explicit_and_retained() {
    let first = isolate(Config::default());
    let second = isolate(Config::default());
    let path = first.storage_dir.clone().unwrap();
    assert_ne!(first.storage_dir, second.storage_dir);
    let ctx =
        aft::context::AppContext::new(Box::new(aft::parser::TreeSitterProvider::new()), first);
    assert_eq!(ctx.config().storage_dir, Some(path.clone()));
    drop(ctx);
    assert!(path.is_dir());
}

#[test]
fn integration_context_storage_preserves_explicit_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let config = isolate(Config {
        storage_dir: Some(dir.path().into()),
        ..Default::default()
    });
    assert_eq!(config.storage_dir.as_deref(), Some(dir.path()));
}
