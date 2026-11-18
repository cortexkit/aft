//! Remote scope descriptions never enter the local owner/ref cleanup fanout.

use super::*;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

type Selector = (String, String);

#[derive(PartialEq, Eq, Hash)]
struct Key {
    pairing: String,
    stamp: (String, String, u64),
}

impl Key {
    fn from_admitted(stamp: &ScopeStamp) -> Self {
        Self {
            pairing: pairing(stamp).to_owned(),
            stamp: stamp_key(stamp),
        }
    }
}

pub(in crate::subc) fn pairing(stamp: &ScopeStamp) -> &str {
    &stamp
        .attributes
        .federated
        .as_ref()
        .expect("admitted scope")
        .pairing_id
}

#[derive(Clone, Default)]
pub(in crate::subc) struct Lifetime(Option<CancellationToken>);

impl Lifetime {
    pub(in crate::subc) fn can_ask(&self) -> bool {
        self.0.is_some()
    }

    pub(in crate::subc) fn ended(&self) -> bool {
        self.0.as_ref().is_some_and(CancellationToken::is_cancelled)
    }
}

pub(in crate::subc) struct Tracker {
    control: Arc<readiness::DaemonControl>,
    owners: HashMap<Selector, String>,
    scopes: HashMap<Key, Lifetime>,
    probes: JoinSet<ScopeStamp>,
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
        Self {
            control,
            owners: HashMap::new(),
            scopes: HashMap::new(),
            probes: JoinSet::new(),
            interval,
        }
    }

    pub(in crate::subc) fn enroll(&mut self, stamp: &ScopeStamp) {
        let key = Key::from_admitted(stamp);
        if self.scopes.contains_key(&key) {
            return;
        }
        // scope.describe sends only owner/ref. Keep the first pairing's
        // reservation even after route close or end: a subsequent query with
        // the same selector could see that pairing's tombstone.
        let selector = (key.stamp.0.clone(), key.stamp.1.clone());
        let owner = self
            .owners
            .entry(selector)
            .or_insert_with(|| key.pairing.clone());
        let lifetime = if *owner != key.pairing {
            log::warn!(
                "Federated scope selector collision: pairing {} cannot use scope.describe reserved by pairing {}",
                key.pairing,
                owner,
            );
            Lifetime::default()
        } else if self
            .control
            .advertises(subc_protocol::scope::SCOPE_DESCRIBE_OP)
        {
            let control = self.control.clone();
            let stamp = stamp.clone();
            let interval = self.interval;
            self.probes.spawn(async move {
                loop {
                    let reply = control.describe(&stamp).await;
                    if reply.as_ref().is_ok_and(describe_answer_ends_scope) {
                        return stamp;
                    }
                    tokio::time::sleep(interval).await;
                }
            });
            Lifetime(Some(CancellationToken::new()))
        } else {
            Lifetime::default()
        };
        self.scopes.insert(key, lifetime);
    }

    pub(in crate::subc) fn lifetime(&self, stamp: &ScopeStamp) -> Lifetime {
        self.scopes
            .get(&Key::from_admitted(stamp))
            .cloned()
            .unwrap_or_default()
    }

    pub(in crate::subc) async fn next(&mut self) -> ScopeStamp {
        loop {
            match self.probes.join_next().await {
                Some(Ok(stamp)) => return stamp,
                Some(Err(error)) => log::warn!("Federated scope describe task failed: {error}"),
                None => std::future::pending().await,
            }
        }
    }

    pub(in crate::subc) fn finish_confirmed_end(&self, stamp: &ScopeStamp) {
        if let Some(token) = self.lifetime(stamp).0 {
            token.cancel();
        }
    }
}

#[cfg(test)]
tokio::task_local! {
    pub(in crate::subc) static TEST_POLL_INTERVAL: Duration;
}
