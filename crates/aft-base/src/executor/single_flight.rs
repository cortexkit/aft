use std::{collections::HashMap, hash::Hash, sync::Arc};

use parking_lot::{Condvar, Mutex};

/// Generation-aware single-flight cache.
///
/// Calls for the same key and generation share one in-flight build. A newer
/// generation supersedes an older in-flight build; the older result is not
/// installed if the entry has moved on by the time it finishes.
pub struct SingleFlight<K, T> {
    inner: Mutex<HashMap<K, FlightEntry<T>>>,
    changed: Condvar,
}

enum FlightEntry<T> {
    Building { generation: u64 },
    Ready { generation: u64, value: Arc<T> },
}

struct BuildingCleanup<'a, K, T>
where
    K: Clone + Eq + Hash,
{
    flight: &'a SingleFlight<K, T>,
    id: K,
    generation: u64,
    installed: bool,
}

impl<'a, K, T> BuildingCleanup<'a, K, T>
where
    K: Clone + Eq + Hash,
{
    fn new(flight: &'a SingleFlight<K, T>, id: K, generation: u64) -> Self {
        Self {
            flight,
            id,
            generation,
            installed: false,
        }
    }

    fn disarm(&mut self) {
        self.installed = true;
    }
}

impl<K, T> Drop for BuildingCleanup<'_, K, T>
where
    K: Clone + Eq + Hash,
{
    fn drop(&mut self) {
        if !self.installed {
            self.flight.clear_building(&self.id, self.generation);
        }
    }
}

impl<K, T> Default for SingleFlight<K, T>
where
    K: Clone + Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, T> SingleFlight<K, T>
where
    K: Clone + Eq + Hash,
{
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            changed: Condvar::new(),
        }
    }

    /// Return the cached value for `id` at `generation`, or build it once.
    ///
    /// The build function runs outside the internal lock. Concurrent callers for
    /// the same `(id, generation)` wait for the in-flight build and receive the
    /// installed value. If a newer generation supersedes this call while its
    /// build is running, this call returns the newer ready value instead of
    /// overwriting it with stale work.
    ///
    /// If the builder returns an error or panics, the in-flight marker is cleared
    /// and waiters are notified so the key can be retried instead of remaining
    /// permanently stuck in `Building`.
    pub fn get_or_build<E>(
        &self,
        id: K,
        generation: u64,
        build_fn: impl FnOnce() -> Result<T, E>,
    ) -> Result<Arc<T>, E> {
        let mut build_fn = Some(build_fn);

        loop {
            let mut guard = self.inner.lock();
            match guard.get(&id) {
                Some(FlightEntry::Ready {
                    generation: ready_generation,
                    value,
                }) if *ready_generation >= generation => return Ok(Arc::clone(value)),
                Some(FlightEntry::Building {
                    generation: building_generation,
                }) if *building_generation >= generation => {
                    self.changed.wait(&mut guard);
                }
                _ => {
                    guard.insert(id.clone(), FlightEntry::Building { generation });
                    drop(guard);

                    let mut cleanup = BuildingCleanup::new(self, id.clone(), generation);
                    let build = build_fn
                        .take()
                        .expect("single-flight build function used more than once");
                    let built = Arc::new(build()?);

                    let mut superseded = false;
                    loop {
                        let mut guard = self.inner.lock();
                        match guard.get(&id) {
                            Some(FlightEntry::Building {
                                generation: current_generation,
                            }) if *current_generation > generation => {
                                superseded = true;
                                self.changed.wait(&mut guard);
                            }
                            Some(FlightEntry::Ready {
                                generation: current_generation,
                                value,
                            }) if *current_generation >= generation => {
                                let value = Arc::clone(value);
                                cleanup.disarm();
                                self.changed.notify_all();
                                return Ok(value);
                            }
                            _ if superseded => {
                                cleanup.disarm();
                                self.changed.notify_all();
                                return Ok(built);
                            }
                            _ => {
                                guard.insert(
                                    id.clone(),
                                    FlightEntry::Ready {
                                        generation,
                                        value: Arc::clone(&built),
                                    },
                                );
                                cleanup.disarm();
                                self.changed.notify_all();
                                return Ok(built);
                            }
                        }
                    }
                }
            }
        }
    }

    fn clear_building(&self, id: &K, generation: u64) {
        let mut guard = self.inner.lock();
        if matches!(
            guard.get(id),
            Some(FlightEntry::Building {
                generation: current_generation,
            }) if *current_generation == generation
        ) {
            guard.remove(id);
        }
        self.changed.notify_all();
    }
}
