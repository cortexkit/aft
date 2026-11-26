//! Refuse a content open that reaches a protected credential file under
//! another name.
//!
//! File-tool floors judge paths. A hard link gives one file several names, so
//! a file under `~/.ssh` can also be reachable from a project folder, and the
//! path check passes for that second name. After a content opener opens a
//! file, and before it reads any byte, [`check`] compares the opened file with
//! the protected credential files and refuses a match.
//!
//! Only files with more than one link are judged: a file with one link has
//! only the name the path policy already judged.
//!
//! Unix has no call that lists a file's other names, so this module keeps an
//! index of the multi-link files found under the registered protected roots,
//! keyed by device and inode. Creating a hard link updates the file's change
//! time (`st_ctime`), and a new or replaced protected file is a new inode with
//! a fresh change time. So a multi-link file whose change time predates the
//! index has had the same names since the index was built, and the index's
//! answer for it is current. A multi-link file changed since then rebuilds the
//! index before it is judged. Single-link files and older multi-link files
//! (package-manager stores, local Git clones) never cause a walk.
//!
//! Windows lists a file's names directly (`FindFirstFileNameW`), so it keeps
//! no index and judges each name by location.
//!
//! Only the small credential floors are covered: the home credential folders,
//! Linux's runtime credential folders, configured read denies, the subc
//! connection file, and the closed folders and credential-named files of the
//! CortexKit data folder. AFT's storage and the rest of the CortexKit data
//! folder hold far too many entries to walk; their live databases are guarded
//! before any open by the registered SQLite file identities instead.

use std::collections::BTreeSet;
use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Folders under a CortexKit data root that file tools never open.
#[cfg(unix)]
const CLOSED_DATA_FOLDERS: [&str; 2] = ["run", "signing"];

/// Protected locations every policy built in this process registered.
#[derive(Clone, Default)]
struct Roots {
    /// Folders (or single files) whose every entry is protected.
    trees: BTreeSet<PathBuf>,
    /// CortexKit data roots: only their closed folders and credential-named
    /// files are protected.
    data_roots: BTreeSet<PathBuf>,
}

static ROOTS: parking_lot::Mutex<Roots> = parking_lot::Mutex::new(Roots {
    trees: BTreeSet::new(),
    data_roots: BTreeSet::new(),
});
/// Bumped whenever a new root is registered, so the index rebuilds.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Add protected roots. The set only grows: a root one route's policy
/// protects stays protected for every opener in the process, which can only
/// refuse more aliases, never admit one.
pub fn register(
    trees: impl IntoIterator<Item = PathBuf>,
    data_roots: impl IntoIterator<Item = PathBuf>,
) {
    let mut roots = ROOTS.lock();
    let mut added = false;
    for tree in trees {
        added |= roots.trees.insert(tree);
    }
    for root in data_roots {
        added |= roots.data_roots.insert(root);
    }
    if added {
        GENERATION.fetch_add(1, Ordering::SeqCst);
        ACTIVE.store(true, Ordering::SeqCst);
    }
}

/// The refusal for an opened file that is also a protected file.
#[derive(Debug)]
pub struct ProtectedAlias {
    protected: Option<PathBuf>,
    opened: PathBuf,
}

impl ProtectedAlias {
    /// The protected name the opened file also has. `None` when the file was
    /// refused because the protected files could not all be checked.
    pub fn protected(&self) -> Option<&Path> {
        self.protected.as_deref()
    }
}

impl std::fmt::Display for ProtectedAlias {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The access policy's generic floor refusal, so callers that only
        // show the error text use the wording a direct path would get.
        write!(
            formatter,
            "path '{}' is denied by the access policy floor",
            self.opened.display()
        )
    }
}

impl std::error::Error for ProtectedAlias {}

/// The refusal [`check`] returned, if this error is one.
pub fn protected_alias(error: &io::Error) -> Option<&ProtectedAlias> {
    error.get_ref()?.downcast_ref::<ProtectedAlias>()
}

fn refusal(opened: &Path, protected: Option<PathBuf>) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        ProtectedAlias {
            protected,
            opened: opened.to_path_buf(),
        },
    )
}

/// Judge an opened content file before any byte is read. `metadata` is the
/// opened handle's own metadata.
pub(crate) fn check(path: &Path, file: &File, metadata: &Metadata) -> io::Result<()> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let _ = file;
        unix::check(path, metadata)
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        windows::check(path, file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, file, metadata);
        Ok(())
    }
}

/// Whether `path` itself names a protected location. A caller that opened a
/// protected file by its protected name was already judged by that name, so
/// the alias check leaves it alone.
fn is_protected_location(path: &Path, roots: &Roots) -> bool {
    let Ok(path) = std::path::absolute(path) else {
        return false;
    };
    let path = crate::path_identity::normalize_path(&path);
    roots
        .trees
        .iter()
        .any(|tree| crate::policy_path::contains_resolved(&path, tree))
        || roots.data_roots.iter().any(|root| {
            matches!(
                crate::cortexkit_data::floor(root, &path, false),
                Some(
                    crate::cortexkit_data::Floor::Closed(_)
                        | crate::cortexkit_data::Floor::Credential
                )
            )
        })
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, Instant, SystemTime};

    /// Clock difference allowed between the filesystem's change times and
    /// the system clock read when an index build starts.
    const CLOCK_SLACK: Duration = Duration::from_secs(2);
    const WALK_ENTRY_LIMIT: usize = 20_000;
    const WALK_TIME_LIMIT: Duration = Duration::from_millis(500);

    pub(super) struct Index {
        generation: u64,
        built_at: SystemTime,
        /// False when the walk stopped at its entry or time bound.
        complete: bool,
        examined: usize,
        entries: HashMap<(u64, u64), PathBuf>,
    }

    static INDEX: parking_lot::Mutex<Option<Index>> = parking_lot::Mutex::new(None);
    static LOGGED_INCOMPLETE: AtomicBool = AtomicBool::new(false);

    pub(super) fn check(path: &Path, metadata: &Metadata) -> io::Result<()> {
        if metadata.nlink() <= 1 {
            return Ok(());
        }
        let identity = (metadata.dev(), metadata.ino());
        let changed = change_time(metadata);
        let generation = GENERATION.load(Ordering::SeqCst);
        let mut slot = INDEX.lock();
        if slot
            .as_ref()
            .is_none_or(|index| index.generation != generation || !index.predates(changed))
        {
            let roots = ROOTS.lock().clone();
            let index = Index::build(generation, &roots, WALK_ENTRY_LIMIT, WALK_TIME_LIMIT);
            if !index.complete && !LOGGED_INCOMPLETE.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "[aft] protected-file index stopped after {} entries; files with several links changed since then are refused",
                    index.examined
                );
            }
            *slot = Some(index);
        }
        let Some(protected) = slot
            .as_ref()
            .and_then(|index| index.verdict(identity, changed))
        else {
            return Ok(());
        };
        drop(slot);
        if is_protected_location(path, &ROOTS.lock()) {
            return Ok(());
        }
        Err(refusal(path, protected))
    }

    fn change_time(metadata: &Metadata) -> SystemTime {
        match (
            u64::try_from(metadata.ctime()),
            u32::try_from(metadata.ctime_nsec()),
        ) {
            (Ok(seconds), Ok(nanos)) => {
                SystemTime::UNIX_EPOCH + Duration::new(seconds, nanos.min(999_999_999))
            }
            // A change time before 1970 is long before any index build.
            _ => SystemTime::UNIX_EPOCH,
        }
    }

    impl Index {
        pub(super) fn build(
            generation: u64,
            roots: &Roots,
            entry_limit: usize,
            time_limit: Duration,
        ) -> Self {
            let built_at = SystemTime::now();
            let mut walk = Walk {
                entries: HashMap::new(),
                examined: 0,
                complete: true,
                entry_limit,
                deadline: Instant::now() + time_limit,
            };
            for tree in &roots.trees {
                walk.tree(tree);
            }
            for root in &roots.data_roots {
                for folder in CLOSED_DATA_FOLDERS {
                    walk.tree(&root.join(folder));
                }
                walk.credential_names(root);
            }
            Self {
                generation,
                built_at,
                complete: walk.complete,
                examined: walk.examined,
                entries: walk.entries,
            }
        }

        /// Whether a file whose status last changed at `changed` already had
        /// all its current names when this index was built.
        fn predates(&self, changed: SystemTime) -> bool {
            changed + CLOCK_SLACK < self.built_at
        }

        /// `Some` when the file must be refused, with the protected name when
        /// one is known. A file the index does not hold is refused only when
        /// the walk was cut short and the file changed after it started, so
        /// the walk may have missed its protected name.
        pub(super) fn verdict(
            &self,
            identity: (u64, u64),
            changed: SystemTime,
        ) -> Option<Option<PathBuf>> {
            if let Some(protected) = self.entries.get(&identity) {
                return Some(Some(protected.clone()));
            }
            (!self.complete && !self.predates(changed)).then_some(None)
        }

        #[cfg(test)]
        pub(super) fn summary(&self) -> (bool, usize) {
            (self.complete, self.examined)
        }
    }

    struct Walk {
        entries: HashMap<(u64, u64), PathBuf>,
        examined: usize,
        complete: bool,
        entry_limit: usize,
        deadline: Instant,
    }

    impl Walk {
        /// Count one more entry, or stop the walk at its bound.
        fn admit(&mut self) -> bool {
            if !self.complete {
                return false;
            }
            if self.examined >= self.entry_limit || Instant::now() >= self.deadline {
                self.complete = false;
                return false;
            }
            self.examined += 1;
            true
        }

        /// Record a multi-link regular file. Only `lstat` is used, so no
        /// protected file (a live database included) is ever opened.
        fn note(&mut self, path: &Path, metadata: &Metadata) {
            if metadata.is_file() && metadata.nlink() > 1 {
                self.entries
                    .insert((metadata.dev(), metadata.ino()), path.to_path_buf());
            }
        }

        fn tree(&mut self, root: &Path) {
            if !self.admit() {
                return;
            }
            let Ok(metadata) = std::fs::symlink_metadata(root) else {
                return;
            };
            if !metadata.is_dir() {
                self.note(root, &metadata);
                return;
            }
            // Stay on the root's filesystem: a mount below a protected folder
            // (a network share under /run/user, for example) is not walked.
            let device = metadata.dev();
            let mut pending = vec![root.to_path_buf()];
            while let Some(directory) = pending.pop() {
                // A folder this process cannot list is skipped, not treated
                // as an incomplete walk: other users' runtime folders under
                // /run/user are unreadable by design.
                let Ok(entries) = std::fs::read_dir(&directory) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if !self.admit() {
                        return;
                    }
                    let path = entry.path();
                    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                        continue;
                    };
                    if metadata.is_dir() {
                        if metadata.dev() == device {
                            pending.push(path);
                        }
                    } else {
                        self.note(&path, &metadata);
                    }
                }
            }
        }

        /// Credential-named files in a CortexKit data root's top level and in
        /// each module folder's top level. Deeper folders hold module data
        /// and checkouts, which are too large to walk.
        fn credential_names(&mut self, root: &Path) {
            let mut folders = vec![(root.to_path_buf(), true)];
            while let Some((folder, descend)) = folders.pop() {
                let Ok(entries) = std::fs::read_dir(&folder) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if !self.admit() {
                        return;
                    }
                    let path = entry.path();
                    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                        continue;
                    };
                    if metadata.is_dir() {
                        let closed = CLOSED_DATA_FOLDERS
                            .iter()
                            .any(|name| entry.file_name() == std::ffi::OsStr::new(name));
                        if descend && !closed {
                            folders.push((path, false));
                        }
                    } else if matches!(
                        crate::cortexkit_data::floor(root, &path, false),
                        Some(crate::cortexkit_data::Floor::Credential)
                    ) {
                        self.note(&path, &metadata);
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;

    pub(super) fn check(path: &Path, file: &File) -> io::Result<()> {
        if crate::file_identity::link_count(file)? <= 1 {
            return Ok(());
        }
        let roots = ROOTS.lock().clone();
        if is_protected_location(path, &roots) {
            return Ok(());
        }
        // A file whose names cannot be listed cannot be shown to be outside
        // every protected folder, so it is refused.
        let names = crate::task_temp_reads::opened_location(file)
            .and_then(|opened| link_names(&opened))
            .map_err(|_| refusal(path, None))?;
        match names
            .into_iter()
            .map(|name| crate::path_identity::normalize_path(&name))
            .find(|name| is_protected_location(name, &roots))
        {
            Some(protected) => Err(refusal(path, Some(protected))),
            None => Ok(()),
        }
    }

    /// Every name of the file at `opened`, a final path from the open handle.
    fn link_names(opened: &Path) -> io::Result<Vec<PathBuf>> {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows_sys::Win32::Foundation::{
            ERROR_HANDLE_EOF, ERROR_MORE_DATA, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            FindClose, FindFirstFileNameW, FindNextFileNameW,
        };

        let wide: Vec<u16> = opened.as_os_str().encode_wide().chain(Some(0)).collect();
        // Listed names start at the volume root without the volume itself,
        // so each is joined to the opened path's volume prefix.
        let volume: PathBuf = opened
            .components()
            .take_while(|component| matches!(component, std::path::Component::Prefix(_)))
            .collect();
        let name = |buffer: &[u16]| {
            let end = buffer
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(buffer.len());
            volume.join(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..end])))
        };
        let mut buffer = vec![0u16; 512];
        let find = loop {
            let mut length = u32::try_from(buffer.len()).map_err(io::Error::other)?;
            // SAFETY: `wide` is NUL-terminated and `buffer` holds `length`
            // UTF-16 units for the whole call.
            let find =
                unsafe { FindFirstFileNameW(wide.as_ptr(), 0, &mut length, buffer.as_mut_ptr()) };
            if find != INVALID_HANDLE_VALUE {
                break find;
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_MORE_DATA as i32)
                || length as usize <= buffer.len()
            {
                return Err(error);
            }
            buffer.resize(length as usize, 0);
        };
        struct Search(windows_sys::Win32::Foundation::HANDLE);
        impl Drop for Search {
            fn drop(&mut self) {
                // SAFETY: the handle came from a successful FindFirstFileNameW.
                unsafe { FindClose(self.0) };
            }
        }
        let search = Search(find);
        let mut names = vec![name(&buffer)];
        loop {
            let mut length = u32::try_from(buffer.len()).map_err(io::Error::other)?;
            // SAFETY: the search handle is open and `buffer` holds `length`
            // UTF-16 units for the whole call.
            if unsafe { FindNextFileNameW(search.0, &mut length, buffer.as_mut_ptr()) } != 0 {
                names.push(name(&buffer));
                continue;
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(code) if code == ERROR_HANDLE_EOF as i32 => return Ok(names),
                Some(code) if code == ERROR_MORE_DATA as i32 && length as usize > buffer.len() => {
                    buffer.resize(length as usize, 0);
                }
                _ => return Err(error),
            }
        }
    }
}

#[cfg(test)]
#[path = "opened_floor_tests.rs"]
mod tests;
