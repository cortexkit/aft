#[test]
fn runon_write_allows_reported_copy_to_dynamic_scratch_directory() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let command = "R=$TMPDIR/red; J=crates/.../claude_code/journal; cp $J.rs $R/$J.rs";
    assert!(runon_write_refusal(command, &request).is_none());
}

#[test]
fn runon_write_allows_unresolved_destinations() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    for command in [
        "cp a.rs $R/a.rs",
        "cp a.rs ${R}/a.rs",
        "cp a.rs $(pwd)/a.rs",
        "cp a.rs ~user/a.rs",
        "cp --target-directory=$R/dir x y",
        "echo > $R/a.rs",
        "J=src; J=$R; cp x $J/a.rs",
        "J=src & cp x $J/a.rs",
        "J=src cp x $J/a.rs",
    ] {
        assert!(
            runon_write_refusal(command, &request).is_none(),
            "{command}"
        );
    }
}

#[test]
fn runon_write_refuses_literal_and_resolved_destinations() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let root = Path::new(&request.workspace_key);
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "source\n").unwrap();
    fs::write(root.join("a.rs"), "source\n").unwrap();
    hermetic_git(root, &["add", "src/a.rs", "a.rs"]);

    for (command, path) in [
        ("J=src; cp x $J/a.rs", "src/a.rs"),
        ("J=src; cp x \"${J}/a.rs\"", "src/a.rs"),
        ("J=src; echo > $J/a.rs", "src/a.rs"),
        ("cp x ./a.rs", "a.rs"),
        ("echo > a.rs", "a.rs"),
    ] {
        let expected =
            RUNON_EDITS_CHECKOUT_MESSAGE.replace("<path>", &root.join(path).display().to_string());
        assert_eq!(
            runon_write_refusal(command, &request),
            Some(expected),
            "{command}"
        );
    }
}

#[test]
fn runon_write_return_paths_exempt_only_the_resolved_requested_file() {
    let (_dir, mut request) = runon_cd_snapshot_fixture();
    let root = Path::new(&request.workspace_key);
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/golden.rs"), "golden\n").unwrap();
    hermetic_git(root, &["add", "sub/golden.rs"]);
    request.cwd = root.join("sub").to_string_lossy().into_owned();
    request.return_paths = vec!["./golden.rs".into()];
    assert!(runon_write_refusal(
        "python3 -c 'open(\"golden.rs\", \"w\").write(\"new\")'",
        &request
    )
    .is_none());
    request.return_paths = vec!["other.rs".into()];
    assert!(runon_write_refusal("echo new > golden.rs", &request).is_some());
    request.return_paths = vec!["golden.rs".into()];
    assert!(runon_write_refusal("echo new > ../other.rs", &request).is_some());
}
