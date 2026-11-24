#[test]
fn checkout_control_denies_preserve_nested_and_non_repository_root_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&root)
        .status()
        .unwrap()
        .success());
    let nested = root.join("src");
    std::fs::create_dir(&nested).unwrap();
    let nonrepo = tempfile::tempdir().unwrap();
    let nonrepo = nonrepo.path().canonicalize().unwrap();
    for path in [&nested, &nonrepo] {
        // Project roots without a local .git do not discover an ancestor
        // repository. Grants keep that behavior and add no ancestor paths.
        let git = resolve_git_policy(path).unwrap();
        assert!(git.read_roots.is_empty());
        let denies = checkout_control_denies(path, &git).unwrap();
        assert!(denies.iter().all(|deny| deny.starts_with(path)));
        assert!(denies.contains(&path.join(".cortexkit/aft.jsonc")));
        assert!(denies.contains(&path.join(".git/config")));
    }
}
