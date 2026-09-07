use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::db::{SqliteStore, TrackedConnection};
#[cfg(test)]
use rusqlite::Connection;
use rusqlite::OpenFlags;
use serde_json::json;

const ROOT_KEYED_COPY_DISK_FLOOR_NUMERATOR: u64 = 3;
const ROOT_KEYED_COPY_DISK_FLOOR_DENOMINATOR: u64 = 2;
const SQLITE_SUFFIXES: [&str; 4] = [".sqlite-wal", ".sqlite-shm", ".sqlite-journal", ".sqlite"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyPartitionKind {
    Callgraph,
    Inspect,
}

impl LegacyPartitionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Callgraph => "callgraph",
            Self::Inspect => "inspect",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyPartitionInventoryEntry {
    pub harness: String,
    pub kind: LegacyPartitionKind,
    pub key: String,
    /// Logical partition path. The current legacy layout stores flat files, but
    /// callers treat `<storage>/<harness>/<domain>/<key>` as the partition ID.
    pub path: PathBuf,
    pub bytes: u64,
    pub callgraph_pointer_mtime: Option<SystemTime>,
    pub inspect_tier2_last_full_run: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyHarnessDuplication {
    pub harness: String,
    pub partitions: usize,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiskFloorDecision {
    pub source_bytes: u64,
    pub available_bytes: u64,
    pub required_bytes: u64,
}

impl DiskFloorDecision {
    pub fn allows_copy(self) -> bool {
        self.available_bytes >= self.required_bytes
    }

    pub fn should_skip_copy(self) -> bool {
        !self.allows_copy()
    }

    pub fn warning_message(self, source: &Path, target: &Path) -> String {
        format!(
            "Skipping root-keyed cache copy from {} into {}: free disk ({}) is below the required 1.5× floor ({} for {} source bytes).",
            source.display(),
            target.display(),
            self.available_bytes,
            self.required_bytes,
            self.source_bytes
        )
    }

    pub fn configure_warning(self, source: &Path, target: &Path) -> serde_json::Value {
        json!({
            "kind": "root_keyed_disk_floor",
            "source_path": source.display().to_string(),
            "target_path": target.display().to_string(),
            "bytes_source": self.source_bytes,
            "bytes_free": self.available_bytes,
            "bytes_required": self.required_bytes,
            "message": self.warning_message(source, target),
        })
    }
}

pub fn required_root_keyed_copy_free_bytes(source_bytes: u64) -> u64 {
    source_bytes
        .saturating_mul(ROOT_KEYED_COPY_DISK_FLOOR_NUMERATOR)
        .saturating_add(ROOT_KEYED_COPY_DISK_FLOOR_DENOMINATOR - 1)
        / ROOT_KEYED_COPY_DISK_FLOOR_DENOMINATOR
}

pub fn evaluate_root_keyed_copy_disk_floor(
    source_bytes: u64,
    available_bytes: u64,
) -> DiskFloorDecision {
    DiskFloorDecision {
        source_bytes,
        available_bytes,
        required_bytes: required_root_keyed_copy_free_bytes(source_bytes),
    }
}

/// Read free bytes for `path` from the filesystem containing the nearest
/// existing ancestor. Future root-keyed migration/copy paths use this seam for
/// the 1.5× disk-floor preflight.
pub fn available_disk_for(path: &Path) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let probe = existing_ancestor(path);
        let c_path = CString::new(probe.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        let result = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
    }

    #[cfg(windows)]
    {
        let _ = path;
        // Root-keyed migration is not wired on Windows yet. Mirror the existing
        // storage-migration posture so call sites can remain total until the
        // Windows-specific disk probe lands.
        Ok(u64::MAX)
    }
}

/// Cheap structural predicate for the coexistence window: true when `candidate`
/// points anywhere under `<storage>/<harness>/(callgraph|inspect)`.
pub fn is_legacy_harness_partition_path(storage_root: &Path, candidate: &Path) -> bool {
    let storage_root = lexical_normalize(storage_root);
    let candidate = if candidate.is_absolute() {
        lexical_normalize(candidate)
    } else {
        lexical_normalize(&storage_root.join(candidate))
    };

    let Ok(relative) = candidate.strip_prefix(&storage_root) else {
        return false;
    };

    let mut components = relative.components();
    let Some(Component::Normal(_harness)) = components.next() else {
        return false;
    };
    let Some(Component::Normal(domain)) = components.next() else {
        return false;
    };

    matches!(domain.to_str(), Some("callgraph" | "inspect"))
}

#[track_caller]
pub fn debug_assert_not_legacy_harness_partition_path(storage_root: &Path, candidate: &Path) {
    debug_assert!(
        !is_legacy_harness_partition_path(storage_root, candidate),
        "new-layout write path must not point into a legacy harness partition: {}",
        candidate.display()
    );
}

pub fn refuse_legacy_partition_write(
    storage_root: &Path,
    candidate: &Path,
    operation: &str,
) -> io::Result<()> {
    if is_legacy_harness_partition_path(storage_root, candidate) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing {operation} into legacy harness partition {}",
                candidate.display()
            ),
        ));
    }
    Ok(())
}

#[track_caller]
pub fn guard_new_layout_write_path(
    storage_root: &Path,
    candidate: &Path,
    operation: &str,
) -> io::Result<()> {
    debug_assert_not_legacy_harness_partition_path(storage_root, candidate);
    refuse_legacy_partition_write(storage_root, candidate, operation)
}

pub fn inventory_legacy_partitions(
    storage_root: &Path,
) -> io::Result<Vec<LegacyPartitionInventoryEntry>> {
    let storage_root = lexical_normalize(storage_root);
    let boundary = match crate::walk_boundary::DeviceBoundary::for_root(&storage_root) {
        Ok(boundary) => boundary,
        // Keep the existing empty-inventory behavior for a storage root that has
        // not been created yet; no recursive walk has started in that case.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut entries = Vec::new();
    for harness_entry in sorted_read_dir(&storage_root)? {
        if !harness_entry.file_type()?.is_dir() {
            continue;
        }
        let harness = harness_entry.file_name().to_string_lossy().to_string();
        let harness_path = harness_entry.path();
        // A mounted child can disappear while recursive inventory holds its
        // ReadDir, causing closedir ENXIO to abort in Drop. Do not enter it.
        if !boundary.should_descend(&harness_path)? {
            crate::slog_warn!(
                "legacy partition inventory skipped foreign filesystem mount {}",
                harness_path.display()
            );
            continue;
        }
        let callgraph_path = harness_path.join(LegacyPartitionKind::Callgraph.as_str());
        if callgraph_path.is_dir() && boundary.should_descend(&callgraph_path)? {
            entries.extend(scan_legacy_callgraph_partitions(
                &harness,
                &callgraph_path,
                &boundary,
            )?);
        }
        let inspect_path = harness_path.join(LegacyPartitionKind::Inspect.as_str());
        if inspect_path.is_dir() && boundary.should_descend(&inspect_path)? {
            entries.extend(scan_legacy_inspect_partitions(
                &harness,
                &inspect_path,
                &boundary,
            )?);
        }
    }
    entries.sort_by(|left, right| {
        left.harness
            .cmp(&right.harness)
            .then_with(|| left.kind.as_str().cmp(right.kind.as_str()))
            .then_with(|| left.key.cmp(&right.key))
    });
    Ok(entries)
}

pub fn summarize_legacy_partition_duplication(
    storage_root: &Path,
) -> io::Result<Vec<LegacyHarnessDuplication>> {
    let mut summaries = BTreeMap::<String, LegacyHarnessDuplication>::new();
    for entry in inventory_legacy_partitions(storage_root)? {
        let summary =
            summaries
                .entry(entry.harness.clone())
                .or_insert_with(|| LegacyHarnessDuplication {
                    harness: entry.harness.clone(),
                    partitions: 0,
                    bytes: 0,
                });
        summary.partitions += 1;
        summary.bytes = summary.bytes.saturating_add(entry.bytes);
    }
    Ok(summaries.into_values().collect())
}

#[derive(Clone, Debug, Default)]
struct PartitionAccumulator {
    bytes: u64,
    callgraph_pointer_mtime: Option<SystemTime>,
    inspect_tier2_last_full_run: Option<i64>,
}

fn scan_legacy_callgraph_partitions(
    harness: &str,
    callgraph_dir: &Path,
    boundary: &crate::walk_boundary::DeviceBoundary,
) -> io::Result<Vec<LegacyPartitionInventoryEntry>> {
    let mut partitions = BTreeMap::<String, PartitionAccumulator>::new();
    for entry in sorted_read_dir(callgraph_dir)? {
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().to_string();
        if file_type.is_dir() {
            if !looks_like_partition_key(&name) {
                continue;
            }
            let partition = partitions.entry(name.clone()).or_default();
            partition.bytes = partition
                .bytes
                .saturating_add(tree_size(&entry.path(), boundary)?);
            if partition.callgraph_pointer_mtime.is_none() {
                partition.callgraph_pointer_mtime = callgraph_pointer_mtime(callgraph_dir, &name);
            }
            continue;
        }

        let Some(key) = callgraph_partition_key_from_name(&name) else {
            continue;
        };
        let partition = partitions.entry(key.clone()).or_default();
        partition.bytes = partition.bytes.saturating_add(file_size(&entry.path())?);
        if name.ends_with(".current") {
            partition.callgraph_pointer_mtime =
                entry.metadata().and_then(|meta| meta.modified()).ok();
        }
    }

    Ok(partitions
        .into_iter()
        .map(|(key, partition)| LegacyPartitionInventoryEntry {
            harness: harness.to_string(),
            kind: LegacyPartitionKind::Callgraph,
            path: callgraph_dir.join(&key),
            key,
            bytes: partition.bytes,
            callgraph_pointer_mtime: partition.callgraph_pointer_mtime,
            inspect_tier2_last_full_run: None,
        })
        .collect())
}

fn scan_legacy_inspect_partitions(
    harness: &str,
    inspect_dir: &Path,
    boundary: &crate::walk_boundary::DeviceBoundary,
) -> io::Result<Vec<LegacyPartitionInventoryEntry>> {
    let mut partitions = BTreeMap::<String, PartitionAccumulator>::new();
    for entry in sorted_read_dir(inspect_dir)? {
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().to_string();
        if file_type.is_dir() {
            if !looks_like_partition_key(&name) {
                continue;
            }
            let partition = partitions.entry(name.clone()).or_default();
            partition.bytes = partition
                .bytes
                .saturating_add(tree_size(&entry.path(), boundary)?);
            if partition.inspect_tier2_last_full_run.is_none() {
                partition.inspect_tier2_last_full_run =
                    inspect_tier2_last_full_run(inspect_dir, &name);
            }
            continue;
        }

        let Some(key) = inspect_partition_key_from_name(&name) else {
            continue;
        };
        let partition = partitions.entry(key.clone()).or_default();
        partition.bytes = partition.bytes.saturating_add(file_size(&entry.path())?);
    }

    Ok(partitions
        .into_iter()
        .map(|(key, mut partition)| {
            if partition.inspect_tier2_last_full_run.is_none() {
                partition.inspect_tier2_last_full_run =
                    inspect_tier2_last_full_run(inspect_dir, &key);
            }
            LegacyPartitionInventoryEntry {
                harness: harness.to_string(),
                kind: LegacyPartitionKind::Inspect,
                path: inspect_dir.join(&key),
                key,
                bytes: partition.bytes,
                callgraph_pointer_mtime: None,
                inspect_tier2_last_full_run: partition.inspect_tier2_last_full_run,
            }
        })
        .collect())
}

fn callgraph_pointer_mtime(callgraph_dir: &Path, key: &str) -> Option<SystemTime> {
    for candidate in [
        callgraph_dir.join(format!("{key}.current")),
        callgraph_dir.join(key).join(format!("{key}.current")),
    ] {
        if let Ok(modified) = fs::metadata(candidate).and_then(|metadata| metadata.modified()) {
            return Some(modified);
        }
    }
    None
}

fn inspect_tier2_last_full_run(inspect_dir: &Path, key: &str) -> Option<i64> {
    let sqlite_path = [
        inspect_dir.join(format!("{key}.sqlite")),
        inspect_dir.join(key).join(format!("{key}.sqlite")),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())?;

    let conn = TrackedConnection::open_path_with_flags(
        &sqlite_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
        SqliteStore::InspectScopeCache,
    )
    .ok()?;
    conn.busy_timeout(Duration::from_millis(500)).ok()?;
    conn.query_row("SELECT MAX(last_full_run) FROM tier2_meta", [], |row| {
        row.get::<_, Option<i64>>(0)
    })
    .ok()
    .flatten()
}

fn callgraph_partition_key_from_name(name: &str) -> Option<String> {
    if name.contains(".tmp.") {
        return None;
    }
    if let Some(key) = name.strip_suffix(".current") {
        return looks_like_partition_key(key).then(|| key.to_string());
    }
    let base = sqliteish_base_name(name)?;
    let key = if let Some((candidate, generation)) = base.split_once(".g") {
        if generation.is_empty() {
            return None;
        }
        candidate
    } else {
        base
    };
    looks_like_partition_key(key).then(|| key.to_string())
}

fn inspect_partition_key_from_name(name: &str) -> Option<String> {
    if name.contains(".tmp.") {
        return None;
    }
    let base = sqliteish_base_name(name)?;
    looks_like_partition_key(base).then(|| base.to_string())
}

fn sqliteish_base_name(name: &str) -> Option<&str> {
    SQLITE_SUFFIXES
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
}

fn looks_like_partition_key(value: &str) -> bool {
    value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn file_size(path: &Path) -> io::Result<u64> {
    Ok(fs::metadata(path)?.len())
}

fn tree_size(path: &Path, boundary: &crate::walk_boundary::DeviceBoundary) -> io::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let metadata = fs::metadata(path)?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }
    if !boundary.should_descend(path)? {
        crate::slog_warn!(
            "legacy partition inventory skipped foreign filesystem mount {}",
            path.display()
        );
        return Ok(0);
    }

    let mut total = 0_u64;
    for entry in fs::read_dir(path)? {
        total = total.saturating_add(tree_size(&entry?.path(), boundary)?);
    }
    Ok(total)
}

fn sorted_read_dir(path: &Path) -> io::Result<Vec<fs::DirEntry>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut entries = entries.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component);
                }
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(unix)]
fn existing_ancestor(path: &Path) -> &Path {
    let mut current = path;
    while !current.exists() {
        if let Some(parent) = current.parent() {
            current = parent;
        } else {
            break;
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::FileTime;
    use rusqlite::params;
    use std::panic::catch_unwind;
    use std::time::UNIX_EPOCH;
    use tempfile::TempDir;

    #[test]
    fn legacy_partition_guard_matches_exact_domains() {
        let storage_root = PathBuf::from("/tmp/aft-storage");
        assert!(is_legacy_harness_partition_path(
            &storage_root,
            &storage_root.join("opencode/callgraph/0123456789abcdef.current")
        ));
        assert!(is_legacy_harness_partition_path(
            &storage_root,
            &storage_root.join("pi/inspect/0123456789abcdef.sqlite")
        ));
        assert!(!is_legacy_harness_partition_path(
            &storage_root,
            &storage_root.join("opencode/callgraph-old/0123456789abcdef.sqlite")
        ));
        assert!(!is_legacy_harness_partition_path(
            &storage_root,
            &storage_root.join("index/0123456789abcdef.sqlite")
        ));
        assert!(!is_legacy_harness_partition_path(
            &storage_root,
            Path::new("/elsewhere/opencode/callgraph/0123456789abcdef.sqlite")
        ));
    }

    #[test]
    fn debug_assert_and_refusal_cover_legacy_write_paths() {
        let storage_root = PathBuf::from("/tmp/aft-storage");
        let legacy_target = storage_root.join("opencode/callgraph/0123456789abcdef.sqlite");
        let panic = catch_unwind(|| {
            debug_assert_not_legacy_harness_partition_path(&storage_root, &legacy_target);
        });
        assert!(panic.is_err());

        let error = refuse_legacy_partition_write(&storage_root, &legacy_target, "publish")
            .expect_err("legacy write must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error
            .to_string()
            .contains("refusing publish into legacy harness partition"));
    }

    #[test]
    fn root_keyed_copy_disk_floor_boundaries() {
        let exact = evaluate_root_keyed_copy_disk_floor(20, 30);
        assert_eq!(exact.required_bytes, 30);
        assert!(exact.allows_copy());
        assert!(!exact.should_skip_copy());

        let below = evaluate_root_keyed_copy_disk_floor(20, 29);
        assert!(below.should_skip_copy());
        assert!(below
            .warning_message(Path::new("/legacy"), Path::new("/shared"))
            .contains("1.5× floor"));

        let above = evaluate_root_keyed_copy_disk_floor(20, 31);
        assert!(above.allows_copy());
        assert_eq!(
            below.configure_warning(Path::new("/legacy"), Path::new("/shared"))["kind"],
            "root_keyed_disk_floor"
        );
    }

    #[test]
    fn inventory_fixture_reports_partition_sizes_and_freshness() {
        let fixture = write_legacy_inventory_fixture();
        let storage_root = fixture.temp.path();

        let inventory = inventory_legacy_partitions(storage_root).expect("inventory");
        assert_eq!(inventory.len(), 2);

        let callgraph = inventory
            .iter()
            .find(|entry| entry.kind == LegacyPartitionKind::Callgraph)
            .expect("callgraph entry");
        let expected_callgraph_bytes = partition_bytes_on_disk(
            &storage_root.join("opencode/callgraph"),
            "0123456789abcdef",
            callgraph_partition_key_from_name,
        );
        assert_eq!(callgraph.harness, "opencode");
        assert_eq!(callgraph.key, "0123456789abcdef");
        assert_eq!(
            callgraph.path,
            storage_root.join("opencode/callgraph/0123456789abcdef")
        );
        assert_eq!(callgraph.bytes, expected_callgraph_bytes);
        let pointer_secs = callgraph
            .callgraph_pointer_mtime
            .expect("pointer mtime")
            .duration_since(UNIX_EPOCH)
            .expect("mtime after epoch")
            .as_secs();
        assert_eq!(pointer_secs, 1_750_000_000);
        assert_eq!(callgraph.inspect_tier2_last_full_run, None);

        let inspect = inventory
            .iter()
            .find(|entry| entry.kind == LegacyPartitionKind::Inspect)
            .expect("inspect entry");
        let minimum_inspect_bytes =
            file_size(&storage_root.join("pi/inspect/fedcba9876543210.sqlite"))
                .expect("inspect sqlite size");
        assert_eq!(inspect.harness, "pi");
        assert_eq!(inspect.key, "fedcba9876543210");
        assert_eq!(
            inspect.path,
            storage_root.join("pi/inspect/fedcba9876543210")
        );
        assert!(inspect.bytes >= minimum_inspect_bytes);
        assert_eq!(inspect.callgraph_pointer_mtime, None);
        assert_eq!(inspect.inspect_tier2_last_full_run, Some(250));

        let summary = summarize_legacy_partition_duplication(storage_root).expect("summary");
        assert_eq!(
            summary,
            vec![
                LegacyHarnessDuplication {
                    harness: "opencode".to_string(),
                    partitions: 1,
                    bytes: expected_callgraph_bytes,
                },
                LegacyHarnessDuplication {
                    harness: "pi".to_string(),
                    partitions: 1,
                    bytes: inspect.bytes,
                },
            ]
        );
    }

    struct LegacyInventoryFixture {
        temp: TempDir,
    }

    fn write_legacy_inventory_fixture() -> LegacyInventoryFixture {
        let temp = tempfile::tempdir().expect("tempdir");

        let callgraph_dir = temp.path().join("opencode/callgraph");
        fs::create_dir_all(&callgraph_dir).expect("create callgraph dir");
        fs::write(
            callgraph_dir.join("0123456789abcdef.current"),
            b"0123456789abcdef.g1.1.sqlite\n",
        )
        .expect("write pointer");
        fs::write(
            callgraph_dir.join("0123456789abcdef.g1.1.sqlite"),
            b"callgraph-db",
        )
        .expect("write generation db");
        fs::write(
            callgraph_dir.join("0123456789abcdef.g1.1.sqlite-wal"),
            b"wal",
        )
        .expect("write generation wal");
        fs::write(
            callgraph_dir.join("0123456789abcdef.current.tmp.123"),
            b"ignored-temp",
        )
        .expect("write ignored temp");
        filetime::set_file_mtime(
            callgraph_dir.join("0123456789abcdef.current"),
            FileTime::from_unix_time(1_750_000_000, 0),
        )
        .expect("set pointer mtime");

        let inspect_dir = temp.path().join("pi/inspect");
        fs::create_dir_all(&inspect_dir).expect("create inspect dir");
        let sqlite_path = inspect_dir.join("fedcba9876543210.sqlite");
        let conn = Connection::open(&sqlite_path).expect("open inspect db");
        conn.execute(
            "CREATE TABLE tier2_meta (category TEXT NOT NULL, project_key TEXT NOT NULL, last_full_run INTEGER NOT NULL)",
            [],
        )
        .expect("create tier2_meta");
        conn.execute(
            "INSERT INTO tier2_meta (category, project_key, last_full_run) VALUES (?1, ?2, ?3)",
            params!["dead_code", "fedcba9876543210", 100_i64],
        )
        .expect("insert first tier2 row");
        conn.execute(
            "INSERT INTO tier2_meta (category, project_key, last_full_run) VALUES (?1, ?2, ?3)",
            params!["duplicates", "fedcba9876543210", 250_i64],
        )
        .expect("insert second tier2 row");
        drop(conn);
        fs::write(inspect_dir.join("misc.txt"), b"ignored").expect("write ignored file");

        LegacyInventoryFixture { temp }
    }

    fn partition_bytes_on_disk(
        domain_dir: &Path,
        expected_key: &str,
        key_fn: fn(&str) -> Option<String>,
    ) -> u64 {
        sorted_read_dir(domain_dir)
            .expect("read partition dir")
            .into_iter()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let key = key_fn(&name)?;
                if key != expected_key {
                    return None;
                }
                Some(file_size(&entry.path()).expect("partition file size"))
            })
            .sum()
    }
}
