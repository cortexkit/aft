use crate::tools_test_support::{
    direct_external_search_for_test, external_search_fixture as fixture,
};
use aft_engine::parent_search::external::ExternalSearch;
use aft_engine::test_bridge::display_data::ExternalSearchTestExt as _;

#[test]
fn external_search_hook_preserves_direct_child_response_bytes() {
    let (_fixture, caller, external, mut req) = fixture();
    // Pattern-only search has one callback, so its callback-count object has
    // no hash-map key ordering to obscure a byte-for-byte comparison.
    req.params = serde_json::json!({"pattern":"hook_parity_witness", "path":external});
    crate::subc::lower_hooks::register();
    let handle = ExternalSearch::of(&caller, &external).expect("published external checkout");
    let direct = direct_external_search_for_test(&req, &handle.engine());
    let (hooked, drift) = handle.search(&req, &caller);
    assert!(direct.success, "{direct:?}");
    assert_eq!(direct.data["fully_degraded"], false, "{direct:?}");
    assert_eq!(drift, 0);
    assert_eq!(
        serde_json::to_string(&hooked).unwrap(),
        serde_json::to_string(&direct).unwrap()
    );
}
