//! Callgraph plane APIs, opt-in until runtime cutover.
//!
//! Composite FirstLoadDriver sequence:
//! 1. `reconcile`: strictly read/hash current members and call `attach` with the
//!    same bytes used by the other planes; put immutable blobs under protected keys.
//! 2. `build_own_generation`: project the v2 manifest through `materialize` into
//!    the private derived path, then compare-and-swap the publication pointer
//!    while a pin prevents reclamation. Copying another view's derived graph stays
//!    disabled until its logical rows are proven equal to an independent rebuild.
//! 3. `install`: call `open_generation` for the verified pinned publication, then
//!    atomically swap its reader and snapshot under the root lock only if the
//!    reconcile revision still matches. Opening a reader alone is not installation.
//! 4. Queries using callgraph, inspect or dead-code must first wait through
//!    QueryWait for CALLGRAPH_QUERY_WAIT (the bounded three-second wait from
//!    `views::contracts`). Use its final installed snapshot and
//!    disclose timeout gap paths; never substitute legacy/newer mutable state.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::blob_store::v2::{parse_hex32, FamilyKey, FamilyPlane, FamilyStoreReader};
use crate::callgraph_store::join::{CallgraphBlob, ManifestBlobReader, ManifestJoinError};
use crate::callgraph_store::ReadonlyCallGraphStore;

use super::contracts::{PlaneAdapter, PlaneError, ViewAccess};
use super::manifest_v2::EntryV2;
use super::readiness::{plane_readiness, FillMap, PlaneReadiness, PlaneState};
use super::snapshot::{ComposedGeneration, DiskState, LiveEntry, Snapshot};
use super::{Manifest, ManifestEntry, RegularPlanes, RelPath};

/// Bump when extracted evidence, its encoding or interpretation changes. Version
/// 9 omits the extraction-only AST index from stored payloads; facts are unchanged.
/// Version 10 stores the parser's corrected symbols: Go type aliases, separate
/// ranges for specs in a grouped `type (...)` block, and non-inverted ranges for
/// HTML headings that share a row.
pub const PRODUCER: &str = "ruled-callgraph-v10-go-alias-and-heading-ranges";

#[derive(Clone, Debug)]
pub struct CallgraphAttachment {
    pub blob: Arc<CallgraphBlob>,
    pub key: FamilyKey,
}

/// Markdown has outline headings, not callable definitions or call references.
/// It remains a manifest member for other planes, but needs no callgraph blob.
pub(crate) fn source_language(path: &Path) -> Option<crate::parser::LangId> {
    crate::parser::detect_language(path)
        .filter(|language| *language != crate::parser::LangId::Markdown)
}

/// Whether a member needs a callgraph blob: a source file in a callgraph
/// language, or resolver configuration such as `package.json`.
pub(crate) fn applies_to(path: &RelPath) -> bool {
    super::assembly::is_resolution_input(path.as_bytes())
        || std::str::from_utf8(path.as_bytes())
            .ok()
            .is_some_and(|p| source_language(Path::new(p)).is_some())
}

/// Attach evidence computed from exactly the caller's already-read byte buffer.
/// No disk access, blob put, publication or membership decision occurs here.
pub fn attach(
    entry: &mut LiveEntry,
    source: &[u8],
    language: &str,
) -> Result<CallgraphAttachment, PlaneError> {
    if entry.disk != DiskState::of_bytes(source) {
        return Err(error("attachment bytes differ from live entry"));
    }
    let blob = if language == "config" {
        CallgraphBlob::config(source.to_vec(), PRODUCER)
    } else {
        // A source that is not valid UTF-8 cannot be parsed. As in checkout
        // assembly, it gets the call graph of an empty file rather than
        // failing the whole reconciliation.
        CallgraphBlob::extract(
            std::str::from_utf8(source).unwrap_or(""),
            language,
            PRODUCER,
        )
        .map_err(error)?
    };
    let key = crate::blob_store::CallgraphKey::from_bytes(source, language, PRODUCER).full_key();
    let attachment = CallgraphAttachment {
        blob: Arc::new(blob),
        key: FamilyKey::from(&key),
    };
    entry
        .attachments
        .insert(FamilyPlane::Callgraph, Arc::new(attachment.clone()));
    Ok(attachment)
}

pub struct BlobReader(pub FamilyStoreReader);
impl ManifestBlobReader for BlobReader {
    fn read_callgraph_blob(&self, key: &str) -> Result<Option<Vec<u8>>, ManifestJoinError> {
        let bytes = parse_hex32(key)
            .ok_or_else(|| ManifestJoinError::InvalidBlob("invalid callgraph key".into()))?;
        self.0
            .get(&FamilyKey::new(FamilyPlane::Callgraph, bytes))
            .map_err(|e| ManifestJoinError::InvalidBlob(e.to_string()))
    }
}

/// Reads callgraph blobs from the family's v3 callgraph plane store.
pub struct V3BlobReader(pub crate::blob_store::v3::PlaneReader);
impl ManifestBlobReader for V3BlobReader {
    fn read_callgraph_blob(&self, key: &str) -> Result<Option<Vec<u8>>, ManifestJoinError> {
        let bytes = parse_hex32(key)
            .ok_or_else(|| ManifestJoinError::InvalidBlob("invalid callgraph key".into()))?;
        self.0
            .get(&FamilyKey::new(FamilyPlane::Callgraph, bytes))
            .map_err(|e| ManifestJoinError::InvalidBlob(e.to_string()))
    }
}

/// Project membership and compatible ready keys, not checkout paths/configuration.
/// Missing/failed callgraph members refuse construction rather than falsely ready.
pub fn project_manifest(
    manifest: &super::snapshot::ComposedGeneration,
) -> Result<Manifest, PlaneError> {
    if manifest.header().producers.callgraph != PRODUCER {
        return Err(error("incompatible callgraph producer"));
    }
    let mut entries = Vec::new();
    for (path, entry) in manifest.entries() {
        let projected = match entry {
            EntryV2::Regular {
                mode,
                resolution_input,
                ..
            } => {
                let key = match manifest.plane_readiness(path, FamilyPlane::Callgraph) {
                    Some(PlaneState::Ready { key }) => Some(key.clone()),
                    None => None,
                    Some(_) => {
                        return Err(error(format!("callgraph member is not ready: {path:?}")))
                    }
                };
                ManifestEntry::Regular {
                    mode: *mode,
                    planes: RegularPlanes {
                        callgraph: key,
                        semantic: None,
                    },
                    resolution_input: *resolution_input,
                }
            }
            EntryV2::Symlink { target_bytes } => ManifestEntry::Symlink {
                target_bytes: target_bytes.clone(),
            },
            EntryV2::Gitlink { oid } => ManifestEntry::Gitlink { oid: oid.clone() },
            EntryV2::Synthetic { .. } => {
                return Err(error(
                    "synthetic v2 callgraph input needs a regular immutable blob",
                ))
            }
        };
        entries.push((path.clone(), projected));
    }
    Manifest::new(entries).map_err(error)
}

pub fn materialize(
    database: &Path,
    manifest: &super::snapshot::ComposedGeneration,
    reader: &impl ManifestBlobReader,
) -> Result<(), PlaneError> {
    let projected = project_manifest(manifest)?;
    super::materialization::materialize_from_blob_reader(database, &projected, reader)
        .map_err(error)
}

/// The checkout root recorded for a v3 member, read without any lock.
fn v3_member_root(route: &super::snapshot::V3Route) -> Result<PathBuf, PlaneError> {
    use rusqlite::OptionalExtension;
    let root: Option<Option<Vec<u8>>> =
        crate::blob_store::v3::LedgerReader::open_existing(&route.storage, &route.family)
            .and_then(|ledger| {
                ledger.read(|c| {
                    Ok(c.query_row(
                        "SELECT root FROM members WHERE scope=?1",
                        [&route.scope],
                        |r| r.get(0),
                    )
                    .optional()?)
                })
            })
            .map_err(error)?;
    let bytes = root
        .flatten()
        .ok_or_else(|| error("v3 member root cannot be bound"))?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|_| error("v3 member root is not UTF-8"))
    }
}

/// Holding a reader keeps exactly its generation pinned, including after a swap.
pub struct PinnedReader {
    pub store: Arc<ReadonlyCallGraphStore>,
    pub generation: Arc<ComposedGeneration>,
}
#[derive(Default)]
pub struct CallgraphPlane {
    readers: RwLock<BTreeMap<(String, String), Arc<PinnedReader>>>,
}
impl CallgraphPlane {
    pub fn reader(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
    ) -> Result<Arc<PinnedReader>, PlaneError> {
        let reader = self
            .readers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(
                access.scope().to_string(),
                snapshot.generation().resident_key(),
            ))
            .cloned();
        match reader {
            Some(reader) => Ok(reader),
            None => {
                // A refused keyless generation will never have a resident reader.
                // Preserve that refusal instead of making it look like a build.
                queryable_manifest(snapshot.generation())?;
                Err(error("pinned callgraph reader is not resident"))
            }
        }
    }
}
impl PlaneAdapter for CallgraphPlane {
    fn plane(&self) -> FamilyPlane {
        FamilyPlane::Callgraph
    }
    fn producer(&self) -> String {
        PRODUCER.into()
    }
    fn applies_to(&self, path: &RelPath) -> bool {
        applies_to(path)
    }
    fn open_generation(
        &self,
        access: &ViewAccess,
        generation: &Arc<ComposedGeneration>,
    ) -> Result<(), PlaneError> {
        queryable_manifest(generation)?;
        let route = access.route(generation);
        let (root, dir): (PathBuf, PathBuf) = match (access, route) {
            // A v3 generation's derived graph lives in its v3 member
            // directory; the checkout root binds rendering as on v2.
            (ViewAccess::Owner(view), super::contracts::StorageRoute::V3(route)) => {
                (view.root().to_path_buf(), route.member_dir())
            }
            (ViewAccess::Reader { .. }, super::contracts::StorageRoute::V3(route)) => {
                (v3_member_root(&route)?, route.member_dir())
            }
            (ViewAccess::Owner(view), _) => {
                (view.root().to_path_buf(), view.view_dir().to_path_buf())
            }
            (
                ViewAccess::Reader {
                    registration,
                    scope,
                },
                _,
            ) => {
                let member = registration
                    .members()
                    .map_err(error)?
                    .into_iter()
                    .find(|m| &m.scope == scope)
                    .ok_or_else(|| error("member not registered"))?;
                (
                    member
                        .root
                        .ok_or_else(|| error("member root cannot be bound"))?,
                    member.view_dir,
                )
            }
        };
        let store = ReadonlyCallGraphStore::open_pinned_derived(
            root,
            access.family().into(),
            dir,
            generation.name(),
        )
        .map_err(|e| {
            if e.is_transient_lock_contention() {
                error(format!("{DERIVED_STORE_BUSY}: {e}"))
            } else {
                error(e)
            }
        })?;
        let retired = self
            .readers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                (access.scope().into(), generation.resident_key()),
                Arc::new(PinnedReader {
                    store: Arc::new(store.retain_generation(generation.clone())),
                    generation: generation.clone(),
                }),
            );
        drop(retired);
        Ok(())
    }
    fn release_generation(&self, access: &ViewAccess, generation: &str) {
        let retired = self
            .readers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(access.scope().into(), generation.into()));
        drop(retired);
    }
    fn readiness(&self, access: &ViewAccess, snapshot: &Snapshot) -> PlaneReadiness {
        if !self
            .readers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(
                access.scope().to_owned(),
                snapshot.generation().resident_key(),
            ))
            .is_some_and(|reader| {
                reader.generation.resident_key() == snapshot.generation().resident_key()
            })
        {
            return PlaneReadiness::Building;
        }
        let readiness = plane_readiness(
            FamilyPlane::Callgraph,
            Some(snapshot.generation()),
            snapshot,
            &FillMap::default(),
            &|path| self.applies_to(path),
        );
        if snapshot.pending_intent().next().is_some() {
            PlaneReadiness::Building
        } else {
            readiness
        }
    }
}
/// Why a checkout view could not supply a reader. `busy` marks a transient
/// SQLite lock on the view's database: the graph exists and the caller should
/// retry, which is reported as building rather than unavailable.
#[derive(Clone, Debug)]
pub(crate) struct CallgraphViewError {
    pub reason: String,
    pub busy: bool,
}

impl From<String> for CallgraphViewError {
    fn from(reason: String) -> Self {
        Self {
            reason,
            busy: false,
        }
    }
}

impl From<&str> for CallgraphViewError {
    fn from(reason: &str) -> Self {
        reason.to_owned().into()
    }
}

impl std::fmt::Display for CallgraphViewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Prefix of the plane error for a derived database that a SQLite lock kept
/// from opening.
pub(crate) const DERIVED_STORE_BUSY: &str = "callgraph derived store is busy";

/// Supplies a checkout's verified call graph to one request, without
/// exposing its storage layout. Implementations never publish v1/v2 data and
/// never substitute another store when acquisition fails.
pub(crate) trait CallgraphReaderSource: Send + Sync {
    fn acquire(&self) -> Result<Arc<ReadonlyCallGraphStore>, CallgraphViewError>;
    fn record_change(&self, path: &Path);
    fn record_overflow(&self) {}
    fn record_cookie(&self, _: &Path) {}
    /// Source files parsed for call-graph evidence so far.
    #[cfg(test)]
    fn extraction_count(&self) -> usize {
        0
    }
}

/// The production source: a v3-only checkout driver carrying the lexical
/// plane (bases are lexical-ready first) and the call-graph plane.
struct CheckoutCallgraphSource {
    driver: Arc<super::first_load::CheckoutDriver>,
    access: ViewAccess,
    bridge: Arc<super::first_load::CallgraphBridge>,
    storage: PathBuf,
    family: String,
    scope: String,
    producers: super::manifest_v2::Producers,
}

/// True when the snapshot's live entries differ from its published
/// generation: a graph opened for that generation would answer for older
/// bytes than the checkout holds.
fn has_unpublished_changes(snapshot: &Snapshot) -> bool {
    snapshot
        .live_entries()
        .any(|(path, entry)| entry.disk != snapshot.generation().disk_state(path))
}

impl CallgraphReaderSource for CheckoutCallgraphSource {
    fn acquire(&self) -> Result<Arc<ReadonlyCallGraphStore>, CallgraphViewError> {
        use super::first_load::FirstLoadDriver;
        let installed = self.driver.installed_snapshot();
        // After a restart, start from this checkout's own published worker
        // generation; verification still reconciles it against the disk.
        let seed = if installed.generation().name() == "empty" {
            match super::base_refs::open_foreign_v3(
                &self.storage,
                &self.family,
                &self.scope,
                &self.producers,
            ) {
                Ok(Some(generation)) => generation,
                Ok(None) => installed.generation().clone(),
                Err(error) => {
                    crate::slog_debug!(
                        "callgraph v3 seed unavailable; reconciling current tree: {}",
                        error
                    );
                    installed.generation().clone()
                }
            }
        } else {
            installed.generation().clone()
        };
        let (mut snapshot, revision) = self
            .driver
            .verified_bind(&self.access, seed)
            .map_err(|error| error.to_string())?;
        if has_unpublished_changes(&snapshot) {
            // Publish the edits as a new worker generation (a donor clone
            // patched with the diff) before the single install, as a fresh
            // bootstrap does inside `verified_bind`. A second install would
            // see the first one marked overflowed on a checkout without a
            // steady watcher subscription and refuse its own cut.
            let own = self
                .driver
                .build_own_generation(
                    &self.access,
                    &snapshot,
                    super::first_load::DERIVED_CALLGRAPH_SEEDING,
                )
                .map_err(|error| error.to_string())?;
            let cut = snapshot
                .install_cut()
                .cloned()
                .ok_or("callgraph view snapshot has no install cut")?;
            let mut delta = super::snapshot::LiveDelta::from_snapshot(&snapshot);
            let draft = super::snapshot::derive_successor(
                &delta.cut(),
                own,
                &super::snapshot::carry_disk_state_only,
            );
            delta
                .replay_and_swap(draft, &super::snapshot::carry_disk_state_only)
                .map_err(|error| format!("{error:?}"))?;
            snapshot = delta
                .snapshot()
                .with_install_cut(cut.clone())
                .with_reconciled_epoch(snapshot.reconciled_epoch().unwrap_or(cut.epoch));
        }
        self.driver
            .install(&self.access, &snapshot, revision)
            .map_err(|error| error.to_string())?;
        // Read the installed cut directly: `installed_snapshot` would mark a
        // checkout without a steady watcher subscription overflowed at once.
        // That mark only decides how the next bind reconciles; this cut was
        // just verified against the disk.
        let (snapshot, _) = self.driver.installed_cut();
        if snapshot.pending_intent().next().is_some()
            || snapshot.watcher() != super::snapshot::WatcherState::Healthy
            || has_unpublished_changes(&snapshot)
        {
            return Err(format!(
                "callgraph view has unverified writes (pending={:?}, watcher={:?}, unpublished={:?})",
                snapshot.pending_intent().take(3).collect::<Vec<_>>(),
                snapshot.watcher(),
                snapshot
                    .live_entries()
                    .filter(|(path, entry)| entry.disk
                        != snapshot.generation().disk_state(path))
                    .map(|(path, _)| path.clone())
                    .take(3)
                    .collect::<Vec<_>>(),
            )
            .into());
        }
        let reader = match self.bridge.adapter.reader(&self.access, &snapshot) {
            Ok(reader) => reader,
            Err(error) if error.reason == super::read::CALLGRAPH_DISABLED => {
                return Err(error.reason.into())
            }
            Err(_) => {
                // A generation installed from the durable pointer (after a
                // restart) has no resident reader yet; it is this checkout's
                // own v3 member, so open its derived graph here.
                self.bridge
                    .adapter
                    .open_generation(&self.access, snapshot.generation())
                    .map_err(|error| CallgraphViewError {
                        busy: error.reason.starts_with(DERIVED_STORE_BUSY),
                        reason: error.to_string(),
                    })?;
                self.bridge
                    .adapter
                    .reader(&self.access, &snapshot)
                    .map_err(|error| error.to_string())?
            }
        };
        Ok(reader.store.clone())
    }

    fn record_change(&self, path: &Path) {
        self.driver.record_absolute_change(path);
    }
    fn record_overflow(&self) {
        self.driver.record_watcher_overflow();
    }
    fn record_cookie(&self, path: &Path) {
        self.driver.record_watcher_cookie(path);
    }
    #[cfg(test)]
    fn extraction_count(&self) -> usize {
        self.bridge.extraction_count()
    }
}

/// Checkout-local call-graph authority used when views are enabled. Its v3
/// member is separate from the lexical view's, so lexical service never waits
/// for graph extraction; both share the family's content-addressed rows.
pub(crate) struct CallgraphView {
    root: PathBuf,
    storage: crate::root_cache::StoragePaths,
    family: String,
    policy: crate::blob_store::v2::TrigramPolicy,
    source: Arc<dyn CallgraphReaderSource>,
    query: std::sync::Mutex<()>,
    ready: std::sync::atomic::AtomicBool,
}

/// The v3 member scope of a checkout's call-graph view.
pub(crate) fn view_scope(root: &Path) -> String {
    format!(
        "{}.callgraph",
        crate::path_identity::project_scope_key(root)
    )
}

impl CallgraphView {
    pub(crate) fn matches(
        &self,
        root: &Path,
        storage: &crate::root_cache::StoragePaths,
        family: &str,
        policy: crate::blob_store::v2::TrigramPolicy,
    ) -> bool {
        self.root == root
            && self.storage == *storage
            && self.family == family
            && self.policy == policy
    }

    pub(crate) fn new(
        root: PathBuf,
        storage: crate::root_cache::StoragePaths,
        family: String,
        policy: crate::blob_store::v2::TrigramPolicy,
    ) -> Result<Self, String> {
        let scope = view_scope(&root);
        let producers = super::manifest_v2::Producers {
            trigram: policy.fingerprint_hex(),
            semantic: None,
            callgraph: PRODUCER.into(),
        };
        let trigram = Arc::new(super::first_load::TrigramBridge::new(
            storage.artifacts.clone(),
            policy,
        ));
        let bridge = Arc::new(super::first_load::CallgraphBridge::default());
        let adapter: Arc<dyn PlaneAdapter> = bridge.adapter.clone();
        let driver = Arc::new(
            super::first_load::CheckoutDriver::new_v3(
                &storage.artifacts,
                &family,
                &scope,
                &root,
                producers.clone(),
                Arc::new(super::first_load::ConfiguredMembershipWalker),
                vec![trigram, bridge.clone()],
            )
            .map_err(|error| error.to_string())?
            .with_adapters(vec![adapter]),
        );
        driver.register_write_intent();
        let source = Arc::new(CheckoutCallgraphSource {
            access: driver.access(),
            driver,
            bridge,
            storage: storage.artifacts.clone(),
            family: family.clone(),
            scope,
            producers,
        });
        Ok(Self::with_source(root, storage, family, policy, source))
    }

    pub(crate) fn with_source(
        root: PathBuf,
        storage: crate::root_cache::StoragePaths,
        family: String,
        policy: crate::blob_store::v2::TrigramPolicy,
        source: Arc<dyn CallgraphReaderSource>,
    ) -> Self {
        Self {
            root,
            storage,
            family,
            policy,
            source,
            query: std::sync::Mutex::new(()),
            ready: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn record_change(&self, path: &Path) {
        self.ready
            .store(false, std::sync::atomic::Ordering::Release);
        self.source.record_change(path);
    }

    pub(crate) fn record_overflow(&self) {
        self.ready
            .store(false, std::sync::atomic::Ordering::Release);
        self.source.record_overflow();
    }

    pub(crate) fn record_cookie(&self, path: &Path) {
        self.source.record_cookie(path);
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn extraction_count(&self) -> usize {
        self.source.extraction_count()
    }

    /// Acquires a reader pinned to one verified generation. Acquisitions are
    /// serialized so an installed membership and its graph come from the
    /// same reconciliation.
    pub(crate) fn reader(&self) -> Result<Arc<ReadonlyCallGraphStore>, CallgraphViewError> {
        let _query = self
            .query
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = self.source.acquire();
        self.ready
            .store(result.is_ok(), std::sync::atomic::Ordering::Release);
        result
    }
}

fn queryable_manifest(
    manifest: &super::snapshot::ComposedGeneration,
) -> Result<Manifest, PlaneError> {
    let projected = project_manifest(manifest)?;
    // A disabled plane still has a valid empty derived database. It must not
    // answer navigation or reachability questions as though a graph was built.
    if super::assembly::manifest_lacks_callgraph(&projected) {
        return Err(error(super::read::CALLGRAPH_DISABLED));
    }
    Ok(projected)
}
fn error(reason: impl std::fmt::Display) -> PlaneError {
    PlaneError {
        plane: Some(FamilyPlane::Callgraph),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILES: &[(&str, &str)] = &[
        ("a.ts", "export function target() { return 1; }\n"),
        (
            "b.ts",
            "import { target } from './a';\nexport function caller() { return target(); }\n",
        ),
        ("package.json", "{\"name\": \"donor-fixture\"}\n"),
        ("notes.md", "# notes\n"),
    ];

    fn checkout(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        for (name, text) in FILES {
            std::fs::write(dir.join(name), text).unwrap();
        }
        std::fs::canonicalize(dir).unwrap()
    }

    fn callers(store: &ReadonlyCallGraphStore, file: &str, symbol: &str) -> Vec<String> {
        let mut names: Vec<_> = store
            .callers_of(Path::new(file), symbol, 1)
            .unwrap()
            .callers
            .iter()
            .map(|caller| format!("{caller:?}"))
            .collect();
        names.sort();
        names
    }

    fn view(root: &Path, storage: &Path) -> CallgraphView {
        CallgraphView::new(
            root.to_path_buf(),
            crate::root_cache::StoragePaths::same(storage),
            "donor-reuse".into(),
            crate::blob_store::v2::TrigramPolicy {
                max_file_size: 1 << 20,
            },
        )
        .unwrap()
    }

    fn events(dir: &Path) -> Vec<super::super::donor::DonorEvent> {
        super::super::donor::events_under(dir)
    }

    #[test]
    fn sealed_base_donor_serves_checkouts_without_reextraction_or_cold_builds() {
        use super::super::donor::DonorEvent;
        let _serial = crate::blob_store::v3::serial_for_test();
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let first = checkout(&temp.path().join("first"));
        let second = checkout(&temp.path().join("second"));
        let member = |root: &Path| {
            crate::blob_store::v3::view_dir(&storage, "donor-reuse", &view_scope(root)).unwrap()
        };

        // A fresh checkout bootstraps the base through the base builder,
        // completes and seals its graph, then clones the donor for itself.
        let first_view = view(&first, &storage);
        let first_store = first_view.reader().unwrap();
        assert_eq!(callers(&first_store, "a.ts", "target").len(), 1);
        assert_eq!(first_view.extraction_count(), 3, "a.ts, b.ts, package.json");
        let all = events(&storage);
        assert_eq!(
            all.iter().filter(|e| **e == DonorEvent::Sealed).count(),
            1,
            "{all:?}"
        );
        assert!(
            !all.iter().any(|e| matches!(e, DonorEvent::Cold(_))),
            "{all:?}"
        );
        assert!(matches!(
            events(&member(&first)).as_slice(),
            [DonorEvent::Cloned(_)]
        ));

        // The base fixed configuration metadata before its first publication.
        let producers = super::super::manifest_v2::Producers {
            trigram: crate::blob_store::v2::TrigramPolicy {
                max_file_size: 1 << 20,
            }
            .fingerprint_hex(),
            semantic: None,
            callgraph: PRODUCER.into(),
        };
        let worker = super::super::base_refs::open_foreign_v3(
            &storage,
            "donor-reuse",
            &view_scope(&first),
            &producers,
        )
        .unwrap()
        .unwrap();
        let base_scope = worker.base_header().unwrap().scope.clone();
        let base =
            super::super::base_refs::resolve_base(&storage, "donor-reuse", &base_scope).unwrap();
        let config = RelPath::new(b"package.json".to_vec()).unwrap();
        assert!(matches!(
            base.manifest().get(&config),
            Some(EntryV2::Regular {
                resolution_input: true,
                ..
            })
        ));
        let donor = super::super::resolve_derived_path(base.view_dir(), base.generation()).unwrap();
        let seal = super::super::donor::verify(&donor).unwrap();
        drop((base, worker));

        // A second checkout of the same bytes reuses the base's blobs and its
        // sealed donor: nothing is parsed and no graph is built cold.
        let second_view = view(&second, &storage);
        let second_store = second_view.reader().unwrap();
        assert_eq!(second_view.extraction_count(), 0);
        assert_eq!(
            callers(&second_store, "a.ts", "target"),
            callers(&first_store, "a.ts", "target")
        );
        assert!(matches!(
            events(&member(&second)).as_slice(),
            [DonorEvent::Cloned(_)]
        ));

        // An edit publishes a new worker generation: another fresh clone of
        // the donor, patched with the diff; only the edited file is parsed.
        std::fs::write(
            second.join("c.ts"),
            "import { caller } from './b';\nexport function top() { return caller(); }\n",
        )
        .unwrap();
        second_view.record_change(&second.join("c.ts"));
        let edited = second_view.reader().unwrap();
        assert_eq!(callers(&edited, "b.ts", "caller").len(), 1);
        assert!(callers(&first_store, "b.ts", "caller").is_empty());
        assert_eq!(second_view.extraction_count(), 1);
        let second_events = events(&member(&second));
        assert_eq!(second_events.len(), 2, "{second_events:?}");
        assert!(second_events
            .iter()
            .all(|e| matches!(e, DonorEvent::Cloned(_))));
        // Readers of the donor never see a write.
        super::super::donor::verify_fingerprint(&donor, &seal).unwrap();

        // Deleting a file the worker added leaves no tombstone behind: the
        // base never had it.
        std::fs::remove_file(second.join("c.ts")).unwrap();
        second_view.record_change(&second.join("c.ts"));
        let deleted = second_view.reader().unwrap();
        assert!(callers(&deleted, "b.ts", "caller").is_empty());
        // A reader acquired before the deletion keeps answering for its own
        // generation: publication never reclaims a generation a reader holds.
        assert_eq!(callers(&edited, "b.ts", "caller").len(), 1);
        assert_eq!(
            callers(&deleted, "a.ts", "target"),
            callers(&first_store, "a.ts", "target")
        );

        // A restart reopens the published worker generation without parsing
        // or rebuilding anything.
        drop((first_store, first_view));
        let restarted = view(&first, &storage);
        let reopened = restarted.reader().unwrap();
        assert_eq!(callers(&reopened, "a.ts", "target").len(), 1);
        assert_eq!(restarted.extraction_count(), 0);
        assert_eq!(events(&member(&first)).len(), 1);
        assert!(!storage.join("blobs/v2").exists());
    }

    #[test]
    fn markdown_is_not_a_callgraph_input_but_resolution_config_is() {
        let plane = CallgraphPlane::default();
        for document in [
            "notes.md",
            "notes.markdown",
            "notes.mdx",
            "notes.qmd",
            "notes.Rmd",
        ] {
            let path = RelPath::from_os_path(Path::new(document)).unwrap();
            assert!(
                !plane.applies_to(&path),
                "{document} requires a callgraph blob"
            );
        }
        for input in [
            "index.ts",
            "src/lib.rs",
            "package.json",
            "tsconfig.json",
            "Cargo.toml",
            ".gitignore",
        ] {
            let path = RelPath::from_os_path(Path::new(input)).unwrap();
            assert!(plane.applies_to(&path), "{input} lost its callgraph input");
        }
    }
}
