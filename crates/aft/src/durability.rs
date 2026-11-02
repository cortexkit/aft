//! Durable file commits use the platform's full sync, including F_FULLFSYNC on
//! macOS. Rebuildable and process-lifetime stores deliberately do not call these
//! sync helpers. Atomic writes select their durability explicitly. The test
//! ledger observes the real calls, never substitutes for them.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
#[derive(Default)]
struct PendingSyncs {
    devices: std::collections::BTreeMap<u64, (File, PathBuf)>,
}

#[cfg(unix)]
thread_local! {
    static SYNC_GROUP: std::cell::RefCell<Option<PendingSyncs>> = const { std::cell::RefCell::new(None) };
}

/// Keep file and directory fsync ordering, but flush the device cache once per
/// device at commit. In particular, macOS F_FULLFSYNC need not flush the same
/// device after every file. The caller must finish the group before unlinking
/// any source or acknowledging a durable backup. Dropping an unfinished group
/// abandons its device barriers; it is only safe when no source was removed.
pub(crate) struct SyncGroup {
    #[cfg(unix)]
    previous: Option<PendingSyncs>,
    #[cfg(unix)]
    active: bool,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl SyncGroup {
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(unix)]
            previous: SYNC_GROUP.with(|group| group.replace(Some(PendingSyncs::default()))),
            #[cfg(unix)]
            active: true,
            _thread: std::marker::PhantomData,
        }
    }

    #[cfg(unix)]
    fn detach(&mut self) -> PendingSyncs {
        self.active = false;
        SYNC_GROUP.with(|group| {
            group
                .replace(self.previous.take())
                .expect("active sync group")
        })
    }

    pub(crate) fn finish(mut self) -> io::Result<()> {
        #[cfg(unix)]
        for (_, (file, path)) in self.detach().devices {
            #[cfg(test)]
            if FAIL_GROUP_SYNC.with(|fail| fail.replace(false)) {
                return Err(io::Error::other("injected backup group sync failure"));
            }
            full_sync(&file, &path)?;
        }
        #[cfg(not(unix))]
        let _ = &mut self;
        Ok(())
    }
}

impl Drop for SyncGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.active {
            self.detach();
        }
    }
}

fn full_sync(file: &File, path: &Path) -> io::Result<()> {
    file.sync_all()?;
    #[cfg(test)]
    FULL_SYNCS.with(|syncs| syncs.borrow_mut().push(path.to_path_buf()));
    #[cfg(not(test))]
    let _ = path;
    Ok(())
}

fn ordered_sync(file: &File, path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if SYNC_GROUP.with(|group| group.borrow().is_some()) {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        // fsync submits each file's data and metadata in publication order.
        // The group's final full sync makes those submissions survive loss of
        // the device's volatile write cache before deletion may begin.
        loop {
            // SAFETY: the borrowed File keeps this descriptor open for fsync.
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        let device = file.metadata()?.dev();
        return SYNC_GROUP.with(|group| {
            let mut group = group.borrow_mut();
            let devices = &mut group.as_mut().expect("active sync group").devices;
            if let std::collections::btree_map::Entry::Vacant(entry) = devices.entry(device) {
                entry.insert((file.try_clone()?, path.to_path_buf()));
            }
            Ok(())
        });
    }
    full_sync(file, path)
}

// Some binaries do not use every atomic-write durability option. Callers choose
// whether private state needs no sync, file sync, or file plus directory sync.
#[derive(Clone, Copy)]
pub(crate) enum AtomicWriteDurability {
    None,
    // Callers publishing several files sync the directory once after the batch.
    File,
    FileAndDirectory,
}

/// Publish private state without leaving a partial destination or a failed temp.
/// Authorization and directory creation remain the caller's responsibility.
pub(crate) fn atomic_write(
    path: &Path,
    bytes: &[u8],
    durability: AtomicWriteDurability,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let temporary = parent.join(format!(".aft-{:032x}.tmp", u128::from_ne_bytes(random)));
    #[cfg(test)]
    before_temp_create(&temporary);
    let file = crate::private_storage::options()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let mut cleanup = TempCleanup(Some(temporary.clone()));
    // Close before replacing so Windows does not retain an open temp handle.
    (move || -> io::Result<()> {
        let mut file = file;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if file.metadata()?.permissions().mode() & 0o777 != crate::private_storage::FILE_MODE {
                file.set_permissions(fs::Permissions::from_mode(
                    crate::private_storage::FILE_MODE,
                ))?;
            }
        }
        file.write_all(bytes)?;
        if !matches!(durability, AtomicWriteDurability::None) {
            sync_file(&file, path)?;
        }
        Ok(())
    })()?;
    crate::fs_lock::rename_over(&temporary, path)?;
    cleanup.0 = None;
    record(EventKind::AtomicReplace, path);
    if matches!(durability, AtomicWriteDurability::FileAndDirectory) {
        sync_dir(parent)?;
    }
    Ok(())
}

struct TempCleanup(Option<PathBuf>);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
type BeforeTempCreate = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_TEMP_CREATE: std::cell::RefCell<Option<BeforeTempCreate>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn before_temp_create(path: &Path) {
    let hook = BEFORE_TEMP_CREATE.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(path);
    }
}

pub(crate) fn sync_file(file: &File, path: &Path) -> io::Result<()> {
    ordered_sync(file, path)?;
    record(EventKind::FileSync, path);
    Ok(())
}

pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_DIRECTORY_SYNC.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_deref() == Some(path) {
            slot.take();
            true
        } else {
            false
        }
    }) {
        return Err(FAIL_DIRECTORY_SYNC_ERRNO.with(|errno| {
            errno.take().map_or_else(
                || io::Error::other("injected directory sync failure"),
                io::Error::from_raw_os_error,
            )
        }));
    }
    #[cfg(unix)]
    {
        ordered_sync(&File::open(path)?, path)?;
        record(EventKind::DirectorySync, path);
    }
    // Windows cannot open a directory as a regular File. Its rename uses
    // MoveFileExW write-through semantics; data files are synced before rename.
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventKind {
    FileSync,
    #[cfg_attr(not(unix), allow(dead_code))]
    DirectorySync,
    DirectoryCreated,
    AtomicReplace,
    CorruptCheckpointSkipped,
}

#[cfg(test)]
thread_local! {
    #[cfg(unix)]
    pub(crate) static FAIL_GROUP_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FULL_SYNCS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
    pub(crate) static FAIL_DIRECTORY_SYNC: std::cell::RefCell<Option<PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
    // Optional errno for FAIL_DIRECTORY_SYNC; leaving it unset injects the
    // existing generic I/O error, so path-only callers need no change.
    pub(crate) static FAIL_DIRECTORY_SYNC_ERRNO: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static EVENTS: std::cell::RefCell<Vec<(EventKind, std::path::PathBuf)>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

#[cfg(test)]
pub(crate) fn take_full_syncs() -> Vec<PathBuf> {
    FULL_SYNCS.with(|syncs| std::mem::take(&mut *syncs.borrow_mut()))
}

#[inline]
pub(crate) fn record(kind: EventKind, path: &Path) {
    #[cfg(test)]
    EVENTS.with(|events| events.borrow_mut().push((kind, path.to_path_buf())));
    #[cfg(not(test))]
    let _ = (kind, path);
}

#[cfg(test)]
pub(crate) fn take() -> Vec<(EventKind, std::path::PathBuf)> {
    EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
}

#[cfg(test)]
pub(crate) fn sync_count(events: &[(EventKind, std::path::PathBuf)]) -> usize {
    events
        .iter()
        .filter(|(kind, _)| matches!(kind, EventKind::FileSync | EventKind::DirectorySync))
        .count()
}
