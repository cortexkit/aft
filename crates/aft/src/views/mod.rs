//! The file manifest the call-graph resolver joins over, and the parent
//! folder engine that fans a session bound to a folder of repositories out
//! to each repository's own index store.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

pub mod parent;

/// Version number for the byte encoding used to identify manifest paths.
/// Increment it whenever that encoding changes.
pub const PATH_IDENTITY_VERSION: u8 = 1;


/// Errors raised while constructing, verifying, or publishing a view.
#[derive(Debug)]
pub enum ViewError {
    Io(std::io::Error),
    IoAt {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    InvalidManifest(String),
    /// Another publisher changed the pointer after this generation was built.
    PublicationConflict {
        attempted_generation: String,
        current_generation: Option<String>,
    },
    ManifestAlreadyExists(String),
    MissingBlob {
        plane: ArtifactPlane,
        key: String,
    },
    MissingTrigram,
    MissingAlias(String),
    /// A manifest or completion was produced by a different producer (model,
    /// extractor or trigram policy) than the one the reader requires.
    ProducerMismatch(String),
    /// A generation's name does not match the content fingerprint of its
    /// manifest.
    GenerationMismatch(String),
}

impl fmt::Display for ViewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "view I/O failed: {error}"),
            Self::IoAt {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "view I/O failed {operation} {}: {source}",
                path.display()
            ),
            Self::Sqlite(error) => write!(formatter, "view SQLite operation failed: {error}"),
            Self::Json(error) => write!(formatter, "view manifest JSON failed: {error}"),
            Self::InvalidManifest(message) => write!(formatter, "invalid view manifest: {message}"),
            Self::PublicationConflict { attempted_generation, current_generation } => write!(
                formatter, "view publication conflict: attempted {attempted_generation}, current {current_generation:?}"
            ),
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
            Self::ProducerMismatch(message) => {
                write!(formatter, "view producer mismatch: {message}")
            }
            Self::GenerationMismatch(message) => {
                write!(formatter, "view generation mismatch: {message}")
            }
        }
    }
}

impl std::error::Error for ViewError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::IoAt { source: error, .. } => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl ViewError {
    pub(crate) fn io_at(operation: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::IoAt {
            operation,
            path: path.to_path_buf(),
            source,
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
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
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
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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
#[derive(Clone, Debug)]
pub struct Manifest {
    pub path_identity_version: u8,
    entries: std::sync::Arc<BTreeMap<RelPath, ManifestEntry>>,
    json: std::sync::Arc<std::sync::OnceLock<(u8, std::sync::Arc<[u8]>)>>,
}

impl PartialEq for Manifest {
    fn eq(&self, other: &Self) -> bool {
        self.path_identity_version == other.path_identity_version && self.entries == other.entries
    }
}

impl Eq for Manifest {}

impl Manifest {
    pub fn new(entries: impl IntoIterator<Item = (RelPath, ManifestEntry)>) -> Result<Self> {
        let mut manifest = Self {
            path_identity_version: PATH_IDENTITY_VERSION,
            entries: Default::default(),
            json: Default::default(),
        };
        for (rel_path, entry) in entries {
            manifest.insert(rel_path, entry)?;
        }
        Ok(manifest)
    }

    pub fn insert(&mut self, rel_path: RelPath, entry: ManifestEntry) -> Result<()> {
        validate_member(&rel_path, &entry)?;
        self.json = Default::default();
        if std::sync::Arc::make_mut(&mut self.entries)
            .insert(rel_path, entry)
            .is_some()
        {
            return Err(ViewError::InvalidManifest(
                "a manifest cannot contain the same rel_path twice".to_string(),
            ));
        }
        Ok(())
    }

    pub fn entries(&self) -> impl Iterator<Item = (&RelPath, &ManifestEntry)> {
        self.entries.iter()
    }

    /// Start at a bytewise lower bound without requiring it to be a valid member
    /// path. Directory prefixes end in `/`, and the root prefix is empty.
    pub(crate) fn entries_from(
        &self,
        lower: &[u8],
    ) -> impl Iterator<Item = (&RelPath, &ManifestEntry)> {
        self.entries
            .range(RelPath(ByteString::new(lower.to_vec()))..)
    }

    pub fn get(&self, rel_path: &RelPath) -> Option<&ManifestEntry> {
        self.entries.get(rel_path)
    }

    pub fn to_json_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.json_bytes()?.to_vec())
    }

    /// Fingerprints and the durable writer share the same canonical encoding.
    pub(super) fn json_bytes(&self) -> Result<std::sync::Arc<[u8]>> {
        if let Some((version, bytes)) = self.json.get() {
            if *version == self.path_identity_version {
                return Ok(bytes.clone());
            }
        }
        let bytes: std::sync::Arc<[u8]> = serde_json::to_vec(self)?.into();
        let _ = self.json.set((self.path_identity_version, bytes.clone()));
        Ok(bytes)
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
        #[cfg(test)]
        MANIFEST_DECODES.with(|count| count.set(count.get() + 1));
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
