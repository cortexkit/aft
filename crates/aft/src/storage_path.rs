use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoragePlatform {
    Windows,
    Other,
}

impl StoragePlatform {
    fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

pub fn storage_dir(configured: Option<&std::path::Path>) -> PathBuf {
    // Test hook for the health-rollup path cache: one tick over many contexts
    // sharing a storage root must resolve it once, not once per context.
    #[cfg(test)]
    STORAGE_DIR_CALLS.with(|calls| calls.set(calls.get() + 1));
    let lookup = |name: &str| std::env::var_os(name);
    let fallback_home = std::env::home_dir();
    let current_dir = std::env::current_dir().ok();
    let root = storage_dir_from(
        configured,
        &lookup,
        StoragePlatform::current(),
        fallback_home.as_deref(),
        current_dir.as_deref(),
    );
    crate::project_content::register_storage_root(&root);
    root
}

/// Resolve the root that holds rebuildable cache artifacts (search, semantic,
/// symbol, call-graph, inspect and view caches).
///
/// `cache_root` is a root the caller already resolved, normally
/// `Config::cache_storage_dir()`, which is `<storage.cache_dir>/aft` when a
/// separate cache volume is configured. That root must be used exactly as
/// given. [`storage_dir`] is the wrong helper here: it lets `AFT_STORAGE_DIR`
/// replace the supplied path, which is right for durable state but would move
/// cache artifacts back onto the durable disk, where the split-storage guard
/// then refuses to create them. Without a resolved root the caches share the
/// default durable root, as they always have.
pub fn cache_artifact_root(cache_root: Option<&std::path::Path>) -> PathBuf {
    match cache_root.filter(|path| !path.as_os_str().is_empty()) {
        Some(root) => {
            crate::project_content::register_storage_root(root);
            root.to_path_buf()
        }
        None => storage_dir(None),
    }
}

// Test hook counting storage_dir resolutions on this thread. Reset
// before a health tick and read after it to prove the tick resolved shared
// roots once instead of once per context.
#[cfg(test)]
thread_local! {
    static STORAGE_DIR_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Test hook counting [`storage_dir`] resolutions on this thread. Reset
/// before a health tick and read after it to prove the tick resolved shared
/// roots once instead of once per context.
#[cfg(test)]
pub(crate) fn storage_dir_calls_for_test() -> u64 {
    STORAGE_DIR_CALLS.with(|calls| calls.get())
}

/// Test hook resetting [`storage_dir_calls_for_test`].
#[cfg(test)]
pub(crate) fn reset_storage_dir_calls_for_test() {
    STORAGE_DIR_CALLS.with(|calls| calls.set(0));
}

/// Resolve the current test environment's default, excluding AFT-specific
/// overrides. HOME and XDG may deliberately name a fixture, not the live store.
#[cfg(test)]
pub(crate) fn storage_dir_without_overrides_for_test() -> PathBuf {
    let lookup = |name: &str| {
        if matches!(name, "AFT_STORAGE_DIR" | "AFT_CACHE_DIR") {
            None
        } else {
            std::env::var_os(name)
        }
    };
    storage_dir_from_test_environment(&lookup)
}

/// Let the persistence fence use the production ladder with account-owned homes
/// instead of the temporary homes installed by a test or its runner.
#[cfg(test)]
pub(crate) fn storage_dir_from_test_environment(
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
) -> PathBuf {
    storage_dir_from(
        None,
        &lookup,
        StoragePlatform::current(),
        std::env::home_dir().as_deref(),
        std::env::current_dir().ok().as_deref(),
    )
}

fn storage_dir_from(
    configured: Option<&std::path::Path>,
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: StoragePlatform,
    fallback_home: Option<&std::path::Path>,
    current_dir: Option<&std::path::Path>,
) -> PathBuf {
    let resolve = |path: &std::path::Path| {
        resolve_storage_path_from(path, lookup, platform, fallback_home, current_dir)
    };

    if let Some(dir) = non_empty_env_path_from(lookup, "AFT_STORAGE_DIR") {
        return resolve(&dir);
    }
    if let Some(dir) = configured.filter(|path| !path.as_os_str().is_empty()) {
        // Explicit process-state paths are already caller-owned. Preserve their
        // spelling so every downstream read/write uses the exact configured root.
        return dir.to_path_buf();
    }
    // AFT_CACHE_DIR predates AFT_STORAGE_DIR as the storage sandbox lever. It
    // remains above the shared data-home ladder so old isolated invocations do
    // not escape into the operator's data root.
    if let Some(dir) = non_empty_env_path_from(lookup, "AFT_CACHE_DIR") {
        return resolve(&dir).join("aft");
    }

    resolve(&cortexkit_data_root_from(lookup, platform))
        .join("cortexkit")
        .join("aft")
}

fn non_empty_env_path_from(
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    name: &str,
) -> Option<PathBuf> {
    lookup(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn storage_home_dir_from(
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: StoragePlatform,
    fallback_home: Option<&std::path::Path>,
) -> Option<PathBuf> {
    let configured = if platform == StoragePlatform::Windows {
        non_empty_env_path_from(lookup, "USERPROFILE")
            .or_else(|| non_empty_env_path_from(lookup, "HOME"))
    } else {
        non_empty_env_path_from(lookup, "HOME")
            .or_else(|| non_empty_env_path_from(lookup, "USERPROFILE"))
    };
    configured.or_else(|| fallback_home.map(PathBuf::from))
}

fn cortexkit_data_root_from(
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: StoragePlatform,
) -> PathBuf {
    if let Some(dir) = non_empty_env_path_from(lookup, "XDG_DATA_HOME") {
        return dir;
    }
    if platform == StoragePlatform::Windows {
        // AFT stores indexes, backups, and checkpoints here.
        // cache-class storage; stable for existing installs.
        // Do not move this shipped ladder to Roaming.
        if let Some(dir) = non_empty_env_path_from(lookup, "LOCALAPPDATA") {
            return dir;
        }
        if let Some(home) = non_empty_env_path_from(lookup, "USERPROFILE") {
            return home.join("AppData").join("Local");
        }
    }
    if let Some(home) = non_empty_env_path_from(lookup, "HOME") {
        return home.join(".local").join("share");
    }
    PathBuf::from(".local").join("share")
}

fn resolve_storage_path_from(
    path: &std::path::Path,
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: StoragePlatform,
    fallback_home: Option<&std::path::Path>,
    current_dir: Option<&std::path::Path>,
) -> PathBuf {
    let storage_home = || storage_home_dir_from(lookup, platform, fallback_home);
    let expanded = if path == std::path::Path::new("~") {
        storage_home().unwrap_or_else(|| path.to_path_buf())
    } else if let Some(raw) = path.to_str() {
        if raw.starts_with("~/") || raw.starts_with("~\\") {
            storage_home()
                .map(|home| home.join(&raw[2..]))
                .unwrap_or_else(|| path.to_path_buf())
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else if let Some(current_dir) = current_dir {
        current_dir.join(expanded)
    } else {
        expanded
    };
    normalize_absolute_path(&absolute)
}

fn normalize_absolute_path(path: &std::path::Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod storage_root_tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    fn resolve_storage_fixture(
        env: &HashMap<&str, OsString>,
        configured: Option<&Path>,
        platform: super::StoragePlatform,
        fallback_home: Option<&Path>,
        current_dir: Option<&Path>,
    ) -> PathBuf {
        super::storage_dir_from(
            configured,
            &|name| env.get(name).cloned(),
            platform,
            fallback_home,
            current_dir,
        )
    }

    #[test]
    fn storage_ladder_matches_daemon_except_for_stable_windows_cache_class_storage() {
        let current_dir = Path::new("/work");
        let fallback_home = Path::new("/system-home");
        let module_suffix = Path::new("cortexkit").join("aft");
        let mut env = HashMap::from([
            ("AFT_STORAGE_DIR", OsString::new()),
            ("AFT_CACHE_DIR", OsString::new()),
            ("XDG_DATA_HOME", OsString::new()),
            ("APPDATA", OsString::from("/wrong-roaming-data")),
            ("USERPROFILE", OsString::new()),
            ("HOME", OsString::new()),
            ("LOCALAPPDATA", OsString::new()),
        ]);

        for platform in [
            super::StoragePlatform::Other,
            super::StoragePlatform::Windows,
        ] {
            assert_eq!(
                resolve_storage_fixture(
                    &env,
                    Some(Path::new("")),
                    platform,
                    Some(fallback_home),
                    Some(current_dir),
                ),
                current_dir.join(".local/share").join(&module_suffix),
                "empty values and an empty configured root are unset"
            );
        }
        assert_eq!(
            resolve_storage_fixture(&env, None, super::StoragePlatform::Other, None, None,),
            PathBuf::from(".local/share/cortexkit/aft"),
            "an unavailable cwd preserves the honest relative path"
        );

        env.insert("HOME", OsString::from("/home/operator"));
        env.insert("USERPROFILE", OsString::from("/wrong-profile"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            Path::new("/home/operator/.local/share").join(&module_suffix)
        );

        env.insert("LOCALAPPDATA", OsString::from("/local-data"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Windows,
                Some(fallback_home),
                Some(current_dir),
            ),
            Path::new("/local-data").join(&module_suffix)
        );
        env.insert("LOCALAPPDATA", OsString::new());
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Windows,
                Some(fallback_home),
                Some(current_dir),
            ),
            Path::new("/wrong-profile/AppData/Local").join(&module_suffix)
        );

        env.insert("XDG_DATA_HOME", OsString::from("relative-data"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            current_dir.join("relative-data").join(&module_suffix)
        );

        env.insert("AFT_CACHE_DIR", OsString::from("/legacy-cache"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            PathBuf::from("/legacy-cache/aft")
        );
        let configured = Path::new("configured/../configured-aft");
        assert_eq!(
            resolve_storage_fixture(
                &env,
                Some(configured),
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            configured,
            "the caller-owned configured spelling outranks the legacy cache lever"
        );

        env.insert("AFT_STORAGE_DIR", OsString::from("~/operator-aft"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                Some(configured),
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            PathBuf::from("/home/operator/operator-aft")
        );
    }
}
