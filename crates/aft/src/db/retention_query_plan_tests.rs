use super::*;
use aft_db::db::bash_tasks::query_plan_tests::*;
use rusqlite::ffi;
use std::ffi::{c_int, c_void, CStr};

#[test]
fn retention_and_hot_query_plans_on_unanalyzed_history() {
    use crate::bash_background::persistence::retention::prune_retention_tick;
    use crate::db::compression_events::*;
    let mut fixture = Fixture::new();
    fixture
        .conn
        .execute_batch(
            "INSERT INTO bash_pattern_watches
           (harness,session_id,task_id,watch_id,pattern_kind,pattern,created_at)
         VALUES ('opencode','s1','t1','w','literal','x',0);",
        )
        .unwrap();
    let (_, statements) = record(&mut fixture.conn, |conn| {
        terminal_rows_eligible_count(conn, NOW).unwrap();
        let mut row = get_bash_task(conn, "opencode", "s65", "t65")
            .unwrap()
            .unwrap();
        row.pid = Some(123);
        upsert_bash_task(conn, &row).unwrap();
        // Completion acknowledgements issue this point update directly from
        // the task registry rather than through the database helper module.
        conn.execute(
            "UPDATE bash_tasks SET completion_delivered = 1
             WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3
               AND status IN ('completed', 'failed', 'killed', 'timed_out', 'fate_unknown')",
            params!["opencode", "s65", "t65"],
        )
        .unwrap();
        assert_eq!(
            list_bash_task_process_ids(conn, &["t65".into(), "missing".into()])
                .unwrap()
                .len(),
            1
        );
        find_bash_task_session(conn, "opencode", "t65").unwrap();
        list_bash_tasks_for_session(conn, "opencode", "s65").unwrap();
        list_replayable_bash_tasks_for_project(conn, "opencode", "p65").unwrap();
        find_bash_task_for_project(conn, "opencode", "p65", "t65").unwrap();
        delete_delivered_terminal_bash_task(conn, "opencode", "s999", "t3999", "plan audit")
            .unwrap();
        delete_bash_task(conn, "opencode", "s998", "t3998").unwrap();
        let cache = CompressionAggregateCache::default();
        cache
            .aggregates_for_session(conn, "opencode", "p65", "s65")
            .unwrap();
        let event = CompressionEventRow {
            harness: "opencode",
            session_id: Some("s65"),
            project_key: "p65",
            tool: "bash",
            task_id: Some("new"),
            compressor: "test",
            original_bytes: 100,
            compressed_bytes: 50,
            original_tokens: 20,
            compressed_tokens: 10,
            created_at: NOW,
        };
        let id = insert_compression_event(conn, &event).unwrap().unwrap();
        cache.record_successful_insert(conn, &event, id);
        // Use a valid persisted-task id so the real selection/layout path also
        // reaches the delete; the synthetic history ids are not layout names.
        let mut old = row.clone();
        old.task_id = "bash-0000000000000000".into();
        old.completed_at = Some(0);
        old.pid = None;
        upsert_bash_task(conn, &old).unwrap();
        // Two ticks exercise both the initial and persisted cursor paths.
        for _ in 0..2 {
            prune_retention_tick(conn, NOW).unwrap();
        }
    });
    let mut unique = std::collections::BTreeMap::new();
    for statement in statements {
        unique
            .entry(statement.template.clone())
            .or_insert(statement);
    }
    println!(
        "SQLite {}: {TASK_HISTORY_ROWS} tasks, {EVENT_HISTORY_ROWS} events, no ANALYZE; {} distinct statements",
        rusqlite::version(),
        unique.len()
    );
    let mut process_lookup = None;
    let mut session_aggregate = None;
    for statement in unique.values() {
        let plan = explain(&fixture.conn, &statement.sql);
        println!(
            "SQL: {}\nPLAN: {plan}\nVM_STEP: {}",
            statement
                .template
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            statement.vm_steps
        );
        if plan.is_empty() {
            // INSERT/UPSERT and transaction statements have no EQP rows. Their
            // bytecode still exposes unique-index and rowid conflict probes.
            let opcodes = fixture
                .conn
                .prepare(&format!("EXPLAIN {}", statement.sql))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            println!(
                "KEY OPCODES: {:?}",
                opcodes
                    .iter()
                    .filter(|op| matches!(
                        op.as_str(),
                        "NoConflict" | "NotExists" | "SeekRowid" | "Rewind" | "Next"
                    ))
                    .collect::<Vec<_>>()
            );
        }
        if statement
            .template
            .trim_start()
            .starts_with("SELECT task_id, pid, pgid, started_at")
        {
            process_lookup = Some((plan, statement.vm_steps));
        } else if statement.template.contains("SELECT SUM(events)")
            && statement.template.contains("AND session_id = ?3")
        {
            session_aggregate = Some((plan, statement.vm_steps));
        }
    }
    let (plan, steps) = process_lookup.expect("production process lookup must be observed");
    let (session_plan, session_steps) =
        session_aggregate.expect("production session aggregate must be observed");
    assert!(plan.contains("task_id=?") && steps > 0 && steps < 500
        && session_plan.contains("(harness=? AND session_id=?)") && session_steps > 0 && session_steps < 10000,
        "hot queries must seek task/session keys: process VM_STEP={steps} plan={plan}; session VM_STEP={session_steps} plan={session_plan}");
}
