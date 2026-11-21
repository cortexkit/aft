use crate::bash_background::task_scope;
use std::path::PathBuf;

pub fn with_local_recovery_mount_for_test<T>(
    current: PathBuf,
    mount: PathBuf,
    run: impl FnOnce() -> T,
) -> T {
    task_scope::with_local_recovery_mount_for_test(current, mount, run)
}
