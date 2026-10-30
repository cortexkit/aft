use super::*;
use crate::edit::mutation::set_before_check;
use crate::protocol::RawRequest;
use serde_json::json;
use std::path::Path;

const ORIGINAL: &str = "export const value = 1;\n";

#[derive(Clone, Copy, Debug)]
enum Tool {
    Edit,
    Write,
    Batch,
    Patch,
    Ast,
    Import,
    Append,
    Symbol,
}

fn request(tool: Tool, path: &Path, number: usize) -> RawRequest {
    let (command, params) = match tool {
        Tool::Edit => (
            "edit_match",
            json!({"file": path, "match": "value = 1", "replacement": format!("value = {number}")}),
        ),
        Tool::Write => (
            "write",
            json!({"file": path, "content": format!("export const value = {number};\n")}),
        ),
        Tool::Batch => (
            "batch",
            json!({"file": path, "edits": [{"line_start": 1, "line_end": 1, "content": format!("export const value = {number};")}]}),
        ),
        Tool::Patch => (
            "apply_patch",
            json!({"patch_text": format!("*** Begin Patch\n*** Update File: {}\n@@\n-export const value = 1;\n+export const value = {number};\n*** End Patch", path.display())}),
        ),
        Tool::Ast => (
            "ast_replace",
            json!({"paths": [path], "lang": "typescript", "pattern": "const value = 1;", "rewrite": format!("const value = {number};")}),
        ),
        Tool::Import => (
            "add_import",
            json!({"file": path, "module": format!("module-{number}"), "names": [format!("Thing{number}")]}),
        ),
        Tool::Append => (
            "edit_match",
            json!({"file": path, "op": "append", "append_content": format!("// writer {number}\n")}),
        ),
        Tool::Symbol => (
            "edit_symbol",
            json!({"file": path, "symbol": "value", "operation": "replace", "content": format!("export const value = {number};")}),
        ),
    };
    let mut value = params;
    value["id"] = json!(format!("writer-{number}"));
    value["command"] = json!(command);
    value["session_id"] = json!(format!("session-{number}"));
    serde_json::from_value(value).unwrap()
}

fn run(req: &RawRequest, ctx: &AppContext) -> Response {
    use crate::commands::*;
    match req.command.as_str() {
        "edit_match" => edit_match::handle_edit_match(req, ctx),
        "edit_symbol" => edit_symbol::handle_edit_symbol(req, ctx),
        "write" => write::handle_write(req, ctx),
        "batch" => batch::handle_batch(req, ctx),
        "apply_patch" => apply_patch::handle_apply_patch(req, ctx),
        "ast_replace" => ast_replace::handle_ast_replace(req, ctx),
        "add_import" => add_import::handle_add_import(req, ctx),
        _ => panic!("unexpected test command"),
    }
}

fn context(root: &Path) -> Arc<AppContext> {
    Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(root.to_path_buf()),
            format_on_edit: false,
            ..Config::default()
        },
    ))
}

struct Hook(std::path::PathBuf);
impl Drop for Hook {
    fn drop(&mut self) {
        set_before_check(&self.0, None);
    }
}

fn assert_winner(tool: Tool, path: &Path, number: usize) {
    let content = std::fs::read_to_string(path).unwrap();
    match tool {
        Tool::Import => {
            assert!(content.contains(&format!("module-{number}")), "{content}");
            assert!(content.contains(&format!("Thing{number}")), "{content}");
            assert!(
                !content.contains(&format!("module-{}", 5 - number)),
                "{content}"
            );
            assert!(content.ends_with(ORIGINAL), "{content}");
        }
        Tool::Append => assert_eq!(content, format!("{ORIGINAL}// writer {number}\n")),
        _ => assert_eq!(content, format!("export const value = {number};\n")),
    }
}

fn race(tool: Tool) {
    let dir = tempfile::tempdir().unwrap();
    let outer = std::fs::canonicalize(dir.path()).unwrap();
    let inner = outer.join("nested");
    std::fs::create_dir(&inner).unwrap();
    let path = inner.join("shared.ts");
    std::fs::write(&path, ORIGINAL).unwrap();
    let contexts = [context(&outer), context(&inner)];
    let roots = [
        ProjectRootId::from_path(&outer).unwrap(),
        ProjectRootId::from_path(&inner).unwrap(),
    ];
    let executor = test_executor(4, 2, 3, 2);
    for index in 0..2 {
        executor.register_actor(roots[index].clone(), Arc::clone(&contexts[index]));
    }
    let (arrived, arrival) = crossbeam_channel::bounded(2);
    let (release, released) = crossbeam_channel::bounded(2);
    let checks = AtomicUsize::new(0);
    set_before_check(
        &path,
        Some(Arc::new(move || {
            if checks.fetch_add(1, Ordering::SeqCst) < 2 {
                arrived.send(()).unwrap();
                released
                    .recv_timeout(Duration::from_secs(10))
                    .expect("release both observed writers");
            }
        })),
    );
    let _hook = Hook(path.clone());
    let handles: Vec<_> = roots
        .into_iter()
        .enumerate()
        .map(|(index, root)| {
            let req = request(tool, &path, index + 2);
            executor.submit(
                root,
                Lane::Mutating,
                req.id.clone(),
                Box::new(move |ctx| run(&req, ctx)),
            )
        })
        .collect();
    for _ in 0..2 {
        arrival
            .recv_timeout(Duration::from_secs(10))
            .expect("both roots read the original");
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), ORIGINAL);
    release.send(()).unwrap();
    release.send(()).unwrap();
    let replies: Vec<_> = handles
        .into_iter()
        .map(|handle| {
            handle
                .recv_timeout(Duration::from_secs(20))
                .expect("writer completes without deadlock")
        })
        .collect();
    assert_eq!(
        replies.iter().filter(|reply| reply.success).count(),
        1,
        "{tool:?}: {replies:?}"
    );
    for (index, reply) in replies.iter().enumerate() {
        let session = format!("session-{}", index + 2);
        let history = contexts[index].backup().lock().history(&session, &path);
        if reply.success {
            assert_winner(tool, &path, index + 2);
            assert_eq!(history.len(), 1, "winner has its normal undo snapshot");
            assert_eq!(history[0].content().unwrap(), ORIGINAL);
        } else {
            assert_eq!(reply.data["code"], "file_changed_since_read", "{reply:?}");
            assert!(
                history.is_empty(),
                "refused writer must have no undo snapshot"
            );
        }
    }
}

#[test]
fn stale_write_edit_two_roots() {
    race(Tool::Edit);
}
#[test]
fn stale_write_write_two_roots() {
    race(Tool::Write);
}
#[test]
fn stale_write_batch_two_roots() {
    race(Tool::Batch);
}
#[test]
fn stale_write_patch_two_roots() {
    race(Tool::Patch);
}
#[test]
fn stale_write_ast_two_roots() {
    race(Tool::Ast);
}
#[test]
fn stale_write_import_two_roots() {
    race(Tool::Import);
}
#[test]
fn stale_write_append_two_roots() {
    race(Tool::Append);
}
#[test]
fn stale_write_symbol_two_roots() {
    race(Tool::Symbol);
}

#[test]
fn stale_write_message_and_code() {
    let error = crate::error::AftError::FileChangedSinceRead {
        path: "src/example.ts".into(),
    };
    assert_eq!(
        error.to_error_json(),
        json!({
            "code": "file_changed_since_read",
            "message": "src/example.ts changed on disk after this call read it; nothing was written. Read the file again and retry."
        })
    );
}

#[test]
fn stale_write_single_writer_keeps_normal_undo() {
    for tool in [
        Tool::Edit,
        Tool::Write,
        Tool::Batch,
        Tool::Patch,
        Tool::Ast,
        Tool::Import,
        Tool::Append,
        Tool::Symbol,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("single.ts");
        std::fs::write(&path, ORIGINAL).unwrap();
        let ctx = context(&root);
        let req = request(tool, &path, 2);
        let reply = run(&req, &ctx);
        assert!(reply.success, "{tool:?}: {reply:?}");
        assert_winner(tool, &path, 2);
        let mut backup = ctx.backup().lock();
        let history = backup.history(req.session(), &path);
        assert_eq!(history.len(), 1, "{tool:?}");
        assert_eq!(history[0].content().unwrap(), ORIGINAL);
        backup.restore_last_operation(req.session()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            ORIGINAL,
            "{tool:?}"
        );
        assert!(backup.history(req.session(), &path).is_empty());
    }
}

#[test]
fn stale_write_after_backup_discards_undo() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("after-backup.ts");
    std::fs::write(&path, ORIGINAL).unwrap();
    let ctx = context(&root);
    let checks = AtomicUsize::new(0);
    let changed_path = path.clone();
    set_before_check(
        &path,
        Some(Arc::new(move || {
            if checks.fetch_add(1, Ordering::SeqCst) == 1 {
                std::fs::write(&changed_path, "// external winner\n").unwrap();
            }
        })),
    );
    let _hook = Hook(path.clone());
    let req = request(Tool::Write, &path, 2);
    let reply = run(&req, &ctx);
    assert!(!reply.success, "{reply:?}");
    assert_eq!(reply.data["code"], "file_changed_since_read");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "// external winner\n"
    );
    assert!(ctx.backup().lock().history(req.session(), &path).is_empty());
}

fn replace_at_check(path: &Path, text: &'static str) -> Hook {
    let changed_path = path.to_path_buf();
    set_before_check(
        path,
        Some(Arc::new(move || {
            std::fs::write(&changed_path, text).unwrap();
        })),
    );
    Hook(path.to_path_buf())
}

#[test]
fn stale_write_patch_multifile_refuses_before_any_write_or_delete() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let first = root.join("a.ts");
    let stale = root.join("z.ts");
    let deleted = root.join("delete.ts");
    let created = root.join("new.ts");
    for path in [&first, &stale, &deleted] {
        std::fs::write(path, ORIGINAL).unwrap();
    }
    let ctx = context(&root);
    let _hook = replace_at_check(&stale, "// external winner\n");
    let req: RawRequest = serde_json::from_value(json!({
        "id": "patch-multiple", "command": "apply_patch", "session_id": "patch-multiple",
        "patch_text": format!("*** Begin Patch\n*** Update File: {}\n@@\n-export const value = 1;\n+export const value = 2;\n*** Delete File: {}\n*** Add File: {}\n+// created\n*** Update File: {}\n@@\n-export const value = 1;\n+export const value = 2;\n*** End Patch", first.display(), deleted.display(), created.display(), stale.display())
    })).unwrap();
    let reply = run(&req, &ctx);
    assert!(!reply.success, "{reply:?}");
    assert_eq!(reply.data["code"], "file_changed_since_read");
    assert_eq!(std::fs::read_to_string(&first).unwrap(), ORIGINAL);
    assert_eq!(std::fs::read_to_string(&deleted).unwrap(), ORIGINAL);
    assert_eq!(
        std::fs::read_to_string(&stale).unwrap(),
        "// external winner\n"
    );
    assert!(!created.exists());
    assert!(ctx.backup().lock().tracked_files(req.session()).is_empty());
}

#[test]
fn stale_write_patch_delete_observes_version() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("delete.ts");
    std::fs::write(&path, ORIGINAL).unwrap();
    let ctx = context(&root);
    let _hook = replace_at_check(&path, "// external winner\n");
    let req: RawRequest = serde_json::from_value(json!({
        "id": "delete", "command": "apply_patch", "patch_text": format!("*** Begin Patch\n*** Delete File: {}\n*** End Patch", path.display())
    })).unwrap();
    let reply = run(&req, &ctx);
    assert_eq!(reply.data["code"], "file_changed_since_read", "{reply:?}");
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "// external winner\n"
    );
    assert!(ctx.backup().lock().tracked_files(req.session()).is_empty());
}

#[test]
fn stale_write_creates_must_still_be_absent() {
    for patch in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("new.ts");
        let ctx = context(&root);
        let _hook = replace_at_check(&path, "// created by someone else\n");
        let req = if patch {
            serde_json::from_value(json!({"id": "create", "command": "apply_patch", "patch_text": format!("*** Begin Patch\n*** Add File: {}\n+// mine\n*** End Patch", path.display())})).unwrap()
        } else {
            request(Tool::Write, &path, 2)
        };
        let reply = run(&req, &ctx);
        assert_eq!(reply.data["code"], "file_changed_since_read", "{reply:?}");
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "// created by someone else\n"
        );
        assert!(ctx.backup().lock().tracked_files(req.session()).is_empty());
    }
}

#[test]
fn stale_write_opposite_patch_orders_finish_without_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    let outer = std::fs::canonicalize(dir.path()).unwrap();
    let inner = outer.join("nested");
    std::fs::create_dir(&inner).unwrap();
    let a = inner.join("a.ts");
    let b = inner.join("b.ts");
    for path in [&a, &b] {
        std::fs::write(path, ORIGINAL).unwrap();
    }
    let executor = test_executor(4, 2, 3, 2);
    let roots = [
        ProjectRootId::from_path(&outer).unwrap(),
        ProjectRootId::from_path(&inner).unwrap(),
    ];
    executor.register_actor(roots[0].clone(), context(&outer));
    executor.register_actor(roots[1].clone(), context(&inner));
    let (arrived, arrival) = crossbeam_channel::bounded(2);
    let (release, released) = crossbeam_channel::bounded(2);
    set_before_check(
        &a,
        Some(Arc::new(move || {
            arrived.send(()).unwrap();
            released
                .recv_timeout(Duration::from_secs(10))
                .expect("release both patches");
        })),
    );
    let _hook = Hook(a.clone());
    let handles: Vec<_> = roots.into_iter().enumerate().map(|(index, root)| {
        let paths = if index == 0 { [&a, &b] } else { [&b, &a] };
        let hunks = paths.iter().map(|path| format!("*** Update File: {}\n@@\n-export const value = 1;\n+export const value = {};\n", path.display(), index + 2)).collect::<String>();
        let req: RawRequest = serde_json::from_value(json!({"id": format!("patch-{index}"), "command": "apply_patch", "session_id": format!("patch-{index}"), "patch_text": format!("*** Begin Patch\n{hunks}*** End Patch")})).unwrap();
        executor.submit(root, Lane::Mutating, req.id.clone(), Box::new(move |ctx| run(&req, ctx)))
    }).collect();
    for _ in 0..2 {
        arrival
            .recv_timeout(Duration::from_secs(10))
            .expect("both patches observed both files");
    }
    for _ in 0..2 {
        release.send(()).unwrap();
    }
    let replies: Vec<_> = handles
        .into_iter()
        .map(|handle| {
            handle
                .recv_timeout(Duration::from_secs(20))
                .expect("opposite patch order must not deadlock")
        })
        .collect();
    assert_eq!(
        replies.iter().filter(|reply| reply.success).count(),
        1,
        "{replies:?}"
    );
    let winner = replies.iter().position(|reply| reply.success).unwrap() + 2;
    let loser = replies.iter().find(|reply| !reply.success).unwrap();
    assert_eq!(loser.data["code"], "file_changed_since_read");
    for path in [&a, &b] {
        assert_winner(Tool::Patch, path, winner);
    }
}
