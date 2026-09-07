// These integration tests build shell-style command strings with paths like
// `cat /tmp/x` or `grep needle /tmp/src`. On Windows, the equivalent paths
// contain backslashes (`C:\Users\...\src`) that the shell parser inside the
// rewrite engine treats as escape sequences, breaking command-line tokenization.
// The rewrite engine itself is platform-agnostic (538 unit tests pass on
// Windows); these scenario-driven integration tests are Unix-only by design.
//
// Real Windows agents that send `cat C:\path\file` to AFT bash would either
// quote the path or use forward slashes; the unit-test cases for both shapes
// live in the rewrite parser's own tests, not here.
#![cfg(unix)]

use std::fs;
use std::time::{Duration, SystemTime};

use aft::bash_rewrite::{parser, try_rewrite};
use aft::commands::edit_match::handle_edit_match;
use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use aft::sandbox_spawn::AuthenticatedPrincipal;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

// Warn-level log capture is shared across all integration test modules via a
// single process-global, thread-local-capturing logger. See test_helpers.
use crate::test_helpers::{init_test_logger, take_logs, user_config, AftProcess};

fn context(root: &std::path::Path, enabled: bool) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(root.to_path_buf()),
            experimental_bash_rewrite: enabled,
            restrict_to_project_root: true,
            ..Config::default()
        },
    )
}

fn context_with_search(root: &std::path::Path, aft_search_registered: bool) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(root.to_path_buf()),
            experimental_bash_rewrite: true,
            restrict_to_project_root: true,
            aft_search_registered,
            ..Config::default()
        },
    )
}

fn request(command: &str, params: Value) -> RawRequest {
    RawRequest {
        id: "test".to_string(),
        command: command.to_string(),
        lsp_hints: None,
        session_id: None,
        params,
    }
}

fn rewrite(command: &str, ctx: &AppContext) -> Option<Value> {
    try_rewrite(command, None, ctx, &AuthenticatedPrincipal::FirstParty)
        .map(|response| response.data)
}

fn rewrite_with_session(command: &str, session_id: &str, ctx: &AppContext) -> Option<Value> {
    try_rewrite(
        command,
        Some(session_id),
        ctx,
        &AuthenticatedPrincipal::FirstParty,
    )
    .map(|response| response.data)
}

fn stable_hash_16(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn output(data: &Value) -> &str {
    data.get("output")
        .and_then(Value::as_str)
        .expect("rewrite output")
}

fn assert_rewritten(command: &str, ctx: &AppContext, tool: &str) -> Value {
    let data = rewrite(command, ctx).unwrap_or_else(|| panic!("{command} should rewrite"));
    assert!(
        output(&data).contains(&format!("Prefer `{tool}` tool over bash.")),
        "missing footer: {data:?}"
    );
    data
}

#[test]
fn rewrites_grep_and_rejects_pipes() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn Needle() {}\n").unwrap();
    filetime::set_file_mtime(
        dir.path().join("src"),
        filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
    )
    .unwrap();
    let ctx = context(dir.path(), true);

    let data = rewrite(
        &format!("grep -ni needle {}", dir.path().join("src").display()),
        &ctx,
    )
    .expect("grep should rewrite");
    // grep/rg use the enforced code-search footer, not the generic "Prefer" one.
    assert!(
        output(&data).contains("DO NOT search code by running grep/rg in bash"),
        "missing enforced grep footer: {data:?}"
    );
    assert_eq!(data["success"], Value::Null);
    assert!(output(&data).contains("Needle"));
    assert!(rewrite("grep needle src | wc -l", &ctx).is_none());
    assert!(rewrite("grep -x needle src", &ctx).is_none());
}

#[test]
fn grep_footer_steers_to_aft_search_when_registered() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn needle() {}\n").unwrap();
    filetime::set_file_mtime(
        dir.path(),
        filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
    )
    .unwrap();
    filetime::set_file_mtime(
        dir.path().join("src"),
        filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
    )
    .unwrap();
    let target = format!("grep -ni needle {}", dir.path().join("src").display());

    // Registered → footer names `aft_search`, not the grep tool.
    let registered = context_with_search(dir.path(), true);
    let data = rewrite(&target, &registered).expect("grep should rewrite");
    let out = output(&data);
    assert!(out.contains("Use the `aft_search` tool instead"), "{out}");
    assert!(!out.contains("Use the `grep` tool instead"), "{out}");

    // Not registered → footer falls back to the indexed grep tool.
    let not_registered = context_with_search(dir.path(), false);
    let data = rewrite(&target, &not_registered).expect("grep should rewrite");
    let out = output(&data);
    assert!(out.contains("Use the `grep` tool instead"), "{out}");
    assert!(!out.contains("Use the `aft_search` tool instead"), "{out}");
}

#[test]
fn grep_rewrite_rejects_oversized_regex_programs() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("notes.txt"), "needle\n").unwrap();
    let ctx = context(dir.path(), true);
    let pattern = "(a?){500000}";

    assert!(rewrite(&format!("grep '{pattern}' {}", dir.path().display()), &ctx).is_none());
}

#[test]
fn rewrites_rg_and_rejects_chains() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("notes.txt"), "alpha beta\n").unwrap();
    filetime::set_file_mtime(
        dir.path(),
        filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
    )
    .unwrap();
    let ctx = context(dir.path(), true);

    let data =
        rewrite(&format!("rg alpha {}", dir.path().display()), &ctx).expect("rg should rewrite");
    assert!(
        output(&data).contains("DO NOT search code by running grep/rg in bash"),
        "missing enforced rg footer: {data:?}"
    );
    assert!(output(&data).contains("alpha beta"));
    assert!(rewrite("rg alpha . && echo done", &ctx).is_none());
}

#[test]
fn rewrites_find_and_rejects_other_flags() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    let ctx = context(dir.path(), true);

    let data = assert_rewritten("find src -name '*.rs' -type f", &ctx, "glob");
    assert!(output(&data).contains("src/main.rs"));
    assert!(rewrite("find src -maxdepth 1 -name '*.rs'", &ctx).is_none());
}

#[test]
fn rewrites_cat_read_and_rejects_multiple_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    fs::write(dir.path().join("b.txt"), "world\n").unwrap();
    let ctx = context(dir.path(), true);

    let data = assert_rewritten(
        &format!("cat {}", dir.path().join("a.txt").display()),
        &ctx,
        "read",
    );
    assert!(output(&data).contains("1: hello"));
    assert!(rewrite("cat a.txt b.txt", &ctx).is_none());
}

#[test]
fn rewrites_cat_append_and_echo_append() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), true);

    let notes = dir.path().join("notes.txt");
    assert_rewritten(
        &format!("cat >> {} <<EOF\nfirst\nEOF", notes.display()),
        &ctx,
        "edit",
    );
    assert_rewritten(
        &format!("echo \"second line\" >> {}", notes.display()),
        &ctx,
        "edit",
    );
    assert_eq!(fs::read_to_string(notes).unwrap(), "first\nsecond line\n");
    assert!(rewrite("cat > notes.txt", &ctx).is_none());
}

#[test]
fn rewrite_append_uses_original_session_for_backups() {
    let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/aft-bash-rewrite-tests");
    fs::create_dir_all(&parent).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("case-")
        .tempdir_in(parent)
        .unwrap();
    let storage = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    fs::write(&file, "before\n").unwrap();
    let ctx = context(dir.path(), true);
    ctx.update_config(|config| {
        config.storage_dir = Some(storage.path().to_path_buf());
    });
    ctx.backup()
        .lock()
        .set_storage_dir(storage.path().to_path_buf(), 168);
    let session_id = "bash-rewrite-session";

    rewrite_with_session(
        &format!("echo scoped >> {}", file.display()),
        session_id,
        &ctx,
    )
    .expect("session rewrite succeeds");

    let session_file = storage
        .path()
        .join("backups")
        .join(stable_hash_16(session_id.as_bytes()))
        .join("session.json");
    let marker = fs::read_to_string(session_file).expect("session marker exists");
    assert!(marker.contains(session_id), "marker: {marker}");
}

#[test]
fn rewrites_sed_range_and_rejects_other_forms() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("lines.txt"), "one\ntwo\nthree\n").unwrap();
    let ctx = context(dir.path(), true);

    let data = assert_rewritten(
        &format!("sed -n '2,3p' {}", dir.path().join("lines.txt").display()),
        &ctx,
        "read",
    );
    assert!(output(&data).contains("2: two"));
    assert!(output(&data).contains("3: three"));
    assert!(rewrite("sed 's/two/TWO/' lines.txt", &ctx).is_none());
}

#[test]
fn rewrites_ls_directory_and_rejects_unknown_flags() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn lib() {}\n").unwrap();
    let ctx = context(dir.path(), true);

    // -a is fine; rewrite to read on the directory.
    let data = assert_rewritten(
        &format!("ls -a {}", dir.path().join("src").display()),
        &ctx,
        "read",
    );
    assert!(output(&data).contains("lib.rs"));
    assert!(rewrite("ls -h src", &ctx).is_none());
    assert!(rewrite("ls -A src", &ctx).is_none());
}

#[test]
fn binary_rewrite_matches_bash_for_double_quoted_backslashes() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let preserved_path = project.path().join(r"read\q.txt");
    let stripped_path = project.path().join("readq.txt");
    fs::write(&preserved_path, "PRESERVED_READ_TARGET\n").unwrap();
    fs::write(&stripped_path, "WRONG_READ_TARGET\n").unwrap();

    let quoted_path = project.path().join("a\"q.txt");
    fs::write(&quoted_path, "ESCAPED_QUOTE_TARGET\n").unwrap();

    let mut aft = AftProcess::spawn();
    let configured = aft.send(
        &json!({
            "id": "configure-double-quote-backslashes",
            "session_id": "double-quote-backslashes",
            "command": "configure",
            "harness": "runner",
            "project_root": project.path(),
            "storage_dir": storage.path(),
            "config": user_config(json!({
                "bash": { "rewrite": true },
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false,
            })),
        })
        .to_string(),
    );
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    let preserved_command = format!(r#"cat "{}""#, preserved_path.display());
    let native_preserved = std::process::Command::new("/bin/bash")
        .args(["-lc", &preserved_command])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        native_preserved.status.success(),
        "native cat failed: {native_preserved:?}"
    );
    let rewritten_preserved = aft.send(
        &json!({
            "id": "read-preserved-backslash",
            "session_id": "double-quote-backslashes",
            "command": "bash",
            "params": {
                "command": preserved_command,
                "workdir": project.path(),
                "compressed": false,
            },
        })
        .to_string(),
    );
    assert_eq!(
        rewritten_preserved["success"], true,
        "rewrite failed: {rewritten_preserved:?}"
    );
    let rewritten_stdout = format!(
        "{}\n",
        output(&rewritten_preserved)
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("1: "))
            .expect("numbered read output")
    );
    assert_eq!(rewritten_stdout.as_bytes(), native_preserved.stdout);
    assert!(!output(&rewritten_preserved).contains("WRONG_READ_TARGET"));

    let escaped_quote_command = format!(
        "cat \"{}\"",
        quoted_path.display().to_string().replace('"', "\\\"")
    );
    let native_escaped = std::process::Command::new("/bin/bash")
        .args(["-lc", &escaped_quote_command])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        native_escaped.status.success(),
        "native cat failed: {native_escaped:?}"
    );
    let rewritten_escaped = aft.send(
        &json!({
            "id": "read-escaped-quote",
            "session_id": "double-quote-backslashes",
            "command": "bash",
            "params": {
                "command": escaped_quote_command,
                "workdir": project.path(),
                "compressed": false,
            },
        })
        .to_string(),
    );
    assert_eq!(
        rewritten_escaped["success"], true,
        "rewrite failed: {rewritten_escaped:?}"
    );
    let rewritten_stdout = format!(
        "{}\n",
        output(&rewritten_escaped)
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("1: "))
            .expect("numbered read output")
    );
    assert_eq!(rewritten_stdout.as_bytes(), native_escaped.stdout);

    let append_path = project.path().join(r"append\q.txt");
    let wrong_append_path = project.path().join("appendq.txt");
    fs::write(&append_path, "before\n").unwrap();
    fs::write(&wrong_append_path, "wrong-before\n").unwrap();
    let append_command = format!(r#"echo appended >> "{}""#, append_path.display());
    let native_append = std::process::Command::new("/bin/bash")
        .args(["-lc", &append_command])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        native_append.status.success(),
        "native append failed: {native_append:?}"
    );
    let native_append_content = fs::read_to_string(&append_path).unwrap();
    fs::write(&append_path, "before\n").unwrap();

    let rewritten_append = aft.send(
        &json!({
            "id": "append-preserved-backslash",
            "session_id": "double-quote-backslashes",
            "command": "bash",
            "params": {
                "command": append_command,
                "workdir": project.path(),
                "compressed": false,
            },
        })
        .to_string(),
    );
    assert_eq!(
        rewritten_append["success"], true,
        "rewrite failed: {rewritten_append:?}"
    );
    assert_eq!(
        fs::read_to_string(&append_path).unwrap(),
        native_append_content
    );
    assert_eq!(
        fs::read_to_string(&wrong_append_path).unwrap(),
        "wrong-before\n"
    );
}

#[test]
fn binary_ls_rewrite_preserves_hidden_visibility_and_direct_read_default() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let tree = project.path().join("tree");
    fs::create_dir(&tree).unwrap();
    fs::write(tree.join("visible.txt"), "visible\n").unwrap();
    fs::write(tree.join(".secret"), "secret\n").unwrap();

    let mut aft = AftProcess::spawn();
    let configured = aft.send(
        &json!({
            "id": "configure-ls-hidden",
            "session_id": "ls-hidden-rewrite",
            "command": "configure",
            "harness": "runner",
            "project_root": project.path(),
            "storage_dir": storage.path(),
            "config": user_config(json!({
                "bash": { "rewrite": true },
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false,
            })),
        })
        .to_string(),
    );
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    let plain = aft.send(
        &json!({
            "id": "ls-hidden-plain",
            "session_id": "ls-hidden-rewrite",
            "command": "bash",
            "params": {
                "command": format!("ls {}", tree.display()),
                "workdir": project.path(),
                "compressed": false,
            },
        })
        .to_string(),
    );
    assert_eq!(
        plain["entries"],
        json!(["visible.txt"]),
        "plain ls: {plain:?}"
    );
    assert!(
        plain["output"]
            .as_str()
            .is_some_and(|value| value.contains("Prefer `read` tool over bash.")),
        "plain ls did not traverse the rewrite path: {plain:?}"
    );

    let native = std::process::Command::new("/bin/bash")
        .args(["-lc", "ls tree"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(native.status.success(), "native ls failed: {native:?}");
    assert_eq!(String::from_utf8(native.stdout).unwrap(), "visible.txt\n");

    let all = aft.send(
        &json!({
            "id": "ls-hidden-all",
            "session_id": "ls-hidden-rewrite",
            "command": "bash",
            "params": {
                "command": format!("ls -a {}", tree.display()),
                "workdir": project.path(),
                "compressed": false,
            },
        })
        .to_string(),
    );
    assert_eq!(
        all["entries"],
        json!([".secret", "visible.txt"]),
        "ls -a: {all:?}"
    );

    let direct = aft.send(
        &json!({
            "id": "read-hidden-direct",
            "session_id": "ls-hidden-rewrite",
            "command": "read",
            "file": tree,
        })
        .to_string(),
    );
    assert_eq!(
        direct["entries"],
        json!([".secret", "visible.txt"]),
        "direct read must retain its show-all default: {direct:?}"
    );
}

/// Regression for a real user report: `ls -l FILENAME` was being rewritten
/// to `read FILENAME`, swapping metadata (size, mtime, permissions) for
/// file contents — completely different semantics. The rewrite must
/// fall through to real bash whenever:
///   - any -l flag is present (user explicitly wants long-format metadata), OR
///   - the path resolves to a regular file (`ls FILE` echoes filename;
///     `read FILE` would dump contents).
#[test]
fn rejects_ls_with_l_flag() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn lib() {}\n").unwrap();
    fs::write(dir.path().join("README.md"), "# project\n").unwrap();
    let ctx = context(dir.path(), true);

    // -l on a directory: user wants metadata, not entries. Fall through.
    assert!(
        rewrite(&format!("ls -l {}", dir.path().join("src").display()), &ctx,).is_none(),
        "ls -l on a directory must fall through to bash (user wants metadata)"
    );

    // -l on a file: the original report. Must fall through.
    assert!(
        rewrite(
            &format!("ls -l {}", dir.path().join("README.md").display()),
            &ctx,
        )
        .is_none(),
        "ls -l on a file must fall through to bash (read would dump contents)"
    );

    // -la on a file: combined flags including -l. Fall through.
    assert!(
        rewrite(
            &format!("ls -la {}", dir.path().join("README.md").display()),
            &ctx,
        )
        .is_none(),
        "ls -la on a file must fall through (-l drops metadata, target is file)"
    );
}

/// `ls FILE` (no flags) and `read FILE` are NOT semantically equivalent:
/// `ls FILE` echoes the filename, `read FILE` dumps file contents. The
/// rewrite must check the path's file type and fall through for files.
#[test]
fn rejects_ls_on_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("README.md"), "# project\n").unwrap();
    let ctx = context(dir.path(), true);

    assert!(
        rewrite(
            &format!("ls {}", dir.path().join("README.md").display()),
            &ctx,
        )
        .is_none(),
        "ls on a regular file must fall through to bash"
    );
}

/// `ls NONEXISTENT` should fall through too — bash's "No such file or
/// directory" wording is well-known to agents, and we don't gain anything
/// by rewriting a guaranteed-failing call into a different failure shape.
#[test]
fn rejects_ls_on_missing_path() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), true);

    assert!(
        rewrite(
            &format!("ls {}", dir.path().join("does-not-exist").display()),
            &ctx,
        )
        .is_none(),
        "ls on a missing path must fall through to bash"
    );
}

#[test]
fn respects_experimental_flag() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let ctx = context(dir.path(), false);

    assert!(rewrite("cat a.txt", &ctx).is_none());
}

/// Regression: when the rewrite target tool refuses (e.g. read returns
/// `path_not_found` for a file outside project_root), dispatch must fall
/// through to the actual bash command — the agent's intent was bash, the
/// rewrite is a transparent optimization. Returning the read error would
/// surprise the agent because bash itself has no project_root restriction.
#[test]
fn rewrite_target_failure_logs_warning_before_fallthrough() {
    init_test_logger();

    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_path = outside.path().join("outside.txt");
    fs::write(&outside_path, "secret\n").unwrap();
    let ctx = context(dir.path(), true);

    assert!(
        rewrite(&format!("cat {}", outside_path.display()), &ctx).is_none(),
        "rewrite still falls through to bash when target tool refuses"
    );

    let logs = take_logs();
    let resolved = outside_path.canonicalize().unwrap();
    assert!(
        logs.iter().any(|line| {
            line.contains("bash rewrite rule cat declined")
                && line.contains("predicate=path_is_safe")
                && line.contains(&format!("resolved_candidate={resolved:?}"))
        }),
        "expected predicate-specific rewrite decline log, got {logs:?}"
    );
}

#[test]
fn rewrite_shape_decline_log_names_predicate_and_resolved_candidate() {
    init_test_logger();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized.txt");
    fs::write(&path, vec![b'x'; 50 * 1024 + 1]).unwrap();
    let ctx = context(dir.path(), true);

    assert!(
        rewrite("cat oversized.txt", &ctx).is_none(),
        "oversized reads must remain native bash commands"
    );

    let logs = take_logs();
    let resolved = path.canonicalize().unwrap();
    assert!(
        logs.iter().any(|line| {
            line.contains("bash rewrite rule cat declined")
                && line.contains("predicate=read_shape_is_faithful")
                && line.contains(&format!("resolved_candidate={resolved:?}"))
        }),
        "expected predicate-specific shape decline log, got {logs:?}"
    );
}

#[test]
fn rewrite_target_failure_falls_through_to_bash() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("outside.txt"), "secret\n").unwrap();
    let ctx = context(dir.path(), true);

    // cat a path outside project_root → read refuses → rewrite must NOT swallow
    // the error. try_rewrite returns None so the bash handler runs the actual
    // cat command.
    let outside_path = outside.path().join("outside.txt");
    assert!(
        rewrite(&format!("cat {}", outside_path.display()), &ctx).is_none(),
        "rewrite must fall through when read refuses outside-project paths"
    );

    // sed with the same outside path → read refuses → fall through.
    assert!(
        rewrite(&format!("sed -n '1,1p' {}", outside_path.display()), &ctx).is_none(),
        "sed→read fallthrough must apply for outside-project paths"
    );

    // ls of a directory outside project_root → read refuses → fall through.
    assert!(
        rewrite(&format!("ls {}", outside.path().display()), &ctx).is_none(),
        "ls→read fallthrough must apply for outside-project directories"
    );

    // grep against an outside path → grep refuses → fall through.
    assert!(
        rewrite(
            &format!("grep -n secret {}", outside.path().display()),
            &ctx
        )
        .is_none(),
        "grep fallthrough must apply for outside-project paths"
    );

    // Sanity: in-project rewrites still succeed (the helper isn't over-falling-through).
    fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    assert_rewritten(
        &format!("cat {}", dir.path().join("a.txt").display()),
        &ctx,
        "read",
    );
}

#[test]
fn parser_handles_quotes_escapes_heredocs_and_rejects_expansion() {
    let parsed = parser::parse("grep 'two words' \"src dir\"").expect("quoted parse");
    assert_eq!(parsed.args, vec!["grep", "two words", "src dir"]);

    let parsed = parser::parse(r"cat file\ name.txt").expect("escaped parse");
    assert_eq!(parsed.args, vec!["cat", "file name.txt"]);

    let parsed = parser::parse("cat >> out.txt <<EOF\nhello\nEOF").expect("heredoc parse");
    assert_eq!(parsed.args, vec!["cat"]);
    assert_eq!(parsed.appends_to.as_deref(), Some("out.txt"));
    assert_eq!(parsed.heredoc.as_deref(), Some("hello\n"));

    assert!(parser::parse("cat $(pwd)").is_none());
    assert!(parser::parse("cat `pwd`").is_none());
    assert!(parser::parse("echo $HOME").is_none());
}

#[test]
fn edit_append_op_appends_creates_and_reports_invalid_paths() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), false);
    let existing = dir.path().join("existing.txt");
    fs::write(&existing, "before\n").unwrap();

    let response = handle_edit_match(
        &request(
            "edit_match",
            json!({"op": "append", "file": existing.display().to_string(), "appendContent": "after\n"}),
        ),
        &ctx,
    );
    assert!(response.success, "append should succeed: {response:?}");
    assert_eq!(fs::read_to_string(&existing).unwrap(), "before\nafter\n");

    let response = handle_edit_match(
        &request(
            "edit_match",
            json!({"op": "append", "file": dir.path().join("new.txt").display().to_string(), "appendContent": "created\n"}),
        ),
        &ctx,
    );
    assert!(
        response.success,
        "create append should succeed: {response:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("new.txt")).unwrap(),
        "created\n"
    );

    let response = handle_edit_match(
        &request(
            "edit_match",
            json!({"op": "append", "file": dir.path().join("missing/child.txt").display().to_string(), "appendContent": "nope", "createDirs": false}),
        ),
        &ctx,
    );
    assert!(!response.success, "invalid path should fail: {response:?}");
}
