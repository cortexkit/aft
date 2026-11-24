use crate::bash_background::task_scope;

pub const LINUX_SCOPE_ENV: &str = crate::bash_background::registry::LINUX_SCOPE_ENV;

pub fn matches_membership(task_id: &str, membership: &str) -> bool {
    task_scope::matches_membership(task_id, membership)
}
use std::path::PathBuf;

pub fn with_local_recovery_mount_for_test<T>(
    current: PathBuf,
    mount: PathBuf,
    run: impl FnOnce() -> T,
) -> T {
    task_scope::with_local_recovery_mount_for_test(current, mount, run)
}

pub fn scope_cgroup(task: &str) -> Result<Option<PathBuf>, String> {
    task_scope::lookup(task).map(|scope| scope.map(|scope| scope.cgroup().to_path_buf()))
}
