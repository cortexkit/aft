//! Handler for the `move_file` command: rename/move a file with backup.

use std::fs;
use std::path::{Path, PathBuf};

use lsp_types::FileChangeType;

use crate::context::AppContext;
use crate::edit;
use crate::protocol::{RawRequest, Response};

/// Handle a `move_file` request.
///
/// Params:
///   - `file` (string, required) — source file path
///   - `destination` (string, required) — destination file path
///
/// Returns: `{ file, destination, moved, backup_id }`
pub fn handle_move_file(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "move_file: missing required param 'file'",
            );
        }
    };

    let destination = match req.params.get("destination").and_then(|v| v.as_str()) {
        Some(d) => d,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "move_file: missing required param 'destination'",
            );
        }
    };

    let src_path = match ctx.validate_write_location(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    let dst_path = match ctx.validate_write_location(&req.id, Path::new(destination)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let source_metadata = match std::fs::symlink_metadata(&src_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // When the source is missing AND the destination already exists, the
            // most likely cause is that this rename was already done earlier in
            // the session (or by another process). Surfacing this distinction
            // saves the agent a round-trip to discover it via `ls` or stat.
            if std::fs::symlink_metadata(&dst_path).is_ok() {
                return Response::error(
                    &req.id,
                    "file_not_found",
                    format!(
                        "move_file: source file not found: {}. Destination '{}' already exists \
                         — was this file already moved earlier? Verify with `read` before retrying.",
                        file, destination
                    ),
                );
            }
            return Response::error(
                &req.id,
                "file_not_found",
                format!("move_file: source file not found: {}", file),
            );
        }
        Err(error) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("move_file: failed to stat source file: {}", error),
            );
        }
    };

    if source_metadata.is_dir() {
        return Response::error(
            &req.id,
            "invalid_request",
            format!("move_file: '{}' is a directory, not a file", file),
        );
    }

    if std::fs::symlink_metadata(&dst_path).is_ok() && !is_case_only_alias(&src_path, &dst_path) {
        return Response::error(
            &req.id,
            "invalid_request",
            format!("move_file: destination already exists: {}", destination),
        );
    }

    let _mutation =
        crate::runtime_drain::record_file_mutation(ctx, [src_path.as_path(), dst_path.as_path()]);

    // Backup source before moving
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        src_path.as_path(),
        "move_file: pre-move backup",
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    if let Err(e) = ctx.backup().lock().snapshot_op_tombstone(
        req.session(),
        &op_id,
        &dst_path,
        "move_file: destination created during move",
    ) {
        ctx.backup()
            .lock()
            .discard_operation_entries(req.session(), &op_id);
        return Response::error(&req.id, e.code(), e.to_string());
    }

    // Create parent directories for destination
    if let Some(parent) = dst_path.parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                ctx.backup()
                    .lock()
                    .discard_operation_entries(req.session(), &op_id);
                return Response::error(
                    &req.id,
                    "io_error",
                    format!("move_file: failed to create directories: {}", e),
                );
            }
        }
    }

    // Move the file
    let move_outcome = match if is_case_only_alias(&src_path, &dst_path) {
        rename_case_only(&src_path, &dst_path)
    } else {
        move_file_on_disk(&src_path, &dst_path)
    } {
        MoveOutcome::Moved => MoveOutcome::Moved,
        MoveOutcome::CopiedSourceDeleteFailed(message) => {
            crate::slog_warn!("move_file: copied but failed to remove source: {}", message);
            MoveOutcome::CopiedSourceDeleteFailed(message)
        }
        MoveOutcome::Failed(message) => {
            ctx.backup()
                .lock()
                .discard_operation_entries(req.session(), &op_id);
            return Response::error(
                &req.id,
                "io_error",
                format!("move_file: failed to move file: {}", message),
            );
        }
    };

    log::debug!("move_file: {} -> {}", file, destination);

    if move_outcome == MoveOutcome::Moved {
        let source_for_lsp = unresolved_existing_path(&src_path, Path::new(file));
        ctx.lsp_notify_watched_config_file(&source_for_lsp, FileChangeType::DELETED);
    }
    ctx.lsp_notify_watched_config_file(&dst_path, FileChangeType::CREATED);

    let mut result = move_success_result(file, destination, move_outcome);

    if let Some(ref id) = backup_id {
        result["backup_id"] = serde_json::json!(id);
    }
    edit::attach_backup_skipped_reason(&mut result, ctx, req.session(), &op_id, None);

    Response::success(&req.id, result)
}

#[derive(Debug, PartialEq, Eq)]
enum MoveOutcome {
    Moved,
    CopiedSourceDeleteFailed(String),
    Failed(String),
}

fn move_file_on_disk(src_path: &Path, dst_path: &Path) -> MoveOutcome {
    match rename_noreplace(src_path, dst_path) {
        Ok(()) => {
            #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
            return remove_source_after_fallback(src_path);
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            MoveOutcome::Moved
        }
        Err(rename_error) => {
            if let Err(error) = crate::edit::remote_links::check_path(src_path) {
                return MoveOutcome::Failed(error.to_string());
            }
            let source_is_symlink = fs::symlink_metadata(src_path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink());
            if source_is_symlink {
                return move_symlink_after_rename_failure(src_path, dst_path, &rename_error);
            }

            copy_regular_file_noreplace(src_path, dst_path)
        }
    }
}

/// Install a completed copy without exposing partial bytes or replacing a
/// destination created after preflight. Staging stays on its filesystem.
fn copy_regular_file_noreplace(source: &Path, destination: &Path) -> MoveOutcome {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let staging = parent.join(format!(".aft-move-{}", crate::backup::new_op_id()));
    let directory = fs::DirBuilder::new();
    #[cfg(unix)]
    let directory = {
        use std::os::unix::fs::DirBuilderExt;
        let mut directory = directory;
        directory.mode(0o700);
        directory
    };
    if let Err(error) = directory.create(&staging) {
        return MoveOutcome::Failed(error.to_string());
    }
    let staged_file = staging.join("file");
    let copied = if crate::context::confinement::remote_mutation() {
        (|| {
            let mut source = fs::File::open(source)?;
            let mut staged = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged_file)?;
            crate::edit::remote_links::check_open(&staged, &staged_file)?;
            std::io::copy(&mut source, &mut staged)?;
            staged.set_permissions(source.metadata()?.permissions())
        })()
    } else {
        fs::copy(source, &staged_file).map(|_| ())
    };
    let installed = copied.and_then(|()| rename_noreplace(&staged_file, destination));
    let _ = fs::remove_file(&staged_file);
    let _ = fs::remove_dir(&staging);
    match installed {
        Ok(()) => remove_source_after_fallback(source),
        Err(error) => MoveOutcome::Failed(error.to_string()),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = |path: &Path| {
        std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
    };
    let (from_dir, source, to_dir, destination) = if crate::context::confinement::remote_mutation()
    {
        let (from, source_name) = crate::edit::remote_links::pin_parent(source)?;
        let (to, destination_name) = crate::edit::remote_links::pin_parent(destination)?;
        crate::edit::remote_links::check_at(from.as_raw_fd(), &source_name, source)?;
        (Some(from), source_name, Some(to), destination_name)
    } else {
        (None, path(source)?, None, path(destination)?)
    };
    let fd = |dir: &Option<crate::pinned_dir::PinnedDir>| {
        dir.as_ref().map_or(libc::AT_FDCWD, |dir| dir.as_raw_fd())
    };
    // Both NUL-terminated path buffers remain live through the native call.
    // The raw syscall is used instead of glibc's renameat2() wrapper, which
    // only exists from glibc 2.28: the release builds Linux binaries against an
    // older glibc so they run on older distributions, and the wrapper's symbol
    // fails to link there. The kernel call itself has existed since Linux 3.15.
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            fd(&from_dir),
            source.as_ptr(),
            fd(&to_dir),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = if from_dir.is_some() {
        unsafe {
            libc::renameatx_np(
                fd(&from_dir),
                source.as_ptr(),
                fd(&to_dir),
                destination.as_ptr(),
                libc::RENAME_EXCL,
            )
        }
    } else {
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) }
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    crate::edit::remote_links::check_path(source)?;
    let path = |path: &Path| -> std::io::Result<Vec<u16>> {
        let mut units: Vec<_> = path.as_os_str().encode_wide().collect();
        if units.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path contains NUL",
            ));
        }
        units.push(0);
        Ok(units)
    };
    let source = path(source)?;
    let destination = path(destination)?;
    // Zero flags forbid replacement and cross-volume copying. Copies instead
    // use the same staged, exclusive installation as on Unix.
    let result = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            0,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    // Without exclusive rename, install first; the caller reports any later
    // source-deletion failure separately from installation failure.
    if fs::symlink_metadata(source)?.file_type().is_symlink() {
        create_symlink(&fs::read_link(source)?, source, destination)
    } else {
        fs::hard_link(source, destination)
    }
}

fn move_symlink_after_rename_failure(
    src_path: &Path,
    dst_path: &Path,
    rename_error: &std::io::Error,
) -> MoveOutcome {
    let target = match fs::read_link(src_path) {
        Ok(target) => target,
        Err(_) => return MoveOutcome::Failed(rename_error.to_string()),
    };

    // std::fs::copy follows a symlink and would silently turn the destination
    // into a regular file. Recreate the link itself, preserving its target text:
    // a relative target stays relative and can legitimately dangle after a move.
    let create_result = create_symlink(&target, src_path, dst_path);
    match create_result {
        Ok(()) => remove_source_after_fallback(src_path),
        Err(error) => MoveOutcome::Failed(error.to_string()),
    }
}

fn remove_source_after_fallback(src_path: &Path) -> MoveOutcome {
    match crate::edit::remote_links::unlink(src_path) {
        Ok(()) => MoveOutcome::Moved,
        Err(remove_error) => MoveOutcome::CopiedSourceDeleteFailed(remove_error.to_string()),
    }
}

#[cfg(unix)]
fn create_symlink(target: &Path, _src_path: &Path, dst_path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, dst_path)
}

#[cfg(windows)]
fn create_symlink(target: &Path, src_path: &Path, dst_path: &Path) -> std::io::Result<()> {
    let resolved_target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        src_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(target)
    };
    if fs::metadata(resolved_target).is_ok_and(|metadata| metadata.is_dir()) {
        std::os::windows::fs::symlink_dir(target, dst_path)
    } else {
        // Dangling targets cannot reveal their type; file links are the safest
        // default and match this command's file-only contract.
        std::os::windows::fs::symlink_file(target, dst_path)
    }
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _src_path: &Path, _dst_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlink moves are unsupported on this platform",
    ))
}

fn unresolved_existing_path(resolved_path: &Path, requested_path: &Path) -> PathBuf {
    if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        resolved_path.to_path_buf()
    }
}

fn move_success_result(
    file: &str,
    destination: &str,
    move_outcome: MoveOutcome,
) -> serde_json::Value {
    let mut result = serde_json::json!({
        "file": file,
        "destination": destination,
        "moved": true,
    });

    if let MoveOutcome::CopiedSourceDeleteFailed(message) = move_outcome {
        result["complete"] = serde_json::json!(false);
        result["source_delete_failed"] = serde_json::json!(true);
        result["warning"] = serde_json::json!(format!(
            "destination was written, but source file could not be deleted after copy: {message}. Both paths now exist; retry deleting the source or accept the duplicate."
        ));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::{move_success_result, MoveOutcome};

    #[cfg(unix)]
    use super::move_symlink_after_rename_failure;
    #[cfg(unix)]
    use std::io::{Error, ErrorKind};
    #[cfg(unix)]
    use std::path::Path;

    #[test]
    fn copied_but_source_delete_failed_shape_marks_partial_success() {
        let result = move_success_result(
            "src.txt",
            "dst.txt",
            MoveOutcome::CopiedSourceDeleteFailed("permission denied".to_string()),
        );

        assert_eq!(result["moved"], true);
        assert_eq!(result["complete"], false);
        assert_eq!(result["source_delete_failed"], true);
        assert!(result["warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("Both paths now exist")));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_fallback_preserves_relative_target_text() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source_dir = temp.path().join("source");
        let destination_dir = temp.path().join("destination");
        std::fs::create_dir_all(&source_dir).expect("source directory");
        std::fs::create_dir_all(&destination_dir).expect("destination directory");
        std::fs::write(source_dir.join("target.txt"), "target contents\n").expect("target file");
        let source = source_dir.join("link.txt");
        let destination = destination_dir.join("link.txt");
        let target = Path::new("target.txt");
        std::os::unix::fs::symlink(target, &source).expect("source symlink");

        let outcome = move_symlink_after_rename_failure(
            &source,
            &destination,
            &Error::from(ErrorKind::CrossesDevices),
        );

        assert_eq!(outcome, MoveOutcome::Moved);
        assert!(std::fs::symlink_metadata(&source).is_err());
        assert!(std::fs::symlink_metadata(&destination)
            .expect("destination metadata")
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(&destination).expect("link target"),
            target
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_fallback_moves_dangling_link() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source-link");
        let destination = temp.path().join("destination-link");
        let target = Path::new("missing/target.txt");
        std::os::unix::fs::symlink(target, &source).expect("dangling source symlink");

        let outcome = move_symlink_after_rename_failure(
            &source,
            &destination,
            &Error::from(ErrorKind::CrossesDevices),
        );

        assert_eq!(outcome, MoveOutcome::Moved);
        assert!(std::fs::symlink_metadata(&source).is_err());
        assert!(std::fs::symlink_metadata(&destination)
            .expect("destination metadata")
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(&destination).expect("link target"),
            target
        );
    }

    #[cfg(unix)]
    #[test]
    fn cross_device_symlink_fallback_preserves_existing_destination() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source-link");
        let destination = temp.path().join("destination-link");
        let target = Path::new("target.txt");
        std::os::unix::fs::symlink(target, &source).expect("source symlink");
        std::fs::write(&destination, "stale destination\n").expect("existing destination");

        let outcome = move_symlink_after_rename_failure(
            &source,
            &destination,
            &Error::from(ErrorKind::CrossesDevices),
        );

        assert!(matches!(outcome, MoveOutcome::Failed(_)));
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "stale destination\n"
        );
        assert_eq!(std::fs::read_link(&source).unwrap(), target);
    }
}

#[cfg(test)]
mod audit_regressions {
    #[test]
    fn copy_fallback_never_overwrites_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.txt");
        let destination = dir.path().join("destination.txt");
        std::fs::write(&source, "source").unwrap();
        std::fs::write(&destination, "external").unwrap();
        assert!(matches!(
            super::copy_regular_file_noreplace(&source, &destination),
            super::MoveOutcome::Failed(_)
        ));
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "external");
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "source");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        std::fs::remove_file(&destination).unwrap();
        assert_eq!(
            super::copy_regular_file_noreplace(&source, &destination),
            super::MoveOutcome::Moved
        );
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "source");
        assert!(!source.exists());
    }

    #[test]
    fn move_install_never_overwrites_late_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.txt");
        let destination = dir.path().join("destination.txt");
        std::fs::write(&source, "source").unwrap();
        // Model a creator installing the destination after handler preflight.
        std::fs::write(&destination, "external").unwrap();
        assert!(matches!(
            super::move_file_on_disk(&source, &destination),
            super::MoveOutcome::Failed(_)
        ));
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "external");
        assert_eq!(std::fs::read_to_string(source).unwrap(), "source");
    }

    #[test]
    fn case_only_rename_preserves_contents() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Foo.txt");
        let destination = dir.path().join("foo.txt");
        std::fs::write(&source, "contents").unwrap();
        let ctx = crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        );
        let req = serde_json::from_value(serde_json::json!({"id":"case-rename", "command":"move_file", "file":source, "destination":destination})).unwrap();
        let response = serde_json::to_value(super::handle_move_file(&req, &ctx)).unwrap();
        assert_eq!(response["success"], true, "{response}");
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "contents");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(names.contains(&std::ffi::OsString::from("foo.txt")));
        assert!(!names.contains(&std::ffi::OsString::from("Foo.txt")));
    }
}

fn is_case_only_alias(source: &Path, destination: &Path) -> bool {
    source.file_name() != destination.file_name()
        && source
            .to_string_lossy()
            .eq_ignore_ascii_case(&destination.to_string_lossy())
        && std::fs::symlink_metadata(source)
            .is_ok_and(|metadata| !metadata.file_type().is_symlink())
        && std::fs::canonicalize(source)
            .ok()
            .zip(std::fs::canonicalize(destination).ok())
            .is_some_and(|(source, destination)| source == destination)
}

fn rename_case_only(source: &Path, destination: &Path) -> MoveOutcome {
    // A private temporary directory avoids collisions and forces the filesystem
    // to store the requested spelling even if a direct rename is a no-op.
    let result = (|| -> std::io::Result<()> {
        let rename = |source: &Path, destination: &Path| {
            if crate::context::confinement::remote_mutation() {
                rename_noreplace(source, destination)
            } else {
                std::fs::rename(source, destination)
            }
        };
        let temporary = source
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!(".aft-case-rename-{}", crate::backup::new_op_id()));
        std::fs::create_dir(&temporary)?;
        let intermediate = temporary.join("file");
        if let Err(error) = rename(source, &intermediate) {
            let _ = std::fs::remove_dir(&temporary);
            return Err(error);
        }
        if let Err(error) = rename(&intermediate, destination) {
            if let Err(restore_error) = rename(&intermediate, source) {
                return Err(std::io::Error::other(format!(
                    "{error}; restore failed: {restore_error}; source retained at {}",
                    intermediate.display()
                )));
            }
            let _ = std::fs::remove_dir(&temporary);
            return Err(error);
        }
        let _ = std::fs::remove_dir(&temporary);
        Ok(())
    })();
    match result {
        Ok(()) => MoveOutcome::Moved,
        Err(error) => MoveOutcome::Failed(error.to_string()),
    }
}

#[cfg(all(test, unix))]
#[test]
fn remote_rename_refuses_a_multiply_linked_entry() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let alias = dir.path().join("alias");
    let destination = dir.path().join("destination");
    fs::write(&source, b"original").unwrap();
    fs::hard_link(&source, &alias).unwrap();
    let result = crate::context::confinement::with_remote_mutation(true, || {
        rename_noreplace(&alias, &destination)
    });
    assert_eq!(
        result.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(fs::read(&source).unwrap(), b"original");
    assert_eq!(fs::read(&alias).unwrap(), b"original");
    assert!(!destination.exists());
}
