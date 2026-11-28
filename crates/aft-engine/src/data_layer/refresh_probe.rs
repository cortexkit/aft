//! Stage boundaries for the allocation probe. Absent from production builds.

use std::sync::OnceLock;

static OBSERVER: OnceLock<fn(&'static str)> = OnceLock::new();

/// Installs the process-wide measurement callback before a probe starts.
pub fn set_observer(observer: fn(&'static str)) {
    let _ = OBSERVER.set(observer);
}

pub(crate) fn stage(name: &'static str) {
    if let Some(observer) = OBSERVER.get() {
        observer(name);
    }
}
