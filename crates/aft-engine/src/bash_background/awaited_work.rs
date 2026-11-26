//! Arbitrate terminal result delivery with the task's existing reply ack.

use super::*;
use crate::subc::awaited_work::{Event, Service};
use cortexkit_role_agent_orchestrator::params::Outcome;
use std::cell::Cell;

#[cfg(all(test, unix))]
#[path = "awaited_work_test_support.rs"]
mod test_support;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Opening,
    OpeningConsumed,
    Open,
    Finish {
        outcome: Outcome,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notice: Option<FinishNotice>,
    },
    Cancel,
    FallbackCancel,
    Settled,
}

impl State {
    fn opening(&self) -> bool {
        matches!(self, Self::Opening | Self::OpeningConsumed)
    }

    fn suppresses_legacy(&self) -> bool {
        !matches!(self, Self::FallbackCancel)
    }

    fn decision(&self) -> Option<Event> {
        match self {
            Self::Finish {
                outcome, summary, ..
            } => Some(Event::Finish {
                outcome: *outcome,
                summary: summary.clone(),
            }),
            Self::Cancel => Some(Event::Cancel),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FinishNotice {
    WithoutCard,
    Card(String),
}

impl FinishNotice {
    fn id(&self) -> Option<String> {
        match self {
            Self::WithoutCard => None,
            Self::Card(id) => Some(id.clone()),
        }
    }
}

impl PersistedTask {
    pub(super) fn has_unsettled_awaited_work(&self) -> bool {
        self.suppresses_legacy_completion() && !matches!(self.awaited_work, Some(State::Settled))
    }

    pub(super) fn suppresses_legacy_completion(&self) -> bool {
        self.awaited_work
            .as_ref()
            .is_some_and(State::suppresses_legacy)
    }

    pub(crate) fn has_pending_fallback_cancel(&self) -> bool {
        matches!(self.awaited_work, Some(State::FallbackCancel))
    }
}

#[derive(Default)]
pub(super) struct Runtime {
    opening: bool,
    pub(super) kill_reply: bool,
}

impl Runtime {
    pub(super) fn spawning(metadata: &PersistedTask) -> Self {
        Self {
            opening: CANDIDATE.get() && metadata.awaited_work.as_ref().is_some_and(State::opening),
            kill_reply: false,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum OpenOutcome {
    Opened,
    NotOpened,
    Unknown,
}

thread_local! {
    static CANDIDATE: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct AwaitedSpawn(bool);

impl Drop for AwaitedSpawn {
    fn drop(&mut self) {
        CANDIDATE.set(self.0);
    }
}

pub(crate) fn state_for_spawn() -> Option<State> {
    CANDIDATE.get().then_some(State::Opening)
}

impl BgTaskRegistry {
    pub(crate) fn awaited_cleanup_age(&self, task: &BgTask) -> Duration {
        task.lock_state().map_or(Duration::MAX, |state| {
            Duration::from_millis(unix_millis().saturating_sub(state.metadata.started_at))
        })
    }

    pub(crate) fn watch_awaited_scope_ends(&self) -> tokio::sync::watch::Receiver<()> {
        self.inner.awaited_scope_ends.subscribe()
    }
    pub(crate) fn kill_for_reply(
        &self,
        key: &str,
        session: &str,
    ) -> Result<BgTaskSnapshot, RegistryError> {
        self.kill_with_reply_intent(key, session, BgTaskStatus::Killed, None, true)
    }

    pub(super) fn reserve_awaited_kill_reply(
        &self,
        task: &BgTask,
        state: &mut BgTaskState,
    ) -> bool {
        let owns_reply = matches!(
            state.metadata.awaited_work,
            Some(State::Open | State::Opening)
        ) && state
            .metadata
            .lifecycle_scope()
            .is_some_and(crate::subc::awaited_work::owns_kill_reply)
            && super::super::TaskIdentity::current().as_ref() == task.launching_identity.as_ref();
        if owns_reply {
            state.awaited_runtime.kill_reply = true;
        }
        owns_reply
    }

    pub(crate) fn awaited_finish_notice(&self, task: &BgTask) -> Option<Option<String>> {
        let state = task.lock_state().ok()?;
        match &state.metadata.awaited_work {
            Some(State::Finish {
                notice: Some(notice),
                ..
            }) => Some(notice.id()),
            _ => None,
        }
    }

    pub(crate) fn freeze_awaited_finish_notice(
        &self,
        task: &BgTask,
        id: Option<String>,
    ) -> std::io::Result<Option<String>> {
        let still_registered = self.task(&task.task_id).is_some();
        let mut db = DeferredDbWrites::new(self, task);
        let mut state = task
            .lock_state()
            .map_err(|_| std::io::Error::other("background task lock poisoned"))?;
        let Some(State::Finish { notice, .. }) = &state.metadata.awaited_work else {
            return Err(std::io::Error::other("awaited task has no finish decision"));
        };
        if let Some(notice) = notice {
            return Ok(notice.id());
        }
        if !still_registered {
            // The event's terminal decision was already saved. Pruning its
            // task may cost the display card, but must not strand the item.
            return Ok(None);
        }
        let saved = id
            .clone()
            .map_or(FinishNotice::WithoutCard, FinishNotice::Card);
        state.metadata = self.update_task_metadata_locked(task, &mut db, |metadata| {
            if let Some(State::Finish { notice, .. }) = &mut metadata.awaited_work {
                *notice = Some(saved);
            }
        })?;
        drop(state);
        // The task's metadata file already holds the durable decision. Its
        // SQLite mirror shares a pool with the card and must not delay finish.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            if std::mem::take(&mut db.captured) {
                let order = task.db_write_order.clone();
                runtime.spawn_blocking(move || drop(order.flush_locked()));
            }
        }
        Ok(id)
    }

    pub(crate) fn prepare_awaited_spawn(&self, service: &Arc<Service>) -> AwaitedSpawn {
        self.attach_awaited_work(service);
        AwaitedSpawn(CANDIDATE.replace(true))
    }

    fn attach_awaited_work(&self, service: &Arc<Service>) {
        *self.inner.awaited_work.lock() = Arc::downgrade(service);
    }

    fn send_awaited(
        &self,
        task: &Arc<BgTask>,
        event: Event,
        notice: Option<PushFrame>,
        persisted: bool,
    ) {
        if let Some(service) = self.inner.awaited_work.lock().upgrade() {
            service.submit(
                self.clone(),
                task.clone(),
                task.task_id.clone(),
                event,
                notice,
                persisted,
            );
        }
    }

    pub(crate) fn knows_awaited_work(&self, key: &str) -> bool {
        self.task(key).is_some_and(|task| {
            task.lock_state()
                .is_ok_and(|state| !matches!(state.metadata.awaited_work, Some(State::Settled)))
        })
    }

    pub(crate) fn pending_awaited_open_cleanups(&self) -> Vec<String> {
        self.task_arcs()
            .into_iter()
            .filter_map(|task| {
                task.lock_state()
                    .ok()?
                    .metadata
                    .has_pending_fallback_cancel()
                    .then(|| task.task_id.clone())
            })
            .collect()
    }

    pub(crate) fn awaited_work_live(&self, task: &BgTask) -> bool {
        task.lock_state().is_ok_and(|state| {
            matches!(
                state.metadata.awaited_work,
                Some(State::Open | State::Finish { .. } | State::Cancel | State::FallbackCancel)
            ) && state
                .metadata
                .lifecycle_scope()
                .is_none_or(|scope| !self.scope_revoked(scope))
        })
    }

    pub(crate) fn retire_awaited_work(&self, task: &BgTask) {
        let mut db = DeferredDbWrites::new(self, task);
        if let Ok(mut state) = task.lock_state() {
            self.settle_awaited_locked(task, &mut state, &mut db);
        };
    }

    fn settle_awaited_locked(
        &self,
        task: &BgTask,
        state: &mut BgTaskState,
        db: &mut DeferredDbWrites<'_>,
    ) {
        match self.update_task_metadata_locked(task, db, |metadata| {
            if metadata.has_pending_fallback_cancel() {
                metadata.awaited_work = None;
            } else if metadata.awaited_work.is_some() {
                metadata.awaited_work = Some(State::Settled);
                metadata.completion_delivered = true;
            }
        }) {
            Ok(metadata) => state.metadata = metadata,
            Err(error) => log::warn!(
                "could not persist awaited-work settlement for {}: {error}",
                task.task_id
            ),
        }
    }

    pub(crate) async fn settle_missing_awaited(
        &self,
        listed: Arc<HashSet<String>>,
        service: Arc<Service>,
    ) {
        let registry = self.clone();
        let Ok(tasks) = tokio::task::spawn_blocking(move || registry.task_arcs()).await else {
            return;
        };
        let mut tasks = tasks.into_iter();
        loop {
            let batch: Vec<_> = (&mut tasks).take(64).collect();
            if batch.is_empty() {
                break;
            }
            let (registry, listed, service) = (self.clone(), listed.clone(), service.clone());
            let _ = tokio::task::spawn_blocking(move || {
                for task in batch {
                    if listed.contains(&task.task_id) {
                        continue;
                    }
                    let mut db = DeferredDbWrites::new(&registry, &task);
                    let Ok(mut state) = task.lock_state() else {
                        continue;
                    };
                    if state.metadata.has_pending_fallback_cancel() {
                        drop(state);
                        registry.attach_awaited_work(&service);
                        registry.send_awaited(&task, Event::CleanupOpen, None, true);
                        continue;
                    }
                    if !state.awaited_runtime.opening
                        && state.metadata.awaited_work.is_some()
                        && !matches!(state.metadata.awaited_work, Some(State::Settled))
                    {
                        registry.settle_awaited_locked(&task, &mut state, &mut db);
                    }
                }
            })
            .await;
        }
    }

    pub(crate) fn persist_awaited_decision(&self, task: &BgTask) -> std::io::Result<()> {
        let mut db = DeferredDbWrites::new(self, task);
        let state = task
            .lock_state()
            .map_err(|_| std::io::Error::other("background task lock poisoned"))?;
        self.persist_task_locked(task, &state.metadata, &mut db)
    }

    pub(crate) fn resume_awaited_work(&self, key: &str, service: &Arc<Service>) {
        self.attach_awaited_work(service);
        self.transition_awaited_open(key, OpenOutcome::Opened, true);
    }

    pub(crate) fn settle_awaited_open(&self, key: &str, outcome: OpenOutcome) -> bool {
        self.transition_awaited_open(key, outcome, false)
    }

    fn transition_awaited_open(&self, key: &str, outcome: OpenOutcome, recovering: bool) -> bool {
        let Some(task) = self.task(key) else {
            return false;
        };
        let opened = matches!(outcome, OpenOutcome::Opened);
        let (metadata, persisted) = {
            let mut db = DeferredDbWrites::new(self, &task);
            let Ok(mut state) = task.lock_state() else {
                return false;
            };
            if recovering && state.awaited_runtime.opening {
                return false;
            }
            state.awaited_runtime.opening = false;
            match &state.metadata.awaited_work {
                Some(mark) if mark.opening() => {
                    let consumed = matches!(mark, State::OpeningConsumed);
                    state.metadata.awaited_work = if opened {
                        Some(if consumed { State::Cancel } else { State::Open })
                    } else {
                        state.metadata.completion_delivered =
                            !state.metadata.notify_on_completion || consumed;
                        matches!(outcome, OpenOutcome::Unknown).then_some(State::FallbackCancel)
                    };
                }
                None if recovering => state.metadata.awaited_work = Some(State::FallbackCancel),
                _ if recovering => {}
                _ => return false,
            }
            let saved = self.persist_task_locked(&task, &state.metadata, &mut db);
            if let Err(error) = &saved {
                log::warn!("could not persist awaited-work registration for {key}: {error}");
            }
            (state.metadata.clone(), saved.is_ok())
        };
        if opened && recovering && metadata.suppresses_legacy_completion() {
            let _ = self.remove_pending_completion(key);
        }
        if matches!(metadata.awaited_work, Some(State::Cancel)) {
            self.send_awaited(&task, Event::Cancel, None, persisted);
        } else if metadata.has_pending_fallback_cancel() {
            self.send_awaited(&task, Event::CleanupOpen, None, persisted);
        }
        if metadata.status.is_terminal() && !matches!(metadata.awaited_work, Some(State::Cancel)) {
            if let Some(service) = self.inner.awaited_work.lock().upgrade() {
                let registry = self.clone();
                service.replay_terminal(move || {
                    registry.enqueue_completion_if_needed(&metadata, Some(&task.paths), true);
                });
            }
        }
        opened
    }

    pub(super) fn awaited_opening(&self, task: &BgTask) -> bool {
        task.lock_state().is_ok_and(|state| {
            state
                .metadata
                .awaited_work
                .as_ref()
                .is_some_and(State::opening)
                || (matches!(
                    state.metadata.awaited_work,
                    Some(State::Open | State::Finish { .. })
                ) && self.inner.awaited_work.lock().strong_count() == 0)
        })
    }

    pub(crate) fn has_awaited_open_pending(&self, key: &str) -> bool {
        self.task(key).is_some_and(|task| {
            task.lock_state().is_ok_and(|state| {
                state
                    .metadata
                    .awaited_work
                    .as_ref()
                    .is_some_and(State::opening)
            })
        })
    }

    pub(super) fn uses_awaited_work(&self, key: &str) -> bool {
        self.task(key).is_some_and(|task| {
            task.lock_state()
                .is_ok_and(|state| state.metadata.suppresses_legacy_completion())
        })
    }

    pub(super) fn publish_awaited_completion(
        &self,
        task: &Arc<BgTask>,
        completion: &BgCompletion,
    ) -> bool {
        let mut db = DeferredDbWrites::new(self, task);
        let Ok(mut state) = task.lock_state() else {
            return false;
        };
        let mut persisted = true;
        let decision = match &state.metadata.awaited_work {
            None | Some(State::FallbackCancel) => return false,
            Some(State::Open)
                if state.awaited_runtime.kill_reply
                    && completion.status == BgTaskStatus::Killed =>
            {
                state.metadata.awaited_work = Some(State::Cancel);
                if let Err(error) = self.persist_task_locked(task, &state.metadata, &mut db) {
                    persisted = false;
                    log::warn!(
                        "could not persist awaited-work kill reply for {}: {error}",
                        task.task_id
                    );
                }
                Event::Cancel
            }
            Some(State::Open) => {
                let outcome = if completion.status == BgTaskStatus::Completed
                    && completion.exit_code == Some(0)
                {
                    Outcome::Done
                } else {
                    Outcome::Failed
                };
                let summary = crate::subc::awaited_work::completion_summary(completion);
                state.metadata.awaited_work = Some(State::Finish {
                    outcome,
                    summary: summary.clone(),
                    notice: None,
                });
                if let Err(error) = self.persist_task_locked(task, &state.metadata, &mut db) {
                    persisted = false;
                    log::warn!(
                        "could not persist awaited-work result for {}: {error}",
                        task.task_id
                    );
                }
                Event::Finish { outcome, summary }
            }
            Some(State::Finish {
                outcome, summary, ..
            }) => Event::Finish {
                outcome: *outcome,
                summary: summary.clone(),
            },
            Some(_) => return true,
        };
        drop(state);
        drop(db);
        let notice =
            matches!(decision, Event::Finish { .. }).then(|| completion.clone().into_push_frame());
        self.send_awaited(task, decision, notice, persisted);
        true
    }

    pub(crate) fn ack_awaited_terminal_reply(&self, key: &str, session: &str) -> bool {
        let Some(task) = self.task_for_session(key, session) else {
            return false;
        };
        let mut db = DeferredDbWrites::new(self, &task);
        let Ok(mut state) = task.lock_state() else {
            return false;
        };
        if !state.metadata.status.is_terminal() {
            return false;
        }
        // Publication uses this same lock. Once either side commits the
        // awaited result, a later read still returns output but cannot switch it.
        let marker = match state.metadata.awaited_work {
            Some(State::Opening) => State::OpeningConsumed,
            Some(State::Open) => State::Cancel,
            None | Some(State::FallbackCancel) => return false,
            Some(_) => return true,
        };
        let decision = marker.decision();
        state.metadata.awaited_work = Some(marker);
        let saved = self.persist_task_locked(&task, &state.metadata, &mut db);
        if let Err(error) = &saved {
            log::warn!("could not persist awaited-work reply for {key}: {error}");
        }
        drop(state);
        drop(db);
        if let Some(decision) = decision {
            self.send_awaited(&task, decision, None, saved.is_ok());
        }
        true
    }

    pub(super) fn publish_awaited_progress(&self, pattern: &BashPatternMatchFrame) -> bool {
        let Some(task) = self.task(&pattern.task_id) else {
            return false;
        };
        let Ok(state) = task.lock_state() else {
            return false;
        };
        let active = state.metadata.suppresses_legacy_completion();
        let open = matches!(state.metadata.awaited_work, Some(State::Open));
        if open && pattern.reason != "task_exit" {
            let context = if pattern.context.is_empty() {
                &pattern.match_text
            } else {
                &pattern.context
            };
            if !context.is_empty() {
                self.send_awaited(
                    &task,
                    Event::Progress(crate::subc::awaited_work::bounded(context, 2000)),
                    Some(PushFrame::BashPatternMatch(pattern.clone())),
                    true,
                );
            }
        }
        active
    }
}
