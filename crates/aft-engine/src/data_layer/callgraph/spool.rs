//! The spool that keeps each file's pass-2/3 resolution on disk until pass 4
//! has read every file's.
//!
//! It writes into its own `runs/` folder of the build's shared workspace
//! ([`BuildScratch`]), which owns the folder's deletion, the checkout's build
//! lock and disk admission. Each spooled range is a checkpoint file (see
//! [`resume`]): its header names the build, the scanned inputs it was
//! resolved from and the paths it covers, its trailer its length and
//! checksum. It is written once under a name derived from those paths,
//! closed and renamed before anything refers to it, and checked against its
//! length and checksum when read back. Once every range is written, the
//! spool completes the build's `passes-2-3` stage ([`ResolutionSpool::commit`]),
//! and a later attempt of the same build can adopt it
//! ([`ResolutionSpool::adopt`]) instead of resolving passes 2-3 again.

use std::path::PathBuf;
use std::sync::Arc;

use aft_store::family::{BuildScratch, ScratchDir};

use super::super::checkout::resume::{self, Digest, FileSum, Header, Marker};
use super::super::checkout::DataError;

/// The name of the workspace's folder the spool shares with pass 4's runs.
const FOLDER: &str = "runs";

/// One spooled range: its file and what reading it back must find.
struct SpoolRange {
    path: PathBuf,
    /// Each entry's length, in the order written.
    lengths: Vec<u32>,
    /// The entries' bytes, together.
    total: u64,
    /// The whole file's length and checksum.
    file: FileSum,
}

/// Encoded per-file resolutions, one file per range of files written in
/// order, read back a range at a time.
pub(crate) struct ResolutionSpool {
    dir: ScratchDir<DataError>,
    ranges: Vec<SpoolRange>,
    /// The build's identity digest, and the digest of the scanned inputs
    /// the resolutions were resolved from, which every range names.
    identity: Digest,
    snapshot: Digest,
}

impl ResolutionSpool {
    /// A spool in the `runs/` folder of `workspace`; a workspace holds one.
    /// Its ranges name `identity` and `snapshot`.
    pub(crate) fn new(
        workspace: &Arc<BuildScratch<DataError>>,
        identity: Digest,
        snapshot: Digest,
    ) -> std::io::Result<Self> {
        Ok(Self {
            dir: workspace.subdir_existing(FOLDER)?,
            ranges: Vec::new(),
            identity,
            snapshot,
        })
    }

    /// The spool of an earlier attempt of the same build: the complete
    /// `passes-2-3` stage `marker` lists in `workspace`, whose files were
    /// already checked in full. Each range must cover exactly the paths of
    /// the matching batch of `batches`, in order, and name `identity` and
    /// `snapshot`.
    ///
    /// # Errors
    ///
    /// When a range cannot be read or covers anything else.
    pub(crate) fn adopt<'b>(
        workspace: &Arc<BuildScratch<DataError>>,
        marker: &Marker,
        identity: Digest,
        snapshot: Digest,
        batches: impl ExactSizeIterator<Item = &'b [String]>,
    ) -> std::io::Result<Self> {
        let mut spool = Self::new(workspace, identity, snapshot)?;
        if marker.sums.len() != batches.len() {
            return Err(std::io::Error::other(format!(
                "the spool holds {} ranges for {} batches",
                marker.sums.len(),
                batches.len()
            )));
        }
        for ((name, file), batch) in marker.files().zip(batches) {
            let path = workspace.path().join(name);
            let (header, body) = resume::read_framed(&path)?;
            let members = batch
                .iter()
                .map(|path| path.as_bytes().to_vec())
                .collect::<Vec<_>>();
            if header
                != (Header {
                    identity,
                    snapshot: Some(snapshot),
                    stage: resume::PASSES.to_string(),
                    members,
                })
            {
                return Err(std::io::Error::other(format!(
                    "{name} covers other files than its batch"
                )));
            }
            let (lengths, total) = lengths(&body, batch.len())?;
            spool.ranges.push(SpoolRange {
                path,
                lengths,
                total,
                file,
            });
        }
        Ok(spool)
    }

    /// The spool's `runs/` folder, which other pass-4 runs share.
    pub(crate) fn dir(&self) -> &ScratchDir<DataError> {
        &self.dir
    }

    /// Writes `entries`, the resolutions of `paths` in that order, as the
    /// next range, returning its number. The workspace's disk admission is
    /// asked first, for the range's bytes.
    pub(crate) fn write_range(
        &mut self,
        paths: &[String],
        entries: &[Vec<u8>],
    ) -> std::io::Result<usize> {
        if paths.len() != entries.len() {
            return Err(std::io::Error::other(
                "a spooled range needs one resolution per path",
            ));
        }
        let header = Header {
            identity: self.identity,
            snapshot: Some(self.snapshot),
            stage: resume::PASSES.to_string(),
            members: paths.iter().map(|path| path.as_bytes().to_vec()).collect(),
        };
        let mut body =
            Vec::with_capacity(entries.iter().map(|entry| entry.len() + 4).sum::<usize>());
        let mut lengths = Vec::with_capacity(entries.len());
        for entry in entries {
            let length = u32::try_from(entry.len())
                .map_err(|_| std::io::Error::other("a spooled resolution is larger than 4 GiB"))?;
            body.extend_from_slice(&length.to_le_bytes());
            body.extend_from_slice(entry);
            lengths.push(length);
        }
        self.dir
            .workspace()
            .check_space(resume::framed_len(&header, body.len()))
            .map_err(std::io::Error::other)?;
        let name = resume::member_name("resolutions", paths.iter().map(|path| path.as_bytes()));
        let path = self.dir.path().join(name);
        // Closed and renamed before the range is listed. Not synced: a
        // file a power loss tore fails its checksum when read back.
        let file = resume::write_framed(&path, &header, &body)?;
        self.ranges.push(SpoolRange {
            path,
            total: lengths.iter().map(|length| u64::from(*length)).sum(),
            lengths,
            file,
        });
        resume::kill_point("passes.range");
        Ok(self.ranges.len() - 1)
    }

    /// Completes the build's `passes-2-3` stage with a marker listing every
    /// range written, in order.
    ///
    /// # Errors
    ///
    /// When admission refuses, or the marker cannot be written.
    pub(crate) fn commit(&self) -> Result<(), DataError> {
        let workspace = self.dir.workspace();
        let files = self
            .ranges
            .iter()
            .map(|range| {
                let name = range
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (format!("{FOLDER}/{name}"), range.file)
            })
            .collect::<Vec<_>>();
        resume::write_marker(
            workspace,
            resume::PASSES,
            self.identity,
            Some(self.snapshot),
            &files,
        )?;
        resume::kill_point("passes.committed");
        Ok(())
    }

    /// The entries of range `index`, in the order written. Fails when the
    /// file is not exactly as written.
    pub(crate) fn read_range(&self, index: usize) -> std::io::Result<Vec<Vec<u8>>> {
        let range = self
            .ranges
            .get(index)
            .ok_or_else(|| std::io::Error::other(format!("no spooled range {index}")))?;
        let bytes = std::fs::read(&range.path)?;
        if bytes.len() as u64 != range.file.len
            || blake3::hash(&bytes).as_bytes() != &range.file.checksum
        {
            return Err(std::io::Error::other(format!(
                "the spooled resolutions in {} changed after they were written",
                range.path.display()
            )));
        }
        let (_, body) = resume::parse_framed(&range.path, &bytes)?;
        let mut reader = resume::Reader::new(&body);
        let mut entries = Vec::with_capacity(range.lengths.len());
        for length in &range.lengths {
            if reader.u32()? != *length {
                return Err(std::io::Error::other("a spooled range is malformed"));
            }
            entries.push(reader.take(*length as usize)?.to_vec());
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

/// Each entry's length in a range's `body`, and their total; the range must
/// hold `count` entries.
fn lengths(body: &[u8], count: usize) -> std::io::Result<(Vec<u32>, u64)> {
    let mut reader = resume::Reader::new(body);
    let mut lengths = Vec::with_capacity(count);
    let mut total = 0u64;
    for _ in 0..count {
        let length = reader.u32()?;
        reader.take(length as usize)?;
        lengths.push(length);
        total += u64::from(length);
    }
    if !reader.is_done() {
        return Err(std::io::Error::other(
            "a spooled range holds more resolutions than its paths",
        ));
    }
    Ok((lengths, total))
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

    fn paths(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn a_spooled_range_reads_back_as_written_and_a_changed_one_fails() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path(), |_| Ok(()));
        let dir = workspace.path().to_path_buf();
        let mut spool = ResolutionSpool::new(&workspace, [1; 32], [2; 32]).unwrap();
        let first = spool
            .write_range(
                &paths(&["a", "b", "c"]),
                &[b"one".to_vec(), Vec::new(), b"three".to_vec()],
            )
            .unwrap();
        let second = spool
            .write_range(&paths(&["d"]), &[b"four".to_vec()])
            .unwrap();
        assert_eq!(
            spool.ranges[second].path,
            dir.join("runs")
                .join(resume::member_name("resolutions", [b"d".as_slice()]))
        );
        assert_eq!(
            spool.read_range(first).unwrap(),
            [b"one".to_vec(), Vec::new(), b"three".to_vec()]
        );
        assert_eq!(spool.read_range(second).unwrap(), [b"four".to_vec()]);
        assert_eq!(spool.bytes(), 12);
        let written = std::fs::read(&spool.ranges[second].path).unwrap();
        let mut changed = written.clone();
        let at = changed.len() - 41;
        changed[at] ^= 1;
        std::fs::write(&spool.ranges[second].path, &changed).unwrap();
        assert!(spool.read_range(second).is_err());
        std::fs::write(&spool.ranges[second].path, &written[..written.len() - 1]).unwrap();
        assert!(spool.read_range(second).is_err());
        std::fs::write(&spool.ranges[second].path, &written).unwrap();
        drop(spool);
        assert!(
            dir.join("runs").exists(),
            "dropping the spool leaves its files to the workspace's owner"
        );
        workspace.remove().unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn a_committed_spool_is_adopted_only_for_its_own_batches_and_inputs() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path(), |_| Ok(()));
        let batches = [paths(&["a", "b"]), paths(&["c"])];
        let mut spool = ResolutionSpool::new(&workspace, [1; 32], [2; 32]).unwrap();
        spool
            .write_range(&batches[0], &[b"A".to_vec(), b"B".to_vec()])
            .unwrap();
        spool.write_range(&batches[1], &[b"C".to_vec()]).unwrap();
        spool.commit().unwrap();
        drop(spool);
        let marker = resume::read_marker(workspace.path(), resume::PASSES)
            .unwrap()
            .unwrap();
        assert_eq!(marker.header.snapshot, Some([2; 32]));
        let adopt = |identity, snapshot, batches: &[Vec<String>]| {
            ResolutionSpool::adopt(
                &workspace,
                &marker,
                identity,
                snapshot,
                batches.iter().map(Vec::as_slice),
            )
        };
        let adopted = adopt([1; 32], [2; 32], &batches).unwrap();
        assert_eq!(
            adopted.read_range(0).unwrap(),
            [b"A".to_vec(), b"B".to_vec()]
        );
        assert_eq!(adopted.read_range(1).unwrap(), [b"C".to_vec()]);
        assert_eq!(adopted.bytes(), 3);
        drop(adopted);
        assert!(adopt([1; 32], [3; 32], &batches).is_err(), "other inputs");
        assert!(adopt([4; 32], [2; 32], &batches).is_err(), "another build");
        assert!(
            adopt([1; 32], [2; 32], &[paths(&["a"]), paths(&["b", "c"])]).is_err(),
            "other batches"
        );
        assert!(
            adopt([1; 32], [2; 32], &batches[..1]).is_err(),
            "fewer batches"
        );
        workspace.remove().unwrap();
    }

    #[test]
    fn a_range_the_disk_floor_refuses_is_not_written() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path(), |bytes| {
            if bytes <= 400 {
                Ok(())
            } else {
                Err(DataError::DiskFloor("below the floor".to_string()))
            }
        });
        let mut spool = ResolutionSpool::new(&workspace, [1; 32], [2; 32]).unwrap();
        spool
            .write_range(&paths(&["a"]), &[b"four".to_vec()])
            .unwrap();
        let error = spool
            .write_range(&paths(&["b"]), &[vec![0; 500]])
            .unwrap_err();
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
