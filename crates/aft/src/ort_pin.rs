pub use aft_base::ort_pin::*;

#[cfg(test)]
#[allow(unused_imports)]
use sha2::Digest;
#[cfg(test)]
#[allow(unused_imports)]
use sha2::Sha256;
#[cfg(test)]
#[allow(unused_imports)]
use std::ffi::OsStr;
#[cfg(test)]
#[allow(unused_imports)]
use std::ffi::OsString;
#[cfg(test)]
#[allow(unused_imports)]
use std::fs::File;
#[cfg(test)]
#[allow(unused_imports)]
use std::io::Read;
#[cfg(test)]
#[allow(unused_imports)]
use std::io::Seek;
#[cfg(test)]
#[allow(unused_imports)]
use std::io::SeekFrom;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::PathBuf;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::AtomicBool;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::Ordering;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Mutex;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Once;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::OnceLock;

#[cfg(test)]
#[allow(hidden_glob_reexports)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    pub(crate) use aft_base::ort_pin::tests::*;
    use std::cell::Cell;

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
}
