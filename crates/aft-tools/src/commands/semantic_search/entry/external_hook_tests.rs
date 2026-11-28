use super::*;
use crate::commands::data_search::ExternalSearch;
use crate::tools_test_support::external_search_fixture as fixture;

#[test]
fn external_search_without_hook_uses_existing_bounded_fallback() {
    const CHILD: &str = "AFT_TEST_EXTERNAL_SEARCH_WITHOUT_HOOK";
    if std::env::var_os(CHILD).is_none() {
        let name = concat!(
            module_path!(),
            "::external_search_without_hook_uses_existing_bounded_fallback"
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let (_fixture, caller, external, req) = fixture();
    assert!(ExternalSearch::of(&caller, &external).is_none());
    // Avoid the public entry's registration, but run the same request body.
    let reply = crate::bounded_io::with_skip_disclosure(|| {
        handle_semantic_search_with_disclosure(&req, &caller)
    });
    assert!(reply.success, "{reply:?}");
    assert_eq!(
        reply.data["semantic_status"], "external_unindexed",
        "{reply:?}"
    );
    let expected_path = external.join("src").join("lib.rs");
    assert!(
        reply.data["text"]
            .as_str()
            .unwrap()
            .contains(&expected_path.display().to_string()),
        "{reply:?}"
    );
}
