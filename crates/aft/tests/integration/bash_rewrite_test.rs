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
use std::sync::{Mutex, Once, OnceLock};

use aft::bash_rewrite::{parser, try_rewrite};
use aft::commands::edit_match::handle_edit_match;
use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use log::{Level, LevelFilter, Log, Metadata, Record};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

static TEST_LOGS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static LOGGER_INIT: Once = Once::new();

struct TestLogger;

impl Log for TestLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Warn
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            TEST_LOGS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .expect("lock test logs")
                .push(format!("{}", record.args()));
        }
    }

    fn flush(&self) {}
}

fn init_test_logger() {
    LOGGER_INIT.call_once(|| {
        log::set_boxed_logger(Box::new(TestLogger)).expect("install test logger");
        log::set_max_level(LevelFilter::Warn);
    });
    TEST_LOGS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("lock test logs")
        .clear();
}

fn take_logs() -> Vec<String> {
    std::mem::take(
        &mut *TEST_LOGS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .expect("lock test logs"),
    )
}

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
    try_rewrite(command, None, ctx).map(|response| response.data)
}

fn rewrite_with_session(command: &str, session_id: &str, ctx: &AppContext) -> Option<Value> {
    try_rewrite(command, Some(session_id), ctx).map(|response| response.data)
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
    let ctx = context(dir.path(), true);

    let data = assert_rewritten(
        &format!("grep -ni needle {}", dir.path().join("src").display()),
        &ctx,
        "grep",
    );
    assert_eq!(data["success"], Value::Null);
    assert!(output(&data).contains("Needle"));
    assert!(rewrite("grep needle src | wc -l", &ctx).is_none());
    assert!(rewrite("grep -x needle src", &ctx).is_none());
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
    let ctx = context(dir.path(), true);

    let data = assert_rewritten(&format!("rg alpha {}", dir.path().display()), &ctx, "grep");
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
    let dir = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    fs::write(&file, "before\n").unwrap();
    let ctx = context(dir.path(), true);
    ctx.config_mut().storage_dir = Some(storage.path().to_path_buf());
    ctx.backup()
        .borrow_mut()
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
    assert!(
        logs.iter().any(|line| {
            line.contains("bash rewrite rule cat declined")
                && line.contains("read declined")
                && line.contains("outside the project root")
        }),
        "expected warn-level rewrite decline log, got {logs:?}"
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
