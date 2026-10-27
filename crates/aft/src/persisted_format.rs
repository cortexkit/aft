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
    /// Separate v3 family registry `blobs/v3/<family>/members.sqlite`.
    ViewsV3Registry,
    /// Format of v3 plane payload databases under `blobs/v3/<family>/`.
    ViewsV3Store,
}

impl PersistedStore {
    pub const ALL: [PersistedStore; 12] = [
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
        PersistedStore::ViewsV3Registry,
        PersistedStore::ViewsV3Store,
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
            PersistedStore::ViewsV3Registry => "views_v3_registry",
            PersistedStore::ViewsV3Store => "views_v3_store",
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
            PersistedStore::ViewsV3Registry => crate::blob_store::v3::REGISTRY_BASE_REFS_VERSION,
            PersistedStore::ViewsV3Store => 1,
        }
    }

    /// The default version this build writes when preparing a store. Guarded
    /// backup stacks opt into their newer format at the imported write boundary;
    /// ordinary history must not raise that reader floor merely on startup.
    pub fn written(self) -> u32 {
        match self {
            Self::BackupMeta => crate::backup::ORDINARY_SCHEMA_VERSION,
            _ => self.supported(),
        }
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
    /// A schema, application-ID, layout-tag or SQLite PRAGMA check failed.
    Incompatible(IncompatibleReason),
    /// The storage root's reader floor requires a newer reader for the store,
    /// even if no artifact of the newer format has been written yet.
    Floor,
    /// The storage root's reader floor itself is in a newer floor schema (or
    /// cannot be parsed), so this build cannot tell which stores it may read
    /// and refuses all of them. `found` carries the floor schema, 0 when the
    /// file is not parseable.
    FloorUnreadable,
}

impl RefusalSource {
    // Status push keeps the original source ranks when new refusal kinds are
    // added, rather than depending on Rust enum discriminants.
    fn sort_key(self) -> u8 {
        match self {
            Self::Artifact => 0,
            Self::Floor => 1,
            Self::FloorUnreadable => 2,
            Self::Incompatible(_) => 3,
        }
    }
    fn is_artifact(self) -> bool {
        matches!(self, Self::Artifact | Self::Incompatible(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompatibleReason {
    SchemaDrift,
    ApplicationId,
    Pragma(&'static str),
    LayoutTag(u32),
}

impl fmt::Display for IncompatibleReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SchemaDrift => f.write_str("schema_drift"),
            Self::ApplicationId => f.write_str("application_id"),
            Self::Pragma(name) => write!(f, "pragma:{name}"),
            Self::LayoutTag(tag) => write!(f, "layout_tag:{tag}"),
        }
    }
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
    pub fn incompatible(
        store: PersistedStore,
        path: impl Into<PathBuf>,
        found: u64,
        reason: IncompatibleReason,
    ) -> Self {
        Self {
            store,
            path: path.into(),
            found,
            supported: store.supported(),
            source: RefusalSource::Incompatible(reason),
        }
    }
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
            RefusalSource::Incompatible(reason) => write!(
                f,
                "{CODE}: store {} at {} is incompatible with this build: {reason}",
                self.store, self.path.display()
            ),
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
        }?;
        if !self.source.is_artifact() && self.store != PersistedStore::AftDb {
            write!(
                f,
                "; restart AFT after repairing the reader floor to retry this store"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for UnsupportedPersistedFormat {}

#[derive(Debug, Clone)]
struct Entry {
    /// The route used by the caller, not the resolved target of a cache symlink.
    scope: PathBuf,
    root: Option<std::sync::Arc<StorageRoot>>,
    refusal: UnsupportedPersistedFormat,
}

#[derive(Debug)]
struct StorageRoot {
    spelling: PathBuf,
    canonical: PathBuf,
    /// Resolve parent aliases but keep the root's own link as a logical boundary.
    boundary: Option<RegisteredBoundary>,
}

#[derive(Debug)]
struct RegisteredBoundary {
    path: PathBuf,
    normalized_spelling: Option<PathBuf>,
}

fn absolute_route(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |root| root.join(path))
    };
    #[cfg(windows)]
    let absolute = crate::windows_path::normalize_windows_path(&absolute);
    // An artifact on a `storage.cache_dir` cache root is owned by the durable
    // storage root that owns the cache root, as if the cache were linked
    // under it. Spell it that way so it is routed, listed and cleared there,
    // and so the cache root never becomes a storage owner of its own.
    match crate::root_cache::durable_route(&absolute) {
        #[cfg(windows)]
        Some(route) => crate::windows_path::normalize_windows_path(&route),
        #[cfg(not(windows))]
        Some(route) => route,
        None => absolute,
    }
}

fn canonical_scope(path: &Path) -> PathBuf {
    #[cfg(test)]
    CANONICAL_PROBES.with(|count| count.set(count.get() + 1));
    let absolute = absolute_route(path);
    let resolved = crate::production_storage::canonicalized_with(&absolute, &|path| {
        std::fs::canonicalize(path).ok()
    });
    crate::inspect::job::normalize_path(&resolved)
}

fn storage_roots() -> &'static std::sync::RwLock<Vec<std::sync::Arc<StorageRoot>>> {
    static ROOTS: OnceLock<std::sync::RwLock<Vec<std::sync::Arc<StorageRoot>>>> = OnceLock::new();
    ROOTS.get_or_init(Default::default)
}

#[cfg(test)]
thread_local! { static ROOT_RESOLUTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
thread_local! { static CANONICAL_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
thread_local! { static BOUNDARY_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
thread_local! { static ALIAS_VERIFICATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn storage_root(path: &Path) -> std::sync::Arc<StorageRoot> {
    let spelling = absolute_route(path);
    {
        let roots = storage_roots()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(root) = roots.iter().find(|root| root.spelling == spelling) {
            return root.clone();
        }
    }
    // On a cache miss, resolve the root identity. Later cache hits return
    // under the read lock without filesystem work or a write lock.
    #[cfg(test)]
    ROOT_RESOLUTIONS.with(|count| count.set(count.get() + 1));
    let canonical = canonical_scope(path);
    let mut roots = storage_roots()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(root) = roots.iter().find(|root| root.spelling == spelling) {
        return root.clone();
    }
    // Bound the optimization cache, not ownership evidence. Root-scoped
    // refusals retain their own identity, and explicit root queries still use
    // the uncached identity when the cache is full.
    if roots.len() >= 128 {
        drop(roots);
        let boundary = registered_boundary(&spelling, &canonical).ok();
        return std::sync::Arc::new(StorageRoot {
            canonical,
            spelling,
            boundary,
        });
    }
    let (boundary, warning) = match registered_boundary(&spelling, &canonical) {
        Ok(boundary) => (Some(boundary), None),
        Err(error) => (None, Some(error)),
    };
    let candidate = std::sync::Arc::new(StorageRoot {
        canonical,
        spelling,
        boundary,
    });
    roots.push(candidate.clone());
    drop(roots);
    // Publish missing metadata before logging; warm calls neither retry the
    // failed probe nor emit another warning for this registration.
    if let Some(error) = warning {
        crate::slog_warn!(
            "storage-root alias boundary unavailable for {}: {error}; retaining direct routes only",
            candidate.spelling.display()
        );
    }
    candidate
}

/// Resolve aliases above a declared root, leaving the root's own link lexical.
/// New cached registrations and explicit uncached queries call this once when
/// constructing their identity; warm registration never calls it.
fn registered_boundary(spelling: &Path, canonical: &Path) -> std::io::Result<RegisteredBoundary> {
    #[cfg(test)]
    BOUNDARY_READS.with(|count| count.set(count.get() + 1));
    let observed = match (spelling.parent(), spelling.file_name()) {
        (Some(parent), Some(leaf)) => std::fs::canonicalize(parent).map(|parent| parent.join(leaf)),
        _ => std::fs::canonicalize(spelling),
    };
    let path = crate::inspect::job::normalize_path(&observed?);
    let normalized = crate::inspect::job::normalize_path(spelling);
    // Lexical '..' cancellation is a direct spelling only when registration
    // proves it still reaches this root, rather than crossing a symlink parent.
    let normalized_spelling = if normalized != spelling {
        #[cfg(test)]
        ALIAS_VERIFICATIONS.with(|count| count.set(count.get() + 1));
        std::fs::canonicalize(&normalized)
            .is_ok_and(|path| crate::inspect::job::normalize_path(&path) == canonical)
            .then_some(normalized)
    } else {
        None
    };
    Ok(RegisteredBoundary {
        path,
        normalized_spelling,
    })
}

/// Cache an owner's root identity, never its cache-directory targets. Warm or
/// saturated registration is memory-only; explicit root queries still resolve
/// an uncached root without expanding the bounded cache.
pub(crate) fn register_storage_root(path: &Path) {
    let spelling = absolute_route(path);
    {
        let roots = storage_roots()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if roots.len() >= 128 || roots.iter().any(|root| root.spelling == spelling) {
            return;
        }
    }
    let _ = storage_root(path);
}

struct RouteProbe {
    path: PathBuf,
    prefixes: std::cell::OnceCell<Vec<(PathBuf, PathBuf)>>,
}

impl RouteProbe {
    fn new(path: &Path) -> Self {
        Self {
            path: absolute_route(path),
            prefixes: std::cell::OnceCell::new(),
        }
    }

    fn finish(&self, root: &StorageRoot, prefix: &Path) -> Option<PathBuf> {
        let relative = self.path.strip_prefix(prefix).ok()?;
        let key = crate::inspect::job::normalize_path(&root.canonical.join(relative));
        key.starts_with(&root.canonical).then_some(key)
    }

    fn direct(&self, root: &StorageRoot) -> Option<PathBuf> {
        if self.path.starts_with(&root.spelling) {
            return self.finish(root, &root.spelling);
        }
        if self.path.starts_with(&root.canonical) {
            return self.finish(root, &root.canonical);
        }
        let boundary = root.boundary.as_ref()?;
        if let Some(spelling) = boundary.normalized_spelling.as_ref() {
            if self.path.starts_with(spelling) {
                return self.finish(root, spelling);
            }
        }
        if self.path.starts_with(&boundary.path) {
            return self.finish(root, &boundary.path);
        }
        None
    }

    fn alias_could_outrank(
        &self,
        root: &StorageRoot,
        selected_key: &Path,
        remaining: usize,
    ) -> bool {
        let Some(boundary) = root.boundary.as_ref() else {
            return false;
        };
        let normalized = crate::inspect::job::normalize_path(&self.path);
        let spelling = crate::inspect::job::normalize_path(&root.spelling);
        let could_outrank = [normalized.as_path(), selected_key]
            .into_iter()
            .any(|route| {
                [
                    spelling.as_path(),
                    root.canonical.as_path(),
                    boundary.path.as_path(),
                ]
                .into_iter()
                .any(|prefix| {
                    route
                        .strip_prefix(prefix)
                        .is_ok_and(|relative| relative.components().count() < remaining)
                })
            });
        could_outrank
    }

    fn under(&self, root: &StorageRoot) -> Option<PathBuf> {
        if let Some(key) = self.direct(root) {
            return Some(key);
        }
        // Share ancestor probes across owner candidates, but never cache them
        // beyond this comparison. The matched prefix maps to the canonical root;
        // the child route stays lexical across relocated cache directories.
        let prefixes = self.prefixes.get_or_init(|| {
            let ancestors: Vec<_> = self.path.ancestors().collect();
            ancestors
                .into_iter()
                .rev()
                .map(|prefix| (prefix.to_path_buf(), canonical_scope(prefix)))
                .collect()
        });
        for (prefix, canonical) in prefixes {
            if *canonical == root.canonical {
                return self.finish(root, prefix);
            }
        }
        None
    }
}

fn route_under_root(path: &Path, root: &StorageRoot) -> Option<PathBuf> {
    RouteProbe::new(path).under(root)
}

fn owning_route(
    path: &Path,
    roots: &[std::sync::Arc<StorageRoot>],
) -> Option<(std::sync::Arc<StorageRoot>, PathBuf)> {
    let probe = RouteProbe::new(path);
    // A floor belongs to the logical namespace used to reach an artifact.
    // Direct lexical matches decide ownership unless a registered boundary can
    // be more specific on that normalized route. Thus /A/semantic -> /B remains
    // A's cache link, while B's own direct paths use B; only a registered nested
    // boundary may challenge A through aliases. With no direct owner, resolve
    // aliases normally so spellings above a storage root still match.
    // A deeper owner leaves fewer components below its boundary. Measure that
    // distance on the normalized comparison key, not raw alias spelling or the
    // unrelated depth of a relocated root's canonical target.
    let direct = roots
        .iter()
        .filter_map(|root| {
            let key = probe.direct(root)?;
            let remaining = key.strip_prefix(&root.canonical).ok()?.components().count();
            Some((remaining, root.clone(), key))
        })
        .min_by_key(|(remaining, _, _)| *remaining);
    if let Some(mut selected) = direct {
        loop {
            let projected = RouteProbe::new(&selected.2);
            let mut next = None;
            for root in roots {
                // Project the selected key (canonical root plus lexical child)
                // against other registered roots before probing aliases. This
                // joins /var and /private/var parent spellings without resolving
                // cache links below the owner.
                let key = projected.direct(root).or_else(|| {
                    if probe.direct(root).is_none()
                        && probe.alias_could_outrank(root, &selected.2, selected.0)
                    {
                        probe.under(root)
                    } else {
                        None
                    }
                });
                if let Some(key) = key {
                    let remaining = key.strip_prefix(&root.canonical).ok()?.components().count();
                    if remaining < selected.0
                        && next.as_ref().is_none_or(|(best, _, _)| remaining < *best)
                    {
                        next = Some((remaining, root.clone(), key));
                    }
                }
            }
            // Each step shortens the normalized child route, so projections
            // through multiple registered roots cannot cycle.
            let Some(more_specific) = next else {
                break;
            };
            selected = more_specific;
        }
        return Some((selected.1, selected.2));
    }
    roots
        .iter()
        .filter_map(|root| {
            let key = probe.under(root)?;
            let remaining = key.strip_prefix(&root.canonical).ok()?.components().count();
            Some((remaining, root.clone(), key))
        })
        .min_by_key(|(remaining, _, _)| *remaining)
        .map(|(_, root, key)| (root, key))
}

fn known_roots(
    entries: &[Entry],
    extra: Option<&std::sync::Arc<StorageRoot>>,
) -> Vec<std::sync::Arc<StorageRoot>> {
    let mut roots = storage_roots()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for root in entries
        .iter()
        .filter_map(|entry| entry.root.as_ref())
        .chain(extra)
    {
        if !roots
            .iter()
            .any(|known| known.spelling == root.spelling && known.canonical == root.canonical)
        {
            roots.push(root.clone());
        }
    }
    roots
}

#[cfg(test)]
fn comparison_scope(path: &Path) -> PathBuf {
    if let Some((_, key)) = owning_route(path, &known_roots(&[], None)) {
        return key;
    }
    // Without a registered owner, use the canonicalized path as the key;
    // never guess a storage boundary from a directory name or suffix.
    canonical_scope(path)
}

fn entry_route(
    entry: &Entry,
    roots: &[std::sync::Arc<StorageRoot>],
) -> (Option<std::sync::Arc<StorageRoot>>, PathBuf) {
    if !entry.refusal.source.is_artifact() {
        if let Some(root) = &entry.root {
            return (Some(root.clone()), root.canonical.clone());
        }
    }
    // Artifact ownership follows a newly registered, more specific root.
    // Floor evidence keeps the explicit owner stored when it was recorded.
    if let Some((root, key)) = owning_route(&entry.scope, roots) {
        return (Some(root), key);
    }
    (None, canonical_scope(&entry.scope))
}

fn same_owner(
    a: Option<&std::sync::Arc<StorageRoot>>,
    b: Option<&std::sync::Arc<StorageRoot>>,
) -> bool {
    a.map(|root| &root.canonical) == b.map(|root| &root.canonical)
}

fn entry_under_root(
    entry: &Entry,
    root: &StorageRoot,
    roots: &[std::sync::Arc<StorageRoot>],
) -> Option<PathBuf> {
    let (owner, key) = entry_route(entry, roots);
    owner
        .is_some_and(|owner| owner.canonical == root.canonical)
        .then_some(key)
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
    let floor_root = (!refusal.source.is_artifact()).then(|| storage_root(scope));
    let inserted = with_registry(|entries| {
        let roots = known_roots(entries, floor_root.as_ref());
        let (root, key) = if let Some(root) = &floor_root {
            (Some(root.clone()), root.canonical.clone())
        } else {
            owning_route(scope, &roots).map_or_else(
                || (None, canonical_scope(scope)),
                |(root, key)| (Some(root), key),
            )
        };
        if let Some(existing) = entries.iter_mut().find(|entry| {
            if entry.refusal.store != refusal.store {
                return false;
            }
            let (owner, entry_key) = entry_route(entry, &roots);
            same_owner(owner.as_ref(), root.as_ref()) && entry_key == key
        }) {
            let changed = existing.refusal != *refusal;
            existing.refusal = refusal.clone();
            existing.scope = scope.to_path_buf();
            existing.root = root;
            changed
        } else {
            entries.push(Entry {
                scope: scope.to_path_buf(),
                root,
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
/// wrote it, or removed by the operator). Floor refusals are retained until
/// a persistence retry safely rechecks the floor file.
pub fn clear(store: PersistedStore, scope: &Path) {
    with_registry(|entries| {
        let roots = known_roots(entries, None);
        let (owner, key) = owning_route(scope, &roots).map_or_else(
            || (None, canonical_scope(scope)),
            |(root, key)| (Some(root), key),
        );
        entries.retain(|entry| {
            if entry.refusal.store != store || !entry.refusal.source.is_artifact() {
                return true;
            }
            let (entry_owner, entry_key) = entry_route(entry, &roots);
            !(same_owner(entry_owner.as_ref(), owner.as_ref()) && entry_key == key)
        })
    });
}

/// Replace only this root's floor evidence after a successful floor recheck.
/// Artifact refusals still require their own format checks before clearing.
pub(crate) fn replace_floor_refusals(root: &Path, refusals: &[UnsupportedPersistedFormat]) {
    let identity = storage_root(root);
    with_registry(|entries| {
        let roots = known_roots(entries, Some(&identity));
        entries.retain(|entry| {
            entry_route(entry, &roots).1 != identity.canonical || entry.refusal.source.is_artifact()
        });
        entries.extend(refusals.iter().map(|refusal| Entry {
            scope: root.to_path_buf(),
            root: Some(identity.clone()),
            refusal: refusal.clone(),
        }));
    });
}

/// Forget artifact refusals under a swept directory only when the refused
/// path is confirmed absent. Permission errors and unreadable paths are not
/// evidence of removal, and a reader floor is never lowered by housekeeping.
pub fn clear_missing_artifacts(store: PersistedStore, scope: &Path) {
    with_registry(|entries| {
        // A swept directory is only a scope inside its existing owner. Making
        // it a new owner would let housekeeping bypass the storage-root floor.
        let roots = known_roots(entries, None);
        let (owner, key) = owning_route(scope, &roots).map_or_else(
            || (None, canonical_scope(scope)),
            |(root, key)| (Some(root), key),
        );
        entries.retain(|entry| {
            let (entry_owner, entry_key) = entry_route(entry, &roots);
            !(entry.refusal.store == store
                && entry.refusal.source.is_artifact()
                && same_owner(entry_owner.as_ref(), owner.as_ref())
                && entry_key.starts_with(&key)
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
        let roots = known_roots(entries, None);
        let (owner, key) = owning_route(path, &roots).map_or_else(
            || (None, canonical_scope(path)),
            |(root, key)| (Some(root), key),
        );
        entries
            .iter()
            .find(|entry| {
                if entry.refusal.store != store {
                    return false;
                }
                let (entry_owner, entry_key) = entry_route(entry, &roots);
                same_owner(entry_owner.as_ref(), owner.as_ref()) && key.starts_with(entry_key)
            })
            .map(|entry| entry.refusal.clone())
    })
}

/// Every refusal recorded at or below `root`, floor refusals first. Status
/// surfaces use this to list what the storage root holds that this build
/// cannot read.
pub fn refusals_under(root: &Path) -> Vec<UnsupportedPersistedFormat> {
    let root = storage_root(root);
    let mut found = with_registry(|entries| {
        let roots = known_roots(entries, Some(&root));
        entries
            .iter()
            .filter(|entry| entry_under_root(entry, &root, &roots).is_some())
            .map(|entry| entry.refusal.clone())
            .collect::<Vec<_>>()
    });
    found.sort_by_key(|refusal| (refusal.source.is_artifact(), refusal.store));
    found.dedup();
    found
}

/// Select refusals for the project's cached artifact key and shared storage.
/// The second result counts unknown owners without assigning their paths to
/// this project. No artifact reads or database queries are needed.
pub(crate) fn refusals_for_push(
    storage: &Path,
    artifact_key: Option<&str>,
) -> (Vec<UnsupportedPersistedFormat>, usize) {
    let root = storage_root(storage);
    let storage_key = &root.canonical;
    let mut owned = Vec::new();
    let mut unattributed = 0;
    let mut refusals = with_registry(|entries| {
        let roots = known_roots(entries, Some(&root));
        entries
            .iter()
            .filter_map(|entry| {
                entry_under_root(entry, &root, &roots).map(|scope| {
                    let artifact = route_under_root(&entry.refusal.path, &root).unwrap_or(scope);
                    (entry.refusal.clone(), artifact)
                })
            })
            .collect::<Vec<_>>()
    });
    refusals.sort_by(|a, b| {
        (
            a.0.store.name(),
            &a.0.path,
            a.0.source.sort_key(),
            a.0.found,
            a.0.supported,
        )
            .cmp(&(
                b.0.store.name(),
                &b.0.path,
                b.0.source.sort_key(),
                b.0.found,
                b.0.supported,
            ))
    });
    refusals.dedup_by(|a, b| a.0 == b.0);
    for (refusal, scope) in refusals {
        if refusal.source != RefusalSource::Artifact
            || (refusal.store == PersistedStore::AftDb && scope == storage_key.join("aft.db"))
        {
            owned.push(refusal);
            continue;
        }
        let plane = match refusal.store {
            PersistedStore::SearchIndex => Some("index"),
            PersistedStore::SemanticIndex | PersistedStore::SemanticSegment => Some("semantic"),
            PersistedStore::CallgraphStore => Some("callgraph"),
            PersistedStore::SymbolCache | PersistedStore::SymbolExtraction => Some("symbols"),
            PersistedStore::ArtifactOwner => Some("artifact-owners"),
            _ => None,
        };
        if let Some(plane) = plane {
            if let Some(key) = artifact_key {
                if scope.starts_with(storage_key.join(plane).join(key)) {
                    owned.push(refusal);
                }
            } else {
                unattributed += 1;
            }
        } else {
            unattributed += 1;
        }
    }
    (owned, unattributed)
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
    let floor = with_registry(|entries| {
        let roots = known_roots(entries, None);
        let owner = owning_route(scope, &roots)
            .or_else(|| owning_route(path, &roots))
            .map(|(root, _)| root);
        entries
            .iter()
            .find(|entry| {
                entry.refusal.store == store
                    && !entry.refusal.source.is_artifact()
                    && same_owner(entry.root.as_ref(), owner.as_ref())
                    && owner.is_some()
            })
            .map(|entry| entry.refusal.clone())
    });
    if let Some(refusal) = floor {
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
pub fn preflight_project_artifacts(storage_root: &Path, artifacts: &Path, project_key: &str) {
    register_storage_root(storage_root);
    let _ = crate::search_index::SearchIndex::check_disk_cache_format(
        &artifacts.join("index").join(project_key),
    );
    let _ = crate::semantic_index::SemanticIndex::check_disk_format(artifacts, project_key);
    let _ = crate::symbol_cache_disk::check_disk_format(artifacts, project_key);
    let _ = crate::callgraph_store::check_published_format(
        &artifacts.join("callgraph").join(project_key),
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

    fn isolated_owner_probe(test: &str) -> bool {
        const CHILD: &str = "AFT_TEST_DIRECT_OWNER_PROBES";
        if std::env::var(CHILD).as_deref() != Ok(test) {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env(CHILD, test)
                .env("RUSTC_WRAPPER", "")
                .env("CARGO_BUILD_RUSTC_WRAPPER", "")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated direct-owner probe failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"));
            return false;
        }
        true
    }

    #[test]
    fn direct_owner_gate_and_clear_do_zero_canonicalization_with_unrelated_roots() {
        if !isolated_owner_probe("persisted_format::tests::direct_owner_gate_and_clear_do_zero_canonicalization_with_unrelated_roots") { return; }
        let fixture = tempfile::tempdir().unwrap();
        let a = fixture.path().join("storage-a");
        let b = fixture.path().join("storage-b");
        std::fs::create_dir_all(a.join("semantic/key")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let path = a.join("semantic/key/cache.bin");
        std::fs::write(&path, b"artifact").unwrap();
        let roots = [storage_root(&a), storage_root(&b)];
        let store = PersistedStore::SemanticIndex;
        let floor = UnsupportedPersistedFormat::floor(store, a.join("reader-floor.json"), 99);
        record(&floor, &a);
        record(
            &UnsupportedPersistedFormat::floor(
                PersistedStore::AftDb,
                b.join("reader-floor.json"),
                99,
            ),
            &b,
        );
        record(
            &UnsupportedPersistedFormat::artifact(store, &path, 99),
            &path,
        );
        CANONICAL_PROBES.with(|count| count.set(0));
        let _ = canonical_scope(&path);
        CANONICAL_PROBES.with(|count| {
            assert_eq!(
                count.get(),
                1,
                "the canonicalization probe counter must observe real resolution"
            )
        });
        CANONICAL_PROBES.with(|count| count.set(0));
        let (owner, _) = owning_route(&path, &roots).unwrap();
        assert_eq!(owner.canonical, roots[0].canonical);
        CANONICAL_PROBES.with(|count| assert_eq!(count.get(), 0, "an unrelated registered root must not trigger ancestor resolution for a direct owner"));
        assert_eq!(gate(store, &path, &path, Some(1)), Err(floor.clone()));
        CANONICAL_PROBES.with(|count| {
            assert_eq!(
                count.get(),
                0,
                "a direct floor gate must stay on the lexical fast path"
            )
        });
        clear(store, &path);
        CANONICAL_PROBES.with(|count| {
            assert_eq!(
                count.get(),
                0,
                "clearing a direct artifact must stay on the lexical fast path"
            )
        });
        assert_eq!(refusal_covering(store, &path), Some(floor));
        let missing = a.join("missing-parent/inner");
        BOUNDARY_READS.with(|count| count.set(0));
        let unavailable = storage_root(&missing);
        assert!(unavailable.boundary.is_none());
        BOUNDARY_READS.with(|count| assert_eq!(count.get(), 1));
        let before = CANONICAL_PROBES.with(|count| count.get());
        let same = storage_root(&missing);
        assert!(std::sync::Arc::ptr_eq(&unavailable, &same));
        register_storage_root(&missing);
        BOUNDARY_READS.with(|count| {
            assert_eq!(
                count.get(),
                1,
                "failed boundary metadata must not be retried on warm registration"
            )
        });
        CANONICAL_PROBES.with(|count| assert_eq!(count.get(), before));
        let indirect = RouteProbe::new(&missing.join("artifact"));
        assert!(!indirect.alias_could_outrank(
            &unavailable,
            &a.join("missing-parent/inner/artifact"),
            3
        ));
    }

    #[test]
    fn an_unregistered_link_to_an_unrelated_registered_root_keeps_its_logical_floor() {
        let fixture = tempfile::tempdir().unwrap();
        let a = fixture.path().join("storage-a");
        let b = fixture.path().join("storage-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(b.join("key")).unwrap();
        directory_alias(&b, &a.join("semantic"));
        let through_a = a.join("semantic/key/cache.bin");
        let through_b = b.join("key/cache.bin");
        std::fs::write(&through_b, b"artifact").unwrap();
        let store = PersistedStore::SemanticIndex;
        let a_floor = UnsupportedPersistedFormat::floor(store, a.join("reader-floor.json"), 99);
        let b_floor = UnsupportedPersistedFormat::floor(store, b.join("reader-floor.json"), 100);
        record(&a_floor, &a);
        record(&b_floor, &b);
        assert_eq!(
            gate(store, &through_a, &through_a, Some(1)),
            Err(a_floor.clone())
        );
        assert_eq!(
            gate(store, &through_b, &through_b, Some(1)),
            Err(b_floor.clone())
        );
        assert_eq!(refusal_covering(store, &through_a), Some(a_floor.clone()));
        assert_eq!(refusal_covering(store, &through_b), Some(b_floor.clone()));
        assert_eq!(refusals_under(&a), vec![a_floor]);
        assert_eq!(refusals_under(&b), vec![b_floor]);
    }

    #[test]
    fn a_more_specific_parent_alias_boundary_beats_a_direct_outer_match() {
        let (fixture, outer, inner, artifact) = nested_storage_roots();
        let alias_parent = fixture.path().join("another-parent-alias");
        directory_alias(&std::fs::canonicalize(&outer).unwrap(), &alias_parent);
        let alias_inner = alias_parent.join("inner");
        let store = PersistedStore::SemanticIndex;
        let outer_floor =
            UnsupportedPersistedFormat::floor(store, outer.join("reader-floor.json"), 99);
        let inner_floor =
            UnsupportedPersistedFormat::floor(store, alias_inner.join("reader-floor.json"), 100);
        record(&outer_floor, &outer);
        record(&inner_floor, &alias_inner);
        assert_eq!(
            gate(store, &artifact, &artifact, Some(1)),
            Err(inner_floor.clone())
        );
        assert_eq!(refusals_under(&inner), vec![inner_floor]);
        assert_eq!(refusals_under(&outer), vec![outer_floor]);
    }

    #[test]
    fn uncached_nested_floor_queries_do_no_resolution_after_their_explicit_root_probe() {
        if !isolated_owner_probe("persisted_format::tests::uncached_nested_floor_queries_do_no_resolution_after_their_explicit_root_probe") { return; }
        uncached_nested_floor_probe(nested_storage_roots());
    }

    #[test]
    fn uncached_roots_behind_a_symlinked_parent_do_no_post_query_resolution() {
        if !isolated_owner_probe("persisted_format::tests::uncached_roots_behind_a_symlinked_parent_do_no_post_query_resolution") { return; }
        let fixture = tempfile::tempdir().unwrap();
        let backing = fixture.path().join("real-parent");
        let parent_alias = fixture.path().join("parent-alias");
        std::fs::create_dir_all(backing.join("outer/padding")).unwrap();
        std::fs::create_dir_all(backing.join("other-disk/semantic/key")).unwrap();
        directory_alias(&backing, &parent_alias);
        directory_alias(&backing.join("other-disk"), &backing.join("outer/inner"));
        let outer = parent_alias.join("outer/padding/..");
        let inner = parent_alias.join("outer/inner");
        let artifact = inner.join("semantic/key/cache.bin");
        std::fs::write(&artifact, b"artifact").unwrap();
        uncached_nested_floor_probe((fixture, outer, inner, artifact));
    }

    #[test]
    fn normalized_registered_spellings_must_reach_the_same_root() {
        let fixture = tempfile::tempdir().unwrap();
        let a = fixture.path().join("storage-a");
        let b = fixture.path().join("storage-b");
        std::fs::create_dir_all(a.join("semantic/key")).unwrap();
        std::fs::create_dir_all(b.join("child")).unwrap();
        directory_alias(&b.join("child"), &a.join("link"));
        let through_parent = a.join("link/..");
        let identity = storage_root(&through_parent);
        let path = a.join("semantic/key/cache.bin");
        std::fs::write(&path, b"artifact").unwrap();
        let store = PersistedStore::SemanticIndex;
        if identity.canonical == canonical_scope(&a) {
            // Some path APIs collapse junction parents before following links.
            // In that case the normalized spelling really is the same root.
            assert_eq!(
                identity
                    .boundary
                    .as_ref()
                    .unwrap()
                    .normalized_spelling
                    .as_ref(),
                Some(&crate::inspect::job::normalize_path(&through_parent))
            );
            let floor = UnsupportedPersistedFormat::floor(
                store,
                through_parent.join("reader-floor.json"),
                99,
            );
            record(&floor, &through_parent);
            assert_eq!(gate(store, &path, &path, Some(1)), Err(floor));
            return;
        }
        assert_eq!(identity.canonical, canonical_scope(&b));
        assert!(
            identity
                .boundary
                .as_ref()
                .unwrap()
                .normalized_spelling
                .is_none(),
            "lexically cancelling a symlink parent must not invent a direct alias for another root"
        );
        let b_floor =
            UnsupportedPersistedFormat::floor(store, through_parent.join("reader-floor.json"), 100);
        let a_floor = UnsupportedPersistedFormat::floor(store, a.join("reader-floor.json"), 99);
        record(&b_floor, &through_parent);
        record(&a_floor, &a);
        assert_eq!(gate(store, &path, &path, Some(1)), Err(a_floor));
    }

    #[test]
    fn cached_roots_behind_a_symlinked_parent_match_both_spellings_without_probes() {
        if !isolated_owner_probe("persisted_format::tests::cached_roots_behind_a_symlinked_parent_match_both_spellings_without_probes") { return; }
        let fixture = tempfile::tempdir().unwrap();
        let backing = fixture.path().join("real-parent");
        let alias = fixture.path().join("parent-alias");
        std::fs::create_dir_all(backing.join("storage/padding")).unwrap();
        std::fs::create_dir_all(backing.join("storage/semantic/key")).unwrap();
        directory_alias(&backing, &alias);
        let registered = alias.join("storage/padding/..");
        let artifact = alias.join("storage/semantic/key/cache.bin");
        std::fs::write(&artifact, b"artifact").unwrap();
        let canonical = std::fs::canonicalize(&artifact).unwrap();
        let floor = UnsupportedPersistedFormat::floor(
            PersistedStore::SemanticIndex,
            registered.join("reader-floor.json"),
            99,
        );
        record(&floor, &registered);
        let before = (
            CANONICAL_PROBES.with(|count| count.get()),
            BOUNDARY_READS.with(|count| count.get()),
        );
        let aliases_before = ALIAS_VERIFICATIONS.with(|count| count.get());
        assert_eq!(aliases_before, 1);
        register_storage_root(&registered);
        for path in [&artifact, &canonical] {
            assert_eq!(
                gate(PersistedStore::SemanticIndex, path, path, Some(1)),
                Err(floor.clone())
            );
            assert_eq!(
                refusal_covering(PersistedStore::SemanticIndex, path),
                Some(floor.clone())
            );
        }
        let after = (
            CANONICAL_PROBES.with(|count| count.get()),
            BOUNDARY_READS.with(|count| count.get()),
        );
        assert_eq!(
            after, before,
            "cached aliases must be normalized before ordinary comparisons"
        );
        ALIAS_VERIFICATIONS.with(|count| assert_eq!(count.get(), aliases_before));
        assert_eq!(floor.path, registered.join("reader-floor.json"));
    }

    fn uncached_nested_floor_probe(
        (fixture, outer, inner, _artifact): (tempfile::TempDir, PathBuf, PathBuf, PathBuf),
    ) {
        for index in 0..140 {
            register_storage_root(&fixture.path().join(format!("registered-{index}")));
        }
        assert_eq!(storage_roots().read().unwrap().len(), 128);
        let alias_parent = fixture.path().join("another-parent-alias");
        directory_alias(&std::fs::canonicalize(&outer).unwrap(), &alias_parent);
        let alias_inner = alias_parent.join("inner");
        let direct_artifact = inner.join("semantic/key/cache.bin");
        CANONICAL_PROBES.with(|count| count.set(0));
        BOUNDARY_READS.with(|count| count.set(0));
        ALIAS_VERIFICATIONS.with(|count| count.set(0));
        register_storage_root(&outer);
        register_storage_root(&alias_inner);
        CANONICAL_PROBES.with(|count| assert_eq!(count.get(), 0));
        BOUNDARY_READS.with(|count| assert_eq!(count.get(), 0));
        let store = PersistedStore::SemanticIndex;
        let outer_floor =
            UnsupportedPersistedFormat::floor(store, outer.join("reader-floor.json"), 99);
        let inner_floor =
            UnsupportedPersistedFormat::floor(store, alias_inner.join("reader-floor.json"), 100);
        record(&outer_floor, &outer);
        record(&inner_floor, &alias_inner);
        let before = (
            CANONICAL_PROBES.with(|count| count.get()),
            BOUNDARY_READS.with(|count| count.get()),
        );
        let aliases_before = ALIAS_VERIFICATIONS.with(|count| count.get());
        assert_eq!(
            aliases_before, 1,
            "the equivalent normalized root spelling is verified once during identity setup"
        );
        assert_eq!(
            before,
            (2, 2),
            "each explicit uncached identity resolves its root and boundary once"
        );
        assert_eq!(
            gate(store, &direct_artifact, &direct_artifact, Some(1)),
            Err(inner_floor)
        );
        clear(store, &direct_artifact);
        let after = (
            CANONICAL_PROBES.with(|count| count.get()),
            BOUNDARY_READS.with(|count| count.get()),
        );
        eprintln!(
            "uncached artifact route: {direct_artifact:?}; owner identities: {:?}",
            with_registry(|entries| entries
                .iter()
                .filter_map(|entry| entry.root.clone())
                .collect::<Vec<_>>())
        );
        assert_eq!(
            after, before,
            "beyond-cap gate/clear must not add I/O after the explicit query probes"
        );
        ALIAS_VERIFICATIONS.with(|count| assert_eq!(count.get(), aliases_before));
        assert_eq!(storage_roots().read().unwrap().len(), 128);
    }

    #[test]
    fn cleanup_scope_does_not_become_a_storage_owner_or_bypass_its_floor() {
        let fixture = tempfile::tempdir().unwrap();
        let tasks = fixture.path().join("bash-tasks");
        let artifact = tasks.join("session/task/control/metadata.json");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, b"readable task metadata").unwrap();
        let store = PersistedStore::BashTask;
        let floor = UnsupportedPersistedFormat::floor(
            store,
            fixture.path().join("reader-floor.json"),
            u64::from(store.supported()) + 1,
        );
        record(&floor, fixture.path());
        let missing = tasks.join("session/missing/control/metadata.json");
        record(
            &UnsupportedPersistedFormat::artifact(store, &missing, 99),
            &missing,
        );
        ROOT_RESOLUTIONS.with(|count| count.set(0));
        clear_missing_artifacts(store, &tasks);
        ROOT_RESOLUTIONS.with(|count| {
            assert_eq!(
                count.get(),
                0,
                "cleanup must not resolve and register a new owner"
            )
        });
        assert_eq!(
            gate(
                store,
                &artifact,
                &artifact,
                Some(u64::from(store.supported()))
            ),
            Err(floor.clone())
        );
        assert_eq!(refusal_covering(store, &artifact), Some(floor));
        assert_eq!(
            refusals_under(fixture.path()).len(),
            1,
            "cleanup must still remove a confirmed missing artifact refusal"
        );
    }

    fn directory_alias(target: &Path, alias: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, alias).unwrap();
        #[cfg(windows)]
        {
            // Rebuild both paths from their components so every separator is a
            // backslash: `mklink` reads a `/inner` segment as an unknown switch.
            let alias: std::path::PathBuf = alias.components().collect();
            let target: std::path::PathBuf = target.components().collect();
            let output = std::process::Command::new("cmd.exe")
                .args(["/c", "mklink", "/J"])
                .arg(&alias)
                .arg(&target)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "junction creation failed: {output:?}"
            );
        }
    }

    fn relocated_artifact(namespace: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let fixture = tempfile::tempdir().unwrap();
        let parent = fixture.path().join("parent");
        let storage = parent.join("storage");
        let outside = fixture.path().join("another-disk");
        std::fs::create_dir_all(storage.join("nested")).unwrap();
        std::fs::create_dir_all(outside.join("key")).unwrap();
        directory_alias(&outside, &storage.join(namespace));
        let alias = fixture.path().join("parent-alias");
        directory_alias(&parent, &alias);
        let reached_root = alias.join("storage/nested/..");
        let artifact = reached_root.join(namespace).join("key/cache.bin");
        std::fs::write(&artifact, b"newer artifact").unwrap();
        assert!(!std::fs::canonicalize(&artifact)
            .unwrap()
            .starts_with(std::fs::canonicalize(&storage).unwrap()));
        (fixture, reached_root, artifact)
    }

    fn nested_storage_roots() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let fixture = tempfile::tempdir().unwrap();
        let outer = fixture.path().join("outer");
        let elsewhere = fixture.path().join("other-disk");
        std::fs::create_dir_all(outer.join("padding")).unwrap();
        std::fs::create_dir_all(elsewhere.join("semantic/key")).unwrap();
        let inner = outer.join("inner");
        directory_alias(&elsewhere, &inner);
        let outer = outer.join("padding/..");
        let artifact = outer.join("inner/semantic/key/cache.bin");
        std::fs::write(&artifact, b"newer artifact").unwrap();
        (fixture, outer, inner, artifact)
    }

    #[test]
    fn nested_roots_choose_the_inner_floor_on_the_normalized_route() {
        let (_fixture, outer, inner, artifact) = nested_storage_roots();
        let store = PersistedStore::SemanticIndex;
        let outer_floor =
            UnsupportedPersistedFormat::floor(store, outer.join("reader-floor.json"), 99);
        let inner_floor =
            UnsupportedPersistedFormat::floor(store, inner.join("reader-floor.json"), 100);
        record(&outer_floor, &outer);
        record(&inner_floor, &inner);
        for path in [
            &artifact,
            &inner.join("semantic/key/cache.bin"),
            &std::fs::canonicalize(&artifact).unwrap(),
        ] {
            assert_eq!(
                gate(store, path, path, Some(1)),
                Err(inner_floor.clone()),
                "the longest normalized owner boundary must select the inner floor"
            );
            assert_eq!(refusal_covering(store, path), Some(inner_floor.clone()));
        }
        assert_eq!(refusals_under(&inner), vec![inner_floor.clone()]);
        assert_eq!(refusals_under(&outer), vec![outer_floor.clone()]);
        assert_eq!(
            refusals_for_push(&inner, Some("key")),
            (vec![inner_floor], 0)
        );
        assert_eq!(
            refusals_for_push(&outer, Some("key")),
            (vec![outer_floor], 0)
        );
    }

    #[test]
    fn nested_roots_keep_inner_artifact_refusals_out_of_the_outer_inventory() {
        let (_fixture, outer, inner, artifact) = nested_storage_roots();
        let outer_floor = UnsupportedPersistedFormat::floor(
            PersistedStore::AftDb,
            outer.join("reader-floor.json"),
            99,
        );
        let inner_floor = UnsupportedPersistedFormat::floor(
            PersistedStore::AftDb,
            inner.join("reader-floor.json"),
            100,
        );
        record(&outer_floor, &outer);
        record(&inner_floor, &inner);
        let sibling = outer.join("semantic/key/cache.bin");
        std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        std::fs::write(&sibling, b"outer artifact").unwrap();
        let outer_refusal =
            UnsupportedPersistedFormat::artifact(PersistedStore::SemanticIndex, &sibling, 99);
        record(&outer_refusal, &sibling);
        let refusal =
            UnsupportedPersistedFormat::artifact(PersistedStore::SemanticIndex, &artifact, 99);
        record(&refusal, &artifact);
        let owned = refusals_under(&inner);
        assert!(
            owned.contains(&refusal),
            "the inner root must retain the refusal recorded through its nested link"
        );
        assert!(owned.contains(&inner_floor));
        assert_eq!(owned.len(), 2);
        let outer_owned = refusals_under(&outer);
        assert!(outer_owned.contains(&outer_floor) && outer_owned.contains(&outer_refusal));
        assert_eq!(outer_owned.len(), 2);
        let (pushed, unknown) = refusals_for_push(&inner, Some("key"));
        assert_eq!(unknown, 0);
        assert!(pushed.contains(&refusal) && pushed.contains(&inner_floor));
        assert_eq!(pushed.len(), 2);
        let (outer_pushed, unknown) = refusals_for_push(&outer, Some("key"));
        assert_eq!(unknown, 0);
        assert!(outer_pushed.contains(&outer_floor) && outer_pushed.contains(&outer_refusal));
        assert_eq!(outer_pushed.len(), 2);
        let lookup = refusal_covering(
            PersistedStore::SemanticIndex,
            &std::fs::canonicalize(&artifact).unwrap(),
        )
        .unwrap();
        assert_eq!(lookup.path, artifact);
        clear(
            PersistedStore::SemanticIndex,
            &inner.join("semantic/key/cache.bin"),
        );
        assert_eq!(refusals_under(&inner), vec![inner_floor]);
    }

    #[test]
    fn registering_an_inner_root_reattributes_an_existing_artifact_refusal() {
        let (_fixture, outer, inner, artifact) = nested_storage_roots();
        let outer_floor = UnsupportedPersistedFormat::floor(
            PersistedStore::AftDb,
            outer.join("reader-floor.json"),
            99,
        );
        record(&outer_floor, &outer);
        let refusal =
            UnsupportedPersistedFormat::artifact(PersistedStore::SemanticIndex, &artifact, 99);
        record(&refusal, &artifact);
        assert!(refusals_under(&outer).contains(&refusal));
        let inner_floor = UnsupportedPersistedFormat::floor(
            PersistedStore::AftDb,
            inner.join("reader-floor.json"),
            100,
        );
        record(&inner_floor, &inner);
        assert!(refusals_under(&inner).contains(&refusal), "an earlier outer-root capture must not hide a refusal after an inner root is registered");
        assert_eq!(refusals_under(&outer), vec![outer_floor]);
    }

    #[test]
    fn an_unregistered_inner_link_keeps_the_outer_root_owner() {
        let (_fixture, outer, _inner, artifact) = nested_storage_roots();
        let floor = UnsupportedPersistedFormat::floor(
            PersistedStore::SemanticIndex,
            outer.join("reader-floor.json"),
            99,
        );
        record(&floor, &outer);
        assert_eq!(
            gate(PersistedStore::SemanticIndex, &artifact, &artifact, Some(1)),
            Err(floor.clone())
        );
        assert_eq!(
            refusal_covering(PersistedStore::SemanticIndex, &artifact),
            Some(floor.clone())
        );
        let refusal =
            UnsupportedPersistedFormat::artifact(PersistedStore::SemanticIndex, &artifact, 99);
        record(&refusal, &artifact);
        let owned = refusals_under(&outer);
        assert!(owned.contains(&floor) && owned.contains(&refusal));
        assert_eq!(owned.len(), 2);
        let (pushed, unknown) = refusals_for_push(&outer, None);
        assert_eq!(unknown, 1);
        assert_eq!(pushed, vec![floor]);
    }

    #[test]
    fn physically_nested_roots_have_separate_floor_and_inventory_owners() {
        let fixture = tempfile::tempdir().unwrap();
        let outer = fixture.path().join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(inner.join("semantic/key")).unwrap();
        let artifact = inner.join("semantic/key/cache.bin");
        std::fs::write(&artifact, b"newer artifact").unwrap();
        let store = PersistedStore::SemanticIndex;
        let outer_floor =
            UnsupportedPersistedFormat::floor(store, outer.join("reader-floor.json"), 99);
        let inner_floor =
            UnsupportedPersistedFormat::floor(store, inner.join("reader-floor.json"), 100);
        record(&outer_floor, &outer);
        record(&inner_floor, &inner);
        assert_eq!(
            gate(store, &artifact, &artifact, Some(1)),
            Err(inner_floor.clone())
        );
        let refusal = UnsupportedPersistedFormat::artifact(store, &artifact, 99);
        record(&refusal, &artifact);
        assert_eq!(refusals_under(&outer), vec![outer_floor]);
        let owned = refusals_under(&inner);
        assert!(owned.contains(&inner_floor) && owned.contains(&refusal));
        assert_eq!(owned.len(), 2);
    }

    #[test]
    fn reader_floor_covers_artifacts_in_relocated_storage_subdirectories() {
        for (namespace, store) in [
            ("semantic", PersistedStore::SemanticIndex),
            ("index", PersistedStore::SearchIndex),
        ] {
            let (_fixture, storage, artifact) = relocated_artifact(namespace);
            let root = std::fs::canonicalize(&storage).unwrap();
            let floor =
                UnsupportedPersistedFormat::floor(store, storage.join("reader-floor.json"), 99);
            record(&floor, &root);
            assert_eq!(
                gate(store, &artifact, &artifact, Some(1)),
                Err(floor.clone()),
                "the storage owner's floor must cover {namespace} even across a mount alias"
            );
            assert_eq!(refusal_covering(store, &artifact), Some(floor.clone()));
            assert_eq!(refusals_under(&storage), vec![floor.clone()]);
            assert_eq!(refusals_for_push(&storage, Some("key")), (vec![floor], 0));
        }
    }

    #[test]
    fn relocated_artifact_refusals_remain_owned_by_the_storage_route() {
        for (namespace, store) in [
            ("semantic", PersistedStore::SemanticIndex),
            ("index", PersistedStore::SearchIndex),
        ] {
            let (_fixture, storage, artifact) = relocated_artifact(namespace);
            let refusal = UnsupportedPersistedFormat::artifact(store, &artifact, 99);
            record(&refusal, &artifact);
            assert_eq!(
                refusals_under(&storage),
                vec![refusal.clone()],
                "a relocated {namespace} artifact must stay in its owner's inventory"
            );
            assert_eq!(
                refusals_for_push(&storage, Some("key")),
                (vec![refusal.clone()], 0)
            );
            assert_eq!(refusal_covering(store, &artifact), Some(refusal.clone()));
            assert_eq!(refusals_under(&storage)[0].path, artifact);
            clear(store, &artifact);
            assert!(refusals_under(&storage).is_empty());
        }
    }

    #[test]
    fn unregistered_artifacts_keep_the_previous_resolved_identity() {
        let (_fixture, _storage, artifact) = relocated_artifact("semantic");
        assert!(owning_route(&artifact, &known_roots(&[], None)).is_none());
        assert_eq!(comparison_scope(&artifact), canonical_scope(&artifact));
        assert_eq!(
            comparison_scope(&artifact),
            crate::inspect::job::normalize_path(&std::fs::canonicalize(&artifact).unwrap())
        );
    }

    #[test]
    fn bounded_root_registration_has_no_resolution_work_after_saturation() {
        let fixture = tempfile::tempdir().unwrap();
        for index in 0..128 {
            register_storage_root(&fixture.path().join(format!("root-{index}")));
        }
        assert!(storage_roots().read().unwrap().len() <= 128);
        ROOT_RESOLUTIONS.with(|count| count.set(0));
        let uncached = fixture.path().join("uncached");
        let _ = storage_root(&uncached);
        assert!(storage_roots().read().unwrap().len() <= 128);
        ROOT_RESOLUTIONS.with(|count| {
            assert_eq!(
                count.get(),
                1,
                "the root-resolution counter must observe an explicit uncached query"
            )
        });
        ROOT_RESOLUTIONS.with(|count| count.set(0));
        register_storage_root(&uncached);
        register_storage_root(&uncached);
        ROOT_RESOLUTIONS.with(|count| {
            assert_eq!(
                count.get(),
                0,
                "the saturated registration hook must stay memory-only"
            )
        });
        let (_relocated, storage, artifact) = relocated_artifact("semantic");
        let floor = UnsupportedPersistedFormat::floor(
            PersistedStore::SemanticIndex,
            storage.join("reader-floor.json"),
            99,
        );
        record(&floor, &storage);
        assert_eq!(
            gate(PersistedStore::SemanticIndex, &artifact, &artifact, Some(1)),
            Err(floor)
        );
    }

    #[test]
    fn refusal_comparisons_resolve_parent_aliases_without_rewriting_returned_paths() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let alias = root.path().join("alias");
        std::fs::create_dir_all(target.join("nested")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd.exe")
                .args(["/c", "mklink", "/J"])
                .arg(&alias)
                .arg(&target)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "junction creation failed: {output:?}"
            );
        }
        let unresolved = alias.join("nested").join("..").join("index/key/cache.bin");
        let canonical_root = std::fs::canonicalize(&target).unwrap();
        let resolved = canonical_root.join("index/key/cache.bin");
        let store = PersistedStore::SearchIndex;
        let refusal = UnsupportedPersistedFormat::incompatible(
            store,
            &resolved,
            1,
            IncompatibleReason::SchemaDrift,
        );
        record(&refusal, &resolved);
        assert_eq!(refusal_covering(store, &unresolved), Some(refusal.clone()));
        assert_eq!(refusals_under(&alias), vec![refusal.clone()]);
        assert_eq!(
            refusals_for_push(&alias, Some("key")),
            (vec![refusal.clone()], 0)
        );
        assert_eq!(refusal_covering(store, &unresolved).unwrap().path, resolved);
        clear(store, &unresolved);
        assert!(refusal_covering(store, &resolved).is_none());

        let artifact = UnsupportedPersistedFormat::artifact(store, &unresolved, 99);
        record(&artifact, &unresolved);
        assert_eq!(
            refusals_for_push(&alias, Some("key")),
            (vec![artifact.clone()], 0)
        );
        assert_eq!(refusals_for_push(&canonical_root, None), (vec![], 1));
        assert_eq!(refusal_covering(store, &resolved), Some(artifact.clone()));
        assert_eq!(refusal_covering(store, &resolved).unwrap().path, unresolved);
        clear_missing_artifacts(store, &canonical_root);
        assert!(refusal_covering(store, &resolved).is_none());

        let floor =
            UnsupportedPersistedFormat::floor(store, canonical_root.join("reader-floor.json"), 99);
        record(&floor, &canonical_root);
        assert_eq!(gate(store, &unresolved, &unresolved, Some(1)), Err(floor));
        replace_floor_refusals(&alias, &[]);
        assert!(refusal_covering(store, &unresolved).is_none());
    }

    #[test]
    fn push_refusal_source_order_preserves_existing_variants() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("aft.db");
        let expected = [
            RefusalSource::Artifact,
            RefusalSource::Floor,
            RefusalSource::FloorUnreadable,
            RefusalSource::Incompatible(IncompatibleReason::SchemaDrift),
        ];
        with_registry(|entries| {
            for (index, source) in expected.into_iter().rev().enumerate() {
                let mut refusal =
                    UnsupportedPersistedFormat::artifact(PersistedStore::AftDb, &path, 99);
                refusal.source = source;
                entries.push(Entry {
                    scope: root.path().join(format!("scope-{index}")),
                    root: None,
                    refusal,
                });
            }
        });
        let (refusals, unknown) = refusals_for_push(root.path(), None);
        assert_eq!(unknown, 0);
        assert_eq!(
            refusals
                .iter()
                .map(|refusal| refusal.source)
                .collect::<Vec<_>>(),
            expected
        );
        let root_key = comparison_scope(root.path());
        with_registry(|entries| {
            let roots = known_roots(entries, None);
            entries.retain(|entry| !entry_route(entry, &roots).1.starts_with(&root_key));
        });
    }

    #[test]
    fn incompatible_refusals_follow_artifact_lifecycle_not_floor_lifecycle() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("members.sqlite");
        let store = PersistedStore::ViewsV3Registry;
        std::fs::write(&path, b"drifted").unwrap();
        let incompatible = UnsupportedPersistedFormat::incompatible(
            store,
            &path,
            1,
            IncompatibleReason::SchemaDrift,
        );
        let message = incompatible.to_string();
        assert_eq!(message, format!("{CODE}: store views_v3_registry at {} is incompatible with this build: schema_drift",path.display()));
        assert!(!message.contains("restart"));
        record(&incompatible, &path);
        assert!(gate(store, &path, &path, Some(1)).is_ok());
        assert!(refusal_covering(store, &path).is_none());
        record(&incompatible, &path);
        std::fs::remove_file(&path).unwrap();
        clear_missing_artifacts(store, root.path());
        assert!(refusal_covering(store, &path).is_none());
        let floor =
            UnsupportedPersistedFormat::floor(store, root.path().join("reader-floor.json"), 2);
        record(&incompatible, &path);
        record(&floor, root.path());
        assert_eq!(gate(store, &path, &path, Some(1)), Err(floor.clone()));
        clear(store, &path);
        assert_eq!(refusal_covering(store, &path), Some(floor));
        assert!(UnsupportedPersistedFormat::is_refusal_message(&message));
    }

    #[test]
    fn artifact_refusal_cannot_shadow_reader_floor() {
        let root = tempfile::tempdir().unwrap();
        let artifact = root.path().join("index/k/cache.bin");
        let store = PersistedStore::SearchIndex;
        let current = u64::from(store.supported());
        assert!(gate(store, &artifact, &artifact, Some(current + 1)).is_err());
        let floor = UnsupportedPersistedFormat::floor(
            store,
            root.path().join(crate::reader_floor::FLOOR_FILE),
            current + 2,
        );
        record(&floor, root.path());
        assert_eq!(gate(store, &artifact, &artifact, Some(current)), Err(floor));
        assert_eq!(
            refusals_under(root.path()).len(),
            2,
            "floor gate must not clear artifact evidence"
        );
    }

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
    fn floor_refusal_without_persistence_retry_requires_restart() {
        let floor = Path::new("storage/reader-floor.json");
        for store in PersistedStore::ALL {
            for refusal in [
                UnsupportedPersistedFormat::floor(store, floor, 99),
                UnsupportedPersistedFormat::floor_unreadable(store, floor, 0),
                UnsupportedPersistedFormat::floor_unreadable(store, floor, 99),
            ] {
                assert_eq!(
                    refusal.to_string().contains("restart AFT"),
                    store != PersistedStore::AftDb
                );
            }
        }
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
