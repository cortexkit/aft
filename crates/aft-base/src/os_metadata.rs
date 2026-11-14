//! Operating-system metadata files that project walks and the file watcher skip.
//!
//! Finder, Explorer and friends drop small bookkeeping files into ordinary
//! folders: an "empty" macOS `~/Desktop` still holds a hidden `.localized`, so
//! the search index reported one file there. None of these files ever matter
//! for code search, so every project walk that feeds an index (trigram,
//! semantic, callgraph) or a glob/grep filesystem fallback leaves them out, and
//! the watcher drops their change events so a Finder `.DS_Store` rewrite never
//! triggers a refresh. An explicit read or edit of one of these paths is not
//! affected: only walks and watch events consult this list.
//!
//! Matching is on the exact file name, at any depth.

use std::ffi::OsStr;
use std::path::Path;

/// Exact file names the operating system creates for its own bookkeeping.
///
/// - `.DS_Store`: macOS Finder view settings, written into every browsed folder.
/// - `.localized`: macOS marker that a folder has a localized display name.
/// - `Icon\r`: macOS custom folder icon; the name really ends in a carriage return.
/// - `Thumbs.db`, `ehthumbs.db`: Windows Explorer thumbnail caches.
/// - `desktop.ini`: Windows folder display settings.
///
/// AppleDouble files (`._<name>`, resource forks macOS writes on non-HFS
/// volumes) are matched by prefix in [`is_os_metadata_file_name`].
const OS_METADATA_FILE_NAMES: [&str; 6] = [
    ".DS_Store",
    ".localized",
    "Icon\r",
    "Thumbs.db",
    "ehthumbs.db",
    "desktop.ini",
];

/// File-name prefix of macOS AppleDouble resource-fork companions.
const APPLE_DOUBLE_PREFIX: &str = "._";

/// True when `name` is an operating-system metadata file name.
pub fn is_os_metadata_file_name(name: &OsStr) -> bool {
    // Every listed name is ASCII, so a name that is not valid UTF-8 cannot match.
    let Some(name) = name.to_str() else {
        return false;
    };
    name.starts_with(APPLE_DOUBLE_PREFIX) || OS_METADATA_FILE_NAMES.contains(&name)
}

/// True when the last component of `path` is an operating-system metadata file name.
pub fn is_os_metadata_path(path: &Path) -> bool {
    path.file_name().is_some_and(is_os_metadata_file_name)
}
