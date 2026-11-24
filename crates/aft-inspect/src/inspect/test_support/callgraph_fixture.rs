#[cfg(test)]
thread_local! {
    static FIXTURE_STORAGE: std::cell::RefCell<Vec<tempfile::TempDir>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh index-store call graph for scanner fixtures. Keep its files until
/// the test thread exits because the snapshot reads them lazily.
#[cfg(test)]
pub fn view_for_test(root: &std::path::Path) -> crate::data_layer::checkout::CheckoutView {
    let storage = tempfile::tempdir().expect("scanner fixture storage");
    let checkout = crate::data_layer::runtime::DataCheckout::open_planes(
        root,
        storage.path(),
        None,
        crate::data_layer::checkout::Planes {
            text: false,
            callgraph: true,
        },
    )
    .expect("bind scanner fixture");
    let view = match checkout.fresh_view(std::time::Duration::from_secs(60)) {
        crate::data_layer::runtime::Fresh::Ready(view) => view,
        crate::data_layer::runtime::Fresh::Building => {
            panic!("scanner fixture call graph did not publish")
        }
        crate::data_layer::runtime::Fresh::Failed(reason) => {
            panic!("scanner fixture call graph failed: {reason}")
        }
    };
    FIXTURE_STORAGE.with(|folders| folders.borrow_mut().push(storage));
    view
}

#[cfg(test)]
pub fn snapshot_for_test(root: &std::path::Path) -> crate::inspect::job::CallgraphSnapshot {
    view_for_test(root)
        .dead_code_snapshot(&crate::inspect::InspectEntryPoints)
        .expect("scanner fixture snapshot")
}
