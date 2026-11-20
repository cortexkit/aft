//! Bounded, read-only accounting of the physical files in a family.

use std::collections::HashSet;
use std::fs::{self, DirEntry, ReadDir};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use super::{Layout, MANIFEST_PREFIX, Manifest, canonical_number, record};
use crate::ids::{FamilyId, Generation, SegmentId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskUsageStop {
    EntryCap,
    Deadline,
    UnreadableDirectory,
    UnreadableEntry,
    UnreadableManifest,
}

/// A best-effort byte count. If `stop` is set, the scan did not finish.
#[derive(Clone, Copy, Debug)]
pub struct DiskUsage {
    pub bytes: u64,
    pub examined: usize,
    pub stop: Option<DiskUsageStop>,
    reuse: Option<ReuseKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReuseKey {
    generation: Generation,
    root_entries: usize,
    stamps: DirectoryStamps,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirectoryStamps {
    family: SystemTime,
    builds: Option<SystemTime>,
    tmp: Option<SystemTime>,
    segments: Option<SystemTime>,
}

impl DirectoryStamps {
    fn read(layout: &Layout) -> Option<Self> {
        fn modified(path: &Path) -> Option<Option<SystemTime>> {
            match fs::symlink_metadata(path) {
                Ok(meta) => meta.modified().ok().map(Some),
                Err(error) if error.kind() == ErrorKind::NotFound => Some(None),
                Err(_) => None,
            }
        }
        // Directory timestamps are hints for changes outside the manifest,
        // not evidence that no new generation has been published.
        Some(Self {
            family: fs::metadata(&layout.dir).ok()?.modified().ok()?,
            builds: modified(&layout.dir.join("builds"))?,
            tmp: modified(&layout.tmp_dir())?,
            segments: modified(&layout.segments_dir())?,
        })
    }
}

/// Measures a family without opening a snapshot or taking a lease. Published
/// lengths come from the newest readable manifest; other files are measured
/// under the same entry and deadline limits as manifest discovery. Returns
/// `None` to reuse `previous` when bounded discovery finds the same published
/// generation and the root entry count and directory stamps still match. Pass
/// a previous sample only for the same family.
#[must_use]
pub fn measure_disk_usage(
    dir: &Path,
    family: FamilyId,
    previous: Option<DiskUsage>,
    entry_cap: usize,
    deadline: Instant,
) -> Option<DiskUsage> {
    let layout = Layout::new(dir);
    let stamps = DirectoryStamps::read(&layout);
    measure_with_stamps(&layout, family, previous, entry_cap, deadline, stamps)
}

fn measure_with_stamps(
    layout: &Layout,
    family: FamilyId,
    previous: Option<DiskUsage>,
    entry_cap: usize,
    deadline: Instant,
    stamps: Option<DirectoryStamps>,
) -> Option<DiskUsage> {
    let mut scan = Scan {
        bytes: 0,
        examined: 0,
        entry_cap,
        deadline,
    };
    let (stop, reuse) = match scan.read_root(layout) {
        Ok(root) => {
            // Manifest names are immutable and numbered. When the highest name
            // still matches a generation we validated, there's no newer
            // publication to open. A higher name that could not be validated
            // may still be in the middle of being written.
            let highest = root
                .manifests
                .iter()
                .map(|(number, _)| *number)
                .max()
                .map_or(Generation::EMPTY, Generation::new);
            let reuse = stamps.map(|stamps| ReuseKey {
                generation: highest,
                root_entries: root.entries.len(),
                stamps,
            });
            if reuse.is_some() && previous.is_some_and(|old| old.reuse == reuse) {
                return None;
            }
            match scan.manifest(family, root.manifests) {
                Ok((manifest, manifest_bytes)) => {
                    let reuse = reuse.filter(|key| key.generation == manifest.generation());
                    (
                        scan.walk(layout, root.entries, &manifest, manifest_bytes)
                            .err(),
                        reuse,
                    )
                }
                Err(stop) => (Some(stop), None),
            }
        }
        Err(stop) => (Some(stop), None),
    };
    // I/O failures may be transient even when the directory stamps don't move.
    let reuse = match stop {
        None | Some(DiskUsageStop::EntryCap | DiskUsageStop::Deadline) => reuse,
        _ => None,
    };
    Some(DiskUsage {
        bytes: scan.bytes,
        examined: scan.examined,
        stop,
        reuse,
    })
}

struct Scan {
    bytes: u64,
    examined: usize,
    entry_cap: usize,
    deadline: Instant,
}

struct RootEntries {
    entries: Vec<DirEntry>,
    manifests: Vec<(u64, PathBuf)>,
}

impl Scan {
    fn read_root(&mut self, layout: &Layout) -> Result<RootEntries, DiskUsageStop> {
        let mut entries = self.open_dir(&layout.dir)?;
        let mut root = Vec::new();
        let mut manifests = Vec::new();
        while let Some(entry) = self.next_entry(&mut entries)? {
            if let Some(number) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix(MANIFEST_PREFIX))
                .and_then(canonical_number)
                && file_type(&entry)?.is_file()
            {
                manifests.push((number, entry.path()));
            }
            root.push(entry);
        }
        Ok(RootEntries {
            entries: root,
            manifests,
        })
    }

    fn walk(
        &mut self,
        layout: &Layout,
        entries: Vec<DirEntry>,
        manifest: &Manifest,
        manifest_bytes: u64,
    ) -> Result<(), DiskUsageStop> {
        self.bytes = manifest_bytes;
        for segment in manifest.segments() {
            self.add(segment.len);
        }
        let mut covered_states = HashSet::new();
        for state in manifest.checkouts() {
            covered_states.insert(layout.state(state.checkout, state.number));
            self.add(record::encoded_len(state.len));
        }
        let current_manifest = layout.manifest(manifest.generation());
        let segments = layout.segments_dir();
        let mut pending = Vec::new();
        for entry in entries {
            self.check_deadline()?;
            if manifest_bytes == 0 || entry.path() != current_manifest {
                self.measure_entry(entry, &mut pending)?;
            }
        }
        while let Some(dir) = pending.pop() {
            let mut entries = self.open_dir(&dir)?;
            while let Some(entry) = self.next_entry(&mut entries)? {
                if (dir == segments && published_segment(&entry, manifest))
                    || (!covered_states.is_empty() && covered_states.contains(&entry.path()))
                {
                    continue;
                }
                self.measure_entry(entry, &mut pending)?;
            }
        }
        Ok(())
    }

    fn manifest(
        &self,
        family: FamilyId,
        mut manifests: Vec<(u64, PathBuf)>,
    ) -> Result<(Manifest, u64), DiskUsageStop> {
        manifests.sort_unstable_by_key(|(number, _)| std::cmp::Reverse(*number));
        for (number, path) in manifests {
            self.check_deadline()?;
            match read_manifest(&path, family, number) {
                Ok(decoded) => return Ok(decoded),
                Err(record::RecordError::Invalid(_)) => {}
                Err(record::RecordError::Io(error)) if error.kind() == ErrorKind::NotFound => {}
                Err(_) => return Err(DiskUsageStop::UnreadableManifest),
            }
        }
        Ok((Manifest::empty(), 0))
    }

    fn check_deadline(&self) -> Result<(), DiskUsageStop> {
        if Instant::now() >= self.deadline {
            return Err(DiskUsageStop::Deadline);
        }
        Ok(())
    }

    fn check_budget(&self) -> Result<(), DiskUsageStop> {
        if self.examined >= self.entry_cap {
            return Err(DiskUsageStop::EntryCap);
        }
        self.check_deadline()
    }

    fn open_dir(&self, path: &Path) -> Result<ReadDir, DiskUsageStop> {
        self.check_budget()?;
        fs::read_dir(path).map_err(|_| DiskUsageStop::UnreadableDirectory)
    }

    fn next_entry(&mut self, entries: &mut ReadDir) -> Result<Option<DirEntry>, DiskUsageStop> {
        // Don't request another entry merely to find out if the limit landed
        // on EOF. Without observing EOF, the count must be marked partial.
        self.check_budget()?;
        let entry = entries.next();
        self.examined += usize::from(entry.is_some());
        entry
            .transpose()
            .map_err(|_| DiskUsageStop::UnreadableEntry)
    }

    fn measure_entry(
        &mut self,
        entry: DirEntry,
        pending: &mut Vec<PathBuf>,
    ) -> Result<(), DiskUsageStop> {
        let kind = file_type(&entry)?;
        if kind.is_dir() {
            pending.push(entry.path());
        } else if kind.is_file() {
            let meta = entry
                .metadata()
                .map_err(|_| DiskUsageStop::UnreadableEntry)?;
            self.add(meta.len());
        }
        Ok(())
    }

    fn add(&mut self, len: u64) {
        self.bytes = self.bytes.saturating_add(len);
    }
}

fn file_type(entry: &DirEntry) -> Result<fs::FileType, DiskUsageStop> {
    entry
        .file_type()
        .map_err(|_| DiskUsageStop::UnreadableEntry)
}

pub(super) fn read_manifest(
    path: &Path,
    family: FamilyId,
    number: u64,
) -> Result<(Manifest, u64), record::RecordError> {
    let payload = record::read(path, record::Kind::Manifest, family, number)?;
    let manifest = Manifest::decode(Generation::new(number), &payload)?;
    Ok((manifest, record::encoded_len(payload.len() as u64)))
}

fn published_segment(entry: &DirEntry, manifest: &Manifest) -> bool {
    entry
        .file_name()
        .to_str()
        .and_then(|name| name.strip_suffix(".seg"))
        .and_then(canonical_number)
        .is_some_and(|id| manifest.segment(SegmentId::new(id)).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::family::{Family, Publication};
    use crate::ids::Digest128;

    #[test]
    fn disk_usage_detects_a_new_generation_even_with_identical_directory_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let id = FamilyId::from_digest(Digest128::of(b"disk usage test"));
        let family = Family::open(dir.path(), id).unwrap();
        let mut writer = family.try_write().unwrap().unwrap();
        let first = writer.publish(&Publication::default()).unwrap();
        let layout = Layout::new(dir.path());
        let stamps = DirectoryStamps::read(&layout).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        let sample =
            |previous, cap| measure_with_stamps(&layout, id, previous, cap, deadline, Some(stamps));
        let previous = sample(None, 256).unwrap();
        assert_eq!(previous.stop, None);
        assert!(sample(Some(previous), 256).is_none());
        let capped = sample(Some(previous), 1).unwrap();
        assert_eq!(capped.stop, Some(DiskUsageStop::EntryCap));
        assert_eq!(capped.examined, 1);

        let second = writer.publish(&Publication::default()).unwrap();
        // Replace one manifest with another so the root count also matches.
        fs::remove_file(layout.manifest(first)).unwrap();
        let next = sample(Some(previous), 256).unwrap();
        assert_eq!(next.stop, None);
        assert_eq!(next.reuse.unwrap().generation, second);
        assert_eq!(
            next.reuse.unwrap().root_entries,
            previous.reuse.unwrap().root_entries
        );
    }

    #[test]
    fn disk_usage_does_not_reuse_a_sample_taken_during_manifest_write() {
        let dir = tempfile::tempdir().unwrap();
        let id = FamilyId::from_digest(Digest128::of(b"disk usage test"));
        let layout = Layout::new(dir.path());
        let generation = Generation::new(1);
        let path = layout.manifest(generation);
        fs::write(&path, b"incomplete").unwrap();
        let stamps = DirectoryStamps::read(&layout).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        let sample = |previous| {
            measure_with_stamps(&layout, id, previous, 256, deadline, Some(stamps)).unwrap()
        };
        let previous = sample(None);
        assert_eq!(previous.bytes, 10);

        let manifest = Manifest::new(generation, 0, SegmentId::new(1), vec![], vec![]);
        let record = record::encode(
            record::Kind::Manifest,
            id,
            generation.get(),
            &manifest.encode(),
        );
        fs::write(path, &record).unwrap();
        let next = sample(Some(previous));
        assert_eq!(next.stop, None);
        assert_eq!(next.bytes, record.len() as u64);
    }
}
