//! Durable generation pins used while a view is assembled or read.
//!
//! An assembly pin is created before its first blob put so a concurrent sweep
//! can keep every prospective blob alive until publication finishes.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::blob_store::v2::{FamilyKey, FamilyStore, TouchReport};
use crate::blob_store::FullKey;
use crate::fs_lock;
use crate::root_cache::{self, ReadMarker};
use crate::views::registry::ViewRegistration;

mod live;
pub use live::LivePin;

/// A pin remains live for thirty minutes after its most recent successful renewal.
pub const PIN_TTL_MS: u64 = 30 * 60 * 1_000;
/// Assemblers renew before a third of the pin lifetime has elapsed.
pub const PIN_RENEW_INTERVAL_MS: u64 = PIN_TTL_MS / 3;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PinOwner {
    pub pid: u32,
    pub start_time: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PinMetadata {
    pub family: String,
    pub view: String,
    pub generation: String,
    pub owner: PinOwner,
    pub created_at: u64,
    pub renewed_at: u64,
}

#[derive(Debug)]
pub enum PinError {
    Io(io::Error),
    Serialize(serde_json::Error),
    InvalidGeneration(String),
    /// The view is no longer registered, so it may not be pinned; see
    /// `ViewRegistration::under_pin_barrier`.
    NotRegistered(String),
    /// The family registry could not be read or locked.
    Registry(String),
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "pin I/O error: {error}"),
            Self::Serialize(error) => write!(f, "pin serialization error: {error}"),
            Self::InvalidGeneration(generation) => {
                write!(f, "invalid pin generation `{generation}`")
            }
            Self::NotRegistered(scope) => {
                write!(f, "view `{scope}` is not registered and cannot be pinned")
            }
            Self::Registry(error) => write!(f, "pin registry error: {error}"),
        }
    }
}

impl std::error::Error for PinError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Serialize(error) => Some(error),
            Self::InvalidGeneration(_) | Self::NotRegistered(_) | Self::Registry(_) => None,
        }
    }
}

impl From<io::Error> for PinError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for PinError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialize(error)
    }
}

/// An on-disk pin around an in-progress assembly, meaningful while its owner lives.
#[derive(Debug)]
pub struct AssemblyPin {
    keys_path: PathBuf,
    metadata_path: PathBuf,
    metadata: PinMetadata,
    released: bool,
}

impl AssemblyPin {
    /// Writes `pins/<generation>.keys` before making the pin visible.
    /// The caller must create this guard before its first blob put.
    pub fn create(
        view_dir: &Path,
        family: impl Into<String>,
        view: impl Into<String>,
        generation: impl Into<String>,
        keys: &[FullKey],
    ) -> Result<Self, PinError> {
        Self::create_with_hex_keys(
            view_dir,
            family.into(),
            view.into(),
            generation.into(),
            keys.iter().map(FullKey::to_hex).collect(),
        )
    }

    /// Creates a pin for per-checkout (v2) work, under the registry barrier of
    /// `registration`. It must list every key the work will rely on, in every
    /// plane, including keys that are already stored; see
    /// [`protect_then_touch`]. Fails with [`PinError::NotRegistered`] once the
    /// view has been deregistered.
    pub fn create_v2(
        registration: &ViewRegistration,
        generation: impl Into<String>,
        keys: &[FamilyKey],
    ) -> Result<Self, PinError> {
        let generation = generation.into();
        let keys = keys.iter().map(FamilyKey::to_hex).collect();
        registration.under_pin_barrier(|view_dir| {
            Self::create_with_hex_keys(
                view_dir,
                registration.family().to_owned(),
                registration.scope().to_owned(),
                generation,
                keys,
            )
        })
    }

    fn create_with_hex_keys(
        view_dir: &Path,
        family: String,
        view: String,
        generation: String,
        keys: Vec<String>,
    ) -> Result<Self, PinError> {
        validate_generation(&generation)?;
        let pins_dir = view_dir.join("pins");
        crate::private_storage::create_dir_all(&pins_dir)?;

        let keys_path = pins_dir.join(format!("{generation}.keys"));
        write_keys(&keys_path, keys)?;
        let now = now_ms();
        let metadata = PinMetadata {
            family,
            view,
            generation: generation.clone(),
            owner: PinOwner {
                pid: std::process::id(),
                start_time: root_cache::process_start_time_ms(std::process::id()).unwrap_or(now),
            },
            created_at: now,
            renewed_at: now,
        };
        let metadata_path = pins_dir.join(format!("{generation}.json"));
        write_metadata(&metadata_path, &metadata)?;
        Ok(Self {
            keys_path,
            metadata_path,
            metadata,
            released: false,
        })
    }

    pub fn metadata(&self) -> &PinMetadata {
        &self.metadata
    }

    pub fn keys_path(&self) -> &Path {
        &self.keys_path
    }

    /// Checks, before a publisher's CAS, that its pin files still exist with
    /// its own metadata. A sweep may have reclaimed a pin it wrongly judged
    /// dead; publishing without it would expose keys nothing protected.
    pub fn verify_held(&self) -> Result<(), PinError> {
        let stored: PinMetadata = serde_json::from_slice(&fs::read(&self.metadata_path)?)?;
        if stored.owner != self.metadata.owner
            || stored.generation != self.metadata.generation
            || stored.family != self.metadata.family
            || !self.keys_path.is_file()
        {
            return Err(PinError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "the assembly pin was replaced or reclaimed",
            )));
        }
        Ok(())
    }

    /// Renews the pin when a put is due. A renewal error is returned before the
    /// caller's put closure runs, so an assembly cannot publish after losing its pin.
    pub fn put<T>(&mut self, put: impl FnOnce() -> Result<T, PinError>) -> Result<T, PinError> {
        self.renew_if_due()?;
        put()
    }

    pub fn renew_if_due(&mut self) -> Result<(), PinError> {
        let now = now_ms();
        if now.saturating_sub(self.metadata.renewed_at) >= PIN_RENEW_INTERVAL_MS {
            self.metadata.renewed_at = now;
            write_metadata(&self.metadata_path, &self.metadata)?;
        }
        Ok(())
    }

    /// Removes both parts of the pin once publication has completed or aborted.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        let _ = fs::remove_file(&self.metadata_path);
        let _ = fs::remove_file(&self.keys_path);
        self.released = true;
    }
}

impl Drop for AssemblyPin {
    fn drop(&mut self) {
        self.release();
    }
}

/// Pins a view generation for an in-flight query and removes the marker on drop.
/// Existing read-marker sweeping reclaims markers left by dead owners.
#[derive(Debug)]
pub struct QueryPin {
    marker: ReadMarker,
}

impl QueryPin {
    pub fn acquire(view_dir: &Path, generation: &str) -> Result<Self, PinError> {
        Ok(Self {
            marker: ReadMarker::create(view_dir, generation)?,
        })
    }

    pub fn touch_if_due(&self) -> Result<(), PinError> {
        self.marker.touch_if_due()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        self.marker.path()
    }
}

pub(crate) fn pin_paths(view_dir: &Path, generation: &str) -> (PathBuf, PathBuf) {
    let pins_dir = view_dir.join("pins");
    (
        pins_dir.join(format!("{generation}.json")),
        pins_dir.join(format!("{generation}.keys")),
    )
}

/// Something whose key list is visible on disk, where a family sweep reads it.
pub trait Protection {
    fn durable_keys_path(&self) -> &Path;
}

impl Protection for AssemblyPin {
    fn durable_keys_path(&self) -> &Path {
        &self.keys_path
    }
}

/// Touches `keys` in `store`, but only after checking that every one of them
/// is already listed in `protection`'s on-disk key file. This is the
/// "protect, then touch" order the family GC relies on: a sweep either saw
/// the protection while marking, or the touch landed at its new epoch.
/// Keys reported missing must be put again from the caller's bytes.
pub fn protect_then_touch(
    protection: &dyn Protection,
    store: &FamilyStore,
    keys: &[FamilyKey],
) -> Result<TouchReport, PinError> {
    let listed = read_keys(protection.durable_keys_path())?
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(unprotected) = keys.iter().find(|key| !listed.contains(key.as_bytes())) {
        return Err(PinError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("key {unprotected} is touched before it is protected"),
        )));
    }
    store
        .touch(keys)
        .map_err(|error| PinError::Io(io::Error::other(error.to_string())))
}

/// Reads a pin's metadata, failing on anything unreadable or malformed. The
/// family sweep aborts on such an error instead of skipping the pin.
pub(crate) fn read_metadata_strict(path: &Path) -> Result<PinMetadata, PinError> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

pub(crate) fn read_keys(path: &Path) -> Result<Vec<[u8; 32]>, PinError> {
    let contents = fs::read_to_string(path)?;
    contents.lines().map(parse_hex_key).collect()
}

pub(crate) fn owner_is_live(owner: &PinOwner) -> bool {
    root_cache::process_start_time_ms(owner.pid)
        .map(|actual| actual == owner.start_time)
        .unwrap_or_else(|| crate::fs_lock::process_alive(owner.pid))
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn validate_generation(generation: &str) -> Result<(), PinError> {
    if generation.is_empty()
        || generation == "."
        || generation == ".."
        || generation.contains(['/', '\\'])
    {
        return Err(PinError::InvalidGeneration(generation.to_owned()));
    }
    Ok(())
}

fn write_keys(path: &Path, mut encoded: Vec<String>) -> Result<(), PinError> {
    encoded.sort_unstable();
    encoded.dedup();
    let file = create_private(path)?;
    write_key_lines(file, encoded.iter().map(String::as_str))?;
    Ok(())
}

/// Writes one key per line. Sweeps consult keys only while their owner lives,
/// so page-cache visibility is enough; dead owners' keys are never consumed.
/// The lines are assembled in
/// memory and handed to the kernel in one `write_all`: writing line by line
/// on an unbuffered `File` costs a system call (or two) per key, which adds
/// up when a pin lists thousands of blobs.
pub(crate) fn write_key_lines<'a>(
    file: File,
    keys: impl IntoIterator<Item = &'a str>,
) -> io::Result<()> {
    let mut contents = Vec::new();
    for key in keys {
        contents.extend_from_slice(key.as_bytes());
        contents.push(b'\n');
    }
    let mut file = work_counters::CountingFile(file);
    file.write_all(&contents)
}

/// Counts the system-call work spent writing pin key files, so tests can pin
/// the number of writes and fsyncs a batch costs. Counting is per thread so
/// parallel tests do not see each other's work; a thread-local increment is
/// negligible next to the write and fsync it counts.
#[doc(hidden)]
pub mod work_counters {
    use std::cell::Cell;
    use std::fs::File;
    use std::io::{self, Write};

    thread_local! {
        static WRITES: Cell<u64> = const { Cell::new(0) };
        static SYNCS: Cell<u64> = const { Cell::new(0) };
        #[cfg(test)]
        static BYTES: Cell<u64> = const { Cell::new(0) };
    }

    /// Pin key-file `(write calls, fsyncs)` made on this thread so far.
    pub fn key_file_work() -> (u64, u64) {
        (WRITES.with(Cell::get), SYNCS.with(Cell::get))
    }

    /// Bytes actually written to pin key files on this thread.
    #[cfg(test)]
    pub(crate) fn key_file_bytes() -> u64 {
        BYTES.with(Cell::get)
    }

    pub(crate) struct CountingFile(pub(crate) File);

    impl Write for CountingFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            WRITES.with(|c| c.set(c.get() + 1));
            let written = self.0.write(buf)?;
            #[cfg(test)]
            BYTES.with(|c| c.set(c.get() + written as u64));
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }
}

fn write_metadata(path: &Path, metadata: &PinMetadata) -> Result<(), PinError> {
    let temporary = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), now_ms()));
    let result = (|| {
        let mut file = create_private(&temporary)?;
        serde_json::to_writer(&mut file, metadata)?;
        file.write_all(b"\n")?;
        crate::durability::sync_file(&file, path)?;
        drop(file);
        fs_lock::rename_over(&temporary, path)?;
        // Strict family sweeps abort on malformed metadata. Keep its data
        // flush even though keys and directory entries are process-lifetime.
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_private(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        return OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path);
    }
    #[cfg(not(unix))]
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn parse_hex_key(value: &str) -> Result<[u8; 32], PinError> {
    if value.len() != 64 {
        return Err(PinError::InvalidGeneration(value.to_owned()));
    }
    let mut key = [0; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| PinError::InvalidGeneration(value.to_owned()))?;
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durability_assembly_pin_counts() {
        let view = tempfile::tempdir().unwrap();
        crate::durability::take();
        let mut pin =
            AssemblyPin::create(view.path(), "family", "view", "generation", &[]).unwrap();
        assert_eq!(crate::durability::sync_count(&crate::durability::take()), 1);
        pin.metadata.renewed_at = now_ms().saturating_sub(PIN_RENEW_INTERVAL_MS);
        pin.renew_if_due().unwrap();
        assert_eq!(crate::durability::sync_count(&crate::durability::take()), 1);
        pin.release();
        assert_eq!(crate::durability::sync_count(&crate::durability::take()), 0);
    }

    #[test]
    fn failed_renewal_stops_the_next_put() {
        let view = tempfile::tempdir().expect("create view");
        let mut pin = AssemblyPin::create(view.path(), "family", "view", "generation", &[])
            .expect("create pin");
        pin.metadata.renewed_at = now_ms().saturating_sub(PIN_RENEW_INTERVAL_MS);
        fs::remove_file(&pin.metadata_path).expect("remove pin metadata");
        fs::create_dir(&pin.metadata_path).expect("make renewal destination invalid");

        let mut put_called = false;
        let result = pin.put(|| {
            put_called = true;
            Ok(())
        });
        assert!(result.is_err());
        assert!(!put_called, "a failed renewal must stop the put");
    }
}
