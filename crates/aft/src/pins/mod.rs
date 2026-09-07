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

use crate::blob_store::FullKey;
use crate::fs_lock;
use crate::root_cache::{self, ReadMarker};

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
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "pin I/O error: {error}"),
            Self::Serialize(error) => write!(f, "pin serialization error: {error}"),
            Self::InvalidGeneration(generation) => {
                write!(f, "invalid pin generation `{generation}`")
            }
        }
    }
}

impl std::error::Error for PinError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Serialize(error) => Some(error),
            Self::InvalidGeneration(_) => None,
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

/// A durable pin around an in-progress assembly.
#[derive(Debug)]
pub struct AssemblyPin {
    keys_path: PathBuf,
    metadata_path: PathBuf,
    metadata: PinMetadata,
    released: bool,
}

impl AssemblyPin {
    /// Creates and syncs `pins/<generation>.keys` before making the pin visible.
    /// The caller must create this guard before its first blob put.
    pub fn create(
        view_dir: &Path,
        family: impl Into<String>,
        view: impl Into<String>,
        generation: impl Into<String>,
        keys: &[FullKey],
    ) -> Result<Self, PinError> {
        let family = family.into();
        let view = view.into();
        let generation = generation.into();
        validate_generation(&generation)?;
        let pins_dir = view_dir.join("pins");
        fs::create_dir_all(&pins_dir)?;

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
        fs_lock::sync_parent(&self.metadata_path);
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

fn write_keys(path: &Path, keys: &[FullKey]) -> Result<(), PinError> {
    let mut encoded = keys.iter().map(FullKey::to_hex).collect::<Vec<_>>();
    encoded.sort_unstable();
    encoded.dedup();
    let mut file = create_private(path)?;
    for key in encoded {
        writeln!(file, "{key}")?;
    }
    file.sync_all()?;
    drop(file);
    fs_lock::sync_parent(path);
    Ok(())
}

fn write_metadata(path: &Path, metadata: &PinMetadata) -> Result<(), PinError> {
    let temporary = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), now_ms()));
    let result = (|| {
        let mut file = create_private(&temporary)?;
        serde_json::to_writer(&mut file, metadata)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs_lock::rename_over(&temporary, path)?;
        fs_lock::sync_parent(path);
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
