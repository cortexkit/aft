use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::fs_lock;
use crate::harness::Harness;

mod log;

use self::log::{iso_timestamp_now, now_millis, JsonLogger};

const SOURCE_MARKER: &str = ".migrated_to_cortexkit";
const TARGET_MARKER: &str = ".migrated_from_legacy";
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct Args {
    pub from: Option<PathBuf>,
    pub to: PathBuf,
    pub harness: Harness,
    pub log: Option<PathBuf>,
    pub status: bool,
}

#[derive(Clone, Debug)]
struct MigrationArgs {
    from: PathBuf,
    to: PathBuf,
    harness: Harness,
    log: PathBuf,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub lock_timeout: Duration,
    pub disk_free_override: Option<u64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            lock_timeout: LOCK_TIMEOUT,
            disk_free_override: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitStatus {
    Success = 0,
    SourceUnreadable = 1,
    InsufficientDisk = 2,
    LockContention = 3,
    PartialState = 4,
    MigrationFailed = 5,
}

impl ExitStatus {
    pub fn code(self) -> u8 {
        self as u8
    }

    fn exit_code(self) -> ExitCode {
        ExitCode::from(self.code())
    }
}

pub fn run(args: Args) -> ExitCode {
    run_with_options(args, Options::default()).exit_code()
}

pub fn run_with_options(args: Args, options: Options) -> ExitStatus {
    if args.status {
        write_status(&args.to, args.harness, args.from.as_deref());
        return ExitStatus::Success;
    }

    let Some(from) = args.from else {
        return ExitStatus::MigrationFailed;
    };
    let Some(log_path) = args.log else {
        return ExitStatus::MigrationFailed;
    };
    let args = MigrationArgs {
        from,
        to: args.to,
        harness: args.harness,
        log: log_path,
    };

    let target_root_error = crate::private_storage::open_root(&args.to).err();
    let log_harness = args.harness.clone();
    let mut log = JsonLogger::open(&args.log, log_harness);
    let started = SystemTime::now();
    log.write(serde_json::json!({
        "step": "start",
        "level": "info",
        "from": args.from,
        "to": args.to,
        "harness": args.harness.storage_segment(),
    }));

    if let Some(error) = target_root_error {
        log.write(serde_json::json!({
            "level": "error",
            "step": "create_target_root",
            "path": args.to,
            "status": "error",
            "error": error.to_string(),
        }));
        return ExitStatus::MigrationFailed;
    }

    let lock_dir = args.to.join(".aft");
    if let Err(error) = crate::private_storage::open_dir(&args.to, &lock_dir) {
        log.write(serde_json::json!({
            "level": "error",
            "step": "create_lock_dir",
            "path": lock_dir,
            "status": "error",
            "error": error.to_string(),
        }));
        return ExitStatus::MigrationFailed;
    }

    let target_harness = args.to.join(args.harness.storage_segment());
    let target_marker = target_marker_path(&args);
    let source_marker = source_marker_path(&args);

    if source_marker.exists() && target_marker.exists() {
        log.write(serde_json::json!({
            "level": "info",
            "step": "marker_check",
            "status": "already_migrated",
        }));
        return ExitStatus::Success;
    }

    let source_bytes = match source_size(&args.from) {
        Ok(bytes) => bytes,
        Err(error) => {
            log.write(serde_json::json!({
                "level": "error",
                "step": "preflight",
                "path": args.from,
                "status": "source_unreadable",
                "error": error.to_string(),
            }));
            return ExitStatus::SourceUnreadable;
        }
    };

    // Each destination volume must hold twice the bytes headed for it. The
    // cache folders (index, semantic, symbols) may be on a different volume
    // from the target root, so the root's free space alone says nothing about
    // them.
    let volumes = match destination_volumes(&args, source_bytes, &options) {
        Ok(volumes) => volumes,
        Err((path, error)) => {
            log.write(serde_json::json!({
                "level": "error",
                "step": "preflight",
                "path": path,
                "status": "disk_check_failed",
                "bytes_source": source_bytes,
                "error": error.to_string(),
            }));
            return ExitStatus::MigrationFailed;
        }
    };
    let mut has_space = true;
    for volume in &volumes {
        let required = volume.source_bytes.saturating_mul(2);
        let ok = volume.free_bytes >= required;
        has_space &= ok;
        log.write(serde_json::json!({
            "level": if ok { "info" } else { "error" },
            "step": "preflight",
            "path": volume.path,
            "bytes_source": volume.source_bytes,
            "bytes_free": volume.free_bytes,
            "bytes_required": required,
            "ok": ok,
        }));
    }
    if !has_space {
        return ExitStatus::InsufficientDisk;
    }

    let lock_path = lock_dir.join("migration.lock");
    let _guard = match fs_lock::try_acquire(&lock_path, options.lock_timeout) {
        Ok(guard) => guard,
        Err(fs_lock::AcquireError::Timeout) => {
            log.write(serde_json::json!({
                "level": "error",
                "step": "lock",
                "path": lock_path,
                "status": "timeout",
            }));
            return ExitStatus::LockContention;
        }
        Err(error) => {
            log.write(serde_json::json!({
                "level": "error",
                "step": "lock",
                "path": lock_path,
                "status": "error",
                "error": error.to_string(),
            }));
            return ExitStatus::MigrationFailed;
        }
    };

    if source_marker.exists() && target_marker.exists() {
        log.write(serde_json::json!({
            "level": "info",
            "step": "marker_check_locked",
            "status": "already_migrated",
        }));
        return ExitStatus::Success;
    }

    if !args.from.exists() {
        if let Err(error) = crate::private_storage::create_dir_all(&target_harness) {
            log.write(serde_json::json!({
                "level": "error",
                "step": "create_harness_dir",
                "path": target_harness,
                "status": "error",
                "error": error.to_string(),
            }));
            return ExitStatus::MigrationFailed;
        }
        if let Err(error) = write_target_marker(&args) {
            log.write(serde_json::json!({
                "level": "error",
                "step": "marker",
                "path": target_marker,
                "status": "error",
                "error": error.to_string(),
            }));
            return ExitStatus::MigrationFailed;
        }
        log_complete(&mut log, started);
        return ExitStatus::Success;
    }

    if let Err(error) = crate::private_storage::create_dir_all(&target_harness) {
        log.write(serde_json::json!({
            "level": "error",
            "step": "create_harness_dir",
            "path": target_harness,
            "status": "error",
            "error": error.to_string(),
        }));
        return ExitStatus::MigrationFailed;
    }

    let mut failed = false;
    for &item in migration_items() {
        if let Err(error) = migrate_item(&args, item, &mut log) {
            failed = true;
            log.write(serde_json::json!({
                "level": "error",
                "step": "copy",
                "subtree": item.name,
                "status": "error",
                "error": error.to_string(),
            }));
        }
    }

    if failed {
        log.write(serde_json::json!({
            "level": "error",
            "step": "complete",
            "status": "failed",
        }));
        return ExitStatus::MigrationFailed;
    }

    if let Err(error) = write_source_marker(&args) {
        log.write(serde_json::json!({
            "level": "error",
            "step": "marker",
            "path": source_marker,
            "status": "error",
            "error": error.to_string(),
        }));
        return ExitStatus::MigrationFailed;
    }

    if let Err(error) = write_target_marker(&args) {
        log.write(serde_json::json!({
            "level": "error",
            "step": "marker",
            "path": target_marker,
            "status": "error",
            "error": error.to_string(),
        }));
        return ExitStatus::PartialState;
    }

    log_complete(&mut log, started);
    ExitStatus::Success
}

fn write_status(target_root: &Path, harness: Harness, source_root: Option<&Path>) {
    let marker_path = target_marker_path_from(target_root, &harness);
    let source_marker_path = source_root.map(|root| root.join(SOURCE_MARKER));
    let source_marker_present = source_marker_path
        .as_ref()
        .is_some_and(|path| path.exists());
    let mut value = match fs::read(&marker_path) {
        Ok(bytes) => match serde_json::from_slice::<Marker>(&bytes) {
            Ok(marker) => serde_json::json!({
                "harness": harness.storage_segment(),
                "target_root": target_root.display().to_string(),
                "migrated": true,
                "marker_path": marker_path.display().to_string(),
                "migrated_at": marker.timestamp,
                "source_path": marker.source_path,
                "aft_version": marker.aft_version,
            }),
            Err(_) => serde_json::json!({
                "harness": harness.storage_segment(),
                "target_root": target_root.display().to_string(),
                "migrated": true,
                "marker_path": marker_path.display().to_string(),
            }),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => serde_json::json!({
            "harness": harness.storage_segment(),
            "target_root": target_root.display().to_string(),
            "migrated": false,
        }),
        Err(_) => serde_json::json!({
            "harness": harness.storage_segment(),
            "target_root": target_root.display().to_string(),
            "migrated": false,
        }),
    };

    if let Some(source_marker_path) = source_marker_path {
        value["source_marker_path"] = serde_json::json!(source_marker_path.display().to_string());
        value["source_marker_present"] = serde_json::json!(source_marker_present);
        value["partial_state"] = serde_json::json!(source_marker_present && !marker_path.exists());
    }

    let mut stdout = io::stdout().lock();
    let _ = serde_json::to_writer(&mut stdout, &value);
    let _ = stdout.write_all(b"\n");
}

/// Sweep `staging-*` files and directories from target parents used by migration.
/// Idempotent. Returns the number of staging entries removed.
pub fn cleanup_staging_dirs(target_root: &Path, harness: Harness) -> io::Result<usize> {
    let mut parents = BTreeSet::new();
    for &item in migration_items() {
        parents.insert(staging_parent_from_root(target_root, &harness, item));
    }

    let mut removed = 0;
    for parent in parents {
        let entries = match fs::read_dir(&parent) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };

        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(s) = name.to_str() else { continue };
            if !s.starts_with("staging-") {
                continue;
            }

            remove_staging_path(&entry.path())?;
            removed += 1;
        }
    }

    Ok(removed)
}

fn staging_parent_from_root(target_root: &Path, harness: &Harness, item: MigrationItem) -> PathBuf {
    let final_path = target_path_from_root(target_root, harness, item);
    if item.merge == MergeKind::ChildUnion {
        final_path
    } else {
        final_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| target_root.to_path_buf())
    }
}

fn remove_staging_path(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn log_complete(log: &mut JsonLogger, started: SystemTime) {
    let duration_ms = SystemTime::now()
        .duration_since(started)
        .unwrap_or(Duration::ZERO)
        .as_millis();
    log.write(serde_json::json!({
        "level": "info",
        "step": "complete",
        "status": "ok",
        "duration_ms": duration_ms,
    }));
}

#[derive(Clone, Copy)]
struct MigrationItem {
    name: &'static str,
    source_name: &'static str,
    target: TargetKind,
    entry: EntryKind,
    merge: MergeKind,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TargetKind {
    Harness,
    Root,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EntryKind {
    Directory,
    File,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MergeKind {
    Whole,
    ChildUnion,
    TrustJson,
}

fn migration_items() -> &'static [MigrationItem] {
    &[
        MigrationItem {
            name: "bash-tasks",
            source_name: "bash-tasks",
            target: TargetKind::Harness,
            entry: EntryKind::Directory,
            merge: MergeKind::Whole,
        },
        MigrationItem {
            name: "backups",
            source_name: "backups",
            target: TargetKind::Harness,
            entry: EntryKind::Directory,
            merge: MergeKind::Whole,
        },
        MigrationItem {
            name: "filters",
            source_name: "filters",
            target: TargetKind::Harness,
            entry: EntryKind::Directory,
            merge: MergeKind::ChildUnion,
        },
        MigrationItem {
            name: "index",
            source_name: "index",
            target: TargetKind::Root,
            entry: EntryKind::Directory,
            merge: MergeKind::ChildUnion,
        },
        MigrationItem {
            name: "last_announced_version",
            source_name: "last_announced_version",
            target: TargetKind::Harness,
            entry: EntryKind::File,
            merge: MergeKind::Whole,
        },
        // ONNX runtime is host-global (shared by all harnesses). Moving it
        // avoids forcing every user to re-download the ~200MB runtime after
        // upgrading. ChildUnion lets a later second-harness migration coexist
        // if a partial migration somehow ran a different runtime version.
        MigrationItem {
            name: "onnxruntime",
            source_name: "onnxruntime",
            target: TargetKind::Root,
            entry: EntryKind::Directory,
            merge: MergeKind::ChildUnion,
        },
        MigrationItem {
            name: "last-update-check.json",
            source_name: "last-update-check.json",
            target: TargetKind::Harness,
            entry: EntryKind::File,
            merge: MergeKind::Whole,
        },
        MigrationItem {
            name: "semantic",
            source_name: "semantic",
            target: TargetKind::Root,
            entry: EntryKind::Directory,
            merge: MergeKind::ChildUnion,
        },
        MigrationItem {
            name: "symbols",
            source_name: "symbols",
            target: TargetKind::Root,
            entry: EntryKind::Directory,
            merge: MergeKind::ChildUnion,
        },
        MigrationItem {
            name: "trusted-filter-projects.json",
            source_name: "trusted-filter-projects.json",
            target: TargetKind::Root,
            entry: EntryKind::File,
            merge: MergeKind::TrustJson,
        },
        MigrationItem {
            name: "warned_tools.json",
            source_name: "warned_tools.json",
            target: TargetKind::Harness,
            entry: EntryKind::File,
            merge: MergeKind::Whole,
        },
    ]
}

fn migrate_item(args: &MigrationArgs, item: MigrationItem, log: &mut JsonLogger) -> io::Result<()> {
    let source = args.from.join(item.source_name);
    if !source.exists() {
        log.write(serde_json::json!({
            "level": "info",
            "step": "copy",
            "subtree": item.name,
            "status": "missing",
            "action": "skipped",
        }));
        return Ok(());
    }

    match item.merge {
        MergeKind::Whole => migrate_whole(args, item, &source, log),
        MergeKind::ChildUnion => migrate_child_union(args, item, &source, log),
        MergeKind::TrustJson => merge_trust_file(args, &source, log),
    }
}

fn migrate_whole(
    args: &MigrationArgs,
    item: MigrationItem,
    source: &Path,
    log: &mut JsonLogger,
) -> io::Result<()> {
    let final_path = target_path(args, item);
    if final_path.exists() {
        log.write(serde_json::json!({
            "level": "warn",
            "step": "copy",
            "subtree": item.name,
            "status": "already_exists",
            "action": "skipped",
        }));
        return Ok(());
    }

    let staging = staging_path(&final_path, item.name);
    if let Some(parent) = staging.parent() {
        crate::private_storage::create_dir_all(parent)?;
    }

    let bytes = match item.entry {
        EntryKind::Directory => copy_dir_recursive(source, &staging),
        EntryKind::File => copy_file(source, &staging),
    }?;

    fs::rename(&staging, &final_path)?;
    #[cfg(test)]
    after_publish(source, &final_path);
    log.write(serde_json::json!({
        "level": "info",
        "step": "copy",
        "subtree": item.name,
        "status": "ok",
        "bytes": bytes,
    }));
    Ok(())
}

fn migrate_child_union(
    args: &MigrationArgs,
    item: MigrationItem,
    source: &Path,
    log: &mut JsonLogger,
) -> io::Result<()> {
    let final_dir = target_path(args, item);
    crate::private_storage::create_dir_all(&final_dir)?;
    let mut copied_bytes = 0_u64;
    let mut failed = false;

    for entry in sorted_read_dir(source)? {
        let name = entry.file_name();
        let child_source = entry.path();
        let child_final = final_dir.join(&name);
        if child_final.exists() {
            log.write(serde_json::json!({
                "level": "warn",
                "step": "copy",
                "subtree": item.name,
                "path": child_final,
                "status": "already_exists",
                "action": "skipped",
            }));
            continue;
        }
        let child_staging = staging_path(&child_final, item.name);
        let result = if child_source.is_dir() {
            copy_dir_recursive(&child_source, &child_staging)
        } else {
            copy_file(&child_source, &child_staging)
        };
        match result {
            Ok(bytes) => {
                fs::rename(&child_staging, &child_final)?;
                #[cfg(test)]
                after_publish(&child_source, &child_final);
                copied_bytes = copied_bytes.saturating_add(bytes);
            }
            Err(error) => {
                failed = true;
                log.write(serde_json::json!({
                    "level": "error",
                    "step": "copy",
                    "subtree": item.name,
                    "path": child_source,
                    "status": "error",
                    "error": error.to_string(),
                }));
            }
        }
    }

    if failed {
        return Err(io::Error::other("one or more children failed to copy"));
    }

    log.write(serde_json::json!({
        "level": "info",
        "step": "copy",
        "subtree": item.name,
        "status": "ok",
        "bytes": copied_bytes,
    }));
    Ok(())
}

fn merge_trust_file(args: &MigrationArgs, source: &Path, log: &mut JsonLogger) -> io::Result<()> {
    let target = args.to.join("trusted-filter-projects.json");
    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();
    for value in [
        read_json_string_array(&target)?,
        read_json_string_array(source)?,
    ] {
        for item in value {
            if seen.insert(item.clone()) {
                paths.push(item);
            }
        }
    }
    atomic_write_json(&target, &paths)?;
    log.write(serde_json::json!({
        "level": "info",
        "step": "copy",
        "subtree": "trusted-filter-projects.json",
        "status": "ok",
        "entries": paths.len(),
    }));
    Ok(())
}

fn read_json_string_array(path: &Path) -> io::Result<Vec<String>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Vec<String>>(&bytes).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn target_path(args: &MigrationArgs, item: MigrationItem) -> PathBuf {
    target_path_from_root(&args.to, &args.harness, item)
}

fn target_path_from_root(target_root: &Path, harness: &Harness, item: MigrationItem) -> PathBuf {
    match item.target {
        TargetKind::Harness => target_root.join(harness.storage_segment()).join(item.name),
        TargetKind::Root => target_root.join(item.name),
    }
}

fn staging_path(final_path: &Path, subtree: &str) -> PathBuf {
    let final_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(subtree);
    final_path.with_file_name(format!(
        "staging-{subtree}-{final_name}-{}-{}",
        std::process::id(),
        now_millis()
    ))
}

fn copy_dir_recursive(source: &Path, target: &Path) -> io::Result<u64> {
    crate::private_storage::create_dir_all(target)?;
    let mut bytes = 0_u64;
    for entry in sorted_read_dir(source)? {
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let copied = if source_path.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?
        } else {
            copy_file(&source_path, &target_path)?
        };
        bytes = bytes.saturating_add(copied);
    }
    sync_path(target);
    Ok(bytes)
}

fn copy_file(source: &Path, target: &Path) -> io::Result<u64> {
    if let Some(parent) = target.parent() {
        crate::private_storage::create_dir_all(parent)?;
    }
    let bytes = crate::private_storage::copy(source, target)?;
    sync_path(target);
    Ok(bytes)
}

fn sorted_read_dir(path: &Path) -> io::Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn source_size(path: &Path) -> io::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let metadata = fs::metadata(path)?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    let mut total = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        total = total.saturating_add(source_size(&entry.path())?);
    }
    Ok(total)
}

/// One destination volume of a migration and the source bytes headed for it.
struct DestinationVolume {
    /// A folder on that volume: the target root, or the first destination
    /// found on another volume.
    path: PathBuf,
    source_bytes: u64,
    free_bytes: u64,
}

/// Group the bytes to migrate by the volume each destination is on, and read
/// each volume's free space. The target root's volume is charged with every
/// byte not headed to another volume; when all destinations share it, that is
/// the whole source tree, as the check has always counted. An error names the
/// path whose volume could not be read.
fn destination_volumes(
    args: &MigrationArgs,
    source_bytes: u64,
    options: &Options,
) -> Result<Vec<DestinationVolume>, (PathBuf, io::Error)> {
    let device = |path: &Path| {
        crate::walk_boundary::filesystem_device_id(existing_ancestor(path))
            .map_err(|error| (path.to_path_buf(), error))
    };
    let free_at = |path: &Path| {
        options
            .disk_free_override
            .map_or_else(|| free_bytes_at(path), Ok)
            .map_err(|error| (path.to_path_buf(), error))
    };
    let root_device = device(&args.to)?;
    let mut elsewhere: Vec<(Option<u64>, PathBuf, u64)> = Vec::new();
    for &item in migration_items() {
        let destination = target_path(args, item);
        let destination_device = device(&destination)?;
        if destination_device == root_device {
            continue;
        }
        let source = args.from.join(item.source_name);
        let bytes = source_size(&source).map_err(|error| (source, error))?;
        match elsewhere
            .iter_mut()
            .find(|(device, _, _)| *device == destination_device)
        {
            Some(volume) => volume.2 = volume.2.saturating_add(bytes),
            None => elsewhere.push((
                destination_device,
                existing_ancestor(&destination).to_path_buf(),
                bytes,
            )),
        }
    }
    let moved_elsewhere = elsewhere
        .iter()
        .fold(0_u64, |total, (_, _, bytes)| total.saturating_add(*bytes));
    let mut volumes = vec![DestinationVolume {
        path: args.to.clone(),
        source_bytes: source_bytes.saturating_sub(moved_elsewhere),
        free_bytes: free_at(&args.to)?,
    }];
    for (_, path, bytes) in elsewhere {
        volumes.push(DestinationVolume {
            free_bytes: free_at(&path)?,
            path,
            source_bytes: bytes,
        });
    }
    Ok(volumes)
}

#[cfg(test)]
thread_local! {
    /// Free bytes reported for folders at or under each path, so a test can
    /// give a simulated second volume less room than the target root.
    static FREE_BYTES_FOR_TEST: std::cell::RefCell<Vec<(PathBuf, u64)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(unix)]
fn free_bytes_at(path: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    #[cfg(test)]
    if let Some(bytes) = FREE_BYTES_FOR_TEST.with(|slot| {
        slot.borrow()
            .iter()
            .find(|(root, _)| path.starts_with(root))
            .map(|(_, bytes)| *bytes)
    }) {
        return Ok(bytes);
    }
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
fn free_bytes_at(_path: &Path) -> io::Result<u64> {
    // v0.27 is Unix-prioritized. Windows disk preflight should be wired to
    // GetDiskFreeSpaceExW through windows-sys in a follow-up.
    #[cfg(test)]
    if let Some(bytes) = FREE_BYTES_FOR_TEST.with(|slot| {
        slot.borrow()
            .iter()
            .find(|(root, _)| _path.starts_with(root))
            .map(|(_, bytes)| *bytes)
    }) {
        return Ok(bytes);
    }
    Ok(u64::MAX)
}

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

#[derive(Serialize, Deserialize)]
struct Marker {
    timestamp: String,
    source_path: String,
    target_path: String,
    harness: String,
    aft_version: String,
}

fn marker(args: &MigrationArgs) -> Marker {
    Marker {
        timestamp: iso_timestamp_now(),
        source_path: args.from.display().to_string(),
        target_path: args.to.display().to_string(),
        harness: args.harness.storage_segment().to_string(),
        aft_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn write_source_marker(args: &MigrationArgs) -> io::Result<()> {
    atomic_write_json(&source_marker_path(args), &marker(args))
}

fn write_target_marker(args: &MigrationArgs) -> io::Result<()> {
    crate::private_storage::create_dir_all(args.to.join(args.harness.storage_segment()))?;
    atomic_write_json(&target_marker_path(args), &marker(args))
}

fn source_marker_path(args: &MigrationArgs) -> PathBuf {
    args.from.join(SOURCE_MARKER)
}

fn target_marker_path(args: &MigrationArgs) -> PathBuf {
    target_marker_path_from(&args.to, &args.harness)
}

fn target_marker_path_from(target_root: &Path, harness: &Harness) -> PathBuf {
    target_root
        .join(harness.storage_segment())
        .join(TARGET_MARKER)
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        crate::private_storage::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    crate::durability::atomic_write(
        path,
        &bytes,
        crate::durability::AtomicWriteDurability::FileAndDirectory,
    )
}

fn sync_path(path: &Path) {
    if let Ok(file) = File::open(path) {
        let _ = file.sync_all();
    }
}

pub fn parse_cli_args<I, S>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut from = None;
    let mut to = None;
    let mut harness = None;
    let mut log = None;
    let mut status = false;
    let mut iter = args.into_iter().map(Into::into);
    while let Some(arg) = iter.next() {
        let arg = arg.to_string_lossy().to_string();
        if arg == "--status" {
            status = true;
            continue;
        }
        let value = match arg.as_str() {
            "--from" | "--to" | "--harness" | "--log" => iter
                .next()
                .ok_or_else(|| format!("missing value for {arg}"))?,
            "--help" | "-h" => return Err(help_text()),
            other => return Err(format!("unknown argument: {other}\n\n{}", help_text())),
        };
        match arg.as_str() {
            "--from" => from = Some(PathBuf::from(value)),
            "--to" => to = Some(PathBuf::from(value)),
            "--harness" => {
                let value = value.to_string_lossy();
                harness = Some(
                    value
                        .parse::<Harness>()
                        .map_err(|err| format!("invalid harness: {err}\n\n{}", help_text()))?,
                );
            }
            "--log" => log = Some(PathBuf::from(value)),
            _ => unreachable!(),
        }
    }

    let to = to.ok_or_else(|| format!("missing required --to\n\n{}", help_text()))?;
    let harness =
        harness.ok_or_else(|| format!("missing required --harness\n\n{}", help_text()))?;
    if status {
        return Ok(Args {
            from,
            to,
            harness,
            log,
            status,
        });
    }

    Ok(Args {
        from: Some(from.ok_or_else(|| format!("missing required --from\n\n{}", help_text()))?),
        to,
        harness,
        log: Some(log.ok_or_else(|| format!("missing required --log\n\n{}", help_text()))?),
        status,
    })
}
pub fn help_text() -> String {
    "Usage: aft migrate-storage --from <legacy_root> --to <new_root> --harness <opencode|pi> --log <log_file>\n       aft migrate-storage --status --to <new_root> --harness <opencode|pi>\n\n\
Blocking one-shot migration from legacy AFT storage into the CortexKit-rooted layout.\n\n\
Exit codes:\n  0  success (including idempotent already-migrated/no-op; missing legacy source is a no-op)\n  1  source unreadable\n  2  insufficient disk space during preflight\n  3  migration lock contention\n  4  migration in progress / partial marker state\n  5  migration failed; inspect the log file"
        .to_string()
}

#[cfg(test)]
type AfterPublish = Box<dyn FnOnce(&Path, &Path)>;
#[cfg(test)]
thread_local! {
    static AFTER_PUBLISH: std::cell::RefCell<Option<AfterPublish>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn after_publish(source: &Path, target: &Path) {
    let hook = AFTER_PUBLISH.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(source, target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_checks_free_space_on_the_volume_of_each_destination() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("legacy");
        fs::create_dir_all(from.join("index/key")).unwrap();
        fs::write(from.join("index/key/cache.bin"), vec![0_u8; 1000]).unwrap();
        fs::create_dir_all(from.join("backups")).unwrap();
        fs::write(from.join("backups/meta.json"), b"{}").unwrap();
        let to = dir.path().join("target");
        let index = to.join("index");
        fs::create_dir_all(&index).unwrap();
        // The index folder is a separate volume with room for less than twice
        // the index bytes; the target root itself has plenty of room.
        let _volume = crate::walk_boundary::simulated_volumes::simulate(&index, u64::MAX - 30);
        FREE_BYTES_FOR_TEST
            .with(|slot| *slot.borrow_mut() = vec![(index.clone(), 1500), (to.clone(), u64::MAX)]);
        let log = dir.path().join("migration.jsonl");

        let status = run_with_options(
            Args {
                from: Some(from),
                to: to.clone(),
                harness: Harness::Opencode,
                log: Some(log.clone()),
                status: false,
            },
            Options::default(),
        );
        FREE_BYTES_FOR_TEST.with(|slot| slot.borrow_mut().clear());

        assert_eq!(status, ExitStatus::InsufficientDisk);
        let preflight = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|record| record["step"] == "preflight")
            .collect::<Vec<_>>();
        let index_line = preflight
            .iter()
            .find(|record| record["path"] == serde_json::json!(index))
            .expect("the index volume is checked on its own");
        assert_eq!(index_line["bytes_source"], 1000);
        assert_eq!(index_line["ok"], false);
        let root_line = preflight
            .iter()
            .find(|record| record["path"] == serde_json::json!(to))
            .expect("the target root is checked");
        assert_eq!(
            root_line["bytes_source"], 2,
            "bytes headed to the index volume are not charged to the root"
        );
        assert_eq!(root_line["ok"], true);
    }

    #[test]
    fn migration_marker_does_not_truncate_a_colliding_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("marker.json");
        fs::write(&path, b"old marker").unwrap();
        let collision = std::rc::Rc::new(std::cell::RefCell::new(None));
        let hook_collision = collision.clone();
        crate::durability::BEFORE_TEMP_CREATE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |temporary| {
                fs::write(temporary, b"another writer's temporary").unwrap();
                *hook_collision.borrow_mut() = Some(temporary.to_path_buf());
            }))
        });
        assert!(
            atomic_write_json(&path, &serde_json::json!({"state": "new"})).is_err(),
            "colliding temp was opened with truncate instead of exclusive creation"
        );
        let temporary = collision.borrow().clone().expect("creation hook must run");
        assert_eq!(fs::read(temporary).unwrap(), b"another writer's temporary");
        assert_eq!(fs::read(path).unwrap(), b"old marker");
    }

    #[test]
    fn published_migration_uses_copy_bytes_not_late_tree_reads() {
        for merge in [MergeKind::Whole, MergeKind::ChildUnion] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source/symbols");
            fs::create_dir_all(source.join("child")).unwrap();
            fs::write(source.join("child/payload"), b"copied!").unwrap();
            let args = MigrationArgs {
                from: dir.path().join("source"),
                to: dir.path().join("target"),
                harness: Harness::Opencode,
                log: dir.path().join("migration.jsonl"),
            };
            let item = MigrationItem {
                name: "symbols",
                source_name: "symbols",
                target: TargetKind::Root,
                entry: EntryKind::Directory,
                merge,
            };
            AFTER_PUBLISH.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(|source, target| {
                    fs::remove_dir_all(source).unwrap();
                    fs::create_dir_all(source).unwrap();
                    fs::write(
                        source.join("changed"),
                        b"different source after publication",
                    )
                    .unwrap();
                    fs::write(target.join("concurrent"), b"another writer").unwrap();
                }))
            });
            let mut log = JsonLogger::open(&args.log, args.harness.clone());
            migrate_item(&args, item, &mut log).unwrap();
            drop(log);
            let records: Vec<serde_json::Value> = fs::read_to_string(&args.log)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                records.last().unwrap()["bytes"],
                7,
                "published copy was sized from mutable paths"
            );
        }
    }
}
