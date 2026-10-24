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
use serde::{
    de::{Error as _, SeqAccess, Visitor},
    Deserialize, Deserializer,
};

use super::{Result, ViewError, ViewStore};

// Protection scans must finish within one common allowance before any unlink.
const GENERATION_DIRECTORY_LIMIT: usize = 65_536;

#[derive(Debug, Default)]
pub(crate) struct GenerationSweep {
    pub removed_generations: usize,
    pub removed_bytes: u64,
}

impl GenerationSweep {
    fn add(&mut self, other: Self) {
        self.removed_generations += other.removed_generations;
        self.removed_bytes = self.removed_bytes.saturating_add(other.removed_bytes);
    }
}

// Stat, rather than opening the file: a second descriptor on a live SQLite
// inode can invalidate its process's locks when that descriptor is closed.
fn unlink_generation_file(path: &Path) -> Option<u64> {
    let bytes = fs::symlink_metadata(path).ok()?.len();
    fs::remove_file(path).ok()?;
    Some(bytes)
}

// A view has at most one cleanup worker. Publications arriving during a sweep
// request another lap rather than starting competing pointer transactions.
static GENERATION_SWEEPS: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
#[cfg(test)]
static GENERATION_SWEEP_SETTLED: std::sync::Condvar = std::sync::Condvar::new();

pub(super) fn schedule_generation_sweep(view: ViewStore) {
    let path = view.view_dir().to_path_buf();
    let jobs = GENERATION_SWEEPS.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let mut jobs = jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pending) = jobs.get_mut(&path) {
            *pending = true;
            return;
        }
        jobs.insert(path.clone(), false);
    }
    let failed_path = path.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("aft-view-generation-sweep".into())
        .spawn(move || loop {
            #[cfg(test)]
            crate::storage_retention::test_hook(&path, "post-publication-sweep");
            // An obsolete .ref protects its owner for the whole first census.
            // A second complete census can reclaim that now-unreferenced owner.
            match view.sweep_generations_with_bytes().and_then(|mut removed| {
                removed.add(view.sweep_generations_with_bytes()?);
                Ok(removed)
            }) {
                Ok(removed) => log::info!(
                    "views: post-publication removed_generations={} removed_bytes={} view={}",
                    removed.removed_generations,
                    removed.removed_bytes,
                    path.display()
                ),
                Err(error) => log::warn!(
                    "views: post-publication generation sweep deferred view={} error={error}",
                    path.display()
                ),
            }
            let mut jobs = jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if jobs
                .get_mut(&path)
                .is_some_and(|pending| std::mem::take(pending))
            {
                continue;
            }
            jobs.remove(&path);
            #[cfg(test)]
            GENERATION_SWEEP_SETTLED.notify_all();
            break;
        })
    {
        jobs.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&failed_path);
        #[cfg(test)]
        GENERATION_SWEEP_SETTLED.notify_all();
        log::warn!(
            "views: generation sweep worker unavailable view={} error={error}",
            failed_path.display()
        );
    }
}

#[cfg(test)]
pub(crate) fn wait_for_generation_sweep_for_test(path: &Path) {
    let jobs = GENERATION_SWEEPS.get_or_init(|| Mutex::new(HashMap::new()));
    let (jobs, timeout) = GENERATION_SWEEP_SETTLED
        .wait_timeout_while(jobs.lock().unwrap(), Duration::from_secs(20), |jobs| {
            jobs.contains_key(path)
        })
        .unwrap();
    assert!(
        !timeout.timed_out() && !jobs.contains_key(path),
        "generation sweep did not settle: {}",
        path.display()
    );
}

fn generation_directory_entries(directory: &Path, limit: usize) -> Result<Vec<fs::DirEntry>> {
    let entries = fs::read_dir(directory)?
        .take(limit.saturating_add(1))
        .collect::<std::io::Result<Vec<_>>>()?;
    if entries.len() > limit {
        return Err(ViewError::InvalidManifest(format!(
            "generation directory scan exceeded {limit} entries"
        )));
    }
    Ok(entries)
}

#[derive(Deserialize)]
struct ReferenceCensus {
    path_identity_version: u8,
    #[serde(default)]
    format_version: Option<u8>,
    #[serde(deserialize_with = "reference_entries")]
    entries: ReferenceEntries,
}

#[derive(Default)]
struct ReferenceEntries {
    keys: std::collections::BTreeSet<[u8; 32]>,
    legacy_seen: bool,
    states_seen: bool,
    synthetic_without_key: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ReferenceKey {
    Legacy(String),
    State { state: String, key: Option<String> },
}

#[derive(Deserialize)]
struct ReferenceMember {
    kind: String,
    planes: Option<std::collections::BTreeMap<String, Option<ReferenceKey>>>,
}

fn reference_entries<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<ReferenceEntries, D::Error> {
    struct EntriesVisitor;
    impl<'de> Visitor<'de> for EntriesVisitor {
        type Value = ReferenceEntries;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("manifest entries")
        }
        fn visit_seq<S: SeqAccess<'de>>(
            self,
            mut sequence: S,
        ) -> std::result::Result<Self::Value, S::Error> {
            let mut result = ReferenceEntries::default();
            while let Some(member) = sequence.next_element::<ReferenceMember>()? {
                let planes = match member.kind.as_str() {
                    "regular" | "synthetic" => member
                        .planes
                        .ok_or_else(|| S::Error::missing_field("planes"))?,
                    "symlink" | "gitlink" => member.planes.unwrap_or_default(),
                    _ => {
                        return Err(S::Error::custom(
                            "unsupported member kind in reference census",
                        ))
                    }
                };
                if member.kind == "synthetic"
                    && !planes.get("callgraph").is_some_and(Option::is_some)
                {
                    result.synthetic_without_key = true;
                }
                for key in planes.into_values().flatten() {
                    let key = match key {
                        ReferenceKey::Legacy(key) => {
                            result.legacy_seen = true;
                            Some(key)
                        }
                        ReferenceKey::State { state, key } => {
                            result.states_seen = true;
                            match state.as_str() {
                                "ready" => Some(key.ok_or_else(|| S::Error::missing_field("key"))?),
                                "pending" | "failed" => None,
                                _ => {
                                    return Err(S::Error::custom(
                                        "unsupported plane state in reference census",
                                    ))
                                }
                            }
                        }
                    };
                    if let Some(key) = key.and_then(|key| crate::blob_store::v2::parse_hex32(&key))
                    {
                        result.keys.insert(key);
                    }
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_seq(EntriesVisitor)
}

fn manifest_reference_keys(
    generation: &str,
    bytes: &[u8],
) -> Result<std::collections::BTreeSet<[u8; 32]>> {
    let census: ReferenceCensus = match serde_json::from_slice(bytes) {
        Ok(census) => census,
        Err(error) => {
            if let Some(name) = super::manifest_v2::GenerationName::parse(generation) {
                return validated_v2_reference_keys(&name, bytes);
            }
            return Err(error.into());
        }
    };
    if census.path_identity_version != super::PATH_IDENTITY_VERSION {
        return Err(ViewError::InvalidManifest(
            "unsupported path identity in reference census".into(),
        ));
    }
    match census.format_version {
        None if !census.entries.states_seen && !census.entries.synthetic_without_key => {
            Ok(census.entries.keys)
        }
        None => Err(ViewError::InvalidManifest(
            "invalid legacy reference encoding".into(),
        )),
        Some(super::manifest_v2::MANIFEST_FORMAT_VERSION) if !census.entries.legacy_seen => {
            let name = super::manifest_v2::GenerationName::parse(generation).ok_or_else(|| {
                ViewError::GenerationMismatch("invalid v2 reference generation name".into())
            })?;
            // Writers use canonical bytes with one trailing newline. Compatible
            // noncanonical inputs must still match the generation name's content fingerprint.
            let canonical = bytes.strip_suffix(b"\n").unwrap_or(bytes);
            if super::manifest_v2::fingerprint_bytes(canonical) != *name.content() {
                return validated_v2_reference_keys(&name, bytes);
            }
            Ok(census.entries.keys)
        }
        Some(version) => Err(ViewError::InvalidManifest(format!(
            "unsupported reference manifest format {version}"
        ))),
    }
}

fn validated_v2_reference_keys(
    name: &super::manifest_v2::GenerationName,
    bytes: &[u8],
) -> Result<std::collections::BTreeSet<[u8; 32]>> {
    let manifest = super::manifest_v2::ManifestV2::from_json_bytes(bytes)?;
    if manifest.content_fingerprint()? != *name.content() {
        return Err(ViewError::GenerationMismatch(
            "reference manifest does not match its generation".into(),
        ));
    }
    Ok(manifest.ready_keys().map(|key| *key.as_bytes()).collect())
}

impl ViewStore {
    pub fn derived_path(&self, generation: &str) -> Result<PathBuf> {
        let owner = self.derived_owner(generation)?;
        Ok(self.view_dir().join(format!("derived-{owner}.sqlite")))
    }

    pub(super) fn derived_owner(&self, generation: &str) -> Result<String> {
        super::validate_generation(generation)?;
        let path = self.view_dir().join(format!("derived-{generation}.ref"));
        match fs::read_to_string(&path) {
            Ok(owner) => {
                super::validate_generation(&owner)?;
                Ok(owner)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(generation.to_owned()),
            Err(error) => Err(ViewError::io_at("reading", &path, error)),
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
        let mut file = crate::private_storage::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| ViewError::io_at("creating", &path, error))?;
        use std::io::Write as _;
        file.write_all(owner.as_bytes())
            .map_err(|error| ViewError::io_at("writing", &path, error))?;
        file.sync_all()
            .map_err(|error| ViewError::io_at("syncing", &path, error))?;
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
        for entry in generation_directory_entries(self.view_dir(), GENERATION_DIRECTORY_LIMIT)? {
            let name = entry.file_name();
            let Some(generation) = name
                .to_str()
                .and_then(|name| name.strip_prefix("manifest-"))
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            let keys = manifest_reference_keys(generation, &fs::read(entry.path())?)?;
            result.insert(generation.to_owned(), keys);
        }
        Ok(result)
    }

    /// Remove generation files only after checking both durable publication and
    /// liveness. A dead assembler can leave a derived file before any manifest.
    pub fn sweep_generations(&self) -> Result<usize> {
        self.sweep_generations_with_bytes()
            .map(|removed| removed.removed_generations)
    }

    pub(crate) fn sweep_generations_with_bytes(&self) -> Result<GenerationSweep> {
        if let Some(registry) = self.generation_sweep_registry()? {
            // V2 pin admission takes the registry before the view pointer.
            // Keep that order and hold the barrier through protection checks
            // and unlink, just as the family collector does.
            return registry
                .with_barrier(|tx| {
                    let scope = self
                        .view_dir()
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    let registered: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM members WHERE scope = ?1)",
                        [scope.as_ref()],
                        |row| row.get(0),
                    )?;
                    if !registered {
                        return Ok(GenerationSweep::default());
                    }
                    self.sweep_generations_locked()
                        .map_err(super::registry::RegistryError::View)
                })
                .map_err(|error| ViewError::Io(std::io::Error::other(error.to_string())));
        }
        self.sweep_generations_bounded(false, GENERATION_DIRECTORY_LIMIT)
    }

    fn sweep_generations_locked(&self) -> Result<GenerationSweep> {
        self.sweep_generations_bounded(true, GENERATION_DIRECTORY_LIMIT)
    }

    fn generation_sweep_registry(&self) -> Result<Option<super::registry::FamilyRegistry>> {
        if let Some(registry) = &self.sweep_registry {
            return Ok(Some(registry.clone()));
        }
        let Some(v2) = self
            .view_dir()
            .parent()
            .filter(|parent| parent.file_name().is_some_and(|name| name == "v2"))
        else {
            return Ok(None);
        };
        let views = v2
            .parent()
            .filter(|parent| parent.file_name().is_some_and(|name| name == "views"))
            .ok_or_else(|| ViewError::InvalidManifest("V2 view has no views root".into()))?;
        let storage = views
            .parent()
            .ok_or_else(|| ViewError::InvalidManifest("V2 view has no storage root".into()))?;
        // Registered publishers and the retention lane carry their registry.
        // Resolve only raw V2 stores here; never sweep an unregistered V2 view
        // using V1's unrelated retention barrier.
        for entry in
            generation_directory_entries(&storage.join("blobs/v2"), GENERATION_DIRECTORY_LIMIT)?
        {
            if !entry.file_type()?.is_dir()
                || !entry.path().join(super::registry::REGISTRY_FILE).is_file()
            {
                continue;
            }
            let family = entry.file_name().to_string_lossy().into_owned();
            let scope = self
                .view_dir()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let found = (|| -> super::registry::RegistryResult<_> {
                let Some(registry) =
                    super::registry::FamilyRegistry::open_existing(storage, &family)?
                else {
                    return Ok(None);
                };
                let member = registry.member(&scope)?;
                Ok(member
                    .filter(|member| member.view_dir == self.view_dir())
                    .map(|_| registry))
            })()
            .map_err(|error| ViewError::Io(std::io::Error::other(error.to_string())))?;
            if found.is_some() {
                return Ok(found);
            }
        }
        Err(ViewError::InvalidManifest(format!(
            "V2 view has no registry: {}",
            self.view_dir().display()
        )))
    }

    fn sweep_generations_bounded(
        &self,
        admission_locked: bool,
        limit: usize,
    ) -> Result<GenerationSweep> {
        // A publisher cannot add a reference after the ownership snapshot and
        // release its base pin before the sweep checks that pin.
        let mut pointer = self.open_pointer_connection()?;
        let ownership =
            pointer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: String = ownership.query_row(
            "SELECT generation FROM pointer WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let current = (!current.is_empty()).then_some(current);
        let mut generations = std::collections::BTreeSet::new();
        let mut derived_owners = std::collections::BTreeSet::new();
        let mut temporaries: HashMap<String, Vec<PathBuf>> = HashMap::new();
        // A partial ownership snapshot could omit a reference to a shared
        // derived file. Fail closed at the iterator bound, before any deletion.
        let entries = generation_directory_entries(self.view_dir(), limit)?;
        for entry in entries {
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
                if name.starts_with(".manifest-") {
                    temporaries
                        .entry(generation.to_owned())
                        .or_default()
                        .push(entry.path());
                }
                generations.insert(generation.to_owned());
            }
        }
        let mut removed = GenerationSweep::default();
        for generation in generations {
            // Even an obsolete reference protects its owner until the next sweep.
            // This keeps shared files alive without depending on directory order.
            if current.as_deref() == Some(&generation) {
                continue;
            }
            // Directory enumeration and ownership reads do not exclude query
            // admission. Only the protection recheck and this generation's
            // unlink need to serialize with pins. The pointer transaction still
            // excludes publication of a new reference to a derived owner.
            let _barrier = if admission_locked {
                None
            } else {
                crate::storage_retention::pin_barrier(self.view_dir())?
            };
            let (metadata_path, keys_path) = crate::pins::pin_paths(self.view_dir(), &generation);
            if metadata_path.exists() {
                let Ok(bytes) = fs::read(&metadata_path) else {
                    continue;
                };
                let Ok(metadata) = serde_json::from_slice::<crate::pins::PinMetadata>(&bytes)
                else {
                    continue;
                };
                if crate::pins::owner_is_live(&metadata.owner) {
                    continue;
                }
                removed.removed_bytes += unlink_generation_file(&metadata_path).unwrap_or_default();
                removed.removed_bytes += unlink_generation_file(&keys_path).unwrap_or_default();
            }
            if crate::root_cache::sweep_read_markers(self.view_dir(), &generation).protected {
                continue;
            }
            #[cfg(test)]
            crate::storage_retention::test_hook(self.view_dir(), "generation-delete");
            // The pointer write transaction still excludes publication here.
            if let Some(paths) = temporaries.remove(&generation) {
                for path in paths {
                    removed.removed_bytes += unlink_generation_file(&path).unwrap_or_default();
                }
            }
            removed.add(self.remove_generation_files_counted(
                &generation,
                derived_owners.contains(&generation),
            ));
        }
        Ok(removed)
    }

    pub(super) fn remove_generation_files(&self, generation: &str) {
        let temporary_prefix = format!(".manifest-{generation}.json.tmp.");
        if let Ok(entries) = fs::read_dir(self.view_dir()) {
            for entry in entries.take(GENERATION_DIRECTORY_LIMIT).flatten() {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(&temporary_prefix))
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        self.try_remove_generation_files(generation);
    }

    fn try_remove_generation_files(&self, generation: &str) -> bool {
        self.try_remove_generation_files_policy(generation, false)
    }

    fn try_remove_generation_files_policy(&self, generation: &str, keep_derived: bool) -> bool {
        self.remove_generation_files_counted(generation, keep_derived)
            .removed_generations
            > 0
    }

    fn remove_generation_files_counted(
        &self,
        generation: &str,
        keep_derived: bool,
    ) -> GenerationSweep {
        if super::validate_generation(generation).is_err() {
            return GenerationSweep::default();
        }
        // The identity registry enforces (rather than merely logs) the same
        // no-unlink-while-open boundary as callgraph generation deletion.
        let _files = crate::db::file_identity::filesystem_guard();
        if !keep_derived
            && crate::db::file_identity::open_connections(
                &self.view_dir().join(format!("derived-{generation}.sqlite")),
            ) != 0
        {
            return GenerationSweep::default();
        }
        let mut removed = false;
        let mut removed_bytes = 0;
        for path in [
            Ok(self.view_dir().join(format!("derived-{generation}.sqlite"))),
            Ok(self.view_dir().join(format!("derived-{generation}.ref"))),
            self.trigram_path(generation),
            self.manifest_path(generation),
        ]
        .into_iter()
        .flatten()
        {
            if keep_derived
                && (path
                    .extension()
                    .is_some_and(|extension| extension == "sqlite")
                    || self
                        .manifest_path(generation)
                        .is_ok_and(|manifest| path == manifest))
            {
                // The referenced database's manifest is the incremental clone
                // baseline. Its trigram is not referenced and can be reclaimed.
                continue;
            }
            let suffixes = if path
                .extension()
                .is_some_and(|extension| extension == "sqlite")
            {
                &["", "-wal", "-shm"][..]
            } else {
                &[""][..]
            };
            for suffix in suffixes {
                let mut name = path.as_os_str().to_owned();
                name.push(suffix);
                if let Some(bytes) = unlink_generation_file(&PathBuf::from(name)) {
                    removed = true;
                    removed_bytes += bytes;
                }
            }
        }
        GenerationSweep {
            removed_generations: usize::from(removed),
            removed_bytes,
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
#[cfg(test)]
static CHECKPOINT_SETTLED: std::sync::Condvar = std::sync::Condvar::new();

#[cfg(test)]
#[derive(Default)]
pub(crate) struct CheckpointTestHooks {
    pub before_settled: Option<Box<dyn FnOnce(&Path) + Send>>,
    pub before_wait: Option<Box<dyn FnOnce() + Send>>,
}

#[cfg(test)]
static CHECKPOINT_TEST_HOOKS: OnceLock<Mutex<HashMap<PathBuf, CheckpointTestHooks>>> =
    OnceLock::new();

#[cfg(test)]
pub(crate) fn checkpoint_test_hooks() -> &'static Mutex<HashMap<PathBuf, CheckpointTestHooks>> {
    CHECKPOINT_TEST_HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn wait_for_derived_checkpoint_for_test(view_dir: &Path) {
    let before_wait = checkpoint_test_hooks()
        .lock()
        .unwrap()
        .get_mut(view_dir)
        .and_then(|hooks| hooks.before_wait.take());
    if let Some(before_wait) = before_wait {
        before_wait();
    }
    let jobs = DEFERRED_CHECKPOINTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (jobs, _) = CHECKPOINT_SETTLED
        .wait_timeout_while(jobs, Duration::from_secs(60), |jobs| {
            jobs.contains_key(view_dir)
        })
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        !jobs.contains_key(view_dir),
        "derived checkpoint did not settle"
    );
}

const DEFERRED_CHECKPOINT_IDLE_DELAY: Duration = Duration::from_millis(250);
const DERIVED_CHECKPOINT_SEAM: &str = "views::generation::checkpoint_derived";
const DERIVED_CHECKPOINT_REASON: &str =
    "a successful TRUNCATE checkpoint returns zero frame counts and the raw connection has no earlier WAL-hook baseline";

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
        .note_unmeasurable(
            DERIVED_CHECKPOINT_SEAM,
            DERIVED_CHECKPOINT_REASON,
            None,
            None,
        );
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
    cleanup: Option<(ViewStore, Arc<AtomicBool>)>,
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
            #[cfg(test)]
            if let Ok(delay) = std::env::var("AFT_TEST_TIMING_DELAY_MS") {
                std::thread::sleep(Duration::from_millis(
                    delay.parse().expect("checkpoint delay"),
                ));
            }
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
            // A publication sweep may have found this superseded keeper still
            // open. Retry after closing it; no SQLite descriptor is opened to
            // inspect the old derived database.
            if let Some((view, ready)) = cleanup {
                // Settlement arms cleanup only after releasing the publisher's
                // locks and pins. If the keeper closed before it was armed,
                // settlement's own sweep sees that close instead.
                if ready.load(Ordering::Acquire) {
                    schedule_generation_sweep(view);
                }
            }
            #[cfg(test)]
            {
                let before_settled = checkpoint_test_hooks()
                    .lock()
                    .unwrap()
                    .get_mut(&key)
                    .and_then(|hooks| hooks.before_settled.take());
                if let Some(before_settled) = before_settled {
                    before_settled(&path);
                }
            }
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
            #[cfg(test)]
            CHECKPOINT_SETTLED.notify_all();
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

/// How [`clone_derived`] produced its copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloneMechanism {
    /// An APFS copy-on-write clone of a quiescent source: no page is rewritten.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Clonefile,
    /// A page-by-page SQLite backup, which writes the whole database again.
    Backup(BackupCopy),
}

impl CloneMechanism {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Clonefile => "clonefile",
            Self::Backup(_) => "sqlite_backup",
        }
    }
}

/// Copy a published derived database to an unpublished destination path.
///
/// On macOS a quiescent source is cloned copy-on-write (see
/// [`clone_quiescent`]), so a seeded checkout does not rewrite a database it
/// shares byte for byte with its donor. Everywhere else, and whenever the
/// source cannot be proven quiescent, the copy goes through the SQLite backup
/// API. Neither path opens a raw descriptor on the source's file set.
pub(super) fn clone_derived(source: &Path, destination: &Path) -> Result<CloneMechanism> {
    #[cfg(target_os = "macos")]
    if !FORCE_BACKUP_CLONE_FOR_TEST.load(Ordering::Relaxed) {
        let started = Instant::now();
        match clone_quiescent(source, destination)? {
            Ok(()) => {
                log::info!(
                    "view derived clone mechanism=clonefile ms={} source={} destination={}",
                    started.elapsed().as_millis(),
                    source.display(),
                    destination.display()
                );
                return Ok(CloneMechanism::Clonefile);
            }
            Err(reason) => log::info!(
                "view derived clone fallback=sqlite_backup reason={reason} source={}",
                source.display()
            ),
        }
    }
    clone_derived_backup(source, destination).map(CloneMechanism::Backup)
}

/// Test-only switch that sends [`clone_derived`] down the backup path, so a
/// measurement can compare both mechanisms in one process.
#[cfg(target_os = "macos")]
pub(crate) static FORCE_BACKUP_CLONE_FOR_TEST: AtomicBool = AtomicBool::new(false);

/// How long the quiescence checks may wait on a lock before the copy falls
/// back to the backup API, which has its own bounded contention handling.
#[cfg(target_os = "macos")]
const QUIESCENT_BUSY_TIMEOUT: Duration = Duration::from_secs(1);

/// Clone `source` with `clonefile(2)` while SQLite proves its main file is
/// the whole database and cannot change.
///
/// `clonefile` takes paths, so this code never opens (and later closes) a
/// raw descriptor of its own on the source's main file, `-wal` or `-shm`;
/// only SQLite's connection touches them, and SQLite manages its descriptors.
/// Closing a raw descriptor would drop every POSIX lock this process's SQLite
/// connections hold on that file. Every step below that touches the source
/// goes through a SQLite connection; the `-wal` is only `stat`ed.
///
/// 1. The source must be in WAL mode. A non-empty `-wal` is first
///    checkpointed with TRUNCATE through the connection.
/// 2. `BEGIN IMMEDIATE` takes the WAL write lock, which excludes writers and
///    WAL-resetting checkpoints in every process.
/// 3. Under that lock the `-wal` must be empty: with no frames, the main file
///    is the complete database and nothing can write to it while the lock is
///    held, so the clone is a consistent snapshot.
///
/// The outer error is fatal for the publication (a live connection on the
/// destination). The inner error is the reason the caller should fall back to
/// the backup copy instead.
#[cfg(target_os = "macos")]
fn clone_quiescent(source: &Path, destination: &Path) -> Result<std::result::Result<(), String>> {
    let fallback = |step: &str, error: &dyn std::fmt::Display| Ok(Err(format!("{step}: {error}")));
    let wal_bytes = || {
        let mut wal = source.as_os_str().to_owned();
        wal.push("-wal");
        match fs::metadata(PathBuf::from(wal)) {
            Ok(metadata) => Ok(metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error),
        }
    };
    let mut connection = match crate::db::file_identity::IdentityConnection::open_with_flags(
        source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        "views::generation::clone_quiescent source",
    ) {
        Ok(connection) => connection,
        Err(error) => return fallback("open", &error),
    };
    if let Err(error) = connection.busy_timeout(QUIESCENT_BUSY_TIMEOUT) {
        return fallback("busy_timeout", &error);
    }
    match connection.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0)) {
        Ok(mode) if mode.eq_ignore_ascii_case("wal") => {}
        Ok(mode) => return fallback("journal_mode", &mode),
        Err(error) => return fallback("journal_mode", &error),
    }
    match wal_bytes() {
        Ok(0) => {}
        Ok(_) => {
            if let Err(error) = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                row.get::<_, i64>(0)
            }) {
                return fallback("checkpoint", &error);
            }
        }
        Err(error) => return fallback("wal_stat", &error),
    }
    let transaction =
        match connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate) {
            Ok(transaction) => transaction,
            Err(error) => return fallback("begin_immediate", &error),
        };
    match wal_bytes() {
        Ok(0) => {}
        Ok(bytes) => return fallback("wal_not_empty", &format!("{bytes} bytes")),
        Err(error) => return fallback("wal_stat", &error),
    }
    {
        let _files = crate::db::file_identity::filesystem_guard();
        remove_unpublished_derived_locked(destination)?;
        if let Err(error) = clonefile(source, destination) {
            return fallback("clonefile", &error);
        }
        // The clone is a new, unpublished file that no connection anywhere has
        // opened: the registry reported none at this path under the guard,
        // and no other process knows the unpublished generation's name. So a
        // plain descriptor may flush it. SQLite's own publication checkpoint
        // would not: with an empty WAL it has no frames to copy and syncs
        // nothing.
        let synced = fs::File::open(destination).and_then(|file| file.sync_all());
        if let Err(error) = synced {
            remove_unpublished_derived_locked(destination)?;
            return fallback("sync", &error);
        }
    }
    super::sync_parent(destination)?;
    drop(transaction);
    Ok(Ok(()))
}

#[cfg(target_os = "macos")]
fn clonefile(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source.as_os_str().as_bytes())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    // `CLONE_NOFOLLOW` from <sys/clonefile.h>; the libc crate does not export
    // it. It refuses a symlinked source instead of cloning its target.
    const CLONE_NOFOLLOW: u32 = 0x0001;
    // Both C strings outlive the call.
    if unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), CLONE_NOFOLLOW) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Copy a coherent SQLite snapshot without opening the source file directly.
/// The backup API includes committed WAL content while preserving every lock
/// SQLite holds for other live connections in this process.
pub(super) fn clone_derived_backup(source: &Path, destination: &Path) -> Result<BackupCopy> {
    let started = Instant::now();
    let source_connection = crate::db::file_identity::IdentityConnection::open(
        source,
        "views::generation::clone_derived source",
    )?;
    source_connection.busy_timeout(Duration::from_secs(5))?;

    let mut destination_connection = {
        let _files = crate::db::file_identity::filesystem_guard();
        remove_unpublished_derived_locked(destination)?;
        crate::db::file_identity::IdentityConnection::open(
            destination,
            "views::generation::clone_derived destination",
        )?
    };
    destination_connection.busy_timeout(Duration::from_secs(5))?;
    let backup = rusqlite::backup::Backup::new(&source_connection, &mut destination_connection)?;
    let copy = copy_all_pages(&backup)?;
    drop(backup);

    log::info!(
        "view derived clone mechanism=sqlite_backup ms={} steps={} busy_retries={} source={} destination={}",
        started.elapsed().as_millis(),
        copy.steps,
        copy.busy_retries,
        source.display(),
        destination.display()
    );
    Ok(copy)
}

#[cfg(test)]
pub(crate) fn clone_derived_for_test(source: &Path, destination: &Path) -> Result<()> {
    clone_derived(source, destination).map(|_| ())
}

/// Delete an unpublished derived file set (main file, `-wal`, `-shm`) so the
/// caller can build a fresh database at the same path. Refuses while this
/// process has any SQLite connection on the path: deleting a file set under a
/// live connection would detach it from its locks and shared-memory index.
pub(super) fn remove_unpublished_derived(path: &Path) -> Result<()> {
    let _files = crate::db::file_identity::filesystem_guard();
    remove_unpublished_derived_locked(path)
}

/// [`remove_unpublished_derived`] for callers that already hold the
/// file-set mutation guard.
fn remove_unpublished_derived_locked(path: &Path) -> Result<()> {
    let open = crate::db::file_identity::open_connections(path);
    if open != 0 {
        return Err(ViewError::InvalidManifest(format!(
            "derived clone destination has {open} live SQLite connection(s): {}",
            path.display()
        )));
    }
    for suffix in ["", "-wal", "-shm"] {
        let mut candidate = path.as_os_str().to_owned();
        candidate.push(suffix);
        let candidate = PathBuf::from(candidate);
        match fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ViewError::io_at("removing", &candidate, error)),
        }
    }
    Ok(())
}

/// How a backup reached completion, for the clone log line and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BackupCopy {
    pub steps: u32,
    pub busy_retries: u32,
}

const BACKUP_BUSY_RETRY_PAUSE: Duration = Duration::from_millis(5);
const BACKUP_BUSY_DEADLINE: Duration = Duration::from_secs(5);

/// Copy every page in one `sqlite3_backup_step(-1)` call.
///
/// The source is a published generation. Nothing writes its logical content
/// again; only checkpoints touch it. Stepping in small batches with a pause
/// only helps when other connections need to write the source between
/// steps, and here there are none. In WAL mode the backup's read lock does
/// not block readers. The only thing that waits on it is a TRUNCATE
/// checkpoint of the source, which waits under its own 5 s busy timeout.
/// One step holds that lock for roughly 0.3 s per 257 MiB. The previous
/// setting (256 pages per step, 5 ms sleep between steps) spent about 1.25 s
/// of every 1.8 s copy sleeping. One step also copies from a single read
/// snapshot, so the backup never has to restart because the source changed.
///
/// `SQLITE_BUSY` or `SQLITE_LOCKED` leaves the copy position unchanged, so
/// retrying the same all-pages step after a short pause is correct. The
/// retries are bounded so that a stuck lock defers the clone as typed SQLite
/// contention, rather than declaring a valid published manifest corrupt.
pub(super) fn copy_all_pages(backup: &rusqlite::backup::Backup<'_, '_>) -> Result<BackupCopy> {
    use rusqlite::backup::StepResult;
    let started = Instant::now();
    let mut copy = BackupCopy {
        steps: 0,
        busy_retries: 0,
    };
    loop {
        copy.steps += 1;
        match backup.step(-1)? {
            StepResult::Done => return Ok(copy),
            // A negative page count asks for every remaining page, so SQLite
            // should not report partial progress. Continue without sleeping
            // if it ever does.
            StepResult::More => {}
            contention @ (StepResult::Busy | StepResult::Locked) => {
                copy.busy_retries += 1;
                if started.elapsed() >= BACKUP_BUSY_DEADLINE {
                    let code = match contention {
                        StepResult::Locked => rusqlite::ffi::SQLITE_LOCKED,
                        _ => rusqlite::ffi::SQLITE_BUSY,
                    };
                    return Err(ViewError::Sqlite(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(code),
                        Some(format!(
                            "derived clone stayed busy for {} ms after {} attempt(s)",
                            started.elapsed().as_millis(),
                            copy.steps
                        )),
                    )));
                }
                std::thread::sleep(BACKUP_BUSY_RETRY_PAUSE);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_scan_rejects_non_ascii_keys_without_panicking() {
        let storage = tempfile::tempdir().unwrap();
        let view = ViewStore::open(storage.path(), "bad-key").unwrap();
        let manifest = crate::views::Manifest::new([(
            crate::views::RelPath::new(b"a.rs".to_vec()).unwrap(),
            crate::views::ManifestEntry::Regular {
                mode: 0o100644,
                resolution_input: false,
                planes: crate::views::RegularPlanes {
                    semantic: None,
                    callgraph: Some(format!("aé{}", "0".repeat(61))),
                },
            },
        )])
        .unwrap();
        fs::write(
            view.manifest_path("bad").unwrap(),
            manifest.to_json_bytes().unwrap(),
        )
        .unwrap();
        assert!(view.blob_references_by_generation().unwrap()["bad"].is_empty());
    }

    fn reference_fixture(view: &ViewStore, generation: &str) {
        let manifest = crate::views::Manifest::new([(
            crate::views::RelPath::new(b"a.rs".to_vec()).unwrap(),
            crate::views::ManifestEntry::Regular {
                mode: 0o100644,
                resolution_input: false,
                planes: crate::views::RegularPlanes {
                    semantic: None,
                    callgraph: Some("11".repeat(32)),
                },
            },
        )])
        .unwrap();
        fs::write(
            view.manifest_path(generation).unwrap(),
            manifest.to_json_bytes().unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn reference_scan_accepts_a_complete_directory_above_the_previous_bound() {
        let storage = tempfile::tempdir().unwrap();
        let view = ViewStore::open(storage.path(), "many-generations").unwrap();
        for index in 0..4097 {
            fs::write(view.view_dir().join(format!("retained-{index}")), []).unwrap();
        }
        reference_fixture(&view, "retained");
        assert_eq!(
            view.blob_references_by_generation().unwrap()["retained"],
            std::collections::BTreeSet::from([[0x11; 32]])
        );
    }

    #[test]
    fn reference_scan_extracts_keys_without_deserializing_membership() {
        let storage = tempfile::tempdir().unwrap();
        let view = ViewStore::open(storage.path(), "keys-only").unwrap();
        reference_fixture(&view, "keys");
        crate::views::take_manifest_decodes();
        assert_eq!(
            view.blob_references_by_generation().unwrap()["keys"],
            std::collections::BTreeSet::from([[0x11; 32]])
        );
        assert_eq!(crate::views::take_manifest_decodes(), 0);
    }

    #[test]
    fn generation_scan_refuses_an_incomplete_census_before_unlink() {
        let storage = tempfile::tempdir().unwrap();
        let view = ViewStore::open(storage.path(), "incomplete-census").unwrap();
        let artifact = view.trigram_path("unpublished").unwrap();
        fs::write(&artifact, []).unwrap();
        fs::write(view.view_dir().join("another-entry"), []).unwrap();
        assert!(view.sweep_generations_bounded(false, 1).is_err());
        assert!(artifact.exists());
    }

    #[test]
    fn reference_scan_preserves_v2_content_fencing_and_noncanonical_compatibility() {
        use crate::views::manifest_v2::{
            EntryPlanes, EntryV2, GenerationName, ManifestHeader, ManifestV2, Producers,
        };
        use crate::views::readiness::PlaneState;
        let storage = tempfile::tempdir().unwrap();
        let view = ViewStore::open(storage.path(), "v2-reference-fence").unwrap();
        let mut manifest = ManifestV2::new(ManifestHeader {
            producers: Producers {
                trigram: "t".into(),
                semantic: Some("s".into()),
                callgraph: "c".into(),
            },
            head_tree: None,
            ignore_fingerprint: None,
            segment: None,
        });
        manifest
            .insert(
                crate::views::RelPath::new(b"a.rs".to_vec()).unwrap(),
                EntryV2::regular(
                    crate::blob_store::v2::ContentHash::of(b"a"),
                    1,
                    EntryPlanes {
                        semantic: Some(PlaneState::Ready {
                            key: "22".repeat(32),
                        }),
                        trigram: None,
                        callgraph: None,
                    },
                ),
            )
            .unwrap();
        let name = GenerationName::for_manifest(&manifest).unwrap().to_string();
        let path = view.manifest_path(&name).unwrap();
        let bytes = manifest.to_json_bytes().unwrap();
        fs::write(&path, &bytes).unwrap();
        let expected = std::collections::BTreeSet::from([[0x22; 32]]);
        assert_eq!(
            view.blob_references_by_generation().unwrap()[&name],
            expected
        );
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        fs::write(&path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
        assert_eq!(
            view.blob_references_by_generation().unwrap()[&name],
            expected
        );
        json["entries"][0]["planes"]["semantic"]["key"] = "33".repeat(32).into();
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(view.blob_references_by_generation().is_err());
    }

    #[test]
    fn reference_scan_refuses_unknown_formats_instead_of_ignoring_their_keys() {
        let storage = tempfile::tempdir().unwrap();
        let view = ViewStore::open(storage.path(), "future-references").unwrap();
        fs::write(
            view.manifest_path("future").unwrap(),
            format!(
                r#"{{"format_version":255,"path_identity_version":{},"entries":[]}}"#,
                crate::views::PATH_IDENTITY_VERSION,
            ),
        )
        .unwrap();
        assert!(view.blob_references_by_generation().is_err());
    }

    #[test]
    fn non_sqlite_artifacts_do_not_have_sqlite_sidecars_removed() {
        let storage = tempfile::tempdir().unwrap();
        let view = ViewStore::open(storage.path(), "sidecars").unwrap();
        let trigram = view.trigram_path("old").unwrap();
        let unrelated = view.view_dir().join("trigram-old.bin-wal");
        fs::write(&trigram, []).unwrap();
        fs::write(&unrelated, b"not a SQLite sidecar").unwrap();
        assert!(view.try_remove_generation_files_policy("old", false));
        assert!(!trigram.exists());
        assert!(unrelated.exists());
    }

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

        // The worker cannot checkpoint until publication has returned and the
        // pending-WAL assertion below has observed the still-live generation.
        let checkpoint = checkpoint_lock(&source);
        let checkpoint_guard = checkpoint.lock().unwrap();
        schedule_derived_checkpoint(
            source.clone(),
            crate::db::file_identity::IdentityConnection::new(connection, "checkpoint test"),
            source.parent().unwrap().to_path_buf(),
            None,
        );

        assert!(
            fs::metadata(&wal).is_ok_and(|metadata| metadata.len() > 0),
            "checkpoint ran synchronously on the publication path"
        );
        drop(checkpoint_guard);
        wait_for_derived_checkpoint_for_test(directory.path());
        assert!(
            !fs::metadata(&wal).is_ok_and(|metadata| metadata.len() > 0),
            "detached derived checkpoint did not truncate the WAL"
        );
        assert_eq!(
            crate::write_ledger::pending_for_test(
                crate::write_ledger::Domain::ViewsDerived,
                &source.parent().unwrap().display().to_string(),
            )
            .1,
            0,
            "an unknowable TRUNCATE quantity must not receive guessed credit"
        );
        let ledger_root = source.parent().unwrap().display().to_string();
        assert!(crate::write_ledger::unmeasurable_for_test(&ledger_root)
            .iter()
            .any(|entry| entry.seam == DERIVED_CHECKPOINT_SEAM
                && entry.reason == DERIVED_CHECKPOINT_REASON));
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

        clone_derived_backup(&source, &destination).unwrap();

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
    fn clone_closed_path_status_store_keeps_committed_wal() {
        let directory = tempfile::tempdir().unwrap();
        let view = directory.path().join("view");
        let source = view.join("derived.sqlite");
        let destination = directory.path().join("cloned.sqlite");
        {
            let mut status = crate::path_status::PathStatusStore::open(&view).unwrap();
            status.mark_pending(b"first.rs", "waiting", 1).unwrap();
            status
                .mark_failed(b"last.rs", "last committed write", 2)
                .unwrap();
        }
        assert_eq!(crate::db::file_identity::open_connections(&source), 0);
        assert!(fs::metadata(view.join("derived.sqlite-wal")).unwrap().len() > 0);
        clone_derived_for_test(&source, &destination).unwrap();
        let copied = crate::path_status::PathStatusStore::open_at(&destination).unwrap();
        let last = copied
            .status_for(b"last.rs")
            .unwrap()
            .expect("last committed path survived clone");
        assert_eq!(last.state, crate::path_status::PathState::Failed);
        assert_eq!(last.reason, "last committed write");
        assert_eq!(last.since_generation, 2);
        assert_eq!(copied.summary().unwrap().pending_count, 1);
    }

    #[test]
    fn clone_busy_donor_exhaustion_remains_building() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let destination = directory.path().join("destination.sqlite");
        let writer =
            crate::db::file_identity::IdentityConnection::open(&source, "busy donor test").unwrap();
        writer
            .execute_batch("CREATE TABLE state (value TEXT); BEGIN EXCLUSIVE;")
            .unwrap();

        // An exclusive donor lock must exhaust the real backup step, not a
        // readiness probe or an injected error before the copy is attempted.
        let error = clone_derived(&source, &destination).unwrap_err();
        writer.execute_batch("ROLLBACK;").unwrap();
        let ViewError::Sqlite(error) = error else {
            panic!("backup contention lost its SQLite error type: {error:?}");
        };
        assert!(crate::db::is_busy_error(&error));
        let response = crate::commands::callgraph_store_adapter::store_error_response(
            "busy-donor",
            "clone",
            crate::callgraph_store::CallGraphStoreError::Sqlite(error),
        );
        assert_eq!(response.data["code"], "callgraph_building");

        let CloneMechanism::Backup(copy) = clone_derived(&source, &destination).unwrap() else {
            panic!("a rollback-journal donor cannot be proven quiescent for clonefile");
        };
        assert_eq!(copy.steps, 1);
        assert_eq!(copy.busy_retries, 0);
    }

    #[test]
    fn clone_copies_a_multi_batch_database_in_one_backup_step() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let destination = directory.path().join("destination.sqlite");
        let connection = crate::db::file_identity::IdentityConnection::open(
            &source,
            "views generation one-step clone test source",
        )
        .unwrap();
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE payload (value BLOB NOT NULL);\
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)\
                 INSERT INTO payload SELECT zeroblob(4096) FROM n;",
            )
            .unwrap();
        let pages: i64 = connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        // More pages than the old 256-page batch, so a stepped copy would
        // need several steps.
        assert!(pages > 1_000, "fixture too small: {pages} pages");

        let copy = clone_derived_backup(&source, &destination).unwrap();

        assert_eq!(
            copy,
            BackupCopy {
                steps: 1,
                busy_retries: 0
            }
        );
        assert_eq!(
            Connection::open(&destination)
                .unwrap()
                .query_row("SELECT COUNT(*) FROM payload", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2000
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

    #[cfg(unix)]
    /// A WAL-mode donor with committed rows, like a published derived file.
    fn wal_donor(path: &Path) -> crate::db::file_identity::IdentityConnection {
        let connection =
            crate::db::file_identity::IdentityConnection::open(path, "views generation donor")
                .unwrap();
        connection
            .pragma_update(None, "journal_mode", "WAL")
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
        connection
    }

    #[cfg(target_os = "macos")]
    fn wal_len(path: &Path) -> u64 {
        fs::metadata(format!("{}-wal", path.display())).map_or(0, |metadata| metadata.len())
    }

    #[cfg(unix)]
    fn state(path: &Path) -> String {
        Connection::open(path)
            .unwrap()
            .query_row("SELECT value FROM state", [], |row| row.get(0))
            .unwrap()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn clone_of_a_quiescent_wal_donor_is_copy_on_write() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let destination = directory.path().join("destination.sqlite");
        let donor = wal_donor(&source);
        assert!(wal_len(&source) > 0, "fixture must start with WAL frames");

        assert_eq!(
            clone_derived(&source, &destination).unwrap(),
            CloneMechanism::Clonefile
        );

        // The frames were checkpointed into the main file through SQLite
        // before the clone, so the clone holds them without a WAL of its own.
        assert_eq!(wal_len(&source), 0);
        assert!(!PathBuf::from(format!("{}-wal", destination.display())).exists());
        assert_eq!(state(&destination), "committed-in-wal");
        assert_eq!(crate::db::file_identity::open_connections(&source), 1);
        donor
            .query_row("SELECT COUNT(*) FROM state", [], |row| row.get::<_, i64>(0))
            .unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn clone_falls_back_to_backup_while_a_reader_keeps_wal_frames() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let destination = directory.path().join("destination.sqlite");
        let _donor = wal_donor(&source);
        // A reader whose snapshot includes the frames stops TRUNCATE from
        // emptying the WAL, so the main file alone is not the database.
        let reader = Connection::open(&source).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM state;")
            .unwrap();

        let mechanism = clone_derived(&source, &destination).unwrap();

        assert!(
            matches!(mechanism, CloneMechanism::Backup(_)),
            "a donor with WAL frames must not be cloned: {mechanism:?}"
        );
        assert!(wal_len(&source) > 0);
        assert_eq!(state(&destination), "committed-in-wal");
        reader.execute_batch("COMMIT;").unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn copy_on_write_clone_refuses_to_replace_a_live_destination() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let destination = directory.path().join("destination.sqlite");
        let _donor = wal_donor(&source);
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

    #[cfg(unix)]
    const LOCK_PROBE_ENV: &str = "AFT_TEST_CLONE_LOCK_PROBE_DATABASE";

    /// Report, from another process, whether some process holds SQLite's
    /// shared lock on `database` and the read lock every connection keeps on
    /// its `-shm` (SQLite's dead-man-switch byte).
    /// POSIX locks never conflict within one process, so only a second
    /// process can see whether this process's locks are still in place.
    #[cfg(unix)]
    fn probe_sqlite_locks(database: &Path) -> String {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "views::generation::tests::clone_lock_probe_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(LOCK_PROBE_ENV, database)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        stdout
            .lines()
            // libtest prints the test name on the same line before the output.
            .find_map(|line| line.split_once("LOCK_PROBE ").map(|(_, result)| result))
            .unwrap_or_else(|| panic!("probe printed no result: {stdout}"))
            .to_owned()
    }

    /// Child half of [`probe_sqlite_locks`]; a no-op in an ordinary run.
    #[cfg(unix)]
    #[test]
    fn clone_lock_probe_child() {
        let Some(database) = std::env::var_os(LOCK_PROBE_ENV) else {
            return;
        };
        let database = PathBuf::from(database);
        // SQLite's unix VFS takes a read lock on this byte range of the main
        // file for SHARED (PENDING_BYTE + 2, 510 bytes), and every connection
        // with the WAL index mapped holds a read lock on byte 128 of `-shm`.
        let held = |path: PathBuf, start: i64, len: i64| {
            use std::os::fd::AsRawFd;
            let file = fs::File::open(&path).unwrap();
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = libc::F_WRLCK as _;
            lock.l_whence = libc::SEEK_SET as _;
            lock.l_start = start as _;
            lock.l_len = len as _;
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) },
                0
            );
            if i64::from(lock.l_type) == i64::from(libc::F_UNLCK) {
                "free"
            } else {
                "held"
            }
        };
        let shared = held(database.clone(), 0x4000_0002, 510);
        let dms = held(PathBuf::from(format!("{}-shm", database.display())), 128, 1);
        println!("LOCK_PROBE db_shared={shared} shm_dms={dms}");
    }

    #[cfg(unix)]
    #[test]
    fn clone_keeps_this_process_sqlite_locks_on_a_live_donor() {
        let directory = tempfile::tempdir().unwrap();

        // Positive control: closing a raw descriptor on a live database drops
        // this process's locks, and the probe sees that. Without it, a probe
        // that always answered "held" would pass the assertion below.
        let control = directory.path().join("control.sqlite");
        let control_connection = wal_donor(&control);
        control_connection
            .execute_batch("BEGIN; SELECT COUNT(*) FROM state;")
            .unwrap();
        assert_eq!(probe_sqlite_locks(&control), "db_shared=held shm_dms=held");
        drop(fs::File::open(&control).unwrap());
        drop(fs::File::open(format!("{}-shm", control.display())).unwrap());
        assert_eq!(probe_sqlite_locks(&control), "db_shared=free shm_dms=free");

        let source = directory.path().join("source.sqlite");
        let destination = directory.path().join("destination.sqlite");
        let donor = wal_donor(&source);
        donor
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        donor
            .execute_batch("BEGIN; SELECT COUNT(*) FROM state;")
            .unwrap();
        assert_eq!(probe_sqlite_locks(&source), "db_shared=held shm_dms=held");

        let mechanism = clone_derived(&source, &destination).unwrap();

        #[cfg(target_os = "macos")]
        assert_eq!(mechanism, CloneMechanism::Clonefile);
        #[cfg(not(target_os = "macos"))]
        assert!(matches!(mechanism, CloneMechanism::Backup(_)));
        assert_eq!(
            probe_sqlite_locks(&source),
            "db_shared=held shm_dms=held",
            "the copy dropped a lock this process's donor connection holds"
        );
        assert_eq!(
            donor
                .query_row("SELECT value FROM state", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "committed-in-wal"
        );
        donor.execute_batch("COMMIT;").unwrap();
        assert_eq!(state(&destination), "committed-in-wal");
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
#[cfg(test)]
mod storage_retention_tests {
    use super::*;
    #[test]
    fn post_publication_sweep_drops_superseded_refs_but_keeps_current_owner() {
        let temp = tempfile::tempdir().unwrap();
        let view = ViewStore::open(temp.path(), "0123456789abcdef").unwrap();
        let owner = view.derived_path("owner").unwrap();
        fs::write(&owner, b"shared database").unwrap();
        fs::write(view.manifest_path("owner").unwrap(), b"owner manifest").unwrap();
        view.reuse_derived("superseded", "owner").unwrap();
        view.reuse_derived("current", "owner").unwrap();
        view.open_pointer_connection()
            .unwrap()
            .execute("UPDATE pointer SET generation = 'current'", [])
            .unwrap();
        schedule_generation_sweep(view.clone());
        wait_for_generation_sweep_for_test(view.view_dir());
        assert!(!view.view_dir().join("derived-superseded.ref").exists());
        assert!(view.view_dir().join("derived-current.ref").exists());
        assert!(owner.exists());
        assert_eq!(view.derived_path("current").unwrap(), owner);
        fs::write(
            view.derived_path("independent").unwrap(),
            b"independent database",
        )
        .unwrap();
        view.open_pointer_connection()
            .unwrap()
            .execute("UPDATE pointer SET generation = 'independent'", [])
            .unwrap();
        schedule_generation_sweep(view.clone());
        wait_for_generation_sweep_for_test(view.view_dir());
        assert!(!owner.exists());
        assert!(view.derived_path("independent").unwrap().exists());
    }

    #[test]
    fn post_publication_sweep_preserves_cloned_pages_after_donor_removal() {
        let temp = tempfile::tempdir().unwrap();
        let view = ViewStore::open(temp.path(), "0123456789abcdef").unwrap();
        let donor = view.derived_path("donor").unwrap();
        let cloned = view.derived_path("cloned").unwrap();
        let connection =
            crate::db::file_identity::IdentityConnection::open(&donor, "clone donor").unwrap();
        connection
            .execute_batch(
                "CREATE TABLE state (value TEXT); INSERT INTO state VALUES ('retained');",
            )
            .unwrap();
        clone_derived(&donor, &cloned).unwrap();
        assert!(!view.view_dir().join("derived-cloned.ref").exists());
        drop(connection);
        view.open_pointer_connection()
            .unwrap()
            .execute("UPDATE pointer SET generation = 'cloned'", [])
            .unwrap();
        schedule_generation_sweep(view.clone());
        wait_for_generation_sweep_for_test(view.view_dir());
        assert!(!donor.exists());
        assert_eq!(crate::db::file_identity::open_connections(&cloned), 0);
        let reader =
            crate::db::file_identity::IdentityConnection::open(&cloned, "clone survivor").unwrap();
        assert_eq!(
            reader
                .query_row("SELECT value FROM state", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "retained"
        );
    }

    #[test]
    fn storage_retention_shared_derived_owner_keeps_manifest_until_unreferenced() {
        let temp = tempfile::tempdir().unwrap();
        let view = ViewStore::open(temp.path(), "0123456789abcdef").unwrap();
        let database = view.derived_path("old").unwrap();
        fs::write(&database, b"shared database").unwrap();
        let manifest = view.manifest_path("old").unwrap();
        let trigram = view.trigram_path("old").unwrap();
        fs::write(&manifest, b"manifest").unwrap();
        fs::write(&trigram, b"trigram").unwrap();
        view.reuse_derived("current", "old").unwrap();
        view.open_pointer_connection()
            .unwrap()
            .execute("UPDATE pointer SET generation = 'current'", [])
            .unwrap();
        assert_eq!(view.sweep_generations().unwrap(), 1);
        assert!(database.exists());
        assert!(manifest.exists());
        assert!(!trigram.exists());
        fs::write(view.derived_path("next").unwrap(), b"next database").unwrap();
        fs::write(view.manifest_path("next").unwrap(), b"next manifest").unwrap();
        view.open_pointer_connection()
            .unwrap()
            .execute("UPDATE pointer SET generation = 'next'", [])
            .unwrap();
        // The first pass removes obsolete references; the second certifies
        // that the formerly shared owner has no remaining references.
        view.sweep_generations().unwrap();
        view.sweep_generations().unwrap();
        assert!(!database.exists());
        assert!(!manifest.exists());
    }

    #[test]
    fn storage_retention_live_assembly_pin_has_no_absolute_expiry() {
        let directory = tempfile::tempdir().unwrap();
        let view = ViewStore::open(directory.path(), "0123456789abcdef").unwrap();
        let path = view.derived_path("old").unwrap();
        fs::write(&path, b"generation").unwrap();
        let pin = crate::pins::AssemblyPin::create(view.view_dir(), "family", "view", "old", &[])
            .unwrap();
        let mut metadata = pin.metadata().clone();
        metadata.renewed_at = 0;
        fs::write(
            view.view_dir().join("pins/old.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        assert_eq!(view.sweep_generations().unwrap(), 0);
        assert!(path.exists());
        drop(pin);
        assert_eq!(view.sweep_generations().unwrap(), 1);
        assert!(!path.exists());
    }
    #[test]
    fn storage_retention_keeps_open_derived_file_set() {
        let directory = tempfile::tempdir().unwrap();
        let view = ViewStore::open(directory.path(), "0123456789abcdef").unwrap();
        let path = view.derived_path("old").unwrap();
        let connection =
            crate::db::file_identity::IdentityConnection::open(&path, "retention test").unwrap();
        schedule_generation_sweep(view.clone());
        wait_for_generation_sweep_for_test(view.view_dir());
        assert_eq!(crate::db::file_identity::open_connections(&path), 1);
        assert_eq!(
            view.sweep_generations_with_bytes().unwrap().removed_bytes,
            0
        );
        assert_eq!(view.sweep_generations().unwrap(), 0);
        assert!(path.exists());
        drop(connection);
        assert_eq!(view.sweep_generations().unwrap(), 1);
        assert!(!path.exists());
    }
}
