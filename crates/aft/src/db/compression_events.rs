pub use aft_db::db::compression_events::*;

#[cfg(test)]
#[allow(unused_imports)]
use parking_lot::Mutex;
#[cfg(test)]
#[allow(unused_imports)]
use rusqlite::{params, Connection};
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashMap;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(test)]
mod tests {
    use aft_db::db::compression_events::tests::*;

    use super::*;
    use crate::bash_background::persistence::retention::{
        prune_retention_once_observed, prune_retention_sweep, prune_retention_sweep_observed,
    };
    use tempfile::tempdir;

    #[test]
    fn retention_sweep_retries_contended_opening_count() {
        let dir = tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        insert_eligible_retention_task(&conn, "bash-0000000000000100");
        let db = std::sync::Arc::new(std::sync::Mutex::new(conn));
        let mut held = Some(db.lock().unwrap());
        let mut opening_attempts = 0usize;

        let outcome = prune_retention_sweep_observed(
            &db,
            crate::db::bash_tasks::TERMINAL_ROW_RETENTION_AGE_MS + 100,
            Some(&[]),
            |phase, attempt| {
                if phase == RetentionCountPhase::Opening {
                    opening_attempts = opening_attempts.max(attempt);
                    if attempt == 2 {
                        drop(held.take());
                    }
                }
            },
        )
        .unwrap();
        let RetentionSweepOutcome::Completed(sweep) = outcome else {
            panic!("opening contention abandoned the retention sweep: {outcome:?}");
        };

        assert!(opening_attempts >= 2);
        assert_eq!(sweep.bash_tasks_removed, 1);
        assert_eq!(sweep.remaining_eligible_rows, Some(0));
        assert_eq!(sweep.count_skip, None);
        // Lock hold times are reported, not asserted: with one row they measure
        // scheduler preemption on a loaded runner, not the sweep's batch size,
        // and a 100 ms bound failed CI on Linux while the retry worked.
        eprintln!(
            "retention opening retry contention: attempts={} worst_count_lock_us={} worst_lock_us={}",
            opening_attempts, sweep.worst_count_lock_micros, sweep.worst_lock_micros
        );
    }

    #[test]
    fn retention_sweep_waits_for_millisecond_mutex_holder() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        insert_eligible_retention_task(&conn, "bash-0000000000000100");
        let db = std::sync::Arc::new(std::sync::Mutex::new(conn));
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder_db = db.clone();
        let holder = std::thread::spawn(move || {
            let guard = holder_db.lock().unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(2));
            drop(guard);
        });
        locked_rx.recv().unwrap();
        // The production budget is 10 ms. A loaded CI runner can stretch the
        // holder's 2 ms hold past that, so this test widens the budget: what
        // it checks is that the sweep waits for a millisecond-scale holder at
        // all, which the old 256-yield loop never did (it gave up after about
        // 22 microseconds whatever the budget).
        RETENTION_COUNT_LOCK_RETRY_BUDGET_OVERRIDE
            .with(|budget| budget.set(Some(Duration::from_secs(5))));
        let started = Instant::now();
        let outcome = prune_retention_sweep_observed(
            &db,
            crate::db::bash_tasks::TERMINAL_ROW_RETENTION_AGE_MS + 100,
            Some(&[]),
            |phase, attempt| {
                if phase == RetentionCountPhase::Opening && attempt == 1 {
                    release_tx.send(()).unwrap();
                }
            },
        )
        .unwrap();
        holder.join().unwrap();
        let RetentionSweepOutcome::Completed(sweep) = outcome else {
            panic!("millisecond contention skipped the sweep: {outcome:?}");
        };
        assert!(
            started.elapsed() >= Duration::from_micros(200),
            "sweep did not wait"
        );
        assert_eq!(sweep.bash_tasks_removed, 1);
        assert_eq!(sweep.count_skip, None);
        eprintln!("millisecond holder: elapsed_us={} count_hold_us={} selection_hold_us={} mutation_hold_us={}", started.elapsed().as_micros(), sweep.worst_count_lock_micros, sweep.worst_selection_lock_micros, sweep.worst_mutation_lock_micros);
    }

    #[test]
    fn retention_sweep_reports_opening_lock_retry_exhaustion() {
        let dir = tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let db = std::sync::Arc::new(std::sync::Mutex::new(conn));
        let _held = db.lock().unwrap();

        let outcome = prune_retention_sweep(&db, RETENTION_AGE_MS + 100, Some(&[])).unwrap();
        let RetentionSweepOutcome::Skipped(skip) = outcome else {
            panic!("exhausted opening contention was not reported: {outcome:?}");
        };

        assert_eq!(
            skip.reason,
            RetentionSweepSkipReason::OpeningCountLockBudgetExhausted
        );
        assert!(skip.attempts >= 2);
        assert!(skip.waited_micros >= RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS);
        assert!(
            RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS * 5 < RETENTION_LOCK_BUDGET_MICROS,
            "count retry budget must remain far below the mutex hold budget"
        );
        assert_eq!(last_retention_sweep_skip_reason(&db), Some(skip.reason));
        eprintln!(
            "retention opening contention: attempts={} waited_us={} configured_retry_budget_us={}",
            skip.attempts, skip.waited_micros, RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS
        );
    }

    #[test]
    fn retention_sweep_reports_closing_lock_retry_exhaustion() {
        let dir = tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let db = std::sync::Arc::new(std::sync::Mutex::new(conn));
        let mut held = None;

        let outcome = prune_retention_sweep_observed(
            &db,
            RETENTION_AGE_MS + 100,
            Some(&[]),
            |phase, attempt| {
                if phase == RetentionCountPhase::Closing && attempt == 1 {
                    held = Some(db.lock().unwrap());
                }
            },
        )
        .unwrap();
        let RetentionSweepOutcome::Completed(sweep) = outcome else {
            panic!("sweep did not reach its closing count: {outcome:?}");
        };
        let skip = sweep.count_skip.expect("closing count skip");

        assert_eq!(sweep.remaining_eligible_rows, None);
        assert_eq!(
            skip.reason,
            RetentionSweepSkipReason::ClosingCountLockBudgetExhausted
        );
        assert!(skip.attempts >= 2);
        assert!(skip.waited_micros >= RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS);
        assert_eq!(last_retention_sweep_skip_reason(&db), Some(skip.reason));
        eprintln!(
            "retention closing contention: attempts={} waited_us={} configured_retry_budget_us={}",
            skip.attempts, skip.waited_micros, RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS
        );
        drop(held.take());
    }

    #[test]
    fn retention_releases_database_mutex_before_layout_stats() {
        let dir = tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        conn.execute(
            "INSERT INTO bash_tasks (
                harness, session_id, task_id, project_key, command, cwd, status,
                started_at, completed_at, completion_delivered
             ) VALUES ('opencode', 'session', 'bash-0000000000000001', 'project',
                       'true', '.', 'completed', 1, 1, 1)",
            [],
        )
        .unwrap();
        let db = std::sync::Arc::new(std::sync::Mutex::new(conn));
        let observed = std::sync::atomic::AtomicBool::new(false);

        let pass = prune_retention_once_observed(
            &db,
            crate::db::bash_tasks::TERMINAL_ROW_RETENTION_AGE_MS + 100,
            Some(&[]),
            || {
                let _guard = db
                    .try_lock()
                    .expect("database mutex held during layout stat phase");
                observed.store(true, Ordering::SeqCst);
            },
        )
        .unwrap()
        .unwrap();

        assert!(observed.load(Ordering::SeqCst));
        assert_eq!(pass.tick.bash_tasks.removed, 1);
    }
}
