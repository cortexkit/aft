//! A checkout's state: which content each of its paths holds, and the
//! evidence that lets a later bind trust that without reading the file.
//!
//! A state is the payload of a `STATE-<n>` record (see [`crate::family`]).
//! Paths are sorted by their bytes, so a directory is a contiguous range and
//! a lookup is a binary search.

use crate::family::record::{Payload, RecordError};
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

/// One path of a checkout.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PathRecord {
    /// The path relative to the checkout root, `/`-separated, exactly as
    /// the filesystem spells it.
    pub path: Vec<u8>,
    /// The Unix mode bits Git records, or zero.
    pub mode: u32,
    pub content: ContentKey,
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
    paths: Vec<PathRecord>,
}

/// Why a state could not be built.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum StateError {
    #[error("path {0:?} appears twice")]
    DuplicatePath(String),
    #[error("a path, Git fingerprint or language is too long to store")]
    TooLong,
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
            paths,
        })
    }

    /// The paths in byte order.
    #[must_use]
    pub fn paths(&self) -> &[PathRecord] {
        &self.paths
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
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(u8::try_from(self.git.len()).expect("checked in new"));
        out.extend_from_slice(&self.git);
        out.extend_from_slice(self.conversion.as_bytes());
        out.extend_from_slice(
            &u32::try_from(self.paths.len())
                .expect("a checkout has fewer than 2^32 paths")
                .to_le_bytes(),
        );
        for record in &self.paths {
            out.extend_from_slice(
                &u32::try_from(record.path.len())
                    .expect("checked in new")
                    .to_le_bytes(),
            );
            out.extend_from_slice(&record.path);
            out.extend_from_slice(&record.mode.to_le_bytes());
            out.extend_from_slice(record.content.digest().as_bytes());
            let stat = &record.stat;
            out.extend_from_slice(&stat.size.to_le_bytes());
            out.extend_from_slice(&stat.mtime_ns.to_le_bytes());
            out.extend_from_slice(&stat.ctime_ns.to_le_bytes());
            put_optional(&mut out, stat.birth_ns.map(i64::cast_unsigned));
            out.extend_from_slice(&stat.file_id.to_le_bytes());
            out.extend_from_slice(&stat.device.to_le_bytes());
            put_optional(&mut out, stat.usn);
            put_optional(&mut out, stat.journal);
            out.extend_from_slice(&stat.observed_ns.to_le_bytes());
            out.push(u8::try_from(record.language.len()).expect("checked in new"));
            out.extend_from_slice(record.language.as_bytes());
        }
        out
    }

    /// Reads a state record's payload.
    ///
    /// # Errors
    ///
    /// When the payload is not a well-formed state.
    pub(crate) fn decode(payload: &[u8]) -> Result<Self, RecordError> {
        let mut input = Payload::new(payload);
        let git_len = usize::from(input.take(1)?[0]);
        let git = input.take(git_len)?.to_vec();
        let conversion = input.digest()?;
        let count = input.u32()?;
        let mut paths = Vec::new();
        for _ in 0..count {
            let path_len =
                usize::try_from(input.u32()?).map_err(|_| RecordError::Invalid("path length"))?;
            let path = input.take(path_len)?.to_vec();
            let mode = input.u32()?;
            let content = ContentKey::from_digest(input.digest()?);
            let stat = StatEvidence {
                size: input.u64()?,
                mtime_ns: input.u64()?.cast_signed(),
                ctime_ns: input.u64()?.cast_signed(),
                birth_ns: optional(&mut input)?.map(u64::cast_signed),
                file_id: input.u64()?,
                device: input.u64()?,
                usn: optional(&mut input)?,
                journal: optional(&mut input)?,
                observed_ns: input.u64()?.cast_signed(),
            };
            let language_len = usize::from(input.take(1)?[0]);
            let language = String::from_utf8(input.take(language_len)?.to_vec())
                .map_err(|_| RecordError::Invalid("language is not UTF-8"))?;
            if paths
                .last()
                .is_some_and(|last: &PathRecord| last.path >= path)
            {
                return Err(RecordError::Invalid("paths out of order"));
            }
            paths.push(PathRecord {
                path,
                mode,
                content,
                stat,
                language,
            });
        }
        input.finish()?;
        Ok(Self {
            git,
            conversion,
            paths,
        })
    }
}

fn put_optional(out: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(value) => {
            out.push(1);
            out.extend_from_slice(&value.to_le_bytes());
        }
        None => out.push(0),
    }
}

fn optional(input: &mut Payload<'_>) -> Result<Option<u64>, RecordError> {
    match input.take(1)?[0] {
        0 => Ok(None),
        1 => Ok(Some(input.u64()?)),
        _ => Err(RecordError::Invalid("optional field flag")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str, content: &str) -> PathRecord {
        PathRecord {
            path: path.as_bytes().to_vec(),
            mode: 0o100_644,
            content: ContentKey::of(content.as_bytes()),
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
}
