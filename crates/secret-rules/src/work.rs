//! A comparison counter for the crate's own complexity tests.
//!
//! Some checks run on attacker-sized input (a JSON body with many members, a
//! response with many key matches), so their cost must grow no faster than
//! `n log n`. Wall-clock limits are a poor proof of that, so the unit tests
//! count comparisons instead: the ordered collections those checks use wrap
//! their keys in [`Counted`], whose `Ord` adds one to a per-thread counter in
//! test builds. Outside tests the counter compiles to nothing.

use std::cmp::Ordering;

#[cfg(test)]
thread_local! {
    static COMPARISONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Record one comparison (test builds only).
#[inline]
pub(crate) fn tick() {
    #[cfg(test)]
    COMPARISONS.with(|count| count.set(count.get() + 1));
}

/// Reset the counter and return what it held.
#[cfg(test)]
pub(crate) fn take() -> u64 {
    COMPARISONS.with(|count| count.replace(0))
}

/// An ordered key whose comparisons are counted in test builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Counted<T>(pub T);

impl<T: Ord> PartialOrd for Counted<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: Ord> Ord for Counted<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        tick();
        self.0.cmp(&other.0)
    }
}
