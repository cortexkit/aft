pub mod index;
// CPU sampling shells out to `sample`/`atos` (macOS) or `perf`/`addr2line` (Linux);
// other targets get a named refusal from main.rs instead of a stub module.
pub mod probe_login_shell_path;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod profile;
pub mod sandbox_launch;
pub mod warmup;
