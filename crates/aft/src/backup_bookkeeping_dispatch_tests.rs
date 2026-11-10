use super::*;
use std::path::PathBuf;
use std::sync::mpsc;

fn request(command: &str, params: serde_json::Value) -> RawRequest {
    RawRequest {
        id: format!("backup-bookkeeping-{command}"),
        command: command.into(),
        params,
        session_id: Some("backup-bookkeeping".into()),
        lsp_hints: None,
    }
}

fn fixture() -> (tempfile::TempDir, Arc<AppContext>) {
    // Undo must exercise a real snapshot, not the system-temp backup exemption.
    let scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let root = tempfile::Builder::new()
        .prefix("backup-bookkeeping-")
        .tempdir_in(scratch)
        .unwrap();
    let config = crate::context_storage::isolate(Config {
        project_root: Some(root.path().canonicalize().unwrap()),
        ..Config::default()
    });
    let storage = config.storage_dir.clone().unwrap();
    let ctx = Arc::new(AppContext::new(
        Box::new(aft::parser::TreeSitterProvider::new()),
        config,
    ));
    ctx.backup().lock().set_storage_dir(storage, 72);
    std::fs::write(root.path().join("small.rs"), "pub fn small() {}\n").unwrap();
    (root, ctx)
}

#[test]
fn backup_bookkeeping_native_and_wrapped_readers_complete_with_store_owned() {
    let (_root, ctx) = fixture();
    let owner = ctx.backup().lock();
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let reader_ctx = Arc::clone(&ctx);
        let worker = scope.spawn(move || {
            let mut responses = Vec::new();
            for (command, params) in [
                ("outline", serde_json::json!({"file": "small.rs"})),
                ("outline", serde_json::json!({"files": ["small.rs"]})),
                ("outline", serde_json::json!({"directory": "."})),
                ("outline", serde_json::json!({"target": ".", "files": true})),
                ("read", serde_json::json!({"file": "small.rs"})),
                (
                    "tool_call",
                    serde_json::json!({"name": "outline", "arguments": {"target": "small.rs"}}),
                ),
                (
                    "tool_call",
                    serde_json::json!({"name": "read", "arguments": {"path": "small.rs"}}),
                ),
            ] {
                responses.push(dispatch(request(command, params), &reader_ctx));
            }
            tx.send(responses).unwrap();
        });
        let completed_while_owned = rx.recv_timeout(Duration::from_secs(5));
        // A regression must release the owner before joining its blocked reader.
        drop(owner);
        worker.join().unwrap();
        for response in completed_while_owned.expect("native reader waited for BackupStore") {
            assert!(response.success, "{response:?}");
        }
    });
}

#[test]
fn backup_bookkeeping_wrapped_write_keeps_undo_entry_and_post_state() {
    let (root, ctx) = fixture();
    let path = root.path().join("small.rs");
    let original = "pub fn small() {}\n";
    let written = "pub fn changed() {}\n";
    let response = dispatch(
        request(
            "tool_call",
            serde_json::json!({
                "name": "write", "arguments": {"path": "small.rs", "content": written}
            }),
        ),
        &ctx,
    );
    assert!(response.success, "{response:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
    assert_eq!(
        ctx.backup()
            .lock()
            .history("backup-bookkeeping", &path)
            .len(),
        1
    );

    // Missing post-state would let undo adopt this external change as the
    // writer's own result when it drains unfinished mutation records.
    std::fs::write(&path, "pub fn external() {}\n").unwrap();
    let undo = || request("undo", serde_json::json!({"file": "small.rs"}));
    let refused = dispatch(undo(), &ctx);
    assert!(
        !refused.success,
        "undo accepted an external edit: {refused:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "pub fn external() {}\n"
    );
    std::fs::write(&path, written).unwrap();
    let restored = dispatch(undo(), &ctx);
    assert!(restored.success, "{restored:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}
