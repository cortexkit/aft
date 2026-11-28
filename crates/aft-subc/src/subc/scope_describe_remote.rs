//! Remote scope descriptions never enter the local owner/ref cleanup fanout.

use super::*;
use std::sync::Weak;
use tokio::{sync::watch, task::JoinSet};
use tokio_util::sync::CancellationToken;

type Selector = (String, String);

fn selector(stamp: &ScopeStamp) -> Selector {
    let (owner, reference, _) = stamp_key(stamp);
    (owner, reference)
}

pub(in crate::subc) fn pairing(stamp: &ScopeStamp) -> &str {
    &stamp
        .attributes
        .federated
        .as_ref()
        .expect("admitted scope")
        .pairing_id
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Validity {
    Checking,
    Live,
    Unverified,
    Ended,
}

struct Resident {
    state: watch::Sender<Validity>,
    ended: CancellationToken,
    wake: Arc<Notify>,
}

impl Resident {
    fn confirm_end(&self) {
        self.state.send_replace(Validity::Ended);
        self.ended.cancel();
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        self.wake.notify_waiters();
    }
}

#[derive(Clone, Default)]
pub(in crate::subc) struct Lifetime(Option<Arc<Resident>>);

impl Lifetime {
    pub(in crate::subc) fn can_ask(&self) -> bool {
        self.0.is_some()
    }

    pub(in crate::subc) fn ended(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|resident| resident.ended.is_cancelled())
    }

    pub(in crate::subc) async fn wait_for_live(&self) -> bool {
        let Some(resident) = self.0.as_ref() else {
            // Without a verified selector, the existing no-ask policy still
            // allows using an already saved folder approval.
            return true;
        };
        let mut state = resident.state.subscribe();
        state
            .wait_for(|state| *state != Validity::Checking)
            .await
            .is_ok_and(|state| *state == Validity::Live)
    }

    pub(in crate::subc) async fn cancelled(&self) {
        match &self.0 {
            Some(resident) => resident.ended.cancelled().await,
            None => std::future::pending().await,
        }
    }
}

type Epochs = HashMap<u64, (ScopeStamp, Weak<Resident>)>;

struct Group {
    // None means the worker retired. A new admission cannot revive it.
    epochs: parking_lot::Mutex<Option<Epochs>>,
    wake: Arc<Notify>,
}

impl Group {
    fn new() -> Self {
        Self {
            epochs: parking_lot::Mutex::new(Some(HashMap::new())),
            wake: Arc::new(Notify::new()),
        }
    }

    fn admit(&self, stamp: &ScopeStamp) -> Option<Lifetime> {
        let mut slot = self.epochs.lock();
        let epochs = slot.as_mut()?;
        if let Some(existing) = epochs
            .get(&stamp.scope_epoch)
            .and_then(|(_, life)| life.upgrade())
        {
            return Some(Lifetime(Some(existing)));
        }
        let resident = Arc::new(Resident {
            state: watch::channel(Validity::Checking).0,
            ended: CancellationToken::new(),
            wake: self.wake.clone(),
        });
        epochs.insert(
            stamp.scope_epoch,
            (stamp.clone(), Arc::downgrade(&resident)),
        );
        self.wake.notify_waiters();
        Some(Lifetime(Some(resident)))
    }

    async fn poll(
        &self,
        control: &readiness::DaemonControl,
        ends: &mpsc::UnboundedSender<ScopeStamp>,
        interval: Duration,
    ) {
        loop {
            let changed = self.wake.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let sent = {
                let mut slot = self.epochs.lock();
                let epochs = slot.as_mut().expect("only this worker retires the group");
                epochs.retain(|_, (_, life)| life.strong_count() != 0);
                if epochs.is_empty() {
                    *slot = None;
                    return;
                }
                epochs.values().cloned().collect::<Vec<_>>()
            };
            let reply = control.describe(&sent[0].0).await;
            if reply.as_ref().is_ok_and(describe_answer_ends_scope) {
                let mut slot = self.epochs.lock();
                let epochs = slot.as_mut().expect("active describe group");
                for (stamp, life) in sent {
                    if let Some(resident) = life.upgrade() {
                        resident.confirm_end();
                    }
                    // Do not remove entries replaced while this request was
                    // in flight. The ended notice carries each exact stamp
                    // so settlement can revoke that epoch's calls and grants.
                    if epochs
                        .get(&stamp.scope_epoch)
                        .is_some_and(|(_, current)| Weak::ptr_eq(&life, current))
                    {
                        epochs.remove(&stamp.scope_epoch);
                    }
                    let _ = ends.send(stamp);
                }
                continue;
            }
            for (stamp, resident) in sent {
                let Some(resident) = resident.upgrade() else {
                    continue;
                };
                let live = matches!(&reply, Ok(ModuleControlResponseToModule::ScopeDescribe {
                    status: subc_protocol::scope::ScopeStatus::Live,
                    scope_epoch: Some(epoch), owner_configured: true, ..
                }) if *epoch == stamp.scope_epoch);
                resident.state.send_if_modified(|state| {
                    if *state != Validity::Checking {
                        return false;
                    }
                    *state = if live {
                        Validity::Live
                    } else {
                        Validity::Unverified
                    };
                    true
                });
            }
            tokio::select! {
                _ = &mut changed => {},
                () = tokio::time::sleep(interval) => {},
            }
        }
    }
}

pub(in crate::subc) struct Tracker {
    control: Arc<readiness::DaemonControl>,
    owners: HashMap<Selector, String>,
    groups: HashMap<Selector, Arc<Group>>,
    probes: JoinSet<(Selector, Arc<Group>)>,
    ends: mpsc::UnboundedSender<ScopeStamp>,
    ended_notices: mpsc::UnboundedReceiver<ScopeStamp>,
    ended: CompletedEnds,
    interval: Duration,
}

impl Tracker {
    pub(in crate::subc) fn new(control: Arc<readiness::DaemonControl>) -> Self {
        #[cfg(test)]
        let interval = TEST_POLL_INTERVAL
            .try_with(|interval| *interval)
            .unwrap_or_else(|_| poll_interval());
        #[cfg(not(test))]
        let interval = poll_interval();
        let (ends, ended_notices) = mpsc::unbounded_channel();
        Self {
            control,
            owners: HashMap::new(),
            groups: HashMap::new(),
            probes: JoinSet::new(),
            ends,
            ended_notices,
            ended: CompletedEnds::default(),
            interval,
        }
    }

    pub(in crate::subc) fn enroll(&mut self, stamp: &ScopeStamp) {
        // The daemon selects a scope by owner and reference, not by
        // pairing. Keep the first pairing's reservation even after probes
        // retire or their ended-epoch records expire.
        let owner = self
            .owners
            .entry(selector(stamp))
            .or_insert_with(|| pairing(stamp).to_owned());
        if owner != pairing(stamp) {
            log::warn!(
                "Federated scope selector collision: pairing {} cannot use scope.describe reserved by pairing {}",
                pairing(stamp),
                owner,
            );
        }
    }

    pub(in crate::subc) fn begin_work(&mut self, stamp: &ScopeStamp) -> Lifetime {
        let selector = selector(stamp);
        if !self.owners.contains_key(&selector) {
            self.enroll(stamp);
        }
        if self.owners[&selector] != pairing(stamp)
            || !self
                .control
                .advertises(subc_protocol::scope::SCOPE_DESCRIBE_OP)
        {
            return Lifetime::default();
        }
        if self.ended.contains(&stamp_key(stamp)) {
            let resident = Arc::new(Resident {
                ended: CancellationToken::new(),
                state: watch::channel(Validity::Ended).0,
                wake: Arc::new(Notify::new()),
            });
            resident.confirm_end();
            return Lifetime(Some(resident));
        }
        if let Some(lifetime) = self
            .groups
            .get(&selector)
            .and_then(|group| group.admit(stamp))
        {
            return lifetime;
        }
        let group = Arc::new(Group::new());
        let lifetime = group.admit(stamp).expect("new group can admit");
        self.groups.insert(selector.clone(), group.clone());
        let control = self.control.clone();
        let ended = self.ends.clone();
        let interval = self.interval;
        self.probes.spawn(async move {
            group.poll(&control, &ended, interval).await;
            (selector, group)
        });
        lifetime
    }

    pub(in crate::subc) async fn next(&mut self) -> ScopeStamp {
        loop {
            tokio::select! {
                Some(stamp) = self.ended_notices.recv() => return stamp,
                Some(result) = self.probes.join_next() => match result {
                    Ok((selector, retired)) => {
                        if self.groups.get(&selector).is_some_and(|current| Arc::ptr_eq(current, &retired)) {
                            self.groups.remove(&selector);
                        }
                    }
                    Err(error) => log::warn!("Federated scope describe task failed: {error}"),
                },
            }
        }
    }

    pub(in crate::subc) fn finish_confirmed_end(&mut self, stamp: &ScopeStamp) {
        self.ended.insert(stamp_key(stamp));
        if let Some(group) = self.groups.get(&selector(stamp)) {
            if let Some((_, resident)) = group
                .epochs
                .lock()
                .as_mut()
                .and_then(|epochs| epochs.remove(&stamp.scope_epoch))
            {
                if let Some(resident) = resident.upgrade() {
                    resident.confirm_end();
                }
            }
            group.wake.notify_waiters();
        }
    }

    #[cfg(test)]
    pub(in crate::subc) async fn await_idle_probe_for_test(&mut self) {
        let (_, group) = self.probes.join_next().await.unwrap().unwrap();
        assert!(group.epochs.lock().is_none());
    }
}

#[cfg(test)]
tokio::task_local! {
    pub(in crate::subc) static TEST_POLL_INTERVAL: Duration;
}

#[cfg(test)]
pub(in crate::subc) fn test_tracker() -> (
    Tracker,
    readiness::PendingControlReplies,
    mpsc::Receiver<WriterFrame>,
) {
    let (writer, frames) = mpsc::channel(32);
    let pending = readiness::PendingControlReplies::default();
    let tracker = Tracker::new(Arc::new(readiness::DaemonControl::new(
        writer,
        Arc::new(DispatchPathMetrics::new()),
        PROTOCOL_VERSION,
        vec![subc_protocol::scope::SCOPE_DESCRIBE_OP.into()],
        vec![],
        pending.clone(),
        HELLO_CORR + 1,
    )));
    (tracker, pending, frames)
}

#[cfg(test)]
pub(in crate::subc) fn test_answer(
    pending: &readiness::PendingControlReplies,
    frame: &Frame,
    status: subc_protocol::scope::ScopeStatus,
    scope_epoch: Option<u64>,
) {
    assert!(pending.resolve(
        Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            frame.header.corr,
            serde_json::to_vec(&ModuleControlResponseToModule::ScopeDescribe {
                status,
                scope_epoch,
                daemon_incarnation: "test-daemon".into(),
                owner_synced: true,
                owner_configured: true,
                scope: None,
            })
            .unwrap(),
        )
        .unwrap()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(epoch: u64) -> ScopeStamp {
        serde_json::from_value(json!({
            "owner":{"kind":"reserved", "module_id":"callosum"},
            "ref":"remote-session", "kind":"federated", "scope_epoch":epoch,
            "owner_authorized":true,
            "attributes":{"federated":{
                "peer_class":"computer", "peer_fingerprint":"a".repeat(32),
                "peer_box_id":"b".repeat(32), "peer_name":"Computer A", "pairing_id":"pairing-A",
                "origin_principal":{"kind":"reserved", "module_id":"llm-runner"},
                "project":{"project_id":"project", "root_key":{"kind":"label", "key":"root"}},
                "remote_claim":{"kind":"agent", "agent_id":"agent"}
            }}
        }))
        .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn detached_unused_remote_epochs_start_no_probes() {
        let (mut tracker, _pending, _frames) = test_tracker();
        for epoch in 1..=100 {
            tracker.enroll(&stamp(epoch));
        }
        assert_eq!(tracker.probes.len(), 0);
        assert_eq!(tracker.owners.len(), 1, "keep the pairing collision fence");
    }

    #[tokio::test(start_paused = true)]
    async fn retained_remote_epochs_share_one_probe_and_confirmed_end() {
        use subc_protocol::scope::ScopeStatus;
        let (mut tracker, pending, mut frames) = test_tracker();
        let first = tracker.begin_work(&stamp(1));
        let frame = frames.recv().await.unwrap();
        test_answer(&pending, &frame, ScopeStatus::Live, Some(1));
        assert!(first.wait_for_live().await);
        let second = tracker.begin_work(&stamp(2));
        assert_eq!(tracker.probes.len(), 1);
        let frame = frames.recv().await.unwrap();
        test_answer(&pending, &frame, ScopeStatus::Live, Some(2));
        assert!(second.wait_for_live().await);
        assert!(frames.try_recv().is_err());
        tokio::time::advance(tracker.interval).await;
        let frame = frames.recv().await.unwrap();
        test_answer(&pending, &frame, ScopeStatus::Ended, None);
        let epochs: HashSet<_> = [
            tracker.next().await.scope_epoch,
            tracker.next().await.scope_epoch,
        ]
        .into_iter()
        .collect();
        assert_eq!(epochs, HashSet::from([1, 2]));
        assert!(first.ended() && second.ended());
    }

    #[tokio::test(start_paused = true)]
    async fn evicted_remote_end_requires_fresh_live_proof_and_keeps_pairing_fence() {
        use subc_protocol::scope::ScopeStatus;
        let (mut tracker, pending, mut frames) = test_tracker();
        let ended = stamp(1);
        tracker.enroll(&ended);
        tracker.finish_confirmed_end(&ended);
        assert!(tracker.begin_work(&ended).ended());
        tokio::time::advance(COMPLETED_END_TTL).await;
        let mut collision = ended.clone();
        collision.attributes.federated.as_mut().unwrap().pairing_id = "other".into();
        tracker.enroll(&collision);
        assert!(!tracker.begin_work(&collision).can_ask());
        assert_eq!(tracker.owners[&selector(&ended)], "pairing-A");
        assert_eq!(tracker.probes.len(), 0);

        let cold = tracker.begin_work(&ended);
        let frame = frames.recv().await.unwrap();
        test_answer(&pending, &frame, ScopeStatus::Ended, None);
        assert!(!cold.wait_for_live().await);
        assert!(cold.ended());
        let notice = tracker.next().await;
        tracker.finish_confirmed_end(&notice);
        tokio::time::advance(COMPLETED_END_TTL).await;
        let live = tracker.begin_work(&ended);
        let frame = frames.recv().await.unwrap();
        test_answer(&pending, &frame, ScopeStatus::Live, Some(1));
        assert!(live.wait_for_live().await);
    }
}
