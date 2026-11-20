//! Private Linux launcher images, shared by daemons started from the same inode.

use std::fs::{self, File, Metadata, Permissions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct PinIdentity(u64, u64);

impl PinIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self(metadata.dev(), metadata.ino())
    }

    pub(super) fn matches(self, metadata: &Metadata) -> bool {
        metadata.is_file() && self == Self::from_metadata(metadata)
    }
}

pub(super) struct ImageKey {
    device: u64,
    inode: u64,
    size: u64,
    modified_secs: i64,
    modified_nanos: i64,
}

impl ImageKey {
    pub(super) fn new(source: &File) -> io::Result<Self> {
        let metadata = source.metadata()?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified_secs: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
        })
    }

    fn directory_name(&self) -> String {
        format!(
            "{:x}-{:x}-{:x}-{:x}-{:x}",
            self.device, self.inode, self.size, self.modified_secs, self.modified_nanos
        )
    }

    fn matches_pin(&self, metadata: &Metadata) -> bool {
        metadata.is_file()
            && metadata.mode() & 0o7777 == 0o500
            && metadata.size() == self.size
            && (metadata.dev(), metadata.ino()) != (self.device, self.inode)
    }

    pub(super) fn pin(&self, run: &Path, source: &File) -> io::Result<(PathBuf, PinIdentity)> {
        self.pin_with(run, source, reflink)
    }

    fn pin_with(
        &self,
        run: &Path,
        source: &File,
        clone_image: impl FnOnce(&File, &File) -> io::Result<()>,
    ) -> io::Result<(PathBuf, PinIdentity)> {
        let images = run.join("aft-images");
        let directory = images.join(self.directory_name());
        open_private_dir(&images)?;
        open_private_dir(&directory)?;
        // All publishers for an identity lock the same file. The lock file is
        // never renamed or deleted, so waiters always lock the same inode.
        let lock = crate::private_storage::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(directory.join("image.lock"))?;
        lock.lock()?;
        let destination = directory.join("aft");
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if self.matches_pin(&metadata) => {
                return Ok((destination, PinIdentity::from_metadata(&metadata)));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let temporary = directory.join("aft.part");
        // A daemon killed while copying may have left this file behind. Only
        // the lock holder may remove it or publish the completed executable.
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let result = publish(source, &temporary, &destination, clone_image);
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        // Surviving tasks can still resolve a pin by pathname, including after
        // the owning daemon has exited. The daemon cannot prove global disuse
        // from its own registry, so keep older pin directories intact.
        log::debug!(
            "retaining older Linux executable pins: live task references may be unobservable"
        );
        let identity = PinIdentity::from_metadata(&destination.symlink_metadata()?);
        Ok((destination, identity))
    }
}

fn open_private_dir(path: &Path) -> io::Result<()> {
    match crate::private_storage::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if fs::symlink_metadata(path)?.is_dir() {
                Ok(())
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

fn publish(
    source: &File,
    temporary: &Path,
    destination: &Path,
    clone_image: impl FnOnce(&File, &File) -> io::Result<()>,
) -> io::Result<()> {
    let mut output = crate::private_storage::options()
        .write(true)
        .create_new(true)
        .open(temporary)?;
    match clone_image(source, &output) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EXDEV | libc::EOPNOTSUPP | libc::ENOTTY | libc::EINVAL | libc::ENOSYS)
            ) =>
        {
            output.set_len(0)?;
            copy_held_image(source, &mut output)?;
        }
        Err(error) => return Err(error),
    }
    output.set_permissions(Permissions::from_mode(0o500))?;
    aft_store::sync::sync_all(&output, temporary)?;
    // Close the writable descriptor before publication, or a concurrent
    // launch can fail with a text-file-busy error.
    drop(output);
    fs::rename(temporary, destination)
}

fn reflink(source: &File, output: &File) -> io::Result<()> {
    // SAFETY: both descriptors are held open for the duration of the ioctl.
    if unsafe { libc::ioctl(output.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn copy_held_image(source: &File, output: &mut File) -> io::Result<()> {
    // Read positionally: a repair must not depend on the held file's cursor.
    let mut offset = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = match source.read_at(&mut buffer, offset) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            return Ok(());
        }
        output.write_all(&buffer[..count])?;
        offset += count as u64;
    }
}

#[cfg(test)]
mod tests;
