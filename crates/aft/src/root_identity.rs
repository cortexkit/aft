//! A root's first bound identity survives missing Git and process restarts.
//! Path identities never become repository identities implicitly.

pub mod key;

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Identity {
    Path,
    Git {
        root_commit: String,
        toplevel: PathBuf,
        relative: Option<String>,
    },
}

/// Only a successful Git answer is fresh verification. Memo or saved-record
/// fallback may select an identity, but cannot certify new repository metadata.
pub(crate) struct IdentityChoice {
    pub(crate) identity: Identity,
    verified: bool,
}

impl IdentityChoice {
    pub(crate) fn verified(identity: Identity) -> Self {
        Self {
            identity,
            verified: true,
        }
    }

    pub(crate) fn retained(identity: Identity) -> Self {
        Self {
            identity,
            verified: false,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    root: PathBuf,
    identity: Identity,
    // A background-verification hint, not evidence for changing the identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    probe_stamp: Option<String>,
}

pub(crate) fn canonical_root(root: &Path) -> PathBuf {
    crate::cache_location::canonical_root(root)
}

fn record_path(root: &Path, storage: &Path) -> PathBuf {
    let hash = blake3::hash(root.as_os_str().as_encoded_bytes());
    storage.join("root-identities").join(format!("{hash}.json"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordStamp {
    modified: std::time::SystemTime,
    len: u64,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
    #[cfg(windows)]
    created: u64,
    /// Volume serial number and file index. A replacement renamed over the
    /// record is a different file, so this changes even when the size,
    /// modification time and creation time all match: Windows stamps file
    /// times from a coarse clock (about 15.6 ms per tick), so two records
    /// written in the same tick can carry identical times.
    #[cfg(windows)]
    identity: crate::file_identity::FileIdentity,
}

/// The caller supplies a canonical root so checking a memo requires just one
/// stat of the record, not another filesystem traversal to resolve the root.
pub(crate) fn record_stamp(
    canonical_root: &Path,
    storage: &Path,
) -> io::Result<Option<RecordStamp>> {
    let path = record_path(canonical_root, storage);
    #[cfg(not(windows))]
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // One metadata-only handle supplies both the times and the file identity,
    // so they always describe the same file. A record whose identity cannot
    // be read yields an error, which leaves the memo uncached rather than
    // trusting times alone.
    #[cfg(windows)]
    let (metadata, identity) = match crate::file_identity::metadata(&path) {
        Ok((_, crate::file_identity::FileIdentity::Unavailable)) => {
            return Err(io::Error::other(
                "the root identity record reported no file identity",
            ))
        }
        Ok(found) => found,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    #[cfg(windows)]
    use std::os::windows::fs::MetadataExt;
    Ok(Some(RecordStamp {
        modified: metadata.modified()?,
        len: metadata.len(),
        #[cfg(unix)]
        identity: (
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ),
        #[cfg(windows)]
        created: metadata.creation_time(),
        #[cfg(windows)]
        identity,
    }))
}

pub(crate) fn read(root: &Path, storage: &Path) -> io::Result<Option<Identity>> {
    Ok(read_record(root, storage)?.map(|record| record.identity))
}

fn read_record(root: &Path, storage: &Path) -> io::Result<Option<Record>> {
    let root = canonical_root(root);
    let path = record_path(&root, storage);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let record: Record = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if record.version != 1 || record.root != root {
        return Err(io::Error::other(
            "root identity record has an unsupported version or target",
        ));
    }
    record.identity.validate(&root)?;
    Ok(Some(record))
}

/// An identity chosen for one bind.
pub(crate) struct BoundIdentity {
    pub(crate) identity: Identity,
    /// The older aft.db pin could not be read, so this identity was not saved
    /// and the root's next bind may choose a different one. Nothing may be
    /// stored under its key until a bind that read the pin confirms it.
    pub(crate) provisional: bool,
}

impl BoundIdentity {
    fn settled(identity: Identity) -> Self {
        Self {
            identity,
            provisional: false,
        }
    }
}

/// Serialize the first choice across processes and aliases. A corrupt record
/// is an error, not permission to choose a replacement identity.
#[cfg(test)]
pub(crate) fn bind(
    root: &Path,
    storage: &Path,
    choose: impl FnOnce(LegacyPin, Option<&Identity>) -> io::Result<IdentityChoice>,
) -> io::Result<Identity> {
    bind_with_recheck(root, storage, true, choose).map(|bound| bound.identity)
}

/// Real binds force verification; background passes may reuse an observed stamp.
pub(crate) fn bind_with_recheck(
    root: &Path,
    storage: &Path,
    force: bool,
    choose: impl FnOnce(LegacyPin, Option<&Identity>) -> io::Result<IdentityChoice>,
) -> io::Result<BoundIdentity> {
    let root = canonical_root(root);
    if let Some(record) = read_record(&root, storage)? {
        if reusable(&record, &root, force) {
            return Ok(BoundIdentity::settled(record.identity));
        }
    }
    let path = record_path(&root, storage);
    let directory = path.parent().unwrap();
    crate::backup::create_private_durable_dir(directory)?;
    let lock_path = path.with_extension("lock");
    crate::production_storage::refuse_write(&lock_path)?;
    let mut options = crate::private_storage::options();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(lock_path)?;
    lock.lock()?;
    let _lock = crate::fs_lock::AdvisoryLockGuard::from_locked_file(lock);
    let previous = read_record(&root, storage)?;
    if let Some(record) = &previous {
        if reusable(record, &root, force) {
            return Ok(BoundIdentity::settled(record.identity.clone()));
        }
    }
    // With Git available, resolve again under the record lock: a different
    // repository can replace a checkout without changing its canonical path.
    let before = crate::developer_tools::git_usable()
        .then(|| crate::root_identity::key::repository_identity_stamp(&root))
        .flatten();
    // A saved record already holds this root's identity, so the older aft.db
    // pin is not consulted and a busy database cannot delay the bind.
    let pins = if previous.is_some() {
        LegacyPins::Known(Vec::new())
    } else {
        legacy_pins(&root, storage)
    };
    let choice = choose_with_pins(&pins, |pin| {
        choose(pin, previous.as_ref().map(|record| &record.identity))
    })?;
    let identity = choice.identity;
    identity.validate(&root)?;
    if matches!(pins, LegacyPins::Unreadable) {
        // The chosen identity may contradict a pin that could not be read.
        // Use it for this bind only; the next bind reads the pin again.
        return Ok(BoundIdentity {
            identity,
            provisional: true,
        });
    }
    if !choice.verified
        && previous
            .as_ref()
            .is_some_and(|record| record.identity != identity)
    {
        return Err(io::Error::other(
            "an unverified choice cannot replace a saved root identity",
        ));
    }
    let probe_stamp = if choice.verified {
        if matches!(&identity, Identity::Git { .. }) {
            before.filter(|stamp| {
                Some(stamp) == crate::root_identity::key::repository_identity_stamp(&root).as_ref()
            })
        } else {
            None
        }
    } else {
        // Retain an older certificate, never the metadata from this failed check.
        previous
            .as_ref()
            .filter(|record| record.identity == identity)
            .and_then(|record| record.probe_stamp.clone())
    };
    if previous
        .as_ref()
        .is_some_and(|record| record.identity == identity && record.probe_stamp == probe_stamp)
    {
        return Ok(BoundIdentity::settled(identity));
    }
    let bytes = serde_json::to_vec(&Record {
        version: 1,
        root,
        identity: identity.clone(),
        probe_stamp,
    })
    .map_err(io::Error::other)?;
    crate::production_storage::refuse_write(&path)?;
    crate::durability::atomic_write(
        &path,
        &bytes,
        crate::durability::AtomicWriteDurability::FileAndDirectory,
    )?;
    Ok(BoundIdentity::settled(identity))
}

fn reusable(record: &Record, root: &Path, force: bool) -> bool {
    matches!(&record.identity, Identity::Path)
        || !crate::developer_tools::git_usable()
        || (!force
            && record.probe_stamp.as_ref().is_some_and(|stamp| {
                crate::root_identity::key::repository_identity_stamp(root).as_ref() == Some(stamp)
            }))
}

/// What older standing entries in aft.db say about a root's identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LegacyPin {
    /// No older standing entry pins this root to its path identity.
    Unpinned,
    /// An older standing entry pins this root to its path identity.
    Path,
    /// aft.db could not be read without waiting, so any pin is unknown. The
    /// identity chosen now is temporary: nothing derived from it, including
    /// legacy memo entries, may be saved.
    Unreadable,
}

/// `(resolved_git_toplevel, scoped_relative_path)` of each older standing entry.
type StandingPin = (Option<String>, Option<String>);

enum LegacyPins {
    Known(Vec<StandingPin>),
    Unreadable,
}

/// Roots whose unreadable pin has already been logged, so a database that
/// stays busy across many binds logs one line per root, not one per bind.
static UNREADABLE_PIN_ROOTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::LazyLock::new(Default::default);

/// Older standing entries already pin an identity even before this record
/// format existed. Import path pins; refuse a repository downgrade or mismatch.
pub(crate) fn select(
    root: &Path,
    storage: &Path,
    choose: impl FnOnce(LegacyPin) -> io::Result<IdentityChoice>,
) -> io::Result<BoundIdentity> {
    let canonical = canonical_root(root);
    let pins = legacy_pins(&canonical, storage);
    let choice = choose_with_pins(&pins, choose)?;
    Ok(BoundIdentity {
        identity: choice.identity,
        provisional: matches!(pins, LegacyPins::Unreadable),
    })
}

fn choose_with_pins(
    pins: &LegacyPins,
    choose: impl FnOnce(LegacyPin) -> io::Result<IdentityChoice>,
) -> io::Result<IdentityChoice> {
    let pins = match pins {
        LegacyPins::Known(pins) => pins,
        LegacyPins::Unreadable => return choose(LegacyPin::Unreadable),
    };
    let pin = if pins.iter().any(|(toplevel, _)| toplevel.is_none()) {
        LegacyPin::Path
    } else {
        LegacyPin::Unpinned
    };
    let choice = choose(pin)?;
    let (toplevel, relative) = choice.identity.topology();
    let toplevel = toplevel.map(|path| path.to_string_lossy().into_owned());
    if pins
        .iter()
        .any(|pin| pin != &(toplevel.clone(), relative.clone()))
    {
        return Err(io::Error::other("the existing standing repository identity cannot be recovered; Git unavailability does not authorize a path identity"));
    }
    Ok(choice)
}

/// Read the older standing pins for `root` without waiting on aft.db. Another
/// process may hold the database for a long time, and a bind must not fail or
/// stall on it: a busy, locked or unopenable database reports `Unreadable`.
fn legacy_pins(root: &Path, storage: &Path) -> LegacyPins {
    match read_legacy_pins(root, storage) {
        Ok(pins) => {
            UNREADABLE_PIN_ROOTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(root);
            LegacyPins::Known(pins)
        }
        Err(error) => {
            if UNREADABLE_PIN_ROOTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(root.to_path_buf())
            {
                crate::slog_info!(
                    "legacy pin unreadable (aft.db busy): identity provisional for root={}; indexes start once the database is free; error={error}",
                    root.display()
                );
            }
            LegacyPins::Unreadable
        }
    }
}

/// Whether the older standing pins for `root` can be read right now without
/// waiting on aft.db. A bind left provisional by a busy database polls this
/// before it tries the whole bind again.
pub(crate) fn legacy_pins_readable(root: &Path, storage: &Path) -> bool {
    read_legacy_pins(&canonical_root(root), storage).is_ok()
}

fn read_legacy_pins(root: &Path, storage: &Path) -> io::Result<Vec<StandingPin>> {
    let database = storage.join("aft.db");
    if !database.try_exists()? {
        return Ok(Vec::new());
    }
    let connection = crate::db::open_readonly(&database).map_err(io::Error::other)?;
    connection
        .busy_timeout(std::time::Duration::ZERO)
        .map_err(io::Error::other)?;
    let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='standing_roots')", [], |row| row.get(0)).map_err(io::Error::other)?;
    if !exists {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare("SELECT resolved_git_toplevel, scoped_relative_path FROM standing_roots WHERE resolved_target = ?1").map_err(io::Error::other)?;
    let pins = statement
        .query_map([root.to_string_lossy().as_ref()], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })
        .map_err(io::Error::other)?
        .collect::<Result<_, _>>()
        .map_err(io::Error::other)?;
    Ok(pins)
}

impl Identity {
    pub(crate) fn git(root: &Path, root_commit: String) -> io::Result<Self> {
        let root = canonical_root(root);
        let toplevel = nearest_git_marker(&root)?.unwrap_or_else(|| root.clone());
        let relative = if root == toplevel {
            None
        } else {
            Some(
                crate::scoped_key::scoped_relative_path(&root, &toplevel)
                    .map_err(io::Error::other)?,
            )
        };
        Ok(Self::Git {
            root_commit,
            toplevel,
            relative,
        })
    }

    fn validate(&self, root: &Path) -> io::Result<()> {
        if let Self::Git {
            root_commit,
            toplevel,
            relative,
        } = self
        {
            if root_commit.trim().is_empty() {
                return Err(io::Error::other("root identity has no repository identity"));
            }
            let expected = if root == toplevel {
                None
            } else {
                Some(
                    crate::scoped_key::scoped_relative_path(root, toplevel)
                        .map_err(io::Error::other)?,
                )
            };
            if relative != &expected {
                return Err(io::Error::other(
                    "root identity has inconsistent repository scope",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn key(&self, root: &Path, standing: bool) -> String {
        match self {
            Self::Path => crate::root_identity::key::artifact_path_identity_key(root),
            Self::Git {
                root_commit,
                relative: None,
                ..
            } => crate::root_identity::key::artifact_key_from_git_identity(root_commit),
            Self::Git {
                root_commit,
                relative: Some(relative),
                ..
            } => {
                let key = crate::scoped_key::scoped_v1_key(root_commit, relative.as_bytes())
                    .expect("validated root identity scope");
                if standing {
                    key
                } else {
                    key[..16].to_string()
                }
            }
        }
    }

    pub(crate) fn topology(&self) -> (Option<PathBuf>, Option<String>) {
        match self {
            Self::Path => (None, None),
            Self::Git {
                toplevel, relative, ..
            } => (Some(toplevel.clone()), relative.clone()),
        }
    }
}

/// Inspect ancestors without invoking Git, including linked-worktree markers.
/// An unreadable marker cannot prove that a folder has no repository identity.
pub(crate) fn nearest_git_marker(root: &Path) -> io::Result<Option<PathBuf>> {
    for directory in root.ancestors() {
        match std::fs::symlink_metadata(directory.join(".git")) {
            Ok(_) => return Ok(Some(directory.to_path_buf())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_stamp_detects_same_size_same_mtime_replacement() {
        let project = tempfile::tempdir().unwrap();
        let root = canonical_root(project.path());
        let storage = tempfile::tempdir().unwrap();
        assert_eq!(record_stamp(&root, storage.path()).unwrap(), None);
        bind(&root, storage.path(), |_, _| {
            Ok(IdentityChoice::retained(Identity::Path))
        })
        .unwrap();
        let path = record_path(&root, storage.path());
        let before = record_stamp(&root, storage.path()).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let replacement = path.with_extension("replacement");
        std::fs::write(&replacement, std::fs::read(&path).unwrap()).unwrap();
        filetime::set_file_mtime(
            &replacement,
            filetime::FileTime::from_last_modification_time(&metadata),
        )
        .unwrap();
        // Windows file times come from a coarse clock, so a replacement written
        // in the same tick as the original carries the same creation time.
        // Copy it explicitly so the test does not depend on machine speed.
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileTimesExt;
            std::fs::OpenOptions::new()
                .write(true)
                .open(&replacement)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_created(metadata.created().unwrap()))
                .unwrap();
        }
        std::fs::rename(&replacement, &path).unwrap();
        assert_ne!(record_stamp(&root, storage.path()).unwrap(), before);
    }

    #[test]
    fn saved_identity_never_reads_the_legacy_database_pin() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        crate::developer_tools::with_git_usable_for_test(true, || {
            let first = bind_with_recheck(root.path(), storage.path(), true, |_, _| {
                Identity::git(root.path(), "a".repeat(40)).map(IdentityChoice::verified)
            })
            .unwrap();
            assert!(!first.provisional);
            // An aft.db that cannot be opened at all: any attempt to read the
            // older pin from it would make this bind provisional.
            std::fs::create_dir(storage.path().join("aft.db")).unwrap();
            // A forced bind of a repository identity re-verifies under the
            // record lock, the path that previously consulted the pin.
            let again = bind_with_recheck(root.path(), storage.path(), true, |pin, saved| {
                assert_eq!(pin, LegacyPin::Unpinned);
                assert!(saved.is_some());
                Identity::git(root.path(), "a".repeat(40)).map(IdentityChoice::verified)
            })
            .unwrap();
            assert!(
                !again.provisional,
                "a root with a saved identity must not read aft.db"
            );
            assert_eq!(again.identity, first.identity);
        });
    }

    #[test]
    fn retained_choice_cannot_replace_a_saved_identity() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        crate::developer_tools::with_git_usable_for_test(true, || {
            let first = bind(root.path(), storage.path(), |_, _| {
                Identity::git(root.path(), "a".repeat(40)).map(IdentityChoice::verified)
            })
            .unwrap();
            let result = bind(root.path(), storage.path(), |_, _| {
                Identity::git(root.path(), "b".repeat(40)).map(IdentityChoice::retained)
            });
            assert!(result.is_err());
            assert_eq!(
                read(root.path(), storage.path())
                    .unwrap()
                    .unwrap()
                    .key(root.path(), false),
                first.key(root.path(), false)
            );
        });
    }

    #[test]
    fn legacy_standing_repository_pin_is_not_downgraded() {
        let root = tempfile::tempdir().unwrap();
        let root = canonical_root(root.path());
        std::fs::create_dir(root.join(".git")).unwrap();
        let storage = tempfile::tempdir().unwrap();
        let record = crate::db::standing_roots::StandingRootRecord {
            literal_path: root.display().to_string(),
            resolved_target: root.display().to_string(),
            resolved_git_toplevel: Some(root.display().to_string()),
            scoped_relative_path: None,
        };
        let mut database = crate::db::open(&storage.path().join("aft.db")).unwrap();
        crate::db::standing_roots::ensure_standing_root(
            &mut database,
            &record,
            &[crate::config::IndexKind::Search],
        )
        .unwrap();
        crate::developer_tools::with_git_usable_for_test(false, || {
            let error = crate::root_identity::key::artifact_cache_key_with_memo(
                &root,
                &root,
                storage.path(),
                None,
            )
            .unwrap_err();
            assert!(error
                .detail()
                .contains("existing standing repository identity cannot be recovered"));
            assert!(read(&root, storage.path()).unwrap().is_none());
        });
        assert_eq!(
            crate::db::standing_roots::get_standing_root(&database, &record.literal_path).unwrap(),
            Some(record)
        );
    }

    #[test]
    fn competing_first_binds_share_one_durable_choice() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [
                Identity::Path,
                Identity::git(root.path(), "a".repeat(40)).unwrap(),
            ]
            .into_iter()
            .map(|choice| {
                let (calls, start) = (&calls, &start);
                let (root, storage) = (root.path(), storage.path());
                scope.spawn(move || {
                    start.wait();
                    // Without Git, both contenders must reuse the first choice.
                    crate::developer_tools::with_git_usable_for_test(false, || {
                        bind(root, storage, |_, _| {
                            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Ok(IdentityChoice::retained(choice))
                        })
                    })
                    .unwrap()
                    .key(root, false)
                })
            })
            .collect();
            let keys: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect();
            assert_eq!(keys[0], keys[1]);
        });
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn corrupt_identity_is_refused_without_replacement() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        bind(root.path(), storage.path(), |_, _| {
            Identity::git(root.path(), "a".repeat(40)).map(IdentityChoice::retained)
        })
        .unwrap();
        let path = record_path(&canonical_root(root.path()), storage.path());
        std::fs::write(&path, b"broken identity").unwrap();
        assert!(bind(root.path(), storage.path(), |_, _| panic!(
            "a corrupt repository record must not select a new identity"
        ))
        .is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"broken identity");
    }

    #[test]
    fn observing_identity_does_not_create_storage() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("not-created");
        crate::developer_tools::with_git_usable_for_test(false, || {
            assert!(crate::scoped_key::resolve_standing_root_in_storage(
                root.path().to_str().unwrap(),
                &storage,
                false
            )
            .is_ok());
        });
        assert!(!storage.exists());
    }

    #[cfg(unix)]
    #[test]
    fn canonical_alias_reuses_the_record() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let storage = directory.path().join("storage");
        let first = bind(&root, &storage, |_, _| {
            Ok(IdentityChoice::retained(Identity::Path))
        })
        .unwrap();
        let second = bind(&alias, &storage, |_, _| {
            panic!("alias must not choose another identity")
        })
        .unwrap();
        assert_eq!(first.key(&root, false), second.key(&alias, false));
    }
}
