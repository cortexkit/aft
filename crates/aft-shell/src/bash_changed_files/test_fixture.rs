use super::*;
use std::process::Command;

pub fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap()
        .success());
}
pub fn repo() -> tempfile::TempDir {
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
pub fn event(journal: &Journal, root: &Path, name: &str, kind: notify::EventKind) {
    journal.event(&notify::Event::new(kind).add_path(root.join(name)));
}

pub const OVERFLOW_TEXT: &str = OVERFLOW;
pub const NOT_SHOWN_TEXT: &str = NOT_SHOWN;

impl Capture {
    pub fn root_for_test(&self) -> &Path {
        &self.root
    }
    pub fn journal_for_test(&self) -> &Arc<Journal> {
        &self.journal
    }

    pub fn deadline_for_test(&self) -> Option<Instant> {
        *self.call_deadline.0.lock().unwrap()
    }

    pub fn report_for_test(&self) -> &Report {
        self.finish()
    }
}
