//! Debug builds may inspect the account's store, but must not change it.
//! Unix protects the effective account's passwd home, independent of HOME/XDG.
//! Windows always protects the process token's profile-derived LocalAppData,
//! independent of HOME/USERPROFILE/LOCALAPPDATA. A successful shell known-folder
//! lookup adds another root; that lookup can reflect redirected or expanded
//! environment paths. If neither OS lookup supplies a root, writes fail closed.

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
            if let Ok(tail) = path.strip_prefix(ancestor) {
                return canonical.join(tail);
            }
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
    fn absolute_normalized(path: &Path) -> io::Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("storage fence current-directory lookup failed: {error}"),
                    )
                })?
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
        Ok(normalized)
    }
    let canonicalize = |path: &Path| path.canonicalize().ok();
    let Ok(path) = absolute_normalized(&canonicalized_with(path, &canonicalize)) else {
        return true;
    };
    let Ok(parent) = absolute_normalized(&canonicalized_with(parent, &canonicalize)) else {
        return true;
    };
    comparison_key(&path, cfg!(windows)).starts_with(&comparison_key(&parent, cfg!(windows)))
}

#[cfg(unix)]
pub(crate) fn account_home() -> io::Result<PathBuf> {
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
            if buffer.len() >= 1024 * 1024 {
                return Err(io::Error::other(
                    "getpwuid_r storage fence lookup exceeded its buffer limit",
                ));
            }
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if error != 0 || result.is_null() {
            return Err(io::Error::other(format!(
                "getpwuid_r storage fence lookup failed (status {error}, no entry: {})",
                result.is_null()
            )));
        }
        // SAFETY: getpwuid_r initialized result on success.
        if unsafe { (*result).pw_dir.is_null() } {
            return Err(io::Error::other(
                "getpwuid_r storage fence lookup returned no home",
            ));
        }
        // SAFETY: a successful lookup supplies a NUL-terminated pw_dir in buffer.
        let home = unsafe { std::ffi::CStr::from_ptr((*result).pw_dir) };
        return valid_account_path(
            PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes())),
            "getpwuid_r",
        );
    }
}

#[cfg(windows)]
pub(crate) fn account_home() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::UI::Shell::GetUserProfileDirectoryW;

    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: this handle belongs to the successful OpenProcessToken call.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let mut token = std::ptr::null_mut();
    // SAFETY: the current process pseudo-handle is valid and token is writable.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::other(format!(
            "OpenProcessToken(TOKEN_QUERY) storage fence lookup failed: {}",
            io::Error::last_os_error()
        )));
    }
    let token = Token(token);
    let mut size = 0;
    // SAFETY: a null buffer queries the required UTF-16 buffer size.
    unsafe {
        GetUserProfileDirectoryW(token.0, std::ptr::null_mut(), &mut size);
    }
    if size == 0 || size > 32768 {
        return Err(io::Error::other(format!(
            "GetUserProfileDirectoryW storage fence size lookup failed (size {size}): {}",
            io::Error::last_os_error()
        )));
    }
    let mut buffer = vec![0u16; size as usize];
    // SAFETY: the buffer has the size supplied by the profile API.
    if unsafe { GetUserProfileDirectoryW(token.0, buffer.as_mut_ptr(), &mut size) } == 0 {
        return Err(io::Error::other(format!(
            "GetUserProfileDirectoryW storage fence lookup failed: {}",
            io::Error::last_os_error()
        )));
    }
    let len = buffer.iter().position(|unit| *unit == 0).ok_or_else(|| {
        io::Error::other("GetUserProfileDirectoryW returned an unterminated path")
    })?;
    valid_account_path(
        PathBuf::from(OsString::from_wide(&buffer[..len])),
        "GetUserProfileDirectoryW",
    )
}

#[cfg(windows)]
pub(crate) fn known_local_app_data() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY,
    };
    struct FolderPath(*mut u16);
    impl Drop for FolderPath {
        fn drop(&mut self) {
            // SAFETY: shell paths use the COM allocator; freeing null is allowed.
            unsafe {
                CoTaskMemFree(self.0.cast());
            }
        }
    }
    let mut raw = std::ptr::null_mut();
    // SAFETY: the GUID is valid and raw is writable. Do not require existence:
    // a harness may redirect the shell's expanded profile to a missing folder.
    let status = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    let path = FolderPath(raw);
    if status < 0 || path.0.is_null() {
        return Err(io::Error::other(format!("SHGetKnownFolderPath(FOLDERID_LocalAppData, KF_FLAG_DONT_VERIFY) storage fence lookup failed (HRESULT 0x{:08x})", status as u32)));
    }
    let mut len = 0;
    // SAFETY: a successful shell lookup returns an allocated NUL-terminated path.
    while unsafe { *path.0.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the scan above found the end of the allocated path.
    let units = unsafe { std::slice::from_raw_parts(path.0, len) };
    valid_account_path(
        PathBuf::from(OsString::from_wide(units)),
        "SHGetKnownFolderPath",
    )
}

fn valid_account_path(path: PathBuf, lookup: &str) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::other(format!(
            "{lookup} storage fence lookup returned an empty or relative path"
        )));
    }
    Ok(path)
}

pub(crate) fn account_storage_roots() -> io::Result<Vec<PathBuf>> {
    #[cfg(windows)]
    {
        combine_windows_roots(account_home(), known_local_app_data())
    }
    #[cfg(unix)]
    {
        Ok(vec![account_home()?.join(".local/share/cortexkit/aft")])
    }
}

#[cfg(any(windows, test))]
fn combine_windows_roots(
    profile: io::Result<PathBuf>,
    local_data: io::Result<PathBuf>,
) -> io::Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    let mut errors = Vec::new();
    match profile {
        Ok(profile) => roots.push(profile.join("AppData/Local/cortexkit/aft")),
        Err(error) => errors.push(error.to_string()),
    }
    match local_data {
        Ok(data) => roots.push(data.join("cortexkit/aft")),
        Err(error) => errors.push(error.to_string()),
    }
    if roots.is_empty() {
        return Err(io::Error::other(format!(
            "no protected account storage root resolved: {}",
            errors.join("; ")
        )));
    }
    Ok(roots)
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
    protection_status_with(path, account_storage_roots).unwrap_or(true)
}

fn protection_status_with(
    path: &Path,
    roots: impl FnOnce() -> io::Result<Vec<PathBuf>>,
) -> io::Result<bool> {
    // Tests and development rigs normally run debug builds (including the
    // target/debug/aft children of Bun tests). Release builds must be allowed
    // automatically: packaging cannot depend on remembering extra opt-ins.
    if !cfg!(debug_assertions) {
        return Ok(false);
    }
    if migration_opt_in() {
        return Ok(false);
    }
    #[cfg(test)]
    if let Some((root, _)) = TEST_ACCOUNT.with(|slot| slot.borrow().clone()) {
        return Ok(is_within(path, &root));
    }
    Ok(roots()?.iter().any(|root| is_within(path, root)))
}

pub(crate) fn refuse_write(path: &Path) -> io::Result<()> {
    refuse_write_for_status(path, protection_status_with(path, account_storage_roots))
}

fn refuse_write_for_status(path: &Path, status: io::Result<bool>) -> io::Result<()> {
    if !matches!(status, Ok(false)) {
        let reason = status
            .err()
            .map(|error| format!("; unresolved protected-root lookup: {error}"))
            .unwrap_or_default();
        Err(io::Error::new(io::ErrorKind::PermissionDenied, format!(
            "{CODE}: dev/test build refused to migrate production storage or write a versioned record at {}{reason}; use a disposable storage root or explicitly set AFT_ALLOW_PRODUCTION_MIGRATION=1", path.display()
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

    #[test]
    fn windows_root_union_keeps_profile_and_redirected_local_data() {
        let fixture = tempfile::tempdir().unwrap();
        let profile = fixture.path().join("profile");
        let redirected = fixture.path().join("redirected-local");
        let roots = combine_windows_roots(Ok(profile.clone()), Ok(redirected.clone())).unwrap();
        assert_eq!(
            roots,
            vec![
                profile.join("AppData/Local/cortexkit/aft"),
                redirected.join("cortexkit/aft")
            ]
        );
        let fallback = combine_windows_roots(
            Ok(profile.clone()),
            Err(io::Error::other("SHGetKnownFolderPath unavailable")),
        )
        .unwrap();
        assert_eq!(fallback, vec![profile.join("AppData/Local/cortexkit/aft")]);
        let shell_only = combine_windows_roots(
            Err(io::Error::other("GetUserProfileDirectoryW unavailable")),
            Ok(redirected.clone()),
        )
        .unwrap();
        assert_eq!(shell_only, vec![redirected.join("cortexkit/aft")]);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn unresolved_account_roots_refuse_writes_without_panicking() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("never-created");
        let roots = combine_windows_roots(
            Err(io::Error::other("GetUserProfileDirectoryW lookup failed")),
            Err(io::Error::other("SHGetKnownFolderPath lookup failed")),
        );
        let status = protection_status_with(&path, || roots);
        let error = refuse_write_for_status(&path, status).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let reason = error.to_string();
        assert!(reason.contains(CODE), "{reason}");
        assert!(reason.contains("GetUserProfileDirectoryW"), "{reason}");
        assert!(reason.contains("SHGetKnownFolderPath"), "{reason}");
        assert!(!path.exists());
    }

    #[cfg(debug_assertions)]
    struct RestoreEnvironment(Vec<(&'static str, Option<OsString>)>);
    #[cfg(debug_assertions)]
    impl RestoreEnvironment {
        fn replace(values: &[(&'static str, &Path)]) -> Self {
            let restore = Self(
                values
                    .iter()
                    .map(|(name, _)| (*name, std::env::var_os(name)))
                    .collect(),
            );
            for (name, path) in values {
                std::env::set_var(name, path);
            }
            restore
        }
    }
    #[cfg(debug_assertions)]
    impl Drop for RestoreEnvironment {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    #[cfg(all(windows, debug_assertions))]
    #[test]
    fn windows_token_profile_is_protected_with_missing_home_environment() {
        use windows_sys::Win32::UI::Shell::{
            SHGetFolderPathW, CSIDL_LOCAL_APPDATA, SHGFP_TYPE_CURRENT,
        };
        let _env = crate::test_env::process_env_lock();
        let profile = account_home().expect("process token supplies the real account profile");
        let fixture = tempfile::tempdir().unwrap();
        let missing_home = fixture.path().join("nonexistent-profile");
        let missing_local = fixture.path().join("nonexistent-local-appdata");
        let _restore = RestoreEnvironment::replace(&[
            ("HOME", &missing_home),
            ("USERPROFILE", &missing_home),
            ("LOCALAPPDATA", &missing_local),
        ]);
        assert_eq!(
            account_home().unwrap(),
            profile,
            "token lookup must ignore substituted environment homes"
        );
        let roots = account_storage_roots()
            .expect("token root must survive failed or redirected shell lookup");
        let expected = profile.join("AppData/Local/cortexkit/aft");
        assert!(
            roots.contains(&expected),
            "token profile root missing: {roots:?}"
        );
        assert!(protected(&expected.join("aft.db")));
        assert!(refuse_write(&expected.join("aft.db"))
            .unwrap_err()
            .to_string()
            .contains(CODE));
        assert!(refuse_write(&fixture.path().join("disposable-storage/aft.db")).is_ok());
        assert!(!missing_home.exists());
        assert!(!missing_local.exists());

        let mut old_buffer = [0u16; 260];
        // SAFETY: the legacy API's MAX_PATH-sized buffer is writable. This probe
        // records its verification failure, never uses it as the fence's root.
        let old_status = unsafe {
            SHGetFolderPathW(
                std::ptr::null_mut(),
                CSIDL_LOCAL_APPDATA as i32,
                std::ptr::null_mut(),
                SHGFP_TYPE_CURRENT as u32,
                old_buffer.as_mut_ptr(),
            )
        };
        eprintln!("legacy SHGetFolderPathW without DONT_VERIFY: HRESULT=0x{:08x}; token-profile roots={roots:?}", old_status as u32);
    }

    #[cfg(all(unix, debug_assertions))]
    #[test]
    fn unix_account_root_is_protected_with_missing_home_environment() {
        let _env = crate::test_env::process_env_lock();
        let home = account_home().expect("passwd supplies the effective account home");
        let fixture = tempfile::tempdir().unwrap();
        let missing = fixture.path().join("nonexistent-home");
        let _restore =
            RestoreEnvironment::replace(&[("HOME", &missing), ("XDG_DATA_HOME", &missing)]);
        assert_eq!(account_home().unwrap(), home);
        let root = home.join(".local/share/cortexkit/aft");
        assert_eq!(account_storage_roots().unwrap(), vec![root.clone()]);
        assert!(protected(&root.join("aft.db")));
        assert!(refuse_write(&root.join("aft.db"))
            .unwrap_err()
            .to_string()
            .contains(CODE));
        assert!(refuse_write(&fixture.path().join("disposable-storage/aft.db")).is_ok());
        assert!(!missing.exists());
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
