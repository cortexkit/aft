//! Age and capacity bounds shared by detached session identities and Push queues.

use super::*;
use std::collections::BTreeMap;
use std::hash::Hash;
use subc_protocol::scope::ScopeStamp;
use tokio::time::Instant as Clock;

pub(super) const TTL: Duration = Duration::from_secs(10 * 60);
pub(super) const KEYS: usize = 256;

struct Deadlines<K> {
    keys: HashMap<K, u64>,
    ordered: BTreeMap<u64, (Clock, K)>,
    sequence: u64,
}

impl<K: PartialEq + Eq + Hash + Clone> Default for Deadlines<K> {
    fn default() -> Self {
        Self {
            keys: HashMap::new(),
            ordered: BTreeMap::new(),
            sequence: 0,
        }
    }
}

impl<K: Eq + Hash + Clone> Deadlines<K> {
    fn arm(&mut self, key: &K) {
        if self.keys.contains_key(key) {
            return;
        }
        let id = self.sequence;
        self.sequence += 1;
        self.keys.insert(key.clone(), id);
        self.ordered.insert(id, (Clock::now() + TTL, key.clone()));
    }

    fn disarm(&mut self, key: &K) {
        if let Some(id) = self.keys.remove(key) {
            self.ordered.remove(&id);
        }
    }

    fn oldest_due(&self) -> Option<&K> {
        let (_, (deadline, key)) = self.ordered.first_key_value()?;
        (self.ordered.len() > KEYS || Clock::now() >= *deadline).then_some(key)
    }
}

type SessionKey = (ProjectRootId, String);

struct RetainedSessionIdentity {
    replay: push::ReplayKey,
    trust: BindTrust,
    scope: Option<scope_watch::Key>,
}

#[derive(Default)]
pub(super) struct SessionIdentities {
    sessions: HashMap<SessionKey, RetainedSessionIdentity>,
    detached: Deadlines<SessionKey>,
}

impl SessionIdentities {
    pub(super) fn remember(&mut self, identity: &RouteIdentity) {
        let key = (identity.root.clone(), identity.session.clone());
        // Keep the verified recipient of legacy task output; an untrusted
        // bind must not replace a retained first-party session identity.
        if matches!(identity.trust, BindTrust::Untrusted)
            && self
                .sessions
                .get(&key)
                .is_some_and(|old| matches!(old.trust, BindTrust::FirstParty))
        {
            return;
        }
        self.detached.disarm(&key);
        self.sessions.insert(
            key,
            RetainedSessionIdentity {
                replay: push::ReplayKey::from_identity(identity),
                trust: identity.trust,
                scope: identity.scope.as_ref().map(scope_describe::stamp_key),
            },
        );
    }

    pub(super) fn detach(&mut self, identity: &RouteIdentity) {
        let key = (identity.root.clone(), identity.session.clone());
        if self.sessions.contains_key(&key) {
            self.detached.arm(&key);
            self.expire();
        }
    }

    pub(super) fn expire(&mut self) {
        while let Some(key) = self.detached.oldest_due().cloned() {
            self.remove(&key);
        }
    }

    fn remove(&mut self, key: &SessionKey) -> Option<push::ReplayKey> {
        self.detached.disarm(key);
        self.sessions.remove(key).map(|identity| identity.replay)
    }

    pub(super) fn remove_root(&mut self, root: &ProjectRootId) {
        let detached = &mut self.detached;
        self.sessions.retain(|key, _| {
            if &key.0 != root {
                return true;
            }
            detached.disarm(key);
            false
        });
    }

    pub(super) fn end_scope(
        &mut self,
        stamp: &ScopeStamp,
        buffer: &mut PushBuffer,
        metrics: &DispatchPathMetrics,
    ) {
        let key = scope_describe::stamp_key(stamp);
        let detached = &mut self.detached;
        self.sessions.retain(|session, identity| {
            if identity.scope.as_ref() != Some(&key) {
                return true;
            }
            detached.disarm(session);
            buffer.discard(metrics, &identity.replay, "scope_ended");
            false
        });
    }

    pub(super) fn replay_key(
        &self,
        root: &ProjectRootId,
        session: &str,
    ) -> Option<(push::ReplayKey, BindTrust)> {
        let identity = self.sessions.get(&(root.clone(), session.to_owned()))?;
        Some((identity.replay.clone(), identity.trust))
    }
}

#[derive(Default)]
pub(super) struct PushBuffer {
    queues: HashMap<push::ReplayKey, VecDeque<PushFrame>>,
    deadlines: Deadlines<push::ReplayKey>,
}

impl PushBuffer {
    pub(super) fn queue(&mut self, key: &push::ReplayKey) -> &mut VecDeque<PushFrame> {
        self.deadlines.arm(key);
        self.queues.entry(key.clone()).or_default()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&push::ReplayKey, &VecDeque<PushFrame>)> {
        self.queues.iter()
    }

    pub(super) fn keys(&self) -> impl Iterator<Item = &push::ReplayKey> {
        self.queues.keys()
    }

    #[cfg(test)]
    pub(super) fn queues_for_test(&self) -> &HashMap<push::ReplayKey, VecDeque<PushFrame>> {
        &self.queues
    }

    pub(super) fn get_mut(&mut self, key: &push::ReplayKey) -> Option<&mut VecDeque<PushFrame>> {
        self.queues.get_mut(key)
    }

    pub(super) fn remove(&mut self, key: &push::ReplayKey) -> Option<VecDeque<PushFrame>> {
        self.deadlines.disarm(key);
        self.queues.remove(key)
    }

    pub(super) fn retain(
        &mut self,
        mut keep: impl FnMut(&push::ReplayKey, &VecDeque<PushFrame>) -> bool,
    ) {
        let deadlines = &mut self.deadlines;
        self.queues.retain(|key, queue| {
            if keep(key, queue) {
                return true;
            }
            deadlines.disarm(key);
            false
        });
    }

    pub(super) fn discard(
        &mut self,
        metrics: &DispatchPathMetrics,
        key: &push::ReplayKey,
        reason: &str,
    ) {
        if let Some(queue) = self.remove(key) {
            for frame in queue {
                push::discard_replay_frame(metrics, key, frame, reason);
            }
        }
    }

    pub(super) fn expire(&mut self, metrics: &DispatchPathMetrics) {
        while let Some(key) = self.deadlines.oldest_due().cloned() {
            self.discard(metrics, &key, "detached_replay_limit");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmed_scope_end_discards_only_its_epoch_replay() {
        let (_dir, root) = test_support::test_root("detached-scope-end");
        let make = |epoch| {
            let stamp = ScopeStamp::new(
                Principal::Direct,
                "shared-ref",
                epoch,
                subc_protocol::scope::ScopeKind::Head,
            );
            let mut route = test_support::route_identity(&root, &format!("scope-session-{epoch}"));
            Arc::get_mut(&mut route.0).unwrap().scope = Some(stamp.clone());
            (route, stamp)
        };
        let (ended, stamp) = make(1);
        let (retained, _) = make(2);
        let mut sessions = SessionIdentities::default();
        let mut buffer = PushBuffer::default();
        for route in [&ended, &retained] {
            sessions.remember(route);
            sessions.detach(route);
            buffer
                .queue(&push::ReplayKey::from_identity(route))
                .push_back(test_support::completion_frame(&format!(
                    "task-{}",
                    route.session
                )));
        }
        sessions.end_scope(&stamp, &mut buffer, &DispatchPathMetrics::new());
        assert!(sessions.replay_key(&root, &ended.session).is_none());
        assert!(sessions.replay_key(&root, &retained.session).is_some());
        assert_eq!(buffer.queues_for_test().len(), 1);
        assert!(buffer
            .queues_for_test()
            .contains_key(&push::ReplayKey::from_identity(&retained)));
    }
}
