//! What a build workspace says about itself, so that a later build of the
//! same checkout can adopt it and every other process can tell when it may
//! be deleted.
//!
//! A cold build that may be resumed writes `IDENTITY` into its workspace
//! first. The file names the checkout's root folder, which the store reads
//! to judge whether the checkout still exists, and carries the builder's
//! own description of the build (its payload), which only that builder
//! compares. A workspace with no readable `IDENTITY` holds nothing a later
//! build could adopt.
//!
//! `ADOPTED` is rewritten each time a build adopts the workspace, and
//! `PAUSED` is written when a build stops because the disk is short: both
//! are empty, and only their presence and modification times matter.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The file holding a workspace's identity.
pub const IDENTITY: &str = "IDENTITY";
/// The file whose modification time is when a build last adopted the
/// workspace.
pub const ADOPTED: &str = "ADOPTED";
/// The file present while the workspace belongs to a build that stopped
/// for lack of disk space rather than dying.
pub const PAUSED: &str = "PAUSED";

/// A dead workspace that no build adopted for this long is deleted.
pub const MAX_IDLE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const MAGIC: &[u8; 8] = b"AFTWSID1";

/// A workspace's identity: its checkout's root folder and the builder's own
/// description of the build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    pub root: PathBuf,
    pub payload: Vec<u8>,
}

/// Writes `identity` into `workspace` as `IDENTITY`: to a `.part` file
/// first, closed and then renamed, so the name never holds a partial file.
///
/// # Errors
///
/// When the file cannot be written or renamed.
pub fn write_identity(workspace: &Path, identity: &WorkspaceIdentity) -> io::Result<()> {
    let root = identity.root.to_string_lossy();
    let mut bytes = Vec::with_capacity(64 + root.len() + identity.payload.len());
    bytes.extend_from_slice(MAGIC);
    push_part(&mut bytes, root.as_bytes())?;
    push_part(&mut bytes, &identity.payload)?;
    let length = bytes.len() as u64;
    bytes.extend_from_slice(&length.to_le_bytes());
    let checksum = blake3::hash(&bytes[..usize::try_from(length).unwrap_or(usize::MAX)]);
    bytes.extend_from_slice(checksum.as_bytes());
    let part = workspace.join(format!("{IDENTITY}.part"));
    {
        let mut file = std::fs::File::create(&part)?;
        file.write_all(&bytes)?;
    }
    std::fs::rename(&part, workspace.join(IDENTITY))
}

fn push_part(bytes: &mut Vec<u8>, part: &[u8]) -> io::Result<()> {
    let length = u32::try_from(part.len())
        .map_err(|_| io::Error::other("a workspace identity part is 4 GiB or larger"))?;
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(part);
    Ok(())
}

/// The identity in `workspace`, or `None` when it has none or the file is
/// not exactly as written.
#[must_use]
pub fn read_identity(workspace: &Path) -> Option<WorkspaceIdentity> {
    let bytes = std::fs::read(workspace.join(IDENTITY)).ok()?;
    let body = bytes.len().checked_sub(40)?;
    let length = crate::le::u64_at(&bytes, body);
    if length != body as u64
        || blake3::hash(&bytes[..body]).as_bytes() != &bytes[body + 8..]
        || bytes.get(..8)? != MAGIC
    {
        return None;
    }
    let mut at = 8;
    let mut part = || -> Option<Vec<u8>> {
        let length = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
        let part = bytes.get(at + 4..at + 4 + length)?.to_vec();
        at += 4 + length;
        Some(part)
    };
    let root = String::from_utf8(part()?).ok()?;
    let payload = part()?;
    (at == body).then(|| WorkspaceIdentity {
        root: PathBuf::from(root),
        payload,
    })
}

/// Records that a build adopted `workspace` now.
///
/// # Errors
///
/// When the file cannot be written.
pub fn mark_adopted(workspace: &Path) -> io::Result<()> {
    let file = std::fs::File::create(workspace.join(ADOPTED))?;
    file.set_modified(SystemTime::now())
}

/// When `workspace` was last made or adopted: the newer of its identity's
/// and its adoption mark's modification times.
#[must_use]
pub fn last_used(workspace: &Path) -> Option<SystemTime> {
    [IDENTITY, ADOPTED]
        .iter()
        .filter_map(|name| {
            std::fs::metadata(workspace.join(name))
                .ok()?
                .modified()
                .ok()
        })
        .max()
}

/// Whether `workspace` belongs to a build that stopped for lack of disk.
#[must_use]
pub fn is_paused(workspace: &Path) -> bool {
    workspace.join(PAUSED).exists()
}

/// Whether a dead `workspace` (no live build holds its checkout's lock) is
/// worth keeping for a later build to adopt: it has a readable identity,
/// its checkout's root folder still exists, and it was made or adopted
/// within [`MAX_IDLE`] of `now`.
#[must_use]
pub fn worth_keeping(workspace: &Path, now: SystemTime) -> bool {
    let Some(identity) = read_identity(workspace) else {
        return false;
    };
    if matches!(
        std::fs::symlink_metadata(&identity.root),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ) {
        return false;
    }
    last_used(workspace)
        .is_some_and(|used| now.duration_since(used).unwrap_or_default() <= MAX_IDLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_reads_back_and_a_changed_or_torn_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let identity = WorkspaceIdentity {
            root: dir.path().join("checkout"),
            payload: b"build=1\n".to_vec(),
        };
        assert_eq!(read_identity(dir.path()), None);
        write_identity(dir.path(), &identity).unwrap();
        assert_eq!(read_identity(dir.path()), Some(identity.clone()));
        assert!(!dir.path().join("IDENTITY.part").exists());
        let path = dir.path().join(IDENTITY);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 41;
        bytes[last] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(read_identity(dir.path()), None, "a changed byte");
        write_identity(dir.path(), &identity).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(read_identity(dir.path()), None, "a torn file");
    }

    #[test]
    fn a_workspace_is_kept_while_its_checkout_exists_and_it_was_used_lately() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        std::fs::create_dir(&root).unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let now = SystemTime::now();
        assert!(!worth_keeping(&workspace, now), "no identity");
        write_identity(
            &workspace,
            &WorkspaceIdentity {
                root: root.clone(),
                payload: Vec::new(),
            },
        )
        .unwrap();
        assert!(worth_keeping(&workspace, now));
        let later = now + MAX_IDLE + Duration::from_secs(60);
        assert!(!worth_keeping(&workspace, later), "idle too long");
        mark_adopted(&workspace).unwrap();
        assert!(!worth_keeping(&workspace, later + MAX_IDLE));
        std::fs::remove_dir(&root).unwrap();
        assert!(!worth_keeping(&workspace, now), "checkout gone");
    }
}
