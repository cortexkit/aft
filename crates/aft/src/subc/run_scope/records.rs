use super::*;
use std::sync::Weak;
use subc_protocol::scope::ScopeStamp;

type Key = scope_watch::Key;
const END_FENCE_CAP: usize = 4_096;

#[derive(Default)]
pub(super) struct Records {
    live: HashMap<Key, Weak<RunScope>>,
    parked: HashMap<Key, Arc<RunScope>>,
    ended: HashSet<Key>,
    end_order: VecDeque<Key>,
}

#[derive(Default)]
pub(super) struct Registry(Arc<StdMutex<Records>>);

impl Registry {
    pub(super) fn revoke(&self, stamp: &ScopeStamp) {
        let key = scope_describe::stamp_key(stamp);
        let scope = {
            let mut records = self.0.lock().unwrap();
            if records.ended.insert(key.clone()) {
                records.end_order.push_back(key.clone());
                if records.end_order.len() > END_FENCE_CAP {
                    let earliest = records.end_order.pop_front().unwrap();
                    records.ended.remove(&earliest);
                }
            }
            records.live.get(&key).and_then(Weak::upgrade)
        };
        if let Some(scope) = scope {
            scope.revoke();
        }
    }

    pub(super) fn bind(
        &self,
        stamp: &ScopeStamp,
        control: &Arc<readiness::DaemonControl>,
    ) -> Arc<RunScope> {
        let key = scope_describe::stamp_key(stamp);
        let mut records = self.0.lock().unwrap();
        if let Some(live) = records.live.get(&key).and_then(Weak::upgrade) {
            return live;
        }
        let scope = Arc::new(RunScope {
            stamp: stamp.clone(),
            control: control.clone(),
            describe_lock: tokio::sync::Mutex::new(()),
            budget: StdMutex::new(None),
            revoked: AtomicBool::new(records.ended.contains(&key)),
            authorization: StdMutex::new(()),
            parked_asks: StdMutex::new(Vec::new()),
            permission_budget: Arc::default(),
            registration: Registration {
                registry: Arc::downgrade(&self.0),
                key: key.clone(),
            },
        });
        records.live.insert(key, Arc::downgrade(&scope));
        scope
    }

    pub(super) fn parked_scopes(&self) -> Vec<ScopeStamp> {
        self.0
            .lock()
            .unwrap()
            .parked
            .values()
            .filter(|scope| !scope.revoked())
            .map(|scope| scope.stamp.clone())
            .collect()
    }

    #[cfg(test)]
    pub(super) fn counts(&self) -> (usize, usize) {
        let records = self.0.lock().unwrap();
        (records.live.len(), records.ended.len())
    }
}

#[derive(Default)]
pub(super) struct Registration {
    registry: Weak<StdMutex<Records>>,
    key: Key,
}

impl Registration {
    pub(super) fn keep_parked(&self, scope: &Arc<RunScope>) {
        if let Some(registry) = self.registry.upgrade() {
            registry
                .lock()
                .unwrap()
                .parked
                .entry(self.key.clone())
                .or_insert_with(|| scope.clone());
        }
    }

    pub(super) fn release_parked(&self) {
        if let Some(registry) = self.registry.upgrade() {
            let parked = registry.lock().unwrap().parked.remove(&self.key);
            drop(parked);
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut records = registry.lock().unwrap();
            // A new bind could already have replaced this last owner's weak entry.
            if records
                .live
                .get(&self.key)
                .is_some_and(|scope| scope.strong_count() == 0)
            {
                records.live.remove(&self.key);
            }
        }
    }
}
