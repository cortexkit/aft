use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TryRecvError, TrySendError};
use parking_lot::Mutex;

use super::analysis_progress::{self, AnalysisProgressShadow, Transition};
use super::client::{LspEvent, ReceivedLspEvent, ServerInstanceId};
use super::roots::ServerKey;

pub(super) const EVENT_CAP: usize = 64;
// Give a scoped document room for its open reports and a late close reply.
const SCOPED_EVENTS_PER_DOCUMENT: usize = 4;

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
    pending: VecDeque<ReceivedLspEvent>,
    reservations: HashMap<(ServerInstanceId, String), usize>,
}

#[derive(Debug)]
pub(crate) struct ScopedReportLease {
    inbox: Weak<Mutex<Inbox>>,
    reservations: Vec<(ServerInstanceId, String)>,
}

impl ScopedReportLease {
    pub(super) fn reserve(
        &mut self,
        selected: impl IntoIterator<Item = (ServerInstanceId, String)>,
        reservation_cap: usize,
    ) {
        if let Some(inbox) = self.inbox.upgrade() {
            let mut inbox = inbox.lock();
            for document in selected {
                if inbox.reservations.len() >= reservation_cap
                    && !inbox.reservations.contains_key(&document)
                {
                    continue;
                }
                *inbox.reservations.entry(document.clone()).or_default() += 1;
                self.reservations.push(document);
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
        for document in &self.reservations {
            let users = inbox
                .reservations
                .get_mut(document)
                .expect("reserved document");
            *users -= 1;
            if *users == 0 {
                inbox.reservations.remove(document);
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
        let Some(event) = inbox.pending.pop_front() else {
            return Err(TryRecvError::Empty);
        };
        if !inbox.pending.is_empty() {
            let _ = inbox.ready.try_send(());
        }
        Ok(event)
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

impl Inbox {
    fn push(&mut self, event: ReceivedLspEvent) -> Result<(), TrySendError<ReceivedLspEvent>> {
        let limit = EVENT_CAP + self.reservations.len() * SCOPED_EVENTS_PER_DOCUMENT;
        // A lease may have ended with events still queued. Keep them in order
        // and resume accepting new events once the queue is below the limit.
        if self.pending.len() >= limit {
            return Err(TrySendError::Full(event));
        }
        self.pending.push_back(event);
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
            pending: VecDeque::new(),
            reservations: HashMap::new(),
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
            reservations: Vec::new(),
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
    fn scoped_inbox_preserves_every_publish_in_receive_order() {
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
        let newest_order = EVENT_CAP as u64 + 2;
        ingress.send(publish(newest_order, 3)).unwrap();
        ingress.send(publish(newest_order + 1, 2)).unwrap();
        assert_eq!(events.len(), EVENT_CAP + 3);
        assert!(ingress.take_overflows(|_, _| true).is_empty());

        for order in 1..=newest_order + 1 {
            assert_eq!(events.try_recv().unwrap().receive_order, Some(order));
        }
        assert!(events.is_empty());
    }

    #[test]
    fn releasing_the_last_scoped_lease_returns_capacity_without_removing_events() {
        let (ingress, events, _) = EventIngress::new();
        let key = ServerKey {
            kind: ServerKind::Rust,
            root: "fixture".into(),
        };
        let instance = ServerInstanceId::next();
        let _registration = ingress.register(instance, key.clone());
        let notify = |order| ReceivedLspEvent {
            event: LspEvent::Notification {
                server_kind: key.kind.clone(),
                root: key.root.clone(),
                method: "fixture/ordinary".into(),
                params: None,
            },
            instance: Some(instance),
            receive_order: Some(order),
        };
        let mut first = ingress.scoped_report_lease();
        let mut second = ingress.scoped_report_lease();
        let document = (instance, "file:///fixture/lib.rs".to_owned());
        let another = (ServerInstanceId::next(), document.1.clone());
        first.reserve([document.clone()], 1);
        second.reserve([document, another], 1);
        assert_eq!(events.inbox.lock().reservations.len(), 1);
        drop(first);

        let raised_limit = EVENT_CAP + SCOPED_EVENTS_PER_DOCUMENT;
        for order in 1..=raised_limit as u64 {
            ingress.send(notify(order)).unwrap();
        }
        assert_eq!(events.len(), raised_limit);
        assert!(ingress.take_overflows(|_, _| true).is_empty());

        drop(second);
        assert!(events.inbox.lock().reservations.is_empty());
        for order in 1..=2 {
            assert_eq!(events.try_recv().unwrap().receive_order, Some(order));
        }
        // There would be room if the old raised limit were still in effect.
        let rejected = raised_limit as u64 + 1;
        ingress.send(notify(rejected)).unwrap();
        assert_eq!(events.len(), raised_limit - 2);
        assert!(matches!(
            ingress.take_overflows(|_, _| true).as_slice(),
            [(_, IngressOverflow::Dropped { through, .. })] if *through == rejected
        ));
        for order in 3..=(SCOPED_EVENTS_PER_DOCUMENT + 1) as u64 {
            assert_eq!(events.try_recv().unwrap().receive_order, Some(order));
        }
        assert_eq!(events.len(), EVENT_CAP - 1);
        ingress.send(notify(rejected + 1)).unwrap();
        assert_eq!(events.len(), EVENT_CAP);
        assert!(ingress.take_overflows(|_, _| true).is_empty());
    }
}
