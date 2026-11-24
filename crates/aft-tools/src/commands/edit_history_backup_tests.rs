use crate::protocol::DEFAULT_SESSION_ID;
use std::fs;

#[test]
fn backup_memory_history_response_is_byte_identical_after_release_and_restart() {
    use crate::commands::edit_history::handle_edit_history;
    use crate::config::Config;
    use crate::context::AppContext;
    use crate::parser::TreeSitterProvider;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("file.bin");
    let storage = root.path().join("storage");
    let make_context = || {
        let ctx = crate::test_context(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.backup().lock().set_storage_dir(storage.clone(), 72);
        ctx
    };
    let ctx = make_context();
    let ids = {
        let mut store = ctx.backup().lock();
        fs::write(&path, [0, 255, 128]).unwrap();
        let first = store
            .snapshot(DEFAULT_SESSION_ID, &path, "first")
            .unwrap()
            .unwrap();
        fs::write(&path, b"text").unwrap();
        let second = store
            .snapshot(DEFAULT_SESSION_ID, &path, "second")
            .unwrap()
            .unwrap();
        let meta_path = store
            .history_meta_path_for_test(DEFAULT_SESSION_ID, &path)
            .unwrap();
        let mut meta: serde_json::Value =
            serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        for entry in meta["entries"].as_array_mut().unwrap() {
            entry["timestamp"] = 42.into();
        }
        fs::write(meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        [first, second]
    };
    let request = serde_json::from_value(
        serde_json::json!({"id":"history", "command":"edit_history", "file":"file.bin"}),
    )
    .unwrap();
    let expected = format!("{{\"id\":\"history\",\"success\":true,\"file\":\"file.bin\",\"entries\":[{{\"backup_id\":\"{}\",\"timestamp\":42,\"description\":\"second\",\"markers\":[],\"external_change_checkpoint\":null,\"generation\":0,\"previous_file\":false}},{{\"backup_id\":\"{}\",\"timestamp\":42,\"description\":\"first\",\"markers\":[],\"external_change_checkpoint\":null,\"generation\":0,\"previous_file\":false}}]}}", ids[1], ids[0]);
    let response_bytes =
        |ctx: &AppContext| serde_json::to_string(&handle_edit_history(&request, ctx)).unwrap();
    assert_eq!(response_bytes(&ctx), expected);
    ctx.backup().lock().release_memory();
    assert_eq!(response_bytes(&ctx), expected);
    drop(ctx);
    let restarted = make_context();
    assert_eq!(response_bytes(&restarted), expected);
    assert_eq!(
        restarted.backup().lock().history_content_reads_for_test(),
        0
    );
}
