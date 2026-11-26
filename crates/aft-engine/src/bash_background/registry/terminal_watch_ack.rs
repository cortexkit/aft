use super::*;
use crate::db::bash_watches;

type Ticket = super::super::store_writes::Ticket;

struct PendingAck {
    task: Arc<BgTask>,
    sequence: u64,
    ticket: Ticket,
}

#[derive(Default)]
pub(super) struct DeferredWatchAcks(Mutex<HashMap<String, PendingAck>>);

impl BgTaskRegistry {
    /// The owning session already gets the outcome in its watch reply. Persist
    /// that fact in the task file before removing any queued completion, but
    /// leave SQLite to the watchdog: a foreign writer must not delay the reply.
    pub(crate) fn ack_terminal_watch_reply(&self, task_id: &str, session_id: &str) {
        let Some(task) = self.task_for_session(task_id, session_id) else {
            return;
        };
        let Ok(mut state) = task.lock_state() else {
            return;
        };
        if !state.metadata.is_terminal() || state.metadata.completion_delivered {
            return;
        }
        let order = &task.db_write_order;
        let mut json_writes = order.lock_json_writes();
        let updated = resolve_task_layout(&task.paths.session_dir, task_id).and_then(|layout| {
            update_task_at(&layout, |disk| {
                if task.terminal_persist_dirty.load(Ordering::SeqCst) {
                    *disk = state.metadata.clone();
                }
                disk.completion_delivered = true;
            })
        });
        let Ok(metadata) = updated else {
            // Without a durable acknowledgement the completion must stay pending.
            return;
        };
        *json_writes += 1;
        let deferred = self.task_row_ticket(&task.paths, &metadata).map(|ticket| {
            let sequence = order.captured.fetch_add(1, Ordering::SeqCst) + 1;
            self.inner.deferred_watch_acks.insert(PendingAck {
                task: Arc::clone(&task),
                sequence,
                ticket,
            });
        });
        task.drain_reply
            .publish_metadata(&metadata, task.hard_kill_renewable);
        state.metadata = metadata;
        drop(json_writes);
        drop(state);
        self.clear_task_watch_state(task_id);
        let _ = self.remove_pending_completion(task_id);
        self.notify_completion_delivered(task_id);
        self.refill_completion_queue();
        if deferred.is_some() {
            self.start_watchdog();
        }
    }

    pub(in crate::bash_background) fn flush_deferred_watch_acks(&self) {
        let queue = &self.inner.deferred_watch_acks;
        let work = queue
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|ack| (Arc::clone(&ack.task), ack.sequence))
            .collect::<Vec<_>>();
        for (task, sequence) in work {
            match self.try_flush_watch_ack(&task) {
                Ok(true) => queue.finish(&task.task_id, sequence),
                Ok(false) => {}
                Err(error) => crate::slog_warn!(
                    "terminal watch acknowledgement for {} will retry: {error}",
                    task.task_id
                ),
            }
        }
    }

    /// Returns false when an ordered write could not be attempted without waiting.
    fn try_flush_watch_ack(&self, task: &BgTask) -> Result<bool, Box<dyn std::error::Error>> {
        // Share the ordinary row flush fence with writers that can reset
        // completion_delivered, such as promoting a foreground task to the
        // background. Read the latest file, not the snapshot from reply time.
        let order = &task.db_write_order;
        let Ok(mut written) = order.written.try_lock() else {
            return Ok(false);
        };
        let (metadata, sequence) = {
            let Ok(_json_writes) = order.json_writes.try_lock() else {
                return Ok(false);
            };
            let layout = match resolve_task_layout(&task.paths.session_dir, &task.task_id) {
                Ok(layout) => layout,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                Err(error) => return Err(error.into()),
            };
            (
                read_task_at(&layout)?,
                order.captured.load(Ordering::SeqCst),
            )
        };
        if !metadata.is_terminal() || !metadata.completion_delivered {
            // Promotion opened a new notification obligation.
            return Ok(true);
        }
        let Some((harness, pool)) = self.db_harness_and_pool_for_task(task) else {
            return Ok(false);
        };
        let row = metadata.to_bash_task_row(&harness, &task.paths)?;
        let Ok(conn) = pool.try_lock() else {
            return Ok(false);
        };
        // A foreign writer must not stall the shared watchdog or keep the
        // connection mutex held until SQLite's default busy wait expires.
        conn.busy_timeout(Duration::ZERO)?;
        let result = (|| {
            let tx = conn.unchecked_transaction()?;
            crate::db::bash_tasks::upsert_bash_task(&tx, &row)?;
            ack_watch_rows(&tx, &harness, &task.session_id, &task.task_id, true, true)?;
            tx.commit()
        })();
        conn.busy_timeout(crate::db::STEADY_BUSY_TIMEOUT)?;
        match result {
            Ok(()) => {
                *written = (*written).max(sequence);
                let mut pending = order
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if pending
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.seq <= sequence)
                {
                    if let Some(ticket) = pending.take().and_then(|snapshot| snapshot.ticket) {
                        ticket.resolved();
                    }
                }
                Ok(true)
            }
            Err(error) if crate::db::is_busy_error(&error) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Task-wide ack: once-watches (and any terminal-task watches)
    /// are dropped after delivery is confirmed; sticky watches clear pending only.
    pub(super) fn ack_persisted_watches_for_task(
        &self,
        session_id: &str,
        task_id: &str,
        task_terminal: bool,
    ) {
        let Some((harness, pool)) = self.db_harness_and_pool_for_task_id(session_id, task_id)
        else {
            return;
        };
        let Ok(conn) = pool.lock() else {
            return;
        };
        let _ = ack_watch_rows(&conn, &harness, session_id, task_id, task_terminal, false);
    }
}

impl DeferredWatchAcks {
    pub(super) fn suppresses(&self, row: &bash_watches::BashPatternWatchRow) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&row.task_id)
            .is_some_and(|ack| ack.task.session_id == row.session_id && !is_retry_gap(row))
    }

    fn insert(&self, ack: PendingAck) {
        let mut pending = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(old) = pending.insert(ack.task.task_id.clone(), ack) {
            old.ticket.resolved();
        }
    }

    fn finish(&self, task_id: &str, sequence: u64) {
        let mut pending = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .get(task_id)
            .is_some_and(|ack| ack.sequence == sequence)
        {
            if let Some(ack) = pending.remove(task_id) {
                ack.ticket.resolved();
            }
        }
    }
}

fn ack_watch_rows(
    conn: &rusqlite::Connection,
    harness: &str,
    session_id: &str,
    task_id: &str,
    terminal: bool,
    retry_on_error: bool,
) -> rusqlite::Result<()> {
    for mut row in
        bash_watches::list_bash_pattern_watches_for_task(conn, harness, session_id, task_id)?
    {
        // A task acknowledgement carries no watch version. Leave warnings
        // about dropped watch output pending until their exact delivery is acknowledged.
        if is_retry_gap(&row) {
            continue;
        }
        let result = if terminal || (row.once && !row.scanning) {
            bash_watches::delete_bash_pattern_watch(
                conn,
                harness,
                session_id,
                task_id,
                &row.watch_id,
            )
            .map(|_| ())
        } else if row.pending_match {
            row.pending_match = false;
            row.match_text = None;
            row.match_offset = None;
            row.match_context = None;
            bash_watches::upsert_bash_pattern_watch(conn, &row)
        } else {
            continue;
        };
        if retry_on_error {
            result?;
        }
    }
    Ok(())
}

fn is_retry_gap(row: &bash_watches::BashPatternWatchRow) -> bool {
    row.pending_match
        && row
            .match_batch
            .as_deref()
            .and_then(|batch| {
                serde_json::from_str::<super::super::watch_lines::LineBatch>(batch).ok()
            })
            .is_some_and(|batch| batch.retry_gap.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash_background::persistence::read_task;
    use crate::context::AppContext;
    use crate::protocol::Response;
    use std::cell::Cell;

    const SESSION: &str = "session";
    const HARNESS: &str = "opencode";

    struct Fixture {
        dir: tempfile::TempDir,
        ctx: AppContext,
        pool: Arc<Mutex<TrackedConnection>>,
        task: Arc<BgTask>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let ctx = AppContext::new(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config::default(),
            );
            let registry = ctx.bash_background();
            registry.set_harness(Harness::Opencode);
            let pool = Arc::new(Mutex::new(
                crate::db::open(&dir.path().join("aft.db")).unwrap(),
            ));
            registry.set_db_pool(Arc::clone(&pool));
            // Drive the deferred worker explicitly so a crash can be modeled
            // before any deferred SQLite work runs.
            registry
                .inner
                .watchdog_started
                .store(true, Ordering::SeqCst);
            let (_, task) = super::super::tests::insert_terminal_piped_task(
                registry, &dir, "true", "done\n", "", false,
            );
            let metadata = task.lock_state().unwrap().metadata.clone();
            registry.dual_write_task(&task.paths, &metadata);
            registry.enqueue_completion_if_needed(&metadata, Some(&task.paths), false);
            assert!(registry.has_completions_for_session(Some(SESSION)));
            Self {
                dir,
                ctx,
                pool,
                task,
            }
        }

        fn registry(&self) -> &BgTaskRegistry {
            self.ctx.bash_background()
        }

        fn sqlite_ack(&self) -> bool {
            crate::db::bash_tasks::get_bash_task(
                &self.pool.lock().unwrap(),
                HARNESS,
                SESSION,
                &self.task.task_id,
            )
            .unwrap()
            .unwrap()
            .completion_delivered
        }

        fn finalize(&self) {
            let mut response = Response::success(
                "watch",
                serde_json::json!({
                    "task_id": self.task.task_id,
                    "status": "completed",
                }),
            );
            let mut text = "done\n".to_owned();
            crate::response_finalize::finalize_tool_response(
                &mut response,
                &mut text,
                &self.ctx,
                SESSION,
                "bash_watch",
                true,
            );
            assert_eq!(text, "done\n");
            assert!(response.data.get("bg_completions").is_none());
            assert!(!self.registry().has_completions_for_session(Some(SESSION)));
            assert!(
                read_task(&self.task.paths.json)
                    .unwrap()
                    .completion_delivered
            );
        }
    }

    thread_local! {
        static BUSY_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    fn refuse_busy(_: i32) -> bool {
        BUSY_CALLS.with(|calls| calls.set(calls.get() + 1));
        false
    }

    #[test]
    fn terminal_watch_ack_finalizes_while_sqlite_writer_holds_lock_then_retries() {
        let f = Fixture::new();
        let db_path = f.dir.path().join("aft.db");
        let foreign = rusqlite::Connection::open(db_path).unwrap();
        let watch = bash_watches::BashPatternWatchRow {
            harness: HARNESS.into(),
            session_id: SESSION.into(),
            task_id: f.task.task_id.clone(),
            watch_id: "other-watch".into(),
            pattern_kind: "substring".into(),
            pattern: "done".into(),
            once: true,
            each_line: false,
            match_batch: None,
            created_at: 1,
            stdout_offset: 0,
            stderr_offset: 0,
            pty_offset: 0,
            scanning: false,
            pending_match: true,
            match_text: Some("done".into()),
            match_offset: Some(0),
            match_context: None,
        };
        let watch_rows = || {
            bash_watches::list_bash_pattern_watches_for_task(
                &f.pool.lock().unwrap(),
                HARNESS,
                SESSION,
                &f.task.task_id,
            )
            .unwrap()
        };
        bash_watches::upsert_bash_pattern_watch(&f.pool.lock().unwrap(), &watch).unwrap();
        f.pool
            .lock()
            .unwrap()
            .busy_handler(Some(refuse_busy))
            .unwrap();
        BUSY_CALLS.with(|calls| calls.set(0));
        foreign.execute_batch("BEGIN IMMEDIATE").unwrap();

        f.finalize();
        assert_eq!(
            BUSY_CALLS.with(Cell::get),
            0,
            "finalize attempted a contended SQL write"
        );
        assert!(f
            .registry()
            .pending_pattern_matches_for_session(SESSION)
            .is_empty());
        f.registry().flush_deferred_watch_acks();
        assert!(!f.sqlite_ack());
        assert_eq!(watch_rows().len(), 1);

        foreign.execute_batch("ROLLBACK").unwrap();
        f.registry().flush_deferred_watch_acks();
        assert!(f.sqlite_ack());
        assert!(watch_rows().is_empty());
        assert!(f
            .registry()
            .inner
            .deferred_watch_acks
            .0
            .lock()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn terminal_watch_ack_replay_prefers_json_to_stale_sqlite_completion_bit() {
        let f = Fixture::new();
        f.finalize();
        assert!(!f.sqlite_ack(), "SQLite still has the pre-reply checkpoint");

        let restarted = BgTaskRegistry::default();
        restarted.set_harness(Harness::Opencode);
        restarted.set_db_pool(Arc::clone(&f.pool));
        restarted
            .replay_session_for_project(f.dir.path(), SESSION, f.dir.path())
            .unwrap();
        assert!(
            restarted.task(&f.task.task_id).is_some(),
            "replay must actually visit the stale task"
        );
        assert!(!restarted.has_completions_for_session(Some(SESSION)));
    }

    #[test]
    fn terminal_watch_ack_cannot_overwrite_a_later_promotion() {
        let f = Fixture::new();
        f.finalize();

        f.registry().promote(&f.task.task_id, SESSION).unwrap();
        assert!(!read_task(&f.task.paths.json).unwrap().completion_delivered);
        f.registry().flush_deferred_watch_acks();

        assert!(
            !f.sqlite_ack(),
            "promotion created a new completion obligation"
        );
        assert!(f
            .registry()
            .inner
            .deferred_watch_acks
            .0
            .lock()
            .unwrap()
            .is_empty());
    }
}
