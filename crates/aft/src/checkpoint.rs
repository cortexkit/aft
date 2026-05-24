use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::backup::BackupStore;
use crate::error::AftError;

/// Metadata about a checkpoint, returned by list/create/restore.
#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    pub name: String,
    pub file_count: usize,
    pub created_at: u64,
    /// Paths that could not be snapshotted (e.g. deleted since last edit),
    /// paired with the OS-level error that stopped us from reading them.
    /// Empty on successful round-trips. Populated only on `create()` — the
    /// `list()` / `restore()` paths leave it empty.
    pub skipped: Vec<(PathBuf, String)>,
}

/// A stored checkpoint: a snapshot of multiple file contents.
#[derive(Debug, Clone)]
struct Checkpoint {
    name: String,
    file_contents: HashMap<PathBuf, String>,
    created_at: u64,
}

/// Workspace-wide, per-session checkpoint store.
///
/// Partitioned by session (issue #14): two OpenCode sessions sharing one bridge
/// can both create checkpoints named `snap1` without collision, and restoring
/// from one session does not leak the other's file set. Checkpoints are kept
/// in memory only — a bridge crash drops all of them, which is a deliberate
/// trade-off to keep this refactor bounded. Durable checkpoints are a possible
/// follow-up.
#[derive(Debug)]
pub struct CheckpointStore {
    /// session -> name -> checkpoint
    checkpoints: HashMap<String, HashMap<String, Checkpoint>>,
}

impl CheckpointStore {
    pub fn new() -> Self {
        CheckpointStore {
            checkpoints: HashMap::new(),
        }
    }

    /// Create a checkpoint by reading the given files, scoped to `session`.
    ///
    /// If `files` is empty, snapshots all tracked files for **that session**
    /// from the BackupStore (other sessions' tracked files are not visible).
    /// Overwrites any existing checkpoint with the same name in this session.
    ///
    /// Unreadable paths (e.g. deleted since their last edit) are skipped with
    /// a warning instead of failing the whole checkpoint. The paths and their
    /// errors are returned via `CheckpointInfo::skipped` so callers can
    /// surface them. A checkpoint is only rejected outright when *every*
    /// requested path fails — that case still returns a `FileNotFound`
    /// error so callers can distinguish "partial success" from "nothing
    /// snapshotted at all".
    pub fn create(
        &mut self,
        session: &str,
        name: &str,
        files: Vec<PathBuf>,
        backup_store: &BackupStore,
    ) -> Result<CheckpointInfo, AftError> {
        let explicit_request = !files.is_empty();
        let file_list = if files.is_empty() {
            backup_store.tracked_files(session)
        } else {
            files
        };

        let mut file_contents = HashMap::new();
        let mut skipped: Vec<(PathBuf, String)> = Vec::new();
        for path in &file_list {
            match std::fs::read_to_string(path) {
                Ok(content) => {
                    file_contents.insert(path.clone(), content);
                }
                Err(e) => {
                    crate::slog_warn!(
                        "checkpoint {}: skipping unreadable file {}: {}",
                        name,
                        path.display(),
                        e
                    );
                    skipped.push((path.clone(), e.to_string()));
                }
            }
        }

        // If the caller explicitly named a single file and it was unreadable,
        // that's a real error — surface it rather than silently returning an
        // empty checkpoint. For empty `files` (tracked-file fallback) with no
        // readable files at all, the empty-file checkpoint is a legitimate
        // "nothing to snapshot" outcome and we keep it.
        if explicit_request && file_contents.is_empty() && !skipped.is_empty() {
            let (path, err) = &skipped[0];
            return Err(AftError::FileNotFound {
                path: format!("{}: {}", path.display(), err),
            });
        }

        let created_at = current_timestamp();
        let file_count = file_contents.len();

        let checkpoint = Checkpoint {
            name: name.to_string(),
            file_contents,
            created_at,
        };

        self.checkpoints
            .entry(session.to_string())
            .or_default()
            .insert(name.to_string(), checkpoint);

        if skipped.is_empty() {
            crate::slog_info!("checkpoint created: {} ({} files)", name, file_count);
        } else {
            crate::slog_info!(
                "checkpoint created: {} ({} files, {} skipped)",
                name,
                file_count,
                skipped.len()
            );
        }

        Ok(CheckpointInfo {
            name: name.to_string(),
            file_count,
            created_at,
            skipped,
        })
    }

    /// Restore a checkpoint by overwriting files with stored content.
    pub fn restore(&self, session: &str, name: &str) -> Result<CheckpointInfo, AftError> {
        let checkpoint = self.get(session, name)?;
        let mut paths = checkpoint.file_contents.keys().cloned().collect::<Vec<_>>();
        paths.sort();

        restore_paths_atomically(checkpoint, &paths)?;

        crate::slog_info!("checkpoint restored: {}", name);

        Ok(CheckpointInfo {
            name: checkpoint.name.clone(),
            file_count: checkpoint.file_contents.len(),
            created_at: checkpoint.created_at,
            skipped: Vec::new(),
        })
    }

    /// Restore a checkpoint using a caller-validated path list.
    pub fn restore_validated(
        &self,
        session: &str,
        name: &str,
        validated_paths: &[PathBuf],
    ) -> Result<CheckpointInfo, AftError> {
        let checkpoint = self.get(session, name)?;

        for path in validated_paths {
            checkpoint
                .file_contents
                .get(path)
                .ok_or_else(|| AftError::FileNotFound {
                    path: path.display().to_string(),
                })?;
        }
        restore_paths_atomically(checkpoint, validated_paths)?;

        crate::slog_info!("checkpoint restored: {}", name);

        Ok(CheckpointInfo {
            name: checkpoint.name.clone(),
            file_count: checkpoint.file_contents.len(),
            created_at: checkpoint.created_at,
            skipped: Vec::new(),
        })
    }

    /// Return the file paths stored for a checkpoint.
    pub fn file_paths(&self, session: &str, name: &str) -> Result<Vec<PathBuf>, AftError> {
        let checkpoint = self.get(session, name)?;
        Ok(checkpoint.file_contents.keys().cloned().collect())
    }

    /// Delete a checkpoint from a session. Returns true when a checkpoint was removed.
    pub fn delete(&mut self, session: &str, name: &str) -> bool {
        let Some(session_checkpoints) = self.checkpoints.get_mut(session) else {
            return false;
        };
        let removed = session_checkpoints.remove(name).is_some();
        if session_checkpoints.is_empty() {
            self.checkpoints.remove(session);
        }
        removed
    }

    /// List all checkpoints for this session with metadata.
    pub fn list(&self, session: &str) -> Vec<CheckpointInfo> {
        self.checkpoints
            .get(session)
            .map(|s| {
                s.values()
                    .map(|cp| CheckpointInfo {
                        name: cp.name.clone(),
                        file_count: cp.file_contents.len(),
                        created_at: cp.created_at,
                        skipped: Vec::new(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Total checkpoint count across all sessions (for `/aft-status`).
    pub fn total_count(&self) -> usize {
        self.checkpoints.values().map(|s| s.len()).sum()
    }

    /// Remove checkpoints older than `ttl_hours` across all sessions.
    /// Empty session entries are pruned after cleanup.
    pub fn cleanup(&mut self, ttl_hours: u32) {
        let now = current_timestamp();
        let ttl_secs = ttl_hours as u64 * 3600;
        self.checkpoints.retain(|_, session_cps| {
            session_cps.retain(|_, cp| now.saturating_sub(cp.created_at) < ttl_secs);
            !session_cps.is_empty()
        });
    }

    fn get(&self, session: &str, name: &str) -> Result<&Checkpoint, AftError> {
        self.checkpoints
            .get(session)
            .and_then(|s| s.get(name))
            .ok_or_else(|| AftError::CheckpointNotFound {
                name: name.to_string(),
            })
    }
}

fn restore_paths_atomically(checkpoint: &Checkpoint, paths: &[PathBuf]) -> Result<(), AftError> {
    let mut pre_restore_snapshot: HashMap<PathBuf, Option<String>> = HashMap::new();
    for path in paths {
        let current = if path.exists() {
            Some(
                std::fs::read_to_string(path).map_err(|_| AftError::FileNotFound {
                    path: path.display().to_string(),
                })?,
            )
        } else {
            None
        };
        pre_restore_snapshot.insert(path.clone(), current);
    }

    let mut restored_paths: Vec<PathBuf> = Vec::new();
    let mut created_dirs: Vec<PathBuf> = Vec::new();
    for path in paths {
        let content = checkpoint
            .file_contents
            .get(path)
            .ok_or_else(|| AftError::FileNotFound {
                path: path.display().to_string(),
            })?;
        if let Err(e) = write_restored_file(path, content, &mut created_dirs) {
            let mut rollback_errors = Vec::new();
            if let Some(snapshot) = pre_restore_snapshot.get(path) {
                if let Err(rollback_error) = restore_snapshot_file(path, snapshot.as_deref()) {
                    rollback_errors.push(format!("{}: {}", path.display(), rollback_error));
                }
            }
            for restored_path in restored_paths.iter().rev() {
                if let Some(snapshot) = pre_restore_snapshot.get(restored_path) {
                    if let Err(rollback_error) =
                        restore_snapshot_file(restored_path, snapshot.as_deref())
                    {
                        rollback_errors.push(format!(
                            "{}: {}",
                            restored_path.display(),
                            rollback_error
                        ));
                    }
                }
            }
            let dirs_rollback_ok = rollback_created_dirs(&created_dirs);
            if rollback_errors.is_empty() && dirs_rollback_ok {
                return Err(e);
            }
            return Err(AftError::IoError {
                path: path.display().to_string(),
                message: format!(
                    "{}; restore_checkpoint rollback_succeeded: {}; rollback_errors: {}",
                    e,
                    rollback_errors.is_empty() && dirs_rollback_ok,
                    if rollback_errors.is_empty() {
                        "none".to_string()
                    } else {
                        rollback_errors.join("; ")
                    }
                ),
            });
        }
        restored_paths.push(path.clone());
    }

    Ok(())
}

fn restore_snapshot_file(path: &Path, content: Option<&str>) -> Result<(), AftError> {
    match content {
        Some(content) => write_restored_file(path, content, &mut Vec::new()),
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(AftError::FileNotFound {
                path: path.display().to_string(),
            }),
        },
    }
}

fn write_restored_file(
    path: &Path,
    content: &str,
    created_dirs: &mut Vec<PathBuf>,
) -> Result<(), AftError> {
    if let Some(parent) = path.parent() {
        let missing_dirs = missing_parent_dirs(parent);
        std::fs::create_dir_all(parent).map_err(|_| AftError::FileNotFound {
            path: path.display().to_string(),
        })?;
        created_dirs.extend(missing_dirs);
    }
    std::fs::write(path, content).map_err(|_| AftError::FileNotFound {
        path: path.display().to_string(),
    })
}

fn missing_parent_dirs(parent: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = Some(parent);

    while let Some(dir) = current {
        if dir.as_os_str().is_empty() || dir.exists() {
            break;
        }
        dirs.push(dir.to_path_buf());
        current = dir.parent();
    }

    dirs
}

fn rollback_created_dirs(dirs: &[PathBuf]) -> bool {
    let mut dirs = dirs.to_vec();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    dirs.dedup();

    let mut ok = true;
    for dir in dirs {
        match std::fs::remove_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => ok = false,
        }
    }
    ok
}

fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DEFAULT_SESSION_ID;
    use std::fs;

    fn temp_file(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("aft_checkpoint_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn create_and_restore_round_trip() {
        let path1 = temp_file("cp_rt1.txt", "hello");
        let path2 = temp_file("cp_rt2.txt", "world");

        let backup_store = BackupStore::new();
        let mut store = CheckpointStore::new();

        let info = store
            .create(
                DEFAULT_SESSION_ID,
                "snap1",
                vec![path1.clone(), path2.clone()],
                &backup_store,
            )
            .unwrap();
        assert_eq!(info.name, "snap1");
        assert_eq!(info.file_count, 2);

        // Modify files
        fs::write(&path1, "changed1").unwrap();
        fs::write(&path2, "changed2").unwrap();

        // Restore
        let info = store.restore(DEFAULT_SESSION_ID, "snap1").unwrap();
        assert_eq!(info.file_count, 2);
        assert_eq!(fs::read_to_string(&path1).unwrap(), "hello");
        assert_eq!(fs::read_to_string(&path2).unwrap(), "world");
    }

    #[test]
    fn overwrite_existing_name() {
        let path = temp_file("cp_overwrite.txt", "v1");
        let backup_store = BackupStore::new();
        let mut store = CheckpointStore::new();

        store
            .create(DEFAULT_SESSION_ID, "dup", vec![path.clone()], &backup_store)
            .unwrap();
        fs::write(&path, "v2").unwrap();
        store
            .create(DEFAULT_SESSION_ID, "dup", vec![path.clone()], &backup_store)
            .unwrap();

        // Restore should give v2 (the overwritten checkpoint)
        fs::write(&path, "v3").unwrap();
        store.restore(DEFAULT_SESSION_ID, "dup").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2");
    }

    #[test]
    fn list_returns_metadata_scoped_to_session() {
        let path = temp_file("cp_list.txt", "data");
        let backup_store = BackupStore::new();
        let mut store = CheckpointStore::new();

        store
            .create(DEFAULT_SESSION_ID, "a", vec![path.clone()], &backup_store)
            .unwrap();
        store
            .create(DEFAULT_SESSION_ID, "b", vec![path.clone()], &backup_store)
            .unwrap();
        store
            .create("other_session", "c", vec![path.clone()], &backup_store)
            .unwrap();

        let default_list = store.list(DEFAULT_SESSION_ID);
        assert_eq!(default_list.len(), 2);
        let names: Vec<&str> = default_list.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));

        let other_list = store.list("other_session");
        assert_eq!(other_list.len(), 1);
        assert_eq!(other_list[0].name, "c");
    }

    #[test]
    fn sessions_isolate_checkpoint_names() {
        // Same checkpoint name in two sessions does not collide on restore.
        let path_a = temp_file("cp_isolated_a.txt", "a-original");
        let path_b = temp_file("cp_isolated_b.txt", "b-original");
        let backup_store = BackupStore::new();
        let mut store = CheckpointStore::new();

        // Both sessions create a checkpoint with the same name but different files.
        store
            .create("session_a", "snap", vec![path_a.clone()], &backup_store)
            .unwrap();
        store
            .create("session_b", "snap", vec![path_b.clone()], &backup_store)
            .unwrap();

        fs::write(&path_a, "a-modified").unwrap();
        fs::write(&path_b, "b-modified").unwrap();

        // Restoring session A's "snap" only touches path_a.
        store.restore("session_a", "snap").unwrap();
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "a-original");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "b-modified");

        // Restoring session B's "snap" only touches path_b.
        fs::write(&path_a, "a-modified").unwrap();
        store.restore("session_b", "snap").unwrap();
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "a-modified");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "b-original");
    }

    #[test]
    fn cleanup_removes_expired_across_sessions() {
        let path = temp_file("cp_cleanup.txt", "data");
        let backup_store = BackupStore::new();
        let mut store = CheckpointStore::new();

        store
            .create(
                DEFAULT_SESSION_ID,
                "recent",
                vec![path.clone()],
                &backup_store,
            )
            .unwrap();

        // Manually insert an expired checkpoint in another session.
        store
            .checkpoints
            .entry("other".to_string())
            .or_default()
            .insert(
                "old".to_string(),
                Checkpoint {
                    name: "old".to_string(),
                    file_contents: HashMap::new(),
                    created_at: 1000, // far in the past
                },
            );

        assert_eq!(store.total_count(), 2);
        store.cleanup(24); // 24 hours
        assert_eq!(store.total_count(), 1);
        assert_eq!(store.list(DEFAULT_SESSION_ID)[0].name, "recent");
        assert!(store.list("other").is_empty());
    }

    #[test]
    fn restore_nonexistent_returns_error() {
        let store = CheckpointStore::new();
        let result = store.restore(DEFAULT_SESSION_ID, "nope");
        assert!(result.is_err());
        match result.unwrap_err() {
            AftError::CheckpointNotFound { name } => {
                assert_eq!(name, "nope");
            }
            other => panic!("expected CheckpointNotFound, got: {:?}", other),
        }
    }

    #[test]
    fn restore_nonexistent_in_other_session_returns_error() {
        // A "snap" that exists in session A must NOT be visible from session B.
        let path = temp_file("cp_cross_session.txt", "data");
        let backup_store = BackupStore::new();
        let mut store = CheckpointStore::new();
        store
            .create("session_a", "only_a", vec![path], &backup_store)
            .unwrap();
        assert!(store.restore("session_b", "only_a").is_err());
    }

    #[test]
    fn create_skips_missing_files_from_backup_tracked_set() {
        // Simulate the reported issue #15-follow-up: an agent deletes a
        // previously-edited file, then calls checkpoint with no explicit
        // file list. Before the fix, the stale backup-tracked entry caused
        // the whole checkpoint to fail on the missing path. Now the checkpoint
        // succeeds with the readable file and reports the skipped one.
        let readable = temp_file("cp_skip_readable.txt", "still_here");
        let deleted = temp_file("cp_skip_deleted.txt", "about_to_vanish");

        // Backup store canonicalizes keys, so the skipped path in the
        // checkpoint result is the canonical form, not the raw temp path.
        let deleted_canonical = fs::canonicalize(&deleted).unwrap();

        let mut backup_store = BackupStore::new();
        backup_store
            .snapshot(DEFAULT_SESSION_ID, &readable, "auto")
            .unwrap();
        backup_store
            .snapshot(DEFAULT_SESSION_ID, &deleted, "auto")
            .unwrap();

        fs::remove_file(&deleted).unwrap();

        let mut store = CheckpointStore::new();
        let info = store
            .create(DEFAULT_SESSION_ID, "partial", vec![], &backup_store)
            .expect("checkpoint should succeed despite one missing file");
        assert_eq!(info.file_count, 1);
        assert_eq!(info.skipped.len(), 1);
        assert_eq!(info.skipped[0].0, deleted_canonical);
        assert!(!info.skipped[0].1.is_empty());
    }

    #[test]
    fn create_with_explicit_single_missing_file_errors() {
        // When the caller names a single file explicitly and it can't be read,
        // fail loudly — an empty checkpoint isn't what the caller asked for.
        let missing = std::env::temp_dir()
            .join("aft_checkpoint_tests/cp_explicit_missing_does_not_exist.txt");
        let _ = fs::remove_file(&missing);

        let backup_store = BackupStore::new();
        let mut store = CheckpointStore::new();
        let result = store.create(
            DEFAULT_SESSION_ID,
            "explicit",
            vec![missing.clone()],
            &backup_store,
        );

        assert!(result.is_err());
        match result.unwrap_err() {
            AftError::FileNotFound { path } => {
                assert!(path.contains(&missing.display().to_string()));
            }
            other => panic!("expected FileNotFound, got: {:?}", other),
        }
    }

    #[test]
    fn create_with_explicit_mixed_files_keeps_readable_and_reports_skipped() {
        // Explicit file list with one readable + one missing: keep the
        // readable one in the checkpoint, report the missing one under
        // `skipped` instead of failing outright.
        let good = temp_file("cp_mixed_good.txt", "ok");
        let missing = std::env::temp_dir().join("aft_checkpoint_tests/cp_mixed_missing.txt");
        let _ = fs::remove_file(&missing);

        let backup_store = BackupStore::new();
        let mut store = CheckpointStore::new();
        let info = store
            .create(
                DEFAULT_SESSION_ID,
                "mixed",
                vec![good.clone(), missing.clone()],
                &backup_store,
            )
            .expect("mixed checkpoint should succeed when any file is readable");
        assert_eq!(info.file_count, 1);
        assert_eq!(info.skipped.len(), 1);
        assert_eq!(info.skipped[0].0, missing);
    }

    #[test]
    fn create_with_empty_files_uses_backup_tracked() {
        let path = temp_file("cp_tracked.txt", "tracked_content");
        let mut backup_store = BackupStore::new();
        backup_store
            .snapshot(DEFAULT_SESSION_ID, &path, "auto")
            .unwrap();

        let mut store = CheckpointStore::new();
        let info = store
            .create(DEFAULT_SESSION_ID, "from_tracked", vec![], &backup_store)
            .unwrap();
        assert!(info.file_count >= 1);

        // Modify and restore
        fs::write(&path, "modified").unwrap();
        store.restore(DEFAULT_SESSION_ID, "from_tracked").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "tracked_content");
    }

    #[test]
    fn restore_recreates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("file.txt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "original nested content").unwrap();

        let backup_store = BackupStore::new();
        let mut store = CheckpointStore::new();
        store
            .create(
                DEFAULT_SESSION_ID,
                "nested",
                vec![path.clone()],
                &backup_store,
            )
            .unwrap();

        fs::remove_dir_all(dir.path().join("nested")).unwrap();

        store.restore(DEFAULT_SESSION_ID, "nested").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "original nested content"
        );
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_restore_rolls_back_on_partial_failure() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.txt");
        let path_b = dir.path().join("b.txt");
        fs::write(&path_a, "checkpoint-a").unwrap();
        fs::write(&path_b, "checkpoint-b").unwrap();

        let backup_store = BackupStore::new();
        let mut store = CheckpointStore::new();
        store
            .create(
                DEFAULT_SESSION_ID,
                "partial_failure",
                vec![path_a.clone(), path_b.clone()],
                &backup_store,
            )
            .unwrap();

        fs::write(&path_a, "pre-restore-a").unwrap();
        fs::write(&path_b, "pre-restore-b").unwrap();
        let mut readonly = fs::metadata(&path_b).unwrap().permissions();
        readonly.set_mode(0o444);
        fs::set_permissions(&path_b, readonly).unwrap();

        let result = store.restore(DEFAULT_SESSION_ID, "partial_failure");
        let mut writable = fs::metadata(&path_b).unwrap().permissions();
        writable.set_mode(0o644);
        fs::set_permissions(&path_b, writable).unwrap();

        assert!(result.is_err(), "restore should surface write failure");
        assert_eq!(fs::read_to_string(&path_a).unwrap(), "pre-restore-a");
        assert_eq!(fs::read_to_string(&path_b).unwrap(), "pre-restore-b");
    }

    #[test]
    fn checkpoint_restore_failure_removes_created_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let missing_root = dir.path().join("created");
        let path_a = missing_root.join("nested").join("a.txt");
        let path_b = dir.path().join("blocking-dir");
        fs::create_dir(&path_b).unwrap();

        let checkpoint = Checkpoint {
            name: "dir-cleanup".to_string(),
            file_contents: HashMap::from([
                (path_a.clone(), "checkpoint-a".to_string()),
                (path_b.clone(), "checkpoint-b".to_string()),
            ]),
            created_at: current_timestamp(),
        };

        let result = restore_paths_atomically(&checkpoint, &[path_a.clone(), path_b.clone()]);

        assert!(
            result.is_err(),
            "second restore write should fail on directory"
        );
        assert!(!path_a.exists(), "restored file should be rolled back");
        assert!(
            !missing_root.exists(),
            "new parent directories should be removed on rollback"
        );
        assert!(path_b.is_dir(), "pre-existing blocking directory remains");
    }
}
