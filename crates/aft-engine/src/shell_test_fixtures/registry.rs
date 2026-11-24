use crate::bash_background::registry::BgTask;
use aft_shell::bash_background::registry::test_support::{
    create_wait_fixture, mark_fixture_terminal,
};
use std::sync::Arc;

pub use aft_shell::bash_background::registry::test_support::*;

pub fn wait_fixture(
    explicit_timeout: Option<u64>,
) -> (tempfile::TempDir, crate::context::AppContext, Arc<BgTask>) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = crate::context::AppContext::from_app(
        crate::context::App::default_shared(),
        crate::config::Config {
            project_root: Some(dir.path().to_path_buf()),
            storage_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        },
    );
    let task = create_wait_fixture(dir.path(), ctx.bash_background(), explicit_timeout);
    (dir, ctx, task)
}

pub fn terminal_fixture() -> (tempfile::TempDir, crate::context::AppContext, Arc<BgTask>) {
    let (dir, ctx, task) = wait_fixture(None);
    assert_eq!(ctx.config().bash.worker_wait_max_ms, 1_800_000);
    mark_fixture_terminal(&task);
    (dir, ctx, task)
}
