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

fn changes() -> &'static tokio::sync::watch::Sender<u64> {
    static CHANGES: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> =
        std::sync::OnceLock::new();
    CHANGES.get_or_init(|| tokio::sync::watch::channel(0).0)
}

pub(crate) fn changed() {
    changes().send_modify(|revision| *revision = revision.wrapping_add(1));
}

pub(crate) fn subscribe() -> tokio::sync::watch::Receiver<u64> {
    changes().subscribe()
}
