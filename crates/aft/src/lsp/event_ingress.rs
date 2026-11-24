use std::collections::HashMap;
use std::sync::{Arc, Weak};

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use parking_lot::Mutex;

use super::client::{LspEvent, ReceivedLspEvent, ServerInstanceId};
use super::roots::ServerKey;

pub(super) const EVENT_CAP: usize = 64;

#[derive(Clone)]
pub(crate) struct EventIngress {
    ordinary: Sender<ReceivedLspEvent>,
    wake: Sender<()>,
    controls: Arc<Mutex<HashMap<ServerInstanceId, Control>>>,
}

struct Control {
    key: ServerKey,
    last_dropped: u64,
    overflowed: bool,
    exit: Option<ReceivedLspEvent>,
    watched_dropped: usize,
}

pub(super) struct EventIngressLease {
    controls: Weak<Mutex<HashMap<ServerInstanceId, Control>>>,
    instance: ServerInstanceId,
}

impl Drop for EventIngressLease {
    fn drop(&mut self) {
        if let Some(controls) = self.controls.upgrade() {
            controls.lock().remove(&self.instance);
        }
    }
}

impl EventIngress {
    pub(super) fn new() -> (Self, Receiver<ReceivedLspEvent>, Receiver<()>) {
        let (ordinary, events) = bounded(EVENT_CAP);
        let (wake, hints) = bounded(1);
        (
            Self {
                ordinary,
                wake,
                controls: Arc::default(),
            },
            events,
            hints,
        )
    }

    pub(super) fn register(&self, instance: ServerInstanceId, key: ServerKey) -> EventIngressLease {
        self.controls.lock().insert(
            instance,
            Control {
                key,
                last_dropped: 0,
                overflowed: false,
                exit: None,
                watched_dropped: 0,
            },
        );
        EventIngressLease {
            controls: Arc::downgrade(&self.controls),
            instance,
        }
    }

    pub(super) fn send(
        &self,
        event: ReceivedLspEvent,
    ) -> Result<(), TrySendError<ReceivedLspEvent>> {
        match self.ordinary.try_send(event) {
            Err(TrySendError::Full(event)) => {
                if matches!(event.event, LspEvent::ServerExited { .. }) {
                    // One exit per reader. Its instance owns exactly one slot,
                    // so a full notifications queue cannot suppress the exit.
                    let mut controls = self.controls.lock();
                    if let Some(control) = event.instance.and_then(|id| controls.get_mut(&id)) {
                        control.exit = Some(event);
                    }
                } else {
                    self.record_drop(&event);
                }
                let _ = self.wake.try_send(());
                Ok(())
            }
            result => result,
        }
    }

    pub(super) fn record_drop(&self, event: &ReceivedLspEvent) {
        let mut controls = self.controls.lock();
        if let Some(control) = event.instance.and_then(|id| controls.get_mut(&id)) {
            control.overflowed = true;
            control.last_dropped = control
                .last_dropped
                .max(event.receive_order.unwrap_or_default());
        }
    }

    pub(super) fn predates_overflow(&self, event: &ReceivedLspEvent) -> bool {
        let Some((id, order)) = event.instance.zip(event.receive_order) else {
            return false;
        };
        self.controls
            .lock()
            .get(&id)
            .is_some_and(|control| control.last_dropped != 0 && order <= control.last_dropped)
    }

    pub(super) fn take_overflows(&self) -> Vec<(ServerInstanceId, ServerKey)> {
        self.controls
            .lock()
            .iter_mut()
            .filter_map(|(&id, control)| {
                std::mem::take(&mut control.overflowed).then(|| (id, control.key.clone()))
            })
            .collect()
    }

    pub(super) fn take_exit(&self) -> Option<ReceivedLspEvent> {
        self.controls
            .lock()
            .values_mut()
            .find_map(|control| control.exit.take())
    }

    pub(super) fn pending(&self) -> bool {
        self.controls
            .lock()
            .values()
            .any(|control| control.overflowed || control.exit.is_some())
    }

    pub(super) fn record_watched_drop(&self, raw: &std::path::Path, resolved: &std::path::Path) {
        let mut controls = self.controls.lock();
        let in_scope = |control: &Control| {
            raw.starts_with(&control.key.root) || resolved.starts_with(&control.key.root)
        };
        let any_matches = controls.values().any(in_scope);
        for control in controls.values_mut() {
            // If no workspace claims the path, we cannot prove any server's
            // watch coverage is unaffected (for example a relative glob base).
            if !any_matches || in_scope(control) {
                control.watched_dropped = control.watched_dropped.saturating_add(1);
            }
        }
    }

    pub(super) fn take_watched_overflows(
        &self,
        live: impl Fn(&ServerKey, ServerInstanceId) -> bool,
    ) -> Vec<(ServerKey, ServerInstanceId, usize)> {
        self.controls
            .lock()
            .iter_mut()
            .filter_map(|(&instance, control)| {
                (control.watched_dropped != 0 && live(&control.key, instance)).then(|| {
                    (
                        control.key.clone(),
                        instance,
                        std::mem::take(&mut control.watched_dropped),
                    )
                })
            })
            .collect()
    }

    #[cfg(test)]
    pub(super) fn exit_pending_for_test(&self, id: ServerInstanceId) -> bool {
        self.controls
            .lock()
            .get(&id)
            .is_some_and(|control| control.exit.is_some())
    }
}
