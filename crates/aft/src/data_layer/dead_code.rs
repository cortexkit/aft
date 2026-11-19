//! Shared names and path rules for the dead-code graph projections.

pub(crate) mod stream;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::path_identity::canonicalize_normalized;

/// The kinds of reference an outbound call can come from.
pub(crate) const OUTBOUND_KINDS: [&str; 5] = [
    "call",
    "construction",
    "callback_ref",
    "value_ref",
    "macro_body_call",
];

/// Store paths made absolute and canonical, as the snapshot names files.
pub(crate) struct Paths<'a> {
    pub(crate) root: &'a Path,
    pub(crate) cache: HashMap<String, PathBuf>,
}

impl Paths<'_> {
    pub(crate) fn resolve(&mut self, store_path: &str) -> PathBuf {
        if let Some(path) = self.cache.get(store_path) {
            return path.clone();
        }
        let path = Path::new(store_path);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let path = canonicalize_normalized(&absolute);
        self.cache.insert(store_path.to_string(), path.clone());
        path
    }
}

pub(crate) fn path_key(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}
