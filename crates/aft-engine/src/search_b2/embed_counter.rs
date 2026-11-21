//! Request-scoped observations for semantic query embeddings.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
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
    identity: u64,
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

fn observations() -> &'static Mutex<HashMap<(String, u64), EmbedCounts>> {
    static OBSERVATIONS: OnceLock<Mutex<HashMap<(String, u64), EmbedCounts>>> = OnceLock::new();
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
    identity: u64,
    _not_send: PhantomData<Rc<()>>,
}

/// Installs attribution for a request until the returned guard is dropped.
pub fn install(request_id: impl Into<String>) -> Guard {
    let request_id = request_id.into();
    let execution = current_execution();
    static NEXT_IDENTITY: AtomicU64 = AtomicU64::new(0);
    let identity = NEXT_IDENTITY.fetch_add(1, Ordering::Relaxed);
    observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((request_id.clone(), identity), EmbedCounts::default());
    active_attributions()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(execution)
        .or_default()
        .push(EmbedAttribution {
            request_id: request_id.clone(),
            identity,
        });
    Guard {
        execution,
        request_id,
        identity,
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
        .map(|current| (current.request_id.clone(), current.identity));
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

/// Returns observations for the innermost matching request in this execution.
/// Reused request IDs in other tasks must not reset or share these counts.
pub fn read(request_id: &str) -> EmbedCounts {
    let identity = active_attributions()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&current_execution())
        .and_then(|attributions| {
            attributions
                .iter()
                .rev()
                .find(|a| a.request_id == request_id)
        })
        .map(|a| a.identity);
    let Some(identity) = identity else {
        return EmbedCounts::default();
    };
    observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(request_id.to_string(), identity))
        .copied()
        .unwrap_or_default()
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut active = active_attributions()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(attributions) = active.get_mut(&self.execution) {
            attributions.retain(|attribution| attribution.identity != self.identity);
            if attributions.is_empty() {
                active.remove(&self.execution);
            }
        }
        drop(active);
        observations()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(self.request_id.clone(), self.identity));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit10_nested_same_id_counts_are_isolated() {
        let request_id = "counter-reused-id";
        let outer = install(request_id);
        record(EmbedCounts {
            requested: 7,
            ..EmbedCounts::default()
        });
        {
            let _inner = install(request_id);
            assert_eq!(read(request_id).requested, 0);
            record(EmbedCounts {
                requested: 2,
                ..EmbedCounts::default()
            });
            assert_eq!(read(request_id).requested, 2);
        }
        assert_eq!(read(request_id).requested, 7);
        drop(outer);
        assert_eq!(read(request_id), EmbedCounts::default());
    }

    #[test]
    fn audit10_concurrent_same_id_counts_are_isolated() {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let first = barrier.clone();
        let worker = std::thread::spawn(move || {
            let _guard = install("counter-overlap");
            record(EmbedCounts {
                requested: 7,
                ..EmbedCounts::default()
            });
            first.wait();
            first.wait();
            assert_eq!(read("counter-overlap").requested, 7);
        });
        barrier.wait();
        {
            let _guard = install("counter-overlap");
            record(EmbedCounts {
                requested: 2,
                ..EmbedCounts::default()
            });
            assert_eq!(read("counter-overlap").requested, 2);
        }
        barrier.wait();
        worker.join().unwrap();
    }

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

    #[test]
    fn audit_growth_completed_request_observations_are_released() {
        let prefix = format!("counter-unit-growth-{}-", std::process::id());
        for index in 0..128 {
            let request_id = format!("{prefix}{index}");
            {
                let _guard = install(&request_id);
                record(EmbedCounts {
                    requested: 1,
                    ..EmbedCounts::default()
                });
                assert_eq!(read(&request_id).requested, 1);
            }
            assert_eq!(read(&request_id), EmbedCounts::default());
        }
        let retained = observations()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .filter(|(request_id, _)| request_id.starts_with(&prefix))
            .count();
        assert_eq!(
            retained, 0,
            "completed request observations must be released"
        );
    }
}
