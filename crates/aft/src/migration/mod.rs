//! One-way import of the pre-view semantic snapshot format.
//!
//! The importer deliberately leaves `semantic.bin` untouched until a complete
//! manifest is visible.  A caller can therefore roll back by removing the view
//! directory and continue to use the legacy snapshot.
//!
//! Nothing calls [`import_legacy_semantic`] on a live root yet. The call-site
//! contract for the integrator: run it under a `ColdBuildLimiter` slot, only
//! for a root with writer capability on the family (a borrow-only worktree
//! must not open the shared blob store), off the bind/artifact-load path, and
//! after the view has been checked for a current generation (done inside).
//! It reads the whole snapshot, re-reads and hashes every source row, and
//! writes every row into the blob store, so its cost scales with the corpus.
//! A `RebuildRequired` outcome only records `migration-state.json` in the view
//! directory; the semantic builder does not consume that marker yet, and the
//! hand-off from the marker to a scheduled rebuild belongs to the integrator.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::alias::AliasStore;
use crate::blob_store::{
    BlobStore, BlobStoreError, FullKey, PutOutcome, SemanticKey, SEMANTIC_PRODUCER_VERSION,
};
use crate::callgraph_store::CallGraphStoreError;
use crate::pins::{AssemblyPin, PinError};
use crate::views::{
    ArtifactPlane, ClosureRequirements, Manifest, ManifestEntry, PublicationArtifacts,
    PublicationClosure, PublicationRequest, PublishOutcome, RegularPlanes, RelPath, ViewError,
    ViewStore,
};

const LEGACY_SEMANTIC_V6: u8 = 6;
const LEGACY_SEMANTIC_V7: u8 = 7;
const LEGACY_CHUNKING_VERSION: u64 = 2;
const MAX_LEGACY_ROWS: usize = 2_000_000;
const MAX_LEGACY_DIMENSION: usize = 16_384;
const IMPORTED_PAYLOAD_VERSION: u8 = 1;

/// Input for importing the semantic snapshot that predates view assembly.
#[derive(Clone, Debug)]
pub struct SemanticMigrationRequest {
    /// Shared AFT storage root.
    pub storage: PathBuf,
    /// Existing checkout whose legacy snapshot is eligible for import.
    pub project_root: PathBuf,
    /// Repository-family key used by both the old semantic cache and new blob store.
    pub family: String,
    /// Checkout-local view key. Different worktrees share `family` but not `view`.
    pub view: String,
    /// Canonical JSON fingerprint for the currently configured embedding model.
    pub configured_model_fingerprint: String,
    /// The producer stamps assigned to imported blobs by this binary.
    pub chunker_version: String,
    pub embed_template_version: String,
}

impl SemanticMigrationRequest {
    /// Builds a request using the repository and checkout identities already used by
    /// the legacy semantic cache.
    pub fn for_root(
        storage: impl Into<PathBuf>,
        project_root: impl Into<PathBuf>,
        configured_model_fingerprint: impl Into<String>,
    ) -> Self {
        let project_root = project_root.into();
        Self {
            storage: storage.into(),
            family: crate::search_index::artifact_cache_key(&project_root),
            view: crate::path_identity::project_scope_key(&project_root),
            project_root,
            configured_model_fingerprint: configured_model_fingerprint.into(),
            chunker_version: SEMANTIC_PRODUCER_VERSION.to_string(),
            embed_template_version: SEMANTIC_PRODUCER_VERSION.to_string(),
        }
    }

    pub fn legacy_semantic_path(&self) -> PathBuf {
        self.storage
            .join("semantic")
            .join(&self.family)
            .join("semantic.bin")
    }
}

/// Terminal classification of one import attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticMigrationOutcome {
    /// No old snapshot exists for this root.
    NotNeeded,
    /// A compatible snapshot was transformed into semantic blobs and published.
    Imported,
    /// A view already has a current generation, so the one-way import has finished.
    AlreadyPublished,
    /// The legacy snapshot cannot be proven compatible and must be rebuilt once.
    RebuildRequired { reason: String },
    /// A rebuild was already requested for this view and must not be scheduled again.
    RebuildAlreadyScheduled { reason: String },
    /// Another publisher won the initial pointer compare-and-swap.
    PublishConflict { current_generation: Option<String> },
}

/// Observable result of a semantic migration attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticMigrationReport {
    pub outcome: SemanticMigrationOutcome,
    /// Number of legacy file rows imported without embedding work.
    pub imported_rows: usize,
    /// Total rows omitted from the imported manifest.
    pub skipped_rows: usize,
    /// Rows omitted specifically because their absolute path is outside the root.
    pub outside_root_rows: usize,
    /// Rows omitted because their current source bytes no longer match the snapshot.
    pub stale_rows: usize,
    /// Semantic embedding work performed by this importer. Compatible imports are zero.
    pub reembedded_rows: usize,
    /// Full keys written or reused by this import, in bytewise path order.
    pub semantic_keys: Vec<String>,
    /// The importer stamps every imported row with these producer values.
    pub chunker_version: String,
    pub embed_template_version: String,
    pub model_fingerprint: String,
}

impl SemanticMigrationReport {
    fn empty(request: &SemanticMigrationRequest, outcome: SemanticMigrationOutcome) -> Self {
        Self {
            outcome,
            imported_rows: 0,
            skipped_rows: 0,
            outside_root_rows: 0,
            stale_rows: 0,
            reembedded_rows: 0,
            semantic_keys: Vec::new(),
            chunker_version: request.chunker_version.clone(),
            embed_template_version: request.embed_template_version.clone(),
            model_fingerprint: request.configured_model_fingerprint.clone(),
        }
    }
}

/// Whether the existing staged callgraph build had to do cold work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallgraphMigrationOutcome {
    Rebuilt,
    AlreadyCurrent,
}

/// Errors returned before a migration has a terminal report.
#[derive(Debug)]
pub enum MigrationError {
    Io(io::Error),
    Blob(BlobStoreError),
    Sqlite(rusqlite::Error),
    Alias(crate::alias::AliasError),
    View(ViewError),
    Pin(PinError),
    Callgraph(CallGraphStoreError),
    InvalidLegacySnapshot(String),
    UndurablePut(PutOutcome),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "migration I/O error: {error}"),
            Self::Blob(error) => write!(formatter, "migration blob-store error: {error}"),
            Self::Sqlite(error) => write!(formatter, "migration SQLite error: {error}"),
            Self::Alias(error) => write!(formatter, "migration alias-store error: {error}"),
            Self::View(error) => write!(formatter, "migration view error: {error}"),
            Self::Pin(error) => write!(formatter, "migration pin error: {error}"),
            Self::Callgraph(error) => write!(formatter, "migration callgraph error: {error}"),
            Self::InvalidLegacySnapshot(reason) => {
                write!(formatter, "invalid legacy semantic snapshot: {reason}")
            }
            Self::UndurablePut(outcome) => {
                write!(formatter, "semantic blob put was not durable: {outcome:?}")
            }
        }
    }
}

impl std::error::Error for MigrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Blob(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::Alias(error) => Some(error),
            Self::View(error) => Some(error),
            Self::Pin(error) => Some(error),
            Self::Callgraph(error) => Some(error),
            Self::InvalidLegacySnapshot(_) | Self::UndurablePut(_) => None,
        }
    }
}

impl From<io::Error> for MigrationError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<BlobStoreError> for MigrationError {
    fn from(error: BlobStoreError) -> Self {
        Self::Blob(error)
    }
}

impl From<rusqlite::Error> for MigrationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<crate::alias::AliasError> for MigrationError {
    fn from(error: crate::alias::AliasError) -> Self {
        Self::Alias(error)
    }
}

impl From<ViewError> for MigrationError {
    fn from(error: ViewError) -> Self {
        Self::View(error)
    }
}

impl From<PinError> for MigrationError {
    fn from(error: PinError) -> Self {
        Self::Pin(error)
    }
}

impl From<CallGraphStoreError> for MigrationError {
    fn from(error: CallGraphStoreError) -> Self {
        Self::Callgraph(error)
    }
}

/// Imports an eligible root's old `semantic.bin` without making embedding requests.
///
/// Compatibility is intentionally strict: V6/V7 snapshots must carry the exact
/// configured model fingerprint and the current legacy chunking version. Earlier
/// or unstamped snapshots are recorded as a rebuild request rather than treated as
/// an import failure. The source snapshot is never deleted or rewritten here.
pub(crate) fn store_live_semantic_blobs(
    request: &SemanticMigrationRequest,
    index: &crate::semantic_index::SemanticIndex,
) -> Result<BTreeMap<Vec<u8>, String>, MigrationError> {
    let raw = index.to_bytes();
    let parsed = parse_legacy_snapshot(&raw, &request.configured_model_fingerprint)
        .map_err(MigrationError::InvalidLegacySnapshot)?;
    let root = fs::canonicalize(&request.project_root)?;
    let view = ViewStore::open(&request.storage, &request.view)?;
    let mut candidates = Vec::new();
    for file in parsed.files {
        let Some(rel_path) = legacy_path_to_rel_path(&file.path, &root) else {
            continue;
        };
        let source_path = root.join(os_path_from_bytes(rel_path.as_bytes()));
        let Ok(source) = fs::read(&source_path) else {
            continue;
        };
        if blake3::hash(&source).as_bytes() != &file.content_hash {
            continue;
        }
        let key = SemanticKey::from_bytes(
            &source,
            rel_path.as_bytes(),
            &request.chunker_version,
            &request.embed_template_version,
            &request.configured_model_fingerprint,
        )
        .full_key();
        candidates.push(ImportCandidate {
            rel_path,
            mode: regular_mode(&source_path)?,
            key,
            payload: encode_imported_payload(request, &file.entries),
        });
    }
    let keys = candidates
        .iter()
        .map(|candidate| candidate.key.clone())
        .collect::<Vec<_>>();
    let generation = format!("semantic-live-{}", &blake3::hash(&raw).to_hex()[..16]);
    let mut pin = AssemblyPin::create(
        view.view_dir(),
        request.family.clone(),
        request.view.clone(),
        generation,
        &keys,
    )?;
    let mut store = BlobStore::open(
        &request.storage,
        request.family.clone(),
        crate::blob_store::BlobPlane::Semantic,
    )?;
    let mut semantic_keys = BTreeMap::new();
    for candidate in candidates {
        pin.renew_if_due()?;
        let put = store.put(&candidate.key, &candidate.payload)?;
        if !put.durable {
            return Err(MigrationError::UndurablePut(put.outcome));
        }
        semantic_keys.insert(
            candidate.rel_path.as_bytes().to_vec(),
            candidate.key.to_hex(),
        );
    }
    pin.release();
    Ok(semantic_keys)
}

/// Imports an eligible root's old `semantic.bin` without making embedding requests.
///
/// Compatibility is intentionally strict: V6/V7 snapshots must carry the exact
/// configured model fingerprint and the current legacy chunking version. Earlier
/// or unstamped snapshots are recorded as a rebuild request rather than treated as
/// an import failure. The source snapshot is never deleted or rewritten here.
pub fn import_legacy_semantic(
    request: &SemanticMigrationRequest,
) -> Result<SemanticMigrationReport, MigrationError> {
    let source_path = request.legacy_semantic_path();
    if !source_path.is_file() {
        return Ok(SemanticMigrationReport::empty(
            request,
            SemanticMigrationOutcome::NotNeeded,
        ));
    }

    let root = fs::canonicalize(&request.project_root)?;
    let view = ViewStore::open(&request.storage, &request.view)?;
    if view.current_generation()?.is_some() {
        return Ok(SemanticMigrationReport::empty(
            request,
            SemanticMigrationOutcome::AlreadyPublished,
        ));
    }

    let raw = fs::read(&source_path)?;
    let parsed = match parse_legacy_snapshot(&raw, &request.configured_model_fingerprint) {
        Ok(parsed) => parsed,
        Err(reason) => return record_rebuild_request(request, &view, reason),
    };

    let mut candidates = Vec::new();
    let mut skipped_rows = 0;
    let mut outside_root_rows = 0;
    let mut stale_rows = 0;
    for file in parsed.files {
        let Some(rel_path) = legacy_path_to_rel_path(&file.path, &root) else {
            outside_root_rows += 1;
            skipped_rows += 1;
            crate::slog_warn!(
                "semantic migration skipped legacy row outside root {}: {}",
                root.display(),
                legacy_path_display(&file.path)
            );
            continue;
        };
        let source_path = root.join(os_path_from_bytes(rel_path.as_bytes()));
        let source = match fs::read(&source_path) {
            Ok(source) => source,
            Err(error) => {
                skipped_rows += 1;
                stale_rows += 1;
                crate::slog_warn!(
                    "semantic migration skipped unreadable legacy row {}: {}",
                    source_path.display(),
                    error
                );
                continue;
            }
        };
        if file.content_hash.iter().all(|byte| *byte == 0)
            || blake3::hash(&source).as_bytes() != &file.content_hash
        {
            skipped_rows += 1;
            stale_rows += 1;
            crate::slog_warn!(
                "semantic migration skipped changed legacy row {}",
                source_path.display()
            );
            continue;
        }

        let key = SemanticKey::from_bytes(
            &source,
            rel_path.as_bytes(),
            &request.chunker_version,
            &request.embed_template_version,
            &request.configured_model_fingerprint,
        )
        .full_key();
        candidates.push(ImportCandidate {
            rel_path,
            mode: regular_mode(&source_path)?,
            key,
            payload: encode_imported_payload(request, &file.entries),
        });
    }
    candidates.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));

    let mut store = BlobStore::open(
        &request.storage,
        request.family.clone(),
        crate::blob_store::BlobPlane::Semantic,
    )?;
    let alias_store = AliasStore::open(&request.storage, &request.family)?;
    let artifacts =
        create_publication_artifacts(view.view_dir(), store.path(), alias_store.path())?;

    let keys = candidates
        .iter()
        .map(|candidate| candidate.key.clone())
        .collect::<Vec<_>>();
    let generation = migration_generation(&raw);
    let mut pin = (!keys.is_empty())
        .then(|| {
            crate::pins::AssemblyPin::create(
                view.view_dir(),
                request.family.clone(),
                request.view.clone(),
                generation.clone(),
                &keys,
            )
        })
        .transpose()?;

    let mut entries = Vec::with_capacity(candidates.len());
    let mut semantic_keys = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if let Some(pin) = pin.as_mut() {
            // Renewal happens before each put; failure leaves the legacy source and
            // current pointer untouched so a later attempt can restart safely.
            pin.renew_if_due()?;
        }
        let put = store.put(&candidate.key, &candidate.payload)?;
        if !put.durable {
            return Err(MigrationError::UndurablePut(put.outcome));
        }
        let key = candidate.key.to_hex();
        semantic_keys.push(key.clone());
        entries.push((
            candidate.rel_path,
            ManifestEntry::Regular {
                mode: candidate.mode,
                planes: RegularPlanes {
                    semantic: Some(key),
                    callgraph: None,
                },
                resolution_input: false,
            },
        ));
    }

    let manifest = Manifest::new(entries)?;
    let closure = MigrationClosure {
        semantic_database: store.path().to_path_buf(),
        trigram: artifacts.trigram_artifact.clone(),
    };
    let publication = view.publish(
        &PublicationRequest {
            generation: &generation,
            base_generation: None,
            manifest: &manifest,
            artifacts,
            closure_requirements: ClosureRequirements::default(),
        },
        &closure,
    )?;
    if let Some(pin) = pin.as_mut() {
        pin.release();
    }

    let outcome = match publication {
        PublishOutcome::Published => SemanticMigrationOutcome::Imported,
        PublishOutcome::Conflict { current_generation } => {
            SemanticMigrationOutcome::PublishConflict { current_generation }
        }
    };
    crate::slog_info!(
        "semantic migration outcome={:?} imported_rows={} skipped_rows={} outside_root_rows={} reembeds=0",
        outcome,
        semantic_keys.len(),
        skipped_rows,
        outside_root_rows
    );
    Ok(SemanticMigrationReport {
        outcome,
        imported_rows: semantic_keys.len(),
        skipped_rows,
        outside_root_rows,
        stale_rows,
        reembedded_rows: 0,
        semantic_keys,
        chunker_version: request.chunker_version.clone(),
        embed_template_version: request.embed_template_version.clone(),
        model_fingerprint: request.configured_model_fingerprint.clone(),
    })
}

/// Runs the existing staged cold callgraph build exactly when no usable store exists.
/// A later rebind opens the published store and returns [`CallgraphMigrationOutcome::AlreadyCurrent`].
pub fn rebuild_legacy_callgraph_once(
    storage: &Path,
    project_root: &Path,
    chunk_size: usize,
) -> Result<CallgraphMigrationOutcome, MigrationError> {
    let project_root = fs::canonicalize(project_root)?;
    let family = crate::search_index::artifact_cache_key(&project_root);
    let callgraph_dir = storage.join("callgraph").join(family);
    let files = crate::callgraph::walk_project_files(&project_root).collect::<Vec<_>>();
    let (_store, rebuilt) =
        crate::callgraph_store::CallGraphStore::ensure_built_with_lease_chunked(
            callgraph_dir,
            project_root,
            &files,
            chunk_size,
        )?;
    Ok(if rebuilt.is_some() {
        CallgraphMigrationOutcome::Rebuilt
    } else {
        CallgraphMigrationOutcome::AlreadyCurrent
    })
}

fn record_rebuild_request(
    request: &SemanticMigrationRequest,
    view: &ViewStore,
    reason: String,
) -> Result<SemanticMigrationReport, MigrationError> {
    let state_path = view.view_dir().join("migration-state.json");
    if state_path.is_file() {
        return Ok(SemanticMigrationReport::empty(
            request,
            SemanticMigrationOutcome::RebuildAlreadyScheduled { reason },
        ));
    }
    let state = RebuildState {
        disposition: "rebuild".to_string(),
        reason: reason.clone(),
    };
    write_rebuild_state(&state_path, &state)?;
    crate::slog_info!(
        "semantic migration rebuild required for {}: {}",
        request.project_root.display(),
        reason
    );
    Ok(SemanticMigrationReport::empty(
        request,
        SemanticMigrationOutcome::RebuildRequired { reason },
    ))
}

#[derive(Serialize)]
struct RebuildState {
    disposition: String,
    reason: String,
}

fn write_rebuild_state(path: &Path, state: &RebuildState) -> Result<(), MigrationError> {
    let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let result = (|| -> Result<(), MigrationError> {
        let mut file = File::create(&temporary)?;
        serde_json::to_writer(&mut file, state)
            .map_err(|error| MigrationError::InvalidLegacySnapshot(error.to_string()))?;
        use std::io::Write as _;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        crate::fs_lock::rename_over(&temporary, path)?;
        crate::fs_lock::sync_parent(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

struct ImportCandidate {
    rel_path: RelPath,
    mode: u32,
    key: FullKey,
    payload: Vec<u8>,
}

struct MigrationClosure {
    semantic_database: PathBuf,
    trigram: PathBuf,
}

impl PublicationClosure for MigrationClosure {
    fn contains_blob(&self, plane: ArtifactPlane, full_key: &str) -> crate::views::Result<bool> {
        if plane != ArtifactPlane::Semantic {
            return Ok(false);
        }
        let Some(key) = decode_hex_key(full_key) else {
            return Ok(false);
        };
        Ok(Connection::open(&self.semantic_database)?
            .query_row(
                "SELECT 1 FROM blob_payloads WHERE full_key = ?1",
                params![key.as_slice()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    fn trigram_is_present(&self) -> crate::views::Result<bool> {
        Ok(self.trigram.is_file())
    }

    fn contains_alias(&self, _git_oid: &str) -> crate::views::Result<bool> {
        Ok(false)
    }
}

fn create_publication_artifacts(
    view_dir: &Path,
    semantic_database: &Path,
    alias_database: &Path,
) -> Result<PublicationArtifacts, MigrationError> {
    let derived_database = view_dir.join("derived.sqlite");
    let derived = Connection::open(&derived_database)?;
    derived.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS migration_derived (singleton INTEGER PRIMARY KEY);",
    )?;
    drop(derived);

    let trigram_artifact = view_dir.join("trigram.bin");
    let trigram = File::create(&trigram_artifact)?;
    trigram.sync_all()?;
    drop(trigram);
    crate::fs_lock::sync_parent(&trigram_artifact);

    Ok(PublicationArtifacts {
        blob_databases: vec![semantic_database.to_path_buf()],
        derived_database,
        trigram_artifact,
        alias_database: alias_database.to_path_buf(),
    })
}

fn migration_generation(raw: &[u8]) -> String {
    let digest = blake3::hash(raw).to_hex();
    format!("migration-{}", &digest[..16])
}

fn regular_mode(path: &Path) -> Result<u32, io::Error> {
    let metadata = fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        return Ok(if metadata.permissions().mode() & 0o111 == 0 {
            0o100644
        } else {
            0o100755
        });
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(0o100644)
    }
}

fn legacy_path_to_rel_path(raw: &[u8], root: &Path) -> Option<RelPath> {
    let legacy = os_path_from_bytes(raw);
    if legacy.is_absolute() {
        let relative = legacy.strip_prefix(root).ok()?;
        RelPath::from_os_path(relative).ok()
    } else {
        RelPath::from_os_path(&legacy).ok()
    }
}

fn os_path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn legacy_path_display(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn decode_hex_key(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64 {
        return None;
    }
    (0..64)
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}

fn encode_imported_payload(request: &SemanticMigrationRequest, entries: &[LegacyEntry]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(IMPORTED_PAYLOAD_VERSION);
    push_bytes(&mut payload, request.chunker_version.as_bytes());
    push_bytes(&mut payload, request.embed_template_version.as_bytes());
    push_bytes(
        &mut payload,
        request.configured_model_fingerprint.as_bytes(),
    );
    payload.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        push_bytes(&mut payload, entry.name.as_bytes());
        push_bytes(
            &mut payload,
            entry
                .qualified_name
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
        payload.push(entry.kind);
        payload.extend_from_slice(&entry.start_line.to_le_bytes());
        payload.extend_from_slice(&entry.end_line.to_le_bytes());
        payload.push(u8::from(entry.exported));
        push_bytes(&mut payload, entry.snippet.as_bytes());
        push_bytes(&mut payload, entry.embed_text.as_bytes());
        push_bytes(&mut payload, &entry.vector);
    }
    payload
}

fn push_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(bytes);
}

struct ParsedLegacySnapshot {
    files: Vec<LegacyFile>,
}

struct LegacyFile {
    path: Vec<u8>,
    content_hash: [u8; 32],
    entries: Vec<LegacyEntry>,
}

struct LegacyEntry {
    name: String,
    qualified_name: Option<String>,
    kind: u8,
    start_line: u32,
    end_line: u32,
    exported: bool,
    snippet: String,
    embed_text: String,
    vector: Vec<u8>,
}

fn parse_legacy_snapshot(
    bytes: &[u8],
    configured_model_fingerprint: &str,
) -> Result<ParsedLegacySnapshot, String> {
    let mut reader = LegacyReader::new(bytes);
    let version = reader.read_u8()?;
    if version != LEGACY_SEMANTIC_V6 && version != LEGACY_SEMANTIC_V7 {
        return Err(format!(
            "semantic.bin version {version} has no established producer stamps"
        ));
    }
    let dimension = reader.read_u32()? as usize;
    if dimension == 0 || dimension > MAX_LEGACY_DIMENSION {
        return Err(format!("invalid semantic dimension {dimension}"));
    }
    let entry_count = reader.read_u32()? as usize;
    if entry_count > MAX_LEGACY_ROWS {
        return Err(format!("too many semantic entries {entry_count}"));
    }
    let fingerprint = reader.read_string()?;
    if !fingerprint_is_compatible(&fingerprint, configured_model_fingerprint) {
        return Err("model fingerprint or chunker version is incompatible".to_string());
    }

    let metadata_count = reader.read_u32()? as usize;
    if metadata_count > MAX_LEGACY_ROWS {
        return Err(format!("too many semantic metadata rows {metadata_count}"));
    }
    let mut metadata = BTreeMap::new();
    for _ in 0..metadata_count {
        let path = reader.read_bytes()?;
        let _seconds = reader.read_u64()?;
        let nanos = reader.read_u32()?;
        if nanos >= 1_000_000_000 {
            return Err(format!("invalid semantic mtime nanos {nanos}"));
        }
        let _size = reader.read_u64()?;
        let content_hash = reader.read_array_32()?;
        if metadata.insert(path.clone(), content_hash).is_some() {
            return Err(format!(
                "duplicate semantic metadata path {}",
                legacy_path_display(&path)
            ));
        }
    }

    let mut entries_by_path = BTreeMap::<Vec<u8>, Vec<LegacyEntry>>::new();
    for _ in 0..entry_count {
        let path = reader.read_bytes()?;
        let name = reader.read_string()?;
        let qualified_name = if version == LEGACY_SEMANTIC_V7 {
            let qualified = reader.read_string()?;
            (!qualified.is_empty()).then_some(qualified)
        } else {
            None
        };
        let kind = reader.read_u8()?;
        let start_line = reader.read_u32()?;
        let end_line = reader.read_u32()?;
        let exported = reader.read_u8()? != 0;
        let snippet = reader.read_string()?;
        let embed_text = reader.read_string()?;
        let vector_len = dimension
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| "semantic vector length overflow".to_string())?;
        let vector = reader.read_exact(vector_len)?.to_vec();
        entries_by_path.entry(path).or_default().push(LegacyEntry {
            name,
            qualified_name,
            kind,
            start_line,
            end_line,
            exported,
            snippet,
            embed_text,
            vector,
        });
    }
    if !reader.is_exhausted() {
        return Err("trailing bytes after semantic snapshot".to_string());
    }

    let mut files = Vec::with_capacity(metadata.len());
    for (path, content_hash) in metadata {
        files.push(LegacyFile {
            entries: entries_by_path.remove(&path).unwrap_or_default(),
            path,
            content_hash,
        });
    }
    // An entry without metadata cannot prove that it belongs to this root. It is
    // intentionally not imported rather than attaching an unverifiable vector.
    if !entries_by_path.is_empty() {
        crate::slog_warn!(
            "semantic migration skipped {} entry path(s) without file metadata",
            entries_by_path.len()
        );
    }
    Ok(ParsedLegacySnapshot { files })
}

fn fingerprint_is_compatible(legacy: &str, configured: &str) -> bool {
    let Ok(legacy) = serde_json::from_str::<serde_json::Value>(legacy) else {
        return false;
    };
    let Ok(configured) = serde_json::from_str::<serde_json::Value>(configured) else {
        return false;
    };
    legacy == configured
        && legacy
            .get("chunking_version")
            .and_then(serde_json::Value::as_u64)
            == Some(LEGACY_CHUNKING_VERSION)
}

struct LegacyReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> LegacyReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn is_exhausted(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| "semantic snapshot offset overflow".to_string())?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| "unexpected end of semantic snapshot".to_string())?;
        self.offset = end;
        Ok(bytes)
    }

    fn read_u8(&mut self) -> Result<u8, String> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        let bytes: [u8; 4] = self
            .read_exact(4)?
            .try_into()
            .map_err(|_| "invalid u32 field".to_string())?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_u64(&mut self) -> Result<u64, String> {
        let bytes: [u8; 8] = self
            .read_exact(8)?
            .try_into()
            .map_err(|_| "invalid u64 field".to_string())?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_array_32(&mut self) -> Result<[u8; 32], String> {
        self.read_exact(32)?
            .try_into()
            .map_err(|_| "invalid 32-byte field".to_string())
    }

    fn read_bytes(&mut self) -> Result<Vec<u8>, String> {
        let len = self.read_u32()? as usize;
        if len > self.bytes.len().saturating_sub(self.offset) {
            return Err("semantic string exceeds remaining snapshot bytes".to_string());
        }
        Ok(self.read_exact(len)?.to_vec())
    }

    fn read_string(&mut self) -> Result<String, String> {
        String::from_utf8(self.read_bytes()?)
            .map_err(|_| "semantic string is not UTF-8".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatible_fingerprint_requires_the_current_chunker() {
        let fingerprint = serde_json::json!({
            "backend": "test",
            "model": "test",
            "base_url": "test",
            "dimension": 3,
            "chunking_version": 2,
        })
        .to_string();
        assert!(fingerprint_is_compatible(&fingerprint, &fingerprint));
        let older = fingerprint.replace("\"chunking_version\":2", "\"chunking_version\":1");
        assert!(!fingerprint_is_compatible(&older, &older));
    }

    #[test]
    fn imported_payload_carries_the_importing_producer_stamps() {
        let request = SemanticMigrationRequest {
            storage: PathBuf::new(),
            project_root: PathBuf::new(),
            family: "family".to_string(),
            view: "view".to_string(),
            configured_model_fingerprint: "model".to_string(),
            chunker_version: "chunker".to_string(),
            embed_template_version: "template".to_string(),
        };
        let payload = encode_imported_payload(&request, &[]);
        assert_eq!(payload[0], IMPORTED_PAYLOAD_VERSION);
        assert!(payload
            .windows(b"chunker".len())
            .any(|part| part == b"chunker"));
        assert!(payload
            .windows(b"template".len())
            .any(|part| part == b"template"));
    }
}
