//! The free disk space AFT keeps on a storage device, and the free-space
//! probe it is measured with. A store refuses a write that would leave less
//! than [`free_disk_floor`] free, and records the measurement behind each
//! refusal so the agent-facing pause message can name the sizes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Minimum free bytes a storage write reservation must leave on the device.
/// The reserve is 5% of capacity, bounded by this minimum and
/// [`MAX_FREE_BYTES`].
pub const MIN_FREE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_FREE_BYTES: u64 = 32 * 1024 * 1024 * 1024;

pub(crate) fn free_disk_floor(capacity: u64) -> u64 {
    (capacity / 20).clamp(MIN_FREE_BYTES, MAX_FREE_BYTES)
}

// View loaders carry refusal strings across their error boundaries. Preserve
// the admission measurement separately so a later probe cannot turn an
// unexpected probe failure into an ordinary reserve pause.
static FLOOR_REFUSALS: Mutex<std::collections::BTreeMap<PathBuf, (u64, u64)>> =
    Mutex::new(std::collections::BTreeMap::new());

pub(crate) fn floor_refusal_measurement(storage: &Path) -> Option<(u64, u64)> {
    let key = canonical_free_space_path(storage).unwrap_or_else(|| storage.to_path_buf());
    FLOOR_REFUSALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .copied()
}

struct FreeSpaceOverrides {
    storage: std::collections::BTreeMap<PathBuf, FreeSpaceOverride>,
    unlimited: usize,
}
struct FreeSpaceOverride {
    canonical: Option<PathBuf>,
    value: Option<(u64, u64)>,
}
static FREE_SPACE: Mutex<FreeSpaceOverrides> = Mutex::new(FreeSpaceOverrides {
    storage: std::collections::BTreeMap::new(),
    unlimited: 0,
});

/// Restores a storage root's previous test probe when dropped, including on unwind.
#[doc(hidden)]
#[must_use]
pub struct FreeSpaceTestGuard {
    storage: PathBuf,
    previous: Option<FreeSpaceOverride>,
}
impl Drop for FreeSpaceTestGuard {
    fn drop(&mut self) {
        let mut overrides = FREE_SPACE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = self.previous.take() {
            overrides.storage.insert(self.storage.clone(), previous);
        } else {
            overrides.storage.remove(&self.storage);
        }
    }
}

/// Overrides probes under `storage` on every thread until the guard drops.
/// Raw and canonical spellings both match; the most specific root wins.
/// Production never calls this setter.
#[doc(hidden)]
pub fn set_free_space_for_test(storage: &Path, free: u64, capacity: u64) -> FreeSpaceTestGuard {
    set_free_space_probe_for_test(storage, Some((free, capacity)))
}

fn set_free_space_probe_for_test(storage: &Path, value: Option<(u64, u64)>) -> FreeSpaceTestGuard {
    let storage = storage.to_path_buf();
    let entry = FreeSpaceOverride {
        canonical: canonical_free_space_path(&storage),
        value,
    };
    let previous = FREE_SPACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .storage
        .insert(storage.clone(), entry);
    FreeSpaceTestGuard { storage, previous }
}

/// Admission can run before a storage root or its descendants exist. Resolve
/// the existing ancestor, preserving missing components only for comparison.
pub(crate) fn canonical_free_space_path(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        let existing = if ancestor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            ancestor
        };
        if let Ok(canonical) = fs::canonicalize(existing) {
            return Some(canonical.join(path.strip_prefix(ancestor).ok()?));
        }
    }
    None
}

#[cfg(test)]
pub(crate) struct UnlimitedFreeSpaceForTestProcess;
#[cfg(test)]
impl Drop for UnlimitedFreeSpaceForTestProcess {
    fn drop(&mut self) {
        FREE_SPACE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unlimited -= 1;
    }
}
/// Existing shared fixtures acquire their guard before creating one or more
/// storage roots. Their fallback can only allow admission; refusal simulations
/// must name a storage root and always take precedence over this fallback.
#[cfg(test)]
pub(crate) fn unlimited_free_space_for_test_process() -> UnlimitedFreeSpaceForTestProcess {
    FREE_SPACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .unlimited += 1;
    UnlimitedFreeSpaceForTestProcess
}
pub(crate) fn free_space(path: &Path) -> Option<(u64, u64)> {
    let value = {
        let overrides = FREE_SPACE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let canonical = if overrides.storage.is_empty() {
            None
        } else {
            canonical_free_space_path(path)
        };
        overrides
            .storage
            .iter()
            .filter_map(|(root, entry)| {
                let canonical_depth = canonical.as_ref().and_then(|path| {
                    entry
                        .canonical
                        .as_ref()
                        .filter(|root| path.starts_with(root))
                        .map(|root| root.components().count())
                });
                let depth = canonical_depth
                    .or_else(|| path.starts_with(root).then(|| root.components().count()))?;
                Some((depth, entry.value))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, value)| value)
            .or_else(|| (overrides.unlimited > 0).then_some(Some((u64::MAX, u64::MAX / 2))))
    };
    if let Some(value) = value {
        return value;
    }
    probe_free_space(path)
}
#[cfg(unix)]
fn probe_free_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    // The storage root may not exist yet. Probe its nearest existing ancestor
    // without creating anything, so a first-floor refusal is read-only.
    let mut existing = path;
    while !existing.exists() {
        existing = existing
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
    }
    let path = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is NUL-terminated and stat points to writable storage.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs succeeded and initialized stat.
    let stat = unsafe { stat.assume_init() };
    Some((
        (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64),
        (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64),
    ))
}
#[cfg(windows)]
fn probe_free_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    // As on unix: probe the nearest existing ancestor without creating it.
    let mut existing = path;
    while !existing.exists() {
        existing = existing
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
    }
    let wide: Vec<u16> = existing
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    // SAFETY: `wide` is NUL-terminated and the three out-pointers name live
    // u64 locals for the duration of the call.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            &mut total,
            &mut total_free,
        )
    };
    // `available` honours per-user quotas, like statvfs's f_bavail.
    (ok != 0).then_some((available, total))
}
#[cfg(not(any(unix, windows)))]
fn probe_free_space(_path: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn free_space_override_guard_restores_after_unwind() {
        let _unlimited = unlimited_free_space_for_test_process();
        let storage = tempdir().unwrap();
        let _previous = set_free_space_for_test(storage.path(), 17, 42);
        let observed = Mutex::new(None);
        let panic = std::panic::catch_unwind(|| {
            let _guard = set_free_space_for_test(storage.path(), 1, 1 << 40);
            *observed.lock().unwrap() = free_space(storage.path());
            panic!("unwind the free-space test guard");
        });
        assert!(panic.is_err());
        assert_eq!(*observed.lock().unwrap(), Some((1, 1 << 40)));
        assert_eq!(free_space(storage.path()), Some((17, 42)));
        drop(_previous);
        assert_eq!(free_space(storage.path()), Some((u64::MAX, u64::MAX / 2)));
    }

    #[test]
    fn free_space_override_reaches_other_threads_under_its_storage_root() {
        let _unlimited = unlimited_free_space_for_test_process();
        let root = tempdir().unwrap();
        let storage = root.path().join("storage");
        let _space = set_free_space_for_test(&storage, 17, 42);
        let path = storage.join("blobs/v3/plane.sqlite");
        assert_eq!(
            std::thread::spawn(move || free_space(&path))
                .join()
                .unwrap(),
            Some((17, 42))
        );
        assert_eq!(
            free_space(&root.path().join("storage-other")),
            Some((u64::MAX, u64::MAX / 2))
        );
        let inner = storage.join("nested");
        let _inner = set_free_space_for_test(&inner, 23, 84);
        assert_eq!(free_space(&inner.join("blobs/v3")), Some((23, 84)));
        assert_eq!(free_space(&storage), Some((17, 42)));
    }

    #[cfg(unix)]
    #[test]
    fn free_space_override_matches_resolved_symlink_roots() {
        let _unlimited = unlimited_free_space_for_test_process();
        let root = tempdir().unwrap();
        let storage = root.path().join("storage");
        fs::create_dir(&storage).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&storage, &alias).unwrap();
        let resolved = fs::canonicalize(&storage).unwrap();

        for suffix in ["", "not-created"] {
            let raw = alias.join(suffix);
            let canonical = resolved.join(suffix);
            let _space = set_free_space_for_test(&raw, 17, 42);
            assert_eq!(free_space(&raw), Some((17, 42)));
            assert_eq!(free_space(&canonical), Some((17, 42)));
            assert_eq!(
                free_space(&canonical.join("missing/blobs/v3")),
                Some((17, 42))
            );
            assert_eq!(
                free_space(&root.path().join("other-storage")),
                Some((u64::MAX, u64::MAX / 2))
            );
        }
        assert!(!storage.join("not-created").exists());
    }
}
