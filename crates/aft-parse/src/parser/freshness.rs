use std::fs::Metadata;
use std::path::Path;

use crate::cache_freshness;

/// Restoring a file's mtime does not restore its inode's change time, which
/// remains available as freshness evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(unix)]
pub struct Stamp([u64; 7]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(not(unix))]
pub struct Stamp;

impl Stamp {
    #[cfg(unix)]
    pub fn of(meta: &Metadata) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        Some(Self([
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mtime() as u64,
            meta.mtime_nsec() as u64,
            meta.ctime() as u64,
            meta.ctime_nsec() as u64,
        ]))
    }

    #[cfg(not(unix))]
    pub fn of(_: &Metadata) -> Option<Self> {
        None
    }

    pub(super) fn read(path: &Path) -> Option<Self> {
        Self::of(&crate::bounded_io::metadata(path).ok()?)
    }

    pub(super) fn unchanged_after(self, path: &Path) -> Option<Self> {
        (Self::read(path) == Some(self)).then_some(self)
    }
}

pub fn fresh_file_stamp(
    path: &Path,
    cached_stamp: Option<Stamp>,
    cached_size: u64,
    cached_content_hash: blake3::Hash,
) -> Option<Option<Stamp>> {
    #[cfg(any(test, feature = "test-support"))]
    super::FRESHNESS_TEST_BARRIERS.with(|slot| {
        if let Some((arrived, released)) = slot.borrow_mut().take() {
            arrived.wait();
            released.wait();
        }
    });
    let metadata = crate::bounded_io::metadata(path).ok()?;
    let stamp = Stamp::of(&metadata);
    let size = metadata.len();
    if size != cached_size {
        return None;
    }
    if stamp.is_some() && stamp == cached_stamp {
        return Some(stamp);
    }
    if size > cache_freshness::CONTENT_HASH_SIZE_CAP {
        return None;
    }
    let hash = cache_freshness::hash_file_if_small(path, size).ok()??;
    (hash == cached_content_hash && (stamp.is_none() || Stamp::read(path) == stamp))
        .then_some(stamp)
}
