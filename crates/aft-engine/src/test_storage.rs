pub use aft_base::test_storage::{
    assert_context, assert_database, assert_root, live_storage_root_from, live_storage_roots,
};

#[cfg(feature = "test-support")]
thread_local! {
    static ISOLATE_CONTEXT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(feature = "test-support")]
pub(crate) fn with_isolated_context<T>(make: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            ISOLATE_CONTEXT.with(|scope| scope.set(self.0));
        }
    }
    let _restore = Restore(ISOLATE_CONTEXT.with(|scope| scope.replace(true)));
    make()
}

pub(crate) fn storage_for_context() -> Option<tempfile::TempDir> {
    let isolate = cfg!(test);
    #[cfg(feature = "test-support")]
    let isolate = isolate || ISOLATE_CONTEXT.with(std::cell::Cell::get);
    isolate.then(|| {
        tempfile::Builder::new()
            .prefix("aft-test-context-")
            .tempdir()
            .expect("private test storage")
    })
}

#[cfg(test)]
#[allow(unused_imports)]
use crate::config::Config;

#[cfg(test)]
#[allow(unused_imports)]
use std::ffi::OsString;
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
    pub(crate) use aft_base::test_storage::tests::*;

    #[test]
    fn test_database_allows_a_temporary_xdg_default() {
        with_temporary_xdg(|fixture| {
            let root = crate::storage_path::storage_dir_without_overrides_for_test();
            assert!(root.starts_with(fixture));
            std::fs::create_dir_all(&root).unwrap();
            let _db = crate::db::open(&root.join("aft.db")).unwrap();
            assert!(root.join("aft.db").is_file());
        });
    }

    fn context(config: Config) -> crate::context::AppContext {
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config)
    }
    #[test]
    fn test_context_defaults_to_private_storage() {
        let first = context(Config::default());
        let second = context(Config::default());
        let first = first.config();
        let second = second.config();
        assert!(first.storage_dir.as_ref().unwrap().is_dir());
        assert_ne!(first.storage_dir, second.storage_dir);
        assert_context(&first);
        assert_context(&second);
    }
    #[test]
    #[should_panic(expected = "test context cannot use the live default AFT storage root")]
    fn test_context_rejects_default_storage_before_any_io() {
        let config = Config {
            storage_dir: Some(live_storage_roots()[0].clone()),
            ..Default::default()
        };
        let _ = context(config);
    }
    #[test]
    fn test_context_publication_keeps_storage_isolated() {
        let ctx = context(Config::default());
        let original = ctx.config().storage_dir.clone();
        ctx.set_config(Config::default());
        assert_eq!(ctx.config().storage_dir, original);
        ctx.update_config(|c| c.storage_dir = None);
        assert_eq!(ctx.config().storage_dir, original);
    }
    #[test]
    #[should_panic(expected = "test context cannot use the live default AFT storage root")]
    fn test_database_open_rejects_default_before_creating_files() {
        let _ = crate::db::open(&live_storage_roots()[0].join("aft.db"));
    }

    #[test]
    #[should_panic(expected = "test context cannot use the live default AFT storage root")]
    fn test_database_single_attempt_rejects_live_storage_before_open() {
        let live = live_storage_roots();
        let _ =
            crate::db::open_with_mode(&live[0].join("aft.db"), crate::db::OpenMode::SingleAttempt);
    }
}
