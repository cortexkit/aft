//! Generated-file predicates shared by search and inspect, with per-file probe
//! accounting for tests. The classification rules live in [`crate::file_kind`].

#[cfg(any(test, feature = "test-support"))]
use std::collections::BTreeMap;
use std::path::Path;
#[cfg(any(test, feature = "test-support"))]
use std::path::PathBuf;
#[cfg(any(test, feature = "test-support"))]
use std::sync::{Mutex, OnceLock};

#[cfg(any(test, feature = "test-support"))]
static FILE_PROBES: OnceLock<Mutex<BTreeMap<PathBuf, usize>>> = OnceLock::new();

pub(crate) fn is_generated_file(project_root: &Path, path: &Path) -> bool {
    if relative_shape_is_generated(project_root, path) {
        return true;
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };
    #[cfg(any(test, feature = "test-support"))]
    bump_file_probe_count(&absolute);
    // Reads at most the first 8 KiB, so a minified bundle whose first line is
    // the whole file is not read in full just to look for a header.
    crate::file_kind::file_head_has_generated_marker(&absolute)
}

pub(crate) fn is_generated_file_from_source(
    project_root: &Path,
    path: &Path,
    source: &str,
) -> bool {
    relative_shape_is_generated(project_root, path) || source_has_generated_marker(source)
}

/// Only the part of `path` below `project_root` decides a generated shape;
/// folders above the project (a checkout under `~/gen/`) must not.
pub(crate) fn relative_shape_is_generated(project_root: &Path, path: &Path) -> bool {
    crate::file_kind::relative_shape_is_generated(project_root, path)
}

pub(crate) fn is_generated_file_with_cached_hint(
    project_root: &Path,
    relative_file: &str,
    cached_hint: Option<bool>,
) -> bool {
    cached_hint.unwrap_or_else(|| is_generated_file(project_root, Path::new(relative_file)))
}

pub(crate) fn path_has_generated_shape(path: &Path) -> bool {
    crate::file_kind::path_has_generated_shape(path)
}

#[cfg(any(test, feature = "test-support"))]
fn debug_file_probes() -> &'static Mutex<BTreeMap<PathBuf, usize>> {
    FILE_PROBES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[cfg(any(test, feature = "test-support"))]
fn bump_file_probe_count(path: &Path) {
    if let Ok(mut probes) = debug_file_probes().lock() {
        *probes.entry(path.to_path_buf()).or_default() += 1;
    }
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn reset_file_probe_count_for_debug(project_root: &Path) {
    let project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    if let Ok(mut probes) = debug_file_probes().lock() {
        probes.retain(|path, _| !path.starts_with(&project_root));
    }
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn file_probe_count_for_debug(project_root: &Path) -> usize {
    let project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    debug_file_probes()
        .lock()
        .map(|probes| {
            probes
                .iter()
                .filter(|(path, _)| path.starts_with(&project_root))
                .map(|(_, count)| *count)
                .sum()
        })
        .unwrap_or_default()
}

/// A generated-code header in the first five lines of the first 8 KiB.
fn source_has_generated_marker(source: &str) -> bool {
    crate::file_kind::head_has_generated_marker(source.as_bytes())
}
