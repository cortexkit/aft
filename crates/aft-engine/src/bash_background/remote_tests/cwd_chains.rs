use super::*;

fn rune_checkout() -> (tempfile::TempDir, RunRequest) {
    let (dir, request) = runon_cd_snapshot_fixture();
    let rune = Path::new(&request.workspace_key).join("rune");
    fs::create_dir(&rune).unwrap();
    fs::write(rune.join("Cargo.toml"), "# snapshot member\n").unwrap();
    (dir, request)
}

#[test]
fn runon_cwd_chains_restore_checkout_root_after_pipeline() {
    let (_dir, request) = rune_checkout();
    let command = r#"set -eo pipefail; cd rune && cargo fmt --check && cargo clippy --locked | head -6; cd ..; S=$(mktemp -d); rsync -a --exclude target --exclude .git ./ "$S/"; cd "$S/rune""#;
    let root = Path::new(&request.workspace_key);
    assert_eq!(runon_cd_refusal(command, &request).unwrap(), None);
    let targets = crate::bash_permissions::scan::top_level_directory_targets(command, root, &|p| {
        runon_snapshot_directory_known(
            root,
            p,
            Path::new("git"),
            Instant::now() + Duration::from_secs(2),
        )
    });
    let paths: Vec<_> = targets.into_iter().map(|target| target.path).collect();
    assert_eq!(paths, [root.join("rune"), root.to_path_buf()]);
}

#[test]
fn runon_cwd_chains_keep_checkout_subdirectory_for_return_path() {
    let (_dir, mut request) = rune_checkout();
    let command = r#"set -eo pipefail; cd rune && cargo fmt --check && cargo clippy --locked | tail -1 && cargo test --locked 2>&1 | grep -E 'test result' | head -4 && for c in board work timeline; do bash scripts/try.sh 520x300 unread-$c >/dev/null; done && bash scripts/try.sh 760x140 unread-board >/dev/null && mv out/unread-board.png out/unread-board-strip.png"#;
    assert_eq!(runon_cd_refusal(command, &request).unwrap(), None);
    request.return_paths = vec!["rune/out/unread-board-strip.png".into()];
    assert_eq!(runon_write_refusal(command, &request), None);
    request.return_paths = vec!["out/unread-board-strip.png".into()];
    let expected = Path::new(&request.cwd).join("rune/out/unread-board-strip.png");
    let refusal = runon_write_refusal(command, &request).expect("wrong return path must refuse");
    assert!(refusal.contains(expected.to_str().unwrap()), "{refusal}");
}

#[test]
fn runon_cwd_chains_refuse_escapes_after_unproven_or_child_cd() {
    let (_dir, request) = rune_checkout();
    for command in [
        "cd missing && x; cd ..",
        "(cd rune && cargo fmt --check); cd ..",
        "cd rune | x; cd ..",
        "echo \"$(cd rune && pwd)\"; cd ..",
    ] {
        let refusal = runon_cd_refusal(command, &request).unwrap();
        assert!(refusal.is_some(), "{command}");
    }
}

#[test]
fn runon_cd_sequential_command_after_setup_is_allowed() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    for package in ["plugin", "e2e-tests"] {
        let path = checkout.join("packages").join(package);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("source.ts"), "source\n").unwrap();
    }
    let command = "printf setup; cd packages/plugin && bun test src/__tests__; cd ../e2e-tests && bun test src/__tests__";
    assert_eq!(runon_cd_refusal(command, &request).unwrap(), None);
}
