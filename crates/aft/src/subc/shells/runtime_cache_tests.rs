use super::*;
use crate::bash_background::buffer::ARTIFACT_OPENS;
use crate::bash_background::persistence::{read_task_at, write_task_at};

#[test]
fn shells_v1_cached_collection_matches_fresh_rows_and_watch_frames() {
    use std::io::Write;
    let (dir, ctx, executor, app) = status_context();
    let db = app.db().unwrap();
    let mut tasks = Vec::new();
    for index in 0..4 {
        let (_, _, task) = captured(dir.path(), b"before\n", index == 1, true);
        let metadata = read_task_at(&task).unwrap();
        crate::db::bash_tasks::upsert_bash_task(
            &db.lock().unwrap(),
            &metadata.to_bash_task_row("test", &task.paths).unwrap(),
        )
        .unwrap();
        if index < 2 {
            ctx.bash_background()
                .insert_shell_fixture(metadata, task.paths.clone())
                .unwrap();
        }
        tasks.push(task);
    }
    let primed = collect_at_revision(&executor, Some(&db), true, DbFacts::default(), 0);
    let mut cached = collect_at_revision(&executor, Some(&db), true, primed.db, 0).db;
    let mut legacy = DbFacts::default();
    let mut old_state = state::State::new("same-epoch".into());
    let mut new_state = state::State::new("same-epoch".into());
    let filter = Filter {
        agent_ids: None,
        include_workers: Some(true),
        cursor: None,
    };
    for step in 0..9 {
        match step {
            1 => {} // A watchdog wake with no task changes must do no opens.
            2 => {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&tasks[0].paths.stderr)
                    .unwrap()
                    .write_all(b"stderr\n")
                    .unwrap();
            }
            3 => {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&tasks[1].paths.pty)
                    .unwrap()
                    .write_all(b"pty\n")
                    .unwrap();
            }
            4 => {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&tasks[2].paths.stdout)
                    .unwrap()
                    .write_all(b"stored\n")
                    .unwrap();
            }
            5 => {
                let mut metadata = read_task_at(&tasks[0]).unwrap();
                metadata.status = BgTaskStatus::Running;
                metadata.finished_at = None;
                metadata.exit_code = None;
                write_task_at(&tasks[0], &metadata).unwrap();
                ctx.bash_background()
                    .insert_shell_fixture(metadata, tasks[0].paths.clone())
                    .unwrap();
            }
            6 => {
                let mut metadata = read_task_at(&tasks[0]).unwrap();
                metadata.mark_terminal(BgTaskStatus::Completed, Some(7), None);
                write_task_at(&tasks[0], &metadata).unwrap();
                ctx.bash_background()
                    .insert_shell_fixture(metadata, tasks[0].paths.clone())
                    .unwrap();
            }
            7 => {
                std::fs::remove_file(&tasks[3].paths.stdout).unwrap();
            }
            8 => {
                std::fs::write(&tasks[3].paths.stdout, b"replaced\n").unwrap();
            }
            _ => {}
        }
        ARTIFACT_OPENS.with(|count| count.set(0));
        let actual = collect_at_revision(&executor, Some(&db), true, cached, step);
        let opens = ARTIFACT_OPENS.with(|count| count.get());
        if step == 1 {
            assert_eq!(opens, 0, "unchanged retained tasks were reopened");
        }
        if step == 2 || step == 3 {
            assert_eq!(
                opens,
                if step == 3 { 1 } else { 2 },
                "an unrelated task was sampled"
            );
        }
        let fresh = super::legacy::collect_legacy(&executor, Some(&db), true, legacy, step);
        let mut actual_rows = actual.rows.clone();
        let mut fresh_rows = fresh.rows.clone();
        actual_rows.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        fresh_rows.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        assert_eq!(
            serde_json::to_vec(&actual_rows).unwrap(),
            serde_json::to_vec(&fresh_rows).unwrap(),
            "rows at step {step}"
        );
        let old_sequence = old_state.sequence;
        let new_sequence = new_state.sequence;
        old_state.replace(fresh.rows, fresh.status);
        new_state.replace(actual.rows, actual.status);
        assert_eq!(
            serde_json::to_vec(&old_state.replay(&filter, true, old_sequence)).unwrap(),
            serde_json::to_vec(&new_state.replay(&filter, true, new_sequence)).unwrap(),
            "watch frames at step {step}"
        );
        cached = actual.db;
        legacy = fresh.db;
    }
}

#[test]
fn shells_v1_saved_rows_follow_committed_store_changes_not_watch_revisions() {
    let (dir, _ctx, executor, app) = status_context();
    let db = app.db().unwrap();
    let first = collect_at_store_revision(&executor, Some(&db), true, DbFacts::default(), 10, 1);
    db.lock().unwrap().trace(Some(trace_facts_sql));
    FACTS_SQL_TRACE.with(|trace| trace.borrow_mut().clear());
    let unchanged = collect_at_store_revision(&executor, Some(&db), true, first.db, 11, 1);
    assert!(
        FACTS_SQL_TRACE.with(|trace| trace
            .borrow()
            .iter()
            .all(|sql| !sql.contains("FROM bash_tasks"))),
        "a watch revision reloaded saved task rows"
    );
    // A second connection deliberately bypasses the Rust writer API, just as
    // another daemon process would. Only SQLite's data_version can detect it.
    let other = crate::db::open(&dir.path().join("aft.db")).unwrap();
    let (_, _, task) = captured(dir.path(), b"restored\n", false, true);
    let metadata = read_task_at(&task).unwrap();
    let row = metadata.to_bash_task_row("test", &task.paths).unwrap();
    let tx = other.unchecked_transaction().unwrap();
    tx.execute("INSERT INTO bash_tasks(harness,session_id,task_id,project_key,command,cwd,status,started_at,completed_at,stdout_path,stderr_path,metadata) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)", rusqlite::params![row.harness,row.session_id,row.task_id,row.project_key,row.command,row.cwd,row.status,row.started_at,row.completed_at,row.stdout_path,row.stderr_path,row.metadata]).unwrap();
    let before_commit = collect_at_store_revision(&executor, Some(&db), false, unchanged.db, 11, 1);
    assert!(before_commit.rows.is_empty());
    tx.commit().unwrap();
    let committed = collect_at_store_revision(&executor, Some(&db), false, before_commit.db, 11, 1);
    assert!(committed.rows_refreshed);
    assert_eq!(committed.rows.len(), 1);
    db.lock().unwrap().trace(None);
}

#[test]
fn shells_v1_local_store_writer_generations_cover_insert_update_delete_and_rollback() {
    use crate::db::bash_tasks::*;
    let dir = tempfile::tempdir().unwrap();
    let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
    let (_, _, task) = captured(dir.path(), b"done\n", false, true);
    let mut row = read_task_at(&task)
        .unwrap()
        .to_bash_task_row("test", &task.paths)
        .unwrap();
    row.completion_delivered = true;
    row.metadata = r#"{"completion_delivered":false}"#.into();
    let changed = |write: &dyn Fn()| {
        let before = store_revision();
        write();
        assert!(store_revision() > before);
    };
    changed(&|| insert_bash_task_if_missing(&conn, &row).unwrap());
    changed(&|| upsert_bash_task(&conn, &row).unwrap());
    changed(&|| {
        checkpoint_bash_task_acknowledgement(&conn, "test", &row.session_id, &row.task_id).unwrap()
    });
    changed(&|| {
        assert_eq!(
            delete_delivered_terminal_bash_task(
                &conn,
                "test",
                &row.session_id,
                &row.task_id,
                "test"
            )
            .unwrap(),
            1
        );
    });
    upsert_bash_task(&conn, &row).unwrap();
    changed(&|| {
        assert_eq!(
            delete_bash_task(&conn, "test", &row.session_id, &row.task_id).unwrap(),
            1
        );
    });
    let tx = conn.unchecked_transaction().unwrap();
    changed(&|| upsert_bash_task(&tx, &row).unwrap());
    tx.rollback().unwrap();
    assert!(get_bash_task(&conn, "test", &row.session_id, &row.task_id)
        .unwrap()
        .is_none());
    row.completed_at = Some(1);
    std::fs::remove_dir_all(&task.paths.dir).unwrap();
    upsert_bash_task(&conn, &row).unwrap();
    changed(&|| {
        assert_eq!(
            prune_terminal_rows(&conn, i64::MAX / 2, 5).unwrap().removed,
            1
        );
    });
}

#[test]
fn shells_v1_production_task_store_writers_are_generation_tracked() {
    fn sources(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy();
            if path.is_dir() {
                if name != "tests" {
                    sources(&path, files);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && !name.ends_with("_tests.rs")
                && name != "tests.rs"
            {
                files.push(path);
            }
        }
    }
    fn visit(
        node: tree_sitter::Node<'_>,
        source: &str,
        path: &std::path::Path,
        writer: &regex::Regex,
        count: &mut usize,
    ) {
        let mut cursor = node.walk();
        let mut test_only = false;
        for child in node.named_children(&mut cursor) {
            let text = &source[child.byte_range()];
            if child.kind() == "attribute_item" {
                test_only |= text.contains("cfg(")
                    && text
                        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
                        .any(|word| word == "test")
                    && !text.contains("test-hooks");
                test_only |= text.contains("::test") || text == "#[test]";
                continue;
            }
            if !test_only {
                if matches!(child.kind(), "string_literal" | "raw_string_literal")
                    && writer.is_match(text)
                {
                    let mut parent = child.parent();
                    let mut tracked = false;
                    while let Some(ancestor) = parent {
                        let code = &source[ancestor.byte_range()];
                        if ancestor.kind() == "call_expression" {
                            if let Some(function) = ancestor.child_by_field_name("function") {
                                tracked |= source[function.byte_range()].rsplit("::").next()
                                    == Some("record_store_write");
                            }
                        }
                        if ancestor.kind() == "function_item" {
                            // Prepared retention deletes execute the statement
                            // later, once per candidate, rather than at prepare.
                            tracked |= code.contains("fn delete_prepared_terminal_rows(")
                                && code.contains("record_store_write(delete.execute(");
                            break;
                        }
                        parent = ancestor.parent();
                    }
                    assert!(
                        tracked,
                        "untracked bash_tasks writer at {}:{}: {text}",
                        path.display(),
                        child.start_position().row + 1
                    );
                    *count += 1;
                } else {
                    visit(child, source, path, writer, count);
                }
            }
            test_only = false;
        }
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let writer = regex::Regex::new(r"(?i)\b(?:INSERT\s+(?:OR\s+\w+\s+)?INTO|REPLACE\s+INTO|UPDATE|DELETE\s+FROM)\s+bash_tasks\b").unwrap();
    let mut files = Vec::new();
    sources(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let mut count = 0;
    for file in files {
        let source = std::fs::read_to_string(&file).unwrap();
        let tree = parser.parse(&source, None).unwrap();
        visit(tree.root_node(), &source, &file, &writer, &mut count);
    }
    assert!(
        count >= 9,
        "writer inventory did not visit known production SQL sites: {count}"
    );
}
