//! Denies the current user read access to a path for a Windows test, and
//! reports whether this machine actually enforces the denial.

use std::path::{Path, PathBuf};

fn icacls_for_test(path: &Path, action: &str, rights: &str) {
    let output = std::process::Command::new("icacls")
        .arg(path)
        .args([action, rights])
        .output()
        .unwrap();
    assert!(output.status.success(), "icacls: {output:?}");
}

/// Restores read access to its path when dropped.
pub(crate) struct DeniedReadForTest(PathBuf);

impl Drop for DeniedReadForTest {
    fn drop(&mut self) {
        icacls_for_test(&self.0, "/remove:d", "*S-1-1-0");
    }
}

/// Denies everyone read-data access to `path` until the returned guard drops.
/// The flag says whether `probe`, run under the denial, was actually refused.
pub(crate) fn deny_read_for_test<T>(
    path: &Path,
    operation: &str,
    probe: impl FnOnce() -> std::io::Result<T>,
) -> (DeniedReadForTest, bool) {
    // Everyone's read-data right is also the directory-listing right. Naming
    // Everyone by its security identifier (SID, `S-1-1-0`) avoids depending on
    // the localized account name on the Windows runner.
    icacls_for_test(path, "/deny", "*S-1-1-0:(RD)");
    let restore = DeniedReadForTest(path.to_path_buf());
    // Installing an access control entry does not prove the process observes it.
    // Probe the operation the fixture needs denied before asserting denial-dependent behavior.
    let enforced = match probe() {
        Err(error) => {
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{error}"
            );
            eprintln!("ACL denial confirmed: {operation} at {}", path.display());
            true
        }
        Ok(_) => {
            eprintln!(
                "note: this machine cannot model ACL denial for {operation} at {}; \
                 the operation succeeded after icacls /deny; skipping only the \
                 ACL-dependent assertions, not the accounting checks",
                path.display()
            );
            false
        }
    };
    (restore, enforced)
}
