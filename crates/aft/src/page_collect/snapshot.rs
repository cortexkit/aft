//! Private, short-lived snapshots of collected page files.
//!
//! Each snapshot is a folder under `<AFT storage>/page-snapshots/`, created
//! owner-only through `private_storage`, holding one file per collected
//! file under a generated name (never the page's own spelling). The bytes
//! live only on disk; memory holds the index of names, sizes and hashes.
//!
//! At most [`MAX_LIVE`] snapshots exist at once, counting collections still
//! in progress. Each expires [`TTL`] after it is complete, and is deleted
//! then, on `page.release`, or (for leftovers of an earlier process) when the
//! daemon starts.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::{mime_for, FileRecord, SnapshotSink};

/// The storage folder that holds every snapshot.
pub(crate) const SNAPSHOT_DIR: &str = "page-snapshots";
/// Most snapshots alive at once, including collections in progress.
pub(crate) const MAX_LIVE: usize = 4;
/// How long a completed snapshot lives.
pub(crate) const TTL: Duration = Duration::from_secs(60);

/// Why `page.read` refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadRefusal {
    /// The snapshot is unknown: expired, released, or never created.
    Expired,
    NotInSnapshot,
    BadRange,
}

impl ReadRefusal {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Expired => "snapshot_expired",
            Self::NotInSnapshot => "not_in_snapshot",
            Self::BadRange => "bad_range",
        }
    }
}

/// A fifth collection while [`MAX_LIVE`] snapshots are alive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Busy {
    /// When the oldest live snapshot expires, from now.
    pub retry_after_ms: u64,
}

/// Why [`SnapshotStore::begin`] could not start a snapshot.
#[derive(Debug)]
pub(crate) enum BeginError {
    Busy(Busy),
    Io(io::Error),
}

struct StoredFile {
    disk_name: String,
    size: u64,
}

/// The harness and agent session a snapshot was collected for.
/// `page.read` and `page.release` name both, and a snapshot answers only to
/// that pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotOwner {
    pub harness: String,
    pub session: String,
}

struct Snapshot {
    dir: PathBuf,
    expires: Instant,
    owner: SnapshotOwner,
    files: BTreeMap<String, StoredFile>,
}

#[derive(Default)]
struct State {
    ready: HashMap<String, Arc<Snapshot>>,
    /// Collections in progress, each holding one of the [`MAX_LIVE`] places.
    building: usize,
}

/// The snapshots of one AFT storage root.
pub(crate) struct SnapshotStore {
    root: PathBuf,
    state: Mutex<State>,
    /// The store's clock in tests. It moves only when a test advances it,
    /// so time a slow machine spends writing snapshot files cannot carry a
    /// snapshot past an expiry boundary the test is checking.
    #[cfg(test)]
    clock: Mutex<Instant>,
}

impl SnapshotStore {
    /// The store for `storage_dir`. The first call in a process removes
    /// snapshots an earlier process left behind; the daemon makes that call
    /// when it starts.
    pub(crate) fn for_storage(storage_dir: &Path) -> Arc<SnapshotStore> {
        static STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<SnapshotStore>>>> = OnceLock::new();
        let mut stores = STORES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            stores
                .entry(storage_dir.to_path_buf())
                .or_insert_with(|| Arc::new(SnapshotStore::open(storage_dir))),
        )
    }

    /// A store under `storage_dir` that starts by deleting every snapshot
    /// folder already there.
    pub(crate) fn open(storage_dir: &Path) -> SnapshotStore {
        let root = storage_dir.join(SNAPSHOT_DIR);
        if let Err(error) = std::fs::remove_dir_all(&root) {
            if error.kind() != io::ErrorKind::NotFound {
                log::warn!(
                    "page snapshots: could not remove leftovers in {}: {error}",
                    root.display()
                );
            }
        }
        SnapshotStore {
            root,
            state: Mutex::new(State::default()),
            #[cfg(test)]
            clock: Mutex::new(Instant::now()),
        }
    }

    fn now(&self) -> Instant {
        #[cfg(test)]
        {
            *self.clock.lock().unwrap()
        }
        #[cfg(not(test))]
        {
            Instant::now()
        }
    }

    #[cfg(test)]
    pub(crate) fn advance_for_test(&self, by: Duration) {
        *self.clock.lock().unwrap() += by;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Remove expired snapshots from the index; their folders are returned
    /// for deletion outside the lock.
    fn take_expired(&self, state: &mut State) -> Vec<PathBuf> {
        let now = self.now();
        let expired = state
            .ready
            .iter()
            .filter(|(_, snapshot)| snapshot.expires <= now)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        expired
            .into_iter()
            .filter_map(|id| state.ready.remove(&id))
            .map(|snapshot| snapshot.dir.clone())
            .collect()
    }

    /// Delete every expired snapshot now.
    pub(crate) fn expire_due(&self) {
        let dirs = {
            let mut state = self.lock();
            self.take_expired(&mut state)
        };
        remove_dirs(dirs);
    }

    /// Start a snapshot for `owner`, or refuse with [`Busy`] when
    /// [`MAX_LIVE`] are live.
    pub(crate) fn begin(self: &Arc<Self>, owner: SnapshotOwner) -> Result<Builder, BeginError> {
        let expired;
        {
            let mut state = self.lock();
            expired = self.take_expired(&mut state);
            if state.ready.len() + state.building >= MAX_LIVE {
                let now = self.now();
                let retry_after_ms = state
                    .ready
                    .values()
                    .map(|snapshot| snapshot.expires.saturating_duration_since(now))
                    .min()
                    // Only collections in progress: they finish within their
                    // deadline, which is at most a few seconds.
                    .unwrap_or(Duration::from_secs(1));
                drop(state);
                remove_dirs(expired);
                return Err(BeginError::Busy(Busy {
                    retry_after_ms: retry_after_ms.as_millis().max(1) as u64,
                }));
            }
            state.building += 1;
        }
        remove_dirs(expired);
        let reservation = Reservation(Some(Arc::clone(self)));
        let id = new_id().map_err(BeginError::Io)?;
        let dir = self.root.join(&id);
        crate::private_storage::create_dir_all(&self.root)
            .and_then(|()| crate::private_storage::create_dir(&dir))
            .map_err(BeginError::Io)?;
        Ok(Builder {
            reservation: Some(reservation),
            id,
            owner,
            dir,
            files: BTreeMap::new(),
            committed: false,
        })
    }

    /// Bytes `offset..offset+length` (cut at the end of the file) of
    /// `path` in snapshot `id`, read from the snapshot folder only. A
    /// snapshot of another session is answered exactly like one that does
    /// not exist.
    pub(crate) fn read(
        &self,
        id: &str,
        owner: &SnapshotOwner,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, ReadRefusal> {
        let (snapshot, expired) = {
            let mut state = self.lock();
            let expired = self.take_expired(&mut state);
            (state.ready.get(id).cloned(), expired)
        };
        remove_dirs(expired);
        let snapshot = snapshot
            .filter(|snapshot| snapshot.owner == *owner)
            .ok_or(ReadRefusal::Expired)?;
        let file = snapshot.files.get(path).ok_or(ReadRefusal::NotInSnapshot)?;
        if length == 0 || length > super::MAX_READ_LENGTH || offset > file.size {
            return Err(ReadRefusal::BadRange);
        }
        let take = length.min(file.size - offset);
        // A release or expiry racing this read deletes the folder; the read
        // then reports the snapshot as gone.
        let mut handle =
            File::open(snapshot.dir.join(&file.disk_name)).map_err(|_| ReadRefusal::Expired)?;
        handle
            .seek(SeekFrom::Start(offset))
            .map_err(|_| ReadRefusal::Expired)?;
        let mut bytes = Vec::with_capacity(take as usize);
        handle
            .take(take)
            .read_to_end(&mut bytes)
            .map_err(|_| ReadRefusal::Expired)?;
        Ok(bytes)
    }

    /// Delete snapshot `id` if it is `owner`'s. Returns whether it was
    /// live; a snapshot of another session is left alone and reported as
    /// not live, as an unknown one is.
    pub(crate) fn release(&self, id: &str, owner: &SnapshotOwner) -> bool {
        let (removed, expired) = {
            let mut state = self.lock();
            let removed = match state.ready.get(id) {
                Some(snapshot) if snapshot.owner == *owner => state.ready.remove(id),
                _ => None,
            };
            (removed, self.take_expired(&mut state))
        };
        let live = removed.is_some();
        remove_dirs(
            removed
                .map(|snapshot| snapshot.dir.clone())
                .into_iter()
                .chain(expired),
        );
        live
    }

    /// Number of snapshots alive or being built.
    #[cfg(test)]
    pub(crate) fn live_count(&self) -> usize {
        let state = self.lock();
        state.ready.len() + state.building
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

fn remove_dirs(dirs: impl IntoIterator<Item = PathBuf>) {
    for dir in dirs {
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            if error.kind() != io::ErrorKind::NotFound {
                log::warn!(
                    "page snapshots: could not remove {}: {error}",
                    dir.display()
                );
            }
        }
    }
}

fn new_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Gives back a collection's place among the [`MAX_LIVE`] when dropped,
/// unless [`Builder::commit`] took the store out to publish the snapshot.
struct Reservation(Option<Arc<SnapshotStore>>);

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(store) = self.0.take() {
            let mut state = store.lock();
            state.building = state.building.saturating_sub(1);
        }
    }
}

/// A snapshot being written. Dropped without [`Builder::commit`], it deletes
/// its folder and frees its place.
pub(crate) struct Builder {
    reservation: Option<Reservation>,
    id: String,
    owner: SnapshotOwner,
    dir: PathBuf,
    files: BTreeMap<String, StoredFile>,
    committed: bool,
}

impl SnapshotSink for Builder {
    fn add(&mut self, path: &str, bytes: &[u8]) -> io::Result<FileRecord> {
        let disk_name = format!("f{}", self.files.len());
        let mut file = crate::private_storage::options()
            .write(true)
            .create_new(true)
            .open(self.dir.join(&disk_name))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        let record = FileRecord {
            path: path.to_string(),
            size: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
            mime: mime_for(path),
        };
        self.files.insert(
            path.to_string(),
            StoredFile {
                disk_name,
                size: record.size,
            },
        );
        Ok(record)
    }

    fn bytes(&self, path: &str) -> io::Result<Vec<u8>> {
        let file = self
            .files
            .get(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not in this snapshot"))?;
        std::fs::read(self.dir.join(&file.disk_name))
    }
}

impl Builder {
    /// Publish the snapshot. Returns its id and when it expires, in
    /// milliseconds since the Unix epoch.
    pub(crate) fn commit(mut self) -> (String, u64) {
        let store = self
            .reservation
            .as_mut()
            .and_then(|reservation| reservation.0.take())
            .expect("a builder is committed once");
        let expires_at_ms = SystemTime::now()
            .checked_add(TTL)
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |at| at.as_millis() as u64);
        let snapshot = Arc::new(Snapshot {
            dir: self.dir.clone(),
            expires: store.now() + TTL,
            owner: self.owner.clone(),
            files: std::mem::take(&mut self.files),
        });
        {
            let mut state = store.lock();
            state.ready.insert(self.id.clone(), snapshot);
            // The place moves from "building" to "ready" under one lock.
            state.building = state.building.saturating_sub(1);
        }
        self.committed = true;
        (self.id.clone(), expires_at_ms)
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        if !self.committed {
            remove_dirs([self.dir.clone()]);
        }
    }
}
