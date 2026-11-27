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
        "inspect",
        "inspect_tier2_run",
        "undo_preview",
        "safety_purge_paths",
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
        "checkpoint",
        "restore_checkpoint",
        "safety_purge",
        "bash",
        "powershell",
        "configure",
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
    let ctx = Arc::new(crate::test_context(
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
fn backup_bookkeeping_keeps_undo_entry_but_does_not_infer_skipped_reason() {
    use aft_engine::test_bridge::search_misc::BackupStoreTestExt as _;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("small.rs");
    let original = "pub fn small() {}\n";
    std::fs::write(&path, original).unwrap();
    let ctx = crate::test_context(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.path().to_path_buf()),
            ..Default::default()
        },
    );
    ctx.backup().lock().permit_temp_path_snapshots_for_test();
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

    // The wrapper cannot know which backup event belongs to a dispatched
    // operation. A handler must attach its own skipped reason.
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
    assert!(result.response.success);
    assert!(result.response.data.get("backup_skipped_reason").is_none());
}

#[test]
fn backup_bookkeeping_does_not_attach_a_concurrent_calls_skip() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let config = crate::config::Config {
        project_root: Some(path.clone()),
        storage_dir: Some(path.join("storage")),
        ..Default::default()
    };
    let ctx = crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.backup().lock().set_policy(crate::backup::BackupPolicy {
        enabled: false,
        ..Default::default()
    });
    std::fs::write(path.join("sample.txt"), "before\n").unwrap();
    let tool_ctx = ToolCallContext {
        project_root: path.clone(),
        session_id: Some("same-session".into()),
        request_id: "conflicts-request".into(),
        ..Default::default()
    };
    let write_ctx = ToolCallContext {
        request_id: "write-request".into(),
        ..tool_ctx.clone()
    };
    std::thread::scope(|scope| {
        let (entered, proceed) = mpsc::sync_channel::<()>(0);
        let (resume, paused) = mpsc::sync_channel::<()>(0);
        let (ctx, tool_ctx) = (&ctx, &tool_ctx);
        let conflicts = scope.spawn(move || {
            let dispatch = |req: RawRequest, _: &AppContext| {
                assert_eq!(req.command, "git_conflicts");
                entered.send(()).unwrap();
                paused.recv().unwrap();
                Response::success(&req.id, json!({"conflicts": []}))
            };
            let ToolCallOutcome::Unary(result) = run_tool_call(
                "conflicts",
                json!({}),
                &Default::default(),
                tool_ctx,
                ctx,
                &dispatch,
                None,
                None,
            );
            result
        });
        proceed.recv().unwrap();
        let dispatch =
            |req: RawRequest, ctx: &AppContext| crate::commands::write::handle_write(&req, ctx);
        let ToolCallOutcome::Unary(written) = run_tool_call(
            "write",
            json!({"path":"sample.txt", "content":"after\n"}),
            &Default::default(),
            &write_ctx,
            ctx,
            &dispatch,
            None,
            None,
        );
        resume.send(()).unwrap();
        let unchanged = conflicts.join().unwrap();
        assert!(written.response.success, "{:?}", written.response);
        assert_eq!(written.response.data["backup_skipped_reason"], "disabled");
        assert!(unchanged.response.success, "{:?}", unchanged.response);
        assert!(
            unchanged
                .response
                .data
                .get("backup_skipped_reason")
                .is_none(),
            "{:?}",
            unchanged.response
        );
        assert!(!unchanged.text.contains("Undo is unavailable"));
    });
}

#[test]
fn import_and_refactor_handlers_report_their_own_backup_skips() {
    use crate::commands::{
        add_import::handle_add_import, extract_function::handle_extract_function,
        inline_symbol::handle_inline_symbol, move_symbol::handle_move_symbol,
        organize_imports::handle_organize_imports, remove_import::handle_remove_import,
    };
    type Handler = fn(&RawRequest, &AppContext) -> Response;
    let cases: [(&str, Handler, &str, Value); 6] = [
        ("add_import", handle_add_import, "export const x = 1;\n", json!({"module":"./dep","names":["dep"]})),
        ("remove_import", handle_remove_import, "import { dep } from './dep';\nexport const x = 1;\n", json!({"module":"./dep"})),
        ("organize_imports", handle_organize_imports, "import { z } from './z';\nimport { a } from './a';\nexport const x = 1;\n", json!({})),
        ("extract_function", handle_extract_function, "function f(a: number) {\n  const x = a + 1;\n  return x;\n}\n", json!({"name":"makeX","start_line":2,"end_line":3})),
        ("inline_symbol", handle_inline_symbol, "export function double(n: number): number {\n  return n * 2;\n}\n\nexport function useIt(x: number): number {\n  return double(x);\n}\n", json!({"symbol":"double","call_site_line":6})),
        ("move_symbol", handle_move_symbol, "export function keep() { return 1; }\nexport function moveMe() { return 2; }\n", json!({"symbol":"moveMe","destination":"dest.ts"})),
    ];
    let mut failures = Vec::new();
    for (command, dispatch, source, mut args) in cases {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("source.ts");
        std::fs::write(&path, source).unwrap();
        args["file"] = json!(path);
        args["format"] = json!(false);
        let config = crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(root.join("storage")),
            ..Default::default()
        };
        let ctx = crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.backup().lock().set_policy(crate::backup::BackupPolicy {
            enabled: false,
            ..Default::default()
        });
        let result = dispatch(&request(command, args), &ctx);
        if !result.success
            || result.data["backup_skipped_reason"] != "disabled"
            || std::fs::read_to_string(path).unwrap() == source
        {
            failures.push((command, result));
        }
    }
    assert!(
        failures.is_empty(),
        "handlers that did not mutate and report their skip: {failures:?}"
    );
}
