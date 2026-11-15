//! Durable flushes shared by the index store and AFT's other file writers.
//!
//! macOS uses fsync by default. Opt-in full drive flushes fall back to fsync on
//! mounts without F_FULLFSYNC support; other errors remain fatal.

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

static MACOS_FULL_SYNC: AtomicBool = AtomicBool::new(false);

/// Select full drive flushes for subsequent macOS syncs. Other platforms ignore it.
pub fn set_macos_full_sync(enabled: bool) {
    MACOS_FULL_SYNC.store(enabled, Ordering::Relaxed);
}

/// The requested macOS policy, even on other platforms. This read is lock-free.
pub fn macos_full_sync() -> bool {
    MACOS_FULL_SYNC.load(Ordering::Relaxed)
}

/// The active sync policy, for runtime diagnostics.
pub fn mode() -> &'static str {
    if cfg!(target_os = "macos") && macos_full_sync() {
        "full_sync"
    } else {
        "fsync"
    }
}

/// Flush file contents and metadata, using the configured macOS sync policy.
pub fn sync_all(file: &File, path: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::sync_all(file, path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        file.sync_all()
    }
}

/// Flush from a panic hook without entering the logger or its warning-set lock.
/// Unsupported full syncs are still counted and fall back to fsync, but do not
/// warn or mark the device as warned by an ordinary caller.
pub fn sync_all_in_panic(file: &File, path: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::sync_all_inner(file, path, false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        file.sync_all()
    }
}

/// Flush file contents, using the configured macOS policy and native semantics elsewhere.
pub fn sync_data(file: &File, path: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // Rust uses F_FULLFSYNC for both sync_all and sync_data on macOS.
        macos::sync_all(file, path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        file.sync_data()
    }
}

/// Number of unsupported full-sync calls that attempted the fsync fallback.
pub fn full_sync_fallbacks() -> u64 {
    #[cfg(target_os = "macos")]
    {
        macos::DIAGNOSTICS
            .fallbacks
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::collections::HashSet;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::AtomicU64;
    use std::sync::{LazyLock, Mutex};

    #[derive(Default)]
    pub(super) struct Diagnostics {
        pub(super) fallbacks: AtomicU64,
        warned_devices: LazyLock<Mutex<HashSet<u64>>>,
    }

    pub(super) static DIAGNOSTICS: Diagnostics = Diagnostics {
        fallbacks: AtomicU64::new(0),
        warned_devices: LazyLock::new(Mutex::default),
    };

    pub(super) fn sync_all(file: &File, path: &Path) -> io::Result<()> {
        sync_all_inner(file, path, true)
    }

    pub(super) fn sync_all_inner(file: &File, path: &Path, warn: bool) -> io::Result<()> {
        sync_all_with(
            file,
            path,
            &DIAGNOSTICS,
            &MACOS_FULL_SYNC,
            || {
                // SAFETY: the borrowed File keeps its descriptor open.
                retry(|| unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) })
            },
            || fsync(file),
            warn,
        )
    }

    fn fsync(file: &File) -> io::Result<()> {
        // SAFETY: the borrowed File keeps its descriptor open.
        retry(|| unsafe { libc::fsync(file.as_raw_fd()) })
    }

    fn retry(mut syscall: impl FnMut() -> libc::c_int) -> io::Result<()> {
        loop {
            if syscall() == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    // Inject the switch and syscalls so tests can change policy without racing
    // other file writers, while keeping production's fallback and diagnostics.
    fn sync_all_with(
        file: &File,
        path: &Path,
        diagnostics: &Diagnostics,
        full_sync_enabled: &AtomicBool,
        full_sync: impl FnOnce() -> io::Result<()>,
        fallback: impl FnOnce() -> io::Result<()>,
        warn: bool,
    ) -> io::Result<()> {
        if !full_sync_enabled.load(Ordering::Relaxed) {
            return fallback();
        }
        match full_sync() {
            Ok(()) => Ok(()),
            Err(error)
                if error.raw_os_error().is_some_and(|errno| {
                    [libc::ENOTTY, libc::ENOTSUP, libc::EINVAL].contains(&errno)
                }) =>
            {
                diagnostics.fallbacks.fetch_add(1, Ordering::Relaxed);
                // Device lookup is diagnostic only; it must not prevent fsync.
                if warn && let Ok(metadata) = file.metadata() {
                    let device = metadata.dev();
                    let first = diagnostics
                        .warned_devices
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .insert(device);
                    if first {
                        log::warn!(
                            "F_FULLFSYNC unsupported; falling back to fsync: path={} errno={} device={device}",
                            path.display(),
                            error.raw_os_error().expect("matched an errno"),
                        );
                    }
                }
                fallback()
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::cell::{Cell, RefCell};
        use std::sync::Once;

        struct TestLogger;
        static LOGGER: TestLogger = TestLogger;
        static INIT: Once = Once::new();
        thread_local! {
            static WARNINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        }

        impl log::Log for TestLogger {
            fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
                metadata.level() == log::Level::Warn
            }

            fn log(&self, record: &log::Record<'_>) {
                if self.enabled(record.metadata()) {
                    WARNINGS.with(|warnings| warnings.borrow_mut().push(record.args().to_string()));
                }
            }

            fn flush(&self) {}
        }

        fn diagnostics() -> Diagnostics {
            INIT.call_once(|| {
                log::set_logger(&LOGGER).expect("install test logger");
                log::set_max_level(log::LevelFilter::Warn);
            });
            WARNINGS.with(|warnings| warnings.borrow_mut().clear());
            Diagnostics::default()
        }

        fn injected(
            file: &File,
            path: &Path,
            diagnostics: &Diagnostics,
            errno: i32,
            fsyncs: &Cell<usize>,
        ) -> io::Result<()> {
            sync_all_with(
                file,
                path,
                diagnostics,
                &AtomicBool::new(true),
                || Err(io::Error::from_raw_os_error(errno)),
                || {
                    fsyncs.set(fsyncs.get() + 1);
                    fsync(file)
                },
                true,
            )
        }

        #[test]
        fn full_sync_enotty_falls_back_and_counts() {
            let diagnostics = diagnostics();
            let file = tempfile::NamedTempFile::new().unwrap();
            let fsyncs = Cell::new(0);
            injected(
                file.as_file(),
                file.path(),
                &diagnostics,
                libc::ENOTTY,
                &fsyncs,
            )
            .unwrap();
            assert_eq!(fsyncs.get(), 1);
            assert_eq!(diagnostics.fallbacks.load(Ordering::Relaxed), 1);
            WARNINGS.with(|warnings| {
                let warnings = warnings.borrow();
                assert_eq!(warnings.len(), 1);
                assert!(warnings[0].contains(&file.path().display().to_string()));
                assert!(warnings[0].contains(&format!("errno={}", libc::ENOTTY)));
            });
        }

        #[test]
        fn full_sync_enotsup_and_einval_fall_back_for_directories() {
            let diagnostics = diagnostics();
            let dir = tempfile::tempdir().unwrap();
            let file = File::open(dir.path()).unwrap();
            let fsyncs = Cell::new(0);
            for errno in [libc::ENOTSUP, libc::EINVAL] {
                injected(&file, dir.path(), &diagnostics, errno, &fsyncs).unwrap();
            }
            assert_eq!(fsyncs.get(), 2);
            assert_eq!(diagnostics.fallbacks.load(Ordering::Relaxed), 2);
        }

        #[test]
        fn full_sync_eio_still_fails_without_fallback() {
            let diagnostics = diagnostics();
            let file = tempfile::NamedTempFile::new().unwrap();
            let fsyncs = Cell::new(0);
            let error = injected(
                file.as_file(),
                file.path(),
                &diagnostics,
                libc::EIO,
                &fsyncs,
            )
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EIO));
            assert_eq!(fsyncs.get(), 0);
            assert_eq!(diagnostics.fallbacks.load(Ordering::Relaxed), 0);
            WARNINGS.with(|warnings| assert!(warnings.borrow().is_empty()));
        }

        #[test]
        fn full_sync_second_fallback_on_same_device_warns_once() {
            let diagnostics = diagnostics();
            let first = tempfile::NamedTempFile::new().unwrap();
            let second = tempfile::NamedTempFile::new().unwrap();
            assert_eq!(
                first.as_file().metadata().unwrap().dev(),
                second.as_file().metadata().unwrap().dev()
            );
            let fsyncs = Cell::new(0);
            for file in [&first, &second] {
                injected(
                    file.as_file(),
                    file.path(),
                    &diagnostics,
                    libc::ENOTTY,
                    &fsyncs,
                )
                .unwrap();
            }
            assert_eq!(fsyncs.get(), 2);
            assert_eq!(diagnostics.fallbacks.load(Ordering::Relaxed), 2);
            WARNINGS.with(|warnings| assert_eq!(warnings.borrow().len(), 1));
        }

        #[test]
        fn full_sync_fallback_failure_is_propagated() {
            let diagnostics = diagnostics();
            let file = tempfile::NamedTempFile::new().unwrap();
            let error = sync_all_with(
                file.as_file(),
                file.path(),
                &diagnostics,
                &AtomicBool::new(true),
                || Err(io::Error::from_raw_os_error(libc::ENOTTY)),
                || Err(io::Error::from_raw_os_error(libc::EIO)),
                true,
            )
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EIO));
            assert_eq!(diagnostics.fallbacks.load(Ordering::Relaxed), 1);
        }

        #[test]
        fn full_sync_success_does_not_fall_back() {
            let diagnostics = diagnostics();
            let file = tempfile::NamedTempFile::new().unwrap();
            sync_all_with(
                file.as_file(),
                file.path(),
                &diagnostics,
                &AtomicBool::new(true),
                || Ok(()),
                || panic!("successful full sync must not fall back"),
                true,
            )
            .unwrap();
            assert_eq!(diagnostics.fallbacks.load(Ordering::Relaxed), 0);
            WARNINGS.with(|warnings| assert!(warnings.borrow().is_empty()));
        }

        #[test]
        fn full_sync_panic_fallback_skips_warning_lock() {
            let diagnostics = diagnostics();
            let file = tempfile::NamedTempFile::new().unwrap();
            let fsyncs = Cell::new(0);
            let warned = diagnostics.warned_devices.lock().unwrap();
            sync_all_with(
                file.as_file(),
                file.path(),
                &diagnostics,
                &AtomicBool::new(true),
                || Err(io::Error::from_raw_os_error(libc::ENOTTY)),
                || {
                    fsyncs.set(fsyncs.get() + 1);
                    fsync(file.as_file())
                },
                false,
            )
            .unwrap();
            assert_eq!(fsyncs.get(), 1);
            assert_eq!(diagnostics.fallbacks.load(Ordering::Relaxed), 1);
            assert!(warned.is_empty());
            WARNINGS.with(|warnings| assert!(warnings.borrow().is_empty()));
        }

        #[test]
        fn sync_policy_defaults_to_fsync_and_changes_on_the_next_call() {
            let diagnostics = diagnostics();
            let file = tempfile::NamedTempFile::new().unwrap();
            assert!(!macos_full_sync());
            assert_eq!(mode(), "fsync");
            let full_syncs = Cell::new(0);
            let fsyncs = Cell::new(0);
            let sync = || {
                sync_all_with(
                    file.as_file(),
                    file.path(),
                    &diagnostics,
                    &MACOS_FULL_SYNC,
                    || {
                        full_syncs.set(full_syncs.get() + 1);
                        Ok(())
                    },
                    || {
                        fsyncs.set(fsyncs.get() + 1);
                        Ok(())
                    },
                    true,
                )
                .unwrap();
            };
            sync();
            assert_eq!((full_syncs.get(), fsyncs.get()), (0, 1));
            set_macos_full_sync(true);
            assert_eq!(mode(), "full_sync");
            sync();
            assert_eq!((full_syncs.get(), fsyncs.get()), (1, 1));
            set_macos_full_sync(false);
            assert_eq!(mode(), "fsync");
            sync();
            assert_eq!((full_syncs.get(), fsyncs.get()), (1, 2));
            assert_eq!(diagnostics.fallbacks.load(Ordering::Relaxed), 0);
            WARNINGS.with(|warnings| assert!(warnings.borrow().is_empty()));
        }
    }
}
