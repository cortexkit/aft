//! Checkout paths supplied by the test runner, including relocated nextest archives.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub(crate) fn manifest_dir() -> &'static Path {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            std::env::var_os("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .expect("CARGO_MANIFEST_DIR must be set at runtime by Cargo or nextest")
        })
        .as_path()
}
