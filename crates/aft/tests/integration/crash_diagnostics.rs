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
        context: *mut c_void,
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
            write_bytes(b" pc=");
            write_hex(fault_pc(context));
            write_bytes(BACKTRACE);

            let mut frames = [std::ptr::null_mut(); 128];
            let frame_count = backtrace(frames.as_mut_ptr(), frames.len() as libc::c_int);
            if frame_count > 0 {
                for frame in &frames[..frame_count as usize] {
                    write_bytes(b"AFT raw frame ip=");
                    write_hex(*frame as usize);
                    write_bytes(b"\n");
                }
                backtrace_symbols_fd(frames.as_ptr(), frame_count, libc::STDERR_FILENO);
            }

            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    unsafe fn fault_pc(context: *mut c_void) -> usize {
        if context.is_null() {
            return 0;
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        unsafe {
            return (*(*(context.cast::<libc::ucontext_t>())).uc_mcontext)
                .__ss
                .__pc as usize;
        }
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        unsafe {
            return (*(*(context.cast::<libc::ucontext_t>())).uc_mcontext)
                .__ss
                .__rip as usize;
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        unsafe {
            return (*(context.cast::<libc::ucontext_t>())).uc_mcontext.gregs[libc::REG_RIP as usize]
                as usize;
        }
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        unsafe {
            return (*(context.cast::<libc::ucontext_t>())).uc_mcontext.pc as usize;
        }
        #[allow(unreachable_code)]
        0
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
                && stderr.contains(" pc=0x")
                && stderr.contains("AFT raw frame ip=0x")
                && stderr.contains("AFT integration fatal signal backtrace:"),
            "fatal-signal context missing from stderr: {stderr}"
        );
        assert!(
            stderr.lines().any(|line| {
                line.contains("integration-") && !line.contains("AFT integration fatal signal")
            }),
            "backtrace_symbols_fd emitted no symbolized frame: {stderr}"
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
        // SQLite resets the dead-man-switch file to its three-byte initial header.
        // That invalidates every previously mapped page beyond the new end.
        let reset_size = 3;
        let before = stderr
            .lines()
            .find(|line| {
                line.contains("event=ftruncate_before")
                    && line.contains("site=xShmMap/unixShmMap")
                    && line.contains(&format!(" arg={reset_size} "))
            })
            .unwrap_or_else(|| {
                panic!(
                    "DMS truncation must be observed inside unixShmMap, not inferred from xTruncate: {stderr}"
                )
            });
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
        let _mapped = stderr
            .lines()
            .find(|line| {
                line.contains("event=leave")
                    && line.contains("site=xShmMap/unixShmMap")
                    && line.contains(" region=0 map_size=32768")
                    && line.contains(" map=0x")
                    && !line.contains(" map=0x0")
            })
            .expect("a successful xShmMap leave must name its returned mapping");
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn lock_probe_command(role: &str, path: &std::path::Path) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "crash_diagnostics::unix::sqlite_lock_probe_child",
                "--nocapture",
            ])
            .env("AFT_SQLITE_LOCK_ROLE", role)
            .env("AFT_SQLITE_LOCK_PATH", path)
            .env(ENABLE_ENV, "1");
        command
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sqlite_extra_shm_close_loses_dms_lock_and_faults() {
        use std::os::unix::process::ExitStatusExt;
        let dir = tempfile::tempdir().unwrap();
        let output = lock_probe_command("extra-close", &dir.path().join("probe.sqlite"))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprint!("{stderr}");
        assert!(stderr.contains("DMS_LOCK=free"), "{stderr}");
        assert!(stderr.contains("DMS_RESET=done"), "{stderr}");
        assert_eq!(output.status.signal(), Some(libc::SIGBUS), "{stderr}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sqlite_without_extra_close_keeps_dms_lock_and_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let output = lock_probe_command("control", &dir.path().join("probe.sqlite"))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprint!("{stderr}");
        assert!(output.status.success(), "{stderr}");
        assert!(stderr.contains("DMS_LOCK=held"), "{stderr}");
        assert!(stderr.contains("DMS_WRITE=EAGAIN"), "{stderr}");
        assert!(stderr.contains("MAPPING_SURVIVED"), "{stderr}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sqlite_credit_hook_keeps_dms_read_lock() {
        let dir = tempfile::tempdir().unwrap();
        let output = lock_probe_command("tracked", &dir.path().join("probe.sqlite"))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprint!("{stderr}");
        assert!(output.status.success(), "{stderr}");
        assert!(
            stderr.contains("DMS_LOCK=held"),
            "credit hook released DMS: {stderr}"
        );
        assert!(stderr.contains("DMS_WRITE=EAGAIN"), "{stderr}");
    }

    /// Closing one connection must not release the locks its siblings rely on.
    ///
    /// The credit-hook role above proves the commit path keeps the dead-man
    /// switch; it says nothing about close, which is where the original defect
    /// lived and where a reopened `-shm` costs this process every lock it holds
    /// on that inode.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sqlite_connection_close_keeps_a_sibling_connections_dms_lock() {
        let dir = tempfile::tempdir().unwrap();
        let output = lock_probe_command("tracked-close", &dir.path().join("probe.sqlite"))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprint!("{stderr}");
        assert!(output.status.success(), "{stderr}");
        assert!(
            stderr.contains("CLOSE_SEAM=done"),
            "fixture never closed a connection: {stderr}"
        );
        assert!(
            stderr.contains("DMS_LOCK=held"),
            "closing a connection released a sibling's dead-man-switch lock: {stderr}"
        );
        assert!(stderr.contains("DMS_WRITE=EAGAIN"), "{stderr}");
        assert!(stderr.contains("MAPPING_SURVIVED"), "{stderr}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    unsafe fn main_file(connection: &rusqlite::Connection) -> *mut rusqlite::ffi::sqlite3_file {
        let mut file: *mut rusqlite::ffi::sqlite3_file = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                rusqlite::ffi::sqlite3_file_control(
                    connection.handle(),
                    c"main".as_ptr(),
                    rusqlite::ffi::SQLITE_FCNTL_FILE_POINTER,
                    std::ptr::from_mut(&mut file).cast(),
                )
            },
            rusqlite::ffi::SQLITE_OK
        );
        file
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sqlite_lock_probe_child() {
        use std::io::{BufRead, Write};
        use std::os::fd::AsRawFd;
        let Ok(role) = std::env::var("AFT_SQLITE_LOCK_ROLE") else {
            return;
        };
        let path = std::path::PathBuf::from(std::env::var_os("AFT_SQLITE_LOCK_PATH").unwrap());
        install();
        unsafe {
            let no_core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &no_core);
        }
        if role == "observer" || role == "resetter" {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!("{}-shm", path.display()))
                .unwrap();
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = libc::F_WRLCK as _;
            lock.l_whence = libc::SEEK_SET as _;
            lock.l_start = 128; // SQLite's Unix dead-man-switch byte.
            lock.l_len = 1;
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) },
                0
            );
            let held = lock.l_type != libc::F_UNLCK as libc::c_short;
            eprintln!("DMS_LOCK={}", if held { "held" } else { "free" });
            if held {
                lock.l_type = libc::F_WRLCK as _;
                assert_eq!(
                    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) },
                    -1
                );
                let error = std::io::Error::last_os_error().raw_os_error().unwrap();
                assert!(error == libc::EAGAIN || error == libc::EACCES);
                eprintln!("DMS_WRITE=EAGAIN");
            }
            drop(file);
            if role == "resetter" {
                let connection = rusqlite::Connection::open(&path).unwrap();
                unsafe {
                    let file = main_file(&connection);
                    let mut mapping = std::ptr::null_mut();
                    assert_eq!(
                        ((*(*file).pMethods).xShmMap.unwrap())(file, 0, 32768, 0, &mut mapping),
                        0
                    );
                }
                // No extension or recovery: leave SQLite's DMS reset observable
                // while the first process still owns its original mapping.
                eprintln!("DMS_RESET=done");
                println!("RESET_READY");
                std::io::stdout().flush().unwrap();
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                drop(connection);
            }
            return;
        }
        if role == "tracked" {
            let connection =
                aft::db::TrackedConnection::open(&path, aft::db::SqliteStore::AftDb).unwrap();
            connection.set_wal_autocheckpoint(1).unwrap();
            connection
                .execute_batch(
                    "PRAGMA journal_mode=WAL; CREATE TABLE t(value); INSERT INTO t VALUES(42);",
                )
                .unwrap();
            let output = lock_probe_command("observer", &path).output().unwrap();
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(output.status.success());
            assert_eq!(
                connection
                    .query_row("SELECT value FROM t", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                42
            );
            eprintln!("MAPPING_SURVIVED");
            return;
        }
        if role == "tracked-close" {
            // The close seam, which the commit-time roles never reach: one process
            // holding two connections, the first closing while the second still
            // has its WAL-index mapped. A second descriptor opened anywhere in
            // that close drops THIS PROCESS's advisory locks on the -shm inode,
            // including the dead-man-switch byte the surviving connection needs,
            // and the observer then finds it free. This is the production shape:
            // the daemon holds many connections per database and closes them
            // continuously.
            let closing =
                aft::db::TrackedConnection::open(&path, aft::db::SqliteStore::AftDb).unwrap();
            closing.set_wal_autocheckpoint(1).unwrap();
            closing
                .execute_batch(
                    "PRAGMA journal_mode=WAL; CREATE TABLE t(value); INSERT INTO t VALUES(42);",
                )
                .unwrap();
            let surviving =
                aft::db::TrackedConnection::open(&path, aft::db::SqliteStore::AftDb).unwrap();
            assert_eq!(
                surviving
                    .query_row("SELECT value FROM t", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                42
            );
            drop(closing);
            eprintln!("CLOSE_SEAM=done");
            let output = lock_probe_command("observer", &path).output().unwrap();
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(output.status.success());
            assert_eq!(
                surviving
                    .query_row("SELECT value FROM t", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                42
            );
            eprintln!("MAPPING_SURVIVED");
            return;
        }
        let raw = rusqlite::Connection::open(&path).unwrap();
        let connection = &raw;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE t(value); INSERT INTO t VALUES(42); BEGIN;",
            )
            .unwrap();
        assert_eq!(
            connection
                .query_row("SELECT value FROM t", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            42
        );
        let mapping = unsafe {
            let file = main_file(connection);
            let mut mapping = std::ptr::null_mut();
            assert_eq!(
                ((*(*file).pMethods).xShmMap.unwrap())(file, 0, 32768, 0, &mut mapping),
                rusqlite::ffi::SQLITE_OK
            );
            assert!(!mapping.is_null());
            mapping
        };
        if role == "extra-close" {
            drop(std::fs::File::open(format!("{}-shm", path.display())).unwrap());
        }
        if role != "extra-close" {
            let output = lock_probe_command("observer", &path).output().unwrap();
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(output.status.success());
            // The failed exclusive probe must leave the already mapped page readable.
            unsafe {
                std::ptr::read_volatile(mapping.cast::<u8>().add(4096));
            }
        } else {
            let mut child = lock_probe_command("resetter", &path)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                assert!(
                    stdout.read_line(&mut line).unwrap() > 0,
                    "resetter ended before reset"
                );
                if line.contains("RESET_READY") {
                    break;
                }
            }
            // This is the mapping obtained before the extra descriptor was closed.
            // A volatile touch beyond Darwin's three retained bytes proves SIGBUS.
            unsafe {
                std::ptr::read_volatile(mapping.cast::<u8>().add(4096));
            }
            let _ = child.stdin.take();
            child.wait().unwrap();
        }
        assert_eq!(
            connection
                .query_row("SELECT value FROM t", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            42
        );
        eprintln!("MAPPING_SURVIVED");
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
