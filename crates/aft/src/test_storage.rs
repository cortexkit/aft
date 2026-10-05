//! Test-only persistence fence. A test context must never inherit live storage.
use crate::config::Config;
use std::path::{Path, PathBuf};

fn normalized(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn assert_context(config: &Config) {
    let configured = config
        .storage_dir
        .as_deref()
        .filter(|p| !p.as_os_str().is_empty())
        .expect(
            "test context must use an explicit isolated storage_dir, never the process default",
        );
    assert_root(configured);
    assert_root(&crate::bash_background::storage_dir(Some(configured)));
}

pub(crate) fn assert_root(root: &Path) {
    let forbidden = crate::bash_background::storage_dir_without_overrides_for_test();
    assert!(
        !normalized(root).starts_with(normalized(&forbidden)),
        "test context cannot use the live default AFT storage root: {}",
        root.display()
    );
}

pub(crate) fn assert_database(path: &Path) {
    if let Some(parent) = path.parent() {
        assert_root(parent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            storage_dir: Some(crate::bash_background::storage_dir_without_overrides_for_test()),
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
        let _ = crate::db::open(
            &crate::bash_background::storage_dir_without_overrides_for_test().join("aft.db"),
        );
    }
}
