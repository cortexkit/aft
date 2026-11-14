//! Fetch-owned response and formatting buffers wipe before every allocation is freed.
use std::ops::{Deref, DerefMut};
use zeroize::{Zeroize, Zeroizing};

#[derive(Clone, Default)]
pub(super) struct Factory {
    #[cfg(any(test, feature = "test-support"))]
    trace: Option<std::sync::Arc<testing::Trace>>,
}
impl Factory {
    pub(super) fn current() -> Self {
        Self {
            #[cfg(any(test, feature = "test-support"))]
            trace: testing::CURRENT.with(|slot| slot.borrow().clone()),
        }
    }
    pub(super) fn with_capacity(&self, capacity: usize) -> Bytes {
        Bytes {
            data: Zeroizing::new(Vec::with_capacity(capacity)),
            factory: self.clone(),
        }
    }
    pub(super) fn copy(&self, bytes: &[u8]) -> Bytes {
        let mut owned = self.with_capacity(bytes.len());
        owned.extend_from_slice(bytes);
        owned
    }
    pub(super) fn zeroed(&self, length: usize) -> Bytes {
        let mut owned = self.with_capacity(length);
        owned.data.resize(length, 0);
        owned
    }
    pub(super) fn format(&self, args: std::fmt::Arguments<'_>) -> Bytes {
        use std::fmt::Write;
        let mut owned = self.with_capacity(0);
        owned
            .write_fmt(args)
            .expect("formatting into owned memory cannot fail");
        owned
    }
    fn wipe(&self, data: &mut Vec<u8>, growth: bool) {
        #[cfg(any(test, feature = "test-support"))]
        let contained_key = self
            .trace
            .as_ref()
            .is_some_and(|trace| trace.contains(data));
        data.as_mut_slice().zeroize();
        #[cfg(any(test, feature = "test-support"))]
        if contained_key {
            self.trace.as_ref().unwrap().record(data, growth);
        }
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = growth;
    }
}

pub(super) struct Bytes {
    data: Zeroizing<Vec<u8>>,
    factory: Factory,
}
impl Bytes {
    pub(super) fn extend_from_slice(&mut self, bytes: &[u8]) {
        let required = self
            .data
            .len()
            .checked_add(bytes.len())
            .expect("buffer length overflow");
        if required > self.data.capacity() {
            // Vec::reserve may free a populated allocation without wiping it.
            // Copy into a new owner, then wipe the old allocation before drop.
            let capacity = required.max(self.data.capacity().saturating_mul(2));
            let mut next = Zeroizing::new(Vec::with_capacity(capacity));
            next.extend_from_slice(&self.data);
            self.factory.wipe(&mut self.data, true);
            self.data = next;
        }
        self.data.extend_from_slice(bytes);
    }
    pub(super) fn into_plain_vec(mut self) -> Vec<u8> {
        // Only the ordinary, non-secret downloader transfers plain ownership.
        std::mem::take(&mut *self.data)
    }
}
impl Drop for Bytes {
    fn drop(&mut self) {
        self.factory.wipe(&mut self.data, false);
    }
}
impl Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}
impl DerefMut for Bytes {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}
impl std::fmt::Write for Bytes {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.extend_from_slice(text.as_bytes());
        Ok(())
    }
}
impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtectedFetchBytes")
            .field("length", &self.len())
            .finish()
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};
    thread_local! { pub static CURRENT: std::cell::RefCell<Option<Arc<Trace>>> = const { std::cell::RefCell::new(None) }; }
    #[derive(Clone, Debug)]
    pub struct Wipe {
        pub growth: bool,
        pub zeroed: bool,
        pub capacity: usize,
    }
    pub struct Trace {
        needle: &'static [u8],
        records: Mutex<Vec<Wipe>>,
        changed: Condvar,
    }
    impl Trace {
        pub fn contains(&self, bytes: &[u8]) -> bool {
            bytes
                .windows(self.needle.len())
                .any(|window| window == self.needle)
        }
        pub fn record(&self, bytes: &Vec<u8>, growth: bool) {
            self.records.lock().unwrap().push(Wipe {
                growth,
                zeroed: bytes.iter().all(|byte| *byte == 0),
                capacity: bytes.capacity(),
            });
            self.changed.notify_all();
        }
        pub fn wait_for(&self, count: usize) -> Vec<Wipe> {
            let end = Instant::now() + Duration::from_secs(2);
            let mut records = self.records.lock().unwrap();
            while records.len() < count && Instant::now() < end {
                records = self
                    .changed
                    .wait_timeout(records, end.saturating_duration_since(Instant::now()))
                    .unwrap()
                    .0;
            }
            records.clone()
        }
    }
    pub struct Restore(Option<Arc<Trace>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|slot| slot.replace(self.0.take()));
        }
    }
    pub fn track(needle: &'static [u8]) -> (Arc<Trace>, Restore) {
        let trace = Arc::new(Trace {
            needle,
            records: Default::default(),
            changed: Default::default(),
        });
        let restore = Restore(CURRENT.with(|slot| slot.replace(Some(trace.clone()))));
        (trace, restore)
    }
}
