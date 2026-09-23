use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aft::cache_freshness::{search_warm_verify_plan_for_test, seed_search_verify_memo_for_test};
use aft::commands::configure::ensure_project_watcher;
use aft::config::Config;
use aft::context::{App, AppContext};
use aft::search_index::{resolve_cache_dir, SearchIndex};

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Descriptor counting is a Unix-only observation (`/proc/self/fd`, `/dev/fd`);
/// on Windows the release is still exercised, just asserted via the watcher
/// count alone.
fn open_fd_count() -> Option<usize> {
    let fd_dir = [Path::new("/proc/self/fd"), Path::new("/dev/fd")]
        .into_iter()
        .find(|dir| dir.is_dir())?;
    Some(
        std::fs::read_dir(fd_dir)
            .expect("read process fd directory")
            .count(),
    )
}

// 90s: FSEvents teardown is a mach RPC that can take tens of seconds when the
// host fseventsd is saturated by other watcher-heavy processes; the assertion
// stays strict, only the settle window is generous.
const SETTLE_BUDGET: Duration = Duration::from_secs(90);

fn wait_for_watcher_count(ctx: &AppContext, expected: usize) {
    let deadline = Instant::now() + SETTLE_BUDGET;
    loop {
        let observed = ctx.watcher_registry_count();
        if observed == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "watcher count did not settle before deadline: expected={expected}, observed={observed}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_fd_count_at_most(maximum: usize) -> Option<usize> {
    let deadline = Instant::now() + SETTLE_BUDGET;
    loop {
        let observed = open_fd_count()?;
        if observed <= maximum {
            return Some(observed);
        }
        assert!(
            Instant::now() < deadline,
            "watcher descriptors did not close before deadline: maximum={maximum}, observed={observed}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn idle_reap_stops_real_watcher_releases_fd_and_rebind_forces_strict_verify() {
    let _serial = crate::helpers::watcher_serial_lock();
    let _watcher_enabled = EnvGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "0");
    let _sync_start = EnvGuard::set("AFT_TEST_SYNC_FILE_WATCHER_START", "1");
    let _force_reap = EnvGuard::set("AFT_TEST_ALLOW_FORCE_IDLE_REAP", "1");

    let root = tempfile::tempdir().expect("project root");
    let storage = tempfile::tempdir().expect("storage root");
    let source = root.path().join("main.rs");
    std::fs::write(&source, "fn marker() {}\n").expect("source file");
    let canonical_root = std::fs::canonicalize(root.path()).expect("canonical project root");

    let app = App::default_shared();
    let ctx = Arc::new(AppContext::from_app(
        app,
        Config {
            project_root: Some(canonical_root.clone()),
            storage_dir: Some(storage.path().to_path_buf()),
            indexes: aft::config::IndexesConfig {
                trigram: true,
                semantic: false,
                callgraph: true,
            },
            ..Config::default()
        },
    ));
    ctx.set_canonical_cache_root(canonical_root.clone());

    let cache_dir = resolve_cache_dir(&canonical_root, Some(storage.path()));
    let mut index = SearchIndex::build(&canonical_root);
    let git_head = index.stored_git_head().map(str::to_owned);
    index.write_to_disk(&cache_dir, git_head.as_deref());
    let cache_file = cache_dir.join("cache.bin");
    assert!(seed_search_verify_memo_for_test(
        &canonical_root,
        &cache_file
    ));
    assert_eq!(
        search_warm_verify_plan_for_test(&canonical_root, &cache_file),
        "skip"
    );

    let fds_before = open_fd_count();
    ensure_project_watcher(&ctx);
    wait_for_watcher_count(&ctx, 1);
    let fds_with_watcher = open_fd_count();
    assert_eq!(ctx.build_status_snapshot()["runtime"]["live_watchers"], 1);

    assert!(ctx.force_idle_teardown_for_test());
    wait_for_watcher_count(&ctx, 0);
    if let Some(before) = fds_before {
        let after = wait_for_fd_count_at_most(before);
        assert!(
            after.is_some_and(|after| after <= before),
            "watcher descriptors leaked: before={before}, live={fds_with_watcher:?}, after={after:?}"
        );
    }
    assert_eq!(
        search_warm_verify_plan_for_test(&canonical_root, &cache_file),
        "strict"
    );
    assert_eq!(ctx.build_status_snapshot()["runtime"]["live_watchers"], 0);

    ensure_project_watcher(&ctx);
    wait_for_watcher_count(&ctx, 1);
    assert_eq!(
        search_warm_verify_plan_for_test(&canonical_root, &cache_file),
        "strict"
    );

    ctx.stop_watcher_runtime_in_background();
    wait_for_watcher_count(&ctx, 0);
}
