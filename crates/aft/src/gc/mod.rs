//! Budgeted mark-and-sweep for immutable family blob stores.
//!
//! References from the retained manifests, live assembly pins, and active query
//! read markers are all marked before the budget selects eviction candidates.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::params;

use crate::blob_store::BlobPlane;
use crate::pins::{self, PinMetadata};
use crate::root_cache;

pub mod family;

/// Payloads newer than this stay available even when a store is over budget.
pub const BLOB_AGE_FLOOR_MS: u64 = 15 * 60 * 1_000;

#[derive(Debug)]
pub enum SweepError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Pin(pins::PinError),
    Metadata(serde_json::Error),
}

impl fmt::Display for SweepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "GC I/O error: {error}"),
            Self::Sqlite(error) => write!(f, "GC SQLite error: {error}"),
            Self::Pin(error) => write!(f, "GC pin error: {error}"),
            Self::Metadata(error) => write!(f, "GC pin metadata error: {error}"),
        }
    }
}

impl std::error::Error for SweepError {}

impl From<std::io::Error> for SweepError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<rusqlite::Error> for SweepError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<pins::PinError> for SweepError {
    fn from(error: pins::PinError) -> Self {
        Self::Pin(error)
    }
}
impl From<serde_json::Error> for SweepError {
    fn from(error: serde_json::Error) -> Self {
        Self::Metadata(error)
    }
}

/// References assembled from the current and previous manifests. `generation_keys`
/// additionally lets active query markers protect an otherwise unretained generation.
#[derive(Clone, Debug, Default)]
pub struct SweepReferences {
    pub retained_keys: BTreeSet<[u8; 32]>,
    pub generation_keys: BTreeMap<String, BTreeSet<[u8; 32]>>,
}

#[derive(Clone, Debug)]
pub struct SweepRequest<'a> {
    pub storage: &'a Path,
    pub family: &'a str,
    pub view_dir: &'a Path,
    pub byte_budget: u64,
    pub now_ms: u64,
    pub references: SweepReferences,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub examined_blobs: usize,
    pub deleted_blobs: usize,
    pub deleted_bytes: u64,
    pub retained_bytes: u64,
    pub protected_pin_keys: usize,
    pub reclaimed_pins: usize,
    pub reclaimed_read_markers: usize,
    pub reclaim_deferred: bool,
}

/// Collect unreferenced payloads older than the age floor even below the disk
/// budget. A budget is not a reason to keep unreachable content forever.
pub fn sweep(request: SweepRequest<'_>) -> Result<SweepReport, SweepError> {
    sweep_bounded(request, &|| false)
}

pub(crate) fn sweep_bounded(
    request: SweepRequest<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<SweepReport, SweepError> {
    let control = SweepControl {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled,
    };
    let _barrier = crate::storage_retention::barrier(request.storage)?;
    let mut report = SweepReport::default();
    let mut references = request.references.retained_keys.clone();
    // Families are shared across checkouts. A one-view mark can delete content
    // still named by a sibling's manifest. Keep every remaining manifest's keys;
    // generation GC removes superseded manifests before this storage-wide pass.
    let views = request.storage.join("views");
    if views.exists() {
        let entries = fs::read_dir(&views)?;
        let entries = entries.take(8193).collect::<Result<Vec<_>, _>>()?;
        if entries.len() > 8192 {
            return Err(std::io::Error::other("blob marking exceeded 8192 views").into());
        }
        for entry in entries {
            control.check()?;
            if !entry.file_type()?.is_dir() || entry.file_name() == "v2" {
                continue;
            }
            let view_dir = entry.path();
            let generation_keys = match crate::views::ViewStore::existing_dir(view_dir.clone()) {
                Some(store) => store
                    .blob_references_by_generation()
                    .map_err(|error| std::io::Error::other(error.to_string()))?,
                None => Default::default(),
            };
            for keys in generation_keys.values() {
                references.extend(keys.iter().copied());
            }
            let sibling = SweepRequest {
                view_dir: &view_dir,
                references: SweepReferences {
                    retained_keys: Default::default(),
                    generation_keys,
                },
                ..request.clone()
            };
            mark_live_assembly_pins(&sibling, &mut references, &mut report)?;
            mark_live_query_pins(&sibling, &mut references, &mut report);
        }
    }
    mark_live_assembly_pins(&request, &mut references, &mut report)?;
    mark_live_query_pins(&request, &mut references, &mut report);

    for plane in [BlobPlane::Semantic, BlobPlane::Callgraph] {
        control.check()?;
        let path = plane_path(request.storage, request.family, plane);
        if !path.exists() {
            continue;
        }
        sweep_plane(
            &path,
            request.now_ms,
            request.byte_budget,
            &references,
            &mut report,
            &control,
        )?;
    }
    drop(_barrier);
    // Compaction can be large. It must not hold pin/bind admission while it
    // runs; SQLite's own locks protect the rows and concurrent readers here.
    let reclaim_control = SweepControl {
        deadline: Instant::now() + Duration::from_secs(60),
        cancelled,
    };
    for plane in [BlobPlane::Semantic, BlobPlane::Callgraph] {
        let path = plane_path(request.storage, request.family, plane);
        let id = blake3::hash(path.to_string_lossy().as_bytes()).to_hex();
        let pending = request
            .storage
            .join("retention/vacuum")
            .join(format!("{id}.json"));
        if !path.exists() {
            continue;
        }
        if reclaim_plane(
            &path,
            &reclaim_control,
            report.deleted_blobs > 0 || pending.exists(),
        )
        .is_err()
        {
            report.reclaim_deferred = true;
            fs::create_dir_all(
                pending
                    .parent()
                    .ok_or_else(|| std::io::Error::other("missing vacuum parent"))?,
            )?;
            fs::write(&pending, b"true\n")?;
        } else {
            let _ = fs::remove_file(pending);
        }
    }
    Ok(report)
}

struct SweepControl<'a> {
    deadline: Instant,
    cancelled: &'a dyn Fn() -> bool,
}
impl SweepControl<'_> {
    fn check(&self) -> std::io::Result<()> {
        if Instant::now() >= self.deadline || (self.cancelled)() {
            Err(std::io::Error::other(
                "blob sweep cancelled or deadline reached",
            ))
        } else {
            Ok(())
        }
    }
}

unsafe extern "C" fn abort_sweep(data: *mut std::ffi::c_void) -> std::os::raw::c_int {
    // SQLite invokes this synchronously on the owning connection. The control
    // outlives that connection, and no callback is installed on another handle.
    let control = unsafe { &*(data as *const SweepControl<'_>) };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| control.check())) {
        Ok(Ok(())) => 0,
        _ => 1,
    }
}

fn mark_live_assembly_pins(
    request: &SweepRequest<'_>,
    references: &mut BTreeSet<[u8; 32]>,
    report: &mut SweepReport,
) -> Result<(), SweepError> {
    let pins_dir = request.view_dir.join("pins");
    let entries = match fs::read_dir(&pins_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };

    for (index, entry) in entries.take(4097).enumerate() {
        if index == 4096 {
            return Err(std::io::Error::other("blob marking exceeded 4096 pins").into());
        }
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let metadata: PinMetadata = match serde_json::from_slice(&fs::read(&path)?) {
            Ok(metadata) => metadata,
            Err(error) => return Err(error.into()),
        };
        if metadata.family != request.family || metadata.view.is_empty() {
            continue;
        }
        let (metadata_path, keys_path) = pins::pin_paths(request.view_dir, &metadata.generation);
        if metadata_path != path {
            continue;
        }
        if !pins::owner_is_live(&metadata.owner) {
            let _ = fs::remove_file(&metadata_path);
            let _ = fs::remove_file(&keys_path);
            report.reclaimed_pins += 1;
            continue;
        }
        let keys = pins::read_keys(&keys_path)?;
        report.protected_pin_keys += keys.len();
        references.extend(keys);
    }
    Ok(())
}

fn mark_live_query_pins(
    request: &SweepRequest<'_>,
    references: &mut BTreeSet<[u8; 32]>,
    report: &mut SweepReport,
) {
    let readers = request.view_dir.join("readers");
    let Ok(entries) = fs::read_dir(readers) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(generation) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let marker_sweep = root_cache::sweep_read_markers(request.view_dir, &generation);
        report.reclaimed_read_markers += marker_sweep.removed_stale;
        if marker_sweep.protected {
            if let Some(keys) = request.references.generation_keys.get(&generation) {
                references.extend(keys.iter().copied());
            }
        }
    }
}

fn sweep_plane(
    path: &Path,
    now_ms: u64,
    _byte_budget: u64,
    references: &BTreeSet<[u8; 32]>,
    report: &mut SweepReport,
    control: &SweepControl<'_>,
) -> Result<(), SweepError> {
    let connection = crate::db::file_identity::IdentityConnection::open(path, "gc::sweep_plane")?;
    // Install a progress deadline through SQLite's own handle, not a second
    // descriptor or a helper that opens/closes the live file set. SQLite drops
    // the callback when this scoped connection closes, before control expires.
    unsafe {
        rusqlite::ffi::sqlite3_progress_handler(
            connection.handle(),
            1000,
            Some(abort_sweep),
            (control as *const SweepControl<'_>).cast_mut().cast(),
        );
    }
    let mut candidates = Vec::new();
    let mut total_bytes = connection.query_row(
        "SELECT COALESCE(SUM(length(payload)), 0) FROM blob_payloads",
        [],
        |row| row.get::<_, u64>(0),
    )?;
    static CURSORS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<PathBuf, Vec<u8>>>> =
        std::sync::OnceLock::new();
    let cursors = CURSORS.get_or_init(Default::default);
    let after = cursors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        .cloned()
        .unwrap_or_default();
    {
        let mut statement = connection.prepare(
            "SELECT full_key, length(payload), created_at_ms
             FROM blob_payloads WHERE full_key > ?1 ORDER BY full_key ASC LIMIT 4096",
        )?;
        let rows = statement.query_map(params![after], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
            ))
        })?;
        for row in rows {
            let (key, bytes, created_at_ms) = row?;
            let Ok(key) = <Vec<u8> as TryInto<[u8; 32]>>::try_into(key) else {
                continue;
            };
            candidates.push((key, bytes, created_at_ms));
        }
    }
    report.examined_blobs += candidates.len();
    let mut cursors = cursors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if candidates.len() == 4096 {
        if let Some((key, _, _)) = candidates.last() {
            cursors.insert(path.to_path_buf(), key.to_vec());
        }
    } else {
        cursors.remove(path);
    }
    drop(cursors);
    for (key, bytes, created_at_ms) in candidates {
        control.check()?;
        // This reference check is the safety boundary: retained manifests and
        // live pins must win over budget pressure.
        if references.contains(&key) || now_ms.saturating_sub(created_at_ms) < BLOB_AGE_FLOOR_MS {
            continue;
        }
        let deleted = connection.execute(
            "DELETE FROM blob_payloads WHERE full_key = ?1",
            params![key.as_slice()],
        )?;
        if deleted == 1 {
            total_bytes = total_bytes.saturating_sub(bytes);
            report.deleted_blobs += 1;
            report.deleted_bytes = report.deleted_bytes.saturating_add(bytes);
        }
    }
    report.retained_bytes = report.retained_bytes.saturating_add(total_bytes);
    Ok(())
}

fn reclaim_plane(path: &Path, control: &SweepControl<'_>, force: bool) -> Result<(), SweepError> {
    control.check()?;
    let connection = crate::db::file_identity::IdentityConnection::open(path, "gc::reclaim_plane")?;
    unsafe {
        rusqlite::ffi::sqlite3_progress_handler(
            connection.handle(),
            1000,
            Some(abort_sweep),
            (control as *const SweepControl<'_>).cast_mut().cast(),
        );
    }
    connection.busy_timeout(Duration::from_millis(100))?;
    let free_bytes: u64 = connection.query_row("SELECT (SELECT freelist_count FROM pragma_freelist_count) * (SELECT page_size FROM pragma_page_size)", [], |row| row.get(0))?;
    if free_bytes == 0 || (!force && free_bytes < 128 * 1024 * 1024) {
        return Ok(());
    }
    let auto_vacuum: u32 = connection.query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?;
    connection.execute_batch(if auto_vacuum == 2 {
        "PRAGMA incremental_vacuum"
    } else {
        "VACUUM"
    })?;
    Ok(())
}

fn plane_path(storage: &Path, family: &str, plane: BlobPlane) -> PathBuf {
    storage
        .join("blobs")
        .join(family)
        .join(format!("{}.sqlite", plane.as_str()))
}

#[cfg(test)]
mod storage_retention_tests {
    use super::*;

    #[test]
    fn storage_retention_collects_unreferenced_blobs_below_budget() {
        let temp = tempfile::tempdir().unwrap();
        let path = plane_path(temp.path(), "family", BlobPlane::Callgraph);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE blob_payloads (full_key BLOB PRIMARY KEY, payload BLOB, created_at_ms INTEGER);").unwrap();
        for key in [[1_u8; 32], [2_u8; 32]] {
            connection
                .execute(
                    "INSERT INTO blob_payloads VALUES (?1, X'01020304', 0)",
                    params![key.as_slice()],
                )
                .unwrap();
        }
        drop(connection);
        let report = sweep(SweepRequest {
            storage: temp.path(),
            family: "family",
            view_dir: &temp.path().join("view"),
            byte_budget: 1024,
            now_ms: BLOB_AGE_FLOOR_MS + 1,
            references: SweepReferences {
                retained_keys: BTreeSet::from([[1_u8; 32]]),
                ..Default::default()
            },
        })
        .unwrap();
        assert_eq!(report.deleted_blobs, 1);
        assert_eq!(report.deleted_bytes, 4);
        let connection = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT hex(full_key) FROM blob_payloads", [], |row| row
                    .get::<_, String>(
                    0
                ))
                .unwrap(),
            "01".repeat(32)
        );
    }
    #[test]
    fn storage_retention_blob_mark_includes_sibling_manifests() {
        let temp = tempfile::tempdir().unwrap();
        let path = plane_path(temp.path(), "family", BlobPlane::Callgraph);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE blob_payloads (full_key BLOB PRIMARY KEY, payload BLOB, created_at_ms INTEGER);").unwrap();
        connection
            .execute(
                "INSERT INTO blob_payloads VALUES (?1, X'01020304', 0)",
                params![[3_u8; 32].as_slice()],
            )
            .unwrap();
        drop(connection);
        let sibling = crate::views::ViewStore::open(temp.path(), "0123456789abcdef").unwrap();
        let manifest = crate::views::Manifest::new([(
            crate::views::RelPath::new(b"file.rs".to_vec()).unwrap(),
            crate::views::ManifestEntry::Regular {
                mode: 0o100644,
                resolution_input: true,
                planes: crate::views::RegularPlanes {
                    semantic: None,
                    callgraph: Some("03".repeat(32)),
                },
            },
        )])
        .unwrap();
        fs::write(
            sibling.manifest_path("current").unwrap(),
            manifest.to_json_bytes().unwrap(),
        )
        .unwrap();
        let report = sweep(SweepRequest {
            storage: temp.path(),
            family: "family",
            view_dir: &temp.path().join("requesting-view"),
            byte_budget: 0,
            now_ms: BLOB_AGE_FLOOR_MS + 1,
            references: Default::default(),
        })
        .unwrap();
        assert_eq!(report.deleted_blobs, 0);
        let connection = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM blob_payloads", [], |row| row
                    .get::<_, i32>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn storage_retention_blob_compaction_returns_free_pages() {
        let temp = tempfile::tempdir().unwrap();
        let path = plane_path(temp.path(), "family", BlobPlane::Callgraph);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE blob_payloads (full_key BLOB PRIMARY KEY, payload BLOB, created_at_ms INTEGER);").unwrap();
        connection
            .execute(
                "INSERT INTO blob_payloads VALUES (?1, zeroblob(1048576), 0)",
                params![[4_u8; 32].as_slice()],
            )
            .unwrap();
        drop(connection);
        let before = fs::metadata(&path).unwrap().len();
        let report = sweep(SweepRequest {
            storage: temp.path(),
            family: "family",
            view_dir: &temp.path().join("requesting-view"),
            byte_budget: 0,
            now_ms: BLOB_AGE_FLOOR_MS + 1,
            references: Default::default(),
        })
        .unwrap();
        assert_eq!(report.deleted_blobs, 1);
        assert!(!report.reclaim_deferred);
        assert!(fs::metadata(path).unwrap().len() < before);
    }
}
