//! The heap bytes of in-memory structures, for AFT's memory census.
//!
//! A figure is measured by walking the live structure and adding up what
//! each part owns on the heap, read from the containers' own lengths and
//! capacities:
//!
//! - `String`, `Vec` and `PathBuf`: capacity times the element size.
//! - `Box`, and the shared allocation of an `Arc` or `Rc`: the size of the
//!   value, plus an `Arc`'s or `Rc`'s two reference counts.
//! - `HashMap` and `HashSet`: the table the standard library allocates for
//!   the map's capacity: one slot and one control byte per bucket, and one
//!   group of control bytes more.
//! - `BTreeMap` and `BTreeSet`: one node of eleven slots for every eleven
//!   entries, each slot the size of an entry. The standard library does not
//!   say how full its nodes are; a node can be as little as half full, and
//!   an internal node also holds its edges, so a B-tree's figure is a lower
//!   bound.
//!
//! An allocation reached through more than one `Arc` or `Rc` is counted
//! once per walk ([`Seen`]). The allocator's own bookkeeping and size-class
//! rounding are not counted, and nothing here waits for a lock: a part held
//! by another thread is left out and counted in [`Seen::contended`].

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::mem::{size_of, size_of_val};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock, TryLockError};

/// What a value owns on the heap, beyond its own inline size.
pub trait HeapBytes {
    /// The bytes this value owns on the heap; an allocation `seen` has
    /// already counted adds nothing.
    fn heap_bytes(&self, seen: &mut Seen) -> usize;
}

/// One walk's memory of the shared allocations it has counted, and of the
/// parts it could not read without waiting.
#[derive(Debug, Default)]
pub struct Seen {
    shared: HashSet<usize>,
    contended: usize,
}

impl Seen {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the shared allocation at `address` is met for the first time
    /// in this walk; it is counted only then.
    pub fn first<T: ?Sized>(&mut self, address: *const T) -> bool {
        self.shared.insert(address.cast::<()>() as usize)
    }

    /// Marks the shared allocation at `address` as counted elsewhere, so
    /// this walk leaves it out.
    pub fn counted_elsewhere<T: ?Sized>(&mut self, address: *const T) {
        self.shared.insert(address.cast::<()>() as usize);
    }

    /// Records that a part was held by another thread and left out.
    pub fn contend(&mut self) {
        self.contended += 1;
    }

    /// How many parts this walk left out because another thread held them.
    #[must_use]
    pub fn contended(&self) -> usize {
        self.contended
    }
}

/// The heap bytes of `value` in a walk of its own.
pub fn heap_bytes_of<T: HeapBytes + ?Sized>(value: &T) -> usize {
    value.heap_bytes(&mut Seen::new())
}

/// Implements [`HeapBytes`] for a struct as the sum of its fields. Every
/// field must be named: the struct is destructured without `..`, so a field
/// added later fails to compile until it is listed here.
#[macro_export]
macro_rules! heap_bytes_of_fields {
    ($ty:ty { $($field:ident),* $(,)? }) => {
        impl $crate::heap::HeapBytes for $ty {
            fn heap_bytes(&self, seen: &mut $crate::heap::Seen) -> usize {
                let Self { $($field),* } = self;
                let _ = &seen;
                0 $(+ $crate::heap::HeapBytes::heap_bytes($field, seen))*
            }
        }
    };
}

/// Implements [`HeapBytes`] as zero for types that own nothing on the heap.
#[macro_export]
macro_rules! heap_bytes_none {
    ($($ty:ty),* $(,)?) => {
        $(
            impl $crate::heap::HeapBytes for $ty {
                fn heap_bytes(&self, _seen: &mut $crate::heap::Seen) -> usize {
                    0
                }
            }
        )*
    };
}

heap_bytes_none!(
    (),
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64,
    str,
    &'static str,
    std::time::SystemTime,
    std::time::Instant,
    std::time::Duration,
    std::ops::Range<usize>,
    std::sync::atomic::AtomicBool,
    std::sync::atomic::AtomicU32,
    std::sync::atomic::AtomicU64,
    std::sync::atomic::AtomicUsize,
    crate::ids::CheckoutId,
    crate::ids::ContentKey,
    crate::ids::Digest128,
    crate::ids::FamilyId,
    crate::ids::Generation,
    crate::ids::SegmentId,
    crate::ids::WriterIncarnation,
);

impl HeapBytes for String {
    fn heap_bytes(&self, _seen: &mut Seen) -> usize {
        self.capacity()
    }
}

impl HeapBytes for PathBuf {
    fn heap_bytes(&self, _seen: &mut Seen) -> usize {
        self.capacity()
    }
}

impl HeapBytes for Cow<'static, str> {
    fn heap_bytes(&self, _seen: &mut Seen) -> usize {
        match self {
            Cow::Borrowed(_) => 0,
            Cow::Owned(owned) => owned.capacity(),
        }
    }
}

impl<T: HeapBytes> HeapBytes for [T] {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        self.iter().map(|item| item.heap_bytes(seen)).sum()
    }
}

impl<T: HeapBytes, const N: usize> HeapBytes for [T; N] {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        self.as_slice().heap_bytes(seen)
    }
}

impl<T: HeapBytes> HeapBytes for Vec<T> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        self.capacity() * size_of::<T>() + self.as_slice().heap_bytes(seen)
    }
}

impl<T: HeapBytes + ?Sized> HeapBytes for Box<T> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        size_of_val::<T>(self) + (**self).heap_bytes(seen)
    }
}

/// The bytes of an `Arc`'s or `Rc`'s strong and weak counts, stored in its
/// shared allocation beside its value.
pub const COUNTS: usize = 2 * size_of::<usize>();

impl<T: HeapBytes + ?Sized> HeapBytes for Arc<T> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        if seen.first(Arc::as_ptr(self)) {
            COUNTS + size_of_val::<T>(self) + (**self).heap_bytes(seen)
        } else {
            0
        }
    }
}

impl<T: HeapBytes + ?Sized> HeapBytes for std::rc::Rc<T> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        if seen.first(std::rc::Rc::as_ptr(self)) {
            COUNTS + size_of_val::<T>(self) + (**self).heap_bytes(seen)
        } else {
            0
        }
    }
}

impl<T: HeapBytes> HeapBytes for Option<T> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        self.as_ref().map_or(0, |value| value.heap_bytes(seen))
    }
}

impl<A: HeapBytes, B: HeapBytes> HeapBytes for (A, B) {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        self.0.heap_bytes(seen) + self.1.heap_bytes(seen)
    }
}

impl<A: HeapBytes, B: HeapBytes, C: HeapBytes> HeapBytes for (A, B, C) {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        self.0.heap_bytes(seen) + self.1.heap_bytes(seen) + self.2.heap_bytes(seen)
    }
}

impl<T: HeapBytes> HeapBytes for OnceLock<T> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        self.get().map_or(0, |value| value.heap_bytes(seen))
    }
}

impl<T: HeapBytes> HeapBytes for Mutex<T> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        match self.try_lock() {
            Ok(value) => value.heap_bytes(seen),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().heap_bytes(seen),
            Err(TryLockError::WouldBlock) => {
                seen.contend();
                0
            }
        }
    }
}

impl<T: HeapBytes> HeapBytes for RwLock<T> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        match self.try_read() {
            Ok(value) => value.heap_bytes(seen),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().heap_bytes(seen),
            Err(TryLockError::WouldBlock) => {
                seen.contend();
                0
            }
        }
    }
}

/// The bytes of the table a hash map or set with `capacity` allocates for
/// entries of `entry` bytes: the standard library keeps at most seven of
/// every eight buckets full (a small table one bucket fewer than it has),
/// and the bucket count is a power of two.
fn hash_table_bytes(capacity: usize, entry: usize) -> usize {
    /// Control bytes stored past the last bucket, so a probe can read a
    /// whole group at any bucket.
    const GROUP: usize = if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
        16
    } else {
        8
    };
    if capacity == 0 {
        return 0;
    }
    let buckets = if capacity < 8 {
        (capacity + 1).next_power_of_two()
    } else {
        (capacity * 8 / 7).next_power_of_two()
    };
    buckets * entry + buckets + GROUP
}

/// The bytes of the nodes a B-tree of `len` entries of `entry` bytes needs
/// at the least: one node of eleven slots, with its parent link and
/// counts, for every eleven entries. For a caller that sizes a map's
/// entries itself; a map's own [`HeapBytes`] adds this for it.
#[must_use]
pub fn btree_bytes(len: usize, entry: usize) -> usize {
    const SLOTS: usize = 11;
    const NODE_HEADER: usize = size_of::<usize>() + 2 * size_of::<u16>();
    len.div_ceil(SLOTS) * (NODE_HEADER + SLOTS * entry)
}

impl<K: HeapBytes, V: HeapBytes, S> HeapBytes for HashMap<K, V, S> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        hash_table_bytes(self.capacity(), size_of::<(K, V)>())
            + self
                .iter()
                .map(|(key, value)| key.heap_bytes(seen) + value.heap_bytes(seen))
                .sum::<usize>()
    }
}

impl<K: HeapBytes, S> HeapBytes for HashSet<K, S> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        hash_table_bytes(self.capacity(), size_of::<K>())
            + self.iter().map(|key| key.heap_bytes(seen)).sum::<usize>()
    }
}

impl<K: HeapBytes, V: HeapBytes> HeapBytes for BTreeMap<K, V> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        btree_bytes(self.len(), size_of::<K>() + size_of::<V>())
            + self
                .iter()
                .map(|(key, value)| key.heap_bytes(seen) + value.heap_bytes(seen))
                .sum::<usize>()
    }
}

impl<K: HeapBytes> HeapBytes for BTreeSet<K> {
    fn heap_bytes(&self, seen: &mut Seen) -> usize {
        btree_bytes(self.len(), size_of::<K>())
            + self.iter().map(|key| key.heap_bytes(seen)).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_count_their_capacity_and_their_elements() {
        let mut text = String::with_capacity(40);
        text.push_str("abc");
        assert_eq!(heap_bytes_of(&text), 40);

        let strings = vec![String::from("abcd"), String::with_capacity(10)];
        assert_eq!(
            heap_bytes_of(&strings),
            strings.capacity() * size_of::<String>() + 4 + 10
        );

        let boxed: Box<[u64]> = vec![1, 2, 3].into_boxed_slice();
        assert_eq!(heap_bytes_of(&boxed), 24);
        assert_eq!(heap_bytes_of(&Some(String::from("xy"))), 2);
        assert_eq!(heap_bytes_of(&None::<String>), 0);
    }

    #[test]
    fn a_shared_allocation_is_counted_once_per_walk() {
        let shared = Arc::new(String::from("shared text"));
        let holders = vec![Arc::clone(&shared), Arc::clone(&shared)];
        let one = COUNTS + size_of::<String>() + shared.capacity();
        assert_eq!(
            heap_bytes_of(&holders),
            holders.capacity() * size_of::<Arc<String>>() + one
        );
        let slice: Arc<[u32]> = Arc::from(vec![1, 2, 3, 4]);
        assert_eq!(heap_bytes_of(&slice), COUNTS + 16);

        let mut seen = Seen::new();
        seen.counted_elsewhere(Arc::as_ptr(&shared));
        assert_eq!(shared.heap_bytes(&mut seen), 0, "counted elsewhere");
    }

    #[test]
    fn hash_tables_count_their_buckets_and_control_bytes() {
        let group = if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
            16
        } else {
            8
        };
        let empty: HashMap<u64, u64> = HashMap::new();
        assert_eq!(heap_bytes_of(&empty), 0);
        let mut small: HashMap<u64, u64> = HashMap::new();
        small.insert(1, 2);
        // A first insert allocates four buckets, of which three can be full.
        assert_eq!(small.capacity(), 3);
        assert_eq!(heap_bytes_of(&small), 4 * 16 + 4 + group);
        let large: HashSet<u32> = (0..100).collect();
        let buckets = (large.capacity() * 8 / 7).next_power_of_two();
        assert_eq!(buckets, 128);
        assert_eq!(heap_bytes_of(&large), buckets * 4 + buckets + group);
    }

    #[test]
    fn btrees_count_full_nodes_and_their_entries() {
        let empty: BTreeSet<u64> = BTreeSet::new();
        assert_eq!(heap_bytes_of(&empty), 0);
        let set: BTreeSet<u64> = (0..12).collect();
        let node = size_of::<usize>() + 4 + 11 * 8;
        assert_eq!(heap_bytes_of(&set), 2 * node);
        let map: BTreeMap<u32, String> = [(1, String::from("abc"))].into();
        assert_eq!(
            heap_bytes_of(&map),
            size_of::<usize>() + 4 + 11 * (4 + size_of::<String>()) + 3
        );
    }

    #[test]
    fn a_held_lock_is_left_out_and_counted() {
        let locked = Mutex::new(String::from("held"));
        let _guard = locked.lock();
        let mut seen = Seen::new();
        assert_eq!(locked.heap_bytes(&mut seen), 0);
        assert_eq!(seen.contended(), 1);
    }
}
