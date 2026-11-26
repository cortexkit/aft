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
    Settled,
}

impl State {
    fn opening(&self) -> bool {
        matches!(self, Self::Opening | Self::OpeningConsumed)
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
        self.awaited_work.is_some() && !matches!(self.awaited_work, Some(State::Settled))
    }
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
    pub(crate) fn awaited_finish_notice(&self, key: &str) -> Option<Option<String>> {
        let task = self.task(key)?;
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
        key: &str,
        id: Option<String>,
    ) -> std::io::Result<Option<String>> {
        let task = self
            .task(key)
            .ok_or_else(|| std::io::Error::other("unknown awaited task"))?;
        let mut db = DeferredDbWrites::new(self, &task);
        let mut state = task
            .lock_state()
            .map_err(|_| std::io::Error::other("background task lock poisoned"))?;
        let Some(State::Finish { notice, .. }) = &state.metadata.awaited_work else {
            return Err(std::io::Error::other("awaited task has no finish decision"));
        };
        if let Some(notice) = notice {
            return Ok(notice.id());
        }
        let saved = id
            .clone()
            .map_or(FinishNotice::WithoutCard, FinishNotice::Card);
        state.metadata = self.update_task_metadata_locked(&task, &mut db, |metadata| {
            if let Some(State::Finish { notice, .. }) = &mut metadata.awaited_work {
                *notice = Some(saved);
            }
        })?;
        Ok(id)
    }

    pub(crate) fn prepare_awaited_spawn(&self, service: &Arc<Service>) -> AwaitedSpawn {
        self.attach_awaited_work(service);
        AwaitedSpawn(CANDIDATE.replace(true))
    }

    fn attach_awaited_work(&self, service: &Arc<Service>) {
        *self.inner.awaited_work.lock() = Arc::downgrade(service);
    }

    fn send_awaited(&self, task: &BgTask, event: Event, notice: Option<PushFrame>) {
        if let Some(service) = self.inner.awaited_work.lock().upgrade() {
            service.submit(self.clone(), task.task_id.clone(), event, notice);
        }
    }

    pub(crate) fn knows_awaited_work(&self, key: &str) -> bool {
        self.task(key).is_some_and(|task| {
            task.lock_state().is_ok_and(|state| {
                !matches!(state.metadata.awaited_work, Some(State::Settled))
                    && (!state.metadata.status.is_terminal()
                        || !state.metadata.completion_delivered
                        || state.metadata.awaited_work.is_some())
            })
        })
    }

    pub(crate) fn awaited_work_live(&self, key: &str) -> bool {
        self.task(key).is_some_and(|task| {
            task.lock_state().is_ok_and(|state| {
                matches!(
                    state.metadata.awaited_work,
                    Some(State::Open | State::Finish { .. } | State::Cancel)
                ) && state
                    .metadata
                    .lifecycle_scope()
                    .is_none_or(|scope| !self.scope_revoked(scope))
            })
        })
    }

    pub(crate) fn retire_awaited_work(&self, key: &str) {
        let Some(task) = self.task(key) else { return };
        let mut db = DeferredDbWrites::new(self, &task);
        if let Ok(mut state) = task.lock_state() {
            if let Ok(metadata) = self.update_task_metadata_locked(&task, &mut db, |metadata| {
                metadata.awaited_work = Some(State::Settled);
                metadata.completion_delivered |= metadata.status.is_terminal();
            }) {
                state.metadata = metadata;
            }
        };
    }

    pub(crate) fn persist_awaited_decision(&self, key: &str) -> bool {
        self.task(key).is_some_and(|task| {
            let mut db = DeferredDbWrites::new(self, &task);
            task.lock_state().is_ok_and(|state| {
                self.persist_task_locked(&task, &state.metadata, &mut db)
                    .is_ok()
            })
        })
    }

    pub(crate) fn resume_awaited_work(&self, key: &str, service: &Arc<Service>) {
        self.attach_awaited_work(service);
        self.transition_awaited_open(key, true, true);
    }

    pub(crate) fn settle_awaited_open(&self, key: &str, opened: bool) -> bool {
        self.transition_awaited_open(key, opened, false)
    }

    fn transition_awaited_open(&self, key: &str, opened: bool, recovering: bool) -> bool {
        let Some(task) = self.task(key) else {
            return false;
        };
        let metadata = {
            let mut db = DeferredDbWrites::new(self, &task);
            let Ok(mut state) = task.lock_state() else {
                return false;
            };
            match &state.metadata.awaited_work {
                Some(mark) if mark.opening() => {
                    let consumed = matches!(mark, State::OpeningConsumed);
                    state.metadata.awaited_work = if opened {
                        Some(if consumed { State::Cancel } else { State::Open })
                    } else {
                        state.metadata.completion_delivered =
                            !state.metadata.notify_on_completion || consumed;
                        None
                    };
                }
                None if recovering => state.metadata.awaited_work = Some(State::Open),
                _ if recovering => {}
                _ => return false,
            }
            if let Err(error) = self.persist_task_locked(&task, &state.metadata, &mut db) {
                log::warn!("could not persist awaited-work registration for {key}: {error}");
            }
            state.metadata.clone()
        };
        if opened && recovering {
            let _ = self.remove_pending_completion(key);
        }
        if matches!(metadata.awaited_work, Some(State::Cancel)) {
            self.send_awaited(&task, Event::Cancel, None);
        } else if metadata.status.is_terminal() {
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
                .is_ok_and(|state| state.metadata.awaited_work.is_some())
        })
    }

    pub(super) fn publish_awaited_completion(
        &self,
        task: &BgTask,
        completion: &BgCompletion,
    ) -> bool {
        let mut db = DeferredDbWrites::new(self, task);
        let Ok(mut state) = task.lock_state() else {
            return false;
        };
        let decision = match &state.metadata.awaited_work {
            None => return false,
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
        self.send_awaited(task, decision, Some(completion.clone().into_push_frame()));
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
            None => return false,
            Some(_) => return true,
        };
        let decision = marker.decision();
        state.metadata.awaited_work = Some(marker);
        if let Err(error) = self.persist_task_locked(&task, &state.metadata, &mut db) {
            log::warn!("could not persist awaited-work reply for {key}: {error}");
        }
        drop(state);
        drop(db);
        if let Some(decision) = decision {
            self.send_awaited(&task, decision, None);
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
        let active = state.metadata.awaited_work.is_some();
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
                );
            }
        }
        active
    }
}
