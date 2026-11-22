//! Byte-bounded cache of projections reconstructed from one publication.

use aft_store::heap::{HeapBytes, Seen};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

struct Entry<T> {
    value: Arc<T>,
    size: usize,
    used: u64,
}

struct State<T> {
    entries: HashMap<Arc<str>, Entry<T>>,
    order: BTreeMap<u64, Arc<str>>,
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
        let capacity = self.entries.capacity();
        // Include unfilled buckets and their control bytes in the estimate.
        let buckets = if capacity == 0 {
            0
        } else {
            (capacity + 1).next_power_of_two()
        };
        buckets * (size_of::<(Arc<str>, Entry<T>)>() + 1)
            + aft_store::heap::btree_bytes(
                self.order.len(),
                size_of::<u64>() + size_of::<Arc<str>>(),
            )
            + self.entries.len() * aft_store::heap::COUNTS
    }

    fn touch(&mut self, path: &str) -> Option<Arc<T>> {
        let entry = self.entries.get_mut(path)?;
        let key = self
            .order
            .remove(&entry.used)
            .expect("cache entry is ordered");
        self.clock += 1;
        entry.used = self.clock;
        self.order.insert(entry.used, key);
        Some(Arc::clone(&entry.value))
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
        }
    }

    pub(super) fn get(&self, path: &str) -> Option<Arc<T>> {
        self.state.lock().expect("decoded cache lock").touch(path)
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

    #[cfg(any(test, feature = "test-hooks"))]
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
    pub(super) fn insert(&self, path: &str, value: Arc<T>) -> Arc<T> {
        let size = value.heap_bytes(&mut Seen::new()) + path.len();
        if size > self.budget {
            return value;
        }
        let mut state = self.state.lock().expect("decoded cache lock");
        // Two threads can project the same immutable file on a miss. Admit
        // only one copy, and let an oversized projection bypass the cache.
        if let Some(held) = state.touch(path) {
            return held;
        }
        while state.held > self.budget - size {
            let (_, path) = state.order.pop_first().expect("cache is over budget");
            state.remove(&path);
            state.evicted += 1;
        }
        let path: Arc<str> = Arc::from(path);
        state.clock += 1;
        let used = state.clock;
        state.order.insert(used, Arc::clone(&path));
        state.entries.insert(
            path,
            Entry {
                value: Arc::clone(&value),
                size,
                used,
            },
        );
        state.held += size;
        state.peak = state.peak.max(state.held);
        state.peak_table = state.peak_table.max(state.table_bytes());
        value
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
