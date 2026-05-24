use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::fs_lock;
use crate::parser::SymbolCache;
use crate::search_index::{cache_relative_path, validate_cached_relative_path};
use crate::symbols::Symbol;
use crate::{slog_info, slog_warn};

const MAGIC: &[u8; 8] = b"AFTSYM1\0";
const VERSION: u32 = 2;
const MAX_ENTRIES: usize = 2_000_000;
const MAX_PATH_BYTES: usize = 16 * 1024;
const MAX_SYMBOL_BYTES: usize = 16 * 1024 * 1024;
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static SYMBOL_LOCK_ACQUIRE_MUTEX: Mutex<()> = Mutex::new(());

pub struct SymbolCacheLock {
    _guard: fs_lock::LockGuard,
}

impl SymbolCacheLock {
    pub fn acquire(storage_dir: &Path, project_key: &str) -> std::io::Result<Self> {
        let dir = storage_dir.join("symbols").join(project_key);
        fs::create_dir_all(&dir)?;
        let path = dir.join("symbols.lock");
        let _acquire_guard = SYMBOL_LOCK_ACQUIRE_MUTEX
            .lock()
            .map_err(|_| std::io::Error::other("symbol cache lock acquisition mutex poisoned"))?;
        fs_lock::try_acquire(&path, Duration::from_secs(2))
            .map(|guard| Self { _guard: guard })
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
    if cache.len() == 0 {
        slog_info!("skipping symbol cache persistence (0 entries)");
        return Ok(());
    }

    let project_root = cache.project_root().ok_or_else(|| {
        std::io::Error::other("symbol cache project root is not set; cannot persist relative paths")
    })?;

    let _cache_lock = SymbolCacheLock::acquire(storage_dir, project_key)?;
    let dir = storage_dir.join("symbols").join(project_key);
    fs::create_dir_all(&dir)?;

    let data_path = dir.join("symbols.bin");
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

    let version = read_u32(&mut reader)?;
    if version != VERSION {
        return Err(format!(
            "unsupported symbol cache version: {version} (expected {VERSION})"
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
            return Err(format!(
                "cached symbol payload too large: {symbol_bytes_len} bytes"
            ));
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
    let entry_count = u32::try_from(entries.len())
        .map_err(|_| std::io::Error::other("too many symbol cache entries"))?;

    writer.write_all(MAGIC)?;
    write_u32(&mut writer, VERSION)?;
    write_u32(&mut writer, root_len)?;
    write_u32(&mut writer, entry_count)?;
    writer.write_all(root.as_bytes())?;

    for (relative_path, mtime, size, content_hash, symbols) in entries {
        let path_bytes = relative_path.to_string_lossy();
        let path_len = u32::try_from(path_bytes.len())
            .map_err(|_| std::io::Error::other("cached path too large"))?;
        let (secs, nanos) = system_time_parts(mtime);
        let symbol_bytes = serde_json::to_vec(symbols).map_err(|error| {
            std::io::Error::other(format!("symbol serialization failed: {error}"))
        })?;
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
    }

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
}
