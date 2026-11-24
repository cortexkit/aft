pub use aft_shell::bash_background::*;

pub fn task_storage_dir(ctx: &crate::context::AppContext) -> std::path::PathBuf {
    aft_shell::bash_background::task_storage_dir(ctx)
}
