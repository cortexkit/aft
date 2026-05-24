//! Integration tests for `ast_search` and `ast_replace` through the binary protocol.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use super::helpers::AftProcess;

fn setup_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("create temp dir");

    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directories");
        }
        fs::write(path, content).expect("write fixture file");
    }

    temp_dir
}

fn configure(aft: &mut AftProcess, root: &Path) {
    let resp = aft.configure(root);
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn read_file(root: &Path, relative_path: &str) -> String {
    fs::read_to_string(root.join(relative_path)).expect("read file")
}

fn count_occurrences(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

fn file_result<'a>(resp: &'a Value, suffix: &str) -> &'a Value {
    // Windows reports paths with backslashes (`src\one.ts`); normalize for
    // suffix matching so the test stays platform-agnostic.
    resp["files"]
        .as_array()
        .expect("files array")
        .iter()
        .find(|entry| {
            let path = entry["file"].as_str().expect("file path");
            let normalized = path.replace('\\', "/");
            normalized.ends_with(suffix)
        })
        .unwrap_or_else(|| panic!("missing file result for suffix {suffix}: {resp:?}"))
}

#[test]
fn ast_replace_replaces_every_match_in_single_typescript_file() {
    let project = setup_project(&[(
        "sample.ts",
        "console.log(first);\nif (ready) {\n  console.log(second);\n}\nconsole.log(third);\n",
    )]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search = send(
        &mut aft,
        json!({
            "id": "search-single",
            "command": "ast_search",
            "pattern": "console.log($ARG)",
            "lang": "typescript",
        }),
    );

    assert_eq!(
        search["success"], true,
        "ast_search should succeed: {search:?}"
    );
    assert_eq!(search["total_matches"], 3);
    assert_eq!(search["files_with_matches"], 1);

    let matched_args: Vec<&str> = search["matches"]
        .as_array()
        .expect("matches array")
        .iter()
        .map(|m| m["meta_variables"]["$ARG"].as_str().expect("captured arg"))
        .collect();
    assert_eq!(matched_args, vec!["first", "second", "third"]);

    let replace = send(
        &mut aft,
        json!({
            "id": "replace-single",
            "command": "ast_replace",
            "pattern": "console.log($ARG)",
            "rewrite": "logger.info($ARG)",
            "lang": "typescript",
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "ast_replace should succeed: {replace:?}"
    );
    assert_eq!(replace["dry_run"], false);
    assert_eq!(replace["total_replacements"], 3);
    assert_eq!(replace["total_files"], 1);
    assert_eq!(replace["files_with_matches"], 1);

    let file_entry = &replace["files"].as_array().expect("files array")[0];
    assert_eq!(file_entry["replacements"], 3);
    assert!(file_entry["backup_id"].as_str().is_some());

    let updated = read_file(project.path(), "sample.ts");
    assert_eq!(count_occurrences(&updated, "logger.info("), 3);
    assert_eq!(count_occurrences(&updated, "console.log("), 0);
    assert!(updated.contains("logger.info(first);"));
    assert!(updated.contains("logger.info(second);"));
    assert!(updated.contains("logger.info(third);"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_replace_replaces_all_matches_across_multiple_files() {
    let project = setup_project(&[
        ("src/one.ts", "console.log(alpha);\n"),
        (
            "src/two.ts",
            "console.log(beta);\nconsole.log(gamma);\nconst untouched = 1;\n",
        ),
        ("src/three.ts", "const nothing_to_replace = true;\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let replace = send(
        &mut aft,
        json!({
            "id": "replace-multi-file",
            "command": "ast_replace",
            "pattern": "console.log($ARG)",
            "rewrite": "logger.info($ARG)",
            "lang": "typescript",
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "ast_replace should succeed: {replace:?}"
    );
    assert_eq!(replace["total_replacements"], 3);
    assert_eq!(replace["total_files"], 2);
    assert_eq!(replace["files_with_matches"], 2);
    assert_eq!(replace["files_searched"], 3);

    assert_eq!(file_result(&replace, "src/one.ts")["replacements"], 1);
    assert_eq!(file_result(&replace, "src/two.ts")["replacements"], 2);

    let one = read_file(project.path(), "src/one.ts");
    let two = read_file(project.path(), "src/two.ts");
    let three = read_file(project.path(), "src/three.ts");

    assert_eq!(count_occurrences(&one, "logger.info("), 1);
    assert_eq!(count_occurrences(&two, "logger.info("), 2);
    assert_eq!(count_occurrences(&one, "console.log("), 0);
    assert_eq!(count_occurrences(&two, "console.log("), 0);
    assert_eq!(three, "const nothing_to_replace = true;\n");

    let actual_replacements =
        count_occurrences(&one, "logger.info(") + count_occurrences(&two, "logger.info(");
    assert_eq!(
        actual_replacements,
        replace["total_replacements"].as_u64().unwrap() as usize
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_replace_operation_undo_restores_all_touched_files() {
    let project = setup_project(&[
        ("src/one.ts", "console.log(alpha);\n"),
        ("src/two.ts", "console.log(beta);\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let replace = send(
        &mut aft,
        json!({
            "id": "replace-before-operation-undo",
            "command": "ast_replace",
            "pattern": "console.log($ARG)",
            "rewrite": "logger.info($ARG)",
            "lang": "typescript",
            "dry_run": false,
        }),
    );
    assert_eq!(replace["success"], true, "replace: {replace:?}");
    assert_eq!(
        read_file(project.path(), "src/one.ts"),
        "logger.info(alpha);\n"
    );
    assert_eq!(
        read_file(project.path(), "src/two.ts"),
        "logger.info(beta);\n"
    );

    let undo = send(
        &mut aft,
        json!({
            "id": "undo-ast-operation",
            "command": "undo",
        }),
    );
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(undo["operation"], true);
    assert_eq!(undo["restored_count"], 2);
    assert_eq!(
        read_file(project.path(), "src/one.ts"),
        "console.log(alpha);\n"
    );
    assert_eq!(
        read_file(project.path(), "src/two.ts"),
        "console.log(beta);\n"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn ast_replace_unwritable_target_fails_without_partial_write() {
    use std::os::unix::fs::PermissionsExt;

    let project = setup_project(&[
        ("src/a.ts", "console.log(alpha);\n"),
        ("src/z.ts", "console.log(beta);\n"),
    ]);
    let read_only = project.path().join("src/z.ts");
    let original_a = read_file(project.path(), "src/a.ts");
    let original_z = read_file(project.path(), "src/z.ts");

    let mut perms = fs::metadata(&read_only).unwrap().permissions();
    perms.set_mode(0o444);
    fs::set_permissions(&read_only, perms).unwrap();

    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let replace = send(
        &mut aft,
        json!({
            "id": "replace-unwritable-target",
            "command": "ast_replace",
            "pattern": "console.log($ARG)",
            "rewrite": "logger.info($ARG)",
            "lang": "typescript",
            "dry_run": false,
        }),
    );

    let mut reset_perms = fs::metadata(&read_only).unwrap().permissions();
    reset_perms.set_mode(0o644);
    fs::set_permissions(&read_only, reset_perms).unwrap();

    assert_eq!(
        replace["success"], false,
        "replace should fail: {replace:?}"
    );
    assert_eq!(replace["code"], "io_error");
    assert_eq!(
        replace["rolled_back"], true,
        "rollback should be reported: {replace:?}"
    );
    assert_eq!(read_file(project.path(), "src/a.ts"), original_a);
    assert_eq!(read_file(project.path(), "src/z.ts"), original_z);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_replace_dry_run_reports_counts_without_writing_files() {
    let original = "console.log(first);\nconsole.log(second);\n";
    let project = setup_project(&[("sample.ts", original)]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let replace = send(
        &mut aft,
        json!({
            "id": "replace-dry-run",
            "command": "ast_replace",
            "pattern": "console.log($ARG)",
            "rewrite": "logger.info($ARG)",
            "lang": "typescript",
            "dry_run": true,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "dry-run replace should succeed: {replace:?}"
    );
    assert_eq!(replace["dry_run"], true);
    assert_eq!(replace["total_replacements"], 2);
    assert_eq!(replace["total_files"], 1);

    let file_entry = &replace["files"].as_array().expect("files array")[0];
    assert_eq!(file_entry["replacements"], 2);
    let diff = file_entry["diff"].as_str().expect("diff string");
    assert!(diff.contains("-console.log(first);"));
    assert!(diff.contains("-console.log(second);"));
    assert!(diff.contains("+logger.info(first);"));
    assert!(diff.contains("+logger.info(second);"));

    let on_disk = read_file(project.path(), "sample.ts");
    assert_eq!(on_disk, original, "dry-run must not modify files on disk");
    assert_eq!(count_occurrences(&on_disk, "console.log("), 2);
    assert_eq!(count_occurrences(&on_disk, "logger.info("), 0);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_and_replace_support_meta_variables_and_preserve_captures() {
    let project = setup_project(&[(
        "transform.ts",
        "function greet(name, punctuation) {\n  const message = name + punctuation;\n  return message;\n}\n",
    )]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search = send(
        &mut aft,
        json!({
            "id": "meta-search",
            "command": "ast_search",
            "pattern": "function $NAME($$$PARAMS) { $$$BODY }",
            "lang": "typescript",
        }),
    );

    assert_eq!(
        search["success"], true,
        "meta ast_search should succeed: {search:?}"
    );
    assert_eq!(search["total_matches"], 1);

    let first_match = &search["matches"].as_array().expect("matches array")[0];
    assert_eq!(first_match["meta_variables"]["$NAME"], "greet");

    let params = first_match["meta_variables"]["$PARAMS"]
        .as_array()
        .expect("params array");
    assert!(params.contains(&Value::String("name".to_string())));
    assert!(params.contains(&Value::String("punctuation".to_string())));

    let body = first_match["meta_variables"]["$BODY"]
        .as_array()
        .expect("body array");
    assert_eq!(body.len(), 2);

    let replace = send(
        &mut aft,
        json!({
            "id": "meta-replace",
            "command": "ast_replace",
            "pattern": "function $NAME($$$PARAMS) { $$$BODY }",
            "rewrite": "const $NAME = ($$$PARAMS) => { $$$BODY }",
            "lang": "typescript",
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "meta ast_replace should succeed: {replace:?}"
    );
    assert_eq!(replace["total_replacements"], 1);

    let updated = read_file(project.path(), "transform.ts");
    assert!(updated.contains("const greet = (name, punctuation) =>"));
    assert!(updated.contains("const message = name + punctuation;"));
    assert!(updated.contains("return message;"));
    assert!(!updated.contains("function greet"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_replace_rejects_invalid_partial_patterns_without_crashing() {
    let original = "try { doWork(); } catch (err) { console.error(err); } finally { cleanup(); }\n";
    let project = setup_project(&[("sample.ts", original)]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    for (id, pattern) in [
        ("invalid-catch", "catch ($ERR) { $$$ }"),
        ("invalid-finally", "finally { $$$ }"),
    ] {
        let resp = send(
            &mut aft,
            json!({
                "id": id,
                "command": "ast_replace",
                "pattern": pattern,
                "rewrite": "noop()",
                "lang": "typescript",
                "dry_run": true,
            }),
        );

        assert_eq!(
            resp["success"], false,
            "invalid pattern should fail: {resp:?}"
        );
        assert_eq!(resp["code"], "invalid_pattern");
        assert!(resp["message"]
            .as_str()
            .expect("error message")
            .contains("Patterns must be complete AST nodes."));
    }

    let alive = aft.send(r#"{"id":"alive","command":"ping"}"#);
    assert_eq!(
        alive["success"], true,
        "process should stay alive after invalid patterns"
    );
    assert_eq!(read_file(project.path(), "sample.ts"), original);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_rejects_invalid_partial_patterns_without_returning_empty_matches() {
    let original = "try { doWork(); } catch (err) { console.error(err); }\n";
    let project = setup_project(&[("sample.ts", original)]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let resp = send(
        &mut aft,
        json!({
            "id": "invalid-ast-search",
            "command": "ast_search",
            "pattern": "catch ($ERR) { $$$ }",
            "lang": "typescript",
        }),
    );

    assert_eq!(
        resp["success"], false,
        "invalid ast_search pattern should fail: {resp:?}"
    );
    assert_eq!(resp["code"], "invalid_pattern");
    assert!(resp["message"]
        .as_str()
        .expect("message")
        .contains("invalid AST pattern"));

    let alive = aft.send(r#"{"id":"alive-after-ast-search","command":"ping"}"#);
    assert_eq!(alive["success"], true);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_and_replace_report_empty_results_for_valid_patterns() {
    let original = "const value = compute();\n";
    let project = setup_project(&[("sample.ts", original)]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search = send(
        &mut aft,
        json!({
            "id": "empty-search",
            "command": "ast_search",
            "pattern": "console.log($ARG)",
            "lang": "typescript",
        }),
    );

    assert_eq!(
        search["success"], true,
        "empty ast_search should succeed: {search:?}"
    );
    assert_eq!(
        search["matches"].as_array().expect("matches array").len(),
        0
    );
    assert_eq!(search["total_matches"], 0);
    assert_eq!(search["files_with_matches"], 0);
    assert_eq!(search["files_searched"], 1);
    assert_eq!(search["no_files_matched_scope"], false);
    assert_eq!(
        search["scope_warnings"]
            .as_array()
            .expect("scope warnings")
            .len(),
        0
    );

    let replace = send(
        &mut aft,
        json!({
            "id": "empty-replace",
            "command": "ast_replace",
            "pattern": "console.log($ARG)",
            "rewrite": "logger.info($ARG)",
            "lang": "typescript",
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "empty ast_replace should succeed: {replace:?}"
    );
    assert_eq!(replace["total_replacements"], 0);
    assert_eq!(replace["total_files"], 0);
    assert_eq!(replace["files_with_matches"], 0);
    assert_eq!(replace["files_searched"], 1);
    assert_eq!(replace["no_files_matched_scope"], false);
    assert_eq!(
        replace["scope_warnings"]
            .as_array()
            .expect("scope warnings")
            .len(),
        0
    );
    assert_eq!(replace["files"].as_array().expect("files array").len(), 0);
    assert_eq!(read_file(project.path(), "sample.ts"), original);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_and_replace_reject_nonexistent_paths() {
    let project = setup_project(&[("sample.ts", "console.log(value);\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let missing_absolute = project.path().join("missing.ts");
    let search = send(
        &mut aft,
        json!({
            "id": "missing-absolute-search",
            "command": "ast_search",
            "pattern": "console.log($ARG)",
            "lang": "typescript",
            "paths": [missing_absolute.display().to_string()],
        }),
    );

    assert_eq!(
        search["success"], false,
        "missing search path should fail: {search:?}"
    );
    assert_eq!(search["code"], "path_not_found");
    assert!(search["message"]
        .as_str()
        .expect("search error message")
        .contains(&missing_absolute.display().to_string()));

    let replace = send(
        &mut aft,
        json!({
            "id": "missing-relative-replace",
            "command": "ast_replace",
            "pattern": "console.log($ARG)",
            "rewrite": "logger.info($ARG)",
            "lang": "typescript",
            "paths": ["does-not-exist"],
            "dry_run": true,
        }),
    );

    assert_eq!(
        replace["success"], false,
        "missing replace path should fail: {replace:?}"
    );
    assert_eq!(replace["code"], "path_not_found");
    assert!(replace["message"]
        .as_str()
        .expect("replace error message")
        .contains("does-not-exist"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_and_replace_report_globs_matching_no_files() {
    let original = "console.log(value);\n";
    let project = setup_project(&[("src/sample.ts", original)]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search = send(
        &mut aft,
        json!({
            "id": "empty-glob-search",
            "command": "ast_search",
            "pattern": "console.log($ARG)",
            "lang": "typescript",
            "paths": ["src"],
            "globs": ["*.go"],
        }),
    );

    assert_eq!(
        search["success"], true,
        "empty glob search should succeed: {search:?}"
    );
    assert_eq!(
        search["matches"].as_array().expect("matches array").len(),
        0
    );
    assert_eq!(search["total_matches"], 0);
    assert_eq!(search["files_with_matches"], 0);
    assert_eq!(search["files_searched"], 0);
    assert_eq!(search["no_files_matched_scope"], true);
    assert_eq!(
        search["scope_warnings"].as_array().expect("scope warnings"),
        &vec![Value::String("*.go → no files".to_string())]
    );

    let replace = send(
        &mut aft,
        json!({
            "id": "empty-glob-replace",
            "command": "ast_replace",
            "pattern": "console.log($ARG)",
            "rewrite": "logger.info($ARG)",
            "lang": "typescript",
            "paths": ["src"],
            "globs": ["*.go"],
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "empty glob replace should succeed: {replace:?}"
    );
    assert_eq!(replace["files"].as_array().expect("files array").len(), 0);
    assert_eq!(replace["total_replacements"], 0);
    assert_eq!(replace["total_files"], 0);
    assert_eq!(replace["files_with_matches"], 0);
    assert_eq!(replace["files_searched"], 0);
    assert_eq!(replace["no_files_matched_scope"], true);
    assert_eq!(
        replace["scope_warnings"]
            .as_array()
            .expect("scope warnings"),
        &vec![Value::String("*.go → no files".to_string())]
    );
    assert_eq!(read_file(project.path(), "src/sample.ts"), original);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_and_replace_support_python_patterns() {
    let project = setup_project(&[("sample.py", "print(alpha)\nprint(beta)\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search = send(
        &mut aft,
        json!({
            "id": "python-search",
            "command": "ast_search",
            "pattern": "print($ARG)",
            "lang": "python",
        }),
    );

    assert_eq!(
        search["success"], true,
        "python ast_search should succeed: {search:?}"
    );
    assert_eq!(search["total_matches"], 2);
    let args: Vec<&str> = search["matches"]
        .as_array()
        .expect("matches array")
        .iter()
        .map(|m| m["meta_variables"]["$ARG"].as_str().expect("python arg"))
        .collect();
    assert_eq!(args, vec!["alpha", "beta"]);

    let replace = send(
        &mut aft,
        json!({
            "id": "python-replace",
            "command": "ast_replace",
            "pattern": "print($ARG)",
            "rewrite": "logger.info($ARG)",
            "lang": "python",
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "python ast_replace should succeed: {replace:?}"
    );
    assert_eq!(replace["total_replacements"], 2);
    assert_eq!(replace["files_with_matches"], 1);

    let updated = read_file(project.path(), "sample.py");
    assert_eq!(count_occurrences(&updated, "logger.info("), 2);
    assert_eq!(count_occurrences(&updated, "print("), 0);
    assert!(updated.contains("logger.info(alpha)"));
    assert!(updated.contains("logger.info(beta)"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_and_replace_support_c_patterns() {
    let project = setup_project(&[(
        "sample.c",
        "int add(int left, int right) {\n    return left + right;\n}\n",
    )]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search = send(
        &mut aft,
        json!({
            "id": "c-search",
            "command": "ast_search",
            "pattern": "int $NAME($$$PARAMS) { $$$BODY }",
            "lang": "c",
        }),
    );

    assert_eq!(
        search["success"], true,
        "c ast_search should succeed: {search:?}"
    );
    assert_eq!(search["total_matches"], 1);
    assert_eq!(search["matches"][0]["meta_variables"]["$NAME"], "add");

    let replace = send(
        &mut aft,
        json!({
            "id": "c-replace",
            "command": "ast_replace",
            "pattern": "int $NAME($$$PARAMS) { $$$BODY }",
            "rewrite": "long $NAME($$$PARAMS) { $$$BODY }",
            "lang": "c",
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "c ast_replace should succeed: {replace:?}"
    );
    assert_eq!(replace["total_replacements"], 1);

    let updated = read_file(project.path(), "sample.c");
    assert!(updated.contains("long add(int left, int right)"));
    assert!(!updated.contains("int add(int left, int right)"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_and_replace_support_cpp_patterns() {
    let project = setup_project(&[("sample.cpp", "int measure() {\n    return 42;\n}\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search = send(
        &mut aft,
        json!({
            "id": "cpp-search",
            "command": "ast_search",
            "pattern": "int $NAME() { return 42; }",
            "lang": "cpp",
        }),
    );

    assert_eq!(
        search["success"], true,
        "cpp ast_search should succeed: {search:?}"
    );
    assert_eq!(search["total_matches"], 1);
    assert_eq!(search["matches"][0]["meta_variables"]["$NAME"], "measure");

    let replace = send(
        &mut aft,
        json!({
            "id": "cpp-replace",
            "command": "ast_replace",
            "pattern": "int $NAME() { return 42; }",
            "rewrite": "long $NAME() { return 42; }",
            "lang": "cpp",
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "cpp ast_replace should succeed: {replace:?}"
    );
    assert_eq!(replace["total_replacements"], 1);

    let updated = read_file(project.path(), "sample.cpp");
    assert!(updated.contains("long measure()"));
    assert!(!updated.contains("int measure()"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_and_replace_support_zig_patterns() {
    let project = setup_project(&[(
        "sample.zig",
        "const answer = 41;\n\nfn greet(name: []const u8) void {\n    _ = name;\n}\n",
    )]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search = send(
        &mut aft,
        json!({
            "id": "zig-search",
            "command": "ast_search",
            "pattern": "fn greet(name: []const u8) void { $$$ }",
            "lang": "zig",
        }),
    );

    assert_eq!(
        search["success"], true,
        "zig ast_search should succeed: {search:?}"
    );
    assert_eq!(search["total_matches"], 1);
    assert!(search["matches"][0]["text"]
        .as_str()
        .expect("zig match text")
        .contains("fn greet(name: []const u8) void"));

    let replace = send(
        &mut aft,
        json!({
            "id": "zig-replace",
            "command": "ast_replace",
            "pattern": "fn greet(name: []const u8) void { _ = name; }",
            "rewrite": "pub fn greet(name: []const u8) void { _ = name; }",
            "lang": "zig",
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "zig ast_replace should succeed: {replace:?}"
    );
    assert_eq!(replace["total_replacements"], 1);

    let updated = read_file(project.path(), "sample.zig");
    assert!(updated.contains("pub fn greet(name: []const u8) void"));
    assert!(!updated.contains("\nfn greet(name: []const u8) void"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_and_replace_support_csharp_patterns() {
    let project = setup_project(&[(
        "Sample.cs",
        "public class Worker\n{\n    private int count = 1;\n\n    public void Run()\n    {\n    }\n}\n",
    )]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search = send(
        &mut aft,
        json!({
            "id": "csharp-search",
            "command": "ast_search",
            "pattern": "public class $NAME { $$$BODY }",
            "lang": "csharp",
        }),
    );

    assert_eq!(
        search["success"], true,
        "csharp ast_search should succeed: {search:?}"
    );
    assert_eq!(search["total_matches"], 1);
    assert_eq!(search["matches"][0]["meta_variables"]["$NAME"], "Worker");

    let replace = send(
        &mut aft,
        json!({
            "id": "csharp-replace",
            "command": "ast_replace",
            "pattern": "public class $NAME { $$$BODY }",
            "rewrite": "public sealed class $NAME { $$$BODY }",
            "lang": "csharp",
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "csharp ast_replace should succeed: {replace:?}"
    );
    assert_eq!(replace["total_replacements"], 1);

    let updated = read_file(project.path(), "Sample.cs");
    assert!(updated.contains("public sealed class Worker"));
    assert!(!updated.contains("public class Worker"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_and_replace_support_solidity_patterns() {
    // Solidity grammar quirk: tree-sitter-solidity parses `[a-zA-Z$_]` as
    // valid identifier characters, so meta-vars stay as `$NAME` (no µ
    // expansion). Patterns that work reliably are full function-declaration
    // shapes; statement-only patterns (`require($COND, $MSG);`) match zero
    // because the parser binds them at the wrong AST node level.
    //
    // The test uses a function-declaration pattern + a structural rewrite
    // that's representative of real Solidity migration work (adding
    // visibility qualifiers / modifiers like `virtual` or `nonReentrant`).
    let project = setup_project(&[(
        "contracts/Counter.sol",
        "// SPDX-License-Identifier: MIT\n\
         pragma solidity ^0.8.20;\n\
         \n\
         contract Counter {\n\
             uint256 public count;\n\
         \n\
             function increment() public {\n\
                 count += 1;\n\
             }\n\
         \n\
             function decrement() public {\n\
                 count -= 1;\n\
             }\n\
         }\n",
    )]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    // Search both `public` functions in one shot using a meta-var name + a
    // variadic body. This is the canonical "find all functions of shape X"
    // query agents reach for.
    let search = send(
        &mut aft,
        json!({
            "id": "solidity-search",
            "command": "ast_search",
            "pattern": "function $NAME() public { $$$BODY }",
            "lang": "solidity",
        }),
    );

    assert_eq!(
        search["success"], true,
        "solidity ast_search should succeed: {search:?}"
    );
    assert_eq!(
        search["total_matches"], 2,
        "expected 2 public functions: {search:?}"
    );

    // Capture verification — meta-vars must propagate through the Solidity
    // grammar like every other language. This is the regression guard for
    // the v0.19.5 expando_char fix: before the fix, `$NAME` was rewritten
    // to `µNAME` for Solidity, but `µ` is not in the Solidity identifier
    // character set, so meta-vars never bound and total_matches was 0.
    let names: Vec<&str> = search["matches"]
        .as_array()
        .expect("matches array")
        .iter()
        .map(|m| {
            m["meta_variables"]["$NAME"]
                .as_str()
                .expect("captured $NAME")
        })
        .collect();
    assert!(
        names.contains(&"increment"),
        "captured names should include `increment`: {names:?}"
    );
    assert!(
        names.contains(&"decrement"),
        "captured names should include `decrement`: {names:?}"
    );

    // Replace path: add a modifier to a specific function. Real-world:
    // agents bulk-add `virtual`, `nonReentrant`, `whenNotPaused`, etc.
    // We use a literal pattern + rewrite to keep the rewrite template
    // simple while still proving the replace pipeline produces valid
    // Solidity output.
    let replace = send(
        &mut aft,
        json!({
            "id": "solidity-replace",
            "command": "ast_replace",
            "pattern": "function increment() public { count += 1; }",
            "rewrite": "function increment() public virtual { count += 1; }",
            "lang": "solidity",
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "solidity ast_replace should succeed: {replace:?}"
    );
    assert_eq!(replace["total_replacements"], 1);

    let updated = read_file(project.path(), "contracts/Counter.sol");
    assert!(
        updated.contains("function increment() public virtual { count += 1; }"),
        "rewrite should add virtual modifier: {updated}"
    );
    // Other functions must be untouched.
    assert!(
        updated.contains("function decrement() public {"),
        "decrement should be unchanged: {updated}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

/// Regression guard for the v0.18 perf fix.
///
/// Pre-fix, `ast_search` over ~150 files took ~23 seconds because
/// `find_all(&str)` reparsed the pattern via tree-sitter on every file and
/// the file loop was strictly serial. Post-fix (precompiled `Pattern` +
/// rayon parallel iter), the same query completes in ~1 second.
///
/// We test against a 60-file synthetic Rust crate with a meta-variable
/// member-access pattern (the worst-case shape that originally triggered
/// the bug). Threshold of 5 seconds gives generous margin for slow CI
/// runners while still catching ~5x regressions.
#[test]
fn ast_search_member_access_pattern_completes_in_reasonable_time() {
    let mut files: Vec<(String, String)> = Vec::new();
    let body = r#"
        pub fn handle(ctx: &Ctx, req: &Request) -> Response {
            let cfg = ctx.config();
            if cfg.search_index {
                run_indexed(req)
            } else {
                run_direct(req)
            }
        }

        struct Ctx;
        struct Request;
        struct Response;
        impl Ctx {
            fn config(&self) -> Config { Config { search_index: true } }
        }
        struct Config { search_index: bool }
    "#;

    for i in 0..60 {
        files.push((format!("src/file_{i:03}.rs"), body.to_string()));
    }

    let owned: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let project = setup_project(&owned);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let start = std::time::Instant::now();
    let resp = send(
        &mut aft,
        json!({
            "id": "perf-search",
            "method": "ast_search",
            "pattern": "$X.search_index",
            "lang": "rust",
            "paths": ["src"],
            "globs": ["*.rs"],
            "context_lines": 2,
        }),
    );
    let elapsed = start.elapsed();

    assert_eq!(resp["success"], true, "ast_search should succeed: {resp:?}");
    assert_eq!(resp["files_searched"], 60);
    assert_eq!(resp["files_with_matches"], 60);
    // Each file has 1 occurrence of the field-access pattern.
    assert_eq!(resp["total_matches"], 60);

    assert!(
        elapsed.as_secs_f64() < 5.0,
        "ast_search regressed: {} files took {:.2}s (expected < 5s). \
         If this fails, check that ast_search.rs still pre-compiles the pattern \
         outside the file loop and uses rayon par_iter for the file walk.",
        resp["files_searched"],
        elapsed.as_secs_f64()
    );

    let status = aft.shutdown();
    assert!(status.success());
}

/// Regression guard for the same perf bug in `ast_replace`.
#[test]
fn ast_replace_member_access_pattern_completes_in_reasonable_time() {
    let mut files: Vec<(String, String)> = Vec::new();
    let body = r#"
        pub fn handle(ctx: &Ctx) -> bool {
            ctx.experimental_search_index
        }
        struct Ctx { experimental_search_index: bool }
    "#;

    for i in 0..60 {
        files.push((format!("src/file_{i:03}.rs"), body.to_string()));
    }

    let owned: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let project = setup_project(&owned);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let start = std::time::Instant::now();
    let resp = send(
        &mut aft,
        json!({
            "id": "perf-replace",
            "method": "ast_replace",
            "pattern": "$X.experimental_search_index",
            "rewrite": "$X.search_index",
            "lang": "rust",
            "paths": ["src"],
            "globs": ["*.rs"],
            "dry_run": true,
        }),
    );
    let elapsed = start.elapsed();

    assert_eq!(
        resp["success"], true,
        "ast_replace should succeed: {resp:?}"
    );
    assert_eq!(resp["files_searched"], 60);
    assert_eq!(resp["files_with_matches"], 60);
    assert_eq!(resp["total_replacements"], 60);

    assert!(
        elapsed.as_secs_f64() < 5.0,
        "ast_replace regressed: {} files took {:.2}s (expected < 5s). \
         If this fails, check that ast_replace.rs still pre-compiles the pattern \
         outside the file loop and uses rayon par_iter for the file walk.",
        resp["files_searched"],
        elapsed.as_secs_f64()
    );

    let status = aft.shutdown();
    assert!(status.success());
}

// ---------------------------------------------------------------------------
// Anonymous-`$$$`-in-rewrite regression
// ---------------------------------------------------------------------------
//
// ast-grep's rewrite template only recognizes NAMED variadics like `$$$BODY`.
// Anonymous `$$$` is emitted as the literal string `$$$` in the output,
// silently destroying captured content. Reported in user dogfooding session
// when sync→async test conversion produced
//   test('alpha', async () => { $$$ })
// instead of preserving the test body.
//
// Fix: handle_ast_replace now rejects rewrites containing anonymous `$$$`
// up front with `code: "invalid_rewrite"` and actionable guidance pointing
// the agent at the named-variadic shape.

#[test]
fn ast_replace_rejects_anonymous_variadic_in_rewrite() {
    let original = "test('alpha', () => { const v = foo(); expect(v).toBe(1); });\n".to_string();
    let project = setup_project(&[("sample.ts", &original)]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let resp = send(
        &mut aft,
        json!({
            "id": "anon-variadic",
            "command": "ast_replace",
            "pattern": "test($NAME, () => { $$$ })",
            "rewrite": "test($NAME, async () => { $$$ })",
            "lang": "typescript",
            "dry_run": false,
        }),
    );

    assert_eq!(
        resp["success"], false,
        "anonymous $$$ in rewrite must be rejected: {resp:?}"
    );
    assert_eq!(resp["code"], "invalid_rewrite");

    let message = resp["message"].as_str().expect("error message string");
    // Guidance must point the agent at the named-variadic shape so they
    // can fix the pattern without guessing.
    assert!(
        message.contains("$$$BODY"),
        "error message should suggest a named variadic like $$$BODY: {message}"
    );

    // Critical safety check: the file must NOT have been written, and
    // must not contain a literal `$$$` from a half-applied rewrite.
    let on_disk = read_file(project.path(), "sample.ts");
    assert_eq!(
        on_disk, original,
        "rejected rewrite must not modify files on disk"
    );
    assert!(
        !on_disk.contains("$$$"),
        "file must not carry a literal `$$$` from a rejected rewrite"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_replace_accepts_named_variadic_in_rewrite() {
    // Counterpart to the rejection test: the documented workaround MUST
    // continue to work. If this regresses, the rejection guard is too
    // aggressive and would break a working pattern.
    let project = setup_project(&[("sample.ts", "test('alpha', () => { foo(); bar(); });\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let resp = send(
        &mut aft,
        json!({
            "id": "named-variadic",
            "command": "ast_replace",
            "pattern": "test($NAME, () => { $$$BODY })",
            "rewrite": "test($NAME, async () => { $$$BODY })",
            "lang": "typescript",
            "dry_run": false,
        }),
    );

    assert_eq!(resp["success"], true, "named variadic must work: {resp:?}");
    assert_eq!(resp["total_replacements"], 1);

    let on_disk = read_file(project.path(), "sample.ts");
    assert!(
        on_disk.contains("async () =>"),
        "rewrite should add `async`: {on_disk}"
    );
    assert!(
        on_disk.contains("foo()"),
        "captured body must be preserved (no literal $$$): {on_disk}"
    );
    assert!(
        on_disk.contains("bar()"),
        "captured body must be preserved (no literal $$$): {on_disk}"
    );
    assert!(
        !on_disk.contains("$$$"),
        "no literal $$$ may leak into output: {on_disk}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

// ---------------------------------------------------------------------------
// Hint tests — verify pattern-mistake hints propagate through the bridge.
// ---------------------------------------------------------------------------
//
// Today's bug: pipe-alternation patterns like
//   `LangId::C | LangId::Cpp | LangId::Bash`
// compile fine in ast-grep (no `invalid_pattern` error) but match zero AST
// nodes against source that obviously contains the literal text. The agent
// reads `total_matches: 0` as "no work to do" and silently misses every hit.
//
// These tests lock in the hint behavior so future agents see actionable
// guidance instead of zero-result silence.

#[test]
fn ast_search_attaches_hint_for_rust_match_arm_pipe_alternation() {
    let project = setup_project(&[(
        "src/lib.rs",
        "fn classify(v: u8) {\n    match v {\n        LangId::C | LangId::Cpp | LangId::Bash => {}\n        _ => {}\n    }\n}\n",
    )]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search = send(
        &mut aft,
        json!({
            "id": "search-pipe",
            "command": "ast_search",
            "pattern": "LangId::C | LangId::Cpp | LangId::Bash",
            "lang": "rust",
        }),
    );

    assert_eq!(
        search["success"], true,
        "search must succeed (zero-match, not error): {search:?}"
    );
    assert_eq!(
        search["total_matches"], 0,
        "the bug we're documenting: pipe-alternative compiles but matches zero"
    );

    // The fix: a hint must be attached so the agent knows why.
    let hint = search["hint"]
        .as_str()
        .expect("zero-match pipe pattern must include a hint");
    assert!(
        hint.contains("|"),
        "hint should call out the `|` operator: {hint}"
    );
    assert!(
        hint.to_lowercase().contains("ast")
            || hint.to_lowercase().contains("alternation")
            || hint.to_lowercase().contains("alternative"),
        "hint should explain the AST-vs-text distinction: {hint}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_attaches_hint_for_regex_alternation() {
    let project = setup_project(&[(
        "src/index.ts",
        "// nothing matching here\nconst foo = 1;\nconst bar = 2;\n",
    )]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search = send(
        &mut aft,
        json!({
            "id": "search-regex-alt",
            "command": "ast_search",
            "pattern": "foo|bar|baz",
            "lang": "typescript",
        }),
    );

    assert_eq!(search["success"], true, "{search:?}");
    assert_eq!(search["total_matches"], 0);
    let hint = search["hint"]
        .as_str()
        .expect("regex-alternation pattern must include a hint");
    assert!(
        hint.contains("|")
            && (hint.contains("ast")
                || hint.contains("AST")
                || hint.contains("alternation")
                || hint.contains("alternative")),
        "hint should explain the issue: {hint}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_no_hint_for_clean_zero_match() {
    let project = setup_project(&[("src/index.ts", "const x = 1;\nconst y = 2;\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    // Legitimate AST pattern that simply doesn't match — no hint should
    // be attached because there's no shape mistake to call out.
    let search = send(
        &mut aft,
        json!({
            "id": "search-clean-zero",
            "command": "ast_search",
            "pattern": "console.log($MSG)",
            "lang": "typescript",
        }),
    );

    assert_eq!(search["success"], true, "{search:?}");
    assert_eq!(search["total_matches"], 0);
    assert!(
        search.get("hint").is_none() || search["hint"].is_null(),
        "clean zero-match must NOT attach a hint: {search:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_search_attaches_hint_for_python_def_trailing_colon() {
    let project = setup_project(&[("module.py", "def add(a, b):\n    return a + b\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search = send(
        &mut aft,
        json!({
            "id": "search-py-colon",
            "command": "ast_search",
            "pattern": "def $FUNC($$$):",
            "lang": "python",
        }),
    );

    assert_eq!(search["success"], true, "{search:?}");
    // The pattern doesn't match because `def $FUNC($$$):` lacks a body.
    if search["total_matches"].as_u64().unwrap_or(99) == 0 {
        let hint = search["hint"]
            .as_str()
            .expect("python def with trailing colon should include a hint");
        assert!(
            hint.to_lowercase().contains("body") || hint.to_lowercase().contains("colon"),
            "hint should mention body/colon: {hint}"
        );
    }

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn ast_replace_attaches_hint_when_pattern_silently_does_not_match() {
    let project = setup_project(&[(
        "src/lib.rs",
        "fn classify(v: u8) {\n    match v {\n        LangId::C | LangId::Cpp => {}\n        _ => {}\n    }\n}\n",
    )]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let replace = send(
        &mut aft,
        json!({
            "id": "replace-silent",
            "command": "ast_replace",
            "pattern": "LangId::C | LangId::Cpp",
            "rewrite": "LangId::Other",
            "lang": "rust",
            "dry_run": true,
        }),
    );

    assert_eq!(
        replace["success"], true,
        "replace must succeed with zero matches, not error: {replace:?}"
    );
    assert_eq!(
        replace["total_replacements"], 0,
        "documenting the bug: pipe-alternative replaces zero"
    );

    let hint = replace["hint"]
        .as_str()
        .expect("zero-replacement pipe pattern must include a hint");
    assert!(
        hint.contains("|"),
        "hint should call out the `|` operator: {hint}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}
