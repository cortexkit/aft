use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static PEAKS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

pub(crate) fn note(root: &Path, rows: usize) {
    if let Ok(mut peaks) = PEAKS.get_or_init(Default::default).lock() {
        let peak = peaks.entry(root.to_path_buf()).or_default();
        *peak = (*peak).max(rows);
    }
}

/// The most rows held at once since the last call, and resets it.
pub(crate) fn take(root: &Path) -> usize {
    PEAKS
        .get_or_init(Default::default)
        .lock()
        .ok()
        .and_then(|mut peaks| peaks.remove(root))
        .unwrap_or(0)
}
