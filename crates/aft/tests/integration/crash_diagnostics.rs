//! Fatal-signal diagnostics for integration tests that otherwise exit without a panic.

#[cfg(unix)]
mod unix {
    use std::ffi::c_void;
    use std::sync::Once;

    const ENABLE_ENV: &str = "AFT_CAPTURE_CRASH_DIAGNOSTICS";
    const CHILD_ENV: &str = "AFT_CRASH_DIAGNOSTIC_TEST_CHILD";
    const PREFIX: &[u8] = b"\nAFT integration fatal signal: signal=";
    const ADDRESS: &[u8] = b" fault_address=";
    const THREAD: &[u8] = b" native_thread=";
    const BACKTRACE: &[u8] = b"\nAFT integration fatal signal backtrace:\n";

    static INSTALL: Once = Once::new();

    unsafe extern "C" {
        fn backtrace(frames: *mut *mut c_void, capacity: libc::c_int) -> libc::c_int;
        fn backtrace_symbols_fd(
            frames: *const *mut c_void,
            frame_count: libc::c_int,
            fd: libc::c_int,
        );
    }

    pub(super) fn install() {
        if std::env::var_os(ENABLE_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
            return;
        }
        INSTALL.call_once(|| unsafe {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            aft::db::shm_diagnostics::install();
            install_for(libc::SIGBUS);
            install_for(libc::SIGSEGV);
        });
    }

    unsafe fn install_for(signal: libc::c_int) {
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        action.sa_sigaction = fatal_signal_handler as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESETHAND;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            assert_eq!(libc::sigaction(signal, &action, std::ptr::null_mut()), 0);
        }
    }

    unsafe extern "C" fn fatal_signal_handler(
        signal: libc::c_int,
        info: *mut libc::siginfo_t,
        _context: *mut c_void,
    ) {
        // The scalar fields use only stack storage and async-signal-safe writes.
        // backtrace/backtrace_symbols_fd are best-effort diagnostics rather than
        // POSIX async-signal-safe calls, so the gate also retains a core-dump
        // path if unwinding cannot make progress. SA_RESETHAND plus re-raising
        // preserves the original fatal signal after diagnostics are emitted.
        unsafe {
            write_bytes(PREFIX);
            write_decimal(signal);
            write_bytes(ADDRESS);
            let address = info
                .as_ref()
                .map(|details| details.si_addr() as usize)
                .unwrap_or_default();
            write_hex(address);
            write_bytes(THREAD);
            // Synchronous SIGBUS/SIGSEGV delivery runs this handler on the
            // faulting thread, so this native handle identifies whose stack
            // backtrace_symbols_fd prints below.
            write_hex(libc::pthread_self() as usize);
            write_bytes(BACKTRACE);

            let mut frames = [std::ptr::null_mut(); 128];
            let frame_count = backtrace(frames.as_mut_ptr(), frames.len() as libc::c_int);
            if frame_count > 0 {
                backtrace_symbols_fd(frames.as_ptr(), frame_count, libc::STDERR_FILENO);
            }

            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    unsafe fn write_bytes(bytes: &[u8]) {
        unsafe {
            libc::write(
                libc::STDERR_FILENO,
                bytes.as_ptr().cast::<c_void>(),
                bytes.len(),
            );
        }
    }

    unsafe fn write_decimal(value: libc::c_int) {
        let mut bytes = [0_u8; 12];
        let mut cursor = bytes.len();
        let mut value = value.unsigned_abs();
        loop {
            cursor -= 1;
            bytes[cursor] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        unsafe { write_bytes(&bytes[cursor..]) };
    }

    unsafe fn write_hex(mut value: usize) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut bytes = [0_u8; 2 + usize::BITS as usize / 4];
        let mut cursor = bytes.len();
        loop {
            cursor -= 1;
            bytes[cursor] = DIGITS[value & 0xf];
            value >>= 4;
            if value == 0 {
                break;
            }
        }
        cursor -= 2;
        bytes[cursor..cursor + 2].copy_from_slice(b"0x");
        unsafe { write_bytes(&bytes[cursor..]) };
    }

    #[test]
    fn fatal_signal_handler_emits_fault_context() {
        use std::os::unix::process::ExitStatusExt;

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crash_diagnostics::unix::fatal_signal_diagnostic_child",
                "--nocapture",
            ])
            .env(ENABLE_ENV, "1")
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();

        assert_eq!(output.status.signal(), Some(libc::SIGBUS));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("AFT integration fatal signal: signal=")
                && stderr.contains(" fault_address=0x")
                && stderr.contains(" native_thread=0x")
                && stderr.contains("AFT integration fatal signal backtrace:"),
            "fatal-signal context missing from stderr: {stderr}"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sqlite_shm_trace_names_the_actual_truncated_inode_and_vfs_site() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crash_diagnostics::unix::sqlite_shm_trace_child",
                "--nocapture",
            ])
            .env(ENABLE_ENV, "1")
            .env("AFT_SQLITE_SHM_TRACE_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        let expected = stderr
            .lines()
            .find_map(|line| line.strip_prefix("EXPECTED_SHM_INODE="))
            .unwrap();
        // Darwin keeps three bytes when resetting the dead-man-switch file; other Unix
        // builds truncate to zero. Both invalidate any already-mapped page.
        let reset_size = if cfg!(target_os = "macos") { 3 } else { 0 };
        let before = stderr
            .lines()
            .find(|line| {
                line.contains("event=ftruncate_before")
                    && line.contains("site=xShmMap/unixShmMap")
                    && line.contains(&format!(" arg={reset_size} "))
                    && line.contains(" file_size=65536")
            })
            .expect(
                "DMS truncation must be observed inside unixShmMap, not inferred from xTruncate",
            );
        assert!(before.contains(&format!(" fd_ino={expected} ")), "{before}");
        assert!(
            before.contains(" path=")
                && before.contains("trace.sqlite")
                && before.contains(" region=0 map_size=32768"),
            "{before}"
        );
        assert!(stderr
            .lines()
            .any(|line| line.contains("event=ftruncate_after")
                && line.contains(&format!(" fd_ino={expected} "))
                && line.contains(&format!(" file_size={reset_size}"))));
        assert!(stderr.contains("site=xShmUnmap/unixShmUnmap"));
        assert!(stderr.contains("site=xTruncate/unixTruncate"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sqlite_shm_trace_child() {
        use std::os::unix::fs::MetadataExt;
        if std::env::var_os("AFT_SQLITE_SHM_TRACE_CHILD").is_none() {
            return;
        }
        install();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.sqlite");
        let connection = rusqlite::Connection::open(&path).unwrap();
        let shm = std::fs::File::create(dir.path().join("trace.sqlite-shm")).unwrap();
        shm.set_len(65536).unwrap();
        eprintln!("EXPECTED_SHM_INODE={}", shm.metadata().unwrap().ino());
        connection
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x); INSERT INTO t VALUES(1);")
            .unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(connection);
    }

    #[test]
    fn fatal_signal_diagnostic_child() {
        if std::env::var_os(CHILD_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
            return;
        }

        let no_core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        unsafe {
            libc::setrlimit(libc::RLIMIT_CORE, &no_core);
        }
        install();
        unsafe {
            libc::raise(libc::SIGBUS);
        }
        panic!("SIGBUS diagnostic handler returned");
    }
}

pub(crate) fn install() {
    #[cfg(unix)]
    unix::install();
}
