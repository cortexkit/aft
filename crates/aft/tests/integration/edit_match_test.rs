use std::fs;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde_json::json;

use super::helpers::AftProcess;

#[test]
fn edit_match_glob_rolls_back_prior_files_when_later_write_fails() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let a = root.join("a.ts");
    let b = root.join("b.ts");
    let c = root.join("c.ts");

    fs::write(&a, "const a = \"OLD\";\n").unwrap();
    fs::write(&b, "const b = \"OLD\";\n").unwrap();
    fs::write(&c, "const c = \"OLD\";\n").unwrap();

    let mut readonly = fs::metadata(&b).unwrap().permissions();
    readonly.set_readonly(true);
    fs::set_permissions(&b, readonly).unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "glob-rollback-write-failure",
        "command": "edit_match",
        "file": format!("{}/*.ts", root.display()),
        "match": "OLD",
        "replacement": "NEW"
    });
    let resp = aft.send(&req.to_string());

    make_writable(&b);

    assert_eq!(resp["success"], false, "glob edit should fail: {resp:?}");
    assert_eq!(resp["code"], "write_error");
    assert_eq!(fs::read_to_string(&a).unwrap(), "const a = \"OLD\";\n");
    assert_eq!(fs::read_to_string(&b).unwrap(), "const b = \"OLD\";\n");
    assert_eq!(fs::read_to_string(&c).unwrap(), "const c = \"OLD\";\n");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_glob_rolls_back_when_any_file_becomes_syntax_invalid() {
    // Glob edit_match is atomic w.r.t. syntax: if any file ends up syntax-
    // invalid after the replacement, the whole batch rolls back to the
    // pre-edit checkpoint. Previously this code reported per-file
    // `syntax_valid: false` and left edits applied, which silently broke
    // the project. The new contract: agent gets a clear `syntax_invalid`
    // error and the working tree is unchanged.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let a = root.join("a.ts");
    let b = root.join("b.ts");
    let c = root.join("c.ts");

    let original_a = "const a = 1; // TARGET\n";
    let original_b = "const b = \"TARGET\";\n";
    let original_c = "const c = TARGET;\n";
    fs::write(&a, original_a).unwrap();
    fs::write(&b, original_b).unwrap();
    fs::write(&c, original_c).unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "glob-rollback-syntax-failure",
        "command": "edit_match",
        "file": format!("{}/*.ts", root.display()),
        "match": "TARGET",
        "replacement": "{;"
    });
    let resp = aft.send(&req.to_string());

    assert_eq!(
        resp["success"], false,
        "glob edit must fail when any file becomes syntax-invalid: {resp:?}"
    );
    assert_eq!(resp["code"], "syntax_invalid", "wrong error code: {resp:?}");
    let msg = resp["message"].as_str().expect("message");
    assert!(
        msg.contains("rolled back"),
        "error message should mention rollback: {msg}"
    );

    // All three files must be unchanged from their original contents.
    assert_eq!(fs::read_to_string(&a).unwrap(), original_a);
    assert_eq!(fs::read_to_string(&b).unwrap(), original_b);
    assert_eq!(fs::read_to_string(&c).unwrap(), original_c);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_glob_succeeds_when_all_files_remain_syntax_valid() {
    // Companion to the rollback test: if the replacement keeps every file
    // syntax-valid, the batch commits normally and per-file results report
    // syntax_valid: true. This guards against an over-eager rollback that
    // would block legitimate batch edits.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let a = root.join("a.ts");
    let b = root.join("b.ts");

    fs::write(&a, "const a = TARGET;\n").unwrap();
    fs::write(&b, "const b = TARGET;\n").unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "glob-syntax-clean",
        "command": "edit_match",
        "file": format!("{}/*.ts", root.display()),
        "match": "TARGET",
        "replacement": "42"
    });
    let resp = aft.send(&req.to_string());

    assert_eq!(resp["success"], true, "expected success: {resp:?}");
    let files = resp["files"].as_array().expect("files array");
    assert_eq!(files.len(), 2);
    for file in files {
        assert_eq!(file["syntax_valid"], true, "file should be valid: {file:?}");
    }
    assert_eq!(fs::read_to_string(&a).unwrap(), "const a = 42;\n");
    assert_eq!(fs::read_to_string(&b).unwrap(), "const b = 42;\n");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_omits_syntax_valid_when_language_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.unusual_extension");
    fs::write(&file, "hello old world\n").unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "unsupported-syntax-valid",
        "command": "edit_match",
        "file": file,
        "match": "old",
        "replacement": "new"
    });
    let resp = aft.send(&req.to_string());

    assert_eq!(resp["success"], true, "expected edit success: {resp:?}");
    assert!(
        resp.get("syntax_valid").is_none(),
        "syntax_valid must be absent when validation could not run: {resp:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("notes.unusual_extension")).unwrap(),
        "hello new world\n"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
fn make_writable(path: &std::path::Path) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(path, permissions).unwrap();
}

#[cfg(windows)]
fn make_writable(path: &std::path::Path) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions).unwrap();
}
