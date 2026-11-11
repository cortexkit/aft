use super::*;
use crate::bash_background::persistence::{DirectorySnapshot, PinnedDir, TaskLayout};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;

const TASK_READ_RECORD_CAPACITY: usize = 64;

pub(super) struct TaskReadRecord {
    paths: TaskPaths,
    directories: [DirectorySnapshot; 3],
    metadata: PersistedTask,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash_background::persistence::{create_task_layout, write_task};

    fn record(registry: &BgTaskRegistry, storage: &Path, index: usize) -> TaskPaths {
        let task_id = format!("bash-{index:016x}");
        let paths = create_task_layout(storage, "session", &task_id)
            .unwrap()
            .paths;
        let mut metadata = PersistedTask::starting(
            task_id,
            "session".into(),
            "echo output".into(),
            storage.to_path_buf(),
            Some(storage.to_path_buf()),
            None,
            false,
            false,
        );
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        metadata.completion_delivered = true;
        write_task(&paths.json, &metadata).unwrap();
        fs::write(&paths.stdout, "output\n").unwrap();
        assert!(registry.is_readable_task_path("session", &paths.stdout));
        paths
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finished_and_recovered_tasks_keep_descriptor_count_bounded() {
        // Isolate /proc/self/fd from other tests opening files concurrently.
        const CHILD: &str = "AFT_TASK_DESCRIPTOR_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "bash_background::registry::task_access::tests::finished_and_recovered_tasks_keep_descriptor_count_bounded",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "descriptor test failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            print!("{}", String::from_utf8_lossy(&output.stdout));
            return;
        }
        let count = || fs::read_dir("/proc/self/fd").unwrap().count();
        let registry = BgTaskRegistry::default();
        let storage = tempfile::tempdir().unwrap();
        let baseline = count();
        let mut peak = baseline;
        for index in 0..520 {
            let paths = record(&registry, storage.path(), index);
            assert!(registry.is_session_owned_artifact_path("session", &paths.stdout));
            peak = peak.max(count());
            assert!(
                count() <= baseline + 16,
                "finished task {index}: descriptor count {} exceeds baseline {baseline} + 16",
                count()
            );
        }
        registry.replay_session(storage.path(), "session").unwrap();
        assert_eq!(registry.inner.tasks.lock().unwrap().len(), 520);
        for task in registry.task_arcs() {
            // Model the process monitor acquired while an adopted task was
            // running, then exercise the same terminal release as the watchdog.
            assert!(!task
                .adopted_exit
                .lock()
                .unwrap()
                .marker_due(Some(std::process::id()), Instant::now()));
            assert!(release_terminal_io_handles(&task));
        }
        let after_replay = count();
        assert!(
            after_replay <= baseline + 16,
            "replay retained task descriptors: {after_replay}, baseline {baseline}"
        );
        println!(
            "520 tasks: baseline={baseline}, finished_peak={peak}, after_replay={after_replay}"
        );
    }

    #[test]
    fn cached_artifact_ownership_rejects_foreign_sessions() {
        let registry = BgTaskRegistry::default();
        let storage = tempfile::tempdir().unwrap();
        let paths = record(&registry, storage.path(), 0);
        assert!(registry.is_session_owned_artifact_path("session", &paths.stdout));
        assert!(!registry.is_session_owned_artifact_path("foreign", &paths.stdout));
    }

    #[test]
    fn cached_artifact_ownership_rejects_replaced_io_directory() {
        let registry = BgTaskRegistry::default();
        let storage = tempfile::tempdir().unwrap();
        let paths = record(&registry, storage.path(), 0);
        assert!(registry.is_session_owned_artifact_path("session", &paths.stdout));
        fs::rename(&paths.io_dir, paths.dir.join("old-io")).unwrap();
        fs::create_dir(&paths.io_dir).unwrap();
        fs::write(&paths.stdout, "replacement output").unwrap();
        assert!(!registry.is_session_owned_artifact_path("session", &paths.stdout));
    }

    #[test]
    fn cached_task_reads_reject_replaced_directories() {
        for name in ["task", "control", "io"] {
            let registry = BgTaskRegistry::default();
            let storage = tempfile::tempdir().unwrap();
            let paths = record(&registry, storage.path(), 0);
            let path = match name {
                "task" => &paths.dir,
                "control" => &paths.control_dir,
                _ => &paths.io_dir,
            };
            fs::rename(path, path.with_extension("old")).unwrap();
            fs::create_dir(path).unwrap();
            assert!(registry.task_read_record(&paths.dir).is_none(), "{name}");
        }
    }

    #[test]
    fn task_read_cache_is_bounded_and_eviction_releases_records() {
        let registry = BgTaskRegistry::default();
        let storage = tempfile::tempdir().unwrap();
        let first = record(&registry, storage.path(), 0);
        let first_record = Arc::downgrade(&registry.task_read_record(&first.dir).unwrap());
        for index in 1..192 {
            record(&registry, storage.path(), index);
            assert!(
                registry.inner.task_read_records.lock().unwrap().len() <= 64,
                "the task-read cache must hold at most 64 records"
            );
        }
        assert!(
            first_record.upgrade().is_none(),
            "eviction must release the cached record"
        );
        assert!(registry.is_readable_task_path("session", &first.stdout));
    }

    #[cfg(unix)]
    #[test]
    fn task_read_cache_identity_failure_releases_records() {
        let registry = BgTaskRegistry::default();
        let storage = tempfile::tempdir().unwrap();
        let paths = record(&registry, storage.path(), 0);
        let cached = Arc::downgrade(&registry.task_read_record(&paths.dir).unwrap());
        fs::rename(&paths.dir, paths.dir.with_extension("old")).unwrap();
        assert!(registry.task_read_record(&paths.dir).is_none());
        assert!(
            cached.upgrade().is_none(),
            "a stale record must leave the cache"
        );
        assert!(registry.inner.task_read_records.lock().unwrap().is_empty());
    }

    #[test]
    fn task_read_cache_cleanup_releases_deleted_task_records() {
        let registry = BgTaskRegistry::default();
        let storage = tempfile::tempdir().unwrap();
        let paths = record(&registry, storage.path(), 0);
        let cached = Arc::downgrade(&registry.task_read_record(&paths.dir).unwrap());
        registry.replay_session(storage.path(), "session").unwrap();
        registry.cleanup_finished(Duration::ZERO);
        assert!(
            !paths.dir.exists(),
            "cleanup must delete the expired bundle"
        );
        assert!(
            cached.upgrade().is_none(),
            "cleanup must release the cached record"
        );
        assert!(registry.inner.task_read_records.lock().unwrap().is_empty());
    }

    #[test]
    fn task_read_cache_retention_releases_deleted_task_records() {
        let registry = BgTaskRegistry::default();
        let storage = tempfile::tempdir().unwrap();
        let paths = record(&registry, storage.path(), 0);
        let cached = Arc::downgrade(&registry.task_read_record(&paths.dir).unwrap());
        let old = std::time::SystemTime::now() - Duration::from_secs(25 * 60 * 60);
        filetime::set_file_mtime(&paths.json, filetime::FileTime::from_system_time(old)).unwrap();
        assert_eq!(registry.maybe_gc_persisted(storage.path()).unwrap(), 1);
        assert!(
            !paths.dir.exists(),
            "retention must delete the expired bundle"
        );
        assert!(
            cached.upgrade().is_none(),
            "retention must release the cached record"
        );
        assert!(registry.inner.task_read_records.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct WorkCounters {
    owner_lookups: AtomicUsize,
    namespace_reads: AtomicUsize,
    task_folder_reads: AtomicUsize,
}

use crate::task_temp_reads::{is_task_environment, task_directory};

/// Recursive readers must not bypass the explicit-path check by entering a
/// bundle through its directory. Do not follow links or credential payloads.
pub(crate) fn safe_task_entry(path: &Path) -> bool {
    if task_directory(path).is_none() {
        return true;
    }
    if crate::project_content::is_sqlite_or_metadata(path) {
        return false;
    }
    if is_task_environment(path) {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
        return false;
    }
    if metadata.is_file() {
        #[cfg(test)]
        crate::project_content::record_open(path);
        return path
            .parent()
            .and_then(|parent| PinnedDir::open(parent).ok())
            .is_some_and(|parent| {
                path.file_name()
                    .is_some_and(|name| parent.open_file(name, false).is_ok())
            });
    }
    true
}

/// A second hard link exposes the same task output through another path.
pub(super) fn has_multiple_hard_links(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() > 1
    }
    #[cfg(windows)]
    {
        // Windows link-count metadata is unstable; handle-based artifact reads
        // retain validate_regular_handle's existing Windows check.
        let _ = metadata;
        false
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        false
    }
}

impl BgTaskRegistry {
    /// Expand only an explicitly named bundle directory or registered file,
    /// never a storage ancestor. Recheck session/head access for each exact name.
    pub(crate) fn explicit_task_artifacts(
        &self,
        session: &str,
        path: &Path,
    ) -> Option<Vec<PathBuf>> {
        // Exact owned outputs include legacy flat-layout tasks, which have no
        // bundle read record. Ownership bypasses storage exclusion, not SQLite
        // or link checks, and never authorizes the containing directory.
        if self.is_session_owned_artifact_path(session, path) {
            let readable = !crate::project_content::is_sqlite_or_metadata(path)
                && fs::symlink_metadata(path).is_ok_and(|metadata| {
                    metadata.is_file() && !has_multiple_hard_links(&metadata)
                });
            return Some(if readable {
                vec![path.to_path_buf()]
            } else {
                Vec::new()
            });
        }
        let record = self.task_read_record(task_directory(path)?)?;
        let paths = &record.paths;
        if ![&paths.dir, &paths.control_dir, &paths.io_dir]
            .into_iter()
            .any(|directory| path == directory)
            && !paths.readable_files().any(|file| path == file)
        {
            return None;
        }
        Some(
            paths
                .readable_files()
                .filter(|file| file.starts_with(path))
                .filter(|file| self.is_readable_task_path(session, file))
                .collect(),
        )
    }

    pub(super) fn task_namespaces(&self, storage: &Path) -> std::io::Result<fs::ReadDir> {
        #[cfg(test)]
        self.inner
            .task_access_work
            .namespace_reads
            .fetch_add(1, Ordering::Relaxed);
        fs::read_dir(storage.join("bash-tasks"))
    }

    #[cfg(test)]
    pub(crate) fn reset_task_access_work(&self) {
        self.inner
            .task_access_work
            .owner_lookups
            .store(0, Ordering::Relaxed);
        self.inner
            .task_access_work
            .namespace_reads
            .store(0, Ordering::Relaxed);
        self.inner
            .task_access_work
            .task_folder_reads
            .store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn task_access_work(&self) -> (usize, usize, usize) {
        let work = &self.inner.task_access_work;
        (
            work.owner_lookups.load(Ordering::Relaxed),
            work.namespace_reads.load(Ordering::Relaxed),
            work.task_folder_reads.load(Ordering::Relaxed),
        )
    }

    fn task_read_record(&self, root: &Path) -> Option<Arc<TaskReadRecord>> {
        {
            let mut records = self.inner.task_read_records.lock().ok()?;
            if let Some(index) = records.iter().position(|(path, _)| path == root) {
                // Take the record out first so a failed identity check also
                // discards the stale permission record.
                let (path, record) = records.remove(index)?;
                for directory in &record.directories {
                    directory.reopen().ok()?;
                }
                records.push_back((path, Arc::clone(&record)));
                return Some(record);
            }
        }
        #[cfg(test)]
        self.inner
            .task_access_work
            .task_folder_reads
            .fetch_add(1, Ordering::Relaxed);
        let resolved = resolve_task_layout(root.parent()?, root.file_name()?.to_str()?).ok()?;
        if resolved.paths.layout != TaskLayout::Directory {
            return None;
        }
        let metadata = read_task_at(&resolved).ok()?;
        if root.parent()?.file_name()?.to_str()?
            != crate::backup::hash_session(&metadata.session_id)
        {
            return None;
        }
        let record = Arc::new(TaskReadRecord {
            paths: resolved.paths,
            directories: [
                resolved.dirs.task.snapshot().ok()?,
                resolved.dirs.control.snapshot().ok()?,
                resolved.dirs.io.snapshot().ok()?,
            ],
            metadata,
        });
        let mut records = self.inner.task_read_records.lock().ok()?;
        if let Some((_, existing)) = records.iter().find(|(path, _)| path == root) {
            return Some(Arc::clone(existing));
        }
        // Keep only a small working set of permission metadata. Directory
        // snapshots carry no handles and are revalidated on each cache hit.
        if records.len() >= TASK_READ_RECORD_CAPACITY {
            records.pop_front();
        }
        records.push_back((root.to_path_buf(), Arc::clone(&record)));
        Some(record)
    }

    pub(super) fn forget_task_read_record(&self, root: &Path) {
        if let Ok(mut records) = self.inner.task_read_records.lock() {
            records.retain(|(path, _)| path != root);
        }
    }

    /// Scratch ownership comes only from a task still held by this registry.
    /// The persisted sandbox path is recorded from the spawn plan, not inferred
    /// from the scratch directory's name or its caller-writable contents.
    pub(crate) fn is_session_owned_scratch_path(&self, session: &str, path: &Path) -> bool {
        let Ok(requested) = fs::canonicalize(path) else {
            return false;
        };
        if !crate::task_temp_reads::is_scratch(&requested) {
            return false;
        }
        self.task_arcs().iter().any(|task| {
            if task.session_id != session {
                return false;
            }
            let root = task
                .lock_state()
                .ok()
                .and_then(|state| state.metadata.sandbox_temp_dir.clone());
            let Some(root) = root else {
                return false;
            };
            // Do not canonicalize the recorded root into a replacement symlink.
            // The allocator records an absolute, canonical directory at spawn.
            crate::sandbox_spawn::is_managed_task_temp_dir(&root)
                && requested.starts_with(&root)
                && fs::canonicalize(&root).is_ok_and(|current| current == root)
                && !crate::project_content::is_sqlite_or_metadata(&requested)
                && fs::symlink_metadata(&requested).is_ok_and(|metadata| {
                    metadata.is_dir() || metadata.is_file() && !has_multiple_hard_links(&metadata)
                })
        })
    }

    /// A task's scratch directory is deleted when its command exits, so a
    /// session that wrote a file there in one shell call and reads it in a
    /// later tool call names a path that is gone. Recognize a missing path
    /// under a scratch root that one of this session's tasks records now or
    /// recorded before cleanup, so the read can fail as not-found rather than
    /// ask for permission to read nothing. `identity` is the policy identity:
    /// absolute, with existing ancestors canonicalized and `..` removed.
    pub(crate) fn is_session_missing_scratch_path(&self, session: &str, identity: &Path) -> bool {
        if !identity.is_absolute()
            || identity.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            || !crate::task_temp_reads::is_scratch(identity)
            || !matches!(fs::symlink_metadata(identity), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            return false;
        }
        let root_missing = |root: &Path| matches!(fs::symlink_metadata(root), Err(error) if error.kind() == std::io::ErrorKind::NotFound);
        self.task_arcs().iter().any(|task| {
            if task.session_id != session {
                return false;
            }
            let Ok(state) = task.lock_state() else {
                return false;
            };
            let under = |root: &Path| {
                crate::sandbox_spawn::is_managed_task_temp_dir(root) && identity.starts_with(root)
            };
            // The recorded root may still exist (a missing file inside a live
            // scratch) as long as it has not been replaced by a link. A root
            // remembered only after cleanup must itself be gone: if the same
            // name exists again, it belongs to some other task.
            let live = state
                .metadata
                .sandbox_temp_dir
                .as_deref()
                .is_some_and(|root| {
                    under(root)
                        && (root_missing(root)
                            || fs::canonicalize(root).is_ok_and(|current| current == root))
                });
            live || state
                .removed_temp_dir
                .as_deref()
                .is_some_and(|root| under(root) && root_missing(root))
        })
    }

    fn head_reads_record(&self, record: &TaskReadRecord) -> bool {
        let Some(caller) = crate::access_policy::current_scope() else {
            return false;
        };
        let Some(scope) = record.metadata.scope.as_ref() else {
            return false;
        };
        if !scope.owner_authorized || scope.kind != subc_protocol::scope::ScopeKind::Worker {
            return false;
        }
        let Some(parent) = scope.parent.as_ref() else {
            return false;
        };
        // A revived worker has a new session and epoch, but its parent
        // scope's owner and reference still identify the owning head.
        if caller.owner != serde_json::to_string(&parent.owner).expect("scope owner serializes")
            || caller.scope_ref != parent.scope_ref
        {
            return false;
        }
        // Scope end or replacement of the head registration must take effect
        // on the next read. Recheck the indexed database row, not the folders.
        self.inner
            .db_pool
            .read()
            .ok()
            .and_then(|pool| pool.clone())
            .and_then(|pool| {
                pool.lock()
                    .ok()
                    .and_then(|conn| crate::db::access_grants::is_head(&conn, &caller).ok())
            })
            .unwrap_or(false)
    }

    /// The read-only exception is identity-checked per bundle, never per file.
    /// Bash's separate output-file exception remains exact-file-only: permission
    /// to inspect a task folder must not let a shell command enter that folder.
    pub(crate) fn is_readable_task_path(&self, session: &str, path: &Path) -> bool {
        if !safe_task_entry(path) {
            return false;
        }
        let Some(root) = task_directory(path) else {
            return false;
        };
        let Some(record) = self.task_read_record(root) else {
            return false;
        };
        let own_io =
            record.metadata.session_id == session && path.starts_with(&record.paths.io_dir);
        if !own_io && !self.head_reads_record(&record) {
            return false;
        }
        if path.is_dir() {
            return PinnedDir::open(path).is_ok();
        }
        let Ok(parent) = PinnedDir::open(path.parent().unwrap_or(root)) else {
            return false;
        };
        path.file_name()
            .is_some_and(|name| parent.open_file(name, false).is_ok())
    }

    pub(crate) fn readable_worker_task_owner(
        &self,
        task_id: &str,
        storage: &Path,
    ) -> Option<String> {
        #[cfg(test)]
        self.inner
            .task_access_work
            .owner_lookups
            .fetch_add(1, Ordering::Relaxed);
        validate_task_id(task_id).ok()?;
        let caller = crate::access_policy::current_scope()?;
        let pool = self.inner.db_pool.read().ok()?.clone()?;
        {
            let conn = pool.lock().ok()?;
            if !crate::db::access_grants::is_head(&conn, &caller).ok()? {
                return None;
            }
        }
        let session = if let Some(task) = self.task(task_id) {
            task.session_id.clone()
        } else {
            let harness = crate::bash_background::route_harness()
                .map(|harness| harness.storage_segment())
                .or_else(|| self.fallback_db_harness())?;
            let conn = pool.lock().ok()?;
            crate::db::bash_tasks::find_bash_task_session(&conn, &harness, task_id).ok()??
        };
        // The row or live task names one namespace, including an earlier worker
        // session after revival. A missing row never causes a directory sweep.
        let root = session_tasks_dir(storage, &session).join(task_id);
        let record = self.task_read_record(&root)?;
        self.head_reads_record(&record).then_some(session)
    }
}
