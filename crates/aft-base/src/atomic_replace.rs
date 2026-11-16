//! Atomic publication through a pinned directory with explicit durability.

use crate::pinned_dir::{random_temp_name, validate_regular_handle, PinnedDir};
#[cfg(windows)]
use crate::pinned_dir::{
    validate_directory_handle, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
};
use std::ffi::OsStr;
use std::fs::File;
#[cfg(windows)]
use std::fs::OpenOptions;
use std::io::{self, Write};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

/// How much of an atomic file replacement is synced to disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaceSync {
    /// Nothing is synced. The rename still protects readers from a partly
    /// written file after a process kill, but not after a power loss.
    None,
    /// The new file's contents are synced before the rename; the directory
    /// entry is not synced after it.
    File,
    /// The new file is synced before the rename and its directory after it,
    /// so the replacement itself survives a power loss.
    FileAndDirectory,
}

/// Sync and crash/fault instrumentation supplied by the artifact owner.
pub trait ReplaceHooks {
    fn sync_file(&mut self, file: &File, path: &Path) -> io::Result<()>;
    fn before_rename(&mut self);
    fn after_rename(&mut self);
}

/// Normal syncing with no crash or fault injection.
pub struct NoReplaceHooks;

impl ReplaceHooks for NoReplaceHooks {
    fn sync_file(&mut self, file: &File, path: &Path) -> io::Result<()> {
        aft_store::sync::sync_all(file, path)
    }
    fn before_rename(&mut self) {}
    fn after_rename(&mut self) {}
}

pub fn sync_directory_with_hooks(dir: &PinnedDir, hooks: &mut impl ReplaceHooks) -> io::Result<()> {
    #[cfg(unix)]
    {
        hooks.sync_file(&dir.file, &dir.path)
    }
    #[cfg(windows)]
    {
        // FlushFileBuffers requires GENERIC_WRITE even for a directory. Keep
        // ordinary pinned opens read-only; acquire write access only to flush,
        // and verify that the reopened handle still names the pinned folder.
        let writable = PinnedDir::from_open_file(
            OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(&dir.path)?,
            dir.path.clone(),
            {
                #[cfg(any(test, feature = "test-support"))]
                {
                    dir.cache_device_override
                }
                #[cfg(not(any(test, feature = "test-support")))]
                {
                    None
                }
            },
        );
        validate_directory_handle(&writable.file)?;
        if !dir.same_identity(&writable)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pinned directory path identity changed",
            ));
        }
        hooks.sync_file(&writable.file, &dir.path)?;
        dir.ensure_current_identity()
    }
}

pub fn atomic_replace_with_hooks(
    dir: &PinnedDir,
    name: &OsStr,
    content: &[u8],
    sync: ReplaceSync,
    hooks: &mut impl ReplaceHooks,
) -> io::Result<()> {
    for _ in 0..32 {
        let temporary = random_temp_name()?;
        let mut file = match dir.open_new_file(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = (|| {
            file.write_all(content)?;
            if sync != ReplaceSync::None {
                hooks.sync_file(&file, &dir.path.join(&temporary))?;
            }
            // Rename protects readers from partial bytes after a process kill;
            // the explicit sync policy separately governs power-loss durability.
            validate_regular_handle(&file)?;
            hooks.before_rename();
            dir.rename(&temporary, name)?;
            hooks.after_rename();
            if sync == ReplaceSync::FileAndDirectory {
                sync_directory_with_hooks(dir, hooks)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = dir.remove_file(&temporary);
        }
        return result;
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate a randomized atomic-write name",
    ))
}
