//! Filesystem identity used only for policy comparisons.

use std::path::{Path, PathBuf};

/// Identity for policy comparisons only. Tool return paths retain their existing
/// resolution contract, including the unchanged spelling when unrestricted.
pub fn policy_path(path: &Path) -> PathBuf {
    let bounded = BOUNDED_POLICY_PATHS.with(|slot| slot.borrow().clone());
    if let Some(state) = bounded {
        if state.error.borrow().is_none() {
            *state.progress.lock().unwrap_or_else(|e| e.into_inner()) = path.to_path_buf();
            match crate::bounded_io::run(path, None, |path| Ok(resolve_policy_path(&path))) {
                Ok(path) => return path,
                Err(error) => *state.error.borrow_mut() = Some(error),
            }
        }
        // The enclosing resolution must reject this entire policy. Do not run
        // more filesystem work after its first failure.
        return crate::path_identity::normalize_path(path);
    }
    resolve_policy_path(path)
}

struct BoundedPolicyPaths {
    progress: std::sync::Arc<std::sync::Mutex<PathBuf>>,
    error: std::cell::RefCell<Option<std::io::Error>>,
}

thread_local! {
    static BOUNDED_POLICY_PATHS: std::cell::RefCell<Option<std::rc::Rc<BoundedPolicyPaths>>> = const { std::cell::RefCell::new(None) };
}

/// Resolve a complete policy with isolated filesystem probes, rejecting all of
/// its derived authority if any probe fails. The caller supplies the I/O budget.
pub fn with_bounded_policy_paths<T>(
    progress: std::sync::Arc<std::sync::Mutex<PathBuf>>,
    work: impl FnOnce() -> T,
) -> std::io::Result<T> {
    struct Restore(Option<std::rc::Rc<BoundedPolicyPaths>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            BOUNDED_POLICY_PATHS.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let state = std::rc::Rc::new(BoundedPolicyPaths {
        progress,
        error: Default::default(),
    });
    let _restore = Restore(BOUNDED_POLICY_PATHS.with(|slot| slot.replace(Some(state.clone()))));
    let value = work();
    let error = state.error.borrow_mut().take();
    match error {
        Some(error) => Err(error),
        None => Ok(value),
    }
}

fn resolve_policy_path(path: &Path) -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    path_probe::record(path_probe::Kind::Resolve, path);
    #[cfg(all(any(test, feature = "test-support"), unix))]
    crate::bounded_io::stall_running_helper_if_requested(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut ancestor = path.clone();
    let mut tail = Vec::new();
    loop {
        if let Ok(mut canonical) = ancestor.canonicalize() {
            for component in tail.iter().rev() {
                canonical.push(component);
            }
            return crate::path_identity::normalize_path(&canonical);
        }
        let Some(name) = ancestor.file_name().map(ToOwned::to_owned) else {
            return crate::path_identity::normalize_path(&path);
        };
        tail.push(name);
        if !ancestor.pop() {
            return crate::path_identity::normalize_path(&path);
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod path_probe {
    use super::Path;
    use std::cell::RefCell;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Kind {
        Resolve,
        SqliteFloor,
    }

    type Probe = Box<dyn Fn(Kind, &Path)>;
    thread_local! {
        static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
    }

    pub fn capture<T>(probe: impl Fn(Kind, &Path) + 'static, work: impl FnOnce() -> T) -> T {
        struct Restore(Option<Probe>);
        impl Drop for Restore {
            fn drop(&mut self) {
                PROBE.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let _restore = Restore(PROBE.with(|slot| slot.replace(Some(Box::new(probe)))));
        work()
    }

    pub fn record(kind: Kind, path: &Path) {
        PROBE.with(|slot| {
            if let Some(probe) = slot.borrow().as_ref() {
                probe(kind, path);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tail_keeps_identity_of_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing").join("leaf");
        // `policy_path` returns the normalized form (no Windows verbatim
        // `\\?\` prefix), so compare against the normalized ancestor.
        let ancestor = crate::path_identity::normalize_path(&dir.path().canonicalize().unwrap());
        assert_eq!(policy_path(&missing), ancestor.join("missing").join("leaf"));
    }

    #[test]
    fn bounded_scope_restores_outer_scope_after_unwind() {
        let progress = std::sync::Arc::new(std::sync::Mutex::new(PathBuf::new()));
        with_bounded_policy_paths(progress, || {
            let outer = BOUNDED_POLICY_PATHS.with(|slot| slot.borrow().clone().unwrap());
            let _ = std::panic::catch_unwind(|| {
                with_bounded_policy_paths(Default::default(), || panic!("unwind nested scope"))
                    .unwrap();
            });
            assert!(BOUNDED_POLICY_PATHS
                .with(|slot| std::rc::Rc::ptr_eq(slot.borrow().as_ref().unwrap(), &outer)));
        })
        .unwrap();
        assert!(BOUNDED_POLICY_PATHS.with(|slot| slot.borrow().is_none()));
    }
}

/// Compare resolved identities without probing the filesystem.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn contains_resolved(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix)
}

#[cfg(windows)]
pub fn contains_resolved(path: &Path, prefix: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "CompareStringOrdinal"]
        fn compare_string_ordinal(
            left: *const u16,
            left_len: i32,
            right: *const u16,
            right_len: i32,
            ignore_case: i32,
        ) -> i32;
    }
    let path = crate::path_identity::normalize_path(path);
    let prefix = crate::path_identity::normalize_path(prefix);
    let mut components = path.components();
    prefix.components().all(|expected| {
        let Some(actual) = components.next() else {
            return false;
        };
        let left = actual.as_os_str().encode_wide().collect::<Vec<_>>();
        let right = expected.as_os_str().encode_wide().collect::<Vec<_>>();
        let (Ok(left_len), Ok(right_len)) = (i32::try_from(left.len()), i32::try_from(right.len()))
        else {
            return false;
        };
        // Both UTF-16 buffers stay alive for the call, and the lengths are checked.
        // CompareStringOrdinal returns CSTR_EQUAL (2); errors conservatively
        // match a protected name. This comparator never grants access.
        unsafe {
            matches!(
                compare_string_ordinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1),
                0 | 2
            )
        }
    })
}

#[cfg(target_os = "macos")]
pub fn contains_resolved(path: &Path, prefix: &Path) -> bool {
    use std::ffi::c_void;
    use std::os::unix::ffi::OsStrExt;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        #[link_name = "CFStringCreateWithCharacters"]
        fn create_string(allocator: *const c_void, chars: *const u16, len: isize) -> *const c_void;
        #[link_name = "CFStringCompare"]
        fn compare_strings(left: *const c_void, right: *const c_void, options: usize) -> isize;
        #[link_name = "CFRelease"]
        fn release(value: *const c_void);
    }

    // Protected names include case and Unicode decomposition aliases on macOS.
    // Use these comparisons only to deny access, never to widen roots or grants.
    let mut components = path.components();
    prefix.components().all(|expected| {
        let Some(actual) = components.next() else {
            return false;
        };
        let (Some(left), Some(right)) =
            (actual.as_os_str().to_str(), expected.as_os_str().to_str())
        else {
            return actual
                .as_os_str()
                .as_bytes()
                .eq_ignore_ascii_case(expected.as_os_str().as_bytes());
        };
        let left = left.encode_utf16().collect::<Vec<_>>();
        let right = right.encode_utf16().collect::<Vec<_>>();
        // The buffers live through string creation. Every created CF string is
        // released, and allocation failure conservatively matches a protected name.
        unsafe {
            let left = create_string(std::ptr::null(), left.as_ptr(), left.len() as isize);
            let right = create_string(std::ptr::null(), right.as_ptr(), right.len() as isize);
            // Ignore case and treat different Unicode decompositions of a letter as equal.
            let equal =
                left.is_null() || right.is_null() || compare_strings(left, right, 1 | 16) == 0;
            if !left.is_null() {
                release(left);
            }
            if !right.is_null() {
                release(right);
            }
            equal
        }
    })
}
