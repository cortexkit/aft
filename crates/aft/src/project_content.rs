pub use aft_base::project_content::*;

#[cfg(test)]
mod open_probe {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    type Opens = Arc<Mutex<Vec<PathBuf>>>;
    fn probes() -> &'static Mutex<BTreeMap<PathBuf, Opens>> {
        static PROBES: OnceLock<Mutex<BTreeMap<PathBuf, Opens>>> = OnceLock::new();
        PROBES.get_or_init(Mutex::default)
    }

    pub(crate) struct Probe {
        root: PathBuf,
        opens: Opens,
    }

    impl Probe {
        pub(crate) fn new(root: &Path) -> Self {
            let opens = Arc::new(Mutex::new(Vec::new()));
            probes()
                .lock()
                .unwrap()
                .insert(root.to_path_buf(), opens.clone());
            Self {
                root: root.to_path_buf(),
                opens,
            }
        }

        pub(crate) fn paths(&self) -> Vec<PathBuf> {
            self.opens.lock().unwrap().clone()
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            probes().lock().unwrap().remove(&self.root);
        }
    }

    pub(crate) fn record_open(path: &Path) {
        for (root, opens) in probes().lock().unwrap().iter() {
            if path.starts_with(root) {
                opens.lock().unwrap().push(path.to_path_buf());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_root_lookup_uses_supplied_identity_and_containment() {
        struct Policy {
            original: PathBuf,
            identity: PathBuf,
            storage: PathBuf,
            allow: bool,
            compared: std::cell::Cell<bool>,
        }
        impl crate::read_policy::StoragePathPolicy for Policy {
            fn identity(&self, path: &Path) -> PathBuf {
                assert_eq!(path, self.original);
                self.identity.clone()
            }

            fn contains(&self, path: &Path, prefix: &Path) -> bool {
                assert_eq!(path, self.identity);
                if prefix == self.storage {
                    self.compared.set(true);
                    self.allow
                } else {
                    false
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join("storage");
        register_storage_root(&storage);
        let mut policy = Policy {
            original: root.join("alias"),
            identity: root.join("opaque-identity"),
            storage: storage.clone(),
            allow: true,
            compared: std::cell::Cell::new(false),
        };
        assert_eq!(
            storage_root_for_path(&policy.original, &policy),
            Some(storage)
        );
        assert!(policy.compared.get());
        policy.allow = false;
        policy.compared.set(false);
        assert_eq!(storage_root_for_path(&policy.original, &policy), None);
        assert!(policy.compared.get());
    }

    #[test]
    fn sqlite_named_directories_remain_project_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut expected = Vec::new();
        for name in [
            "models.db",
            "models.SQLITE",
            "models.sqlite3",
            "event-wal",
            "event-shm",
            "event-journal",
        ] {
            let directory = root.join(name);
            std::fs::create_dir(&directory).unwrap();
            let source = directory.join("source.rs");
            std::fs::write(&source, "fn user_source() {}\n").unwrap();
            assert!(!is_excluded(&directory), "excluded directory {directory:?}");
            assert!(!is_excluded(&source));
            expected.push(source);
        }
        let storage = root.join("internal.db");
        std::fs::create_dir(&storage).unwrap();
        register_storage_root(&storage);
        std::fs::write(storage.join("internal.rs"), "fn internal() {}\n").unwrap();
        assert!(is_excluded(&storage));
        let mut walked = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        walked.sort();
        expected.sort();
        assert_eq!(walked, expected);
        for name in [
            "fixture.db",
            "fixture.sqlite",
            "fixture.sqlite3",
            "fixture-wal",
            "fixture-shm",
            "fixture-journal",
        ] {
            let file = root.join(name);
            std::fs::write(&file, "not source").unwrap();
            assert!(is_excluded(&file), "admitted file {file:?}");
        }
    }

    #[test]
    fn direct_project_reads_reject_storage_and_sqlite_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        register_storage_root(&storage);
        let probe = Probe::new(&root);
        for path in [
            storage.join("internal.rs"),
            root.join("live.db-shm"),
            root.join("LIVE.SQLITE-WAL"),
        ] {
            std::fs::write(&path, "valid UTF-8 must still not be opened").unwrap();
            assert!(read(&path).is_err(), "admitted {path:?}");
            assert!(open(&path).is_err(), "opened {path:?}");
            assert!(read_to_string(&path).is_err(), "decoded {path:?}");
            assert!(crate::inspect::scanners::source_text::read_source_text(&path).is_ok());
        }
        assert!(
            probe.paths().is_empty(),
            "opened excluded paths: {:?}",
            probe.paths()
        );
        let ordinary = root.join("storage-sibling.rs");
        std::fs::write(&ordinary, "fn user_source() {}\n").unwrap();
        assert!(!read(&ordinary).unwrap().is_empty());
        assert_eq!(probe.paths(), vec![ordinary]);
    }

    // Windows cannot compare file identities here and refuses known SQLite names, not this `.rs` hard-link alias.

    #[cfg(unix)]
    #[test]
    fn nonstandard_live_sqlite_names_are_excluded_by_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let database = root.join("unconventional.cache");
        let connection = crate::db::lifecycle::TrackedConnection::open(
            &database,
            crate::db::lifecycle::SqliteStore::AftDb,
        )
        .unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE probe(value TEXT); INSERT INTO probe VALUES ('alive');").unwrap();
        let probe = Probe::new(&root);
        assert!(is_excluded(&database));
        for (index, original) in [
            database.clone(),
            root.join("unconventional.cache-wal"),
            root.join("unconventional.cache-shm"),
        ]
        .iter()
        .enumerate()
        {
            let alias = root.join(format!("alias_{index}.rs"));
            std::fs::hard_link(original, &alias).unwrap();
            assert!(
                read(&alias).is_err(),
                "opened a live SQLite inode through {alias:?}"
            );
        }
        assert!(
            probe.paths().is_empty(),
            "opened live SQLite files: {:?}",
            probe.paths()
        );
        assert_eq!(
            connection
                .query_row("SELECT value FROM probe", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "alive"
        );
    }

    #[test]
    fn gitdir_lines_parse_absolute_relative_and_malformed() {
        // Absolute, relative, and CRLF-terminated lines resolve; anything else
        // is not a worktree marker and must not prune.
        assert_eq!(
            parse_gitdir_bytes(b"gitdir: /repo/.git/worktrees/wt\n"),
            Some(PathBuf::from("/repo/.git/worktrees/wt"))
        );
        assert_eq!(
            parse_gitdir_bytes(b"  gitdir:  ../.git/worktrees/wt  \r\n"),
            Some(PathBuf::from("../.git/worktrees/wt"))
        );
        for malformed in [
            b"gitdir:".as_slice(),
            b"gitdir:   \n",
            b"not a gitdir\n",
            b"",
            b"gitdir : /repo/.git/worktrees/wt\n",
        ] {
            assert_eq!(parse_gitdir_bytes(malformed), None, "{malformed:?}");
        }
    }

    #[test]
    fn windows_gitdir_lines_name_a_worktrees_admin_dir() {
        // Worktree gitdir lines on Windows use forward slashes or drive paths.
        // Pure path logic, so this runs on every platform without git.
        for line in [
            "gitdir: C:/repo/.git/worktrees/wt\n",
            "gitdir: C:/repo/.git/worktrees/wt",
        ] {
            let target =
                parse_gitdir_bytes(line.as_bytes()).expect("Windows gitdir line must parse");
            assert_eq!(
                target.parent().and_then(|parent| parent.file_name()),
                Some(std::ffi::OsStr::new("worktrees"))
            );
            assert_eq!(
                target.parent().and_then(|parent| parent.parent()),
                Some(std::path::Path::new("C:/repo/.git"))
            );
        }
        // A submodule-style gitdir never names `worktrees/`.
        let target = parse_gitdir_bytes(b"gitdir: ../.git/modules/sub\n").unwrap();
        assert_ne!(
            target.parent().and_then(|parent| parent.file_name()),
            Some(std::ffi::OsStr::new("worktrees"))
        );
    }

    #[test]
    fn commondir_points_at_the_shared_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let common = root.join(".git");
        let admin = common.join("worktrees").join("wt");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(admin.join("commondir"), "../../\n").unwrap();
        assert_eq!(read_commondir_target(&admin), Some(PathBuf::from("../../")));
        assert_eq!(read_commondir_target(&common), None);
    }

    #[test]
    fn hand_rolled_worktree_layout_resolves_without_git() {
        // The same layout `git worktree add` creates, without needing git:
        // main/.git is the common dir, main/linked/.git names its admin entry.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main = root.join("main");
        let linked = main.join("linked");
        let admin = main.join(".git").join("worktrees").join("linked");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", linked.join(".git").display()),
        )
        .unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        let boundary = NestedWorktreeBoundary::for_root(&main);
        assert!(boundary.is_nested_worktree_root(&linked));
        assert!(!boundary.is_nested_worktree_root(&main));
        assert!(!boundary.is_nested_worktree_root(&root));
        assert!(boundary.is_under_nested_worktree(&linked.join("file.rs")));
        assert!(!boundary.is_under_nested_worktree(&main.join("file.rs")));
    }
}
