//! Manifest v2: one checkout generation, with per-entry content identity and
//! per-plane readiness.
//!
//! Differences from the v1 manifest in `views/mod.rs`:
//!
//! - membership is the walker's (tracked and untracked files that are not
//!   ignored), not the HEAD tree;
//! - every regular entry records `content` (BLAKE3 of the bytes), `size`, an
//!   optional `git_oid`, and a state per plane: `Ready(key)`, `Pending(reason)`
//!   or `Failed(reason, producer)`. Every plane key of one entry comes from the
//!   one buffer whose hash is `content`;
//! - the header names the producers (trigram policy, semantic model/chunker,
//!   callgraph extractor). A reader that requires other producers rejects the
//!   manifest instead of serving keys it cannot interpret;
//! - a generation is identified by the fingerprint of its manifest content.
//!   The published name adds a builder token (pid, process start, sequence),
//!   so two builders of equal manifests always write private files, and the
//!   loser of the pointer race can recognize an equivalent winner.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

use crate::blob_store::v2::{to_hex, ContentHash, FamilyKey, FamilyPlane};

use super::contracts::{observe, DurabilityObserver, DurabilityStep};
use super::readiness::PlaneState;
use super::{
    sync_directory, sync_file, validate_generation, ByteString, RelPath, Result, ViewError,
    ViewStore, PATH_IDENTITY_VERSION,
};

pub const MANIFEST_FORMAT_VERSION: u8 = 2;
const GENERATION_PREFIX: &str = "g2";

/// The producer of every plane key a manifest may contain.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Producers {
    /// `TrigramPolicy::fingerprint_hex()`.
    pub trigram: String,
    /// The semantic producer (model, chunker and template), or `None` when
    /// semantic indexing is not configured.
    pub semantic: Option<String>,
    /// The callgraph extractor version.
    pub callgraph: String,
}

impl Producers {
    pub fn for_plane(&self, plane: FamilyPlane) -> Option<&str> {
        match plane {
            FamilyPlane::Trigram => Some(&self.trigram),
            FamilyPlane::Semantic => self.semantic.as_deref(),
            FamilyPlane::Callgraph => Some(&self.callgraph),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManifestHeader {
    pub producers: Producers,
    /// HEAD tree fingerprint at build time; used for seed selection only.
    #[serde(default)]
    pub head_tree: Option<String>,
    /// Fingerprint of the ignore files that decided membership.
    #[serde(default)]
    pub ignore_fingerprint: Option<String>,
    /// Hex id of the trigram segment this generation's overlay is relative to.
    #[serde(default)]
    pub segment: Option<String>,
}

/// Per-plane states of one regular entry. `None` means the plane does not
/// apply to the file (for example, no semantic chunks for an image).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EntryPlanes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigram: Option<PlaneState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<PlaneState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callgraph: Option<PlaneState>,
}

impl EntryPlanes {
    pub fn get(&self, plane: FamilyPlane) -> Option<&PlaneState> {
        match plane {
            FamilyPlane::Trigram => self.trigram.as_ref(),
            FamilyPlane::Semantic => self.semantic.as_ref(),
            FamilyPlane::Callgraph => self.callgraph.as_ref(),
        }
    }

    pub fn get_mut(&mut self, plane: FamilyPlane) -> &mut Option<PlaneState> {
        match plane {
            FamilyPlane::Trigram => &mut self.trigram,
            FamilyPlane::Semantic => &mut self.semantic,
            FamilyPlane::Callgraph => &mut self.callgraph,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryV2 {
    Regular {
        mode: u32,
        content: ContentHash,
        size: u64,
        /// Set only when the bytes are proven equal to the HEAD blob.
        #[serde(default)]
        git_oid: Option<String>,
        resolution_input: bool,
        planes: EntryPlanes,
    },
    Symlink {
        target_bytes: ByteString,
    },
    Gitlink {
        oid: String,
    },
    Synthetic {
        name: String,
        callgraph: PlaneState,
    },
}

impl EntryV2 {
    /// A regular file entry with every applicable plane pending.
    pub fn regular(content: ContentHash, size: u64, planes: EntryPlanes) -> Self {
        Self::Regular {
            mode: 0o100644,
            content,
            size,
            git_oid: None,
            resolution_input: false,
            planes,
        }
    }

    pub fn content(&self) -> Option<ContentHash> {
        match self {
            Self::Regular { content, .. } => Some(*content),
            _ => None,
        }
    }

    pub fn planes(&self) -> Option<&EntryPlanes> {
        match self {
            Self::Regular { planes, .. } => Some(planes),
            _ => None,
        }
    }

    pub fn plane_state(&self, plane: FamilyPlane) -> Option<&PlaneState> {
        match self {
            Self::Regular { planes, .. } => planes.get(plane),
            Self::Synthetic { callgraph, .. } if plane == FamilyPlane::Callgraph => Some(callgraph),
            _ => None,
        }
    }
}

/// The one manifest that defines a v2 generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestV2 {
    header: ManifestHeader,
    entries: BTreeMap<RelPath, EntryV2>,
}

impl ManifestV2 {
    pub fn new(header: ManifestHeader) -> Self {
        Self {
            header,
            entries: BTreeMap::new(),
        }
    }

    pub fn header(&self) -> &ManifestHeader {
        &self.header
    }

    pub fn header_mut(&mut self) -> &mut ManifestHeader {
        &mut self.header
    }

    pub fn insert(&mut self, rel_path: RelPath, entry: EntryV2) -> Result<()> {
        validate_entry(&rel_path, &entry)?;
        if self.entries.insert(rel_path, entry).is_some() {
            return Err(ViewError::InvalidManifest(
                "a manifest cannot contain the same rel_path twice".to_string(),
            ));
        }
        Ok(())
    }

    /// Inserts or replaces the entry for `rel_path`; `None` removes it.
    pub fn set(&mut self, rel_path: RelPath, entry: Option<EntryV2>) -> Result<()> {
        match entry {
            Some(entry) => {
                validate_entry(&rel_path, &entry)?;
                self.entries.insert(rel_path, entry);
            }
            None => {
                self.entries.remove(&rel_path);
            }
        }
        Ok(())
    }

    pub fn get(&self, rel_path: &RelPath) -> Option<&EntryV2> {
        self.entries.get(rel_path)
    }

    pub fn get_mut(&mut self, rel_path: &RelPath) -> Option<&mut EntryV2> {
        self.entries.get_mut(rel_path)
    }

    pub fn entries(&self) -> impl Iterator<Item = (&RelPath, &EntryV2)> {
        self.entries.iter()
    }

    pub fn entries_mut(&mut self) -> impl Iterator<Item = (&RelPath, &mut EntryV2)> {
        self.entries.iter_mut()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Paths whose entries differ between two manifests, in either direction.
    pub fn diff_paths(&self, other: &Self) -> BTreeSet<RelPath> {
        let mut changed = BTreeSet::new();
        for (path, entry) in &self.entries {
            if other.entries.get(path) != Some(entry) {
                changed.insert(path.clone());
            }
        }
        for path in other.entries.keys() {
            if !self.entries.contains_key(path) {
                changed.insert(path.clone());
            }
        }
        changed
    }

    /// Every `Ready` key, with its plane.
    pub fn ready_keys(&self) -> impl Iterator<Item = FamilyKey> + '_ {
        self.entries.values().flat_map(|entry| {
            FamilyPlane::ALL
                .into_iter()
                .filter_map(move |plane| match entry.plane_state(plane) {
                    Some(PlaneState::Ready { key }) => crate::blob_store::v2::parse_hex32(key)
                        .map(|bytes| FamilyKey::new(plane, bytes)),
                    _ => None,
                })
        })
    }

    /// The segment this generation references, if any.
    pub fn segment_id(&self) -> Option<[u8; 32]> {
        self.header
            .segment
            .as_deref()
            .and_then(crate::blob_store::v2::parse_hex32)
    }

    /// Rejects a manifest whose producers differ from `expected`.
    pub fn ensure_producers(&self, expected: &Producers) -> Result<()> {
        if &self.header.producers == expected {
            Ok(())
        } else {
            Err(ViewError::ProducerMismatch(format!(
                "manifest was produced by {:?}, reader requires {:?}",
                self.header.producers, expected
            )))
        }
    }

    pub fn to_json_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }

    /// BLAKE3 over the canonical JSON encoding: equal manifests, equal
    /// fingerprints, whoever built them.
    pub fn content_fingerprint(&self) -> Result<[u8; 32]> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"aft/view-manifest/v2");
        hasher.update(&self.to_json_bytes()?);
        Ok(*hasher.finalize().as_bytes())
    }
}

#[derive(Deserialize, Serialize)]
struct JsonEntry {
    rel_path: RelPath,
    #[serde(flatten)]
    entry: EntryV2,
}

#[derive(Deserialize, Serialize)]
struct JsonManifestV2 {
    format_version: u8,
    path_identity_version: u8,
    #[serde(flatten)]
    header: ManifestHeader,
    entries: Vec<JsonEntry>,
}

impl Serialize for ManifestV2 {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        JsonManifestV2 {
            format_version: MANIFEST_FORMAT_VERSION,
            path_identity_version: PATH_IDENTITY_VERSION,
            header: self.header.clone(),
            entries: self
                .entries
                .iter()
                .map(|(rel_path, entry)| JsonEntry {
                    rel_path: rel_path.clone(),
                    entry: entry.clone(),
                })
                .collect(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ManifestV2 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let json = JsonManifestV2::deserialize(deserializer)?;
        if json.format_version != MANIFEST_FORMAT_VERSION {
            return Err(D::Error::custom(format!(
                "unsupported manifest format_version {}",
                json.format_version
            )));
        }
        if json.path_identity_version != PATH_IDENTITY_VERSION {
            return Err(D::Error::custom(format!(
                "unsupported path_identity_version {}",
                json.path_identity_version
            )));
        }
        let mut manifest = Self::new(json.header);
        for member in json.entries {
            manifest
                .insert(member.rel_path, member.entry)
                .map_err(D::Error::custom)?;
        }
        Ok(manifest)
    }
}

fn validate_entry(rel_path: &RelPath, entry: &EntryV2) -> Result<()> {
    match entry {
        EntryV2::Synthetic { name, .. } => {
            let mut expected = vec![0_u8];
            expected.extend_from_slice(name.as_bytes());
            if rel_path.as_bytes() != expected.as_slice() {
                return Err(ViewError::InvalidManifest(
                    "synthetic entries must use the reserved \\0<name> rel_path key".to_string(),
                ));
            }
        }
        _ if rel_path.is_synthetic() => {
            return Err(ViewError::InvalidManifest(
                "only synthetic entries may use a leading NUL rel_path".to_string(),
            ));
        }
        EntryV2::Regular { mode, .. } if !matches!(*mode, 0o100644 | 0o100755) => {
            return Err(ViewError::InvalidManifest(
                "regular manifest entries must use mode 100644 or 100755".to_string(),
            ));
        }
        _ => {}
    }
    Ok(())
}

/// A v2 generation name: `g2-<content fingerprint>-<pid>-<start>-<seq>`.
///
/// The content part identifies the generation; the builder part makes every
/// builder's files private, even when two builders produce equal manifests.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct GenerationName {
    content: [u8; 32],
    pid: u32,
    start: u64,
    seq: u64,
}

static GENERATION_SEQ: AtomicU64 = AtomicU64::new(0);

impl GenerationName {
    /// A fresh private name for a manifest built by this process.
    pub fn for_manifest(manifest: &ManifestV2) -> Result<Self> {
        let owner = super::registry::current_owner();
        Ok(Self {
            content: manifest.content_fingerprint()?,
            pid: owner.pid,
            start: owner.start_time,
            seq: GENERATION_SEQ.fetch_add(1, Ordering::Relaxed),
        })
    }

    pub fn parse(name: &str) -> Option<Self> {
        let mut parts = name.split('-');
        if parts.next()? != GENERATION_PREFIX {
            return None;
        }
        let content = crate::blob_store::v2::parse_hex32(parts.next()?)?;
        let pid = parts.next()?.parse().ok()?;
        let start = parts.next()?.parse().ok()?;
        let seq = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            content,
            pid,
            start,
            seq,
        })
    }

    pub fn content(&self) -> &[u8; 32] {
        &self.content
    }

    /// True when both names identify the same manifest content.
    pub fn same_content(&self, other: &Self) -> bool {
        self.content == other.content
    }
}

impl std::fmt::Display for GenerationName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{GENERATION_PREFIX}-{}-{}-{}-{}",
            to_hex(&self.content),
            self.pid,
            self.start,
            self.seq
        )
    }
}

/// The outcome of a v2 pointer compare-and-swap.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishV2 {
    Published,
    /// Another builder published a manifest with the same content first. The
    /// caller's private files are unreferenced and its work is already live.
    EquivalentWinner {
        current: String,
    },
    /// Another generation won; re-derive from it.
    Conflict {
        current: Option<String>,
    },
}

/// A manifest that is durable under its private name and waits for its CAS.
#[derive(Debug)]
pub struct PreparedV2 {
    generation: GenerationName,
    base: Option<String>,
}

impl PreparedV2 {
    pub fn generation(&self) -> &GenerationName {
        &self.generation
    }
}

impl ViewStore {
    /// Writes a v2 manifest once under its private generation name and syncs
    /// it and its directory, without touching the pointer.
    pub fn prepare_v2(
        &self,
        generation: &GenerationName,
        base: Option<&str>,
        manifest: &ManifestV2,
        observer: Option<&dyn DurabilityObserver>,
    ) -> Result<PreparedV2> {
        if manifest.content_fingerprint()? != generation.content {
            return Err(ViewError::GenerationMismatch(format!(
                "generation {generation} does not name this manifest's content"
            )));
        }
        let name = generation.to_string();
        let path = self.manifest_path(&name)?;
        write_bytes_once(&path, &manifest.to_json_bytes()?)?;
        observe(observer, DurabilityStep::ManifestWritten);
        sync_directory(self.view_dir())?;
        observe(observer, DurabilityStep::ManifestParentSynced);
        Ok(PreparedV2 {
            generation: generation.clone(),
            base: base.map(str::to_owned),
        })
    }

    /// Swaps the pointer from the prepared base to the prepared generation.
    pub fn commit_v2(
        &self,
        prepared: PreparedV2,
        observer: Option<&dyn DurabilityObserver>,
    ) -> Result<PublishV2> {
        let name = prepared.generation.to_string();
        let outcome =
            self.compare_and_swap_pointer(&name, prepared.base.as_deref().unwrap_or_default())?;
        observe(observer, DurabilityStep::PointerCas);
        match outcome {
            super::PublishOutcome::Published => {
                super::checkpoint_pointer_after_cas(&self.pointer_path())?;
                sync_directory(self.view_dir())?;
                observe(observer, DurabilityStep::PointerSynced);
                Ok(PublishV2::Published)
            }
            super::PublishOutcome::Conflict { current_generation } => {
                let equivalent = current_generation
                    .as_deref()
                    .and_then(GenerationName::parse)
                    .is_some_and(|current| current.same_content(&prepared.generation));
                Ok(match (equivalent, current_generation) {
                    (true, Some(current)) => PublishV2::EquivalentWinner { current },
                    (_, current) => PublishV2::Conflict { current },
                })
            }
        }
    }

    /// Loads a v2 manifest and verifies that its content matches its name.
    pub fn load_manifest_v2(&self, generation: &str) -> Result<ManifestV2> {
        let name = GenerationName::parse(generation).ok_or_else(|| {
            ViewError::GenerationMismatch(format!("`{generation}` is not a v2 generation name"))
        })?;
        let path = self.manifest_path(generation)?;
        let manifest = ManifestV2::from_json_bytes(
            &fs::read(&path).map_err(|error| ViewError::io_at("reading", &path, error))?,
        )?;
        if manifest.content_fingerprint()? != name.content {
            return Err(ViewError::GenerationMismatch(format!(
                "manifest {generation} does not match its content fingerprint"
            )));
        }
        Ok(manifest)
    }
}

fn write_bytes_once(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        ViewError::InvalidManifest("manifest path must have a parent directory".to_string())
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("manifest");
    validate_generation(file_name)?;
    let temporary = parent.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        GENERATION_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = crate::private_storage::options()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| ViewError::io_at("creating", &temporary, error))?;
        file.write_all(bytes)
            .map_err(|error| ViewError::io_at("writing", &temporary, error))?;
        file.write_all(b"\n")
            .map_err(|error| ViewError::io_at("writing", &temporary, error))?;
        file.sync_all()
            .map_err(|error| ViewError::io_at("syncing", &temporary, error))?;
        drop(file);
        fs::hard_link(&temporary, path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                ViewError::ManifestAlreadyExists(file_name.to_owned())
            } else {
                ViewError::io_at("linking manifest to", path, error)
            }
        })?;
        sync_file(path)
    })();
    let _ = fs::remove_file(&temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn producers() -> Producers {
        Producers {
            trigram: "trigram-policy".to_string(),
            semantic: Some("model-a".to_string()),
            callgraph: "callgraph-v1".to_string(),
        }
    }

    fn manifest() -> ManifestV2 {
        let mut manifest = ManifestV2::new(ManifestHeader {
            producers: producers(),
            head_tree: Some("tree".to_string()),
            ignore_fingerprint: None,
            segment: None,
        });
        manifest
            .insert(
                RelPath::new(b"src/a.rs".to_vec()).unwrap(),
                EntryV2::regular(
                    ContentHash::of(b"a"),
                    1,
                    EntryPlanes {
                        trigram: Some(PlaneState::Ready {
                            key: "11".repeat(32),
                        }),
                        semantic: Some(PlaneState::Pending {
                            reason: "queued".to_string(),
                        }),
                        callgraph: Some(PlaneState::Failed {
                            reason: "parse error".to_string(),
                            producer: "callgraph-v1".to_string(),
                        }),
                    },
                ),
            )
            .unwrap();
        manifest
            .insert(
                RelPath::new(b"link".to_vec()).unwrap(),
                EntryV2::Symlink {
                    target_bytes: ByteString::new(b"src".to_vec()),
                },
            )
            .unwrap();
        manifest
    }

    #[test]
    fn manifest_round_trips_with_states_and_rejects_other_producers() {
        let manifest = manifest();
        let decoded = ManifestV2::from_json_bytes(&manifest.to_json_bytes().unwrap()).unwrap();
        assert_eq!(decoded, manifest);
        assert_eq!(
            decoded.content_fingerprint().unwrap(),
            manifest.content_fingerprint().unwrap()
        );
        decoded.ensure_producers(&producers()).unwrap();
        let mut other = producers();
        other.semantic = Some("model-b".to_string());
        assert!(matches!(
            decoded.ensure_producers(&other),
            Err(ViewError::ProducerMismatch(_))
        ));
    }

    #[test]
    fn a_v1_manifest_is_not_read_as_v2() {
        let v1 = br#"{"path_identity_version":1,"entries":[]}"#;
        assert!(ManifestV2::from_json_bytes(v1).is_err());
    }

    #[test]
    fn equal_manifests_get_private_names_with_one_content_identity() {
        let manifest = manifest();
        let a = GenerationName::for_manifest(&manifest).unwrap();
        let b = GenerationName::for_manifest(&manifest).unwrap();
        assert_ne!(a.to_string(), b.to_string());
        assert!(a.same_content(&b));
        assert_eq!(GenerationName::parse(&a.to_string()), Some(a));
    }
}
