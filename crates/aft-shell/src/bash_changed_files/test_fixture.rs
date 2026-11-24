fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap()
        .success());
}
fn repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    git(
        root.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(root.path(), &["config", "user.name", "Test"]);
    std::fs::write(root.path().join("clean.txt"), "old\n").unwrap();
    std::fs::write(root.path().join("deleted.txt"), "gone\n").unwrap();
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "initial"]);
    root
}
fn event(journal: &Journal, root: &Path, name: &str, kind: notify::EventKind) {
    journal.event(&notify::Event::new(kind).add_path(root.join(name)));
}
