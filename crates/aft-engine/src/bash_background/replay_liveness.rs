use super::{
    is_process_alive, is_recorded_process_alive, BgTaskRegistry, BgTaskStatus, PersistedTask,
    ProcessIdentity,
};

pub(super) fn has_live_launch_owner(metadata: &PersistedTask) -> bool {
    // Before the shell has a durable PID, only its launcher can distinguish a
    // launch in progress from an abandoned one. A peer must not turn the row
    // terminal and let GC remove a shim the shell is about to execute.
    metadata.status == BgTaskStatus::Starting
        && metadata.child_pid.is_none()
        && metadata.pgid.is_none()
        && metadata.launch_owner.as_ref().is_some_and(|owner| {
            is_process_alive(owner.pid)
                && ProcessIdentity::of(owner.pid)
                    .is_some_and(|current| owner.same_process(&current))
        })
}

/// Require affirmative evidence before inventing a terminal result in GC.
pub(super) fn recorded_local_task_is_gone(metadata: &PersistedTask) -> bool {
    use super::super::process::recorded_process_liveness;
    use crate::process_identity::ProcessLiveness;

    let pids = BgTaskRegistry::persisted_task_recorded_pids(metadata);
    if metadata.remote.is_some()
        || pids.is_empty()
        || !pids.iter().all(|&pid| {
            pid > 0 && recorded_process_liveness(pid, metadata.started_at) == ProcessLiveness::Gone
        })
    {
        return false;
    }
    #[cfg(unix)]
    if let Some(pgid) = metadata.pgid.filter(|pgid| *pgid > 0) {
        // The recorded leader can be gone while other group members still run.
        let gone = unsafe { libc::killpg(pgid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        return gone
            || super::super::process::live_process_group_members(pgid)
                .is_ok_and(|group| group.is_clean());
    }
    true
}

impl BgTaskRegistry {
    pub(super) fn persisted_task_process_is_alive(metadata: &PersistedTask) -> bool {
        if metadata.remote.is_some() && !metadata.is_terminal() {
            return true;
        }
        Self::persisted_task_recorded_pids(metadata)
            .into_iter()
            .any(|pid| is_recorded_process_alive(pid, metadata.started_at))
    }

    /// The task's recorded child and process group leader, in that order.
    pub(super) fn persisted_task_recorded_pids(metadata: &PersistedTask) -> Vec<u32> {
        let group_leader = metadata.pgid.and_then(|pid| u32::try_from(pid).ok());
        metadata.child_pid.into_iter().chain(group_leader).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn clock_step_preserves_starting_task_launch_owner_but_not_reused_pid() {
        use crate::process_identity::with_linux_boot_time_for_test;
        let mut task = PersistedTask::starting(
            "bash-0123456789abcdef".into(),
            "session".into(),
            "true".into(),
            std::path::PathBuf::new(),
            None,
            None,
            false,
            false,
        );
        let pid = std::process::id();
        task.launch_owner = with_linux_boot_time_for_test(100, || ProcessIdentity::of(pid));
        let saved = serde_json::to_vec(&task).unwrap();
        with_linux_boot_time_for_test(200, || {
            let mut task: PersistedTask = serde_json::from_slice(&saved).unwrap();
            let mut current = ProcessIdentity::of(pid).unwrap();
            assert_ne!(
                task.launch_owner.as_ref().unwrap().started_at_ms,
                current.started_at_ms
            );
            assert!(has_live_launch_owner(&task));

            task.launch_owner.as_mut().unwrap().native_birth = None;
            assert!(
                !has_live_launch_owner(&task),
                "legacy records require exact epoch"
            );

            current.native_birth.as_mut().unwrap().start_ticks += 1;
            task.launch_owner = Some(current);
            assert!(
                !has_live_launch_owner(&task),
                "matching epoch must not hide PID reuse"
            );
        });
    }

    #[test]
    fn executable_pin_replay_defers_only_the_live_owner_before_child_identity() {
        let mut task = PersistedTask::starting(
            "bash-starting".into(),
            "session".into(),
            "gh --shim-version".into(),
            std::path::PathBuf::new(),
            None,
            None,
            false,
            false,
        );
        let owner = ProcessIdentity::of(std::process::id()).unwrap();
        assert!(!has_live_launch_owner(&task));
        task.launch_owner = Some(ProcessIdentity::from_epoch(
            owner.pid,
            owner.started_at_ms.wrapping_add(1),
        ));
        assert!(
            !has_live_launch_owner(&task),
            "a reused PID cannot own the launch"
        );
        task.launch_owner = Some(owner.clone());
        assert!(has_live_launch_owner(&task));
        task.child_pid = Some(owner.pid);
        assert!(
            !has_live_launch_owner(&task),
            "keep normal recovery after the child checkpoint"
        );
    }
}

#[cfg(all(target_os = "linux", feature = "test-hooks"))]
pub(super) mod test_hook {
    use super::BgTaskRegistry;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::Path;
    use std::time::Duration;

    pub(crate) fn before_child_identity(metadata: &Path) {
        if let Ok(address) = std::env::var("AFT_TEST_BASH_BEFORE_CHILD_IDENTITY") {
            let mut stream =
                TcpStream::connect(address).expect("connect to starting-task replay test");
            stream
                .set_read_timeout(Some(Duration::from_secs(60)))
                .unwrap();
            writeln!(stream, "{}", metadata.display()).unwrap();
            let _ = stream.read(&mut [0]);
        }
    }

    pub(crate) fn after_replay(registry: &BgTaskRegistry, storage: &Path) {
        if let Ok(address) = std::env::var("AFT_TEST_BASH_AFTER_STARTING_REPLAY") {
            // Run the disk sweep and registered-task image cleanup before
            // releasing the launch, rather than wait for a watchdog tick.
            registry.maybe_gc_persisted(storage).unwrap();
            for task in registry.task_arcs() {
                registry.cleanup_terminal_task_image(&task);
            }
            TcpStream::connect(address)
                .unwrap()
                .write_all(b"1")
                .unwrap();
        }
    }
}
