//! Test-only persistence fence. A test context must never inherit live storage.
use crate::config::Config;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::production_storage::{account_home, canonicalized_with, comparison_key, is_within};

pub fn live_storage_roots() -> Vec<PathBuf> {
    let home = account_home().expect("test storage fence must resolve the account profile");
    #[cfg(windows)]
    let local_data = Some(
        crate::production_storage::known_local_app_data()
            .unwrap_or_else(|_| home.join("AppData/Local")),
    );
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
    let mut roots = crate::production_storage::account_storage_roots()
        .expect("test storage fence must resolve a protected root");
    roots.extend([current, account]);
    roots
}

pub fn live_storage_root_from(
    home: &Path,
    local_data: Option<&Path>,
    temporary: &[PathBuf],
    lookup: &impl Fn(&str) -> Option<OsString>,
) -> PathBuf {
    crate::storage_path::storage_dir_from_test_environment(&|name| {
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

pub fn assert_context(config: &Config) {
    let configured = config
        .storage_dir
        .as_deref()
        .filter(|p| !p.as_os_str().is_empty())
        .expect(
            "test context must use an explicit isolated storage_dir, never the process default",
        );
    assert_root(configured);
    assert_root(&crate::storage_path::storage_dir(Some(configured)));
}

pub fn assert_root(root: &Path) {
    assert!(
        !live_storage_roots()
            .iter()
            .any(|forbidden| is_within(root, forbidden)),
        "test context cannot use the live default AFT storage root: {}",
        root.display()
    );
}

pub fn assert_database(path: &Path) {
    if let Some(parent) = path.parent() {
        assert_root(parent);
    }
}
