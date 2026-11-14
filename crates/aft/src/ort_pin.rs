pub use aft_base::ort_pin::*;

#[cfg(test)]
#[allow(unused_imports)]
use sha2::{Digest, Sha256};
#[cfg(test)]
#[allow(unused_imports)]
use std::ffi::{OsStr, OsString};
#[cfg(test)]
#[allow(unused_imports)]
use std::fs::File;
#[cfg(test)]
#[allow(unused_imports)]
use std::io::{Read, Seek, SeekFrom};
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Mutex, Once, OnceLock};

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn write_library(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write fake library");
        path
    }

    fn hash_of(path: &Path) -> String {
        sha256_file(path).expect("hash fake library")
    }

    fn policy<'a>(
        spawn_value: Option<&'a OsStr>,
        require_pinned: bool,
        managed_roots: &'a [PathBuf],
        pinned_sha256: &'a [&'a str],
    ) -> OrtPinPolicy<'a> {
        OrtPinPolicy {
            spawn_value,
            require_pinned,
            managed_roots,
            pinned_sha256,
        }
    }

    /// Stands in for the loader: reads whatever `load_path` names, as `dlopen`
    /// would map it. `None` (a bare-name load) is not expected here.
    fn read_as_loader(load_path: Option<&Path>) -> Result<Vec<u8>, String> {
        let load_path = load_path.ok_or("a pinned library must be loaded by path")?;
        std::fs::read(load_path).map_err(|error| error.to_string())
    }

    #[test]
    fn a_library_swapped_between_the_check_and_the_load_is_never_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "libonnxruntime.dylib", b"genuine build");
        let replacement = write_library(dir.path(), "replacement.dylib", b"swapped build");
        let hash = hash_of(&path);
        let pinned = [hash.as_str()];
        let roots = [dir.path().to_path_buf()];
        let policy = policy(None, true, &roots, &pinned);

        let swapped = Cell::new(None);
        let mapped = RefCell::new(None);
        let result = load_authorized_with(
            |candidate| open_authorized_with(&policy, candidate),
            Some(path.as_os_str()),
            || swapped.set(Some(std::fs::rename(&replacement, &path).is_ok())),
            |load_path| {
                *mapped.borrow_mut() = Some(read_as_loader(load_path)?);
                Ok(())
            },
        );
        // What the loader got is the checked file, on every platform.
        assert_eq!(*mapped.borrow(), Some(b"genuine build".to_vec()));
        if cfg!(unix) {
            // The rename succeeded, and the path now names the replacement.
            assert_eq!(swapped.get(), Some(true), "the swap ran");
            assert_eq!(std::fs::read(&path).unwrap(), b"swapped build");
            // The load is still reported as failed: the rename unlinked the
            // checked file, which moves its change time, and the check after
            // the load treats that as a change. A library replaced mid-load
            // means something is writing to the folder.
            let error = result.expect_err("a swap during the load is reported");
            assert!(
                error.starts_with("onnx runtime library changed while it was being checked"),
                "{error}"
            );
        } else {
            // The held file cannot be replaced at all.
            assert_eq!(swapped.get(), Some(false), "the swap was refused");
            assert_eq!(result, Ok(()));
        }
    }

    #[test]
    fn a_library_rewritten_in_place_after_the_check_is_reported_and_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "libonnxruntime.dylib", b"genuine build");
        let hash = hash_of(&path);
        let pinned = [hash.as_str()];
        let roots = [dir.path().to_path_buf()];
        let policy = policy(None, true, &roots, &pinned);

        let rewritten = Cell::new(None);
        let result = load_authorized_with(
            |candidate| open_authorized_with(&policy, candidate),
            Some(path.as_os_str()),
            // Same length, so only the content (and the change time) differs.
            || rewritten.set(Some(std::fs::write(&path, b"hostile build").is_ok())),
            read_as_loader,
        );
        assert_eq!(
            rewritten.get(),
            Some(cfg!(unix)),
            "the rewrite ran as expected"
        );
        if cfg!(unix) {
            // No user-space lock stops a same-user writer on Unix; the rewrite
            // is caught after the load, and the load is reported as failed.
            let error = result.expect_err("a library rewritten after its check must not be used");
            assert!(
                error.starts_with(&format!(
                    "onnx runtime library changed while it was being checked at {}",
                    path.display()
                )),
                "{error}"
            );
            assert!(
                !crate::semantic_index::is_onnx_runtime_unavailable(&error),
                "a rewritten library is not a missing runtime: {error}"
            );
        } else {
            // Windows refuses the write while the file is held.
            assert_eq!(result, Ok(b"genuine build".to_vec()));
        }
    }

    #[test]
    fn a_pinned_library_is_loaded_through_the_checked_file_not_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "libonnxruntime.dylib", b"genuine build");
        let hash = hash_of(&path);
        let pinned = [hash.as_str()];
        let roots = [dir.path().to_path_buf()];
        let library =
            open_authorized_with(&policy(None, true, &roots, &pinned), Some(path.as_os_str()))
                .expect("a pinned library is admitted");
        let load_path = library
            .load_path()
            .expect("a pinned library has a load path");
        #[cfg(target_os = "macos")]
        assert!(load_path.starts_with("/dev/fd"), "{}", load_path.display());
        #[cfg(all(unix, not(target_os = "macos")))]
        assert!(
            load_path.starts_with("/proc/self/fd"),
            "{}",
            load_path.display()
        );
        #[cfg(windows)]
        assert_eq!(load_path, path);
        assert_eq!(library.confirm_unchanged(), Ok(()));
    }

    #[test]
    fn an_operator_library_is_loaded_by_its_own_path() {
        let operator_dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let path = write_library(operator_dir.path(), "libonnxruntime.dylib", b"brew build");
        let roots = [storage.path().join("onnxruntime")];
        let pinned: [&str; 0] = [];
        let library = open_authorized_with(
            &policy(Some(path.as_os_str()), true, &roots, &pinned),
            Some(path.as_os_str()),
        )
        .expect("the operator's spawn-time choice is admitted");
        assert_eq!(library.load_path(), Some(path.clone()));
        // Its bytes are accepted whatever they are, so there is nothing to
        // re-check after the load.
        std::fs::write(&path, b"other build").unwrap();
        assert_eq!(library.confirm_unchanged(), Ok(()));
    }

    #[test]
    fn a_bare_name_load_has_no_path() {
        let roots: [PathBuf; 0] = [];
        let pinned: [&str; 0] = [];
        let library = open_authorized_with(&policy(None, false, &roots, &pinned), None)
            .expect("standalone bare-name loads are allowed");
        assert_eq!(library.load_path(), None);
    }

    /// A real ONNX Runtime the pin accepts, if this machine has one:
    /// `AFT_TEST_ORT_LIBRARY_DIR`, else the runtime AFT manages for this user.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn real_pinned_runtime() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        const NAME: &str = "libonnxruntime.dylib";
        #[cfg(target_os = "linux")]
        const NAME: &str = "libonnxruntime.so";
        let dir = std::env::var_os("AFT_TEST_ORT_LIBRARY_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                let home = PathBuf::from(std::env::var_os("HOME")?);
                Some(home.join(".local/share/cortexkit/aft/onnxruntime/1.24.4"))
            })?;
        let library = dir.join(NAME);
        let hash = sha256_file(&library).ok()?;
        pinned_hashes().contains(&hash.as_str()).then_some(library)
    }

    /// The first two entries of ONNX Runtime's `OrtApiBase`.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[repr(C)]
    struct OrtApiBase {
        _get_api: unsafe extern "C" fn(u32) -> *const std::ffi::c_void,
        get_version_string: unsafe extern "C" fn() -> *const std::ffi::c_char,
    }

    /// The real loader, the real library: a file swapped in after the check is
    /// not what `dlopen` maps. Loading by path here would fail (the
    /// replacement is not a library at all); loading through the descriptor
    /// maps the checked ONNX Runtime and reports its version.
    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn real_dlopen_maps_the_checked_runtime_after_a_swap() {
        let Some(real) = real_pinned_runtime() else {
            eprintln!(
                "skipping: no pinned ONNX Runtime on this machine (AFT_TEST_ORT_LIBRARY_DIR)"
            );
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(real.file_name().unwrap());
        std::fs::copy(&real, &path).unwrap();
        let replacement = write_library(dir.path(), "replacement", b"not a library");
        let pinned = pinned_hashes();
        let roots = [dir.path().to_path_buf()];
        let policy = policy(None, true, &roots, &pinned);

        let mapped_version = RefCell::new(None);
        let result = load_authorized_with(
            |candidate| open_authorized_with(&policy, candidate),
            Some(path.as_os_str()),
            || std::fs::rename(&replacement, &path).unwrap(),
            |load_path| {
                let load_path = load_path.ok_or("a pinned library is loaded by path")?;
                let c_path = std::ffi::CString::new(load_path.to_str().unwrap()).unwrap();
                let symbol_name = std::ffi::CString::new("OrtGetApiBase").unwrap();
                // SAFETY: loads a pinned Microsoft build and calls its
                // documented entry point; the handle is closed before return.
                unsafe {
                    let handle = libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW);
                    if handle.is_null() {
                        let error = std::ffi::CStr::from_ptr(libc::dlerror());
                        return Err(error.to_string_lossy().into_owned());
                    }
                    let symbol = libc::dlsym(handle, symbol_name.as_ptr());
                    assert!(!symbol.is_null(), "OrtGetApiBase is exported");
                    let get_api_base: unsafe extern "C" fn() -> *const OrtApiBase =
                        std::mem::transmute(symbol);
                    let version =
                        std::ffi::CStr::from_ptr(((*get_api_base()).get_version_string)())
                            .to_string_lossy()
                            .into_owned();
                    libc::dlclose(handle);
                    *mapped_version.borrow_mut() = Some(version);
                    Ok(())
                }
            },
        );
        assert_eq!(*mapped_version.borrow(), Some("1.24.4".to_string()));
        // The version came from the checked library, but the rename itself
        // is still reported as a failed load, as in the swap test above.
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn sha256_file_matches_a_known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "lib", b"abc");
        assert_eq!(
            hash_of(&path),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn pinned_library_is_authorized_and_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "libonnxruntime.dylib", b"genuine build");
        let hash = hash_of(&path);
        let pinned = [hash.as_str()];
        let roots = [dir.path().to_path_buf()];
        // Inside the managed folder and not named by ORT_DYLIB_PATH at spawn,
        // so only a matching pinned hash can admit it.
        let policy = policy(None, true, &roots, &pinned);

        let loads = Cell::new(0);
        let result = load_authorized_with(
            |candidate| open_authorized_with(&policy, candidate),
            Some(path.as_os_str()),
            || {},
            |loaded| {
                // The loader gets a path naming the checked file (not
                // necessarily its original path; see `load_path`).
                assert_eq!(read_as_loader(loaded), Ok(b"genuine build".to_vec()));
                loads.set(loads.get() + 1);
                Ok(())
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(loads.get(), 1);
        assert_eq!(
            authorize_with(&policy, Some(path.as_os_str())),
            Ok(OrtLoadAuthorization::Pinned {
                path: path.clone(),
                sha256: hash
            })
        );
    }

    #[test]
    fn mismatched_library_is_refused_and_never_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "libonnxruntime.dylib", b"tampered build");
        let pinned = ["872533f130f1839a5bc01788ddb4f75c83a189763441ba1178788ed965449289"];
        let roots = [dir.path().to_path_buf()];
        let policy = policy(None, false, &roots, &pinned);

        let loads = Cell::new(0);
        let error = load_authorized_with(
            |candidate| open_authorized_with(&policy, candidate),
            Some(path.as_os_str()),
            || {},
            |_| {
                loads.set(loads.get() + 1);
                Ok(())
            },
        )
        .expect_err("a library off the pinned list must be refused");
        assert_eq!(
            loads.get(),
            0,
            "a refused library must never reach the loader"
        );
        assert!(
            error.starts_with(&format!(
                "onnx runtime library hash mismatch at {}",
                path.display()
            )),
            "{error}"
        );
        assert!(
            !crate::semantic_index::is_onnx_runtime_unavailable(&error),
            "a tampered library is not a missing runtime: {error}"
        );
    }

    #[test]
    fn spawn_time_operator_path_outside_the_managed_folder_is_accepted_unpinned() {
        let operator_dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let path = write_library(operator_dir.path(), "libonnxruntime.dylib", b"brew build");
        let roots = [storage.path().join("onnxruntime")];
        let pinned = ["872533f130f1839a5bc01788ddb4f75c83a189763441ba1178788ed965449289"];
        for require_pinned in [false, true] {
            let policy = policy(Some(path.as_os_str()), require_pinned, &roots, &pinned);
            assert_eq!(
                authorize_with(&policy, Some(path.as_os_str())),
                Ok(OrtLoadAuthorization::OperatorUnpinned {
                    path: path.clone(),
                    sha256: hash_of(&path)
                }),
                "require_pinned={require_pinned}"
            );
        }
    }

    #[test]
    fn spawn_time_path_inside_the_managed_folder_must_still_match_the_pin() {
        let storage = tempfile::tempdir().unwrap();
        let version_dir = storage.path().join("onnxruntime").join("1.24.4");
        std::fs::create_dir_all(&version_dir).unwrap();
        let path = write_library(&version_dir, "libonnxruntime.dylib", b"swapped build");
        let roots = [storage.path().join("onnxruntime")];
        let pinned = ["872533f130f1839a5bc01788ddb4f75c83a189763441ba1178788ed965449289"];
        // The bridge passes its managed path at spawn; that is AFT's own
        // download, so a bad hash is refused whether or not the process is
        // the supervised module (require_pinned true or false).
        for require_pinned in [false, true] {
            let policy = policy(Some(path.as_os_str()), require_pinned, &roots, &pinned);
            let error = authorize_with(&policy, Some(path.as_os_str()))
                .expect_err("a managed library with a bad hash must be refused");
            assert!(
                error.starts_with("onnx runtime library hash mismatch at"),
                "require_pinned={require_pinned}: {error}"
            );
        }
    }

    #[test]
    fn managed_folder_reached_through_a_symlink_is_still_managed() {
        #[cfg(unix)]
        {
            let storage = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let version_dir = storage.path().join("onnxruntime").join("1.24.4");
            std::fs::create_dir_all(&version_dir).unwrap();
            let real = write_library(&version_dir, "libonnxruntime.dylib", b"swapped build");
            let link = outside.path().join("libonnxruntime.dylib");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let roots = [storage.path().join("onnxruntime")];
            let pinned: [&str; 0] = [];
            let policy = policy(Some(link.as_os_str()), false, &roots, &pinned);
            assert!(authorize_with(&policy, Some(link.as_os_str())).is_err());
        }
    }

    #[test]
    fn a_value_aft_exported_itself_is_not_an_operator_override() {
        // The process starts with no ORT_DYLIB_PATH; AFT later resolves its
        // managed download and exports it. The spawn capture must still read
        // "unset", so the exported path is held to the pin.
        let env: RefCell<Option<OsString>> = RefCell::new(None);
        let spawn = SpawnEnvValue::new();
        assert_eq!(spawn.capture_with(|| env.borrow().clone()), None);

        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "libonnxruntime.dylib", b"swapped build");
        *env.borrow_mut() = Some(path.as_os_str().to_os_string());

        let spawn_value = spawn.capture_with(|| env.borrow().clone());
        assert_eq!(spawn_value, None, "the first capture must win");
        let roots: [PathBuf; 0] = [];
        let pinned = ["872533f130f1839a5bc01788ddb4f75c83a189763441ba1178788ed965449289"];
        let policy = policy(spawn_value, false, &roots, &pinned);
        let error = authorize_with(&policy, Some(path.as_os_str()))
            .expect_err("AFT's own export is not the operator's choice");
        assert!(error.starts_with("onnx runtime library hash mismatch at"));
    }

    #[test]
    fn spawn_capture_keeps_a_value_present_at_spawn() {
        let spawn = SpawnEnvValue::new();
        let value = OsString::from("/opt/onnxruntime/libonnxruntime.so");
        assert_eq!(
            spawn.capture_with(|| Some(value.clone())),
            Some(value.as_os_str())
        );
        assert_eq!(
            spawn.capture_with(|| None),
            Some(value.as_os_str()),
            "a later unset must not erase the spawn value"
        );
        let empty = SpawnEnvValue::new();
        assert_eq!(empty.capture_with(|| Some(OsString::new())), None);
    }

    #[test]
    fn bare_name_load_is_refused_under_the_supervised_module() {
        let roots: [PathBuf; 0] = [];
        let pinned: [&str; 0] = [];
        let error = authorize_with(&policy(None, true, &roots, &pinned), None)
            .expect_err("the supervised module must not load by bare name");
        assert!(
            error.starts_with(ONNX_RUNTIME_MISSING_PREFIX),
            "the wait-for-download flow keys on this prefix: {error}"
        );
    }

    #[test]
    fn bare_name_load_is_allowed_standalone() {
        let roots: [PathBuf; 0] = [];
        let pinned: [&str; 0] = [];
        assert_eq!(
            authorize_with(&policy(None, false, &roots, &pinned), None),
            Ok(OrtLoadAuthorization::LoaderSearchUnpinned)
        );
        assert_eq!(
            authorize_with(&policy(None, false, &roots, &pinned), Some(OsStr::new(""))),
            Ok(OrtLoadAuthorization::LoaderSearchUnpinned),
            "an empty ORT_DYLIB_PATH is unset"
        );
    }

    #[test]
    fn unreadable_library_reports_a_missing_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent").join("libonnxruntime.dylib");
        let roots: [PathBuf; 0] = [];
        let pinned: [&str; 0] = [];
        let error = authorize_with(
            &policy(Some(path.as_os_str()), false, &roots, &pinned),
            Some(path.as_os_str()),
        )
        .expect_err("a file that cannot be hashed cannot be loaded");
        assert!(error.starts_with(ONNX_RUNTIME_MISSING_PREFIX), "{error}");
        assert!(error.contains("doctor --fix"), "{error}");
    }

    #[test]
    fn installer_pins_the_same_library_hashes() {
        // The bridge installer (onnx-runtime.ts, `librarySha256`) checks a
        // download against its own copy of PINNED_ORT_LIBRARIES before
        // publishing it. If the two lists drift apart, the installer publishes
        // builds AFT then refuses, or refuses builds AFT would load.
        let installer = Path::new(crate::test_support::manifest_dir())
            .join("../../packages/aft-bridge/src/onnx-runtime.ts");
        let Ok(source) = std::fs::read_to_string(&installer) else {
            eprintln!("skipping: {} not present", installer.display());
            return;
        };
        let installer_pins: Vec<&str> = source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("librarySha256: \""))
            .filter_map(|rest| rest.split('"').next())
            .collect();
        let mut rust_pins: Vec<&str> = PINNED_ORT_LIBRARIES
            .iter()
            .filter(|library| library.asset.ends_with(".nupkg"))
            .map(|library| library.sha256)
            .collect();
        let mut installer_sorted = installer_pins.clone();
        rust_pins.sort_unstable();
        installer_sorted.sort_unstable();
        assert_eq!(installer_sorted, rust_pins);
    }

    #[test]
    fn pinned_list_covers_every_platform_the_installer_downloads() {
        for library in [
            "runtimes/osx-arm64/native/libonnxruntime.dylib",
            "runtimes/linux-x64/native/libonnxruntime.so",
            "runtimes/linux-arm64/native/libonnxruntime.so",
            "runtimes/win-x64/native/onnxruntime.dll",
            "runtimes/win-arm64/native/onnxruntime.dll",
        ] {
            let entry = PINNED_ORT_LIBRARIES
                .iter()
                .find(|entry| entry.library == library)
                .unwrap_or_else(|| panic!("no pin for {library}"));
            assert_eq!(entry.sha256.len(), 64, "{library}");
            assert!(
                entry
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "{library}: pins are lowercase hex"
            );
        }
    }

    #[test]
    fn accepts_the_legacy_macos_cache_hash() {
        assert!(PINNED_ORT_LIBRARIES.iter().any(|entry| {
            entry.sha256 == "872533f130f1839a5bc01788ddb4f75c83a189763441ba1178788ed965449289"
        }));
    }
}
