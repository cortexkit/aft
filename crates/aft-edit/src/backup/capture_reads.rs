use super::*;

fn read_captured_content(path: &Path, metadata: &std::fs::Metadata) -> std::io::Result<Vec<u8>> {
    refuse_storage_path_with_metadata(path, Some(metadata)).map_err(std::io::Error::other)?;
    #[cfg(any(test, feature = "test-support"))]
    {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        *CAPTURE_READ_COUNTS.lock().unwrap().entry(key).or_default() += 1;
    }
    std::fs::read(path)
}

#[cfg(any(test, feature = "test-support"))]
static CAPTURE_READ_COUNTS: LazyLock<Mutex<HashMap<PathBuf, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn reset_capture_read_count(path: &Path) {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    CAPTURE_READ_COUNTS.lock().unwrap().remove(&key);
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn capture_read_count(path: &Path) -> usize {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    CAPTURE_READ_COUNTS
        .lock()
        .unwrap()
        .get(&key)
        .copied()
        .unwrap_or_default()
}
