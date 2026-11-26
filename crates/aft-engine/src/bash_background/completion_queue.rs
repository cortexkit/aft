//! The in-memory queue of completion notices awaiting delivery, and its
//! bounds.
//!
//! A notice leaves the queue only when its session acknowledges it. A session
//! that stops acknowledging (often one that has ended) would otherwise keep
//! its notices in memory for the daemon's lifetime, and each response to that
//! session re-sends all of them. So the queue holds a bounded number of
//! notices per session and overall. A notice that finds the queue full waits
//! as its task id; as the session acknowledges what is queued, the queue
//! refills from those tasks oldest first, a bounded batch at a time.
//!
//! Waiting ids are not capped, so no deliverable notice is ever dropped.
//! Their bound is the registry's own task retention: the periodic cleanup
//! pass removes the id of every task the registry no longer holds.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::{Deref, DerefMut};

use super::*;
use crate::protocol::BashLongRunningFrame;

impl BgTask {
    pub(super) fn set_completion_delivered(
        &self,
        delivered: bool,
        registry: &BgTaskRegistry,
    ) -> std::io::Result<()> {
        registry.retry_terminal_persist(self)?;
        let mut db = DeferredDbWrites::new(registry, self);
        let mut state = self
            .lock_state()
            .map_err(|_| std::io::Error::other("background task lock poisoned"))?;
        if delivered && state.metadata.has_unsettled_awaited_work() {
            return Ok(());
        }
        let updated = registry.update_task_metadata_locked(self, &mut db, |metadata| {
            metadata.completion_delivered = delivered;
        })?;
        state.metadata = updated;
        Ok(())
    }
}

impl BgTaskRegistry {
    pub(super) fn emit_bash_pattern_match(&self, session_id: &str, pattern_match: PatternMatch) {
        let mut frame = pattern_notice_frame(session_id, pattern_match);
        frame.launching_identity = self.task_launching_identity(session_id, &frame.task_id);
        if self.publish_awaited_progress(&frame) {
            return;
        }
        if frame.launching_identity.is_none()
            && !self.originating_session_has_live_route(session_id)
        {
            return;
        }
        let Ok(progress_sender) = self
            .inner
            .progress_sender
            .lock()
            .map(|sender| sender.clone())
        else {
            return;
        };
        if let Some(sender) = progress_sender.as_ref() {
            sender(PushFrame::BashPatternMatch(frame));
        }
    }

    pub(super) fn enqueue_completion_if_needed(
        &self,
        metadata: &PersistedTask,
        paths: Option<&TaskPaths>,
        emit_frame: bool,
    ) {
        if metadata.status.is_terminal()
            && (!metadata.completion_delivered || metadata.awaited_work.is_some())
        {
            let cache =
                paths.and_then(|paths| self.render_terminal_output_from_paths(metadata, paths));
            self.enqueue_completion_from_parts(
                metadata,
                None,
                paths,
                emit_frame,
                cache.as_ref(),
                CompletionTokenPolicy::Count,
            );
        }
    }

    pub(super) fn render_terminal_output_from_paths(
        &self,
        metadata: &PersistedTask,
        paths: &TaskPaths,
    ) -> Option<TerminalOutputCache> {
        if metadata.mode == BgMode::Pty {
            return None;
        }
        let mut buffer = BgBuffer::registered(paths, BgMode::Pipes);
        let disk_truncation = buffer.enforce_terminal_cap();
        Some(self.render_terminal_output(metadata, &buffer, disk_truncation, Some(paths)))
    }

    pub(super) fn enqueue_completion_from_parts(
        &self,
        metadata: &PersistedTask,
        buffer: Option<&BgBuffer>,
        paths: Option<&TaskPaths>,
        emit_frame: bool,
        terminal_render: Option<&TerminalOutputCache>,
        token_policy: CompletionTokenPolicy,
    ) {
        if !metadata.status.is_terminal() {
            return;
        }
        // A task shared with a project registry publishes through it, and a
        // `bash.kill_scope` recovery registry publishes nothing: it has no
        // session and is detached afterwards, so a notice queued there (and
        // the routed flag below, which it would set) would be lost.
        match self.notice_target(&metadata.task_id) {
            NoticeTarget::Here => {}
            NoticeTarget::Elsewhere(registry) => {
                return registry.enqueue_completion_from_parts(
                    metadata,
                    buffer,
                    paths,
                    emit_frame,
                    terminal_render,
                    token_policy,
                );
            }
            NoticeTarget::Nowhere => return,
        }

        let owner = self.task(&metadata.task_id);
        if let Some(task) = owner.as_ref() {
            let buffer = buffer
                .cloned()
                .or_else(|| task.lock_state().ok().map(|state| state.buffer.clone()));
            if let Some(buffer) = buffer {
                self.capture_display_terminal(task, metadata, &buffer);
            }
        }
        if owner.as_ref().is_some_and(|task| {
            task.terminal_notification_routed
                .lock()
                .is_ok_and(|routed| *routed)
        }) {
            return;
        }

        let owned_buffer = if buffer.is_none() && metadata.mode != BgMode::Pty {
            paths.map(|paths| BgBuffer::registered(paths, BgMode::Pipes))
        } else {
            None
        };
        let render_buffer = buffer.or(owned_buffer.as_ref());
        #[cfg(unix)]
        if let Some(buffer) = buffer {
            self.finish_db_hint(metadata, buffer);
        }
        let owned_render = if terminal_render.is_none() {
            render_buffer.map(|buffer| {
                let mut capped_buffer = buffer.clone();
                let disk_truncation = capped_buffer.enforce_terminal_cap();
                self.render_terminal_output(metadata, &capped_buffer, disk_truncation, paths)
            })
        } else {
            None
        };
        let render = terminal_render.or(owned_render.as_ref());

        // Completion reminders use the already-rendered terminal output and a
        // smaller, exit-aware head+tail cap. They never invoke the compressor
        // themselves.
        let (output_preview, output_truncated, bash_output_list_envelope) =
            if let Some(saved) = self.saved_completion_preview(metadata) {
                (
                    saved.output_preview,
                    saved.output_truncated,
                    saved.bash_output_list_envelope,
                )
            } else {
                let (mut output_preview, output_truncated) = render
                    .map(|cache| self.completion_preview_for_notice(metadata, cache, paths))
                    .unwrap_or_else(|| (String::new(), false));
                if let Some(note) = rendered_execution_note(metadata) {
                    output_preview = format!("{note}\n{output_preview}");
                }
                append_remote_report(&mut output_preview, metadata);
                let note = render.and_then(|cache| cache.read_only_note.as_deref());
                push_read_only_note(&mut output_preview, note);
                if metadata.status == BgTaskStatus::Failed
                    || metadata.status == BgTaskStatus::FateUnknown
                {
                    if let Some(reason) = metadata.status_reason.as_deref() {
                        output_preview = if output_preview.is_empty() {
                            reason.to_string()
                        } else {
                            format!("{reason}\n{output_preview}")
                        };
                    }
                }
                self.append_db_hint(&metadata.task_id, &mut output_preview);
                let bash_output_list_envelope = render
                    .and_then(|cache| append_bash_output_envelope(cache, &mut output_preview));

                self.mask_completion_text(&metadata.task_id, &mut output_preview);
                let saved = self.save_completion_preview(
                    &metadata.task_id,
                    &output_preview,
                    output_truncated,
                    &bash_output_list_envelope,
                );
                (
                    saved.output_preview,
                    saved.output_truncated,
                    saved.bash_output_list_envelope,
                )
            };
        // Rendering may call an external compressor, so do not hold the
        // publication mutex across it. Recheck afterwards: a concurrent kill
        // can publish while the original completion is being rendered.
        let mut routed = owner
            .as_ref()
            .and_then(|task| task.terminal_notification_routed.lock().ok());
        if owner
            .as_ref()
            .is_some_and(|task| self.awaited_opening(task))
        {
            return;
        }
        if let Some(routed) = routed.as_mut() {
            if **routed {
                return;
            }
            **routed = true;
        }
        #[cfg(test)]
        self.inner
            .completion_processing_attempts
            .fetch_add(1, Ordering::SeqCst);

        let token_counts = self.completion_token_counts(
            metadata,
            buffer,
            paths,
            render.map(|render| render.output_preview.as_str()),
            token_policy,
        );
        let completion = BgCompletion {
            changed_files: self.changed_files_report(&metadata.task_id),
            task_id: metadata.task_id.clone(),
            session_id: metadata.session_id.clone(),
            launching_identity: metadata.launching_identity.clone(),
            status: metadata.status.clone(),
            exit_code: metadata.exit_code,
            command: metadata.command.clone(),
            output_preview,
            bash_output_list_envelope,
            output_truncated,
            original_tokens: token_counts.original_tokens,
            compressed_tokens: token_counts.compressed_tokens,
            tokens_skipped: token_counts.tokens_skipped,
            status_reason: metadata.status_reason.clone(),
            remote_oom_kill: remote::remote_oom_kill(metadata),
            output_incomplete: metadata.output_incomplete,
            live_descendants: metadata.live_descendants.clone(),
            live_descendants_omitted: metadata.live_descendants_omitted,
            live_descendants_unavailable: live_descendants_unavailable(metadata),
            live_descendants_summary: live_descendants_summary(metadata),
        };

        // Account once per registered task, including foreground tasks whose
        // delivery flag is preset to suppress notices. Queue delivery/acks and
        // restart replay are separate from this in-process publication fence;
        // the database also deduplicates compression events across restarts.
        self.record_compression_event_if_applicable(metadata, &token_counts);
        if let Some(task) = owner.as_ref() {
            if self.publish_awaited_completion(task, &completion) {
                self.clear_task_watch_state(&metadata.task_id);
                return;
            }
        } else if metadata.awaited_work.is_some() {
            return;
        }

        // A session ack can arrive through another root actor. Reconcile with
        // the shared rows before task-local watch flags decide terminal routing.
        self.sync_memory_watches_from_persistence(&metadata.task_id);
        let (watch_controlled, watch_matched) = self.task_watch_state(&metadata.task_id);
        if watch_controlled {
            if !watch_matched && !metadata.completion_delivered {
                self.record_bash_watch_exit(&completion, emit_frame);
            }
            // Clear only in-memory control state. The saved match remains
            // available for delivery until it is acknowledged or expires.
            self.clear_task_watch_state(&metadata.task_id);
            return;
        }

        // Push-frame queue is gated on `completion_delivered` so foreground
        // bash with `notify_on_completion=false` does not leak a user-visible
        // completion notification. `mark_terminal` pre-sets
        // `completion_delivered=true` for those tasks; honoring it here keeps
        // the suppression invariant the test
        // `no_notify_foreground_poll_completion_does_not_enqueue_completion`
        // asserts. The compression-event recording above intentionally runs
        // before this gate so foreground bash still contributes to the
        // session/project aggregates.
        // Rendering uses a snapshot taken before the reply may have acked
        // this task. Hold the current state through queue insertion so the ack
        // can fence any late snapshot instead of restoring a notification.
        let ack_state = owner.as_ref().and_then(|task| task.lock_state().ok());
        if metadata.completion_delivered
            || owner.as_ref().is_some_and(|task| {
                task.db_write_order
                    .recovered_reply_delivered
                    .load(Ordering::Acquire)
            })
            || ack_state
                .as_ref()
                .is_some_and(|state| state.metadata.completion_delivered)
        {
            return;
        }

        // Push-frame queue dedupe stays per-task to prevent duplicate
        // user-visible completion notifications.
        let pushed = self
            .inner
            .completions
            .lock()
            .is_ok_and(|mut queue| queue.push(completion.clone()));

        drop(ack_state);
        if pushed && emit_frame {
            self.emit_bash_completed(completion);
        }
    }

    pub(crate) fn maybe_emit_long_running_reminder(&self, task: &Arc<BgTask>) {
        if !self
            .inner
            .long_running_reminder_enabled
            .load(Ordering::SeqCst)
        {
            return;
        }
        let policy = match task.lock_state() {
            Ok(state) if state.metadata.awaited_work.is_some() => return,
            Ok(state) => state.metadata.runner_notice_policy,
            Err(_) => None,
        };
        let interval_ms = policy
            .map(|policy| policy.reminder_interval_ms)
            .unwrap_or_else(|| {
                self.inner
                    .long_running_reminder_interval_ms
                    .load(Ordering::SeqCst)
            });
        if interval_ms == 0 {
            return;
        }
        let interval = Duration::from_millis(interval_ms);
        let now = Instant::now();
        let Ok(mut last_reminder_at) = task.last_reminder_at.lock() else {
            return;
        };
        let since = last_reminder_at.unwrap_or(task.started);
        if now.duration_since(since) < interval {
            return;
        }
        let command = task
            .lock_state()
            .map(|state| state.metadata.command.clone())
            .unwrap_or_default();
        *last_reminder_at = Some(now);
        self.emit_bash_long_running(BashLongRunningFrame::new(
            task.task_id.clone(),
            task.session_id.clone(),
            command,
            task.started.elapsed().as_millis() as u64,
        ));
    }

    pub(super) fn emit_bash_long_running(&self, mut frame: BashLongRunningFrame) {
        frame.launching_identity = self.task_launching_identity(&frame.session_id, &frame.task_id);
        if !self.originating_session_has_live_route(&frame.session_id) {
            return;
        }
        let Ok(progress_sender) = self
            .inner
            .progress_sender
            .lock()
            .map(|sender| sender.clone())
        else {
            return;
        };
        if let Some(sender) = progress_sender.as_ref() {
            sender(PushFrame::BashLongRunning(frame));
        }
    }
}

fn pattern_notice_frame(session_id: &str, mut pattern: PatternMatch) -> BashPatternMatchFrame {
    let terminal_summary = pattern.batch.as_mut().is_some_and(|batch| {
        if !batch.terminal_summary {
            return false;
        }
        batch.lines.clear();
        pattern.context = crate::task_watch_lines::format_watch_line_batch(&pattern.task_id, batch);
        true
    });
    let mut frame = if !terminal_summary && pattern.match_text == WATCH_TASK_EXIT_TEXT {
        BashPatternMatchFrame::task_exit(
            pattern.task_id,
            session_id,
            pattern.match_text,
            pattern.context,
        )
    } else {
        BashPatternMatchFrame::new(
            pattern.task_id,
            session_id,
            pattern.watch_id,
            pattern.match_text,
            pattern.match_offset,
            pattern.context,
            pattern.once,
        )
    };
    frame.batch = pattern.batch;
    frame
}

/// Notices queued for one session.
pub(crate) const MAX_QUEUED_PER_SESSION: usize = 128;
/// Notices queued across all sessions.
const MAX_QUEUED: usize = 4096;
/// Notices one refill loads for a session.
const REFILL_BATCH: usize = 32;

/// What the bounds need to know about a queued notice.
pub(crate) trait QueuedNotice {
    fn task_id(&self) -> &str;
    fn session_id(&self) -> &str;
}

impl QueuedNotice for BgCompletion {
    fn task_id(&self) -> &str {
        &self.task_id
    }
    fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// Queued notices in delivery order, plus the notices that found the queue
/// full, in the order they arrived.
pub(crate) struct NoticeQueue<N> {
    queued: VecDeque<N>,
    /// (session id, task id) of each notice waiting outside the queue.
    overflowed: VecDeque<(String, String)>,
    /// The task ids in `overflowed`, so a repeated notice is found without
    /// scanning the whole waiting list.
    overflowed_tasks: HashSet<String>,
}

impl<N> Default for NoticeQueue<N> {
    fn default() -> Self {
        Self {
            queued: VecDeque::new(),
            overflowed: VecDeque::new(),
            overflowed_tasks: HashSet::new(),
        }
    }
}

impl<N> Deref for NoticeQueue<N> {
    type Target = VecDeque<N>;
    fn deref(&self) -> &VecDeque<N> {
        &self.queued
    }
}

impl<N> DerefMut for NoticeQueue<N> {
    fn deref_mut(&mut self) -> &mut VecDeque<N> {
        &mut self.queued
    }
}

impl<N: QueuedNotice> NoticeQueue<N> {
    fn queued_for(&self, session: &str) -> usize {
        self.queued
            .iter()
            .filter(|queued| queued.session_id() == session)
            .count()
    }

    /// Queue `notice` unless its task already has one queued or the queue is
    /// full for its session or overall; a full queue remembers the task so a
    /// refill can load its notice later. Returns whether it was queued.
    pub(crate) fn push(&mut self, notice: N) -> bool {
        if self
            .queued
            .iter()
            .any(|queued| queued.task_id() == notice.task_id())
        {
            return false;
        }
        if self.queued.len() >= MAX_QUEUED
            || self.queued_for(notice.session_id()) >= MAX_QUEUED_PER_SESSION
        {
            if self.overflowed_tasks.insert(notice.task_id().to_string()) {
                self.overflowed.push_back((
                    notice.session_id().to_string(),
                    notice.task_id().to_string(),
                ));
            }
            return false;
        }
        self.queued.push_back(notice);
        true
    }

    /// Take the task ids of the next waiting notices that fit, oldest first:
    /// at most [`REFILL_BATCH`] per session and never past either bound.
    fn take_refill(&mut self) -> Vec<String> {
        let mut free_overall = MAX_QUEUED.saturating_sub(self.queued.len());
        let mut taken_for: HashMap<String, usize> = HashMap::new();
        let mut taken = Vec::new();
        let mut index = 0;
        while index < self.overflowed.len() && free_overall > 0 {
            let session = &self.overflowed[index].0;
            let already = taken_for.get(session).copied().unwrap_or(0);
            let free = MAX_QUEUED_PER_SESSION.saturating_sub(self.queued_for(session) + already);
            if already >= REFILL_BATCH || free == 0 {
                index += 1;
                continue;
            }
            let (session, task) = self.overflowed.remove(index).expect("index in range");
            self.overflowed_tasks.remove(&task);
            *taken_for.entry(session).or_default() += 1;
            taken.push(task);
            free_overall -= 1;
        }
        taken
    }

    /// Forget waiting ids for which `held` is false.
    fn prune_waiting(&mut self, held: impl Fn(&str) -> bool) {
        let tasks = &mut self.overflowed_tasks;
        self.overflowed.retain(|(_, task)| {
            let keep = held(task);
            if !keep {
                tasks.remove(task);
            }
            keep
        });
    }
}

impl BgTaskRegistry {
    /// Drop waiting notice ids whose task the registry no longer holds; such a
    /// notice can never be refilled. Task lookups run without the queue lock.
    pub(crate) fn prune_completion_overflow(&self) {
        let Some(waiting) = self.inner.completions.lock().ok().map(|queue| {
            queue
                .overflowed
                .iter()
                .map(|(_, task)| task.clone())
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        let gone = waiting
            .into_iter()
            .filter(|task_id| self.task(task_id).is_none())
            .collect::<HashSet<_>>();
        if gone.is_empty() {
            return;
        }
        if let Ok(mut queue) = self.inner.completions.lock() {
            queue.prune_waiting(|task| !gone.contains(task));
        }
    }

    /// Refill the queue from notices that found it full, oldest first, as
    /// acknowledgements make room. Each is enqueued again from its task, which
    /// re-checks the delivered mark and the queued task ids, so a notice is
    /// never delivered twice; one that finds the queue full again waits again.
    pub(super) fn refill_completion_queue(&self) {
        let Some(waiting) = self
            .inner
            .completions
            .lock()
            .ok()
            .map(|mut queue| queue.take_refill())
        else {
            return;
        };
        for task_id in waiting {
            let Some(task) = self.task(&task_id) else {
                continue;
            };
            let Some(metadata) = task.lock_state().ok().map(|state| state.metadata.clone()) else {
                continue;
            };
            // Its first routing found the queue full and queued nothing; let
            // this enqueue route it again.
            if let Ok(mut routed) = task.terminal_notification_routed.lock() {
                *routed = false;
            }
            self.enqueue_completion_if_needed(&metadata, Some(&task.paths), true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Notice(String, String);

    impl QueuedNotice for Notice {
        fn task_id(&self) -> &str {
            &self.0
        }
        fn session_id(&self) -> &str {
            &self.1
        }
    }

    fn notice(session: &str, task: usize) -> Notice {
        Notice(format!("bash-{task:016x}"), session.to_string())
    }

    #[test]
    fn a_full_session_keeps_its_oldest_notices_and_refills_in_order() {
        let mut queue = NoticeQueue::default();
        for task in 0..MAX_QUEUED_PER_SESSION + 40 {
            assert_eq!(
                queue.push(notice("busy", task)),
                task < MAX_QUEUED_PER_SESSION
            );
        }
        assert!(queue.push(notice("other", 1_000)));
        // Refused again while waiting: still one entry for the task.
        assert!(!queue.push(notice("busy", MAX_QUEUED_PER_SESSION)));
        assert_eq!(queue.queued_for("busy"), MAX_QUEUED_PER_SESSION);
        assert!(queue.take_refill().is_empty(), "no room until an ack");
        for _ in 0..40 {
            queue.pop_front();
        }
        let refill = queue.take_refill();
        assert_eq!(refill.len(), REFILL_BATCH);
        assert_eq!(refill[0], format!("bash-{:016x}", MAX_QUEUED_PER_SESSION));
        assert_eq!(
            refill[REFILL_BATCH - 1],
            format!("bash-{:016x}", MAX_QUEUED_PER_SESSION + REFILL_BATCH - 1)
        );
        // A task's notice is queued once.
        assert!(!queue.push(notice("other", 1_000)));
    }

    #[test]
    fn the_whole_queue_is_bounded_across_sessions() {
        let mut queue = NoticeQueue::default();
        for task in 0..MAX_QUEUED + 5 {
            queue.push(notice(&format!("session-{}", task % 64), task));
        }
        assert_eq!(queue.len(), MAX_QUEUED);
    }

    #[test]
    fn acknowledgements_deliver_every_notice_past_the_cap_without_a_restart() {
        let fixture = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let total = MAX_QUEUED_PER_SESSION * 2 + 7;
        let mut finished = Vec::new();
        for _ in 0..total {
            let (task_id, task) = super::super::tests::insert_terminal_piped_task(
                &registry, &fixture, "cmd", "done\n", "", false,
            );
            let metadata = task.lock_state().unwrap().metadata.clone();
            registry.enqueue_completion_if_needed(&metadata, Some(&task.paths), false);
            finished.push(task_id);
        }
        let mut delivered = Vec::new();
        loop {
            let resident = registry.drain_completions_for_session(Some("session"));
            assert!(
                resident.len() <= MAX_QUEUED_PER_SESSION,
                "{}",
                resident.len()
            );
            if resident.is_empty() {
                break;
            }
            let ids: Vec<String> = resident.into_iter().map(|notice| notice.task_id).collect();
            for id in &ids {
                assert!(!delivered.contains(id), "{id} delivered twice");
            }
            registry.ack_completions_for_session(Some("session"), &ids);
            delivered.extend(ids);
        }
        // Every notice arrived, in the order its task finished.
        assert_eq!(delivered, finished);
    }

    /// Notices that found the queue full wait as task ids. Once the registry
    /// no longer holds a task, its id can never be delivered, so the cleanup
    /// pass removes it; waiting ids for retained tasks stay.
    #[test]
    fn cleanup_prunes_waiting_ids_of_tasks_the_registry_dropped() {
        let fixture = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let total = MAX_QUEUED_PER_SESSION + 50;
        let mut finished = Vec::new();
        for _ in 0..total {
            let (task_id, task) = super::super::tests::insert_terminal_piped_task(
                &registry, &fixture, "cmd", "done\n", "", false,
            );
            let metadata = task.lock_state().unwrap().metadata.clone();
            registry.enqueue_completion_if_needed(&metadata, Some(&task.paths), false);
            finished.push(task_id);
        }
        let waiting =
            |registry: &BgTaskRegistry| registry.inner.completions.lock().unwrap().overflowed.len();
        assert_eq!(waiting(&registry), 50);
        // The registry forgets the first 30 waiting tasks.
        {
            let mut tasks = registry.inner.tasks.lock().unwrap();
            for task_id in &finished[MAX_QUEUED_PER_SESSION..MAX_QUEUED_PER_SESSION + 30] {
                tasks.remove(task_id);
            }
        }
        crate::bash_background::watchdog::run_cleanup([registry.clone()]);
        let queue = registry.inner.completions.lock().unwrap();
        assert_eq!(queue.overflowed.len(), 20);
        assert!(queue
            .overflowed
            .iter()
            .all(|(_, task)| finished[MAX_QUEUED_PER_SESSION + 30..].contains(task)));
    }
}
