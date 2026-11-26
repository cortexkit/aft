use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TryRecvError, TrySendError};
use parking_lot::Mutex;

use super::analysis_progress::{self, AnalysisProgressShadow, Transition};
use super::client::{LspEvent, ReceivedLspEvent, ServerInstanceId};
use super::roots::ServerKey;

pub(super) const EVENT_CAP: usize = 64;

#[derive(Clone)]
pub(crate) struct EventIngress {
    inbox: Weak<Mutex<Inbox>>,
    wake: Sender<()>,
    controls: Arc<Mutex<HashMap<ServerInstanceId, Control>>>,
}

#[derive(Clone)]
pub(super) struct EventReceiver {
    inbox: Arc<Mutex<Inbox>>,
    ready: Receiver<()>,
}

struct Inbox {
    ready: Sender<()>,
    next: u64,
    ordinary: usize,
    pending: BTreeMap<u64, Queued>,
    scoped: HashMap<ServerInstanceId, HashMap<String, Slot>>,
}

struct Queued {
    event: ReceivedLspEvent,
    scoped_uri: Option<String>,
}

#[derive(Default)]
struct Slot {
    users: usize,
    pending: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct ScopedReportLease {
    inbox: Weak<Mutex<Inbox>>,
    slots: Vec<(ServerInstanceId, String)>,
}

impl ScopedReportLease {
    pub(super) fn reserve(
        &mut self,
        selected: impl IntoIterator<Item = (ServerInstanceId, String)>,
        per_server_cap: usize,
    ) {
        if let Some(inbox) = self.inbox.upgrade() {
            let mut inbox = inbox.lock();
            for (instance, uri) in selected {
                let slots = inbox.scoped.entry(instance).or_default();
                if !slots.contains_key(&uri) && slots.len() >= per_server_cap {
                    continue;
                }
                slots.entry(uri.clone()).or_default().users += 1;
                self.slots.push((instance, uri));
            }
        }
    }
}

impl Drop for ScopedReportLease {
    fn drop(&mut self) {
        let Some(inbox) = self.inbox.upgrade() else {
            return;
        };
        let mut inbox = inbox.lock();
        for (instance, uri) in &self.slots {
            let slots = inbox.scoped.get_mut(instance).expect("registered instance");
            let slot = slots.get_mut(uri).expect("registered document");
            slot.users -= 1;
            if slot.users == 0 && slot.pending.is_none() {
                slots.remove(uri);
                if slots.is_empty() {
                    inbox.scoped.remove(instance);
                }
            }
        }
    }
}

impl EventReceiver {
    pub(super) fn len(&self) -> usize {
        self.inbox.lock().pending.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(super) fn ready(&self) -> &Receiver<()> {
        &self.ready
    }

    pub(super) fn try_recv(&self) -> Result<ReceivedLspEvent, TryRecvError> {
        let mut inbox = self.inbox.lock();
        let _ = self.ready.try_recv();
        let Some((_, queued)) = inbox.pending.pop_first() else {
            return Err(TryRecvError::Empty);
        };
        if let Some(uri) = queued.scoped_uri {
            let instance = queued.event.instance.expect("scoped publish instance");
            let slots = inbox.scoped.get_mut(&instance).unwrap();
            let slot = slots.get_mut(&uri).unwrap();
            if slot.users == 0 {
                slots.remove(&uri);
                if slots.is_empty() {
                    inbox.scoped.remove(&instance);
                }
            } else {
                slot.pending = None;
            }
        } else {
            inbox.ordinary -= 1;
        }
        if !inbox.pending.is_empty() {
            let _ = inbox.ready.try_send(());
        }
        Ok(queued.event)
    }

    pub(super) fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ReceivedLspEvent, RecvTimeoutError> {
        self.recv_deadline(Instant::now() + timeout)
    }

    pub(super) fn recv_deadline(
        &self,
        deadline: Instant,
    ) -> Result<ReceivedLspEvent, RecvTimeoutError> {
        loop {
            if let Ok(event) = self.try_recv() {
                return Ok(event);
            }
            self.ready.recv_deadline(deadline)?;
        }
    }
}

fn document_publish(event: &LspEvent) -> Option<(&str, Option<i64>)> {
    match event {
        LspEvent::Notification {
            method,
            params: Some(params),
            ..
        } if method == "textDocument/publishDiagnostics" => Some((
            params.get("uri")?.as_str()?,
            params.get("version").and_then(serde_json::Value::as_i64),
        )),
        _ => None,
    }
}

impl Inbox {
    fn push(&mut self, event: ReceivedLspEvent) -> Result<(), TrySendError<ReceivedLspEvent>> {
        let scoped_uri = if let Some((instance, (uri, version))) =
            event.instance.zip(document_publish(&event.event))
        {
            if let Some(slot) = self
                .scoped
                .get_mut(&instance)
                .and_then(|slots| slots.get_mut(uri))
                .filter(|slot| slot.users != 0)
            {
                if let Some(previous) = slot.pending {
                    let previous_version = document_publish(&self.pending[&previous].event.event)
                        .and_then(|(_, version)| version);
                    if previous_version
                        .zip(version)
                        .is_some_and(|(previous, next)| previous > next)
                    {
                        return Ok(());
                    }
                    self.pending.remove(&previous);
                }
                slot.pending = Some(self.next);
                Some(uri.to_owned())
            } else {
                None
            }
        } else {
            None
        };
        if scoped_uri.is_none() {
            if self.ordinary == EVENT_CAP {
                return Err(TrySendError::Full(event));
            }
            self.ordinary += 1;
        }
        // Put a replacement in its new receive position. Progress and
        // registration notifications that came before it must still run first.
        self.pending.insert(self.next, Queued { event, scoped_uri });
        self.next += 1;
        let _ = self.ready.try_send(());
        Ok(())
    }
}

struct Control {
    key: ServerKey,
    last_dropped: u64,
    overflowed: bool,
    progress: Option<AnalysisProgressShadow>,
    progress_limit_exceeded: bool,
    exit: Option<ReceivedLspEvent>,
    watched_dropped: usize,
}

pub(super) enum IngressOverflow {
    Dropped {
        through: u64,
        progress: Option<AnalysisProgressShadow>,
    },
    ProgressLimit,
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
    pub(super) fn new() -> (Self, EventReceiver, Receiver<()>) {
        let (ready, received) = bounded(1);
        let (wake, hints) = bounded(1);
        let inbox = Arc::new(Mutex::new(Inbox {
            ready,
            next: 0,
            ordinary: 0,
            pending: BTreeMap::new(),
            scoped: HashMap::new(),
        }));
        (
            Self {
                inbox: Arc::downgrade(&inbox),
                wake,
                controls: Arc::default(),
            },
            EventReceiver {
                inbox,
                ready: received,
            },
            hints,
        )
    }

    pub(super) fn scoped_report_lease(&self) -> ScopedReportLease {
        ScopedReportLease {
            inbox: self.inbox.clone(),
            slots: Vec::new(),
        }
    }

    pub(super) fn register(&self, instance: ServerInstanceId, key: ServerKey) -> EventIngressLease {
        self.controls.lock().insert(
            instance,
            Control {
                progress: analysis_progress::uses_analysis_progress(&key.kind)
                    .then(AnalysisProgressShadow::default),
                key,
                last_dropped: 0,
                overflowed: false,
                progress_limit_exceeded: false,
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
        if let Some(((instance, order), (token, transition))) = event
            .instance
            .zip(event.receive_order)
            .zip(Transition::from_event(&event.event))
        {
            let mut controls = self.controls.lock();
            if let Some(control) = controls.get_mut(&instance) {
                if control.progress_limit_exceeded {
                    return Ok(());
                }
                if let Some(progress) = &mut control.progress {
                    if !progress.record(token.to_string(), transition, order) {
                        control.progress_limit_exceeded = true;
                        control.overflowed = true;
                        let _ = self.wake.try_send(());
                        return Ok(());
                    }
                }
            }
        }
        let Some(inbox) = self.inbox.upgrade() else {
            return Err(TrySendError::Disconnected(event));
        };
        let result = inbox.lock().push(event);
        match result {
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
        self.controls.lock().get(&id).is_some_and(|control| {
            control.progress_limit_exceeded
                || (control.last_dropped != 0 && order <= control.last_dropped)
        })
    }

    pub(super) fn overflow_pending_for(&self, event: &ReceivedLspEvent) -> bool {
        event.instance.is_some_and(|instance| {
            self.controls
                .lock()
                .get(&instance)
                .is_some_and(|control| control.overflowed)
        })
    }

    pub(super) fn take_overflows(
        &self,
        ready: impl Fn(&ServerKey, ServerInstanceId) -> bool,
    ) -> Vec<(ServerKey, IngressOverflow)> {
        self.controls
            .lock()
            .iter_mut()
            .filter_map(|(&id, control)| {
                if !control.overflowed || !ready(&control.key, id) {
                    return None;
                }
                control.overflowed = false;
                let overflow = if control.progress_limit_exceeded {
                    IngressOverflow::ProgressLimit
                } else {
                    // The snapshot replaces every transition through its last
                    // receipt. Do not replay ordinary events from that interval.
                    if let Some(progress) = &control.progress {
                        control.last_dropped = control.last_dropped.max(progress.last_transition);
                    }
                    IngressOverflow::Dropped {
                        through: control.last_dropped,
                        progress: control.progress.clone(),
                    }
                };
                Some((control.key.clone(), overflow))
            })
            .collect()
    }

    pub(super) fn take_exit(&self) -> Option<ReceivedLspEvent> {
        self.controls
            .lock()
            .values_mut()
            .find_map(|control| control.exit.take())
    }

    pub(super) fn pending(&self, ready: impl Fn(&ServerKey, ServerInstanceId) -> bool) -> bool {
        self.controls.lock().iter().any(|(&id, control)| {
            (control.overflowed && ready(&control.key, id)) || control.exit.is_some()
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::registry::ServerKind;
    use serde_json::json;

    #[test]
    fn scoped_inbox_keeps_the_latest_version_in_receive_order() {
        let (ingress, events, _) = EventIngress::new();
        let key = ServerKey {
            kind: ServerKind::Rust,
            root: "fixture".into(),
        };
        let instance = ServerInstanceId::next();
        let _registration = ingress.register(instance, key.clone());
        let uri = "file:///fixture/lib.rs";
        let notify = |order, method: &str, params| ReceivedLspEvent {
            event: LspEvent::Notification {
                server_kind: key.kind.clone(),
                root: key.root.clone(),
                method: method.into(),
                params,
            },
            instance: Some(instance),
            receive_order: Some(order),
        };
        let publish = |order, version| {
            notify(
                order,
                "textDocument/publishDiagnostics",
                Some(json!({
                    "uri": uri, "version": version, "diagnostics": []
                })),
            )
        };
        let mut lease = ingress.scoped_report_lease();
        lease.reserve([(instance, uri.into())], 1);
        ingress.send(publish(1, 1)).unwrap();
        for order in 2..=EVENT_CAP as u64 + 1 {
            ingress
                .send(notify(order, "fixture/ordinary", None))
                .unwrap();
        }
        let latest_order = EVENT_CAP as u64 + 2;
        ingress.send(publish(latest_order, 3)).unwrap();
        ingress.send(publish(latest_order + 1, 2)).unwrap();
        assert_eq!(events.len(), EVENT_CAP + 1);
        assert!(ingress.take_overflows(|_, _| true).is_empty());
        drop(lease);

        // A pending report survives release, but it cannot jump ahead of
        // ordinary events received before the newer publication.
        for order in 2..latest_order {
            assert_eq!(events.try_recv().unwrap().receive_order, Some(order));
        }
        assert_eq!(events.try_recv().unwrap().receive_order, Some(latest_order));
        assert!(events.is_empty());
        assert!(events.inbox.lock().scoped.is_empty());
    }
}
