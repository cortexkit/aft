//! Proven Git-object aliases and byte-exact manifest path identities.
//!
//! Git object IDs hash a `blob <len>\0` header as well as file bytes, while
//! content-addressed artifacts use BLAKE3 of the file bytes.  This module keeps
//! that distinction explicit: aliases are accepted only after recomputing the
//! Git blob ID from the exact bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha1::Sha1;
use sha2::Digest;

/// The manifest format that stores paths as exact, slash-separated bytes.
pub const PATH_IDENTITY_VERSION: u32 = 1;

const ALIAS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS oid_aliases (
    git_oid BLOB NOT NULL PRIMARY KEY CHECK(length(git_oid) = 20),
    blake3 BLOB NOT NULL CHECK(length(blake3) = 32)
) WITHOUT ROWID;
"#;

const MANIFEST_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS manifest_metadata (
    singleton INTEGER NOT NULL PRIMARY KEY CHECK(singleton = 1),
    path_identity_version INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS manifest_entries (
    rel_path BLOB NOT NULL PRIMARY KEY,
    entry_json TEXT NOT NULL
) WITHOUT ROWID;
"#;

/// Errors raised while proving aliases or preserving path identity.
#[derive(Debug)]
pub enum AliasError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Git {
        command: &'static str,
        stderr: String,
    },
    InvalidArtifactKey(String),
    InvalidGitOid(String),
    InvalidRelativePath(String),
    InvalidManifestEntry(String),
    DuplicateManifestPath(Vec<u8>),
    CorruptAliasDigest,
    ConflictingAlias(GitOid),
}

impl fmt::Display for AliasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "alias/path-identity I/O error: {error}"),
            Self::Sqlite(error) => write!(f, "alias/path-identity SQLite error: {error}"),
            Self::Git { command, stderr } => write!(f, "git {command} failed: {stderr}"),
            Self::InvalidArtifactKey(key) => write!(f, "invalid artifact key `{key}`"),
            Self::InvalidGitOid(oid) => write!(f, "invalid SHA-1 Git object ID `{oid}`"),
            Self::InvalidRelativePath(reason) => {
                write!(f, "invalid manifest relative path: {reason}")
            }
            Self::InvalidManifestEntry(reason) => write!(f, "invalid manifest entry: {reason}"),
            Self::DuplicateManifestPath(path) => {
                write!(f, "duplicate manifest path `{}`", path_display(path))
            }
            Self::CorruptAliasDigest => {
                f.write_str("stored alias has an invalid BLAKE3 digest length")
            }
            Self::ConflictingAlias(oid) => write!(
                f,
                "Git object ID {oid} is already aliased to different BLAKE3 bytes"
            ),
        }
    }
}

impl std::error::Error for AliasError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::Git { .. }
            | Self::InvalidArtifactKey(_)
            | Self::InvalidGitOid(_)
            | Self::InvalidRelativePath(_)
            | Self::InvalidManifestEntry(_)
            | Self::DuplicateManifestPath(_)
            | Self::CorruptAliasDigest
            | Self::ConflictingAlias(_) => None,
        }
    }
}

impl From<std::io::Error> for AliasError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for AliasError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// A SHA-1 Git object ID represented as its 20 raw bytes, not text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GitOid([u8; 20]);

impl GitOid {
    /// Parses Git's lower- or upper-case, 40-character SHA-1 object ID form.
    pub fn from_hex(value: &str) -> Result<Self, AliasError> {
        if value.len() != 40 || !value.as_bytes().iter().all(u8::is_ascii_hexdigit) {
            return Err(AliasError::InvalidGitOid(value.to_owned()));
        }

        let mut bytes = [0_u8; 20];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high =
                hex_nibble(pair[0]).ok_or_else(|| AliasError::InvalidGitOid(value.to_owned()))?;
            let low =
                hex_nibble(pair[1]).ok_or_else(|| AliasError::InvalidGitOid(value.to_owned()))?;
            bytes[index] = high << 4 | low;
        }
        Ok(Self(bytes))
    }

    /// Creates an object ID from the raw bytes used by the alias SQLite key.
    pub fn from_bytes(value: &[u8]) -> Result<Self, AliasError> {
        value
            .try_into()
            .map(Self)
            .map_err(|_| AliasError::InvalidGitOid(hex(value)))
    }

    /// Raw bytes suitable for a SQLite BLOB key.
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Lower-case hexadecimal form used by Git command output and JSON manifests.
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Git tree modes that determine whether a path can be aliased.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitMode {
    Regular { executable: bool },
    Symlink,
    Gitlink,
    Other(Vec<u8>),
}

impl GitMode {
    /// Parses the mode field emitted by `git ls-tree` without converting paths or
    /// object IDs through a lossy string representation.
    pub fn from_git_mode(value: &[u8]) -> Self {
        match value {
            b"100644" => Self::Regular { executable: false },
            b"100755" => Self::Regular { executable: true },
            b"120000" => Self::Symlink,
            b"160000" => Self::Gitlink,
            _ => Self::Other(value.to_vec()),
        }
    }

    /// Returns true only for Git's regular-file modes: `100644` and `100755`.
    pub const fn is_regular(&self) -> bool {
        matches!(self, Self::Regular { .. })
    }

    /// The canonical Git tree mode bytes.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Regular { executable: false } => b"100644",
            Self::Regular { executable: true } => b"100755",
            Self::Symlink => b"120000",
            Self::Gitlink => b"160000",
            Self::Other(value) => value,
        }
    }
}

/// A Git-tracked path with the metadata used to decide whether it can be aliased.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackedPath {
    /// Exact Git path bytes. They always use `/` separators.
    pub rel_path: Vec<u8>,
    pub mode: GitMode,
    pub git_oid: GitOid,
    /// A path with a Git clean/smudge filter, including Git LFS, is never aliased.
    pub filtered: bool,
    /// Only paths present in the previous generation belong in the zero-read report denominator.
    pub present_in_previous_generation: bool,
    /// `false` is accepted for report inputs so untracked files cannot accidentally
    /// become eligible when callers combine Git and watcher path lists.
    pub tracked: bool,
}

impl TrackedPath {
    pub fn new(rel_path: Vec<u8>, mode: GitMode, git_oid: GitOid) -> Result<Self, AliasError> {
        validate_rel_path(&rel_path)?;
        Ok(Self {
            rel_path,
            mode,
            git_oid,
            filtered: false,
            present_in_previous_generation: false,
            tracked: true,
        })
    }

    pub fn with_filter_status(mut self, filtered: bool) -> Self {
        self.filtered = filtered;
        self
    }

    pub fn with_previous_generation(mut self, present: bool) -> Self {
        self.present_in_previous_generation = present;
        self
    }

    /// Returns true when the mode and filter policy permit a Git-to-BLAKE3 alias.
    pub fn is_alias_eligible(&self) -> bool {
        self.tracked && self.mode.is_regular() && !self.filtered
    }
}

/// Why a candidate was deliberately not entered into the alias table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AliasSkip {
    Untracked,
    Symlink,
    Gitlink,
    NonRegular,
    Filtered,
    LfsPointer,
    GitOidMismatch,
}

/// The immutable result of attempting to seed one alias.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AliasWrite {
    Inserted([u8; 32]),
    Reused([u8; 32]),
    Skipped(AliasSkip),
}

/// SQLite storage for proven `(git_oid -> blake3(bytes))` aliases.
pub struct AliasStore {
    path: PathBuf,
    connection: Connection,
}

impl AliasStore {
    /// Opens `<storage>/blobs/<artifact_key>/oid-alias.sqlite`.
    pub fn open(storage: &Path, artifact_key: &str) -> Result<Self, AliasError> {
        validate_artifact_key(artifact_key)?;
        let path = storage
            .join("blobs")
            .join(artifact_key)
            .join("oid-alias.sqlite");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_millis(5_000))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=OFF;",
        )?;
        connection.execute_batch(ALIAS_SCHEMA)?;
        Ok(Self { path, connection })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolves a previously proven alias without reading the checkout file.
    pub fn resolve(&self, git_oid: GitOid) -> Result<Option<[u8; 32]>, AliasError> {
        let digest = self
            .connection
            .query_row(
                "SELECT blake3 FROM oid_aliases WHERE git_oid = ?1",
                params![git_oid.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        digest
            .map(|digest| {
                digest
                    .try_into()
                    .map_err(|_| AliasError::CorruptAliasDigest)
            })
            .transpose()
    }

    /// Writes an alias only after independently proving the Git blob hash.
    ///
    /// The working-tree caller supplies bytes it already read for indexing. This
    /// method never substitutes a text conversion for those bytes, so Git's
    /// header length and BLAKE3 digest describe the same byte sequence.
    pub fn seed_proven_alias(
        &mut self,
        path: &TrackedPath,
        bytes: &[u8],
    ) -> Result<AliasWrite, AliasError> {
        validate_rel_path(&path.rel_path)?;
        let eligibility = if !path.tracked {
            Some(AliasSkip::Untracked)
        } else if path.filtered {
            Some(AliasSkip::Filtered)
        } else {
            match path.mode {
                GitMode::Regular { .. } => None,
                GitMode::Symlink => Some(AliasSkip::Symlink),
                GitMode::Gitlink => Some(AliasSkip::Gitlink),
                GitMode::Other(_) => Some(AliasSkip::NonRegular),
            }
        };
        if let Some(skip) = eligibility {
            return Ok(AliasWrite::Skipped(skip));
        }
        if is_lfs_pointer(bytes) {
            return Ok(AliasWrite::Skipped(AliasSkip::LfsPointer));
        }
        if git_blob_oid(bytes) != path.git_oid {
            return Ok(AliasWrite::Skipped(AliasSkip::GitOidMismatch));
        }

        let digest = *blake3::hash(bytes).as_bytes();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = tx
            .query_row(
                "SELECT blake3 FROM oid_aliases WHERE git_oid = ?1",
                params![path.git_oid.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        let outcome = match existing {
            Some(existing) if existing.as_slice() == digest => AliasWrite::Reused(digest),
            Some(_) => return Err(AliasError::ConflictingAlias(path.git_oid)),
            None => {
                tx.execute(
                    "INSERT INTO oid_aliases (git_oid, blake3) VALUES (?1, ?2)",
                    params![path.git_oid.as_bytes().as_slice(), digest.as_slice()],
                )?;
                AliasWrite::Inserted(digest)
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    pub fn alias_count(&self) -> Result<usize, AliasError> {
        self.connection
            .query_row("SELECT COUNT(*) FROM oid_aliases", [], |row| row.get(0))
            .map_err(Into::into)
    }

    /// Measures zero-read checkout resolution. Only tracked regular, unfiltered
    /// paths present in the previous generation are eligible for the denominator.
    pub fn zero_read_checkout_report(
        &self,
        paths: impl IntoIterator<Item = impl std::borrow::Borrow<TrackedPath>>,
    ) -> Result<ZeroReadCheckoutReport, AliasError> {
        let mut report = ZeroReadCheckoutReport::default();
        for path in paths {
            let path = path.borrow();
            let exclusion = if !path.tracked {
                Some(ExcludedPathClass::Untracked)
            } else if path.filtered {
                Some(ExcludedPathClass::Filtered)
            } else {
                match path.mode {
                    GitMode::Regular { .. } if path.present_in_previous_generation => None,
                    GitMode::Regular { .. } => Some(ExcludedPathClass::NotPreviouslyIndexed),
                    GitMode::Symlink => Some(ExcludedPathClass::Symlink),
                    GitMode::Gitlink => Some(ExcludedPathClass::Gitlink),
                    GitMode::Other(_) => Some(ExcludedPathClass::NonRegular),
                }
            };
            if let Some(exclusion) = exclusion {
                report.excluded.record(exclusion);
                continue;
            }

            report.denominator += 1;
            if self.resolve(path.git_oid)?.is_some() {
                report.numerator += 1;
            }
        }
        Ok(report)
    }

    /// Reads only Git metadata for `HEAD`, then applies the zero-read report to
    /// paths named by a previous manifest. File contents are never opened here.
    pub fn report_head_checkout(
        &self,
        repo_root: &Path,
        previous_manifest_paths: &BTreeSet<Vec<u8>>,
    ) -> Result<ZeroReadCheckoutReport, AliasError> {
        let mut paths = head_tree_entries(repo_root)?;
        for path in &mut paths {
            path.present_in_previous_generation = previous_manifest_paths.contains(&path.rel_path);
        }
        self.zero_read_checkout_report(&paths)
    }
}

/// Excluded path totals printed with a zero-read checkout report.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExcludedPathClasses {
    pub untracked: usize,
    pub symlink: usize,
    pub gitlink: usize,
    pub filtered: usize,
    pub non_regular: usize,
    pub not_previously_indexed: usize,
}

impl ExcludedPathClasses {
    fn record(&mut self, class: ExcludedPathClass) {
        match class {
            ExcludedPathClass::Untracked => self.untracked += 1,
            ExcludedPathClass::Symlink => self.symlink += 1,
            ExcludedPathClass::Gitlink => self.gitlink += 1,
            ExcludedPathClass::Filtered => self.filtered += 1,
            ExcludedPathClass::NonRegular => self.non_regular += 1,
            ExcludedPathClass::NotPreviouslyIndexed => self.not_previously_indexed += 1,
        }
    }
}

#[derive(Clone, Copy)]
enum ExcludedPathClass {
    Untracked,
    Symlink,
    Gitlink,
    Filtered,
    NonRegular,
    NotPreviouslyIndexed,
}

/// The numerator, denominator, and every excluded path class for zero-read checkout reporting.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ZeroReadCheckoutReport {
    pub numerator: usize,
    pub denominator: usize,
    pub excluded: ExcludedPathClasses,
}

impl ZeroReadCheckoutReport {
    /// Uses integer arithmetic so the 95% acceptance boundary is deterministic.
    pub fn meets_95_percent(&self) -> bool {
        self.denominator != 0 && (self.numerator as u128) * 100 >= (self.denominator as u128) * 95
    }
}

impl fmt::Display for ZeroReadCheckoutReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "zero-read checkout: numerator={} denominator={} excluded={{untracked={}, symlink={}, gitlink={}, filtered={}, non_regular={}, not_previously_indexed={}}}",
            self.numerator,
            self.denominator,
            self.excluded.untracked,
            self.excluded.symlink,
            self.excluded.gitlink,
            self.excluded.filtered,
            self.excluded.non_regular,
            self.excluded.not_previously_indexed,
        )
    }
}

/// A manifest's two optional plane keys for a regular path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlaneKeys {
    pub semantic: Option<[u8; 32]>,
    pub callgraph: Option<[u8; 32]>,
}

/// The tagged manifest entry schema. Paths live beside entries as raw BLOBs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestEntry {
    Regular {
        mode: GitMode,
        planes: PlaneKeys,
        resolution_input: bool,
    },
    Symlink {
        target_bytes: Vec<u8>,
    },
    Gitlink {
        oid: GitOid,
    },
    Synthetic {
        name: String,
        callgraph: [u8; 32],
    },
}

impl ManifestEntry {
    pub fn regular(mode: GitMode, planes: PlaneKeys, resolution_input: bool) -> Self {
        Self::Regular {
            mode,
            planes,
            resolution_input,
        }
    }
}

/// One byte-exact manifest path and its tagged entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestRecord {
    pub rel_path: Vec<u8>,
    pub entry: ManifestEntry,
}

/// A single view manifest with a fixed path identity encoding version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Manifest {
    pub path_identity_version: u32,
    pub entries: Vec<ManifestRecord>,
}

impl Manifest {
    /// Validates entries and orders them by raw path bytes, not platform strings.
    pub fn new(mut entries: Vec<ManifestRecord>) -> Result<Self, AliasError> {
        for record in &entries {
            validate_manifest_record(record)?;
        }
        entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
        for pair in entries.windows(2) {
            if pair[0].rel_path == pair[1].rel_path {
                return Err(AliasError::DuplicateManifestPath(pair[0].rel_path.clone()));
            }
        }
        Ok(Self {
            path_identity_version: PATH_IDENTITY_VERSION,
            entries,
        })
    }

    /// Encodes the canonical JSON manifest. UTF-8 paths are strings; non-UTF-8
    /// paths are objects of the exact required shape: `{"b64":"..."}`.
    pub fn to_json_value(&self) -> serde_json::Value {
        let entries = self
            .entries
            .iter()
            .map(|record| {
                let mut object = entry_json_object(&record.entry);
                object.insert("rel_path".to_string(), json_bytes(&record.rel_path));
                serde_json::Value::Object(object)
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "path_identity_version": self.path_identity_version,
            "entries": entries,
        })
    }

    pub fn to_json_string(&self) -> String {
        self.to_json_value().to_string()
    }
}

/// SQLite representation for raw manifest paths. `rel_path` is deliberately a
/// BLOB column so SQLite never applies text collation or Unicode normalization.
pub struct ManifestSqliteStore {
    connection: Connection,
}

impl ManifestSqliteStore {
    pub fn open(path: &Path) -> Result<Self, AliasError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.execute_batch(MANIFEST_SCHEMA)?;
        Ok(Self { connection })
    }

    /// Replaces the store's one manifest snapshot atomically.
    pub fn write(&mut self, manifest: &Manifest) -> Result<(), AliasError> {
        if manifest.path_identity_version != PATH_IDENTITY_VERSION {
            return Err(AliasError::InvalidManifestEntry(format!(
                "path_identity_version must be {PATH_IDENTITY_VERSION}"
            )));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM manifest_entries", [])?;
        tx.execute(
            "INSERT INTO manifest_metadata (singleton, path_identity_version) VALUES (1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET path_identity_version = excluded.path_identity_version",
            params![i64::from(manifest.path_identity_version)],
        )?;
        for record in &manifest.entries {
            let entry_json =
                serde_json::Value::Object(entry_json_object(&record.entry)).to_string();
            tx.execute(
                "INSERT INTO manifest_entries (rel_path, entry_json) VALUES (?1, ?2)",
                params![record.rel_path, entry_json],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn path_identity_version(&self) -> Result<Option<u32>, AliasError> {
        self.connection
            .query_row(
                "SELECT path_identity_version FROM manifest_metadata WHERE singleton = 1",
                [],
                |row| row.get::<_, u32>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Returns raw BLOB paths in SQLite's bytewise primary-key order.
    pub fn paths(&self) -> Result<Vec<Vec<u8>>, AliasError> {
        let mut statement = self
            .connection
            .prepare("SELECT rel_path FROM manifest_entries ORDER BY rel_path")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

/// Computes Git's SHA-1 blob object ID from exact file bytes.
pub fn git_blob_oid(bytes: &[u8]) -> GitOid {
    let mut hasher = Sha1::new();
    hasher.update(b"blob ");
    hasher.update(bytes.len().to_string().as_bytes());
    hasher.update([0]);
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut oid = [0_u8; 20];
    oid.copy_from_slice(&digest);
    GitOid(oid)
}

/// Returns whether `bytes` are an LFS pointer. This conservative secondary
/// check protects aliases even if a caller failed to propagate `filter=lfs`.
pub fn is_lfs_pointer(bytes: &[u8]) -> bool {
    bytes.starts_with(b"version https://git-lfs.github.com/spec/v1\n")
}

/// Lists `HEAD` paths from Git metadata without opening working-tree files.
pub fn head_tree_entries(repo_root: &Path) -> Result<Vec<TrackedPath>, AliasError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["ls-tree", "-r", "-z", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Err(AliasError::Git {
            command: "ls-tree -r -z HEAD",
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    let mut paths = parse_ls_tree_output(&output.stdout)?;
    let filters = git_filter_attributes(repo_root, &paths)?;
    for path in &mut paths {
        path.filtered = filters.get(&path.rel_path).copied().unwrap_or(false);
    }
    Ok(paths)
}

fn parse_ls_tree_output(output: &[u8]) -> Result<Vec<TrackedPath>, AliasError> {
    output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let separator = record
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(|| {
                    AliasError::InvalidManifestEntry("malformed git ls-tree record".to_owned())
                })?;
            let (header, rel_path) = (&record[..separator], &record[separator + 1..]);
            let mut fields = header.split(|byte| *byte == b' ');
            let mode = fields.next().ok_or_else(|| {
                AliasError::InvalidManifestEntry("missing git tree mode".to_owned())
            })?;
            let _object_type = fields.next().ok_or_else(|| {
                AliasError::InvalidManifestEntry("missing git tree object type".to_owned())
            })?;
            let oid = fields.next().ok_or_else(|| {
                AliasError::InvalidManifestEntry("missing git tree object ID".to_owned())
            })?;
            if fields.next().is_some() {
                return Err(AliasError::InvalidManifestEntry(
                    "malformed git tree header".to_owned(),
                ));
            }
            let oid = std::str::from_utf8(oid)
                .map_err(|_| AliasError::InvalidGitOid(hex(oid)))
                .and_then(GitOid::from_hex)?;
            TrackedPath::new(rel_path.to_vec(), GitMode::from_git_mode(mode), oid)
        })
        .collect()
}

fn git_filter_attributes(
    repo_root: &Path,
    paths: &[TrackedPath],
) -> Result<BTreeMap<Vec<u8>, bool>, AliasError> {
    if paths.is_empty() {
        return Ok(BTreeMap::new());
    }

    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["check-attr", "--cached", "-z", "--stdin", "filter"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let stdin = child.stdin.as_mut().ok_or_else(|| {
            AliasError::InvalidManifestEntry("missing git check-attr stdin pipe".to_owned())
        })?;
        for path in paths {
            stdin.write_all(&path.rel_path)?;
            stdin.write_all(&[0])?;
        }
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(AliasError::Git {
            command: "check-attr --cached -z --stdin filter",
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    let fields = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() % 3 != 0 {
        return Err(AliasError::InvalidManifestEntry(
            "malformed git check-attr output".to_owned(),
        ));
    }

    let mut filtered = BTreeMap::new();
    for fields in fields.chunks_exact(3) {
        let path = fields[0].to_vec();
        let attribute = fields[1];
        let value = fields[2];
        if attribute != b"filter" {
            return Err(AliasError::InvalidManifestEntry(
                "unexpected git check-attr attribute".to_owned(),
            ));
        }
        filtered.insert(
            path,
            !matches!(value, b"unspecified" | b"unset" | b"set" | b""),
        );
    }
    Ok(filtered)
}

fn validate_artifact_key(artifact_key: &str) -> Result<(), AliasError> {
    if artifact_key.is_empty()
        || artifact_key == "."
        || artifact_key == ".."
        || artifact_key.contains(['/', '\\', '\0'])
    {
        return Err(AliasError::InvalidArtifactKey(artifact_key.to_owned()));
    }
    Ok(())
}

fn validate_manifest_record(record: &ManifestRecord) -> Result<(), AliasError> {
    match &record.entry {
        ManifestEntry::Synthetic { name, .. } => {
            if name.is_empty() || name.as_bytes().contains(&0) {
                return Err(AliasError::InvalidManifestEntry(
                    "synthetic entry name must be non-empty and NUL-free".to_owned(),
                ));
            }
            let expected = [vec![0], name.as_bytes().to_vec()].concat();
            if record.rel_path != expected {
                return Err(AliasError::InvalidManifestEntry(
                    "synthetic entry path must be a leading NUL followed by its name".to_owned(),
                ));
            }
        }
        ManifestEntry::Regular { mode, .. } => {
            validate_rel_path(&record.rel_path)?;
            if !mode.is_regular() {
                return Err(AliasError::InvalidManifestEntry(
                    "regular manifest entry must use mode 100644 or 100755".to_owned(),
                ));
            }
        }
        ManifestEntry::Symlink { .. } | ManifestEntry::Gitlink { .. } => {
            validate_rel_path(&record.rel_path)?;
        }
    }
    Ok(())
}

fn validate_rel_path(path: &[u8]) -> Result<(), AliasError> {
    if path.is_empty() {
        return Err(AliasError::InvalidRelativePath("path is empty".to_owned()));
    }
    if path[0] == b'/' {
        return Err(AliasError::InvalidRelativePath(
            "path is absolute".to_owned(),
        ));
    }
    if path.contains(&b'\\') {
        return Err(AliasError::InvalidRelativePath(
            "path must use `/` separators".to_owned(),
        ));
    }
    if path.contains(&0) {
        return Err(AliasError::InvalidRelativePath(
            "filesystem path contains NUL".to_owned(),
        ));
    }
    if path.len() >= 3 && path[0].is_ascii_alphabetic() && path[1] == b':' && path[2] == b'/' {
        return Err(AliasError::InvalidRelativePath(
            "path is drive-absolute".to_owned(),
        ));
    }
    if path
        .split(|byte| *byte == b'/')
        .any(|component| component.is_empty() || matches!(component, b"." | b".."))
    {
        return Err(AliasError::InvalidRelativePath(
            "path has an empty, `.` or `..` component".to_owned(),
        ));
    }
    Ok(())
}

fn entry_json_object(entry: &ManifestEntry) -> serde_json::Map<String, serde_json::Value> {
    let mut object = serde_json::Map::new();
    match entry {
        ManifestEntry::Regular {
            mode,
            planes,
            resolution_input,
        } => {
            object.insert(
                "kind".to_owned(),
                serde_json::Value::String("regular".to_owned()),
            );
            object.insert(
                "mode".to_owned(),
                serde_json::Value::String(String::from_utf8_lossy(mode.as_bytes()).into_owned()),
            );
            object.insert(
                "planes".to_owned(),
                serde_json::json!({
                    "semantic": planes.semantic.map(|key| hex(&key)),
                    "callgraph": planes.callgraph.map(|key| hex(&key)),
                }),
            );
            object.insert(
                "resolution_input".to_owned(),
                serde_json::Value::Bool(*resolution_input),
            );
        }
        ManifestEntry::Symlink { target_bytes } => {
            object.insert(
                "kind".to_owned(),
                serde_json::Value::String("symlink".to_owned()),
            );
            object.insert("target_bytes".to_owned(), json_bytes(target_bytes));
        }
        ManifestEntry::Gitlink { oid } => {
            object.insert(
                "kind".to_owned(),
                serde_json::Value::String("gitlink".to_owned()),
            );
            object.insert("oid".to_owned(), serde_json::Value::String(oid.to_hex()));
        }
        ManifestEntry::Synthetic { name, callgraph } => {
            object.insert(
                "kind".to_owned(),
                serde_json::Value::String("synthetic".to_owned()),
            );
            object.insert("name".to_owned(), serde_json::Value::String(name.clone()));
            object.insert(
                "planes".to_owned(),
                serde_json::json!({ "callgraph": hex(callgraph) }),
            );
        }
    }
    object
}

fn json_bytes(bytes: &[u8]) -> serde_json::Value {
    match std::str::from_utf8(bytes) {
        Ok(value) => serde_json::Value::String(value.to_owned()),
        Err(_) => serde_json::json!({ "b64": BASE64.encode(bytes) }),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use fmt::Write as _;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn path_display(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}
