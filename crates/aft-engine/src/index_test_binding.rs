#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Storage folders handed out by `serve_text_index_for_test`, removed when
    /// the test's thread ends.
    static TEST_STORAGE: std::cell::RefCell<Vec<tempfile::TempDir>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Binds `ctx`'s index-store checkout over the files already on disk and
/// waits until its text index serves them. A context with no storage folder
/// gets a throwaway one, so a test never writes into the real storage.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn serve_text_index_for_test(ctx: &AppContext) {
    serve_text_for_test(ctx, true);
}

/// Binds the index store's checkout of `ctx`'s root and waits until its text
/// index serves, without waiting for the call graph: for a test that holds
/// the call graph's build to look at a checkout whose text alone is ready.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn serve_text_only_for_test(ctx: &AppContext) {
    serve_text_for_test(ctx, false);
}

#[cfg(any(test, feature = "test-support"))]
fn serve_text_for_test(ctx: &AppContext, whole_build: bool) {
    bind_for_test(ctx);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    // The text index publishes before the call graph; the build's last
    // publication changes the text generation answers are cached by, so a
    // test that compares answers across calls waits for the build to end.
    while text_plane_ready(ctx) != Some(true)
        || (whole_build
            && ctx
                .data_checkout()
                .is_some_and(|checkout| checkout.checkout().status().building))
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the text index never served: {:?}",
            report(ctx).map(|report| report.text.status)
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Binds the index store's checkout of `ctx`'s root, giving the context a
/// throwaway storage folder when it has none, without waiting for a build.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn bind_for_test(ctx: &AppContext) {
    if ctx.config().storage_dir.is_none() {
        let dir = tempfile::tempdir().expect("test storage folder");
        let path = dir.path().to_path_buf();
        TEST_STORAGE.with(|dirs| dirs.borrow_mut().push(dir));
        ctx.update_config(|config| config.storage_dir = Some(path));
    }
    if ctx.canonical_cache_root_opt().is_none() {
        if let Some(root) = ctx.config().project_root.clone() {
            ctx.set_canonical_cache_root(std::fs::canonicalize(&root).unwrap_or(root));
        }
    }
    ctx.data_checkout()
        .unwrap_or_else(|| panic!("the index store binds the root: {:?}", report(ctx)));
}
