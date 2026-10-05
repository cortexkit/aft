//! Named refusal of on-disk formats written by a newer AFT build.
//!
//! Rolling back to an older build must never destroy what a newer build wrote.
//! Every versioned artifact AFT persists therefore follows one rule: when its
//! reader meets a format version above the highest one this build reads, it
//! refuses the artifact *by name*, before reading or changing the payload. It
//! never calls the artifact corrupt, never skips it silently, never rebuilds
//! over it, rewrites it or quarantines it. The component that owns the
//! artifact reports itself unavailable with the refusal as its reason.
//!
//! [`UnsupportedPersistedFormat`] is that refusal. Readers record each one in
//! a process-wide registry keyed by the artifact path (or by the storage root
//! when the refusal comes from the reader floor, see
//! [`crate::reader_floor`]). Writers consult the registry before replacing an
//! artifact, and status surfaces read it to name the reason a component is
//! unavailable. Keying by path keeps unrelated storage roots (and parallel
//! tests with their own temporary roots) independent.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Stable wire code carried at the start of every refusal message, so a
/// consumer can recognise the refusal without parsing the rest of the text.
pub const CODE: &str = "storage_requires_newer_reader";

/// One independently versioned on-disk format.
///
/// The name of each store is the key used in the reader floor file and in
/// `aft --formats`, so it must never change once shipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PersistedStore {
    /// `aft.db` SQLite schema (`schema_version` table maximum).
    AftDb,
    /// Trigram search index `index/<key>/cache.bin`.
    SearchIndex,
    /// Semantic index base snapshot `semantic/<key>/semantic.bin`.
    SemanticIndex,
    /// Checksummed delta frames appended to the semantic base snapshot.
    SemanticSegment,
    /// Callgraph generation databases (`meta.schema_version`).
    CallgraphStore,
    /// Symbol cache file envelope `symbols/<key>/symbols.bin`.
    SymbolCache,
    /// Symbol extraction schema stored inside the symbol cache. A newer
    /// extraction schema is newer content, so an older build must not
    /// overwrite it with its own extraction.
    SymbolExtraction,
    /// Artifact ownership manifest `artifact-owners/<key>/owner.json`.
    ArtifactOwner,
    /// Undo backup stack metadata `<harness>/backups/.../meta.json`.
    BackupMeta,
    /// Background bash task metadata `<harness>/bash-tasks/.../metadata.json`.
    BashTask,
}

impl PersistedStore {
    pub const ALL: [PersistedStore; 10] = [
        PersistedStore::AftDb,
        PersistedStore::SearchIndex,
        PersistedStore::SemanticIndex,
        PersistedStore::SemanticSegment,
        PersistedStore::CallgraphStore,
        PersistedStore::SymbolCache,
        PersistedStore::SymbolExtraction,
        PersistedStore::ArtifactOwner,
        PersistedStore::BackupMeta,
        PersistedStore::BashTask,
    ];

    /// Stable key used in the floor file and in `aft --formats`.
    pub const fn name(self) -> &'static str {
        match self {
            PersistedStore::AftDb => "aft.db",
            PersistedStore::SearchIndex => "search",
            PersistedStore::SemanticIndex => "semantic",
            PersistedStore::SemanticSegment => "semantic_segment",
            PersistedStore::CallgraphStore => "callgraph",
            PersistedStore::SymbolCache => "symbols",
            PersistedStore::SymbolExtraction => "symbols_extraction",
            PersistedStore::ArtifactOwner => "artifact_owner",
            PersistedStore::BackupMeta => "backup_meta",
            PersistedStore::BashTask => "bash_task",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|store| store.name() == name)
    }

    /// Highest format version of this store that this build reads.
    pub fn supported(self) -> u32 {
        match self {
            PersistedStore::AftDb => crate::db::CURRENT_SCHEMA_VERSION,
            PersistedStore::SearchIndex => crate::search_index::INDEX_FORMAT_VERSION,
            PersistedStore::SemanticIndex => crate::semantic_index::SEMANTIC_BASE_FORMAT_VERSION,
            PersistedStore::SemanticSegment => {
                crate::semantic_index::SEMANTIC_SEGMENT_FORMAT_VERSION
            }
            PersistedStore::CallgraphStore => crate::callgraph_store::STORE_FORMAT_VERSION,
            PersistedStore::SymbolCache => crate::symbol_cache_disk::FORMAT_VERSION,
            PersistedStore::SymbolExtraction => crate::symbol_cache_disk::SCHEMA_VERSION,
            PersistedStore::ArtifactOwner => crate::artifact_owner::SCHEMA_VERSION,
            PersistedStore::BackupMeta => crate::backup::SCHEMA_VERSION,
            PersistedStore::BashTask => crate::bash_background::persistence::SCHEMA_VERSION,
        }
    }

    /// Format version of this store that this build writes. Every store
    /// currently writes the highest version it reads; the two are kept apart
    /// so a build that learns to read a format before it starts writing it
    /// raises the floor only when it actually writes.
    pub fn written(self) -> u32 {
        self.supported()
    }
}

impl fmt::Display for PersistedStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Where a refusal was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalSource {
    /// The artifact's own header or envelope carries the newer version.
    Artifact,
    /// The storage root's reader floor requires a newer reader for the store,
    /// even if no artifact of the newer format has been written yet.
    Floor,
    /// The storage root's reader floor itself is in a newer floor schema (or
    /// cannot be parsed), so this build cannot tell which stores it may read
    /// and refuses all of them. `found` carries the floor schema, 0 when the
    /// file is not parseable.
    FloorUnreadable,
}

/// An on-disk format newer than this build reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedPersistedFormat {
    pub store: PersistedStore,
    /// The artifact (or, for a floor refusal, the floor file) that was refused.
    pub path: PathBuf,
    /// The version found on disk (or required by the floor).
    pub found: u64,
    /// The highest version of `store` this build reads.
    pub supported: u32,
    pub source: RefusalSource,
}

impl UnsupportedPersistedFormat {
    pub fn artifact(store: PersistedStore, path: impl Into<PathBuf>, found: u64) -> Self {
        Self {
            store,
            path: path.into(),
            found,
            supported: store.supported(),
            source: RefusalSource::Artifact,
        }
    }

    pub fn floor(store: PersistedStore, floor_path: impl Into<PathBuf>, required: u64) -> Self {
        Self {
            store,
            path: floor_path.into(),
            found: required,
            supported: store.supported(),
            source: RefusalSource::Floor,
        }
    }

    pub fn floor_unreadable(
        store: PersistedStore,
        floor_path: impl Into<PathBuf>,
        floor_schema: u64,
    ) -> Self {
        Self {
            store,
            path: floor_path.into(),
            found: floor_schema,
            supported: store.supported(),
            source: RefusalSource::FloorUnreadable,
        }
    }

    /// `Some` when `found` is above what this build reads for `store`.
    pub fn check(store: PersistedStore, path: &Path, found: u64) -> Option<Self> {
        (found > u64::from(store.supported())).then(|| Self::artifact(store, path, found))
    }

    /// Short degraded-mode reason: the code plus the store name.
    pub fn reason(&self) -> String {
        format!("{CODE}:{}", self.store.name())
    }

    /// Wrap as an I/O error that callers can recognise with
    /// [`from_io_error`](Self::from_io_error) after propagation.
    pub fn into_io_error(self) -> std::io::Error {
        std::io::Error::other(self)
    }

    /// Recover a refusal carried by an I/O error built with
    /// [`into_io_error`](Self::into_io_error).
    pub fn from_io_error(error: &std::io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref::<Self>()
    }

    /// Whether `message` is a rendered refusal (it starts with [`CODE`]).
    pub fn is_refusal_message(message: &str) -> bool {
        message.starts_with(CODE)
    }
}

impl fmt::Display for UnsupportedPersistedFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.source {
            RefusalSource::Artifact => write!(
                f,
                "{CODE}: {} at {} has format version {}; this build reads up to {}",
                self.store,
                self.path.display(),
                self.found,
                self.supported
            ),
            RefusalSource::Floor => write!(
                f,
                "{CODE}: reader floor {} requires {} format version {}; this build reads up to {}",
                self.path.display(),
                self.store,
                self.found,
                self.supported
            ),
            RefusalSource::FloorUnreadable if self.found == 0 => write!(
                f,
                "{CODE}: reader floor {} cannot be parsed, so this build cannot tell whether it may read {}",
                self.path.display(),
                self.store
            ),
            RefusalSource::FloorUnreadable => write!(
                f,
                "{CODE}: reader floor {} has floor_schema {}; this build reads floor_schema up to {}, so it cannot tell whether it may read {}",
                self.path.display(),
                self.found,
                crate::reader_floor::FLOOR_SCHEMA,
                self.store
            ),
        }
    }
}

impl std::error::Error for UnsupportedPersistedFormat {}

#[derive(Debug, Clone)]
struct Entry {
    /// Paths at or below `scope` are covered by the refusal.
    scope: PathBuf,
    refusal: UnsupportedPersistedFormat,
}

fn registry() -> &'static Mutex<Vec<Entry>> {
    static REGISTRY: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

fn with_registry<T>(f: impl FnOnce(&mut Vec<Entry>) -> T) -> T {
    let mut guard = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

/// Record `refusal` for every path at or below `scope` and log it once.
///
/// Artifact refusals normally use the artifact path itself as the scope; a
/// store whose artifact is a directory of generations (the callgraph) uses
/// that directory. Floor refusals use the storage root.
pub fn record(refusal: &UnsupportedPersistedFormat, scope: &Path) {
    let inserted = with_registry(|entries| {
        if let Some(existing) = entries
            .iter_mut()
            .find(|entry| entry.refusal.store == refusal.store && entry.scope == scope)
        {
            let changed = existing.refusal != *refusal;
            existing.refusal = refusal.clone();
            changed
        } else {
            entries.push(Entry {
                scope: scope.to_path_buf(),
                refusal: refusal.clone(),
            });
            true
        }
    });
    if inserted {
        crate::slog_warn!(
            "{refusal}; leaving it untouched and reporting the component unavailable"
        );
    }
}

/// Record an artifact refusal scoped to its own path and return it, so a
/// reader can write `return Err(refuse(...))`.
pub fn refuse(refusal: UnsupportedPersistedFormat) -> UnsupportedPersistedFormat {
    let scope = refusal.path.clone();
    record(&refusal, &scope);
    refusal
}

/// Forget artifact refusals recorded for exactly `scope`, after the artifact
/// was found readable again (for example replaced by the newer build that
/// wrote it, or removed by the operator). Floor refusals are never cleared:
/// the floor is never lowered while the process runs.
pub fn clear(store: PersistedStore, scope: &Path) {
    with_registry(|entries| {
        entries.retain(|entry| {
            !(entry.refusal.store == store
                && entry.refusal.source == RefusalSource::Artifact
                && entry.scope == scope)
        })
    });
}

/// Forget artifact refusals under a swept directory only when the refused
/// path is confirmed absent. Permission errors and unreadable paths are not
/// evidence of removal, and a reader floor is never lowered by housekeeping.
pub fn clear_missing_artifacts(store: PersistedStore, root: &Path) {
    with_registry(|entries| {
        entries.retain(|entry| {
            !(entry.refusal.store == store
                && entry.refusal.source == RefusalSource::Artifact
                && entry.scope.starts_with(root)
                && std::fs::symlink_metadata(&entry.refusal.path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound))
        })
    });
}

/// The refusal, if any, that covers `path` for `store`: an artifact refusal
/// recorded at or above `path`, or a floor refusal for the storage root that
/// contains it.
pub fn refusal_covering(store: PersistedStore, path: &Path) -> Option<UnsupportedPersistedFormat> {
    with_registry(|entries| {
        entries
            .iter()
            .find(|entry| entry.refusal.store == store && path.starts_with(&entry.scope))
            .map(|entry| entry.refusal.clone())
    })
}

/// Every refusal recorded at or below `root`, floor refusals first. Status
/// surfaces use this to list what the storage root holds that this build
/// cannot read.
pub fn refusals_under(root: &Path) -> Vec<UnsupportedPersistedFormat> {
    let mut found = with_registry(|entries| {
        entries
            .iter()
            .filter(|entry| entry.scope.starts_with(root))
            .map(|entry| entry.refusal.clone())
            .collect::<Vec<_>>()
    });
    found.sort_by_key(|refusal| (refusal.source == RefusalSource::Artifact, refusal.store));
    found.dedup();
    found
}

/// Gate one artifact read: a floor refusal for the store wins; otherwise a
/// newer `found` version is recorded and returned. Anything else (a readable
/// version, or no recognisable artifact at all) clears a stale refusal left
/// for the same artifact, for example after the operator removed it.
pub fn gate(
    store: PersistedStore,
    path: &Path,
    scope: &Path,
    found: Option<u64>,
) -> Result<(), UnsupportedPersistedFormat> {
    if let Some(refusal) =
        refusal_covering(store, path).filter(|refusal| refusal.source != RefusalSource::Artifact)
    {
        return Err(refusal);
    }
    match found.and_then(|found| UnsupportedPersistedFormat::check(store, path, found)) {
        Some(refusal) => {
            record(&refusal, scope);
            Err(refusal)
        }
        None => {
            clear(store, scope);
            Ok(())
        }
    }
}

/// Peek the version headers of one project's shared artifacts (search,
/// semantic, symbol cache, callgraph, ownership manifest) and record a named
/// refusal for each one written in a newer format. Reads headers only; never
/// changes anything. Absent artifacts are skipped.
pub fn preflight_project_artifacts(storage_root: &Path, project_key: &str) {
    let _ = crate::search_index::SearchIndex::check_disk_cache_format(
        &storage_root.join("index").join(project_key),
    );
    let _ = crate::semantic_index::SemanticIndex::check_disk_format(storage_root, project_key);
    let _ = crate::symbol_cache_disk::check_disk_format(storage_root, project_key);
    let _ = crate::callgraph_store::check_published_format(
        &storage_root.join("callgraph").join(project_key),
        project_key,
    );
    let _ = crate::artifact_owner::check_manifest_format(storage_root, project_key);
}

/// Highest format version of every store this build reads, as printed by
/// `aft --formats`. Placement compares this map with the storage root's
/// reader floor without starting the candidate against any data.
pub fn formats_json() -> serde_json::Value {
    let stores = PersistedStore::ALL
        .into_iter()
        .map(|store| {
            (
                store.name().to_string(),
                serde_json::json!(store.supported()),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({
        "formats_schema": 1,
        "floor_schema": crate::reader_floor::FLOOR_SCHEMA,
        "version": env!("CARGO_PKG_VERSION"),
        "stores": stores,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_names_round_trip_and_are_unique() {
        let mut names = PersistedStore::ALL
            .iter()
            .map(|store| store.name())
            .collect::<Vec<_>>();
        for store in PersistedStore::ALL {
            assert_eq!(PersistedStore::from_name(store.name()), Some(store));
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), PersistedStore::ALL.len());
    }

    #[test]
    fn refusal_names_the_store_path_and_versions() {
        let refusal = UnsupportedPersistedFormat::artifact(
            PersistedStore::SearchIndex,
            "/tmp/x/cache.bin",
            99,
        );
        let text = refusal.to_string();
        assert!(text.starts_with(CODE), "{text}");
        assert!(text.contains("search"), "{text}");
        assert!(text.contains("/tmp/x/cache.bin"), "{text}");
        assert!(text.contains("99"), "{text}");
        assert!(
            text.contains(&PersistedStore::SearchIndex.supported().to_string()),
            "{text}"
        );
        let io = refusal.clone().into_io_error();
        assert_eq!(
            UnsupportedPersistedFormat::from_io_error(&io),
            Some(&refusal)
        );
    }

    #[test]
    fn registry_scopes_refusals_by_path_and_clears_only_artifacts() {
        let root = tempfile::tempdir().unwrap();
        let artifact = root.path().join("index").join("k").join("cache.bin");
        assert!(gate(PersistedStore::SearchIndex, &artifact, &artifact, Some(99)).is_err());
        assert!(refusal_covering(PersistedStore::SearchIndex, &artifact).is_some());
        assert!(refusal_covering(PersistedStore::SemanticIndex, &artifact).is_none());
        let sibling = root.path().join("index").join("other").join("cache.bin");
        assert!(refusal_covering(PersistedStore::SearchIndex, &sibling).is_none());
        assert_eq!(refusals_under(root.path()).len(), 1);

        // A readable version clears the artifact refusal again.
        let current = u64::from(PersistedStore::SearchIndex.supported());
        assert!(gate(
            PersistedStore::SearchIndex,
            &artifact,
            &artifact,
            Some(current)
        )
        .is_ok());
        assert!(refusal_covering(PersistedStore::SearchIndex, &artifact).is_none());

        // A floor refusal covers the whole root and survives `clear`.
        let floor = UnsupportedPersistedFormat::floor(
            PersistedStore::SearchIndex,
            root.path().join(crate::reader_floor::FLOOR_FILE),
            current + 1,
        );
        record(&floor, root.path());
        clear(PersistedStore::SearchIndex, root.path());
        assert_eq!(
            gate(
                PersistedStore::SearchIndex,
                &artifact,
                &artifact,
                Some(current)
            ),
            Err(floor)
        );
    }
}
