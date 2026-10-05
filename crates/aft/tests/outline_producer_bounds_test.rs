use aft::commands::outline::handle_outline;
use aft::config::Config;
use aft::context::{default_language_provider_factory, AppContext};
use aft::protocol::RawRequest;
use serde_json::json;
use std::fs;

fn outline(root: &std::path::Path) -> serde_json::Value {
    let ctx = AppContext::new(
        default_language_provider_factory(),
        crate::context_storage::isolate(Config {
            project_root: Some(root.to_path_buf()),
            ..Default::default()
        }),
    );
    let response = handle_outline(
        &RawRequest {
            id: "bounds".into(),
            command: "outline".into(),
            session_id: None,
            lsp_hints: None,
            params: json!({"directory": root, "files": true, "include_tests": true}),
        },
        &ctx,
    );
    assert!(response.success, "{response:?}");
    response.data
}

#[test]
fn expensive_outline_line_count_is_unknown() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("large.txt"), "x\n".repeat(2_000_000)).unwrap();
    let started = std::time::Instant::now();
    let data = outline(temp.path());
    eprintln!(
        "outline 4000000 bytes: {:?}, lines={}",
        started.elapsed(),
        data["files"][0]["lines"]
    );
    assert!(data["files"][0]["lines"].is_null());
    assert_eq!(data["complete"], false);
}

#[test]
fn empty_directory_tree_has_entry_budget() {
    let temp = tempfile::tempdir().unwrap();
    for n in 0..11_000 {
        fs::create_dir(temp.path().join(format!("dir-{n:05}"))).unwrap();
    }
    let started = std::time::Instant::now();
    let data = outline(temp.path());
    eprintln!(
        "outline 11000 empty dirs: {:?}, complete={}",
        started.elapsed(),
        data["complete"]
    );
    assert_eq!(data["complete"], false);
}
