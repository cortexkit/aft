use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Cargo and nextest provide the source checkout location when launching tests or examples.
pub fn manifest_dir() -> &'static Path {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            std::env::var_os("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .expect("CARGO_MANIFEST_DIR must be set at runtime by Cargo or nextest")
        })
        .as_path()
}
