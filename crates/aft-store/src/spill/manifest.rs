//! Immutable manifest generations avoid rename-over differences across volumes.
//! Only a synced `.ready` generation publishes runs; `.part` files are ignored.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::memory::{Buffer, Memory};
use super::run::{RunMeta, disk_add};
use super::{Error, corrupt};
use crate::Digest128;

const MARKER: &[u8; 8] = b"AFTSPILL";
const MANIFEST: &[u8; 8] = b"AFTSPMF2";
const PREFIX: &str = "aft-spill-";

#[derive(Debug)]
pub(super) struct Workspace {
    pub(super) guard: Option<super::WriteGuard>,
    pub(super) path: PathBuf,
    pub(super) next_id: u64,
    pub(super) retain: bool,
    generation: u64,
    memory: Memory,
}

impl Workspace {
    pub(super) fn create(root: &Path, memory: &Memory) -> Result<Self, Error> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Config("system clock before Unix epoch"))?
            .as_nanos();
        let path = loop {
            let path = root.join(format!(
                "{PREFIX}{}-{time}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let builder = fs::DirBuilder::new();
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = builder;
                builder.mode(0o700);
                builder
            };
            match builder.create(&path) {
                Ok(()) => break path,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        let mut this = Self {
            guard: None,
            path,
            next_id: 0,
            retain: false,
            generation: 0,
            memory: memory.clone(),
        };
        crate::fs::sync_parent(&this.path)?;
        let marker = this.path.join("owner");
        crate::fs::write_new_durable(&marker, MARKER)?;
        disk_add(memory, MARKER.len() as u64);
        this.publish(&[], 0)?;
        Ok(this)
    }

    pub(super) fn open(path: &Path, memory: &Memory) -> Result<Self, Error> {
        if !marked(path)? {
            return Err(corrupt(path, "not a spill workspace"));
        }
        let this = Self {
            guard: None,
            path: path.to_owned(),
            next_id: 0,
            retain: false,
            generation: 0,
            memory: memory.clone(),
        };
        let mut bytes = 0u64;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                bytes = bytes
                    .checked_add(entry.metadata()?.len())
                    .ok_or(Error::Overflow)?;
            }
        }
        memory.update(|s| {
            s.disk_bytes = bytes;
            s.peak_disk_bytes = bytes;
        });
        Ok(this)
    }

    pub(super) fn publish(&mut self, runs: &[RunMeta], sequence: u64) -> Result<(), Error> {
        if let Some(guard) = &self.guard {
            guard.check()?;
        }
        let next = self.generation.checked_add(1).ok_or(Error::Overflow)?;
        let final_path = self.path.join(format!("manifest-{next:020}.ready"));
        let partial = final_path.with_extension("part");
        let mut file = crate::fs::create_new_private(&partial)?;
        let mut header = [0; 40];
        header[..8].copy_from_slice(MANIFEST);
        header[8..16].copy_from_slice(&sequence.to_le_bytes());
        header[16..24].copy_from_slice(&self.next_id.to_le_bytes());
        header[24..32].copy_from_slice(&(runs.len() as u64).to_le_bytes());
        header[32..40].copy_from_slice(&next.to_le_bytes());
        let mut hash = blake3::Hasher::new();
        self.write(&mut file, &mut hash, &header)?;
        for run in runs {
            self.write(&mut file, &mut hash, &run.encode())?;
        }
        let digest = Digest128::from_hash(&hash.finalize());
        file.write_all(digest.as_bytes())?;
        disk_add(&self.memory, 16);
        crate::sync::sync_all(&file, &partial)?;
        drop(file);
        crate::fs::rename_new_durable(&partial, &final_path)?;
        let old = self.generation;
        self.generation = next;
        if old != 0 {
            self.remove(&self.path.join(format!("manifest-{old:020}.ready")))?;
        }
        Ok(())
    }

    fn write(&self, file: &mut File, hash: &mut blake3::Hasher, bytes: &[u8]) -> Result<(), Error> {
        file.write_all(bytes)?;
        hash.update(bytes);
        disk_add(&self.memory, bytes.len() as u64);
        Ok(())
    }

    pub(super) fn remove(&self, path: &Path) -> Result<(), Error> {
        let len = match fs::symlink_metadata(path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        crate::fs::remove_file(path)?;
        self.memory
            .update(|s| s.disk_bytes = s.disk_bytes.saturating_sub(len));
        Ok(())
    }

    pub(super) fn remove_run(&self, run: &RunMeta) -> Result<(), Error> {
        self.remove(&run.path(&self.path))
    }

    pub(super) fn load(
        &mut self,
        runs: &mut Buffer<RunMeta>,
        sequence: &mut u64,
    ) -> Result<(), Error> {
        let mut latest = 0;
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            if let Some(number) = manifest_number(&entry.file_name().to_string_lossy()) {
                latest = latest.max(number);
            }
        }
        if latest == 0 {
            return Err(corrupt(&self.path, "no finished manifest"));
        }
        let path = self.path.join(format!("manifest-{latest:020}.ready"));
        let mut file = crate::fs::open_read(&path)?;
        let mut header = [0; 40];
        exact(&mut file, &mut header, &path)?;
        if &header[..8] != MANIFEST || u64_at(&header, 32) != latest {
            return Err(corrupt(&path, "manifest header mismatch"));
        }
        let count = usize::try_from(u64_at(&header, 24))
            .map_err(|_| corrupt(&path, "manifest run count overflow"))?;
        if count > runs.capacity() {
            return Err(Error::Budget {
                needed: count.saturating_mul(std::mem::size_of::<RunMeta>()),
                budget: self.memory.stats().budget_bytes,
            });
        }
        let expected_len = (count as u64)
            .checked_mul(56)
            .and_then(|n| n.checked_add(56))
            .ok_or(Error::Overflow)?;
        if file.metadata()?.len() != expected_len {
            return Err(corrupt(&path, "manifest length mismatch"));
        }
        let mut hash = blake3::Hasher::new();
        hash.update(&header);
        self.next_id = u64_at(&header, 16);
        *sequence = u64_at(&header, 8);
        for _ in 0..count {
            let mut bytes = [0; 56];
            exact(&mut file, &mut bytes, &path)?;
            hash.update(&bytes);
            let run = RunMeta::decode(&bytes);
            if run.id >= self.next_id
                || run.level >= 64
                || run.block_bytes == 0
                || run.records == 0
                || run.max_record > run.payload
                || runs.iter().any(|r| r.id == run.id)
            {
                return Err(corrupt(&path, "invalid manifest run metadata"));
            }
            runs.push(run);
        }
        let mut digest = [0; 16];
        exact(&mut file, &mut digest, &path)?;
        if Digest128::from_hash(&hash.finalize()).as_bytes() != &digest {
            return Err(corrupt(&path, "manifest checksum mismatch"));
        }
        self.generation = latest;
        // A crash can leave a completed but unpublished replacement run or a
        // partial write. None belongs to the last committed snapshot.
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let obsolete_manifest = manifest_number(&name).is_some_and(|n| n != latest);
            let orphan = name
                .strip_prefix("run-")
                .and_then(|n| n.strip_suffix(".bin"))
                .and_then(|n| n.parse::<u64>().ok())
                .is_some_and(|id| !runs.iter().any(|r| r.id == id));
            if name.ends_with(".part") || obsolete_manifest || orphan {
                self.remove(&entry.path())?;
            }
        }
        crate::fs::sync_dir(&self.path)?;
        Ok(())
    }
}

fn manifest_number(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("manifest-")?.strip_suffix(".ready")?;
    if digits.len() != 20 || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn u64_at(bytes: &[u8], start: usize) -> u64 {
    u64::from_le_bytes(bytes[start..start + 8].try_into().expect("fixed field"))
}

fn exact(file: &mut File, bytes: &mut [u8], path: &Path) -> Result<(), Error> {
    file.read_exact(bytes).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            corrupt(path, "truncated manifest")
        } else {
            Error::Io(e)
        }
    })
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if !self.retain {
            let _ = fs::remove_dir_all(&self.path);
            self.memory.update(|s| s.disk_bytes = 0);
        }
    }
}

fn marked(path: &Path) -> Result<bool, Error> {
    if !fs::symlink_metadata(path)?.file_type().is_dir() {
        return Ok(false);
    }
    let marker = path.join("owner");
    match fs::symlink_metadata(&marker) {
        Ok(meta) if meta.is_file() && meta.len() == 8 => {}
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    let mut bytes = [0; 8];
    crate::fs::open_read(&marker)?.read_exact(&mut bytes)?;
    Ok(&bytes == MARKER)
}

pub(super) fn sweep(root: &Path, mut abandoned: impl FnMut(&Path) -> bool) -> Result<usize, Error> {
    let mut removed = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(PREFIX)
            && marked(&entry.path())?
            && abandoned(&entry.path())
        {
            fs::remove_dir_all(entry.path())?;
            removed += 1;
        }
    }
    if removed > 0 {
        crate::fs::sync_dir(root)?;
    }
    Ok(removed)
}
