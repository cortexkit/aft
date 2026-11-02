//! Cache expensive private-size probes, not namespace totals. Every measurement
//! still inventories names and metadata without opening any database file.

use super::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

#[derive(Clone, PartialEq, Eq)]
struct Identity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    created: Option<std::time::SystemTime>,
}

#[derive(Clone, PartialEq, Eq)]
struct Version {
    identity: Identity,
    len: u64,
    allocated: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(unix)]
    links: u64,
}

pub(super) struct Entry {
    version: Version,
    private: bool,
    shares_data: bool,
    directory: bool,
    charge: Option<u64>,
}

impl Entry {
    pub(super) fn new(meta: &fs::Metadata, private: bool, shares_data: bool) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            version: Version {
                identity: Identity {
                    #[cfg(unix)]
                    device: meta.dev(),
                    #[cfg(unix)]
                    inode: meta.ino(),
                    created: meta.created().ok(),
                },
                len: meta.len(),
                allocated: file_allocated(meta),
                modified: meta.modified().ok(),
                #[cfg(unix)]
                changed: (meta.ctime(), meta.ctime_nsec()),
                #[cfg(unix)]
                links: meta.nlink(),
            },
            private,
            shares_data,
            directory: meta.is_dir(),
            charge: None,
        }
    }
}

#[derive(Default)]
struct Inventory {
    entries: BTreeMap<PathBuf, Entry>,
}

fn inventory(path: &Path) -> Result<Arc<Mutex<Inventory>>> {
    type Cache = HashMap<PathBuf, (Instant, Arc<Mutex<Inventory>>)>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let path = fs::canonicalize(path)
        .map_err(|e| crate::views::io::at("resolving v3 accounting directory", path, e))?;
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Evict unused inventories rather than retaining them indefinitely. An
    // expired cache only costs fresh probes; it never changes the charge.
    cache.retain(|_, (used, _)| used.elapsed() < Duration::from_secs(600));
    let (used, inventory) = cache
        .entry(path)
        .or_insert_with(|| (Instant::now(), Arc::new(Mutex::new(Inventory::default()))));
    *used = Instant::now();
    Ok(inventory.clone())
}

pub(super) fn measure(path: &Path, deadline: Option<Instant>) -> Result<Option<u64>> {
    let inventory = inventory(path)?;
    let mut inventory = if deadline.is_some() {
        match inventory.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
        }
    } else {
        inventory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    let mut entries = BTreeMap::new();
    if !walk_allocated(path, deadline, Level::Family, &mut entries)? {
        return Ok(None);
    }
    // Deleting or replacing a clone changes a sibling's private size without
    // changing the sibling's metadata. Copy-on-write updates to a base or
    // member can do the same. The family registry and blob databases outside
    // views/ (including SQLite recreating their sidecars at the same names)
    // do not share view data blocks.
    let invalidate_private = entries.len() != inventory.entries.len()
        || entries.iter().any(|(path, entry)| {
            inventory.entries.get(path).is_none_or(|old| {
                old.directory != entry.directory
                    || ((entry.directory || entry.shares_data || old.shares_data)
                        && old.version.identity != entry.version.identity)
                    || ((entry.shares_data || old.shares_data) && old.version != entry.version)
            })
        });
    let mut total = 0u64;
    for (path, entry) in &mut entries {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(None);
        }
        if entry.directory {
            continue;
        }
        let allocated = entry.version.allocated;
        let charge = if entry.private {
            let cached = inventory
                .entries
                .get(path)
                .filter(|old| !invalidate_private && old.private && old.version == entry.version)
                .and_then(|old| old.charge);
            if let Some(charge) = cached {
                entry.charge = Some(charge);
                charge
            } else {
                let (charge, reusable) = unshared::measure(path, allocated);
                entry.charge = reusable.then_some(charge);
                charge
            }
        } else {
            allocated
        };
        total = total.saturating_add(charge);
    }
    inventory.entries = entries;
    Ok(Some(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(root: &Path, scope: &str) -> PathBuf {
        let path = root.join("views").join(scope);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn incremental_measurement_reuses_unchanged_files_and_measures_growth_and_new_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = member(root.path(), "worker");
        let path = dir.join("payload");
        fs::write(&path, [7; 4096]).unwrap();
        unshared::with_private_bytes_for_test(
            |path| Some(fs::metadata(path).unwrap().len()),
            || {
                assert_eq!(allocated_bytes(root.path()).unwrap(), 4096);
                let before = work_profile::count("private_size_measurement");
                assert_eq!(allocated_bytes(root.path()).unwrap(), 4096);
                assert_eq!(work_profile::count("private_size_measurement"), before);
                fs::write(&path, [8; 8192]).unwrap();
                assert_eq!(allocated_bytes(root.path()).unwrap(), 8192);
                assert_eq!(work_profile::count("private_size_measurement"), before + 1);
                fs::write(dir.join("new"), [9; 4096]).unwrap();
                assert_eq!(allocated_bytes(root.path()).unwrap(), 12288);
                assert_eq!(work_profile::count("private_size_measurement"), before + 3);
                fs::remove_file(&path).unwrap();
                assert_eq!(allocated_bytes(root.path()).unwrap(), 4096);
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn incremental_measurement_invalidates_sibling_private_sizes_after_clone_deletion() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let survivor = member(root.path(), "one").join("clone");
        let sibling = member(root.path(), "two").join("clone");
        fs::write(&survivor, [1; 4096]).unwrap();
        fs::write(&sibling, [1; 4096]).unwrap();
        let probe_sibling = sibling.clone();
        // Model APFS's private-size change on unlink on every test platform.
        // The native APFS test below also checks the real filesystem query.
        unshared::with_private_bytes_for_test(
            move |_| Some(if probe_sibling.exists() { 0 } else { 4096 }),
            || {
                assert_eq!(allocated_bytes(root.path()).unwrap(), 0);
                let before = fs::metadata(&survivor).unwrap();
                fs::remove_file(&sibling).unwrap();
                let after = fs::metadata(&survivor).unwrap();
                assert_eq!(
                    (
                        before.ino(),
                        before.len(),
                        before.mtime(),
                        before.mtime_nsec()
                    ),
                    (after.ino(), after.len(), after.mtime(), after.mtime_nsec())
                );
                assert_eq!(allocated_bytes(root.path()).unwrap(), 4096);
            },
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn incremental_measurement_apfs_clone_deletion_remeasures_unchanged_sibling() {
        use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
        let root = tempfile::tempdir().unwrap();
        let survivor = member(root.path(), "one").join("clone");
        let sibling = member(root.path(), "two").join("clone");
        fs::write(&survivor, vec![7; 1 << 20]).unwrap();
        let from = std::ffi::CString::new(survivor.as_os_str().as_bytes()).unwrap();
        let to = std::ffi::CString::new(sibling.as_os_str().as_bytes()).unwrap();
        // SAFETY: both paths are valid, NUL-terminated strings for the call.
        assert_eq!(
            unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), 0) },
            0,
            "APFS clonefile required: {}",
            std::io::Error::last_os_error()
        );
        let shared = allocated_bytes(root.path()).unwrap();
        let before = fs::metadata(&survivor).unwrap();
        fs::remove_file(&sibling).unwrap();
        let after = fs::metadata(&survivor).unwrap();
        assert_eq!(
            (
                before.ino(),
                before.len(),
                before.mtime(),
                before.mtime_nsec()
            ),
            (after.ino(), after.len(), after.mtime(), after.mtime_nsec())
        );
        let unshared = allocated_bytes(root.path()).unwrap();
        assert!(
            unshared > shared,
            "private size must grow when the other clone is deleted"
        );
        assert_eq!(
            unshared,
            super::super::unshared::charge(&survivor, file_allocated(&after))
        );
    }
}
