//! Generation-owned derived artifacts. Unpublished files are protected by the
//! assembly pin; published files by the pointer and query read markers.
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::{Duration, Instant},
};

use rusqlite::Connection;

use super::{Result, ViewError, ViewStore};

impl ViewStore {
    pub fn derived_path(&self, generation: &str) -> Result<PathBuf> {
        let owner = self.derived_owner(generation)?;
        Ok(self.view_dir().join(format!("derived-{owner}.sqlite")))
    }

    pub(super) fn derived_owner(&self, generation: &str) -> Result<String> {
        super::validate_generation(generation)?;
        match fs::read_to_string(self.view_dir().join(format!("derived-{generation}.ref"))) {
            Ok(owner) => {
                super::validate_generation(&owner)?;
                Ok(owner)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(generation.to_owned()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn reuse_derived(&self, generation: &str, base: &str) -> Result<()> {
        super::validate_generation(generation)?;
        // Serialize new ownership references with sweeping across processes. The
        // caller keeps the base pin until this durable reference is visible.
        let mut pointer = self.open_pointer_connection()?;
        let _ownership =
            pointer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let owner = self.derived_owner(base)?;
        let path = self.view_dir().join(format!("derived-{generation}.ref"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        use std::io::Write as _;
        file.write_all(owner.as_bytes())?;
        file.sync_all()?;
        super::sync_parent(&path)
    }

    pub fn trigram_path(&self, generation: &str) -> Result<PathBuf> {
        super::validate_generation(generation)?;
        Ok(self.view_dir().join(format!("trigram-{generation}.bin")))
    }

    pub fn blob_references_by_generation(
        &self,
    ) -> Result<std::collections::BTreeMap<String, std::collections::BTreeSet<[u8; 32]>>> {
        let mut result = std::collections::BTreeMap::new();
        for entry in fs::read_dir(self.view_dir())? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(generation) = name
                .to_str()
                .and_then(|name| name.strip_prefix("manifest-"))
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            let manifest = self.load_manifest(generation)?;
            let keys = manifest
                .plane_keys()
                .filter_map(|(_, key)| {
                    if key.len() != 64 {
                        return None;
                    }
                    let bytes = (0..64)
                        .step_by(2)
                        .map(|offset| u8::from_str_radix(&key[offset..offset + 2], 16).ok())
                        .collect::<Option<Vec<_>>>()?;
                    bytes.try_into().ok()
                })
                .collect();
            result.insert(generation.to_owned(), keys);
        }
        Ok(result)
    }

    /// Remove generation files only after checking both durable publication and
    /// liveness. A dead assembler can leave a derived file before any manifest.
    pub fn sweep_generations(&self) -> Result<usize> {
        // A publisher cannot add a reference after the ownership snapshot and
        // release its base pin before the sweep checks that pin.
        let mut pointer = self.open_pointer_connection()?;
        let _ownership =
            pointer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = self.current_generation()?;
        let mut generations = std::collections::BTreeSet::new();
        let mut derived_owners = std::collections::BTreeSet::new();
        for entry in fs::read_dir(self.view_dir())? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(generation) = name
                .strip_prefix("derived-")
                .and_then(|s| s.strip_suffix(".ref"))
            {
                generations.insert(generation.to_owned());
                derived_owners.insert(self.derived_owner(generation)?);
            }
            let generation = name
                .strip_prefix("derived-")
                .and_then(|s| s.strip_suffix(".sqlite"))
                .or_else(|| {
                    name.strip_prefix("manifest-")
                        .and_then(|s| s.strip_suffix(".json"))
                })
                .or_else(|| {
                    name.strip_prefix("trigram-")
                        .and_then(|s| s.strip_suffix(".bin"))
                })
                .or_else(|| {
                    name.strip_prefix(".manifest-")
                        .and_then(|s| s.split_once(".json.tmp.").map(|(generation, _)| generation))
                });
            if let Some(generation) = generation {
                generations.insert(generation.to_owned());
            }
        }
        let mut removed = 0;
        for generation in generations {
            // Even an obsolete reference protects its owner until the next sweep.
            // This keeps shared files alive without depending on directory order.
            if current.as_deref() == Some(&generation) || derived_owners.contains(&generation) {
                continue;
            }
            let (metadata_path, keys_path) = crate::pins::pin_paths(self.view_dir(), &generation);
            if metadata_path.exists() {
                let Ok(bytes) = fs::read(&metadata_path) else {
                    continue;
                };
                let Ok(metadata) = serde_json::from_slice::<crate::pins::PinMetadata>(&bytes)
                else {
                    continue;
                };
                if crate::pins::owner_is_live(&metadata.owner)
                    && crate::pins::now_ms().saturating_sub(metadata.renewed_at)
                        <= crate::pins::PIN_TTL_MS
                {
                    continue;
                }
                let _ = fs::remove_file(metadata_path);
                let _ = fs::remove_file(keys_path);
            }
            if crate::root_cache::sweep_read_markers(self.view_dir(), &generation).protected {
                continue;
            }
            // Recheck after pin inspection: a publisher keeps its assembly pin
            // until its committed pointer is visible.
            if self.current_generation()?.as_deref() == Some(&generation) {
                continue;
            }
            self.remove_generation_files(&generation);
            removed += 1;
        }
        Ok(removed)
    }

    pub(super) fn remove_generation_files(&self, generation: &str) {
        if super::validate_generation(generation).is_err() {
            return;
        }
        let temporary_prefix = format!(".manifest-{generation}.json.tmp.");
        if let Ok(entries) = fs::read_dir(self.view_dir()) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(&temporary_prefix))
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        for path in [
            Ok(self.view_dir().join(format!("derived-{generation}.sqlite"))),
            Ok(self.view_dir().join(format!("derived-{generation}.ref"))),
            self.trigram_path(generation),
            self.manifest_path(generation),
        ]
        .into_iter()
        .flatten()
        {
            crate::db::file_identity::guard_replacement(&path, "view generation sweep");
            for suffix in ["", "-wal", "-shm"] {
                let mut name = path.as_os_str().to_owned();
                name.push(suffix);
                let _ = fs::remove_file(PathBuf::from(name));
            }
        }
    }
}

#[derive(Clone)]
struct DeferredCheckpointJob {
    path: PathBuf,
    cancelled: Arc<AtomicBool>,
}

static DEFERRED_CHECKPOINTS: OnceLock<Mutex<HashMap<PathBuf, DeferredCheckpointJob>>> =
    OnceLock::new();
static CHECKPOINT_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
const DEFERRED_CHECKPOINT_IDLE_DELAY: Duration = Duration::from_millis(250);
const DERIVED_CHECKPOINT_RESIDUAL: &str =
    "views::generation::checkpoint_derived: checkpoint bytes remain uncredited because a successful TRUNCATE result does not expose the prior backfill count";

fn checkpoint_lock(path: &Path) -> Arc<Mutex<()>> {
    let mut locks = CHECKPOINT_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

fn checkpoint_derived(
    path: &Path,
    connection: Option<&Connection>,
    ledger_root: Option<&Path>,
) -> Result<()> {
    let lock = checkpoint_lock(path);
    let _guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let owned;
    let connection = if let Some(connection) = connection {
        connection
    } else {
        owned = crate::db::file_identity::IdentityConnection::open(
            path,
            "views::generation::checkpoint_derived",
        )?;
        &owned
    };
    connection.busy_timeout(Duration::from_secs(5))?;
    // FULL makes SQLite synchronize the WAL before checkpointing and the main
    // database after copying frames. Opening either file here to fsync it would
    // release this process's advisory locks on that inode.
    connection.pragma_update(None, "synchronous", "FULL")?;
    let (busy, log_frames, checkpointed_frames): (i64, i64, i64) =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if let Some(root) = ledger_root {
        // A successful TRUNCATE returns zero counts, and this raw connection has
        // no WAL hook baseline. Leave the unknown quantity in the named residual
        // instead of inspecting the live WAL-index through another descriptor.
        crate::write_ledger::register(
            crate::write_ledger::Domain::Other,
            root.display().to_string(),
        )
        .note_seam_label(DERIVED_CHECKPOINT_RESIDUAL);
    }
    if busy != 0 {
        return Err(ViewError::InvalidManifest(format!(
            "derived WAL checkpoint remained busy: path={} log_frames={} checkpointed_frames={}",
            path.display(),
            log_frames,
            checkpointed_frames
        )));
    }
    super::sync_parent(path)?;
    Ok(())
}

/// Run the generation-sized checkpoint after pointer publication. A later
/// publication cancels a not-yet-started obsolete job; its clone has already
/// forced the source checkpoint through [`clone_derived`].
pub(super) fn schedule_derived_checkpoint(
    path: PathBuf,
    connection: crate::db::file_identity::IdentityConnection,
    root: PathBuf,
) {
    let key = path.parent().unwrap_or(&path).to_path_buf();
    let cancelled = Arc::new(AtomicBool::new(false));
    let job = DeferredCheckpointJob {
        path: path.clone(),
        cancelled: Arc::clone(&cancelled),
    };
    if let Some(previous) = DEFERRED_CHECKPOINTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key.clone(), job)
    {
        previous.cancelled.store(true, Ordering::Release);
        log::debug!(
            "view derived checkpoint superseded path={}",
            previous.path.display()
        );
    }
    let path_for_error = path.clone();
    let key_for_error = key.clone();
    let cancelled_for_error = Arc::clone(&cancelled);
    let spawn = std::thread::Builder::new()
        .name("aft-view-checkpoint".to_owned())
        .spawn(move || {
            std::thread::sleep(DEFERRED_CHECKPOINT_IDLE_DELAY);
            let started = Instant::now();
            let mut io = super::io::Window::new();
            let skipped = cancelled.load(Ordering::Acquire);
            if !skipped {
                match checkpoint_derived(&path, Some(&connection), Some(&root)) {
                    Ok(()) => log::info!(
                        "view derived checkpoint completed ms={} path={}",
                        started.elapsed().as_millis(),
                        path.display()
                    ),
                    Err(error) => log::warn!(
                        "view derived checkpoint deferred to next clone path={} error={}",
                        path.display(),
                        error
                    ),
                }
            }
            // Include the keeper close: it may checkpoint even a superseded job.
            drop(connection);
            crate::slog_info!(
                "index_event kind=view_checkpoint root={} generation={} skipped={} {}",
                root.display(),
                path.file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unknown")
                    .strip_prefix("derived-")
                    .unwrap_or("unknown"),
                skipped,
                io.finish()
            );
            let mut jobs = DEFERRED_CHECKPOINTS
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if jobs
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(&current.cancelled, &cancelled))
            {
                jobs.remove(&key);
            }
        });
    if let Err(error) = spawn {
        let mut jobs = DEFERRED_CHECKPOINTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if jobs
            .get(&key_for_error)
            .is_some_and(|current| Arc::ptr_eq(&current.cancelled, &cancelled_for_error))
        {
            jobs.remove(&key_for_error);
        }
        log::warn!(
            "view derived checkpoint worker unavailable; next clone will checkpoint path={} error={}",
            path_for_error.display(),
            error
        );
    }
}

/// Copy a coherent SQLite snapshot without opening the source file directly.
/// The backup API includes committed WAL content while preserving every lock
/// SQLite holds for other live connections in this process.
pub(super) fn clone_derived(source: &Path, destination: &Path) -> Result<()> {
    let started = Instant::now();
    let source_connection = crate::db::file_identity::IdentityConnection::open(
        source,
        "views::generation::clone_derived source",
    )?;
    source_connection.busy_timeout(Duration::from_secs(5))?;

    let mut destination_connection = {
        let _files = crate::db::file_identity::filesystem_guard();
        let open = crate::db::file_identity::open_connections(destination);
        if open != 0 {
            return Err(ViewError::InvalidManifest(format!(
                "derived clone destination has {open} live SQLite connection(s): {}",
                destination.display()
            )));
        }
        for suffix in ["", "-wal", "-shm"] {
            let mut candidate = destination.as_os_str().to_owned();
            candidate.push(suffix);
            match fs::remove_file(PathBuf::from(candidate)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        crate::db::file_identity::IdentityConnection::open(
            destination,
            "views::generation::clone_derived destination",
        )?
    };
    destination_connection.busy_timeout(Duration::from_secs(5))?;
    let backup = rusqlite::backup::Backup::new(&source_connection, &mut destination_connection)?;
    backup.run_to_completion(256, Duration::from_millis(5), None)?;
    drop(backup);

    log::info!(
        "view derived clone mechanism=sqlite_backup ms={} source={} destination={}",
        started.elapsed().as_millis(),
        source.display(),
        destination.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_checkpoint_runs_after_the_publication_path_returns() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let connection = Connection::open(&source).unwrap();
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        connection
            .pragma_update(None, "synchronous", "FULL")
            .unwrap();
        connection
            .pragma_update(None, "wal_autocheckpoint", 0)
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE state (value TEXT NOT NULL);\
                 INSERT INTO state VALUES ('durable');",
            )
            .unwrap();
        let wal = PathBuf::from(format!("{}-wal", source.display()));
        assert!(fs::metadata(&wal).unwrap().len() > 0);

        schedule_derived_checkpoint(
            source.clone(),
            crate::db::file_identity::IdentityConnection::new(connection, "checkpoint test"),
            source.parent().unwrap().to_path_buf(),
        );

        assert!(
            fs::metadata(&wal).is_ok_and(|metadata| metadata.len() > 0),
            "checkpoint ran synchronously on the publication path"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::metadata(&wal).is_ok_and(|metadata| metadata.len() > 0) {
            assert!(
                Instant::now() < deadline,
                "detached derived checkpoint did not finish"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            crate::write_ledger::pending_for_test(
                crate::write_ledger::Domain::ViewsDerived,
                &source.parent().unwrap().display().to_string(),
            )
            .1,
            0,
            "an unknowable TRUNCATE quantity must not receive guessed credit"
        );
        assert!(crate::write_ledger::seam_labels_for_test(
            crate::write_ledger::Domain::Other,
            &source.parent().unwrap().display().to_string(),
        )
        .iter()
        .any(|label| label == DERIVED_CHECKPOINT_RESIDUAL));
        assert_eq!(
            Connection::open(&source)
                .unwrap()
                .query_row("SELECT value FROM state", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "durable"
        );
    }

    #[test]
    fn clone_uses_sqlite_backup_for_committed_wal_with_a_live_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let destination = directory.path().join("destination.sqlite");
        let connection = crate::db::file_identity::IdentityConnection::open(
            &source,
            "views generation clone test source",
        )
        .unwrap();
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        connection
            .pragma_update(None, "synchronous", "FULL")
            .unwrap();
        connection
            .pragma_update(None, "wal_autocheckpoint", 0)
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE state (value TEXT NOT NULL);\
                 INSERT INTO state VALUES ('committed-in-wal');",
            )
            .unwrap();
        let wal = PathBuf::from(format!("{}-wal", source.display()));
        assert!(fs::metadata(&wal).unwrap().len() > 0);
        assert_eq!(crate::db::file_identity::open_connections(&source), 1);

        clone_derived(&source, &destination).unwrap();

        assert_eq!(
            crate::db::file_identity::open_connections(&source),
            1,
            "the clone must not close or replace the live source connection"
        );
        assert_eq!(
            Connection::open(&destination)
                .unwrap()
                .query_row("SELECT value FROM state", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "committed-in-wal"
        );
    }

    #[test]
    fn clone_refuses_to_replace_a_live_destination() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let destination = directory.path().join("destination.sqlite");
        Connection::open(&source)
            .unwrap()
            .execute_batch("CREATE TABLE source(value);")
            .unwrap();
        let destination_connection = crate::db::file_identity::IdentityConnection::open(
            &destination,
            "views generation clone test destination",
        )
        .unwrap();
        destination_connection
            .execute_batch("CREATE TABLE destination(value);")
            .unwrap();

        let error = clone_derived(&source, &destination).unwrap_err();
        assert!(
            matches!(&error, ViewError::InvalidManifest(message) if message.contains("live SQLite connection")),
            "unexpected error: {error}"
        );
        destination_connection
            .query_row("SELECT COUNT(*) FROM destination", [], |_| Ok(()))
            .unwrap();
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[test]
    fn ownership_reference_waits_for_sweep_pointer_lock() {
        let storage = tempfile::tempdir().unwrap();
        let view = ViewStore::open(storage.path(), "ownership-test").unwrap();
        let mut pointer = view.open_pointer_connection().unwrap();
        let ownership = pointer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx.send(view.reuse_derived("fill", "base")).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            matches!(
                done_rx.recv_timeout(Duration::from_millis(200)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "ownership reference escaped the sweep lock"
        );
        drop(ownership);
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
    }
}
