//! A cold build's scratch workspace, and the spool that keeps each file's
//! pass-2/3 resolution on disk until pass 4 has read every file's.
//!
//! The workspace is `<family>/builds/<checkout>/<build-id>/`, outside the
//! writer's `tmp/`, with an `owner` file the build keeps locked while it
//! lives, so a later sweep can tell a dead build's workspace from a live
//! one. Spooled files go under its `runs/`, each written once under a new
//! name, closed before anything refers to it, and checked against its
//! length and checksum when read back.

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

/// A cold build's scratch folder, deleted with everything in it when the
/// build drops it.
pub(crate) struct BuildWorkspace {
    dir: PathBuf,
    /// Locked for the workspace's life; dropped before the folder is
    /// deleted, since Windows deletes no open file.
    owner: Option<File>,
    next_run: std::sync::atomic::AtomicU64,
}

impl BuildWorkspace {
    /// A new workspace for one build of `checkout` in the store at
    /// `family_dir`.
    pub(crate) fn create(family_dir: &Path, checkout: &str) -> std::io::Result<Self> {
        Self::create_in(&family_dir.join("builds").join(checkout))
    }

    /// A new workspace for a build that has no store, such as a gate's or a
    /// test's, in the system's temporary folder.
    pub(crate) fn temporary() -> std::io::Result<Self> {
        Self::create_in(&std::env::temp_dir().join("aft-callgraph-builds"))
    }

    fn create_in(parent: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(parent)?;
        let dir = parent.join(fresh_id());
        std::fs::create_dir(&dir)?;
        std::fs::create_dir(dir.join("runs"))?;
        let owner = File::create_new(dir.join("owner"))?;
        if let Err(error) = owner.try_lock() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(std::io::Error::other(format!(
                "the build workspace {} could not be locked: {error}",
                dir.display()
            )));
        }
        Ok(Self {
            dir,
            owner: Some(owner),
            next_run: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// A path under `runs/` no file of this workspace has had.
    fn new_run(&self, kind: &str) -> PathBuf {
        let number = self
            .next_run
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.dir.join("runs").join(format!("{kind}-{number}"))
    }
}

impl Drop for BuildWorkspace {
    fn drop(&mut self) {
        drop(self.owner.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A name no other workspace of the checkout has: the process, the time and
/// a counter, hashed.
fn fresh_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    hasher.update(&now.as_nanos().to_le_bytes());
    hasher.update(
        &COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_le_bytes(),
    );
    hasher.finalize().to_hex()[..16].to_string()
}

/// One spooled range: its file and what reading it back must find.
struct SpoolRange {
    path: PathBuf,
    /// Each entry's length, in the order written.
    lengths: Vec<u32>,
    total: u64,
    checksum: blake3::Hash,
}

/// Encoded per-file resolutions, one file per range of files written in
/// order, read back a range at a time.
pub(crate) struct ResolutionSpool<'w> {
    workspace: &'w BuildWorkspace,
    ranges: Vec<SpoolRange>,
}

impl<'w> ResolutionSpool<'w> {
    pub(crate) fn new(workspace: &'w BuildWorkspace) -> Self {
        Self {
            workspace,
            ranges: Vec::new(),
        }
    }

    /// Writes `entries` as the next range, returning its number.
    pub(crate) fn write_range(&mut self, entries: &[Vec<u8>]) -> std::io::Result<usize> {
        let path = self.workspace.new_run("resolutions");
        let mut hasher = blake3::Hasher::new();
        let mut lengths = Vec::with_capacity(entries.len());
        let mut total = 0u64;
        {
            let mut out = BufWriter::new(File::create_new(&path)?);
            for entry in entries {
                let length = u32::try_from(entry.len()).map_err(|_| {
                    std::io::Error::other("a spooled resolution is larger than 4 GiB")
                })?;
                out.write_all(entry)?;
                hasher.update(entry);
                lengths.push(length);
                total += u64::from(length);
            }
            // Closed before the range is listed. Not synced: the spool is
            // rebuilt from scratch by a build that did not finish.
            drop(out.into_inner().map_err(|error| error.into_error())?);
        }
        self.ranges.push(SpoolRange {
            path,
            lengths,
            total,
            checksum: hasher.finalize(),
        });
        Ok(self.ranges.len() - 1)
    }

    /// The entries of range `index`, in the order written. Fails when the
    /// file is not exactly as written.
    pub(crate) fn read_range(&self, index: usize) -> std::io::Result<Vec<Vec<u8>>> {
        let range = self
            .ranges
            .get(index)
            .ok_or_else(|| std::io::Error::other(format!("no spooled range {index}")))?;
        let bytes = std::fs::read(&range.path)?;
        if bytes.len() as u64 != range.total || blake3::hash(&bytes) != range.checksum {
            return Err(std::io::Error::other(format!(
                "the spooled resolutions in {} changed after they were written",
                range.path.display()
            )));
        }
        let mut entries = Vec::with_capacity(range.lengths.len());
        let mut at = 0usize;
        for length in &range.lengths {
            let end = at + *length as usize;
            entries.push(bytes[at..end].to_vec());
            at = end;
        }
        Ok(entries)
    }

    /// The bytes written so far, over every range.
    pub(crate) fn bytes(&self) -> u64 {
        self.ranges.iter().map(|range| range.total).sum()
    }

    /// The memory the spool's own table holds: each range's path and one
    /// length per file.
    pub(crate) fn table_bytes(&self) -> usize {
        self.ranges
            .iter()
            .map(|range| {
                std::mem::size_of::<SpoolRange>()
                    + range.path.as_os_str().len()
                    + range.lengths.capacity() * std::mem::size_of::<u32>()
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spooled_range_reads_back_as_written_and_a_changed_one_fails() {
        let workspace = BuildWorkspace::temporary().unwrap();
        let dir = workspace.dir.clone();
        assert!(dir.join("owner").is_file());
        let mut spool = ResolutionSpool::new(&workspace);
        let first = spool
            .write_range(&[b"one".to_vec(), Vec::new(), b"three".to_vec()])
            .unwrap();
        let second = spool.write_range(&[b"four".to_vec()]).unwrap();
        assert_eq!(
            spool.read_range(first).unwrap(),
            [b"one".to_vec(), Vec::new(), b"three".to_vec()]
        );
        assert_eq!(spool.read_range(second).unwrap(), [b"four".to_vec()]);
        assert_eq!(spool.bytes(), 12);
        std::fs::write(&spool.ranges[second].path, b"fous").unwrap();
        assert!(spool.read_range(second).is_err());
        std::fs::write(&spool.ranges[second].path, b"four!").unwrap();
        assert!(spool.read_range(second).is_err());
        drop(spool);
        drop(workspace);
        assert!(!dir.exists());
    }
}
