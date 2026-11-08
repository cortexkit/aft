//! Symbol tables taken from content records, shared by every checkout in
//! the process.
//!
//! Records are keyed by content, so the worktrees of a repository whose
//! files hold the same bytes read the same records. The first checkout to
//! decode a record's outline lists the table here; a later checkout whose
//! file holds those bytes takes the same table instead of reading and
//! decoding the record again, and holds it rather than a copy.
//!
//! Every table a symbol cache took from here stays listed here, weakly, for
//! as long as any cache holds it, so its memory is counted once, here, and
//! not by each cache (see [`SymbolCache::insert_shared_table`]). Beyond
//! those, the most recently used tables are held here too, up to
//! [`KEPT_BYTES`], so a checkout bound again after its cache was cleared,
//! or a worktree bound after its siblings' caches were, still finds them.
//!
//! [`SymbolCache::insert_shared_table`]: crate::parser::SymbolCache::insert_shared_table

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use aft_store::content::RecordKey;

use crate::symbols::Symbol;

/// The most bytes of tables held here for later binds, on top of those
/// some symbol cache holds: about three repositories' tables (AFT's own
/// come to about 16 MiB).
const KEPT_BYTES: u64 = 64 << 20;

/// A shared table, with the content hash a symbol cache records beside it.
pub(super) struct Table {
    pub(super) symbols: Arc<Vec<Symbol>>,
    /// The symbol cache's content hash of the bytes the table describes.
    pub(super) content_hash: blake3::Hash,
}

struct Slot {
    table: Weak<Vec<Symbol>>,
    content_hash: blake3::Hash,
    /// The table's estimated size, as a symbol cache estimates it.
    bytes: u64,
    /// The tick of its last use; while the table is held for later binds,
    /// its key in [`State::kept`].
    used: u64,
}

#[derive(Default)]
struct State {
    slots: HashMap<RecordKey, Slot>,
    /// The tables held for later binds, oldest use first.
    kept: BTreeMap<u64, (RecordKey, Arc<Vec<Symbol>>, u64)>,
    kept_bytes: u64,
    tick: u64,
    /// How many slots there were after the last sweep of those whose table
    /// is gone.
    swept_at: usize,
    hits: u64,
    misses: u64,
}

impl State {
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Holds `symbols` for later binds as used at `now`, moving it from its
    /// place at `previous` when it was held already, and lets go of the
    /// least recently used tables past the budget.
    fn keep(
        &mut self,
        previous: Option<u64>,
        now: u64,
        key: RecordKey,
        symbols: &Arc<Vec<Symbol>>,
        bytes: u64,
        budget: u64,
    ) {
        match previous.and_then(|previous| self.kept.remove(&previous)) {
            Some(held) => {
                self.kept.insert(now, held);
            }
            None => {
                self.kept.insert(now, (key, Arc::clone(symbols), bytes));
                self.kept_bytes = self.kept_bytes.saturating_add(bytes);
            }
        }
        while self.kept_bytes > budget {
            let Some((_, (_, _, bytes))) = self.kept.pop_first() else {
                break;
            };
            self.kept_bytes = self.kept_bytes.saturating_sub(bytes);
        }
    }

    /// Forgets the slots whose table nothing holds any more, once there are
    /// twice as many slots as after the last sweep.
    fn sweep_when_due(&mut self) {
        if self.slots.len() < self.swept_at.max(1024).saturating_mul(2) {
            return;
        }
        self.slots.retain(|_, slot| slot.table.strong_count() > 0);
        self.swept_at = self.slots.len();
    }
}

/// The process's shared symbol tables.
pub(crate) struct Tables {
    state: Mutex<State>,
    budget: u64,
}

/// What the shared tables hold, for the memory census.
pub(crate) struct TableStats {
    /// Tables some symbol cache, or this one for later binds, holds.
    pub(crate) tables: u64,
    /// Their estimated bytes, with the bookkeeping that lists them.
    pub(crate) bytes: u64,
    /// The bytes of tables held for later binds.
    pub(crate) kept_bytes: u64,
    pub(crate) budget_bytes: u64,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
}

/// The process's shared symbol tables.
pub(crate) fn shared() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| Tables::new(KEPT_BYTES))
}

impl Tables {
    fn new(budget: u64) -> Self {
        Self {
            state: Mutex::new(State::default()),
            budget,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The table of the record under `key`, when some checkout of the
    /// process took it from that record and it is still held.
    pub(super) fn get(&self, key: RecordKey) -> Option<Table> {
        #[cfg(test)]
        if sharing_off() {
            return None;
        }
        let mut state = self.lock();
        let now = state.next_tick();
        let found = state.slots.get_mut(&key).and_then(|slot| {
            let symbols = slot.table.upgrade()?;
            let previous = std::mem::replace(&mut slot.used, now);
            Some((symbols, slot.content_hash, slot.bytes, previous))
        });
        let Some((symbols, content_hash, bytes, previous)) = found else {
            state.misses += 1;
            return None;
        };
        state.hits += 1;
        state.keep(Some(previous), now, key, &symbols, bytes, self.budget);
        Some(Table {
            symbols,
            content_hash,
        })
    }

    /// Lists `symbols`, the table taken from the record under `key`, for
    /// every checkout of the process. Returns the table a symbol cache
    /// should hold, which is one already listed for `key` when there is
    /// one, and whether it is listed here (it is unless a test turned
    /// sharing off), so its memory is counted here.
    pub(super) fn share(
        &self,
        key: RecordKey,
        symbols: Vec<Symbol>,
        content_hash: blake3::Hash,
    ) -> (Arc<Vec<Symbol>>, bool) {
        #[cfg(test)]
        if sharing_off() {
            return (Arc::new(symbols), false);
        }
        let mut state = self.lock();
        let now = state.next_tick();
        let listed = state.slots.get_mut(&key).and_then(|slot| {
            let symbols = slot.table.upgrade()?;
            let previous = std::mem::replace(&mut slot.used, now);
            Some((symbols, slot.bytes, previous))
        });
        if let Some((symbols, bytes, previous)) = listed {
            state.keep(Some(previous), now, key, &symbols, bytes, self.budget);
            return (symbols, true);
        }
        let symbols = Arc::new(symbols);
        let bytes = crate::parser::symbols_estimated_bytes(&symbols);
        state.slots.insert(
            key,
            Slot {
                table: Arc::downgrade(&symbols),
                content_hash,
                bytes,
                used: now,
            },
        );
        state.keep(None, now, key, &symbols, bytes, self.budget);
        state.sweep_when_due();
        (symbols, true)
    }

    /// What the tables hold now, or `None` while another thread holds them.
    pub(crate) fn stats(&self) -> Option<TableStats> {
        let state = self.state.try_lock().ok()?;
        let (tables, table_bytes) = state
            .slots
            .values()
            .filter(|slot| slot.table.strong_count() > 0)
            .fold((0u64, 0u64), |(tables, bytes), slot| {
                (tables + 1, bytes.saturating_add(slot.bytes))
            });
        let bookkeeping = state.slots.capacity()
            * (std::mem::size_of::<RecordKey>() + std::mem::size_of::<Slot>())
            + state.kept.len()
                * (std::mem::size_of::<u64>()
                    + std::mem::size_of::<(RecordKey, Arc<Vec<Symbol>>, u64)>());
        Some(TableStats {
            tables,
            bytes: table_bytes.saturating_add(bookkeeping as u64),
            kept_bytes: state.kept_bytes,
            budget_bytes: self.budget,
            hits: state.hits,
            misses: state.misses,
        })
    }
}

#[cfg(test)]
thread_local! {
    /// Set by [`without_sharing`] on this thread.
    static SHARING_OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn sharing_off() -> bool {
    SHARING_OFF.with(std::cell::Cell::get)
}

/// Runs `work` on this thread as if no table were shared: nothing is
/// found here and nothing is listed, as before tables were shared.
#[cfg(all(test, unix))]
pub(crate) fn without_sharing<T>(work: impl FnOnce() -> T) -> T {
    SHARING_OFF.with(|off| off.set(true));
    let result = work();
    SHARING_OFF.with(|off| off.set(false));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use aft_store::ids::ContentKey;

    fn key(index: u32) -> RecordKey {
        RecordKey::new(
            ContentKey::of(format!("content {index}").as_bytes()),
            "rust",
            "v1",
        )
    }

    fn table(index: u32) -> Vec<Symbol> {
        vec![Symbol {
            name: format!("item_{index}"),
            kind: crate::symbols::SymbolKind::Function,
            range: crate::symbols::Range {
                start_line: 0,
                start_col: 0,
                end_line: 1,
                end_col: 1,
            },
            signature: None,
            scope_chain: Vec::new(),
            exported: true,
            parent: None,
        }]
    }

    /// A table is found while a cache holds it, or while it is among the
    /// most recently used within the budget, and only then.
    #[test]
    fn tables_live_while_held_or_recently_used() {
        let one = crate::parser::symbols_estimated_bytes(&table(0));
        let tables = Tables::new(2 * one);
        let hash = blake3::hash(b"bytes");
        let (held, listed) = tables.share(key(0), table(0), hash);
        assert!(listed);
        // A second share of the same record gives the listed table.
        let (again, _) = tables.share(key(0), table(0), hash);
        assert!(Arc::ptr_eq(&held, &again));
        drop(again);
        tables.share(key(1), table(1), hash);
        tables.share(key(2), table(2), hash);
        // Past the budget the oldest kept table goes, but a cache holds it.
        let found = tables.get(key(0)).expect("a held table is found");
        assert!(Arc::ptr_eq(&found.symbols, &held));
        assert_eq!(found.content_hash, hash);
        drop((found, held));
        tables.share(key(3), table(3), hash);
        tables.share(key(4), table(4), hash);
        assert!(tables.get(key(0)).is_none(), "nothing holds it any more");
        assert!(tables.get(key(4)).is_some());
        let stats = tables.stats().unwrap();
        assert_eq!(stats.tables, 2);
        assert_eq!(stats.kept_bytes, 2 * one);
    }
}
