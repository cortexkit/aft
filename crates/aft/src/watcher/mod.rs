//! Minimal watcher-side collection primitives.
//!
//! Watcher threads may invalidate paths and collect a stable byte snapshot, but
//! never hash, store, or publish artifacts. Those operations belong to plane
//! workers so a burst of watcher events cannot mutate a view directly.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A changed stat may trigger this many additional reads after the first read.
pub const MAX_STABLE_READ_RETRIES: usize = 3;
/// Includes the first read plus every permitted retry.
pub const MAX_STABLE_READ_ATTEMPTS: usize = MAX_STABLE_READ_RETRIES + 1;

/// The metadata fields that make a read safe to hand to a plane worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileStamp {
    pub size: u64,
    pub modified_ns: Option<u128>,
    #[cfg(unix)]
    pub inode: u64,
    #[cfg(unix)]
    pub ctime_ns: i128,
}

impl FileStamp {
    pub fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            size: metadata.len(),
            modified_ns: system_time_ns(metadata.modified().ok()),
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                metadata.ino()
            },
            #[cfg(unix)]
            ctime_ns: {
                use std::os::unix::fs::MetadataExt;
                i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec())
            },
        }
    }

    #[cfg(test)]
    fn synthetic(size: u64, modified_ns: u128) -> Self {
        Self {
            size,
            modified_ns: Some(modified_ns),
            #[cfg(unix)]
            inode: 1,
            #[cfg(unix)]
            ctime_ns: 1,
        }
    }
}

fn system_time_ns(time: Option<SystemTime>) -> Option<u128> {
    time.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
}

/// A value whose bytes were bracketed by equal filesystem stamps.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StableRead<T> {
    pub value: T,
    pub stamp: FileStamp,
    pub attempts: usize,
}

#[derive(Debug)]
pub enum StableReadError {
    Io(io::Error),
    /// The file changed around every allowed read. The caller must leave the
    /// path pending and wait for another watcher event rather than publishing it.
    Unstable {
        attempts: usize,
    },
}

impl fmt::Display for StableReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "stable read I/O error: {error}"),
            Self::Unstable { attempts } => {
                write!(
                    f,
                    "file changed around all {attempts} stable-read attempt(s)"
                )
            }
        }
    }
}

impl std::error::Error for StableReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Unstable { .. } => None,
        }
    }
}

impl From<io::Error> for StableReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Reads `path` only when an identical stat surrounds the read.
pub fn read_stable_file(path: &Path) -> Result<StableRead<Vec<u8>>, StableReadError> {
    read_stable(
        || Ok(FileStamp::from_metadata(&fs::metadata(path)?)),
        || fs::read(path),
    )
}

/// Performs the stable-read algorithm with injectable stat and read operations.
///
/// The plane worker owns this operation because it is the component that will
/// hash the collected bytes next. Watcher dispatch only queues the path.
pub fn read_stable<T>(
    mut stat: impl FnMut() -> io::Result<FileStamp>,
    mut read: impl FnMut() -> io::Result<T>,
) -> Result<StableRead<T>, StableReadError> {
    for retry in 0..=MAX_STABLE_READ_RETRIES {
        let before = stat()?;
        let value = read()?;
        let after = stat()?;
        if before == after {
            return Ok(StableRead {
                value,
                stamp: after,
                attempts: retry + 1,
            });
        }
    }
    Err(StableReadError::Unstable {
        attempts: MAX_STABLE_READ_ATTEMPTS,
    })
}

/// The watcher-owned queue of invalidations. It deliberately has no hashing,
/// blob-store, or publication API; plane workers consume its collected paths.
#[derive(Debug, Default)]
pub struct WatcherCollector {
    invalidated_paths: BTreeSet<PathBuf>,
}

impl WatcherCollector {
    pub fn invalidate(&mut self, path: impl Into<PathBuf>) {
        self.invalidated_paths.insert(path.into());
    }

    /// Drains one deduplicated collection batch. A later watcher event can add a
    /// persistently unstable path again without special recovery state.
    pub fn take_collected(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.invalidated_paths)
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_mismatch_gets_three_retries_then_is_left_unstable() {
        let mut stat_calls = 0;
        let mut read_calls = 0;
        let result = read_stable(
            || {
                stat_calls += 1;
                let stamp = if stat_calls % 2 == 1 {
                    FileStamp::synthetic(1, stat_calls as u128)
                } else {
                    FileStamp::synthetic(2, stat_calls as u128)
                };
                Ok(stamp)
            },
            || {
                read_calls += 1;
                Ok::<_, io::Error>(b"read".to_vec())
            },
        );

        assert!(matches!(
            result,
            Err(StableReadError::Unstable {
                attempts: MAX_STABLE_READ_ATTEMPTS
            })
        ));
        assert_eq!(read_calls, MAX_STABLE_READ_ATTEMPTS);
        assert_eq!(stat_calls, MAX_STABLE_READ_ATTEMPTS * 2);
    }

    #[test]
    fn third_retry_can_produce_a_stable_snapshot() {
        let stable = FileStamp::synthetic(9, 12);
        let changing = FileStamp::synthetic(9, 13);
        let mut stamps = vec![
            stable.clone(),
            changing.clone(),
            stable.clone(),
            changing.clone(),
            stable.clone(),
            changing,
            stable.clone(),
            stable.clone(),
        ]
        .into_iter();
        let result = read_stable(
            || Ok(stamps.next().expect("one stamp per stat")),
            || Ok::<_, io::Error>(b"stable bytes".to_vec()),
        )
        .expect("third retry succeeds");

        assert_eq!(result.value, b"stable bytes");
        assert_eq!(result.attempts, MAX_STABLE_READ_ATTEMPTS);
        assert_eq!(result.stamp, stable);
    }

    #[test]
    fn watcher_collector_only_deduplicates_and_drains_paths() {
        let mut collector = WatcherCollector::default();
        collector.invalidate("src/z.rs");
        collector.invalidate("src/a.rs");
        collector.invalidate("src/z.rs");
        assert_eq!(
            collector.take_collected(),
            vec![PathBuf::from("src/a.rs"), PathBuf::from("src/z.rs")]
        );
        assert!(collector.take_collected().is_empty());
    }
}
