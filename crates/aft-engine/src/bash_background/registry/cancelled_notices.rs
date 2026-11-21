use super::*;

static RESERVED: std::sync::LazyLock<std::sync::Mutex<HashSet<usize>>> =
    std::sync::LazyLock::new(Default::default);

struct Notice {
    registry: BgTaskRegistry,
    task: Arc<BgTask>,
    text: String,
}

impl Drop for Notice {
    fn drop(&mut self) {
        RESERVED
            .lock()
            .unwrap()
            .remove(&(Arc::as_ptr(&self.task) as usize));
    }
}

impl Notice {
    fn commit(&self) {
        let mut db = DeferredDbWrites::new(&self.registry, &self.task);
        let Ok(mut state) = self.task.lock_state() else {
            return;
        };
        if !state.metadata.cancelled_call_unannounced {
            return;
        }
        let mut updated = state.metadata.clone();
        updated.cancelled_call_unannounced = false;
        if let Err(error) = self
            .registry
            .persist_task_locked(&self.task, &updated, &mut db)
        {
            crate::slog_warn!(
                "cancelled-call notice for task {} was not cleared on disk: {error}",
                self.task.task_id
            );
            return;
        }
        state.metadata = updated;
        if let Ok(mut tasks) = self.registry.inner.cancelled_call_tasks.lock() {
            if let Some(ids) = tasks.get_mut(&self.task.session_id) {
                ids.remove(&self.task.task_id);
                if ids.is_empty() {
                    tasks.remove(&self.task.session_id);
                }
            }
        }
    }
}

/// Reserves the notices for one reply without clearing their durable flags.
/// A response discarded before delivery releases its reservations when dropped.
#[derive(Clone, Default)]
#[doc(hidden)]
pub struct CancelledCallReceipt(Option<Arc<Vec<Notice>>>);

impl std::fmt::Debug for CancelledCallReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CancelledCallReceipt")
            .field(&self.0.as_ref().map(|notices| notices.len()))
            .finish()
    }
}

impl CancelledCallReceipt {
    #[doc(hidden)]
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .flat_map(|notices| notices.iter().map(|notice| notice.text.as_str()))
    }

    fn commit_matching(&self, visible: impl Fn(&str) -> bool) {
        for notice in self.0.iter().flat_map(|notices| notices.iter()) {
            if visible(&notice.text) {
                notice.commit();
            }
        }
    }
}

impl aft_base::pending_notices::NoticeAcknowledgment for CancelledCallReceipt {
    fn commit_written_text(&self, text: &str) {
        self.commit_matching(|line| text.lines().any(|part| part == line));
    }

    fn commit_written_frame(&self, body: &[u8]) -> Option<tokio::task::JoinHandle<()>> {
        let notices = self.0.as_ref()?;
        let result = serde_json::from_slice::<serde_json::Value>(body).ok()?;
        let matches = notices
            .iter()
            .map(|notice| {
                result["content"].as_array().is_some_and(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .any(|text| text.lines().any(|line| line == notice.text))
                })
            })
            .collect::<Vec<_>>();
        if matches.iter().any(|present| *present) {
            // Filesystem and database writes must not block the socket writer.
            let notices = Arc::clone(notices);
            return Some(tokio::task::spawn_blocking(move || {
                for (notice, written) in notices.iter().zip(matches) {
                    if written {
                        notice.commit();
                    }
                }
            }));
        }
        None
    }
}

impl BgTaskRegistry {
    #[doc(hidden)]
    pub fn reserve_cancelled_call_notices(&self, session_id: &str) -> CancelledCallReceipt {
        let Some(task_ids) = self
            .inner
            .cancelled_call_tasks
            .lock()
            .ok()
            .and_then(|tasks| tasks.get(session_id).cloned())
        else {
            return CancelledCallReceipt::default();
        };
        let mut task_ids = task_ids.into_iter().collect::<Vec<_>>();
        task_ids.sort();
        let mut notices = Vec::new();
        for task_id in task_ids {
            let Some(task) = self.task_for_session(&task_id, session_id) else {
                continue;
            };
            if task
                .launching_identity
                .as_ref()
                .is_some_and(|owner| super::super::TaskIdentity::current().as_ref() != Some(owner))
            {
                continue;
            }
            let Ok(state) = task.lock_state() else {
                continue;
            };
            if !state.metadata.cancelled_call_unannounced {
                continue;
            }
            let text = if state.metadata.status_reason.as_deref() == Some(OWNER_PAUSED_REASON)
                && state.metadata.status == BgTaskStatus::Killed
            {
                format!("task {task_id}: killed (cancelled by owner)")
            } else if state.metadata.status.is_terminal() {
                continue;
            } else {
                let job = state
                    .metadata
                    .remote
                    .as_ref()
                    .and_then(|remote| remote.job_id)
                    .map(|job_id| format!(" (job {job_id})"))
                    .unwrap_or_default();
                format!("task {task_id}{job} from your cancelled call is still running; check it with bash_status or stop it with bash_kill")
            };
            if !RESERVED.lock().unwrap().insert(Arc::as_ptr(&task) as usize) {
                continue;
            }
            drop(state);
            notices.push(Notice {
                registry: self.clone(),
                task,
                text,
            });
        }
        CancelledCallReceipt((!notices.is_empty()).then(|| Arc::new(notices)))
    }

    #[cfg(test)]
    pub(crate) fn take_cancelled_call_notices(&self, session_id: &str) -> Vec<String> {
        let receipt = self.reserve_cancelled_call_notices(session_id);
        let lines = receipt.lines().map(str::to_owned).collect();
        receipt.commit_matching(|_| true);
        lines
    }
}
