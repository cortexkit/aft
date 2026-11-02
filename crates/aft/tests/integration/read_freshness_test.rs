//! `read` must return the bytes that are on disk at the moment of the call.
//!
//! These tests replay the sequences that would expose a read path serving
//! remembered content: a file edited through AFT and then restored behind
//! AFT's back by `git checkout --`, a rewrite that keeps the file's size and
//! modification time, and an external write while no file-watcher event can
//! have arrived. The default test daemon runs with its file watcher disabled,
//! so every external change here is invisible to watcher-driven invalidation;
//! a read that is still correct cannot be relying on one.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use super::helpers::AftProcess;

const MARKER: &str = "TUNING-ONLY PATCH";
const SESSION: &str = "read-freshness";

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    crate::test_helpers::apply_hermetic_git_env(command.current_dir(root));
    let output = command.args(args).output().expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn original_source() -> String {
    (1..=120).map(|n| format!("fn line_{n}() {{}}\n")).collect()
}

/// Commit a 120-line Rust file in a fresh repository and return the directory
/// the daemon should treat as its project root together with the file path.
/// With `linked_worktree`, the file is read from a `git worktree add`
/// checkout, whose `.git` is a pointer file into the primary repository.
fn committed_fixture(dir: &Path, linked_worktree: bool) -> (PathBuf, PathBuf) {
    let primary = dir.join("primary");
    fs::create_dir_all(primary.join("src")).expect("create fixture src");
    fs::write(primary.join("src/lib.rs"), original_source()).expect("write fixture");
    git(&primary, &["init", "-q"]);
    git(&primary, &["add", "."]);
    // CI runners have no global git identity, so the commit names its own.
    git(
        &primary,
        &[
            "-c",
            "user.name=AFT Tests",
            "-c",
            "user.email=aft-tests@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    let root = if linked_worktree {
        let worktree = dir.join("linked");
        git(
            &primary,
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().expect("utf-8 path"),
            ],
        );
        worktree
    } else {
        primary
    };
    let root = fs::canonicalize(root).expect("canonical fixture root");
    let file = root.join("src/lib.rs");
    (root, file)
}

fn configure(aft: &mut AftProcess, root: &Path, hashline: bool) {
    let mut request = json!({
        "id": "read-freshness-configure",
        "command": "configure",
        "session_id": SESSION,
        "harness": "opencode",
        "project_root": root,
    });
    if hashline {
        request["edit_slot_survives"] = json!(true);
        request["config"] = json!([{
            "tier": "project",
            "source": root.join(".cortexkit/aft.jsonc"),
            "doc": json!({
                "edit_mode": "hashline",
                "indexes": { "trigram": false, "semantic": false }
            })
            .to_string()
        }]);
    }
    let response = aft.send(&request.to_string());
    assert_eq!(response["success"], true, "configure failed: {response:#}");
}

fn tool_call(
    aft: &mut AftProcess,
    id: &str,
    name: &str,
    arguments: Value,
    hashline: bool,
) -> Value {
    let mut request = json!({
        "id": id,
        "command": "tool_call",
        "session_id": SESSION,
        "name": name,
        "arguments": arguments,
    });
    if hashline {
        request["edit_slot_survives"] = json!(true);
    }
    let response = aft.send(&request.to_string());
    assert_eq!(response["success"], true, "{name} failed: {response:#}");
    response
}

/// The agent-visible text of a ranged read of lines 40-100, a window that
/// contains every line these tests change.
fn read_window(aft: &mut AftProcess, id: &str, file: &Path, hashline: bool) -> String {
    let response = tool_call(
        aft,
        id,
        "read",
        json!({ "filePath": file, "startLine": 40, "endLine": 100 }),
        hashline,
    );
    response["text"]
        .as_str()
        .unwrap_or_else(|| panic!("read returned no text: {response:#}"))
        .to_string()
}

fn read_whole(aft: &mut AftProcess, id: &str, file: &Path, hashline: bool) -> String {
    let response = tool_call(aft, id, "read", json!({ "filePath": file }), hashline);
    response["text"]
        .as_str()
        .unwrap_or_else(|| panic!("read returned no text: {response:#}"))
        .to_string()
}

/// Edit through AFT, read the edit back, restore the file with
/// `git checkout --`, and read again at once. The second read must show the
/// restored line, not the edit that no longer exists on disk.
fn assert_read_sees_git_checkout_restore(linked_worktree: bool, hashline: bool) {
    let dir = crate::test_helpers::tempdir();
    let (root, file) = committed_fixture(dir.path(), linked_worktree);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root, hashline);

    let patched_line = format!("fn tuned() {{}} // {MARKER}");
    if hashline {
        // Hashline sessions only accept tagged patches from `edit`; `write`
        // is the AFT mutation that stays shape-independent here.
        let patched = original_source().replace("fn line_98() {}", &patched_line);
        tool_call(
            &mut aft,
            "freshness-write",
            "write",
            json!({ "filePath": file, "content": patched }),
            hashline,
        );
    } else {
        tool_call(
            &mut aft,
            "freshness-edit",
            "edit",
            json!({
                "filePath": file,
                "edits": [{ "oldString": "fn line_98() {}", "newString": patched_line }]
            }),
            hashline,
        );
    }
    let after_edit = read_window(&mut aft, "freshness-read-edited", &file, hashline);
    assert!(
        after_edit.contains(MARKER),
        "the edit must be visible before the restore: {after_edit}"
    );

    git(&root, &["checkout", "--", "src/lib.rs"]);
    assert!(
        !fs::read_to_string(&file).unwrap().contains(MARKER),
        "git checkout must have restored the committed bytes"
    );

    let after_restore = read_window(&mut aft, "freshness-read-restored", &file, hashline);
    assert!(
        !after_restore.contains(MARKER) && after_restore.contains("fn line_98() {}"),
        "read returned content that is no longer on disk after git checkout: {after_restore}"
    );
    let whole = read_whole(&mut aft, "freshness-read-restored-whole", &file, hashline);
    assert!(
        !whole.contains(MARKER) && whole.contains("fn line_98() {}"),
        "whole-file read returned content that is no longer on disk: {whole}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn read_after_git_checkout_restore_returns_restored_bytes() {
    assert_read_sees_git_checkout_restore(false, false);
}

#[test]
fn read_after_git_checkout_restore_in_linked_worktree_returns_restored_bytes() {
    assert_read_sees_git_checkout_restore(true, false);
}

#[test]
fn hashline_read_after_git_checkout_restore_returns_restored_bytes() {
    assert_read_sees_git_checkout_restore(false, true);
}

#[test]
fn hashline_read_after_git_checkout_restore_in_linked_worktree_returns_restored_bytes() {
    assert_read_sees_git_checkout_restore(true, true);
}

/// Rewrite the file with different bytes of the same length and put the old
/// modification time back, so a cache keyed on (path, size, mtime) could not
/// tell the two versions apart.
fn assert_read_sees_same_size_same_mtime_rewrite(hashline: bool) {
    let dir = crate::test_helpers::tempdir();
    let (root, file) = committed_fixture(dir.path(), false);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root, hashline);

    let first = read_window(&mut aft, "same-size-read-first", &file, hashline);
    assert!(first.contains("fn line_50() {}"), "{first}");
    let whole_first = read_whole(&mut aft, "same-size-read-first-whole", &file, hashline);
    assert!(whole_first.contains("fn line_50() {}"), "{whole_first}");

    let before = fs::metadata(&file).expect("stat before rewrite");
    let rewritten = original_source().replace("fn line_50() {}", "fn LINE_50() {}");
    assert_eq!(
        rewritten.len() as u64,
        before.len(),
        "rewrite must keep the size"
    );
    fs::write(&file, &rewritten).expect("same-size rewrite");
    filetime::set_file_mtime(
        &file,
        filetime::FileTime::from_last_modification_time(&before),
    )
    .expect("restore mtime");
    let after = fs::metadata(&file).expect("stat after rewrite");
    assert_eq!(after.len(), before.len());
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());

    let second = read_window(&mut aft, "same-size-read-second", &file, hashline);
    assert!(
        second.contains("fn LINE_50() {}") && !second.contains("fn line_50() {}"),
        "ranged read served the previous version of a same-size, same-mtime file: {second}"
    );
    let whole_second = read_whole(&mut aft, "same-size-read-second-whole", &file, hashline);
    assert!(
        whole_second.contains("fn LINE_50() {}") && !whole_second.contains("fn line_50() {}"),
        "whole-file read served the previous version of a same-size, same-mtime file: {whole_second}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn read_after_same_size_same_mtime_rewrite_returns_new_bytes() {
    assert_read_sees_same_size_same_mtime_rewrite(false);
}

#[test]
fn hashline_read_after_same_size_same_mtime_rewrite_returns_new_bytes() {
    assert_read_sees_same_size_same_mtime_rewrite(true);
}

/// The test daemon's file watcher is disabled, so no change event for this
/// write can ever arrive. The read must still see the new bytes.
#[test]
fn read_after_external_write_without_watcher_event_returns_new_bytes() {
    let dir = crate::test_helpers::tempdir();
    let (root, file) = committed_fixture(dir.path(), false);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root, false);

    let first = read_window(&mut aft, "external-read-first", &file, false);
    assert!(!first.contains(MARKER), "{first}");

    let external = original_source().replace(
        "fn line_60() {}",
        &format!("fn line_60() {{}} // {MARKER} (external)"),
    );
    fs::write(&file, external).expect("external write");

    let second = read_window(&mut aft, "external-read-second", &file, false);
    assert!(
        second.contains(MARKER),
        "read did not see an external write that no watcher event announced: {second}"
    );

    assert!(aft.shutdown().success());
}
