//! A checkout's state: which content each of its paths holds, and the
//! evidence that lets a later bind trust that without reading the file.
//!
//! A state is the payload of a `STATE-<n>` record (see [`crate::family`]).
//! Paths are sorted by their bytes, so a directory is a contiguous range and
//! a lookup is a binary search.

use std::path::Path;
use std::sync::Arc;

use crate::content::DecodeError;
use crate::content::codec::{Decoder, Encoder};
use crate::family::record::RecordError;
use crate::ids::{ContentKey, Digest128};

/// What a stat call said about a file when its content was last read.
///
/// A later bind trusts the recorded content only when every field it can
/// observe again is unchanged and the file was not modified too close to
/// `observed_ns` (the racily-clean rule). Fields an operating system does not
/// provide are `None`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StatEvidence {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub birth_ns: Option<i64>,
    /// The inode, or the file id on Windows.
    pub file_id: u64,
    /// The device, or the volume serial number on Windows.
    pub device: u64,
    /// On Windows, the file's update sequence number and its volume journal.
    pub usn: Option<u64>,
    pub journal: Option<u64>,
    /// When the stat was taken.
    pub observed_ns: i64,
}

/// What AFT knows about a path's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathKind {
    /// Read and indexed; `content` is the key of its bytes.
    Text,
    /// Read and found to be binary; `content` is the key of its bytes.
    Binary,
    /// Larger than the read limit, or than the text-index limit when its
    /// path is not one a scan reads past that limit, and not read;
    /// `content` is a stand-in derived from its path and evidence.
    Oversize,
    /// Could not be read; `content` is a stand-in. Grep reads it directly
    /// and reports the error.
    Unreadable,
    /// A symbolic link, never followed. Its target is in
    /// [`PathRecord::link`]; `content` is a stand-in.
    Symlink,
    /// Text larger than the text-index limit, read because its path is one
    /// a scan reads up to the larger read limit (a file the call graph
    /// parses or meaning search embeds); `content` is the key of its bytes.
    /// Its text is not indexed, so text queries read it directly.
    Large,
}

impl PathKind {
    const fn code(self) -> u8 {
        match self {
            Self::Text => 0,
            Self::Binary => 1,
            Self::Oversize => 2,
            Self::Unreadable => 3,
            Self::Symlink => 4,
            Self::Large => 5,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Text),
            1 => Some(Self::Binary),
            2 => Some(Self::Oversize),
            3 => Some(Self::Unreadable),
            4 => Some(Self::Symlink),
            5 => Some(Self::Large),
            _ => None,
        }
    }

    /// Whether `content` is the key of the path's real bytes.
    #[must_use]
    pub const fn has_content(self) -> bool {
        matches!(self, Self::Text | Self::Binary | Self::Large)
    }
}

/// One path of a checkout.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PathRecord {
    /// The path relative to the checkout root, `/`-separated, exactly as
    /// the filesystem spells it.
    pub path: Vec<u8>,
    /// The Unix mode bits Git records, or zero.
    pub mode: u32,
    pub kind: PathKind,
    pub content: ContentKey,
    /// A symlink's target bytes, exactly as stored in the link; empty for
    /// every other kind.
    pub link: Vec<u8>,
    pub stat: StatEvidence,
    /// The language the file is parsed as, empty when it is not parsed.
    pub language: String,
}

/// Which content each path of a checkout held at one moment.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CheckoutState {
    /// The Git tree and index the state was built from, when the checkout
    /// is in Git. Opaque to the store; compared byte for byte.
    pub git: Vec<u8>,
    /// A digest of every attributes source and conversion setting in effect,
    /// under which the recorded contents were read.
    pub conversion: Digest128,
    /// The checkout's root folder as UTF-8, so another checkout of the
    /// family can tell when this one's folder is gone; empty when the root
    /// is not UTF-8 or was not recorded.
    root: Vec<u8>,
    /// Shared by every copy of the state, so copying a state (a view, the
    /// call graph's inputs) copies no record.
    paths: Arc<Vec<PathRecord>>,
    /// For each path, the call-graph fragment that holds its resolution in
    /// this checkout, when it has one. Empty, or one entry per path.
    fragments: Arc<Vec<Option<Digest128>>>,
    /// The producer of the graph, independent of which content records a
    /// shared family happens to hold. Legacy states have no producer stamp.
    graph_extractor: Option<String>,
}

/// Why a state could not be built.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum StateError {
    #[error("path {0:?} appears twice")]
    DuplicatePath(String),
    #[error("a path, Git fingerprint or language is too long to store")]
    TooLong,
    #[error("plane keys must have one entry per path")]
    PlaneMismatch,
}

impl CheckoutState {
    /// A state holding `paths`, sorted.
    ///
    /// # Errors
    ///
    /// When a path appears twice or a field is too long to store.
    pub fn new(
        git: Vec<u8>,
        conversion: Digest128,
        mut paths: Vec<PathRecord>,
    ) -> Result<Self, StateError> {
        paths.sort_by(|a, b| a.path.cmp(&b.path));
        if let Some(pair) = paths.windows(2).find(|pair| pair[0].path == pair[1].path) {
            return Err(StateError::DuplicatePath(
                String::from_utf8_lossy(&pair[0].path).into_owned(),
            ));
        }
        if git.len() > usize::from(u8::MAX)
            || paths.iter().any(|record| {
                u32::try_from(record.path.len()).is_err()
                    || record.language.len() > usize::from(u8::MAX)
            })
        {
            return Err(StateError::TooLong);
        }
        Ok(Self {
            git,
            conversion,
            root: Vec::new(),
            paths: Arc::new(paths),
            fragments: Arc::default(),
            graph_extractor: None,
        })
    }

    /// This state recording `root` as its checkout's root folder. A root
    /// that is not UTF-8 is not recorded.
    #[must_use]
    pub fn with_root(mut self, root: &Path) -> Self {
        self.root = root
            .to_str()
            .map_or_else(Vec::new, |root| root.as_bytes().to_vec());
        self
    }

    /// This state recording the root folder `from` records.
    #[must_use]
    pub fn with_root_of(mut self, from: &Self) -> Self {
        self.root.clone_from(&from.root);
        self
    }

    /// The checkout's root folder, when recorded.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        std::str::from_utf8(&self.root)
            .ok()
            .filter(|root| !root.is_empty())
            .map(Path::new)
    }

    /// This state with `fragments`, one entry per path in path order: the
    /// call-graph fragment each path's resolution is in.
    ///
    /// # Errors
    ///
    /// When `fragments` does not have one entry per path.
    pub fn with_fragments(mut self, fragments: Vec<Option<Digest128>>) -> Result<Self, StateError> {
        if fragments.len() != self.paths.len() {
            return Err(StateError::PlaneMismatch);
        }
        self.fragments = Arc::new(fragments);
        self.graph_extractor = None;
        Ok(self)
    }

    /// Names the producer of these fragments. Content records and fragments
    /// can live in different shared segments, so finding a content record
    /// does not prove that the graph was built by the same extractor.
    ///
    /// # Errors
    ///
    /// When `fragments` does not have one entry per path.
    pub fn with_fragments_for(
        self,
        fragments: Vec<Option<Digest128>>,
        extractor: &str,
    ) -> Result<Self, StateError> {
        let mut state = self.with_fragments(fragments)?;
        state.graph_extractor = Some(extractor.to_string());
        Ok(state)
    }

    /// Whether this extractor built the graph the state names. An unstamped
    /// state is never current, even if shared segments hold current records.
    #[must_use]
    pub fn has_fragments_for(&self, extractor: &str) -> bool {
        self.has_fragments() && self.graph_extractor.as_deref() == Some(extractor)
    }

    /// Whether the state names each path's call-graph fragment: a state
    /// published before the call graph was built names none.
    #[must_use]
    pub fn has_fragments(&self) -> bool {
        !self.fragments.is_empty() || self.paths.is_empty()
    }

    /// The call-graph fragment of the path at `index`, if it has one.
    #[must_use]
    pub fn fragment(&self, index: usize) -> Option<Digest128> {
        self.fragments.get(index).copied().flatten()
    }

    /// The paths in byte order.
    #[must_use]
    pub fn paths(&self) -> &[PathRecord] {
        &self.paths
    }

    /// Sets the language of the path at `index` in [`paths`](Self::paths),
    /// the one field of a record that changes without moving it. Copies the
    /// records first only when another copy of this state shares them.
    ///
    /// # Errors
    ///
    /// When `language` is too long to store.
    ///
    /// # Panics
    ///
    /// When `index` is not a path's index.
    pub fn set_language(&mut self, index: usize, language: String) -> Result<(), StateError> {
        if language.len() > usize::from(u8::MAX) {
            return Err(StateError::TooLong);
        }
        Arc::make_mut(&mut self.paths)[index].language = language;
        Ok(())
    }

    /// The record of `path`.
    #[must_use]
    pub fn get(&self, path: &[u8]) -> Option<&PathRecord> {
        self.position(path).map(|index| &self.paths[index])
    }

    /// The index of `path` in [`paths`](Self::paths).
    #[must_use]
    pub fn position(&self, path: &[u8]) -> Option<usize> {
        self.paths
            .binary_search_by(|record| record.path.as_slice().cmp(path))
            .ok()
    }

    /// The range of [`paths`](Self::paths) inside directory `dir`, given
    /// without its trailing slash. The empty directory is the whole checkout.
    #[must_use]
    pub fn directory(&self, dir: &[u8]) -> std::ops::Range<usize> {
        if dir.is_empty() {
            return 0..self.paths.len();
        }
        let mut prefix = dir.to_vec();
        prefix.push(b'/');
        let start = self
            .paths
            .partition_point(|record| record.path.as_slice() < prefix.as_slice());
        let end =
            start + self.paths[start..].partition_point(|record| record.path.starts_with(&prefix));
        start..end
    }

    /// The bytes stored as the payload of a state record.
    ///
    /// Paths are sorted, so each is stored as the length it shares with the
    /// path before it plus the rest. Numbers are varints; times are stored
    /// as differences from a related time (change and birth time from the
    /// modification time, observation time and device from the previous
    /// path's), which are small or zero for files of one checkout.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        self.encode_layout(STATE_FORMAT)
    }

    /// [`Self::encode`] naming layout `format` instead of [`STATE_FORMAT`], as
    /// a build writing that layout would label these bytes: for tests of
    /// what a reader does with a payload from a newer or older build.
    #[cfg(test)]
    pub(crate) fn encode_as(&self, format: u8) -> Vec<u8> {
        self.encode_layout(format)
    }

    fn encode_layout(&self, format: u8) -> Vec<u8> {
        let mut out = Encoder::default();
        out.byte(format);
        out.blob(&self.git);
        out.blob(self.conversion.as_bytes());
        out.blob(&self.root);
        out.usize(self.paths.len());
        let mut previous: Option<&PathRecord> = None;
        for record in self.paths.iter() {
            let shared = previous.map_or(0, |previous| {
                previous
                    .path
                    .iter()
                    .zip(&record.path)
                    .take_while(|(a, b)| a == b)
                    .count()
            });
            out.usize(shared);
            out.blob(&record.path[shared..]);
            out.u32(record.mode);
            out.byte(record.kind.code());
            out.blob(record.content.digest().as_bytes());
            out.blob(&record.link);
            let stat = &record.stat;
            let before = previous.map(|previous| &previous.stat);
            out.u64(stat.size);
            out.u64(zigzag(stat.mtime_ns));
            out.u64(zigzag(stat.ctime_ns.wrapping_sub(stat.mtime_ns)));
            put_optional(
                &mut out,
                stat.birth_ns
                    .map(|birth| zigzag(birth.wrapping_sub(stat.mtime_ns))),
            );
            out.u64(stat.file_id);
            out.u64(zigzag(
                stat.device
                    .wrapping_sub(before.map_or(0, |before| before.device))
                    .cast_signed(),
            ));
            put_optional(&mut out, stat.usn);
            put_optional(&mut out, stat.journal);
            out.u64(zigzag(
                stat.observed_ns
                    .wrapping_sub(before.map_or(0, |before| before.observed_ns)),
            ));
            out.str(&record.language);
            previous = Some(record);
        }
        // Flag 2 adds a producer stamp before the optional digests. Older
        // readers reject this flag as unusable and rebuild the checkout.
        out.byte(if self.graph_extractor.is_some() {
            2
        } else {
            u8::from(!self.fragments.is_empty())
        });
        if let Some(extractor) = &self.graph_extractor {
            out.blob(extractor.as_bytes());
        }
        for fragment in self.fragments.iter() {
            match fragment {
                Some(digest) => {
                    out.byte(1);
                    out.blob(digest.as_bytes());
                }
                None => out.byte(0),
            }
        }
        out.finish()
    }

    /// Reads a state record's payload.
    ///
    /// # Errors
    ///
    /// When the payload is not a well-formed state, and with
    /// [`RecordError::NewerPayload`] when it names a layout above
    /// [`STATE_FORMAT`]: a newer build wrote it, so it is refused rather than
    /// read as damaged and built again.
    pub(crate) fn decode(payload: &[u8]) -> Result<Self, RecordError> {
        Self::decode_layout(payload, true)
    }

    fn decode_layout(payload: &[u8], stamped_graphs: bool) -> Result<Self, RecordError> {
        let invalid = |error: DecodeError| RecordError::Invalid(error.0);
        let mut input = Decoder::new(payload).map_err(invalid)?;
        let format = input.byte().map_err(invalid)?;
        if format > STATE_FORMAT {
            return Err(RecordError::NewerPayload {
                found: u16::from(format),
                supported: u16::from(STATE_FORMAT),
            });
        }
        // Format 2 is format 3 without the root folder.
        if format != STATE_FORMAT && format != 2 {
            return Err(RecordError::Invalid("unknown checkout state format"));
        }
        let git = input.blob().map_err(invalid)?.to_vec();
        if git.len() > usize::from(u8::MAX) {
            return Err(RecordError::Invalid("Git fingerprint too long"));
        }
        let conversion = Digest128::from_bytes(
            input
                .blob()
                .map_err(invalid)?
                .try_into()
                .map_err(|_| RecordError::Invalid("conversion digest length"))?,
        );
        let root = if format == STATE_FORMAT {
            let root = input.blob().map_err(invalid)?.to_vec();
            if std::str::from_utf8(&root).is_err() {
                return Err(RecordError::Invalid("root folder is not UTF-8"));
            }
            root
        } else {
            Vec::new()
        };
        let count = input.count_of(MIN_PATH_RECORD_LEN).map_err(invalid)?;
        let mut paths = Vec::<PathRecord>::with_capacity(count);
        for _ in 0..count {
            let shared = input.usize().map_err(invalid)?;
            let suffix = input.blob().map_err(invalid)?;
            let mut path = match paths.last() {
                Some(last) if shared <= last.path.len() => last.path[..shared].to_vec(),
                None if shared == 0 => Vec::new(),
                _ => return Err(RecordError::Invalid("shared path prefix too long")),
            };
            path.extend_from_slice(suffix);
            let mode = input.u32().map_err(invalid)?;
            let kind = PathKind::from_code(input.byte().map_err(invalid)?)
                .ok_or(RecordError::Invalid("path kind"))?;
            let content = ContentKey::from_digest(Digest128::from_bytes(
                input
                    .blob()
                    .map_err(invalid)?
                    .try_into()
                    .map_err(|_| RecordError::Invalid("content key length"))?,
            ));
            let link = input.blob().map_err(invalid)?.to_vec();
            let before = paths.last().map(|last| last.stat);
            let size = input.u64().map_err(invalid)?;
            let mtime_ns = unzigzag(input.u64().map_err(invalid)?);
            let ctime_ns = mtime_ns.wrapping_add(unzigzag(input.u64().map_err(invalid)?));
            let birth_ns =
                optional(&mut input)?.map(|delta| mtime_ns.wrapping_add(unzigzag(delta)));
            let file_id = input.u64().map_err(invalid)?;
            let device = before
                .map_or(0, |before| before.device)
                .wrapping_add(unzigzag(input.u64().map_err(invalid)?).cast_unsigned());
            let usn = optional(&mut input)?;
            let journal = optional(&mut input)?;
            let observed_ns = before
                .map_or(0, |before| before.observed_ns)
                .wrapping_add(unzigzag(input.u64().map_err(invalid)?));
            let language = input.string().map_err(invalid)?;
            if language.len() > usize::from(u8::MAX) {
                return Err(RecordError::Invalid("language too long"));
            }
            if paths.last().is_some_and(|last| last.path >= path) {
                return Err(RecordError::Invalid("paths out of order"));
            }
            paths.push(PathRecord {
                path,
                mode,
                kind,
                content,
                link,
                stat: StatEvidence {
                    size,
                    mtime_ns,
                    ctime_ns,
                    birth_ns,
                    file_id,
                    device,
                    usn,
                    journal,
                    observed_ns,
                },
                language,
            });
        }
        let mut fragments = Vec::new();
        let mut graph_extractor = None;
        match input.byte().map_err(invalid)? {
            0 => {}
            flag @ (1 | 2) if flag == 1 || stamped_graphs => {
                if flag == 2 {
                    graph_extractor = Some(
                        std::str::from_utf8(input.blob().map_err(invalid)?)
                            .map_err(|_| RecordError::Invalid("graph extractor is not UTF-8"))?
                            .to_string(),
                    );
                }
                fragments.reserve(paths.len());
                for _ in 0..paths.len() {
                    fragments.push(match input.byte().map_err(invalid)? {
                        0 => None,
                        1 => Some(Digest128::from_bytes(
                            input
                                .blob()
                                .map_err(invalid)?
                                .try_into()
                                .map_err(|_| RecordError::Invalid("fragment key length"))?,
                        )),
                        _ => return Err(RecordError::Invalid("fragment flag")),
                    });
                }
            }
            _ => return Err(RecordError::Invalid("fragment section flag")),
        }
        input.finish().map_err(invalid)?;
        Ok(Self {
            git,
            conversion,
            root,
            paths: Arc::new(paths),
            fragments: Arc::new(fragments),
            graph_extractor,
        })
    }
}

/// The version of [`CheckoutState::encode`]'s layout: the first byte after
/// the payload's string table. A reader refuses a payload naming a higher
/// one, since a newer build wrote it. So every later layout must keep the
/// string table's encoding and this byte where they are, and any other
/// change to the layout must raise this number, except the stamped fragment
/// flag 2: old readers deliberately treat that flag as unusable and rebuild,
/// rather than refusing the whole family on rollback. The record envelope
/// and the rest of the state layout are unchanged.
const STATE_FORMAT: u8 = 3;

/// How much of a state record [`newer_state_format`] reads to find the
/// payload's format byte: the record header and the payload's string table,
/// which holds only the checkout's language names, fit in it many times
/// over. A table that does not is read whole.
pub(crate) const FORMAT_PEEK: u64 = 64 << 10;

/// The refusal [`CheckoutState::decode`] gives the state record at `path`,
/// the `number`th state of `family`'s checkout, when its payload names a
/// layout above [`STATE_FORMAT`]. Reads only the record's first bytes
/// unless they name such a layout, or the string table runs past them; then
/// the record is read and checked whole, so damaged bytes are told apart
/// from a newer payload exactly as a reader tells them. `None` for any other
/// record, or one that cannot be read: whoever reads it later reports why.
pub(crate) fn newer_state_format(
    path: &Path,
    family: crate::ids::FamilyId,
    number: u64,
) -> Option<RecordError> {
    let head = crate::family::record::head(path, FORMAT_PEEK)?;
    let format =
        Decoder::new(crate::family::record::head_payload(&head)).and_then(|mut input| input.byte());
    match format {
        Ok(format) if format <= STATE_FORMAT => return None,
        // The whole record was read and holds no format byte: it is
        // damaged, and the reader that needs it says so.
        Err(_) if (head.len() as u64) < FORMAT_PEEK => return None,
        _ => {}
    }
    let payload =
        crate::family::record::read(path, crate::family::record::Kind::State, family, number)
            .ok()?;
    match CheckoutState::decode(&payload) {
        Err(error @ RecordError::NewerPayload { .. }) => Some(error),
        _ => None,
    }
}

/// The fewest bytes one path record takes in [`CheckoutState::encode`]'s
/// layout: the 16-byte content key and its length byte, plus one byte for
/// each of the other fifteen fields (a varint, flag, kind byte, string index
/// or empty blob length). A decoded path count is checked against it before
/// anything is reserved, so a payload cannot claim more paths than its bytes
/// could hold.
const MIN_PATH_RECORD_LEN: usize = Digest128::LEN + 1 + 15;

const fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)).cast_unsigned()
}

const fn unzigzag(value: u64) -> i64 {
    (value >> 1).cast_signed() ^ -((value & 1).cast_signed())
}

fn put_optional(out: &mut Encoder, value: Option<u64>) {
    match value {
        Some(value) => {
            out.byte(1);
            out.u64(value);
        }
        None => out.byte(0),
    }
}

fn optional(input: &mut Decoder<'_>) -> Result<Option<u64>, RecordError> {
    let invalid = |error: DecodeError| RecordError::Invalid(error.0);
    match input.byte().map_err(invalid)? {
        0 => Ok(None),
        1 => Ok(Some(input.u64().map_err(invalid)?)),
        _ => Err(RecordError::Invalid("optional field flag")),
    }
}

crate::heap_bytes_none!(StatEvidence, PathKind);
crate::heap_bytes_of_fields!(PathRecord {
    path,
    mode,
    kind,
    content,
    link,
    stat,
    language,
});
crate::heap_bytes_of_fields!(CheckoutState {
    git,
    conversion,
    root,
    paths,
    fragments,
    graph_extractor,
});

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str, content: &str) -> PathRecord {
        PathRecord {
            path: path.as_bytes().to_vec(),
            mode: 0o100_644,
            kind: PathKind::Text,
            content: ContentKey::of(content.as_bytes()),
            link: Vec::new(),
            stat: StatEvidence {
                size: content.len() as u64,
                mtime_ns: -5,
                birth_ns: Some(7),
                usn: Some(9),
                observed_ns: 11,
                ..StatEvidence::default()
            },
            language: "rust".to_string(),
        }
    }

    #[test]
    fn graph_producer_stamps_round_trip_and_legacy_readers_rebuild() {
        for paths in [Vec::new(), vec![record("a.rs", "a")]] {
            let fragments = vec![Some(Digest128::of(b"fragment")); paths.len()];
            let state = CheckoutState::new(Vec::new(), Digest128::of(b""), paths).unwrap();
            let legacy = state.clone().with_fragments(fragments.clone()).unwrap();
            assert!(!legacy.has_fragments_for("current"));
            let stamped = state.with_fragments_for(fragments, "current").unwrap();
            let payload = stamped.encode();
            let decoded = CheckoutState::decode(&payload).unwrap();
            assert_eq!(decoded, stamped);
            assert!(decoded.has_fragments_for("current"));
            assert!(!decoded.has_fragments_for("other"));
            // This is the old decoder's fragment-flag branch. Invalid state
            // payloads are unusable, not newer-format refusals, on rollback.
            assert!(matches!(
                CheckoutState::decode_layout(&payload, false),
                Err(RecordError::Invalid("fragment section flag"))
            ));
            assert_eq!(
                CheckoutState::decode_layout(&legacy.encode(), false).unwrap(),
                legacy
            );
        }
    }

    #[test]
    fn a_state_keeps_its_root_folder_through_encoding_and_rebuilding() {
        let root = Path::new("/work/checkouts/aft-feature");
        let state = CheckoutState::new(
            Vec::new(),
            Digest128::of(b""),
            vec![record("src/a.rs", "a")],
        )
        .unwrap()
        .with_root(root);
        assert_eq!(state.root(), Some(root));
        let decoded = CheckoutState::decode(&state.encode()).unwrap();
        assert_eq!(decoded.root(), Some(root));
        assert_eq!(decoded, state);
        // A state rebuilt from another keeps the root it names.
        let rebuilt = CheckoutState::new(Vec::new(), Digest128::of(b""), Vec::new())
            .unwrap()
            .with_root_of(&decoded);
        assert_eq!(rebuilt.root(), Some(root));
        // A state that never named a root names none after decoding either.
        let rootless =
            CheckoutState::new(Vec::new(), Digest128::of(b""), vec![record("a", "a")]).unwrap();
        assert_eq!(
            CheckoutState::decode(&rootless.encode()).unwrap().root(),
            None
        );
    }

    #[test]
    fn round_trips_sorted_and_refuses_duplicates() {
        let state = CheckoutState::new(
            b"tree".to_vec(),
            Digest128::of(b"attrs"),
            vec![
                record("src/b.rs", "b"),
                record("src/a.rs", "a"),
                record("README", "r"),
            ],
        )
        .unwrap();
        assert_eq!(state.paths()[0].path, b"README");
        let decoded = CheckoutState::decode(&state.encode()).unwrap();
        assert_eq!(decoded, state);
        // Extreme stat values survive the differences they are stored as.
        let mut extreme = record("z", "z");
        extreme.stat = StatEvidence {
            size: u64::MAX,
            mtime_ns: i64::MIN,
            ctime_ns: i64::MAX,
            birth_ns: Some(i64::MAX),
            file_id: u64::MAX,
            device: u64::MAX,
            usn: Some(0),
            journal: Some(u64::MAX),
            observed_ns: i64::MIN,
        };
        let state = CheckoutState::new(
            Vec::new(),
            Digest128::of(b""),
            vec![record("a", "a"), extreme, record("zz", "zz")],
        )
        .unwrap();
        assert_eq!(CheckoutState::decode(&state.encode()).unwrap(), state);
        // Fragment keys, some paths without one, survive too.
        let with_fragments = state
            .clone()
            .with_fragments(vec![
                Some(Digest128::of(b"f")),
                None,
                Some(Digest128::of(b"g")),
            ])
            .unwrap();
        let decoded = CheckoutState::decode(&with_fragments.encode()).unwrap();
        assert_eq!(decoded, with_fragments);
        assert_eq!(decoded.fragment(0), Some(Digest128::of(b"f")));
        assert_eq!(decoded.fragment(1), None);
        assert!(matches!(
            state.with_fragments(vec![None]),
            Err(StateError::PlaneMismatch)
        ));
        assert!(matches!(
            CheckoutState::new(
                Vec::new(),
                Digest128::of(b""),
                vec![record("a", "1"), record("a", "2")]
            ),
            Err(StateError::DuplicatePath(_))
        ));
    }

    #[test]
    fn a_directory_is_a_contiguous_range() {
        let state = CheckoutState::new(
            Vec::new(),
            Digest128::of(b""),
            ["a/b", "a/c/d", "a.rs", "ab/x", "a/b2"]
                .iter()
                .map(|path| record(path, path))
                .collect(),
        )
        .unwrap();
        let inside = |dir: &str| {
            state.paths()[state.directory(dir.as_bytes())]
                .iter()
                .map(|record| String::from_utf8_lossy(&record.path).into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(inside("a"), ["a/b", "a/b2", "a/c/d"]);
        assert_eq!(inside("a/c"), ["a/c/d"]);
        assert_eq!(inside("").len(), 5);
        assert!(inside("zz").is_empty());
    }

    /// A state payload whose header is well formed, with `count` as its path
    /// count and `filler` zero bytes after it.
    fn claiming(count: u64, filler: usize) -> Vec<u8> {
        let mut out = Encoder::default();
        out.byte(STATE_FORMAT);
        out.blob(b"");
        out.blob(Digest128::of(b"").as_bytes());
        // No root folder.
        out.blob(b"");
        out.u64(count);
        for _ in 0..filler {
            out.byte(0);
        }
        out.finish()
    }

    #[test]
    fn a_path_count_its_bytes_cannot_hold_fails_before_any_record() {
        const FILLER: usize = 64 * 1024;
        // 2^31 paths, about 400 GiB of records in memory, from 64 KiB.
        // One path per byte, which a bound of one byte per item let through.
        // And one path more than the bytes hold at the smallest record size.
        for count in [
            1 << 31,
            FILLER as u64,
            (FILLER / MIN_PATH_RECORD_LEN) as u64 + 1,
        ] {
            assert!(
                matches!(
                    CheckoutState::decode(&claiming(count, FILLER)),
                    Err(RecordError::Invalid("count exceeds the bytes left"))
                ),
                "{count} paths in {FILLER} bytes"
            );
        }
        // A count the bytes can hold gets as far as its first record, which
        // zero bytes do not make.
        assert!(matches!(
            CheckoutState::decode(&claiming((FILLER / MIN_PATH_RECORD_LEN) as u64, FILLER)),
            Err(RecordError::Invalid("content key length"))
        ));
    }

    #[test]
    fn records_of_the_smallest_size_still_decode() {
        // Every field as short as the layout allows: no path count of a real
        // state is ever refused by the bound.
        let smallest = |path: Vec<u8>| PathRecord {
            path,
            mode: 0,
            kind: PathKind::Text,
            content: ContentKey::of(b""),
            link: Vec::new(),
            stat: StatEvidence::default(),
            language: String::new(),
        };
        let paths = std::iter::once(Vec::new())
            .chain((1..=200u8).map(|byte| vec![byte]))
            .map(smallest)
            .collect();
        let state = CheckoutState::new(Vec::new(), Digest128::of(b""), paths).unwrap();
        assert_eq!(CheckoutState::decode(&state.encode()).unwrap(), state);
    }
}
