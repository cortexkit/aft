//! Test-only persistence fence. A test context must never inherit live storage.
use crate::config::Config;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use crate::production_storage::account_folder;
use crate::production_storage::{account_home, canonicalized_with, comparison_key, is_within};

fn live_storage_roots() -> [PathBuf; 2] {
    let home = account_home();
    #[cfg(windows)]
    let local_data = Some(account_folder(
        windows_sys::Win32::UI::Shell::CSIDL_LOCAL_APPDATA,
    ));
    #[cfg(not(windows))]
    let local_data: Option<PathBuf> = None;
    let mut temporary = vec![std::env::temp_dir()];
    // The repository gate puts its disposable homes under target/, which is
    // not necessarily under the OS temp directory (notably on Windows CI).
    if let Some(gate_home) = std::env::var_os("AFT_GATE_HERMETIC_HOME_ROOT") {
        temporary.push(PathBuf::from(gate_home));
    }
    let current = live_storage_root_from(&home, local_data.as_deref(), &temporary, &|name| {
        std::env::var_os(name)
    });
    // Keep protecting the account default when a test replaces a real custom
    // XDG home. Environment mutation must not turn off the native-root fence.
    let account = live_storage_root_from(&home, local_data.as_deref(), &[], &|_| None);
    [current, account]
}

fn live_storage_root_from(
    home: &Path,
    local_data: Option<&Path>,
    temporary: &[PathBuf],
    lookup: &impl Fn(&str) -> Option<OsString>,
) -> PathBuf {
    crate::bash_background::storage_dir_from_test_environment(&|name| {
        let non_temporary = || {
            lookup(name).filter(|value| {
                !temporary
                    .iter()
                    .any(|root| is_within(Path::new(value), root))
            })
        };
        match name {
            "AFT_STORAGE_DIR" | "AFT_CACHE_DIR" => None,
            "HOME" | "USERPROFILE" => non_temporary().or_else(|| Some(home.as_os_str().into())),
            "LOCALAPPDATA" => {
                non_temporary().or_else(|| local_data.map(|path| path.as_os_str().into()))
            }
            // Production honors XDG_DATA_HOME on Windows too. A test runner's
            // temporary XDG/LocalAppData namespace is not the operator's store.
            _ => non_temporary(),
        }
    })
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
    assert!(
        !live_storage_roots()
            .iter()
            .any(|forbidden| is_within(root, forbidden)),
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

    #[test]
    fn windows_storage_comparison_ignores_verbatim_prefix_case_and_separators() {
        let parent = comparison_key(
            Path::new(r"C:\Users\RunnerAdmin\AppData\Local\cortexkit\aft"),
            true,
        );
        for root in [
            r"\\?\c:\users\runneradmin\appdata\LOCAL\CORTEXKIT\AFT\aft.db",
            "c:/Users/RunnerAdmin/AppData/Local/cortexkit/aft/aft.db",
        ] {
            assert!(
                comparison_key(Path::new(root), true).starts_with(&parent),
                "{root}"
            );
        }
        assert_eq!(
            comparison_key(Path::new(r"\\?\UNC\SERVER\Share\aft"), true),
            comparison_key(Path::new(r"\\server\share\AFT"), true)
        );
        assert!(!comparison_key(
            Path::new(r"C:\Users\RunnerAdmin\AppData\Local\cortexkit\aft-other"),
            true
        )
        .starts_with(&parent));
    }

    #[test]
    fn storage_comparison_expands_short_name_ancestors_before_appending_missing_tail() {
        // 8.3 aliases are filesystem assignments, not a string substitution.
        // Inject that assignment so this check also runs on Unix hosts.
        let short = Path::new("C:/Users/RUNNER~1/AppData/Local/cortexkit/aft/aft.db");
        let canonical = canonicalized_with(short, &|path| {
            (path == Path::new("C:/Users/RUNNER~1")).then(|| PathBuf::from("C:/Users/RunnerAdmin"))
        });
        assert_eq!(
            comparison_key(&canonical, true),
            comparison_key(
                Path::new(r"\\?\c:\users\RunnerAdmin\AppData\Local\cortexkit\aft\aft.db"),
                true
            )
        );
    }

    #[test]
    fn temporary_home_and_xdg_do_not_redefine_the_live_storage_root() {
        let home = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let live_data = home.path().join("data");
        let temporary = [fixture.path().to_path_buf()];
        let live = live_storage_root_from(home.path(), None, &temporary, &|name| {
            (name == "XDG_DATA_HOME").then(|| live_data.as_os_str().into())
        });
        assert_eq!(live, live_data.join("cortexkit").join("aft"));
        let with_override = |name: &str| match name {
            "HOME" | "USERPROFILE" => Some(fixture.path().as_os_str().into()),
            "XDG_DATA_HOME" | "LOCALAPPDATA" => Some(fixture.path().join("data").into_os_string()),
            "AFT_STORAGE_DIR" | "AFT_CACHE_DIR" => {
                Some(fixture.path().join("cache").into_os_string())
            }
            _ => None,
        };
        let sandbox_default =
            crate::bash_background::storage_dir_from_test_environment(&with_override);
        let forbidden = live_storage_root_from(home.path(), None, &temporary, &with_override);
        assert!(!is_within(&sandbox_default, &forbidden));
        let expected = if cfg!(windows) {
            home.path().join("AppData").join("Local")
        } else {
            home.path().join(".local").join("share")
        };
        assert_eq!(forbidden, expected.join("cortexkit").join("aft"));
    }

    fn with_temporary_xdg(test: impl FnOnce(&Path)) {
        let _env = crate::test_env::process_env_lock();
        let fixture = tempfile::tempdir().unwrap();
        struct Restore(Option<OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match &self.0 {
                    Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                    None => std::env::remove_var("XDG_DATA_HOME"),
                }
            }
        }
        let _restore = Restore(std::env::var_os("XDG_DATA_HOME"));
        std::env::set_var("XDG_DATA_HOME", fixture.path());
        test(fixture.path());
    }

    #[test]
    fn test_database_allows_a_temporary_xdg_default() {
        with_temporary_xdg(|fixture| {
            let root = crate::bash_background::storage_dir_without_overrides_for_test();
            assert!(root.starts_with(fixture));
            std::fs::create_dir_all(&root).unwrap();
            let _db = crate::db::open(&root.join("aft.db")).unwrap();
            assert!(root.join("aft.db").is_file());
        });
    }

    #[test]
    #[should_panic(expected = "test context cannot use the live default AFT storage root")]
    fn test_root_rejects_account_default_even_with_a_temporary_xdg_home() {
        // Derive the expected root independently of the guard's resolver. This
        // assertion performs no I/O even if the fence is accidentally removed.
        #[cfg(windows)]
        let data = account_folder(windows_sys::Win32::UI::Shell::CSIDL_LOCAL_APPDATA);
        #[cfg(not(windows))]
        let data = account_home().join(".local").join("share");
        with_temporary_xdg(|_| assert_root(&data.join("cortexkit").join("aft")));
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
}
