use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
#[cfg(test)]
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::fs_lock;
use crate::parser::SymbolCache;
use crate::search_index::{cache_relative_path, validate_cached_relative_path};
use crate::slog_warn;
use crate::symbols::Symbol;

const MAGIC: &[u8; 8] = b"AFTSYM1\0";
const FORMAT_VERSION: u32 = 3;

/// Version of the symbol extraction schema stored in the disk cache.
///
/// Bump this whenever symbol-extraction logic changes: tree-sitter grammar
/// upgrades, query updates, extractor behavior, or symbol shape changes. A
/// mismatch rejects persisted symbols so they are regenerated on next access.
pub const SCHEMA_VERSION: u32 = 4;

const MAX_ENTRIES: usize = 2_000_000;
const MAX_PATH_BYTES: usize = 16 * 1024;
const MAX_SYMBOL_BYTES: usize = 16 * 1024 * 1024;
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static SYMBOL_LOCK_ACQUIRE_MUTEX: Mutex<()> = Mutex::new(());
#[cfg(test)]
static WATCHED_CACHE_WRITE: OnceLock<Mutex<Option<(PathBuf, usize)>>> = OnceLock::new();

pub struct SymbolCacheLock {
    _guard: Option<fs_lock::LockGuard>,
}

impl SymbolCacheLock {
    pub fn acquire(
        storage_dir: &Path,
        project_key: &str,
        project_root: &Path,
    ) -> std::io::Result<Self> {
        let dir = storage_dir.join("symbols").join(project_key);
        let path = dir.join("symbols.lock");
        let access = crate::root_cache::ArtifactAccess::for_root(project_root);
        if !access.allows_write(project_key, &path) {
            return Ok(Self { _guard: None });
        }
        fs::create_dir_all(&dir)?;
        let _acquire_guard = SYMBOL_LOCK_ACQUIRE_MUTEX
            .lock()
            .map_err(|_| std::io::Error::other("symbol cache lock acquisition mutex poisoned"))?;
        fs_lock::try_acquire(&path, Duration::from_secs(2))
            .map(|guard| Self {
                _guard: Some(guard),
            })
            .map_err(|error| match error {
                fs_lock::AcquireError::Timeout => {
                    std::io::Error::other("timed out acquiring symbol cache lock")
                }
                fs_lock::AcquireError::Io(error) => error,
            })
    }
}

#[derive(Debug, Clone)]
pub struct DiskSymbolCache {
    pub(crate) entries: Vec<DiskSymbolEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct DiskSymbolEntry {
    pub(crate) relative_path: PathBuf,
    pub(crate) mtime: SystemTime,
    pub(crate) size: u64,
    pub(crate) content_hash: blake3::Hash,
    pub(crate) symbols: Vec<Symbol>,
}

impl DiskSymbolCache {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub(crate) fn cache_path(storage_dir: &Path, project_key: &str) -> PathBuf {
    storage_dir
        .join("symbols")
        .join(project_key)
        .join("symbols.bin")
}

#[cfg(test)]
pub(crate) fn watch_cache_writes(path: PathBuf) {
    *WATCHED_CACHE_WRITE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((path, 0));
}

#[cfg(test)]
pub(crate) fn watched_cache_write_count() -> usize {
    WATCHED_CACHE_WRITE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map_or(0, |(_, count)| *count)
}

#[cfg(test)]
fn note_cache_write(path: &Path) {
    let mut watched = WATCHED_CACHE_WRITE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((watched_path, count)) = watched.as_mut() {
        if watched_path == path {
            *count += 1;
        }
    }
}

pub fn read_from_disk(storage_dir: &Path, project_key: &str) -> Option<DiskSymbolCache> {
    let data_path = cache_path(storage_dir, project_key);
    if !data_path.exists() {
        return None;
    }

    match read_cache_file(&data_path) {
        Ok(cache) => Some(cache),
        Err(error) => {
            slog_warn!(
                "corrupt symbol cache at {}: {}, rebuilding",
                data_path.display(),
                error
            );
            None
        }
    }
}

pub fn write_to_disk(
    cache: &SymbolCache,
    storage_dir: &Path,
    project_key: &str,
) -> std::io::Result<()> {
    let project_root = cache.project_root().ok_or_else(|| {
        std::io::Error::other("symbol cache project root is not set; cannot persist relative paths")
    })?;

    let dir = storage_dir.join("symbols").join(project_key);
    let data_path = dir.join("symbols.bin");
    let access = crate::root_cache::ArtifactAccess::for_root(&project_root);
    if !access.allows_write(project_key, &data_path) {
        return Ok(());
    }
    #[cfg(test)]
    note_cache_write(&data_path);
    let _cache_lock = SymbolCacheLock::acquire(storage_dir, project_key, &project_root)?;
    fs::create_dir_all(&dir)?;
    let tmp_path = dir.join(format!(
        "symbols.bin.tmp.{}.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let write_result = write_cache_file(cache, &project_root, &tmp_path).and_then(|()| {
        fs::rename(&tmp_path, &data_path)?;
        if let Ok(dir_file) = File::open(&dir) {
            let _ = dir_file.sync_all();
        }
        Ok(())
    });

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    } else {
        let bytes = fs::metadata(&data_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        crate::write_ledger::credit(
            crate::write_ledger::Domain::SymbolCache,
            project_root.display().to_string(),
            bytes,
            0,
        );
    }

    write_result
}

fn read_cache_file(path: &Path) -> Result<DiskSymbolCache, String> {
    let mut reader = BufReader::new(File::open(path).map_err(|error| error.to_string())?);

    let mut magic = [0u8; 8];
    reader
        .read_exact(&mut magic)
        .map_err(|error| format!("failed to read symbol cache magic: {error}"))?;
    if &magic != MAGIC {
        return Err("invalid symbol cache magic".to_string());
    }

    let format_version = read_u32(&mut reader)?;
    if format_version != FORMAT_VERSION {
        return Err(format!(
            "unsupported symbol cache format version: {format_version} (expected {FORMAT_VERSION})"
        ));
    }

    let schema_version = read_u32(&mut reader)?;
    if schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported symbol cache schema version: {schema_version} (expected {SCHEMA_VERSION})"
        ));
    }

    let root_len = read_u32(&mut reader)? as usize;
    let entry_count = read_u32(&mut reader)? as usize;
    if root_len > MAX_PATH_BYTES {
        return Err(format!("project root path too large: {root_len} bytes"));
    }
    if entry_count > MAX_ENTRIES {
        return Err(format!("too many symbol cache entries: {entry_count}"));
    }

    let _project_root = PathBuf::from(read_string_with_len(&mut reader, root_len)?);
    let mut entries = Vec::with_capacity(entry_count);

    for _ in 0..entry_count {
        let path_len = read_u32(&mut reader)? as usize;
        if path_len > MAX_PATH_BYTES {
            return Err(format!("cached path too large: {path_len} bytes"));
        }
        let relative_path = validate_cached_relative_path(&PathBuf::from(read_string_with_len(
            &mut reader,
            path_len,
        )?))
        .ok_or_else(|| "cached symbol path escapes project root".to_string())?;
        let mtime_secs = read_i64(&mut reader)?;
        let mtime_nanos = read_u32(&mut reader)?;
        let size = read_u64(&mut reader)?;
        let mut hash_bytes = [0u8; 32];
        reader
            .read_exact(&mut hash_bytes)
            .map_err(|error| format!("failed to read symbol content hash: {error}"))?;
        let content_hash = blake3::Hash::from_bytes(hash_bytes);
        let symbol_bytes_len = read_u32(&mut reader)? as usize;
        if symbol_bytes_len > MAX_SYMBOL_BYTES {
            // One oversized file must not cost every other file its cached
            // symbols. Skip past this payload and let the file be re-extracted
            // on demand. A payload shorter than its declared length is still
            // truncation and rejects the cache below.
            let skipped = std::io::copy(
                &mut reader.by_ref().take(symbol_bytes_len as u64),
                &mut std::io::sink(),
            )
            .map_err(|error| format!("failed to skip oversized symbol payload: {error}"))?;
            if skipped != symbol_bytes_len as u64 {
                return Err(format!(
                    "truncated symbol payload: expected {symbol_bytes_len} bytes, found {skipped}"
                ));
            }
            slog_warn!(
                "skipping cached symbols for {}: payload {} bytes exceeds {} byte limit",
                relative_path.display(),
                symbol_bytes_len,
                MAX_SYMBOL_BYTES
            );
            continue;
        }

        let mut symbol_bytes = vec![0u8; symbol_bytes_len];
        reader
            .read_exact(&mut symbol_bytes)
            .map_err(|error| format!("failed to read symbol payload: {error}"))?;
        let symbols: Vec<Symbol> = serde_json::from_slice(&symbol_bytes)
            .map_err(|error| format!("failed to decode cached symbols: {error}"))?;

        entries.push(DiskSymbolEntry {
            relative_path,
            mtime: system_time_from_parts(mtime_secs, mtime_nanos)?,
            size,
            content_hash,
            symbols,
        });
    }

    Ok(DiskSymbolCache { entries })
}

fn write_cache_file(
    cache: &SymbolCache,
    project_root: &Path,
    tmp_path: &Path,
) -> std::io::Result<()> {
    let mut writer = BufWriter::new(File::create(tmp_path)?);
    let entries = cache
        .disk_entries()
        .into_iter()
        .map(|(path, mtime, size, content_hash, symbols)| {
            cache_relative_path(project_root, path)
                .map(|relative_path| (relative_path, mtime, size, content_hash, symbols))
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| std::io::Error::other("refusing to cache path outside project root"))?;
    let root = project_root.to_string_lossy();
    let root_len = u32::try_from(root.len())
        .map_err(|_| std::io::Error::other("project root too large to cache"))?;
    // Validate the upper bound now; the count actually written can be smaller
    // because oversized entries are left out, so it is patched in afterwards.
    u32::try_from(entries.len())
        .map_err(|_| std::io::Error::other("too many symbol cache entries"))?;

    writer.write_all(MAGIC)?;
    write_u32(&mut writer, FORMAT_VERSION)?;
    write_u32(&mut writer, SCHEMA_VERSION)?;
    write_u32(&mut writer, root_len)?;
    let entry_count_offset = writer.stream_position()?;
    write_u32(&mut writer, 0)?;
    writer.write_all(root.as_bytes())?;

    let mut written_entries: u32 = 0;
    for (relative_path, mtime, size, content_hash, symbols) in entries {
        let path_bytes = relative_path.to_string_lossy();
        let path_len = u32::try_from(path_bytes.len())
            .map_err(|_| std::io::Error::other("cached path too large"))?;
        let (secs, nanos) = system_time_parts(mtime);
        let symbol_bytes = serde_json::to_vec(symbols).map_err(|error| {
            std::io::Error::other(format!("symbol serialization failed: {error}"))
        })?;
        // The reader refuses payloads above MAX_SYMBOL_BYTES, so writing one
        // would only waste disk and startup time. Leave the file out; it is
        // re-extracted when its symbols are needed.
        if symbol_bytes.len() > MAX_SYMBOL_BYTES {
            slog_warn!(
                "not persisting symbols for {}: payload {} bytes exceeds {} byte limit",
                relative_path.display(),
                symbol_bytes.len(),
                MAX_SYMBOL_BYTES
            );
            continue;
        }
        let symbol_len = u32::try_from(symbol_bytes.len())
            .map_err(|_| std::io::Error::other("cached symbol payload too large"))?;

        write_u32(&mut writer, path_len)?;
        writer.write_all(path_bytes.as_bytes())?;
        write_i64(&mut writer, secs)?;
        write_u32(&mut writer, nanos)?;
        write_u64(&mut writer, size)?;
        writer.write_all(content_hash.as_bytes())?;
        write_u32(&mut writer, symbol_len)?;
        writer.write_all(&symbol_bytes)?;
        written_entries += 1;
    }

    writer.seek(SeekFrom::Start(entry_count_offset))?;
    write_u32(&mut writer, written_entries)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn system_time_parts(time: SystemTime) -> (i64, u32) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
            duration.subsec_nanos(),
        ),
        Err(error) => {
            let duration = error.duration();
            let nanos = duration.subsec_nanos();
            if nanos == 0 {
                (-(duration.as_secs() as i64), 0)
            } else {
                (-(duration.as_secs() as i64) - 1, 1_000_000_000 - nanos)
            }
        }
    }
}

fn system_time_from_parts(secs: i64, nanos: u32) -> Result<SystemTime, String> {
    if nanos >= 1_000_000_000 {
        return Err(format!(
            "invalid symbol cache mtime nanos: {nanos} >= 1_000_000_000"
        ));
    }

    if secs >= 0 {
        let duration = Duration::new(secs as u64, nanos);
        UNIX_EPOCH
            .checked_add(duration)
            .ok_or_else(|| format!("symbol cache mtime overflows SystemTime: {secs}.{nanos}"))
    } else {
        let whole = Duration::new(secs.unsigned_abs(), 0);
        let base = UNIX_EPOCH.checked_sub(whole).ok_or_else(|| {
            format!("symbol cache negative mtime overflows SystemTime: {secs}.{nanos}")
        })?;
        base.checked_add(Duration::new(0, nanos)).ok_or_else(|| {
            format!("symbol cache negative mtime overflows SystemTime: {secs}.{nanos}")
        })
    }
}

fn read_string_with_len<R: Read>(reader: &mut R, len: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; len];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| format!("failed to read string: {error}"))?;
    String::from_utf8(bytes).map_err(|error| format!("invalid utf-8 string: {error}"))
}

fn read_u32<R: Read>(reader: &mut R) -> Result<u32, String> {
    let mut bytes = [0u8; 4];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| format!("failed to read u32: {error}"))?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_i64<R: Read>(reader: &mut R) -> Result<i64, String> {
    let mut bytes = [0u8; 8];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| format!("failed to read i64: {error}"))?;
    Ok(i64::from_le_bytes(bytes))
}

fn read_u64<R: Read>(reader: &mut R) -> Result<u64, String> {
    let mut bytes = [0u8; 8];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| format!("failed to read u64: {error}"))?;
    Ok(u64::from_le_bytes(bytes))
}

fn write_u32<W: Write>(writer: &mut W, value: u32) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_i64<W: Write>(writer: &mut W, value: i64) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u64<W: Write>(writer: &mut W, value: u64) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::{Range, SymbolKind};

    fn test_symbol(name: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            range: Range {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 1,
            },
            signature: None,
            scope_chain: Vec::new(),
            exported: false,
            parent: None,
        }
    }

    fn test_cache(project: &Path, file_name: &str) -> SymbolCache {
        let file = project.join(file_name);
        fs::write(&file, format!("fn {file_name}() {{}}\n")).expect("write file");
        let metadata = fs::metadata(&file).expect("metadata");
        let content_hash = blake3::hash(&fs::read(&file).expect("read file"));
        let mut cache = SymbolCache::new();
        cache.set_project_root(project.to_path_buf());
        cache.insert(
            file,
            metadata.modified().expect("mtime"),
            metadata.len(),
            content_hash,
            vec![test_symbol(file_name)],
        );
        cache
    }

    #[test]
    fn borrow_only_root_skips_symbol_lock_and_persist() {
        let project = tempfile::tempdir().expect("project");
        let storage = tempfile::tempdir().expect("storage");
        let project_key = "shared-artifact-key".to_string();
        crate::root_cache::configure_artifact_access(project.path(), &project_key, true);
        let cache = test_cache(project.path(), "lib.rs");

        let _lock = SymbolCacheLock::acquire(storage.path(), &project_key, project.path())
            .expect("borrow-only lock downgrade");
        let cache_dir = storage.path().join("symbols").join(&project_key);
        assert!(!cache_dir.join("symbols.lock").exists());

        write_to_disk(&cache, storage.path(), &project_key).expect("borrow-only persist downgrade");

        assert!(!cache_dir.join("symbols.bin").exists());
        assert!(!cache_dir.exists());
    }

    #[test]
    fn concurrent_symbol_cache_writes_do_not_share_temp_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project");
        let storage = dir.path().join("storage");

        let cache_a = test_cache(&project, "a");
        let cache_b = test_cache(&project, "b");
        let storage_a = storage.clone();
        let writer_a = std::thread::spawn(move || {
            write_to_disk(&cache_a, &storage_a, "unit-project").expect("write a");
        });
        let storage_b = storage.clone();
        let writer_b = std::thread::spawn(move || {
            write_to_disk(&cache_b, &storage_b, "unit-project").expect("write b");
        });

        writer_a.join().expect("writer a");
        writer_b.join().expect("writer b");

        let loaded = read_from_disk(&storage, "unit-project").expect("load symbol cache");
        assert_eq!(loaded.len(), 1);
        assert!(fs::read_dir(storage.join("symbols").join("unit-project"))
            .expect("read symbol cache dir")
            .all(|entry| !entry
                .expect("cache entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")));
    }

    #[test]
    fn symbol_cache_rejects_mismatched_schema_version() {
        let storage = tempfile::tempdir().expect("create storage dir");
        let path = cache_path(storage.path(), "schema-project");
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");

        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&SCHEMA_VERSION.wrapping_add(1).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        fs::write(&path, bytes).expect("write wrong-schema cache");

        assert!(read_from_disk(storage.path(), "schema-project").is_none());
    }

    #[test]
    fn symbol_cache_rejects_paths_outside_project_root_on_write() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project");
        let outside = dir.path().join("outside.rs");
        fs::write(&outside, "fn outside() {}\n").expect("write outside");
        let metadata = fs::metadata(&outside).expect("metadata");

        let mut cache = SymbolCache::new();
        cache.set_project_root(project);
        cache.insert(
            outside.clone(),
            metadata.modified().expect("mtime"),
            metadata.len(),
            blake3::hash(&fs::read(&outside).expect("read outside")),
            vec![test_symbol("outside")],
        );

        let error = write_to_disk(&cache, dir.path(), "escape-project").expect_err("reject escape");
        assert!(error.to_string().contains("outside project root"));
    }

    /// A symbol whose serialized payload is just over the per-entry limit.
    fn oversized_symbol() -> Symbol {
        test_symbol(&"x".repeat(MAX_SYMBOL_BYTES + 1))
    }

    fn insert_file(cache: &mut SymbolCache, project: &Path, file_name: &str, symbol: Symbol) {
        let file = project.join(file_name);
        fs::write(&file, format!("fn {file_name}() {{}}\n")).expect("write file");
        let metadata = fs::metadata(&file).expect("metadata");
        cache.insert(
            file.clone(),
            metadata.modified().expect("mtime"),
            metadata.len(),
            blake3::hash(&fs::read(&file).expect("read file")),
            vec![symbol],
        );
    }

    /// Append one entry in the on-disk layout without going through the writer,
    /// so a test can reproduce a cache that an older writer produced.
    fn push_raw_entry(bytes: &mut Vec<u8>, relative_path: &str, symbols: &[Symbol]) {
        let payload = serde_json::to_vec(symbols).expect("serialize symbols");
        bytes.extend_from_slice(&(relative_path.len() as u32).to_le_bytes());
        bytes.extend_from_slice(relative_path.as_bytes());
        bytes.extend_from_slice(&1_700_000_000i64.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&16u64.to_le_bytes());
        bytes.extend_from_slice(blake3::hash(relative_path.as_bytes()).as_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
    }

    fn raw_cache_header(entry_count: u32) -> Vec<u8> {
        let root = "/project";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(root.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&entry_count.to_le_bytes());
        bytes.extend_from_slice(root.as_bytes());
        bytes
    }

    #[test]
    fn oversized_entry_on_disk_is_skipped_without_discarding_other_entries() {
        let storage = tempfile::tempdir().expect("create storage dir");
        let path = cache_path(storage.path(), "oversized-read");
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");

        let mut bytes = raw_cache_header(3);
        push_raw_entry(&mut bytes, "a.rs", &[test_symbol("a")]);
        push_raw_entry(&mut bytes, "huge.js", &[oversized_symbol()]);
        push_raw_entry(&mut bytes, "b.rs", &[test_symbol("b")]);
        fs::write(&path, bytes).expect("write cache");

        let loaded = read_from_disk(storage.path(), "oversized-read")
            .expect("one oversized entry must not discard the whole cache");
        let paths: Vec<_> = loaded
            .entries
            .iter()
            .map(|entry| entry.relative_path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["a.rs".to_string(), "b.rs".to_string()]);
        assert_eq!(loaded.entries[1].symbols[0].name, "b");
    }

    #[test]
    fn oversized_entry_is_not_persisted_and_other_entries_round_trip() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project");
        let storage = dir.path().join("storage");

        let mut cache = SymbolCache::new();
        cache.set_project_root(project.clone());
        insert_file(&mut cache, &project, "a.rs", test_symbol("a"));
        insert_file(&mut cache, &project, "huge.js", oversized_symbol());
        insert_file(&mut cache, &project, "b.rs", test_symbol("b"));

        write_to_disk(&cache, &storage, "oversized-write").expect("write cache");

        let written = fs::metadata(cache_path(&storage, "oversized-write"))
            .expect("cache file")
            .len();
        assert!(
            written < MAX_SYMBOL_BYTES as u64,
            "oversized payload was written to disk ({written} bytes)"
        );
        let loaded =
            read_from_disk(&storage, "oversized-write").expect("cache without oversized entry");
        let mut paths: Vec<_> = loaded
            .entries
            .iter()
            .map(|entry| entry.relative_path.to_string_lossy().into_owned())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["a.rs".to_string(), "b.rs".to_string()]);
    }

    #[test]
    fn truncated_entry_still_rejects_the_whole_cache() {
        let storage = tempfile::tempdir().expect("create storage dir");
        let path = cache_path(storage.path(), "truncated");
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");

        let mut bytes = raw_cache_header(2);
        push_raw_entry(&mut bytes, "a.rs", &[test_symbol("a")]);
        push_raw_entry(&mut bytes, "huge.js", &[oversized_symbol()]);
        // Cut the oversized payload short: skipping it must notice the missing
        // bytes instead of treating a truncated file as a valid cache.
        bytes.truncate(bytes.len() - 10);
        fs::write(&path, bytes).expect("write cache");

        assert!(read_from_disk(storage.path(), "truncated").is_none());
    }
}
