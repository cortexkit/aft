//! Process-wide Darwin I/O deltas, not per-root counters. Other roots and
//! background work can contribute to every interval. Overlap counts flag
//! concurrent publications but cannot rule out other concurrent I/O.
use std::sync::{Mutex, OnceLock};

pub(crate) use crate::process_io::Bytes;
#[derive(Default)]
struct Activity {
    active: u64,
    starts: u64,
}
fn activity() -> &'static Mutex<Activity> {
    static STATE: OnceLock<Mutex<Activity>> = OnceLock::new();
    STATE.get_or_init(Mutex::default)
}
/// Counts other publication lifetimes intersecting the measured interval.
pub(crate) struct Overlap {
    active: u64,
    starts: u64,
    registered: bool,
    finished: Option<u64>,
}
impl Overlap {
    pub(crate) fn new(register: bool) -> Self {
        let mut state = activity()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active = state.active;
        if register {
            state.active += 1;
            state.starts += 1;
        }
        Self {
            active,
            starts: state.starts,
            registered: register,
            finished: None,
        }
    }
    pub(crate) fn finish(&mut self) -> u64 {
        if let Some(value) = self.finished {
            return value;
        }
        let mut state = activity()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let value = self.active + state.starts - self.starts;
        if self.registered {
            state.active -= 1;
        }
        self.finished = Some(value);
        value
    }
}
impl Drop for Overlap {
    fn drop(&mut self) {
        self.finish();
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Phase {
    Manifest,
    Blobs,
    Clone,
    Materialize,
    Closure,
    DerivedOther,
    Cas,
}
/// Disjoint intervals use the same sample at each boundary. Nested derived
/// subtotals are aliases, not additional bytes to add to the top-level sum.
pub(crate) struct PublicationIo {
    previous: Option<Bytes>,
    buckets: [Bytes; 7],
    available: bool,
    phase: Phase,
    blob_counter: crate::write_ledger::Counter,
    derived_counter: crate::write_ledger::Counter,
    closure_counter: crate::write_ledger::Counter,
}
impl PublicationIo {
    pub(crate) fn new(root: &std::path::Path) -> Self {
        Self::from_sample(Bytes::capture(), root)
    }
    fn from_sample(sample: Option<Bytes>, root: &std::path::Path) -> Self {
        let root = root.display().to_string();
        Self {
            previous: sample,
            buckets: [Bytes::default(); 7],
            available: sample.is_some(),
            phase: Phase::Manifest,
            blob_counter: crate::write_ledger::register(
                crate::write_ledger::Domain::ViewsBlob,
                root.clone(),
            ),
            derived_counter: crate::write_ledger::register(
                crate::write_ledger::Domain::ViewsDerived,
                root.clone(),
            ),
            closure_counter: crate::write_ledger::register(
                crate::write_ledger::Domain::ViewsClosure,
                root,
            ),
        }
    }
    fn advance(&mut self, next: Option<Bytes>) {
        if let Some(delta) = self.previous.zip(next).and_then(|(a, b)| b.delta(a)) {
            self.buckets[self.phase as usize] = self.buckets[self.phase as usize].add(delta);
            let counter = match self.phase {
                Phase::Blobs => Some(&self.blob_counter),
                Phase::Clone | Phase::Materialize | Phase::DerivedOther => {
                    Some(&self.derived_counter)
                }
                Phase::Closure => Some(&self.closure_counter),
                Phase::Manifest | Phase::Cas => None,
            };
            if let Some(counter) = counter {
                counter.credit(delta.logical, delta.written);
            }
        } else {
            self.available = false;
        }
        self.previous = next;
    }
    pub(crate) fn enter(&mut self, phase: Phase) {
        self.advance(Bytes::capture());
        self.phase = phase;
    }
    pub(crate) fn finish(&mut self) {
        self.advance(Bytes::capture());
    }
    pub(crate) fn fields(&self) -> String {
        let derived = self.buckets[2..6]
            .iter()
            .copied()
            .fold(Bytes::default(), Bytes::add);
        let total = self
            .buckets
            .iter()
            .copied()
            .fold(Bytes::default(), Bytes::add);
        let mut fields = format!("io_scope=process io_available={}", self.available);
        for (name, value) in [
            ("manifest", self.buckets[0]),
            ("blobs", self.buckets[1]),
            ("derived", derived),
            ("cas", self.buckets[6]),
            ("derived_clone", self.buckets[2]),
            ("materialize", self.buckets[3]),
            ("closure", self.buckets[4]),
            ("derived_other", self.buckets[5]),
            ("total", total),
        ] {
            fields.push(' ');
            fields.push_str(&Bytes::fields(self.available.then_some(value), name));
        }
        fields
    }
}
pub(crate) struct Window {
    before: Option<Bytes>,
    overlap: Overlap,
}
impl Window {
    pub(crate) fn new() -> Self {
        Self {
            before: Bytes::capture(),
            overlap: Overlap::new(false),
        }
    }
    pub(crate) fn finish(&mut self) -> String {
        let delta = self
            .before
            .zip(Bytes::capture())
            .and_then(|(a, b)| b.delta(a));
        format!(
            "io_scope=process io_available={} concurrent_publications={} {}",
            delta.is_some(),
            self.overlap.finish(),
            Bytes::fields(delta, "total")
        )
    }
    pub(crate) fn event(kind: &'static str, root: &std::path::Path) -> Event {
        Event {
            window: Self::new(),
            kind,
            root: root.to_owned(),
        }
    }
}
pub(crate) struct Event {
    window: Window,
    kind: &'static str,
    root: std::path::PathBuf,
}
impl Drop for Event {
    fn drop(&mut self) {
        crate::slog_info!(
            "index_event kind={} root={} {}",
            self.kind,
            self.root.display(),
            self.window.finish()
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_byte_buckets_sum_to_process_delta() {
        let bytes = |n| Bytes {
            written: n,
            logical: n * 2,
            read: n * 3,
        };
        let root = std::path::Path::new("/ledger-view-fixture");
        let mut p = PublicationIo::from_sample(Some(bytes(100)), root);
        for (phase, n) in [
            (Phase::Manifest, 101),
            (Phase::Blobs, 103),
            (Phase::Clone, 107),
            (Phase::Materialize, 115),
            (Phase::Closure, 131),
            (Phase::DerivedOther, 163),
            (Phase::Cas, 227),
        ] {
            p.phase = phase;
            p.advance(Some(bytes(n)));
        }
        let fields = p.fields();
        for expected in [
            "manifest_physical_bytes_written=1 ",
            "blobs_physical_bytes_written=2 ",
            "derived_physical_bytes_written=60 ",
            "cas_physical_bytes_written=64 ",
            "total_physical_bytes_written=127 ",
            "total_logical_bytes_written=254 ",
            "total_bytes_read=381",
        ] {
            assert!(fields.contains(expected), "{fields}");
        }
        assert_eq!(
            p.buckets.iter().copied().fold(Bytes::default(), Bytes::add),
            bytes(227).delta(bytes(100)).unwrap()
        );
        let root = root.display().to_string();
        let blob =
            crate::write_ledger::pending_for_test(crate::write_ledger::Domain::ViewsBlob, &root);
        let derived =
            crate::write_ledger::pending_for_test(crate::write_ledger::Domain::ViewsDerived, &root);
        let closure =
            crate::write_ledger::pending_for_test(crate::write_ledger::Domain::ViewsClosure, &root);
        assert_eq!(blob, (4, 2));
        assert_eq!(derived, (88, 44));
        assert_eq!(closure, (32, 16));
        assert_eq!(
            (
                blob.0 + derived.0 + closure.0,
                blob.1 + derived.1 + closure.1
            ),
            (124, 62),
            "ledger uses the exact blobs + derived publication byte buckets"
        );
    }
    #[test]
    fn missing_counters_are_unknown_not_zero() {
        let mut p = PublicationIo::from_sample(None, std::path::Path::new("/missing"));
        p.advance(Some(Bytes::default()));
        assert!(p.fields().contains("total_physical_bytes_written=unknown"));
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_logical_bytes_observe_file_write() {
        crate::process_io::assert_darwin_logical_bytes_observe_file_write();
    }
}
