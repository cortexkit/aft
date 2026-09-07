//! Budgeted mark-and-sweep for immutable family blob stores.
//!
//! References from the retained manifests, live assembly pins, and active query
//! read markers are all marked before the budget selects eviction candidates.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use crate::blob_store::BlobPlane;
use crate::pins::{self, PinMetadata, PIN_TTL_MS};
use crate::root_cache;

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
    pub deleted_blobs: usize,
    pub deleted_bytes: u64,
    pub retained_bytes: u64,
    pub protected_pin_keys: usize,
    pub reclaimed_pins: usize,
    pub reclaimed_read_markers: usize,
}

/// Performs one mark-and-sweep pass. It deletes only unreferenced payloads older
/// than the age floor, stopping as soon as the family fits within its byte budget.
pub fn sweep(request: SweepRequest<'_>) -> Result<SweepReport, SweepError> {
    let mut report = SweepReport::default();
    let mut references = request.references.retained_keys.clone();
    mark_live_assembly_pins(&request, &mut references, &mut report)?;
    mark_live_query_pins(&request, &mut references, &mut report);

    for plane in [BlobPlane::Semantic, BlobPlane::Callgraph] {
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
        )?;
    }
    Ok(report)
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

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let metadata: PinMetadata = match serde_json::from_slice(&fs::read(&path)?) {
            Ok(metadata) => metadata,
            Err(_) => continue, // A partial metadata write is never a reason to delete blobs.
        };
        if metadata.family != request.family || metadata.view.is_empty() {
            continue;
        }
        let (metadata_path, keys_path) = pins::pin_paths(request.view_dir, &metadata.generation);
        if metadata_path != path {
            continue;
        }
        let expired = request.now_ms.saturating_sub(metadata.renewed_at) > PIN_TTL_MS;
        if expired || !pins::owner_is_live(&metadata.owner) {
            let _ = fs::remove_file(&metadata_path);
            let _ = fs::remove_file(&keys_path);
            crate::fs_lock::sync_parent(&metadata_path);
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
    byte_budget: u64,
    references: &BTreeSet<[u8; 32]>,
    report: &mut SweepReport,
) -> Result<(), SweepError> {
    let connection = Connection::open(path)?;
    let mut candidates = Vec::new();
    let mut total_bytes = 0_u64;
    {
        let mut statement = connection.prepare(
            "SELECT full_key, length(payload), created_at_ms
             FROM blob_payloads ORDER BY created_at_ms ASC, full_key ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
            ))
        })?;
        for row in rows {
            let (key, bytes, created_at_ms) = row?;
            total_bytes = total_bytes.saturating_add(bytes);
            let Ok(key) = <Vec<u8> as TryInto<[u8; 32]>>::try_into(key) else {
                continue;
            };
            candidates.push((key, bytes, created_at_ms));
        }
    }

    for (key, bytes, created_at_ms) in candidates {
        if total_bytes <= byte_budget {
            break;
        }
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

fn plane_path(storage: &Path, family: &str, plane: BlobPlane) -> PathBuf {
    storage
        .join("blobs")
        .join(family)
        .join(format!("{}.sqlite", plane.as_str()))
}
