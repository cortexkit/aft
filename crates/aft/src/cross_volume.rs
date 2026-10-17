//! Moves between storage folders that may sit on different volumes.
//!
//! The rebuildable cache folders under the storage root (views, blobs,
//! callgraph, semantic, index, symbols, inspect) can be placed on a separate
//! volume, as a mount point or through a symlink. A `rename(2)` from one
//! volume to another fails with `EXDEV` (`ERROR_NOT_SAME_DEVICE` on Windows),
//! so a move whose source and destination folders can differ in volume goes
//! through [`move_path`], which falls back to a copy that is published by a
//! same-folder rename in the destination and only then removes the source.
//!
//! [`rename`] is the plain rename every such move starts with. Tests make it
//! report the cross-device error for folders registered with
//! `crate::walk_boundary::simulated_volumes`, because CI has no second volume.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// True when `error` is the operating system's "cannot move across volumes"
/// error.
pub(crate) fn is_cross_device(error: &io::Error) -> bool {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::EXDEV) {
        return true;
    }
    // ERROR_NOT_SAME_DEVICE
    #[cfg(windows)]
    if error.raw_os_error() == Some(17) {
        return true;
    }
    error.kind() == io::ErrorKind::CrossesDevices
}

#[cfg(test)]
fn cross_device_error() -> io::Error {
    #[cfg(unix)]
    return io::Error::from_raw_os_error(libc::EXDEV);
    #[cfg(windows)]
    return io::Error::from_raw_os_error(17);
    #[cfg(not(any(unix, windows)))]
    return io::Error::from(io::ErrorKind::CrossesDevices);
}

/// `fs::rename`, except that under test a move between two folders on
/// different simulated volumes fails exactly as a real cross-volume rename.
pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(test)]
    {
        use crate::walk_boundary::simulated_volumes::device_of;
        let source = device_of(from);
        let destination = to.parent().and_then(device_of);
        if (source.is_some() || destination.is_some()) && source != destination {
            return Err(cross_device_error());
        }
    }
    fs::rename(from, to)
}

/// True when `path` is on a different volume from its parent folder, which is
/// what a mount point looks like. False when either device cannot be read
/// (Windows standard metadata has no volume identifier).
pub(crate) fn is_mount_point(path: &Path) -> io::Result<bool> {
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let own = crate::walk_boundary::filesystem_device_id(path)?;
    let above = crate::walk_boundary::filesystem_device_id(parent)?;
    Ok(matches!((own, above), (Some(own), Some(above)) if own != above))
}

/// How [`move_path`] moved its source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Moved {
    /// One rename: source and destination are on the same volume.
    Renamed,
    /// The volumes differ: the source was copied, published under the
    /// destination name, and then removed.
    Copied,
}

/// Move the file or directory `from` to `to`, which must not exist.
///
/// A same-volume move is one rename. A cross-volume move copies `from` into a
/// staging name inside `to`'s own folder, syncs every copied file and folder,
/// renames the staging copy to `to`, and removes `from` last. If the copy
/// fails, the staging copy is removed and `from` is left untouched, so an
/// error never loses data. If only the final removal of `from` fails, the
/// move has still happened: the leftover source is logged and `Ok` returned.
pub(crate) fn move_path(from: &Path, to: &Path) -> io::Result<Moved> {
    let error = match rename(from, to) {
        Ok(()) => return Ok(Moved::Renamed),
        Err(error) => error,
    };
    if !is_cross_device(&error) {
        return Err(error);
    }
    if fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "cannot move {} across volumes: {} already exists",
                from.display(),
                to.display()
            ),
        ));
    }
    let parent = to
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staging = staging_path(parent, to)?;
    if let Err(error) = copy_tree(from, &staging).and_then(|()| {
        // Same folder, so this rename stays on the destination volume.
        fs::rename(&staging, to)?;
        crate::durability::sync_dir(parent)
    }) {
        remove_path(&staging).ok();
        return Err(io::Error::new(
            error.kind(),
            format!(
                "cannot move {} across volumes to {}: {error}; the source was left in place",
                from.display(),
                to.display()
            ),
        ));
    }
    if let Err(error) = remove_path(from) {
        crate::slog_warn!(
            "moved {} across volumes to {} but could not remove the source: {error}",
            from.display(),
            to.display()
        );
    }
    Ok(Moved::Copied)
}

fn staging_path(parent: &Path, to: &Path) -> io::Result<PathBuf> {
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    Ok(parent.join(format!(
        ".{}.aft-move-{}-{:016x}",
        to.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        u64::from_le_bytes(nonce)
    )))
}

/// Copy `from` to the new path `to`, keeping permissions, and sync it.
/// Symlinks are recreated as links (never followed); any other special file
/// is refused rather than silently skipped.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(fs::read_link(from)?, to);
        #[cfg(not(unix))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("cannot copy symlink {} across volumes", from.display()),
        ));
    }
    if kind.is_file() {
        fs::copy(from, to)?;
        fs::File::open(to)?.sync_all()?;
        return Ok(());
    }
    if !kind.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("cannot copy special file {} across volumes", from.display()),
        ));
    }
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        copy_tree(&entry.path(), &to.join(entry.file_name()))?;
    }
    fs::set_permissions(to, metadata.permissions())?;
    crate::durability::sync_dir(to)
}

fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk_boundary::simulated_volumes::simulate;

    #[test]
    fn cross_volume_move_copies_publishes_and_removes_the_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("a.json"), b"first").unwrap();
        fs::write(source.join("nested/b.bin"), b"second").unwrap();
        let other = temp.path().join("other-volume");
        fs::create_dir(&other).unwrap();
        let _volume = simulate(&other, u64::MAX - 1);
        let destination = other.join("moved");

        assert!(
            is_cross_device(&rename(&source, &destination).unwrap_err()),
            "the seam must reproduce the kernel's cross-volume refusal"
        );
        assert_eq!(move_path(&source, &destination).unwrap(), Moved::Copied);

        assert!(!source.exists(), "the source is removed after the copy");
        assert_eq!(fs::read(destination.join("a.json")).unwrap(), b"first");
        assert_eq!(
            fs::read(destination.join("nested/b.bin")).unwrap(),
            b"second"
        );
        let leftovers = fs::read_dir(&other)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("moved")]);
    }

    #[test]
    fn same_volume_move_is_one_rename() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("file");
        fs::write(&source, b"x").unwrap();
        let destination = temp.path().join("renamed");
        assert_eq!(move_path(&source, &destination).unwrap(), Moved::Renamed);
        assert_eq!(fs::read(destination).unwrap(), b"x");
    }

    #[test]
    fn cross_volume_move_refuses_an_existing_destination_and_keeps_the_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("state.json");
        fs::write(&source, b"keep").unwrap();
        let other = temp.path().join("other-volume");
        fs::create_dir(&other).unwrap();
        let _volume = simulate(&other, u64::MAX - 2);
        let destination = other.join("state.json");
        fs::write(&destination, b"existing").unwrap();

        let error = move_path(&source, &destination).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&source).unwrap(), b"keep");
        assert_eq!(fs::read(&destination).unwrap(), b"existing");
    }

    #[test]
    fn a_folder_on_another_device_than_its_parent_is_a_mount_point() {
        let temp = tempfile::tempdir().unwrap();
        let mounted = temp.path().join("views");
        fs::create_dir(&mounted).unwrap();
        let plain = temp.path().join("plain");
        fs::create_dir(&plain).unwrap();
        // Register the parent too so the comparison also works where the
        // platform reports no real device number.
        let _parent = simulate(temp.path(), u64::MAX - 3);
        let _volume = simulate(&mounted, u64::MAX - 4);
        assert!(is_mount_point(&mounted).unwrap());
        assert!(!is_mount_point(&plain).unwrap());
    }
}
