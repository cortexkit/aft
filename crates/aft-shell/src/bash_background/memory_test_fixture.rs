#[cfg(unix)]
use super::*;

#[cfg(unix)]
pub const MEMORY_PRESSURE_COMMAND: &str = "python3 -c 'import time; allocation = bytearray(b\"x\" * (192 * 1024 * 1024)); time.sleep(20)'";

pub fn report_memory_runtime_skip(name: &str, reason: &str) {
    use std::io::Write;
    // Bypass libtest's passing-test capture so capability skips remain visible in CI.
    writeln!(std::io::stderr().lock(), "SKIP {name}: {reason}").unwrap();
}

#[cfg(unix)]
pub fn with_prepare_gate<R>(gate: impl FnOnce() + 'static, run: impl FnOnce() -> R) -> R {
    super::super::with_memory_prepare_gate_for_test(gate, run)
}

#[cfg(unix)]
pub fn persisted_cap(metadata: &PersistedTask) -> Option<u64> {
    metadata.memory_policy.as_ref().map(|policy| policy.mib)
}

#[cfg(target_os = "linux")]
pub fn sampling_available_for_test() -> bool {
    crate::bash_background::memory_limit::sampling_available_for_test()
}

#[cfg(target_os = "linux")]
pub struct AttachFaultGuard(crate::bash_background::memory_limit::AttachFaultGuard);

#[cfg(target_os = "linux")]
impl AttachFaultGuard {
    pub fn for_task(task: String, path: PathBuf, errno: i32) -> Self {
        Self(crate::bash_background::memory_limit::AttachFaultGuard::for_task(task, path, errno))
    }
    pub fn attempts(&self) -> usize {
        self.0.attempts()
    }
}
