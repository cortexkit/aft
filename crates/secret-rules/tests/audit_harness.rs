//! Regression tests for the Athena audit item 14: the acceptance harness must
//! fail on a missing group, must record header replacement bytes, and must
//! detect leaks with its own decoder instead of the production matcher set.

mod support;

#[test]
fn a_missing_group_fails_instead_of_skipping() {
    let error = support::require_group("secret_rules_v1.json", "NO_SUCH_GROUP")
        .expect_err("a missing group must be reported");
    assert!(error.contains("NO_SUCH_GROUP"), "unhelpful error: {error}");
}

#[test]
fn a_present_group_loads_and_reports_its_case_count() {
    let cases = support::require_group("secret_rules_v1.json", "P1").expect("P1 exists");
    assert!(!cases.is_empty());
}

#[test]
fn every_group_lives_in_the_file_its_runner_loads() {
    // The check names the file for each group, so a group moved to a file the
    // runner never loads fails here instead of passing as "present".
    support::check_groups_present();
}

#[test]
fn the_independent_leak_detector_finds_every_common_encoding() {
    let key: &[u8] = b"0123456789abcdef";
    let encodings: Vec<Vec<u8>> = vec![
        key.to_vec(),
        b"0123456789%61bcdef".to_vec(),
        b"30313233343536373839616263646566".to_vec(),
        b"0123456789abcdef".to_vec(),
    ];
    for encoding in encodings {
        assert!(
            support::discloses(key, &encoding),
            "detector missed {:?}",
            String::from_utf8_lossy(&encoding)
        );
    }
    // The placeholder itself must not look like a leak, or every case would
    // "leak" and the detector would prove nothing.
    assert!(!support::discloses(key, b"{{ck-secret:x}}"));
    assert!(!support::discloses(
        b"\xde\xad\xbe\xef\xca\xfe\x01\x23\xbe\xef\x45\x67\xaa\xbb\xcc\xdd",
        b"{{ck-secret:repo_key}}"
    ));
}
