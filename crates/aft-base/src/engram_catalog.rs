//! AFT's backup catalog for ENGRAM, the fleet backup service.
//!
//! ENGRAM reads `<storage root>/engram-catalog.json` to learn which parts of
//! AFT's storage it may capture. Every module writes its own catalog at
//! startup so a freshly provisioned machine is covered without a manual step.
//!
//! Two ENGRAM rules shape what is written here:
//! - a declared path that does not exist halts the whole fleet capture, so
//!   every entry is derived from what is on disk at the moment of writing
//!   (excluded entries included): a folder or file that is missing, is a
//!   symlink, or has the wrong kind is simply not declared;
//! - entry ids are the key of ENGRAM's capture history, so an id, once
//!   written, must keep naming the same folder. Each write therefore reads the
//!   catalog already on disk and keeps every surviving path's id; only paths
//!   new since then get fresh ids (see [`assign_entry_ids`]).
//!
//! ENGRAM finds a module's catalog only at `<fleet data home>/<module id>/`
//! and rejects one whose `module_id` differs from that folder's name, so the
//! catalog is written only when the storage root's folder is named `aft`.
//!
//! Entries and their ids (`<label>` is a harness label, see [`harness_label`]):
//!
//! | Path | Entry id | Class / mechanism |
//! |---|---|---|
//! | `aft.db` | `aft/db` | device-local page-db, `read-transaction-live` |
//! | `trusted-filter-projects.json` | `aft/trusted-filter-projects` | device-local filetree (one file) |
//! | `<harness>/backups` | `aft/backups-<label>` | [`UNDO_BACKUPS_CLASS`] filetree |
//! | `<harness>/backups.parked-<suffix>` | `aft/backups-<label>-parked`, later ones `aft/backups-<label>-parked-<suffix>` | [`UNDO_BACKUPS_CLASS`] filetree |
//! | `<harness>/checkpoints` | `aft/checkpoints-<label>` | excluded filetree |
//! | `<harness>/bash-tasks` | `aft/bash-tasks-<label>` | excluded filetree |
//! | `<name>` from [`EXCLUDED_ROOT_DIRS`] | `aft/excluded-<name>` | excluded filetree |
//!
//! `aft.db` is declared as one entry: ENGRAM's page-db capture reads its
//! `-wal`/`-shm` sidecars through SQLite, so they are never declared alone.
//! Excluded entries carry a `reason`, which ENGRAM requires. Anything else in
//! the storage root (lock folders, small UI state files, the catalog itself)
//! is intentionally not declared.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// File name ENGRAM looks for in each module's data directory.
pub const CATALOG_FILENAME: &str = "engram-catalog.json";

/// ENGRAM checks that `module_id` matches the name of the directory holding
/// the catalog; AFT's storage root is `<fleet data home>/aft`.
const MODULE_ID: &str = "aft";

/// Descriptor schema version understood by ENGRAM.
const SCHEMA_VERSION: u32 = 1;

/// Harness folder names with a fixed spelling.
const FIXED_HARNESS_DIRS: &[&str] = &["opencode", "pi", "runner"];

/// Prefixes of harness folders whose names embed a client name or fingerprint.
const MCP_HARNESS_PREFIX: &str = "mcp--";
const FED_HARNESS_PREFIX: &str = "fed--";

/// Storage-root folder where checkpoints are kept before the daemon knows
/// its harness. It holds checkpoints (and, in principle, task output) but
/// never undo backups, so it gets the `checkpoints`/`bash-tasks` entries of a
/// harness folder but no backups entries.
const UNBOUND_HARNESS_DIR: &str = "unbound";

const BACKUPS_DIR: &str = "backups";
const PARKED_BACKUPS_PREFIX: &str = "backups.parked-";
const CHECKPOINTS_DIR: &str = "checkpoints";
const BASH_TASKS_DIR: &str = "bash-tasks";

/// AFT's shared SQLite database file (declared with ENGRAM's page-db
/// mechanism, which copies changed database pages).
const DB_FILE: &str = "aft.db";
/// The user's list of projects trusted for output filters.
const TRUSTED_FILTER_PROJECTS_FILE: &str = "trusted-filter-projects.json";

/// Class of every undo-backup entry (`<harness>/backups` and parked copies).
/// Undo history is not backed up: the entries are declared excluded under
/// the ids they always had, so ENGRAM's history shows the change on the same
/// entries. Kept in one place so the class is a one-line change.
const UNDO_BACKUPS_CLASS: &str = "excluded";
/// Reason written for undo-backup entries while [`UNDO_BACKUPS_CLASS`] is
/// `"excluded"` (ENGRAM refuses an excluded entry without a reason).
const UNDO_BACKUPS_EXCLUDED_REASON: &str = "earlier copies of edited files; may include secrets";

const CHECKPOINTS_REASON: &str = "undo snapshots of project files; may hold secrets";
const BASH_TASKS_REASON: &str = "background command output; may hold secrets";

/// Storage-root folders declared as excluded, with the reason ENGRAM shows.
/// Each is declared only while it exists as a real folder. `checkpoints` and
/// `bash-tasks` here are the legacy root-level copies of the per-harness
/// folders; `bash-tasks-quarantine` holds task output set aside during repair.
const EXCLUDED_ROOT_DIRS: &[(&str, &str)] = &[
    (BASH_TASKS_DIR, BASH_TASKS_REASON),
    ("bash-tasks-quarantine", BASH_TASKS_REASON),
    ("blobs", "rebuildable cache"),
    ("callgraph", "rebuildable index"),
    (CHECKPOINTS_DIR, CHECKPOINTS_REASON),
    ("diagnostics", "diagnostic files with paths and query text"),
    ("dsym", "rebuildable debug symbols"),
    ("git-hooks", "generated per machine"),
    ("index", "rebuildable index"),
    ("inspect", "rebuildable index"),
    ("logs", "daemon logs with paths and query text"),
    ("onnxruntime", "re-downloadable runtime"),
    ("rust-completed-checks", "rebuildable cache"),
    ("semantic", "rebuildable index"),
    ("shims", "generated per machine"),
    ("symbols", "rebuildable index"),
    ("views", "rebuildable index"),
];

/// Length of the hex hash an MCP harness folder name ends with
/// (`mcp--<readable client>--<32 hex>`).
const MCP_HASH_HEX_LEN: usize = 32;

#[derive(Debug, Serialize)]
struct Descriptor {
    schema_version: u32,
    module_id: &'static str,
    entries: Vec<Entry>,
}

/// One catalog entry. The optional fields are left out of the JSON when
/// unset, so an entry without them serializes exactly as it did before they
/// existed.
#[derive(Debug, Serialize)]
struct Entry {
    entry_id: String,
    class: &'static str,
    mechanism: &'static str,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    writer_interaction: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

/// What [`write_catalog`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The file was created or replaced.
    Written,
    /// The file already held exactly these bytes, so it was left untouched
    /// (its modification time is preserved).
    Unchanged,
    /// The storage root's folder is not named `aft` (for example an
    /// `AFT_STORAGE_DIR` pointing elsewhere), so ENGRAM would never look
    /// there and would reject the `module_id` if it did. Nothing was written.
    SkippedRootName,
}

/// One entry found on disk, before its entry id is settled.
struct Candidate {
    /// `/`-separated path relative to the storage root.
    path: String,
    /// The id this entry gets unless another entry already took it.
    preferred_id: String,
    /// The id used when `preferred_id` is taken by an earlier entry.
    fallback_id: String,
    class: &'static str,
    mechanism: &'static str,
    writer_interaction: Option<&'static str>,
    reason: Option<&'static str>,
}

impl Candidate {
    /// An entry with one fixed id, for paths that occur once per storage root
    /// and so can never compete for an id.
    fn fixed(id: &str, path: &str, class: &'static str, mechanism: &'static str) -> Self {
        Self {
            path: path.to_string(),
            preferred_id: id.to_string(),
            fallback_id: id.to_string(),
            class,
            mechanism,
            writer_interaction: None,
            reason: None,
        }
    }

    /// An excluded filetree entry with the reason ENGRAM requires.
    fn excluded(
        preferred_id: String,
        fallback_id: String,
        path: String,
        reason: &'static str,
    ) -> Self {
        Self {
            path,
            preferred_id,
            fallback_id,
            class: "excluded",
            mechanism: "filetree",
            writer_interaction: None,
            reason: Some(reason),
        }
    }
}

/// Write the catalog at daemon startup, and again whenever the storage root
/// gains something it should declare (see `App::open_db`, which calls this
/// after creating `aft.db`). Never fails: a catalog that cannot be written
/// must not stop AFT from serving, so errors become a warning in the log.
pub fn refresh_catalog(storage_root: &Path) {
    match write_catalog(storage_root) {
        Ok(WriteOutcome::Written) => crate::slog_info!(
            "engram catalog written: {}",
            storage_root.join(CATALOG_FILENAME).display()
        ),
        Ok(WriteOutcome::Unchanged) => {}
        Ok(WriteOutcome::SkippedRootName) => crate::slog_info!(
            "engram catalog not written: storage root {} is not a folder named \"{MODULE_ID}\", \
             so ENGRAM would not find it",
            storage_root.display()
        ),
        Err(error) => crate::slog_warn!(
            "engram catalog not written under {}: {error}",
            storage_root.display()
        ),
    }
}

/// Render the catalog for `storage_root` and write it atomically, skipping
/// the write when the file already holds the same bytes.
pub fn write_catalog(storage_root: &Path) -> io::Result<WriteOutcome> {
    if !storage_root.is_dir() {
        // Nothing to describe yet, and creating the storage root is not this
        // module's job.
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("storage root {} does not exist", storage_root.display()),
        ));
    }
    if storage_root.file_name() != Some(std::ffi::OsStr::new(MODULE_ID)) {
        return Ok(WriteOutcome::SkippedRootName);
    }
    let bytes = render_catalog(storage_root)?;
    write_if_changed(&storage_root.join(CATALOG_FILENAME), &bytes)
}

/// Render the catalog bytes for the folders that exist under `storage_root`
/// right now, keeping the ids of the catalog already there. The same tree
/// (catalog file included) always renders the same bytes.
pub fn render_catalog(storage_root: &Path) -> io::Result<Vec<u8>> {
    let previous = previous_ids(&storage_root.join(CATALOG_FILENAME));
    let descriptor = Descriptor {
        schema_version: SCHEMA_VERSION,
        module_id: MODULE_ID,
        entries: assign_entry_ids(collect_candidates(storage_root)?, &previous),
    };
    let mut bytes = serde_json::to_vec_pretty(&descriptor).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Every entry that exists under `storage_root` right now, in sorted path
/// order. Within one id family (say all `aft/backups-*` entries) the order is
/// the sorted folder order, which is what keeps first-come ids stable.
fn collect_candidates(storage_root: &Path) -> io::Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for name in sorted_dir_names(storage_root)? {
        let path = storage_root.join(&name);
        if name == DB_FILE {
            if is_real_file(&path) {
                let mut db = Candidate::fixed("aft/db", DB_FILE, "device-local", "page-db");
                // ENGRAM copies changed pages under one held read transaction;
                // AFT's writers keep running meanwhile (WAL).
                db.writer_interaction = Some("read-transaction-live");
                candidates.push(db);
            }
            continue;
        }
        if name == TRUSTED_FILTER_PROJECTS_FILE {
            if is_real_file(&path) {
                candidates.push(Candidate::fixed(
                    "aft/trusted-filter-projects",
                    TRUSTED_FILTER_PROJECTS_FILE,
                    "device-local",
                    "filetree",
                ));
            }
            continue;
        }
        if let Some((_, reason)) = EXCLUDED_ROOT_DIRS.iter().find(|(dir, _)| *dir == name) {
            if is_real_dir(&path) {
                let id = format!("aft/excluded-{name}");
                candidates.push(Candidate::excluded(id.clone(), id, name.clone(), reason));
            }
            continue;
        }
        let (label, holds_backups) = if name == UNBOUND_HARNESS_DIR {
            (UNBOUND_HARNESS_DIR.to_string(), false)
        } else {
            match harness_label(&name) {
                Some(label) => (label, true),
                None => continue,
            }
        };
        if is_real_dir(&path) {
            collect_harness_candidates(&path, &name, &label, holds_backups, &mut candidates)?;
        }
    }
    Ok(candidates)
}

/// The entries inside one harness folder. `harness_dir` is the folder name,
/// `label` its id part; undo backups are only looked for when
/// `holds_backups` is set.
fn collect_harness_candidates(
    harness_path: &Path,
    harness_dir: &str,
    label: &str,
    holds_backups: bool,
    candidates: &mut Vec<Candidate>,
) -> io::Result<()> {
    // Only reachable when two MCP folders share a readable client name; the
    // folder name itself carries the distinguishing hash.
    let long_label = harness_dir.replace("--", "-");
    let backups_reason = (UNDO_BACKUPS_CLASS == "excluded").then_some(UNDO_BACKUPS_EXCLUDED_REASON);
    // Sorted names put `backups` before its parked copies and the oldest dated
    // park first, so the first parked folder keeps the undated id and later
    // ones add their suffix.
    for child in sorted_dir_names(harness_path)? {
        if !is_real_dir(&harness_path.join(&child)) {
            continue;
        }
        let path = format!("{harness_dir}/{child}");
        if child == BACKUPS_DIR && holds_backups {
            candidates.push(Candidate {
                path,
                preferred_id: format!("aft/backups-{label}"),
                fallback_id: format!("aft/backups-{long_label}"),
                class: UNDO_BACKUPS_CLASS,
                mechanism: "filetree",
                writer_interaction: None,
                reason: backups_reason,
            });
        } else if let Some(suffix) = child
            .strip_prefix(PARKED_BACKUPS_PREFIX)
            .filter(|suffix| holds_backups && is_safe_name_part(suffix))
        {
            candidates.push(Candidate {
                path,
                preferred_id: format!("aft/backups-{label}-parked"),
                fallback_id: format!("aft/backups-{label}-parked-{suffix}"),
                class: UNDO_BACKUPS_CLASS,
                mechanism: "filetree",
                writer_interaction: None,
                reason: backups_reason,
            });
        } else if child == CHECKPOINTS_DIR {
            candidates.push(Candidate::excluded(
                format!("aft/checkpoints-{label}"),
                format!("aft/checkpoints-{long_label}"),
                path,
                CHECKPOINTS_REASON,
            ));
        } else if child == BASH_TASKS_DIR {
            candidates.push(Candidate::excluded(
                format!("aft/bash-tasks-{label}"),
                format!("aft/bash-tasks-{long_label}"),
                path,
                BASH_TASKS_REASON,
            ));
        }
    }
    Ok(())
}

/// The fields of the catalog already on disk that are needed to keep each
/// path's entry id across writes.
#[derive(Deserialize)]
struct PreviousCatalog {
    entries: Vec<PreviousEntry>,
}

#[derive(Deserialize)]
struct PreviousEntry {
    entry_id: String,
    path: String,
}

/// Path -> entry id from the catalog at `catalog_path`. A missing file gives
/// an empty map. A file that cannot be read, does not parse, or names one
/// path or id twice also gives an empty map, with a warning: ids then start
/// fresh, which is what a first write does anyway.
fn previous_ids(catalog_path: &Path) -> BTreeMap<String, String> {
    let bytes = match fs::read(catalog_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(error) => {
            crate::slog_warn!(
                "engram catalog {} unreadable, entry ids start fresh: {error}",
                catalog_path.display()
            );
            return BTreeMap::new();
        }
    };
    let catalog = match serde_json::from_slice::<PreviousCatalog>(&bytes) {
        Ok(catalog) => catalog,
        Err(error) => {
            crate::slog_warn!(
                "engram catalog {} malformed, entry ids start fresh: {error}",
                catalog_path.display()
            );
            return BTreeMap::new();
        }
    };
    let mut ids = BTreeMap::new();
    let mut seen_ids = BTreeSet::new();
    for entry in catalog.entries {
        if !seen_ids.insert(entry.entry_id.clone())
            || ids.insert(entry.path, entry.entry_id).is_some()
        {
            crate::slog_warn!(
                "engram catalog {} repeats a path or entry id, entry ids start fresh",
                catalog_path.display()
            );
            return BTreeMap::new();
        }
    }
    ids
}

/// Give each candidate a unique id. A path the previous catalog already
/// declared keeps its id, so removing one folder never renames another
/// (two MCP folders sharing a readable client name, or dated parked
/// copies, would otherwise shift ids). Every other candidate takes, in
/// order, its preferred id when free, else its fallback id, else the
/// fallback with a counter; kept ids are reserved first, so a fresh id
/// never repeats one.
fn assign_entry_ids(candidates: Vec<Candidate>, previous: &BTreeMap<String, String>) -> Vec<Entry> {
    let mut used = BTreeSet::new();
    let kept = candidates
        .iter()
        .map(|candidate| {
            previous
                .get(&candidate.path)
                .filter(|id| used.insert((*id).clone()))
                .cloned()
        })
        .collect::<Vec<_>>();
    let mut entries = Vec::with_capacity(candidates.len());
    for (candidate, kept) in candidates.into_iter().zip(kept) {
        let entry_id = if let Some(id) = kept {
            id
        } else if !used.contains(&candidate.preferred_id) {
            candidate.preferred_id
        } else if !used.contains(&candidate.fallback_id) {
            candidate.fallback_id
        } else {
            (2..)
                .map(|n| format!("{}-{n}", candidate.fallback_id))
                .find(|id| !used.contains(id))
                .expect("an unbounded counter always finds a free id")
        };
        used.insert(entry_id.clone());
        entries.push(Entry {
            entry_id,
            class: candidate.class,
            mechanism: candidate.mechanism,
            path: candidate.path,
            writer_interaction: candidate.writer_interaction,
            reason: candidate.reason,
        });
    }
    entries
}

/// The harness part of an entry id for a storage-root folder, or `None` when
/// the folder is not a harness folder.
///
/// MCP folders are `mcp--<readable client>--<hash>`; the id uses the client
/// name only (`mcp-claude-code`), matching the ids ENGRAM already has.
/// Federated folders have no readable name, so the whole folder name is used
/// with its `--` separators shortened.
pub(crate) fn harness_label(dir_name: &str) -> Option<String> {
    if FIXED_HARNESS_DIRS.contains(&dir_name) {
        return Some(dir_name.to_string());
    }
    if !is_safe_name_part(dir_name) {
        return None;
    }
    if let Some(rest) = dir_name.strip_prefix(MCP_HARNESS_PREFIX) {
        let readable = match rest.rsplit_once("--") {
            Some((readable, hash))
                if !readable.is_empty()
                    && hash.len() == MCP_HASH_HEX_LEN
                    && hash.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                readable
            }
            _ => rest,
        };
        if readable.is_empty() {
            return None;
        }
        return Some(format!("mcp-{}", readable.replace("--", "-")));
    }
    if dir_name.starts_with(FED_HARNESS_PREFIX) && dir_name.len() > FED_HARNESS_PREFIX.len() {
        return Some(dir_name.replace("--", "-"));
    }
    None
}

/// Folder names copied into ids and paths are limited to the characters AFT
/// itself uses for them, so a stray folder can never produce a path ENGRAM
/// rejects (separators, `..`, drive prefixes, non-ASCII).
fn is_safe_name_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A directory that is not a symlink: ENGRAM resolves declared paths itself,
/// and a link would make the declared folder depend on its target.
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// A regular file that is not a symlink, for the same reason as [`is_real_dir`].
fn is_real_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// UTF-8 child names of `dir`, sorted. Non-UTF-8 names can never be harness
/// or backup folders and are skipped.
fn sorted_dir_names(dir: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        if let Ok(name) = entry?.file_name().into_string() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Write `bytes` to `target` unless the file already holds them.
///
/// The bytes go to a temporary file in the same folder, which is then renamed
/// over the target, so a reader never sees a half-written catalog; the
/// temporary file is removed when any step fails. A dev or test build refuses
/// to write into the account's real storage, like every other versioned
/// record AFT writes there.
fn write_if_changed(target: &Path, bytes: &[u8]) -> io::Result<WriteOutcome> {
    if fs::read(target).is_ok_and(|existing| existing == bytes) {
        return Ok(WriteOutcome::Unchanged);
    }
    crate::production_storage::refuse_write(target)?;
    crate::durability::atomic_write(
        target,
        bytes,
        crate::durability::AtomicWriteDurability::File,
    )?;
    Ok(WriteOutcome::Written)
}
