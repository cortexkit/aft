//! The common reader for an already selected, pinned publication.

use std::path::PathBuf;
use std::sync::Arc;

use crate::callgraph_store::{ReadonlyCallGraphStore, Result};
use crate::pins::QueryPin;

// Keep the first refusal on the calling test thread so a failed assertion names
// the branch and its observed values without rerunning a potentially racy read.
#[cfg(test)]
thread_local! {
    static CHECKOUT_VIEW_REFUSAL: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn record_checkout_view_refusal(reason: String) {
    CHECKOUT_VIEW_REFUSAL.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(reason);
        }
    });
}

#[cfg(test)]
pub(crate) fn take_checkout_view_refusal() -> Option<String> {
    CHECKOUT_VIEW_REFUSAL.with(|slot| slot.borrow_mut().take())
}

macro_rules! note_checkout_verification_refusal {
    ($($arg:tt)*) => {
        #[cfg(test)]
        record_checkout_view_refusal(format!($($arg)*));
    };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VerificationIo {
    pub files_statd: usize,
    pub files_read: usize,
    pub bytes_hashed: usize,
}

#[cfg(test)]
thread_local! {
    static VERIFICATION_IO: std::cell::Cell<VerificationIo> = const {
        std::cell::Cell::new(VerificationIo { files_statd: 0, files_read: 0, bytes_hashed: 0 })
    };
}

#[cfg(test)]
pub(crate) fn take_verification_io() -> VerificationIo {
    VERIFICATION_IO.with(|io| io.replace(VerificationIo::default()))
}

fn read_verification_source(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let source = crate::project_content::read(path)?;
    #[cfg(test)]
    VERIFICATION_IO.with(|io| {
        let mut count = io.get();
        count.files_read += 1;
        io.set(count);
    });
    Ok(source)
}

fn verification_key(source: &[u8], language: String, producer: &str) -> String {
    #[cfg(test)]
    VERIFICATION_IO.with(|io| {
        let mut count = io.get();
        count.bytes_hashed += source.len();
        io.set(count);
    });
    crate::blob_store::CallgraphKey::from_bytes(source, language, producer)
        .full_key()
        .to_hex()
}

#[derive(Clone)]
pub(crate) struct VerifiedCallgraphFile {
    size: u64,
    modified: std::time::SystemTime,
    expected_key: String,
}

fn verification_stat(path: &std::path::Path) -> std::io::Result<(u64, std::time::SystemTime)> {
    #[cfg(test)]
    VERIFICATION_IO.with(|io| {
        let mut count = io.get();
        count.files_statd += 1;
        io.set(count);
    });
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.len(), metadata.modified()?))
}

/// Stat-first verification of an immutable manifest. The owner clears `verified`
/// on the same watcher invalidation ticket used by search verification, so an
/// event always rechecks content even if a writer preserved size and mtime.
/// Blocking inspect can reuse its already collected root stats instead of
/// issuing another metadata walk just for this reader.
pub(crate) fn callgraph_paths_match_cached(
    manifest: &super::Manifest,
    root: &std::path::Path,
    paths: &[PathBuf],
    observed: Option<&[(PathBuf, u64, std::time::SystemTime)]>,
    verified: &mut std::collections::BTreeMap<PathBuf, VerifiedCallgraphFile>,
) -> super::Result<bool> {
    let observed = observed.map(|files| {
        files
            .iter()
            .map(|(path, size, modified)| (path.as_path(), (*size, *modified)))
            .collect::<std::collections::BTreeMap<_, _>>()
    });
    for path in paths {
        let Ok(relative) = path.strip_prefix(root) else {
            note_checkout_verification_refusal!(
                "verification_root_mismatch: path={path:?} root={root:?}"
            );
            return Ok(false);
        };
        let key = super::RelPath::from_os_path(relative).map_err(|error| {
            note_checkout_verification_refusal!("verification_relative_path_invalid: path={path:?} relative={relative:?} error={error}");
            error
        })?;
        let language = if super::assembly::is_resolution_input(key.as_bytes()) {
            Some("config".to_string())
        } else {
            crate::parser::detect_language(path).map(|lang| format!("{lang:?}").to_lowercase())
        };
        let Some(language) = language else { continue };
        let Some(super::ManifestEntry::Regular { planes, .. }) = manifest.get(&key) else {
            note_checkout_verification_refusal!(
                "verification_regular_entry_missing: path={path:?} key={key:?} entry={:?}",
                manifest.get(&key)
            );
            return Ok(false);
        };
        let Some(expected_key) = planes.callgraph.as_ref() else {
            note_checkout_verification_refusal!(
                "verification_callgraph_key_missing: path={path:?} key={key:?} planes={planes:?}"
            );
            return Ok(false);
        };
        let stats = match observed
            .as_ref()
            .and_then(|files| files.get(path.as_path()))
            .copied()
            .map(Ok)
            .unwrap_or_else(|| verification_stat(path))
        {
            Ok(stats) => stats,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                note_checkout_verification_refusal!(
                    "verification_stat_not_found: path={path:?} error={error}"
                );
                return Ok(false);
            }
            Err(error) => {
                note_checkout_verification_refusal!(
                    "verification_stat_error: path={path:?} error={error}"
                );
                return Err(error.into());
            }
        };
        if verified.get(path).is_some_and(|previous| {
            previous.size == stats.0
                && previous.modified == stats.1
                && previous.expected_key == *expected_key
        }) {
            continue;
        }
        let source = read_verification_source(path).map_err(|error| {
            note_checkout_verification_refusal!(
                "verification_source_read_error: path={path:?} stats={stats:?} error={error}"
            );
            error
        })?;
        let actual_key = verification_key(&source, language, super::callgraph::PRODUCER);
        if actual_key != *expected_key {
            note_checkout_verification_refusal!("verification_source_key_mismatch: path={path:?} expected_key={expected_key} actual_key={actual_key} stats={stats:?} source_bytes={}", source.len());
            verified.remove(path);
            return Ok(false);
        }
        // Do not associate verified bytes with metadata from before an edit.
        let after_stats = verification_stat(path).map_err(|error| {
            note_checkout_verification_refusal!(
                "verification_post_read_stat_error: path={path:?} before={stats:?} error={error}"
            );
            error
        })?;
        if after_stats != stats {
            note_checkout_verification_refusal!("verification_post_read_stat_changed: path={path:?} before={stats:?} after={after_stats:?}");
            return Ok(false);
        }
        verified.insert(
            path.clone(),
            VerifiedCallgraphFile {
                size: stats.0,
                modified: stats.1,
                expected_key: expected_key.clone(),
            },
        );
    }
    Ok(true)
}

/// Why a view generation published with the call graph off is not served.
pub(crate) const CALLGRAPH_DISABLED: &str = "call graph is disabled (indexes.callgraph=false): this view generation was published without call graph data";
pub(crate) const CALLGRAPH_READINESS_BUSY: &str = "callgraph_readiness_busy";
pub(crate) const CALLGRAPH_READINESS_PROBE_FAILED: &str = "callgraph_readiness_probe_failed";

/// Open exactly the selected generation. The caller decides whether its snapshot
/// is acceptable; opening a reader must never replace a pinned generation with a
/// newer pointer or silently fall back to the mutable legacy store.
///
/// A generation published with the call graph off has an empty derived
/// database; serving it would answer "no callers" and "no dead code" for a
/// graph that was never built. Published v1 readers refuse it here, as unavailable
/// with [`CALLGRAPH_DISABLED`]; the checkout plane applies the same manifest guard
/// before opening a pinned v2 reader.
/// A manifest that cannot be read is refused too, rather than assumed to
/// carry a call graph.
pub(crate) fn open_published_callgraph(
    project_root: PathBuf,
    family: String,
    view_dir: PathBuf,
    generation: &str,
    pin: Option<Arc<QueryPin>>,
) -> Result<ReadonlyCallGraphStore> {
    if !generation_has_callgraph(&view_dir, generation)? {
        return Err(crate::callgraph_store::CallGraphStoreError::Unavailable(
            CALLGRAPH_DISABLED.to_string(),
        ));
    }
    ReadonlyCallGraphStore::open_manifest_view(project_root, family, view_dir, generation, pin)
}

/// Why a linked worktree's own generation cannot answer call-graph queries:
/// it was published without a derived database, which is built on first use.
pub(crate) const CALLGRAPH_NOT_BUILT: &str =
    "this checkout's call graph has not been built yet; the first query that needs it starts the build";

/// True when `generation` has a call-graph database to read: its own derived
/// file, or the checkout-wide `derived.sqlite` that older publications used,
/// when that holds a ready graph. A linked worktree publishes generations
/// with neither until a query needs its graph. Its view directory can still
/// hold a `derived.sqlite` placeholder from a legacy import, which is not a
/// graph. A busy legacy probe counts as no graph; it is only consulted when
/// the generation has no file of its own.
pub(crate) fn derived_built(view_dir: &std::path::Path, generation: &str) -> bool {
    super::resolve_derived_path(view_dir, generation).is_ok_and(|path| path.is_file())
        || matches!(
            crate::callgraph_store::manifest_view_database_ready(
                view_dir,
                generation,
                std::time::Instant::now(),
            ),
            Ok(true)
        )
}

/// The main checkout's published call graph, opened to answer for the linked
/// worktree at `project_root`, when it can answer for it.
///
/// It answers only when the worktree's published `manifest` has exactly the
/// main checkout's call-graph inputs: the same paths, modes and content keys.
/// Then main's graph is the graph a build of the worktree would produce.
/// Derived rows hold manifest-relative paths and the reader is bound to
/// `project_root`, so answers name the worktree's files, not main's.
///
/// `Unavailable` covers every case where equality is not proven: no main
/// checkout (or `project_root` is main), a change the watcher saw in the
/// worktree, main's view missing, unpublished or not ready, another artifact
/// family, different files, or a probe that failed. The caller then builds the
/// worktree's own graph instead of guessing. A locked database is `Busy`.
/// Main's generation gets a query pin, so generation sweeps do not delete its
/// files while they are read, and the pin lives as long as the returned reader.
pub(crate) fn main_checkout_callgraph(
    storage: &std::path::Path,
    project_root: &std::path::Path,
    family: &str,
    own_generation: &str,
    manifest: &super::Manifest,
) -> MainCheckoutGraph {
    use MainCheckoutGraph::{Answer, Busy, Unavailable};
    let Some(main_root) = main_checkout_root(project_root) else {
        return Unavailable;
    };
    if main_root == project_root
        || checkout_diverged(project_root)
        || crate::search_index::artifact_cache_key_read_only(&main_root, storage)
            .ok()
            .as_deref()
            != Some(family)
        || super::assembly::manifest_lacks_callgraph(manifest)
    {
        return Unavailable;
    }
    let view_dir = storage
        .join("views")
        .join(crate::path_identity::project_scope_key(&main_root));
    let Some(view) = super::ViewStore::existing_dir(view_dir.clone()) else {
        return Unavailable;
    };
    let generation = match view.current_generation_read_only() {
        Ok(Some(generation)) => generation,
        Ok(None) => return Unavailable,
        Err(super::ViewError::Sqlite(error)) if crate::db::is_busy_error(&error) => return Busy,
        Err(_) => return Unavailable,
    };
    let Ok(pin) = QueryPin::acquire(&view_dir, &generation) else {
        return Unavailable;
    };
    // Generations are immutable, so the verdict for this pair is cached and the
    // main manifest is read and compared once, not on every query.
    type Verdicts = std::collections::HashMap<(PathBuf, String, PathBuf, String), bool>;
    static VERDICTS: std::sync::OnceLock<std::sync::Mutex<Verdicts>> = std::sync::OnceLock::new();
    let key = (
        project_root.to_path_buf(),
        own_generation.to_owned(),
        view_dir.clone(),
        generation.clone(),
    );
    let verdicts = VERDICTS.get_or_init(Default::default);
    let known = verdicts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .copied();
    let equal = match known {
        Some(equal) => equal,
        None => {
            // Main's publication can hold its derived database locked (a
            // checkpoint or a writer) past the busy wait. That gives no verdict
            // either way: it is not cached, and the caller retries instead of
            // building a graph main may well be able to answer with.
            let ready = match crate::callgraph_store::manifest_view_database_ready(
                &view_dir,
                &generation,
                std::time::Instant::now() + crate::db::TOOL_RETRY_BUSY_WAIT,
            ) {
                Ok(ready) => ready,
                Err(error) if error.is_transient_lock_contention() => return Busy,
                Err(_) => return Unavailable,
            };
            // A generation can reuse an earlier generation's derived database
            // (`derived_owner`). Compare against the manifest that database was
            // built from, as `sibling_checkout_seed` does before cloning it.
            let equal = ready
                && view
                    .derived_owner(&generation)
                    .and_then(|owner| view.load_manifest(&owner))
                    .is_ok_and(|main| {
                        super::materialization::manifest_callgraph_equivalent(manifest, &main)
                    });
            let mut verdicts = verdicts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if verdicts.len() >= 256 {
                verdicts.clear();
            }
            verdicts.insert(key, equal);
            equal
        }
    };
    if !equal {
        return Unavailable;
    }
    match ReadonlyCallGraphStore::open_manifest_view(
        project_root.to_path_buf(),
        family.to_owned(),
        view_dir,
        &generation,
        Some(Arc::new(pin)),
    ) {
        Ok(store) => Answer(store, generation),
        Err(error) if error.is_transient_lock_contention() => Busy,
        Err(_) => Unavailable,
    }
}

/// What the main checkout's published graph can do for a linked worktree.
pub(crate) enum MainCheckoutGraph {
    /// Main's graph answers for the worktree; the generation is main's.
    Answer(ReadonlyCallGraphStore, String),
    /// Equality is not proven: the worktree needs its own graph.
    Unavailable,
    /// A database of main's view was locked, so there is no verdict yet. The
    /// caller reports the graph as building and asks again, rather than
    /// failing the query or starting a build main may make unnecessary.
    Busy,
}

/// Checkouts the file watcher saw change since this process loaded their
/// view. A published manifest lags an edit by the watcher's quiet window, so
/// a manifest that still equals main's does not prove the files do. Once a
/// checkout changes, main's graph stops answering for it for the rest of the
/// process; its next query builds its own graph instead.
static DIVERGED_CHECKOUTS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::OnceLock::new();

pub(crate) fn note_checkout_diverged(project_root: &std::path::Path) {
    DIVERGED_CHECKOUTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(project_root.to_path_buf());
}

fn checkout_diverged(project_root: &std::path::Path) -> bool {
    DIVERGED_CHECKOUTS.get().is_some_and(|roots| {
        roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(project_root)
    })
}

/// The main worktree of the repository `project_root` belongs to: the first
/// entry `git worktree list` reports. Cached per root once found; a failed
/// probe is retried on the next call rather than remembered.
fn main_checkout_root(project_root: &std::path::Path) -> Option<PathBuf> {
    type Roots = std::collections::HashMap<PathBuf, PathBuf>;
    static ROOTS: std::sync::OnceLock<std::sync::Mutex<Roots>> = std::sync::OnceLock::new();
    let roots = ROOTS.get_or_init(Default::default);
    if let Some(main) = roots
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(project_root)
    {
        return Some(main.clone());
    }
    let main = super::assembly::worker_seed::worktree_roots(project_root)?
        .into_iter()
        .next()?;
    // Views are keyed by the canonical root; git may report another spelling.
    let main = std::fs::canonicalize(&main).unwrap_or(main);
    roots
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(project_root.to_path_buf(), main.clone());
    Some(main)
}

/// True when the checkout's current view generation (the v1 view under
/// `<storage>/views/<scope>`) exists and was published without call graph
/// data. Read-only: nothing is created when the view is absent.
pub(crate) fn current_generation_lacks_callgraph(
    storage: &std::path::Path,
    root: &std::path::Path,
) -> bool {
    let view_dir = storage
        .join("views")
        .join(crate::path_identity::project_scope_key(root));
    let Some(store) = super::ViewStore::existing_dir(view_dir.clone()) else {
        return false;
    };
    let Ok(Some(generation)) = store.current_generation_read_only() else {
        return false;
    };
    matches!(generation_has_callgraph(&view_dir, &generation), Ok(false))
}

/// Whether `generation` was published with call graph data. Generations are
/// immutable, so the answer is cached per view directory and generation; the
/// manifest is read once, not on every call graph query.
fn generation_has_callgraph(view_dir: &std::path::Path, generation: &str) -> Result<bool> {
    type Cache = std::collections::HashMap<(PathBuf, String), bool>;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Cache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = (view_dir.to_path_buf(), generation.to_owned());
    if let Some(known) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        return Ok(*known);
    }
    let unavailable = |reason: String| {
        crate::callgraph_store::CallGraphStoreError::Unavailable(format!(
            "view generation {generation} manifest unreadable: {reason}"
        ))
    };
    let store = super::ViewStore::existing_dir(view_dir.to_path_buf())
        .ok_or_else(|| unavailable("view pointer missing".into()))?;
    let manifest = store
        .load_manifest(generation)
        .map_err(|error| unavailable(error.to_string()))?;
    let has = !super::assembly::manifest_lacks_callgraph(&manifest);
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.len() >= 1024 {
        cache.clear();
    }
    cache.insert(key, has);
    Ok(has)
}

/// Watcher edits do not change HEAD. Refuse to project an older published plane
/// when any requested tracked source still has a different content key.
///
/// Checkout assembly keys every published callgraph plane with the ruled
/// producer, so the current source must be keyed the same way. Keying it with
/// the legacy producer would never match and would leave every published
/// generation looking stale to inspect.
pub(crate) fn callgraph_paths_match(
    manifest: &super::Manifest,
    root: &std::path::Path,
    paths: &[PathBuf],
) -> super::Result<bool> {
    callgraph_paths_match_with_producer(manifest, root, paths, super::callgraph::PRODUCER)
}

/// Content verification for an opted-in ruled callgraph generation. Legacy
/// payload keys are intentionally incompatible with its dispatch-hint producer.
pub fn callgraph_paths_match_v2(
    manifest: &super::snapshot::ComposedGeneration,
    root: &std::path::Path,
    paths: &[PathBuf],
) -> super::Result<bool> {
    let projected = super::callgraph::project_manifest(manifest)
        .map_err(|error| super::ViewError::InvalidManifest(error.to_string()))?;
    callgraph_paths_match_with_producer(&projected, root, paths, super::callgraph::PRODUCER)
}

fn callgraph_paths_match_with_producer(
    manifest: &super::Manifest,
    root: &std::path::Path,
    paths: &[PathBuf],
    producer: &str,
) -> super::Result<bool> {
    for path in paths {
        let Ok(relative) = path.strip_prefix(root) else {
            return Ok(false);
        };
        let key = super::RelPath::from_os_path(relative)?;
        let language = if super::assembly::is_resolution_input(key.as_bytes()) {
            Some("config".to_string())
        } else {
            crate::parser::detect_language(path)
                .map(|language| format!("{language:?}").to_lowercase())
        };
        let Some(language) = language else { continue };
        // Untracked paths cannot change this published graph. In particular,
        // do not read an untracked path that has become a directory.
        let Some(entry) = manifest.get(&key) else {
            continue;
        };
        let source = match read_verification_source(path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        if let super::ManifestEntry::Regular { planes, .. } = entry {
            let current = verification_key(&source, language, producer);
            if planes.callgraph.as_deref() != Some(current.as_str()) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Opens only the generation protected by the reader's verified marker.
/// No pointer, manifest or plane artifact is created or repaired on this path.
pub fn open_foreign_generation(
    reader: &super::registry::ReaderRegistration,
    scope: &str,
    producers: &super::manifest_v2::Producers,
) -> super::Result<Option<std::sync::Arc<super::snapshot::ComposedGeneration>>> {
    let protected = reader.protect_current(scope).map_err(|error| {
        super::ViewError::InvalidManifest(format!("foreign view {scope} unavailable: {error}"))
    })?;
    let Some(protected) = protected else {
        return Ok(None);
    };
    let store =
        super::ViewStore::existing_dir(protected.view_dir().to_path_buf()).ok_or_else(|| {
            super::ViewError::InvalidManifest(format!(
                "foreign view {scope} unavailable: pointer missing"
            ))
        })?;
    let manifest = store.load_manifest_v2(protected.generation())?;
    manifest.ensure_producers(producers)?;
    if manifest.base_header().is_some() {
        // A base reference is valid only in a v3 worker manifest under
        // `blobs/v3/<family>/views/<scope>`, opened through
        // `open_foreign_v3_generation`. A v2 view naming a base is a corrupt
        // edge: its header cannot be authenticated, and it is never trusted.
        return Err(super::ViewError::InvalidManifest(format!(
            "foreign view {scope} names a base outside the v3 namespace; refusing an unauthenticated base header"
        )));
    }
    Ok(Some(std::sync::Arc::new(
        super::snapshot::ComposedGeneration::new(
            protected.generation().to_owned(),
            manifest,
            Some(super::snapshot::Residency::Protected(protected)),
        ),
    )))
}

/// Opens a v3 worker's published generation for a foreign or parent-folder
/// reader: a dual pin-then-verify of the worker and, through the authenticated
/// base resolver, of the base it names. The bundle owns both markers and its
/// answers carry the worker's `PublishedFrontier`. A base that stays missing or
/// moving through every attempt is reported unavailable, never replaced.
// No production caller until the parent-folder and foreign-reader routing
// slices select v3 workers; tests exercise it now.
#[cfg_attr(not(test), allow(dead_code))]
pub fn open_foreign_v3_generation(
    storage: &std::path::Path,
    family: &str,
    scope: &str,
    producers: &super::manifest_v2::Producers,
) -> super::Result<Option<std::sync::Arc<super::snapshot::ComposedGeneration>>> {
    super::base_refs::open_foreign_v3(storage, family, scope, producers).map_err(|error| {
        super::ViewError::InvalidManifest(format!("foreign view {scope} unavailable: {error}"))
    })
}

#[cfg(test)]
mod producer_tests {

    #[test]
    fn foreign_open_refuses_unauthenticated_base_headers() {
        use crate::views::manifest_v2::{
            BaseHeader, GenerationName, ManifestHeader, ManifestV2, Producers, PublishV2,
        };
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let registry =
            crate::views::registry::FamilyRegistry::open(storage.path(), "unverified-base")
                .unwrap();
        let owner = registry
            .register_view_with_storage(storage.path(), "worker", root.path())
            .unwrap();
        let producers = Producers {
            trigram: "tri".into(),
            semantic: None,
            callgraph: "graph".into(),
        };
        let manifest = ManifestV2::new_v3(
            ManifestHeader {
                producers: producers.clone(),
                head_tree: None,
                ignore_fingerprint: None,
                segment: None,
            },
            Some(BaseHeader {
                scope: "not-authenticated".into(),
                membership_digest: "11".repeat(32),
                identity_digest: "22".repeat(32),
                bound_generation: "diagnostic-only".into(),
            }),
            Default::default(),
        );
        // Seed an untrusted persisted header directly, bypassing the owner loader's refusal.
        let store = owner.view_store().unwrap();
        let name = GenerationName::for_manifest(&manifest).unwrap();
        let prepared = store.prepare_v2(&name, None, &manifest, None).unwrap();
        assert_eq!(
            store.commit_v2(prepared, None).unwrap(),
            PublishV2::Published
        );
        let reader = registry.register_reader("unverified-header").unwrap();
        let error = super::open_foreign_generation(&reader, "worker", &producers).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("refusing an unauthenticated base header"),
            "{error}"
        );
        assert_eq!(
            store.current_generation_read_only().unwrap(),
            Some(name.to_string())
        );
    }

    #[test]
    fn untracked_language_paths_do_not_read_source() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("untracked.rs");
        std::fs::create_dir(&path).unwrap();
        let manifest = super::super::Manifest::new([]).unwrap();
        super::take_verification_io();
        assert!(super::callgraph_paths_match(&manifest, root.path(), &[path]).unwrap());
        assert_eq!(super::take_verification_io().files_read, 0);
    }
    use super::*;
    #[test]
    fn ruled_callgraph_content_match_accepts_unchanged_and_rejects_edit() {
        let root = tempfile::tempdir().unwrap();
        let absolute = root.path().join("file.rs");
        let bytes = b"fn target() {}";
        std::fs::write(&absolute, bytes).unwrap();
        let mut entry = super::super::snapshot::LiveEntry::new(
            super::super::snapshot::DiskState::of_bytes(bytes),
            0,
        );
        let attachment = super::super::callgraph::attach(&mut entry, bytes, "rust").unwrap();
        let mut manifest =
            super::super::manifest_v2::ManifestV2::new(super::super::manifest_v2::ManifestHeader {
                producers: super::super::manifest_v2::Producers {
                    trigram: "test".into(),
                    semantic: None,
                    callgraph: super::super::callgraph::PRODUCER.into(),
                },
                head_tree: None,
                ignore_fingerprint: None,
                segment: None,
            });
        manifest
            .insert(
                super::super::RelPath::new(b"file.rs".to_vec()).unwrap(),
                super::super::manifest_v2::EntryV2::regular(
                    crate::blob_store::v2::ContentHash::of(bytes),
                    bytes.len() as u64,
                    super::super::manifest_v2::EntryPlanes {
                        callgraph: Some(super::super::readiness::PlaneState::ready(
                            &attachment.key,
                        )),
                        ..Default::default()
                    },
                ),
            )
            .unwrap();
        let manifest = super::super::snapshot::ComposedGeneration::new("test", manifest, None);
        assert!(callgraph_paths_match_v2(&manifest, root.path(), &[absolute.clone()]).unwrap());
        std::fs::write(&absolute, b"fn changed() {}").unwrap();
        assert!(!callgraph_paths_match_v2(&manifest, root.path(), &[absolute]).unwrap());
    }

    #[test]
    #[ignore = "manual source-verification IO measurement on the worker checkout"]
    fn inspect_repository_source_verification_io_profile() {
        let root = std::fs::canonicalize(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap(),
        )
        .unwrap();
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        let mut entries = Vec::new();
        let mut paths = Vec::new();
        let mut stats = Vec::new();
        for path in files {
            let relative =
                super::super::RelPath::from_os_path(path.strip_prefix(&root).unwrap()).unwrap();
            let language = if super::super::assembly::is_resolution_input(relative.as_bytes()) {
                Some("config".to_string())
            } else {
                crate::parser::detect_language(&path).map(|lang| format!("{lang:?}").to_lowercase())
            };
            let Some(language) = language else { continue };
            let bytes = std::fs::read(&path).unwrap();
            let metadata = std::fs::metadata(&path).unwrap();
            let key = crate::blob_store::CallgraphKey::from_bytes(
                &bytes,
                language,
                super::super::callgraph::PRODUCER,
            )
            .full_key()
            .to_hex();
            entries.push((
                relative,
                super::super::ManifestEntry::Regular {
                    mode: 0o100644,
                    planes: super::super::RegularPlanes {
                        semantic: None,
                        callgraph: Some(key),
                    },
                    resolution_input: false,
                },
            ));
            stats.push((path.clone(), metadata.len(), metadata.modified().unwrap()));
            paths.push(path);
        }
        let manifest = super::super::Manifest::new(entries).unwrap();
        // This is the source-verification part of the reader, not a fabricated
        // cold callgraph benchmark. Generation assembly is deliberately excluded.
        take_verification_io();
        for _ in 0..2 {
            assert!(callgraph_paths_match(&manifest, &root, &paths).unwrap());
        }
        let before = take_verification_io();
        let mut verified = std::collections::BTreeMap::new();
        assert!(callgraph_paths_match_cached(
            &manifest,
            &root,
            &paths,
            Some(&stats),
            &mut verified
        )
        .unwrap());
        let cold = take_verification_io();
        assert!(callgraph_paths_match_cached(
            &manifest,
            &root,
            &paths,
            Some(&stats),
            &mut verified
        )
        .unwrap());
        let warm = take_verification_io();
        eprintln!("inspect_repo_source_verification root={} source_files={} previous_per_call={before:?} revised_cold={cold:?} revised_warm={warm:?}", root.display(), paths.len());
        assert_eq!(warm, VerificationIo::default());
        assert_eq!(before.files_read, paths.len() * 2);
        assert_eq!(cold.files_read, paths.len());
    }
}
