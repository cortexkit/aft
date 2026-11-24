#[cfg(any(unix, windows))]
pub use aft_base::production_storage::account_home;
#[cfg(any(test, feature = "test-support"))]
pub use aft_base::production_storage::with_test_account;
#[cfg(test)]
pub(crate) use aft_base::production_storage::CODE;
pub use aft_base::production_storage::{
    canonicalized_with, protected, refuse_write, resolve_account_home_dir,
};

#[cfg(test)]
#[allow(unused_imports)]
use std::ffi::OsString;
#[cfg(test)]
#[allow(unused_imports)]
use std::io;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Component;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    pub(crate) use aft_base::production_storage::tests::*;

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
