//! Byte-bounded cache of projections reconstructed from one publication.

use aft_store::heap::{HeapBytes, Seen};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

const CHURN_FLOOR: usize = 4096;

#[derive(Clone, Copy, Debug)]
pub(in crate::data_layer) struct CacheChurn {
    pub(in crate::data_layer) redecodes: usize,
    pub(in crate::data_layer) distinct: usize,
    pub(in crate::data_layer) budget: usize,
}

#[derive(Default)]
struct Pass {
    seen: HashSet<Arc<str>>,
    path_bytes: usize,
    redecodes: usize,
    oversized_redecodes: usize,
    decodes: usize,
    joined: usize,
}

impl Pass {
    fn note(&mut self, path: Arc<str>) -> bool {
        let bytes = path.len() + aft_store::heap::COUNTS;
        let first = self.seen.insert(path);
        if first {
            self.path_bytes += bytes;
        }
        first
    }

    fn computed(&mut self, path: Arc<str>, admissible: bool, budget: usize) -> Option<CacheChurn> {
        self.decodes += 1;
        if !self.note(path) {
            if admissible {
                self.redecodes += 1;
            } else {
                self.oversized_redecodes += 1;
            }
        }
        let distinct = self.seen.len();
        (self.redecodes >= CHURN_FLOOR && self.redecodes > distinct).then_some(CacheChurn {
            redecodes: self.redecodes,
            distinct,
            budget,
        })
    }
}

type Flight<T> = Arc<OnceLock<Option<Arc<T>>>>;

enum Read<T> {
    Cached(Arc<T>),
    InFlight(Arc<str>, Flight<T>),
}

struct Entry<T> {
    value: Arc<T>,
    size: usize,
    used: u64,
    seen_in_pass: bool,
}

struct State<T> {
    entries: HashMap<Arc<str>, Entry<T>>,
    order: BTreeMap<u64, Arc<str>>,
    flights: HashMap<Arc<str>, Flight<T>>,
    pass: Option<Pass>,
    held: usize,
    peak: usize,
    peak_table: usize,
    evicted: usize,
    clock: u64,
}

impl<T> Default for State<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            flights: HashMap::new(),
            pass: None,
            held: 0,
            peak: 0,
            peak_table: 0,
            evicted: 0,
            clock: 0,
        }
    }
}

impl<T> State<T> {
    fn table_bytes(&self) -> usize {
        fn table<E>(capacity: usize) -> usize {
            // Include unfilled buckets and their control bytes in the estimate.
            if capacity == 0 {
                0
            } else {
                (capacity + 1).next_power_of_two() * (size_of::<E>() + 1)
            }
        }
        table::<(Arc<str>, Entry<T>)>(self.entries.capacity())
            + aft_store::heap::btree_bytes(
                self.order.len(),
                size_of::<u64>() + size_of::<Arc<str>>(),
            )
            + self.entries.len() * aft_store::heap::COUNTS
            + table::<(Arc<str>, Flight<T>)>(self.flights.capacity())
            + self.flights.len() * (size_of::<OnceLock<Option<Arc<T>>>>() + aft_store::heap::COUNTS)
            // Counting the pass's keys here is conservative: some are still
            // shared with entries whose payload charge includes the path.
            + self.pass.as_ref().map_or(0, |pass| table::<Arc<str>>(pass.seen.capacity()) + pass.path_bytes)
    }

    fn touch(&mut self, path: &str) -> Option<Arc<T>> {
        let entry = self.entries.get_mut(path)?;
        let key = self
            .order
            .remove(&entry.used)
            .expect("cache entry is ordered");
        self.clock += 1;
        entry.used = self.clock;
        let mut first = false;
        if let Some(pass) = &mut self.pass {
            if !std::mem::replace(&mut entry.seen_in_pass, true) {
                first = pass.note(Arc::clone(&key));
            }
        }
        self.order.insert(entry.used, key);
        let value = Arc::clone(&entry.value);
        if first {
            self.peak_table = self.peak_table.max(self.table_bytes());
        }
        Some(value)
    }

    fn remove(&mut self, path: &str) {
        if let Some(entry) = self.entries.remove(path) {
            self.order.remove(&entry.used);
            self.held -= entry.size;
        }
    }
}

pub(super) struct DecodedCache<T> {
    budget: usize,
    state: Mutex<State<T>>,
    churn: OnceLock<CacheChurn>,
    #[cfg(test)]
    lookups: std::sync::atomic::AtomicUsize,
}

impl<T> Default for DecodedCache<T> {
    fn default() -> Self {
        Self::new(super::bind_cache_bytes())
    }
}

impl<T> DecodedCache<T> {
    pub(super) fn new(budget: usize) -> Self {
        Self {
            budget,
            state: Mutex::default(),
            churn: OnceLock::new(),
            #[cfg(test)]
            lookups: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(super) fn begin_pass(&mut self, track_churn: bool) {
        self.churn.take();
        let state = self.state.get_mut().expect("decoded cache lock");
        state.pass = track_churn.then(Pass::default);
        state.evicted = 0;
        state.peak = state.held;
        state.peak_table = state.table_bytes();
        for entry in state.entries.values_mut() {
            entry.seen_in_pass = false;
        }
    }

    pub(super) fn end_pass(&mut self) {
        let state = self.state.get_mut().expect("decoded cache lock");
        #[cfg(feature = "test-hooks")]
        if let Some(pass) = &state.pass {
            if std::env::var_os("AFT_REFRESH_PROFILE").is_some() {
                log::info!(
                    "refresh declarer cache: held={} peak={} evicted={} table={} peak_table={} decodes={} distinct={} redecodes={} oversized_redecodes={} joined={} budget={}",
                    state.held, state.peak, state.evicted, state.table_bytes(), state.peak_table,
                    pass.decodes, pass.seen.len(), pass.redecodes, pass.oversized_redecodes, pass.joined, self.budget
                );
            }
        }
        state.pass = None;
    }

    pub(super) fn churn(&self) -> Option<CacheChurn> {
        self.churn.get().copied()
    }

    pub(super) fn get(&self, path: &str) -> Option<Arc<T>> {
        self.state.lock().expect("decoded cache lock").touch(path)
    }

    fn lookup(&self, path: &str) -> Read<T> {
        #[cfg(test)]
        self.lookups
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut state = self.state.lock().expect("decoded cache lock");
        if let Some(value) = state.touch(path) {
            return Read::Cached(value);
        }
        if let Some((key, flight)) = state.flights.get_key_value(path) {
            let read = Read::InFlight(Arc::clone(key), Arc::clone(flight));
            if let Some(pass) = &mut state.pass {
                pass.joined += 1;
            }
            return read;
        }
        let path = state
            .pass
            .as_ref()
            .and_then(|pass| pass.seen.get(path))
            .map_or_else(|| Arc::from(path), Arc::clone);
        let flight = Arc::new(OnceLock::new());
        state.flights.insert(Arc::clone(&path), Arc::clone(&flight));
        state.peak_table = state.peak_table.max(state.table_bytes());
        Read::InFlight(path, flight)
    }

    #[cfg(test)]
    pub(super) fn lookups(&self) -> usize {
        self.lookups.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn peek(&self, path: &str) -> Option<Arc<T>> {
        self.state
            .lock()
            .expect("decoded cache lock")
            .entries
            .get(path)
            .map(|entry| Arc::clone(&entry.value))
    }

    pub(super) fn remove(&mut self, path: &str) {
        self.state
            .get_mut()
            .expect("decoded cache lock")
            .remove(path);
    }

    #[cfg(test)]
    pub(super) fn counts(&self) -> (usize, usize, usize, usize, usize) {
        let state = self.state.lock().expect("decoded cache lock");
        (
            state.held,
            state.peak,
            state.evicted,
            state.table_bytes(),
            state.peak_table,
        )
    }
}

impl<T: HeapBytes> DecodedCache<T> {
    pub(super) fn get_or_project(
        &self,
        path: &str,
        project: impl FnOnce() -> Option<Arc<T>>,
    ) -> Option<Arc<T>> {
        self.read(self.lookup(path), project)
    }

    fn read(&self, read: Read<T>, project: impl FnOnce() -> Option<Arc<T>>) -> Option<Arc<T>> {
        let (path, flight) = match read {
            Read::Cached(value) => return Some(value),
            Read::InFlight(path, flight) => (path, flight),
        };
        // Never hold the table lock while projecting or waiting. The cell
        // shares the result even when it is too large to admit to the cache.
        flight
            .get_or_init(|| {
                let value = project();
                let size = value
                    .as_ref()
                    .map(|value| value.heap_bytes(&mut Seen::new()) + path.len());
                let mut state = self.state.lock().expect("decoded cache lock");
                if let Some((value, size)) = value.as_ref().zip(size) {
                    let admissible = size <= self.budget;
                    if let Some(pass) = &mut state.pass {
                        if let Some(churn) =
                            pass.computed(Arc::clone(&path), admissible, self.budget)
                        {
                            let _ = self.churn.set(churn);
                        }
                    }
                    if admissible {
                        Self::install(&mut state, Arc::clone(&path), value, size, self.budget);
                    }
                }
                state.flights.remove(path.as_ref());
                state.peak_table = state.peak_table.max(state.table_bytes());
                value
            })
            .clone()
    }

    fn install(state: &mut State<T>, path: Arc<str>, value: &Arc<T>, size: usize, budget: usize) {
        while state.held > budget - size {
            let (_, path) = state.order.pop_first().expect("cache is over budget");
            state.remove(&path);
            state.evicted += 1;
        }
        state.clock += 1;
        let used = state.clock;
        state.order.insert(used, Arc::clone(&path));
        state.entries.insert(
            path,
            Entry {
                value: Arc::clone(value),
                size,
                used,
                seen_in_pass: state.pass.is_some(),
            },
        );
        state.held += size;
        state.peak = state.peak.max(state.held);
    }
}

impl<T: HeapBytes> HeapBytes for DecodedCache<T> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        let state = self.state.lock().expect("decoded cache lock");
        state
            .entries
            .iter()
            .map(|(path, entry)| path.len() + entry.value.heap_bytes(seen))
            .sum::<usize>()
            + state.table_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use std::sync::Barrier;

    #[test]
    fn concurrent_misses_share_one_projection_even_without_admission() {
        for budget in [1, 4096] {
            let cache = DecodedCache::new(budget);
            let ready = Barrier::new(2);
            let projected = AtomicUsize::new(0);
            let (first, second) = std::thread::scope(|scope| {
                let access = || {
                    let lookup = cache.lookup("p");
                    // Both callers encounter the miss before either projects it.
                    ready.wait();
                    cache
                        .read(lookup, || {
                            projected.fetch_add(1, Relaxed);
                            Some(Arc::new(String::from("projected")))
                        })
                        .unwrap()
                };
                let first = scope.spawn(access);
                let second = scope.spawn(access);
                (first.join().unwrap(), second.join().unwrap())
            });
            assert_eq!(projected.load(Relaxed), 1);
            assert!(Arc::ptr_eq(&first, &second));
            assert_eq!(cache.peek("p").is_some(), budget > 1);
            assert!(cache.state.lock().unwrap().flights.is_empty());
        }
    }

    #[test]
    fn churn_stops_only_at_the_floor_and_above_the_distinct_count() {
        let mut cache = DecodedCache::new(128);
        cache.begin_pass(true);
        let project = |path| {
            cache
                .get_or_project(path, || Some(Arc::new("a".repeat(64))))
                .unwrap()
        };
        let _ = (project("a"), project("b"));
        for n in 0..CHURN_FLOOR - 1 {
            let _ = project(if n % 2 == 0 { "a" } else { "b" });
            assert!(cache.churn().is_none(), "stopped below the floor");
        }
        let _ = project("b");
        let limit = cache
            .churn()
            .expect("capacity re-decodes crossed the floor");
        assert_eq!(
            (limit.redecodes, limit.distinct, limit.budget),
            (CHURN_FLOOR, 2, 128)
        );
        cache.end_pass();
        cache.begin_pass(true);
        assert!(cache.churn().is_none());

        let mut pass = Pass::default();
        for path in 0..CHURN_FLOOR {
            assert!(pass
                .computed(Arc::from(path.to_string()), true, 128)
                .is_none());
        }
        for _ in 0..CHURN_FLOOR {
            assert!(pass.computed(Arc::from("0"), true, 128).is_none());
        }
        let limit = pass.computed(Arc::from("0"), true, 128).unwrap();
        assert_eq!(
            (limit.redecodes, limit.distinct),
            (CHURN_FLOOR + 1, CHURN_FLOOR)
        );
    }
}
