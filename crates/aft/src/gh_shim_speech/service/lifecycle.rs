use super::{Code, Refusal, Service, SERVICES};
use std::sync::Arc;

#[derive(Default)]
pub(super) struct State {
    pub(super) managed: bool,
    pub(super) accepting: bool,
    active: usize,
    retiring: bool,
    detaching: bool,
    detached: bool,
    released: bool,
    unproven: bool,
}

pub(crate) struct WorkGuard(Arc<Service>);
impl Drop for WorkGuard {
    fn drop(&mut self) {
        {
            let mut state = self
                .0
                .lifecycle
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(active) = state.active.checked_sub(1) else {
                state.unproven = true;
                return;
            };
            state.active = active;
        }
        self.0.finish_retirement();
    }
}

pub(crate) struct RuntimeGuard(Arc<Service>);
impl RuntimeGuard {
    pub(crate) fn retain_until_process_exit(&self) {
        self.0
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unproven = true;
    }

    pub(crate) fn retire(&self) -> bool {
        self.0.retire()
    }
}
impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

impl Service {
    pub(crate) fn runtime_guard(self: &Arc<Self>) -> RuntimeGuard {
        RuntimeGuard(self.clone())
    }

    pub(crate) fn begin_post(self: &Arc<Self>) -> Result<WorkGuard, Refusal> {
        let mut state = self
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !state.accepting || state.retiring {
            return Err(Refusal::new(Code::AuthorityUnavailable, "not_serving_mode"));
        }
        state.active += 1;
        Ok(WorkGuard(self.clone()))
    }

    pub(crate) fn track_finalization(self: &Arc<Self>) -> WorkGuard {
        self.lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active += 1;
        WorkGuard(self.clone())
    }

    fn retire(&self) -> bool {
        {
            let mut state = self
                .lifecycle
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.accepting = false;
            state.retiring = true;
        }
        self.finish_retirement();
        self.lifecycle.lock().is_ok_and(|state| state.released)
    }

    fn finish_retirement(&self) {
        let Ok(mut state) = self.lifecycle.lock() else {
            return;
        };
        if !state.retiring
            || state.active != 0
            || state.detaching
            || state.released
            || state.unproven
        {
            return;
        }
        let Ok(admission) = self.admission.try_lock() else {
            return;
        };
        let Ok(tasks) = self.pending_tasks.lock() else {
            return;
        };
        let Ok(ids) = self.pending_ids.lock() else {
            return;
        };
        let Ok(scopes) = self.pending_scopes.lock() else {
            return;
        };
        if !tasks.is_empty() || !ids.is_empty() || !scopes.is_empty() {
            return;
        }
        if !state.detached {
            let Ok(mut runtime_guard) = self.runtime.lock() else {
                return;
            };
            let Ok(mut on_end_guard) = self.on_end.lock() else {
                return;
            };
            let runtime = runtime_guard.take();
            let on_end = on_end_guard.take();
            state.detaching = true;
            drop(runtime_guard);
            drop(on_end_guard);
            drop(scopes);
            drop(ids);
            drop(tasks);
            drop(admission);
            drop(state);
            // Runtime callbacks own writer senders and may drop objects that
            // queue terminal markers. Count that finalization before releasing
            // the serving/storage anchor, without holding the lifecycle mutex.
            drop(runtime);
            drop(on_end);
            {
                let Ok(mut state) = self.lifecycle.lock() else {
                    return;
                };
                state.detached = true;
                state.detaching = false;
            }
            self.finish_retirement();
            return;
        }
        let Ok(mut held) = self.lease.lock() else {
            return;
        };
        if let Some(lease) = held.as_ref() {
            if !crate::root_cache::release_quiescent_speech_serving_lease(lease) {
                return;
            }
        }
        let lease = held.take();
        state.detaching = true;
        drop(held);
        drop(scopes);
        drop(ids);
        drop(tasks);
        drop(admission);
        drop(state);
        drop(lease);
        if let Some(services) = SERVICES.get() {
            let mut services = services.lock().unwrap_or_else(|error| error.into_inner());
            if services
                .get(&self.root)
                .is_some_and(|service| std::ptr::eq(service.as_ref(), self))
            {
                services.remove(&self.root);
            }
        }
        if let Ok(mut state) = self.lifecycle.lock() {
            state.released = true;
            state.detaching = false;
        }
    }
}
