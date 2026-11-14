pub use aft_base::fs_lock::*;

#[cfg(test)]
pub(crate) mod retry_clock {
    use super::*;

    struct State {
        started: Instant,
        now: Instant,
        preparation_delay: Duration,
        sleeps: Vec<Duration>,
        attempts: usize,
    }

    thread_local! {
        static CLOCK: std::cell::RefCell<Option<State>> = const { std::cell::RefCell::new(None) };
    }

    pub(crate) struct Guard;

    impl Guard {
        pub(crate) fn install(preparation_delay: Duration) -> Self {
            CLOCK.with(|clock| {
                let mut clock = clock.borrow_mut();
                assert!(clock.is_none(), "retry clock already installed");
                let started = Instant::now();
                *clock = Some(State {
                    started,
                    now: started,
                    preparation_delay,
                    sleeps: Vec::new(),
                    attempts: 0,
                });
            });
            Self
        }

        pub(crate) fn observations(&self) -> (Duration, Vec<Duration>, usize) {
            CLOCK.with(|clock| {
                let clock = clock.borrow();
                let state = clock.as_ref().unwrap();
                (
                    state.now - state.started,
                    state.sleeps.clone(),
                    state.attempts,
                )
            })
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            CLOCK.with(|clock| *clock.borrow_mut() = None);
        }
    }

    pub(super) fn now() -> Option<Instant> {
        CLOCK.with(|clock| clock.borrow().as_ref().map(|state| state.now))
    }

    pub(super) fn before_acquire() {
        CLOCK.with(|clock| {
            if let Some(state) = clock.borrow_mut().as_mut() {
                // Model filesystem preparation after the caller chose its deadline.
                state.now += std::mem::take(&mut state.preparation_delay);
            }
        });
    }

    pub(super) fn note_attempt() {
        CLOCK.with(|clock| {
            if let Some(state) = clock.borrow_mut().as_mut() {
                state.attempts += 1;
            }
        });
    }

    pub(super) fn sleep(duration: Duration) -> bool {
        CLOCK.with(|clock| {
            let mut clock = clock.borrow_mut();
            let Some(state) = clock.as_mut() else {
                return false;
            };
            state.sleeps.push(duration);
            state.now += duration;
            assert!(
                state.sleeps.len() <= 1_000,
                "retry loop ignored its deadline"
            );
            true
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "opt-in live-owner wait CPU measurement"]
    fn measurement_live_owner_wait_cpu() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("held.lock");
        let _owner = acquire(&path).unwrap();
        let sleeps = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _observer = observe_retry_sleeps_for_test(Arc::clone(&sleeps));
        let cpu = || {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            assert_eq!(
                unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
                0
            );
            time.tv_sec as f64 + time.tv_nsec as f64 / 1e9
        };
        let cpu_started = cpu();
        let started = Instant::now();
        assert!(matches!(
            try_acquire(&path, Duration::from_millis(1050)),
            Err(AcquireError::Timeout)
        ));
        eprintln!(
            "LOCK_WAIT_MEASUREMENT poll_ms={} sleeps={} wall_ms={:.3} thread_cpu_ms={:.3}",
            POLL_INTERVAL_MS,
            sleeps.load(Ordering::SeqCst),
            started.elapsed().as_secs_f64() * 1000.0,
            (cpu() - cpu_started) * 1000.0
        );
        assert!(sleeps.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn live_owner_poll_sleeps_to_the_original_deadline() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("held.lock");
        let _owner = acquire(&path).unwrap();
        let clock = retry_clock::Guard::install(Duration::ZERO);
        assert!(matches!(
            try_acquire(&path, Duration::from_millis(1050)),
            Err(AcquireError::Timeout)
        ));
        let (elapsed, sleeps, attempts) = clock.observations();
        assert_eq!(elapsed, Duration::from_millis(1050));
        assert_eq!(attempts, 11);
        let mut expected = vec![Duration::from_millis(100); 10];
        expected.push(Duration::from_millis(50));
        assert_eq!(sleeps, expected);
    }

    fn macos_snapshot_bytes(state: u8, flags: u32) -> [u8; 648] {
        let mut bytes = [0; 648];
        bytes[0..8].copy_from_slice(&1_700_000_000_i64.to_ne_bytes());
        bytes[8..12].copy_from_slice(&123_456_i32.to_ne_bytes());
        bytes[32..36].copy_from_slice(&flags.to_ne_bytes());
        bytes[36] = state;
        bytes[243..248].copy_from_slice(b"owner");
        bytes[392..396].copy_from_slice(&42_u32.to_ne_bytes());
        bytes[564..568].copy_from_slice(&123_i32.to_ne_bytes());
        bytes
    }

    #[test]
    fn semantic_batch_lease_macos_snapshot_keeps_birth_and_exit_phase() {
        let info = decode_macos_kinfo(&macos_snapshot_bytes(SZOMB, P_WEXIT)).unwrap();
        assert_eq!(info.comm, "owner");
        assert_eq!((info.ruid, info.pgid), (42, 123));
        assert_eq!(info.exit_state(), MacosExitState::Zombie);
        let identity = macos_process_identity(Some(info)).unwrap();
        assert_eq!(identity.start_time, 1_700_000_000_123_456);
        assert!(identity.exited);
        assert!(identity.boot_id.is_none());

        let exiting = decode_macos_kinfo(&macos_snapshot_bytes(2, P_WEXIT)).unwrap();
        assert_eq!(exiting.exit_state(), MacosExitState::Exiting);
        assert!(macos_process_identity(Some(exiting)).unwrap().exited);
        let running = decode_macos_kinfo(&macos_snapshot_bytes(2, 0)).unwrap();
        assert_eq!(running.exit_state(), MacosExitState::NotExiting);
        assert!(!macos_process_identity(Some(running)).unwrap().exited);
    }

    #[test]
    fn semantic_batch_lease_macos_missing_identity_stays_inconclusive() {
        assert!(macos_process_identity(None).is_none());
        let mut bytes = macos_snapshot_bytes(SZOMB, P_WEXIT);
        assert!(decode_macos_kinfo(&bytes[..16]).is_none());
        bytes[8..12].copy_from_slice(&1_000_000_i32.to_ne_bytes());
        assert!(macos_process_identity(decode_macos_kinfo(&bytes)).is_none());
        bytes[8..12].copy_from_slice(&0_i32.to_ne_bytes());
        bytes[0..8].copy_from_slice(&0_i64.to_ne_bytes());
        assert!(macos_process_identity(decode_macos_kinfo(&bytes)).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn semantic_batch_lease_macos_birth_matches_live_libproc() {
        let pid = std::process::id();
        let identity = process_identity(pid).expect("kernel identity for this process");
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let written = unsafe { proc_pidinfo(pid as i32, 3, 0, info.as_mut_ptr().cast(), size) };
        assert_eq!(written, size, "libproc must answer for this live process");
        let info = unsafe { info.assume_init() };
        assert_eq!(
            identity.start_time,
            info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec
        );
        assert!(!identity.exited);
        assert_eq!(
            crate::root_cache::process_start_time_ms(pid),
            Some(identity.start_time / 1_000)
        );
    }
    use std::io::{BufRead, Write};
    use std::process::{Child, ChildStdin, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Barrier};

    #[cfg(unix)]
    #[test]
    fn advisory_lock_guard_releases_lock_with_a_live_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("advisory.lock");
        for shared in [true, false] {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .unwrap();
            if shared {
                file.lock_shared().unwrap();
            } else {
                file.lock().unwrap();
            }
            // A duplicate shares the open file description, just like fork's copy.
            let duplicate = file.try_clone().unwrap();
            let guard = AdvisoryLockGuard::from_locked_file(file);
            let probe = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            assert!(matches!(
                probe.try_lock(),
                Err(fs::TryLockError::WouldBlock)
            ));
            drop(guard);
            probe
                .try_lock()
                .expect("the owner released exclusion even with a live duplicate");
            assert!(duplicate.metadata().is_ok());
            probe.unlock().unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_process_probe_records_creation_identity() {
        let pid = std::process::id();
        let identity = process_identity(pid).expect("current Windows process creation identity");
        assert!(identity.start_time > 0);
        assert!(windows_process_start_time_ms(pid).unwrap() > 0);
        assert_eq!(
            windows_creation_ticks_to_unix_ms(116_444_736_000_000_000),
            Some(0)
        );
        assert_eq!(
            windows_creation_ticks_to_unix_ms(116_444_736_010_000_000),
            Some(1000)
        );
        assert_eq!(windows_creation_ticks_to_unix_ms(0), None);
        assert!(process_alive(pid));
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
        let mut metadata = current_process_metadata();
        metadata.process_start_time = Some(identity.start_time.saturating_add(1));
        assert!(
            !lock_owner_is_alive(&metadata),
            "recycled PID must not keep an old lease alive"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_process_probe_recognizes_exited_status_259() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 259"])
            .spawn()
            .unwrap();
        let pid = child.id();
        assert_eq!(child.wait().unwrap().code(), Some(259));
        assert!(
            !process_alive(pid),
            "a signaled process object is exited even when its exit code is STILL_ACTIVE"
        );
    }

    #[test]
    fn malformed_reclaim_preserves_replacement_epoch() {
        let (_dir, path) = test_lock_path();
        fs::write(&path, b"malformed").unwrap();
        let replacement = synthetic_metadata(std::process::id(), current_hostname(), now_ms());
        let expected_epoch = replacement.writer_epoch.clone();
        let replace_path = path.clone();
        BEFORE_MALFORMED_RECLAIM.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::remove_file(&replace_path).unwrap();
                write_synthetic_lock(&replace_path, &replacement);
            }))
        });
        let result = acquire_with_config(&path, Some(Duration::from_millis(100)), test_config());
        assert!(
            matches!(result, Err(AcquireError::Timeout)),
            "replacement owner must block acquisition"
        );
        assert_eq!(
            read_lock_metadata(&path).unwrap().writer_epoch,
            expected_epoch
        );
    }

    #[test]
    fn heartbeat_spawn_failure_removes_own_lock() {
        let (_dir, path) = test_lock_path();
        BEFORE_HEARTBEAT_SPAWN_FAILURE.with(|slot| *slot.borrow_mut() = Some(Box::new(|| {})));
        assert!(create_new_lock(&path, &current_hostname(), test_config()).is_err());
        assert!(
            !path.exists(),
            "failed acquisition must not leave a live-PID lock"
        );
        drop(create_new_lock(&path, &current_hostname(), test_config()).unwrap());
    }

    #[test]
    fn heartbeat_spawn_failure_preserves_replacement_lock() {
        let (_dir, path) = test_lock_path();
        let replacement = synthetic_metadata(std::process::id(), current_hostname(), now_ms());
        let expected_epoch = replacement.writer_epoch.clone();
        let replace_path = path.clone();
        BEFORE_HEARTBEAT_SPAWN_FAILURE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::remove_file(&replace_path).unwrap();
                write_synthetic_lock(&replace_path, &replacement);
            }))
        });
        assert!(create_new_lock(&path, &current_hostname(), test_config()).is_err());
        assert_eq!(
            read_lock_metadata(&path).unwrap().writer_epoch,
            expected_epoch
        );
    }

    /// A lock file whose metadata cannot be read because another actor is
    /// mid-create/delete is contention, not a failure. The create arm already
    /// treated it that way; the metadata read beside it did not, so on Windows
    /// a pending-deletion lock denied the read and propagated a hard error to a
    /// caller that then panicked mid-write. Permission-denied is classified as
    /// contention on every platform, so this reproduces without Windows: with
    /// the retry the acquisition spends its budget and times out; without it
    /// the raw permission error escapes immediately.
    #[cfg(unix)]
    #[test]
    fn unreadable_lock_metadata_is_retried_as_contention_not_propagated() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.lock");
        std::fs::write(&path, b"{}\n").expect("seed lock file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("deny reads");
        if std::fs::read(&path).is_ok() {
            // Running as a user that bypasses the mode bits (root in some CI
            // images); the premise cannot hold, so assert nothing.
            return;
        }

        let outcome = try_acquire(&path, Duration::from_millis(250));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
        match outcome {
            Err(AcquireError::Timeout) => {}
            Err(AcquireError::Io(error)) => {
                panic!("contended metadata read propagated instead of retrying: {error:?}")
            }
            Ok(_) => panic!("acquired a lock whose metadata could not be read"),
        }
    }

    const HARNESS_TEST: &str = "fs_lock::tests::fs_lock_separate_process_harness";
    const HARNESS_MODE_INSPECT: &str = "inspect";

    fn test_config() -> LockConfig {
        LockConfig {
            heartbeat_interval_ms: 25,
            heartbeat_refresh_interval_ms: 25,
            stale_heartbeat_ms: 2_000,
            live_owner_warn_ms: LIVE_OWNER_WARN_MS,
            poll_interval_ms: 10,
        }
    }

    fn test_lock_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("test.lock");
        (dir, path)
    }

    fn write_synthetic_lock(path: &Path, metadata: &LockMetadata) {
        let mut file = open_new_lock_file(path).expect("create synthetic lock");
        write_lock_metadata_to_file(&mut file, metadata).expect("write synthetic lock");
    }

    #[derive(Serialize)]
    struct LegacyLockMetadata<'a> {
        pid: u32,
        hostname: &'a str,
        created_at_ms: u64,
        heartbeat_at_ms: u64,
        writer_epoch: &'a str,
    }

    fn write_legacy_lock(path: &Path, metadata: &LockMetadata) -> String {
        let legacy = LegacyLockMetadata {
            pid: metadata.pid,
            hostname: &metadata.hostname,
            created_at_ms: metadata.created_at_ms,
            heartbeat_at_ms: metadata.heartbeat_at_ms,
            writer_epoch: &metadata.writer_epoch,
        };
        let contents = format!(
            "{}\n",
            serde_json::to_string(&legacy).expect("serialize legacy lock")
        );
        fs::write(path, &contents).expect("write legacy synthetic lock");
        contents
    }

    fn synthetic_metadata(pid: u32, hostname: String, created_at_ms: u64) -> LockMetadata {
        LockMetadata {
            pid,
            hostname,
            // Synthetic metadata defaults to the legacy shape so tests must opt
            // in when they need to exercise process-instance identity.
            process_start_time: None,
            boot_id: None,
            created_at_ms,
            heartbeat_at_ms: created_at_ms,
            writer_epoch: format!("synthetic-{pid}-{created_at_ms}"),
        }
    }

    fn current_process_metadata() -> LockMetadata {
        let now = now_ms();
        let pid = std::process::id();
        let process_identity = process_identity(pid);
        let mut metadata = synthetic_metadata(pid, current_hostname(), now);
        metadata.process_start_time = process_identity
            .as_ref()
            .map(|identity| identity.start_time);
        metadata.boot_id = process_identity.and_then(|identity| identity.boot_id);
        metadata
    }

    fn different_start_time(start_time: u64) -> u64 {
        start_time.checked_add(1).unwrap_or(start_time - 1)
    }

    fn write_reclaim_token(lock_path: &Path, metadata: &LockMetadata) -> PathBuf {
        let token_path = reclaim_token_path(lock_path);
        write_synthetic_lock(&token_path, metadata);
        token_path
    }

    fn take_reclaim_test_logs() -> Vec<String> {
        RECLAIM_TEST_LOGS.with(|logs| std::mem::take(&mut *logs.borrow_mut()))
    }

    #[test]
    fn lock_operation_trace_lines_are_debug_not_info() {
        let source = include_str!("../../aft-base/src/fs_lock.rs");
        assert!(source.contains("slog_debug!(\"acquired filesystem lock at {}\", path.display())"));
        assert!(
            source.contains("slog_debug!(\"released filesystem lock at {}\", self.path.display())")
        );
        assert!(!source.contains("slog_info!(\"acquired filesystem lock at {}\", path.display())"));
        assert!(
            !source.contains("slog_info!(\"released filesystem lock at {}\", self.path.display())")
        );
    }

    #[test]
    fn acquire_creates_lockfile_and_unlocks_on_drop() {
        let (_dir, path) = test_lock_path();

        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lock");
        let metadata = read_lock_metadata(&path).expect("read lock metadata");
        assert_eq!(metadata.pid, std::process::id());
        assert_eq!(metadata.hostname, current_hostname());
        assert_eq!(metadata.created_at_ms, guard.metadata.created_at_ms);
        assert_eq!(metadata.writer_epoch, guard.metadata.writer_epoch);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn permission_denied_is_treated_as_transient_create_contention() {
        // Windows surfaces a contended create/delete on the same lock path as
        // access-denied; acquire must retry these rather than fail the caller.
        let err = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(is_transient_create_contention(&err));
    }

    #[test]
    fn unrelated_io_errors_are_not_treated_as_contention() {
        // A genuinely fatal error (e.g. the parent dir is missing) must still
        // propagate, not spin in the transient-retry arm.
        let err = io::Error::from(io::ErrorKind::NotFound);
        assert!(!is_transient_create_contention(&err));
    }

    #[cfg(windows)]
    #[test]
    fn windows_sharing_violation_is_treated_as_transient_create_contention() {
        // ERROR_SHARING_VIOLATION (32) is the other contention code Windows
        // returns when a concurrent actor holds the path open mid-create.
        let err = io::Error::from_raw_os_error(32);
        assert!(is_transient_create_contention(&err));
    }

    #[test]
    fn reclaim_refuses_to_delete_a_different_owners_lock() {
        let (_dir, path) = test_lock_path();

        // A lock currently owned by "owner B".
        let owner_b = synthetic_metadata(4242, "host-b".to_string(), now_ms());
        create_lock_file_atomically(&path, &owner_b).expect("write owner B lock");

        // We judged a DIFFERENT (older) owner A as stale. Reclaiming must NOT
        // delete B's lock (the TOCTOU split-brain guard).
        let judged_a = synthetic_metadata(1111, "host-a".to_string(), now_ms() - 1_000_000);
        let outcome = reclaim_lock_file(&path, &judged_a).expect("reclaim");
        assert!(
            matches!(outcome, ReclaimResult::Unchanged),
            "must not remove a different owner's lock"
        );
        assert!(path.exists(), "owner B's lock must survive");
        let still = read_lock_metadata(&path).expect("still readable");
        assert_eq!(still.pid, 4242, "owner B's lock intact");
    }

    #[test]
    fn reclaim_deletes_when_identity_still_matches() {
        let (_dir, path) = test_lock_path();
        let owner = synthetic_metadata(1111, "host-a".to_string(), 5_000);
        create_lock_file_atomically(&path, &owner).expect("write lock");

        // Same identity we judged → safe to remove.
        let outcome = reclaim_lock_file(&path, &owner).expect("reclaim");
        assert!(
            matches!(outcome, ReclaimResult::Removed),
            "matching-identity stale lock should be removed"
        );
        assert!(!path.exists());

        // Reclaiming a now-absent lock is a no-op, not an error.
        assert!(matches!(
            reclaim_lock_file(&path, &owner).expect("reclaim missing"),
            ReclaimResult::Unchanged
        ));
    }

    #[test]
    fn try_acquire_once_reclaims_malformed_after_one_grace_read() {
        for contents in [
            b"".as_slice(),
            b"{\"pid\":999999999,\"hostname\":".as_slice(),
        ] {
            let (_dir, path) = test_lock_path();
            fs::write(&path, contents).unwrap();
            let sleeper_entries = Arc::new(AtomicUsize::new(0));
            let _observer = observe_retry_sleeps_for_test(Arc::clone(&sleeper_entries));
            let guard = try_acquire_once(&path).expect("stable malformed lease must be reclaimed");
            assert!(guard.verify_writer_epoch().unwrap());
            assert_eq!(sleeper_entries.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn try_acquire_once_malformed_reclaim_stops_when_token_is_held() {
        let (_dir, path) = test_lock_path();
        fs::write(&path, b"malformed").unwrap();
        write_reclaim_token(&path, &current_process_metadata());
        let sleeper_entries = Arc::new(AtomicUsize::new(0));
        let _observer = observe_retry_sleeps_for_test(Arc::clone(&sleeper_entries));
        assert!(matches!(
            try_acquire_once(&path),
            Err(AcquireError::Timeout)
        ));
        assert_eq!(sleeper_entries.load(Ordering::SeqCst), 1);
        assert_eq!(fs::read(&path).unwrap(), b"malformed");
    }

    #[test]
    fn try_acquire_once_malformed_reclaim_preserves_live_replacement() {
        let (_dir, path) = test_lock_path();
        fs::write(&path, b"malformed").unwrap();
        let replacement = current_process_metadata();
        let expected_epoch = replacement.writer_epoch.clone();
        let replace_path = path.clone();
        BEFORE_MALFORMED_RECLAIM.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::remove_file(&replace_path).unwrap();
                write_synthetic_lock(&replace_path, &replacement);
            }))
        });
        assert!(matches!(
            try_acquire_once(&path),
            Err(AcquireError::Timeout)
        ));
        assert_eq!(
            read_lock_metadata(&path).unwrap().writer_epoch,
            expected_epoch
        );
    }

    #[test]
    fn try_acquire_once_never_waits_behind_live_owner() {
        const OUTER_THREAD_JOIN_TIMEOUT: Duration = Duration::from_secs(30);

        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lock");
        let contender_path = path.clone();
        let sleeper_entries = Arc::new(AtomicUsize::new(0));
        let contender_sleeper_entries = Arc::clone(&sleeper_entries);
        let (announced_tx, announced_rx) = mpsc::sync_channel(1);
        let (enter_tx, enter_rx) = mpsc::sync_channel::<()>(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let contender = std::thread::spawn(move || {
            let _ = announced_tx.send(());
            let _ = enter_rx.recv();
            let _observer = observe_retry_sleeps_for_test(contender_sleeper_entries);
            let _ = result_tx.send(try_acquire_once(&contender_path));
        });

        announced_rx
            .recv_timeout(OUTER_THREAD_JOIN_TIMEOUT)
            .expect("contender should announce before the controlled enter gate");
        // Keep the announce-to-enter gap under channel control rather than
        // charging an arbitrary scheduler pause to the acquisition decision.
        enter_tx
            .send(())
            .expect("contender should wait at the controlled enter gate");

        let result = match result_rx.recv_timeout(OUTER_THREAD_JOIN_TIMEOUT) {
            Ok(result) => result,
            Err(error) => {
                drop(guard);
                let _ = result_rx.recv_timeout(OUTER_THREAD_JOIN_TIMEOUT);
                let _ = contender.join();
                panic!("contender did not finish before the outer join bound: {error}");
            }
        };

        contender.join().expect("contender should exit");
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(
            sleeper_entries.load(Ordering::SeqCst),
            0,
            "zero-timeout acquisition must return Timeout without sleeping"
        );
    }

    #[test]
    fn acquire_serializes_concurrent_callers() {
        let (_dir, path) = test_lock_path();
        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(3));
        let inside = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(AtomicUsize::new(0));
        let max_inside = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            let inside = Arc::clone(&inside);
            let entered = Arc::clone(&entered);
            let max_inside = Arc::clone(&max_inside);
            handles.push(thread::spawn(move || {
                barrier.wait();
                let guard = acquire_with_config(&path, Some(Duration::from_secs(2)), test_config())
                    .expect("thread acquire lock");
                let previous = inside.fetch_add(1, Ordering::SeqCst);
                assert_eq!(previous, 0, "two lock holders overlapped");
                entered.fetch_add(1, Ordering::SeqCst);
                max_inside.fetch_max(previous + 1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(75));
                inside.fetch_sub(1, Ordering::SeqCst);
                drop(guard);
            }));
        }

        barrier.wait();
        for handle in handles {
            handle.join().expect("join worker");
        }

        assert_eq!(entered.load(Ordering::SeqCst), 2);
        assert_eq!(max_inside.load(Ordering::SeqCst), 1);
        assert!(!path.exists());
    }

    #[test]
    fn failed_atomic_replacement_preserves_existing_destination() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let source = dir.path().join("source.tmp");
        let destination = dir.path().join("artifact.bin");
        fs::write(&source, b"new artifact").expect("write source");
        fs::write(&destination, b"valid old artifact").expect("write destination");

        let error = rename_over_with(&source, &destination, |_from, _to| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected replacement failure",
            ))
        })
        .expect_err("replacement must fail");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            fs::read(&destination).expect("read preserved destination"),
            b"valid old artifact"
        );
        assert_eq!(
            fs::read(&source).expect("read retained source"),
            b"new artifact"
        );
    }

    /// Number of lease files and beats per lease that model sixty seconds of
    /// heartbeats at the production interval.
    const LEDGER_LEASES: usize = 8;
    const LEDGER_BEATS_PER_LEASE: usize = (60_000 / HEARTBEAT_INTERVAL_MS) as usize;

    fn create_owned_lock(path: &Path) -> LockMetadata {
        let metadata = current_process_metadata();
        create_lock_file_atomically(path, &metadata).expect("create lock");
        metadata
    }

    #[cfg(unix)]
    fn inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).expect("lock metadata").ino()
    }

    #[test]
    fn healthy_refresh_cadence_does_not_trigger_stale_warning() {
        let config = LockConfig::default();
        assert_eq!(config.live_owner_warning_age_ms(), 30_000);
        assert_eq!(config.cross_host_stale_heartbeat_ms(), 75_000);
        assert_eq!(test_config().live_owner_warning_age_ms(), 2_000);
    }

    #[test]
    fn idle_writer_lease_writes_twice_in_sixty_seconds() {
        let (_dir, path) = test_lock_path();
        let owner = create_owned_lock(&path);
        let (pump, _handle) = HeartbeatPump::attach(&path, &owner).unwrap();
        let config = LockConfig::default();
        let start = Instant::now();
        let interval = Duration::from_millis(config.heartbeat_refresh_interval_ms);
        let mut next = start + interval;
        let bytes = fs::metadata(&path).unwrap().len();
        let _ = io_ledger::take();
        let mut writes = 0;
        // Drive the production scheduler with a virtual minute; actual lease
        // writes still go through the kept descriptor and I/O ledger.
        for seconds in (5..=60).step_by(5) {
            writes += usize::from(
                refresh_heartbeat_if_due(
                    &pump,
                    &mut next,
                    interval,
                    start + Duration::from_secs(seconds),
                )
                .unwrap(),
            );
        }
        let ledger = io_ledger::take();
        assert_eq!(writes, 2, "one idle lease must write at 25s and 50s only");
        assert_eq!(ledger.bytes_written, bytes * 2);
        assert_eq!(
            (ledger.file_syncs, ledger.dir_syncs, ledger.new_files),
            (0, 0, 0)
        );
        eprintln!("idle writer lease / virtual 60s: before=12 after={writes} {ledger:?}");
        unregister_lease_slot(pump.slot, &path);
    }

    #[test]
    fn delayed_refresh_preserves_live_owner_and_foreign_freshness() {
        let (_dir, path) = test_lock_path();
        let mut owner = create_owned_lock(&path);
        let now = now_ms();
        // A wake delayed 40s beyond the 25s target is still inside the unchanged
        // 75s foreign-host window. Local identity remains protected even later.
        owner.heartbeat_at_ms = now - 65_000;
        fs::write(&path, lock_metadata_bytes(&owner).unwrap()).unwrap();
        let config = LockConfig::default();
        assert_eq!(config.cross_host_stale_heartbeat_ms(), 75_000);
        assert!(metadata_blocks_acquisition(
            &owner,
            "foreign-observer",
            config,
            now
        ));
        assert!(metadata_blocks_acquisition(
            &owner,
            &owner.hostname,
            config,
            now + 3_600_000
        ));
        let (pump, _handle) = HeartbeatPump::attach(&path, &owner).unwrap();
        let wake = Instant::now();
        let mut next = wake - Duration::from_secs(40);
        assert!(refresh_heartbeat_if_due(&pump, &mut next, Duration::from_secs(25), wake).unwrap());
        let refreshed = read_lock_metadata(&path).unwrap();
        assert!(refreshed.heartbeat_at_ms >= now);
        assert!(metadata_blocks_acquisition(
            &refreshed,
            "foreign-observer",
            config,
            now + 70_000
        ));
        unregister_lease_slot(pump.slot, &path);
    }

    #[test]
    fn failed_refresh_retries_on_next_short_wake() {
        let (_dir, path) = test_lock_path();
        let owner = create_owned_lock(&path);
        let (pump, _handle) = HeartbeatPump::attach(&path, &owner).unwrap();
        let wake = Instant::now();
        let mut next = wake;
        fs::write(&path, b"partial").unwrap();
        assert!(refresh_heartbeat_if_due(&pump, &mut next, Duration::from_secs(25), wake).is_err());
        assert_eq!(next, wake, "a failed write must not postpone the retry");
        fs::write(&path, lock_metadata_bytes(&owner).unwrap()).unwrap();
        assert!(refresh_heartbeat_if_due(
            &pump,
            &mut next,
            Duration::from_secs(25),
            wake + Duration::from_secs(5)
        )
        .unwrap());
        unregister_lease_slot(pump.slot, &path);
    }

    /// Sixty seconds of forced heartbeats on several leases must not fsync anything
    /// and must not create a new file per beat. The ledger totals are printed
    /// so the cost can be compared across changes.
    #[test]
    fn heartbeat_ledger_for_sixty_seconds_has_no_fsync_and_no_new_inodes() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let leases = (0..LEDGER_LEASES)
            .map(|index| {
                let path = dir.path().join(format!("writer-{index}.lease"));
                let owner = create_owned_lock(&path);
                (path, owner)
            })
            .collect::<Vec<_>>();
        #[cfg(unix)]
        let inodes_before = leases
            .iter()
            .map(|(path, _)| inode(path))
            .collect::<Vec<_>>();
        let file_bytes: u64 = leases
            .iter()
            .map(|(path, _)| fs::metadata(path).expect("lock size").len())
            .sum();

        let _ = io_ledger::take();
        for _ in 0..LEDGER_BEATS_PER_LEASE {
            for (path, owner) in &leases {
                heartbeat_once(path, owner, &Mutex::new(None)).expect("heartbeat");
            }
        }
        let ledger = io_ledger::take();
        eprintln!(
            "fs_lock heartbeat ledger: leases={LEDGER_LEASES} beats_per_lease={LEDGER_BEATS_PER_LEASE} {ledger:?}"
        );

        assert_eq!(ledger.file_syncs, 0, "heartbeats must not fsync the lease");
        assert_eq!(
            ledger.dir_syncs, 0,
            "heartbeats must not fsync the directory"
        );
        assert_eq!(ledger.new_files, 0, "heartbeats must rewrite in place");
        assert_eq!(
            ledger.bytes_written,
            file_bytes * LEDGER_BEATS_PER_LEASE as u64,
            "each beat rewrites exactly one lease's bytes"
        );
        #[cfg(unix)]
        assert_eq!(
            leases
                .iter()
                .map(|(path, _)| inode(path))
                .collect::<Vec<_>>(),
            inodes_before,
            "heartbeats must keep the lease inode"
        );
        for (path, owner) in &leases {
            let current = read_lock_metadata(path).expect("read lease");
            assert!(lock_identity_matches(&current, owner));
            assert!(current.heartbeat_at_ms >= owner.heartbeat_at_ms);
        }
    }

    /// The old contract asserted file + directory syncs at creation and during
    /// reclaim. Ownership instead depends on the live process identity: after
    /// power loss the owner is dead, and torn leases are stale. The atomic hard
    /// link and reclaim token still serialize live contenders without making
    /// those process-lifetime records power-loss durable.
    #[test]
    fn durability_lease_create_and_reclaim_unsynced() {
        let (_dir, path) = test_lock_path();
        let _ = io_ledger::take();
        crate::durability::take();
        let guard = acquire_with_config(&path, Some(Duration::from_secs(2)), test_config())
            .expect("create lease");
        let created = io_ledger::take();
        drop(guard);
        assert_eq!(
            (created.file_syncs, created.dir_syncs, created.new_files),
            (0, 0, 1),
            "lease creation must not sync: {created:?}"
        );
        assert_eq!(crate::durability::sync_count(&crate::durability::take()), 0);

        let dead = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &dead);
        let _ = io_ledger::take();
        crate::durability::take();
        let guard = acquire_with_config(&path, Some(Duration::from_secs(2)), test_config())
            .expect("reclaim dead lease");
        let reclaimed = io_ledger::take();
        drop(guard);
        // The failed create's temp file, reclaim token and new lease still
        // require three inodes, but none need to survive their owner's death.
        assert_eq!(
            (
                reclaimed.file_syncs,
                reclaimed.dir_syncs,
                reclaimed.new_files
            ),
            (0, 0, 3),
            "lease reclaim must not sync: {reclaimed:?}"
        );
        assert_eq!(crate::durability::sync_count(&crate::durability::take()), 0);
    }

    /// Leases a live daemon of this size holds at once: one root-keyed inspect
    /// writer lease and one root-keyed callgraph writer lease per bound project,
    /// for about a hundred projects with both caches open.
    const DAEMON_SCALE_LEASES: usize = 200;

    /// Fails if a beat opened or closed a descriptor instead of writing through
    /// the handle the guard keeps, which is what a regression to open-per-beat
    /// would show up as in the ledger.
    fn assert_beat_opens_no_descriptor(beat: &io_ledger::IoLedger) {
        assert_eq!(
            (beat.opens, beat.closes),
            (0, 0),
            "a beat must write through the kept handle, without opening or \
             closing a descriptor: {beat:?}"
        );
    }

    /// What the heartbeat costs for a daemon-shaped set of long-lived leases.
    ///
    /// Before the kept handle, every beat of every lease opened the file,
    /// read it, wrote it and closed it: 200 opens and 200 closes per beat, 40
    /// per second at the five-second interval, and the macOS `close()` that
    /// pulls the page write-back ran on the heartbeat thread. A beat now writes
    /// through the handle the heartbeat already holds. `kept_descriptors` counts
    /// how many descriptor opens occur across all the beats, so a regression to
    /// open-per-beat moves it from zero to the number of beats.
    #[test]
    fn daemon_scale_heartbeats_keep_descriptors_open_between_beats() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let _ = io_ledger::take();
        let guards = (0..DAEMON_SCALE_LEASES)
            .map(|index| {
                acquire_with_config(
                    &dir.path().join(format!("root-{index}.writer.lease")),
                    Some(Duration::from_secs(2)),
                    test_config(),
                )
                .expect("acquire live lease")
            })
            .collect::<Vec<_>>();
        // Open-only: every lease created here had no prior contents, so its
        // heartbeat started from the handle acquisition opened. A kept
        // descriptor released since would have taken the close path below.
        let setup = io_ledger::take();
        assert_eq!(
            setup.closes, 0,
            "acquiring a fresh lease must not close a descriptor: {setup:?}"
        );

        let beats = 3;
        let _ = io_ledger::take();
        for _ in 0..beats {
            for guard in &guards {
                guard.heartbeat_once().expect("beat");
            }
        }
        let beat_cost = io_ledger::take();
        eprintln!(
            "fs_lock heartbeat cost: leases={DAEMON_SCALE_LEASES} beats_per_lease={beats} \
             opens={} closes={} bytes={} (open+close per beat would be opens={} closes={})",
            beat_cost.opens,
            beat_cost.closes,
            beat_cost.bytes_written,
            DAEMON_SCALE_LEASES * beats,
            DAEMON_SCALE_LEASES * beats,
        );
        assert_beat_opens_no_descriptor(&beat_cost);
        assert!(beat_cost.bytes_written > 0, "{beat_cost:?}");

        for guard in &guards {
            let current = read_lock_metadata(guard.path()).expect("read lease");
            assert!(lock_identity_matches(&current, &guard.metadata));
        }

        drop(guards);
        let released = io_ledger::take();
        // Release closes each handle once. Other tests in this process run
        // their own leases on their own threads, so this asks only that the
        // releases are accounted for, never for exactly this test's count.
        assert!(
            released.closes >= DAEMON_SCALE_LEASES as u64,
            "each live lease closes its handle once, at release: {released:?}"
        );
    }

    /// A crash after an unsynced rename can leave a zero-length lease. Readers
    /// must treat it as stale: acquisition removes it and takes the lock, the
    /// owner's heartbeat reports a transient malformed read, and nothing panics.
    #[test]
    fn zero_length_lease_is_treated_as_stale() {
        let (_dir, path) = test_lock_path();
        fs::write(&path, b"").expect("write empty lease");
        let owner = current_process_metadata();
        assert!(matches!(
            heartbeat_once(&path, &owner, &Mutex::new(None)),
            Err(HeartbeatError::Malformed(_))
        ));
        assert!(!heartbeat_error_is_terminal(&HeartbeatError::Malformed(
            serde_json::from_slice::<LockMetadata>(b"").unwrap_err()
        )));

        let guard = acquire_with_config(&path, Some(Duration::from_secs(2)), test_config())
            .expect("empty lease must be reclaimed");
        assert!(guard.verify_writer_epoch().expect("verify new owner"));
    }

    /// When the new timestamp would change the file length, the heartbeat
    /// falls back to an atomic rename, still without fsync.
    #[test]
    fn heartbeat_length_change_falls_back_to_unsynced_rename() {
        let (_dir, path) = test_lock_path();
        let mut owner = current_process_metadata();
        owner.heartbeat_at_ms = 7;
        create_lock_file_atomically(&path, &owner).expect("create lock");

        let _ = io_ledger::take();
        heartbeat_once(&path, &owner, &Mutex::new(None)).expect("heartbeat");
        let ledger = io_ledger::take();
        assert_eq!((ledger.file_syncs, ledger.dir_syncs), (0, 0), "{ledger:?}");
        assert_eq!(ledger.new_files, 1, "{ledger:?}");
        let current = read_lock_metadata(&path).expect("read lease");
        assert!(lock_identity_matches(&current, &owner));
        assert!(current.heartbeat_at_ms > 7);
        let leftovers = fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "temp file must not be left behind");
    }

    #[test]
    fn heartbeat_updates_lockfile_timestamp() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lock");
        let initial_metadata = read_lock_metadata(&path).expect("read initial metadata");
        let initial = initial_metadata.heartbeat_at_ms;

        // Poll for up to 2s rather than sleeping a fixed multiple of the
        // heartbeat interval. `park_timeout` is a *maximum* wait, not a
        // guaranteed periodic timer — under load (shared macOS CI runners
        // running other cargo-test threads concurrently) the heartbeat
        // thread may not fire 3 times within 75ms even though
        // heartbeat_interval_ms=25. The contract being asserted is "the
        // heartbeat advances eventually", not "it advances within N
        // heartbeat intervals".
        //
        let deadline = std::time::Instant::now() + Duration::from_millis(2_000);
        let mut updated = initial;
        while std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            match read_lock_metadata(&path) {
                Ok(meta) => {
                    updated = meta.heartbeat_at_ms;
                    if updated > initial {
                        break;
                    }
                }
                Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    // Heartbeat thread is mid-rewrite (Windows
                    // remove-then-rename window). Retry next iteration.
                    continue;
                }
                Err(other) => panic!("read updated metadata: {other:?}"),
            }
        }
        assert!(
            updated > initial,
            "heartbeat timestamp did not advance within 2s"
        );
        let updated_metadata = read_lock_metadata(&path).expect("read final metadata");
        assert_eq!(
            updated_metadata.process_start_time, guard.metadata.process_start_time,
            "heartbeat rewrite must preserve the owner's process start time"
        );
        assert_eq!(
            updated_metadata.boot_id, guard.metadata.boot_id,
            "heartbeat rewrite must preserve the owner's boot identity"
        );
        drop(guard);
    }

    #[test]
    fn dead_pid_lock_is_reclaimed() {
        let (_dir, path) = test_lock_path();
        let metadata = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &metadata);

        let (guard, lines) = crate::logging::capture_log_lines(|| {
            acquire_with_config(&path, Some(Duration::from_secs(1)), test_config())
                .expect("reclaim dead pid lock")
        });
        let reclaim_lines: Vec<_> = lines
            .iter()
            .filter(|line| line.contains("removing filesystem lock"))
            .collect();
        assert_eq!(
            reclaim_lines,
            vec![&format!(
                "INFO removing filesystem lock at {} from dead or recycled PID {}",
                path.display(),
                metadata.pid
            )]
        );
        let metadata = read_lock_metadata(&path).expect("read reclaimed lock");
        assert_eq!(metadata.pid, std::process::id());
        drop(guard);
    }

    #[test]
    fn zero_timeout_dead_pid_reclaim_acquires_after_removing_stale_file() {
        let (_dir, path) = test_lock_path();
        let metadata = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &metadata);

        let guard = acquire_with_config(&path, Some(Duration::ZERO), test_config())
            .expect("zero-timeout acquire should claim the reaped stale lock");
        let metadata = read_lock_metadata(&path).expect("read reclaimed lock");
        assert_eq!(metadata.pid, std::process::id());
        drop(guard);
    }

    #[test]
    fn dead_same_host_reclaim_token_is_reaped_with_stale_lock() {
        let (_dir, path) = test_lock_path();
        let stale = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &stale);
        let token_path = write_reclaim_token(&path, &stale);

        let guard = acquire_with_config(&path, Some(Duration::from_secs(1)), test_config())
            .expect("dead token must not wedge stale-lock reclamation");
        assert!(!token_path.exists(), "stale reclaim token must be removed");
        drop(guard);
        assert!(!path.exists(), "acquired lock must be released normally");
    }

    #[test]
    fn live_reclaim_token_blocks_and_is_untouched() {
        let (_dir, path) = test_lock_path();
        let stale = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &stale);
        let live = current_process_metadata();
        let token_path = write_reclaim_token(&path, &live);

        let result = acquire_with_config(&path, Some(Duration::ZERO), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(read_lock_metadata(&token_path).expect("live token"), live);
    }

    #[test]
    fn foreign_host_reclaim_token_is_authoritative() {
        let (_dir, path) = test_lock_path();
        let stale = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &stale);
        let mut foreign = stale.clone();
        foreign.hostname = format!("{}-foreign", current_hostname());
        let token_path = write_reclaim_token(&path, &foreign);

        let result = acquire_with_config(&path, Some(Duration::ZERO), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(
            read_lock_metadata(&token_path).expect("foreign token"),
            foreign
        );
    }

    #[test]
    fn malformed_reclaim_token_is_held_until_older_than_sixty_seconds() {
        use filetime::{set_file_mtime, FileTime};

        let (_dir, path) = test_lock_path();
        let stale = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &stale);
        let token_path = reclaim_token_path(&path);
        fs::write(&token_path, b"{ malformed").expect("write malformed token");

        let young_result = acquire_with_config(&path, Some(Duration::ZERO), test_config());
        assert!(matches!(young_result, Err(AcquireError::Timeout)));
        assert_eq!(fs::read(&token_path).expect("young token"), b"{ malformed");

        let old = SystemTime::now()
            .checked_sub(RECLAIM_TOKEN_MALFORMED_STALE_AGE + Duration::from_secs(1))
            .expect("old timestamp");
        set_file_mtime(&token_path, FileTime::from_system_time(old)).expect("age token");
        let guard = acquire_with_config(&path, Some(Duration::from_secs(1)), test_config())
            .expect("old malformed token should be reclaimed");
        assert!(!token_path.exists());
        drop(guard);
    }

    #[test]
    fn held_reclaim_token_logs_blocked_without_claiming_lock_removal() {
        let (_dir, path) = test_lock_path();
        let stale = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &stale);
        let live = current_process_metadata();
        write_reclaim_token(&path, &live);
        take_reclaim_test_logs();

        let result = acquire_with_config(&path, Some(Duration::ZERO), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        let logs = take_reclaim_test_logs();
        assert!(logs.iter().any(|line| {
            line.contains(&format!("reclaim of {} blocked", path.display()))
                && line.contains(&format!("pid {} (alive)", live.pid))
        }));
        assert!(!logs
            .iter()
            .any(|line| line.contains("removing filesystem lock")));
    }

    #[test]
    fn dead_unreclaimable_lock_retry_backoff_stays_bounded() {
        let mut elapsed_ms = 0_u64;
        let mut attempts = 0_u32;
        while elapsed_ms < 60_000 {
            elapsed_ms = elapsed_ms.saturating_add(dead_reclaim_backoff_ms(attempts, 100));
            attempts += 1;
        }

        assert!(
            attempts <= 17,
            "{attempts} retries exceed the one-minute bound"
        );
        assert_eq!(dead_reclaim_backoff_ms(0, 100), 250);
        assert_eq!(dead_reclaim_backoff_ms(20, 100), 5_000);
    }

    #[test]
    fn stale_heartbeat_from_live_pid_blocks() {
        let (_dir, path) = test_lock_path();
        let mut metadata = current_process_metadata();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let identity = process_identity(std::process::id())
                .expect("current process should have a start-time identity");
            assert_eq!(metadata.process_start_time, Some(identity.start_time));
            assert_eq!(metadata.boot_id, identity.boot_id);
        }
        metadata.created_at_ms = now_ms().saturating_sub(60_000);
        metadata.heartbeat_at_ms = now_ms().saturating_sub(60_000);
        write_synthetic_lock(&path, &metadata);

        let result = acquire_with_config(&path, Some(Duration::from_millis(80)), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(read_lock_metadata(&path).expect("read lock"), metadata);

        remove_lock_file(&path).expect("cleanup synthetic lock");
    }

    #[test]
    fn availability_probe_preserves_a_live_owner_with_stale_heartbeat() {
        let (_dir, path) = test_lock_path();
        let mut metadata = current_process_metadata();
        metadata.heartbeat_at_ms = 0;
        write_synthetic_lock(&path, &metadata);
        let original = fs::read(&path).unwrap();
        assert_eq!(
            lock_availability(&path).unwrap(),
            LockAvailability::LiveOwner
        );
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn availability_probe_observes_dead_and_missing_owners_without_reclaiming() {
        let (_dir, path) = test_lock_path();
        assert_eq!(
            lock_availability(&path).unwrap(),
            LockAvailability::Available
        );
        assert!(!path.exists());
        let metadata = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &metadata);
        let original = fs::read(&path).unwrap();
        assert_eq!(lock_availability(&path).unwrap(), LockAvailability::Stale);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn availability_probe_uses_extended_cross_host_heartbeat_policy() {
        let (_dir, path) = test_lock_path();
        let mut metadata = synthetic_metadata(
            999_999_999,
            format!("other-{}", current_hostname()),
            now_ms(),
        );
        write_synthetic_lock(&path, &metadata);
        assert!(
            lock_availability(&path).unwrap() == LockAvailability::LiveOwner,
            "a local PID probe cannot attest a remote owner"
        );
        metadata.heartbeat_at_ms = now_ms().saturating_sub(STALE_HEARTBEAT_MS * 2);
        fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert!(
            lock_availability(&path).unwrap() == LockAvailability::LiveOwner,
            "cross-host reclamation needs the extended threshold"
        );
        metadata.heartbeat_at_ms = now_ms().saturating_sub(STALE_HEARTBEAT_MS * 5 + 1_000);
        fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert_eq!(lock_availability(&path).unwrap(), LockAvailability::Stale);
        assert_eq!(read_lock_metadata(&path).unwrap(), metadata);
    }

    #[test]
    fn availability_probe_does_not_treat_malformed_metadata_as_available() {
        let (_dir, path) = test_lock_path();
        fs::write(&path, b"not-json").unwrap();
        assert_eq!(
            lock_availability(&path).unwrap(),
            LockAvailability::Malformed
        );
        assert_eq!(fs::read(&path).unwrap(), b"not-json");
    }

    #[test]
    fn availability_probe_keeps_io_errors_distinct_and_can_retry() {
        let (_dir, path) = test_lock_path();
        fs::create_dir(&path).unwrap();
        assert!(lock_availability(&path).is_err());
        assert!(path.is_dir());
        fs::remove_dir(&path).unwrap();
        assert_eq!(
            lock_availability(&path).unwrap(),
            LockAvailability::Available
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn live_pid_with_wrong_start_time_is_reclaimed() {
        let (_dir, path) = test_lock_path();
        let mut metadata = current_process_metadata();
        let current_identity = process_identity(std::process::id())
            .expect("current process should have a start-time identity");
        metadata.process_start_time = Some(different_start_time(current_identity.start_time));
        metadata.boot_id = current_identity.boot_id;
        write_synthetic_lock(&path, &metadata);

        let guard = acquire_with_config(&path, Some(Duration::ZERO), test_config())
            .expect("zero-timeout acquire should reclaim a reused PID");
        assert_eq!(guard.metadata.pid, std::process::id());
        assert_eq!(
            guard.metadata.process_start_time,
            Some(current_identity.start_time)
        );
        drop(guard);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_pid_with_wrong_boot_id_is_reclaimed() {
        let (_dir, path) = test_lock_path();
        let mut metadata = current_process_metadata();
        let current_identity = process_identity(std::process::id())
            .expect("current process should have a start-time identity");
        metadata.boot_id = Some(format!("wrong-{}", current_identity.boot_id.unwrap()));
        write_synthetic_lock(&path, &metadata);

        let guard = acquire_with_config(&path, Some(Duration::ZERO), test_config())
            .expect("zero-timeout acquire should reclaim a rebooted PID identity");
        assert_eq!(guard.metadata.pid, std::process::id());
        drop(guard);
    }

    #[test]
    fn legacy_live_pid_lock_keeps_pid_only_liveness() {
        let (_dir, path) = test_lock_path();
        let stale_at = now_ms().saturating_sub(60_000);
        let mut metadata = synthetic_metadata(std::process::id(), current_hostname(), stale_at);
        metadata.heartbeat_at_ms = stale_at;
        let original = write_legacy_lock(&path, &metadata);
        assert!(!original.contains("process_start_time"));
        assert!(!original.contains("boot_id"));

        let result = acquire_with_config(&path, Some(Duration::from_millis(80)), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(
            fs::read_to_string(&path).expect("read legacy lock"),
            original
        );

        remove_lock_file(&path).expect("cleanup legacy lock");
    }

    #[test]
    fn legacy_dead_pid_lock_is_reclaimed() {
        let (_dir, path) = test_lock_path();
        let metadata = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        let original = write_legacy_lock(&path, &metadata);
        assert!(!original.contains("process_start_time"));
        assert!(!original.contains("boot_id"));

        let guard = acquire_with_config(&path, Some(Duration::ZERO), test_config())
            .expect("zero-timeout acquire should reclaim a legacy dead PID lock");
        assert_eq!(guard.metadata.pid, std::process::id());
        drop(guard);
    }

    #[test]
    fn healthy_live_owner_blocks() {
        let (_dir, path) = test_lock_path();
        let metadata = current_process_metadata();
        write_synthetic_lock(&path, &metadata);

        let result = acquire_with_config(&path, Some(Duration::from_millis(80)), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));

        remove_lock_file(&path).expect("cleanup synthetic lock");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn semantic_batch_lease_exited_unreaped_owner_is_dead() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        for legacy in [false, true] {
            let (_root, path) = test_lock_path();
            let mut child = Command::new("sh")
                .args(["-c", "read gate; exit 0"])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let identity = process_identity(child.id()).unwrap();
            let mut metadata = synthetic_metadata(child.id(), current_hostname(), now_ms());
            metadata.process_start_time = Some(identity.start_time);
            metadata.boot_id = identity.boot_id;
            if legacy {
                write_legacy_lock(&path, &metadata);
            } else {
                write_synthetic_lock(&path, &metadata);
            }
            writeln!(child.stdin.take().unwrap(), "exit").unwrap();
            // WNOWAIT proves exit without reaping: kill(pid, 0) can still see
            // this PID, but the process can no longer execute protected work.
            let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            let observed = unsafe {
                libc::waitid(
                    libc::P_PID,
                    libc::id_t::try_from(child.id()).unwrap(),
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            let wait_error = io::Error::last_os_error();
            let exited = process_identity(child.id()).is_some_and(|identity| identity.exited);
            let recovered = try_acquire_once(&path);
            child.wait().unwrap();
            assert_eq!(observed, 0, "waitid failed: {wait_error}");
            assert!(exited, "the native identity probe did not observe exit");
            assert!(
                recovered.is_ok(),
                "an exited unreaped owner blocked recovery (legacy={legacy})"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn semantic_batch_lease_linux_stat_requires_quiescent_zombie() {
        let one =
            "4242 (worker (lease owner)) Z 1 1 1 0 -1 4194304 10 0 0 0 0 0 0 0 20 0 1 0 1234 0";
        assert_eq!(linux_process_stat_identity(one), Some((1234, true)));
        let live_threads = one.replace(" 1 0 1234 ", " 2 0 1234 ");
        assert_eq!(
            linux_process_stat_identity(&live_threads),
            Some((1234, false)),
            "a zombie leader does not prove its worker threads exited"
        );
        assert_eq!(
            linux_process_stat_identity(&one.replace(" 1 0 1234 ", " 0 0 1234 ")),
            Some((1234, false))
        );
        assert_eq!(
            linux_process_stat_identity(&one.replace(") Z ", ") X ")),
            Some((1234, true))
        );
        assert_eq!(
            linux_process_stat_identity(&one.replace(") Z ", ") S ")),
            Some((1234, false))
        );
        assert_eq!(
            linux_process_stat_identity(&one.replace(" 1 0 1234 ", " bad 0 1234 ")),
            None
        );
        assert_eq!(
            linux_process_stat_identity(&one.replace("1234", "bad")),
            None
        );
        assert_eq!(
            linux_process_stat_identity(&one.replace(") Z ", ") bad ")),
            None
        );
    }

    #[test]
    fn malformed_lockfile_is_reclaimed() {
        let (_dir, path) = test_lock_path();
        fs::write(&path, b"not valid json").expect("write malformed lock");

        let guard = acquire_with_config(&path, Some(Duration::from_secs(1)), test_config())
            .expect("reclaim malformed lock");
        let metadata = read_lock_metadata(&path).expect("read reclaimed lock");
        assert_eq!(metadata.pid, std::process::id());
        drop(guard);
    }

    #[test]
    fn cross_host_lock_is_not_stolen_before_extended_stale_threshold() {
        let (_dir, path) = test_lock_path();
        let now = now_ms();
        let mut metadata = current_process_metadata();
        metadata.hostname = format!("{}-other", current_hostname());
        metadata.process_start_time = metadata.process_start_time.map(different_start_time);
        metadata.created_at_ms = now;
        metadata.heartbeat_at_ms = now;
        metadata.writer_epoch = format!("cross-host-{now}");
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert_ne!(
            metadata.process_start_time,
            process_identity(std::process::id()).map(|identity| identity.start_time)
        );
        write_synthetic_lock(&path, &metadata);

        let result = acquire_with_config(&path, Some(Duration::from_millis(80)), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(read_lock_metadata(&path).expect("read lock"), metadata);

        remove_lock_file(&path).expect("cleanup synthetic lock");
    }

    #[test]
    fn stale_cross_host_lock_is_reclaimed_after_extended_threshold() {
        let (_dir, path) = test_lock_path();
        let stale_at =
            now_ms().saturating_sub(test_config().cross_host_stale_heartbeat_ms() + 1_000);
        let mut metadata = current_process_metadata();
        metadata.hostname = format!("{}-other", current_hostname());
        metadata.process_start_time = metadata.process_start_time.map(different_start_time);
        metadata.created_at_ms = stale_at;
        metadata.heartbeat_at_ms = stale_at;
        metadata.writer_epoch = format!("cross-host-{stale_at}");
        write_synthetic_lock(&path, &metadata);

        let guard = acquire_with_config(&path, Some(Duration::from_secs(1)), test_config())
            .expect("reclaim stale cross-host lock");
        let reclaimed = read_lock_metadata(&path).expect("read reclaimed lock");
        assert_eq!(reclaimed.hostname, current_hostname());
        assert_ne!(reclaimed.created_at_ms, metadata.created_at_ms);
        drop(guard);
    }

    #[test]
    fn live_owner_over_10min_warns_but_blocks() {
        let (_dir, path) = test_lock_path();
        let mut metadata = current_process_metadata();
        metadata.created_at_ms = now_ms().saturating_sub(11 * 60 * 1_000);
        metadata.heartbeat_at_ms = now_ms();
        write_synthetic_lock(&path, &metadata);

        let result = acquire_with_config(&path, Some(Duration::from_millis(80)), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(read_lock_metadata(&path).expect("read lock"), metadata);

        remove_lock_file(&path).expect("cleanup synthetic lock");
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct HeartbeatObservation {
        step: usize,
        observed_at_ms: u64,
        heartbeat_at_ms: Option<u64>,
        volume: Option<u64>,
        inode: Option<u64>,
        modified_at_ms: Option<u64>,
        error: Option<String>,
    }

    fn observe_heartbeat(path: &Path, step: usize) -> HeartbeatObservation {
        let mut observation = HeartbeatObservation {
            step,
            observed_at_ms: now_ms(),
            heartbeat_at_ms: None,
            volume: None,
            inode: None,
            modified_at_ms: None,
            error: None,
        };
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) => {
                observation.error = Some(error.to_string());
                return observation;
            }
        };
        // Identity and contents come from the same open, including if the path
        // is atomically replaced while the observer is reading it.
        if let Some(identity) = LeaseFileIdentity::of_file(&file) {
            observation.volume = Some(identity.volume);
            observation.inode = Some(identity.file);
        }
        observation.modified_at_ms = file
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as u64);
        let mut bytes = Vec::new();
        match file.read_to_end(&mut bytes) {
            Ok(_) => match serde_json::from_slice::<LockMetadata>(&bytes) {
                Ok(metadata) => observation.heartbeat_at_ms = Some(metadata.heartbeat_at_ms),
                Err(error) => observation.error = Some(error.to_string()),
            },
            Err(error) => observation.error = Some(error.to_string()),
        }
        observation
    }

    struct HeartbeatHarness {
        child: Child,
        input: Option<ChildStdin>,
        lines: mpsc::Receiver<String>,
        reader: Option<JoinHandle<()>>,
        transcript: Vec<String>,
    }

    impl HeartbeatHarness {
        fn read_observation(
            &mut self,
            prefix: &str,
            deadline: Instant,
        ) -> Result<HeartbeatObservation, String> {
            loop {
                let line = self
                    .lines
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|error| format!("observer response: {error}"))?;
                self.transcript.push(line.clone());
                if let Some(json) = line.strip_prefix(prefix) {
                    return serde_json::from_str(json)
                        .map_err(|error| format!("observer JSON: {error}"));
                }
            }
        }

        fn observe(
            &mut self,
            step: usize,
            deadline: Instant,
        ) -> Result<HeartbeatObservation, String> {
            let input = self.input.as_mut().expect("observer input is open");
            writeln!(input, "{step}").map_err(|error| error.to_string())?;
            input.flush().map_err(|error| error.to_string())?;
            self.read_observation("sample:", deadline)
        }

        fn diagnostics(&self, path: &Path, guard: &LockGuard, step: usize) -> String {
            let held_identity = guard.heartbeat_file.try_lock().ok().and_then(|mut slot| {
                slot.as_mut()
                    .and_then(|lease| LeaseFileIdentity::of_file(lease.file()))
            });
            format!(
                "parent={:?} held_identity={held_identity:?} stopped={} visible={}\nchild observations:\n{}",
                observe_heartbeat(path, step),
                guard.pump.heartbeat_stopped(),
                guard.heartbeat_is_visible(),
                self.transcript.join("\n")
            )
        }

        fn finish(&mut self) -> std::process::ExitStatus {
            self.input.take();
            let status = self.child.wait().expect("observer exit");
            self.reader.take().unwrap().join().expect("observer reader");
            self.transcript.extend(self.lines.try_iter());
            status
        }
    }

    impl Drop for HeartbeatHarness {
        fn drop(&mut self) {
            // An assertion can unwind before normal EOF/wait cleanup. Reap the
            // observer and join its stdout reader before the owner releases the
            // lease, so no child retains the test runner's inherited stderr.
            self.input.take();
            if !matches!(self.child.try_wait(), Ok(Some(_))) {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    /// Run the Rust test binary as an independent lease observer. Each request
    /// opens the lease afresh and returns its content timestamp and file identity.
    fn spawn_test_harness(mode: &str, path: &Path) -> HeartbeatHarness {
        let mut command = std::process::Command::new(std::env::current_exe().expect("test exe"));
        command
            .args(["--exact", HARNESS_TEST, "--nocapture", "--test-threads=1"])
            .env("FS_LOCK_HARNESS_MODE", mode)
            .env("FS_LOCK_HARNESS_PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        for name in [
            "AFT_TEST_REQUIRE_RUST_ANALYZER",
            "AFT_TEST_STORAGE_ROOT",
            "AFT_STORAGE_DIR",
        ] {
            command.env_remove(name);
        }
        let mut child = command.spawn().expect("spawn test harness process");
        let stdin = child.stdin.take().expect("harness stdin");
        let stdout = child.stdout.take().expect("harness stdout");
        let (sender, receiver) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        HeartbeatHarness {
            child,
            input: Some(stdin),
            lines: receiver,
            reader: Some(reader),
            transcript: Vec::new(),
        }
    }

    /// Another process must see background heartbeats through the kept handle.
    /// A beat interval is not a scheduling guarantee: allow progress within the
    /// configured stale bound, and never substitute a parent-side read for the
    /// observer's actual timestamp and inode.
    #[test]
    fn separate_process_sees_heartbeat_advance() {
        if std::env::var_os(HARNESS_TEST).is_some() {
            return;
        }
        assert_separate_process_heartbeat_advances(test_config());
    }

    #[test]
    fn separate_process_waits_for_heartbeat_within_stale_bound() {
        assert_separate_process_heartbeat_advances(LockConfig {
            heartbeat_interval_ms: 400,
            ..test_config()
        });
    }

    fn assert_separate_process_heartbeat_advances(config: LockConfig) {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, config).expect("acquire lock");
        guard.heartbeat_once().expect("initial beat");
        let mut harness = spawn_test_harness(HARNESS_MODE_INSPECT, &path);
        let ready = harness
            .read_observation("ready:", Instant::now() + Duration::from_secs(10))
            .unwrap_or_else(|error| {
                panic!(
                    "observer startup failed: {error}\n{}",
                    harness.diagnostics(&path, &guard, 0)
                )
            });
        let mut previous = ready.heartbeat_at_ms.unwrap_or_else(|| {
            panic!(
                "observer could not read initial lease\n{}",
                harness.diagnostics(&path, &guard, 0)
            )
        });
        for step in 1..=4 {
            let deadline = Instant::now() + Duration::from_millis(config.stale_heartbeat_ms);
            loop {
                let observation = harness.observe(step, deadline).unwrap_or_else(|error| {
                    panic!(
                        "observer step {step} failed: {error}\n{}",
                        harness.diagnostics(&path, &guard, step)
                    )
                });
                if let Some(seen) = observation.heartbeat_at_ms.filter(|seen| *seen > previous) {
                    eprintln!("heartbeat observer advanced: {observation:?}");
                    previous = seen;
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "observer heartbeat did not advance from {previous} within {}ms at step {step}\n{}",
                    config.stale_heartbeat_ms,
                    harness.diagnostics(&path, &guard, step)
                );
                thread::sleep(
                    Duration::from_millis(25)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
        let status = harness.finish();
        assert!(
            status.success(),
            "observer failed: {status:?}\n{}",
            harness.diagnostics(&path, &guard, 4)
        );
    }

    #[cfg(unix)]
    #[test]
    fn separate_process_harness_drop_reaps_observer() {
        let (_dir, path) = test_lock_path();
        write_synthetic_lock(&path, &current_process_metadata());
        let mut harness = spawn_test_harness(HARNESS_MODE_INSPECT, &path);
        harness
            .read_observation("ready:", Instant::now() + Duration::from_secs(10))
            .expect("observer ready");
        let pid = harness.child.id() as libc::pid_t;
        drop(harness);
        let mut status = 0;
        // SAFETY: pid names the child this test just spawned; status is a valid
        // out pointer. A completed Drop must already have waited for this child.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        let error = io::Error::last_os_error().raw_os_error();
        if waited == 0 {
            // SAFETY: only our still-running child can be waitable under pid.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
        }
        assert_eq!(waited, -1, "observer survived harness drop");
        assert_eq!(error, Some(libc::ECHILD), "observer was not reaped");
    }

    /// A replacement by another part of this process must leave the owner's
    /// heartbeat working: writers keep publishing through the same guard, and a
    /// heartbeat that stopped would let another process reclaim the lease after
    /// the stale window while the writer is still running. The test replaces the
    /// lease (which releases the published handle), then opens a fresh handle
    /// and publishes it, and requires the next beat to refresh through it.
    #[test]
    fn replacing_the_lease_keeps_the_next_beat_working_for_the_owner() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire");
        let mut restored = guard.metadata.clone();
        // Same digit count as the live heartbeat, so the serialized lease keeps
        // its byte length; the replacement itself is still an atomic rename.
        restored.heartbeat_at_ms = now_ms().saturating_sub(1_000);
        let fresh = {
            let bytes = lock_metadata_bytes(&restored).expect("serialize");
            with_lease_handles_closed(&path, || {
                replace_lock_bytes_unsynced(&path, &bytes).map(|()| open_lease_for_heartbeat(&path))
            })
            .expect("replace the lease")
            .expect("open the replacement lease")
        };
        // The owner keeps a reference, as `LockGuard` does, so the handle
        // survives until the guard is released.
        let replacement = Arc::new(Mutex::new(Some(fresh)));
        register_lease_slot(guard.pump.slot, &path, &replacement);

        let stale_at = restored.heartbeat_at_ms;
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut advanced = false;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(
                test_config().heartbeat_interval_ms * 2,
            ));
            if read_lock_metadata(&path)
                .map(|meta| meta.heartbeat_at_ms > stale_at)
                .unwrap_or(false)
            {
                advanced = true;
                break;
            }
        }
        assert!(
            advanced,
            "the heartbeat did not refresh the lease through the replacement \
             handle: {:?}",
            read_lock_metadata(&path)
        );
        assert!(
            guard.verify_writer_epoch().expect("verify lease"),
            "writers must still verify their epoch after a replacement"
        );
    }

    /// Body of the separate-process harness this module's tests spawn.
    #[test]
    fn fs_lock_separate_process_harness() {
        let Some(mode) = std::env::var_os("FS_LOCK_HARNESS_MODE") else {
            return;
        };
        let path = PathBuf::from(std::env::var_os("FS_LOCK_HARNESS_PATH").expect("harness path"));
        println!("mode:{mode:?} path={}", path.display());
        match mode.to_string_lossy().as_ref() {
            HARNESS_MODE_INSPECT => {
                println!(
                    "ready:{}",
                    serde_json::to_string(&observe_heartbeat(&path, 0)).unwrap()
                );
                io::stdout().flush().unwrap();
                for line in io::stdin().lock().lines() {
                    let step = line.unwrap().parse().expect("observation step");
                    println!(
                        "sample:{}",
                        serde_json::to_string(&observe_heartbeat(&path, step)).unwrap()
                    );
                    io::stdout().flush().unwrap();
                }
            }
            other => panic!("unknown harness mode {other}"),
        }
    }

    /// A beat reads ownership through the descriptor the heartbeat keeps, not
    /// through the path. When another process reclaims the lease, the rename
    /// gives the path a new file while the kept descriptor still refers to the
    /// old, unlinked one. Reading through it would still see this owner's
    /// identity, so the beat would refresh the orphan and report success while
    /// the live lease went stale. The beat must instead see that the path is no
    /// longer the file it holds and stop.
    #[test]
    fn beat_stops_when_another_owner_replaces_the_lease_file() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire");
        guard
            .heartbeat_once()
            .expect("beat through the kept descriptor");

        // Another process's reclaim: it writes its own lease beside the file and
        // renames it over the path. It cannot see this process's open
        // descriptors, so the heartbeat keeps the old, now unlinked file.
        let mut foreign = current_process_metadata();
        foreign.pid = foreign.pid.wrapping_add(1);
        foreign.created_at_ms = now_ms().saturating_add(1_000);
        foreign.writer_epoch = format!("foreign-{}", now_ms());
        assert!(!lock_identity_matches(&foreign, &guard.metadata));
        let staging = path.with_extension("foreign");
        fs::write(
            &staging,
            lock_metadata_bytes(&foreign).expect("serialize foreign lease"),
        )
        .expect("write the foreign owner's lease");
        rename_over(&staging, &path).expect("foreign reclaim renames over the lease");

        assert!(
            matches!(guard.heartbeat_once(), Err(HeartbeatError::NotOwner)),
            "a beat must not accept the orphaned descriptor it still holds"
        );
        assert_eq!(
            read_lock_metadata(&path).expect("read foreign lease"),
            foreign,
            "a beat must never write over the foreign owner's lease"
        );
    }

    /// Replacing or removing another lease must not cost this lease its kept
    /// descriptor. Every replacement releases the handle it publishes for its
    /// own path; releasing every published handle instead would drop this one
    /// and the next beat would reopen the file, restoring the per-beat
    /// descriptor churn the kept handle exists to remove.
    #[test]
    fn unrelated_lease_churn_keeps_the_kept_descriptor() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mut guards = (0..2)
            .map(|index| {
                acquire_with_config(
                    &dir.path().join(format!("tenant-{index}.lease")),
                    Some(Duration::from_secs(2)),
                    test_config(),
                )
                .expect("acquire")
            })
            .collect::<Vec<_>>();
        let neighbour = dir.path().join("neighbour.lease");

        for _ in 0..3 {
            // Another tenant is acquired, replaced and released while this
            // lease keeps its own descriptor.
            let churn = acquire_with_config(&neighbour, None, test_config()).expect("churn lease");
            let mut restored = churn.metadata.clone();
            restored.heartbeat_at_ms = now_ms().saturating_sub(1_000);
            let bytes = lock_metadata_bytes(&restored).expect("serialize");
            with_lease_handles_closed(&neighbour, || {
                replace_lock_bytes_unsynced(&neighbour, &bytes)
            })
            .expect("replace the neighbour");
            drop(churn);
            let _ = io_ledger::take();
        }

        let _ = io_ledger::take();
        for guard in &guards {
            guard.heartbeat_once().expect("beat");
        }
        let cost = io_ledger::take();
        assert_beat_opens_no_descriptor(&cost);
        guards.clear();
    }

    /// The identity check runs on every beat, so it must not open or close the
    /// lease a second time. Unix needs a stat for it; on Windows the path lookup
    /// takes one short-lived attribute handle, which this ledger only counts if
    /// it went through the heartbeat's own open. What it pins is that the beat
    /// neither reopens the lease for reading nor closes the descriptor it
    /// keeps.
    #[test]
    fn descriptor_identity_check_opens_no_descriptor() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire");
        let _ = io_ledger::take();
        for _ in 0..3 {
            guard.heartbeat_once().expect("beat");
        }
        let ledger = io_ledger::take();
        assert_beat_opens_no_descriptor(&ledger);
    }

    #[test]
    fn stopped_heartbeat_releases_handle_without_unlocking_or_touching_replacement() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = dir.path().join("cache");
        fs::create_dir(&cache).expect("cache dir");
        let path = cache.join("writer.lease");
        let mut guard = acquire_with_config(&path, None, test_config()).expect("acquire lease");
        let _ = io_ledger::take();

        guard.stop_heartbeat_for_removal();
        assert_eq!(io_ledger::take().closes, 1, "close the kept handle now");
        assert!(guard.heartbeat.is_none(), "join before closing the handle");
        assert!(guard.heartbeat_file.lock().unwrap().is_none());
        assert!(!guard
            .verify_writer_epoch()
            .expect("stopped guard is not writable"));
        assert!(matches!(
            guard.pump.heartbeat_once(),
            Err(HeartbeatError::NotPumped)
        ));
        assert!(matches!(
            try_acquire_once(&path),
            Err(AcquireError::Timeout)
        ));
        assert!(lock_identity_matches(
            &read_lock_metadata(&path).expect("lease remains until removal"),
            &guard.metadata
        ));

        // Windows refuses this directory rename while the heartbeat handle is
        // open. Keep the lease on disk until the directory has moved so another
        // writer cannot enter the old cache during retirement.
        let retired = dir.path().join("retired");
        fs::rename(&cache, &retired).expect("retire with the lease guard still alive");
        fs::create_dir(&cache).expect("replacement cache");
        let replacement = acquire_with_config(&path, None, test_config()).expect("new owner");
        let canary = cache.join("canary");
        fs::write(&canary, b"replacement").expect("canary");
        drop(guard);
        assert!(replacement
            .verify_writer_epoch()
            .expect("replacement owns lease"));
        assert_eq!(
            fs::read(canary).expect("replacement preserved"),
            b"replacement"
        );
        fs::remove_dir_all(retired).expect("reap without a lingering lease handle");
    }

    #[test]
    fn drop_stops_heartbeat_thread() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lock");
        drop(guard);

        thread::sleep(Duration::from_millis(
            test_config().heartbeat_interval_ms * 3,
        ));
        assert!(
            !path.exists(),
            "heartbeat recreated or kept updating lockfile"
        );
    }

    #[test]
    fn heartbeat_error_classification_terminal_vs_transient() {
        // Terminal: the lock is provably no longer ours to refresh.
        assert!(heartbeat_error_is_terminal(&HeartbeatError::LockGone));
        assert!(heartbeat_error_is_terminal(&HeartbeatError::NotOwner));
        // Transient: a temporary I/O hiccup or a read that raced a concurrent
        // writer. These must NOT kill the heartbeat — it retries instead.
        assert!(!heartbeat_error_is_terminal(&HeartbeatError::Io(
            io::Error::other("disk blip")
        )));
        let malformed: serde_json::Error =
            serde_json::from_str::<LockMetadata>("not json").unwrap_err();
        assert!(!heartbeat_error_is_terminal(&HeartbeatError::Malformed(
            malformed
        )));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn released_lease_closes_all_descriptors_synchronously() {
        let (dir, path) = test_lock_path();
        let open_under_root = || {
            fs::read_dir("/proc/self/fd")
                .expect("descriptor census")
                .filter_map(Result::ok)
                .filter_map(|entry| fs::read_link(entry.path()).ok())
                .filter(|target| target.starts_with(dir.path()))
                .collect::<Vec<_>>()
        };
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lease");
        let handle = Arc::downgrade(&guard.heartbeat_file);
        assert!(!open_under_root().is_empty(), "live lease keeps its handle");
        drop(guard);
        assert!(
            handle.upgrade().is_none(),
            "heartbeat must release its handle"
        );
        assert!(open_under_root().is_empty(), "no descriptors after release");
    }

    #[test]
    fn malformed_beat_reopens_atomic_replacement_on_next_beat() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(
            &path,
            None,
            LockConfig {
                heartbeat_interval_ms: 60_000,
                ..test_config()
            },
        )
        .expect("acquire lease");
        let live = fs::read(&path).expect("read lease");
        fs::write(&path, vec![b'x'; live.len()]).expect("corrupt lease");

        assert!(matches!(
            guard.pump.heartbeat_once(),
            Err(HeartbeatError::Malformed(_))
        ));
        assert!(guard.heartbeat_file.lock().unwrap().is_none());
        assert!(guard.heartbeat_is_visible());

        let mut restored = guard.metadata.clone();
        restored.heartbeat_at_ms = now_ms().saturating_sub(1_000_000);
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, lock_metadata_bytes(&restored).unwrap()).unwrap();
        rename_over(&replacement, &path).expect("atomic recovery outside the lease registry");
        let _ = io_ledger::take();
        guard
            .pump
            .heartbeat_once()
            .expect("retry through live inode");
        let recovered = read_lock_metadata(&path).expect("independently read recovered lease");
        assert!(lock_identity_matches(&recovered, &guard.metadata));
        assert!(recovered.heartbeat_at_ms > restored.heartbeat_at_ms);
        let recovery = io_ledger::take();
        assert_eq!((recovery.opens, recovery.closes), (1, 0));

        guard.pump.heartbeat_once().expect("healthy next beat");
        assert_beat_opens_no_descriptor(&io_ledger::take());
    }

    #[test]
    fn valid_same_owner_replacement_refreshes_within_stale_bound() {
        let (_dir, path) = test_lock_path();
        let config = test_config();
        let guard = acquire_with_config(
            &path,
            None,
            LockConfig {
                heartbeat_interval_ms: 60_000,
                ..config
            },
        )
        .expect("acquire lease");
        let held_identity = LeaseFileIdentity::of_file(
            guard
                .heartbeat_file
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .file(),
        );
        let mut restored = guard.metadata.clone();
        restored.heartbeat_at_ms = now_ms().saturating_sub(1_000_000);
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, lock_metadata_bytes(&restored).unwrap()).unwrap();
        // Do not release or re-index the kept handle: an atomic replacement by
        // another process cannot notify this process's lease registry either.
        rename_over(&replacement, &path).expect("replace valid same-owner lease");
        let live_identity = LeaseFileIdentity::of_path(&path).expect("replacement identity");
        assert_ne!(held_identity, Some(live_identity));

        let _ = io_ledger::take();
        let started = Instant::now();
        let result = guard.pump.heartbeat_once();
        let elapsed = started.elapsed();
        let on_disk = read_lock_metadata(&path).expect("independent path read");
        let cost = io_ledger::take();
        eprintln!(
            "valid replacement beat: elapsed={elapsed:?} result={result:?} held={held_identity:?} live={live_identity:?} sentinel={} observed={} cost={cost:?}",
            restored.heartbeat_at_ms, on_disk.heartbeat_at_ms
        );
        assert!(
            elapsed < Duration::from_millis(config.stale_heartbeat_ms),
            "same-owner replacement delayed a beat beyond its stale bound: {elapsed:?}"
        );
        result.expect("refresh replacement through reopened handle");
        assert!(on_disk.heartbeat_at_ms > restored.heartbeat_at_ms);
        assert!(lock_identity_matches(&on_disk, &guard.metadata));
        assert_eq!((cost.opens, cost.closes), (1, 1));
        let reopened_identity = LeaseFileIdentity::of_file(
            guard
                .heartbeat_file
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .file(),
        );
        assert_eq!(reopened_identity, Some(live_identity));
        guard.pump.heartbeat_once().expect("healthy next beat");
        assert_beat_opens_no_descriptor(&io_ledger::take());
    }

    #[test]
    fn heartbeat_survives_transient_malformed_and_recovers() {
        // Regression: a single transient failure (e.g. a read that races a
        // concurrent writer and sees a momentarily-unparseable file) used to
        // permanently kill the heartbeat thread. The guard holder would then
        // run its critical section with a stale heartbeat_at_ms, letting
        // another process reclaim the lock after the stale window — concurrent
        // writers / split-brain. The heartbeat must instead retry and resume
        // refreshing once the file is readable again.
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lock");
        let owner = guard.metadata.clone();

        // Corrupt the lockfile out from under the heartbeat (simulates a
        // concurrent-writer race producing a momentarily-unparseable read).
        // The heartbeat reads-then-writes, so it observes Malformed and, with
        // the fix, retries instead of dying. The corruption keeps the lease's
        // byte length: a length change takes the process-wide replacement path,
        // which releases the beat's kept handle and leaves the ownership check
        // to the next open, and this test is about retrying a read that failed.
        let live = fs::read(&path).expect("read live lease");
        fs::write(&path, vec![b'x'; live.len()]).expect("corrupt lockfile");

        // Give the heartbeat several intervals to observe the malformed file.
        // Pre-fix, the thread is dead by now.
        thread::sleep(Duration::from_millis(
            test_config().heartbeat_interval_ms * 4,
        ));

        // Restore the owner's own lease bytes with a clearly-stale heartbeat
        // timestamp. The replacement bytes keep the lease's byte length, so the
        // next beat takes its in-place path. The restore's timestamp only has to
        // be older than the beat that follows, so a value from before the
        // corruption is enough.
        let mut restored = owner.clone();
        restored.heartbeat_at_ms = now_ms().saturating_sub(1_000_000);
        let sentinel = restored.heartbeat_at_ms;
        // Use an atomic replacement to exercise reopening the lease, rather
        // than only rewriting the inode that the heartbeat previously held.
        {
            let bytes = lock_metadata_bytes(&restored).expect("serialize restored metadata");
            with_lease_handles_closed(&path, || replace_lock_bytes_unsynced(&path, &bytes))
                .expect("restore lock metadata");
        }

        // If the heartbeat thread is still alive (the fix), it will overwrite
        // heartbeat_at_ms with a current value. Poll for that recovery. The
        // window covers the beat that finds the restored lease but is still
        // failing on the earlier corrupt state, plus several retries after it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut recovered = false;
        while std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
            match read_lock_metadata(&path) {
                Ok(meta)
                    if meta.created_at_ms == owner.created_at_ms
                        && meta.heartbeat_at_ms > sentinel =>
                {
                    recovered = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(
            recovered,
            "heartbeat did not recover after a transient malformed read — thread likely died: \
             stopped={} visible={} on_disk={:?}",
            guard.pump.heartbeat_stopped(),
            guard.heartbeat_is_visible(),
            read_lock_metadata(&path)
        );
        drop(guard);
    }
}
