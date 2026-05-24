use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const CONTENT_HASH_SIZE_CAP: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFreshness {
    pub mtime: SystemTime,
    pub size: u64,
    pub content_hash: blake3::Hash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshnessVerdict {
    HotFresh,
    ContentFresh {
        new_mtime: SystemTime,
        new_size: u64,
    },
    Stale,
    Deleted,
}

pub fn hash_bytes(bytes: &[u8]) -> blake3::Hash {
    blake3::hash(bytes)
}

pub fn hash_file_if_small(path: &Path, size: u64) -> std::io::Result<Option<blake3::Hash>> {
    if size > CONTENT_HASH_SIZE_CAP {
        return Ok(None);
    }
    fs::read(path).map(|bytes| Some(hash_bytes(&bytes)))
}

pub fn zero_hash() -> blake3::Hash {
    blake3::Hash::from_bytes([0u8; 32])
}

pub fn collect(path: &Path) -> std::io::Result<FileFreshness> {
    let metadata = fs::metadata(path)?;
    let mtime = metadata.modified().unwrap_or(UNIX_EPOCH);
    let size = metadata.len();
    let content_hash = hash_file_if_small(path, size)?.unwrap_or_else(zero_hash);
    Ok(FileFreshness {
        mtime,
        size,
        content_hash,
    })
}

pub fn verify_file(path: &Path, cached: &FileFreshness) -> FreshnessVerdict {
    let Ok(metadata) = fs::metadata(path) else {
        return FreshnessVerdict::Deleted;
    };
    let new_size = metadata.len();
    let new_mtime = metadata.modified().unwrap_or(UNIX_EPOCH);
    if new_size == cached.size && new_mtime == cached.mtime {
        return FreshnessVerdict::HotFresh;
    }
    if new_size != cached.size || new_size > CONTENT_HASH_SIZE_CAP {
        return FreshnessVerdict::Stale;
    }
    match hash_file_if_small(path, new_size) {
        Ok(Some(hash)) if hash == cached.content_hash => FreshnessVerdict::ContentFresh {
            new_mtime,
            new_size,
        },
        _ => FreshnessVerdict::Stale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn hot_fresh_when_mtime_size_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"same");
        let fresh = collect(&path).unwrap();
        assert_eq!(verify_file(&path, &fresh), FreshnessVerdict::HotFresh);
    }

    #[test]
    fn content_fresh_when_only_mtime_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"same");
        let fresh = collect(&path).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"").unwrap();
        file.sync_all().unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        assert!(matches!(
            verify_file(&path, &fresh),
            FreshnessVerdict::ContentFresh { .. }
        ));
    }

    #[test]
    fn stale_when_size_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"same");
        let fresh = collect(&path).unwrap();
        write(&path, b"different");
        assert_eq!(verify_file(&path, &fresh), FreshnessVerdict::Stale);
    }

    #[test]
    fn deleted_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"same");
        let fresh = collect(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(verify_file(&path, &fresh), FreshnessVerdict::Deleted);
    }

    #[test]
    fn over_cap_hash_is_not_computed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        fs::write(&path, vec![0u8; CONTENT_HASH_SIZE_CAP as usize + 1]).unwrap();
        assert!(hash_file_if_small(&path, CONTENT_HASH_SIZE_CAP + 1)
            .unwrap()
            .is_none());
    }
}
