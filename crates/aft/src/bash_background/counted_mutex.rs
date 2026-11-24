//! A mutex that publishes the size of the collection it guards each time a
//! guard is released, so the watchdog can see that a registry has nothing to
//! poll without taking the lock. Every change goes through a guard, so the
//! published size never drifts from the collection.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LockResult, Mutex, MutexGuard, PoisonError, TryLockError, TryLockResult};

/// A collection whose size [`CountedMutex`] publishes.
pub(crate) trait Counted {
    fn count(&self) -> usize;
}

impl<K, V, S> Counted for std::collections::HashMap<K, V, S> {
    fn count(&self) -> usize {
        self.len()
    }
}

impl Counted for super::watches::WatchRegistry {
    fn count(&self) -> usize {
        self.watched_task_count()
    }
}

pub(crate) struct CountedMutex<T: Counted> {
    inner: Mutex<T>,
    count: AtomicUsize,
}

impl<T: Counted> CountedMutex<T> {
    pub(crate) fn new(value: T) -> Self {
        let count = AtomicUsize::new(value.count());
        Self {
            inner: Mutex::new(value),
            count,
        }
    }

    /// The collection's size when the last guard was released.
    pub(crate) fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    pub(crate) fn lock(&self) -> LockResult<CountedGuard<'_, T>> {
        match self.inner.lock() {
            Ok(guard) => Ok(self.wrap(guard)),
            Err(poison) => Err(PoisonError::new(self.wrap(poison.into_inner()))),
        }
    }

    pub(crate) fn try_lock(&self) -> TryLockResult<CountedGuard<'_, T>> {
        match self.inner.try_lock() {
            Ok(guard) => Ok(self.wrap(guard)),
            Err(TryLockError::Poisoned(poison)) => Err(TryLockError::Poisoned(PoisonError::new(
                self.wrap(poison.into_inner()),
            ))),
            Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
        }
    }

    fn wrap<'a>(&'a self, guard: MutexGuard<'a, T>) -> CountedGuard<'a, T> {
        CountedGuard {
            guard,
            count: &self.count,
        }
    }
}

pub(crate) struct CountedGuard<'a, T: Counted> {
    guard: MutexGuard<'a, T>,
    count: &'a AtomicUsize,
}

impl<T: Counted> Deref for CountedGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: Counted> DerefMut for CountedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T: Counted> Drop for CountedGuard<'_, T> {
    fn drop(&mut self) {
        // Still under the lock: the next guard sees this size or a newer one.
        self.count.store(self.guard.count(), Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn released_guards_publish_the_current_size() {
        let map = CountedMutex::new(HashMap::from([(1, 1)]));
        assert_eq!(map.count(), 1);
        {
            let mut guard = map.lock().unwrap();
            guard.insert(2, 2);
            guard.insert(3, 3);
        }
        assert_eq!(map.count(), 3);
        map.try_lock().ok().unwrap().clear();
        assert_eq!(map.count(), 0);
        let held = map.lock().unwrap();
        assert!(matches!(map.try_lock(), Err(TryLockError::WouldBlock)));
        drop(held);
        // A panic while a guard is held still publishes the size it left.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = map.lock().unwrap();
            guard.insert(4, 4);
            panic!("poison");
        }));
        assert_eq!(map.count(), 1);
        let recovered = map.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(recovered.len(), 1);
    }
}
