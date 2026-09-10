//! Request-scoped observations for semantic query embeddings.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};
use std::thread::ThreadId;

use serde::{Deserialize, Serialize};

/// Model identity used by the offline search-quality embedding fixture.
pub const FIXTURE_PROVIDER_MODEL: &str = "aft-search-fixture-v1";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EmbedCounts {
    pub requested: u64,
    pub cache_hits: u64,
    pub live_calls: u64,
}

impl EmbedCounts {
    fn add_assign(&mut self, observation: Self) {
        self.requested = self.requested.saturating_add(observation.requested);
        self.cache_hits = self.cache_hits.saturating_add(observation.cache_hits);
        self.live_calls = self.live_calls.saturating_add(observation.live_calls);
    }
}

#[derive(Clone, Debug)]
struct EmbedAttribution {
    request_id: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ExecutionId {
    Task(tokio::task::Id),
    Thread(ThreadId),
}

fn current_execution() -> ExecutionId {
    tokio::task::try_id()
        .map(ExecutionId::Task)
        .unwrap_or_else(|| ExecutionId::Thread(std::thread::current().id()))
}

fn active_attributions() -> &'static Mutex<HashMap<ExecutionId, Vec<EmbedAttribution>>> {
    static ACTIVE: OnceLock<Mutex<HashMap<ExecutionId, Vec<EmbedAttribution>>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn observations() -> &'static Mutex<HashMap<String, EmbedCounts>> {
    static OBSERVATIONS: OnceLock<Mutex<HashMap<String, EmbedCounts>>> = OnceLock::new();
    OBSERVATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Keeps embedding observations attributed to one request task.
///
/// Calls made outside a Tokio task use the current executor thread instead,
/// which keeps the synchronous CLI and unit-test entry points isolated too.
#[must_use]
pub struct Guard {
    execution: ExecutionId,
    request_id: String,
    _not_send: PhantomData<Rc<()>>,
}

/// Installs attribution for a request until the returned guard is dropped.
pub fn install(request_id: impl Into<String>) -> Guard {
    let request_id = request_id.into();
    let execution = current_execution();
    observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(request_id.clone(), EmbedCounts::default());
    active_attributions()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(execution)
        .or_default()
        .push(EmbedAttribution {
            request_id: request_id.clone(),
        });
    Guard {
        execution,
        request_id,
        _not_send: PhantomData,
    }
}

/// Adds embedding counts to the currently attributed request.
pub fn record(counts: EmbedCounts) {
    let execution = current_execution();
    let request_id = active_attributions()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&execution)
        .and_then(|attributions| attributions.last())
        .map(|current| current.request_id.clone());
    let Some(request_id) = request_id else {
        return;
    };

    observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(request_id)
        .or_default()
        .add_assign(counts);
}

/// Returns the observations accumulated for `request_id`.
pub fn read(request_id: &str) -> EmbedCounts {
    observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(request_id)
        .copied()
        .unwrap_or_default()
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut active = active_attributions()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut remove_execution = false;
        if let Some(attributions) = active.get_mut(&self.execution) {
            let popped = attributions.pop();
            debug_assert_eq!(
                popped.as_ref().map(|current| current.request_id.as_str()),
                Some(self.request_id.as_str())
            );
            remove_execution = attributions.is_empty();
        }
        if remove_execution {
            active.remove(&self.execution);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_observations_and_ignores_unattributed_work() {
        record(EmbedCounts {
            requested: 99,
            cache_hits: 99,
            live_calls: 99,
        });

        let request_id = "counter-unit-sum";
        let _guard = install(request_id);
        record(EmbedCounts {
            requested: 2,
            cache_hits: 0,
            live_calls: 1,
        });
        record(EmbedCounts {
            requested: 0,
            cache_hits: 1,
            live_calls: 0,
        });

        assert_eq!(
            read(request_id),
            EmbedCounts {
                requested: 2,
                cache_hits: 1,
                live_calls: 1,
            }
        );
    }

    #[test]
    fn concurrent_requests_remain_isolated() {
        let workers = (0..8)
            .map(|index| {
                std::thread::spawn(move || {
                    let request_id = format!("counter-unit-concurrent-{index}");
                    let _guard = install(&request_id);
                    record(EmbedCounts {
                        requested: index,
                        cache_hits: 1,
                        live_calls: index % 2,
                    });
                    (request_id.clone(), read(&request_id))
                })
            })
            .collect::<Vec<_>>();

        for (index, worker) in workers.into_iter().enumerate() {
            let (request_id, counts) = worker.join().expect("counter worker");
            assert_eq!(request_id, format!("counter-unit-concurrent-{index}"));
            assert_eq!(counts.requested, index as u64);
            assert_eq!(counts.cache_hits, 1);
            assert_eq!(counts.live_calls, (index % 2) as u64);
        }
    }
}
