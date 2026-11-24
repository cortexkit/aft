//! Filesystem identity used only for policy comparisons.

use std::path::{Path, PathBuf};

/// Canonicalize a path that may no longer exist (pending callgraph paths
/// legitimately include deleted files): canonicalize the nearest existing
/// ancestor of the ORIGINAL spelling and re-append the missing tail, so alias
/// spellings (macOS /var vs /private/var) normalize even for dead paths.
///
/// Symlink semantics match the callgraph store's `normalize_file_path`
/// (filesystem-first): `root/link/../x` where `link` targets a foreign
/// directory canonicalizes to the FOREIGN parent, not a lexical `root/x`.
/// Lexical `.`/`..` resolution applies only past the deepest existing
/// component (a nonexistent component cannot be a symlink) and to the tail
/// appended onto an already-canonical, symlink-free base.
/// Component-wise lenient canonicalization with filesystem-first semantics
/// (matching the callgraph store's `normalize_file_path`): each existing
/// component — including symlinks — resolves through the filesystem; genuinely
/// absent components accumulate on a missing stack and resolve lexically. `..`
/// pops the missing stack first, and only when the stack is empty does it take
/// the parent of the canonical base (symlink-free, so a lexical parent is
/// sound there). Handles re-entry: in `dead/../link/../x`, `dead/..` drains
/// back to the existing base and `link` (a symlink) resolves through the
/// filesystem instead of being erased lexically.
///
/// Returns `None` — and containment fails closed — where realpath would not
/// resolve either: a dangling symlink or other filesystem error on an existing
/// component (the store falls back to the raw spelling for those, which
/// `relative_path` keeps as an absolute out-of-root key), and `..` traversal
/// through a non-directory (realpath ENOTDIR).
pub(crate) fn canonicalize_lenient(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Some(canonical);
    }
    let mut resolved = PathBuf::new();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                resolved.push(component.as_os_str());
                // Canonicalize the anchor so a missing child directly under a
                // drive/UNC root compares in the same (verbatim) spelling as a
                // canonicalized root on Windows; "/" is a no-op on Unix.
                if let Ok(canonical_anchor) = std::fs::canonicalize(&resolved) {
                    resolved = canonical_anchor;
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if missing.pop().is_none() {
                    if !resolved.as_os_str().is_empty() && !resolved.is_dir() {
                        // `file/..` — realpath rejects with ENOTDIR.
                        return None;
                    }
                    resolved.pop();
                }
            }
            Component::Normal(name) => {
                if missing.is_empty() {
                    let candidate = resolved.join(name);
                    match std::fs::canonicalize(&candidate) {
                        Ok(canonical) => resolved = canonical,
                        Err(_) => match std::fs::symlink_metadata(&candidate) {
                            // Genuinely absent: lexical from here until `..`
                            // drains back.
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                missing.push(name.to_owned())
                            }
                            // Exists but does not canonicalize (dangling
                            // symlink) or the probe itself failed: fail closed.
                            _ => return None,
                        },
                    }
                } else {
                    missing.push(name.to_owned());
                }
            }
        }
    }
    for name in missing {
        resolved.push(name);
    }
    Some(resolved)
}

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
