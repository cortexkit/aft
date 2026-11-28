use super::*;

pub(super) fn suffix_fixture() -> Fixture {
    fixture_with_worker_directory(
        FakeCingulate::up(),
        tempfile::tempdir().unwrap(),
        Arc::new(Executor::new()),
        "broca-call-yield",
    )
}

pub(super) fn worker_suffix_file(f: &Fixture, relative: &str) -> PathBuf {
    let path: PathBuf = f.worker.project_root.join(relative).components().collect();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "pub fn suffix_marker() {}\n").unwrap();
    path
}

pub(super) fn build_suffix_inventory(f: &Fixture) {
    let checkout = aft_engine::data_layer::runtime::DataCheckout::open_planes(
        &f.worker.project_root,
        &f.dir.path().join("suffix-index"),
        None,
        crate::data_layer::checkout::Planes::ALL,
    )
    .unwrap();
    assert!(matches!(
        checkout.fresh_view(Duration::from_secs(120)),
        aft_engine::data_layer::runtime::Fresh::Ready(_)
    ));
    f.ctx.pin_data_checkout(checkout);
    assert!(matches!(
        f.ctx.text_view(),
        crate::data_layer::host::TextView::Ready(_)
    ));
}

#[tokio::test]
async fn consent_worker_read_suggests_unique_longest_indexed_suffix_without_asking() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = suffix_fixture();
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let counterpart = worker_suffix_file(&f, "crates/broca-session-loop/src/run.rs");
    worker_suffix_file(&f, "crates/another-crate/src/run.rs");
    build_suffix_inventory(&f);
    let requested: PathBuf = f
        .dir
        .path()
        .join("broca-session-loop/src/run.rs")
        .components()
        .collect();
    assert!(!requested.exists());

    let reply = f.read(&f.worker, &requested, 7).await;

    assert_eq!(code(&reply), "path_not_found_did_you_mean", "{reply}");
    assert_eq!(
        text(&reply),
        format!(
            "path not found: {}; did you mean {}?",
            requested.display(),
            counterpart.display()
        )
    );
    assert!(f.fake.calls("consent.request").is_empty());
    assert!(f.asks().is_empty());
}

#[cfg(test)]
use aft_engine::test_bridge::display_data::IndexHost as _;

use aft_engine::test_bridge::display_data::DataCheckoutTestExt as _;
