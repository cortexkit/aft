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
            // Symbols are not a manifest plane: no producer is recorded here.
            FamilyPlane::Symbol => None,
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

/// A depth-one reference to a fixed-membership base. The generation is
/// diagnostic: readiness successors keep the same identity and membership.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BaseHeader {
    pub scope: String,
    pub membership_digest: String,
    pub identity_digest: String,
    pub bound_generation: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PublishedFrontier {
    pub delta_cut_version: u64,
    pub watcher_epoch: u64,
    pub published_at_ms: u64,
    pub verified_frontier: Option<VerifiedFrontier>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VerifiedFrontier {
    #[serde(rename = "E0")]
    pub e0: u64,
    #[serde(rename = "F")]
    pub f: u64,
    pub delivery_seq: u64,
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
            FamilyPlane::Symbol => None,
        }
    }

    pub fn get_mut(&mut self, plane: FamilyPlane) -> &mut Option<PlaneState> {
        match plane {
            FamilyPlane::Trigram => &mut self.trigram,
            FamilyPlane::Semantic => &mut self.semantic,
            FamilyPlane::Callgraph => &mut self.callgraph,
            // Unreachable while FamilyPlane::ALL excludes Symbol (pinned by
            // `blob_store::v2` test `symbol_is_not_a_manifest_plane`). A write
            // here could not be persisted, so it fails loudly instead.
            FamilyPlane::Symbol => {
                panic!("symbol readiness is not a manifest plane; EntryPlanes has no symbol state")
            }
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
    v3: bool,
    base: Option<BaseHeader>,
    published: Option<PublishedFrontier>,
    tombstones: BTreeSet<RelPath>,
    readiness_overrides: BTreeMap<RelPath, EntryPlanes>,
}

impl ManifestV2 {
    pub fn new(header: ManifestHeader) -> Self {
        Self {
            header,
            entries: BTreeMap::new(),
            v3: false,
            base: None,
            published: None,
            tombstones: BTreeSet::new(),
            readiness_overrides: BTreeMap::new(),
        }
    }

    pub fn new_v3(
        header: ManifestHeader,
        base: Option<BaseHeader>,
        published: PublishedFrontier,
    ) -> Self {
        Self {
            v3: true,
            base,
            published: Some(published),
            ..Self::new(header)
        }
    }

    pub fn base_header(&self) -> Option<&BaseHeader> {
        self.base.as_ref()
    }
    /// Rebinds a v3 worker delta to `header`. The worker publisher calls this
    /// with the header of the base it authenticated, never a caller's string.
    pub fn set_base_header(&mut self, header: BaseHeader) -> Result<()> {
        if !self.v3 || self.base.is_none() {
            return Err(ViewError::InvalidManifest(
                "only a v3 worker delta can be rebound to a base".into(),
            ));
        }
        self.base = Some(header);
        Ok(())
    }
    pub fn published(&self) -> Option<&PublishedFrontier> {
        self.published.as_ref()
    }
    pub fn tombstones(&self) -> &BTreeSet<RelPath> {
        &self.tombstones
    }
    pub fn readiness_overrides(&self) -> &BTreeMap<RelPath, EntryPlanes> {
        &self.readiness_overrides
    }

    pub fn set_readiness_override(&mut self, path: RelPath, patch: EntryPlanes) -> Result<()> {
        if !self.v3
            || self.base.is_none()
            || self.entries.contains_key(&path)
            || self.tombstones.contains(&path)
        {
            return Err(ViewError::InvalidManifest(
                "readiness overrides require an inherited path in a v3 worker".into(),
            ));
        }
        self.readiness_overrides.insert(path, patch);
        Ok(())
    }

    pub(super) fn inherit(&mut self, path: RelPath, patch: Option<EntryPlanes>) {
        self.entries.remove(&path);
        self.tombstones.remove(&path);
        match patch {
            Some(patch) => {
                self.readiness_overrides.insert(path, patch);
            }
            None => {
                self.readiness_overrides.remove(&path);
            }
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
        if self.tombstones.contains(&rel_path) || self.readiness_overrides.contains_key(&rel_path) {
            return Err(ViewError::InvalidManifest(
                "manifest sections must be disjoint".into(),
            ));
        }
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
                self.tombstones.remove(&rel_path);
                self.readiness_overrides.remove(&rel_path);
                self.entries.insert(rel_path, entry);
            }
            None => {
                self.entries.remove(&rel_path);
                self.readiness_overrides.remove(&rel_path);
                if self.base.is_some() {
                    self.tombstones.insert(rel_path);
                }
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

    /// Deduplicated Ready keys in entries and overrides. Deduplication is lazy
    /// so a bounded marking walk can stop without scanning the remaining rows.
    pub fn ready_keys(&self) -> impl Iterator<Item = FamilyKey> + '_ {
        let entries = self.entries.values().flat_map(|entry| {
            FamilyPlane::ALL
                .into_iter()
                .filter_map(move |plane| match entry.plane_state(plane) {
                    Some(PlaneState::Ready { key }) => crate::blob_store::v2::parse_hex32(key)
                        .map(|bytes| FamilyKey::new(plane, bytes)),
                    _ => None,
                })
        });
        let overrides = self.readiness_overrides.values().flat_map(|patch| {
            FamilyPlane::ALL
                .into_iter()
                .filter_map(move |plane| match patch.get(plane) {
                    Some(PlaneState::Ready { key }) => crate::blob_store::v2::parse_hex32(key)
                        .map(|bytes| FamilyKey::new(plane, bytes)),
                    _ => None,
                })
        });
        let mut seen = BTreeSet::new();
        entries
            .chain(overrides)
            .filter(move |key| seen.insert(*key))
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
        Ok(fingerprint_bytes(&self.to_json_bytes()?))
    }
}

pub(super) fn fingerprint_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aft/view-manifest/v2");
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

#[derive(Serialize)]
struct BorrowedEntry<'a> {
    rel_path: &'a RelPath,
    #[serde(flatten)]
    entry: &'a EntryV2,
}

struct BorrowedEntries<'a>(&'a BTreeMap<RelPath, EntryV2>);

impl Serialize for BorrowedEntries<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_seq(
            self.0
                .iter()
                .map(|(rel_path, entry)| BorrowedEntry { rel_path, entry }),
        )
    }
}

#[derive(Serialize)]
struct BorrowedManifest<'a> {
    format_version: u8,
    path_identity_version: u8,
    #[serde(flatten)]
    header: &'a ManifestHeader,
    entries: BorrowedEntries<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<&'a BaseHeader>,
    #[serde(skip_serializing_if = "Option::is_none")]
    published: Option<&'a PublishedFrontier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tombstones: Option<&'a BTreeSet<RelPath>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    readiness_overrides: Option<Vec<OverrideRow<'a>>>,
}

#[derive(Serialize)]
struct OverrideRow<'a> {
    rel_path: &'a RelPath,
    planes: &'a EntryPlanes,
}
#[derive(Deserialize, Serialize)]
struct OwnedOverrideRow {
    rel_path: RelPath,
    planes: EntryPlanes,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base: Option<BaseHeader>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    published: Option<PublishedFrontier>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tombstones: Vec<RelPath>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    readiness_overrides: Vec<OwnedOverrideRow>,
}

impl Serialize for ManifestV2 {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        #[cfg(test)]
        SERIALIZATIONS.with(|count| count.set(count.get() + 1));
        BorrowedManifest {
            format_version: if self.v3 { 3 } else { MANIFEST_FORMAT_VERSION },
            path_identity_version: PATH_IDENTITY_VERSION,
            header: &self.header,
            entries: BorrowedEntries(&self.entries),
            base: self.base.as_ref(),
            published: self.published.as_ref(),
            tombstones: self.v3.then_some(&self.tombstones),
            readiness_overrides: self.v3.then(|| {
                self.readiness_overrides
                    .iter()
                    .map(|(rel_path, planes)| OverrideRow { rel_path, planes })
                    .collect()
            }),
        }
        .serialize(serializer)
    }
}

#[cfg(test)]
thread_local! {
    static SERIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl<'de> Deserialize<'de> for ManifestV2 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let json = JsonManifestV2::deserialize(deserializer)?;
        if !matches!(json.format_version, 2 | 3) {
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
        if json.format_version == 2
            && (json.base.is_some()
                || json.published.is_some()
                || !json.tombstones.is_empty()
                || !json.readiness_overrides.is_empty())
        {
            return Err(D::Error::custom("v2 cannot contain composition sections"));
        }
        let mut manifest = if json.format_version == 3 {
            Self::new_v3(
                json.header,
                json.base,
                json.published
                    .ok_or_else(|| D::Error::custom("v3 requires a published frontier"))?,
            )
        } else {
            Self::new(json.header)
        };
        for member in json.entries {
            manifest
                .insert(member.rel_path, member.entry)
                .map_err(D::Error::custom)?;
        }
        for path in json.tombstones {
            if manifest.base.is_none()
                || manifest.entries.contains_key(&path)
                || !manifest.tombstones.insert(path)
            {
                return Err(D::Error::custom(
                    "tombstones must be unique inherited paths, disjoint from entries",
                ));
            }
        }
        for row in json.readiness_overrides {
            if manifest.readiness_overrides.contains_key(&row.rel_path) {
                return Err(D::Error::custom("duplicate readiness override"));
            }
            manifest
                .set_readiness_override(row.rel_path, row.planes)
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
/// Names are observations, not reader leases; callers inspecting generations
/// after the operation returns must acquire their own protection.
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
    _pin: crate::pins::QueryPin,
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
        let bytes = manifest.to_json_bytes()?;
        if fingerprint_bytes(&bytes) != generation.content {
            return Err(ViewError::GenerationMismatch(format!(
                "generation {generation} does not name this manifest's content"
            )));
        }
        let name = generation.to_string();
        // The generation sweeper also takes an IMMEDIATE pointer transaction.
        // Admit protection under that exclusion before exposing a private manifest.
        let pin = self.pin_prepared_generation(&name)?;
        let path = self.manifest_path(&name)?;
        write_bytes_once(&path, &bytes)?;
        observe(observer, DurabilityStep::ManifestWritten);
        sync_directory(self.view_dir())?;
        observe(observer, DurabilityStep::ManifestParentSynced);
        Ok(PreparedV2 {
            generation: generation.clone(),
            base: base.map(str::to_owned),
            _pin: pin,
        })
    }

    /// Swaps the pointer from the prepared base to the prepared generation.
    pub fn commit_v2(
        &self,
        prepared: PreparedV2,
        observer: Option<&dyn DurabilityObserver>,
    ) -> Result<PublishV2> {
        self.commit_v2_completion(prepared, observer, || Ok(()), false)
    }

    /// Complete publication-time SQLite reads before scheduling retirement.
    /// The callback must close its connections before returning, including on
    /// error; a callback error does not roll back the already durable pointer.
    pub(crate) fn commit_v2_before_sweep(
        &self,
        prepared: PreparedV2,
        observer: Option<&dyn DurabilityObserver>,
        after_publish: impl FnOnce() -> Result<()>,
    ) -> Result<PublishV2> {
        self.commit_v2_completion(prepared, observer, after_publish, true)
    }

    fn commit_v2_completion(
        &self,
        prepared: PreparedV2,
        observer: Option<&dyn DurabilityObserver>,
        after_publish: impl FnOnce() -> Result<()>,
        coordinate_probe: bool,
    ) -> Result<PublishV2> {
        let name = prepared.generation.to_string();
        // Keep both the private attempt and the resolved pointer generation
        // protected through observer callbacks and return, not merely CAS.
        let (outcome, _resolved_pin) = self.compare_and_swap_pointer_with_pin(
            &name,
            prepared.base.as_deref().unwrap_or_default(),
            true,
        )?;
        observe(observer, DurabilityStep::PointerCas);
        match outcome {
            super::PublishOutcome::Published => {
                super::checkpoint_pointer_after_cas(&self.pointer_path())?;
                sync_directory(self.view_dir())?;
                observe(observer, DurabilityStep::PointerSynced);
                let completion = if coordinate_probe {
                    super::generation::with_publication_probe(self.view_dir(), after_publish)
                } else {
                    after_publish()
                };
                super::generation::schedule_generation_sweep(self.clone());
                completion?;
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
        let bytes = fs::read(&path).map_err(|error| ViewError::io_at("reading", &path, error))?;
        #[cfg(test)]
        MANIFEST_READS.with(|reads| {
            if let Some(reads) = reads.borrow_mut().as_mut() {
                let entry = reads.entry(path.clone()).or_default();
                entry.0 += 1;
                entry.1 += bytes.len() as u64;
            }
        });
        let manifest = ManifestV2::from_json_bytes(&bytes)?;
        if manifest.content_fingerprint()? != name.content {
            return Err(ViewError::GenerationMismatch(format!(
                "manifest {generation} does not match its content fingerprint"
            )));
        }
        Ok(manifest)
    }
}

#[cfg(test)]
type ManifestReads = BTreeMap<std::path::PathBuf, (u64, u64)>;

#[cfg(test)]
thread_local! {
    static MANIFEST_READS: std::cell::RefCell<Option<ManifestReads>> = const { std::cell::RefCell::new(None) };
}

/// Counts successful manifest file reads and their byte demand on this thread,
/// at the actual read boundary rather than at a marking or cache lookup site.
#[cfg(test)]
pub(crate) struct ManifestReadProbe;

#[cfg(test)]
impl ManifestReadProbe {
    pub(crate) fn start() -> Self {
        MANIFEST_READS.with(|reads| {
            let mut reads = reads.borrow_mut();
            assert!(reads.is_none(), "manifest read probes cannot nest");
            *reads = Some(BTreeMap::new());
        });
        Self
    }

    pub(crate) fn finish(self) -> ManifestReads {
        MANIFEST_READS.with(|reads| reads.borrow_mut().take().unwrap())
    }
}

#[cfg(test)]
impl Drop for ManifestReadProbe {
    fn drop(&mut self) {
        MANIFEST_READS.with(|reads| {
            reads.borrow_mut().take();
        });
    }
}

fn write_bytes_once(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::production_storage::refuse_write(path)
        .map_err(|error| ViewError::io_at("writing manifest", path, error))?;
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

    #[test]
    fn publication_probe_closes_before_generation_maintenance_is_scheduled() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{mpsc, Arc};
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let registry =
            super::super::registry::FamilyRegistry::open(storage.path(), "probe-order").unwrap();
        let owner = registry
            .register_view_with_storage(storage.path(), "worker", root.path())
            .unwrap();
        let view = owner.view_store().unwrap();
        let first_manifest = manifest();
        let first = GenerationName::for_manifest(&first_manifest).unwrap();
        let prepared = view
            .prepare_v2(&first, None, &first_manifest, None)
            .unwrap();
        assert_eq!(
            view.commit_v2(prepared, None).unwrap(),
            PublishV2::Published
        );
        super::super::wait_for_generation_maintenance_for_test(view.view_dir());
        let mut next_manifest = first_manifest;
        next_manifest.header_mut().head_tree = Some("next-tree".into());
        let next = GenerationName::for_manifest(&next_manifest).unwrap();
        let open = Arc::new(AtomicBool::new(false));
        let overlap = Arc::new(AtomicBool::new(false));
        let (started, maintenance_started) = mpsc::channel();
        let observed_open = open.clone();
        let observed_overlap = overlap.clone();
        let _hook = crate::storage_retention::observe_test_hook(
            view.view_dir(),
            Arc::new(move |step| {
                if step == "post-publication-sweep" {
                    observed_overlap
                        .fetch_or(observed_open.load(Ordering::SeqCst), Ordering::SeqCst);
                    let _ = started.send(());
                }
            }),
        );
        let (entered, probe_opened) = mpsc::channel();
        let (resume, finish_probe) = mpsc::channel();
        let publishing = view.clone();
        let next_for_worker = next.clone();
        let first_name = first.to_string();
        let worker = std::thread::spawn(move || {
            let prepared = publishing
                .prepare_v2(&next_for_worker, Some(&first_name), &next_manifest, None)
                .unwrap();
            publishing.commit_v2_before_sweep(prepared, None, || {
                let connection = crate::db::file_identity::IdentityConnection::open_with_flags(
                    publishing.pointer_path(),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                    "publication probe ordering test",
                )
                .unwrap();
                let _: String = connection
                    .query_row(
                        "SELECT generation FROM pointer WHERE singleton=1",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                open.store(true, Ordering::SeqCst);
                entered.send(()).unwrap();
                finish_probe
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                drop(connection);
                open.store(false, Ordering::SeqCst);
                Ok(())
            })
        });
        probe_opened
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert!(crate::db::file_identity::open_connections(&view.pointer_path()) > 0);
        let scheduled_while_open = maintenance_started
            .recv_timeout(std::time::Duration::from_millis(200))
            .is_ok();
        resume.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap(), PublishV2::Published);
        super::super::wait_for_generation_maintenance_for_test(view.view_dir());
        assert!(
            !scheduled_while_open,
            "maintenance started before the publication probe closed"
        );
        assert!(!overlap.load(Ordering::SeqCst));
        assert_eq!(
            view.current_generation_read_only().unwrap(),
            Some(next.to_string())
        );
        assert!(view.load_manifest_v2(&next.to_string()).is_ok());
    }

    #[test]
    fn completion_error_does_not_undo_a_landed_pointer() {
        let storage = tempfile::tempdir().unwrap();
        let view = ViewStore::open(storage.path(), "completion-error").unwrap();
        let manifest = manifest();
        let name = GenerationName::for_manifest(&manifest).unwrap();
        let prepared = view.prepare_v2(&name, None, &manifest, None).unwrap();
        assert!(view
            .commit_v2_before_sweep(prepared, None, || Err(ViewError::InvalidManifest(
                "probe failure".into()
            )))
            .is_err());
        super::super::wait_for_generation_maintenance_for_test(view.view_dir());
        assert_eq!(
            view.current_generation_read_only().unwrap(),
            Some(name.to_string())
        );
        assert!(view.load_manifest_v2(&name.to_string()).is_ok());
    }

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
    fn v2_publication_burst_reclaims_superseded_generations() {
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let registry =
            crate::views::registry::FamilyRegistry::open(storage.path(), "0123456789abcdef")
                .unwrap();
        let registration = registry
            .register_view_with_storage(storage.path(), "burst", &root)
            .unwrap();
        let view = registration.view_store().unwrap();
        let mut previous = None;
        let mut generations = Vec::new();
        for index in 0..8 {
            let mut manifest = manifest();
            manifest.header_mut().head_tree = Some(format!("tree-{index}"));
            let name = GenerationName::for_manifest(&manifest).unwrap();
            let pin =
                crate::pins::AssemblyPin::create_v2(&registration, name.to_string(), &[]).unwrap();
            let prepared = view
                .prepare_v2(&name, previous.as_deref(), &manifest, None)
                .unwrap();
            let database = view.derived_path(&name.to_string()).unwrap();
            let connection = crate::db::file_identity::IdentityConnection::open(
                &database,
                "V2 publication test",
            )
            .unwrap();
            connection
                .execute_batch("CREATE TABLE state (value TEXT);")
                .unwrap();
            drop(connection);
            // A caller may still own the family barrier at commit. The worker
            // must acquire it before the pointer, not re-enter it on this thread.
            let outcome = registry
                .with_barrier(|_| {
                    view.commit_v2(prepared, None)
                        .map_err(crate::views::registry::RegistryError::View)
                })
                .unwrap();
            assert_eq!(outcome, PublishV2::Published);
            previous = Some(name.to_string());
            generations.push(name.to_string());
            drop(pin);
        }
        crate::views::generation::wait_for_generation_sweep_for_test(view.view_dir());
        for generation in &generations[..generations.len() - 1] {
            assert!(
                !view.manifest_path(generation).unwrap().exists(),
                "superseded V2 manifest survived: {generation}"
            );
            assert!(
                !view.derived_path(generation).unwrap().exists(),
                "superseded V2 database survived: {generation}"
            );
        }
        assert!(view
            .manifest_path(generations.last().unwrap())
            .unwrap()
            .exists());
        assert!(view
            .derived_path(generations.last().unwrap())
            .unwrap()
            .exists());
    }

    #[test]
    fn v2_inflight_commit_protects_private_and_equivalent_generations() {
        struct Pause {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl DurabilityObserver for Pause {
            fn reached(&self, step: DurabilityStep) {
                if step == DurabilityStep::PointerCas {
                    self.entered.send(()).unwrap();
                    self.release
                        .lock()
                        .unwrap()
                        .recv_timeout(std::time::Duration::from_secs(20))
                        .unwrap();
                }
            }
        }
        let storage = tempfile::tempdir().unwrap();
        let registry =
            crate::views::registry::FamilyRegistry::open(storage.path(), "family").unwrap();
        let registration = registry
            .register_view_with_storage(
                storage.path(),
                "inflight",
                &storage.path().join("absent-root"),
            )
            .unwrap();
        let view = registration.view_store().unwrap();
        let first_manifest = manifest();
        let first = GenerationName::for_manifest(&first_manifest).unwrap();
        view.commit_v2(
            view.prepare_v2(&first, None, &first_manifest, None)
                .unwrap(),
            None,
        )
        .unwrap();
        crate::views::generation::wait_for_generation_sweep_for_test(view.view_dir());
        let private = GenerationName::for_manifest(&first_manifest).unwrap();
        let prepared = view
            .prepare_v2(&private, None, &first_manifest, None)
            .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let committing_view = view.clone();
        let commit = std::thread::spawn(move || {
            committing_view
                .commit_v2(
                    prepared,
                    Some(&Pause {
                        entered: entered_tx,
                        release: std::sync::Mutex::new(release_rx),
                    }),
                )
                .unwrap()
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let mut replacement_manifest = first_manifest;
        replacement_manifest.header_mut().head_tree = Some("replacement".into());
        let replacement = GenerationName::for_manifest(&replacement_manifest).unwrap();
        view.commit_v2(
            view.prepare_v2(
                &replacement,
                Some(&first.to_string()),
                &replacement_manifest,
                None,
            )
            .unwrap(),
            None,
        )
        .unwrap();
        crate::views::generation::wait_for_generation_sweep_for_test(view.view_dir());
        let private_kept = view.manifest_path(&private.to_string()).unwrap().is_file();
        let equivalent_kept = view.manifest_path(&first.to_string()).unwrap().is_file();
        release_tx.send(()).unwrap();
        let outcome = commit.join().unwrap();
        assert!(
            private_kept,
            "cleanup deleted a private generation while its commit was in flight"
        );
        assert!(
            equivalent_kept,
            "cleanup deleted the EquivalentWinner target before commit returned"
        );
        assert_eq!(
            outcome,
            PublishV2::EquivalentWinner {
                current: first.to_string()
            }
        );
        assert_eq!(
            view.current_generation().unwrap(),
            Some(replacement.to_string())
        );
        view.sweep_generations().unwrap();
        assert!(!view.manifest_path(&private.to_string()).unwrap().exists());
        assert!(!view.manifest_path(&first.to_string()).unwrap().exists());
        assert!(view
            .manifest_path(&replacement.to_string())
            .unwrap()
            .is_file());
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
    fn preparation_hashes_the_bytes_it_writes_with_one_serialization() {
        let storage = tempfile::tempdir().unwrap();
        let view = ViewStore::open(storage.path(), "serialization").unwrap();
        let manifest = manifest();
        let canonical = serde_json::to_vec(&JsonManifestV2 {
            format_version: MANIFEST_FORMAT_VERSION,
            path_identity_version: PATH_IDENTITY_VERSION,
            header: manifest.header.clone(),
            base: None,
            published: None,
            tombstones: Vec::new(),
            readiness_overrides: Vec::new(),
            entries: manifest
                .entries
                .iter()
                .map(|(rel_path, entry)| JsonEntry {
                    rel_path: rel_path.clone(),
                    entry: entry.clone(),
                })
                .collect(),
        })
        .unwrap();
        assert_eq!(manifest.to_json_bytes().unwrap(), canonical);
        let name = GenerationName::for_manifest(&manifest).unwrap();
        SERIALIZATIONS.with(|count| count.set(0));
        view.prepare_v2(&name, None, &manifest, None).unwrap();
        assert_eq!(SERIALIZATIONS.with(|count| count.get()), 1);
        let mut written = fs::read(view.manifest_path(&name.to_string()).unwrap()).unwrap();
        assert_eq!(written.pop(), Some(b'\n'));
        assert_eq!(written, canonical);
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

    #[test]
    fn registered_builder_name_probe_child() {
        let Some(storage) = std::env::var_os("AFT_TEST_PRIVATE_BUILDER_STORAGE") else {
            return;
        };
        let root = std::env::var_os("AFT_TEST_PRIVATE_BUILDER_ROOT").unwrap();
        let registry = super::super::registry::FamilyRegistry::open(
            Path::new(&storage),
            "private-name-family",
        )
        .unwrap();
        let owner = registry
            .register_view_with_storage(registry.storage(), "shared-view", Path::new(&root))
            .unwrap();
        let name = GenerationName::for_manifest(&manifest()).unwrap();
        println!(
            "PRIVATE_BUILDER={}",
            serde_json::to_string(&(name.to_string(), owner.view_dir())).unwrap()
        );
    }

    #[test]
    fn registered_builders_keep_equal_manifest_generation_paths_private() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let registry =
            super::super::registry::FamilyRegistry::open(storage.path(), "private-name-family")
                .unwrap();
        let first = registry
            .register_view_with_storage(registry.storage(), "shared-view", root.path())
            .unwrap();
        let second = registry
            .register_view_with_storage(registry.storage(), "shared-view", root.path())
            .unwrap();
        assert_eq!(first.view_dir(), second.view_dir());
        let view = first.view_store().unwrap();
        let manifest = manifest();
        let a = GenerationName::for_manifest(&manifest).unwrap();
        let b = GenerationName::for_manifest(&manifest).unwrap();
        let a_path = view.derived_path(&a.to_string()).unwrap();
        let b_path = view.derived_path(&b.to_string()).unwrap();
        assert!(!a_path.exists() && !b_path.exists());
        assert!(a.same_content(&b));
        assert_ne!(
            a_path, b_path,
            "same-process builders must not share a destination"
        );

        let spawn = || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "views::manifest_v2::tests::registered_builder_name_probe_child",
                    "--nocapture",
                ])
                .env("AFT_TEST_PRIVATE_BUILDER_STORAGE", storage.path())
                .env("AFT_TEST_PRIVATE_BUILDER_ROOT", root.path())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        let children = [spawn(), spawn()];
        let mut names = BTreeSet::from([a.to_string(), b.to_string()]);
        for child in children {
            let pid = child.id();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            let record = stdout
                .lines()
                .find_map(|line| line.strip_prefix("PRIVATE_BUILDER="))
                .unwrap();
            let (name, view_dir): (String, std::path::PathBuf) =
                serde_json::from_str(record).unwrap();
            assert_eq!(view_dir, first.view_dir());
            let parsed = GenerationName::parse(&name).unwrap();
            assert_eq!(parsed.pid, pid);
            assert!(a.same_content(&parsed));
            assert!(!view.derived_path(&name).unwrap().exists());
            assert!(
                names.insert(name),
                "different processes must not share a destination"
            );
        }
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn prepared_manifests_stay_pinned_until_commit_resolves() {
        let storage = tempfile::tempdir().unwrap();
        let registry =
            crate::views::registry::FamilyRegistry::open(storage.path(), "prepared-builders")
                .unwrap();
        let registration = registry
            .register_view_with_storage(storage.path(), "prepared", &storage.path().join("root"))
            .unwrap();
        let view = registration.view_store().unwrap();
        let manifest = manifest();
        let a = GenerationName::for_manifest(&manifest).unwrap();
        let b = GenerationName::for_manifest(&manifest).unwrap();
        let first = view.prepare_v2(&a, None, &manifest, None).unwrap();
        let second = view.prepare_v2(&b, None, &manifest, None).unwrap();

        view.sweep_generations().unwrap();
        assert!(
            view.load_manifest_v2(&a.to_string()).is_ok(),
            "a prepared winner must survive a sweep before CAS"
        );
        assert!(
            view.load_manifest_v2(&b.to_string()).is_ok(),
            "an unresolved competing builder must remain protected"
        );
        assert_eq!(view.commit_v2(first, None).unwrap(), PublishV2::Published);

        view.sweep_generations().unwrap();
        assert!(view.load_manifest_v2(&b.to_string()).is_ok());
        assert_eq!(
            view.commit_v2(second, None).unwrap(),
            PublishV2::EquivalentWinner {
                current: a.to_string()
            }
        );

        view.sweep_generations().unwrap();
        assert!(view.load_manifest_v2(&a.to_string()).is_ok());
        assert!(
            !view.manifest_path(&b.to_string()).unwrap().exists(),
            "the unpinned loser can be reclaimed after commit returns"
        );
    }

    #[test]
    fn resolved_equivalent_winner_stays_pinned_during_commit_observer() {
        struct Supersede<'a> {
            view: &'a ViewStore,
            replacement: std::cell::RefCell<Option<PreparedV2>>,
            winner: String,
            attempt: String,
            observed: std::cell::Cell<bool>,
        }
        impl DurabilityObserver for Supersede<'_> {
            fn reached(&self, step: DurabilityStep) {
                if step == DurabilityStep::PointerCas {
                    self.observed.set(true);
                    assert_eq!(
                        self.view
                            .commit_v2(self.replacement.borrow_mut().take().unwrap(), None)
                            .unwrap(),
                        PublishV2::Published
                    );
                    self.view.sweep_generations().unwrap();
                    assert!(
                        self.view.load_manifest_v2(&self.winner).is_ok(),
                        "the resolved winner must survive until commit returns"
                    );
                    assert!(
                        self.view.load_manifest_v2(&self.attempt).is_ok(),
                        "the private attempt must survive until commit returns"
                    );
                }
            }
        }
        let storage = tempfile::tempdir().unwrap();
        let registry =
            crate::views::registry::FamilyRegistry::open(storage.path(), "resolved-builders")
                .unwrap();
        let registration = registry
            .register_view_with_storage(storage.path(), "resolved", &storage.path().join("root"))
            .unwrap();
        let view = registration.view_store().unwrap();
        let manifest = manifest();
        let winner = GenerationName::for_manifest(&manifest).unwrap();
        view.commit_v2(
            view.prepare_v2(&winner, None, &manifest, None).unwrap(),
            None,
        )
        .unwrap();
        let attempt = GenerationName::for_manifest(&manifest).unwrap();
        let prepared = view.prepare_v2(&attempt, None, &manifest, None).unwrap();
        let mut replacement_manifest = manifest;
        replacement_manifest.header_mut().head_tree = Some("replacement".into());
        let replacement = GenerationName::for_manifest(&replacement_manifest).unwrap();
        let observer = Supersede {
            view: &view,
            replacement: std::cell::RefCell::new(Some(
                view.prepare_v2(
                    &replacement,
                    Some(&winner.to_string()),
                    &replacement_manifest,
                    None,
                )
                .unwrap(),
            )),
            winner: winner.to_string(),
            attempt: attempt.to_string(),
            observed: std::cell::Cell::new(false),
        };
        assert_eq!(
            view.commit_v2(prepared, Some(&observer)).unwrap(),
            PublishV2::EquivalentWinner {
                current: winner.to_string()
            }
        );
        assert!(observer.observed.get());
        view.sweep_generations().unwrap();
        assert!(!view.manifest_path(&winner.to_string()).unwrap().exists());
        assert!(!view.manifest_path(&attempt.to_string()).unwrap().exists());
        assert!(view.load_manifest_v2(&replacement.to_string()).is_ok());
    }
}
