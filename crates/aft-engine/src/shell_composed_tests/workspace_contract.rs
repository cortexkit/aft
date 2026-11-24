#[test]
fn exec_remote_single_locked_contract_is_pinned() {
    let lock = std::path::Path::new(crate::test_support::manifest_dir()).join("../../Cargo.lock");
    let lock: toml::Value = std::fs::read_to_string(lock).unwrap().parse().unwrap();
    let packages: Vec<_> = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["name"].as_str() == Some("cortexkit-exec-remote-types"))
        .collect();
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0]["version"].as_str(), Some("0.2.22"));
}

#[test]
fn exec_remote_published_vectors_use_one_locked_contract() {
    // Runtime cargo metadata resolves unrelated target dependencies even in
    // offline mode, so unit tests embed the corpus rather than requiring those
    // packages in the caller's Cargo cache. Keep the singleton version fence
    // explicit when updating the dependency.
    let lock: toml::Value = toml::from_str(include_str!("../../../../Cargo.lock")).unwrap();
    let versions = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|package| package["name"].as_str() == Some("cortexkit-exec-remote-types"))
        .map(|package| package["version"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(versions, ["0.2.22"], "exactly one caller contract version");
}
