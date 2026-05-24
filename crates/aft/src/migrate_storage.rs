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
        write_status(&args.to, args.harness);
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

    let target_root_error = fs::create_dir_all(&args.to).err();
    let mut log = JsonLogger::open(&args.log, args.harness);
    let started = SystemTime::now();
    log.write(serde_json::json!({
        "step": "start",
        "level": "info",
        "from": args.from,
        "to": args.to,
        "harness": args.harness.as_str(),
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
    if let Err(error) = fs::create_dir_all(&lock_dir) {
        log.write(serde_json::json!({
            "level": "error",
            "step": "create_lock_dir",
            "path": lock_dir,
            "status": "error",
            "error": error.to_string(),
        }));
        return ExitStatus::MigrationFailed;
    }

    let target_harness = args.to.join(args.harness.as_str());
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

    let free_bytes = match options.disk_free_override {
        Some(bytes) => Ok(bytes),
        None => free_bytes_at(&args.to),
    };
    let free_bytes = match free_bytes {
        Ok(bytes) => bytes,
        Err(error) => {
            log.write(serde_json::json!({
                "level": "error",
                "step": "preflight",
                "path": args.to,
                "status": "disk_check_failed",
                "bytes_source": source_bytes,
                "error": error.to_string(),
            }));
            return ExitStatus::MigrationFailed;
        }
    };
    let required = source_bytes.saturating_mul(2);
    let has_space = free_bytes >= required;
    log.write(serde_json::json!({
        "level": if has_space { "info" } else { "error" },
        "step": "preflight",
        "bytes_source": source_bytes,
        "bytes_free": free_bytes,
        "bytes_required": required,
        "ok": has_space,
    }));
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
        if let Err(error) = fs::create_dir_all(&target_harness) {
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

    if let Err(error) = fs::create_dir_all(&target_harness) {
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

fn write_status(target_root: &Path, harness: Harness) {
    let marker_path = target_marker_path_from(target_root, harness);
    let value = match fs::read(&marker_path) {
        Ok(bytes) => match serde_json::from_slice::<Marker>(&bytes) {
            Ok(marker) => serde_json::json!({
                "harness": harness.as_str(),
                "target_root": target_root.display().to_string(),
                "migrated": true,
                "marker_path": marker_path.display().to_string(),
                "migrated_at": marker.timestamp,
                "source_path": marker.source_path,
                "aft_version": marker.aft_version,
            }),
            Err(_) => serde_json::json!({
                "harness": harness.as_str(),
                "target_root": target_root.display().to_string(),
                "migrated": true,
                "marker_path": marker_path.display().to_string(),
            }),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => serde_json::json!({
            "harness": harness.as_str(),
            "target_root": target_root.display().to_string(),
            "migrated": false,
        }),
        Err(_) => serde_json::json!({
            "harness": harness.as_str(),
            "target_root": target_root.display().to_string(),
            "migrated": false,
        }),
    };

    let mut stdout = io::stdout().lock();
    let _ = serde_json::to_writer(&mut stdout, &value);
    let _ = stdout.write_all(b"\n");
}

/// Sweep `staging-*` directories from prior failed migrations in the harness root.
/// Idempotent. Returns the number of staging dirs removed.
pub fn cleanup_staging_dirs(target_root: &Path, harness: Harness) -> io::Result<usize> {
    let harness_dir = target_root.join(harness.as_str());
    if !harness_dir.exists() {
        return Ok(0);
    }

    let mut removed = 0;
    for entry in fs::read_dir(&harness_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if !s.starts_with("staging-") {
            continue;
        }
        let path = entry.path();
        if fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }

    Ok(removed)
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
        fs::create_dir_all(parent)?;
    }

    let copy_result = match item.entry {
        EntryKind::Directory => copy_dir_recursive(source, &staging),
        EntryKind::File => copy_file(source, &staging).map(|_| ()),
    };
    if let Err(error) = copy_result {
        return Err(error);
    }

    fs::rename(&staging, &final_path)?;
    let bytes = source_size(source)?;
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
    fs::create_dir_all(&final_dir)?;
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
            copy_file(&child_source, &child_staging).map(|_| ())
        };
        match result {
            Ok(()) => {
                fs::rename(&child_staging, &child_final)?;
                copied_bytes = copied_bytes.saturating_add(source_size(&child_final)?);
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
    match item.target {
        TargetKind::Harness => args.to.join(args.harness.as_str()).join(item.name),
        TargetKind::Root => args.to.join(item.name),
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

fn copy_dir_recursive(source: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in sorted_read_dir(source)? {
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else {
            copy_file(&source_path, &target_path)?;
        }
    }
    sync_path(target);
    Ok(())
}

fn copy_file(source: &Path, target: &Path) -> io::Result<u64> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = fs::copy(source, target)?;
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

#[cfg(unix)]
fn free_bytes_at(path: &Path) -> io::Result<u64> {
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
fn free_bytes_at(_path: &Path) -> io::Result<u64> {
    // v0.27 is Unix-prioritized. Windows disk preflight should be wired to
    // GetDiskFreeSpaceExW through windows-sys in a follow-up.
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
        harness: args.harness.as_str().to_string(),
        aft_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn write_source_marker(args: &MigrationArgs) -> io::Result<()> {
    atomic_write_json(&source_marker_path(args), &marker(args))
}

fn write_target_marker(args: &MigrationArgs) -> io::Result<()> {
    fs::create_dir_all(args.to.join(args.harness.as_str()))?;
    atomic_write_json(&target_marker_path(args), &marker(args))
}

fn source_marker_path(args: &MigrationArgs) -> PathBuf {
    args.from.join(SOURCE_MARKER)
}

fn target_marker_path(args: &MigrationArgs) -> PathBuf {
    target_marker_path_from(&args.to, args.harness)
}

fn target_marker_path_from(target_root: &Path, harness: Harness) -> PathBuf {
    target_root.join(harness.as_str()).join(TARGET_MARKER)
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("marker"),
        std::process::id(),
        now_millis()
    ));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        serde_json::to_writer(&mut file, value).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent() {
            sync_path(parent);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
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
