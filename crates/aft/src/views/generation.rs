//! Generation-owned derived artifacts. Unpublished files are protected by the
//! assembly pin; published files by the pointer and query read markers.
use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use super::{Result, ViewStore};

impl ViewStore {
    pub fn derived_path(&self, generation: &str) -> Result<PathBuf> {
        super::validate_generation(generation)?;
        Ok(self.view_dir().join(format!("derived-{generation}.sqlite")))
    }

    pub fn trigram_path(&self, generation: &str) -> Result<PathBuf> {
        super::validate_generation(generation)?;
        Ok(self.view_dir().join(format!("trigram-{generation}.bin")))
    }

    pub fn blob_references_by_generation(
        &self,
    ) -> Result<std::collections::BTreeMap<String, std::collections::BTreeSet<[u8; 32]>>> {
        let mut result = std::collections::BTreeMap::new();
        for entry in fs::read_dir(self.view_dir())? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(generation) = name
                .to_str()
                .and_then(|name| name.strip_prefix("manifest-"))
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            let manifest = self.load_manifest(generation)?;
            let keys = manifest
                .plane_keys()
                .filter_map(|(_, key)| {
                    if key.len() != 64 {
                        return None;
                    }
                    let bytes = (0..64)
                        .step_by(2)
                        .map(|offset| u8::from_str_radix(&key[offset..offset + 2], 16).ok())
                        .collect::<Option<Vec<_>>>()?;
                    bytes.try_into().ok()
                })
                .collect();
            result.insert(generation.to_owned(), keys);
        }
        Ok(result)
    }

    /// Remove generation files only after checking both durable publication and
    /// liveness. A dead assembler can leave a derived file before any manifest.
    pub fn sweep_generations(&self) -> Result<usize> {
        let current = self.current_generation()?;
        let mut generations = std::collections::BTreeSet::new();
        for entry in fs::read_dir(self.view_dir())? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let generation = name
                .strip_prefix("derived-")
                .and_then(|s| s.strip_suffix(".sqlite"))
                .or_else(|| {
                    name.strip_prefix("manifest-")
                        .and_then(|s| s.strip_suffix(".json"))
                })
                .or_else(|| {
                    name.strip_prefix("trigram-")
                        .and_then(|s| s.strip_suffix(".bin"))
                });
            if let Some(generation) = generation {
                generations.insert(generation.to_owned());
            }
        }
        let mut removed = 0;
        for generation in generations {
            if current.as_deref() == Some(&generation) {
                continue;
            }
            let (metadata_path, keys_path) = crate::pins::pin_paths(self.view_dir(), &generation);
            if metadata_path.exists() {
                let Ok(bytes) = fs::read(&metadata_path) else {
                    continue;
                };
                let Ok(metadata) = serde_json::from_slice::<crate::pins::PinMetadata>(&bytes)
                else {
                    continue;
                };
                if crate::pins::owner_is_live(&metadata.owner)
                    && crate::pins::now_ms().saturating_sub(metadata.renewed_at)
                        <= crate::pins::PIN_TTL_MS
                {
                    continue;
                }
                let _ = fs::remove_file(metadata_path);
                let _ = fs::remove_file(keys_path);
            }
            if crate::root_cache::sweep_read_markers(self.view_dir(), &generation).protected {
                continue;
            }
            // Recheck after pin inspection: a publisher keeps its assembly pin
            // until its committed pointer is visible.
            if self.current_generation()?.as_deref() == Some(&generation) {
                continue;
            }
            self.remove_generation_files(&generation);
            removed += 1;
        }
        Ok(removed)
    }

    pub(super) fn remove_generation_files(&self, generation: &str) {
        for path in [
            self.derived_path(generation),
            self.trigram_path(generation),
            self.manifest_path(generation),
        ]
        .into_iter()
        .flatten()
        {
            for suffix in ["", "-wal", "-shm"] {
                let mut name = path.as_os_str().to_owned();
                name.push(suffix);
                let _ = fs::remove_file(PathBuf::from(name));
            }
        }
    }
}

/// The source is a checkpointed immutable generation, never a live SQLite
/// database. Copying its main file therefore cannot omit committed WAL pages.
pub(super) fn clone_derived(source: &Path, destination: &Path) -> Result<()> {
    let started = Instant::now();
    let mechanism = if try_clone(source, destination) {
        "reflink"
    } else {
        let _ = fs::remove_file(destination);
        fs::copy(source, destination)?;
        "copy"
    };
    log::info!(
        "view derived clone mechanism={} ms={} source={} destination={}",
        mechanism,
        started.elapsed().as_millis(),
        source.display(),
        destination.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn try_clone(source: &Path, destination: &Path) -> bool {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let (Ok(source), Ok(destination)) = (
        CString::new(source.as_os_str().as_bytes()),
        CString::new(destination.as_os_str().as_bytes()),
    ) else {
        return false;
    };
    // Both C strings remain alive for the duration of clonefile.
    unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), 0) == 0 }
}

#[cfg(target_os = "linux")]
fn try_clone(source: &Path, destination: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let (Ok(source), Ok(destination)) = (
        fs::File::open(source),
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination),
    ) else {
        return false;
    };
    // FICLONE takes the source descriptor as its third argument.
    unsafe {
        libc::ioctl(
            destination.as_raw_fd(),
            0x40049409 as libc::c_ulong,
            source.as_raw_fd(),
        ) == 0
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn try_clone(_source: &Path, _destination: &Path) -> bool {
    false
}
