//! Canonical trigram payloads and segments for per-checkout views.
//!
//! A trigram **payload** is one file's postings under one policy: a flag byte
//! (`indexed`, `unindexed_binary`, `unindexed_oversize`) and the sorted,
//! distinct `(trigram, next_mask, loc_mask)` records of its bytes, with the
//! file id implicit. It is stored in the family's trigram store under
//! `TrigramKey(content, policy)`.
//!
//! A **segment** is the immutable postings for a set of `(rel_path, content)`
//! files, named by the BLAKE3 of its file table and policy. Its bytes are
//! canonical: the same member set produces the same bytes whether it is built
//! by reading the files or from stored payloads, so any builder of a segment
//! writes identical bytes and a second rename of the same name replaces an
//! equal file.
//!
//! Segment files are written temp + fsync + rename under their content-derived
//! name. The segment's row is committed, and the segment is listed in the
//! builder's live pin, before the file is written, so a sweep never meets a
//! segment that nothing accounts for.
//!
//! Layout (all integers big-endian):
//!
//! ```text
//! magic "AFTSEG02" | format u32 | policy fingerprint [32]
//! file_count u32 | trigram_count u32 | posting_count u64
//! file table:   (path_len u32, path bytes, content [32], size u64, flag u8) * file_count
//! lookup table: (trigram u32, first_posting u64, posting_count u32) * trigram_count   (16 bytes each)
//! postings:     (file_id u32, next_mask u8, loc_mask u8) * posting_count            (6 bytes each)
//! footer:       BLAKE3 of everything above [32]
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::blob_store::v2::{
    segment_path, ContentHash, FamilyKey, FamilyPlane, FamilyStore, FamilyStoreReader, StoreError,
    TrigramKey, TrigramPolicy,
};

use super::contracts::{observe, DurabilityObserver, DurabilityStep};
use super::RelPath;

pub const SEGMENT_MAGIC: &[u8; 8] = b"AFTSEG02";
pub const SEGMENT_FORMAT: u32 = 1;
pub const LOOKUP_ENTRY_BYTES: usize = 16;
pub const POSTING_BYTES: usize = 6;

#[derive(Debug)]
pub enum SegmentError {
    Io(std::io::Error),
    Store(StoreError),
    Malformed(String),
    MissingPayload { rel_path: RelPath, key: FamilyKey },
    DuplicatePath(RelPath),
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "segment I/O error: {error}"),
            Self::Store(error) => write!(f, "{error}"),
            Self::Malformed(message) => write!(f, "malformed trigram data: {message}"),
            Self::MissingPayload { rel_path, key } => write!(
                f,
                "no stored trigram payload {key} for {}",
                String::from_utf8_lossy(rel_path.as_bytes())
            ),
            Self::DuplicatePath(rel_path) => write!(
                f,
                "segment member listed twice: {}",
                String::from_utf8_lossy(rel_path.as_bytes())
            ),
        }
    }
}

impl std::error::Error for SegmentError {}

impl From<std::io::Error> for SegmentError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<StoreError> for SegmentError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

pub type SegmentResult<T> = Result<T, SegmentError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TrigramFlag {
    Indexed,
    UnindexedBinary,
    UnindexedOversize,
}

impl TrigramFlag {
    const fn byte(self) -> u8 {
        match self {
            Self::Indexed => 0,
            Self::UnindexedBinary => 1,
            Self::UnindexedOversize => 2,
        }
    }

    fn from_byte(byte: u8) -> SegmentResult<Self> {
        match byte {
            0 => Ok(Self::Indexed),
            1 => Ok(Self::UnindexedBinary),
            2 => Ok(Self::UnindexedOversize),
            other => Err(SegmentError::Malformed(format!("unknown flag {other}"))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TrigramRecord {
    pub trigram: u32,
    pub next_mask: u8,
    pub loc_mask: u8,
}

/// One file's trigram payload under one policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrigramPayload {
    pub flag: TrigramFlag,
    pub records: Vec<TrigramRecord>,
}

impl TrigramPayload {
    /// Extracts the payload from a file's bytes. It is a pure function of the
    /// bytes and the policy, which is what lets the key omit the path. The
    /// records are the search index's own posting fold of the bytes.
    pub fn extract(bytes: &[u8], policy: &TrigramPolicy) -> Self {
        if bytes.len() as u64 > policy.max_file_size {
            return Self {
                flag: TrigramFlag::UnindexedOversize,
                records: Vec::new(),
            };
        }
        if crate::search_index::is_binary_bytes(bytes) {
            return Self {
                flag: TrigramFlag::UnindexedBinary,
                records: Vec::new(),
            };
        }
        Self {
            flag: TrigramFlag::Indexed,
            records: crate::search_index::file_posting_fold(bytes)
                .map(|(trigram, next_mask, loc_mask)| TrigramRecord {
                    trigram,
                    next_mask,
                    loc_mask,
                })
                .collect(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(5 + self.records.len() * POSTING_BYTES);
        bytes.push(self.flag.byte());
        bytes.extend_from_slice(&(self.records.len() as u32).to_be_bytes());
        for record in &self.records {
            bytes.extend_from_slice(&record.trigram.to_be_bytes());
            bytes.push(record.next_mask);
            bytes.push(record.loc_mask);
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> SegmentResult<Self> {
        let mut cursor = Cursor::new(bytes);
        let flag = TrigramFlag::from_byte(cursor.u8()?)?;
        let count = cursor.u32()? as usize;
        let mut records = Vec::with_capacity(count.min(bytes.len() / POSTING_BYTES));
        let mut previous = None;
        for _ in 0..count {
            let record = TrigramRecord {
                trigram: cursor.u32()?,
                next_mask: cursor.u8()?,
                loc_mask: cursor.u8()?,
            };
            if previous.is_some_and(|previous| previous >= record.trigram) {
                return Err(SegmentError::Malformed(
                    "payload records are not strictly sorted".to_string(),
                ));
            }
            previous = Some(record.trigram);
            records.push(record);
        }
        cursor.finish()?;
        if flag != TrigramFlag::Indexed && !records.is_empty() {
            return Err(SegmentError::Malformed(
                "an unindexed payload carries records".to_string(),
            ));
        }
        Ok(Self { flag, records })
    }
}

/// One member of a segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentMember {
    pub rel_path: RelPath,
    pub content: ContentHash,
    pub size: u64,
}

/// A file table row of a segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentFile {
    pub rel_path: RelPath,
    pub content: ContentHash,
    pub size: u64,
    pub flag: TrigramFlag,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Posting {
    pub file_id: u32,
    pub next_mask: u8,
    pub loc_mask: u8,
}

/// Canonical segment bytes and their content-derived id.
#[derive(Clone, Eq, PartialEq)]
pub struct SegmentBytes {
    pub id: [u8; 32],
    pub bytes: Vec<u8>,
}

impl fmt::Debug for SegmentBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentBytes")
            .field("id", &crate::blob_store::v2::to_hex(&self.id))
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// Assembles canonical segment bytes from members and their payloads.
pub fn assemble(
    policy: &TrigramPolicy,
    members: impl IntoIterator<Item = (SegmentMember, TrigramPayload)>,
) -> SegmentResult<SegmentBytes> {
    let mut sorted: BTreeMap<RelPath, (SegmentMember, TrigramPayload)> = BTreeMap::new();
    for (member, payload) in members {
        let path = member.rel_path.clone();
        if sorted.insert(path.clone(), (member, payload)).is_some() {
            return Err(SegmentError::DuplicatePath(path));
        }
    }
    let policy_fingerprint = policy.fingerprint();
    let mut file_table = Vec::new();
    let mut postings_by_trigram: BTreeMap<u32, Vec<Posting>> = BTreeMap::new();
    for (file_id, (member, payload)) in sorted.values().enumerate() {
        let path = member.rel_path.as_bytes();
        file_table.extend_from_slice(&(path.len() as u32).to_be_bytes());
        file_table.extend_from_slice(path);
        file_table.extend_from_slice(member.content.as_bytes());
        file_table.extend_from_slice(&member.size.to_be_bytes());
        file_table.push(payload.flag.byte());
        for record in &payload.records {
            postings_by_trigram
                .entry(record.trigram)
                .or_default()
                .push(Posting {
                    file_id: file_id as u32,
                    next_mask: record.next_mask,
                    loc_mask: record.loc_mask,
                });
        }
    }
    let id = {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"aft/trigram-segment/v1");
        hasher.update(&policy_fingerprint);
        hasher.update(&file_table);
        *hasher.finalize().as_bytes()
    };
    let posting_count: u64 = postings_by_trigram
        .values()
        .map(|postings| postings.len() as u64)
        .sum();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(SEGMENT_MAGIC);
    bytes.extend_from_slice(&SEGMENT_FORMAT.to_be_bytes());
    bytes.extend_from_slice(&policy_fingerprint);
    bytes.extend_from_slice(&(sorted.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&(postings_by_trigram.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&posting_count.to_be_bytes());
    bytes.extend_from_slice(&file_table);
    let mut first = 0_u64;
    for (trigram, postings) in &postings_by_trigram {
        bytes.extend_from_slice(&trigram.to_be_bytes());
        bytes.extend_from_slice(&first.to_be_bytes());
        bytes.extend_from_slice(&(postings.len() as u32).to_be_bytes());
        first += postings.len() as u64;
    }
    for postings in postings_by_trigram.values() {
        for posting in postings {
            bytes.extend_from_slice(&posting.file_id.to_be_bytes());
            bytes.push(posting.next_mask);
            bytes.push(posting.loc_mask);
        }
    }
    let footer = blake3::hash(&bytes);
    bytes.extend_from_slice(footer.as_bytes());
    Ok(SegmentBytes { id, bytes })
}

/// Builds a segment by reading each member's bytes from the checkout.
pub fn build_from_files(
    root: &Path,
    members: &[RelPath],
    policy: &TrigramPolicy,
) -> SegmentResult<SegmentBytes> {
    let mut inputs = Vec::with_capacity(members.len());
    for rel_path in members {
        let bytes = fs::read(root.join(rel_path_to_os(rel_path)?))?;
        inputs.push((
            SegmentMember {
                rel_path: rel_path.clone(),
                content: ContentHash::of(&bytes),
                size: bytes.len() as u64,
            },
            TrigramPayload::extract(&bytes, policy),
        ));
    }
    assemble(policy, inputs)
}

/// Where a segment builder reads stored payloads from.
pub trait PayloadSource {
    fn payload(&self, key: &FamilyKey) -> Result<Option<Vec<u8>>, StoreError>;
}

impl PayloadSource for FamilyStore {
    fn payload(&self, key: &FamilyKey) -> Result<Option<Vec<u8>>, StoreError> {
        self.get(key)
    }
}

impl PayloadSource for FamilyStoreReader {
    fn payload(&self, key: &FamilyKey) -> Result<Option<Vec<u8>>, StoreError> {
        self.get(key)
    }
}

/// Builds a segment from stored payloads only; no checkout bytes are read.
pub fn build_from_blobs(
    source: &dyn PayloadSource,
    members: &[SegmentMember],
    policy: &TrigramPolicy,
) -> SegmentResult<SegmentBytes> {
    let mut inputs = Vec::with_capacity(members.len());
    for member in members {
        let key = TrigramKey {
            content: member.content,
            policy: *policy,
        }
        .family_key();
        let payload = source
            .payload(&key)?
            .ok_or_else(|| SegmentError::MissingPayload {
                rel_path: member.rel_path.clone(),
                key,
            })?;
        inputs.push((member.clone(), TrigramPayload::decode(&payload)?));
    }
    assemble(policy, inputs)
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Writes `segment` into the family directory under the protocol above. The
/// caller must already have listed `segment.id` in its live pin.
pub fn write_segment(
    store: &FamilyStore,
    storage: &Path,
    segment: &SegmentBytes,
    observer: Option<&dyn DurabilityObserver>,
) -> SegmentResult<PathBuf> {
    if store.plane() != FamilyPlane::Trigram {
        return Err(SegmentError::Malformed(
            "segments are written through the trigram store".to_string(),
        ));
    }
    let path = segment_path(storage, store.family(), &segment.id)?;
    store.begin_segment(&segment.id)?;
    observe(observer, DurabilityStep::SegmentRowRecorded);
    let already_durable = SegmentReader::open(&path).is_ok_and(|reader| reader.id() == segment.id);
    if !already_durable {
        let temporary = path.with_file_name(format!(
            ".{}.tmp.{}.{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("trigram-seg"),
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> SegmentResult<()> {
            let mut file = crate::private_storage::options()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(&segment.bytes)?;
            file.sync_all()?;
            drop(file);
            crate::fs_lock::rename_over(&temporary, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
    }
    super::sync_directory(path.parent().unwrap_or(storage))
        .map_err(|error| SegmentError::Malformed(error.to_string()))?;
    observe(observer, DurabilityStep::SegmentFileSynced);
    if !store.finish_segment(&segment.id, segment.bytes.len() as u64)? {
        return Err(SegmentError::Malformed(
            "the segment row disappeared before it became durable".to_string(),
        ));
    }
    observe(observer, DurabilityStep::SegmentDurable);
    Ok(path)
}

/// A verified, in-memory segment.
#[derive(Debug)]
pub struct SegmentReader {
    id: [u8; 32],
    policy_fingerprint: [u8; 32],
    files: Vec<SegmentFile>,
    lookup: Vec<(u32, u64, u32)>,
    postings: Vec<Posting>,
}

impl SegmentReader {
    pub fn open(path: &Path) -> SegmentResult<Self> {
        Self::from_bytes(&fs::read(path)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> SegmentResult<Self> {
        if bytes.len() < 32 {
            return Err(SegmentError::Malformed("segment is truncated".to_string()));
        }
        let (body, footer) = bytes.split_at(bytes.len() - 32);
        if blake3::hash(body).as_bytes() != footer {
            return Err(SegmentError::Malformed(
                "segment footer does not match its bytes".to_string(),
            ));
        }
        let mut cursor = Cursor::new(body);
        if cursor.take(8)? != SEGMENT_MAGIC {
            return Err(SegmentError::Malformed("bad segment magic".to_string()));
        }
        if cursor.u32()? != SEGMENT_FORMAT {
            return Err(SegmentError::Malformed(
                "unknown segment format".to_string(),
            ));
        }
        let policy_fingerprint: [u8; 32] = cursor.take(32)?.try_into().expect("32 bytes");
        let file_count = cursor.u32()? as usize;
        let trigram_count = cursor.u32()? as usize;
        let posting_count = cursor.u64()? as usize;
        let table_start = cursor.position;
        let mut files = Vec::with_capacity(file_count.min(body.len()));
        for _ in 0..file_count {
            let path_len = cursor.u32()? as usize;
            let rel_path = RelPath::new(cursor.take(path_len)?.to_vec())
                .map_err(|error| SegmentError::Malformed(error.to_string()))?;
            let content = ContentHash::from_bytes(cursor.take(32)?.try_into().expect("32 bytes"));
            let size = cursor.u64()?;
            let flag = TrigramFlag::from_byte(cursor.u8()?)?;
            files.push(SegmentFile {
                rel_path,
                content,
                size,
                flag,
            });
        }
        let id = {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"aft/trigram-segment/v1");
            hasher.update(&policy_fingerprint);
            hasher.update(&body[table_start..cursor.position]);
            *hasher.finalize().as_bytes()
        };
        let mut lookup = Vec::with_capacity(trigram_count.min(body.len() / LOOKUP_ENTRY_BYTES));
        for _ in 0..trigram_count {
            lookup.push((cursor.u32()?, cursor.u64()?, cursor.u32()?));
        }
        let mut postings = Vec::with_capacity(posting_count.min(body.len() / POSTING_BYTES));
        for _ in 0..posting_count {
            postings.push(Posting {
                file_id: cursor.u32()?,
                next_mask: cursor.u8()?,
                loc_mask: cursor.u8()?,
            });
        }
        cursor.finish()?;
        Ok(Self {
            id,
            policy_fingerprint,
            files,
            lookup,
            postings,
        })
    }

    pub fn id(&self) -> [u8; 32] {
        self.id
    }

    pub fn policy_fingerprint(&self) -> [u8; 32] {
        self.policy_fingerprint
    }

    pub fn files(&self) -> &[SegmentFile] {
        &self.files
    }

    pub fn trigram_count(&self) -> usize {
        self.lookup.len()
    }

    /// The postings of one trigram, in file-id order.
    pub fn postings(&self, trigram: u32) -> &[Posting] {
        match self
            .lookup
            .binary_search_by_key(&trigram, |(trigram, _, _)| *trigram)
        {
            Ok(index) => {
                let (_, first, count) = self.lookup[index];
                let start = first as usize;
                let end = start
                    .saturating_add(count as usize)
                    .min(self.postings.len());
                &self.postings[start.min(end)..end]
            }
            Err(_) => &[],
        }
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, len: usize) -> SegmentResult<&'a [u8]> {
        let end = self
            .position
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| SegmentError::Malformed("unexpected end of data".to_string()))?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn u8(&mut self) -> SegmentResult<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> SegmentResult<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> SegmentResult<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn finish(&self) -> SegmentResult<()> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(SegmentError::Malformed("trailing bytes".to_string()))
        }
    }
}

pub(crate) fn rel_path_to_os(rel_path: &RelPath) -> SegmentResult<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(
            rel_path.as_bytes(),
        )))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(rel_path.as_bytes())
            .map(PathBuf::from)
            .map_err(|_| SegmentError::Malformed("non-UTF-8 path on this platform".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_store::v2::StoreWriteAccess;

    fn policy() -> TrigramPolicy {
        TrigramPolicy { max_file_size: 64 }
    }

    fn checkout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), b"fn alpha() { beta(); }\n").unwrap();
        fs::write(dir.path().join("src/b.rs"), b"fn beta() {}\n").unwrap();
        fs::write(dir.path().join("big.txt"), vec![b'x'; 100]).unwrap();
        fs::write(dir.path().join("bin.dat"), [0_u8, 159, 146, 150, 0, 1, 2]).unwrap();
        dir
    }

    fn members() -> Vec<RelPath> {
        ["src/b.rs", "big.txt", "src/a.rs", "bin.dat"]
            .into_iter()
            .map(|path| RelPath::new(path.as_bytes().to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn file_built_and_blob_built_segments_are_byte_identical() {
        let root = checkout();
        let storage = tempfile::tempdir().unwrap();
        let store = FamilyStore::open(
            &StoreWriteAccess::for_registered_view(storage.path(), "family"),
            FamilyPlane::Trigram,
        )
        .unwrap();
        let mut blob_members = Vec::new();
        for rel_path in members() {
            let bytes = fs::read(root.path().join(rel_path_to_os(&rel_path).unwrap())).unwrap();
            let content = ContentHash::of(&bytes);
            let key = TrigramKey {
                content,
                policy: policy(),
            }
            .family_key();
            store
                .put_or_touch(&key, &TrigramPayload::extract(&bytes, &policy()).encode())
                .unwrap();
            blob_members.push(SegmentMember {
                rel_path,
                content,
                size: bytes.len() as u64,
            });
        }
        // Remove the checkout so the blob-built segment cannot read it.
        let from_files = build_from_files(root.path(), &members(), &policy()).unwrap();
        drop(root);
        let from_blobs = build_from_blobs(&store, &blob_members, &policy()).unwrap();
        assert_eq!(from_files, from_blobs);

        let reader = SegmentReader::from_bytes(&from_files.bytes).unwrap();
        assert_eq!(reader.id(), from_files.id);
        let flags = reader
            .files()
            .iter()
            .map(|file| (file.rel_path.as_bytes().to_vec(), file.flag))
            .collect::<Vec<_>>();
        assert_eq!(
            flags,
            vec![
                (b"big.txt".to_vec(), TrigramFlag::UnindexedOversize),
                (b"bin.dat".to_vec(), TrigramFlag::UnindexedBinary),
                (b"src/a.rs".to_vec(), TrigramFlag::Indexed),
                (b"src/b.rs".to_vec(), TrigramFlag::Indexed),
            ]
        );
        let beta = crate::search_index::pack_trigram(b'b', b'e', b't');
        let files = reader
            .postings(beta)
            .iter()
            .map(|posting| posting.file_id)
            .collect::<Vec<_>>();
        assert_eq!(files, vec![2, 3]);
    }

    #[test]
    fn payload_trigrams_match_the_search_index_extraction() {
        let bytes = b"Hello, World";
        let payload = TrigramPayload::extract(bytes, &policy());
        let mut expected = crate::search_index::extract_trigrams(bytes)
            .into_iter()
            .map(|(trigram, _, _)| trigram)
            .collect::<Vec<_>>();
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(
            payload
                .records
                .iter()
                .map(|record| record.trigram)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(TrigramPayload::decode(&payload.encode()).unwrap(), payload);
    }

    /// A payload's next-character and position masks are exactly what the
    /// search index stores for the same bytes, including the next-character
    /// mask of a trigram whose only occurrence ends the file.
    #[test]
    fn payload_masks_match_what_the_search_index_stores() {
        // "abc" repeats before upper- and lower-case letters and past position
        // eight, so both masks fold several occurrences; "qz!" ends the file.
        let bytes = b"abcXabc\nABC xabcabc qz!";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.txt");
        fs::write(&path, bytes).unwrap();
        let mut index = crate::search_index::SearchIndex::new();
        index.index_file(&path, bytes);
        let stored = index.delta_posting_masks(&path);

        let end_of_file = crate::search_index::pack_trigram(b'q', b'z', b'!');
        assert_eq!(
            stored.get(&end_of_file).map(|masks| masks.0),
            Some(1),
            "the fixture must exercise the end-of-file next character"
        );
        let payload = TrigramPayload::extract(
            bytes,
            &TrigramPolicy {
                max_file_size: 1 << 20,
            },
        );
        let extracted = payload
            .records
            .iter()
            .map(|record| (record.trigram, (record.next_mask, record.loc_mask)))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(extracted, stored);
    }

    #[test]
    fn a_corrupt_segment_is_rejected() {
        let root = checkout();
        let mut segment = build_from_files(root.path(), &members(), &policy()).unwrap();
        let middle = segment.bytes.len() / 2;
        segment.bytes[middle] ^= 0xff;
        assert!(SegmentReader::from_bytes(&segment.bytes).is_err());
    }
}
