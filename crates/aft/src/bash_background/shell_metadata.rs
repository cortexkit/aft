//! Immutable launch facts for the operator's shell list.
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ShellCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub kind: String,
}

thread_local! {
    static CURRENT: RefCell<Option<ShellCall>> = const { RefCell::new(None) };
}

pub(crate) struct Guard(Option<ShellCall>);

impl Drop for Guard {
    fn drop(&mut self) {
        CURRENT.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

pub(crate) fn enter(call: ShellCall) -> Guard {
    Guard(CURRENT.with(|slot| slot.replace(Some(call))))
}

pub(crate) fn current() -> Option<ShellCall> {
    CURRENT.with(|slot| slot.borrow().clone())
}

pub(crate) use crate::db::bash_tasks::changed;

// A watchdog wakeup alone does not imply changed database facts.
// Advance the cache generation only when task metadata is persisted.
static FACTS_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn facts_changed() {
    FACTS_REVISION.fetch_add(1, std::sync::atomic::Ordering::Release);
    changed();
}

pub(crate) fn facts_revision() -> u64 {
    FACTS_REVISION.load(std::sync::atomic::Ordering::Acquire)
}

pub(crate) use crate::db::bash_tasks::subscribe;
