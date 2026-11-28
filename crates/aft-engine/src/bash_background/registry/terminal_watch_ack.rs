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
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn terminal_watch_acks_empty_for_test(&self) -> bool {
        self.inner.deferred_watch_acks.0.lock().unwrap().is_empty()
    }

    /// The owning session already gets the outcome in its watch reply. Persist
    /// that fact in the task file before removing any queued completion, but
    /// leave SQLite to the watchdog: a foreign writer must not delay the reply.
    pub fn ack_terminal_watch_reply(&self, task_id: &str, session_id: &str) {
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
