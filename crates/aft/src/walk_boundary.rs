//! Filesystem-boundary guards for recursive directory walks.
//!
//! On macOS, `ReadDir` can panic while it is dropped when a mounted filesystem
//! disappears (`closedir(3)` returns ENXIO). A destructor panic cannot unwind
//! safely, so it aborts the whole daemon. Recursive walks must therefore avoid
//! opening a child directory that belongs to another mounted filesystem.

use std::io;
use std::path::{Path, PathBuf};

/// Filesystem identity captured from the root of one recursive walk.
///
/// Unix exposes the device id directly through `MetadataExt::dev`. Windows does
/// not expose a stable volume serial through `std::fs::Metadata`, so raw
/// `read_dir` walkers keep their historical behavior there; `ignore::WalkBuilder`
/// still uses its cross-platform `same_file_system` implementation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeviceBoundary {
    root_device: Option<u64>,
}

impl DeviceBoundary {
    /// Capture the device that recursive descendants must remain on.
    pub(crate) fn for_root(root: &Path) -> io::Result<Self> {
        Ok(Self {
            root_device: filesystem_device_id(root)?,
        })
    }

    /// Return whether `child` can be entered without crossing the root mount.
    pub(crate) fn should_descend(&self, child: &Path) -> io::Result<bool> {
        self.should_descend_with(child, filesystem_device_id)
    }

    /// Lookup-injectable form used by recursive walkers' regression tests.
    pub(crate) fn should_descend_with<F>(&self, child: &Path, device_lookup: F) -> io::Result<bool>
    where
        F: FnOnce(&Path) -> io::Result<Option<u64>>,
    {
        let Some(root_device) = self.root_device else {
            // See the type-level Windows note above. There is no std-only volume
            // serial to compare on Windows, so this intentionally remains a no-op.
            return Ok(true);
        };
        let Some(child_device) = device_lookup(child)? else {
            return Ok(true);
        };
        Ok(same_filesystem_device(root_device, child_device))
    }

    #[cfg(test)]
    pub(crate) fn from_device_for_test(root_device: u64) -> Self {
        Self {
            root_device: Some(root_device),
        }
    }
}

/// Expand a glob by walking only its literal base on the same filesystem.
///
/// The `glob` crate traverses `**` internally and offers no filesystem-boundary
/// option. Routing recursive glob expansion through `ignore` prevents a vanished
/// mounted child from turning `ReadDir::drop`'s ENXIO into a daemon abort.
pub(crate) fn expand_glob_same_file_system(
    full_pattern: &str,
) -> Result<Vec<PathBuf>, glob::PatternError> {
    expand_glob_with_visit(full_pattern, |_| {})
}

// The observer runs on walked entries before matching, so tests can verify
// traversal depth independently of the paths returned by the glob.
fn expand_glob_with_visit(
    full_pattern: &str,
    mut visit: impl FnMut(&Path),
) -> Result<Vec<PathBuf>, glob::PatternError> {
    let normalized = full_pattern.replace('\\', "/");
    let Some(first_glob) = normalized.find(['*', '?', '[', '{']) else {
        return Ok(Vec::new());
    };
    let (base, relative_pattern) = match normalized[..first_glob].rfind('/') {
        Some(0) => (PathBuf::from("/"), &normalized[1..]),
        Some(base_end) => (
            PathBuf::from(&normalized[..base_end]),
            &normalized[base_end + 1..],
        ),
        None => (PathBuf::from("."), normalized.as_str()),
    };
    let relative = glob::Pattern::new(relative_pattern)?;
    // Normalization above makes '/' the separator even for Windows input.
    // A whole-component ** can match descendants at arbitrarily deep levels.
    let components: Vec<_> = relative_pattern.split('/').collect();
    let max_depth = (!components.contains(&"**")).then_some(components.len());
    let options = glob::MatchOptions {
        case_sensitive: !cfg!(windows),
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };

    // Do not cross a mount while expanding an agent-provided glob: a vanished
    // child ReadDir can panic in Drop after closedir reports ENXIO and abort AFT.
    Ok(ignore::WalkBuilder::new(&base)
        .same_file_system(true)
        .max_depth(max_depth)
        .hidden(false)
        .parents(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .build()
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.into_path();
            visit(&path);
            path
        })
        .filter(|path| {
            path.strip_prefix(&base)
                .ok()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .is_some_and(|path| relative.matches_with(&path, options))
        })
        .collect())
}

/// Direct device-boundary predicate. Keeping it independent of filesystem I/O
/// gives tests a portable mutation target without requiring a fragile loopback
/// mount on macOS.
pub(crate) fn same_filesystem_device(root_device: u64, child_device: u64) -> bool {
    root_device == child_device
}

#[cfg(unix)]
pub(crate) fn filesystem_device_id(path: &Path) -> io::Result<Option<u64>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path)?;
    #[cfg(test)]
    if let Some(device) = simulated_volumes::device_of(path) {
        return Ok(Some(device));
    }
    Ok(Some(metadata.dev()))
}

#[cfg(not(unix))]
pub(crate) fn filesystem_device_id(path: &Path) -> io::Result<Option<u64>> {
    // Standard metadata on non-Unix platforms has file attributes but no
    // reliable volume identifier, so this raw-directory-walk check is a no-op.
    // Recursive walks using `ignore::WalkBuilder` still enforce their own
    // cross-filesystem restriction.
    #[cfg(test)]
    {
        std::fs::metadata(path)?;
        if let Some(device) = simulated_volumes::device_of(path) {
            return Ok(Some(device));
        }
    }
    let _ = path;
    Ok(None)
}

/// Test seam that makes chosen folders look like separate volumes.
///
/// CI has no second filesystem to mount, so tests register a folder with a
/// made-up device number. `filesystem_device_id` then reports that number for
/// the folder and everything under it, and `crate::cross_volume::rename`
/// refuses a move between two different simulated devices with the same
/// cross-device error the kernel returns. Registrations are keyed by path, so
/// tests running in parallel in their own temporary folders do not see each
/// other's volumes.
#[cfg(test)]
pub(crate) mod simulated_volumes {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    fn registry() -> &'static Mutex<Vec<(PathBuf, u64)>> {
        static VOLUMES: OnceLock<Mutex<Vec<(PathBuf, u64)>>> = OnceLock::new();
        VOLUMES.get_or_init(Default::default)
    }

    /// Removes its registration when dropped.
    #[must_use]
    pub(crate) struct SimulatedVolume {
        paths: Vec<PathBuf>,
    }

    impl Drop for SimulatedVolume {
        fn drop(&mut self) {
            registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|(path, _)| !self.paths.contains(path));
        }
    }

    /// Report `path` and everything below it as device `device`.
    pub(crate) fn simulate(path: &Path, device: u64) -> SimulatedVolume {
        let mut paths = vec![path.to_path_buf()];
        // Register every resolved spelling too, so a caller that resolved the
        // path before asking for its device still matches: macOS `/var` is
        // `/private/var`; on Windows `fs::canonicalize` returns a verbatim
        // `\\?\C:\...` path with 8.3 short names (GitHub runners keep TEMP
        // under `C:\Users\RUNNER~1`) expanded, and AFT opens configured roots
        // under the plain `C:\Users\runneradmin\...` spelling that
        // `canonical_root_path` returns. Missing that last spelling made the
        // simulated cache volume invisible and enrollment saw the real, shared
        // volume.
        // A folder the test creates only later resolves through its nearest
        // existing ancestor.
        let mut existing = path;
        let mut missing = Vec::new();
        while !existing.exists() {
            match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    missing.push(name.to_os_string());
                    existing = parent;
                }
                _ => break,
            }
        }
        let resolved = [
            std::fs::canonicalize(existing).ok(),
            crate::bash_background::persistence::canonical_root_path(existing).ok(),
        ];
        for mut spelling in resolved.into_iter().flatten() {
            for name in missing.iter().rev() {
                spelling.push(name);
            }
            if !paths.contains(&spelling) {
                paths.push(spelling);
            }
        }
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(paths.iter().map(|path| (path.clone(), device)));
        SimulatedVolume { paths }
    }

    /// The simulated device of the innermost registered folder containing
    /// `path`, or `None` when no registered folder contains it.
    pub(crate) fn device_of(path: &Path) -> Option<u64> {
        let volumes = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if volumes.is_empty() {
            return None;
        }
        volumes
            .iter()
            .filter(|(root, _)| path.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .map(|(_, device)| *device)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::{Path, PathBuf};

    use super::{expand_glob_with_visit, same_filesystem_device, DeviceBoundary};

    fn observed_glob(pattern: &str) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut visited = Vec::new();
        let mut matches = expand_glob_with_visit(pattern, |path| visited.push(path.to_path_buf()))
            .expect("valid glob");
        matches.sort();
        (matches, visited)
    }

    #[test]
    fn simulated_volume_covers_the_plain_resolved_spelling_of_its_folder() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("detour")).unwrap();
        std::fs::create_dir(temp.path().join("cache-volume")).unwrap();
        // A spelling that names the folder only after normalization, as an
        // 8.3 short name such as `C:\Users\RUNNER~1` does on GitHub's
        // Windows runners.
        let spelled = temp.path().join("detour").join("..").join("cache-volume");
        let _volume = super::simulated_volumes::simulate(&spelled, u64::MAX - 90);
        // AFT opens configured roots under this spelling (plain `C:\...` on
        // Windows, not the verbatim `\\?\C:\...` that fs::canonicalize gives).
        let resolved = crate::bash_background::persistence::canonical_root_path(&spelled).unwrap();
        assert_eq!(
            super::simulated_volumes::device_of(&resolved),
            Some(u64::MAX - 90)
        );
        assert_eq!(
            super::simulated_volumes::device_of(&resolved.join("aft")),
            Some(u64::MAX - 90)
        );
        // A folder registered before the test creates it.
        let later = temp.path().join("detour").join("..").join("created-later");
        let _later = super::simulated_volumes::simulate(&later, u64::MAX - 91);
        std::fs::create_dir(&later).unwrap();
        let resolved = crate::bash_background::persistence::canonical_root_path(&later).unwrap();
        assert_eq!(
            super::simulated_volumes::device_of(&resolved),
            Some(u64::MAX - 91)
        );
    }

    #[test]
    fn single_level_glob_does_not_visit_descendants() {
        let root = tempfile::tempdir().unwrap();
        let selected = root.path().join("gh-alfonso-one");
        let deep = selected.join("profile/deep");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir(root.path().join("other")).unwrap();

        let (matches, visited) = observed_glob(&format!("{}/gh-alfonso-*", root.path().display()));
        assert_eq!(matches, vec![selected.clone()]);
        assert!(visited.contains(&selected));
        assert!(
            !visited.contains(&selected.join("profile")),
            "walk entered a matched directory"
        );
        assert!(!visited.contains(&deep), "walk visited a deeper descendant");
    }

    #[test]
    fn multi_level_glob_visits_only_matching_depth() {
        let root = tempfile::tempdir().unwrap();
        let member = root.path().join("crates/alpha");
        std::fs::create_dir_all(member.join("src/nested")).unwrap();
        let manifest = member.join("Cargo.toml");
        std::fs::write(&manifest, "").unwrap();
        let deep = member.join("src/nested/lib.rs");
        std::fs::write(&deep, "").unwrap();

        let (matches, visited) = observed_glob(&format!(
            "{}/crates/[ab]*/Cargo.toml",
            root.path().display()
        ));
        assert_eq!(matches, vec![manifest.clone()]);
        assert!(visited.contains(&manifest));
        assert!(!visited.contains(&deep), "walk exceeded the pattern depth");
        assert!(!visited.contains(&member.join("src/nested")));

        // Backslash-separated input is normalized before counting components.
        let windows_style =
            format!("{}\\crates\\[ab]*\\Cargo.toml", root.path().display()).replace('/', "\\");
        let (matches, visited) = observed_glob(&windows_style);
        assert_eq!(matches, vec![manifest]);
        assert!(
            !visited.contains(&deep),
            "backslash pattern exceeded its depth"
        );
    }

    #[test]
    fn recursive_glob_still_visits_and_matches_deep_entries() {
        let root = tempfile::tempdir().unwrap();
        let member = root.path().join("crates/alpha");
        let deep = member.join("src/nested/Cargo.toml");
        std::fs::create_dir_all(deep.parent().unwrap()).unwrap();
        let manifest = member.join("Cargo.toml");
        std::fs::write(&manifest, "").unwrap();
        std::fs::write(&deep, "").unwrap();

        let (matches, visited) =
            observed_glob(&format!("{}/crates/**/Cargo.toml", root.path().display()));
        assert_eq!(matches, vec![manifest, deep.clone()]);
        assert!(
            visited.contains(&deep),
            "recursive walk must reach deep entries"
        );
    }

    #[test]
    fn device_predicate_accepts_root_device_and_rejects_foreign_device() {
        assert!(same_filesystem_device(41, 41));
        assert!(
            !same_filesystem_device(41, 99),
            "a child on another device must not be entered"
        );
    }

    #[test]
    fn injectable_device_lookup_skips_foreign_child() {
        let boundary = DeviceBoundary::from_device_for_test(41);
        let foreign = Path::new("/simulated-foreign-mount");

        assert!(
            !boundary
                .should_descend_with(foreign, |_| Ok(Some(99)))
                .expect("simulated device lookup"),
            "the injected foreign child must be skipped"
        );
        assert!(
            boundary
                .should_descend_with(Path::new("/same-mount"), |_| Ok(Some(41)))
                .expect("simulated device lookup"),
            "the root filesystem remains walkable"
        );
        assert!(
            boundary
                .should_descend_with(Path::new("/lookup-error"), |_| {
                    Err(io::Error::other("simulated stat failure"))
                })
                .is_err(),
            "walkers must disclose stat failures instead of treating them as safe"
        );
    }
}
