//! Debug builds may inspect the account's store, but must not change it.
//! Account directories come from the OS, never HOME/XDG or AFT overrides.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

pub(crate) const CODE: &str = "dev_build_refused_production_migration";

// Resolve an existing ancestor as well as the leaf, which may not exist yet.
pub(crate) fn canonicalized_with(
    path: &Path,
    canonicalize: &impl Fn(&Path) -> Option<PathBuf>,
) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Some(canonical) = canonicalize(ancestor) {
            return canonical.join(path.strip_prefix(ancestor).unwrap());
        }
    }
    path.to_path_buf()
}

pub(crate) fn comparison_key(path: &Path, windows: bool) -> Vec<OsString> {
    if windows {
        crate::windows_path::normalize_windows_path(path)
            .to_string_lossy()
            .trim_end_matches('\\')
            .split('\\')
            .map(|part| OsString::from(part.to_lowercase()))
            .collect()
    } else {
        path.components()
            .map(|part| part.as_os_str().into())
            .collect()
    }
}

pub(crate) fn is_within(path: &Path, parent: &Path) -> bool {
    fn absolute_normalized(path: &Path) -> PathBuf {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .expect("storage fence requires a current directory")
                .join(path)
        };
        let mut normalized = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::CurDir => (),
                Component::ParentDir => {
                    normalized.pop();
                }
                _ => normalized.push(component),
            }
        }
        normalized
    }
    let canonicalize = |path: &Path| path.canonicalize().ok();
    let path = absolute_normalized(&canonicalized_with(path, &canonicalize));
    let parent = absolute_normalized(&canonicalized_with(parent, &canonicalize));
    comparison_key(&path, cfg!(windows)).starts_with(&comparison_key(&parent, cfg!(windows)))
}

#[cfg(unix)]
pub(crate) fn account_home() -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = vec![0u8; 16384];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: entry and buffer are writable for the supplied sizes.
        let error = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if error == libc::ERANGE {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        assert!(
            error == 0 && !result.is_null(),
            "cannot resolve account home for storage fence"
        );
        // SAFETY: a successful lookup supplies a NUL-terminated pw_dir in buffer.
        let home = unsafe { std::ffi::CStr::from_ptr((*result).pw_dir) };
        return PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes()));
    }
}

#[cfg(windows)]
pub(crate) fn account_folder(folder: u32) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::UI::Shell::{SHGetFolderPathW, SHGFP_TYPE_CURRENT};
    let mut buffer = [0u16; 260];
    // SAFETY: SHGetFolderPathW writes at most MAX_PATH UTF-16 code units.
    let status = unsafe {
        SHGetFolderPathW(
            std::ptr::null_mut(),
            folder as i32,
            std::ptr::null_mut(),
            SHGFP_TYPE_CURRENT as u32,
            buffer.as_mut_ptr(),
        )
    };
    assert!(
        status >= 0,
        "cannot resolve account folder for storage fence"
    );
    let len = buffer.iter().position(|unit| *unit == 0).unwrap();
    PathBuf::from(OsString::from_wide(&buffer[..len]))
}

#[cfg(all(windows, test))]
pub(crate) fn account_home() -> PathBuf {
    account_folder(windows_sys::Win32::UI::Shell::CSIDL_PROFILE)
}

fn account_storage_root() -> PathBuf {
    #[cfg(windows)]
    let data = account_folder(windows_sys::Win32::UI::Shell::CSIDL_LOCAL_APPDATA);
    #[cfg(not(windows))]
    let data = account_home().join(".local/share");
    data.join("cortexkit/aft")
}

fn migration_opt_in() -> bool {
    let lookup = |name: &str| {
        #[cfg(test)]
        if let Some((_, allow)) = TEST_ACCOUNT.with(|slot| slot.borrow().clone()) {
            // Supply an environment lookup, not a policy override: the opt-in
            // test must exercise the same variable name and exact-value check.
            return (allow && name == "AFT_ALLOW_PRODUCTION_MIGRATION")
                .then(|| OsString::from("1"));
        }
        std::env::var_os(name)
    };
    lookup("AFT_ALLOW_PRODUCTION_MIGRATION").as_deref() == Some(std::ffi::OsStr::new("1"))
}

pub(crate) fn protected(path: &Path) -> bool {
    // Tests and development rigs normally run debug builds (including the
    // target/debug/aft children of Bun tests). Release builds must be allowed
    // automatically: packaging cannot depend on remembering extra opt-ins.
    if !cfg!(debug_assertions) {
        return false;
    }
    if migration_opt_in() {
        return false;
    }
    #[cfg(test)]
    if let Some((root, _)) = TEST_ACCOUNT.with(|slot| slot.borrow().clone()) {
        return is_within(path, &root);
    }
    is_within(path, &account_storage_root())
}

pub(crate) fn refuse_write(path: &Path) -> io::Result<()> {
    if protected(path) {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, format!(
            "{CODE}: dev/test build refused to migrate production storage or write a versioned record at {}; use a disposable storage root or explicitly set AFT_ALLOW_PRODUCTION_MIGRATION=1", path.display()
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static TEST_ACCOUNT: std::cell::RefCell<Option<(PathBuf, bool)>> = const { std::cell::RefCell::new(None) };
}

/// Thread-local OS-directory seam, never available to a spawned binary.
#[cfg(test)]
pub(crate) fn with_test_account<T>(root: &Path, allow: bool, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<(PathBuf, bool)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_ACCOUNT.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore =
        Restore(TEST_ACCOUNT.with(|slot| slot.replace(Some((root.to_path_buf(), allow)))));
    run()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old_database(root: &Path) -> PathBuf {
        std::fs::create_dir_all(root).unwrap();
        let path = root.join("aft.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL PRIMARY KEY); INSERT INTO schema_version VALUES (0);").unwrap();
        drop(conn);
        path
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_production_migration_is_refused_without_touching_database() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("production");
        let path = old_database(&root);
        let before = std::fs::read(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        with_test_account(&root, false, || {
            let error = crate::db::open(&path)
                .err()
                .expect("debug production migration must be refused");
            assert!(error.to_string().contains(CODE), "{error}");
        });
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            metadata.modified().unwrap()
        );
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            1,
            "no journal or lock files may be created"
        );
    }

    #[test]
    fn explicit_production_opt_in_allows_migration() {
        let fixture = tempfile::tempdir().unwrap();
        let path = old_database(fixture.path());
        with_test_account(fixture.path(), true, || {
            let conn = crate::db::open(&path).unwrap();
            assert_eq!(
                conn.query_row("SELECT MAX(version) FROM schema_version", [], |row| row
                    .get::<_, u32>(0))
                    .unwrap(),
                crate::db::CURRENT_SCHEMA_VERSION
            );
        });
    }

    #[test]
    fn non_production_storage_migrates_normally() {
        let fixture = tempfile::tempdir().unwrap();
        let path = old_database(&fixture.path().join("scratch"));
        with_test_account(&fixture.path().join("production"), false, || {
            let conn = crate::db::open(&path).unwrap();
            assert_eq!(
                conn.query_row("SELECT MAX(version) FROM schema_version", [], |row| row
                    .get::<_, u32>(0))
                    .unwrap(),
                crate::db::CURRENT_SCHEMA_VERSION
            );
        });
    }

    #[cfg(debug_assertions)]
    #[test]
    fn matching_production_schema_opens_readonly() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("aft.db");
        drop(crate::db::open(&path).unwrap());
        let before = std::fs::read(&path).unwrap();
        with_test_account(fixture.path(), false, || {
            let conn = crate::db::open(&path).unwrap();
            assert_eq!(
                conn.query_row("SELECT MAX(version) FROM schema_version", [], |row| row
                    .get::<_, u32>(0))
                    .unwrap(),
                crate::db::CURRENT_SCHEMA_VERSION
            );
            assert!(conn.execute("DELETE FROM schema_version", []).is_err());
        });
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn direct_production_migration_is_refused() {
        let fixture = tempfile::tempdir().unwrap();
        let path = old_database(fixture.path());
        let before = std::fs::read(&path).unwrap();
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        with_test_account(fixture.path(), false, || {
            let error = crate::db::run_migrations(&mut conn).unwrap_err();
            assert!(error.to_string().contains(CODE), "{error}");
        });
        drop(conn);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn production_bash_record_upgrade_is_refused_untouched() {
        use crate::bash_background::persistence::*;
        let fixture = tempfile::tempdir().unwrap();
        let layout =
            create_task_layout(fixture.path(), "session", "bash-0000000000000001").unwrap();
        let mut metadata = PersistedTask::starting(
            layout.paths.task_id.clone(),
            "session".into(),
            "true".into(),
            fixture.path().into(),
            None,
            None,
            false,
            false,
        );
        // Seed an old record directly; the production writer would upgrade it.
        metadata.schema_version = 6;
        let before = serde_json::to_vec(&metadata).unwrap();
        std::fs::write(&layout.paths.json, &before).unwrap();
        with_test_account(fixture.path(), false, || {
            let error = write_task_at(&layout, &metadata).unwrap_err();
            assert!(error.to_string().contains(CODE), "{error}");
            assert!(
                create_task_layout(fixture.path(), "session", "bash-0000000000000002").is_err()
            );
        });
        assert_eq!(std::fs::read(&layout.paths.json).unwrap(), before);
        with_test_account(fixture.path(), true, || {
            write_task_at(&layout, &metadata).unwrap()
        });
        assert_eq!(
            read_task_at(&layout).unwrap().schema_version,
            SCHEMA_VERSION
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn production_versioned_stores_and_floor_are_write_fenced() {
        let fixture = tempfile::tempdir().unwrap();
        with_test_account(fixture.path(), false, || {
            assert!(crate::reader_floor::raise(
                fixture.path(),
                &[(crate::persisted_format::PersistedStore::BashTask, 7)]
            )
            .unwrap_err()
            .to_string()
            .contains(CODE));
            assert!(crate::views::ViewStore::open(fixture.path(), "scope").is_err());
            assert!(crate::db::TrackedConnection::open(
                &fixture.path().join("blobs.sqlite"),
                crate::db::SqliteStore::BlobStore
            )
            .is_err());
            assert!(
                crate::backup::create_private_durable_dir(&fixture.path().join("checkpoints"))
                    .is_err()
            );
            let access = crate::root_cache::ArtifactAccess::for_root(fixture.path());
            for store in ["index", "semantic", "symbols", "callgraph"] {
                assert!(
                    !access.allows_write("key", &fixture.path().join(store).join("key/artifact"))
                );
            }
        });
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
    }

    #[cfg(all(unix, debug_assertions))]
    #[test]
    fn production_alias_and_relative_tail_cannot_bypass_fence() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("production");
        std::fs::create_dir_all(&root).unwrap();
        let alias = fixture.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        with_test_account(&root, false, || {
            assert!(refuse_write(&alias.join("uncreated/../aft.db")).is_err());
            assert!(refuse_write(&fixture.path().join("production-sibling/aft.db")).is_ok());
        });
    }

    #[test]
    fn production_write_fence_matches_build_debug_assertions() {
        let fixture = tempfile::tempdir().unwrap();
        with_test_account(fixture.path(), false, || {
            assert_eq!(
                protected(&fixture.path().join("aft.db")),
                cfg!(debug_assertions)
            );
        });
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn optimized_build_migrates_fake_production_without_opt_in() {
        let fixture = tempfile::tempdir().unwrap();
        let path = old_database(fixture.path());
        with_test_account(fixture.path(), false, || {
            let conn = crate::db::open(&path).unwrap();
            assert_eq!(
                conn.query_row("SELECT MAX(version) FROM schema_version", [], |row| row
                    .get::<_, u32>(0))
                    .unwrap(),
                crate::db::CURRENT_SCHEMA_VERSION
            );
        });
    }
}
