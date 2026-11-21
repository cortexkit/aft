//! Session-owned hashline snapshot publication, rendering, and residency.
//!
//! The scanner owns the byte model and produces a coherent whole-file tag.  This
//! module owns the part that is deliberately session-local: deciding which
//! displayed rows are seen, carrying the tag in text, and keeping only a bounded
//! history of snapshots.  No state in this module is persisted to disk.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::hashline::scan::{scan_bytes_with_request, CaptureError};

pub use crate::hashline::scan::{
    BoundaryEvidence, CoverageInput, RawLineRecord, RetainedLine, ScanCoverage, ScanRequest,
    ScanResult, Snapshot, Terminator, TerminatorKind,
};

/// The maximum complete file that may be read for a hashline snapshot.
///
/// A tag hashes the whole file, even when only a range is rendered.  Refusing
/// larger files keeps that invariant explicit instead of silently minting a tag
/// from a partial byte stream.
pub const MAX_FILE_READ_BYTES: u64 = 64 * 1024 * 1024;
/// The existing read response's line-numbered body budget.
pub const MAX_RENDER_BYTES: usize = 50 * 1024;
/// The existing read response's display-only line length limit.
pub const MAX_RENDER_LINE_LENGTH: usize = 2_000;
/// Maximum number of canonical paths resident in one session.
pub const MAX_SNAPSHOT_PATHS: usize = 30;
/// Maximum number of versions retained for one canonical path.
pub const MAX_VERSIONS_PER_PATH: usize = 4;
/// Maximum snapshot residency across one session, including exact normalized
/// content identities and retained raw records.
pub const MAX_SNAPSHOT_TOTAL_BYTES: usize = 64 * 1024 * 1024;
/// Maximum bounded history of handles removed by residency eviction.
pub const MAX_EVICTION_RECORDS: usize = 256;

/// A range of absolute, one-based output rows.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl LineRange {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn is_empty(self) -> bool {
        self.start == 0 || self.start > self.end
    }

    pub fn contains(self, line: usize) -> bool {
        !self.is_empty() && (self.start..=self.end).contains(&line)
    }
}

/// A mutation's output rows that should be carried by an edit response.
///
/// Ranges are normalized and coalesced when they are used.  The renderer adds
/// the nearest surviving predecessor and successor to each range so a chained
/// edit has a small amount of stable context without publishing the whole file.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AffectedRegion {
    pub ranges: Vec<LineRange>,
}

impl AffectedRegion {
    pub fn new(ranges: impl IntoIterator<Item = LineRange>) -> Self {
        Self {
            ranges: coalesce_ranges(ranges),
        }
    }

    pub fn from_range(start: usize, end: usize) -> Self {
        Self::new([LineRange::new(start, end)])
    }

    pub fn insertion(start: usize, inserted_lines: usize) -> Self {
        if inserted_lines == 0 {
            return Self::default();
        }
        Self::from_range(
            start,
            start.saturating_add(inserted_lines).saturating_sub(1),
        )
    }

    pub fn deletion(start: usize, end: usize) -> Self {
        Self::from_range(start, end)
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

/// A read selection before the scanner knows the final line count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadSelection {
    WholeFile,
    Range { start: usize, end: usize },
    Lines(BTreeSet<usize>),
    Head(usize),
    Tail(usize),
}

impl Default for ReadSelection {
    fn default() -> Self {
        Self::WholeFile
    }
}

impl ReadSelection {
    pub const fn whole_file() -> Self {
        Self::WholeFile
    }

    pub const fn range(start: usize, end: usize) -> Self {
        Self::Range { start, end }
    }

    pub const fn head(lines: usize) -> Self {
        Self::Head(lines)
    }

    pub const fn tail(lines: usize) -> Self {
        Self::Tail(lines)
    }

    pub fn lines<I>(lines: I) -> Self
    where
        I: IntoIterator<Item = usize>,
    {
        Self::Lines(lines.into_iter().collect())
    }

    fn scan_request(&self) -> ScanRequest {
        match self {
            Self::WholeFile | Self::Tail(_) => ScanRequest::whole_file(),
            Self::Range { start, end } => ScanRequest::new(CoverageInput::range(*start, *end)),
            Self::Lines(lines) => ScanRequest::new(CoverageInput::lines(lines.iter().copied())),
            Self::Head(lines) => ScanRequest::new(CoverageInput::range(1, *lines)),
        }
    }

    fn selected_lines(&self, total_lines: usize) -> BTreeSet<usize> {
        match self {
            Self::WholeFile => (1..=total_lines).collect(),
            Self::Range { start, end } if *start > *end || *start == 0 => BTreeSet::new(),
            Self::Range { start, end } => (*start..=(*end).min(total_lines)).collect(),
            Self::Lines(lines) => lines
                .iter()
                .copied()
                .filter(|line| *line > 0 && *line <= total_lines)
                .collect(),
            Self::Head(lines) => (1..=(*lines).min(total_lines)).collect(),
            Self::Tail(lines) => {
                let first = total_lines.saturating_sub(*lines).saturating_add(1);
                if *lines == 0 || first > total_lines {
                    BTreeSet::new()
                } else {
                    (first..=total_lines).collect()
                }
            }
        }
    }

    fn is_explicitly_empty(&self) -> bool {
        match self {
            Self::Range { start, end } => *start == 0 || *start > *end,
            Self::Head(lines) | Self::Tail(lines) => *lines == 0,
            Self::Lines(lines) => lines.is_empty(),
            Self::WholeFile => false,
        }
    }
}

/// The three read shapes accepted by the bash rewrite funnel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BashReadKind {
    Cat,
    Head { lines: usize },
    Tail { lines: usize },
}

impl BashReadKind {
    pub const fn selection(self) -> ReadSelection {
        match self {
            Self::Cat => ReadSelection::WholeFile,
            Self::Head { lines } => ReadSelection::Head(lines),
            Self::Tail { lines } => ReadSelection::Tail(lines),
        }
    }
}

/// A tag carried in the agent-visible text of a successful read or edit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaggedRendering {
    pub text: String,
    pub requested_path: String,
    pub tag: String,
    pub seen_lines: BTreeSet<usize>,
    pub rendered_lines: BTreeSet<usize>,
    pub elided_range: Option<LineRange>,
    pub display_truncated_lines: BTreeSet<usize>,
}

impl TaggedRendering {
    pub fn is_empty_body(&self) -> bool {
        self.rendered_lines.is_empty()
    }
}

/// A tagless rendering used when the read is not eligible to publish a handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaglessRendering {
    pub text: String,
    pub requested_path: String,
    pub rendered_lines: BTreeSet<usize>,
    pub elided_range: Option<LineRange>,
}

/// Reasons that deliberately do not mint a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UntaggableReason {
    NotRegularFile,
    ReadOnly,
    VirtualPath,
    Binary,
    InvalidUtf8,
    Oversize { bytes: u64, limit: u64 },
    EmptyRange,
    BeyondEof,
    Io(String),
}

impl fmt::Display for UntaggableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRegularFile => formatter.write_str("path is not a regular file"),
            Self::ReadOnly => formatter.write_str("path is not write-eligible"),
            Self::VirtualPath => formatter.write_str("virtual paths cannot carry snapshots"),
            Self::Binary => formatter.write_str("binary files cannot carry snapshots"),
            Self::InvalidUtf8 => formatter.write_str("file is not valid UTF-8"),
            Self::Oversize { bytes, limit } => {
                write!(
                    formatter,
                    "file is too large for a snapshot ({bytes} > {limit} bytes)"
                )
            }
            Self::EmptyRange => formatter.write_str("requested range is empty"),
            Self::BeyondEof => formatter.write_str("requested range is beyond EOF"),
            Self::Io(reason) => write!(formatter, "read failed: {reason}"),
        }
    }
}

/// The result of a taggable or tagless read publication attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadPublication {
    Tagged {
        snapshot: Snapshot,
        rendering: TaggedRendering,
    },
    Tagless {
        rendering: TaglessRendering,
        reason: UntaggableReason,
    },
}

impl ReadPublication {
    pub fn snapshot(&self) -> Option<&Snapshot> {
        match self {
            Self::Tagged { snapshot, .. } => Some(snapshot),
            Self::Tagless { .. } => None,
        }
    }

    pub fn tagged_rendering(&self) -> Option<&TaggedRendering> {
        match self {
            Self::Tagged { rendering, .. } => Some(rendering),
            Self::Tagless { .. } => None,
        }
    }

    pub fn text(&self) -> &str {
        match self {
            Self::Tagged { rendering, .. } => &rendering.text,
            Self::Tagless { rendering, .. } => &rendering.text,
        }
    }
}

/// A deterministic handle removed from snapshot residency.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EvictionRecord {
    pub canonical_path: PathBuf,
    pub tag: String,
}

impl EvictionRecord {
    fn new(path: &Path, tag: &str) -> Self {
        Self {
            canonical_path: path.to_path_buf(),
            tag: fold_tag(tag),
        }
    }
}

type ContentFingerprint = [u8; 32];

#[derive(Clone, Debug)]
struct EvictionHistoryEntry {
    record: EvictionRecord,
    content_fingerprint: ContentFingerprint,
}

#[derive(Clone, Debug)]
struct StoredSnapshot {
    snapshot: Snapshot,
    normalized_bytes: Vec<u8>,
    published_at: u64,
    last_used: u64,
}

impl StoredSnapshot {
    fn residency_bytes(&self) -> usize {
        self.snapshot
            .residency_bytes()
            .saturating_add(self.normalized_bytes.len())
    }

    fn has_same_normalized_content(&self, tag: &str, normalized_bytes: &[u8]) -> bool {
        fold_tag(&self.snapshot.tag) == fold_tag(tag) && self.normalized_bytes == normalized_bytes
    }
}

/// The outcome of publishing one snapshot into a bounded store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishStatus {
    Stored,
    Oversize { retained_bytes: usize, limit: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishOutcome {
    pub status: PublishStatus,
    /// The evidence exposed by this publication. The resident store may hold a
    /// merged superset after coalescing another read of the same content.
    pub snapshot: Option<Snapshot>,
    pub evicted: Vec<EvictionRecord>,
}

impl PublishOutcome {
    pub fn stored(&self) -> bool {
        matches!(self.status, PublishStatus::Stored)
    }

    pub fn oversize(&self) -> bool {
        matches!(self.status, PublishStatus::Oversize { .. })
    }
}

/// Lookup failures map directly to the registered hashline rejection codes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotLookupError {
    UnknownTag,
    EvictedTag,
    AmbiguousTag,
}

impl SnapshotLookupError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownTag => "hashline_unknown_tag",
            Self::EvictedTag => "hashline_evicted_tag",
            Self::AmbiguousTag => "hashline_ambiguous_tag",
        }
    }

    pub const fn steering(&self) -> &'static str {
        match self {
            Self::AmbiguousTag => {
                "use apply_patch or another available non-hashline edit surface; re-reading preserves this colliding four-hex tag"
            }
            Self::UnknownTag | Self::EvictedTag => {
                "run `read <path>` again to mint a fresh tag, then retry the edit"
            }
        }
    }
}

impl fmt::Display for SnapshotLookupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for SnapshotLookupError {}

/// A result which exposes ambiguity separately from evicted and unknown tags.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotLookup {
    Found(Snapshot),
    Unknown,
    Evicted,
    Ambiguous,
}

/// Bounded, session-owned snapshot and eviction-history storage.
#[derive(Clone, Debug, Default)]
pub struct SnapshotStore {
    paths: BTreeMap<PathBuf, Vec<StoredSnapshot>>,
    total_bytes: usize,
    clock: u64,
    eviction_history: VecDeque<EvictionHistoryEntry>,
}

impl SnapshotStore {
    pub const fn new() -> Self {
        Self {
            paths: BTreeMap::new(),
            total_bytes: 0,
            clock: 0,
            eviction_history: VecDeque::new(),
        }
    }

    pub fn snapshot_count(&self) -> usize {
        self.paths.values().map(Vec::len).sum()
    }

    pub fn path_count(&self) -> usize {
        self.paths.len()
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn eviction_history_len(&self) -> usize {
        self.eviction_history.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snapshot_count() == 0
    }

    pub fn clear(&mut self) {
        self.paths.clear();
        self.total_bytes = 0;
        self.eviction_history.clear();
    }

    /// Publish a snapshot. Oversize publication is a no-op: it cannot evict a
    /// resident entry, create history, or perturb recency.
    pub fn publish(&mut self, path: impl AsRef<Path>, snapshot: Snapshot) -> PublishOutcome {
        let path = canonical_key(path.as_ref());
        let mut published_snapshot = snapshot;
        let normalized_bytes = std::mem::take(&mut published_snapshot.normalized_bytes);

        // The four-hex handle is deliberately lossy. Exact normalized bytes are
        // retained separately so only byte-identical content coalesces; a real
        // tag collision remains represented by distinct resident versions.
        let coalesced = self.paths.get(&path).and_then(|versions| {
            versions
                .iter()
                .position(|version| {
                    version.has_same_normalized_content(&published_snapshot.tag, &normalized_bytes)
                })
                .map(|index| {
                    (
                        index,
                        merge_snapshot_evidence(&versions[index].snapshot, &published_snapshot),
                    )
                })
        });
        let retained_bytes = coalesced.as_ref().map_or_else(
            || {
                published_snapshot
                    .residency_bytes()
                    .saturating_add(normalized_bytes.len())
            },
            |(_, merged)| {
                merged
                    .residency_bytes()
                    .saturating_add(normalized_bytes.len())
            },
        );
        if published_snapshot.byte_count > MAX_FILE_READ_BYTES
            || retained_bytes > MAX_SNAPSHOT_TOTAL_BYTES
        {
            return PublishOutcome {
                status: PublishStatus::Oversize {
                    retained_bytes,
                    limit: MAX_SNAPSHOT_TOTAL_BYTES,
                },
                snapshot: None,
                evicted: Vec::new(),
            };
        }

        let mut evicted = Vec::new();
        self.bump_clock();
        let now = self.clock;

        if let Some((index, merged)) = coalesced {
            let previous_bytes = self.paths[&path][index].residency_bytes();
            self.remove_history_for_content(
                &EvictionRecord::new(&path, &published_snapshot.tag),
                &normalized_bytes,
            );
            let version = &mut self
                .paths
                .get_mut(&path)
                .expect("coalesced snapshot path remains resident")[index];
            version.snapshot = merged;
            // A same-content re-read is a fresh retention claim. Refresh both
            // clocks so the per-path version cap cannot evict the newly exposed
            // tag using the age of the partial snapshot it coalesced into.
            version.published_at = now;
            version.last_used = now;
            let merged_bytes = version.residency_bytes();
            self.total_bytes = self
                .total_bytes
                .saturating_sub(previous_bytes)
                .saturating_add(merged_bytes);

            while self.total_bytes > MAX_SNAPSHOT_TOTAL_BYTES {
                let Some((oldest_path, oldest_index)) = self.least_recent_version() else {
                    break;
                };
                evicted.push(self.remove_version_for_eviction(&oldest_path, oldest_index));
            }

            return PublishOutcome {
                status: PublishStatus::Stored,
                snapshot: Some(published_snapshot),
                evicted,
            };
        }

        if !self.paths.contains_key(&path) && self.paths.len() >= MAX_SNAPSHOT_PATHS {
            if let Some(oldest_path) = self.least_recent_path() {
                evicted.extend(self.remove_path_for_eviction(&oldest_path));
            }
        }

        if let Some(versions) = self.paths.get(&path) {
            if versions.len() >= MAX_VERSIONS_PER_PATH {
                if let Some(index) = versions
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, version)| (version.published_at, version.last_used))
                    .map(|(index, _)| index)
                {
                    evicted.push(self.remove_version_for_eviction(&path, index));
                }
            }
        }

        self.remove_history_for_content(
            &EvictionRecord::new(&path, &published_snapshot.tag),
            &normalized_bytes,
        );
        self.total_bytes = self.total_bytes.saturating_add(retained_bytes);
        self.paths
            .entry(path.clone())
            .or_default()
            .push(StoredSnapshot {
                snapshot: published_snapshot.clone(),
                normalized_bytes,
                published_at: now,
                last_used: now,
            });

        while self.total_bytes > MAX_SNAPSHOT_TOTAL_BYTES {
            let Some((oldest_path, index)) = self.least_recent_version() else {
                break;
            };
            evicted.push(self.remove_version_for_eviction(&oldest_path, index));
        }

        PublishOutcome {
            status: PublishStatus::Stored,
            snapshot: Some(published_snapshot),
            evicted,
        }
    }

    /// Alias emphasizing that publication is the only way to make a snapshot
    /// visible to later edits.
    pub fn insert(&mut self, path: impl AsRef<Path>, snapshot: Snapshot) -> PublishOutcome {
        self.publish(path, snapshot)
    }

    pub fn publish_bytes(
        &mut self,
        path: impl AsRef<Path>,
        bytes: &[u8],
        coverage: CoverageInput,
    ) -> PublishOutcome {
        let snapshot = scan_bytes_with_request(bytes, ScanRequest::new(coverage))
            .snapshot
            .expect("in-memory scans always observe EOF");
        self.publish(path, snapshot)
    }

    pub fn lookup(
        &mut self,
        path: impl AsRef<Path>,
        tag: &str,
    ) -> Result<Snapshot, SnapshotLookupError> {
        let path = canonical_key(path.as_ref());
        let folded = fold_tag(tag);
        let Some(versions) = self.paths.get(&path) else {
            return if self.history_contains(&path, &folded) {
                Err(SnapshotLookupError::EvictedTag)
            } else {
                Err(SnapshotLookupError::UnknownTag)
            };
        };

        let indices: Vec<usize> = versions
            .iter()
            .enumerate()
            .filter_map(|(index, version)| {
                (fold_tag(&version.snapshot.tag) == folded).then_some(index)
            })
            .collect();
        if indices.is_empty() {
            return if self.history_contains(&path, &folded) {
                Err(SnapshotLookupError::EvictedTag)
            } else {
                Err(SnapshotLookupError::UnknownTag)
            };
        }

        let first_index = indices[0];
        let first_content = &versions[first_index].normalized_bytes;
        if indices
            .iter()
            .skip(1)
            .any(|index| versions[*index].normalized_bytes != *first_content)
        {
            return Err(SnapshotLookupError::AmbiguousTag);
        }
        if self.history_contains_different_content(&path, &folded, first_content) {
            return Err(SnapshotLookupError::EvictedTag);
        }
        let resolved = versions[first_index].snapshot.clone();

        self.bump_clock();
        let now = self.clock;
        let versions = self
            .paths
            .get_mut(&path)
            .expect("snapshot path remains resident during lookup");
        for index in indices {
            versions[index].last_used = now;
        }
        Ok(resolved)
    }

    pub fn resolve(
        &mut self,
        path: impl AsRef<Path>,
        tag: &str,
    ) -> Result<Snapshot, SnapshotLookupError> {
        self.lookup(path, tag)
    }

    pub fn lookup_state(&self, path: impl AsRef<Path>, tag: &str) -> SnapshotLookup {
        let path = canonical_key(path.as_ref());
        let folded = fold_tag(tag);
        let Some(versions) = self.paths.get(&path) else {
            return if self.history_contains(&path, &folded) {
                SnapshotLookup::Evicted
            } else {
                SnapshotLookup::Unknown
            };
        };
        let candidates: Vec<&StoredSnapshot> = versions
            .iter()
            .filter(|version| fold_tag(&version.snapshot.tag) == folded)
            .collect();
        if candidates.is_empty() {
            return if self.history_contains(&path, &folded) {
                SnapshotLookup::Evicted
            } else {
                SnapshotLookup::Unknown
            };
        }
        let first = candidates[0];
        if candidates
            .iter()
            .skip(1)
            .any(|candidate| candidate.normalized_bytes != first.normalized_bytes)
        {
            SnapshotLookup::Ambiguous
        } else if self.history_contains_different_content(&path, &folded, &first.normalized_bytes) {
            SnapshotLookup::Evicted
        } else {
            SnapshotLookup::Found(first.snapshot.clone())
        }
    }

    pub fn contains(&self, path: impl AsRef<Path>, tag: &str) -> bool {
        matches!(self.lookup_state(path, tag), SnapshotLookup::Found(_))
    }

    /// Remove a path because its lifecycle ended (for example, an MV source).
    /// Lifecycle invalidation is intentionally not an eviction and therefore
    /// must not create an eviction-history record.
    pub fn invalidate_path(&mut self, path: impl AsRef<Path>) -> bool {
        let path = canonical_key(path.as_ref());
        let Some(versions) = self.paths.remove(&path) else {
            return false;
        };
        self.total_bytes = self
            .total_bytes
            .saturating_sub(versions.iter().map(StoredSnapshot::residency_bytes).sum());
        true
    }

    pub fn remove_path(&mut self, path: impl AsRef<Path>) -> bool {
        self.invalidate_path(path)
    }

    pub fn eviction_history_contains(&self, path: impl AsRef<Path>, tag: &str) -> bool {
        self.history_contains(&canonical_key(path.as_ref()), &fold_tag(tag))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Path, &Snapshot)> {
        self.paths.iter().flat_map(|(path, versions)| {
            versions
                .iter()
                .map(move |version| (path.as_path(), &version.snapshot))
        })
    }

    fn bump_clock(&mut self) {
        self.clock = self.clock.saturating_add(1);
    }

    fn least_recent_path(&self) -> Option<PathBuf> {
        self.paths
            .iter()
            .map(|(path, versions)| {
                let last_used = versions
                    .iter()
                    .map(|version| version.last_used)
                    .max()
                    .unwrap_or(0);
                let inserted = versions
                    .iter()
                    .map(|version| version.published_at)
                    .min()
                    .unwrap_or(0);
                (last_used, inserted, path)
            })
            .min_by(|left, right| {
                left.0
                    .cmp(&right.0)
                    .then(left.1.cmp(&right.1))
                    .then(left.2.cmp(right.2))
            })
            .map(|(_, _, path)| path.clone())
    }

    fn least_recent_version(&self) -> Option<(PathBuf, usize)> {
        self.paths
            .iter()
            .flat_map(|(path, versions)| {
                versions.iter().enumerate().map(move |(index, version)| {
                    (
                        version.last_used,
                        version.published_at,
                        path.clone(),
                        index,
                        fold_tag(&version.snapshot.tag),
                    )
                })
            })
            .min_by(|left, right| {
                left.0
                    .cmp(&right.0)
                    .then(left.1.cmp(&right.1))
                    .then(left.2.cmp(&right.2))
                    .then(left.4.cmp(&right.4))
                    .then(left.3.cmp(&right.3))
            })
            .map(|(_, _, path, index, _)| (path, index))
    }

    fn remove_version_for_eviction(&mut self, path: &Path, index: usize) -> EvictionRecord {
        let (record, fingerprint, should_remove_path) = {
            let versions = self.paths.get_mut(path).expect("version path exists");
            let removed = versions.remove(index);
            let record = EvictionRecord::new(path, &removed.snapshot.tag);
            let fingerprint = content_fingerprint(&removed.normalized_bytes);
            self.total_bytes = self.total_bytes.saturating_sub(removed.residency_bytes());
            (record, fingerprint, versions.is_empty())
        };
        if should_remove_path {
            self.paths.remove(path);
        }
        self.record_eviction(record.clone(), fingerprint);
        record
    }

    fn remove_path_for_eviction(&mut self, path: &Path) -> Vec<EvictionRecord> {
        let Some(versions) = self.paths.remove(path) else {
            return Vec::new();
        };
        let mut records = Vec::with_capacity(versions.len());
        for version in versions {
            self.total_bytes = self.total_bytes.saturating_sub(version.residency_bytes());
            let record = EvictionRecord::new(path, &version.snapshot.tag);
            let fingerprint = content_fingerprint(&version.normalized_bytes);
            self.record_eviction(record.clone(), fingerprint);
            records.push(record);
        }
        records
    }

    fn record_eviction(&mut self, record: EvictionRecord, content_fingerprint: ContentFingerprint) {
        self.remove_history(&record, &content_fingerprint);
        self.eviction_history.push_back(EvictionHistoryEntry {
            record,
            content_fingerprint,
        });
        while self.eviction_history.len() > MAX_EVICTION_RECORDS {
            self.eviction_history.pop_front();
        }
    }

    fn remove_history(
        &mut self,
        record: &EvictionRecord,
        content_fingerprint: &ContentFingerprint,
    ) {
        self.eviction_history.retain(|entry| {
            entry.record != *record || entry.content_fingerprint != *content_fingerprint
        });
    }

    /// [`Self::remove_history`] for content not fingerprinted yet. The
    /// whole-content hash is computed only when the history holds an entry
    /// for the same path and tag, which is rare: hashing first made every
    /// tagged read and every applied edit hash the whole file again just to
    /// find nothing to remove.
    fn remove_history_for_content(&mut self, record: &EvictionRecord, normalized_bytes: &[u8]) {
        if self
            .eviction_history
            .iter()
            .any(|entry| entry.record == *record)
        {
            self.remove_history(record, &content_fingerprint(normalized_bytes));
        }
    }

    fn history_contains(&self, path: &Path, tag: &str) -> bool {
        self.eviction_history
            .iter()
            .any(|entry| entry.record.canonical_path == path && entry.record.tag == tag)
    }

    fn history_contains_different_content(
        &self,
        path: &Path,
        tag: &str,
        normalized_bytes: &[u8],
    ) -> bool {
        // Hashed only once an entry for the same path and tag turns up.
        let mut fingerprint = None;
        self.eviction_history.iter().any(|entry| {
            entry.record.canonical_path == path
                && entry.record.tag == tag
                && entry.content_fingerprint
                    != *fingerprint.get_or_insert_with(|| content_fingerprint(normalized_bytes))
        })
    }
}

#[cfg(test)]
thread_local! {
    /// Whole-content fingerprints computed on this thread.
    static CONTENT_FINGERPRINTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn content_fingerprint(normalized_bytes: &[u8]) -> ContentFingerprint {
    #[cfg(test)]
    CONTENT_FINGERPRINTS.with(|count| count.set(count.get() + 1));
    *blake3::hash(normalized_bytes).as_bytes()
}

fn merge_snapshot_evidence(existing: &Snapshot, incoming: &Snapshot) -> Snapshot {
    let mut merged = existing.clone();
    merged.records.extend(incoming.records.clone());
    merged
        .retained_lines
        .extend(incoming.retained_lines.clone());
    merged
        .coverage
        .requested_lines
        .extend(incoming.coverage.requested_lines.iter().copied());
    merged.coverage.retain_all |= incoming.coverage.retain_all;
    merged
        .coverage
        .retained_lines
        .extend(incoming.coverage.retained_lines.iter().copied());
    merged
        .coverage
        .seen_lines
        .extend(incoming.coverage.seen_lines.iter().copied());
    merged.coverage.scanned_line_count = merged
        .coverage
        .scanned_line_count
        .max(incoming.coverage.scanned_line_count);
    merged.coverage.total_lines = merged
        .coverage
        .total_lines
        .max(incoming.coverage.total_lines);
    merged.coverage.byte_count = merged.coverage.byte_count.max(incoming.coverage.byte_count);
    merged.coverage.eof_observed |= incoming.coverage.eof_observed;
    merged.boundary.empty_file |= incoming.boundary.empty_file;
    merged.boundary.bof_observed |= incoming.boundary.bof_observed;
    merged.boundary.eof_observed |= incoming.boundary.eof_observed;
    merged.boundary.first_seen = merged.coverage.seen_lines.iter().next().copied();
    merged.boundary.last_seen = merged.coverage.seen_lines.iter().next_back().copied();
    merged.total_lines = merged.total_lines.max(incoming.total_lines);
    merged.byte_count = merged.byte_count.max(incoming.byte_count);
    merged.provenance = incoming.provenance.clone();
    merged.capture_provenance = incoming.capture_provenance.clone();
    merged
}

/// Compares two fully resolved snapshot views. Exact normalized-content
/// coalescing happens during publication, so this only compares retained evidence.
pub fn equivalent_snapshots(left: &Snapshot, right: &Snapshot) -> bool {
    left.records == right.records
        && left.coverage.retained_lines == right.coverage.retained_lines
        && left.coverage.seen_lines == right.coverage.seen_lines
        && left.total_lines == right.total_lines
        && left.boundary.empty_file == right.boundary.empty_file
        && left.boundary.bof_observed == right.boundary.bof_observed
        && left.boundary.first_seen == right.boundary.first_seen
        && left.boundary.last_seen == right.boundary.last_seen
}

impl Snapshot {
    /// Count retained raw-record bytes. The store adds the exact normalized
    /// content identity when enforcing the total residency budget.
    pub fn residency_bytes(&self) -> usize {
        self.records
            .values()
            .map(RawLineRecord::to_bytes)
            .map(|bytes| bytes.len())
            .sum()
    }

    pub fn retained_payload_bytes(&self) -> usize {
        self.residency_bytes()
    }
}

/// Render a snapshot's retained records using the gate-on text carrier.
pub fn render_tagged_snapshot(
    snapshot: &Snapshot,
    requested_path: impl Into<String>,
) -> TaggedRendering {
    render_tagged_snapshot_with_options(snapshot, requested_path, RenderOptions::default())
}

/// Rendering limits are configurable for deterministic unit tests while the
/// default remains the shipped read contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderOptions {
    pub max_output_bytes: usize,
    pub max_line_length: usize,
    /// Headroom for finalization notices. When nonzero, the `[path#TAG]` header
    /// and elision footer also fit inside the advertised limit, not after the body.
    pub reserved_bytes: usize,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            max_output_bytes: MAX_RENDER_BYTES,
            max_line_length: MAX_RENDER_LINE_LENGTH,
            reserved_bytes: 0,
        }
    }
}

pub fn render_tagged_snapshot_with_options(
    snapshot: &Snapshot,
    requested_path: impl Into<String>,
    options: RenderOptions,
) -> TaggedRendering {
    let requested_path = requested_path.into();
    let mut text = format!("[{requested_path}#{}]\n", snapshot.tag.to_ascii_uppercase());
    let body_limit = read_body_limit(options, text.len(), snapshot.total_lines);
    let mut body_bytes = 0usize;
    let mut rendered_lines = BTreeSet::new();
    let mut display_truncated_lines = BTreeSet::new();
    let mut first_elided = None;
    let mut last_elided = None;

    for (&line_number, record) in &snapshot.records {
        let content = String::from_utf8_lossy(&record.content);
        let (display, was_truncated) = truncate_display_line(&content, options.max_line_length);
        let line = format!("{line_number}:{display}\n");
        if body_bytes.saturating_add(line.len()) > body_limit {
            first_elided.get_or_insert(line_number);
            last_elided = Some(line_number);
            continue;
        }
        body_bytes = body_bytes.saturating_add(line.len());
        text.push_str(&line);
        rendered_lines.insert(line_number);
        if was_truncated {
            display_truncated_lines.insert(line_number);
        }
    }

    let elided_range = first_elided.map(|start| {
        let end = last_elided.unwrap_or(start);
        let notice = elision_notice(options.max_output_bytes, start, end);
        text.push_str(&notice);
        LineRange::new(start, end)
    });

    TaggedRendering {
        text,
        requested_path,
        tag: snapshot.tag.to_ascii_uppercase(),
        seen_lines: rendered_lines.clone(),
        rendered_lines,
        elided_range,
        display_truncated_lines,
    }
}

/// Render retained records without the hashline carrier. This is used for the
/// legacy/gate-off branch and for declined or ineligible bash rewrites.
pub fn render_tagless_snapshot(
    snapshot: &Snapshot,
    requested_path: impl Into<String>,
) -> TaglessRendering {
    render_tagless_snapshot_with_options(snapshot, requested_path, RenderOptions::default())
}

fn render_tagless_snapshot_with_options(
    snapshot: &Snapshot,
    requested_path: impl Into<String>,
    options: RenderOptions,
) -> TaglessRendering {
    let requested_path = requested_path.into();
    let mut text = String::new();
    let body_limit = read_body_limit(options, 0, snapshot.total_lines);
    let mut body_bytes = 0usize;
    let mut rendered_lines = BTreeSet::new();
    let mut first_elided = None;
    let mut last_elided = None;
    for (&line_number, record) in &snapshot.records {
        let line = format!(
            "{line_number}: {}\n",
            String::from_utf8_lossy(&record.content)
        );
        if body_bytes.saturating_add(line.len()) > body_limit {
            first_elided.get_or_insert(line_number);
            last_elided = Some(line_number);
            continue;
        }
        body_bytes = body_bytes.saturating_add(line.len());
        text.push_str(&line);
        rendered_lines.insert(line_number);
    }
    let elided_range = first_elided.map(|start| {
        let end = last_elided.unwrap_or(start);
        text.push_str(&elision_notice(options.max_output_bytes, start, end));
        LineRange::new(start, end)
    });
    TaglessRendering {
        text,
        requested_path,
        rendered_lines,
        elided_range,
    }
}

fn elision_notice(limit: usize, start: usize, end: usize) -> String {
    format!(
        "... (output truncated at {}KB, use start_line/end_line to read sections; lines {start}-{end} are not addressable)\n",
        limit / 1024
    )
}

fn read_body_limit(options: RenderOptions, header_bytes: usize, total_lines: usize) -> usize {
    if options.reserved_bytes == 0 {
        return options.max_output_bytes;
    }
    // Decide elision before publishing seen-line evidence. Finalization may
    // shorten notices, but must never remove already-published hashline rows.
    options.max_output_bytes.saturating_sub(
        options.reserved_bytes
            + header_bytes
            + elision_notice(options.max_output_bytes, total_lines, total_lines).len(),
    )
}

fn truncate_display_line(content: &str, max_length: usize) -> (String, bool) {
    if content.chars().count() <= max_length {
        return (content.to_string(), false);
    }
    let truncated: String = content.chars().take(max_length).collect();
    (format!("{truncated}... (truncated)"), true)
}

/// Read a regular, writable UTF-8 file and publish the rows that the agent can
/// actually address. A line omitted by the 50 KiB output cap is removed from
/// the published snapshot, while display-truncated lines remain eligible.
pub fn capture_taggable_read(
    store: &mut SnapshotStore,
    canonical_path: impl AsRef<Path>,
    requested_path: impl Into<String>,
    selection: ReadSelection,
) -> io::Result<ReadPublication> {
    capture_taggable_read_with_options(
        store,
        canonical_path,
        requested_path,
        selection,
        RenderOptions::default(),
    )
}

pub fn capture_taggable_read_with_options(
    store: &mut SnapshotStore,
    canonical_path: impl AsRef<Path>,
    requested_path: impl Into<String>,
    selection: ReadSelection,
    options: RenderOptions,
) -> io::Result<ReadPublication> {
    let canonical_path = canonical_path.as_ref();
    let source = read_source(canonical_path)?;
    capture_taggable_source_with_options(
        store,
        canonical_path,
        requested_path,
        selection,
        options,
        &source,
    )
}

/// Bytes and permissions observed through the same open file. Metadata is kept
/// private so callers cannot accidentally pair earlier bytes with a later stat.
pub(crate) struct ReadSource {
    bytes: Vec<u8>,
    metadata: fs::Metadata,
}

impl ReadSource {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub(crate) fn has_complete_bytes(&self) -> bool {
        self.metadata.is_file()
            && self.metadata.len() <= MAX_FILE_READ_BYTES
            && self.bytes.len() as u64 == self.metadata.len()
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static SOURCE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static SOURCE_AFTER_READ: std::cell::RefCell<Option<Box<dyn FnOnce() + Send>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct HelperStateScope(usize, Option<Box<dyn FnOnce() + Send>>);
#[cfg(test)]
impl Drop for HelperStateScope {
    fn drop(&mut self) {
        SOURCE_READS.with(|reads| reads.set(self.0));
        SOURCE_AFTER_READ.with(|hook| *hook.borrow_mut() = self.1.take());
    }
}
#[cfg(test)]
pub(crate) fn helper_state_scope() -> HelperStateScope {
    HelperStateScope(
        SOURCE_READS.with(|reads| reads.replace(0)),
        SOURCE_AFTER_READ.with(|hook| hook.replace(None)),
    )
}

pub(crate) fn read_source(path: &Path) -> io::Result<ReadSource> {
    #[cfg(test)]
    let hook = SOURCE_AFTER_READ.with(|hook| hook.borrow_mut().take());
    #[cfg(test)]
    let caller_thread = std::thread::current().id();
    let (result, _reads) = crate::bounded_io::run(path, None, move |path| {
        #[cfg(test)]
        SOURCE_AFTER_READ.with(|slot| *slot.borrow_mut() = hook);
        let result = read_source_unbounded(&path);
        #[cfg(test)]
        let reads = if std::thread::current().id() == caller_thread {
            // Nested probes can reuse their caller's bounded helper. Its TLS
            // counter is already updated; transfer only from another thread.
            0
        } else {
            SOURCE_READS.with(|reads| reads.get())
        };
        #[cfg(not(test))]
        let reads = 0usize;
        Ok((result, reads))
    })?;
    #[cfg(test)]
    SOURCE_READS.with(|reads| reads.set(reads.get() + _reads));
    result
}

fn read_source_unbounded(path: &Path) -> io::Result<ReadSource> {
    let mut file = match crate::task_temp_reads::open(path) {
        Ok(file) => file,
        Err(error) => {
            // Windows cannot open a directory as a regular File. Preserve the
            // disk capture's explicit non-regular-file refusal in that case.
            let metadata = fs::metadata(path)?;
            if !metadata.is_file() {
                return Ok(ReadSource {
                    bytes: Vec::new(),
                    metadata,
                });
            }
            return Err(error);
        }
    };
    let metadata = file.metadata()?;
    let mut bytes = Vec::new();
    if metadata.is_file() && metadata.len() <= MAX_FILE_READ_BYTES {
        #[cfg(test)]
        SOURCE_READS.with(|reads| reads.set(reads.get() + 1));
        file.read_to_end(&mut bytes)?;
        #[cfg(test)]
        SOURCE_AFTER_READ.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        let after = file.metadata()?;
        if !same_read_identity(&metadata, &after) || bytes.len() as u64 != metadata.len() {
            return Err(io::Error::other("file changed during hashline read"));
        }
    }
    Ok(ReadSource { bytes, metadata })
}

fn same_read_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.mode() != after.mode()
        {
            return false;
        }
    }
    before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
        && before.permissions().readonly() == after.permissions().readonly()
}

/// Apply the disk capture's complete refusal and publication rules to bytes
/// already obtained with `read_source`, without opening the file again.
pub(crate) fn capture_taggable_source_with_options(
    store: &mut SnapshotStore,
    canonical_path: &Path,
    requested_path: impl Into<String>,
    selection: ReadSelection,
    options: RenderOptions,
    source: &ReadSource,
) -> io::Result<ReadPublication> {
    let requested_path = requested_path.into();
    let metadata = &source.metadata;
    if !metadata.is_file() {
        return Ok(ReadPublication::Tagless {
            rendering: TaglessRendering {
                text: String::new(),
                requested_path,
                rendered_lines: BTreeSet::new(),
                elided_range: None,
            },
            reason: UntaggableReason::NotRegularFile,
        });
    }
    let write_eligible = is_write_eligible(metadata);
    if metadata.len() > MAX_FILE_READ_BYTES {
        return Ok(ReadPublication::Tagless {
            rendering: TaglessRendering {
                text: String::new(),
                requested_path,
                rendered_lines: BTreeSet::new(),
                elided_range: None,
            },
            reason: UntaggableReason::Oversize {
                bytes: metadata.len(),
                limit: MAX_FILE_READ_BYTES,
            },
        });
    }

    let bytes = source.bytes();
    if is_binary(bytes) {
        return Ok(ReadPublication::Tagless {
            rendering: TaglessRendering {
                text: String::new(),
                requested_path,
                rendered_lines: BTreeSet::new(),
                elided_range: None,
            },
            reason: UntaggableReason::Binary,
        });
    }
    if std::str::from_utf8(bytes).is_err() {
        return Ok(ReadPublication::Tagless {
            rendering: TaglessRendering {
                text: String::new(),
                requested_path,
                rendered_lines: BTreeSet::new(),
                elided_range: None,
            },
            reason: UntaggableReason::InvalidUtf8,
        });
    }

    let source_snapshot = scan_bytes_with_request(bytes, selection.scan_request())
        .snapshot
        .expect("in-memory scans always observe EOF");
    let selected = selection.selected_lines(source_snapshot.total_lines);
    if !write_eligible {
        let tagless_snapshot = snapshot_for_lines(&source_snapshot, &selected);
        return Ok(ReadPublication::Tagless {
            rendering: render_tagless_snapshot_with_options(
                &tagless_snapshot,
                requested_path,
                options,
            ),
            reason: UntaggableReason::ReadOnly,
        });
    }
    if selection.is_explicitly_empty()
        || (selected.is_empty() && !matches!(selection, ReadSelection::WholeFile))
    {
        let tagless_snapshot = snapshot_for_lines(&source_snapshot, &selected);
        return Ok(ReadPublication::Tagless {
            rendering: render_tagless_snapshot_with_options(
                &tagless_snapshot,
                requested_path.clone(),
                options,
            ),
            reason: if bytes.is_empty() {
                UntaggableReason::EmptyRange
            } else {
                UntaggableReason::BeyondEof
            },
        });
    }

    let selected_snapshot = snapshot_for_lines(&source_snapshot, &selected);
    let candidate_rendering =
        render_tagged_snapshot_with_options(&selected_snapshot, requested_path.clone(), options);
    let published_snapshot =
        snapshot_for_lines(&selected_snapshot, &candidate_rendering.rendered_lines);
    let outcome = store.publish(canonical_path, published_snapshot);
    if let PublishStatus::Oversize {
        retained_bytes,
        limit,
    } = outcome.status
    {
        return Ok(ReadPublication::Tagless {
            rendering: render_tagless_snapshot_with_options(
                &selected_snapshot,
                requested_path,
                options,
            ),
            reason: UntaggableReason::Oversize {
                bytes: retained_bytes as u64,
                limit: limit as u64,
            },
        });
    }
    // Keep the elision notice from the pre-publication render.  The published
    // snapshot intentionally contains only rendered rows, so rendering it a
    // second time would lose the information that the tail of the requested
    // domain was scanned but left unseen.
    let published_snapshot = outcome
        .snapshot
        .expect("a non-oversize publication exposes its accepted snapshot");
    Ok(ReadPublication::Tagged {
        snapshot: published_snapshot,
        rendering: candidate_rendering,
    })
}

/// Apply the same capture rules to an accepted cat/head/tail rewrite. The
/// funnel and experimental gate are checked before this function is allowed to
/// publish, so declined rewrites remain store-neutral.
pub fn capture_bash_rewrite_read(
    store: &mut SnapshotStore,
    canonical_path: impl AsRef<Path>,
    requested_path: impl Into<String>,
    kind: BashReadKind,
    experimental_bash_rewrite: bool,
    funnel_accepted: bool,
    effective_hashline: bool,
) -> io::Result<ReadPublication> {
    capture_bash_rewrite_read_with_options(
        store,
        canonical_path,
        requested_path,
        kind,
        experimental_bash_rewrite,
        funnel_accepted,
        effective_hashline,
        RenderOptions::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn capture_bash_rewrite_read_with_options(
    store: &mut SnapshotStore,
    canonical_path: impl AsRef<Path>,
    requested_path: impl Into<String>,
    kind: BashReadKind,
    experimental_bash_rewrite: bool,
    funnel_accepted: bool,
    effective_hashline: bool,
    options: RenderOptions,
) -> io::Result<ReadPublication> {
    if !(experimental_bash_rewrite && funnel_accepted && effective_hashline) {
        return capture_tagless_read_with_options(
            canonical_path,
            requested_path,
            kind.selection(),
            options,
        );
    }
    capture_taggable_read_with_options(
        store,
        canonical_path,
        requested_path,
        kind.selection(),
        options,
    )
}

/// Capture a bash read without publication. This path intentionally does not
/// touch the store, even when the command shape is valid but the gate is off.
pub fn capture_tagless_read(
    canonical_path: impl AsRef<Path>,
    requested_path: impl Into<String>,
    selection: ReadSelection,
) -> io::Result<ReadPublication> {
    capture_tagless_read_with_options(
        canonical_path,
        requested_path,
        selection,
        RenderOptions::default(),
    )
}

fn capture_tagless_read_with_options(
    canonical_path: impl AsRef<Path>,
    requested_path: impl Into<String>,
    selection: ReadSelection,
    options: RenderOptions,
) -> io::Result<ReadPublication> {
    let canonical_path = canonical_path.as_ref();
    let requested_path = requested_path.into();
    let bytes = fs::read(canonical_path)?;
    let snapshot = scan_bytes_with_request(&bytes, selection.scan_request())
        .snapshot
        .expect("in-memory scans always observe EOF");
    let selected = selection.selected_lines(snapshot.total_lines);
    let snapshot = snapshot_for_lines(&snapshot, &selected);
    Ok(ReadPublication::Tagless {
        rendering: render_tagless_snapshot_with_options(&snapshot, requested_path, options),
        reason: UntaggableReason::VirtualPath,
    })
}

/// Publish an affected-region snapshot from authoritative final bytes. This is
/// deliberately separate from a read capture: the edit response owns which
/// current rows are relevant, not the caller's original read range.
pub fn publish_edit_response_snapshot(
    store: &mut SnapshotStore,
    canonical_path: impl AsRef<Path>,
    requested_path: impl Into<String>,
    final_bytes: &[u8],
    affected: &AffectedRegion,
) -> EditResponseSnapshot {
    let canonical_path = canonical_path.as_ref();
    let requested_path = requested_path.into();
    if final_bytes.len() as u64 > MAX_FILE_READ_BYTES || is_binary(final_bytes) {
        return EditResponseSnapshot::unavailable(
            requested_path,
            "final bytes are not a readable, taggable text file",
        );
    }
    if std::str::from_utf8(final_bytes).is_err() {
        return EditResponseSnapshot::unavailable(
            requested_path,
            "final bytes are not valid UTF-8",
        );
    }
    let whole = scan_bytes_with_request(final_bytes, ScanRequest::whole_file())
        .snapshot
        .expect("in-memory scans always observe EOF");
    let selected = affected_output_lines(&whole, affected);
    let selected_snapshot = snapshot_for_lines(&whole, &selected);
    let outcome = store.publish(canonical_path, selected_snapshot);
    if outcome.oversize() {
        return EditResponseSnapshot::unavailable(
            requested_path,
            "affected snapshot exceeds the session residency budget",
        );
    }
    let selected_snapshot = outcome
        .snapshot
        .expect("a non-oversize publication exposes its accepted snapshot");
    let rendering = render_tagged_snapshot(&selected_snapshot, requested_path.clone());
    EditResponseSnapshot {
        snapshot: Some(selected_snapshot),
        rendering: Some(rendering),
        requested_path,
        notice: None,
    }
}

/// A fresh post-write carrier, or an explicit notice when the final state is
/// not safe to chain from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditResponseSnapshot {
    pub snapshot: Option<Snapshot>,
    pub rendering: Option<TaggedRendering>,
    pub requested_path: String,
    pub notice: Option<String>,
}

impl EditResponseSnapshot {
    pub fn unavailable(requested_path: String, reason: &str) -> Self {
        Self {
            snapshot: None,
            rendering: None,
            requested_path,
            notice: Some(format!(
                "No hashline tag is available for the final file; re-read before chaining ({reason})."
            )),
        }
    }

    pub fn tag(&self) -> Option<&str> {
        self.rendering
            .as_ref()
            .map(|rendering| rendering.tag.as_str())
    }
}

/// A removed MV source has no final state to mint. This invalidates the source
/// without turning the handle into an evicted handle.
pub fn invalidate_removed_source(store: &mut SnapshotStore, source: impl AsRef<Path>) -> bool {
    store.invalidate_path(source)
}

fn snapshot_for_lines(snapshot: &Snapshot, lines: &BTreeSet<usize>) -> Snapshot {
    let records: BTreeMap<usize, RawLineRecord> = snapshot
        .records
        .iter()
        .filter_map(|(&line, record)| lines.contains(&line).then_some((line, record.clone())))
        .collect();
    let retained_lines = snapshot
        .retained_lines
        .iter()
        .filter_map(|(&line, record)| lines.contains(&line).then_some((line, record.clone())))
        .collect();
    let mut coverage = snapshot.coverage.clone();
    coverage.retained_lines = lines.clone();
    coverage.seen_lines = lines.clone();
    let boundary = BoundaryEvidence {
        empty_file: snapshot.boundary.empty_file,
        bof_observed: snapshot.boundary.bof_observed,
        eof_observed: snapshot.boundary.eof_observed,
        first_seen: lines.iter().next().copied(),
        last_seen: lines.iter().next_back().copied(),
    };
    Snapshot {
        tag: snapshot.tag.clone(),
        normalized_bytes: snapshot.normalized_bytes.clone(),
        records,
        retained_lines,
        coverage,
        boundary,
        total_lines: snapshot.total_lines,
        byte_count: snapshot.byte_count,
        provenance: snapshot.provenance.clone(),
        capture_provenance: snapshot.capture_provenance.clone(),
    }
}

fn affected_output_lines(snapshot: &Snapshot, affected: &AffectedRegion) -> BTreeSet<usize> {
    let total = snapshot.total_lines;
    if total == 0 {
        return BTreeSet::new();
    }
    let ranges = coalesce_ranges(affected.ranges.iter().copied());
    let mut selected = BTreeSet::new();
    for range in ranges {
        let start = range.start.max(1);
        let end = range.end.min(total);
        if start <= end {
            selected.extend(start..=end);
        }
        if start > 1 {
            selected.insert(start - 1);
        }
        if end < total {
            selected.insert(end.saturating_add(1));
        }
    }
    selected.retain(|line| snapshot.records.contains_key(line));
    selected
}

fn coalesce_ranges<I>(ranges: I) -> Vec<LineRange>
where
    I: IntoIterator<Item = LineRange>,
{
    let mut ranges: Vec<LineRange> = ranges
        .into_iter()
        .filter(|range| !range.is_empty())
        .collect();
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut result: Vec<LineRange> = Vec::new();
    for range in ranges {
        if let Some(last) = result.last_mut() {
            if range.start <= last.end.saturating_add(1) {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        result.push(range);
    }
    result
}

fn canonical_key(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn fold_tag(tag: &str) -> String {
    tag.to_ascii_lowercase()
}

fn is_binary(bytes: &[u8]) -> bool {
    !bytes.is_empty() && content_inspector::inspect(bytes).is_binary()
}

fn is_write_eligible(metadata: &fs::Metadata) -> bool {
    if metadata.permissions().readonly() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return metadata.permissions().mode() & 0o222 != 0;
    }
    #[cfg(not(unix))]
    {
        true
    }
}

impl From<CaptureError> for UntaggableReason {
    fn from(error: CaptureError) -> Self {
        Self::Io(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    #[test]
    fn read_source_rejects_identity_changes_during_the_read() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("changing.txt");
        fs::write(&path, b"old\n").unwrap();
        let changed = path.clone();
        SOURCE_AFTER_READ.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(changed, b"a new and longer version\n").unwrap();
            }))
        });
        assert!(
            read_source(&path).is_err(),
            "never combine earlier bytes with a later file stamp"
        );
    }

    fn snapshot(bytes: &[u8], lines: impl IntoIterator<Item = usize>) -> Snapshot {
        scan_bytes_with_request(bytes, ScanRequest::new(CoverageInput::lines(lines)))
            .snapshot
            .expect("in-memory snapshot")
    }

    fn snapshot_with_forced_tag(bytes: &[u8], tag: &str) -> Snapshot {
        let mut snapshot = snapshot(bytes, [1]);
        snapshot.tag = tag.to_string();
        snapshot
    }

    fn writable_fixture(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, bytes).expect("fixture write");
        path
    }

    /// Publishing, re-reading and resolving a file whose path and tag have
    /// no eviction history never hashes its content: the fingerprint only
    /// matters when it is compared with a history entry.
    #[test]
    fn publishing_and_resolving_without_history_hashes_no_content() {
        let mut store = SnapshotStore::new();
        let path = PathBuf::from("/tmp/fingerprint-free.txt");
        let content = b"line one\nline two\n".repeat(512);
        let first = snapshot(&content, [1]);
        let tag = first.tag.clone();
        CONTENT_FINGERPRINTS.with(|count| count.set(0));
        store.publish(&path, first);
        // The same content read again coalesces into the first version.
        store.publish(&path, snapshot(&content, [2]));
        store.publish(&path, snapshot(b"other content\n", [1]));
        assert!(store.lookup(&path, &tag).is_ok());
        assert_eq!(store.snapshot_count(), 2);
        assert_eq!(CONTENT_FINGERPRINTS.with(|count| count.get()), 0);
    }

    #[test]
    fn equivalent_re_reads_collapse_even_with_different_provenance() {
        let mut left = snapshot(b"one\ntwo\n", [1, 2]);
        let mut right = left.clone();
        right.provenance = right.provenance.with_label("capture", "second");
        right.capture_provenance = right
            .capture_provenance
            .with_label("descriptor", "different");
        assert!(equivalent_snapshots(&left, &right));
        left.records.get_mut(&1).unwrap().content = b"changed".to_vec();
        assert!(!equivalent_snapshots(&left, &right));
    }

    #[test]
    fn genuine_folded_tag_collision_stays_distinct_and_preserves_evicted_history() {
        const COLLIDING_TAG: &str = "C0DE";
        let mut store = SnapshotStore::new();
        let resident_path = PathBuf::from("/tmp/genuine-collision-resident.txt");

        store.publish(
            &resident_path,
            snapshot_with_forced_tag(b"first collision content\n", COLLIDING_TAG),
        );
        store.publish(
            &resident_path,
            snapshot_with_forced_tag(b"second collision content\n", &COLLIDING_TAG.to_lowercase()),
        );

        assert_eq!(
            store.snapshot_count(),
            2,
            "colliding content must not coalesce"
        );
        assert_eq!(
            store.lookup(&resident_path, COLLIDING_TAG),
            Err(SnapshotLookupError::AmbiguousTag)
        );
        assert!(SnapshotLookupError::AmbiguousTag
            .steering()
            .contains("apply_patch"));

        let evicted_path = PathBuf::from("/tmp/genuine-collision-evicted.txt");
        store.publish(
            &evicted_path,
            snapshot_with_forced_tag(b"evicted collision content\n", COLLIDING_TAG),
        );
        for index in 0..MAX_VERSIONS_PER_PATH {
            store.publish(
                &evicted_path,
                snapshot_with_forced_tag(
                    format!("filler content {index}\n").as_bytes(),
                    &format!("F{index:03X}"),
                ),
            );
        }
        assert_eq!(
            store.lookup(&evicted_path, COLLIDING_TAG),
            Err(SnapshotLookupError::EvictedTag),
            "the first colliding content must have entered eviction history"
        );

        store.publish(
            &evicted_path,
            snapshot_with_forced_tag(b"replacement collision content\n", COLLIDING_TAG),
        );

        assert!(store.eviction_history_contains(&evicted_path, COLLIDING_TAG));
        assert_eq!(
            store.lookup(&evicted_path, COLLIDING_TAG),
            Err(SnapshotLookupError::EvictedTag),
            "publishing different colliding content must not erase the prior eviction"
        );
    }

    #[test]
    fn coalescing_uses_normalized_content_not_retained_window_equality() {
        let mut store = SnapshotStore::new();
        let path = PathBuf::from("/tmp/normalized-content.txt");
        let first = snapshot(b"one \ntwo\n", [1]);
        let second = snapshot(b"one\t\ntwo\n", [2]);
        let tag = first.tag.clone();

        store.publish(&path, first);
        store.publish(&path, second);

        assert_eq!(store.snapshot_count(), 1);
        let resolved = store
            .lookup(&path, &tag)
            .expect("normalized content matches");
        assert_eq!(resolved.coverage.seen_lines, BTreeSet::from([1, 2]));
    }

    #[test]
    fn tagged_rendering_keeps_absolute_numbers_and_display_truncation_seen() {
        let long = "x".repeat(MAX_RENDER_LINE_LENGTH + 20);
        let snapshot = snapshot(format!("short\n{long}\nlast\n").as_bytes(), [1, 2, 3]);
        let rendered = render_tagged_snapshot(&snapshot, "agent/path.txt");
        assert!(rendered.text.starts_with("[agent/path.txt#"));
        assert!(rendered.text.contains("1:short\n"));
        assert!(rendered.text.contains("2:"));
        assert!(rendered.text.contains("... (truncated)"));
        assert_eq!(rendered.rendered_lines, BTreeSet::from([1, 2, 3]));
        assert!(rendered.display_truncated_lines.contains(&2));
    }

    #[test]
    fn output_elision_removes_unrendered_rows_from_published_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = (1..=20)
            .map(|line| format!("{line}:{}\n", "x".repeat(20)))
            .collect::<String>();
        let path = writable_fixture(temp.path(), "large.txt", bytes.as_bytes());
        let mut store = SnapshotStore::new();
        let publication = capture_taggable_read_with_options(
            &mut store,
            &path,
            "large.txt",
            ReadSelection::WholeFile,
            RenderOptions {
                max_output_bytes: 80,
                max_line_length: MAX_RENDER_LINE_LENGTH,
                reserved_bytes: 0,
            },
        )
        .unwrap();
        let ReadPublication::Tagged {
            snapshot,
            rendering,
        } = publication
        else {
            panic!("expected tagged publication");
        };
        assert!(rendering.elided_range.is_some());
        assert_eq!(snapshot.coverage.seen_lines, rendering.rendered_lines);
        assert!(!snapshot.coverage.is_seen(20));
        assert!(snapshot.coverage.is_seen(1));
        assert!(store.contains(&path, &snapshot.tag));
    }

    #[test]
    fn ranged_and_bash_tail_publications_have_only_seen_rows() {
        let temp = tempfile::tempdir().unwrap();
        let path = writable_fixture(temp.path(), "tail.txt", b"one\ntwo\nthree\nfour\n");
        let mut store = SnapshotStore::new();
        let publication =
            capture_taggable_read(&mut store, &path, "tail.txt", ReadSelection::range(2, 3))
                .unwrap();
        let ReadPublication::Tagged { snapshot, .. } = publication else {
            panic!("expected tagged range");
        };
        assert_eq!(snapshot.coverage.seen_lines, BTreeSet::from([2, 3]));
        let publication = capture_bash_rewrite_read(
            &mut store,
            &path,
            "tail.txt",
            BashReadKind::Tail { lines: 2 },
            true,
            true,
            true,
        )
        .unwrap();
        let ReadPublication::Tagged { snapshot, .. } = publication else {
            panic!("expected tagged tail");
        };
        assert_eq!(snapshot.coverage.seen_lines, BTreeSet::from([3, 4]));
        assert!(snapshot.eof_observed());
    }

    #[test]
    fn empty_or_beyond_eof_ranges_do_not_mint_empty_file_tags() {
        let temp = tempfile::tempdir().unwrap();
        let empty = writable_fixture(temp.path(), "empty.txt", b"");
        let one_line = writable_fixture(temp.path(), "one-line.txt", b"one\n");
        let mut store = SnapshotStore::new();
        let empty_result =
            capture_taggable_read(&mut store, &empty, "empty.txt", ReadSelection::range(1, 1))
                .unwrap();
        assert!(matches!(empty_result, ReadPublication::Tagless { .. }));
        let beyond_result = capture_taggable_read(
            &mut store,
            &one_line,
            "one-line.txt",
            ReadSelection::range(2, 2),
        )
        .unwrap();
        assert!(matches!(beyond_result, ReadPublication::Tagless { .. }));
        assert_eq!(store.snapshot_count(), 0);
    }

    #[test]
    fn declined_bash_rewrite_is_store_neutral() {
        let temp = tempfile::tempdir().unwrap();
        let path = writable_fixture(temp.path(), "cat.txt", b"one\ntwo\n");
        let mut store = SnapshotStore::new();
        let before = store.clone();
        let publication = capture_bash_rewrite_read(
            &mut store,
            &path,
            "cat.txt",
            BashReadKind::Cat,
            false,
            true,
            true,
        )
        .unwrap();
        assert!(matches!(publication, ReadPublication::Tagless { .. }));
        assert_eq!(store.snapshot_count(), before.snapshot_count());
        assert_eq!(store.eviction_history_len(), before.eviction_history_len());
    }

    #[test]
    fn edit_response_renders_changed_rows_and_surviving_neighbors() {
        let mut store = SnapshotStore::new();
        let result = publish_edit_response_snapshot(
            &mut store,
            "/virtual/edit.txt",
            "edit.txt",
            b"a\ninserted\nc\nd\n",
            &AffectedRegion::from_range(2, 2),
        );
        let rendering = result.rendering.as_ref().unwrap();
        assert!(rendering.text.contains("1:a\n"));
        assert!(rendering.text.contains("2:inserted\n"));
        assert!(rendering.text.contains("3:c\n"));
        assert!(!rendering.text.contains("4:d\n"));
        assert!(result.tag().is_some());
    }

    #[test]
    fn edit_response_empty_file_has_boundary_evidence_without_rows() {
        let mut store = SnapshotStore::new();
        let result = publish_edit_response_snapshot(
            &mut store,
            "/virtual/empty.txt",
            "empty.txt",
            b"",
            &AffectedRegion::deletion(1, 4),
        );
        let snapshot = result.snapshot.unwrap();
        assert!(snapshot.records.is_empty());
        assert!(snapshot.boundary.empty_file);
        assert!(result.rendering.unwrap().text.starts_with("[empty.txt#"));
    }

    #[test]
    fn path_and_version_limits_evict_deterministically() {
        let mut store = SnapshotStore::new();
        for path_number in 0..=MAX_SNAPSHOT_PATHS {
            let path = PathBuf::from(format!("/tmp/hashline-{path_number}.txt"));
            let result = store.publish(&path, snapshot(format!("{path_number}\n").as_bytes(), [1]));
            assert!(result.stored());
        }
        assert_eq!(store.path_count(), MAX_SNAPSHOT_PATHS);
        assert!(matches!(
            store.lookup("/tmp/hashline-0.txt", "0000"),
            Err(SnapshotLookupError::UnknownTag | SnapshotLookupError::EvictedTag)
        ));

        let path = PathBuf::from("/tmp/versions.txt");
        let mut tags = Vec::new();
        for value in 0..=MAX_VERSIONS_PER_PATH {
            let current = snapshot(format!("version-{value}\n").as_bytes(), [1]);
            tags.push(current.tag.clone());
            store.publish(&path, current);
        }
        assert_eq!(
            store.lookup(&path, &tags[0]),
            Err(SnapshotLookupError::EvictedTag)
        );
        assert!(store.lookup(&path, &tags[1]).is_ok());
    }

    #[test]
    fn same_content_publications_coalesce_before_version_eviction() {
        let mut store = SnapshotStore::new();
        let path = PathBuf::from("/tmp/coalesced-version.txt");
        let bytes = b"one\ntwo\nthree\nfour\nfive\nsix\n";
        let mut tag = None;

        for line in 1..=(MAX_VERSIONS_PER_PATH + 2) {
            let current = snapshot(bytes, [line]);
            tag.get_or_insert(current.tag.clone());
            let outcome = store.publish(&path, current);
            assert!(outcome.stored());
            assert!(outcome.evicted.is_empty());
        }

        assert_eq!(store.snapshot_count(), 1);
        assert_eq!(store.eviction_history_len(), 0);
        let resolved = store
            .lookup(&path, tag.as_deref().unwrap())
            .expect("coalesced version remains resident");
        assert_eq!(
            resolved.coverage.seen_lines,
            BTreeSet::from([1, 2, 3, 4, 5, 6])
        );
    }

    #[test]
    fn full_reread_refreshes_coalesced_version_age() {
        let mut store = SnapshotStore::new();
        let path = PathBuf::from("/tmp/coalesced-reread.py");
        let original = (1..=130)
            .map(|line| format!("line_{line} = {line}\n"))
            .collect::<String>();
        let mut first = snapshot(original.as_bytes(), [1, 2]);
        first.tag = "A001".into();
        let first_tag = first.tag.clone();
        store.publish(&path, first);

        let mut older_tags = Vec::new();
        for version in 2..=MAX_VERSIONS_PER_PATH {
            let bytes = format!("version_{version} = {version}\n");
            let tag = format!("A{version:03}");
            older_tags.push(tag.clone());
            store.publish(&path, snapshot_with_forced_tag(bytes.as_bytes(), &tag));
        }

        let mut reread = snapshot(original.as_bytes(), 1..=130);
        reread.tag = first_tag.clone();
        store.publish(&path, reread);
        store.publish(&path, snapshot_with_forced_tag(b"newest = 5\n", "A005"));

        let retained = store
            .lookup(&path, &first_tag)
            .expect("the fresh full reread must survive the next version publication");
        assert!(retained.is_seen(16));
        assert_eq!(
            store.lookup(&path, &older_tags[0]),
            Err(SnapshotLookupError::EvictedTag),
            "the least recently published version should be evicted instead"
        );
    }

    #[test]
    fn overflowing_eviction_history_transitions_evicted_to_unknown() {
        let mut store = SnapshotStore::new();
        let mut handles = Vec::new();
        // Each path is filled to its version bound, then the path is displaced.
        // This produces more than MAX_EVICTION_RECORDS distinct history keys.
        for path_number in 0..(MAX_EVICTION_RECORDS + MAX_SNAPSHOT_PATHS + 4) {
            let path = PathBuf::from(format!("/tmp/history-{path_number}.txt"));
            let current = snapshot(format!("history-{path_number}\n").as_bytes(), [1]);
            handles.push((path.clone(), current.tag.clone()));
            store.publish(path, current);
        }
        let first = &handles[0];
        assert_eq!(store.eviction_history_len(), MAX_EVICTION_RECORDS);
        assert!(matches!(
            store.lookup(&first.0, &first.1),
            Err(SnapshotLookupError::UnknownTag)
        ));
        let retained = &handles[handles.len() - MAX_SNAPSHOT_PATHS - 1];
        assert!(matches!(
            store.lookup(&retained.0, &retained.1),
            Err(SnapshotLookupError::EvictedTag)
        ));
        assert_eq!(
            SnapshotLookupError::EvictedTag.steering(),
            SnapshotLookupError::UnknownTag.steering()
        );
    }

    #[test]
    fn oversize_publish_does_not_perturb_store_or_history() {
        let mut store = SnapshotStore::new();
        let path = PathBuf::from("/tmp/resident.txt");
        let resident = snapshot(b"resident\n", [1]);
        store.publish(&path, resident.clone());
        let before = store.clone();
        let mut oversize = resident;
        oversize.byte_count = MAX_FILE_READ_BYTES + 1;
        let outcome = store.publish("/tmp/oversize.txt", oversize);
        assert!(outcome.oversize());
        assert_eq!(store.snapshot_count(), before.snapshot_count());
        assert_eq!(store.path_count(), before.path_count());
        assert_eq!(store.total_bytes(), before.total_bytes());
        assert_eq!(store.eviction_history_len(), before.eviction_history_len());
    }

    #[test]
    fn case_insensitive_lookup_and_invalidation_are_path_scoped() {
        let mut store = SnapshotStore::new();
        let path = PathBuf::from("/tmp/scoped.txt");
        let current = snapshot(b"scoped\n", [1]);
        let tag = current.tag.clone();
        store.publish(&path, current);
        assert!(store.lookup(&path, &tag.to_ascii_lowercase()).is_ok());
        assert!(store.invalidate_path(&path));
        assert_eq!(
            store.lookup(&path, &tag),
            Err(SnapshotLookupError::UnknownTag)
        );
        assert_eq!(store.eviction_history_len(), 0);
    }
}
