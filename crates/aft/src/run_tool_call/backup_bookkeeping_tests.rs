use super::*;
use std::sync::{mpsc, Arc};

fn request(command: &str, params: Value) -> RawRequest {
    RawRequest {
        id: "backup-bookkeeping".into(),
        command: command.into(),
        params,
        session_id: Some("backup-bookkeeping".into()),
        lsp_hints: None,
    }
}

#[test]
fn backup_bookkeeping_is_conservative_for_mutations_and_tool_envelopes() {
    for command in [
        "outline",
        "zoom",
        "read",
        "grep",
        "glob",
        "search",
        "semantic_search",
        "callgraph",
        "callers",
        "call_tree",
        "impact",
        "trace_to",
        "trace_to_symbol",
        "trace_data",
    ] {
        assert!(
            is_read_only_request(&request(command, json!({}))),
            "{command}"
        );
        for name in [command.to_string(), format!("aft_{command}")] {
            assert!(
                is_read_only_request(&request("tool_call", json!({"name": name}))),
                "{name}"
            );
        }
    }
    for command in [
        "write",
        "edit_match",
        "edit_symbol",
        "batch",
        "apply_patch",
        "hashline_edit",
        "delete_file",
        "move_file",
        "add_import",
        "remove_import",
        "organize_imports",
        "move_symbol",
        "extract_function",
        "inline_symbol",
        "ast_replace",
        "undo",
        "undo_preview",
        "checkpoint",
        "restore_checkpoint",
        "safety_purge",
        "safety_purge_paths",
        "bash",
        "powershell",
        "configure",
        "inspect",
        "unknown",
        "",
    ] {
        assert!(
            !is_read_only_request(&request(command, json!({}))),
            "{command}"
        );
        assert!(
            !is_read_only_request(&request("tool_call", json!({"name": command}))),
            "{command}"
        );
    }
    assert!(!is_read_only_request(&request("tool_call", json!({}))));
    assert!(!is_read_only_request(&request(
        "tool_call",
        json!({"name": 12})
    )));
}

#[test]
fn backup_bookkeeping_read_only_calls_do_not_wait_for_store_owner() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("small.rs");
    std::fs::write(&path, "pub fn small() {}\n").unwrap();
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.path().to_path_buf()),
            ..Default::default()
        },
    ));
    let root = root.path().to_path_buf();
    let (tx, rx) = mpsc::sync_channel(1);
    let owner = ctx.backup().lock();
    std::thread::scope(|scope| {
        let reader_ctx = Arc::clone(&ctx);
        let worker = scope.spawn(move || {
            let mut responses = Vec::new();
            for (name, args) in [
                ("outline", json!({"target": "small.rs"})),
                ("outline", json!({"target": ["small.rs"]})),
                ("outline", json!({"target": ".", "files": true})),
                ("outline", json!({"target": "."})),
                ("read", json!({"path": "small.rs"})),
                (
                    "zoom",
                    json!({"batch": [{"target": "small.rs", "symbols": "small"}]}),
                ),
            ] {
                let format = crate::subc_format::FormatContext::from_tool_call(name, &args, &root);
                let tool_ctx = ToolCallContext {
                    project_root: root.clone(),
                    session_id: Some("reader".into()),
                    request_id: "reader".into(),
                    standard_edit_grammar: true,
                    ..Default::default()
                };
                let dispatch = |req: RawRequest, ctx: &AppContext| match req.command.as_str() {
                    "outline" => crate::commands::outline::handle_outline(&req, ctx),
                    "read" => crate::commands::read::handle_read(&req, ctx),
                    "zoom" => crate::commands::zoom::handle_zoom(&req, ctx),
                    other => panic!("unexpected command: {other}"),
                };
                let ToolCallOutcome::Unary(result) = run_tool_call(
                    name,
                    args,
                    &format,
                    &tool_ctx,
                    &reader_ctx,
                    &dispatch,
                    None,
                    None,
                );
                responses.push(result.response);
            }
            tx.send(responses).unwrap();
        });
        // Release before joining even on failure: an old implementation parks
        // the reader on this guard, and must fail rather than hang the suite.
        let completed_while_owned = rx.recv_timeout(Duration::from_secs(5));
        drop(owner);
        worker.join().unwrap();
        let responses = completed_while_owned.expect("read-only calls waited for BackupStore");
        for response in responses {
            assert!(response.success, "{response:?}");
        }
    });
}

#[test]
fn backup_bookkeeping_mutating_tool_keeps_skipped_reason_and_undo_entry() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("small.rs");
    let original = "pub fn small() {}\n";
    std::fs::write(&path, original).unwrap();
    let ctx = AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.path().to_path_buf()),
            ..Default::default()
        },
    );
    let args = json!({"path": "small.rs", "content": "pub fn changed() {}\n"});
    let format = crate::subc_format::FormatContext::from_tool_call("write", &args, root.path());
    let tool_ctx = ToolCallContext {
        project_root: root.path().to_path_buf(),
        session_id: Some("writer".into()),
        request_id: "writer".into(),
        standard_edit_grammar: true,
        ..Default::default()
    };
    let dispatch =
        |req: RawRequest, ctx: &AppContext| crate::commands::write::handle_write(&req, ctx);
    let ToolCallOutcome::Unary(result) = run_tool_call(
        "write", args, &format, &tool_ctx, &ctx, &dispatch, None, None,
    );
    assert!(result.response.success, "{:?}", result.response);
    let history = ctx.backup().lock().history("writer", &path);
    assert_eq!(history.len(), 1);

    // Skipped reasons are added by the wrapper even if a successful mutation
    // handler has not attached the reason to its own response.
    ctx.backup().lock().set_policy(crate::backup::BackupPolicy {
        enabled: false,
        ..Default::default()
    });
    let args = json!({"path": "small.rs", "content": "pub fn skipped() {}\n"});
    let format = crate::subc_format::FormatContext::from_tool_call("write", &args, root.path());
    let dispatch = |req: RawRequest, ctx: &AppContext| {
        ctx.backup()
            .lock()
            .snapshot_with_op(req.session(), &path, "skipped probe", Some("skipped"))
            .unwrap();
        Response::success(&req.id, json!({}))
    };
    let ToolCallOutcome::Unary(result) = run_tool_call(
        "write", args, &format, &tool_ctx, &ctx, &dispatch, None, None,
    );
    assert_eq!(result.response.data["backup_skipped_reason"], "disabled");
}
