use super::*;

impl BackupStore {
    #[doc(hidden)]
    pub fn discard_paths_for_tests(&self) -> &Vec<PathBuf> {
        &self.discard_paths_for_tests
    }

    /// Record one more directory as created by the newest entry for `key`, to
    /// test that undo authorizes every directory it would remove.
    #[doc(hidden)]
    pub fn add_created_dir_for_test(&mut self, session: &str, key: &Path, dir: PathBuf) {
        let entry = self
            .entries
            .get_mut(session)
            .and_then(|files| files.get_mut(key))
            .and_then(|stack| stack.last_mut())
            .expect("key has a backup");
        entry.created_dirs.push(dir);
    }

    /// Move a session's backups to a different key, to test how undo treats a
    /// key it did not write itself.
    #[doc(hidden)]
    pub fn rekey_for_test(&mut self, session: &str, from: &Path, to: PathBuf) {
        let files = self.entries.get_mut(session).expect("session has backups");
        let stack = files.remove(from).expect("key has backups");
        files.insert(to, stack);
    }
}

impl BackupStore {
    #[doc(hidden)]
    pub fn history_meta_path_for_test(&self, session: &str, path: &Path) -> Option<PathBuf> {
        Some(
            self.session_dir(session)?
                .join(Self::path_hash(&canonicalize_key(path)))
                .join("meta.json"),
        )
    }

    #[doc(hidden)]
    pub fn history_content_reads_for_test(&self) -> u64 {
        self.history_content_reads.load(Ordering::Relaxed)
    }
}
#[cfg(feature = "test-support")]
impl super::BackupStore {
    #[doc(hidden)]
    pub fn new_for_unit_test() -> Self {
        Self {
            enforce_temp_path_policy: false,
            ..Self::new()
        }
    }
}
