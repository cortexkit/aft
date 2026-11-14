use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::bash_background::persistence::PinnedDir;

use super::GrammarManifest;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StorageFault {
    #[default]
    None,
    Create,
    Admit,
    Write,
    Publish,
    Read,
}

pub struct Storage {
    pub root: PathBuf,
    pub fault: StorageFault,
}

pub fn verified(manifest: &GrammarManifest, bytes: &[u8]) -> bool {
    bytes.len() as u64 == manifest.size && format!("{:x}", Sha256::digest(bytes)) == manifest.sha256
}

impl Storage {
    pub fn folder(&self) -> PathBuf {
        self.root.join("grammars")
    }

    fn directory(&self, create: bool) -> io::Result<PinnedDir> {
        let folder = self.folder();
        crate::cache_location::refuse_durable_cache_write(&folder)?;
        if create && (self.fault == StorageFault::Admit || self.fault == StorageFault::Create) {
            return Err(io::Error::other("injected grammar storage failure"));
        }
        if create {
            crate::private_storage::create_dir_all(&folder)?;
        }
        if let Some(access) = crate::cache_location::pin_cache_path(&folder)? {
            access.directory(false)
        } else {
            PinnedDir::repair_open_path(&folder)
        }
    }

    fn read_at(&self, dir: &PinnedDir, manifest: &GrammarManifest) -> io::Result<Option<Vec<u8>>> {
        crate::bash_background::persistence::retry_artifact_read(|| {
            self.read_at_once(dir, manifest)
        })
    }

    fn read_at_once(
        &self,
        dir: &PinnedDir,
        manifest: &GrammarManifest,
    ) -> io::Result<Option<Vec<u8>>> {
        if self.fault == StorageFault::Read {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected transient grammar open error",
            ));
        }
        let name = format!("{}.wasm", manifest.sha256);
        let mut file = match dir.repair_read_file(OsStr::new(&name)) {
            Ok(file) => file,
            Err(error) => {
                if error.kind() == io::ErrorKind::NotFound {
                    return Ok(None);
                }
                // Classification never opens a link's target. A transient open
                // failure on a regular artifact is a storage error, not a reason
                // to remove a file shared with another process.
                if std::fs::symlink_metadata(dir.path().join(&name))
                    .is_ok_and(|metadata| !metadata.file_type().is_file())
                {
                    return Ok(None);
                }
                return Err(error);
            }
        };
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        // A stored artifact is untrusted too. Do not allocate for its on-disk size.
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            if (bytes.len() as u64).saturating_add(n as u64) > manifest.size {
                return Ok(None);
            }
            bytes.extend_from_slice(&buffer[..n]);
        }
        drop(file);
        if verified(manifest, &bytes) {
            Ok(Some(bytes))
        } else {
            Ok(None)
        }
    }

    pub fn read(&self, manifest: &GrammarManifest) -> io::Result<Option<Vec<u8>>> {
        match self.directory(false) {
            Ok(dir) => self.read_at(&dir, manifest),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Publish verified bytes atomically. Return only bytes
    /// re-read and verified from storage, never the download buffer.
    pub fn publish(&self, manifest: &GrammarManifest, bytes: &[u8]) -> io::Result<Vec<u8>> {
        let dir = self.directory(true)?;
        if !verified(manifest, bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "grammar publication requires verified bytes",
            ));
        }
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(io::Error::other)?;
        let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let temp = format!(".{}.{}.{nonce}.tmp", manifest.sha256, std::process::id());
        let dest = format!("{}.wasm", manifest.sha256);
        let result = (|| {
            let mut file = dir.repair_new_file(OsStr::new(&temp))?;
            if self.fault == StorageFault::Write {
                file.write_all(&bytes[..bytes.len().min(1)])?;
                return Err(io::Error::other("injected grammar write failure"));
            }
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            if self.fault == StorageFault::Publish {
                return Err(io::Error::other("injected grammar publication failure"));
            }
            // A valid artifact at this hash-named entry is byte-identical to
            // these verified bytes. Replacing the entry is therefore safe even
            // if another process published first, and never follows a symlink target.
            crate::bash_background::persistence::randomized_atomic_replace(
                &dir,
                OsStr::new(&dest),
                bytes,
            )
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "cannot publish grammar {}: {error}",
                        dir.path().join(&dest).display()
                    ),
                )
            })?;
            self.read_at(&dir, manifest)?.ok_or_else(|| {
                io::Error::other(format!(
                    "stored grammar is unavailable: {}",
                    dir.path().join(&dest).display()
                ))
            })
        })();
        let _ = dir.repair_remove_file(OsStr::new(&temp));
        result
    }
}

#[cfg(any(test, feature = "test-hooks"))]
pub fn stored_path(root: &std::path::Path, manifest: &GrammarManifest) -> PathBuf {
    root.join("grammars")
        .join(format!("{}.wasm", manifest.sha256))
}
