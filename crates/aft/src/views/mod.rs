//! Per-checkout view manifests and durable generation publication.
//!
//! Blob stores are family-scoped, while this module owns the checkout-scoped
//! assembly that names one immutable manifest per generation.  The only durable
//! membership representation is the manifest itself; closure inputs remain
//! transient while a generation is published.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Version number for the byte encoding used to identify manifest paths.
/// Increment it whenever that encoding changes.
pub const PATH_IDENTITY_VERSION: u8 = 1;
const POINTER_DATABASE: &str = "pointer.sqlite";
const POINTER_BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Errors raised while constructing, verifying, or publishing a view.
#[derive(Debug)]
pub enum ViewError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    InvalidManifest(String),
    ManifestAlreadyExists(String),
    MissingBlob { plane: ArtifactPlane, key: String },
    MissingTrigram,
    MissingAlias(String),
}

impl fmt::Display for ViewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "view I/O failed: {error}"),
            Self::Sqlite(error) => write!(formatter, "view SQLite operation failed: {error}"),
            Self::Json(error) => write!(formatter, "view manifest JSON failed: {error}"),
            Self::InvalidManifest(message) => write!(formatter, "invalid view manifest: {message}"),
            Self::ManifestAlreadyExists(generation) => {
                write!(
                    formatter,
                    "a manifest already exists for generation {generation}"
                )
            }
            Self::MissingBlob { plane, key } => {
                write!(
                    formatter,
                    "manifest references missing {plane:?} blob {key}"
                )
            }
            Self::MissingTrigram => write!(
                formatter,
                "published generation is missing its trigram state"
            ),
            Self::MissingAlias(oid) => write!(
                formatter,
                "published generation references missing alias {oid}"
            ),
        }
    }
}

impl std::error::Error for ViewError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ViewError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for ViewError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<serde_json::Error> for ViewError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Result type used by the view publication API.
pub type Result<T> = std::result::Result<T, ViewError>;

/// A byte-exact manifest value. JSON uses a UTF-8 string when possible and a
/// `{"b64": ...}` object otherwise, so no path bytes are lost at the boundary.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct ByteString(Vec<u8>);

impl ByteString {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl From<Vec<u8>> for ByteString {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes)
    }
}

impl From<&[u8]> for ByteString {
    fn from(bytes: &[u8]) -> Self {
        Self::new(bytes)
    }
}

impl Serialize for ByteString {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match std::str::from_utf8(&self.0) {
            Ok(text) => serializer.serialize_str(text),
            Err(_) => {
                let mut encoded = BTreeMap::new();
                encoded.insert("b64", STANDARD.encode(&self.0));
                encoded.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for ByteString {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(text) => Ok(Self::new(text.into_bytes())),
            serde_json::Value::Object(mut object) if object.len() == 1 => {
                let encoded = object
                    .remove("b64")
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .ok_or_else(|| {
                        D::Error::custom("byte string object must contain only string b64")
                    })?;
                STANDARD
                    .decode(encoded)
                    .map(Self::new)
                    .map_err(D::Error::custom)
            }
            _ => Err(D::Error::custom(
                "byte string must be a UTF-8 string or an object with b64",
            )),
        }
    }
}

/// A relative path key stored as exact bytes in bytewise canonical order.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RelPath(ByteString);

impl RelPath {
    /// Creates a manifest key from slash-separated relative path bytes.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        validate_filesystem_rel_path(&bytes)?;
        Ok(Self(ByteString::new(bytes)))
    }

    /// Creates the reserved key for a manifest-only synthetic member.
    pub fn synthetic(name: &str) -> Result<Self> {
        if name.is_empty() || name.as_bytes().contains(&0) {
            return Err(ViewError::InvalidManifest(
                "synthetic entry names must be non-empty and contain no NUL".to_string(),
            ));
        }
        let mut key = Vec::with_capacity(name.len() + 1);
        key.push(0);
        key.extend_from_slice(name.as_bytes());
        Ok(Self(ByteString::new(key)))
    }

    /// Converts an OS path to the slash-separated byte identity used in a manifest.
    pub fn from_os_path(path: &Path) -> Result<Self> {
        if path.is_absolute() {
            return Err(ViewError::InvalidManifest(
                "rel_path must not be absolute".to_string(),
            ));
        }

        #[cfg(unix)]
        let bytes = {
            use std::os::unix::ffi::OsStrExt as _;
            path.as_os_str().as_bytes().to_vec()
        };
        #[cfg(windows)]
        let bytes: Vec<u8> = path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .map(|byte| if *byte == b'\\' { b'/' } else { *byte })
            .collect();
        #[cfg(all(not(unix), not(windows)))]
        let bytes = path.as_os_str().as_encoded_bytes().to_vec();

        Self::new(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn is_synthetic(&self) -> bool {
        self.0.as_bytes().first() == Some(&0)
    }
}

impl Serialize for RelPath {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RelPath {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = ByteString::deserialize(deserializer)?.into_bytes();
        if bytes.first() == Some(&0) {
            if bytes.len() == 1 || bytes[1..].contains(&0) {
                return Err(D::Error::custom("invalid synthetic rel_path"));
            }
            Ok(Self(ByteString::new(bytes)))
        } else {
            Self::new(bytes).map_err(D::Error::custom)
        }
    }
}

/// One of the content-addressed artifact planes referenced by a manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactPlane {
    Semantic,
    Callgraph,
}

/// Full keys for a regular file's per-file artifacts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RegularPlanes {
    pub semantic: Option<String>,
    pub callgraph: Option<String>,
}

/// Full keys for a synthetic member. Synthetic state participates only in the
/// callgraph join, so it cannot accidentally acquire a semantic key.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SyntheticPlanes {
    pub callgraph: String,
}

/// The tagged union stored for every manifest member. Each entry serializes a
/// `kind` field that identifies its variant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManifestEntry {
    Regular {
        mode: u32,
        planes: RegularPlanes,
        resolution_input: bool,
    },
    Symlink {
        target_bytes: ByteString,
    },
    Gitlink {
        oid: String,
    },
    Synthetic {
        name: String,
        planes: SyntheticPlanes,
    },
}

/// The variant name used for member-by-member manifest assertions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManifestEntryKind {
    Regular,
    Symlink,
    Gitlink,
    Synthetic,
}

impl ManifestEntry {
    pub fn kind(&self) -> ManifestEntryKind {
        match self {
            Self::Regular { .. } => ManifestEntryKind::Regular,
            Self::Symlink { .. } => ManifestEntryKind::Symlink,
            Self::Gitlink { .. } => ManifestEntryKind::Gitlink,
            Self::Synthetic { .. } => ManifestEntryKind::Synthetic,
        }
    }
}

/// The one manifest that defines a view generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Manifest {
    pub path_identity_version: u8,
    entries: BTreeMap<RelPath, ManifestEntry>,
}

impl Manifest {
    pub fn new(entries: impl IntoIterator<Item = (RelPath, ManifestEntry)>) -> Result<Self> {
        let mut manifest = Self {
            path_identity_version: PATH_IDENTITY_VERSION,
            entries: BTreeMap::new(),
        };
        for (rel_path, entry) in entries {
            manifest.insert(rel_path, entry)?;
        }
        Ok(manifest)
    }

    pub fn insert(&mut self, rel_path: RelPath, entry: ManifestEntry) -> Result<()> {
        validate_member(&rel_path, &entry)?;
        if self.entries.insert(rel_path, entry).is_some() {
            return Err(ViewError::InvalidManifest(
                "a manifest cannot contain the same rel_path twice".to_string(),
            ));
        }
        Ok(())
    }

    pub fn entries(&self) -> impl Iterator<Item = (&RelPath, &ManifestEntry)> {
        self.entries.iter()
    }

    pub fn get(&self, rel_path: &RelPath) -> Option<&ManifestEntry> {
        self.entries.get(rel_path)
    }

    pub fn to_json_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }

    fn plane_keys(&self) -> impl Iterator<Item = (ArtifactPlane, &str)> {
        self.entries.values().flat_map(|entry| match entry {
            ManifestEntry::Regular { planes, .. } => [
                planes
                    .semantic
                    .as_deref()
                    .map(|key| (ArtifactPlane::Semantic, key)),
                planes
                    .callgraph
                    .as_deref()
                    .map(|key| (ArtifactPlane::Callgraph, key)),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>(),
            ManifestEntry::Synthetic { planes, .. } => {
                vec![(ArtifactPlane::Callgraph, planes.callgraph.as_str())]
            }
            ManifestEntry::Symlink { .. } | ManifestEntry::Gitlink { .. } => Vec::new(),
        })
    }
}

#[derive(Deserialize, Serialize)]
struct JsonManifestEntry {
    rel_path: RelPath,
    #[serde(flatten)]
    entry: ManifestEntry,
}

#[derive(Deserialize, Serialize)]
struct JsonManifest {
    path_identity_version: u8,
    entries: Vec<JsonManifestEntry>,
}

impl Serialize for Manifest {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let entries = self
            .entries
            .iter()
            .map(|(rel_path, entry)| JsonManifestEntry {
                rel_path: rel_path.clone(),
                entry: entry.clone(),
            })
            .collect();
        JsonManifest {
            path_identity_version: self.path_identity_version,
            entries,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Manifest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let json = JsonManifest::deserialize(deserializer)?;
        if json.path_identity_version != PATH_IDENTITY_VERSION {
            return Err(D::Error::custom(format!(
                "unsupported path_identity_version {}",
                json.path_identity_version
            )));
        }
        Self::new(
            json.entries
                .into_iter()
                .map(|member| (member.rel_path, member.entry)),
        )
        .map_err(D::Error::custom)
    }
}

/// A transient list of aliases that a derived generation references. It is not
/// serialized beside the manifest, preventing a side closure table from becoming
/// a second membership authority.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClosureRequirements {
    pub referenced_aliases: BTreeSet<String>,
}

/// Presence checks used by durable restart and manifest-membership validation.
pub trait PublicationClosure {
    fn contains_blob(&self, plane: ArtifactPlane, full_key: &str) -> Result<bool>;
    fn trigram_is_present(&self) -> Result<bool>;
    fn contains_alias(&self, git_oid: &str) -> Result<bool>;
}

/// Ensures that a generation never publishes a manifest pointing at incomplete
/// blob, trigram, or alias state.
pub fn probe_publication_closure(
    manifest: &Manifest,
    requirements: &ClosureRequirements,
    closure: &impl PublicationClosure,
) -> Result<()> {
    for (plane, key) in manifest.plane_keys() {
        if !closure.contains_blob(plane, key)? {
            return Err(ViewError::MissingBlob {
                plane,
                key: key.to_owned(),
            });
        }
    }
    if !closure.trigram_is_present()? {
        return Err(ViewError::MissingTrigram);
    }
    for oid in &requirements.referenced_aliases {
        if !closure.contains_alias(oid)? {
            return Err(ViewError::MissingAlias(oid.clone()));
        }
    }
    Ok(())
}

/// Files whose committed state must be durable before a pointer can name a new
/// generation. All SQLite paths are checkpointed with `PASSIVE` before fsync.
#[derive(Clone, Debug)]
pub struct PublicationArtifacts {
    pub blob_databases: Vec<PathBuf>,
    pub derived_database: PathBuf,
    pub trigram_artifact: PathBuf,
    pub alias_database: PathBuf,
}

/// Input to a single manifest publication attempt.
#[derive(Clone, Debug)]
pub struct PublicationRequest<'a> {
    pub generation: &'a str,
    /// `None` is the empty initial pointer. A conflict reports the winning base
    /// so callers can re-derive and retry without overwriting it.
    pub base_generation: Option<&'a str>,
    pub manifest: &'a Manifest,
    pub artifacts: PublicationArtifacts,
    pub closure_requirements: ClosureRequirements,
}

/// Observable publication stages, primarily for failpoint and durability-order
/// tests. Production callers do not need to retain the sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationStep {
    /// The SQLite WAL checkpoint completed; fsync has not begun yet.
    BlobWalCheckpointed,
    BlobWalFsync,
    DerivedAndTrigramDurable,
    AliasRowsDurable,
    ClosureProbed,
    ManifestFileWritten,
    ManifestParentFsynced,
    PointerCas,
    PointerCheckpointed,
    PointerDatabaseFsynced,
    PointerDirectoryFsynced,
}

pub trait PublicationObserver {
    fn reached(&self, step: PublicationStep);
}

/// The result of comparing a generation's base pointer in SQLite.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    Published,
    Conflict { current_generation: Option<String> },
}

/// SQLite settings required for the per-view pointer connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PointerPragmas {
    pub journal_mode: String,
    pub synchronous: i64,
    pub busy_timeout_ms: i64,
    pub foreign_keys: i64,
}

/// Checkout-scoped storage for immutable manifests and the SQLite current pointer.
#[derive(Clone, Debug)]
pub struct ViewStore {
    view_dir: PathBuf,
}

impl ViewStore {
    /// Opens `<storage>/views/<project_scope_key>` and initializes its singleton
    /// pointer row under `BEGIN IMMEDIATE` so concurrent first opens agree.
    pub fn open(storage: impl AsRef<Path>, project_scope_key: &str) -> Result<Self> {
        validate_scope_key(project_scope_key)?;
        let view_dir = storage.as_ref().join("views").join(project_scope_key);
        fs::create_dir_all(&view_dir)?;
        let store = Self { view_dir };
        store.initialize_pointer()?;
        Ok(store)
    }

    pub fn view_dir(&self) -> &Path {
        &self.view_dir
    }

    pub fn pointer_path(&self) -> PathBuf {
        self.view_dir.join(POINTER_DATABASE)
    }

    pub fn manifest_path(&self, generation: &str) -> Result<PathBuf> {
        validate_generation(generation)?;
        Ok(self.view_dir.join(format!("manifest-{generation}.json")))
    }

    pub fn current_generation(&self) -> Result<Option<String>> {
        let connection = self.open_pointer_connection()?;
        let generation: String = connection.query_row(
            "SELECT generation FROM pointer WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        Ok((!generation.is_empty()).then_some(generation))
    }

    /// Reads the settings installed on a new pointer connection. SQLite keeps
    /// `synchronous`, `busy_timeout`, and `foreign_keys` per connection, so this
    /// check must use the same open path as publication rather than a raw handle.
    pub fn pointer_pragmas(&self) -> Result<PointerPragmas> {
        let connection = self.open_pointer_connection()?;
        Ok(PointerPragmas {
            journal_mode: connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?,
            synchronous: connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?,
            busy_timeout_ms: connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?,
            foreign_keys: connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?,
        })
    }

    pub fn load_manifest(&self, generation: &str) -> Result<Manifest> {
        Ok(Manifest::from_json_bytes(&fs::read(
            self.manifest_path(generation)?,
        )?)?)
    }

    /// Publishes a fully durable generation. The manifest is written exactly once;
    /// a lost pointer race leaves that immutable manifest unreferenced so a caller
    /// can safely re-derive from the winner without losing either update.
    pub fn publish(
        &self,
        request: &PublicationRequest<'_>,
        closure: &impl PublicationClosure,
    ) -> Result<PublishOutcome> {
        self.publish_with_observer(request, closure, None)
    }

    pub fn publish_with_observer(
        &self,
        request: &PublicationRequest<'_>,
        closure: &impl PublicationClosure,
        observer: Option<&dyn PublicationObserver>,
    ) -> Result<PublishOutcome> {
        validate_generation(request.generation)?;
        if request.manifest.path_identity_version != PATH_IDENTITY_VERSION {
            return Err(ViewError::InvalidManifest(
                "manifest has an unsupported path identity version".to_string(),
            ));
        }
        let manifest_path = self.manifest_path(request.generation)?;
        if manifest_path.exists() {
            return Err(ViewError::ManifestAlreadyExists(
                request.generation.to_string(),
            ));
        }

        for path in &request.artifacts.blob_databases {
            checkpoint_and_sync_database(path, observer)?;
        }
        observe(observer, PublicationStep::BlobWalFsync);

        checkpoint_and_sync_database(&request.artifacts.derived_database, observer)?;
        sync_file_and_parent(&request.artifacts.trigram_artifact)?;
        observe(observer, PublicationStep::DerivedAndTrigramDurable);

        checkpoint_and_sync_database(&request.artifacts.alias_database, observer)?;
        observe(observer, PublicationStep::AliasRowsDurable);

        probe_publication_closure(request.manifest, &request.closure_requirements, closure)?;
        observe(observer, PublicationStep::ClosureProbed);

        write_manifest_once(&manifest_path, request.manifest)?;
        observe(observer, PublicationStep::ManifestFileWritten);
        sync_directory(&self.view_dir)?;
        observe(observer, PublicationStep::ManifestParentFsynced);

        let outcome = self.compare_and_swap_pointer(
            request.generation,
            request.base_generation.unwrap_or_default(),
        )?;
        observe(observer, PublicationStep::PointerCas);

        if outcome == PublishOutcome::Published {
            checkpoint_pointer_after_cas(&self.pointer_path())?;
            observe(observer, PublicationStep::PointerCheckpointed);
            sync_file(&self.pointer_path())?;
            observe(observer, PublicationStep::PointerDatabaseFsynced);
            sync_directory(&self.view_dir)?;
            observe(observer, PublicationStep::PointerDirectoryFsynced);
        }
        Ok(outcome)
    }

    fn initialize_pointer(&self) -> Result<()> {
        let connection = self.open_pointer_connection()?;
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS pointer (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 generation TEXT NOT NULL
             );
             INSERT OR IGNORE INTO pointer (singleton, generation) VALUES (1, '');
             COMMIT;",
        )?;
        Ok(())
    }

    fn open_pointer_connection(&self) -> Result<Connection> {
        let connection = Connection::open(self.pointer_path())?;
        configure_connection(&connection)?;
        Ok(connection)
    }

    fn compare_and_swap_pointer(
        &self,
        generation: &str,
        base_generation: &str,
    ) -> Result<PublishOutcome> {
        let mut connection = self.open_pointer_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = transaction.execute(
            "UPDATE pointer SET generation = ?1 WHERE generation = ?2",
            params![generation, base_generation],
        )?;
        if updated == 1 {
            transaction.commit()?;
            Ok(PublishOutcome::Published)
        } else {
            let current_generation: String = transaction.query_row(
                "SELECT generation FROM pointer WHERE singleton = 1",
                [],
                |row| row.get(0),
            )?;
            transaction.commit()?;
            Ok(PublishOutcome::Conflict {
                current_generation: (!current_generation.is_empty()).then_some(current_generation),
            })
        }
    }
}

fn validate_filesystem_rel_path(bytes: &[u8]) -> Result<()> {
    if bytes.is_empty()
        || bytes.first() == Some(&b'/')
        || bytes.contains(&0)
        || bytes
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err(ViewError::InvalidManifest(
            "rel_path must be a non-empty relative slash-separated byte path without NUL or traversal"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_member(rel_path: &RelPath, entry: &ManifestEntry) -> Result<()> {
    match entry {
        ManifestEntry::Synthetic { name, .. } => {
            if rel_path.as_bytes()
                != [b'\0']
                    .into_iter()
                    .chain(name.as_bytes().iter().copied())
                    .collect::<Vec<_>>()
            {
                return Err(ViewError::InvalidManifest(
                    "synthetic entries must use the reserved \\0<name> rel_path key".to_string(),
                ));
            }
        }
        ManifestEntry::Regular { mode, .. } => {
            if rel_path.is_synthetic() {
                return Err(ViewError::InvalidManifest(
                    "only synthetic entries may use a leading NUL rel_path".to_string(),
                ));
            }
            if !matches!(*mode, 0o100644 | 0o100755) {
                return Err(ViewError::InvalidManifest(
                    "regular manifest entries must use mode 100644 or 100755".to_string(),
                ));
            }
        }
        ManifestEntry::Symlink { .. } | ManifestEntry::Gitlink { .. }
            if rel_path.is_synthetic() =>
        {
            return Err(ViewError::InvalidManifest(
                "only synthetic entries may use a leading NUL rel_path".to_string(),
            ));
        }
        ManifestEntry::Symlink { .. } | ManifestEntry::Gitlink { .. } => {}
    }
    Ok(())
}

fn validate_generation(generation: &str) -> Result<()> {
    if generation.is_empty()
        || generation.contains(['/', '\\', '\0'])
        || generation == "."
        || generation == ".."
    {
        return Err(ViewError::InvalidManifest(
            "generation must be a non-empty file-name component".to_string(),
        ));
    }
    Ok(())
}

fn validate_scope_key(project_scope_key: &str) -> Result<()> {
    if project_scope_key.is_empty()
        || project_scope_key.contains(['/', '\\', '\0'])
        || project_scope_key == "."
        || project_scope_key == ".."
    {
        return Err(ViewError::InvalidManifest(
            "project_scope_key must be a non-empty directory-name component".to_string(),
        ));
    }
    Ok(())
}

fn configure_connection(connection: &Connection) -> Result<()> {
    connection.busy_timeout(POINTER_BUSY_TIMEOUT)?;
    // Two publishers opening the pointer database at once both switch it to
    // WAL; SQLite skips the busy handler on that lock upgrade when another
    // connection is mid-switch, so the loser sees SQLITE_BUSY immediately
    // (seen as `database is locked` 0.36 s into the CAS race test under a
    // full parallel gate). Wait it out within the busy budget instead.
    crate::blob_store::retry_while_busy(POINTER_BUSY_TIMEOUT, || {
        connection.pragma_update(None, "journal_mode", "WAL")
    })?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "OFF")?;
    Ok(())
}

fn checkpoint_and_sync_database(
    path: &Path,
    observer: Option<&dyn PublicationObserver>,
) -> Result<()> {
    if !path.is_file() {
        return Err(ViewError::InvalidManifest(format!(
            "durability input is not a SQLite file: {}",
            path.display()
        )));
    }
    let connection = Connection::open(path)?;
    configure_connection(&connection)?;
    connection.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
    observe(observer, PublicationStep::BlobWalCheckpointed);
    drop(connection);
    sync_file(path)?;
    // SQLite deletes the WAL when the last connection to the database closes,
    // so between an existence probe and the open the file can legitimately
    // vanish when another connection on the same file closes (the checkpoint
    // that removal implies already carried its frames into the main file).
    // Open directly and treat NotFound as "nothing left to sync" instead of
    // probing first.
    let wal_path = PathBuf::from(format!("{}-wal", path.display()));
    match open_file_for_sync(&wal_path) {
        Ok(file) => file.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ViewError::Io(error)),
    }
    sync_parent(path)
}

fn checkpoint_pointer_after_cas(path: &Path) -> Result<()> {
    let connection = Connection::open(path)?;
    configure_connection(&connection)?;
    connection.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
    Ok(())
}

fn write_manifest_once(path: &Path, manifest: &Manifest) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        ViewError::InvalidManifest("manifest path must have a parent directory".to_string())
    })?;
    let temporary = parent.join(format!(
        ".{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("manifest"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&manifest.to_json_bytes()?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::hard_link(&temporary, path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                ViewError::ManifestAlreadyExists(
                    path.file_stem()
                        .and_then(|name| name.to_str())
                        .unwrap_or("unknown")
                        .to_string(),
                )
            } else {
                ViewError::Io(error)
            }
        })?;
        // The hard link gives the generation its permanent name without an
        // overwrite race; fsync that name before its directory is persisted.
        sync_file(path)?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    write_result
}

fn sync_file_and_parent(path: &Path) -> Result<()> {
    sync_file(path)?;
    sync_parent(path)
}

fn sync_file(path: &Path) -> Result<()> {
    open_file_for_sync(path)?.sync_all()?;
    Ok(())
}

fn open_file_for_sync(path: &Path) -> std::io::Result<File> {
    #[cfg(windows)]
    {
        // FlushFileBuffers rejects read-only handles, so Windows must open
        // durability inputs for writing even though no bytes are changed.
        OpenOptions::new().write(true).open(path)
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        ViewError::InvalidManifest("durability input must have a parent directory".to_string())
    })?;
    sync_directory(parent)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    // Windows does not expose a portable directory handle fsync API. SQLite and
    // NTFS provide the durable rename boundary for these files on that platform.
    Ok(())
}

fn observe(observer: Option<&dyn PublicationObserver>, step: PublicationStep) {
    if let Some(observer) = observer {
        observer.reached(step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two publishers checkpoint the same artifact database before publishing.
    /// An artifact written in rollback-journal mode is switched to WAL by the
    /// first checkpoint connection, and SQLite skips the busy handler on that
    /// lock upgrade when another connection is mid-switch (the same first-open
    /// race the blob store pins), so the loser saw `database is locked` 0.36 s
    /// into the CAS race test under a full parallel gate. Eight racing
    /// checkpointers on a fresh rollback-journal file redden without the retry
    /// in `configure_connection` within 150 rounds.
    #[test]
    fn concurrent_artifact_checkpoints_never_see_busy() {
        let mut failures = Vec::new();
        for round in 0..150 {
            let dir = tempfile::tempdir().expect("tempdir");
            let artifact = dir.path().join("artifact.sqlite");
            Connection::open(&artifact)
                .expect("artifact")
                .execute_batch("CREATE TABLE t (x INTEGER);")
                .expect("schema");
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let artifact = artifact.clone();
                    std::thread::spawn(move || checkpoint_and_sync_database(&artifact, None))
                })
                .collect();
            for handle in handles {
                if let Err(error) = handle.join().expect("opener thread") {
                    failures.push(format!("round {round}: {error}"));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "concurrent artifact checkpoints must wait out the WAL switch: {failures:?}"
        );
    }

    #[test]
    fn sync_file_flushes_an_existing_artifact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let artifact = dir.path().join("artifact.bin");
        fs::write(&artifact, b"durable").expect("artifact");

        sync_file(&artifact).expect("flush pre-existing artifact");
    }

    #[test]
    fn byte_string_uses_base64_only_when_utf8_cannot_represent_the_path() {
        assert_eq!(
            serde_json::to_string(&ByteString::from(b"src/lib.rs".as_slice())).unwrap(),
            "\"src/lib.rs\""
        );
        assert_eq!(
            serde_json::to_string(&ByteString::from(b"bad-\xff".as_slice())).unwrap(),
            "{\"b64\":\"YmFkLf8=\"}"
        );
    }

    #[test]
    fn synthetic_paths_sort_before_filesystem_paths() {
        let synthetic = RelPath::synthetic("global-gitignore").unwrap();
        let regular = RelPath::new(b"src/main.rs".to_vec()).unwrap();
        assert!(synthetic < regular);
    }
}
