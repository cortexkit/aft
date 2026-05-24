//! Integration tests for `move_symbol` through the binary protocol.
//!
//! Uses temp-dir isolation (copy fixtures, mutate copies, verify results)
//! to test the full move pipeline: symbol extraction, destination insertion,
//! consumer import rewiring, checkpoint creation/restore, and error paths.

use crate::helpers::{fixture_path, AftProcess};

/// Copy the `tests/fixtures/move_symbol/` directory into a temp dir,
/// including the `features/` subdirectory.  Returns `(TempDir, root_path)`.
fn setup_move_fixture() -> (tempfile::TempDir, String) {
    let fixtures = fixture_path("move_symbol");
    let tmp = tempfile::tempdir().expect("create temp dir");

    // Copy top-level fixture files
    for entry in std::fs::read_dir(&fixtures).expect("read fixtures dir") {
        let entry = entry.expect("read entry");
        let src = entry.path();
        if src.is_file() {
            let dst = tmp.path().join(entry.file_name());
            std::fs::copy(&src, &dst).expect("copy fixture file");
        }
    }

    // Copy features/ subdirectory
    let features_src = fixtures.join("features");
    if features_src.is_dir() {
        let features_dst = tmp.path().join("features");
        std::fs::create_dir_all(&features_dst).expect("create features dir");
        for entry in std::fs::read_dir(&features_src).expect("read features dir") {
            let entry = entry.expect("read entry");
            let src = entry.path();
            if src.is_file() {
                let dst = features_dst.join(entry.file_name());
                std::fs::copy(&src, &dst).expect("copy feature fixture");
            }
        }
    }

    let root = tmp.path().display().to_string();
    (tmp, root)
}

/// Helper: configure aft with the given project root and assert success.
fn configure(aft: &mut AftProcess, root: &str) {
    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":"{}"}}"#,
        root
    ));
    assert_eq!(
        resp["success"], true,
        "configure should succeed: {:?}",
        resp
    );
}

fn write_file(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("create parent");
    std::fs::write(path, content).expect("write file");
}

fn assert_move_symbol_unsupported(ext: &str, source: &str, dest: &str) {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let src = tmp.path().join(format!("source.{ext}"));
    let dst = tmp.path().join(format!("dest.{ext}"));
    write_file(&src, source);
    write_file(&dst, dest);

    let mut aft = AftProcess::spawn();
    let resp = aft.send(&format!(
        r#"{{"id":"unsupported-{ext}","command":"move_symbol","file":"{}","symbol":"Foo","destination":"{}"}}"#,
        src.display(),
        dst.display()
    ));
    assert_eq!(resp["success"], false, "move should fail: {resp:?}");
    assert_eq!(
        resp["code"], "unsupported_language",
        "wrong error: {resp:?}"
    );
    aft.shutdown();
}

#[test]
fn move_symbol_rejects_python_source() {
    assert_move_symbol_unsupported("py", "def Foo():\n    pass\n", "\n");
}

#[test]
fn move_symbol_rejects_rust_source() {
    assert_move_symbol_unsupported("rs", "pub fn Foo() {}\n", "\n");
}

#[test]
fn move_symbol_rejects_go_source() {
    assert_move_symbol_unsupported("go", "package main\n\nfunc Foo() {}\n", "package main\n");
}

#[test]
fn move_symbol_rewrites_barrel_named_reexport() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();
    let src_dir = tmp.path().join("src");
    let foo = src_dir.join("foo.ts");
    let bar = src_dir.join("bar.ts");
    let index = src_dir.join("index.ts");
    write_file(&foo, "export function Foo() { return 1; }\n");
    write_file(&bar, "export const Bar = 2;\n");
    write_file(&index, "export { Foo } from './foo';\n");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);
    let resp = aft.send(&format!(
        r#"{{"id":"barrel","command":"move_symbol","file":"{}","symbol":"Foo","destination":"{}"}}"#,
        foo.display(),
        bar.display()
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");
    let index_content = std::fs::read_to_string(index).expect("read index");
    assert!(
        index_content.contains("export { Foo } from './bar';"),
        "barrel should point at ./bar:\n{index_content}"
    );
    aft.shutdown();
}

// ---------------------------------------------------------------------------
// Success path tests
// ---------------------------------------------------------------------------

/// Basic move: formatDate from service.ts → utils.ts.
/// Verifies symbol removed from source, added to destination with export,
/// and consumer_a imports from the new location.
#[test]
fn move_symbol_basic() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":"{}","symbol":"formatDate","destination":"{}"}}"#,
        source, dest
    ));

    assert_eq!(
        resp["success"], true,
        "move_symbol should succeed: {:?}",
        resp
    );
    assert!(
        resp["files_modified"].as_u64().unwrap() >= 2,
        "at least source + dest should be modified"
    );
    assert!(
        resp["consumers_updated"].as_u64().unwrap() >= 1,
        "at least one consumer should be updated"
    );
    assert!(
        resp["checkpoint_name"]
            .as_str()
            .unwrap()
            .contains("formatDate"),
        "checkpoint should reference the moved symbol"
    );

    // Verify source no longer contains formatDate function
    let source_content = std::fs::read_to_string(&source).expect("read source");
    assert!(
        !source_content.contains("export function formatDate"),
        "formatDate should be removed from source"
    );
    // Other symbols should remain
    assert!(
        source_content.contains("export function parseDate"),
        "parseDate should stay in source"
    );
    assert!(
        source_content.contains("DATE_FORMAT"),
        "DATE_FORMAT should stay in source"
    );

    // Verify destination now contains formatDate
    let dest_content = std::fs::read_to_string(&dest).expect("read dest");
    assert!(
        dest_content.contains("export function formatDate"),
        "formatDate should appear in destination with export"
    );
    // Original destination content should remain
    assert!(
        dest_content.contains("export function slugify"),
        "slugify should still be in destination"
    );

    // Verify consumer_a now imports from utils instead of service
    let consumer_a =
        std::fs::read_to_string(format!("{}/consumer_a.ts", root)).expect("read consumer_a");
    assert!(
        consumer_a.contains("'./utils'") || consumer_a.contains("\"./utils\""),
        "consumer_a should import from ./utils, got:\n{}",
        consumer_a
    );
    assert!(
        !consumer_a.contains("'./service'") || consumer_a.contains("parseDate"),
        "consumer_a should no longer import formatDate from ./service"
    );

    aft.shutdown();
}

/// Explicitly verify ALL 5+ consumer files have correct import paths after move.
#[test]
fn move_symbol_multiple_consumers() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":"{}","symbol":"formatDate","destination":"{}"}}"#,
        source, dest
    ));

    assert_eq!(resp["success"], true, "move should succeed: {:?}", resp);

    // consumer_a.ts — same dir, imports only formatDate
    // Should: import { formatDate } from './utils'
    let ca = std::fs::read_to_string(format!("{}/consumer_a.ts", root)).unwrap();
    assert!(
        ca.contains("'./utils'") || ca.contains("\"./utils\""),
        "consumer_a should import from ./utils:\n{}",
        ca
    );
    assert!(
        ca.contains("formatDate"),
        "consumer_a should still reference formatDate"
    );

    // consumer_b.ts — imports both formatDate and parseDate
    // Should: keep parseDate from ./service, add formatDate from ./utils
    let cb = std::fs::read_to_string(format!("{}/consumer_b.ts", root)).unwrap();
    assert!(
        cb.contains("'./utils'") || cb.contains("\"./utils\""),
        "consumer_b should have import from ./utils:\n{}",
        cb
    );
    assert!(
        cb.contains("parseDate"),
        "consumer_b should still reference parseDate"
    );

    // consumer_c.ts — aliased import { formatDate as fmtDate }
    // Should: import from ./utils with alias preserved
    let cc = std::fs::read_to_string(format!("{}/consumer_c.ts", root)).unwrap();
    assert!(
        cc.contains("'./utils'") || cc.contains("\"./utils\""),
        "consumer_c should import from ./utils:\n{}",
        cc
    );

    // consumer_d.ts — imports only DATE_FORMAT (NOT formatDate)
    // Should: remain UNCHANGED
    let cd_original =
        std::fs::read_to_string(fixture_path("move_symbol").join("consumer_d.ts")).unwrap();
    let cd = std::fs::read_to_string(format!("{}/consumer_d.ts", root)).unwrap();
    assert_eq!(
        cd.trim(),
        cd_original.trim(),
        "consumer_d should be unchanged (only imports DATE_FORMAT)"
    );

    // consumer_e.ts — in features/ subdirectory, imports via '../service'
    // Should: import from '../utils'
    let ce = std::fs::read_to_string(format!("{}/features/consumer_e.ts", root)).unwrap();
    assert!(
        ce.contains("'../utils'") || ce.contains("\"../utils\""),
        "consumer_e should import from ../utils:\n{}",
        ce
    );

    // consumer_f.ts — imports only parseDate (NOT formatDate)
    // Should: remain UNCHANGED
    let cf_original =
        std::fs::read_to_string(fixture_path("move_symbol").join("consumer_f.ts")).unwrap();
    let cf = std::fs::read_to_string(format!("{}/consumer_f.ts", root)).unwrap();
    assert_eq!(
        cf.trim(),
        cf_original.trim(),
        "consumer_f should be unchanged (only imports parseDate)"
    );

    aft.shutdown();
}

/// Aliased import: `import { formatDate as fmtDate }` should preserve alias after move.
#[test]
fn move_symbol_aliased_import() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":"{}","symbol":"formatDate","destination":"{}"}}"#,
        source, dest
    ));

    assert_eq!(resp["success"], true, "move should succeed: {:?}", resp);

    // consumer_c uses: import { formatDate as fmtDate } from './service';
    // After move, should be: import { formatDate as fmtDate } from './utils';
    let cc = std::fs::read_to_string(format!("{}/consumer_c.ts", root)).unwrap();

    assert!(
        cc.contains("fmtDate"),
        "alias 'fmtDate' should be preserved:\n{}",
        cc
    );
    assert!(
        cc.contains("formatDate as fmtDate"),
        "alias form 'formatDate as fmtDate' should be preserved:\n{}",
        cc
    );
    assert!(
        cc.contains("'./utils'") || cc.contains("\"./utils\""),
        "should import from ./utils:\n{}",
        cc
    );

    aft.shutdown();
}

// ---------------------------------------------------------------------------
// Checkpoint tests
// ---------------------------------------------------------------------------

/// Checkpoint: move creates a checkpoint that can be listed and restored.
#[test]
fn move_symbol_checkpoint() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    // Snapshot originals
    let source_original = std::fs::read_to_string(&source).unwrap();
    let dest_original = std::fs::read_to_string(&dest).unwrap();
    let ca_original = std::fs::read_to_string(format!("{}/consumer_a.ts", root)).unwrap();

    // Perform the move
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":"{}","symbol":"formatDate","destination":"{}"}}"#,
        source, dest
    ));
    assert_eq!(resp["success"], true, "move should succeed: {:?}", resp);
    let checkpoint_name = resp["checkpoint_name"].as_str().unwrap().to_string();

    // Verify list_checkpoints shows it
    let resp = aft.send(r#"{"id":"2","command":"list_checkpoints"}"#);
    let checkpoints = resp["checkpoints"].as_array().expect("checkpoints array");
    let found = checkpoints
        .iter()
        .find(|c| c["name"].as_str() == Some(&checkpoint_name));
    assert!(
        found.is_some(),
        "checkpoint '{}' should appear in list_checkpoints, got: {:?}",
        checkpoint_name,
        checkpoints
    );
    let cp = found.unwrap();
    assert!(
        cp["file_count"].as_u64().unwrap() >= 2,
        "checkpoint should cover at least source + dest files"
    );

    // Restore the checkpoint
    let resp = aft.send(&format!(
        r#"{{"id":"3","command":"restore_checkpoint","name":"{}"}}"#,
        checkpoint_name
    ));
    assert_eq!(
        resp["name"].as_str(),
        Some(checkpoint_name.as_str()),
        "restore should return checkpoint name: {:?}",
        resp
    );

    // Verify files are back to their original state
    let source_restored = std::fs::read_to_string(&source).unwrap();
    let dest_restored = std::fs::read_to_string(&dest).unwrap();
    let ca_restored = std::fs::read_to_string(format!("{}/consumer_a.ts", root)).unwrap();

    assert_eq!(
        source_original, source_restored,
        "source should be restored to original"
    );
    assert_eq!(
        dest_original, dest_restored,
        "dest should be restored to original"
    );
    assert_eq!(
        ca_original, ca_restored,
        "consumer_a should be restored to original"
    );

    aft.shutdown();
}

#[test]
fn move_symbol_operation_undo_restores_source_destination_and_consumers() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);
    let consumer_a = format!("{}/consumer_a.ts", root);
    let consumer_e = format!("{}/features/consumer_e.ts", root);

    let source_original = std::fs::read_to_string(&source).unwrap();
    let dest_original = std::fs::read_to_string(&dest).unwrap();
    let consumer_a_original = std::fs::read_to_string(&consumer_a).unwrap();
    let consumer_e_original = std::fs::read_to_string(&consumer_e).unwrap();

    let resp = aft.send(&format!(
        r#"{{"id":"move-before-undo","command":"move_symbol","file":"{}","symbol":"formatDate","destination":"{}"}}"#,
        source, dest
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");
    assert!(
        resp["backup_ids"].as_array().unwrap().len() >= 4,
        "move should snapshot source, destination, and consumers: {resp:?}"
    );
    assert_ne!(std::fs::read_to_string(&source).unwrap(), source_original);
    assert_ne!(std::fs::read_to_string(&dest).unwrap(), dest_original);
    assert_ne!(
        std::fs::read_to_string(&consumer_a).unwrap(),
        consumer_a_original
    );
    assert_ne!(
        std::fs::read_to_string(&consumer_e).unwrap(),
        consumer_e_original
    );

    let undo = aft.send(r#"{"id":"undo-move-symbol-operation","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo should succeed: {undo:?}");
    assert_eq!(undo["operation"], true);
    assert!(
        undo["restored_count"].as_u64().unwrap() >= 4,
        "undo should restore all touched files: {undo:?}"
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), source_original);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), dest_original);
    assert_eq!(
        std::fs::read_to_string(&consumer_a).unwrap(),
        consumer_a_original
    );
    assert_eq!(
        std::fs::read_to_string(&consumer_e).unwrap(),
        consumer_e_original
    );

    aft.shutdown();
}

#[test]
fn move_symbol_undo_removes_new_destination_file() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().display().to_string();
    let source = tmp.path().join("source.ts");
    let dest = tmp.path().join("new_dest.ts");
    write_file(
        &source,
        "export function keep() { return 1; }\nexport function moveMe() { return 2; }\n",
    );

    let source_original = std::fs::read_to_string(&source).unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let resp = aft.send(&format!(
        r#"{{"id":"move-to-new-dest","command":"move_symbol","file":"{}","symbol":"moveMe","destination":"{}"}}"#,
        source.display(),
        dest.display()
    ));
    assert_eq!(resp["success"], true, "move should succeed: {resp:?}");
    assert!(dest.exists(), "destination should be created");
    assert_ne!(std::fs::read_to_string(&source).unwrap(), source_original);

    let undo = aft.send(r#"{"id":"undo-move-new-dest","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo should succeed: {undo:?}");
    assert_eq!(std::fs::read_to_string(&source).unwrap(), source_original);
    assert!(
        !dest.exists(),
        "new destination should be deleted by tombstone undo"
    );

    aft.shutdown();
}

// ---------------------------------------------------------------------------
// Error path tests
// ---------------------------------------------------------------------------

/// `move_symbol` without prior `configure` returns `not_configured`.
#[test]
fn move_symbol_not_configured() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();

    // Use real files from the temp dir so the file_not_found guard passes,
    // but don't call configure — the not_configured guard fires next.
    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":"{}","symbol":"formatDate","destination":"{}"}}"#,
        source, dest
    ));

    assert_eq!(resp["success"], false, "should fail: {:?}", resp);
    assert_eq!(resp["code"], "not_configured");

    aft.shutdown();
}

/// `move_symbol` for a nonexistent symbol returns `symbol_not_found`.
#[test]
fn move_symbol_symbol_not_found() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":"{}","symbol":"nonExistentFn","destination":"{}"}}"#,
        source, dest
    ));

    assert_eq!(resp["success"], false, "should fail: {:?}", resp);
    assert_eq!(resp["code"], "symbol_not_found");

    aft.shutdown();
}

#[test]
fn move_symbol_ambiguous_symbol_is_error_response() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let source = tmp.path().join("source.ts");
    let dest = tmp.path().join("dest.ts");
    std::fs::write(
        &source,
        "export function duplicate(): string { return 'top'; }\nexport class Boxed {\n  duplicate(): string { return 'method'; }\n}\n",
    )
    .expect("write source");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"ambiguous","command":"move_symbol","file":"{}","symbol":"duplicate","destination":"{}"}}"#,
        source.display(),
        dest.display()
    ));

    assert_eq!(resp["success"], false, "should fail: {resp:?}");
    assert_eq!(resp["code"], "ambiguous_symbol");
    assert!(resp["candidates"].as_array().unwrap().len() >= 2);

    aft.shutdown();
}

/// `move_symbol` rejects non-top-level symbols (class methods).
#[test]
fn move_symbol_non_top_level() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    // "format" is a method inside the DateHelper class in service.ts
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":"{}","symbol":"format","destination":"{}","scope":"DateHelper"}}"#,
        source, dest
    ));

    assert_eq!(
        resp["success"], false,
        "should fail for class method: {:?}",
        resp
    );
    assert_eq!(
        resp["code"], "invalid_request",
        "should return invalid_request for non-top-level: {:?}",
        resp
    );
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .contains("non-top-level")
            || resp["message"]
                .as_str()
                .unwrap_or("")
                .contains("non-top-level"),
        "error message should mention non-top-level: {:?}",
        resp
    );

    aft.shutdown();
}

/// `move_symbol` with missing file returns file_not_found.
#[test]
fn move_symbol_file_not_found() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":"{}/nonexistent.ts","symbol":"foo","destination":"{}"}}"#,
        root, dest
    ));

    assert_eq!(resp["success"], false, "should fail: {:?}", resp);
    assert_eq!(resp["code"], "file_not_found");

    aft.shutdown();
}

/// `move_symbol` must surface `project_too_large` instead of silently moving
/// the symbol. Moving without rewriting consumer imports leaves the workspace
/// broken, so the fail-loud behavior is a correctness fix (Oracle v0.15.1
/// review, bug #2). Pre-fix the callers_of Err branch caught every error
/// including `project_too_large` and defaulted to zero consumers.
#[test]
fn move_symbol_project_too_large() {
    let (_tmp, root) = setup_move_fixture();
    let mut aft = AftProcess::spawn();

    // Configure with an artificially low cap so the 7+ file fixture trips the
    // guard. This asserts the guard fires BEFORE the move writes anything.
    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":"{}","max_callgraph_files":1}}"#,
        root
    ));
    assert_eq!(resp["success"], true);

    let source = format!("{}/service.ts", root);
    let dest = format!("{}/utils.ts", root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":"{}","symbol":"formatDate","destination":"{}"}}"#,
        source, dest
    ));

    assert_eq!(resp["success"], false, "move should fail: {:?}", resp);
    assert_eq!(resp["code"], "project_too_large");
    let msg = resp["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("max_callgraph_files"),
        "error should mention max_callgraph_files: {}",
        msg
    );

    aft.shutdown();
}

/// Move of an exported symbol does not leave the `export` keyword behind.
///
/// Regression: when moving `export function greet`, the byte range of
/// `function_declaration` excludes the wrapping `export_statement`, so
/// `remove_symbol_from_source` only removed `function greet(...) {...}` and
/// left a stray `export` that then attached to the next sibling declaration.
/// `find_export_keyword_start` extends the deletion range backwards to cover
/// the `export` keyword and trailing whitespace.
#[test]
fn move_symbol_does_not_leak_export_keyword() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let source = tmp.path().join("sample.ts");
    let dest = tmp.path().join("helper.ts");

    std::fs::write(
        &source,
        "export function greet(user: string) {\n  return `Hello, ${user}!`;\n}\n\nfunction other(): number {\n  return 1;\n}\n",
    )
    .expect("write source");

    let mut aft = AftProcess::spawn();
    let root = tmp.path().display().to_string();
    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":"{}"}}"#,
        root
    ));
    assert_eq!(resp["success"], true);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"move_symbol","file":"{}","symbol":"greet","destination":"{}"}}"#,
        source.display(),
        dest.display()
    ));
    assert_eq!(resp["success"], true, "move should succeed: {:?}", resp);

    let after_source = std::fs::read_to_string(&source).expect("read source");
    let after_dest = std::fs::read_to_string(&dest).expect("read dest");

    // Source: `other` should still NOT be exported. If the bug regressed,
    // `export ` would be left attached to `function other`.
    assert!(
        !after_source.contains("export function other"),
        "export keyword leaked onto the next declaration:\n{}",
        after_source
    );
    assert!(
        after_source.contains("function other(): number {"),
        "`other` should be present and unmodified:\n{}",
        after_source
    );
    assert!(
        !after_source.contains("greet"),
        "`greet` should be removed from source:\n{}",
        after_source
    );

    // Destination: `greet` should be exported (single export, not duplicated).
    assert!(
        after_dest.contains("export function greet"),
        "destination should have `export function greet`:\n{}",
        after_dest
    );
    assert!(
        !after_dest.contains("export export"),
        "destination should not have duplicate export:\n{}",
        after_dest
    );

    aft.shutdown();
}

/// Extract preserves the `export` keyword on the enclosing function.
///
/// Regression: insertion point was `function_declaration.start_byte()`, which
/// is AFTER the `export` keyword. The extracted function got inserted between
/// `export ` and `function`, silently transferring the `export` from the
/// original function to the new extracted one.
#[test]
fn extract_function_preserves_enclosing_export_keyword() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("sample.ts");

    std::fs::write(
        &file,
        "export function process(items: string[]) {\n  try {\n    const items2 = items.map(i => i.toLowerCase());\n    const message = `count: ${items2.length}`;\n    console.log(message);\n    return message;\n  } catch (e) {\n    throw new Error(`Failed: ${e}`);\n  }\n}\n",
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    let root = tmp.path().display().to_string();
    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":"{}"}}"#,
        root
    ));
    assert_eq!(resp["success"], true);

    // Extract just the items.map(...) line.
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"extract_function","file":"{}","start_line":3,"end_line":4,"name":"makeItems"}}"#,
        file.display()
    ));
    assert_eq!(resp["success"], true, "extract should succeed: {:?}", resp);

    let after = std::fs::read_to_string(&file).expect("read file");
    // `process` must still be exported after extraction.
    assert!(
        after.contains("export function process"),
        "process should still be exported:\n{}",
        after
    );
    // The extracted function `makeItems` must NOT be exported.
    assert!(
        !after.contains("export function makeItems"),
        "extracted function should not be exported:\n{}",
        after
    );
    assert!(
        after.contains("function makeItems("),
        "extracted function should be present:\n{}",
        after
    );

    aft.shutdown();
}
