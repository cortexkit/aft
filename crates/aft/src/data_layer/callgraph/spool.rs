//! The spool that keeps each file's pass-2/3 resolution on disk until pass 4
//! has read every file's.
//!
//! It writes into its own `runs/` folder of the build's shared workspace
//! ([`BuildScratch`]), which owns the folder's deletion, the checkout's build
//! lock and disk admission. Each spooled file is written once under a new name, closed
//! before anything refers to it, and checked against its length and
//! checksum when read back.

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::PathBuf;
use std::sync::Arc;

use aft_store::family::{BuildScratch, ScratchDir};

use super::super::checkout::DataError;

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
pub(crate) struct ResolutionSpool {
    dir: ScratchDir<DataError>,
    ranges: Vec<SpoolRange>,
}

impl ResolutionSpool {
    /// A spool in a new `runs/` folder of `workspace`; a workspace holds one.
    pub(crate) fn new(workspace: &Arc<BuildScratch<DataError>>) -> std::io::Result<Self> {
        Ok(Self {
            dir: workspace.subdir("runs")?,
            ranges: Vec::new(),
        })
    }

    /// Writes `entries` as the next range, returning its number. The
    /// workspace's disk admission is asked first, for the range's bytes.
    pub(crate) fn write_range(&mut self, entries: &[Vec<u8>]) -> std::io::Result<usize> {
        let bytes = entries.iter().map(|entry| entry.len() as u64).sum();
        self.dir
            .workspace()
            .check_space(bytes)
            .map_err(std::io::Error::other)?;
        let path = self
            .dir
            .path()
            .join(format!("resolutions-{}", self.ranges.len() + 1));
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

    fn workspace(
        dir: &std::path::Path,
        admit: impl Fn(u64) -> Result<(), DataError> + Send + Sync + 'static,
    ) -> Arc<BuildScratch<DataError>> {
        Arc::new(BuildScratch::create_unattached(dir, admit).unwrap())
    }

    #[test]
    fn a_spooled_range_reads_back_as_written_and_a_changed_one_fails() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path(), |_| Ok(()));
        let dir = workspace.path().to_path_buf();
        let mut spool = ResolutionSpool::new(&workspace).unwrap();
        let first = spool
            .write_range(&[b"one".to_vec(), Vec::new(), b"three".to_vec()])
            .unwrap();
        let second = spool.write_range(&[b"four".to_vec()]).unwrap();
        assert_eq!(
            spool.ranges[second].path,
            dir.join("runs").join("resolutions-2")
        );
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
        assert!(
            dir.join("runs").exists(),
            "dropping the spool leaves its files to the workspace's owner"
        );
        workspace.remove().unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn a_range_the_disk_floor_refuses_is_not_written() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path(), |bytes| {
            if bytes <= 4 {
                Ok(())
            } else {
                Err(DataError::DiskFloor("below the floor".to_string()))
            }
        });
        let mut spool = ResolutionSpool::new(&workspace).unwrap();
        spool.write_range(&[b"four".to_vec()]).unwrap();
        let error = spool.write_range(&[b"five!".to_vec()]).unwrap_err();
        assert!(error.to_string().contains("below the floor"), "{error}");
        assert_eq!(spool.ranges.len(), 1);
        assert_eq!(
            std::fs::read_dir(workspace.path().join("runs"))
                .unwrap()
                .count(),
            1,
            "only the admitted range is on disk"
        );
        workspace.remove().unwrap();
    }
}
