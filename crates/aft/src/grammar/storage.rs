use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

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
        crate::root_cache::refuse_durable_cache_write(&folder)?;
        if self.fault == StorageFault::Admit || (create && self.fault == StorageFault::Create) {
            return Err(io::Error::other("injected grammar storage failure"));
        }
        if create {
            crate::private_storage::create_dir_all(&folder)?;
        }
        if let Some(access) = crate::root_cache::pin_cache_path(&folder)? {
            access.directory(false)
        } else {
            PinnedDir::repair_open_path(&folder)
        }
    }

    fn read_at(&self, dir: &PinnedDir, manifest: &GrammarManifest) -> Option<Vec<u8>> {
        if self.fault == StorageFault::Read {
            return None;
        }
        let name = format!("{}.wasm", manifest.sha256);
        let mut file = dir.repair_read_file(OsStr::new(&name)).ok()?;
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        // A stored artifact is untrusted too. Do not allocate for its on-disk size.
        loop {
            let n = file.read(&mut buffer).ok()?;
            if n == 0 {
                break;
            }
            if (bytes.len() as u64).saturating_add(n as u64) > manifest.size {
                drop(file);
                let _ = dir.repair_remove_file(OsStr::new(&name));
                return None;
            }
            bytes.extend_from_slice(&buffer[..n]);
        }
        drop(file);
        if verified(manifest, &bytes) {
            Some(bytes)
        } else {
            let _ = dir.repair_remove_file(OsStr::new(&name));
            None
        }
    }

    pub fn read(&self, manifest: &GrammarManifest) -> Option<Vec<u8>> {
        self.read_at(&self.directory(false).ok()?, manifest)
    }

    /// Publish without replacing another process's artifact. Return only bytes
    /// re-read and verified from storage, never the download buffer.
    pub fn publish(&self, manifest: &GrammarManifest, bytes: &[u8]) -> io::Result<Vec<u8>> {
        let dir = self.directory(true)?;
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
            for retry in 0..=1 {
                let source = dir.repair_entry_for_rename(OsStr::new(&temp))?;
                match dir.repair_rename_entry_to(
                    OsStr::new(&temp),
                    &source,
                    &dir,
                    OsStr::new(&dest),
                ) {
                    Ok(()) => {
                        return self
                            .read_at(&dir, manifest)
                            .ok_or_else(|| io::Error::other("stored grammar is unavailable"))
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        if let Some(bytes) = self.read_at(&dir, manifest) {
                            return Ok(bytes);
                        }
                        if retry == 1 {
                            return Err(error);
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::other("grammar publication exhausted"))
        })();
        let _ = dir.repair_remove_file(OsStr::new(&temp));
        result
    }
}

pub fn stored_path(root: &Path, manifest: &GrammarManifest) -> PathBuf {
    root.join("grammars")
        .join(format!("{}.wasm", manifest.sha256))
}
