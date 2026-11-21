//! Allocations charged for their capacity rather than length.

use std::mem::size_of;
use std::sync::{Arc, Mutex};

use super::{Error, Stats};

#[derive(Clone, Debug)]
pub(super) struct Memory(Arc<Mutex<Stats>>);

impl Memory {
    pub(super) fn new(budget: usize) -> Self {
        Self(Arc::new(Mutex::new(Stats {
            budget_bytes: budget,
            ..Stats::default()
        })))
    }

    pub(super) fn stats(&self) -> Stats {
        *self.0.lock().expect("spill counters poisoned")
    }

    pub(super) fn update(&self, update: impl FnOnce(&mut Stats)) {
        update(&mut self.0.lock().expect("spill counters poisoned"));
    }

    pub(super) fn available(&self) -> usize {
        let s = self.stats();
        s.budget_bytes - s.buffered_bytes
    }

    fn charge(&self, bytes: usize) -> Result<(), Error> {
        let mut s = self.0.lock().expect("spill counters poisoned");
        if bytes > s.budget_bytes - s.buffered_bytes {
            return Err(Error::Budget {
                needed: s.buffered_bytes.saturating_add(bytes),
                budget: s.budget_bytes,
            });
        }
        s.buffered_bytes += bytes;
        s.peak_buffered_bytes = s.peak_buffered_bytes.max(s.buffered_bytes);
        s.allocations += 1;
        Ok(())
    }

    fn release(&self, bytes: usize) {
        self.update(|s| s.buffered_bytes -= bytes);
    }
}

/// All record, index, reader, heap, and I/O allocations go through this wrapper.
/// Only `grow_to` may grow a vector after construction.
#[derive(Debug)]
pub(super) struct Buffer<T> {
    vec: Vec<T>,
    memory: Memory,
    charged: usize,
}

impl<T> Buffer<T> {
    pub(super) fn new(capacity: usize, memory: &Memory) -> Result<Self, Error> {
        let bytes = capacity
            .checked_mul(size_of::<T>())
            .ok_or(Error::Overflow)?;
        memory.charge(bytes)?;
        let mut vec = Vec::new();
        if let Err(error) = vec.try_reserve_exact(capacity) {
            memory.release(bytes);
            return Err(Error::Allocation(error));
        }
        let actual = vec.capacity() * size_of::<T>();
        if actual > bytes
            && let Err(error) = memory.charge(actual - bytes)
        {
            memory.release(bytes);
            return Err(error);
        }
        Ok(Self {
            vec,
            memory: memory.clone(),
            charged: actual,
        })
    }

    pub(super) fn grow_to(&mut self, capacity: usize) -> Result<(), Error> {
        debug_assert!(capacity > self.capacity());
        let bytes = capacity
            .checked_mul(size_of::<T>())
            .ok_or(Error::Overflow)?;
        // A realloc may move the buffer. Charge the new capacity while the
        // old capacity is still live, even if the allocator extends in place.
        self.memory.charge(bytes)?;
        let previous = self.vec.as_ptr();
        if let Err(error) = self.vec.try_reserve_exact(capacity - self.vec.len()) {
            self.memory.release(bytes);
            return Err(Error::Allocation(error));
        }
        self.memory.release(self.charged);
        let relocated = self.charged != 0 && self.vec.as_ptr() != previous;
        let actual = self.capacity() * size_of::<T>();
        self.charged = bytes;
        if actual > bytes {
            self.memory.charge(actual - bytes)?;
        }
        self.charged = actual;
        if relocated {
            self.memory.update(|s| s.relocations += 1);
        }
        Ok(())
    }

    pub(super) fn push(&mut self, item: T) {
        assert!(self.vec.len() < self.vec.capacity());
        self.vec.push(item);
    }

    pub(super) fn extend(&mut self, bytes: &[T])
    where
        T: Copy,
    {
        assert!(bytes.len() <= self.vec.capacity() - self.vec.len());
        self.vec.extend_from_slice(bytes);
    }

    pub(super) fn capacity(&self) -> usize {
        self.vec.capacity()
    }
    pub(super) fn pop(&mut self) -> Option<T> {
        self.vec.pop()
    }
    pub(super) fn clear(&mut self) {
        self.vec.clear();
    }
    pub(super) fn remove(&mut self, index: usize) -> T {
        self.vec.remove(index)
    }

    pub(super) fn into_untracked(mut self) -> Vec<T> {
        std::mem::take(&mut self.vec)
    }
}

impl<T> std::ops::Deref for Buffer<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.vec
    }
}

impl<T> std::ops::DerefMut for Buffer<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.vec
    }
}

impl<T> Drop for Buffer<T> {
    fn drop(&mut self) {
        self.memory.release(self.charged);
    }
}

impl Buffer<u8> {
    pub(super) fn resize_zeroed(&mut self, len: usize) {
        assert!(len <= self.vec.capacity());
        self.vec.resize(len, 0);
    }
}
