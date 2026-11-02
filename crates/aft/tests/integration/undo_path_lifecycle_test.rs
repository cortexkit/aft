//! Undo and history for one path across the file's deletion, recreation, and
//! replacement.
//!
//! The undo store is keyed by path, so a path's stack can span several files
//! that lived there one after another. These tests drive the real binary
//! through each way a file can be removed or replaced (AFT's delete, `rm`,
//! AFT's move into the path, `mv` over the file, an editor's temp-file save)
//! and check two promises: undo never destroys content AFT did not write, and
//! `edit_history` marks where one file's life at the path ended.

use super::helpers::AftProcess;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).unwrap())
}

fn write(aft: &mut AftProcess, file: &Path, content: &str) {
    let response = send(
        aft,
        json!({"id": "write", "command": "write", "file": file.display().to_string(), "content": content}),
    );
    assert_eq!(response["success"], true, "write: {response:?}");
}

fn edit(aft: &mut AftProcess, file: &Path, find: &str, replacement: &str) {
    let response = send(
        aft,
        json!({
            "id": "edit",
            "command": "edit_match",
            "file": file.display().to_string(),
            "match": find,
            "replacement": replacement,
        }),
    );
    assert_eq!(response["success"], true, "edit: {response:?}");
}

fn undo_file(aft: &mut AftProcess, file: &Path) -> Value {
    let response = send(
        aft,
        json!({"id": "undo", "command": "undo", "file": file.display().to_string()}),
    );
    assert_eq!(response["success"], true, "undo: {response:?}");
    response
}

fn undo_operation(aft: &mut AftProcess) -> Value {
    let response = send(aft, json!({"id": "undo-op", "command": "undo"}));
    assert_eq!(response["success"], true, "operation undo: {response:?}");
    response
}

/// History entries newest first.
fn history(aft: &mut AftProcess, file: &Path) -> Vec<Value> {
    let response = send(
        aft,
        json!({"id": "history", "command": "edit_history", "file": file.display().to_string()}),
    );
    assert_eq!(response["success"], true, "history: {response:?}");
    response["entries"].as_array().unwrap().clone()
}

fn markers(entry: &Value) -> Vec<&str> {
    entry["markers"]
        .as_array()
        .unwrap_or_else(|| panic!("history entry has no markers: {entry:?}"))
        .iter()
        .map(|marker| marker.as_str().unwrap())
        .collect()
}

fn read(file: &Path) -> Option<String> {
    fs::read_to_string(file).ok()
}

/// Write A0, edit it to A1 then A2: three snapshots for the first file.
fn first_file_with_history(aft: &mut AftProcess, file: &Path) {
    write(aft, file, "A0\n");
    edit(aft, file, "A0", "A1");
    edit(aft, file, "A1", "A2");
}

/// Assert the history belongs to two files: the newest `newer` entries to the
/// current one, the rest labelled as a previous file.
fn assert_two_files(entries: &[Value], newer: usize) {
    for (index, entry) in entries.iter().enumerate() {
        let previous = index >= newer;
        assert_eq!(
            entry["previous_file"], previous,
            "entry {index} previous_file: {entries:#?}"
        );
        assert_eq!(
            entry["generation"],
            if previous { 0 } else { 1 },
            "entry {index} generation: {entries:#?}"
        );
    }
}

#[test]
fn aft_delete_then_recreate_labels_previous_file_and_undo_walks_the_timeline() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("a.txt");
    let mut aft = AftProcess::spawn();
    first_file_with_history(&mut aft, &file);
    let delete = send(
        &mut aft,
        json!({"id": "delete", "command": "delete_file", "files": [file.display().to_string()]}),
    );
    assert_eq!(delete["success"], true, "delete: {delete:?}");
    write(&mut aft, &file, "B0\n");
    edit(&mut aft, &file, "B0", "B1");

    let entries = history(&mut aft, &file);
    assert_eq!(entries.len(), 6, "{entries:#?}");
    assert_two_files(&entries, 2);
    assert!(markers(&entries[1]).contains(&"created"), "{entries:#?}");
    assert!(markers(&entries[2]).contains(&"deleted"), "{entries:#?}");

    // Every step reverses one change AFT itself made, so none needs to
    // preserve anything, and undoing the delete still brings A back.
    let mut states = Vec::new();
    for _ in 0..6 {
        let undo = undo_file(&mut aft, &file);
        assert!(
            undo.get("warning").is_none(),
            "unexpected warning: {undo:?}"
        );
        assert!(undo.get("external_change_checkpoint").is_none(), "{undo:?}");
        states.push(read(&file));
    }
    let expected = [
        Some("B0\n"),
        None,
        Some("A2\n"),
        Some("A1\n"),
        Some("A0\n"),
        None,
    ];
    assert_eq!(
        states,
        expected.map(|state| state.map(str::to_string)).to_vec()
    );
    assert!(aft.shutdown().success());
}

#[test]
fn rm_then_recreate_labels_previous_file() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("a.txt");
    let mut aft = AftProcess::spawn();
    first_file_with_history(&mut aft, &file);
    fs::remove_file(&file).unwrap();
    write(&mut aft, &file, "B0\n");
    edit(&mut aft, &file, "B0", "B1");

    let entries = history(&mut aft, &file);
    assert_eq!(entries.len(), 5, "{entries:#?}");
    assert_two_files(&entries, 2);
    let creation = markers(&entries[1]);
    assert!(creation.contains(&"created"), "{entries:#?}");
    assert!(
        creation.contains(&"changed_outside_aft_before"),
        "the rm AFT never saw must be marked: {entries:#?}"
    );

    assert_eq!(read(&file).as_deref(), Some("B1\n"));
    undo_file(&mut aft, &file);
    assert_eq!(read(&file).as_deref(), Some("B0\n"));
    undo_file(&mut aft, &file);
    assert_eq!(read(&file), None);
    // The path is empty, so restoring the previous file's content loses nothing.
    let undo = undo_file(&mut aft, &file);
    assert!(undo.get("external_change_checkpoint").is_none(), "{undo:?}");
    assert_eq!(read(&file).as_deref(), Some("A1\n"));
    assert!(aft.shutdown().success());
}

#[test]
fn aft_move_into_vacated_path_labels_previous_file() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("a.txt");
    let other = dir.path().join("b.txt");
    let mut aft = AftProcess::spawn();
    first_file_with_history(&mut aft, &file);
    // move_file refuses an existing destination, so the path is vacated first.
    fs::remove_file(&file).unwrap();
    fs::write(&other, "B0\n").unwrap();
    let moved = send(
        &mut aft,
        json!({
            "id": "move",
            "command": "move_file",
            "file": other.display().to_string(),
            "destination": file.display().to_string(),
        }),
    );
    assert_eq!(moved["success"], true, "move: {moved:?}");
    edit(&mut aft, &file, "B0", "B1");

    let entries = history(&mut aft, &file);
    assert_eq!(entries.len(), 5, "{entries:#?}");
    assert_two_files(&entries, 2);
    assert!(markers(&entries[1]).contains(&"created"), "{entries:#?}");
    assert!(aft.shutdown().success());
}

/// `mv b.txt a.txt` over a file AFT has edited, then one AFT edit of the new
/// content. Returns with a.txt holding B1.
fn rename_over_file_with_history(aft: &mut AftProcess, dir: &Path) -> std::path::PathBuf {
    let file = dir.join("a.txt");
    let other = dir.join("b.txt");
    first_file_with_history(aft, &file);
    fs::write(&other, "B0\n").unwrap();
    fs::rename(&other, &file).unwrap();
    edit(aft, &file, "B0", "B1");
    file
}

/// Restore a checkpoint by name, as `aft_safety restore name=<name>` does.
fn restore_checkpoint(aft: &mut AftProcess, name: &str) {
    let response = send(
        aft,
        json!({"id": "restore", "command": "restore_checkpoint", "name": name}),
    );
    assert_eq!(
        response["success"], true,
        "restore_checkpoint: {response:?}"
    );
}

/// The checkpoint an undo reply names, checked against the reply's warning.
fn external_change_checkpoint(reply: &Value, checkpoint_field: &Value, warning: &str) -> String {
    let name = checkpoint_field
        .as_str()
        .unwrap_or_else(|| panic!("the external change was not saved: {reply:?}"))
        .to_string();
    assert!(
        name.starts_with("external-change-a.txt-"),
        "unexpected checkpoint name {name}"
    );
    assert!(
        warning.contains(&format!("checkpoint '{name}'"))
            && warning.contains(&format!("aft_safety restore name={name}")),
        "warning must name the checkpoint and how to restore it: {reply:?}"
    );
    name
}

#[test]
fn rename_over_file_undo_saves_the_new_file_as_checkpoint_and_keeps_walking_back() {
    let dir = crate::test_helpers::tempdir();
    let mut aft = AftProcess::spawn();
    let file = rename_over_file_with_history(&mut aft, dir.path());

    let entries = history(&mut aft, &file);
    assert_eq!(
        markers(&entries[0]),
        vec!["changed_outside_aft_before"],
        "{entries:#?}"
    );

    let first = undo_file(&mut aft, &file);
    assert!(first.get("warning").is_none(), "{first:?}");
    assert_eq!(read(&file).as_deref(), Some("B0\n"));

    // The next entry was A's last edit, but the path now holds B0, which AFT
    // never wrote there. Undo saves B0 as a checkpoint, then restores A1.
    let second = undo_file(&mut aft, &file);
    assert_eq!(read(&file).as_deref(), Some("A1\n"));
    let checkpoint = external_change_checkpoint(
        &second,
        &second["external_change_checkpoint"],
        second["warning"].as_str().unwrap_or_default(),
    );
    let entries = history(&mut aft, &file);
    assert_eq!(
        entries[0]["external_change_checkpoint"],
        checkpoint.as_str(),
        "history marks where the external change was saved: {entries:#?}"
    );
    assert!(
        markers(&entries[0]).contains(&"external_change_checkpoint"),
        "{entries:#?}"
    );

    // The saved content is not an undo step: repeated undo keeps walking back
    // through AFT's own history to the start, never returning B0.
    let third = undo_file(&mut aft, &file);
    assert!(third.get("warning").is_none(), "{third:?}");
    assert_eq!(read(&file).as_deref(), Some("A0\n"));
    let fourth = undo_file(&mut aft, &file);
    assert!(fourth.get("warning").is_none(), "{fourth:?}");
    assert_eq!(read(&file), None, "undoing the creation removes the file");

    restore_checkpoint(&mut aft, &checkpoint);
    assert_eq!(
        read(&file).as_deref(),
        Some("B0\n"),
        "B0 must be recoverable from the checkpoint"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn rename_over_file_operation_undo_saves_the_new_file_and_keeps_walking_back() {
    let dir = crate::test_helpers::tempdir();
    let mut aft = AftProcess::spawn();
    let file = rename_over_file_with_history(&mut aft, dir.path());

    undo_operation(&mut aft);
    assert_eq!(read(&file).as_deref(), Some("B0\n"));
    let second = undo_operation(&mut aft);
    assert_eq!(read(&file).as_deref(), Some("A1\n"));
    let warnings = second["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{second:?}");
    let checkpoint = external_change_checkpoint(
        &second,
        &second["external_change_checkpoint"],
        warnings[0].as_str().unwrap(),
    );
    assert_eq!(
        second["restored"][0]["external_change_checkpoint"],
        checkpoint.as_str(),
        "{second:?}"
    );

    let third = undo_operation(&mut aft);
    assert!(
        third.get("external_change_checkpoint").is_none(),
        "{third:?}"
    );
    assert_eq!(read(&file).as_deref(), Some("A0\n"));
    undo_operation(&mut aft);
    assert_eq!(read(&file), None);

    restore_checkpoint(&mut aft, &checkpoint);
    assert_eq!(
        read(&file).as_deref(),
        Some("B0\n"),
        "B0 must be recoverable from the checkpoint"
    );
    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn recursive_delete_undo_saves_a_file_recreated_outside_aft_before_restoring_the_tree() {
    let dir = crate::test_helpers::tempdir();
    let tree = dir.path().join("tree");
    fs::create_dir_all(tree.join("empty")).unwrap();
    let file = tree.join("a.txt");
    fs::write(&file, "A0\n").unwrap();
    fs::hard_link(&file, tree.join("a-link.txt")).unwrap();
    std::os::unix::fs::symlink("a.txt", tree.join("a-symlink")).unwrap();
    let mut aft = AftProcess::spawn();
    let delete = send(
        &mut aft,
        json!({
            "id": "delete-tree",
            "command": "delete_file",
            "file": tree.display().to_string(),
            "recursive": true,
        }),
    );
    assert_eq!(delete["success"], true, "delete: {delete:?}");

    // Outside AFT, the tree comes back with new content at one of its paths.
    fs::create_dir(&tree).unwrap();
    fs::write(&file, "NEW\n").unwrap();

    // Undo goes through the same checkpoint-first path as any other undo:
    // the new content is saved before the deleted tree is restored over it.
    let undo = undo_operation(&mut aft);
    let warnings = undo["warnings"].as_array().unwrap();
    let warning = warnings
        .iter()
        .filter_map(Value::as_str)
        .find(|warning| warning.contains("checkpoint"))
        .unwrap_or_default();
    let checkpoint =
        external_change_checkpoint(&undo, &undo["external_change_checkpoint"], warning);
    assert_eq!(read(&file).as_deref(), Some("A0\n"));
    assert!(tree.join("empty").is_dir());
    assert_eq!(
        fs::read_link(tree.join("a-symlink")).unwrap(),
        Path::new("a.txt")
    );
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(&file).unwrap().ino(),
            fs::metadata(tree.join("a-link.txt")).unwrap().ino(),
            "the hard link is relinked"
        );
    }

    restore_checkpoint(&mut aft, &checkpoint);
    assert_eq!(
        read(&file).as_deref(),
        Some("NEW\n"),
        "the content created outside AFT must be recoverable"
    );
    assert!(aft.shutdown().success());
}

/// Save `content` the way many editors do: write a sibling temp file and
/// rename it over the original, which gives the path a new inode.
fn editor_atomic_save(file: &Path, content: &str) {
    let temp = file.with_extension("txt.swp");
    fs::write(&temp, content).unwrap();
    fs::rename(&temp, file).unwrap();
}

#[test]
fn editor_atomic_save_with_unchanged_content_undoes_normally() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("a.txt");
    let mut aft = AftProcess::spawn();
    first_file_with_history(&mut aft, &file);
    editor_atomic_save(&file, "A2\n");

    let undo = undo_file(&mut aft, &file);
    assert!(undo.get("warning").is_none(), "{undo:?}");
    assert!(undo.get("external_change_checkpoint").is_none(), "{undo:?}");
    assert_eq!(read(&file).as_deref(), Some("A1\n"));
    assert_eq!(history(&mut aft, &file).len(), 2);
    assert!(aft.shutdown().success());
}

#[test]
fn editor_atomic_save_with_new_content_is_saved_before_undo() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("a.txt");
    let mut aft = AftProcess::spawn();
    first_file_with_history(&mut aft, &file);
    editor_atomic_save(&file, "A2 plus editor work\n");

    let undo = undo_file(&mut aft, &file);
    let checkpoint = external_change_checkpoint(
        &undo,
        &undo["external_change_checkpoint"],
        undo["warning"].as_str().unwrap_or_default(),
    );
    assert_eq!(read(&file).as_deref(), Some("A1\n"));
    undo_file(&mut aft, &file);
    assert_eq!(read(&file).as_deref(), Some("A0\n"));
    restore_checkpoint(&mut aft, &checkpoint);
    assert_eq!(read(&file).as_deref(), Some("A2 plus editor work\n"));
    assert!(aft.shutdown().success());
}

#[test]
fn undoing_aft_own_edits_reports_no_external_change() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("a.txt");
    let mut aft = AftProcess::spawn();
    first_file_with_history(&mut aft, &file);

    for expected in [Some("A1\n"), Some("A0\n"), None] {
        let undo = undo_file(&mut aft, &file);
        assert!(
            undo.get("warning").is_none(),
            "undo of AFT's own edit must not claim an external change: {undo:?}"
        );
        assert!(undo.get("external_change_checkpoint").is_none(), "{undo:?}");
        assert_eq!(read(&file).as_deref(), expected);
    }
    assert!(aft.shutdown().success());
}

/// Only file contents are checked, so this holds whatever the reply looks
/// like: after the path was changed outside AFT, repeated undo must step back
/// through every AFT change to the file's start and never return to the
/// externally written content.
#[test]
fn repeated_undo_past_an_external_change_reaches_the_oldest_state() {
    for operation_undo in [false, true] {
        let dir = crate::test_helpers::tempdir();
        let mut aft = AftProcess::spawn();
        let file = rename_over_file_with_history(&mut aft, dir.path());

        let mut states = Vec::new();
        for _ in 0..4 {
            if operation_undo {
                undo_operation(&mut aft);
            } else {
                undo_file(&mut aft, &file);
            }
            states.push(read(&file));
        }
        let expected = [Some("B0\n"), Some("A1\n"), Some("A0\n"), None];
        assert_eq!(
            states,
            expected.map(|state| state.map(str::to_string)).to_vec(),
            "operation_undo={operation_undo}"
        );
        assert!(aft.shutdown().success());
    }
}
