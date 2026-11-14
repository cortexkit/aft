//! Launcher images have process lifetimes, not output-retention lifetimes.
use super::*;
use crate::pinned_executable::TASK_IMAGE_DIR;

#[derive(Default)]
pub(super) struct ImageCleanupPass {
    pub removed: usize,
    pub remaining: usize,
}

impl ImageCleanupPass {
    pub(super) fn record(&mut self, result: Self) {
        self.removed += result.removed;
        self.remaining += result.remaining;
    }

    pub(super) fn report(&self) {
        if self.removed + self.remaining > 0 {
            log::debug!(
                "task startup-image cleanup: removed {}, {} remain among images inspected in this bounded pass",
                self.removed,
                self.remaining
            );
        }
    }
}

fn image_present(io: &Path) -> bool {
    #[cfg(test)]
    super::super::persistence::work_counts::record_stat();
    !matches!(
        fs::symlink_metadata(io.join(TASK_IMAGE_DIR)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

fn quiescent(metadata: &PersistedTask) -> bool {
    if !metadata.is_terminal()
        || metadata.group_member_identities_truncated
        || BgTaskRegistry::persisted_task_process_is_alive(metadata)
    {
        return false;
    }
    let Some(pgid) = metadata.pgid else {
        return metadata.child_pid.is_none()
            && metadata.group_member_identities.is_empty()
            && metadata.live_descendants.as_ref().is_none_or(Vec::is_empty)
            && metadata.live_descendants_omitted == 0;
    };
    // Windows cannot enumerate task descendants. A dead shell alone is not
    // proof they exited, including after replay; keep its image until durable
    // Windows job/descendant tracking can establish that the whole task ended.
    let Ok(listing) = live_process_group_members(pgid) else {
        return false;
    };
    if !listing.is_clean() {
        return false;
    }
    // A sample can name a survivor whose start time was unreadable. Without
    // an identity to rule out PID reuse, a still-live PID must retain the image.
    if metadata.live_descendants.as_ref().is_some_and(|members| {
        members.iter().any(|member| {
            !metadata
                .group_member_identities
                .iter()
                .any(|identity| identity.pid == member.pid)
                && super::super::process::is_process_alive(member.pid)
        })
    }) {
        return false;
    }
    // A saved member may have left the original group. PID reuse is not a
    // survivor, but an unreadable start time or group cannot prove its exit.
    !metadata.group_member_identities.iter().any(|identity| {
        super::super::process::is_process_alive(identity.pid)
            && crate::root_cache::process_start_time_ms(identity.pid)
                .is_none_or(|start| start == identity.started_at_ms)
            && !identity.is_running_in_group(pgid)
    })
}

fn cleanup_image(
    paths: &TaskPaths,
    metadata: &PersistedTask,
    permitted: impl FnOnce() -> bool,
) -> ImageCleanupPass {
    if !image_present(&paths.io_dir) {
        return ImageCleanupPass::default();
    }
    let mut result = ImageCleanupPass {
        removed: 0,
        remaining: 1,
    };
    if !quiescent(metadata) || !permitted() {
        return result;
    }
    let removal = (|| {
        // Use the same pinned, no-follow directory operations as bundle
        // cleanup: a child-writable IO pathname must not redirect deletion.
        let dirs = super::super::persistence::repin_task_io(paths)?;
        let name = std::ffi::OsStr::new(TASK_IMAGE_DIR);
        let image = dirs.io.repair_child(name)?;
        let identity = image.directory_identity()?;
        drop(image);
        let mut warnings = Vec::new();
        let mut removed_entries = 0;
        let removal = dirs.io.repair_remove_child_with_warnings(
            name,
            identity,
            &[],
            &mut removed_entries,
            &mut warnings,
        );
        for warning in warnings {
            log::debug!("task startup-image cleanup: {warning}");
        }
        removal
    })();
    match removal {
        Ok(()) => {
            result.removed = 1;
            result.remaining = 0;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            result.remaining = 0;
        }
        Err(error) => log::debug!(
            "failed to remove task startup image for {}: {error}",
            metadata.task_id
        ),
    }
    result
}

impl BgTaskRegistry {
    pub(super) fn cleanup_terminal_task_image(&self, task: &BgTask) -> ImageCleanupPass {
        #[cfg(unix)]
        let probe_running = self
            .db_hint_job(&task.task_id)
            .is_some_and(|job| !job.is_finished());
        #[cfg(not(unix))]
        let probe_running = false;
        let Ok(state) = task.lock_state() else {
            return ImageCleanupPass::default();
        };
        cleanup_image(&task.paths, &state.metadata, || {
            !state.kill_in_flight && !state.descendant_sample_pending && !probe_running
        })
    }

    pub(super) fn cleanup_saved_task_image(
        &self,
        session: &Path,
        task_id: &str,
        permitted: impl FnOnce() -> bool,
    ) -> ImageCleanupPass {
        // Images were introduced with directory-layout tasks. Probe before
        // opening metadata so already-cleaned tasks need no extra record reads,
        // even while their output bundles are inside the retention grace.
        if !image_present(&session.join(task_id).join("io")) {
            return ImageCleanupPass::default();
        }
        let Ok(task) = resolve_task_layout(session, task_id) else {
            return ImageCleanupPass {
                removed: 0,
                remaining: 1,
            };
        };
        let Ok(metadata) = read_task_at(&task) else {
            return ImageCleanupPass {
                removed: 0,
                remaining: 1,
            };
        };
        cleanup_image(&task.paths, &metadata, || {
            !task_db_write_order_is_live(&metadata.session_id, task_id) && permitted()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::persistence::{create_task_layout, write_task_at, ResolvedTask};
    use super::*;

    fn saved_image(storage: &Path, index: usize) -> (ResolvedTask, PersistedTask) {
        let task = create_task_layout(storage, "images", &format!("bash-{index:016x}")).unwrap();
        let mut metadata = PersistedTask::starting(
            task.paths.task_id.clone(),
            "images".into(),
            "echo result".into(),
            storage.into(),
            Some(storage.into()),
            None,
            true,
            false,
        );
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        metadata.finished_at = Some(metadata.started_at.saturating_sub(60 * 60 * 1000));
        write_task_at(&task, &metadata).unwrap();
        let image = task.paths.io_dir.join(TASK_IMAGE_DIR);
        fs::create_dir(&image).unwrap();
        fs::write(image.join("launcher"), b"retained image").unwrap();
        fs::create_dir(image.join("shims")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("../launcher", image.join("shims/gh")).unwrap();
        #[cfg(windows)]
        fs::write(image.join("shims/gh.cmd"), b"@launcher").unwrap();
        fs::write(&task.paths.stdout, b"result\n").unwrap();
        (task, metadata)
    }

    #[test]
    fn finished_image_is_removed_on_replay_without_delivering_or_changing_output() {
        let storage = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let (task, metadata) = saved_image(storage.path(), 1);
        let saved = fs::read(&task.paths.json).unwrap();
        assert!(!metadata.completion_delivered);
        registry
            .insert_rehydrated_task(metadata, task.paths.clone(), true)
            .unwrap();
        assert!(!task.paths.io_dir.join(TASK_IMAGE_DIR).exists());
        assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"result\n");
        assert_eq!(fs::read(&task.paths.json).unwrap(), saved);
    }

    #[test]
    fn terminal_images_are_not_reprobed_after_replay_cleanup() {
        use super::super::super::persistence::work_counts;

        let storage = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let mut tasks = Vec::new();
        for index in 0..4 {
            let (task, metadata) = saved_image(storage.path(), index);
            if index % 2 == 0 {
                fs::remove_dir_all(task.paths.io_dir.join(TASK_IMAGE_DIR)).unwrap();
            }
            registry
                .insert_rehydrated_task(metadata, task.paths.clone(), true)
                .unwrap();
            tasks.push(registry.task(&task.paths.task_id).unwrap());
        }
        work_counts::reset();
        registry.cleanup_finished(super::super::super::watchdog::FINISHED_RETENTION);
        registry.cleanup_finished(super::super::super::watchdog::FINISHED_RETENTION);
        assert_eq!(
            work_counts::get().stats,
            0,
            "terminal images already removed or absent were probed again"
        );
        let _ban = forbid_task_state_locks_for_test();
        for task in tasks {
            assert_eq!(registry.cleanup_terminal_task_image(&task).remaining, 0);
        }
    }

    #[test]
    fn old_finished_images_are_cleaned_in_bounded_persisted_gc_batches() {
        let storage = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let count = super::super::super::gc_cursor::GC_ENTRY_BUDGET + 5;
        let images: Vec<_> = (0..count)
            .map(|index| {
                let (task, _) = saved_image(storage.path(), index);
                task.paths.io_dir.join(TASK_IMAGE_DIR)
            })
            .collect();
        // Fresh metadata and undelivered results must not delay image cleanup
        // until the output bundle's retention or abandonment deadline.
        assert_eq!(registry.maybe_gc_persisted(storage.path()).unwrap(), 0);
        let remaining = images.iter().filter(|image| image.exists()).count();
        assert!(remaining > 0 && remaining < count, "remaining: {remaining}");
        for _ in 0..4 {
            assert_eq!(registry.maybe_gc_persisted(storage.path()).unwrap(), 0);
        }
        assert!(images.iter().all(|image| !image.exists()));
    }

    #[test]
    fn a_running_record_keeps_its_image() {
        let storage = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let (task, mut metadata) = saved_image(storage.path(), 1);
        metadata.status = BgTaskStatus::Running;
        let result = cleanup_image(&task.paths, &metadata, || true);
        assert_eq!((result.removed, result.remaining), (0, 1));
        assert!(task.paths.io_dir.join(TASK_IMAGE_DIR).exists());
        // The saved pass uses the same terminal guard, including records from
        // another daemon that this registry has not replayed.
        write_task_at(&task, &metadata).unwrap();
        assert_eq!(registry.maybe_gc_persisted(storage.path()).unwrap(), 0);
        assert!(task.paths.io_dir.join(TASK_IMAGE_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn descendant_outlives_shell_and_keeps_image_until_it_exits() {
        use super::super::super::process::start_new_session;
        use std::process::{Child, Command, Stdio};

        struct Shell(Child);
        impl Drop for Shell {
            fn drop(&mut self) {
                unsafe {
                    libc::kill(-(self.0.id() as i32), libc::SIGKILL);
                }
                let _ = self.0.wait();
            }
        }

        let storage = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let (task, mut metadata) = saved_image(storage.path(), 1);
        let mut command = Command::new("sh");
        command.current_dir(storage.path()).args(["-c",
            &format!("sh -c 'echo $$ > descendant.tmp; mv descendant.tmp descendant; exec sleep 30' & {}", crate::test_gate::gate("release-shell", "sleep 0.01")),
        ]).stdout(Stdio::null()).stderr(Stdio::null());
        start_new_session(&mut command);
        let mut shell = Shell(command.spawn().unwrap());
        let pgid = shell.0.id() as i32;
        metadata.mark_running(shell.0.id(), pgid);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !storage.path().join("descendant").exists() {
            assert!(Instant::now() < deadline, "descendant did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        let descendant: i32 = fs::read_to_string(storage.path().join("descendant"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        record_group_check(&mut metadata, &live_process_group_members(pgid));
        fs::write(storage.path().join("release-shell"), b"go").unwrap();
        assert!(shell.0.wait().unwrap().success());
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        // Even a stale empty sample is not evidence the group is empty now.
        metadata.live_descendants = Some(Vec::new());
        write_task_at(&task, &metadata).unwrap();
        registry
            .insert_rehydrated_task(metadata, task.paths.clone(), true)
            .unwrap();
        let image = task.paths.io_dir.join(TASK_IMAGE_DIR);
        registry.cleanup_finished(Duration::from_secs(24 * 60 * 60));
        assert!(image.is_dir(), "live descendant lost its launcher");
        assert_eq!(unsafe { libc::kill(descendant, libc::SIGKILL) }, 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        while image.exists() {
            assert!(
                Instant::now() < deadline,
                "exited descendant retained its image"
            );
            registry.cleanup_finished(Duration::from_secs(24 * 60 * 60));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"result\n");
        assert!(task.paths.json.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_process_listing_keeps_the_image() {
        let storage = tempfile::tempdir().unwrap();
        let (task, mut metadata) = saved_image(storage.path(), 1);
        let pgid = i32::MAX - 123;
        metadata.pgid = Some(pgid);
        super::super::super::process::test_hooks::fail_listing(pgid);
        let result = cleanup_image(&task.paths, &metadata, || true);
        assert_eq!((result.removed, result.remaining), (0, 1));
        assert!(task.paths.io_dir.join(TASK_IMAGE_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn saved_survivor_without_readable_start_time_keeps_the_image() {
        let storage = tempfile::tempdir().unwrap();
        let (task, mut metadata) = saved_image(storage.path(), 1);
        metadata.pgid = Some(i32::MAX - 124);
        metadata.live_descendants = Some(vec![LiveDescendant {
            pid: std::process::id(),
            comm: "unknown".into(),
            argv0: "unknown".into(),
            user: None,
            command: None,
        }]);
        assert!(metadata.group_member_identities.is_empty());
        let result = cleanup_image(&task.paths, &metadata, || true);
        assert_eq!((result.removed, result.remaining), (0, 1));
        assert!(task.paths.io_dir.join(TASK_IMAGE_DIR).exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_unknown_descendant_liveness_keeps_the_image() {
        let storage = tempfile::tempdir().unwrap();
        let (task, mut metadata) = saved_image(storage.path(), 1);
        metadata.pgid = Some(i32::MAX);
        let result = cleanup_image(&task.paths, &metadata, || true);
        assert_eq!((result.removed, result.remaining), (0, 1));
        assert!(task.paths.io_dir.join(TASK_IMAGE_DIR).exists());
    }
}
