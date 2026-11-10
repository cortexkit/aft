//! A whole-project index as materializing a file reads it.
//!
//! A rollup that works out every file builds each index in memory from every
//! file's facts. A rollup patched from a kept dead-code graph cannot afford
//! that: it holds in memory only the entries that differ from the kept
//! graph's file, and reads any other entry from that file the first time a
//! file asks for it. Both answer the same keys with the same values, so
//! materializing is the same code either way.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use super::reads::Tracked;

/// Reads the entries kept in one slot: usually one, more when keys share a
/// slot.
pub(crate) type LoadSlot<V> = dyn Fn(usize) -> Result<Vec<(String, V)>, String> + Send + Sync;

/// The slot a key's kept entry would be in, `None` when no slot can hold it.
pub(crate) type SlotOf = dyn Fn(&str) -> Option<usize> + Send + Sync;

/// Entries read from a kept dead-code graph's file when first asked for.
pub(crate) struct KeptEntries<V> {
    slot_of: Box<SlotOf>,
    slots: Vec<OnceLock<Vec<(String, V)>>>,
    load: Box<LoadSlot<V>>,
    /// The first slot that could not be read: the answers are then
    /// incomplete, and the rollup must not be used.
    failure: parking_lot::Mutex<Option<String>>,
}

impl<V> KeptEntries<V> {
    pub(crate) fn new(slots: usize, slot_of: Box<SlotOf>, load: Box<LoadSlot<V>>) -> Self {
        Self {
            slot_of,
            slots: (0..slots).map(|_| OnceLock::new()).collect(),
            load,
            failure: parking_lot::Mutex::new(None),
        }
    }

    fn slot(&self, slot: usize) -> &[(String, V)] {
        self.slots.get(slot).map_or(&[], |cell| {
            cell.get_or_init(|| match (self.load)(slot) {
                Ok(entries) => entries,
                Err(error) => {
                    self.failure.lock().get_or_insert(error);
                    Vec::new()
                }
            })
        })
    }

    fn get(&self, key: &str) -> Option<&V> {
        let slot = (self.slot_of)(key)?;
        self.slot(slot)
            .iter()
            .find(|(kept, _)| kept == key)
            .map(|(_, value)| value)
    }
}

/// One whole-project index.
pub(crate) struct Index<V> {
    /// Every entry; for a patched index, the entries that differ from the
    /// kept file's.
    memory: BTreeMap<String, V>,
    /// For a patched index: the keys whose kept entries no longer hold, and
    /// every kept entry.
    kept: Option<(BTreeSet<String>, KeptEntries<V>)>,
}

impl<V> Default for Index<V> {
    fn default() -> Self {
        Self::memory(BTreeMap::new())
    }
}

impl<V: std::fmt::Debug> std::fmt::Debug for Index<V> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Index")
            .field("memory", &self.memory)
            .field("patched", &self.kept.is_some())
            .finish()
    }
}

impl<V> Index<V> {
    /// An index holding every entry.
    pub(crate) fn memory(entries: BTreeMap<String, V>) -> Self {
        Self {
            memory: entries,
            kept: None,
        }
    }

    /// A patched index: `changed` entries over the `kept` ones, with the
    /// kept entries of `removed` keys left out.
    pub(crate) fn patched(
        changed: BTreeMap<String, V>,
        removed: BTreeSet<String>,
        kept: KeptEntries<V>,
    ) -> Self {
        Self {
            memory: changed,
            kept: Some((removed, kept)),
        }
    }

    /// The value at `key`, without recording the read.
    pub(crate) fn peek(&self, key: &str) -> Option<&V> {
        if let Some(value) = self.memory.get(key) {
            return Some(value);
        }
        let (removed, kept) = self.kept.as_ref()?;
        if removed.contains(key) {
            return None;
        }
        kept.get(key)
    }

    /// The entries held in memory, and the keys whose kept entries no longer
    /// hold: what a patched index differs from the kept file by.
    pub(crate) fn changes(&self) -> (&BTreeMap<String, V>, Option<&BTreeSet<String>>) {
        (&self.memory, self.kept.as_ref().map(|(removed, _)| removed))
    }

    /// The first kept entry that could not be read, if any.
    pub(crate) fn failure(&self) -> Option<String> {
        self.kept
            .as_ref()
            .and_then(|(_, kept)| kept.failure.lock().clone())
    }

    /// Every entry, reading each kept one: for checking a patched index
    /// against one built from every file, not for materializing.
    pub(crate) fn entries(&self) -> BTreeMap<String, &V> {
        let mut entries = self
            .memory
            .iter()
            .map(|(key, value)| (key.clone(), value))
            .collect::<BTreeMap<_, _>>();
        if let Some((removed, kept)) = &self.kept {
            for slot in 0..kept.slots.len() {
                for (key, value) in kept.slot(slot) {
                    if !removed.contains(key) && !entries.contains_key(key) {
                        entries.insert(key.clone(), value);
                    }
                }
            }
        }
        entries
    }
}

impl<V> Tracked<Index<V>> {
    pub(crate) fn get(&self, key: &str) -> Option<&V> {
        self.record_key(key);
        self.all().peek(key)
    }

    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
}

impl Tracked<Index<BTreeSet<String>>> {
    /// Whether `pair`'s second string is in the set its first string keys,
    /// recorded as a read of the pair: for an index of (file, symbol) pairs
    /// held as each file's symbols.
    pub(crate) fn contains_pair(&self, pair: &(String, String)) -> bool {
        self.record_key(&super::reads::pair_key(&pair.0, &pair.1));
        self.all()
            .peek(&pair.0)
            .is_some_and(|symbols| symbols.contains(&pair.1))
    }
}
