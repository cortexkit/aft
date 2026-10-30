pub mod backups;
pub mod clean_old_caches;
pub mod fix_config;
pub mod index;
pub mod probe_login_shell_path;
// `aft cache prune-legacy`, the explicit legacy-cache deletion command.
pub mod prune_legacy;
// Compiled everywhere: the memory census view has no OS dependency, and the
// CPU sampler (`sample`/`atos` on macOS, `perf`/`addr2line` on Linux) refuses
// by name at runtime where it is unsupported.
pub mod profile;
pub mod sandbox_launch;
pub mod setup;
pub mod warmup;
