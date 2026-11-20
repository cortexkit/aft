use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

#[cfg(unix)]
use super::helpers::user_config;
use super::helpers::AftProcess;

fn configured_aft(project: &Path) -> AftProcess {
    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &json!({
            "id": "cfg-apply-patch",
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );
    aft
}

#[cfg(unix)]
fn configured_restricted_aft(project: &Path) -> AftProcess {
    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &json!({
            "id": "cfg-apply-patch-restricted",
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "config": user_config(json!({ "restrict_to_project_root": true })),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );
    aft
}

fn write_file(root: &Path, relative: &str, content: &str) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path
}

fn apply(aft: &mut AftProcess, id: &str, patch_text: &str) -> Value {
    aft.send(
        &json!({
            "id": id,
            "command": "apply_patch",
            "params": { "patch_text": patch_text },
        })
        .to_string(),
    )
}

fn preview(aft: &mut AftProcess, id: &str, patch_text: &str) -> Value {
    aft.send(
        &json!({
            "id": id,
            "command": "apply_patch",
            "params": { "patch_text": patch_text, "preview": true },
        })
        .to_string(),
    )
}

#[test]
fn apply_patch_full_multifile_and_single_undo_reverts_all() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "update.txt", "alpha\nold\nomega\n");
    write_file(root, "delete.txt", "remove me\n");
    let mut aft = configured_aft(root);

    let patch = r#"*** Begin Patch
*** Add File: add.txt
+created
*** Update File: update.txt
@@
 alpha
-old
+new
 omega
*** Delete File: delete.txt
*** End Patch"#;

    let resp = apply(&mut aft, "apply-full", patch);
    assert_eq!(resp["success"], true, "patch failed: {resp:?}");
    assert_eq!(resp["complete"], true);
    assert_eq!(resp["partial"], false);
    assert_eq!(resp["metadata"]["files"].as_array().unwrap().len(), 3);
    assert_eq!(
        fs::read_to_string(root.join("add.txt")).unwrap(),
        "created\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("update.txt")).unwrap(),
        "alpha\nnew\nomega\n"
    );
    assert!(!root.join("delete.txt").exists());

    let undo = aft.send(&json!({ "id": "undo-full", "command": "undo" }).to_string());
    assert_eq!(undo["success"], true, "undo failed: {undo:?}");
    assert_eq!(undo["operation"], true);
    // All three touched paths are reported: the two content restores
    // (update.txt, delete.txt) plus the removal of the created add.txt,
    // which undo reports as a restored path since the tombstone commit
    // is part of reverting the operation.
    assert_eq!(
        undo["restored_count"], 3,
        "all reverted paths reported: {undo:?}"
    );
    assert!(!root.join("add.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("update.txt")).unwrap(),
        "alpha\nold\nomega\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("delete.txt")).unwrap(),
        "remove me\n"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn apply_patch_crlf_envelope_preserves_only_content_carriage_returns() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "existing.txt", "alpha\nold\rinside\nomega\n");
    let mut aft = configured_aft(root);

    let patch = [
        "*** Begin Patch",
        "*** Add File: created.txt",
        "+created",
        "*** Update File: existing.txt",
        "@@",
        "-old\rinside",
        "+new\rinside",
        "*** End Patch",
    ]
    .join("\r\n");

    let preview_resp = preview(&mut aft, "preview-crlf-envelope", &patch);
    assert_eq!(
        preview_resp["success"], true,
        "CRLF preview failed: {preview_resp:?}"
    );
    assert!(!root.join("created.txt").exists());
    assert_eq!(
        fs::read(&root.join("existing.txt")).unwrap(),
        b"alpha\nold\rinside\nomega\n"
    );

    let apply_resp = apply(&mut aft, "apply-crlf-envelope", &patch);
    assert_eq!(
        apply_resp["success"], true,
        "CRLF apply failed: {apply_resp:?}"
    );
    assert_eq!(
        preview_resp["preview_diff"], apply_resp["metadata"]["diff"],
        "preview and apply must normalize the same patch envelope"
    );
    assert_eq!(fs::read(root.join("created.txt")).unwrap(), b"created\n");
    assert_eq!(
        fs::read(root.join("existing.txt")).unwrap(),
        b"alpha\nnew\rinside\nomega\n",
        "the CRLF terminator must be removed while the content carriage return remains"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn apply_patch_update_uses_reflow_fuzzy_hunk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(
        root,
        "reflow.txt",
        "const message = first + second + third;\n",
    );
    let mut aft = configured_aft(root);

    let patch = r#"*** Begin Patch
*** Update File: reflow.txt
@@
-const message = first +
-  second +
-  third;
+const message = first + second + fourth;
*** End Patch"#;

    let resp = apply(&mut aft, "apply-reflow", patch);
    assert_eq!(resp["success"], true, "reflow patch failed: {resp:?}");
    assert_eq!(
        fs::read_to_string(root.join("reflow.txt")).unwrap(),
        "const message = first + second + fourth;\n"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn apply_patch_preserves_dominant_line_endings_and_preview_parity() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let replacement = root.join("replacement.txt");
    fs::write(&replacement, b"alpha\r\nold\r\nomega\r\n").unwrap();
    let mut aft = configured_aft(root);

    let replacement_patch = r#"*** Begin Patch
*** Update File: replacement.txt
@@
-old
+new
*** End Patch"#;
    let preview_resp = preview(&mut aft, "preview-crlf", replacement_patch);
    assert_eq!(
        preview_resp["success"], true,
        "preview failed: {preview_resp:?}"
    );
    assert_eq!(
        fs::read(&replacement).unwrap(),
        b"alpha\r\nold\r\nomega\r\n"
    );

    let apply_resp = apply(&mut aft, "apply-crlf", replacement_patch);
    assert_eq!(
        apply_resp["success"], true,
        "CRLF replacement failed: {apply_resp:?}"
    );
    assert_eq!(
        preview_resp["preview_diff"], apply_resp["metadata"]["diff"],
        "preview and apply must render the same resulting bytes"
    );
    assert_eq!(
        fs::read(&replacement).unwrap(),
        b"alpha\r\nnew\r\nomega\r\n"
    );

    let fixtures = [
        ("insertion.txt", b"alpha\r\nomega\r\n".as_slice()),
        ("mixed.txt", b"alpha\r\nold\nomega\r\n".as_slice()),
        ("unterminated.txt", b"alpha\r\nold".as_slice()),
        ("lf-replacement.txt", b"alpha\nold\nomega\n".as_slice()),
        ("lf-insertion.txt", b"alpha\nomega\n".as_slice()),
        ("lf-unterminated.txt", b"alpha\nold".as_slice()),
    ];
    for (path, contents) in fixtures {
        fs::write(root.join(path), contents).unwrap();
    }

    let remaining_patch = r#"*** Begin Patch
*** Update File: insertion.txt
@@
+inserted
*** Update File: mixed.txt
@@
-old
+new
*** Update File: unterminated.txt
@@
-old
+new
*** Update File: lf-replacement.txt
@@
-old
+new
*** Update File: lf-insertion.txt
@@
+inserted
*** Update File: lf-unterminated.txt
@@
-old
+new
*** End Patch"#;
    let remaining_resp = apply(&mut aft, "apply-line-endings", remaining_patch);
    assert_eq!(
        remaining_resp["success"], true,
        "line-ending updates failed: {remaining_resp:?}"
    );

    let expected = [
        (
            "insertion.txt",
            b"alpha\r\nomega\r\ninserted\r\n".as_slice(),
        ),
        ("mixed.txt", b"alpha\r\nnew\r\nomega\r\n".as_slice()),
        ("unterminated.txt", b"alpha\r\nnew\r\n".as_slice()),
        ("lf-replacement.txt", b"alpha\nnew\nomega\n".as_slice()),
        ("lf-insertion.txt", b"alpha\nomega\ninserted\n".as_slice()),
        ("lf-unterminated.txt", b"alpha\nnew\n".as_slice()),
    ];
    for (path, contents) in expected {
        assert_eq!(fs::read(root.join(path)).unwrap(), contents, "{path}");
    }

    assert!(aft.shutdown().success());
}

#[test]
fn apply_patch_move_happy_path_and_undo_restores_source() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "source.txt", "before\nkeep\n");
    let mut aft = configured_aft(root);

    let patch = r#"*** Begin Patch
*** Update File: source.txt
*** Move to: nested/dest.txt
@@
-before
+after
 keep
*** End Patch"#;

    let resp = apply(&mut aft, "apply-move", patch);
    assert_eq!(resp["success"], true, "move failed: {resp:?}");
    let file = &resp["metadata"]["files"][0];
    assert_eq!(file["type"], "move");
    assert_eq!(file["relativePath"], "nested/dest.txt");
    assert!(file["movePath"]
        .as_str()
        .unwrap()
        .ends_with("nested/dest.txt"));
    assert!(!root.join("source.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("nested/dest.txt")).unwrap(),
        "after\nkeep\n"
    );

    let undo = aft.send(&json!({ "id": "undo-move", "command": "undo" }).to_string());
    assert_eq!(undo["success"], true, "undo failed: {undo:?}");
    assert_eq!(
        fs::read_to_string(root.join("source.txt")).unwrap(),
        "before\nkeep\n"
    );
    assert!(!root.join("nested/dest.txt").exists());

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn restricted_apply_patch_rejects_symlink_delete_without_touching_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let target = write_file(root, "target.txt", "target contents\n");
    let link = root.join("target-link.txt");
    std::os::unix::fs::symlink("target.txt", &link).unwrap();
    let mut aft = configured_restricted_aft(root);

    let patch = r#"*** Begin Patch
*** Delete File: target-link.txt
*** End Patch"#;
    let response = apply(&mut aft, "apply-delete-symlink", patch);

    assert_eq!(
        response["success"], false,
        "delete should fail: {response:?}"
    );
    assert_eq!(response["code"], "invalid_request");
    assert!(response["message"]
        .as_str()
        .unwrap()
        .contains("refusing to delete symlink"));
    assert!(
        fs::symlink_metadata(&link).is_ok(),
        "symlink should remain intact"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "target contents\n");

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn restricted_apply_patch_move_removes_symlink_entry_without_touching_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let target = write_file(root, "target.txt", "target contents\n");
    let source = root.join("before-link.txt");
    let destination = root.join("after.txt");
    std::os::unix::fs::symlink("target.txt", &source).unwrap();
    let mut aft = configured_restricted_aft(root);

    let patch = r#"*** Begin Patch
*** Update File: before-link.txt
*** Move to: after.txt
@@
-target contents
+updated contents
*** End Patch"#;
    let response = apply(&mut aft, "apply-move-symlink", patch);

    assert_eq!(
        response["success"], true,
        "move should succeed: {response:?}"
    );
    assert!(
        fs::symlink_metadata(&source).is_err(),
        "the source symlink entry should be removed"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "target contents\n");
    assert_eq!(
        fs::read_to_string(&destination).unwrap(),
        "updated contents\n"
    );

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
fn assert_move_onto_symlink_refused(response: &Value) {
    assert_eq!(response["success"], false, "move should fail: {response:?}");
    assert_eq!(response["code"], "invalid_request");
    assert!(
        response["message"].as_str().unwrap().contains(
            "refusing to move onto symlink 'destination.txt'; symlink undo is not supported"
        ),
        "{response:?}"
    );
}

#[cfg(unix)]
const MOVE_ONTO_DESTINATION_PATCH: &str = r#"*** Begin Patch
*** Update File: source.txt
*** Move to: destination.txt
@@
-before
+after
*** End Patch"#;

/// A move writes through a destination symlink into its target, while the
/// root check judged only the link's own location: the target outside the
/// root was overwritten although every direct write to it is refused.
#[cfg(unix)]
#[test]
fn restricted_apply_patch_move_onto_symlink_to_outside_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = dir.path();
    let outside_file = write_file(outside.path(), "outside.txt", "outside original\n");
    let source = write_file(root, "source.txt", "before\n");
    std::os::unix::fs::symlink(&outside_file, root.join("destination.txt")).unwrap();
    let mut aft = configured_restricted_aft(root);

    let previewed = preview(
        &mut aft,
        "preview-move-onto-link",
        MOVE_ONTO_DESTINATION_PATCH,
    );
    assert_move_onto_symlink_refused(&previewed);
    let applied = apply(
        &mut aft,
        "apply-move-onto-link",
        MOVE_ONTO_DESTINATION_PATCH,
    );
    assert_move_onto_symlink_refused(&applied);

    assert_eq!(
        fs::read_to_string(&outside_file).unwrap(),
        "outside original\n"
    );
    assert_eq!(fs::read_to_string(&source).unwrap(), "before\n");
    assert!(fs::symlink_metadata(root.join("destination.txt"))
        .unwrap()
        .file_type()
        .is_symlink());

    assert!(aft.shutdown().success());
}

/// A move back onto the same symlink is otherwise turned into an in-place
/// update of the link's location, which writes through to the target while
/// the undo snapshot records only the link. (Under `restrict_to_project_root`
/// an outside target is refused earlier, as a move source read through a link.)
#[cfg(unix)]
#[test]
fn apply_patch_move_onto_same_symlink_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let target = write_file(root, "target.txt", "before\n");
    std::os::unix::fs::symlink("target.txt", root.join("destination.txt")).unwrap();
    let mut aft = configured_aft(root);

    let patch = r#"*** Begin Patch
*** Update File: destination.txt
*** Move to: destination.txt
@@
-before
+after
*** End Patch"#;
    let previewed = preview(&mut aft, "preview-move-onto-same-link", patch);
    assert_move_onto_symlink_refused(&previewed);
    let applied = apply(&mut aft, "apply-move-onto-same-link", patch);
    assert_move_onto_symlink_refused(&applied);

    assert_eq!(fs::read_to_string(&target).unwrap(), "before\n");

    assert!(aft.shutdown().success());
}

/// Without root restriction the move used to overwrite the link's target,
/// and undo then restored the source and the link but never the target's
/// old content, because the snapshot recorded only the link.
#[cfg(unix)]
#[test]
fn apply_patch_move_onto_symlink_is_refused_so_its_target_is_never_lost() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = write_file(root, "source.txt", "before\n");
    let target = write_file(root, "target.txt", "keep me\n");
    std::os::unix::fs::symlink("target.txt", root.join("destination.txt")).unwrap();
    let mut aft = configured_aft(root);

    let previewed = preview(
        &mut aft,
        "preview-move-onto-local-link",
        MOVE_ONTO_DESTINATION_PATCH,
    );
    assert_move_onto_symlink_refused(&previewed);
    let applied = apply(
        &mut aft,
        "apply-move-onto-local-link",
        MOVE_ONTO_DESTINATION_PATCH,
    );
    assert_move_onto_symlink_refused(&applied);
    assert_eq!(fs::read_to_string(&target).unwrap(), "keep me\n");
    assert_eq!(fs::read_to_string(&source).unwrap(), "before\n");

    // Nothing was written, so undo has nothing of this patch to revert.
    let _ = aft.send(&json!({ "id": "undo-move-onto-local-link", "command": "undo" }).to_string());
    assert_eq!(fs::read_to_string(&target).unwrap(), "keep me\n");
    assert_eq!(fs::read_to_string(&source).unwrap(), "before\n");

    assert!(aft.shutdown().success());
}

/// A move reads its source through an in-root symlink, while the root check
/// judged only the link's location: a target outside the root was copied
/// into the project although a direct read of the link is refused.
#[cfg(unix)]
#[test]
fn restricted_apply_patch_move_from_symlink_to_outside_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = dir.path();
    let secret = write_file(outside.path(), "secret.txt", "outside secret\n");
    let link = root.join("link.txt");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let mut aft = configured_restricted_aft(root);

    let patch = r#"*** Begin Patch
*** Update File: link.txt
*** Move to: copied.txt
@@
+appended
*** End Patch"#;
    for response in [
        preview(&mut aft, "preview-move-from-outside-link", patch),
        apply(&mut aft, "apply-move-from-outside-link", patch),
    ] {
        assert_eq!(response["success"], false, "move should fail: {response:?}");
        assert_eq!(response["code"], "path_outside_root", "{response:?}");
    }

    assert!(
        !root.join("copied.txt").exists(),
        "outside content copied in"
    );
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&secret).unwrap(), "outside secret\n");

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn restricted_apply_patch_symlink_delete_rejection_rolls_back_prior_hunks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let good = write_file(root, "good.txt", "before\n");
    let target = write_file(root, "target.txt", "target contents\n");
    let link = root.join("target-link.txt");
    std::os::unix::fs::symlink("target.txt", &link).unwrap();
    let mut aft = configured_restricted_aft(root);

    let patch = r#"*** Begin Patch
*** Update File: good.txt
@@
-before
+after
*** Delete File: target-link.txt
*** End Patch"#;
    let response = apply(&mut aft, "apply-delete-symlink-rollback", patch);

    assert_eq!(
        response["success"], false,
        "patch should fail: {response:?}"
    );
    assert_eq!(fs::read_to_string(&good).unwrap(), "before\n");
    assert!(
        fs::symlink_metadata(&link).is_ok(),
        "symlink should remain intact"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "target contents\n");

    assert!(aft.shutdown().success());
}

#[test]
fn apply_patch_same_path_move_is_an_in_place_update_and_undoable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "source.txt", "before\nkeep\n");
    let mut aft = configured_aft(root);

    let patch = r#"*** Begin Patch
*** Update File: source.txt
*** Move to: source.txt
@@
-before
+after
 keep
*** End Patch"#;

    let resp = apply(&mut aft, "apply-same-path-move", patch);
    assert_eq!(resp["success"], true, "same-path update failed: {resp:?}");
    assert_eq!(resp["output"], "Updated source.txt");
    let file = &resp["metadata"]["files"][0];
    assert_eq!(file["type"], "update");
    assert_eq!(file["relativePath"], "source.txt");
    assert!(file.get("movePath").is_none());
    assert_eq!(
        fs::read_to_string(root.join("source.txt")).unwrap(),
        "after\nkeep\n"
    );

    let undo = aft.send(&json!({ "id": "undo-same-path", "command": "undo" }).to_string());
    assert_eq!(undo["success"], true, "undo failed: {undo:?}");
    assert_eq!(
        fs::read_to_string(root.join("source.txt")).unwrap(),
        "before\nkeep\n"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn apply_patch_same_path_move_alias_is_an_in_place_update() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "source.txt", "before\n");
    let mut aft = configured_aft(root);

    let patch = r#"*** Begin Patch
*** Update File: source.txt
*** Move to: ./source.txt
@@
-before
+after
*** End Patch"#;

    let resp = apply(&mut aft, "apply-same-path-alias", patch);
    assert_eq!(resp["success"], true, "aliased update failed: {resp:?}");
    assert_eq!(resp["output"], "Updated source.txt");
    let file = &resp["metadata"]["files"][0];
    assert_eq!(file["type"], "update");
    assert!(file.get("movePath").is_none());
    assert_eq!(
        fs::read_to_string(root.join("source.txt")).unwrap(),
        "after\n"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn apply_patch_same_path_move_preview_is_an_in_place_diff() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "source.txt", "before\nkeep\n");
    let mut aft = configured_aft(root);

    let patch = r#"*** Begin Patch
*** Update File: source.txt
*** Move to: source.txt
@@
-before
+after
 keep
*** End Patch"#;

    let resp = preview(&mut aft, "preview-same-path-move", patch);
    assert_eq!(resp["success"], true, "preview failed: {resp:?}");
    assert_eq!(resp["preview"], true);
    assert_eq!(resp["affected_rel_paths"], json!(["source.txt"]));
    assert!(resp["preview_diff"]
        .as_str()
        .unwrap()
        .contains("-before\n+after"));
    assert_eq!(
        fs::read_to_string(root.join("source.txt")).unwrap(),
        "before\nkeep\n"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn apply_patch_partial_keeps_successful_hunks_and_reports_failure() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "one.txt", "old\n");
    write_file(
        root,
        "two.txt",
        "header\nfunction two() {\n  const first = 1;\n  const actual = 2;\n  return first;\n}\n",
    );
    let mut aft = configured_aft(root);

    let patch = r#"*** Begin Patch
*** Update File: one.txt
@@
-old
+new
*** Update File: two.txt
@@
 function two() {
   const first = 1;
-  const expected = 2;
+  const replacement = 2;
   return first;
 }
*** End Patch"#;

    let resp = apply(&mut aft, "apply-partial", patch);
    assert_eq!(
        resp["success"], true,
        "partial should be success envelope: {resp:?}"
    );
    assert_eq!(resp["complete"], false);
    assert_eq!(resp["partial"], true);
    assert_eq!(resp["all_failed"], false);
    assert_eq!(fs::read_to_string(root.join("one.txt")).unwrap(), "new\n");
    assert_eq!(
        fs::read_to_string(root.join("two.txt")).unwrap(),
        "header\nfunction two() {\n  const first = 1;\n  const actual = 2;\n  return first;\n}\n"
    );
    let failure = resp["failures"][0]["error"].as_str().unwrap();
    assert!(failure.contains("Failed to find expected lines"));
    assert!(failure.contains("Nearest miss at lines 2-6 (matched 4/5 context lines)"));
    assert!(failure.contains("  4 |   const actual = 2;"));
    assert!(failure.contains(
        "First divergence: wanted line 3 `  const expected = 2;` vs file line 4 `  const actual = 2;`"
    ));
    assert!(resp["output"].as_str().unwrap().contains("Nearest miss"));

    assert!(aft.shutdown().success());
}

#[test]
fn apply_patch_total_failure_returns_error_envelope_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let original = "preamble\nfn target() {\n  let first = 1;\n  let actual = 2;\n  return first;\n}\ntrailer\n";
    write_file(root, "target.txt", original);
    let mut aft = configured_aft(root);

    let patch = r#"*** Begin Patch
*** Update File: target.txt
@@
 fn target() {
   let first = 1;
-  let expected = 2;
+  let replacement = 2;
   return first;
 }
*** End Patch"#;

    let resp = apply(&mut aft, "apply-total-failure", patch);
    assert_eq!(
        resp["success"], false,
        "total failure should be error: {resp:?}"
    );
    assert_eq!(resp["code"], "apply_patch_failed");
    assert_eq!(resp["complete"], false);
    assert_eq!(resp["all_failed"], true);
    assert_eq!(resp["partial"], false);
    assert_eq!(resp["metadata"]["files"].as_array().unwrap().len(), 0);
    assert_eq!(
        fs::read_to_string(root.join("target.txt")).unwrap(),
        original
    );

    let nearest_block = "Nearest miss at lines 2-6 (matched 4/5 context lines):";
    assert!(resp["message"].as_str().unwrap().contains(nearest_block));
    assert!(resp["output"].as_str().unwrap().contains(nearest_block));
    let failure = resp["failures"][0]["error"].as_str().unwrap();
    assert!(failure.contains(nearest_block));
    assert!(failure.contains("  4 |   let actual = 2;"));
    assert!(failure.contains(
        "First divergence: wanted line 3 `  let expected = 2;` vs file line 4 `  let actual = 2;`"
    ));

    assert!(aft.shutdown().success());
}

#[test]
fn apply_patch_preview_reports_diff_and_leaves_disk_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "existing.txt", "old\n");
    let mut aft = configured_aft(root);

    let patch = r#"*** Begin Patch
*** Add File: preview-new.txt
+created
*** Update File: existing.txt
@@
-old
+new
*** End Patch"#;

    let resp = preview(&mut aft, "preview-valid", patch);
    assert_eq!(resp["success"], true, "preview failed: {resp:?}");
    assert_eq!(resp["preview"], true);
    assert!(resp["preview_diff"]
        .as_str()
        .unwrap()
        .contains("preview-new.txt"));
    assert_eq!(resp["affected_rel_paths"].as_array().unwrap().len(), 2);
    assert!(!root.join("preview-new.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("existing.txt")).unwrap(),
        "old\n"
    );

    let invalid = r#"*** Begin Patch
*** Update File: existing.txt
@@
-missing
+new
*** End Patch"#;
    let err = preview(&mut aft, "preview-invalid", invalid);
    assert_eq!(
        err["success"], false,
        "invalid preview should fail: {err:?}"
    );
    assert_eq!(
        fs::read_to_string(root.join("existing.txt")).unwrap(),
        "old\n"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn apply_patch_add_existing_fails_and_syntax_rollback_does_not_poison_undo() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "exists.txt", "already\n");
    write_file(root, "good.ts", "const good = 1;\n");
    write_file(root, "bad.ts", "const value = 1;\n");
    let mut aft = configured_aft(root);

    let add_existing = r#"*** Begin Patch
*** Add File: exists.txt
+replacement
*** End Patch"#;
    let add_resp = apply(&mut aft, "add-existing", add_existing);
    assert_eq!(
        add_resp["success"], false,
        "add existing should fail: {add_resp:?}"
    );
    assert!(add_resp["failures"][0]["error"]
        .as_str()
        .unwrap()
        .contains("file already exists"));
    assert_eq!(
        fs::read_to_string(root.join("exists.txt")).unwrap(),
        "already\n"
    );

    let mixed = r#"*** Begin Patch
*** Update File: good.ts
@@
-const good = 1;
+const good = 2;
*** Update File: bad.ts
@@
-const value = 1;
+const value = {;
*** End Patch"#;
    let resp = apply(&mut aft, "syntax-rollback", mixed);
    assert_eq!(
        resp["success"], true,
        "mixed patch should be partial: {resp:?}"
    );
    assert_eq!(resp["partial"], true);
    assert_eq!(
        fs::read_to_string(root.join("good.ts")).unwrap(),
        "const good = 2;\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("bad.ts")).unwrap(),
        "const value = 1;\n"
    );

    let undo = aft.send(&json!({ "id": "undo-syntax", "command": "undo" }).to_string());
    assert_eq!(
        undo["success"], true,
        "undo should restore only successful hunk: {undo:?}"
    );
    assert_eq!(
        fs::read_to_string(root.join("good.ts")).unwrap(),
        "const good = 1;\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("bad.ts")).unwrap(),
        "const value = 1;\n"
    );

    assert!(aft.shutdown().success());
}
