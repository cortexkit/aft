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
    stamp: Option<Stamp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    family: SystemTime,
    builds: Option<SystemTime>,
    tmp: Option<SystemTime>,
    segments: Option<SystemTime>,
}

impl Stamp {
    fn read(layout: &Layout) -> Option<Self> {
        fn modified(path: &Path) -> Option<Option<SystemTime>> {
            match fs::symlink_metadata(path) {
                Ok(meta) => meta.modified().ok().map(Some),
                Err(error) if error.kind() == ErrorKind::NotFound => Some(None),
                Err(_) => None,
            }
        }
        // Publishing a manifest adds a new immutable name to the family root.
        // An unchanged root stamp means no new manifest has been published.
        // Scratch creation/removal and segment reclamation can happen without
        // publishing, so check those directories before reusing the sample.
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
/// `None` to reuse `previous` when its directory stamps still match. Pass a
/// previous sample only for the same family.
#[must_use]
pub fn measure_disk_usage(
    dir: &Path,
    family: FamilyId,
    previous: Option<DiskUsage>,
    entry_cap: usize,
    deadline: Instant,
) -> Option<DiskUsage> {
    let layout = Layout::new(dir);
    let stamp = Stamp::read(&layout);
    if stamp.is_some() && previous.is_some_and(|old| old.stamp == stamp) {
        return None;
    }
    let mut scan = Scan {
        bytes: 0,
        examined: 0,
        entry_cap,
        deadline,
    };
    let stop = scan.run(&layout, family).err();
    // I/O failures may be transient even when the directory stamps don't move.
    let stamp = match stop {
        None | Some(DiskUsageStop::EntryCap | DiskUsageStop::Deadline) => stamp,
        _ => None,
    };
    Some(DiskUsage {
        bytes: scan.bytes,
        examined: scan.examined,
        stop,
        stamp,
    })
}

struct Scan {
    bytes: u64,
    examined: usize,
    entry_cap: usize,
    deadline: Instant,
}

impl Scan {
    fn run(&mut self, layout: &Layout, family: FamilyId) -> Result<(), DiskUsageStop> {
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
        drop(entries);
        let (manifest, manifest_bytes) = self.manifest(family, manifests)?;
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
        for entry in root {
            self.check_deadline()?;
            if manifest_bytes == 0 || entry.path() != current_manifest {
                self.measure_entry(entry, &mut pending)?;
            }
        }
        while let Some(dir) = pending.pop() {
            let mut entries = self.open_dir(&dir)?;
            while let Some(entry) = self.next_entry(&mut entries)? {
                if (dir == segments && published_segment(&entry, &manifest))
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
