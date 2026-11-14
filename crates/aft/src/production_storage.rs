pub use aft_base::production_storage::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::ffi::OsString;
#[cfg(test)]
#[allow(unused_imports)]
use std::io;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Component, Path, PathBuf};

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

    #[cfg(all(unix, debug_assertions))]
    #[test]
    fn unix_account_config_root_ignores_home_and_xdg() {
        let _env = crate::test_env::process_env_lock();
        let home = account_home().expect("passwd supplies the effective account home");
        let fixture = tempfile::tempdir().unwrap();
        let missing = fixture.path().join("nonexistent-home");
        let _restore =
            RestoreEnvironment::replace(&[("HOME", &missing), ("XDG_CONFIG_HOME", &missing)]);
        let root = home.join(".config/cortexkit");
        assert_eq!(account_config_roots().unwrap(), vec![root.clone()]);
        // Only computes the verdict; nothing under the real directory is touched.
        assert!(config_write_refusal(&root.join("aft.jsonc"))
            .unwrap()
            .contains(CODE));
        assert!(config_write_refusal(&missing.join(".config/cortexkit/aft.jsonc")).is_none());
        assert!(!missing.exists());
    }

    #[test]
    fn config_write_fence_matches_build_debug_assertions() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("aft.jsonc");
        with_test_account(fixture.path(), false, || {
            assert_eq!(
                config_write_refusal(&path).is_some(),
                cfg!(debug_assertions)
            );
        });
        with_test_account(fixture.path(), true, || {
            assert!(
                config_write_refusal(&path).is_none(),
                "the opt-in allows it"
            );
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
